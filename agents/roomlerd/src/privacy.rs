// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-99 P1b — the device's half of privacy mode.
//!
//! A controller whose session grant holds `Permissions::PRIVACY` asks, over
//! the session's own `privacy` DataChannel, to cover this screen: a curtain
//! the person in front of it sees, and every capture path looks through
//! (`WDA_EXCLUDEFROMCAPTURE`, verified after it is set rather than trusted;
//! `indicator/win.rs`). The hub grants the bit only to a device that
//! advertises `privacy-mode` (P1a), and this side decides again, on its own
//! word, at every "on":
//!
//! | # | gate | refusal |
//! |---|---|---|
//! | 1 | the session's grant holds `PRIVACY` (checked when the channel opens) | `not_granted` |
//! | 2 | the owner's `privacy_mode_allowed`, read NOW | `disabled_on_device` |
//! | 3 | a curtain is possible here: Windows 10 2004 or later, in a build whose overlay came up | `unavailable` |
//! | 4 | the overlay put the curtain up, out of capture | `no_curtain` |
//!
//! Gate 2 is the one that survives a compromised server: the owner's switch is
//! device-owned and absent from `DesiredConfig`, so no server push reaches it.
//!
//! Turning it off is never refused. The curtain comes down when the controller
//! says so, when the session ends (the channel closes, or the overlay drops the
//! session), and when the owner switches the gate off, which the viewer is told
//! (`disabled_on_device`). Nothing survives the session: the windows die with
//! the process that drew them.
//!
//! ⚠️ The curtain HIDES the screen; it does not stop a person at the keyboard.
//! It is click-through by necessity (the controller's injected clicks must
//! reach the application beneath it), so the person's own mouse passes through
//! too, and the system UI the shell draws above every topmost window (the Start
//! menu, notifications, Alt+Tab's thumbnails) still shows over it. Blocking
//! local input is P2's `BlockInput`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bson::oid::ObjectId;
use roomler_ai_remote_control::models::RpcCap;
use roomler_ai_remote_control::permissions::Permissions;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

/// The first Windows build whose `SetWindowDisplayAffinity` knows
/// `WDA_EXCLUDEFROMCAPTURE` (10 2004). Earlier builds treat it as
/// `WDA_MONITOR`, which blacks the window out of the capture instead of
/// leaving it out: the controller would see the curtain too.
pub const MIN_WINDOWS_BUILD: u32 = 19041;

/// How often an attached channel looks at its own state. A channel this side
/// closed often never reports `on_close` (webrtc-rs leaves it `Closing`).
const WATCH_EVERY: Duration = Duration::from_millis(500);

// ── The owner's gate ────────────────────────────────────────────────────────

fn allowed_tx() -> &'static watch::Sender<bool> {
    static TX: OnceLock<watch::Sender<bool>> = OnceLock::new();
    TX.get_or_init(|| watch::channel(false).0)
}

/// Seed (at start-up) or re-seed (after a LOCAL `ConfigSet`) the owner's
/// `privacy_mode_allowed`. The server never reaches this: the key is
/// structurally absent from `DesiredConfig`. Going OFF takes every curtain
/// that is up down (see [`attach`]).
pub fn set_device_allowed(allowed: bool) {
    allowed_tx().send_if_modified(|v| {
        let changed = *v != allowed;
        *v = allowed;
        changed
    });
}

/// The owner's gate, as it is now.
pub fn device_allowed() -> bool {
    *allowed_tx().borrow()
}

// ── Whether a curtain is possible here ──────────────────────────────────────

static OVERLAY_UP: AtomicBool = AtomicBool::new(false);

/// Called once the native overlay that draws the curtain is running in this
/// process (`ViewerIndicator::new` succeeded). Without it there is nothing to
/// draw with — a service in session 0, a headless host — and the device must
/// not advertise what it cannot do.
pub fn set_overlay_up(up: bool) {
    OVERLAY_UP.store(up, Ordering::Release);
}

/// Can this process put a curtain up at all? Windows 10 2004 or later, a build
/// with the overlay (`viewer-indicator`), and the overlay running.
pub fn curtain_possible() -> bool {
    #[cfg(all(target_os = "windows", feature = "viewer-indicator"))]
    {
        OVERLAY_UP.load(Ordering::Acquire)
            && crate::encode::hwid::windows_build().is_some_and(|b| b >= MIN_WINDOWS_BUILD)
    }
    #[cfg(not(all(target_os = "windows", feature = "viewer-indicator")))]
    {
        false
    }
}

/// The `rpc` words for privacy mode: `privacy-available` where a curtain is
/// possible, and `privacy-mode` only while the owner allows it as well. The
/// hub grants `PRIVACY` on `privacy-mode` alone; `privacy-available` lets it
/// tell the viewer "the owner has not allowed it" instead of nothing.
pub fn words(possible: bool, allowed: bool) -> Vec<RpcCap> {
    match (possible, allowed) {
        (false, _) => Vec::new(),
        (true, false) => vec![RpcCap::PrivacyAvailable],
        (true, true) => vec![RpcCap::PrivacyAvailable, RpcCap::PrivacyMode],
    }
}

/// [`words`] for this process, now.
pub fn current_words() -> Vec<RpcCap> {
    words(curtain_possible(), device_allowed())
}

/// The consent prompt's line for a grant that may cover the screen. `None`
/// unless the grant holds `PRIVACY` AND this device could act on it: a line
/// the device would refuse anyway (gate 2 or 3) tells the person something
/// that cannot happen.
///
/// One short line: the native panel draws `detail` on a single line and cuts
/// what does not fit. How to end the session is on the curtain itself.
pub fn consent_line(
    permissions: Permissions,
    allowed: bool,
    possible: bool,
) -> Option<&'static str> {
    (permissions.contains(Permissions::PRIVACY) && allowed && possible)
        .then_some("They may also cover this screen while they work.")
}

/// `roomlerd curtain-smoke` — put a curtain up through the PRODUCTION overlay
/// for `seconds`, print whether it went up out of capture, and take it down.
/// A field check that needs no server and no viewer: a screenshot of the host
/// shows the curtain while a session's stream shows the desktop under it.
/// `Ok(false)` when no curtain went up (not possible here, or refused).
pub fn curtain_smoke(seconds: u64) -> anyhow::Result<bool> {
    let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel(1);
    let (consent_tx, _consent_rx) = tokio::sync::mpsc::channel(1);
    let indicator = crate::indicator::ViewerIndicator::new(
        kill_tx,
        crate::rc_sessions::RcSessionRegistry::new(),
        consent_tx,
    )?;
    set_overlay_up(true);
    let session = ObjectId::new();
    indicator.show_session_full(
        session,
        "curtain-smoke".into(),
        "VIEW | PRIVACY".into(),
        String::new(),
    );
    let possible = curtain_possible();
    let up = possible && indicator.set_privacy(session, true, "curtain-smoke");
    println!("curtain-smoke: possible={possible} up={up}");
    if up {
        std::thread::sleep(Duration::from_secs(seconds));
        indicator.set_privacy(session, false, "curtain-smoke");
        println!("curtain-smoke: down after {seconds}s");
    }
    indicator.hide_session(session.to_hex());
    Ok(up)
}

// ── The wire, on the `privacy` DataChannel ─────────────────────────────────

/// Controller → device.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "t")]
pub enum Incoming {
    /// Cover the screen (`on: true`) or uncover it. `id` is the controller's
    /// handle for this request, echoed on the answer.
    #[serde(rename = "rc:privacy.set")]
    Set {
        #[serde(default)]
        id: String,
        on: bool,
    },
    /// Say where things stand (a viewer that reconnected its channel).
    #[serde(rename = "rc:privacy.status")]
    Status {
        #[serde(default)]
        id: String,
    },
}

/// Device → controller: one `rc:privacy.state`.
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct StateMsg {
    pub t: &'static str,
    pub id: String,
    /// Is the curtain up for this session?
    pub on: bool,
    /// Why a request was refused, or why the curtain came down when the
    /// controller did not ask: `not_granted`, `disabled_on_device`,
    /// `unavailable`, `no_curtain`. Absent when it did what was asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

impl StateMsg {
    fn new(id: &str, on: bool, reason: Option<&'static str>) -> Self {
        Self {
            t: "rc:privacy.state",
            id: id.to_string(),
            on,
            reason,
        }
    }
}

// ── The curtain's host, and one session's privacy ───────────────────────────

/// Whatever draws the curtain: the native overlay in production
/// (`ViewerIndicator`), a recorder in tests.
pub trait CurtainHost: Send + Sync + 'static {
    /// Put `session`'s curtain up (`on`) or take it down. `true` only when it
    /// is UP and out of capture; a take-down answers `false`. May block for
    /// as long as the overlay takes to answer, so callers off a runtime
    /// thread only.
    fn set_curtain(&self, session: ObjectId, on: bool, controller: &str) -> bool;
}

impl CurtainHost for crate::indicator::ViewerIndicator {
    fn set_curtain(&self, session: ObjectId, on: bool, controller: &str) -> bool {
        self.set_privacy(session, on, controller)
    }
}

/// What the `privacy` channel of one session needs. Set by the signalling loop
/// before the answer, so a channel can never open without it.
#[derive(Clone)]
pub struct PrivacyCtx {
    pub session_id: ObjectId,
    pub controller_name: String,
    pub host: Arc<dyn CurtainHost>,
    /// Gate 3, asked at every "on": [`curtain_possible`] in production. A
    /// field so the channel's own test can run the whole path on a runner
    /// where no curtain is possible.
    pub possible: fn() -> bool,
}

/// One session's privacy mode: its gates and whether its curtain is up. The
/// decisions are here and the channel glue is in [`attach`], so the gates are
/// tested without a peer.
pub struct Session {
    ctx: PrivacyCtx,
    /// Whether the curtain is up. Read without the lock, so a `status` on the
    /// channel's message loop never waits behind an overlay that is slow to
    /// answer.
    on: AtomicBool,
    /// Serialises the transitions (each may block on the overlay).
    transition: Mutex<()>,
}

impl Session {
    pub fn new(ctx: PrivacyCtx) -> Self {
        Self {
            ctx,
            on: AtomicBool::new(false),
            transition: Mutex::new(()),
        }
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    fn take_down(&self, why: &str) {
        self.ctx
            .host
            .set_curtain(self.ctx.session_id, false, &self.ctx.controller_name);
        self.on.store(false, Ordering::Release);
        info!(session = %self.ctx.session_id, why, "privacy: curtain down");
    }

    /// A controller's `rc:privacy.set`, against the gates as they are now.
    pub fn set(&self, id: &str, want: bool, allowed: bool, possible: bool) -> StateMsg {
        let _t = self.transition.lock().unwrap_or_else(|p| p.into_inner());
        if !want {
            if self.is_on() {
                self.take_down("controller");
            }
            return StateMsg::new(id, false, None);
        }
        if !allowed {
            return StateMsg::new(id, self.is_on(), Some("disabled_on_device"));
        }
        if !possible {
            return StateMsg::new(id, self.is_on(), Some("unavailable"));
        }
        if self.is_on() {
            return StateMsg::new(id, true, None);
        }
        if !self
            .ctx
            .host
            .set_curtain(self.ctx.session_id, true, &self.ctx.controller_name)
        {
            warn!(session = %self.ctx.session_id, "privacy: the overlay could not put a curtain up");
            return StateMsg::new(id, false, Some("no_curtain"));
        }
        self.on.store(true, Ordering::Release);
        info!(session = %self.ctx.session_id, "privacy: curtain up");
        StateMsg::new(id, true, None)
    }

    /// The owner switched the gate off: a curtain that is up comes down, and
    /// the viewer is told why. `None` when there was nothing up.
    pub fn owner_switched_off(&self) -> Option<StateMsg> {
        let _t = self.transition.lock().unwrap_or_else(|p| p.into_inner());
        if !self.is_on() {
            return None;
        }
        self.take_down("the owner switched it off");
        Some(StateMsg::new("", false, Some("disabled_on_device")))
    }

    /// The session ended: whatever is up comes down. Idempotent.
    pub fn end(&self) {
        let _t = self.transition.lock().unwrap_or_else(|p| p.into_inner());
        if self.is_on() {
            self.take_down("session ended");
        }
    }
}

// ── The channel ─────────────────────────────────────────────────────────────

fn channel_gone(dc: &RTCDataChannel) -> bool {
    matches!(
        dc.ready_state(),
        RTCDataChannelState::Closing | RTCDataChannelState::Closed
    )
}

async fn send(dc: &RTCDataChannel, s: &StateMsg) {
    if let Ok(text) = serde_json::to_string(s)
        && let Err(e) = dc.send_text(text).await
    {
        tracing::debug!(%e, "privacy DC: send failed (channel closing)");
    }
}

/// Run a blocking [`Session`] call off the runtime: the overlay may take a
/// moment to answer, and the channel's message loop must not wait on it.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

/// Serve `rc:privacy.*` on `dc` for the session in `ctx`. Installed only when
/// the session's grant holds `Permissions::PRIVACY`.
pub fn attach(dc: Arc<RTCDataChannel>, ctx: PrivacyCtx) {
    info!(session = %ctx.session_id, "privacy DC attached");
    let possible = ctx.possible;
    let session = Arc::new(Session::new(ctx));
    let gone = Arc::new(AtomicBool::new(false));

    // The session's end, two ways: `on_close` when the far end closed the
    // channel, and a look at the channel's state for when this side did. The
    // overlay also drops a session's curtain when the session leaves it.
    let end = {
        let session = session.clone();
        let gone = gone.clone();
        move || {
            if !gone.swap(true, Ordering::AcqRel) {
                let session = session.clone();
                tokio::task::spawn_blocking(move || session.end());
            }
        }
    };
    let on_close = end.clone();
    dc.on_close(Box::new(move || {
        on_close();
        Box::pin(async {})
    }));

    // Watch the channel, and the owner's gate: OFF takes the curtain down.
    {
        let dc = dc.clone();
        let session = session.clone();
        let gone = gone.clone();
        let end = end.clone();
        let mut allowed = allowed_tx().subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = allowed.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        let now = *allowed.borrow_and_update();
                        if !now {
                            let s = session.clone();
                            if let Some(Some(msg)) = blocking(move || s.owner_switched_off()).await {
                                send(&dc, &msg).await;
                            }
                        }
                    }
                    _ = tokio::time::sleep(WATCH_EVERY) => {}
                }
                if gone.load(Ordering::Acquire) {
                    return;
                }
                if channel_gone(&dc) {
                    end();
                    return;
                }
            }
        });
    }

    let dc_for_handler = dc.clone();
    dc.on_message(Box::new(move |msg| {
        let dc = dc_for_handler.clone();
        let session = session.clone();
        Box::pin(async move {
            if !msg.is_string {
                return;
            }
            let Ok(text) = std::str::from_utf8(&msg.data) else {
                return;
            };
            match serde_json::from_str::<Incoming>(text) {
                Ok(Incoming::Set { id, on }) => {
                    // Off the message loop: the overlay may take a moment.
                    tokio::spawn(async move {
                        let (allowed, possible) = (device_allowed(), possible());
                        let s = session.clone();
                        if let Some(msg) = blocking(move || s.set(&id, on, allowed, possible)).await
                        {
                            send(&dc, &msg).await;
                        }
                    });
                }
                Ok(Incoming::Status { id }) => {
                    send(&dc, &StateMsg::new(&id, session.is_on(), None)).await;
                }
                Err(e) => warn!(%e, "privacy DC: unparseable message"),
            }
        })
    }));
}

/// A channel that answers every request with `reason` instead of silence, so
/// a viewer that opened it anyway is told why: `not_granted` on a session
/// WITHOUT `Permissions::PRIVACY`, `unavailable` where the session was given
/// no privacy context.
pub fn attach_refusing(dc: Arc<RTCDataChannel>, session_id: ObjectId, reason: &'static str) {
    info!(session = %session_id, reason, "privacy DC attached in REFUSE mode");
    let dc_for_handler = dc.clone();
    dc.on_message(Box::new(move |msg| {
        let dc = dc_for_handler.clone();
        Box::pin(async move {
            if !msg.is_string {
                return;
            }
            let id = std::str::from_utf8(&msg.data)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
                .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string))
                .unwrap_or_default();
            send(&dc, &StateMsg::new(&id, false, Some(reason))).await;
        })
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every call, and answers an "up" with `up_ok`.
    struct Recorder {
        calls: Mutex<Vec<bool>>,
        up_ok: bool,
    }

    impl CurtainHost for Recorder {
        fn set_curtain(&self, _session: ObjectId, on: bool, _controller: &str) -> bool {
            self.calls.lock().unwrap().push(on);
            on && self.up_ok
        }
    }

    fn session(up_ok: bool) -> (Session, Arc<Recorder>) {
        let host = Arc::new(Recorder {
            calls: Mutex::new(Vec::new()),
            up_ok,
        });
        let s = Session::new(PrivacyCtx {
            session_id: ObjectId::new(),
            controller_name: "Alex".into(),
            host: host.clone(),
            possible: || true,
        });
        (s, host)
    }

    /// Each gate refuses in its own words, and none of them touches the
    /// overlay: a refused "on" must not flash a curtain.
    #[test]
    fn every_gate_refuses_before_the_overlay_is_asked() {
        let (s, host) = session(true);
        assert_eq!(
            s.set("a", true, false, true).reason,
            Some("disabled_on_device")
        );
        assert_eq!(s.set("b", true, true, false).reason, Some("unavailable"));
        // The owner's gate is named first: it is the one the owner can change.
        assert_eq!(
            s.set("c", true, false, false).reason,
            Some("disabled_on_device")
        );
        assert!(host.calls.lock().unwrap().is_empty());
        assert!(!s.is_on());
    }

    #[test]
    fn on_then_off_puts_the_curtain_up_and_takes_it_down() {
        let (s, host) = session(true);
        let up = s.set("1", true, true, true);
        assert_eq!(up, StateMsg::new("1", true, None));
        assert!(s.is_on());
        // A second "on" is answered, not drawn twice.
        assert!(s.set("2", true, true, true).on);
        let down = s.set("3", false, true, true);
        assert_eq!(down, StateMsg::new("3", false, None));
        assert!(!s.is_on());
        assert_eq!(*host.calls.lock().unwrap(), vec![true, false]);
    }

    /// The overlay could not put it up (or could not keep it out of the
    /// capture): refused, and nothing claims a curtain that is not there.
    #[test]
    fn an_overlay_that_fails_is_a_refusal_not_a_curtain() {
        let (s, _) = session(false);
        let r = s.set("x", true, true, true);
        assert_eq!(r, StateMsg::new("x", false, Some("no_curtain")));
        assert!(!s.is_on());
    }

    /// Off is never refused — not even when the gates have closed since.
    #[test]
    fn off_is_never_refused() {
        let (s, host) = session(true);
        assert!(s.set("1", true, true, true).on);
        let r = s.set("2", false, false, false);
        assert_eq!(r, StateMsg::new("2", false, None));
        assert_eq!(*host.calls.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn the_owner_switching_off_takes_it_down_and_says_why() {
        let (s, host) = session(true);
        assert_eq!(s.owner_switched_off(), None, "nothing up, nothing to say");
        assert!(s.set("1", true, true, true).on);
        let msg = s.owner_switched_off().expect("a curtain was up");
        assert_eq!(msg.reason, Some("disabled_on_device"));
        assert!(!msg.on && !s.is_on());
        assert_eq!(*host.calls.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn the_session_ending_takes_it_down_once() {
        let (s, host) = session(true);
        assert!(s.set("1", true, true, true).on);
        s.end();
        s.end();
        assert_eq!(*host.calls.lock().unwrap(), vec![true, false]);
    }

    /// `privacy-mode` only with `privacy-available`, and only while the owner
    /// allows it; nothing at all where no curtain is possible.
    #[test]
    fn the_words_follow_the_platform_and_the_owner() {
        assert!(words(false, true).is_empty());
        assert!(words(false, false).is_empty());
        assert_eq!(words(true, false), vec![RpcCap::PrivacyAvailable]);
        assert_eq!(
            words(true, true),
            vec![RpcCap::PrivacyAvailable, RpcCap::PrivacyMode]
        );
    }

    /// The prompt says the screen may be covered only when it could be.
    #[test]
    fn the_consent_line_appears_only_when_the_device_could_act_on_it() {
        let p = Permissions::VIEW | Permissions::INPUT | Permissions::PRIVACY;
        assert!(consent_line(p, true, true).is_some());
        assert!(consent_line(p, false, true).is_none());
        assert!(consent_line(p, true, false).is_none());
        assert!(consent_line(Permissions::VIEW | Permissions::INPUT, true, true).is_none());
    }

    #[test]
    fn the_wire_reads_and_writes_as_documented() {
        assert_eq!(
            serde_json::from_str::<Incoming>(r#"{"t":"rc:privacy.set","id":"a","on":true}"#)
                .unwrap(),
            Incoming::Set {
                id: "a".into(),
                on: true
            }
        );
        assert_eq!(
            serde_json::from_str::<Incoming>(r#"{"t":"rc:privacy.status"}"#).unwrap(),
            Incoming::Status { id: String::new() }
        );
        // `on` is required: a set that does not say which way is an error,
        // never a default that might cover the screen.
        assert!(serde_json::from_str::<Incoming>(r#"{"t":"rc:privacy.set","id":"a"}"#).is_err());
        let v = serde_json::to_value(StateMsg::new("a", false, Some("no_curtain"))).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"t":"rc:privacy.state","id":"a","on":false,"reason":"no_curtain"})
        );
        let v = serde_json::to_value(StateMsg::new("b", true, None)).unwrap();
        assert!(v.get("reason").is_none(), "{v}");
    }

    #[test]
    fn the_owner_gate_is_live() {
        set_device_allowed(true);
        assert!(device_allowed());
        set_device_allowed(false);
        assert!(!device_allowed());
    }

    /// Off Windows, or without the overlay, a curtain is never possible — and
    /// so no word is ever advertised there.
    #[cfg(not(all(target_os = "windows", feature = "viewer-indicator")))]
    #[test]
    fn no_curtain_is_possible_without_the_windows_overlay() {
        set_overlay_up(true);
        assert!(!curtain_possible());
        // Whatever the owner's gate says: there is nothing to offer.
        assert!(current_words().is_empty());
    }
}

// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-85 P3b — the device's half of REMOTE recording.
//!
//! A controller already in a session asks, over the session's own `record`
//! DataChannel, to record this screen. The recording is made HERE, by the same
//! recorder a local one uses, and stays here; the server only ever sees the
//! grant (P3a) and what this side reports about it. The channel exists only on
//! a session whose grant holds `Permissions::RECORD`, which the hub strips
//! unless the controller may record AND this device advertises `record`.
//!
//! A start passes, in order (each refusal is named on the channel and
//! reported to the server):
//!
//! | # | gate | refusal |
//! |---|---|---|
//! | 1 | the owner's `record_remote_enabled`, read NOW | `disabled_on_device` |
//! | 2 | a recorder can run in this process | `unavailable` |
//! | 3 | computer audio asked for: `record_remote_audio` and an audio build | `audio_not_allowed` |
//! | 4 | one recording at a time | `busy` |
//! | 5 | one start at a time per session | `already_starting` |
//! | 6 | a session consented ON THE HOST asks the host again (a fresh prompt id, never the session's); a deny is final for [`DENY_COOLDOWN`] | `consent_denied`, `consent_timeout`, `no_prompt_surface`, `rate_limited` |
//! | 7 | something ON SCREEN says "recording" before the first frame: the daemon's badge (Windows) or the companion's banner | `no_indicator_surface` |
//!
//! ⚠️ Gate 7 has no unattended exception yet. A host with nobody at it could
//! record without a banner, but a recorder there runs as SYSTEM/root, which
//! the manager refuses until P1e; until then every remote recording is behind
//! a banner.
//!
//! The microphone is not a remote option: [`RecordingManager::start_remote`]
//! has no parameter for it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use roomler_ai_remote_control::models::{RecordCap, RecordingActivityKind};
use roomler_ai_remote_control::signaling::ClientMsg;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

use super::manager::{RecordingManager, RemoteInitiator, StartError};

/// After a host says no, the same session may not ask again for this long:
/// a controller must not be able to wear the person down with prompts.
pub const DENY_COOLDOWN: Duration = Duration::from_secs(60);

/// How often a recording in progress reports its size and length.
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

// ── The device owner's gates ────────────────────────────────────────────────

/// The owner's two remote-recording gates, as they are NOW.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gates {
    /// `record_remote_enabled`.
    pub enabled: bool,
    /// `record_remote_audio`.
    pub audio: bool,
}

fn gates_tx() -> &'static watch::Sender<Gates> {
    static TX: OnceLock<watch::Sender<Gates>> = OnceLock::new();
    TX.get_or_init(|| watch::channel(Gates::default()).0)
}

/// Seed (at start-up) or re-seed (after a LOCAL `ConfigSet`) the live gates.
/// The server can never reach this: both keys are structurally absent from
/// `DesiredConfig`. A gate going OFF stops a remote recording in progress
/// (see [`Handler::follow`]).
pub fn adopt(cfg: &roomler_node_core::config::AgentConfig) {
    let next = Gates {
        enabled: cfg.record_remote_enabled,
        audio: cfg.record_remote_audio,
    };
    gates_tx().send_if_modified(|g| {
        let changed = *g != next;
        *g = next;
        changed
    });
}

/// The gates as they are now.
pub fn gates() -> Gates {
    *gates_tx().borrow()
}

// ── The recorder this process runs ─────────────────────────────────────────

static MANAGER: OnceLock<Arc<RecordingManager>> = OnceLock::new();

/// Hand the daemon's recorder supervisor to the remote path — the SAME one
/// the LocalAPI verbs use, so a local and a remote recording can never run at
/// once. Called once, where the daemon builds it.
pub fn install(manager: Arc<RecordingManager>) {
    let _ = MANAGER.set(manager);
}

fn manager() -> Option<&'static Arc<RecordingManager>> {
    MANAGER.get()
}

/// What this device advertises in `AgentCaps.record` right now.
pub fn advertised() -> Vec<String> {
    advertise(
        gates(),
        manager().is_some_and(|m| m.available()),
        cfg!(feature = "audio"),
    )
}

/// Pure half of [`advertised`]: `remote` only while the owner's gate is on
/// AND a recorder can run here (a SYSTEM/root service cannot, until P1e);
/// `remote-audio` only on top of that, with the audio gate on and an audio
/// build. Nothing at all otherwise — silence is how the hub learns "no".
pub fn advertise(g: Gates, can_record: bool, audio_built: bool) -> Vec<String> {
    let mut out = Vec::new();
    if g.enabled && can_record {
        out.push(RecordCap::Remote.wire().to_string());
        if g.audio && audio_built {
            out.push(RecordCap::RemoteAudio.wire().to_string());
        }
    }
    out
}

// ── The wire, on the `record` DataChannel ──────────────────────────────────

/// Controller → device.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "t")]
pub enum Incoming {
    /// Start recording. `id` is the controller's handle for this attempt
    /// (echoed on every state); `audio` asks for what the computer plays.
    #[serde(rename = "rc:record.start")]
    Start {
        #[serde(default)]
        id: String,
        #[serde(default)]
        audio: bool,
    },
    /// Stop the recording this session started.
    #[serde(rename = "rc:record.stop")]
    Stop {
        #[serde(default)]
        id: String,
    },
    /// Say where things stand (a viewer that reconnected its channel).
    #[serde(rename = "rc:record.status")]
    Status,
}

/// Device → controller: one `rc:record.state`.
#[derive(Debug, Serialize, Default, Clone, PartialEq, Eq)]
pub struct StateMsg {
    pub t: &'static str,
    pub id: String,
    /// `pending_consent` | `recording` | `stopped` | `refused` | `failed` | `idle`.
    pub state: &'static str,
    /// The refusal code or stop reason (closed sets, `docs/recording.md` §10).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// A sentence for a human, when there is more to say than the code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The recording's file name (never a path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub bytes: u64,
    pub duration_ms: u64,
    pub audio: bool,
}

impl StateMsg {
    fn new(id: &str, state: &'static str) -> Self {
        Self {
            t: "rc:record.state",
            id: id.to_string(),
            state,
            ..Default::default()
        }
    }
}

/// The refusals decided before anything is asked or launched, in order.
pub fn precheck(
    g: Gates,
    can_record: bool,
    audio: bool,
    audio_built: bool,
    busy: bool,
) -> Result<(), &'static str> {
    if !g.enabled {
        return Err("disabled_on_device");
    }
    if !can_record {
        return Err("unavailable");
    }
    if audio && !(g.audio && audio_built) {
        return Err("audio_not_allowed");
    }
    if busy {
        return Err("busy");
    }
    Ok(())
}

/// A file name for the controller: never the path it lives at.
fn file_name(path: Option<&str>) -> Option<String> {
    path.and_then(|p| {
        std::path::Path::new(p)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    })
}

// ── One session's handler ──────────────────────────────────────────────────

/// Whether the desktop companion can show this session's prompt and banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Companion {
    /// Ask the system: start the companion if it is installed and not
    /// running (`companion::ensure_running`, bounded).
    System,
    /// A fixed answer — for tests, which must never launch the real app.
    Fixed(bool),
}

impl Companion {
    async fn up(self) -> bool {
        match self {
            Self::System => tokio::time::timeout(
                crate::signaling::COMPANION_START_BUDGET,
                crate::companion::ensure_running(),
            )
            .await
            .unwrap_or(false),
            Self::Fixed(up) => up,
        }
    }
}

/// What a session's `record` channel needs from the signalling loop that
/// built the peer. Set once per session, before the offer is answered.
#[derive(Clone)]
pub struct SessionCtx {
    pub session_id: ObjectId,
    pub controller_user_id: ObjectId,
    pub controller_name: String,
    /// The asking organisation on a multi-org device; empty otherwise.
    pub org: String,
    /// `Some(window)` when this session was consented ON THE HOST, so a
    /// recording asks the host again with the same window; `None` when it
    /// was auto-granted (the owner opted into remote recording at gate 1).
    pub prompt_window: Option<Duration>,
    pub consent: crate::consent::ConsentBroker,
    pub indicator: crate::indicator::ViewerIndicator,
    /// This session's outbound queue — where activity reports go.
    pub outbound: mpsc::Sender<ClientMsg>,
    /// [`Companion::System`] in production.
    pub companion: Companion,
}

struct Handler {
    ctx: SessionCtx,
    dc: Arc<RTCDataChannel>,
    /// A start is between its first check and its answer.
    starting: AtomicBool,
    /// When the host last said no (for [`DENY_COOLDOWN`]).
    last_deny: StdMutex<Option<Instant>>,
    /// The prompt standing on the host's screen for this session, if any.
    prompt: StdMutex<Option<String>>,
    /// The controller's id for the recording in progress.
    current: StdMutex<Option<String>>,
}

/// Serve `rc:record.*` on `dc` for the session in `ctx`. Installed only when
/// the session's grant holds `Permissions::RECORD`.
pub fn attach(dc: Arc<RTCDataChannel>, ctx: SessionCtx) {
    info!(session = %ctx.session_id, "record DC attached");
    let h = Arc::new(Handler {
        ctx,
        dc: dc.clone(),
        starting: AtomicBool::new(false),
        last_deny: StdMutex::new(None),
        prompt: StdMutex::new(None),
        current: StdMutex::new(None),
    });
    let on_close = h.clone();
    dc.on_close(Box::new(move || {
        let h = on_close.clone();
        Box::pin(async move {
            h.session_gone().await;
        })
    }));
    dc.on_message(Box::new(move |msg| {
        let h = h.clone();
        Box::pin(async move {
            if !msg.is_string {
                return;
            }
            let Ok(text) = std::str::from_utf8(&msg.data) else {
                return;
            };
            match serde_json::from_str::<Incoming>(text) {
                // A prompt can stand for minutes: never hold the channel's
                // message loop for it, or a Stop could not get through.
                Ok(Incoming::Start { id, audio }) => {
                    tokio::spawn(h.start(id, audio));
                }
                Ok(Incoming::Stop { id }) => h.stop(id).await,
                Ok(Incoming::Status) => h.status().await,
                Err(e) => warn!(session = %h.ctx.session_id, %e, "record DC: unparseable message"),
            }
        })
    }));
}

/// A channel that answers every request with `reason` instead of silence, so
/// a viewer that opened it anyway is told why rather than left waiting:
/// `not_granted` on a session WITHOUT `Permissions::RECORD`, `unavailable`
/// where the session was not given a recording context.
pub fn attach_refusing(dc: Arc<RTCDataChannel>, session_id: ObjectId, reason: &'static str) {
    info!(session = %session_id, reason, "record DC attached in REFUSE mode");
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
            let mut s = StateMsg::new(&id, "refused");
            s.reason = Some(reason.into());
            send(&dc, &s).await;
        })
    }));
}

async fn send(dc: &RTCDataChannel, s: &StateMsg) {
    if let Ok(text) = serde_json::to_string(s)
        && let Err(e) = dc.send_text(text).await
    {
        tracing::debug!(%e, "record DC: send failed (channel closing)");
    }
}

impl Handler {
    fn report(
        &self,
        kind: RecordingActivityKind,
        name: Option<String>,
        bytes: Option<u64>,
        duration_ms: Option<u64>,
        reason: Option<String>,
    ) {
        let msg = ClientMsg::RecordingActivity {
            session_id: self.ctx.session_id,
            kind,
            name,
            bytes,
            duration_ms,
            reason,
        };
        // Never blocks a session: a full queue means the connection is
        // already struggling, and losing a log line beats stalling.
        if self.ctx.outbound.try_send(msg).is_err() {
            tracing::debug!(session = %self.ctx.session_id, "recording activity dropped (queue full or closed)");
        }
    }

    async fn refuse(&self, id: &str, reason: &str, detail: Option<String>) {
        info!(session = %self.ctx.session_id, %reason, ?detail, "remote recording refused");
        let mut s = StateMsg::new(id, "refused");
        s.reason = Some(reason.to_string());
        s.detail = detail;
        send(&self.dc, &s).await;
        self.report(
            RecordingActivityKind::Refused,
            None,
            None,
            None,
            Some(reason.to_string()),
        );
    }

    /// Is the recording in progress this session's?
    fn owns_active(&self) -> bool {
        manager()
            .and_then(|m| m.active_remote())
            .is_some_and(|r| r.session_id == self.ctx.session_id)
    }

    async fn start(self: Arc<Self>, id: String, audio: bool) {
        let Some(m) = manager() else {
            self.refuse(&id, "unavailable", None).await;
            return;
        };
        if let Err(code) = precheck(
            gates(),
            m.available(),
            audio,
            cfg!(feature = "audio"),
            m.state().active,
        ) {
            self.refuse(&id, code, None).await;
            return;
        }
        if self.starting.swap(true, Ordering::AcqRel) {
            self.refuse(&id, "already_starting", None).await;
            return;
        }
        let outcome = self.clone().start_inner(&id, audio).await;
        self.starting.store(false, Ordering::Release);
        if let Some((code, detail)) = outcome {
            self.refuse(&id, code, detail).await;
        }
    }

    /// The prompt, the indicator and the launch. `Some((code, detail))` is a
    /// refusal for the caller to send; `None` = recording (or the session
    /// went away mid-prompt, which owes nobody an answer).
    async fn start_inner(
        self: Arc<Self>,
        id: &str,
        audio: bool,
    ) -> Option<(&'static str, Option<String>)> {
        let m = manager()?;
        let sid = self.ctx.session_id;

        if let Some(window) = self.ctx.prompt_window {
            if self
                .last_deny
                .lock()
                .ok()
                .and_then(|d| *d)
                .is_some_and(|at| at.elapsed() < DENY_COOLDOWN)
            {
                return Some(("rate_limited", None));
            }
            send(&self.dc, &StateMsg::new(id, "pending_consent")).await;
            let (decision, have_surface) = self.ask_host(window, audio).await;
            use crate::consent::Decision;
            match decision {
                Decision::Granted => {
                    self.report(RecordingActivityKind::PromptGranted, None, None, None, None);
                }
                Decision::Denied => {
                    if let Ok(mut d) = self.last_deny.lock() {
                        *d = Some(Instant::now());
                    }
                    self.report(RecordingActivityKind::PromptDenied, None, None, None, None);
                    return Some(("consent_denied", None));
                }
                Decision::Timeout => {
                    self.report(
                        RecordingActivityKind::PromptTimedOut,
                        None,
                        None,
                        None,
                        Some(
                            if have_surface {
                                "consent_timeout"
                            } else {
                                "no_prompt_surface"
                            }
                            .into(),
                        ),
                    );
                    return Some(if have_surface {
                        ("consent_timeout", None)
                    } else {
                        ("no_prompt_surface", None)
                    });
                }
                // The session ended while the host was being asked.
                Decision::Cancelled => return None,
            }
            // The owner may have switched it off while the prompt stood.
            if !gates().enabled {
                return Some(("disabled_on_device", None));
            }
        }

        // ⚠️ Something on screen says "recording" BEFORE the first frame.
        let shown = self.ctx.indicator.set_recording(sid, true);
        let companion = shown.listed && self.ctx.companion.up().await;
        if !(shown.native || companion) {
            self.ctx.indicator.set_recording(sid, false);
            return Some((
                "no_indicator_surface",
                Some(
                    "nothing on this device can show that it is being recorded (no banner, and \
                     the Roomler desktop app is not running)"
                        .into(),
                ),
            ));
        }

        let initiator = RemoteInitiator {
            session_id: sid,
            controller_user_id: self.ctx.controller_user_id,
            controller_name: self.ctx.controller_name.clone(),
        };
        match m.start_remote(initiator, audio).await {
            Ok(state) => {
                let name = file_name(state.path.as_deref());
                info!(session = %sid, ?name, audio, "remote recording started");
                if let Ok(mut c) = self.current.lock() {
                    *c = Some(id.to_string());
                }
                let mut s = StateMsg::new(id, "recording");
                s.name = name.clone();
                s.audio = state.system_audio;
                send(&self.dc, &s).await;
                self.report(RecordingActivityKind::Started, name, None, None, None);
                tokio::spawn(self.clone().follow(id.to_string()));
                None
            }
            Err(e) => {
                self.ctx.indicator.set_recording(sid, false);
                Some(match e {
                    StartError::Busy => ("busy", None),
                    StartError::Unavailable(d) => ("unavailable", Some(d)),
                    StartError::Failed(d) => ("start_failed", Some(d)),
                })
            }
        }
    }

    /// The just-in-time question, through the same surfaces as every other
    /// prompt: the daemon's own panel first, the companion second, the CLI
    /// always. Returns the decision and whether anyone could have been asked.
    async fn ask_host(&self, window: Duration, audio: bool) -> (crate::consent::Decision, bool) {
        // ⚠️ A FRESH id, never the session's, nor derived from it: the session
        // already has an answered prompt, and a decision recorded against its
        // id must not be able to answer this one (the ssh / ssh-consent lesson).
        let prompt_id = ObjectId::new().to_hex();
        let what = if audio {
            "record this screen and what the computer plays"
        } else {
            "record this screen"
        };
        let name = &self.ctx.controller_name;
        let native = self
            .ctx
            .indicator
            .show_prompt(crate::indicator::PromptView {
                session_hex: prompt_id.clone(),
                title: "Screen recording request".into(),
                lead: format!("{name} wants to {what}."),
                detail: "The recording is saved on this device, and a banner shows while it runs."
                    .into(),
                permissions: String::new(),
                org: self.ctx.org.clone(),
                expires_at: Instant::now() + window,
            });
        let prompt = crate::consent::PendingPrompt {
            kind: crate::consent::PromptKind::Record,
            asked_by: name,
            permissions: "RECORD".into(),
            // The whole question, for a companion that predates `record`.
            detail: format!("Wants to {what}. The recording stays on this device."),
            org: self.ctx.org.clone(),
            timeout: window,
            surface: if native {
                crate::consent::PromptSurface::Native
            } else {
                crate::consent::PromptSurface::Companion
            },
        };
        // The marker is written either way (`roomlerd consent --list` shows a
        // natively-prompted question too); without it the companion has
        // nothing to render, so it is part of "could anyone be asked".
        let marker = match self.ctx.consent.write_prompt(&prompt_id, &prompt) {
            Ok(_) => true,
            Err(e) => {
                warn!(%e, native, "record: could not write the .pending consent marker");
                false
            }
        };
        let have_surface = native || (marker && self.ctx.companion.up().await);
        info!(session = %self.ctx.session_id, native, have_surface, "consent prompt surface (record)");
        if let Ok(mut p) = self.prompt.lock() {
            *p = Some(prompt_id.clone());
        }
        let decision = self
            .ctx
            .consent
            .request_with_mode(&prompt_id, crate::consent::Mode::Prompt { timeout: window })
            .await;
        self.ctx.indicator.hide_prompt(&prompt_id);
        if let Ok(mut p) = self.prompt.lock() {
            *p = None;
        }
        (decision, have_surface)
    }

    async fn stop(&self, id: String) {
        if !self.owns_active() {
            let mut s = StateMsg::new(&id, "idle");
            s.reason = Some("not_recording".into());
            send(&self.dc, &s).await;
            return;
        }
        // The follower reports the ending once the file is final.
        tokio::spawn(async move {
            if let Some(m) = manager() {
                m.stop_with(Some("requested")).await;
            }
        });
    }

    async fn status(&self) {
        let id = self
            .current
            .lock()
            .ok()
            .and_then(|c| c.clone())
            .unwrap_or_default();
        let s = match manager().map(|m| m.state()) {
            Some(st) if st.active && self.owns_active() => {
                let mut s = StateMsg::new(&id, "recording");
                s.name = file_name(st.path.as_deref());
                s.bytes = st.bytes;
                s.duration_ms = st.duration_ms;
                s.audio = st.system_audio;
                s
            }
            _ => StateMsg::new(&id, "idle"),
        };
        send(&self.dc, &s).await;
    }

    /// The channel closed: the session is over. A prompt standing for it is
    /// withdrawn, and a recording it started ends `session_ended`.
    async fn session_gone(&self) {
        let prompt = self.prompt.lock().ok().and_then(|p| p.clone());
        if let Some(p) = prompt {
            self.ctx.consent.cancel(&p);
        }
        if self.owns_active() {
            info!(session = %self.ctx.session_id, "session ended — stopping its remote recording");
            tokio::spawn(async move {
                if let Some(m) = manager() {
                    m.stop_with(Some("session_ended")).await;
                }
            });
        }
    }

    /// Follow a recording this session started until its file is final:
    /// progress once a second, a stop when the owner switches remote
    /// recording off or the session is gone, then how it ended.
    async fn follow(self: Arc<Self>, id: String) {
        let Some(m) = manager() else { return };
        let sid = self.ctx.session_id;
        let mut gates_rx = gates_tx().subscribe();
        let mut tick = tokio::time::interval(PROGRESS_EVERY);
        let mut stopping = false;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                changed = gates_rx.changed() => {
                    if changed.is_err() {
                        // Unreachable (the sender is a static); keep ticking.
                    }
                }
            }
            let st = m.state();
            if st.active && self.owns_active() {
                let off = !gates().enabled;
                let gone = self.dc.ready_state() == RTCDataChannelState::Closed;
                if !stopping && (off || gone) {
                    stopping = true;
                    let reason = if off { "gate_revoked" } else { "session_ended" };
                    info!(session = %sid, reason, "stopping a remote recording");
                    tokio::spawn(async move {
                        if let Some(m) = manager() {
                            m.stop_with(Some(reason)).await;
                        }
                    });
                }
                let mut s = StateMsg::new(&id, "recording");
                s.name = file_name(st.path.as_deref());
                s.bytes = st.bytes;
                s.duration_ms = st.duration_ms;
                s.audio = st.system_audio;
                send(&self.dc, &s).await;
                continue;
            }
            // Over: how it ended is the state's `last`.
            let last = st.last.clone().unwrap_or_default();
            let name = file_name(last.path.as_deref());
            let mut s = StateMsg::new(
                &id,
                if last.path.is_some() {
                    "stopped"
                } else {
                    "failed"
                },
            );
            s.reason = Some(if last.reason.is_empty() {
                "requested".into()
            } else {
                last.reason.clone()
            });
            s.detail = last.detail.clone();
            s.name = name.clone();
            s.bytes = last.bytes;
            s.duration_ms = last.duration_ms;
            send(&self.dc, &s).await;
            self.report(
                RecordingActivityKind::Stopped,
                name,
                Some(last.bytes),
                Some(last.duration_ms),
                s.reason.clone(),
            );
            self.ctx.indicator.set_recording(sid, false);
            if let Ok(mut c) = self.current.lock() {
                *c = None;
            }
            info!(session = %sid, reason = ?s.reason, bytes = last.bytes, "remote recording ended");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> Gates {
        Gates {
            enabled: true,
            audio: false,
        }
    }

    #[test]
    fn nothing_is_advertised_unless_the_owner_opted_in_and_a_recorder_can_run() {
        assert!(advertise(Gates::default(), true, true).is_empty());
        assert!(
            advertise(on(), false, true).is_empty(),
            "a SYSTEM/root service cannot record until P1e — it must not claim to"
        );
        assert_eq!(advertise(on(), true, true), vec!["remote"]);
        let both = Gates {
            enabled: true,
            audio: true,
        };
        assert_eq!(advertise(both, true, true), vec!["remote", "remote-audio"]);
        assert_eq!(
            advertise(both, true, false),
            vec!["remote"],
            "no audio build, no audio claim"
        );
        // The audio gate alone opts nothing in.
        let audio_only = Gates {
            enabled: false,
            audio: true,
        };
        assert!(advertise(audio_only, true, true).is_empty());
    }

    #[test]
    fn the_prechecks_refuse_in_order_and_by_name() {
        let off = Gates::default();
        assert_eq!(
            precheck(off, false, true, false, true),
            Err("disabled_on_device")
        );
        assert_eq!(
            precheck(on(), false, false, true, false),
            Err("unavailable")
        );
        assert_eq!(
            precheck(on(), true, true, true, false),
            Err("audio_not_allowed")
        );
        let both = Gates {
            enabled: true,
            audio: true,
        };
        assert_eq!(
            precheck(both, true, true, false, false),
            Err("audio_not_allowed"),
            "an audio gate on a build without audio"
        );
        assert_eq!(precheck(both, true, true, true, true), Err("busy"));
        assert_eq!(precheck(both, true, true, true, false), Ok(()));
        assert_eq!(precheck(on(), true, false, false, false), Ok(()));
    }

    #[test]
    fn the_wire_parses_and_speaks_the_documented_shapes() {
        assert_eq!(
            serde_json::from_str::<Incoming>(r#"{"t":"rc:record.start","id":"a1","audio":true}"#)
                .unwrap(),
            Incoming::Start {
                id: "a1".into(),
                audio: true
            }
        );
        assert_eq!(
            serde_json::from_str::<Incoming>(r#"{"t":"rc:record.start"}"#).unwrap(),
            Incoming::Start {
                id: String::new(),
                audio: false
            },
            "audio is OFF unless asked for"
        );
        assert_eq!(
            serde_json::from_str::<Incoming>(r#"{"t":"rc:record.stop","id":"a1"}"#).unwrap(),
            Incoming::Stop { id: "a1".into() }
        );
        // ⚠️ There is no microphone on this wire: a field that asks for one is
        // ignored, never honoured.
        assert_eq!(
            serde_json::from_str::<Incoming>(
                r#"{"t":"rc:record.start","id":"m","microphone":true}"#
            )
            .unwrap(),
            Incoming::Start {
                id: "m".into(),
                audio: false
            }
        );

        let mut s = StateMsg::new("a1", "refused");
        s.reason = Some("no_indicator_surface".into());
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["t"], "rc:record.state");
        assert_eq!(v["state"], "refused");
        assert_eq!(v["reason"], "no_indicator_surface");
        assert!(v.get("name").is_none() && v.get("detail").is_none());
    }

    #[test]
    fn the_controller_is_told_a_file_name_never_a_path() {
        let p = if cfg!(windows) {
            r"C:\Users\someone\Videos\Roomler\Roomler Recording 2026-09-25 14-30-12.mp4"
        } else {
            "/home/someone/Videos/Roomler/Roomler Recording 2026-09-25 14-30-12.mp4"
        };
        assert_eq!(
            file_name(Some(p)).as_deref(),
            Some("Roomler Recording 2026-09-25 14-30-12.mp4")
        );
        assert_eq!(file_name(None), None);
    }

    #[test]
    fn adopt_follows_the_config_and_only_signals_a_change() {
        let mut cfg = roomler_node_core::config::test_fixture();
        cfg.record_remote_enabled = true;
        adopt(&cfg);
        let rx = gates_tx().subscribe();
        adopt(&cfg);
        assert!(!rx.has_changed().unwrap(), "the same gates are not news");
        cfg.record_remote_enabled = false;
        adopt(&cfg);
        assert!(
            rx.has_changed().unwrap(),
            "an OFF must reach a recording in progress"
        );
        assert_eq!(gates(), Gates::default());
    }
}

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
//! P1f — **the unattended exception to gate 7.** A service with nobody signed
//! in (SYSTEM or root, no console user) records a remote session with no
//! banner, because there is nobody to show one to: the owner's gate 1 is
//! what allows it, the controller is told (`unattended: true` on the state),
//! and the recorder is the daemon itself writing into the daemon's own folder,
//! locked to the service side, never a person's. Someone who signs in is
//! never recorded without a banner:
//! - the identity is decided ONCE, before the indicator is skipped, and the
//!   manager refuses the launch if it no longer holds;
//! - an unattended recording already running is stopped `session_changed` the
//!   moment someone signs in ([`Handler::follow`], every tick).
//!
//! All three read who is signed in FRESH: a cached "nobody" read twice within
//! milliseconds is one observation, and review found the launch re-check a
//! no-op on the cache before this was so. A LOCAL recording still needs
//! someone at the device.
//!
//! The microphone is not a remote option: [`RecordingManager::start_remote`]
//! has no parameter for it.
//!
//! P3b-3 — **a recording outlives its session for the re-attach grace.** The
//! reconnect ladder mints a new session id on every drop, and a relay flap or
//! a reloaded viewer must not cost a recording. When the session that owns a
//! recording goes, the recording is DETACHED, not stopped:
//! - its banner stays up, marked reconnecting (the indicator keeps a
//!   recording session's entry when the session ends): a recording is never
//!   unseen;
//! - the SAME controller (user id, not session id) on a new session holding
//!   RECORD picks it up when its `record` channel asks for the status, and
//!   the new session's banner takes over;
//! - nobody else can: another controller is told nothing is recording;
//! - after [`RecordingManager::reattach_grace`] with nobody back, it stops
//!   `session_ended`. Meanwhile the owner's OFF still stops it
//!   (`gate_revoked`), and an unattended one still stops `session_changed`
//!   the moment someone signs in.
//!
//! ⚠️ "Goes" is read from the channel's STATE, not only from its `on_close`:
//! a channel closed from this side often never fires it, and that is every
//! network drop (the device's own session watchdog closes the peer). See
//! [`channel_gone`].
//!
//! Exactly one follower speaks for a recording at a time ([`FOLLOWER`]), so
//! its ending is reported once.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

use super::launch::Identity;
use super::manager::{RecordingManager, RemoteInitiator, StartError};

/// After a host says no, the same session may not ask again for this long:
/// a controller must not be able to wear the person down with prompts.
pub const DENY_COOLDOWN: Duration = Duration::from_secs(60);

/// How often a recording in progress reports its size and length.
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

/// How often a session's `record` channel is looked at for its end (see
/// [`channel_gone`]).
const WATCH_EVERY: Duration = Duration::from_millis(500);

/// Is this session's channel over, whichever end closed it?
///
/// ⚠️ Not `== Closed`, and not `on_close` alone. A close from the FAR end
/// always reaches `Closed` and fires `on_close`. A channel closed from THIS
/// side (the peer's own `close()`: the session watchdog after a network drop,
/// a terminate) is set `Closing` and its read loop told to stop, and whether
/// it then reaches `Closed` and fires `on_close` depends on which branch that
/// loop's `select!` takes: in the tests, most drops (8 of 13) stayed
/// `Closing` for good. A dropped network never hears from the far end, and
/// it is the case the re-attach grace exists for: a recording that waited
/// for `Closed` ran on, never detached.
///
/// Not `!= Open` either: webrtc-rs hands a channel the far end opened to
/// `on_data_channel` before it is open.
fn channel_gone(dc: &RTCDataChannel) -> bool {
    matches!(
        dc.ready_state(),
        RTCDataChannelState::Closing | RTCDataChannelState::Closed
    )
}

// ── P3b-3: a recording whose session dropped ───────────────────────────────

/// A remote recording whose session is gone, waiting for its controller to
/// come back on a new one. One at a time, like recordings.
struct Detached {
    /// The session it belonged to (its banner entry, its reports).
    session_id: ObjectId,
    /// Who may pick it up: the USER, since every reconnect is a new session.
    controller_user_id: ObjectId,
    /// The controller's handle for it: every state it is sent carries it.
    id: String,
    unattended: bool,
    /// The dropped session's surfaces: its banner comes down, and the ending
    /// is reported on its queue, if nobody picks it up.
    indicator: crate::indicator::ViewerIndicator,
    outbound: mpsc::Sender<ClientMsg>,
}

static DETACHED: StdMutex<Option<Detached>> = StdMutex::new(None);

/// P3b-3 — the handler whose session holds the remote recording in progress.
/// The controller's next session may ask for the status before that
/// session's end has been seen (its channel is looked at twice a second);
/// with this it can see the drop itself and detach the recording first,
/// instead of answering "idle" to the one controller it belongs to.
static HOLDER: StdMutex<Option<std::sync::Weak<Handler>>> = StdMutex::new(None);

fn hold(h: &Arc<Handler>) {
    if let Ok(mut slot) = HOLDER.lock() {
        *slot = Some(Arc::downgrade(h));
    }
}

/// Which follower speaks for the recording in progress. Every [`Handler::follow`]
/// takes the next number; one that finds a later number has lost the
/// recording to another session and stops quietly, so an ending is reported
/// once, by whoever holds it last.
static FOLLOWER: AtomicU64 = AtomicU64::new(0);

/// Take the detached recording if `pick` says it may be taken.
fn take_detached(pick: impl FnOnce(&Detached) -> bool) -> Option<Detached> {
    let mut d = DETACHED.lock().ok()?;
    if d.as_ref().is_some_and(pick) {
        d.take()
    } else {
        None
    }
}

/// Is `session_id`'s recording the one waiting to be picked up?
fn is_detached(session_id: ObjectId) -> bool {
    DETACHED
        .lock()
        .ok()
        .is_some_and(|d| d.as_ref().is_some_and(|d| d.session_id == session_id))
}

/// P3b-3 — watch a detached recording until it is picked up or ends: every
/// tick, the owner's OFF (`gate_revoked`), someone signing in at an
/// unattended one (`session_changed`), the recording ending on its own (the
/// host's Stop, a full disk), or the grace running out (`session_ended`).
/// Whichever takes the detached recording first owns its ending: this task,
/// or the controller's next session.
async fn grace_watch(session_id: ObjectId) {
    let Some(m) = manager() else { return };
    let deadline = Instant::now() + m.reattach_grace();
    // The first tick is immediate: with no grace at all, it stops at once.
    let mut tick = tokio::time::interval(PROGRESS_EVERY);
    loop {
        tick.tick().await;
        let Some(unattended) = DETACHED.lock().ok().and_then(|d| {
            d.as_ref()
                .filter(|d| d.session_id == session_id)
                .map(|d| d.unattended)
        }) else {
            // Picked up by the controller's next session.
            return;
        };
        let reason = if !m.state().active {
            // It ended by itself; its own reason stands.
            None
        } else if !gates().enabled {
            Some("gate_revoked")
        } else if unattended && fresh_remote_identity(m).await != Ok(Identity::Unattended) {
            Some("session_changed")
        } else if Instant::now() >= deadline {
            Some("session_ended")
        } else {
            continue;
        };
        let Some(d) = take_detached(|d| d.session_id == session_id) else {
            return;
        };
        if let Some(r) = reason {
            info!(session = %session_id, reason = r, "stopping a detached remote recording");
            m.stop_with(Some(r)).await;
        }
        let last = m.state().last.unwrap_or_default();
        let msg = ClientMsg::RecordingActivity {
            session_id: d.session_id,
            kind: RecordingActivityKind::Stopped,
            name: file_name(last.path.as_deref()),
            bytes: Some(last.bytes),
            duration_ms: Some(last.duration_ms),
            reason: Some(if last.reason.is_empty() {
                "session_ended".into()
            } else {
                last.reason.clone()
            }),
        };
        if d.outbound.try_send(msg).is_err() {
            tracing::debug!(session = %d.session_id, "recording activity dropped (queue full or closed)");
        }
        d.indicator.end_recording(d.session_id);
        info!(session = %d.session_id, reason = %last.reason, "detached remote recording ended");
        return;
    }
}

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

/// P1f — [`RecordingManager::identity_remote_fresh`] off the async runtime:
/// asking afresh who is signed in runs `loginctl` on Linux. An answer that
/// cannot be had reads as "nobody to record as" at a start (refused) and as
/// "someone" in the watch (stopped): closed both ways.
async fn fresh_remote_identity(
    m: &'static Arc<RecordingManager>,
) -> Result<Identity, super::launch::Refusal> {
    tokio::task::spawn_blocking(move || m.identity_remote_fresh())
        .await
        .unwrap_or(Err(super::launch::Refusal::NoConsoleUser))
}

/// What this device advertises in `AgentCaps.record` right now.
pub fn advertised() -> Vec<String> {
    advertise(
        gates(),
        manager().is_some_and(|m| m.available_remote()),
        cfg!(feature = "audio"),
    )
}

/// Pure half of [`advertised`]. Nothing at all unless a recorder can serve a
/// remote session here: silence is how the hub learns "no recorder", and a
/// viewer then shows no Record control. Then `available` (P3c-2: the device
/// HAS the feature — capability, never permission); `remote` only while the
/// owner's gate is on; `remote-audio` only on top of that, with the audio
/// gate on and an audio build.
pub fn advertise(g: Gates, can_record: bool, audio_built: bool) -> Vec<String> {
    let mut out = Vec::new();
    if !can_record {
        return out;
    }
    out.push(RecordCap::Available.wire().to_string());
    if g.enabled {
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
    /// P3b-2 — this controller's finished recordings on this device.
    #[serde(rename = "rc:record.list")]
    List {
        #[serde(default)]
        id: String,
    },
    /// P3b-2 — send one of them from byte `offset` (a resumed transfer asks
    /// for where the last one broke off).
    #[serde(rename = "rc:record.get")]
    Get {
        #[serde(default)]
        id: String,
        name: String,
        #[serde(default)]
        offset: u64,
    },
    /// P3b-2 — abandon the transfer in progress.
    #[serde(rename = "rc:record.cancel")]
    Cancel {
        #[serde(default)]
        id: String,
    },
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
    /// FR-85 P1f — recording with nobody signed in at the device, so nothing
    /// on its screen says so. Absent (false) otherwise; a viewer that does
    /// not know the field reads nothing different.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unattended: bool,
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
    /// P1f — the recording in progress runs unattended (a service with
    /// nobody signed in), for the re-attach grace to keep watching for a
    /// sign-in once this session is gone.
    unattended: AtomicBool,
    /// P3b-3 — THIS session asked the recording to end (the controller's
    /// Stop, or the follower's own gate / sign-in stop). A drop while it
    /// finalizes then neither detaches it nor silences its report: this
    /// session's follower still reports the ending, once.
    ending: AtomicBool,
    /// The session's end has been handled ([`Handler::session_gone`] runs
    /// once: from `on_close`, or from the channel watch that sees an end
    /// `on_close` never reports).
    gone: AtomicBool,
    /// P3b-2 — the transfer in progress (`id`, its cancel flag): one at a
    /// time per session.
    transfer: StdMutex<Option<(String, Arc<AtomicBool>)>>,
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
        unattended: AtomicBool::new(false),
        ending: AtomicBool::new(false),
        gone: AtomicBool::new(false),
        transfer: StdMutex::new(None),
    });
    // The session's end, two ways: `on_close` when the far end closed the
    // channel, and a look at the channel's state for when this side did,
    // which `on_close` never reports (see `channel_gone`).
    let on_close = h.clone();
    dc.on_close(Box::new(move || {
        let h = on_close.clone();
        Box::pin(async move {
            h.session_gone().await;
        })
    }));
    let watched = Arc::downgrade(&h);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(WATCH_EVERY).await;
            let Some(h) = watched.upgrade() else { return };
            if channel_gone(&h.dc) {
                h.session_gone().await;
                return;
            }
        }
    });
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
                Ok(Incoming::List { id }) => h.list(id).await,
                // A transfer runs for as long as the file takes: off the
                // message loop, so a Cancel can get through.
                Ok(Incoming::Get { id, name, offset }) => {
                    tokio::spawn(h.get(id, name, offset));
                }
                Ok(Incoming::Cancel { id }) => h.cancel(&id),
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
    send_json(dc, s).await
}

async fn send_json<T: Serialize>(dc: &RTCDataChannel, v: &T) {
    if let Ok(text) = serde_json::to_string(v)
        && let Err(e) = dc.send_text(text).await
    {
        tracing::debug!(%e, "record DC: send failed (channel closing)");
    }
}

// ── P3b-2: downloading a remote recording ──────────────────────────────────

/// A transfer's chunk: the files channel's size, so the two pumps behave
/// alike under the same SCTP limits.
const CHUNK: usize = 64 * 1024;
/// Stop queuing chunks past this much buffered on the channel (the files
/// channel's number): a multi-GB file must not become a multi-GB queue.
const BACKPRESSURE_HIGH: usize = 4 * 1024 * 1024;

/// One of this controller's recordings, as the list says it.
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct ListedRecording {
    pub name: String,
    pub bytes: u64,
    pub duration_ms: u64,
    /// RFC 3339.
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub width: u32,
    pub height: u32,
}

/// Does `name`'s sidecar say `controller` started it remotely? The only
/// thing that makes a recording theirs to list or fetch — a recording the
/// person at the device made, or another controller's, is not.
///
/// ⚠️ By USER, never by session: the reconnect ladder mints new session ids,
/// and a controller whose connection flapped must still reach their file.
pub fn owned_by(dir: &Path, name: &str, controller: &ObjectId) -> bool {
    let path = dir.join(name);
    let Ok(json) = std::fs::read_to_string(super::sidecar::Sidecar::path_for(&path)) else {
        return false;
    };
    let Ok(sc) = serde_json::from_str::<super::sidecar::Sidecar>(&json) else {
        return false;
    };
    matches!(
        sc.initiator,
        super::sidecar::Initiator::Remote { ref controller_user_id, .. }
            if *controller_user_id == controller.to_hex()
    )
}

/// `controller`'s finished recordings in `dir`, newest first.
pub fn owned_recordings(dir: &Path, controller: &ObjectId) -> Vec<ListedRecording> {
    super::manager::list_recordings(dir)
        .into_iter()
        .filter(|it| owned_by(dir, &it.name, controller))
        .map(|it| ListedRecording {
            name: it.name,
            bytes: it.bytes,
            duration_ms: it.duration_ms,
            started_at: it.started_at,
            stop_reason: it.stop_reason,
            width: it.width,
            height: it.height,
        })
        .collect()
}

/// Open a recording to send it: never through a link. The name has already
/// passed `check_recording_name` (a bare `*.mp4`, no separator, no `..`);
/// this refuses a symlink or junction at that name, and anything that is not
/// a regular file once open.
///
/// ⚠️ Two layers, and each refuses a link on its own, so the unit test stays
/// green with either one deleted. Keep both: the pre-check also keeps a FIFO
/// from blocking `open`, and the no-follow open closes the swap between the
/// check and the open.
pub fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open the link itself, never its target.
        opts.custom_flags(0x0020_0000);
    }
    let file = opts.open(path)?;
    // Swapped between the check and the open? The handle says what was opened.
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
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
            m.available_remote(),
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

        // P1f — decided ONCE, here, and asked FRESH (never a cached "nobody"):
        // the manager re-checks it, fresh again, before the recorder launches,
        // and `follow` stops an unattended recording the moment someone signs
        // in.
        let identity = match fresh_remote_identity(m).await {
            Ok(i) => i,
            Err(r) => return Some(("unavailable", Some(r.message().into()))),
        };
        let unattended = identity == Identity::Unattended;

        // ⚠️ Something on screen says "recording" BEFORE the first frame —
        // wherever there is someone to see it. An unattended host (a service
        // with nobody signed in) has no one: the owner's gate is what allowed
        // it, and the controller is told it records unattended.
        if !unattended {
            let shown = self.ctx.indicator.set_recording(sid, true);
            let companion = shown.listed && self.ctx.companion.up().await;
            if !(shown.native || companion) {
                self.ctx.indicator.end_recording(sid);
                return Some((
                    "no_indicator_surface",
                    Some(
                        "nothing on this device can show that it is being recorded (no banner, \
                         and the Roomler desktop app is not running)"
                            .into(),
                    ),
                ));
            }
        }

        // The session may have gone while the host was asked, or the banner
        // raised: nobody is left to record for, or to answer.
        if channel_gone(&self.dc) {
            if !unattended {
                self.ctx.indicator.end_recording(sid);
            }
            return None;
        }

        let initiator = RemoteInitiator {
            session_id: sid,
            controller_user_id: self.ctx.controller_user_id,
            controller_name: self.ctx.controller_name.clone(),
        };
        match m.start_remote(initiator, audio, identity).await {
            Ok(state) => {
                let name = file_name(state.path.as_deref());
                info!(session = %sid, ?name, audio, "remote recording started");
                if let Ok(mut c) = self.current.lock() {
                    *c = Some(id.to_string());
                }
                self.unattended.store(unattended, Ordering::Release);
                let mut s = StateMsg::new(id, "recording");
                s.name = name.clone();
                s.audio = state.system_audio;
                s.unattended = unattended;
                send(&self.dc, &s).await;
                self.report(RecordingActivityKind::Started, name, None, None, None);
                hold(&self);
                tokio::spawn(self.clone().follow(id.to_string(), unattended));
                None
            }
            Err(e) => {
                self.ctx.indicator.end_recording(sid);
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
        // The follower reports the ending once the file is final — even if
        // the session drops meanwhile (P3b-3: an ending asked for is not
        // detached).
        self.ending.store(true, Ordering::Release);
        tokio::spawn(async move {
            if let Some(m) = manager() {
                m.stop_with(Some("requested")).await;
            }
        });
    }

    async fn status(self: Arc<Self>) {
        // P3b-3 — a recording this controller's previous session left
        // running (a relay flap, a reloaded viewer) continues on this one.
        // Only the same USER: the channel exists only on a session holding
        // RECORD, and a recording is its controller's alone.
        let me = self.ctx.controller_user_id;
        // The session holding it may already be gone without its channel
        // watch having looked yet: if it is this controller's and its channel
        // is over, detach it now rather than answer "idle".
        let holder = HOLDER
            .lock()
            .ok()
            .and_then(|h| h.as_ref().and_then(std::sync::Weak::upgrade));
        if let Some(h) = holder
            && h.ctx.controller_user_id == me
            && channel_gone(&h.dc)
        {
            h.detach_recording();
        }
        if let Some(d) = take_detached(|d| d.controller_user_id == me)
            && self.clone().reattach(d).await
        {
            return;
        }
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

    /// P3b-3 — pick up `d`, which this controller's previous session left
    /// running: the recording moves to this session, this session's banner
    /// says so and the old one comes down, and this session's follower speaks
    /// for it from now on. `false` = it ended before it could be picked up
    /// (the caller answers the plain status).
    async fn reattach(self: Arc<Self>, d: Detached) -> bool {
        let Some(m) = manager() else { return false };
        let sid = self.ctx.session_id;
        if !m.retarget_remote(d.session_id, sid) {
            // It ended between the drop and now: the old banner goes.
            d.indicator.end_recording(d.session_id);
            return false;
        }
        // The new banner first, then the old one down: never a moment with
        // neither (an unattended recording has neither, and needs none).
        if !d.unattended {
            self.ctx.indicator.set_recording(sid, true);
        }
        d.indicator.end_recording(d.session_id);
        if let Ok(mut c) = self.current.lock() {
            *c = Some(d.id.clone());
        }
        self.unattended.store(d.unattended, Ordering::Release);
        let st = m.state();
        let name = file_name(st.path.as_deref());
        info!(
            session = %sid,
            from = %d.session_id,
            ?name,
            "remote recording picked up by its controller's new session"
        );
        let mut s = StateMsg::new(&d.id, "recording");
        s.name = name.clone();
        s.bytes = st.bytes;
        s.duration_ms = st.duration_ms;
        s.audio = st.system_audio;
        s.unattended = d.unattended;
        send(&self.dc, &s).await;
        self.report(RecordingActivityKind::Reattached, name, None, None, None);
        hold(&self);
        tokio::spawn(self.clone().follow(d.id, d.unattended));
        true
    }

    /// P3b-2 — this controller's finished recordings.
    async fn list(&self, id: String) {
        let mut items = Vec::new();
        if let Some(m) = manager() {
            // P1e — each place read as its own identity: the person's folder
            // as the person (so whatever it has been made to point at is read
            // with their rights), the unattended folder (P1f) as the daemon.
            // The first place wins a name, as `get` does.
            for (dir, identity) in m.remote_places().await {
                let who = self.ctx.controller_user_id;
                let found = m
                    .as_identity(identity, move || owned_recordings(&dir, &who))
                    .await
                    .unwrap_or_default();
                for item in found {
                    if !items.iter().any(|i: &ListedRecording| i.name == item.name) {
                        items.push(item);
                    }
                }
            }
        }
        send_json(
            &self.dc,
            &serde_json::json!({ "t": "rc:record.list", "id": id, "items": items }),
        )
        .await;
    }

    async fn transfer_error(&self, id: &str, reason: &str, detail: Option<String>) {
        info!(session = %self.ctx.session_id, %reason, ?detail, "record: transfer refused");
        send_json(
            &self.dc,
            &serde_json::json!({ "t": "rc:record.error", "id": id, "reason": reason, "detail": detail }),
        )
        .await;
    }

    /// P3b-2 — send one of this controller's recordings from `offset`.
    ///
    /// `rc:record.file {id, name, offset, size}`, then the bytes as binary
    /// messages, then `rc:record.done {id, name, bytes, sha256}`: `sha256`
    /// is of the WHOLE file, so a transfer resumed from an offset (the prefix
    /// read from disk, not sent) is checked end to end like any other.
    async fn get(self: Arc<Self>, id: String, name: String, offset: u64) {
        if let Err(detail) = super::manager::check_recording_name(&name) {
            self.transfer_error(&id, "bad_name", Some(detail)).await;
            return;
        }
        let Some(m) = manager() else {
            self.transfer_error(&id, "unavailable", None).await;
            return;
        };
        let places = m.remote_places().await;
        if places.is_empty() {
            self.transfer_error(&id, "unavailable", None).await;
            return;
        }
        let who = self.ctx.controller_user_id;
        // P1e — the ownership check and the open run as each place's
        // identity, so a link or a junction in the person's folder reaches
        // only what the person could read themselves. The first place that
        // holds this controller's recording by that name answers (P1f: the
        // person's folder, then the unattended one), as the list does.
        let mut opened = None;
        for (dir, identity) in places {
            let name_c = name.clone();
            opened = m
                .as_identity(identity, move || {
                    if !owned_by(&dir, &name_c, &who) {
                        return None;
                    }
                    Some(open_no_follow(&dir.join(&name_c)))
                })
                .await
                .flatten();
            if opened.is_some() {
                break;
            }
        }
        // ⚠️ Someone else's recording and no recording look alike: a
        // controller learns nothing about files that are not theirs.
        let file = match opened {
            Some(Ok(f)) => f,
            Some(Err(e)) => {
                self.transfer_error(&id, "not_found", Some(e.to_string()))
                    .await;
                return;
            }
            None => {
                self.transfer_error(&id, "not_found", None).await;
                return;
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        // The slot is claimed in ONE expression, so the (non-`Send`) guard is
        // gone before anything awaits.
        let claimed = match self.transfer.lock() {
            Ok(mut t) if t.is_none() => {
                *t = Some((id.clone(), cancel.clone()));
                true
            }
            Ok(_) => false,
            Err(_) => return,
        };
        if !claimed {
            self.transfer_error(&id, "transfer_in_progress", None).await;
            return;
        }
        let outcome = self.pump(&id, file, &name, offset, &cancel).await;
        if let Ok(mut t) = self.transfer.lock() {
            *t = None;
        }
        match outcome {
            Ok(sent) => {
                info!(session = %self.ctx.session_id, %name, offset, sent, "record: download sent");
                self.report(
                    RecordingActivityKind::Downloaded,
                    Some(name),
                    Some(sent),
                    None,
                    None,
                );
            }
            Err((reason, detail)) => self.transfer_error(&id, reason, detail).await,
        }
    }

    /// `std_file` was opened as the recorder's identity (P1e); reading an
    /// open handle needs no identity at all.
    async fn pump(
        &self,
        id: &str,
        std_file: std::fs::File,
        name: &str,
        offset: u64,
        cancel: &AtomicBool,
    ) -> Result<u64, (&'static str, Option<String>)> {
        use sha2::{Digest, Sha256};
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let read_failed = |e: std::io::Error| ("read_failed", Some(e.to_string()));
        let size = std_file.metadata().map_err(read_failed)?.len();
        if offset > size {
            return Err((
                "bad_offset",
                Some(format!("{offset} is past the end ({size})")),
            ));
        }
        let mut file = tokio::fs::File::from_std(std_file);
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        // The part the controller already has: hashed, not sent.
        let mut left = offset;
        while left > 0 {
            let want = buf.len().min(left as usize);
            let n = file.read(&mut buf[..want]).await.map_err(read_failed)?;
            if n == 0 {
                return Err(("read_failed", Some("the file shrank".into())));
            }
            hasher.update(&buf[..n]);
            left -= n as u64;
        }
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(read_failed)?;
        send_json(
            &self.dc,
            &serde_json::json!({ "t": "rc:record.file", "id": id, "name": name, "offset": offset, "size": size }),
        )
        .await;
        let mut sent: u64 = 0;
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(("cancelled", None));
            }
            if channel_gone(&self.dc) {
                return Err(("session_ended", None));
            }
            // ⚠️ `channel_gone`, not `== Closed`: a channel this side closed
            // may never reach `Closed`, and that check could not end this wait.
            while self.dc.buffered_amount().await > BACKPRESSURE_HIGH {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if cancel.load(Ordering::Acquire) {
                    return Err(("cancelled", None));
                }
                if channel_gone(&self.dc) {
                    return Err(("session_ended", None));
                }
            }
            let n = file.read(&mut buf).await.map_err(read_failed)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            self.dc
                .send(&bytes::Bytes::copy_from_slice(&buf[..n]))
                .await
                .map_err(|e| ("send_failed", Some(e.to_string())))?;
            sent += n as u64;
        }
        send_json(
            &self.dc,
            &serde_json::json!({
                "t": "rc:record.done", "id": id, "name": name, "bytes": sent,
                "size": size, "sha256": hex::encode(hasher.finalize()),
            }),
        )
        .await;
        Ok(sent)
    }

    fn cancel(&self, id: &str) {
        if let Ok(t) = self.transfer.lock()
            && let Some((tid, flag)) = t.as_ref()
            && (id.is_empty() || tid == id)
        {
            flag.store(true, Ordering::Release);
        }
    }

    /// The channel closed: the session is over. A prompt standing for it is
    /// withdrawn. A recording it holds is DETACHED (P3b-3): it waits for its
    /// controller's next session for the re-attach grace, its banner still
    /// up, and ends `session_ended` if nobody comes back ([`grace_watch`]).
    /// Once, whichever of `on_close` and the channel watch gets here first.
    async fn session_gone(&self) {
        if self.gone.swap(true, Ordering::AcqRel) {
            return;
        }
        let prompt = self.prompt.lock().ok().and_then(|p| p.clone());
        if let Some(p) = prompt {
            self.ctx.consent.cancel(&p);
        }
        // A transfer has nowhere left to go (the controller resumes it from
        // its offset on the next session).
        self.cancel("");
        self.detach_recording();
    }

    /// P3b-3 — put this session's recording into the re-attach grace. Once:
    /// the session's end (`session_gone`) and the follower finding the
    /// channel gone both land here, and whichever comes first detaches it.
    /// The follower's look is what stops it following; the detach is the
    /// same either way.
    ///
    /// A recording this session already asked to end is left to finish: its
    /// follower reports the ending. One that moved to another session (picked
    /// up) is not this session's any more.
    fn detach_recording(&self) {
        if !self.owns_active() || self.ending.load(Ordering::Acquire) {
            return;
        }
        let sid = self.ctx.session_id;
        {
            let Ok(mut slot) = DETACHED.lock() else {
                return;
            };
            if slot.as_ref().is_some_and(|d| d.session_id == sid) {
                return;
            }
            *slot = Some(Detached {
                session_id: sid,
                controller_user_id: self.ctx.controller_user_id,
                id: self
                    .current
                    .lock()
                    .ok()
                    .and_then(|c| c.clone())
                    .unwrap_or_default(),
                unattended: self.unattended.load(Ordering::Acquire),
                indicator: self.ctx.indicator.clone(),
                outbound: self.ctx.outbound.clone(),
            });
        }
        let grace = manager().map(|m| m.reattach_grace()).unwrap_or_default();
        info!(
            session = %sid,
            grace_s = grace.as_secs(),
            "session ended while recording — waiting for its controller to come back"
        );
        tokio::spawn(grace_watch(sid));
    }

    /// Follow a recording this session started (or picked up, P3b-3) until
    /// its file is final: progress once a second, a stop when the owner
    /// switches remote recording off or someone signs in at an unattended
    /// one, then how it ended. A session that drops hands the recording to
    /// the re-attach grace and stops following it.
    async fn follow(self: Arc<Self>, id: String, unattended: bool) {
        let Some(m) = manager() else { return };
        let me = FOLLOWER.fetch_add(1, Ordering::AcqRel) + 1;
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
            // P3b-3 — another session's follower speaks for it now.
            if FOLLOWER.load(Ordering::Acquire) != me {
                return;
            }
            let st = m.state();
            if st.active && self.owns_active() {
                // P3b-3 — the session is gone and this session had not asked
                // it to end: the recording is detached (here if the close
                // callback has not done it), and whoever picks it up, or the
                // grace, reports how it ends. A stop this session asked for
                // is still reported here.
                if !self.ending.load(Ordering::Acquire)
                    && (is_detached(sid) || channel_gone(&self.dc))
                {
                    self.detach_recording();
                    return;
                }
                let off = !gates().enabled;
                // P1f — an UNATTENDED recording ends the moment someone signs
                // in: they never saw it start, and nothing on their screen
                // says it runs. Asked fresh every tick, never from a cache;
                // an answer that cannot be had counts as "someone" (closed).
                let signed_in = unattended
                    && !stopping
                    && fresh_remote_identity(m).await != Ok(Identity::Unattended);
                if !stopping && (off || signed_in) {
                    stopping = true;
                    self.ending.store(true, Ordering::Release);
                    let reason = if off {
                        "gate_revoked"
                    } else {
                        "session_changed"
                    };
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
                s.unattended = unattended;
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
            self.ctx.indicator.end_recording(sid);
            if let Ok(mut c) = self.current.lock() {
                *c = None;
            }
            self.ending.store(false, Ordering::Release);
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
    fn nothing_is_served_unless_the_owner_opted_in_and_a_recorder_can_run() {
        // P3c-2: a recorder that could serve says only that it exists.
        assert_eq!(advertise(Gates::default(), true, true), vec!["available"]);
        assert!(
            advertise(on(), false, true).is_empty(),
            "a host that cannot record a remote session must claim nothing, not even `available`"
        );
        assert!(advertise(Gates::default(), false, true).is_empty());
        assert_eq!(advertise(on(), true, true), vec!["available", "remote"]);
        let both = Gates {
            enabled: true,
            audio: true,
        };
        assert_eq!(
            advertise(both, true, true),
            vec!["available", "remote", "remote-audio"]
        );
        assert_eq!(
            advertise(both, true, false),
            vec!["available", "remote"],
            "no audio build, no audio claim"
        );
        // The audio gate alone opts nothing in.
        let audio_only = Gates {
            enabled: false,
            audio: true,
        };
        assert_eq!(advertise(audio_only, true, true), vec!["available"]);
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

    /// A recording and its sidecar, in `dir`.
    fn made(dir: &Path, name: &str, initiator: Option<super::super::sidecar::Initiator>) {
        use super::super::mp4::ColorInfo;
        use super::super::sidecar::{AudioInfo, SIDECAR_VERSION, Sidecar};
        let path = dir.join(name);
        std::fs::write(&path, b"not really an mp4").unwrap();
        if let Some(initiator) = initiator {
            let sc = Sidecar {
                version: SIDECAR_VERSION,
                file: name.into(),
                initiator,
                started_at: "2026-09-25T12:30:12Z".into(),
                ended_at: None,
                duration_ms: 1000,
                width: 320,
                height: 240,
                fps: 30,
                codec: "h264".into(),
                encoder: "openh264".into(),
                color: ColorInfo::BT601_LIMITED,
                audio: AudioInfo::default(),
                frames: 30,
                late_ticks: 0,
                events: Vec::new(),
                stop_reason: None,
                bytes: 17,
                pointer: None,
            };
            std::fs::write(Sidecar::path_for(&path), sc.to_json()).unwrap();
        }
    }

    /// P3b-2 — a recording is a controller's to list or fetch only when its
    /// sidecar says THAT user started it remotely: not the person at the
    /// device's own, not another controller's, not one with no sidecar or a
    /// broken one.
    #[test]
    fn only_the_controller_who_started_it_owns_a_recording() {
        use super::super::sidecar::Initiator;
        let dir = tempfile::tempdir().unwrap();
        let me = ObjectId::new();
        let other = ObjectId::new();
        let remote = |who: &ObjectId| Initiator::Remote {
            controller_user_id: who.to_hex(),
            controller_name: Some("x".into()),
        };
        made(dir.path(), "mine.mp4", Some(remote(&me)));
        made(dir.path(), "theirs.mp4", Some(remote(&other)));
        made(
            dir.path(),
            "local.mp4",
            Some(Initiator::Local { user: None }),
        );
        made(dir.path(), "bare.mp4", None);
        made(dir.path(), "broken.mp4", None);
        std::fs::write(dir.path().join("broken.mp4.roomler.json"), b"{nope").unwrap();

        assert!(owned_by(dir.path(), "mine.mp4", &me));
        for name in [
            "theirs.mp4",
            "local.mp4",
            "bare.mp4",
            "broken.mp4",
            "absent.mp4",
        ] {
            assert!(!owned_by(dir.path(), name, &me), "{name}");
        }
        let listed: Vec<String> = owned_recordings(dir.path(), &me)
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(listed, vec!["mine.mp4"]);
        assert!(owned_recordings(dir.path(), &ObjectId::new()).is_empty());
    }

    /// P3b-2 — a download never follows a link out of the folder.
    #[test]
    fn a_link_at_a_recordings_name_is_never_opened() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, b"secret").unwrap();
        let real = dir.path().join("real.mp4");
        std::fs::write(&real, b"a recording").unwrap();
        assert!(open_no_follow(&real).is_ok(), "the positive control");
        assert!(
            open_no_follow(dir.path()).is_err(),
            "a directory is not a recording"
        );

        let link = dir.path().join("link.mp4");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&outside, &link).is_ok();
        // Creating a symlink on Windows needs a privilege (or developer
        // mode): where it is refused, the case cannot be staged here.
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&outside, &link).is_ok();
        if made {
            assert!(
                open_no_follow(&link).is_err(),
                "followed a link out of the folder"
            );
        }
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

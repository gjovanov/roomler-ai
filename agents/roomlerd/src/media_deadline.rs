// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-148 P4b-3 (D55) — the agent's half of the deadline-limited video
//! channel: which of a session's two video channels a frame goes on, and
//! whether it goes at all.
//!
//! A framed session's viewer opens two video channels (FR-148 P4b-2):
//! `video-bytes`, ordered and reliable, and `video-deadline` beside it,
//! unordered with `maxRetransmits: 0`. On a leg that is TCP somewhere a lost
//! chunk on the ordered channel holds every message behind it until it is
//! sent again, and the backlog has no bound; on `video-deadline` a lost chunk
//! is abandoned (FR-148 P4b-1) and its frame is gone instead. Both send tasks
//! (the VP9-444 pump's and the FFmpeg pump's) reach either channel only
//! through a [`Lane`]:
//!
//! | Piece | Rule |
//! |---|---|
//! | the pick ([`pick`]) | `video-deadline` when the carrier class is TCP somewhere ([`CarrierClass::is_tcp`]: TURN over TCP or TLS, by the carrier line's `relay_transport`, or a mesh path whose overlay tier is not `Direct`) and the viewer opened one on a framed session; `video-bytes` otherwise, and whenever the class is not known |
//! | the class ([`Pump::read`]) | read from the selected pair, at the pump's start and at each 5 s transport re-check, never from `detect_constrained_transport`'s 3 s default: with no pair selected it is `unknown`, so the session starts on `video-bytes` and the re-check picks again once a pair is selected |
//! | the switch | at a key frame, over one `frame_seq` space: a frame dropped for the switch or for D consumes its number too |
//! | the drain rule | once the pick says switch, nothing more is written on the old channel, and every frame is dropped whole until its buffered amount reads 0 (acknowledged, or abandoned and forwarded); then a key frame is asked through the session's gate ([`KeyframeGate`]), frames are dropped until one comes, and it goes whole on the new channel. The wait has no time bound: the switch line says how long it held the video |
//! | the deadline D ([`DEADLINE`]) | under `on`, on `video-deadline` only: a frame older than D at its first write, counted from its hand-off to the send task, is dropped whole, and a key frame is asked through the gate; a frame begun is finished |
//! | the mark | under `on`, once per stream, before the first write on `video-deadline`: `RTCSctpTransport::mark_deadline_stream` (FR-148 P4b-1). Refused, nothing switches, one line says so, and it is tried again at the next key frame |
//! | `shadow` | the pick and D run on the channel `on` would be on, and the lines and counts say what `on` would do; every write is `off`'s, byte for byte |
//!
//! The switch is `ROOMLERD_VIDEO_DEADLINE`, or the config key
//! `video_deadline` (`off` · `shadow` · `on`; unset or anything else is
//! `off`, the kill value), read once per pump beside `ROOMLERD_PATH_MODEL`'s
//! read (`media_delivery::DeliverySampler::from_env`). Under `off` a send
//! task's writes, numbers and holds are today's: the lane takes
//! `video-bytes`' stash as the send task took it, and holds it through the
//! frame's chunked send as the send task's `if let` did (FR-65's `dc_lock`).
//!
//! The lines: `video channel: <from> -> <to> (carrier=<class>,
//! frame_seq=<n>, held_ms=<n>, dropped=<n>)` at each switch (`none` as
//! `<from>` at a pump's first pick), with `(shadow)` after `channel` for the
//! switch `on` would make; one line when the carrier wants `video-deadline`
//! and the session has none open; one when its stream's mark is refused. The
//! counts go on the `path delivery` line (FR-148 P0) under `shadow` and `on`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use tracing::{info, warn};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::sctp_transport::RTCSctpTransport;

use crate::session_carrier::{OverlayPath, PairView, RelayTransport, SessionCarrier};

/// The switch, `ROOMLERD_VIDEO_DEADLINE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    Shadow,
    On,
}

impl Mode {
    pub fn word(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Shadow => "shadow",
            Mode::On => "on",
        }
    }
}

/// The switch, read once per pump, beside `ROOMLERD_PATH_MODEL`'s read
/// (`media_delivery::DeliverySampler::from_env`). A set variable wins over the
/// config key `video_deadline`.
pub fn mode() -> Mode {
    mode_of(tunnel_core::env::node_env("VIDEO_DEADLINE").as_deref())
}

/// `shadow` and `on` as written; unset or any other value is `off`.
pub fn mode_of(value: Option<&str>) -> Mode {
    match value {
        Some("shadow") => Mode::Shadow,
        Some("on") => Mode::On,
        _ => Mode::Off,
    }
}

/// `video-bytes`: ordered and reliable, the channel every DC session has.
pub const BYTES_LABEL: &str = "video-bytes";

/// `video-deadline`: unordered with `maxRetransmits: 0`, opened beside
/// `video-bytes` on a framed session (FR-148 P4b-2). Matched by equality.
pub const DEADLINE_LABEL: &str = "video-deadline";

/// A session's two video channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Bytes,
    Deadline,
}

impl Channel {
    pub fn label(self) -> &'static str {
        match self {
            Channel::Bytes => BYTES_LABEL,
            Channel::Deadline => DEADLINE_LABEL,
        }
    }
}

/// D (FR-148 O-b): the oldest a frame may be at its first write on
/// `video-deadline`, from its hand-off to the send task. Older, it is dropped
/// whole.
pub const DEADLINE: Duration = Duration::from_millis(300);

/// The carrier of a session's selected pair, as the pick reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CarrierClass {
    /// No pair selected, or one whose carrier this end cannot read: the far
    /// end's relay, a relay whose gather left no stamp, a mesh pair the overlay
    /// said nothing about.
    Unknown,
    /// host · srflx · prflx, not over the mesh.
    Direct,
    /// The agent's own TURN allocation, over UDP.
    TurnUdp,
    /// The agent's own TURN allocation, over TCP.
    TurnTcp,
    /// The agent's own TURN allocation, over TLS over TCP.
    TurnTls,
    /// Over the mesh, whose tier for the peer is `Direct`.
    MeshDirect,
    /// Over the mesh, whose tier for the peer is anything but `Direct`.
    MeshRelay,
}

impl CarrierClass {
    /// Every class, in the order of its code.
    pub const ALL: [CarrierClass; 7] = [
        CarrierClass::Unknown,
        CarrierClass::Direct,
        CarrierClass::TurnUdp,
        CarrierClass::TurnTcp,
        CarrierClass::TurnTls,
        CarrierClass::MeshDirect,
        CarrierClass::MeshRelay,
    ];

    pub fn word(self) -> &'static str {
        match self {
            CarrierClass::Unknown => "unknown",
            CarrierClass::Direct => "direct",
            CarrierClass::TurnUdp => "turn_udp",
            CarrierClass::TurnTcp => "turn_tcp",
            CarrierClass::TurnTls => "turn_tls",
            CarrierClass::MeshDirect => "mesh_direct",
            CarrierClass::MeshRelay => "mesh_relay",
        }
    }

    /// TCP somewhere on the leg: `video-deadline`'s carriers.
    pub fn is_tcp(self) -> bool {
        matches!(
            self,
            CarrierClass::TurnTcp | CarrierClass::TurnTls | CarrierClass::MeshRelay
        )
    }

    fn code(self) -> u8 {
        self as u8
    }

    fn of_code(code: u8) -> Self {
        Self::ALL
            .get(usize::from(code))
            .copied()
            .unwrap_or(CarrierClass::Unknown)
    }
}

/// The class of a selected pair: the pair, what carries the agent's own relay
/// for it (the carrier line's `relay_transport`), whether it rides the mesh,
/// and what the overlay said about the remote's peer (`None` when nothing
/// could be learned).
pub fn class_of(
    pair: &PairView,
    relay: RelayTransport,
    rides: bool,
    overlay: Option<&OverlayPath>,
) -> CarrierClass {
    if rides {
        // `overlay_remote_is_relay_tier`'s rule (peer.rs): any tier but
        // `Direct` is a relay.
        return match overlay {
            Some(OverlayPath::Direct) => CarrierClass::MeshDirect,
            Some(OverlayPath::Relay(_) | OverlayPath::Other(_)) => CarrierClass::MeshRelay,
            None => CarrierClass::Unknown,
        };
    }
    match crate::session_carrier::carrier_on(pair, false, None) {
        SessionCarrier::Direct => CarrierClass::Direct,
        SessionCarrier::Turn | SessionCarrier::TurnLocal => match relay {
            RelayTransport::Udp => CarrierClass::TurnUdp,
            RelayTransport::Tcp => CarrierClass::TurnTcp,
            RelayTransport::Tls => CarrierClass::TurnTls,
            // The far end's relay (the agent's own candidate is none), or one
            // whose gather left no stamp: what carries it is not known here.
            RelayTransport::NoRelay | RelayTransport::Unknown => CarrierClass::Unknown,
        },
        _ => CarrierClass::Unknown,
    }
}

/// The pick: `video-deadline` on a carrier that is TCP somewhere, when one is
/// open; `video-bytes` otherwise.
pub fn pick(class: CarrierClass, deadline_open: bool) -> Channel {
    if deadline_open && class.is_tcp() {
        Channel::Deadline
    } else {
        Channel::Bytes
    }
}

/// The class of the pair selected now: `unknown` while none is.
async fn class_now(pc: &RTCPeerConnection) -> CarrierClass {
    let Some((pair, relay, rides)) =
        crate::session_carrier::nominated_pair(pc, Duration::ZERO).await
    else {
        return CarrierClass::Unknown;
    };
    // As the carrier line reads it: the ICE agent's read under `route`, the
    // remote's range otherwise.
    let rides = rides.unwrap_or_else(|| crate::peer::addr_is_overlay_range(&pair.remote_addr));
    let overlay = if rides {
        crate::session_carrier::overlay_path(&pair.remote_addr).await
    } else {
        None
    };
    class_of(&pair, relay, rides, overlay.as_ref())
}

/// rc.130, rc.217 — the least gap between two key frames a session forces on
/// request.
pub const MIN_KF_GAP: Duration = Duration::from_millis(500);

/// rc.130 — a session's gate for forced key frames: the viewer's
/// `rc:keyframe` and, from FR-148 P4b-3, the send tasks' own asks go through
/// it. The flag it sets coalesces (a pump forces at most one IDR per encode
/// however often it is set), and the gap bounds a misbehaving or old
/// controller to one forced IDR per [`MIN_KF_GAP`], so a resync storm cannot
/// pile the largest frames onto a congested link.
#[derive(Debug)]
pub struct KeyframeGate {
    requested: Arc<AtomicBool>,
    last: Mutex<Option<Instant>>,
}

impl KeyframeGate {
    /// The gate over `requested`, the flag the pumps poll before each encode.
    pub fn new(requested: Arc<AtomicBool>) -> Self {
        Self {
            requested,
            last: Mutex::new(None),
        }
    }

    /// One ask, now: whether it passed. FR-135 R0 (N12) counts every ask, and
    /// whether the gap dropped it.
    pub fn ask(&self, session_id: ObjectId) -> bool {
        self.ask_at(session_id, Instant::now())
    }

    fn ask_at(&self, session_id: ObjectId, now: Instant) -> bool {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let allow = last
            .map(|t| now.duration_since(t) >= MIN_KF_GAP)
            .unwrap_or(true);
        crate::session_telemetry::counters(session_id).note_keyframe_ask(allow);
        if allow {
            *last = Some(now);
            drop(last);
            self.requested.store(true, Relaxed);
            crate::session_telemetry::counters(session_id).note_keyframe();
        }
        allow
    }
}

/// What a session's pumps and its control channel share: the session's
/// `video-deadline` once the viewer opened one, and its key frame gate.
pub struct Session {
    deadline: tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>,
    gate: Arc<KeyframeGate>,
}

static SESSIONS: LazyLock<Mutex<HashMap<ObjectId, Weak<Session>>>> =
    LazyLock::new(Default::default);

impl Session {
    /// A session's, found by its id while the returned `Arc` lives. A later
    /// peer of the same session replaces an earlier one's.
    pub fn register(session_id: ObjectId, keyframe_requested: Arc<AtomicBool>) -> Arc<Self> {
        let session = Arc::new(Self {
            deadline: tokio::sync::Mutex::new(None),
            gate: Arc::new(KeyframeGate::new(keyframe_requested)),
        });
        let mut all = SESSIONS.lock().unwrap_or_else(PoisonError::into_inner);
        all.retain(|_, s| s.strong_count() > 0);
        all.insert(session_id, Arc::downgrade(&session));
        session
    }

    /// The session's, while its peer lives.
    pub fn of(session_id: ObjectId) -> Option<Arc<Self>> {
        let all = SESSIONS.lock().unwrap_or_else(PoisonError::into_inner);
        all.get(&session_id).and_then(Weak::upgrade)
    }

    /// The viewer's `video-deadline`, as it opened.
    pub async fn stash(&self, dc: Arc<RTCDataChannel>) {
        *self.deadline.lock().await = Some(dc);
    }

    /// The session's key frame gate.
    pub fn gate(&self) -> Arc<KeyframeGate> {
        Arc::clone(&self.gate)
    }
}

/// A pump's counts for the `path delivery` line: the frames D dropped, the
/// frames switches dropped, the switches, and the channel in use. Under
/// `shadow`, what `on` would have done.
#[derive(Debug)]
pub struct Counters {
    mode: Mode,
    late: AtomicU64,
    drained: AtomicU64,
    switches: AtomicU64,
    /// 0 before the first pick, then 1 + [`Channel`]'s index.
    channel: AtomicU8,
}

impl Counters {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            late: AtomicU64::new(0),
            drained: AtomicU64::new(0),
            switches: AtomicU64::new(0),
            channel: AtomicU8::new(0),
        }
    }

    /// The switch's value, as the pump read it.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The line's part, its counts since the last take; `None` under `off`.
    /// `forward_tsns`: the association's FORWARD TSN count, from its gauges.
    pub fn take(&self, forward_tsns: Option<u64>) -> Option<DeadlineLine> {
        if self.mode == Mode::Off {
            return None;
        }
        Some(DeadlineLine {
            forward_tsns,
            late: self.late.swap(0, Relaxed),
            drained: self.drained.swap(0, Relaxed),
            switches: self.switches.swap(0, Relaxed),
            channel: match self.channel.load(Relaxed) {
                1 => Some(Channel::Bytes),
                2 => Some(Channel::Deadline),
                _ => None,
            },
        })
    }

    fn set_channel(&self, channel: Channel) {
        let code = match channel {
            Channel::Bytes => 1,
            Channel::Deadline => 2,
        };
        self.channel.store(code, Relaxed);
    }
}

/// The video channel's part of the `path delivery` line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeadlineLine {
    /// FORWARD TSN packets the association sent since it began (FR-148
    /// P4b-1's gauge); `None` with no gauges.
    pub forward_tsns: Option<u64>,
    /// Frames D dropped over the line (under `shadow`: would have).
    pub late: u64,
    /// Frames dropped while a switch drained or waited for its key frame.
    pub drained: u64,
    /// Switches over the line (under `shadow`: would have made).
    pub switches: u64,
    /// The channel in use at the line (under `shadow`: `on`'s); `None` before
    /// the first frame.
    pub channel: Option<Channel>,
}

impl DeadlineLine {
    pub fn channel_label(&self) -> &'static str {
        self.channel.map_or("none", Channel::label)
    }
}

/// A pump's handle: its switch value and counters, the session it serves,
/// and the carrier class it last read.
pub struct Pump {
    session_id: ObjectId,
    counters: Arc<Counters>,
    session: Option<Arc<Session>>,
    class: Arc<AtomicU8>,
}

impl Pump {
    /// At the pump's start, once `detect_constrained_transport` returned: the
    /// class of the pair selected by then, `unknown` with none.
    pub async fn start(
        session_id: ObjectId,
        pc: &RTCPeerConnection,
        counters: &Arc<Counters>,
    ) -> Self {
        let session = match counters.mode {
            Mode::Off => None,
            Mode::Shadow | Mode::On => Session::of(session_id),
        };
        let pump = Self {
            session_id,
            counters: Arc::clone(counters),
            session,
            class: Arc::new(AtomicU8::new(CarrierClass::Unknown.code())),
        };
        pump.read(pc).await;
        pump
    }

    /// The selected pair's class, read now; the send task's next frame picks
    /// by it. At the pump's start and at each transport re-check. Under `off`
    /// nothing is read.
    pub async fn read(&self, pc: &RTCPeerConnection) {
        if self.counters.mode == Mode::Off {
            return;
        }
        let class = class_now(pc).await;
        let was = CarrierClass::of_code(self.class.swap(class.code(), Relaxed));
        if was != class {
            info!(
                session = %self.session_id,
                from = was.word(),
                to = class.word(),
                "video channel: carrier class (FR-148 P4b-3)"
            );
        }
    }

    /// The send task's lane: `bytes` is the session's `video-bytes` stash,
    /// `framed` whether the session framed its chunks (an unframed stream on
    /// an unordered channel is garbage, so it never leaves `video-bytes`).
    pub fn lane(
        &self,
        bytes: &Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
        pc: &RTCPeerConnection,
        framed: bool,
    ) -> Lane<SessionWire> {
        let wire = SessionWire {
            bytes: Arc::clone(bytes),
            session: self.session.clone(),
            sctp: pc.sctp(),
            session_id: self.session_id,
        };
        Lane::new(
            wire,
            self.session_id,
            Arc::clone(&self.counters),
            Arc::clone(&self.class),
            framed,
        )
    }
}

/// What a lane needs from the session: its channels, their buffered amounts,
/// the mark, and the key frame gate.
pub trait Wire: Send + Sync {
    /// A channel to write on.
    type Dc: Clone + Send + Sync;
    /// What a frame's send holds while it writes.
    type Held: Send;

    /// `video-bytes` as today's send task took it: its stash, locked, and held
    /// through the frame's send; `None` when nothing is stashed.
    fn bytes(&self) -> impl Future<Output = (Self::Held, Option<Self::Dc>)> + Send;

    /// `video-deadline`, stashed and open.
    fn deadline(&self) -> impl Future<Output = Option<Self::Dc>> + Send;

    /// Bytes written on `dc` and not yet acknowledged or abandoned.
    fn buffered(&self, dc: &Self::Dc) -> impl Future<Output = usize> + Send;

    /// `dc`'s SCTP stream.
    fn stream(dc: &Self::Dc) -> u16;

    /// Marks `dc`'s stream deadline-limited; false when refused.
    fn mark(&self, dc: &Self::Dc) -> impl Future<Output = bool> + Send;

    /// One key frame ask through the session's gate: whether it passed.
    fn ask_keyframe(&self) -> bool;
}

/// A lane's wire in a live session.
pub struct SessionWire {
    bytes: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    session: Option<Arc<Session>>,
    sctp: Arc<RTCSctpTransport>,
    session_id: ObjectId,
}

impl Wire for SessionWire {
    type Dc = Arc<RTCDataChannel>;
    type Held = tokio::sync::OwnedMutexGuard<Option<Arc<RTCDataChannel>>>;

    async fn bytes(&self) -> (Self::Held, Option<Self::Dc>) {
        let held = Arc::clone(&self.bytes).lock_owned().await;
        let dc = held.clone();
        (held, dc)
    }

    async fn deadline(&self) -> Option<Self::Dc> {
        let dc = self.session.as_ref()?.deadline.lock().await.clone()?;
        (dc.ready_state() == RTCDataChannelState::Open).then_some(dc)
    }

    async fn buffered(&self, dc: &Self::Dc) -> usize {
        dc.buffered_amount().await
    }

    fn stream(dc: &Self::Dc) -> u16 {
        dc.id()
    }

    async fn mark(&self, dc: &Self::Dc) -> bool {
        self.sctp.mark_deadline_stream(dc.id()).await
    }

    fn ask_keyframe(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.gate.ask(self.session_id))
    }
}

/// A frame the lane let through: write it on `dc`, numbered `frame_seq`,
/// while this lives.
pub struct Routed<W: Wire> {
    pub dc: W::Dc,
    pub frame_seq: u32,
    _held: W::Held,
}

/// A switch under way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Switch {
    from: Channel,
    to: Channel,
    class: CarrierClass,
    /// When the pick said switch.
    since: Instant,
    /// From then until the old channel read drained.
    drained_ms: u64,
    /// Frames dropped since then.
    dropped: u32,
    /// Whether a key frame ask passed the gate since the drain.
    asked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Steady,
    /// Nothing more goes on the old channel: frames are dropped until it
    /// drained.
    Draining(Switch),
    /// Drained: frames are dropped until a key frame, which goes whole on the
    /// new channel.
    Keying(Switch),
}

/// A send task's road onto the session's video channels, with its one
/// `frame_seq` space.
pub struct Lane<W: Wire> {
    wire: W,
    session_id: ObjectId,
    counters: Arc<Counters>,
    class: Arc<AtomicU8>,
    framed: bool,
    frame_seq: u32,
    /// The channel frames go on (under `shadow`, `on`'s); `None` before the
    /// first pick.
    on: Option<Channel>,
    phase: Phase,
    /// The stream marked, once.
    marked: Option<u16>,
    /// A mark was refused: tried again at the next key frame.
    mark_refused: bool,
    said_refused: bool,
    said_none_open: bool,
}

impl<W: Wire> Lane<W> {
    pub fn new(
        wire: W,
        session_id: ObjectId,
        counters: Arc<Counters>,
        class: Arc<AtomicU8>,
        framed: bool,
    ) -> Self {
        Self {
            wire,
            session_id,
            counters,
            class,
            framed,
            frame_seq: 0,
            on: None,
            phase: Phase::Steady,
            marked: None,
            mark_refused: false,
            said_refused: false,
            said_none_open: false,
        }
    }

    /// A frame's road, decided once, before its first write: `Some`, write it
    /// whole on the channel and under the number given, while the `Routed`
    /// lives; `None`, write nothing. `wire` is the frame as
    /// `frame_video_bytes` framed it, `enqueued_at` its hand-off to the send
    /// task.
    pub async fn route(&mut self, wire: &[u8], enqueued_at: Instant) -> Option<Routed<W>> {
        let keyframe = crate::peer::video_bytes_is_keyframe(wire);
        self.route_at(keyframe, enqueued_at, Instant::now()).await
    }

    async fn route_at(
        &mut self,
        keyframe: bool,
        enqueued_at: Instant,
        now: Instant,
    ) -> Option<Routed<W>> {
        // Today's take: with no `video-bytes` stashed, no write and no number.
        let (held, bytes) = self.wire.bytes().await;
        let bytes = bytes?;
        let age = now.saturating_duration_since(enqueued_at);
        match self.counters.mode {
            Mode::Off => Some(self.write(bytes, held)),
            Mode::Shadow => {
                self.shadow(keyframe, age, now).await;
                Some(self.write(bytes, held))
            }
            Mode::On => self.on(keyframe, age, now, bytes, held).await,
        }
    }

    fn write(&mut self, dc: W::Dc, held: W::Held) -> Routed<W> {
        self.frame_seq = self.frame_seq.wrapping_add(1);
        Routed {
            dc,
            frame_seq: self.frame_seq,
            _held: held,
        }
    }

    fn class(&self) -> CarrierClass {
        CarrierClass::of_code(self.class.load(Relaxed))
    }

    async fn on(
        &mut self,
        keyframe: bool,
        age: Duration,
        now: Instant,
        bytes: W::Dc,
        held: W::Held,
    ) -> Option<Routed<W>> {
        let class = self.class();
        let deadline = match self.framed {
            true => self.wire.deadline().await,
            false => None,
        };
        self.say_none_open(class, deadline.is_some());
        let want = pick(class, deadline.is_some());
        if self.phase == Phase::Steady && self.on != Some(want) {
            // The mark first: refused, nothing switches.
            let marked = match (&deadline, want) {
                (Some(dc), Channel::Deadline) => self.mark(dc, keyframe).await,
                _ => true,
            };
            match self.on {
                // The first pick, before any frame went: no old channel, no
                // wait.
                None => {
                    let first = if marked { want } else { Channel::Bytes };
                    self.on = Some(first);
                    self.counters.set_channel(first);
                    self.say_first(first, class);
                }
                Some(from) if marked => {
                    self.phase = Phase::Draining(Switch {
                        from,
                        to: want,
                        class,
                        since: now,
                        drained_ms: 0,
                        dropped: 0,
                        asked: false,
                    });
                }
                Some(_) => {}
            }
        }
        match self.phase {
            Phase::Steady => match (self.on, deadline) {
                (Some(Channel::Deadline), Some(dc)) => {
                    if age > DEADLINE {
                        self.drop_late();
                        let _ = self.wire.ask_keyframe();
                        return None;
                    }
                    Some(self.write(dc, held))
                }
                _ => Some(self.write(bytes, held)),
            },
            Phase::Draining(mut s) => {
                let old = match s.from {
                    Channel::Bytes => Some(bytes.clone()),
                    Channel::Deadline => deadline.clone(),
                };
                let drained = match &old {
                    Some(dc) => self.wire.buffered(dc).await == 0,
                    // Closed: nothing more drains from it.
                    None => true,
                };
                if !drained {
                    return self.drop_switching();
                }
                s.drained_ms = ms(now.saturating_duration_since(s.since));
                self.phase = Phase::Keying(s);
                self.keying(keyframe, age, now, bytes, deadline, held)
            }
            Phase::Keying(_) => self.keying(keyframe, age, now, bytes, deadline, held),
        }
    }

    /// A drained switch's frame: dropped until a key frame, which goes whole
    /// on the new channel, or on `video-bytes` when `video-deadline` went while
    /// the switch waited.
    fn keying(
        &mut self,
        keyframe: bool,
        age: Duration,
        now: Instant,
        bytes: W::Dc,
        deadline: Option<W::Dc>,
        held: W::Held,
    ) -> Option<Routed<W>> {
        let Phase::Keying(mut s) = self.phase else {
            return None;
        };
        if !keyframe {
            if !s.asked {
                s.asked = self.wire.ask_keyframe();
                self.phase = Phase::Keying(s);
            }
            return self.drop_switching();
        }
        let (to, dc) = match (s.to, deadline) {
            (Channel::Deadline, Some(dc)) => (Channel::Deadline, dc),
            _ => (Channel::Bytes, bytes),
        };
        if to == Channel::Deadline && age > DEADLINE {
            // Too old for the new channel: dropped, and another asked.
            self.drop_late();
            s.asked = self.wire.ask_keyframe();
            self.phase = Phase::Keying(s);
            return None;
        }
        let out = self.write(dc, held);
        self.phase = Phase::Steady;
        self.on = Some(to);
        self.counters.set_channel(to);
        if to != s.from {
            self.counters.switches.fetch_add(1, Relaxed);
        }
        info!(
            session = %self.session_id,
            drained_ms = s.drained_ms,
            "video channel: {} -> {} (carrier={}, frame_seq={}, held_ms={}, dropped={})",
            s.from.label(),
            to.label(),
            s.class.word(),
            out.frame_seq,
            ms(now.saturating_duration_since(s.since)),
            s.dropped,
        );
        Some(out)
    }

    /// `on`'s choices, without its writes, marks, drops or asks: the channel
    /// it would be on, the switch it would make at a key frame, the frames D
    /// would drop there.
    async fn shadow(&mut self, keyframe: bool, age: Duration, now: Instant) {
        let class = self.class();
        let open = self.framed && self.wire.deadline().await.is_some();
        self.say_none_open(class, open);
        let want = pick(class, open);
        match self.on {
            None => {
                self.on = Some(want);
                self.counters.set_channel(want);
                self.say_first(want, class);
            }
            Some(from) if self.phase == Phase::Steady && want != from => {
                self.phase = Phase::Keying(Switch {
                    from,
                    to: want,
                    class,
                    since: now,
                    drained_ms: 0,
                    dropped: 0,
                    asked: true,
                });
            }
            Some(_) => {}
        }
        if let Phase::Keying(s) = self.phase
            && keyframe
        {
            self.phase = Phase::Steady;
            self.on = Some(s.to);
            self.counters.set_channel(s.to);
            self.counters.switches.fetch_add(1, Relaxed);
            info!(
                session = %self.session_id,
                "video channel (shadow): {} -> {} (carrier={}, frame_seq={}, held_ms={}, dropped=0)",
                s.from.label(),
                s.to.label(),
                s.class.word(),
                self.frame_seq.wrapping_add(1),
                ms(now.saturating_duration_since(s.since)),
            );
        }
        if self.on == Some(Channel::Deadline) && age > DEADLINE {
            self.counters.late.fetch_add(1, Relaxed);
        }
    }

    /// Under `on`, before the first write on `video-deadline`: its stream
    /// marked, once. A refusal is tried again at the next key frame.
    async fn mark(&mut self, dc: &W::Dc, keyframe: bool) -> bool {
        let stream = W::stream(dc);
        if self.marked == Some(stream) {
            return true;
        }
        if self.mark_refused && !keyframe {
            return false;
        }
        if self.wire.mark(dc).await {
            self.marked = Some(stream);
            self.mark_refused = false;
            return true;
        }
        self.mark_refused = true;
        if !self.said_refused {
            self.said_refused = true;
            warn!(
                session = %self.session_id,
                stream,
                "video channel: video-deadline's stream was not marked; staying on video-bytes, and trying again at the next key frame (FR-148 P4b-3)"
            );
        }
        false
    }

    /// A frame dropped for a switch: it consumes its number.
    fn drop_switching(&mut self) -> Option<Routed<W>> {
        self.frame_seq = self.frame_seq.wrapping_add(1);
        if let Phase::Draining(s) | Phase::Keying(s) = &mut self.phase {
            s.dropped = s.dropped.saturating_add(1);
        }
        self.counters.drained.fetch_add(1, Relaxed);
        None
    }

    /// A frame D dropped: it consumes its number.
    fn drop_late(&mut self) {
        self.frame_seq = self.frame_seq.wrapping_add(1);
        self.counters.late.fetch_add(1, Relaxed);
    }

    fn tag(&self) -> &'static str {
        match self.counters.mode {
            Mode::Shadow => " (shadow)",
            Mode::Off | Mode::On => "",
        }
    }

    fn say_first(&self, first: Channel, class: CarrierClass) {
        info!(
            session = %self.session_id,
            "video channel{}: none -> {} (carrier={}, frame_seq={}, held_ms=0, dropped=0)",
            self.tag(),
            first.label(),
            class.word(),
            self.frame_seq.wrapping_add(1),
        );
    }

    /// The carrier wants `video-deadline` and the session has none open (an
    /// older viewer, or an unframed session): one line says so.
    fn say_none_open(&mut self, class: CarrierClass, open: bool) {
        if open || !class.is_tcp() || self.said_none_open {
            return;
        }
        self.said_none_open = true;
        info!(
            session = %self.session_id,
            carrier = class.word(),
            framed = self.framed,
            "video channel{}: the carrier is TCP somewhere and no video-deadline is open; video-bytes (FR-148 P4b-3)",
            self.tag(),
        );
    }
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_carrier::CandidateKind;
    use CarrierClass as C;

    /// `video-bytes`' stream in the rig, and `video-deadline`'s.
    const BYTES: u16 = 1;
    const DL: u16 = 3;

    #[derive(Default)]
    struct State {
        bytes: Option<u16>,
        deadline: Option<u16>,
        buffered: HashMap<u16, usize>,
        mark_ok: bool,
        marks: Vec<u16>,
        ask_ok: bool,
        asks: u32,
    }

    /// A session's wire, scripted: its channels are their stream ids.
    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<State>>);

    impl Fake {
        fn new() -> Self {
            let fake = Fake::default();
            {
                let mut s = fake.st();
                s.bytes = Some(BYTES);
                s.deadline = Some(DL);
                s.mark_ok = true;
                s.ask_ok = true;
            }
            fake
        }

        fn st(&self) -> std::sync::MutexGuard<'_, State> {
            self.0.lock().unwrap()
        }
    }

    impl Wire for Fake {
        type Dc = u16;
        type Held = ();

        async fn bytes(&self) -> ((), Option<u16>) {
            ((), self.st().bytes)
        }

        async fn deadline(&self) -> Option<u16> {
            self.st().deadline
        }

        async fn buffered(&self, dc: &u16) -> usize {
            self.st().buffered.get(dc).copied().unwrap_or(0)
        }

        fn stream(dc: &u16) -> u16 {
            *dc
        }

        async fn mark(&self, dc: &u16) -> bool {
            let mut s = self.st();
            s.marks.push(*dc);
            s.mark_ok
        }

        fn ask_keyframe(&self) -> bool {
            let mut s = self.st();
            s.asks += 1;
            s.ask_ok
        }
    }

    fn sid() -> ObjectId {
        ObjectId::parse_str("65a1b2c3d4e5f60718293a4c").unwrap()
    }

    /// A lane on a scripted wire, its counts and its class, and a clock that
    /// moves 10 ms a frame.
    struct Rig {
        fake: Fake,
        lane: Lane<Fake>,
        counters: Arc<Counters>,
        class: Arc<AtomicU8>,
        t: Instant,
    }

    impl Rig {
        fn new(mode: Mode) -> Self {
            Self::with(mode, true)
        }

        fn with(mode: Mode, framed: bool) -> Self {
            let fake = Fake::new();
            let counters = Arc::new(Counters::new(mode));
            let class = Arc::new(AtomicU8::new(C::Unknown.code()));
            let lane = Lane::new(
                fake.clone(),
                sid(),
                Arc::clone(&counters),
                Arc::clone(&class),
                framed,
            );
            Self {
                fake,
                lane,
                counters,
                class,
                t: Instant::now() + Duration::from_secs(60),
            }
        }

        /// The class a pump's read stored.
        fn class(&self, class: CarrierClass) {
            self.class.store(class.code(), Relaxed);
        }

        /// One frame, `age_ms` old at its turn: its channel and number when
        /// written.
        fn frame(&mut self, keyframe: bool, age_ms: u64) -> Option<(u16, u32)> {
            self.t += Duration::from_millis(10);
            let enqueued = self.t - Duration::from_millis(age_ms);
            futures::executor::block_on(self.lane.route_at(keyframe, enqueued, self.t))
                .map(|r| (r.dc, r.frame_seq))
        }

        fn key(&mut self) -> Option<(u16, u32)> {
            self.frame(true, 0)
        }

        fn delta(&mut self) -> Option<(u16, u32)> {
            self.frame(false, 0)
        }
    }

    /// What `f` logged, beside what it returned.
    fn logged<T>(f: impl FnOnce() -> T) -> (T, String) {
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let writer = buf.clone();
        // Two dispatchers, never one: with one, a callsite's interest is taken
        // once for every thread.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let out = tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        (out, text)
    }

    /// FR-148 P4b-3 — the pick table: `video-deadline` on a carrier that is
    /// TCP somewhere (TURN over TCP or TLS, a relayed mesh path) when one is
    /// open, `video-bytes` on every other class, `unknown` included, and
    /// whenever none is open. Per switch value, the channel a session's first
    /// key frame goes on: `on`'s pick; `off`'s and `shadow`'s `video-bytes`,
    /// and `shadow`'s line names the channel `on` would be on.
    #[test]
    fn the_pick_takes_video_deadline_on_a_tcp_carrier_alone() {
        let tcp = [C::TurnTcp, C::TurnTls, C::MeshRelay];
        for class in C::ALL {
            assert_eq!(C::of_code(class.code()), class);
            assert_eq!(class.is_tcp(), tcp.contains(&class), "{class:?}");
            assert_eq!(pick(class, false), Channel::Bytes, "{class:?}, none open");
            let want = match tcp.contains(&class) {
                true => Channel::Deadline,
                false => Channel::Bytes,
            };
            assert_eq!(pick(class, true), want, "{class:?}");
            for mode in [Mode::Off, Mode::Shadow, Mode::On] {
                let mut rig = Rig::new(mode);
                rig.class(class);
                let on = match mode {
                    Mode::On => want,
                    Mode::Off | Mode::Shadow => Channel::Bytes,
                };
                let stream = match on {
                    Channel::Deadline => DL,
                    Channel::Bytes => BYTES,
                };
                assert_eq!(rig.key(), Some((stream, 1)), "{class:?} under {mode:?}");
                let line = rig.counters.take(None);
                match mode {
                    Mode::Off => assert_eq!(line, None),
                    Mode::Shadow | Mode::On => {
                        assert_eq!(line.unwrap().channel, Some(want), "{class:?} {mode:?}")
                    }
                }
            }
        }
        assert_eq!(C::of_code(200), C::Unknown);
        assert_eq!(pick(C::Unknown, true), Channel::Bytes);
    }

    /// FR-148 P4b-3 — the class, from a selected pair: the agent's own relay
    /// by what carries it (the carrier line's `relay_transport`); a mesh pair
    /// by the overlay's tier, any tier but `Direct` a relay (as
    /// `overlay_remote_is_relay_tier` reads it); `unknown` for the far end's
    /// relay, a relay with no stamp, an unspecified candidate, and a mesh pair
    /// the overlay said nothing about, never `direct` by default.
    #[test]
    fn the_class_is_read_from_the_selected_pair() {
        use CandidateKind::{Host, Prflx, Relay, Srflx, Unspecified};
        let pair = |local, remote, addr: &str| PairView {
            local_kind: local,
            remote_kind: remote,
            local_addr: "192.0.2.10".into(),
            remote_addr: addr.into(),
        };
        let (far, mesh) = ("198.51.100.7", "100.64.0.9");
        let tier = |kind: &str| Some(OverlayPath::Relay(Some(kind.into())));
        use RelayTransport as T;
        for (p, relay, rides, overlay, want) in [
            (pair(Host, Host, far), T::NoRelay, false, None, C::Direct),
            (pair(Srflx, Prflx, far), T::NoRelay, false, None, C::Direct),
            (pair(Relay, Host, far), T::Udp, false, None, C::TurnUdp),
            (pair(Relay, Srflx, far), T::Tcp, false, None, C::TurnTcp),
            (pair(Relay, Relay, far), T::Tls, false, None, C::TurnTls),
            (pair(Relay, Host, far), T::Unknown, false, None, C::Unknown),
            (pair(Host, Relay, far), T::NoRelay, false, None, C::Unknown),
            (
                pair(Unspecified, Host, far),
                T::Unknown,
                false,
                None,
                C::Unknown,
            ),
            (
                pair(Host, Host, mesh),
                T::NoRelay,
                true,
                Some(OverlayPath::Direct),
                C::MeshDirect,
            ),
            (
                pair(Host, Host, mesh),
                T::NoRelay,
                true,
                tier("derp"),
                C::MeshRelay,
            ),
            (
                pair(Host, Host, mesh),
                T::NoRelay,
                true,
                tier("turn"),
                C::MeshRelay,
            ),
            (
                pair(Host, Host, mesh),
                T::NoRelay,
                true,
                tier("org"),
                C::MeshRelay,
            ),
            (
                pair(Host, Host, mesh),
                T::NoRelay,
                true,
                Some(OverlayPath::Relay(None)),
                C::MeshRelay,
            ),
            (
                pair(Host, Host, mesh),
                T::NoRelay,
                true,
                Some(OverlayPath::Other("blocked".into())),
                C::MeshRelay,
            ),
            (pair(Host, Host, mesh), T::NoRelay, true, None, C::Unknown),
        ] {
            assert_eq!(
                class_of(&p, relay, rides, overlay.as_ref()),
                want,
                "{p:?} {relay:?} {rides} {overlay:?}"
            );
        }
    }

    /// FR-148 P4b-3 — never `detect_constrained_transport`'s 3 s default:
    /// with no pair selected the class read at a pump's start, and at each
    /// re-check, is `unknown`.
    #[tokio::test]
    async fn with_no_pair_selected_the_class_is_unknown() {
        let pc = webrtc::api::APIBuilder::new()
            .build()
            .new_peer_connection(Default::default())
            .await
            .expect("a peer connection");
        for mode in [Mode::Shadow, Mode::On] {
            let counters = Arc::new(Counters::new(mode));
            let pump = Pump::start(sid(), &pc, &counters).await;
            assert_eq!(C::of_code(pump.class.load(Relaxed)), C::Unknown);
            pump.class.store(C::TurnTcp.code(), Relaxed);
            pump.read(&pc).await;
            assert_eq!(C::of_code(pump.class.load(Relaxed)), C::Unknown, "re-read");
        }
        pc.close().await.expect("closed");
    }

    /// FR-148 P4b-3 — the default fired: the session starts on `video-bytes`
    /// with the class unknown, a `video-deadline` open or not, and the pick
    /// runs again once a re-check reads the selected pair: at the next frame
    /// the switch begins, and its first key frame goes on `video-deadline`.
    #[test]
    fn a_session_with_no_pair_starts_on_video_bytes_and_switches_at_the_re_check() {
        let mut rig = Rig::new(Mode::On);
        assert_eq!(rig.key(), Some((BYTES, 1)));
        for n in 2..=5 {
            assert_eq!(rig.delta(), Some((BYTES, n)));
        }
        rig.class(C::TurnTcp);
        assert_eq!(
            rig.delta(),
            None,
            "drained at once: dropped, a key frame asked"
        );
        assert_eq!(rig.fake.st().asks, 1);
        assert_eq!(rig.key(), Some((DL, 7)));
        assert_eq!(rig.delta(), Some((DL, 8)));
    }

    /// FR-148 P4b-3 — the drain rule. Once the pick says switch nothing more
    /// goes on the old channel; every frame, a key frame included, is dropped
    /// whole while its buffered amount is above 0, each consuming its
    /// number; then a key frame is asked through the gate, once it passes,
    /// frames are dropped until one comes, and it goes whole on the new
    /// channel. The switch line says how long the video was held. A refused
    /// ask is asked again at the next frame. The wait has no time bound.
    #[test]
    fn a_switch_drains_the_old_channel_then_waits_for_a_key_frame() {
        let mut rig = Rig::new(Mode::On);
        rig.class(C::Direct);
        assert_eq!(rig.key(), Some((BYTES, 1)));
        for n in 2..=5 {
            assert_eq!(rig.delta(), Some((BYTES, n)));
        }
        rig.fake.st().buffered.insert(BYTES, 50_000);
        rig.class(C::TurnTcp);
        let (wrote, out) = logged(|| {
            // 6, then 7: a key frame while `video-bytes` holds bytes, then 8.
            let mut wrote = vec![rig.delta(), rig.key(), rig.delta()];
            rig.fake.st().buffered.insert(BYTES, 0);
            wrote.extend([rig.delta(), rig.delta(), rig.key(), rig.delta()]);
            wrote
        });
        assert_eq!(
            wrote,
            [None, None, None, None, None, Some((DL, 11)), Some((DL, 12))]
        );
        assert_eq!(rig.fake.st().asks, 1, "one ask, after the drain");
        assert_eq!(
            rig.counters.take(None),
            Some(DeadlineLine {
                forward_tsns: None,
                late: 0,
                drained: 5,
                switches: 1,
                channel: Some(Channel::Deadline),
            })
        );
        let line = "video channel: video-bytes -> video-deadline \
                    (carrier=turn_tcp, frame_seq=11, held_ms=50, dropped=5)";
        assert_eq!(out.matches(line).count(), 1, "{out}");
        assert!(out.contains("drained_ms=30"), "{out}");

        // A refused ask is asked again, frame by frame, until one passes.
        let mut rig = Rig::new(Mode::On);
        rig.class(C::Direct);
        assert_eq!(rig.key(), Some((BYTES, 1)));
        rig.fake.st().ask_ok = false;
        rig.class(C::TurnTls);
        assert_eq!((rig.delta(), rig.delta()), (None, None));
        assert_eq!(rig.fake.st().asks, 2);
        rig.fake.st().ask_ok = true;
        assert_eq!((rig.delta(), rig.delta()), (None, None));
        assert_eq!(rig.fake.st().asks, 3, "none after one passed");
        assert_eq!(rig.key(), Some((DL, 6)));

        // No time bound: an old channel that never drains holds the video.
        let mut rig = Rig::new(Mode::On);
        rig.class(C::Direct);
        assert_eq!(rig.key(), Some((BYTES, 1)));
        rig.fake.st().buffered.insert(BYTES, 1);
        rig.class(C::MeshRelay);
        for i in 0..10_000 {
            assert_eq!(rig.frame(i % 30 == 0, 0), None, "frame {i}");
        }
        assert_eq!(rig.fake.st().asks, 0, "no ask before the drain");
        assert_eq!(rig.lane.frame_seq, 10_001);
    }

    /// FR-148 P4b-3 — one `frame_seq` space per send task, across a switch
    /// and back: every frame consumes one number, written or dropped, so the
    /// numbers written rise without a repeat and the gaps between them are
    /// the frames dropped, each counted (a switch's or D's).
    #[test]
    fn one_frame_seq_space_across_a_switch_and_back() {
        enum Do {
            Frame(bool, u64),
            Class(CarrierClass),
            Buffered(u16, usize),
        }
        use Do::{Buffered, Class, Frame};
        let script = [
            Class(C::Direct),
            Frame(true, 0),
            Frame(false, 0),
            Frame(false, 0),
            Class(C::TurnTcp),
            Buffered(BYTES, 9_000),
            Frame(false, 0),
            Frame(true, 0),
            Buffered(BYTES, 0),
            Frame(false, 0),
            Frame(true, 0),
            Frame(false, 0),
            Frame(false, 400),
            Frame(false, 0),
            Class(C::Direct),
            Buffered(DL, 3_000),
            Frame(true, 0),
            Frame(false, 0),
            Buffered(DL, 0),
            Frame(false, 0),
            Frame(false, 0),
            Frame(true, 0),
            Frame(false, 5_000),
            Frame(false, 0),
        ];
        let mut rig = Rig::new(Mode::On);
        let (mut frames, mut written, mut dropped) = (0_u32, Vec::new(), Vec::new());
        for step in script {
            match step {
                Class(c) => rig.class(c),
                Buffered(stream, n) => {
                    rig.fake.st().buffered.insert(stream, n);
                }
                Frame(key, age) => {
                    frames += 1;
                    match rig.frame(key, age) {
                        Some(w) => written.push(w),
                        None => dropped.push(frames),
                    }
                    assert_eq!(rig.lane.frame_seq, frames, "one number a frame");
                }
            }
        }
        let numbers: Vec<u32> = written.iter().map(|w| w.1).collect();
        let mut all: Vec<u32> = numbers.iter().chain(&dropped).copied().collect();
        all.sort_unstable();
        assert_eq!(
            all,
            (1..=frames).collect::<Vec<_>>(),
            "{written:?} {dropped:?}"
        );
        assert!(numbers.windows(2).all(|w| w[0] < w[1]), "{numbers:?}");
        assert_eq!(
            written,
            [
                (BYTES, 1),
                (BYTES, 2),
                (BYTES, 3),
                (DL, 7),
                (DL, 8),
                (DL, 10),
                (BYTES, 15),
                (BYTES, 16),
                (BYTES, 17),
            ]
        );
        let line = rig.counters.take(None).unwrap();
        assert_eq!(
            line.drained + line.late,
            dropped.len() as u64,
            "each drop counted"
        );
        assert_eq!((line.late, line.switches), (1, 2));
        assert_eq!(line.channel, Some(Channel::Bytes));
    }

    /// FR-148 P4b-3 — D, under `on`, on `video-deadline` only: a frame older
    /// than 300 ms at its turn, from its hand-off, is dropped whole (the road
    /// is decided once, before the first write, so a frame begun is
    /// finished), consumes its number, is counted, and a key frame is asked
    /// through the gate. A frame on `video-bytes` is never D's, under any
    /// value; `shadow` counts what `on` would drop and drops nothing.
    #[test]
    fn d_drops_a_late_frame_whole_on_video_deadline_alone() {
        let mut rig = Rig::new(Mode::On);
        rig.class(C::TurnTcp);
        assert_eq!(rig.key(), Some((DL, 1)));
        assert_eq!(rig.frame(false, 300), Some((DL, 2)), "D old is not older");
        assert_eq!(rig.frame(false, 301), None, "older than D");
        assert_eq!(rig.lane.frame_seq, 3, "its number consumed");
        assert_eq!(rig.fake.st().asks, 1, "a key frame asked");
        assert_eq!(rig.frame(false, 10), Some((DL, 4)));
        let line = rig.counters.take(None).unwrap();
        assert_eq!((line.late, line.drained, line.switches), (1, 0, 0));
        assert_eq!(DEADLINE, Duration::from_millis(300));

        for (mode, class) in [
            (Mode::On, C::TurnUdp),
            (Mode::On, C::Direct),
            (Mode::Off, C::TurnTcp),
            (Mode::Shadow, C::TurnTcp),
        ] {
            let mut rig = Rig::new(mode);
            rig.class(class);
            assert_eq!(rig.key(), Some((BYTES, 1)), "{mode:?} {class:?}");
            assert_eq!(rig.frame(false, 30_000), Some((BYTES, 2)), "{mode:?}");
            assert_eq!(rig.fake.st().asks, 0, "{mode:?}");
            let late = rig.counters.take(None).map(|l| l.late);
            let want = match (mode, class.is_tcp()) {
                (Mode::Off, _) => None,
                (Mode::Shadow, true) => Some(1),
                _ => Some(0),
            };
            assert_eq!(late, want, "{mode:?} {class:?}");
        }
    }

    /// FR-148 P4b-3 — the mark: under `on` only, once per stream, before the
    /// first write on `video-deadline`, however often the session switches.
    /// Refused, nothing switches: the session stays on `video-bytes`, one
    /// line says so, and the mark is tried again at the next key frame.
    #[test]
    fn the_mark_comes_once_under_on_before_the_first_write_on_video_deadline() {
        let mut rig = Rig::new(Mode::On);
        rig.class(C::TurnTcp);
        let fake = rig.fake.clone();
        let mut lane_marks = Vec::new();
        // The fake answers `mark` before the lane hands the frame back.
        assert_eq!(rig.key(), Some((DL, 1)));
        lane_marks.append(&mut fake.st().marks);
        assert_eq!(lane_marks, [DL]);
        for _ in 0..20 {
            assert!(rig.delta().is_some());
        }
        rig.class(C::Direct);
        assert_eq!((rig.delta(), rig.key()), (None, Some((BYTES, 23))));
        rig.class(C::MeshRelay);
        assert_eq!((rig.delta(), rig.key()), (None, Some((DL, 25))));
        assert!(fake.st().marks.is_empty(), "never a second mark");

        for mode in [Mode::Off, Mode::Shadow] {
            let mut rig = Rig::new(mode);
            rig.class(C::TurnTcp);
            for i in 0..50 {
                rig.frame(i % 10 == 0, 0);
            }
            assert!(rig.fake.st().marks.is_empty(), "{mode:?}");
        }

        let mut rig = Rig::new(Mode::On);
        rig.class(C::TurnTcp);
        rig.fake.st().mark_ok = false;
        let (wrote, out) = logged(|| {
            let mut wrote = vec![rig.key()];
            wrote.extend((0..5).map(|_| rig.delta()));
            wrote.push(rig.key());
            wrote
        });
        let today: Vec<_> = (1..=7).map(|n| Some((BYTES, n))).collect();
        assert_eq!(wrote, today, "no switch");
        assert_eq!(
            rig.fake.st().marks,
            [DL, DL],
            "the first frame, the next key frame"
        );
        assert_eq!(out.matches("was not marked").count(), 1, "{out}");
        assert_eq!(rig.counters.take(None).unwrap().drained, 0);
        rig.fake.st().mark_ok = true;
        assert_eq!(rig.delta(), Some((BYTES, 8)), "not between key frames");
        assert_eq!(rig.key(), Some((DL, 9)));
        assert_eq!(rig.fake.st().marks, [DL, DL, DL]);
    }

    /// FR-148 P4b-3 — `shadow` writes exactly `off`'s bytes, and `off` today's:
    /// over five seeds of random frames, key frames, ages, carrier changes,
    /// a `video-deadline` that comes and goes, buffered amounts and a
    /// `video-bytes` that is sometimes not there, each lane's messages, as the
    /// send task frames them, equal today's send task's (`video-bytes`, the
    /// next number, while a channel is stashed), in channel, bytes and order.
    /// Neither marks or asks; `shadow`'s counts say it ran.
    #[test]
    fn shadow_writes_exactly_off_s_bytes() {
        const CHUNK: usize = 16 * 1024;
        fn messages(routed: Option<(u16, u32)>, wire: &[u8]) -> Option<(u16, Vec<Vec<u8>>)> {
            let (dc, seq) = routed?;
            let count = wire.len().div_ceil(CHUNK).max(1) as u16;
            let framed = wire.chunks(CHUNK).enumerate();
            let framed = framed.map(|(i, c)| crate::peer::chunk_framed(seq, i as u16, count, c));
            Some((dc, framed.collect()))
        }
        let (mut switches, mut late) = (0, 0);
        for seed in [1_u64, 2, 3, 0x0d55, 0x5eed_f00d] {
            let mut state = seed;
            let mut next = move || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            let fake = Fake::new();
            let class = Arc::new(AtomicU8::new(C::Unknown.code()));
            let lane = |mode| {
                let counters = Arc::new(Counters::new(mode));
                let lane = Lane::new(
                    fake.clone(),
                    sid(),
                    Arc::clone(&counters),
                    class.clone(),
                    true,
                );
                (lane, counters)
            };
            let ((mut off, _), (mut shadow, counts)) = (lane(Mode::Off), lane(Mode::Shadow));
            let mut today = 0_u32;
            let t0 = Instant::now() + Duration::from_secs(60);
            for i in 0..3_000_u64 {
                let r = next();
                if r % 97 == 0 {
                    class.store(C::ALL[(r >> 8) as usize % C::ALL.len()].code(), Relaxed);
                }
                let stashed = {
                    let mut s = fake.st();
                    if (r >> 12) % 300 == 0 {
                        s.deadline = s.deadline.xor(Some(DL));
                    }
                    s.bytes = ((r >> 20) % 150 != 0).then_some(BYTES);
                    s.buffered.insert(BYTES, ((r >> 28) % 3) as usize * 1_000);
                    s.buffered.insert(DL, ((r >> 36) % 2) as usize * 500);
                    s.bytes.is_some()
                };
                let keyframe = (r >> 44) % 30 == 0;
                let size = match (r >> 50) % 10 {
                    0 => (r >> 16) as usize % 40_000,
                    _ => (r >> 16) as usize % 2_000,
                };
                let payload: Vec<u8> = (0..size).map(|b| (b as u64 ^ i) as u8).collect();
                let wire = crate::peer::frame_video_bytes(&payload, keyframe, i);
                let now = t0 + Duration::from_millis(i * 10);
                let enqueued = now - Duration::from_millis((r >> 54) % 1_500);
                let key = crate::peer::video_bytes_is_keyframe(&wire);
                let todays = stashed.then(|| {
                    today += 1;
                    (BYTES, today)
                });
                let run = |lane: &mut Lane<Fake>| {
                    futures::executor::block_on(lane.route_at(key, enqueued, now))
                        .map(|r| (r.dc, r.frame_seq))
                };
                let want = messages(todays, &wire);
                assert_eq!(
                    messages(run(&mut off), &wire),
                    want,
                    "seed {seed} frame {i}: off"
                );
                assert_eq!(
                    messages(run(&mut shadow), &wire),
                    want,
                    "seed {seed} frame {i}"
                );
            }
            assert!(fake.st().marks.is_empty(), "seed {seed}: no mark");
            assert_eq!(fake.st().asks, 0, "seed {seed}: no ask");
            let line = counts.take(None).unwrap();
            assert_eq!(line.drained, 0, "seed {seed}");
            (switches, late) = (switches + line.switches, late + line.late);
        }
        assert!(switches > 0 && late > 0, "{switches} {late}: shadow ran");
    }

    /// FR-148 P4b-3 — mixed versions (§3.7): an older viewer opens no
    /// `video-deadline`, and an unframed session never uses one; under `on`
    /// on a TCP carrier the pick finds nothing and the session gets today's
    /// writes, `video-bytes` and the next number every frame, late or not,
    /// with one line that says so.
    #[test]
    fn an_older_viewer_gets_today_s_writes_under_on() {
        for (what, framed, deadline) in [
            ("no video-deadline", true, None),
            ("an unframed session", false, Some(DL)),
        ] {
            let mut rig = Rig::with(Mode::On, framed);
            rig.fake.st().deadline = deadline;
            rig.class(C::TurnTcp);
            let (wrote, out) = logged(|| {
                (0..200_u64)
                    .map(|i| rig.frame(i % 30 == 0, (i % 7) * 100))
                    .collect::<Vec<_>>()
            });
            let today: Vec<_> = (1..=200).map(|n| Some((BYTES, n))).collect();
            assert_eq!(wrote, today, "{what}");
            assert_eq!(
                out.matches("no video-deadline is open").count(),
                1,
                "{what}: {out}"
            );
            assert!(rig.fake.st().marks.is_empty(), "{what}");
            assert_eq!(rig.fake.st().asks, 0, "{what}");
        }
    }

    /// FR-148 P4b-3 — the session's gate, which the viewer's `rc:keyframe`
    /// and the send tasks' asks share: one ask passes per [`MIN_KF_GAP`], it
    /// sets the flag the pumps poll, and every ask is counted, with those the
    /// gap dropped (FR-135 R0, N12).
    #[test]
    fn the_gate_passes_one_ask_per_gap() {
        let flag = Arc::new(AtomicBool::new(false));
        let gate = KeyframeGate::new(Arc::clone(&flag));
        let (id, t0) = (ObjectId::new(), Instant::now());
        assert!(gate.ask_at(id, t0));
        assert!(flag.swap(false, Relaxed));
        assert!(!gate.ask_at(id, t0 + Duration::from_millis(499)));
        assert!(!flag.load(Relaxed));
        assert!(gate.ask_at(id, t0 + MIN_KF_GAP));
        assert!(flag.load(Relaxed));
        assert_eq!(
            crate::session_telemetry::counters(id).keyframe_asks(),
            (3, 1)
        );
        let session = Session::register(id, Arc::clone(&flag));
        assert!(Arc::ptr_eq(
            &Session::of(id).unwrap().gate(),
            &session.gate()
        ));
        drop(session);
        assert!(Session::of(id).is_none(), "found while its peer holds it");
    }

    /// FR-148 P4b-3 — in a live session the lane holds `video-bytes`' stash
    /// through the frame's send, as the send task's `if let` did (FR-65's
    /// `dc_lock`: the pump's own take of the stash waits out a frame's send),
    /// under every value; and a send task can own it.
    #[tokio::test]
    async fn the_lane_holds_video_bytes_stash_through_the_send() {
        let pc = Arc::new(
            webrtc::api::APIBuilder::new()
                .build()
                .new_peer_connection(Default::default())
                .await
                .expect("a peer connection"),
        );
        let dc = pc
            .create_data_channel(BYTES_LABEL, None)
            .await
            .expect("a data channel");
        let video_bytes_dc = Arc::new(tokio::sync::Mutex::new(Some(dc)));
        let wire = bytes::Bytes::from(crate::peer::frame_video_bytes(b"frame", true, 1));
        for mode in [Mode::Off, Mode::Shadow, Mode::On] {
            let lanes = Pump::start(sid(), &pc, &Arc::new(Counters::new(mode))).await;
            let mut lane = lanes.lane(&video_bytes_dc, &pc, true);
            let out = lane.route(&wire, Instant::now()).await.expect("written");
            assert_eq!(out.frame_seq, 1, "{mode:?}");
            assert_eq!(out.dc.label(), BYTES_LABEL);
            assert!(video_bytes_dc.try_lock().is_err(), "{mode:?}: held");
            drop(out);
            assert!(video_bytes_dc.try_lock().is_ok(), "{mode:?}: let go");
            let wire = wire.clone();
            let task = tokio::spawn(async move {
                lane.route(&wire, Instant::now()).await.map(|o| o.frame_seq)
            });
            assert_eq!(task.await.unwrap(), Some(2), "{mode:?}");
        }
        pc.close().await.expect("closed");
    }

    /// `peer.rs` as compiled, with `\n` line ends.
    fn peer_rs() -> String {
        include_str!("peer.rs").replace("\r\n", "\n")
    }

    /// The fn that starts with `head` in `src`, to its closing brace at
    /// column 0.
    fn body_of(src: &str, head: &str) -> String {
        assert_eq!(src.matches(head).count(), 1, "one {head:?} in peer.rs");
        let rest = &src[src.find(head).unwrap()..];
        rest[..rest.find("\n}\n").expect("the fn's end")].to_owned()
    }

    /// FR-148 P4b-3 — `video-deadline` has an arm of its own in the data
    /// channel match, by equality, after `video-bytes`' and before the
    /// catch-all, which stashes it for the send tasks and logs its negotiated
    /// shape and its stream.
    #[test]
    fn the_video_deadline_channel_has_an_arm_of_its_own() {
        let src = peer_rs();
        assert_eq!(
            (BYTES_LABEL, DEADLINE_LABEL),
            ("video-bytes", "video-deadline")
        );
        let arm = "\"video-deadline\" => {";
        assert_eq!(src.matches(arm).count(), 1);
        let at = src.find(arm).unwrap();
        assert!(src.find("\"video-bytes\" => {").unwrap() < at);
        let rest = &src[at..];
        let body = &rest[..rest
            .find("_ => attach_log_only(dc, session_id),")
            .expect("the catch-all after it")];
        for want in [
            "ordered = dc.ordered(),",
            "max_retransmits = ?dc.max_retransmits(),",
            "stream_id = dc.id(),",
            "deadline_stash.stash(dc.clone()).await;",
            "attach_log_only(dc, session_id);",
        ] {
            assert!(body.contains(want), "{want}");
        }
        assert!(!src.contains("starts_with(\"video-"), "never by prefix");
    }

    /// FR-148 P4b-3 — both send tasks take every frame's road from their lane,
    /// once, before its first write: the chunk loop writes on the lane's
    /// channel under the lane's number, and no send task keeps a number of its
    /// own. Each pump starts the lane's class after
    /// `detect_constrained_transport` and reads it again at each re-check,
    /// ahead of the env override's arm, so it is read under every override.
    #[test]
    fn both_send_tasks_take_every_frame_s_road_from_their_lane() {
        let src = peer_rs();
        for (name, frame) in [
            (
                "async fn media_pump_vp9_444_dc(",
                "while let Some((enqueued_at, wire)) = next.take() {",
            ),
            (
                "async fn media_pump_ffmpeg_dc(",
                "while let Some((epoch, enqueued_at, wire)) = next.take() {",
            ),
        ] {
            let body = body_of(&src, name);
            for (call, n) in [
                (
                    "crate::media_deadline::Pump::start(session_id, &pc, delivery.deadline()).await",
                    1,
                ),
                ("lanes.lane(&video_bytes_dc, &pc, chunk_framing)", 1),
                ("lane.route(&wire, enqueued_at).await", 1),
                ("lanes.read(&pc).await", 1),
                ("frame_seq.wrapping_add(", 0),
                ("let mut frame_seq", 0),
            ] {
                assert_eq!(body.matches(call).count(), n, "{name} {call}");
            }
            let detect = body.find("detect_constrained_transport(&pc, session_id).await");
            let start = body.find("Pump::start(").unwrap();
            assert!(detect.unwrap() < start && start < body.find("lanes.lane(").unwrap());
            let task = &body[body.find(frame).unwrap()..];
            let chunks = task.find("while off < total {").unwrap();
            assert!(task.find("lane.route(").unwrap() < chunks, "{name}");
            let sends = &task[chunks..task.find("if ok {").unwrap()];
            assert_eq!(sends.matches("out.dc.send(").count(), 2, "{name}");
            assert_eq!(sends.matches("out.frame_seq,").count(), 1, "{name}");
            assert!(!sends.contains("video_bytes_dc"), "{name}");
            let recheck = "if last_transport_check.elapsed() >= TRANSPORT_RECHECK_INTERVAL {";
            let recheck = &body[body.find(recheck).unwrap()..];
            let read = recheck.find("lanes.read(&pc).await").unwrap();
            let arm = recheck.find("if !crate::encode::transport_is_constrained() {");
            assert!(read < arm.unwrap(), "{name}: read under every override");
        }
    }

    /// FR-148 P4b-3 — the viewer's `rc:keyframe` and the send tasks ask
    /// through one gate: the session's, which `AgentPeer::new` registers over
    /// the flag the pumps poll and hands the control channel's handler.
    #[test]
    fn the_control_channel_asks_through_the_session_s_gate() {
        let src = peer_rs();
        let handler = body_of(&src, "fn attach_control_handler(");
        let arm = &handler[handler.find("\"rc:keyframe\" => {").unwrap()..];
        let arm = &arm[..arm.find("\"rc:decode-error\" => {").unwrap()];
        assert!(arm.contains("if keyframe_gate.ask(session_id) {"), "{arm}");
        for gone in [
            "MIN_KF_GAP: Duration",
            "last_kf_request",
            "keyframe_requested",
        ] {
            assert!(!handler.contains(gone), "{gone}");
        }
        for (wiring, n) in [
            (
                "crate::media_deadline::Session::register(session_id, keyframe_requested.clone())",
                1,
            ),
            ("let keyframe_for_dc = deadline_session.gate();", 1),
            ("_deadline_session: deadline_session,", 1),
        ] {
            assert_eq!(src.matches(wiring).count(), n, "{wiring}");
        }
    }
}

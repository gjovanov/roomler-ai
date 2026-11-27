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
//! | the pick ([`pick`]) | `video-deadline` when the carrier is TCP somewhere ([`TcpEnd`]) and the viewer opened one on a framed session; `video-bytes` otherwise, and whenever neither end's carrier is known |
//! | TCP somewhere ([`TcpEnd::of`]) | the agent's own class is TCP ([`CarrierClass::is_tcp`]: TURN over TCP or TLS, by the carrier line's `relay_transport`, or a mesh path whose overlay tier is not `Direct`), or, on a pair selected off the mesh, the viewer's newest report of its own relay is `tcp` or `tls` (FR-148 P4b-3b, D58: `rc:decodestat`'s `relay`, kept in [`ViewerRelayReport`]). On the mesh the report is ignored: the browser's label for its candidate there is no read of the carrier |
//! | the class ([`Pump::read`]) | read from the selected pair, at the pump's start and at each 5 s transport re-check, never from `detect_constrained_transport`'s 3 s default: with no pair selected it is `unknown`, so the session starts on `video-bytes` and the re-check picks again once a pair is selected. The viewer's report is read with it |
//! | the switch | at a key frame, over one `frame_seq` space: a frame dropped for the switch or for D consumes its number too |
//! | the drain rule | once the pick says switch, nothing more is written on the old channel, and every frame is dropped whole until its buffered amount reads 0 (acknowledged, or abandoned and forwarded); then a key frame is asked through the session's gate ([`KeyframeGate`]), at each frame until an ask passes its 500 ms gap (a switch's own key frame is not recovery, so the key frame rule never spaces it: [`LaneAsk::Switch`]), frames are dropped until one comes, and it goes whole on the new channel. The wait has no time bound: the switch line says how long it held the video |
//! | the deadline D ([`DEADLINE`]) | under `on`, on `video-deadline` only: a frame older than D at its first write, counted from its hand-off to the send task, is dropped whole, and a key frame is asked through the gate; a frame begun is finished |
//! | the mark | under `on`, once per stream, before the first write on `video-deadline`: `RTCSctpTransport::mark_deadline_stream` (FR-148 P4b-1), on the channel's id, which the vendored webrtc's accept path sets to its stream's (FR-148 P4b). Refused (no association, the slot held, no such stream), nothing switches, a line names each reason once until a mark is accepted, and the mark is tried again at the next frame, key or delta, until one is accepted; accepted between key frames, the switch asks its own key frame |
//! | the key frame rule ([`KeyframeGate`], FR-148 P4b-4) | under `on`, while the lane is on `video-deadline` or switching to it, a recovery key frame (D's ask, the viewer's `rc:keyframe`) passes only once [`kf_gap`] has passed since the session's last key frame written (or since the gate's last grant, if later): `max(MIN_KF_GAP, 8 × kf_bytes / (KF_SHARE × rate))`, `kf_bytes` in bytes and `rate` in bit/s. A refused recovery ask is owed, never forgotten: the first frame after the gap opens grants it (after rc.130's gap alone once the lane has left `video-deadline`), and a key frame written answers it. A switch's own asks (FR-148 P4b-4b), and every ask elsewhere, get rc.130's 500 ms gap, grant for grant |
//! | `shadow` | the pick and D run on the channel `on` would be on, and the lines and counts say what `on` would do, the key frame rule's included; every write and every grant is `off`'s, byte for byte |
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
//! frame_seq=<n>, held_ms=<n>, dropped=<n>, tcp_end=<end>)` at each switch
//! (`none` as `<from>` at a pump's first pick), with `(shadow)` after
//! `channel` for the switch `on` would make; one line when the carrier wants
//! `video-deadline` and the session has none open; one when its stream's mark
//! is refused, once per reason, and one when it is marked after refusals; and
//! the class line, with `viewer_relay` and `tcp_end`, when a read changes the
//! class, the report or the end that holds the TCP leg. `carrier` is the
//! agent's own class. The counts go on the `path delivery` line (FR-148 P0)
//! under `shadow` and `on`, and after them FR-148 P4b-4's: the session's key
//! frame asks over the line by who asked, the refusals by the 500 ms gap and
//! by the rule, the owed asks granted, and the rule's gap at the line.
//!
//! FR-148 P4b-5a — the key frame ledger closes that line, from the pump's
//! start, under `shadow` and `on`: the key frames written whole by channel
//! (the send task tells the lane, [`Lane::written`]), the key frames the lane
//! dropped by D and by a switch, and the viewer's newest count of the key
//! frames it completed whole (`rc:decodestat`'s `kf`, kept in
//! [`ViewerRelayReport`]); O20 reads the two against each other. And the
//! first write after a still stretch, a second or more with no lane write
//! from the end of the last write's sends, writes [`WAKE_LINE`] with the
//! gauges a look reads then, and the first key frame
//! after a wake on a delta [`WAKE_KEY_LINE`] (D60). Nothing they count or read
//! changes a pick, a drop, an ask or a write.

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
use webrtc::sctp::association::gauges::{GaugeReader, Gauges};
use webrtc::sctp_transport::{DeadlineMarkRefusal, RTCSctpTransport};

use crate::session_carrier::{OverlayPath, PairView, RelayTransport, SessionCarrier};
use crate::session_telemetry::{KeyframeAsker, KeyframeRefusal, KeyframeSplit, SessionCounters};

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

    /// FR-148 P4b-5a — its place in a pair of counts: `video-bytes` first.
    fn index(self) -> usize {
        match self {
            Channel::Bytes => 0,
            Channel::Deadline => 1,
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

/// FR-148 P4b-3b (D58) — what carries the viewer's own relay, as its
/// `rc:decodestat` reports it in `relay`: the `relayProtocol` of the local
/// candidate of the pair the browser selected, when that candidate is a relay.
/// The agent cannot read the far end's leg itself ([`class_of`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewerRelay {
    Udp,
    Tcp,
    Tls,
}

impl ViewerRelay {
    /// `relay` on one `rc:decodestat`, by exact equality with `udp`, `tcp` or
    /// `tls`. Anything else, a non-string or an absent key included, is no
    /// report.
    pub fn of_decodestat(val: &serde_json::Value) -> Option<Self> {
        match val.get("relay").and_then(serde_json::Value::as_str) {
            Some("udp") => Some(ViewerRelay::Udp),
            Some("tcp") => Some(ViewerRelay::Tcp),
            Some("tls") => Some(ViewerRelay::Tls),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            ViewerRelay::Udp => "udp",
            ViewerRelay::Tcp => "tcp",
            ViewerRelay::Tls => "tls",
        }
    }

    /// TCP, or TLS over TCP: the viewer's leg to its relay is TCP.
    pub fn is_tcp(self) -> bool {
        matches!(self, ViewerRelay::Tcp | ViewerRelay::Tls)
    }

    /// A report's word in the class line, `none` without one.
    fn word_of(report: Option<Self>) -> &'static str {
        report.map_or("none", Self::word)
    }

    fn code(report: Option<Self>) -> u8 {
        match report {
            None => 0,
            Some(ViewerRelay::Udp) => 1,
            Some(ViewerRelay::Tcp) => 2,
            Some(ViewerRelay::Tls) => 3,
        }
    }

    fn of_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(ViewerRelay::Udp),
            2 => Some(ViewerRelay::Tcp),
            3 => Some(ViewerRelay::Tls),
            _ => None,
        }
    }
}

/// FR-148 P4b-3b (D58) — the end of the selected pair that holds a TCP leg:
/// what the pick reads, and the lines' `tcp_end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpEnd {
    None,
    Agent,
    Viewer,
    Both,
}

impl TcpEnd {
    /// The agent's own class, and the viewer's newest report where it counts:
    /// on a pair selected off the mesh (`off_mesh`). On the mesh the agent's
    /// own read decides and the report is ignored, because the browser names
    /// its local candidate there `prflx` (FR-148 §15, M0R).
    pub fn of(class: CarrierClass, off_mesh: bool, viewer: Option<ViewerRelay>) -> Self {
        Self::of_legs(
            class.is_tcp(),
            off_mesh && viewer.is_some_and(ViewerRelay::is_tcp),
        )
    }

    fn of_legs(agent: bool, viewer: bool) -> Self {
        match (agent, viewer) {
            (false, false) => TcpEnd::None,
            (true, false) => TcpEnd::Agent,
            (false, true) => TcpEnd::Viewer,
            (true, true) => TcpEnd::Both,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            TcpEnd::None => "none",
            TcpEnd::Agent => "agent",
            TcpEnd::Viewer => "viewer",
            TcpEnd::Both => "both",
        }
    }

    /// TCP somewhere on the pair: `video-deadline`'s carriers.
    pub fn is_tcp(self) -> bool {
        self != TcpEnd::None
    }
}

/// The agent's own class alone, as a viewer that sends no report leaves it:
/// P4b-3's pick.
impl From<CarrierClass> for TcpEnd {
    fn from(class: CarrierClass) -> Self {
        Self::of_legs(class.is_tcp(), false)
    }
}

/// FR-148 P4b-3b — set in a pump's read, beside the agent's own class, when
/// the viewer's report names a TCP leg that counts ([`TcpEnd::of`]), so the
/// send task's lane takes both from one load.
const VIEWER_TCP: u8 = 0x80;

/// A pump's read as its lane takes it: the agent's own class, and the end
/// that holds the TCP leg.
fn read_code(class: CarrierClass, end: TcpEnd) -> u8 {
    match end {
        TcpEnd::Viewer | TcpEnd::Both => class.code() | VIEWER_TCP,
        TcpEnd::None | TcpEnd::Agent => class.code(),
    }
}

fn read_of(code: u8) -> (CarrierClass, TcpEnd) {
    let class = CarrierClass::of_code(code & !VIEWER_TCP);
    (
        class,
        TcpEnd::of_legs(class.is_tcp(), code & VIEWER_TCP != 0),
    )
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

/// The pick: `video-deadline` on a carrier that is TCP somewhere, at either
/// end of the pair (FR-148 P4b-3b), when one is open; `video-bytes`
/// otherwise. A class alone is the agent's own end, as P4b-3 picked.
pub fn pick(end: impl Into<TcpEnd>, deadline_open: bool) -> Channel {
    if deadline_open && end.into().is_tcp() {
        Channel::Deadline
    } else {
        Channel::Bytes
    }
}

/// The class of the pair selected now, and whether it is off the mesh (FR-148
/// P4b-3b: where the viewer's report counts): `unknown` and not, while none
/// is.
async fn class_now(pc: &RTCPeerConnection) -> (CarrierClass, bool) {
    let Some((pair, relay, rides)) =
        crate::session_carrier::nominated_pair(pc, Duration::ZERO).await
    else {
        return (CarrierClass::Unknown, false);
    };
    // As the carrier line reads it: the ICE agent's read under `route`, the
    // remote's range otherwise.
    let rides = rides.unwrap_or_else(|| crate::peer::addr_is_overlay_range(&pair.remote_addr));
    let overlay = if rides {
        crate::session_carrier::overlay_path(&pair.remote_addr).await
    } else {
        None
    };
    (class_of(&pair, relay, rides, overlay.as_ref()), !rides)
}

/// rc.130, rc.217 — the least gap between two key frames a session forces on
/// request.
pub const MIN_KF_GAP: Duration = Duration::from_millis(500);

/// FR-148 P4b-4 (D55's Recovery, O6) — the share of the path's rate a
/// session's forced key frames may take on `video-deadline`: recovery costs at
/// most a quarter of the path, and AC24's bound is this rule's rate.
pub const KF_SHARE: f64 = 0.25;

/// FR-148 P4b-4 (D55) — the least time from a session's last key frame to a
/// forced recovery one under the rule: `max(MIN_KF_GAP, 8 × kf_bytes /
/// (KF_SHARE × rate))`, with `kf_bytes` the size of the last key frame
/// written, in bytes, and `rate` the path's, in bit/s ([`kf_rate`]). Before
/// the first key frame, or with no rate, [`MIN_KF_GAP`] alone.
pub fn kf_gap(kf_bytes: Option<u64>, rate_bps: Option<u64>) -> Duration {
    let cost = match (kf_bytes, rate_bps) {
        (Some(bytes), Some(rate)) if rate > 0 => {
            let secs = bytes as f64 * 8.0 / (KF_SHARE * rate as f64);
            Duration::try_from_secs_f64(secs).unwrap_or(Duration::MAX)
        }
        _ => Duration::ZERO,
    };
    cost.max(MIN_KF_GAP)
}

/// FR-148 P4b-4 (D59) — the rate [`kf_gap`] divides by, bit/s: the in-flight
/// budget's when this association's own figures gave it
/// (`media_inflight::Budget::rate_measured`), the encoder's current target
/// otherwise, and `None` with neither.
pub fn kf_rate(measured_bps: Option<u64>, target_bps: Option<u32>) -> Option<u64> {
    measured_bps
        .filter(|&r| r > 0)
        .or_else(|| target_bps.filter(|&t| t > 0).map(u64::from))
}

/// FR-148 P4b-4 (D59) — where a session's key frame rule stands, as its lane
/// last said: the switch's value as the pump read it, and whether the lane is
/// on `video-deadline` or switching to it (under `shadow`, whether `on` would
/// be).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    pub mode: Mode,
    pub deadline: bool,
}

impl Scope {
    /// Under `off`, and before any lane has said anything: rc.130's gate.
    pub const TODAY: Scope = Scope {
        mode: Mode::Off,
        deadline: false,
    };

    /// The rule acts on the session's recovery asks, D's and the viewer's:
    /// `on`, on `video-deadline` or switching to it. A switch's own ask is
    /// never the rule's ([`LaneAsk::Switch`]).
    fn acts(self) -> bool {
        self.mode == Mode::On && self.deadline
    }

    /// Under `shadow`, the rule would act: `on`'s lane would be on
    /// `video-deadline` or switching to it.
    fn shadows(self) -> bool {
        self.mode == Mode::Shadow && self.deadline
    }
}

/// FR-148 P4b-4b — why a session's lane asks for a key frame, which decides
/// the gap that spaces it. D55's rule spaces recovery key frames, and a
/// switch's own key frame is not one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneAsk {
    /// D dropped a frame on `video-deadline`, in `Steady`: recovery, under the
    /// key frame rule while it acts, as the viewer's `rc:keyframe` is.
    Late,
    /// A switch waits for the key frame it switches at: its `Keying` phase's
    /// ask, and its ask again after a key frame there older than D. rc.130's
    /// 500 ms gap, in both of the gate's worlds and toward either channel,
    /// and a refusal is forgotten, so the lane asks again at its next frame.
    /// A switch drops every frame until its key frame comes, so the rule's
    /// gap there would freeze the picture while the link carries no video.
    Switch,
}

/// What an ask got from the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// It passed, and set the flag.
    Granted,
    /// A recovery ask refused while the rule acts, by the 500 ms gap or by
    /// the rule's, and owed: the first frame after the gap opens grants it,
    /// unless a key frame is written before.
    Owed,
    /// Refused by rc.130's gap, outside the rule's scope or as a switch's own
    /// ask (FR-148 P4b-4b), and forgotten.
    Refused,
}

/// One world's grants and its owed ask: the session's own, or under `shadow`
/// those `on` would have had.
#[derive(Clone, Copy, Debug, Default)]
struct World {
    /// When an ask last passed. rc.130's gap runs from it.
    granted: Option<Instant>,
    /// FR-148 P4b-4 — the one owed ask (asks coalesce, as the flag does):
    /// when the oldest still owed was refused.
    owed: Option<Instant>,
}

impl World {
    /// The world after an ask at `now`: one that passed is its last grant and
    /// grants the owed ask too (true when one was owed); one refused while
    /// the rule acts (`rule`) is owed.
    fn answered(&mut self, verdict: Result<(), KeyframeRefusal>, rule: bool, now: Instant) -> bool {
        match verdict {
            Ok(()) => {
                self.granted = Some(now);
                self.owed.take().is_some()
            }
            Err(_) => {
                if rule {
                    self.owed.get_or_insert(now);
                }
                false
            }
        }
    }
}

#[derive(Debug)]
struct GateState {
    /// The session's grants, and under `on` its owed ask.
    real: World,
    /// Under `shadow`: the grants and the owed ask `on` would have had.
    shadow: World,
    /// What the session's lane last said ([`Scope::TODAY`] before it said).
    scope: Scope,
    /// FR-148 P4b-4 (D59) — the last key frame the session's send task wrote,
    /// on either channel and whatever asked for it: when, and its record's
    /// size.
    key: Option<(Instant, u64)>,
}

impl GateState {
    /// Whether `world` lets an ask through at `now`. rc.130: once
    /// [`MIN_KF_GAP`] has passed since its last grant. Under the rule (`rule`):
    /// once [`kf_gap`] has passed since the later of its last grant and the
    /// last key frame written, so a grant whose key frame is still on its way
    /// is never granted again. Refused, by what: the 500 ms gap, or the rule's
    /// cost past it.
    fn verdict(
        &self,
        world: &World,
        rule: bool,
        rate_bps: Option<u64>,
        now: Instant,
    ) -> Result<(), KeyframeRefusal> {
        let since = match rule {
            true => world.granted.max(self.key.map(|(at, _)| at)),
            false => world.granted,
        };
        let Some(since) = since else {
            return Ok(());
        };
        let waited = now.saturating_duration_since(since);
        if waited < MIN_KF_GAP {
            Err(KeyframeRefusal::Gap)
        } else if rule && waited < kf_gap(self.key.map(|(_, bytes)| bytes), rate_bps) {
            Err(KeyframeRefusal::Rule)
        } else {
            Ok(())
        }
    }

    /// `world` owes an ask, and its gap has opened at `now`: the rule's while
    /// it acts (`rule`), rc.130's once the lane has left `video-deadline`.
    fn owes(&self, world: &World, rule: bool, rate_bps: Option<u64>, now: Instant) -> bool {
        world.owed.is_some() && self.verdict(world, rule, rate_bps, now).is_ok()
    }
}

/// rc.130 — a session's gate for forced key frames: the viewer's
/// `rc:keyframe` and, from FR-148 P4b-3, the send tasks' own asks go through
/// it. The flag it sets coalesces (a pump forces at most one IDR per encode
/// however often it is set), and the gap bounds a misbehaving or old
/// controller to one forced IDR per [`MIN_KF_GAP`], so a resync storm cannot
/// pile the largest frames onto a congested link.
///
/// FR-148 P4b-4 (D55's Recovery, D59): under `on`, while the session's lane is
/// on `video-deadline` or switching to it, the gap is [`kf_gap`], counted from
/// the last key frame written (or the last grant, if later), and a refused
/// ask is owed instead of forgotten: the lane's next frame after the gap
/// opens grants it ([`Self::on_frame`]). Everywhere else the gate is rc.130's,
/// grant for grant; under `shadow` it also keeps the grants and the owed ask
/// `on` would have had, for N12's counts.
///
/// FR-148 P4b-4b: the rule spaces recovery key frames, D's asks and the
/// viewer's. A switch's own ask ([`LaneAsk::Switch`]) is rc.130's in both
/// worlds, and never owed; the key frame it brings, written, restarts the
/// rule's clock and answers an owed ask like any other.
#[derive(Debug)]
pub struct KeyframeGate {
    requested: Arc<AtomicBool>,
    state: Mutex<GateState>,
    /// FR-148 P4b-4 — the rate the rule divides by, bit/s, as the pump last
    /// read it ([`kf_rate`]); 0 for none.
    rate_bps: AtomicU64,
}

impl KeyframeGate {
    /// The gate over `requested`, the flag the pumps poll before each encode.
    pub fn new(requested: Arc<AtomicBool>) -> Self {
        Self {
            requested,
            state: Mutex::new(GateState {
                real: World::default(),
                shadow: World::default(),
                scope: Scope::TODAY,
                key: None,
            }),
            rate_bps: AtomicU64::new(0),
        }
    }

    /// The viewer's ask, now: whether it passed. FR-135 R0 (N12) counts every
    /// ask, and whether it was let through; FR-148 P4b-4 counts it as the
    /// viewer's, and by what refused it.
    pub fn ask(&self, session_id: ObjectId) -> bool {
        self.ask_at(session_id, Instant::now())
    }

    fn ask_at(&self, session_id: ObjectId, now: Instant) -> bool {
        self.answer(KeyframeAsker::Viewer, session_id, now, None) == Answer::Granted
    }

    /// FR-148 P4b-4 — the lane's ask at `now`, in the lane's `scope`, for
    /// `why`: whether a key frame will come without its asking again, because
    /// the ask passed or the rule owes it. FR-148 P4b-4b — a switch's own ask
    /// is rc.130's and never owed, so it is answered only when it passes. The
    /// gate keeps the lane's scope either way, for the viewer's asks.
    pub fn ask_lane(&self, session_id: ObjectId, now: Instant, scope: Scope, why: LaneAsk) -> bool {
        self.answer(KeyframeAsker::Lane, session_id, now, Some((scope, why))) != Answer::Refused
    }

    fn answer(
        &self,
        asker: KeyframeAsker,
        session_id: ObjectId,
        now: Instant,
        lane: Option<(Scope, LaneAsk)>,
    ) -> Answer {
        let rate = self.rate();
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((scope, _)) = lane {
            st.scope = scope;
        }
        let scope = st.scope;
        // FR-148 P4b-4b — the rule spaces recovery key frames, D's and the
        // viewer's. A switch's own is not one: rc.130's gap in both worlds,
        // and a refusal is forgotten.
        let recovery = !matches!(lane, Some((_, LaneAsk::Switch)));
        let acts = recovery && scope.acts();
        let real = st.verdict(&st.real, acts, rate, now);
        let real_paid = st.real.answered(real, acts, now);
        // The world `on` would have had follows every ask under every value,
        // so under `shadow` it is `on`'s from the session's first ask; N12's
        // split reads it there.
        let rule = recovery && scope.shadows();
        let would = st.verdict(&st.shadow, rule, rate, now);
        let would_paid = st.shadow.answered(would, rule, now);
        let (counted, paid) = match scope.mode {
            Mode::Shadow => (would, would_paid),
            Mode::Off | Mode::On => (real, real_paid),
        };
        drop(st);
        let answer = match real {
            Ok(()) => Answer::Granted,
            Err(_) if acts => Answer::Owed,
            Err(_) => Answer::Refused,
        };
        let n12 = crate::session_telemetry::counters(session_id);
        n12.note_keyframe_ask(answer == Answer::Granted);
        n12.note_keyframe_split(asker, counted.err());
        if paid {
            n12.note_keyframe_owed_grant();
        }
        match answer {
            Answer::Granted => {
                self.requested.store(true, Relaxed);
                n12.note_keyframe();
            }
            Answer::Owed => n12.note_keyframe_owed(),
            Answer::Refused => {}
        }
        answer
    }

    /// FR-148 P4b-4 — a frame the session's lane was handed, its road decided
    /// at `now`: the rule's scope after it, and the size of the key frame it
    /// wrote, if it wrote one. A key frame written, whatever asked for it,
    /// starts the rule's gap again and answers the owed ask. Any other frame
    /// grants the owed ask once the gap has opened: checked here, at each
    /// frame, with no timer, so a session with no frames costs nothing.
    pub fn on_frame(
        &self,
        session_id: ObjectId,
        now: Instant,
        scope: Scope,
        key_written: Option<u64>,
    ) {
        let rate = self.rate();
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        st.scope = scope;
        if let Some(bytes) = key_written {
            st.key = Some((now, bytes));
            st.real.owed = None;
            st.shadow.owed = None;
            return;
        }
        // Only an ask the rule refused is ever owed, so under `off` and
        // `shadow`, and before the lane first reaches `video-deadline`,
        // nothing is granted here.
        let real = st.owes(&st.real, scope.acts(), rate, now);
        if real {
            st.real.answered(Ok(()), scope.acts(), now);
        }
        let would = scope.mode == Mode::Shadow && st.owes(&st.shadow, scope.shadows(), rate, now);
        if would {
            st.shadow.answered(Ok(()), scope.shadows(), now);
        }
        drop(st);
        if real || would {
            let n12 = crate::session_telemetry::counters(session_id);
            n12.note_keyframe_owed_grant();
            if real {
                self.requested.store(true, Relaxed);
                n12.note_keyframe();
            }
        }
    }

    /// FR-148 P4b-4 — the rate the rule divides by, as the pump reads it at
    /// its budget's step ([`Counters::note_rate`]).
    pub fn set_rate(&self, rate_bps: Option<u64>) {
        self.rate_bps.store(rate_bps.unwrap_or(0), Relaxed);
    }

    fn rate(&self) -> Option<u64> {
        Some(self.rate_bps.load(Relaxed)).filter(|&r| r > 0)
    }

    /// FR-148 P4b-4 — the rule's gap as it stands: [`kf_gap`] of the last key
    /// frame written and the rate the pump last handed over. It spaces forced
    /// key frames only in the rule's scope; the `path delivery` line reads it.
    pub fn gap(&self) -> Duration {
        let rate = self.rate();
        let st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        kf_gap(st.key.map(|(_, bytes)| bytes), rate)
    }
}

/// What a session's pumps and its control channel share: the session's
/// `video-deadline` once the viewer opened one, its key frame gate, and the
/// viewer's report of its own relay (FR-148 P4b-3b).
pub struct Session {
    deadline: tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>,
    gate: Arc<KeyframeGate>,
    viewer_relay: Arc<ViewerRelayReport>,
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
            viewer_relay: Arc::default(),
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

    /// FR-148 P4b-3b — the session's slot for the viewer's report of its own
    /// relay, which the control channel writes.
    pub fn viewer_relay(&self) -> Arc<ViewerRelayReport> {
        Arc::clone(&self.viewer_relay)
    }
}

/// FR-148 P4b-3b (D58) — a session's newest report of the viewer's own relay
/// (`rc:decodestat`'s `relay`), with its arrival time. The control channel
/// writes it and the pumps read it with the class. Each `rc:decodestat`
/// overwrites it, so one without the key clears it, and it has no age bound:
/// it holds until the next `rc:decodestat` replaces it, so a stall that slows
/// the control channel cannot flip the pick back. It keeps nothing until a
/// pump under `shadow` or `on` serves the session ([`Pump::start`]), so under
/// `off` nothing is kept.
///
/// FR-148 P4b-5a — beside it, the newest `kf` a report carried: the key
/// frames the viewer completed whole on `video-bytes` and on
/// `video-deadline`, from its session's start ([`Self::kf`]). A report
/// without the key, or with one that is not two counts, leaves the last, so a
/// line reads the newest count the viewer sent; kept under the same rule.
#[derive(Debug, Default)]
pub struct ViewerRelayReport {
    kept: AtomicBool,
    newest: Mutex<Option<(ViewerRelay, Instant)>>,
    kf: Mutex<Option<(u64, u64)>>,
}

impl ViewerRelayReport {
    /// One `rc:decodestat`, as the control channel received it.
    pub fn note(&self, val: &serde_json::Value) {
        self.note_at(ViewerRelay::of_decodestat(val), Instant::now());
        if let Some(kf) = viewer_kf_of(val)
            && self.kept.load(Relaxed)
        {
            *self.kf.lock().unwrap_or_else(PoisonError::into_inner) = Some(kf);
        }
    }

    fn note_at(&self, report: Option<ViewerRelay>, at: Instant) {
        if !self.kept.load(Relaxed) {
            return;
        }
        *self.newest.lock().unwrap_or_else(PoisonError::into_inner) = report.map(|r| (r, at));
    }

    /// The newest report and when it arrived; `None` when the newest
    /// `rc:decodestat` carried none, or before one came.
    pub fn newest(&self) -> Option<(ViewerRelay, Instant)> {
        *self.newest.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// From now on, each `rc:decodestat` is kept.
    fn keep(&self) {
        self.kept.store(true, Relaxed);
    }

    /// FR-148 P4b-5a — the viewer's newest count of the key frames it
    /// completed whole, `(video-bytes, video-deadline)`; `None` until a
    /// report carried one.
    pub fn kf(&self) -> Option<(u64, u64)> {
        *self.kf.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// FR-148 P4b-5a — `kf` on one `rc:decodestat`: exactly two counts, each a
/// JSON integer from 0 to `u64::MAX`. Anything else, an absent key included,
/// is no report.
fn viewer_kf_of(val: &serde_json::Value) -> Option<(u64, u64)> {
    match val.get("kf")?.as_array()?.as_slice() {
        [bytes, deadline] => Some((bytes.as_u64()?, deadline.as_u64()?)),
        _ => None,
    }
}

/// FR-148 P4b-5a — two counts from a pump's start, one a [`Channel`] or one a
/// cause; a line reads them and never takes them.
#[derive(Debug, Default)]
struct Ledger([AtomicU64; 2]);

impl Ledger {
    fn add(&self, at: usize) {
        self.0[at].fetch_add(1, Relaxed);
    }

    fn read(&self) -> (u64, u64) {
        (self.0[0].load(Relaxed), self.0[1].load(Relaxed))
    }
}

/// FR-148 P4b-5a — a count pair as a line writes it, `(<a>, <b>)`, or `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairOrNone(pub Option<(u64, u64)>);

impl std::fmt::Display for PairOrNone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some((a, b)) => write!(f, "({a}, {b})"),
            None => f.write_str("None"),
        }
    }
}

/// FR-148 P4b-5a — a count as a line writes it, or `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NumOrNone(Option<u64>);

impl std::fmt::Display for NumOrNone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(n) => write!(f, "{n}"),
            None => f.write_str("None"),
        }
    }
}

/// A pump's counts for the `path delivery` line: the frames D dropped, the
/// frames switches dropped, the switches, and the channel in use. Under
/// `shadow`, what `on` would have done. FR-148 P4b-5a — and the key frame
/// ledger, which is what the pump really did: the key frames written whole
/// and the key frames dropped, from the pump's start.
#[derive(Debug)]
pub struct Counters {
    mode: Mode,
    late: AtomicU64,
    drained: AtomicU64,
    switches: AtomicU64,
    /// 0 before the first pick, then 1 + [`Channel`]'s index.
    channel: AtomicU8,
    /// FR-148 P4b-4 — under `shadow` and `on`, from the pump's start: the
    /// session's gate and its key frame asks.
    asks: Mutex<Option<Asks>>,
    /// FR-148 P4b-5a — key frames whose every chunk the send task sent
    /// without error, by the channel they went on ([`Channel::index`]).
    kf_written: Ledger,
    /// FR-148 P4b-5a — key frames the lane dropped before the socket: by D
    /// (0), and while a switch drained or waited (1). Only `on` drops.
    kf_dropped: Ledger,
}

/// FR-148 P4b-4 — what a pump's line reads of its session's key frame asks:
/// the gate the pump feeds the rate, and N12's split as the last line took
/// it. FR-148 P4b-5a — and the session's report of the viewer's key frames.
#[derive(Debug)]
struct Asks {
    gate: Arc<KeyframeGate>,
    n12: Arc<SessionCounters>,
    taken: KeyframeSplit,
    viewer: Arc<ViewerRelayReport>,
}

impl Counters {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            late: AtomicU64::new(0),
            drained: AtomicU64::new(0),
            switches: AtomicU64::new(0),
            channel: AtomicU8::new(0),
            asks: Mutex::new(None),
            kf_written: Ledger::default(),
            kf_dropped: Ledger::default(),
        }
    }

    /// The switch's value, as the pump read it.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The line's part, its counts since the last take; `None` under `off`.
    /// `forward_tsns`: the association's FORWARD TSN count, from its gauges.
    /// FR-148 P4b-5a — the key frame ledger is read, never taken: its counts
    /// run from the pump's start.
    pub fn take(&self, forward_tsns: Option<u64>) -> Option<DeadlineLine> {
        if self.mode == Mode::Off {
            return None;
        }
        let (asks, kf_gap, viewer_kf) = self.take_asks();
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
            asks,
            kf_gap_ms: kf_gap.map(|gap| u64::try_from(gap.as_millis()).unwrap_or(u64::MAX)),
            kf_written: self.kf_written.read(),
            kf_dropped: self.kf_dropped.read(),
            viewer_kf,
        })
    }

    /// FR-148 P4b-4 — at the pump's start, under `shadow` and `on`: the
    /// session's gate, which [`Self::note_rate`] feeds, and N12's counts,
    /// whose split each line takes from here on. FR-148 P4b-5a — and the
    /// session's report of the viewer's key frames, which each line reads.
    fn follow(
        &self,
        gate: Arc<KeyframeGate>,
        n12: Arc<SessionCounters>,
        viewer: Arc<ViewerRelayReport>,
    ) {
        let taken = n12.keyframe_split();
        *self.asks.lock().unwrap_or_else(PoisonError::into_inner) = Some(Asks {
            gate,
            n12,
            taken,
            viewer,
        });
    }

    /// FR-148 P4b-4 (D59) — the rate the session's key frame rule divides by,
    /// as the pump reads it at each frame's budget step: the in-flight
    /// budget's when measured, else the encoder's current target
    /// ([`kf_rate`]). Under `off`, and with no session, nothing.
    pub fn note_rate(&self, measured_bps: Option<u64>, target_bps: Option<u32>) {
        let asks = self.asks.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(asks) = asks.as_ref() {
            asks.gate.set_rate(kf_rate(measured_bps, target_bps));
        }
    }

    /// N12's split since the last line: every key frame ask by who asked, the
    /// refusals by what refused them, and the owed asks granted (under
    /// `shadow`, what `on` would have refused, owed and granted); and the
    /// rule's gap at the line ([`KeyframeGate::gap`]), `None` with no session.
    /// FR-148 P4b-5a — and the viewer's newest count of its key frames,
    /// `None` with no session or before a report carried one.
    fn take_asks(&self) -> (KeyframeSplit, Option<Duration>, Option<(u64, u64)>) {
        let mut asks = self.asks.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(asks) = asks.as_mut() else {
            return (KeyframeSplit::default(), None, None);
        };
        let now = asks.n12.keyframe_split();
        let over = now.since(asks.taken);
        asks.taken = now;
        (over, Some(asks.gate.gap()), asks.viewer.kf())
    }

    /// FR-148 P4b-5a — a key frame whose every chunk went, on `channel`.
    /// Under `off`, nothing.
    fn wrote_key(&self, channel: Channel) {
        if self.mode != Mode::Off {
            self.kf_written.add(channel.index());
        }
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
    /// FR-148 P4b-4 (D59) — N12's split of the session's key frame asks over
    /// the line.
    pub asks: KeyframeSplit,
    /// FR-148 P4b-4 — the key frame rule's gap at the line, ms: `kf_gap` of
    /// the last key frame written and the pump's rate; `None` for a pump that
    /// serves no session's gate.
    pub kf_gap_ms: Option<u64>,
    /// FR-148 P4b-5a — key frames written whole from the pump's start, on
    /// `video-bytes` and on `video-deadline`: every chunk sent without error.
    /// Under `shadow` every write is on `video-bytes`.
    pub kf_written: (u64, u64),
    /// FR-148 P4b-5a — key frames the lane dropped before the socket from
    /// the pump's start: by D, and while a switch drained or waited. Under
    /// `shadow` the lane drops nothing, so both stay 0.
    pub kf_dropped: (u64, u64),
    /// FR-148 P4b-5a — the viewer's newest count of the key frames it
    /// completed whole, on `video-bytes` and on `video-deadline`, from its
    /// session's start (`rc:decodestat`'s `kf`); `None` until a report
    /// carried one.
    pub viewer_kf: Option<(u64, u64)>,
}

impl DeadlineLine {
    pub fn channel_label(&self) -> &'static str {
        self.channel.map_or("none", Channel::label)
    }
}

/// A pump's handle: its switch value and counters, the session it serves,
/// and what it last read: the carrier class and the end that holds the TCP
/// leg (one byte, [`read_code`]), and the viewer's report (FR-148 P4b-3b).
pub struct Pump {
    session_id: ObjectId,
    counters: Arc<Counters>,
    session: Option<Arc<Session>>,
    class: Arc<AtomicU8>,
    viewer: AtomicU8,
}

impl Pump {
    /// At the pump's start, once `detect_constrained_transport` returned: the
    /// class of the pair selected by then, `unknown` with none.
    pub async fn start(
        session_id: ObjectId,
        pc: &RTCPeerConnection,
        counters: &Arc<Counters>,
    ) -> Self {
        let pump = Self::new(session_id, counters);
        pump.read(pc).await;
        pump
    }

    /// The handle before its first read. Under `shadow` and `on` it finds the
    /// session's, whose key frame gate its counters follow (FR-148 P4b-4) and
    /// which keeps the viewer's report of its relay from then on (FR-148
    /// P4b-3b); under `off` it finds none.
    fn new(session_id: ObjectId, counters: &Arc<Counters>) -> Self {
        let session = match counters.mode {
            Mode::Off => None,
            Mode::Shadow | Mode::On => Session::of(session_id),
        };
        if let Some(session) = &session {
            counters.follow(
                session.gate(),
                crate::session_telemetry::counters(session_id),
                session.viewer_relay(),
            );
            session.viewer_relay.keep();
        }
        Self {
            session_id,
            counters: Arc::clone(counters),
            session,
            class: Arc::new(AtomicU8::new(CarrierClass::Unknown.code())),
            viewer: AtomicU8::new(ViewerRelay::code(None)),
        }
    }

    /// The selected pair's class, read now, with the viewer's newest report;
    /// the send task's next frame picks by them. At the pump's start and at
    /// each transport re-check. Under `off` nothing is read.
    pub async fn read(&self, pc: &RTCPeerConnection) {
        if self.counters.mode == Mode::Off {
            return;
        }
        let (class, off_mesh) = class_now(pc).await;
        self.note(class, off_mesh);
    }

    /// FR-148 P4b-3b (D58) — one read: the agent's own class, whether the pair
    /// is selected off the mesh, and the session's newest report, which counts
    /// only there. The class line names both ends when one of the three it
    /// carries changed: the class, the report, or the end that holds the TCP
    /// leg. `from` and `to` stay the agent's own class.
    fn note(&self, class: CarrierClass, off_mesh: bool) {
        let report = self
            .session
            .as_ref()
            .and_then(|s| s.viewer_relay.newest())
            .map(|(report, _)| report);
        let end = TcpEnd::of(class, off_mesh, report);
        let (was, was_end) = read_of(self.class.swap(read_code(class, end), Relaxed));
        let was_report = ViewerRelay::of_code(self.viewer.swap(ViewerRelay::code(report), Relaxed));
        if (was, was_report, was_end) != (class, report, end) {
            info!(
                session = %self.session_id,
                from = was.word(),
                to = class.word(),
                viewer_relay = ViewerRelay::word_of(report),
                tcp_end = end.word(),
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
            gauges: None,
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

    /// Marks `dc`'s stream deadline-limited; refused, why (FR-148 P4b).
    fn mark(&self, dc: &Self::Dc) -> impl Future<Output = Result<(), DeadlineMarkRefusal>> + Send;

    /// One key frame ask through the session's gate, at `now`, the lane in
    /// `scope`, for `why`: whether a key frame comes without asking again,
    /// because it passed or (FR-148 P4b-4) the rule owes it, which it never
    /// does for a switch's own ask (FR-148 P4b-4b).
    fn ask_keyframe(&self, now: Instant, scope: Scope, why: LaneAsk) -> bool;

    /// FR-148 P4b-4 — a frame's road was decided at `now`, the lane in `scope`
    /// after it: `key_written`, the size of the key frame it wrote.
    fn frame_done(&self, now: Instant, scope: Scope, key_written: Option<u64>);

    /// FR-148 P4b-5a (D60) — the association's gauges now, as a look reads
    /// them (`GaugeReader::peek`: it resets nothing the `path delivery`
    /// line's take owns); `None` with no association, or while its
    /// transport's slot is held. Never waits.
    fn gauges(&mut self) -> Option<Gauges>;
}

/// A lane's wire in a live session.
pub struct SessionWire {
    bytes: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    session: Option<Arc<Session>>,
    sctp: Arc<RTCSctpTransport>,
    session_id: ObjectId,
    /// FR-148 P4b-5a — the association's gauges once a look found them: one
    /// association serves a transport for its life, so the reader is asked
    /// for until it comes, and then only read.
    gauges: Option<GaugeReader>,
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
        dc.id() // its stream's, from the vendored webrtc's accept path (FR-148 P4b)
    }

    async fn mark(&self, dc: &Self::Dc) -> Result<(), DeadlineMarkRefusal> {
        self.sctp.mark_deadline_stream(dc.id()).await
    }

    fn ask_keyframe(&self, now: Instant, scope: Scope, why: LaneAsk) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.gate.ask_lane(self.session_id, now, scope, why))
    }

    fn frame_done(&self, now: Instant, scope: Scope, key_written: Option<u64>) {
        if let Some(s) = &self.session {
            s.gate.on_frame(self.session_id, now, scope, key_written);
        }
    }

    fn gauges(&mut self) -> Option<Gauges> {
        if self.gauges.is_none() {
            self.gauges = self.sctp.gauges();
        }
        self.gauges.as_ref().map(GaugeReader::peek)
    }
}

/// A frame the lane let through: write it on `dc`, numbered `frame_seq`,
/// while this lives.
pub struct Routed<W: Wire> {
    pub dc: W::Dc,
    pub frame_seq: u32,
    _held: W::Held,
    /// FR-148 P4b-5a — the channel `dc` is, and whether the frame is a key
    /// frame: what [`Lane::written`] counts.
    channel: Channel,
    key: bool,
}

/// A switch under way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Switch {
    from: Channel,
    to: Channel,
    class: CarrierClass,
    /// The end that held the TCP leg when the pick said switch.
    end: TcpEnd,
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

/// FR-148 P4b-5a (D60) — a still stretch: this long or longer with no lane
/// write, from the end of the last write's sends to the next frame the lane
/// lets through. A constant, never a knob.
const STILL: Duration = Duration::from_secs(1);

/// FR-148 P4b-5a (D60) — at most one [`WAKE_LINE`] a pump in any this long;
/// and how long a wake on a delta waits for its [`WAKE_KEY_LINE`].
const WAKE_EVERY: Duration = Duration::from_secs(10);

/// FR-148 P4b-5a (D60) — the first write after a still stretch, which a field
/// read greps for.
pub const WAKE_LINE: &str = "path wake (FR-148 P4b-5a, D60)";

/// FR-148 P4b-5a (D60) — the first key frame written after a wake on a delta;
/// `grep 'path wake'` finds it beside [`WAKE_LINE`].
pub const WAKE_KEY_LINE: &str = "path wake key (FR-148 P4b-5a, D60)";

/// A write, as the wake lines name it.
#[derive(Clone, Copy, Debug)]
struct Wrote {
    key: bool,
    bytes: u64,
    channel: Channel,
}

/// FR-148 P4b-5a (D60) — a lane's still stretches: what the path held at the
/// first write after a second or more with none, and at the first key frame
/// after it. A tail loss before a still screen is found only by a T3, which
/// cuts the window to one MTU; these lines read the window there. On the
/// data-channel pumps a still screen still writes the 60 ms keepalive, so a
/// stretch is most often a pump that held its frames or a lane that dropped
/// them.
#[derive(Clone, Copy, Debug, Default)]
struct Wake {
    /// The end of the last write, and the association's fast retransmissions
    /// and T3-rtx expiries since it began, read then (`None` without gauges):
    /// when the send task said its sends ended ([`Lane::written`]), else when
    /// the lane let it through.
    last: Option<(Instant, Option<(u64, u64)>)>,
    /// The last [`WAKE_LINE`].
    said: Option<Instant>,
    /// The stretches since it that wrote none.
    skipped: u64,
    /// A wake on a delta, until its key frame or [`WAKE_EVERY`]: when, and
    /// the T3-rtx expiries then.
    keyless: Option<(Instant, Option<u64>)>,
}

impl Wake {
    /// A write at `now`, with the gauges a look read then.
    fn wrote(&mut self, session_id: ObjectId, now: Instant, wrote: Wrote, gauges: Option<Gauges>) {
        let loss = gauges.map(|g| (g.fast_retransmits, g.t3_expiries));
        if let Some((at, t3)) = self.keyless {
            let delay = now.saturating_duration_since(at);
            if delay >= WAKE_EVERY {
                self.keyless = None;
            } else if wrote.key {
                self.keyless = None;
                let since = t3
                    .zip(gauges)
                    .map(|(t3, g)| g.t3_expiries.saturating_sub(t3));
                emit_wake_key(session_id, ms(delay), wrote, gauges.as_ref(), since);
            }
        }
        if let Some((last, last_loss)) = self.last {
            let still = now.saturating_duration_since(last);
            let quiet = self
                .said
                .is_some_and(|said| now.saturating_duration_since(said) < WAKE_EVERY);
            if still >= STILL && quiet {
                self.skipped = self.skipped.saturating_add(1);
            } else if still >= STILL {
                let over = last_loss.zip(loss).map(|((fast, t3), (fast_now, t3_now))| {
                    (fast_now.saturating_sub(fast), t3_now.saturating_sub(t3))
                });
                emit_wake(
                    session_id,
                    ms(still),
                    gauges.as_ref(),
                    over,
                    wrote,
                    self.skipped,
                );
                self.said = Some(now);
                self.skipped = 0;
                self.keyless = (!wrote.key).then_some((now, gauges.map(|g| g.t3_expiries)));
            }
        }
        self.last = Some((now, loss));
    }

    /// The last write's sends ended at `now`, with the gauges a look read
    /// then: the next stretch, and the counts over it, start here.
    fn sent(&mut self, now: Instant, gauges: Option<Gauges>) {
        let loss = gauges.map(|g| (g.fast_retransmits, g.t3_expiries));
        self.last = Some((now, loss));
    }
}

/// [`WAKE_LINE`]: the stretch, the gauges at its first write, the fast
/// retransmissions and T3-rtx expiries over it (`over`, `None` when the write
/// before it read no gauges), the frame, and the stretches skipped since the
/// last line. Without gauges, the stretch and the frame alone.
fn emit_wake(
    session_id: ObjectId,
    still_ms: u64,
    gauges: Option<&Gauges>,
    over: Option<(u64, u64)>,
    wrote: Wrote,
    skipped: u64,
) {
    let kind = if wrote.key { "key" } else { "delta" };
    match gauges {
        Some(g) => info!(
            session = %session_id,
            still_ms,
            cwnd = g.cwnd,
            ssthresh = g.ssthresh,
            inflight_bytes = g.inflight_bytes,
            pending_bytes = g.pending_bytes,
            ack_delay_ms = %NumOrNone(g.ack_delay.map(ms)),
            max_payload = g.max_payload,
            t3_in_stretch = %NumOrNone(over.map(|(_, t3)| t3)),
            fast_in_stretch = %NumOrNone(over.map(|(fast, _)| fast)),
            frame = %kind,
            bytes = wrote.bytes,
            channel = %wrote.channel.label(),
            skipped,
            "{WAKE_LINE}"
        ),
        None => info!(
            session = %session_id,
            still_ms,
            frame = %kind,
            bytes = wrote.bytes,
            channel = %wrote.channel.label(),
            skipped,
            "{WAKE_LINE}"
        ),
    }
}

/// [`WAKE_KEY_LINE`]: the key frame's delay from the wake, its size and
/// channel, the gauges at its write, and the T3-rtx expiries since the wake
/// (`None` when the wake read no gauges). Without gauges, the first three.
fn emit_wake_key(
    session_id: ObjectId,
    delay_ms: u64,
    wrote: Wrote,
    gauges: Option<&Gauges>,
    t3_since_wake: Option<u64>,
) {
    match gauges {
        Some(g) => info!(
            session = %session_id,
            delay_ms,
            bytes = wrote.bytes,
            channel = %wrote.channel.label(),
            cwnd = g.cwnd,
            ssthresh = g.ssthresh,
            inflight_bytes = g.inflight_bytes,
            pending_bytes = g.pending_bytes,
            ack_delay_ms = %NumOrNone(g.ack_delay.map(ms)),
            max_payload = g.max_payload,
            t3_since_wake = %NumOrNone(t3_since_wake),
            "{WAKE_KEY_LINE}"
        ),
        None => info!(
            session = %session_id,
            delay_ms,
            bytes = wrote.bytes,
            channel = %wrote.channel.label(),
            "{WAKE_KEY_LINE}"
        ),
    }
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
    /// FR-148 P4b — marks refused since one was accepted, and the reasons said.
    refused: u32,
    said_refused: Vec<DeadlineMarkRefusal>,
    said_none_open: bool,
    /// FR-148 P4b-5a (D60) — its still stretches, under `shadow` and `on`.
    wake: Wake,
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
            refused: 0,
            said_refused: Vec::new(),
            said_none_open: false,
            wake: Wake::default(),
        }
    }

    /// A frame's road, decided once, before its first write: `Some`, write it
    /// whole on the channel and under the number given, while the `Routed`
    /// lives; `None`, write nothing. `wire` is the frame as
    /// `frame_video_bytes` framed it, `enqueued_at` its hand-off to the send
    /// task.
    pub async fn route(&mut self, wire: &[u8], enqueued_at: Instant) -> Option<Routed<W>> {
        let keyframe = crate::peer::video_bytes_is_keyframe(wire);
        self.route_at(keyframe, wire.len(), enqueued_at, Instant::now())
            .await
    }

    /// FR-148 P4b-5a — the send task's word on a frame the lane let through,
    /// once its chunk loop ended: `ok`, every chunk sent without error. A key
    /// frame written whole counts on the channel it went on; a failed send
    /// counts on neither side, and is no drop. The write ends here, so a still
    /// stretch starts here (D60): a frame whose chunks took a second to hand
    /// over is no stretch. Returns `ok`: the condition of the send task's arm
    /// for a frame on the wire. Under `off`, nothing is counted and neither
    /// the clock nor the gauges are read.
    pub fn written(&mut self, out: &Routed<W>, ok: bool) -> bool {
        if self.counters.mode == Mode::Off {
            return ok;
        }
        self.written_at(out, ok, Instant::now())
    }

    /// [`Self::written`], the frame's sends ended at `now`.
    fn written_at(&mut self, out: &Routed<W>, ok: bool, now: Instant) -> bool {
        if self.counters.mode != Mode::Off {
            if ok && out.key {
                self.counters.wrote_key(out.channel);
            }
            let gauges = self.wire.gauges();
            self.wake.sent(now, gauges);
        }
        ok
    }

    /// The road of a frame `len` bytes long; then, under `shadow` and `on`,
    /// the session's gate hears of it (FR-148 P4b-4): the key frame rule's
    /// clock restarts at a key frame written, and any other frame grants an
    /// owed ask once the rule's gap has opened. FR-148 P4b-5a (D60) — and a
    /// frame written is the lane's write for its still stretches, read with
    /// the association's gauges as they stand.
    async fn route_at(
        &mut self,
        keyframe: bool,
        len: usize,
        enqueued_at: Instant,
        now: Instant,
    ) -> Option<Routed<W>> {
        let out = self.road(keyframe, enqueued_at, now).await;
        if self.counters.mode != Mode::Off {
            let written = (keyframe && out.is_some()).then_some(len as u64);
            self.wire.frame_done(now, self.scope(), written);
            if let Some(out) = &out {
                let wrote = Wrote {
                    key: keyframe,
                    bytes: len as u64,
                    channel: out.channel,
                };
                let gauges = self.wire.gauges();
                self.wake.wrote(self.session_id, now, wrote, gauges);
            }
        }
        out
    }

    async fn road(
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
            Mode::Off => Some(self.write(Channel::Bytes, keyframe, bytes, held)),
            Mode::Shadow => {
                self.shadow(keyframe, age, now).await;
                Some(self.write(Channel::Bytes, keyframe, bytes, held))
            }
            Mode::On => self.on(keyframe, age, now, bytes, held).await,
        }
    }

    /// A frame let through on `dc`, which is `channel`.
    fn write(&mut self, channel: Channel, keyframe: bool, dc: W::Dc, held: W::Held) -> Routed<W> {
        self.frame_seq = self.frame_seq.wrapping_add(1);
        Routed {
            dc,
            frame_seq: self.frame_seq,
            _held: held,
            channel,
            key: keyframe,
        }
    }

    /// The pump's last read: the agent's own class, and the end that holds
    /// the TCP leg (FR-148 P4b-3b).
    fn carrier(&self) -> (CarrierClass, TcpEnd) {
        read_of(self.class.load(Relaxed))
    }

    /// FR-148 P4b-4 — the key frame rule's scope as the lane stands: on
    /// `video-deadline`, or switching to it (under `shadow`, `on`'s lane).
    fn scope(&self) -> Scope {
        let on = match self.phase {
            Phase::Draining(s) | Phase::Keying(s) => Some(s.to),
            Phase::Steady => self.on,
        };
        Scope {
            mode: self.counters.mode,
            deadline: on == Some(Channel::Deadline),
        }
    }

    /// A key frame ask through the session's gate, the lane as it stands, for
    /// `why`.
    fn ask(&self, now: Instant, why: LaneAsk) -> bool {
        self.wire.ask_keyframe(now, self.scope(), why)
    }

    async fn on(
        &mut self,
        keyframe: bool,
        age: Duration,
        now: Instant,
        bytes: W::Dc,
        held: W::Held,
    ) -> Option<Routed<W>> {
        let (class, end) = self.carrier();
        let deadline = match self.framed {
            true => self.wire.deadline().await,
            false => None,
        };
        self.say_none_open(class, end, deadline.is_some());
        let want = pick(end, deadline.is_some());
        if self.phase == Phase::Steady && self.on != Some(want) {
            // The mark first: refused, nothing switches; the next frame tries.
            let marked = match (&deadline, want) {
                (Some(dc), Channel::Deadline) => self.mark(dc).await,
                _ => true,
            };
            match self.on {
                // The first pick, before any frame went: no old channel, no
                // wait.
                None => {
                    let first = if marked { want } else { Channel::Bytes };
                    self.on = Some(first);
                    self.counters.set_channel(first);
                    self.say_first(first, class, end);
                }
                Some(from) if marked => {
                    self.phase = Phase::Draining(Switch {
                        from,
                        to: want,
                        class,
                        end,
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
                        self.drop_late(keyframe);
                        let _ = self.ask(now, LaneAsk::Late);
                        return None;
                    }
                    Some(self.write(Channel::Deadline, keyframe, dc, held))
                }
                _ => Some(self.write(Channel::Bytes, keyframe, bytes, held)),
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
                    return self.drop_switching(keyframe);
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
    /// the switch waited. Its asks are the switch's own, its ask again after a
    /// key frame older than D included ([`LaneAsk::Switch`], FR-148 P4b-4b):
    /// asked at each frame until one passes rc.130's gap, never owed.
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
                s.asked = self.ask(now, LaneAsk::Switch);
                self.phase = Phase::Keying(s);
            }
            return self.drop_switching(keyframe);
        }
        let (to, dc) = match (s.to, deadline) {
            (Channel::Deadline, Some(dc)) => (Channel::Deadline, dc),
            _ => (Channel::Bytes, bytes),
        };
        if to == Channel::Deadline && age > DEADLINE {
            // Too old for the new channel: dropped, and another asked, still
            // the switch's own.
            self.drop_late(keyframe);
            s.asked = self.ask(now, LaneAsk::Switch);
            self.phase = Phase::Keying(s);
            return None;
        }
        let out = self.write(to, keyframe, dc, held);
        self.phase = Phase::Steady;
        self.on = Some(to);
        self.counters.set_channel(to);
        if to != s.from {
            self.counters.switches.fetch_add(1, Relaxed);
        }
        info!(
            session = %self.session_id,
            drained_ms = s.drained_ms,
            "video channel: {} -> {} (carrier={}, frame_seq={}, held_ms={}, dropped={}, tcp_end={})",
            s.from.label(),
            to.label(),
            s.class.word(),
            out.frame_seq,
            ms(now.saturating_duration_since(s.since)),
            s.dropped,
            s.end.word(),
        );
        Some(out)
    }

    /// `on`'s choices, without its writes, marks, drops or asks: the channel
    /// it would be on, the switch it would make at a key frame, the frames D
    /// would drop there.
    async fn shadow(&mut self, keyframe: bool, age: Duration, now: Instant) {
        let (class, end) = self.carrier();
        let open = self.framed && self.wire.deadline().await.is_some();
        self.say_none_open(class, end, open);
        let want = pick(end, open);
        match self.on {
            None => {
                self.on = Some(want);
                self.counters.set_channel(want);
                self.say_first(want, class, end);
            }
            Some(from) if self.phase == Phase::Steady && want != from => {
                self.phase = Phase::Keying(Switch {
                    from,
                    to: want,
                    class,
                    end,
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
                "video channel (shadow): {} -> {} (carrier={}, frame_seq={}, held_ms={}, dropped=0, tcp_end={})",
                s.from.label(),
                s.to.label(),
                s.class.word(),
                self.frame_seq.wrapping_add(1),
                ms(now.saturating_duration_since(s.since)),
                s.end.word(),
            );
        }
        if self.on == Some(Channel::Deadline) && age > DEADLINE {
            self.counters.late.fetch_add(1, Relaxed);
        }
    }

    /// Under `on`, before the first write on `video-deadline`: its stream
    /// marked, once.
    ///
    /// FR-148 P4b — a refusal is tried again at the lane's next frame that
    /// wants `video-deadline`, key or delta, and at each one after until one
    /// is accepted. A key frame need not come first: the encoder sends one
    /// only when asked, and before, a session whose first mark was refused
    /// waited on `video-bytes` for one that never came. A mark accepted
    /// between key frames starts the switch, whose own key frame its ask
    /// brings through the session's gate ([`LaneAsk::Switch`]: rc.130's gap,
    /// never owed). A try never waits for the transport's slot: held, it is a
    /// refusal, tried again at the next frame. Each reason is said once, with
    /// the stream, until a mark is accepted, and a mark accepted after
    /// refusals says how many there were.
    async fn mark(&mut self, dc: &W::Dc) -> bool {
        let stream = W::stream(dc);
        if self.marked == Some(stream) {
            return true;
        }
        match self.wire.mark(dc).await {
            Ok(()) => {
                if self.refused > 0 {
                    info!(
                        session = %self.session_id,
                        stream,
                        refused = self.refused,
                        "video channel: video-deadline's stream was marked after it was refused; switching to it (FR-148 P4b-3)"
                    );
                }
                self.marked = Some(stream);
                self.refused = 0;
                self.said_refused.clear();
                true
            }
            Err(why) => {
                self.refused = self.refused.saturating_add(1);
                if !self.said_refused.contains(&why) {
                    self.said_refused.push(why);
                    warn!(
                        session = %self.session_id,
                        stream,
                        reason = %why,
                        "video channel: video-deadline's stream was not marked; staying on video-bytes, and trying again at each frame (FR-148 P4b-3)"
                    );
                }
                false
            }
        }
    }

    /// A frame dropped for a switch: it consumes its number. FR-148 P4b-5a —
    /// a key frame is counted as one (`kf_dropped.1`).
    fn drop_switching(&mut self, keyframe: bool) -> Option<Routed<W>> {
        self.frame_seq = self.frame_seq.wrapping_add(1);
        if let Phase::Draining(s) | Phase::Keying(s) = &mut self.phase {
            s.dropped = s.dropped.saturating_add(1);
        }
        self.counters.drained.fetch_add(1, Relaxed);
        if keyframe {
            self.counters.kf_dropped.add(1);
        }
        None
    }

    /// A frame D dropped: it consumes its number. FR-148 P4b-5a — a key frame
    /// is counted as one (`kf_dropped.0`).
    fn drop_late(&mut self, keyframe: bool) {
        self.frame_seq = self.frame_seq.wrapping_add(1);
        self.counters.late.fetch_add(1, Relaxed);
        if keyframe {
            self.counters.kf_dropped.add(0);
        }
    }

    fn tag(&self) -> &'static str {
        match self.counters.mode {
            Mode::Shadow => " (shadow)",
            Mode::Off | Mode::On => "",
        }
    }

    fn say_first(&self, first: Channel, class: CarrierClass, end: TcpEnd) {
        info!(
            session = %self.session_id,
            "video channel{}: none -> {} (carrier={}, frame_seq={}, held_ms=0, dropped=0, tcp_end={})",
            self.tag(),
            first.label(),
            class.word(),
            self.frame_seq.wrapping_add(1),
            end.word(),
        );
    }

    /// The carrier wants `video-deadline` and the session has none open (an
    /// older viewer, or an unframed session): one line says so.
    fn say_none_open(&mut self, class: CarrierClass, end: TcpEnd, open: bool) {
        if open || !end.is_tcp() || self.said_none_open {
            return;
        }
        self.said_none_open = true;
        info!(
            session = %self.session_id,
            carrier = class.word(),
            framed = self.framed,
            tcp_end = end.word(),
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
        /// FR-148 P4b: why a mark is refused while `mark_ok` is false;
        /// `None`, no such stream.
        refusal: Option<DeadlineMarkRefusal>,
        marks: Vec<u16>,
        ask_ok: bool,
        asks: u32,
        /// FR-148 P4b-4b: why the lane made each ask, in order.
        whys: Vec<LaneAsk>,
        /// FR-148 P4b-4: a session's gate the lane's asks and frames reach,
        /// in place of `ask_ok`.
        gate: Option<(Arc<KeyframeGate>, ObjectId)>,
        /// FR-148 P4b-5a: what the association's gauges read, as a look
        /// gives them; `None`, no association or its slot held.
        gauges: Option<Gauges>,
        /// FR-148 P4b-5a: the lane's looks at them.
        gauge_reads: u32,
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

        async fn mark(&self, dc: &u16) -> Result<(), DeadlineMarkRefusal> {
            let mut s = self.st();
            s.marks.push(*dc);
            match s.mark_ok {
                true => Ok(()),
                false => Err(s.refusal.unwrap_or(DeadlineMarkRefusal::NoStream)),
            }
        }

        fn ask_keyframe(&self, now: Instant, scope: Scope, why: LaneAsk) -> bool {
            let mut s = self.st();
            s.asks += 1;
            s.whys.push(why);
            match s.gate.clone() {
                Some((gate, id)) => {
                    drop(s);
                    gate.ask_lane(id, now, scope, why)
                }
                None => s.ask_ok,
            }
        }

        fn frame_done(&self, now: Instant, scope: Scope, key_written: Option<u64>) {
            let gate = self.st().gate.clone();
            if let Some((gate, id)) = gate {
                gate.on_frame(id, now, scope, key_written);
            }
        }

        fn gauges(&mut self) -> Option<Gauges> {
            let mut s = self.st();
            s.gauge_reads += 1;
            s.gauges
        }
    }

    fn sid() -> ObjectId {
        ObjectId::parse_str("65a1b2c3d4e5f60718293a4c").unwrap()
    }

    /// FR-148 P4b-4 — a gate over a fresh flag, for a session of its own,
    /// its rate `rate_bps`.
    fn gate_at(rate_bps: Option<u64>) -> (KeyframeGate, Arc<AtomicBool>, ObjectId) {
        let flag = Arc::new(AtomicBool::new(false));
        let gate = KeyframeGate::new(Arc::clone(&flag));
        gate.set_rate(rate_bps);
        (gate, flag, ObjectId::new())
    }

    /// The rule's scope: `on`, the lane on `video-deadline`.
    const RULE: Scope = Scope {
        mode: Mode::On,
        deadline: true,
    };

    /// N12's split of a session's key frame asks.
    fn split(id: ObjectId) -> KeyframeSplit {
        crate::session_telemetry::counters(id).keyframe_split()
    }

    /// The cells' draws (xorshift64), from `seed`.
    fn draws(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        }
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
            self.sized(keyframe, age_ms, 1_000)
        }

        /// [`Self::frame`], `len` bytes long.
        fn sized(&mut self, keyframe: bool, age_ms: u64, len: usize) -> Option<(u16, u32)> {
            self.t += Duration::from_millis(10);
            let enqueued = self.t - Duration::from_millis(age_ms);
            futures::executor::block_on(self.lane.route_at(keyframe, len, enqueued, self.t))
                .map(|r| (r.dc, r.frame_seq))
        }

        /// FR-148 P4b-4 — the lane's asks and frames go through a session's
        /// gate, whose rate is `rate_bps`: the gate, its flag and the session.
        fn gated(
            &mut self,
            rate_bps: Option<u64>,
        ) -> (Arc<KeyframeGate>, Arc<AtomicBool>, ObjectId) {
            let (gate, flag, id) = gate_at(rate_bps);
            let gate = Arc::new(gate);
            self.fake.st().gate = Some((Arc::clone(&gate), id));
            (gate, flag, id)
        }

        /// Frames on time, until the next one is due at `at`.
        fn deltas_until(&mut self, at: Instant) {
            while self.t + Duration::from_millis(10) < at {
                self.delta();
            }
        }

        fn key(&mut self) -> Option<(u16, u32)> {
            self.frame(true, 0)
        }

        fn delta(&mut self) -> Option<(u16, u32)> {
            self.frame(false, 0)
        }

        /// FR-148 P4b-5a — [`Self::frame`] as the send task takes it: routed,
        /// then its chunks sent, every one without error (`ok`) or not, and
        /// the lane told in the arm's condition.
        fn sent(&mut self, keyframe: bool, age_ms: u64, ok: bool) -> Option<(u16, u32)> {
            self.t += Duration::from_millis(10);
            let enqueued = self.t - Duration::from_millis(age_ms);
            let out =
                futures::executor::block_on(self.lane.route_at(keyframe, 1_000, enqueued, self.t))?;
            assert_eq!(
                self.lane.written_at(&out, ok, self.t),
                ok,
                "the arm's condition"
            );
            Some((out.dc, out.frame_seq))
        }

        /// FR-148 P4b-5a — a frame `len` bytes long, on time, its turn at
        /// `at`.
        fn wrote_at(&mut self, at: Instant, keyframe: bool, len: usize) -> Option<(u16, u32)> {
            self.t = at;
            futures::executor::block_on(self.lane.route_at(keyframe, len, at, at))
                .map(|r| (r.dc, r.frame_seq))
        }
    }

    /// What `f` logged, beside what it returned.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test's own log capture, never a console (root clippy.toml, #3938)"
    )]
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

    /// FR-148 P4b-3b — a session whose viewer reports its relay, under
    /// `mode`: its `Session`, registered as `AgentPeer::new` registers it, a
    /// pump that serves it, and a lane on a scripted wire that picks by the
    /// pump's reads. Each has a fresh id, since sessions are found by id.
    fn ends(mode: Mode) -> (Arc<Session>, Pump, Rig) {
        let id = ObjectId::new();
        let session = Session::register(id, Arc::new(AtomicBool::new(false)));
        let rig = Rig::new(mode);
        let mut pump = Pump::new(id, &rig.counters);
        pump.class = Arc::clone(&rig.class);
        (session, pump, rig)
    }

    /// One `rc:decodestat` as the viewer sends it, with `relay` as given or
    /// without the key.
    fn decodestat(relay: Option<serde_json::Value>) -> serde_json::Value {
        let mut val =
            serde_json::json!({ "t": "rc:decodestat", "fps": 30, "struggling": false, "de": 1 });
        if let Some(relay) = relay {
            val["relay"] = relay;
        }
        val
    }

    /// The viewer reports `relay` (`None`: an `rc:decodestat` without it) and
    /// the pump then reads the pair: its class, and whether it is off the mesh.
    fn report_and_read(
        session: &Session,
        pump: &Pump,
        relay: Option<&str>,
        class: CarrierClass,
        off_mesh: bool,
    ) {
        session
            .viewer_relay()
            .note(&decodestat(relay.map(Into::into)));
        pump.note(class, off_mesh);
    }

    /// FR-148 P4b-3b cell 3 — `relay` is read by exact equality: `udp`, `tcp`
    /// and `tls` are kept, and anything else, a non-string or an absent key
    /// included, is no report. Each `rc:decodestat` overwrites the session's
    /// report with its arrival time, so one without the key clears it, and so
    /// does one whose value cannot be read.
    #[test]
    fn the_viewer_s_relay_is_read_by_equality_and_the_newest_overwrites() {
        use ViewerRelay::{Tcp, Tls, Udp};
        use serde_json::json;
        for (relay, want) in [
            (Some(json!("udp")), Some(Udp)),
            (Some(json!("tcp")), Some(Tcp)),
            (Some(json!("tls")), Some(Tls)),
            (Some(json!("TLS")), None),
            (Some(json!("tcp ")), None),
            (Some(json!("quic")), None),
            (Some(json!("")), None),
            (Some(json!(1)), None),
            (Some(json!(true)), None),
            (Some(json!(null)), None),
            (Some(json!(["tls"])), None),
            (None, None),
        ] {
            let val = decodestat(relay.clone());
            assert_eq!(ViewerRelay::of_decodestat(&val), want, "{relay:?}");
        }
        assert_eq!(
            ViewerRelay::of_decodestat(&json!({ "t": "rc:decodestat", "relay": { "p": "tls" } })),
            None
        );

        let slot = ViewerRelayReport::default();
        slot.keep();
        let t0 = Instant::now();
        slot.note_at(Some(Tls), t0);
        assert_eq!(slot.newest(), Some((Tls, t0)));
        let t1 = t0 + Duration::from_secs(3_600);
        slot.note_at(Some(Tcp), t1);
        assert_eq!(slot.newest(), Some((Tcp, t1)), "the newest, with its time");
        slot.note(&decodestat(None));
        assert_eq!(slot.newest(), None, "an absent key clears it");
        slot.note(&decodestat(Some(json!("udp"))));
        assert_eq!(slot.newest().map(|(r, _)| r), Some(Udp));
        slot.note(&decodestat(Some(json!("TLS"))));
        assert_eq!(slot.newest(), None, "so does a value it cannot read");
    }

    /// FR-148 P4b-3b — TCP somewhere, read at both ends (D58): the agent's own
    /// class is TCP, or the pair is selected off the mesh and the viewer's
    /// newest report is `tcp` or `tls`. On the mesh, or with no pair selected,
    /// the report is ignored; with no report the end is the agent's alone, as
    /// P4b-3 picked. One byte carries a pump's read to its lane.
    #[test]
    fn tcp_somewhere_reads_both_ends_off_the_mesh() {
        use TcpEnd::{Agent, Both, None as Neither, Viewer};
        use ViewerRelay::{Tcp, Tls, Udp};
        for (class, off_mesh, report, want) in [
            (C::Unknown, true, Some(Tls), Viewer),
            (C::Unknown, true, Some(Tcp), Viewer),
            (C::Unknown, true, Some(Udp), Neither),
            (C::Unknown, true, None, Neither),
            (C::Unknown, false, Some(Tls), Neither),
            (C::Direct, true, Some(Tls), Viewer),
            (C::TurnUdp, true, Some(Tls), Viewer),
            (C::TurnUdp, true, Some(Udp), Neither),
            (C::TurnTls, true, Some(Tcp), Both),
            (C::TurnTcp, true, Some(Tls), Both),
            (C::TurnTcp, true, Some(Udp), Agent),
            (C::TurnTls, true, None, Agent),
            (C::MeshDirect, false, Some(Tcp), Neither),
            (C::MeshDirect, false, Some(Tls), Neither),
            (C::MeshRelay, false, Some(Udp), Agent),
            (C::MeshRelay, false, Some(Tls), Agent),
            (C::MeshRelay, false, None, Agent),
        ] {
            let end = TcpEnd::of(class, off_mesh, report);
            assert_eq!(end, want, "{class:?} off_mesh={off_mesh} {report:?}");
            let channel = match want {
                Neither => Channel::Bytes,
                Agent | Viewer | Both => Channel::Deadline,
            };
            assert_eq!(pick(end, true), channel, "{class:?} {report:?}");
            assert_eq!(pick(end, false), Channel::Bytes, "none open");
        }
        for class in C::ALL {
            assert_eq!(TcpEnd::from(class), TcpEnd::of(class, true, None));
            for end in [Neither, Agent, Viewer, Both] {
                let code = read_code(class, end);
                let want = match end {
                    Viewer | Both => TcpEnd::of_legs(class.is_tcp(), true),
                    Neither | Agent => TcpEnd::from(class),
                };
                assert_eq!(read_of(code), (class, want), "{class:?} {end:?}");
            }
        }
        assert_eq!(
            read_of(C::TurnTcp.code()),
            (C::TurnTcp, Agent),
            "P4b-3's byte"
        );
    }

    /// FR-148 P4b-3b cell 4 — the viewer's relay alone: a pair off the mesh
    /// whose remote is the viewer's relay reads `unknown` at the agent, whose
    /// own candidate is none. A viewer that reports `tls` or `tcp` puts the
    /// session's first frame on `video-deadline`, `tcp_end=viewer`; with
    /// `udp`, or no report, it goes on `video-bytes`, as P4b-3 sent it.
    #[test]
    fn the_viewer_s_tcp_relay_alone_picks_video_deadline() {
        for (relay, stream, end) in [
            (Some("tls"), DL, "viewer"),
            (Some("tcp"), DL, "viewer"),
            (Some("udp"), BYTES, "none"),
            (None, BYTES, "none"),
        ] {
            let (session, pump, mut rig) = ends(Mode::On);
            let (wrote, out) = logged(|| {
                report_and_read(&session, &pump, relay, C::Unknown, true);
                rig.key()
            });
            assert_eq!(wrote, Some((stream, 1)), "{relay:?}");
            let label = if stream == DL {
                DEADLINE_LABEL
            } else {
                BYTES_LABEL
            };
            let first = format!(
                "video channel: none -> {label} (carrier=unknown, frame_seq=1, held_ms=0, dropped=0, tcp_end={end})"
            );
            assert_eq!(out.matches(&first).count(), 1, "{relay:?}: {out}");
            assert_eq!(rig.delta(), Some((stream, 2)), "{relay:?}");
        }
    }

    /// FR-148 P4b-3b cell 5 — both relays: the agent's own leg over UDP
    /// (`turn_udp`) and the viewer's over TLS picks `video-deadline`,
    /// `tcp_end=viewer`; the agent's own over TLS and the viewer's over TCP is
    /// `tcp_end=both`. `carrier` stays the agent's own class.
    #[test]
    fn both_relays_pick_by_the_end_that_holds_the_tcp_leg() {
        for (class, relay, stream, end) in [
            (C::TurnUdp, Some("tls"), DL, "viewer"),
            (C::TurnUdp, Some("tcp"), DL, "viewer"),
            (C::TurnUdp, Some("udp"), BYTES, "none"),
            (C::TurnUdp, None, BYTES, "none"),
            (C::TurnTls, Some("tcp"), DL, "both"),
            (C::TurnTcp, Some("tls"), DL, "both"),
            (C::TurnTls, Some("udp"), DL, "agent"),
            (C::TurnTcp, None, DL, "agent"),
        ] {
            let (session, pump, mut rig) = ends(Mode::On);
            let (wrote, out) = logged(|| {
                report_and_read(&session, &pump, relay, class, true);
                rig.key()
            });
            assert_eq!(wrote, Some((stream, 1)), "{class:?} {relay:?}");
            let label = if stream == DL {
                DEADLINE_LABEL
            } else {
                BYTES_LABEL
            };
            let first = format!(
                "video channel: none -> {label} (carrier={}, frame_seq=1, held_ms=0, dropped=0, tcp_end={end})",
                class.word()
            );
            assert_eq!(out.matches(&first).count(), 1, "{class:?} {relay:?}: {out}");
        }
    }

    /// FR-148 P4b-3b cell 6 — a pair that rides the mesh ignores the report,
    /// and the agent's own read decides: `mesh_direct` with a report of `tcp`
    /// or `tls` stays on `video-bytes`, `mesh_relay` takes `video-deadline`
    /// whatever the report says, and a mesh pair the overlay said nothing
    /// about (`unknown`) stays on `video-bytes`.
    #[test]
    fn on_the_mesh_the_report_is_ignored() {
        for relay in [None, Some("udp"), Some("tcp"), Some("tls")] {
            for (class, stream, end) in [
                (C::MeshDirect, BYTES, "none"),
                (C::MeshRelay, DL, "agent"),
                (C::Unknown, BYTES, "none"),
            ] {
                let (session, pump, mut rig) = ends(Mode::On);
                let (wrote, out) = logged(|| {
                    report_and_read(&session, &pump, relay, class, false);
                    rig.key()
                });
                assert_eq!(wrote, Some((stream, 1)), "{class:?} {relay:?}");
                let tail = format!("dropped=0, tcp_end={end})");
                assert_eq!(out.matches(&tail).count(), 1, "{class:?} {relay:?}: {out}");
            }
        }
    }

    /// FR-148 P4b-3b cell 6 — with no pair selected the agent cannot say the
    /// pair is off the mesh, so a report waits for the re-check that reads
    /// one: the class is `unknown` and the end `none`, whatever the report.
    #[tokio::test]
    async fn with_no_pair_selected_the_report_waits_for_one() {
        let pc = webrtc::api::APIBuilder::new()
            .build()
            .new_peer_connection(Default::default())
            .await
            .expect("a peer connection");
        for mode in [Mode::Shadow, Mode::On] {
            let id = ObjectId::new();
            let session = Session::register(id, Arc::new(AtomicBool::new(false)));
            let counters = Arc::new(Counters::new(mode));
            let pump = Pump::start(id, &pc, &counters).await;
            session
                .viewer_relay()
                .note(&decodestat(Some(serde_json::json!("tls"))));
            assert!(session.viewer_relay().newest().is_some(), "{mode:?}: kept");
            pump.read(&pc).await;
            let read = read_of(pump.class.load(Relaxed));
            assert_eq!(read, (C::Unknown, TcpEnd::None), "{mode:?}");
        }
        pc.close().await.expect("closed");
    }

    /// FR-148 P4b-3b cell 7 — a report that changes mid-session moves the
    /// session as a class change does (P4b-3, D57): nothing more on the old
    /// channel until it drained, then a key frame asked, the frames before it
    /// dropped, and the key frame whole on the new channel, over one
    /// `frame_seq` space. A report that goes from `tls` to absent switches
    /// back the same way.
    #[test]
    fn a_report_that_changes_mid_session_switches_at_a_key_frame() {
        let (session, pump, mut rig) = ends(Mode::On);
        pump.note(C::Unknown, true);
        assert_eq!(rig.key(), Some((BYTES, 1)));
        for n in 2..=4 {
            assert_eq!(rig.delta(), Some((BYTES, n)));
        }
        rig.fake.st().buffered.insert(BYTES, 9_000);
        let (wrote, out) = logged(|| {
            report_and_read(&session, &pump, Some("tls"), C::Unknown, true);
            // 5, then 6: a key frame while `video-bytes` holds bytes.
            let mut wrote = vec![rig.delta(), rig.key()];
            rig.fake.st().buffered.insert(BYTES, 0);
            // 7: drained, a key frame asked; 8; 9: the key frame.
            wrote.extend([rig.delta(), rig.delta(), rig.key(), rig.delta()]);
            wrote
        });
        assert_eq!(
            wrote,
            [None, None, None, None, Some((DL, 9)), Some((DL, 10))]
        );
        assert_eq!(rig.fake.st().asks, 1, "one ask, after the drain");
        let line = "video channel: video-bytes -> video-deadline \
                    (carrier=unknown, frame_seq=9, held_ms=40, dropped=4, tcp_end=viewer)";
        assert_eq!(out.matches(line).count(), 1, "{out}");
        assert!(out.contains("drained_ms=20"), "{out}");

        rig.fake.st().buffered.insert(DL, 3_000);
        let (wrote, out) = logged(|| {
            report_and_read(&session, &pump, None, C::Unknown, true);
            let mut wrote = vec![rig.key()];
            rig.fake.st().buffered.insert(DL, 0);
            wrote.extend([rig.delta(), rig.key(), rig.delta()]);
            wrote
        });
        assert_eq!(wrote, [None, None, Some((BYTES, 13)), Some((BYTES, 14))]);
        assert_eq!(rig.fake.st().asks, 2);
        let line = "video channel: video-deadline -> video-bytes \
                    (carrier=unknown, frame_seq=13, held_ms=20, dropped=2, tcp_end=none)";
        assert_eq!(out.matches(line).count(), 1, "{out}");
        assert_eq!(
            rig.lane.frame_seq, 14,
            "one number a frame, written or dropped"
        );
        let line = rig.counters.take(None).unwrap();
        assert_eq!((line.switches, line.drained, line.late), (2, 6, 0));
        assert_eq!(line.channel, Some(Channel::Bytes));
    }

    /// FR-148 P4b-3b cell 8 — under `off` the pump finds no session, so the
    /// session keeps nothing, the pump reads nothing and writes no line, and
    /// every frame goes as P4b-3's `off` sends it. Under `shadow` the shadow
    /// lane picks with the report as `on` would and its line says so, while
    /// every write goes as under `off`: no drain, no mark, no ask.
    #[tokio::test]
    async fn off_keeps_nothing_and_shadow_shows_the_switch_the_report_causes() {
        let pc = webrtc::api::APIBuilder::new()
            .build()
            .new_peer_connection(Default::default())
            .await
            .expect("a peer connection");
        let (session, pump, mut rig) = ends(Mode::Off);
        assert!(pump.session.is_none(), "off finds no session");
        let ((), out) = logged(|| {
            session
                .viewer_relay()
                .note(&decodestat(Some(serde_json::json!("tls"))));
            futures::executor::block_on(pump.read(&pc));
        });
        assert_eq!(session.viewer_relay().newest(), None, "nothing kept");
        assert_eq!(pump.class.load(Relaxed), C::Unknown.code(), "nothing read");
        rig.class(C::TurnTcp);
        let (wrote, more) = logged(|| {
            (0..40_u64)
                .map(|i| rig.frame(i % 10 == 0, i * 40))
                .collect::<Vec<_>>()
        });
        let today: Vec<_> = (1..=40).map(|n| Some((BYTES, n))).collect();
        assert_eq!(wrote, today);
        assert_eq!(format!("{out}{more}"), "", "no line under off");
        assert_eq!(rig.counters.take(None), None);
        pc.close().await.expect("closed");

        let (session, pump, mut rig) = ends(Mode::Shadow);
        pump.note(C::Unknown, true);
        assert_eq!(
            (rig.key(), rig.delta()),
            (Some((BYTES, 1)), Some((BYTES, 2)))
        );
        rig.fake.st().buffered.insert(BYTES, 9_000);
        let (wrote, out) = logged(|| {
            report_and_read(&session, &pump, Some("tls"), C::Unknown, true);
            vec![rig.delta(), rig.key(), rig.delta()]
        });
        assert_eq!(
            wrote,
            [Some((BYTES, 3)), Some((BYTES, 4)), Some((BYTES, 5))],
            "off's writes"
        );
        assert!(
            session.viewer_relay().newest().is_some(),
            "kept under shadow"
        );
        let line = "video channel (shadow): video-bytes -> video-deadline \
                    (carrier=unknown, frame_seq=4, held_ms=10, dropped=0, tcp_end=viewer)";
        assert_eq!(out.matches(line).count(), 1, "{out}");
        assert!(rig.fake.st().marks.is_empty(), "no mark");
        assert_eq!(rig.fake.st().asks, 0, "no ask");
        let line = rig.counters.take(None).unwrap();
        assert_eq!((line.switches, line.drained), (1, 0));
        assert_eq!(line.channel, Some(Channel::Deadline), "on's channel");
    }

    /// FR-148 P4b-3b cell 9 — the lines. The class line keeps `from` and
    /// `to`, the agent's own class, and carries `viewer_relay` and `tcp_end`;
    /// it is written when the class, the report or the end changes, and a
    /// report repeated unchanged writes nothing. The "no video-deadline is
    /// open" line carries `tcp_end` as a field.
    #[test]
    fn the_lines_name_the_end_that_holds_the_tcp_leg() {
        let (session, pump, _rig) = ends(Mode::On);
        let class_line = |from: &str, to: &str, relay: &str, end: &str| {
            format!(
                "video channel: carrier class (FR-148 P4b-3) session={} from=\"{from}\" \
                 to=\"{to}\" viewer_relay=\"{relay}\" tcp_end=\"{end}\"",
                pump.session_id
            )
        };
        let ((), out) = logged(|| pump.note(C::Unknown, true));
        assert_eq!(out, "", "nothing changed");
        for (relay, class, off_mesh, want) in [
            (
                Some("tls"),
                C::Unknown,
                true,
                Some(("unknown", "unknown", "tls", "viewer")),
            ),
            (Some("tls"), C::Unknown, true, None),
            (
                Some("tls"),
                C::TurnTls,
                true,
                Some(("unknown", "turn_tls", "tls", "both")),
            ),
            (
                Some("udp"),
                C::TurnTls,
                true,
                Some(("turn_tls", "turn_tls", "udp", "agent")),
            ),
            (Some("udp"), C::TurnTls, true, None),
            (
                Some("tls"),
                C::Unknown,
                true,
                Some(("turn_tls", "unknown", "tls", "viewer")),
            ),
            // The pair moved onto the mesh, its class still `unknown`.
            (
                Some("tls"),
                C::Unknown,
                false,
                Some(("unknown", "unknown", "tls", "none")),
            ),
            (
                None,
                C::Unknown,
                false,
                Some(("unknown", "unknown", "none", "none")),
            ),
        ] {
            let ((), out) = logged(|| {
                for _ in 0..5 {
                    report_and_read(&session, &pump, relay, class, off_mesh);
                }
            });
            match want {
                Some((from, to, relay, end)) => {
                    let line = class_line(from, to, relay, end);
                    assert_eq!(out.matches(&line).count(), 1, "{line}\n{out}");
                    assert_eq!(out.matches("carrier class").count(), 1, "{out}");
                }
                None => assert_eq!(out, "", "an unchanged report"),
            }
        }

        let (session, pump, mut rig) = ends(Mode::On);
        rig.fake.st().deadline = None;
        let (wrote, out) = logged(|| {
            report_and_read(&session, &pump, Some("tls"), C::Unknown, true);
            (0..20_u64)
                .map(|i| rig.frame(i % 10 == 0, 0))
                .collect::<Vec<_>>()
        });
        let today: Vec<_> = (1..=20).map(|n| Some((BYTES, n))).collect();
        assert_eq!(wrote, today, "none open: video-bytes");
        let none_open = "video channel: the carrier is TCP somewhere and no video-deadline \
                         is open; video-bytes (FR-148 P4b-3)";
        assert_eq!(out.matches(none_open).count(), 1, "{out}");
        assert!(
            out.contains("carrier=\"unknown\" framed=true tcp_end=\"viewer\""),
            "{out}"
        );
    }

    /// FR-148 P4b-3b cell 10 — an older viewer, one that never sends `relay`,
    /// gets P4b-3's picks and lines: over five seeds of random class
    /// sequences, on and off the mesh, with key frames, ages, buffered
    /// amounts, a `video-deadline` that comes and goes and the older viewer's
    /// own `rc:decodestat`s, a lane fed through the report's whole road (the
    /// session's slot and the pump's reads) writes what a lane fed the class
    /// alone writes, as P4b-3's pump stored it, frame for frame, and logs the
    /// same lines. Each `tcp_end` there reads `agent` or `none`, as the class
    /// is TCP or not, so a line with it cut is P4b-3's; and the class line
    /// comes exactly when the class changes, with `viewer_relay="none"`.
    #[test]
    fn an_older_viewer_gets_p4b_3_s_picks_and_lines() {
        #[derive(Clone, Copy)]
        enum Do {
            Read(CarrierClass, bool),
            Decodestat(u64),
            Frame {
                key: bool,
                age_ms: u64,
                bytes: bool,
                deadline: bool,
                buffered: (usize, usize),
            },
        }
        fn script(seed: u64) -> Vec<Do> {
            let mut state = seed;
            let mut next = move || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            let mut deadline = true;
            (0..2_000)
                .map(|_| {
                    let r = next();
                    match r % 23 {
                        0 => Do::Read(C::ALL[(r >> 8) as usize % C::ALL.len()], (r >> 16) % 2 == 0),
                        1 => Do::Decodestat(r >> 8),
                        _ => {
                            if (r >> 12) % 150 == 0 {
                                deadline = !deadline;
                            }
                            Do::Frame {
                                key: (r >> 44) % 25 == 0,
                                age_ms: (r >> 54) % 700,
                                bytes: (r >> 20) % 150 != 0,
                                deadline,
                                buffered: (
                                    ((r >> 28) % 3) as usize * 1_000,
                                    ((r >> 36) % 2) as usize * 500,
                                ),
                            }
                        }
                    }
                })
                .collect()
        }
        /// The older viewer's `rc:decodestat`: today's keys, never `relay`.
        fn older(r: u64) -> serde_json::Value {
            let mut val = serde_json::json!({ "t": "rc:decodestat", "fps": r % 61, "struggling": r.is_multiple_of(3), "de": 1 });
            if r.is_multiple_of(2) {
                val["rx_bps"] = (r % 9_000_000).into();
                val["queue_ms"] = ((r % 200) as i64 - 100).into();
            }
            val
        }
        /// One run of the script: what each frame wrote, and the log.
        fn run(
            mode: Mode,
            seed: u64,
            through_the_report: bool,
        ) -> (Vec<Option<(u16, u32)>>, String) {
            let (session, pump, mut rig) = ends(mode);
            logged(|| {
                let mut wrote = Vec::new();
                for step in script(seed) {
                    match step {
                        Do::Read(class, off_mesh) if through_the_report => {
                            pump.note(class, off_mesh)
                        }
                        Do::Read(class, _) => rig.class(class),
                        Do::Decodestat(r) if through_the_report => {
                            session.viewer_relay().note(&older(r))
                        }
                        Do::Decodestat(_) => {}
                        Do::Frame {
                            key,
                            age_ms,
                            bytes,
                            deadline,
                            buffered,
                        } => {
                            {
                                let mut s = rig.fake.st();
                                s.bytes = bytes.then_some(BYTES);
                                s.deadline = deadline.then_some(DL);
                                s.buffered.insert(BYTES, buffered.0);
                                s.buffered.insert(DL, buffered.1);
                            }
                            wrote.push(rig.frame(key, age_ms));
                        }
                    }
                }
                wrote
            })
        }
        let (mut switches, mut viewer_lines) = (0, 0);
        for mode in [Mode::On, Mode::Shadow] {
            for seed in [1_u64, 2, 3, 0x0d58, 0x5eed_f00d] {
                let (wrote, log) = run(mode, seed, true);
                let (today, today_log) = run(mode, seed, false);
                assert_eq!(wrote, today, "{mode:?} seed {seed}: the picks");
                // Each line from its message on: the fmt layer's timestamp
                // differs between the two runs. FR-148 P4b-5a — the lane's
                // wake lines are its lines too: a second of frames that a
                // switch or D dropped is a still stretch.
                let message = |l: &str| {
                    let at = l.find("video channel").or_else(|| l.find("path wake"));
                    l[at.expect(l)..].to_owned()
                };
                let (class_lines, lane): (Vec<&str>, Vec<&str>) = log
                    .lines()
                    .partition(|l| l.contains("video channel: carrier class"));
                assert_eq!(
                    lane.iter().map(|l| message(l)).collect::<Vec<_>>(),
                    today_log.lines().map(message).collect::<Vec<_>>(),
                    "{mode:?} seed {seed}: the lines"
                );
                for l in &lane {
                    if let Some((_, rest)) = l.split_once("(carrier=") {
                        let class = rest.split(',').next().unwrap();
                        let tcp = C::ALL.iter().find(|c| c.word() == class).unwrap().is_tcp();
                        let end = if tcp { "agent" } else { "none" };
                        assert!(l.contains(&format!(", tcp_end={end})")), "{l}");
                        switches += 1;
                    } else if l.contains("no video-deadline is open") {
                        assert!(l.contains("tcp_end=\"agent\""), "{l}");
                    }
                }
                let mut was = C::Unknown;
                let mut changes = 0;
                for step in script(seed) {
                    if let Do::Read(class, _) = step {
                        changes += usize::from(class != was);
                        was = class;
                    }
                }
                assert_eq!(class_lines.len(), changes, "{mode:?} seed {seed}");
                for l in &class_lines {
                    assert!(l.contains("viewer_relay=\"none\""), "{l}");
                    let to = l.split("to=\"").nth(1).unwrap().split('"').next().unwrap();
                    let tcp = C::ALL.iter().find(|c| c.word() == to).unwrap().is_tcp();
                    let end = if tcp { "agent" } else { "none" };
                    assert!(l.contains(&format!("tcp_end=\"{end}\"")), "{l}");
                    viewer_lines += usize::from(l.contains("tcp_end=\"viewer\""));
                }
            }
        }
        assert!(switches > 0, "the scripts switch");
        assert_eq!(viewer_lines, 0);
    }

    /// FR-148 P4b-3b — the control channel hands every `rc:decodestat` to the
    /// slot its pumps read: the session's, which `AgentPeer::new` registered,
    /// given to the handler beside the session's gate. The arm notes the
    /// message after everything it did before, with no way out before it, so
    /// one without `relay` clears the report.
    #[test]
    fn the_control_channel_notes_each_decodestat_in_the_session_s_slot() {
        let src = peer_rs();
        let handler = body_of(&src, "fn attach_control_handler(");
        let params = &handler[..handler.find(") {\n").unwrap()];
        let gate = params.find("keyframe_gate: Arc<crate::media_deadline::KeyframeGate>,");
        let slot = params.find("viewer_relay: Arc<crate::media_deadline::ViewerRelayReport>,");
        assert!(gate.unwrap() < slot.unwrap(), "{params}");
        let arm = &handler[handler.find("\"rc:decodestat\" => {").unwrap()..];
        let arm = &arm[..arm.find("\"rc:display-match\" => {").unwrap()];
        assert_eq!(arm.matches("viewer_relay.note(&val);").count(), 1, "{arm}");
        let note = arm.find("viewer_relay.note(&val);").unwrap();
        assert!(arm.find("note_viewer_decodestat(").unwrap() < note, "{arm}");
        assert!(!arm.contains("return"), "{arm}");
        for (wiring, n) in [
            ("let deadline_stash = deadline_for_callback.clone();", 1),
            (
                "keyframe_for_dc,\n                            deadline_stash.viewer_relay(),\n",
                1,
            ),
            ("viewer_relay()", 1),
        ] {
            assert_eq!(src.matches(wiring).count(), n, "{wiring}");
        }
    }

    /// FR-148 P4b-3b on P4b-4b (R1, C1) — `ends` under `mode`, wired as a live
    /// session is: the lane's asks and frames reach the session's gate, as
    /// `SessionWire` takes them there, and the rule's rate is 2 Mbit/s, handed
    /// over through the pump's counters as each frame's budget step hands it.
    /// The pump's first read is made: a pair off the mesh whose agent-side
    /// class is `turn_udp`, before any report. Under `off`, `Pump::read` reads
    /// nothing.
    fn ends_on_turn_udp(mode: Mode) -> (Arc<Session>, Pump, Rig) {
        let (session, pump, rig) = ends(mode);
        rig.fake.st().gate = Some((session.gate(), pump.session_id));
        rig.counters.note_rate(Some(2_000_000), None);
        if mode != Mode::Off {
            pump.note(C::TurnUdp, true);
        }
        (session, pump, rig)
    }

    /// FR-148 P4b-3b with P4b-4 and P4b-4b (R1) — where the steps meet: the
    /// viewer's report puts the session in the key frame rule's scope. The
    /// pump forces a key frame, 80 KB, at the frame after each grant. The gate
    /// grants the viewer's `rc:keyframe`, and the session's first key frame
    /// goes on `video-bytes`: the pump read `turn_udp` off the mesh, and no
    /// report. 100 ms on, the viewer reports `tls` and the pump reads it.
    /// Under `on` that is TCP somewhere, at the viewer's end (D58), so the
    /// lane switches to `video-deadline`, drained at once. The key frame it
    /// switches at is not recovery (P4b-4b): its asks are refused until
    /// rc.130's 500 ms gap from the viewer's grant has passed, and that key
    /// frame goes whole on `video-deadline` 500 ms after the first. Written,
    /// it restarts the rule's clock, and the lane hands the gate the rule's
    /// scope with each frame. The viewer's ask 590 ms after it, past the
    /// 500 ms gap and inside the rule's 1.28 s at 2 Mbit/s, is the rule's
    /// (D55's Recovery): owed and not granted, counted `refused_rule`, and
    /// granted at the first frame once the rule's gap has opened. Under `off`
    /// the report is never kept, and the ask passes rc.130's gap. Under
    /// `shadow` the same traffic grants what `off` grants, the shadow lane
    /// says the switch `on` would make, and the world `on` would have had
    /// owes the ask.
    #[test]
    fn the_viewer_s_report_puts_the_session_in_the_key_frame_rule_s_scope() {
        /// One run of the traffic under a value of the switch.
        struct Run {
            /// Each key frame the pump forced: when, from the session's first,
            /// and its channel and number.
            keys: Vec<(Duration, Option<(u16, u32)>)>,
            /// The viewer's ask 1.09 s after the first key frame: whether it
            /// passed.
            granted: bool,
            /// Who owes it then: the session's world, and the shadow world.
            owed: (bool, bool),
            /// N12's split of the session's asks.
            split: KeyframeSplit,
            /// Why the lane asked, ask by ask.
            whys: Vec<LaneAsk>,
            /// The pump's counts on the `path delivery` line, at the end.
            line: Option<DeadlineLine>,
            /// What the run logged.
            said: String,
        }
        let ms = Duration::from_millis;
        let run = |mode: Mode| {
            let (session, pump, mut rig) = ends_on_turn_udp(mode);
            let (id, gate) = (pump.session_id, session.gate());
            let flag = Arc::clone(&gate.requested);
            let ((keys, granted, owed), said) = logged(|| {
                assert!(gate.ask_at(id, rig.t), "the viewer's first ask");
                let t0 = rig.t + ms(10);
                let mut keys = Vec::new();
                // The pump's frames until the next is due at `at`: a key
                // frame, 80 KB, at the frame after each grant.
                let mut frames_until = |rig: &mut Rig, at: Instant| {
                    while rig.t + ms(10) < at {
                        let key = flag.swap(false, Relaxed);
                        let wrote = rig.sized(key, 0, if key { 80_000 } else { 1_000 });
                        if key {
                            keys.push((rig.t - t0, wrote));
                        }
                    }
                };
                frames_until(&mut rig, t0 + ms(100));
                // The viewer's report, and the pump's read of it with the
                // class.
                session
                    .viewer_relay()
                    .note(&decodestat(Some(serde_json::json!("tls"))));
                if mode != Mode::Off {
                    pump.note(C::TurnUdp, true);
                }
                frames_until(&mut rig, t0 + ms(1_100));
                // The viewer's `rc:keyframe`: under `on`, 590 ms after the
                // switch's key frame.
                let granted = gate.ask_at(id, rig.t);
                let owed = {
                    let st = gate.state.lock().unwrap();
                    (st.real.owed.is_some(), st.shadow.owed.is_some())
                };
                frames_until(&mut rig, t0 + ms(2_000));
                (keys, granted, owed)
            });
            Run {
                keys,
                granted,
                owed,
                split: split(id),
                whys: rig.fake.st().whys.clone(),
                line: rig.counters.take(None),
                said,
            }
        };

        let on = run(Mode::On);
        assert!(
            !on.granted,
            "on: the report's switch puts the gate in the rule's scope, and after the \
             switch's key frame the rule owes the viewer's ask; key frames {:?}\n{}",
            on.keys, on.said
        );
        assert_eq!(on.owed, (true, false), "on: the session's world owes it");
        let read = "from=\"turn_udp\" to=\"turn_udp\" viewer_relay=\"tls\" tcp_end=\"viewer\"";
        assert_eq!(on.said.matches(read).count(), 1, "{}", on.said);
        let first = "video channel: none -> video-bytes (carrier=turn_udp, frame_seq=1, \
                     held_ms=0, dropped=0, tcp_end=none)";
        assert_eq!(on.said.matches(first).count(), 1, "{}", on.said);
        let switch = "video channel: video-bytes -> video-deadline (carrier=turn_udp, \
                      frame_seq=51, held_ms=400, dropped=40, tcp_end=viewer)";
        assert_eq!(on.said.matches(switch).count(), 1, "{}", on.said);
        // The switch's key frame at rc.130's gap from the viewer's grant; the
        // owed ask's at the frame after the rule's gap from it opened, 1.28 s
        // on.
        assert_eq!(
            on.keys,
            [
                (ms(0), Some((BYTES, 1))),
                (ms(500), Some((DL, 51))),
                (ms(500 + 1_290), Some((DL, 180))),
            ]
        );
        assert_eq!(on.whys, [LaneAsk::Switch; 40], "the switch's own asks");
        // The viewer's two asks; the switch's 40, 39 refused by the gap; the
        // viewer's second refused by the rule, owed, and granted.
        let on_asks = KeyframeSplit {
            viewer: 2,
            lane: 40,
            refused_gap: 39,
            refused_rule: 1,
            owed_granted: 1,
        };
        assert_eq!(on.split, on_asks);
        let line = on.line.expect("a line under on");
        assert_eq!((line.asks, line.kf_gap_ms), (on_asks, Some(1_280)));
        assert_eq!((line.switches, line.drained, line.late), (1, 40, 0));
        assert_eq!(line.channel, Some(Channel::Deadline));

        let off = run(Mode::Off);
        assert!(off.granted, "off: rc.130's 500 ms gap alone");
        assert_eq!(off.owed, (false, false));
        assert_eq!(
            off.keys,
            [(ms(0), Some((BYTES, 1))), (ms(1_100), Some((BYTES, 111)))]
        );
        let asked = KeyframeSplit {
            viewer: 2,
            ..KeyframeSplit::default()
        };
        assert_eq!(off.split, asked);
        assert!(off.whys.is_empty());
        assert!(off.line.is_none(), "no line under off");
        assert!(!off.said.contains("video channel"), "{}", off.said);

        let shadow = run(Mode::Shadow);
        assert_eq!(
            (shadow.granted, &shadow.keys),
            (off.granted, &off.keys),
            "shadow grants what off grants"
        );
        assert_eq!(
            shadow.owed,
            (false, true),
            "and the shadow world owes the ask"
        );
        let switch = "video channel (shadow): video-bytes -> video-deadline \
                      (carrier=turn_udp, frame_seq=111, held_ms=1000, dropped=0, tcp_end=viewer)";
        assert_eq!(shadow.said.matches(switch).count(), 1, "{}", shadow.said);
        assert!(shadow.whys.is_empty(), "the shadow lane asks nothing");
        let counted = KeyframeSplit {
            viewer: 2,
            refused_rule: 1,
            ..KeyframeSplit::default()
        };
        assert_eq!(shadow.split, counted, "N12 counts the rule's refusal");
        let line = shadow.line.expect("a line under shadow");
        assert_eq!((line.asks, line.kf_gap_ms), (counted, Some(1_280)));
    }

    /// FR-148 P4b-3b with P4b-4b (C1) — P4b-4b inside the switch P4b-3b's
    /// report causes: the switch's own asks are never the key frame rule's,
    /// and D's late-frame ask in `Steady` on `video-deadline` after it is.
    /// R1's set-up under `on`: the gate grants the viewer's `rc:keyframe`
    /// 10 ms before the session's first key frame, 80 KB on `video-bytes`,
    /// whose rule's gap is 1.28 s at 2 Mbit/s, and 100 ms on the viewer
    /// reports `tls`, which the pump reads. The switch to `video-deadline`
    /// drains at once, then asks at each frame and drops it. rc.130's gap
    /// refuses each ask until 500 ms after the viewer's grant, and each
    /// refusal is forgotten, never owed; then an ask passes, and the key frame
    /// it brings goes whole on `video-deadline`, its line naming the viewer's
    /// end. D then drops a frame 590 ms after that key frame and asks: past
    /// the 500 ms gap and inside the rule's, the ask is owed, counted
    /// `refused_rule`, and granted at the first frame once the rule's gap
    /// from the switch's key frame has opened. Were the switch's asks
    /// recovery, as on P4b-4, its first would be owed, and the switch would
    /// drop every frame until the rule's gap from the first key frame opened,
    /// 1.28 s on.
    #[test]
    fn the_report_s_switch_keys_at_the_500_ms_gap_and_d_s_ask_after_it_is_the_rule_s() {
        let ms = Duration::from_millis;
        let (session, pump, mut rig) = ends_on_turn_udp(Mode::On);
        let (id, gate) = (pump.session_id, session.gate());
        let flag = Arc::clone(&gate.requested);
        assert!(gate.ask_at(id, rig.t), "the viewer's first ask");
        assert!(flag.swap(false, Relaxed));
        assert_eq!(rig.sized(true, 0, 80_000), Some((BYTES, 1)));
        let t0 = rig.t;
        assert_eq!(gate.gap(), ms(1_280), "80 KB at 2 Mbit/s");
        rig.deltas_until(t0 + ms(100));
        report_and_read(&session, &pump, Some("tls"), C::TurnUdp, true);
        // The switch: drained at once, it asks at each frame until an ask
        // passes, and drops every frame meanwhile.
        let mut owed = Vec::new();
        while !flag.load(Relaxed) && rig.t < t0 + ms(3_000) {
            assert_eq!(rig.delta(), None, "dropped while the switch waits");
            owed.push(gate.state.lock().unwrap().real.owed.is_some());
        }
        assert_eq!(
            rig.t - t0,
            ms(490),
            "the switch's own asks pass rc.130's gap from the viewer's grant, not the \
             rule's from the key frame; owed after each frame: {owed:?}"
        );
        assert_eq!(owed, [false; 40], "each refusal forgotten, never owed");
        assert_eq!(rig.fake.st().asks, 40, "asked again at each frame");
        assert!(flag.swap(false, Relaxed));
        let (wrote, out) = logged(|| rig.sized(true, 0, 80_000));
        assert_eq!(wrote, Some((DL, 51)), "{out}");
        let switch = "video channel: video-bytes -> video-deadline (carrier=turn_udp, \
                      frame_seq=51, held_ms=400, dropped=40, tcp_end=viewer)";
        assert_eq!(out.matches(switch).count(), 1, "{out}");
        // In `Steady` on `video-deadline`: D drops a frame 590 ms after the
        // switch's key frame, and asks.
        let t1 = rig.t;
        rig.deltas_until(t1 + ms(590));
        assert_eq!(rig.frame(false, 301), None, "D drops it, and asks");
        assert!(!flag.load(Relaxed), "D's ask is the rule's: not granted");
        assert!(gate.state.lock().unwrap().real.owed.is_some(), "owed");
        assert_eq!(split(id).refused_rule, 1, "refused by the rule");
        while !flag.load(Relaxed) {
            assert!(rig.delta().is_some());
            assert!(rig.t < t1 + ms(3_000), "never granted");
        }
        assert_eq!(rig.t - t1, ms(1_280), "granted once the rule's gap opens");
        let mut whys = vec![LaneAsk::Switch; 40];
        whys.push(LaneAsk::Late);
        assert_eq!(rig.fake.st().whys, whys);
        let asks = KeyframeSplit {
            viewer: 1,
            lane: 41,
            refused_gap: 39,
            refused_rule: 1,
            owed_granted: 1,
        };
        assert_eq!(split(id), asks);
        flag.store(false, Relaxed);
        assert_eq!(rig.sized(true, 0, 80_000), Some((DL, 180)));
        let line = rig.counters.take(None).unwrap();
        assert_eq!((line.late, line.drained, line.switches), (1, 40, 1));
        assert_eq!((line.asks, line.kf_gap_ms), (asks, Some(1_280)));
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
                asks: KeyframeSplit::default(),
                kf_gap_ms: None,
                // FR-148 P4b-5a: frame 7, a key frame the drain dropped. No
                // send task told the lane of a write.
                kf_written: (0, 0),
                kf_dropped: (0, 1),
                viewer_kf: None,
            })
        );
        let line = "video channel: video-bytes -> video-deadline \
                    (carrier=turn_tcp, frame_seq=11, held_ms=50, dropped=5, tcp_end=agent)";
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
    /// Refused, nothing switches: the session stays on `video-bytes` and one
    /// line says so. FR-148 P4b — the mark is tried again at each frame, and
    /// one accepted between key frames starts the switch (P4b-3 tried it
    /// again only at the next key frame).
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
        assert_eq!(rig.fake.st().marks, [DL; 7], "each frame, key or delta");
        assert_eq!(out.matches("was not marked").count(), 1, "{out}");
        assert_eq!(rig.counters.take(None).unwrap().drained, 0);
        rig.fake.st().mark_ok = true;
        assert_eq!(
            rig.delta(),
            None,
            "marked between key frames: the switch drops the delta and asks"
        );
        assert_eq!(rig.key(), Some((DL, 9)));
        assert_eq!(rig.fake.st().marks, [DL; 8]);
    }

    /// FR-148 P4b — a refused mark is tried again at the lane's next frame,
    /// key or delta, and at each one after until one is accepted, so a
    /// session whose encoder sends no key frame unasked (P4b-5's first `on`
    /// runs: none came in 240 s) still reaches `video-deadline`. Accepted on a
    /// delta, the session switches as any switch does (D57): `video-bytes`
    /// drains, and the switch asks its own key frame through the session's
    /// gate (FR-148 P4b-4b's [`LaneAsk::Switch`]), which goes whole on
    /// `video-deadline`. Here nothing is buffered and no ask passed before, so
    /// `video-deadline` carries the second frame after the cause of the
    /// refusals cleared: the first is dropped for the switch, and the second
    /// is the key frame its ask brought. The viewer asked for nothing. The ask
    /// is the lane's and never owed, the key frame it brought counts on
    /// `video-deadline` in P4b-5a's ledger, and it restarts the rule's clock
    /// (D61): the viewer's ask 490 ms after it is refused by the gap, and its
    /// ask 500 ms after it passes.
    #[test]
    fn a_refused_mark_reaches_video_deadline_with_no_key_frame_unasked() {
        let ms = Duration::from_millis;
        let mut rig = Rig::new(Mode::On);
        rig.class(C::TurnTcp);
        let (gate, flag, id) = rig.gated(Some(2_000_000));
        rig.fake.st().mark_ok = false;
        // The pump: a key frame at the session's first frame and at the frame
        // after each grant, a delta otherwise, each sent whole as the send
        // task sends it.
        let frame = |rig: &mut Rig, first: bool| {
            let key = flag.swap(false, Relaxed) || first;
            (key, rig.sent(key, 0, true))
        };
        let mut wrote = vec![frame(&mut rig, true)];
        wrote.extend((0..9).map(|_| frame(&mut rig, false)));
        let refused: Vec<_> = (1..=10).map(|n| (n == 1, Some((BYTES, n)))).collect();
        assert_eq!(wrote, refused, "refused: video-bytes, and no switch");
        // The cause clears.
        rig.fake.st().mark_ok = true;
        let wrote: Vec<_> = (0..4).map(|_| frame(&mut rig, false)).collect();
        assert_eq!(
            wrote,
            [
                (false, None),
                (true, Some((DL, 12))),
                (false, Some((DL, 13))),
                (false, Some((DL, 14)))
            ],
            "marked at the next frame, and the switch's own key frame on video-deadline"
        );
        let keyed = rig.t - ms(20);
        assert_eq!(
            rig.fake.st().marks,
            [DL; 11],
            "each frame until one was accepted, then none"
        );
        assert_eq!(rig.fake.st().whys, [LaneAsk::Switch]);
        let s = split(id);
        assert_eq!(
            (
                s.viewer,
                s.lane,
                s.refused_gap,
                s.refused_rule,
                s.owed_granted
            ),
            (0, 1, 0, 0, 0)
        );
        let line = rig.counters.take(None).unwrap();
        assert_eq!((line.switches, line.drained), (1, 1));
        assert_eq!(line.kf_dropped, (0, 0));
        assert_eq!(
            line.kf_written,
            (1, 1),
            "the session's first key frame on video-bytes, the switch's own on video-deadline"
        );
        assert!(
            !gate.ask_at(id, keyed + ms(490)),
            "the gap, from the key frame"
        );
        assert!(gate.ask_at(id, keyed + ms(500)));
    }

    /// FR-148 P4b — a refused mark's line names why it was refused and the
    /// stream, once: the lane's tries at the frames after it say nothing more.
    #[test]
    fn a_refused_mark_says_why_and_on_which_stream() {
        let mut rig = Rig::new(Mode::On);
        rig.class(C::TurnTcp);
        rig.fake.st().mark_ok = false;
        let (_, out) = logged(|| {
            rig.key();
            for _ in 0..5 {
                rig.delta();
            }
        });
        let said: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("was not marked"))
            .collect();
        assert_eq!(said.len(), 1, "{out}");
        assert!(said[0].contains("reason=no_stream"), "{out}");
        assert!(said[0].contains("stream=3"), "{out}");
    }

    /// FR-148 P4b — each reason a mark is refused for is said once, with the
    /// stream, until a mark is accepted; one accepted after refusals says how
    /// many there were; and after it, a refusal on another stream is said
    /// again.
    #[test]
    fn each_reason_is_said_once_until_a_mark_is_accepted() {
        let mut rig = Rig::new(Mode::On);
        rig.class(C::TurnTcp);
        let set = |rig: &Rig, why: DeadlineMarkRefusal| {
            let mut s = rig.fake.st();
            s.mark_ok = false;
            s.refusal = Some(why);
        };
        let (wrote, out) = logged(|| {
            set(&rig, DeadlineMarkRefusal::NoAssociation);
            let mut wrote = vec![rig.key(), rig.delta()];
            set(&rig, DeadlineMarkRefusal::SlotHeld);
            wrote.extend([rig.delta(), rig.delta()]);
            set(&rig, DeadlineMarkRefusal::NoAssociation);
            wrote.push(rig.delta());
            rig.fake.st().mark_ok = true;
            wrote.extend([rig.delta(), rig.key()]);
            // Off it, and back to another stream, refused.
            rig.class(C::Direct);
            wrote.extend([rig.delta(), rig.key()]);
            rig.fake.st().deadline = Some(DL + 2);
            set(&rig, DeadlineMarkRefusal::NoAssociation);
            rig.class(C::TurnTcp);
            wrote.push(rig.delta());
            wrote
        });
        let mut today: Vec<_> = (1..=5).map(|n| Some((BYTES, n))).collect();
        today.extend([
            None,
            Some((DL, 7)),
            None,
            Some((BYTES, 9)),
            Some((BYTES, 10)),
        ]);
        assert_eq!(wrote, today);
        assert_eq!(rig.fake.st().marks, [DL, DL, DL, DL, DL, DL, DL + 2]);
        let said: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("was not marked"))
            .collect();
        assert_eq!(said.len(), 3, "{out}");
        for (line, (why, stream)) in said.iter().zip([
            ("no_association", DL),
            ("slot_held", DL),
            ("no_association", DL + 2),
        ]) {
            assert!(line.contains(&format!("reason={why}")), "{line}");
            assert!(line.contains(&format!("stream={stream}")), "{line}");
        }
        let late: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("was marked after it was refused"))
            .collect();
        assert_eq!(late.len(), 1, "{out}");
        assert!(late[0].contains("stream=3"), "{out}");
        assert!(late[0].contains("refused=5"), "{out}");
    }

    /// [`logged`], for a future: what it logged while it was polled, beside
    /// its output. The tasks it spawned log to nobody.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test's own log capture, never a console (root clippy.toml, #3938)"
    )]
    async fn logged_async<T>(f: impl Future<Output = T>) -> (T, String) {
        use tracing::instrument::WithSubscriber;
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
        // Two dispatchers, as `logged` keeps them.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let out = f.with_subscriber(subscriber).await;
        let text = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        (out, text)
    }

    /// FR-148 P4b — a session's two ends, both webrtc-rs peers in this
    /// process, every candidate in the SDP, as `peer.rs`'s `Pair` joins a
    /// `files` channel. The viewer opens its channels in the browser's order
    /// and shapes (`useRemoteControl.ts`): `input`, unordered with no
    /// retransmits, then `control`, `video-bytes`, and `video-deadline`
    /// beside it, unordered with no retransmits. The daemon's end takes them
    /// through the vendored webrtc's accept path, into the stashes
    /// `AgentPeer`'s `on_data_channel` arms fill (`video-bytes` into the stash
    /// its pump reads, `video-deadline` into the session's,
    /// [`Session::stash`]). The mark's own cell in `media_delivery` joins two
    /// SCTP associations, and never reaches that accept path, where the id
    /// was lost.
    ///
    /// The daemon answers as the DTLS client, as today, or as the server
    /// (`daemon_server`), the viewer then the client, whose streams are even.
    /// A browser that is the DTLS client opens `input`, its first channel, on
    /// stream 0. A webrtc-rs client never opens a channel in band on stream 0
    /// (the channel's own placeholder id, 0, counts as taken in
    /// `generate_and_set_data_channel_id`), so there both ends open `input`
    /// negotiated on stream 0.
    struct Ends {
        viewer_pc: Arc<RTCPeerConnection>,
        daemon_pc: Arc<RTCPeerConnection>,
        session_id: ObjectId,
        _session: Arc<Session>,
        bytes: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
        /// The viewer's channels, label and id, in the order it opened them.
        viewer: Vec<(String, u16)>,
        /// The channels the daemon's handler saw, label and id there.
        accepted: Arc<Mutex<Vec<(String, u16)>>>,
    }

    impl Ends {
        async fn open(daemon_server: bool) -> Self {
            use webrtc::api::APIBuilder;
            use webrtc::api::setting_engine::SettingEngine;
            use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
            use webrtc::dtls_transport::dtls_role::DTLSRole;

            let session_id = ObjectId::new();
            let session = Session::register(session_id, Arc::new(AtomicBool::new(false)));
            let mut daemon_setting = SettingEngine::default();
            if daemon_server {
                daemon_setting
                    .set_answering_dtls_role(DTLSRole::Server)
                    .expect("a DTLS role");
            }
            let viewer_pc = Arc::new(
                APIBuilder::new()
                    .build()
                    .new_peer_connection(Default::default())
                    .await
                    .expect("the viewer's peer"),
            );
            let daemon_pc = Arc::new(
                APIBuilder::new()
                    .with_setting_engine(daemon_setting)
                    .build()
                    .new_peer_connection(Default::default())
                    .await
                    .expect("the daemon's peer"),
            );
            let unordered = |negotiated: Option<u16>| {
                Some(RTCDataChannelInit {
                    ordered: Some(false),
                    max_retransmits: Some(0),
                    negotiated,
                    ..Default::default()
                })
            };
            let stream_0 = daemon_server.then_some(0);
            if daemon_server {
                daemon_pc
                    .create_data_channel("input", unordered(stream_0))
                    .await
                    .expect("input");
            }
            let mut opened = vec![];
            for (label, init) in [
                ("input", unordered(stream_0)),
                ("control", None),
                (BYTES_LABEL, None),
                (DEADLINE_LABEL, unordered(None)),
            ] {
                let dc = viewer_pc
                    .create_data_channel(label, init)
                    .await
                    .expect("a channel");
                opened.push(dc);
            }

            let accepted: Arc<Mutex<Vec<(String, u16)>>> = Arc::default();
            let bytes: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>> = Arc::default();
            {
                let accepted = Arc::clone(&accepted);
                let bytes = Arc::clone(&bytes);
                let session = Arc::clone(&session);
                daemon_pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
                    accepted
                        .lock()
                        .unwrap()
                        .push((dc.label().to_owned(), dc.id()));
                    let bytes = Arc::clone(&bytes);
                    let session = Arc::clone(&session);
                    Box::pin(async move {
                        match dc.label() {
                            BYTES_LABEL => *bytes.lock().await = Some(dc.clone()),
                            DEADLINE_LABEL => session.stash(dc.clone()).await,
                            _ => {}
                        }
                    })
                }));
            }

            let offer = viewer_pc.create_offer(None).await.expect("an offer");
            let mut gathered = viewer_pc.gathering_complete_promise().await;
            viewer_pc
                .set_local_description(offer)
                .await
                .expect("the offer");
            let _ = gathered.recv().await;
            let offer = viewer_pc.local_description().await.expect("a local offer");
            daemon_pc
                .set_remote_description(offer)
                .await
                .expect("the offer");
            let answer = daemon_pc.create_answer(None).await.expect("an answer");
            let mut gathered = daemon_pc.gathering_complete_promise().await;
            daemon_pc
                .set_local_description(answer)
                .await
                .expect("the answer");
            let _ = gathered.recv().await;
            let answer = daemon_pc.local_description().await.expect("a local answer");
            viewer_pc
                .set_remote_description(answer)
                .await
                .expect("the answer");

            let open = |dc: Option<&Arc<RTCDataChannel>>| {
                dc.is_some_and(|dc| dc.ready_state() == RTCDataChannelState::Open)
            };
            let until = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                let both = open(bytes.lock().await.as_ref())
                    && open(session.deadline.lock().await.as_ref())
                    && opened.iter().all(|dc| open(Some(dc)));
                if both {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < until,
                    "the session's channels never opened"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let viewer = opened
                .iter()
                .map(|dc| (dc.label().to_owned(), dc.id()))
                .collect();
            Self {
                viewer_pc,
                daemon_pc,
                session_id,
                _session: session,
                bytes,
                viewer,
                accepted,
            }
        }

        /// The viewer's channels, label and id, in the order it opened them.
        fn viewer_ids(&self) -> Vec<(String, u16)> {
            self.viewer.clone()
        }

        /// The viewer's id of its channel `label`, and the id the daemon's
        /// handler saw for it.
        fn ids_of(&self, label: &str) -> (u16, Option<u16>) {
            let viewer = self.viewer.iter().find(|(l, _)| l == label).expect(label);
            let accepted = self.accepted.lock().unwrap();
            let seen = accepted.iter().find(|(l, _)| l == label).map(|(_, id)| *id);
            (viewer.1, seen)
        }

        /// The session's first frame, a key frame, under `on`, TCP at the
        /// daemon's end, as a send task takes it: the channel its lane put it
        /// on, the streams the association has marked then, and what the lane
        /// said.
        async fn first_key_frame(&self) -> (Channel, String, Option<Vec<u16>>, String) {
            let counters = Arc::new(Counters::new(Mode::On));
            let pump = Pump::start(self.session_id, &self.daemon_pc, &counters).await;
            pump.note(C::TurnTcp, false);
            let mut lane = pump.lane(&self.bytes, &self.daemon_pc, true);
            let key = crate::peer::frame_video_bytes(b"key", true, 1);
            let (out, said) = logged_async(lane.route(&key, Instant::now())).await;
            let out = out.expect("written");
            let wrote = (out.channel, out.dc.label().to_owned());
            drop(out);
            let marked = self.daemon_pc.sctp().deadline_streams().await;
            (wrote.0, wrote.1, marked, said)
        }

        /// Both ends closed. A close runs every step whatever it returns, and
        /// the second end's channels may find the association already gone
        /// (`sending reset packet in non-Established state`), so its answer is
        /// not read, as `peer_close_tests`' pairs do not read theirs.
        async fn close(self) {
            let _ = self.viewer_pc.close().await;
            let _ = self.daemon_pc.close().await;
        }
    }

    /// `(label, id)` pairs, owned, as [`Ends::viewer_ids`] gives them.
    fn named(ids: &[(&str, u16)]) -> Vec<(String, u16)> {
        ids.iter()
            .map(|&(label, id)| (label.to_owned(), id))
            .collect()
    }

    /// FR-148 P4b — the daemon the DTLS client, as it answers today: the
    /// viewer's channels are on odd streams, none of them 0. Its
    /// `video-deadline` comes through the accept path with its stream's id,
    /// the lane marks that stream and no other, and the session's first frame
    /// goes on `video-deadline`. P4b-5's first `on` runs: the mark asked for
    /// stream 0, which did not exist, and the session stayed on `video-bytes`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_mark_finds_the_viewer_s_video_deadline_when_the_daemon_is_the_dtls_client() {
        let ends = Ends::open(false).await;
        let viewer = ends.viewer_ids();
        let (deadline, accepted) = ends.ids_of(DEADLINE_LABEL);
        let (channel, label, marked, said) = ends.first_key_frame().await;
        let refused = said.matches("was not marked").count();
        ends.close().await;
        assert_eq!(
            viewer,
            named(&[
                ("input", 1),
                ("control", 3),
                (BYTES_LABEL, 5),
                (DEADLINE_LABEL, 7)
            ])
        );
        assert_eq!(
            (accepted, channel, label.as_str(), marked, refused),
            (
                Some(deadline),
                Channel::Deadline,
                DEADLINE_LABEL,
                Some(vec![deadline]),
                0
            ),
            "{said}"
        );
    }

    /// FR-148 P4b — the daemon the DTLS server, the viewer the client: the
    /// viewer's channels are on even streams, and stream 0 is `input`'s, its
    /// first. The lane marks `video-deadline`'s stream and leaves stream 0
    /// alone. Before, the mark asked for stream 0 and found `input`'s, so the
    /// session went on `video-deadline` with its stream unmarked (its chunks
    /// abandoned at their first send, FR-17's rule), and `input`'s stream was
    /// marked instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_mark_leaves_stream_0_alone_when_the_viewer_is_the_dtls_client() {
        let ends = Ends::open(true).await;
        let viewer = ends.viewer_ids();
        let (deadline, accepted) = ends.ids_of(DEADLINE_LABEL);
        let (channel, label, marked, said) = ends.first_key_frame().await;
        let refused = said.matches("was not marked").count();
        ends.close().await;
        assert_eq!(
            viewer,
            named(&[
                ("input", 0),
                ("control", 2),
                (BYTES_LABEL, 4),
                (DEADLINE_LABEL, 6)
            ])
        );
        assert_eq!(
            (accepted, channel, label.as_str(), marked, refused),
            (
                Some(deadline),
                Channel::Deadline,
                DEADLINE_LABEL,
                Some(vec![deadline]),
                0
            ),
            "{said}"
        );
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
                    futures::executor::block_on(lane.route_at(key, wire.len(), enqueued, now))
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

    /// FR-148 P4b-4 (D55) — the rule's gap is `max(MIN_KF_GAP, 8 × kf_bytes /
    /// (KF_SHARE × rate))`, bytes and bit/s: 80 KB at 2 Mbit/s, 1.28 s; 20 KB
    /// at 16 Mbit/s, 40 ms, so the 500 ms floor; no rate, or no key frame yet,
    /// 500 ms. At the gate, under `on` on `video-deadline`, it runs from the
    /// last key frame written: an ask a millisecond before it opens is
    /// refused, one as it opens passes.
    #[test]
    fn the_rule_s_gap_is_the_last_key_frame_at_a_quarter_of_the_rate() {
        assert_eq!(KF_SHARE, 0.25);
        let table = [
            (Some(80_000), Some(2_000_000), 1_280),
            (Some(20_000), Some(16_000_000), 500),
            (Some(250_000), Some(1_000_000), 8_000),
            (Some(80_000), None, 500),
            (Some(80_000), Some(0), 500),
            (None, Some(2_000_000), 500),
            (None, None, 500),
        ];
        for (bytes, rate, gap_ms) in table {
            let gap = Duration::from_millis(gap_ms);
            assert_eq!(kf_gap(bytes, rate), gap, "{bytes:?} {rate:?}");
            let (gate, flag, id) = gate_at(rate);
            let t0 = Instant::now();
            assert!(
                gate.ask_lane(id, t0, RULE, LaneAsk::Late),
                "the session's first ask"
            );
            assert!(flag.swap(false, Relaxed));
            if let Some(bytes) = bytes {
                gate.on_frame(id, t0, RULE, Some(bytes));
            }
            let early = t0 + gap - Duration::from_millis(1);
            assert!(!gate.ask_at(id, early), "{bytes:?} {rate:?}");
            assert!(!flag.load(Relaxed), "{bytes:?} {rate:?}");
            assert!(gate.ask_at(id, t0 + gap), "{bytes:?} {rate:?}");
            assert!(flag.load(Relaxed), "{bytes:?} {rate:?}");
        }
        assert_eq!(
            kf_gap(Some(u64::MAX), Some(1)),
            Duration::MAX,
            "no overflow"
        );
    }

    /// FR-148 P4b-4 (D59) — the rule's rate: the in-flight budget's, when
    /// this association's figures gave it, wins; the encoder's target stands
    /// in when the budget has none; with neither, the 500 ms floor. The
    /// pump's read reaches its session's gate from the pump's start under
    /// `shadow` and `on`, and never under `off`; the line's `kf_gap_ms` is the
    /// gap at that rate.
    #[tokio::test]
    async fn the_rule_s_rate_is_the_measured_budget_s_else_the_encoder_s_target() {
        assert_eq!(kf_rate(Some(3_200_000), Some(1_600_000)), Some(3_200_000));
        assert_eq!(kf_rate(None, Some(1_600_000)), Some(1_600_000));
        assert_eq!(kf_rate(Some(0), Some(1_600_000)), Some(1_600_000));
        assert_eq!(kf_rate(None, Some(0)), None);
        assert_eq!(kf_rate(None, None), None);
        let pc = webrtc::api::APIBuilder::new()
            .build()
            .new_peer_connection(Default::default())
            .await
            .expect("a peer connection");
        // 80 KB: 0.8 s at a measured 3.2 Mbit/s, 1.6 s at a 1.6 Mbit/s
        // target, the floor with neither.
        for (measured, target, gap_ms) in [
            (Some(3_200_000), Some(1_600_000), 800),
            (None, Some(1_600_000), 1_600),
            (None, None, 500),
        ] {
            for mode in [Mode::Off, Mode::Shadow, Mode::On] {
                let id = ObjectId::new();
                let session = Session::register(id, Arc::new(AtomicBool::new(false)));
                let counters = Arc::new(Counters::new(mode));
                let _pump = Pump::start(id, &pc, &counters).await;
                counters.note_rate(measured, target);
                let gate = session.gate();
                let want = match mode {
                    Mode::Off => None,
                    Mode::Shadow | Mode::On => kf_rate(measured, target),
                };
                assert_eq!(gate.rate(), want, "{mode:?} {measured:?} {target:?}");
                let t0 = Instant::now();
                let scope = Scope {
                    mode,
                    deadline: true,
                };
                gate.on_frame(id, t0, scope, Some(80_000));
                // The line's gap is the rule's at that rate; `off` has no line.
                let line = counters.take(None).map(|l| l.kf_gap_ms);
                let want_line = (mode != Mode::Off).then_some(Some(gap_ms));
                assert_eq!(line, want_line, "{mode:?} {measured:?} {target:?}");
                if mode == Mode::On {
                    let gap = Duration::from_millis(gap_ms);
                    assert!(!gate.ask_at(id, t0 + gap - Duration::from_millis(1)));
                    assert!(gate.ask_at(id, t0 + gap), "{measured:?} {target:?}");
                }
            }
        }
        pc.close().await.expect("closed");
    }

    /// FR-148 P4b-4 (D55, R5) — under `on`, on `video-deadline`: an ask the
    /// rule refuses is owed, never forgotten, and the lane's first frame
    /// after the gap opens grants it, with no further ask; another refused
    /// ask meanwhile, the viewer's, coalesces into it. P4b-3 forgot it.
    #[test]
    fn a_refused_ask_is_granted_at_the_first_frame_after_the_gap_opens() {
        let ms = Duration::from_millis;
        let mut rig = Rig::new(Mode::On);
        rig.class(C::TurnTcp);
        let (gate, flag, id) = rig.gated(Some(2_000_000));
        // 80 KB at 2 Mbit/s: 1.28 s.
        assert_eq!(rig.sized(true, 0, 80_000), Some((DL, 1)));
        let t0 = rig.t;
        assert_eq!(rig.frame(false, 301), None, "D drops it, and asks");
        assert_eq!(rig.fake.st().asks, 1);
        assert!(!flag.load(Relaxed), "refused: 10 ms after the key frame");
        rig.deltas_until(t0 + ms(600));
        assert!(
            !gate.ask_at(id, rig.t),
            "the viewer's, past 500 ms: the rule's"
        );
        let mut frames = 0;
        while !flag.load(Relaxed) {
            assert!(rig.delta().is_some());
            frames += 1;
            assert!(frames < 1_000, "never granted");
        }
        assert_eq!(rig.t - t0, ms(1_280), "the first frame after the gap opens");
        assert_eq!(rig.fake.st().asks, 1, "no further ask");
        let s = split(id);
        assert_eq!((s.lane, s.viewer), (1, 1));
        assert_eq!((s.refused_gap, s.refused_rule, s.owed_granted), (1, 1, 1));
        flag.store(false, Relaxed);
        for _ in 0..200 {
            assert!(rig.delta().is_some());
        }
        assert!(!flag.load(Relaxed), "one grant for the two asks");
    }

    /// FR-148 P4b-4 (D59, R4) — a key frame written restarts the rule's clock
    /// and answers the owed ask, whatever asked for it: the encoder's own,
    /// unasked; one a grant forced; one written on `video-bytes`. The gap is
    /// the last key frame's own size at the rate.
    #[test]
    fn a_key_frame_written_restarts_the_clock_and_clears_an_owed_ask() {
        let ms = Duration::from_millis;
        let mut rig = Rig::new(Mode::On);
        rig.class(C::TurnTcp);
        let (gate, flag, id) = rig.gated(Some(2_000_000));
        assert_eq!(rig.sized(true, 0, 80_000), Some((DL, 1)));
        let t0 = rig.t;
        assert_eq!(rig.frame(false, 400), None, "D: its ask is owed");
        // The encoder's own key frame, 1 s on, before the gap opens: written,
        // it answers the owed ask, and the gap runs from it.
        rig.deltas_until(t0 + ms(1_000));
        assert!(rig.sized(true, 0, 80_000).is_some());
        assert_eq!(rig.t, t0 + ms(1_000));
        rig.deltas_until(t0 + ms(1_600));
        assert!(
            !flag.load(Relaxed),
            "nothing owed once the first gap opened"
        );
        // 1.59 s after the first key frame, 590 ms after the second: refused.
        assert!(!gate.ask_at(id, rig.t), "the rule's, from the second");
        while !flag.load(Relaxed) {
            assert!(rig.delta().is_some());
        }
        assert_eq!(rig.t, t0 + ms(2_280), "1.28 s after the second");
        // The key frame the grant forced, 20 KB: the floor, from it.
        flag.store(false, Relaxed);
        assert!(rig.sized(true, 0, 20_000).is_some());
        let t2 = rig.t;
        rig.deltas_until(t2 + ms(490));
        assert_eq!(rig.frame(false, 400), None, "490 ms on: the gap's");
        assert!(!flag.load(Relaxed));
        assert!(rig.delta().is_some());
        assert!(flag.load(Relaxed), "500 ms on: 20 KB at 2 Mbit/s is 320 ms");
        assert_eq!(rig.t, t2 + ms(500));
        let s = split(id);
        assert_eq!((s.refused_gap, s.refused_rule, s.owed_granted), (2, 1, 2));

        // One written on `video-bytes` counts too: while a switch to
        // `video-deadline` drains, a recovery ask, the viewer's, 590 ms after
        // it is the rule's. (A switch's own ask never is: FR-148 P4b-4b.)
        let mut rig = Rig::new(Mode::On);
        rig.class(C::Direct);
        let (gate, flag, id) = rig.gated(Some(2_000_000));
        assert_eq!(rig.sized(true, 0, 80_000), Some((BYTES, 1)));
        let t0 = rig.t;
        rig.fake.st().buffered.insert(BYTES, 1);
        rig.class(C::TurnTcp);
        rig.deltas_until(t0 + ms(600));
        assert!(!gate.ask_at(id, rig.t), "590 ms after it");
        assert!(!flag.load(Relaxed));
        assert_eq!(split(id).refused_rule, 1, "the rule's, from it");
    }

    /// FR-148 P4b-4 (D55, AC24) — a steady stream of D drops: 60 s at 30
    /// frames a second, every frame late after the session's first key frame,
    /// each framed as the send task frames it (`frame_video_bytes`), and a key
    /// frame whenever the gate set the flag. Every late frame asks. The grants
    /// never come closer than the rule's gap from the last key frame written
    /// or the last grant, and each comes within one frame interval after the
    /// gap opens, answering an owed ask. P4b-3 granted one every 500 ms.
    #[test]
    fn under_steady_d_drops_the_grants_keep_the_rule_s_gap() {
        let interval = Duration::from_nanos(1_000_000_000 / 30);
        for (rate, first) in [
            (2_000_000_u64, 80_000_usize),
            (600_000, 40_000),
            (16_000_000, 20_000),
        ] {
            let mut rig = Rig::new(Mode::On);
            rig.class(C::TurnTcp);
            let (_gate, flag, id) = rig.gated(Some(rate));
            let t0 = rig.t;
            let mut gap = Duration::ZERO;
            let mut since = t0;
            let mut grants = 0_u32;
            for i in 0..=1_800_u32 {
                let now = t0 + interval * i;
                let key = i == 0 || flag.swap(false, Relaxed);
                let size = if key { first } else { 2_000 };
                let payload: Vec<u8> = (0..size).map(|b| (b ^ i as usize) as u8).collect();
                let wire = crate::peer::frame_video_bytes(&payload, key, u64::from(i));
                let age = Duration::from_millis(if i == 0 { 0 } else { 400 });
                let keyframe = crate::peer::video_bytes_is_keyframe(&wire);
                let routed = rig.lane.route_at(keyframe, wire.len(), now - age, now);
                let out = futures::executor::block_on(routed).map(|r| r.dc);
                if i == 0 {
                    assert_eq!(out, Some(DL));
                    gap = kf_gap(Some(wire.len() as u64), Some(rate));
                    continue;
                }
                assert_eq!(out, None, "frame {i}: late");
                if flag.load(Relaxed) {
                    let waited = now - since;
                    assert!(waited >= gap, "{rate}: frame {i}, {waited:?} < {gap:?}");
                    assert!(waited < gap + interval, "{rate}: frame {i}, {waited:?}");
                    since = now;
                    grants += 1;
                }
            }
            assert!(grants >= 20, "{rate}: {grants} grants");
            let s = split(id);
            assert_eq!(s.lane, 1_800, "{rate}");
            assert_eq!(s.owed_granted, grants, "{rate}: each answered an owed ask");
            assert_eq!(s.refused_gap + s.refused_rule, 1_800 - grants, "{rate}");
            assert_eq!(rig.counters.take(None).unwrap().late, 1_800, "{rate}");
        }
    }

    /// FR-148 P4b-4 (R1) — outside the rule's scope the gate is P4b-3's,
    /// grant for grant: under `off`, under `shadow` (its lane on
    /// `video-deadline` or not), and under `on` on `video-bytes`. Over five
    /// seeds of random asks from the viewer and the lane, with frames and key
    /// frames between them on a thin path, every answer, every flag and
    /// N12's counts equal P4b-3's gate's.
    #[test]
    fn outside_the_rule_s_scope_the_gate_grants_what_p4b_3_granted() {
        /// P4b-3's gate: one ask passes per 500 ms, and every other is
        /// forgotten.
        #[derive(Default)]
        struct Today(Option<Instant>);
        impl Today {
            fn ask(&mut self, now: Instant) -> bool {
                let allow = self.0.is_none_or(|t| now.duration_since(t) >= MIN_KF_GAP);
                if allow {
                    self.0 = Some(now);
                }
                allow
            }
        }
        let t0 = Instant::now();
        for seed in [1_u64, 2, 3, 0x0d55, 0x5eed_f00d] {
            for mode in [Mode::Off, Mode::Shadow, Mode::On] {
                let mut next = draws(seed);
                let (gate, flag, id) = gate_at(Some(300_000));
                let mut today = Today::default();
                let (mut t, mut asks, mut gapped) = (t0, 0_u32, 0_u32);
                for i in 0..3_000 {
                    let r = next();
                    t += Duration::from_millis(r % 300);
                    let scope = Scope {
                        mode,
                        deadline: mode == Mode::Shadow && (r >> 40).is_multiple_of(2),
                    };
                    let asked = match (r >> 16) % 5 {
                        0 => Some(gate.ask_at(id, t)),
                        1 | 2 => Some(gate.ask_lane(id, t, scope, LaneAsk::Late)),
                        3 => {
                            gate.on_frame(id, t, scope, Some(20_000 + (r >> 24) % 200_000));
                            None
                        }
                        _ => {
                            gate.on_frame(id, t, scope, None);
                            None
                        }
                    };
                    let want = asked.map(|_| today.ask(t));
                    assert_eq!(asked, want, "seed {seed} {mode:?} event {i}");
                    let granted = want == Some(true);
                    assert_eq!(
                        flag.swap(false, Relaxed),
                        granted,
                        "seed {seed} {mode:?} {i}"
                    );
                    asks += u32::from(want.is_some());
                    gapped += u32::from(want == Some(false));
                }
                let n12 = crate::session_telemetry::counters(id);
                assert_eq!(n12.keyframe_asks(), (asks, gapped), "seed {seed} {mode:?}");
                assert!(gapped > 0 && asks > gapped, "seed {seed}: {asks} {gapped}");
            }
        }
    }

    /// FR-148 P4b-4 (R1) — `shadow` counts without acting: on one sequence of
    /// asks from both sides and frames, the lane on `video-deadline`, its
    /// counts of asks refused, by the gap and by the rule, and of owed asks
    /// granted equal what `on` does, and its grants equal `off`'s.
    #[test]
    fn shadow_counts_what_on_would_refuse_and_grants_what_off_grants() {
        let t0 = Instant::now();
        for seed in [7_u64, 11, 0xd59, 0xfeed, 0x5eed_f00d] {
            let run = |mode| {
                let mut next = draws(seed);
                let (gate, flag, id) = gate_at(Some(1_000_000));
                let scope = Scope {
                    mode,
                    deadline: true,
                };
                let mut t = t0;
                let mut grants = Vec::new();
                for _ in 0..3_000 {
                    let r = next();
                    t += Duration::from_millis(r % 100 + 1);
                    match (r >> 16) % 8 {
                        0 => {
                            gate.ask_at(id, t);
                        }
                        1 | 2 => {
                            gate.ask_lane(id, t, scope, LaneAsk::Late);
                        }
                        3 => gate.on_frame(id, t, scope, Some(20_000 + (r >> 24) % 150_000)),
                        _ => gate.on_frame(id, t, scope, None),
                    }
                    grants.push(flag.swap(false, Relaxed));
                }
                (grants, split(id))
            };
            let (off, _) = run(Mode::Off);
            let (shadow, counted) = run(Mode::Shadow);
            let (on, done) = run(Mode::On);
            assert_eq!(shadow, off, "seed {seed}: shadow grants what off grants");
            assert_eq!(counted, done, "seed {seed}: and counts what on does");
            assert_ne!(on, off, "seed {seed}: the rule acted under on");
            assert!(done.refused_rule > 0 && done.owed_granted > 0, "{done:?}");
        }
    }

    /// FR-148 P4b-4 (R6) — N12's split: the viewer's asks and the lane's are
    /// counted apart, every ask is still counted as before, and refusals by
    /// the 500 ms gap and by the rule apart; an owed ask granted is counted
    /// once, at the frame that grants it or at the ask that passes. An ask
    /// owed on `video-deadline` stays owed after the lane has left it, and
    /// rc.130's gap then grants it.
    #[test]
    fn n12_counts_the_viewer_s_asks_and_the_lane_s_and_refusals_by_gap_and_by_rule() {
        let (gate, flag, id) = gate_at(Some(2_000_000));
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        // The clock: 80 KB at 2 Mbit/s, 1.28 s.
        gate.on_frame(id, t0, RULE, Some(80_000));
        assert!(!gate.ask_at(id, at(100)), "viewer: the gap");
        let late = LaneAsk::Late;
        assert!(
            gate.ask_lane(id, at(200), RULE, late),
            "lane: the gap, owed"
        );
        assert!(
            gate.ask_lane(id, at(700), RULE, late),
            "lane: the rule, owed"
        );
        assert!(!gate.ask_at(id, at(900)), "viewer: the rule");
        gate.on_frame(id, at(1_279), RULE, None);
        assert!(!flag.load(Relaxed));
        gate.on_frame(id, at(1_280), RULE, None);
        assert!(flag.swap(false, Relaxed), "the owed ask, granted");
        assert!(!gate.ask_at(id, at(1_300)), "viewer: the gap, owed");
        let bytes = Scope {
            mode: Mode::On,
            deadline: false,
        };
        assert!(
            !gate.ask_lane(id, at(1_500), bytes, late),
            "lane, on video-bytes"
        );
        assert!(!flag.load(Relaxed));
        assert!(
            gate.ask_lane(id, at(1_780), bytes, late),
            "500 ms after the grant"
        );
        assert!(flag.swap(false, Relaxed));
        assert_eq!(
            split(id),
            KeyframeSplit {
                viewer: 3,
                lane: 4,
                refused_gap: 4,
                refused_rule: 2,
                owed_granted: 2,
            }
        );
        let n12 = crate::session_telemetry::counters(id);
        assert_eq!(n12.keyframe_asks(), (7, 6), "every ask, as before");
        // The line takes the split over its span, and the rule's gap at it:
        // 80 KB at 2 Mbit/s.
        let counters = Counters::new(Mode::On);
        counters.follow(Arc::new(gate), Arc::clone(&n12), Arc::default());
        let line = counters.take(None).unwrap();
        assert_eq!(line.asks, KeyframeSplit::default());
        assert_eq!(line.kf_gap_ms, Some(1_280));
        n12.note_keyframe_split(KeyframeAsker::Viewer, Some(KeyframeRefusal::Rule));
        let line = counters.take(None).unwrap();
        assert_eq!((line.asks.viewer, line.asks.refused_rule), (1, 1));
        assert_eq!(counters.take(None).unwrap().asks, KeyframeSplit::default());
    }

    /// FR-148 P4b-4b — a switch's own key frame is not recovery, so the key
    /// frame rule never spaces it. Under `on`, at 1 Mbit/s, an 80 KB key frame
    /// is written on `video-bytes` (the rule's gap from it: 2.56 s), a second
    /// after the gate's last grant; 100 ms on, the carrier turns TCP and the
    /// old channel reads drained at once. The switch's first ask passes
    /// rc.130's 500 ms gap at that frame and sets the flag, the key frame it
    /// brings goes whole on `video-deadline` at the next, and its line's
    /// `dropped` counts the one frame before it. Written, that key frame
    /// restarts the rule's clock: D's ask after it is the rule's. On P4b-4 the
    /// switch's ask was owed until 2.56 s after the key frame on `video-bytes`,
    /// every frame dropped meanwhile.
    #[test]
    fn a_switch_s_key_frame_comes_at_the_500_ms_gap_not_the_rule_s() {
        let ms = Duration::from_millis;
        let mut rig = Rig::new(Mode::On);
        rig.class(C::Direct);
        let (gate, flag, id) = rig.gated(Some(1_000_000));
        let t0 = rig.t + ms(10);
        assert!(gate.ask_at(id, t0 - ms(1_000)), "the gate's last grant");
        flag.store(false, Relaxed);
        assert_eq!(rig.sized(true, 0, 80_000), Some((BYTES, 1)));
        assert_eq!((rig.t, gate.gap()), (t0, ms(2_560)), "80 KB at 1 Mbit/s");
        rig.deltas_until(t0 + ms(100));
        rig.class(C::TurnTcp);
        let (wrote, out) = logged(|| {
            let dropped = rig.delta();
            // The pump: a key frame once the gate set the flag.
            let key = flag.swap(false, Relaxed);
            (dropped, key, rig.sized(key, 0, 80_000))
        });
        assert_eq!(
            wrote,
            (None, true, Some((DL, 12))),
            "granted at t0 + 100 ms: {out}"
        );
        let line = "video channel: video-bytes -> video-deadline \
                    (carrier=turn_tcp, frame_seq=12, held_ms=10, dropped=1, tcp_end=agent)";
        assert_eq!(out.matches(line).count(), 1, "{out}");
        let s = split(id);
        assert_eq!(
            (s.viewer, s.lane, s.refused_gap, s.refused_rule),
            (1, 1, 0, 0)
        );
        // The switch's key frame, written at t0 + 110 ms, restarts the rule's
        // clock: D's ask after it is owed until 2.56 s after it.
        assert_eq!(rig.frame(false, 301), None, "D drops it, and asks");
        while !flag.load(Relaxed) {
            assert!(rig.delta().is_some());
        }
        assert_eq!(rig.t, t0 + ms(110 + 2_560));
        assert_eq!(rig.fake.st().whys, [LaneAsk::Switch, LaneAsk::Late]);
        let s = split(id);
        assert_eq!((s.lane, s.refused_gap, s.owed_granted), (2, 1, 1));
    }

    /// FR-148 P4b-4b — a switch's ask inside 500 ms of the gate's last grant
    /// is refused by rc.130's gap and forgotten, never owed, so the lane asks
    /// again at each frame until the gap has passed: toward `video-deadline`
    /// and toward `video-bytes`, as P4b-3's did. N12 counts each ask as the
    /// lane's and each refusal as the gap's. In the gate's other world, the
    /// one N12 counts under `shadow`, a switch's ask is the 500 ms gap's too.
    /// On P4b-4 the ask toward `video-deadline` was owed at once and never
    /// asked again.
    #[test]
    fn a_switch_s_ask_inside_500_ms_of_the_last_grant_is_refused_not_owed() {
        let ms = Duration::from_millis;
        for (from, to, old, new) in [
            (C::Direct, C::TurnTcp, BYTES, DL),
            (C::TurnTls, C::Direct, DL, BYTES),
        ] {
            let mut rig = Rig::new(Mode::On);
            rig.class(from);
            let (gate, flag, id) = rig.gated(Some(1_000_000));
            // The gate's last grant, the viewer's, 10 ms before the key frame
            // it brings.
            let granted = rig.t;
            assert!(gate.ask_at(id, granted));
            flag.store(false, Relaxed);
            assert_eq!(rig.sized(true, 0, 80_000), Some((old, 1)), "{to:?}");
            rig.deltas_until(granted + ms(100));
            rig.class(to);
            let mut asks = 0;
            while !flag.load(Relaxed) {
                assert_eq!(rig.delta(), None, "{to:?}: dropped while it waits");
                asks += 1;
                assert_eq!(rig.fake.st().asks, asks, "{to:?}: asked at each frame");
                let owed = gate.state.lock().unwrap().real.owed;
                assert_eq!(owed, None, "{to:?}: frame {asks}, never owed");
                assert!(asks <= 100, "{to:?}: never granted");
            }
            assert_eq!(rig.t, granted + MIN_KF_GAP, "{to:?}: as the gap passed");
            assert_eq!(asks, 41, "{to:?}");
            assert!(
                rig.fake.st().whys.iter().all(|&w| w == LaneAsk::Switch),
                "{to:?}"
            );
            let want = KeyframeSplit {
                viewer: 1,
                lane: 41,
                refused_gap: 40,
                refused_rule: 0,
                owed_granted: 0,
            };
            assert_eq!(split(id), want, "{to:?}");
            flag.store(false, Relaxed);
            assert_eq!(rig.key(), Some((new, 51)), "{to:?}");
        }

        // The other world: under `shadow` the gate keeps the grants and the
        // owed ask `on` would have had, and N12 counts them. A switch's ask is
        // the 500 ms gap's there too, inside the rule's 2.56 s, and never owed.
        // (The shadow lane asks nothing, so this is the gate's word alone.)
        let (gate, flag, id) = gate_at(Some(1_000_000));
        let shadow = Scope {
            mode: Mode::Shadow,
            deadline: true,
        };
        let t0 = Instant::now();
        gate.on_frame(id, t0, shadow, Some(80_000));
        for (at, passes) in [(0, true), (499, false), (500, true)] {
            let asked = gate.ask_lane(id, t0 + ms(at), shadow, LaneAsk::Switch);
            let flagged = flag.swap(false, Relaxed);
            assert_eq!((asked, flagged), (passes, passes), "{at} ms");
        }
        let st = gate.state.lock().unwrap();
        assert_eq!((st.real.owed, st.shadow.owed), (None, None));
        drop(st);
        let s = split(id);
        assert_eq!((s.lane, s.refused_gap, s.refused_rule), (3, 1, 0));
    }

    /// FR-148 P4b-4b — the viewer's asks during a switch toward
    /// `video-deadline` stay recovery, under the rule as D59 scopes it: owed
    /// inside the gap, and answered once. Under `on`, at 1 Mbit/s, a switch
    /// drains for 700 ms after an 80 KB key frame on `video-bytes`. The
    /// viewer's ask 590 ms after it, past 500 ms since any grant but inside
    /// the rule's 2.56 s, is owed, not granted. Once drained, the switch's own
    /// ask passes rc.130's gap, and its grant answers the owed ask. The viewer
    /// asks again before the switch's key frame comes: owed, and that key
    /// frame, written, clears it. No second key frame is forced for either,
    /// past the rule's gap from it.
    #[test]
    fn a_viewer_s_ask_during_a_switch_stays_owed_and_is_answered_once() {
        let ms = Duration::from_millis;
        let mut rig = Rig::new(Mode::On);
        rig.class(C::Direct);
        let (gate, flag, id) = rig.gated(Some(1_000_000));
        assert_eq!(rig.sized(true, 0, 80_000), Some((BYTES, 1)));
        let t0 = rig.t;
        rig.fake.st().buffered.insert(BYTES, 1);
        rig.class(C::TurnTcp);
        rig.deltas_until(t0 + ms(600));
        assert!(!gate.ask_at(id, rig.t), "590 ms after the key frame");
        assert!(gate.state.lock().unwrap().real.owed.is_some(), "owed");
        rig.deltas_until(t0 + ms(700));
        assert!(!flag.load(Relaxed), "the rule's gap has not opened");
        rig.fake.st().buffered.insert(BYTES, 0);
        assert_eq!(rig.delta(), None, "drained: dropped, the switch's ask");
        assert!(flag.swap(false, Relaxed), "the switch's own: granted");
        assert_eq!(split(id).owed_granted, 1, "its grant answered the owed ask");
        assert!(!gate.ask_at(id, rig.t + ms(5)), "the viewer's again");
        assert!(gate.state.lock().unwrap().real.owed.is_some(), "owed");
        assert_eq!(rig.sized(true, 0, 80_000), Some((DL, 72)));
        let owed = gate.state.lock().unwrap().real.owed;
        assert_eq!(owed, None, "the switch's key frame, written, answered it");
        for i in 0..300 {
            assert!(rig.delta().is_some());
            assert!(!flag.load(Relaxed), "frame {i}: no second key frame");
        }
        let want = KeyframeSplit {
            viewer: 2,
            lane: 1,
            refused_gap: 1,
            refused_rule: 1,
            owed_granted: 1,
        };
        assert_eq!(split(id), want);
        assert_eq!(rig.fake.st().whys, [LaneAsk::Switch]);
    }

    /// FR-148 P4b-4b — D's ask again in a switch's `Keying` is the switch's
    /// own. Under `on`, at 1 Mbit/s, a switch to `video-deadline` begins 3 s
    /// after an 80 KB key frame, past the rule's gap from it, so its first ask
    /// passes in either gate. The key frame that comes is older than D at
    /// `video-deadline`: it is dropped, and the ask after it goes through
    /// rc.130's 500 ms gap from that grant, refused and asked again at each
    /// frame until the gap has passed, never owed. On P4b-4 the re-ask was
    /// owed, and granted 2.56 s after the grant.
    #[test]
    fn d_s_re_ask_in_keying_is_the_switch_s_own() {
        let ms = Duration::from_millis;
        let mut rig = Rig::new(Mode::On);
        rig.class(C::Direct);
        let (gate, flag, id) = rig.gated(Some(1_000_000));
        assert_eq!(rig.sized(true, 0, 80_000), Some((BYTES, 1)));
        let t0 = rig.t;
        rig.deltas_until(t0 + ms(3_000));
        rig.class(C::TurnTcp);
        assert_eq!(rig.delta(), None, "drained at once: the switch's ask");
        assert!(flag.swap(false, Relaxed), "past the rule's gap: granted");
        let granted = rig.t;
        assert_eq!(rig.sized(true, 400, 80_000), None, "older than D: dropped");
        let mut asks = 2;
        assert_eq!(rig.fake.st().asks, asks, "the ask again");
        let owed = gate.state.lock().unwrap().real.owed;
        assert_eq!(owed, None, "refused by the 500 ms gap, never owed");
        while !flag.load(Relaxed) {
            assert_eq!(rig.delta(), None, "dropped while the switch waits");
            asks += 1;
            assert_eq!(rig.fake.st().asks, asks, "asked again at each frame");
            assert!(asks <= 100, "never granted");
        }
        assert_eq!(rig.t, granted + MIN_KF_GAP, "500 ms, not the rule's 2.56 s");
        assert_eq!(asks, 51);
        assert!(rig.fake.st().whys.iter().all(|&w| w == LaneAsk::Switch));
        let s = split(id);
        let counts = (s.lane, s.refused_gap, s.refused_rule, s.owed_granted);
        assert_eq!(counts, (51, 49, 0, 0));
        flag.store(false, Relaxed);
        assert_eq!(rig.key(), Some((DL, 352)));
        let line = rig.counters.take(None).unwrap();
        assert_eq!((line.late, line.drained, line.switches), (1, 50, 1));
    }

    /// FR-148 P4b-4 (D59) — both pumps hand their encoder to the budget's
    /// step, which reads the rule's rate from it at every frame past the
    /// gate: the one call each pump had, edited in place.
    #[test]
    fn both_pumps_feed_the_rule_s_rate_at_each_frame() {
        let src = peer_rs();
        let call = "delivery.on_frame(&pc, &mut governor, encoder.as_ref()); // FR-148 D46, P4b-4";
        for name in [
            "async fn media_pump_vp9_444_dc(",
            "async fn media_pump_ffmpeg_dc(",
        ] {
            let body = body_of(&src, name);
            assert_eq!(body.matches(call).count(), 1, "{name}");
            assert_eq!(body.matches("delivery.on_frame(").count(), 1, "{name}");
        }
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

    /// FR-148 P4b-5a — under `on`, a key frame whose every chunk went counts
    /// on the channel it went on: `kf_written.0` on `video-bytes`, `.1` on
    /// `video-deadline`. A delta counts on neither, nor does a frame the
    /// switch dropped. The counts run from the pump's start: a line's take
    /// leaves them, unlike the per-line counts beside them.
    #[test]
    fn a_key_frame_written_whole_counts_on_the_channel_it_went_on() {
        let mut rig = Rig::new(Mode::On);
        rig.class(C::Direct);
        assert_eq!(rig.sent(true, 0, true), Some((BYTES, 1)));
        assert_eq!(rig.sent(false, 0, true), Some((BYTES, 2)));
        rig.class(C::TurnTcp);
        assert_eq!(rig.sent(false, 0, true), None, "the switch waits");
        assert_eq!(rig.sent(true, 0, true), Some((DL, 4)), "its key frame");
        assert_eq!(rig.sent(false, 0, true), Some((DL, 5)));
        assert_eq!(rig.sent(true, 0, true), Some((DL, 6)));
        let line = rig.counters.take(None).unwrap();
        assert_eq!(line.kf_written, (1, 2));
        assert_eq!(line.kf_dropped, (0, 0), "a delta dropped is no key frame");
        assert_eq!((line.drained, line.switches), (1, 1), "the line's own");
        let again = rig.counters.take(None).unwrap();
        assert_eq!((again.kf_written, again.drained), ((1, 2), 0), "not taken");
        assert_eq!(rig.sent(true, 0, true), Some((DL, 7)));
        assert_eq!(rig.counters.take(None).unwrap().kf_written, (1, 3));
    }

    /// FR-148 P4b-5a — the key frames the lane dropped before the socket,
    /// under `on`: by D (`kf_dropped.0`), on `video-deadline` and at a
    /// switch's key frame toward it, and while a switch drained or waited
    /// (`.1`); each also counted where `late` and `drained` count it, and
    /// never as written.
    #[test]
    fn a_key_frame_dropped_counts_by_why_and_never_as_written() {
        let mut rig = Rig::new(Mode::On);
        rig.class(C::TurnTcp);
        assert_eq!(rig.sent(true, 0, true), Some((DL, 1)));
        assert_eq!(rig.sent(true, 301, true), None, "older than D");
        assert_eq!(rig.sent(false, 301, true), None, "a delta older than D");
        rig.fake.st().buffered.insert(DL, 9_000);
        rig.class(C::Direct);
        assert_eq!(rig.sent(true, 0, true), None, "while video-deadline drains");
        assert_eq!(rig.sent(false, 0, true), None);
        rig.fake.st().buffered.insert(DL, 0);
        assert_eq!(rig.sent(true, 0, true), Some((BYTES, 6)), "the switch's");
        let line = rig.counters.take(None).unwrap();
        assert_eq!((line.late, line.drained), (2, 2));
        assert_eq!(line.kf_dropped, (1, 1));
        assert_eq!(line.kf_written, (1, 1));

        // A switch's key frame older than D, toward `video-deadline`: D's.
        let mut rig = Rig::new(Mode::On);
        rig.class(C::Direct);
        assert_eq!(rig.sent(true, 0, true), Some((BYTES, 1)));
        rig.class(C::TurnTcp);
        assert_eq!(rig.sent(true, 400, true), None, "the switch's, too old");
        assert_eq!(rig.sent(true, 0, true), Some((DL, 3)));
        let line = rig.counters.take(None).unwrap();
        assert_eq!((line.late, line.drained), (1, 0));
        assert_eq!((line.kf_dropped, line.kf_written), ((1, 0), (1, 1)));
    }

    /// FR-148 P4b-5a — under `shadow` every frame goes on `video-bytes`, so
    /// its key frames count on `kf_written.0` alone; the lane drops nothing,
    /// so `kf_dropped` stays (0, 0) while `late` counts what D would drop.
    #[test]
    fn shadow_counts_key_frames_on_video_bytes_and_drops_none() {
        let mut rig = Rig::new(Mode::Shadow);
        rig.class(C::TurnTcp);
        assert_eq!(rig.sent(true, 0, true), Some((BYTES, 1)));
        assert_eq!(rig.sent(true, 400, true), Some((BYTES, 2)));
        assert_eq!(rig.sent(false, 400, true), Some((BYTES, 3)));
        rig.fake.st().buffered.insert(BYTES, 9_000);
        rig.class(C::Direct);
        assert_eq!(rig.sent(true, 0, true), Some((BYTES, 4)));
        let line = rig.counters.take(None).unwrap();
        assert_eq!(line.late, 2, "what on would drop");
        assert_eq!(line.switches, 1, "what on would make");
        assert_eq!(line.kf_written, (3, 0));
        assert_eq!(line.kf_dropped, (0, 0));
    }

    /// FR-148 P4b-5a — a send that failed counts on neither side, and is no
    /// drop: the lane routed it. The lane's stand-in cannot fail a send (the
    /// sends are the send task's, on `out.dc`), so the send task's count is
    /// held here through `written`'s `ok`, and in `peer.rs` by
    /// [`both_send_tasks_take_every_frame_s_road_from_their_lane`]: the one
    /// call is the condition of the arm after the chunk loop.
    #[test]
    fn a_failed_send_counts_on_neither_side() {
        for (mode, want) in [(Mode::Shadow, (1, 0)), (Mode::On, (0, 1))] {
            let mut rig = Rig::new(mode);
            rig.class(C::TurnTcp);
            assert!(rig.sent(true, 0, false).is_some(), "{mode:?}: routed");
            assert!(rig.sent(true, 0, true).is_some(), "{mode:?}");
            let line = rig.counters.take(None).unwrap();
            assert_eq!(line.kf_written, want, "{mode:?}");
            assert_eq!(line.kf_dropped, (0, 0), "{mode:?}");
        }
    }

    /// FR-148 P4b-5a — under `off` nothing is counted, the gauges are never
    /// read, no wake line is written, and every frame goes as before.
    #[test]
    fn off_counts_nothing_and_writes_no_wake_line() {
        let mut rig = Rig::new(Mode::Off);
        rig.class(C::TurnTcp);
        rig.fake.st().gauges = Some(gauges(1, 1));
        let t0 = rig.t;
        let (wrote, out) = logged(|| {
            vec![
                rig.sent(true, 0, true),
                rig.sent(true, 400, false),
                rig.wrote_at(t0 + Duration::from_secs(5), false, 900),
                rig.wrote_at(t0 + Duration::from_millis(5_300), true, 40_000),
                rig.sent(true, 0, true),
            ]
        });
        let today: Vec<_> = (1..=5).map(|n| Some((BYTES, n))).collect();
        assert_eq!(wrote, today);
        assert_eq!(out, "", "no line under off");
        assert_eq!(rig.counters.take(None), None);
        assert_eq!(rig.counters.kf_written.read(), (0, 0));
        assert_eq!(rig.counters.kf_dropped.read(), (0, 0));
        assert_eq!(rig.fake.st().gauge_reads, 0, "no gauges read");
    }

    /// FR-148 P4b-5a — `rc:decodestat`'s `kf`, the viewer's key frames
    /// completed whole on `video-bytes` and on `video-deadline`: under
    /// `shadow` and `on` the session keeps the newest, the line carries it,
    /// and a report without the key, or with one that is not two counts,
    /// leaves the last. Under `off` nothing is kept.
    #[test]
    fn the_viewer_s_key_frame_counts_ride_the_line_under_shadow_and_on() {
        use serde_json::json;
        let kf = |v: serde_json::Value| {
            let mut val = decodestat(None);
            val["kf"] = v;
            val
        };
        for mode in [Mode::Shadow, Mode::On] {
            let (session, _pump, rig) = ends(mode);
            let viewer = || rig.counters.take(None).unwrap().viewer_kf;
            assert_eq!(viewer(), None, "{mode:?}: before a report");
            session.viewer_relay().note(&decodestat(None));
            assert_eq!(viewer(), None, "{mode:?}: a report without the key");
            session.viewer_relay().note(&kf(json!([3, 5])));
            assert_eq!(viewer(), Some((3, 5)), "{mode:?}");
            session.viewer_relay().note(&decodestat(Some(json!("tls"))));
            assert_eq!(viewer(), Some((3, 5)), "{mode:?}: no key, the last");
            assert!(session.viewer_relay().newest().is_some(), "the relay's");
            for bad in [
                json!([3]),
                json!([3, 5, 7]),
                json!([-1, 5]),
                json!([3.5, 5]),
                json!([3, null]),
                json!(["3", "5"]),
                json!("3,5"),
                json!({ "bytes": 3, "deadline": 5 }),
                json!(null),
            ] {
                session.viewer_relay().note(&kf(bad.clone()));
                assert_eq!(viewer(), Some((3, 5)), "{mode:?}: {bad}");
            }
            session.viewer_relay().note(&kf(json!([4, 9])));
            assert_eq!(viewer(), Some((4, 9)), "{mode:?}");
            assert_eq!(session.viewer_relay().kf(), Some((4, 9)));
        }
        let (session, pump, rig) = ends(Mode::Off);
        assert!(pump.session.is_none());
        session.viewer_relay().note(&kf(json!([3, 5])));
        assert_eq!(session.viewer_relay().kf(), None, "nothing kept under off");
        assert_eq!(rig.counters.take(None), None);
    }

    /// FR-148 P4b-5a (D60) — a gauge reading, made up: the window one MTU, as
    /// a T3 leaves it, after `t3` T3-rtx expiries and `fast` fast
    /// retransmissions since the association began.
    fn gauges(t3: u64, fast: u64) -> Gauges {
        Gauges {
            max_payload: 1_200,
            cwnd: 1_228,
            ssthresh: 4_912,
            rwnd: 1_048_576,
            inflight_bytes: 2_400,
            pending_bytes: 0,
            fast_retransmits: fast,
            t3_expiries: t3,
            forward_tsns: 0,
            ack_delay: Some(Duration::from_millis(37)),
            ack_delay_min: None,
        }
    }

    /// The wake line, as a field read greps for it, with gauges.
    fn wake_line(
        still: u64,
        in_stretch: (&str, &str),
        frame: (&str, u64, &str),
        skipped: u64,
    ) -> String {
        format!(
            "path wake (FR-148 P4b-5a, D60) session={} still_ms={still} cwnd=1228 ssthresh=4912 \
             inflight_bytes=2400 pending_bytes=0 ack_delay_ms=37 max_payload=1200 \
             t3_in_stretch={} fast_in_stretch={} frame={} bytes={} channel={} skipped={skipped}\n",
            sid(),
            in_stretch.0,
            in_stretch.1,
            frame.0,
            frame.1,
            frame.2,
        )
    }

    /// FR-148 P4b-5a (D60) — a second or more with no write is a still
    /// stretch, and its first write says so in one `path wake` line: how
    /// long, the gauges as a look reads them then, the T3-rtx expiries and
    /// fast retransmissions from the last write before it to this one, and
    /// the frame. When that frame is a delta, the first key frame written
    /// within 10 s says so in a second line: its delay from the wake, its
    /// size and channel, the gauges at its write, and the T3s since the wake.
    /// A stretch under a second writes nothing.
    #[test]
    fn a_still_stretch_says_so_at_its_first_write_and_at_its_key_frame() {
        for (mode, channel) in [(Mode::Shadow, "video-bytes"), (Mode::On, "video-deadline")] {
            let mut rig = Rig::new(mode);
            rig.class(C::TurnTcp);
            let t0 = rig.t;
            let at = |ms: u64| t0 + Duration::from_millis(ms);
            let (wrote, out) = logged(|| {
                rig.fake.st().gauges = Some(gauges(2, 5));
                let mut wrote = vec![rig.wrote_at(at(0), true, 40_000)];
                // A T3 before the stretch: not its.
                rig.fake.st().gauges = Some(gauges(3, 5));
                wrote.push(rig.wrote_at(at(10), false, 800));
                rig.fake.st().gauges = Some(gauges(4, 7));
                wrote.push(rig.wrote_at(at(1_210), false, 900));
                rig.fake.st().gauges = Some(Gauges {
                    cwnd: 2_456,
                    ..gauges(5, 7)
                });
                wrote.push(rig.wrote_at(at(1_510), true, 40_000));
                wrote.push(rig.wrote_at(at(1_520), true, 40_000));
                wrote
            });
            let stream = if mode == Mode::On { DL } else { BYTES };
            let today: Vec<_> = (1..=5).map(|n| Some((stream, n))).collect();
            assert_eq!(wrote, today, "{mode:?}: the writes as before");
            assert_eq!(out.matches("path wake").count(), 2, "{mode:?}: {out}");
            let wake = wake_line(1_200, ("1", "2"), ("delta", 900, channel), 0);
            assert_eq!(out.matches(&wake).count(), 1, "{wake}{out}");
            let key = format!(
                "path wake key (FR-148 P4b-5a, D60) session={} delay_ms=300 bytes=40000 \
                 channel={channel} cwnd=2456 ssthresh=4912 inflight_bytes=2400 pending_bytes=0 \
                 ack_delay_ms=37 max_payload=1200 t3_since_wake=1\n",
                sid()
            );
            assert_eq!(out.matches(&key).count(), 1, "{key}{out}");
        }

        // 900 ms, and 999 ms: no stretch; 1,000 ms is one.
        let mut rig = Rig::new(Mode::On);
        let t0 = rig.t;
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let ((), out) = logged(|| {
            rig.wrote_at(at(0), true, 1_000);
            rig.wrote_at(at(900), false, 1_000);
            rig.wrote_at(at(1_899), false, 1_000);
        });
        assert!(!out.contains("path wake"), "{out}");
        let ((), out) = logged(|| {
            rig.wrote_at(at(2_899), true, 1_000);
        });
        assert_eq!(out.matches("path wake").count(), 1, "{out}");
        assert!(
            out.contains("still_ms=1000 frame=key bytes=1000 channel=video-bytes skipped=0"),
            "{out}"
        );
    }

    /// FR-148 P4b-5a (D60) — at most one wake line a pump in any 10 s: a
    /// stretch inside 10 s of the last line writes nothing, and the next line
    /// counts it. A wake on a key frame writes one line, and a wake on a delta
    /// whose key frame comes 10 s or more later writes no second line.
    #[test]
    fn a_stretch_inside_ten_seconds_of_a_wake_is_counted_at_the_next() {
        let mut rig = Rig::new(Mode::On);
        rig.fake.st().gauges = Some(gauges(0, 0));
        let t0 = rig.t;
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let ((), out) = logged(|| {
            rig.wrote_at(at(0), true, 40_000);
            rig.wrote_at(at(1_200), false, 900);
            rig.wrote_at(at(1_210), false, 900);
            rig.wrote_at(at(2_500), false, 900);
            rig.wrote_at(at(2_510), false, 900);
            rig.wrote_at(at(4_000), false, 900);
            rig.wrote_at(at(12_000), true, 40_000);
            rig.wrote_at(at(12_010), true, 40_000);
        });
        assert_eq!(out.matches("path wake").count(), 2, "{out}");
        assert_eq!(out.matches("path wake key").count(), 0, "{out}");
        let first = wake_line(1_200, ("0", "0"), ("delta", 900, "video-bytes"), 0);
        let second = wake_line(8_000, ("0", "0"), ("key", 40_000, "video-bytes"), 2);
        assert_eq!(out.matches(&first).count(), 1, "{first}{out}");
        assert_eq!(out.matches(&second).count(), 1, "{second}{out}");
        assert!(out.find(&first) < out.find(&second));
    }

    /// FR-148 P4b-5a (D60) — without gauges (no association yet, or its
    /// slot held) the lines carry the rest; with no gauges at the stretch's
    /// last write, its counts over the stretch read `None`.
    #[test]
    fn a_wake_without_gauges_carries_the_rest() {
        let mut rig = Rig::new(Mode::Shadow);
        let t0 = rig.t;
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let ((), out) = logged(|| {
            rig.wrote_at(at(0), true, 40_000);
            rig.wrote_at(at(1_200), false, 900);
            rig.wrote_at(at(1_500), true, 40_000);
        });
        let wake = format!(
            "path wake (FR-148 P4b-5a, D60) session={} still_ms=1200 frame=delta bytes=900 \
             channel=video-bytes skipped=0\n",
            sid()
        );
        let key = format!(
            "path wake key (FR-148 P4b-5a, D60) session={} delay_ms=300 bytes=40000 \
             channel=video-bytes\n",
            sid()
        );
        assert_eq!(out.matches(&wake).count(), 1, "{wake}{out}");
        assert_eq!(out.matches(&key).count(), 1, "{key}{out}");

        let mut rig = Rig::new(Mode::Shadow);
        let t0 = rig.t;
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let ((), out) = logged(|| {
            rig.wrote_at(at(0), true, 40_000);
            rig.fake.st().gauges = Some(gauges(1, 1));
            rig.wrote_at(at(1_200), false, 900);
        });
        let wake = wake_line(1_200, ("None", "None"), ("delta", 900, "video-bytes"), 0);
        assert_eq!(out.matches(&wake).count(), 1, "{wake}{out}");
    }

    /// FR-148 P4b-5a (D60) — a still stretch runs from the end of the last
    /// frame's sends, the send task's word on it (`written`), not from its
    /// road: a frame whose chunks take a second or more to hand over (the
    /// stack's queue full on a slow path) leaves no stretch behind it, and the
    /// T3-rtx expiries and fast retransmissions over a stretch are counted
    /// from that end, not from the road.
    #[test]
    fn a_frame_s_own_sends_are_no_still_stretch() {
        let mut rig = Rig::new(Mode::On);
        rig.class(C::TurnTcp);
        let t0 = rig.t;
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let route = |rig: &mut Rig, ms: u64, keyframe: bool, len: usize| {
            futures::executor::block_on(rig.lane.route_at(keyframe, len, at(ms), at(ms)))
                .expect("written")
        };
        let ((), out) = logged(|| {
            rig.fake.st().gauges = Some(gauges(1, 1));
            // A key frame of 300 KB whose sends take 1.5 s, a T3 and two
            // fast retransmissions among them; the next frame at once.
            let key = route(&mut rig, 0, true, 300_000);
            rig.fake.st().gauges = Some(gauges(2, 3));
            assert!(rig.lane.written_at(&key, true, at(1_500)));
            let delta = route(&mut rig, 1_510, false, 900);
            // A T3 among the delta's own sends: before the stretch.
            rig.fake.st().gauges = Some(gauges(3, 3));
            assert!(rig.lane.written_at(&delta, true, at(1_520)));
            // Then 1.2 s with nothing, and a T3 in it.
            rig.fake.st().gauges = Some(gauges(4, 3));
            let next = route(&mut rig, 2_720, false, 900);
            assert!(rig.lane.written_at(&next, true, at(2_730)));
        });
        assert_eq!(out.matches("path wake").count(), 1, "{out}");
        let wake = wake_line(1_200, ("1", "0"), ("delta", 900, "video-deadline"), 0);
        assert_eq!(out.matches(&wake).count(), 1, "{wake}{out}");
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
    /// FR-148 P4b-5a — and tells the lane how the frame's send went, once,
    /// as the condition of the arm of a frame whose every chunk went: after
    /// the chunk loop, whose failed send set `ok` false and left it.
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
            let arm = task.find("if lane.written(&out, ok) {").expect(name);
            let sends = &task[chunks..arm];
            assert_eq!(sends.matches("out.dc.send(").count(), 2, "{name}");
            assert_eq!(sends.matches("out.frame_seq,").count(), 1, "{name}");
            assert!(!sends.contains("video_bytes_dc"), "{name}");
            assert_eq!(sends.matches("ok = false;\n").count(), 1, "{name}");
            assert_eq!(body.matches("lane.written(").count(), 1, "{name}");
            assert_eq!(body.matches("if ok {").count(), 0, "{name}");
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

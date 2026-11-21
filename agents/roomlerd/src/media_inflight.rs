// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! FR-148 D46, step B1: the in-flight budget in time, computed in shadow on
//! both data-channel pumps.
//!
//! Every bound on the video a pump has out is in bytes or in frames today (the
//! send channel's depth, the VP9-444 watermark, the FFmpeg relay ledger), and
//! none bounds how long those bytes wait on a carrier that queues: a TURN relay
//! over TCP holds seconds of them (`media_delivery::shaped_link::tcp_shape`).
//! The budget bounds them in time:
//!
//! - `out` is the pump's own unsent bytes ([`PumpBytes`]), plus SCTP's pending
//!   bytes, plus its bytes in flight, over the whole association.
//! - `budget = max(min(rate, window / least) × (least + S), 3 × max_payload)`,
//!   with `window = min(cwnd, rwnd + inflight)`.
//! - A frame is admitted while `out < budget`, and held at `out == budget` or
//!   over, with two exceptions that admit it anyway: nothing is out; or SCTP's
//!   own window binds with nothing pending (`cwnd > 0 && pending == 0 &&
//!   inflight >= cwnd`), since the vendored SCTP grows its window only while a
//!   chunk waits (`association_internal.rs`).
//! - `rate` is the acknowledgement clock's bytes over the last second
//!   ([`RATE_WINDOW`]), the most of those figures over the last two
//!   ([`RATE_HOLD`]); before the first full second, the plan's opening rate. A
//!   second in which the budget did not hold was application-limited: it never
//!   lowers the rate and never lets it expire, so a still screen that wakes
//!   gets the last busy rate. A second in which it held throughout (in each
//!   quarter) does lower it; one that held only at an edge is a still
//!   stretch's edge, and counts as application-limited (`Budget::rate_at`).
//! - `least` is the least acknowledgement delay any peek of the gauges showed
//!   on the association, and [`SLACK`] before the first SACK. A new association
//!   (the clock's handle changes) and a new selected candidate pair start it
//!   over.
//! - `S` is [`SLACK`], 150 ms, a constant and not a knob.
//! - A still span is the clock unmoved for `least + S` with bytes out. Its
//!   release is read at the first frame [`BURST_WITHIN`] after its last still
//!   reading or later: a burst, [`BURST_PERMILLE`] or more of what SCTP had in
//!   flight then already acknowledged, is a head-of-line stall, through which
//!   the carrier kept serializing; anything less is a drain at the link's rate
//!   (an outage, or a TCP RTO), and so is a first frame too late to tell. After
//!   [`HOL_RELEASES`] head-of-line releases in a row a still span writes
//!   through: a frame goes while `out < budget + rate × still_age` and SCTP
//!   queues nothing, never faster than the clock's rate. Otherwise it holds,
//!   the first still span on an association too. A new association or a new
//!   selected pair forgets the class, with the least. A second of the clock
//!   that starts inside a released still span, or while its release comes
//!   back, counts from the span's first still reading: the burst brought back
//!   what the carrier serialized through all of it.
//!
//! [`Budget`] is the core: it reads no clock and does no I/O; it takes the time
//! and the readings as arguments, its rings are fixed (no allocation per
//! frame), and it keeps the counters of the `path delivery` line's window.
//! [`Shadow`] is the pump's side: once per frame, before capture, it reads the
//! association's clock and gauges (by `peek`, so the delivery sampler's takes
//! stay its own, FR-148 O10) and steps the core. In shadow the verdict reaches
//! those counters and nothing else: no gate, no AIMD call, no keyframe request.
//!
//! The switch is `ROOMLERD_INFLIGHT_BUDGET` (config key `inflight_budget`):
//! `off`, the default, computes nothing; `shadow` computes and writes the
//! fields; `on` (B3) acts on a constrained (relay) session's capacity gate
//! ([`Shadow::gate`], [`Pass`]): the step runs before the gate decides, a hold
//! skips the pass before capture as a full send channel does, a hold in a
//! still span calls no AIMD decrease, and the FFmpeg relay byte gate and the
//! VP9-444 watermark arm act on nothing. A direct session under `on` computes
//! the budget as `shadow` does. See `docs/rate-control.md` (§ The in-flight
//! budget).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use webrtc::ice_transport::ice_candidate_pair::RTCIceCandidatePair;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::sctp::association::delivery::DeliveryClock;
use webrtc::sctp::association::gauges::{GaugeReader, Gauges};

/// S: the standing queue the budget allows over the least acknowledgement
/// delay. A constant (D46 item 5): the line's spread of acknowledgement delays
/// over the least is what would lower it toward 75 ms.
pub const SLACK: Duration = Duration::from_millis(150);

/// The acknowledgement clock's figures are its bytes over this long.
pub const RATE_WINDOW: Duration = Duration::from_secs(1);

/// The rate is the most of the figures over this long.
pub const RATE_HOLD: Duration = Duration::from_secs(2);

/// The budget is never under this many full packets, as the open-time train's
/// cap is not (`media_train`).
pub const FLOOR_PACKETS: u64 = 3;

/// The clock's readings and the rate's figures are kept at most one per this
/// long, so the core's rings are fixed.
pub const GRAIN: Duration = Duration::from_millis(50);

/// FR-148 D48 — B2's `held` ([`Over::held`]): the verdicts held a frame in
/// at least this share of the whole grains of each of a span's seconds, per
/// cent. On the `tcp_shape` loaded cells a backlogged writer's samples held
/// 0.36 of a second's grains or more, an application-limited writer's 0.08
/// or less: a hold or three as an outage's release drained or the next stall
/// filled the budget, which made such a sample a P3 that set the shadow
/// model's capacity under the link. Chosen between the two
/// (`docs/rate-control.md`).
pub const HELD_SHARE_PERCENT: u64 = 20;

/// FR-148 D46 — a still span's release is a burst when this share of what
/// SCTP had in flight at the span's last still reading is acknowledged by the
/// first frame [`BURST_WITHIN`] after that reading, per mille. A head-of-line
/// stall's release brings back at once what the carrier serialized through
/// the stall; a drain at the link's rate, what the link carries in the time,
/// of a budget's worth in flight (the rate over the least and S). Pending
/// bytes are not counted: SCTP had not sent them, and no release brings them
/// back. Chosen on the `tcp_shape` stall and outage cells
/// (`docs/rate-control.md`).
pub const BURST_PERMILLE: u64 = 600;

/// FR-148 D46 — how long after a still span's last still reading its release
/// is read. A first frame at [`BURST_PERMILLE`] of the least and S or later is
/// too late to tell: a drain at the rate has brought back that share of a
/// budget by then, and the release reads as a drain.
pub const BURST_WITHIN: Duration = Duration::from_millis(50);

/// FR-148 D46 — head-of-line releases in a row before a still span writes
/// through. A drain starts the count over.
pub const HOL_RELEASES: u32 = 1;

/// `RATE_WINDOW / GRAIN` readings and the one the window starts from, with room.
const CLOCK_SLOTS: usize = 32;
/// `RATE_HOLD / GRAIN` figures and one, with room.
const FIGURE_SLOTS: usize = 48;

/// The switch, `ROOMLERD_INFLIGHT_BUDGET` (D46 item 7). From B3 `on` acts on
/// relay sessions.
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

    /// Anything but `off` computes the budget; only `on` acts on it, on a
    /// relay session ([`Shadow::gate`]).
    pub fn computes(self) -> bool {
        self != Mode::Off
    }
}

/// The switch, read once per pump, beside `ROOMLERD_PATH_MODEL`'s read
/// (`media_delivery::DeliverySampler::from_env`). A set variable wins over the
/// config key `inflight_budget`.
pub fn mode() -> Mode {
    mode_of(tunnel_core::env::node_env("INFLIGHT_BUDGET").as_deref())
}

/// `shadow` and `on` as written; unset or any other value is `off`.
pub fn mode_of(value: Option<&str>) -> Mode {
    match value {
        Some("shadow") => Mode::Shadow,
        Some("on") => Mode::On,
        _ => Mode::Off,
    }
}

/// One frame's reading: the acknowledgement clock, a peek of the gauges, the
/// pump's own unsent bytes, and the opening rate (bit/s, 0 when not known).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    pub acked_bytes: u64,
    pub gauges: Gauges,
    pub pump_bytes: u64,
    pub open_bps: u64,
}

impl Reading {
    /// What SCTP has: pending and in flight.
    pub fn sctp_out(&self) -> u64 {
        self.gauges
            .pending_bytes
            .saturating_add(self.gauges.inflight_bytes)
    }

    /// `out`: the pump's unsent bytes and what SCTP has.
    pub fn out(&self) -> u64 {
        self.pump_bytes.saturating_add(self.sctp_out())
    }
}

/// The budget's answer for one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// `out < budget`.
    Admit,
    /// Nothing is out.
    NothingOut,
    /// `out >= budget`, but SCTP's own window binds with nothing pending.
    WindowBinds,
    /// `out >= budget`, in a still span after head-of-line releases, with
    /// nothing pending in SCTP and `out < budget + rate × still_age`: written
    /// through the stall.
    WritesThrough,
    /// `out >= budget`.
    Hold,
}

impl Verdict {
    pub fn holds(self) -> bool {
        self == Verdict::Hold
    }
}

/// FR-148 D46 — what a still span's release showed about the carrier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// A burst: the carrier kept serializing through the stall, as a TCP leg
    /// does behind a lost segment, and what waited came back at once.
    HeadOfLine,
    /// No burst: the release acknowledged as fast as the link drains what
    /// waited (an outage, or a TCP RTO).
    Drain,
}

/// FR-148 D46 — the stall classifier's thresholds: [`BURST_PERMILLE`],
/// [`BURST_WITHIN`] and [`HOL_RELEASES`]. Constants, not knobs: only the
/// `tcp_shape` harness runs others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Classifier {
    pub burst_permille: u64,
    pub within: Duration,
    pub releases: u32,
}

impl Default for Classifier {
    fn default() -> Self {
        Self {
            burst_permille: BURST_PERMILLE,
            within: BURST_WITHIN,
            releases: HOL_RELEASES,
        }
    }
}

/// Today's bounds, counted beside the budget in the same windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    /// The VP9-444 pump shed a frame over the buffered-amount watermark.
    Watermark,
    /// The FFmpeg pump's ledger gate held a pass.
    Ledger,
    /// A send channel was full: a capacity-gate pass, or a `try_send` that
    /// found it full.
    ChannelFull,
}

/// One line's window of the budget, as the `path delivery` line writes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowReport {
    /// Frames the budget would have held, and frames it was asked about.
    pub held: (u64, u64),
    /// The budget and `out`, bytes, at their most.
    pub budget_max: u64,
    pub out_max: u64,
    /// The acknowledgement clock's rate at the window's last frame, bit/s; 0
    /// before the first full second when no opening rate was known.
    pub rate_bps: u64,
    /// The least acknowledgement delay then, ms; `None` before the first SACK,
    /// when S stands in for it.
    pub least_ms: Option<u64>,
    /// S, ms.
    pub slack_ms: u64,
    /// The acknowledgement delays over the least, ms, at each frame the clock
    /// had moved since the frame before: (least, median, most). The median is
    /// to 5 ms under 1 s, and to 100 ms up to 10 s. `None` when there was none.
    pub ack_over_ms: Option<(u64, u64, u64)>,
    /// Spans the clock stood still with bytes out for `least + S` or longer:
    /// (count, longest ms).
    pub still: (u64, u64),
    /// Today's bounds: (VP9 watermark sheds, FFmpeg ledger gate passes,
    /// channel-full passes and sheds).
    pub bounds: (u64, u64, u64),
    /// FR-148 D46 — still spans whose release was read in the window, by
    /// class: (head-of-line, drain).
    pub released: (u64, u64),
    /// FR-148 D46 — frames admitted while a still span stood, whatever let
    /// them go: under the budget, on SCTP's window, or written through.
    pub still_admitted: u64,
    /// FR-148 D46 B3 — under `on`, on a relay session: the holds that shut a
    /// pass by themselves (the send channel and the followers let it
    /// through), and the passes the budget acted on. `(0, 0)` in shadow and
    /// on a direct session.
    pub acted: (u64, u64),
    /// FR-148 D46 B3 — of those holds, the ones in a still span: a stall of
    /// the carrier, which calls no AIMD decrease.
    pub acted_stalled: u64,
}

/// A fixed ring: a push to a full ring drops its oldest.
#[derive(Clone, Copy, Debug)]
struct Ring<T: Copy + Default, const N: usize> {
    buf: [T; N],
    head: usize,
    len: usize,
}

impl<T: Copy + Default, const N: usize> Ring<T, N> {
    fn new() -> Self {
        Self {
            buf: [T::default(); N],
            head: 0,
            len: 0,
        }
    }

    /// The `i`-th from the oldest.
    fn get(&self, i: usize) -> Option<T> {
        (i < self.len).then(|| self.buf[(self.head + i) % N])
    }

    fn oldest(&self) -> Option<T> {
        self.get(0)
    }

    fn newest(&self) -> Option<T> {
        self.len.checked_sub(1).and_then(|i| self.get(i))
    }

    fn newest_mut(&mut self) -> Option<&mut T> {
        let i = self.len.checked_sub(1)?;
        Some(&mut self.buf[(self.head + i) % N])
    }

    fn push(&mut self, v: T) {
        if self.len == N {
            self.pop_oldest();
        }
        self.buf[(self.head + self.len) % N] = v;
        self.len += 1;
    }

    fn pop_oldest(&mut self) {
        if self.len > 0 {
            self.head = (self.head + 1) % N;
            self.len -= 1;
        }
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    fn iter(&self) -> impl Iterator<Item = T> + '_ {
        (0..self.len).map(move |i| self.buf[(self.head + i) % N])
    }
}

/// A reading of the clock: when, µs from the core's first step, and the bytes
/// acknowledged then.
#[derive(Clone, Copy, Debug, Default)]
struct Tick {
    at: u64,
    acked: u64,
}

/// The most of the 1 s figures that closed within one [`GRAIN`].
#[derive(Clone, Copy, Debug, Default)]
struct Figure {
    first: u64,
    last: u64,
    bps: u64,
}

/// The clock standing still with bytes out: at what count, from the first
/// reading at it to the last, and what SCTP had in flight at the last.
#[derive(Clone, Copy, Debug, Default)]
struct Still {
    acked: u64,
    from: u64,
    to: u64,
    inflight: u64,
}

/// FR-148 D46 — a still span's release, being read: the span's first and
/// last still readings (µs), the count the clock stood at, and what SCTP had
/// in flight at the last.
#[derive(Clone, Copy, Debug)]
struct Releasing {
    first: u64,
    from: u64,
    acked: u64,
    inflight: u64,
}

const FINE_MS: u64 = 5;
const FINE_TO_MS: u64 = 1_000;
const COARSE_MS: u64 = 100;
const COARSE_TO_MS: u64 = 10_000;
const FINE_SLOTS: usize = (FINE_TO_MS / FINE_MS) as usize;
const SPREAD_SLOTS: usize = FINE_SLOTS + ((COARSE_TO_MS - FINE_TO_MS) / COARSE_MS) as usize + 1;

/// The acknowledgement delays over the least in one window, ms, as a fixed
/// histogram: 5 ms slots under 1 s, 100 ms slots to 10 s, and one over.
#[derive(Clone, Debug)]
struct Spread {
    counts: [u32; SPREAD_SLOTS],
    n: u32,
    least: u64,
    most: u64,
}

impl Spread {
    fn new() -> Self {
        Self {
            counts: [0; SPREAD_SLOTS],
            n: 0,
            least: u64::MAX,
            most: 0,
        }
    }

    fn slot(ms: u64) -> usize {
        if ms < FINE_TO_MS {
            (ms / FINE_MS) as usize
        } else if ms < COARSE_TO_MS {
            FINE_SLOTS + ((ms - FINE_TO_MS) / COARSE_MS) as usize
        } else {
            SPREAD_SLOTS - 1
        }
    }

    fn floor_of(slot: usize) -> u64 {
        if slot < FINE_SLOTS {
            slot as u64 * FINE_MS
        } else if slot < SPREAD_SLOTS - 1 {
            FINE_TO_MS + (slot - FINE_SLOTS) as u64 * COARSE_MS
        } else {
            COARSE_TO_MS
        }
    }

    fn add(&mut self, ms: u64) {
        let c = &mut self.counts[Self::slot(ms)];
        *c = c.saturating_add(1);
        self.n = self.n.saturating_add(1);
        self.least = self.least.min(ms);
        self.most = self.most.max(ms);
    }

    /// (least, median, most); the median at its slot's floor, within the two.
    fn read(&self) -> Option<(u64, u64, u64)> {
        if self.n == 0 {
            return None;
        }
        let half = self.n.div_ceil(2);
        let mut seen = 0u32;
        let median = self
            .counts
            .iter()
            .position(|&c| {
                seen = seen.saturating_add(c);
                seen >= half
            })
            .map_or(self.most, Self::floor_of);
        Some((self.least, median.clamp(self.least, self.most), self.most))
    }
}

/// The counters of one line's window.
#[derive(Clone, Debug)]
struct Window {
    offered: u64,
    held: u64,
    budget_max: u64,
    out_max: u64,
    rate_bps: u64,
    least: Option<Duration>,
    spread: Spread,
    still: (u64, u64),
    bounds: (u64, u64, u64),
    released: (u64, u64),
    still_admitted: u64,
    acted: (u64, u64),
    acted_stalled: u64,
}

impl Window {
    fn new() -> Self {
        Self {
            offered: 0,
            held: 0,
            budget_max: 0,
            out_max: 0,
            rate_bps: 0,
            least: None,
            spread: Spread::new(),
            still: (0, 0),
            bounds: (0, 0, 0),
            released: (0, 0),
            still_admitted: 0,
            acted: (0, 0),
            acted_stalled: 0,
        }
    }
}

fn us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The grains wholly inside `a` to `b`, µs: from the first that starts at or
/// after `a` to the last that ends at or before `b` (FR-148 D48). Empty when
/// there is none.
fn whole_grains(a: u64, b: u64) -> std::ops::Range<u64> {
    let grain = us(GRAIN);
    let (lo, hi) = (a.div_ceil(grain), b / grain);
    lo..hi.max(lo)
}

/// SCTP's own window binds and nothing waits in its queue: the vendored SCTP
/// grows its window only while a chunk waits, so a frame goes whatever the
/// budget (else a window a T3 cut to one packet stays there).
fn window_binds(g: &Gauges) -> bool {
    g.cwnd > 0 && g.pending_bytes == 0 && g.inflight_bytes >= u64::from(g.cwnd)
}

/// The core: the budget's state on one association, stepped once per frame.
#[derive(Clone, Debug)]
pub struct Budget {
    slack: Duration,
    /// The first step's time: the rings keep µs from it.
    epoch: Option<Instant>,
    least: Option<Duration>,
    /// The gauges' two delays at the last peek: a delay folds into the least
    /// only when it changed since, so a reset is not undone by a value from
    /// before it (`ack_delay_min` keeps its least until the sampler takes).
    seen: (Option<Duration>, Option<Duration>),
    clock: Ring<Tick, CLOCK_SLOTS>,
    figures: Ring<Figure, FIGURE_SLOTS>,
    /// The rate the figures last gave; what an application-limited stretch
    /// keeps. `None` before the first figure.
    standing: Option<u64>,
    /// The holds by grain: bit k is a hold in the grain k grains before
    /// `hold_grain`'s.
    holds: u64,
    hold_grain: u64,
    /// FR-148 D48, rule 4 — of those, the holds SCTP's congestion window
    /// sized ([`Self::ramp_sized`]), by grain as `holds` keeps them. Only
    /// [`Self::over`] reads it, for [`Over::ramp`].
    ramp_holds: u64,
    last_acked: Option<u64>,
    still: Option<Still>,
    /// FR-148 D46 — the stall classifier: its thresholds, the release being
    /// read, the head-of-line releases in a row (0 after a drain, before the
    /// first release and after a reset), and the last release's class.
    classifier: Classifier,
    releasing: Option<Releasing>,
    hol_run: u32,
    released: Option<Release>,
    /// FR-148 D46 — the last released still span: its first still reading
    /// (µs), where its release ends (its last still reading, then the step
    /// that read the release), and the count the clock stood at. A second of
    /// the clock that starts from the first to that end counts from the first
    /// and that count: a burst can come back over more than one step.
    last_still: Option<(u64, u64, u64)>,
    /// FR-148 D46 B2 — the last still span that stood [`Self::still_after`]
    /// or more: its first and last still readings and its end (µs), the end
    /// its last still reading until the step that read its release. A
    /// delivery sample it meets is no capacity ([`Self::over`]).
    stood: Option<(u64, u64, u64)>,
    /// FR-148 D46 B2 — the last step found a still span standing for the
    /// least and S, or read a release; taken by [`Self::take_mark`].
    mark: bool,
    /// FR-148 D46 B3 — the last step found a still span standing for the
    /// least and S ([`Self::stalled`]).
    still_now: bool,
    /// FR-148 D46 B2 — the opening: whether this association's figures gave
    /// the last step's rate, or the opening rate stood in (before the first
    /// full second, and after it while the application-limited rule keeps
    /// that rate); and the first step whose rate they gave (µs). A sample
    /// that starts before it is no P3 capacity ([`Over::opening`]).
    measured: bool,
    first_measured: Option<u64>,
    /// The last step's budget, `out` and rate.
    last: (u64, u64, Option<u64>),
    /// FR-148 D48 — SCTP's fast retransmits and T3-rtx expiries at the last
    /// step, over this budget's associations together; `None` before this
    /// association's first step. The shadow model's window rules read it
    /// ([`Self::loss`]).
    loss: Option<u64>,
    /// The associations' before this one: a new association's counts start
    /// from 0, and the sum never runs back.
    loss_before: u64,
    window: Window,
}

/// FR-148 D46 B2 — what the budget saw over a delivery sample's span
/// ([`Budget::over`]): the facts P3 and the burst rule read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Over {
    /// A still span that stood [`Budget::still_after`] or more met the span:
    /// from its first still reading to the step that read its release.
    pub stood: bool,
    /// That span stood still for [`Budget::still_after`] or more inside the
    /// span itself, from its first still reading to its last.
    pub still_inside: bool,
    /// The verdicts held a frame in at least [`HELD_SHARE_PERCENT`] of the
    /// grains of each whole second of the span, counting only the grains
    /// wholly inside it (FR-148 D48).
    pub held: bool,
    /// The span starts inside the last released still span, or while its
    /// release came back: how long before the span's start that span's first
    /// still reading was, and the bytes acknowledged from there to the start.
    pub released_from: Option<(Duration, u64)>,
    /// The span starts before the budget's first measured rate on this
    /// association: the opening rate stood in for the rate, and SCTP's
    /// window was still opening, so a hold or a backlog said nothing of the
    /// path. Before the budget's first step, every span is.
    pub opening: bool,
    /// FR-148 D48, rule 4 — SCTP's congestion window sized a hold in a grain
    /// wholly inside the span: the budget's window term was under its rate
    /// and the congestion window was the smaller window
    /// (`cwnd <= rwnd + inflight`). The association's ramp bound such a hold,
    /// not the path. A hold the receive window sized is the viewer's, and is
    /// not one.
    pub ramp: bool,
    /// FR-148 D48, rule 1 — the budget's rate at its last step, bit/s: the
    /// most of its one-second figures over two seconds, at the sample's
    /// close. `None` before the budget's first step.
    pub rate: Option<u64>,
}

impl Default for Budget {
    fn default() -> Self {
        Self::new()
    }
}

impl Budget {
    pub fn new() -> Self {
        Self::slacked(SLACK, Classifier::default())
    }

    /// The tcp_shape harness's slack sweep only: S is not a knob.
    #[cfg(test)]
    pub(crate) fn with_slack(slack: Duration) -> Self {
        Self::slacked(slack, Classifier::default())
    }

    /// The tcp_shape harness's classifier runs only: its thresholds are not
    /// knobs either.
    #[cfg(test)]
    pub(crate) fn with_classifier(slack: Duration, classifier: Classifier) -> Self {
        Self::slacked(slack, classifier)
    }

    fn slacked(slack: Duration, classifier: Classifier) -> Self {
        Self {
            slack,
            epoch: None,
            least: None,
            seen: (None, None),
            clock: Ring::new(),
            figures: Ring::new(),
            standing: None,
            holds: 0,
            hold_grain: 0,
            ramp_holds: 0,
            last_acked: None,
            still: None,
            classifier,
            releasing: None,
            hol_run: 0,
            released: None,
            last_still: None,
            stood: None,
            mark: false,
            still_now: false,
            measured: false,
            first_measured: None,
            last: (0, 0, None),
            loss: None,
            loss_before: 0,
            window: Window::new(),
        }
    }

    /// One frame: fold the reading, size the budget, and answer.
    pub fn step(&mut self, now: Instant, r: &Reading) -> Verdict {
        let t = us(now.saturating_duration_since(*self.epoch.get_or_insert(now)));
        let moved = self.last_acked.is_some_and(|a| a != r.acked_bytes);
        self.last_acked = Some(r.acked_bytes);
        self.fold_least(&r.gauges);
        if moved && let (Some(delay), Some(least)) = (r.gauges.ack_delay, self.least) {
            self.window.spread.add(ms(delay.saturating_sub(least)));
        }
        // A release due is read before a span's end can start another, and
        // one that starts here is read here when it is due already. A span
        // that ends here is the rate's to see at this step.
        let read = self.read_release(t, r.acked_bytes);
        self.watch_still(t, r);
        let read = self.read_release(t, r.acked_bytes) || read;
        let rate = self.rate_at(t, r.acked_bytes, r.open_bps);
        if self.measured {
            self.first_measured.get_or_insert(t);
        }
        let budget = self.size(rate, &r.gauges);
        let still = self.still_age(t);
        self.mark = read || still.is_some();
        self.still_now = still.is_some();
        let through = still.and_then(|age| self.through(rate, age));
        let out = r.out();
        let verdict = if out == 0 {
            Verdict::NothingOut
        } else if out < budget {
            Verdict::Admit
        } else if window_binds(&r.gauges) {
            Verdict::WindowBinds
        } else if r.gauges.pending_bytes == 0
            && through.is_some_and(|extra| out < budget.saturating_add(extra))
        {
            Verdict::WritesThrough
        } else {
            Verdict::Hold
        };
        if verdict.holds() {
            self.age_holds(t);
            self.holds |= 1;
            if self.ramp_sized(rate, &r.gauges) {
                self.ramp_holds |= 1;
            }
        }
        self.last = (budget, out, rate);
        self.loss = Some(
            self.loss_before
                .saturating_add(r.gauges.fast_retransmits)
                .saturating_add(r.gauges.t3_expiries),
        );
        let w = &mut self.window;
        w.offered += 1;
        w.held += u64::from(verdict.holds());
        w.still_admitted += u64::from(still.is_some() && !verdict.holds());
        w.budget_max = w.budget_max.max(budget);
        w.out_max = w.out_max.max(out);
        w.rate_bps = rate.unwrap_or(0);
        w.least = self.least;
        verdict
    }

    /// The budget, bytes, at the last step.
    pub fn last_budget(&self) -> u64 {
        self.last.0
    }

    /// The rate at the last step, bit/s; `None` before the first full second
    /// when no opening rate was known.
    pub fn rate_bps(&self) -> Option<u64> {
        self.last.2
    }

    /// The least acknowledgement delay; `None` before the first SACK.
    pub fn least(&self) -> Option<Duration> {
        self.least
    }

    /// How long the clock may stand still with bytes out before it is a stall
    /// and not a full path: the least and S.
    pub fn still_after(&self) -> Duration {
        self.least.unwrap_or(self.slack) + self.slack
    }

    /// FR-148 D46 — the class of the last still span released on this
    /// association and pair; `None` before the first, and after a reset.
    pub fn last_release(&self) -> Option<Release> {
        self.released
    }

    /// FR-148 D46 B2 — whether a step since the last take found a still span
    /// standing for the least and S, or read a release. The pump hands it to
    /// the shadow model's gate (`Pipe::note_stall`), and nothing else reads it.
    pub fn take_mark(&mut self) -> bool {
        std::mem::take(&mut self.mark)
    }

    /// FR-148 D46 B3 — the last step found a still span standing for the least
    /// and S: a hold then is a stall of the carrier, and calls no AIMD decrease.
    pub fn stalled(&self) -> bool {
        self.still_now
    }

    /// FR-148 D46 B3 — the budget was asked about a pass it acts on.
    fn note_asked(&mut self) {
        let n = &mut self.window.acted.1;
        *n = n.saturating_add(1);
    }

    /// FR-148 D46 B3 — its hold shut the pass by itself; `stalled`, in a still
    /// span.
    fn note_acted_hold(&mut self, stalled: bool) {
        let w = &mut self.window;
        w.acted.0 = w.acted.0.saturating_add(1);
        w.acted_stalled = w.acted_stalled.saturating_add(u64::from(stalled));
    }

    /// FR-148 D46 B2 — what the budget saw over a delivery sample's span,
    /// `from` to `to`, whose clock stood at `from_acked` at its start: whether
    /// a still span of the least and S met it, from its first still reading to
    /// its release's ([`Over::stood`]), and stood that long inside it; whether
    /// the verdicts held a frame in a share of each of its whole seconds
    /// ([`Over::held`]); and whether it
    /// starts inside the last released span, or while its release came back;
    /// and whether it starts before the first measured rate ([`Over::opening`]).
    /// FR-148 D48: whether SCTP's congestion window sized a hold in it
    /// ([`Over::ramp`]), and the rate at the last step ([`Over::rate`]).
    /// Nothing before the first step, and nothing of an earlier association.
    pub fn over(&self, from: Instant, to: Instant, from_acked: u64) -> Over {
        let Some(epoch) = self.epoch else {
            return Over {
                opening: true,
                ..Over::default()
            };
        };
        let at = |i: Instant| us(i.saturating_duration_since(epoch));
        let (a, b) = (at(from), at(to));
        if b <= a {
            return Over::default();
        }
        let parts = ((b - a) / 1_000_000).max(1);
        let met = self.stood.filter(|&(first, _, end)| first <= b && end >= a);
        Over {
            stood: met.is_some(),
            still_inside: met.is_some_and(|(first, last, _)| {
                last.min(b).saturating_sub(first.max(a)) >= us(self.still_after())
            }),
            held: self.held_each(a, b, parts),
            released_from: self
                .last_still
                .filter(|&(first, end, _)| first < a && a <= end)
                .map(|(first, _, count)| {
                    (
                        Duration::from_micros(a - first),
                        from_acked.saturating_sub(count),
                    )
                }),
            opening: self.first_measured.is_none_or(|m| a < m),
            ramp: whole_grains(a, b).any(|g| self.in_mask(self.ramp_holds, g)),
            rate: self.last.2,
        }
    }

    /// FR-148 D46 B2 — whether this association's figures gave the last
    /// step's rate, or the opening rate stood in. FR-148 D48: the shadow
    /// model's window rules bound a window by a measured rate only.
    pub fn rate_measured(&self) -> bool {
        self.measured
    }

    /// FR-148 D48 — SCTP's fast retransmits and T3-rtx expiries at the last
    /// step, over this budget's associations together, so the count never
    /// runs back; `None` before this association's first step.
    pub fn loss(&self) -> Option<u64> {
        self.loss
    }

    /// Whether the verdicts held a frame in at least [`HELD_SHARE_PERCENT`]
    /// of the grains of each of `parts` equal stretches of the grains wholly
    /// inside `a` to `b`, µs. FR-148 D48: a grain the span only partly covers
    /// is its edge's, and the mask keeps no finer time, so a hold there (an
    /// outage's release before the span, or the next one's start after it) is
    /// not the span's; a span with no whole grain held nothing. And a hold or
    /// three in a second say nothing of the path: an application-limited
    /// writer's release drains through them.
    fn held_each(&self, a: u64, b: u64, parts: u64) -> bool {
        let grains = whole_grains(a, b);
        if grains.is_empty() {
            return false;
        }
        let (lo, n) = (grains.start, grains.end - grains.start);
        (0..parts).all(|p| {
            let first = lo + p * n / parts;
            let last = (lo + (p + 1) * n / parts).max(first + 1);
            let held = (first..last)
                .filter(|&g| self.in_mask(self.holds, g))
                .count();
            let held = u64::try_from(held).unwrap_or(u64::MAX);
            held > 0 && held.saturating_mul(100) >= HELD_SHARE_PERCENT * (last - first)
        })
    }

    /// Whether `mask`, kept by grain as `holds` is, has grain `g`. The masks
    /// keep 64 grains: a grain older than that, or newer than the last one
    /// they moved to, held nothing they know.
    fn in_mask(&self, mask: u64, g: u64) -> bool {
        g <= self.hold_grain && self.hold_grain - g < 64 && (mask >> (self.hold_grain - g)) & 1 == 1
    }

    /// FR-148 D48, rule 4 — whether SCTP's congestion window, not the rate,
    /// sized a step's budget at `rate`: its window term under the rate (as
    /// [`Self::size`] takes the smaller), with the congestion window the
    /// smaller of the two windows. With the receive window the smaller, the
    /// viewer sized it: a real limit of the pair, and no ramp.
    fn ramp_sized(&self, rate: Option<u64>, g: &Gauges) -> bool {
        let by_window = self.by_window(g);
        rate.is_none_or(|r| by_window < r)
            && u64::from(g.cwnd) <= u64::from(g.rwnd).saturating_add(g.inflight_bytes)
    }

    /// The selected candidate pair changed: the old path's least is no floor
    /// for the new one's, and its releases say nothing of the new carrier.
    pub fn reset_least(&mut self) {
        self.least = None;
        self.forget_releases();
    }

    /// A new association: its clock counts from 0 and its delays are its own.
    /// The standing rate stays, as the opening rate of the new one, which
    /// stands in until the new one's figures give one.
    pub fn on_new_association(&mut self) {
        self.least = None;
        self.seen = (None, None);
        self.clock.clear();
        self.figures.clear();
        self.holds = 0;
        self.ramp_holds = 0;
        self.loss_before = self.loss.take().unwrap_or(self.loss_before);
        self.last_acked = None;
        self.still = None;
        self.last_still = None;
        self.stood = None;
        self.mark = false;
        self.still_now = false;
        self.measured = false;
        self.first_measured = None;
        self.forget_releases();
    }

    /// FR-148 D46 — the class of the releases goes with the least: the next
    /// still span holds until a release is read again.
    fn forget_releases(&mut self) {
        self.releasing = None;
        self.hol_run = 0;
        self.released = None;
    }

    /// FR-148 D46 — how long a still span has stood at `t`, µs from its first
    /// still reading, once that is the least and S; `None` while the clock
    /// moves or nothing is out.
    fn still_age(&self, t: u64) -> Option<u64> {
        let s = self.still?;
        let age = t.saturating_sub(s.from);
        (age >= us(self.still_after())).then_some(age)
    }

    /// FR-148 D46 — what a still span `age` µs old may have out past the
    /// budget, bytes: the clock's rate over the span's age, after
    /// [`Classifier::releases`] head-of-line releases in a row. `None`
    /// otherwise, or with no rate known: the span holds.
    fn through(&self, rate: Option<u64>, age: u64) -> Option<u64> {
        if self.hol_run < self.classifier.releases.max(1) {
            return None;
        }
        rate.map(|bps| bps.saturating_mul(age) / 8_000_000)
    }

    /// FR-148 D46 — a still span's release, read at the first step
    /// [`Classifier::within`] after the span's last still reading: a burst
    /// when [`Classifier::burst_permille`] of what SCTP had in flight then is
    /// acknowledged by now, else a drain. A step that late a share of the
    /// least and S after it is too late to tell, and reads as a drain.
    /// Returns whether it read one.
    fn read_release(&mut self, t: u64, acked: u64) -> bool {
        let Some(r) = self.releasing else {
            return false;
        };
        let since = t.saturating_sub(r.from);
        if since < us(self.classifier.within) {
            return false;
        }
        self.releasing = None;
        self.last_still = Some((r.first, t, r.acked));
        if let Some((first, last, _)) = self.stood
            && first == r.first
        {
            self.stood = Some((first, last, t));
        }
        let permille = self.classifier.burst_permille;
        let late = since.saturating_mul(1_000) >= us(self.still_after()).saturating_mul(permille);
        let back = acked.saturating_sub(r.acked);
        let burst = !late
            && r.inflight > 0
            && back.saturating_mul(1_000) >= r.inflight.saturating_mul(permille);
        let class = if burst {
            self.hol_run = self.hol_run.saturating_add(1);
            self.window.released.0 += 1;
            Release::HeadOfLine
        } else {
            self.hol_run = 0;
            self.window.released.1 += 1;
            Release::Drain
        };
        self.released = Some(class);
        true
    }

    pub fn note_bound(&mut self, b: Bound) {
        let n = match b {
            Bound::Watermark => &mut self.window.bounds.0,
            Bound::Ledger => &mut self.window.bounds.1,
            Bound::ChannelFull => &mut self.window.bounds.2,
        };
        *n = n.saturating_add(1);
    }

    /// The window's counters, and a new window: the rate and the least carry
    /// over, so a window with no frame still says what they stand at.
    pub fn take_window(&mut self) -> WindowReport {
        let mut next = Window::new();
        next.rate_bps = self.window.rate_bps;
        next.least = self.window.least;
        let w = std::mem::replace(&mut self.window, next);
        WindowReport {
            held: (w.held, w.offered),
            budget_max: w.budget_max,
            out_max: w.out_max,
            rate_bps: w.rate_bps,
            least_ms: w.least.map(ms),
            slack_ms: ms(self.slack),
            ack_over_ms: w.spread.read(),
            still: w.still,
            bounds: w.bounds,
            released: w.released,
            still_admitted: w.still_admitted,
            acted: w.acted,
            acted_stalled: w.acted_stalled,
        }
    }

    fn fold_least(&mut self, g: &Gauges) {
        let fresh = |v: Option<Duration>, was: Option<Duration>| v.filter(|&v| Some(v) != was);
        let delays = [
            fresh(g.ack_delay, self.seen.0),
            fresh(g.ack_delay_min, self.seen.1),
        ];
        for d in delays.into_iter().flatten() {
            self.least = Some(self.least.map_or(d, |l| l.min(d)));
        }
        self.seen = (g.ack_delay, g.ack_delay_min);
    }

    /// The rate at `t`; `None` before the first figure when no opening rate is
    /// known.
    ///
    /// The rate falls only at a figure whose second was busy throughout: the
    /// budget held in each quarter of it. Any other second was the
    /// application's limit, in part or in whole (BBR marks a sample
    /// application-limited when any of it was), and the rate stands at the
    /// most of what it was and what the figures give. A second that holds only
    /// at one end is the edge of a still stretch: let it lower the rate and a
    /// still screen ends at the still screen's rate, the one thing D46 item 3's
    /// application-limited rule is for.
    fn rate_at(&mut self, t: u64, acked: u64, open_bps: u64) -> Option<u64> {
        if self
            .clock
            .newest()
            .is_none_or(|n| t.saturating_sub(n.at) >= us(GRAIN))
        {
            self.clock.push(Tick { at: t, acked });
        }
        // The window starts at the newest reading a window old or older.
        while self
            .clock
            .get(1)
            .is_some_and(|second| t.saturating_sub(second.at) >= us(RATE_WINDOW))
        {
            self.clock.pop_oldest();
        }
        let opening = self.standing.or((open_bps > 0).then_some(open_bps));
        let from = self
            .clock
            .oldest()
            .filter(|from| t.saturating_sub(from.at) >= us(RATE_WINDOW));
        let Some(from) = from else {
            self.measured = false;
            return opening;
        };
        // FR-148 D46 — a window that starts inside a released still span, or
        // while its release comes back, starts at the span's first still
        // reading and its count: the release brought back what the carrier
        // serialized from there.
        let (start, base) = self
            .last_still
            .filter(|&(first, end, _)| first < from.at && from.at <= end)
            .map_or((from.at, from.acked), |(first, _, count)| (first, count));
        let bps = acked.saturating_sub(base).saturating_mul(8_000_000) / t.saturating_sub(start);
        self.add_figure(t, bps);
        while self
            .figures
            .oldest()
            .is_some_and(|f| t.saturating_sub(f.last) > us(RATE_HOLD))
        {
            self.figures.pop_oldest();
        }
        let most = self.figures.iter().map(|f| f.bps).max().unwrap_or(bps);
        let held = self.held_throughout(from.at, t);
        let rate = if held {
            most
        } else {
            most.max(opening.unwrap_or(0))
        };
        // FR-148 D46 B2 — the figures gave it, or the application-limited
        // rule keeps one they gave; else the opening rate still stands in.
        self.measured = held || most >= opening.unwrap_or(0) || self.measured;
        self.standing = Some(rate);
        Some(rate)
    }

    fn add_figure(&mut self, t: u64, bps: u64) {
        if let Some(f) = self.figures.newest_mut()
            && t.saturating_sub(f.first) < us(GRAIN)
        {
            f.bps = f.bps.max(bps);
            f.last = t;
            return;
        }
        self.figures.push(Figure {
            first: t,
            last: t,
            bps,
        });
    }

    /// The holds' masks, moved on to `t`'s grain.
    fn age_holds(&mut self, t: u64) {
        let grain = t / us(GRAIN);
        if grain > self.hold_grain {
            let by = grain - self.hold_grain;
            self.holds = if by >= 64 { 0 } else { self.holds << by };
            self.ramp_holds = if by >= 64 { 0 } else { self.ramp_holds << by };
            self.hold_grain = grain;
        }
    }

    /// Whether the budget held in each quarter of the window from `from` to
    /// `t`. A window longer than the mask's 64 grains was not busy.
    fn held_throughout(&mut self, from: u64, t: u64) -> bool {
        self.age_holds(t);
        let grain = us(GRAIN);
        let n = (t / grain).saturating_sub(from / grain) + 1;
        if n > 64 {
            return false;
        }
        (0..4u64).all(|q| {
            let lo = q * n / 4;
            let hi = ((q + 1) * n / 4).max(lo + 1);
            let mask = (u64::MAX >> (64 - (hi - lo))) << lo;
            self.holds & mask != 0
        })
    }

    /// A span of the clock standing still with bytes out, from the first
    /// reading at its count to the last: counted when it ends, if it lasted
    /// the least and S; when the clock moved, its release read from its last
    /// still reading, and the span kept for the rate (FR-148 D46).
    fn watch_still(&mut self, t: u64, r: &Reading) {
        let out = r.sctp_out() > 0;
        let inflight = r.gauges.inflight_bytes;
        let after = us(self.still_after());
        if out
            && let Some(s) = self.still.as_mut()
            && s.acked == r.acked_bytes
        {
            s.to = t;
            s.inflight = inflight;
            let (first, last) = (s.from, s.to);
            if last.saturating_sub(first) >= after {
                self.stood = Some((first, last, last));
            }
            return;
        }
        if let Some(s) = self.still.take() {
            let len = s.to.saturating_sub(s.from);
            if len >= after {
                self.stood = Some((s.from, s.to, s.to));
                self.window.still.0 += 1;
                self.window.still.1 = self.window.still.1.max(len / 1_000);
                if r.acked_bytes != s.acked {
                    self.releasing = Some(Releasing {
                        first: s.from,
                        from: s.to,
                        acked: s.acked,
                        inflight: s.inflight,
                    });
                    self.last_still = Some((s.from, s.to, s.acked));
                }
            }
        }
        if out {
            self.still = Some(Still {
                acked: r.acked_bytes,
                from: t,
                to: t,
                inflight,
            });
        }
    }

    /// What SCTP's window carries in a least, bit/s: `window / least`.
    fn by_window(&self, g: &Gauges) -> u64 {
        let least = self.least.unwrap_or(self.slack);
        let window = u64::from(g.cwnd).min(u64::from(g.rwnd).saturating_add(g.inflight_bytes));
        window.saturating_mul(8_000_000) / us(least).max(1)
    }

    /// FR-148 D46 B2 — whether SCTP's window, not the rate, sized the last
    /// step's budget, read with that step's gauges: the tcp_shape harness's
    /// look at the opening.
    #[cfg(test)]
    pub(crate) fn window_sized(&self, g: &Gauges) -> bool {
        let by_window = self.by_window(g);
        self.last.2.is_none_or(|r| by_window < r)
    }

    /// The budget, bytes, at `rate` against the gauges now.
    fn size(&self, rate: Option<u64>, g: &Gauges) -> u64 {
        let least = self.least.unwrap_or(self.slack);
        let by_window = self.by_window(g);
        let rate = rate.map_or(by_window, |r| r.min(by_window));
        (rate.saturating_mul(us(least + self.slack)) / 8_000_000)
            .max(FLOOR_PACKETS.saturating_mul(u64::from(g.max_payload)))
    }
}

/// The pump's own unsent bytes, as the budget counts them (D46 item 9): taken
/// where the pump hands a frame to its send task, released per 16 KiB chunk
/// as SCTP takes it, and the rest when the frame fails or is dropped. Wrapping:
/// a release can run before its take, and the pump, the only reader, reads
/// after its own takes. Today's FFmpeg ledger is another count, untouched.
#[derive(Debug, Default)]
pub struct PumpBytes(AtomicU64);

impl PumpBytes {
    pub fn take(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn release(&self, n: u64) {
        self.0.fetch_sub(n, Ordering::Relaxed);
    }

    pub fn now(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// The send task's handle on the count; with the budget off, one that counts
/// nothing.
#[derive(Clone, Debug, Default)]
pub struct PumpCount(Option<Arc<PumpBytes>>);

impl PumpCount {
    pub fn of(bytes: &Arc<PumpBytes>) -> Self {
        Self(Some(Arc::clone(bytes)))
    }

    /// One frame of `len` bytes, taken off the channel.
    pub fn unsent(&self, len: usize) -> Unsent<'_> {
        Unsent {
            count: self.0.as_deref(),
            left: len as u64,
        }
    }
}

/// One frame's bytes in the send task: [`Unsent::sent`] releases each chunk
/// SCTP took, and the drop releases what is left (a failed send, a stale or
/// dropped frame, or a task that ends).
#[derive(Debug)]
pub struct Unsent<'a> {
    count: Option<&'a PumpBytes>,
    left: u64,
}

impl Unsent<'_> {
    pub fn sent(&mut self, n: usize) {
        let n = (n as u64).min(self.left);
        self.left -= n;
        if let Some(c) = self.count {
            c.release(n);
        }
    }
}

impl Drop for Unsent<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.count
            && self.left > 0
        {
            c.release(self.left);
        }
    }
}

/// Whether the selected candidate pair changed: a first pair is no change, and
/// no pair (none selected, or none read) keeps the last.
#[derive(Debug)]
pub struct PairWatch<K> {
    last: Option<K>,
}

impl<K> Default for PairWatch<K> {
    fn default() -> Self {
        Self { last: None }
    }
}

impl<K: PartialEq> PairWatch<K> {
    pub fn changed(&mut self, now: Option<K>) -> bool {
        let Some(now) = now else {
            return false;
        };
        let changed = self.last.as_ref().is_some_and(|last| *last != now);
        self.last = Some(now);
        changed
    }
}

/// FR-148 D46 B3 — one pass of a pump's capacity gate as the budget answers
/// it ([`Shadow::gate`]). Under `on`, on a constrained (relay) session, the
/// budget acts: its hold for the coming frame shuts the pass before capture,
/// the FFmpeg relay byte gate and the VP9-444 watermark arm act on nothing,
/// and a hold in a still span (the clock unmoved for the least and S with
/// bytes out: a stall of the carrier) calls no AIMD decrease. Any other pass
/// is today's: the byte gate shuts it, the watermark sheds, and each skip
/// outside FR-35's opener grace decreases.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    acts: bool,
    holds: bool,
    stalled: bool,
    byte_gate: bool,
    /// The hold shut the pass by itself: the send channel and the followers
    /// let it through ([`Self::shuts`]).
    alone: bool,
}

impl Pass {
    /// Today's pass: the budget acts on nothing, and `byte_gate` (the FFmpeg
    /// pump's relay or direct byte gate; `false` on VP9-444) shuts it.
    pub fn today(byte_gate: bool) -> Self {
        Self {
            byte_gate,
            ..Self::default()
        }
    }

    /// The budget acts on this pass.
    pub fn acts(&self) -> bool {
        self.acts
    }

    /// The capacity gate's last term, asked after the send channel's and the
    /// followers': while the budget acts, its hold, which has then shut the
    /// pass by itself; otherwise today's byte gate.
    pub fn shuts(&mut self) -> bool {
        if !self.acts {
            return self.byte_gate;
        }
        self.alone = self.holds;
        self.holds
    }

    /// The budget's hold shut this pass by itself.
    pub fn held(&self) -> bool {
        self.alone
    }

    /// That hold was in a still span: a stall of the carrier.
    pub fn stalled(&self) -> bool {
        self.alone && self.stalled
    }

    /// Whether the gate's skip calls the AIMD's decrease: never inside FR-35's
    /// opener grace (`grace`), as today, and never for the budget's hold in a
    /// still span (D46's no-decrease rule). A pass the send channel or a
    /// follower shut decreases as today.
    pub fn decreases(&self, grace: bool) -> bool {
        !grace && !self.stalled()
    }

    /// Whether the VP9-444 watermark arm sheds a frame `over` its mark: as
    /// today, and never while the budget acts.
    pub fn sheds(&self, over: bool) -> bool {
        over && !self.acts
    }
}

/// The opening rate a pump's step reads: the governor's applied rate, or
/// before one is applied the rate the pump opened at; 0 when neither is known.
pub(crate) fn opening(applied_bps: u32, open_at_bps: Option<u32>) -> u64 {
    let open = if applied_bps > 0 {
        applied_bps
    } else {
        open_at_bps.unwrap_or(0)
    };
    u64::from(open)
}

/// The pump's side of the budget: the association's handles, the pair it last
/// read, its own unsent bytes, and the core. In shadow every call returns
/// nothing to the pump: the verdict reaches the window's counters only.
/// FR-148 D46 B2 adds one reader, the delivery sampler: the stall mark
/// ([`Self::take_mark`]) and the facts over a sample ([`Budget::over`]) go to
/// the shadow model and the pair memory, never to a gate or a rate. FR-148
/// D46 B3: under `on`, on a constrained session, [`Self::gate`] steps the core
/// before the pump's capacity gate decides, and its [`Pass`] acts there.
#[derive(Debug)]
pub struct Shadow {
    budget: Budget,
    clock: Option<DeliveryClock>,
    gauges: Option<GaugeReader>,
    pair: PairWatch<RTCIceCandidatePair>,
    pump: Arc<PumpBytes>,
    /// FR-148 D46 B3 — the switch is `on`: the budget acts on a constrained
    /// session's gate.
    on: bool,
}

impl Default for Shadow {
    fn default() -> Self {
        Self::new()
    }
}

impl Shadow {
    pub fn new() -> Self {
        Self {
            budget: Budget::new(),
            clock: None,
            gauges: None,
            pair: PairWatch::default(),
            pump: Arc::default(),
            on: false,
        }
    }

    /// The pump's budget, from the switch it read: none when `off`, and none
    /// when the delivery sampler is off, since its line carries the fields.
    /// `on` acts on constrained sessions ([`Self::gate`]).
    pub fn for_pump(mode: Mode, sampler_on: bool) -> Option<Self> {
        if mode == Mode::Off {
            return None;
        }
        if !sampler_on {
            tracing::info!(
                inflight_budget = mode.word(),
                "the in-flight budget stays off: ROOMLERD_PATH_MODEL=off writes no \
                 path delivery line to carry it (FR-148 D46)"
            );
            return None;
        }
        Some(Self {
            on: mode == Mode::On,
            ..Self::new()
        })
    }

    /// FR-148 D46 B3 — the switch this budget was made from is `on`.
    pub fn is_on(&self) -> bool {
        self.on
    }

    /// The send task's handle on the pump's unsent bytes.
    pub fn pump_count(&self) -> PumpCount {
        PumpCount::of(&self.pump)
    }

    /// A frame handed to the send task: its bytes are the pump's until SCTP
    /// takes them.
    pub fn on_handed(&self, len: u64) {
        self.pump.take(len);
    }

    /// The pump's one call per frame, before capture: read the association's
    /// clock and gauges and step the budget. Never waits: while the
    /// transport's slot is held the handles it has answer, or nothing does.
    pub fn on_frame(&mut self, pc: &RTCPeerConnection, applied_bps: u32, open_at_bps: Option<u32>) {
        self.on_frame_with(
            Instant::now(),
            opening(applied_bps, open_at_bps),
            || pc.sctp().delivery_clock(),
            || pc.sctp().gauges(),
        );
    }

    /// [`Self::on_frame`] at `now`, with the clock from `clock` and the gauges
    /// from `gauges`, asked for as the delivery sampler asks for them. A new
    /// clock is a new association: the core starts its least and its rate's
    /// figures over.
    pub fn on_frame_with(
        &mut self,
        now: Instant,
        open_bps: u64,
        clock: impl FnOnce() -> Option<DeliveryClock>,
        gauges: impl FnOnce() -> Option<GaugeReader>,
    ) {
        let _ = self.read(now, open_bps, clock, gauges);
    }

    /// [`Self::on_frame_with`]'s read and step, with the step's verdict:
    /// `None` when there was no clock or no gauges to read.
    fn read(
        &mut self,
        now: Instant,
        open_bps: u64,
        clock: impl FnOnce() -> Option<DeliveryClock>,
        gauges: impl FnOnce() -> Option<GaugeReader>,
    ) -> Option<Verdict> {
        if let Some(fresh) = clock() {
            if !self.clock.as_ref().is_some_and(|c| c.same(&fresh)) {
                self.budget.on_new_association();
                self.clock = Some(fresh);
                self.gauges = None;
            }
            if self.gauges.is_none() {
                self.gauges = gauges();
            }
        }
        let (Some(clock), Some(gauges)) = (self.clock.as_ref(), self.gauges.as_ref()) else {
            return None;
        };
        let (acked, peeked) = (clock.read().acked_bytes, gauges.peek());
        Some(self.step(now, acked, peeked, open_bps))
    }

    /// One frame's reading, read. The verdict is counted, and goes nowhere
    /// else.
    pub fn on_reading(&mut self, now: Instant, acked_bytes: u64, gauges: Gauges, open_bps: u64) {
        let _ = self.step(now, acked_bytes, gauges, open_bps);
    }

    fn step(&mut self, now: Instant, acked_bytes: u64, gauges: Gauges, open_bps: u64) -> Verdict {
        let reading = Reading {
            acked_bytes,
            gauges,
            pump_bytes: self.pump.now(),
            open_bps,
        };
        self.budget.step(now, &reading)
    }

    /// FR-148 D46 B3 — the pump's call before its capacity gate decides, with
    /// today's byte gate for the pass (`false` on VP9-444). Under `on`, on a
    /// constrained session, it reads the association and steps the core here,
    /// before the gate, and the [`Pass`] it answers acts on the gate.
    /// Otherwise it reads nothing, the pass is today's, and the step stays
    /// [`Self::on_frame`]'s, after the gate, as in shadow.
    pub fn gate(
        &mut self,
        pc: &RTCPeerConnection,
        applied_bps: u32,
        open_at_bps: Option<u32>,
        constrained: bool,
        byte_gate: bool,
    ) -> Pass {
        if !self.acts_on(constrained) {
            return Pass::today(byte_gate);
        }
        self.gate_with(
            Instant::now(),
            opening(applied_bps, open_at_bps),
            || pc.sctp().delivery_clock(),
            || pc.sctp().gauges(),
            constrained,
            byte_gate,
        )
    }

    /// [`Self::gate`] at `now`, with the clock and the gauges asked for as
    /// [`Self::on_frame_with`] asks for them.
    pub fn gate_with(
        &mut self,
        now: Instant,
        open_bps: u64,
        clock: impl FnOnce() -> Option<DeliveryClock>,
        gauges: impl FnOnce() -> Option<GaugeReader>,
        constrained: bool,
        byte_gate: bool,
    ) -> Pass {
        if !self.acts_on(constrained) {
            return Pass::today(byte_gate);
        }
        let verdict = self.read(now, open_bps, clock, gauges);
        self.acting(verdict, byte_gate)
    }

    /// [`Self::gate`] on one frame's reading, as [`Self::on_reading`] reads
    /// one.
    #[cfg(test)]
    pub(crate) fn gate_reading(
        &mut self,
        now: Instant,
        acked_bytes: u64,
        gauges: Gauges,
        open_bps: u64,
        constrained: bool,
        byte_gate: bool,
    ) -> Pass {
        if !self.acts_on(constrained) {
            return Pass::today(byte_gate);
        }
        let verdict = self.step(now, acked_bytes, gauges, open_bps);
        self.acting(Some(verdict), byte_gate)
    }

    /// The budget acts on the gate: `on`, on a constrained session.
    fn acts_on(&self, constrained: bool) -> bool {
        self.on && constrained
    }

    /// The pass the budget acts on, from its step's verdict (`None`: nothing
    /// was read, so nothing holds). The byte gate is counted as today's
    /// ledger bound's firing, though it shuts nothing now.
    fn acting(&mut self, verdict: Option<Verdict>, byte_gate: bool) -> Pass {
        if byte_gate {
            self.budget.note_bound(Bound::Ledger);
        }
        if verdict.is_some() {
            self.budget.note_asked();
        }
        let holds = verdict.is_some_and(Verdict::holds);
        Pass {
            acts: true,
            holds,
            stalled: holds && self.budget.stalled(),
            byte_gate,
            alone: false,
        }
    }

    /// FR-148 D46 B3 — the capacity gate's last term for `pass`
    /// ([`Pass::shuts`]): a hold that shut the pass by itself is counted on
    /// the line, apart from the send channel's skips.
    pub fn shut(&mut self, pass: &mut Pass) -> bool {
        let shut = pass.shuts();
        if pass.held() {
            self.budget.note_acted_hold(pass.stalled());
        }
        shut
    }

    /// FR-148 D46 B3 — the VP9-444 watermark arm for `pass`
    /// ([`Pass::sheds`]): while the budget acts, a frame over the mark is
    /// counted as the watermark's firing, and nothing is shed.
    pub fn shed(&mut self, pass: &Pass, over: bool) -> bool {
        if pass.acts() && over {
            self.budget.note_bound(Bound::Watermark);
        }
        pass.sheds(over)
    }

    /// The pumps' 5 s re-read of the selected candidate pair: a new pair
    /// starts the least over. Takes no handler slot.
    pub async fn on_pair_check(&mut self, pc: &RTCPeerConnection) {
        let sctp = pc.sctp();
        let dtls = sctp.transport();
        let ice = dtls.ice_transport();
        let pair = ice.get_selected_candidate_pair().await;
        self.on_pair(pair);
    }

    pub fn on_pair(&mut self, pair: Option<RTCIceCandidatePair>) {
        if self.pair.changed(pair) {
            self.budget.reset_least();
        }
    }

    pub fn note_bound(&mut self, b: Bound) {
        self.budget.note_bound(b);
    }

    pub fn take_window(&mut self) -> WindowReport {
        self.budget.take_window()
    }

    /// The core: what it saw over a delivery sample's span is P3's and the
    /// burst rule's to read (FR-148 D46 B2, [`Budget::over`]).
    pub fn budget(&self) -> &Budget {
        &self.budget
    }

    /// FR-148 D46 B2 — [`Budget::take_mark`].
    pub fn take_mark(&mut self) -> bool {
        self.budget.take_mark()
    }

    /// A test's least, as `media_delivery`'s tests set the sampler's.
    #[cfg(test)]
    pub(crate) fn set_least(&mut self, least: Duration) {
        self.budget.least = Some(least);
    }
}

#[cfg(test)]
mod tests {
    use webrtc::ice_transport::ice_candidate::RTCIceCandidate;

    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn d(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// Gauges with a 1200-byte payload: the window, the peer's window, what is
    /// in flight and what is pending.
    fn g(cwnd: u32, rwnd: u32, inflight: u64, pending: u64) -> Gauges {
        Gauges {
            max_payload: 1_200,
            cwnd,
            rwnd,
            inflight_bytes: inflight,
            pending_bytes: pending,
            ..Gauges::default()
        }
    }

    /// A wide window: the rate term binds, not SCTP's window.
    fn wide(inflight: u64, pending: u64) -> Gauges {
        g(64 << 20, 64 << 20, inflight, pending)
    }

    fn delayed(mut g: Gauges, ack_delay_ms: u64, ack_delay_min_ms: u64) -> Gauges {
        g.ack_delay = Some(d(ack_delay_ms));
        g.ack_delay_min = Some(d(ack_delay_min_ms));
        g
    }

    fn read(acked: u64, gauges: Gauges, open_bps: u64) -> Reading {
        Reading {
            acked_bytes: acked,
            gauges,
            pump_bytes: 0,
            open_bps,
        }
    }

    /// The budget at 2 Mbit/s with a least of 50 ms: 2 Mbit/s × 200 ms = 50 kB.
    fn at_2_mbps_50_ms() -> (Budget, Instant) {
        let mut b = Budget::new();
        let t0 = Instant::now();
        b.step(t0, &read(0, delayed(wide(0, 0), 50, 50), 2_000_000));
        assert_eq!(b.least(), Some(d(50)));
        assert_eq!(b.last_budget(), 50_000);
        (b, t0)
    }

    #[test]
    fn the_inflight_budget_admits_while_out_is_under_it() {
        let (mut b, t0) = at_2_mbps_50_ms();
        let v = b.step(
            at(t0, 10),
            &read(0, delayed(wide(40_000, 9_999), 50, 50), 2_000_000),
        );
        assert_eq!(v, Verdict::Admit);
        // `out` counts the pump's own unsent bytes too.
        let mut r = read(0, delayed(wide(40_000, 9_000), 50, 50), 2_000_000);
        r.pump_bytes = 999;
        assert_eq!(b.step(at(t0, 20), &r), Verdict::Admit);
        r.pump_bytes = 1_000;
        assert_eq!(b.step(at(t0, 30), &r), Verdict::Hold);
    }

    #[test]
    fn the_inflight_budget_holds_at_out_equal_to_it() {
        let (mut b, t0) = at_2_mbps_50_ms();
        let v = b.step(
            at(t0, 10),
            &read(0, delayed(wide(40_000, 10_000), 50, 50), 2_000_000),
        );
        assert_eq!(b.last_budget(), 50_000);
        assert_eq!(v, Verdict::Hold, "the equal case holds");
        let v = b.step(
            at(t0, 20),
            &read(0, delayed(wide(60_000, 0), 50, 50), 2_000_000),
        );
        assert_eq!(v, Verdict::Hold);
    }

    #[test]
    fn the_inflight_budget_admits_with_nothing_out() {
        // No window and no payload yet: a budget of 0, which nothing is under.
        let mut b = Budget::new();
        let t0 = Instant::now();
        let none = g(0, 0, 0, 0);
        let none = Gauges {
            max_payload: 0,
            ..none
        };
        assert_eq!(b.step(t0, &read(0, none, 0)), Verdict::NothingOut);
        assert_eq!(b.last_budget(), 0);
        let one = Gauges {
            pending_bytes: 1,
            ..none
        };
        assert_eq!(b.step(at(t0, 10), &read(0, one, 0)), Verdict::Hold);
    }

    #[test]
    fn the_inflight_budget_admits_when_sctp_s_window_binds_with_nothing_pending() {
        // 100 kbit/s × 300 ms (least = S before the first SACK) = 3750 bytes,
        // over the 3600-byte floor; one window of 4380 bytes is all in flight.
        let mut b = Budget::new();
        let t0 = Instant::now();
        let bound = g(4_380, 1 << 20, 4_380, 0);
        assert_eq!(b.step(t0, &read(0, bound, 100_000)), Verdict::WindowBinds);
        assert_eq!(b.last_budget(), 3_750);
        // A chunk waiting: SCTP's window grows anyway, so the budget holds.
        let waiting = g(4_380, 1 << 20, 4_380, 1);
        assert_eq!(
            b.step(at(t0, 10), &read(0, waiting, 100_000)),
            Verdict::Hold
        );
        // The window not full: nothing binds but the budget.
        let under = g(4_380, 1 << 20, 4_379, 0);
        assert_eq!(b.step(at(t0, 20), &read(0, under, 100_000)), Verdict::Hold);
    }

    #[test]
    fn the_inflight_budget_is_never_under_three_packets() {
        // 8 kbit/s × 300 ms = 300 bytes: the floor, 3 × 1200, stands instead.
        let mut b = Budget::new();
        let t0 = Instant::now();
        assert_eq!(b.step(t0, &read(0, wide(3_599, 0), 8_000)), Verdict::Admit);
        assert_eq!(b.last_budget(), 3_600);
        assert_eq!(
            b.step(at(t0, 10), &read(0, wide(3_600, 0), 8_000)),
            Verdict::Hold
        );
    }

    #[test]
    fn the_inflight_budget_never_lets_out_more_than_sctp_s_window_in_a_least() {
        // 8 Mbit/s offered, but a 15 kB window over a least of 60 ms carries
        // 2 Mbit/s: 2 Mbit/s × 210 ms = 52 500 bytes.
        let mut b = Budget::new();
        let t0 = Instant::now();
        let small = delayed(g(15_000, 1 << 20, 0, 0), 60, 60);
        b.step(t0, &read(0, small, 8_000_000));
        assert_eq!(b.last_budget(), 52_500);
    }

    /// Steps every 50 ms from `from` to `to` ms, the clock counting `bps`, with
    /// `out` bytes in flight in a wide window and one pending byte, so no
    /// window binds: 10 MiB out holds at every step, 1 byte never does. The
    /// opening rate is 1.5 Mbit/s. Returns the acked count after the last.
    fn run(b: &mut Budget, t0: Instant, from: u64, to: u64, acked: u64, bps: u64, out: u64) -> u64 {
        let mut acked = acked;
        let mut t = from;
        while t <= to {
            b.step(at(t0, t), &read(acked, wide(out, 1), 1_500_000));
            acked += bps / 8 * 50 / 1_000;
            t += 50;
        }
        acked
    }

    #[test]
    fn the_inflight_budget_s_rate_is_the_opening_rate_before_the_first_full_second() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        let acked = run(&mut b, t0, 0, 950, 0, 4_000_000, 1);
        assert_eq!(b.rate_bps(), Some(1_500_000), "the opening rate, at 950 ms");
        run(&mut b, t0, 1_000, 1_000, acked, 4_000_000, 1);
        assert_eq!(b.rate_bps(), Some(4_000_000), "the first second's figure");
        // No opening rate known: the window alone caps it until then.
        let mut b = Budget::new();
        b.step(t0, &read(0, wide(1, 0), 0));
        assert_eq!(b.rate_bps(), None);
    }

    fn near(got: Option<u64>, want: u64) -> bool {
        got.is_some_and(|got| got.abs_diff(want) <= want / 100)
    }

    #[test]
    fn the_inflight_budget_s_rate_is_the_most_of_its_1_s_figures_over_2_s() {
        // Busy at 4 Mbit/s for 3 s, then at 1 Mbit/s, holding throughout.
        let mut b = Budget::new();
        let t0 = Instant::now();
        let acked = run(&mut b, t0, 0, 2_950, 0, 4_000_000, 10 << 20);
        let acked = run(&mut b, t0, 3_000, 3_500, acked, 1_000_000, 10 << 20);
        assert!(near(b.rate_bps(), 4_000_000), "{:?}", b.rate_bps());
        // At 4.5 s the most of the figures over 2 s is the one closed at
        // 2.5 s or later that holds the most of the fast second.
        let acked = run(&mut b, t0, 3_550, 4_500, acked, 1_000_000, 10 << 20);
        assert!(near(b.rate_bps(), 4_000_000), "{:?}", b.rate_bps());
        // Past 5 s the 4 Mbit/s second is out of the 2 s.
        let acked = run(&mut b, t0, 4_550, 5_500, acked, 1_000_000, 10 << 20);
        assert!(near(b.rate_bps(), 2_500_000), "{:?}", b.rate_bps());
        run(&mut b, t0, 5_550, 6_100, acked, 1_000_000, 10 << 20);
        assert!(near(b.rate_bps(), 1_000_000), "{:?}", b.rate_bps());
    }

    #[test]
    fn the_inflight_budget_s_rate_falls_after_a_held_window() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        let acked = run(&mut b, t0, 0, 2_950, 0, 4_000_000, 10 << 20);
        assert!(near(b.rate_bps(), 4_000_000));
        run(&mut b, t0, 3_000, 8_000, acked, 500_000, 10 << 20);
        assert!(near(b.rate_bps(), 500_000), "{:?}", b.rate_bps());
        assert!(b.take_window().held.0 > 0);
    }

    #[test]
    fn the_inflight_budget_s_rate_neither_falls_nor_expires_while_nothing_holds() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        // Busy at 4 Mbit/s, holding.
        let acked = run(&mut b, t0, 0, 2_950, 0, 4_000_000, 10 << 20);
        assert!(near(b.rate_bps(), 4_000_000));
        // A still screen for 7 s: 0.2 Mbit/s, with a byte out, never held.
        let acked = run(&mut b, t0, 3_000, 10_000, acked, 200_000, 1);
        assert!(near(b.rate_bps(), 4_000_000), "{:?}", b.rate_bps());
        // An application-limited second faster than the rate raises it.
        run(&mut b, t0, 10_050, 12_000, acked, 6_000_000, 1);
        assert!(near(b.rate_bps(), 6_000_000), "{:?}", b.rate_bps());
    }

    #[test]
    fn the_inflight_budget_s_rate_stands_across_a_still_screen_that_wakes() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        let acked = run(&mut b, t0, 0, 2_950, 0, 4_000_000, 10 << 20);
        let acked = run(&mut b, t0, 3_000, 7_950, acked, 200_000, 1);
        assert!(near(b.rate_bps(), 4_000_000), "{:?}", b.rate_bps());
        // The screen wakes onto the link's 4 Mbit/s, and the budget holds from
        // 200 ms on: the seconds that end in it are a still stretch's edge.
        let acked = run(&mut b, t0, 8_000, 8_150, acked, 4_000_000, 1);
        let acked = run(&mut b, t0, 8_200, 8_900, acked, 4_000_000, 10 << 20);
        assert!(near(b.rate_bps(), 4_000_000), "{:?}", b.rate_bps());
        run(&mut b, t0, 8_950, 10_000, acked, 4_000_000, 10 << 20);
        assert!(near(b.rate_bps(), 4_000_000), "{:?}", b.rate_bps());
    }

    #[test]
    fn the_inflight_budget_s_least_is_s_before_the_first_sack() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        b.step(t0, &read(0, wide(0, 0), 1_000_000));
        assert_eq!(b.least(), None);
        // 1 Mbit/s × (S + S) = 37 500 bytes.
        assert_eq!(b.last_budget(), 37_500);
        assert_eq!(b.still_after(), SLACK + SLACK);
    }

    #[test]
    fn the_inflight_budget_s_least_falls_with_a_lower_reading() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        b.step(t0, &read(0, delayed(wide(0, 0), 80, 80), 1_000_000));
        assert_eq!(b.least(), Some(d(80)));
        b.step(at(t0, 10), &read(0, delayed(wide(0, 0), 60, 60), 1_000_000));
        assert_eq!(b.least(), Some(d(60)));
        b.step(at(t0, 20), &read(0, delayed(wide(0, 0), 70, 60), 1_000_000));
        assert_eq!(b.least(), Some(d(60)), "a higher reading leaves it");
        b.step(at(t0, 30), &read(0, delayed(wide(0, 0), 70, 55), 1_000_000));
        assert_eq!(b.least(), Some(d(55)), "the gauges' own least folds too");
        // 1 Mbit/s × (55 + 150) ms.
        assert_eq!(b.last_budget(), 25_625);
    }

    fn candidate(address: &str, port: u16) -> RTCIceCandidate {
        RTCIceCandidate {
            address: address.to_owned(),
            port,
            ..RTCIceCandidate::default()
        }
    }

    fn pair(local_port: u16) -> RTCIceCandidatePair {
        RTCIceCandidatePair::new(
            candidate("10.0.0.1", local_port),
            candidate("10.0.0.2", 3478),
        )
    }

    #[test]
    fn the_inflight_budget_s_least_starts_over_on_a_new_pair() {
        let t0 = Instant::now();
        let mut s = Shadow::new();
        s.on_reading(t0, 0, delayed(wide(0, 0), 40, 40), 1_000_000);
        assert_eq!(s.budget().least(), Some(d(40)));
        // The first pair, read again, and no pair read: no change.
        s.on_pair(Some(pair(50_000)));
        s.on_pair(Some(pair(50_000)));
        s.on_pair(None);
        assert_eq!(s.budget().least(), Some(d(40)));
        // A new pair: the old path's least goes.
        s.on_pair(Some(pair(50_001)));
        assert_eq!(s.budget().least(), None);
        // The gauges' own least still holds the old path's 40 ms until the
        // sampler takes: unchanged, it does not come back.
        s.on_reading(at(t0, 10), 0, delayed(wide(0, 0), 40, 40), 1_000_000);
        assert_eq!(s.budget().least(), None);
        s.on_reading(at(t0, 20), 0, delayed(wide(0, 0), 120, 40), 1_000_000);
        assert_eq!(s.budget().least(), Some(d(120)));
        // Back to the first pair is a change too.
        s.on_pair(Some(pair(50_000)));
        assert_eq!(s.budget().least(), None);
    }

    #[test]
    fn the_inflight_budget_s_least_starts_over_on_a_new_association() {
        let t0 = Instant::now();
        let mut b = Budget::new();
        let acked = run(&mut b, t0, 0, 1_500, 0, 4_000_000, 10 << 20);
        b.step(at(t0, 1_550), &read(acked, delayed(wide(1, 1), 40, 40), 0));
        assert_eq!(b.least(), Some(d(40)));
        b.on_new_association();
        assert_eq!(b.least(), None);
        // The new association's clock counts from 0: the rate stands until its
        // own first second, then its figures alone.
        b.step(at(t0, 1_600), &read(0, delayed(wide(1, 1), 40, 40), 0));
        assert_eq!(
            b.least(),
            Some(d(40)),
            "a new association's delays are its own"
        );
        assert!(near(b.rate_bps(), 4_000_000), "{:?}", b.rate_bps());
    }

    #[test]
    fn a_still_clock_counts_only_with_bytes_out() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        // Two seconds of a clock that does not move, with nothing out.
        for t in (0..=2_000).step_by(50) {
            b.step(at(t0, t), &read(5_000, wide(0, 0), 1_000_000));
        }
        b.step(at(t0, 2_050), &read(6_000, wide(0, 0), 1_000_000));
        assert_eq!(b.take_window().still, (0, 0));
    }

    #[test]
    fn a_still_clock_counts_only_past_the_least_and_s() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        // A least of 50 ms: a span counts from 200 ms.
        let out = delayed(wide(10_000, 0), 50, 50);
        b.step(at(t0, 900), &read(1_000, out, 1_000_000));
        // 150 ms still, then the clock moves: not counted.
        for t in [1_000, 1_050, 1_100, 1_150] {
            b.step(at(t0, t), &read(2_000, out, 1_000_000));
        }
        // 250 ms still, then it moves: counted.
        for t in [1_200, 1_250, 1_300, 1_350, 1_400, 1_450] {
            b.step(at(t0, t), &read(3_000, out, 1_000_000));
        }
        b.step(at(t0, 1_500), &read(4_000, out, 1_000_000));
        // 200 ms exactly, ended by nothing out: counted.
        for t in [1_550, 1_600, 1_650, 1_700] {
            b.step(at(t0, t), &read(4_000, out, 1_000_000));
        }
        b.step(at(t0, 1_750), &read(4_000, wide(0, 0), 1_000_000));
        assert_eq!(b.take_window().still, (2, 250));
    }

    #[test]
    fn the_inflight_budget_s_window_counts_and_starts_over() {
        let (mut b, t0) = at_2_mbps_50_ms();
        b.step(
            at(t0, 10),
            &read(0, delayed(wide(60_000, 0), 50, 50), 2_000_000),
        );
        b.step(
            at(t0, 20),
            &read(1_200, delayed(wide(30_000, 0), 90, 50), 2_000_000),
        );
        b.step(
            at(t0, 30),
            &read(2_400, delayed(wide(30_000, 0), 75, 50), 2_000_000),
        );
        b.note_bound(Bound::Watermark);
        b.note_bound(Bound::Ledger);
        b.note_bound(Bound::Ledger);
        b.note_bound(Bound::ChannelFull);
        let w = b.take_window();
        assert_eq!(w.held, (1, 4));
        assert_eq!((w.budget_max, w.out_max), (50_000, 60_000));
        assert_eq!(
            (w.rate_bps, w.least_ms, w.slack_ms),
            (2_000_000, Some(50), 150)
        );
        assert_eq!(w.ack_over_ms, Some((25, 25, 40)));
        assert_eq!(w.bounds, (1, 2, 1));
        // The next window starts over, and says where the rate and least stand.
        let next = b.take_window();
        assert_eq!(
            next,
            WindowReport {
                rate_bps: 2_000_000,
                least_ms: Some(50),
                slack_ms: 150,
                ..WindowReport::default()
            }
        );
    }

    #[test]
    fn the_spread_s_median_is_to_its_slot() {
        let mut s = Spread::new();
        assert_eq!(s.read(), None);
        for ms in [3, 7, 12, 40, 41, 44, 2_500] {
            s.add(ms);
        }
        assert_eq!(s.read(), Some((3, 40, 2_500)));
        let mut s = Spread::new();
        for ms in [1_250, 1_260, 30_000] {
            s.add(ms);
        }
        assert_eq!(s.read(), Some((1_250, 1_250, 30_000)));
    }

    #[test]
    fn the_inflight_budget_s_rings_are_fixed() {
        // A frame every millisecond for 10 s: the rings stay in their slots.
        let mut b = Budget::new();
        let t0 = Instant::now();
        for t in 0..10_000u64 {
            b.step(at(t0, t), &read(t * 250, wide(10 << 20, 1), 2_000_000));
            assert!(b.clock.len <= 22 && b.figures.len <= 41, "at {t} ms");
        }
        assert!(near(b.rate_bps(), 2_000_000), "{:?}", b.rate_bps());
    }

    #[test]
    fn the_inflight_budget_switch_is_off_unless_shadow_or_on() {
        assert_eq!(mode_of(Some("shadow")), Mode::Shadow);
        assert_eq!(mode_of(Some("on")), Mode::On);
        for off in [
            None,
            Some("off"),
            Some(""),
            Some("SHADOW"),
            Some("1"),
            Some("true"),
        ] {
            assert_eq!(mode_of(off), Mode::Off, "{off:?}");
        }
        assert!(Mode::On.computes() && Mode::Shadow.computes() && !Mode::Off.computes());
        assert!(Shadow::for_pump(Mode::Off, true).is_none());
        assert!(Shadow::for_pump(Mode::Shadow, true).is_some_and(|s| !s.is_on()));
        assert!(
            Shadow::for_pump(Mode::On, true).is_some_and(|s| s.is_on()),
            "on acts on relay sessions from B3"
        );
        assert!(
            Shadow::for_pump(Mode::Shadow, false).is_none(),
            "no line to carry it"
        );
    }

    #[test]
    fn the_inflight_budget_s_pump_count_takes_and_releases_per_chunk() {
        let s = Shadow::new();
        let count = s.pump_count();
        s.on_handed(40_000);
        s.on_handed(1_000);
        assert_eq!(s.pump.now(), 41_000);
        {
            let mut frame = count.unsent(40_000);
            frame.sent(16_384);
            assert_eq!(s.pump.now(), 24_616);
            frame.sent(16_384);
            frame.sent(7_232);
        }
        assert_eq!(s.pump.now(), 1_000);
        // A failure after one chunk: the drop releases the rest.
        {
            let mut frame = count.unsent(1_000);
            frame.sent(400);
            assert_eq!(s.pump.now(), 600);
        }
        assert_eq!(s.pump.now(), 0);
        // A release before its take (the send task can win the race) settles.
        drop(count.unsent(500));
        s.on_handed(500);
        assert_eq!(s.pump.now(), 0);
        // With the budget off the count counts nothing.
        let off = PumpCount::default();
        let mut frame = off.unsent(16_384);
        frame.sent(16_384);
    }

    /// FR-148 D46's stall classifier: busy at 2 Mbit/s with 40 000 bytes in
    /// flight, a still span from 2 s to 2.8 s with 60 000, and its release,
    /// then busy to 5.95 s with 8 000. A burst brings what was in flight back
    /// at once; a drain brings back the link's 12 500 bytes a step. One
    /// pending byte throughout, so no window binds. (ms, acked, in flight), a
    /// step every 50 ms.
    fn stall_then_release(burst: bool) -> Vec<(u64, u64, u64)> {
        let mut steps = Vec::new();
        let mut acked = 0;
        for t in (0..=1_950).step_by(50) {
            steps.push((t, acked, 40_000));
            acked += 12_500;
        }
        for t in (2_000..=2_800).step_by(50) {
            steps.push((t, acked, 60_000));
        }
        acked += if burst { 60_001 } else { 12_500 };
        steps.push((2_850, acked, if burst { 8_000 } else { 47_501 }));
        for t in (2_900..=5_950).step_by(50) {
            acked += 12_500;
            steps.push((t, acked, 8_000));
        }
        steps
    }

    fn feed(b: &mut Budget, t0: Instant, steps: &[(u64, u64, u64)]) {
        for &(t, acked, out) in steps {
            b.step(at(t0, t), &read(acked, wide(out, 1), 1_500_000));
        }
    }

    /// The budget after [`stall_then_release`], with a fresh window, and the
    /// clock's count for a step at 6 s that sees it move.
    fn released(burst: bool) -> (Budget, Instant, u64) {
        let mut b = Budget::new();
        let t0 = Instant::now();
        let steps = stall_then_release(burst);
        feed(&mut b, t0, &steps);
        let w = b.take_window();
        let class = if burst { (1, 0) } else { (0, 1) };
        assert_eq!((w.released, w.still), (class, (1, 800)));
        let acked = steps.last().unwrap().1 + 12_500;
        (b, t0, acked)
    }

    /// A still span at `acked`, a step every 50 ms from `from` to `to` ms with
    /// `out` bytes in flight and nothing pending: what holds here holds on
    /// the class, not on SCTP's queue.
    fn stand(
        b: &mut Budget,
        t0: Instant,
        from: u64,
        to: u64,
        acked: u64,
        out: u64,
    ) -> Vec<Verdict> {
        (from..=to)
            .step_by(50)
            .map(|t| b.step(at(t0, t), &read(acked, wide(out, 0), 1_500_000)))
            .collect()
    }

    /// [`stall_then_release`]'s steps to the span's last still reading at
    /// 2.8 s, and the count the clock stood at.
    fn to_the_last_still_reading() -> (Vec<(u64, u64, u64)>, u64) {
        let steps: Vec<_> = stall_then_release(true)
            .into_iter()
            .filter(|s| s.0 <= 2_800)
            .collect();
        let a = steps.last().unwrap().1;
        (steps, a)
    }

    #[test]
    fn a_burst_release_is_head_of_line_and_one_at_the_rate_a_drain() {
        for (burst, class) in [(true, Release::HeadOfLine), (false, Release::Drain)] {
            let steps = stall_then_release(burst);
            let mut b = Budget::new();
            let t0 = Instant::now();
            // Not read until the first step 50 ms after the last still
            // reading, at 2.8 s.
            let (to_2_8, rest) = steps.split_at(steps.iter().position(|s| s.0 == 2_850).unwrap());
            feed(&mut b, t0, to_2_8);
            assert_eq!(b.last_release(), None, "burst {burst}");
            feed(&mut b, t0, &rest[..1]);
            assert_eq!(b.last_release(), Some(class), "burst {burst}");
            let w = b.take_window();
            assert_eq!(w.still, (1, 800));
            let want = if burst { (1, 0) } else { (0, 1) };
            assert_eq!(w.released, want, "burst {burst}");
        }
        // The share is of what SCTP had in flight at the last still reading,
        // 60 000 bytes, and not of its pending byte: 36 000 back by 50 ms
        // after it is a burst at 600 per mille, 35 999 not. A step 25 ms after
        // it reads nothing yet.
        for (back, class) in [(36_000, Release::HeadOfLine), (35_999, Release::Drain)] {
            let mut b = Budget::new();
            let t0 = Instant::now();
            let (mut steps, a) = to_the_last_still_reading();
            steps.push((2_825, a + 1, 59_999));
            feed(&mut b, t0, &steps);
            assert_eq!(b.last_release(), None, "{back} back");
            feed(&mut b, t0, &[(2_850, a + back, 8_000)]);
            assert_eq!(b.last_release(), Some(class), "{back} back");
        }
    }

    #[test]
    fn a_release_whose_first_step_comes_too_late_to_tell_is_a_drain() {
        // The least and S are 300 ms here (no SACK's delay), so a first step
        // 180 ms after the last still reading, 600 per mille of them, is
        // late, and 179 ms is not: all 60 001 bytes back reads as a drain in
        // the one and as a burst in the other.
        for (late_ms, class) in [(180, Release::Drain), (179, Release::HeadOfLine)] {
            let mut b = Budget::new();
            let t0 = Instant::now();
            let (steps, a) = to_the_last_still_reading();
            feed(&mut b, t0, &steps);
            feed(&mut b, t0, &[(2_800 + late_ms, a + 60_001, 8_000)]);
            assert_eq!(b.last_release(), Some(class), "{late_ms} ms");
        }
    }

    #[test]
    fn the_first_still_span_holds() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        let acked = run(&mut b, t0, 0, 1_950, 0, 2_000_000, 40_000);
        // 2 Mbit/s × (S + S) = 75 000 bytes, and 100 000 out.
        let verdicts = stand(&mut b, t0, 2_000, 2_800, acked, 100_000);
        assert_eq!(b.last_budget(), 75_000);
        assert_eq!(b.last_release(), None);
        assert!(verdicts.iter().all(|&v| v == Verdict::Hold), "{verdicts:?}");
        let w = b.take_window();
        assert_eq!((w.still_admitted, w.released), (0, (0, 0)));
    }

    #[test]
    fn after_a_head_of_line_release_a_still_span_writes_through_at_the_clock_s_rate() {
        let (mut b, t0, acked) = released(true);
        assert_eq!(b.last_release(), Some(Release::HeadOfLine));
        // Until the clock has stood still for the least and S (300 ms with no
        // SACK's delay) the budget holds as before.
        let verdicts = stand(&mut b, t0, 6_000, 6_250, acked, 500_000);
        assert!(verdicts.iter().all(|&v| v == Verdict::Hold), "{verdicts:?}");
        // From then a frame goes while out < budget + rate × still_age, the
        // span's age from its first still reading, and no more.
        let probe = |b: &Budget, t: u64, out: u64| {
            let mut b = b.clone();
            let v = b.step(at(t0, t), &read(acked, wide(out, 0), 1_500_000));
            (v, b.last_budget(), b.rate_bps().unwrap())
        };
        for t in (6_300..=6_800).step_by(50) {
            let (_, budget, rate) = probe(&b, t, 1);
            let through = rate * (t - 6_000) * 1_000 / 8_000_000;
            assert!(through > 0, "at {t} ms");
            assert_eq!(
                probe(&b, t, budget + through - 1).0,
                Verdict::WritesThrough,
                "at {t} ms"
            );
            assert_eq!(probe(&b, t, budget + through).0, Verdict::Hold, "at {t} ms");
            assert_eq!(probe(&b, t, budget - 1).0, Verdict::Admit, "at {t} ms");
            // Each step of the span: a frame that goes.
            let v = b.step(
                at(t0, t),
                &read(acked, wide(budget + through - 1, 0), 1_500_000),
            );
            assert_eq!(v, Verdict::WritesThrough, "at {t} ms");
        }
        let w = b.take_window();
        assert_eq!(w.still_admitted, 11, "{w:?}");
        assert_eq!(w.held.0, 6, "the six steps before the span was still");
    }

    #[test]
    fn after_a_drain_release_a_still_span_holds() {
        let (mut b, t0, acked) = released(false);
        assert_eq!(b.last_release(), Some(Release::Drain));
        let verdicts = stand(&mut b, t0, 6_000, 6_800, acked, 500_000);
        assert!(verdicts.iter().all(|&v| v == Verdict::Hold), "{verdicts:?}");
        assert_eq!(b.take_window().still_admitted, 0);
    }

    #[test]
    fn a_new_association_and_a_new_pair_each_forget_the_class() {
        let (mut b, t0, _) = released(true);
        b.on_new_association();
        assert_eq!(b.last_release(), None);
        // The new association's clock counts from 0, and its first still span
        // holds.
        let verdicts = stand(&mut b, t0, 6_000, 6_800, 0, 500_000);
        assert!(verdicts.iter().all(|&v| v == Verdict::Hold), "{verdicts:?}");
        // Through the pumps' side, with a least of 60 ms: a new selected pair
        // forgets it too.
        let mut s = Shadow::new();
        let t0 = Instant::now();
        for (t, acked, out) in stall_then_release(true) {
            s.on_reading(at(t0, t), acked, delayed(wide(out, 1), 60, 60), 1_500_000);
        }
        s.on_pair(Some(pair(50_000)));
        assert_eq!(s.budget().last_release(), Some(Release::HeadOfLine));
        assert_eq!(s.budget().least(), Some(d(60)));
        s.on_pair(Some(pair(50_001)));
        assert_eq!(s.budget().last_release(), None);
        assert_eq!(s.budget().least(), None, "with the least");
    }

    /// The verdict on a frame that leaves `out` at the budget, at `t` ms on a
    /// clock standing at `acked`: B1's budget holds it.
    fn at_the_budget(b: &Budget, t0: Instant, t: u64, acked: u64) -> Verdict {
        let mut look = b.clone();
        look.step(at(t0, t), &read(acked, wide(1, 0), 1_500_000));
        let budget = look.last_budget();
        let mut b = b.clone();
        b.step(at(t0, t), &read(acked, wide(budget, 0), 1_500_000))
    }

    #[test]
    fn two_head_of_line_releases_in_a_row_when_two_are_asked() {
        let two = Classifier {
            releases: 2,
            ..Classifier::default()
        };
        let mut b = Budget::with_classifier(SLACK, two);
        let t0 = Instant::now();
        let steps = stall_then_release(true);
        feed(&mut b, t0, &steps);
        assert_eq!(b.last_release(), Some(Release::HeadOfLine));
        // One is not enough: 300 ms into the next still span, a frame at the
        // budget holds.
        let a = steps.last().unwrap().1 + 12_500;
        stand(&mut b, t0, 6_000, 6_250, a, 1);
        assert_eq!(at_the_budget(&b, t0, 6_300, a), Verdict::Hold);
        // That span's release is the second in a row: the next span writes
        // through.
        stand(&mut b, t0, 6_300, 6_800, a, 1);
        b.step(at(t0, 6_850), &read(a + 2, wide(8_000, 1), 1_500_000));
        b.step(at(t0, 6_900), &read(a + 12_502, wide(8_000, 1), 1_500_000));
        assert_eq!(b.last_release(), Some(Release::HeadOfLine));
        let a = a + 25_002;
        stand(&mut b, t0, 7_000, 7_250, a, 1);
        assert_eq!(at_the_budget(&b, t0, 7_300, a), Verdict::WritesThrough);
        // And a drain between starts the count over: 1 250 of the 10 000
        // bytes in flight back 50 ms after the span's last still reading.
        stand(&mut b, t0, 7_300, 7_300, a, 10_000);
        b.step(at(t0, 7_350), &read(a + 1_250, wide(8_000, 1), 1_500_000));
        assert_eq!(b.last_release(), Some(Release::Drain));
        let a = a + 12_500;
        stand(&mut b, t0, 7_500, 7_750, a, 1);
        assert_eq!(at_the_budget(&b, t0, 7_800, a), Verdict::Hold);
    }

    #[test]
    fn a_still_span_writes_through_only_while_sctp_queues_nothing() {
        let (mut b, t0, acked) = released(true);
        stand(&mut b, t0, 6_000, 6_250, acked, 1);
        // 300 ms into the span a frame over the budget goes with nothing
        // pending in SCTP, and holds with one byte pending: SCTP's window
        // binds, and what is written waits in its queue for the release.
        let mut look = b.clone();
        look.step(at(t0, 6_300), &read(acked, wide(1, 0), 1_500_000));
        let over = look.last_budget() + 1;
        let verdict = |pending: u64| {
            let mut b = b.clone();
            b.step(at(t0, 6_300), &read(acked, wide(over, pending), 1_500_000))
        };
        assert_eq!(verdict(0), Verdict::WritesThrough);
        assert_eq!(verdict(1), Verdict::Hold);
    }

    #[test]
    fn a_second_that_starts_inside_a_released_still_span_counts_from_its_first_still_reading() {
        // Busy at 2 Mbit/s, 12 500 bytes a 50 ms step; a head-of-line stall
        // from 2 s to 2.8 s through which the carrier serialized at the same
        // rate, its 225 000 bytes back over two steps, 50 000 at 2.85 s and
        // the rest at 2.9 s; then busy again.
        let mut b = Budget::new();
        let t0 = Instant::now();
        let mut acked = 0;
        for t in (0..=1_950).step_by(50) {
            b.step(at(t0, t), &read(acked, wide(40_000, 1), 1_500_000));
            acked += 12_500;
        }
        for t in (2_000..=2_800).step_by(50) {
            b.step(at(t0, t), &read(acked, wide(60_000, 1), 1_500_000));
        }
        let stood = acked;
        // The second from 2.8 s to 3.8 s counts from 2 s: 450 000 bytes over
        // 1.8 s are the link's 2 Mbit/s, where over the 1 s they would be 3.6
        // Mbit/s. So does the second from 2.85 s, which starts while the
        // release comes back (3.3 Mbit/s over its 1 s). The rate stays at the
        // link's.
        for t in (2_850..=4_000).step_by(50) {
            let acked = match t {
                2_850 => stood + 50_000,
                _ => stood + 250 * (t - 2_000),
            };
            b.step(at(t0, t), &read(acked, wide(40_000, 1), 1_500_000));
            assert_eq!(b.rate_bps(), Some(2_000_000), "at {t} ms");
        }
    }

    /// FR-148 D46 B2 — a step marks while a still span stands for the least
    /// and S (no least here: 300 ms), and where it reads the span's release;
    /// never while the clock moves.
    #[test]
    fn a_step_marks_a_still_span_that_stands_and_the_release_it_reads() {
        for burst in [true, false] {
            let mut b = Budget::new();
            let t0 = Instant::now();
            let mut marked = Vec::new();
            for &(t, acked, out) in &stall_then_release(burst) {
                b.step(at(t0, t), &read(acked, wide(out, 1), 1_500_000));
                if b.take_mark() {
                    marked.push(t);
                }
            }
            assert!(!b.take_mark(), "a take clears it");
            let want: Vec<u64> = (2_300..=2_850).step_by(50).collect();
            assert_eq!(marked, want, "burst {burst}");
        }
    }

    /// FR-148 D46 B2 — what the budget saw over a delivery sample's span,
    /// after [`stall_then_release`]: the still span meets a span from its
    /// first still reading (2 s) to the step that read its release (2.85 s),
    /// stood its 300 ms inside one that starts before 2.5 s, and starts one
    /// inside it with the bytes back since 2 s; a span after the release, or
    /// before the stall, saw none of it, though one from the first step
    /// starts in the opening (the first measured rate is at 1 s). Nothing
    /// held there.
    #[test]
    fn the_budget_says_what_it_saw_over_a_samples_span() {
        let (b, t0, _) = released(true);
        let steps = stall_then_release(true);
        let count = |t: u64| steps.iter().find(|s| s.0 == t).unwrap().1;
        let stood = count(2_000);
        let inside = b.over(at(t0, 2_400), at(t0, 4_400), stood);
        assert_eq!(
            inside,
            Over {
                stood: true,
                still_inside: true,
                held: false,
                released_from: Some((d(400), 0)),
                opening: false,
                ramp: false,
                rate: b.rate_bps(),
            }
        );
        // Met by its last 200 ms of stand and its release only.
        let late = b.over(at(t0, 2_600), at(t0, 4_600), stood);
        assert!(late.stood && !late.still_inside, "{late:?}");
        // From the release's own step: 0.85 s after the first still
        // reading, with the burst back.
        let back = b.over(at(t0, 2_850), at(t0, 4_850), count(2_850));
        assert_eq!(back.released_from, Some((d(850), count(2_850) - stood)));
        assert!(back.stood);
        for (from, to) in [(2_900, 4_900), (0, 1_950)] {
            assert_eq!(
                b.over(at(t0, from), at(t0, to), count(from)),
                Over {
                    opening: from == 0,
                    rate: b.rate_bps(),
                    ..Over::default()
                },
                "{from} to {to} ms"
            );
        }
        // Before the first step it knows nothing, and measured nothing.
        assert_eq!(
            Budget::new().over(t0, at(t0, 2_000), 0),
            Over {
                opening: true,
                ..Over::default()
            }
        );
    }

    /// FR-148 D46 B2 — the holds over a sample's span: one in each of its
    /// whole seconds, or not.
    #[test]
    fn the_budget_says_whether_it_held_in_each_second_of_a_span() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        let acked = run(&mut b, t0, 0, 2_950, 0, 2_000_000, 10 << 20);
        assert!(b.over(at(t0, 900), at(t0, 2_900), 0).held);
        assert!(
            b.over(at(t0, 900), at(t0, 2_400), 0).held,
            "one whole second"
        );
        run(&mut b, t0, 3_000, 4_950, acked, 2_000_000, 1);
        assert!(b.over(at(t0, 2_000), at(t0, 2_990), 0).held);
        assert!(
            !b.over(at(t0, 2_000), at(t0, 4_000), 0).held,
            "none in its second second"
        );
        assert!(!b.over(at(t0, 3_000), at(t0, 4_950), 0).held);
    }

    /// FR-148 D48 — B2's `held` on an application-limited writer: one hold
    /// in the grain a span starts in, before its start (an outage's release
    /// draining), and one in the grain it ends in, after its end (the next
    /// outage filling the budget), are the edges', not the span's; nor is one
    /// inside it in a grain it only partly covers, as the mask keeps no finer
    /// time. A hold in a grain wholly inside it is the span's: in a span of
    /// four whole grains, a quarter of them, over [`HELD_SHARE_PERCENT`].
    #[test]
    fn a_hold_in_a_span_s_edge_grains_is_not_the_span_s() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        let hold = |b: &mut Budget, t: u64, acked: u64| {
            let verdict = b.step(at(t0, t), &read(acked, wide(10 << 20, 1), 1_500_000));
            assert_eq!(verdict, Verdict::Hold, "at {t} ms");
        };
        let acked = run(&mut b, t0, 0, 1_950, 0, 2_000_000, 1);
        hold(&mut b, 2_010, acked);
        let acked = run(&mut b, t0, 2_050, 3_950, acked, 2_000_000, 1);
        hold(&mut b, 3_990, acked);
        run(&mut b, t0, 4_000, 4_950, acked, 2_000_000, 1);
        // Grains 2.00-2.05 s and 3.95-4.00 s: the holds fall outside the span.
        assert!(!b.over(at(t0, 2_030), at(t0, 3_970), 0).held);
        // Inside it, but in the grains it only partly covers.
        assert!(!b.over(at(t0, 2_005), at(t0, 3_995), 0).held);
        assert!(
            !b.over(at(t0, 2_005), at(t0, 2_045), 0).held,
            "no whole grain"
        );
        // Wholly inside, the grain a span starts in holds its hold, and so
        // does the grain it ends in; ended 5 ms short, the one hold is in a
        // grain the span partly covers.
        assert!(b.over(at(t0, 1_990), at(t0, 2_240), 0).held);
        assert!(b.over(at(t0, 3_760), at(t0, 4_000), 0).held);
        assert!(!b.over(at(t0, 3_760), at(t0, 3_995), 0).held);
    }

    /// FR-148 D48 — B2's `held` takes a share: a second is held when the
    /// verdicts held a frame in at least a fifth of its whole grains
    /// ([`HELD_SHARE_PERCENT`]). Eight holds in 39 grains are; seven are not;
    /// one hold 30 ms into a span, an application-limited writer's outage
    /// draining as on the `tcp_shape` 80 % outage cells, is not; and each of
    /// a span's seconds needs its own share.
    #[test]
    fn a_second_is_held_when_a_fifth_of_its_whole_grains_are() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        let acked = run(&mut b, t0, 0, 1_950, 0, 2_000_000, 1);
        let acked = run(&mut b, t0, 2_000, 2_350, acked, 2_000_000, 10 << 20);
        let acked = run(&mut b, t0, 2_400, 3_950, acked, 2_000_000, 1);
        let verdict = b.step(at(t0, 4_030), &read(acked, wide(10 << 20, 1), 1_500_000));
        assert_eq!(verdict, Verdict::Hold);
        let held = |b: &Budget, from: u64, to: u64| b.over(at(t0, from), at(t0, to), 0).held;
        assert!(held(&b, 2_000, 3_990), "8 of its 39 grains");
        assert!(!held(&b, 2_050, 4_040), "7 of 39");
        assert!(!held(&b, 2_000, 4_000), "none in its second second");
        assert!(held(&b, 2_000, 2_990), "8 of 19");
        run(&mut b, t0, 4_050, 5_950, acked, 2_000_000, 1);
        assert!(!held(&b, 4_000, 5_990), "one hold, 30 ms in");
        assert!(held(&b, 4_000, 4_200), "the same hold, one of four grains");
    }

    /// FR-148 D48, rule 4 — a hold SCTP's congestion window sized, in a
    /// grain wholly inside a span, is the association's ramp
    /// ([`Over::ramp`]): a 15 kB window over the 150 ms least (none read
    /// yet) carries 800 kbit/s, under the 2 Mbit/s rate, and 30 kB out holds.
    /// The congestion window the smaller window, or equal to the receive
    /// window and what is in flight, it is the ramp's; the receive window the
    /// smaller, the viewer's; a wide window, the rate's. Neither of those is,
    /// though both hold. A hold in an edge grain is not the span's, as for
    /// `held`, and a new association forgets them. The count of losses the
    /// window rules read never runs back across associations.
    #[test]
    fn a_hold_sctp_s_congestion_window_sized_is_the_ramp_s() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        let hold = |b: &mut Budget, t: u64, acked: u64, gauges: Gauges| {
            let verdict = b.step(at(t0, t), &read(acked, gauges, 1_500_000));
            assert_eq!(verdict, Verdict::Hold, "at {t} ms");
        };
        let span = |b: &Budget, from: u64, to: u64| b.over(at(t0, from), at(t0, to), 0);
        let acked = run(&mut b, t0, 0, 1_950, 0, 2_000_000, 10 << 20);
        assert!(near(b.rate_bps(), 2_000_000), "{:?}", b.rate_bps());
        assert!(!span(&b, 0, 1_950).ramp, "the rate's holds");
        hold(&mut b, 2_010, acked, g(15_000, 1 << 20, 10_000, 20_000));
        let acked = run(&mut b, t0, 2_050, 2_950, acked, 2_000_000, 1);
        assert!(span(&b, 2_000, 2_950).ramp, "the congestion window's");
        assert!(!span(&b, 2_030, 2_950).ramp, "an edge grain's");
        hold(&mut b, 3_010, acked, g(15_000, 5_000, 10_000, 20_000));
        let acked = run(&mut b, t0, 3_050, 3_950, acked, 2_000_000, 1);
        assert!(span(&b, 3_000, 3_950).ramp, "the equal case");
        hold(&mut b, 4_010, acked, g(1 << 20, 5_000, 10_000, 20_000));
        let acked = run(&mut b, t0, 4_050, 4_950, acked, 2_000_000, 1);
        hold(&mut b, 5_010, acked, wide(10 << 20, 1));
        let acked = run(&mut b, t0, 5_050, 5_950, acked, 2_000_000, 1);
        for from in [4_000, 5_000] {
            let o = span(&b, from, from + 200);
            assert!(o.held && !o.ramp, "{from} ms: {o:?}");
        }
        assert!(span(&b, 3_000, 5_950).ramp);
        let loss = |b: &mut Budget, t: u64, fast: u64, t3: u64| {
            let gauges = Gauges {
                fast_retransmits: fast,
                t3_expiries: t3,
                ..wide(1, 0)
            };
            b.step(at(t0, t), &read(acked, gauges, 1_500_000));
            b.loss()
        };
        assert_eq!(loss(&mut b, 6_000, 3, 1), Some(4));
        b.on_new_association();
        assert!(!span(&b, 3_000, 5_950).ramp, "a new association's");
        assert_eq!(b.loss(), None);
        assert_eq!(loss(&mut b, 6_050, 0, 0), Some(4), "never runs back");
        assert_eq!(loss(&mut b, 6_100, 1, 0), Some(5));
    }

    /// FR-148 D46 B2 — the budget's opening, which a P3 sample may not start
    /// in: a span that starts before the association's first measured rate
    /// is the opening's ([`Over::opening`]), one that starts at or after it
    /// is not. Held throughout from the first step, the first figure (1 s)
    /// gives the rate. A new association opens again, its opening rate the
    /// old one's rate. While the application-limited rule keeps the opening
    /// rate (the figures under it, no second held throughout), nothing is
    /// measured, and the first second held throughout ends the opening.
    #[test]
    fn a_span_that_starts_before_the_first_measured_rate_is_the_opening() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        run(&mut b, t0, 0, 2_950, 0, 2_000_000, 10 << 20);
        assert!(b.over(t0, at(t0, 2_900), 0).opening);
        assert!(b.over(at(t0, 950), at(t0, 2_900), 0).opening);
        assert!(!b.over(at(t0, 1_000), at(t0, 2_900), 0).opening);
        b.on_new_association();
        run(&mut b, t0, 3_000, 4_950, 0, 2_000_000, 10 << 20);
        assert!(b.over(at(t0, 3_000), at(t0, 4_950), 0).opening);
        assert!(!b.over(at(t0, 4_000), at(t0, 4_950), 0).opening);
        // 1 Mbit/s under the 1.5 Mbit/s opening rate, one byte out: the
        // opening rate stands, and every span is the opening's.
        let mut kept = Budget::new();
        let acked = run(&mut kept, t0, 0, 2_950, 0, 1_000_000, 1);
        assert_eq!(kept.rate_bps(), Some(1_500_000));
        assert!(kept.over(at(t0, 2_000), at(t0, 2_950), 0).opening);
        // Held from 3 s: a second held throughout gives the rate by 4 s.
        run(&mut kept, t0, 3_000, 4_950, acked, 1_000_000, 10 << 20);
        assert_eq!(kept.rate_bps(), Some(1_000_000));
        assert!(kept.over(at(t0, 2_000), at(t0, 4_950), 0).opening);
        assert!(!kept.over(at(t0, 4_000), at(t0, 4_950), 0).opening);
    }

    /// FR-148 D46 B3 — the budget `on`, as a pump reads the switch, on the
    /// core `b`.
    fn on_with(b: Budget) -> Shadow {
        let mut s = Shadow::for_pump(Mode::On, true).expect("on computes");
        s.budget = b;
        s
    }

    /// The pass the budget `on` answers on a relay session for `r` at `t` ms,
    /// beside the verdict its core gives for it.
    fn relay_pass(
        s: &mut Shadow,
        t0: Instant,
        t: u64,
        r: &Reading,
        byte_gate: bool,
    ) -> (Verdict, Pass) {
        let verdict = s.budget.clone().step(at(t0, t), r);
        let pass = s.gate_reading(
            at(t0, t),
            r.acked_bytes,
            r.gauges,
            r.open_bps,
            true,
            byte_gate,
        );
        (verdict, pass)
    }

    /// FR-148 D46 B3 — under `on`, on a relay session, only a hold shuts the
    /// pass: nothing out, under the budget, SCTP's window binding, and a still
    /// span written through each admit it, whatever the byte gate says. None
    /// of them is a stall's, and the watermark sheds nothing.
    #[test]
    fn under_on_every_verdict_but_hold_admits_a_relay_pass() {
        // 2 Mbit/s with a least of 50 ms: a budget of 50 000 bytes.
        let (b, t0) = at_2_mbps_50_ms();
        let at_50 = |g: Gauges| read(0, delayed(g, 50, 50), 2_000_000);
        let cases = [
            ("nothing out", Verdict::NothingOut, at_50(wide(0, 0))),
            (
                "under the budget",
                Verdict::Admit,
                at_50(wide(40_000, 9_999)),
            ),
            (
                "SCTP's window binds",
                Verdict::WindowBinds,
                at_50(g(48_000, 64 << 20, 60_000, 0)),
            ),
            ("at the budget", Verdict::Hold, at_50(wide(40_000, 10_000))),
        ];
        for (what, want, r) in cases {
            for byte_gate in [false, true] {
                let mut s = on_with(b.clone());
                let (verdict, mut pass) = relay_pass(&mut s, t0, 10, &r, byte_gate);
                assert_eq!(verdict, want, "{what}");
                assert!(pass.acts(), "{what}");
                assert_eq!(
                    pass.shuts(),
                    verdict.holds(),
                    "{what}, byte gate {byte_gate}"
                );
                assert!(pass.decreases(false), "{what}: no still span");
                assert!(!pass.sheds(true), "{what}");
            }
        }
        // Written through a still span after a head-of-line release.
        let (mut b, t0, acked) = released(true);
        stand(&mut b, t0, 6_000, 6_250, acked, 1);
        let mut look = b.clone();
        look.step(at(t0, 6_300), &read(acked, wide(1, 0), 1_500_000));
        let over = read(acked, wide(look.last_budget() + 1, 0), 1_500_000);
        let mut s = on_with(b);
        let (verdict, mut pass) = relay_pass(&mut s, t0, 6_300, &over, true);
        assert_eq!(verdict, Verdict::WritesThrough);
        assert!(s.budget().stalled(), "in a still span");
        assert!(!pass.shuts(), "written through");
        assert!(!pass.stalled() && pass.decreases(false));
    }

    /// FR-148 D46 B3 — the no-decrease rule. A hold while the clock has stood
    /// still for the least and S with bytes out is a stall of the carrier: it
    /// shuts the pass and calls no AIMD decrease. A hold before that
    /// decreases, and so does a pass the send channel or a follower shut while
    /// the budget held, since the hold did not shut it. Inside FR-35's opener
    /// grace nothing decreases, as today. The line counts the holds that
    /// acted, and the stalled ones among them.
    #[test]
    fn under_on_a_hold_in_a_still_span_calls_no_decrease() {
        let mut b = Budget::new();
        let t0 = Instant::now();
        let acked = run(&mut b, t0, 0, 1_950, 0, 2_000_000, 40_000);
        let _ = b.take_window();
        let mut s = on_with(b);
        // The first still span holds: 2 Mbit/s × (S + S) = 75 000 bytes, and
        // 100 000 out from 2 s. With no SACK's delay the least and S are 300 ms.
        let r = read(acked, wide(100_000, 0), 1_500_000);
        for t in (2_000..=2_800).step_by(50) {
            let mut pass =
                s.gate_reading(at(t0, t), r.acked_bytes, r.gauges, r.open_bps, true, false);
            let still = t >= 2_300;
            assert!(s.shut(&mut pass), "at {t} ms");
            assert_eq!(pass.stalled(), still, "at {t} ms");
            assert_eq!(pass.decreases(false), !still, "at {t} ms");
            assert!(
                !pass.decreases(true),
                "inside the opener's grace, at {t} ms"
            );
        }
        // A pass shut before the budget's term was asked: its skip decreases.
        let pass = s.gate_reading(
            at(t0, 2_850),
            r.acked_bytes,
            r.gauges,
            r.open_bps,
            true,
            false,
        );
        assert!(s.budget().stalled());
        assert!(!pass.held() && !pass.stalled() && pass.decreases(false));
        let w = s.take_window();
        assert_eq!(w.held, (18, 18));
        assert_eq!((w.acted, w.acted_stalled), ((17, 18), 11));
    }

    /// FR-148 D46 B3 — the budget acts only under `on`, on a constrained
    /// session. A direct session under `on`, and any session in shadow, gets
    /// today's pass: the gate reads and steps nothing (the step stays after
    /// the gate, as in shadow), the byte gate shuts it, the watermark sheds,
    /// and its skips decrease outside the grace.
    #[test]
    fn the_budget_acts_only_under_on_on_a_constrained_session() {
        let t0 = Instant::now();
        // A megabyte out: the budget holds.
        let deep = read(0, delayed(wide(1 << 20, 1), 50, 50), 2_000_000);
        for mode in [Mode::Shadow, Mode::On] {
            for (constrained, byte_gate) in
                [(false, false), (false, true), (true, false), (true, true)]
            {
                let mut s = Shadow::for_pump(mode, true).unwrap();
                let mut pass = s.gate_reading(
                    t0,
                    deep.acked_bytes,
                    deep.gauges,
                    deep.open_bps,
                    constrained,
                    byte_gate,
                );
                let acts = mode == Mode::On && constrained;
                let what = format!("{mode:?}, constrained {constrained}, byte gate {byte_gate}");
                assert_eq!(pass.acts(), acts, "{what}");
                assert_eq!(pass.shuts(), acts || byte_gate, "{what}");
                assert_eq!(pass.sheds(true), !acts, "{what}");
                assert!(pass.decreases(false) && !pass.decreases(true), "{what}");
                let w = s.take_window();
                assert_eq!(w.held, (u64::from(acts), u64::from(acts)), "{what}");
                assert_eq!(w.acted, (0, u64::from(acts)), "{what}");
                assert_eq!(w.bounds.1, u64::from(acts && byte_gate), "{what}");
            }
        }
    }

    /// FR-148 D46 B3 — under `on`, on a relay session, today's two bounds act
    /// on nothing and are still counted where they fire: the byte gate at the
    /// gate, the watermark at its arm. The line counts the holds that shut a
    /// pass by themselves, apart from a pass the send channel shut first. In
    /// shadow the bounds are the pump's own calls to count
    /// (`DeliverySampler::note_gate`, `note_watermark`), and these count
    /// nothing.
    #[test]
    fn under_on_the_byte_gate_and_the_watermark_fire_and_act_on_nothing() {
        let t0 = Instant::now();
        let nothing = read(0, delayed(wide(0, 0), 50, 50), 2_000_000);
        let deep = read(0, delayed(wide(1 << 20, 1), 50, 50), 2_000_000);
        let mut s = Shadow::for_pump(Mode::On, true).unwrap();
        // Nothing out and the byte gate shut: the pass goes, and a frame over
        // the watermark is not shed.
        let mut pass = s.gate_reading(t0, 0, nothing.gauges, 2_000_000, true, true);
        assert!(!s.shut(&mut pass));
        assert!(!s.shed(&pass, true));
        assert!(!s.shed(&pass, false));
        // A megabyte out and the byte gate open: the hold shuts the pass.
        let mut pass = s.gate_reading(at(t0, 10), 0, deep.gauges, 2_000_000, true, false);
        assert!(s.shut(&mut pass) && pass.held());
        // Again, but the send channel was full: the budget's term is not
        // asked, and its hold acted on nothing.
        let _ = s.gate_reading(at(t0, 20), 0, deep.gauges, 2_000_000, true, true);
        let w = s.take_window();
        assert_eq!(w.held, (2, 3), "the core's verdicts");
        assert_eq!((w.acted, w.acted_stalled), ((1, 3), 0));
        assert_eq!(
            w.bounds,
            (1, 2, 0),
            "one watermark firing, two of the byte gate"
        );
        // In shadow: today's pass, and nothing counted here.
        let mut s = Shadow::for_pump(Mode::Shadow, true).unwrap();
        let mut pass = s.gate_reading(t0, 0, deep.gauges, 2_000_000, true, true);
        assert!(s.shut(&mut pass) && !pass.held());
        assert!(s.shed(&pass, true));
        assert_eq!(
            s.take_window(),
            WindowReport {
                slack_ms: 150,
                ..WindowReport::default()
            }
        );
    }
}

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
//!
//! [`Budget`] is the core: it reads no clock and does no I/O; it takes the time
//! and the readings as arguments, its rings are fixed (no allocation per
//! frame), and it keeps the counters of the `path delivery` line's window.
//! [`Shadow`] is the pump's side: once per frame, before capture, it reads the
//! association's clock and gauges (by `peek`, so the delivery sampler's takes
//! stay its own, FR-148 O10) and steps the core. In B1 the verdict reaches
//! those counters and nothing else: no gate, no AIMD call, no keyframe request.
//!
//! The switch is `ROOMLERD_INFLIGHT_BUDGET` (config key `inflight_budget`):
//! `off`, the default, computes nothing; `shadow` computes and writes the
//! fields; `on` runs as `shadow` until B3, with one warning a pump. See
//! `docs/rate-control.md` (§ The in-flight budget).

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

/// `RATE_WINDOW / GRAIN` readings and the one the window starts from, with room.
const CLOCK_SLOTS: usize = 32;
/// `RATE_HOLD / GRAIN` figures and one, with room.
const FIGURE_SLOTS: usize = 48;

/// The switch, `ROOMLERD_INFLIGHT_BUDGET` (D46 item 7). In B1 `on` runs as
/// `shadow`.
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

    /// Anything but `off` computes the budget. In B1 none acts on it.
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
    /// `out >= budget`.
    Hold,
}

impl Verdict {
    pub fn holds(self) -> bool {
        self == Verdict::Hold
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
/// reading at it to the last.
#[derive(Clone, Copy, Debug, Default)]
struct Still {
    acked: u64,
    from: u64,
    to: u64,
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
        }
    }
}

fn us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
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
    last_acked: Option<u64>,
    still: Option<Still>,
    /// The last step's budget, `out` and rate.
    last: (u64, u64, Option<u64>),
    window: Window,
}

impl Default for Budget {
    fn default() -> Self {
        Self::new()
    }
}

impl Budget {
    pub fn new() -> Self {
        Self::slacked(SLACK)
    }

    /// The tcp_shape harness's slack sweep only: S is not a knob.
    #[cfg(test)]
    pub(crate) fn with_slack(slack: Duration) -> Self {
        Self::slacked(slack)
    }

    fn slacked(slack: Duration) -> Self {
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
            last_acked: None,
            still: None,
            last: (0, 0, None),
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
        let rate = self.rate_at(t, r.acked_bytes, r.open_bps);
        self.watch_still(t, r);
        let budget = self.size(rate, &r.gauges);
        let out = r.out();
        let verdict = if out == 0 {
            Verdict::NothingOut
        } else if out < budget {
            Verdict::Admit
        } else if window_binds(&r.gauges) {
            Verdict::WindowBinds
        } else {
            Verdict::Hold
        };
        if verdict.holds() {
            self.age_holds(t);
            self.holds |= 1;
        }
        self.last = (budget, out, rate);
        let w = &mut self.window;
        w.offered += 1;
        w.held += u64::from(verdict.holds());
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

    /// The selected candidate pair changed: the old path's least is no floor
    /// for the new one's.
    pub fn reset_least(&mut self) {
        self.least = None;
    }

    /// A new association: its clock counts from 0 and its delays are its own.
    /// The standing rate stays, as the opening rate of the new one.
    pub fn on_new_association(&mut self) {
        self.least = None;
        self.seen = (None, None);
        self.clock.clear();
        self.figures.clear();
        self.holds = 0;
        self.last_acked = None;
        self.still = None;
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
            return opening;
        };
        let bps =
            acked.saturating_sub(from.acked).saturating_mul(8_000_000) / t.saturating_sub(from.at);
        self.add_figure(t, bps);
        while self
            .figures
            .oldest()
            .is_some_and(|f| t.saturating_sub(f.last) > us(RATE_HOLD))
        {
            self.figures.pop_oldest();
        }
        let most = self.figures.iter().map(|f| f.bps).max().unwrap_or(bps);
        let rate = if self.held_throughout(from.at, t) {
            most
        } else {
            most.max(opening.unwrap_or(0))
        };
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

    /// The holds' mask, moved on to `t`'s grain.
    fn age_holds(&mut self, t: u64) {
        let grain = t / us(GRAIN);
        if grain > self.hold_grain {
            let by = grain - self.hold_grain;
            self.holds = if by >= 64 { 0 } else { self.holds << by };
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
    /// the least and S.
    fn watch_still(&mut self, t: u64, r: &Reading) {
        let out = r.sctp_out() > 0;
        if out
            && let Some(s) = self.still.as_mut()
            && s.acked == r.acked_bytes
        {
            s.to = t;
            return;
        }
        if let Some(s) = self.still.take() {
            let len = s.to.saturating_sub(s.from);
            if len >= us(self.still_after()) {
                self.window.still.0 += 1;
                self.window.still.1 = self.window.still.1.max(len / 1_000);
            }
        }
        if out {
            self.still = Some(Still {
                acked: r.acked_bytes,
                from: t,
                to: t,
            });
        }
    }

    /// The budget, bytes, at `rate` against the gauges now.
    fn size(&self, rate: Option<u64>, g: &Gauges) -> u64 {
        let least = self.least.unwrap_or(self.slack);
        let window = u64::from(g.cwnd).min(u64::from(g.rwnd).saturating_add(g.inflight_bytes));
        let by_window = window.saturating_mul(8_000_000) / us(least).max(1);
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

/// The pump's side of the budget in B1: the association's handles, the pair
/// it last read, its own unsent bytes, and the core. Every call returns
/// nothing to the pump: the verdict reaches the window's counters only.
#[derive(Debug)]
pub struct Shadow {
    budget: Budget,
    clock: Option<DeliveryClock>,
    gauges: Option<GaugeReader>,
    pair: PairWatch<RTCIceCandidatePair>,
    pump: Arc<PumpBytes>,
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
        }
    }

    /// The pump's budget, from the switch it read: none when `off`, and none
    /// when the delivery sampler is off, since its line carries the fields.
    /// `on` runs as `shadow` in B1, with one warning.
    pub fn for_pump(mode: Mode, sampler_on: bool) -> Option<Self> {
        match mode {
            Mode::Off => return None,
            Mode::On => tracing::warn!(
                "ROOMLERD_INFLIGHT_BUDGET=on runs as shadow until FR-148 D46 B3: \
                 the in-flight budget is computed and holds nothing"
            ),
            Mode::Shadow => {}
        }
        if !sampler_on {
            tracing::info!(
                inflight_budget = mode.word(),
                "the in-flight budget stays off: ROOMLERD_PATH_MODEL=off writes no \
                 path delivery line to carry it (FR-148 D46)"
            );
            return None;
        }
        Some(Self::new())
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
        let open = if applied_bps > 0 {
            applied_bps
        } else {
            open_at_bps.unwrap_or(0)
        };
        self.on_frame_with(
            Instant::now(),
            u64::from(open),
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
            return;
        };
        let (acked, peeked) = (clock.read().acked_bytes, gauges.peek());
        self.on_reading(now, acked, peeked, open_bps);
    }

    /// One frame's reading, read. The verdict is counted, and goes nowhere
    /// else.
    pub fn on_reading(&mut self, now: Instant, acked_bytes: u64, gauges: Gauges, open_bps: u64) {
        let reading = Reading {
            acked_bytes,
            gauges,
            pump_bytes: self.pump.now(),
            open_bps,
        };
        let _ = self.budget.step(now, &reading);
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

    /// The core, for a test's look.
    #[cfg(test)]
    pub(crate) fn budget(&self) -> &Budget {
        &self.budget
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
        assert!(Shadow::for_pump(Mode::Shadow, true).is_some());
        assert!(
            Shadow::for_pump(Mode::On, true).is_some(),
            "on runs as shadow in B1"
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
}

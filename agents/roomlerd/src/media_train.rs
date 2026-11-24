// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! FR-148 P0, step 4a: the open-time train's reading, as a pure state machine.
//!
//! At a session's open the encoder takes 0.65 to 2.8 s to start. The train
//! uses that time to read the path: it writes bytes on a stream of its own,
//! lets SCTP's slow start step them up, stops at the first push-back, and
//! reads the rate from SCTP's acknowledgements. This module is the reading's
//! core and nothing more: [`Train::step`] takes a reading of the
//! association's acknowledgement clock and gauges and the time, and answers
//! "write n bytes" or "stopped, and this is the reading". No session runs it
//! yet (FR-148 P0's step 4b wires it in, behind a switch); the cells below
//! drive it over the real vendored SCTP through `media_delivery`'s shaped
//! link. While a train runs, its writer keeps the association's pacer on
//! ([`Pacing`]), steered after every step to what [`Train::pace`] answers,
//! and off from the stop on (D43, D64).

use std::collections::VecDeque;
use std::time::Duration;

use webrtc::sctp::association::delivery::Delivery;
use webrtc::sctp::association::gauges::Gauges;
use webrtc::sctp::association::pacer::{PaceSwitch, WHOLE};

/// Bytes of SCTP payload a train writes at most (D38).
pub const BUDGET: u64 = 512 * 1024;

/// How long a train runs at most (D38).
pub const DEADLINE: Duration = Duration::from_millis(2_500);

/// A round whose least acknowledgement delay sits this far above the least
/// seen has a standing queue (P-d's floor). The stop's threshold adds one
/// message's serialisation at the estimated rate: an acknowledgement of two
/// packets sent together carries the first one's serialisation, which at 0.4
/// Mbit/s is 24.6 ms with no queue at all.
const STANDING: Duration = Duration::from_millis(10);

/// A sample this far above the least seen shows the bottleneck queued: the
/// in-flight cap applies from then on. It is P-d's floor too: two packets
/// sent together at 2 Mbit/s already put 4.9 ms between their samples, and a
/// cap that starts on that holds a climb at its first round's rate.
const QUEUED: Duration = Duration::from_millis(10);

/// The in-flight cap's queue: a third of the least round trip or of this
/// floor, whichever is longer. P-e allows a whole one; the rest is for an
/// estimate that reads high.
const QUEUE_FLOOR: Duration = Duration::from_millis(50);

/// The first sample of a train acknowledges two packets sent together: when
/// it sits this far above the least sample, the difference is the first
/// packet's serialisation, a packet-pair estimate of the rate.
const PAIR_LEAD: Duration = Duration::from_millis(10);

/// An estimate from the spacing of acknowledgements spans at least this, and
/// at least [`DISPERSION_ACKS`] SACKs: a SACK comes a few milliseconds early
/// or late, and over a single SACK's interval at 2 Mbit/s (9.8 ms) that read
/// 30 % high.
const DISPERSION_SPAN: Duration = Duration::from_millis(10);
const DISPERSION_ACKS: usize = 4;

/// A round is judged on delay at its end only with this many samples, as
/// HyStart++ judges a round only with N_RTT_SAMPLE (RFC 9406 §4.2): the
/// least of one or two says little, since a SACK the scheduler held reads as
/// a queue. A run of this many samples in a row, every one standing, judges
/// the queue without waiting for a round's end, once it spans half the least
/// round trip and [`EARLY_SPAN`]: the bottleneck was busy all that time, and
/// what it delivered then is the link's rate. A sample that does not stand
/// ends the run, since the link idled before its packet, as it does after a
/// delayed acknowledgement holds the sender; so does a silence longer than
/// the queue the next sample shows and a message's serialisation, in which
/// the bottleneck may have emptied before a burst queued behind itself.
const ROUND_SAMPLES: u32 = 4;
const EARLY_SPAN: Duration = Duration::from_millis(50);

/// Once a slow link's bottleneck queued, the train writes no more than what
/// had been acknowledged then and what its estimate of the rate carries in
/// this long; then it writes nothing, and the judgment comes from what is in
/// flight. At 0.4 Mbit/s and 250 ms that is about 8 KB and 34 KB, inside
/// P-f's 64 KB for an estimate up to 1.6 times the link's rate, however late
/// the judgment comes; and the packets it leaves in flight carry the run of
/// samples that judges the queue.
const WRITE_SPAN: Duration = Duration::from_millis(700);

/// The train stops this long before [`DEADLINE`], or two step intervals
/// before it where steps come further apart: a step can come late.
const DEADLINE_LEAD: Duration = Duration::from_millis(25);

/// The fewest messages the in-flight cap allows: a receiver acknowledges
/// every second packet at once and a lone one only when its delayed
/// acknowledgement fires, up to [`DELAYED_ACK`] after it arrived, so two
/// keep the acknowledgements coming and a third keeps the link busy while
/// they return.
const CAP_FLOOR_MESSAGES: u64 = 3;

/// Rounds held at the cap, and never by SCTP's windows, each delivering less
/// than 10 % more than the one before, that make a plateau: the path is
/// full. The cap lets every round put a third more in flight than the
/// estimate's pipe, so a round it held grows unless the path did not. At 0.4
/// Mbit/s and 20 ms, three messages fill the path and no sample shows a
/// queue. A round a window held grows as SCTP's window does, which in
/// congestion avoidance is one packet a round: less than 10 % above ten.
const PLATEAU_ROUNDS: u32 = 2;

/// The longest a receiver holds an acknowledgement (RFC 4960 §6.2, and a
/// browser's SCTP): a lone packet waits this long for its SACK.
const DELAYED_ACK: Duration = Duration::from_millis(200);

/// SCTP's common header and a DATA chunk's header: what each packet of a
/// message that fills one chunk adds to it.
const PACKET_OVERHEAD: u64 = 12 + 16;

/// The share of P-e's bound, max(the least round trip, [`QUEUE_FLOOR`]),
/// that [`pace_share`]'s derivation leaves out: the queue it predicts stays
/// within the rest. The derivation leaves out a step's lateness, the second
/// packet a SACK waits for, and the serialisation a least round trip
/// carries; its predictions sat within 15 % of what the cells measured. A
/// quarter is also the most that keeps the whole window at a 250 ms round
/// trip, where the whole window's queue is 0.73 of one.
const PACE_MARGIN: f64 = 0.25;

/// The share of the congestion window, in thousandths ([`WHOLE`] is all of
/// it), that the association's pacer lets out per least round trip in slow
/// start until the bottleneck queued: the largest up to the whole window
/// whose queue, by the derivation below, stays within `1 - PACE_MARGIN` of
/// P-e's bound. It comes from the least round trip and [`QUEUED`] alone.
///
/// Paced at a share G of cwnd over the least round trip R, with every SACK
/// adding what it acknowledges to cwnd (slow start, while chunks wait), the
/// window and the rate grow as e^(x t / R), where x e^x = G. Once the rate
/// passes the link the SACKs still carry what left a round trip before, so
/// the rate keeps that growth for one more round trip, and then rises by G
/// times the link a round trip, as the link's SACKs add to the window. The
/// queue is what the rate put above the link. The first packet that meets
/// [`QUEUED`] of it is acknowledged a round trip and that queue later, and
/// its sample engages the cap, which binds the association's sends from
/// then on: the queue at that moment is the most the train causes.
fn pace_share(least: Duration) -> u32 {
    let r = least.as_secs_f64();
    if r <= 0.0 {
        return WHOLE;
    }
    let theta = QUEUED.as_secs_f64() / r;
    let bound = (1.0 - PACE_MARGIN) * least.max(QUEUE_FLOOR).as_secs_f64() / r;
    if pace_queue(1.0, theta) <= bound {
        return WHOLE;
    }
    let g = bisect(0.0, 1.0, |g| pace_queue(g, theta) <= bound);
    ((g * f64::from(WHOLE)) as u32).max(1)
}

/// The queue, in round trips, that a slow start paced at a share `g` of the
/// window has built when a sample shows `theta` round trips of it: the
/// derivation at [`pace_share`]. Time counts in round trips from when the
/// rate passed the link.
fn pace_queue(g: f64, theta: f64) -> f64 {
    let x = bisect(0.0, 1.0, |x| x * x.exp() < g);
    let ex = x.exp();
    let queue = |tau: f64| {
        if tau <= 1.0 {
            ((x * tau).exp() - 1.0) / x - tau
        } else {
            let s = tau - 1.0;
            (ex - 1.0) / x - 1.0 + (ex - 1.0) * s + g * s * s / 2.0
        }
    };
    let met = bisect(0.0, 16.0, |tau| queue(tau) < theta);
    queue(met + 1.0 + theta)
}

/// The point in `lo..hi` where `below` turns false, by bisection.
fn bisect(mut lo: f64, mut hi: f64, below: impl Fn(f64) -> bool) -> f64 {
    for _ in 0..60 {
        let mid = (lo + hi) / 2.0;
        if below(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Why a train stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// A round's least acknowledgement delay, or every sample of a run of
    /// them, stood above the least seen: a standing queue at the bottleneck.
    Delay,
    /// Rounds the train held at its in-flight cap delivered no more than the
    /// round before: the path is full.
    Plateau,
    /// A fast retransmission or a T3-rtx expiry.
    Loss,
    /// The byte budget is spent; or the write budget is, and what the train
    /// wrote came back without a judgment.
    Budget,
    /// The train's time is up.
    Deadline,
    /// The first frame is ready: the session needs the path.
    Ready,
    /// Nothing was acknowledged for longer than a delayed acknowledgement and
    /// two round trips.
    Stall,
}

impl Stop {
    /// Whether the path pushed back. Only then is the low edge more than a
    /// floor.
    pub fn is_push_back(self) -> bool {
        matches!(self, Stop::Delay | Stop::Plateau | Stop::Loss)
    }

    /// The reason as a log line names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Stop::Delay => "delay",
            Stop::Plateau => "plateau",
            Stop::Loss => "loss",
            Stop::Budget => "budget",
            Stop::Deadline => "deadline",
            Stop::Ready => "ready",
            Stop::Stall => "stall",
        }
    }
}

/// The unit of a reading's rates (D42).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Bits a second of SCTP payload: user data, without SCTP's headers or
    /// anything beneath them.
    SctpPayloadBps,
}

/// What a train read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// The best whole round's delivery rate.
    pub low_bps: u64,
    /// The rate offered in the round that pushed back, when that round had a
    /// standing queue and delivered less than it was offered; `None` when no
    /// round did, when the reading is only a floor, and where it would be
    /// below the low edge.
    pub high_bps: Option<u64>,
    pub stop: Stop,
    /// The low edge is only a floor: the train ended before the path pushed
    /// back (every stop but `delay`, `plateau` and `loss`).
    pub floor_only: bool,
    pub unit: Unit,
    /// Rounds closed: a round ends when the first byte sent after it began is
    /// acknowledged.
    pub rounds: u32,
    /// Bytes of payload the train wrote. SCTP sends every one of them, after
    /// the stop too.
    pub written_bytes: u64,
    /// The SCTP packets that carry them, once each: what DTLS, UDP, IP and a
    /// relay add beneath is the carrier's.
    pub wire_bytes: u64,
    /// The least acknowledgement delay seen: the round trip with no queue.
    pub min_rtt: Option<Duration>,
    /// The slow start threshold at the first write. 0 on an association its
    /// peer's COOKIE ECHO established: congestion avoidance from the first
    /// byte.
    pub ssthresh_at_start: u32,
    /// Fast retransmissions during the train.
    pub fast_retransmits: u64,
    /// T3-rtx expiries during the train.
    pub t3_expiries: u64,
    /// From the first step to the stop.
    pub duration: Duration,
}

/// What the writer does after a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Write this many bytes now, as messages of [`Train::message_len`]
    /// bytes; 0 writes nothing.
    Write(u64),
    /// The train is over: write nothing more.
    Stopped(Reading),
}

/// How the association's pacer runs while a train does (D43, D64), as
/// [`Train::pace`] answers it after each step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pace {
    /// The share of the congestion window, in thousandths, let out per least
    /// round trip in slow start: [`pace_share`] of the least round trip until
    /// the bottleneck queued, the whole window ([`WHOLE`]) from then on.
    pub share: u32,
    /// The in-flight cap, once the bottleneck queued: new data waits while
    /// what is in flight is at or above it.
    pub cap: Option<u64>,
}

/// The association's pacer, for as long as a train runs: [`Pacing::on`]
/// turns it on, [`Pacing::follow`] steers it to each step's [`Train::pace`]
/// and turns it off at the train's stop, and dropping it turns it off too,
/// so an error that ends the train's writer, or a writer dropped with its
/// train, leaves no pacing behind.
#[derive(Debug)]
#[must_use = "the pacer is off again as soon as this is dropped"]
pub struct Pacing(PaceSwitch);

impl Pacing {
    /// Turns `switch`'s pacer on, at the whole window with no cap.
    pub fn on(switch: PaceSwitch) -> Self {
        switch.set(true);
        Self(switch)
    }

    /// Steers the pacer to `pace`, or turns it off at `None`.
    pub fn follow(&self, pace: Option<Pace>) {
        match pace {
            Some(p) => self.0.steer(p.share, p.cap),
            None => self.0.set(false),
        }
    }
}

impl Drop for Pacing {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// What the first step saw.
#[derive(Debug, Clone, Copy)]
struct Start {
    at: Duration,
    acked: u64,
    fast_retransmits: u64,
    t3_expiries: u64,
    ssthresh: u32,
    /// The congestion window at the first write, in whole messages.
    first_window: u64,
}

/// The round in progress.
#[derive(Debug, Clone, Copy, Default)]
struct Round {
    at: Duration,
    /// Bytes acknowledged and sent (written less pending) when it began.
    acked: u64,
    sent: u64,
    /// It ends when the acknowledged bytes pass this: the first byte sent
    /// after it began.
    mark: u64,
    /// Its least acknowledgement delay, and the samples it took.
    min: Option<Duration>,
    samples: u32,
    /// The cap held the writer while SCTP had room in both windows.
    held: bool,
    /// One of SCTP's windows held back a message the train had written.
    windowed: bool,
    /// The write budget held the writer.
    spent: bool,
}

/// A run of samples in a row, every one standing.
#[derive(Debug, Clone, Copy)]
struct Run {
    /// When its first sample came, and the bytes acknowledged and sent then.
    at: Duration,
    acked: u64,
    sent: u64,
    /// When its newest sample came.
    last: Duration,
    samples: u32,
    min: Duration,
}

/// The open-time train's state.
#[derive(Debug)]
pub struct Train {
    msg: u64,
    start: Option<Start>,
    written: u64,
    /// The previous step's time and acknowledged bytes.
    last_at: Duration,
    last_acked: u64,
    /// When bytes were last acknowledged.
    last_ack_at: Option<Duration>,
    /// The most bytes one step saw acknowledged.
    peak_step: u64,
    /// The least, the first and the greatest acknowledgement delay taken.
    min: Option<Duration>,
    first_sample: Option<Duration>,
    max_sample: Option<Duration>,
    /// The bottleneck queued once: the in-flight cap and the write budget
    /// apply, from the bytes acknowledged then.
    capped: bool,
    capped_acked: u64,
    run: Option<Run>,
    /// Acknowledgement events of more than one message, `(at, acked)`, the
    /// oldest at least [`DISPERSION_SPAN`] old.
    events: VecDeque<(Duration, u64)>,
    /// The best rate over [`DISPERSION_SPAN`] or more of acknowledgements,
    /// bytes a second.
    dispersion: u64,
    round: Round,
    rounds: u32,
    /// The best whole round's delivery, and the last round's, bytes a second.
    best: u64,
    last_round: u64,
    /// Held rounds in a row that did not grow.
    flat: u32,
    /// The offered rate of the round that pushed back, bytes a second.
    high: Option<u64>,
    stopped: Option<Reading>,
    /// The pacer's share of the window in slow start, from the least
    /// acknowledgement delay ([`pace_share`]).
    share: u32,
}

/// Bytes a second, of `bytes` over `d`.
fn rate(bytes: u64, d: Duration) -> u64 {
    let us = d.as_micros().max(1);
    u64::try_from(u128::from(bytes) * 1_000_000 / us).unwrap_or(u64::MAX)
}

/// Bytes, of `bps` (bytes a second) over `d`.
fn bytes_in(bps: u64, d: Duration) -> u64 {
    u64::try_from(u128::from(bps) * d.as_micros() / 1_000_000).unwrap_or(u64::MAX)
}

/// How long `bytes` take at `bps` (bytes a second, above 0).
fn time_of(bytes: u64, bps: u64) -> Duration {
    let us = u128::from(bytes) * 1_000_000 / u128::from(bps.max(1));
    Duration::from_micros(u64::try_from(us).unwrap_or(u64::MAX))
}

impl Train {
    /// A train whose messages each fill one DATA chunk of `max_payload`
    /// bytes (the gauges' `max_payload`).
    pub fn new(max_payload: u32) -> Self {
        Self {
            msg: u64::from(max_payload.max(1)),
            start: None,
            written: 0,
            last_at: Duration::ZERO,
            last_acked: 0,
            last_ack_at: None,
            peak_step: 0,
            min: None,
            first_sample: None,
            max_sample: None,
            capped: false,
            capped_acked: 0,
            run: None,
            events: VecDeque::new(),
            dispersion: 0,
            round: Round::default(),
            rounds: 0,
            best: 0,
            last_round: 0,
            flat: 0,
            high: None,
            stopped: None,
            share: WHOLE,
        }
    }

    /// The length of every message the train asks for.
    pub fn message_len(&self) -> usize {
        usize::try_from(self.msg).unwrap_or(usize::MAX)
    }

    /// How the association's pacer runs after this step: `None` once the
    /// train stopped, whatever stopped it. Until the bottleneck queued it
    /// lets out [`pace_share`] of the window over the least round trip in
    /// slow start; from then on the whole window, with the in-flight cap,
    /// which then binds what the association sends as well as what the
    /// train writes: chunks already pending wait while in flight is at or
    /// above it. A train that queued before anything estimated the rate has
    /// no cap yet.
    pub fn pace(&self) -> Option<Pace> {
        if self.stopped.is_some() {
            return None;
        }
        Some(if self.capped {
            Pace {
                share: WHOLE,
                cap: Some(self.cap()).filter(|&c| c < u64::MAX),
            }
        } else {
            Pace {
                share: self.share,
                cap: None,
            }
        })
    }

    /// One step: `at` is the time on any clock that does not go back,
    /// `delivery` and `gauges` the association's clock and gauges read just
    /// now, and `ready` whether the first frame is ready. The first step
    /// writes the congestion window and one message more; every later one
    /// writes what keeps slow start growing, within the first round's window
    /// and, once the bottleneck queued, the in-flight cap and the write
    /// budget.
    pub fn step(
        &mut self,
        at: Duration,
        delivery: &Delivery,
        gauges: &Gauges,
        ready: bool,
    ) -> Step {
        if let Some(r) = &self.stopped {
            return Step::Stopped(r.clone());
        }
        let msg = self.msg;
        let Some(start) = self.start else {
            // The window and one message more, within the budget.
            let first_window = (u64::from(gauges.cwnd) / msg).clamp(1, BUDGET / msg - 1) * msg;
            self.start = Some(Start {
                at,
                acked: delivery.acked_bytes,
                fast_retransmits: gauges.fast_retransmits,
                t3_expiries: gauges.t3_expiries,
                ssthresh: gauges.ssthresh,
                first_window,
            });
            self.round = Round {
                at,
                mark: first_window,
                ..Round::default()
            };
            self.last_at = at;
            self.written = first_window + msg;
            return Step::Write(self.written);
        };
        let acked = delivery.acked_bytes.saturating_sub(start.acked);
        let pending = gauges.pending_bytes;
        let sent = self.written.saturating_sub(pending);
        let step_acked = acked.saturating_sub(self.last_acked);
        // A SACK of one message is a lone packet's, and may have waited for
        // the receiver's delayed acknowledgement: it is not a sample.
        let mut ran = false;
        if step_acked > msg
            && let Some(s) = gauges.ack_delay_min
        {
            let was_capped = self.capped;
            self.sample(s);
            if self.capped && !was_capped {
                self.capped_acked = acked;
            }
            ran = self.extend_run(s, at, acked, sent);
        }
        if step_acked > 0 {
            self.peak_step = self.peak_step.max(step_acked);
            self.last_acked = acked;
            self.last_ack_at = Some(at);
            self.disperse(at, acked, step_acked);
        }
        if gauges.fast_retransmits > start.fast_retransmits
            || gauges.t3_expiries > start.t3_expiries
        {
            return self.stop(Stop::Loss, at, gauges);
        }
        if step_acked > 0
            && acked > self.round.mark
            && let Some(stop) = self.close_round(at, acked, sent)
        {
            return self.stop(stop, at, gauges);
        }
        if ran && self.run_stands(at) {
            // Every sampled packet of the run met a standing queue: the link
            // was busy all that time, and what it delivered is its rate.
            let (delivered, offered) = self.run_rates(at, acked, sent);
            self.best = self.best.max(delivered);
            if offered > delivered {
                self.high.get_or_insert(offered);
            }
            return self.stop(Stop::Delay, at, gauges);
        }
        if ready {
            return self.stop(Stop::Ready, at, gauges);
        }
        let elapsed = at.saturating_sub(start.at);
        let lead = DEADLINE_LEAD.max(2 * at.saturating_sub(self.last_at));
        if elapsed + lead >= DEADLINE {
            return self.stop(Stop::Deadline, at, gauges);
        }
        if self.written + msg > BUDGET {
            return self.stop(Stop::Budget, at, gauges);
        }
        if let (Some(min), Some(last)) = (self.min, self.last_ack_at)
            && gauges.inflight_bytes > 0
            && at.saturating_sub(last) > 2 * (DELAYED_ACK + min)
        {
            return self.stop(Stop::Stall, at, gauges);
        }
        let out = gauges.inflight_bytes + pending;
        let mut room = u64::MAX;
        if self.rounds == 0 {
            // Until the first round ends: what the first window acknowledges
            // releases as much again, not twice as much.
            room = start.first_window.saturating_sub(out);
        }
        if self.capped {
            room = room.min(self.cap().saturating_sub(out));
        }
        // Enough pending that slow start grows at every SACK until the next
        // step: twice the most one step acknowledged, and a message.
        let want = (2 * msg)
            .max(2 * self.peak_step + msg)
            .saturating_sub(pending);
        // The cap, and not SCTP's windows, held the writer: SCTP sent all it
        // was given, and both windows had room for another message.
        let window_room =
            u64::from(gauges.cwnd) >= gauges.inflight_bytes + msg && u64::from(gauges.rwnd) >= msg;
        if self.capped && self.rounds > 0 && room < want && pending == 0 && window_room {
            self.round.held = true;
        }
        if pending >= msg && !window_room {
            self.round.windowed = true;
        }
        if let Some(budget) = self.write_budget() {
            let left = budget.saturating_sub(self.written);
            if left < want.min(room) {
                self.round.spent = true;
                if left < msg && out <= msg {
                    // Nothing more may be written, and what was written
                    // came back, but a lone packet, without a judgment.
                    return self.stop(Stop::Budget, at, gauges);
                }
            }
            room = room.min(left);
        }
        let n = want.min(room).min(BUDGET - self.written) / msg * msg;
        self.written += n;
        self.last_at = at;
        Step::Write(n)
    }

    /// One acknowledgement delay of a SACK of more than one message.
    fn sample(&mut self, s: Duration) {
        self.first_sample.get_or_insert(s);
        let min = self.min.map_or(s, |m| m.min(s));
        let max = self.max_sample.map_or(s, |m| m.max(s));
        if self.min != Some(min) {
            self.share = pace_share(min);
        }
        self.min = Some(min);
        self.max_sample = Some(max);
        self.round.min = Some(self.round.min.map_or(s, |m| m.min(s)));
        self.round.samples += 1;
        if max.saturating_sub(min) >= QUEUED {
            self.capped = true;
        }
    }

    /// The rate over the newest span of [`DISPERSION_SPAN`] and
    /// [`DISPERSION_ACKS`] SACKs or more that ends at an acknowledgement of
    /// more than one message.
    fn disperse(&mut self, at: Duration, acked: u64, step_acked: u64) {
        if step_acked <= self.msg {
            // A lone packet's SACK neither starts nor ends a span.
            if let Some(last) = self.events.back_mut() {
                last.1 = acked;
            }
            return;
        }
        self.events.push_back((at, acked));
        // A span may start at the event `j` back from the newest.
        let starts = |events: &VecDeque<(Duration, u64)>, j: usize| {
            events.len() - 1 - j >= DISPERSION_ACKS
                && at.saturating_sub(events[j].0) >= DISPERSION_SPAN
        };
        // An event a newer one could start every later span from never
        // starts one again.
        while self.events.len() >= 2 && starts(&self.events, 1) {
            self.events.pop_front();
        }
        if starts(&self.events, 0) {
            let (t, a) = self.events[0];
            self.dispersion = self.dispersion.max(rate(acked - a, at.saturating_sub(t)));
        }
    }

    /// A sample extends the run when it stands and ends it when it does not.
    /// One that comes later after the run's newest than its own queue and a
    /// message's serialisation starts the run again: its packet reached the
    /// bottleneck after the one sampled before it had left, so the
    /// bottleneck may have idled between them. True when it extended or
    /// started the run.
    fn extend_run(&mut self, s: Duration, at: Duration, acked: u64, sent: u64) -> bool {
        let Some(least) = self.min else {
            self.run = None;
            return false;
        };
        let queue = s.saturating_sub(least);
        if queue < self.standing() {
            self.run = None;
            return false;
        }
        let busy = queue + self.serialisation();
        if self.run.is_some_and(|r| at.saturating_sub(r.last) > busy) {
            self.run = None;
        }
        let run = self.run.get_or_insert(Run {
            at,
            acked,
            sent,
            last: at,
            samples: 0,
            min: s,
        });
        run.samples += 1;
        run.min = run.min.min(s);
        run.last = at;
        true
    }

    /// Whether the run holds enough samples over long enough, its least
    /// still standing.
    fn run_stands(&self, at: Duration) -> bool {
        match (self.run, self.min) {
            (Some(r), Some(least)) => {
                r.samples >= ROUND_SAMPLES
                    && at.saturating_sub(r.at) >= (least / 2).max(EARLY_SPAN)
                    && r.min.saturating_sub(least) >= self.standing()
            }
            _ => false,
        }
    }

    /// The delivered and offered rates since the run's first sample, bytes
    /// a second.
    fn run_rates(&self, at: Duration, acked: u64, sent: u64) -> (u64, u64) {
        let Some(r) = self.run else {
            return (0, 0);
        };
        let d = at.saturating_sub(r.at);
        (
            rate(acked.saturating_sub(r.acked), d),
            rate(sent.saturating_sub(r.sent), d),
        )
    }

    /// The most the train may have written once the bottleneck queued, on a
    /// slow link: one whose packet's serialisation shows in the first
    /// sample, so that the estimate had the rate before the queue came.
    /// `None` elsewhere: on a faster link the estimate when the queue first
    /// shows can be a sixth of the rate, and a budget that followed it would
    /// hold the climb.
    fn write_budget(&self) -> Option<u64> {
        (self.capped && self.pair() > 0)
            .then(|| self.capped_acked + bytes_in(self.estimate(), WRITE_SPAN))
    }

    /// The rate the bottleneck serves, as estimated so far, bytes a second;
    /// 0 when nothing says yet.
    fn estimate(&self) -> u64 {
        self.dispersion.max(self.best).max(self.pair())
    }

    /// The packet-pair estimate, bytes a second: 0 on a link too fast for
    /// one packet's serialisation to show in the first sample's lead.
    fn pair(&self) -> u64 {
        match (self.first_sample, self.min) {
            (Some(first), Some(min)) if first.saturating_sub(min) >= PAIR_LEAD => {
                rate(self.msg, first.saturating_sub(min))
            }
            _ => 0,
        }
    }

    /// One message's serialisation at the estimate; zero while nothing
    /// estimates the rate.
    fn serialisation(&self) -> Duration {
        match self.estimate() {
            0 => Duration::ZERO,
            c => time_of(self.msg, c),
        }
    }

    /// A round's least delay above the least seen that is a standing queue.
    fn standing(&self) -> Duration {
        STANDING + self.serialisation()
    }

    /// Whether the round's least delay so far stands above the least seen.
    fn queue_stands(&self) -> bool {
        match (self.round.min, self.min) {
            (Some(m), Some(least)) => m.saturating_sub(least) >= self.standing(),
            _ => false,
        }
    }

    /// The round's delivered and offered rates so far, bytes a second.
    fn round_rates(&self, at: Duration, acked: u64, sent: u64) -> (u64, u64) {
        let d = at.saturating_sub(self.round.at);
        (
            rate(acked.saturating_sub(self.round.acked), d),
            rate(sent.saturating_sub(self.round.sent), d),
        )
    }

    /// The most the train keeps in flight and pending once the bottleneck
    /// queued: the estimated path's pipe and a queue of a third of the round
    /// trip (of 50 ms at least); and never less than the best round's pipe
    /// and a message, nor [`CAP_FLOOR_MESSAGES`]. It is the nearest whole
    /// number of messages to that: a slow link's pipe is a few messages,
    /// and a train that writes in pairs fills it only with about a message
    /// more, which a cap cut down to whole messages can take away.
    fn cap(&self) -> u64 {
        let (c, Some(min)) = (self.estimate(), self.min) else {
            return u64::MAX;
        };
        if c == 0 {
            return u64::MAX;
        }
        let cap = bytes_in(c, min + min.max(QUEUE_FLOOR) / 3)
            .max(bytes_in(self.best, min) + self.msg)
            .max(CAP_FLOOR_MESSAGES * self.msg);
        (cap + self.msg / 2) / self.msg * self.msg
    }

    /// The round ends: its delivery, and whether it pushed back.
    fn close_round(&mut self, at: Duration, acked: u64, sent: u64) -> Option<Stop> {
        let r = self.round;
        let (delivered, offered) = self.round_rates(at, acked, sent);
        self.rounds += 1;
        self.best = self.best.max(delivered);
        let queue = match (r.min, self.min) {
            (Some(m), Some(least)) => Some(m.saturating_sub(least)),
            _ => None,
        };
        let pushed_back = offered > delivered;
        let stop = if r.samples >= ROUND_SAMPLES && self.queue_stands() {
            if pushed_back {
                self.high.get_or_insert(offered);
            }
            Some(Stop::Delay)
        } else if r.held && !r.windowed && !r.spent && delivered * 10 < self.last_round * 11 {
            self.flat += 1;
            if self.flat >= PLATEAU_ROUNDS {
                if pushed_back && queue.is_some_and(|q| q >= STANDING) {
                    self.high.get_or_insert(offered);
                }
                Some(Stop::Plateau)
            } else {
                None
            }
        } else {
            self.flat = 0;
            None
        };
        self.last_round = delivered;
        self.round = Round {
            at,
            acked,
            sent,
            mark: sent,
            ..Round::default()
        };
        stop
    }

    fn stop(&mut self, stop: Stop, at: Duration, gauges: &Gauges) -> Step {
        let start = self.start.unwrap_or(Start {
            at,
            acked: 0,
            fast_retransmits: gauges.fast_retransmits,
            t3_expiries: gauges.t3_expiries,
            ssthresh: gauges.ssthresh,
            first_window: 0,
        });
        let floor_only = !stop.is_push_back();
        let low_bps = self.best * 8;
        let high_bps = self
            .high
            .map(|h| h * 8)
            .filter(|&h| !floor_only && h >= low_bps);
        let reading = Reading {
            low_bps,
            high_bps,
            stop,
            floor_only,
            unit: Unit::SctpPayloadBps,
            rounds: self.rounds,
            written_bytes: self.written,
            wire_bytes: self.written + self.written.div_ceil(self.msg) * PACKET_OVERHEAD,
            min_rtt: self.min,
            ssthresh_at_start: start.ssthresh,
            fast_retransmits: gauges
                .fast_retransmits
                .saturating_sub(start.fast_retransmits),
            t3_expiries: gauges.t3_expiries.saturating_sub(start.t3_expiries),
            duration: at.saturating_sub(start.at),
        };
        self.stopped = Some(reading.clone());
        Step::Stopped(reading)
    }
}

/// The cells: the train over the real vendored SCTP, through the shaped link
/// of `media_delivery`, on both open paths. Real time; one cell at a time.
#[cfg(test)]
mod cells {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use bytes::Bytes;
    use webrtc::sctp::chunk::chunk_payload_data::PayloadProtocolIdentifier;

    use super::*;
    use crate::media_delivery::shaped_link::{self, Draws, Noise, OpenPath};

    /// What the harness's full packet carries: 1,228 bytes of SCTP packet,
    /// 1,200 of them payload. The shaper serialises packet bytes, so a link
    /// of `bps` carries `bps` x 1,200 / 1,228 of payload (D42).
    const PAYLOAD: u64 = 1_200;
    const PACKET: u64 = 1_228;

    /// The writer's step: how often the driver reads the clock and the
    /// gauges.
    const TICK: Duration = Duration::from_millis(1);

    /// The longest the driver waits after a stop for what the train wrote
    /// to be acknowledged.
    const DRAIN: Duration = Duration::from_secs(30);

    /// One cell runs at a time: the cells are timed, and a debug build of
    /// two associations at 50 Mbit/s keeps a core busy.
    static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[derive(Debug, Clone, Copy)]
    struct Cell {
        bps: u64,
        rtt: Duration,
        /// The bottleneck's queue: the longest a packet may wait.
        queue: Duration,
        path: OpenPath,
        /// The viewer's receive window; 0 is the vendored default, 1 MiB,
        /// what a browser's SCTP opens with.
        window: u32,
        /// When the first frame is ready, if it is during the train.
        ready_after: Option<Duration>,
        /// Scheduling noise on the link's deliveries and the writer's steps.
        noise: Option<Noise>,
    }

    impl Cell {
        /// A grid cell: a queue of a second (deeper than anything the train
        /// can fill), and the default window.
        fn grid(bps: u64, rtt_ms: u64, path: OpenPath) -> Self {
            Self {
                bps,
                rtt: Duration::from_millis(rtt_ms),
                queue: Duration::from_secs(1),
                path,
                window: 0,
                ready_after: None,
                noise: None,
            }
        }

        fn name(&self) -> String {
            let path = match self.path {
                OpenPath::InitAck => "I",
                OpenPath::CookieEcho => "C",
            };
            let noise = self.noise.map_or(String::new(), |n| {
                format!(
                    " noise {}:{}/{}/{}",
                    n.seed,
                    n.jitter.as_millis(),
                    n.stall.as_millis(),
                    n.stall_every.as_millis()
                )
            });
            format!(
                "{}/{}/{path}{noise}",
                self.bps as f64 / 1e6,
                self.rtt.as_millis()
            )
        }

        /// The link's payload rate.
        fn payload_bps(&self) -> u64 {
            self.bps * PAYLOAD / PACKET
        }
    }

    #[derive(Debug, Default, Clone, Copy)]
    struct Limits {
        /// Steps at which the peer's window held the sender while the
        /// congestion window had room for another message.
        window: u32,
        /// Steps at which the congestion window held the sender.
        cwnd: u32,
    }

    #[derive(Debug)]
    struct Outcome {
        reading: Reading,
        /// The longest a packet of the agent's waited at the bottleneck, the
        /// drain after the stop included.
        peak_queue: Duration,
        tail_drops: u64,
        /// Whether everything written was acknowledged within [`DRAIN`].
        drained: bool,
        /// What the viewer read, after the drain.
        taken: u64,
        limits: Limits,
        /// A line for each step that saw an acknowledgement, closed a round
        /// or stopped: what the train read and how it judged.
        trace: Vec<String>,
        /// When the cap engaged, and the window, in flight and pending then;
        /// the pacer's share once a sample came, and its last before the cap.
        capped_at: Option<(Duration, u32, u64, u64)>,
        share_first: Option<u32>,
        share_at_cap: Option<u32>,
    }

    /// The trace line of one step, or `None` for a step that saw nothing:
    /// `rounds`, `round` and `last_acked` are the train's before the step.
    #[allow(clippy::too_many_arguments)]
    fn trace_step(
        t: &Train,
        at: Duration,
        acked: u64,
        g: &Gauges,
        rounds: u32,
        round: Round,
        last_acked: u64,
        step: &Step,
    ) -> Option<String> {
        let step_acked = acked.saturating_sub(last_acked);
        let closed = t.rounds != rounds;
        let stopped = matches!(step, Step::Stopped(_));
        if step_acked == 0 && !closed && !stopped {
            return None;
        }
        let ms = |d: Option<Duration>| {
            d.map_or("-".to_owned(), |d| format!("{:.1}", d.as_secs_f64() * 1e3))
        };
        let sample = if step_acked > t.msg {
            g.ack_delay_min
        } else {
            None
        };
        // The round this step judged: on a close, the one before it, with
        // this step's sample.
        let judged = if closed {
            Round {
                min: match (round.min, sample) {
                    (Some(m), Some(s)) => Some(m.min(s)),
                    (m, s) => m.or(s),
                },
                ..round
            }
        } else {
            t.round
        };
        let queue = match (judged.min, t.min) {
            (Some(m), Some(least)) => Some(m.saturating_sub(least)),
            _ => None,
        };
        let pair = t.pair();
        let kbs = |b: u64| b / 1_000;
        let verdict = match step {
            Step::Write(n) => format!("write {n}"),
            Step::Stopped(r) => format!("STOP {}", r.stop.as_str()),
        };
        let run = t.run.map_or("-".to_owned(), |r| {
            format!(
                "{}/{:.0}",
                r.samples,
                at.saturating_sub(r.at).as_secs_f64() * 1e3
            )
        });
        Some(format!(
            "TR t {:7.1} ack {acked:6} +{step_acked:5} infl {:6} pend {:5} cwnd {:6} ad {} | \
             {}round {} rmin {} least {} q {} std {:.1} held {} win {} spent {} run {run} | \
             est {} (best {} disp {} pair {}) kB/s capped {} cap {} budget {} wr {} -> {verdict}",
            at.as_secs_f64() * 1e3,
            g.inflight_bytes,
            g.pending_bytes,
            g.cwnd,
            ms(g.ack_delay_min),
            if closed { "CLOSED " } else { "" },
            if closed { rounds + 1 } else { t.rounds + 1 },
            ms(judged.min),
            ms(t.min),
            ms(queue),
            t.standing().as_secs_f64() * 1e3,
            judged.held,
            judged.windowed,
            judged.spent,
            kbs(t.estimate()),
            kbs(t.best),
            kbs(t.dispersion),
            kbs(pair),
            t.capped,
            if t.capped { t.cap() } else { 0 },
            t.write_budget().unwrap_or(0),
            t.written,
        ))
    }

    /// Windows 11 stops honouring a background process's 1 ms timer request
    /// after a few seconds: on the dev box a probe's 1 ms sleeps took 15.4 ms
    /// from its sixth second on, and a link emulated on such timers is not
    /// the link a cell names (a 20 ms round trip read 45 ms, and the steps
    /// came 15.6 ms apart). Taking the timer resolution out of the power
    /// throttling's hands, as Microsoft documents it, keeps the request
    /// honoured; the call is the process's, so once is enough.
    #[cfg(target_os = "windows")]
    fn keep_timer_resolution() {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
            ProcessPowerThrottling, SetProcessInformation,
        };
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        // SAFETY: `state` is a valid `PROCESS_POWER_THROTTLING_STATE` and the
        // size passed is its own; the pseudo-handle names this process and
        // needs no closing. The call only reads `state`.
        unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&raw const state).cast(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }

    async fn run(cell: &Cell) -> Outcome {
        let _turn = TURN.lock().await;
        #[cfg(target_os = "windows")]
        keep_timer_resolution();
        #[cfg(target_os = "windows")]
        let _timer = crate::win_timer::TimerResolutionGuard::request_1ms();
        let (a, b) = match cell.noise {
            Some(n) => shaped_link::shaped_noisy(cell.bps, cell.rtt / 2, cell.queue, n),
            None => shaped_link::shaped(cell.bps, cell.rtt / 2, cell.queue),
        };
        // The writer's steps meet the noise too, from draws of their own.
        let mut step_draws = cell
            .noise
            .map(|n| Draws::new(n.reseeded(0x2545_f491_4f6c_dd1d)));
        let mut trace = Vec::new();
        let a = Arc::new(a);
        let taken = Arc::new(AtomicU64::new(0));
        let (agent, viewer) = shaped_link::pair_opened(
            a.clone(),
            Arc::new(b),
            cell.window,
            cell.path,
            Arc::clone(&taken),
        )
        .await;
        let stream = agent
            .open_stream(1, PayloadProtocolIdentifier::Binary)
            .await
            .unwrap();
        let clock = agent.delivery_clock();
        let gauges = agent.gauges();
        let acked_before = clock.read().acked_bytes;
        let mut train = Train::new(gauges.take().max_payload);
        // The association paces while the train runs (D43): on now, steered
        // after every step, and off at the stop.
        let pacing = Pacing::on(agent.pacer());
        let message = Bytes::from(vec![0x7e; train.message_len()]);
        let mut limits = Limits::default();
        // The pacer's first share once a sample came and its last before the
        // cap, and the window, in flight and pending at the step the cap
        // engaged.
        let (mut share_first, mut share_at_cap) = (None, None);
        let mut capped_at = None;
        let t0 = Instant::now();
        let reading = loop {
            let at = t0.elapsed();
            let g = gauges.take();
            let d = clock.read();
            let msg = message.len() as u64;
            // With nothing in flight a window of 0 is one the peer has not
            // said yet (path C before its first SACK), not a limit.
            if g.inflight_bytes > 0
                && u64::from(g.rwnd) < msg
                && u64::from(g.cwnd) >= g.inflight_bytes + msg
            {
                limits.window += 1;
            }
            if u64::from(g.cwnd) < g.inflight_bytes + msg {
                limits.cwnd += 1;
            }
            let ready = cell.ready_after.is_some_and(|r| at >= r);
            let (rounds, round, last_acked) = (train.rounds, train.round, train.last_acked);
            let step = train.step(at, &d, &g, ready);
            trace.extend(trace_step(
                &train,
                at,
                d.acked_bytes - acked_before,
                &g,
                rounds,
                round,
                last_acked,
                &step,
            ));
            let pace = train.pace();
            match pace {
                Some(_) if train.capped => {
                    capped_at.get_or_insert((at, g.cwnd, g.inflight_bytes, g.pending_bytes));
                }
                Some(Pace { share, .. }) if train.min.is_some() => {
                    share_first.get_or_insert(share);
                    share_at_cap = Some(share);
                }
                _ => {}
            }
            pacing.follow(pace);
            match step {
                Step::Write(n) => {
                    for _ in 0..n / msg {
                        stream.write(&message).await.unwrap();
                    }
                }
                Step::Stopped(r) => break r,
            }
            let due = Instant::now() + TICK;
            let late = step_draws.as_mut().map_or(Duration::ZERO, |d| d.late(due));
            tokio::time::sleep(TICK + late).await;
        };
        assert!(!agent.pacer().is_on(), "the stop turned the pacer off");
        drop(pacing);
        let stopped = Instant::now();
        let mut drained = false;
        while stopped.elapsed() < DRAIN {
            if clock.read().acked_bytes - acked_before >= reading.written_bytes {
                drained = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let outcome = Outcome {
            peak_queue: a.peak_wait(),
            tail_drops: a.tail_drops(),
            drained,
            taken: taken.load(Ordering::SeqCst),
            limits,
            reading,
            trace,
            capped_at,
            share_first,
            share_at_cap,
        };
        let _ = agent.close().await;
        let _ = viewer.close().await;
        outcome
    }

    /// The properties this cell breaks, by name.
    fn broken(cell: &Cell, o: &Outcome) -> Vec<&'static str> {
        let r = &o.reading;
        let p = cell.payload_bps();
        let mut out = Vec::new();
        if r.stop.is_push_back() && r.low_bps.abs_diff(p) > p / 5 {
            out.push("P-a");
        }
        if matches!(r.stop, Stop::Budget | Stop::Deadline | Stop::Ready)
            && (!r.floor_only || r.low_bps > p + p / 5)
        {
            out.push("P-b");
        }
        if r.high_bps.is_some_and(|h| h < r.low_bps) {
            out.push("P-c");
        }
        // P-d: no packet of the agent's waited 10 ms at the bottleneck, so
        // no round's least queueing delay reached it.
        if r.stop == Stop::Delay && o.peak_queue < Duration::from_millis(10) {
            out.push("P-d");
        }
        if o.peak_queue > cell.rtt.max(Duration::from_millis(50)) {
            out.push("P-e: queue");
        }
        if r.written_bytes > BUDGET {
            out.push("P-e: bytes");
        }
        if r.duration > DEADLINE {
            out.push("P-e: time");
        }
        if !o.drained {
            out.push("P-e: not drained");
        }
        if cell.bps == 400_000 && r.written_bytes > 64_000 {
            out.push("P-f");
        }
        if r.floor_only == r.stop.is_push_back() {
            out.push("floor-only is not the stop's");
        }
        match cell.path {
            OpenPath::CookieEcho if r.ssthresh_at_start != 0 => {
                out.push("path C: ssthresh at the first write");
            }
            OpenPath::InitAck if r.ssthresh_at_start == 0 => {
                out.push("path I: ssthresh at the first write");
            }
            _ => {}
        }
        out
    }

    fn describe(cell: &Cell, o: &Outcome) -> String {
        let r = &o.reading;
        let mbps = |b: u64| b as f64 / 1e6;
        let cap = o
            .capped_at
            .map_or("never".to_owned(), |(at, cwnd, infl, pend)| {
                format!(
                    "at {} ms cwnd {cwnd} infl {infl} pend {pend}",
                    at.as_millis()
                )
            });
        let share = |s: Option<u32>| s.map_or("-".to_owned(), |s| s.to_string());
        format!(
            "train cell {} window {}: {} low {:.3} ({:.2}) high {} floor_only {} rounds {} \
             written {} wire {} min_rtt {:?} ss0 {} fr {} t3 {} t {} ms; \
             peak queue {:.1} ms, tail drops {}, drained {}, taken {}, limited by window {} cwnd {}; \
             capped {cap}; share {} then {}",
            cell.name(),
            if cell.window == 0 {
                1 << 20
            } else {
                cell.window
            },
            r.stop.as_str(),
            mbps(r.low_bps),
            r.low_bps as f64 / cell.payload_bps() as f64,
            r.high_bps
                .map_or("-".to_owned(), |h| format!("{:.3}", mbps(h))),
            r.floor_only,
            r.rounds,
            r.written_bytes,
            r.wire_bytes,
            r.min_rtt,
            r.ssthresh_at_start,
            r.fast_retransmits,
            r.t3_expiries,
            r.duration.as_millis(),
            o.peak_queue.as_secs_f64() * 1e3,
            o.tail_drops,
            o.drained,
            o.taken,
            o.limits.window,
            o.limits.cwnd,
            share(o.share_first),
            share(o.share_at_cap),
        )
    }

    /// Runs `cell`, prints what it read, and fails on every property it
    /// breaks and every one `extra` adds.
    async fn assert_cell(cell: Cell, extra: fn(&Cell, &Outcome) -> Vec<&'static str>) {
        let o = run(&cell).await;
        let line = describe(&cell, &o);
        let mut b = broken(&cell, &o);
        b.extend(extra(&cell, &o));
        if !b.is_empty() {
            print_trace(&cell, &o);
        }
        println!("{line}");
        assert!(b.is_empty(), "{line}: breaks {b:?}");
    }

    /// What the train read and judged at each step that saw something: the
    /// first thing to read when a cell breaks.
    fn print_trace(cell: &Cell, o: &Outcome) {
        for t in &o.trace {
            println!("{} {t}", cell.name());
        }
    }

    fn none(_: &Cell, _: &Outcome) -> Vec<&'static str> {
        Vec::new()
    }

    macro_rules! grid {
        ($($name:ident: $bps:expr, $rtt:expr, $path:expr;)*) => {$(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): see docs/rate-control.md"]
            async fn $name() {
                assert_cell(Cell::grid($bps, $rtt, $path), none).await;
            }
        )*};
    }

    grid! {
        path_i_0_4_mbps_20_ms: 400_000, 20, OpenPath::InitAck;
        path_i_0_4_mbps_80_ms: 400_000, 80, OpenPath::InitAck;
        path_i_0_4_mbps_250_ms: 400_000, 250, OpenPath::InitAck;
        path_i_2_mbps_20_ms: 2_000_000, 20, OpenPath::InitAck;
        path_i_2_mbps_80_ms: 2_000_000, 80, OpenPath::InitAck;
        path_i_2_mbps_250_ms: 2_000_000, 250, OpenPath::InitAck;
        path_i_8_mbps_20_ms: 8_000_000, 20, OpenPath::InitAck;
        path_i_8_mbps_80_ms: 8_000_000, 80, OpenPath::InitAck;
        path_i_8_mbps_250_ms: 8_000_000, 250, OpenPath::InitAck;
        path_i_50_mbps_20_ms: 50_000_000, 20, OpenPath::InitAck;
        path_i_50_mbps_80_ms: 50_000_000, 80, OpenPath::InitAck;
        path_i_50_mbps_250_ms: 50_000_000, 250, OpenPath::InitAck;
        path_c_0_4_mbps_20_ms: 400_000, 20, OpenPath::CookieEcho;
        path_c_0_4_mbps_80_ms: 400_000, 80, OpenPath::CookieEcho;
        path_c_0_4_mbps_250_ms: 400_000, 250, OpenPath::CookieEcho;
        path_c_2_mbps_20_ms: 2_000_000, 20, OpenPath::CookieEcho;
        path_c_2_mbps_80_ms: 2_000_000, 80, OpenPath::CookieEcho;
        path_c_2_mbps_250_ms: 2_000_000, 250, OpenPath::CookieEcho;
        path_c_8_mbps_20_ms: 8_000_000, 20, OpenPath::CookieEcho;
        path_c_8_mbps_80_ms: 8_000_000, 80, OpenPath::CookieEcho;
        path_c_8_mbps_250_ms: 8_000_000, 250, OpenPath::CookieEcho;
        path_c_50_mbps_20_ms: 50_000_000, 20, OpenPath::CookieEcho;
        path_c_50_mbps_80_ms: 50_000_000, 80, OpenPath::CookieEcho;
        path_c_50_mbps_250_ms: 50_000_000, 250, OpenPath::CookieEcho;
    }

    /// The scheduling noise of a busy test runner, from draws of `seed`:
    /// every packet up to 2 ms late each way and every step of the writer as
    /// late, and about every half second each of the three loses its core
    /// for up to 20 ms. CI run 44383834538 broke P-f in the quiet cell at
    /// 0.4 Mbit/s and 250 ms on path I, on its Linux and its Windows runner.
    /// Under the train's earlier rule 2 ms of jitter alone wrote the Windows
    /// run's 64,800 bytes again, and with 5 ms and a stall of up to 30 ms on
    /// one packet in 20 (about every half second at that rate) it wrote
    /// 66,000 to 88,800 bytes, the last in six rounds as on Linux.
    fn runner_noise(seed: u64) -> Noise {
        Noise {
            seed,
            jitter: Duration::from_millis(2),
            stall: Duration::from_millis(20),
            stall_every: Duration::from_millis(500),
        }
    }

    macro_rules! noisy {
        ($($name:ident: $bps:expr, $path:expr, $seed:expr;)*) => {$(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): see docs/rate-control.md"]
            async fn $name() {
                let cell = Cell {
                    noise: Some(runner_noise($seed)),
                    ..Cell::grid($bps, 250, $path)
                };
                assert_cell(cell, none).await;
            }
        )*};
    }

    // The slow links at 250 ms, where a judgment that comes late writes the
    // most, under a busy runner's noise. A measurement, like the grid (D44).
    noisy! {
        path_i_0_4_mbps_250_ms_under_noise: 400_000, OpenPath::InitAck, 1;
        path_c_0_4_mbps_250_ms_under_noise: 400_000, OpenPath::CookieEcho, 2;
        path_i_2_mbps_250_ms_under_noise: 2_000_000, OpenPath::InitAck, 3;
        path_c_2_mbps_250_ms_under_noise: 2_000_000, OpenPath::CookieEcho, 4;
    }

    /// The whole grid under a busy runner's noise, five seeds a cell: every
    /// property of every run, and a line for each. Run with `--ignored
    /// --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "a measurement for the docs, about four minutes"]
    async fn grid_under_noise() {
        let mut broke = Vec::new();
        for seed in 1..=5 {
            for bps in [400_000, 2_000_000, 8_000_000, 50_000_000] {
                for rtt_ms in [20, 80, 250] {
                    for path in [OpenPath::InitAck, OpenPath::CookieEcho] {
                        let cell = Cell {
                            noise: Some(runner_noise(seed)),
                            ..Cell::grid(bps, rtt_ms, path)
                        };
                        let o = run(&cell).await;
                        let b = broken(&cell, &o);
                        if !b.is_empty() {
                            print_trace(&cell, &o);
                            broke.push(format!("{}: {b:?}", cell.name()));
                        }
                        println!("{}", describe(&cell, &o));
                    }
                }
            }
        }
        assert!(broke.is_empty(), "{broke:?}");
    }

    /// The shallow buffer: 8 Mbit/s, 80 ms, a bottleneck queue of 20 ms.
    fn shallow(path: OpenPath) -> Cell {
        Cell {
            queue: Duration::from_millis(20),
            ..Cell::grid(8_000_000, 80, path)
        }
    }

    /// A shallow buffer's loss is push-back: the train stops on `loss` and
    /// never reports a high edge above twice its low edge.
    fn stops_on_loss(_: &Cell, o: &Outcome) -> Vec<&'static str> {
        let r = &o.reading;
        let mut out = Vec::new();
        if r.stop != Stop::Loss {
            out.push("shallow: not a loss stop");
        }
        if r.high_bps.is_some_and(|h| h > 2 * r.low_bps) {
            out.push("shallow: high above twice low");
        }
        out
    }

    /// D43's cell, green since the association paces while the train runs.
    /// Unpaced, a SACK of two packets released four back to back, so in a
    /// round the queue grew by the window of the round before; a 20 ms queue,
    /// a quarter of the pipe, overflowed once that passed a quarter, and no
    /// round closed before the loss carried much more than half the link's
    /// rate (0.14 to 0.52 of it over twelve runs). Paced, a flight leaves
    /// spread over its round trip, the queue builds only once a round carries
    /// more than the link, and the round before the loss carries the link:
    /// 0.84 to 1.02 of it, five runs of five, alone. It stays a measurement
    /// (D44): it ends on a push-back whose low edge a busy runner can move
    /// (docs/rate-control.md, "The open-time train").
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): see docs/rate-control.md"]
    async fn path_i_a_shallow_buffer_stops_on_loss() {
        assert_cell(shallow(OpenPath::InitAck), stops_on_loss).await;
    }

    /// On path C congestion avoidance from the first byte cannot fill 8
    /// Mbit/s at 80 ms within the train's time, so nothing is lost: the
    /// reading is a floor (P-b), and a loss, should one come, is judged as on
    /// path I.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn path_c_a_shallow_buffer_reads_a_floor_or_stops_on_loss() {
        assert_cell(shallow(OpenPath::CookieEcho), |cell, o| {
            if o.reading.stop == Stop::Loss {
                stops_on_loss(cell, o)
            } else if o.reading.floor_only {
                Vec::new()
            } else {
                vec!["shallow: push-back that is not a loss"]
            }
        })
        .await;
    }

    /// The viewer's window of 64 KiB on 50 Mbit/s and 80 ms holds the sender
    /// far below the link: a floor, with no high edge, and the gauges show
    /// the window, not the congestion window, held it.
    fn peer_window(path: OpenPath) -> Cell {
        Cell {
            window: 64 * 1024,
            ..Cell::grid(50_000_000, 80, path)
        }
    }

    fn window_is_a_floor(_: &Cell, o: &Outcome) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !o.reading.floor_only || o.reading.high_bps.is_some() {
            out.push("window: read as push-back");
        }
        if o.limits.window == 0 {
            out.push("window: the sender was never window-limited");
        }
        out
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn path_i_a_peer_window_limit_is_a_floor() {
        assert_cell(peer_window(OpenPath::InitAck), window_is_a_floor).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn path_c_a_peer_window_limit_is_a_floor() {
        assert_cell(peer_window(OpenPath::CookieEcho), window_is_a_floor).await;
    }

    /// P-d: 50 Mbit/s at 250 ms holds 1.5 MB, three times what the train
    /// may write, so no queue can stand; the queue slow start's doubling
    /// builds inside each round is not push-back. It ends on `budget` or
    /// `deadline`, never on `delay`.
    fn room_is_not_push_back(_: &Cell, o: &Outcome) -> Vec<&'static str> {
        match o.reading.stop {
            Stop::Budget | Stop::Deadline => Vec::new(),
            _ => vec!["P-d: a link with room did not end on budget or deadline"],
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn path_i_a_link_with_room_ends_on_budget_or_deadline() {
        assert_cell(
            Cell::grid(50_000_000, 250, OpenPath::InitAck),
            room_is_not_push_back,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn path_c_a_link_with_room_ends_on_budget_or_deadline() {
        assert_cell(
            Cell::grid(50_000_000, 250, OpenPath::CookieEcho),
            room_is_not_push_back,
        )
        .await;
    }

    /// P-g: the first frame is ready 0.3 s in, while the train still climbs
    /// at 8 Mbit/s and 80 or 250 ms: the reading is a floor, whatever it is.
    fn ready_early(rtt_ms: u64, path: OpenPath) -> Cell {
        Cell {
            ready_after: Some(Duration::from_millis(300)),
            ..Cell::grid(8_000_000, rtt_ms, path)
        }
    }

    fn ready_is_a_floor(_: &Cell, o: &Outcome) -> Vec<&'static str> {
        let r = &o.reading;
        let mut out = Vec::new();
        if r.stop != Stop::Ready || !r.floor_only {
            out.push("P-g: not a ready floor");
        }
        if r.duration > Duration::from_millis(300) + 10 * TICK {
            out.push("P-g: ran past the window");
        }
        out
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn path_i_a_window_of_0_3_s_at_80_ms_reads_a_floor() {
        assert_cell(ready_early(80, OpenPath::InitAck), ready_is_a_floor).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn path_i_a_window_of_0_3_s_at_250_ms_reads_a_floor() {
        assert_cell(ready_early(250, OpenPath::InitAck), ready_is_a_floor).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn path_c_a_window_of_0_3_s_at_80_ms_reads_a_floor() {
        assert_cell(ready_early(80, OpenPath::CookieEcho), ready_is_a_floor).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn path_c_a_window_of_0_3_s_at_250_ms_reads_a_floor() {
        assert_cell(ready_early(250, OpenPath::CookieEcho), ready_is_a_floor).await;
    }

    /// Path C's ramp, measured and not asserted: how long an association
    /// keeps the link short of its rate when a writer keeps it busy, opened
    /// by the INIT ACK (slow start) and by the peer's COOKIE ECHO (congestion
    /// avoidance from the first byte, one packet more a round trip). Run with
    /// `--ignored --nocapture`; each line gives the first time the last round
    /// trip (200 ms at least) carried 80 % of the link's payload rate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "a measurement for the docs, about a minute"]
    async fn path_c_ramp_to_the_link_rate() {
        for (bps, rtt_ms) in [
            (2_000_000, 80),
            (8_000_000, 80),
            (2_000_000, 250),
            (8_000_000, 250),
        ] {
            for path in [OpenPath::InitAck, OpenPath::CookieEcho] {
                let cell = Cell::grid(bps, rtt_ms, path);
                let (reach, rate_bps, cwnd) = ramp(&cell, Duration::from_secs(15)).await;
                println!(
                    "ramp {}: 80 % after {}; last round trip {:.3} Mbit/s, cwnd {cwnd}",
                    cell.name(),
                    reach.map_or("more than 15 s".to_owned(), |r| format!(
                        "{} ms",
                        r.as_millis()
                    )),
                    rate_bps as f64 / 1e6,
                );
            }
        }
    }

    /// Writes as fast as SCTP takes it for up to `limit`: the time the link
    /// first carried 80 % of its rate over a round trip, the rate over the
    /// last one, and the congestion window then.
    async fn ramp(cell: &Cell, limit: Duration) -> (Option<Duration>, u64, u32) {
        let _turn = TURN.lock().await;
        #[cfg(target_os = "windows")]
        keep_timer_resolution();
        #[cfg(target_os = "windows")]
        let _timer = crate::win_timer::TimerResolutionGuard::request_1ms();
        let (a, b) = shaped_link::shaped(cell.bps, cell.rtt / 2, cell.queue);
        let taken = Arc::new(AtomicU64::new(0));
        let (agent, viewer) =
            shaped_link::pair_opened(Arc::new(a), Arc::new(b), cell.window, cell.path, taken).await;
        let stream = agent
            .open_stream(1, PayloadProtocolIdentifier::Binary)
            .await
            .unwrap();
        let clock = agent.delivery_clock();
        let gauges = agent.gauges();
        let message = Bytes::from(vec![0x7e; PAYLOAD as usize]);
        let base = clock.read().acked_bytes;
        let span = cell.rtt.max(Duration::from_millis(200));
        let target = cell.payload_bps() / 8 * 4 / 5;
        let mut seen: VecDeque<(Duration, u64)> = VecDeque::new();
        let (mut reach, mut last) = (None, 0);
        let t0 = Instant::now();
        while t0.elapsed() < limit {
            let pending = gauges.take().pending_bytes;
            for _ in 0..(16 * PAYLOAD).saturating_sub(pending) / PAYLOAD {
                stream.write(&message).await.unwrap();
            }
            let at = t0.elapsed();
            let acked = clock.read().acked_bytes - base;
            seen.push_back((at, acked));
            while seen.len() > 2 && at.saturating_sub(seen[1].0) >= span {
                seen.pop_front();
            }
            let (t, a) = seen[0];
            if at.saturating_sub(t) >= span {
                last = rate(acked - a, at.saturating_sub(t));
                if reach.is_none() && last >= target {
                    reach = Some(at);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let cwnd = gauges.take().cwnd;
        let _ = agent.close().await;
        let _ = viewer.close().await;
        (reach, last * 8, cwnd)
    }
}

/// The machine alone, on readings made up by hand.
#[cfg(test)]
mod machine {
    use super::*;

    const MSG: u64 = 1_200;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// The gauges of a path I association with `inflight` and `pending`
    /// bytes, its windows wide open.
    fn gauges(inflight: u64, pending: u64) -> Gauges {
        Gauges {
            max_payload: 1_200,
            cwnd: 4_380,
            ssthresh: 1 << 20,
            rwnd: 1 << 20,
            inflight_bytes: inflight,
            pending_bytes: pending,
            ..Gauges::default()
        }
    }

    fn acked(bytes: u64) -> Delivery {
        Delivery {
            acked_bytes: bytes,
            ..Delivery::default()
        }
    }

    fn reading(step: Step) -> Reading {
        match step {
            Step::Stopped(r) => r,
            Step::Write(n) => panic!("still writing ({n} bytes)"),
        }
    }

    #[test]
    fn the_first_step_writes_the_congestion_window_and_one_message() {
        let mut t = Train::new(1_200);
        assert_eq!(t.message_len(), 1_200);
        assert_eq!(
            t.step(ms(0), &acked(0), &gauges(0, 0), false),
            Step::Write(4 * MSG)
        );
    }

    #[test]
    fn the_first_write_keeps_within_the_budget() {
        let mut t = Train::new(1_200);
        let wide = Gauges {
            cwnd: 4 << 20,
            ..gauges(0, 0)
        };
        let Step::Write(n) = t.step(ms(0), &acked(0), &wide, false) else {
            unreachable!()
        };
        assert!(n <= BUDGET && n + MSG > BUDGET, "{n}");
    }

    #[test]
    fn until_the_first_round_ends_it_keeps_to_the_first_window() {
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &gauges(0, 0), false);
        // Three in flight and one pending fill the first window and more.
        assert_eq!(
            t.step(ms(1), &acked(0), &gauges(3 * MSG, MSG), false),
            Step::Write(0)
        );
        // A SACK of two: the pending one went out, one more fits.
        let g = Gauges {
            ack_delay_min: Some(ms(40)),
            ..gauges(2 * MSG, 0)
        };
        assert_eq!(t.step(ms(40), &acked(2 * MSG), &g, false), Step::Write(MSG));
    }

    #[test]
    fn a_stopped_train_answers_with_the_same_reading() {
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &gauges(0, 0), false);
        let first = reading(t.step(ms(5), &acked(0), &gauges(3 * MSG, MSG), true));
        assert_eq!(first.stop, Stop::Ready);
        let again = reading(t.step(ms(900), &acked(4 * MSG), &gauges(0, 0), false));
        assert_eq!(first, again);
    }

    #[test]
    fn a_ready_frame_ends_it_with_a_floor() {
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &gauges(0, 0), false);
        let r = reading(t.step(ms(300), &acked(0), &gauges(3 * MSG, MSG), true));
        assert_eq!(
            (r.stop, r.floor_only, r.high_bps),
            (Stop::Ready, true, None)
        );
        assert_eq!(r.duration, ms(300));
        assert_eq!(r.written_bytes, 4 * MSG);
        assert_eq!(r.wire_bytes, 4 * (MSG + PACKET_OVERHEAD));
        assert_eq!(r.unit, Unit::SctpPayloadBps);
    }

    #[test]
    fn only_the_trains_own_losses_count() {
        let mut t = Train::new(1_200);
        let before = Gauges {
            fast_retransmits: 5,
            t3_expiries: 2,
            ..gauges(0, 0)
        };
        t.step(ms(0), &acked(0), &before, false);
        assert!(matches!(
            t.step(ms(1), &acked(0), &before, false),
            Step::Write(_)
        ));
        let lost = Gauges {
            fast_retransmits: 6,
            ..before
        };
        let r = reading(t.step(ms(2), &acked(0), &lost, false));
        assert_eq!((r.stop, r.floor_only), (Stop::Loss, false));
        assert_eq!((r.fast_retransmits, r.t3_expiries), (1, 0));
    }

    #[test]
    fn the_deadline_ends_it_before_the_trains_time_is_up() {
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &gauges(0, 0), false);
        let mut at = ms(0);
        let r = loop {
            at += ms(10);
            if let Step::Stopped(r) = t.step(at, &acked(0), &gauges(3 * MSG, MSG), false) {
                break r;
            }
        };
        assert_eq!((r.stop, r.floor_only), (Stop::Deadline, true));
        assert!(r.duration < DEADLINE, "{:?}", r.duration);
        assert!(r.duration + DEADLINE_LEAD >= DEADLINE, "{:?}", r.duration);
    }

    /// A link that acknowledges everything as soon as it is written, with no
    /// queue: nothing pushes back, so the budget ends the train.
    #[test]
    fn a_link_that_never_pushes_back_ends_on_the_budget() {
        let mut t = Train::new(1_200);
        let mut written = 0;
        let mut at = ms(0);
        let g = Gauges {
            ack_delay_min: Some(ms(20)),
            ..gauges(0, 0)
        };
        let r = loop {
            match t.step(at, &acked(written), &g, false) {
                Step::Write(n) => written += n,
                Step::Stopped(r) => break r,
            }
            at += ms(1);
        };
        assert_eq!(
            (r.stop, r.floor_only, r.high_bps),
            (Stop::Budget, true, None)
        );
        assert!(
            r.written_bytes <= BUDGET && r.written_bytes + MSG > BUDGET,
            "{}",
            r.written_bytes
        );
    }

    /// A round whose least delay stands 40 ms above the least seen stops the
    /// train on `delay`, and the low edge is the best whole round's rate.
    #[test]
    fn a_round_that_queued_throughout_stops_on_delay() {
        let mut t = Train::new(1_200);
        let Step::Write(mut sent) = t.step(ms(0), &acked(0), &gauges(0, 0), false) else {
            unreachable!()
        };
        // Round 1 ends at 100 ms, every sample at the least, 50 ms; the
        // SACK at 75 ms is a lone packet's and no sample.
        let g = |delay: u64| Gauges {
            cwnd: 1 << 20,
            ack_delay_min: Some(ms(delay)),
            ..gauges(0, 0)
        };
        for (at, acked_bytes) in [(50, 2 * MSG), (75, 3 * MSG), (100, 4 * MSG)] {
            if let Step::Write(n) = t.step(ms(at), &acked(acked_bytes), &g(50), false) {
                sent += n;
            }
        }
        assert_eq!(t.rounds, 1);
        assert_eq!(t.best, 4 * MSG * 10);
        // Round 2 ends when the first byte sent after it began is
        // acknowledged, at 200 ms; its four samples sat 40 ms above the
        // least.
        for (at, acked_bytes) in [(125, 6 * MSG), (150, 8 * MSG), (175, 10 * MSG)] {
            if let Step::Write(n) = t.step(ms(at), &acked(acked_bytes), &g(90), false) {
                sent += n;
            }
        }
        let r = reading(t.step(ms(200), &acked(sent), &g(90), false));
        assert_eq!((r.stop, r.floor_only, r.rounds), (Stop::Delay, false, 2));
        assert_eq!(r.min_rtt, Some(ms(50)));
        assert_eq!(r.low_bps, t.best * 8);
        assert!(r.low_bps >= 4 * MSG * 10 * 8);
    }

    /// A round of fewer than four samples is not judged on delay, however
    /// far above the least its least sits: one SACK the scheduler held
    /// reads as a queue.
    #[test]
    fn a_round_of_fewer_than_four_samples_is_no_delay_stop() {
        let g = gauges(0, 0);
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &g, false);
        t.min = Some(ms(50));
        t.best = 10_000_000;
        t.round.min = Some(ms(90));
        t.round.samples = ROUND_SAMPLES - 1;
        assert_eq!(t.close_round(ms(100), 4 * MSG, 4 * MSG), None);
        t.round.min = Some(ms(90));
        t.round.samples = ROUND_SAMPLES;
        assert_eq!(t.close_round(ms(200), 8 * MSG, 8 * MSG), Some(Stop::Delay));
    }

    /// P-d at its edge: a round whose least delay sits under 10 ms above the
    /// least seen is no standing queue; 10 ms and one message's
    /// serialisation at the estimate is.
    #[test]
    fn a_round_under_ten_ms_above_the_least_is_no_delay_stop() {
        let g = gauges(0, 0);
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &g, false);
        t.min = Some(ms(50));
        // 80 Mbit/s: one message's serialisation is 0.12 ms.
        t.best = 10_000_000;
        t.round.min = Some(ms(50) + Duration::from_micros(9_900));
        t.round.samples = ROUND_SAMPLES;
        assert_eq!(t.close_round(ms(100), 4 * MSG, 4 * MSG), None);
        t.round.min = Some(ms(50) + Duration::from_micros(10_200));
        t.round.samples = ROUND_SAMPLES;
        assert_eq!(t.close_round(ms(200), 8 * MSG, 8 * MSG), Some(Stop::Delay));
    }

    #[test]
    fn a_high_edge_below_the_low_edge_or_on_a_floor_is_never_reported() {
        let g = gauges(0, 0);
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &g, false);
        t.best = 1_000;
        t.high = Some(900);
        assert_eq!(reading(t.stop(Stop::Delay, ms(10), &g)).high_bps, None);
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &g, false);
        t.best = 1_000;
        t.high = Some(2_000);
        assert_eq!(reading(t.stop(Stop::Budget, ms(10), &g)).high_bps, None);
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &g, false);
        t.best = 1_000;
        t.high = Some(2_000);
        assert_eq!(
            reading(t.stop(Stop::Delay, ms(10), &g)).high_bps,
            Some(16_000)
        );
    }

    /// A train that queued the bottleneck, its estimate 24 kB/s over a
    /// least round trip of 100 ms: a queue stands at 10 ms and one
    /// message's 50 ms. The cap holds the writer in every step below.
    fn queued() -> Train {
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &gauges(0, 0), false);
        t.capped = true;
        t.min = Some(ms(100));
        t.best = 24_000;
        t.rounds = 1;
        t.round.mark = u64::MAX;
        t.written = 20 * MSG;
        t
    }

    /// One SACK of two messages, through byte `bytes`, with `delay`.
    fn sack(t: &mut Train, at: u64, bytes: u64, delay: u64) -> Step {
        let g = Gauges {
            ack_delay_min: Some(ms(delay)),
            ..gauges(8 * MSG, 0)
        };
        t.step(ms(at), &acked(bytes), &g, false)
    }

    /// Samples in a row, every one standing, stop the train on delay before
    /// its round ends, at the delivery since the run's first sample.
    #[test]
    fn a_run_of_samples_that_stand_stops_on_delay() {
        let mut t = queued();
        assert_eq!(sack(&mut t, 200, 2 * MSG, 100), Step::Write(0));
        for (at, bytes) in [(250, 4 * MSG), (300, 6 * MSG), (350, 8 * MSG)] {
            assert_eq!(sack(&mut t, at, bytes, 170), Step::Write(0));
        }
        let r = reading(sack(&mut t, 400, 10 * MSG, 170));
        assert_eq!((r.stop, r.floor_only, r.rounds), (Stop::Delay, false, 1));
        // 7,200 bytes in the 150 ms since the run's first sample.
        assert_eq!(r.low_bps, 48_000 * 8);
    }

    /// A sample that does not stand ends the run, however long it ran: the
    /// link idled before that packet. The next run starts from the sample
    /// after it.
    #[test]
    fn a_sample_that_does_not_stand_ends_the_run() {
        let mut t = queued();
        for (at, bytes, delay) in [
            (250, 4 * MSG, 170),
            (300, 6 * MSG, 170),
            (350, 8 * MSG, 170),
            (400, 10 * MSG, 120),
            (450, 12 * MSG, 170),
            (500, 14 * MSG, 170),
            (550, 16 * MSG, 170),
        ] {
            assert_eq!(sack(&mut t, at, bytes, delay), Step::Write(0), "{at}");
        }
        assert!(t.run.is_some_and(|r| r.samples == 3));
    }

    /// A sample that comes after a silence longer than its own queue and a
    /// message's serialisation starts the run again: the bottleneck may have
    /// emptied in that silence, and a burst sent after it queues behind
    /// itself.
    #[test]
    fn a_silence_longer_than_the_next_queue_starts_the_run_again() {
        let mut t = queued();
        for (at, bytes) in [(250, 4 * MSG), (300, 6 * MSG), (350, 8 * MSG)] {
            assert_eq!(sack(&mut t, at, bytes, 170), Step::Write(0));
        }
        assert!(t.run.is_some_and(|r| r.samples == 3));
        // 121 ms of silence, then a sample 70 ms above the least: more than
        // its queue and one message's 50 ms.
        assert_eq!(sack(&mut t, 471, 10 * MSG, 170), Step::Write(0));
        assert!(t.run.is_some_and(|r| r.samples == 1 && r.at == ms(471)));
    }

    /// Once the bottleneck queued, the train writes no more than what had
    /// been acknowledged then and what its estimate carries in
    /// [`WRITE_SPAN`]; then it writes nothing until what it wrote is back,
    /// and ends on a floor.
    #[test]
    fn a_spent_write_budget_holds_the_writer_and_ends_on_a_floor() {
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &gauges(0, 0), false);
        // A slow link, its first sample 60 ms above the least: the pair
        // reads 20 kB/s. Queued with 6,000 bytes acknowledged, the train may
        // write 6,000 and 14,000 bytes.
        t.capped = true;
        t.capped_acked = 6_000;
        t.min = Some(ms(100));
        t.first_sample = Some(ms(160));
        t.rounds = 1;
        t.round.mark = u64::MAX;
        t.written = 14 * MSG;
        assert_eq!(t.write_budget(), Some(20_000));
        // All of it acknowledged: the cap allows three messages, the budget
        // two.
        assert_eq!(
            t.step(ms(10), &acked(14 * MSG), &gauges(0, 0), false),
            Step::Write(2 * MSG)
        );
        // Two in flight: nothing more may be written, and the train waits.
        assert_eq!(
            t.step(ms(20), &acked(14 * MSG), &gauges(2 * MSG, 0), false),
            Step::Write(0)
        );
        // All but a lone packet back: a floor.
        let r = reading(t.step(ms(30), &acked(15 * MSG), &gauges(MSG, 0), false));
        assert_eq!(
            (r.stop, r.floor_only, r.high_bps),
            (Stop::Budget, true, None)
        );
        assert_eq!(r.written_bytes, 16 * MSG);
        assert!(r.written_bytes <= 20_000);
    }

    /// A link too fast for a packet's serialisation to show in the first
    /// sample has no write budget: the estimate when the queue first shows
    /// lags the rate.
    #[test]
    fn a_fast_link_has_no_write_budget() {
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &gauges(0, 0), false);
        t.capped = true;
        t.min = Some(ms(100));
        t.first_sample = Some(ms(105));
        t.best = 20_000;
        assert_eq!(t.write_budget(), None);
    }

    /// The pacer's share of the window is the largest whose queue, by the
    /// derivation, stays within three quarters of P-e's bound, max(the least
    /// round trip, 50 ms): a hundredth of the window more breaks it. From
    /// about 235 ms on, the whole window's queue fits.
    #[test]
    fn the_pace_share_is_the_largest_whose_queue_keeps_within_three_quarters_of_p_e() {
        for least in [5_u32, 20, 50, 80, 160, 200] {
            let share = pace_share(ms(u64::from(least)));
            let r = f64::from(least);
            let theta = 10.0 / r;
            let bound = 0.75 * r.max(50.0) / r;
            let queue = |s: u32| pace_queue(f64::from(s) / f64::from(WHOLE), theta);
            assert!(share < WHOLE, "{least} ms: {share}");
            assert!(queue(share) <= bound, "{least} ms: {share}");
            assert!(queue(share + 10) > bound, "{least} ms: {share}");
        }
        // Where the grid's paths are, 20 and 80 ms: under half the window.
        assert!((380..=410).contains(&pace_share(ms(20))));
        assert!((420..=460).contains(&pace_share(ms(80))));
        for least in [ms(250), ms(1_000), Duration::ZERO] {
            assert_eq!(pace_share(least), WHOLE, "{least:?}");
        }
    }

    /// Until the bottleneck queued, the pacer lets out the share the least
    /// round trip gives, and follows the least as it falls; from then on the
    /// whole window, with the in-flight cap.
    #[test]
    fn the_pace_follows_the_least_round_trip_then_the_cap() {
        let whole = Some(Pace {
            share: WHOLE,
            cap: None,
        });
        let mut t = Train::new(1_200);
        assert_eq!(t.pace(), whole);
        t.step(ms(0), &acked(0), &gauges(0, 0), false);
        assert_eq!(t.pace(), whole, "no round trip yet");
        let shared = |least| {
            Some(Pace {
                share: pace_share(ms(least)),
                cap: None,
            })
        };
        sack(&mut t, 75, 2 * MSG, 75);
        assert_eq!(t.pace(), shared(75));
        sack(&mut t, 85, 4 * MSG, 70);
        assert_eq!(t.pace(), shared(70));
        // 9 ms above the least: no queue yet.
        sack(&mut t, 95, 6 * MSG, 79);
        assert_eq!(t.pace(), shared(70));
        // 10 ms above it: the bottleneck queued.
        sack(&mut t, 105, 8 * MSG, 80);
        assert!(t.capped && t.stopped.is_none());
        let cap = t.cap();
        assert!(cap < u64::MAX && cap.is_multiple_of(MSG), "{cap}");
        assert_eq!(
            t.pace(),
            Some(Pace {
                share: WHOLE,
                cap: Some(cap)
            })
        );
    }

    /// Whatever stops the train, the pacing ends with it: `pace` answers
    /// `None` from the stop on, so the writer's [`Pacing`] turns the pacer
    /// off at the step that stopped it. Each path is set up as the tests
    /// above set it up.
    #[test]
    fn the_pacing_ends_on_every_stop_path() {
        let ready = started();
        let loss = started();
        let expiry = started();
        let deadline = started();
        let mut bytes = started();
        bytes.written = BUDGET;
        // As `a_spent_write_budget_holds_the_writer_and_ends_on_a_floor`.
        let mut writes = started();
        writes.capped = true;
        writes.capped_acked = 6_000;
        writes.min = Some(ms(100));
        writes.first_sample = Some(ms(160));
        writes.rounds = 1;
        writes.round.mark = u64::MAX;
        writes.written = 16 * MSG;
        // A round of four samples, 40 ms above the least.
        let mut round = started();
        round.min = Some(ms(50));
        round.best = 10_000_000;
        round.round.min = Some(ms(90));
        round.round.samples = ROUND_SAMPLES;
        // As `a_run_of_samples_that_stand_stops_on_delay`.
        let mut run = queued();
        sack(&mut run, 200, 2 * MSG, 100);
        for (at, bytes) in [(250, 4 * MSG), (300, 6 * MSG), (350, 8 * MSG)] {
            sack(&mut run, at, bytes, 170);
        }
        // A round the cap held that grew less than 10 %, after one more.
        let mut plateau = started();
        plateau.round.held = true;
        plateau.flat = PLATEAU_ROUNDS - 1;
        plateau.last_round = 48_000;
        // Nothing acknowledged for 600 ms of a 50 ms path.
        let mut stall = started();
        stall.min = Some(ms(50));
        stall.last_ack_at = Some(ms(0));
        stall.last_at = ms(590);

        type Last = fn(&mut Train) -> Step;
        let paths: [(Stop, Train, Last); 10] = [
            (Stop::Ready, ready, |t| {
                t.step(ms(300), &acked(0), &gauges(3 * MSG, MSG), true)
            }),
            (Stop::Loss, loss, |t| {
                let g = Gauges {
                    fast_retransmits: 1,
                    ..gauges(3 * MSG, MSG)
                };
                t.step(ms(10), &acked(0), &g, false)
            }),
            (Stop::Loss, expiry, |t| {
                let g = Gauges {
                    t3_expiries: 1,
                    ..gauges(3 * MSG, MSG)
                };
                t.step(ms(10), &acked(0), &g, false)
            }),
            (Stop::Deadline, deadline, |t| {
                t.step(DEADLINE, &acked(0), &gauges(3 * MSG, MSG), false)
            }),
            (Stop::Budget, bytes, |t| {
                t.step(ms(10), &acked(0), &gauges(3 * MSG, MSG), false)
            }),
            (Stop::Budget, writes, |t| {
                t.step(ms(30), &acked(15 * MSG), &gauges(MSG, 0), false)
            }),
            (Stop::Delay, round, |t| {
                t.step(ms(100), &acked(4 * MSG), &gauges(0, 0), false)
            }),
            (Stop::Delay, run, |t| sack(t, 400, 10 * MSG, 170)),
            (Stop::Plateau, plateau, |t| {
                t.step(ms(100), &acked(4 * MSG), &gauges(0, 0), false)
            }),
            (Stop::Stall, stall, |t| {
                t.step(ms(600), &acked(0), &gauges(3 * MSG, 0), false)
            }),
        ];
        for (stop, mut t, last) in paths {
            let switch = PaceSwitch::default();
            let pacing = Pacing::on(switch.clone());
            pacing.follow(t.pace());
            assert!(switch.is_on(), "{stop:?}: on before the stop");
            let step = last(&mut t);
            pacing.follow(t.pace());
            assert_eq!(reading(step).stop, stop);
            assert_eq!(t.pace(), None, "{stop:?}");
            assert!(!switch.is_on(), "{stop:?}: off from the stop");
            // A step after the stop leaves it off.
            t.step(ms(2_000), &acked(1 << 20), &gauges(0, 0), false);
            pacing.follow(t.pace());
            assert!(!switch.is_on(), "{stop:?}");
        }
    }

    /// The writer's pacing: on from the train's first step, steered to each
    /// step's pace, off at the stop; and off when it is dropped without a
    /// stop, as an error that ends the writer, or a writer dropped with its
    /// train, drops it.
    #[test]
    fn the_pacing_is_off_once_the_writer_stops_ends_or_is_dropped() {
        let switch = PaceSwitch::default();
        let pacing = Pacing::on(switch.clone());
        assert!(switch.is_on());
        assert_eq!((switch.share(), switch.cap()), (WHOLE, None));
        pacing.follow(Some(Pace {
            share: 440,
            cap: None,
        }));
        assert_eq!((switch.share(), switch.cap()), (440, None));
        pacing.follow(Some(Pace {
            share: WHOLE,
            cap: Some(9_600),
        }));
        assert_eq!((switch.share(), switch.cap()), (WHOLE, Some(9_600)));
        pacing.follow(None);
        assert!(!switch.is_on());
        assert_eq!((switch.share(), switch.cap()), (WHOLE, None));
        drop(pacing);
        assert!(!switch.is_on());

        // A writer that ends on an error before its train stops.
        fn writer(switch: PaceSwitch, fail: bool) -> Result<(), &'static str> {
            let pacing = Pacing::on(switch);
            pacing.follow(Some(Pace {
                share: 440,
                cap: Some(4_800),
            }));
            if fail {
                return Err("the channel closed");
            }
            pacing.follow(None);
            Ok(())
        }
        assert_eq!(writer(switch.clone(), false), Ok(()));
        assert!(!switch.is_on());
        assert_eq!(writer(switch.clone(), true), Err("the channel closed"));
        assert!(!switch.is_on());
        assert_eq!((switch.share(), switch.cap()), (WHOLE, None));

        // A writer dropped with its train, mid-train.
        struct Writer {
            _train: Train,
            _pacing: Pacing,
        }
        let w = Writer {
            _train: started(),
            _pacing: Pacing::on(switch.clone()),
        };
        assert!(switch.is_on());
        drop(w);
        assert!(!switch.is_on());
    }

    /// A train after its first step.
    fn started() -> Train {
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &gauges(0, 0), false);
        t
    }
}

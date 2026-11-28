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
/// in-flight cap applies from then on, on trial until samples of packets sent
/// after it confirm the queue ([`Hold`]). It is P-d's floor too: two
/// packets sent together at 2 Mbit/s already put 4.9 ms between their
/// samples, and a cap that starts on that holds a climb at its first round's
/// rate.
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

/// FR-148 4a-3c — the samples of packets sent after the cap engaged on trial
/// that confirm it by standing [`QUEUED`] above the least, and those that
/// lift it by not standing ([`Hold`]). Two: a stall's samples all acknowledge
/// packets sent before it, so one stall never confirms the trial it began,
/// and a second stall alone does not either (at 2/80/C a runner's noise held
/// a second SACK to 101.8 ms against an 89 ms least, and one confirming
/// sample capped the train at three messages, a plateau at 0.11). Eight:
/// under a cap the estimate's pipe sizes, a slow link's queue may show in
/// one sample of four, as a round of [`ROUND_SAMPLES`] judges it, so two
/// standing samples need two rounds' worth before a trial lifts.
const TRIAL_STANDING: u32 = 2;
const TRIAL_CLEAR: u32 = TRIAL_STANDING * ROUND_SAMPLES;

/// FR-148 4a-3d — the packets a receiver acknowledges in one SACK while none
/// arrives out of order: every second one at once, and a lone one only when
/// its delayed acknowledgement fires (RFC 4960 §6.2, and the vendored
/// crate's). A claim's probe is a whole number of them ([`Train::probe`]):
/// with an odd number in flight the last packet waits a round for its
/// partner, so a delivery the cap holds reads one message a round short, as
/// SCTP's pace and not the link's. At 8/20/C a first-round stall's lead of
/// 17.7 ms claimed 68 kB/s on a 1 MB/s link, and a probe of three came back
/// as one SACK of two, 2,400 bytes over 29.7 ms: 81 kB/s, within a quarter of
/// the claim, which stood, and the train stopped on a plateau at 0.09
/// (4a-3c's settling run, seed 4). With an odd probe allowed, the model of
/// the train breaks 794 runs that 4a-3c's rule holds, against 12 with an
/// even one.
const SACK_PACKETS: u64 = 2;

/// Once the bottleneck first queued, the train writes no more than what had
/// been acknowledged then and what its estimate of the rate carries in this
/// long ([`Train::write_budget`]); then it writes nothing, and the judgment
/// comes from what is in flight. At 0.4 Mbit/s the cap first applies with 5
/// to 6 KB acknowledged on path I and at 20 ms on path C, and 18 to 19 KB at
/// 80 and 250 ms on path C, whose climb is paced from its first window; the
/// estimate carries 35 KB at the link's rate: inside P-f's 64 KB for an
/// estimate up to 1.66 and 1.28 times the link's rate, however late the
/// judgment comes and however many trials lift; and the packets it leaves in
/// flight carry the run of samples that judges the queue.
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
/// trip, where the whole window's queue is 0.73 of one. The cap's trial
/// ([`Hold`]) leaves it as it was.
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
/// then on: the queue at that moment is the most the train causes. The cap
/// engages on trial (4a-3c, [`Hold`]), at that sample and with its bound,
/// so the trial adds no lag here.
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

/// FR-148 4a-3c — the in-flight cap on trial. A host stall reads as a queue:
/// a SACK the scheduler held, and those it held with it, sit as far above
/// the least as the stall was long, while the bottleneck's queue does not
/// move (prod 1: four SACKs over 33 ms, the window cells' one, each capped a
/// climb in its first rounds, where the estimate, and so the cap, was a
/// fraction of the link's rate). A queue is the path's: while it stands,
/// every packet sent into it meets it. A stall is a moment's: it delays only
/// the SACKs that cross it, and each of those acknowledges a packet sent
/// before it. So the cap engages at the sample it engaged at before, with
/// the same bound, the whole window and the write budget, and only samples of
/// packets sent after it engaged judge it: [`TRIAL_STANDING`] that stand
/// confirm it, and [`TRIAL_CLEAR`] that do not lift it. A stall cannot
/// confirm the trial it engaged, and a second one cannot alone. A trial that
/// began on a lead read as the link puts that lead to the test ([`Claim`]),
/// and a delivery that refutes it lifts the trial at once. Until it is
/// confirmed, no round counts as held at it, so a plateau never stops a
/// train on a cap a stall set, and the most one step acknowledged keeps its
/// value: a stall's held SACKs come at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hold {
    /// The step it engaged at: a sample of a packet sent before this does
    /// not judge it.
    at: Duration,
    /// How far the greatest sample stood above the least then.
    lead: Duration,
    /// Samples of packets sent since that stood, and that did not.
    stood: u32,
    clear: u32,
    /// The lead the trial read as the link, if it began on one.
    claim: Option<Claim>,
}

/// FR-148 4a-3d — a lead read as the link, put to the test by delivery. A
/// trial reads its engagement as a link that serialises one message in a
/// lead: the pair's, on any path where the first sample was a pair
/// ([`Train::pair`]), or under [`QUEUE_FLOOR`] and [`QUEUED`] its own lead
/// where that is shorter than the least round trip. A stall's lead reads the
/// same, so the lead is only a claim. While it is open the cap is its
/// probe ([`Train::probe`]), a whole number of [`SACK_PACKETS`] that SCTP
/// lets out and the receiver acknowledges, and a window opens at the first
/// judging sample once SCTP has the probe in flight. Over a least round trip
/// or more, a delivery more than a quarter above the claim refutes it. Until
/// then, while the probe's messages at the claimed rate take more than a
/// quarter longer than the window, a faster link had them to deliver within
/// it, and the window goes on; once they take less, the probe was too small
/// to refute over that window, and it grows by [`SACK_PACKETS`]. A delivery
/// never confirms a claim: on the claimed link the probe builds the link's
/// own queue, whose samples stand and confirm the trial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Claim {
    /// One message's serialisation at the claimed rate.
    lead: Duration,
    /// The probe's messages as the step began, and whether SCTP had them all
    /// in flight at a step since the probe last grew.
    probe: u64,
    out: bool,
    /// The first judging sample's `(at, acked)` once the probe was out: the
    /// window the delivery counts from.
    from: Option<(Duration, u64)>,
    /// Windows too short for the probe to refute the claim over.
    grown: u64,
    /// A delivery refuted it.
    refuted: bool,
}

/// FR-148 4a-3c — what the cap's trials came to, for the daemon's INFO line
/// at the stop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Trials {
    /// Trials the cap began, those confirmed, and those lifted.
    pub engaged: u32,
    pub confirmed: u32,
    pub lifted: u32,
    /// Leads read as the link and put to the test, and those the delivery
    /// refuted.
    pub claimed: u32,
    pub refuted: u32,
    /// FR-148 4a-3d — the claims made on a path whose least round trip is
    /// [`QUEUE_FLOOR`] and [`QUEUED`] or more, where 4a-3c read the pair as
    /// the link untested, and the times a probe grew.
    pub long: u32,
    pub grown: u32,
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
    /// Whether the first sample's packet left in the same step as the packet
    /// ahead of it: only then is its lead a serialisation ([`Train::pair`]).
    first_paired: bool,
    /// The bottleneck queued: the in-flight cap applies.
    capped: bool,
    /// The bytes acknowledged when the cap first applied, on trial or
    /// confirmed: the write budget counts from them, whatever came after.
    capped_acked: Option<u64>,
    /// The cap on trial, if it is (4a-3c): it applies as `capped` does,
    /// except that no round counts as held at it.
    hold: Option<Hold>,
    trials: Trials,
    /// `(at, sent)` at every step that saw bytes leave the association: when
    /// each byte was first sent, to the step.
    sends: Vec<(Duration, u64)>,
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
            first_paired: false,
            capped: false,
            capped_acked: None,
            hold: None,
            trials: Trials::default(),
            sends: Vec::new(),
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

    /// What the cap's trials came to so far.
    pub fn trials(&self) -> Trials {
        self.trials
    }

    /// How the association's pacer runs after this step: `None` once the
    /// train stopped, whatever stopped it. Until the bottleneck queued it
    /// lets out [`pace_share`] of the window over the least round trip in
    /// slow start; from then on the whole window, with the in-flight cap,
    /// which then binds what the association sends as well as what the
    /// train writes: chunks already pending wait while in flight is at or
    /// above it. A train that queued before anything estimated the rate has
    /// no cap yet. A cap on trial ([`Hold`]) paces as the cap does, and one
    /// lifted gives the share back.
    pub fn pace(&self) -> Option<Pace> {
        if self.stopped.is_some() {
            return None;
        }
        Some(if self.cap_applies() {
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

    /// The in-flight cap applies: it is confirmed or on trial.
    fn cap_applies(&self) -> bool {
        self.capped || self.hold.is_some()
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
        if self.sends.last().is_none_or(|&(_, s)| sent > s) {
            self.sends.push((at, sent));
        }
        if let Some(c) = self.open_claim() {
            // What SCTP let out of the probe the cap kept since the last step.
            let probe = self.probe(&c);
            if let Some(c) = self.hold.as_mut().and_then(|h| h.claim.as_mut()) {
                c.probe = probe;
                c.out |= gauges.inflight_bytes >= probe * msg;
            }
        }
        let step_acked = acked.saturating_sub(self.last_acked);
        // A SACK of one message is a lone packet's, and may have waited for
        // the receiver's delayed acknowledgement: it is not a sample.
        let mut ran = false;
        if step_acked > msg
            && let Some(s) = gauges.ack_delay_min
        {
            if self.first_sample.is_none() {
                // The sampled packet, the newest acknowledged, and the one
                // ahead of it left the association in one step.
                let ahead = self.sent_at(acked.saturating_sub(msg));
                self.first_paired = ahead.is_some() && ahead == self.sent_at(acked);
            }
            self.sample(s);
            self.judge(s, at, acked);
            if self.cap_applies() {
                self.capped_acked.get_or_insert(acked);
            }
            ran = self.extend_run(s, at, acked, sent);
        }
        if step_acked > 0 {
            if self.hold.is_none() {
                self.peak_step = self.peak_step.max(step_acked);
            }
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
        if self.cap_applies() {
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
    }

    /// FR-148 4a-3c — whether the bottleneck queued, from the sample `s` of
    /// the bytes up to `acked` that the step at `at` took. The greatest sample
    /// [`QUEUED`] above the least engages the cap on trial ([`Hold`]), with a
    /// claim where it began on a lead read as the link ([`Claim`]). On trial,
    /// only a sample whose packet left at or after the cap engaged judges it:
    /// [`TRIAL_STANDING`] that stand confirm it, and [`TRIAL_CLEAR`] that do
    /// not stand lift it, after which the greatest sample is the least again,
    /// and a sample [`QUEUED`] above it engages the cap anew. A delivery that
    /// refutes the claim lifts it at once, unless one of its samples stood
    /// (4a-3d): the trial has then met a queue of its own, which its samples
    /// judge.
    fn judge(&mut self, s: Duration, at: Duration, acked: u64) {
        let (Some(least), Some(max)) = (self.min, self.max_sample) else {
            return;
        };
        if self.capped {
            return;
        }
        let Some(mut hold) = self.hold else {
            let lead = max.saturating_sub(least);
            if lead >= QUEUED {
                let claim = self.claim(lead, least);
                self.trials.engaged += 1;
                self.trials.claimed += u32::from(claim.is_some());
                self.trials.long += u32::from(claim.is_some() && least >= QUEUE_FLOOR + QUEUED);
                self.hold = Some(Hold {
                    at,
                    lead,
                    stood: 0,
                    clear: 0,
                    claim,
                });
            }
            return;
        };
        if at.saturating_sub(s) < hold.at {
            return;
        }
        if let Some(c) = hold.claim.as_mut()
            && !c.refuted
        {
            self.test_claim(c, least, at, acked);
            if c.refuted && hold.stood == 0 {
                self.hold = None;
                self.max_sample = Some(least);
                self.trials.lifted += 1;
                return;
            }
        }
        let stands = s.saturating_sub(least) >= QUEUED;
        hold.stood += u32::from(stands);
        if hold.stood >= TRIAL_STANDING {
            self.hold = None;
            self.capped = true;
            self.trials.confirmed += 1;
            return;
        }
        hold.clear += u32::from(!stands);
        if hold.clear >= TRIAL_CLEAR {
            self.hold = None;
            self.max_sample = Some(least);
            self.trials.lifted += 1;
        } else {
            self.hold = Some(hold);
        }
    }

    /// FR-148 4a-3d — the claim a trial that engaged on `lead` over the
    /// least round trip `least` makes, if any: the pair's lead where the
    /// first sample was a pair, on any path; else, on a path under
    /// [`QUEUE_FLOOR`] and [`QUEUED`], the trial's own lead where it is
    /// shorter than the least round trip. On a long path 4a-3c read the pair
    /// as the link untested, and a first-round stall's pair, one message in
    /// 40.6 ms at 50 Mbit/s and 80 ms, held a cap of three messages for some
    /// 900 ms and spent the write budget it sized: floors at 0.00 and 0.03.
    fn claim(&self, lead: Duration, least: Duration) -> Option<Claim> {
        let lead = if self.pair() > 0 {
            self.first_sample?.saturating_sub(least)
        } else if least < QUEUE_FLOOR + QUEUED && lead < least {
            lead
        } else {
            return None;
        };
        Some(Claim {
            lead,
            probe: 0,
            out: false,
            from: None,
            grown: 0,
            refuted: false,
        })
    }

    /// The trial's claim while no delivery has refuted it.
    fn open_claim(&self) -> Option<Claim> {
        self.hold.and_then(|h| h.claim).filter(|c| !c.refuted)
    }

    /// FR-148 4a-3d — the claim `c` put to the test by the sample at `at` of
    /// the bytes up to `acked`. Once SCTP had the probe in flight, the first
    /// sample that judges the trial opens the window. From a least round trip
    /// `least` on, a delivery more than a quarter above the claimed rate
    /// refutes the claim. One within it says only that the link carried what
    /// it was given: while the probe's messages, at the claimed rate, take
    /// more than a quarter longer than the window, a faster link could have
    /// shown itself in it, and the window goes on; once they take less, the
    /// probe grows by [`SACK_PACKETS`], and the next window opens once SCTP
    /// has the larger probe out. Where every window that did not refute
    /// grew the probe at once, the model broke 134 runs that 4a-3c's rule
    /// holds, against 12: on the claimed link each growth is queue.
    fn test_claim(&mut self, c: &mut Claim, least: Duration, at: Duration, acked: u64) {
        if !c.out {
            return;
        }
        let Some((t, a)) = c.from else {
            c.from = Some((at, acked));
            return;
        };
        let span = self.span(t, a, at, acked);
        if span < least {
            return;
        }
        if rate(acked.saturating_sub(a), span) * 4 > rate(self.msg, c.lead) * 5 {
            c.refuted = true;
            self.trials.refuted += 1;
        } else if u128::from(c.probe) * c.lead.as_micros() * 4 <= span.as_micros() * 5 {
            c.grown += 1;
            c.out = false;
            c.from = None;
            self.trials.grown += 1;
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
            self.dispersion = self
                .dispersion
                .max(rate(acked - a, self.span(t, a, at, acked)));
        }
    }

    /// When the association first sent byte `b`, to the step; `None` for a
    /// byte no step saw leave.
    fn sent_at(&self, b: u64) -> Option<Duration> {
        let i = self.sends.partition_point(|&(_, s)| s < b);
        self.sends.get(i).map(|&(t, _)| t)
    }

    /// FR-148 4a-3c — the span a delivery of the bytes acknowledged from `a`,
    /// acknowledged at `t`, to `acked` at `at` is taken over: the
    /// acknowledgements', or the sends' of the same bytes where those took
    /// longer. A link never delivers bytes faster than they were sent, as the
    /// delivery-rate estimate of draft-cheng-iccrg-delivery-rate-estimation
    /// takes it; a host stall releases the SACKs it held together, and over
    /// their span alone the bytes read as delivered at several times the
    /// link's rate (8/20/I, a 40 ms stall: 3.9 MB/s on a 1 MB/s link, and a
    /// reading of 17.6 Mbit/s).
    fn span(&self, t: Duration, a: u64, at: Duration, acked: u64) -> Duration {
        let acks = at.saturating_sub(t);
        match (self.sent_at(a), self.sent_at(acked)) {
            (Some(from), Some(to)) => acks.max(to.saturating_sub(from)),
            _ => acks,
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
            rate(
                acked.saturating_sub(r.acked),
                self.span(r.at, r.acked, at, acked),
            ),
            rate(sent.saturating_sub(r.sent), d),
        )
    }

    /// FR-148 4a-3c — the most the train may have written once the
    /// bottleneck queued: what was acknowledged when the cap first applied,
    /// on trial or confirmed, and what the estimate carries in
    /// [`WRITE_SPAN`]. Where it counts from is set once and never moves, and
    /// it holds from then on, whether the cap still applies or not, and with
    /// a pair or without: P-f's bound at 0.4 Mbit/s is this sum, however
    /// many trials lift. A lift that moved it, after a trial in which no
    /// sample stood, counted a stall twice on a slow link: at 0.4 Mbit/s and
    /// 80 ms on path C, a 40 ms stall held the climb under a cap of three
    /// messages for 640 ms, and counted from the lift the train wrote 68,400
    /// bytes, over P-f's 64,000. A first-round stall on a fast link pays
    /// instead: it reads as a slow link's pair, the trial's three messages
    /// spend the budget its estimate sizes before eight samples lift it, and
    /// the train stops on the budget with a floor. A window's worth a
    /// least round trip would be no tighter: the cap is at least the
    /// estimate's pipe and a third, so the estimate's 700 ms is at most three
    /// quarters of what the cap carries in them. It follows the estimate as
    /// it grows, so a slow start whose estimate lags never meets it within a
    /// 175 ms round trip: a round writes about twice what the last delivered.
    /// `None` only while nothing estimates the rate, when no cap binds either.
    fn write_budget(&self) -> Option<u64> {
        let e = self.estimate();
        self.capped_acked
            .filter(|_| e > 0)
            .map(|a| a + bytes_in(e, WRITE_SPAN))
    }

    /// The rate the bottleneck serves, as estimated so far, bytes a second;
    /// 0 when nothing says yet.
    fn estimate(&self) -> u64 {
        self.dispersion.max(self.best).max(self.pair())
    }

    /// The packet-pair estimate, bytes a second: 0 on a link too fast for
    /// one packet's serialisation to show in the first sample's lead, and 0
    /// where the first sample's packet left a step after the one ahead of it
    /// (4a-3c). A lead is a serialisation only when the two packets reached
    /// the bottleneck together; a paced packet waits only for what is left of
    /// the one ahead, and its lead reads the link fast: path C's first round
    /// is paced, and at 0.4/80/C a 10.7 ms lead read 112 kB/s on a 50 kB/s
    /// link, the cap let 88 ms queue, and under noise 111 ms and 64,800 bytes.
    fn pair(&self) -> u64 {
        match (self.first_sample, self.min) {
            (Some(first), Some(min))
                if self.first_paired && first.saturating_sub(min) >= PAIR_LEAD =>
            {
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
            rate(
                acked.saturating_sub(self.round.acked),
                self.span(self.round.at, self.round.acked, at, acked),
            ),
            rate(sent.saturating_sub(self.round.sent), d),
        )
    }

    /// The most the train keeps in flight and pending once the bottleneck
    /// queued: the estimated path's pipe and a queue of a third of the round
    /// trip (of 50 ms at least); and never less than the best round's pipe
    /// and a message, nor [`CAP_FLOOR_MESSAGES`]. It is the nearest whole
    /// number of messages to that: a slow link's pipe is a few messages,
    /// and a train that writes in pairs fills it only with about a message
    /// more, which a cap cut down to whole messages can take away. While a
    /// claim is open, it is the claim's probe ([`Train::probe`]).
    fn cap(&self) -> u64 {
        match self.open_claim() {
            Some(c) => self.probe(&c) * self.msg,
            None => self.estimated_cap(),
        }
    }

    /// The cap the estimate sets, [`Train::cap`] without a claim's probe;
    /// `u64::MAX` while nothing estimates the rate.
    fn estimated_cap(&self) -> u64 {
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

    /// FR-148 4a-3d — the messages an open claim `c` keeps out: the trial's
    /// own cap, as the estimate moves ([`CAP_FLOOR_MESSAGES`] while nothing
    /// estimates the rate), rounded up to a whole number of
    /// [`SACK_PACKETS`], and that many more for every window too short to
    /// refute the claim over. On the claimed link the rounding queues one
    /// message more than the cap at most, and each growth two. A probe fixed
    /// at the trial's engagement broke 16 runs in the model, against 12. Its
    /// writes are the train's, so the write budget bounds them, whatever the
    /// probe asks.
    fn probe(&self, c: &Claim) -> u64 {
        let own = match self.estimated_cap() {
            u64::MAX => CAP_FLOOR_MESSAGES,
            cap => cap / self.msg,
        };
        own.next_multiple_of(SACK_PACKETS) + SACK_PACKETS * c.grown
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
        /// FR-148 4a-3c — a host stall: this long after the train starts,
        /// what reaches the agent waits this long ([`shaped_link::HostStall`]).
        stall: Option<(Duration, Duration)>,
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
                stall: None,
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
            let stall = self.stall.map_or(String::new(), |(at, len)| {
                format!(" stall {}+{}", at.as_millis(), len.as_millis())
            });
            format!(
                "{}/{}/{path}{noise}{stall}",
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
        /// What the cap's trials came to (4a-3c).
        trials: Trials,
        /// When the cell's host stall began, if it did.
        stall_began: Option<Duration>,
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
             est {} (best {} disp {} pair {}) kB/s capped {} cap {} hold {} budget {} wr {} -> {verdict}",
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
            if t.cap_applies() { t.cap() } else { 0 },
            t.hold.map_or("-".to_owned(), |h| format!(
                "{}@{:.1}",
                h.clear,
                h.at.as_secs_f64() * 1e3
            )),
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
        let slot = Arc::new(shaped_link::StallSlot::default());
        let (a, b) = match (cell.noise, cell.stall) {
            (Some(n), _) => shaped_link::shaped_noisy(cell.bps, cell.rtt / 2, cell.queue, n),
            (None, Some(_)) => {
                shaped_link::shaped_stalled(cell.bps, cell.rtt / 2, cell.queue, Arc::clone(&slot))
            }
            (None, None) => shaped_link::shaped(cell.bps, cell.rtt / 2, cell.queue),
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
        if let Some((at, len)) = cell.stall {
            slot.arm(shaped_link::HostStall { from: t0 + at, len });
        }
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
                Some(_) if train.hold.is_some() => {}
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
            trials: train.trials(),
            stall_began: slot.began().map(|b| b.saturating_duration_since(t0)),
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
             capped {cap}; trials {} lifted {} confirmed {} claims {} refuted {}; share {} then {}{}; {:?}",
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
            o.trials.engaged,
            o.trials.lifted,
            o.trials.confirmed,
            o.trials.claimed,
            o.trials.refuted,
            share(o.share_first),
            share(o.share_at_cap),
            o.stall_began.map_or(String::new(), |b| format!(
                "; stall began {} ms",
                b.as_millis()
            )),
            o.trials,
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

    /// FR-148 4a-3c — `cell`, with a host stall of [`STALL`] that starts
    /// `at_ms` after the train does ([`shaped_link::HostStall`]): the SACKs
    /// due in it reach the agent late, the first at its end and the rest a
    /// little later, while the link's own queue does not move.
    fn stalled(cell: Cell, at_ms: u64) -> Cell {
        Cell {
            stall: Some((Duration::from_millis(at_ms), Duration::from_millis(STALL))),
            ..cell
        }
    }

    /// The stall cells' stall: longer than the 33 ms over which a loaded
    /// runner's late SACKs capped the train in prod 1 (2/20/I's four
    /// standing samples, its window cells' one), and short of the 50 ms the
    /// rule was ranked up to.
    const STALL: u64 = 40;

    /// Each stall cell must read as its cell does without the stall: the
    /// cell's own checks and P-a to P-g. They stay measurements (D44): after
    /// the stall, one SACK a busy runner delivers 10 ms late among the
    /// samples that judge the cap's trial confirms it, as one late SACK
    /// capped the window cells in prod 1. The machine's tests are the gate.
    macro_rules! stall_cells {
        ($($name:ident: $cell:expr, $at:expr, $extra:expr;)*) => {$(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): see docs/rate-control.md"]
            async fn $name() {
                assert_cell(stalled($cell, $at), $extra).await;
            }
        )*};
    }

    stall_cells! {
        path_i_8_mbps_20_ms_stalls_in_the_first_round: Cell::grid(8_000_000, 20, OpenPath::InitAck), 0, none;
        path_i_8_mbps_20_ms_stalls_mid_climb: Cell::grid(8_000_000, 20, OpenPath::InitAck), 150, none;
        path_i_8_mbps_20_ms_stalls_before_the_cap: Cell::grid(8_000_000, 20, OpenPath::InitAck), 250, none;
        path_i_a_shallow_buffer_stalls_in_the_first_round: shallow(OpenPath::InitAck), 0, stops_on_loss;
        path_i_a_shallow_buffer_stalls_mid_climb: shallow(OpenPath::InitAck), 600, stops_on_loss;
        path_i_a_shallow_buffer_stalls_before_the_cap: shallow(OpenPath::InitAck), 1100, stops_on_loss;
        path_i_a_peer_window_stalls_in_the_first_round: peer_window(OpenPath::InitAck), 0, window_is_a_floor;
        path_i_a_peer_window_stalls_mid_climb: peer_window(OpenPath::InitAck), 600, window_is_a_floor;
        path_i_a_peer_window_stalls_before_the_stop: peer_window(OpenPath::InitAck), 1100, window_is_a_floor;
        // Path C's first SACK, about 285 ms in, is the handshake's lone
        // packet's and no sample; its first round's samples come from about
        // 450 ms, and a stall from 300 ms meets the first of them.
        path_c_a_peer_window_stalls_in_the_first_round: peer_window(OpenPath::CookieEcho), 300, window_is_a_floor;
        path_c_a_peer_window_stalls_mid_climb: peer_window(OpenPath::CookieEcho), 1200, window_is_a_floor;
        path_c_a_peer_window_stalls_before_the_stop: peer_window(OpenPath::CookieEcho), 2300, window_is_a_floor;
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
        t.capped_acked = Some(6_000);
        t.min = Some(ms(100));
        t.first_sample = Some(ms(160));
        t.first_paired = true;
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

    /// The write budget counts from the bytes acknowledged when the cap first
    /// applied, on trial, and holds from then on: on a link too fast for a
    /// pair too, and whatever came after. A trial that lifts after a sample
    /// stood leaves it where it was, and so does the next: at 0.4 Mbit/s and
    /// 250 ms a budget that each trial counted afresh let the train write
    /// 60,000 bytes, and with two samples to confirm, 78,000 at 80 ms on path
    /// C, where the first round has no pair (P-f: 64,000).
    #[test]
    fn the_write_budget_counts_from_the_first_cap_and_never_turns_off() {
        let mut t = climbing();
        assert_eq!(t.write_budget(), None);
        sack_exact(&mut t, ms(140), 2 * MSG, ms(120), 8 * MSG);
        assert_eq!(t.pair(), 0);
        let budget = |t: &Train| Some(2 * MSG + bytes_in(t.estimate(), WRITE_SPAN));
        assert_eq!(t.write_budget(), budget(&t));
        // One sample of a packet sent after the trial began stands.
        sack_exact(&mut t, ms(236), 4 * MSG, ms(95), 8 * MSG);
        assert!(t.hold.is_some_and(|h| h.stood == 1));
        for i in 0..TRIAL_CLEAR {
            let n = u64::from(i);
            sack_exact(&mut t, ms(238 + 2 * n), (6 + 2 * n) * MSG, ms(80), 8 * MSG);
        }
        assert!(t.hold.is_none() && !t.capped);
        assert_eq!(t.write_budget(), budget(&t));
        // A second trial, 20 messages on.
        sack_exact(&mut t, ms(300), 22 * MSG, ms(95), 8 * MSG);
        assert!(t.hold.is_some_and(|h| h.at == ms(300)));
        assert_eq!(t.capped_acked, Some(2 * MSG));
        assert_eq!(t.write_budget(), budget(&t));
        assert_eq!(
            t.trials(),
            Trials {
                engaged: 2,
                lifted: 1,
                ..Trials::default()
            }
        );
    }

    /// A trial in which no sample stood lifts, and the write budget still
    /// counts from where the cap first applied: P-f's bound is that sum,
    /// however many trials lift. Moved to the lift, the budget counted a
    /// stall twice on a slow link: at 0.4 Mbit/s and 80 ms on path C a 40 ms
    /// stall held the climb under a cap of three messages for 640 ms, and the
    /// train wrote 68,400 bytes, and under the runner's noise 75,600, over
    /// P-f's 64,000 (the model of the train).
    #[test]
    fn a_lifted_trial_never_moves_where_the_write_budget_counts_from() {
        let mut t = climbing();
        sack_exact(&mut t, ms(140), 2 * MSG, ms(120), 8 * MSG);
        assert_eq!(t.capped_acked, Some(2 * MSG));
        for i in 0..TRIAL_CLEAR {
            let n = u64::from(i);
            sack_exact(&mut t, ms(230 + 2 * n), (4 + 2 * n) * MSG, ms(80), 8 * MSG);
        }
        assert!(t.hold.is_none() && !t.capped);
        assert_eq!(t.capped_acked, Some(2 * MSG));
        assert_eq!(
            t.write_budget(),
            Some(2 * MSG + bytes_in(t.estimate(), WRITE_SPAN))
        );
        assert_eq!(
            t.trials(),
            Trials {
                engaged: 1,
                lifted: 1,
                ..Trials::default()
            }
        );
        // The next trial counts from the first one too.
        sack_exact(&mut t, ms(300), 20 * MSG, ms(95), 8 * MSG);
        assert!(t.hold.is_some_and(|h| h.at == ms(300)));
        assert_eq!(t.capped_acked, Some(2 * MSG));
    }

    /// P-f's bound at 0.4 Mbit/s: whatever the trials do, the train writes
    /// no more than what was acknowledged when the cap first applied and what
    /// the estimate carries in [`WRITE_SPAN`]. At 250 ms on path I the pair
    /// engages the cap on trial in the first round, here at four messages
    /// (4,800 bytes); with the estimate a quarter over the link's 50 kB/s
    /// that is 4,800 + 43,750 = 48,550 bytes, 15,450 under P-f's 64,000, and
    /// the estimate would have to read 1.69 times the link to reach it. The
    /// pair's trial lifts, and a stall engages another before the stop: the
    /// bound moves for neither, and between them, with no cap, it holds the
    /// writer alone.
    #[test]
    fn the_write_budget_holds_p_f_at_0_4_mbit_s() {
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &gauges(0, 0), false);
        sack_exact(&mut t, ms(300), 2 * MSG, ms(298), 2 * MSG);
        sack_exact(&mut t, ms(340), 4 * MSG, ms(274), MSG);
        assert!(t.hold.is_some() && t.pair() == rate(MSG, ms(24)));
        assert_eq!(t.capped_acked, Some(4 * MSG));
        t.best = 62_500;
        let bound = 4 * MSG + bytes_in(62_500, WRITE_SPAN);
        assert_eq!(bound, 48_550);
        // Two messages every 40 ms, 60 kB/s; a stall's SACK at 940.
        let mut done = 4 * MSG;
        let mut at = 340;
        let r = loop {
            at += 40;
            done = (done + 2 * MSG).min(t.written);
            let delay = if at == 940 { 300 } else { 274 };
            let inflight = t.written - done;
            // At 380 SCTP holds two messages its full window will not send.
            let pending = if at == 380 { 2 * MSG } else { 0 };
            let g = Gauges {
                ack_delay_min: Some(ms(delay)),
                ..gauges(inflight, pending)
            };
            let step = t.step(ms(at), &acked(done), &g, false);
            assert!(t.written <= bound, "{} bytes at {at} ms", t.written);
            if at == 900 {
                assert!(t.hold.is_none() && !t.capped && t.trials().lifted == 1);
            }
            if let Step::Stopped(r) = step {
                break r;
            }
        };
        assert_eq!(r.stop, Stop::Budget);
        // Each trial claims the pair's lead on this long path (4a-3d), and
        // its probe is the cap's four messages: the bound is the same.
        assert_eq!(
            t.trials(),
            Trials {
                engaged: 2,
                lifted: 1,
                claimed: 2,
                long: 2,
                ..Trials::default()
            }
        );
        assert_eq!(t.estimate(), 62_500);
        assert!(r.written_bytes <= bound && bound < 64_000);
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
    /// whole window, with the in-flight cap: on trial at first (4a-3c), and
    /// confirmed once the samples of two packets sent after it stand too.
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
        // 10 ms above it: the bottleneck queued. The cap engages, on trial.
        sack(&mut t, 105, 8 * MSG, 80);
        assert!(!t.capped && t.hold.is_some() && t.stopped.is_none());
        let cap = t.cap();
        assert!(cap < u64::MAX && cap.is_multiple_of(MSG), "{cap}");
        assert_eq!(
            t.pace(),
            Some(Pace {
                share: WHOLE,
                cap: Some(cap)
            })
        );
        // Packets sent at 108 and 110, after that, meet the queue too:
        // confirmed.
        sack(&mut t, 190, 10 * MSG, 82);
        assert!(!t.capped && t.hold.is_some_and(|h| h.stood == 1));
        sack(&mut t, 192, 12 * MSG, 82);
        assert!(t.capped && t.hold.is_none() && t.stopped.is_none());
        assert_eq!(
            t.pace(),
            Some(Pace {
                share: WHOLE,
                cap: Some(t.cap())
            })
        );
    }

    /// FR-148 4a-3c: a step at `at` whose SACK acknowledged up to `bytes`,
    /// its newest chunk `delay` after it left, with `inflight` in flight.
    fn sack_exact(t: &mut Train, at: Duration, bytes: u64, delay: Duration, inflight: u64) -> Step {
        let g = Gauges {
            ack_delay_min: Some(delay),
            ..gauges(inflight, 0)
        };
        t.step(at, &acked(bytes), &g, false)
    }

    /// A train 80 ms from its peer, climbing: its least round trip taken,
    /// its estimate 100 kB/s, which puts its cap at 9 messages, no sample
    /// standing, and its round far from its end. 40 messages written.
    fn climbing() -> Train {
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &gauges(0, 0), false);
        t.min = Some(ms(80));
        t.first_sample = Some(ms(80));
        t.max_sample = Some(ms(80));
        t.share = pace_share(ms(80));
        t.best = 100_000;
        t.rounds = 1;
        t.round.mark = u64::MAX;
        t.written = 40 * MSG;
        assert_eq!(t.cap(), 9 * MSG);
        t
    }

    /// A host stall: one SACK held 40 ms reads 40 ms over the least, and so
    /// do those its packets left before it was read. A single stalled
    /// sample never caps: the cap engages on trial, as it engaged before,
    /// and [`TRIAL_CLEAR`] samples of packets sent after it that show no
    /// queue lift it. The train paces at its share again, and the stall's
    /// sample no longer counts: only a new one 10 ms over the least engages
    /// the cap again.
    #[test]
    fn a_stalled_sample_never_caps() {
        let mut t = climbing();
        let cap = t.cap();
        sack_exact(&mut t, ms(140), 2 * MSG, ms(120), 8 * MSG);
        assert!(!t.capped, "a stall's sample confirmed the cap");
        assert_eq!(
            t.hold,
            Some(Hold {
                at: ms(140),
                lead: ms(40),
                stood: 0,
                clear: 0,
                claim: None
            })
        );
        assert_eq!(
            t.pace(),
            Some(Pace {
                share: WHOLE,
                cap: Some(cap)
            })
        );
        // Released with it: its packet left at 40, before the trial began.
        sack_exact(&mut t, ms(142), 4 * MSG, ms(102), 6 * MSG);
        assert!(!t.capped && t.hold.is_some_and(|h| h.clear == 0));
        // The writer keeps to the cap: 8 messages in flight leave it one.
        assert_eq!(
            sack_exact(&mut t, ms(150), 6 * MSG, ms(85), 8 * MSG),
            Step::Write(cap - 8 * MSG)
        );
        // Packets sent from 150 on, after it began, meet no queue.
        for i in 0..TRIAL_CLEAR - 1 {
            let n = u64::from(i);
            sack_exact(&mut t, ms(230 + 2 * n), (8 + 2 * n) * MSG, ms(80), 8 * MSG);
            assert!(!t.capped && t.hold.is_some_and(|h| h.clear == i + 1));
        }
        // On trial, what a step acknowledged may be a stall's held SACKs at
        // once: the most one step acknowledged keeps its value.
        assert_eq!(t.peak_step, 0);
        sack_exact(&mut t, ms(245), 22 * MSG, ms(80), 8 * MSG);
        assert!(!t.capped && t.hold.is_none());
        assert_eq!(
            t.trials(),
            Trials {
                engaged: 1,
                lifted: 1,
                ..Trials::default()
            }
        );
        assert_eq!(
            t.pace(),
            Some(Pace {
                share: pace_share(ms(80)),
                cap: None
            })
        );
        sack_exact(&mut t, ms(250), 24 * MSG, ms(89), 8 * MSG);
        assert!(t.hold.is_none(), "the stall's sample still counted");
        sack_exact(&mut t, ms(255), 26 * MSG, ms(90), 8 * MSG);
        assert!(!t.capped && t.hold.is_some_and(|h| h.at == ms(255)));
    }

    /// Two stalls: the second's SACK, of a packet sent after the first
    /// engaged the cap on trial, stands, as one confirming sample capped
    /// 2/80/C at three messages under a runner's noise, a plateau at 0.11.
    /// A queue stands for the packets that follow it; a stall does not: the
    /// samples of the next ones meet none, and the trial lifts.
    #[test]
    fn a_second_stall_never_confirms_a_trial() {
        let mut t = climbing();
        sack_exact(&mut t, ms(140), 2 * MSG, ms(120), 8 * MSG);
        // Sent at 150, held by a second stall: it stands, and only once.
        sack_exact(&mut t, ms(250), 4 * MSG, ms(100), 8 * MSG);
        assert!(!t.capped && t.hold.is_some_and(|h| (h.stood, h.clear) == (1, 0)));
        for i in 0..TRIAL_CLEAR {
            let n = u64::from(i);
            sack_exact(&mut t, ms(252 + 2 * n), (6 + 2 * n) * MSG, ms(80), 8 * MSG);
            assert!(!t.capped, "a second stall confirmed the cap");
        }
        assert!(t.hold.is_none());
        assert_eq!(
            t.trials(),
            Trials {
                engaged: 1,
                lifted: 1,
                ..Trials::default()
            }
        );
    }

    /// A queue the cap keeps small may show in one sample of four: two
    /// standing among eight confirm the trial. Lifted after four that did
    /// not stand, as a round's count would, the trial let a slow link's climb
    /// go on into the queue it had found: in the model at 0.4/80/I under
    /// noise, the next trial confirmed with 80.2 ms queued against P-e's 80,
    /// and at 0.4/250/I the train stopped on its budget at 0.71 of the link,
    /// where eight read 1.00.
    #[test]
    fn a_queue_that_stands_in_one_sample_of_four_confirms_the_trial() {
        let mut t = climbing();
        sack_exact(&mut t, ms(140), 2 * MSG, ms(92), 8 * MSG);
        assert!(t.hold.is_some());
        let mut bytes = 2 * MSG;
        for (i, at) in [230, 235, 240, 245, 250, 255, 260, 265]
            .into_iter()
            .enumerate()
        {
            assert!(!t.capped && t.hold.is_some(), "lifted before sample {i}");
            bytes += 2 * MSG;
            let delay = if i % 4 == 3 { 94 } else { 85 };
            sack_exact(&mut t, ms(at), bytes, ms(delay), 8 * MSG);
        }
        assert!(t.capped && t.hold.is_none());
        assert_eq!(
            t.trials(),
            Trials {
                engaged: 1,
                confirmed: 1,
                ..Trials::default()
            }
        );
    }

    /// A true queue: its first standing sample engages the cap at once, with
    /// the bound it had before, at the step the derivation of the pacer's
    /// share takes it at, so the trial adds no lag to it; the samples of the
    /// first two packets sent after that which meet the queue too confirm
    /// it, a least round trip and the queue later, its write budget counted
    /// from the bytes acknowledged when it engaged.
    #[test]
    fn a_true_queue_caps_within_the_derived_lag() {
        let mut t = climbing();
        sack_exact(&mut t, ms(140), 2 * MSG, ms(92), 8 * MSG);
        assert!(!t.capped);
        assert_eq!(
            t.pace(),
            Some(Pace {
                share: WHOLE,
                cap: Some(t.cap())
            })
        );
        // Its packet left at 55, before the cap engaged: it judges nothing.
        sack_exact(&mut t, ms(150), 4 * MSG, ms(95), 8 * MSG);
        assert!(!t.capped && t.hold.is_some_and(|h| h.stood == 0));
        // Left at 141 and 143: the queue stands for both.
        sack_exact(&mut t, ms(235), 6 * MSG, ms(94), 8 * MSG);
        assert!(!t.capped && t.hold.is_some_and(|h| h.stood == 1));
        sack_exact(&mut t, ms(237), 8 * MSG, ms(94), 8 * MSG);
        assert!(t.capped && t.hold.is_none());
        assert_eq!(t.capped_acked, Some(2 * MSG));
        assert_eq!(
            t.trials(),
            Trials {
                engaged: 1,
                confirmed: 1,
                ..Trials::default()
            }
        );
    }

    /// [`QUEUED`] is the edge both ways: 9 ms over the least neither engages
    /// nor confirms the cap, 10 ms does both; a trial that met one sample of
    /// no queue still waits for the rest.
    #[test]
    fn a_sample_exactly_at_queued_caps_when_confirmed() {
        let mut t = climbing();
        sack_exact(&mut t, ms(130), 2 * MSG, ms(89), 8 * MSG);
        assert!(!t.capped && t.hold.is_none());
        sack_exact(&mut t, ms(140), 4 * MSG, ms(90), 8 * MSG);
        assert!(!t.capped && t.hold.is_some());
        sack_exact(&mut t, ms(229), 6 * MSG, ms(89), 8 * MSG);
        assert!(!t.capped && t.hold.is_some_and(|h| (h.stood, h.clear) == (0, 1)));
        sack_exact(&mut t, ms(231), 8 * MSG, ms(90), 8 * MSG);
        assert!(!t.capped && t.hold.is_some_and(|h| (h.stood, h.clear) == (1, 1)));
        sack_exact(&mut t, ms(233), 10 * MSG, ms(90), 8 * MSG);
        assert!(t.capped && t.hold.is_none());
    }

    /// The trial's own edge: a stall's late sample whose packet left a
    /// microsecond before the cap engaged judges nothing, however late it
    /// reads; one that left at the step it engaged at judges it.
    #[test]
    fn a_stall_ending_at_the_trials_edge_never_caps() {
        let mut t = climbing();
        sack_exact(&mut t, ms(140), 2 * MSG, ms(120), 8 * MSG);
        let us = Duration::from_micros(1);
        sack_exact(&mut t, ms(239), 4 * MSG, ms(99) + us, 8 * MSG);
        assert!(!t.capped && t.hold.is_some_and(|h| (h.stood, h.clear) == (0, 0)));
        sack_exact(&mut t, ms(240), 6 * MSG, ms(100), 8 * MSG);
        assert!(!t.capped && t.hold.is_some_and(|h| h.stood == 1));
        sack_exact(&mut t, ms(241), 8 * MSG, ms(100), 8 * MSG);
        assert!(t.capped, "packets sent as the cap engaged meet the queue");
    }

    /// A slow link's pair: the first sample sits a message's serialisation
    /// above the least, 24 ms at 0.4 Mbit/s, and that lead engages the cap
    /// at the step that took the least, in the train's first round, with
    /// the write budget counted from then, as before; the queue its own
    /// bound keeps confirms it. 0.4/80/I read 87.7 ms of queue against P-e's
    /// 80 when nothing bound it before a later sample stood. The pair's lead
    /// is a claim on this long path too (4a-3d), and its probe the cap's six
    /// messages: on the claimed link its queue stands.
    #[test]
    fn a_slow_links_pair_engages_the_cap_in_the_first_round() {
        let mut t = Train::new(1_200);
        t.step(ms(0), &acked(0), &gauges(0, 0), false);
        sack(&mut t, 133, 2 * MSG, 132);
        assert!(t.hold.is_none() && !t.capped && t.first_paired);
        sack(&mut t, 241, 4 * MSG, 108);
        assert!(!t.capped && t.hold.is_some_and(|h| h.at == ms(241)));
        assert!(
            t.hold
                .is_some_and(|h| h.claim.is_some_and(|c| c.lead == ms(24)))
        );
        assert_eq!(t.cap(), 6 * MSG);
        assert_eq!(t.pair(), rate(MSG, ms(24)));
        assert_eq!(
            t.write_budget(),
            Some(4 * MSG + bytes_in(t.estimate(), WRITE_SPAN))
        );
        // Sent at 260 and 265, they meet the queue the cap keeps: confirmed.
        sack(&mut t, 390, 6 * MSG, 130);
        sack(&mut t, 395, 8 * MSG, 130);
        assert!(t.capped && t.hold.is_none());
    }

    /// A first sample whose packet left a step after the one ahead of it is
    /// no pair: path C's first round is paced, a packet waits only for what
    /// is left of the one ahead, and at 0.4/80/C a 10.7 ms lead read 112 kB/s
    /// on a 50 kB/s link, so the cap let 88 ms queue against P-e's 80, and
    /// under noise 111 ms and 64,800 bytes against P-f's 64,000. Two that
    /// left in one step are one.
    #[test]
    fn a_paced_first_sample_is_no_pair() {
        for paced in [true, false] {
            let mut t = Train::new(1_200);
            t.step(ms(0), &acked(0), &gauges(0, 0), false);
            // The first message leaves at 10; the second at 30 when paced.
            t.step(ms(10), &acked(0), &gauges(MSG, 3 * MSG), false);
            if paced {
                t.step(ms(30), &acked(0), &gauges(2 * MSG, 2 * MSG), false);
            } else {
                t.step(ms(10), &acked(0), &gauges(2 * MSG, 2 * MSG), false);
            }
            sack(&mut t, 160, 2 * MSG, 130);
            sack(&mut t, 200, 4 * MSG, 108);
            assert_eq!(t.first_paired, !paced, "paced {paced}");
            let pair = if paced { 0 } else { rate(MSG, ms(22)) };
            assert_eq!(t.pair(), pair, "paced {paced}");
        }
    }

    /// A train on a short path, climbing: its least round trip `least` ms,
    /// its best round `best` bytes a second, 40 messages written.
    fn short_path(least: u64, best: u64) -> Train {
        let mut t = climbing();
        t.min = Some(ms(least));
        t.first_sample = Some(ms(least));
        t.max_sample = Some(ms(least));
        t.share = pace_share(ms(least));
        t.best = best;
        t
    }

    /// FR-148 4a-3d: a delivery that agrees with a claim confirms nothing; the
    /// claimed link's own queue does. A slow link on a short path: the
    /// trial's lead of 24 ms claims 0.4 Mbit/s, and the cap is the probe, the
    /// trial's own three messages rounded up to two SACKs' four. Once SCTP has
    /// them out, a least round trip's delivery, 50 kB/s, agrees with the
    /// claim, and 4a-3c confirmed the trial on that, at a sample of no queue.
    /// A link a quarter faster would have shown in it as well, so the window
    /// goes on; once it outlasts what the probe could refute over, the probe
    /// grows by a pair, and the queue the six messages keep stands: two
    /// samples of it confirm the trial.
    #[test]
    fn a_delivery_never_confirms_a_claim_the_links_own_queue_does() {
        let mut t = short_path(44, 50_000);
        sack_exact(&mut t, ms(140), 2 * MSG, ms(68), 3 * MSG);
        assert!(t.hold.is_some_and(|h| h.claim.is_some()));
        assert_eq!(t.cap(), 4 * MSG);
        // SCTP has the probe out; the first judging sample opens the window.
        sack_exact(&mut t, ms(200), 4 * MSG, ms(50), 4 * MSG);
        // 48 ms on, two messages more: 50 kB/s, the claim's rate.
        sack_exact(&mut t, ms(248), 6 * MSG, ms(52), 4 * MSG);
        assert!(
            !t.capped,
            "a delivery that agreed with the claim confirmed it"
        );
        assert!(t.hold.is_some() && t.trials().refuted == 0);
        assert_eq!(
            t.cap(),
            4 * MSG,
            "the window went on with the probe as it was"
        );
        // 96 ms: four messages at 24 ms take 96 ms, under a quarter more.
        sack_exact(&mut t, ms(296), 8 * MSG, ms(66), 4 * MSG);
        assert!(!t.capped && t.hold.is_some());
        assert_eq!(t.cap(), 6 * MSG);
        sack_exact(&mut t, ms(300), 8 * MSG, ms(44), 6 * MSG);
        sack_exact(&mut t, ms(344), 10 * MSG, ms(70), 6 * MSG);
        assert!(t.capped && t.hold.is_none());
        let n = t.trials();
        assert_eq!(
            (n.engaged, n.confirmed, n.lifted, n.claimed, n.refuted),
            (1, 1, 0, 1, 0)
        );
    }

    /// FR-148 4a-3d: a long path's pair is a claim too. A stall held the
    /// first SACK of a train 80 ms from its peer, so its pair reads one
    /// message in 41 ms, 29 kB/s, on a link of several MB/s; 4a-3c read that
    /// as the link, untested, and its cap of three messages held the climb
    /// until eight clear samples lifted it, one a round trip, while the
    /// write budget the pair sized was spent: at 50 Mbit/s behind the
    /// viewer's window and at 8 Mbit/s behind the shallow buffer the train
    /// stopped on floors at 0.00 and 0.03. The probe's four messages come
    /// back within a round trip, twice the claim and more, which refutes it,
    /// and the trial lifts at once, none of its samples having stood. Where
    /// the write budget counts from does not move.
    #[test]
    fn a_first_round_stalls_pair_on_a_long_path_is_refuted_at_once() {
        let mut t = climbing();
        t.first_sample = Some(ms(121));
        t.max_sample = Some(ms(121));
        t.first_paired = true;
        t.best = 0;
        sack_exact(&mut t, ms(140), 2 * MSG, ms(80), 2 * MSG);
        assert!(t.hold.is_some_and(|h| h.claim.is_some()));
        assert_eq!(t.pair(), rate(MSG, ms(41)));
        assert_eq!(t.cap(), 4 * MSG);
        sack_exact(&mut t, ms(141), 2 * MSG, ms(80), 4 * MSG);
        // Sent from 141 on: four messages back within the round trip.
        sack_exact(&mut t, ms(221), 4 * MSG, ms(80), 2 * MSG);
        sack_exact(&mut t, ms(222), 6 * MSG, ms(80), 2 * MSG);
        assert!(t.hold.is_some(), "refuted within a SACK's span");
        sack_exact(&mut t, ms(302), 8 * MSG, ms(80), 2 * MSG);
        assert!(
            t.hold.is_none() && !t.capped,
            "a stall's pair held the trial"
        );
        let n = t.trials();
        assert_eq!(
            (n.engaged, n.confirmed, n.lifted, n.claimed, n.refuted),
            (1, 0, 1, 1, 1)
        );
        assert_eq!(t.capped_acked, Some(2 * MSG));
        assert_eq!(
            t.pace(),
            Some(Pace {
                share: pace_share(ms(80)),
                cap: None
            })
        );
    }

    /// FR-148 4a-3d: a probe is a whole number of SACKs. A first-round stall
    /// on a short path to a fast link: at 8/20/C a SACK held 17.7 ms over a
    /// 23 ms least claimed 68 kB/s, and 4a-3c's probe of three came back as
    /// one SACK of two, the third waiting for a partner, 2,400 bytes over
    /// 29.7 ms: 81 kB/s, within a quarter of the claim, which stood, and the
    /// train stopped on a plateau at 0.09. The probe here is four messages,
    /// two SACKs, and they come back faster than a quarter over the claim,
    /// which refutes it. A lead as long as the least round trip, or a path
    /// of 60 ms with no pair, claims nothing.
    #[test]
    fn a_claims_probe_is_a_whole_number_of_sacks() {
        let mut t = short_path(23, 60_000);
        sack_exact(&mut t, ms(140), 2 * MSG, ms(41), 3 * MSG);
        assert!(t.hold.is_some_and(|h| h.claim.is_some()));
        assert_eq!(t.cap(), 4 * MSG, "an odd probe reads SCTP's pace");
        sack_exact(&mut t, ms(141), 2 * MSG, ms(23), 4 * MSG);
        // Sent from 141 on: two SACKs of two, and two more a round later.
        for (at, bytes) in [(165, 4), (166, 6)] {
            sack_exact(&mut t, ms(at), bytes * MSG, ms(24), 2 * MSG);
            assert!(t.hold.is_some() && !t.capped);
        }
        sack_exact(&mut t, ms(190), 8 * MSG, ms(24), 2 * MSG);
        assert!(
            t.hold.is_none() && !t.capped,
            "a stall's lead held the trial"
        );
        let n = t.trials();
        assert_eq!(
            (n.engaged, n.confirmed, n.lifted, n.claimed, n.refuted),
            (1, 0, 1, 1, 1)
        );
        for (least, delay) in [(40, 80), (60, 75)] {
            let mut t = short_path(least, 100_000);
            sack_exact(&mut t, ms(140), 2 * MSG, ms(delay), 3 * MSG);
            assert!(t.hold.is_some_and(|h| h.claim.is_none()), "{least} {delay}");
        }
    }

    /// FR-148 4a-3d: a probe too small to refute a claim over its window
    /// grows until the delivery answers. A 25 ms stall on a 50 ms path to a
    /// fast link claims 48 kB/s, and 4a-3c's probe, as many messages as the
    /// claim serialises in the least round trip and 37.5 ms, was three: the
    /// rule refuted a faster link only for leads under about 13 ms there,
    /// and in the model 1,654 runs of stalls of 12 to 25 ms on 2 to 50 Mbit/s
    /// broke a property. Four messages a round of 82 ms read 59 kB/s, within
    /// a quarter of the claim, and 4a-3c confirmed the stall's trial on that.
    /// The four take 100 ms at the claimed rate, less than a quarter over the
    /// window, so they could not refute it there: the probe grows by a pair,
    /// and six come back at 89 kB/s, which refutes the claim.
    #[test]
    fn a_probe_too_small_to_refute_grows_until_the_delivery_answers() {
        let mut t = short_path(50, 20_000);
        sack_exact(&mut t, ms(140), 2 * MSG, ms(75), 3 * MSG);
        assert!(t.hold.is_some_and(|h| h.claim.is_some()));
        assert_eq!(t.cap(), 4 * MSG);
        sack_exact(&mut t, ms(141), 2 * MSG, ms(50), 4 * MSG);
        sack_exact(&mut t, ms(200), 4 * MSG, ms(50), 4 * MSG);
        sack_exact(&mut t, ms(241), 6 * MSG, ms(50), 4 * MSG);
        sack_exact(&mut t, ms(282), 8 * MSG, ms(50), 4 * MSG);
        assert!(
            !t.capped,
            "a stall's claim stood on a probe too small to refute it"
        );
        assert!(t.hold.is_some() && t.trials().refuted == 0);
        assert_eq!(t.cap(), 6 * MSG);
        sack_exact(&mut t, ms(283), 8 * MSG, ms(50), 6 * MSG);
        for (at, bytes) in [(340, 10), (367, 12), (394, 14)] {
            sack_exact(&mut t, ms(at), bytes * MSG, ms(50), 6 * MSG);
        }
        assert!(t.hold.is_none() && !t.capped);
        let n = t.trials();
        assert_eq!(
            (n.engaged, n.confirmed, n.lifted, n.claimed, n.refuted),
            (1, 0, 1, 1, 1)
        );
    }

    /// FR-148 4a-3d: the probe is the trial's own cap as the estimate moves,
    /// rounded up to whole SACKs, and two messages more for each window it
    /// grew by; a refuted claim's probe goes, and the cap is the estimate's
    /// again. A claim of the pair on a long path: the estimate's cap of nine
    /// messages is a probe of ten, and of twelve once it grew; as the
    /// estimate nearly doubles, the cap's seventeen are a probe of twenty.
    #[test]
    fn the_probe_follows_the_trials_cap_in_whole_sacks() {
        let mut t = climbing();
        t.first_sample = Some(ms(104));
        t.max_sample = Some(ms(104));
        t.first_paired = true;
        sack_exact(&mut t, ms(140), 2 * MSG, ms(80), 8 * MSG);
        let claim = t.hold.and_then(|h| h.claim);
        assert!(claim.is_some_and(|c| c.lead == ms(24)), "{claim:?}");
        assert_eq!(t.estimated_cap(), 9 * MSG);
        assert_eq!(t.cap(), 10 * MSG);
        let grow = |t: &mut Train| {
            if let Some(c) = t.hold.as_mut().and_then(|h| h.claim.as_mut()) {
                c.grown += 1;
            }
        };
        grow(&mut t);
        assert_eq!(t.cap(), 12 * MSG);
        t.best = 190_000;
        assert_eq!(t.estimated_cap(), 17 * MSG);
        assert_eq!(t.cap(), 20 * MSG);
        if let Some(c) = t.hold.as_mut().and_then(|h| h.claim.as_mut()) {
            c.refuted = true;
        }
        assert_eq!(t.cap(), 17 * MSG);
        assert_eq!(
            t.trials(),
            Trials {
                engaged: 1,
                claimed: 1,
                long: 1,
                ..Trials::default()
            }
        );
    }

    /// FR-148 4a-3d: a refutation lifts a trial at once only where none of
    /// its samples stood; one that stood met a queue of its own, which the
    /// trial's samples judge from then on, the cap the estimate's. Lifted on
    /// any refutation, the model broke 18 runs that 4a-3c's rule holds,
    /// against 12; never lifted by one, 13, with 8/80/I behind the shallow
    /// buffer stalled in the first round on a floor at 0.05.
    #[test]
    fn a_refutation_after_a_sample_stood_leaves_the_trial_to_its_samples() {
        let mut t = short_path(23, 60_000);
        sack_exact(&mut t, ms(140), 2 * MSG, ms(41), 3 * MSG);
        sack_exact(&mut t, ms(141), 2 * MSG, ms(23), 4 * MSG);
        sack_exact(&mut t, ms(165), 4 * MSG, ms(24), 2 * MSG);
        // Sent at 142, it met a queue: it stands.
        sack_exact(&mut t, ms(176), 6 * MSG, ms(34), 2 * MSG);
        assert!(t.hold.is_some_and(|h| h.stood == 1));
        sack_exact(&mut t, ms(190), 8 * MSG, ms(24), 2 * MSG);
        assert_eq!(t.trials().refuted, 1);
        assert!(
            t.hold.is_some() && !t.capped,
            "a trial that met a queue lifted"
        );
        assert_eq!(t.cap(), t.estimated_cap());
        sack_exact(&mut t, ms(200), 10 * MSG, ms(35), 2 * MSG);
        assert!(t.capped && t.hold.is_none());
    }

    /// FR-148 4a-3d: a window opens only once SCTP has the probe in flight,
    /// and spans a least round trip at least. While SCTP holds part of the
    /// probe back, in its window, its pacer or the write budget, a delivery
    /// says only that the link carried what it was given, and a probe grown
    /// on it would be queue: here three of the four are out, and a delivery
    /// of 60 kB/s over 40 ms against a claim of 100 grows nothing. SACKs a
    /// host stall released together read nothing over their own span: two
    /// messages a millisecond apart are 2.4 MB/s on the claimed link of
    /// 50 kB/s.
    #[test]
    fn a_window_needs_the_probe_out_and_a_least_round_trip() {
        let mut t = short_path(30, 20_000);
        sack_exact(&mut t, ms(140), 2 * MSG, ms(42), 3 * MSG);
        assert!(
            t.hold
                .is_some_and(|h| h.claim.is_some_and(|c| c.lead == ms(12)))
        );
        for (at, bytes) in [(190, 4), (230, 6), (270, 8)] {
            sack_exact(&mut t, ms(at), bytes * MSG, ms(30), 3 * MSG);
        }
        assert!(
            t.hold
                .is_some_and(|h| h.claim.is_some_and(|c| !c.out && c.from.is_none()))
        );
        assert_eq!(t.cap(), 4 * MSG, "a window without the probe out grew it");
        sack_exact(&mut t, ms(271), 8 * MSG, ms(30), 4 * MSG);
        sack_exact(&mut t, ms(300), 10 * MSG, ms(30), 2 * MSG);
        assert!(
            t.hold
                .is_some_and(|h| h.claim.is_some_and(|c| c.from == Some((ms(300), 10 * MSG))))
        );

        let mut t = short_path(44, 50_000);
        sack_exact(&mut t, ms(140), 2 * MSG, ms(68), 3 * MSG);
        sack_exact(&mut t, ms(141), 2 * MSG, ms(44), 4 * MSG);
        sack_exact(&mut t, ms(200), 4 * MSG, ms(50), 4 * MSG);
        sack_exact(&mut t, ms(201), 6 * MSG, ms(50), 4 * MSG);
        assert_eq!(t.trials().refuted, 0, "refuted over a millisecond");
    }

    /// FR-148 4a-3d: P-f holds whatever a probe asks. A probe is writes, and
    /// every write the train makes keeps within the write budget, counted
    /// once from what was acknowledged when the cap first applied: at 0.4
    /// Mbit/s on a short path the claim's probe, grown here to 44 messages,
    /// asks for 52,800 bytes in flight, more than the budget's 37,400 in all,
    /// and the train writes 37,200, stops on the budget, and never passes it.
    #[test]
    fn every_probe_keeps_within_the_write_budget() {
        let mut t = short_path(30, 50_000);
        t.written = 4 * MSG;
        sack_exact(&mut t, ms(140), 2 * MSG, ms(54), 2 * MSG);
        assert!(
            t.hold
                .is_some_and(|h| h.claim.is_some_and(|c| c.lead == ms(24)))
        );
        let bound = 2 * MSG + bytes_in(50_000, WRITE_SPAN);
        assert_eq!(t.write_budget(), Some(bound));
        assert_eq!(bound, 37_400);
        if let Some(c) = t.hold.as_mut().and_then(|h| h.claim.as_mut()) {
            c.grown = 20;
        }
        assert_eq!(t.cap(), 44 * MSG);
        let mut done = 2 * MSG;
        let mut at = 140;
        let r = loop {
            at += 48;
            done = (done + 2 * MSG).min(t.written);
            let inflight = t.written - done;
            let step = sack_exact(&mut t, ms(at), done, ms(30), inflight);
            assert!(t.written <= bound, "{} bytes at {at} ms", t.written);
            if let Step::Stopped(r) = step {
                break r;
            }
        };
        assert_eq!(r.stop, Stop::Budget);
        assert_eq!(r.written_bytes, 31 * MSG);
        assert!(r.written_bytes <= bound && bound < 64_000);
    }

    /// A round held at a cap on trial is not held at the cap: a stall whose
    /// cap holds a round that does not grow never stops the train on a
    /// plateau, as such caps stopped 8/80/I and the window cells at a
    /// quarter of a megabit in prod 1. Once a queue confirmed the cap, a
    /// held round that did not grow counts, as before.
    #[test]
    fn a_cap_on_trial_never_reads_a_plateau() {
        let g = |inflight, delay| Gauges {
            ack_delay_min: delay,
            cwnd: 1 << 20,
            ..gauges(inflight, 0)
        };
        let mut t = climbing();
        let cap = t.cap();
        t.flat = PLATEAU_ROUNDS - 1;
        t.last_round = 48_000;
        t.round.mark = 3 * MSG;
        // A stall's sample; the cap holds the writer, the windows have room.
        let step = t.step(ms(140), &acked(2 * MSG), &g(cap, Some(ms(120))), false);
        assert_eq!(step, Step::Write(0));
        // The round ends, 32 kB/s: no plateau, and no flat round.
        let step = t.step(ms(150), &acked(4 * MSG), &g(cap, None), false);
        assert!(matches!(step, Step::Write(_)), "{step:?}");
        assert_eq!(t.flat, 0);
        // Packets sent at 145 and 147 meet a queue: confirmed. The next round
        // the cap holds does not grow: a plateau.
        t.flat = PLATEAU_ROUNDS - 1;
        t.last_round = 1_000_000;
        t.round.mark = u64::MAX;
        assert_eq!(
            t.step(ms(240), &acked(6 * MSG), &g(cap, Some(ms(95))), false),
            Step::Write(0)
        );
        assert!(!t.capped && !t.round.held);
        assert_eq!(
            t.step(ms(242), &acked(8 * MSG), &g(cap, Some(ms(95))), false),
            Step::Write(0)
        );
        assert!(t.capped && t.round.held);
        t.round.mark = 9 * MSG;
        let r = reading(t.step(ms(250), &acked(10 * MSG), &g(cap, None), false));
        assert_eq!(r.stop, Stop::Plateau);
    }

    /// A stall releases the SACKs it held within a few milliseconds of each
    /// other, for bytes sent over 40 ms: what they acknowledge reads as
    /// delivered no faster than it was sent, in a round and in the
    /// dispersion alike.
    #[test]
    fn released_acknowledgements_read_no_faster_than_they_were_sent() {
        let mut t = climbing();
        // 36,000 bytes leave over 40 ms, from 100 to 140.
        for (at, pending) in [(100, 28 * MSG), (120, 18 * MSG), (140, 10 * MSG)] {
            assert_eq!(
                t.step(ms(at), &acked(0), &gauges(10 * MSG, pending), false),
                Step::Write(0)
            );
        }
        t.round = Round {
            at: ms(195),
            mark: 25 * MSG,
            ..Round::default()
        };
        // Their SACKs come 3 ms apart from 196 to 208.
        for (i, at) in [196, 199, 202, 205, 208].into_iter().enumerate() {
            let step = t.step(
                ms(at),
                &acked((i as u64 + 1) * 6 * MSG),
                &gauges(0, 0),
                false,
            );
            assert!(matches!(step, Step::Write(_)), "{step:?}");
        }
        assert_eq!(t.rounds, 2);
        assert_eq!(t.best, rate(30 * MSG, ms(40)));
        assert_eq!(t.dispersion, rate(24 * MSG, ms(40)));
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
        writes.capped_acked = Some(6_000);
        writes.min = Some(ms(100));
        writes.first_sample = Some(ms(160));
        writes.first_paired = true;
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

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
//! link.

use std::collections::VecDeque;
use std::time::Duration;

use webrtc::sctp::association::delivery::Delivery;
use webrtc::sctp::association::gauges::Gauges;

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

/// A round the cap held is judged before it ends, as HyStart++ judges every
/// round (RFC 9406 §4.2, N_RTT_SAMPLE), once it holds this many samples and
/// spans half the least round trip and [`EARLY_SPAN`]: if every sample so
/// far stood, the bottleneck was busy all that time, and the round's delivery
/// so far is the link's. A round the cap did not hold waits for its end: in
/// slow start the first samples of a round can be the tail of the previous
/// round's burst, with the link idle after it.
const ROUND_SAMPLES: u32 = 4;
const EARLY_SPAN: Duration = Duration::from_millis(50);

/// The train stops this long before [`DEADLINE`], or two step intervals
/// before it where steps come further apart: a step can come late.
const DEADLINE_LEAD: Duration = Duration::from_millis(25);

/// The fewest messages the in-flight cap allows: a receiver acknowledges
/// every second packet at once and a lone one only when its delayed
/// acknowledgement fires (the vendored receiver, after one such timeout, not
/// until the next packet comes), so two keep the acknowledgements coming and
/// a third keeps the link busy while they return.
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

/// Why a train stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// A round's least acknowledgement delay stood above the least seen: a
    /// standing queue at the bottleneck.
    Delay,
    /// Rounds the train held at its in-flight cap delivered no more than the
    /// round before: the path is full.
    Plateau,
    /// A fast retransmission or a T3-rtx expiry.
    Loss,
    /// The byte budget is spent.
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
    /// Its least acknowledgement delay, and how many it took.
    min: Option<Duration>,
    samples: u32,
    /// The cap held the writer while SCTP had room in both windows.
    held: bool,
    /// One of SCTP's windows held back a message the train had written.
    windowed: bool,
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
    /// The bottleneck queued once: the in-flight cap applies.
    capped: bool,
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
            events: VecDeque::new(),
            dispersion: 0,
            round: Round::default(),
            rounds: 0,
            best: 0,
            last_round: 0,
            flat: 0,
            high: None,
            stopped: None,
        }
    }

    /// The length of every message the train asks for.
    pub fn message_len(&self) -> usize {
        usize::try_from(self.msg).unwrap_or(usize::MAX)
    }

    /// One step: `at` is the time on any clock that does not go back,
    /// `delivery` and `gauges` the association's clock and gauges read just
    /// now, and `ready` whether the first frame is ready. The first step
    /// writes the congestion window and one message more; every later one
    /// writes what keeps slow start growing, within the first round's window
    /// and, once the bottleneck queued, the in-flight cap.
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
        if step_acked > msg
            && let Some(s) = gauges.ack_delay_min
        {
            self.sample(s);
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
        if step_acked > 0 && acked > self.round.mark {
            if let Some(stop) = self.close_round(at, acked, sent) {
                return self.stop(stop, at, gauges);
            }
        } else if self.early(at) && self.queue_stands() {
            // A round the cap held, its queue standing at every sample so
            // far: the link was busy all that time, and what it delivered
            // is the link's rate.
            let (delivered, offered) = self.round_rates(at, acked, sent);
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

    /// Whether the round in progress may be judged before it ends.
    fn early(&self, at: Duration) -> bool {
        let r = &self.round;
        r.held
            && r.samples >= ROUND_SAMPLES
            && self
                .min
                .is_some_and(|min| at.saturating_sub(r.at) >= (min / 2).max(EARLY_SPAN))
    }

    /// The rate the bottleneck serves, as estimated so far, bytes a second;
    /// 0 when nothing says yet.
    fn estimate(&self) -> u64 {
        let mut c = self.dispersion.max(self.best);
        if let (Some(first), Some(min)) = (self.first_sample, self.min) {
            let lead = first.saturating_sub(min);
            if lead >= PAIR_LEAD {
                c = c.max(rate(self.msg, lead));
            }
        }
        c
    }

    /// A round's least delay above the least seen that is a standing queue.
    fn standing(&self) -> Duration {
        match self.estimate() {
            0 => STANDING,
            c => STANDING + time_of(self.msg, c),
        }
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
    /// and a message, nor [`CAP_FLOOR_MESSAGES`].
    fn cap(&self) -> u64 {
        let (c, Some(min)) = (self.estimate(), self.min) else {
            return u64::MAX;
        };
        if c == 0 {
            return u64::MAX;
        }
        bytes_in(c, min + min.max(QUEUE_FLOOR) / 3)
            .max(bytes_in(self.best, min) + self.msg)
            .max(CAP_FLOOR_MESSAGES * self.msg)
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
        let stop = if self.queue_stands() {
            if pushed_back {
                self.high.get_or_insert(offered);
            }
            Some(Stop::Delay)
        } else if r.held && !r.windowed && delivered * 10 < self.last_round * 11 {
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
    use crate::media_delivery::shaped_link::{self, OpenPath};

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
            }
        }

        fn name(&self) -> String {
            let path = match self.path {
                OpenPath::InitAck => "I",
                OpenPath::CookieEcho => "C",
            };
            format!("{}/{}/{path}", self.bps as f64 / 1e6, self.rtt.as_millis())
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
    }

    async fn run(cell: &Cell) -> Outcome {
        let _turn = TURN.lock().await;
        #[cfg(target_os = "windows")]
        let _timer = crate::win_timer::TimerResolutionGuard::request_1ms();
        let (a, b) = shaped_link::shaped(cell.bps, cell.rtt / 2, cell.queue);
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
        let message = Bytes::from(vec![0x7e; train.message_len()]);
        let mut limits = Limits::default();
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
            match train.step(at, &d, &g, ready) {
                Step::Write(n) => {
                    for _ in 0..n / msg {
                        stream.write(&message).await.unwrap();
                    }
                }
                Step::Stopped(r) => break r,
            }
            tokio::time::sleep(TICK).await;
        };
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
        format!(
            "train cell {} window {}: {} low {:.3} ({:.2}) high {} floor_only {} rounds {} \
             written {} wire {} min_rtt {:?} ss0 {} fr {} t3 {} t {} ms; \
             peak queue {:.1} ms, tail drops {}, drained {}, taken {}, limited by window {} cwnd {}",
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
        )
    }

    /// Runs `cell`, prints what it read, and fails on every property it
    /// breaks and every one `extra` adds.
    async fn assert_cell(cell: Cell, extra: fn(&Cell, &Outcome) -> Vec<&'static str>) {
        let o = run(&cell).await;
        let line = describe(&cell, &o);
        println!("{line}");
        let mut b = broken(&cell, &o);
        b.extend(extra(&cell, &o));
        assert!(b.is_empty(), "{line}: breaks {b:?}");
    }

    fn none(_: &Cell, _: &Outcome) -> Vec<&'static str> {
        Vec::new()
    }

    macro_rules! grid {
        ($($name:ident: $bps:expr, $rtt:expr, $path:expr;)*) => {$(
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    /// RED, and kept as written: P-a does not hold here while SCTP's slow
    /// start is the step law. A SACK of two packets releases four, so in a
    /// round the queue grows by the window of the round before; a 20 ms queue,
    /// a quarter of the pipe, overflows once that passes a quarter, and no
    /// round closed before the loss is seen carries much more than half the
    /// link's rate (docs/rate-control.md, "The open-time train").
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "P-a cannot hold under slow start's bursts: see the doc comment"]
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
        // Round 1 ends at 100 ms, every sample at the least, 50 ms.
        let g = |delay: u64| Gauges {
            cwnd: 1 << 20,
            ack_delay_min: Some(ms(delay)),
            ..gauges(0, 0)
        };
        for (at, acked_bytes) in [(50, 2 * MSG), (100, 4 * MSG)] {
            if let Step::Write(n) = t.step(ms(at), &acked(acked_bytes), &g(50), false) {
                sent += n;
            }
        }
        assert_eq!(t.rounds, 1);
        assert_eq!(t.best, 4 * MSG * 10);
        // Round 2 ends when the first byte sent after it began is
        // acknowledged; its every sample sat 40 ms above the least.
        let r = reading(t.step(ms(200), &acked(sent), &g(90), false));
        assert_eq!((r.stop, r.floor_only, r.rounds), (Stop::Delay, false, 2));
        assert_eq!(r.min_rtt, Some(ms(50)));
        assert_eq!(r.low_bps, t.best * 8);
        assert!(r.low_bps >= 4 * MSG * 10 * 8);
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
        assert_eq!(t.close_round(ms(100), 4 * MSG, 4 * MSG), None);
        t.round.min = Some(ms(50) + Duration::from_micros(10_200));
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
}

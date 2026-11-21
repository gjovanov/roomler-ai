// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! FR-148 P0, step 2 — delivery samples from SCTP's acknowledgement clock, each
//! marked backlogged, application-limited or stalled. In shadow: nothing reads
//! them, and a line of their own reports them.
//!
//! The goodput estimator (`encode::goodput`) times the agent's `dc.send()`. That
//! call returns once SCTP has queued the bytes (its pending queue holds 128 KiB),
//! so it says how fast SCTP took them, and it is silent whenever the queue is
//! downstream. A sample here reads the association's acknowledgement clock
//! instead (`RTCSctpTransport::delivery_clock`, a patch of the vendored webrtc and
//! webrtc-sctp): the user-data bytes the peer acknowledged, counted in SCTP's SACK
//! handler over every stream, and how many times the queue of written, not yet
//! sent chunks filled and ran empty, counted in that queue.
//!
//! Two readings, the first heartbeat at least [`SAMPLE_MIN`] after the last
//! (about 2 s on both data-channel pumps: the VP9-444 pump beats every 30
//! frames, a second at 30 fps, the FFmpeg pump every 2 s), make one [`Sample`]:
//!
//! | Class | When | What it says about the path |
//! |---|---|---|
//! | backlogged | the queue held bytes for the whole sample, and some were acknowledged | it carried this rate with bytes waiting: evidence about capacity |
//! | application-limited | the queue was empty at the start or ran empty in it: the path took everything offered | a lower bound, which never lowers a capacity |
//! | stalled | the queue held bytes for the whole sample and none was acknowledged | evidence about stalls, never about capacity |
//!
//! The queue's fills and empties are counted inside SCTP, not polled, so a sample
//! is classed right whatever its length and however late a heartbeat runs.
//!
//! ⚠️ Nothing that acts reads these numbers: no rate, pacing or queue budget,
//! and neither the viewer nor the wire changes. `ROOMLERD_PATH_MODEL=off` takes no sample and
//! writes no line. What a sample cannot tell is in `docs/rate-control.md`
//! (§ Delivery samples).
//!
//! Step 3 writes a second line from the same call, [`MODEL_LINE`]: every path
//! number the pump's heartbeat carries, each with how it was obtained and how
//! old it is, and what FR-148 §4.1's model would hold beside them
//! (`encode::pipe::PathView`). Also in shadow, also off with the switch; see
//! `docs/rate-control.md` (§ Path numbers and their provenance).
//!
//! O10 writes a third, [`OPEN_LINE`], once a pump at its first media write:
//! which way SCTP opened the session's association (the peer's INIT ACK first,
//! or its COOKIE ECHO), read from the association's gauges without taking from
//! them, and the windows then. Also off with the switch; see
//! `docs/rate-control.md` (§ The open-time train).
//!
//! The delivery line also says whether a queue stood on the path: the
//! classifier above cannot see one that a TCP carrier holds, since the queue
//! of unsent chunks keeps running empty while seconds of bytes wait in the
//! carrier (`shaped_link::tcp_shape`). SCTP's acknowledgement delay holds that
//! wait: it is stamped when a chunk leaves that queue for the wire. The sampler takes
//! the association's gauges (`RTCSctpTransport::gauges`) where each sample
//! starts and closes, beside the clock: a sample whose least delay stands
//! [`QUEUED_OVER_MS`] over the session's least is queued, when it acknowledged
//! enough bytes for the delay to be its own ([`counts`]). Also in shadow, also
//! off with the switch; see `docs/rate-control.md` (§ The standing queue).
//!
//! FR-148 D46 B2 gives a sample one reader: its push-back on the shadow model
//! ([`push_back_of`]). A queued sample is a capacity at its own rate (P1); with
//! the in-flight budget computing, so is a counted one that was backlogged with
//! no still span in it and starts at or after the budget's first measured rate
//! (P3); a sample a still span met is none, since its release comes back as a
//! burst, nor is one that closed in a stall the budget marks within [`settle`]
//! after. The pump hands it to the governor's shadow model and path memory,
//! which no rate or gate reads, and which opens a session only under
//! `path_model = on` (`docs/rate-control.md`, § The path memory).

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::sctp::association::delivery::{Delivery, DeliveryClock};
use webrtc::sctp::association::gauges::{GaugeReader, Gauges};

use crate::encode::governor::RateGovernor;
use crate::encode::pipe::{Fall, PathNumber, PathRate, PathView, PushedBy, Ruled};
use crate::media_inflight::{Bound, Budget, Over, Pass, PumpCount, Shadow, WindowReport};

/// A sample closes at the first heartbeat at least this long after the last.
pub const SAMPLE_MIN: Duration = Duration::from_millis(1500);

/// How often the line is written. Each line folds about five samples.
pub const LINE_EVERY: Duration = Duration::from_secs(10);

/// The line's message, which a field read greps for.
pub const LINE: &str = "path delivery (FR-148 P0, shadow, ≈10s)";

/// A sample whose least acknowledgement delay stands this far over the
/// session's least is queued. In `shaped_link::tcp_shape`'s first runs, on a
/// deep lossless queue (TURN over TCP's shape) every loaded second stood this
/// far over it, 69 of 69, and on a queue that drops (TURN over UDP's) none
/// did: 65 ms over it at most, in 80 seconds, the most of four runs.
pub const QUEUED_OVER_MS: u64 = 250;

/// The longest a receiver holds the SACK of a lone packet, ms: the vendored
/// SCTP's `ACK_INTERVAL` (`timer/ack_timer.rs`), and the hold of Chrome's
/// dcSCTP and of usrsctp (Firefox). See [`counts`].
pub const DELAYED_ACK_MS: u64 = 200;

/// Whether a sample's least acknowledgement delay counts: the sample
/// acknowledged more than `max_payload × (ms / DELAYED_ACK_MS + 2)` bytes, the
/// most that held SACKs alone acknowledge in `ms`.
///
/// A receiver acknowledges the second of two packets at once, and holds the
/// SACK of a lone one for up to [`DELAYED_ACK_MS`], a wait its delay then
/// carries. For every SACK of a sample to have been held, each packet must
/// have come after the hold of the one before it ran out, so the SACKs left
/// the receiver at least [`DELAYED_ACK_MS`] apart. While the way back varies
/// by less than that, at most `ms / DELAYED_ACK_MS + 2` of them reach the agent
/// in `ms`, and each acknowledges one packet of at most `max_payload` bytes of
/// user data. A sample that acknowledged more had a SACK sent at once, whose
/// delay holds no wait, so its least holds none either. A receiver that holds
/// longer leaves fewer lone packets in a sample than this allows. `max_payload`
/// 0 (no gauges stored yet) counts nothing.
pub fn counts(ms: u64, bytes: u64, max_payload: u32) -> bool {
    let lone = (ms / DELAYED_ACK_MS).saturating_add(2);
    max_payload > 0 && bytes > u64::from(max_payload).saturating_mul(lone)
}

/// A delay in whole milliseconds, as the line writes it.
pub fn ms_of(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// FR-148's switch, `ROOMLERD_PATH_MODEL` (D28): `off` is the release before
/// P0 exactly (no delivery sample, no path memory, no line); `shadow` and `on`
/// sample and keep the path memory. `shadow`, the default, opens by today's
/// rate memory; `on` lets the path model's entry decide the opening of a
/// constrained FFmpeg-pump session (`media_pair_memory`, P1 step 3). The
/// config key `path_model` sets it on a device (a set variable wins).
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

    /// Anything but `off` takes delivery samples and keeps the path memory.
    pub fn samples(self) -> bool {
        self != Mode::Off
    }
}

/// The switch, read once per pump. Read through `tunnel_core::env`, as the
/// other media switches are: the one read of it.
pub fn mode() -> Mode {
    mode_of(tunnel_core::env::node_env("PATH_MODEL").as_deref())
}

/// `off` and `on` as written; unset or any other value is `shadow`.
pub fn mode_of(value: Option<&str>) -> Mode {
    match value {
        Some("off") => Mode::Off,
        Some("on") => Mode::On,
        _ => Mode::Shadow,
    }
}

/// `off` is the only value that stops the sampler; any other value, or none,
/// samples.
pub fn enabled_by(value: Option<&str>) -> bool {
    mode_of(value).samples()
}

/// What a sample says about the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// The sender had bytes the path had not taken for the whole sample, and the
    /// path delivered some: evidence about capacity.
    Backlogged,
    /// The queue was empty at the start or ran empty: the path took everything
    /// offered. A lower bound on what it carries.
    AppLimited,
    /// Bytes waited for the whole sample and none was acknowledged. Evidence
    /// about stalls, never about capacity.
    Stalled,
}

/// One delivery sample: its length, the bytes acknowledged in it, its class,
/// and what SCTP's gauges said over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub ms: u64,
    pub bytes: u64,
    pub class: Class,
    /// `None` when the gauges were not taken where the sample started and
    /// where it closed.
    pub queue: Option<QueueReading>,
}

/// What SCTP's gauges said over one sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueReading {
    /// The least acknowledgement delay of the sample's SACKs, ms; `None` when
    /// none gave one.
    pub least_ms: Option<u64>,
    /// How far that least stood over the session's least, ms; `None` when the
    /// sample does not count ([`counts`]) or no SACK gave a delay.
    pub standing_ms: Option<u64>,
    /// The gauges at the sample's close.
    pub gauges: Gauges,
    /// Fast retransmissions and T3-rtx expiries in the sample.
    pub loss: (u64, u64),
}

impl QueueReading {
    /// A standing queue: the sample counts, and its least stood
    /// [`QUEUED_OVER_MS`] or more over the session's least.
    pub fn queued(&self) -> bool {
        self.standing_ms.is_some_and(|s| s >= QUEUED_OVER_MS)
    }
}

impl Sample {
    /// The sample between two readings of one association's clock, `ms` apart.
    pub fn between(from: Delivery, to: Delivery, ms: u64) -> Self {
        let bytes = to.acked_bytes.saturating_sub(from.acked_bytes);
        let ran_empty = from.queue_empty() || to.queue_empties > from.queue_empties;
        let class = if ran_empty {
            Class::AppLimited
        } else if bytes == 0 {
            Class::Stalled
        } else {
            Class::Backlogged
        };
        Self {
            ms,
            bytes,
            class,
            queue: None,
        }
    }

    /// The delivered rate, bit/s; `None` for a sample of no length.
    pub fn bps(&self) -> Option<u64> {
        self.bytes.saturating_mul(8_000).checked_div(self.ms)
    }
}

/// FR-148 D46 B2 — a delivery sample's push-back on the shadow model: a
/// capacity at `bps`, and which rule of D46 item 10 made it one. FR-148 D48:
/// `reread` when rule 1 raised a P3 to the budget's rate at its close.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SamplePushBack {
    pub bps: u32,
    pub by: PushedBy,
    pub reread: bool,
}

/// FR-148 D48 — what a closed sample gives the shadow model
/// ([`sample_read_of`]): a push-back, or a P3 that rule 4 kept out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleRead {
    /// P1, or P3 at its own rate or re-read by rule 1.
    Push(SamplePushBack),
    /// A P3 with no loss in it and a hold SCTP's congestion window sized
    /// ([`Over::ramp`]): the association's ramp bound it, not the path.
    Ramp,
}

/// FR-148 D46 B2 — how a sample that a stall's still span met is read: its
/// release comes back as a burst, and a sample holding the burst reads more
/// than the path carries; a sample that closes while a stall begins reads
/// less.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Burst {
    /// The rule: a sample that a still span of the least and S met, from the
    /// span's first still reading to the step that read its release, is no
    /// capacity, P1 or P3 ([`Over::stood`]). Nor is one that closed in a
    /// stall: its push-back waits [`settle`] past its close, and a step of
    /// the budget that marks a stall inside that wait drops it
    /// ([`Self::waits`]).
    Exclude,
    /// Measured beside it, test only: the release's half of the rule, read
    /// at the close ([`Over::stood`]), and no wait.
    #[cfg(test)]
    Stood,
    /// Measured beside it, test only: the classifier's rate correction. A
    /// sample that starts inside a released span counts from the span's first
    /// still reading; P3 still asks that the clock never stood still for the
    /// least and S inside the sample ([`Over::still_inside`]).
    #[cfg(test)]
    Correct,
    /// Measured beside it, test only: no rule but P3's own. Each sample at
    /// its own rate.
    #[cfg(test)]
    Own,
}

impl Burst {
    /// A still span of the least and S that met the sample makes it none.
    fn reads_stood(self) -> bool {
        match self {
            Burst::Exclude => true,
            #[cfg(test)]
            Burst::Stood => true,
            #[cfg(test)]
            Burst::Correct | Burst::Own => false,
        }
    }

    /// The push-back waits out [`settle`], and a stall marked inside the wait
    /// drops it.
    pub fn waits(self) -> bool {
        self == Burst::Exclude
    }
}

/// The burst rule the pumps' samples are read by.
pub const BURST: Burst = Burst::Exclude;

/// FR-148 D46 B2 — how long after a sample's close the budget may still mark
/// a stall the close fell in: the least and S, which the stall's still span
/// must stand, and one held SACK ([`DELAYED_ACK_MS`]). The SACK of a lone
/// packet delivered as a stall begins comes back that late, inside the stall,
/// and ends its first still span short of the least and S; the span after it
/// is the one that stands. On the `tcp_shape` stall and outage cells the P3
/// samples that closed in a stall before either span stood read 0.73 to 0.90
/// times the link over three runs: held, with no still span of the least and S.
pub fn settle(b: &Budget) -> Duration {
    b.still_after() + Duration::from_millis(DELAYED_ACK_MS)
}

/// How many closed samples' push-backs may wait at once: a sample closes about
/// every 2 s, and [`settle`] passes 2 s only past a least of about 1.65 s.
const WAITING_MAX: usize = 4;

/// FR-148 D48 — what the budget's last step read, as the shadow model's
/// window rules take it: its rate, whether this association's figures gave
/// it, and SCTP's loss count. `None` before the association's first step.
pub fn path_rate(s: &Shadow) -> Option<PathRate> {
    let b = s.budget();
    Some(PathRate {
        bps: b.rate_bps().map(|r| u32::try_from(r).unwrap_or(u32::MAX)),
        measured: b.rate_measured(),
        loss: b.loss()?,
    })
}

/// FR-148 D46 B2 — a sample's push-back (D46 item 10), or `None`:
/// [`sample_read_of`]'s, when it gives one.
pub fn push_back_of(s: &Sample, over: Option<&Over>, burst: Burst) -> Option<SamplePushBack> {
    match sample_read_of(s, over, burst)? {
        SampleRead::Push(p) => Some(p),
        SampleRead::Ramp => None,
    }
}

/// FR-148 D46 B2 — what a sample gives the shadow model (D46 item 10), or
/// `None`. `over` is what the in-flight budget saw over the sample
/// (`Budget::over`), `None` while the budget is off:
///
/// | | budget off | budget on or in shadow |
/// |---|---|---|
/// | P1: queued ([`QueueReading::queued`]) | at its own rate | at its own rate |
/// | P3: counted, backlogged (SCTP's class, or holds in a share of each second's grains, [`Over::held`]), no still span of the least and S in it, and it starts at or after the budget's first measured rate | nothing | at its own rate |
/// | FR-148 D48 rule 1: such a P3 with a loss in it ([`QueueReading::loss`]) | nothing | at the larger of its own rate and the budget's at its close ([`Over::rate`]) |
/// | FR-148 D48 rule 4: such a P3 with no loss in it and a hold SCTP's congestion window sized ([`Over::ramp`]) | nothing | nothing: [`SampleRead::Ramp`] |
/// | a still span met it ([`Burst::Exclude`]) | (not seen) | nothing |
/// | anything else, an application-limited sample or one the opening held among them | nothing | nothing |
///
/// A sample read without gauges is never one: it has no queue reading. The
/// rule's other half, a stall marked inside [`settle`] after the close, is the
/// sampler's to apply ([`DeliverySampler::heartbeat`]). A sample that starts
/// in the budget's opening ([`Over::opening`]) is no P3 capacity: the opening
/// rate stood in and SCTP's window was still opening, so its holds and its
/// backlog were the opening's, not the path's. A P3's two D48 rules take a
/// loss first: a loss says the path was full, and the budget's rate, the
/// most of the same clock's one-second figures, reads the second the
/// recovery cost least; with no loss, a hold the congestion window sized
/// says only that the association was still ramping.
pub fn sample_read_of(s: &Sample, over: Option<&Over>, burst: Burst) -> Option<SampleRead> {
    let q = s.queue?;
    let by = match over {
        None => q.queued().then_some(PushedBy::P1)?,
        Some(o) => {
            if burst.reads_stood() && o.stood {
                return None;
            }
            let backlogged = s.class == Class::Backlogged || o.held;
            let counted = q.standing_ms.is_some();
            if q.queued() {
                PushedBy::P1
            } else if counted && backlogged && !o.still_inside && !o.opening {
                PushedBy::P3
            } else {
                return None;
            }
        }
    };
    let (bytes, ms) = match (burst, over.and_then(|o| o.released_from)) {
        #[cfg(test)]
        (Burst::Correct, Some((before, extra))) => (
            s.bytes.saturating_add(extra),
            s.ms.saturating_add(ms_of(before)),
        ),
        _ => (s.bytes, s.ms),
    };
    let bps = bytes.saturating_mul(8_000).checked_div(ms)?;
    let p = SamplePushBack {
        bps: u32::try_from(bps).unwrap_or(u32::MAX),
        by,
        reread: false,
    };
    if let (PushedBy::P3, Some(o)) = (by, over) {
        if q.loss != (0, 0) {
            let rate = o.rate.map_or(0, |r| u32::try_from(r).unwrap_or(u32::MAX));
            if rate > p.bps {
                return Some(SampleRead::Push(SamplePushBack {
                    bps: rate,
                    reread: true,
                    ..p
                }));
            }
        } else if o.ramp {
            return Some(SampleRead::Ramp);
        }
    }
    Some(SampleRead::Push(p))
}

/// One line: the samples since the last, folded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineReport {
    /// Milliseconds the line's samples cover.
    pub span_ms: u64,
    /// Samples per class: (backlogged, application-limited, stalled).
    pub samples: (u32, u32, u32),
    /// The backlogged samples' rates, bit/s: (lowest, over all their bytes,
    /// highest). `None` when there was none.
    pub backlogged_bps: Option<(u64, u64, u64)>,
    /// The highest application-limited rate, bit/s. `None` when there was none.
    pub app_limited_bps: Option<u64>,
    /// Milliseconds of stalled samples.
    pub stalled_ms: u64,
    /// Bytes acknowledged over the line's samples.
    pub acked_bytes: u64,
    /// The standing queue over the line's samples, from SCTP's gauges; `None`
    /// when no sample of the line was read with them.
    pub queue: Option<QueueLine>,
    /// FR-148 D46 B1 — the in-flight budget's window, computed in shadow;
    /// `None` when its switch is off.
    pub budget: Option<WindowReport>,
}

/// The standing queue over one line's samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueLine {
    /// The least acknowledgement delay, ms: the session's least, and the
    /// lowest and highest of the samples that count ([`counts`]). `None` when
    /// none of the line's samples counts.
    pub ack_ms: Option<(u64, u64, u64)>,
    /// The lowest and highest standing over the session's least, ms, of those
    /// samples.
    pub standing_ms: Option<(u64, u64)>,
    /// Of those samples, the queued ones ([`QueueReading::queued`]).
    pub queued: u32,
    /// The lowest and highest bytes in flight at a sample's close.
    pub inflight: (u64, u64),
    /// The lowest and highest congestion window at a sample's close.
    pub cwnd: (u32, u32),
    /// The slow start threshold at the last sample's close.
    pub ssthresh: u32,
    /// The lowest peer receive window at a sample's close.
    pub rwnd_low: u32,
    /// The most bytes waiting in SCTP's pending queue at a sample's close.
    pub pending_high: u64,
    /// Fast retransmissions and T3-rtx expiries over the line's samples.
    pub loss: (u64, u64),
}

/// The samples of the line being built.
#[derive(Clone, Debug, Default)]
struct Fold {
    report: LineReport,
    backlogged_bytes: u64,
    backlogged_ms: u64,
}

impl Fold {
    fn is_empty(&self) -> bool {
        self.report.span_ms == 0 && self.report.samples == (0, 0, 0)
    }

    /// One sample's gauges into the line.
    fn add_queue(&mut self, q: &QueueReading) {
        let g = q.gauges;
        let line = self.report.queue.get_or_insert(QueueLine {
            inflight: (g.inflight_bytes, g.inflight_bytes),
            cwnd: (g.cwnd, g.cwnd),
            rwnd_low: g.rwnd,
            ..QueueLine::default()
        });
        line.inflight = (
            line.inflight.0.min(g.inflight_bytes),
            line.inflight.1.max(g.inflight_bytes),
        );
        line.cwnd = (line.cwnd.0.min(g.cwnd), line.cwnd.1.max(g.cwnd));
        line.ssthresh = g.ssthresh;
        line.rwnd_low = line.rwnd_low.min(g.rwnd);
        line.pending_high = line.pending_high.max(g.pending_bytes);
        line.loss = (
            line.loss.0.saturating_add(q.loss.0),
            line.loss.1.saturating_add(q.loss.1),
        );
        if let (Some(least), Some(standing)) = (q.least_ms, q.standing_ms) {
            // The session's least as of the line's latest sample that counts:
            // it only falls, until a new association starts it over.
            let session = least.saturating_sub(standing);
            line.ack_ms = Some(match line.ack_ms {
                Some((_, lo, hi)) => (session, lo.min(least), hi.max(least)),
                None => (session, least, least),
            });
            line.standing_ms = Some(match line.standing_ms {
                Some((lo, hi)) => (lo.min(standing), hi.max(standing)),
                None => (standing, standing),
            });
            if q.queued() {
                line.queued = line.queued.saturating_add(1);
            }
        }
    }

    fn add(&mut self, s: Sample) {
        if let Some(q) = &s.queue {
            self.add_queue(q);
        }
        let r = &mut self.report;
        r.span_ms = r.span_ms.saturating_add(s.ms);
        r.acked_bytes = r.acked_bytes.saturating_add(s.bytes);
        let bps = s.bps().unwrap_or(0);
        match s.class {
            Class::Backlogged => {
                r.samples.0 = r.samples.0.saturating_add(1);
                self.backlogged_bytes = self.backlogged_bytes.saturating_add(s.bytes);
                self.backlogged_ms = self.backlogged_ms.saturating_add(s.ms);
                let (lo, _, hi) = r.backlogged_bps.unwrap_or((bps, 0, bps));
                let mean = self
                    .backlogged_bytes
                    .saturating_mul(8_000)
                    .checked_div(self.backlogged_ms)
                    .unwrap_or(0);
                r.backlogged_bps = Some((lo.min(bps), mean, hi.max(bps)));
            }
            Class::AppLimited => {
                r.samples.1 = r.samples.1.saturating_add(1);
                r.app_limited_bps = Some(r.app_limited_bps.unwrap_or(0).max(bps));
            }
            Class::Stalled => {
                r.samples.2 = r.samples.2.saturating_add(1);
                r.stalled_ms = r.stalled_ms.saturating_add(s.ms);
            }
        }
    }
}

/// One data-channel pump's delivery samples. Fed by SCTP itself (the counters
/// behind [`DeliveryClock`]); the pump drains it with one call per heartbeat,
/// [`DeliverySampler::heartbeat`], and its line is written every [`LINE_EVERY`].
#[derive(Debug)]
pub struct DeliverySampler {
    on: bool,
    /// FR-148 P1 — the switch as this pump read it, for the pair memory.
    mode: Mode,
    clock: Option<DeliveryClock>,
    /// The association's gauges, from the slot its clock came from.
    gauges: Option<GaugeReader>,
    /// The reading the next sample starts from, and the gauges taken there.
    base: Option<(Instant, Delivery, Option<Gauges>)>,
    /// The session's least acknowledgement delay, ms: the least of the
    /// samples that counted ([`counts`]). A new association starts it over.
    least_ms: Option<u64>,
    fold: Fold,
    line_since: Option<Instant>,
    /// Whose line a drop writes.
    session: Option<ObjectId>,
    /// Step 3 — the latest path numbers, not yet written, and when the last
    /// [`MODEL_LINE`] was (its own [`LINE_EVERY`]).
    view: Option<PathView>,
    model_since: Option<Instant>,
    /// O10 — whether this pump wrote its [`OPEN_LINE`].
    opened: bool,
    /// FR-148 D46 B1 — the in-flight budget, in shadow; `None` when its switch
    /// is off, or this sampler is (the line carries its fields). Boxed: its
    /// rings are 3.5 KB, kept out of the pump's state while it is off.
    budget: Option<Box<Shadow>>,
    /// FR-148 D46 B2 — closed samples' push-backs ([`push_back_of`]), each
    /// with the time [`settle`] ends; a stall the budget marks before then
    /// drops it ([`Self::on_frame`]), and the heartbeat hands the governor's
    /// shadow model each whose time has come. With the budget off a push-back
    /// waits for nothing. FR-148 D48: a P3 that rule 4 kept out waits in its
    /// place ([`SampleRead::Ramp`]), so that the `path model` line names the
    /// rule when the push-back would have been handed.
    waiting: VecDeque<(SampleRead, Instant)>,
    /// FR-148 D46 B3 — the pass [`Self::gate`] answered last, for the gate's
    /// terms; and, under `on`, whether the budget acted when the pump last
    /// said which mode acted (`None` before it first said).
    pass: Pass,
    said: Option<bool>,
}

impl DeliverySampler {
    /// On unless `ROOMLERD_PATH_MODEL=off`: the pump's one read of the switch,
    /// and of FR-148 D46's, `ROOMLERD_INFLIGHT_BUDGET`, beside it.
    pub fn from_env() -> Self {
        let mode = mode();
        let mut sampler = Self::new(mode.samples());
        sampler.mode = mode;
        sampler.budget = Shadow::for_pump(crate::media_inflight::mode(), sampler.on).map(Box::new);
        sampler
    }

    /// FR-148 P1 — the switch as this pump read it.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn new(on: bool) -> Self {
        Self {
            on,
            mode: if on { Mode::Shadow } else { Mode::Off },
            clock: None,
            gauges: None,
            base: None,
            least_ms: None,
            fold: Fold::default(),
            line_since: None,
            session: None,
            view: None,
            model_since: None,
            opened: false,
            budget: None,
            waiting: VecDeque::new(),
            pass: Pass::default(),
            said: None,
        }
    }

    /// A sampler with the in-flight budget in shadow, as
    /// `ROOMLERD_INFLIGHT_BUDGET=shadow` gives one.
    #[cfg(test)]
    pub(crate) fn with_budget(mut self) -> Self {
        self.budget = Shadow::for_pump(crate::media_inflight::Mode::Shadow, self.on).map(Box::new);
        self
    }

    /// FR-148 D46 B3 — a sampler with the in-flight budget `on`, as
    /// `ROOMLERD_INFLIGHT_BUDGET=on` gives one.
    #[cfg(test)]
    pub(crate) fn with_budget_on(mut self) -> Self {
        self.budget = Shadow::for_pump(crate::media_inflight::Mode::On, self.on).map(Box::new);
        self
    }

    /// The pump's one call, at its heartbeat: read the association's clock and
    /// its gauges, close a sample when one is due, and write the line when it
    /// is due; hand each closed sample's push-back whose [`settle`] has passed
    /// to the governor's shadow model (FR-148 D46 B2,
    /// [`RateGovernor::on_sample_push_back`]); and take the
    /// path numbers from the governor (step 3, `seed_age` the pair memory
    /// entry's age at the open), writing [`MODEL_LINE`] when that is due.
    /// Never waits: before the association exists, or while its transport's
    /// slot is held, the clock and gauges it already has answer, or nothing
    /// does. Off, it calls none of them.
    pub fn heartbeat(
        &mut self,
        pc: &RTCPeerConnection,
        session_id: ObjectId,
        governor: &mut RateGovernor,
        seed_age: Option<Duration>,
    ) {
        if self.on {
            let now = Instant::now();
            self.observe(
                now,
                session_id,
                || pc.sctp().delivery_clock(),
                || pc.sctp().gauges(),
            );
            while let Some(read) = self.due_read(now) {
                match read {
                    SampleRead::Push(p) => {
                        governor.on_sample_push_back(p.bps, p.by, now);
                        if p.reread {
                            governor.on_sample_ruled(Ruled::ReRead, now);
                        }
                    }
                    SampleRead::Ramp => governor.on_sample_ruled(Ruled::Ramp, now),
                }
            }
            self.on_view(now, session_id, |t| governor.path_view(t, seed_age));
        }
    }

    /// FR-148 D46 B2 — the oldest waiting push-back whose [`settle`] has passed
    /// by `now`, taken; `None` when none has. FR-148 D48: or a P3 rule 4 kept
    /// out.
    fn due_read(&mut self, now: Instant) -> Option<SampleRead> {
        let &(read, until) = self.waiting.front()?;
        (until <= now).then(|| {
            self.waiting.pop_front();
            read
        })
    }

    /// FR-148 D46 B2 — the budget marked a stall at `now`: a push-back still
    /// inside its [`settle`] closed in that stall, and is dropped.
    fn on_mark(&mut self, now: Instant) {
        self.waiting.retain(|&(_, until)| until <= now);
    }

    /// FR-148 D46 B2 — the next push-back the heartbeat would hand at `now`:
    /// the tests' look at what reaches the governor. FR-148 D48: a P3 that
    /// rule 4 kept out is passed over ([`Self::take_read`] sees it).
    #[cfg(test)]
    pub(crate) fn take_push_back(&mut self, now: Instant) -> Option<SamplePushBack> {
        while let Some(read) = self.due_read(now) {
            if let SampleRead::Push(p) = read {
                return Some(p);
            }
        }
        None
    }

    /// FR-148 D48 — the next [`SampleRead`] the heartbeat would hand at `now`.
    #[cfg(test)]
    pub(crate) fn take_read(&mut self, now: Instant) -> Option<SampleRead> {
        self.due_read(now)
    }

    /// [`Self::heartbeat`]'s samples at `now`, with the clock from `clock` and
    /// the gauges from `gauges`. The gauges are asked for only right after the
    /// clock came, from the same slot, so they are its association's. Off, it
    /// calls nothing and returns `None`.
    pub fn observe(
        &mut self,
        now: Instant,
        session_id: ObjectId,
        clock: impl FnOnce() -> Option<DeliveryClock>,
        gauges: impl FnOnce() -> Option<GaugeReader>,
    ) -> Option<Sample> {
        if !self.on {
            return None;
        }
        self.session = Some(session_id);
        if let Some(fresh) = clock() {
            if !self.clock.as_ref().is_some_and(|c| c.same(&fresh)) {
                // A first association, or a new one: no sample spans the two,
                // and the gauges and the least were the old one's.
                self.clock = Some(fresh);
                self.base = None;
                self.gauges = None;
                self.least_ms = None;
            }
            if self.gauges.is_none() {
                self.gauges = gauges();
            }
        }
        let reading = self.clock.as_ref()?.read();
        // FR-148 P0: the agent's one take of the gauges. A take resets the
        // least acknowledgement delay, so a second reader that took would split
        // each sample's least between the two, and neither would hold it. The
        // sampler takes here only, where a sample starts and where it closes
        // (`on_reading_with` calls `take` there and nowhere else), beside the
        // clock reading, so a sample's least is the least of its own SACKs.
        // Every other reader looks without taking (`GaugeReader::peek`: FR-148
        // O10's open line), and the train takes only in its tests.
        let reader = self.gauges.clone();
        let sample = self.on_reading_with(now, reading, || reader.as_ref().map(GaugeReader::take));
        if let Some(line) = self.take_line(now) {
            emit(session_id, &line);
        }
        sample
    }

    /// One reading of the clock at `now`, without gauges. See
    /// [`Self::on_reading_with`].
    pub fn on_reading(&mut self, now: Instant, reading: Delivery) -> Option<Sample> {
        self.on_reading_with(now, reading, || None)
    }

    /// One reading of the clock at `now`. The first sets where the next sample
    /// starts; one at least [`SAMPLE_MIN`] after it closes that sample, which it
    /// returns; one sooner changes nothing. `take` takes the gauges, and is
    /// called where a sample starts and where it closes, and nowhere else. Off,
    /// it takes nothing.
    pub fn on_reading_with(
        &mut self,
        now: Instant,
        reading: Delivery,
        take: impl FnOnce() -> Option<Gauges>,
    ) -> Option<Sample> {
        if !self.on {
            return None;
        }
        self.line_since.get_or_insert(now);
        let Some((since, from, from_gauges)) = self.base else {
            self.base = Some((now, reading, take()));
            return None;
        };
        let span = now.saturating_duration_since(since);
        if span < SAMPLE_MIN {
            return None;
        }
        let ms = u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
        let to_gauges = take();
        let mut sample = Sample::between(from, reading, ms);
        sample.queue = self.judge(&sample, from_gauges, to_gauges);
        // FR-148 D46 B2 — its push-back, by what the budget saw over it, once
        // the budget has had `settle` to mark a stall the close fell in.
        let budget = self.budget.as_deref().map(Shadow::budget);
        let over = budget.map(|b| b.over(since, now, from.acked_bytes));
        if let Some(read) = sample_read_of(&sample, over.as_ref(), BURST) {
            let until = now + budget.map_or(Duration::ZERO, settle);
            if self.waiting.len() == WAITING_MAX {
                self.waiting.pop_front();
            }
            self.waiting.push_back((read, until));
        }
        self.fold.add(sample);
        self.base = Some((now, reading, to_gauges));
        Some(sample)
    }

    /// What the gauges taken at a sample's start and close say over it. Both
    /// takes are needed: the least covers the SACKs since the take before, so
    /// it is the sample's own only when its start took too.
    fn judge(
        &mut self,
        s: &Sample,
        from: Option<Gauges>,
        to: Option<Gauges>,
    ) -> Option<QueueReading> {
        let (from, to) = (from?, to?);
        let least_ms = to.ack_delay_min.map(ms_of);
        let standing_ms = match least_ms {
            Some(least) if counts(s.ms, s.bytes, to.max_payload) => {
                let session = self.least_ms.map_or(least, |l| l.min(least));
                self.least_ms = Some(session);
                Some(least.saturating_sub(session))
            }
            _ => None,
        };
        Some(QueueReading {
            least_ms,
            standing_ms,
            gauges: to,
            loss: (
                to.fast_retransmits.saturating_sub(from.fast_retransmits),
                to.t3_expiries.saturating_sub(from.t3_expiries),
            ),
        })
    }

    /// The line, once [`LINE_EVERY`] has passed since the last and it holds a
    /// sample.
    pub fn take_line(&mut self, now: Instant) -> Option<LineReport> {
        let since = self.line_since?;
        if now.saturating_duration_since(since) < LINE_EVERY || self.fold.is_empty() {
            return None;
        }
        self.line_since = Some(now);
        Some(self.report())
    }

    /// The samples not written yet, as the drop writes them; `None` when there
    /// are none.
    pub fn take_rest(&mut self) -> Option<LineReport> {
        (!self.fold.is_empty()).then(|| self.report())
    }

    /// The fold's report, and FR-148 D46's budget window beside it.
    fn report(&mut self) -> LineReport {
        LineReport {
            budget: self.budget.as_deref_mut().map(Shadow::take_window),
            ..std::mem::take(&mut self.fold).report
        }
    }

    /// Step 3, [`Self::heartbeat`]'s path numbers at `now`: keep `view`'s as the
    /// latest, and write them once [`LINE_EVERY`] has passed since the last
    /// [`MODEL_LINE`] (the first heartbeat starts the count). A session that
    /// ends between two lines has its last numbers written by the drop. Off, it
    /// calls nothing.
    pub fn on_view(
        &mut self,
        now: Instant,
        session_id: ObjectId,
        view: impl FnOnce(Instant) -> PathView,
    ) {
        if !self.on {
            return;
        }
        self.session = Some(session_id);
        let since = *self.model_since.get_or_insert(now);
        self.view = Some(view(now));
        if now.saturating_duration_since(since) >= LINE_EVERY {
            self.model_since = Some(now);
            if let Some(view) = self.view.take() {
                emit_model(session_id, &view);
            }
        }
    }

    /// O10 — the pump's call at each media message it hands to its send task.
    /// The first reads the association's gauges without taking from them
    /// (`GaugeReader::peek`: the least acknowledgement delay stays for a reader
    /// that takes it) and writes [`OPEN_LINE`]; every later call returns at
    /// once. Never waits: a transport whose slot is held reads as no gauges.
    /// Off, it reads and writes nothing.
    pub fn on_media_write(&mut self, pc: &RTCPeerConnection, session_id: ObjectId, pump: Pump) {
        self.on_first_write(session_id, pump, || pc.sctp().gauges());
    }

    /// [`Self::on_media_write`], with the gauges from `gauges`. Returns what the
    /// line said, the one time it writes it; `None` after that, and off.
    pub fn on_first_write(
        &mut self,
        session_id: ObjectId,
        pump: Pump,
        gauges: impl FnOnce() -> Option<GaugeReader>,
    ) -> Option<OpenedBy> {
        if !self.on || self.opened {
            return None;
        }
        self.opened = true;
        let read = gauges().map(|g| g.peek());
        emit_open(session_id, pump, read.as_ref());
        Some(OpenedBy::of(read.as_ref()))
    }

    /// FR-148 D46 B1 — the in-flight budget's step, once per frame before
    /// capture, after today's gates let the frame through. The governor's
    /// applied rate stands for the opening rate, or before one is applied the
    /// rate the pump opened at. Returns nothing: in shadow the verdict reaches
    /// the line's counters and nothing else. FR-148 D46 B2: a step that saw a
    /// still span stand for the least and S, or read a release, keeps the next
    /// window out of the governor's shadow model and pair memory
    /// ([`RateGovernor::note_path_stall`]), and drops each push-back still
    /// inside its [`settle`]. Off, it reads nothing. FR-148 D46 B3: under
    /// `on`, on a constrained session, the step ran at [`Self::gate`], before
    /// the gate, and this reads nothing. FR-148 D48: beside the mark, the
    /// step's rate and SCTP's loss count go to the shadow model's window
    /// rules ([`RateGovernor::note_path_rate`]).
    pub fn on_frame(&mut self, pc: &RTCPeerConnection, governor: &mut RateGovernor) {
        if self.pass.acts() {
            return;
        }
        let marked = self.budget.as_mut().is_some_and(|b| {
            b.on_frame(pc, governor.applied_bps(), governor.open_at_bps());
            b.take_mark()
        });
        self.framed(governor, marked);
    }

    /// [`Self::on_frame`] on the association's own handles, as the tcp_shape
    /// harness's closed-loop writer holds them (FR-148 D51 B4c).
    #[cfg(test)]
    pub(crate) fn on_frame_with(
        &mut self,
        governor: &mut RateGovernor,
        clock: &DeliveryClock,
        gauges: &GaugeReader,
    ) {
        if self.pass.acts() {
            return;
        }
        let open = crate::media_inflight::opening(governor.applied_bps(), governor.open_at_bps());
        let marked = self.budget.as_mut().is_some_and(|b| {
            b.on_frame_with(
                Instant::now(),
                open,
                || Some(clock.clone()),
                || Some(gauges.clone()),
            );
            b.take_mark()
        });
        self.framed(governor, marked);
    }

    /// The step's end: a stall it marked goes to the governor's shadow model
    /// and drops the push-backs inside their settle, and its rate follows.
    fn framed(&mut self, governor: &mut RateGovernor, marked: bool) {
        if marked {
            governor.note_path_stall();
            self.on_mark(Instant::now());
        }
        self.note_path_rate(governor);
    }

    /// FR-148 D48 — the budget's last step, to the governor's shadow model:
    /// its rate, whether this association's figures gave it, and SCTP's loss
    /// count ([`path_rate`]). Off, or before the association's first step,
    /// nothing.
    fn note_path_rate(&self, governor: &mut RateGovernor) {
        if let Some(rate) = self.budget.as_deref().and_then(path_rate) {
            governor.note_path_rate(rate);
        }
    }

    /// FR-148 D46 B1 — at the pumps' 5 s re-check of the transport: a new
    /// selected candidate pair starts the budget's least over. Off, it reads
    /// nothing.
    pub async fn on_pair_check(&mut self, pc: &RTCPeerConnection) {
        if let Some(b) = self.budget.as_mut() {
            b.on_pair_check(pc).await;
        }
    }

    /// FR-148 D46 B1 — today's capacity gate held a pass: `full` when the send
    /// channel was full, `ledger` when the FFmpeg ledger's byte gate was shut.
    /// FR-148 D46 B3: while the budget acts, [`Self::gate`] counted the byte
    /// gate's firing at every pass, whatever shut it.
    pub fn note_gate(&mut self, full: bool, ledger: bool) {
        if let Some(b) = self.budget.as_mut() {
            if full {
                b.note_bound(Bound::ChannelFull);
            }
            if ledger && !self.pass.acts() {
                b.note_bound(Bound::Ledger);
            }
        }
    }

    /// FR-148 D46 B3 — the pump's call before its capacity gate decides, with
    /// the session's transport and today's byte gate for the pass (`false` on
    /// VP9-444). Under `on`, on a constrained session, the budget's step runs
    /// here, before the gate, and a stall it marks is handled as
    /// [`Self::on_frame`] handles one; the pass then acts through
    /// [`Self::shuts`], [`Self::on_skip`] and [`Self::sheds`]. Otherwise it
    /// reads nothing, and those answer as today. Under `on` the pump says once
    /// which mode acted on the session, and again when that changes.
    pub fn gate(
        &mut self,
        pc: &RTCPeerConnection,
        governor: &mut RateGovernor,
        session_id: ObjectId,
        constrained: bool,
        byte_gate: bool,
    ) {
        let pass = match self.budget.as_mut() {
            Some(b) => b.gate(
                pc,
                governor.applied_bps(),
                governor.open_at_bps(),
                constrained,
                byte_gate,
            ),
            None => Pass::today(byte_gate),
        };
        self.gate_pass(governor, session_id, pass);
    }

    /// [`Self::gate`] on the association's own handles, as the tcp_shape
    /// harness's closed-loop writer holds them (FR-148 D51 B4c).
    #[cfg(test)]
    pub(crate) fn gate_with(
        &mut self,
        governor: &mut RateGovernor,
        clock: &DeliveryClock,
        gauges: &GaugeReader,
        constrained: bool,
        byte_gate: bool,
    ) {
        let open = crate::media_inflight::opening(governor.applied_bps(), governor.open_at_bps());
        let pass = match self.budget.as_mut() {
            Some(b) => b.gate_with(
                Instant::now(),
                open,
                || Some(clock.clone()),
                || Some(gauges.clone()),
                constrained,
                byte_gate,
            ),
            None => Pass::today(byte_gate),
        };
        self.gate_pass(governor, ObjectId::from_bytes([0; 12]), pass);
    }

    /// The gate's end: the pass is kept for the gate's terms, a stall the
    /// acting budget marked goes to the governor, and an acting step's rate.
    fn gate_pass(&mut self, governor: &mut RateGovernor, session_id: ObjectId, pass: Pass) {
        if self.gated(Instant::now(), session_id, pass) {
            governor.note_path_stall();
        }
        if pass.acts() {
            self.note_path_rate(governor);
        }
    }

    /// The pass [`Self::gate`] answered, kept for the gate's terms. A stall
    /// the acting budget marked drops each push-back still inside its
    /// [`settle`], as [`Self::on_frame`] does; returns whether it marked one,
    /// for the governor.
    fn gated(&mut self, now: Instant, session_id: ObjectId, pass: Pass) -> bool {
        self.pass = pass;
        let Some(b) = self.budget.as_mut() else {
            return false;
        };
        let marked = pass.acts() && b.take_mark();
        let on = b.is_on();
        if marked {
            self.on_mark(now);
        }
        if on && self.said != Some(pass.acts()) {
            self.said = Some(pass.acts());
            say_budget_mode(session_id, pass.acts());
        }
        marked
    }

    /// FR-148 D46 B3 — the capacity gate's last term, after the send channel's
    /// and the followers': the budget's hold while it acts, counted on the
    /// line when it shut the pass by itself; today's byte gate otherwise.
    pub fn shuts(&mut self) -> bool {
        match self.budget.as_mut() {
            Some(b) => b.shut(&mut self.pass),
            None => self.pass.shuts(),
        }
    }

    /// FR-148 D46 B3 — the gate's skip: `decrease` (the governor's
    /// `on_backpressure_skip`, at the time it is given) runs unless FR-35's
    /// opener grace is open (`grace`), or the pass is the budget's hold in a
    /// still span, a stall of the carrier (D46's no-decrease rule). Anything
    /// else decreases as today.
    pub fn on_skip<T>(
        &self,
        grace: bool,
        decrease: impl FnOnce(Instant) -> Option<T>,
    ) -> Option<T> {
        if self.pass.decreases(grace) {
            decrease(Instant::now())
        } else {
            None
        }
    }

    /// FR-148 D46 B3 — whether the VP9-444 watermark arm sheds a frame `over`
    /// its mark: as today, but never while the budget acts; then the firing is
    /// counted on the line and nothing is shed.
    pub fn sheds(&mut self, over: bool) -> bool {
        match self.budget.as_mut() {
            Some(b) => b.shed(&self.pass, over),
            None => self.pass.sheds(over),
        }
    }

    /// FR-148 D46 B1 — the VP9-444 pump shed a frame over the watermark.
    pub fn note_watermark(&mut self) {
        if let Some(b) = self.budget.as_mut() {
            b.note_bound(Bound::Watermark);
        }
    }

    /// FR-148 D46 B1 — a `try_send` found the send channel full.
    pub fn note_full(&mut self) {
        if let Some(b) = self.budget.as_mut() {
            b.note_bound(Bound::ChannelFull);
        }
    }

    /// FR-148 D46 B1 — a frame entered the send channel: its bytes are the
    /// pump's own until SCTP takes them.
    pub fn on_handed(&self, len: u64) {
        if let Some(b) = self.budget.as_ref() {
            b.on_handed(len);
        }
    }

    /// FR-148 D46 B1 — the send task's handle on the pump's unsent bytes, one
    /// that counts nothing with the budget off.
    pub fn pump_count(&self) -> PumpCount {
        self.budget
            .as_deref()
            .map(Shadow::pump_count)
            .unwrap_or_default()
    }

    /// FR-148 D46 B3 — [`Self::gate`] on one frame's reading, as
    /// [`Self::on_frame_reading`] reads one; returns whether the acting budget
    /// marked a stall, which the pump hands the governor.
    #[cfg(test)]
    pub(crate) fn gate_reading(
        &mut self,
        now: Instant,
        acked: u64,
        g: Gauges,
        open_bps: u64,
        constrained: bool,
        byte_gate: bool,
    ) -> bool {
        let pass = match self.budget.as_mut() {
            Some(b) => b.gate_reading(now, acked, g, open_bps, constrained, byte_gate),
            None => Pass::today(byte_gate),
        };
        self.gated(now, ObjectId::from_bytes([0; 12]), pass)
    }

    /// One frame's reading for the budget, as [`Self::on_frame`] reads it from
    /// the association, its mark taken as that takes it.
    #[cfg(test)]
    pub(crate) fn on_frame_reading(&mut self, now: Instant, acked: u64, g: Gauges, open_bps: u64) {
        if self.pass.acts() {
            return;
        }
        let marked = self.budget.as_mut().is_some_and(|b| {
            b.on_reading(now, acked, g, open_bps);
            b.take_mark()
        });
        if marked {
            self.on_mark(now);
        }
    }

    #[cfg(test)]
    pub(crate) fn budget_mut(&mut self) -> Option<&mut Shadow> {
        self.budget.as_deref_mut()
    }
}

impl Drop for DeliverySampler {
    /// A session left before its next line still has its last samples written,
    /// and its last path numbers.
    fn drop(&mut self) {
        let Some(session_id) = self.session else {
            return;
        };
        if let Some(rest) = self.take_rest() {
            emit(session_id, &rest);
        }
        if let Some(view) = self.view.take() {
            emit_model(session_id, &view);
        }
    }
}

/// FR-148 D46 B3 — under `on`, which mode acted on the session: said by the
/// pump once, and again when a change of transport changes it.
fn say_budget_mode(session_id: ObjectId, acts: bool) {
    if acts {
        tracing::info!(
            %session_id,
            budget_acts = "on",
            "the in-flight budget acts on this relay session (FR-148 D46 B3): its hold \
             skips a pass before capture, and the relay byte gate and the VP9-444 \
             watermark arm hold nothing"
        );
    } else {
        tracing::info!(
            %session_id,
            budget_acts = "shadow",
            "the in-flight budget is on and runs as shadow on this direct session \
             (FR-148 D46 B3), beside its own gates"
        );
    }
}

/// The line. Its field names and their order are pinned by a test, since a
/// field read greps for them. The standing queue's fields follow when a
/// sample of the line was read with SCTP's gauges, and are left out when none
/// was. FR-148 D46's in-flight budget closes the line when its switch is not
/// `off`; with it off the line is as before, to the byte.
pub fn emit(session_id: ObjectId, line: &LineReport) {
    match (&line.queue, &line.budget) {
        (Some(q), None) => tracing::info!(
            %session_id,
            span_ms = line.span_ms,
            delivery_samples = ?line.samples,
            backlogged_bps = ?line.backlogged_bps,
            app_limited_bps = ?line.app_limited_bps,
            stalled_ms = line.stalled_ms,
            acked_bytes = line.acked_bytes,
            ack_ms = ?q.ack_ms,
            standing_ms = ?q.standing_ms,
            queued = q.queued,
            inflight = ?q.inflight,
            cwnd = ?q.cwnd,
            ssthresh = q.ssthresh,
            rwnd_low = q.rwnd_low,
            pending_high = q.pending_high,
            loss = ?q.loss,
            "{LINE}"
        ),
        (None, None) => tracing::info!(
            %session_id,
            span_ms = line.span_ms,
            delivery_samples = ?line.samples,
            backlogged_bps = ?line.backlogged_bps,
            app_limited_bps = ?line.app_limited_bps,
            stalled_ms = line.stalled_ms,
            acked_bytes = line.acked_bytes,
            "{LINE}"
        ),
        (Some(q), Some(b)) => tracing::info!(
            %session_id,
            span_ms = line.span_ms,
            delivery_samples = ?line.samples,
            backlogged_bps = ?line.backlogged_bps,
            app_limited_bps = ?line.app_limited_bps,
            stalled_ms = line.stalled_ms,
            acked_bytes = line.acked_bytes,
            ack_ms = ?q.ack_ms,
            standing_ms = ?q.standing_ms,
            queued = q.queued,
            inflight = ?q.inflight,
            cwnd = ?q.cwnd,
            ssthresh = q.ssthresh,
            rwnd_low = q.rwnd_low,
            pending_high = q.pending_high,
            loss = ?q.loss,
            budget_held = ?b.held,
            budget_max = b.budget_max,
            budget_out_max = b.out_max,
            budget_rate_bps = b.rate_bps,
            budget_least_ms = ?b.least_ms,
            budget_slack_ms = b.slack_ms,
            budget_ack_over_ms = ?b.ack_over_ms,
            budget_still = ?b.still,
            bound_watermark = b.bounds.0,
            bound_ledger = b.bounds.1,
            bound_full = b.bounds.2,
            budget_released = ?b.released,
            budget_still_admitted = b.still_admitted,
            budget_acted = ?b.acted,
            budget_acted_stalled = b.acted_stalled,
            "{LINE}"
        ),
        (None, Some(b)) => tracing::info!(
            %session_id,
            span_ms = line.span_ms,
            delivery_samples = ?line.samples,
            backlogged_bps = ?line.backlogged_bps,
            app_limited_bps = ?line.app_limited_bps,
            stalled_ms = line.stalled_ms,
            acked_bytes = line.acked_bytes,
            budget_held = ?b.held,
            budget_max = b.budget_max,
            budget_out_max = b.out_max,
            budget_rate_bps = b.rate_bps,
            budget_least_ms = ?b.least_ms,
            budget_slack_ms = b.slack_ms,
            budget_ack_over_ms = ?b.ack_over_ms,
            budget_still = ?b.still,
            bound_watermark = b.bounds.0,
            bound_ledger = b.bounds.1,
            bound_full = b.bounds.2,
            budget_released = ?b.released,
            budget_still_admitted = b.still_admitted,
            budget_acted = ?b.acted,
            budget_acted_stalled = b.acted_stalled,
            "{LINE}"
        ),
    }
}

/// FR-148 P0, step 3 — the path numbers' line, which a field read greps for.
pub const MODEL_LINE: &str = "path model (FR-148 P0, shadow, ≈10s)";

/// A path number as the line writes it: `<bps>/<how>/<age>ms`, or `None`.
struct Shown(Option<PathNumber>);

impl std::fmt::Display for Shown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(n) => n.fmt(f),
            None => f.write_str("None"),
        }
    }
}

/// FR-148 D48 — the rule that last kept a reading out of the shadow model,
/// re-read it or bounded it, as the line writes it: `<word>/<age>ms`, or `-`
/// while none has.
struct RuledShown(Option<(Ruled, Duration)>);

impl std::fmt::Display for RuledShown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some((rule, age)) => write!(f, "{}/{}ms", rule.word(), age.as_millis()),
            None => f.write_str("-"),
        }
    }
}

/// FR-148 D50 — the shadow model's last fall as the line writes it:
/// `<from_bps>-><to_bps>/<age>ms`, or `-` while none has.
struct FellShown(Option<Fall>);

impl std::fmt::Display for FellShown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(fall) => fall.fmt(f),
            None => f.write_str("-"),
        }
    }
}

/// Step 3's line. Each field is the heartbeat's field of the same name with
/// its tag; `viewer_rx_bps` and `seed_bps` have none there, and the `model_`
/// fields are the shadow model's. Their names and order are pinned by a test.
pub fn emit_model(session_id: ObjectId, v: &PathView) {
    let (anchor, floor, capacity) = v.belief;
    let belief = format!("({}, {}, {})", Shown(anchor), Shown(floor), Shown(capacity));
    tracing::info!(
        %session_id,
        goodput_bps = %Shown(v.goodput),
        pipe_belief = %belief,
        pipe_measured = %Shown(v.measured),
        prior_bps = %Shown(v.prior),
        learned_ceiling_bps = %Shown(v.learned_ceiling),
        slow_link_floor_bps = %Shown(v.slow_link_floor),
        viewer_rx_bps = %Shown(v.viewer_rx),
        seed_bps = %Shown(v.seed),
        model_floor_bps = %Shown(v.model_floor),
        model_capacity_bps = %Shown(v.model_capacity),
        model_stand_in_bps = %Shown(v.model_stand_in),
        // FR-148 D46 B2 — `p1` or `p3` when a delivery sample's push-back
        // lowered the model's floor last, or set its capacity; `-` otherwise.
        model_floor_push_back = %PushedBy::word_of(v.model_floor_by),
        model_capacity_push_back = %PushedBy::word_of(v.model_capacity_by),
        // FR-148 D48 — the rule that last kept a reading out of the model,
        // re-read it or bounded it (`stall`, `reread`, `ramp`, `bound` or
        // `hole`), and how long ago; `-` while none has.
        model_rule = %RuledShown(v.model_ruled),
        // FR-148 D50 — the model's last fall: the floor before and after,
        // and how long ago; `-` while none has.
        model_fell = %FellShown(v.model_fell),
        "{MODEL_LINE}"
    );
}

/// The pair memory's seed for `peer` as a session opens from it, which is
/// `RateMemory::seed_at_open`'s, and how old its entry is then: the age the
/// path model line gives the seed and every number that stands on it.
pub fn seed_at_open(path: &Path, peer: &str) -> (Option<u32>, Option<Duration>) {
    let memory = crate::encode::rate_memory::RateMemory::load(path);
    let now = crate::encode::rate_memory::now_unix();
    let seed = memory.seed_at_open(peer, now);
    let at = seed.and(memory.entries.get(peer)).map(|e| e.at_unix);
    (
        seed,
        at.map(|at| Duration::from_secs(now.saturating_sub(at))),
    )
}

/// FR-148 O10 — the open path's line, once a pump at its first media write,
/// which a field read greps for.
pub const OPEN_LINE: &str = "path open (FR-148 P0, O10)";

/// The data-channel pump that writes [`OPEN_LINE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pump {
    Vp9444Dc,
    FfmpegDc,
}

impl Pump {
    pub fn word(self) -> &'static str {
        match self {
            Pump::Vp9444Dc => "vp9-444-dc",
            Pump::FfmpegDc => "ffmpeg-dc",
        }
    }
}

/// Which way a session's association opened, as its gauges read at the pump's
/// first media write. The vendored SCTP sets the slow start threshold in three
/// places only: where the peer's INIT ACK arrives, to the peer's window (path
/// I), and where a fast retransmission or a T3-rtx expiry cuts it. An
/// association its peer's COOKIE ECHO established (path C) has a threshold of
/// 0 until its first loss. So before any loss, 0 is path C and above 0 is path
/// I; after one, the gauges cannot tell. The peer's window is no help: any SACK
/// sets it, on both paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenedBy {
    /// Path C: the threshold 0, and no loss yet.
    CookieEcho,
    /// Path I: the threshold above 0, and no loss yet.
    InitAck,
    /// A fast retransmission or a T3-rtx expiry came first. Either sets the
    /// threshold, on both paths.
    Unknown,
    /// No gauges: no association, or its transport's slot was held.
    Unread,
}

impl OpenedBy {
    /// The classification: a pure function of the gauges read at the first
    /// write.
    pub fn of(gauges: Option<&Gauges>) -> Self {
        match gauges {
            None => Self::Unread,
            Some(g) if g.fast_retransmits > 0 || g.t3_expiries > 0 => Self::Unknown,
            Some(g) if g.ssthresh == 0 => Self::CookieEcho,
            Some(_) => Self::InitAck,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::CookieEcho => "cookie_echo",
            Self::InitAck => "init_ack",
            Self::Unknown => "unknown",
            Self::Unread => "unread",
        }
    }
}

/// O10's line: the session, the pump, the open path, and the gauges it was
/// read from; without gauges, the first three alone. No field can carry an
/// address. The names and their order are pinned by a test.
pub fn emit_open(session_id: ObjectId, pump: Pump, gauges: Option<&Gauges>) {
    let open_path = OpenedBy::of(gauges).word();
    match gauges {
        Some(g) => tracing::info!(
            %session_id,
            pump = %pump.word(),
            open_path = %open_path,
            ssthresh = g.ssthresh,
            rwnd = g.rwnd,
            cwnd = g.cwnd,
            inflight_bytes = g.inflight_bytes,
            pending_bytes = g.pending_bytes,
            max_payload = g.max_payload,
            fast_retransmits = g.fast_retransmits,
            t3_expiries = g.t3_expiries,
            "{OPEN_LINE}"
        ),
        None => tracing::info!(
            %session_id,
            pump = %pump.word(),
            open_path = %open_path,
            "{OPEN_LINE}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};

    use super::shaped_link::{self, OpenPath};
    use super::*;
    use crate::encode::pipe::Basis;

    /// A reading: bytes acknowledged so far, and the queue's fills and empties
    /// so far.
    fn r(acked: u64, fills: u64, empties: u64) -> Delivery {
        Delivery {
            acked_bytes: acked,
            queue_fills: fills,
            queue_empties: empties,
        }
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn sid() -> ObjectId {
        ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap()
    }

    /// What `f` logs, as the daemon's log file writes it (`roomler_node_core::
    /// logging::init`'s file layer: time, level, message, fields; no target).
    pub(super) fn logged(f: impl FnOnce()) -> String {
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
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned()
    }

    #[test]
    fn a_queue_that_held_bytes_throughout_and_delivered_is_backlogged() {
        let s = Sample::between(r(0, 1, 0), r(500_000, 1, 0), 2_000);
        assert_eq!(s.class, Class::Backlogged);
        assert_eq!(s.bps(), Some(2_000_000));
    }

    #[test]
    fn a_queue_that_ran_empty_once_is_application_limited() {
        // Holding bytes at both readings, empty once between them.
        let s = Sample::between(r(0, 1, 0), r(300_000, 2, 1), 2_000);
        assert_eq!(s.class, Class::AppLimited);
        // Empty at the first reading, holding bytes from just after it.
        let s = Sample::between(r(0, 3, 3), r(300_000, 4, 3), 2_000);
        assert_eq!(s.class, Class::AppLimited);
        assert_eq!(s.bps(), Some(1_200_000));
    }

    #[test]
    fn bytes_that_waited_with_nothing_acknowledged_are_a_stall() {
        let s = Sample::between(r(1_000, 5, 4), r(1_000, 5, 4), 2_000);
        assert_eq!(s.class, Class::Stalled);
        assert_eq!(s.bps(), Some(0));
    }

    #[test]
    fn an_idle_association_is_application_limited_not_stalled() {
        let s = Sample::between(r(9_000, 7, 7), r(9_000, 7, 7), 2_000);
        assert_eq!(s.class, Class::AppLimited);
    }

    #[test]
    fn a_sample_of_no_length_has_no_rate() {
        assert_eq!(Sample::between(r(0, 1, 0), r(10, 1, 0), 0).bps(), None);
    }

    #[test]
    fn samples_tile_the_readings_from_the_first_on() {
        let t0 = Instant::now();
        let mut s = DeliverySampler::new(true);
        // The first reading only sets where the first sample starts.
        assert_eq!(s.on_reading(t0, r(0, 1, 0)), None);
        // A reading sooner than SAMPLE_MIN closes nothing and moves nothing.
        assert_eq!(s.on_reading(at(t0, 1_000), r(100_000, 1, 0)), None);
        // The next one closes the sample from the first reading.
        let a = s.on_reading(at(t0, 2_000), r(200_000, 1, 0)).unwrap();
        assert_eq!(
            (a.ms, a.bytes, a.class),
            (2_000, 200_000, Class::Backlogged)
        );
        // At exactly SAMPLE_MIN after the last, the next closes from there.
        let b = s.on_reading(at(t0, 3_500), r(260_000, 2, 1)).unwrap();
        assert_eq!((b.ms, b.bytes, b.class), (1_500, 60_000, Class::AppLimited));
        let c = s.on_reading(at(t0, 6_000), r(260_000, 3, 2)).unwrap();
        assert_eq!((c.ms, c.bytes, c.class), (2_500, 0, Class::AppLimited));
        // No gap and no overlap: the spans add up to the time read.
        assert_eq!(a.ms + b.ms + c.ms, 6_000);
        assert_eq!(a.bytes + b.bytes + c.bytes, 260_000);
    }

    #[test]
    fn the_line_comes_every_ten_seconds_and_folds_its_samples() {
        let t0 = Instant::now();
        let mut s = DeliverySampler::new(true);
        s.on_reading(t0, r(0, 1, 0));
        // 2 s backlogged at 2 Mbit/s, at 1 Mbit/s, then 2 s stalled, then 2 s
        // application-limited at 0.4 Mbit/s.
        s.on_reading(at(t0, 2_000), r(500_000, 1, 0));
        s.on_reading(at(t0, 4_000), r(750_000, 1, 0));
        s.on_reading(at(t0, 6_000), r(750_000, 1, 0));
        s.on_reading(at(t0, 8_000), r(850_000, 2, 2));
        assert_eq!(s.take_line(at(t0, 8_000)), None, "a line before 10 s");
        s.on_reading(at(t0, 10_000), r(850_000, 2, 2));
        let line = s.take_line(at(t0, 10_000)).unwrap();
        assert_eq!(
            line,
            LineReport {
                span_ms: 10_000,
                samples: (2, 2, 1),
                backlogged_bps: Some((1_000_000, 1_500_000, 2_000_000)),
                app_limited_bps: Some(400_000),
                stalled_ms: 2_000,
                acked_bytes: 850_000,
                queue: None,
                budget: None,
            }
        );
        // The next line starts empty and comes ten seconds after this one.
        s.on_reading(at(t0, 12_000), r(860_000, 3, 3));
        assert_eq!(s.take_line(at(t0, 19_999)), None);
        let next = s.take_line(at(t0, 20_000)).unwrap();
        assert_eq!(next.samples, (0, 1, 0));
        assert_eq!(next.span_ms, 2_000);
        assert_eq!(next.backlogged_bps, None);
    }

    /// The jupiter chain of 2026-11-02: an idle trickle was read as the path and
    /// written back five times. A still screen's keepalives, each one queued,
    /// sent and acknowledged between heartbeats, must yield only
    /// application-limited samples, so no sample of them can stand for what the
    /// path carries.
    #[test]
    fn an_idle_trickle_yields_only_application_limited_samples() {
        let t0 = Instant::now();
        let mut s = DeliverySampler::new(true);
        let mut samples = Vec::new();
        let mut lines = Vec::new();
        // Two minutes on the VP9-444 pump's 1 s heartbeat: a 2 KiB keepalive a
        // second; at some readings the latest one is still queued.
        for sec in 0..=120u64 {
            let fills = sec + 1;
            let empties = if sec % 3 == 0 { sec } else { sec + 1 };
            let reading = r(sec * 2_048, fills, empties);
            if let Some(x) = s.on_reading(at(t0, sec * 1_000), reading) {
                samples.push(x);
            }
            if let Some(l) = s.take_line(at(t0, sec * 1_000)) {
                lines.push(l);
            }
        }
        assert_eq!(samples.len(), 60);
        assert!(
            samples.iter().all(|x| x.class == Class::AppLimited),
            "{samples:?}"
        );
        assert!(!lines.is_empty());
        for l in &lines {
            assert_eq!(l.samples.0, 0, "a backlogged sample in {l:?}");
            assert_eq!(l.samples.2, 0, "a stalled sample in {l:?}");
            assert_eq!(l.backlogged_bps, None);
            assert_eq!(l.app_limited_bps, Some(16_384));
        }
    }

    #[test]
    fn the_switch_reads_off_and_only_off() {
        assert!(!enabled_by(Some("off")));
        assert!(enabled_by(None));
        assert!(enabled_by(Some("shadow")));
        assert!(enabled_by(Some("on")));
        assert!(enabled_by(Some("")));
    }

    /// FR-148 P1 (D28) — three modes; unset or unknown is `shadow`.
    #[test]
    fn the_switch_has_three_modes_and_defaults_to_shadow() {
        assert_eq!(mode_of(Some("off")), Mode::Off);
        assert_eq!(mode_of(Some("shadow")), Mode::Shadow);
        assert_eq!(mode_of(Some("on")), Mode::On);
        for unknown in [None, Some(""), Some("OFF"), Some("1"), Some("true")] {
            assert_eq!(mode_of(unknown), Mode::Shadow, "{unknown:?}");
        }
        assert!(Mode::On.samples() && Mode::Shadow.samples() && !Mode::Off.samples());
        assert_eq!(DeliverySampler::new(false).mode(), Mode::Off);
        assert_eq!(DeliverySampler::new(true).mode(), Mode::Shadow);
    }

    #[test]
    fn off_takes_no_sample_and_writes_no_line() {
        let t0 = Instant::now();
        let asked = Cell::new(false);
        let gauged = Cell::new(false);
        let out = logged(|| {
            let mut s = DeliverySampler::new(false);
            for i in 0..30u64 {
                let sample = s.observe(
                    at(t0, i * 2_000),
                    sid(),
                    || {
                        asked.set(true);
                        None
                    },
                    || {
                        gauged.set(true);
                        None
                    },
                );
                assert_eq!(sample, None);
                assert_eq!(s.on_reading(at(t0, i * 2_000), r(i * 1_000, 1, 0)), None);
                let take = || {
                    gauged.set(true);
                    Some(Gauges::default())
                };
                assert_eq!(s.on_reading_with(at(t0, i * 2_000), r(i, 1, 0), take), None);
                assert_eq!(s.take_line(at(t0, i * 2_000)), None);
            }
            // Its drop writes nothing either.
        });
        assert!(!asked.get(), "off asked SCTP for its clock");
        assert!(!gauged.get(), "off asked SCTP for its gauges");
        assert!(out.is_empty(), "off wrote {out:?}");
    }

    /// A line as a busy session on TURN over TCP writes it: two samples
    /// queued of the five.
    fn busy_line() -> LineReport {
        LineReport {
            span_ms: 10_012,
            samples: (2, 3, 0),
            backlogged_bps: Some((2_390_112, 2_401_000, 2_410_334)),
            app_limited_bps: Some(310_112),
            stalled_ms: 0,
            acked_bytes: 3_012_345,
            queue: Some(QueueLine {
                ack_ms: Some((61, 64, 1_318)),
                standing_ms: Some((3, 1_257)),
                queued: 2,
                inflight: (1_200, 1_048_576),
                cwnd: (4_380, 1_052_956),
                ssthresh: 1_048_576,
                rwnd_low: 0,
                pending_high: 131_072,
                loss: (0, 1),
            }),
            budget: None,
        }
    }

    #[test]
    fn the_line_names_its_fields() {
        let out = logged(|| emit(sid(), &busy_line()));
        let mut rest = out.as_str();
        for want in [
            " INFO ",
            LINE,
            "session_id=65a1b2c3d4e5f60718293a4b",
            "span_ms=10012",
            "delivery_samples=(2, 3, 0)",
            "backlogged_bps=Some((2390112, 2401000, 2410334))",
            "app_limited_bps=Some(310112)",
            "stalled_ms=0",
            "acked_bytes=3012345",
            // FR-148 P0 — the standing queue, from SCTP's gauges.
            "ack_ms=Some((61, 64, 1318))",
            "standing_ms=Some((3, 1257))",
            "queued=2",
            "inflight=(1200, 1048576)",
            "cwnd=(4380, 1052956)",
            "ssthresh=1048576",
            "rwnd_low=0",
            "pending_high=131072",
            "loss=(0, 1)",
        ] {
            let at = rest
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} is not in order in {out:?}"));
            rest = &rest[at + want.len()..];
        }
        assert_eq!(out.lines().count(), 1, "{out:?}");
        // An idle screen's line: every sample application-limited, and none
        // acknowledged enough bytes to count.
        let idle = LineReport {
            span_ms: 10_004,
            samples: (0, 5, 0),
            backlogged_bps: None,
            app_limited_bps: Some(16_384),
            stalled_ms: 0,
            acked_bytes: 20_480,
            queue: Some(QueueLine {
                inflight: (0, 2_048),
                cwnd: (4_380, 4_380),
                ssthresh: 1_048_576,
                rwnd_low: 1_046_528,
                pending_high: 0,
                ..QueueLine::default()
            }),
            budget: None,
        };
        let idle = logged(|| emit(sid(), &idle));
        for want in [
            "backlogged_bps=None",
            "ack_ms=None",
            "standing_ms=None",
            "queued=0",
        ] {
            assert!(idle.contains(want), "{want:?} is not in {idle:?}");
        }
        // A line no sample of which was read with the gauges: the fields
        // before them alone.
        let bare = LineReport {
            queue: None,
            ..busy_line()
        };
        let bare = logged(|| emit(sid(), &bare));
        assert!(bare.contains("acked_bytes=3012345"), "{bare:?}");
        for gone in [
            "ack_ms",
            "standing_ms",
            "queued",
            "inflight",
            "cwnd",
            "ssthresh",
            "rwnd_low",
            "pending_high",
            "loss",
        ] {
            assert!(!bare.contains(gone), "{gone:?} is in {bare:?}");
        }
        // Its cost: at most one line a pump every LINE_EVERY, 360 a session
        // hour. A busy line has no field None.
        let per_hour = 3_600 / LINE_EVERY.as_secs();
        for (what, l) in [("busy", &out), ("idle", &idle), ("bare", &bare)] {
            println!(
                "delivery line, {what}: {} bytes; {per_hour} lines and {} bytes a session hour",
                l.len(),
                l.len() as u64 * per_hour
            );
            assert!(l.len() < 512, "{} bytes: {l:?}", l.len());
        }
    }

    /// FR-148 D46 B1 — a window of the in-flight budget, made-up values.
    fn budget_window() -> WindowReport {
        WindowReport {
            held: (212, 300),
            budget_max: 61_440,
            out_max: 1_048_576,
            rate_bps: 1_987_000,
            least_ms: Some(62),
            slack_ms: 150,
            ack_over_ms: Some((3, 45, 1_257)),
            still: (1, 812),
            bounds: (0, 37, 4),
            released: (2, 1),
            still_admitted: 23,
            acted: (140, 300),
            acted_stalled: 12,
        }
    }

    /// The budget's fields close the line, after every field before them, so
    /// the field reads of `standing_ms=` and `queued=` hold; with its switch
    /// off the line has none of them.
    #[test]
    fn the_inflight_budget_s_fields_close_the_line() {
        let budget = [
            "budget_held=(212, 300)",
            "budget_max=61440",
            "budget_out_max=1048576",
            "budget_rate_bps=1987000",
            "budget_least_ms=Some(62)",
            "budget_slack_ms=150",
            "budget_ack_over_ms=Some((3, 45, 1257))",
            "budget_still=(1, 812)",
            "bound_watermark=0",
            "bound_ledger=37",
            "bound_full=4",
            "budget_released=(2, 1)",
            "budget_still_admitted=23",
            "budget_acted=(140, 300)",
            "budget_acted_stalled=12",
        ];
        let line = LineReport {
            budget: Some(budget_window()),
            ..busy_line()
        };
        let out = logged(|| emit(sid(), &line));
        let mut rest = out.as_str();
        for want in ["standing_ms=Some((3, 1257))", "queued=2", "loss=(0, 1)"]
            .into_iter()
            .chain(budget)
        {
            let at = rest
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} is not in order in {out:?}"));
            rest = &rest[at + want.len()..];
        }
        assert_eq!(out.lines().count(), 1, "{out:?}");
        // Without the gauges' fields, the budget's follow the bytes.
        let bare = LineReport {
            queue: None,
            ..line.clone()
        };
        let bare = logged(|| emit(sid(), &bare));
        let mut rest = bare.as_str();
        for want in ["acked_bytes=3012345"].into_iter().chain(budget) {
            let at = rest
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} is not in order in {bare:?}"));
            rest = &rest[at + want.len()..];
        }
        assert!(!bare.contains("standing_ms"), "{bare:?}");
        // No field of the budget's is one a field read greps for.
        for field in budget {
            assert!(!field.contains("standing_ms=") && !field.contains("queued="));
        }
        // Off: not one of them.
        let off = logged(|| emit(sid(), &busy_line()));
        assert!(
            !off.contains("budget_") && !off.contains("bound_"),
            "{off:?}"
        );
        println!("delivery line with the budget: {} bytes", out.len());
        assert!(out.len() < 1_024, "{} bytes: {out:?}", out.len());
    }

    /// FR-148 D46 B3 — `on` no longer runs as `shadow`: no warning where a
    /// pump reads the switch; the pump says once which mode acted on its
    /// session, and again when a change of transport changes it; `shadow`
    /// says nothing. A sampler that is off carries no budget, `on` or not.
    #[test]
    fn the_inflight_budget_on_says_once_which_mode_acted() {
        use crate::media_inflight::Mode as BudgetMode;
        let mut pumps = Vec::new();
        let out = logged(|| {
            pumps.push(Shadow::for_pump(BudgetMode::On, true).is_some_and(|s| s.is_on()));
            pumps.push(Shadow::for_pump(BudgetMode::Shadow, true).is_some_and(|s| !s.is_on()));
            pumps.push(Shadow::for_pump(BudgetMode::Off, true).is_none());
        });
        assert_eq!(pumps, [true, true, true]);
        assert!(out.is_empty(), "{out:?}");
        let t0 = Instant::now();
        let nothing = Gauges {
            max_payload: 1_200,
            ..Gauges::default()
        };
        let mut on = DeliverySampler::new(true).with_budget_on();
        let mut shadow = DeliverySampler::new(true).with_budget();
        let said = logged(|| {
            // A relay session, then a direct one, then a relay again.
            for (frame, relay) in (0..30u64).map(|f| (f, !(10..20).contains(&f))) {
                for s in [&mut on, &mut shadow] {
                    s.gate_reading(at(t0, frame * 33), 0, nothing, 2_000_000, relay, false);
                }
            }
        });
        assert_eq!(said.lines().count(), 3, "{said:?}");
        assert_eq!(said.matches(" INFO ").count(), 3, "{said:?}");
        assert_eq!(
            said.matches("the in-flight budget acts on this relay session")
                .count(),
            2,
            "{said:?}"
        );
        assert_eq!(
            said.matches("runs as shadow on this direct session")
                .count(),
            1,
            "{said:?}"
        );
        assert_eq!(said.matches("budget_acts=\"on\"").count(), 2, "{said:?}");
        let off = logged(|| assert!(Shadow::for_pump(BudgetMode::On, false).is_none()));
        assert!(off.contains("the in-flight budget stays off"), "{off:?}");
        for s in [
            DeliverySampler::new(false).with_budget(),
            DeliverySampler::new(false).with_budget_on(),
        ] {
            let mut s = s;
            assert!(s.budget_mut().is_none());
        }
    }

    /// FR-148 D46 B1 — shadow acts on nothing. Over a recorded sequence of a
    /// deep queue's readings, in which the budget would hold most frames, a
    /// sampler with the budget in shadow answers every call a pump makes as
    /// one with it off does: the same samples, the same lines but for the
    /// budget's own field, the same switch. The calls that feed the budget
    /// return nothing, so its verdict can reach no gate, no AIMD call and no
    /// keyframe request: it reaches the line's counters, which show it held.
    #[test]
    fn the_inflight_budget_in_shadow_answers_every_pump_call_as_off_does() {
        let t0 = Instant::now();
        let mut off = DeliverySampler::new(true);
        let mut shadow = DeliverySampler::new(true).with_budget();
        let mut held = 0;
        let mut lines = 0;
        // 30 s at 30 fps of TURN over TCP: 2 Mbit/s acknowledged while a
        // megabyte stands in flight, the delay growing with it.
        for frame in 0..900u64 {
            let now = at(t0, frame * 1_000 / 30);
            let acked = frame * 250_000 / 30;
            let g = Gauges {
                max_payload: 1_200,
                cwnd: 1_048_576,
                ssthresh: 1_048_576,
                rwnd: 131_072,
                inflight_bytes: 1_000_000,
                pending_bytes: 131_072,
                ack_delay: Some(Duration::from_millis(60 + frame)),
                ack_delay_min: Some(Duration::from_millis(60 + frame / 2)),
                ..Gauges::default()
            };
            for s in [&mut off, &mut shadow] {
                s.on_frame_reading(now, acked, g, 2_000_000);
                s.note_gate(frame % 7 == 0, frame % 11 == 0);
                if frame % 13 == 0 {
                    s.note_watermark();
                }
                if frame % 17 == 0 {
                    s.note_full();
                }
                s.on_handed(8_000);
                let count = s.pump_count();
                let mut chunk = count.unsent(8_000);
                chunk.sent(8_000);
            }
            assert_eq!(off.mode(), shadow.mode());
            // The heartbeat's reading, once a second.
            if frame % 30 == 0 {
                let reading = Delivery {
                    acked_bytes: acked,
                    queue_fills: frame / 30 + 1,
                    queue_empties: 0,
                };
                let a = off.on_reading_with(now, reading, || Some(g));
                let b = shadow.on_reading_with(now, reading, || Some(g));
                assert_eq!(a, b, "frame {frame}");
                let (a, b) = (off.take_line(now), shadow.take_line(now));
                assert_eq!(a.is_some(), b.is_some(), "frame {frame}");
                if let (Some(a), Some(b)) = (a, b) {
                    lines += 1;
                    assert_eq!(a.budget, None);
                    let window = b.budget.expect("the budget's window");
                    held += window.held.0;
                    assert_eq!(LineReport { budget: None, ..b }, a, "frame {frame}");
                }
            }
        }
        let (a, b) = (off.take_rest(), shadow.take_rest());
        assert_eq!(a.map(|a| a.span_ms), b.map(|b| b.span_ms));
        assert!(lines >= 2, "{lines} lines");
        assert!(held > 0, "the budget never held on a megabyte out");
    }

    /// FR-148 D46 B3 — frame `frame` at 30 fps of TURN over TCP, as in
    /// `the_inflight_budget_in_shadow_answers_every_pump_call_as_off_does`: 2
    /// Mbit/s acknowledged while a megabyte stands in flight. The budget holds
    /// each.
    fn deep_frame(frame: u64) -> (u64, Gauges) {
        let g = Gauges {
            max_payload: 1_200,
            cwnd: 1_048_576,
            ssthresh: 1_048_576,
            rwnd: 131_072,
            inflight_bytes: 1_000_000,
            pending_bytes: 131_072,
            ack_delay: Some(Duration::from_millis(60 + frame)),
            ack_delay_min: Some(Duration::from_millis(60 + frame / 2)),
            ..Gauges::default()
        };
        (frame * 250_000 / 30, g)
    }

    /// As [`deep_frame`], but every third frame finds 10 000 bytes in flight
    /// and nothing pending, under the budget: it admits.
    fn deep_or_drained(frame: u64) -> (u64, Gauges) {
        let (acked, mut g) = deep_frame(frame);
        if frame.is_multiple_of(3) {
            g.inflight_bytes = 10_000;
            g.pending_bytes = 0;
        }
        (acked, g)
    }

    /// One pass's facts at a pump's capacity gate: the session on a relay,
    /// today's byte gate, a full send channel, FR-35's opener grace open, and
    /// the encoded frame over the VP9-444 watermark.
    #[derive(Debug, Clone, Copy, Default)]
    struct Facts {
        relay: bool,
        byte_gate: bool,
        full: bool,
        grace: bool,
        over: bool,
    }

    /// What the passes did: skipped at the gate, with the decreases their
    /// skips called; marked a stall for the governor at the gate; shed at the
    /// watermark arm; or went to the send channel.
    #[derive(Debug, Default, PartialEq, Eq)]
    struct Passes {
        skipped: u64,
        decreases: u64,
        marked: u64,
        shed: u64,
        went: u64,
    }

    /// One pass through a sampler as both pumps run it from B3
    /// (`peer.rs`): the budget's gate, the capacity gate with the budget's
    /// term last, the skip's decrease through it (no keyframe asked), or the
    /// step after the gate, the watermark arm through it, and the frame
    /// handed.
    fn pass(
        s: &mut DeliverySampler,
        now: Instant,
        (acked, g): (u64, Gauges),
        f: Facts,
        n: &mut Passes,
    ) {
        n.marked += u64::from(s.gate_reading(now, acked, g, 2_000_000, f.relay, f.byte_gate));
        if f.full || s.shuts() {
            n.skipped += 1;
            s.note_gate(f.full, f.byte_gate);
            let _ = s.on_skip(f.grace, |_| {
                n.decreases += 1;
                Some(())
            });
            return;
        }
        s.on_frame_reading(now, acked, g, 2_000_000);
        handed(s, n, f.over);
    }

    /// One pass as both pumps ran it at B2 (`8df1957e1`): the capacity gate
    /// with today's terms, the skip's decrease outside the grace, the step
    /// after the gate, the watermark arm, and the frame handed.
    fn pass_at_b2(
        s: &mut DeliverySampler,
        now: Instant,
        (acked, g): (u64, Gauges),
        f: Facts,
        n: &mut Passes,
    ) {
        if f.full || f.byte_gate {
            n.skipped += 1;
            s.note_gate(f.full, f.byte_gate);
            n.decreases += u64::from(!f.grace);
            return;
        }
        s.on_frame_reading(now, acked, g, 2_000_000);
        let over = f.over;
        if over {
            n.shed += 1;
            s.note_watermark();
            return;
        }
        hand(s, n);
    }

    /// The watermark arm through the budget's pass, then the frame handed.
    fn handed(s: &mut DeliverySampler, n: &mut Passes, over: bool) {
        if s.sheds(over) {
            n.shed += 1;
            s.note_watermark();
            return;
        }
        hand(s, n);
    }

    fn hand(s: &mut DeliverySampler, n: &mut Passes) {
        n.went += 1;
        s.on_handed(8_000);
        let count = s.pump_count();
        let mut chunk = count.unsent(8_000);
        chunk.sent(8_000);
    }

    /// The heartbeat's reading once a second and the line when it is due, as
    /// the protected shadow test takes them.
    fn heartbeat_at(s: &mut DeliverySampler, frame: u64, now: Instant) -> Option<LineReport> {
        let (acked, g) = deep_frame(frame);
        let reading = Delivery {
            acked_bytes: acked,
            queue_fills: frame / 30 + 1,
            queue_empties: 0,
        };
        let _ = s.on_reading_with(now, reading, || Some(g));
        s.take_line(now)
    }

    /// FR-148 D46 B3 — the hold. Under `on`, on a relay session, over a deep
    /// queue's readings: each pass the budget holds skips before capture with
    /// one decrease call (FR-35's grace closed) and no keyframe asked, the
    /// watermark arm (which asks one) sheds no frame that went, and the line
    /// counts the holds that acted beside the passes the budget was asked
    /// about, and the watermark's firings.
    #[test]
    fn under_on_the_budget_s_hold_skips_a_relay_pass_and_decreases_once_per_hold() {
        let t0 = Instant::now();
        let mut s = DeliverySampler::new(true).with_budget_on();
        let mut n = Passes::default();
        let relay = Facts {
            relay: true,
            over: true,
            ..Facts::default()
        };
        for frame in 0..300u64 {
            pass(
                &mut s,
                at(t0, frame * 1_000 / 30),
                deep_or_drained(frame),
                relay,
                &mut n,
            );
        }
        let w = s.budget_mut().unwrap().take_window();
        assert_eq!(
            n,
            Passes {
                skipped: 200,
                decreases: 200,
                went: 100,
                ..Passes::default()
            }
        );
        assert_eq!((w.acted, w.acted_stalled), ((200, 300), 0), "{w:?}");
        assert_eq!(w.held, (200, 300), "every hold of the core's acted");
        assert_eq!(w.bounds, (100, 0, 0), "the watermark's firings, counted");
    }

    /// FR-148 D46 B3 — the no-decrease rule through the sampler. Under `on`, on
    /// a relay session, the clock stands still for a second with the megabyte
    /// out: each hold skips its pass, and from the least and S on (210 ms)
    /// none calls a decrease; the gate marks the stall for the governor at
    /// those steps, as the shadow's step does after the gate; the line counts
    /// the stalled holds apart.
    #[test]
    fn under_on_a_stall_hold_calls_no_decrease_and_marks_the_stall() {
        let t0 = Instant::now();
        let mut s = DeliverySampler::new(true).with_budget_on();
        let mut n = Passes::default();
        let relay = Facts {
            relay: true,
            ..Facts::default()
        };
        for frame in 0..60u64 {
            pass(
                &mut s,
                at(t0, frame * 1_000 / 30),
                deep_frame(frame),
                relay,
                &mut n,
            );
        }
        assert_eq!((n.skipped, n.decreases, n.marked), (60, 60, 0), "{n:?}");
        // From 2 s the clock stands at its count then.
        let (stood, _) = deep_frame(60);
        for frame in 60..90u64 {
            let (_, g) = deep_frame(frame);
            pass(
                &mut s,
                at(t0, frame * 1_000 / 30),
                (stood, g),
                relay,
                &mut n,
            );
        }
        // Frames 60 to 66 hold before the span is 210 ms old, 67 to 89 in it.
        assert_eq!((n.skipped, n.decreases, n.marked), (90, 67, 23), "{n:?}");
        assert_eq!((n.shed, n.went), (0, 0));
        let w = s.budget_mut().unwrap().take_window();
        assert_eq!((w.acted, w.acted_stalled), ((90, 90), 23), "{w:?}");
    }

    /// FR-148 D46 B3 — a direct session under `on` computes the budget as
    /// shadow does, beside its own gates: through the same passes as a shadow
    /// sampler, the byte gate shuts what it shuts, the watermark sheds what it
    /// sheds, every skip decreases, and the two write the same lines, the
    /// budget's fields included.
    #[test]
    fn under_on_a_direct_session_runs_as_shadow() {
        let t0 = Instant::now();
        let mut on = DeliverySampler::new(true).with_budget_on();
        let mut shadow = DeliverySampler::new(true).with_budget();
        let (mut a, mut b) = (Passes::default(), Passes::default());
        let mut lines = 0;
        for frame in 0..900u64 {
            let now = at(t0, frame * 1_000 / 30);
            let direct = Facts {
                byte_gate: frame % 11 == 0,
                full: frame % 7 == 0,
                over: frame % 13 == 0,
                ..Facts::default()
            };
            pass(&mut on, now, deep_or_drained(frame), direct, &mut a);
            pass(&mut shadow, now, deep_or_drained(frame), direct, &mut b);
            if frame % 30 == 0 {
                let (x, y) = (
                    heartbeat_at(&mut on, frame, now),
                    heartbeat_at(&mut shadow, frame, now),
                );
                assert_eq!(x, y, "frame {frame}");
                lines += usize::from(x.is_some());
            }
        }
        assert_eq!(a, b);
        assert!(a.skipped > 0 && a.shed > 0 && a.went > 0, "{a:?}");
        assert_eq!(a.decreases, a.skipped, "every skip decreases");
        assert!(lines >= 2, "{lines} lines");
        let (x, y) = (on.take_rest(), shadow.take_rest());
        assert_eq!(x, y);
    }

    /// FR-148 D46 B3 — `off` and `shadow` act as today. Through the same
    /// passes, a sampler driven with B3's calls (the budget's gate, its term
    /// in the capacity gate, the skip's decrease and the watermark arm through
    /// it) does what one driven as the pumps were at B2 does: the byte gate
    /// shuts what it shut, the watermark sheds what it shed, each skip outside
    /// the grace decreases, and the lines are the same, the budget's fields
    /// included. On a relay session and on a direct one.
    #[test]
    fn off_and_shadow_gate_as_today() {
        let t0 = Instant::now();
        for shadowed in [false, true] {
            let mk = || {
                let s = DeliverySampler::new(true);
                if shadowed { s.with_budget() } else { s }
            };
            let (mut b3, mut b2) = (mk(), mk());
            let (mut x, mut y) = (Passes::default(), Passes::default());
            let mut lines = 0;
            for frame in 0..900u64 {
                let now = at(t0, frame * 1_000 / 30);
                let f = Facts {
                    relay: frame % 5 != 0,
                    byte_gate: frame % 3 == 1,
                    full: frame % 7 == 0,
                    grace: frame < 60,
                    over: frame % 13 == 0,
                };
                pass(&mut b3, now, deep_or_drained(frame), f, &mut x);
                pass_at_b2(&mut b2, now, deep_or_drained(frame), f, &mut y);
                if frame % 30 == 0 {
                    let (p, q) = (
                        heartbeat_at(&mut b3, frame, now),
                        heartbeat_at(&mut b2, frame, now),
                    );
                    assert_eq!(p, q, "shadow {shadowed}, frame {frame}");
                    lines += usize::from(p.is_some());
                }
            }
            assert_eq!(x, y, "shadow {shadowed}");
            assert!(x.skipped > 0 && x.shed > 0 && x.went > 0, "{x:?}");
            assert!(x.decreases < x.skipped, "the grace kept some: {x:?}");
            assert!(lines >= 2, "{lines} lines");
            assert_eq!(b3.take_rest(), b2.take_rest(), "shadow {shadowed}");
        }
    }

    /// FR-148 D46 B3 — today's two bounds: out under `on` on a relay session,
    /// in under `off`, `shadow` and `on` on a direct one. A pass with nothing
    /// out and the byte gate shut goes only under `on` on a relay, and a frame
    /// over the watermark is shed everywhere else. Where a line is written,
    /// each bound's firing is counted on it.
    #[test]
    fn the_byte_gate_and_the_watermark_are_out_under_on_and_in_under_off_and_shadow() {
        let t0 = Instant::now();
        let nothing = (
            0,
            Gauges {
                max_payload: 1_200,
                ..Gauges::default()
            },
        );
        let today = Passes {
            skipped: 1,
            decreases: 1,
            shed: 1,
            ..Passes::default()
        };
        for (what, s, relay, want) in [
            ("off", DeliverySampler::new(true), true, &today),
            (
                "shadow",
                DeliverySampler::new(true).with_budget(),
                true,
                &today,
            ),
            (
                "on, direct",
                DeliverySampler::new(true).with_budget_on(),
                false,
                &today,
            ),
            (
                "on, relay",
                DeliverySampler::new(true).with_budget_on(),
                true,
                &Passes {
                    went: 2,
                    ..Passes::default()
                },
            ),
        ] {
            let mut s = s;
            let mut n = Passes::default();
            let shut = Facts {
                relay,
                byte_gate: true,
                ..Facts::default()
            };
            let over = Facts {
                relay,
                over: true,
                ..Facts::default()
            };
            pass(&mut s, t0, nothing, shut, &mut n);
            pass(&mut s, at(t0, 33), nothing, over, &mut n);
            assert_eq!(&n, want, "{what}");
            let bounds = s.budget_mut().map(|b| {
                let w = b.take_window();
                (w.bounds.0, w.bounds.1)
            });
            let counted = (what != "off").then_some((1, 1));
            assert_eq!(
                bounds, counted,
                "{what}: the watermark's and the byte gate's firings"
            );
        }
    }

    /// FR-148 D46 B3 — FR-35's opener grace under `on`: on a relay session the
    /// budget's holds inside the grace skip their passes and call no decrease,
    /// as today's skips there do; once the grace is over each hold decreases
    /// once. The pumps hand the grace to the same call (`peer.rs`, pinned by
    /// `both_pumps_gate_on_the_budget_s_pass`).
    #[test]
    fn fr35_s_opener_grace_keeps_the_aimd_through_the_budget_s_holds_under_on() {
        let t0 = Instant::now();
        let mut s = DeliverySampler::new(true).with_budget_on();
        let mut n = Passes::default();
        let grace = Facts {
            relay: true,
            grace: true,
            ..Facts::default()
        };
        for frame in 0..60u64 {
            pass(
                &mut s,
                at(t0, frame * 1_000 / 30),
                deep_frame(frame),
                grace,
                &mut n,
            );
        }
        assert_eq!((n.skipped, n.decreases), (60, 0), "inside the grace: {n:?}");
        let after = Facts {
            grace: false,
            ..grace
        };
        for frame in 60..120u64 {
            pass(
                &mut s,
                at(t0, frame * 1_000 / 30),
                deep_frame(frame),
                after,
                &mut n,
            );
        }
        assert_eq!((n.skipped, n.decreases), (120, 60), "after it: {n:?}");
        let w = s.budget_mut().unwrap().take_window();
        assert_eq!(w.acted, (120, 120));
    }

    /// FR-148 D46 B3 — FR-70's `plan_divergences` read as today under `on`.
    /// The FFmpeg pump hands its plan check today's relay byte gate, computed
    /// as before and before the budget's gate, and the budget takes that gate
    /// by value. Over passes where the budget holds while the ledger's gate is
    /// open and admits while it is shut, a plan shadow beside an `on` sampler
    /// counts what one beside an `off` sampler counts: none. A pump that
    /// handed the plan the budget's answer would count a divergence at every
    /// such pass. The pump's text is pinned by
    /// `both_pumps_gate_on_the_budget_s_pass`.
    #[test]
    fn plan_divergences_read_today_s_byte_gate_under_on() {
        use crate::encode::plan::{PlanDivergences, PlanInputs, PlanKnobs, Shadow as PlanShadow};
        use crate::encode::policy::DimsInputs;
        let knobs = PlanKnobs {
            relay_max_bps: 3_000_000,
            constrained_queue_ms: 450,
            constrained_queue_measured: true,
            direct_queue_ms: 150,
            direct_hrd_pct: 100,
            refined_cap: None,
            priority_caps: Default::default(),
        };
        let inputs = PlanInputs {
            constrained: true,
            target_fps: 30,
            paced_fps: None,
            pipe_bps: None,
            ceiling_reference_bps: 3_000_000,
            hrd_pct: 200,
            target_bps: 0,
            dims: DimsInputs {
                native_w: 1920,
                native_h: 1200,
                merged_target: crate::peer::TargetResolution::Native,
                merged_priority_cap: None,
                slow_link_cap: None,
                refined: false,
                refined_cap: None,
                soft_cap: None,
            },
        };
        let mut beside_off = PlanShadow::default();
        let mut beside_on = PlanShadow::default();
        let mut fed_the_budget = PlanShadow::default();
        let budget = beside_off.produce(&inputs, &knobs).queue_budget_bytes;
        beside_on.produce(&inputs, &knobs);
        fed_the_budget.produce(&inputs, &knobs);
        let mut off = DeliverySampler::new(true);
        let mut on = DeliverySampler::new(true).with_budget_on();
        let t0 = Instant::now();
        let mut differ = 0;
        for frame in 0..300u64 {
            let now = at(t0, frame * 1_000 / 30);
            // The ledger under its budget, then at twice it: the pump's relay
            // gate is `inflight_now >= queue_budget`.
            let inflight = if frame % 2 == 0 {
                budget / 2
            } else {
                budget * 2
            };
            let byte_gate = inflight >= budget;
            let _ = beside_off.check_gate_verdict(inflight, 0.0, byte_gate);
            let _ = beside_on.check_gate_verdict(inflight, 0.0, byte_gate);
            // The budget holds where the ledger's gate is open, and has
            // nothing out where it is shut.
            let (acked, mut g) = deep_frame(frame);
            if byte_gate {
                (g.inflight_bytes, g.pending_bytes) = (0, 0);
            }
            off.gate_reading(now, acked, g, 2_000_000, true, byte_gate);
            on.gate_reading(now, acked, g, 2_000_000, true, byte_gate);
            assert_eq!(off.shuts(), byte_gate, "frame {frame}");
            let acted = on.shuts();
            differ += u32::from(acted != byte_gate);
            let _ = fed_the_budget.check_gate_verdict(inflight, 0.0, acted);
        }
        assert_eq!(beside_off.counts(), PlanDivergences::default());
        assert_eq!(beside_on.counts(), beside_off.counts());
        assert_eq!(differ, 300, "the budget answered otherwise at every pass");
        assert_eq!(fed_the_budget.counts().gate, differ);
    }

    /// FR-148 D46 B3 — both pumps act on the budget's pass, read from
    /// `peer.rs` as compiled: the budget's gate comes before the capacity
    /// gate decides; its term is the gate's last, after the send channel's and
    /// the followers'; the arm's decrease goes through it and FR-35's grace,
    /// and the arm asks for no keyframe; the step after the gate is there as
    /// before; and the VP9-444 watermark arm goes through it. The FFmpeg pump
    /// hands FR-70's plan check today's byte gate before the budget's gate,
    /// and never sets it again.
    #[test]
    fn both_pumps_gate_on_the_budget_s_pass() {
        const PEER_RS: &str = include_str!("peer.rs");
        const GATE: &str =
            "if send_tx.capacity() == 0 || pipeline.followers_congested() || delivery.shuts() {";
        const STEP: &str = "delivery.on_frame(&pc, &mut governor);";
        let src = PEER_RS.replace("\r\n", "\n");
        let pump = |head: &str| {
            assert_eq!(src.matches(head).count(), 1, "one {head:?} in peer.rs");
            let rest = &src[src.find(head).unwrap()..];
            rest[..rest.find("\n}\n").expect("the pump's end")].to_owned()
        };
        let vp9 = pump("async fn media_pump_vp9_444_dc(");
        let ffmpeg = pump("async fn media_pump_ffmpeg_dc(");
        for (name, body, gate, skip) in [
            (
                "VP9-444",
                &vp9,
                "delivery.gate(&pc, &mut governor, session_id, constrained_transport, false);",
                "delivery.on_skip(false, |t| governor.on_backpressure_skip(t))",
            ),
            (
                "FFmpeg",
                &ffmpeg,
                "delivery.gate(&pc, &mut governor, session_id, constrained, byte_gate);",
                "delivery.on_skip(in_grace, |t| governor.on_backpressure_skip(t))",
            ),
        ] {
            assert_eq!(body.matches("delivery.gate(").count(), 1, "{name}");
            assert_eq!(body.matches(gate).count(), 1, "{name}: {gate}");
            assert_eq!(body.matches(GATE).count(), 1, "{name}: the capacity gate");
            let at = body.find(GATE).unwrap();
            assert!(
                body.find(gate).unwrap() < at,
                "{name}: the budget's gate first"
            );
            let arm = &body[at..];
            let arm = &arm[..arm.find("continue;").expect("the arm's continue")];
            assert!(
                arm.contains(skip),
                "{name}: the decrease through the budget"
            );
            assert!(
                !arm.contains("keyframe_requested") && !arm.contains("note_keyframe"),
                "{name}: the arm asks no keyframe"
            );
            assert_eq!(
                body.matches("on_backpressure_skip(").count(),
                1,
                "{name}: one decrease at the gate, the budget's"
            );
            assert!(body[at..].contains(STEP), "{name}: the step after the gate");
        }
        assert!(vp9.contains("if delivery.sheds(buffered > dc_buffered_high) {"));
        assert!(ffmpeg.contains("let in_grace = constrained && !opener_grace_done;"));
        let check = "plan_shadow.check_gate_verdict(inflight_now, measured_wait_ms, byte_gate)";
        assert_eq!(
            ffmpeg.matches(check).count(),
            1,
            "FR-70's check of today's gate"
        );
        assert!(ffmpeg.find(check).unwrap() < ffmpeg.find("delivery.gate(").unwrap());
        assert_eq!(
            ffmpeg
                .matches(
                    "let byte_gate = if constrained {\n            inflight_now >= queue_budget"
                )
                .count(),
            1,
            "today's relay byte gate, computed as before"
        );
        assert_eq!(
            ffmpeg.matches("byte_gate =").count(),
            1,
            "set once, at its let"
        );
    }

    /// FR-148 D46 B2 — a sample read with gauges: `standing_ms` over the
    /// session's least (`None`: it does not count), 2 s long.
    fn read_sample(class: Class, kbps: u64, standing_ms: Option<u64>) -> Sample {
        Sample {
            ms: 2_000,
            bytes: kbps * 1_000 / 8 * 2,
            class,
            queue: Some(QueueReading {
                least_ms: Some(60 + standing_ms.unwrap_or(0)),
                standing_ms,
                gauges: Gauges::default(),
                loss: (0, 0),
            }),
        }
    }

    fn pushed(by: PushedBy, kbps: u32) -> Option<SamplePushBack> {
        Some(SamplePushBack {
            bps: kbps * 1_000,
            by,
            reread: false,
        })
    }

    /// FR-148 D46 B2 (P1) — a `queued` sample is a capacity at its own rate
    /// whatever the budget's setting: off (no `Over`), or on or in shadow
    /// with nothing seen over it. Its class does not matter: the queue stood
    /// on the path.
    #[test]
    fn a_queued_sample_pushes_back_at_its_own_rate_whatever_the_budget() {
        let quiet = Over::default();
        for class in [Class::Backlogged, Class::AppLimited] {
            let s = read_sample(class, 1_500, Some(340));
            assert_eq!(push_back_of(&s, None, BURST), pushed(PushedBy::P1, 1_500));
            assert_eq!(
                push_back_of(&s, Some(&quiet), BURST),
                pushed(PushedBy::P1, 1_500)
            );
        }
        // A sample read without gauges has no queue reading: never one.
        let bare = Sample {
            queue: None,
            ..read_sample(Class::Backlogged, 1_500, Some(340))
        };
        assert_eq!(push_back_of(&bare, Some(&quiet), BURST), None);
    }

    /// FR-148 D46 B2 (P3) — a counted, backlogged sample with no still span
    /// in it is a capacity with the budget on or in shadow, and nothing with
    /// it off: SCTP's `Backlogged` class alone feeds nothing then. Under the
    /// budget a sample whose SCTP queue ran empty is backlogged too when the
    /// verdicts held a frame in a share of each of its seconds.
    #[test]
    fn a_backlogged_sample_with_no_still_span_pushes_back_with_the_budget_and_not_without() {
        let quiet = Over::default();
        let held = Over {
            held: true,
            ..quiet
        };
        let backlogged = read_sample(Class::Backlogged, 1_900, Some(40));
        assert_eq!(
            push_back_of(&backlogged, Some(&quiet), BURST),
            pushed(PushedBy::P3, 1_900)
        );
        assert_eq!(push_back_of(&backlogged, None, BURST), None, "off: P1 only");
        let emptied = read_sample(Class::AppLimited, 1_900, Some(40));
        assert_eq!(
            push_back_of(&emptied, Some(&held), BURST),
            pushed(PushedBy::P3, 1_900)
        );
        assert_eq!(push_back_of(&emptied, Some(&quiet), BURST), None);
        assert_eq!(push_back_of(&emptied, None, BURST), None);
        // A sample that does not count has no reading of its own queue.
        let uncounted = read_sample(Class::Backlogged, 1_900, None);
        assert_eq!(push_back_of(&uncounted, Some(&held), BURST), None);
    }

    /// FR-148 D46 B2 — a sample a still span of the least and S stood in is
    /// no P3 capacity, under the rule and under each rule measured beside it.
    #[test]
    fn a_sample_with_a_still_span_of_least_and_s_in_it_is_no_p3_capacity() {
        let stood = Over {
            stood: true,
            still_inside: true,
            held: true,
            released_from: None,
            opening: false,
            ..Over::default()
        };
        let s = read_sample(Class::Backlogged, 1_200, Some(40));
        for burst in [Burst::Exclude, Burst::Stood, Burst::Correct, Burst::Own] {
            assert_eq!(push_back_of(&s, Some(&stood), burst), None, "{burst:?}");
        }
    }

    /// FR-148 D46 B2 — the burst rule. A queued sample that starts inside a
    /// released still span and holds its release reads the burst: 2.6 Mbit/s
    /// on a 2 Mbit/s link. The rule makes it no capacity; with no rule it is
    /// P1 at the burst's rate, and the classifier's correction counts it from
    /// the span's first still reading, 0.8 s earlier with 0.2 MB back since.
    #[test]
    fn a_sample_that_holds_a_releases_burst_is_no_capacity() {
        let burst = Over {
            stood: true,
            still_inside: false,
            held: false,
            released_from: Some((Duration::from_millis(800), 200_000)),
            opening: false,
            ..Over::default()
        };
        let s = read_sample(Class::Backlogged, 2_600, Some(300));
        assert_eq!(push_back_of(&s, Some(&burst), Burst::Exclude), None);
        assert_eq!(push_back_of(&s, Some(&burst), Burst::Stood), None);
        assert_eq!(
            push_back_of(&s, Some(&burst), Burst::Own),
            pushed(PushedBy::P1, 2_600)
        );
        // (650 000 + 200 000) bytes over 2.8 s.
        assert_eq!(
            push_back_of(&s, Some(&burst), Burst::Correct).map(|p| p.bps),
            Some(2_428_571)
        );
        // Off, the budget sees no span: P1 reads what it reads, and the model
        // never lets a push-back raise its floor (`pipe::tests`).
        assert_eq!(push_back_of(&s, None, BURST), pushed(PushedBy::P1, 2_600));
    }

    /// FR-148 D46 B2 — an application-limited sample (SCTP's queue ran
    /// empty, no standing queue, not held in each second) is no push-back
    /// under any setting of the budget, so it never lowers the floor.
    #[test]
    fn an_application_limited_sample_never_lowers_the_floor() {
        let s = read_sample(Class::AppLimited, 300, Some(20));
        for over in [None, Some(Over::default())] {
            assert_eq!(push_back_of(&s, over.as_ref(), BURST), None, "{over:?}");
        }
        let stalled = read_sample(Class::Stalled, 0, Some(20));
        assert_eq!(push_back_of(&stalled, Some(&Over::default()), BURST), None);
        // And through the model: a floor that stood stands.
        use crate::encode::pair_memory::{Read, Take};
        use crate::encode::pipe::Pipe;
        let now = Instant::now();
        let mut pipe = Pipe::default();
        pipe.observe_delivered_in(2_000_000, Basis::AppLimited, true, now);
        for over in [None, Some(Over::default())] {
            if let Some(p) = push_back_of(&s, over.as_ref(), BURST) {
                pipe.observe_sample_capacity(p.bps, p.by, Take::Sample(Read::Clear), now);
            }
        }
        assert_eq!(pipe.model().floor(now).map(|f| f.bps), Some(2_000_000));
    }

    /// FR-148 D48 — `s` with SCTP's fast retransmits and T3-rtx expiries
    /// over it.
    fn lossy(s: Sample, loss: (u64, u64)) -> Sample {
        Sample {
            queue: s.queue.map(|q| QueueReading { loss, ..q }),
            ..s
        }
    }

    /// FR-148 D48 — a P3 that rule 1 re-read at `kbps`.
    fn reread(kbps: u32) -> Option<SamplePushBack> {
        Some(SamplePushBack {
            bps: kbps * 1_000,
            by: PushedBy::P3,
            reread: true,
        })
    }

    /// FR-148 D48, rule 1 — a P3 sample with a loss in it is re-read at the
    /// budget's rate at its close when that is more: its own mean is what
    /// SCTP's recovery let through, and the rate, the most of the same
    /// clock's one-second figures, reads the second the recovery cost least.
    /// A fast retransmit and a T3-rtx expiry alike are a loss. A rate under
    /// its own, or none known, leaves it as read; so does no loss; and P1,
    /// which read its own queue, is never re-read.
    #[test]
    fn a_p3_sample_with_a_loss_in_it_is_re_read_at_the_budget_s_rate() {
        let at = |rate: Option<u64>| Over {
            held: true,
            rate,
            ..Over::default()
        };
        let p3 = read_sample(Class::Backlogged, 1_200, Some(40));
        for loss in [(1, 0), (0, 1), (3, 2)] {
            let s = lossy(p3, loss);
            assert_eq!(
                push_back_of(&s, Some(&at(Some(1_900_000))), BURST),
                reread(1_900),
                "{loss:?}"
            );
            assert_eq!(
                sample_read_of(&s, Some(&at(Some(1_900_000))), BURST),
                reread(1_900).map(SampleRead::Push)
            );
            for under in [Some(1_000_000), Some(1_200_000), None] {
                assert_eq!(
                    push_back_of(&s, Some(&at(under)), BURST),
                    pushed(PushedBy::P3, 1_200),
                    "{loss:?} {under:?}"
                );
            }
        }
        assert_eq!(
            push_back_of(&p3, Some(&at(Some(1_900_000))), BURST),
            pushed(PushedBy::P3, 1_200),
            "no loss"
        );
        let queued = lossy(read_sample(Class::Backlogged, 1_200, Some(340)), (1, 0));
        assert_eq!(
            push_back_of(&queued, Some(&at(Some(1_900_000))), BURST),
            pushed(PushedBy::P1, 1_200)
        );
    }

    /// FR-148 D48, rule 4 — a P3 sample with no loss in it and a hold SCTP's
    /// congestion window sized ([`Over::ramp`]) is kept out: the
    /// association's ramp bound it, not the path. Kept out, it still takes
    /// the sampler's slot ([`SampleRead::Ramp`]), which the `path model`
    /// line names. With a loss in it rule 1 reads it first. A hold the
    /// receive window sized leaves `ramp` unset (`media_inflight::tests`),
    /// and the P3 stands. P1 reads its own queue either way, and with the
    /// budget off no rule reads anything.
    #[test]
    fn a_p3_sample_the_ramp_bound_is_kept_out_and_one_with_a_loss_is_re_read() {
        let ramp = Over {
            held: true,
            ramp: true,
            rate: Some(1_900_000),
            ..Over::default()
        };
        let p3 = read_sample(Class::Backlogged, 1_200, Some(40));
        assert_eq!(
            sample_read_of(&p3, Some(&ramp), BURST),
            Some(SampleRead::Ramp)
        );
        assert_eq!(push_back_of(&p3, Some(&ramp), BURST), None);
        let viewer = Over {
            ramp: false,
            ..ramp
        };
        assert_eq!(
            push_back_of(&p3, Some(&viewer), BURST),
            pushed(PushedBy::P3, 1_200)
        );
        for loss in [(1, 0), (0, 1)] {
            assert_eq!(
                push_back_of(&lossy(p3, loss), Some(&ramp), BURST),
                reread(1_900),
                "{loss:?}"
            );
        }
        // A loss and a rate under its own: as read, not kept out.
        let slow = Over {
            rate: Some(1_000_000),
            ..ramp
        };
        assert_eq!(
            push_back_of(&lossy(p3, (1, 0)), Some(&slow), BURST),
            pushed(PushedBy::P3, 1_200)
        );
        let queued = read_sample(Class::Backlogged, 1_200, Some(340));
        assert_eq!(
            push_back_of(&queued, Some(&ramp), BURST),
            pushed(PushedBy::P1, 1_200)
        );
        assert_eq!(sample_read_of(&p3, None, BURST), None);
        // The opening's is no P3 to rule on.
        let opening = Over {
            opening: true,
            ..ramp
        };
        assert_eq!(sample_read_of(&p3, Some(&opening), BURST), None);
    }

    /// FR-148 D46 B2 — the sampler's own push-back, by what its budget saw: a
    /// backlogged sample on a moving clock is P3 with the budget in shadow
    /// and nothing with it off; the queued one after it is P1 either way.
    /// With the budget off a push-back is due at its close; in shadow it is
    /// due once [`settle`] has passed, at the next reading. The first sample
    /// starts in the budget's opening, before its first measured rate (1 s):
    /// backlogged too, it is no P3 capacity.
    #[test]
    fn the_sampler_pushes_back_by_what_its_budget_saw() {
        let t0 = Instant::now();
        for budget in [false, true] {
            let mut s = DeliverySampler::new(true);
            if budget {
                s = s.with_budget();
            }
            let mut pushed = Vec::new();
            let mut closed = Vec::new();
            // 2 Mbit/s acknowledged for 7 s at 30 fps; the least stands
            // 340 ms over the session's in the third sample.
            for frame in 0..=210u64 {
                let now = at(t0, frame * 1_000 / 30);
                let acked = frame * 250_000 / 30;
                let least = if frame > 120 { 400 } else { 60 };
                let g = Gauges {
                    max_payload: 1_200,
                    cwnd: 1 << 20,
                    rwnd: 1 << 20,
                    inflight_bytes: 50_000,
                    pending_bytes: 1,
                    ack_delay: Some(Duration::from_millis(least)),
                    ack_delay_min: Some(Duration::from_millis(least)),
                    ..Gauges::default()
                };
                s.on_frame_reading(now, acked, g, 2_000_000);
                if frame % 30 == 0 {
                    let reading = Delivery {
                        acked_bytes: acked,
                        queue_fills: 1,
                        queue_empties: 0,
                    };
                    if s.on_reading_with(now, reading, || Some(g)).is_some() {
                        closed.push(frame / 30);
                    }
                    while let Some(p) = s.take_push_back(now) {
                        pushed.push((frame / 30, p.by, p.bps));
                    }
                }
            }
            assert_eq!(closed, [2, 4, 6]);
            let want: Vec<_> = if budget {
                vec![(5, PushedBy::P3, 2_000_000), (7, PushedBy::P1, 2_000_000)]
            } else {
                vec![(6, PushedBy::P1, 2_000_000)]
            };
            assert_eq!(pushed, want, "budget {budget}");
        }
    }

    /// FR-148 D48, rule 4, through the sampler — the run above in shadow,
    /// its least 60 ms throughout, with one frame at 3 s that holds: SCTP's
    /// congestion window of 6 kB over the least carries 800 kbit/s, under
    /// the 2 Mbit/s rate, and 21 kB out holds. The P3 the sample closes into
    /// at 4 s is kept out, its slot handed at 5 s as the ramp's; the next is
    /// P3 again. The receive window sizing the same hold, the P3 stands. A
    /// fast retransmit over the sample reads it by rule 1 first: not kept
    /// out, and at no less than its own rate.
    #[test]
    fn the_sampler_keeps_out_a_p3_the_ramp_bound() {
        let t0 = Instant::now();
        let base = Gauges {
            max_payload: 1_200,
            cwnd: 1 << 20,
            rwnd: 1 << 20,
            inflight_bytes: 50_000,
            pending_bytes: 1,
            ack_delay: Some(Duration::from_millis(60)),
            ack_delay_min: Some(Duration::from_millis(60)),
            ..Gauges::default()
        };
        let held = Gauges {
            inflight_bytes: 6_000,
            pending_bytes: 15_000,
            ..base
        };
        let ramp = Gauges {
            cwnd: 6_000,
            ..held
        };
        let viewer = Gauges { rwnd: 0, ..held };
        let p3 = SampleRead::Push(SamplePushBack {
            bps: 2_000_000,
            by: PushedBy::P3,
            reread: false,
        });
        for (hold, fast) in [(ramp, 0), (viewer, 0), (ramp, 1)] {
            let mut s = DeliverySampler::new(true).with_budget();
            let mut reads = Vec::new();
            for frame in 0..=210u64 {
                let now = at(t0, frame * 1_000 / 30);
                let acked = frame * 250_000 / 30;
                let g = if frame == 90 { hold } else { base };
                let g = Gauges {
                    fast_retransmits: if frame >= 90 { fast } else { 0 },
                    ..g
                };
                s.on_frame_reading(now, acked, g, 2_000_000);
                if frame % 30 == 0 {
                    let reading = Delivery {
                        acked_bytes: acked,
                        queue_fills: 1,
                        queue_empties: 0,
                    };
                    s.on_reading_with(now, reading, || Some(g));
                    while let Some(read) = s.take_read(now) {
                        reads.push((frame / 30, read));
                    }
                }
            }
            if fast > 0 {
                assert!(
                    matches!(
                        reads.as_slice(),
                        [(5, SampleRead::Push(p)), (7, _)]
                            if p.by == PushedBy::P3 && p.bps >= 2_000_000
                    ),
                    "a loss: {reads:?}"
                );
            } else if hold.cwnd == ramp.cwnd {
                assert_eq!(reads, [(5, SampleRead::Ramp), (7, p3)]);
            } else {
                assert_eq!(reads, [(5, p3), (7, p3)], "the viewer's window");
            }
        }
    }

    /// FR-148 D46 B2 — the rule's other half: a backlogged sample that closes
    /// while the clock stands still waits out [`settle`] (60 + 150 + 200 ms
    /// here), and the budget marks the stall inside the wait, the still span
    /// standing the least and S: it reaches nothing. The same sample whose
    /// clock moves again 150 ms into the pause is P3 once the wait is over.
    /// The sample is the session's second: the first starts in the budget's
    /// opening, and is none either way.
    #[test]
    fn a_sample_that_closes_in_a_stall_is_no_capacity() {
        let t0 = Instant::now();
        for (pause_ms, want) in [(600, None), (150, Some(PushedBy::P3))] {
            let mut s = DeliverySampler::new(true).with_budget();
            let mut pushed = Vec::new();
            let mut acked = 0;
            for ms in (0..=5_000u64).step_by(10) {
                let now = at(t0, ms);
                // 2 Mbit/s acknowledged, standing still from 3.95 s.
                let still = (3_950..3_950 + pause_ms).contains(&ms);
                if !still {
                    acked += 2_500;
                }
                let g = Gauges {
                    max_payload: 1_200,
                    cwnd: 1 << 20,
                    rwnd: 1 << 20,
                    inflight_bytes: 50_000,
                    pending_bytes: 1,
                    ack_delay: Some(Duration::from_millis(60)),
                    ack_delay_min: Some(Duration::from_millis(60)),
                    ..Gauges::default()
                };
                s.on_frame_reading(now, acked, g, 2_000_000);
                if ms % 1_000 == 0 {
                    let reading = Delivery {
                        acked_bytes: acked,
                        queue_fills: 1,
                        queue_empties: 0,
                    };
                    s.on_reading_with(now, reading, || Some(g));
                }
                while let Some(p) = s.take_push_back(now) {
                    pushed.push(p.by);
                }
            }
            assert_eq!(pushed.first().copied(), want, "a {pause_ms} ms pause");
        }
    }

    /// FR-148 D46 B2 — gauges that hold at any budget: `out` bytes in flight
    /// past a 1 MiB window with a chunk waiting, so SCTP's window binds no
    /// frame through, and the rate, not the window, sizes the budget.
    fn holding(out: u64) -> Gauges {
        Gauges {
            max_payload: 1_200,
            cwnd: 1 << 20,
            rwnd: 1 << 20,
            inflight_bytes: out,
            pending_bytes: 1,
            ack_delay: Some(Duration::from_millis(60)),
            ack_delay_min: Some(Duration::from_millis(60)),
            ..Gauges::default()
        }
    }

    /// FR-148 D46 B2 — a sampler with the budget in shadow for `secs`
    /// seconds at 30 fps: 2 Mbit/s acknowledged, the opening rate 2 Mbit/s,
    /// `out(ms)` bytes in flight, a reading once a second whose SCTP queue
    /// ran empty (so only the budget's holds make a sample backlogged).
    /// Returns each push-back the heartbeat would hand, and when (ms).
    fn opening_run(secs: u64, out: impl Fn(u64) -> u64) -> Vec<(u64, SamplePushBack)> {
        let t0 = Instant::now();
        let mut s = DeliverySampler::new(true).with_budget();
        let mut pushed = Vec::new();
        for frame in 0..=secs * 30 {
            let ms = frame * 1_000 / 30;
            let now = at(t0, ms);
            let acked = frame * 250_000 / 30;
            let g = holding(out(ms));
            s.on_frame_reading(now, acked, g, 2_000_000);
            if frame % 30 == 0 {
                let reading = Delivery {
                    acked_bytes: acked,
                    queue_fills: frame / 30 + 1,
                    queue_empties: frame / 30 + 1,
                };
                s.on_reading_with(now, reading, || Some(g));
            }
            while let Some(p) = s.take_push_back(now) {
                pushed.push((ms, p));
            }
        }
        pushed
    }

    /// FR-148 D46 B2 — the opening: the budget held in each second of the
    /// session's first 2 s, and in its first second at the opening rate,
    /// which stands in until its first figure (1 s). A sample that starts
    /// there is no P3 capacity; one that starts at the first figure is, and
    /// a queued sample is P1 either way.
    #[test]
    fn a_sample_the_budget_held_at_its_opening_is_no_p3_capacity() {
        use crate::media_inflight::Reading;
        let t0 = Instant::now();
        let mut b = Budget::new();
        for ms in (0..=2_000u64).step_by(10) {
            let r = Reading {
                acked_bytes: ms * 250,
                gauges: holding(10 << 20),
                pump_bytes: 0,
                open_bps: 2_000_000,
            };
            b.step(at(t0, ms), &r);
        }
        let opening = b.over(t0, at(t0, 2_000), 0);
        assert!(opening.held && opening.opening, "{opening:?}");
        let s = read_sample(Class::AppLimited, 2_000, Some(0));
        assert_eq!(push_back_of(&s, Some(&opening), BURST), None);
        let measured = b.over(at(t0, 1_000), at(t0, 2_000), 250_000);
        assert!(measured.held && !measured.opening, "{measured:?}");
        assert_eq!(
            push_back_of(&s, Some(&measured), BURST),
            pushed(PushedBy::P3, 2_000)
        );
        let queued = read_sample(Class::AppLimited, 2_000, Some(340));
        assert_eq!(
            push_back_of(&queued, Some(&opening), BURST),
            pushed(PushedBy::P1, 2_000)
        );
    }

    /// FR-148 D46 B2 — the budget holds at every frame of 5 s: the first
    /// sample (0 to 2 s) starts in its opening and is none; the second (2 to
    /// 4 s), held in each second at the measured rate with no still span, is
    /// P3 at its own rate, handed once its settle has passed.
    #[test]
    fn a_held_sample_at_a_measured_rate_with_no_still_span_is_p3() {
        let p3 = pushed(PushedBy::P3, 2_000).unwrap();
        assert_eq!(opening_run(5, |_| 10 << 20), [(4_433, p3)]);
    }

    /// FR-148 D46 B2 — a session whose budget held only while it opened (the
    /// first 2 s), with one byte out after: no sample's push-back reaches
    /// the shadow model, whose capacity stays absent.
    #[test]
    fn a_session_whose_only_held_sample_is_the_openers_leaves_the_capacity_absent() {
        use crate::encode::pair_memory::{Read, Take};
        use crate::encode::pipe::Pipe;
        let pushed = opening_run(7, |ms| if ms < 2_000 { 10 << 20 } else { 1 });
        let now = Instant::now();
        let mut pipe = Pipe::default();
        pipe.observe_delivered_in(2_000_000, Basis::AppLimited, true, now);
        for (_, p) in &pushed {
            pipe.observe_sample_capacity(p.bps, p.by, Take::Sample(Read::Clear), now);
        }
        assert_eq!(pipe.model().capacity(now), None, "{pushed:?}");
        assert_eq!(pipe.model().floor(now).map(|f| f.bps), Some(2_000_000));
        assert!(pushed.is_empty(), "{pushed:?}");
    }

    /// Gauges as a take reads them: `least` is the least delay since the take
    /// before, ms.
    fn gauges(least: Option<u64>) -> Gauges {
        Gauges {
            max_payload: 1_200,
            cwnd: 131_072,
            ssthresh: 1_048_576,
            rwnd: 1_000_000,
            inflight_bytes: 60_000,
            ack_delay: least.map(Duration::from_millis),
            ack_delay_min: least.map(Duration::from_millis),
            ..Gauges::default()
        }
    }

    /// Feeds samples of (bytes, least ms) 2 s apart, after a reading that
    /// starts the first, and returns what the gauges said over each.
    fn feed(
        s: &mut DeliverySampler,
        t0: Instant,
        samples: &[(u64, Option<u64>)],
    ) -> Vec<QueueReading> {
        assert_eq!(
            s.on_reading_with(t0, r(0, 1, 0), || Some(gauges(None))),
            None
        );
        let mut acked = 0;
        let mut out = Vec::new();
        for (i, &(bytes, least)) in samples.iter().enumerate() {
            acked += bytes;
            let now = at(t0, 2_000 * (i as u64 + 1));
            let sample = s.on_reading_with(now, r(acked, 1, 0), || Some(gauges(least)));
            out.push(
                sample
                    .and_then(|x| x.queue)
                    .expect("a sample read with gauges"),
            );
        }
        out
    }

    /// The byte rule at its edges: in a sample of `ms`, at most `ms / 200 + 2`
    /// held SACKs, each for one packet of one DATA chunk's payload (1,200 bytes
    /// on the vendored SCTP's 1,228-byte packets).
    #[test]
    fn a_sample_counts_past_what_lone_packets_could_carry() {
        // 2 s, the FFmpeg pump's heartbeat: at most 12 packets.
        assert!(!counts(2_000, 14_400, 1_200));
        assert!(counts(2_000, 14_401, 1_200));
        // 1.5 s, the shortest sample: 9.
        assert!(!counts(1_500, 10_800, 1_200));
        assert!(counts(1_500, 10_801, 1_200));
        // A late heartbeat's 6 s: 32.
        assert!(!counts(6_000, 38_400, 1_200));
        assert!(counts(6_000, 38_401, 1_200));
        // No gauges stored: nothing counts.
        assert!(!counts(2_000, u64::MAX, 0));
    }

    /// Where the sampler takes the gauges: where a sample starts and where it
    /// closes, and at no other reading.
    #[test]
    fn the_gauges_are_taken_where_a_sample_starts_and_closes_only() {
        let t0 = Instant::now();
        let takes = Cell::new(0u32);
        let take = || {
            takes.set(takes.get() + 1);
            Some(gauges(Some(61)))
        };
        let mut s = DeliverySampler::new(true);
        s.on_reading_with(t0, r(0, 1, 0), take);
        s.on_reading_with(at(t0, 1_000), r(10_000, 1, 0), take);
        assert_eq!(takes.get(), 1, "a reading sooner than SAMPLE_MIN took");
        assert!(
            s.on_reading_with(at(t0, 2_000), r(20_000, 1, 0), take)
                .is_some()
        );
        s.on_reading_with(at(t0, 2_500), r(30_000, 1, 0), take);
        assert!(
            s.on_reading_with(at(t0, 3_500), r(40_000, 1, 0), take)
                .is_some()
        );
        assert_eq!(takes.get(), 3);
    }

    /// A sample whose start took no gauges (none there yet) reads none: its
    /// least would cover the SACKs before it too.
    #[test]
    fn a_sample_without_a_take_at_its_start_reads_no_gauges() {
        let t0 = Instant::now();
        let mut s = DeliverySampler::new(true);
        s.on_reading_with(t0, r(0, 1, 0), || None);
        let a = s
            .on_reading_with(at(t0, 2_000), r(100_000, 1, 0), || Some(gauges(Some(61))))
            .unwrap();
        assert_eq!(a.queue, None, "{a:?}");
        let b = s
            .on_reading_with(at(t0, 4_000), r(200_000, 1, 0), || Some(gauges(Some(62))))
            .unwrap();
        assert_eq!(b.queue.and_then(|q| q.least_ms), Some(62), "{b:?}");
        assert_eq!(b.queue.and_then(|q| q.standing_ms), Some(0), "{b:?}");
    }

    /// FR-148 P0 — the line folds its samples' gauges: the least and the standing
    /// over the samples that count, the windows and the queue at each close,
    /// the threshold last, and the losses in the line.
    #[test]
    fn the_line_folds_its_samples_gauges() {
        let t0 = Instant::now();
        let mut s = DeliverySampler::new(true);
        let g = |inflight, cwnd, ssthresh, rwnd, pending, least: Option<u64>, fr, t3| Gauges {
            inflight_bytes: inflight,
            cwnd,
            ssthresh,
            rwnd,
            pending_bytes: pending,
            fast_retransmits: fr,
            t3_expiries: t3,
            ..gauges(least)
        };
        let start = g(0, 4_380, 1_048_576, 1_048_576, 0, None, 2, 1);
        s.on_reading_with(t0, r(0, 1, 0), || Some(start));
        let closes = [
            (
                200_000,
                g(50_000, 60_000, 1_048_576, 900_000, 10_000, Some(70), 2, 1),
            ),
            (
                200_000,
                g(80_000, 90_000, 1_048_576, 700_000, 130_000, Some(64), 3, 1),
            ),
            (
                150_000,
                g(120_000, 128_000, 64_000, 800_000, 131_072, Some(500), 5, 2),
            ),
            // Stalled: no SACK, so no delay.
            (0, g(120_000, 4_800, 64_000, 800_000, 131_072, None, 5, 3)),
            // One lone packet: its delay does not count.
            (
                1_000,
                g(1_000, 4_800, 64_000, 1_000_000, 0, Some(280), 5, 3),
            ),
        ];
        let mut acked = 0;
        let mut readings = Vec::new();
        for (i, (bytes, close)) in closes.into_iter().enumerate() {
            acked += bytes;
            let now = at(t0, 2_000 * (i as u64 + 1));
            let sample = s
                .on_reading_with(now, r(acked, 1, 0), || Some(close))
                .unwrap();
            readings.push(sample.queue.unwrap());
        }
        let standing: Vec<_> = readings.iter().map(|q| q.standing_ms).collect();
        assert_eq!(standing, [Some(0), Some(0), Some(436), None, None]);
        let line = s.take_line(at(t0, 10_000)).unwrap();
        assert_eq!(
            line.queue,
            Some(QueueLine {
                ack_ms: Some((64, 64, 500)),
                standing_ms: Some((0, 436)),
                queued: 1,
                inflight: (1_000, 120_000),
                cwnd: (4_800, 128_000),
                ssthresh: 64_000,
                rwnd_low: 700_000,
                pending_high: 131_072,
                loss: (3, 2),
            })
        );
    }

    /// FR-148 P0 — a queued sample stands [`QUEUED_OVER_MS`] or more over the
    /// session's least, and one 1 ms short of it does not.
    #[test]
    fn a_sample_is_queued_250_ms_over_the_sessions_least() {
        let mut s = DeliverySampler::new(true);
        let q = feed(
            &mut s,
            Instant::now(),
            &[
                (200_000, Some(61)),
                (200_000, Some(310)),
                (200_000, Some(311)),
            ],
        );
        let standing: Vec<_> = q.iter().map(|x| x.standing_ms).collect();
        assert_eq!(standing, [Some(0), Some(249), Some(250)]);
        let queued: Vec<_> = q.iter().map(QueueReading::queued).collect();
        assert_eq!(queued, [false, false, true]);
    }

    /// FR-148 P0 — a session that starts on a still screen, then loads, keeps the
    /// least of a sample that counts. A small lone packet's delay carries no
    /// full packet's serialisation, nor the second packet's that sends a SACK
    /// at once, so it sits under what any loaded sample can read: measured
    /// from it, every loaded sample would stand that much higher.
    #[test]
    fn the_session_least_comes_from_samples_that_count() {
        let mut s = DeliverySampler::new(true);
        let q = feed(
            &mut s,
            Instant::now(),
            &[
                // A still screen: a small message a second, its SACKs at once.
                (400, Some(40)),
                (400, Some(40)),
                (400, Some(41)),
                // Then loaded.
                (200_000, Some(60)),
                (200_000, Some(300)),
                (200_000, Some(320)),
            ],
        );
        for x in &q[..3] {
            assert!(x.least_ms.is_some() && x.standing_ms.is_none(), "{q:?}");
        }
        let standing: Vec<_> = q[3..].iter().map(|x| x.standing_ms).collect();
        assert_eq!(standing, [Some(0), Some(240), Some(260)], "{q:?}");
        assert_eq!(s.least_ms, Some(60));
        let line = s.take_rest().unwrap().queue.unwrap();
        assert_eq!(line.ack_ms, Some((60, 60, 320)));
        assert_eq!(line.standing_ms, Some((0, 260)));
        assert_eq!(
            line.queued, 1,
            "from the still screen's least it would be 2"
        );
    }

    /// FR-148 P0 — why the byte rule: a browser holds a lone packet's SACK for 200
    /// ms, so on a still screen after a loaded spell every SACK waited, and a
    /// path's jitter adds to that. Such a sample's least stands 260 ms over
    /// the session's and does not count; the same least with a loaded
    /// sample's bytes is queued.
    #[test]
    fn a_still_screens_held_sacks_never_read_queued() {
        let mut s = DeliverySampler::new(true);
        let q = feed(
            &mut s,
            Instant::now(),
            &[
                (200_000, Some(61)),
                (2_400, Some(321)),
                (2_400, Some(321)),
                (200_000, Some(321)),
            ],
        );
        assert_eq!(q[0].standing_ms, Some(0));
        assert!(!q[1].queued() && !q[2].queued(), "{q:?}");
        assert_eq!(q[3].standing_ms, Some(260));
        assert!(q[3].queued(), "{q:?}");
    }

    #[test]
    fn a_dropped_sampler_writes_its_last_samples() {
        let t0 = Instant::now();
        let out = logged(|| {
            let mut s = DeliverySampler::new(true);
            s.session = Some(sid());
            s.on_reading(t0, r(0, 1, 0));
            s.on_reading(at(t0, 2_000), r(250_000, 1, 0));
        });
        assert!(out.contains(LINE), "{out:?}");
        assert!(out.contains("delivery_samples=(1, 0, 0)"), "{out:?}");
        // And one with nothing sampled writes nothing.
        let out = logged(|| {
            let mut s = DeliverySampler::new(true);
            s.session = Some(sid());
            s.on_reading(t0, r(0, 1, 0));
        });
        assert!(out.is_empty(), "{out:?}");
    }

    fn n(bps: u32, basis: Basis, ms: u64) -> Option<PathNumber> {
        Some(PathNumber::new(bps, basis, Duration::from_millis(ms)))
    }

    /// Step 3: a relayed session sending more than the path takes. Every field
    /// has a number.
    fn busy_view() -> PathView {
        let floor = n(1_399_272, Basis::AppLimited, 41_300);
        let goodput = n(1_162_000, Basis::Backlogged, 800);
        PathView {
            goodput,
            belief: (floor, floor, goodput),
            measured: goodput,
            prior: n(1_162_000, Basis::Assumed, 800),
            learned_ceiling: n(0, Basis::Assumed, 312_400),
            slow_link_floor: n(987_700, Basis::Backlogged, 800),
            viewer_rx: n(1_130_112, Basis::AppLimited, 400),
            seed: Some(PathNumber::remembered(
                1_250_000,
                Duration::from_secs(9_120),
            )),
            model_floor: floor,
            model_capacity: goodput,
            model_stand_in: floor,
            model_floor_by: None,
            model_capacity_by: Some(PushedBy::P3),
            model_ruled: Some((Ruled::Hole, Duration::from_millis(3_400))),
            // FR-148 D50: a fall a minute back, before a delivery raised the
            // floor again.
            model_fell: Some(Fall {
                from_bps: 2_131_000,
                to_bps: 1_162_000,
                age: Duration::from_millis(61_300),
            }),
        }
    }

    /// Step 3: the same session's screen at rest. Nothing measures the
    /// capacity; the floor stands, older.
    fn idle_view() -> PathView {
        let floor = n(1_399_272, Basis::AppLimited, 101_300);
        PathView {
            belief: (floor, floor, None),
            learned_ceiling: n(0, Basis::Assumed, 372_400),
            viewer_rx: n(9_120, Basis::AppLimited, 600),
            seed: Some(PathNumber::remembered(
                1_250_000,
                Duration::from_secs(9_180),
            )),
            model_floor: floor,
            ..PathView::default()
        }
    }

    #[test]
    fn the_model_line_names_its_fields() {
        let out = logged(|| emit_model(sid(), &busy_view()));
        let mut rest = out.as_str();
        for want in [
            " INFO ",
            MODEL_LINE,
            "session_id=65a1b2c3d4e5f60718293a4b",
            "goodput_bps=1162000/backlogged/800ms",
            "pipe_belief=(1399272/app-limited/41300ms, 1399272/app-limited/41300ms, \
             1162000/backlogged/800ms)",
            "pipe_measured=1162000/backlogged/800ms",
            "prior_bps=1162000/assumed/800ms",
            "learned_ceiling_bps=0/assumed/312400ms",
            "slow_link_floor_bps=987700/backlogged/800ms",
            "viewer_rx_bps=1130112/app-limited/400ms",
            "seed_bps=1250000/remembered/9120000ms",
            "model_floor_bps=1399272/app-limited/41300ms",
            "model_capacity_bps=1162000/backlogged/800ms",
            "model_stand_in_bps=1399272/app-limited/41300ms",
            "model_floor_push_back=-",
            "model_capacity_push_back=p3",
            "model_rule=hole/3400ms",
        ] {
            let at = rest
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} is not in order in {out:?}"));
            rest = &rest[at + want.len()..];
        }
        assert_eq!(out.lines().count(), 1, "{out:?}");
        assert!(
            !out.contains("None"),
            "a busy line has every number: {out:?}"
        );
        let idle = logged(|| emit_model(sid(), &idle_view()));
        for want in [
            "goodput_bps=None",
            "pipe_belief=(1399272/app-limited/101300ms, 1399272/app-limited/101300ms, None)",
            "pipe_measured=None",
            "prior_bps=None",
            "viewer_rx_bps=9120/app-limited/600ms",
            "model_capacity_bps=None",
            "model_stand_in_bps=None",
            "model_floor_push_back=-",
            "model_capacity_push_back=-",
            "model_rule=-",
        ] {
            assert!(idle.contains(want), "{want:?} is not in {idle:?}");
        }
        // Its cost: at most one line a pump every LINE_EVERY.
        let per_hour = 3_600 / LINE_EVERY.as_secs();
        for (what, l) in [("busy", &out), ("idle", &idle)] {
            println!(
                "path model line, {what}: {} bytes; {per_hour} lines and {} bytes a session hour\n{l}",
                l.len(),
                l.len() as u64 * per_hour
            );
            assert!(l.len() < 800, "{} bytes: {l:?}", l.len());
        }
    }

    /// FR-148 D48 — the line's one new field names the rule that last kept a
    /// reading out of the shadow model, re-read it or bounded it: B2's stall
    /// gate, rule 1's re-read, rule 4's ramp, rule 2's bound, rule 3's hole,
    /// with its age; `-` while none has (`the_model_line_names_its_fields`).
    #[test]
    fn the_model_line_names_each_rule() {
        for (rule, word) in [
            (Ruled::Stall, "stall"),
            (Ruled::ReRead, "reread"),
            (Ruled::Ramp, "ramp"),
            (Ruled::Bound, "bound"),
            (Ruled::Hole, "hole"),
        ] {
            let view = PathView {
                model_ruled: Some((rule, Duration::from_millis(1_250))),
                ..busy_view()
            };
            let out = logged(|| emit_model(sid(), &view));
            let want = format!("model_rule={word}/1250ms");
            assert!(out.contains(&want), "{want:?} is not in {out:?}");
            assert_eq!(out.matches("model_rule=").count(), 1, "{out:?}");
        }
    }

    /// FR-148 D50 — the line's one new field, beside `model_rule`: the shadow
    /// model's last fall, the floor before and after and how long ago, or `-`
    /// while none has. The busy line carries one and stays under its cost.
    #[test]
    fn the_model_line_writes_the_last_fall() {
        let busy = logged(|| emit_model(sid(), &busy_view()));
        let want = "model_rule=hole/3400ms model_fell=2131000->1162000/61300ms";
        assert!(busy.contains(want), "{want:?} is not in {busy:?}");
        assert_eq!(busy.matches("model_fell=").count(), 1, "{busy:?}");
        assert!(busy.len() < 800, "{} bytes: {busy:?}", busy.len());
        let idle = logged(|| emit_model(sid(), &idle_view()));
        let want = "model_rule=- model_fell=-";
        assert!(idle.contains(want), "{want:?} is not in {idle:?}");
        assert_eq!(idle.matches("model_fell=").count(), 1, "{idle:?}");
        let view = PathView {
            model_fell: Some(Fall {
                from_bps: 1_983_640,
                to_bps: 1_011_220,
                age: Duration::ZERO,
            }),
            ..idle_view()
        };
        let out = logged(|| emit_model(sid(), &view));
        let want = "model_fell=1983640->1011220/0ms";
        assert!(out.contains(want), "{want:?} is not in {out:?}");
    }

    #[test]
    fn the_model_line_comes_every_ten_seconds_and_a_drop_writes_the_last() {
        let t0 = Instant::now();
        let views = Cell::new(0u32);
        let view = |ms: u32| {
            let views = &views;
            move |_: Instant| {
                views.set(views.get() + 1);
                PathView {
                    viewer_rx: n(ms, Basis::AppLimited, 0),
                    ..PathView::default()
                }
            }
        };
        let out = logged(|| {
            let mut s = DeliverySampler::new(true);
            for ms in (0..=14_000u32).step_by(2_000) {
                s.on_view(at(t0, u64::from(ms)), sid(), view(ms));
            }
        });
        assert_eq!(views.get(), 8, "one view a heartbeat");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out:?}");
        assert!(
            lines[0].contains("viewer_rx_bps=10000/"),
            "the line at 10 s: {out:?}"
        );
        assert!(
            lines[1].contains("viewer_rx_bps=14000/"),
            "the drop's: {out:?}"
        );
        // A drop right after a line writes nothing more.
        let out = logged(|| {
            let mut s = DeliverySampler::new(true);
            s.on_view(t0, sid(), view(0));
            s.on_view(at(t0, 10_000), sid(), view(10_000));
        });
        assert_eq!(out.lines().count(), 1, "{out:?}");
    }

    #[test]
    fn off_takes_no_path_numbers_and_writes_no_model_line() {
        let t0 = Instant::now();
        let asked = Cell::new(false);
        let out = logged(|| {
            let mut s = DeliverySampler::new(false);
            for i in 0..30u64 {
                s.on_view(at(t0, i * 2_000), sid(), |_| {
                    asked.set(true);
                    busy_view()
                });
            }
            // Its drop writes nothing either.
        });
        assert!(!asked.get(), "off asked the governor for its numbers");
        assert!(out.is_empty(), "off wrote {out:?}");
    }

    /// The seed is `RateMemory::seed_at_open`'s, and its age is its entry's:
    /// none for a pair the memory does not seed.
    #[test]
    fn the_seed_at_open_comes_with_its_entrys_age() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rate-memory.json");
        let mut memory = crate::encode::rate_memory::RateMemory::default();
        let written = crate::encode::rate_memory::now_unix() - 3 * 3_600;
        memory.record_session("198.51.100.7|relay:turn/udp", Some(564_865), written);
        memory.save(&path).unwrap();
        let (seed, age) = seed_at_open(&path, "198.51.100.7|relay:turn/udp");
        assert_eq!(seed, Some(564_865));
        let age = age.expect("an age").as_secs();
        assert!((3 * 3_600..3 * 3_600 + 60).contains(&age), "{age} s");
        assert_eq!(seed_at_open(&path, "198.51.100.8|direct"), (None, None));
        let missing = dir.path().join("none.json");
        assert_eq!(
            seed_at_open(&missing, "198.51.100.7|relay:turn/udp"),
            (None, None)
        );
    }

    /// O10 — gauges as an association holds them before any loss: the
    /// threshold given, the rest a session's first write.
    fn opening(ssthresh: u32) -> Gauges {
        Gauges {
            max_payload: 1_188,
            cwnd: 4_380,
            ssthresh,
            rwnd: 1_043_200,
            inflight_bytes: 0,
            pending_bytes: 2_404,
            ..Gauges::default()
        }
    }

    #[test]
    fn a_threshold_of_0_with_no_loss_opened_on_the_cookie_echo() {
        assert_eq!(OpenedBy::of(Some(&opening(0))), OpenedBy::CookieEcho);
        // Whatever the peer's window says: a SACK sets it on both paths.
        for rwnd in [0, 1_500, 1_048_576] {
            let g = Gauges { rwnd, ..opening(0) };
            assert_eq!(OpenedBy::of(Some(&g)), OpenedBy::CookieEcho, "{g:?}");
        }
    }

    #[test]
    fn a_threshold_above_0_with_no_loss_opened_on_the_init_ack() {
        for ssthresh in [1, 1_500, 131_072, 1_048_576, u32::MAX] {
            let g = opening(ssthresh);
            assert_eq!(OpenedBy::of(Some(&g)), OpenedBy::InitAck, "{g:?}");
        }
    }

    #[test]
    fn a_loss_before_the_first_write_leaves_the_open_path_unknown() {
        for (ssthresh, fast_retransmits, t3_expiries) in [
            (0, 1, 0),
            (0, 0, 1),
            (17_520, 1, 0),
            (4_800, 0, 2),
            (4_800, 3, 1),
        ] {
            let g = Gauges {
                fast_retransmits,
                t3_expiries,
                ..opening(ssthresh)
            };
            assert_eq!(OpenedBy::of(Some(&g)), OpenedBy::Unknown, "{g:?}");
        }
    }

    #[test]
    fn no_gauges_is_unread() {
        assert_eq!(OpenedBy::of(None), OpenedBy::Unread);
        let words = [
            OpenedBy::CookieEcho,
            OpenedBy::InitAck,
            OpenedBy::Unknown,
            OpenedBy::Unread,
        ]
        .map(OpenedBy::word);
        assert_eq!(words, ["cookie_echo", "init_ack", "unknown", "unread"]);
    }

    #[test]
    fn the_open_line_names_its_fields() {
        let out = logged(|| emit_open(sid(), Pump::FfmpegDc, Some(&opening(0))));
        let mut rest = out.as_str();
        for want in [
            " INFO ",
            OPEN_LINE,
            "session_id=65a1b2c3d4e5f60718293a4b",
            "pump=ffmpeg-dc",
            "open_path=cookie_echo",
            "ssthresh=0",
            "rwnd=1043200",
            "cwnd=4380",
            "inflight_bytes=0",
            "pending_bytes=2404",
            "max_payload=1188",
            "fast_retransmits=0",
            "t3_expiries=0",
        ] {
            let at = rest
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} is not in order in {out:?}"));
            rest = &rest[at + want.len()..];
        }
        assert_eq!(out.lines().count(), 1, "{out:?}");
        let init_ack = logged(|| emit_open(sid(), Pump::Vp9444Dc, Some(&opening(1_048_576))));
        for want in ["pump=vp9-444-dc", "open_path=init_ack", "ssthresh=1048576"] {
            assert!(init_ack.contains(want), "{want:?} is not in {init_ack:?}");
        }
        // Without gauges: the session, the pump and the open path alone.
        let unread = logged(|| emit_open(sid(), Pump::Vp9444Dc, None));
        assert!(unread.contains("open_path=unread"), "{unread:?}");
        assert!(!unread.contains("ssthresh"), "{unread:?}");
        // Its cost: one line a session.
        for l in [&out, &init_ack, &unread] {
            println!("open line: {} bytes\n{l}", l.len());
            assert!(l.len() < 400, "{} bytes: {l:?}", l.len());
        }
    }

    #[test]
    fn the_open_line_comes_once_a_pump_and_off_writes_none() {
        let asked = Cell::new(0u32);
        let gauges = || {
            asked.set(asked.get() + 1);
            None
        };
        let out = logged(|| {
            let mut s = DeliverySampler::new(true);
            let first = s.on_first_write(sid(), Pump::FfmpegDc, gauges);
            assert_eq!(first, Some(OpenedBy::Unread));
            for _ in 0..5 {
                assert_eq!(s.on_first_write(sid(), Pump::FfmpegDc, gauges), None);
            }
        });
        assert_eq!(
            asked.get(),
            1,
            "the gauges are read at the first write only"
        );
        assert_eq!(out.lines().count(), 1, "{out:?}");
        assert!(out.contains(OPEN_LINE), "{out:?}");
        let out = logged(|| {
            let mut s = DeliverySampler::new(false);
            for _ in 0..5 {
                assert_eq!(s.on_first_write(sid(), Pump::FfmpegDc, gauges), None);
            }
        });
        assert_eq!(asked.get(), 1, "off read the gauges");
        assert!(out.is_empty(), "off wrote {out:?}");
    }

    /// The number a logged line gives `name`.
    fn field(out: &str, name: &str) -> u64 {
        let key = format!(" {name}=");
        let at = out
            .find(&key)
            .unwrap_or_else(|| panic!("no {name} in {out:?}"))
            + key.len();
        let digits: String = out[at..].chars().take_while(char::is_ascii_digit).collect();
        digits
            .parse()
            .unwrap_or_else(|_| panic!("{name} in {out:?}"))
    }

    /// O10's cells, over 4a's shaped link: an association opened by `path` on
    /// 8 Mbit/s at 80 ms with the viewer's default window (1 MiB), a control
    /// message written and acknowledged first, as a session's other channels
    /// open before its video, and then the pump's first media write. Returns
    /// what the line said, the line, and the gauges taken after it.
    async fn the_first_write_on(path: OpenPath) -> (Option<OpenedBy>, String, Gauges) {
        use bytes::Bytes;
        use webrtc::sctp::chunk::chunk_payload_data::PayloadProtocolIdentifier;

        let (a, b) =
            shaped_link::shaped(8_000_000, Duration::from_millis(40), Duration::from_secs(1));
        let (agent, viewer) =
            shaped_link::pair_opened(Arc::new(a), Arc::new(b), 0, path, Arc::default()).await;
        let control = agent
            .open_stream(0, PayloadProtocolIdentifier::Binary)
            .await
            .unwrap();
        control
            .write(&Bytes::from_static(b"control"))
            .await
            .unwrap();
        let clock = agent.delivery_clock();
        let deadline = Instant::now() + Duration::from_secs(10);
        while clock.read().acked_bytes == 0 {
            assert!(
                Instant::now() < deadline,
                "the control message was never acknowledged"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let video = agent
            .open_stream(1, PayloadProtocolIdentifier::Binary)
            .await
            .unwrap();
        let mut sampler = DeliverySampler::new(true);
        let mut said = None;
        let out = logged(|| {
            said = sampler.on_first_write(sid(), Pump::FfmpegDc, || Some(agent.gauges()));
        });
        video
            .write(&Bytes::from(vec![0x5a; 16 * 1024]))
            .await
            .unwrap();
        let again = logged(|| {
            let second = sampler.on_first_write(sid(), Pump::FfmpegDc, || Some(agent.gauges()));
            assert_eq!(second, None);
        });
        assert!(again.is_empty(), "a second write wrote {again:?}");
        let after = agent.gauges().take();
        let _ = agent.close().await;
        let _ = viewer.close().await;
        (said, out, after)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_association_its_init_ack_opened_reads_init_ack_at_the_first_write() {
        let (said, out, after) = the_first_write_on(OpenPath::InitAck).await;
        println!("path I: {out}");
        assert_eq!(said, Some(OpenedBy::InitAck), "{out:?}");
        assert!(out.contains("open_path=init_ack"), "{out:?}");
        assert_eq!(
            field(&out, "ssthresh"),
            1_048_576,
            "the viewer's window: {out:?}"
        );
        assert_eq!(
            field(&out, "fast_retransmits") + field(&out, "t3_expiries"),
            0
        );
        // The line peeked: the control message's least delay is still there.
        assert!(after.ack_delay_min.is_some(), "the line took it: {after:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_association_its_peers_cookie_echo_opened_reads_cookie_echo_at_the_first_write() {
        let (said, out, after) = the_first_write_on(OpenPath::CookieEcho).await;
        println!("path C: {out}");
        assert_eq!(said, Some(OpenedBy::CookieEcho), "{out:?}");
        assert!(out.contains("open_path=cookie_echo"), "{out:?}");
        assert_eq!(field(&out, "ssthresh"), 0, "{out:?}");
        // The control message's SACK set the peer's window: by the first media
        // write, path C's 0 and 0 is the threshold's 0 alone.
        assert!(field(&out, "rwnd") > 0, "{out:?}");
        assert_eq!(
            field(&out, "fast_retransmits") + field(&out, "t3_expiries"),
            0
        );
        assert!(after.ack_delay_min.is_some(), "the line took it: {after:?}");
    }
}

/// Done-criterion 3 of FR-148 P0's step 2: the sampler reads a known pipe. Two
/// in-process SCTP associations (the vendored webrtc-sctp, unmodified but for
/// the clock and the gauges) over a link this module shapes: each direction
/// serialises every packet at the link's rate, holds it 10 ms more, and drops a
/// packet that would wait more than 200 ms in its queue. Real time, so each
/// cell takes a few seconds. Step 4a's train cells (`media_train`) use the same
/// link with a one-way delay and a queue of their own ([`shaped`]), and the
/// shaper's peak wait ([`ShapedConn::peak_wait`]).
///
/// The viewer's receive window ([`rwnd`]) is 7/8 of what the link's queue
/// holds, so what is in flight stays under the queue at every rate, and the
/// writer's messages ([`message_len`]) fit that window. A lossy link measures
/// SCTP's loss recovery, not the sampler: with SCTP's 1 MB default window,
/// backlogged samples at 8 Mbit/s read 0.9 to 6.9 Mbit/s, and with a 48 KiB
/// window over 0.4 Mbit/s's 10,000-byte queue one sample under load read 68 %.
/// A path recovering from loss reads as a low backlogged rate: a sample
/// carries what the path delivered, not what it could have. One cell drops
/// packets on purpose: an unordered `maxRetransmits: 0` channel, which
/// recovers nothing, so it measures what the clock counts of a lossy path.
///
/// What it does not prove: a real carrier's loss, jitter, reordering and
/// stalls, a browser's SCTP at the far end, and DTLS and ICE beneath SCTP.
#[cfg(test)]
pub(crate) mod shaped_link {
    use std::any::Any;
    use std::io;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use async_trait::async_trait;
    use bytes::Bytes;
    use tokio::sync::{Mutex, mpsc};
    use webrtc::sctp::association::{Association, Config};
    use webrtc::sctp::chunk::chunk_payload_data::PayloadProtocolIdentifier;
    use webrtc::sctp::stream::ReliabilityType;
    use webrtc::util::Conn;

    use super::*;

    type UResult<T> = std::result::Result<T, webrtc::util::Error>;

    const DELAY: Duration = Duration::from_millis(10);
    const QUEUE_MS: u64 = 200;
    const QUEUE: Duration = Duration::from_millis(QUEUE_MS);

    /// The viewer's receive window on a `bps` link: 7/8 of what the link's
    /// queue holds, so a sender that fills the window never overruns the
    /// queue, at every rate.
    fn rwnd(bps: u64) -> u32 {
        let queue = bps / 8 * QUEUE_MS / 1000;
        u32::try_from(queue * 7 / 8).unwrap_or(u32::MAX)
    }

    /// The backlogged writer's message on a `bps` link: a quarter of the
    /// window, at most 16 KiB. The receiver counts a message's chunks against
    /// its window before the message is whole, and drops new DATA once the
    /// window is full, so a message must fit the window with room to spare.
    fn message_len(bps: u64) -> usize {
        (usize::try_from(rwnd(bps)).unwrap_or(usize::MAX) / 4).clamp(1024, 16 * 1024)
    }

    /// A packet this long or longer carries a full DATA chunk: a lossy
    /// direction drops only such packets, so the handshake, SACKs and
    /// FORWARD TSNs always pass.
    const FULL_PACKET: usize = 1_000;

    /// One direction of the link.
    struct Shaper {
        bps: u64,
        /// How long a packet travels once it is serialised.
        one_way: Duration,
        /// The longest a packet may wait for the wire; one that would wait
        /// longer is dropped at the tail.
        queue: Duration,
        next_free: StdMutex<Instant>,
        due: mpsc::UnboundedSender<(Instant, Vec<u8>)>,
        /// Drops every n-th [`FULL_PACKET`] before its queue; 0 drops none.
        drop_every: u64,
        full_packets: AtomicU64,
        /// The longest any packet waited for the wire, nanoseconds.
        peak_wait_ns: AtomicU64,
        /// Packets dropped at the tail.
        tail_drops: AtomicU64,
        /// FR-148 D46 — outages: no packet starts its serialisation in one.
        outages: Option<Outages>,
        /// FR-148 D46, the drop step — a packet that starts its serialisation
        /// at or after this instant goes at this rate: a path whose capacity
        /// falls for real.
        step: Option<(Instant, u64)>,
    }

    /// FR-148 D46 — a link's outages: from `t0 + every` on, one every `every`,
    /// `len` long, in which the link neither serializes nor delivers. Time
    /// stands still for it: a packet that would start then starts at the
    /// outage's end, and one due then is due `len` later, so what queued in
    /// one drains at the link's rate after it.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct Outages {
        pub(crate) t0: Instant,
        pub(crate) every: Duration,
        pub(crate) len: Duration,
    }

    impl Outages {
        /// The start of the outage `t` falls in, if it falls in one.
        fn during(&self, t: Instant) -> Option<Instant> {
            let k = t.saturating_duration_since(self.t0).as_nanos() / self.every.as_nanos();
            let k = u32::try_from(k).ok().filter(|&k| k >= 1)?;
            let start = self.t0 + self.every * k;
            (t < start + self.len).then_some(start)
        }

        /// `t`, or the end of the outage it falls in.
        fn after(&self, t: Instant) -> Instant {
            self.during(t).map_or(t, |start| start + self.len)
        }
    }

    impl Shaper {
        fn send(&self, b: &[u8]) {
            if self.drop_every > 0
                && b.len() >= FULL_PACKET
                && (self.full_packets.fetch_add(1, Ordering::Relaxed) + 1)
                    .is_multiple_of(self.drop_every)
            {
                return; // Lost on the way, as a lossy carrier loses it.
            }
            let now = Instant::now();
            let mut next_free = self.next_free.lock().unwrap();
            let start = (*next_free).max(now);
            let start = self.outages.map_or(start, |o| o.after(start));
            let wait = start.saturating_duration_since(now);
            if wait > self.queue {
                self.tail_drops.fetch_add(1, Ordering::Relaxed);
                return; // Dropped at the tail, as a full router queue does.
            }
            let wait_ns = u64::try_from(wait.as_nanos()).unwrap_or(u64::MAX);
            self.peak_wait_ns.fetch_max(wait_ns, Ordering::Relaxed);
            let bps = match self.step {
                Some((at, bps)) if start >= at => bps,
                _ => self.bps,
            };
            let wire = Duration::from_nanos(b.len() as u64 * 8 * 1_000_000_000 / bps);
            *next_free = start + wire;
            let _ = self.due.send((start + wire + self.one_way, b.to_vec()));
        }
    }

    /// One end: it sends through its own shaper and receives what the other
    /// end's shaper delivers.
    pub(crate) struct ShapedConn {
        out: Shaper,
        inbox: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
        local: SocketAddr,
        remote: SocketAddr,
    }

    impl ShapedConn {
        /// The longest any packet this end sent waited in its queue for the
        /// wire: the peak queueing delay at this direction's bottleneck.
        pub(crate) fn peak_wait(&self) -> Duration {
            Duration::from_nanos(self.out.peak_wait_ns.load(Ordering::Relaxed))
        }

        /// The packets this end's queue dropped at its tail.
        pub(crate) fn tail_drops(&self) -> u64 {
            self.out.tail_drops.load(Ordering::Relaxed)
        }
    }

    /// Scheduling noise, as a busy test runner makes it, seeded so that a
    /// cell meets the same draws on every run. Every wake-up of a task comes
    /// up to `jitter` late, as a timer's does; and about once every
    /// `stall_every` the scheduler gives the task's core away for up to
    /// `stall`, and a wake-up due then comes when it returns. On the link a
    /// wake-up is a packet's delivery, and the packets behind a late one wait
    /// for it, in order. No wake-up comes more than `stall + jitter` late.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct Noise {
        pub(crate) seed: u64,
        pub(crate) jitter: Duration,
        pub(crate) stall: Duration,
        /// The mean time between stalls; zero for none.
        pub(crate) stall_every: Duration,
    }

    impl Noise {
        /// The same noise, drawn from another sequence: one per task.
        pub(crate) fn reseeded(self, salt: u64) -> Self {
            Self {
                seed: self.seed ^ salt,
                ..self
            }
        }
    }

    /// One task's draws of a [`Noise`].
    #[derive(Debug)]
    pub(crate) struct Draws {
        noise: Noise,
        state: u64,
        /// The stall due next or under way: when it starts and ends.
        stall: Option<(Instant, Instant)>,
    }

    impl Draws {
        pub(crate) fn new(noise: Noise) -> Self {
            Self {
                noise,
                state: noise.seed,
                stall: None,
            }
        }

        /// How late a wake-up due at `due` comes. A stall starts from 0 to
        /// twice `stall_every` after the one before it ended (after the first
        /// wake-up, for the first), whether or not anything was due in it.
        pub(crate) fn late(&mut self, due: Instant) -> Duration {
            let jitter = uniform(&mut self.state, self.noise.jitter);
            let every = self.noise.stall_every;
            if every.is_zero() {
                return jitter;
            }
            let mut after = due;
            loop {
                let (start, end) = match self.stall {
                    Some(stall) => stall,
                    None => {
                        let start = after + uniform(&mut self.state, 2 * every);
                        let stall = (start, start + uniform(&mut self.state, self.noise.stall));
                        self.stall = Some(stall);
                        stall
                    }
                };
                if due < start {
                    return jitter;
                }
                if due < end {
                    return end - due + jitter;
                }
                self.stall = None;
                after = end;
            }
        }
    }

    /// A draw from 0 to `most`.
    fn uniform(state: &mut u64, most: Duration) -> Duration {
        let ns = u64::try_from(most.as_nanos()).unwrap_or(u64::MAX);
        Duration::from_nanos(draw(state) % ns.saturating_add(1))
    }

    /// The next draw of a seeded sequence (splitmix64).
    fn draw(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// One direction's delivery: each packet reaches the far inbox when due,
    /// or as late as `noise` draws.
    fn direction(
        bps: u64,
        one_way: Duration,
        queue: Duration,
        drop_every: u64,
        noise: Option<Noise>,
    ) -> (Shaper, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (due_tx, mut due_rx) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut draws = noise.map(Draws::new);
            while let Some((due, p)) = due_rx.recv().await {
                let due = draws.as_mut().map_or(due, |d| due + d.late(due));
                tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
                if inbox_tx.send(p).is_err() {
                    break;
                }
            }
        });
        let shaper = Shaper {
            bps,
            one_way,
            queue,
            next_free: StdMutex::new(Instant::now()),
            due: due_tx,
            drop_every,
            full_packets: AtomicU64::new(0),
            peak_wait_ns: AtomicU64::new(0),
            tail_drops: AtomicU64::new(0),
            outages: None,
            step: None,
        };
        (shaper, inbox_rx)
    }

    /// FR-148 D46 — one direction of a link with `outages`, lossless: a
    /// packet due in an outage is due its length later.
    fn out_direction(
        bps: u64,
        one_way: Duration,
        queue: Duration,
        outages: Outages,
    ) -> (Shaper, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (due_tx, mut due_rx) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some((due, p)) = due_rx.recv().await {
                let due = outages.during(due).map_or(due, |_| due + outages.len);
                tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
                if inbox_tx.send(p).is_err() {
                    break;
                }
            }
        });
        let shaper = Shaper {
            bps,
            one_way,
            queue,
            next_free: StdMutex::new(Instant::now()),
            due: due_tx,
            drop_every: 0,
            full_packets: AtomicU64::new(0),
            peak_wait_ns: AtomicU64::new(0),
            tail_drops: AtomicU64::new(0),
            outages: Some(outages),
            step: None,
        };
        (shaper, inbox_rx)
    }

    fn link(bps: u64) -> (ShapedConn, ShapedConn) {
        lossy_link(bps, 0)
    }

    /// A link whose agent-to-viewer direction drops every `drop_every`-th
    /// packet that carries a full DATA chunk; 0 drops none.
    fn lossy_link(bps: u64, drop_every: u64) -> (ShapedConn, ShapedConn) {
        shaped_dropping(bps, DELAY, QUEUE, drop_every, None)
    }

    /// A link of `bps` each way, whose packets travel `one_way` after their
    /// serialisation and wait at most `queue` for the wire, losing nothing
    /// else: the agent's end first.
    pub(crate) fn shaped(bps: u64, one_way: Duration, queue: Duration) -> (ShapedConn, ShapedConn) {
        shaped_dropping(bps, one_way, queue, 0, None)
    }

    /// As [`shaped`], with `noise` on both directions, each its own draws.
    pub(crate) fn shaped_noisy(
        bps: u64,
        one_way: Duration,
        queue: Duration,
        noise: Noise,
    ) -> (ShapedConn, ShapedConn) {
        shaped_dropping(bps, one_way, queue, 0, Some(noise))
    }

    fn shaped_dropping(
        bps: u64,
        one_way: Duration,
        queue: Duration,
        drop_every: u64,
        noise: Option<Noise>,
    ) -> (ShapedConn, ShapedConn) {
        let (a_out, b_inbox) = direction(bps, one_way, queue, drop_every, noise);
        let (b_out, a_inbox) = direction(
            bps,
            one_way,
            queue,
            0,
            noise.map(|n| n.reseeded(0x5bd1_e995_7f4a_7c15)),
        );
        let (a, b): (SocketAddr, SocketAddr) = (
            "10.0.0.1:5000".parse().unwrap(),
            "10.0.0.2:5000".parse().unwrap(),
        );
        (
            ShapedConn {
                out: a_out,
                inbox: Mutex::new(a_inbox),
                local: a,
                remote: b,
            },
            ShapedConn {
                out: b_out,
                inbox: Mutex::new(b_inbox),
                local: b,
                remote: a,
            },
        )
    }

    #[async_trait]
    impl Conn for ShapedConn {
        async fn connect(&self, _: SocketAddr) -> UResult<()> {
            Ok(())
        }
        async fn recv(&self, buf: &mut [u8]) -> UResult<usize> {
            match self.inbox.lock().await.recv().await {
                Some(p) => {
                    let n = p.len().min(buf.len());
                    buf[..n].copy_from_slice(&p[..n]);
                    Ok(n)
                }
                None => Err(io::Error::from(io::ErrorKind::ConnectionAborted).into()),
            }
        }
        async fn recv_from(&self, buf: &mut [u8]) -> UResult<(usize, SocketAddr)> {
            Ok((self.recv(buf).await?, self.remote))
        }
        async fn send(&self, b: &[u8]) -> UResult<usize> {
            self.out.send(b);
            Ok(b.len())
        }
        async fn send_to(&self, b: &[u8], _: SocketAddr) -> UResult<usize> {
            self.send(b).await
        }
        fn local_addr(&self) -> UResult<SocketAddr> {
            Ok(self.local)
        }
        fn remote_addr(&self) -> Option<SocketAddr> {
            Some(self.remote)
        }
        async fn close(&self) -> UResult<()> {
            Ok(())
        }
        fn as_any(&self) -> &(dyn Any + Send + Sync) {
            self
        }
    }

    /// The agent's end and the viewer's, associated over `a` and `b`; the
    /// viewer reads everything it is sent, through a window of `rwnd` bytes.
    async fn pair(
        a: Arc<dyn Conn + Send + Sync>,
        b: Arc<dyn Conn + Send + Sync>,
        rwnd: u32,
    ) -> (Association, Arc<Association>) {
        pair_counting(a, b, rwnd, Arc::default()).await
    }

    /// As [`pair`], and `taken` counts the bytes the viewer reads.
    async fn pair_counting(
        a: Arc<dyn Conn + Send + Sync>,
        b: Arc<dyn Conn + Send + Sync>,
        rwnd: u32,
        taken: Arc<AtomicU64>,
    ) -> (Association, Arc<Association>) {
        pair_opened(a, b, rwnd, OpenPath::InitAck, taken).await
    }

    /// Which handshake establishes the agent's end. The agent's real
    /// association is a client and so is the browser's: both send INIT, and
    /// which answer the agent takes first is a race.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum OpenPath {
        /// Path I: the agent's INIT is answered, and the peer's INIT ACK sets
        /// its slow start threshold to the peer's window. The agent is the
        /// client here.
        InitAck,
        /// Path C: the peer's COOKIE ECHO establishes the agent's end, whose
        /// slow start threshold and peer window stay 0 until the first SACK.
        /// The agent is the server here.
        CookieEcho,
    }

    /// The agent's end and the viewer's, associated over `a` and `b` by
    /// `path`; the viewer reads everything it is sent through a window of
    /// `rwnd` bytes (0: the vendored default, 1 MiB), and `taken` counts what
    /// it read.
    pub(crate) async fn pair_opened(
        a: Arc<dyn Conn + Send + Sync>,
        b: Arc<dyn Conn + Send + Sync>,
        rwnd: u32,
        path: OpenPath,
        taken: Arc<AtomicU64>,
    ) -> (Association, Arc<Association>) {
        let config = |net_conn, rwnd, name: &str| Config {
            net_conn,
            max_receive_buffer_size: rwnd,
            max_message_size: 0,
            name: name.to_owned(),
        };
        let (agent, viewer) = match path {
            OpenPath::InitAck => {
                let agent = tokio::spawn(Association::client(config(a, 0, "agent")));
                let viewer = Association::server(config(b, rwnd, "viewer")).await;
                (agent.await.unwrap().unwrap(), viewer.unwrap())
            }
            OpenPath::CookieEcho => {
                let viewer = tokio::spawn(Association::client(config(b, rwnd, "viewer")));
                let agent = Association::server(config(a, 0, "agent")).await;
                (agent.unwrap(), viewer.await.unwrap().unwrap())
            }
        };
        let viewer = Arc::new(viewer);
        let reader = Arc::clone(&viewer);
        tokio::spawn(async move {
            while let Some(stream) = reader.accept_stream().await {
                let taken = Arc::clone(&taken);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65_536];
                    // A shut stream answers 0 at once, forever: stop there.
                    while let Ok(n @ 1..) = stream.read(&mut buf).await {
                        taken.fetch_add(n as u64, Ordering::SeqCst);
                    }
                });
            }
        });
        (agent, viewer)
    }

    /// `n` samples of the agent's end, each [`SAMPLE`] long, after `warm`, while
    /// `write` runs.
    async fn sample_while<F>(bps: u64, warm: Duration, n: usize, write: F) -> Vec<Sample>
    where
        F: FnOnce(Arc<webrtc::sctp::stream::Stream>) -> tokio::task::JoinHandle<()>,
    {
        const SAMPLE: Duration = Duration::from_secs(2);
        let (a, b) = link(bps);
        let (agent, viewer) = pair(Arc::new(a), Arc::new(b), rwnd(bps)).await;
        let stream = agent
            .open_stream(1, PayloadProtocolIdentifier::Binary)
            .await
            .unwrap();
        let writer = write(stream);
        tokio::time::sleep(warm).await;
        let mut sampler = DeliverySampler::new(true);
        let mut out = Vec::new();
        let clock = || Some(agent.delivery_clock());
        let gauges = || Some(agent.gauges());
        let start = Instant::now();
        assert_eq!(sampler.observe(start, sid(), clock, gauges), None);
        for i in 1..=n as u32 {
            tokio::time::sleep_until(tokio::time::Instant::from_std(start + SAMPLE * i)).await;
            if let Some(s) = sampler.observe(Instant::now(), sid(), clock, gauges) {
                out.push(s);
            }
        }
        writer.abort();
        let _ = agent.close().await;
        let _ = viewer.close().await;
        out
    }

    fn sid() -> ObjectId {
        ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap()
    }

    /// The sender always has more than the link takes: every sample is
    /// backlogged, at the link's rate within 20 %.
    async fn backlogged_reads_the_link(bps: u64) {
        let samples = sample_while(bps, Duration::from_secs(3), 3, |stream| {
            tokio::spawn(async move {
                let msg = Bytes::from(vec![0x5a; message_len(bps)]);
                while stream.write(&msg).await.is_ok() {}
            })
        })
        .await;
        println!("backlogged on a {bps} bit/s link: {samples:?}");
        assert_eq!(samples.len(), 3, "{samples:?}");
        for s in &samples {
            assert_eq!(s.class, Class::Backlogged, "{bps} bit/s: {samples:?}");
            let got = s.bps().unwrap();
            let lo = bps * 8 / 10;
            let hi = bps * 12 / 10;
            assert!(
                (lo..=hi).contains(&got),
                "{bps} bit/s link delivered {got} bit/s: {samples:?}"
            );
        }
    }

    /// A trickle well under the link: every sample is application-limited.
    async fn a_trickle_is_application_limited(bps: u64) {
        let samples = sample_while(bps, Duration::from_secs(1), 3, |stream| {
            tokio::spawn(async move {
                // 2 KiB every 100 ms: 164 kbit/s.
                let msg = Bytes::from(vec![0xa5; 2 * 1024]);
                let mut tick = tokio::time::interval(Duration::from_millis(100));
                loop {
                    tick.tick().await;
                    if stream.write(&msg).await.is_err() {
                        break;
                    }
                }
            })
        })
        .await;
        println!("a trickle on a {bps} bit/s link: {samples:?}");
        assert_eq!(samples.len(), 3, "{samples:?}");
        for s in &samples {
            assert_eq!(s.class, Class::AppLimited, "{bps} bit/s: {samples:?}");
            assert!(s.bytes > 0, "{bps} bit/s: nothing delivered: {samples:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backlogged_at_0_4_mbps_reads_the_link() {
        backlogged_reads_the_link(400_000).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backlogged_at_2_mbps_reads_the_link() {
        backlogged_reads_the_link(2_000_000).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backlogged_at_8_mbps_reads_the_link() {
        backlogged_reads_the_link(8_000_000).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_trickle_at_0_4_mbps_is_application_limited() {
        a_trickle_is_application_limited(400_000).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_trickle_at_2_mbps_is_application_limited() {
        a_trickle_is_application_limited(2_000_000).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_trickle_at_8_mbps_is_application_limited() {
        a_trickle_is_application_limited(8_000_000).await;
    }

    /// The clock counts what the viewer received, not what was lost. An
    /// unordered channel with `maxRetransmits: 0`, as the viewer's opt-in
    /// opens it, writes as fast as it may over a 2 Mbit/s link that loses one
    /// full DATA packet in ten. Every chunk is abandoned at its first send,
    /// and FORWARD TSNs let the cumulative point pass the lost ones. Before
    /// the clock left them out it counted nearly all of them: at the third
    /// sample's end 2,128,000 bytes against 1,920,000 the viewer took, with
    /// 216,000 lost.
    ///
    /// It is not exact. A lost chunk that no later DATA follows before a
    /// FORWARD TSN covering it reaches the viewer is never reported missing,
    /// and the clock counts it. How often that happens depends on the timing.
    /// At the third sample's end, of about 215,000 bytes lost, four runs on a
    /// Windows box counted 11,000 to 21,000 over what the viewer took, and two
    /// on CI's Linux runners, under the whole suite, 75,000 and 70,000. So at
    /// each sample's end the clock has counted less than the viewer took plus
    /// two thirds of what was lost, and at least nine tenths of what the viewer
    /// took: the samples read backlogged.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lossy_unordered_channel_counts_what_the_viewer_took() {
        const SAMPLE: Duration = Duration::from_secs(2);
        const DROP_EVERY: u64 = 10;
        let bps = 2_000_000;
        let (a, b) = lossy_link(bps, DROP_EVERY);
        let a = Arc::new(a);
        let taken = Arc::new(AtomicU64::new(0));
        let (agent, viewer) =
            pair_counting(a.clone(), Arc::new(b), rwnd(bps), Arc::clone(&taken)).await;
        let stream = agent
            .open_stream(1, PayloadProtocolIdentifier::Binary)
            .await
            .unwrap();
        stream.set_reliability_params(true, ReliabilityType::Rexmit, 0);
        let writer = tokio::spawn(async move {
            // One chunk a message, so a message the viewer read is a chunk it
            // received, and a lost chunk is a lost message.
            let msg = Bytes::from(vec![0x3c; 1_000]);
            while stream.write(&msg).await.is_ok() {}
        });
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut sampler = DeliverySampler::new(true);
        let clock = || Some(agent.delivery_clock());
        let gauges = || Some(agent.gauges());
        let start = Instant::now();
        assert_eq!(sampler.observe(start, sid(), clock, gauges), None);
        let mut ends = Vec::new();
        for i in 1..=3u32 {
            tokio::time::sleep_until(tokio::time::Instant::from_std(start + SAMPLE * i)).await;
            let sample = sampler.observe(Instant::now(), sid(), clock, gauges);
            // The clock first, then the viewer's count, which only grows.
            let acked = agent.delivery_clock().read().acked_bytes;
            let took = taken.load(Ordering::SeqCst);
            // Each dropped packet held one 1,000-byte chunk.
            let lost = a.out.full_packets.load(Ordering::SeqCst) / DROP_EVERY * 1_000;
            ends.push((sample, acked, took, lost));
        }
        writer.abort();
        let _ = agent.close().await;
        let _ = viewer.close().await;
        println!("a lossy unordered channel, (sample, acked, viewer took, lost): {ends:?}");
        for (sample, acked, took, lost) in &ends {
            assert!(*acked < took + lost * 2 / 3, "counted {acked}: {ends:?}");
            assert!(
                *acked >= took * 9 / 10,
                "counted {acked} of {took}: {ends:?}"
            );
            let sample = sample.as_ref().expect("a sample every 2 s");
            assert_eq!(sample.class, Class::Backlogged, "{ends:?}");
        }
    }

    /// A second association is a new clock: no sample spans the two, and the
    /// first reading of the new one only sets where the next sample starts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_association_starts_a_new_sample() {
        let w = rwnd(8_000_000);
        let (a1, b1) = link(8_000_000);
        let (first, first_viewer) = pair(Arc::new(a1), Arc::new(b1), w).await;
        let (a2, b2) = link(8_000_000);
        let (second, second_viewer) = pair(Arc::new(a2), Arc::new(b2), w).await;
        assert!(first.delivery_clock().same(&first.delivery_clock()));
        assert!(!first.delivery_clock().same(&second.delivery_clock()));
        assert!(first.gauges().same(&first.gauges()));
        assert!(!first.gauges().same(&second.gauges()));
        let t0 = Instant::now();
        let mut s = DeliverySampler::new(true);
        let (one, two) = (|| Some(first.gauges()), || Some(second.gauges()));
        assert_eq!(
            s.observe(t0, sid(), || Some(first.delivery_clock()), one),
            None
        );
        let x = s.observe(at(t0, 2_000), sid(), || Some(first.delivery_clock()), one);
        assert_eq!(x.map(|x| x.ms), Some(2_000));
        s.least_ms = Some(1);
        // The new association's first reading starts over, with its own
        // gauges and its own least...
        assert_eq!(
            s.observe(at(t0, 4_000), sid(), || Some(second.delivery_clock()), two),
            None
        );
        assert!(s.gauges.as_ref().is_some_and(|g| g.same(&second.gauges())));
        assert_eq!(s.least_ms, None);
        // ...a held slot (no fresh clock) keeps reading the one it has, and
        // asks for no gauges...
        let asked = std::cell::Cell::new(false);
        let held = || {
            asked.set(true);
            None
        };
        assert_eq!(s.observe(at(t0, 5_000), sid(), || None, held), None);
        assert!(!asked.get(), "gauges asked for with no clock from the slot");
        // ...and the next sample is the new association's alone.
        let y = s.observe(at(t0, 6_000), sid(), || None, held);
        assert_eq!(y.map(|y| y.ms), Some(2_000));
        assert!(y.is_some_and(|y| y.queue.is_some()), "{y:?}");
        for a in [&first, &second] {
            let _ = a.close().await;
        }
        for v in [&first_viewer, &second_viewer] {
            let _ = v.close().await;
        }
    }

    /// FR-148 D46 B1 — the in-flight budget's least starts over on a new
    /// association, as the sampler's does: a new clock is a new association.
    /// A held slot reads the handles it has.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_inflight_budget_s_least_starts_over_on_a_new_clock() {
        let w = rwnd(8_000_000);
        let (a1, b1) = link(8_000_000);
        let (first, first_viewer) = pair(Arc::new(a1), Arc::new(b1), w).await;
        let (a2, b2) = link(8_000_000);
        let (second, second_viewer) = pair(Arc::new(a2), Arc::new(b2), w).await;
        let t0 = Instant::now();
        let one_ms = Some(Duration::from_millis(1));
        let mut s = crate::media_inflight::Shadow::new();
        s.on_frame_with(
            t0,
            0,
            || Some(first.delivery_clock()),
            || Some(first.gauges()),
        );
        s.set_least(Duration::from_millis(1));
        s.on_frame_with(at(t0, 10), 0, || Some(first.delivery_clock()), || None);
        assert_eq!(s.budget().least(), one_ms, "the same association keeps it");
        s.on_frame_with(at(t0, 20), 0, || None, || None);
        assert_eq!(s.budget().least(), one_ms, "a held slot keeps it");
        s.on_frame_with(
            at(t0, 30),
            0,
            || Some(second.delivery_clock()),
            || Some(second.gauges()),
        );
        let own = second.gauges().peek();
        let own = [own.ack_delay, own.ack_delay_min]
            .into_iter()
            .flatten()
            .min();
        assert_eq!(s.budget().least(), own, "the new association's own least");
        for a in [&first, &second] {
            let _ = a.close().await;
        }
        for v in [&first_viewer, &second_viewer] {
            let _ = v.close().await;
        }
    }

    /// Writes 16 KiB and waits until all of it is acknowledged and a SACK's
    /// delay is stored, so that no SACK with a delay comes after it returns.
    async fn delivered(
        agent: &Association,
        stream: &webrtc::sctp::stream::Stream,
        total: &mut u64,
    ) {
        let msg = Bytes::from(vec![0x11; 16 * 1024]);
        stream.write(&msg).await.unwrap();
        *total += msg.len() as u64;
        let deadline = Instant::now() + Duration::from_secs(10);
        while agent.delivery_clock().read().acked_bytes < *total
            || agent.gauges().peek().ack_delay_min.is_none()
        {
            assert!(Instant::now() < deadline, "16 KiB not acknowledged in 10 s");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// FR-148 P0 — the sampler is the agent's one reader that takes the
    /// gauges. A reader that only looks (`GaugeReader::peek`, FR-148 O10's open
    /// line) leaves a sample's least to the sampler's take; a reader that took
    /// instead would leave the sample none.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_peek_leaves_the_least_to_the_samplers_take() {
        let bps = 8_000_000;
        let (a, b) = link(bps);
        let (agent, viewer) = pair(Arc::new(a), Arc::new(b), rwnd(bps)).await;
        let stream = agent
            .open_stream(1, PayloadProtocolIdentifier::Binary)
            .await
            .unwrap();
        let clock = || Some(agent.delivery_clock());
        let gauges = || Some(agent.gauges());
        let t0 = Instant::now();
        let mut s = DeliverySampler::new(true);
        let mut total = 0;
        // The sample's start: the sampler takes.
        assert_eq!(s.observe(t0, sid(), clock, gauges), None);
        delivered(&agent, &stream, &mut total).await;
        let peeked = agent.gauges().peek().ack_delay_min;
        assert!(peeked.is_some(), "no SACK gave a delay");
        assert_eq!(
            agent.gauges().peek().ack_delay_min,
            peeked,
            "a peek took the least"
        );
        // The sample's close: the sampler's take reads what the peeks saw.
        let x = s.observe(at(t0, 2_000), sid(), clock, gauges).unwrap();
        assert_eq!(x.queue.and_then(|q| q.least_ms), peeked.map(ms_of), "{x:?}");
        // A second reader that takes leaves the next sample no least.
        delivered(&agent, &stream, &mut total).await;
        assert!(agent.gauges().take().ack_delay_min.is_some());
        let y = s.observe(at(t0, 4_000), sid(), clock, gauges).unwrap();
        assert_eq!(y.queue.map(|q| q.least_ms), Some(None), "{y:?}");
        let _ = agent.close().await;
        let _ = viewer.close().await;
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    /// FR-148 O11 — what the delivery classifier, SCTP's gauges and the
    /// viewer see on the shape a TCP carrier gives SCTP (TURN over TCP 443): a
    /// deep queue that loses nothing, because the kernel's send buffer and
    /// TCP's own retransmissions hold what the link has not carried yet. The
    /// control is the UDP carrier's shape: a router queue of 100 ms that drops
    /// at its tail.
    ///
    /// Each cell writes 1.5 times the link's rate in frames at 30 fps, each
    /// stamped with its write time, over the default 1 MiB windows at a 60 ms
    /// round trip, and records 20 one-second samples: the class, SCTP's
    /// gauges, the link's queue and what the viewer read. A second is loaded
    /// when the link's queue never fell under 500 ms in it. The hypothesis,
    /// asserted last in each deep cell: fewer than half the loaded seconds
    /// read backlogged. Measured on Windows, debug build; four runs gave the
    /// same classes, and peaks within 1 %:
    ///
    /// | deep cell | loaded seconds | backlogged | link queue, peak | |
    /// |---|---|---|---|---|
    /// | 0.4 Mbit/s, path I | 19 | 0 | 11.06 s | green |
    /// | 2 Mbit/s, path I, skipping over 256 KiB as the VP9-444 pump does | 19 | 0 | 1.06 s | green |
    /// | 2 Mbit/s, path I | 19 | 12 | 4.23 s | red: the peer's window binds at 8 s |
    /// | 0.4 Mbit/s, path C | 12 | 12 | 0.92 s | red |
    /// | 2 Mbit/s, path C, with the skip or without | 0 | 19 of all 20 | 0.37 s | red: never loaded |
    ///
    /// Path I's slow start runs to the peer's window, and its congestion
    /// window grows only while chunks wait, so it stays a few KB over what is
    /// in flight, and the queue of unsent chunks runs empty 20 to 30 times a
    /// second: every sample is application-limited while seconds of queue
    /// stand on the link, until the peer's window binds. The pump's skip
    /// keeps the buffered amount near 256 KiB, so the window never binds, the
    /// classifier never sees the queue, and the queue stands still at a
    /// second, where the viewer's transit growth stops reading it after 3 s.
    /// The governor's own class for a window's delivery (writes over 1.1
    /// times what arrived) reads 2 of that cell's 19 windows, because the
    /// skip writes what the path drains. Path C runs congestion avoidance
    /// from the first byte (no INIT ACK sets its threshold), one MTU a round
    /// trip, so the backlog waits in SCTP's own queue, and the classifier
    /// reads it.
    ///
    /// The candidate signals over the 69 loaded seconds of the four deep cells
    /// that had any, and the 80 seconds of the four controls (0.4 and 2
    /// Mbit/s, paths I and C, 11 to 69 tail drops each), the least and the
    /// most of four runs:
    ///
    /// | signal | deep, loaded | controls (false) |
    /// |---|---|---|
    /// | SCTP's acknowledgement delay over its least ≥ 250 ms | 69 | 0 (at most 65 ms) |
    /// | in flight ≥ 3 × delivered × the least delay | 69 | 7 to 11 |
    /// | the viewer's transit over its least ≥ 200 ms (FR-71's) | 69 | 35 to 37 |
    /// | the stream's buffered amount over 256 KiB | 38 to 40 | 0 |
    /// | transit growth ≥ 100 ms | 35 | 19 to 23 |
    /// | the link loop congested | 32 | 6 to 8 |
    /// | backlogged | 24 | 76 |
    ///
    /// The acknowledgement delay holds only the link's queue. The viewer's
    /// transit also holds the wait in SCTP's own queue (128 KiB, 2.7 s at 0.4
    /// Mbit/s), so it reads the controls' backlog as well. The deep cells
    /// assert the first row on every loaded second and the controls assert
    /// its absence: pins of these numbers, not fail-first.
    ///
    /// The stall cells hold the viewer's deliveries for 0.8 s every 3.3 s, as
    /// a TCP receiver holds what waits behind a lost segment, and fold the
    /// viewer's stats windows (closed as `rc-dc-worker.ts` closes them) into
    /// a real [`Pipe`], whose floor takes every window. The window after a
    /// stall carries the stall's bytes on top of its own. Under a writer at
    /// 80 % of the link, path I's floor ends at 2,857 to 3,030 kbit/s in six
    /// runs, 1.46 to 1.55 times the link's 1,954, by where the stalls fall in
    /// the windows; path C's congestion window caps what the link carries in
    /// a stall, and its floor ends at 2,475 to 2,505 in five runs, 1.27 to
    /// 1.28 times (red at 20 s: it rises stall by stall, 2,090 to 2,501 over
    /// five, as the window grows). Under the pump's skip at 1.5 times the
    /// link, the standing queue already holds what a stall carries:
    /// each window after a stall reads 3,363 to 3,476, and the floor ends at
    /// 3,470 to 3,476 in three runs, 1.78 times the link, about 1 plus the
    /// stall over the window. No capacity is taken in any stall cell. The
    /// controls' windows can pass the link too, by SCTP's own release of what
    /// waited behind a loss: 695 to 697 kbit/s on 0.4 Mbit/s's 390, and up to
    /// 3,389 on 2 Mbit/s's 1,954. There the fold overstates the product's
    /// floor: it leaves out the `Overproduced` screen, by which a window with
    /// a blocked send is valid and its goodput damps the floor.
    ///
    /// FR-148 D46 B1 — the `the_budget_*` cells run a shape under today's
    /// writers and then under the product's in-flight budget
    /// ([`crate::media_inflight::Budget`]), which holds a frame while what
    /// SCTP has out stands at `rate × (least + S)`. Each run prints a `D46 |`
    /// line: the link's queue, each second's least transit over the fastest a
    /// frame can go, what the viewer read, and what it read fresh, within 300
    /// ms of the fastest. `docs/rate-control.md` (§ The in-flight budget)
    /// holds three runs' numbers. The deep cells assert the design's bar on
    /// the budget's run, and the controls that its viewer reads 0.95 of
    /// today's or more. `the_inflight_budget_in_shadow_holds_nothing_on_a_shaped_link`
    /// runs the plain writer off and then with the budget in shadow, the
    /// pumps' B1 path: a write at every tick in both.
    ///
    /// `cargo test -p roomlerd --lib media_delivery::shaped_link::tcp_shape --
    /// --skip clipboard:: --nocapture --test-threads=1 --include-ignored`
    mod tcp_shape {
        use tokio::time::MissedTickBehavior;

        use super::*;
        use crate::encode::goodput::{GoodputEstimator, GoodputSink};
        use crate::encode::pair_memory::{Closing, Take};
        use crate::encode::pipe::{Basis, PathRate, Pipe};
        use crate::encode::pipe_state::PipeState;
        use crate::encode::viewer_rate::{LinkLoop, OfferLoop, offered_over_arrival};
        use crate::media_inflight::{
            BURST_WITHIN, Budget, Classifier, Mode, Reading, Release, Verdict, WindowReport,
        };

        /// The lab slice's round trip.
        const RTT: Duration = Duration::from_millis(60);
        /// A queue no run fills: nothing is dropped.
        const DEEP: Duration = Duration::from_secs(60);
        /// The control's queue, dropping at its tail.
        const SHALLOW: Duration = Duration::from_millis(100);
        const SAMPLE: Duration = Duration::from_secs(1);
        const SAMPLES: u32 = 20;
        const FPS: u32 = 30;
        /// How often a second's link queue is read, for its least and most.
        const POLL: Duration = Duration::from_millis(10);
        /// A second whose link queue never fell under this is loaded.
        const LOADED_MS: u64 = 500;
        /// The candidate signals' thresholds: the second's least
        /// acknowledgement delay over the least yet; what is in flight over
        /// what the second's delivery rate fills in that least delay.
        const RTT_OVER_MS: u64 = 250;
        const BDP_PCT: u64 = 300;
        /// FR-71's transit slack (`pipe_state::TRANSIT_SLACK_MS`), and FR-59's
        /// growth (`viewer_rate::QUEUE_GROWTH_MS`).
        const TRANSIT_OVER_MS: u64 = 200;
        const GROWTH_MS: i64 = 100;
        /// The VP9-444 pump's relay watermark on `dc.buffered_amount()`: over
        /// it, a frame is skipped and a keyframe asked for (`peer.rs`).
        const OVERFLOW: u64 = 256 * 1024;
        /// What one full packet carries: 1,200 bytes of payload in 1,228.
        const PAYLOAD: u64 = 1_200;
        const PACKET: u64 = 1_228;
        /// The stall cells' head-of-line stalls: one this long this often, so
        /// that they fall at different points of the viewer's ≈1 s windows.
        /// Only a stall across a window's close lifts a window: at 4 s, one
        /// run of two had none, and its floor ended at 1.08 times the link.
        const STALL_EVERY: Duration = Duration::from_millis(3_300);
        const STALL_FOR: Duration = Duration::from_millis(800);
        /// FR-148 P0 — the still screen's message, one a second, and its
        /// cells' length: five of the sampler's samples.
        const SPARSE_LEN: usize = 200;
        const SPARSE_SECONDS: u32 = 10;
        /// FR-148 D46 — fresh delivery: a message read within this of the
        /// fastest its frame can go ([`fastest_ms`]).
        const FRESH_MS: u64 = 300;
        /// The slack the budget cells run at: D46's S.
        const SLACK: Duration = Duration::from_millis(150);

        /// FR-148 D46 — the in-flight budget on the writer.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Budgeted {
            No,
            /// The pumps' B1: computed once a frame through the product's
            /// [`Shadow`], counted, and holding nothing.
            Shadow,
            /// The budget's verdict holds the frame, at this slack.
            Holds(Duration),
            /// FR-148 D46 — as `Holds`, B1's: no still span writes through.
            HoldsAsB1(Duration),
            /// FR-148 D46 — as `Holds`, but a still span writes through only
            /// after two head-of-line releases in a row.
            HoldsAfterTwo(Duration),
            /// FR-148 D46 B3 — the pumps' own [`Shadow`] under `on`, on a
            /// relay session: its gate's pass holds the frame
            /// ([`Shadow::gate_with`], [`Shadow::shut`]).
            On,
        }

        /// One cell at a time: the cells are timed.
        static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

        #[derive(Debug, Clone, Copy)]
        struct Shape {
            bps: u64,
            queue: Duration,
            path: OpenPath,
            /// The writer's offer, percent of the link's rate.
            offer_pct: u64,
            /// The writer skips a frame while the stream's buffered amount is
            /// over [`OVERFLOW`], as the VP9-444 pump does on a relay.
            skip_over: bool,
            /// FR-148 D46 — the in-flight budget: none, in shadow, or holding.
            budget: Budgeted,
            /// Head-of-line stalls on the agent's direction: how often, how long.
            stalls: Option<(Duration, Duration)>,
            /// FR-148 D46 — outages of the whole link ([`Outages`]): how often,
            /// how long.
            outages: Option<(Duration, Duration)>,
            /// FR-148 P0 — a still screen: one [`SPARSE_LEN`] message a second
            /// instead of the frames.
            sparse: bool,
            /// The run's length, in one-second rows.
            seconds: u32,
            /// FR-148 D46, the drop step — the agent's direction falls to this
            /// rate this long after the run's start ([`stepped`]).
            step: Option<(Duration, u64)>,
            /// FR-148 D48 — the link's round trip: [`RTT`], or a longer one.
            rtt: Duration,
            /// FR-148 D48 — a viewer that reads slower than the link: its read
            /// rate, bit/s of payload, through a receive window of this many
            /// bytes. `None` reads each message as it arrives, through the
            /// vendored default window (1 MiB).
            viewer: Option<(u64, u32)>,
        }

        impl Shape {
            fn name(&self) -> String {
                let path = match self.path {
                    OpenPath::InitAck => "I",
                    OpenPath::CookieEcho => "C",
                };
                let queue = if self.queue >= DEEP {
                    "deep"
                } else {
                    "shallow"
                };
                let skip = if self.skip_over {
                    ", skip over 256 KiB"
                } else {
                    ""
                };
                let skip = match self.budget {
                    Budgeted::No => skip.to_owned(),
                    Budgeted::Shadow => format!("{skip}, in-flight budget in shadow"),
                    Budgeted::Holds(slack) => {
                        format!("{skip}, in-flight budget, slack {} ms", ms(slack))
                    }
                    Budgeted::HoldsAsB1(slack) => {
                        format!("{skip}, in-flight budget as B1, slack {} ms", ms(slack))
                    }
                    Budgeted::HoldsAfterTwo(slack) => format!(
                        "{skip}, in-flight budget after two releases, slack {} ms",
                        ms(slack)
                    ),
                    Budgeted::On => format!("{skip}, in-flight budget on (B3's gate)"),
                };
                let mut stalls = match (self.stalls, self.outages, self.step) {
                    (Some((every, stall)), _, _) => format!(", a {stall:?} stall every {every:?}"),
                    (None, Some((every, len)), _) => format!(", a {len:?} outage every {every:?}"),
                    (None, None, Some((at, to))) => {
                        format!(", falling to {} Mbit/s at {at:?}", to as f64 / 1e6)
                    }
                    (None, None, None) => String::new(),
                };
                if self.rtt != RTT {
                    stalls.push_str(&format!(", a {} ms round trip", ms(self.rtt)));
                }
                if let Some((bps, window)) = self.viewer {
                    stalls.push_str(&format!(
                        ", the viewer reading {} kbit/s through {} KiB",
                        bps / 1000,
                        window / 1024
                    ));
                }
                if self.sparse {
                    return format!(
                        "{} Mbit/s, {queue} queue, path {path}, one {SPARSE_LEN}-byte message a second",
                        self.bps as f64 / 1e6
                    );
                }
                format!(
                    "{} Mbit/s, {queue} queue, path {path}, offer {} %{skip}{stalls}",
                    self.bps as f64 / 1e6,
                    self.offer_pct
                )
            }

            /// The link's payload rate: after the fall, on a link that falls;
            /// FR-148 D48: the viewer's read rate, where the viewer reads
            /// slower than the link and is the session's limit.
            fn payload_bps(&self) -> u64 {
                if let Some((bps, _)) = self.viewer {
                    return bps;
                }
                self.step.map_or(self.bps, |(_, to)| to) * PAYLOAD / PACKET
            }

            fn frame_len(&self) -> usize {
                usize::try_from(self.bps * self.offer_pct / 100 / 8 / u64::from(FPS)).unwrap()
            }

            /// What the writer offers, bit/s: the budget's opening rate, as
            /// the pump's is the rate its encoder was opened at.
            fn offer_bps(&self) -> u64 {
                self.bps * self.offer_pct / 100
            }
        }

        /// The least a message of `shape`'s can take, write to read, ms: the
        /// one-way delay and its packets on the link. A run's own least is no
        /// base: under the budget the queue stands from the first second.
        fn fastest_ms(shape: &Shape) -> u64 {
            let len = if shape.sparse {
                SPARSE_LEN as u64
            } else {
                shape.frame_len() as u64
            };
            let wire = len + len.div_ceil(PAYLOAD) * (PACKET - PAYLOAD);
            ms(shape.rtt / 2) + wire * 8_000 / shape.bps
        }

        /// FR-148 D46 — the writer's core: the pumps' shadow, or the core
        /// itself where its verdict holds the frame, or (B3) the pumps'
        /// shadow under `on`, whose gate holds it.
        #[derive(Debug)]
        enum Core {
            Shadow(Box<Shadow>),
            Holds(Box<Budget>),
            On(Box<Shadow>),
        }

        impl Core {
            fn of(budget: Budgeted) -> Option<Self> {
                match budget {
                    Budgeted::No => None,
                    Budgeted::Shadow => Some(Self::Shadow(Box::default())),
                    Budgeted::On => Shadow::for_pump(Mode::On, true).map(|s| Self::On(Box::new(s))),
                    Budgeted::Holds(slack) => {
                        Some(Self::Holds(Box::new(Budget::with_slack(slack))))
                    }
                    Budgeted::HoldsAsB1(slack) => {
                        let never = Classifier {
                            releases: u32::MAX,
                            ..Classifier::default()
                        };
                        Some(Self::Holds(Box::new(Budget::with_classifier(slack, never))))
                    }
                    Budgeted::HoldsAfterTwo(slack) => {
                        let two = Classifier {
                            releases: 2,
                            ..Classifier::default()
                        };
                        Some(Self::Holds(Box::new(Budget::with_classifier(slack, two))))
                    }
                }
            }

            fn budget(&self) -> &Budget {
                match self {
                    Self::Shadow(s) | Self::On(s) => s.budget(),
                    Self::Holds(b) => b,
                }
            }

            fn take_window(&mut self) -> WindowReport {
                match self {
                    Self::Shadow(s) | Self::On(s) => s.take_window(),
                    Self::Holds(b) => b.take_window(),
                }
            }
        }

        /// The writer's counts.
        #[derive(Debug, Default)]
        struct WriterStats {
            /// Ticks, and frames written.
            ticks: AtomicU64,
            written: AtomicU64,
            /// Frames the budget held, and frames skipped over the watermark.
            held: AtomicU64,
            skipped: AtomicU64,
            /// Frames the budget let go because SCTP's own window bound with
            /// nothing in its queue.
            window: AtomicU64,
            /// The budget at the last frame, bytes.
            budget: AtomicU64,
            /// Spans, µs from the run's start, in which the acknowledgement
            /// clock stood still for [`Budget::still_after`] or more while
            /// bytes were out.
            silent: StdMutex<Vec<(u64, u64)>>,
            /// The budget's step with its reads: steps, their nanoseconds,
            /// the longest.
            steps: AtomicU64,
            step_ns: AtomicU64,
            step_max_ns: AtomicU64,
            /// FR-148 D46 — frames a still span's write-through let go, and
            /// a holding budget's steps as it saw them.
            through: AtomicU64,
            trace: StdMutex<Vec<Step>>,
            /// FR-148 D46 B2 — the steps that marked a stall for the shadow
            /// model's gate (`Budget::take_mark`), µs from the run's start.
            marks: StdMutex<Vec<u64>>,
        }

        /// FR-148 D46 — one step of a holding budget: its time (µs from the
        /// run's start), the reading, and what the core made of it.
        #[derive(Debug, Clone, Copy)]
        struct Step {
            t_us: u64,
            acked: u64,
            inflight: u64,
            pending: u64,
            verdict: Verdict,
            rate_bps: u64,
            budget: u64,
            still_after_us: u64,
            release: Option<Release>,
            /// FR-148 D46 B2 — the opening's look: whether this association's
            /// figures gave the step's rate (`Budget::rate_measured`), whether
            /// SCTP's window and not the rate sized its budget
            /// (`Budget::window_sized`), and SCTP's window, bytes.
            measured: bool,
            window_sized: bool,
            cwnd: u32,
            /// FR-148 D48 — the viewer's receive window SCTP last heard of,
            /// bytes: with `inflight`, what says whether the congestion
            /// window or the receive window sized a window-sized hold.
            rwnd: u32,
            /// FR-148 D46, the drop step — SCTP's slow start threshold, and
            /// its fast retransmits and T3-rtx expiries since the association
            /// began, at the step's peek.
            ssthresh: u32,
            fast_rtx: u64,
            t3: u64,
        }

        impl Step {
            /// FR-148 D48, rule 4 — a hold SCTP's congestion window sized: a
            /// window-sized hold (`by_window < rate`) whose window was the
            /// congestion window, `cwnd <= rwnd + inflight`. One the receive
            /// window sized is the viewer's, and does not count.
            fn ramp_held(&self) -> bool {
                self.verdict.holds()
                    && self.window_sized
                    && u64::from(self.cwnd) <= u64::from(self.rwnd).saturating_add(self.inflight)
            }
        }

        /// One message the viewer read: when it arrived and when it was
        /// written (microseconds from the cell's start), and its length.
        #[derive(Debug, Clone, Copy)]
        struct Arrival {
            at_us: u64,
            sent_us: u64,
            len: u64,
        }

        fn transit_ms(x: &Arrival) -> u64 {
            x.at_us.saturating_sub(x.sent_us) / 1000
        }

        /// One second of a run, as the agent and the viewer saw it.
        #[derive(Debug, Clone, Copy)]
        struct Second {
            t: u32,
            class: Class,
            /// What SCTP saw acknowledged, bit/s.
            acked_bps: u64,
            /// The least and the most a packet would have waited in the link's
            /// queue during the second.
            q_lo_ms: u64,
            q_hi_ms: u64,
            inflight: u64,
            cwnd: u32,
            ssthresh: u32,
            rwnd: u32,
            pending: u64,
            /// Times the queue of written, unsent chunks ran empty.
            empties: u64,
            /// The stream's buffered amount: written, not yet acknowledged.
            buffered: u64,
            /// The second's least acknowledgement delay.
            ack_min_ms: Option<u64>,
            /// What the writer wrote and what the viewer read, bit/s.
            sent_bps: u64,
            rx_bps: u64,
            /// The newest arrival's transit, write to read, and its growth
            /// since the second before's.
            transit_ms: Option<u64>,
            growth_ms: Option<i64>,
            /// The writer's blocked sends (`goodput::MIN_BLOCKED_SEND` each),
            /// and the goodput estimate when the window qualified.
            blocked_ms: u64,
            goodput_bps: Option<u32>,
            fast_rtx: u64,
            t3: u64,
            /// FR-148 D46 — the frames the writer held or skipped in the
            /// second, the budget at its end (0 without one), the least
            /// transit of the second's arrivals, and what arrived fresh
            /// ([`FRESH_MS`]), bit/s.
            held: u64,
            budget: u64,
            transit_lo_ms: Option<u64>,
            fresh_bps: u64,
        }

        /// The candidate signals of a standing queue for one second, each
        /// against the least seen so far, as a live reader would hold it.
        #[derive(Debug, Clone, Copy)]
        struct Signals {
            bdp_pct: Option<u64>,
            rtt_over_ms: Option<u64>,
            transit_over_ms: Option<u64>,
        }

        fn signals_of(rows: &[Second]) -> Vec<Signals> {
            let mut rtt_min: Option<u64> = None;
            let mut transit_min: Option<u64> = None;
            rows.iter()
                .map(|r| {
                    rtt_min = match (rtt_min, r.ack_min_ms) {
                        (Some(m), Some(a)) => Some(m.min(a)),
                        (m, a) => m.or(a),
                    };
                    transit_min = match (transit_min, r.transit_ms) {
                        (Some(m), Some(t)) => Some(m.min(t)),
                        (m, t) => m.or(t),
                    };
                    let bdp_pct = rtt_min
                        .filter(|&m| m > 0 && r.acked_bps > 0)
                        .map(|m| r.inflight * 100 * 8_000 / (r.acked_bps * m));
                    Signals {
                        bdp_pct,
                        rtt_over_ms: r.ack_min_ms.zip(rtt_min).map(|(a, m)| a - m),
                        transit_over_ms: r.transit_ms.zip(transit_min).map(|(t, m)| t - m),
                    }
                })
                .collect()
        }

        /// One stats window of the viewer's, as `rc-dc-worker.ts` closes it:
        /// at the first arrival a second or more after the last close, that
        /// arrival's bytes counted in the window it closes.
        #[derive(Debug, Clone, Copy)]
        struct ViewerWindow {
            end_us: u64,
            ms: u64,
            rx_bps: u32,
            sent_bps: u32,
            /// The transit queue's growth over the window (the worker's
            /// `QueueDrift`, read at arrival), its mean transit, and that over
            /// the least window mean yet.
            growth_ms: i64,
            transit_ms: u64,
            excess_ms: u64,
        }

        fn viewer_windows(
            arrivals: &[Arrival],
            frames: &[(u64, u64)],
            from_us: u64,
        ) -> Vec<ViewerWindow> {
            let mut out = Vec::new();
            let mut opened: Option<(u64, u64)> = None;
            let (mut bytes, mut transit_sum, mut n) = (0u64, 0u64, 0u64);
            let mut floor: Option<u64> = None;
            for x in arrivals.iter().filter(|x| x.at_us >= from_us) {
                let Some((since, transit_before)) = opened else {
                    opened = Some((x.at_us, transit_ms(x)));
                    continue;
                };
                bytes += x.len;
                transit_sum += transit_ms(x);
                n += 1;
                let elapsed = x.at_us - since;
                if elapsed < 1_000_000 {
                    continue;
                }
                let sent: u64 = frames
                    .iter()
                    .filter(|f| f.0 > since && f.0 <= x.at_us)
                    .map(|f| f.1)
                    .sum();
                let mean = transit_sum / n;
                let low = floor.map_or(mean, |f| f.min(mean));
                floor = Some(low);
                let bps = |b: u64| u32::try_from(b * 8_000_000 / elapsed).unwrap_or(u32::MAX);
                out.push(ViewerWindow {
                    end_us: x.at_us,
                    ms: elapsed / 1000,
                    rx_bps: bps(bytes),
                    sent_bps: bps(sent),
                    growth_ms: transit_ms(x) as i64 - transit_before as i64,
                    transit_ms: mean,
                    excess_ms: mean - low,
                });
                opened = Some((x.at_us, transit_ms(x)));
                (bytes, transit_sum, n) = (0, 0, 0);
            }
            out
        }

        /// The session's path belief over a run's viewer windows, folded by
        /// the real [`Pipe`] the way the governor folds it: every window's
        /// arrival rate is a delivery; a valid window's goodput, and a valid,
        /// congested and offered window's arrival rate, are capacities. Valid
        /// is FR-79's gate cut to its transit rule: a window whose transit is
        /// [`TRANSIT_OVER_MS`] over the least is `TransitStalled`, and the
        /// window after one is its shadow. The sender-side screen that would
        /// call a window `Overproduced` first is not modelled.
        #[derive(Debug, Default, Clone, Copy)]
        struct Belief {
            floor: Option<u32>,
            capacity: Option<u32>,
            anchor: Option<u32>,
            max_rx: u32,
            windows: u32,
            /// Windows whose writes ran over what arrived
            /// ([`offered_over_arrival`]): the class the governor gives a
            /// window's delivery, and so the shadow model's.
            offered: u32,
            invalid: u32,
            congested: u32,
            link_capacities: u32,
            goodput_capacities: u32,
        }

        /// FR-148 D46 — `pushbacks` are capacities at their times (µs from
        /// `t0`), folded as backlogged push-backs whatever the window: a
        /// standing queue's push-back on the floor.
        fn belief_of(
            windows: &[ViewerWindow],
            goodput: &[(u64, u32)],
            t0: Instant,
            pushbacks: &[(u64, u32)],
        ) -> Belief {
            let mut pipe = Pipe::default();
            let mut link = LinkLoop::new();
            let mut offer = OfferLoop::default();
            let mut goodput = goodput.iter().peekable();
            let mut pushbacks = pushbacks.iter().peekable();
            let mut prev_stalled = false;
            let mut b = Belief::default();
            for w in windows {
                let at = t0 + Duration::from_micros(w.end_us);
                while let Some(&(end_us, bps)) = pushbacks.next_if(|p| p.0 <= w.end_us) {
                    let at = t0 + Duration::from_micros(end_us);
                    pipe.observe_capacity(bps, Basis::Backlogged, at);
                }
                let stalled = w.excess_ms >= TRANSIT_OVER_MS;
                let valid = !stalled && !prev_stalled;
                prev_stalled = stalled;
                b.windows += 1;
                if !valid {
                    b.invalid += 1;
                }
                while let Some(&(_, bps)) = goodput.next_if(|g| g.0 <= w.end_us) {
                    if valid {
                        pipe.observe_capacity(bps, Basis::Backlogged, at);
                        b.goodput_capacities += 1;
                    }
                }
                let offered = offered_over_arrival(w.sent_bps, w.rx_bps);
                if offered {
                    b.offered += 1;
                }
                let offer_held = offer.observe(offered);
                pipe.observe_delivered_in(w.rx_bps, Basis::of_window(offered), valid, at);
                let growth = i16::try_from(w.growth_ms.clamp(-30_000, 30_000)).unwrap();
                if link.observe(w.rx_bps, growth).congested {
                    b.congested += 1;
                    if valid && offer_held {
                        pipe.observe_capacity(w.rx_bps, Basis::Backlogged, at);
                        b.link_capacities += 1;
                    }
                }
                b.max_rx = b.max_rx.max(w.rx_bps);
            }
            for &(end_us, bps) in pushbacks {
                pipe.observe_capacity(bps, Basis::Backlogged, t0 + Duration::from_micros(end_us));
            }
            let end = windows
                .last()
                .map_or(t0, |w| t0 + Duration::from_micros(w.end_us));
            b.floor = pipe.floor_bps(end);
            b.capacity = pipe.capacity_bps(end);
            b.anchor = pipe.ceiling_anchor_bps(end);
            b
        }

        /// FR-148 D46 B2 — the shadow model's fold of a run, in the
        /// governor's order: each window's delivery (D24's gate, and the
        /// [`STALL_WINDOWS`](crate::encode::pipe::STALL_WINDOWS) gate after
        /// the core's stall marks, `gate` of them here), then its goodput and
        /// link push-backs; and each sample's push-back ([`push_back_of`]
        /// under `burst`) when the sampler hands it ([`handed`]), filed under
        /// the last valid window's take, as `RateGovernor::on_sample_push_back`
        /// files it. A window's
        /// take is `Overproduced` when the writer held, skipped or blocked in
        /// a second it covers, else `Clear`: the classifier's sender-side
        /// screen, cut to what the harness counts.
        #[derive(Debug, Clone, Copy)]
        struct ModelFold {
            floor: Option<u32>,
            capacity: Option<u32>,
            /// The fold's own law, `Pipe::floor_bps`, in the governor's order.
            own_floor: Option<u32>,
            /// P1's and P3's push-backs: made, taken (a valid window before
            /// them), and taken above the model's floor then (each a raise
            /// under a window push-back's law, none under B2's).
            made: [u32; 2],
            taken: [u32; 2],
            above: u32,
            /// Windows the stall gate kept out, and the most a window the
            /// model took delivered.
            gated: u32,
            max_taken_rx: u32,
            closing: Closing,
            /// What last set the model's capacity, while it stands at the
            /// run's end: `goodput` or `link` (a window's push-back, P0's law)
            /// or `p1` / `p3` (a sample's), when (µs from `t0`), and its rate.
            cap_by: Option<(&'static str, u64, u32)>,
            /// FR-148 D50 — the falls the model read: none on a fold without
            /// the clock.
            falls: usize,
        }

        /// Whether the writer held, skipped or blocked in a second `w` covers.
        fn sender_pushed(r: &TcpRun, w: &ViewerWindow) -> bool {
            let from = w.end_us.saturating_sub(w.ms * 1_000);
            r.rows.iter().zip(0_u64..).any(|(s, i)| {
                let a = r.start_us + i * 1_000_000;
                a < w.end_us && a + 1_000_000 > from && (s.held > 0 || s.blocked_ms > 0)
            })
        }

        /// FR-148 D46 B2 — the opening rules measured: what makes a sample
        /// the opening's ([`Over::opening`]), so no P3 capacity.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Open {
            /// #4115's first commit: nothing is.
            None,
            /// (a) a hold in the sample came at the opening's stand-in rate.
            StandIn,
            /// (b) a hold came at the stand-in, or while SCTP's window and not
            /// the rate sized the budget.
            Window,
            /// (c) the sample starts before the budget's first measured rate:
            /// the rule built (`Budget::over`).
            Start,
            /// (c), or a hold that SCTP's window sized.
            StartOrWindow,
            /// The session's first sample, whatever the budget saw.
            First,
        }

        /// The opening rules, by the word the lines print.
        const OPENS: [(&str, Open); 6] = [
            ("n", Open::None),
            ("a", Open::StandIn),
            ("b", Open::Window),
            ("c", Open::Start),
            ("cw", Open::StartOrWindow),
            ("f", Open::First),
        ];

        /// FR-148 D46 B2 — the holding core's steps over a sample's span,
        /// from the trace: steps, holds, the holds at the opening's stand-in
        /// rate and those SCTP's window sized, of them those its congestion
        /// window sized (`ramp`, FR-148 D48's rule 4), and SCTP's window at
        /// its least and most. All 0 for a core that leaves no trace (the
        /// shadow).
        #[derive(Debug, Clone, Copy, Default)]
        struct Holds {
            steps: u32,
            held: u32,
            stand_in: u32,
            window: u32,
            ramp: u32,
            cwnd: (u32, u32),
        }

        fn holds_over(r: &TcpRun, c: &Covered) -> Holds {
            let from = c.end_us.saturating_sub(c.sample.ms * 1_000);
            let mut h = Holds::default();
            for s in r
                .trace
                .iter()
                .filter(|s| (from..=c.end_us).contains(&s.t_us))
            {
                h.cwnd = if h.steps == 0 {
                    (s.cwnd, s.cwnd)
                } else {
                    (h.cwnd.0.min(s.cwnd), h.cwnd.1.max(s.cwnd))
                };
                h.steps += 1;
                if s.verdict.holds() {
                    h.held += 1;
                    h.stand_in += u32::from(!s.measured);
                    h.window += u32::from(s.window_sized);
                    h.ramp += u32::from(s.ramp_held());
                }
            }
            h
        }

        /// Whether `open` makes the run's sample `i` the opening's.
        fn opening_of(r: &TcpRun, i: usize, open: Open) -> bool {
            let c = &r.samples[i];
            let Some(o) = c.over else {
                return false;
            };
            let h = holds_over(r, c);
            match open {
                Open::None => false,
                Open::StandIn => h.stand_in > 0,
                Open::Window => h.stand_in > 0 || h.window > 0,
                Open::Start => o.opening,
                Open::StartOrWindow => o.opening || h.window > 0,
                Open::First => i == 0,
            }
        }

        /// FR-148 D48 — the rule sets a run is folded under: B2's (`c`), the
        /// drop step's first pick (`rr+bm0+u90+l`: rules 1 to 3, the product
        /// without rule 4), D48's (`d48`: rules 1 to 4) and the product's
        /// (`d50`), each on the budget's [`Over`] as the product reads it,
        /// B2's `held` share included. The product's own code applies each: a
        /// sample's read through [`sample_read_of`] on the budget's [`Over`],
        /// and a window's through `Pipe`'s window rules, which read the
        /// budget's rate as the fold notes it before each window's close
        /// ([`Pipe::note_path_rate`]). FR-148 D50: only the product's fold
        /// passes the shadow model the clock a fall is read against; the
        /// others fold the law before D50 ([`Pipe::set_unclocked`]).
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Rules {
            /// B2 as built: no D48 rule.
            B2,
            /// Rules 1 to 3.
            Pick,
            /// D48: rules 1 to 4, on the law before D50.
            D48,
            /// The product: rules 1 to 4, and D50's fall event.
            D50,
        }

        impl Rules {
            /// What a sample's read takes of the budget's `Over`: B2 knows
            /// neither the budget's rate nor the ramp, the first pick no ramp.
            fn over(self, o: Over) -> Over {
                match self {
                    Rules::B2 => Over {
                        ramp: false,
                        rate: None,
                        ..o
                    },
                    Rules::Pick => Over { ramp: false, ..o },
                    Rules::D48 | Rules::D50 => o,
                }
            }

            /// Whether the fold notes the budget's rate for `Pipe`'s window
            /// rules (2 and 3).
            fn windows(self) -> bool {
                self != Rules::B2
            }

            /// FR-148 D50 — whether the shadow model reads a fall against the
            /// budget's measured rate: the product's fold only.
            fn clocked(self) -> bool {
                self == Rules::D50
            }
        }

        /// The rule sets, by the word the drop lines print.
        const RULES: [(&str, Rules); 4] = [
            ("c", Rules::B2),
            ("rr+bm0+u90+l", Rules::Pick),
            ("d48", Rules::D48),
            ("d50", Rules::D50),
        ];

        /// FR-148 D46 B2 — when the sampler hands sample `i`'s push-back
        /// under `burst`, `open` and `rules` (µs from `t0`): at its close, or,
        /// under the rule, once its settle has passed. Or why it never does:
        /// rule 4 kept it out (`ramp`), the opening's (`opening`), a still
        /// span met it (`stood`), none under any rule (`none`), a stall mark
        /// inside the wait (`marked`), or the writer stopped inside it
        /// (`ended`), as a session's close drops what its sampler still holds.
        fn handed(
            r: &TcpRun,
            i: usize,
            burst: Burst,
            open: Open,
            rules: Rules,
        ) -> Result<(u64, SamplePushBack), &'static str> {
            let c = &r.samples[i];
            let with =
                |opening: bool, rules: Rules| c.over.map(|o| rules.over(Over { opening, ..o }));
            let over = with(opening_of(r, i, open), rules);
            let p = match sample_read_of(&c.sample, over.as_ref(), burst) {
                Some(SampleRead::Push(p)) => p,
                Some(SampleRead::Ramp) => return Err("ramp"),
                None => {
                    let open_off = with(false, Rules::B2);
                    return Err(
                        if push_back_of(&c.sample, open_off.as_ref(), burst).is_some() {
                            "opening"
                        } else if push_back_of(&c.sample, open_off.as_ref(), Burst::Own).is_some() {
                            "stood"
                        } else {
                            "none"
                        },
                    );
                }
            };
            if !burst.waits() {
                return Ok((c.end_us, p));
            }
            let until = c.end_us + c.settle_us;
            if r.marks.iter().any(|&m| m > c.end_us && m < until) {
                return Err("marked");
            }
            if until > r.stop_us {
                return Err("ended");
            }
            Ok((until, p))
        }

        /// The product's fold: its rules and its law (FR-148 D50), under
        /// `burst`, `gate` and `open`.
        fn model_fold(r: &TcpRun, burst: Burst, gate: u8, open: Open) -> ModelFold {
            fold_with(r, burst, gate, open, Rules::D50, &mut Vec::new())
        }

        /// FR-148 D48 — what the budget's step read, as the governor notes it
        /// for `Pipe`'s window rules (`media_delivery::path_rate`): one
        /// association a run, so its loss count is the budget's.
        fn path_rate_at(s: &Step) -> PathRate {
            PathRate {
                bps: (s.rate_bps > 0).then(|| u32::try_from(s.rate_bps).unwrap_or(u32::MAX)),
                measured: s.measured,
                loss: s.fast_rtx + s.t3,
            }
        }

        /// The holding core's last step at or before `t_us`.
        fn step_at(r: &TcpRun, t_us: u64) -> Option<&Step> {
            let i = r.trace.partition_point(|s| s.t_us <= t_us);
            i.checked_sub(1).map(|i| &r.trace[i])
        }

        /// FR-148 D46, the drop step — one change the fold made to the model:
        /// its floor raised or lowered, or its capacity set, by what (`win` a
        /// valid window's delivery, `goodput` or `link` a window's push-back,
        /// `p1` or `p3` a sample's), when (µs from `t0`), at what rate, the
        /// floor before and after, and the window's or sample's index. FR-148
        /// D48: and the rule that re-read the sample or bounded the window
        /// (`reread`, `bound`), or none. FR-148 D50: a push-back the model
        /// read as a fall says so (`fall`, `reread_fall`, `bound_fall`).
        #[derive(Debug, Clone, Copy)]
        struct FoldEvent {
            t_us: u64,
            by: &'static str,
            bps: u32,
            floor: (Option<u32>, Option<u32>),
            capacity: bool,
            of: usize,
            rule: &'static str,
        }

        fn fold_with(
            r: &TcpRun,
            burst: Burst,
            gate: u8,
            open: Open,
            rules: Rules,
            log: &mut Vec<FoldEvent>,
        ) -> ModelFold {
            let mut pipe = Pipe::default();
            pipe.set_stall_gate(gate);
            if !rules.clocked() {
                pipe.set_unclocked();
            }
            // The events this fold logs, from here: its falls are counted.
            let first = log.len();
            let mut link = LinkLoop::new();
            let mut offer = OfferLoop::default();
            let mut goodput = r.goodput_at.iter().peekable();
            let mut marks = r.marks.iter().peekable();
            // Each push-back at the time the sampler hands it; a P3 rule 4
            // keeps out never, and one rule 1 re-reads at its new rate.
            let mut due: Vec<(u64, SamplePushBack, usize)> = (0..r.samples.len())
                .filter_map(|i| {
                    let (t, p) = handed(r, i, burst, open, rules).ok()?;
                    Some((t, p, i))
                })
                .collect();
            due.sort_by_key(|&(t, _, _)| t);
            let mut pushes = due.into_iter().peekable();
            let offer_bps = u32::try_from(r.shape.offer_bps()).unwrap_or(u32::MAX);
            let at = |t_us: u64| r.t0 + Duration::from_micros(t_us);
            let floor_of = |pipe: &Pipe, now: Instant| pipe.model().floor(now).map(|f| f.bps);
            let (mut made, mut taken, mut above) = ([0_u32; 2], [0_u32; 2], 0);
            let mut cap_by = None;
            let mut hand = |pipe: &mut Pipe,
                            take: Option<Take>,
                            (t_us, p, i): (u64, SamplePushBack, usize),
                            log: &mut Vec<FoldEvent>| {
                let now = at(t_us);
                let k = usize::from(p.by == PushedBy::P3);
                made[k] += 1;
                let Some(take) = take else {
                    return;
                };
                taken[k] += 1;
                let before = floor_of(pipe, now);
                if before.is_some_and(|f| p.bps > f) {
                    above += 1;
                }
                let was = pipe.model().fell(now);
                pipe.observe_sample_capacity(p.bps, p.by, take, now);
                let fell = pipe.model().fell(now) != was;
                cap_by = Some((p.by.word(), t_us, p.bps));
                // D46 item 10: a push-back never raises the floor, nor makes one.
                let after = floor_of(pipe, now);
                assert!(
                    after <= before,
                    "{p:?} raised the floor: {before:?} to {after:?}"
                );
                log.push(FoldEvent {
                    t_us,
                    by: p.by.word(),
                    bps: p.bps,
                    floor: (before, after),
                    capacity: true,
                    of: i,
                    rule: match (p.reread, fell) {
                        (true, true) => "reread_fall",
                        (true, false) => "reread",
                        (false, true) => "fall",
                        (false, false) => "",
                    },
                });
            };
            // A window's push-back that the model took sets its capacity now.
            let set_now = |pipe: &Pipe, bps: u32, now: Instant| {
                pipe.model()
                    .capacity(now)
                    .is_some_and(|c| c.bps == bps && c.age.is_zero())
            };
            let (mut take, mut prev_stalled) = (None, false);
            let (mut left, mut gated, mut max_taken_rx) = (0_u8, 0, 0);
            let mut window_cap = None;
            for (j, w) in r.windows.iter().enumerate() {
                let now = at(w.end_us);
                while let Some(d) = pushes.next_if(|(e, _, _)| *e <= w.end_us) {
                    hand(&mut pipe, take, d, log);
                }
                let mut marked = false;
                while marks.next_if(|&&m| m <= w.end_us).is_some() {
                    marked = true;
                }
                if marked {
                    pipe.note_stall();
                    left = gate;
                }
                // FR-148 D48 — the budget's last step before the window's
                // close, as the governor notes it: `Pipe`'s window rules read
                // it there. B2's fold notes nothing, as with the budget off.
                if rules.windows()
                    && let Some(s) = step_at(r, w.end_us)
                {
                    pipe.note_path_rate(path_rate_at(s));
                }
                let stalled = w.excess_ms >= TRANSIT_OVER_MS;
                let valid = !stalled && !prev_stalled;
                prev_stalled = stalled;
                let offered = offered_over_arrival(w.sent_bps, w.rx_bps);
                let offer_held = offer.observe(offered);
                let state = if stalled {
                    PipeState::TransitStalled
                } else if sender_pushed(r, w) {
                    PipeState::Overproduced
                } else {
                    PipeState::Clear
                };
                let t = Take::of(state, true, w.sent_bps, offer_bps);
                let before = floor_of(&pipe, now);
                pipe.observe_delivered_in(w.rx_bps, Basis::of_window(offered), valid, now);
                let after = floor_of(&pipe, now);
                // What the model took of the window's delivery under the rule
                // `Pipe` ruled at its close: at the bound, or none of it.
                let ([taken_rx, _, _], word) = pipe.window_takes(w.rx_bps);
                let rx = taken_rx.unwrap_or(w.rx_bps);
                let rule = if taken_rx == Some(w.rx_bps) { "" } else { word };
                if after != before {
                    log.push(FoldEvent {
                        t_us: w.end_us,
                        by: "win",
                        bps: rx,
                        floor: (before, after),
                        capacity: false,
                        of: j,
                        rule,
                    });
                }
                if w.rx_bps > 0 {
                    let kept_out = left > 0 || taken_rx.is_none();
                    left = left.saturating_sub(1);
                    gated += u32::from(kept_out);
                    if valid && !kept_out {
                        max_taken_rx = max_taken_rx.max(rx);
                    }
                }
                // A window's push-back, through the call the governor makes:
                // the goodput fold's (`k` 1) or the link loop's (`k` 2), which
                // the model takes as `Pipe::window_takes` says.
                let mut push = |pipe: &mut Pipe, by: &'static str, k: usize, bps: u32| {
                    let before = floor_of(pipe, now);
                    let was = pipe.model().fell(now);
                    if k == 2 {
                        pipe.observe_link_capacity_in(bps, Basis::Backlogged, t, now);
                    } else {
                        pipe.observe_capacity_in(bps, Basis::Backlogged, t, now);
                    }
                    let fell = pipe.model().fell(now) != was;
                    let (takes, word) = pipe.window_takes(bps);
                    let rule = match (takes[k] == Some(bps), fell) {
                        (true, false) => "",
                        (true, true) => "fall",
                        (false, false) => word,
                        // Only rule 2's bound takes a push-back at another
                        // rate; a window no rule lets in reads no fall.
                        (false, true) => "bound_fall",
                    };
                    let set = takes[k].is_some_and(|b| set_now(pipe, b, now));
                    let bps = takes[k].unwrap_or(bps);
                    if set {
                        window_cap = Some((by, w.end_us, bps));
                    }
                    let after = floor_of(pipe, now);
                    if set || after != before {
                        log.push(FoldEvent {
                            t_us: w.end_us,
                            by,
                            bps,
                            floor: (before, after),
                            capacity: set,
                            of: j,
                            rule,
                        });
                    }
                };
                while let Some(&(_, bps)) = goodput.next_if(|g| g.0 <= w.end_us) {
                    if valid {
                        push(&mut pipe, "goodput", 1, bps);
                    }
                }
                let growth = i16::try_from(w.growth_ms.clamp(-30_000, 30_000)).unwrap();
                if link.observe(w.rx_bps, growth).congested && valid && offer_held {
                    push(&mut pipe, "link", 2, w.rx_bps);
                }
                take = valid.then_some(t);
            }
            for d in pushes {
                hand(&mut pipe, take, d, log);
            }
            let end = r.windows.last().map_or(r.t0, |w| at(w.end_us));
            let capacity = pipe.model().capacity(end).map(|c| c.bps);
            let cap_by = match (cap_by, window_cap) {
                (Some(s), Some(w)) => Some(if s.1 > w.1 { s } else { w }),
                (s, w) => s.or(w),
            }
            .filter(|by| capacity == Some(by.2));
            ModelFold {
                floor: pipe.model().floor(end).map(|f| f.bps),
                capacity,
                own_floor: pipe.floor_bps(end),
                made,
                taken,
                above,
                gated,
                max_taken_rx,
                closing: pipe.memory().closing(end, 0),
                cap_by,
                falls: log[first..]
                    .iter()
                    .filter(|e| e.rule.ends_with("fall"))
                    .count(),
            }
        }

        /// FR-148 D46 B2 — one run's shadow-model floor, as a share of the
        /// link's payload rate, under each burst rule (`e` the rule, `c` the
        /// classifier's correction, `o` none) at 0 to 3 gated windows after a
        /// stall mark, with the opening rule built; then the rule at [`STALL_WINDOWS`](crate::encode::pipe::STALL_WINDOWS)
        /// in full: the push-backs, the model's capacity, the memory's
        /// counted samples and their split, and the most a taken window read;
        /// then each opening rule's floor and capacity under the burst rule
        /// built ([`OPENS`]), and what last set the capacity. The rules that
        /// read the holding core's trace print `?` for the shadow, which
        /// leaves none. FR-148 D48: each fold is the product's, its four
        /// rules in ([`model_fold`]); the samples listed are B2's push-backs,
        /// each with what the product made of it.
        fn print_b2(r: &TcpRun) {
            let link = r.shape.payload_bps() as f64;
            let x = |f: Option<u32>| {
                f.map_or_else(|| "-".to_owned(), |f| format!("{:.3}", f64::from(f) / link))
            };
            let gate = crate::encode::pipe::STALL_WINDOWS;
            let mut sweep = Vec::new();
            for (word, burst) in [
                ("e", Burst::Exclude),
                ("s", Burst::Stood),
                ("c", Burst::Correct),
                ("o", Burst::Own),
            ] {
                for g in 0..=3_u8 {
                    sweep.push(format!(
                        "{word}{g}={}",
                        x(model_fold(r, burst, g, Open::Start).floor)
                    ));
                }
            }
            let traced = !r.trace.is_empty() || r.samples.iter().all(|c| c.over.is_none());
            let known = |open: Open| {
                traced || !matches!(open, Open::StandIn | Open::Window | Open::StartOrWindow)
            };
            let opens: Vec<String> = OPENS
                .iter()
                .map(|&(word, open)| {
                    if !known(open) {
                        return format!("{word}=?");
                    }
                    let m = model_fold(r, BURST, gate, open);
                    format!("{word}={}/{}", x(m.floor), x(m.capacity))
                })
                .collect();
            let fates = |i: usize| -> String {
                OPENS
                    .iter()
                    .map(|&(word, open)| {
                        let fate = if known(open) {
                            handed(r, i, BURST, open, Rules::D48)
                                .map_or_else(|why| why.to_owned(), |(_, p)| x(Some(p.bps)))
                        } else {
                            "?".to_owned()
                        };
                        format!("{word}={fate}")
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let m = model_fold(r, BURST, gate, Open::Start);
            let c = m.closing;
            let cap_by = m.cap_by.map_or_else(
                || "-".to_owned(),
                |(by, t_us, bps)| format!("{by}@{}ms:{}", t_us / 1_000, x(Some(bps))),
            );
            println!(
                "D46 B2 | {} | link={} own={} | {} | p1={}/{} p3={}/{} above={} | floor={} \
                 cap={} counted={} over={} clear={} p1c={}/{} p3c={}/{} gated={} maxrx={} \
                 marks={} samples={} | open {} capby={}",
                r.shape.name(),
                kbps(r.shape.payload_bps()),
                x(m.own_floor),
                sweep.join(" "),
                m.made[0],
                m.taken[0],
                m.made[1],
                m.taken[1],
                m.above,
                x(m.floor),
                x(m.capacity),
                c.counted,
                c.counted_overproduced,
                c.counted_clear,
                c.counted_p1.0,
                c.counted_p1.1,
                c.counted_p3.0,
                c.counted_p3.1,
                m.gated,
                x(Some(m.max_taken_rx)),
                r.marks.len(),
                r.samples.len(),
                opens.join(" "),
                cap_by,
            );
            // Each sample that is a push-back under no rule but P3's own, the
            // opening's aside: its span, rate and facts, the holding core's
            // steps over it, and what the rule and each opening rule make of
            // it.
            for (i, c) in r.samples.iter().enumerate() {
                let open_off = c.over.map(|o| {
                    Rules::B2.over(Over {
                        opening: false,
                        ..o
                    })
                });
                let Some(p) = push_back_of(&c.sample, open_off.as_ref(), Burst::Own) else {
                    continue;
                };
                let to = c.end_us / 1_000;
                let o = c.over.as_ref();
                let h = holds_over(r, c);
                println!(
                    "D46 B2 sample | {} | {}-{} ms {} {} class={:?} standing={:?} stood={} \
                     inside={} held={} from={:?} opening={} holds={}/{} stand_in={} window={} \
                     ramp={} cwnd={}-{} | rule={} | {}",
                    r.shape.name(),
                    to.saturating_sub(c.sample.ms),
                    to,
                    x(Some(p.bps)),
                    p.by.word(),
                    c.sample.class,
                    c.sample.queue.and_then(|q| q.standing_ms),
                    o.is_some_and(|o| o.stood),
                    o.is_some_and(|o| o.still_inside),
                    o.is_some_and(|o| o.held),
                    o.and_then(|o| o.released_from),
                    o.is_some_and(|o| o.opening),
                    h.held,
                    h.steps,
                    h.stand_in,
                    h.window,
                    h.ramp,
                    h.cwnd.0,
                    h.cwnd.1,
                    handed(r, i, BURST, Open::Start, Rules::D48)
                        .map_or_else(|why| why.to_owned(), |(_, p)| x(Some(p.bps))),
                    fates(i),
                );
            }
            // The session's first sample, under each opening rule.
            if let Some(c) = r.samples.first() {
                let open_off = c.over.map(|o| {
                    Rules::B2.over(Over {
                        opening: false,
                        ..o
                    })
                });
                let p = push_back_of(&c.sample, open_off.as_ref(), Burst::Own);
                let to = c.end_us / 1_000;
                let h = holds_over(r, c);
                println!(
                    "D46 B2 first | {} | {}-{} ms {} {} holds={}/{} stand_in={} window={} | {}",
                    r.shape.name(),
                    to.saturating_sub(c.sample.ms),
                    to,
                    x(p.map(|p| p.bps)),
                    p.map_or("none", |p| p.by.word()),
                    h.held,
                    h.steps,
                    h.stand_in,
                    h.window,
                    fates(0),
                );
            }
        }

        /// FR-148 D48 — B2's `Over::held` against the holding core's trace,
        /// on each sample it read held, B2's edge-grain rule read held, or P3
        /// took: the holds inside the span (the first few, ms after its start)
        /// and in its two edge grains outside it, and the grains wholly inside
        /// it that held, in all and in each of `held`'s stretches (one per
        /// whole second). The grains count from the core's first step, as its
        /// mask's do; `whole` is a hold in any grain wholly inside each
        /// stretch (the edge fix alone), `edge` with the span's two edge grains
        /// too (B2's), and `held` the product's, a share of each stretch's
        /// whole grains (`HELD_SHARE_PERCENT`). P3 is B2's rules on the
        /// product's `held`; `rgrains` counts the held whole grains with a
        /// hold SCTP's congestion window sized (rule 4's fact) against all of
        /// them, and `d48` says what the product made of the sample.
        fn print_held(r: &TcpRun) {
            let Some(epoch) = r.trace.first().map(|s| s.t_us) else {
                return;
            };
            let grain = us(crate::media_inflight::GRAIN);
            let link = r.shape.payload_bps() as f64;
            let name = r.shape.name();
            let all: std::collections::BTreeSet<u64> = r
                .trace
                .iter()
                .filter(|s| s.verdict.holds())
                .map(|s| s.t_us.saturating_sub(epoch) / grain)
                .collect();
            for c in &r.samples {
                let Some(o) = c.over else {
                    continue;
                };
                let p = push_back_of(&c.sample, Some(&Rules::B2.over(o)), BURST);
                let d48 = match sample_read_of(&c.sample, Some(&o), BURST) {
                    Some(SampleRead::Ramp) => "ramp",
                    Some(SampleRead::Push(q)) if q.reread => "reread",
                    Some(SampleRead::Push(q)) => q.by.word(),
                    None => "none",
                };
                let (a, b) = (
                    c.from_us.saturating_sub(epoch),
                    c.end_us.saturating_sub(epoch),
                );
                let (lo, hi) = (a / grain, b / grain);
                let parts = ((b - a) / 1_000_000).max(1);
                let edge_n = hi - lo + 1;
                let edge = (0..parts).all(|k| {
                    let first = lo + k * edge_n / parts;
                    let last = (lo + (k + 1) * edge_n / parts).max(first + 1);
                    all.range(first..last).next().is_some()
                });
                if !o.held && !edge && !p.is_some_and(|p| p.by == PushedBy::P3) {
                    continue;
                }
                let (wlo, whi) = (a.div_ceil(grain), b / grain);
                let mut held = std::collections::BTreeSet::new();
                let mut ramped = std::collections::BTreeSet::new();
                let (mut inside, mut before, mut after) = (0_u32, 0_u32, 0_u32);
                let (mut wsized, mut ramp) = (0_u32, 0_u32);
                let mut at = Vec::new();
                for s in r.trace.iter().filter(|s| s.verdict.holds()) {
                    let t = s.t_us.saturating_sub(epoch);
                    let g = t / grain;
                    if (a..=b).contains(&t) {
                        inside += 1;
                        wsized += u32::from(s.window_sized);
                        ramp += u32::from(s.ramp_held());
                        if at.len() < 6 {
                            at.push((t - a) / 1_000);
                        }
                    } else if g == lo && t < a {
                        before += 1;
                    } else if g == hi && t > b {
                        after += 1;
                    }
                    if (wlo..whi).contains(&g) {
                        held.insert(g);
                        if s.ramp_held() {
                            ramped.insert(g);
                        }
                    }
                }
                let n = whi.saturating_sub(wlo);
                let stretches: Vec<(usize, u64)> = (0..parts)
                    .map(|k| {
                        let first = wlo + k * n / parts;
                        let last = (wlo + (k + 1) * n / parts).max(first + 1);
                        (held.range(first..last).count(), last - first)
                    })
                    .collect();
                let whole = n > 0 && stretches.iter().all(|&(h, _)| h > 0);
                println!(
                    "D48 held | {name} | {}-{} ms {:?} {} {} loss={:?} | held={} edge={edge} \
                     whole={whole} parts={parts} inside={inside} wsized={wsized} ramp={ramp} \
                     at={at:?} before={before} after={after} grains={}/{n} share={:.3} \
                     by_second={} rgrains={}/{} d48={d48}",
                    c.from_us / 1_000,
                    c.end_us / 1_000,
                    c.sample.class,
                    p.map_or("none", |p| p.by.word()),
                    p.map_or_else(
                        || "-".to_owned(),
                        |p| format!("{:.3}", f64::from(p.bps) / link)
                    ),
                    c.sample.queue.map(|q| q.loss),
                    o.held,
                    held.len(),
                    held.len() as f64 / n.max(1) as f64,
                    stretches
                        .iter()
                        .map(|&(h, m)| format!("{h}/{m}"))
                        .collect::<Vec<_>>()
                        .join(","),
                    ramped.len(),
                    held.len(),
                );
            }
        }

        /// FR-148 D46, the drop step — what the holding core's trace read over
        /// a span (µs from `t0`): its steps; the holds, those SCTP's window
        /// sized and the `WindowBinds` verdicts; SCTP's window and what it had
        /// in flight, least and most; the fast retransmits and T3-rtx expiries
        /// from the step before the span to its last; and the budget's rate at
        /// that last step, and whether the association's figures gave it.
        #[derive(Debug, Clone, Copy, Default)]
        struct Gauged {
            steps: u32,
            holds: u32,
            window_holds: u32,
            /// FR-148 D48 — the holds SCTP's congestion window sized.
            ramp_holds: u32,
            binds: u32,
            cwnd: (u32, u32),
            ssthresh: u32,
            inflight: (u64, u64),
            loss: (u64, u64),
            rate_bps: u64,
            measured: bool,
        }

        fn gauged(r: &TcpRun, from_us: u64, to_us: u64) -> Gauged {
            let lo = r.trace.partition_point(|s| s.t_us < from_us);
            let hi = r.trace.partition_point(|s| s.t_us <= to_us).max(lo);
            let span = &r.trace[lo..hi];
            let mut g = Gauged::default();
            for s in span {
                g.cwnd = if g.steps == 0 {
                    (s.cwnd, s.cwnd)
                } else {
                    (g.cwnd.0.min(s.cwnd), g.cwnd.1.max(s.cwnd))
                };
                g.ssthresh = if g.steps == 0 {
                    s.ssthresh
                } else {
                    g.ssthresh.min(s.ssthresh)
                };
                g.inflight = if g.steps == 0 {
                    (s.inflight, s.inflight)
                } else {
                    (g.inflight.0.min(s.inflight), g.inflight.1.max(s.inflight))
                };
                g.steps += 1;
                if s.verdict.holds() {
                    g.holds += 1;
                    g.window_holds += u32::from(s.window_sized);
                    g.ramp_holds += u32::from(s.ramp_held());
                }
                g.binds += u32::from(s.verdict == Verdict::WindowBinds);
            }
            let base = lo.checked_sub(1).map(|i| &r.trace[i]).or(span.first());
            if let (Some(b), Some(e)) = (base, span.last()) {
                g.loss = (
                    e.fast_rtx.saturating_sub(b.fast_rtx),
                    e.t3.saturating_sub(b.t3),
                );
                g.rate_bps = e.rate_bps;
                g.measured = e.measured;
            }
            g
        }

        /// AC2's bracket (D45): the floor at 0.80 to 1.05 times the link's
        /// payload rate, and the capacity absent or at least 0.95.
        fn on_bar(r: &TcpRun, m: &ModelFold) -> bool {
            let link = r.shape.payload_bps();
            let floor = m.floor.is_some_and(|f| {
                let f = u64::from(f) * 100;
                f >= link * 80 && f <= link * 105
            });
            floor && m.capacity.is_none_or(|c| u64::from(c) * 100 >= link * 95)
        }

        /// FR-148 D46, the drop step — each rule set's bracket at the run's
        /// end (floor / capacity, times the link's payload rate, and `on` or
        /// `OFF` AC2's bar; [`RULES`]), and how many falls the product's fold
        /// read (FR-148 D50). Under a holding budget, D48's reading (the law
        /// before D50) and the product's: every change each fold made to the
        /// model, with the rule that re-read or bounded it or the fall it
        /// read, and what SCTP's gauges and the budget's verdicts read over
        /// the window or the sample that made it, on a shallow queue, on a
        /// link whose rate falls, on any run the fold leaves off the bar, and
        /// wherever the two end apart or the product read a fall. Then each
        /// fall the product read: when, by what push-back at what rate, and
        /// the floor before and after, with the depth (after over before).
        fn print_drop(r: &TcpRun) {
            let link = r.shape.payload_bps();
            let x = |v: u64| format!("{:.3}", v as f64 / link.max(1) as f64);
            let xo = |f: Option<u32>| f.map_or_else(|| "-".to_owned(), |f| x(u64::from(f)));
            let gate = crate::encode::pipe::STALL_WINDOWS;
            let name = r.shape.name();
            let rules: Vec<String> = RULES
                .iter()
                .map(|&(word, rules)| {
                    let m = fold_with(r, BURST, gate, Open::Start, rules, &mut Vec::new());
                    let bar = if on_bar(r, &m) { "on" } else { "OFF" };
                    format!("{word}={}/{}/{bar}", xo(m.floor), xo(m.capacity))
                })
                .collect();
            println!(
                "D46 drop | {name} | link={} | {}",
                kbps(link),
                rules.join(" ")
            );
            let (mut d48_log, mut d50_log) = (Vec::new(), Vec::new());
            let d48 = fold_with(r, BURST, gate, Open::Start, Rules::D48, &mut d48_log);
            let d50 = fold_with(r, BURST, gate, Open::Start, Rules::D50, &mut d50_log);
            println!(
                "D50 falls | {name} | link={} | d50={}",
                kbps(link),
                d50.falls
            );
            if r.trace.is_empty() {
                return;
            }
            let apart = (d48.floor, d48.capacity) != (d50.floor, d50.capacity);
            let link_falls = r.shape.step.is_some();
            for (word, m, log) in [("d48", &d48, &d48_log), ("d50", &d50, &d50_log)] {
                let on = on_bar(r, m);
                if r.shape.queue < DEEP || !on || link_falls || apart || m.falls > 0 {
                    let tag = if on { "pick event" } else { "pick miss" };
                    print_events(r, &format!("{tag} {word}"), log, m);
                }
            }
            for e in d50_log.iter().filter(|e| e.rule.ends_with("fall")) {
                let (from, to) = (e.floor.0.unwrap_or(0), e.floor.1.unwrap_or(0));
                println!(
                    "D50 fall | {name} | {} ms {} {} floor {}->{} ({:.3})",
                    e.t_us / 1_000,
                    e.by,
                    x(u64::from(e.bps)),
                    x(u64::from(from)),
                    x(u64::from(to)),
                    f64::from(to) / f64::from(from.max(1)),
                );
            }
        }

        /// What the holding core's trace read from `from_us` to `to_us`, as
        /// the drop lines write it.
        fn gauges_line(r: &TcpRun, from_us: u64, to_us: u64) -> String {
            let link = r.shape.payload_bps();
            let g = gauged(r, from_us, to_us);
            format!(
                "cwnd={}-{} ssthresh_lo={} inflight={}-{} fr={} t3={} holds={}/{} wholds={} \
                 cwholds={} binds={} rate={:.3}{}",
                g.cwnd.0,
                g.cwnd.1,
                g.ssthresh,
                g.inflight.0,
                g.inflight.1,
                g.loss.0,
                g.loss.1,
                g.holds,
                g.steps,
                g.window_holds,
                g.ramp_holds,
                g.binds,
                g.rate_bps as f64 / link.max(1) as f64,
                if g.measured { "" } else { " (stand-in)" },
            )
        }

        /// FR-148 D46, the drop step — a fold's changes to the model, each
        /// with what SCTP's gauges and the budget's verdicts read over the
        /// window or the sample that made it, under `tag`.
        fn print_events(r: &TcpRun, tag: &str, log: &[FoldEvent], m: &ModelFold) {
            let link = r.shape.payload_bps();
            let x = |v: u64| format!("{:.3}", v as f64 / link.max(1) as f64);
            let xo = |f: Option<u32>| f.map_or_else(|| "-".to_owned(), |f| x(u64::from(f)));
            let name = r.shape.name();
            let last_floor = log.iter().rposition(|e| e.floor.0 != e.floor.1);
            let last_cap = log
                .iter()
                .rposition(|e| e.capacity)
                .filter(|&i| m.capacity == Some(log[i].bps));
            let ms_of = |us: u64| us / 1_000;
            for (k, e) in log.iter().enumerate() {
                let sets = match (Some(k) == last_floor, Some(k) == last_cap) {
                    (true, true) => " [ends floor+cap]",
                    (true, false) => " [ends floor]",
                    (false, true) => " [ends cap]",
                    (false, false) => "",
                };
                let what = if matches!(e.by, "p1" | "p3") {
                    let c = &r.samples[e.of];
                    let from = c.end_us.saturating_sub(c.sample.ms * 1_000);
                    let q = c.sample.queue;
                    let o = c.over.unwrap_or_default();
                    let close = q.map(|q| q.gauges).unwrap_or_default();
                    format!(
                        "sample {}-{} ms {:?} loss={:?} close cwnd={} ssthresh={} inflight={} \
                         pending={} | over held={} stood={} | {}",
                        ms_of(from),
                        ms_of(c.end_us),
                        c.sample.class,
                        q.map(|q| q.loss),
                        close.cwnd,
                        close.ssthresh,
                        close.inflight_bytes,
                        close.pending_bytes,
                        o.held,
                        o.stood,
                        gauges_line(r, from, c.end_us),
                    )
                } else {
                    let w = &r.windows[e.of];
                    let from = w.end_us.saturating_sub(w.ms * 1_000);
                    format!(
                        "window {}-{} ms rx={} sent={} excess={} | {}",
                        ms_of(from),
                        ms_of(w.end_us),
                        x(u64::from(w.rx_bps)),
                        x(u64::from(w.sent_bps)),
                        w.excess_ms,
                        gauges_line(r, from, w.end_us),
                    )
                };
                let rule = if e.rule.is_empty() {
                    String::new()
                } else {
                    format!(" rule={}", e.rule)
                };
                println!(
                    "D46 drop {tag} | {name} | {} ms {} {}{rule} floor {}->{}{}{} | {what}",
                    ms_of(e.t_us),
                    e.by,
                    x(u64::from(e.bps)),
                    xo(e.floor.0),
                    xo(e.floor.1),
                    if e.capacity { " cap" } else { "" },
                    sets,
                );
            }
        }

        /// FR-148 P0 — the gauges the sampler takes, from the harness's own
        /// takes. The table takes every second; this holds what it took since
        /// the sampler's last take (the least of the leasts, the latest of
        /// the rest), so the sampler reads what a take of its own would have.
        #[derive(Debug, Default)]
        struct Split(Option<Gauges>);

        impl Split {
            fn push(&mut self, g: Gauges) {
                let least = match self.0.and_then(|p| p.ack_delay_min) {
                    Some(p) => Some(g.ack_delay_min.map_or(p, |n| n.min(p))),
                    None => g.ack_delay_min,
                };
                self.0 = Some(Gauges {
                    ack_delay_min: least,
                    ..g
                });
            }

            fn take(&mut self) -> Option<Gauges> {
                self.0.take()
            }
        }

        /// One of the sampler's samples, and the run's rows it covers.
        #[derive(Debug, Clone)]
        struct Covered {
            rows: std::ops::Range<usize>,
            sample: Sample,
            /// When the sample closed, µs from the run's `t0`.
            end_us: u64,
            /// FR-148 D48 — when it started, µs from `t0`: the instant the
            /// budget's [`Budget::over`] read it from, to the µs.
            from_us: u64,
            /// FR-148 D46 B2 — what the writer's in-flight budget saw over
            /// it (`Budget::over`); `None` with the budget off. And its
            /// [`settle`] at the close, µs (0 with the budget off).
            over: Option<Over>,
            settle_us: u64,
        }

        impl Covered {
            /// Every second of the sample was loaded.
            fn loaded(&self, rows: &[Second]) -> bool {
                !self.rows.is_empty()
                    && rows[self.rows.clone()]
                        .iter()
                        .all(|s| s.q_lo_ms > LOADED_MS)
            }

            fn queued(&self) -> bool {
                self.sample.queue.is_some_and(|q| q.queued())
            }
        }

        #[derive(Debug)]
        struct TcpRun {
            shape: Shape,
            rows: Vec<Second>,
            windows: Vec<ViewerWindow>,
            belief: Belief,
            peak_queue: Duration,
            tail_drops: u64,
            /// FR-148 P0 — the delivery sampler's samples and lines.
            samples: Vec<Covered>,
            lines: Vec<LineReport>,
            /// FR-148 D46 — what `belief` was folded from, for a fold with
            /// push-backs; the writer's ticks and counts over its life; and
            /// the frames the budget's verdicts held, acted on or not.
            t0: Instant,
            goodput_at: Vec<(u64, u32)>,
            ticks: u64,
            written: u64,
            held: u64,
            skipped: u64,
            would_hold: u64,
            /// Frames the budget let go because SCTP's window bound with
            /// nothing in its queue.
            window_writes: u64,
            /// The spans the clock stood still with bytes out
            /// ([`WriterStats::silent`]).
            silent: Vec<(u64, u64)>,
            /// The budget's step with its reads: (steps, mean ns, longest ns).
            step: (u64, u64, u64),
            /// FR-148 D46 — the frames still spans wrote through; the still
            /// spans released, head-of-line and drain, and the frames admitted
            /// in still spans, over the run's windows; a holding budget's
            /// steps; the link's queue at each poll (µs from `t0`, µs); and
            /// what the viewer read.
            through: u64,
            released: (u64, u64),
            still_admitted: u64,
            trace: Vec<Step>,
            queue_at: Vec<(u64, u64)>,
            arrivals: Vec<Arrival>,
            /// FR-148 D46 B2 — the core's stall marks (µs from `t0`), where
            /// the run's rows start, and when its writer stopped (µs from
            /// `t0`): a push-back still in its settle wait then is never
            /// handed, as a session's close drops what its sampler holds.
            marks: Vec<u64>,
            start_us: u64,
            stop_us: u64,
            /// FR-148 D46 B3 — under `on`, over the run's windows: the holds
            /// that shut a pass by themselves, the passes the budget acted on,
            /// and the holds among the first that fell in a still span (a
            /// stall: the pump calls no decrease for them).
            acted: (u64, u64, u64),
        }

        fn us(d: Duration) -> u64 {
            u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
        }

        fn ms(d: Duration) -> u64 {
            u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
        }

        /// How long a packet sent now would wait in `c`'s queue.
        fn queue_now(c: &ShapedConn) -> Duration {
            c.out
                .next_free
                .lock()
                .unwrap()
                .saturating_duration_since(Instant::now())
        }

        /// When a packet due at `due` reaches the far end: when due, or at the
        /// end of the stall it fell due in. A stall starts each `every` after
        /// `t0` and lasts `stall`.
        fn held_until(due: Instant, t0: Instant, every: Duration, stall: Duration) -> Instant {
            let k = due.saturating_duration_since(t0).as_nanos() / every.as_nanos();
            match u32::try_from(k) {
                Ok(k @ 1..) => (t0 + every * k + stall).max(due),
                _ => due,
            }
        }

        /// As [`shaped`], and the viewer's end receives nothing during each
        /// stall: what fell due in it is handed over at its end, at once and
        /// in order, as a TCP receiver releases what waited behind a lost
        /// segment. The link carries on at its rate meanwhile.
        fn stalling(
            bps: u64,
            queue: Duration,
            t0: Instant,
            every: Duration,
            stall: Duration,
        ) -> (ShapedConn, ShapedConn) {
            let (due_tx, mut due_rx) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
            let (inbox_tx, b_inbox) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                while let Some((due, p)) = due_rx.recv().await {
                    let release = held_until(due, t0, every, stall);
                    tokio::time::sleep_until(tokio::time::Instant::from_std(release)).await;
                    if inbox_tx.send(p).is_err() {
                        break;
                    }
                }
            });
            let a_out = Shaper {
                bps,
                one_way: RTT / 2,
                queue,
                next_free: StdMutex::new(Instant::now()),
                due: due_tx,
                drop_every: 0,
                full_packets: AtomicU64::new(0),
                peak_wait_ns: AtomicU64::new(0),
                tail_drops: AtomicU64::new(0),
                outages: None,
                step: None,
            };
            let (b_out, a_inbox) = direction(bps, RTT / 2, queue, 0, None);
            let (a, b): (SocketAddr, SocketAddr) = (
                "10.0.0.1:5000".parse().unwrap(),
                "10.0.0.2:5000".parse().unwrap(),
            );
            (
                ShapedConn {
                    out: a_out,
                    inbox: Mutex::new(a_inbox),
                    local: a,
                    remote: b,
                },
                ShapedConn {
                    out: b_out,
                    inbox: Mutex::new(b_inbox),
                    local: b,
                    remote: a,
                },
            )
        }

        /// FR-148 D46 — as [`shaped`], with an outage of `len` every `every`
        /// from `t0` on both directions ([`Outages`]): the link neither
        /// serializes nor delivers in it, and drains what queued in it at its
        /// rate after it, as a link that went down and came back does.
        fn outaged(
            bps: u64,
            queue: Duration,
            t0: Instant,
            every: Duration,
            len: Duration,
        ) -> (ShapedConn, ShapedConn) {
            let outages = Outages { t0, every, len };
            let (a_out, b_inbox) = out_direction(bps, RTT / 2, queue, outages);
            let (b_out, a_inbox) = out_direction(bps, RTT / 2, queue, outages);
            let (a, b): (SocketAddr, SocketAddr) = (
                "10.0.0.1:5000".parse().unwrap(),
                "10.0.0.2:5000".parse().unwrap(),
            );
            (
                ShapedConn {
                    out: a_out,
                    inbox: Mutex::new(a_inbox),
                    local: a,
                    remote: b,
                },
                ShapedConn {
                    out: b_out,
                    inbox: Mutex::new(b_inbox),
                    local: b,
                    remote: a,
                },
            )
        }

        /// FR-148 D46, the drop step — as [`shaped`], and the agent's direction
        /// falls to `to_bps` at `at`, for good: a path whose capacity drops for
        /// real, which the drop rules must not read as a window's artefact.
        fn stepped(
            bps: u64,
            queue: Duration,
            at: Instant,
            to_bps: u64,
        ) -> (ShapedConn, ShapedConn) {
            let (mut a_out, b_inbox) = direction(bps, RTT / 2, queue, 0, None);
            a_out.step = Some((at, to_bps));
            let (b_out, a_inbox) = direction(bps, RTT / 2, queue, 0, None);
            let (a, b): (SocketAddr, SocketAddr) = (
                "10.0.0.1:5000".parse().unwrap(),
                "10.0.0.2:5000".parse().unwrap(),
            );
            (
                ShapedConn {
                    out: a_out,
                    inbox: Mutex::new(a_inbox),
                    local: a,
                    remote: b,
                },
                ShapedConn {
                    out: b_out,
                    inbox: Mutex::new(b_inbox),
                    local: b,
                    remote: a,
                },
            )
        }

        /// As [`pair_opened`] with the default windows (1 MiB), and the viewer
        /// logs when each message arrived and the write time it carries in
        /// its first 8 bytes. FR-148 D48: with `slow`, the viewer reads at that
        /// rate (bit/s) through a receive window of that many bytes, so the
        /// window fills and the viewer is the session's limit; a message
        /// arrives when the viewer reads it.
        async fn pair_timed(
            a: Arc<dyn Conn + Send + Sync>,
            b: Arc<dyn Conn + Send + Sync>,
            path: OpenPath,
            t0: Instant,
            log: Arc<StdMutex<Vec<Arrival>>>,
            slow: Option<(u64, u32)>,
        ) -> (Association, Arc<Association>) {
            let window = slow.map_or(0, |(_, window)| window);
            let config = |net_conn, rwnd, name: &str| Config {
                net_conn,
                max_receive_buffer_size: rwnd,
                max_message_size: 0,
                name: name.to_owned(),
            };
            let (agent, viewer) = match path {
                OpenPath::InitAck => {
                    let agent = tokio::spawn(Association::client(config(a, 0, "agent")));
                    let viewer = Association::server(config(b, window, "viewer")).await;
                    (agent.await.unwrap().unwrap(), viewer.unwrap())
                }
                OpenPath::CookieEcho => {
                    let viewer = tokio::spawn(Association::client(config(b, window, "viewer")));
                    let agent = Association::server(config(a, 0, "agent")).await;
                    (agent.unwrap(), viewer.await.unwrap().unwrap())
                }
            };
            let viewer = Arc::new(viewer);
            let reader = Arc::clone(&viewer);
            tokio::spawn(async move {
                while let Some(stream) = reader.accept_stream().await {
                    let log = Arc::clone(&log);
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65_536];
                        // The slow viewer's next free read: never faster than
                        // its rate, and no catching up after a wait.
                        let mut free = Instant::now();
                        while let Ok(n @ 8..) = stream.read(&mut buf).await {
                            let at = Instant::now();
                            let sent_us = u64::from_le_bytes(buf[..8].try_into().unwrap());
                            log.lock().unwrap().push(Arrival {
                                at_us: us(at.saturating_duration_since(t0)),
                                sent_us,
                                len: n as u64,
                            });
                            if let Some((bps, _)) = slow {
                                free = free.max(at)
                                    + Duration::from_micros(n as u64 * 8_000_000 / bps);
                                tokio::time::sleep_until(tokio::time::Instant::from_std(free))
                                    .await;
                            }
                        }
                    });
                }
            });
            (agent, viewer)
        }

        /// A writer offering `shape`'s rate in frames at [`FPS`], each
        /// stamped with its write time, for [`SAMPLES`] seconds of samples.
        async fn run(shape: Shape) -> TcpRun {
            let _turn = TURN.lock().await;
            #[cfg(target_os = "windows")]
            let _timer = crate::win_timer::TimerResolutionGuard::request_1ms();
            let t0 = Instant::now();
            let (a, b) = match (shape.stalls, shape.outages, shape.step) {
                (Some((every, stall)), _, _) => stalling(shape.bps, shape.queue, t0, every, stall),
                (None, Some((every, len)), _) => outaged(shape.bps, shape.queue, t0, every, len),
                (None, None, Some((at, to))) => stepped(shape.bps, shape.queue, t0 + at, to),
                (None, None, None) => shaped(shape.bps, shape.rtt / 2, shape.queue),
            };
            let a = Arc::new(a);
            let arrivals: Arc<StdMutex<Vec<Arrival>>> = Arc::default();
            let (agent, viewer) = pair_timed(
                a.clone(),
                Arc::new(b),
                shape.path,
                t0,
                Arc::clone(&arrivals),
                shape.viewer,
            )
            .await;
            let stream = agent
                .open_stream(1, PayloadProtocolIdentifier::Binary)
                .await
                .unwrap();
            let sink = GoodputSink::new();
            let frames: Arc<StdMutex<Vec<(u64, u64)>>> = Arc::default();
            let stats: Arc<WriterStats> = Arc::default();
            // FR-148 D46 — the budget reads the association's clock and gauges
            // through the handles a pump holds, and never takes the gauges.
            let core = Core::of(shape.budget).map(|c| Arc::new(StdMutex::new(c)));
            let writer = {
                let stream = Arc::clone(&stream);
                let sink = sink.clone();
                let frames = Arc::clone(&frames);
                let stats = Arc::clone(&stats);
                let core = core.clone();
                let (clock, gauges) = (agent.delivery_clock(), agent.gauges());
                let (every, len) = if shape.sparse {
                    (SAMPLE, SPARSE_LEN)
                } else {
                    (SAMPLE / FPS, shape.frame_len())
                };
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(every);
                    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
                    let mut frame = vec![0x6b_u8; len];
                    // The clock's last move: when, and the bytes it stood at.
                    let mut moved = (Instant::now(), 0_u64);
                    loop {
                        tick.tick().await;
                        stats.ticks.fetch_add(1, Ordering::Relaxed);
                        let hold = core.as_ref().is_some_and(|core| {
                            let mut core = core.lock().unwrap();
                            let began = Instant::now();
                            let mut step = None;
                            let hold = match &mut *core {
                                // The pumps' B1 call: it returns nothing.
                                Core::Shadow(s) => {
                                    s.on_frame_with(
                                        began,
                                        shape.offer_bps(),
                                        || Some(clock.clone()),
                                        || Some(gauges.clone()),
                                    );
                                    if s.take_mark() {
                                        let t = us(began.saturating_duration_since(t0));
                                        stats.marks.lock().unwrap().push(t);
                                    }
                                    false
                                }
                                // FR-148 D46 B3 — the pumps' call under `on`, on
                                // a relay session: the gate's step, and its pass
                                // as the capacity gate's last term (the writer
                                // has no send channel to fill: its write waits).
                                Core::On(s) => {
                                    let mut pass = s.gate_with(
                                        began,
                                        shape.offer_bps(),
                                        || Some(clock.clone()),
                                        || Some(gauges.clone()),
                                        true,
                                        false,
                                    );
                                    if s.take_mark() {
                                        let t = us(began.saturating_duration_since(t0));
                                        stats.marks.lock().unwrap().push(t);
                                    }
                                    s.shut(&mut pass)
                                }
                                Core::Holds(b) => {
                                    let r = Reading {
                                        acked_bytes: clock.read().acked_bytes,
                                        gauges: gauges.peek(),
                                        pump_bytes: 0,
                                        open_bps: shape.offer_bps(),
                                    };
                                    let verdict = b.step(began, &r);
                                    if b.take_mark() {
                                        let t = us(began.saturating_duration_since(t0));
                                        stats.marks.lock().unwrap().push(t);
                                    }
                                    if matches!(verdict, Verdict::WindowBinds) {
                                        stats.window.fetch_add(1, Ordering::Relaxed);
                                    }
                                    if matches!(verdict, Verdict::WritesThrough) {
                                        stats.through.fetch_add(1, Ordering::Relaxed);
                                    }
                                    step = Some(Step {
                                        t_us: us(began.saturating_duration_since(t0)),
                                        acked: r.acked_bytes,
                                        inflight: r.gauges.inflight_bytes,
                                        pending: r.gauges.pending_bytes,
                                        verdict,
                                        rate_bps: b.rate_bps().unwrap_or(0),
                                        budget: b.last_budget(),
                                        still_after_us: us(b.still_after()),
                                        release: b.last_release(),
                                        measured: b.rate_measured(),
                                        window_sized: b.window_sized(&r.gauges),
                                        cwnd: r.gauges.cwnd,
                                        rwnd: r.gauges.rwnd,
                                        ssthresh: r.gauges.ssthresh,
                                        fast_rtx: r.gauges.fast_retransmits,
                                        t3: r.gauges.t3_expiries,
                                    });
                                    // A clock that stood still with bytes out
                                    // for the least and the slack was a stall.
                                    if r.acked_bytes != moved.1 || r.out() == 0 {
                                        let still = began.saturating_duration_since(moved.0);
                                        if still >= b.still_after() {
                                            let at =
                                                |i: Instant| us(i.saturating_duration_since(t0));
                                            stats
                                                .silent
                                                .lock()
                                                .unwrap()
                                                .push((at(moved.0), at(began)));
                                        }
                                        moved = (began, r.acked_bytes);
                                    }
                                    verdict.holds()
                                }
                            };
                            let ns = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
                            stats.steps.fetch_add(1, Ordering::Relaxed);
                            stats.step_ns.fetch_add(ns, Ordering::Relaxed);
                            stats.step_max_ns.fetch_max(ns, Ordering::Relaxed);
                            if let Some(step) = step {
                                stats.trace.lock().unwrap().push(step);
                            }
                            stats
                                .budget
                                .store(core.budget().last_budget(), Ordering::Relaxed);
                            hold
                        });
                        if hold {
                            stats.held.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        if shape.skip_over && stream.buffered_amount() as u64 > OVERFLOW {
                            stats.skipped.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let stamp = us(t0.elapsed());
                        frame[..8].copy_from_slice(&stamp.to_le_bytes());
                        let began = Instant::now();
                        if stream.write(&Bytes::copy_from_slice(&frame)).await.is_err() {
                            break;
                        }
                        sink.record(len as u64, began.elapsed());
                        frames.lock().unwrap().push((stamp, len as u64));
                        stats.written.fetch_add(1, Ordering::Relaxed);
                    }
                })
            };
            let clock = agent.delivery_clock();
            let gauges = agent.gauges();
            let mut goodput = GoodputEstimator::new();
            let mut goodput_at = Vec::new();
            let mut rows = Vec::new();
            let (mut seen_rx, mut seen_tx) = (0, 0);
            let mut last_transit: Option<u64> = None;
            let fastest = fastest_ms(&shape);
            let held_of = |s: &WriterStats| {
                s.held.load(Ordering::Relaxed) + s.skipped.load(Ordering::Relaxed)
            };
            // FR-148 D46 — a line closes the budget's window, as in the pumps.
            let with_budget = |mut l: LineReport| {
                if let Some(core) = &core {
                    l.budget = Some(core.lock().unwrap().take_window());
                }
                l
            };
            let start = Instant::now();
            let first = gauges.take();
            let _ = sink.drain();
            let mut prev = (start, clock.read());
            let mut held_before = held_of(&stats);
            // FR-148 P0 — the delivery sampler itself, on this run's clock and,
            // through `Split`, on its gauges.
            let mut sampler = DeliverySampler::new(true);
            let mut split = Split::default();
            let (mut samples, mut lines) = (Vec::new(), Vec::new());
            let mut covered = 0;
            let mut queue_at = Vec::new();
            assert_eq!(sampler.on_reading_with(start, prev.1, || Some(first)), None);
            // FR-148 D46 B2 — where the sampler's sample starts: the writer's
            // core says what it saw from there at its close.
            let mut base = prev;
            for i in 1..=shape.seconds {
                let end = start + SAMPLE * i;
                let (mut q_lo, mut q_hi) = (Duration::MAX, Duration::ZERO);
                loop {
                    let q = queue_now(&a);
                    q_lo = q_lo.min(q);
                    q_hi = q_hi.max(q);
                    let now = Instant::now();
                    queue_at.push((us(now.saturating_duration_since(t0)), us(q)));
                    if now >= end {
                        break;
                    }
                    let next = (now + POLL).min(end);
                    tokio::time::sleep_until(tokio::time::Instant::from_std(next)).await;
                }
                let now = Instant::now();
                let d = clock.read();
                let g = gauges.take();
                split.push(g);
                let sample = Sample::between(prev.1, d, ms(now - prev.0));
                let blocked = sink.drain();
                let blocked_ms = ms(blocked.iter().map(|s| s.elapsed).sum::<Duration>());
                let goodput_bps = if goodput.observe_window(&blocked, now) {
                    goodput.estimate_bps(now)
                } else {
                    None
                };
                if let Some(bps) = goodput_bps {
                    goodput_at.push((us(now - t0), bps));
                }
                let (rx, newest, transit_lo_ms, in_time) = {
                    let log = arrivals.lock().unwrap();
                    let new = &log[seen_rx..];
                    seen_rx = log.len();
                    (
                        new.iter().map(|x| x.len).sum::<u64>(),
                        new.last().map(transit_ms),
                        new.iter().map(transit_ms).min(),
                        new.iter()
                            .filter(|x| transit_ms(x) <= fastest + FRESH_MS)
                            .map(|x| x.len)
                            .sum::<u64>(),
                    )
                };
                let held_now = held_of(&stats);
                let held = held_now - held_before;
                held_before = held_now;
                let sent = {
                    let log = frames.lock().unwrap();
                    let fresh = &log[seen_tx..];
                    seen_tx = log.len();
                    fresh.iter().map(|f| f.1).sum::<u64>()
                };
                let growth_ms = newest.zip(last_transit).map(|(t, l)| t as i64 - l as i64);
                if newest.is_some() {
                    last_transit = newest;
                }
                let span = sample.ms.max(1);
                rows.push(Second {
                    t: i,
                    class: sample.class,
                    acked_bps: sample.bps().unwrap_or(0),
                    q_lo_ms: ms(q_lo),
                    q_hi_ms: ms(q_hi),
                    inflight: g.inflight_bytes,
                    cwnd: g.cwnd,
                    ssthresh: g.ssthresh,
                    rwnd: g.rwnd,
                    pending: g.pending_bytes,
                    empties: d.queue_empties.saturating_sub(prev.1.queue_empties),
                    buffered: stream.buffered_amount() as u64,
                    ack_min_ms: g.ack_delay_min.map(ms),
                    sent_bps: sent * 8_000 / span,
                    rx_bps: rx * 8_000 / span,
                    transit_ms: newest,
                    growth_ms,
                    blocked_ms,
                    goodput_bps,
                    fast_rtx: g.fast_retransmits,
                    t3: g.t3_expiries,
                    held,
                    budget: if core.is_some() {
                        stats.budget.load(Ordering::Relaxed)
                    } else {
                        0
                    },
                    transit_lo_ms,
                    fresh_bps: in_time * 8_000 / span,
                });
                if let Some(sample) = sampler.on_reading_with(now, d, || split.take()) {
                    let seen = core.as_ref().map(|c| {
                        let c = c.lock().unwrap();
                        (
                            c.budget().over(base.0, now, base.1.acked_bytes),
                            us(settle(c.budget())),
                        )
                    });
                    samples.push(Covered {
                        rows: covered..rows.len(),
                        sample,
                        end_us: us(now - t0),
                        from_us: us(base.0.saturating_duration_since(t0)),
                        over: seen.map(|(o, _)| o),
                        settle_us: seen.map_or(0, |(_, s)| s),
                    });
                    covered = rows.len();
                    base = (now, d);
                }
                lines.extend(sampler.take_line(now).map(&with_budget));
                prev = (now, d);
            }
            lines.extend(sampler.take_rest().map(&with_budget));
            let stop_us = us(t0.elapsed());
            writer.abort();
            // Its counts are final once it has stopped.
            let _ = writer.await;
            let last_window = core.as_ref().map(|c| c.lock().unwrap().take_window());
            let budget_windows = || lines.iter().filter_map(|l| l.budget).chain(last_window);
            let would_hold: u64 = budget_windows().map(|b| b.held.0).sum();
            let released =
                budget_windows().fold((0, 0), |(h, d), b| (h + b.released.0, d + b.released.1));
            let still_admitted: u64 = budget_windows().map(|b| b.still_admitted).sum();
            let acted = budget_windows().fold((0, 0, 0), |(h, a, s), b| {
                (h + b.acted.0, a + b.acted.1, s + b.acted_stalled)
            });
            let load = |n: &AtomicU64| n.load(Ordering::Relaxed);
            let from_us = us(start - t0);
            let silent: Vec<(u64, u64)> = stats
                .silent
                .lock()
                .unwrap()
                .iter()
                .copied()
                .filter(|s| s.1 > from_us)
                .collect();
            let steps = load(&stats.steps);
            let step = (
                steps,
                load(&stats.step_ns) / steps.max(1),
                load(&stats.step_max_ns),
            );
            let peak_queue = a.peak_wait();
            let tail_drops = a.tail_drops();
            let arrivals = arrivals.lock().unwrap().clone();
            let frames = frames.lock().unwrap().clone();
            let trace = std::mem::take(&mut *stats.trace.lock().unwrap());
            let marks = std::mem::take(&mut *stats.marks.lock().unwrap());
            let _ = tokio::time::timeout(Duration::from_secs(5), agent.close()).await;
            let _ = tokio::time::timeout(Duration::from_secs(5), viewer.close()).await;
            let windows = viewer_windows(&arrivals, &frames, us(start - t0));
            let belief = belief_of(&windows, &goodput_at, t0, &[]);
            TcpRun {
                shape,
                rows,
                windows,
                belief,
                peak_queue,
                tail_drops,
                samples,
                lines,
                t0,
                goodput_at,
                ticks: load(&stats.ticks),
                written: load(&stats.written),
                held: load(&stats.held),
                skipped: load(&stats.skipped),
                would_hold,
                window_writes: load(&stats.window),
                silent,
                step,
                through: load(&stats.through),
                released,
                still_admitted,
                trace,
                queue_at,
                arrivals,
                marks,
                start_us: from_us,
                stop_us,
                acted,
            }
        }

        fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
            v.map_or_else(|| "-".to_owned(), |v| v.to_string())
        }

        fn kbps(bps: u64) -> u64 {
            bps / 1000
        }

        /// (Loaded seconds a signal read on, loaded seconds, other seconds it
        /// read on, other seconds.)
        type Count = (usize, usize, usize, usize);

        /// What each candidate signal read.
        fn readings(r: &TcpRun) -> Vec<(&'static str, Count)> {
            let sig = signals_of(&r.rows);
            let mut link = LinkLoop::new();
            let congested: Vec<bool> = r
                .rows
                .iter()
                .map(|s| {
                    let growth = s.growth_ms.unwrap_or(0).clamp(-30_000, 30_000);
                    let rx = u32::try_from(s.rx_bps).unwrap_or(u32::MAX);
                    link.observe(rx, i16::try_from(growth).unwrap()).congested
                })
                .collect();
            type Read = fn(&Second, &Signals) -> bool;
            let reads: [(&'static str, Read); 6] = [
                ("backlogged (the classifier)", |s, _| {
                    s.class == Class::Backlogged
                }),
                ("rtt over its least >= 250 ms", |_, g| {
                    g.rtt_over_ms.is_some_and(|v| v >= RTT_OVER_MS)
                }),
                ("inflight >= 3 x delivered x least rtt", |_, g| {
                    g.bdp_pct.is_some_and(|v| v >= BDP_PCT)
                }),
                ("transit over its least >= 200 ms", |_, g| {
                    g.transit_over_ms.is_some_and(|v| v >= TRANSIT_OVER_MS)
                }),
                ("transit growth >= 100 ms", |s, _| {
                    s.growth_ms.is_some_and(|v| v >= GROWTH_MS)
                }),
                ("buffered amount > 256 KiB", |s, _| s.buffered > OVERFLOW),
            ];
            let loaded: Vec<bool> = r.rows.iter().map(|s| s.q_lo_ms > LOADED_MS).collect();
            let count = |read: &dyn Fn(usize) -> bool| {
                let on = |want: bool| {
                    let of = (0..r.rows.len()).filter(|&i| loaded[i] == want);
                    let n = of.clone().count();
                    (of.filter(|&i| read(i)).count(), n)
                };
                let ((a, b), (c, d)) = (on(true), on(false));
                (a, b, c, d)
            };
            let mut out: Vec<_> = reads
                .iter()
                .map(|&(name, read)| (name, count(&|i| read(&r.rows[i], &sig[i]))))
                .collect();
            out.push(("the link loop: congested", count(&|i| congested[i])));
            out
        }

        fn print_run(r: &TcpRun) {
            let link = r.shape.payload_bps();
            println!(
                "== {} (payload {} kbit/s): peak queue {:?}, tail drops {}",
                r.shape.name(),
                kbps(link),
                r.peak_queue,
                r.tail_drops
            );
            println!(
                " t class      acked  q_lo..q_hi ms  inflight    cwnd  ssthresh    rwnd  pending emp \
                 buffered ackmin  sent    rx transit growth blk_ms goodput fr t3 | bdp% rtt+ tr+ \
                 | held  budget tr_lo fresh"
            );
            for (s, g) in r.rows.iter().zip(signals_of(&r.rows)) {
                let class = format!("{:?}", s.class);
                println!(
                    "{:>2} {class:<10} {:>5} {:>6}..{:<6} {:>9} {:>7} {:>9} {:>7} {:>8} {:>3} {:>8} {:>6} {:>5} {:>5} {:>7} {:>6} {:>6} {:>7} {:>2} {:>2} | {:>4} {:>4} {:>4} | {:>4} {:>7} {:>5} {:>5}",
                    s.t,
                    kbps(s.acked_bps),
                    s.q_lo_ms,
                    s.q_hi_ms,
                    s.inflight,
                    s.cwnd,
                    s.ssthresh,
                    s.rwnd,
                    s.pending,
                    s.empties,
                    s.buffered,
                    opt(s.ack_min_ms),
                    kbps(s.sent_bps),
                    kbps(s.rx_bps),
                    opt(s.transit_ms),
                    opt(s.growth_ms),
                    s.blocked_ms,
                    opt(s.goodput_bps.map(|b| b / 1000)),
                    s.fast_rtx,
                    s.t3,
                    opt(g.bdp_pct),
                    opt(g.rtt_over_ms),
                    opt(g.transit_over_ms),
                    s.held,
                    s.budget,
                    opt(s.transit_lo_ms),
                    kbps(s.fresh_bps),
                );
            }
            for (name, (a, b, c, d)) in readings(r) {
                println!("   {name:<40} loaded {a:>2}/{b:<2} other {c:>2}/{d:<2}");
            }
            let b = &r.belief;
            println!(
                "   viewer windows {}: offered over arrival {}; max rx {} kbit/s; invalid {}, \
                 congested {}, capacities link {} goodput {}; pipe (anchor, floor, capacity) = \
                 ({}, {}, {}) kbit/s",
                b.windows,
                b.offered,
                b.max_rx / 1000,
                b.invalid,
                b.congested,
                b.link_capacities,
                b.goodput_capacities,
                opt(b.anchor.map(|v| v / 1000)),
                opt(b.floor.map(|v| v / 1000)),
                opt(b.capacity.map(|v| v / 1000)),
            );
            // FR-148 P0 — the delivery sampler's samples, and its lines as the
            // daemon writes them.
            for c in &r.samples {
                let q = c.sample.queue;
                let class = format!("{:?}", c.sample.class);
                println!(
                    "   sampler: seconds {:>2}..={:<2} {class:<10} {:>7} B in {} ms, least {} ms, standing {} ms{}{}",
                    c.rows.start + 1,
                    c.rows.end,
                    c.sample.bytes,
                    c.sample.ms,
                    opt(q.and_then(|q| q.least_ms)),
                    opt(q.and_then(|q| q.standing_ms)),
                    if c.loaded(&r.rows) { ", loaded" } else { "" },
                    if c.queued() { ", queued" } else { "" },
                );
            }
            for l in &r.lines {
                print!("   {}", super::super::tests::logged(|| emit(sid(), l)));
            }
        }

        fn print_windows(r: &TcpRun) {
            for w in &r.windows {
                println!(
                    "   window to {:>6} ms ({:>4} ms): rx {:>5} sent {:>5} kbit/s, \
                     growth {:>5} ms, transit {:>5} ms (+{})",
                    w.end_us / 1000,
                    w.ms,
                    w.rx_bps / 1000,
                    w.sent_bps / 1000,
                    w.growth_ms,
                    w.transit_ms,
                    w.excess_ms
                );
            }
        }

        fn shape(bps: u64, queue: Duration, path: OpenPath) -> Shape {
            Shape {
                bps,
                queue,
                path,
                offer_pct: 150,
                skip_over: false,
                budget: Budgeted::No,
                stalls: None,
                outages: None,
                sparse: false,
                seconds: SAMPLES,
                step: None,
                rtt: RTT,
                viewer: None,
            }
        }

        /// FR-148 P0 — the delivery sampler's queued samples: at least half of
        /// those whose seconds were all loaded.
        fn sampler_reads_the_queue(r: &TcpRun) {
            let name = r.shape.name();
            let loaded: Vec<&Covered> = r.samples.iter().filter(|c| c.loaded(&r.rows)).collect();
            let queued = loaded.iter().filter(|c| c.queued()).count();
            assert!(
                !loaded.is_empty(),
                "{name}: no sample of the sampler's was loaded"
            );
            assert!(
                queued * 2 >= loaded.len(),
                "{name}: {queued} of {} loaded samples read queued",
                loaded.len()
            );
        }

        /// FR-148 P0 — the delivery sampler's lines carry the gauges and read
        /// no queued sample.
        fn sampler_reads_no_queue(r: &TcpRun) {
            let name = r.shape.name();
            assert!(!r.lines.is_empty(), "{name}: no line");
            for l in &r.lines {
                let q = l
                    .queue
                    .unwrap_or_else(|| panic!("{name}: a line without gauges: {l:?}"));
                assert_eq!(q.queued, 0, "{name}: {l:?}");
            }
            assert!(
                r.samples.iter().all(|c| !c.queued()),
                "{name}: {:?}",
                r.samples
            );
        }

        /// The hypothesis: on the deep, lossless queue, fewer than half the
        /// loaded seconds read backlogged.
        async fn deep(shape: Shape) {
            let r = run(shape).await;
            print_run(&r);
            assert_eq!(
                r.tail_drops,
                0,
                "{}: the deep queue drops nothing",
                shape.name()
            );
            let loaded: Vec<&Second> = r.rows.iter().filter(|s| s.q_lo_ms > LOADED_MS).collect();
            let backlogged = loaded
                .iter()
                .filter(|s| s.class == Class::Backlogged)
                .count();
            let all = r
                .rows
                .iter()
                .filter(|s| s.class == Class::Backlogged)
                .count();
            assert!(
                !loaded.is_empty(),
                "{}: the link's queue never stayed over {LOADED_MS} ms for a second; \
                 {all} of {} seconds read backlogged",
                shape.name(),
                r.rows.len()
            );
            // In the first runs SCTP's own acknowledgement delay over its
            // least read every loaded second, the lowest 343 ms over the
            // least. The pin is at half the loaded seconds, so that a busy
            // runner cannot flip it (FR-148 D44); the measured numbers are
            // in `docs/rate-control.md`.
            let queued = r
                .rows
                .iter()
                .zip(signals_of(&r.rows))
                .filter(|(s, g)| {
                    s.q_lo_ms > LOADED_MS && g.rtt_over_ms.is_some_and(|v| v >= RTT_OVER_MS)
                })
                .count();
            assert!(
                queued * 2 >= loaded.len(),
                "{}: the acknowledgement delay read {queued} of {} loaded seconds",
                shape.name(),
                loaded.len()
            );
            sampler_reads_the_queue(&r);
            // The hypothesis, last.
            assert!(
                backlogged * 2 < loaded.len(),
                "{}: {backlogged} of {} loaded seconds read backlogged",
                shape.name(),
                loaded.len()
            );
        }

        /// The UDP shape: the same writer over a queue that drops.
        async fn control(shape: Shape) {
            let r = run(shape).await;
            print_run(&r);
            assert!(r.tail_drops > 0, "{}: the control drops", shape.name());
            assert!(
                r.rows.iter().all(|s| s.q_hi_ms <= LOADED_MS),
                "{}: the control's queue passed {LOADED_MS} ms",
                shape.name()
            );
            // A pin of the first run's numbers: no false reading here.
            let worst = signals_of(&r.rows)
                .iter()
                .filter_map(|g| g.rtt_over_ms)
                .max()
                .unwrap_or(0);
            assert!(
                worst < RTT_OVER_MS,
                "{}: the acknowledgement delay rose {worst} ms over its least",
                shape.name()
            );
            sampler_reads_no_queue(&r);
        }

        /// FR-148 P0 — a still screen: one small message a second. Each
        /// sample is two lone packets, whose SACKs the viewer may hold, and
        /// none acknowledges enough bytes to count, so none is queued.
        async fn sparse(queue: Duration) {
            let shape = Shape {
                sparse: true,
                seconds: SPARSE_SECONDS,
                ..shape(2_000_000, queue, OpenPath::InitAck)
            };
            let r = run(shape).await;
            print_run(&r);
            let name = shape.name();
            // A still screen's message is 200 bytes a second, which never
            // passes `counts`' byte bound, so two samples show the guard
            // holds. The first runs took five (FR-148 D44).
            assert!(r.samples.len() >= 2, "{name}: {:?}", r.samples);
            for c in &r.samples {
                let q = c
                    .sample
                    .queue
                    .unwrap_or_else(|| panic!("{name}: a sample without gauges: {c:?}"));
                assert!(
                    !counts(c.sample.ms, c.sample.bytes, q.gauges.max_payload),
                    "{name}: a still screen's sample counted: {c:?}"
                );
            }
            assert!(
                r.samples
                    .iter()
                    .any(|c| c.sample.queue.is_some_and(|q| q.least_ms.is_some())),
                "{name}: no SACK gave a delay: {:?}",
                r.samples
            );
            sampler_reads_no_queue(&r);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_still_screen_on_the_deep_queue_reads_no_queue() {
            sparse(DEEP).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_still_screen_on_the_shallow_queue_reads_no_queue() {
            sparse(SHALLOW).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "red: the peer's 1 MiB window binds at 8 s, then 12 of 19 loaded seconds read backlogged"]
        async fn deep_2_mbps_path_i_reads_under_half_backlogged() {
            deep(shape(2_000_000, DEEP, OpenPath::InitAck)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "red: path C keeps the backlog in SCTP's queue: never loaded, 19 of 20 seconds backlogged"]
        async fn deep_2_mbps_path_c_reads_under_half_backlogged() {
            deep(shape(2_000_000, DEEP, OpenPath::CookieEcho)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn deep_0_4_mbps_path_i_reads_under_half_backlogged() {
            deep(shape(400_000, DEEP, OpenPath::InitAck)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "red: path C reads backlogged on 12 of 12 loaded seconds"]
        async fn deep_0_4_mbps_path_c_reads_under_half_backlogged() {
            deep(shape(400_000, DEEP, OpenPath::CookieEcho)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn deep_2_mbps_path_i_with_the_pump_skip_reads_under_half_backlogged() {
            let shape = Shape {
                skip_over: true,
                ..shape(2_000_000, DEEP, OpenPath::InitAck)
            };
            deep(shape).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "red: as without the skip: never loaded, 19 of 20 seconds backlogged"]
        async fn deep_2_mbps_path_c_with_the_pump_skip_reads_under_half_backlogged() {
            let shape = Shape {
                skip_over: true,
                ..shape(2_000_000, DEEP, OpenPath::CookieEcho)
            };
            deep(shape).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn shallow_2_mbps_path_i_control() {
            control(shape(2_000_000, SHALLOW, OpenPath::InitAck)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn shallow_2_mbps_path_c_control() {
            control(shape(2_000_000, SHALLOW, OpenPath::CookieEcho)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn shallow_0_4_mbps_path_i_control() {
            control(shape(400_000, SHALLOW, OpenPath::InitAck)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn shallow_0_4_mbps_path_c_control() {
            control(shape(400_000, SHALLOW, OpenPath::CookieEcho)).await;
        }

        /// The stall cells' shape: a lossless link that stalls, under a writer
        /// that never offers the link's rate.
        fn stalled(path: OpenPath) -> Shape {
            Shape {
                offer_pct: 80,
                stalls: Some((STALL_EVERY, STALL_FOR)),
                ..shape(2_000_000, DEEP, path)
            }
        }

        /// Head-of-line stalls on a lossless link: the viewer window after a
        /// stall carries the stall's bytes too, and `Pipe`'s floor takes
        /// every window.
        async fn stalls(shape: Shape) {
            let r = run(shape).await;
            print_run(&r);
            print_windows(&r);
            let link = shape.payload_bps();
            let floor = u64::from(r.belief.floor.unwrap_or(0));
            assert!(
                floor * 10 > link * 13,
                "{}: floor {floor} bit/s on a link of {link}",
                shape.name()
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): it pins a defect, Pipe's floor taking the window after a stall"]
        async fn stalls_2_mbps_path_i_lift_the_floor_over_the_link() {
            stalls(stalled(OpenPath::InitAck)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "red: path C's window caps what the link carries in a stall: floor 1.27 to 1.28 x the link in 20 s"]
        async fn stalls_2_mbps_path_c_lift_the_floor_over_the_link() {
            stalls(stalled(OpenPath::CookieEcho)).await;
        }

        /// The lab's shape on TCP: 1.5 times the link under the pump's skip,
        /// whose watermark holds a standing queue of about a second, and the
        /// stalls under it. What a stall holds is already queued, so the
        /// window after it carries the link's rate for the stall's length on
        /// top of its own second.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): it pins a defect, Pipe's floor taking the window after a stall"]
        async fn stalls_under_the_pump_skip_2_mbps_path_i_lift_the_floor_over_the_link() {
            stalls(Shape {
                offer_pct: 150,
                skip_over: true,
                ..stalled(OpenPath::InitAck)
            })
            .await;
        }

        /// FR-148 D46 — one run in the numbers the budget is judged by.
        #[derive(Debug, Clone, Copy)]
        struct Summary {
            /// The link's queue at its least in each second, what stood all
            /// second: the median and the most over the run's seconds.
            link_ms: (u64, u64),
            /// Each second's least transit, write to read, over the fastest a
            /// frame can go ([`fastest_ms`]): the median and the most.
            transit_ms: (u64, u64),
            /// What the viewer read, and what it read within [`FRESH_MS`] of
            /// the fastest, bit/s over the run's seconds.
            rx_bps: u64,
            fresh_bps: u64,
            /// Frames written; held by the budget or skipped over the
            /// watermark; and held by the budget's verdicts, acted on or not.
            written: u64,
            held: u64,
            would_hold: u64,
            /// The floor of the run's fold; with a push-back at each queued
            /// sample (P1), and their count; and, under a holding budget, with
            /// one at each sample that counted, was backlogged (SCTP's own
            /// class, or the budget held a frame in a share of each of its
            /// seconds) and
            /// held no span of a still clock (P3).
            floor: Option<u32>,
            floor_p1: (Option<u32>, usize),
            floor_p3: Option<(Option<u32>, usize)>,
        }

        /// `bps` as a percent of `r`'s link.
        fn pct(bps: u64, r: &TcpRun) -> u64 {
            bps * 100 / r.shape.payload_bps().max(1)
        }

        fn median_max(mut v: Vec<u64>) -> (u64, u64) {
            v.sort_unstable();
            match v.len() {
                0 => (0, 0),
                n => (v[n / 2], v[n - 1]),
            }
        }

        fn summary(r: &TcpRun) -> Summary {
            let link_ms = median_max(r.rows.iter().map(|s| s.q_lo_ms).collect());
            let fastest = fastest_ms(&r.shape);
            let transit_ms = median_max(
                r.rows
                    .iter()
                    .filter_map(|s| s.transit_lo_ms.map(|t| t.saturating_sub(fastest)))
                    .collect(),
            );
            let seconds = (r.rows.len() as u64).max(1);
            let fold = |pushbacks: Vec<(u64, u32)>| {
                let n = pushbacks.len();
                let belief = belief_of(&r.windows, &r.goodput_at, r.t0, &pushbacks);
                (belief.floor, n)
            };
            let at = |c: &Covered| {
                let bps = c.sample.bps().unwrap_or(0);
                (c.end_us, u32::try_from(bps).unwrap_or(u32::MAX))
            };
            let p3 = |c: &&Covered| {
                let counted = c.sample.queue.is_some_and(|q| q.standing_ms.is_some());
                let rows = &r.rows[c.rows.clone()];
                let held = !rows.is_empty() && rows.iter().all(|s| s.held > 0);
                let from = c.end_us.saturating_sub(c.sample.ms * 1000);
                let still = r.silent.iter().any(|&(a, b)| a < c.end_us && b > from);
                counted && (c.sample.class == Class::Backlogged || held) && !still
            };
            Summary {
                link_ms,
                transit_ms,
                rx_bps: r.rows.iter().map(|s| s.rx_bps).sum::<u64>() / seconds,
                fresh_bps: r.rows.iter().map(|s| s.fresh_bps).sum::<u64>() / seconds,
                written: r.written,
                held: r.held + r.skipped,
                would_hold: r.would_hold,
                floor: r.belief.floor,
                floor_p1: fold(r.samples.iter().filter(|c| c.queued()).map(at).collect()),
                floor_p3: matches!(
                    r.shape.budget,
                    Budgeted::Holds(_)
                        | Budgeted::HoldsAsB1(_)
                        | Budgeted::HoldsAfterTwo(_)
                        | Budgeted::On
                )
                .then(|| fold(r.samples.iter().filter(p3).map(at).collect())),
            }
        }

        fn print_summary(r: &TcpRun, s: &Summary) {
            let link = r.shape.payload_bps();
            let floor = |f: Option<u32>| {
                f.map_or_else(
                    || "-".to_owned(),
                    |f| format!("{} ({:.2} x)", f / 1000, f64::from(f) / link as f64),
                )
            };
            let p3 = s
                .floor_p3
                .map_or_else(|| "-".to_owned(), |(f, n)| format!("{} from {n}", floor(f)));
            println!(
                "D46 | {} | standing link {} / {} ms, transit {} / {} ms (median / most) | \
                 read {} %, fresh {} % of {} kbit/s | ticks {}, written {}, held or skipped \
                 {}, the budget's verdicts held {}, written on SCTP's window {} | floor {}, \
                 P1 {} from {}, P3 {p3} kbit/s; still clock {} spans | budget step {} ns \
                 mean, {} ns most, {} steps | still spans released head-of-line {}, drain {}; \
                 admitted in still spans {}, written through {}",
                r.shape.name(),
                s.link_ms.0,
                s.link_ms.1,
                s.transit_ms.0,
                s.transit_ms.1,
                pct(s.rx_bps, r),
                pct(s.fresh_bps, r),
                kbps(link),
                r.ticks,
                s.written,
                s.held,
                s.would_hold,
                r.window_writes,
                floor(s.floor),
                floor(s.floor_p1.0),
                s.floor_p1.1,
                r.silent.len(),
                r.step.1,
                r.step.2,
                r.step.0,
                r.released.0,
                r.released.1,
                r.still_admitted,
                r.through,
            );
        }

        /// FR-148 D46 — each shape in turn, its rows and its summary printed,
        /// and a holding budget's releases.
        async fn measured(shapes: &[Shape]) -> Vec<(TcpRun, Summary)> {
            let mut out = Vec::new();
            for &shape in shapes {
                let r = run(shape).await;
                print_run(&r);
                let s = summary(&r);
                print_summary(&r, &s);
                print_b2(&r);
                print_held(&r);
                print_drop(&r);
                print_releases(&r);
                out.push((r, s));
            }
            out
        }

        /// FR-148 D46 — each still span a holding budget saw released: when
        /// its clock last stood still (ms from the run's start), how long it
        /// stood, what SCTP had out then, the rate and the budget, the bytes
        /// back at each step of the next 400 ms (`ms:bytes` after that last
        /// still reading), and the class the core read [`BURST_WITHIN`] after
        /// it.
        fn print_releases(r: &TcpRun) {
            let name = r.shape.name();
            let mut span: Option<(usize, usize)> = None;
            for (i, s) in r.trace.iter().enumerate() {
                let out = s.inflight + s.pending;
                if out > 0
                    && let Some((from, _)) = span
                    && r.trace[from].acked == s.acked
                {
                    span = Some((from, i));
                    continue;
                }
                if let Some((from, to)) = span.take() {
                    let (first, last) = (&r.trace[from], &r.trace[to]);
                    let still = last.t_us - first.t_us;
                    if still >= s.still_after_us && s.acked != last.acked {
                        let since = |x: &Step| x.t_us.saturating_sub(last.t_us);
                        let after: Vec<String> = r.trace[i..]
                            .iter()
                            .take_while(|x| since(x) <= 400_000)
                            .map(|x| format!("{}:{}", since(x) / 1000, x.acked - last.acked))
                            .collect();
                        let read = r.trace[i..]
                            .iter()
                            .find(|x| since(x) >= us(BURST_WITHIN))
                            .and_then(|x| x.release);
                        let span = &r.trace[from..=to];
                        let count = |v: Verdict| span.iter().filter(|x| x.verdict == v).count();
                        println!(
                            "D46 release | {name} | at {} ms, still {} ms, out {} (in flight {}, \
                             pending {}), rate {} kbit/s, budget {}, written through {}, held {} \
                             | {} | read {read:?}",
                            last.t_us / 1000,
                            still / 1000,
                            last.inflight + last.pending,
                            last.inflight,
                            last.pending,
                            kbps(last.rate_bps),
                            last.budget,
                            count(Verdict::WritesThrough),
                            count(Verdict::Hold),
                            after.join(" "),
                        );
                    }
                }
                if out > 0 {
                    span = Some((i, i));
                }
            }
        }

        /// FR-148 D46 — the queue and transit bar, ms.
        const BAR_MS: u64 = 300;

        /// FR-148 D46 — after each outage's end with a second of the run after
        /// it: (its end, ms from the run's start; then how long after it, ms,
        /// the link's queue and each arrival's transit over the fastest last
        /// stood at [`BAR_MS`] or over, before the next outage began; 0 when
        /// they never did). The queue is read until [`BAR_MS`] before the next
        /// outage: a packet sent in that last stretch waits for the outage too.
        fn recoveries(r: &TcpRun) -> Vec<(u64, u64, u64)> {
            let Some((every, len)) = r.shape.outages else {
                return Vec::new();
            };
            let end = r.queue_at.last().map_or(0, |q| q.0);
            let fastest = fastest_ms(&r.shape);
            let mut out = Vec::new();
            for k in 1_u64.. {
                let release = us(every) * k + us(len);
                let next = us(every) * (k + 1);
                if release + 1_000_000 > end {
                    break;
                }
                let after = |t: Option<u64>| t.map_or(0, |t| (t - release) / 1000);
                let queue = r
                    .queue_at
                    .iter()
                    .filter(|q| q.0 >= release && q.0 + BAR_MS * 1000 < next)
                    .filter(|q| q.1 >= BAR_MS * 1000)
                    .map(|q| q.0)
                    .max();
                let transit = r
                    .arrivals
                    .iter()
                    .filter(|x| x.at_us >= release && x.at_us < next)
                    .filter(|x| transit_ms(x).saturating_sub(fastest) >= BAR_MS)
                    .map(|x| x.at_us)
                    .max();
                out.push((release / 1000, after(queue), after(transit)));
            }
            out
        }

        fn print_recoveries(r: &TcpRun) {
            for (release, queue, transit) in recoveries(r) {
                println!(
                    "D46 outage | {} | the outage ending at {release} ms: link queue under \
                     {BAR_MS} ms from {queue} ms after it, transit from {transit} ms",
                    r.shape.name()
                );
            }
        }

        /// The budget's fresh delivery over today's writer's, the bar 0.95
        /// (D46 item 11), printed whichever way it falls.
        fn print_fresh_ratio(today: &Summary, budget: &(TcpRun, Summary)) {
            let ratio = budget.1.fresh_bps as f64 / today.fresh_bps.max(1) as f64;
            let read = budget.1.rx_bps as f64 / today.rx_bps.max(1) as f64;
            println!(
                "D46 | {} | fresh delivery {:.3} of today's writer's ({} against {} kbit/s); \
                 the bar is 0.95: {}; read delivery {:.3} of today's writer's ({} against {} \
                 kbit/s)",
                budget.0.shape.name(),
                ratio,
                kbps(budget.1.fresh_bps),
                kbps(today.fresh_bps),
                if ratio >= 0.95 { "over it" } else { "UNDER it" },
                read,
                kbps(budget.1.rx_bps),
                kbps(today.rx_bps),
            );
        }

        fn budgeted(shape: Shape) -> Shape {
            Shape {
                skip_over: false,
                budget: Budgeted::Holds(SLACK),
                ..shape
            }
        }

        /// FR-148 D46 — the budget as B1 built it, measured beside the
        /// classifier's on the stall and outage shapes. Its rate is D46's: a
        /// second that starts inside a released still span counts from it.
        fn as_b1(shape: Shape) -> Shape {
            Shape {
                skip_over: false,
                budget: Budgeted::HoldsAsB1(SLACK),
                ..shape
            }
        }

        /// FR-148 D46 — the classifier asking two head-of-line releases in a
        /// row before a still span writes through, on the stall shapes.
        fn after_two(shape: Shape) -> Shape {
            Shape {
                skip_over: false,
                budget: Budgeted::HoldsAfterTwo(SLACK),
                ..shape
            }
        }

        fn shadowed(shape: Shape) -> Shape {
            Shape {
                budget: Budgeted::Shadow,
                ..shape
            }
        }

        /// FR-148 D46 B3 — the pumps' own budget under `on`, on a relay
        /// session, where the watermark arm sheds nothing.
        fn turned_on(shape: Shape) -> Shape {
            Shape {
                skip_over: false,
                budget: Budgeted::On,
                ..shape
            }
        }

        fn skipping(shape: Shape) -> Shape {
            Shape {
                skip_over: true,
                ..shape
            }
        }

        /// The bar on a TCP shape: the budget's standing queue under 300 ms in
        /// every second, on the link and end to end, while the viewer reads
        /// at least 0.9 of the link.
        fn on_the_bar(r: &TcpRun, s: &Summary) {
            let name = r.shape.name();
            assert!(s.link_ms.1 < 300, "{name}: {s:?}");
            assert!(s.transit_ms.1 < 300, "{name}: {s:?}");
            assert!(pct(s.rx_bps, r) >= 90, "{name}: {s:?}");
        }

        /// A deep shape under today's two writers, then under the budget, and
        /// under B1's beside it (FR-148 D46 B2: both budget writers' floors).
        async fn deep_against_today(shape: Shape) {
            let s = measured(&[shape, skipping(shape), budgeted(shape), as_b1(shape)]).await;
            print_fresh_ratio(&s[0].1, &s[2]);
            print_fresh_ratio(&s[0].1, &s[3]);
            on_the_bar(&s[2].0, &s[2].1);
        }

        /// A control under today's writer, then under the budget, which
        /// delivers at least 0.95 of what today's writer does, and under B1's
        /// budget in the same run (FR-148 D46 B2, B3's control measurement).
        async fn control_against_today(shape: Shape) {
            let s = measured(&[shape, budgeted(shape), as_b1(shape)]).await;
            print_fresh_ratio(&s[0].1, &s[1]);
            print_fresh_ratio(&s[0].1, &s[2]);
            assert!(
                s[1].1.rx_bps * 100 >= s[0].1.rx_bps * 95,
                "{}: {:?} against {:?}",
                s[1].0.shape.name(),
                s[1].1,
                s[0].1
            );
        }

        /// A stall shape under today's writer, then under B1's budget, whose
        /// floor ends lower (a stall releases at most the budget), then under
        /// the budget with the stall classifier, which delivers fresh at least
        /// 0.95 of what today's writer does (D46 item 11's bar), and last
        /// under the classifier asking two releases, printed only.
        async fn stalls_against_today(today: Shape) {
            let s = measured(&[today, as_b1(today), budgeted(today), after_two(today)]).await;
            print_fresh_ratio(&s[0].1, &s[1]);
            print_fresh_ratio(&s[0].1, &s[2]);
            print_fresh_ratio(&s[0].1, &s[3]);
            assert!(
                s[1].1.floor < s[0].1.floor,
                "{}: {:?} against {:?}",
                s[1].0.shape.name(),
                s[1].1,
                s[0].1
            );
            assert!(
                s[2].1.fresh_bps * 100 >= s[0].1.fresh_bps * 95,
                "{}: {:?} against {:?}",
                s[2].0.shape.name(),
                s[2].1,
                s[0].1
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_on_the_deep_2_mbps_path_i() {
            deep_against_today(shape(2_000_000, DEEP, OpenPath::InitAck)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_on_the_deep_2_mbps_path_c() {
            deep_against_today(shape(2_000_000, DEEP, OpenPath::CookieEcho)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_on_the_deep_0_4_mbps_path_i() {
            deep_against_today(shape(400_000, DEEP, OpenPath::InitAck)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_on_the_deep_0_4_mbps_path_c() {
            deep_against_today(shape(400_000, DEEP, OpenPath::CookieEcho)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_on_the_shallow_2_mbps_path_i_control() {
            control_against_today(shape(2_000_000, SHALLOW, OpenPath::InitAck)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_on_the_shallow_2_mbps_path_c_control() {
            control_against_today(shape(2_000_000, SHALLOW, OpenPath::CookieEcho)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_on_the_shallow_0_4_mbps_path_i_control() {
            control_against_today(shape(400_000, SHALLOW, OpenPath::InitAck)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_on_the_shallow_0_4_mbps_path_c_control() {
            control_against_today(shape(400_000, SHALLOW, OpenPath::CookieEcho)).await;
        }

        /// FR-148 D46, the drop step — when the link's rate halves, the
        /// fall's time from the run's start.
        const HALVES_AT: Duration = Duration::from_secs(6);

        /// A 2 Mbit/s link on path I that falls to 1 Mbit/s at [`HALVES_AT`]
        /// for good, `queue` deep, offered 1.5 times its first rate.
        fn halving(queue: Duration) -> Shape {
            Shape {
                step: Some((HALVES_AT, 1_000_000)),
                ..shape(2_000_000, queue, OpenPath::InitAck)
            }
        }

        /// FR-148 D46, the drop step — the risk its rules carry: a path that
        /// drops because its rate fell, not because the budget overran a
        /// shallow queue. Under the two budget writers, printed only: the drop
        /// lines read each rule set against the link after the fall.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the drop rules on a link whose rate halves"]
        async fn the_budget_when_a_shallow_2_mbps_link_halves_path_i() {
            measured(&[budgeted(halving(SHALLOW)), as_b1(halving(SHALLOW))]).await;
        }

        /// As the shallow one, on the deep queue, which drops nothing: the
        /// samples after the fall read a standing queue.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the drop rules on a link whose rate halves"]
        async fn the_budget_when_a_deep_2_mbps_link_halves_path_i() {
            measured(&[budgeted(halving(DEEP)), as_b1(halving(DEEP))]).await;
        }

        /// FR-148 D48 — an opener at four times a 0.4 Mbit/s shallow link:
        /// the budget's rate is a stand-in (the opening rate) well over the
        /// link while the first holes are released.
        fn opener(path: OpenPath) -> Shape {
            Shape {
                offer_pct: 400,
                ..shape(400_000, SHALLOW, path)
            }
        }

        /// FR-148 D48 — a viewer that reads at half a 2 Mbit/s shallow link
        /// through a 128 KiB receive window: the window fills, and the
        /// session's limit is the viewer, not the link.
        fn slow_viewer(path: OpenPath) -> Shape {
            Shape {
                viewer: Some((2_000_000 * PAYLOAD / PACKET / 2, 128 * 1024)),
                ..shape(2_000_000, SHALLOW, path)
            }
        }

        /// FR-148 D48 — a 2 Mbit/s shallow link with a 200 ms round trip:
        /// SCTP's window takes longer to fill the link, so a sample can be
        /// window-sized with nothing lost.
        fn long_round_trip(path: OpenPath) -> Shape {
            Shape {
                rtt: Duration::from_millis(200),
                ..shape(2_000_000, SHALLOW, path)
            }
        }

        /// FR-148 D48 — the opener under the two budget writers, printed
        /// only: the drop lines read the model's floor against the link.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the path model at an opener over a shallow link"]
        async fn the_budget_opening_at_4_x_a_shallow_0_4_mbps_link_path_i() {
            let s = opener(OpenPath::InitAck);
            measured(&[budgeted(s), as_b1(s)]).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the path model at an opener over a shallow link"]
        async fn the_budget_opening_at_4_x_a_shallow_0_4_mbps_link_path_c() {
            let s = opener(OpenPath::CookieEcho);
            measured(&[budgeted(s), as_b1(s)]).await;
        }

        /// FR-148 D48 — the slow viewer under the two budget writers, printed
        /// only: the drop lines read the model against the viewer's rate.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the path model behind a viewer slower than the link"]
        async fn the_budget_when_the_viewer_reads_at_half_a_shallow_2_mbps_link_path_i() {
            let s = slow_viewer(OpenPath::InitAck);
            measured(&[budgeted(s), as_b1(s)]).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the path model behind a viewer slower than the link"]
        async fn the_budget_when_the_viewer_reads_at_half_a_shallow_2_mbps_link_path_c() {
            let s = slow_viewer(OpenPath::CookieEcho);
            measured(&[budgeted(s), as_b1(s)]).await;
        }

        /// FR-148 D48 — the long round trip under the two budget writers,
        /// printed only.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the path model on a long round trip over a shallow link"]
        async fn the_budget_on_a_shallow_2_mbps_link_with_a_200_ms_round_trip_path_i() {
            let s = long_round_trip(OpenPath::InitAck);
            measured(&[budgeted(s), as_b1(s)]).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the path model on a long round trip over a shallow link"]
        async fn the_budget_on_a_shallow_2_mbps_link_with_a_200_ms_round_trip_path_c() {
            let s = long_round_trip(OpenPath::CookieEcho);
            measured(&[budgeted(s), as_b1(s)]).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_under_stalls_2_mbps_path_i() {
            stalls_against_today(stalled(OpenPath::InitAck)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_under_stalls_2_mbps_path_c() {
            stalls_against_today(stalled(OpenPath::CookieEcho)).await;
        }

        /// Today's writer here is the VP9-444 pump's skip: at 1.5 times the
        /// link a writer without one queues without end.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_under_stalls_at_1_5_x_2_mbps_path_i() {
            stalls_against_today(skipping(Shape {
                offer_pct: 150,
                ..stalled(OpenPath::InitAck)
            }))
            .await;
        }

        /// FR-148 D46 — as the path I cell above, on path C.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_under_stalls_at_1_5_x_2_mbps_path_c() {
            stalls_against_today(skipping(Shape {
                offer_pct: 150,
                ..stalled(OpenPath::CookieEcho)
            }))
            .await;
        }

        /// FR-148 D46 — the outage cells' shape: the stall shape's link, but in
        /// each of its stalls the link neither serializes nor delivers
        /// ([`outaged`]), and drains what queued at its rate after it.
        fn outage(path: OpenPath) -> Shape {
            Shape {
                offer_pct: 80,
                outages: Some((STALL_EVERY, STALL_FOR)),
                ..shape(2_000_000, DEEP, path)
            }
        }

        /// FR-148 D46 — an outage shape under today's writer, B1's budget,
        /// then the budget with the stall classifier, which holds through
        /// each outage: after each, the link's queue and the transit are back
        /// under [`BAR_MS`] within a second, and it delivers at least what
        /// today's writer delivers fresh.
        async fn outages_against_today(today: Shape) {
            let s = measured(&[today, as_b1(today), budgeted(today)]).await;
            for (r, _) in &s {
                print_recoveries(r);
            }
            print_fresh_ratio(&s[0].1, &s[1]);
            print_fresh_ratio(&s[0].1, &s[2]);
            let (r, b) = (&s[2].0, &s[2].1);
            let name = r.shape.name();
            let back = recoveries(r);
            assert!(
                !back.is_empty(),
                "{name}: no outage ended a second before the run"
            );
            for (release, queue, transit) in back {
                assert!(
                    queue < 1_000 && transit < 1_000,
                    "{name}: the outage ending at {release} ms: the link queue stood at \
                     {BAR_MS} ms {queue} ms after it, a transit {transit} ms after it"
                );
            }
            assert!(
                b.fresh_bps >= s[0].1.fresh_bps,
                "{name}: {b:?} against {:?}",
                s[0].1
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_through_outages_2_mbps_path_i() {
            outages_against_today(outage(OpenPath::InitAck)).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_through_outages_2_mbps_path_c() {
            outages_against_today(outage(OpenPath::CookieEcho)).await;
        }

        /// Today's writer at 1.5 times the link is the VP9-444 pump's skip, as
        /// under the stalls.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_through_outages_at_1_5_x_2_mbps_path_i() {
            outages_against_today(skipping(Shape {
                offer_pct: 150,
                ..outage(OpenPath::InitAck)
            }))
            .await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget against today's writers"]
        async fn the_budget_through_outages_at_1_5_x_2_mbps_path_c() {
            outages_against_today(skipping(Shape {
                offer_pct: 150,
                ..outage(OpenPath::CookieEcho)
            }))
            .await;
        }

        /// FR-148 D46 — the slack's choice, each slack on the same three
        /// shapes: the deep 2 Mbit/s path I shape (the queue the slack lets
        /// stand), the stall shape at 1.5 times the link (what a head-of-line
        /// stall costs while the budget is out), and the 2 Mbit/s path I
        /// control (what it costs where SCTP's window governs). The standing
        /// queue follows the slack.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget at three slacks"]
        async fn the_budget_at_75_150_and_300_ms_of_slack() {
            let deep_shape = shape(2_000_000, DEEP, OpenPath::InitAck);
            let stall_shape = Shape {
                offer_pct: 150,
                ..stalled(OpenPath::InitAck)
            };
            let control_shape = shape(2_000_000, SHALLOW, OpenPath::InitAck);
            let mut standing = Vec::new();
            for slack in [75, 150, 300].map(Duration::from_millis) {
                let at = |s: Shape| Shape {
                    budget: Budgeted::Holds(slack),
                    ..s
                };
                let s = measured(&[at(deep_shape), at(stall_shape), at(control_shape)]).await;
                standing.push((ms(slack), s[0].1.link_ms.0));
            }
            assert!(
                standing.windows(2).all(|w| w[0].1 < w[1].1),
                "the deep shape's median standing queue by slack: {standing:?}"
            );
        }

        /// FR-148 D46 B1 — the budget in shadow holds nothing. Today's plain
        /// writer on the deep 2 Mbit/s path I shape, with the budget off and
        /// then in shadow through the pumps' own [`Shadow`]: in both runs it
        /// writes a frame at every tick and holds and skips none, while the
        /// shadow's verdicts would have held frames. Then today's skip
        /// writer, off and in shadow, whose skips follow the link's timing:
        /// printed, not compared.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget in shadow against off"]
        async fn the_inflight_budget_in_shadow_holds_nothing_on_a_shaped_link() {
            let plain = shape(2_000_000, DEEP, OpenPath::InitAck);
            let s = measured(&[
                plain,
                shadowed(plain),
                skipping(plain),
                shadowed(skipping(plain)),
            ])
            .await;
            for (r, _) in &s[..2] {
                let name = r.shape.name();
                assert_eq!((r.held, r.skipped), (0, 0), "{name}: held or skipped");
                // An abort can land between a tick and its write.
                assert!(
                    r.written > 0 && r.ticks.saturating_sub(r.written) <= 1,
                    "{name}: {} ticks, {} written",
                    r.ticks,
                    r.written
                );
            }
            let (off, shadow) = (&s[0].0, &s[1].0);
            assert_eq!(off.would_hold, 0, "off computes nothing");
            assert!(
                shadow.would_hold > 0,
                "the shadow's verdicts would have held frames: {}",
                shadow.would_hold
            );
            for (r, _) in &s[2..] {
                println!(
                    "D46 | {}: ticks {}, written {}, skipped {}, held {}",
                    r.shape.name(),
                    r.ticks,
                    r.written,
                    r.skipped,
                    r.held
                );
            }
        }

        /// FR-148 D46 B3 — a shape off and under `on`: the frames held of the
        /// writer's ticks; the holds the line counts as acted, of the passes
        /// acted on, and those in still spans; the core's stall marks; the
        /// transit and the standing queue (median / most); and the read and
        /// fresh delivery, percent of the link.
        fn print_b3(off: &(TcpRun, Summary), on: &(TcpRun, Summary)) {
            let ((a, x), (b, y)) = (off, on);
            println!(
                "D46 B3 | {} | held {} of {} ticks (off {} of {}); acted holds {} of {} \
                 passes, {} in still spans; stall marks {} | transit {} / {} ms (off {} / \
                 {}) | standing link {} / {} ms (off {} / {}) | read {} %, fresh {} % (off \
                 {} %, {} %)",
                b.shape.name(),
                b.held,
                b.ticks,
                a.held + a.skipped,
                a.ticks,
                b.acted.0,
                b.acted.1,
                b.acted.2,
                b.marks.len(),
                y.transit_ms.0,
                y.transit_ms.1,
                x.transit_ms.0,
                x.transit_ms.1,
                y.link_ms.0,
                y.link_ms.1,
                x.link_ms.0,
                x.link_ms.1,
                pct(y.rx_bps, b),
                pct(y.fresh_bps, b),
                pct(x.rx_bps, a),
                pct(x.fresh_bps, a),
            );
        }

        /// FR-148 D46 B3 — the budget under `on` holds frames on a relay
        /// session. Today's plain writer on the deep 2 Mbit/s path I shape and
        /// on the stall shape, off and then under `on` through the pumps' own
        /// [`Shadow`] and its gate ([`Shadow::gate_with`], [`Shadow::shut`]):
        /// off holds nothing; under `on` the gate holds frames, each a hold the
        /// line counts as acted, and sheds none; on the deep shape the standing
        /// queue and the transit are on the bar, and on the stall shape fresh
        /// delivery is at least 0.95 of off's (D46 item 11's bar).
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the in-flight budget on against off"]
        async fn the_inflight_budget_on_holds_frames_on_a_shaped_relay() {
            let deep = shape(2_000_000, DEEP, OpenPath::InitAck);
            let stall = stalled(OpenPath::InitAck);
            let s = measured(&[deep, turned_on(deep), stall, turned_on(stall)]).await;
            for pair in s.chunks(2) {
                let (off, on) = (&pair[0], &pair[1]);
                print_b3(off, on);
                print_fresh_ratio(&off.1, on);
                let name = on.0.shape.name();
                assert_eq!((off.0.held, off.0.skipped), (0, 0), "{name}: off held");
                assert!(on.0.held > 0, "{name}: the gate held nothing");
                assert_eq!(
                    on.0.held, on.0.acted.0,
                    "{name}: each hold is one the line counts"
                );
                assert_eq!(on.0.skipped, 0, "{name}: the watermark is out");
            }
            on_the_bar(&s[1].0, &s[1].1);
            assert!(
                s[3].1.fresh_bps * 100 >= s[2].1.fresh_bps * 95,
                "{}: {:?} against {:?}",
                s[3].0.shape.name(),
                s[3].1,
                s[2].1
            );
        }

        /// FR-148 D51 B4c — the closed-loop writer: the VP9-444 pump's own loop
        /// on a relay session (`peer.rs`, `media_pump_vp9_444_dc`), run on the
        /// shaped link against a viewer that reports as the browser does. The
        /// fixed writer above offers a constant, so it cannot show a sender
        /// that swings its own offer when the budget's queue appears, which is
        /// what slice B3T measured. This one runs the pump's gate and its 2 ms
        /// retry, a 30 fps capture, the real governor (its AIMD, fps cap, age
        /// loop, link loop and drain) and an encoder that cannot make a frame
        /// smaller than its floor (W-67's design, §4.5).
        mod closed_loop {
            use std::collections::BTreeMap;

            use super::*;
            use crate::encode::governor::{AppliedBitrate, GovernorFlags, RateGovernor};
            use crate::encode::viewer_rate::{self, ViewerFeedback};

            /// The lab path B3T ran on: 2 Mbit/s on the wire, of which SCTP's
            /// payload got E_sat = 0.8137 at saturation (the loaded `off` runs'
            /// mean E_measured), so the link carries C_sat of payload.
            const WIRE_BPS: u64 = 2_000_000;
            const E_SAT: (u64, u64) = (8_137, 10_000);
            /// cap.awk's modeled E for T: AC32's C, the lab's reported fill.
            const E_MODEL: (u64, u64) = (8_869, 10_000);
            /// T's base round trip: `on`'s median, 23-43 ms, is an empty queue.
            const BASE_RTT: Duration = Duration::from_millis(30);
            /// The encoder's least frame over a frame interval Δt, a + b·Δt
            /// bits, fitted through B3T's arm means (libvpx at its maximum
            /// quantizer on the motion content). Past `FLOOR_SPAN` a delta is
            /// as big as it gets; a keyframe is `KEY_FLOORS` floors of a 30 fps
            /// frame.
            const FLOOR_A_BITS: u64 = 50_000;
            const FLOOR_B_BPS: u64 = 450_000;
            const FLOOR_SPAN: Duration = Duration::from_millis(100);
            const KEY_FLOORS: u64 = 5;
            /// One encode, and one decode in the viewer's worker.
            const ENCODE: Duration = Duration::from_millis(10);
            const DECODE: Duration = Duration::from_millis(6);
            /// The capture's frame interval: 30 fps, the pump's target.
            const CAPTURE: Duration = Duration::from_nanos(1_000_000_000 / 30);
            const TARGET_FPS: u32 = 30;
            /// The relay's ceiling: `encode::relay_max_bps`'s default.
            const RELAY_CEILING: u32 = 3_000_000;
            /// The pump's send channel on a relay, its gate's retry, and its
            /// poll while a drain pauses production (`peer.rs`).
            const SEND_DEPTH: usize = 2;
            const GATE_RETRY: Duration = Duration::from_millis(2);
            const DRAIN_POLL: Duration = Duration::from_millis(10);
            /// The browser's decode-backlog rule (`rc-hop-stats.ts`): a window
            /// whose mean backlog is over this is bad, and this many bad windows
            /// in a row report the struggling bit.
            const STRUGGLE_MS: u64 = 33;
            const STRUGGLE_WINDOWS: u32 = 2;
            /// The viewer keeps its clock probe's least round trip of the last
            /// eight (`useRemoteControl.ts`, `CLOCK_RING`).
            const PROBE_RING: usize = 8;
            /// The run, and the opening its figures leave out.
            const RUN: Duration = Duration::from_secs(60);
            const OPENING: Duration = Duration::from_secs(5);
            /// A still screen's delta, bytes: what a desktop sends before
            /// anything on it moves.
            const STILL_DELTA: usize = 1_000;
            /// The `off` control's standing queue: each second's least.
            const STANDING_MS: u64 = 500;

            /// Which law the pump runs: today's (`off`: the VP9-444 watermark arm
            /// sheds a frame over 256 KiB buffered) or the budget acting (`on`).
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            enum Arm {
                Off,
                On,
            }

            impl Arm {
                fn word(self) -> &'static str {
                    match self {
                        Arm::Off => "off",
                        Arm::On => "on",
                    }
                }
            }

            /// The shaped link's packet rate, which carries C_sat of payload.
            fn packet_bps() -> u64 {
                WIRE_BPS * E_SAT.0 / E_SAT.1 * PACKET / PAYLOAD
            }

            /// The link's payload capacity (C_sat), and AC32's C.
            fn payload_bps() -> u64 {
                packet_bps() * PAYLOAD / PACKET
            }

            fn c_bps() -> u64 {
                WIRE_BPS * E_MODEL.0 / E_MODEL.1
            }

            /// How long a byte written now waits for the far end: the link's
            /// queue and SCTP's own pending bytes ahead of it.
            fn queued(link: &ShapedConn, gauges: &GaugeReader) -> u64 {
                us(queue_now(link)) + gauges.peek().pending_bytes * 8_000_000 / packet_bps()
            }

            /// The frame the encoder makes at `target_bps` over `dt`, bytes:
            /// never under its floor, a keyframe at `KEY_FLOORS` floors.
            fn frame_len(target_bps: u32, dt: Duration, key: bool) -> usize {
                let floor = |dt: Duration| FLOOR_A_BITS + FLOOR_B_BPS * us(dt) / 1_000_000;
                let bits = if key {
                    KEY_FLOORS * floor(CAPTURE)
                } else {
                    let dt = dt.min(FLOOR_SPAN);
                    (u64::from(target_bps) * us(dt) / 1_000_000).max(floor(dt))
                };
                usize::try_from(bits / 8).unwrap_or(usize::MAX).max(16)
            }

            /// One governor window, as the pump's `viewer-rate fps cap` line
            /// carries it (µs from `t0`).
            #[derive(Debug, Clone, Copy)]
            struct Win {
                t_us: u64,
                divisor: u32,
                cap_fps: u32,
                reported_fps: u32,
                struggling: bool,
                age: Option<(u16, u16)>,
                floor: Option<u16>,
                age_over: bool,
                link_congested: bool,
                drain_ms: Option<u32>,
                target_bps: u32,
            }

            /// What the pump did, pass by pass.
            #[derive(Debug, Default)]
            struct Pumped {
                passes: u64,
                /// Passes the budget's hold shut by itself, and the send
                /// channel's full ones.
                holds: u64,
                full: u64,
                drained: u64,
                /// Captured frames the divisor discarded.
                divided: u64,
                keys: u64,
                /// Encoded frames the watermark arm shed, and the frames a full
                /// send channel refused.
                shed: u64,
                refused: u64,
                /// When each frame was captured, encoded and handed to the
                /// send channel (µs from `t0`).
                captured_at: Vec<u64>,
                encoded_at: Vec<u64>,
                sent_at: Vec<u64>,
                /// The applied moves by their why, and the target over time.
                moves: BTreeMap<String, u32>,
                targets: Vec<(u64, u32)>,
                windows: Vec<Win>,
            }

            /// A frame in the viewer's worker: its write stamp, when its last
            /// byte arrived and when it painted, µs from `t0`.
            #[derive(Debug, Clone, Copy)]
            struct Painted {
                stamp: u64,
                arrival: u64,
                paint: u64,
            }

            /// One window as the browser reported it.
            #[derive(Debug, Clone, Copy)]
            struct Seen {
                t_us: u64,
                fps: u32,
                backlog_ms: u64,
                struggling: bool,
                age: Option<(u16, u16)>,
                arrival_ms: u16,
                probe_rtt_ms: u16,
                rx_bps: u32,
                queue_ms: Option<i64>,
            }

            fn note(p: &mut Pumped, t0: Instant, target: &mut u32, a: Option<AppliedBitrate>) {
                if let Some(a) = a {
                    *p.moves.entry(a.why.to_string()).or_default() += 1;
                    *target = a.bps;
                    p.targets.push((us(t0.elapsed()), a.bps));
                }
            }

            /// The pump: `peer.rs`'s VP9-444 loop on a relay, pass by pass. The
            /// budget's gate and the capacity gate, the 2 ms retry with the
            /// skip's decrease through the pass, the budget's step, the newest
            /// capture, the viewer window, the drain, the divisor, the governor's
            /// tick, the encode, the watermark arm and the send channel; and the
            /// send task behind it, one frame at a time into SCTP. Until `moves`
            /// the screen is still: a delta is `STILL_DELTA` whatever the target.
            #[allow(clippy::too_many_arguments)] // one pump's handles; a struct would only rename them
            async fn pump(
                arm: Arm,
                t0: Instant,
                start: Instant,
                moves: Instant,
                stop: Instant,
                stream: Arc<webrtc::sctp::stream::Stream>,
                clock: DeliveryClock,
                gauges: GaugeReader,
                feedback: Arc<ViewerFeedback>,
                written: Arc<AtomicU64>,
            ) -> Pumped {
                let mut governor = RateGovernor::new(
                    TARGET_FPS,
                    SEND_DEPTH,
                    GovernorFlags::default(),
                    0,
                    None,
                    Instant::now(),
                );
                let mut delivery = match arm {
                    Arm::Off => DeliverySampler::new(true),
                    Arm::On => DeliverySampler::new(true).with_budget_on(),
                };
                let (send_tx, mut send_rx) = mpsc::channel::<Bytes>(SEND_DEPTH);
                let sender = {
                    let stream = Arc::clone(&stream);
                    let count = delivery.pump_count();
                    let written = Arc::clone(&written);
                    let sink = governor.goodput_sink();
                    tokio::spawn(async move {
                        while let Some(frame) = send_rx.recv().await {
                            let mut unsent = count.unsent(frame.len());
                            let began = Instant::now();
                            if stream.write(&frame).await.is_err() {
                                break;
                            }
                            unsent.sent(frame.len());
                            written.fetch_add(frame.len() as u64, Ordering::Relaxed);
                            sink.record_frame(frame.len() as u64, began.elapsed(), frame[8] == 1);
                        }
                    })
                };
                let floor_bps = crate::encode::area_min_bitrate_bps(1920, 1080, true);
                let mut p = Pumped::default();
                let mut target = 0_u32;
                // libvpx's pending keyframe, and the pump's `keyframe_requested`:
                // a stream opens on a keyframe.
                let (mut force_key, mut key_asked) = (false, true);
                let mut taken: Option<u32> = None;
                let mut last_encoded: Option<Instant> = None;
                while Instant::now() < stop {
                    p.passes += 1;
                    delivery.gate_with(&mut governor, &clock, &gauges, true, false);
                    let full = send_tx.capacity() == 0;
                    if full || delivery.shuts() {
                        if full {
                            p.full += 1;
                        } else {
                            p.holds += 1;
                        }
                        delivery.note_gate(full, false);
                        let a = delivery.on_skip(false, |t| governor.on_backpressure_skip(t));
                        note(&mut p, t0, &mut target, a);
                        tokio::time::sleep(GATE_RETRY).await;
                        continue;
                    }
                    delivery.on_frame_with(&mut governor, &clock, &gauges);
                    // The capture: the newest frame not taken yet, or the next.
                    let newest = u32::try_from(
                        Instant::now().saturating_duration_since(start).as_nanos()
                            / CAPTURE.as_nanos(),
                    )
                    .unwrap_or(u32::MAX);
                    let k = match taken {
                        Some(t) if t >= newest => {
                            let due = start + CAPTURE * (t + 1);
                            tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
                            t + 1
                        }
                        _ => newest,
                    };
                    taken = Some(k);
                    let captured = start + CAPTURE * k;
                    p.captured_at.push(us(t0.elapsed()));
                    let forced = std::mem::take(&mut key_asked);
                    force_key |= forced;
                    if let Some(vw) = governor.tick_viewer_window(
                        Instant::now(),
                        TARGET_FPS,
                        || feedback.take_report(),
                        || feedback.take_age(),
                        || feedback.take_link(),
                        true,
                        |own| own,
                        written.load(Ordering::Relaxed),
                    ) {
                        p.windows.push(Win {
                            t_us: us(t0.elapsed()),
                            divisor: vw.skip_divisor,
                            cap_fps: vw.cap_fps,
                            reported_fps: vw.reported_fps,
                            struggling: vw.struggling,
                            age: vw.age_ms,
                            floor: governor.viewer_age().map(|(_, f)| f),
                            age_over: vw.age_over,
                            link_congested: vw.link_congested,
                            drain_ms: vw.drain_for_ms,
                            target_bps: governor.applied_bps(),
                        });
                    }
                    if governor.draining(Instant::now()) {
                        p.drained += 1;
                        tokio::time::sleep(DRAIN_POLL).await;
                        continue;
                    }
                    if governor.should_skip_delta_frame(forced) {
                        p.divided += 1;
                        continue;
                    }
                    let a = governor.pre_encode_tick(
                        RELAY_CEILING,
                        floor_bps,
                        true,
                        send_tx.capacity(),
                        Instant::now(),
                    );
                    note(&mut p, t0, &mut target, a);
                    tokio::time::sleep(ENCODE).await;
                    let key = std::mem::take(&mut force_key);
                    let dt =
                        last_encoded.map_or(CAPTURE, |l| captured.saturating_duration_since(l));
                    last_encoded = Some(captured);
                    let len = if captured < moves && !key {
                        STILL_DELTA
                    } else {
                        frame_len(target, dt, key)
                    };
                    p.encoded_at.push(us(t0.elapsed()));
                    p.keys += u64::from(key);
                    if delivery.sheds(stream.buffered_amount() as u64 > OVERFLOW) {
                        let a = governor.on_send_overflow(Instant::now());
                        note(&mut p, t0, &mut target, a);
                        p.shed += 1;
                        delivery.note_watermark();
                        key_asked = true;
                        continue;
                    }
                    let mut frame = vec![0x6b_u8; len];
                    frame[..8].copy_from_slice(&us(t0.elapsed()).to_le_bytes());
                    frame[8] = u8::from(key);
                    match send_tx.try_send(Bytes::from(frame)) {
                        Ok(()) => {
                            delivery.on_handed(len as u64);
                            p.sent_at.push(us(t0.elapsed()));
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            p.refused += 1;
                            delivery.note_full();
                            key_asked = true;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }
                drop(send_tx);
                let _ = tokio::time::timeout(Duration::from_secs(5), sender).await;
                p
            }

            /// The browser: decodes each frame as it arrives, paints it when
            /// decoded, sends a clock probe on every stats tick and keeps the
            /// least round trip of the last eight, whose echo waited behind
            /// the queue (so the age it maps is low by half that queue), and at
            /// each tick (a second, at its own phase) reports what
            /// `rc-dc-worker.ts` and `decodeStatWireMessage` report, a one-way
            /// trip later.
            async fn browser(
                t0: Instant,
                start: Instant,
                stop: Instant,
                arrivals: Arc<StdMutex<Vec<Arrival>>>,
                link: Arc<ShapedConn>,
                gauges: GaugeReader,
                feedback: Arc<ViewerFeedback>,
            ) -> Vec<Seen> {
                let mut seen = Vec::new();
                let mut next = 0;
                let mut decoder_free = 0_u64;
                let mut decoding: VecDeque<Painted> = VecDeque::new();
                let mut probes: VecDeque<(u64, u64)> = VecDeque::new();
                let mut last_painted: Option<(u64, u64)> = None;
                let mut bad = 0;
                let mut tick = start + Duration::from_millis(500);
                let mut prev = us(start.saturating_duration_since(t0));
                while tick < stop {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(tick)).await;
                    let now = us(t0.elapsed());
                    let queue = queued(&link, &gauges);
                    probes.push_back((us(BASE_RTT) + queue, queue));
                    if probes.len() > PROBE_RING {
                        probes.pop_front();
                    }
                    let best = probes
                        .iter()
                        .copied()
                        .min_by_key(|p| p.0)
                        .unwrap_or_default();
                    let mut rx = 0;
                    {
                        let log = arrivals.lock().unwrap();
                        for x in &log[next..] {
                            decoder_free = x.at_us.max(decoder_free) + us(DECODE);
                            decoding.push_back(Painted {
                                stamp: x.sent_us,
                                arrival: x.at_us,
                                paint: decoder_free,
                            });
                            rx += x.len;
                        }
                        next = log.len();
                    }
                    let mut painted = Vec::new();
                    while decoding.front().is_some_and(|f| f.paint <= now) {
                        painted.extend(decoding.pop_front());
                    }
                    let span = now.saturating_sub(prev).max(1);
                    prev = now;
                    let n = painted.len() as u64;
                    let fps = u32::try_from((n * 1_000_000 + span / 2) / span).unwrap_or(u32::MAX);
                    let backlog_ms =
                        painted.iter().map(|f| f.paint - f.arrival).sum::<u64>() / n.max(1) / 1_000;
                    bad = if n > 0 && backlog_ms > STRUGGLE_MS {
                        bad + 1
                    } else {
                        0
                    };
                    let struggling = bad >= STRUGGLE_WINDOWS;
                    // The probe's offset is low by half the queue its echo met.
                    let skew = best.1 / 2;
                    let age =
                        |at: u64, stamp: u64| at.saturating_sub(stamp).saturating_sub(skew) / 1_000;
                    let ms16 = |v: u64| u16::try_from(v).unwrap_or(u16::MAX);
                    let ages: Vec<u64> = painted.iter().map(|f| age(f.paint, f.stamp)).collect();
                    let age_pair = (n > 0).then(|| {
                        (
                            ms16(ages.iter().sum::<u64>() / n),
                            ms16(ages.iter().copied().min().unwrap_or(0)),
                        )
                    });
                    let arrival_ms = ms16(
                        painted.iter().map(|f| age(f.arrival, f.stamp)).sum::<u64>() / n.max(1),
                    )
                    .max(1);
                    // `QueueDrift` at paint: each frame against the one before,
                    // the last window's last frame included.
                    let mut drift: Option<i64> = None;
                    for f in &painted {
                        if let Some((stamp, paint)) = last_painted {
                            let wire = f.stamp as i64 - stamp as i64;
                            let arrived = f.paint as i64 - paint as i64;
                            if wire > 0 && wire <= 5_000_000 && (0..=5_000_000).contains(&arrived) {
                                *drift.get_or_insert(0) +=
                                    (arrived - wire).clamp(-1_000_000, 1_000_000);
                            }
                        }
                        last_painted = Some((f.stamp, f.paint));
                    }
                    let queue_ms = drift.map(|d| d / 1_000);
                    let rx_bps = u32::try_from(rx * 8_000_000 / span).unwrap_or(u32::MAX);
                    let probe_rtt_ms = ms16(best.0 / 1_000);
                    seen.push(Seen {
                        t_us: now,
                        fps,
                        backlog_ms,
                        struggling,
                        age: age_pair,
                        arrival_ms,
                        probe_rtt_ms,
                        rx_bps,
                        queue_ms,
                    });
                    tokio::time::sleep(BASE_RTT / 2).await;
                    feedback.store_report(viewer_rate::pack_report(fps, struggling));
                    if let Some((avg, min)) = age_pair {
                        feedback.store_age(viewer_rate::pack_age_with_arrival(
                            avg,
                            min,
                            probe_rtt_ms,
                            arrival_ms,
                        ));
                    }
                    if let Some(q) = queue_ms {
                        let q = i16::try_from(q.clamp(i16::MIN.into(), i16::MAX.into())).unwrap();
                        feedback.store_link(viewer_rate::pack_link(rx_bps, q));
                    }
                    tick += SAMPLE;
                }
                seen
            }

            #[derive(Debug)]
            struct LoopRun {
                arm: Arm,
                /// Where the figures start (the opening's end) and the run's
                /// stop, µs from `t0`.
                from_us: u64,
                stop_us: u64,
                pumped: Pumped,
                seen: Vec<Seen>,
                arrivals: Vec<Arrival>,
                /// The queue a byte written then would wait behind, every poll.
                queue_at: Vec<(u64, u64)>,
            }

            /// One arm on B3T's path: `still` of a still screen, then `RUN` of
            /// motion, whose first `OPENING` the figures leave out.
            async fn run_loop(arm: Arm, still: Duration) -> LoopRun {
                let _turn = TURN.lock().await;
                #[cfg(target_os = "windows")]
                let _timer = crate::win_timer::TimerResolutionGuard::request_1ms();
                let t0 = Instant::now();
                let (a, b) = shaped(packet_bps(), BASE_RTT / 2, DEEP);
                let a = Arc::new(a);
                let arrivals: Arc<StdMutex<Vec<Arrival>>> = Arc::default();
                let (agent, viewer) = pair_timed(
                    a.clone(),
                    Arc::new(b),
                    OpenPath::InitAck,
                    t0,
                    Arc::clone(&arrivals),
                    None,
                )
                .await;
                let stream = agent
                    .open_stream(1, PayloadProtocolIdentifier::Binary)
                    .await
                    .unwrap();
                let (clock, gauges) = (agent.delivery_clock(), agent.gauges());
                let feedback = Arc::new(ViewerFeedback::new());
                let written = Arc::new(AtomicU64::new(0));
                let start = Instant::now();
                let moves = start + still;
                let stop = moves + RUN;
                let pumping = tokio::spawn(pump(
                    arm,
                    t0,
                    start,
                    moves,
                    stop,
                    Arc::clone(&stream),
                    clock,
                    gauges.clone(),
                    Arc::clone(&feedback),
                    Arc::clone(&written),
                ));
                let viewing = tokio::spawn(browser(
                    t0,
                    start,
                    stop,
                    Arc::clone(&arrivals),
                    Arc::clone(&a),
                    gauges.clone(),
                    Arc::clone(&feedback),
                ));
                let mut queue_at = Vec::new();
                while Instant::now() < stop {
                    queue_at.push((us(t0.elapsed()), queued(&a, &gauges)));
                    tokio::time::sleep(POLL).await;
                }
                let pumped = pumping.await.unwrap();
                let seen = viewing.await.unwrap();
                let arrivals = arrivals.lock().unwrap().clone();
                let _ = tokio::time::timeout(Duration::from_secs(5), agent.close()).await;
                let _ = tokio::time::timeout(Duration::from_secs(5), viewer.close()).await;
                LoopRun {
                    arm,
                    from_us: us((moves + OPENING).saturating_duration_since(t0)),
                    stop_us: us(stop.saturating_duration_since(t0)),
                    pumped,
                    seen,
                    arrivals,
                    queue_at,
                }
            }

            /// The `q`-th percentile of `v`, sorted in place; 0 for none.
            fn quantile(v: &mut [u64], q: usize) -> u64 {
                if v.is_empty() {
                    return 0;
                }
                v.sort_unstable();
                v[(v.len() - 1) * q / 100]
            }

            /// What a run reads from its opening's end to its stop.
            #[derive(Debug, Clone, Copy)]
            struct Read {
                /// Delivered payload, bit/s, and its share of the payload
                /// capacity (C_sat) and of AC32's C, thousandths.
                rx_bps: u64,
                of_payload: u64,
                of_c: u64,
                /// Frames captured, encoded and sent a second, tenths.
                cap_fps: u64,
                enc_fps: u64,
                sent_fps: u64,
                /// Windows whose divisor was 2 or more, of the windows.
                divided: (usize, usize),
                /// Frame transit, write to read, and a probe's round trip,
                /// base and queue: median and p95, ms.
                transit: (u64, u64),
                rtt: (u64, u64),
                /// Seconds whose queue never fell under `STANDING_MS`, of the
                /// seconds; the median second's least queue, ms.
                standing: (usize, usize),
                least_ms: u64,
            }

            fn read(r: &LoopRun) -> Read {
                let (from, to) = (r.from_us, r.stop_us);
                let span = to - from;
                let inside = |t: u64| t >= from && t < to;
                let got: Vec<&Arrival> = r.arrivals.iter().filter(|x| inside(x.at_us)).collect();
                let rx_bps = got.iter().map(|x| x.len).sum::<u64>() * 8_000_000 / span;
                let per_s =
                    |v: &[u64]| v.iter().filter(|&&t| inside(t)).count() as u64 * 10_000_000 / span;
                let windows: Vec<&Win> =
                    r.pumped.windows.iter().filter(|w| inside(w.t_us)).collect();
                let mut transit: Vec<u64> = got
                    .iter()
                    .map(|x| x.at_us.saturating_sub(x.sent_us) / 1_000)
                    .collect();
                let mut rtt: Vec<u64> = r
                    .queue_at
                    .iter()
                    .filter(|q| inside(q.0))
                    .map(|q| (us(BASE_RTT) + q.1) / 1_000)
                    .collect();
                let mut least: BTreeMap<u64, u64> = BTreeMap::new();
                for &(t, q) in r.queue_at.iter().filter(|q| inside(q.0)) {
                    let s = least.entry((t - from) / 1_000_000).or_insert(u64::MAX);
                    *s = (*s).min(q / 1_000);
                }
                let mut leasts: Vec<u64> = least.values().copied().collect();
                Read {
                    rx_bps,
                    of_payload: rx_bps * 1_000 / payload_bps(),
                    of_c: rx_bps * 1_000 / c_bps(),
                    cap_fps: per_s(&r.pumped.captured_at),
                    enc_fps: per_s(&r.pumped.encoded_at),
                    sent_fps: per_s(&r.pumped.sent_at),
                    divided: (
                        windows.iter().filter(|w| w.divisor >= 2).count(),
                        windows.len(),
                    ),
                    transit: (quantile(&mut transit, 50), quantile(&mut transit, 95)),
                    rtt: (quantile(&mut rtt, 50), quantile(&mut rtt, 95)),
                    standing: (
                        leasts.iter().filter(|&&q| q > STANDING_MS).count(),
                        leasts.len(),
                    ),
                    least_ms: quantile(&mut leasts, 50),
                }
            }

            fn print_loop(r: &LoopRun, x: &Read) {
                let p = &r.pumped;
                let thousandths = |v: u64| format!("{}.{:03}", v / 1_000, v % 1_000);
                let tenths = |v: u64| format!("{}.{}", v / 10, v % 10);
                println!(
                    "B4c | {} | payload {} kbit/s, C {} kbit/s | delivered {} kbit/s = {} of payload, \
                     {} of C | captured {} fps, encoded {} fps, sent {} fps, discarded {} /s | divisor >= 2 in {} of \
                     {} windows | transit p50 {} / p95 {} ms | rtt p50 {} / p95 {} ms | standing {} of {} \
                     s, least queue median {} ms",
                    r.arm.word(),
                    kbps(payload_bps()),
                    kbps(c_bps()),
                    kbps(x.rx_bps),
                    thousandths(x.of_payload),
                    thousandths(x.of_c),
                    tenths(x.cap_fps),
                    tenths(x.enc_fps),
                    tenths(x.sent_fps),
                    tenths(x.cap_fps.saturating_sub(x.enc_fps)),
                    x.divided.0,
                    x.divided.1,
                    x.transit.0,
                    x.transit.1,
                    x.rtt.0,
                    x.rtt.1,
                    x.standing.0,
                    x.standing.1,
                    x.least_ms,
                );
                let windows = || p.windows.iter().filter(|w| w.t_us >= r.from_us);
                println!(
                    "B4c | {} | passes {}, holds {}, channel full {}, drained {}, divided {}, keys {}, \
                     shed {}, refused {} | age_over {} windows, link congested {}, drains {}, struggling \
                     {} | moves {:?}",
                    r.arm.word(),
                    p.passes,
                    p.holds,
                    p.full,
                    p.drained,
                    p.divided,
                    p.keys,
                    p.shed,
                    p.refused,
                    windows().filter(|w| w.age_over).count(),
                    windows().filter(|w| w.link_congested).count(),
                    windows().filter(|w| w.drain_ms.is_some()).count(),
                    windows().filter(|w| w.struggling).count(),
                    p.moves,
                );
                for w in &p.windows {
                    let s = r.seen.iter().rev().find(|s| s.t_us <= w.t_us);
                    println!(
                        "B4c | {} | t {:>5} ms | divisor {} cap {:>2} reported {:>2}{} | age {:?} floor {:?}{}{}{} | \
                         target {:>7} | viewer: fps {}{}, age {:?}, arrival {} ms, rtt {} ms, rx {} kbit/s, \
                         growth {:?} ms, backlog {} ms",
                        r.arm.word(),
                        w.t_us / 1_000,
                        w.divisor,
                        w.cap_fps,
                        w.reported_fps,
                        if w.struggling { " struggling" } else { "" },
                        w.age,
                        w.floor,
                        if w.age_over { " AGE_OVER" } else { "" },
                        if w.link_congested { " LINK" } else { "" },
                        w.drain_ms
                            .map(|d| format!(" DRAIN {d}"))
                            .unwrap_or_default(),
                        w.target_bps,
                        s.map_or(0, |s| s.fps),
                        if s.is_some_and(|s| s.struggling) {
                            " struggling"
                        } else {
                            ""
                        },
                        s.and_then(|s| s.age),
                        s.map_or(0, |s| s.arrival_ms),
                        s.map_or(0, |s| s.probe_rtt_ms),
                        s.map_or(0, |s| u64::from(s.rx_bps) / 1_000),
                        s.and_then(|s| s.queue_ms),
                        s.map_or(0, |s| s.backlog_ms),
                    );
                }
            }

            /// FR-148 D51 C6 — the budget `on` with the pump's own loop, against
            /// today's law (`off`), each 60 s on B3T's path with the closed-loop
            /// writer. `on`: the fill is at least 0.95 of the payload capacity,
            /// the divisor stays at 1 in at least 0.9 of the windows, and the
            /// transit's p95 is under 300 ms. `off`, the control: the fill is
            /// at least 0.95 too, over a standing queue of more than 500 ms.
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the pump's own loop on B3T's path, off and on"]
            async fn budget_on_with_the_pump_loop() {
                let off = run_loop(Arm::Off, Duration::ZERO).await;
                let x = read(&off);
                print_loop(&off, &x);
                let on = run_loop(Arm::On, Duration::ZERO).await;
                let y = read(&on);
                print_loop(&on, &y);
                assert!(x.of_payload >= 950, "off: fill {x:?}");
                assert!(x.least_ms > STANDING_MS, "off: no standing queue {x:?}");
                assert!(y.of_payload >= 950, "on: fill {y:?}");
                assert!(y.divided.0 * 10 <= y.divided.1, "on: the divisor {y:?}");
                assert!(y.transit.1 < 300, "on: transit {y:?}");
            }

            /// FR-148 D51 — the same two arms after 10 s of a still screen, so
            /// the age loop's floor and the viewer's clock probe learn an empty
            /// queue before anything moves. A probe of the swing's start: it
            /// prints, and asserts only that the run read.
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "a real-time measurement, not a CI gate (FR-148 D44): the pump's own loop on B3T's path after a still opening"]
            async fn budget_on_after_a_still_opening() {
                const STILL: Duration = Duration::from_secs(10);
                let off = run_loop(Arm::Off, STILL).await;
                let x = read(&off);
                print_loop(&off, &x);
                let on = run_loop(Arm::On, STILL).await;
                let y = read(&on);
                print_loop(&on, &y);
                assert!(
                    x.divided.1 > 0 && y.divided.1 > 0,
                    "no windows: {x:?} {y:?}"
                );
            }
        }
    }
}

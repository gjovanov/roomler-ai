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
//! ⚠️ Nothing reads these numbers: no rate, pacing or queue budget, and neither
//! the viewer nor the wire changes. `ROOMLERD_PATH_MODEL=off` takes no sample and
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

use std::path::Path;
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::sctp::association::delivery::{Delivery, DeliveryClock};
use webrtc::sctp::association::gauges::{GaugeReader, Gauges};

use crate::encode::pipe::{PathNumber, PathView};
use crate::media_inflight::{Bound, PumpCount, Shadow, WindowReport};

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
        }
    }

    /// A sampler with the in-flight budget in shadow, as
    /// `ROOMLERD_INFLIGHT_BUDGET=shadow` gives one.
    #[cfg(test)]
    pub(crate) fn with_budget(mut self) -> Self {
        self.budget = Shadow::for_pump(crate::media_inflight::Mode::Shadow, self.on).map(Box::new);
        self
    }

    /// The pump's one call, at its heartbeat: read the association's clock and
    /// its gauges, close a sample when one is due, and write the line when it
    /// is due; and take the path numbers from `view` (step 3), writing
    /// [`MODEL_LINE`] when that is due. Never waits: before the association
    /// exists, or while its transport's slot is held, the clock and gauges it
    /// already has answer, or nothing does. Off, it calls none of them.
    pub fn heartbeat(
        &mut self,
        pc: &RTCPeerConnection,
        session_id: ObjectId,
        view: impl FnOnce(Instant) -> PathView,
    ) {
        if self.on {
            let now = Instant::now();
            self.observe(
                now,
                session_id,
                || pc.sctp().delivery_clock(),
                || pc.sctp().gauges(),
            );
            self.on_view(now, session_id, view);
        }
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
    /// capture, after today's gates let the frame through. The applied rate
    /// stands for the opening rate, or before one is applied the rate the pump
    /// opened at. Returns nothing: in shadow the verdict reaches the line's
    /// counters and nothing else. Off, it reads nothing.
    pub fn on_frame(&mut self, pc: &RTCPeerConnection, applied_bps: u32, open_at_bps: Option<u32>) {
        if let Some(b) = self.budget.as_mut() {
            b.on_frame(pc, applied_bps, open_at_bps);
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
    pub fn note_gate(&mut self, full: bool, ledger: bool) {
        if let Some(b) = self.budget.as_mut() {
            if full {
                b.note_bound(Bound::ChannelFull);
            }
            if ledger {
                b.note_bound(Bound::Ledger);
            }
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

    /// One frame's reading for the budget, as [`Self::on_frame`] reads it from
    /// the association.
    #[cfg(test)]
    pub(crate) fn on_frame_reading(&mut self, now: Instant, acked: u64, g: Gauges, open_bps: u64) {
        if let Some(b) = self.budget.as_mut() {
            b.on_reading(now, acked, g, open_bps);
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

    /// FR-148 D46 B1 — `on` runs as `shadow`, with one warning a pump; and a
    /// sampler that is off carries no budget.
    #[test]
    fn the_inflight_budget_on_runs_as_shadow_with_one_warning() {
        use crate::media_inflight::Mode as BudgetMode;
        let mut pumps = Vec::new();
        let out = logged(|| {
            pumps.push(Shadow::for_pump(BudgetMode::On, true).is_some());
            pumps.push(Shadow::for_pump(BudgetMode::Shadow, true).is_some());
            pumps.push(Shadow::for_pump(BudgetMode::Off, true).is_some());
        });
        assert_eq!(pumps, [true, true, false]);
        assert_eq!(out.matches(" WARN ").count(), 1, "{out:?}");
        assert!(
            out.contains("ROOMLERD_INFLIGHT_BUDGET=on runs as shadow"),
            "{out:?}"
        );
        let off = logged(|| assert!(Shadow::for_pump(BudgetMode::Shadow, false).is_none()));
        assert!(off.contains("the in-flight budget stays off"), "{off:?}");
        assert!(
            DeliverySampler::new(false)
                .with_budget()
                .budget_mut()
                .is_none()
        );
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
            let wire = Duration::from_nanos(b.len() as u64 * 8 * 1_000_000_000 / self.bps);
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
        use crate::encode::pipe::{Basis, Pipe};
        use crate::encode::viewer_rate::{LinkLoop, OfferLoop, offered_over_arrival};
        use crate::media_inflight::{
            BURST_WITHIN, Budget, Classifier, Reading, Release, Verdict, WindowReport,
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
                };
                let stalls = match (self.stalls, self.outages) {
                    (Some((every, stall)), _) => format!(", a {stall:?} stall every {every:?}"),
                    (None, Some((every, len))) => format!(", a {len:?} outage every {every:?}"),
                    (None, None) => String::new(),
                };
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

            /// The link's payload rate.
            fn payload_bps(&self) -> u64 {
                self.bps * PAYLOAD / PACKET
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
            ms(RTT / 2) + wire * 8_000 / shape.bps
        }

        /// FR-148 D46 — the writer's core: the pumps' shadow, or the core
        /// itself where its verdict holds the frame.
        #[derive(Debug)]
        enum Core {
            Shadow(Box<Shadow>),
            Holds(Box<Budget>),
        }

        impl Core {
            fn of(budget: Budgeted) -> Option<Self> {
                match budget {
                    Budgeted::No => None,
                    Budgeted::Shadow => Some(Self::Shadow(Box::default())),
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
                    Self::Shadow(s) => s.budget(),
                    Self::Holds(b) => b,
                }
            }

            fn take_window(&mut self) -> WindowReport {
                match self {
                    Self::Shadow(s) => s.take_window(),
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

        /// As [`pair_opened`] with the default windows (1 MiB), and the viewer
        /// logs when each message arrived and the write time it carries in
        /// its first 8 bytes.
        async fn pair_timed(
            a: Arc<dyn Conn + Send + Sync>,
            b: Arc<dyn Conn + Send + Sync>,
            path: OpenPath,
            t0: Instant,
            log: Arc<StdMutex<Vec<Arrival>>>,
        ) -> (Association, Arc<Association>) {
            let config = |net_conn, name: &str| Config {
                net_conn,
                max_receive_buffer_size: 0,
                max_message_size: 0,
                name: name.to_owned(),
            };
            let (agent, viewer) = match path {
                OpenPath::InitAck => {
                    let agent = tokio::spawn(Association::client(config(a, "agent")));
                    let viewer = Association::server(config(b, "viewer")).await;
                    (agent.await.unwrap().unwrap(), viewer.unwrap())
                }
                OpenPath::CookieEcho => {
                    let viewer = tokio::spawn(Association::client(config(b, "viewer")));
                    let agent = Association::server(config(a, "agent")).await;
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
                        while let Ok(n @ 8..) = stream.read(&mut buf).await {
                            let at_us = us(t0.elapsed());
                            let sent_us = u64::from_le_bytes(buf[..8].try_into().unwrap());
                            log.lock().unwrap().push(Arrival {
                                at_us,
                                sent_us,
                                len: n as u64,
                            });
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
            let (a, b) = match (shape.stalls, shape.outages) {
                (Some((every, stall)), _) => stalling(shape.bps, shape.queue, t0, every, stall),
                (None, Some((every, len))) => outaged(shape.bps, shape.queue, t0, every, len),
                (None, None) => shaped(shape.bps, RTT / 2, shape.queue),
            };
            let a = Arc::new(a);
            let arrivals: Arc<StdMutex<Vec<Arrival>>> = Arc::default();
            let (agent, viewer) = pair_timed(
                a.clone(),
                Arc::new(b),
                shape.path,
                t0,
                Arc::clone(&arrivals),
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
                                    false
                                }
                                Core::Holds(b) => {
                                    let r = Reading {
                                        acked_bytes: clock.read().acked_bytes,
                                        gauges: gauges.peek(),
                                        pump_bytes: 0,
                                        open_bps: shape.offer_bps(),
                                    };
                                    let verdict = b.step(began, &r);
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
                    samples.push(Covered {
                        rows: covered..rows.len(),
                        sample,
                        end_us: us(now - t0),
                    });
                    covered = rows.len();
                }
                lines.extend(sampler.take_line(now).map(&with_budget));
                prev = (now, d);
            }
            lines.extend(sampler.take_rest().map(&with_budget));
            writer.abort();
            // Its counts are final once it has stopped.
            let _ = writer.await;
            let last_window = core.as_ref().map(|c| c.lock().unwrap().take_window());
            let budget_windows = || lines.iter().filter_map(|l| l.budget).chain(last_window);
            let would_hold: u64 = budget_windows().map(|b| b.held.0).sum();
            let released =
                budget_windows().fold((0, 0), |(h, d), b| (h + b.released.0, d + b.released.1));
            let still_admitted: u64 = budget_windows().map(|b| b.still_admitted).sum();
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
            /// class, or the budget held a frame in each of its seconds) and
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
                    Budgeted::Holds(_) | Budgeted::HoldsAsB1(_) | Budgeted::HoldsAfterTwo(_)
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
            println!(
                "D46 | {} | fresh delivery {:.3} of today's writer's ({} against {} kbit/s); \
                 the bar is 0.95: {}",
                budget.0.shape.name(),
                ratio,
                kbps(budget.1.fresh_bps),
                kbps(today.fresh_bps),
                if ratio >= 0.95 { "over it" } else { "UNDER it" },
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

        /// A deep shape under today's two writers, then under the budget.
        async fn deep_against_today(shape: Shape) {
            let s = measured(&[shape, skipping(shape), budgeted(shape)]).await;
            print_fresh_ratio(&s[0].1, &s[2]);
            on_the_bar(&s[2].0, &s[2].1);
        }

        /// A control under today's writer, then under the budget, which
        /// delivers at least 0.95 of what today's writer does.
        async fn control_against_today(shape: Shape) {
            let s = measured(&[shape, budgeted(shape)]).await;
            print_fresh_ratio(&s[0].1, &s[1]);
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
    }
}

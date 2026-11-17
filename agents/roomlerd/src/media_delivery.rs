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

use std::path::Path;
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::sctp::association::delivery::{Delivery, DeliveryClock};

use crate::encode::pipe::{PathNumber, PathView};

/// A sample closes at the first heartbeat at least this long after the last.
pub const SAMPLE_MIN: Duration = Duration::from_millis(1500);

/// How often the line is written. Each line folds about five samples.
pub const LINE_EVERY: Duration = Duration::from_secs(10);

/// The line's message, which a field read greps for.
pub const LINE: &str = "path delivery (FR-148 P0, shadow, ≈10s)";

/// FR-148's switch, `ROOMLERD_PATH_MODEL` (D28): `off` is the release before
/// P0 exactly (no delivery sample, no path memory, no line); `shadow` and `on`
/// sample and keep the path memory. Until P1 step 3, `on` behaves as `shadow`:
/// the opener reads today's rate memory in every mode.
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

/// One delivery sample: its length, the bytes acknowledged in it, its class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub ms: u64,
    pub bytes: u64,
    pub class: Class,
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
        Self { ms, bytes, class }
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

    fn add(&mut self, s: Sample) {
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
    /// The reading the next sample starts from.
    base: Option<(Instant, Delivery)>,
    fold: Fold,
    line_since: Option<Instant>,
    /// Whose line a drop writes.
    session: Option<ObjectId>,
    /// Step 3 — the latest path numbers, not yet written, and when the last
    /// [`MODEL_LINE`] was (its own [`LINE_EVERY`]).
    view: Option<PathView>,
    model_since: Option<Instant>,
}

impl DeliverySampler {
    /// On unless `ROOMLERD_PATH_MODEL=off`: the pump's one read of the switch.
    pub fn from_env() -> Self {
        let mode = mode();
        let mut sampler = Self::new(mode.samples());
        sampler.mode = mode;
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
            base: None,
            fold: Fold::default(),
            line_since: None,
            session: None,
            view: None,
            model_since: None,
        }
    }

    /// The pump's one call, at its heartbeat: read the association's clock,
    /// close a sample when one is due, and write the line when it is due; and
    /// take the path numbers from `view` (step 3), writing [`MODEL_LINE`] when
    /// that is due. Never waits: before the association exists, or while its
    /// transport's slot is held, the clock it already has answers, or nothing
    /// does. Off, it calls neither.
    pub fn heartbeat(
        &mut self,
        pc: &RTCPeerConnection,
        session_id: ObjectId,
        view: impl FnOnce(Instant) -> PathView,
    ) {
        if self.on {
            let now = Instant::now();
            self.observe(now, session_id, || pc.sctp().delivery_clock());
            self.on_view(now, session_id, view);
        }
    }

    /// [`Self::heartbeat`]'s samples at `now`, with the clock from `clock`. Off,
    /// it calls nothing and returns `None`.
    pub fn observe(
        &mut self,
        now: Instant,
        session_id: ObjectId,
        clock: impl FnOnce() -> Option<DeliveryClock>,
    ) -> Option<Sample> {
        if !self.on {
            return None;
        }
        self.session = Some(session_id);
        if let Some(fresh) = clock()
            && !self.clock.as_ref().is_some_and(|c| c.same(&fresh))
        {
            // A first association, or a new one: no sample spans the two.
            self.clock = Some(fresh);
            self.base = None;
        }
        let reading = self.clock.as_ref()?.read();
        let sample = self.on_reading(now, reading);
        if let Some(line) = self.take_line(now) {
            emit(session_id, &line);
        }
        sample
    }

    /// One reading of the clock at `now`. The first sets where the next sample
    /// starts; one at least [`SAMPLE_MIN`] after it closes that sample, which it
    /// returns; one sooner changes nothing. Off, it takes nothing.
    pub fn on_reading(&mut self, now: Instant, reading: Delivery) -> Option<Sample> {
        if !self.on {
            return None;
        }
        self.line_since.get_or_insert(now);
        let Some((since, from)) = self.base else {
            self.base = Some((now, reading));
            return None;
        };
        let span = now.saturating_duration_since(since);
        if span < SAMPLE_MIN {
            return None;
        }
        let ms = u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
        let sample = Sample::between(from, reading, ms);
        self.fold.add(sample);
        self.base = Some((now, reading));
        Some(sample)
    }

    /// The line, once [`LINE_EVERY`] has passed since the last and it holds a
    /// sample.
    pub fn take_line(&mut self, now: Instant) -> Option<LineReport> {
        let since = self.line_since?;
        if now.saturating_duration_since(since) < LINE_EVERY || self.fold.is_empty() {
            return None;
        }
        self.line_since = Some(now);
        Some(std::mem::take(&mut self.fold).report)
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
}

impl Drop for DeliverySampler {
    /// A session left before its next line still has its last samples written,
    /// and its last path numbers.
    fn drop(&mut self) {
        let Some(session_id) = self.session else {
            return;
        };
        if !self.fold.is_empty() {
            emit(session_id, &std::mem::take(&mut self.fold).report);
        }
        if let Some(view) = self.view.take() {
            emit_model(session_id, &view);
        }
    }
}

/// The line. Its field names are pinned by a test, since a field read greps
/// for them.
pub fn emit(session_id: ObjectId, line: &LineReport) {
    tracing::info!(
        %session_id,
        span_ms = line.span_ms,
        delivery_samples = ?line.samples,
        backlogged_bps = ?line.backlogged_bps,
        app_limited_bps = ?line.app_limited_bps,
        stalled_ms = line.stalled_ms,
        acked_bytes = line.acked_bytes,
        "{LINE}"
    );
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};

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
    fn logged(f: impl FnOnce()) -> String {
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
        let out = logged(|| {
            let mut s = DeliverySampler::new(false);
            for i in 0..30u64 {
                let sample = s.observe(at(t0, i * 2_000), sid(), || {
                    asked.set(true);
                    None
                });
                assert_eq!(sample, None);
                assert_eq!(s.on_reading(at(t0, i * 2_000), r(i * 1_000, 1, 0)), None);
                assert_eq!(s.take_line(at(t0, i * 2_000)), None);
            }
            // Its drop writes nothing either.
        });
        assert!(!asked.get(), "off asked SCTP for its clock");
        assert!(out.is_empty(), "off wrote {out:?}");
    }

    #[test]
    fn the_line_names_its_fields() {
        let line = LineReport {
            span_ms: 10_012,
            samples: (2, 3, 0),
            backlogged_bps: Some((2_390_112, 2_401_000, 2_410_334)),
            app_limited_bps: Some(310_112),
            stalled_ms: 0,
            acked_bytes: 3_012_345,
        };
        let out = logged(|| emit(sid(), &line));
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
        ] {
            let at = rest
                .find(want)
                .unwrap_or_else(|| panic!("{want:?} is not in order in {out:?}"));
            rest = &rest[at + want.len()..];
        }
        assert_eq!(out.lines().count(), 1, "{out:?}");
        // An idle screen's line: every sample application-limited.
        let idle = LineReport {
            span_ms: 10_004,
            samples: (0, 5, 0),
            backlogged_bps: None,
            app_limited_bps: Some(16_384),
            stalled_ms: 0,
            acked_bytes: 20_480,
        };
        let idle = logged(|| emit(sid(), &idle));
        assert!(idle.contains("backlogged_bps=None"), "{idle:?}");
        // Its cost: at most one line a pump every LINE_EVERY, 360 a session
        // hour. A busy line has no field None.
        let per_hour = 3_600 / LINE_EVERY.as_secs();
        for (what, l) in [("busy", &out), ("idle", &idle)] {
            println!(
                "delivery line, {what}: {} bytes; {per_hour} lines and {} bytes a session hour",
                l.len(),
                l.len() as u64 * per_hour
            );
            assert!(l.len() < 400, "{} bytes: {l:?}", l.len());
        }
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
}

/// Done-criterion 3 of FR-148 P0's step 2: the sampler reads a known pipe. Two
/// in-process SCTP associations (the vendored webrtc-sctp, unmodified but for
/// the clock) over a link this module shapes: each direction serialises every
/// packet at the link's rate, holds it 10 ms more, and drops a packet that
/// would wait more than 200 ms in its queue. Real time, so each cell takes a
/// few seconds.
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
mod shaped_link {
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
        next_free: StdMutex<Instant>,
        due: mpsc::UnboundedSender<(Instant, Vec<u8>)>,
        /// Drops every n-th [`FULL_PACKET`] before its queue; 0 drops none.
        drop_every: u64,
        full_packets: AtomicU64,
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
            if start.saturating_duration_since(now) > QUEUE {
                return; // Dropped at the tail, as a full router queue does.
            }
            let wire = Duration::from_nanos(b.len() as u64 * 8 * 1_000_000_000 / self.bps);
            *next_free = start + wire;
            let _ = self.due.send((start + wire + DELAY, b.to_vec()));
        }
    }

    /// One end: it sends through its own shaper and receives what the other
    /// end's shaper delivers.
    struct ShapedConn {
        out: Shaper,
        inbox: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
        local: SocketAddr,
        remote: SocketAddr,
    }

    /// One direction's delivery: each packet reaches the far inbox when due.
    fn direction(bps: u64, drop_every: u64) -> (Shaper, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (due_tx, mut due_rx) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some((due, p)) = due_rx.recv().await {
                tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
                if inbox_tx.send(p).is_err() {
                    break;
                }
            }
        });
        let shaper = Shaper {
            bps,
            next_free: StdMutex::new(Instant::now()),
            due: due_tx,
            drop_every,
            full_packets: AtomicU64::new(0),
        };
        (shaper, inbox_rx)
    }

    fn link(bps: u64) -> (ShapedConn, ShapedConn) {
        lossy_link(bps, 0)
    }

    /// A link whose agent-to-viewer direction drops every `drop_every`-th
    /// packet that carries a full DATA chunk; 0 drops none.
    fn lossy_link(bps: u64, drop_every: u64) -> (ShapedConn, ShapedConn) {
        let (a_out, b_inbox) = direction(bps, drop_every);
        let (b_out, a_inbox) = direction(bps, 0);
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
        let config = |net_conn, rwnd, name: &str| Config {
            net_conn,
            max_receive_buffer_size: rwnd,
            max_message_size: 0,
            name: name.to_owned(),
        };
        let agent = tokio::spawn(Association::client(config(a, 0, "agent")));
        let viewer = Association::server(config(b, rwnd, "viewer")).await;
        let viewer = Arc::new(viewer.unwrap());
        let agent = agent.await.unwrap().unwrap();
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
        let start = Instant::now();
        assert_eq!(sampler.observe(start, sid(), clock), None);
        for i in 1..=n as u32 {
            tokio::time::sleep_until(tokio::time::Instant::from_std(start + SAMPLE * i)).await;
            if let Some(s) = sampler.observe(Instant::now(), sid(), clock) {
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
        let start = Instant::now();
        assert_eq!(sampler.observe(start, sid(), clock), None);
        let mut ends = Vec::new();
        for i in 1..=3u32 {
            tokio::time::sleep_until(tokio::time::Instant::from_std(start + SAMPLE * i)).await;
            let sample = sampler.observe(Instant::now(), sid(), clock);
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
        let t0 = Instant::now();
        let mut s = DeliverySampler::new(true);
        assert_eq!(s.observe(t0, sid(), || Some(first.delivery_clock())), None);
        let x = s.observe(at(t0, 2_000), sid(), || Some(first.delivery_clock()));
        assert_eq!(x.map(|x| x.ms), Some(2_000));
        // The new association's first reading starts over...
        assert_eq!(
            s.observe(at(t0, 4_000), sid(), || Some(second.delivery_clock())),
            None
        );
        // ...a held slot (no fresh clock) keeps reading the one it has...
        assert_eq!(s.observe(at(t0, 5_000), sid(), || None), None);
        // ...and the next sample is the new association's alone.
        let y = s.observe(at(t0, 6_000), sid(), || None);
        assert_eq!(y.map(|y| y.ms), Some(2_000));
        for a in [&first, &second] {
            let _ = a.close().await;
        }
        for v in [&first_viewer, &second_viewer] {
            let _ = v.close().await;
        }
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }
}

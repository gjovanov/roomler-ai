//! Measured-rate stage 0 — estimate what the session is ACTUALLY
//! delivering, and report it. Nothing here changes behaviour.
//!
//! ## Why
//!
//! Every constrained-path decision today keys off a NOMINAL relay clamp
//! (3 Mbps) and a boolean `constrained`, while the variable that matters
//! is this session's delivered rate — field-measured at ~2 Mbps on relay
//! sessions, and varying by PoP, VPN and hour. The AIMD only observes
//! send-channel occupancy, and SCTP's buffer absorbs the mismatch, so it
//! parks at the ceiling and never learns the pipe: a field capture shows
//! `target_bps=3000000` constant across a session delivering 1.75 Mbps.
//!
//! Stage 0 adds the measurement and logs it. Stages 1+ derive the
//! ceilings, queue budget, HRD window and settle time from it. Landing
//! the estimator alone first is deliberate: the estimate can be checked
//! against known truth in the field before anything depends on it.
//!
//! ## The one hard problem: a fast sample is not evidence
//!
//! Handing a frame to SCTP is not the same as delivering it. When the
//! socket buffer has headroom a frame serialises in microseconds, which
//! computes to an absurd throughput that says nothing about the pipe —
//! it is a LOWER BOUND ("at least this fast"), not a measurement.
//!
//! The fix is structural rather than a magic number: only measure across
//! a **busy period** — an unbroken stretch where the send task always had
//! another frame waiting — and only when that stretch lasted at least
//! [`MIN_BUSY_PERIOD`]. A period that long can only end when the pipe
//! drains, so its bytes-over-time IS the drain rate. Periods shorter than
//! that are discarded rather than down-weighted, so no amount of idle
//! traffic can bias the estimate upward.
//!
//! Consequences worth knowing:
//!
//! - On an unbound LAN link, periods never last that long, the estimate
//!   stays `None`, and every derived quantity falls back to the nominal
//!   band. Direct sessions are byte-identical to today, by construction.
//! - ⚠️ **This next claim was WRONG, and it was the design's load-bearing
//!   assumption.** It read: "on relay sessions the at-rest polish traffic
//!   (~1.75 Mbps sustained) keeps periods alive, so the estimate survives
//!   an idle viewer." Field-measured at rc.453 on a live relay session
//!   (browser → PC50045, corp VPN, hevc_qsv): `goodput_bps=None`,
//!   `goodput_samples=(0, 11597)` — **zero accepted**, and still zero
//!   under forced motion. Frames serialise in ~14 µs because the
//!   bottleneck is capture+encode *upstream* of the socket sampled here,
//!   so the queue never fills. It was an assumption written as a finding,
//!   and nothing here could have caught it — only a session could.
//!   Locked now by `the_measured_field_shape_yields_no_estimate`.
//!
//! ## Where this leaves stage 0
//!
//! Reporting-only, and currently reporting `None` on exactly the sessions
//! it was built for. **Stages 1+ must not be built on it as it stands** —
//! `G=None` permanently means the nominal band 100 % of the time, i.e. a
//! large change that is green, implemented, and inert, with the constants
//! it was meant to replace still quietly in charge.
//!
//! The way out, analysed but NOT built: `B = min(nominal, 0.85 × G)` only
//! ever LOWERS the clamp, so the estimator never needs to observe unused
//! capacity — it needs evidence of *insufficiency*, which is precisely a
//! congestion episode. So sample during **backpressure**, at its natural
//! timescale, and the 300 ms floor can go with it (a queue-full episode is
//! by construction not buffer headroom, which is what the floor stood in
//! for). The pump already emits the signal: `frames_skipped_backpressure`,
//! 94 of them in that same session. The arithmetic was never the problem —
//! see `a_backpressure_episode_on_a_throttled_link_reads_the_throttle`.
//!
//! ## Asymmetric adaptation
//!
//! Down fast, up slow. A VPN throttling mid-session is something to
//! believe immediately; one lucky burst is not evidence the pipe grew.
//! The estimate also decays to `None` after [`CONFIDENCE_TTL`] without a
//! qualifying sample, so a stale number can never outlive the conditions
//! that produced it — silence reverts to the nominal band rather than
//! pinning whatever was last true.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shortest stretch that counts as a measurement.
///
/// Below this the sample is dominated by socket-buffer headroom rather
/// than the pipe (see the module docs). 300 ms is ~9 frames at 30 fps —
/// long enough that the buffer cannot have absorbed the whole stretch,
/// short enough to still sample a brief congestion episode.
pub const MIN_BUSY_PERIOD: Duration = Duration::from_millis(300);

/// How long an estimate outlives its last qualifying sample.
pub const CONFIDENCE_TTL: Duration = Duration::from_secs(60);

/// EWMA weight for a sample BELOW the current estimate — the pipe
/// shrank, believe it quickly.
const ALPHA_DOWN: f64 = 0.50;

/// EWMA weight for a sample ABOVE it — the pipe may have grown, or we
/// may have caught one good burst. Move slowly.
const ALPHA_UP: f64 = 0.10;

/// Most periods the sink will hold before dropping the oldest. Bounds
/// the memory if the folding side ever stops draining; the estimator
/// only needs recent history, so the old ones are the right ones to lose.
const MAX_PENDING: usize = 64;

/// One completed busy period: how much left the send task, and over how
/// long, with the queue never empty in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusyPeriod {
    pub bytes: u64,
    pub elapsed: Duration,
}

impl BusyPeriod {
    /// Delivered bits per second, or `None` if the period is too short
    /// to mean anything (see [`MIN_BUSY_PERIOD`]) or carried no bytes.
    pub fn bps(&self) -> Option<f64> {
        if self.elapsed < MIN_BUSY_PERIOD || self.bytes == 0 {
            return None;
        }
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            return None;
        }
        Some((self.bytes as f64 * 8.0) / secs)
    }
}

/// The hand-off between the send task (which observes busy periods) and
/// the pump (which folds them at its existing 1 s cadence).
///
/// A plain `std::sync::Mutex` on purpose: the send task holds it for a
/// push with no `.await` inside, once per busy period — far rarer than
/// per frame — so an async mutex would buy nothing and cost a yield on
/// the video path.
#[derive(Clone, Default)]
pub struct GoodputSink {
    pending: Arc<Mutex<Vec<BusyPeriod>>>,
}

impl GoodputSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called by the send task when a busy period ends. Cheap and
    /// infallible — a poisoned lock is ignored rather than propagated,
    /// because losing a telemetry sample must never take down the video
    /// path.
    pub fn record(&self, bytes: u64, elapsed: Duration) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        if pending.len() >= MAX_PENDING {
            pending.remove(0);
        }
        pending.push(BusyPeriod { bytes, elapsed });
    }

    /// Called by the pump to take everything observed since last time.
    pub fn drain(&self) -> Vec<BusyPeriod> {
        let Ok(mut pending) = self.pending.lock() else {
            return Vec::new();
        };
        std::mem::take(&mut *pending)
    }
}

/// Rolling estimate of the session's delivered rate.
#[derive(Debug, Default)]
pub struct GoodputEstimator {
    ewma_bps: Option<f64>,
    last_sample_at: Option<Instant>,
    /// Periods that qualified. Reported in the heartbeat so a field
    /// reader can tell "no confidence because the link is idle" from
    /// "no confidence because nothing is wired up".
    accepted: u64,
    /// Periods that were too short or empty. The ratio is the evidence
    /// that the min-period rule is doing its job.
    rejected: u64,
}

impl GoodputEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one busy period. Returns whether it qualified.
    pub fn observe(&mut self, period: BusyPeriod, now: Instant) -> bool {
        let Some(sample) = period.bps() else {
            self.rejected += 1;
            return false;
        };
        self.ewma_bps = Some(match self.ewma_bps {
            // First qualifying sample: adopt it outright. Seeding from a
            // nominal guess would bias every later reading toward a
            // number we are specifically trying to stop trusting.
            None => sample,
            Some(prev) => {
                let alpha = if sample < prev { ALPHA_DOWN } else { ALPHA_UP };
                prev + alpha * (sample - prev)
            }
        });
        self.last_sample_at = Some(now);
        self.accepted += 1;
        true
    }

    /// The current estimate, or `None` when there is no confidence —
    /// either nothing has qualified yet, or the last one aged out.
    /// Callers treat `None` as "fall back to the nominal band".
    pub fn estimate_bps(&self, now: Instant) -> Option<u32> {
        let last = self.last_sample_at?;
        if now.duration_since(last) > CONFIDENCE_TTL {
            return None;
        }
        let bps = self.ewma_bps?;
        // Saturating rather than wrapping: an absurd sample should read
        // as "very fast", never as "almost nothing".
        Some(bps.clamp(0.0, u32::MAX as f64) as u32)
    }

    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    pub fn rejected(&self) -> u64 {
        self.rejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn period(bytes: u64, ms: u64) -> BusyPeriod {
        BusyPeriod {
            bytes,
            elapsed: Duration::from_millis(ms),
        }
    }

    fn period_us(bytes: u64, us: u64) -> BusyPeriod {
        BusyPeriod {
            bytes,
            elapsed: Duration::from_micros(us),
        }
    }

    /// **The rc.453 field read, made executable.**
    ///
    /// A live relay session (browser → PC50045, corp VPN, hevc_qsv) reported
    /// `goodput_bps=None, goodput_samples=(0, 11597)` — not one period in
    /// eleven thousand qualified, and forcing motion did not change it.
    ///
    /// The cause is the SHAPE, not the wiring. Frames serialise into the
    /// socket in ~14 µs because the bottleneck is capture+encode *upstream*
    /// of the socket this estimator samples, so the queue never fills and
    /// every period measures a memcpy rather than the link. The module docs'
    /// claim that "at-rest polish keeps periods alive on relay sessions" was
    /// an assumption written as a finding.
    ///
    /// ⚠️ Kept so that cannot be re-derived the expensive way. If someone
    /// lowers [`MIN_BUSY_PERIOD`] to "fix" the `None`, this fails — which is
    /// correct: 1.5 KB in 14 µs computes to ~857 Mbps, i.e. the speed of
    /// memory, and believing it once poisons the EWMA for a minute.
    #[test]
    fn the_measured_field_shape_yields_no_estimate() {
        let mut est = GoodputEstimator::new();
        let t0 = Instant::now();
        for i in 0..11_597u64 {
            // ~1.5 KB per frame, serialised in 14 µs, at a 30 fps cadence.
            est.observe(period_us(1_500, 14), t0 + Duration::from_micros(i * 33_000));
        }
        assert_eq!(est.accepted(), 0, "the field read was (0, 11597)");
        assert_eq!(est.rejected(), 11_597);
        assert_eq!(
            est.estimate_bps(t0),
            None,
            "eleven thousand memcpys are not evidence about a link"
        );
    }

    /// The other half of the control, and the whole case for the redesign:
    /// when the LINK is the bottleneck, the period worth measuring is exactly
    /// the congestion episode. A 1 Mbps shaped link that backs up for two
    /// seconds delivers 250 KB in that window — long enough to qualify, and
    /// it reads the throttle rather than the memcpy.
    ///
    /// This is the unit-level form of the `tc` experiment: throttled must
    /// read ≈ the throttle, and [`the_measured_field_shape_yields_no_estimate`]
    /// is the same control's negative arm. Together they say the estimator's
    /// arithmetic was never the problem — where it samples is.
    #[test]
    fn a_backpressure_episode_on_a_throttled_link_reads_the_throttle() {
        let mut est = GoodputEstimator::new();
        let t0 = Instant::now();
        // 1 Mbps for 2 s = 250 000 bytes.
        assert!(est.observe(period(250_000, 2_000), t0));
        assert_eq!(est.estimate_bps(t0), Some(1_000_000));
    }

    /// The whole design rests on this: a frame that serialised instantly
    /// is buffer headroom, not capacity, and must not reach the estimate
    /// at all. Not down-weighted — discarded.
    #[test]
    fn a_period_shorter_than_the_minimum_is_not_evidence() {
        let mut est = GoodputEstimator::new();
        let t0 = Instant::now();
        // 250 KB in 1 ms computes to 2 Gbps. Believing it once would
        // poison the EWMA for minutes.
        assert!(!est.observe(period(250_000, 1), t0));
        assert_eq!(
            est.estimate_bps(t0),
            None,
            "no confidence from a short period"
        );
        assert_eq!(est.rejected(), 1);
        assert_eq!(est.accepted(), 0);
    }

    /// An idle link produces only short periods, so the estimate never
    /// forms — which is what keeps direct sessions byte-identical.
    #[test]
    fn an_idle_link_never_gains_confidence() {
        let mut est = GoodputEstimator::new();
        let t0 = Instant::now();
        for i in 0..200 {
            est.observe(period(40_000, 5), t0 + Duration::from_millis(i * 33));
        }
        assert_eq!(est.estimate_bps(t0), None);
        assert_eq!(est.accepted(), 0);
    }

    #[test]
    fn a_qualifying_period_is_adopted_outright() {
        let mut est = GoodputEstimator::new();
        let t0 = Instant::now();
        // 250 KB over 1 s = 2 Mbps — the field-measured relay figure.
        assert!(est.observe(period(250_000, 1000), t0));
        assert_eq!(est.estimate_bps(t0), Some(2_000_000));
    }

    /// Down fast: a VPN throttling mid-session is worth believing at
    /// once. One sample must move the estimate most of the way.
    #[test]
    fn the_estimate_falls_fast() {
        let mut est = GoodputEstimator::new();
        let t0 = Instant::now();
        est.observe(period(250_000, 1000), t0); // 2 Mbps
        est.observe(period(125_000, 1000), t0); // 1 Mbps
        let got = est.estimate_bps(t0).unwrap();
        assert_eq!(got, 1_500_000, "ALPHA_DOWN=0.5 halves the gap in one step");
    }

    /// Up slow: one lucky burst is not proof the pipe grew.
    #[test]
    fn the_estimate_rises_slowly() {
        let mut est = GoodputEstimator::new();
        let t0 = Instant::now();
        est.observe(period(125_000, 1000), t0); // 1 Mbps
        est.observe(period(250_000, 1000), t0); // 2 Mbps
        let got = est.estimate_bps(t0).unwrap();
        assert_eq!(got, 1_100_000, "ALPHA_UP=0.1 moves a tenth of the gap");
        assert!(
            got < 1_500_000,
            "rising must be slower than falling, or a burst sets the ceiling"
        );
    }

    /// A stale estimate is worse than none: conditions change, and a
    /// number that outlives its evidence would pin the session to a pipe
    /// that no longer exists.
    #[test]
    fn confidence_expires_without_fresh_samples() {
        let mut est = GoodputEstimator::new();
        let t0 = Instant::now();
        est.observe(period(250_000, 1000), t0);
        assert!(
            est.estimate_bps(t0 + CONFIDENCE_TTL).is_some(),
            "still fresh at the boundary"
        );
        assert_eq!(
            est.estimate_bps(t0 + CONFIDENCE_TTL + Duration::from_secs(1)),
            None,
            "past the TTL it must read as no-confidence, not as the old value"
        );
    }

    /// ...and comes back when evidence does.
    #[test]
    fn a_fresh_sample_restores_confidence() {
        let mut est = GoodputEstimator::new();
        let t0 = Instant::now();
        est.observe(period(250_000, 1000), t0);
        let stale = t0 + CONFIDENCE_TTL + Duration::from_secs(1);
        assert_eq!(est.estimate_bps(stale), None);
        est.observe(period(250_000, 1000), stale);
        assert_eq!(est.estimate_bps(stale), Some(2_000_000));
    }

    #[test]
    fn an_empty_period_is_rejected_not_counted_as_zero() {
        let mut est = GoodputEstimator::new();
        let t0 = Instant::now();
        est.observe(period(250_000, 1000), t0);
        assert!(!est.observe(period(0, 5000), t0));
        assert_eq!(
            est.estimate_bps(t0),
            Some(2_000_000),
            "a byteless period must not drag the estimate toward zero"
        );
    }

    #[test]
    fn sink_round_trips_and_drains_once() {
        let sink = GoodputSink::new();
        sink.record(1_000, Duration::from_millis(400));
        sink.record(2_000, Duration::from_millis(500));
        let drained = sink.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0], period(1_000, 400));
        assert!(sink.drain().is_empty(), "drain must consume");
    }

    /// If the folding side ever stops draining, the sink must not grow
    /// without bound — and it should keep the RECENT periods, since the
    /// estimator only cares about those.
    #[test]
    fn sink_is_bounded_and_drops_the_oldest() {
        let sink = GoodputSink::new();
        for i in 0..(MAX_PENDING as u64 + 10) {
            sink.record(i, Duration::from_millis(400));
        }
        let drained = sink.drain();
        assert_eq!(drained.len(), MAX_PENDING);
        assert_eq!(drained[0].bytes, 10, "oldest dropped");
        assert_eq!(drained[MAX_PENDING - 1].bytes, MAX_PENDING as u64 + 9);
    }

    /// The sink is shared across tasks; a clone must be the same sink,
    /// not a fresh one (a copy would silently swallow every sample).
    #[test]
    fn a_cloned_sink_shares_storage() {
        let sink = GoodputSink::new();
        let writer = sink.clone();
        writer.record(1_000, Duration::from_millis(400));
        assert_eq!(sink.drain().len(), 1);
    }
}

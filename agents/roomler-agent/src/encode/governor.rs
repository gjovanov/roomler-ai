//! P8c — the rate governor: ONE owner for the four rate controllers
//! the DC pumps previously threaded by hand.
//!
//! The encode inventory (P8 plan, 2026-08-20) found four control loops
//! that discover each other by accident inside the pump bodies:
//!
//! 1. **AIMD bitrate** (`aimd::AimdController`) — driven off
//!    send-channel occupancy at three different loop positions
//!    (capacity gate, pre-encode, try_send overflow).
//! 2. **Viewer-rate fps cap** (`viewer_rate::ViewerRateController`) —
//!    1 s windows folding the browser's decode report into a
//!    frame-skip divisor.
//! 3. **Encode pressure** (`encode_pressure::EncodePressure`) — the
//!    `encode_factor` maxrate scale, stepped once per 2 s heartbeat.
//! 4. **Auto-downscale tier** (`encode_pressure::DownscaleTier`) —
//!    the soft resolution cap when the bitrate lever is exhausted.
//!
//! This module changes NO behavior. The four controllers are owned
//! unchanged; every method here is a 1:1 relocation of a pump touch
//! point, so the pump calls the governor at exactly the positions it
//! previously mutated controller state — same order, same inputs,
//! same rate limits. What it buys structurally:
//!
//! - every cross-loop input is a **named parameter** (`send_depth` at
//!   construction, `send_capacity`, `ceiling_bps`, the decode report,
//!   `avg_encode_ms`) instead of ambient pump locals;
//! - `last_applied_bitrate` has ONE owner (the `applied_bps` mirror)
//!   instead of a per-pump local that each touch point must remember
//!   to update;
//! - the declared invariants at the bottom are tests, not tribal
//!   knowledge (divisor can't starve the refine signal; the skip gate
//!   never sheds a forced keyframe; the 1 s window gate holds).
//!
//! Behavioral unification (shared clock, merged MD signals) is a
//! LATER, separate decision once this soaks — deliberately not here.

use std::time::{Duration, Instant};

use super::aimd::AimdController;
use super::encode_pressure::{DownscaleTier, EncodePressure};
use super::goodput::{GoodputEstimator, GoodputSink};
use super::viewer_rate::{self, ViewerRateController};

/// A bitrate target the pump must apply to the live encoder
/// (`enc.set_bitrate(bps)`). Emitted at most once per underlying
/// AIMD move (`take_pending` is change-gated).
///
/// ⚠ Surfaced side effect, not a surprise: on QSV/AMF encoders
/// `set_bitrate` lands as a debounced encoder REBUILD whose first
/// frame is a key-flagged IDR — callers treat `Some(_)` as a
/// possible viewer resync point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedBitrate {
    pub bps: u32,
    /// Whether this target differs from the previously-applied one —
    /// the change-gated log condition the pumps use.
    pub changed: bool,
}

/// Outcome of a viewer-rate window fold (once per second).
#[derive(Debug, Clone, Copy)]
pub struct ViewerWindow {
    pub reported_fps: u32,
    pub struggling: bool,
    pub cap_fps: u32,
    pub skip_divisor: u32,
    /// Divisor moved this window — the pump's log condition is
    /// `changed || struggling`.
    pub changed: bool,
}

/// Outcome of a heartbeat tier step, emitted only when the
/// auto-downscale tier actually moved (rare: ≥5 saturated windows
/// down / ≥30 deep-headroom windows up, 60 s cooldown).
#[derive(Debug, Clone, Copy)]
pub struct TierChange {
    pub cap_long_edge: Option<u32>,
    pub ewma_encode_ms: f32,
}

/// One `RateGovernor` per pump instance. See the module docs for
/// what it owns and why.
pub struct RateGovernor {
    // Loop 1 — AIMD. Lazily constructed at the first pre-encode tick
    // (needs the first frame's ceiling), exactly as the pumps did.
    aimd: Option<AimdController>,
    send_depth: usize,
    /// Mirror of the last applied bitrate, for the heartbeat + the
    /// change-gated log. 0 = nothing applied yet.
    applied_bps: u32,

    // Loop 2 — viewer-rate fps cap.
    viewer_rate: ViewerRateController,
    viewer_window_at: Instant,
    skip_divisor: u32,
    skip_counter: u32,
    frames_skipped_decode: u64,

    // Loops 3+4 — encode pressure + auto-downscale tier.
    pressure: EncodePressure,
    encode_factor: f32,
    tier: DownscaleTier,
    auto_res_cap: Option<u32>,

    // Measured-rate stage 0 — OBSERVE ONLY. Nothing above reads this
    // yet; it is folded on the viewer-window tick and reported in the
    // heartbeat so the estimate can be checked against known truth
    // (relay sessions ≈ 2 Mbps, direct ⇒ None) before stage 1 derives
    // the ceiling from it.
    goodput: GoodputEstimator,
    goodput_sink: GoodputSink,
}

/// The viewer-rate fold cadence (unchanged from the pump bodies).
const VIEWER_WINDOW: Duration = Duration::from_secs(1);

impl RateGovernor {
    pub fn new(target_fps: u32, send_depth: usize, now: Instant) -> Self {
        Self {
            aimd: None,
            send_depth,
            applied_bps: 0,
            viewer_rate: ViewerRateController::new(target_fps),
            viewer_window_at: now,
            skip_divisor: 1,
            skip_counter: 0,
            frames_skipped_decode: 0,
            pressure: EncodePressure::new(),
            encode_factor: 1.0,
            tier: DownscaleTier::new(),
            auto_res_cap: None,
            goodput: GoodputEstimator::new(),
            goodput_sink: GoodputSink::new(),
        }
    }

    /// Hand to the send task at spawn. It records one busy period per
    /// unbroken run of frames; the governor folds them on the viewer
    /// window tick.
    pub fn goodput_sink(&self) -> GoodputSink {
        self.goodput_sink.clone()
    }

    /// The measured delivered rate, or `None` for no confidence.
    ///
    /// ⚠ Stage 0: **reported, never consumed.** When stage 1 wires this
    /// into the budgets, the rule is `B = min(nominal, 0.85 × G)` —
    /// measurement may only ever LOWER the clamp, because the clamp also
    /// protects the TURN path.
    pub fn measured_goodput_bps(&self, now: Instant) -> Option<u32> {
        self.goodput.estimate_bps(now)
    }

    /// `(accepted, rejected)` busy periods — heartbeat telemetry. A high
    /// rejected count with zero accepted is the healthy signature of an
    /// unbound link, not a wiring fault.
    pub fn goodput_samples(&self) -> (u64, u64) {
        (self.goodput.accepted(), self.goodput.rejected())
    }

    /// Backpressure-gate arm: the send channel is FULL (or a shared-
    /// pipeline follower is congested) and the pump is about to skip
    /// production. Feed the full-occupancy sample so the multiplicative
    /// decrease runs DURING sustained congestion (the rc.171/Phase B
    /// starvation fix) — no-op until the AIMD exists (first frame not
    /// yet encoded).
    pub fn on_backpressure_skip(&mut self, now: Instant) -> Option<AppliedBitrate> {
        let ctrl = self.aimd.as_mut()?;
        ctrl.observe(self.send_depth as u32, true, now);
        let bps = ctrl.take_pending()?;
        Some(self.record_applied(bps))
    }

    /// Pre-encode tick, every frame that reaches the encoder: lazily
    /// construct the AIMD at the session's first ceiling, push the
    /// current ceiling (quality preference × relay clamp — lowering
    /// clamps desired down immediately), and feed a non-full occupancy
    /// sample so the additive increase can recover once the link
    /// drains.
    pub fn pre_encode_tick(
        &mut self,
        ceiling_bps: u32,
        send_capacity: usize,
        now: Instant,
    ) -> Option<AppliedBitrate> {
        let depth = self.send_depth;
        let ctrl = self.aimd.get_or_insert_with(|| {
            AimdController::new(
                ceiling_bps,
                super::MIN_BITRATE_BPS,
                ceiling_bps,
                depth as u32,
                now,
            )
        });
        ctrl.set_ceiling(ceiling_bps);
        ctrl.observe(
            depth.saturating_sub(send_capacity) as u32,
            send_capacity == 0,
            now,
        );
        let bps = ctrl.take_pending()?;
        Some(self.record_applied(bps))
    }

    /// try_send-Full arm: an encoded frame overflowed the send channel
    /// (a big IDR / motion frame the link can't drain) — a secondary
    /// congestion signal (rate-limited MD internally).
    pub fn on_send_overflow(&mut self, now: Instant) -> Option<AppliedBitrate> {
        let ctrl = self.aimd.as_mut()?;
        ctrl.note_buffer_overflow(now);
        let bps = ctrl.take_pending()?;
        Some(self.record_applied(bps))
    }

    /// A fresh encoder starts at its constructor's full-ceiling
    /// maxrate; force the AIMD to re-apply its current (possibly
    /// lower) target so the stream doesn't snap back up after a dim
    /// change / resolution switch.
    pub fn on_encoder_rebuilt(&mut self) {
        if let Some(ctrl) = self.aimd.as_mut() {
            ctrl.force_reapply();
        }
    }

    /// The VP9 pump's HISTORICAL rebuild behavior, preserved verbatim
    /// (P8c is structural-only): zero the applied-bitrate MIRROR so the
    /// next apply logs, WITHOUT `force_reapply` — the AIMD's internal
    /// last-applied is untouched, so the fresh encoder runs at its
    /// boot-time bitrate until the controller's next MD/AI/ceiling
    /// move actually shifts `desired`. ⚠ Named divergence from
    /// [`Self::on_encoder_rebuilt`] (the ffmpeg pump's semantics);
    /// unifying them is a behavioral decision for the post-soak pass.
    pub fn on_encoder_rebuilt_mirror_only(&mut self) {
        self.applied_bps = 0;
    }

    /// Once a second, fold the browser's decode report into the
    /// send-fps cap and the frame-skip divisor. `take_report` is
    /// called ONLY when the window is due (the pump's atomic swap
    /// consumes the report); `fold_followers` folds the shared
    /// pipeline's follower windows given this pump's own divisor
    /// (the stream paces to the slowest viewer).
    pub fn tick_viewer_window(
        &mut self,
        now: Instant,
        target_fps: u32,
        take_report: impl FnOnce() -> u32,
        fold_followers: impl FnOnce(u32) -> u32,
    ) -> Option<ViewerWindow> {
        if now.duration_since(self.viewer_window_at) < VIEWER_WINDOW {
            return None;
        }
        self.viewer_window_at = now;
        // Stage 0: fold whatever the send task observed since the last
        // window. Deliberately inside the window gate rather than per
        // frame — the send task is a different task, and this is the
        // existing once-a-second rendezvous.
        for period in self.goodput_sink.drain() {
            self.goodput.observe(period, now);
        }
        let (reported_fps, struggling) = viewer_rate::unpack_report(take_report());
        let own_div = self
            .viewer_rate
            .observe(reported_fps, struggling, target_fps);
        let new_div = own_div.max(fold_followers(own_div));
        let changed = new_div != self.skip_divisor;
        self.skip_divisor = new_div;
        Some(ViewerWindow {
            reported_fps,
            struggling,
            cap_fps: self.viewer_rate.cap_fps(),
            skip_divisor: new_div,
            changed,
        })
    }

    /// The decode-pressure frame-skip gate: keep 1 of every
    /// `skip_divisor` delta frames. A frame carrying a forced keyframe
    /// is NEVER skipped (the browser needs the IDR to resync) and does
    /// not advance the counter — exactly the pump's original gate.
    pub fn should_skip_delta_frame(&mut self, force_keyframe: bool) -> bool {
        if self.skip_divisor > 1 && !force_keyframe {
            if self.skip_counter + 1 < self.skip_divisor {
                self.skip_counter += 1;
                self.frames_skipped_decode += 1;
                return true;
            }
            self.skip_counter = 0;
        }
        false
    }

    /// Per-heartbeat (2 s) step: fold the window's average encode time
    /// into the encode-pressure factor (applies to the ceiling from
    /// the next frame on), then the factor's saturation signal into
    /// the auto-downscale tier. Returns the tier change, if any, for
    /// the pump's log line.
    pub fn heartbeat(&mut self, avg_encode_ms: f32, now: Instant) -> Option<TierChange> {
        self.encode_factor = self.pressure.observe(avg_encode_ms);
        let new_cap = self.tier.observe(self.pressure.tier_signal(), now)?;
        self.auto_res_cap = new_cap;
        Some(TierChange {
            cap_long_edge: new_cap,
            ewma_encode_ms: self.pressure.ewma_ms(),
        })
    }

    /// The encode-pressure maxrate scale (1.0 = full quality) —
    /// consumed by `policy::rate_plan` every frame.
    pub fn encode_factor(&self) -> f32 {
        self.encode_factor
    }

    /// The auto-downscale soft cap — consumed by `policy::plan_dims`'
    /// soft slot every frame.
    pub fn auto_res_cap(&self) -> Option<u32> {
        self.auto_res_cap
    }

    /// The last bitrate handed to `enc.set_bitrate` (0 = none yet) —
    /// heartbeat truth.
    pub fn applied_bps(&self) -> u32 {
        self.applied_bps
    }

    pub fn frames_skipped_decode(&self) -> u64 {
        self.frames_skipped_decode
    }

    fn record_applied(&mut self, bps: u32) -> AppliedBitrate {
        let changed = bps != self.applied_bps;
        self.applied_bps = bps;
        AppliedBitrate { bps, changed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEPTH: usize = 12;
    const CEILING: u32 = 12_000_000;

    fn gov(now: Instant) -> RateGovernor {
        RateGovernor::new(30, DEPTH, now)
    }

    #[test]
    fn first_pre_encode_tick_applies_the_initial_ceiling_once() {
        let now = Instant::now();
        let mut g = gov(now);
        // First tick constructs the AIMD at the ceiling; the seeded
        // last_applied=0 makes take_pending emit the initial target
        // exactly once.
        let applied = g
            .pre_encode_tick(CEILING, DEPTH, now)
            .expect("initial apply");
        assert_eq!(applied.bps, CEILING);
        assert!(applied.changed);
        assert_eq!(g.applied_bps(), CEILING);
        // Same conditions again — change-gated, nothing to apply.
        assert!(g.pre_encode_tick(CEILING, DEPTH, now).is_none());
    }

    #[test]
    fn sustained_full_channel_walks_bitrate_down_never_below_floor() {
        let start = Instant::now();
        let mut g = gov(start);
        g.pre_encode_tick(CEILING, DEPTH, start);
        let mut last = g.applied_bps();
        // Full-channel gate arms spaced past the MD rate limit walk the
        // target down monotonically; the floor holds no matter how long
        // congestion lasts.
        for i in 1..=100u64 {
            let now = start + Duration::from_millis(600 * i);
            if let Some(applied) = g.on_backpressure_skip(now) {
                assert!(applied.bps <= last, "MD must never raise the target");
                last = applied.bps;
            }
            assert!(g.applied_bps() >= crate::encode::MIN_BITRATE_BPS);
        }
        assert!(
            last < CEILING,
            "sustained congestion must have decreased the target"
        );
    }

    #[test]
    fn lowering_the_ceiling_clamps_the_target_immediately() {
        let now = Instant::now();
        let mut g = gov(now);
        g.pre_encode_tick(CEILING, DEPTH, now);
        // A transport flip to relay shrinks the ceiling; the very next
        // tick must emit the clamped target (refine can raise dims —
        // and thereby the requested ceiling — but the constrained
        // clamp always wins; see also the policy-level flatness test).
        let relay_ceiling = 3_000_000;
        let applied = g
            .pre_encode_tick(relay_ceiling, DEPTH, now)
            .expect("clamp emits");
        assert_eq!(applied.bps, relay_ceiling);
    }

    #[test]
    fn rebuild_forces_a_reapply_of_the_current_target() {
        let now = Instant::now();
        let mut g = gov(now);
        g.pre_encode_tick(CEILING, DEPTH, now);
        assert!(g.pre_encode_tick(CEILING, DEPTH, now).is_none());
        // A fresh encoder starts at its constructor maxrate — the
        // governor must re-emit the (unchanged) desired target so the
        // pump re-applies it.
        g.on_encoder_rebuilt();
        let applied = g
            .pre_encode_tick(CEILING, DEPTH, now)
            .expect("post-rebuild reapply");
        assert_eq!(applied.bps, CEILING);
        assert!(
            !applied.changed,
            "reapply of the same value is not a change (no log)"
        );
    }

    #[test]
    fn viewer_window_is_one_second_gated_and_report_consumed_lazily() {
        let start = Instant::now();
        let mut g = gov(start);
        let mut consumed = 0u32;
        // Not due yet — the report closure must NOT run (the pump's
        // atomic swap would otherwise eat a report early).
        let r = g.tick_viewer_window(
            start + Duration::from_millis(300),
            30,
            || {
                consumed += 1;
                0
            },
            |own| own,
        );
        assert!(r.is_none());
        assert_eq!(consumed, 0, "report consumed before the window was due");
        let r = g.tick_viewer_window(
            start + Duration::from_millis(1100),
            30,
            || {
                consumed += 1;
                0
            },
            |own| own,
        );
        assert!(r.is_some());
        assert_eq!(consumed, 1);
    }

    #[test]
    fn skip_gate_keeps_one_in_n_and_never_sheds_forced_keyframes() {
        let start = Instant::now();
        let mut g = gov(start);
        // Fold a follower divisor of 3 in (own report healthy → own
        // divisor 1; the shared stream paces to the slowest viewer).
        let w = g
            .tick_viewer_window(start + Duration::from_secs(2), 30, || 0, |_| 3)
            .expect("window due");
        assert_eq!(w.skip_divisor, 3);
        assert!(w.changed);
        // Divisor 3 ⇒ skip, skip, keep — repeating.
        let pattern: Vec<bool> = (0..6).map(|_| g.should_skip_delta_frame(false)).collect();
        assert_eq!(pattern, [true, true, false, true, true, false]);
        assert_eq!(g.frames_skipped_decode(), 4);
        // A forced keyframe is NEVER shed and doesn't advance the cycle.
        assert!(!g.should_skip_delta_frame(true));
        assert!(
            g.should_skip_delta_frame(false),
            "cycle resumes where it was"
        );
    }

    /// Plan invariant (P8c #2): the divisor can never starve the
    /// refine/quiet-tick signal — the viewer-rate min_fps floor (12)
    /// bounds the divisor at ceil(target_fps / 12), so encoded frames
    /// keep flowing no matter how weak the reported decode rate is.
    #[test]
    fn divisor_never_starves_encoded_cadence() {
        let start = Instant::now();
        let mut g = gov(start);
        for i in 1..=30u64 {
            let now = start + Duration::from_secs(2 * i);
            // A viewer reporting 1 fps and struggling, forever.
            let w = g.tick_viewer_window(now, 30, || viewer_rate::pack_report(1, true), |own| own);
            let w = w.expect("2 s apart — every window due");
            assert!(
                w.skip_divisor <= 3,
                "divisor {} exceeds ceil(30/min_fps=12)=3 — encoded cadence starved",
                w.skip_divisor
            );
        }
    }

    #[test]
    fn heartbeat_saturation_lowers_the_factor() {
        let start = Instant::now();
        let mut g = gov(start);
        assert_eq!(g.encode_factor(), 1.0);
        // Sustained encoder saturation (avg encode ≫ frame budget)
        // must walk the maxrate factor down; the tier may or may not
        // step within this horizon (its own windows + cooldown) — the
        // factor is the invariant here.
        for i in 1..=20u64 {
            let _ = g.heartbeat(100.0, start + Duration::from_secs(2 * i));
        }
        assert!(
            g.encode_factor() < 1.0,
            "sustained saturation left the factor at full quality"
        );
        assert!(g.encode_factor() >= crate::encode::encode_pressure::FACTOR_FLOOR);
    }
}

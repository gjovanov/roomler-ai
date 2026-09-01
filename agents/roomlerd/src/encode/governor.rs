// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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
    /// FR-15 — the viewer's paint-age report this window (avg, floor-sample),
    /// ms. None = viewer sent no age (old web, or no frames painted).
    pub age_ms: Option<(u16, u16)>,
    /// FR-15 — the age loop judged the transport over-rate this window
    /// (constrained only). Two responses, both through machinery that
    /// already exists: it folds into the fps-cap `struggling` path (an
    /// instant, re-open-free byte cut) and it feeds the AIMD a congestion
    /// sample, so the decrease reaches the encoder through the pump's
    /// NORMAL apply arms on the next frame — including, on a thrifty
    /// constrained QSV session, the FR-10 deferral that keeps a re-open
    /// lump out of the middle of a drag.
    pub age_over: bool,
    /// FR-35 — the ceiling learner stepped the constrained ceiling up this
    /// window (for the pump's log line).
    pub ceiling_grown: Option<super::ceiling_learn::Grow>,
    /// FR-59 P3 — the viewer's transit queue has grown for two
    /// consecutive windows: the agent is putting bits in faster than they
    /// come out, whatever its own send queue says.
    pub link_congested: bool,
    /// FR-59 P3 — the ceiling that window's ARRIVAL rate justifies.
    /// `None` unless `link_congested` — see `viewer_rate::LinkLoop`.
    pub link_ceiling_bps: Option<u32>,
}

/// Outcome of a heartbeat tier step, emitted only when the
/// auto-downscale tier actually moved (rare: ≥5 saturated windows
/// down / ≥30 deep-headroom windows up, 60 s cooldown).
#[derive(Debug, Clone, Copy)]
pub struct TierChange {
    pub cap_long_edge: Option<u32>,
    pub ewma_encode_ms: f32,
}

/// Every rate kill switch the governor honours, resolved ONCE by the pump
/// from env/config so the governor itself stays pure.
///
/// A struct rather than a row of positional `bool`s: FR-59 added two more
/// to the two that were already there, and four adjacent bools at a call
/// site is a transposition waiting to happen — one that no test would
/// catch, because every one of them is a legal value in every position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernorFlags {
    /// Measured-rate stage 1 — a held goodput estimate clamps the nominal
    /// ceiling (`encode::measured_ceiling_enabled`). DIRECT paths only;
    /// see `pre_encode_tick`. False = observe-and-report only.
    pub measured_ceiling: bool,
    /// FR-15 — the constrained-transport age loop may ACT, not merely
    /// learn and report (`encode::relay_age_feedback_enabled`).
    pub age_feedback: bool,
    /// FR-59 P1 — the AIMD floor may descend toward a MEASURED pipe on a
    /// constrained transport (`encode::slow_link_floor_enabled`).
    pub floor_relief: bool,
    /// FR-59 P6 — a measurement that contradicts the FR-35 learned/seeded
    /// ceiling abandons it (`encode::seed_contradiction_enabled`).
    pub seed_contradiction: bool,
    /// FR-59 P3 — the viewer's own arrival rate may clamp the constrained
    /// ceiling while its transit queue is growing
    /// (`encode::viewer_rate_clamp_enabled`).
    pub viewer_rate_clamp: bool,
}

impl Default for GovernorFlags {
    /// All on — the shipped posture. Tests that care flip one field BY
    /// NAME; tests that do not get the production behaviour.
    fn default() -> Self {
        Self {
            measured_ceiling: true,
            age_feedback: true,
            floor_relief: true,
            seed_contradiction: true,
            viewer_rate_clamp: true,
        }
    }
}

impl GovernorFlags {
    /// The shipped posture, read from env/config. The pumps call this;
    /// tests construct the struct literally.
    #[cfg_attr(
        not(any(feature = "ffmpeg-encoder", feature = "vp9-444")),
        allow(dead_code)
    )]
    pub fn from_env() -> Self {
        Self {
            measured_ceiling: super::measured_ceiling_enabled(),
            age_feedback: super::relay_age_feedback_enabled(),
            floor_relief: super::slow_link_floor_enabled(),
            seed_contradiction: super::seed_contradiction_enabled(),
            viewer_rate_clamp: super::viewer_rate_clamp_enabled(),
        }
    }
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
    // Loop 2b (FR-15) — constrained-transport age feedback.
    age_loop: viewer_rate::AgeLoop,
    /// Last viewer age (window avg, learned floor) for the heartbeat.
    last_viewer_age: Option<(u16, u16)>,
    /// FR-15 kill switch, resolved once by the pump
    /// (`encode::relay_age_feedback_enabled` — env/config
    /// `RELAY_AGE_FEEDBACK`). False = learn + report only, never act.
    age_feedback: bool,

    // Loops 3+4 — encode pressure + auto-downscale tier.
    pressure: EncodePressure,
    encode_factor: f32,
    tier: DownscaleTier,
    auto_res_cap: Option<u32>,
    // P5 (FR-1) — fps-first cadence pacing for HW encoders; sits between
    // the factor (masked while engaged) and the tier (unchanged).
    pace: super::encode_pressure::FpsPace,

    // Measured-rate v2 (2026-08-27) — CONSUMED at stage 1: the send
    // task reports per-frame blocked sends, the viewer-window tick
    // folds one window at a time, and `pre_encode_tick` clamps the
    // nominal ceiling to `derived_ceiling_bps(G)` while an estimate
    // holds. The heartbeat still reports the raw estimate + counts.
    goodput: GoodputEstimator,
    goodput_sink: GoodputSink,
    /// Stage-1 kill switch, resolved once by the pump
    /// (`encode::measured_ceiling_enabled` — env/config
    /// `MEASURED_CEILING`). False = observe-and-report only.
    measured_ceiling: bool,
    /// FR-35 — grows the constrained ceiling above the nominal on delivery
    /// evidence; inert (returns the plan) when `hi` is 0 or off-relay.
    learner: super::ceiling_learn::CeilingLearner,
    /// AIMD decrease count at the last tick — a change means the pipe pushed
    /// back since, which the learner must hear.
    last_decreases: u32,
    /// Send-task byte total at the last viewer window, for the window's
    /// delivered rate (the learner's "carried" evidence).
    last_sent_bytes: u64,

    /// FR-59 — the slow-link kill switches.
    slow_link: GovernorFlags,
    /// FR-59 P6 — the ceiling a measurement contradicted, parked here for
    /// the pump to log. Drained by `take_seed_abandoned`; the learner only
    /// ever reports an abandonment once, so this cannot spam.
    seed_abandoned_bps: Option<u32>,
    /// FR-59 P1 — the floor actually in force after the evidence-gated
    /// relief, for the heartbeat. `None` = the nominal floor stands.
    relieved_floor_bps: Option<u32>,
    /// FR-59 P3 — the viewer-side congestion loop.
    link_loop: viewer_rate::LinkLoop,
    /// FR-59 P3 — the viewer's ARRIVAL rate and when it was reported.
    /// Held only while its queue is growing (see `viewer_rate::LinkLoop`
    /// for why that gate is not optional), and expired by
    /// [`LINK_CEILING_TTL`] so a viewer that goes SILENT cannot pin the
    /// session forever — silence is not evidence of congestion. Stored
    /// RAW rather than pre-discounted, because it feeds two consumers
    /// with different margins: the P3 ceiling clamp and the P1 floor.
    link_rx_bps: Option<(u32, Instant)>,
}

/// The viewer-rate fold cadence (unchanged from the pump bodies).
const VIEWER_WINDOW: Duration = Duration::from_secs(1);

/// FR-59 P3 — how long an arrival-rate ceiling outlives the window that
/// produced it. Generous enough to bridge a viewer that skips a report,
/// short enough that a viewer which goes away entirely cannot leave the
/// session clamped: silence is not evidence of congestion, and the loop
/// clears the clamp on the first non-congested window anyway.
const LINK_CEILING_TTL: Duration = Duration::from_secs(15);

impl RateGovernor {
    /// `flags` carries every kill switch (see [`GovernorFlags`]); the
    /// pumps resolve them once from env/config, tests pass them
    /// explicitly, so the governor stays pure.
    pub fn new(
        target_fps: u32,
        send_depth: usize,
        flags: GovernorFlags,
        ceiling_hi_bps: u32,
        ceiling_seed_bps: Option<u32>,
        now: Instant,
    ) -> Self {
        Self {
            aimd: None,
            send_depth,
            applied_bps: 0,
            viewer_rate: ViewerRateController::new(target_fps),
            viewer_window_at: now,
            skip_divisor: 1,
            skip_counter: 0,
            frames_skipped_decode: 0,
            age_loop: viewer_rate::AgeLoop::new(),
            last_viewer_age: None,
            age_feedback: flags.age_feedback,
            pressure: EncodePressure::new(),
            encode_factor: 1.0,
            tier: DownscaleTier::new(),
            auto_res_cap: None,
            goodput: GoodputEstimator::new(),
            goodput_sink: GoodputSink::new(),
            pace: super::encode_pressure::FpsPace::new(),
            measured_ceiling: flags.measured_ceiling,
            learner: super::ceiling_learn::CeilingLearner::new(ceiling_hi_bps, ceiling_seed_bps),
            last_decreases: 0,
            last_sent_bytes: 0,
            slow_link: flags,
            seed_abandoned_bps: None,
            relieved_floor_bps: None,
            link_loop: viewer_rate::LinkLoop::new(),
            link_rx_bps: None,
        }
    }

    /// FR-59 P6 — take the ceiling a measurement contradicted, if one was
    /// abandoned since the last call, so the pump can log it once.
    pub fn take_seed_abandoned(&mut self) -> Option<u32> {
        self.seed_abandoned_bps.take()
    }

    /// FR-59 P1 — the floor currently in force when the evidence-gated
    /// relief has lowered it; `None` while the nominal floor stands.
    /// Heartbeat telemetry: without it, "the floor let go" and "the pipe
    /// was never measured" look identical from a log.
    pub fn relieved_floor_bps(&self) -> Option<u32> {
        self.relieved_floor_bps
    }

    /// FR-35 — the ceiling the AIMD runs under: the plan's, lifted by what the
    /// learner has proven on a constrained session. Also the value the pump
    /// opens the encoder with, so the opening keyframe is sized by it.
    pub fn effective_ceiling(&mut self, plan_ceiling_bps: u32, constrained: bool) -> u32 {
        if constrained {
            self.learner.effective_ceiling(plan_ceiling_bps)
        } else {
            plan_ceiling_bps
        }
    }

    /// FR-35 — the learned ceiling (0 = nothing learned or seeded), for the
    /// heartbeat.
    /// AIMD decreases so far this session (any cause) — the "evidence" bit the
    /// rate memory needs before it accepts a LOWER stable rate.
    pub fn decreases(&self) -> u32 {
        self.aimd.as_ref().map_or(0, |a| a.decreases())
    }

    pub fn learned_ceiling_bps(&self) -> u32 {
        self.learner.learned_bps()
    }

    /// FR-35 P2 — the session's stable rate when it beats the nominal.
    pub fn stable_bps(&self) -> Option<u32> {
        self.learner.stable_bps()
    }

    /// Hand to the send task at spawn. It reports every frame's
    /// serialisation time; the sink keeps only genuinely blocked sends
    /// (≥ `goodput::MIN_BLOCKED_SEND`) and the governor folds one
    /// window per viewer tick.
    pub fn goodput_sink(&self) -> GoodputSink {
        self.goodput_sink.clone()
    }

    /// The measured delivered rate, or `None` for no confidence.
    ///
    /// Stage 1 consumes this in [`Self::pre_encode_tick`] as
    /// `ceiling := min(nominal, derived_ceiling_bps(G))` — measurement
    /// may only ever LOWER the clamp, because the nominal clamp also
    /// protects the TURN path.
    pub fn measured_goodput_bps(&self, now: Instant) -> Option<u32> {
        self.goodput.estimate_bps(now)
    }

    /// `(accepted, rejected)` goodput WINDOWS — heartbeat telemetry.
    /// `(0, 0)` is the healthy signature of an unbound link (nothing
    /// ever blocked); rejected counts windows whose blocked time was
    /// too thin to trust.
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
    /// current floor (area-scaled legibility minimum — see
    /// `encode::area_min_bitrate_bps`; the controller caps it at the
    /// live ceiling) and ceiling (quality preference × relay clamp —
    /// lowering clamps desired down immediately), and feed a non-full
    /// occupancy sample so the additive increase can recover once the
    /// link drains. Floor BEFORE ceiling so a transport flip to relay
    /// resolves both in one tick (the flat floor lands first, then the
    /// clamp isn't maxed against a stale big-screen floor).
    pub fn pre_encode_tick(
        &mut self,
        ceiling_bps: u32,
        floor_bps: u32,
        constrained: bool,
        send_capacity: usize,
        now: Instant,
    ) -> Option<AppliedBitrate> {
        // Stage 1 (2026-08-27) — clamp the nominal ceiling to the
        // measured pipe while an estimate holds: the session then tops
        // out just UNDER the drain rate instead of rediscovering it by
        // congesting the send queue on every burst (the "chunky" skips).
        // Only ever LOWERS the nominal; confidence decays via the
        // estimator's TTL, so silence reverts to the nominal band.
        //
        // DIRECT-ONLY (field 2026-08-27, CORPLAP-3 + CORPLAP-2 over the corp
        // relay, same day the clamp shipped): per-frame samples through
        // a lumpy TURN-TCP pipe read near-zero during TCP stalls, the
        // down-fast EWMA crashed the estimate, and the ceiling rode the
        // 1 Mbps floor — relay sessions got WORSE than the nominal
        // clamp they had before. The nominal relay clamp already IS the
        // physics bound there; the estimate stays OBSERVED on relay
        // (heartbeat `goodput_bps`) as the dataset for a future
        // lumpiness-robust design.
        let ceiling_bps = if self.measured_ceiling
            && !constrained
            && let Some(g) = self.goodput.estimate_bps(now)
        {
            ceiling_bps.min(super::goodput::derived_ceiling_bps(g))
        } else {
            ceiling_bps
        };
        // FR-59 P3 — the viewer's arrival rate, while it is still live.
        // Expired here rather than at the window fold because a viewer that
        // goes SILENT is silent, not congested, and the fold only runs when
        // a report arrives.
        let link_rx_bps = match self.link_rx_bps {
            Some((rx, at)) if now.duration_since(at) <= LINK_CEILING_TTL => Some(rx),
            Some(_) => {
                self.link_rx_bps = None;
                None
            }
            None => None,
        };
        // FR-59 — what this session believes the pipe carries, on a
        // constrained transport: the agent's own blocked-send goodput, the
        // viewer's arrival rate while its queue is growing, or the LOWER of
        // the two. P1, P3 and P6 all key off this one number.
        //
        // Deliberately consumed here even though stage 1 above refuses to
        // clamp the CEILING with the goodput half on relay: the uses have
        // opposite risk. Lowering a FLOOR, bounding a ceiling the receiver
        // says it cannot keep up with, and abandoning an over-ambitious
        // learned ceiling all SHED bits and cut latency; an under-estimated
        // nominal ceiling collapses quality, which is why stage 1 stays
        // direct-only.
        //
        // ⚠ The two halves are not redundant. The goodput estimate needs
        // the agent's own sends to BLOCK, and on the link this program
        // exists for they do not (field: `send_wait_max_ms` 0.1 ms while
        // the viewer ran 2.3 s behind) — the queue is downstream. The
        // viewer's half needs a viewer that reports. Either alone leaves a
        // real case uncovered.
        let measured_bps = if constrained {
            match (self.goodput.estimate_bps(now), link_rx_bps) {
                (Some(g), Some(rx)) => Some(g.min(rx)),
                (g, rx) => g.or(rx),
            }
        } else {
            None
        };
        // FR-59 P6 — a measurement far under the learned/seeded ceiling
        // contradicts it. Must run BEFORE `effective_ceiling`, or this
        // frame is still planned at the ceiling the evidence just refuted.
        if let Some(g) = measured_bps
            && let Some(dropped) = self
                .learner
                .on_measurement(g, self.slow_link.seed_contradiction)
        {
            self.seed_abandoned_bps = Some(dropped);
        }
        // FR-35 — lift the constrained ceiling by what the learner has proven.
        let ceiling_bps = if constrained {
            self.learner.effective_ceiling(ceiling_bps)
        } else {
            ceiling_bps
        };
        // FR-59 P3 — and then bound it by what the VIEWER says is actually
        // arriving, while its transit queue is growing. Applied AFTER the
        // learner deliberately: the learner's evidence is what the pipe
        // carried in the past, and a live report that the receiver is
        // falling behind outranks it.
        let ceiling_bps = match link_rx_bps {
            Some(rx) if constrained => ceiling_bps
                .min(((rx as u64) * viewer_rate::LINK_CEILING_PCT / 100) as u32)
                .max(1),
            _ => ceiling_bps,
        };
        // FR-59 P1 — the legibility floor descends toward a measured pipe.
        // `MIN_BITRATE_BPS` is calibrated for the 2–9 Mbps band every
        // measured relay sat in; below it the floor is not a floor but a
        // PIN, because it is also the AIMD's `floor_bps` and the
        // multiplicative decrease bottoms out there.
        //
        // ⚠ Load-bearing for P3, not only for P1: `set_ceiling` raises any
        // ceiling back up to the floor, so without the relief the arrival-
        // rate clamp above would be silently undone on exactly the links it
        // exists for. Evidence-gated, so a session with no measurement at
        // all is byte-for-byte unchanged.
        let floor_bps = match measured_bps {
            Some(g) if self.slow_link.floor_relief => {
                let relieved = super::goodput::measured_floor_bps(
                    g,
                    floor_bps,
                    super::slow_link_min_bitrate_bps(),
                );
                self.relieved_floor_bps = (relieved < floor_bps).then_some(relieved);
                relieved
            }
            _ => {
                self.relieved_floor_bps = None;
                floor_bps
            }
        };
        let depth = self.send_depth;
        let ctrl = self.aimd.get_or_insert_with(|| {
            // FR-59 P1 — construct at the floor already in force rather
            // than the flat `MIN_BITRATE_BPS`, so the constructor's own
            // `initial.clamp(floor, ceiling)` and the `set_floor` on the
            // next line cannot momentarily disagree.
            AimdController::new(ceiling_bps, floor_bps, ceiling_bps, depth as u32, now)
        });
        ctrl.set_floor(floor_bps);
        ctrl.set_ceiling(ceiling_bps);
        ctrl.observe(
            depth.saturating_sub(send_capacity) as u32,
            send_capacity == 0,
            now,
        );
        // FR-35 — any decrease (full channel, overflow, stall, age) since the
        // last tick pulls the learned ceiling back to the post-decrease target.
        let decreases = ctrl.decreases();
        if decreases != self.last_decreases {
            self.last_decreases = decreases;
            if constrained {
                self.learner.on_decrease(ctrl.desired(), now);
            }
        }
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

    /// Feed the AIMD a congestion sample from a BLOCKED SEND, without
    /// consuming the move.
    ///
    /// `send_wait` measures the pipe's refusal to drain directly: no clock
    /// sync, no viewer, and it works on both transports — a frame that sat
    /// seconds inside the DataChannel send call is unambiguous congestion.
    /// It was telemetry-only on relay because the goodput clamp is
    /// direct-only (the FR-1 P2 relay regression), so the one signal that
    /// always works was the one nothing acted on.
    ///
    /// Deliberately NOT `on_send_overflow`: that one calls `take_pending`,
    /// which marks the move applied while the encoder is never told. The
    /// pump's own `pre_encode_tick` picks this up one frame later and
    /// routes it through the normal apply arms — the same discipline the
    /// FR-15 age loop follows.
    pub fn note_send_stall(&mut self, wait: Duration, now: Instant) {
        // FR-35 — a send blocked ≥ 1 s is a HARD stall: ×0.5 at once.
        let hard = self.learner.on_stall(wait, now);
        if let Some(ctrl) = self.aimd.as_mut() {
            if hard {
                ctrl.apply_hard_md(now);
            } else {
                ctrl.note_buffer_overflow(now);
            }
        }
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
    #[allow(clippy::too_many_arguments)] // one tick = the pump's per-window facts; a struct would only rename the list
    pub fn tick_viewer_window(
        &mut self,
        now: Instant,
        target_fps: u32,
        take_report: impl FnOnce() -> u32,
        take_age: impl FnOnce() -> u64,
        take_link: impl FnOnce() -> u64,
        constrained: bool,
        fold_followers: impl FnOnce(u32) -> u32,
        sent_bytes_total: u64,
    ) -> Option<ViewerWindow> {
        let elapsed = now.duration_since(self.viewer_window_at);
        if elapsed < VIEWER_WINDOW {
            return None;
        }
        self.viewer_window_at = now;
        // FR-35 — the window's delivered rate, from the send task's running
        // byte total (bits per second over the actual elapsed window).
        let sent_bps: u32 = {
            let bytes = sent_bytes_total.saturating_sub(self.last_sent_bytes);
            self.last_sent_bytes = sent_bytes_total;
            let ms = elapsed.as_millis().max(1) as u64;
            (bytes.saturating_mul(8000) / ms).min(u32::MAX as u64) as u32
        };
        // Fold the window's blocked-send samples in one byte-weighted
        // aggregate. Deliberately inside the window gate rather than per
        // frame — the send task is a different task, and this is the
        // existing once-a-second rendezvous.
        let samples = self.goodput_sink.drain();
        self.goodput.observe_window(&samples, now);
        let (reported_fps, struggling) = viewer_rate::unpack_report(take_report());
        // FR-15 — the viewer's paint age is the only sensor that sees the
        // whole constrained path (WG-over-DERP/TCP queues sit below every
        // agent counter). Sustained age over the learned floor drives BOTH
        // existing responses: the fps cap (folded into `struggling` — an
        // instant, re-open-free byte cut) and a rate-limited MD (returned
        // as `age_md` so the pump applies it under its normal — on thrifty
        // relay, FR-10-deferred — rules). Direct transports keep their own
        // machinery; the loop still LEARNS there so the heartbeat can show
        // ages on every transport.
        let report = viewer_rate::unpack_age(take_age());
        let mut age_over = false;
        if let Some((avg, min, rtt)) = report {
            // P2 — half the viewer's own measured round trip is the smallest
            // age this path can physically produce; the loop uses it both to
            // reject impossible floor samples and to catch a session that was
            // congested from its first window.
            let triggered = self.age_loop.observe(avg, min, rtt / 2);
            age_over = triggered && constrained && self.age_feedback;
        }
        let age_ms = report.map(|(avg, min, _)| (avg, min));
        self.last_viewer_age =
            report.map(|(avg, _, _)| (avg, self.age_loop.floor_ms().unwrap_or(0)));
        // FR-59 P3 — the viewer's own view of the link. Unlike the age
        // report above, neither quantity needs the clock probe, so this
        // still speaks in the windows where the age is absent or was
        // rejected as implausible — which on a jittery mobile path is most
        // of them. Learned on every transport, ACTED on when constrained
        // (a direct path has the measured-ceiling clamp for this job).
        let link = viewer_rate::unpack_link(take_link());
        let verdict = link
            .map(|(rx_bps, queue_ms)| self.link_loop.observe(rx_bps, queue_ms))
            .unwrap_or_default();
        let link_acts = constrained && self.slow_link.viewer_rate_clamp;
        if link_acts {
            match (verdict.congested, link) {
                (true, Some((rx_bps, _))) => self.link_rx_bps = Some((rx_bps, now)),
                // Cleared the moment a window is NOT congested: the clamp
                // describes a live overload, not a remembered one.
                (false, Some(_)) => self.link_rx_bps = None,
                _ => {}
            }
        }
        // FR-59 P3 — a growing transit queue folds into `struggling` exactly
        // as FR-15's age excess does: an instant, re-open-free byte cut
        // through machinery that already exists.
        let link_over = link_acts && verdict.congested;
        let own_div = self.viewer_rate.observe(
            reported_fps,
            struggling || age_over || link_over,
            target_fps,
        );
        let new_div = own_div.max(fold_followers(own_div));
        let changed = new_div != self.skip_divisor;
        self.skip_divisor = new_div;
        // Feed the AIMD a congestion sample and STOP — deliberately no
        // `take_pending` here: consuming the move would mark it applied
        // while the encoder never heard about it. The pump's own
        // `pre_encode_tick` picks it up one frame later (≤33 ms) and
        // routes it through the existing apply arms.
        // FR-59 P3 — same treatment for a growing transit queue. This is
        // the arm the field case needed: the send channel was never full,
        // so nothing else in the loop could produce a decrease.
        if (age_over || link_over)
            && let Some(ctrl) = self.aimd.as_mut()
        {
            ctrl.note_buffer_overflow(now);
        }
        // FR-35 — hand the learner this window's evidence.
        let ceiling_grown = if constrained {
            let desired = self.aimd.as_ref().map(|c| c.desired()).unwrap_or(0);
            self.learner
                .on_window(desired, sent_bps, self.last_viewer_age, now)
        } else {
            None
        };
        Some(ViewerWindow {
            reported_fps,
            struggling,
            cap_fps: self.viewer_rate.cap_fps(),
            skip_divisor: new_div,
            changed,
            age_ms,
            age_over,
            ceiling_grown,
            link_congested: link_over,
            link_ceiling_bps: verdict.ceiling_bps,
        })
    }

    /// FR-15 — the last viewer age report (window avg, learned floor), ms,
    /// for the pump heartbeats: field verification reads the loop from
    /// `agent_logs` instead of the viewer's screen.
    pub fn viewer_age(&self) -> Option<(u16, u16)> {
        self.last_viewer_age
    }

    /// FR-15 P2 — count of floor samples rejected as below the path's
    /// physical minimum. A climbing count means the clock probe is being
    /// skewed by the congestion it rides through; that is a different fault
    /// from a slow path and the heartbeat must not conflate them.
    pub fn viewer_age_implausible(&self) -> u32 {
        self.age_loop.implausible_samples()
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
    ///
    /// P5 (FR-1) — on a HW encoder (`is_hw`) the FIRST relief lever is
    /// CADENCE, not bitrate: encode time there is pixels-bound and
    /// nearly bitrate-independent, so the factor was pure quality loss
    /// (field: factor 0.4 with encode still ~25 ms). While the fps pace
    /// is engaged the exposed factor is masked at 1.0; the pressure's
    /// INTERNAL factor keeps marching so `tier_signal`'s
    /// exhausted-lever condition still fires when even the paced floor
    /// can't hold — resolution stays the second lever, exactly as
    /// before.
    pub fn heartbeat(
        &mut self,
        avg_encode_ms: f32,
        is_hw: bool,
        target_fps: u32,
        now: Instant,
    ) -> Option<TierChange> {
        self.encode_factor = self.pressure.observe(avg_encode_ms);
        let paced = if is_hw {
            self.pace.observe(self.pressure.ewma_ms(), target_fps)
        } else {
            None
        };
        if paced.is_some() {
            self.encode_factor = 1.0;
        }
        let new_cap = self.tier.observe(self.pressure.tier_signal(), now)?;
        self.auto_res_cap = new_cap;
        Some(TierChange {
            cap_long_edge: new_cap,
            ewma_encode_ms: self.pressure.ewma_ms(),
        })
    }

    /// The paced consumption rate for HW encoders (`None` = run at
    /// target). Consumed by the pump's cadence gate each loop.
    pub fn paced_fps(&self) -> Option<u32> {
        self.pace.paced()
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
        RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                measured_ceiling: true,
                age_feedback: true,
                ..GovernorFlags::default()
            },
            0,
            None,
            now,
        )
    }

    /// FR-35 — on a constrained session the learner lifts the ceiling above
    /// the nominal only on carried, clean windows, never above hi, and a
    /// direct session is untouched.
    #[test]
    fn ceiling_learner_lifts_the_constrained_ceiling_on_carried_windows() {
        let start = Instant::now();
        let mut g = RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                measured_ceiling: false,
                age_feedback: false,
                ..GovernorFlags::default()
            },
            8_000_000,
            None,
            start,
        );
        let a = g
            .pre_encode_tick(
                3_000_000,
                crate::encode::MIN_BITRATE_BPS,
                true,
                DEPTH,
                start,
            )
            .expect("initial apply");
        assert_eq!(
            a.bps, 3_000_000,
            "opens at the nominal with nothing learned"
        );
        let mut bytes: u64 = 0;
        let mut grown = 0;
        for i in 1..=40u64 {
            bytes += 3_000_000 / 8; // the window carried 3 Mbps
            let now = start + Duration::from_secs(i);
            let w = g
                .tick_viewer_window(now, 30, || 0, || 0, || 0, true, |o| o, bytes)
                .expect("window due");
            if w.ceiling_grown.is_some() {
                grown += 1;
            }
            let _ = g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, now);
        }
        assert!(grown >= 3, "expected several steps, got {grown}");
        let lifted = g.effective_ceiling(3_000_000, true);
        assert!(lifted > 3_000_000 && lifted <= 8_000_000, "lifted={lifted}");
        assert_eq!(
            g.effective_ceiling(3_000_000, false),
            3_000_000,
            "direct is untouched"
        );
        // A quiet session (nothing carried) never lifts.
        let mut q = RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                measured_ceiling: false,
                age_feedback: false,
                ..GovernorFlags::default()
            },
            8_000_000,
            None,
            start,
        );
        let _ = q.pre_encode_tick(
            3_000_000,
            crate::encode::MIN_BITRATE_BPS,
            true,
            DEPTH,
            start,
        );
        for i in 1..=30u64 {
            let now = start + Duration::from_secs(i);
            let w = q
                .tick_viewer_window(now, 30, || 0, || 0, || 0, true, |o| o, i * 100_000)
                .expect("window due");
            assert!(w.ceiling_grown.is_none());
        }
        assert_eq!(q.effective_ceiling(3_000_000, true), 3_000_000);
        // Learning off (hi = 0): the plan verbatim, seed ignored.
        let mut off = RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                measured_ceiling: false,
                age_feedback: false,
                ..GovernorFlags::default()
            },
            0,
            Some(20_000_000),
            start,
        );
        assert_eq!(off.effective_ceiling(3_000_000, true), 3_000_000);
    }

    /// Stage 1: a held goodput estimate clamps the nominal ceiling to
    /// 85 % of the measured drain, so the target converges just under
    /// the pipe instead of congesting it every burst.
    #[test]
    fn measured_goodput_clamps_the_ceiling() {
        let start = Instant::now();
        let mut g = gov(start);
        let sink = g.goodput_sink();
        // One congested second: 40 × 30 KB frames, 24 ms serialisation
        // each = a 10 Mbps drain measurement.
        for _ in 0..40 {
            sink.record(30_000, Duration::from_millis(24));
        }
        let t1 = start + Duration::from_secs(2);
        assert!(
            g.tick_viewer_window(t1, 30, || 0, || 0, || 0, false, |o| o, 0)
                .is_some()
        );
        assert_eq!(g.measured_goodput_bps(t1), Some(10_000_000));
        let applied = g
            .pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, DEPTH, t1)
            .expect("initial apply");
        assert_eq!(
            applied.bps, 8_500_000,
            "12M nominal clamped to 0.85 × 10M measured"
        );
    }

    /// FR-59 P1 — the whole point, end to end: on a CONSTRAINED transport
    /// with a measured pipe far under the legibility floor, the AIMD is
    /// actually allowed to converge below `MIN_BITRATE_BPS`.
    ///
    /// The pre-FR-59 arm is the regression this exists to prevent: with
    /// the relief off, the same congestion sequence bottoms out AT the
    /// floor — 3.8× the pipe — which is what pinned the field session at
    /// 1.5 Mbps on a 395 kbps hotspot while the viewer ran seconds behind.
    #[test]
    fn a_measured_slow_pipe_lets_the_floor_and_the_target_descend() {
        // 395 kbps measured: 12 × 1 KB frames each taking ~20 ms.
        fn feed_slow_pipe(g: &mut RateGovernor, start: Instant) -> Instant {
            let sink = g.goodput_sink();
            for _ in 0..12 {
                sink.record(1_000, Duration::from_micros(20_248));
            }
            let t1 = start + Duration::from_secs(2);
            assert!(
                g.tick_viewer_window(t1, 30, || 0, || 0, || 0, true, |o| o, 0)
                    .is_some()
            );
            t1
        }

        // Relief ON (shipped posture).
        let start = Instant::now();
        let mut g = gov(start);
        let t1 = feed_slow_pipe(&mut g, start);
        // Pinned exactly, so a drift in either the fixture or the
        // derivation shows up as its own failure rather than as a
        // confusing one in the floor assert below.
        let measured = g.measured_goodput_bps(t1).expect("a held estimate");
        assert_eq!(measured, 395_100, "the field-class pipe, ~395 kbps");
        // Drive the AIMD down with a saturated channel. The MD is rate
        // limited to one per 500 ms, so walk the clock.
        let mut t = t1;
        for _ in 0..40 {
            t += Duration::from_millis(600);
            g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, 0, t);
        }
        let floor = g.relieved_floor_bps().expect("the relief engaged");
        assert_eq!(floor, 335_835, "85 % of the measured pipe");
        assert!(
            g.applied_bps() < crate::encode::MIN_BITRATE_BPS,
            "target {} must be free to fall below the 1.5 Mbps pin",
            g.applied_bps()
        );
        assert!(
            g.applied_bps() >= floor,
            "…but never below the relieved floor"
        );

        // Relief OFF — the pre-FR-59 pin.
        let start = Instant::now();
        let mut off = RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                floor_relief: false,
                ..GovernorFlags::default()
            },
            0,
            None,
            start,
        );
        let t1 = feed_slow_pipe(&mut off, start);
        let mut t = t1;
        for _ in 0..40 {
            t += Duration::from_millis(600);
            off.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, 0, t);
        }
        assert_eq!(off.relieved_floor_bps(), None, "kill switch: no relief");
        assert_eq!(
            off.applied_bps(),
            crate::encode::MIN_BITRATE_BPS,
            "pinned at the floor, 3.8× the measured pipe"
        );
    }

    /// FR-59 P1 — a DIRECT session is untouched: the relief reads the
    /// constrained-only measurement, so nothing about a direct path can
    /// lower its floor (AC5).
    #[test]
    fn the_floor_relief_never_touches_a_direct_session() {
        let start = Instant::now();
        let mut g = gov(start);
        let sink = g.goodput_sink();
        for _ in 0..12 {
            sink.record(1_000, Duration::from_micros(20_248));
        }
        let t1 = start + Duration::from_secs(2);
        g.tick_viewer_window(t1, 30, || 0, || 0, || 0, false, |o| o, 0);
        let mut t = t1;
        for _ in 0..40 {
            t += Duration::from_millis(600);
            g.pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, 0, t);
        }
        assert_eq!(g.relieved_floor_bps(), None);
        assert_eq!(g.applied_bps(), crate::encode::MIN_BITRATE_BPS);
    }

    /// FR-59 P3 — the case the whole program exists for: the agent's own
    /// send NEVER blocks (so there is no goodput estimate at all), while
    /// the viewer reports its transit queue growing. Nothing else in the
    /// loop can see this — and the response has to reach the encoder.
    #[test]
    fn a_growing_viewer_queue_drives_the_rate_with_no_agent_side_evidence() {
        let start = Instant::now();
        let mut g = gov(start);
        // Not one blocked send: the queue is downstream of the agent.
        assert_eq!(g.measured_goodput_bps(start), None);

        let mut t = start;
        let mut congested_seen = false;
        for i in 0..8 {
            t += Duration::from_millis(1100);
            let w = g
                .tick_viewer_window(
                    t,
                    30,
                    || viewer_rate::pack_report(30, false),
                    || 0, // no age: the clock probe never locked
                    || viewer_rate::pack_link(400_000, 250),
                    true,
                    |o| o,
                    0,
                )
                .expect("window due");
            if i >= 1 {
                assert!(w.link_congested, "window {i} should read congested");
                congested_seen = true;
            }
            // Walk the AIMD forward past its MD spacing.
            for _ in 0..4 {
                t += Duration::from_millis(200);
                g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
            }
        }
        assert!(congested_seen);
        // 90 % of the 400 kbps that actually arrived.
        assert!(
            g.applied_bps() <= 360_000,
            "target {} must be bounded by the arrival rate",
            g.applied_bps()
        );
        // …and the floor relief is what let it get there: without it
        // `set_ceiling` would have raised the clamp back to 1.5 Mbps.
        assert!(g.relieved_floor_bps().is_some());

        // A window that reports a DRAINING queue releases the clamp, and
        // the AIMD is free to climb again — recovery is not one-way.
        t += Duration::from_millis(1100);
        let w = g
            .tick_viewer_window(
                t,
                30,
                || viewer_rate::pack_report(30, false),
                || 0,
                || viewer_rate::pack_link(400_000, -200),
                true,
                |o| o,
                0,
            )
            .expect("window due");
        assert!(!w.link_congested);
        for _ in 0..40 {
            t += Duration::from_secs(6);
            g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        }
        assert!(
            g.applied_bps() > 360_000,
            "the clamp must not outlive the congestion (got {})",
            g.applied_bps()
        );
    }

    /// FR-59 P3 kill switch, and the direct-path guarantee (AC5).
    #[test]
    fn the_viewer_link_clamp_is_off_by_switch_and_on_direct() {
        for (label, flags, constrained) in [
            (
                "kill switch",
                GovernorFlags {
                    viewer_rate_clamp: false,
                    ..GovernorFlags::default()
                },
                true,
            ),
            ("direct path", GovernorFlags::default(), false),
        ] {
            let start = Instant::now();
            let mut g = RateGovernor::new(30, DEPTH, flags, 0, None, start);
            let mut t = start;
            for _ in 0..8 {
                t += Duration::from_millis(1100);
                let w = g
                    .tick_viewer_window(
                        t,
                        30,
                        || viewer_rate::pack_report(30, false),
                        || 0,
                        || viewer_rate::pack_link(400_000, 250),
                        constrained,
                        |o| o,
                        0,
                    )
                    .expect("window due");
                assert!(!w.link_congested, "{label}: must not act");
                for _ in 0..4 {
                    t += Duration::from_millis(200);
                    g.pre_encode_tick(
                        CEILING,
                        crate::encode::MIN_BITRATE_BPS,
                        constrained,
                        DEPTH,
                        t,
                    );
                }
            }
            assert_eq!(
                g.applied_bps(),
                CEILING,
                "{label}: the target must be untouched"
            );
            assert_eq!(g.relieved_floor_bps(), None, "{label}");
        }
    }

    /// FR-59 P6 — a seeded ceiling that the first measured window
    /// contradicts is abandoned, reported ONCE, and the session plans at
    /// the nominal band instead.
    #[test]
    fn a_contradicted_seed_is_abandoned_and_reported_once() {
        let start = Instant::now();
        let mut g = RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                measured_ceiling: true,
                age_feedback: true,
                ..GovernorFlags::default()
            },
            8_000_000,
            Some(5_964_000),
            start,
        );
        // Before any measurement the seed rules: 85 % of 5.964 M.
        assert_eq!(g.effective_ceiling(3_000_000, true), 5_069_400);
        let sink = g.goodput_sink();
        for _ in 0..12 {
            sink.record(1_000, Duration::from_micros(20_248));
        }
        let t1 = start + Duration::from_secs(2);
        g.tick_viewer_window(t1, 30, || 0, || 0, || 0, true, |o| o, 0);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, 0, t1);
        assert_eq!(g.take_seed_abandoned(), Some(5_069_400));
        assert_eq!(g.take_seed_abandoned(), None, "reported exactly once");
        assert_eq!(
            g.effective_ceiling(3_000_000, true),
            3_000_000,
            "back to the nominal band"
        );
    }

    /// The kill switch keeps stage 1 observe-and-report only.
    #[test]
    fn measured_ceiling_disabled_keeps_nominal() {
        let start = Instant::now();
        let mut g = RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                measured_ceiling: false,
                age_feedback: true,
                ..GovernorFlags::default()
            },
            0,
            None,
            start,
        );
        let sink = g.goodput_sink();
        for _ in 0..40 {
            sink.record(30_000, Duration::from_millis(24));
        }
        let t1 = start + Duration::from_secs(2);
        g.tick_viewer_window(t1, 30, || 0, || 0, || 0, false, |o| o, 0);
        assert_eq!(g.measured_goodput_bps(t1), Some(10_000_000));
        let applied = g
            .pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, DEPTH, t1)
            .expect("initial apply");
        assert_eq!(applied.bps, CEILING, "disabled = nominal band");
    }

    /// Relay sessions IGNORE the measured clamp (field 2026-08-27: the
    /// lumpy TURN-TCP pipe produced near-zero samples during stalls and
    /// the clamp rode the floor — worse than the nominal relay clamp).
    /// The estimate itself still folds (observe-only there).
    #[test]
    fn constrained_sessions_ignore_the_measured_clamp() {
        let start = Instant::now();
        let mut g = gov(start);
        let sink = g.goodput_sink();
        for _ in 0..40 {
            sink.record(30_000, Duration::from_millis(24));
        }
        let t1 = start + Duration::from_secs(2);
        g.tick_viewer_window(t1, 30, || 0, || 0, || 0, false, |o| o, 0);
        assert_eq!(g.measured_goodput_bps(t1), Some(10_000_000));
        let applied = g
            .pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t1)
            .expect("initial apply");
        assert_eq!(
            applied.bps, CEILING,
            "constrained = nominal band, measurement observed but not consumed"
        );
    }

    /// The measurement may only ever LOWER the clamp — a pipe measured
    /// faster than the nominal ceiling changes nothing.
    #[test]
    fn measured_clamp_only_lowers() {
        let start = Instant::now();
        let mut g = gov(start);
        let sink = g.goodput_sink();
        // 40 × 60 KB over 24 ms each = 20 Mbps; derived 17M > 12M nominal.
        for _ in 0..40 {
            sink.record(60_000, Duration::from_millis(24));
        }
        let t1 = start + Duration::from_secs(2);
        g.tick_viewer_window(t1, 30, || 0, || 0, || 0, false, |o| o, 0);
        let applied = g
            .pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, DEPTH, t1)
            .expect("initial apply");
        assert_eq!(applied.bps, CEILING, "measurement never raises the ceiling");
    }

    #[test]
    fn first_pre_encode_tick_applies_the_initial_ceiling_once() {
        let now = Instant::now();
        let mut g = gov(now);
        // First tick constructs the AIMD at the ceiling; the seeded
        // last_applied=0 makes take_pending emit the initial target
        // exactly once.
        let applied = g
            .pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, DEPTH, now)
            .expect("initial apply");
        assert_eq!(applied.bps, CEILING);
        assert!(applied.changed);
        assert_eq!(g.applied_bps(), CEILING);
        // Same conditions again — change-gated, nothing to apply.
        assert!(
            g.pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, DEPTH, now)
                .is_none()
        );
    }

    #[test]
    fn sustained_full_channel_walks_bitrate_down_never_below_floor() {
        let start = Instant::now();
        let mut g = gov(start);
        g.pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, DEPTH, start);
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
        g.pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, DEPTH, now);
        // A transport flip to relay shrinks the ceiling; the very next
        // tick must emit the clamped target (refine can raise dims —
        // and thereby the requested ceiling — but the constrained
        // clamp always wins; see also the policy-level flatness test).
        let relay_ceiling = 3_000_000;
        let applied = g
            .pre_encode_tick(
                relay_ceiling,
                crate::encode::MIN_BITRATE_BPS,
                false,
                DEPTH,
                now,
            )
            .expect("clamp emits");
        assert_eq!(applied.bps, relay_ceiling);
    }

    /// The area-scaled floor rides the same tick: a collapsed target is
    /// lifted to the floor immediately, and a relay flip (flat floor +
    /// low ceiling, same call order as the pumps) restores MD room.
    #[test]
    fn floor_passes_through_to_the_controller() {
        let start = Instant::now();
        let mut g = gov(start);
        g.pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, DEPTH, start);
        // Sustained congestion walks the target to the flat floor.
        let mut now = start;
        for _ in 0..40 {
            now += Duration::from_millis(600);
            g.on_backpressure_skip(now);
        }
        assert_eq!(g.applied_bps(), crate::encode::MIN_BITRATE_BPS);
        // A big-screen floor lifts it on the next tick.
        let applied = g
            .pre_encode_tick(CEILING, 3_000_000, false, DEPTH, now)
            .expect("floor lift emits");
        assert_eq!(applied.bps, 3_000_000);
        // Relay flip: flat floor + 2 M ceiling in one tick — the clamp
        // wins, the stale floor cannot pin the target above it.
        let applied = g
            .pre_encode_tick(2_000_000, crate::encode::MIN_BITRATE_BPS, false, DEPTH, now)
            .expect("clamp emits");
        assert_eq!(applied.bps, 2_000_000);
    }

    #[test]
    fn rebuild_forces_a_reapply_of_the_current_target() {
        let now = Instant::now();
        let mut g = gov(now);
        g.pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, DEPTH, now);
        assert!(
            g.pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, DEPTH, now)
                .is_none()
        );
        // A fresh encoder starts at its constructor maxrate — the
        // governor must re-emit the (unchanged) desired target so the
        // pump re-applies it.
        g.on_encoder_rebuilt();
        let applied = g
            .pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, DEPTH, now)
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
            || 0,
            || 0,
            false,
            |own| own,
            0,
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
            || 0,
            || 0,
            false,
            |own| own,
            0,
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
            .tick_viewer_window(
                start + Duration::from_secs(2),
                30,
                || 0,
                || 0,
                || 0,
                false,
                |_| 3,
                0,
            )
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
            let w = g.tick_viewer_window(
                now,
                30,
                || viewer_rate::pack_report(1, true),
                || 0,
                || 0,
                false,
                |own| own,
                0,
            );
            let w = w.expect("2 s apart — every window due");
            assert!(
                w.skip_divisor <= 3,
                "divisor {} exceeds ceil(30/min_fps=12)=3 — encoded cadence starved",
                w.skip_divisor
            );
        }
    }

    /// P5 — on a HW encoder, sustained saturation engages the fps pace
    /// and MASKS the bitrate factor (pixels-bound encode time doesn't
    /// respond to bitrate); recovery releases both.
    #[test]
    fn hw_saturation_paces_fps_and_masks_the_factor() {
        let start = Instant::now();
        let mut g = gov(start);
        for i in 1..=10u64 {
            let _ = g.heartbeat(25.0, true, 60, start + Duration::from_secs(2 * i));
        }
        assert_eq!(g.paced_fps(), Some(40), "25 ms EWMA ⇒ ~40 fps pace");
        assert_eq!(g.encode_factor(), 1.0, "factor masked while paced");
        for i in 11..=30u64 {
            let _ = g.heartbeat(8.0, true, 60, start + Duration::from_secs(2 * i));
        }
        assert_eq!(g.paced_fps(), None, "recovered encoder releases the pace");
    }

    /// FR-15 — sustained viewer age over the learned floor on a
    /// CONSTRAINED transport lowers the target, and the decrease reaches
    /// the pump through the ordinary `pre_encode_tick` apply path (the
    /// window tick must never consume it itself).
    #[test]
    fn constrained_age_excess_lowers_the_target_through_the_normal_apply() {
        let start = Instant::now();
        let mut g = gov(start);
        // Establish the AIMD at the relay-ish ceiling.
        let ceiling = 3_000_000;
        let first = g
            .pre_encode_tick(ceiling, crate::encode::MIN_BITRATE_BPS, true, DEPTH, start)
            .expect("initial apply");
        assert_eq!(first.bps, ceiling);
        // Two windows at a ~60 ms floor, then sustained 200 ms age.
        let mut applied = first.bps;
        for (i, (avg, min)) in [(62u16, 60u16), (64, 61), (200, 62), (205, 63), (210, 63)]
            .into_iter()
            .enumerate()
        {
            let t = start + Duration::from_millis(1100 * (i as u64 + 1));
            let w = g
                .tick_viewer_window(
                    t,
                    30,
                    || viewer_rate::pack_report(30, false),
                    || viewer_rate::pack_age(avg, min, 100),
                    || 0,
                    true,
                    |o| o,
                    0,
                )
                .expect("window due");
            assert_eq!(w.age_ms, Some((avg, min)));
            if let Some(a) =
                g.pre_encode_tick(ceiling, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t)
            {
                applied = a.bps;
            }
        }
        assert!(
            applied < ceiling,
            "sustained age excess left the target at the open-loop ceiling ({applied})"
        );
        assert_eq!(g.viewer_age().map(|(_, f)| f), Some(60), "floor learned");
    }

    /// FR-15 — the same age history on a DIRECT transport changes
    /// nothing (direct owns the measured ceiling + byte gate), and the
    /// kill switch reverts constrained sessions to the same open loop.
    #[test]
    fn age_loop_is_constrained_only_and_respects_the_kill_switch() {
        let ceiling = 3_000_000;
        let ages = [(62u16, 60u16), (64, 61), (200, 62), (205, 63), (210, 63)];
        for (label, constrained, feedback) in
            [("direct", false, true), ("kill switch", true, false)]
        {
            let start = Instant::now();
            let mut g = RateGovernor::new(
                30,
                DEPTH,
                GovernorFlags {
                    measured_ceiling: false,
                    age_feedback: feedback,
                    ..GovernorFlags::default()
                },
                0,
                None,
                start,
            );
            let mut applied = g
                .pre_encode_tick(
                    ceiling,
                    crate::encode::MIN_BITRATE_BPS,
                    constrained,
                    DEPTH,
                    start,
                )
                .expect("initial apply")
                .bps;
            for (i, (avg, min)) in ages.into_iter().enumerate() {
                let t = start + Duration::from_millis(1100 * (i as u64 + 1));
                let w = g
                    .tick_viewer_window(
                        t,
                        30,
                        || viewer_rate::pack_report(30, false),
                        || viewer_rate::pack_age(avg, min, 100),
                        || 0,
                        constrained,
                        |o| o,
                        0,
                    )
                    .expect("window due");
                assert!(!w.age_over, "{label}: age must not act");
                if let Some(a) = g.pre_encode_tick(
                    ceiling,
                    crate::encode::MIN_BITRATE_BPS,
                    constrained,
                    DEPTH,
                    t,
                ) {
                    applied = a.bps;
                }
            }
            assert_eq!(applied, ceiling, "{label}: target moved anyway");
            // The floor is still LEARNED everywhere — the heartbeat reports
            // ages on every transport, it just doesn't act on them.
            assert_eq!(g.viewer_age().map(|(_, f)| f), Some(60), "{label}");
        }
    }

    /// FR-15 — a viewer that reports no age (pre-FR-15 web) leaves the
    /// constrained session byte-identical to the open-loop posture.
    #[test]
    fn absent_age_report_leaves_the_loop_off() {
        let start = Instant::now();
        let mut g = gov(start);
        let ceiling = 3_000_000;
        let mut applied = g
            .pre_encode_tick(ceiling, crate::encode::MIN_BITRATE_BPS, true, DEPTH, start)
            .expect("initial apply")
            .bps;
        for i in 1..=6u64 {
            let t = start + Duration::from_millis(1100 * i);
            let w = g
                .tick_viewer_window(
                    t,
                    30,
                    || viewer_rate::pack_report(30, false),
                    || 0, // no age slot written this window
                    || 0, // no link slot written this window
                    true,
                    |o| o,
                    0,
                )
                .expect("window due");
            assert_eq!(w.age_ms, None);
            assert!(!w.age_over);
            if let Some(a) =
                g.pre_encode_tick(ceiling, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t)
            {
                applied = a.bps;
            }
        }
        assert_eq!(applied, ceiling);
        assert_eq!(g.viewer_age(), None);
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
            let _ = g.heartbeat(100.0, false, 30, start + Duration::from_secs(2 * i));
        }
        assert!(
            g.encode_factor() < 1.0,
            "sustained saturation left the factor at full quality"
        );
        assert!(g.encode_factor() >= crate::encode::encode_pressure::FACTOR_FLOOR);
    }
}

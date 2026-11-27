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

use super::aimd::{AimdController, Cause, Causes, CeilingSource, Congestion, FloorSource};
use super::encode_pressure::{DownscaleTier, EncodePressure};
use super::goodput::{GoodputEstimator, GoodputSink};
use super::pipe::{Basis, MeasuredBy, PathNumber, PathView};
use super::viewer_rate::{self, ViewerRateController};

/// A bitrate target the pump must apply to the live encoder
/// (`enc.set_bitrate(bps)`). Emitted at most once per underlying
/// AIMD move (`take_move` is change-gated).
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
    /// FR-148 P0 — why the target moved since the last one handed out. A held
    /// target (the FFmpeg pump's `deferred_bps`) keeps it until it lands.
    pub why: Why,
}

/// FR-148 D51 B4a-1 — a skip at the pump's capacity gate, as the delivery
/// sampler classes it (`DeliverySampler::on_skip`), at the time it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateSkip {
    /// A full send channel, a congested follower, or today's byte gate:
    /// [`RateGovernor::on_backpressure_skip`].
    Backpressure(Instant),
    /// The in-flight budget's own hold under `on`, outside a still span:
    /// [`RateGovernor::on_budget_hold`].
    BudgetHold(Instant),
}

/// FR-148 P0 — why the live target moved: the controller's causes since the
/// last target it handed out, in the order they happened, and for an additive
/// increase whether the sender was application-limited. Written as the causes
/// joined by `+`: `decrease(full)`, `decrease(stall)+ceiling(plan)`,
/// `increase(app-limited)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Why {
    pub causes: Causes,
    /// For an `increase` only: did the last viewer window write under half the
    /// target it ran with (R3, [`pair_memory::pushing_against_plan`])? `None`
    /// without an increase, or before the first window.
    ///
    /// [`pair_memory::pushing_against_plan`]: super::pair_memory::pushing_against_plan
    pub app_limited: Option<bool>,
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut sep = "";
        for cause in self.causes.iter() {
            f.write_str(sep)?;
            sep = "+";
            match (cause, self.app_limited) {
                (Cause::Increase, Some(true)) => f.write_str("increase(app-limited)")?,
                (Cause::Increase, Some(false)) => f.write_str("increase(loaded)")?,
                (Cause::Increase, None) => f.write_str("increase(no-window)")?,
                (other, _) => write!(f, "{other}")?,
            }
        }
        match (sep, self.causes.more()) {
            ("", _) => f.write_str("none"),
            (_, true) => f.write_str("+more"),
            (_, false) => Ok(()),
        }
    }
}

/// How many distinct [`Why`]s the tally keeps between two heartbeats. Past it,
/// a move counts under `more`, so the tally stays bounded even where nothing
/// drains it (the heartbeat line filtered out).
const MOVES_KEPT: usize = 16;

/// FR-148 P0 — the applied moves since the last heartbeat, counted by their
/// [`Why`], in the order each first happened. Written `why:count` joined by
/// `,` (`increase(app-limited):3,ceiling(plan):1`), or `none`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Moves {
    kept: Vec<(Why, u32)>,
    more: u32,
}

impl Moves {
    fn note(&mut self, why: Why) {
        if let Some((_, n)) = self.kept.iter_mut().find(|(w, _)| *w == why) {
            *n = n.saturating_add(1);
        } else if self.kept.len() < MOVES_KEPT {
            self.kept.push((why, 1));
        } else {
            self.more = self.more.saturating_add(1);
        }
    }
}

impl std::fmt::Display for Moves {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.kept.is_empty() && self.more == 0 {
            return f.write_str("none");
        }
        let mut sep = "";
        for (why, n) in &self.kept {
            write!(f, "{sep}{why}:{n}")?;
            sep = ",";
        }
        if self.more > 0 {
            write!(f, "{sep}more:{}", self.more)?;
        }
        Ok(())
    }
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
    /// come out, whatever its own send queue says. FR-79 V5b — and the sender
    /// offered more than arrived in both, see `viewer_rate::OfferLoop`.
    pub link_congested: bool,
    /// FR-59 P3 — the ceiling that window's ARRIVAL rate justifies.
    /// `None` unless `link_congested` — see `viewer_rate::LinkLoop`.
    pub link_ceiling_bps: Option<u32>,
    /// FR-59 P4 — production is pausing this long so the transit queue
    /// can drain. `None` = no drain ordered this window.
    pub drain_for_ms: Option<u32>,
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
    /// FR-59 P4 — a transit queue too deep to cut our way out of is
    /// DRAINED by pausing production (`encode::queue_drain_enabled`).
    pub queue_drain: bool,
    /// FR-79 V5b — only a measurement is evidence about the rate
    /// (`encode::measured_evidence_enabled`): a stall is judged against a live
    /// reading, a later delivery can take a reading out, a hold must be carried
    /// to reach the pair memory, the memory records a reading or nothing, and a
    /// growing queue counts only while the sender offered more than arrived.
    /// Off = V5a's rules, byte for byte.
    pub measured_evidence: bool,
    /// FR-63 — the opener ramps instead of committing to a constant
    /// (`encode::rate_slow_start_enabled`).
    ///
    /// ⚠️ The ONE flag here that is **not** the shipped posture: it defaults
    /// OFF, in `Default` too, because this FR's rule is that no controller
    /// change ships live without field evidence. Every other field defaults
    /// on because it already earned that.
    pub slow_start: bool,
    /// FR-63 B1 — whether the shadow controller (`encode::ratectl`) runs beside
    /// the governor. `Shadow` (default) runs it observe-only; `Off` is the kill
    /// switch. It steers NOTHING in B1 — the heartbeat's `shadow_*` fields are
    /// its only output — so, unlike every other flag here, turning it on cannot
    /// change a rate the encoder sees.
    pub ratectl: super::RatectlMode,
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
            queue_drain: true,
            measured_evidence: true,
            // FR-63 — see the field doc: OFF even here, until field evidence.
            slow_start: false,
            // FR-63 B1 — the shadow runs by default; it drives nothing, so ON
            // is the shipped posture (unlike `slow_start`, which acts).
            ratectl: super::RatectlMode::Shadow,
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
            queue_drain: super::queue_drain_enabled(),
            measured_evidence: super::measured_evidence_enabled(),
            slow_start: super::rate_slow_start_enabled(),
            ratectl: super::ratectl_mode(),
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
    /// FR-70 M0 — the last window's age at ARRIVAL (ms), for the heartbeat's
    /// sender / transit / viewer split. `None` from a pre-M0 viewer.
    last_viewer_arrival: Option<u16>,
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
    // FR-112 P2 — the software pump's pace (never below 30), kept apart from
    // the hardware one above: the VP9-444 pump feeds only this.
    sw_pace: super::encode_pressure::SoftwarePace,

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
    /// FR-79 V5b — was the sender feeding the queue that loop says is growing?
    offer_loop: viewer_rate::OfferLoop,
    /// FR-79 V5b — valid congested windows the sender was not feeding, for the
    /// heartbeat: "the loop went quiet" and "this rule stopped it" read apart.
    link_unoffered: u32,
    /// FR-59 P3 — the viewer's ARRIVAL rate and when it was reported.
    /// Held only while its queue is growing (see `viewer_rate::LinkLoop`
    /// for why that gate is not optional), and expired by
    /// [`LINK_CEILING_TTL`] so a viewer that goes SILENT cannot pin the
    /// session forever — silence is not evidence of congestion. Stored
    /// RAW rather than pre-discounted, because it feeds two consumers
    /// with different margins: the P3 ceiling clamp and the P1 floor.
    link_rx_bps: Option<(u32, Instant)>,
    /// FR-59 P4 — production is paused until this instant so the transit
    /// queue can drain. An INSTANT rather than a countdown, because the
    /// pump polls it from a loop that may not tick at all while paused.
    drain_until: Option<Instant>,
    /// FR-59 P8 — a pair the rate memory remembers BELOW the nominal floor
    /// (the same seed P5 reads). The session OPENS at it, and until live
    /// evidence arrives it stands in as the pipe measurement, so the P1
    /// floor relief, the P2 queue budget and the P6 contradiction check all
    /// see the remembered rate rather than the 1.5 M fleet constant. `None`
    /// for an unremembered or a fast pair, and with the relief switched off
    /// — then nothing here changes. Field 2026-09-02: `seed_bps=387500` was
    /// logged and the session still opened at the 2.55 M relay cap (the
    /// FR-35 learner only ever RAISES the ceiling), a 372 KB opener into a
    /// ~300 kbps pipe — ten seconds of queue before any measurement spoke.
    ///
    /// ⚠️ FR-70 P1: this is what the session OPENS at, and nothing more. What
    /// stands in for a measurement afterwards is `prior`, which starts here
    /// and decays — the seed held a session at 200 kbps for four minutes
    /// through the floor relief AND the queue budget while nothing measured
    /// (field 2026-09-04, `6a9abc30`; see `encode::prior`).
    open_seed_bps: Option<u32>,
    /// FR-148 P1 step 3 — where the AIMD, the first encoder and slow start
    /// open when it sits under the ceiling: today's rule gives the P8 seed
    /// above; the path model's gives its opening after a trusted push-back,
    /// which may sit above the nominal floor. `None` = at the ceiling.
    open_at_bps: Option<u32>,
    /// FR-70 P1 — the belief about the pipe while nothing measures it: the
    /// seed, then the last live measurement, decaying toward the band on
    /// clean windows. Read wherever `open_seed_bps` used to stand in.
    prior: super::prior::RatePrior,
    /// FR-70 P1 — did a send stall land since the last viewer window? The
    /// prior's "clean" verdict needs the PIPE's push-back, and a stall is the
    /// one push-back that reaches the governor between windows.
    stall_seen: bool,
    /// FR-71 T1a — the per-window plane classifier (shadow).
    pipe: super::pipe_state::PipeClassifier,
    /// FR-71 T1a — what the pump measured about its own send side since the
    /// last viewer window, handed in just before the tick. `None` = the pump
    /// keeps no such figures (the VP9-444 pump), so the sender side reads as
    /// quiet and only the split and the viewer's report classify.
    window_sender: Option<WindowSenderStats>,
    /// FR-79 — windows the validity gate rejected, per reason. ONE field in
    /// place of the four T1b, T2 and T2b each added for their own rule.
    evidence: super::evidence::Rejections,
    /// FR-79 V3b / FR-74 — the ONE belief about what the path carries, fed by
    /// every window (a demonstrated floor) and by every push-back (a capacity
    /// estimate). SHADOW for now: computed and reported, so the field can compare
    /// it against `pipe_bps` before anything acts. The one exception is V5b,
    /// which reads a rise of the floor as a veto on a reading ([`Self::refute_readings`]).
    belief: super::pipe::Pipe,
    /// FR-148 D51 — the in-flight budget's last step as the pump noted it
    /// ([`Self::note_path_rate`]). Under `on`, on a relay session, it bounds
    /// the target (B4b), it is the reading (B4b-2), and its envelope bounds
    /// the age loop and the prior's push-back (B4a-2). Off, in shadow and on a
    /// direct session it never acts, and nothing here reads it.
    budget_path: Option<super::pipe::PathRate>,
    /// FR-148 D46 B2 — the last window's [`Take`](super::pair_memory::Take)
    /// when the gate found it valid, `None` when it did not (or before the
    /// first): what a delivery sample's push-back is filed under
    /// ([`Self::on_sample_push_back`]).
    sample_take: Option<super::pair_memory::Take>,
    /// FR-79 V5b — how many times a delivery took out what stood below it, for
    /// the heartbeat.
    readings_refuted: u32,
    /// FR-79 V5b — the last reading a valid window held, the goodput ahead of the
    /// arrival rate, as the prior anchors. It outlives both their TTLs, because a
    /// truly slow pair's reading expires long before the session ends and FR-59
    /// P8 needs it in the memory. Not the prior's anchor, which [`Self::refute_readings`]
    /// can set to a delivery: a lower bound, and it would move an entry above it
    /// down for a session that measured nothing.
    last_reading_bps: Option<u32>,
    /// FR-35 — a send blocked ≥ `HARD_STALL` since the last window boundary.
    /// The ×0.5 it earns is applied there, where the gate can say whether the
    /// block was the pipe (FR-79).
    window_hard_stall: bool,
    /// FR-63 — the opener's ramp. Lazily built at the first CONSTRAINED tick
    /// because it needs that tick's RESOLVED ceiling, exactly as the AIMD is.
    /// `None` while the flag is off, on a direct transport, or once the ramp
    /// has finished — the three cases where the normal controller is already
    /// the right answer.
    slow_start: Option<super::slow_start::SlowStart>,
    /// FR-63 — has the "armed" line been logged for this session? The ramp is
    /// the only rate decision with no other trace, so a session where it did
    /// or did not engage would otherwise be indistinguishable in a log.
    slow_start_logged: bool,
    /// FR-63 — did any congestion signal arrive since the last viewer window?
    /// The ramp doubles on a clean window and ENDS on a dirty one, so this is
    /// the one bit of state that decides which.
    slow_start_congested: bool,
    /// FR-63 B1 — the ONE delay-based controller, run in SHADOW. `Some` unless
    /// `ratectl = off`; fed at the live positions, read only by the `shadow_*`
    /// accessors below.
    ///
    /// ⚠️ **Nothing the live path computes may read this.** It is the FR's whole
    /// safety property for this phase: the controller observes and the heartbeat
    /// reports, and `tests::the_shadow_never_moves_the_live_decision` holds the
    /// line. B2 adds the one seam that lets it steer; B1 has none.
    shadow: Option<super::ratectl::Controller>,
    /// FR-148 P0 — how each path number was obtained (shadow; see [`PathTags`]).
    path: PathTags,
    /// FR-148 P0 — the last viewer window's R3 verdict: did it write under half
    /// the target it ran with? Read only to word an additive increase
    /// ([`Why::app_limited`]). `None` before the first window.
    window_app_limited: Option<bool>,
    /// FR-148 P0 — the applied moves since the last heartbeat, by [`Why`].
    moves: Moves,
}

/// FR-71 T1a — what the pump measured about its own send side over one
/// viewer window. The pump reports every PASS ([`RateGovernor::note_window_sender`]),
/// and the governor folds the passes into the window it reads at the
/// boundary ([`Self::absorb`]).
///
/// ⚠️ FR-119 P2b-8 — until then the governor KEPT ONLY THE LAST PASS: the
/// pump hands its facts over on every iteration, as deltas since the previous
/// one, and each call overwrote the one before. So the "window" the classifier
/// and the gate read was one pass long: a stalled pass anywhere but in the
/// window's last iteration never reached FR-79's `AgentStalled`, and
/// `frames_sent` / `gate_skips` read 0–2 where the window held 30–120.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSenderStats {
    /// Bytes still held for this session (the FR-59 P2 ledger) and the
    /// budget the gate compares it against.
    pub inflight_bytes: usize,
    pub budget_bytes: usize,
    /// FR-79 — pump passes that overran their stall budget in this window.
    /// Non-zero means the agent's own loop was not free, so nothing this
    /// window measured is evidence about the pipe. A pump that keeps no
    /// send-side figures reports `0` and the gate's other conditions stand.
    pub stalled_passes: u32,
    /// FR-119 P2b-8 — the transport is still in its opening (FR-35's opener
    /// window, kept for every session): what a blocked send measured here is
    /// the transport ramping, so the gate drops the window ([`super::evidence::Reason::Opening`]).
    pub opening: bool,
    /// Frames the byte-budget gate skipped this window.
    pub gate_skips: u32,
    /// The longest a frame waited in the send queue this window, ms, and
    /// the window's average (`None` where the pump keeps no such figure).
    pub send_wait_max_ms: f64,
    pub send_wait_avg_ms: Option<f64>,
    /// Frames the send task wrote this window.
    pub frames_sent: u32,
}

impl WindowSenderStats {
    /// FR-119 P2b-8 — fold one more pass into this window. Counts add, the
    /// wait's maximum is the larger, its average is weighted by the frames each
    /// pass sent (the send task records a wait for every frame it writes, so
    /// the weights are exact), `opening` is true if any pass was in the
    /// opening, and the levels (`inflight_bytes`, `budget_bytes`) are the
    /// latest pass's.
    pub fn absorb(&mut self, next: WindowSenderStats) {
        let (n0, n1) = (f64::from(self.frames_sent), f64::from(next.frames_sent));
        self.send_wait_avg_ms = match (self.send_wait_avg_ms, next.send_wait_avg_ms) {
            (Some(a), Some(b)) if n0 + n1 > 0.0 => Some((a * n0 + b * n1) / (n0 + n1)),
            (Some(a), _) => Some(a),
            (None, b) => b,
        };
        self.inflight_bytes = next.inflight_bytes;
        self.budget_bytes = next.budget_bytes;
        self.stalled_passes = self.stalled_passes.saturating_add(next.stalled_passes);
        self.opening |= next.opening;
        self.gate_skips = self.gate_skips.saturating_add(next.gate_skips);
        self.send_wait_max_ms = self.send_wait_max_ms.max(next.send_wait_max_ms);
        self.frames_sent = self.frames_sent.saturating_add(next.frames_sent);
    }
}
/// The viewer-rate fold cadence (unchanged from the pump bodies).
const VIEWER_WINDOW: Duration = Duration::from_secs(1);

/// FR-59 P3 — how long an arrival-rate ceiling outlives the window that
/// produced it. Generous enough to bridge a viewer that skips a report,
/// short enough that a viewer which goes away entirely cannot leave the
/// session clamped: silence is not evidence of congestion, and the loop
/// clears the clamp on the first non-congested window anyway.
const LINK_CEILING_TTL: Duration = Duration::from_secs(15);

/// FR-59 — queue depth (ms) at or below which the arrival-rate clamp is
/// released. Deliberately NOT zero: the estimate is a running integral of a
/// noisy derivative, so it rarely lands exactly on 0, and a release that
/// waits for perfection never fires. One `QUEUE_GROWTH_MS` worth of residue
/// is the same quantum the loop already treats as "not growing".
const LINK_RELEASE_DEPTH_MS: i32 = viewer_rate::QUEUE_GROWTH_MS as i32;

/// FR-148 P0 — a stage that changed a bound becomes its source: the last stage
/// to change it is the one whose number is in force.
fn retag<S>(source: &mut S, before: u32, after: u32, stage: S) {
    if after != before {
        *source = stage;
    }
}

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
        let plan = super::pair_memory::OpenPlan::Today(ceiling_seed_bps);
        Self::new_with(target_fps, send_depth, flags, ceiling_hi_bps, &plan, now)
    }

    /// FR-148 P1 step 3 — [`Self::new`] from what the session opens from:
    /// today's seed, or the path model's floor and opening. `new(…, s, …)` is
    /// `new_with(…, &OpenPlan::Today(s), …)`.
    pub fn new_with(
        target_fps: u32,
        send_depth: usize,
        flags: GovernorFlags,
        ceiling_hi_bps: u32,
        plan: &super::pair_memory::OpenPlan,
        now: Instant,
    ) -> Self {
        let (ceiling_seed_bps, open_at_bps) = plan.resolve(flags.floor_relief);
        // FR-59 P8 — the relief and the prior take the opening only under the
        // nominal floor with the relief on: at or above the band a seed never
        // stood in for anything (the relief is inert there and the P6 check
        // would read it as a contradiction).
        let open_seed_bps =
            open_at_bps.filter(|s| *s > 0 && *s < super::MIN_BITRATE_BPS && flags.floor_relief);
        let mut governor = Self {
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
            last_viewer_arrival: None,
            age_feedback: flags.age_feedback,
            pressure: EncodePressure::new(),
            encode_factor: 1.0,
            tier: DownscaleTier::new(),
            auto_res_cap: None,
            goodput: GoodputEstimator::new(),
            goodput_sink: GoodputSink::new(),
            pace: super::encode_pressure::FpsPace::new(),
            sw_pace: super::encode_pressure::SoftwarePace::new(),
            measured_ceiling: flags.measured_ceiling,
            learner: super::ceiling_learn::CeilingLearner::new(ceiling_hi_bps, ceiling_seed_bps)
                .with_carried_hold(flags.measured_evidence),
            last_decreases: 0,
            last_sent_bytes: 0,
            slow_link: flags,
            seed_abandoned_bps: None,
            relieved_floor_bps: None,
            link_loop: viewer_rate::LinkLoop::new(),
            offer_loop: viewer_rate::OfferLoop::default(),
            link_unoffered: 0,
            link_rx_bps: None,
            drain_until: None,
            open_seed_bps,
            open_at_bps,
            // Seeded with the SAME filtered value.
            prior: super::prior::RatePrior::new(
                open_seed_bps,
                super::MIN_BITRATE_BPS,
                // FR-79 V3 — the prior always decays; the switch is gone (FR-70 P1
                // was field-verified with its own same-build control on 0.4.64).
                true,
            ),
            stall_seen: false,
            pipe: super::pipe_state::PipeClassifier::new(),
            window_sender: None,
            evidence: super::evidence::Rejections::default(),
            belief: super::pipe::Pipe::default(),
            budget_path: None,
            sample_take: None,
            readings_refuted: 0,
            last_reading_bps: None,
            window_hard_stall: false,
            slow_start: None,
            slow_start_logged: false,
            slow_start_congested: false,
            // FR-63 B1 — the shadow controller, seeded with the same
            // remembered-slow rate the live opener uses (FR-59 P8). `None` when
            // the kill switch is off. It opens lazily at its first domain, like
            // the AIMD.
            shadow: match flags.ratectl {
                super::RatectlMode::Off => None,
                super::RatectlMode::Shadow => {
                    Some(super::ratectl::Controller::new(ceiling_seed_bps))
                }
            },
            path: PathTags::open(now, ceiling_seed_bps),
            window_app_limited: None,
            moves: Moves::default(),
        };
        // FR-148 P0 — the learned ceiling as the session opens, at the seed or at
        // its nominal start; and the seed, a rate an earlier session measured,
        // is an application-limited reading to the shadow model.
        governor.path.learned = governor
            .path
            .learned_at_open(governor.learner.learned_bps());
        if let Some(seed_bps) = governor.path.seed_bps {
            governor.belief.remember(seed_bps, now);
        }
        governor
    }

    /// FR-59 P8 — the remembered rate this session opened at, when the pair
    /// is remembered slower than the nominal floor (see `open_seed_bps`).
    /// The pump opens its FIRST encoder at this rate so the governor's
    /// first tick does not rebuild an encoder it just opened at the ceiling.
    #[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
    pub fn open_seed_bps(&self) -> Option<u32> {
        self.open_seed_bps
    }

    /// FR-148 P1 step 3 — where the session opens when that sits under the
    /// ceiling (see `open_at_bps`). The pump opens its first encoder, and a
    /// replacement before the first tick, at it. Today's rule gives
    /// [`Self::open_seed_bps`].
    #[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
    pub fn open_at_bps(&self) -> Option<u32> {
        self.open_at_bps
    }

    /// FR-148 P1 step 3 — the rate a session built from `plan` opens at under
    /// a plan ceiling of `plan_ceiling_bps`, with this governor's relay cap
    /// and floor relief: the AIMD's first target before the floor in force
    /// clamps it. The open line logs today's rule's beside the model's.
    #[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
    pub fn opening_of(&self, plan: &super::pair_memory::OpenPlan, plan_ceiling_bps: u32) -> u32 {
        let (seed, open_at) = plan.resolve(self.slow_link.floor_relief);
        let ceiling = super::ceiling_learn::CeilingLearner::new(self.learner.hi_bps(), seed)
            .ceiling_for(plan_ceiling_bps);
        open_at.map_or(ceiling, |o| o.min(ceiling))
    }

    /// FR-59 P4 — is production paused for a queue drain right now?
    ///
    /// Polled by the pump every iteration. Clearing the deadline here
    /// (rather than on a timer) is what tells the link loop the drain was
    /// SERVED, so the next deep window can order another one — without
    /// that hand-back a single drain would suppress every later one.
    pub fn draining(&mut self, now: Instant) -> bool {
        match self.drain_until {
            Some(until) if now < until => true,
            Some(_) => {
                self.drain_until = None;
                self.link_loop.drain_served();
                false
            }
            None => false,
        }
    }

    /// FR-59 P3/P4 — link-loop telemetry: congested windows, drains
    /// ordered, and the live queue-depth estimate (ms).
    pub fn link_stats(&self) -> (u32, u32, i32) {
        let (drains, depth) = self.link_loop.drain_stats();
        (self.link_loop.congested_windows(), drains, depth)
    }

    /// FR-79 V3b — the ONE belief, in SHADOW: `(believed, floor, capacity)`.
    ///
    /// Read it against `pipe_bps` in the heartbeat. Where they agree the new
    /// composition is safe to adopt; where `believed` has a number and
    /// `pipe_bps` is `None`, that is a session the old rule was blind on —
    /// which on the relay hosts is most of them.
    pub fn belief_bps(&self, now: Instant) -> (Option<u32>, Option<u32>, Option<u32>) {
        (
            self.belief.ceiling_anchor_bps(now),
            self.belief.floor_bps(now),
            self.belief.capacity_bps(now),
        )
    }

    /// FR-79 V3b — `(deliveries folded, capacities folded)`.
    pub fn belief_counts(&self) -> (u32, u32) {
        self.belief.counts()
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

    /// FR-70 M3a.2 — what [`Self::effective_ceiling`] would return, without
    /// recording anything: the per-frame call keeps the learner's nominal in
    /// step with the frame actually encoded, so the ceiling for a dims
    /// replacement's dims is asked here.
    pub fn ceiling_for(&self, plan_ceiling_bps: u32, constrained: bool) -> u32 {
        if constrained {
            self.learner.ceiling_for(plan_ceiling_bps)
        } else {
            plan_ceiling_bps
        }
    }

    /// FR-35 — the learned ceiling (0 = nothing learned or seeded), for the
    /// heartbeat.
    pub fn learned_ceiling_bps(&self) -> u32 {
        self.learner.learned_bps()
    }

    /// FR-148 P1 — the pair memory's side of this session, which the pump's
    /// guard reads at each viewer window. Nothing here drives a rate.
    pub fn memory(&self) -> &super::pair_memory::SessionMemory {
        self.belief.memory()
    }

    /// FR-148 P1 — open the pair memory's side from the pair's entry, right
    /// after the governor is built. Nothing the session acts on moves.
    pub fn open_memory(
        &mut self,
        entry: Option<&super::pair_memory::PairEntry>,
        now_unix: u64,
        now: Instant,
    ) {
        self.belief.open_memory(entry, now_unix, now);
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
    pub fn blocked_send_bps(&self, now: Instant) -> Option<u32> {
        self.goodput.estimate_bps(now)
    }

    /// FR-59 — what this session believes the PIPE carries: the agent's own
    /// blocked-send goodput, the viewer's arrival rate while its queue is
    /// growing, or the lower of the two. Constrained transports only.
    ///
    /// ⚠ Use this, not [`Self::blocked_send_bps`], for anything that
    /// asks "how fast is the link". Field 2026-09-02, a 150 kbit shaped
    /// link: `goodput_bps` was `None` in **every** window of a 47-window
    /// run, because the agent's sends never blocked — the queue was
    /// downstream. P2's queue budget read the goodput-only accessor and so
    /// could never engage (`frames_skipped_backpressure=0` throughout),
    /// while P1's floor relief, which reads the widened value, engaged and
    /// drove the target to 200 kbps. Same missing widening, one call site
    /// updated and the other not.
    pub fn pipe_bps(&self, now: Instant, constrained: bool) -> Option<u32> {
        if !constrained {
            return None;
        }
        // FR-59 P8 — with no live evidence at all, the remembered rate
        // (lowest priority: any measurement outranks it). FR-70 P1: the
        // rate as it DECAYS, so the queue budget denominated in it cannot
        // seal the session against the measurement that would replace it.
        self.measured_bps(now, constrained)
            .or(self.prior.stand_in_bps())
    }

    /// FR-79 V5b — [`Self::pipe_bps`] without the stand-in: what this session
    /// has actually MEASURED, right now. `None` = no claim to contradict.
    ///
    /// ⚠️ This, not `pipe_bps`, is what a stall is judged against. The stand-in
    /// is a number nobody measured, and a cut made against it used to lower it
    /// for the next stall.
    pub fn measured_bps(&self, now: Instant, constrained: bool) -> Option<u32> {
        if !constrained {
            return None;
        }
        let link = match self.link_rx_bps {
            Some((rx, at)) if now.duration_since(at) <= LINK_CEILING_TTL => Some(rx),
            _ => None,
        };
        // FR-148 D51 B4b-2 — under `on`, on a relay session, the budget's held
        // rate stands where the blocked-send goodput stood.
        match (self.sent_bps(now), link) {
            (Some(g), Some(rx)) => Some(g.min(rx)),
            (g, rx) => g.or(rx),
        }
    }

    /// What this session has to say to the pair memory right now, `None` for
    /// nothing. The pump reads it once per viewer window, and the last answer is
    /// what [`rate_memory::RateMemory::record_session`] gets when the session
    /// ends, whichever way it ends.
    ///
    /// FR-35 P3b — the memory wants the rate the pipe actually CARRIED, not just
    /// the learner's above-nominal ceiling. On a short or frozen session the
    /// learner never rises, so `stable_bps()` is `None`. FR-70 P1 — then what the
    /// session actually KNOWS about the pipe: a live measurement, or the prior as
    /// it has decayed. FR-79 V4 — evidence only: the applied rate used to stand
    /// in when nothing measured, which is the memory learning from itself.
    /// FR-79 V5b — `stable_bps()` is a hold a valid window carried. A hold nothing
    /// carried used to write the session's own opener back here.
    ///
    /// ⚠️ FR-79 V5b (a), reversing FR-70 P1 — with the rules on, the stand-in is
    /// not evidence: what a session writes after its hold is a reading, live or
    /// the last it took, and a session with neither writes nothing, so a key's
    /// timestamp is not refreshed. P1 wrote the decayed prior on purpose, so that
    /// a session measuring nothing could free a pair misremembered slow. That way
    /// out barely worked (V4 moves an entry a tenth of the gap upward, and past the
    /// band a stand-in is `None`), and on the way down it held a key low by itself.
    /// Field 2026-11-02, jupiter: a false reading of 9,489 walked to 3,322, was
    /// written, and each session opened from it wrote its own stand-in back with a
    /// fresh TTL: 3,322 → 2,989 → 2,690, with no rise in five writes.
    ///
    /// [`rate_memory::RateMemory::record_session`]: super::rate_memory::RateMemory::record_session
    #[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
    pub fn session_evidence_bps(&self, now: Instant, constrained: bool) -> Option<u32> {
        if !self.slow_link.measured_evidence {
            return self
                .stable_bps()
                .or_else(|| self.pipe_bps(now, constrained));
        }
        self.stable_bps()
            .or_else(|| self.measured_bps(now, constrained))
            .or(self.last_reading_bps.filter(|_| constrained))
    }

    /// FR-79 V5b — a valid window delivered `delivered_bps`, and that raised the
    /// floor: it came after whatever set the floor before it. Any reading it beats
    /// by [`CONTRADICTION_FACTOR`] is wrong, because the bytes arrived, so it
    /// stops counting: the goodput estimate is forgotten, the arrival-rate clamp
    /// lifts, and a stand-in loses its base. Returns the delivery if anything
    /// fell, for the prior to re-anchor on.
    ///
    /// ⚠️ The floor rising is what makes this safe, and comparing a reading
    /// against the floor is not the same rule. Field 2026-10-30 on one laptop:
    /// the reading of 180,484 at 12:19:52 was true (paint age 6.1 s, then 26 more
    /// stalls at 200–253 kbps), and it sat at 0.23 of the floor. The reading of
    /// 709,460 at 12:04:06 was false (4,657,230 delivered two seconds later), and
    /// it sat at 0.39. A week of two laptops (13,575 heartbeats, 5 readings)
    /// sorts correctly by the rise: both false readings fell within 2 s, and the
    /// floor did not move for the true one's 60 s.
    ///
    /// ⚠️ It goes through nothing but the viewer's report on a VALID window. A
    /// stall's shadow is its backlog draining, and FR-79's own rule is that no
    /// consumer acts on a rejected window.
    ///
    /// [`CONTRADICTION_FACTOR`]: super::ceiling_learn::CONTRADICTION_FACTOR
    fn refute_readings(&mut self, delivered_bps: u32, now: Instant) -> Option<u32> {
        let refutes = |reading: u32| {
            u64::from(delivered_bps)
                >= u64::from(reading) * u64::from(super::ceiling_learn::CONTRADICTION_FACTOR)
        };
        // Whether or not it has expired: the write-back would still use it.
        self.last_reading_bps = self.last_reading_bps.filter(|r| !refutes(*r));
        let goodput = self.goodput.estimate_bps(now).filter(|g| refutes(*g));
        if goodput.is_some() {
            self.goodput.forget();
        }
        let link = match self.link_rx_bps {
            Some((rx, at)) if now.duration_since(at) <= LINK_CEILING_TTL => Some(rx),
            _ => None,
        }
        .filter(|rx| refutes(*rx));
        if link.is_some() {
            self.link_rx_bps = None;
        }
        let stand_in = self.prior.stand_in_bps().filter(|s| refutes(*s));
        if goodput.is_none() && link.is_none() && stand_in.is_none() {
            return None;
        }
        self.readings_refuted = self.readings_refuted.saturating_add(1);
        tracing::info!(
            delivered_bps,
            goodput_bps = ?goodput,
            arrival_bps = ?link,
            stand_in_bps = ?stand_in,
            "FR-79 V5b: the path delivered at least twice what stood below it — \
             those readings no longer count"
        );
        Some(delivered_bps)
    }

    /// FR-79 V5b — how many times a delivery took out what stood below it.
    pub fn readings_refuted(&self) -> u32 {
        self.readings_refuted
    }

    /// FR-79 V5b — valid congested windows the sender was not feeding.
    pub fn link_unoffered(&self) -> u32 {
        self.link_unoffered
    }

    /// FR-59 P3 — is the viewer's arrival-rate clamp currently HELD (a live
    /// `link_rx_bps` within its TTL)? Test seam: with FR-70 P1 the floor no
    /// longer snaps to the constant on release, so "released" has to be
    /// read from the clamp itself rather than inferred from the floor.
    #[cfg(test)]
    pub fn link_loop_holding(&self) -> bool {
        self.link_rx_bps.is_some()
    }

    /// FR-70 P1 — what stands in for a pipe measurement right now, for the
    /// heartbeat: the remembered seed, or the last live measurement, as it
    /// decays toward the band on clean windows. `None` = no prior in force
    /// (an unremembered pair, or one whose prior has decayed away). Read it
    /// next to `slow_link_floor_bps` and `goodput_bps`: a relief that is
    /// letting go while `goodput_bps` stays `None` is this, working.
    pub fn prior_bps(&self) -> Option<u32> {
        self.prior.stand_in_bps()
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
        // FR-63 — congestion for the opener's ramp, recorded BEFORE the `?`
        // below so it is not lost when the AIMD has not been built yet. The
        // first frames of a session are exactly when that happens.
        self.slow_start_congested = true;
        // FR-63 B1 — the SHADOW hears the same budget-gate skip (a congestion
        // sample, not a blocked send), also before the `?`.
        if let Some(s) = self.shadow.as_mut() {
            s.on_budget_skip(now);
        }
        let ctrl = self.aimd.as_mut()?;
        ctrl.observe(self.send_depth as u32, true, now);
        let moved = ctrl.take_move()?;
        Some(self.record_applied(moved))
    }

    /// FR-148 D51 B4a-1 — the capacity gate's skip, as the delivery sampler
    /// classed it ([`GateSkip`]): a full send channel or a congested follower
    /// is today's [`Self::on_backpressure_skip`]; the in-flight budget's own
    /// hold under `on` is [`Self::on_budget_hold`], no AIMD event.
    pub fn on_gate_skip(&mut self, skip: GateSkip) -> Option<AppliedBitrate> {
        match skip {
            GateSkip::Backpressure(now) => self.on_backpressure_skip(now),
            GateSkip::BudgetHold(now) => {
                self.on_budget_hold(now);
                None
            }
        }
    }

    /// FR-148 D51 B4a-1 — the in-flight budget held a pass by itself, under
    /// `on`, outside a still span. The hold keeps its capacity facts: the link
    /// is full, which ends the opener's ramp (FR-63), and FR-63's shadow hears
    /// the same budget-gate skip. It is no AIMD event: no full-occupancy sample,
    /// so neither the ×0.85 nor the settle window that blocks the additive
    /// increase. The budget owns the queue, and its rate bounds the target
    /// ([`super::pipe::PathRate`], B4b).
    pub fn on_budget_hold(&mut self, now: Instant) {
        self.slow_start_congested = true;
        if let Some(s) = self.shadow.as_mut() {
            s.on_budget_skip(now);
        }
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
        //
        // FR-148 P0 — each stage below that changes the ceiling becomes its
        // source, and so does each floor stage: the line that reports a move
        // the bound made names the stage whose number was in force.
        let (plan_bps, mut ceiling_src) = (ceiling_bps, CeilingSource::Plan);
        let ceiling_bps = if self.measured_ceiling
            && !constrained
            && let Some(g) = self.goodput.estimate_bps(now)
        {
            ceiling_bps.min(super::goodput::derived_ceiling_bps(g))
        } else {
            ceiling_bps
        };
        retag(&mut ceiling_src, plan_bps, ceiling_bps, CeilingSource::Pipe);
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
        // FR-79 V3 — ONE belief, read through the one accessor. This block used
        // to compose it a third time, beside `pipe_bps` and the identical
        // `remembered_candidate_bps`; three copies of one law is how they drift.
        let measured_bps = self.pipe_bps(now, constrained);
        // FR-59 P6 — a measurement far under the learned/seeded ceiling
        // contradicts it. Must run BEFORE `effective_ceiling`, or this
        // frame is still planned at the ceiling the evidence just refuted.
        if let Some(g) = measured_bps
            && let Some(dropped) = self
                .learner
                .on_measurement(g, self.slow_link.seed_contradiction)
        {
            self.seed_abandoned_bps = Some(dropped);
            self.path.learned_moved(self.learner.learned_bps(), now);
        }
        // FR-35 — lift the constrained ceiling by what the learner has proven.
        let ceiling_bps = if constrained {
            self.learner.effective_ceiling(ceiling_bps)
        } else {
            ceiling_bps
        };
        // FR-148 P0 — `learned` whenever the learner's lift is the number in
        // force, so the FFmpeg pump's own lift of its plan reads `learned` too.
        if constrained && ceiling_bps > 0 && ceiling_bps == self.learner.ceiling_for(0) {
            ceiling_src = CeilingSource::Learned;
        }
        // FR-59 P3 — and then bound it by what the VIEWER says is actually
        // arriving, while its transit queue is growing. Applied AFTER the
        // learner deliberately: the learner's evidence is what the pipe
        // carried in the past, and a live report that the receiver is
        // falling behind outranks it.
        let lifted_bps = ceiling_bps;
        let ceiling_bps = match link_rx_bps {
            Some(rx) if constrained => ceiling_bps
                .min(((rx as u64) * viewer_rate::LINK_CEILING_PCT / 100) as u32)
                .max(1),
            _ => ceiling_bps,
        };
        retag(
            &mut ceiling_src,
            lifted_bps,
            ceiling_bps,
            CeilingSource::Arrival,
        );
        // FR-148 D51 B4b — under `on`, on a relay session, the budget owns the
        // queue, and the rate it measured while it held a frame in the last
        // second outside a still span is the rate the path carries: the target
        // stays under it (gain 1.0). Off, shadow, a direct session and a budget
        // that has not held lately leave the chain as it was.
        let arrived_bps = ceiling_bps;
        let ceiling_bps = match self.budget_held_bps() {
            Some(rate) if constrained => ceiling_bps.min(rate),
            _ => ceiling_bps,
        };
        retag(
            &mut ceiling_src,
            arrived_bps,
            ceiling_bps,
            CeilingSource::Budget,
        );
        // FR-63 — the opener has no evidence yet, so it must not commit to
        // whatever constant the chain above produced. Build the ramp from THIS
        // tick's RESOLVED ceiling (the AIMD below is built the same way) and
        // cap the ceiling by it until the ramp finishes.
        //
        // Field 2026-09-03: this ceiling was 2_550_000 on a path measured at
        // 213_180 — 12× over, 444 ms of queue, a 1550 ms paint, then six
        // windows collapsing to the truth. The same host over-drove from the
        // OPPOSITE direction a day earlier with a remembered 6_134_627 and a
        // 6287 ms paint. No constant was safe; only measuring is.
        //
        // ⚠️ Constructed once and kept once `done` — dropping it would let
        // `get_or_insert_with` restart the ramp on the next tick and re-open a
        // session that had already earned its rate.
        //
        // ⚠️ The floor passed here is `open_seed_bps` — the rate THIS PAIR was
        // remembered at — and NOT `floor_bps`. `floor_bps` is
        // `area_min_bitrate_bps`, a flat 1.5 M on every constrained transport:
        // a fleet constant, which is precisely the thing this phase exists to
        // stop trusting. Passing it made `SlowStart::new` resolve
        // `300_000.max(1_500_000)` and open at 1.5 M — still 7× over the
        // 213 kbps field path, and one doubling from the ceiling, i.e. no ramp
        // at all. The floor parameter means "a rate already PROVEN", and only
        // the remembered seed is that.
        if self.slow_link.slow_start && constrained {
            // FR-148 P1 step 3 — the opening, which is the P8 seed under
            // today's rule and the model's opening after a push-back.
            let seeded = self.open_at_bps;
            let ss = self.slow_start.get_or_insert_with(|| {
                super::slow_start::SlowStart::new(seeded.unwrap_or(0), ceiling_bps)
            });
            if !self.slow_start_logged {
                self.slow_start_logged = true;
                tracing::info!(
                    open_bps = ss.target_bps(),
                    ceiling_bps,
                    seed_bps = seeded,
                    "FR-63 slow-start armed — opening below the ceiling and \
                     doubling per clean window"
                );
            }
        }
        let clamped_bps = ceiling_bps;
        let ceiling_bps = match self.slow_start.as_ref() {
            Some(ss) if !ss.done() => ceiling_bps.min(ss.target_bps()).max(1),
            _ => ceiling_bps,
        };
        retag(
            &mut ceiling_src,
            clamped_bps,
            ceiling_bps,
            CeilingSource::Opener,
        );
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
        let mut floor_src = FloorSource::Area;
        let floor_bps = match measured_bps {
            Some(g) if self.slow_link.floor_relief => {
                let relieved = super::goodput::measured_floor_bps(
                    g,
                    floor_bps,
                    super::slow_link_min_bitrate_bps(),
                );
                self.relieved_floor_bps = (relieved < floor_bps).then_some(relieved);
                retag(&mut floor_src, floor_bps, relieved, FloorSource::Relief);
                // FR-148 P0 — tagged as the reading or stand-in it came from.
                if let Some((_, tag)) = self.pipe_tagged(now, constrained) {
                    self.path.relieved = tag;
                }
                relieved
            }
            _ => {
                self.relieved_floor_bps = None;
                floor_bps
            }
        };
        // FR-63 — and the floor has to descend WITH the ramp, or the ceiling
        // cap above is silently undone: `set_ceiling` raises any ceiling back
        // up to the floor. That is the same coupling the P1 comment above
        // documents for the arrival clamp, and it bit this phase exactly as
        // predicted — 0.4.55 shipped the cap alone, so on a constrained path
        // the flat 1.5 M floor pinned the opener and slow-start was INERT
        // while looking wired.
        //
        // Bounded by `slow_link_min_bitrate` — the existing absolute stop, not
        // a new constant. Below roughly it a full-resolution frame is
        // illegible at any QP and the honest lever is fewer PIXELS, which the
        // relay resolution cap already applies. Only ever LOWERS the floor.
        let floor_bps = match self.slow_start.as_ref() {
            Some(ss) if !ss.done() => {
                let ramped = floor_bps.min(ss.target_bps().max(super::slow_link_min_bitrate_bps()));
                if ramped < floor_bps {
                    // The heartbeat's contract is "the floor actually in
                    // force"; leaving it None here would report that the
                    // nominal floor stands when it does not.
                    self.relieved_floor_bps = Some(ramped);
                    // FR-148 P0 — the opener's ramp is computed: assumed, as old
                    // as the session.
                    self.path.relieved = Tag::assumed(Origin::At(self.path.opened_at));
                    floor_src = FloorSource::Ramp;
                }
                ramped
            }
            _ => floor_bps,
        };
        let depth = self.send_depth;
        let open_at_bps = self.open_at_bps;
        // FR-63 B1 — feed the SHADOW the same resolved domain and occupancy the
        // live AIMD below gets, and let it open on its first frame. Placed where
        // no `&mut self.aimd` borrow is live and before the `?` the AIMD takes,
        // so the shadow sees every frame. It returns nothing; its only outlet is
        // the `shadow_*` heartbeat accessors.
        if let Some(s) = self.shadow.as_mut() {
            s.mark_opened(now);
            s.set_domain(floor_bps, ceiling_bps, constrained);
            s.observe_frame(send_capacity == 0, now);
        }
        let ctrl = self.aimd.get_or_insert_with(|| {
            // FR-59 P1 — construct at the floor already in force rather
            // than the flat `MIN_BITRATE_BPS`, so the constructor's own
            // `initial.clamp(floor, ceiling)` and the `set_floor` on the
            // next line cannot momentarily disagree.
            // FR-59 P8 — and OPEN at the remembered rate when the pair is
            // remembered slower than the nominal floor: the ceiling is the
            // relay cap, and an opener sized by it is the burst this program
            // exists to prevent. The additive increase climbs from here.
            // FR-148 P1 step 3 — or at the path model's opening after a
            // trusted push-back (`open_at_bps`).
            let initial = open_at_bps.map_or(ceiling_bps, |s| s.min(ceiling_bps));
            AimdController::new(initial, floor_bps, ceiling_bps, depth as u32, now)
        });
        ctrl.set_floor_from(floor_bps, floor_src);
        ctrl.set_ceiling_from(ceiling_bps, ceiling_src);
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
                self.path.learned_moved(self.learner.learned_bps(), now);
            }
        }
        let moved = ctrl.take_move()?;
        Some(self.record_applied(moved))
    }

    /// try_send-Full arm: an encoded frame overflowed the send channel
    /// (a big IDR / motion frame the link can't drain) — a secondary
    /// congestion signal (rate-limited MD internally).
    pub fn on_send_overflow(&mut self, now: Instant) -> Option<AppliedBitrate> {
        // FR-63 — congestion for the opener's ramp; see `on_backpressure_skip`
        // for why this precedes the `?`.
        self.slow_start_congested = true;
        // FR-63 B1 — the SHADOW hears the same overflow, before the `?`.
        if let Some(s) = self.shadow.as_mut() {
            s.on_overflow(now);
        }
        let ctrl = self.aimd.as_mut()?;
        ctrl.note_buffer_overflow(now);
        let moved = ctrl.take_move()?;
        Some(self.record_applied(moved))
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
    /// Deliberately NOT `on_send_overflow`: that one calls `take_move`,
    /// which marks the move applied while the encoder is never told. The
    /// pump's own `pre_encode_tick` picks this up one frame later and
    /// routes it through the normal apply arms — the same discipline the
    /// FR-15 age loop follows.
    pub fn note_send_stall(&mut self, wait: Duration, now: Instant) {
        // FR-63 — a blocked send is congestion for the opener's ramp.
        self.slow_start_congested = true;
        // FR-63 B1 — the SHADOW hears the same stall: a hard one (≥ 1 s) halves
        // it, a softer one is a rate-limited decrease.
        if let Some(s) = self.shadow.as_mut() {
            s.on_stall(wait, now);
        }
        // FR-70 P1 — and the pipe pushing back, for the prior's window verdict.
        self.stall_seen = true;
        // FR-35 — a send blocked ≥ 1 s is a HARD stall and earns ×0.5 rather
        // than the ×0.85 ladder. FR-79 — it is APPLIED at the window boundary,
        // because only there is it known whether the block was the pipe or a
        // pass the runtime stalled underneath it. The learner still notes the
        // stall here: it vetoes growth for a while either way.
        let hard = self.learner.on_stall(wait, now);
        if hard {
            self.window_hard_stall = true;
        } else if let Some(ctrl) = self.aimd.as_mut() {
            ctrl.note_congestion(Congestion::Stall, now);
        }
    }

    /// A fresh encoder starts at its constructor's rate, not the session's:
    /// FFmpeg's at its full-ceiling maxrate, libvpx's at its 8 Mbps boot
    /// target (`DEFAULT_BITRATE_BPS`). Force the AIMD to re-apply its
    /// current (possibly lower) target on the next tick, so the stream
    /// doesn't snap back up after a dim change, a capture reopen or a
    /// keyframe-fallback rebuild. Before the first tick there is no AIMD
    /// yet and the call does nothing: that tick builds it and applies the
    /// opening target anyway. The mirror keeps the last value applied, so
    /// the heartbeat's `target_bps` does not read 0 across a rebuild.
    ///
    /// ⚠ Both data-channel pumps call this after every build (#3472). The
    /// VP9-444 pump called a hook that zeroed the mirror only, so nothing
    /// told a fresh libvpx encoder its target until the AIMD next moved:
    /// a relay session at its 3 Mbps ceiling ran 6 min 18 s at 8.8 Mbps
    /// with `target_bps=0` in every heartbeat (2026-11-04). Pinned by
    /// `a_vp9_rebuild_hands_the_fresh_encoder_its_target`.
    pub fn on_encoder_rebuilt(&mut self) {
        if let Some(ctrl) = self.aimd.as_mut() {
            ctrl.force_reapply();
        }
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
        // FR-148 P0 — the transport the pump's heartbeat reads its numbers on,
        // taken on every call so the `path model` line reads the same one.
        self.path.constrained = constrained;
        let elapsed = now.duration_since(self.viewer_window_at);
        if elapsed < VIEWER_WINDOW {
            return None;
        }
        self.viewer_window_at = now;
        // FR-79 — the PREVIOUS window's verdict, read before this one is
        // classified: it is one of the four facts the validity gate reads (see
        // `encode::evidence`).
        let prev_state = self.pipe.last();
        // FR-63 — one verdict per window for the opener's ramp: double if
        // nothing congested since the last one, otherwise END it. Taking the
        // flag first keeps the borrow of `self.slow_start` clean.
        let congested = std::mem::take(&mut self.slow_start_congested);
        // (Applied below, once the window's plane verdict is in: FR-71 T1b
        // holds the ramp on a stalled window, which is neither clean nor
        // congested.)
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
        let raw_report = take_report();
        let (reported_fps, struggling) = viewer_rate::unpack_report(raw_report);
        // FR-15 — the viewer's paint age is the only sensor that sees the
        // whole constrained path (WG-over-DERP/TCP queues sit below every
        // agent counter). Sustained age over the learned floor drives BOTH
        // existing responses: the fps cap (folded into `struggling` — an
        // instant, re-open-free byte cut) and a rate-limited MD (returned
        // as `age_md` so the pump applies it under its normal — on thrifty
        // relay, FR-10-deferred — rules). Direct transports keep their own
        // machinery; the loop still LEARNS there so the heartbeat can show
        // ages on every transport.
        let raw_age = take_age();
        let report = viewer_rate::unpack_age(raw_age);
        // FR-70 M0 — the same window's age at ARRIVAL, when the viewer sends
        // it; the heartbeat splits the fused age with it. Stored beside the
        // report (absent ⇒ absent) — it is telemetry, no loop reads it.
        self.last_viewer_arrival = viewer_rate::unpack_age_arrival(raw_age);
        // FR-59 P3 — the viewer's own view of the link, taken here so the
        // verdict below can see whether the viewer said anything at all.
        let link = viewer_rate::unpack_link(take_link());
        // The window's blocked-send samples. Drained here rather than per
        // frame — the send task is a different task, and this is the existing
        // once-a-second rendezvous. Whether they are FOLDED is the gate's call,
        // taken below once the window has a verdict.
        let samples = self.goodput_sink.drain();
        let blocked_sends_n = samples.len() as u32;
        // FR-71 T1a — one verdict per window on which plane is the limiter,
        // taken BEFORE any loop acts on it, and with it FR-79's validity gate:
        // a window is evidence about the pipe only if the agent's own loop was
        // free, the transport did not stall, the window before it did not
        // either, and the carrier did not move. EVERY consumer below reads this
        // one verdict; none carries a rule of its own any more (V1 deleted
        // three that did: T1b's hold, T2's quarantine, T2b's shadow).
        //
        // The classifier runs on every constrained window — it stopped being a
        // shadow the moment the gate was built on it, so `transit_classify`
        // went with them. `samples` is this window's blocked sends; the
        // sender's other figures arrive from the pump via `note_window_sender`.
        //
        // FR-119 P2b-8 — the GATE runs on every window, direct ones included;
        // only the classifier is constrained-only. A direct window used to be
        // evidence by definition, and the capacity it folded is what FR-119's
        // grant reads: a session's opening window, blocked by its own first
        // keyframe, took a granted 120 fps back for 60 s. On a direct window
        // the state is `Unknown` and there is no predecessor (a stale
        // constrained verdict from before a transport flip must not cast a
        // shadow here), so only the agent's own stall and the opening apply.
        let sender = self.window_sender.take();
        let (state, prev_state) = if constrained {
            let split = match (report, self.last_viewer_arrival) {
                (Some((avg, _, _)), Some(arrival)) => {
                    let s = viewer_rate::split_age(
                        avg,
                        arrival,
                        sender.and_then(|s| s.send_wait_avg_ms),
                    );
                    Some(super::pipe_state::SplitMs {
                        sender_ms: s.sender_ms,
                        transit_ms: s.transit_ms,
                        viewer_ms: s.viewer_ms,
                    })
                }
                _ => None,
            };
            let signals = super::pipe_state::WindowSignals {
                split,
                reported: raw_report != 0 || report.is_some() || link.is_some(),
                inflight_bytes: sender.map_or(0, |s| s.inflight_bytes),
                budget_bytes: sender.map_or(0, |s| s.budget_bytes),
                gate_skips: sender.map_or(0, |s| s.gate_skips),
                blocked_sends: blocked_sends_n,
                send_wait_max_ms: sender.map_or(0.0, |s| s.send_wait_max_ms),
                frames_sent: sender.map_or(0, |s| s.frames_sent),
                struggling,
            };
            (self.pipe.classify(&signals), prev_state)
        } else {
            (super::pipe_state::PipeState::Unknown, None)
        };
        // FR-79 V3b — the viewer's arrival rate is a DEMONSTRATED delivery:
        // those bytes reached the far end, whatever the queue was doing. It
        // arrives every window on every carrier, and until now it was thrown
        // away unless the link loop called the window congested — which is why
        // a relay-TCP session could finish knowing nothing about its own path
        // (CORPLAP-3, `goodput_samples=(0, 5)`). It is a FLOOR, never a
        // capacity: an idle desktop delivers 300 kbps through a gigabit path.
        // V5b reads whether it rose.
        //
        // FR-148 P0 — taken here, once the window has its verdict, because the
        // delivery's class needs it: backlogged when the sender offered more than
        // arrived (the per-window test the offer loop takes below), else
        // application-limited. Nothing above this point reads the belief.
        let delivered = link.map(|(rx_bps, _)| {
            let backlogged = state == super::pipe_state::PipeState::Overproduced
                || viewer_rate::offered_over_arrival(sent_bps, rx_bps);
            (rx_bps, Basis::of_window(backlogged))
        });
        let rejection = super::evidence::rejected(super::evidence::WindowFacts {
            state,
            prev_state,
            // The pump counts its own stalled passes; a pump that keeps no
            // send-side figures (VP9-444) reports none and the remaining
            // conditions still stand.
            agent_stalled: sender.is_some_and(|s| s.stalled_passes > 0),
            opening: sender.is_some_and(|s| s.opening),
            // FR-119 P2b-13 — direct only, like the opening began. P2b-14:
            // ANY blocked keyframe, not only a keyframe alone. The deltas
            // queued behind it block on the keyframe's own backlog, so they
            // measure the same ramp; a pipe narrower than the stream shows in
            // the keyframe-free windows either side, where deltas block alone.
            keyframe_burst: !constrained && samples.iter().any(|s| s.keyframe),
            // V2 wires the carrier; until then a change cannot be seen
            // here and the memory keying is where it will be read.
            carrier_changed: false,
        });
        if let Some(r) = rejection {
            self.evidence.note(r);
        }
        let valid = rejection.is_none();
        // FR-148 P1 (R1, R3) — did the classifier READ this window, and did the
        // sender push against the target it ran with? Its push-backs act in the
        // session either way; the pair memory counts one only when both hold
        // (`pair_memory::Take`). The age is a reading by the age loop's own test,
        // the half round trip it observes against below.
        let take = super::pair_memory::Take::of(
            state,
            report.is_some_and(|(_, min, rtt)| viewer_rate::age_is_plausible(min, rtt / 2)),
            sent_bps,
            self.applied_bps,
        );
        self.sample_take = valid.then_some(take);
        // FR-148 P0 — the same R3 test, kept whatever the gate said, for the line
        // that words an additive increase: the sender's own byte count against
        // the target the window ran with, so a still screen reads app-limited.
        self.window_app_limited = Some(!super::pair_memory::pushing_against_plan(
            state,
            sent_bps,
            self.applied_bps,
        ));
        // FR-148 P1 (D24) — the delivery is folded once the gate has ruled, which
        // reads only the window's facts: `Pipe` takes it whatever the verdict, the
        // shadow model and the pair memory only from a valid window.
        let floor_raised = delivered.is_some_and(|(rx_bps, basis)| {
            self.belief.observe_delivered_in(rx_bps, basis, valid, now)
        });
        if let Some((rx_bps, basis)) = delivered {
            self.path.arrival = Some((rx_bps, basis, now));
        }
        // FR-79 V5 — the gate says whether this window MEASURES the pipe. It
        // was also being read as whether the window says anything at all, and
        // for a stalled transport those differ: the stall is the pipe pushing
        // back, and on a relay-TCP session it is the only push-back there is.
        //
        // That alone does not make it OUR fault. FR-71's finding 4 is the
        // other case — an 8 Mbps leg head-of-line-blocked for 4.9 s with 1485
        // bytes queued — where a cut costs quality for an event the sender did
        // not cause. The estimator is the discriminator: a stall while we are
        // sending above the last MEASURED capacity is the rate's doing; at or
        // below it, it is the transport's and T1b's answer still stands. With
        // nothing measured yet there is no claim to contradict, so no cut.
        //
        // Field 2026-09-09, CORPLAP-2 AV1 over DERP/TCP: the goodput estimator
        // accepted ONE sample all session (1,058,145 bps) while the target
        // ramped to 1,928,555; eleven windows stalled, paint age reached
        // 7,784 ms, and the rate ended HIGHER than it started — because every
        // loop below that can lower it reads `valid`.
        // ⚠️ `pipe_bps`, NOT `blocked_send_bps` — the invariant that accessor's
        // own doc states, and V5 shipped on the wrong side of it for one
        // release. The agent's sends only block when the queue is LOCAL; on a
        // relay-TCP path the queue is downstream, so the blocked-send source is
        // silent exactly where this rule is needed. Measured on 0.4.96,
        // 2026-09-09: CORPLAP-3 read `goodput_bps=None` with
        // `goodput_samples=(0, 5)` — every window's blocked time under
        // `MIN_WINDOW_BLOCKED` — so a goodput-only comparison could never fire
        // and V5 was inert on that host. `pipe_bps` composes the same goodput
        // with the viewer's arrival rate and falls back to the decaying prior,
        // which is what "how fast is the link" has meant since FR-59 P3.
        // ⚠️ V5b — but a stall needs a MEASUREMENT to contradict, so it stops
        // short of that fallback. Field 2026-10-30 11:48–12:19, CORPLAP-2, one
        // session on `relay:derp/tcp`: stalls cut 13 times of 16 with a stand-in
        // standing and 0 of 26 with nothing, the target spending 3 min 12 s under
        // 1 M on a path that delivered 5.39 M. And 2026-10-20 14:16: a stand-in of
        // 19,578 under a 1.17 M target, so every stall read as ours.
        let stall_reference = if self.slow_link.measured_evidence {
            self.measured_bps(now, constrained)
        } else {
            self.pipe_bps(now, constrained)
        };
        let stall_is_ours = rejection.is_some_and(|r| r.is_congestion())
            && match (stall_reference, self.aimd.as_ref().map(|c| c.desired())) {
                // A fifth of slack so a session parked at the measurement does
                // not chatter in and out of cutting on rounding alone.
                (Some(pipe), Some(desired)) => desired > pipe.saturating_add(pipe / 5),
                _ => false,
            };
        // FR-79 — the fold is the gate's first consumer: an invalid window's
        // blocked sends are DROPPED, not held. What they measured was the
        // stall, and the next valid window measures the pipe on its own merits.
        if valid {
            // FR-79 V3b — a blocked send is the path RESISTING, so an accepted
            // fold is a capacity estimate, not a delivery. Only fold it into
            // the belief when the estimator actually accepted the window;
            // `observe_window` rejects anything under `MIN_WINDOW_BLOCKED`.
            // FR-148 P0 — so it is always backlogged: the estimator takes only
            // sends that blocked (`goodput::MIN_BLOCKED_SEND` each).
            if self.goodput.observe_window(&samples, now)
                && let Some(bps) = self.goodput.estimate_bps(now)
            {
                self.belief
                    .observe_capacity_in(bps, Basis::Backlogged, take, now);
            }
        }
        // FR-35 — a send blocked ≥ `HARD_STALL` is a hard stall: ×0.5, bypassing
        // the ×0.85 ladder. It applies here rather than in `note_send_stall`
        // because only the window boundary knows whether the block was the
        // PIPE (2026-09-08 12:15: 2.9 s and 7.4 s blocked sends inside stalled
        // pump passes, on a path that carried 6.6 Mbps twenty seconds later).
        let hard_stall = std::mem::take(&mut self.window_hard_stall);
        if valid
            && hard_stall
            && let Some(ctrl) = self.aimd.as_mut()
        {
            ctrl.apply_hard_md(now);
        }
        // FR-63 — one verdict per window for the opener's ramp: double if
        // nothing congested since the last one, otherwise END it. Neither on
        // a held window (FR-71 T1b): a stall is not a clean window, and it is
        // not congestion either.
        if let Some(ss) = self.slow_start.as_mut()
            && !ss.done()
        {
            if valid {
                if congested {
                    ss.on_congestion();
                } else {
                    ss.on_clean_window();
                }
            } else if stall_is_ours {
                // FR-79 V5 — the ramp must END on a stall it caused. Reading
                // only valid windows, it saw a run of clean ones (an idle
                // screen produces those: nothing is offered, so nothing
                // congests) and kept doubling into a pipe it had already
                // overrun.
                ss.on_congestion();
            }
        }
        let mut age_over = false;
        // FR-59 — is the queue still DEEP right now? This is the LEVEL, not
        // the streak `age_over` needs, and it is what holds the P3 clamp.
        //
        // ⚠ The clamp cannot be held by the queue-growth integral, which is
        // what the first two attempts tried. `Σ(Δarrival − Δwire)` is a
        // DERIVATIVE: it sees the queue grow and falls silent the moment
        // growth stops — and growth stops as soon as the sender's cadence
        // stretches to match the drained rate, which is exactly what the
        // viewer-rate fps cap makes happen. Field 2026-09-02, adjacent
        // windows: paint age 505 ms while the depth integral read 10. The
        // age is an absolute measure against a learned floor and does not
        // vanish when growth does.
        let mut age_elevated = false;
        // FR-148 D51 B4a-2 — under `on`, on a relay session, the age the
        // budget's own queue adds (its envelope) is the budget's to hold: the
        // age loop and the prior's push-back below act only past it. 0, as
        // before, when the budget does not act.
        let envelope_ms = self.budget_envelope_ms();
        if let Some((avg, min, rtt)) = report {
            // P2 — half the viewer's own measured round trip is the smallest
            // age this path can physically produce; the loop uses it both to
            // reject impossible floor samples and to catch a session that was
            // congested from its first window.
            let triggered = self.age_loop.observe_within(avg, min, rtt / 2, envelope_ms);
            // FR-71 T1b — the loop still LEARNS on a held window (a stall
            // never lowers a floor that is a minimum), it just does not fire.
            age_over = triggered && constrained && self.age_feedback && (valid || stall_is_ours);
            // FR-79 V5 — a stall we caused must not ERASE the streak that
            // would have cut for it. T1b's reset stands for every other
            // rejection, where the age is real but unattributable.
            if !valid && !stall_is_ours {
                self.age_loop.reset_streak();
            }
            // Same floor the age loop learned, falling back to the path's
            // physical minimum so a session with nothing learned yet still
            // has a bound rather than treating every age as elevated.
            let floor = self.age_loop.floor_ms().unwrap_or((rtt / 2).max(1));
            age_elevated =
                avg.saturating_sub(floor) >= viewer_rate::AGE_SLACK_MS.saturating_add(envelope_ms);
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
        let verdict = link
            .map(|(rx_bps, queue_ms)| self.link_loop.observe(rx_bps, queue_ms))
            .unwrap_or_default();
        // FR-79 V5b — a growing queue says nothing about the rate unless the
        // sender was feeding it (`viewer_rate::OfferLoop`, which says why the
        // classifier's word counts too). Asked of every report, whatever the
        // switch says, so the streak stays true to the windows.
        // FR-148 P0 — the streak's own verdict, kept apart from the switch: it
        // is the class of the arrival rate taken below.
        let offer_held = link.is_some_and(|(rx_bps, _)| {
            self.offer_loop.observe(
                state == super::pipe_state::PipeState::Overproduced
                    || viewer_rate::offered_over_arrival(sent_bps, rx_bps),
            )
        });
        let offered = offer_held || !self.slow_link.measured_evidence;
        // FR-79 — on an invalid window the clamp is neither armed nor released:
        // whatever it held before the stall, it still holds. The report that
        // lands in a stall's shadow is the stall's own arrival rate (field
        // 2026-09-08 14:52: 70 kbps while the queue grew, which set 834,800 on
        // a path that had just carried 7.45 M).
        let link_acts = constrained && self.slow_link.viewer_rate_clamp && valid;
        // FR-59 P4 — a queue too deep to cut our way out of. A rate cut
        // drains at `capacity − inflow`, which on a converged session is
        // nearly nothing: at 90 % of a 400 kbps pipe a 2 s backlog clears
        // at 40 kbps, i.e. over ~20 s. Stopping production sets inflow to
        // zero, so the same backlog clears in the ~2 s it represents.
        //
        // ⚠ Deliberately NO forced keyframe on resume: a pause loses no
        // frames, so the delta chain is intact, and an IDR at these rates
        // is itself seconds of transit — the cure would re-create the
        // disease. This is why P4 pauses rather than discarding the
        // agent-side queue (which is, on this path, 1–4 KB anyway: the
        // queue that matters is downstream and cannot be recalled).
        let drain_for_ms = verdict
            .drain_for_ms
            .filter(|_| constrained && self.slow_link.queue_drain);
        if let Some(ms) = drain_for_ms {
            self.drain_until = Some(now + Duration::from_millis(ms as u64));
        }
        if link_acts {
            match (verdict.congested, link) {
                (true, Some((rx_bps, _))) if offered => {
                    // FR-79 V3b — an arrival rate measured while the viewer's
                    // queue is GROWING is the path's limit, not merely a
                    // delivery: the sender was offering more than arrived,
                    // which V5b checks and no longer assumes.
                    // FR-148 P0 — with the switch off it gets here unoffered, and
                    // is then application-limited (the jupiter chain's trickle).
                    let basis = Basis::of_window(offer_held);
                    // FR-148 D48 — the window's own delivery: near a loss the
                    // shadow model's window rules read it (rules 2 and 3).
                    self.belief
                        .observe_link_capacity_in(rx_bps, basis, take, now);
                    self.link_rx_bps = Some((rx_bps, now));
                    self.path.link_rx = basis;
                }
                // FR-79 V5b — neither arms nor releases: the queue did grow, so
                // whatever the clamp holds it still holds, and on its own TTL.
                (true, Some(_)) => self.link_unoffered = self.link_unoffered.saturating_add(1),
                // FR-59 — ONSET and HOLD read different sensors, because
                // they ask different questions.
                //
                // Onset (above) is "is the queue growing?" — the drift
                // derivative, which needs no clock probe and so speaks on
                // the links where the age report is absent or rejected.
                //
                // Release is "is the queue still DEEP?" — and the derivative
                // cannot answer that. Two attempts got this wrong the same
                // way: v1 released when growth stopped, v2 released when the
                // growth INTEGRAL drained. Both are made of growth, and
                // growth stops as soon as the sender's cadence stretches to
                // match the drained rate — which the viewer-rate fps cap
                // guarantees. Field 2026-09-02, adjacent windows: paint age
                // 505 ms while the integral read 10; the relief held in 2 of
                // 58 windows and peak age stayed at 2.5 s.
                //
                // So release only when BOTH say the queue is gone: the age
                // level is back near its learned floor AND the integral has
                // drained. Either one still reading deep holds the clamp,
                // and the AIMD's additive increase does the climbing back —
                // down fast, up slow.
                //
                // ⚠ `age_elevated` is false when the viewer reported no age
                // at all, so a session with no clock lock degrades to the
                // integral-only rule rather than holding the clamp forever.
                // The `LINK_CEILING_TTL` is the backstop for a viewer that
                // goes silent entirely.
                (false, Some(_))
                    if !age_elevated && self.link_loop.depth_ms() <= LINK_RELEASE_DEPTH_MS =>
                {
                    self.link_rx_bps = None
                }
                _ => {}
            }
        }
        // FR-79 V5b — taken after this window's own readings are in, so a blocked
        // send the same window's delivery beats is out at once. Everything that
        // reads the pipe below and in the next tick sees the result.
        let refuted_by = match link {
            Some((rx_bps, _))
                if constrained && valid && floor_raised && self.slow_link.measured_evidence =>
            {
                self.refute_readings(rx_bps, now)
            }
            _ => None,
        };
        // FR-59 P3 — a growing transit queue folds into `struggling` exactly
        // as FR-15's age excess does: an instant, re-open-free byte cut
        // through machinery that already exists.
        let link_over = link_acts && verdict.congested && offered;
        let own_div = self.viewer_rate.observe(
            reported_fps,
            struggling || age_over || link_over,
            target_fps,
        );
        let new_div = own_div.max(fold_followers(own_div));
        let changed = new_div != self.skip_divisor;
        self.skip_divisor = new_div;
        // Feed the AIMD a congestion sample and STOP — deliberately no
        // `take_move` here: consuming the move would mark it applied
        // while the encoder never heard about it. The pump's own
        // `pre_encode_tick` picks it up one frame later (≤33 ms) and
        // routes it through the existing apply arms.
        // FR-59 P3 — same treatment for a growing transit queue. This is
        // the arm the field case needed: the send channel was never full,
        // so nothing else in the loop could produce a decrease.
        // FR-79 V5 — and the same treatment for a transit stall we caused.
        // This is the arm the CORPLAP-2 session needed: the send channel was
        // never full (`bytes_inflight=0`, `send_wait_max_ms=0.3`), the viewer's
        // age report landed in a rejected window, so NOTHING in the loop could
        // produce a decrease while the transport stalled eleven times.
        // `note_congestion` is the rate-limited ×0.85 with its 500 ms
        // spacing — deliberately not `apply_hard_md`, which has no spacing at
        // all and would halve once per stalled window.
        // FR-148 P0 — the cause names the first of the three that holds.
        if (age_over || link_over || stall_is_ours)
            && let Some(ctrl) = self.aimd.as_mut()
        {
            let signal = if age_over {
                Congestion::Age
            } else if link_over {
                Congestion::Link
            } else {
                Congestion::TransitStall
            };
            ctrl.note_congestion(signal, now);
        }
        // FR-70 P1 — advance the prior. A LIVE measurement (blocked-send
        // goodput, or the viewer's arrival rate while its queue grows)
        // re-anchors it; a window in which the PIPE did not push back lets it
        // decay toward the band. Push-back is a send stall, an age excess,
        // a viewer-reported queue growth or a drain — deliberately NOT a
        // byte-budget skip, which is the pump's own throttle and on the
        // field session was the prior's own artefact (`encode::prior`).
        // `age_elevated` rather than `age_over` alone: the LEVEL speaks even
        // when the age loop is switched off from acting.
        // FR-71 T1b — a held window is neither clean nor pushed back, so the
        // prior does not move at all.
        if constrained && valid {
            let stall_seen = std::mem::take(&mut self.stall_seen);
            // FR-79 V5b — a delivery that refuted something re-anchors the prior
            // when no reading is left: the stand-in then says "carried at least
            // this", not the number the delivery disproved. Past the band that
            // ends the prior, and the session is the unremembered one.
            // FR-148 D51 B4b-2 — under `on`, on a relay session, the budget's
            // held rate is the sender's reading (`sent_bps`); B4a-2 — and its
            // queue, within the envelope, is no age push-back (above).
            let reading = self.sent_bps(now).or(match self.link_rx_bps {
                Some((rx, at)) if now.duration_since(at) <= LINK_CEILING_TTL => Some(rx),
                _ => None,
            });
            if reading.is_some() {
                self.last_reading_bps = reading;
            }
            let live = reading.or(refuted_by);
            let pushed_back =
                age_over || age_elevated || link_over || drain_for_ms.is_some() || stall_seen;
            self.prior.on_window(live, pushed_back);
            // FR-148 P0 — what the prior's base now stands on (shadow).
            if live.is_some_and(|l| l > 0) {
                self.path.prior = match reading {
                    Some(r) if r > 0 => self.reading_origin(now),
                    _ => Origin::At(now),
                };
            }
        } else if constrained && stall_is_ours && !self.slow_link.measured_evidence {
            // FR-79 V5 — no measurement (the stall's own numbers are not the
            // pipe's: that is the 14:52 defect), but the push-back is real, so
            // the prior re-anchors instead of decaying toward the band as it
            // would on a window where the pipe simply did not push back.
            //
            // ⚠️ V5b — behind the kill switch only. A stall carries no number, so
            // this walked the prior with nothing behind it: 180,484 → 84,195 in
            // 28 s on 2026-10-30, and the session then wrote 84,195 to the pair
            // memory as evidence. A rejected window now leaves the prior alone.
            self.prior.on_window(None, true);
        }
        // FR-35 — hand the learner this window's evidence.
        let ceiling_grown = if constrained {
            let desired = self.aimd.as_ref().map(|c| c.desired()).unwrap_or(0);
            // FR-79 V5b — only a valid window can carry a hold: the opener's
            // keyframe falls in a rejected one.
            if valid {
                self.learner.note_carried(desired, sent_bps);
            }
            self.learner
                .on_window(desired, sent_bps, self.last_viewer_age, now)
        } else {
            None
        };
        // FR-148 P0 — a step is a rate the path carried with the target pinned
        // under the ceiling: a lower bound. A cut or an abandonment is tagged
        // where it happens (`pre_encode_tick`).
        if ceiling_grown.is_some() {
            let learned_bps = self.learner.learned_bps();
            self.path.learned = (learned_bps, Tag::measured(Basis::AppLimited, now));
        }
        // FR-63 B1 — fold the SAME window into the shadow controller, reading
        // only what the live loops above already computed. It returns nothing
        // and no field below is a function of it. `frames_lost` is 0 until the
        // viewer-feedback v2 wire (B2) carries it, so the loss trigger is inert
        // in B1 — the age LEVEL, goodput and the full channel carry the verdict.
        // `frames_rx` is the viewer's reported fps, which is also the `fps == 0`
        // "no evidence" sentinel (a window the viewer painted nothing in).
        let shadow_goodput = self.goodput.estimate_bps(now);
        if let Some(s) = self.shadow.as_mut() {
            s.fold_window(
                &super::ratectl::ShadowWindow {
                    fps: reported_fps,
                    struggling,
                    age_avg_ms: report.map(|(avg, _, _)| avg),
                    age_min_ms: report.map(|(_, min, _)| min),
                    rtt_ms: report.map(|(_, _, rtt)| rtt),
                    rx_bps: link.map(|(rx, _)| rx),
                    queue_ms: link.map(|(_, q)| q),
                    goodput_bps: shadow_goodput,
                    sent_bps,
                    frames_rx: reported_fps,
                    frames_lost: 0,
                },
                now,
            );
        }
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
            link_ceiling_bps: verdict.ceiling_bps.filter(|_| offered),
            drain_for_ms,
        })
    }

    /// FR-15 — the last viewer age report (window avg, learned floor), ms,
    /// for the pump heartbeats: field verification reads the loop from
    /// `agent_logs` instead of the viewer's screen.
    pub fn viewer_age(&self) -> Option<(u16, u16)> {
        self.last_viewer_age
    }

    /// FR-70 M0 — the fused paint age split by plane, when the viewer
    /// reported an arrival age: `sender_ms` is the pump's own send-queue
    /// wait for the window (`None` where the pump has no such figure),
    /// `transit_ms` is arrival − sender, `viewer_ms` is paint − arrival.
    /// `None` from a pre-M0 viewer, or a window with no age at all.
    pub fn viewer_age_split(&self, sender_ms: Option<f64>) -> Option<viewer_rate::AgeSplit> {
        let (age, _) = self.last_viewer_age?;
        let arrival = self.last_viewer_arrival?;
        Some(viewer_rate::split_age(age, arrival, sender_ms))
    }

    /// FR-71 T1a — the pump hands in what it measured about its own send side
    /// on THIS PASS, as deltas since its previous call, before every tick. The
    /// governor folds the passes into the window ([`WindowSenderStats::absorb`])
    /// and the tick that closes the window consumes the sum; a pump that never
    /// calls this (the VP9-444 pump) classifies from the split and the
    /// viewer's report alone.
    pub fn note_window_sender(&mut self, stats: WindowSenderStats) {
        match self.window_sender.as_mut() {
            Some(window) => window.absorb(stats),
            None => self.window_sender = Some(stats),
        }
    }

    /// FR-71 T1a — the last window's verdict on which plane is the limiter,
    /// for the heartbeat. `None` before the first constrained window; FR-79
    /// made it load-bearing, so there is no longer a switch that stops it.
    pub fn pipe_state(&self) -> Option<super::pipe_state::PipeState> {
        self.pipe.last()
    }

    /// FR-71 AC2 — how many `transit-stalled` windows were report gaps
    /// (a subset of `pipe_state_counts()[3]`), so gap-holds and split-holds
    /// are counted apart in the heartbeat.
    pub fn pipe_gap_stalls(&self) -> u32 {
        self.pipe.gap_stalls()
    }

    /// FR-71 T1a — windows per verdict so far: `[unknown, clear, overproduced,
    /// transit_stalled, viewer_late]`.
    pub fn pipe_state_counts(&self) -> [u32; 5] {
        self.pipe.counts()
    }

    /// FR-79 — windows the validity gate rejected, as
    /// `[agent-stalled, transit-stalled, stall-shadow, carrier-changed, opening,
    /// keyframe-burst]` (FR-119 P2b-8 appended the fifth and P2b-13 the sixth,
    /// so the earlier ones keep their places).
    pub fn evidence_rejected(&self) -> [u32; 6] {
        self.evidence.counts()
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

    /// FR-112 P2 — a software pump's cadence, once per heartbeat window
    /// ([`SoftwarePace`](super::encode_pressure::SoftwarePace): the
    /// sustainable rate in 5-fps steps, hysteretic, never below 30). Neither
    /// the bitrate factor nor the resolution tier is engaged here: libvpx's
    /// cost barely follows the bitrate (FR-112 §2), and the VP9-444 pump takes
    /// no tier. Returns [`Self::paced_software_fps`].
    pub fn pace_software(&mut self, avg_encode_ms: f32, target_fps: u32) -> Option<u32> {
        self.sw_pace.observe(avg_encode_ms, target_fps)
    }

    /// FR-112 P2 — the rate a software pump runs at (`None` = its target),
    /// never paced below 0.4.141's software target. The tier only ever adds
    /// frames.
    pub fn paced_software_fps(&self) -> Option<u32> {
        self.sw_pace.paced()
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

    /// FR-148 P0 — the moves handed out since the last call, counted by their
    /// [`Why`]. Each pump's heartbeat line takes them (`target_moves`), so a
    /// count covers the time since the previous heartbeat line.
    pub fn take_moves(&mut self) -> Moves {
        std::mem::take(&mut self.moves)
    }

    pub fn frames_skipped_decode(&self) -> u64 {
        self.frames_skipped_decode
    }

    // --- FR-63 B1 — the shadow controller's outputs, for the heartbeat ---
    //
    // These accessors are the ONLY path out of the shadow. None of them feeds a
    // live decision; the controller drives nothing until B2. `None` throughout
    // when `ratectl = off` or the shadow has not opened yet.

    /// What the shadow controller would be targeting right now.
    pub fn shadow_target_bps(&self) -> Option<u32> {
        self.shadow
            .as_ref()
            .and_then(super::ratectl::Controller::target_bps)
    }

    /// The shadow's state machine position (`startup`/`normal`/`overuse`/
    /// `hold`/`underuse`), or `off` when the shadow is not running.
    pub fn shadow_state(&self) -> &'static str {
        self.shadow.as_ref().map_or("off", |s| s.state().as_str())
    }

    /// The shadow's own estimate of what the path carries.
    pub fn shadow_measured_bps(&self) -> Option<u32> {
        self.shadow
            .as_ref()
            .and_then(super::ratectl::Controller::measured_bps)
    }

    /// Why the shadow's target last moved.
    pub fn shadow_reason(&self) -> &'static str {
        self.shadow.as_ref().map_or("off", |s| s.reason().as_str())
    }

    /// Bounded pauses the shadow would have ordered on a constrained path (it
    /// cannot pause anything in B1 — this counts what it would have done).
    pub fn shadow_pauses(&self) -> u32 {
        self.shadow
            .as_ref()
            .map_or(0, super::ratectl::Controller::pauses)
    }

    /// `(shadow − live) / live`, percent — the quantity the flip criterion
    /// reads. NEGATIVE means the shadow asked for LESS than the governor
    /// applied, the safe direction. `None` when the shadow is off, has no target
    /// yet, or the live target is 0.
    pub fn shadow_disagree_pct(&self) -> Option<i32> {
        let shadow = self.shadow_target_bps()?;
        let live = self.applied_bps();
        if live == 0 {
            return None;
        }
        Some(((i64::from(shadow) - i64::from(live)) * 100 / i64::from(live)) as i32)
    }

    /// The target handed out, with why it moved (FR-148 P0): the controller's
    /// causes, and for an increase the last window's R3 verdict. Each move is
    /// counted by its `why` for the heartbeat ([`Self::take_moves`]).
    fn record_applied(&mut self, (bps, causes): (u32, Causes)) -> AppliedBitrate {
        let changed = bps != self.applied_bps;
        self.applied_bps = bps;
        let why = Why {
            causes,
            app_limited: self
                .window_app_limited
                .filter(|_| causes.contains(Cause::Increase)),
        };
        self.moves.note(why);
        AppliedBitrate { bps, changed, why }
    }
}

/// FR-148 P0 — the path numbers with their tags, and the shadow model beside
/// them. Nothing a session does reads any of this: its one reader is the
/// `path model` line (`media_delivery`).
impl RateGovernor {
    /// Every path number the pump's heartbeat carries, each with how it was
    /// obtained and how old it is, and the shadow model's view: what the `path
    /// model` line writes. Each number is the one the heartbeat's accessor gives
    /// at `now`, which `tests::same_numbers` checks at every replayed window. `seed_age` is
    /// how old the pair memory's entry was when the session opened.
    pub fn path_view(&self, now: Instant, seed_age: Option<Duration>) -> PathView {
        let tags = &self.path;
        let number = |(bps, tag): (u32, Tag)| tags.number(bps, tag, now, seed_age);
        // The model counts a remembered number's age from the open; the entry's
        // own age is known here.
        let aged = |n: PathNumber| PathNumber {
            age: n.age + seed_age.filter(|_| n.remembered).unwrap_or_default(),
            ..n
        };
        let prior = self.prior_tagged().map(number);
        let model = self.belief.model();
        let measured = self.measured_tagged(now, tags.constrained);
        PathView {
            goodput: self.goodput_tagged(now).map(number),
            belief: (
                self.belief.anchor_number(now),
                self.belief.floor_number(now),
                self.belief.capacity_number(now),
            ),
            measured: measured.map(|(bps, tag, _)| number((bps, tag))),
            measured_by: measured.map(|(_, _, by)| by),
            prior,
            learned_ceiling: Some(number((self.learner.learned_bps(), tags.learned.1))),
            slow_link_floor: self
                .relieved_floor_bps
                .map(|bps| number((bps, tags.relieved))),
            viewer_rx: tags
                .arrival
                .map(|(bps, basis, at)| number((bps, Tag::measured(basis, at)))),
            seed: tags.seed_bps.map(|bps| number((bps, Tag::REMEMBERED))),
            model_floor: model.floor(now).map(aged),
            model_capacity: model.capacity(now),
            model_stand_in: model.stand_in(prior, now).map(aged),
            model_floor_by: model.floor_by(),
            model_capacity_by: model.capacity_by(now),
            model_ruled: self.belief.ruled(now),
            model_fell: model.fell(now),
        }
    }

    /// FR-148 D46 B2 — a delivery sample's push-back, P1 or P3, at the pump's
    /// heartbeat: to the shadow model and the pair memory only
    /// ([`super::pipe::Pipe::observe_sample_capacity`]), filed under the last
    /// window's take. After a window the gate rejected, nothing: the sample
    /// ran beside a stall the viewer saw. No rate or gate reads it, and the
    /// path memory it reaches opens a session only under `path_model = on`.
    pub fn on_sample_push_back(&mut self, bps: u32, by: super::pipe::PushedBy, now: Instant) {
        if let Some(take) = self.sample_take {
            self.belief.observe_sample_capacity(bps, by, take, now);
        }
    }

    /// FR-148 D46 B2 — the in-flight budget, computing on or in shadow, saw
    /// the clock stand still for the least and S with bytes out, or read a
    /// release: the next [`super::pipe::STALL_WINDOWS`] windows are no reading
    /// for the shadow model or the pair memory
    /// ([`super::pipe::Pipe::note_stall`]). Nothing else moves.
    pub fn note_path_stall(&mut self) {
        self.belief.note_stall();
    }

    /// FR-148 D48 — a sample rule acted on a closed sample, at the pump's
    /// heartbeat: rule 1 re-read its P3 push-back at the budget's rate, or
    /// rule 4 kept it out. The `path model` line names it
    /// ([`super::pipe::Pipe::note_ruled`]) when the push-back would have
    /// reached the shadow model: after a window the gate rejected B2 files
    /// none ([`Self::on_sample_push_back`]), so no rule acted on one.
    pub fn on_sample_ruled(&mut self, rule: super::pipe::Ruled, now: Instant) {
        if self.sample_take.is_some() {
            self.belief.note_ruled(rule, now);
        }
    }

    /// FR-148 D48 — the in-flight budget's last step, computing on or in
    /// shadow, beside its stall mark ([`Self::note_path_stall`]): its rate,
    /// whether this association's figures gave it, and SCTP's loss count. The
    /// shadow model's window rules read it at the next window's close
    /// ([`super::pipe::Pipe::note_path_rate`]). FR-148 D51: kept for B4's
    /// readers, which read it only while the budget acts
    /// ([`super::pipe::PathRate::acts`]).
    pub fn note_path_rate(&mut self, rate: super::pipe::PathRate) {
        self.belief.note_path_rate(rate);
        self.budget_path = Some(rate);
    }

    /// FR-148 D51 — whether the in-flight budget acts on this session (`on`,
    /// a relay session), from its last step. Off, shadow and a direct session
    /// never say so, and nothing below reads the budget then.
    fn budget_acts(&self) -> bool {
        self.budget_path.is_some_and(|r| r.acts)
    }

    /// FR-148 D51 B4b — the rate the acting budget measured, while it held a
    /// frame in the last second outside a still span: the rate the path
    /// carries while the sender is backlogged. `None` otherwise.
    fn budget_held_bps(&self) -> Option<u32> {
        self.budget_path
            .filter(|r| r.acts && r.held && r.measured)
            .and_then(|r| r.bps)
            .filter(|&bps| bps > 0)
    }

    /// FR-148 D51 B4a-2 — the acting budget's envelope, in ms: the age its
    /// own queue may add, S and the window's largest frame at its rate. 0
    /// when it does not act.
    fn budget_envelope_ms(&self) -> u16 {
        self.budget_path
            .filter(|r| r.acts)
            .map_or(0, |r| r.envelope_ms)
    }

    /// FR-148 D51 B4f, B4g — the interval the pump chose for its next encode,
    /// the least it is charged where it waited at the gate (`encode::charge`):
    /// while the budget acts, its capture interval at the rate it runs at
    /// (FR-112's paced rate while in force, else `target_fps`) times the skip
    /// divisor. `None` while the budget does not act: the pts is today's.
    #[cfg_attr(not(feature = "vp9-444"), allow(dead_code))]
    pub fn charge_interval(&self, target_fps: u32) -> Option<Duration> {
        if !self.budget_acts() {
            return None;
        }
        let fps = self.paced_software_fps().unwrap_or(target_fps).max(1);
        let interval = 1_000_000 / u64::from(fps);
        Some(Duration::from_micros(
            interval.saturating_mul(u64::from(self.skip_divisor.max(1))),
        ))
    }

    /// FR-148 D51 B4b-2 — what the sender measured: the blocked-send goodput,
    /// or, while the budget acts, the budget's held rate in its place. Under
    /// `on` the budget keeps sends from blocking, so the goodput reads only
    /// the opening's burst, and the budget is what reads the path.
    fn sent_bps(&self, now: Instant) -> Option<u32> {
        if self.budget_acts() {
            self.budget_held_bps()
        } else {
            self.goodput.estimate_bps(now)
        }
    }

    /// [`Self::sent_bps`], tagged: backlogged either way, the goodput's aged
    /// from the last window it folded, the budget's from its last step.
    fn sent_tagged(&self, now: Instant) -> Option<(u32, Tag)> {
        if self.budget_acts() {
            let bps = self.budget_held_bps()?;
            return Some((bps, Tag::measured(Basis::Backlogged, now)));
        }
        self.goodput_tagged(now)
    }

    /// [`Self::blocked_send_bps`], tagged: backlogged by construction, aged
    /// from the last window the estimator folded.
    fn goodput_tagged(&self, now: Instant) -> Option<(u32, Tag)> {
        let bps = self.goodput.estimate_bps(now)?;
        let at = self.goodput.sampled_at()?;
        Some((bps, Tag::measured(Basis::Backlogged, at)))
    }

    /// [`Self::measured_bps`], tagged: the sender's tag ([`Self::sent_tagged`])
    /// or the held arrival rate's, whichever reading it is (the sender's on a
    /// tie). FR-148 D51 B4g — with its source: the sender's is the goodput's,
    /// or the budget's held rate while the budget acts, under the same basis.
    fn measured_tagged(&self, now: Instant, constrained: bool) -> Option<(u32, Tag, MeasuredBy)> {
        if !constrained {
            return None;
        }
        let link = match self.link_rx_bps {
            Some((rx, at)) if now.duration_since(at) <= LINK_CEILING_TTL => Some((
                rx,
                Tag::measured(self.path.link_rx, at),
                MeasuredBy::Arrival,
            )),
            _ => None,
        };
        let sender = if self.budget_acts() {
            MeasuredBy::Budget
        } else {
            MeasuredBy::Goodput
        };
        let sent = self.sent_tagged(now).map(|(bps, tag)| (bps, tag, sender));
        match (sent, link) {
            (Some(g), Some(l)) => Some(if g.0 <= l.0 { g } else { l }),
            (g, l) => g.or(l),
        }
    }

    /// [`Self::pipe_bps`], tagged: the measured reading's, else the stand-in's.
    fn pipe_tagged(&self, now: Instant, constrained: bool) -> Option<(u32, Tag)> {
        if !constrained {
            return None;
        }
        self.measured_tagged(now, constrained)
            .map(|(bps, tag, _)| (bps, tag))
            .or_else(|| self.prior_tagged())
    }

    /// [`Self::prior_bps`], tagged: assumed, aged from the reading its base
    /// was anchored on, or from the seed's entry.
    fn prior_tagged(&self) -> Option<(u32, Tag)> {
        let bps = self.prior.stand_in_bps()?;
        Some((bps, Tag::assumed(self.path.prior)))
    }

    /// When the reading the prior anchors on at `now` was taken: the sender's
    /// ([`Self::sent_tagged`]), else the held arrival rate's.
    fn reading_origin(&self, now: Instant) -> Origin {
        match (self.sent_tagged(now), self.link_rx_bps) {
            (Some((_, tag)), _) => tag.origin,
            (None, Some((_, at))) => Origin::At(at),
            (None, None) => Origin::At(now),
        }
    }

    /// FR-148 D51 — the prior's base and its net steps from it, for the
    /// `tcp_shape` closed loop's trail: the walk the stand-in alone hides
    /// while it sits above the band.
    #[cfg(test)]
    pub(crate) fn prior_walk(&self) -> (Option<u32>, i32) {
        (self.prior.base_bps(), self.prior.steps())
    }

    /// FR-148 D51 B4a-1 — whether a congestion signal for the opener's ramp
    /// arrived since the last viewer window, for C2.
    #[cfg(test)]
    pub(crate) fn slow_start_congested(&self) -> bool {
        self.slow_start_congested
    }
}

/// FR-148 P0 — where the evidence behind a held number was taken: at an
/// instant of this session, or in an earlier one (the pair memory's seed,
/// whose age at the open the pump knows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    At(Instant),
    Seed,
}

/// FR-148 P0 — a held number's class, and where its age runs from. A measured
/// class from the seed is a remembered number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tag {
    basis: Basis,
    origin: Origin,
}

impl Tag {
    /// The seed: measured in an earlier session. The entry does not say which
    /// measured class (FR-148 P1 stores it), so the weaker one.
    const REMEMBERED: Tag = Tag {
        basis: Basis::AppLimited,
        origin: Origin::Seed,
    };

    fn measured(basis: Basis, at: Instant) -> Self {
        Self {
            basis,
            origin: Origin::At(at),
        }
    }

    fn assumed(origin: Origin) -> Self {
        Self {
            basis: Basis::Assumed,
            origin,
        }
    }
}

/// FR-148 P0 — how each path number the governor holds was obtained, kept
/// beside the numbers where they are written. Read only by
/// [`RateGovernor::path_view`]: no decision reads a tag.
#[derive(Debug, Clone, Copy)]
struct PathTags {
    opened_at: Instant,
    /// The pair memory's seed, as the governor was given it.
    seed_bps: Option<u32>,
    /// The transport the pump's latest tick named: what its heartbeat reads
    /// the numbers on.
    constrained: bool,
    /// The last per-window arrival rate the governor took, and its class.
    arrival: Option<(u32, Basis, Instant)>,
    /// The class of the held arrival-rate reading (`link_rx_bps`). Set with
    /// it, so never read while that is `None`.
    link_rx: Basis,
    /// What the prior's base stands on: the seed, until a reading anchors it.
    prior: Origin,
    /// The learned ceiling as last tagged, and its tag.
    learned: (u32, Tag),
    /// What the relieved floor was derived from.
    relieved: Tag,
}

impl PathTags {
    fn open(now: Instant, seed_bps: Option<u32>) -> Self {
        Self {
            opened_at: now,
            seed_bps: seed_bps.filter(|s| *s > 0),
            constrained: false,
            arrival: None,
            link_rx: Basis::Assumed,
            prior: Origin::Seed,
            learned: (0, Tag::assumed(Origin::At(now))),
            relieved: Tag::assumed(Origin::At(now)),
        }
    }

    /// The learned ceiling as the session opens: at the seed, with the seed's
    /// tag, or at its nominal start (0, assumed).
    fn learned_at_open(&self, learned_bps: u32) -> (u32, Tag) {
        if learned_bps > 0 {
            (learned_bps, Tag::REMEMBERED)
        } else {
            (0, Tag::assumed(Origin::At(self.opened_at)))
        }
    }

    /// The learned ceiling moved other than by a step (a cut, an abandonment):
    /// it is the controller's own number.
    fn learned_moved(&mut self, learned_bps: u32, now: Instant) {
        if learned_bps != self.learned.0 {
            self.learned = (learned_bps, Tag::assumed(Origin::At(now)));
        }
    }

    /// `bps` with `tag`, read at `now`.
    fn number(&self, bps: u32, tag: Tag, now: Instant, seed_age: Option<Duration>) -> PathNumber {
        match tag.origin {
            Origin::At(at) => PathNumber::new(bps, tag.basis, now.saturating_duration_since(at)),
            Origin::Seed => {
                let age =
                    seed_age.unwrap_or_default() + now.saturating_duration_since(self.opened_at);
                match tag.basis {
                    Basis::Assumed => PathNumber::new(bps, Basis::Assumed, age),
                    _ => PathNumber::remembered(bps, age),
                }
            }
        }
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

    /// FR-63 B1 — the containment proof, and the only field evidence B1 can
    /// offer (a fleet release is a different phase). Two governors, identical
    /// but for `ratectl` (Off vs the shipped Shadow), driven through the SAME
    /// frames, events and windows — a thin constrained pipe that congests, then
    /// recovers — must produce byte-identical LIVE output: the applied bitrate,
    /// the relieved floor, the pipe-state tally, the link stats and the goodput
    /// samples. The shadow observes and the heartbeat reports; it steers
    /// nothing. If a future edit ever wires it into a live decision, this test
    /// goes red. (Red-run confirmed 2026-10-29: making the shadow's fold write
    /// `self.applied_bps` fails exactly this test.)
    #[test]
    fn the_shadow_never_moves_the_live_decision() {
        let now = Instant::now();
        let mk = |mode: crate::encode::RatectlMode| {
            RateGovernor::new(
                30,
                DEPTH,
                GovernorFlags {
                    ratectl: mode,
                    ..GovernorFlags::default()
                },
                0,
                Some(300_000), // a remembered-slow pair, so the shadow opens and climbs
                now,
            )
        };
        let mut off = mk(crate::encode::RatectlMode::Off);
        let mut shadow = mk(crate::encode::RatectlMode::Shadow);
        assert!(off.shadow_target_bps().is_none(), "Off runs no shadow");
        // `shadow` has no target until its first `set_domain` (it opens lazily,
        // like the AIMD); the end-of-test assertions prove it opened and ran.

        // Drive both byte-identically. A constrained session: a floor of 1.5 M,
        // a 2.55 M ceiling, send capacity swinging full↔free, viewer windows
        // with a rising then falling age, and a hard stall partway.
        let mut t = now;
        let ceiling = 2_550_000u32;
        let floor = 1_500_000u32;
        for i in 0..120u32 {
            t += Duration::from_millis(33);
            let capacity = if i % 7 == 0 { 0 } else { DEPTH }; // periodic full
            let a = off.pre_encode_tick(ceiling, floor, true, capacity, t);
            let b = shadow.pre_encode_tick(ceiling, floor, true, capacity, t);
            assert_eq!(
                a.map(|x| x.bps),
                b.map(|x| x.bps),
                "pre_encode_tick diverged at frame {i}"
            );
            if i == 40 {
                // `note_send_stall` returns () (it feeds the AIMD a sample for
                // the next tick); the per-step `applied_bps` assertions below
                // are what prove it did the same thing to both governors.
                off.note_send_stall(Duration::from_secs(2), t);
                shadow.note_send_stall(Duration::from_secs(2), t);
            }
            if i % 30 == 29 {
                // A viewer window: a rising age in the first half, falling after.
                let age = if i < 70 { 400u16 } else { 40u16 };
                let packed_age = viewer_rate::pack_age(age, age, 80);
                let packed_link = viewer_rate::pack_link(900_000, if i < 70 { 150 } else { 0 });
                let packed_report = viewer_rate::pack_report(28, i < 70);
                let va = off.tick_viewer_window(
                    t,
                    30,
                    || packed_report,
                    || packed_age,
                    || packed_link,
                    true,
                    |d| d,
                    u64::from(i) * 50_000,
                );
                let vb = shadow.tick_viewer_window(
                    t,
                    30,
                    || packed_report,
                    || packed_age,
                    || packed_link,
                    true,
                    |d| d,
                    u64::from(i) * 50_000,
                );
                // ViewerWindow is not PartialEq; compare the fields the pump
                // reads and acts on.
                assert_eq!(
                    va.map(|w| (w.skip_divisor, w.age_over, w.link_congested, w.drain_for_ms)),
                    vb.map(|w| (w.skip_divisor, w.age_over, w.link_congested, w.drain_for_ms)),
                    "tick_viewer_window diverged at frame {i}"
                );
            }
            // Every live output must match at every step.
            assert_eq!(
                off.applied_bps(),
                shadow.applied_bps(),
                "applied_bps at {i}"
            );
            assert_eq!(
                off.relieved_floor_bps(),
                shadow.relieved_floor_bps(),
                "relieved_floor_bps at {i}"
            );
            assert_eq!(
                off.pipe_state_counts(),
                shadow.pipe_state_counts(),
                "pipe_state_counts at {i}"
            );
            assert_eq!(off.link_stats(), shadow.link_stats(), "link_stats at {i}");
            assert_eq!(
                off.goodput_samples(),
                shadow.goodput_samples(),
                "goodput_samples at {i}"
            );
            assert_eq!(off.viewer_age(), shadow.viewer_age(), "viewer_age at {i}");
        }

        // And the shadow actually did something — otherwise the test proves
        // only that a dead controller is inert.
        assert!(
            shadow.shadow_target_bps().is_some(),
            "the shadow must have a target to be worth containing"
        );
        assert_ne!(shadow.shadow_state(), "off", "the shadow must have run");
    }

    /// FR-79 — the WIRING (the classifier is unit-tested in
    /// `encode::pipe_state`, the gate in `encode::evidence`, the laws in the
    /// B0 cells). A silent window while the pump kept sending is a transit
    /// stall; so is a report whose transit share dwarfs its floor. Neither is
    /// evidence about the pipe, so the age loop does not fire on the backlog's
    /// own elevated windows, and each rejection is attributable in the
    /// heartbeat. Before FR-79 this needed a kill switch (`transit_hold`) and
    /// its own counter; the rule is now unconditional.
    #[test]
    fn the_age_loop_does_not_fire_on_a_window_the_gate_rejected() {
        use crate::encode::pipe_state::PipeState;
        let start = Instant::now();
        let mut g = gov(start);
        let mut t = start;
        let sender = |frames_sent: u32| WindowSenderStats {
            inflight_bytes: 1_500,
            budget_bytes: 128_000,
            gate_skips: 0,
            send_wait_max_ms: 1.0,
            send_wait_avg_ms: Some(0.5),
            frames_sent,
            stalled_passes: 0,
            opening: false,
        };
        // Three clean windows learn the floors (40 ms transit, 1 ms viewer).
        for _ in 0..3 {
            t += Duration::from_millis(1100);
            g.note_window_sender(sender(30));
            g.tick_viewer_window(
                t,
                30,
                || viewer_rate::pack_report(30, false),
                || viewer_rate::pack_age_with_arrival(42, 40, 80, 41),
                || 0,
                true,
                |o| o,
                0,
            )
            .expect("window due");
            assert_eq!(g.pipe_state(), Some(PipeState::Clear));
        }
        // Two silent windows with the pump still sending: finding 4's first
        // seconds.
        for _ in 0..2 {
            t += Duration::from_millis(1100);
            g.note_window_sender(sender(30));
            g.tick_viewer_window(t, 30, || 0, || 0, || 0, true, |o| o, 0)
                .expect("window due");
            assert_eq!(g.pipe_state(), Some(PipeState::TransitStalled));
        }
        // The backlog lands: two windows whose transit share is seconds.
        let mut fired = false;
        for _ in 0..2 {
            t += Duration::from_millis(1100);
            g.note_window_sender(sender(30));
            let w = g
                .tick_viewer_window(
                    t,
                    30,
                    || viewer_rate::pack_report(30, false),
                    || viewer_rate::pack_age_with_arrival(4000, 3500, 80, 3999),
                    || 0,
                    true,
                    |o| o,
                    0,
                )
                .expect("window due");
            assert_eq!(g.pipe_state(), Some(PipeState::TransitStalled));
            fired |= w.age_over;
        }
        assert!(
            !fired,
            "the age loop must not fire on windows that are not evidence"
        );
        // Four rejections, every one a transit stall — no agent stall, no
        // shadow (each window's predecessor stalled too, and the stall itself
        // is the more specific answer), no carrier change.
        assert_eq!(g.evidence_rejected(), [0, 4, 0, 0, 0, 0]);
    }
    /// FR-70 P1 — a remembered-slow pair, constrained, as the field ran it.
    fn remembered(seed_bps: u32, flags: GovernorFlags, now: Instant) -> RateGovernor {
        RateGovernor::new(30, DEPTH, flags, 8_000_000, Some(seed_bps), now)
    }

    /// FR-70 P1 — the WIRING (the law is unit-tested in `encode::prior`).
    ///
    /// Field 2026-09-04, CORPLAP-1 → neo16 (`6a9abc30`): a pair remembered
    /// at 200 kbps ran four minutes at `slow_link_floor_bps=Some(200000)`
    /// with `goodput_bps=None`, zero send stalls and zero viewer-congested
    /// windows. Nothing ever measured the pipe, and the memory held the
    /// floor AND the queue budget for the whole session. With nothing
    /// measuring, both must let go by themselves.
    #[test]
    fn an_unmeasured_prior_lets_the_floor_and_the_budget_go() {
        const PLAN_CEILING: u32 = 2_550_000;
        const FLOOR: u32 = crate::encode::MIN_BITRATE_BPS;
        let t0 = Instant::now();
        let mut g = remembered(200_000, GovernorFlags::default(), t0);
        g.pre_encode_tick(PLAN_CEILING, FLOOR, true, DEPTH, t0);
        assert_eq!(
            g.applied_bps(),
            200_000,
            "opens at the remembered rate (P8)"
        );
        assert_eq!(g.relieved_floor_bps(), Some(200_000));
        assert_eq!(g.prior_bps(), Some(200_000));
        assert_eq!(
            g.pipe_bps(t0, true),
            Some(200_000),
            "the queue budget reads the seed"
        );

        // Ten clean windows, nothing measured: one decay step, and the floor
        // — and with it the AIMD's target — follows it up.
        let mut now = t0;
        for _ in 0..10 {
            now += Duration::from_secs(1);
            g.tick_viewer_window(now, 30, || 0, || 0, || 0, true, |o| o, 0);
        }
        g.pre_encode_tick(PLAN_CEILING, FLOOR, true, DEPTH, now);
        assert_eq!(g.prior_bps(), Some(250_000));
        assert_eq!(g.relieved_floor_bps(), Some(212_500));
        assert!(g.applied_bps() >= 212_500, "applied={}", g.applied_bps());
        assert_eq!(g.pipe_bps(now, true), Some(250_000));

        // Long enough, and the prior is gone: nominal floor, nominal budget —
        // the unremembered session, byte for byte.
        for _ in 0..110 {
            now += Duration::from_secs(1);
            g.tick_viewer_window(now, 30, || 0, || 0, || 0, true, |o| o, 0);
        }
        g.pre_encode_tick(PLAN_CEILING, FLOOR, true, DEPTH, now);
        assert_eq!(g.prior_bps(), None);
        assert_eq!(g.relieved_floor_bps(), None);
        assert_eq!(g.pipe_bps(now, true), None);
        assert!(g.applied_bps() >= FLOOR, "applied={}", g.applied_bps());
        // The memory has nothing better than the applied rate now, so the
        // pair is recorded at ≥ the band and stops re-seeding slow.
        assert_eq!(g.pipe_bps(now, true), None);
    }

    /// A probe that finds the pipe: the viewer reports its queue growing at
    /// 300 kbps of arrival. That is a MEASUREMENT — the floor follows the
    /// live value, the prior becomes it, and once the report releases the
    /// decay resumes from 300 k rather than from the seed.
    #[test]
    fn a_live_measurement_re_anchors_the_prior() {
        const PLAN_CEILING: u32 = 2_550_000;
        const FLOOR: u32 = crate::encode::MIN_BITRATE_BPS;
        let t0 = Instant::now();
        let mut g = remembered(200_000, GovernorFlags::default(), t0);
        g.pre_encode_tick(PLAN_CEILING, FLOOR, true, DEPTH, t0);
        let mut now = t0;
        for _ in 0..20 {
            now += Duration::from_secs(1);
            g.tick_viewer_window(now, 30, || 0, || 0, || 0, true, |o| o, 0);
        }
        assert_eq!(g.prior_bps(), Some(312_500));
        // Four windows of a growing transit queue at 300 kbps of arrival.
        let growing_from = now;
        for _ in 0..4 {
            now += Duration::from_secs(1);
            g.tick_viewer_window(
                now,
                30,
                || 0,
                || 0,
                || viewer_rate::pack_link(300_000, 200),
                true,
                |o| o,
                offering(growing_from, now),
            );
        }
        assert_eq!(
            g.prior_bps(),
            Some(300_000),
            "re-anchored on the measurement"
        );
        assert_eq!(g.pipe_bps(now, true), Some(300_000));
        g.pre_encode_tick(PLAN_CEILING, FLOOR, true, DEPTH, now);
        assert_eq!(
            g.relieved_floor_bps(),
            Some(255_000),
            "the floor follows the LIVE value, not the prior"
        );
        // The queue drains and the clamp releases; then nothing measures.
        for _ in 0..4 {
            now += Duration::from_secs(1);
            g.tick_viewer_window(
                now,
                30,
                || 0,
                || 0,
                || viewer_rate::pack_link(300_000, -300),
                true,
                |o| o,
                0,
            );
        }
        for _ in 0..10 {
            now += Duration::from_secs(1);
            g.tick_viewer_window(now, 30, || 0, || 0, || 0, true, |o| o, 0);
        }
        assert_eq!(
            g.prior_bps(),
            Some(330_000),
            "decays from the measurement, not from the seed — and gently (×1.1)"
        );
        assert_eq!(g.pipe_bps(now, true), Some(330_000));
    }

    /// The hazard the down-step closes: a prior a few percent ABOVE the pipe
    /// grows a queue too slowly for the link loop to latch (it wants 100 ms
    /// of growth per window) and the AIMD cannot decrease below the floor.
    /// The age LEVEL is what sees it — two elevated windows walk the prior
    /// back down until the queue drains.
    #[test]
    fn age_excess_without_a_measurement_walks_the_prior_down() {
        const PLAN_CEILING: u32 = 2_550_000;
        const FLOOR: u32 = crate::encode::MIN_BITRATE_BPS;
        let t0 = Instant::now();
        let mut g = remembered(200_000, GovernorFlags::default(), t0);
        g.pre_encode_tick(PLAN_CEILING, FLOOR, true, DEPTH, t0);
        let mut now = t0;
        for _ in 0..30 {
            now += Duration::from_secs(1);
            g.tick_viewer_window(now, 30, || 0, || 0, || 0, true, |o| o, 0);
        }
        assert_eq!(g.prior_bps(), Some(390_625));
        // The viewer's paint age sits 200 ms over a 100 ms floor, while its
        // queue-growth report stays under the link loop's onset — the slow
        // leak. No measurement exists; only the level speaks.
        let elevated = |g: &mut RateGovernor, now: &mut Instant| {
            *now += Duration::from_secs(1);
            g.tick_viewer_window(
                *now,
                30,
                || 0,
                || viewer_rate::pack_age(300, 100, 200),
                || viewer_rate::pack_link(380_000, 50),
                true,
                |o| o,
                0,
            );
        };
        elevated(&mut g, &mut now);
        assert_eq!(
            g.prior_bps(),
            Some(390_625),
            "one elevated window is a lump"
        );
        elevated(&mut g, &mut now);
        assert_eq!(g.prior_bps(), Some(312_500), "two running: one step down");
        g.pre_encode_tick(PLAN_CEILING, FLOOR, true, DEPTH, now);
        assert_eq!(g.relieved_floor_bps(), Some(265_625));
        elevated(&mut g, &mut now);
        elevated(&mut g, &mut now);
        assert_eq!(g.prior_bps(), Some(250_000));
        // The queue drains, the age returns to its floor: the climb resumes.
        for _ in 0..12 {
            now += Duration::from_secs(1);
            g.tick_viewer_window(
                now,
                30,
                || 0,
                || viewer_rate::pack_age(105, 100, 200),
                || 0,
                true,
                |o| o,
                0,
            );
        }
        assert_eq!(g.prior_bps(), Some(312_500));
    }

    /// A direct transport has no prior in force at all — the relief, the
    /// budget and the memory are constrained-only machinery.
    #[test]
    fn a_direct_session_has_no_prior() {
        let t0 = Instant::now();
        let mut g = remembered(200_000, GovernorFlags::default(), t0);
        g.pre_encode_tick(CEILING, crate::encode::MIN_BITRATE_BPS, false, DEPTH, t0);
        assert_eq!(g.relieved_floor_bps(), None);
        assert_eq!(g.pipe_bps(t0, false), None);
        assert_eq!(g.pipe_bps(t0, false), None);
    }

    /// FR-63 — the WIRING, not the law (the law is unit-tested in
    /// `encode::slow_start`). A constrained session must not open at whatever
    /// constant the ceiling chain resolved.
    ///
    /// Field 2026-09-03 (CORPLAP-1 over a corp VPN): it opened at 2_550_000
    /// into a path measured at 213_180 — 444 ms of queue, a 1550 ms paint,
    /// then six windows collapsing to the truth.
    #[test]
    fn slow_start_caps_the_opening_ceiling_only_when_enabled_and_constrained() {
        const PLAN_CEILING: u32 = 2_550_000;
        const FLOOR: u32 = 200_000;
        let open = crate::encode::slow_start::OPEN_BPS;
        let t0 = Instant::now();
        let mk = |slow_start: bool| {
            RateGovernor::new(
                30,
                DEPTH,
                GovernorFlags {
                    slow_start,
                    ..GovernorFlags::default()
                },
                0,
                None,
                t0,
            )
        };

        // ON + constrained: capped at the ramp's opening rate.
        let mut on = mk(true);
        on.pre_encode_tick(PLAN_CEILING, FLOOR, true, DEPTH, t0);
        assert!(
            on.applied_bps() <= open,
            "opened at {} — slow-start must cap it at {open}",
            on.applied_bps()
        );

        // OFF: byte-for-byte the old behaviour — the session opens high.
        let mut off = mk(false);
        off.pre_encode_tick(PLAN_CEILING, FLOOR, true, DEPTH, t0);
        assert!(
            off.applied_bps() > open,
            "the kill switch must restore the old opener"
        );

        // ON but DIRECT: untouched. The over-drive this fixes was measured on
        // constrained transports, and a direct pair usually has the headroom.
        let mut direct = mk(true);
        direct.pre_encode_tick(PLAN_CEILING, FLOOR, false, DEPTH, t0);
        assert!(
            direct.applied_bps() > open,
            "slow-start must not touch a direct session"
        );
    }

    /// FR-63 — the REAL constrained floor must not be read as evidence.
    ///
    /// The test above passes `FLOOR = 200_000`, which is below
    /// [`slow_start::OPEN_BPS`] and therefore cannot detect the defect this
    /// locks: the wiring used to hand `SlowStart::new` the governor's
    /// `floor_bps`, which on every constrained transport is
    /// `area_min_bitrate_bps` — a flat `MIN_BITRATE_BPS` (1.5 M) fleet
    /// constant. `OPEN_BPS.max(1_500_000)` opened the session at 1.5 M: still
    /// 7× over the 213 kbps field path, and one doubling from the 2.55 M
    /// ceiling, i.e. no ramp at all while looking like one.
    ///
    /// The floor parameter means "a rate already PROVEN for this pair". A
    /// constant proves nothing, so only `open_seed_bps` may fill it.
    #[test]
    fn the_nominal_1_5m_floor_is_not_evidence_and_cannot_lift_the_opener() {
        const PLAN_CEILING: u32 = 2_550_000;
        let open = crate::encode::slow_start::OPEN_BPS;
        let t0 = Instant::now();

        let mut g = RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                slow_start: true,
                ..GovernorFlags::default()
            },
            0,
            None, // no rate memory for this pair — nothing is proven
            t0,
        );
        // The real constrained floor, not a convenient small one.
        g.pre_encode_tick(
            PLAN_CEILING,
            crate::encode::MIN_BITRATE_BPS,
            true,
            DEPTH,
            t0,
        );
        assert!(
            g.applied_bps() <= open,
            "opened at {} — the flat {} floor must not lift the opener",
            g.applied_bps(),
            crate::encode::MIN_BITRATE_BPS
        );
    }

    /// FR-63 × FR-59 P8 — a rate the pair actually EARNED still wins.
    ///
    /// The counterpart to the test above: `open_seed_bps` is real evidence
    /// (this pair was measured at this rate), so slow-start must not drag a
    /// remembered-slow pair back down to `OPEN_BPS` and make it re-earn what
    /// it already proved. Field value from 2026-09-02: `seed_bps=387500`.
    #[test]
    fn a_remembered_rate_still_opens_the_session_above_the_ramp_floor() {
        const PLAN_CEILING: u32 = 2_550_000;
        const REMEMBERED: u32 = 387_500;
        let t0 = Instant::now();

        let mut g = RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                slow_start: true,
                floor_relief: true, // `open_seed_bps` is gated on it
                ..GovernorFlags::default()
            },
            8_000_000,
            Some(REMEMBERED),
            t0,
        );
        g.pre_encode_tick(
            PLAN_CEILING,
            crate::encode::MIN_BITRATE_BPS,
            true,
            DEPTH,
            t0,
        );
        assert!(
            g.applied_bps() <= REMEMBERED,
            "opened at {} — must not exceed the remembered {REMEMBERED}",
            g.applied_bps()
        );
        assert!(
            g.applied_bps() > crate::encode::slow_start::OPEN_BPS,
            "opened at {} — a PROVEN rate must not be dragged down to the ramp floor",
            g.applied_bps()
        );
    }

    /// FR-59 P8 — a constrained governor with the FR-35 rate memory seeded.
    fn seeded(seed: Option<u32>, floor_relief: bool, now: Instant) -> RateGovernor {
        RateGovernor::new(
            15,
            DEPTH,
            GovernorFlags {
                floor_relief,
                viewer_rate_clamp: true,
                seed_contradiction: true,
                ..GovernorFlags::default()
            },
            8_000_000,
            seed,
            now,
        )
    }

    /// FR-59 P8 — a pair remembered BELOW the nominal floor opens AT the
    /// remembered rate, with the floor relief seeded from it. Field
    /// 2026-09-02: `seed_bps=387500` logged, then the encoder opened at the
    /// 2.55 M relay cap anyway (the FR-35 learner only ever RAISES the
    /// ceiling), a 372 KB opener burst into a ~300 kbps pipe — ten seconds
    /// of queue before the first measurement could say a word.
    #[test]
    fn remembered_slow_pair_opens_at_the_remembered_rate() {
        let now = Instant::now();
        let mut g = seeded(Some(387_500), true, now);
        let applied = g
            .pre_encode_tick(2_550_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, now)
            .expect("the first tick applies");
        assert_eq!(applied.bps, 387_500, "opens at the seed, not the ceiling");
        assert_eq!(
            g.relieved_floor_bps(),
            Some(super::super::goodput::measured_floor_bps(
                387_500,
                crate::encode::MIN_BITRATE_BPS,
                crate::encode::slow_link_min_bitrate_bps(),
            )),
            "the floor relief is seeded too, so the AIMD can hold the target there"
        );
        assert_eq!(g.open_seed_bps(), Some(387_500));
        assert_eq!(
            g.pipe_bps(now, true),
            Some(387_500),
            "P2's queue budget sees the seed until live evidence lands"
        );
        assert_eq!(
            g.pipe_bps(now, false),
            None,
            "a direct session never reads it"
        );
    }

    /// FR-59 P8 — kill switch: with the floor relief off the opener is the
    /// pre-P8 one, byte for byte.
    #[test]
    fn seed_opening_is_inert_without_the_floor_relief() {
        let now = Instant::now();
        let mut g = seeded(Some(387_500), false, now);
        let applied = g
            .pre_encode_tick(2_550_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, now)
            .expect("applies");
        assert_eq!(applied.bps, 2_550_000);
        assert_eq!(g.open_seed_bps(), None);
        assert_eq!(g.pipe_bps(now, true), None);
        assert_eq!(g.relieved_floor_bps(), None);
    }

    /// FR-59 P8 — a pair remembered FASTER than the nominal floor is the
    /// FR-35 learner's business (it lifts the ceiling), not P8's.
    #[test]
    fn a_fast_memory_still_opens_at_the_ceiling() {
        let now = Instant::now();
        let mut g = seeded(Some(4_677_606), true, now);
        let applied = g
            .pre_encode_tick(2_550_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, now)
            .expect("applies");
        assert!(
            applied.bps >= 2_550_000,
            "a seed above the nominal floor never lowers the opener: {}",
            applied.bps
        );
        assert_eq!(g.open_seed_bps(), None);
        assert_eq!(g.relieved_floor_bps(), None);
        assert_eq!(g.pipe_bps(now, true), None);
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
        assert_eq!(g.blocked_send_bps(t1), Some(10_000_000));
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
        let measured = g.blocked_send_bps(t1).expect("a held estimate");
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
        assert_eq!(g.blocked_send_bps(start), None);

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
                    offering(start, t),
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

    /// FR-59 P4 — a queue deep enough that a rate cut cannot clear it
    /// pauses production, the pause EXPIRES on its own, and expiring is
    /// what re-arms the next drain.
    #[test]
    fn a_deep_queue_pauses_production_and_the_pause_expires() {
        let start = Instant::now();
        let mut g = gov(start);
        assert!(!g.draining(start), "nothing to drain yet");

        let mut t = start;
        let mut ordered = None;
        for _ in 0..6 {
            t += Duration::from_millis(1100);
            let w = g
                .tick_viewer_window(
                    t,
                    30,
                    || viewer_rate::pack_report(30, false),
                    || 0,
                    || viewer_rate::pack_link(400_000, 300),
                    true,
                    |o| o,
                    0,
                )
                .expect("window due");
            if let Some(ms) = w.drain_for_ms {
                ordered = Some(ms);
                break;
            }
        }
        let ms = ordered.expect("a 900 ms-deep queue must order a drain");
        assert_eq!(ms, viewer_rate::DRAIN_MAX_MS, "bounded sub-second");
        // The pump is paused for exactly that long, and not a tick longer.
        assert!(g.draining(t));
        assert!(g.draining(t + Duration::from_millis(ms as u64 - 1)));
        assert!(!g.draining(t + Duration::from_millis(ms as u64)));
        // Expiry handed the drain back, so the loop may order another.
        let (_, drains, _) = g.link_stats();
        assert_eq!(drains, 1);
        let mut t2 = t + Duration::from_secs(1);
        let mut second = None;
        for _ in 0..6 {
            t2 += Duration::from_millis(1100);
            let w = g
                .tick_viewer_window(
                    t2,
                    30,
                    || viewer_rate::pack_report(30, false),
                    || 0,
                    || viewer_rate::pack_link(400_000, 300),
                    true,
                    |o| o,
                    0,
                )
                .expect("window due");
            if w.drain_for_ms.is_some() {
                second = Some(());
                break;
            }
        }
        assert!(second.is_some(), "a served drain must re-arm");
    }

    /// FR-59 P4 kill switch, and the direct-path guarantee: a drain is a
    /// visible freeze, so it must be impossible to get one where it was
    /// not asked for.
    #[test]
    fn the_drain_is_off_by_switch_and_on_direct() {
        for (label, flags, constrained) in [
            (
                "kill switch",
                GovernorFlags {
                    queue_drain: false,
                    ..GovernorFlags::default()
                },
                true,
            ),
            ("direct path", GovernorFlags::default(), false),
        ] {
            let start = Instant::now();
            let mut g = RateGovernor::new(30, DEPTH, flags, 0, None, start);
            let mut t = start;
            for _ in 0..10 {
                t += Duration::from_millis(1100);
                let w = g
                    .tick_viewer_window(
                        t,
                        30,
                        || viewer_rate::pack_report(30, false),
                        || 0,
                        || viewer_rate::pack_link(400_000, 400),
                        constrained,
                        |o| o,
                        0,
                    )
                    .expect("window due");
                assert_eq!(w.drain_for_ms, None, "{label}: must not order a drain");
                assert!(!g.draining(t), "{label}: must never pause");
            }
        }
    }

    /// FR-59 — the clamp must SURVIVE a window that merely stops growing.
    ///
    /// The field regression this locks (2026-09-02, 150 kbit, 47 windows):
    /// congestion is detected in bursts, so releasing on the first
    /// non-congested window made the relief flap — it held in 5 of 47
    /// windows while the depth estimate sat at 119–651 ms and paint age
    /// reached 4,656 ms. A session that has converged ONTO the pipe has
    /// zero growth and a full queue, which is precisely when releasing
    /// re-opens the overshoot.
    #[test]
    fn the_arrival_clamp_holds_until_the_queue_has_drained() {
        let start = Instant::now();
        let mut g = gov(start);
        let mut t = start;
        let step = |g: &mut RateGovernor, t: &mut Instant, q: i16| {
            *t += Duration::from_millis(1100);
            g.tick_viewer_window(
                *t,
                30,
                || viewer_rate::pack_report(30, false),
                || 0,
                || viewer_rate::pack_link(400_000, q),
                true,
                |o| o,
                offering(start, *t),
            )
            .expect("window due")
        };
        // Two growing windows arm the clamp and build depth to 600 ms.
        step(&mut g, &mut t, 300);
        assert!(step(&mut g, &mut t, 300).link_congested);
        // A FLAT window: growth stopped, but 600 ms of queue is still out
        // there. The clamp must hold.
        let w = step(&mut g, &mut t, 0);
        assert!(!w.link_congested, "not growing any more");
        t += Duration::from_millis(10);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        assert!(
            g.relieved_floor_bps().is_some(),
            "the clamp must survive a merely-not-growing window"
        );
        // Now drain it: negative drift until the integral is spent.
        for _ in 0..8 {
            step(&mut g, &mut t, -300);
        }
        t += Duration::from_millis(10);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        // …and release once the queue is actually gone. FR-70 P1: releasing
        // the CLAMP no longer snaps the floor back to the 1.5 M constant —
        // that snap forced the target 4× over a pipe just measured at 400 k
        // and re-created the queue the release had waited for. The last
        // measurement stays as the prior (400 k ⇒ a floor of 340 k), and
        // decays from there while nothing measures.
        assert_eq!(
            g.pipe_bps(t, true),
            Some(400_000),
            "the clamp released ⇒ the prior is the last measurement"
        );
        assert_eq!(
            g.relieved_floor_bps(),
            Some(340_000),
            "the floor follows the last MEASURED rate, not the constant"
        );
        // Ten clean windows later the prior has taken one decay step.
        let mut t2 = t;
        for _ in 0..10 {
            t2 += Duration::from_millis(1100);
            g.tick_viewer_window(t2, 30, || 0, || 0, || 0, true, |o| o, 0);
        }
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t2);
        assert_eq!(
            g.prior_bps(),
            Some(440_000),
            "×1.1 — a MEASURED base decays gently"
        );
        assert_eq!(g.relieved_floor_bps(), Some(374_000));
    }

    /// FR-59 — the clamp must survive the exact field shape that defeated
    /// both earlier release rules: **growth has stopped, and the queue is
    /// still deep**.
    ///
    /// Measured 2026-09-02 on adjacent windows: paint age 505 ms while the
    /// growth integral read 10. v1 released when growth stopped; v2
    /// released when the integral drained. Both are made of growth, and
    /// growth stops the moment the sender's cadence stretches to match the
    /// drained rate — which the viewer-rate fps cap guarantees. Only the
    /// age LEVEL sees a queue that has stopped growing.
    #[test]
    fn the_clamp_holds_on_a_deep_queue_that_stopped_growing() {
        const FLOOR: u16 = 110;
        let start = Instant::now();
        let mut g = gov(start);
        let mut t = start;
        // `rtt/2` is the physical bound; a floor sample at it is plausible.
        let win = |g: &mut RateGovernor, t: &mut Instant, q: i16, age: u16| {
            *t += Duration::from_millis(1100);
            g.tick_viewer_window(
                *t,
                30,
                || viewer_rate::pack_report(30, false),
                || viewer_rate::pack_age(age, FLOOR, FLOOR * 2),
                || viewer_rate::pack_link(400_000, q),
                true,
                |o| o,
                offering(start, *t),
            )
            .expect("window due")
        };
        // Two growing windows arm the clamp.
        win(&mut g, &mut t, 300, 400);
        assert!(win(&mut g, &mut t, 300, 500).link_congested);
        t += Duration::from_millis(10);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        assert!(g.relieved_floor_bps().is_some(), "clamp armed");

        // THE FIELD SHAPE: growth has stopped and the integral DRAINS —
        // field 2026-09-02 saw it fall 484 -> 10 across adjacent windows,
        // i.e. slightly negative drift as the cadence caught up — while
        // the standing queue stayed 505 ms deep.
        for _ in 0..10 {
            win(&mut g, &mut t, -80, 505);
        }
        assert!(
            g.link_stats().2 <= LINK_RELEASE_DEPTH_MS,
            "the integral has drained — this is what fooled v2"
        );
        t += Duration::from_millis(10);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        assert!(
            g.relieved_floor_bps().is_some(),
            "…but the queue is still 505 ms deep, so the clamp must HOLD"
        );

        // Now the queue genuinely clears: age returns to its floor.
        for _ in 0..3 {
            win(&mut g, &mut t, 0, FLOOR + 5);
        }
        t += Duration::from_millis(10);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        // …and releases once the age is back at the floor. The RELEASE is
        // the clamp letting go of the ceiling; FR-70 P1 keeps the floor at
        // the last measurement (400 k ⇒ 340 k) rather than the 1.5 M
        // constant — see `the_arrival_clamp_holds_until_the_queue_has_drained`.
        assert!(
            g.link_stats().2 <= LINK_RELEASE_DEPTH_MS && !g.link_loop_holding(),
            "the clamp must have released"
        );
        assert_eq!(g.relieved_floor_bps(), Some(340_000));
    }

    /// A viewer that reports NO age must not pin the clamp forever — it
    /// degrades to the integral-only rule.
    #[test]
    fn a_viewer_with_no_age_report_still_releases() {
        let start = Instant::now();
        let mut g = gov(start);
        let mut t = start;
        let win = |g: &mut RateGovernor, t: &mut Instant, q: i16| {
            *t += Duration::from_millis(1100);
            g.tick_viewer_window(
                *t,
                30,
                || viewer_rate::pack_report(30, false),
                || 0, // no age at all — the clock probe never locked
                || viewer_rate::pack_link(400_000, q),
                true,
                |o| o,
                offering(start, *t),
            )
        };
        win(&mut g, &mut t, 300);
        win(&mut g, &mut t, 300);
        t += Duration::from_millis(10);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        assert!(g.relieved_floor_bps().is_some());
        for _ in 0..10 {
            win(&mut g, &mut t, -300);
        }
        t += Duration::from_millis(10);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        // No age signal ⇒ the integral decides the RELEASE, as before; and
        // (FR-70 P1) the released clamp leaves the last measurement as the
        // floor's prior instead of snapping to the constant.
        assert!(!g.link_loop_holding(), "the clamp must have released");
        assert_eq!(g.relieved_floor_bps(), Some(340_000));
    }

    /// FR-59 — `pipe_bps` is the widened estimate every "how fast
    /// is the link" consumer must use. The field defect it closes: on a
    /// link whose sends never block there is NO goodput estimate, so the
    /// goodput-only accessor left P2's queue budget permanently inert while
    /// P1's floor relief — reading the widened value — engaged.
    #[test]
    fn the_pipe_estimate_includes_the_viewer_when_goodput_is_absent() {
        let start = Instant::now();
        let mut g = gov(start);
        // No blocked sends at all — the harness case.
        assert_eq!(g.blocked_send_bps(start), None);
        let mut t = start;
        for _ in 0..3 {
            t += Duration::from_millis(1100);
            g.tick_viewer_window(
                t,
                30,
                || viewer_rate::pack_report(30, false),
                || 0,
                || viewer_rate::pack_link(400_000, 300),
                true,
                |o| o,
                offering(start, t),
            );
        }
        assert_eq!(
            g.blocked_send_bps(t),
            None,
            "the goodput-only view still sees nothing"
        );
        assert_eq!(
            g.pipe_bps(t, true),
            Some(400_000),
            "…while the widened view has the viewer's arrival rate"
        );
        // Direct transports never consume it.
        assert_eq!(g.pipe_bps(t, false), None);
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
                        // The bytes matter: with none, FR-79 V5b would stop this
                        // report and the switch under test would prove nothing.
                        offering(start, t),
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

    /// FR-59 P6 kill switch — the SAME contradiction, ignored: nothing is
    /// abandoned and the session keeps planning at the seeded ceiling until
    /// the AIMD walks it down, which is the pre-P6 posture.
    ///
    /// The learner proves the switch for itself
    /// (`ceiling_learn::on_measurement_is_inert_without_a_contradiction`);
    /// this proves the governor's FLAG reaches it. The wiring is one line —
    /// `on_measurement(g, self.slow_link.seed_contradiction)` — and every
    /// other test constructs the flags with the field on, so a hard-wired
    /// `true` there would have passed the whole module (AC6).
    #[test]
    fn a_contradicted_seed_stands_when_the_switch_is_off() {
        let start = Instant::now();
        let mut g = RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                seed_contradiction: false,
                ..GovernorFlags::default()
            },
            8_000_000,
            Some(5_964_000),
            start,
        );
        assert_eq!(g.effective_ceiling(3_000_000, true), 5_069_400);
        // The field pipe, 12.8× under the seed — the ON arm above abandons on it.
        let sink = g.goodput_sink();
        for _ in 0..12 {
            sink.record(1_000, Duration::from_micros(20_248));
        }
        let t1 = start + Duration::from_secs(2);
        g.tick_viewer_window(t1, 30, || 0, || 0, || 0, true, |o| o, 0);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, 0, t1);
        assert_eq!(
            g.blocked_send_bps(t1),
            Some(395_100),
            "the contradicting measurement is held — the switch is the only thing in the way"
        );
        assert_eq!(
            g.take_seed_abandoned(),
            None,
            "kill switch: nothing abandoned"
        );
        assert_eq!(
            g.effective_ceiling(3_000_000, true),
            5_069_400,
            "the seed stands until the AIMD walks it down"
        );
    }

    /// FR-79 V5 — the WIRING (the per-reason law is unit-tested in
    /// `encode::evidence`, and finding 4's opposite cell is the B0 sim's).
    ///
    /// A session that has MEASURED its pipe and is sending above it must come
    /// DOWN when the transport stalls. Before V5 every loop that can lower the
    /// rate read `valid` — the goodput fold, the hard MD, the ramp verdict,
    /// the age cut and the viewer clamp alike — so a `TransitStalled` window
    /// reached none of them. Measured with this change reverted: two stalled
    /// windows against a 1.0 Mbps measurement move a 3.0 Mbps target by
    /// nothing at all.
    ///
    /// Field 2026-09-09, CORPLAP-2 AV1 over DERP/TCP (`6aa10d91`): the
    /// estimator accepted ONE sample all session (1,058,145 bps) while the
    /// target ramped to 1,928,555; eleven windows stalled, paint age reached
    /// 7,784 ms, and the rate ENDED HIGHER than it started.
    #[test]
    fn a_transit_stall_above_the_measured_pipe_cuts_the_rate() {
        use crate::encode::pipe_state::PipeState;
        let start = Instant::now();
        let mut g = RateGovernor::new(
            30,
            DEPTH,
            // ⚠️ `measured_ceiling` OFF on purpose. With it on, the
            // measurement itself clamps the ceiling and the rate falls for
            // that reason alone — the cell would pass with this whole change
            // reverted, proving nothing. Off, the only thing left in the loop
            // that can lower the rate on a rejected window is V5.
            GovernorFlags {
                measured_ceiling: false,
                age_feedback: true,
                ..GovernorFlags::default()
            },
            8_000_000,
            None,
            start,
        );
        let mut t = start;
        let sender = |frames_sent: u32| WindowSenderStats {
            inflight_bytes: 1_500,
            budget_bytes: 128_000,
            gate_skips: 0,
            send_wait_max_ms: 1.0,
            send_wait_avg_ms: Some(0.5),
            frames_sent,
            stalled_passes: 0,
            opening: false,
        };
        // Three clean windows learn the floors, then the session's ONE
        // measurement folds on the fourth: 1500 bytes per 12 ms = 1.0 Mbps.
        // (12 ms, not 8: `goodput::MIN_BLOCKED_SEND` drops anything under
        // 10 ms at the source, as headroom rather than evidence.)
        let sink = g.goodput_sink();
        for i in 0..4 {
            if i == 3 {
                for _ in 0..12 {
                    sink.record(1_500, Duration::from_millis(12));
                }
            }
            t += Duration::from_millis(1100);
            g.note_window_sender(sender(30));
            g.tick_viewer_window(
                t,
                30,
                || viewer_rate::pack_report(30, false),
                || viewer_rate::pack_age_with_arrival(42, 40, 80, 41),
                || 0,
                true,
                |o| o,
                0,
            )
            .expect("window due");
            // The measuring window carries blocked sends, so it reads
            // `Overproduced` — still VALID evidence, which is how the fold
            // gets its sample at all.
            if i < 3 {
                assert_eq!(g.pipe_state(), Some(PipeState::Clear));
            }
        }
        assert_eq!(
            g.blocked_send_bps(t),
            Some(1_000_000),
            "the one measurement"
        );
        // The rate sits at 3 Mbps — three times what the pipe measured.
        // ⚠️ A real send capacity, not 0: a zero reads as a saturated channel
        // and fires the backpressure MD, which would cut the rate here for a
        // reason that has nothing to do with the stall.
        let before = g
            .pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t)
            .map(|a| a.bps)
            .expect("a first target");
        assert!(before > 1_200_000, "the premise: above the measurement");
        // Two silent windows with the pump still sending: the transport
        // stalled, and it is OUR rate that is above the measurement.
        for _ in 0..2 {
            t += Duration::from_millis(1100);
            g.note_window_sender(sender(30));
            g.tick_viewer_window(t, 30, || 0, || 0, || 0, true, |o| o, 0)
                .expect("window due");
            assert_eq!(g.pipe_state(), Some(PipeState::TransitStalled));
        }
        let after = g
            .pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t)
            .map(|a| a.bps)
            .unwrap_or(before);
        assert!(
            after < before,
            "a transit stall above the measured pipe did not cut the rate \
             (before {before}, after {after})"
        );
        assert_eq!(
            g.evidence_rejected(),
            [0, 2, 0, 0, 0, 0],
            "both rejections were transit stalls"
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
        assert_eq!(g.blocked_send_bps(t1), Some(10_000_000));
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
        assert_eq!(g.blocked_send_bps(t1), Some(10_000_000));
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

    /// FR-71 T2 — a send blocked ≥ `HARD_STALL` followed by a window in which
    /// the pipe pushed back on NOTHING is a pause of the pipe, not its
    /// capacity: nothing halves, no ceiling is abandoned, no floor is relieved.
    /// Field 2026-09-08 12:15 UTC (CORPLAP-1 on the corp VPN, `6a9fed1d`): an
    /// overlay rekey storm blocked two sends for 2.9 s and 7.4 s on a pipe that
    /// carried 6.6 M at 50 ms twenty seconds later, and four movers read those
    /// blocked sends as a 1.6 M pipe — 6.60 → 0.68 M in 36 s, three minutes
    /// back. The hold (T1b) masked none of them.
    fn t2_governor(now: Instant) -> RateGovernor {
        RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                floor_relief: true,
                seed_contradiction: true,
                measured_ceiling: true,
                age_feedback: true,
                slow_start: false,
                ..GovernorFlags::default()
            },
            8_000_000,
            Some(6_600_000),
            now,
        )
    }

    fn t2_sender(frames_sent: u32) -> WindowSenderStats {
        WindowSenderStats {
            inflight_bytes: 0,
            budget_bytes: 128_000,
            gate_skips: 0,
            send_wait_max_ms: 0.1,
            send_wait_avg_ms: Some(0.05),
            frames_sent,
            stalled_passes: 0,
            opening: false,
        }
    }

    /// One reported, unblocked window at 50 ms age, then the pump's tick.
    fn t2_clean_window(g: &mut RateGovernor, t: Instant) {
        g.note_window_sender(t2_sender(30));
        g.tick_viewer_window(
            t,
            30,
            || viewer_rate::pack_report(30, false),
            || viewer_rate::pack_age_with_arrival(50, 49, 80, 50),
            || 0,
            true,
            |o| o,
            0,
        )
        .expect("window due");
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
    }

    #[test]
    fn a_hard_stall_inside_a_stalled_pump_pass_cuts_nothing() {
        let start = Instant::now();
        let mut g = t2_governor(start);
        let mut t = start;
        for _ in 0..4 {
            t += Duration::from_millis(1100);
            t2_clean_window(&mut g, t);
        }
        let before = g.applied_bps();
        let ceiling_before = g.learned_ceiling_bps();
        // The opener runs at 85 % of the remembered 6.6 M.
        assert!(
            ceiling_before >= 5_000_000,
            "the seeded ceiling stands: {ceiling_before}"
        );
        assert!(
            before > 3_000_000,
            "the session runs above the nominal: {before}"
        );

        // The stall: one send blocked 2.9 s inside a pump pass that overran its
        // budget — the 12:15 shape, where every timed phase read 0 ms and the
        // rest was `other`.
        g.goodput_sink().record(25_000, Duration::from_millis(2900));
        t += Duration::from_millis(2900);
        g.note_send_stall(Duration::from_millis(2900), t);
        g.note_window_sender(WindowSenderStats {
            stalled_passes: 1,
            ..t2_sender(2)
        });
        g.tick_viewer_window(t, 30, || 0, || 0, || 0, true, |o| o, 0)
            .expect("window due");
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);

        // The next window: reported, nothing blocked, age at the floor — the
        // pipe is back exactly as it was.
        t += Duration::from_millis(1100);
        t2_clean_window(&mut g, t);

        assert_eq!(g.applied_bps(), before, "a pause must not halve the rate");
        assert_eq!(
            g.learned_ceiling_bps(),
            ceiling_before,
            "a pause is not a measurement that contradicts the learned ceiling"
        );
        assert_eq!(g.relieved_floor_bps(), None, "a pause relieves no floor");
        // The window was rejected for the agent's own stall, and nothing else
        // read it.
        assert_eq!(g.evidence_rejected()[0], 1, "the agent-stalled window");
    }

    /// …and the control: the same blocked send on a FREE loop is the pipe
    /// pushing back, which is the case FR-35's ×0.5 was written for. The gate
    /// accepts the window (a full queue on a free loop is exactly what the
    /// goodput estimator measures), so the halving applies at that window and
    /// the measurement folds.
    #[test]
    fn a_hard_stall_on_a_free_loop_still_halves_at_its_own_window() {
        let start = Instant::now();
        let mut g = t2_governor(start);
        let mut t = start;
        for _ in 0..4 {
            t += Duration::from_millis(1100);
            t2_clean_window(&mut g, t);
        }
        let before = g.applied_bps();
        // A thin pipe: the send blocks for 2.9 s and the viewer keeps
        // reporting, with no pump pass overrunning its budget.
        g.goodput_sink().record(25_000, Duration::from_millis(2900));
        t += Duration::from_millis(2900);
        g.note_send_stall(Duration::from_millis(2900), t);
        g.note_window_sender(t2_sender(12));
        g.tick_viewer_window(
            t,
            30,
            || viewer_rate::pack_report(12, false),
            || viewer_rate::pack_age_with_arrival(300, 50, 80, 299),
            || 0,
            true,
            |o| o,
            0,
        )
        .expect("window due");
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        assert_eq!(
            g.pipe_state(),
            Some(crate::encode::pipe_state::PipeState::Overproduced),
            "a blocked send on a free loop is the sender's own back-pressure"
        );
        assert!(
            g.applied_bps() <= before / 2 + 1_000,
            "a hard stall the gate accepted halves: {} vs {before}",
            g.applied_bps()
        );
        assert_eq!(
            g.evidence_rejected(),
            [0, 0, 0, 0, 0, 0],
            "every window was evidence"
        );
        assert!(
            g.relieved_floor_bps().is_some(),
            "an accepted measurement relieves the floor"
        );
    }

    /// FR-71 T2b — the viewer's arrival-rate report that lands in the first
    /// window AFTER a `transit-stalled` run describes the stall, not the pipe.
    /// Field 2026-09-08 14:52 UTC (CORPLAP-1, 0.4.90 with T2): a carrier
    /// demotion stalled the relay for six seconds; the hold kept 7.45 M through
    /// three stalled windows with 0 B in flight, then the viewer reported
    /// 70 kbps of arrivals with a growing queue and the P3 clamp set the
    /// target to 834,800 while the sender's queue read over budget — no send
    /// had blocked for a second, so T2 had nothing to defer.
    #[test]
    fn a_link_report_in_the_shadow_of_a_transit_stall_does_not_arm_the_clamp() {
        let start = Instant::now();
        let mut g = t2_governor(start);
        g.slow_link.viewer_rate_clamp = true;
        let mut t = start;
        for _ in 0..4 {
            t += Duration::from_millis(1100);
            t2_clean_window(&mut g, t);
        }
        let before = g.applied_bps();
        assert!(before > 3_000_000, "runs above the nominal: {before}");
        // The stall: a silent window, then two whose transit share is seconds
        // — the sender idle throughout.
        t += Duration::from_millis(1100);
        g.note_window_sender(t2_sender(30));
        g.tick_viewer_window(t, 30, || 0, || 0, || 0, true, |o| o, 0)
            .expect("window due");
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        for (age, arrival, link) in [
            (2135u16, 2134u16, 0u64),
            (3626, 3624, viewer_rate::pack_link(70_340, 300)),
        ] {
            t += Duration::from_millis(1100);
            g.note_window_sender(t2_sender(30));
            g.tick_viewer_window(
                t,
                30,
                || viewer_rate::pack_report(1, false),
                || viewer_rate::pack_age_with_arrival(age, age - 30, 80, arrival),
                || link,
                true,
                |o| o,
                0,
            )
            .expect("window due");
            assert_eq!(
                g.pipe_state(),
                Some(crate::encode::pipe_state::PipeState::TransitStalled)
            );
            g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        }
        assert_eq!(
            g.applied_bps(),
            before,
            "the hold keeps the rate through the stall"
        );
        // The window after: the backlog lands (age 5.8 s), the queue reads
        // over budget, and the viewer's link report says 70 kbps arrived
        // while its queue grew — the stall's own numbers.
        t += Duration::from_millis(1100);
        g.note_window_sender(WindowSenderStats {
            inflight_bytes: 200_000,
            budget_bytes: 128_000,
            gate_skips: 83,
            send_wait_max_ms: 278.0,
            send_wait_avg_ms: Some(19.9),
            frames_sent: 12,
            stalled_passes: 0,
            opening: false,
        });
        g.tick_viewer_window(
            t,
            30,
            || viewer_rate::pack_report(7, false),
            || viewer_rate::pack_age_with_arrival(5785, 5700, 80, 5760),
            || viewer_rate::pack_link(834_800, 600),
            true,
            |o| o,
            0,
        )
        .expect("window due");
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        assert!(
            !g.link_loop_holding(),
            "a link report in the stall's shadow must not arm the arrival-rate clamp"
        );
        // Two shadowed reports: the second stalled window carried one too.
        assert_eq!(
            g.evidence_rejected()[2],
            1,
            "the window in the stall's shadow"
        );
        assert!(
            g.applied_bps() > before / 2,
            "no clamp to the stall's arrival rate: {} vs {before}",
            g.applied_bps()
        );
        // The windows after THAT are the pipe again: growing reports there act.
        let reporting_from = t;
        for q in [900i16, 1200] {
            t += Duration::from_millis(1100);
            g.note_window_sender(t2_sender(30));
            g.tick_viewer_window(
                t,
                30,
                || viewer_rate::pack_report(30, false),
                || viewer_rate::pack_age_with_arrival(60, 50, 80, 59),
                || viewer_rate::pack_link(400_000, q),
                true,
                |o| o,
                offering(reporting_from, t),
            )
            .expect("window due");
        }
        assert!(
            g.link_loop_holding(),
            "out of the shadow the clamp reads the reports again"
        );
        assert_eq!(
            g.evidence_rejected()[2],
            1,
            "the window in the stall's shadow"
        );
    }

    /// FR-112 P2 — the VP9-444 pump's pace is the software one (`SoftwarePace`,
    /// tested in `encode_pressure`): paced to what the encoder sustains, never
    /// below 30, and apart from the hardware pace, which it never moves.
    #[test]
    fn the_software_pace_is_its_own() {
        let mut g = gov(Instant::now());
        assert_eq!(g.pace_software(25.0, 60), Some(40), "1000 / 25 ms = 40 fps");
        assert_eq!(g.paced_software_fps(), Some(40));
        assert_eq!(g.paced_fps(), None, "the hardware pace is untouched");
        let mut g = gov(Instant::now());
        assert_eq!(g.pace_software(45.0, 60), Some(30), "22 fps: held at 30");
    }

    /// FR-119 P2b-8 — the pump reports every PASS; the window the gate and the
    /// classifier read is their sum. Counts add, the longest wait wins, the
    /// average weighs each pass by the frames it sent, any pass in the opening
    /// makes it an opening window, and the levels are the latest pass's.
    #[test]
    fn a_window_is_the_sum_of_its_passes() {
        let mut w = WindowSenderStats {
            inflight_bytes: 1_000,
            budget_bytes: 128_000,
            stalled_passes: 1,
            opening: true,
            gate_skips: 2,
            send_wait_max_ms: 40.0,
            send_wait_avg_ms: Some(10.0),
            frames_sent: 1,
        };
        w.absorb(WindowSenderStats {
            inflight_bytes: 3_000,
            budget_bytes: 96_000,
            stalled_passes: 0,
            opening: false,
            gate_skips: 3,
            send_wait_max_ms: 5.0,
            send_wait_avg_ms: Some(2.0),
            frames_sent: 3,
        });
        assert_eq!(w.stalled_passes, 1);
        assert!(
            w.opening,
            "a pass in the opening makes it an opening window"
        );
        assert_eq!(w.gate_skips, 5);
        assert_eq!(w.frames_sent, 4);
        assert_eq!(w.send_wait_max_ms, 40.0);
        assert_eq!(w.send_wait_avg_ms, Some(4.0), "(10 × 1 + 2 × 3) / 4");
        assert_eq!(
            (w.inflight_bytes, w.budget_bytes),
            (3_000, 96_000),
            "the levels are the latest pass's"
        );
    }

    /// FR-119 P2b-8 — the defect the sum fixes. A pump pass that stalled at the
    /// START of a window was overwritten by the clean passes after it: the
    /// window reached the gate as if the loop had been free all along.
    #[test]
    fn a_stalled_pass_early_in_the_window_still_rejects_it() {
        let start = Instant::now();
        let mut g = t2_governor(start);
        let mut t = start;
        for _ in 0..4 {
            t += Duration::from_millis(1100);
            t2_clean_window(&mut g, t);
        }
        g.note_window_sender(WindowSenderStats {
            stalled_passes: 1,
            ..t2_sender(1)
        });
        for _ in 0..29 {
            g.note_window_sender(t2_sender(1));
        }
        t += Duration::from_millis(1100);
        g.tick_viewer_window(t, 30, || 0, || 0, || 0, true, |o| o, 0)
            .expect("window due");
        assert_eq!(
            g.evidence_rejected()[0],
            1,
            "the window held a stalled pass, wherever in it"
        );
    }

    /// FR-119 P2b-8 — a DIRECT window inside the transport's opening is not a
    /// capacity measurement. A direct window used to be evidence by
    /// definition: on the MacBook a session's opening keyframe blocked its
    /// sends for 43–82 ms, folded as 14–22 Mbps of capacity, and FR-119's
    /// grant took a freshly granted 120 fps back for the claim's 60 s.
    #[test]
    fn a_direct_window_in_the_opening_folds_no_capacity() {
        let start = Instant::now();
        let mut g = gov(start);
        let sink = g.goodput_sink();
        // The opening: 40 × 30 KB sends, each blocked 24 ms (a 10 Mbps drain).
        for _ in 0..40 {
            sink.record(30_000, Duration::from_millis(24));
        }
        g.note_window_sender(WindowSenderStats {
            opening: true,
            ..t2_sender(40)
        });
        let t1 = start + Duration::from_secs(2);
        g.tick_viewer_window(t1, 30, || 0, || 0, || 0, false, |o| o, 0)
            .expect("window due");
        assert_eq!(
            g.belief_bps(t1).2,
            None,
            "no capacity claim from the opening"
        );
        assert_eq!(
            g.blocked_send_bps(t1),
            None,
            "and no estimate for the measured-ceiling clamp"
        );
        assert_eq!(g.evidence_rejected()[4], 1, "rejected for the opening");

        // The control: the same window once the opening has ended folds as it
        // always did. A direct pipe pushing back is still evidence.
        for _ in 0..40 {
            sink.record(30_000, Duration::from_millis(24));
        }
        g.note_window_sender(t2_sender(40));
        let t2 = t1 + Duration::from_millis(1100);
        g.tick_viewer_window(t2, 30, || 0, || 0, || 0, false, |o| o, 0)
            .expect("window due");
        assert_eq!(g.blocked_send_bps(t2), Some(10_000_000));
        assert!(g.belief_bps(t2).2.is_some(), "the capacity claim folds");
    }

    /// FR-119 P2b-13 — a DIRECT window whose only blocked send was a keyframe
    /// is not a capacity measurement: the keyframe spent its send growing the
    /// SCTP window that the deltas before it had left small. Field 2026-10-24:
    /// one 625 KB keyframe blocked 71.6 ms, folded as 69.9 Mbit/s on a mesh
    /// path that carried 311 Mbit/s, and took a granted 120 fps back. A pipe
    /// that really is narrower shows it in deltas blocking without a keyframe
    /// (P2b-14), and their window still folds. On a relay the keyframe alone
    /// still folds, as before.
    #[test]
    fn a_direct_window_whose_only_blocked_sends_are_keyframes_folds_no_capacity() {
        let start = Instant::now();
        let mut g = gov(start);
        let sink = g.goodput_sink();
        // Past the opening: one keyframe, blocked 72 ms (a 69 Mbit/s drain).
        sink.record_frame(625_000, Duration::from_millis(72), true);
        g.note_window_sender(t2_sender(120));
        let t1 = start + Duration::from_secs(2);
        g.tick_viewer_window(t1, 120, || 0, || 0, || 0, false, |o| o, 0)
            .expect("window due");
        assert_eq!(
            g.belief_bps(t1).2,
            None,
            "no capacity claim from a keyframe alone"
        );
        assert_eq!(g.blocked_send_bps(t1), None, "and nothing for the clamp");
        assert_eq!(g.evidence_rejected()[5], 1, "rejected as a keyframe burst");

        // A narrow pipe: the deltas block with no keyframe in the window, and
        // the window folds (10 × 30 KB in 24 ms each).
        for _ in 0..10 {
            sink.record(30_000, Duration::from_millis(24));
        }
        g.note_window_sender(t2_sender(11));
        let t2 = t1 + Duration::from_millis(1100);
        g.tick_viewer_window(t2, 120, || 0, || 0, || 0, false, |o| o, 0)
            .expect("window due");
        assert!(g.belief_bps(t2).2.is_some(), "the capacity claim folds");
        assert_eq!(g.evidence_rejected()[5], 1, "not a keyframe burst");

        // On a constrained window the keyframe alone is still read.
        let mut r = gov(start);
        let relay_sink = r.goodput_sink();
        relay_sink.record_frame(625_000, Duration::from_millis(900), true);
        r.note_window_sender(t2_sender(2));
        r.tick_viewer_window(t1, 30, || 0, || 0, || 0, true, |o| o, 0)
            .expect("window due");
        assert_eq!(
            r.evidence_rejected()[5],
            0,
            "a relay keeps the keyframe sample"
        );
        assert!(r.blocked_send_bps(t1).is_some(), "and folds it");
    }

    /// FR-79 V5b — the pump's running byte total at 1 Mbps since `from`: over 1.1 ×
    /// every arrival rate these cells report, so a growing queue counts, and under
    /// the 70 % of their ceilings that would carry a hold.
    fn offering(from: Instant, t: Instant) -> u64 {
        t.duration_since(from).as_millis() as u64 * 125
    }

    /// FR-79 V5b — a relayed session with only the rule under test in the way.
    /// `measured_ceiling` is off for the reason the V5 cell above gives.
    fn v5b_governor(start: Instant, seed: Option<u32>) -> RateGovernor {
        v5b_governor_hi(start, 8_000_000, seed)
    }

    /// With the learner's `hi` at the seeded ceiling it cannot step, so a window
    /// that carries the target does not also move it off the hold.
    fn v5b_governor_hi(start: Instant, hi_bps: u32, seed: Option<u32>) -> RateGovernor {
        RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                measured_ceiling: false,
                age_feedback: true,
                ..GovernorFlags::default()
            },
            hi_bps,
            seed,
            start,
        )
    }

    fn v5b_sender() -> WindowSenderStats {
        WindowSenderStats {
            inflight_bytes: 1_500,
            budget_bytes: 128_000,
            gate_skips: 0,
            send_wait_max_ms: 1.0,
            send_wait_avg_ms: Some(0.5),
            frames_sent: 30,
            stalled_passes: 0,
            opening: false,
        }
    }

    /// One reported window at a healthy paint age, carrying `link`, the pump idle.
    fn v5b_reported(g: &mut RateGovernor, t: &mut Instant, link: u64) {
        v5b_sending(g, t, link, 0);
    }

    /// One reported window with the pump's running byte total at `sent`.
    fn v5b_sending(g: &mut RateGovernor, t: &mut Instant, link: u64, sent: u64) {
        *t += Duration::from_millis(1100);
        g.note_window_sender(v5b_sender());
        g.tick_viewer_window(
            *t,
            30,
            || viewer_rate::pack_report(30, false),
            || viewer_rate::pack_age_with_arrival(70, 68, 80, 69),
            || link,
            true,
            |o| o,
            sent,
        )
        .expect("window due");
    }

    /// A silent window with the pump still sending: a transit stall.
    fn v5b_gap(g: &mut RateGovernor, t: &mut Instant) {
        *t += Duration::from_millis(1100);
        g.note_window_sender(v5b_sender());
        g.tick_viewer_window(*t, 30, || 0, || 0, || 0, true, |o| o, 0)
            .expect("window due");
        assert_eq!(
            g.pipe_state(),
            Some(crate::encode::pipe_state::PipeState::TransitStalled)
        );
    }

    /// `pack_link`'s arrival rate, in a window whose queue did not grow.
    fn v5b_delivery(rx_bps: u32) -> u64 {
        viewer_rate::pack_link(rx_bps, 0)
    }

    /// The pump's tick at a 3 M plan: a real send capacity, so nothing but the
    /// rule under test can lower the rate.
    fn v5b_tick(g: &mut RateGovernor, t: Instant) -> u32 {
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        g.applied_bps()
    }

    /// One 102 ms blocked send folds as the field's 223,794 bps: the same bytes
    /// and time as the 2026-10-30 11:48:36 reading, in a valid window.
    fn v5b_fold_223794(g: &mut RateGovernor, t: &mut Instant) {
        g.goodput_sink()
            .record(2_854, Duration::from_micros(102_022));
        v5b_reported(g, t, 0);
        assert_eq!(g.blocked_send_bps(*t), Some(223_794));
    }

    /// FR-79 V5b — the fail-first cell. Field 2026-10-20 13:53–14:19, CORPLAP-2,
    /// HEVC over `relay:derp/tcp`: `goodput_bps=None` throughout, a prior of
    /// 17.8–23.7 kbps under a 3.63 M target, 24 gap windows, 3.63 → 0.94 M with
    /// 0 B in flight. At 14:16, prior 19,578: 1.17 → 1.00 → 0.85 → 0.61 M.
    ///
    /// Each cut also walked the number the next stall was judged against. With
    /// a stand-in standing, a stall read "ours" every time. With nothing
    /// standing it never did, which is the A/B from 2026-10-30 11:48–12:19: 13
    /// cuts of 16 stalls with a stand-in, 0 of 26 with nothing, one session,
    /// one path, one build. So the cell runs every prior FR-71's 55 drops saw,
    /// with the no-prior arm as its control.
    #[test]
    fn a_stall_with_only_a_stand_in_standing_cuts_nothing() {
        for prior in [None, Some(19_578), Some(223_794), Some(800_000)] {
            let start = Instant::now();
            let mut t = start;
            let mut g = v5b_governor(start, None);
            // The classifier calls a silent window a stall only once the viewer
            // has reported at all.
            v5b_reported(&mut g, &mut t, 0);
            if let Some(p) = prior {
                g.prior.on_window(Some(p), false);
            }
            assert_eq!(g.pipe_bps(t, true), prior, "the premise");
            let before = v5b_tick(&mut g, t);
            assert_eq!(before, 3_000_000, "at the plan, so no climb can hide a cut");
            for _ in 0..3 {
                v5b_gap(&mut g, &mut t);
                v5b_tick(&mut g, t);
            }
            assert_eq!(
                g.applied_bps(),
                before,
                "prior {prior:?}: a stand-in is not a measurement"
            );
            assert_eq!(
                g.prior_bps(),
                prior,
                "prior {prior:?}: a stall with no number does not walk it"
            );
        }
    }

    /// FR-79 V5b — a stall above a LIVE measurement still cuts (the 07:41 fix),
    /// but it carries no number, so the prior does not walk under it. Field
    /// 2026-10-30 12:20:22–40: the prior fell 180,484 → 164,076 → 149,160 while
    /// `goodput_bps` read 180,484, and once the goodput expired it went on down
    /// to 84,195, which the session then wrote to the pair memory as evidence.
    #[test]
    fn a_stall_that_cuts_does_not_walk_the_prior() {
        let start = Instant::now();
        let mut t = start;
        let mut g = v5b_governor(start, None);
        let sink = g.goodput_sink();
        for _ in 0..12 {
            sink.record(1_500, Duration::from_millis(12));
        }
        v5b_reported(&mut g, &mut t, 0);
        assert_eq!(
            g.blocked_send_bps(t),
            Some(1_000_000),
            "the one measurement"
        );
        assert_eq!(g.prior_bps(), Some(1_000_000), "the prior anchors on it");
        let before = v5b_tick(&mut g, t);
        for _ in 0..2 {
            v5b_gap(&mut g, &mut t);
        }
        assert!(
            v5b_tick(&mut g, t) < before,
            "the stall above it still cuts"
        );
        assert_eq!(
            g.prior_bps(),
            Some(1_000_000),
            "two numberless stalls must not move the prior"
        );
    }

    /// FR-79 V5b, the second half. Field 2026-10-30 11:48:36, CORPLAP-2 on
    /// `0.4.187`: one 102 ms blocked send folded as `goodput_bps=223,794`, and
    /// two seconds later the viewer reported 1,199,442 arriving. The path
    /// delivered 5.39 M seven minutes on. The reading stood its 60 s TTL
    /// anyway: the target fell 3.0 → 0.68 M in 22 s at a paint age of 50–104 ms,
    /// through V5's cut and the queue budget denominated in the same number.
    ///
    /// A reading below what the path then delivered is the reading being wrong.
    #[test]
    fn a_delivery_after_a_reading_takes_the_reading_out() {
        let start = Instant::now();
        let mut t = start;
        let mut g = v5b_governor(start, None);
        v5b_fold_223794(&mut g, &mut t);
        let before = v5b_tick(&mut g, t);
        v5b_reported(&mut g, &mut t, v5b_delivery(1_199_442));
        assert_eq!(g.belief_bps(t).1, Some(1_199_442), "the floor rose");
        assert_eq!(
            g.pipe_bps(t, true),
            Some(1_199_442),
            "what the path delivered stands in, and the reading is gone"
        );
        for _ in 0..2 {
            v5b_gap(&mut g, &mut t);
        }
        assert_eq!(
            v5b_tick(&mut g, t),
            before,
            "with the reading refuted a stall has no claim to contradict"
        );
    }

    /// FR-79 V5b — the control the rule above must not break, and why it cannot
    /// be a plain `reading < floor`. Field 2026-10-30 12:19:52, the same
    /// laptop twenty minutes later: 1,546,425 delivered nine seconds BEFORE a
    /// reading of 180,484, at a paint age of 6.1 s, on a path that went on
    /// stalling at 200–253 kbps. That reading was true, and it sat at 0.23 of
    /// the floor, where the false 12:04:06 one sat at 0.39, so a comparison
    /// against the floor alone would throw out the true one and could keep the
    /// false one. What tells them apart is the order: nothing delivered AFTER
    /// the 12:19:52 reading raised the floor, for the whole of its 60 s TTL.
    #[test]
    fn a_delivery_before_a_reading_does_not_refute_it() {
        let start = Instant::now();
        let mut t = start;
        let mut g = v5b_governor(start, None);
        v5b_reported(&mut g, &mut t, v5b_delivery(1_546_425));
        // 1,500 bytes over 66.5 ms is 180,451 bps, the field's reading to 0.02 %.
        g.goodput_sink()
            .record(1_500, Duration::from_micros(66_500));
        v5b_reported(&mut g, &mut t, 0);
        let reading = g.blocked_send_bps(t).expect("the reading");
        assert!(
            u64::from(reading) * 2 < u64::from(g.belief_bps(t).1.expect("a floor")),
            "the premise: the reading sits far under the floor"
        );
        let before = v5b_tick(&mut g, t);
        // What arrives after it stays under the damped floor: twice the reading
        // and more, but the floor does not rise, as it did not in the field.
        for rx in [225_000, 253_125, 400_000] {
            v5b_reported(&mut g, &mut t, v5b_delivery(rx));
        }
        assert_eq!(g.pipe_bps(t, true), Some(reading), "the reading stands");
        assert_eq!(g.blocked_send_bps(t), Some(reading));
        for _ in 0..2 {
            v5b_gap(&mut g, &mut t);
        }
        assert!(
            v5b_tick(&mut g, t) < before,
            "and a stall above it still cuts"
        );
    }

    /// FR-79 V5b — FR-79's own law: no consumer acts on a rejected window. A
    /// stall's shadow is its backlog draining, so a burst there refutes nothing,
    /// though V3b still records it as a delivery.
    #[test]
    fn a_delivery_in_a_rejected_window_refutes_nothing() {
        let start = Instant::now();
        let mut t = start;
        let mut g = v5b_governor(start, None);
        v5b_fold_223794(&mut g, &mut t);
        v5b_gap(&mut g, &mut t);
        // The next window is in the stall's shadow.
        v5b_reported(&mut g, &mut t, v5b_delivery(5_000_000));
        assert_eq!(
            g.evidence_rejected()[2],
            1,
            "rejected as the stall's shadow"
        );
        assert_eq!(
            g.belief_bps(t).1,
            Some(5_000_000),
            "V3b records the delivery"
        );
        assert_eq!(g.blocked_send_bps(t), Some(223_794), "the reading stands");
        assert_eq!(g.pipe_bps(t, true), Some(223_794));
    }

    /// FR-79 V5b — the factor. A delivery that raises the floor but does not
    /// reach twice the reading says the reading was lumpy, not wrong: FR-59 P6's
    /// `CONTRADICTION_FACTOR`, the same law in the other direction.
    #[test]
    fn a_delivery_under_twice_the_reading_refutes_nothing() {
        let start = Instant::now();
        let mut t = start;
        let mut g = v5b_governor(start, None);
        v5b_fold_223794(&mut g, &mut t);
        v5b_reported(&mut g, &mut t, v5b_delivery(447_587));
        assert_eq!(g.belief_bps(t).1, Some(447_587), "the floor rose");
        assert_eq!(g.blocked_send_bps(t), Some(223_794), "one bit short of 2×");
        v5b_reported(&mut g, &mut t, v5b_delivery(447_588));
        assert_eq!(g.blocked_send_bps(t), None, "exactly 2× refutes it");
    }

    /// FR-79 V5b — a refutation takes out one reading, not the source. The next
    /// blocked send is a reading in its own right, adopted outright rather than
    /// folded over the number the path disproved, and a true collapse after a
    /// false reading is still believed.
    #[test]
    fn a_reading_after_a_refutation_counts_on_its_own_merits() {
        let start = Instant::now();
        let mut t = start;
        let mut g = v5b_governor(start, None);
        v5b_fold_223794(&mut g, &mut t);
        v5b_reported(&mut g, &mut t, v5b_delivery(1_199_442));
        assert_eq!(g.readings_refuted(), 1);
        assert_eq!(g.blocked_send_bps(t), None, "the reading is gone");
        v5b_fold_223794(&mut g, &mut t);
        assert_eq!(g.measured_bps(t, true), Some(223_794), "a new one stands");
        assert_eq!(g.readings_refuted(), 1, "nothing refuted it yet");
    }

    /// FR-79 V5b — the source a stall is judged against: the goodput, or the
    /// viewer's arrival rate inside its 15 s TTL, the lower of the two. Never the
    /// stand-in, which `pipe_bps` still offers everything else. Nothing here is
    /// measured on a direct window, as `pipe_bps` has always said.
    #[test]
    fn the_measured_source_is_the_readings_and_never_the_stand_in() {
        let start = Instant::now();
        let mut t = start;
        let mut g = v5b_governor(start, None);
        assert_eq!(g.measured_bps(t, true), None, "nothing yet");
        for i in 1..=2 {
            let growing = viewer_rate::pack_link(400_000, 250);
            v5b_sending(&mut g, &mut t, growing, i * 137_500);
        }
        assert_eq!(g.measured_bps(t, true), Some(400_000), "the arrival rate");
        assert_eq!(g.measured_bps(t, false), None, "nothing on a direct window");
        let expired = t + LINK_CEILING_TTL + Duration::from_secs(1);
        assert_eq!(g.measured_bps(expired, true), None, "past its TTL");
        assert_eq!(
            g.pipe_bps(expired, true),
            Some(400_000),
            "where the stand-in still answers for the rest of the session"
        );
    }

    /// FR-79 V5b — the arrival-rate clamp is a reading too. Field 2026-10-30
    /// 12:04:06: the viewer's arrival of 581,546 in a growing window clamped the
    /// target 3.19 → 0.52 M, and 4,657,230 was delivered two seconds later, at
    /// a paint age of 187 ms. The clamp stood until its age and depth released
    /// it, and the target took 80 s to climb back.
    #[test]
    fn a_delivery_lifts_the_arrival_rate_clamp_it_contradicts() {
        let start = Instant::now();
        let mut t = start;
        let mut g = v5b_governor(start, None);
        // The field's 1,107,067 bps offered, over these 1.1 s windows.
        for i in 1..=2 {
            let growing = viewer_rate::pack_link(581_546, 250);
            v5b_sending(&mut g, &mut t, growing, i * 152_222);
        }
        assert!(
            g.link_loop_holding(),
            "the premise: two growing windows arm it"
        );
        assert_eq!(v5b_tick(&mut g, t), 523_391, "the field's 0.9 × 581,546");
        // Elevated age, so the ordinary release cannot be what lifts it; a
        // viewer-late window, valid, as the field's was.
        t += Duration::from_millis(1100);
        g.note_window_sender(v5b_sender());
        g.tick_viewer_window(
            t,
            30,
            || viewer_rate::pack_report(30, false),
            || viewer_rate::pack_age_with_arrival(187, 68, 80, 70),
            || v5b_delivery(4_657_230),
            true,
            |o| o,
            0,
        )
        .expect("window due");
        assert!(!g.link_loop_holding(), "the delivery refuted what it held");
        assert_eq!(
            g.prior_bps(),
            None,
            "a delivery past the band ends the prior: the unremembered session"
        );
        // Two frames: `set_floor` caps at the ceiling the last tick left, so the
        // floor only reaches the target on the tick after the clamp lifts.
        v5b_tick(&mut g, t);
        assert_eq!(
            v5b_tick(&mut g, t),
            crate::encode::MIN_BITRATE_BPS,
            "back at the nominal floor within a frame, the climb continues from there"
        );
    }

    /// FR-79 V5b — a remembered rate is the same claim with less behind it. Field
    /// 2026-09-04 (`6a9abc30`): 200 kbps in the memory held a session there for
    /// four minutes through the floor relief and the queue budget.
    #[test]
    fn a_delivery_re_anchors_a_seed_it_contradicts() {
        let start = Instant::now();
        let mut t = start;
        let mut g = v5b_governor(start, Some(200_000));
        assert_eq!(g.prior_bps(), Some(200_000), "the premise");
        v5b_reported(&mut g, &mut t, v5b_delivery(1_199_442));
        assert_eq!(g.prior_bps(), Some(1_199_442));
    }

    /// FR-79 V5b q3. Field 2026-10-27 15:29, CORPLAP-2 on `0.4.178`: a 130 s
    /// session on an idle desktop (0.16–0.25 Mbps) never stepped and never cut,
    /// so `stable_bps()` was its own opener, 0.85 × the memory, and the write-back
    /// took half the gap: −7.5 % for having measured nothing. Over 2026-10-25 →
    /// 10-30 all 18 writes were rungs of the session's own opener, and the entry
    /// lost 43 % while those sessions delivered up to 10.2 M.
    #[test]
    fn a_hold_nothing_carried_is_not_evidence() {
        let start = Instant::now();
        let mut t = start;
        let mut g = v5b_governor(start, Some(4_760_000));
        let opener = v5b_tick(&mut g, t);
        assert_eq!(opener, 4_046_000, "the premise: 0.85 × the memory");
        let mut sent = 0u64;
        for _ in 0..15 {
            sent += 30_000; // 0.22 Mbps over a 1.1 s window: an idle desktop
            t += Duration::from_millis(1100);
            g.note_window_sender(v5b_sender());
            g.tick_viewer_window(
                t,
                30,
                || viewer_rate::pack_report(30, false),
                || viewer_rate::pack_age_with_arrival(70, 68, 80, 69),
                || 0,
                true,
                |o| o,
                sent,
            )
            .expect("window due");
            v5b_tick(&mut g, t);
        }
        assert_eq!(g.applied_bps(), opener, "the premise: the target held");
        assert_eq!(
            g.stable_bps(),
            None,
            "a hold nothing carried is no evidence"
        );
    }

    /// The control for the cell above: the same hold, with one valid window that
    /// carried it. FR-35's `CARRIED_PCT`, the learner's own test for the same
    /// question.
    #[test]
    fn a_hold_a_valid_window_carried_is_evidence() {
        let start = Instant::now();
        let mut t = start;
        let mut g = v5b_governor_hi(start, 4_046_000, Some(4_760_000));
        let opener = v5b_tick(&mut g, t);
        let mut sent = 0u64;
        for i in 0..15 {
            // 4,046,000 × 70 % × 1.1 s ÷ 8 = 389,428 bytes in the sixth window.
            sent += if i == 5 { 390_000 } else { 30_000 };
            t += Duration::from_millis(1100);
            g.note_window_sender(v5b_sender());
            g.tick_viewer_window(
                t,
                30,
                || viewer_rate::pack_report(30, false),
                || viewer_rate::pack_age_with_arrival(70, 68, 80, 69),
                || 0,
                true,
                |o| o,
                sent,
            )
            .expect("window due");
            v5b_tick(&mut g, t);
        }
        assert_eq!(g.stable_bps(), Some(opener));
    }

    /// The opener's keyframe carries bytes in a window FR-119 P2b-8 already
    /// rejects, and a session that opens and goes idle would otherwise have
    /// proved its own opener with its own opening.
    #[test]
    fn the_opening_burst_does_not_carry_a_hold() {
        let start = Instant::now();
        let mut t = start;
        let mut g = v5b_governor_hi(start, 4_046_000, Some(4_760_000));
        v5b_tick(&mut g, t);
        let mut sent = 0u64;
        for i in 0..15 {
            sent += if i == 0 { 700_000 } else { 30_000 };
            t += Duration::from_millis(1100);
            g.note_window_sender(WindowSenderStats {
                opening: i == 0,
                ..v5b_sender()
            });
            g.tick_viewer_window(
                t,
                30,
                || viewer_rate::pack_report(30, false),
                || viewer_rate::pack_age_with_arrival(70, 68, 80, 69),
                || 0,
                true,
                |o| o,
                sent,
            )
            .expect("window due");
            v5b_tick(&mut g, t);
        }
        assert_eq!(g.evidence_rejected()[4], 1, "rejected as the opening");
        assert_eq!(g.stable_bps(), None);
    }

    /// FR-79 V5b, the kill switch: `measured_evidence = false` is V5a's rules, all
    /// three of them, on the same drives as the cells above.
    #[test]
    fn with_the_switch_off_the_rules_are_v5a_s() {
        let off = |start: Instant, seed: Option<u32>| {
            RateGovernor::new(
                30,
                DEPTH,
                GovernorFlags {
                    measured_ceiling: false,
                    age_feedback: true,
                    measured_evidence: false,
                    ..GovernorFlags::default()
                },
                8_000_000,
                seed,
                start,
            )
        };
        // A stand-in is enough to cut on, and the stall walks it.
        let start = Instant::now();
        let mut t = start;
        let mut g = off(start, None);
        v5b_reported(&mut g, &mut t, 0);
        g.prior.on_window(Some(19_578), false);
        let before = v5b_tick(&mut g, t);
        for _ in 0..2 {
            v5b_gap(&mut g, &mut t);
            v5b_tick(&mut g, t);
        }
        assert!(g.applied_bps() < before, "the stand-in cuts");
        assert_eq!(g.prior_bps(), Some(17_798), "and the stall walks it");
        // A delivery takes out nothing.
        let start = Instant::now();
        let mut t = start;
        let mut g = off(start, None);
        v5b_fold_223794(&mut g, &mut t);
        v5b_reported(&mut g, &mut t, v5b_delivery(1_199_442));
        assert_eq!(g.blocked_send_bps(t), Some(223_794));
        assert_eq!(g.pipe_bps(t, true), Some(223_794));
        assert_eq!(g.readings_refuted(), 0);
        // A hold nothing carried is still the stable rate.
        let start = Instant::now();
        let mut t = start;
        let mut g = off(start, Some(4_760_000));
        let opener = v5b_tick(&mut g, t);
        for _ in 0..15 {
            v5b_reported(&mut g, &mut t, 0);
            v5b_tick(&mut g, t);
        }
        assert_eq!(g.stable_bps(), Some(opener));
    }

    /// FR-79 V5b — the pair the replay below remembers (`rate_memory::memory_key`).
    const V5B_KEY: &str = "203.0.113.221|relay:turn/udp";
    /// What the pump wrote in one idle window on jupiter: 2,280 B over the 2 s
    /// before the report, about 76 B a frame at 15 fps.
    const V5B_IDLE_BYTES: u64 = 1_140;
    /// The same bytes as the viewer's arrival rate.
    const V5B_IDLE_RX: u32 = 9_120;

    /// FR-79 V5b — one relayed session of the replay: the pump's clock and running
    /// byte total, a 3 M plan, an 8 M `hi`, the viewer's paint floor at 17 ms.
    struct V5bSession {
        g: RateGovernor,
        t: Instant,
        sent: u64,
    }

    impl V5bSession {
        fn open(measured_evidence: bool, seed: Option<u32>) -> Self {
            let t = Instant::now();
            let mut g = RateGovernor::new(
                30,
                DEPTH,
                GovernorFlags {
                    measured_evidence,
                    ..GovernorFlags::default()
                },
                8_000_000,
                seed,
                t,
            );
            g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
            Self { g, t, sent: 0 }
        }

        /// One second: the pump wrote `wrote` bytes, the viewer reported `link`
        /// at a paint age of `age` (`None` = it said nothing at all), and the
        /// pump's tick follows.
        fn window(
            &mut self,
            wrote: u64,
            age: Option<u16>,
            link: u64,
            opening: bool,
        ) -> ViewerWindow {
            self.window_over(Duration::from_secs(1), wrote, age, link, opening)
        }

        /// [`Self::window`], `dt` long.
        fn window_over(
            &mut self,
            dt: Duration,
            wrote: u64,
            age: Option<u16>,
            link: u64,
            opening: bool,
        ) -> ViewerWindow {
            self.t += dt;
            self.sent += wrote;
            self.g.note_window_sender(WindowSenderStats {
                opening,
                ..v5b_sender()
            });
            let w = self
                .g
                .tick_viewer_window(
                    self.t,
                    30,
                    || age.map_or(0, |_| viewer_rate::pack_report(30, false)),
                    || age.map_or(0, |a| viewer_rate::pack_age_with_arrival(a, 17, 30, a - 2)),
                    || link,
                    true,
                    |o| o,
                    self.sent,
                )
                .expect("window due");
            self.g.pre_encode_tick(
                3_000_000,
                crate::encode::MIN_BITRATE_BPS,
                true,
                DEPTH,
                self.t,
            );
            same_numbers(&self.g, self.t);
            w
        }

        /// An idle window: the trickle, reported at paint age `age`.
        fn idle(&mut self, age: u16) -> ViewerWindow {
            let link = viewer_rate::pack_link(V5B_IDLE_RX, 0);
            self.window(V5B_IDLE_BYTES, Some(age), link, false)
        }

        /// A silent window with the pump still sending: a transit stall.
        fn stall(&mut self) {
            self.window(V5B_IDLE_BYTES, None, 0, false);
            assert_eq!(
                self.g.pipe_state(),
                Some(crate::encode::pipe_state::PipeState::TransitStalled)
            );
        }

        fn evidence(&self) -> Option<u32> {
            self.g.session_evidence_bps(self.t, true)
        }

        /// Field 21:05:19–21:06:25: the screen moved for a minute. 10.7 Mbps
        /// delivered, the opener's three windows rejected, the learner stepping
        /// every 5 s, and the session's peak delivery at the end of it.
        fn load(&mut self) {
            let moving = viewer_rate::pack_link(10_700_000, 0);
            for i in 0..55 {
                self.window(1_337_500, Some(28), moving, i < 3);
            }
            let peak = viewer_rate::pack_link(15_915_240, 0);
            self.window(1_337_500, Some(28), peak, false);
        }

        /// Field 21:06:26–21:06:32: the screen stopped, a transit stall at
        /// 21:06:31, and then the two windows that read a growing queue on idle
        /// traffic, the first in the stall's shadow and the second valid.
        fn idle_report(&mut self) -> ViewerWindow {
            for _ in 0..3 {
                self.idle(68);
            }
            self.stall();
            let growing = viewer_rate::pack_link(9_489, 145);
            self.window(V5B_IDLE_BYTES, Some(216), growing, false);
            self.window(V5B_IDLE_BYTES, Some(216), growing, false)
        }

        /// Field 21:06:33–21:07:32. The arrival rate's 15 s first, at an elevated
        /// age so nothing releases it early. Then one letter a window, from the
        /// heartbeats' counters: a heartbeat spans two windows, so the mix is the
        /// field's and the order inside a pair is not. `S` a stall, `C` a clean
        /// report, `E` a report at an elevated age. The window after an `S` is
        /// rejected as its shadow, whatever it says. Returns each value the
        /// stand-in took.
        fn walk(&mut self) -> Vec<Option<u32>> {
            for _ in 0..15 {
                self.idle(150);
            }
            let mut seen = vec![self.g.prior_bps()];
            for window in "SSSSSSSSCECCCSSSSSSCESSSCCCSSCEE".chars() {
                match window {
                    'S' => self.stall(),
                    'C' => {
                        self.idle(56);
                    }
                    _ => {
                        self.idle(150);
                    }
                }
                if seen.last() != Some(&self.g.prior_bps()) {
                    seen.push(self.g.prior_bps());
                }
            }
            seen
        }
    }

    /// FR-79 V5b, the replay, with the rules that shipped: the switch off. Field
    /// 2026-11-02 21:05:18–21:07:34, jupiter on `0.4.197`, one relayed `h264_vaapi`
    /// session and the sessions opened from what it wrote. It never lost a byte to
    /// a slow path, and it left the pair remembered at 3.3 kbps:
    ///
    /// 1. A minute at 10.7 Mbps, then the screen stopped.
    /// 2. The viewer's queue read as growing on idle traffic, so the arrival rate
    ///    of that trickle became the pipe. The target fell 6.2 M → 200 kbps and
    ///    FR-59 P6 abandoned the ceiling the session had just earned.
    /// 3. The session walked that reading down and wrote the last rung to the pair
    ///    memory as evidence.
    /// 4. The next sessions opened at that number.
    /// 5. Each wrote its own stand-in back, which refreshed the 7-day TTL, so the
    ///    key held itself low with nothing behind it.
    ///
    /// Every number here is the field's, to the unit. This arm stays: it pins the
    /// switch, and it is what says the replay is the session.
    #[test]
    fn the_jupiter_chain_replays_to_the_unit_with_the_switch_off() {
        let mut s = V5bSession::open(false, None);
        s.load();
        assert_eq!(s.g.learned_ceiling_bps(), 6_209_663, "twelve steps");
        assert_eq!(s.g.prior_bps(), None, "nothing measured the pipe");
        // Step 2.
        let w = s.idle_report();
        assert!(w.link_congested);
        assert_eq!(w.link_ceiling_bps, Some(8_540));
        assert_eq!(
            s.g.evidence_rejected()[2],
            1,
            "its first window, the shadow"
        );
        assert_eq!(s.g.applied_bps(), 200_000, "target at the floor");
        assert_eq!(s.g.take_seed_abandoned(), Some(6_209_663));
        assert_eq!(s.g.learned_ceiling_bps(), 0);
        assert_eq!(s.g.prior_bps(), Some(9_489));
        assert_eq!(
            s.g.belief_bps(s.t),
            (Some(7_962_364), Some(7_962_364), Some(9_489)),
            "the heartbeat's `pipe_belief` at 21:06:33"
        );
        // Step 3: the rungs, ×10/11 with integer floors.
        let rungs = s.walk();
        let rungs: Vec<u32> = rungs.into_iter().flatten().collect();
        assert_eq!(
            rungs,
            [
                9_489, 8_626, 7_841, 7_128, 6_480, 5_890, 5_354, 4_867, 4_424, 4_021, 3_655, 3_322
            ]
        );
        assert_eq!(s.g.measured_bps(s.t, true), None, "nothing live at the end");
        let evidence = s.evidence();
        assert_eq!(evidence, Some(3_322));
        let mut memory = super::super::rate_memory::RateMemory::default();
        let at = 1_793_000_000u64;
        assert_eq!(memory.record_session(V5B_KEY, evidence, at), 3_322);
        // Steps 4 and 5, twice: each session opens low, takes one rung off the
        // seed (×4/5) and writes it back, damped to half the gap. The field's
        // 3,322 → 2,989 → 2,690.
        for (session, (stand_in, kept)) in [(2_657, 2_989), (2_391, 2_690)].into_iter().enumerate()
        {
            let at = at + 600 * (session as u64 + 1);
            let seed = memory.seed_for(V5B_KEY, at);
            let mut next = V5bSession::open(false, seed);
            assert_eq!(next.g.open_seed_bps(), seed);
            assert_eq!(next.g.applied_bps(), 200_000, "opens at the floor");
            next.idle(150);
            next.idle(150);
            assert_eq!(next.evidence(), Some(stand_in));
            memory.record_session(V5B_KEY, next.evidence(), at);
            let entry = &memory.entries[V5B_KEY];
            assert_eq!(entry.stable_bps, kept);
            assert_eq!(entry.at_unix, at, "the TTL refreshed on nothing");
        }
    }

    /// FR-79 V5b — the replay's step 2 with the rules on. A queue that grew on
    /// 1,140 B a window says nothing about the rate: the sender was not offering
    /// anything for the path to refuse, so the trickle's arrival rate is the
    /// content's weight, the case `viewer_rate::LinkLoop`'s own warning describes.
    #[test]
    fn the_jupiter_chain_takes_no_reading_from_idle_traffic() {
        let mut s = V5bSession::open(true, None);
        s.load();
        let before = s.g.applied_bps();
        let w = s.idle_report();
        assert!(!w.link_congested, "nothing the sender offered");
        assert_eq!(w.link_ceiling_bps, None);
        assert_eq!(s.g.link_unoffered(), 1, "and the heartbeat says so");
        assert!(!s.g.link_loop_holding(), "no clamp to the trickle");
        assert_eq!(s.g.measured_bps(s.t, true), None, "and no reading");
        assert_eq!(s.g.take_seed_abandoned(), None);
        assert_eq!(s.g.learned_ceiling_bps(), 6_209_663, "the ceiling stands");
        assert!(s.g.applied_bps() >= before, "the target stands");
        assert_eq!(
            s.g.belief_bps(s.t).1,
            Some(15_915_240),
            "no capacity sample took the floor down"
        );
    }

    /// FR-79 V5b — the replay's step 3 with the rules on: a session whose only
    /// "reading" was idle traffic leaves the memory alone.
    #[test]
    fn the_jupiter_chain_writes_nothing_at_session_end() {
        let mut s = V5bSession::open(true, None);
        s.load();
        s.idle_report();
        s.walk();
        assert_eq!(s.evidence(), None);
        let mut memory = super::super::rate_memory::RateMemory::default();
        assert_eq!(
            memory.record_session(V5B_KEY, s.evidence(), 1_793_000_000),
            0
        );
        assert!(memory.entries.is_empty());
    }

    /// FR-79 V5b — the replay's step 5 with the rules on, from a key an earlier
    /// build already poisoned: a session that opens from it and measures nothing
    /// leaves its value and its age, so the entry runs out rather than holding
    /// itself low.
    #[test]
    fn the_jupiter_chain_does_not_refresh_a_poisoned_key() {
        let mut memory = super::super::rate_memory::RateMemory::default();
        let written = 1_793_000_000u64;
        memory.record_session(V5B_KEY, Some(3_322), written);
        let at = written + 600;
        let mut s = V5bSession::open(true, memory.seed_for(V5B_KEY, at));
        assert_eq!(
            s.g.open_seed_bps(),
            Some(3_322),
            "step 4 stands: it opens low"
        );
        s.idle(150);
        s.idle(150);
        assert_eq!(s.evidence(), None, "a seed is not a measurement");
        memory.record_session(V5B_KEY, s.evidence(), at);
        let entry = &memory.entries[V5B_KEY];
        assert_eq!(entry.stable_bps, 3_322);
        assert_eq!(entry.at_unix, written, "its age stands");
    }

    /// FR-148 P0 — the `path model` line's numbers are the heartbeat's, accessor
    /// by accessor, whatever the tags say; and the learned ceiling's tag is for
    /// the value it has. Every window of a [`V5bSession`] checks this.
    fn same_numbers(g: &RateGovernor, t: Instant) {
        let v = g.path_view(t, None);
        let bps = |n: Option<PathNumber>| n.map(|n| n.bps);
        let constrained = g.path.constrained;
        assert_eq!(bps(v.goodput), g.blocked_send_bps(t), "goodput_bps");
        assert_eq!(
            (bps(v.belief.0), bps(v.belief.1), bps(v.belief.2)),
            g.belief_bps(t),
            "pipe_belief"
        );
        assert_eq!(
            bps(v.measured),
            g.measured_bps(t, constrained),
            "pipe_measured"
        );
        assert_eq!(bps(v.prior), g.prior_bps(), "prior_bps");
        let learned = g.learned_ceiling_bps();
        assert_eq!(bps(v.learned_ceiling), Some(learned), "learned_ceiling_bps");
        assert_eq!(g.path.learned.0, learned, "the learned ceiling's tag");
        assert_eq!(
            bps(v.slow_link_floor),
            g.relieved_floor_bps(),
            "slow_link_floor_bps"
        );
        let pipe = g.pipe_tagged(t, constrained).map(|(bps, _)| bps);
        assert_eq!(pipe, g.pipe_bps(t, constrained), "pipe_bps");
    }

    /// FR-148 P0 — the check both field chains below fail on the laws of
    /// 2026-11-16: in neither session did a backlogged reading say the path
    /// carried less than it had delivered, so the floor stands at the most it
    /// delivered, and nothing offered to the pair memory is below that.
    fn keeps_what_it_delivered(floor: Option<u32>, offer: Option<u32>, delivered: u32) -> bool {
        floor.is_some_and(|f| f >= delivered) && offer.is_none_or(|o| o >= delivered)
    }

    /// A number's value and word, for the cells below.
    fn word(n: Option<PathNumber>) -> Option<(u32, &'static str)> {
        n.map(|n| (n.bps, n.word()))
    }

    /// FR-148 P0 (AC3) — the jupiter chain of 2026-11-02, the replay above, read
    /// with provenance. On the laws of 2026-11-16 (`measured_evidence` off) the
    /// trickle's push-back halved the floor the busy minute set, and the session
    /// offered the stand-in's last rung to the pair memory. The line says what
    /// those numbers were: an application-limited push-back and an assumed
    /// stand-in. The model that reads the tags keeps the floor and offers it.
    /// Nothing the session acts on moved: the switch-off cell above still pins
    /// the field's numbers to the unit.
    #[test]
    fn the_jupiter_chain_read_with_provenance_keeps_its_floor() {
        let delivered = 15_915_240;
        let mut s = V5bSession::open(false, None);
        s.load();
        s.idle_report();
        s.walk();
        let t = s.t;
        // The laws of 2026-11-16.
        let (_, floor, capacity) = s.g.belief_bps(t);
        assert_eq!((floor, capacity), (Some(7_962_364), Some(9_489)));
        assert_eq!(s.evidence(), Some(3_322));
        assert!(!keeps_what_it_delivered(floor, s.evidence(), delivered));
        // What the line says of them.
        let v = s.g.path_view(t, None);
        assert_eq!(
            word(v.belief.2),
            Some((9_489, "app-limited")),
            "the push-back"
        );
        assert_eq!(word(v.belief.1), Some((7_962_364, "app-limited")));
        assert_eq!(word(v.prior), Some((3_322, "assumed")), "what was offered");
        assert_eq!(word(v.learned_ceiling), Some((0, "assumed")), "abandoned");
        assert_eq!(word(v.viewer_rx), Some((V5B_IDLE_RX, "app-limited")));
        assert_eq!(v.measured, None, "nothing live at the end");
        // Read with provenance.
        let model_floor = v.model_floor.expect("a floor");
        assert_eq!(word(Some(model_floor)), Some((delivered, "app-limited")));
        assert_eq!(v.model_capacity, None, "nothing backlogged pushed back");
        assert_eq!(
            v.model_stand_in,
            Some(model_floor),
            "the stand-in stands at it"
        );
        let offer = s.g.belief.model().offer(t);
        assert_eq!(offer, Some(model_floor));
        assert!(keeps_what_it_delivered(
            Some(model_floor.bps),
            offer.map(|o| o.bps),
            delivered
        ));
    }

    /// FR-148 P0 — a pair the replay's CORPLAP-2 session writes (an invented
    /// documentation address).
    const CORPLAP2_KEY: &str = "198.51.100.40|relay:derp/tcp";

    /// FR-148 P0 — the CORPLAP-2 heartbeats of 2026-11-11
    /// (`tests/fixtures/fr148-corplap2-20261111.txt`): each interval between two
    /// heartbeats, the rate the pump wrote over it, and the stand-in the second
    /// one reported. Times are read in tenths of a second, so no interval is a
    /// float's rounding off its length.
    fn corplap2_20261111() -> Vec<(Duration, u32, Option<u32>)> {
        let mut rows = include_str!("../../tests/fixtures/fr148-corplap2-20261111.txt")
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| {
                let f: Vec<&str> = l.split(' ').collect();
                let tenths: u64 = f[0].replace('.', "").parse().expect("seconds");
                let bytes: u64 = f[1].parse().expect("bytes_written");
                (tenths, bytes, f[2].parse::<u32>().ok())
            });
        let mut last = rows.next().expect("a first heartbeat");
        rows.map(|row| {
            let tenths = row.0 - last.0;
            let rate = u32::try_from((row.1 - last.1) * 80 / tenths).expect("a rate");
            last = row;
            (Duration::from_millis(tenths * 100), rate, row.2)
        })
        .collect()
    }

    /// FR-148 P0 (AC3) — FR-148 §2.1, where a placeholder became the pair's
    /// remembered rate: CORPLAP-2, 2026-11-11, one relayed AV1 session of 45
    /// minutes, replayed from its heartbeats on the laws of 2026-11-16.
    ///
    /// The path delivered 1,399,272 bit/s at 394.9 s. At 909.1 s the viewer's
    /// queue read as growing while the pump wrote 498,304, and its arrival rate,
    /// 564,865, became the reading: an application-limited one, since the pump
    /// was offering no more than arrived. Nothing measured after it. The stand-in
    /// walked on the paint ages, 178 single steps of the prior's law to 179,980,
    /// and the session wrote that to the pair memory.
    ///
    /// Two windows an interval, the bytes the pump wrote taken as arrived. The
    /// rates and the stand-in's 179 values are the field's. The windows between
    /// two steps are not (10 clean windows a step up, 2 pushed back a step down,
    /// at the replay's paint ages), and neither is the anchor's queue growth.
    #[test]
    fn the_corplap2_chain_read_with_provenance_keeps_its_floor() {
        let intervals = corplap2_20261111();
        assert_eq!(intervals.len(), 1_347);
        let delivered = intervals.iter().map(|i| i.1).max().expect("a rate");
        assert_eq!(delivered, 1_399_272);
        // The stand-in as the field's heartbeats reported it, each value once.
        let mut ladder: Vec<u32> = Vec::new();
        for prior in intervals.iter().filter_map(|i| i.2) {
            if ladder.last() != Some(&prior) {
                ladder.push(prior);
            }
        }
        assert_eq!(ladder.len(), 179);
        assert_eq!((ladder[0], ladder[178]), (564_865, 179_980));
        let anchor = intervals
            .iter()
            .position(|i| i.2.is_some())
            .expect("a prior");
        assert_eq!(anchor, 447, "the interval ending at 909.1 s");
        let mut s = V5bSession::open(false, None);
        let mut halves = intervals
            .iter()
            .flat_map(|&(dt, rate, _)| [(dt / 2, rate); 2]);
        let mut used = 0usize;
        let mut window = |s: &mut V5bSession, age: u16, link: Option<u64>| {
            let (dt, rate) = halves.next().expect("an interval left");
            used += 1;
            let wrote = u64::from(rate) * dt.as_millis() as u64 / 8_000;
            let link = link.unwrap_or(viewer_rate::pack_link(rate, 0));
            s.window_over(dt, wrote, Some(age), link, false);
        };
        for _ in 0..2 * anchor {
            window(&mut s, 56, None);
        }
        assert_eq!(s.g.prior_bps(), None, "nothing measured before 909.1 s");
        let growing = viewer_rate::pack_link(564_865, 145);
        window(&mut s, 224, Some(growing));
        window(&mut s, 224, Some(growing));
        let read_at = s.t;
        assert!(s.g.link_loop_holding());
        assert_eq!(s.g.prior_bps(), Some(564_865));
        let reading = s.g.path_view(read_at, None).measured;
        assert_eq!(word(reading), Some((564_865, "app-limited")), "the reading");
        // It holds while the clamp does, then walks.
        while s.g.link_loop_holding() {
            window(&mut s, 56, None);
        }
        for step in ladder.windows(2) {
            let (age, windows) = if step[1] > step[0] {
                (56, 10)
            } else {
                (150, 2)
            };
            for _ in 0..windows {
                window(&mut s, age, None);
            }
            assert_eq!(s.g.prior_bps(), Some(step[1]));
        }
        let t = s.t;
        // The laws of 2026-11-16.
        assert_eq!(s.g.stable_bps(), None);
        assert_eq!(s.evidence(), Some(179_980));
        let mut memory = super::super::rate_memory::RateMemory::default();
        let written = memory.record_session(CORPLAP2_KEY, s.evidence(), 1_794_000_000);
        assert_eq!(written, 179_980);
        let floor = s.g.belief_bps(t).1;
        assert_eq!(floor, Some(982_068), "damped toward the reading");
        assert!(!keeps_what_it_delivered(floor, s.evidence(), delivered));
        // What the line says of them: the stand-in is assumed, as old as the
        // reading it walked from.
        let v = s.g.path_view(t, None);
        let prior = v.prior.expect("the stand-in");
        assert_eq!(word(Some(prior)), Some((179_980, "assumed")));
        assert_eq!(prior.age, t - read_at);
        // Read with provenance.
        let model_floor = v.model_floor.expect("a floor");
        assert_eq!(word(Some(model_floor)), Some((delivered, "app-limited")));
        assert_eq!(v.model_capacity, None, "nothing backlogged pushed back");
        assert_eq!(
            v.model_stand_in,
            Some(model_floor),
            "the stand-in stands at it"
        );
        let offer = s.g.belief.model().offer(t);
        assert_eq!(offer, Some(model_floor));
        assert!(keeps_what_it_delivered(
            Some(model_floor.bps),
            offer.map(|o| o.bps),
            delivered
        ));
        // The rest of the session, not replayed, delivered less than the floor.
        let rest = &intervals[used.div_ceil(2)..];
        assert!(!rest.is_empty());
        assert!(rest.iter().all(|i| i.1 < delivered));
    }

    /// FR-148 P1 — a time the pair memory's cells close at.
    const P1_NOW_UNIX: u64 = 1_795_000_000;

    /// FR-148 P1 (D24) — the pair memory takes a delivery only from a window the
    /// validity gate accepted. The opener's burst (rejected `Opening`) and the
    /// drain in a stall's shadow (rejected `StallShadow`) each deliver more than
    /// the path carries once settled, and neither reaches the floor a close would
    /// write. `Pipe`'s own floor still takes every window, as #3995 pinned it.
    /// Master's model took every window too: its twin here, fed the same
    /// deliveries, would offer the burst.
    #[test]
    fn the_opener_burst_and_a_stall_drain_set_no_stored_floor() {
        use crate::encode::pair_memory::FloorBasis;
        use crate::encode::pipe::PathModel;
        let mut s = V5bSession::open(true, None);
        s.g.open_memory(None, P1_NOW_UNIX, s.t);
        let start = s.t;
        let mut master = PathModel::default();
        let mut window = |s: &mut V5bSession, rx: u32, opening: bool| {
            let link = viewer_rate::pack_link(rx, 0);
            s.window(u64::from(rx) / 8, Some(28), link, opening);
            master.delivered(rx, Basis::AppLimited, s.t);
        };
        for _ in 0..3 {
            window(&mut s, 6_000_000, true);
        }
        for _ in 0..5 {
            window(&mut s, 1_200_000, false);
        }
        s.stall();
        window(&mut s, 5_000_000, false);
        window(&mut s, 1_200_000, false);
        let unix = P1_NOW_UNIX + s.t.duration_since(start).as_secs();
        let c = s.g.memory().closing(s.t, unix);
        assert_eq!(
            c.floor.map(|f| (f.bps, f.basis)),
            Some((1_200_000, FloorBasis::AppLimited))
        );
        assert_eq!(c.best, Some((1_200_000, FloorBasis::AppLimited)));
        let p0 = s.g.belief.model().floor(s.t).map(|f| f.bps);
        assert_eq!(p0, Some(1_200_000), "the P0 model is gated the same way");
        assert_eq!(s.g.belief_bps(s.t).1, Some(6_000_000), "Pipe's floor");
        assert_eq!(master.offer(s.t).map(|f| f.bps), Some(6_000_000), "master");
    }

    /// FR-148 P1 (AC7, D24) — a session whose every window the gate rejected
    /// writes nothing: the opener's burst, and then the session ends. Master's
    /// model, fed the same deliveries, would offer the burst as a floor.
    #[test]
    fn a_session_of_rejected_windows_writes_nothing() {
        use crate::encode::pair_memory::{Why, close};
        use crate::encode::pipe::PathModel;
        let mut s = V5bSession::open(true, None);
        s.g.open_memory(None, P1_NOW_UNIX, s.t);
        let start = s.t;
        let mut master = PathModel::default();
        for _ in 0..3 {
            let link = viewer_rate::pack_link(6_000_000, 0);
            s.window(750_000, Some(28), link, true);
            master.delivered(6_000_000, Basis::AppLimited, s.t);
        }
        let unix = P1_NOW_UNIX + s.t.duration_since(start).as_secs();
        let c = s.g.memory().closing(s.t, unix);
        assert!(!c.measured, "{c:?}");
        assert_eq!((c.floor, c.record, c.best), (None, None, None));
        assert_eq!(close(None, &c), (None, Why::NothingMeasured));
        assert_eq!(master.offer(s.t).map(|f| f.bps), Some(6_000_000), "master");
    }

    /// FR-148 D46 B2 — a delivery sample's push-back reaches the shadow model
    /// and the pair memory only, filed under the last window the gate
    /// accepted: the applied rate and `Pipe`'s own law do not move. Before any
    /// window, and after one the gate rejected, it reaches nothing. A stall
    /// the in-flight budget saw keeps the next window out of the model, while
    /// `Pipe`'s own floor still takes it.
    #[test]
    fn a_sample_push_back_moves_only_the_shadow_model() {
        use crate::encode::pipe::PushedBy;
        let mut s = V5bSession::open(true, None);
        s.g.open_memory(None, P1_NOW_UNIX, s.t);
        let model = |s: &V5bSession| {
            let m = s.g.belief.model();
            (
                m.floor(s.t).map(|f| f.bps),
                m.capacity(s.t).map(|c| c.bps),
                m.floor_by(),
            )
        };
        s.g.on_sample_push_back(500_000, PushedBy::P1, s.t);
        assert_eq!(model(&s), (None, None, None), "no window yet");
        let link = viewer_rate::pack_link(1_200_000, 0);
        for _ in 0..5 {
            s.window(150_000, Some(28), link, false);
        }
        assert_eq!(model(&s), (Some(1_200_000), None, None));
        let (applied, own) = (s.g.applied_bps(), s.g.belief_bps(s.t));
        s.g.on_sample_push_back(900_000, PushedBy::P1, s.t);
        let (floor, capacity, by) = model(&s);
        assert!(
            floor.is_some_and(|f| (900_000..1_200_000).contains(&f)),
            "{floor:?}"
        );
        assert_eq!((capacity, by), (Some(900_000), Some(PushedBy::P1)));
        let m = s.g.belief.model();
        assert_eq!(m.capacity_by(s.t), Some(PushedBy::P1));
        assert_eq!((s.g.applied_bps(), s.g.belief_bps(s.t)), (applied, own));
        // The memory filed it once, under the window's take: counted, or set
        // aside as D29 says.
        let filed = |s: &V5bSession| {
            let c = s.g.memory().closing(s.t, P1_NOW_UNIX);
            c.counted + c.not_counted + c.unread + c.viewer_late + c.under_plan
        };
        assert_eq!(filed(&s), 1);
        // A stall the viewer saw: the sample beside it reaches nothing.
        s.stall();
        let before = model(&s);
        s.g.on_sample_push_back(400_000, PushedBy::P3, s.t);
        assert_eq!(model(&s), before);
        assert_eq!(filed(&s), 1);
        // The stall's shadow, then a valid window back at the floor.
        for _ in 0..2 {
            s.window(150_000, Some(28), link, false);
        }
        assert_eq!(model(&s).0, Some(1_200_000));
        // The budget saw a stall: its release reaches `Pipe`'s own floor only.
        s.g.note_path_stall();
        let more = viewer_rate::pack_link(1_500_000, 0);
        s.window(187_500, Some(28), more, false);
        assert_eq!(model(&s).0, Some(1_200_000), "kept out of the model");
        assert_eq!(s.g.belief_bps(s.t).1, Some(1_500_000), "Pipe's own floor");
        s.window(187_500, Some(28), more, false);
        assert_eq!(model(&s).0, Some(1_500_000));
    }

    /// FR-148 D50 — the `path model` line's last fall comes from the shadow
    /// model through `path_view`. With no rate noted (the in-flight budget
    /// off) two delivery samples' push-backs under the floor are damped and
    /// read no fall; with the budget's measured rate noted under the floor,
    /// the next one is a fall, and the view carries the floor before and
    /// after, and its age as it grows. `Pipe`'s own numbers do not move, and
    /// the heartbeat's are the view's at every window (`same_numbers`).
    #[test]
    fn the_path_view_carries_the_models_last_fall() {
        use crate::encode::pipe::{Fall, PathRate, PushedBy};
        let mut s = V5bSession::open(true, None);
        s.g.open_memory(None, P1_NOW_UNIX, s.t);
        let link = viewer_rate::pack_link(2_000_000, 0);
        for _ in 0..5 {
            s.window(250_000, Some(28), link, false);
        }
        let v = s.g.path_view(s.t, None);
        assert_eq!(
            (v.model_floor.map(|f| f.bps), v.model_fell),
            (Some(2_000_000), None)
        );
        let own = s.g.belief_bps(s.t);
        for want in [1_500_000, 1_250_000] {
            s.t += Duration::from_secs(2);
            s.g.on_sample_push_back(1_000_000, PushedBy::P3, s.t);
            let v = s.g.path_view(s.t, None);
            assert_eq!(
                (v.model_floor.map(|f| f.bps), v.model_fell),
                (Some(want), None)
            );
        }
        s.g.note_path_rate(PathRate {
            bps: Some(1_020_000),
            measured: true,
            loss: 0,
            ..PathRate::default()
        });
        s.t += Duration::from_secs(2);
        s.g.on_sample_push_back(1_000_000, PushedBy::P3, s.t);
        let v = s.g.path_view(s.t, None);
        let floor = v.model_floor.expect("a floor");
        assert_eq!((floor.bps, floor.word()), (1_020_000, "backlogged"));
        assert_eq!(v.model_floor_by, Some(PushedBy::P3));
        let fall = Fall {
            from_bps: 1_250_000,
            to_bps: 1_020_000,
            age: Duration::ZERO,
        };
        assert_eq!(v.model_fell, Some(fall));
        let later = s.t + Duration::from_millis(3_400);
        let aged = s.g.path_view(later, None).model_fell.map(|f| f.age);
        assert_eq!(aged, Some(Duration::from_millis(3_400)));
        assert_eq!(s.g.belief_bps(s.t), own, "Pipe's own numbers");
        same_numbers(&s.g, s.t);
    }

    /// FR-148 P1 (D25 corrected) — a slow link replayed through the governor:
    /// arrivals near 0.6 Mbit/s, the viewer's queue growing in runs of three
    /// windows while the pump offers 1 Mbit/s, so the link loop refuses the
    /// second and third of each run, some above the damped floor and some below
    /// it. The clean windows between the runs deliver a little above the floor.
    /// The record counts every refusal but the first and ends trusted, whichever
    /// side of the floor the last refusal was on. On D25's first wording ("a
    /// delivery above the floor ends the record") each clean run ends it, and
    /// the session ends with two samples a second apart: not trusted.
    ///
    /// R3 — the first refusal runs under the session's opening target of
    /// 3 Mbit/s while the pump writes 1 Mbit/s: under half its plan, so the
    /// record does not take it (`under_plan`). The link clamp cuts the target
    /// to 522 kbit/s in that window, and every later refusal is taken. Before
    /// R3 the record held six windows over 17 s.
    #[test]
    fn a_slow_link_ends_trusted_whichever_side_of_the_floor_it_last_refused() {
        use crate::encode::pair_memory::{Closing, trusted};
        let replay = |last_run: u32, first_wording: bool| -> (Closing, u64) {
            let mut s = V5bSession::open(true, None);
            s.g.open_memory(None, P1_NOW_UNIX, s.t);
            if first_wording {
                s.g.belief.memory_mut().set_end_above_floor();
            }
            let start = s.t;
            let clean = |s: &mut V5bSession, rx: u32, windows: usize| {
                for _ in 0..windows {
                    let link = viewer_rate::pack_link(rx, 0);
                    s.window(u64::from(rx) / 8, Some(56), link, false);
                }
            };
            let refused = |s: &mut V5bSession, rx: u32| {
                for _ in 0..3 {
                    let link = viewer_rate::pack_link(rx, 145);
                    s.window(125_000, Some(224), link, false);
                }
            };
            // The floor: 620 k, damped to 590 k by the first run (below it),
            // raised to 610 k, then 640 k by the second run (above it), then
            // 650 k, and the last run either side of that.
            clean(&mut s, 620_000, 10);
            refused(&mut s, 580_000);
            clean(&mut s, 610_000, 5);
            refused(&mut s, 640_000);
            clean(&mut s, 650_000, 5);
            refused(&mut s, last_run);
            let unix = P1_NOW_UNIX + s.t.duration_since(start).as_secs();
            (s.g.memory().closing(s.t, unix), unix)
        };
        for (last_run, side) in [(670_000, "above"), (600_000, "below")] {
            let (c, unix) = replay(last_run, false);
            let r = c.record.expect("a record");
            assert_eq!((r.windows, r.span_s()), (5, 16), "{side}");
            assert_eq!(c.under_plan, 1, "{side}");
            // QR3's split: every counted refusal came from a window the
            // classifier read `Clear`, where only the link report said the
            // queue grew; none from the sender's own counters.
            assert_eq!(
                (
                    c.counted,
                    c.counted_overproduced,
                    c.counted_clear,
                    c.viewer_late
                ),
                (5, 0, 5, 0),
                "{side}"
            );
            assert!(trusted(&r, unix), "{side}");
            assert!(!c.ended, "{side}");
            assert_eq!(c.not_counted, 0, "{side}");
            let (first, unix) = replay(last_run, true);
            assert!(first.ended, "{side}");
            let r = first.record.expect("the last run's samples");
            assert_eq!((r.windows, r.span_s()), (2, 1), "{side}");
            assert!(!trusted(&r, unix), "{side}: the first wording fails it");
        }
    }

    /// FR-148 P1 (the coordinator's ruling on the review's finding 4) — a
    /// window the classifier blames on the viewer is not a reading of the
    /// path. Three runs of refusals as in the slow-link replay (the viewer's
    /// queue growing for three windows while the pump offers 1 Mbit/s into
    /// 580 kbit/s), in four kinds of window: `Clear` (the transit share 168 ms
    /// over its floor), the viewer `struggling` on a flat age, the browser's
    /// own share 150 ms over its floor, and the classifier's tie (the transit
    /// and the browser shares each 300 ms over their floors). The link loop
    /// pushes back on the same windows in the first three, and the session's
    /// belief takes every push-back whatever the window; the memory counts the
    /// late viewer's as `viewer_late` and takes none of them. The tie stays
    /// `TransitStalled` (`transit_excess >= viewer_excess`), the gate rejects
    /// it as before, and nothing reaches either.
    #[test]
    fn a_window_the_classifier_blames_on_the_viewer_is_not_a_reading_of_the_path() {
        use crate::encode::pair_memory::Closing;
        use crate::encode::pipe_state::PipeState;
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Refused {
            Clear,
            Struggling,
            ViewerShare,
            Tie,
        }
        // One window: the pump wrote `wrote` bytes, and the viewer reported
        // `link` and a paint age of `age` ms of which `arrival` ms to arrival.
        fn win(s: &mut V5bSession, wrote: u64, link: u64, age_arrival: (u16, u16), late: bool) {
            s.t += Duration::from_secs(1);
            s.sent += wrote;
            s.g.note_window_sender(v5b_sender());
            s.g.tick_viewer_window(
                s.t,
                30,
                || viewer_rate::pack_report(30, late),
                || viewer_rate::pack_age_with_arrival(age_arrival.0, 17, 30, age_arrival.1),
                || link,
                true,
                |o| o,
                s.sent,
            )
            .expect("window due");
            s.g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, s.t);
        }
        let replay = |how: Refused| -> (Closing, u32, Vec<PipeState>) {
            let mut s = V5bSession::open(true, None);
            s.g.open_memory(None, P1_NOW_UNIX, s.t);
            let start = s.t;
            let mut states = Vec::new();
            for _ in 0..3 {
                for _ in 0..8 {
                    win(
                        &mut s,
                        77_500,
                        viewer_rate::pack_link(620_000, 0),
                        (56, 54),
                        false,
                    );
                }
                let (age_arrival, late) = match how {
                    Refused::Clear => ((224, 222), false),
                    Refused::Struggling => ((56, 54), true),
                    Refused::ViewerShare => ((206, 54), false),
                    Refused::Tie => ((656, 354), false),
                };
                for _ in 0..3 {
                    let link = viewer_rate::pack_link(580_000, 145);
                    win(&mut s, 125_000, link, age_arrival, late);
                    states.push(s.g.pipe_state().expect("a constrained window"));
                }
            }
            let unix = P1_NOW_UNIX + s.t.duration_since(start).as_secs();
            let c = s.g.memory().closing(s.t, unix);
            (c, s.g.belief_counts().1, states)
        };
        let (clear, clear_caps, states) = replay(Refused::Clear);
        assert!(states.iter().all(|s| *s == PipeState::Clear), "{states:?}");
        assert!(clear.counted > 0 && clear.record.is_some(), "{clear:?}");
        assert_eq!(clear.viewer_late, 0);
        let pushed = clear.counted + clear.under_plan + clear.not_counted;
        assert_eq!(
            pushed, clear_caps,
            "every link push-back reached the belief"
        );
        for how in [Refused::Struggling, Refused::ViewerShare] {
            let (c, caps, states) = replay(how);
            assert!(
                states.iter().all(|s| *s == PipeState::ViewerLate),
                "{how:?}: {states:?}"
            );
            assert_eq!(
                caps, clear_caps,
                "{how:?}: the session's belief took them all"
            );
            assert_eq!(c.viewer_late, pushed, "{how:?}");
            assert_eq!((c.counted, c.under_plan, c.record), (0, 0, None), "{how:?}");
            assert_eq!(
                c.floor.map(|f| f.bps),
                Some(620_000),
                "{how:?}: the floor stands"
            );
        }
        let (tie, caps, states) = replay(Refused::Tie);
        assert!(
            states.iter().all(|s| *s == PipeState::TransitStalled),
            "{states:?}"
        );
        assert_eq!(caps, 0, "the gate rejected every tie");
        assert_eq!((tie.counted, tie.viewer_late, tie.record), (0, 0, None));
    }

    /// FR-148 P1 — the viewer's paint age in a screened session.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ScreenAge {
        /// None at all: the viewer's clock probe has not locked, so it sends
        /// the link report alone.
        Absent,
        /// It grows with the queue, by what each slowdown window did not
        /// deliver.
        Grows,
        /// It stays at its floor while the link report reads the queue growing
        /// (R3's V2, the unordered-video shape).
        Flat,
        /// It creeps up with the queue, at most 150 ms over its floor: still
        /// under the transit-stall threshold (V2 too).
        Creeps,
    }

    /// FR-148 P1 (R1, R2, R3) — one session of the screen: a screen that sends
    /// `content` bit/s, on a relay of about 6 Mbit/s, slowed eight times for
    /// `len` windows to `depth_pct` of what is offered, 12 s apart. Each
    /// slowdown is followed by one window that delivers the backlog, so no
    /// byte is lost, and the viewer's link report reads the queue growing by
    /// what each slowdown window did not deliver.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Screen {
        content: u32,
        /// R3's V3: the encoder's output is this share of the target the
        /// window ran with, whatever the screen shows; `None` = `content`.
        offer_pct: Option<u32>,
        depth_pct: u32,
        len: u32,
        age: ScreenAge,
        /// A busy first minute at 5.6 Mbit/s.
        busy: bool,
        /// The pair's entry: an application-limited floor, a day old.
        prior: Option<u32>,
    }

    /// The nominal constrained opening the screen reads the next session at.
    const SCREEN_N: u32 = 2_550_000;

    /// What a screened session leaves for the next one.
    #[derive(Debug)]
    struct Screened {
        closing: crate::encode::pair_memory::Closing,
        trusted: bool,
        /// The entry's floor after the close.
        floor: Option<u32>,
        /// The next session's opening under `on`.
        next: crate::encode::pair_memory::ModelOpening,
    }

    impl Screened {
        /// R1's half of the pass: no trusted record, and the next open under
        /// `on` at N or above, with no slow-link profile.
        fn opens_at_nominal(&self) -> bool {
            !self.trusted && self.next.bps >= SCREEN_N && !self.next.profile
        }
    }

    /// Which law the pair memory takes push-backs by, in a screened session.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Law {
        /// Master's, before R1: every push-back offered to the record.
        Master,
        /// R1 and R2, before R3: a push-back under the plan is offered.
        BeforeR3,
        /// R1, R2 and R3.
        Now,
    }

    /// Replays `v` through the governor under `law`, closes it, and reads the
    /// next opening (N 2.55 Mbit/s, hi 8 Mbit/s, T 1 Mbit/s).
    fn screen(v: Screen, law: Law) -> Screened {
        use crate::encode::pair_memory::{
            Floor, FloorBasis, PairEntry, close, model_opening, trusted,
        };
        // One window: the pump wrote `offer` bit/s, the viewer received `rx`,
        // its queue grew `queue` ms, at paint age `age` (none without a report).
        fn win(s: &mut V5bSession, offer: u32, rx: u32, queue: i16, age: Option<u16>) {
            s.t += Duration::from_secs(1);
            s.sent += u64::from(offer / 8);
            s.g.note_window_sender(v5b_sender());
            s.g.tick_viewer_window(
                s.t,
                30,
                || {
                    if rx > 0 {
                        viewer_rate::pack_report(15, false)
                    } else {
                        0
                    }
                },
                || match (rx > 0, age) {
                    (true, Some(a)) => viewer_rate::pack_age_with_arrival(a, 17, 30, a - 2),
                    _ => 0,
                },
                || {
                    if rx > 0 {
                        viewer_rate::pack_link(rx, queue)
                    } else {
                        0
                    }
                },
                true,
                |o| o,
                s.sent,
            )
            .expect("window due");
            s.g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, s.t);
        }
        let entry = v.prior.map(|bps| PairEntry {
            floor: Floor {
                bps,
                basis: FloorBasis::AppLimited,
                at_unix: P1_NOW_UNIX - 86_400,
            },
            push_back: None,
        });
        let mut s = V5bSession::open(true, None);
        s.g.open_memory(entry.as_ref(), P1_NOW_UNIX, s.t);
        if law == Law::Master {
            s.g.belief.memory_mut().set_read_any();
        }
        if law != Law::Now {
            s.g.belief.memory_mut().set_plan_any();
        }
        let start = s.t;
        let floor_age = |delay: u32| match v.age {
            ScreenAge::Absent => None,
            ScreenAge::Grows => Some((56 + delay).min(60_000) as u16),
            ScreenAge::Flat => Some(56),
            ScreenAge::Creeps => Some(56 + delay.min(150) as u16),
        };
        let offer = |s: &V5bSession| match v.offer_pct {
            Some(pct) => (u64::from(s.g.applied_bps()) * u64::from(pct) / 100) as u32,
            None => v.content,
        };
        if v.busy {
            for _ in 0..30 {
                win(&mut s, 5_600_000, 5_600_000, 0, floor_age(0));
            }
        }
        for _ in 0..8 {
            for _ in 0..12 {
                let o = offer(&s);
                win(&mut s, o, o, 0, floor_age(0));
            }
            let mut backlog: u64 = 0;
            let mut delay: u32 = 0;
            for _ in 0..v.len {
                let o = offer(&s);
                let rx = o / 100 * v.depth_pct;
                backlog += u64::from(o - rx);
                let growth = if rx == 0 {
                    1_000
                } else {
                    ((u64::from(o - rx) * 1_000) / u64::from(rx)).min(1_000) as u32
                };
                delay += growth;
                win(&mut s, o, rx, growth as i16, floor_age(delay));
            }
            let o = offer(&s);
            let drain = o.saturating_add(backlog.min(u64::from(u32::MAX)) as u32);
            let age = floor_age(delay / 2);
            win(&mut s, o, drain, -(delay.min(1_000) as i16), age);
        }
        for _ in 0..10 {
            let o = offer(&s);
            win(&mut s, o, o, 0, floor_age(0));
        }
        let unix = P1_NOW_UNIX + s.t.duration_since(start).as_secs();
        let closing = s.g.memory().closing(s.t, unix);
        let (written, _) = close(entry.as_ref(), &closing);
        let stands = written.or(entry);
        Screened {
            closing,
            trusted: closing.record.is_some_and(|r| trusted(&r, unix)),
            floor: stands.map(|e| e.floor.bps),
            next: model_opening(SCREEN_N, 8_000_000, stands.as_ref(), 1_000_000, unix + 60),
        }
    }

    /// FR-148 P1 (R1) — the screen of W-15's brief: a still screen on a relay
    /// that slows for a few seconds at a time leaves no trusted push-back
    /// record, keeps the remembered floor, and the next session under `on`
    /// opens at the nominal. 336 sessions: a still screen of 40 or 300 kbit/s,
    /// slowdowns of 3 to 5 s to 0 to 88 % of the offer, a busy first minute or
    /// none, a remembered 2.2 Mbit/s floor or none, and a viewer that sends its
    /// paint age or one whose clock probe has not locked.
    ///
    /// On master's law 24 of them fail, all the same class: no age, so the
    /// classifier reads `Unknown` and the gate accepts the window, a still
    /// screen throughout, and slowdowns that deliver 80 to 88 % of the offer.
    /// The link loop reads two growing windows that were offered, a backlogged
    /// sample at that rate counts (it is over half the session's best
    /// delivery), and the drain does not end the record (not twice its rate):
    /// two slowdowns make it trusted, and the next session would open at
    /// 200 kbit/s with the profile on a 6 Mbit/s path. With R1 such a window's
    /// sample still acts in the session, and is not offered to the record.
    #[test]
    fn a_still_screen_on_a_stalling_relay_leaves_no_trusted_record() {
        let mut variants = Vec::new();
        for content in [40_000u32, 300_000] {
            for depth_pct in [0u32, 20, 50, 70, 80, 85, 88] {
                for age in [ScreenAge::Grows, ScreenAge::Absent] {
                    for busy in [false, true] {
                        for len in [3u32, 4, 5] {
                            for prior in [None, Some(2_200_000u32)] {
                                variants.push(Screen {
                                    content,
                                    offer_pct: None,
                                    depth_pct,
                                    len,
                                    age,
                                    busy,
                                    prior,
                                });
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(variants.len(), 336);
        let red = |law: Law| -> Vec<String> {
            variants
                .iter()
                .filter_map(|v| {
                    let r = screen(*v, law);
                    (!r.opens_at_nominal()).then(|| format!("{v:?}: {r:?}"))
                })
                .collect()
        };
        let on_master = red(Law::Master);
        assert_eq!(on_master.len(), 24, "{on_master:#?}");
        // R1 alone is enough for them, and the law as it now stands keeps them.
        let with_r1 = red(Law::BeforeR3);
        assert!(with_r1.is_empty(), "{with_r1:#?}");
        let now = red(Law::Now);
        assert!(now.is_empty(), "{now:#?}");
        // The class, exactly: no age, no busy minute, slowdowns of 80 % or
        // more, and a drain that does not deliver twice the sample:
        // 1 + n·(1 − k) ≤ 2·k.
        for v in &variants {
            let survives = 100 + v.len * (100 - v.depth_pct) <= 2 * v.depth_pct;
            let class = v.age == ScreenAge::Absent && !v.busy && v.depth_pct >= 80 && survives;
            assert_eq!(!screen(*v, Law::Master).opens_at_nominal(), class, "{v:?}");
        }
        // With R1 those windows' samples are counted as unread on the close line.
        let r = screen(
            Screen {
                content: 40_000,
                offer_pct: None,
                depth_pct: 85,
                len: 4,
                age: ScreenAge::Absent,
                busy: false,
                prior: None,
            },
            Law::Now,
        );
        assert_eq!(r.closing.record, None);
        assert!(r.closing.unread > 0, "{r:?}");
        assert_eq!(r.next.bps, SCREEN_N);
    }

    /// FR-148 P1 (R3) — the screen's three families, on R1 and R2, and then
    /// with the plan-limited test (`pair_memory::pushing_against_plan`). Pass:
    /// no trusted record, the remembered floor not lowered, and the next open
    /// under `on` at N or above with no profile.
    ///
    /// | family | sessions | red on R1 + R2 | red with R3 |
    /// |---|---|---|---|
    /// | V1: slowdowns that deliver 88 to 98 %, the age growing or absent | 240 | 0 | 0 |
    /// | V2: the age flat, or creeping under the stall threshold, while the link report grows | 336 | 84 | 42 |
    /// | V3: a steady offer at 55 % or 80 % of the target, 80 to 95 % slowdowns, the age growing or absent | 192 | 0 | 0 |
    /// | V3 with V2's flat age (beyond the ruling) | 96 | 20 | 20 |
    ///
    /// V2 is red on R1 and R2: the classifier reads a window `Clear` on a flat
    /// age, the link loop pushes back at the slowdown's arrival rate, and the
    /// samples count. R3 clears the 40 kbit/s screen: its output stays under
    /// half of every target the session runs with (the slow-link floor keeps
    /// the target at 200 kbit/s or more). It cannot clear the 300 kbit/s
    /// screen: the session's own link clamp cuts the target to 0.9 of the
    /// slowdown's arrival within the first slowdown (about 230 kbit/s), and
    /// from then on the screen writes more than half of it. The slow-link cell
    /// above has the same shape (its first refusal runs under the 3 Mbit/s
    /// opening and is not taken, the later ones run under the clamp and are),
    /// so no test against the target a window ran with can tell the two
    /// apart. That is the finding, pinned here so a change shows. V3 with a
    /// flat age writes 55 % or more of its target by construction.
    #[test]
    fn the_screens_families_on_r1_r2_and_the_plan_limited_test() {
        let pass = |v: &Screen, law: Law| {
            let r = screen(*v, law);
            r.opens_at_nominal() && v.prior.is_none_or(|p| r.floor.is_some_and(|f| f >= p))
        };
        let mut v1 = Vec::new();
        let mut v2 = Vec::new();
        let mut v3 = Vec::new();
        let mut v3_flat = Vec::new();
        for busy in [false, true] {
            for len in [3u32, 4, 5] {
                for prior in [None, Some(2_200_000u32)] {
                    let at = |content, offer_pct, depth_pct, age| Screen {
                        content,
                        offer_pct,
                        depth_pct,
                        len,
                        age,
                        busy,
                        prior,
                    };
                    for content in [40_000u32, 300_000] {
                        for depth_pct in [88u32, 90, 92, 95, 98] {
                            for age in [ScreenAge::Grows, ScreenAge::Absent] {
                                v1.push(at(content, None, depth_pct, age));
                            }
                        }
                        for depth_pct in [0u32, 20, 50, 70, 80, 85, 88] {
                            for age in [ScreenAge::Flat, ScreenAge::Creeps] {
                                v2.push(at(content, None, depth_pct, age));
                            }
                        }
                    }
                    for offer_pct in [55u32, 80] {
                        for depth_pct in [80u32, 85, 90, 95] {
                            for age in [ScreenAge::Grows, ScreenAge::Absent] {
                                v3.push(at(40_000, Some(offer_pct), depth_pct, age));
                            }
                            v3_flat.push(at(40_000, Some(offer_pct), depth_pct, ScreenAge::Flat));
                        }
                    }
                }
            }
        }
        assert_eq!(
            (v1.len(), v2.len(), v3.len(), v3_flat.len()),
            (240, 336, 192, 96)
        );
        let red = |vs: &[Screen], law: Law| -> Vec<Screen> {
            vs.iter().copied().filter(|v| !pass(v, law)).collect()
        };
        for (name, vs) in [("V1", &v1), ("V3", &v3)] {
            assert!(red(vs, Law::BeforeR3).is_empty(), "{name} on R1 + R2");
            assert!(red(vs, Law::Now).is_empty(), "{name} with R3");
        }
        let before = red(&v2, Law::BeforeR3);
        let now = red(&v2, Law::Now);
        assert_eq!((before.len(), now.len()), (84, 42));
        // R3 clears the 40 kbit/s screen, and nothing it did not see red.
        assert!(now.iter().all(|v| v.content == 300_000), "{now:#?}");
        let as_300 = |v: &Screen| Screen {
            content: 300_000,
            ..*v
        };
        for v in &before {
            assert_eq!(now.contains(v), v.content == 300_000, "{v:?}");
            if v.content == 40_000 {
                // Its 300 kbit/s twin is the one that stays red.
                assert!(!pass(&as_300(v), Law::Now), "{v:?}");
            }
        }
        assert_eq!(red(&v3_flat, Law::BeforeR3).len(), 20);
        assert_eq!(red(&v3_flat, Law::Now).len(), 20);
    }

    /// FR-148 P0 — what the line reads on a healthy session: the jupiter replay
    /// on today's laws, at the end of its busy minute and on the still screen
    /// after it. Everything measured is application-limited, nothing pushed
    /// back, and the model's floor is the busiest second. `--nocapture` prints
    /// the two lines.
    #[test]
    fn a_healthy_session_reads_application_limited() {
        let mut s = V5bSession::open(true, None);
        s.load();
        let busy = s.g.path_view(s.t, None);
        for _ in 0..30 {
            s.idle(56);
        }
        let idle = s.g.path_view(s.t, None);
        for v in [busy, idle] {
            assert_eq!(word(v.model_floor), Some((15_915_240, "app-limited")));
            assert_eq!(word(v.learned_ceiling), Some((6_209_663, "app-limited")));
            assert_eq!((v.measured, v.model_capacity, v.prior), (None, None, None));
            assert_eq!(v.belief.2, None, "no capacity");
        }
        assert_eq!(word(busy.viewer_rx), Some((15_915_240, "app-limited")));
        assert_eq!(word(idle.viewer_rx), Some((V5B_IDLE_RX, "app-limited")));
        assert_eq!(
            idle.model_floor.unwrap().age,
            busy.model_floor.unwrap().age + Duration::from_secs(30)
        );
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_ansi(false)
            .with_test_writer()
            .finish();
        let session = bson::oid::ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        tracing::subscriber::with_default(subscriber, || {
            crate::media_delivery::emit_model(session, &busy);
            crate::media_delivery::emit_model(session, &idle);
        });
    }

    /// FR-79 V5b (a) — a truly slow pair's reading expires long before its session
    /// ends: the arrival rate after 15 s, the goodput after 60. FR-59 P8 opens the
    /// next session at what this one found, so the reading still goes to the memory,
    /// as it was read and not as the prior has decayed since.
    #[test]
    fn a_reading_that_expired_is_written_as_it_was_read() {
        for (measured_evidence, written) in [(true, 300_000), (false, 399_300)] {
            let mut s = V5bSession::open(measured_evidence, None);
            // A real overdrive: 1 Mbps into a 300 kbps path, two windows growing.
            let growing = viewer_rate::pack_link(300_000, 200);
            for _ in 0..2 {
                s.window(125_000, Some(56), growing, false);
            }
            assert!(s.g.link_loop_holding(), "the premise: a reading");
            let drained = viewer_rate::pack_link(300_000, -400);
            s.window(V5B_IDLE_BYTES, Some(56), drained, false);
            assert_eq!(s.g.measured_bps(s.t, true), None, "nothing live now");
            for _ in 0..30 {
                s.idle(56);
            }
            assert_eq!(s.g.prior_bps(), Some(399_300), "three steps of ×1.1");
            assert_eq!(s.evidence(), Some(written));
            assert_eq!(
                s.g.session_evidence_bps(s.t, false),
                None,
                "a direct window has nothing to say about a relay"
            );
        }
    }

    /// FR-79 V5b (a), the 12:19 shape. Field 2026-10-30 12:19:50–12:22:06, CORPLAP-2:
    /// a true collapse. The arrival rate read 12,070 in a growing window with
    /// 145,305 bps offered, a blocked send read 180,484 two seconds later, and once
    /// that expired the stand-in walked eight rungs to 84,195, which the session
    /// wrote as evidence. It measured 180,484.
    #[test]
    fn a_walked_stand_in_is_not_what_the_session_measured() {
        let mut s = V5bSession::open(true, None);
        let growing = viewer_rate::pack_link(12_070, 150);
        for _ in 0..2 {
            s.window(18_164, Some(216), growing, false);
        }
        assert!(s.g.link_loop_holding(), "the premise: the report counted");
        // 2,301 B over 101.992 ms folds as the field's reading to the unit.
        s.g.goodput_sink()
            .record(2_301, Duration::from_micros(101_992));
        s.idle(56);
        assert_eq!(s.g.blocked_send_bps(s.t), Some(180_484));
        assert_eq!(
            s.g.prior_bps(),
            Some(180_484),
            "the goodput outranks the arrival rate as the anchor"
        );
        for _ in 0..61 {
            s.idle(56);
        }
        assert_eq!(s.g.blocked_send_bps(s.t), None, "past its 60 s");
        for _ in 0..4 {
            s.idle(150);
        }
        assert_eq!(s.g.prior_bps(), Some(149_160), "the field's second rung");
        assert_eq!(s.evidence(), Some(180_484));
    }

    /// FR-79 V5b (a) — a reading a delivery took out is not written either. What
    /// stands in after it says "at least this", and a lower bound is not a
    /// measurement of the path.
    #[test]
    fn a_session_whose_only_reading_was_refuted_writes_nothing() {
        let mut s = V5bSession::open(true, None);
        s.g.goodput_sink()
            .record(2_854, Duration::from_micros(102_022));
        s.idle(56);
        assert_eq!(s.evidence(), Some(223_794), "it stands until it is beaten");
        s.window(150_000, Some(56), v5b_delivery(1_199_442), false);
        assert_eq!(s.g.readings_refuted(), 1);
        assert_eq!(s.g.prior_bps(), Some(1_199_442), "the premise");
        assert_eq!(s.evidence(), None);
    }

    /// FR-79 V5b (a) — and a reading that has expired can still be wrong: what the
    /// write-back would have used is what the delivery beat.
    #[test]
    fn a_delivery_takes_out_an_expired_reading_too() {
        let mut s = V5bSession::open(true, None);
        s.g.goodput_sink()
            .record(2_854, Duration::from_micros(102_022));
        s.idle(56);
        for _ in 0..61 {
            s.idle(56);
        }
        assert_eq!(s.g.blocked_send_bps(s.t), None, "past its 60 s");
        assert_eq!(s.evidence(), Some(223_794), "still this session's reading");
        s.window(150_000, Some(56), v5b_delivery(1_199_442), false);
        assert_eq!(s.evidence(), None);
    }

    /// FR-79 V5b — a key that is already poisoned. Nothing that opens from it keeps
    /// it alive any more, so it runs out seven days after its last write, to the
    /// second, however many sessions opened from it. A session that really measures
    /// moves it sooner, by the tenth of the gap FR-79 V4 allows upward.
    #[test]
    fn a_poisoned_key_runs_out_or_a_measurement_lifts_it() {
        use super::super::rate_memory::{RateMemory, TTL};
        let written = 1_793_000_000u64;
        let poisoned = || {
            let mut memory = RateMemory::default();
            memory.record_session(V5B_KEY, Some(3_322), written);
            memory
        };
        let mut memory = poisoned();
        for day in 1..=7u64 {
            let at = written + day * 86_400;
            let mut s = V5bSession::open(true, memory.seed_for(V5B_KEY, at));
            assert_eq!(s.g.open_seed_bps(), Some(3_322), "day {day} opens low");
            s.idle(56);
            s.idle(150);
            s.idle(150);
            memory.record_session(V5B_KEY, s.evidence(), at);
        }
        let last_second = written + TTL.as_secs();
        assert_eq!(memory.seed_for(V5B_KEY, last_second), Some(3_322));
        assert_eq!(
            memory.seed_for(V5B_KEY, last_second + 1),
            None,
            "then the pair is cold"
        );

        let mut memory = poisoned();
        let at = written + 600;
        let mut s = V5bSession::open(true, memory.seed_for(V5B_KEY, at));
        let sink = s.g.goodput_sink();
        for _ in 0..12 {
            sink.record(1_500, Duration::from_millis(12));
        }
        s.idle(56);
        assert_eq!(s.evidence(), Some(1_000_000));
        assert_eq!(
            memory.record_session(V5B_KEY, s.evidence(), at),
            102_989,
            "3,322 + 0.1 × the gap"
        );
    }

    /// FR-79 V5b (b) — what the rule must keep. The week's other two congested
    /// reports both had the sender well over what arrived: CORPLAP-2 on 2026-10-30,
    /// 1,107,067 bps offered at 12:04:05 and 145,305 at 12:19:50. The first was false
    /// and a later delivery takes it out; the second was true. This rule is not what
    /// tells them apart.
    #[test]
    fn a_report_the_sender_offered_still_counts() {
        for (rx_bps, wrote, ceiling) in [(581_546, 138_383, 523_391), (12_070, 18_164, 10_863)] {
            let mut s = V5bSession::open(true, None);
            let growing = viewer_rate::pack_link(rx_bps, 250);
            s.window(wrote, Some(56), growing, false);
            let w = s.window(wrote, Some(56), growing, false);
            assert!(w.link_congested, "{rx_bps}");
            assert_eq!(w.link_ceiling_bps, Some(ceiling));
            assert!(s.g.link_loop_holding());
            assert_eq!(s.g.link_unoffered(), 0);
        }
    }

    /// FR-79 V5b (b) — one keyframe is not an overdrive, in either order. Jupiter's
    /// session sent 214,092 B in one, and its false report came 5 s after a 56,299 B
    /// rebuild keyframe. Summing the two windows, or asking of one, would let this
    /// through, and it is the same trickle again.
    #[test]
    fn a_keyframe_among_idle_windows_is_not_an_offer() {
        let growing = viewer_rate::pack_link(V5B_IDLE_RX, 145);
        for order in [[214_092, V5B_IDLE_BYTES], [V5B_IDLE_BYTES, 214_092]] {
            let mut s = V5bSession::open(true, None);
            s.window(order[0], Some(56), growing, false);
            let w = s.window(order[1], Some(56), growing, false);
            assert!(!w.link_congested, "{order:?}");
            assert_eq!(w.link_ceiling_bps, None, "the log line says no ceiling");
            assert!(!s.g.link_loop_holding());
            assert_eq!(s.g.link_unoffered(), 1);
        }
    }

    /// FR-79 V5b (b) — a path that really is stuck, the sender still offering: it
    /// counts, at worst a window after the queue began to grow.
    #[test]
    fn a_stuck_path_counts_a_window_later_at_worst() {
        let growing = viewer_rate::pack_link(300_000, 200);
        let mut s = V5bSession::open(true, None);
        s.window(V5B_IDLE_BYTES, Some(56), growing, false);
        let w = s.window(125_000, Some(56), growing, false);
        assert!(!w.link_congested, "one window has offered so far");
        let w = s.window(125_000, Some(56), growing, false);
        assert!(w.link_congested, "two have");
        assert!(s.g.link_loop_holding());
    }

    /// FR-79 V5b (b) — `bytes_written` counts what left the pump's own channel and
    /// frames are stamped before it, so a queue growing INSIDE that channel reads
    /// sent ≈ rx. The classifier already calls that window the sender's. The byte
    /// budget's skip is the FFmpeg pump's sign of it, and a blocked send is the one
    /// sign the VP9-444 pump has.
    #[test]
    fn a_queue_in_the_pumps_own_channel_is_an_offer() {
        let growing = viewer_rate::pack_link(300_000, 200);
        let at_the_rate_that_arrived = 37_500;
        let mut s = V5bSession::open(true, None);
        for _ in 0..2 {
            s.g.note_window_sender(WindowSenderStats {
                gate_skips: 7,
                ..v5b_sender()
            });
            s.window(at_the_rate_that_arrived, Some(56), growing, false);
        }
        assert_eq!(
            s.g.pipe_state(),
            Some(crate::encode::pipe_state::PipeState::Overproduced)
        );
        assert!(s.g.link_loop_holding(), "the budget skipped frames");
        assert_eq!(s.g.link_unoffered(), 0);

        let mut s = V5bSession::open(true, None);
        let sink = s.g.goodput_sink();
        for _ in 0..2 {
            sink.record(at_the_rate_that_arrived, Duration::from_millis(900));
            s.window(at_the_rate_that_arrived, Some(56), growing, false);
        }
        assert!(s.g.link_loop_holding(), "a send blocked");
        assert_eq!(s.g.link_unoffered(), 0);
    }

    /// FR-79 V5b (b) — a growing window the sender was not feeding neither arms the
    /// clamp nor releases it: the queue did grow, it just isn't the rate's doing.
    /// What stands runs out on the clock of the window that counted.
    #[test]
    fn a_window_the_sender_did_not_feed_leaves_the_clamp_as_it_stands() {
        let growing = viewer_rate::pack_link(300_000, 200);
        let mut s = V5bSession::open(true, None);
        for _ in 0..2 {
            s.window(125_000, Some(56), growing, false);
        }
        assert!(s.g.link_loop_holding(), "armed");
        let armed = s.t;
        let w = s.window(V5B_IDLE_BYTES, Some(56), growing, false);
        assert!(!w.link_congested);
        assert_eq!(s.g.link_unoffered(), 1);
        assert!(s.g.link_loop_holding(), "not released");
        let ttl = armed + LINK_CEILING_TTL;
        assert_eq!(s.g.measured_bps(ttl, true), Some(300_000));
        assert_eq!(
            s.g.measured_bps(ttl + Duration::from_secs(1), true),
            None,
            "not refreshed"
        );
    }

    /// FR-119 P2b-14 — a DIRECT window whose blocked sends include a keyframe
    /// is not a capacity measurement even when a delta blocked beside it: the
    /// delta waited on the keyframe's backlog. Field 2026-10-25, a LAN path:
    /// the idle-settle keyframe after a motion burst blocked 66 ms, a delta
    /// behind it blocked too, the window folded as 60.7 Mbit/s against the
    /// 88.8 Mbit/s ceiling at 120, and 120 went back after 6 s on `pipe`.
    #[test]
    fn a_direct_window_with_a_keyframe_and_a_delta_behind_it_folds_no_capacity() {
        let start = Instant::now();
        let mut g = gov(start);
        let sink = g.goodput_sink();
        sink.record_frame(500_000, Duration::from_millis(66), true);
        sink.record_frame(50_000, Duration::from_millis(12), false);
        g.note_window_sender(t2_sender(120));
        let t1 = start + Duration::from_secs(2);
        g.tick_viewer_window(t1, 120, || 0, || 0, || 0, false, |o| o, 0)
            .expect("window due");
        assert_eq!(g.belief_bps(t1).2, None, "no capacity claim");
        assert_eq!(g.blocked_send_bps(t1), None, "and nothing for the clamp");
        assert_eq!(g.evidence_rejected()[5], 1, "rejected as a keyframe burst");
    }

    /// #3472 — the VP9-444 pump calls `on_encoder_rebuilt` when it builds a
    /// fresh libvpx encoder (`peer.rs`, the rebuild for dims, which a capture
    /// reopen and the keyframe fallback reach too), and a fresh encoder starts
    /// at libvpx's boot target (`DEFAULT_BITRATE_BPS`, 8 Mbps), not the
    /// session's. On a pump whose AIMD sits still (a relay held at its
    /// ceiling) the next tick must still hand the new encoder its target, and
    /// the heartbeat's mirror must not read 0 in between. Field, 2026-11-04,
    /// under the mirror-only hook this replaced: display match rebuilt a
    /// 3 Mbps relay session's encoder 0.1 s in, and it ran 6 min 18 s at
    /// 8.8 Mbps with `target_bps=0` in every heartbeat, until the first
    /// decrease. FR-62's A4 row 3.
    #[test]
    fn a_vp9_rebuild_hands_the_fresh_encoder_its_target() {
        let now = Instant::now();
        let mut g = gov(now);
        let relay_cap = 3_000_000;
        let floor = crate::encode::MIN_BITRATE_BPS;
        let first = g
            .pre_encode_tick(relay_cap, floor, true, DEPTH, now)
            .expect("the first tick applies the ceiling");
        assert_eq!(first.bps, relay_cap);
        assert!(
            g.pre_encode_tick(relay_cap, floor, true, DEPTH, now)
                .is_none(),
            "the premise: a relay held at its ceiling has no move of its own"
        );
        g.on_encoder_rebuilt();
        assert_eq!(
            g.applied_bps(),
            relay_cap,
            "the heartbeat's target does not read 0 across the rebuild"
        );
        let applied = g
            .pre_encode_tick(relay_cap, floor, true, DEPTH, now)
            .expect("the fresh encoder is told its target on the next tick");
        assert_eq!(applied.bps, relay_cap);
    }

    /// FR-148 P0 — the scripted session of the no-behaviour-change test: every
    /// input that moves the live target, at fixed times, on one transport. A
    /// measured pipe (direct), a plan ceiling that drops and returns, an area
    /// floor that rises, fullness at the gate and at the tick, overflows,
    /// rebuilds, a viewer window every 1023 ms (an age that rises, a transit
    /// queue that grows and then drains), and on a constrained session slow
    /// start, the learner, and a soft and a hard send stall; then long runs with
    /// nothing full, loaded, then still, then loaded, for the additive increase.
    /// Every applied move is written as `call:bps:changed`, `call` counting each
    /// call into the governor, and beside it as `call:why`.
    fn replay(constrained: bool) -> (String, String) {
        let start = Instant::now();
        let mut g = RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                slow_start: constrained,
                ..GovernorFlags::default()
            },
            if constrained { 8_000_000 } else { 0 },
            None,
            start,
        );
        let (hi, lo) = if constrained {
            (6_000_000, 4_000_000)
        } else {
            (CEILING, 3_000_000)
        };
        let mut out: Vec<String> = Vec::new();
        let mut whys: Vec<String> = Vec::new();
        let mut note = |call: u32, a: Option<AppliedBitrate>| {
            if let Some(a) = a {
                out.push(format!("{call}:{}:{}", a.bps, u8::from(a.changed)));
                whys.push(format!("{call}:{}", a.why));
            }
        };
        let mut call = 0u32;
        let mut sent: u64 = 0;
        let mut t = start;
        for i in 0..2400u32 {
            t += Duration::from_millis(33);
            // The send task's bytes: the target's worth while the screen moves,
            // a trickle while it is still.
            sent += if (1200..1800).contains(&i) {
                2_000
            } else {
                u64::from(g.applied_bps()) / 240
            };
            match i {
                // A 10 Mbit/s pipe, measured on blocked sends: the direct clamp.
                10..=14 if !constrained => {
                    for _ in 0..8 {
                        g.goodput_sink().record(30_000, Duration::from_millis(24));
                    }
                }
                45 | 46 | 350 => {
                    call += 1;
                    note(call, g.on_send_overflow(t));
                }
                130 | 700 => {
                    call += 1;
                    g.on_encoder_rebuilt();
                }
                400 if constrained => {
                    call += 1;
                    g.note_send_stall(Duration::from_millis(300), t);
                }
                450 if constrained => {
                    g.goodput_sink().record(25_000, Duration::from_millis(1500));
                    call += 1;
                    g.note_send_stall(Duration::from_millis(1500), t);
                }
                _ => {}
            }
            if i % 31 == 30 {
                let age: (u16, u16) = if (360..480).contains(&i) {
                    (300, 60)
                } else {
                    (62, 60)
                };
                let link = match i {
                    540..620 => viewer_rate::pack_link(300_000, 250),
                    620..700 => viewer_rate::pack_link(300_000, -200),
                    _ => 0,
                };
                call += 1;
                let _ = g.tick_viewer_window(
                    t,
                    30,
                    || viewer_rate::pack_report(30, false),
                    || viewer_rate::pack_age(age.0, age.1, 80),
                    || link,
                    constrained,
                    |d| d,
                    sent,
                );
            }
            call += 1;
            if (300..330).contains(&i) && i % 2 == 0 {
                // The gate skips the frame on a full channel: no tick.
                note(call, g.on_backpressure_skip(t));
                continue;
            }
            let ceiling = if (200..260).contains(&i) { lo } else { hi };
            let floor = if (120..150).contains(&i) {
                hi / 8 * 7
            } else {
                crate::encode::MIN_BITRATE_BPS
            };
            let capacity = if matches!(i, 20..=22 | 80 | 1000 | 2000) {
                0
            } else {
                DEPTH
            };
            note(
                call,
                g.pre_encode_tick(ceiling, floor, constrained, capacity, t),
            );
        }
        (out.join(" "), whys.join(" "))
    }

    /// FR-148 P0 — what [`replay`] applied on a direct transport before any
    /// cause was named: recorded on `211ac93b6`, before this change was written.
    const REPLAY_DIRECT: &str = "1:12000000:1 21:10200000:1 32:8500000:1 47:7225000:1 85:6141250:1 126:8500000:1 \
         138:8500000:0 210:3000000:1 313:2550000:1 330:2167500:1 365:1842375:1 523:2373625:1 \
         680:2904875:1 728:2904875:0 838:3436125:1 994:3967375:1 1038:3372205:1 1195:3903455:1 \
         1352:4434705:1 1509:4965955:1 1665:5497205:1 1822:6028455:1 1979:6778455:1 \
         2070:5761640:1 2227:6511640:1 2384:7261640:1";

    /// FR-148 P0 — the same on a constrained transport.
    const REPLAY_CONSTRAINED: &str = "1:300000:1 33:600000:1 67:1500000:1 126:5250000:1 138:5250000:0 210:4000000:1 \
         243:4250000:1 313:3612500:1 330:3070625:1 365:2610010:1 418:2218500:1 453:1885725:1 \
         486:1500000:1 614:270000:1 730:270000:0 771:303750:1 928:341718:1 1040:290445:1 \
         1197:326750:1 1354:367593:1 1511:413542:1 1667:465234:1 1824:523388:1 1981:588811:1 \
         2072:500480:1 2229:563040:1 2386:633420:1";

    /// FR-148 P0 — naming the causes moves nothing: the scripted session above
    /// applies exactly the targets it applied before any cause was named, at the
    /// same calls, on both transports. A difference here is a behaviour change.
    #[test]
    fn naming_the_causes_moves_no_target() {
        assert_eq!(replay(false).0, REPLAY_DIRECT, "direct");
        assert_eq!(replay(true).0, REPLAY_CONSTRAINED, "constrained");
    }

    /// FR-148 P0 — the word a move's line prints, or `-` when nothing moved.
    fn why_of(a: Option<AppliedBitrate>) -> String {
        a.map_or_else(|| "-".to_string(), |a| a.why.to_string())
    }

    /// FR-148 P0 — why each move of [`replay`] happened, on a direct transport:
    /// the measured 10 Mbit/s pipe clamps the plan (`ceiling(pipe)`), and in the
    /// still span (2 KB a frame, about 0.48 Mbit/s under a 4-6 M target) the
    /// increases read `app-limited`.
    const REPLAY_DIRECT_WHY: &str = "1:open 21:decrease(full) 32:ceiling(pipe) 47:decrease(overflow) 85:decrease(full) \
         126:floor(area) 138:reapply 210:ceiling(plan) 313:decrease(full) 330:decrease(full) \
         365:decrease(overflow) 523:increase(loaded) 680:increase(loaded) 728:reapply \
         838:increase(loaded) 994:increase(loaded) 1038:decrease(full) 1195:increase(loaded) \
         1352:increase(app-limited) 1509:increase(app-limited) 1665:increase(app-limited) \
         1822:increase(app-limited) 1979:increase(loaded) 2070:decrease(full) \
         2227:increase(loaded) 2384:increase(loaded)";

    /// FR-148 P0 — the same, constrained: slow start opens at 300 k and its ramp
    /// lifts the floor (`floor(ramp)`) until it ends and the area floor stands;
    /// the soft stall, the age loop, the hard stall and the growing transit
    /// queue each name their decrease, and the queue's arrival-rate ceiling
    /// folds into the same move. The still span's 0.48 Mbit/s is over half a
    /// 0.33-0.52 M target, so its increases read `loaded`.
    const REPLAY_CONSTRAINED_WHY: &str = "1:open 33:floor(ramp) 67:floor(area) 126:floor(area) 138:reapply 210:ceiling(plan) \
         243:increase(loaded) 313:decrease(full) 330:decrease(full) 365:decrease(overflow) \
         418:decrease(stall) 453:decrease(age) 486:hard-decrease \
         614:decrease(link)+ceiling(arrival) 730:reapply 771:increase(loaded) \
         928:increase(loaded) 1040:decrease(full) 1197:increase(loaded) 1354:increase(loaded) \
         1511:increase(loaded) 1667:increase(loaded) 1824:increase(loaded) \
         1981:increase(loaded) 2072:decrease(full) 2229:increase(loaded) 2386:increase(loaded)";

    /// FR-148 P0 — and every move of that session says why, at the call that
    /// made it.
    #[test]
    fn every_move_of_the_replay_says_why() {
        assert_eq!(replay(false).1, REPLAY_DIRECT_WHY, "direct");
        assert_eq!(replay(true).1, REPLAY_CONSTRAINED_WHY, "constrained");
    }

    /// FR-148 P0 — the pump's own inputs, one at a time, each named on the move
    /// it makes: the first target, a full channel at the gate and at the tick,
    /// an overflow, a higher area floor, a lower plan, and a rebuild.
    #[test]
    fn each_pump_input_names_its_move() {
        let floor = crate::encode::MIN_BITRATE_BPS;
        let mut t = Instant::now();
        let mut g = gov(t);
        assert_eq!(
            why_of(g.pre_encode_tick(3_000_000, floor, true, DEPTH, t)),
            "open"
        );
        t += Duration::from_millis(600);
        assert_eq!(why_of(g.on_backpressure_skip(t)), "decrease(full)");
        t += Duration::from_millis(600);
        assert_eq!(
            why_of(g.pre_encode_tick(3_000_000, floor, true, 0, t)),
            "decrease(full)"
        );
        t += Duration::from_millis(600);
        assert_eq!(why_of(g.on_send_overflow(t)), "decrease(overflow)");
        let up = g.pre_encode_tick(3_000_000, 2_000_000, true, DEPTH, t);
        assert_eq!(
            (up.map(|a| a.bps), why_of(up)),
            (Some(2_000_000), "floor(area)".to_string())
        );
        let down = g.pre_encode_tick(1_800_000, floor, true, DEPTH, t);
        assert_eq!(
            (down.map(|a| a.bps), why_of(down)),
            (Some(1_800_000), "ceiling(plan)".to_string())
        );
        g.on_encoder_rebuilt();
        let again = g
            .pre_encode_tick(1_800_000, floor, true, DEPTH, t)
            .expect("the rebuilt encoder is told its target");
        assert_eq!(
            (again.bps, again.changed, again.why.to_string()),
            (1_800_000, false, "reapply".to_string())
        );
    }

    /// FR-148 P0 — a folded move names every cause in the order it happened: a
    /// soft send stall's ×0.85 lands between two ticks and the next tick lowers
    /// the plan too, so one target carries both. The heartbeat counts it once,
    /// under its whole why, and taking the count empties it.
    #[test]
    fn a_folded_move_names_every_cause_in_order() {
        let floor = crate::encode::MIN_BITRATE_BPS;
        let mut t = Instant::now();
        let mut g = gov(t);
        g.pre_encode_tick(3_000_000, floor, true, DEPTH, t);
        t += Duration::from_millis(600);
        g.note_send_stall(Duration::from_millis(300), t);
        let a = g
            .pre_encode_tick(2_000_000, floor, true, DEPTH, t)
            .expect("one move");
        assert_eq!(
            (a.bps, a.why.to_string()),
            (2_000_000, "decrease(stall)+ceiling(plan)".to_string())
        );
        assert_eq!(
            g.take_moves().to_string(),
            "open:1,decrease(stall)+ceiling(plan):1"
        );
        assert_eq!(
            g.take_moves().to_string(),
            "none",
            "the next heartbeat starts empty"
        );
    }

    /// FR-148 P0 — an additive increase says whether the sender was
    /// application-limited in the last viewer window: R3, the send task's own
    /// byte count against the target the window ran with.
    #[test]
    fn an_increase_says_whether_the_sender_was_application_limited() {
        let floor = crate::encode::MIN_BITRATE_BPS;
        let increase = |window_bytes: Option<u64>| -> String {
            let t0 = Instant::now();
            let mut g = gov(t0);
            g.pre_encode_tick(3_000_000, floor, false, DEPTH, t0);
            // Room to climb: an overflow takes the target to 2.55 M.
            g.on_send_overflow(t0 + Duration::from_millis(600));
            let mut t = t0 + Duration::from_millis(1100);
            if let Some(bytes) = window_bytes {
                g.tick_viewer_window(
                    t,
                    30,
                    || viewer_rate::pack_report(30, false),
                    || 0,
                    || 0,
                    false,
                    |d| d,
                    bytes,
                )
                .expect("window due");
            }
            while t < t0 + Duration::from_secs(20) {
                t += Duration::from_millis(200);
                if let Some(a) = g.pre_encode_tick(3_000_000, floor, false, DEPTH, t) {
                    return a.why.to_string();
                }
            }
            "-".to_string()
        };
        assert_eq!(increase(None), "increase(no-window)");
        // 50 KB in the 1.1 s window is 0.36 Mbit/s under a 2.55 M target: what
        // a still screen writes.
        assert_eq!(increase(Some(50_000)), "increase(app-limited)");
        // 400 KB is 2.9 Mbit/s: the sender pushed against its target.
        assert_eq!(increase(Some(400_000)), "increase(loaded)");
    }

    /// FR-148 P0 — FR-15's age loop names its decrease: two windows learn a
    /// 60 ms floor, then the age sits at 200 ms and the loop cuts twice.
    #[test]
    fn an_age_excess_names_its_decrease() {
        let floor = crate::encode::MIN_BITRATE_BPS;
        let start = Instant::now();
        let mut g = gov(start);
        let mut whys = vec![why_of(
            g.pre_encode_tick(3_000_000, floor, true, DEPTH, start),
        )];
        for (i, (avg, min)) in [(62u16, 60u16), (64, 61), (200, 62), (205, 63), (210, 63)]
            .into_iter()
            .enumerate()
        {
            let t = start + Duration::from_millis(1100 * (i as u64 + 1));
            g.tick_viewer_window(
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
            whys.push(why_of(g.pre_encode_tick(3_000_000, floor, true, DEPTH, t)));
        }
        assert_eq!(
            whys,
            ["open", "-", "-", "-", "decrease(age)", "decrease(age)"]
        );
        assert_eq!(g.take_moves().to_string(), "open:1,decrease(age):2");
    }

    /// FR-148 P0 — FR-59 P3's growing transit queue names its decrease, and the
    /// arrival-rate ceiling it sets folds into the same move at the next tick.
    /// Then the viewer reports a faster arrival: the relieved floor rises with
    /// it and pulls the target up (`floor(relief)`), first to the ceiling still
    /// in force, then to itself.
    #[test]
    fn a_growing_transit_queue_names_its_decrease_and_its_bounds() {
        let floor = crate::encode::MIN_BITRATE_BPS;
        let start = Instant::now();
        let mut g = gov(start);
        let mut t = start;
        let mut moves = Vec::new();
        for i in 0..8 {
            t += Duration::from_millis(1100);
            let rx = if i < 4 { 400_000 } else { 1_200_000 };
            g.tick_viewer_window(
                t,
                30,
                || viewer_rate::pack_report(30, false),
                || 0,
                || viewer_rate::pack_link(rx, 250),
                true,
                |o| o,
                // 4 Mbit/s offered, over 1.1 × either arrival rate: the queue
                // the viewer reports is the sender's.
                offering(start, t) * 4,
            )
            .expect("window due");
            for _ in 0..4 {
                t += Duration::from_millis(200);
                if let Some(a) = g.pre_encode_tick(3_000_000, floor, true, DEPTH, t) {
                    moves.push(format!("w{i}:{}:{}", a.bps, a.why));
                }
            }
        }
        assert_eq!(
            moves,
            [
                "w0:3000000:open",
                "w1:360000:decrease(link)+ceiling(arrival)",
                "w2:340000:decrease(link)",
                "w4:360000:floor(relief)",
                "w4:1020000:floor(relief)",
            ]
        );
    }

    /// FR-148 P0 — FR-79 V5's transit stall over the measured pipe names its
    /// decrease: the cell of `a_transit_stall_above_the_measured_pipe_cuts_the_rate`,
    /// where both silent windows cut and the one move names the cause once.
    #[test]
    fn a_transit_stall_names_its_decrease() {
        let floor = crate::encode::MIN_BITRATE_BPS;
        let start = Instant::now();
        let mut g = RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                measured_ceiling: false,
                ..GovernorFlags::default()
            },
            8_000_000,
            None,
            start,
        );
        let sender = WindowSenderStats {
            inflight_bytes: 1_500,
            budget_bytes: 128_000,
            gate_skips: 0,
            send_wait_max_ms: 1.0,
            send_wait_avg_ms: Some(0.5),
            frames_sent: 30,
            stalled_passes: 0,
            opening: false,
        };
        let mut t = start;
        for i in 0..4 {
            if i == 3 {
                for _ in 0..12 {
                    g.goodput_sink().record(1_500, Duration::from_millis(12));
                }
            }
            t += Duration::from_millis(1100);
            g.note_window_sender(sender);
            g.tick_viewer_window(
                t,
                30,
                || viewer_rate::pack_report(30, false),
                || viewer_rate::pack_age_with_arrival(42, 40, 80, 41),
                || 0,
                true,
                |o| o,
                0,
            )
            .expect("window due");
        }
        assert_eq!(
            why_of(g.pre_encode_tick(3_000_000, floor, true, DEPTH, t)),
            "open"
        );
        for _ in 0..2 {
            t += Duration::from_millis(1100);
            g.note_window_sender(sender);
            g.tick_viewer_window(t, 30, || 0, || 0, || 0, true, |o| o, 0)
                .expect("window due");
        }
        let a = g.pre_encode_tick(3_000_000, floor, true, DEPTH, t);
        assert_eq!(
            (a.map(|a| a.bps), why_of(a)),
            (Some(2_167_500), "decrease(transit-stall)".to_string())
        );
    }

    /// FR-148 P0 — FR-35's ×0.5 names itself: the free-loop hard stall of
    /// `a_hard_stall_on_a_free_loop_still_halves_at_its_own_window`.
    #[test]
    fn a_hard_stall_names_its_halving() {
        let start = Instant::now();
        let mut g = t2_governor(start);
        let mut t = start;
        for _ in 0..4 {
            t += Duration::from_millis(1100);
            t2_clean_window(&mut g, t);
        }
        g.goodput_sink().record(25_000, Duration::from_millis(2900));
        t += Duration::from_millis(2900);
        g.note_send_stall(Duration::from_millis(2900), t);
        g.note_window_sender(t2_sender(12));
        g.tick_viewer_window(
            t,
            30,
            || viewer_rate::pack_report(12, false),
            || viewer_rate::pack_age_with_arrival(300, 50, 80, 299),
            || 0,
            true,
            |o| o,
            0,
        )
        .expect("window due");
        let a = g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        assert_eq!(
            (a.map(|a| a.bps), why_of(a)),
            (Some(2_805_000), "hard-decrease".to_string())
        );
    }

    /// FR-148 P0 — the constrained ceiling's own stages name their moves: FR-35's
    /// learned ceiling holds a plan that drops under it (`ceiling(learned)`), and
    /// FR-63's opener caps a session that flips from direct to a relay
    /// (`ceiling(opener)`), whose ramp then lifts the floor one doubling at a
    /// time (`floor(ramp)`) until it ends and the area floor stands.
    #[test]
    fn the_learned_ceiling_and_the_opener_name_their_moves() {
        let floor = crate::encode::MIN_BITRATE_BPS;
        let start = Instant::now();
        let mut g = t2_governor(start);
        assert_eq!(
            why_of(g.pre_encode_tick(7_000_000, floor, true, DEPTH, start)),
            "open"
        );
        let a = g.pre_encode_tick(3_000_000, floor, true, DEPTH, start);
        assert_eq!(
            (a.map(|a| a.bps), why_of(a)),
            (Some(5_610_000), "ceiling(learned)".to_string())
        );

        let mut g = RateGovernor::new(
            30,
            DEPTH,
            GovernorFlags {
                slow_start: true,
                ..GovernorFlags::default()
            },
            0,
            None,
            start,
        );
        assert_eq!(
            why_of(g.pre_encode_tick(3_000_000, floor, false, DEPTH, start)),
            "open"
        );
        let a = g.pre_encode_tick(3_000_000, floor, true, DEPTH, start);
        assert_eq!(
            (a.map(|a| a.bps), why_of(a)),
            (Some(300_000), "ceiling(opener)".to_string())
        );
        let mut t = start;
        let mut moves = Vec::new();
        for _ in 0..3 {
            t += Duration::from_millis(1100);
            g.tick_viewer_window(
                t,
                30,
                || viewer_rate::pack_report(30, false),
                || 0,
                || 0,
                true,
                |o| o,
                0,
            )
            .expect("window due");
            for _ in 0..2 {
                t += Duration::from_millis(33);
                if let Some(a) = g.pre_encode_tick(3_000_000, floor, true, DEPTH, t) {
                    moves.push(format!("{}:{}", a.bps, a.why));
                }
            }
        }
        // Each doubling lands one tick after its window: the controller caps a
        // new floor at the ceiling already in force, and the ramp raised both.
        assert_eq!(
            moves,
            [
                "600000:floor(ramp)",
                "1200000:floor(ramp)",
                "1500000:floor(area)"
            ]
        );
    }

    /// FR-148 P0 — the FFmpeg pump holds a target for a rebuild-bound encoder
    /// (`deferred_bps`) and applies it at the next quiet. The hold is the
    /// [`AppliedBitrate`] itself, so the line that lands it prints the why it was
    /// handed out with, whatever the governor did since, and the heartbeat
    /// counted the move when it was handed out. A newer target replaces a hold,
    /// and its why explains its own step.
    #[test]
    fn a_held_target_keeps_its_why_until_it_lands() {
        let floor = crate::encode::MIN_BITRATE_BPS;
        let mut t = Instant::now();
        let mut g = gov(t);
        g.pre_encode_tick(3_000_000, floor, true, DEPTH, t);
        t += Duration::from_millis(600);
        let mut held = g.on_send_overflow(t);
        for _ in 0..10 {
            t += Duration::from_millis(33);
            assert_eq!(
                why_of(g.pre_encode_tick(3_000_000, floor, true, DEPTH, t)),
                "-"
            );
        }
        assert_eq!(g.take_moves().to_string(), "open:1,decrease(overflow):1");
        let landed = held.take().expect("a held target");
        assert_eq!(
            (landed.bps, landed.why.to_string()),
            (2_550_000, "decrease(overflow)".to_string())
        );

        t += Duration::from_millis(600);
        held = g.on_send_overflow(t);
        if let Some(newer) = g.pre_encode_tick(1_800_000, floor, true, DEPTH, t) {
            held = Some(newer);
        }
        let landed = held.expect("a held target");
        assert_eq!(
            (landed.bps, landed.why.to_string()),
            (1_800_000, "ceiling(plan)".to_string())
        );
    }

    /// FR-148 P0 — what a line writes for a why and for the heartbeat's tally.
    /// A move with more distinct causes than are kept ends `+more`, and past 16
    /// distinct whys the tally counts a move under `more`, so it stays bounded
    /// where nothing takes it (the heartbeat line filtered out).
    #[test]
    fn a_why_and_the_tally_read_as_written() {
        assert_eq!(Why::default().to_string(), "none");
        assert_eq!(Moves::default().to_string(), "none");

        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut c = AimdController::new(12_000_000, 1_500_000, 12_000_000, 8, t0);
        for (n, signal) in [
            (600, Congestion::Full),
            (1_200, Congestion::Overflow),
            (1_800, Congestion::Stall),
            (2_400, Congestion::Age),
            (3_000, Congestion::Link),
            (3_600, Congestion::TransitStall),
        ] {
            c.note_congestion(signal, ms(n));
        }
        c.apply_hard_md(ms(3_610));
        c.set_ceiling_from(1_800_000, CeilingSource::Opener);
        let (_, causes) = c.take_move().expect("a move");
        assert_eq!(
            Why {
                causes,
                app_limited: None
            }
            .to_string(),
            "open+decrease(full)+decrease(overflow)+decrease(stall)+decrease(age)+\
             decrease(link)+decrease(transit-stall)+hard-decrease+more"
        );

        let every = [
            Cause::Open,
            Cause::Increase,
            Cause::Decrease(Congestion::Full),
            Cause::Decrease(Congestion::Overflow),
            Cause::Decrease(Congestion::Stall),
            Cause::Decrease(Congestion::Age),
            Cause::Decrease(Congestion::Link),
            Cause::Decrease(Congestion::TransitStall),
            Cause::HardDecrease,
            Cause::Ceiling(CeilingSource::Plan),
            Cause::Ceiling(CeilingSource::Pipe),
            Cause::Ceiling(CeilingSource::Learned),
            Cause::Ceiling(CeilingSource::Arrival),
            Cause::Ceiling(CeilingSource::Budget),
            Cause::Ceiling(CeilingSource::Opener),
            Cause::Floor(FloorSource::Area),
            Cause::Floor(FloorSource::Relief),
            Cause::Floor(FloorSource::Ramp),
            Cause::Reapply,
        ];
        let mut tally = Moves::default();
        for cause in every.into_iter().chain([Cause::Open]) {
            tally.note(Why {
                causes: Causes::of(cause),
                app_limited: None,
            });
        }
        assert_eq!(
            tally.to_string(),
            "open:2,increase(no-window):1,decrease(full):1,decrease(overflow):1,\
             decrease(stall):1,decrease(age):1,decrease(link):1,decrease(transit-stall):1,\
             hard-decrease:1,ceiling(plan):1,ceiling(pipe):1,ceiling(learned):1,\
             ceiling(arrival):1,ceiling(budget):1,ceiling(opener):1,floor(area):1,more:3"
        );
    }

    /// FR-148 D51 — the budget's last step under `on`, on a relay session: its
    /// measured rate, whether it held lately outside a still span, and its
    /// envelope.
    fn acting(bps: u32, held: bool, envelope_ms: u16) -> crate::encode::pipe::PathRate {
        crate::encode::pipe::PathRate {
            bps: Some(bps),
            measured: true,
            loss: 0,
            acts: true,
            held,
            envelope_ms,
        }
    }

    /// FR-148 D51 — one viewer window `dt` after `t`, with the budget's step
    /// `rate` noted first, a decode report of 30 fps, and a paint age of
    /// `avg` over a least of `min` on a 60 ms round trip.
    fn budget_window(
        g: &mut RateGovernor,
        t: &mut Instant,
        rate: crate::encode::pipe::PathRate,
        (avg, min): (u16, u16),
    ) -> ViewerWindow {
        *t += Duration::from_millis(1_100);
        g.note_path_rate(rate);
        g.tick_viewer_window(
            *t,
            30,
            || viewer_rate::pack_report(30, false),
            || viewer_rate::pack_age(avg, min, 60),
            || 0,
            true,
            |o| o,
            0,
        )
        .expect("window due")
    }

    /// FR-148 D51 B4b (C5) — under the budget the target follows its measured
    /// rate. (a) The budget measured 1.5 M and held lately, the target is
    /// 1.0 M: the additive increase climbs (no hold blocks it) and never
    /// passes 1.5 M. (b) A target of 2.5 M at the same rate is capped at 1.5 M
    /// at once, by the budget. (c) Steps with no hold lately lift the cap, and
    /// the increase climbs past it.
    #[test]
    fn under_the_budget_the_target_follows_its_measured_rate() {
        const AREA: u32 = 500_000;
        let frame = Duration::from_millis(33);
        let t0 = Instant::now();
        // (a)
        let mut g = gov(t0);
        let opened = g.pre_encode_tick(1_000_000, AREA, true, DEPTH, t0);
        assert_eq!(opened.map(|a| a.bps), Some(1_000_000));
        let (mut t, mut top) = (t0, 1_000_000);
        for _ in 0..40 * 30 {
            t += frame;
            g.note_path_rate(acting(1_500_000, true, 200));
            if let Some(a) = g.pre_encode_tick(3_000_000, AREA, true, DEPTH, t) {
                top = top.max(a.bps);
            }
        }
        assert!(top > 1_000_000, "(a) the increase climbed: {top}");
        assert!(top <= 1_500_000, "(a) never past the budget's rate: {top}");
        assert_eq!(g.applied_bps(), 1_500_000, "(a) it reached it");
        // (b)
        let mut g = gov(t0);
        g.pre_encode_tick(2_500_000, AREA, true, DEPTH, t0);
        assert_eq!(g.applied_bps(), 2_500_000);
        g.note_path_rate(acting(1_500_000, true, 200));
        let capped = g.pre_encode_tick(3_000_000, AREA, true, DEPTH, t0 + frame);
        assert_eq!(capped.map(|a| a.bps), Some(1_500_000), "(b) capped at once");
        let why = capped.map(|a| a.why.to_string()).unwrap_or_default();
        assert!(why.contains("ceiling(budget)"), "(b) by the budget: {why}");
        // (c)
        let (mut t, mut top) = (t0 + frame, 0);
        for _ in 0..20 * 30 {
            t += frame;
            g.note_path_rate(acting(1_500_000, false, 200));
            if let Some(a) = g.pre_encode_tick(3_000_000, AREA, true, DEPTH, t) {
                top = top.max(a.bps);
            }
        }
        assert!(top > 1_500_000, "(c) the cap lifted: {top}");
    }

    /// FR-148 D51 B4b-2 (C7) — under the budget its held rate is the live
    /// reading. The prior stands two steps under an opening reading of 3.5 M.
    /// A window in which the budget did not hold lately changes neither the
    /// prior nor `measured_bps`; a window in which it held re-anchors the
    /// prior on its measured rate, the steps reset, and that rate is
    /// `measured_bps`.
    #[test]
    fn under_the_budget_its_held_rate_is_the_live_reading() {
        let t0 = Instant::now();
        let mut g = gov(t0);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t0);
        g.prior.on_window(Some(3_500_000), false);
        for _ in 0..4 {
            g.prior.on_window(None, true);
        }
        assert_eq!(g.prior_walk(), (Some(3_500_000), -2));
        let mut t = t0;
        budget_window(&mut g, &mut t, acting(1_400_000, false, 200), (42, 40));
        assert_eq!(
            g.prior_walk(),
            (Some(3_500_000), -2),
            "no hold: the prior stays"
        );
        assert_eq!(g.measured_bps(t, true), None, "no hold: no reading");
        budget_window(&mut g, &mut t, acting(1_400_000, true, 200), (42, 40));
        assert_eq!(g.prior_walk(), (Some(1_400_000), 0), "held: re-anchored");
        assert_eq!(
            g.measured_bps(t, true),
            Some(1_400_000),
            "held: the reading"
        );
    }

    /// FR-148 D51 B4a-2 (C8) — the budget's queue does not walk the prior
    /// down. Under `on`, with an envelope of 200 ms and no hold lately (so no
    /// reading re-anchors it): six windows 150 ms over a learned floor of
    /// 40 ms are the budget's own queue, not push-back; two windows 400 ms over
    /// it are, and step the prior down once.
    #[test]
    fn the_budgets_queue_does_not_walk_the_prior_down() {
        let t0 = Instant::now();
        let mut g = gov(t0);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t0);
        g.prior.on_window(Some(3_500_000), false);
        let mut t = t0;
        let rate = acting(1_400_000, false, 200);
        for age in [(42, 40), (43, 40)].into_iter().chain([(190, 40); 6]) {
            budget_window(&mut g, &mut t, rate, age);
        }
        assert_eq!(g.prior_walk(), (Some(3_500_000), 0), "within the envelope");
        for _ in 0..2 {
            budget_window(&mut g, &mut t, rate, (440, 40));
        }
        assert_eq!(g.prior_walk(), (Some(3_500_000), -1), "past it");
    }

    /// FR-148 D51 B4a-2 (C3) — the age loop leaves the budget's queue to the
    /// budget. Under `on`, with an envelope of 200 ms (S and a 50 ms frame):
    /// six windows 150 ms over a learned floor of 40 ms fire nothing. No
    /// window is over, the divisor stays at 1, no move is an age decrease,
    /// and the prior is not pushed back.
    #[test]
    fn the_age_loop_leaves_the_budgets_queue_to_the_budget() {
        let t0 = Instant::now();
        let mut g = gov(t0);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t0);
        let mut t = t0;
        let rate = acting(1_400_000, false, 200);
        for age in [(42, 40), (43, 40)].into_iter().chain([(190, 40); 6]) {
            let w = budget_window(&mut g, &mut t, rate, age);
            assert!(!w.age_over && w.skip_divisor == 1, "{age:?}: {w:?}");
            let moved =
                g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
            let why = moved.map(|a| a.why.to_string()).unwrap_or_default();
            assert!(!why.contains("decrease(age)"), "{age:?}: {why}");
        }
        assert_eq!(g.viewer_age(), Some((190, 40)));
        assert_eq!(g.prior_walk().1, 0, "not pushed back");
    }

    /// FR-148 D51 B4a-2 (C4) — the age loop still catches a lag past the
    /// envelope: the same session with the age 400 ms over its floor fires on
    /// the second such window, and the next frame's move is the age's
    /// decrease. Green before B4 and after: the backstop holds.
    #[test]
    fn the_age_loop_still_catches_a_lag_past_the_envelope() {
        let t0 = Instant::now();
        let mut g = gov(t0);
        g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t0);
        let mut t = t0;
        let rate = acting(1_400_000, false, 200);
        for age in [(42, 40), (43, 40), (440, 40)] {
            let w = budget_window(&mut g, &mut t, rate, age);
            assert!(!w.age_over, "{age:?}: one window is a lump");
        }
        let w = budget_window(&mut g, &mut t, rate, (440, 40));
        assert!(w.age_over, "the second fires: {w:?}");
        let moved = g.pre_encode_tick(3_000_000, crate::encode::MIN_BITRATE_BPS, true, DEPTH, t);
        let why = moved.map(|a| a.why.to_string()).unwrap_or_default();
        assert!(why.contains("decrease(age)"), "{why}");
    }

    /// FR-148 D51 B4f (C9) — while the budget acts, the pump charges an
    /// encode at most the interval it chose for the frame: its capture
    /// interval at 30 fps; a divisor of 3 charges three of them; FR-112's
    /// paced rate (40 fps on a 60 fps target) is the interval while in force.
    /// A budget that does not act bounds nothing, whatever the divisor.
    #[test]
    fn under_on_the_charge_is_the_pumps_interval_times_the_divisor() {
        let t0 = Instant::now();
        let mut g = gov(t0);
        assert_eq!(g.charge_interval(30), None, "no step yet");
        g.note_path_rate(acting(1_500_000, true, 200));
        assert_eq!(g.charge_interval(30), Some(Duration::from_micros(33_333)));
        let w = g
            .tick_viewer_window(
                t0 + Duration::from_secs(2),
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
        assert_eq!(
            g.charge_interval(30),
            Some(Duration::from_micros(3 * 33_333)),
            "the divisor"
        );
        g.note_path_rate(crate::encode::pipe::PathRate {
            acts: false,
            ..acting(1_500_000, true, 200)
        });
        assert_eq!(g.charge_interval(30), None, "not acting");
        let mut g = gov(t0);
        g.note_path_rate(acting(1_500_000, true, 200));
        assert_eq!(g.pace_software(25.0, 60), Some(40));
        assert_eq!(
            g.charge_interval(60),
            Some(Duration::from_micros(25_000)),
            "the paced rate"
        );
    }

    /// FR-148 D51 — the kill switch. `off` and `shadow` are today's paths byte
    /// for byte: with the budget not acting, nothing B4 adds is read. Two
    /// governors through the same 100,000 random events (frames at any send
    /// depth, gate skips, viewer windows with any age, arrival rate and decode
    /// backlog, and the budget's steps); one hears each step as a budget that
    /// does not act, with random B4 fields, the other as before D51. Every
    /// live output is the same at every event.
    #[test]
    fn b4_reads_nothing_while_the_budget_does_not_act() {
        use crate::encode::pipe::PathRate;
        let t0 = Instant::now();
        let mk = || remembered(900_000, GovernorFlags::default(), t0);
        let (mut a, mut b) = (mk(), mk());
        let mut state = 0x0b4d_51c0_ffee_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let (mut t, mut sent) = (t0, 0u64);
        for i in 0..100_000u32 {
            let r = next();
            t += Duration::from_millis(1 + r % 40);
            match (r >> 8) % 8 {
                0..=3 => {
                    let cap = ((r >> 16) % (DEPTH as u64 + 1)) as usize;
                    let floor = crate::encode::MIN_BITRATE_BPS;
                    let x = a.pre_encode_tick(3_000_000, floor, true, cap, t);
                    let y = b.pre_encode_tick(3_000_000, floor, true, cap, t);
                    assert_eq!(x, y, "frame, event {i}");
                    sent += (r >> 24) % 20_000;
                }
                4 => {
                    let x = a.on_gate_skip(GateSkip::Backpressure(t));
                    let y = b.on_gate_skip(GateSkip::Backpressure(t));
                    assert_eq!(x, y, "skip, event {i}");
                }
                5 => {
                    let rate = PathRate {
                        bps: Some(200_000 + (r >> 16) as u32 % 4_000_000),
                        measured: r & 1 == 0,
                        loss: (r >> 40) % 4,
                        ..PathRate::default()
                    };
                    a.note_path_rate(PathRate {
                        held: r & 2 == 0,
                        envelope_ms: ((r >> 20) % 400) as u16,
                        ..rate
                    });
                    b.note_path_rate(rate);
                }
                _ => {
                    let avg = 20 + ((r >> 16) % 900) as u16;
                    let min = 10 + ((r >> 28) % 200) as u16;
                    let rtt = 20 + ((r >> 36) % 200) as u16;
                    let link = viewer_rate::pack_link(
                        100_000 + ((r >> 44) % 3_000_000) as u32,
                        ((r >> 52) % 400) as i16 - 100,
                    );
                    let report = viewer_rate::pack_report(5 + (r % 26) as u32, r & 4 == 0);
                    let window = |g: &mut RateGovernor| {
                        g.tick_viewer_window(
                            t,
                            30,
                            || report,
                            || viewer_rate::pack_age(avg, min.min(avg), rtt),
                            || link,
                            true,
                            |o| o,
                            sent,
                        )
                        .map(|w| format!("{w:?}"))
                    };
                    let (x, y) = (window(&mut a), window(&mut b));
                    assert_eq!(x, y, "window, event {i}");
                }
            }
            assert_eq!(a.applied_bps(), b.applied_bps(), "event {i}");
            assert_eq!(a.relieved_floor_bps(), b.relieved_floor_bps(), "event {i}");
            assert_eq!(
                a.measured_bps(t, true),
                b.measured_bps(t, true),
                "event {i}"
            );
            assert_eq!(a.prior_walk(), b.prior_walk(), "event {i}");
            // FR-148 D51 B4f (C9): and the pump charges each encode today's
            // pts (`encode::charge`), whatever the divisor and the rate.
            let fps = 15 + (r % 46) as u32;
            assert_eq!(a.charge_interval(fps), None, "charge, event {i}");
        }
    }

    /// FR-148 D51 B4g (C15) — the measured reading names its source. With no
    /// reading, none. With the budget not acting, the blocked-send goodput.
    /// Under `on`, the budget acting and holding, its held rate: its basis is
    /// the goodput's (`backlogged`), so only the source tells them apart. The
    /// held arrival rate where it is the lower, the budget's again where it
    /// is not; and none while the budget acts without holding, the goodput
    /// not read in its place.
    #[test]
    fn the_measured_reading_names_its_source() {
        use crate::encode::pipe::PathRate;
        let start = Instant::now();
        let t1 = start + Duration::from_secs(2);
        let source = |g: &RateGovernor| {
            let v = g.path_view(t1, None);
            (v.measured.map(|n| (n.bps, n.word())), v.measured_by)
        };
        let mut g = gov(start);
        g.tick_viewer_window(t1, 30, || 0, || 0, || 0, true, |o| o, 0)
            .expect("window due");
        assert_eq!(source(&g), (None, None), "no reading");
        // A relayed keyframe blocked: the goodput folds.
        let mut g = gov(start);
        g.goodput_sink()
            .record_frame(625_000, Duration::from_millis(900), true);
        g.note_window_sender(t2_sender(2));
        g.tick_viewer_window(t1, 30, || 0, || 0, || 0, true, |o| o, 0)
            .expect("window due");
        let goodput = g.blocked_send_bps(t1).expect("the goodput folded");
        g.note_path_rate(PathRate {
            acts: false,
            ..acting(1_400_000, true, 200)
        });
        let backlogged = |bps: u32| Some((bps, "backlogged"));
        assert_eq!(
            source(&g),
            (backlogged(goodput), Some(MeasuredBy::Goodput)),
            "the budget does not act"
        );
        g.note_path_rate(acting(1_400_000, true, 200));
        assert_eq!(
            source(&g),
            (backlogged(1_400_000), Some(MeasuredBy::Budget)),
            "the budget acts and held"
        );
        g.link_rx_bps = Some((1_000_000, t1));
        g.path.link_rx = Basis::AppLimited;
        assert_eq!(
            source(&g),
            (Some((1_000_000, "app-limited")), Some(MeasuredBy::Arrival)),
            "the arrival rate is the lower"
        );
        g.link_rx_bps = Some((2_000_000, t1));
        assert_eq!(
            source(&g),
            (backlogged(1_400_000), Some(MeasuredBy::Budget)),
            "the arrival rate is not"
        );
        g.link_rx_bps = None;
        g.note_path_rate(acting(1_400_000, false, 200));
        assert_eq!(source(&g), (None, None), "the budget acts, no hold");
    }
}

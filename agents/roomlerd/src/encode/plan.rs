// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-70 M3a — the media PLAN, in SHADOW.
//!
//! # What this is
//!
//! [`Plan`] is everything the media loop is allowed to know (FR-70 M3's
//! design, `docs/fr/FR-70-media-pipeline-leaves-the-runtime.md` §"M3 — the
//! plan handoff"): the encoder's target rate, the geometry to encode at, the
//! cadence, and the byte budget the backpressure gate judges the send queue
//! against. Today `media_pump_ffmpeg_dc` computes each of those INLINE,
//! scattered through one pass of a 3,000-line loop, against live shared
//! state. [`produce`] computes the same numbers ONCE, from explicit inputs, as
//! a pure function — the form M3b will make the loop read.
//!
//! # Why a shadow, and why at frame cadence
//!
//! M3a changes NO behaviour. The pump keeps every inline decision; at the top
//! of each pass it also calls [`produce`] with the facts it has at that
//! instant and, where each inline value is computed later in the same pass,
//! compares the two ([`Shadow`]). A divergence is counted per field
//! ([`PlanDivergences`], on the heartbeat as `plan_divergences`) and logged
//! once per session per field with both values. A count only adds, so one
//! divergence reads on every heartbeat after it. The retirement gate is the
//! operator's standing rule: the counter must read **fleet-zero** before M3b
//! flips the loop to read the plan and deletes the inline arithmetic — a
//! divergence is then a measured bug in the plan rather than a regression in
//! the field, which is the only way "byte-identical" is provable rather than
//! asserted.
//!
//! The comparison runs every pass, not once per viewer window: the AIMD's
//! additive increase is a per-frame step (`pre_encode_tick` runs per frame),
//! so a window-cadence plan would diverge by construction and its counter
//! could never reach zero (spec §"Crux (2) resolved").
//!
//! # Facts and decisions
//!
//! The producer reads FACTS and emits DECISIONS. What stays with the loop,
//! deliberately: the bytes in flight, the send-wait EMA (`direct_wait_ema_ms`
//! and its `gate_send_wait` place — a running MEASUREMENT of the loop's own
//! queue, which only reads like a decision), the live age of the frame on the
//! wire, and the captured frame's own dims. What the plan owns: the budget
//! triple, the cadence, the geometry, the target.
//!
//! One input is itself a decision the plan does not own yet: `target_bps` is
//! the rate governor's standing target. The controller is stateful and ticks
//! inside the pass; making it the plan's is M4 (one controller), not M3. What
//! M3a CAN check about the rate is the two places the loop decides a rate by
//! itself. The inline open is judged against the first target the controller
//! hands that encoder ([`Shadow::note_built`], [`Shadow::after_tick`]). A
//! make-before-break replacement is judged the same way when the plan held
//! while it opened ([`Shadow::note_rate_swap_adopted`],
//! [`Shadow::note_dims_swap_adopted`]); one that arrives after the plan moved
//! was built at the plan of its moment, and is counted apart
//! ([`Shadow::stale_swaps`]), never as a divergence. The field's first reads
//! (2026-10-21) found one of each: a stale `hevc_qsv` swap in a stall cascade,
//! and a `hevc_vaapi` dims replacement opened at the old frame's rate — the
//! shadow's first genuine finding.
//!
//! # Lags and the first tick (M3a.3)
//!
//! A week of fleet reads (AC10, 2026-10-21 → 28) found two more things the
//! counters treated as bugs and were not:
//!
//! - **A lag.** The plan is produced at the top of the pass; the inline
//!   geometry is decided after the capture, from inputs read again. When an
//!   input moves between the two reads — the host's display mode switched
//!   under a live session (the snapshot's native dims are the LAST frame's), an
//!   `rc:resolution` or a Priority write landing in between, a viewer joining
//!   a shared pipeline — the two disagree for a frame, and M3b's loop, reading
//!   the plan, would do the same. M3a's design listed these as legitimate
//!   one-frame lags. So [`Shadow::check_dims`] takes the inline INPUTS as well
//!   as the inline dims: a geometry difference whose inputs moved is a lag
//!   ([`Shadow::lags`], logged once with the input that moved), and one whose
//!   inputs held is still a `dims` divergence. An inline open built from moved
//!   inputs is judged the same way, in its own pass (M3a.5).
//! - **The first tick after an adoption.** A replacement adopted while the
//!   plan held is judged against the controller's first target after it. When
//!   the controller moves on that very tick, within the ceiling the loop knew —
//!   the field's cases were congestion decreases, often set off by the
//!   replacement's own IDR — the re-rate is the plan's own movement one tick
//!   later: M3a.1's stale adoption, a tick on. Counted apart
//!   ([`Shadow::stale_ticks`]). A replacement built ABOVE that ceiling is
//!   still a divergence: the loop could have known it, which is exactly the
//!   `hevc_vaapi` finding above. An inline open is never exempt — the loop can
//!   consult the controller before it opens, and the pump now does on a
//!   persisted transport flip (the field's other finding). M3a.5 exempts one
//!   that did consult it, when its first frame comes on a later pass.
//!
//! # A rebuild opens at the plan (M3a.4)
//!
//! The next week (AC10, 2026-11-11 to 18) found inline opens built at a size
//! or a rate the plan no longer held, each in a burst where full rebuilds and
//! M2 dims swaps ran within a second of each other. The open's size came from
//! M2's pin, and its rate from the controller's target before the pass's tick.
//! So the pump now asks [`frame_step`] whether a frame is served, dropped or
//! opened for, and an open is at the plan's target and geometry, never the
//! pin's. [`open_rate`] ticks the controller first on every rebuild, as M3a.3
//! did for the flip. [`dims_move`] is M2's own decision, beside them, so one
//! test rig drives all three in the pump's order.
//!
//! # A build is judged once, against the plan of its moment (M3a.5)
//!
//! The week after that (AC10, read 2026-11-27) found two sessions whose one
//! divergence was the shadow's own: a build judged on a later pass, against a
//! plan that was not its own. The shadow kept one record of the encoder the
//! loop built and only the tick retired it, but a pass can drop its frame
//! before its tick, and a stale adoption left the record of the encoder it
//! replaced. So each build is judged once, against the plan of its moment:
//!
//! - its dims where the loop decides them: an inline open in its own pass
//!   ([`Shadow::note_built`], under the lag rule above), and a dims swap at
//!   its adoption, against the plan of the inputs it was decided from
//!   ([`Shadow::swap_spawn`], [`Shadow::note_dims_swap_adopted`]);
//! - its rate at its first frame's tick ([`Shadow::after_tick`]), where a move
//!   the controller made since the rate was decided, within the ceiling, is a
//!   stale tick, for an inline open that consulted the controller as for a
//!   replacement;
//! - and the next build replaces its record, a stale one too.
//!
//! A lag closes in its own pass, and a stale adoption or a stale tick never
//! reaches a divergence count.
//!
//! # Knobs are read once
//!
//! Every env/config knob the inline arithmetic reads per frame
//! (`constrained_queue_ms`, `direct_queue_ms`, `relay_max_bps`, the HRD
//! window, the dial caps, the refined-rung cap) is resolved ONCE per pump into
//! [`PlanKnobs`]. The shadow therefore adds no env read to the frame path
//! (FR-70's rule), and the producer is pure over its arguments — which is what
//! lets the B0 cells (`encode::sim`) drive it with the constants explicit.

use super::policy::{self, DimsInputs};
use super::rate_profile;
use super::resample;
use super::{PriorityCaps, aimd};
use crate::peer::TargetResolution;

/// FR-70 M3 — everything the media loop is allowed to know. Computed by
/// [`produce`], published whole, read once at the top of a pass (M3b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Plan {
    /// The encoder's target, on the ladder `FfmpegEncoder::set_bitrate`
    /// applies (`aimd::coarsen_bitrate`). `0` = the controller has no target
    /// yet (before its first tick).
    pub bitrate_bps: u32,
    /// The geometry to encode at — the user's pick merged with the caps and
    /// the rungs (`policy::plan_dims`), reduced to the dims the resampler
    /// produces (`resample::target_dims`), so the loop never merges anything.
    /// `(0, 0)` = no geometry yet (no frame has been captured, so the native
    /// dims are unknown); the session's first encoder opens at its frame's
    /// dims, which M2 left to the inline open on purpose.
    pub dims: (u32, u32),
    /// Frames per second the loop may produce — the pacer's only input: the
    /// governor's paced rate when one is engaged, else the target. ⚠️ The loop
    /// paces itself ONLY below the target (`FpsPace` engages ≥ 5 fps under
    /// it); at the target the capture backend is the single pacer (rc.93 — a
    /// second pump-side floor halved the fps).
    pub fps: u32,
    /// FR-59 P2 / FR-74 P1 — the byte budget the gate compares the send queue
    /// against (the soft budget: link time at the reference rate).
    pub queue_budget_bytes: usize,
    /// FR-74 P1b — the HARD byte ceiling: the larger of the soft budget and
    /// the encoder's own HRD reservoir. At or above it the gate trips on bytes
    /// alone. Equal to the soft budget on a constrained path, whose gate has
    /// no wait qualifier.
    pub queue_hard_budget_bytes: usize,
    /// FR-74 P1b — the measured-wait bound (ms) that qualifies the soft
    /// budget: between soft and hard the gate trips only once the measured
    /// send wait has crossed it. `0` = bytes alone trip the soft budget — the
    /// constrained path's rule, which is what lets ONE gate function serve
    /// both transports (locked by
    /// `the_constrained_gate_is_the_unified_gate_at_lag_zero`).
    pub queue_lag_bound_ms: u64,
    /// Monotonic. Moves when any other field moves, so a plan recomputed with
    /// identical fields costs the reader nothing.
    pub epoch: u64,
}

impl Plan {
    /// Every field but the epoch.
    fn same_fields(&self, other: &Plan) -> bool {
        self.bitrate_bps == other.bitrate_bps
            && self.dims == other.dims
            && self.fps == other.fps
            && self.queue_budget_bytes == other.queue_budget_bytes
            && self.queue_hard_budget_bytes == other.queue_hard_budget_bytes
            && self.queue_lag_bound_ms == other.queue_lag_bound_ms
    }

    /// Does the plan carry a geometry yet?
    pub(crate) fn has_dims(&self) -> bool {
        self.dims.0 > 0 && self.dims.1 > 0
    }
}

/// The knobs the inline arithmetic reads from env/config per frame, resolved
/// ONCE per pump. Env and config are process-stable (the config registry is a
/// `OnceLock`), so these are the numbers the per-frame readers return — locked
/// by `the_knobs_resolve_to_what_the_per_frame_readers_read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlanKnobs {
    /// `encode::relay_max_bps` — the constrained path's nominal ceiling.
    pub relay_max_bps: u32,
    /// `rate_profile::constrained_queue_ms`.
    pub constrained_queue_ms: u64,
    /// `encode::constrained_queue_measured_enabled` (FR-59 P2).
    pub constrained_queue_measured: bool,
    /// `rate_profile::direct_queue_ms`.
    pub direct_queue_ms: u64,
    /// `rate_profile::direct_hrd_pct` — the HRD window before the AV1 floor.
    pub direct_hrd_pct: usize,
    /// `encode::idle_refine_cap_long_edge` — the refined rung's own bound.
    pub refined_cap: Option<u32>,
    /// The Priority dial's dims-caps (`encode::priority_relay_cap`).
    pub priority_caps: PriorityCaps,
}

impl PlanKnobs {
    pub(crate) fn from_env() -> Self {
        Self {
            relay_max_bps: super::relay_max_bps(),
            constrained_queue_ms: rate_profile::constrained_queue_ms(),
            constrained_queue_measured: super::constrained_queue_measured_enabled(),
            direct_queue_ms: rate_profile::direct_queue_ms(),
            direct_hrd_pct: rate_profile::direct_hrd_pct(),
            refined_cap: super::idle_refine_cap_long_edge(),
            priority_caps: PriorityCaps::from_env(),
        }
    }
}

/// The facts the producer reads — what the pump has at the top of a pass.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlanInputs {
    /// Relay-TCP (constrained) or direct, as the pump currently believes.
    pub constrained: bool,
    /// The capture/encode target rate for this transport.
    pub target_fps: u32,
    /// The governor's fps-first pace (`None` = run at the target).
    pub paced_fps: Option<u32>,
    /// `RateGovernor::pipe_bps` — what the session believes the pipe carries
    /// (constrained only; `None` on direct).
    pub pipe_bps: Option<u32>,
    /// The ceiling the LAST planned frame was planned at (`last_ceiling_bps`):
    /// the direct budget's reference. ⚠️ Deliberately the previous frame's —
    /// the pump reads it at the gate, before this frame's ceiling exists, and
    /// a plan that used this frame's would gate differently on every pass
    /// the ceiling moves.
    pub ceiling_reference_bps: u32,
    /// The live encoder's HRD window (`rate_profile::hrd_pct_for` of its
    /// name), or the direct default while no encoder exists.
    pub hrd_pct: usize,
    /// The rate governor's standing target (`applied_bps`; 0 = none yet).
    pub target_bps: u32,
    /// The geometry decision's inputs: the native dims last captured (0 =
    /// unknown) and the control-plane state (merged target, merged dial cap,
    /// slow-link cap, refine state, soft cap).
    pub dims: DimsInputs,
}

/// The producer: the plan from the facts, pure. `prev` carries the epoch.
pub(crate) fn produce(inputs: &PlanInputs, knobs: &PlanKnobs, prev: Option<&Plan>) -> Plan {
    let (queue_budget_bytes, queue_hard_budget_bytes, queue_lag_bound_ms) = if inputs.constrained {
        // FR-59 P2 — denominated in the measured pipe when one is believed,
        // never above the nominal band; bytes alone trip it.
        let reference = rate_profile::constrained_queue_reference_bps(
            knobs.relay_max_bps,
            inputs.pipe_bps,
            knobs.constrained_queue_measured,
        );
        let budget =
            rate_profile::constrained_queue_budget_bytes_at(reference, knobs.constrained_queue_ms);
        (budget, budget, 0)
    } else {
        // FR-74 P1/P1b — link time at the path's ceiling, qualified by the
        // measured wait up to the encoder's own reservoir.
        let soft = rate_profile::direct_queue_budget_bytes_at(
            inputs.ceiling_reference_bps,
            knobs.direct_queue_ms,
        );
        let hard = rate_profile::direct_queue_hard_budget_bytes_at(
            inputs.ceiling_reference_bps,
            inputs.hrd_pct,
            knobs.direct_queue_ms,
        );
        (soft, hard, knobs.direct_queue_ms)
    };
    let fps = inputs.paced_fps.unwrap_or(inputs.target_fps);
    let fields = Plan {
        bitrate_bps: ladder(inputs.target_bps),
        dims: geometry(&inputs.dims),
        fps,
        queue_budget_bytes,
        queue_hard_budget_bytes,
        queue_lag_bound_ms,
        epoch: 0,
    };
    let epoch = match prev {
        Some(p) if p.same_fields(&fields) => p.epoch,
        Some(p) => p.epoch + 1,
        None => 1,
    };
    Plan { epoch, ..fields }
}

/// The plan's geometry for these inputs: the native dims under
/// `policy::plan_dims`' target, or (0, 0) while the native dims are unknown.
fn geometry(inputs: &DimsInputs) -> (u32, u32) {
    if inputs.native_w == 0 || inputs.native_h == 0 {
        return (0, 0);
    }
    resample::target_dims(
        (inputs.native_w, inputs.native_h),
        policy::plan_dims(inputs).effective_target,
    )
}

/// The controller's target on the ladder the encoder applies; 0 stays 0.
fn ladder(bps: u32) -> u32 {
    if bps == 0 {
        0
    } else {
        aimd::coarsen_bitrate(bps)
    }
}

/// Per-field divergence counts for the session, on the heartbeat as
/// `plan_divergences`. The retirement gate reads every field at zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PlanDivergences {
    /// `queue_budget_bytes` vs the pump's `queue_budget`.
    pub budget: u32,
    /// `queue_hard_budget_bytes` vs the pump's `hard_budget`.
    pub hard_budget: u32,
    /// `queue_lag_bound_ms` vs the bound the pump's gate used.
    pub lag_bound: u32,
    /// The plan's triple through the one gate function vs the pump's verdict.
    pub gate: u32,
    /// `fps` vs the rate the pump paced to.
    pub fps: u32,
    /// `dims` vs the dims the pump planned this frame at, and the dims of
    /// every encoder the loop built.
    pub dims: u32,
    /// `bitrate_bps` vs the rate every encoder the loop built was built at.
    pub bitrate: u32,
}

impl std::fmt::Display for PlanDivergences {
    /// The heartbeat's form — the same shape `Debug` prints, written out so
    /// the fields are READ by the build that ships it (a derived `Debug` is
    /// not a read to the dead-code lint) and so a grep is stable:
    /// `PlanDivergences { budget: 0, hard_budget: 0, … }`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PlanDivergences {{ budget: {}, hard_budget: {}, lag_bound: {}, gate: {}, fps: {}, dims: {}, bitrate: {} }}",
            self.budget,
            self.hard_budget,
            self.lag_bound,
            self.gate,
            self.fps,
            self.dims,
            self.bitrate
        )
    }
}

/// One field's first divergence of the session, with both values, for the
/// pump to log once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FirstDivergence {
    pub field: &'static str,
    pub plan: String,
    pub inline: String,
}

/// The plan a make-before-break replacement was SPAWNED under. The loop
/// decides a replacement's rate (and, for a dims swap, its dims) at the spawn,
/// so that plan — not the one in force when the open finishes, which on a
/// rebuild-bound backend can be seconds later — is the one it is judged
/// against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SwapSpawn {
    bitrate_bps: u32,
    dims: (u32, u32),
}

/// A replacement adopted after the plan moved while it opened, for the pump
/// to log once per session. Not a divergence: the loop built it at the plan's
/// values of the moment it decided them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StaleSwap {
    pub kind: &'static str,
    pub built_bps: u32,
    pub spawned_bps: u32,
    pub now_bps: u32,
    pub spawned_dims: (u32, u32),
    pub now_dims: (u32, u32),
}

/// M3a.5 — what a dims replacement's adoption hands the pump to log: the
/// session's first stale adoption, and a first divergence of a replacement
/// built at dims the plan that held does not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Adoption {
    pub stale: Option<StaleSwap>,
    pub diverged: Vec<FirstDivergence>,
}

/// M3a.3 — a geometry difference whose inputs moved between the plan's
/// snapshot and the inline read, for the pump to log once per session. Not a
/// divergence: the plan was produced from the facts of its moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanLag {
    /// The first input that differs: `native`, `target`, `priority_cap`,
    /// `slow_link_cap`, `refine` or `soft_cap`.
    pub moved: &'static str,
    pub plan_dims: (u32, u32),
    pub inline_dims: (u32, u32),
}

/// The first of [`DimsInputs`]'s fields that differs, by name; `None` when
/// they are equal. The order is the order a field read would ask in.
fn moved_input(plan: &DimsInputs, inline: &DimsInputs) -> Option<&'static str> {
    if (plan.native_w, plan.native_h) != (inline.native_w, inline.native_h) {
        Some("native")
    } else if plan.merged_target != inline.merged_target {
        Some("target")
    } else if plan.merged_priority_cap != inline.merged_priority_cap {
        Some("priority_cap")
    } else if plan.slow_link_cap != inline.slow_link_cap {
        Some("slow_link_cap")
    } else if plan.refined != inline.refined || plan.refined_cap != inline.refined_cap {
        Some("refine")
    } else if plan.soft_cap != inline.soft_cap {
        Some("soft_cap")
    } else {
        None
    }
}

/// M3a.5: the rate of the live encoder the loop built, waiting for the first
/// tick of a pass that encodes, where [`Shadow::after_tick`] judges it. Its
/// dims were judged when it was built, against the plan of that moment.
#[derive(Debug, Clone, Copy)]
struct Built {
    /// `open`, `dims` or `rate`, for the stale tick's line.
    kind: &'static str,
    /// The rate it was built at (raw; laddered at the comparison).
    rate_bps: u32,
    /// Its dims, for the stale tick's line; `None` for a rate replacement,
    /// whose dims are the live encoder's.
    dims: Option<(u32, u32)>,
    /// The rung of the plan its rate was decided under: the controller's
    /// answer an inline open consulted, or the plan a replacement was spawned
    /// under. A tick that has moved from it is the controller's own move.
    /// `None` for an open that had no controller to consult (the session's
    /// first): its first tick is the controller's first answer.
    decided_bps: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Budget,
    HardBudget,
    LagBound,
    Gate,
    Fps,
    Dims,
    Bitrate,
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Field::Budget => "budget",
            Field::HardBudget => "hard_budget",
            Field::LagBound => "lag_bound",
            Field::Gate => "gate",
            Field::Fps => "fps",
            Field::Dims => "dims",
            Field::Bitrate => "bitrate",
        }
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// The shadow: the plan of the current pass, the counters, and the encoder
/// the loop built since the controller last ticked.
#[derive(Debug, Default)]
pub(crate) struct Shadow {
    plan: Option<Plan>,
    counts: PlanDivergences,
    /// Fields already reported this session (one log line per field).
    reported: u8,
    /// M3a.5: the rate of the encoder the loop built last, until the first
    /// tick of a pass that encodes judges it. Every build replaces it, a stale
    /// adoption too: an encoder replaced before it encoded a frame never
    /// served one, and nothing it was built from is the plan's any more.
    built: Option<Built>,
    /// Replacements adopted after the plan moved while they opened.
    stale_swaps: u32,
    /// Whether this session's first stale adoption has been reported.
    stale_reported: bool,
    /// M3a.3 — the geometry inputs the current plan was produced from.
    inputs: Option<DimsInputs>,
    /// M3a.3 — the geometry inputs the inline arithmetic read last
    /// ([`Self::check_dims`]); the inline open of the same pass builds from
    /// them.
    inline_inputs: Option<DimsInputs>,
    /// M3a.3 — geometry differences whose inputs moved between the two reads.
    lags: u32,
    /// The session's first lag, waiting for the pump to log it.
    lag_pending: Option<PlanLag>,
    /// Whether this session's first lag has been handed out.
    lag_reported: bool,
    /// M3a.3 — builds the controller re-rated on their first frame's tick,
    /// within the ceiling: replacements, and inline opens that asked it (M3a.5).
    stale_ticks: u32,
    /// The session's first stale tick, waiting for the pump to log it.
    stale_tick_pending: Option<StaleSwap>,
    /// Whether this session's first stale tick has been handed out.
    stale_tick_reported: bool,
}

impl Shadow {
    /// The pass's plan, from the facts at this instant.
    pub(crate) fn produce(&mut self, inputs: &PlanInputs, knobs: &PlanKnobs) -> Plan {
        let plan = produce(inputs, knobs, self.plan.as_ref());
        self.plan = Some(plan);
        self.inputs = Some(inputs.dims);
        plan
    }

    /// The current pass's plan (test seam; the pump reads the counters).
    #[cfg(test)]
    pub(crate) fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    /// How many distinct plans the session has had (0 = none yet).
    pub(crate) fn epoch(&self) -> u64 {
        self.plan.map_or(0, |p| p.epoch)
    }

    pub(crate) fn counts(&self) -> PlanDivergences {
        self.counts
    }

    fn diverge<T: PartialEq + std::fmt::Debug>(
        &mut self,
        field: Field,
        plan: T,
        inline: T,
        out: &mut Vec<FirstDivergence>,
    ) {
        if plan == inline {
            return;
        }
        let count = match field {
            Field::Budget => &mut self.counts.budget,
            Field::HardBudget => &mut self.counts.hard_budget,
            Field::LagBound => &mut self.counts.lag_bound,
            Field::Gate => &mut self.counts.gate,
            Field::Fps => &mut self.counts.fps,
            Field::Dims => &mut self.counts.dims,
            Field::Bitrate => &mut self.counts.bitrate,
        };
        *count = count.saturating_add(1);
        if self.reported & field.bit() == 0 {
            self.reported |= field.bit();
            out.push(FirstDivergence {
                field: field.name(),
                plan: format!("{plan:?}"),
                inline: format!("{inline:?}"),
            });
        }
    }

    /// The budget triple the pump's gate used this pass. On a constrained
    /// path the pump's hard budget IS its soft one and its gate has no wait
    /// bound, so the caller passes `(soft, soft, 0)`.
    pub(crate) fn check_budget(
        &mut self,
        inline_soft: usize,
        inline_hard: usize,
        inline_lag_ms: u64,
    ) -> Vec<FirstDivergence> {
        let mut out = Vec::new();
        let Some(plan) = self.plan else {
            return out;
        };
        self.diverge(
            Field::Budget,
            plan.queue_budget_bytes,
            inline_soft,
            &mut out,
        );
        self.diverge(
            Field::HardBudget,
            plan.queue_hard_budget_bytes,
            inline_hard,
            &mut out,
        );
        self.diverge(
            Field::LagBound,
            plan.queue_lag_bound_ms,
            inline_lag_ms,
            &mut out,
        );
        out
    }

    /// The gate's verdict: the plan's triple through the ONE gate function
    /// (`rate_profile::direct_gate_trips`) against the verdict the pump's two
    /// arms reached from the same facts.
    pub(crate) fn check_gate_verdict(
        &mut self,
        inflight: usize,
        measured_wait_ms: f64,
        inline_trips: bool,
    ) -> Vec<FirstDivergence> {
        let mut out = Vec::new();
        let Some(plan) = self.plan else {
            return out;
        };
        let trips = rate_profile::direct_gate_trips(
            inflight,
            plan.queue_budget_bytes,
            plan.queue_hard_budget_bytes,
            measured_wait_ms,
            plan.queue_lag_bound_ms,
        );
        self.diverge(Field::Gate, trips, inline_trips, &mut out);
        out
    }

    /// The rate the pump paced this pass to.
    pub(crate) fn check_fps(&mut self, inline_fps: u32) -> Vec<FirstDivergence> {
        let mut out = Vec::new();
        let Some(plan) = self.plan else {
            return out;
        };
        self.diverge(Field::Fps, plan.fps, inline_fps, &mut out);
        out
    }

    /// The dims this frame's plan produces, against the dims the inline
    /// arithmetic decided from `inline_inputs`. Skipped (no count) while the
    /// plan has no geometry — before the first frame. M3a.3: when the two
    /// disagree and an input moved between the plan's snapshot and the inline
    /// read, it is a lag ([`Self::lags`]), not a divergence.
    pub(crate) fn check_dims(
        &mut self,
        inline_dims: (u32, u32),
        inline_inputs: &DimsInputs,
    ) -> Vec<FirstDivergence> {
        self.inline_inputs = Some(*inline_inputs);
        let mut out = Vec::new();
        let Some(plan) = self.plan else {
            return out;
        };
        if !plan.has_dims() || plan.dims == inline_dims {
            return out;
        }
        if let Some(moved) = self.inputs_moved() {
            self.note_lag(moved, plan.dims, inline_dims);
            return out;
        }
        self.diverge(Field::Dims, plan.dims, inline_dims, &mut out);
        out
    }

    /// The first geometry input that differs between the plan's snapshot and
    /// the inline read; `None` when they agree or either is missing.
    fn inputs_moved(&self) -> Option<&'static str> {
        match (self.inputs.as_ref(), self.inline_inputs.as_ref()) {
            (Some(plan), Some(inline)) => moved_input(plan, inline),
            _ => None,
        }
    }

    fn note_lag(&mut self, moved: &'static str, plan_dims: (u32, u32), inline_dims: (u32, u32)) {
        self.lags = self.lags.saturating_add(1);
        if !self.lag_reported {
            self.lag_reported = true;
            self.lag_pending = Some(PlanLag {
                moved,
                plan_dims,
                inline_dims,
            });
        }
    }

    /// The session's first lag, once, for the pump to log.
    pub(crate) fn take_lag(&mut self) -> Option<PlanLag> {
        self.lag_pending.take()
    }

    /// Geometry differences whose inputs moved between the two reads.
    pub(crate) fn lags(&self) -> u32 {
        self.lags
    }

    /// The loop's inline open built an encoder at `open`'s rate and `dims`.
    ///
    /// M3a.5: each build is judged once, against the plan of its own moment.
    /// Its dims are judged here, against this pass's plan: the open built from
    /// the inputs [`Self::check_dims`] read this pass, so a difference whose
    /// inputs moved from the plan's snapshot is a lag (M3a.3). Its rate waits
    /// for the first tick of a pass that encodes ([`Self::after_tick`]). A
    /// pass can drop its frame after the open (the drain, the decode-pressure
    /// skip), and a later pass's plan is not this open's: the AC10 week's
    /// second read (2026-11-27) found an open that had agreed with the plan
    /// judged on such a later pass, after the controller had moved, and
    /// counted a `bitrate` divergence for the rest of the session.
    pub(crate) fn note_built(&mut self, open: OpenRate, dims: (u32, u32)) -> Vec<FirstDivergence> {
        let mut out = Vec::new();
        if let Some(plan) = self.plan
            && plan.has_dims()
            && plan.dims != dims
        {
            match self.inputs_moved() {
                Some(moved) => self.note_lag(moved, plan.dims, dims),
                None => self.diverge(Field::Dims, plan.dims, dims, &mut out),
            }
        }
        self.built = Some(Built {
            kind: "open-first-tick",
            rate_bps: open.bps,
            dims: Some(dims),
            decided_bps: open.consulted.then(|| ladder(open.bps)),
        });
        out
    }

    /// The controller ticked on a pass that encodes: its target becomes the
    /// plan's `bitrate_bps` (an epoch move when it changed — the controller
    /// republishing, not a divergence), and the encoder the loop built last,
    /// whose first frame this is, has its rate judged: built at the plan's
    /// rung. A `bitrate` divergence here is an encoder the controller re-rates
    /// before its first frame although it did not move: on a rebuild-bound
    /// backend, a second open.
    ///
    /// M3a.3: `ceiling_bps` is the ceiling the loop planned this frame at. An
    /// encoder whose rate the controller has moved from since its rate was
    /// decided ([`Built::decided_bps`]), while its rate sat within that
    /// ceiling, is a stale tick ([`Self::stale_ticks`]): the controller's own
    /// move. Above the ceiling it stays a divergence (the loop could have
    /// known). M3a.5: an inline open that consulted the controller is exempt
    /// as a replacement is, and the move is read from the rung the build was
    /// decided under, not from this pass's snapshot: a pass that ends before
    /// its tick carries the build to a later one. Only the session's first
    /// open, which had nothing to consult, is never exempt.
    pub(crate) fn after_tick(&mut self, target_bps: u32, ceiling_bps: u32) -> Vec<FirstDivergence> {
        let mut out = Vec::new();
        let Some(mut plan) = self.plan else {
            self.built = None;
            return out;
        };
        let bitrate = ladder(target_bps);
        if bitrate != plan.bitrate_bps {
            plan.bitrate_bps = bitrate;
            plan.epoch += 1;
            self.plan = Some(plan);
        }
        if let Some(built) = self.built.take() {
            let built_rung = ladder(built.rate_bps);
            if built_rung != plan.bitrate_bps {
                let moved = built.decided_bps.is_some_and(|d| d != plan.bitrate_bps);
                if moved && built_rung <= ladder(ceiling_bps) {
                    self.note_stale_tick(&built, &plan);
                } else {
                    self.diverge(Field::Bitrate, plan.bitrate_bps, built_rung, &mut out);
                }
            }
        }
        out
    }

    fn note_stale_tick(&mut self, built: &Built, now: &Plan) {
        self.stale_ticks = self.stale_ticks.saturating_add(1);
        if self.stale_tick_reported {
            return;
        }
        self.stale_tick_reported = true;
        self.stale_tick_pending = Some(StaleSwap {
            kind: built.kind,
            built_bps: built.rate_bps,
            spawned_bps: built.decided_bps.unwrap_or(now.bitrate_bps),
            now_bps: now.bitrate_bps,
            spawned_dims: built.dims.unwrap_or(now.dims),
            now_dims: now.dims,
        });
    }

    /// The session's first stale tick, once, for the pump to log.
    pub(crate) fn take_stale_tick(&mut self) -> Option<StaleSwap> {
        self.stale_tick_pending.take()
    }

    /// Builds the controller re-rated on their first frame's tick, within the
    /// ceiling: replacements, and inline opens that asked it (M3a.5).
    pub(crate) fn stale_ticks(&self) -> u32 {
        self.stale_ticks
    }

    /// The plan a replacement is decided under; `None` before the first plan.
    /// Its rung is the pass's plan's. M3a.5: its dims are the plan's geometry
    /// for the inputs the loop's inline read saw ([`Self::check_dims`]), which
    /// M2 decides a dims swap from. A target that moved between the plan's
    /// snapshot and that read is a lag of that pass, counted there; recorded
    /// as the snapshot's dims, it read at the adoption as a plan that moved
    /// while the swap opened (the AC10 week's second read, a `hevc_qsv`
    /// session resizing in fit mode), and the swap stayed unjudged.
    pub(crate) fn swap_spawn(&self) -> Option<SwapSpawn> {
        self.plan.map(|p| SwapSpawn {
            bitrate_bps: p.bitrate_bps,
            dims: self.inline_inputs.as_ref().map_or(p.dims, geometry),
        })
    }

    /// The loop adopted a RATE replacement built at `rate_bps`. Its rate is
    /// the one thing the loop decided for it — `rebuild_spec` copies the live
    /// encoder's dims, which M2's pin holds while a dims swap opens — so the
    /// rate is all that is judged.
    pub(crate) fn note_rate_swap_adopted(
        &mut self,
        rate_bps: u32,
        spawned: Option<SwapSpawn>,
    ) -> Option<StaleSwap> {
        self.judge_swap("rate", rate_bps, None, spawned).1
    }

    /// The loop adopted a DIMS replacement built at `rate_bps` and `dims`; it
    /// decided both. M3a.5: its dims are judged at the adoption, against the
    /// plan that held while it opened.
    pub(crate) fn note_dims_swap_adopted(
        &mut self,
        rate_bps: u32,
        dims: (u32, u32),
        spawned: Option<SwapSpawn>,
    ) -> Adoption {
        let mut diverged = Vec::new();
        let (held, stale) = self.judge_swap("dims", rate_bps, Some(dims), spawned);
        if held
            && let Some(now) = self.plan
            && now.has_dims()
            && dims != now.dims
        {
            self.diverge(Field::Dims, now.dims, dims, &mut diverged);
        }
        Adoption { stale, diverged }
    }

    /// Did the plan move while the replacement opened?
    ///
    /// **It held** — the common case, opens are milliseconds on most
    /// backends: the replacement is judged exactly as an inline open is,
    /// against the controller's first target after adoption
    /// ([`Self::after_tick`]). Re-rated on that tick although the plan did
    /// not move is a divergence: the loop built an encoder at a rate the plan
    /// does not stand behind. The field's driven read found one (2026-10-21,
    /// `hevc_vaapi`): a dims replacement for 1050×590 opened at the old
    /// 1600×900 frame's 25 M rung while the plan already aimed at the smaller
    /// frame, whose ceiling clamped the target to 8 M on the next tick.
    ///
    /// **It moved** — a QSV open runs 0.2–3.6 s and a stall cascade halves
    /// the target every few seconds, so on the field's first read (a
    /// `hevc_qsv` session) a replacement spawned at the 12 M rung arrived
    /// after the controller had reached 2.7 M. The loop built it at the plan
    /// of its moment; the re-rate is the plan's own movement. Counted apart,
    /// as `stale_swaps`, and not judged at the next tick. With no plan at the
    /// spawn there is nothing to have moved, and it is judged as held.
    ///
    /// M3a.5: either way it replaces the encoder the loop built before it,
    /// whose rate is then never judged; the first value is whether it held.
    fn judge_swap(
        &mut self,
        kind: &'static str,
        rate_bps: u32,
        dims: Option<(u32, u32)>,
        spawned: Option<SwapSpawn>,
    ) -> (bool, Option<StaleSwap>) {
        let held = |decided_bps| Built {
            kind: if dims.is_some() {
                "dims-first-tick"
            } else {
                "rate-first-tick"
            },
            rate_bps,
            dims,
            decided_bps,
        };
        self.built = None;
        let (spawned, now) = match (spawned, self.plan) {
            (Some(spawned), Some(now)) => (spawned, now),
            (_, now) => {
                self.built = Some(held(now.map(|p| p.bitrate_bps)));
                return (true, None);
            }
        };
        let geometry_known = spawned.dims.0 > 0 && spawned.dims.1 > 0 && now.has_dims();
        let moved = now.bitrate_bps != spawned.bitrate_bps
            || (dims.is_some() && geometry_known && now.dims != spawned.dims);
        if !moved {
            self.built = Some(held(Some(spawned.bitrate_bps)));
            return (true, None);
        }
        self.stale_swaps = self.stale_swaps.saturating_add(1);
        if self.stale_reported {
            return (false, None);
        }
        self.stale_reported = true;
        let line = StaleSwap {
            kind,
            built_bps: rate_bps,
            spawned_bps: spawned.bitrate_bps,
            now_bps: now.bitrate_bps,
            spawned_dims: spawned.dims,
            now_dims: now.dims,
        };
        (false, Some(line))
    }

    /// Replacements adopted after the plan moved while they opened.
    pub(crate) fn stale_swaps(&self) -> u32 {
        self.stale_swaps
    }
}

/// FR-70 M3a.4: what a pass does with the frame it captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameStep {
    /// The live encoder takes the frame, resampled to this target: M2's pin
    /// while a replacement opens, the plan's own otherwise.
    Serve(TargetResolution),
    /// The frame is dropped and nothing opens: the live encoder cannot take
    /// it, and either it cannot be brought to the plan's geometry or a cap
    /// change is still reaching the capture.
    Skip,
    /// A new encoder opens for the frame resampled to this target, the plan's
    /// own. Any replacement still opening is superseded.
    Open(TargetResolution),
}

impl FrameStep {
    /// The target the frame is resampled to; `None` when it is dropped.
    pub(crate) fn target(self) -> Option<TargetResolution> {
        match self {
            FrameStep::Serve(target) | FrameStep::Open(target) => Some(target),
            FrameStep::Skip => None,
        }
    }
}

/// FR-70 M3a.4: the facts [`frame_step`] decides from, read after the capture.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameFacts {
    /// The frame's dims as captured: a backend may already have scaled it.
    pub dims: (u32, u32),
    /// Its native dims (`Frame::native_dims`).
    pub native: (u32, u32),
    /// The resampler can scale it (BGRA); any other frame passes through.
    pub scalable: bool,
    /// A keepalive re-fed the last real frame: nothing new was captured.
    pub keepalive: bool,
    /// The plan's target for this frame (`policy::plan_dims`).
    pub plan_target: TargetResolution,
    /// M2's pin: the target the live encoder was built for, while its
    /// replacement opens in the background.
    pub pinned: Option<TargetResolution>,
    /// The live encoder's dims; `None` = no encoder.
    pub encoder: Option<(u32, u32)>,
}

/// FR-70 M3a.4: what the pump may drop after a capture-cap change before a
/// frame the live encoder cannot take opens an encoder at its own size. A
/// backend applies a cap from its next capture, so a frame or two are still in
/// flight at the old dims; and a backend that scaled its last capture under the
/// old cap re-feeds that capture on every keepalive until it captures again.
/// Refilled at every cap change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StaleBudget {
    frames: u32,
    keepalives: u32,
}

impl StaleBudget {
    /// No cap change waiting: the session's start.
    pub(crate) const SPENT: Self = StaleBudget {
        frames: 0,
        keepalives: 0,
    };
    /// A cap just changed. Three frames, as before M3a.4; and 25 keepalives,
    /// about 1.5 s at the pump's 60 ms keepalive, for a keepalive that cannot
    /// reach the plan's geometry. That is longer than DXGI-direct took to
    /// capture again after a cap lifted to none (about 0.4 s in the AC10 week)
    /// and than DRM's idle gate holds a capture back (`drm_idle::MAX_SKIP`,
    /// 1 s), and well inside the viewer's keyframe probe, sent after 6 s with
    /// no frames (`RC_STALL_PROBE_TICKS`): a wait with no bound left a capture
    /// that never comes back with no encoder.
    pub(crate) const FULL: Self = StaleBudget {
        frames: 3,
        keepalives: 25,
    };
}

/// The dims `resample::apply_target_resolution` gives a frame of `dims`.
fn resampled(dims: (u32, u32), scalable: bool, target: TargetResolution) -> (u32, u32) {
    if scalable {
        resample::target_dims(dims, target)
    } else {
        dims
    }
}

/// FR-70 M3a.4: serve the frame to the live encoder, drop it, or open a new
/// encoder for it. A new encoder opens at the plan's geometry for this frame
/// (`resample::target_dims` of its native dims under the plan's target), with
/// the frame resampled to the plan's target, never to M2's pin. So it opens at
/// neither the capture's native size while the plan caps it, nor the pinned
/// size of a swap the plan has moved past.
///
/// The AC10 week (2026-11-11 to 18) found both. An AMF host at its lock
/// screen flapped between one display (DXGI) and the virtual screen (GDI)
/// every few seconds: the pin was `Native`, so the GDI frame passed the
/// resampler uncapped and the inline open built at the virtual screen's native
/// size while the plan held a capped one. An NVENC laptop adopted a dims swap
/// up to its native size on an idle screen; the keepalive re-fed the last real
/// frame, scaled by the capture backend under the old cap, until the stale-skip
/// budget ran out and an inline open built at the old size.
///
/// A frame that cannot be brought to the plan's geometry (scaled under a cap
/// since raised; the resampler never upscales) is stale. A keepalive of one is
/// a copy of an old capture and says nothing about what the capture delivers
/// now, so it waits for a fresh capture out of an allowance of its own
/// ([`StaleBudget::FULL`]). A real one, and a keepalive past that allowance,
/// spend the frame budget, and past it open at their own size, so a backend
/// that never delivers the plan's geometry still gets an encoder (and the
/// shadow counts its dims). A frame that does reach the plan's geometry spends
/// the frame budget only while a live encoder serves, as before M3a.4.
pub(crate) fn frame_step(facts: &FrameFacts, budget: &mut StaleBudget) -> FrameStep {
    let serve = facts.pinned.unwrap_or(facts.plan_target);
    if facts.encoder == Some(resampled(facts.dims, facts.scalable, serve)) {
        return FrameStep::Serve(serve);
    }
    let reach = resampled(facts.dims, facts.scalable, facts.plan_target);
    let stale = reach != resample::target_dims(facts.native, facts.plan_target);
    if stale && facts.keepalive && budget.keepalives > 0 {
        budget.keepalives -= 1;
        return FrameStep::Skip;
    }
    if (stale || facts.encoder.is_some()) && budget.frames > 0 {
        budget.frames -= 1;
        return FrameStep::Skip;
    }
    FrameStep::Open(facts.plan_target)
}

/// FR-70 M2: the dims swap to open in the background, decided from the plan:
/// the plan's geometry for this frame and its target, when the live encoder
/// was built for another target and is not at that geometry already. `None`
/// keeps the live encoder.
pub(crate) fn dims_move(
    native: (u32, u32),
    plan_target: TargetResolution,
    built: TargetResolution,
    encoder: (u32, u32),
) -> Option<((u32, u32), TargetResolution)> {
    let dims = resample::target_dims(native, plan_target);
    (plan_target != built && dims != encoder).then_some((dims, plan_target))
}

/// FR-70 M3a.4: the rate an inline open builds its encoder at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpenRate {
    pub bps: u32,
    /// The controller ticked for this pass before the open; the pass's own
    /// tick is skipped, one tick a pass.
    pub ticked: bool,
    /// M3a.5: `bps` is the target the controller handed out, not the
    /// remembered seed under the ceiling ([`Shadow::note_built`]).
    consulted: bool,
}

/// FR-70 M3a.4: a rebuild (the controller already holds a target, or a
/// persisted transport flip dropped the encoder) ticks the controller first,
/// under the ceiling of the dims it opens at, and opens at the target it hands
/// out (`tick` runs the pass's tick and returns `applied_bps` after it).
/// Opened before the tick, a rung the tick crossed re-rated the new encoder at
/// once, a second open on a rebuild-bound backend. The controller cannot move
/// between the open and the pass's encode; a pass that drops its frame after
/// the open (the drain, the decode-pressure skip) leaves the first frame to a
/// later pass, whose tick may move for the controller's own reasons (M3a.5,
/// [`Shadow::after_tick`]). The session's first open has no controller yet:
/// it opens at the remembered seed under the ceiling, and the pass's tick
/// builds the controller from it.
pub(crate) fn open_rate(
    applied_bps: u32,
    seed_bps: Option<u32>,
    ceiling_bps: u32,
    flip_pending: bool,
    tick: impl FnOnce() -> u32,
) -> OpenRate {
    let first = || seed_bps.map_or(ceiling_bps, |s| ceiling_bps.min(s));
    if !flip_pending && applied_bps == 0 {
        return OpenRate {
            bps: first(),
            ticked: false,
            consulted: false,
        };
    }
    match tick() {
        0 => OpenRate {
            bps: first(),
            ticked: true,
            consulted: false,
        },
        target => OpenRate {
            bps: target,
            ticked: true,
            consulted: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::TargetResolution;

    const NOMINAL: u32 = 3_000_000;
    /// A ceiling above every rate these tests build at (FR-74 P1's CORPLAP-1
    /// direct ceiling): the M3a.3 first-tick rule then turns only on whether
    /// the tick moved the target.
    const CEIL: u32 = 43_000_000;

    fn knobs() -> PlanKnobs {
        PlanKnobs {
            relay_max_bps: NOMINAL,
            constrained_queue_ms: 450,
            constrained_queue_measured: true,
            direct_queue_ms: 150,
            direct_hrd_pct: 100,
            refined_cap: None,
            priority_caps: PriorityCaps::default(),
        }
    }

    fn dims(native_w: u32, native_h: u32) -> DimsInputs {
        DimsInputs {
            native_w,
            native_h,
            merged_target: TargetResolution::Native,
            merged_priority_cap: None,
            slow_link_cap: None,
            refined: false,
            refined_cap: None,
            soft_cap: None,
        }
    }

    fn constrained(pipe_bps: Option<u32>) -> PlanInputs {
        PlanInputs {
            constrained: true,
            target_fps: 30,
            paced_fps: None,
            pipe_bps,
            ceiling_reference_bps: NOMINAL,
            hrd_pct: 200,
            target_bps: 0,
            dims: dims(1920, 1200),
        }
    }

    fn direct(ceiling: u32, hrd_pct: usize) -> PlanInputs {
        PlanInputs {
            constrained: false,
            target_fps: 60,
            paced_fps: None,
            pipe_bps: None,
            ceiling_reference_bps: ceiling,
            hrd_pct,
            target_bps: 0,
            dims: dims(1920, 1200),
        }
    }

    #[test]
    fn a_constrained_budget_is_the_reference_rate_s_window_floored_at_one_chunk() {
        // No belief: 450 ms of the nominal band.
        let p = produce(&constrained(None), &knobs(), None);
        assert_eq!(p.queue_budget_bytes, 168_750);
        assert_eq!(p.queue_hard_budget_bytes, p.queue_budget_bytes);
        assert_eq!(p.queue_lag_bound_ms, 0);
        // A measured pipe UNDER the band lowers it (FR-59 P2's field case:
        // 395 kbps through a phone hotspot).
        let p = produce(&constrained(Some(395_000)), &knobs(), None);
        assert_eq!(p.queue_budget_bytes, 22_218);
        // A measurement ABOVE the band never raises it.
        let p = produce(&constrained(Some(9_000_000)), &knobs(), None);
        assert_eq!(p.queue_budget_bytes, 168_750);
        // The one-chunk floor under an absurd reference.
        let p = produce(&constrained(Some(1)), &knobs(), None);
        assert_eq!(
            p.queue_budget_bytes,
            rate_profile::CONSTRAINED_QUEUE_MIN_BYTES
        );
        // With the measured denomination OFF the band stands.
        let k = PlanKnobs {
            constrained_queue_measured: false,
            ..knobs()
        };
        let p = produce(&constrained(Some(395_000)), &k, None);
        assert_eq!(p.queue_budget_bytes, 168_750);
    }

    #[test]
    fn a_direct_budget_is_the_ceiling_s_link_time_qualified_up_to_the_reservoir() {
        // 43 Mbps direct (FR-74 P1's CORPLAP-1 ceiling): soft = 150 ms of it,
        // hard = the larger of that and the 1× reservoir (43 M / 8 bytes).
        let p = produce(&direct(43_000_000, 100), &knobs(), None);
        assert_eq!(p.queue_budget_bytes, 806_250);
        assert_eq!(p.queue_hard_budget_bytes, 5_375_000);
        assert_eq!(p.queue_lag_bound_ms, 150);
        // AV1's floor at 200 % doubles the reservoir.
        let av1 = rate_profile::hrd_pct_for("av1_qsv", 100);
        let p = produce(&direct(43_000_000, av1), &knobs(), None);
        assert_eq!(p.queue_hard_budget_bytes, 10_750_000);
        // The 48 KiB floor on a thin ceiling.
        let p = produce(&direct(1_000_000, 100), &knobs(), None);
        assert_eq!(p.queue_budget_bytes, 48 * 1024);
        assert_eq!(p.queue_hard_budget_bytes, 125_000);
    }

    #[test]
    fn no_reference_rate_means_no_gate() {
        let p = produce(&direct(0, 100), &knobs(), None);
        assert_eq!(p.queue_budget_bytes, usize::MAX);
        assert_eq!(p.queue_hard_budget_bytes, usize::MAX);
        assert!(!rate_profile::direct_gate_trips(
            usize::MAX / 2,
            p.queue_budget_bytes,
            p.queue_hard_budget_bytes,
            1_000.0,
            p.queue_lag_bound_ms
        ));
        // A zero window disables the gate on either transport.
        let k = PlanKnobs {
            constrained_queue_ms: 0,
            direct_queue_ms: 0,
            ..knobs()
        };
        assert_eq!(
            produce(&constrained(None), &k, None).queue_budget_bytes,
            usize::MAX
        );
        assert_eq!(
            produce(&direct(43_000_000, 100), &k, None).queue_hard_budget_bytes,
            usize::MAX
        );
    }

    /// The constrained gate is `inflight >= budget`, bytes alone. The plan
    /// states it as the triple `(b, b, 0)` so ONE gate function serves both
    /// transports; this locks the algebra for every inflight and every wait.
    #[test]
    fn the_constrained_gate_is_the_unified_gate_at_lag_zero() {
        let p = produce(&constrained(Some(395_000)), &knobs(), None);
        let b = p.queue_budget_bytes;
        for inflight in [0, 1, b / 2, b - 1, b, b + 1, 10 * b, usize::MAX / 2] {
            for wait_ms in [0.0, 0.1, 149.0, 150.0, 5_000.0] {
                assert_eq!(
                    rate_profile::direct_gate_trips(
                        inflight,
                        p.queue_budget_bytes,
                        p.queue_hard_budget_bytes,
                        wait_ms,
                        p.queue_lag_bound_ms,
                    ),
                    inflight >= b,
                    "inflight {inflight} wait {wait_ms}"
                );
            }
        }
    }

    #[test]
    fn fps_is_the_pace_or_the_target() {
        let paced = PlanInputs {
            paced_fps: Some(45),
            ..direct(43_000_000, 100)
        };
        assert_eq!(produce(&paced, &knobs(), None).fps, 45);
        assert_eq!(produce(&direct(43_000_000, 100), &knobs(), None).fps, 60);
    }

    #[test]
    fn dims_are_plan_dims_through_the_resampler_s_rule_or_nothing_before_the_first_frame() {
        // No frame yet: no geometry, and the shadow skips the comparison.
        let blind = PlanInputs {
            dims: dims(0, 0),
            ..constrained(None)
        };
        let p = produce(&blind, &knobs(), None);
        assert_eq!(p.dims, (0, 0));
        assert!(!p.has_dims());
        // Native, uncapped.
        let p = produce(&constrained(None), &knobs(), None);
        assert_eq!(p.dims, (1920, 1200));
        assert!(p.has_dims());
        // The slow-link cap (FR-59 P5): 1280 long edge, aspect kept.
        let capped = PlanInputs {
            dims: DimsInputs {
                slow_link_cap: Some(1280),
                ..dims(1920, 1200)
            },
            ..constrained(None)
        };
        assert_eq!(produce(&capped, &knobs(), None).dims, (1280, 800));
        // A user box BELOW native is the box; one above it is the resampler's
        // no-op (no upscaling).
        let small = PlanInputs {
            dims: DimsInputs {
                merged_target: TargetResolution::Fixed {
                    width: 944,
                    height: 590,
                },
                ..dims(1920, 1200)
            },
            ..constrained(None)
        };
        assert_eq!(produce(&small, &knobs(), None).dims, (944, 590));
        let big = PlanInputs {
            dims: DimsInputs {
                merged_target: TargetResolution::Fixed {
                    width: 2560,
                    height: 1600,
                },
                ..dims(1920, 1200)
            },
            ..constrained(None)
        };
        assert_eq!(produce(&big, &knobs(), None).dims, (1920, 1200));
    }

    #[test]
    fn bitrate_is_the_controller_s_target_on_the_ladder() {
        assert_eq!(produce(&constrained(None), &knobs(), None).bitrate_bps, 0);
        let p = produce(
            &PlanInputs {
                target_bps: 2_600_000,
                ..constrained(None)
            },
            &knobs(),
            None,
        );
        assert_eq!(p.bitrate_bps, aimd::coarsen_bitrate(2_600_000));
        assert_eq!(p.bitrate_bps, 3_000_000);
    }

    /// The heartbeat prints the counters through `Display`, in the shape the
    /// field reads grep for.
    #[test]
    fn the_counters_print_as_the_heartbeat_greps_them() {
        let d = PlanDivergences {
            budget: 1,
            hard_budget: 2,
            lag_bound: 3,
            gate: 4,
            fps: 5,
            dims: 6,
            bitrate: 7,
        };
        assert_eq!(
            d.to_string(),
            "PlanDivergences { budget: 1, hard_budget: 2, lag_bound: 3, gate: 4, fps: 5, dims: 6, bitrate: 7 }"
        );
        assert_eq!(d.to_string(), format!("{d:?}"));
    }

    #[test]
    fn the_epoch_moves_only_when_a_field_does() {
        let k = knobs();
        let a = produce(&constrained(None), &k, None);
        assert_eq!(a.epoch, 1);
        let b = produce(&constrained(None), &k, Some(&a));
        assert_eq!(b.epoch, 1, "identical fields keep the epoch");
        assert_eq!(a, b);
        let c = produce(&constrained(Some(395_000)), &k, Some(&b));
        assert_eq!(c.epoch, 2, "a budget move is a plan move");
        let d = produce(&constrained(None), &k, Some(&c));
        assert_eq!(
            d.epoch, 3,
            "moving back is another — monotonic, never reused"
        );
    }

    /// The negative control's committed half: a planted divergence is counted
    /// on its field, reported once, and never bleeds into another field.
    #[test]
    fn the_shadow_counts_each_field_and_reports_the_first_once() {
        let mut s = Shadow::default();
        // Nothing to compare against before the first plan.
        assert!(s.check_budget(1, 1, 1).is_empty());
        assert_eq!(s.counts(), PlanDivergences::default());
        let plan = s.produce(&constrained(None), &knobs());
        // The inline values, exact: no divergence.
        assert!(
            s.check_budget(plan.queue_budget_bytes, plan.queue_hard_budget_bytes, 0)
                .is_empty()
        );
        assert_eq!(s.counts(), PlanDivergences::default());
        // A planted divergence on ONE field: counted and reported once...
        let first = s.check_budget(plan.queue_budget_bytes + 1, plan.queue_hard_budget_bytes, 0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].field, "budget");
        assert_eq!(first[0].plan, plan.queue_budget_bytes.to_string());
        assert_eq!(first[0].inline, (plan.queue_budget_bytes + 1).to_string());
        // ...the second time counted, not reported.
        assert!(
            s.check_budget(plan.queue_budget_bytes + 1, plan.queue_hard_budget_bytes, 0)
                .is_empty()
        );
        assert_eq!(
            s.counts(),
            PlanDivergences {
                budget: 2,
                ..Default::default()
            }
        );
        // Other fields untouched; each has its own first report.
        let first = s.check_fps(29);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].field, "fps");
        assert_eq!(
            s.counts(),
            PlanDivergences {
                budget: 2,
                fps: 1,
                ..Default::default()
            }
        );
        // The gate verdict: the plan's triple against a pump that disagreed.
        let b = plan.queue_budget_bytes;
        assert!(
            s.check_gate_verdict(b, 0.0, true).is_empty(),
            "at the budget both trip"
        );
        let first = s.check_gate_verdict(b - 1, 0.0, true);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].field, "gate");
        assert_eq!(first[0].plan, "false");
        assert_eq!(first[0].inline, "true");
        assert_eq!(s.counts().gate, 1);
    }

    #[test]
    fn the_shadow_skips_dims_until_the_plan_has_geometry() {
        let mut s = Shadow::default();
        s.produce(
            &PlanInputs {
                dims: dims(0, 0),
                ..constrained(None)
            },
            &knobs(),
        );
        assert!(s.check_dims((1920, 1200), &dims(1920, 1200)).is_empty());
        assert_eq!(s.counts().dims, 0);
        s.produce(&constrained(None), &knobs());
        assert!(s.check_dims((1920, 1200), &dims(1920, 1200)).is_empty());
        // The inputs held and the dims did not: the divergence M3a.3 keeps.
        let first = s.check_dims((1280, 800), &dims(1920, 1200));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].field, "dims");
        assert_eq!(s.counts().dims, 1);
        assert_eq!(s.lags(), 0);
        assert_eq!(s.take_lag(), None);
    }

    #[test]
    fn an_encoder_the_loop_built_is_judged_against_the_controller_s_first_target() {
        let mut s = Shadow::default();
        let plan = s.produce(
            &PlanInputs {
                target_bps: 2_000_000,
                ..constrained(None)
            },
            &knobs(),
        );
        assert_eq!(plan.bitrate_bps, 2_000_000);
        // Built at the controller's rate (a raw 2.1 M is the same rung): clean.
        assert!(s.note_built(seed(2_100_000), (1920, 1200)).is_empty());
        assert!(s.after_tick(2_000_000, CEIL).is_empty());
        assert_eq!(s.counts(), PlanDivergences::default());
        // The controller moved mid-pass: an epoch move, not a divergence...
        let epoch = s.epoch();
        assert!(s.after_tick(2_600_000, CEIL).is_empty());
        assert_eq!(s.epoch(), epoch + 1);
        assert_eq!(s.plan().unwrap().bitrate_bps, 3_000_000);
        // ...and an encoder built at the OLD rung is the finding: the
        // controller re-rates it on its first tick (a second open on QSV).
        assert!(s.note_built(seed(2_000_000), (1920, 1200)).is_empty());
        let first = s.after_tick(2_600_000, CEIL);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].field, "bitrate");
        assert_eq!(first[0].plan, "3000000");
        assert_eq!(first[0].inline, "2000000");
        assert_eq!(s.counts().bitrate, 1);
        // Built at the plan's rate but not its dims: a dims divergence, judged
        // at the open (M3a.5).
        let first = s.note_built(seed(3_000_000), (1280, 800));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].field, "dims");
        assert!(s.after_tick(3_000_000, CEIL).is_empty());
        assert_eq!(
            s.counts(),
            PlanDivergences {
                bitrate: 1,
                dims: 1,
                ..Default::default()
            }
        );
        // A build is judged once.
        assert!(s.after_tick(3_000_000, CEIL).is_empty());
        assert_eq!(s.counts().dims, 1);
    }

    /// The field's first read (2026-10-21, a hevc_qsv session in a stall
    /// cascade): the live encoder at the 20 M rung, a rate swap spawned when
    /// the controller said 10.7 M (the 12 M rung), the target at 5.4 M while
    /// it opened, adopted, then 2.74 M (the 3 M rung) on the next tick.
    fn qsv_cascade(s: &mut Shadow) -> Option<SwapSpawn> {
        s.produce(
            &PlanInputs {
                target_bps: 19_670_400,
                ..direct(43_000_000, 100)
            },
            &knobs(),
        );
        assert!(s.after_tick(10_708_885, CEIL).is_empty());
        let spawned = s.swap_spawn();
        assert!(s.after_tick(5_396_362, CEIL).is_empty());
        spawned
    }

    #[test]
    fn a_replacement_adopted_after_the_plan_moved_is_stale_not_a_divergence() {
        let mut s = Shadow::default();
        let spawned = qsv_cascade(&mut s);
        assert_eq!(spawned.map(|p| p.bitrate_bps), Some(12_000_000));
        assert_eq!(
            s.note_rate_swap_adopted(12_000_000, spawned),
            Some(StaleSwap {
                kind: "rate",
                built_bps: 12_000_000,
                spawned_bps: 12_000_000,
                now_bps: 6_000_000,
                spawned_dims: (1920, 1200),
                now_dims: (1920, 1200),
            })
        );
        // Not judged at the next tick: the re-rate is the plan's own movement.
        assert!(s.after_tick(2_744_302, CEIL).is_empty());
        assert_eq!(s.counts(), PlanDivergences::default());
        assert_eq!(s.stale_swaps(), 1);
    }

    /// The same adoption judged the way M3a first shipped — at the
    /// controller's next tick, whatever the plan did meanwhile — is exactly
    /// the line the field logged.
    #[test]
    fn judged_at_the_next_tick_the_stale_adoption_reads_as_the_field_s_divergence() {
        let mut s = Shadow::default();
        qsv_cascade(&mut s);
        assert!(s.note_built(seed(12_000_000), (1920, 1200)).is_empty());
        let first = s.after_tick(2_744_302, CEIL);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].field, "bitrate");
        assert_eq!(first[0].plan, "3000000");
        assert_eq!(first[0].inline, "12000000");
    }

    /// The field's driven read (2026-10-21, `hevc_vaapi`, direct): the target
    /// at 22.95 M (the 25 M rung) for 1600×900; a resolution move down spawned
    /// a dims replacement for 1050×590 at `min(last_ceiling, applied)` — the
    /// OLD frame's rate — the plan held through its 4 ms open, and the
    /// controller's first tick after adoption clamped to the smaller frame's
    /// ceiling, 9.87 M (the 8 M rung). The line the agent logged, exactly.
    #[test]
    fn a_dims_replacement_opened_at_the_old_frame_s_rate_is_a_divergence() {
        let mut s = Shadow::default();
        let plan = s.produce(
            &PlanInputs {
                target_bps: 22_950_000,
                ..direct(43_000_000, 100)
            },
            &knobs(),
        );
        assert_eq!(plan.bitrate_bps, 25_000_000);
        let spawned = s.swap_spawn();
        assert_eq!(
            s.note_dims_swap_adopted(25_000_000, plan.dims, spawned)
                .stale,
            None,
            "the plan held: not stale"
        );
        let first = s.after_tick(9_873_281, 9_873_281);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].field, "bitrate");
        assert_eq!(first[0].plan, "8000000");
        assert_eq!(first[0].inline, "25000000");
        assert_eq!(s.stale_swaps(), 0);
    }

    #[test]
    fn a_replacement_built_at_the_plan_that_held_is_clean() {
        let mut s = Shadow::default();
        let plan = s.produce(
            &PlanInputs {
                target_bps: 3_000_000,
                ..constrained(None)
            },
            &knobs(),
        );
        assert_eq!(s.note_rate_swap_adopted(3_000_000, s.swap_spawn()), None);
        assert!(s.after_tick(3_000_000, CEIL).is_empty());
        assert_eq!(
            s.note_dims_swap_adopted(3_000_000, plan.dims, s.swap_spawn()),
            Adoption {
                stale: None,
                diverged: Vec::new()
            }
        );
        assert!(s.after_tick(3_000_000, CEIL).is_empty());
        assert_eq!(s.counts(), PlanDivergences::default());
        assert_eq!(s.stale_swaps(), 0);
    }

    #[test]
    fn a_dims_replacement_is_judged_on_its_dims_and_a_rate_one_is_not() {
        let mut s = Shadow::default();
        let plan = s.produce(
            &PlanInputs {
                target_bps: 3_000_000,
                ..constrained(None)
            },
            &knobs(),
        );
        // A dims replacement built at other dims than the plan aims at,
        // judged at its adoption (M3a.5).
        let adopted = s.note_dims_swap_adopted(3_000_000, (1280, 800), s.swap_spawn());
        assert_eq!(adopted.stale, None);
        assert_eq!(adopted.diverged.len(), 1);
        assert_eq!(adopted.diverged[0].field, "dims");
        assert!(s.after_tick(3_000_000, CEIL).is_empty());
        // A plan whose geometry moves while a dims replacement opens: stale,
        // and not judged at the tick.
        let spawned = s.swap_spawn();
        s.produce(
            &PlanInputs {
                target_bps: 3_000_000,
                dims: dims(2560, 1600),
                ..constrained(None)
            },
            &knobs(),
        );
        let stale = s
            .note_dims_swap_adopted(3_000_000, plan.dims, spawned)
            .stale;
        assert_eq!(stale.map(|x| x.kind), Some("dims"));
        assert!(s.after_tick(3_000_000, CEIL).is_empty());
        // A RATE replacement's geometry is the live encoder's (M2's pin), not
        // its decision: moved geometry alone neither stales nor judges it.
        let rate_spawned = s.swap_spawn();
        s.produce(
            &PlanInputs {
                target_bps: 3_000_000,
                ..constrained(None)
            },
            &knobs(),
        );
        assert_eq!(s.note_rate_swap_adopted(3_000_000, rate_spawned), None);
        assert!(s.after_tick(3_000_000, CEIL).is_empty());
        assert_eq!(s.counts().dims, 1);
        assert_eq!(s.stale_swaps(), 1);
    }

    #[test]
    fn a_replacement_spawned_before_any_plan_is_judged_as_held() {
        let mut s = Shadow::default();
        assert_eq!(s.swap_spawn(), None);
        // No plan at the tick either: nothing to judge against.
        assert_eq!(s.note_rate_swap_adopted(12_000_000, None), None);
        assert!(s.after_tick(12_000_000, CEIL).is_empty());
        // A plan by the tick: judged there, as an inline open is.
        s.produce(
            &PlanInputs {
                target_bps: 3_000_000,
                ..constrained(None)
            },
            &knobs(),
        );
        assert_eq!(s.note_rate_swap_adopted(12_000_000, None), None);
        let first = s.after_tick(3_000_000, CEIL);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].inline, "12000000");
        assert_eq!(s.stale_swaps(), 0);
    }

    #[test]
    fn a_stale_adoption_is_reported_once_and_counted_every_time() {
        let mut s = Shadow::default();
        let spawned = qsv_cascade(&mut s);
        assert!(s.note_rate_swap_adopted(12_000_000, spawned).is_some());
        assert!(s.note_rate_swap_adopted(12_000_000, spawned).is_none());
        assert_eq!(s.stale_swaps(), 2);
        assert!(s.after_tick(2_744_302, CEIL).is_empty());
        assert_eq!(s.counts(), PlanDivergences::default());
    }

    /// M3a.3, the field's D1 (FR-108's display match switching the host's mode
    /// under a live session): the snapshot's native dims are the LAST frame's,
    /// this frame is the first at the new mode. A lag at the check and at the
    /// tick of the open built for it, never a divergence; the next pass's plan
    /// agrees again.
    #[test]
    fn a_native_change_between_the_snapshot_and_the_check_is_a_lag() {
        let mut s = Shadow::default();
        s.produce(&constrained(None), &knobs());
        assert!(s.check_dims((1024, 768), &dims(1024, 768)).is_empty());
        assert_eq!(s.counts(), PlanDivergences::default());
        assert_eq!(s.lags(), 1);
        assert_eq!(
            s.take_lag(),
            Some(PlanLag {
                moved: "native",
                plan_dims: (1920, 1200),
                inline_dims: (1024, 768),
            })
        );
        assert_eq!(s.take_lag(), None, "handed out once");
        // The inline open builds for the new mode in the same pass.
        assert!(s.note_built(seed(3_000_000), (1024, 768)).is_empty());
        assert!(s.after_tick(3_000_000, CEIL).is_empty());
        assert_eq!(s.counts(), PlanDivergences::default());
        assert_eq!(s.lags(), 2, "counted every time");
        assert_eq!(s.take_lag(), None, "reported once a session");
        // The next pass's snapshot carries the new native: no lag, no divergence.
        s.produce(
            &PlanInputs {
                dims: dims(1024, 768),
                ..constrained(None)
            },
            &knobs(),
        );
        assert!(s.check_dims((1024, 768), &dims(1024, 768)).is_empty());
        assert_eq!(s.lags(), 2);
        assert_eq!(s.counts(), PlanDivergences::default());
    }

    /// M3a.3, the field's D2 and D3: an `rc:resolution` applied between the
    /// snapshot and the check, or a viewer joining a shared pipeline (the
    /// merged target moves), names the input that moved.
    #[test]
    fn a_target_write_between_the_snapshot_and_the_check_is_a_lag_named_by_its_input() {
        let mut s = Shadow::default();
        s.produce(&constrained(None), &knobs());
        let inline = DimsInputs {
            merged_target: TargetResolution::Fixed {
                width: 944,
                height: 590,
            },
            ..dims(1920, 1200)
        };
        assert!(s.check_dims((944, 590), &inline).is_empty());
        assert_eq!(s.take_lag().map(|l| l.moved), Some("target"));
        assert_eq!(s.counts().dims, 0);
        // A Priority write in between is named as such.
        let mut s = Shadow::default();
        s.produce(&constrained(None), &knobs());
        let inline = DimsInputs {
            merged_priority_cap: Some(1280),
            ..dims(1920, 1200)
        };
        assert!(s.check_dims((1280, 800), &inline).is_empty());
        assert_eq!(s.take_lag().map(|l| l.moved), Some("priority_cap"));
    }

    /// The lag rule's negative control: an inline open whose inputs HELD and
    /// whose dims are not the plan's is still the divergence it always was.
    #[test]
    fn an_open_built_from_held_inputs_at_other_dims_is_still_a_divergence() {
        let mut s = Shadow::default();
        s.produce(&constrained(None), &knobs());
        assert!(s.check_dims((1920, 1200), &dims(1920, 1200)).is_empty());
        let first = s.note_built(seed(3_000_000), (1280, 800));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].field, "dims");
        assert!(s.after_tick(3_000_000, CEIL).is_empty());
        assert_eq!(s.lags(), 0);
    }

    /// M3a.3, the field's B2: a 6 M replacement adopted while the plan held,
    /// and the controller's first tick after it a congestion decrease to
    /// 1.5 M. Within the ceiling: the plan's own movement one tick on.
    #[test]
    fn the_first_tick_after_an_adoption_moving_within_the_ceiling_is_a_stale_tick() {
        let mut s = Shadow::default();
        s.produce(
            &PlanInputs {
                target_bps: 6_000_000,
                ..direct(CEIL, 100)
            },
            &knobs(),
        );
        let spawned = s.swap_spawn();
        assert_eq!(
            s.note_rate_swap_adopted(6_000_000, spawned),
            None,
            "the plan held while it opened"
        );
        assert!(s.after_tick(1_500_000, CEIL).is_empty());
        assert_eq!(s.counts(), PlanDivergences::default());
        assert_eq!(s.stale_ticks(), 1);
        assert_eq!(
            s.stale_swaps(),
            0,
            "a different count from the open-time stale"
        );
        assert_eq!(
            s.take_stale_tick(),
            Some(StaleSwap {
                kind: "rate-first-tick",
                built_bps: 6_000_000,
                spawned_bps: ladder(6_000_000),
                now_bps: ladder(1_500_000),
                spawned_dims: (1920, 1200),
                now_dims: (1920, 1200),
            })
        );
        assert_eq!(s.take_stale_tick(), None, "handed out once");
    }

    /// The stale-tick rule's two negative controls: an inline open that did
    /// not consult the controller, re-rated on its first tick, is a
    /// divergence (M3a.4 made every rebuild consult it), and a replacement
    /// whose rate the tick did NOT move is judged as before.
    #[test]
    fn an_inline_open_or_an_unmoved_tick_is_never_a_stale_tick() {
        let mut s = Shadow::default();
        s.produce(
            &PlanInputs {
                target_bps: 3_000_000,
                ..direct(CEIL, 100)
            },
            &knobs(),
        );
        // The field's B1: an inline open at the old transport's rate, the
        // controller raising the target under the new ceiling on that tick.
        assert!(s.note_built(seed(3_000_000), (1920, 1200)).is_empty());
        let first = s.after_tick(12_000_000, CEIL);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].field, "bitrate");
        assert_eq!(s.stale_ticks(), 0);
        // A replacement built off the plan, judged on a tick that held.
        let spawned = s.swap_spawn();
        assert_eq!(s.note_rate_swap_adopted(3_000_000, spawned), None);
        let first = s.after_tick(12_000_000, CEIL);
        assert_eq!(s.counts().bitrate, 2);
        assert_eq!(
            first.len(),
            0,
            "the field's second divergence is counted, not logged"
        );
        assert_eq!(s.stale_ticks(), 0);
    }

    /// The knobs the producer takes once are the numbers the per-frame readers
    /// return — whatever the environment says — and, at the built-in defaults,
    /// the shipped constants.
    #[test]
    fn the_knobs_resolve_to_what_the_per_frame_readers_read() {
        // Serialised against the tests that toggle the dial caps.
        let _guard = crate::encode::RELAY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let k = PlanKnobs::from_env();
        assert_eq!(k.relay_max_bps, crate::encode::relay_max_bps());
        assert_eq!(k.constrained_queue_ms, rate_profile::constrained_queue_ms());
        assert_eq!(
            k.constrained_queue_measured,
            crate::encode::constrained_queue_measured_enabled()
        );
        assert_eq!(k.direct_queue_ms, rate_profile::direct_queue_ms());
        assert_eq!(k.direct_hrd_pct, rate_profile::direct_hrd_pct());
        assert_eq!(k.refined_cap, crate::encode::idle_refine_cap_long_edge());
        for dial in [
            crate::encode::priority::BALANCED,
            crate::encode::priority::SHARPER,
            crate::encode::priority::SMOOTHER,
            42,
        ] {
            for constrained in [false, true] {
                assert_eq!(
                    crate::encode::priority_relay_cap_for(dial, constrained, &k.priority_caps),
                    crate::encode::priority_relay_cap(dial, constrained),
                    "dial {dial} constrained {constrained}"
                );
            }
        }
        // The budget the pump pre-resolves at start is the producer's.
        assert_eq!(
            rate_profile::constrained_queue_budget_bytes(k.relay_max_bps),
            rate_profile::constrained_queue_budget_bytes_at(
                k.relay_max_bps,
                k.constrained_queue_ms
            )
        );
        // And the pump's own hrd read is the producer's input.
        assert_eq!(
            rate_profile::hrd_pct_for("hevc_qsv", k.direct_hrd_pct),
            k.direct_hrd_pct
        );
    }

    /// An open with no controller to consult: the session's first.
    fn seed(bps: u32) -> OpenRate {
        OpenRate {
            bps,
            ticked: false,
            consulted: false,
        }
    }

    // FR-70 M3a.4: a rebuild opens at the plan's current size and rate.

    /// A frame as the capture hands it to the pump.
    #[derive(Debug, Clone, Copy)]
    struct Shot {
        dims: (u32, u32),
        native: (u32, u32),
        keepalive: bool,
        /// M3a.5: a drop gate ends the pass after its frame step, before the
        /// controller's tick: the drain (`governor.draining`) or the
        /// decode-pressure skip (`should_skip_delta_frame`).
        gated: bool,
        /// M3a.5: the controller's tick on this pass is a congestion decrease.
        cut: bool,
        /// M3a.5: an `rc:resolution` write that lands on this pass, between
        /// the plan's snapshot and the inline read.
        lands: Option<TargetResolution>,
    }

    impl Shot {
        /// A capture the backend did not scale (GDI, or no cap).
        fn native(native: (u32, u32)) -> Self {
            Shot {
                dims: native,
                native,
                keepalive: false,
                gated: false,
                cut: false,
                lands: None,
            }
        }

        fn gated(self) -> Self {
            Shot {
                gated: true,
                ..self
            }
        }

        fn cut(self) -> Self {
            Shot { cut: true, ..self }
        }

        fn lands(self, target: TargetResolution) -> Self {
            Shot {
                lands: Some(target),
                ..self
            }
        }

        /// A capture by a backend that scales before the readback under the
        /// pump's cap (DXGI-direct's VideoProcessor): exactly the box, unless
        /// the box covers the frame.
        fn scaled(native: (u32, u32), cap: Option<(u32, u32)>) -> Self {
            let dims = match cap {
                Some((w, h)) if !(w >= native.0 && h >= native.1) => (w, h),
                _ => native,
            };
            Shot {
                dims,
                ..Shot::native(native)
            }
        }

        /// The idle keepalive's re-feed of this capture.
        fn again(self) -> Self {
            Shot {
                keepalive: true,
                ..self
            }
        }
    }

    /// Passes a background open takes before the loop top adopts it.
    const OPEN_PASSES: u32 = 3;

    /// The ceiling a geometry plans: a stand-in for `policy::rate_plan`, since
    /// these cells are about which geometry the ceiling is asked for.
    fn ceiling_of((w, h): (u32, u32)) -> u32 {
        (w * h * 8).clamp(3_000_000, 48_000_000)
    }

    /// The controller's tick: an additive step under the ceiling it is handed.
    fn tick(applied: u32, ceiling: u32) -> u32 {
        (applied + 450_000).min(ceiling)
    }

    /// M3a.5: an encoder's first encoded frame, beside the plan of that pass.
    #[derive(Debug, Clone, Copy)]
    struct First {
        pass: usize,
        dims: (u32, u32),
        rung: u32,
        plan_dims: (u32, u32),
        plan_rung: u32,
    }

    /// An inline open, beside the plan it was opened under.
    #[derive(Clone, Copy)]
    struct Opened {
        pass: usize,
        dims: (u32, u32),
        /// The plan's geometry for the frame it opened on.
        plan_dims: (u32, u32),
        /// The rung it opened at, and the plan's rung when its first frame
        /// encodes (after the pass's tick).
        rung: u32,
        plan_rung: u32,
    }

    impl Opened {
        fn on_plan(&self) -> bool {
            self.dims == self.plan_dims && self.rung == self.plan_rung
        }

        fn describe(&self) -> String {
            format!(
                "pass {}: {:?} at {} where the plan held {:?} at {}",
                self.pass, self.dims, self.rung, self.plan_dims, self.plan_rung
            )
        }
    }

    /// The FFmpeg DC pump's frame path a pass at a time, in its order and
    /// through the functions it calls: the loop top adopts a finished
    /// background open, the shadow produces the pass's plan, the capture, M2's
    /// make-before-break ([`dims_move`]) and its pin, the backend cap and the
    /// stale-skip budget it resets, [`frame_step`], an inline open at
    /// [`open_rate`], the controller's tick, the shadow's judgment.
    struct Rig {
        shadow: Shadow,
        /// The control plane's long-edge cap (the merged Priority cap).
        cap: Option<u32>,
        /// The native dims the last frame carried (`capture_native_dims`).
        last_native: (u32, u32),
        encoder: Option<(u32, u32)>,
        built: Option<TargetResolution>,
        pending: Option<Opening>,
        backend_cap: Option<(u32, u32)>,
        budget: StaleBudget,
        applied: u32,
        pass: usize,
        opened: Vec<Opened>,
        /// M3a.5: the merged `rc:resolution` target.
        target: TargetResolution,
        /// M3a.5: the controller steps up each tick (`tick`), or holds.
        stepping: bool,
        /// M3a.5: the backend re-rates in place (NVENC) at a rung the tick
        /// crosses; rebuild-bound (QSV, AMF) keeps the rate for a rate swap.
        inplace: bool,
        /// M3a.5: the live encoder's rate, and whether it has encoded yet.
        rate: u32,
        fresh: bool,
        first: Vec<First>,
        /// M3a.5: the shadow's counts after each pass, as a heartbeat reads
        /// them, and the passes whose geometry inputs moved.
        trail: Vec<PlanDivergences>,
        moved: Vec<usize>,
        lagged: Vec<usize>,
    }

    /// M2's background open, as `PendingDimsOpen` carries it.
    #[derive(Clone, Copy)]
    struct Opening {
        dims: (u32, u32),
        target: TargetResolution,
        /// The plan it was spawned under.
        spawned: Option<SwapSpawn>,
        rate: u32,
        /// Passes left until it has opened.
        left: u32,
    }

    impl Rig {
        /// A session mid-flight: an encoder at `encoder`, built for `built`,
        /// the backend capped to it, and the controller at `applied`.
        fn live(
            cap: Option<u32>,
            native: (u32, u32),
            encoder: (u32, u32),
            built: TargetResolution,
            applied: u32,
        ) -> Self {
            Rig {
                shadow: Shadow::default(),
                cap,
                last_native: native,
                encoder: Some(encoder),
                built: Some(built),
                pending: None,
                backend_cap: match built {
                    TargetResolution::Native => None,
                    TargetResolution::Fixed { width, height } => Some((width, height)),
                },
                budget: StaleBudget::SPENT,
                applied,
                pass: 0,
                opened: Vec::new(),
                target: TargetResolution::Native,
                stepping: true,
                inplace: true,
                rate: applied,
                fresh: false,
                first: Vec::new(),
                trail: Vec::new(),
                moved: Vec::new(),
                lagged: Vec::new(),
            }
        }

        fn inputs(&self, native: (u32, u32)) -> DimsInputs {
            DimsInputs {
                merged_priority_cap: self.cap,
                merged_target: self.target,
                ..dims(native.0, native.1)
            }
        }

        /// The controller's answer on this pass, under `ceiling`.
        fn answer(&self, ceiling: u32, cut: bool) -> u32 {
            if cut {
                self.applied / 100 * 85
            } else if self.stepping {
                tick(self.applied, ceiling)
            } else {
                self.applied.min(ceiling)
            }
        }

        /// The end of a pass: what its heartbeat would read.
        fn end(&mut self, step: FrameStep, lags_before: u32) -> FrameStep {
            if self.shadow.lags() > lags_before {
                self.lagged.push(self.pass);
            }
            self.trail.push(self.shadow.counts());
            step
        }

        fn step(&mut self, shot: Shot) -> FrameStep {
            self.pass += 1;
            let lags_before = self.shadow.lags();
            if let Some(o) = self.pending.as_mut() {
                o.left = o.left.saturating_sub(1);
                if o.left == 0 {
                    let o = *o;
                    self.pending = None;
                    self.encoder = Some(o.dims);
                    self.built = Some(o.target);
                    self.rate = o.rate;
                    self.fresh = true;
                    let _ = self
                        .shadow
                        .note_dims_swap_adopted(o.rate, o.dims, o.spawned);
                }
            }
            let _ = self.shadow.produce(
                &PlanInputs {
                    target_bps: self.applied,
                    dims: self.inputs(self.last_native),
                    ..direct(CEIL, 100)
                },
                &knobs(),
            );
            if shot.native != self.last_native || shot.lands.is_some() {
                self.moved.push(self.pass);
            }
            if let Some(target) = shot.lands {
                self.target = target;
            }
            self.last_native = shot.native;
            let inline = self.inputs(shot.native);
            let plan_target = policy::plan_dims(&inline).effective_target;
            let plan_dims = resample::target_dims(shot.native, plan_target);
            let _ = self.shadow.check_dims(plan_dims, &inline);
            if let (Some(built), None, Some(encoder)) = (self.built, self.pending, self.encoder)
                && let Some((dims, target)) = dims_move(shot.native, plan_target, built, encoder)
            {
                self.pending = Some(Opening {
                    dims,
                    target,
                    spawned: self.shadow.swap_spawn(),
                    rate: policy::replacement_open_bps(ceiling_of(dims), self.applied, None),
                    left: OPEN_PASSES,
                });
            }
            let pinned = self.pending.and(self.built);
            let cap = match pinned.unwrap_or(plan_target) {
                TargetResolution::Native => None,
                TargetResolution::Fixed { width, height } => Some((width, height)),
            };
            if cap != self.backend_cap {
                self.backend_cap = cap;
                self.budget = StaleBudget::FULL;
            }
            let facts = FrameFacts {
                dims: shot.dims,
                native: shot.native,
                scalable: true,
                keepalive: shot.keepalive,
                plan_target,
                pinned,
                encoder: self.encoder,
            };
            let step = frame_step(&facts, &mut self.budget);
            let Some(target) = step.target() else {
                return self.end(step, lags_before);
            };
            let encoded = resample::target_dims(shot.dims, target);
            let ceiling = ceiling_of(encoded);
            let mut ticked = false;
            if step == FrameStep::Open(target) {
                let answer = self.answer(ceiling, false);
                let open = open_rate(self.applied, None, ceiling, false, || {
                    self.applied = answer;
                    answer
                });
                ticked = open.ticked;
                self.encoder = Some(encoded);
                self.built = Some(target);
                self.pending = None;
                self.rate = open.bps;
                self.fresh = true;
                let _ = self.shadow.note_built(open, encoded);
                self.opened.push(Opened {
                    pass: self.pass,
                    dims: encoded,
                    plan_dims,
                    rung: ladder(open.bps),
                    plan_rung: 0,
                });
            }
            if shot.gated {
                return self.end(step, lags_before);
            }
            if !ticked {
                let next = self.answer(ceiling, shot.cut);
                if self.inplace && ladder(next) != ladder(self.rate) {
                    self.rate = next;
                }
                self.applied = next;
            }
            let _ = self.shadow.after_tick(self.applied, ceiling);
            if let (FrameStep::Open(_), Some(o)) = (step, self.opened.last_mut()) {
                o.plan_rung = ladder(self.applied);
            }
            // The live encoder takes the frame.
            if std::mem::take(&mut self.fresh)
                && let Some(dims) = self.encoder
            {
                self.first.push(First {
                    pass: self.pass,
                    dims,
                    rung: ladder(self.rate),
                    plan_dims,
                    plan_rung: ladder(self.applied),
                });
            }
            self.end(step, lags_before)
        }

        /// M3a.5: the rule this phase makes true, read from a run. Every
        /// encoder's size and rung are the plan's when its first frame
        /// encodes; every field reads zero at every pass; and a lag lasts
        /// only the pass whose input moved.
        fn verdict(&self) -> Vec<String> {
            let mut off: Vec<String> = self
                .first
                .iter()
                .filter(|f| f.dims != f.plan_dims || f.rung != f.plan_rung)
                .map(|f| {
                    format!(
                        "pass {}: first frame at {:?} on {} where the plan held {:?} on {}",
                        f.pass, f.dims, f.rung, f.plan_dims, f.plan_rung
                    )
                })
                .collect();
            if let Some((i, c)) = self
                .trail
                .iter()
                .enumerate()
                .find(|(_, c)| **c != PlanDivergences::default())
            {
                let apart = self.trail[i..]
                    .iter()
                    .filter(|c| **c != PlanDivergences::default());
                off.push(format!(
                    "from pass {} on, {} of {} passes read {c}",
                    i + 1,
                    apart.count(),
                    self.trail.len()
                ));
            }
            off.extend(
                self.lagged
                    .iter()
                    .filter(|p| !self.moved.contains(p))
                    .map(|p| format!("pass {p}: a lag with no input moved on it")),
            );
            off
        }

        /// The inline opens built off the plan's size or rate, described.
        fn off_plan(&self) -> Vec<String> {
            self.opened
                .iter()
                .filter(|o| !o.on_plan())
                .map(Opened::describe)
                .collect()
        }
    }

    /// (a) A full rebuild racing an M2 dims swap: in flight, and just adopted.
    #[test]
    fn a_rebuild_racing_a_dims_swap_opens_at_the_plan_s_size_and_rate() {
        // In flight: the plan caps a 2560x1600 desktop at 1280 (a Priority cap),
        // the replacement opens behind a `Native` pin, and the display mode
        // switches to 1920x1200 before it lands. The plan for the new mode is
        // 1280x800; the live encoder cannot take the frame.
        let (n0, n1) = ((2560, 1600), (1920, 1200));
        let mut flight = Rig::live(None, n0, n0, TargetResolution::Native, 9_100_000);
        flight.cap = Some(1280);
        flight.step(Shot::native(n0));
        let spawned = flight.pending.is_some();
        for _ in 0..8 {
            flight.step(Shot::native(n1));
        }
        // Just adopted: an idle screen capped at 1920x1200 by the capture
        // backend; the cap lifts, the swap to native is adopted, and only the
        // keepalive's re-feed of the last capture arrives until the backend
        // delivers a native frame.
        let n = (2560, 1600);
        let capped = Shot::scaled(n, Some((1920, 1200)));
        let box_target = TargetResolution::Fixed {
            width: 1920,
            height: 1200,
        };
        let mut adopted = Rig::live(Some(1920), n, (1920, 1200), box_target, 9_100_000);
        adopted.cap = None;
        for _ in 0..12 {
            adopted.step(capped.again());
        }
        adopted.step(Shot::native(n));

        let off: Vec<_> = flight
            .off_plan()
            .into_iter()
            .chain(adopted.off_plan())
            .collect();
        let opens = flight.opened.len() + adopted.opened.len();
        assert!(
            off.is_empty(),
            "{} of {opens} rebuilds opened off the plan: {off:?}",
            off.len()
        );
        // And the races ran: the cap opened a replacement, the mode switch
        // under its pin took an inline open, and the swap to native was
        // adopted and serves the native frame with nothing reopened.
        assert!(spawned);
        assert_eq!(flight.opened.len(), 1);
        assert_eq!(flight.encoder, Some((1280, 800)));
        assert_eq!(adopted.encoder, Some(n));
        assert!(adopted.opened.is_empty());
    }

    /// (b) A rebuild racing a native-geometry flap: the capture flips between
    /// one display (DXGI, scaling under the cap) and the virtual screen (GDI,
    /// never scaled) every 5 s, under a cap that binds only the virtual screen.
    #[test]
    fn a_rebuild_through_a_native_geometry_flap_opens_at_the_plan_s_size_and_rate() {
        let (display, virtual_screen) = ((1920, 1080), (3840, 1080));
        let mut rig = Rig::live(
            Some(2560),
            display,
            display,
            TargetResolution::Native,
            9_100_000,
        );
        // 10 passes a second, five 5-second phases.
        for phase in 0..5 {
            for _ in 0..50 {
                let shot = if phase % 2 == 0 {
                    Shot::scaled(display, rig.backend_cap)
                } else {
                    Shot::native(virtual_screen)
                };
                rig.step(shot);
            }
        }
        let off = rig.off_plan();
        assert!(
            off.is_empty(),
            "{} of {} rebuilds opened off the plan: {off:?}",
            off.len(),
            rig.opened.len()
        );
        // The flap forced a rebuild each time the virtual screen came back.
        assert_eq!(rig.opened.len(), 2);
    }

    /// (c) A rate move during an encoder open: the controller's next target
    /// crosses a rung from the one it held when the rebuild began.
    #[test]
    fn an_inline_open_is_built_at_the_rate_the_plan_holds_when_its_first_frame_encodes() {
        // (the target before the pass, the controller's next): up over a rung,
        // down under one, up from the 8 M rung, and one that holds its rung.
        let moves = [
            (22_300_000, 22_750_000),
            (20_000_000, 17_000_000),
            (9_000_000, 10_200_000),
            (15_000_000, 15_400_000),
        ];
        let mut s = Shadow::default();
        let mut off = Vec::new();
        for (before, next) in moves {
            let _ = s.produce(
                &PlanInputs {
                    target_bps: before,
                    ..direct(CEIL, 100)
                },
                &knobs(),
            );
            let applied = std::cell::Cell::new(before);
            let open = open_rate(before, None, CEIL, false, || {
                applied.set(next);
                next
            });
            if !open.ticked {
                applied.set(next);
            }
            let _ = s.note_built(open, (1920, 1200));
            let _ = s.after_tick(applied.get(), CEIL);
            let (rung, plan_rung) = (ladder(open.bps), ladder(applied.get()));
            if rung != plan_rung {
                off.push((before, next, rung, plan_rung));
            }
        }
        assert!(
            off.is_empty(),
            "{} of {} rebuilds opened off the plan's rate (before, next, opened rung, plan rung): {off:?}",
            off.len(),
            moves.len()
        );
        assert_eq!(s.counts().bitrate, 0);
    }

    /// (d) A capture that never delivers the plan's geometry still gets an
    /// encoder: a swap up to a larger box is adopted on a still screen, and the
    /// backend, which scaled the last capture under the old box, captures
    /// nothing new (DXGI-direct redelivers only on a cap lifted to none). The
    /// keepalives wait about 1.5 s for the plan's geometry, then one opens at
    /// its own size, as before M3a.4, and the encoder serves the rest: the
    /// viewer's keyframe probe (after 6 s of no frames) is answered.
    #[test]
    fn a_capture_that_never_reaches_the_plan_s_geometry_still_gets_an_encoder() {
        let n = (2560, 1600);
        let old_box = TargetResolution::Fixed {
            width: 1280,
            height: 800,
        };
        let mut rig = Rig::live(Some(1280), n, (1280, 800), old_box, 9_100_000);
        rig.cap = Some(1920);
        let stale = Shot::scaled(n, Some((1280, 800))).again();
        let mut dropped = 0;
        for _ in 0..80 {
            if rig.step(stale) == FrameStep::Skip {
                dropped += 1;
            }
        }
        // The swap opened and was adopted while the old encoder served.
        assert_eq!(
            rig.built,
            Some(TargetResolution::Fixed {
                width: 1920,
                height: 1200
            })
        );
        // One open, at the capture's own size, after a bounded wait.
        let opens: Vec<_> = rig.opened.iter().map(|o| (o.pass, o.dims)).collect();
        assert_eq!(opens.len(), 1, "opens (pass, dims): {opens:?}");
        assert_eq!(opens[0].1, (1280, 800));
        // 25 keepalives and then the 3 every cap change allows, 60 ms apart.
        assert_eq!(dropped, 28);
        assert_eq!(rig.encoder, Some((1280, 800)));
    }

    // FR-70 M3a.5: once the plan and an encoder agree again, every field reads
    // zero again.

    /// (e) A native geometry move through the capture fallback, and back, on a
    /// backend that re-rates in place. DXGI-direct's output grows from
    /// 1920x1080 to 3440x1440; AccessLost repeats and capture falls back to
    /// GDI's virtual screen (6000x1440); the inline open for it lands inside a
    /// capture stall, so its pass drops the frame (the decode-pressure skip),
    /// and the controller's next tick is a congestion decrease; capture then
    /// returns to DXGI at 1920x1080 and the encoder reopens there.
    #[test]
    fn a_native_move_through_the_capture_fallback_and_back_reads_zero_at_every_pass() {
        let (display, wide, virtual_screen) = ((1920, 1080), (3440, 1440), (6000, 1440));
        let mut rig = Rig::live(None, display, display, TargetResolution::Native, 1_842_375);
        rig.stepping = false;
        for _ in 0..30 {
            rig.step(Shot::native(display));
        }
        for _ in 0..60 {
            rig.step(Shot::native(wide));
        }
        rig.step(Shot::native(virtual_screen).gated());
        rig.step(Shot::native(virtual_screen).cut());
        for _ in 0..120 {
            rig.step(Shot::native(virtual_screen));
        }
        for _ in 0..60 {
            rig.step(Shot::native(display));
        }
        let off = rig.verdict();
        assert!(
            off.is_empty(),
            "{} findings over {} passes (lags {}, stale ticks {}): {off:?}",
            off.len(),
            rig.pass,
            rig.shadow.lags(),
            rig.shadow.stale_ticks()
        );
        // The shape ran: three opens, the second inside the stall, and the
        // decrease after it counted apart, as the controller's own move.
        let opens: Vec<_> = rig.opened.iter().map(|o| o.dims).collect();
        assert_eq!(opens, [wide, virtual_screen, display]);
        assert_eq!(rig.first.len(), 3);
        assert_eq!(rig.shadow.stale_ticks(), 1);
        assert_eq!(rig.lagged, [31, 91, 213]);
        assert_eq!(rig.encoder, Some(display));
    }

    /// (f) A viewer resizing in fit mode on an idle screen, on a rebuild-bound
    /// backend: five `rc:resolution` moves in about 2.2 s (a pass is one 60 ms
    /// keepalive), each starting an M2 swap that opens in three passes. The
    /// capture scales under the capture cap and captures again only now and
    /// then, so after an adoption the keepalives re-feed a capture scaled under
    /// the old cap; the third move lands between the plan's snapshot and the
    /// inline read, and the swap it starts is adopted after the snapshot moved.
    #[test]
    fn a_viewer_resizing_in_fit_mode_reads_zero_at_every_pass() {
        let native = (1920, 1080);
        let fixed = |width, height| TargetResolution::Fixed { width, height };
        let mut rig = Rig::live(None, native, (1294, 728), fixed(1294, 728), 9_000_000);
        rig.target = fixed(1550, 728);
        rig.stepping = false;
        rig.inplace = false;
        // pass -> (the box written before it, or landing on it; a capture).
        let moves = [
            (6, fixed(1510, 708), false),
            (23, fixed(1550, 728), false),
            (29, fixed(1634, 768), true),
            (35, fixed(1716, 810), false),
            (43, fixed(1758, 830), false),
        ];
        let captures = [1, 12, 33, 39, 47];
        let mut last = Shot::scaled(native, rig.backend_cap);
        for pass in 1..=150 {
            if captures.contains(&pass) {
                last = Shot::scaled(native, rig.backend_cap);
            }
            let mut shot = if captures.contains(&pass) {
                last
            } else {
                last.again()
            };
            if let Some(&(_, target, lands)) = moves.iter().find(|m| m.0 == pass) {
                if lands {
                    shot = shot.lands(target);
                } else {
                    rig.target = target;
                }
            }
            rig.step(shot);
        }
        let off = rig.verdict();
        assert!(
            off.is_empty(),
            "{} findings over {} passes (lags {}, stale swaps {}): {off:?}",
            off.len(),
            rig.pass,
            rig.shadow.lags(),
            rig.shadow.stale_swaps()
        );
        // The shape ran: the five swaps adopted, the one the landing started
        // at the size the plan held at its adoption, so it is not stale.
        let firsts: Vec<_> = rig.first.iter().map(|f| f.dims).collect();
        assert_eq!(
            firsts,
            [(1258, 708), (1364, 768), (1440, 810), (1474, 830)],
            "the swap to 1294x728 never encoded: its keepalives could not reach it"
        );
        assert_eq!(rig.lagged, [29]);
        assert_eq!(rig.shadow.stale_swaps(), 0);
        assert_eq!(rig.encoder, Some((1474, 830)));
        assert!(rig.opened.is_empty());
    }
}

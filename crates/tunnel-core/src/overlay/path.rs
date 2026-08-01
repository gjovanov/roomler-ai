//! P3 — PathMonitor: measured path selection (consolidation invariant I3).
//!
//! One probe-and-score model for *which carrier tier to run a peer on*. It
//! REPLACED the reactive cooldown/deny/strike machinery accreted across
//! rc.136→rc.225 (`DirectCooldowns`, `direct_retry_cooldown`, the sticky
//! denies) through a staged rollout — PR-A shadow compare, PR-B instrument
//! fidelity, PR-C consumed decisions, PR-D default-on, and PR-E deleted the
//! legacy machinery outright after three green soaks (48 h shadow parity,
//! 48 h fleet shadow + ON pilot, ~42 h fleet-wide ON). Since PR-E this
//! module IS path selection; the `PathShadow` harness in `runtime.rs` is
//! telemetry.
//!
//! ## The two-plane design (adversarial-critique findings F1+F2)
//!
//! The original single-score draft (`S = B + Q − P` deciding everything) was
//! BROKEN: quality drift stretched the cooldown-parity windows to `[0, ∞)` —
//! a healthy incumbent's heard-credit pumped its Q while a non-incumbent tier
//! had no Q-recovery event, so one failed probe could lock a tier out
//! permanently (and in the +Q direction, escalated denies collapsed toward
//! zero, reintroducing the relay↔direct flap the deny exists to stop). The
//! fix separates ELIGIBILITY from RANKING:
//!
//! * **Eligibility plane (parity-exact, lockout-impossible).** A direct tier
//!   is probe-eligible iff `B_tier − P_tier ≥ B_RELAY` — priors + decaying
//!   penalty ONLY, no Q term. A failure books `P = W` with
//!   `W = 2 × (B_tier − B_RELAY)`, decaying exponentially with half-life `H`;
//!   the tier re-crosses the threshold (`P ≤ W/2`) after EXACTLY `H`. With
//!   `H` chosen per failure from the legacy escalation table (60 s ordinary ≡
//!   `DIRECT_COOLDOWN`; 900 s at the same strike thresholds ≡ the 15 min
//!   denies) the suppression windows reproduce today's `until > now` gates
//!   bit-for-bit — and because P always decays to zero, every tier retries
//!   eventually: **lockout is impossible by construction** (the legacy
//!   guarantee the single-score draft lost).
//! * **Ranking plane (Q, advisory).** `S = B + Q − P` ranks among
//!   ALREADY-eligible tiers — which to probe first when several are eligible.
//!   Q is an EWMA clamped to ±100, fed by real observations (probe latch,
//!   heard-this-sweep, one-way strikes, deaths, authenticated inbound inits).
//!   Q can never make an ineligible tier eligible nor an eligible tier
//!   ineligible — locked by `no_q_state_can_delay_or_advance_eligibility`.
//!
//! The field-load-bearing **fixed 60 s LAN retry spacing** (P8: bilateral
//! punch alignment after the 2026-07-25 hibernate incident depends on BOTH
//! ends retrying on the same fixed cadence) is therefore penalty-only — and
//! additionally pinned scoreless in the scheduler: a LAN probe is never
//! proposed within [`LAN_PROBE_SPACING`] of the previous LAN attempt, no
//! matter what any score says (F2, locked by
//! `lan_spacing_pinned_under_q_extremes`).
//!
//! ## What the monitor does NOT decide
//!
//! * **Carrier death** — that's the P2 lifecycle ([`super::lifecycle`]). The
//!   monitor CONSUMES deaths as evidence ([`PathMonitor::on_death`]).
//! * **The relay sub-tier** (anchor / dialer / both-allocate / DERP) — stays
//!   capability-symmetric in `RelayCoordinator`. In particular the server's
//!   forced-DERP push STAYS AN OVERRIDE: DERP is receivable only if the far
//!   end opened its mux, which UDP-capable nodes do only on the server push,
//!   so a unilateral score-driven flip would be a permanent split. The
//!   monitor consumes the push as an event ([`PathMonitor::on_forced_derp`]:
//!   annotate the pinned window, reset relay-Q for a fresh evidence window)
//!   and its direct-tier decisions are UNAFFECTED by an active pin (locked by
//!   `forced_derp_pin_invariants`).
//! * **Score-driven voluntary switches** — disabled at the flip (upgrades
//!   require a latched probe exactly as today; demotes only on a
//!   [`DeathReason`]). The [`Hysteresis`] rate-limit (≥30 s between voluntary
//!   switches) ships now as the guard the post-flip demotion work must go
//!   through, wired to nothing in shadow mode.
//!
//! ## Purity
//!
//! No sockets, no async, no clock reads — every method takes `now`. The
//! numeric constants deliberately mirror the legacy ones still living in
//! `runtime.rs` (`DIRECT_COOLDOWN` et al.); a cross-check test there pins the
//! two sets equal until PR-E deletes the legacy side.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bson::oid::ObjectId;

use super::lifecycle::{DeathReason, DirectTier};

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

/// `ROOMLER_NODE_OVERLAY_PATHMON` — how far the monitor is engaged.
/// Parsed by [`PathMonMode::parse`]; the env READ lives with the caller
/// (`runtime.rs`) so this module stays env-free.
/// PR-E — the legacy selection machinery is DELETED: the monitor is the one
/// and only path selector, unconditionally. This enum now gates TELEMETRY
/// only (the 10-min summaries + divergence-style execution logs); it is kept
/// so `ROOMLER_NODE_OVERLAY_PATHMON=shadow|off` set on a pre-PR-E host keeps
/// parsing — the runtime warns once at startup that those values no longer
/// change selection (rollback to legacy = deploy a pre-PR-E rc).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PathMonMode {
    /// Telemetry off (summaries and comparison logs suppressed). Selection
    /// is still the monitor — there is nothing else.
    Off,
    /// Vestigial (pre-PR-E: legacy authoritative). Telemetry on; selection
    /// is the monitor.
    Shadow,
    /// The default: telemetry on, monitor authoritative.
    On,
}

impl PathMonMode {
    /// PR-D — `off|0|false` → Off, `shadow` → Shadow (the per-host revert
    /// rail), anything else (incl. unset, `on`, `1`, `true`) → **On**: the
    /// monitor is authoritative by default after two green 48 h soaks
    /// (soak #1 shadow-parity on rc.246; soak #2 fleet shadow + neo16
    /// ON-pilot on rc.271/272 — harmful 0, steady 0–0.05 % throughout,
    /// incl. a fleet-wide server-churn incident absorbed mid-soak).
    pub(crate) fn parse(v: Option<&str>) -> Self {
        match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("off") | Some("0") | Some("false") => PathMonMode::Off,
            Some("shadow") => PathMonMode::Shadow,
            _ => PathMonMode::On,
        }
    }

    /// Lowercase name for logs (the 10-min summary prints it — pre-PR-D the
    /// summary said `[shadow]` unconditionally, which lied on the ON pilot).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PathMonMode::Off => "off",
            PathMonMode::Shadow => "shadow",
            PathMonMode::On => "on",
        }
    }
}

// ---------------------------------------------------------------------------
// Constants — the model's numbers
// ---------------------------------------------------------------------------

/// Base priors. Cold start (Q = 0, P = 0) makes `S = B`, which reproduces the
/// legacy tier precedence LAN > public-direct > srflx > relay bit-for-bit
/// (locked by `cold_start_ordering_matches_legacy_precedence`).
pub(crate) const B_LAN: f64 = 400.0;
pub(crate) const B_PUBLIC: f64 = 330.0;
pub(crate) const B_SRFLX: f64 = 260.0;
pub(crate) const B_RELAY: f64 = 200.0;

/// Ordinary suppression half-life ≡ the legacy `DIRECT_COOLDOWN` (60 s).
pub(crate) const H_ORDINARY: Duration = Duration::from_secs(60);
/// Escalated suppression half-life ≡ the legacy `DIRECT_DENY_COOLDOWN` /
/// `SRFLX_DENY_COOLDOWN` (both 15 min since rc.225).
pub(crate) const H_ESCALATED: Duration = Duration::from_secs(15 * 60);
/// Strike thresholds ≡ legacy `DIRECT_MAX_FAILURES` / `SRFLX_MAX_FAILURES`.
/// The LAN tier under make-before-break never escalates (P8 — fixed 60 s
/// spacing on both ends guarantees bilateral punch alignment; see
/// `direct_retry_cooldown`).
pub(crate) const MAX_FAILURES_LAN_PUBLIC: u32 = 2;
pub(crate) const MAX_FAILURES_SRFLX: u32 = 3;

/// F2 — the scoreless LAN probe-spacing pin: never propose a LAN attempt
/// within this of the previous one, independent of every score and penalty.
pub(crate) const LAN_PROBE_SPACING: Duration = Duration::from_secs(60);
/// Global in-flight probe cap (structural bound; the per-peer bound is one —
/// `WgDevice::probes` is keyed by pubkey and the monitor mirrors it).
pub(crate) const PROBE_CAP: usize = 16;
/// Voluntary (score-driven) switches must be at least this far apart per
/// peer. Disabled-at-flip machinery — see [`Hysteresis`]; prod code reaches
/// it only when the post-flip demotion work lands (the allow tracks that).
#[allow(dead_code)]
pub(crate) const SWITCH_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// Q clamp: quality lives in [−Q_CLAMP, +Q_CLAMP].
const Q_CLAMP: f64 = 100.0;
/// Probe latched: sample `+100 − min(50, latency_ms/40)`, α 0.4. Latency
/// `None` (PR-A — the latch is 5 s-quantized until PR-B's `handshake_at`
/// timestamp) deducts nothing rather than inventing a number.
const Q_LATCH_ALPHA: f64 = 0.4;
const Q_LATCH_LATENCY_DIVISOR: f64 = 40.0;
const Q_LATCH_MAX_DEDUCTION: f64 = 50.0;
/// Heard-this-sweep (keepalive-inclusive rx advanced): slow +100 drift.
const Q_HEARD_SAMPLE: f64 = 100.0;
const Q_HEARD_ALPHA: f64 = 0.05;
/// A one-way strike stored this sweep: fast negative drift.
const Q_BAD_SWEEP_SAMPLE: f64 = -60.0;
const Q_BAD_SWEEP_ALPHA: f64 = 0.2;
/// Authenticated inbound WG init on a tier: strong positive evidence (the
/// path traversed BOTH NATs a moment ago).
const Q_INBOUND_SAMPLE: f64 = 80.0;
const Q_INBOUND_ALPHA: f64 = 0.4;
/// A death slams Q to the floor (not an EWMA step — the carrier is gone).
const Q_DEATH_SLAM: f64 = -100.0;

/// Eligibility float guard: `powf` at exactly one half-life is not guaranteed
/// bit-exact, and legacy `until > now` counts the boundary instant as
/// not-cooling — the epsilon keeps the boundary on the eligible side.
const ELIGIBILITY_EPS: f64 = 1e-9;

fn base(tier: DirectTier) -> f64 {
    match tier {
        DirectTier::Lan => B_LAN,
        DirectTier::Public => B_PUBLIC,
        DirectTier::Srflx => B_SRFLX,
        DirectTier::Relay => B_RELAY,
    }
}

/// Penalty weight `W = 2 × (B_tier − B_RELAY)`: eligibility (`B − P ≥
/// B_RELAY` ⇔ `P ≤ W/2`) is re-crossed after exactly one half-life.
fn weight(tier: DirectTier) -> f64 {
    2.0 * (base(tier) - B_RELAY)
}

/// The suppression half-life for a failure — the legacy
/// `direct_retry_cooldown` escalation table, verbatim: srflx sticks at 3
/// strikes, LAN/public at 2, EXCEPT the LAN tier under make-before-break,
/// which never escalates (a LAN retry is a zero-disruption shadow probe and
/// the fixed spacing is what keeps bilateral punches aligned — P8).
fn suppression_half_life(tier: DirectTier, fails: u32, mbb: bool) -> Duration {
    let max = match tier {
        DirectTier::Srflx => MAX_FAILURES_SRFLX,
        DirectTier::Lan if mbb => return H_ORDINARY,
        _ => MAX_FAILURES_LAN_PUBLIC,
    };
    if fails >= max {
        H_ESCALATED
    } else {
        H_ORDINARY
    }
}

// ---------------------------------------------------------------------------
// Building blocks
// ---------------------------------------------------------------------------

/// Clamped EWMA — the ranking plane's accumulator.
#[derive(Clone, Copy, Default, Debug)]
struct Ewma(f64);

impl Ewma {
    fn feed(&mut self, sample: f64, alpha: f64) {
        self.0 = (self.0 + alpha * (sample - self.0)).clamp(-Q_CLAMP, Q_CLAMP);
    }
    fn slam(&mut self, v: f64) {
        self.0 = v.clamp(-Q_CLAMP, Q_CLAMP);
    }
    fn get(self) -> f64 {
        self.0
    }
}

/// One booked suppression: full weight at `at`, halving every `half_life`.
#[derive(Clone, Copy, Debug)]
struct Penalty {
    w: f64,
    half_life: Duration,
    at: Instant,
}

impl Penalty {
    fn value(&self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.w * 0.5_f64.powf(elapsed / self.half_life.as_secs_f64())
    }
}

/// Per-(peer, direct-tier) state: quality, active suppression, strike count.
#[derive(Default)]
struct TierState {
    q: Ewma,
    penalty: Option<Penalty>,
    fails: u32,
}

/// Per-peer monitor state.
#[derive(Default)]
struct PeerPath {
    /// Indexed [Lan, Public, Srflx] — see [`tier_idx`].
    tiers: [TierState; 3],
    /// Relay-tier quality (telemetry + forced-DERP evidence window; the relay
    /// tier's ELIGIBILITY is structural — it is the floor).
    relay_q: Ewma,
    /// In-flight upgrade probe (mirror of `upgrade_probes` / the WgDevice
    /// probe slot — one per peer, structural).
    probe: Option<(DirectTier, Instant)>,
    /// F2 — when the last LAN attempt (probe start or install) began.
    last_lan_attempt: Option<Instant>,
    /// P7 — the server's forced-DERP pin window, as an annotation. Direct
    /// decisions ignore it entirely (see the module doc).
    forced_derp_until: Option<Instant>,
    /// Voluntary-switch hysteresis bookkeeping (disabled-at-flip machinery).
    hysteresis: Hysteresis,
}

fn tier_idx(tier: DirectTier) -> Option<usize> {
    match tier {
        DirectTier::Lan => Some(0),
        DirectTier::Public => Some(1),
        DirectTier::Srflx => Some(2),
        DirectTier::Relay => None,
    }
}

/// Direct tiers in legacy precedence order — the ranking tie-break, so a
/// cold-start ranking (all S equal to B) IS the legacy order.
const DIRECT_TIERS: [DirectTier; 3] = [DirectTier::Lan, DirectTier::Public, DirectTier::Srflx];

// ---------------------------------------------------------------------------
// Decision inputs / outputs
// ---------------------------------------------------------------------------

/// Which direct tiers currently HAVE a dialable candidate for this peer —
/// the runtime's endpoint/flag/hairpin checks, WITHOUT the cooldown gates
/// (cooling is the monitor's own penalty plane). Availability is eligibility
/// input, never score: a tier with no endpoint simply isn't on the ballot.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct TierAvailability {
    pub(crate) lan: bool,
    pub(crate) public: bool,
    pub(crate) srflx: bool,
}

impl TierAvailability {
    fn has(&self, tier: DirectTier) -> bool {
        match tier {
            DirectTier::Lan => self.lan,
            DirectTier::Public => self.public,
            DirectTier::Srflx => self.srflx,
            DirectTier::Relay => true,
        }
    }
}

/// What the peer currently runs on, from the runtime's `by_node` view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Incumbent {
    /// No installed carrier (fresh peer / just torn down).
    None,
    /// Installed direct carrier on this tier.
    Direct(DirectTier),
    /// Installed relay carrier.
    Relay,
}

/// A path decision — the monitor's, or the legacy machinery's recorded one
/// (`LegacyChoice` instrumentation). Compared by [`classify`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PathAction {
    /// No change (incumbent kept / nothing worth doing).
    Keep,
    /// Install this direct tier now (fresh peer, or the destructive
    /// break-before-make upgrade when MBB is env-off, or an inbound
    /// re-point).
    Install(DirectTier),
    /// Start a make-before-break shadow probe toward this tier.
    Probe(DirectTier),
    /// Coordinate / fall back to the relay tier.
    Relay,
}

/// The result of a probe, as fed back by the runtime's promote/expire sweep.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum ProbeOutcome {
    /// Handshake latched — the tier is proven bidirectional. `latency_ms` is
    /// the observed latch latency (PR-B; `None` until then — no deduction).
    Latched { latency_ms: Option<f64> },
    /// Deadline blown. Books the tier failure. NO Q event — the penalty
    /// already encodes the failure; double-booking it into Q was part of the
    /// original design's lockout math (F1).
    Expired,
}

// ---------------------------------------------------------------------------
// Hysteresis (disabled-at-flip voluntary-switch guard)
// ---------------------------------------------------------------------------

/// Rate-limit for SCORE-DRIVEN voluntary switches (≥ [`SWITCH_MIN_INTERVAL`]
/// apart per peer). Deliberately consulted by NOTHING in shadow mode and by
/// nothing legacy-parity at the flip (upgrades stay probe-gated, demotes stay
/// death-gated — both exempt): this exists so the post-flip "demote a
/// degraded-but-live carrier" work has its flap-guard ready-made, with the
/// bound locked by `voluntary_switch_hysteresis_bound` from day one.
#[derive(Default)]
pub(crate) struct Hysteresis {
    last_switch: Option<Instant>,
}

impl Hysteresis {
    /// Record ANY switch (install / promote / death-fallback), voluntary or
    /// not — the spacing is measured from the last actual change.
    pub(crate) fn record_switch(&mut self, now: Instant) {
        self.last_switch = Some(now);
    }
    /// May a VOLUNTARY switch fire now? Dead in prod until the post-flip
    /// score-driven demotion consumes it (the allow tracks that) — the bound
    /// itself is already locked by `voluntary_switch_hysteresis_bound`.
    #[allow(dead_code)]
    pub(crate) fn voluntary_allowed(&self, now: Instant) -> bool {
        self.last_switch
            .is_none_or(|t| now.saturating_duration_since(t) >= SWITCH_MIN_INTERVAL)
    }
}

// ---------------------------------------------------------------------------
// The monitor
// ---------------------------------------------------------------------------

/// The path monitor: per-peer two-plane state + the decision function.
/// Owned by the overlay loop; single-threaded; pure of I/O and clocks.
#[derive(Default)]
pub(crate) struct PathMonitor {
    peers: HashMap<ObjectId, PeerPath>,
    /// Global in-flight probe count (bounds [`decide`]'s Probe proposals).
    probes_in_flight: usize,
}

impl PathMonitor {
    // -- evidence events -----------------------------------------------------

    /// A direct-tier attempt FAILED (carrier death on that tier, or a probe
    /// expiry): book the strike + the suppression penalty. `mbb` is the
    /// make-before-break env state at failure time (the legacy escalation
    /// table reads it at the same moment).
    fn book_failure(&mut self, peer: &ObjectId, tier: DirectTier, mbb: bool, now: Instant) {
        let Some(idx) = tier_idx(tier) else { return };
        let p = self.peers.entry(*peer).or_default();
        let t = &mut p.tiers[idx];
        t.fails += 1;
        t.penalty = Some(Penalty {
            w: weight(tier),
            half_life: suppression_half_life(tier, t.fails, mbb),
            at: now,
        });
    }

    /// P2 seam — a carrier on `tier` died with `reason`. Direct tiers book
    /// the strike+penalty (≡ the legacy sweep's cooldown bookkeeping); every
    /// tier's Q is slammed. Tolerates either landing order with the lifecycle
    /// work: the reason is evidence detail, not control flow.
    pub(crate) fn on_death(
        &mut self,
        peer: &ObjectId,
        tier: DirectTier,
        _reason: DeathReason,
        mbb: bool,
        now: Instant,
    ) {
        if tier.is_direct() {
            self.book_failure(peer, tier, mbb, now);
        }
        let p = self.peers.entry(*peer).or_default();
        match tier_idx(tier) {
            Some(idx) => p.tiers[idx].q.slam(Q_DEATH_SLAM),
            None => p.relay_q.slam(Q_DEATH_SLAM),
        }
        p.hysteresis.record_switch(now);
    }

    /// The sweep advanced the peer's keepalive-inclusive last-heard marker.
    pub(crate) fn on_heard(&mut self, peer: &ObjectId, tier: DirectTier) {
        let p = self.peers.entry(*peer).or_default();
        match tier_idx(tier) {
            Some(idx) => p.tiers[idx].q.feed(Q_HEARD_SAMPLE, Q_HEARD_ALPHA),
            None => p.relay_q.feed(Q_HEARD_SAMPLE, Q_HEARD_ALPHA),
        }
    }

    /// The sweep stored a one-way strike (tx advanced, rx flat).
    pub(crate) fn on_bad_sweep(&mut self, peer: &ObjectId, tier: DirectTier) {
        let p = self.peers.entry(*peer).or_default();
        match tier_idx(tier) {
            Some(idx) => p.tiers[idx].q.feed(Q_BAD_SWEEP_SAMPLE, Q_BAD_SWEEP_ALPHA),
            None => p.relay_q.feed(Q_BAD_SWEEP_SAMPLE, Q_BAD_SWEEP_ALPHA),
        }
    }

    /// The P2 tick cleared the tier's strikes (direct carrier genuinely
    /// receiving) — mirror of the legacy `clear_tier_strikes` disposition
    /// (CC1: the carrier's OWN tier only, never cross-tier).
    pub(crate) fn on_healthy_rx(&mut self, peer: &ObjectId, tier: DirectTier) {
        if let (Some(p), Some(idx)) = (self.peers.get_mut(peer), tier_idx(tier)) {
            p.tiers[idx].fails = 0;
        }
    }

    /// A direct carrier was INSTALLED on `tier` (fresh install, destructive
    /// upgrade, inbound re-point) or the relay carrier was installed
    /// (`tier == Relay`). Records switch time + the LAN attempt marker.
    pub(crate) fn on_installed(&mut self, peer: &ObjectId, tier: DirectTier, now: Instant) {
        let p = self.peers.entry(*peer).or_default();
        if tier == DirectTier::Lan {
            p.last_lan_attempt = Some(now);
        }
        p.hysteresis.record_switch(now);
    }

    /// A make-before-break probe STARTED toward `tier`.
    pub(crate) fn on_probe_started(&mut self, peer: &ObjectId, tier: DirectTier, now: Instant) {
        let p = self.peers.entry(*peer).or_default();
        if p.probe.is_none() {
            self.probes_in_flight += 1;
        }
        p.probe = Some((tier, now));
        if tier == DirectTier::Lan {
            p.last_lan_attempt = Some(now);
        }
    }

    /// A probe settled. Latched: Q credit + tier strikes clear (≡ the legacy
    /// promote's `*_fails.remove`) + switch recorded (the promote IS the
    /// switch). Expired: the tier failure books (strike + penalty), and — F1
    /// — NO Q event.
    pub(crate) fn on_probe_result(
        &mut self,
        peer: &ObjectId,
        tier: DirectTier,
        outcome: ProbeOutcome,
        mbb: bool,
        now: Instant,
    ) {
        if let Some(p) = self.peers.get_mut(peer)
            && p.probe.take().is_some()
        {
            self.probes_in_flight = self.probes_in_flight.saturating_sub(1);
        }
        match outcome {
            ProbeOutcome::Latched { latency_ms } => {
                let deduction = latency_ms
                    .map(|ms| (ms / Q_LATCH_LATENCY_DIVISOR).min(Q_LATCH_MAX_DEDUCTION))
                    .unwrap_or(0.0);
                let p = self.peers.entry(*peer).or_default();
                if let Some(idx) = tier_idx(tier) {
                    p.tiers[idx].q.feed(Q_CLAMP - deduction, Q_LATCH_ALPHA);
                    p.tiers[idx].fails = 0;
                }
                p.hysteresis.record_switch(now);
            }
            ProbeOutcome::Expired => {
                self.book_failure(peer, tier, mbb, now);
            }
        }
    }

    /// A probe was ABORTED without a verdict (peer re-pointed by an inbound
    /// init / feature toggled off): free the slot, book nothing.
    pub(crate) fn on_probe_aborted(&mut self, peer: &ObjectId) {
        if let Some(p) = self.peers.get_mut(peer)
            && p.probe.take().is_some()
        {
            self.probes_in_flight = self.probes_in_flight.saturating_sub(1);
        }
    }

    /// rc.225 ≡ `DirectCooldowns::reset_peer` — the peer's advertised direct
    /// endpoints CHANGED: everything measured against the old endpoints is
    /// stale evidence. Clears penalties, strikes AND quality (F1 — new
    /// endpoints get a fresh Q, completing the rc.225 semantics) for the
    /// direct tiers. Relay-Q is untouched (relay reachability didn't change).
    pub(crate) fn on_endpoint_change(&mut self, peer: &ObjectId) {
        if let Some(p) = self.peers.get_mut(peer) {
            for t in &mut p.tiers {
                t.penalty = None;
                t.fails = 0;
                t.q = Ewma::default();
            }
        }
    }

    /// P8 ≡ the resume-from-suspend `cooldowns = default()`: every score and
    /// suppression is stale evidence after a sleep. Probe bookkeeping is KEPT
    /// — the legacy resume path leaves `upgrade_probes` in flight too (they
    /// settle via their own promote/expire sweep).
    pub(crate) fn on_resume(&mut self) {
        for p in self.peers.values_mut() {
            for t in &mut p.tiers {
                *t = TierState::default();
            }
            p.relay_q = Ewma::default();
            p.last_lan_attempt = None;
        }
    }

    /// The peer left the netmap.
    pub(crate) fn on_peer_removed(&mut self, peer: &ObjectId) {
        if let Some(p) = self.peers.remove(peer)
            && p.probe.is_some()
        {
            self.probes_in_flight = self.probes_in_flight.saturating_sub(1);
        }
    }

    /// P7 — the server pinned this pair to DERP for `ttl`. Annotation only:
    /// direct-tier decisions are unaffected (the pin is about the relay
    /// SUB-tier). Relay-Q resets — evidence gathered under the broken TURN
    /// sub-tier says nothing about the DERP window.
    pub(crate) fn on_forced_derp(&mut self, peer: &ObjectId, ttl: Duration, now: Instant) {
        let p = self.peers.entry(*peer).or_default();
        p.forced_derp_until = Some(now + ttl);
        p.relay_q = Ewma::default();
    }

    /// PR-E — the current strike count on a direct tier. The sweep's sticky-
    /// pin log line keys off this now that the legacy fail maps are gone.
    pub(crate) fn strikes(&self, peer: &ObjectId, tier: DirectTier) -> u32 {
        tier_idx(tier)
            .and_then(|idx| self.peers.get(peer).map(|p| p.tiers[idx].fails))
            .unwrap_or(0)
    }

    /// PR-E — is `tier` in its escalated (sticky) suppression regime for this
    /// peer? Mirrors [`suppression_half_life`]'s table, including the P8
    /// LAN-under-MBB never-sticky rule.
    pub(crate) fn is_sticky(&self, peer: &ObjectId, tier: DirectTier, mbb: bool) -> bool {
        let max = match tier {
            DirectTier::Srflx => MAX_FAILURES_SRFLX,
            DirectTier::Lan if mbb => return false,
            _ => MAX_FAILURES_LAN_PUBLIC,
        };
        self.strikes(peer, tier) >= max
    }

    /// Is the server's forced-DERP pin active for this peer? Consumed by the
    /// PR-B scheduler comparison (never-select-against-a-pin); until then the
    /// pin-invariant tests are its only caller (the allow tracks that).
    #[allow(dead_code)]
    pub(crate) fn forced_derp_active(&self, peer: &ObjectId, now: Instant) -> bool {
        self.peers
            .get(peer)
            .and_then(|p| p.forced_derp_until)
            .is_some_and(|until| until > now)
    }

    // -- the decision --------------------------------------------------------

    /// Eligibility-plane query: may `tier` be attempted for this peer now?
    /// Priors + penalty ONLY — no Q term (the invariant the two-plane split
    /// exists for). The relay tier is always eligible (it is the floor).
    pub(crate) fn eligible(&self, peer: &ObjectId, tier: DirectTier, now: Instant) -> bool {
        let Some(idx) = tier_idx(tier) else {
            return true;
        };
        let p = match self.peers.get(peer) {
            Some(p) => p,
            None => return true, // fresh peer: no penalties anywhere
        };
        let penalty = p.tiers[idx]
            .penalty
            .map(|pen| pen.value(now))
            .unwrap_or(0.0);
        base(tier) - penalty >= B_RELAY - ELIGIBILITY_EPS
    }

    /// Ranking-plane score `S = B + Q − P` — meaningful ONLY among eligible
    /// tiers (never consulted for eligibility).
    pub(crate) fn score(&self, peer: &ObjectId, tier: DirectTier, now: Instant) -> f64 {
        let p = self.peers.get(peer);
        let (q, penalty) = match tier_idx(tier) {
            Some(idx) => p
                .map(|p| {
                    (
                        p.tiers[idx].q.get(),
                        p.tiers[idx]
                            .penalty
                            .map(|pen| pen.value(now))
                            .unwrap_or(0.0),
                    )
                })
                .unwrap_or((0.0, 0.0)),
            None => (p.map(|p| p.relay_q.get()).unwrap_or(0.0), 0.0),
        };
        base(tier) + q - penalty
    }

    /// The path decision for one peer, given what the runtime currently sees:
    /// the incumbent carrier, which direct tiers have a dialable candidate,
    /// and the make-before-break env state. Shadow-compared against the
    /// legacy `install_peers` branch outcomes 1:1:
    ///
    /// * already direct → `Keep` (upg' between direct tiers is post-flip work);
    /// * a probe in flight (this peer, or the global cap) → `Keep`;
    /// * relay incumbent + an eligible direct tier → `Probe` (MBB) /
    ///   `Install` (destructive mode);
    /// * fresh peer + LAN best under MBB → `Probe(Lan)` (P9 — a same-/24
    ///   candidate is only a HINT: vendor-default subnets false-match across
    ///   sites, so the unproven LAN dial must not preempt the working-carrier
    ///   walk; the executor probes LAN and installs the next-best tier / relay
    ///   in the same pass; cap-full → `Relay`);
    /// * fresh peer otherwise → `Install` best eligible direct tier, else `Relay`;
    /// * relay incumbent + nothing eligible → `Keep`.
    ///
    /// "Best" = highest `S` among eligible+available, tie-broken by legacy
    /// precedence; the F2 LAN spacing pin filters LAN scorelessly.
    pub(crate) fn decide(
        &self,
        peer: &ObjectId,
        incumbent: Incumbent,
        avail: TierAvailability,
        mbb: bool,
        now: Instant,
    ) -> PathAction {
        if let Incumbent::Direct(_) = incumbent {
            return PathAction::Keep;
        }
        let state = self.peers.get(peer);
        if state.is_some_and(|p| p.probe.is_some()) {
            // One probe per peer, structural. An installed incumbent keeps
            // carrying traffic while the probe runs; a FRESH peer (no
            // carrier at all) must not sit carrierless waiting on it —
            // PR-C: ride relay for the working carrier (P9's carrier-first
            // principle) and let the probe promote over it when it latches.
            // Soak #1 Class A caught legacy racing a fresh direct install
            // over a dangling probe here (a sweep death leaves
            // `upgrade_probes` intact and the fresh walk ignored it, so a
            // late promote could clobber the fresh carrier).
            return match incumbent {
                Incumbent::None => PathAction::Relay,
                _ => PathAction::Keep,
            };
        }

        let lan_spaced_out = state
            .and_then(|p| p.last_lan_attempt)
            .is_some_and(|t| now.saturating_duration_since(t) < LAN_PROBE_SPACING);

        let mut best: Option<(DirectTier, f64)> = None;
        for tier in DIRECT_TIERS {
            if !avail.has(tier) || !self.eligible(peer, tier, now) {
                continue;
            }
            // F2 — the LAN spacing pin is scoreless and unconditional.
            if tier == DirectTier::Lan && lan_spaced_out {
                continue;
            }
            let s = self.score(peer, tier, now);
            // Strictly-greater keeps the DIRECT_TIERS iteration order as the
            // tie-break — cold start (all S = B) IS the legacy precedence.
            if best.is_none_or(|(_, bs)| s > bs) {
                best = Some((tier, s));
            }
        }

        match (incumbent, best) {
            (Incumbent::Relay, Some((tier, _))) => {
                if mbb {
                    if self.probes_in_flight >= PROBE_CAP {
                        return PathAction::Keep;
                    }
                    PathAction::Probe(tier)
                } else {
                    PathAction::Install(tier)
                }
            }
            (Incumbent::Relay, None) => PathAction::Keep,
            // P9 — fresh-LAN is probe-first under MBB: a same-/24 candidate is
            // only a HINT (192.168.68.0/24 Deco, 192.168.1.0/24 … false-match
            // across sites; field 2026-07-28: fresh installs burned the 12 s
            // LAN deadline with NO carrier, repeatedly, against corp peers
            // advertising another city's 192.168.68.x). Probe the LAN, take
            // the next-best tier / relay for the working carrier meanwhile.
            // Cap-full → Relay: the carrier comes first, the probe can wait
            // for the relay→direct upgrade arm once a slot frees.
            (Incumbent::None, Some((DirectTier::Lan, _))) if mbb => {
                if self.probes_in_flight >= PROBE_CAP {
                    return PathAction::Relay;
                }
                PathAction::Probe(DirectTier::Lan)
            }
            (Incumbent::None, Some((tier, _))) => PathAction::Install(tier),
            (Incumbent::None, None) => PathAction::Relay,
            (Incumbent::Direct(_), _) => unreachable!("handled above"),
        }
    }

    /// The inbound-init decision (≡ `handle_direct_inbound`'s accept/refuse):
    /// feed the evidence (authenticated init = strong positive; a srflx init
    /// is D9 — proof the pair CAN punch right now, so it clears the srflx
    /// suppression outright), then accept iff the tier is eligible.
    /// `None` = refuse (≡ the legacy cooldown gate). The action mirrors the
    /// legacy branch: already-probing → `Keep` (answer only), relay incumbent
    /// under MBB → `Probe`, else `Install` (re-point).
    pub(crate) fn inbound_init(
        &mut self,
        peer: &ObjectId,
        tier: DirectTier,
        incumbent: Incumbent,
        mbb: bool,
        now: Instant,
    ) -> Option<PathAction> {
        let p = self.peers.entry(*peer).or_default();
        if let Some(idx) = tier_idx(tier) {
            p.tiers[idx].q.feed(Q_INBOUND_SAMPLE, Q_INBOUND_ALPHA);
            if tier == DirectTier::Srflx {
                // D9 — the init traversed both NATs; the suppression was
                // measured against conditions that just changed.
                p.tiers[idx].penalty = None;
                p.tiers[idx].fails = 0;
            }
        }
        if !self.eligible(peer, tier, now) {
            return None;
        }
        if self.peers.get(peer).is_some_and(|p| p.probe.is_some()) {
            return Some(PathAction::Keep);
        }
        if mbb && incumbent == Incumbent::Relay {
            Some(PathAction::Probe(tier))
        } else {
            Some(PathAction::Install(tier))
        }
    }
}

// ---------------------------------------------------------------------------
// Divergence classification (the shadow harness's pure half)
// ---------------------------------------------------------------------------

/// Shadow-comparison outcome for one decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DivergenceClass {
    /// Monitor and legacy chose the same action.
    Match,
    /// They differ — the soak gate counts these (steady < 0.1 %,
    /// transition < 2 %; see the P3 acceptance gate).
    Diverged,
}

/// Strict comparison — PR-A deliberately classifies nothing as "benign":
/// timing-window forgiveness enters with PR-B's scheduler comparison, where
/// it can be justified per-class instead of blurring the signal here.
pub(crate) fn classify(legacy: PathAction, monitor: PathAction) -> DivergenceClass {
    if legacy == monitor {
        DivergenceClass::Match
    } else {
        DivergenceClass::Diverged
    }
}

/// Rolling shadow counters, surfaced in the 10-minute summary log.
#[derive(Default, Debug)]
pub(crate) struct ShadowStats {
    /// Compared decisions (install_peers surfaces + inbound).
    pub(crate) decisions: u64,
    /// Strict mismatches.
    pub(crate) diverged: u64,
    /// HARMFUL class: the monitor refused a tier that legacy then proved
    /// (probe latched / carrier turned healthy) within 60 s. The acceptance
    /// gate requires ZERO of these.
    pub(crate) harmful: u64,
    /// Model-bug tripwire: a tier the monitor still called eligible
    /// immediately after its own death/expiry penalty booked (must be 0 —
    /// the penalty math guarantees ineligibility right after booking).
    pub(crate) post_death_eligible: u64,
    /// PR-C (soak #1 finding) — D10 re-dials, counted but NOT compared: a
    /// zombie-srflx re-dial refreshes the DIAL TARGET of the current tier
    /// (Srflx→Srflx), so it is not a tier-selection event and comparing it
    /// against `decide()` was a PR-A over-inclusion (50+/host/45 h of false
    /// "divergence" in soak #1). The re-dial stays execution-layer
    /// permanently — even a post-PR-E authoritative monitor doesn't own
    /// dial-target freshness.
    pub(crate) d10_redials: u64,
    /// P3 PR-B — `(decisions, diverged)` per trigger class
    /// (netmap / delta / reupgrade / resume / initial / inbound): the
    /// scheduler comparison as its own divergence class. A reupgrade-tick
    /// divergence means the monitor would have SCHEDULED differently; a
    /// netmap/delta one that it would have SELECTED differently — the soak
    /// gate reads them separately.
    pub(crate) by_class: HashMap<&'static str, (u64, u64)>,
}

impl ShadowStats {
    /// Compact, sorted `class:decisions/diverged` rendering for the 10-min
    /// summary log line.
    pub(crate) fn classes_line(&self) -> String {
        let mut cls: Vec<String> = self
            .by_class
            .iter()
            .map(|(k, (d, v))| format!("{k}:{d}/{v}"))
            .collect();
        cls.sort();
        cls.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> ObjectId {
        let mut b = [0u8; 12];
        b[11] = n;
        ObjectId::from_bytes(b)
    }

    fn all_avail() -> TierAvailability {
        TierAvailability {
            lan: true,
            public: true,
            srflx: true,
        }
    }

    /// Drive `n` heard events into a tier's Q.
    fn pump_heard(m: &mut PathMonitor, peer: &ObjectId, tier: DirectTier, n: usize) {
        for _ in 0..n {
            m.on_heard(peer, tier);
        }
    }

    /// PR-D — unset (and anything unrecognized) defaults to ON; `shadow` is
    /// the explicit per-host revert; `off` still kills the monitor.
    #[test]
    fn mode_parse_defaults_to_on() {
        assert_eq!(PathMonMode::parse(None), PathMonMode::On);
        assert_eq!(PathMonMode::parse(Some("on")), PathMonMode::On);
        assert_eq!(PathMonMode::parse(Some("1")), PathMonMode::On);
        assert_eq!(PathMonMode::parse(Some("bogus")), PathMonMode::On);
        assert_eq!(PathMonMode::parse(Some("shadow")), PathMonMode::Shadow);
        assert_eq!(PathMonMode::parse(Some("SHADOW")), PathMonMode::Shadow);
        assert_eq!(PathMonMode::parse(Some("off")), PathMonMode::Off);
        assert_eq!(PathMonMode::parse(Some("0")), PathMonMode::Off);
        assert_eq!(PathMonMode::On.as_str(), "on");
        assert_eq!(PathMonMode::Shadow.as_str(), "shadow");
    }

    #[test]
    fn ewma_clamps_and_converges() {
        let mut e = Ewma::default();
        e.feed(100.0, 0.4);
        assert!((e.get() - 40.0).abs() < 1e-9);
        for _ in 0..200 {
            e.feed(100.0, 0.4);
        }
        assert!(e.get() <= 100.0 && e.get() > 99.9, "converges under clamp");
        e.slam(-250.0);
        assert_eq!(e.get(), -100.0, "slam clamps");
    }

    /// The eligibility plane reproduces the legacy 60 s window exactly:
    /// suppressed at 59 s, eligible at 61 s (the boundary instant itself is
    /// eligible, like legacy's `until > now`).
    #[test]
    fn eligibility_parity_ordinary_window_59_61() {
        for tier in DIRECT_TIERS {
            let mut m = PathMonitor::default();
            let a = oid(1);
            let t0 = Instant::now();
            m.on_death(&a, tier, DeathReason::HandshakeDeadline, true, t0);
            assert!(
                !m.eligible(&a, tier, t0 + Duration::from_secs(59)),
                "{tier:?} must still be suppressed at 59 s"
            );
            assert!(
                m.eligible(&a, tier, t0 + Duration::from_secs(61)),
                "{tier:?} must be eligible again at 61 s"
            );
            // Other tiers were never poisoned (CC1).
            for other in DIRECT_TIERS {
                if other != tier {
                    assert!(m.eligible(&a, other, t0 + Duration::from_secs(1)));
                }
            }
        }
    }

    /// The escalated window (2 strikes LAN/public, 3 srflx) reproduces the
    /// legacy 15 min deny: suppressed at 14:59, eligible at 15:01 — and the
    /// LAN tier under make-before-break NEVER escalates (P8).
    #[test]
    fn eligibility_parity_escalated_window_899_901() {
        // Public: second failure escalates.
        let mut m = PathMonitor::default();
        let a = oid(1);
        let t0 = Instant::now();
        m.on_death(
            &a,
            DirectTier::Public,
            DeathReason::HandshakeDeadline,
            true,
            t0,
        );
        let t1 = t0 + Duration::from_secs(120);
        m.on_death(
            &a,
            DirectTier::Public,
            DeathReason::HandshakeDeadline,
            true,
            t1,
        );
        assert!(!m.eligible(&a, DirectTier::Public, t1 + Duration::from_secs(899)));
        assert!(m.eligible(&a, DirectTier::Public, t1 + Duration::from_secs(901)));

        // Srflx: third failure escalates (the second does not).
        let mut m = PathMonitor::default();
        m.on_death(
            &a,
            DirectTier::Srflx,
            DeathReason::HandshakeDeadline,
            true,
            t0,
        );
        m.on_death(
            &a,
            DirectTier::Srflx,
            DeathReason::HandshakeDeadline,
            true,
            t1,
        );
        assert!(
            m.eligible(&a, DirectTier::Srflx, t1 + Duration::from_secs(61)),
            "srflx strike 2 stays on the ordinary window"
        );
        let t2 = t1 + Duration::from_secs(120);
        m.on_death(
            &a,
            DirectTier::Srflx,
            DeathReason::HandshakeDeadline,
            true,
            t2,
        );
        assert!(!m.eligible(&a, DirectTier::Srflx, t2 + Duration::from_secs(899)));
        assert!(m.eligible(&a, DirectTier::Srflx, t2 + Duration::from_secs(901)));

        // LAN under MBB: strike 5 still gets the ordinary 60 s window.
        let mut m = PathMonitor::default();
        let mut t = t0;
        for _ in 0..5 {
            m.on_death(&a, DirectTier::Lan, DeathReason::HandshakeDeadline, true, t);
            t += Duration::from_secs(120);
        }
        let last = t - Duration::from_secs(120);
        assert!(m.eligible(&a, DirectTier::Lan, last + Duration::from_secs(61)));
        // LAN with MBB env-OFF: second failure escalates like public.
        let mut m = PathMonitor::default();
        m.on_death(
            &a,
            DirectTier::Lan,
            DeathReason::HandshakeDeadline,
            false,
            t0,
        );
        m.on_death(
            &a,
            DirectTier::Lan,
            DeathReason::HandshakeDeadline,
            false,
            t1,
        );
        assert!(!m.eligible(&a, DirectTier::Lan, t1 + Duration::from_secs(899)));
        assert!(m.eligible(&a, DirectTier::Lan, t1 + Duration::from_secs(901)));
    }

    /// F1's core invariant: NO Q state — however adversarial — can move an
    /// eligibility boundary in either direction. Two monitors book the same
    /// failure; one is then Q-hammered (max positive on the failed tier, max
    /// negative on the others); the eligibility timings must be identical.
    #[test]
    fn no_q_state_can_delay_or_advance_eligibility() {
        let a = oid(1);
        let t0 = Instant::now();
        let mut quiet = PathMonitor::default();
        let mut noisy = PathMonitor::default();
        for m in [&mut quiet, &mut noisy] {
            m.on_death(
                &a,
                DirectTier::Public,
                DeathReason::HandshakeDeadline,
                true,
                t0,
            );
        }
        // Pump the failed tier's Q to the ceiling (+heard, +inbound on a LAN
        // init so no srflx D9-clear touches penalties) and the others to the
        // floor.
        pump_heard(&mut noisy, &a, DirectTier::Public, 500);
        for _ in 0..500 {
            noisy.on_bad_sweep(&a, DirectTier::Lan);
            noisy.on_bad_sweep(&a, DirectTier::Srflx);
        }
        for probe_t in [59u64, 61, 899, 901] {
            let t = t0 + Duration::from_secs(probe_t);
            assert_eq!(
                quiet.eligible(&a, DirectTier::Public, t),
                noisy.eligible(&a, DirectTier::Public, t),
                "Q must not move the Public boundary at {probe_t} s"
            );
        }
        // And negative Q on never-failed tiers cannot make them ineligible.
        assert!(noisy.eligible(&a, DirectTier::Lan, t0 + Duration::from_secs(1)));
        assert!(noisy.eligible(&a, DirectTier::Srflx, t0 + Duration::from_secs(1)));
    }

    /// Cold start reproduces the legacy precedence walk exactly.
    #[test]
    fn cold_start_ordering_matches_legacy_precedence() {
        let m = PathMonitor::default();
        let a = oid(1);
        let now = Instant::now();
        // P9 — LAN-best on a FRESH peer probes first under MBB (the same-/24
        // candidate is unproven); the destructive walk survives with MBB off.
        assert_eq!(
            m.decide(&a, Incumbent::None, all_avail(), true, now),
            PathAction::Probe(DirectTier::Lan)
        );
        assert_eq!(
            m.decide(&a, Incumbent::None, all_avail(), false, now),
            PathAction::Install(DirectTier::Lan)
        );
        let no_lan = TierAvailability {
            lan: false,
            ..all_avail()
        };
        assert_eq!(
            m.decide(&a, Incumbent::None, no_lan, true, now),
            PathAction::Install(DirectTier::Public)
        );
        let srflx_only = TierAvailability {
            lan: false,
            public: false,
            srflx: true,
        };
        assert_eq!(
            m.decide(&a, Incumbent::None, srflx_only, true, now),
            PathAction::Install(DirectTier::Srflx)
        );
        assert_eq!(
            m.decide(&a, Incumbent::None, TierAvailability::default(), true, now),
            PathAction::Relay
        );
        // Relay incumbent: MBB probes, destructive mode installs, nothing
        // available keeps.
        assert_eq!(
            m.decide(&a, Incumbent::Relay, all_avail(), true, now),
            PathAction::Probe(DirectTier::Lan)
        );
        assert_eq!(
            m.decide(&a, Incumbent::Relay, all_avail(), false, now),
            PathAction::Install(DirectTier::Lan)
        );
        assert_eq!(
            m.decide(&a, Incumbent::Relay, TierAvailability::default(), true, now),
            PathAction::Keep
        );
        // Already direct: always Keep (cross-direct upgrades are post-flip).
        assert_eq!(
            m.decide(
                &a,
                Incumbent::Direct(DirectTier::Srflx),
                all_avail(),
                true,
                now
            ),
            PathAction::Keep
        );
    }

    /// Q ranks among ELIGIBLE tiers: after LAN's penalty lapses its slammed Q
    /// keeps it ranked below a heard-up srflx — but never OFF the ballot.
    #[test]
    fn ranking_prefers_higher_q_among_eligible() {
        let mut m = PathMonitor::default();
        let a = oid(1);
        let t0 = Instant::now();
        m.on_death(
            &a,
            DirectTier::Lan,
            DeathReason::HandshakeDeadline,
            true,
            t0,
        );
        pump_heard(&mut m, &a, DirectTier::Srflx, 14); // Q ≈ +51
        let t = t0 + Duration::from_secs(120); // LAN penalty ≈ 100, eligible again
        assert!(m.eligible(&a, DirectTier::Lan, t));
        let s_lan = m.score(&a, DirectTier::Lan, t);
        let s_srflx = m.score(&a, DirectTier::Srflx, t);
        assert!(
            s_srflx > s_lan,
            "srflx ({s_srflx:.1}) must outrank slammed+penalized LAN ({s_lan:.1})"
        );
        let avail = TierAvailability {
            lan: true,
            public: false,
            srflx: true,
        };
        assert_eq!(
            m.decide(&a, Incumbent::Relay, avail, true, t),
            PathAction::Probe(DirectTier::Srflx)
        );
    }

    /// One probe per peer + the global cap serialize Probe proposals.
    #[test]
    fn probe_serialization_one_per_peer_and_global_cap() {
        let mut m = PathMonitor::default();
        let a = oid(1);
        let now = Instant::now();
        m.on_probe_started(&a, DirectTier::Srflx, now);
        assert_eq!(
            m.decide(&a, Incumbent::Relay, all_avail(), true, now),
            PathAction::Keep,
            "an in-flight probe blocks new proposals for the peer"
        );
        m.on_probe_result(
            &a,
            DirectTier::Srflx,
            ProbeOutcome::Latched {
                latency_ms: Some(40.0),
            },
            true,
            now,
        );
        assert_eq!(m.probes_in_flight, 0, "latch frees the slot");

        // Fill the global cap with other peers.
        for n in 0..PROBE_CAP {
            m.on_probe_started(&oid(100 + n as u8), DirectTier::Lan, now);
        }
        let fresh = oid(2);
        assert_eq!(
            m.decide(&fresh, Incumbent::Relay, all_avail(), true, now),
            PathAction::Keep,
            "global cap blocks new Probe proposals"
        );
        // P9 — a fresh-LAN peer under MBB is probe-first, so the cap applies
        // to it too: cap-full falls back to Relay (carrier first), never to a
        // destructive unproven-LAN install.
        assert_eq!(
            m.decide(&fresh, Incumbent::None, all_avail(), true, now),
            PathAction::Relay
        );
        m.on_peer_removed(&oid(100));
        assert_eq!(
            m.decide(&fresh, Incumbent::Relay, all_avail(), true, now),
            PathAction::Probe(DirectTier::Lan),
            "freeing one slot unblocks"
        );
    }

    /// P9 — a fresh peer whose best tier is LAN probes first under MBB (the
    /// same-/24 candidate is unproven — vendor-default subnets false-match
    /// across sites); destructive mode (MBB env-off) and non-LAN best tiers
    /// keep the legacy fresh `Install`.
    #[test]
    fn fresh_lan_probes_first_under_mbb() {
        let m = PathMonitor::default();
        let a = oid(7);
        let now = Instant::now();
        assert_eq!(
            m.decide(&a, Incumbent::None, all_avail(), true, now),
            PathAction::Probe(DirectTier::Lan),
            "fresh + LAN best + MBB → shadow probe, not a destructive install"
        );
        assert_eq!(
            m.decide(&a, Incumbent::None, all_avail(), false, now),
            PathAction::Install(DirectTier::Lan),
            "MBB env-off keeps the destructive fresh install"
        );
        let no_lan = TierAvailability {
            lan: false,
            public: true,
            srflx: true,
        };
        assert_eq!(
            m.decide(&a, Incumbent::None, no_lan, true, now),
            PathAction::Install(DirectTier::Public),
            "non-LAN best tiers keep the fresh Install"
        );
    }

    /// F2 — the LAN spacing pin holds under adversarial Q and with no
    /// penalty on the books (probe aborted without failure).
    #[test]
    fn lan_spacing_pinned_under_q_extremes() {
        let mut m = PathMonitor::default();
        let a = oid(1);
        let t0 = Instant::now();
        m.on_probe_started(&a, DirectTier::Lan, t0);
        m.on_probe_aborted(&a); // freed WITHOUT a failure — no penalty booked
        assert!(m.eligible(&a, DirectTier::Lan, t0 + Duration::from_secs(1)));
        // Q-hammer LAN to the ceiling.
        pump_heard(&mut m, &a, DirectTier::Lan, 500);
        let lan_only = TierAvailability {
            lan: true,
            public: false,
            srflx: true,
        };
        // 30 s after the attempt: LAN is eligible + top-Q, yet the pin
        // filters it — the next eligible tier is proposed instead.
        assert_eq!(
            m.decide(
                &a,
                Incumbent::Relay,
                lan_only,
                true,
                t0 + Duration::from_secs(30)
            ),
            PathAction::Probe(DirectTier::Srflx),
            "LAN must be spaced out at 30 s regardless of Q"
        );
        // 61 s after: the pin releases.
        assert_eq!(
            m.decide(
                &a,
                Incumbent::Relay,
                lan_only,
                true,
                t0 + Duration::from_secs(61)
            ),
            PathAction::Probe(DirectTier::Lan)
        );
    }

    /// The forced-DERP pin is an annotation: direct decisions identical with
    /// and without it; relay-Q resets; the pin expires by TTL.
    #[test]
    fn forced_derp_pin_invariants() {
        let mut m = PathMonitor::default();
        let a = oid(1);
        let now = Instant::now();
        pump_heard(&mut m, &a, DirectTier::Relay, 20);
        assert!(m.score(&a, DirectTier::Relay, now) > B_RELAY);
        let before = m.decide(&a, Incumbent::Relay, all_avail(), true, now);
        m.on_forced_derp(&a, Duration::from_secs(1800), now);
        assert!(m.forced_derp_active(&a, now));
        assert_eq!(
            m.decide(&a, Incumbent::Relay, all_avail(), true, now),
            before,
            "the pin must not touch direct-tier decisions"
        );
        for tier in DIRECT_TIERS {
            assert!(
                m.eligible(&a, tier, now),
                "pin never blocks direct eligibility"
            );
        }
        assert_eq!(
            m.score(&a, DirectTier::Relay, now),
            B_RELAY,
            "relay-Q resets for a fresh evidence window"
        );
        assert!(!m.forced_derp_active(&a, now + Duration::from_secs(1801)));
    }

    /// D9 — an inbound srflx init clears the srflx suppression outright (even
    /// a sticky one) and accepts; LAN/public inits still honour their
    /// suppression window exactly like the legacy cooldown gate.
    #[test]
    fn inbound_init_d9_clears_srflx_and_respects_cooldowns() {
        let mut m = PathMonitor::default();
        let a = oid(1);
        let t0 = Instant::now();
        // Srflx: escalate to sticky (3 strikes), then an inbound init lands.
        for i in 0..3u64 {
            m.on_death(
                &a,
                DirectTier::Srflx,
                DeathReason::HandshakeDeadline,
                true,
                t0 + Duration::from_secs(i),
            );
        }
        let t = t0 + Duration::from_secs(10);
        assert!(!m.eligible(&a, DirectTier::Srflx, t));
        assert_eq!(
            m.inbound_init(&a, DirectTier::Srflx, Incumbent::Relay, true, t),
            Some(PathAction::Probe(DirectTier::Srflx)),
            "a srflx init overrides the suppression (D9)"
        );
        assert!(m.eligible(&a, DirectTier::Srflx, t), "and clears it");

        // LAN: suppressed → refused; lapsed → accepted; re-point shape when
        // not on relay/MBB.
        m.on_death(&a, DirectTier::Lan, DeathReason::HandshakeDeadline, true, t);
        assert_eq!(
            m.inbound_init(
                &a,
                DirectTier::Lan,
                Incumbent::Relay,
                true,
                t + Duration::from_secs(30)
            ),
            None,
            "a cooling LAN init is refused (anti-thrash)"
        );
        let late = t + Duration::from_secs(61);
        assert_eq!(
            m.inbound_init(&a, DirectTier::Lan, Incumbent::Relay, true, late),
            Some(PathAction::Probe(DirectTier::Lan))
        );
        assert_eq!(
            m.inbound_init(
                &a,
                DirectTier::Lan,
                Incumbent::Direct(DirectTier::Public),
                true,
                late
            ),
            Some(PathAction::Install(DirectTier::Lan)),
            "direct-on-another-src re-points (no relay to protect)"
        );
        assert_eq!(
            m.inbound_init(&a, DirectTier::Lan, Incumbent::Relay, false, late),
            Some(PathAction::Install(DirectTier::Lan)),
            "MBB off re-points destructively"
        );
    }

    /// F1 — endpoint change resets penalties, strikes AND Q; the next failure
    /// restarts the escalation ladder on the ordinary window.
    #[test]
    fn endpoint_change_resets_penalties_strikes_and_q() {
        let mut m = PathMonitor::default();
        let a = oid(1);
        let t0 = Instant::now();
        m.on_death(
            &a,
            DirectTier::Public,
            DeathReason::HandshakeDeadline,
            true,
            t0,
        );
        m.on_death(
            &a,
            DirectTier::Public,
            DeathReason::HandshakeDeadline,
            true,
            t0 + Duration::from_secs(61),
        );
        let t = t0 + Duration::from_secs(120);
        assert!(!m.eligible(&a, DirectTier::Public, t), "sticky deny active");
        m.on_endpoint_change(&a);
        assert!(
            m.eligible(&a, DirectTier::Public, t),
            "reset restores eligibility"
        );
        assert_eq!(m.score(&a, DirectTier::Public, t), B_PUBLIC, "Q reset to 0");
        // Next failure books the ORDINARY window (fails restarted), not the
        // escalated one.
        m.on_death(
            &a,
            DirectTier::Public,
            DeathReason::HandshakeDeadline,
            true,
            t,
        );
        assert!(m.eligible(&a, DirectTier::Public, t + Duration::from_secs(61)));
    }

    /// F1 — a probe expiry books the penalty but NO Q event; a death slams Q.
    #[test]
    fn death_books_strike_and_penalty_and_probe_expire_books_no_q() {
        let mut m = PathMonitor::default();
        let a = oid(1);
        let t0 = Instant::now();
        pump_heard(&mut m, &a, DirectTier::Srflx, 14);
        let q_before = m.score(&a, DirectTier::Srflx, t0) - B_SRFLX;
        m.on_probe_started(&a, DirectTier::Srflx, t0);
        m.on_probe_result(&a, DirectTier::Srflx, ProbeOutcome::Expired, true, t0);
        assert!(!m.eligible(&a, DirectTier::Srflx, t0 + Duration::from_secs(30)));
        let far = t0 + Duration::from_secs(3600); // penalty fully decayed
        let q_after = m.score(&a, DirectTier::Srflx, far) - B_SRFLX;
        assert!(
            (q_after - q_before).abs() < 1e-9,
            "expiry must not touch Q (penalty already encodes it — F1)"
        );
        m.on_death(&a, DirectTier::Srflx, DeathReason::RxStale, true, far);
        let q_slammed = m.score(&a, DirectTier::Srflx, far + Duration::from_secs(3600)) - B_SRFLX;
        assert_eq!(q_slammed, Q_DEATH_SLAM, "death slams Q to the floor");
    }

    /// P8 — resume clears every peer's scores + suppressions but keeps probe
    /// bookkeeping (the legacy resume path leaves `upgrade_probes` in flight).
    #[test]
    fn resume_clears_all_peers_but_keeps_probe_bookkeeping() {
        let mut m = PathMonitor::default();
        let (a, b) = (oid(1), oid(2));
        let t0 = Instant::now();
        m.on_death(
            &a,
            DirectTier::Lan,
            DeathReason::HandshakeDeadline,
            true,
            t0,
        );
        m.on_death(
            &b,
            DirectTier::Public,
            DeathReason::HandshakeDeadline,
            true,
            t0,
        );
        m.on_probe_started(&b, DirectTier::Srflx, t0);
        m.on_resume();
        let t = t0 + Duration::from_secs(1);
        assert!(m.eligible(&a, DirectTier::Lan, t));
        assert!(m.eligible(&b, DirectTier::Public, t));
        assert_eq!(m.score(&a, DirectTier::Lan, t), B_LAN);
        assert_eq!(
            m.probes_in_flight, 1,
            "in-flight probe bookkeeping survives"
        );
        assert_eq!(
            m.decide(&b, Incumbent::Relay, all_avail(), true, t),
            PathAction::Keep,
            "the surviving probe still serializes"
        );
    }

    #[test]
    fn classifier_and_stats_table() {
        assert_eq!(
            classify(PathAction::Keep, PathAction::Keep),
            DivergenceClass::Match
        );
        assert_eq!(
            classify(
                PathAction::Probe(DirectTier::Lan),
                PathAction::Probe(DirectTier::Lan)
            ),
            DivergenceClass::Match
        );
        for (l, m) in [
            (
                PathAction::Probe(DirectTier::Lan),
                PathAction::Probe(DirectTier::Srflx),
            ),
            (
                PathAction::Install(DirectTier::Lan),
                PathAction::Probe(DirectTier::Lan),
            ),
            (PathAction::Relay, PathAction::Keep),
            (PathAction::Keep, PathAction::Install(DirectTier::Public)),
        ] {
            assert_eq!(classify(l, m), DivergenceClass::Diverged);
        }
        let s = ShadowStats::default();
        assert_eq!(
            (s.decisions, s.diverged, s.harmful, s.post_death_eligible),
            (0, 0, 0, 0)
        );
    }

    /// The voluntary-switch guard: an adversarial record/query stream can
    /// never extract two voluntary switches within [`SWITCH_MIN_INTERVAL`].
    #[test]
    fn voluntary_switch_hysteresis_bound() {
        let mut h = Hysteresis::default();
        let t0 = Instant::now();
        assert!(h.voluntary_allowed(t0), "fresh peer may switch");
        h.record_switch(t0);
        let mut granted: Vec<Instant> = vec![t0];
        // Adversarial stream: query every second for 5 minutes, record every
        // grant (a granted voluntary switch IS a switch).
        for s in 1..300u64 {
            let t = t0 + Duration::from_secs(s);
            if h.voluntary_allowed(t) {
                h.record_switch(t);
                granted.push(t);
            }
        }
        for w in granted.windows(2) {
            assert!(
                w[1].duration_since(w[0]) >= SWITCH_MIN_INTERVAL,
                "two voluntary switches closer than the floor"
            );
        }
        assert_eq!(granted.len(), 10, "300 s / 30 s floor = 10 grants");
    }

    /// The probe-latch Q credit deducts observed latency (PR-B feeds it) and
    /// clears the tier's strikes like the legacy promote.
    #[test]
    fn probe_latch_credits_q_and_clears_strikes() {
        let mut m = PathMonitor::default();
        let a = oid(1);
        let t0 = Instant::now();
        // One failure (strike 1), then a successful probe.
        m.on_death(
            &a,
            DirectTier::Public,
            DeathReason::HandshakeDeadline,
            true,
            t0,
        );
        let t = t0 + Duration::from_secs(61);
        m.on_probe_started(&a, DirectTier::Public, t);
        m.on_probe_result(
            &a,
            DirectTier::Public,
            ProbeOutcome::Latched {
                latency_ms: Some(2000.0),
            },
            true,
            t,
        );
        // Sample = 100 − min(50, 2000/40=50) = 50; Q was slammed −100 →
        // −100 + 0.4×(50−(−100)) = −40.
        let q = m.score(&a, DirectTier::Public, t + Duration::from_secs(3600)) - B_PUBLIC;
        assert!(
            (q - (-40.0)).abs() < 1e-6,
            "latency-deducted latch credit (got {q})"
        );
        // Strikes cleared: the NEXT failure books the ordinary window again.
        m.on_death(&a, DirectTier::Public, DeathReason::RxStale, true, t);
        assert!(m.eligible(&a, DirectTier::Public, t + Duration::from_secs(61)));
    }
}

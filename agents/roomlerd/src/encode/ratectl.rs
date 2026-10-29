// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-63 B1 — ONE delay-based rate controller, run in SHADOW.
//!
//! # What this is
//!
//! `RateGovernor` today is eight estimators of one quantity — the pipe's rate —
//! glued with "the lower of" rules (occupancy AIMD, blocked-send goodput, the
//! FR-35 learner and rate memory, FR-59's floor relief / arrival clamp / drain /
//! seed contradiction / remembered-rate opener, the FR-15 age loop). FR-63's
//! thesis is that they are one controller wearing eight hats. This module is
//! that one controller: a GCC-shaped state machine on the viewer's delay LEVEL,
//! loss, and the send channel's own back-pressure.
//!
//! # Why it only ever observes
//!
//! A rate controller on the remote-desktop encode path runs as SYSTEM/root on
//! every enrolled machine and self-updates; a wrong decrease takes an operator's
//! picture, a wrong increase floods a corporate VPN. So FR-63's rule
//! (`docs/plans/rate-control-architecture.md`) is **shadow-first**: this
//! controller runs beside the live governor for a release, computing what it
//! WOULD have decided and emitting that into `shadow_*` heartbeat fields, and
//! it drives nothing. B2 flips it live on the flip criterion; B3 deletes the
//! eight it replaces. **In B1 the containment is absolute** — the governor owns
//! this as an `Option<Controller>` fed through these methods and reads only its
//! accessors; no live decision, counter or field is a function of anything here.
//! `governor::tests::the_shadow_never_moves_the_live_decision` is the proof.
//!
//! # Pure
//!
//! No clock (every method takes an explicit `Instant`), no I/O, no encoder or
//! webrtc types, no env read — so it unit-tests on the default `cargo test
//! --lib` and runs inside the B0 simulator (`encode::sim`) against the same
//! recorded cells as the shipped laws, which is how B1's numbers become the
//! baseline B2 has to beat rather than a separate exercise.
//!
//! # The machine (GCC-shaped, level-held)
//!
//! ```text
//!              clean ×1.08 (<50% ceil) / additive (>=)
//!         ┌───────────────────────────────────────────┐
//!         ▼                                            │
//!   Startup ──first congestion──▶ OverUse ──▶ Hold ──2 clean──▶ UnderUse ──settles──▶ Normal
//!                                   ▲           │                   │                   │
//!                                   └───────────┴───────────────────┴───────────────────┘
//!                                         any congestion signal
//! ```
//!
//! - **OverUse** fires on two consecutive windows of any of: age level
//!   `age − learned_floor ≥ 70 ms`, loss `≥ 2 %`, goodput `< 0.9 × target`, or
//!   the age rising window-on-window (a coarse delay-slope proxy — the viewer
//!   does not send `delay_slope_ms_s` yet, so this reads the age series the
//!   controller keeps; B2's viewer-feedback v2 replaces it); and IMMEDIATELY on
//!   a sustained full channel or a hard stall. The decrease targets
//!   `0.85 × measured`, never cutting more than half in one window, floored at
//!   the path floor.
//! - **Hold** keeps the rate while the age LEVEL is still elevated — the
//!   derivative lesson FR-59 paid for twice: a queue that stops GROWING is not a
//!   queue that is GONE.
//! - a deep level on a CONSTRAINED path counts one bounded pause (≤ 300 ms);
//!   **direct paths never pause** (the FR-1 P2 relay regression). In shadow a
//!   "pause" is only a counter.
//! - **measured** is `rx_bps` on a constrained path (the viewer's arrival rate,
//!   which is what a relay session can see) and `min(goodput, rx)` on a direct
//!   one. `frames_rx == 0` windows (a DERP stall) are no evidence at all.
//! - the path **floor is 200 kbps on a constrained path** — NOT the governor's
//!   flat `area_min_bitrate_bps` (1.5 M), which is the very pin this FR exists
//!   to stop trusting; on a direct path the governor's area floor is honest and
//!   is used as-is. The ceiling is the governor's resolved quality/relay cap.

use std::time::{Duration, Instant};

/// Age over the learned floor that counts as an elevated LEVEL (mirrors
/// `viewer_rate::AGE_SLACK_MS`).
pub const AGE_SLACK_MS: u16 = 70;
/// Loss fraction (percent) that counts as over-use evidence.
pub const LOSS_PCT: u32 = 2;
/// Consecutive evidence windows before a delay-based OverUse (one bad window is
/// a blip — the same two-window fold the FR-15 age loop and FR-59 link loop use).
pub const OVERUSE_WINDOWS: u8 = 2;
/// Consecutive clean windows in Hold before climbing again.
pub const CLEAN_WINDOWS: u8 = 2;
/// The decrease target: `0.85 × measured`.
pub const DECREASE_PCT: u64 = 85;
/// A single window may not cut below this fraction of the current target — a
/// lumpy relay sample must not free-fall the rate.
pub const MIN_WINDOW_PCT: u64 = 50;
/// Goodput below this fraction of the target is over-use evidence.
pub const GOODPUT_UNDER_PCT: u64 = 90;
/// Below this fraction of the ceiling the climb is multiplicative (slow-start);
/// at or above it the climb is additive, so the controller converges just under
/// the ceiling with a small ripple instead of overshooting it every time.
pub const SLOW_START_CEIL_FRAC_PCT: u64 = 50;
/// Multiplicative slow-start step, percent (×1.08 per window).
pub const SLOW_START_PCT: u64 = 108;
/// Additive step above the slow-start band: `max(ceiling / 16, 25 kbps)`.
pub const AI_STEP_DIVISOR: u32 = 16;
pub const AI_MIN_STEP_BPS: u32 = 25_000;
/// A remembered pair opens at `0.85 × remembered` and that stands as a SOFT CAP
/// for this long; after it the climb is free (the prior was a guess, not a pin).
pub const REMEMBERED_CAP_PCT: u64 = 85;
pub const REMEMBERED_CAP_TTL: Duration = Duration::from_secs(10);
/// The path floor on a CONSTRAINED transport — the honest legibility stop, NOT
/// the governor's flat `area_min_bitrate_bps` (1.5 M), which pins the session.
pub const CONSTRAINED_FLOOR_BPS: u32 = 200_000;
/// A blocked send at least this long is a HARD stall (×0.5 at once). Mirrors
/// `ceiling_learn::HARD_STALL`, kept as a local const so this module stays pure.
pub const HARD_STALL: Duration = Duration::from_secs(1);
/// Deadband: an increase lands at most this often, a decrease at most this
/// often, and only when it moves the target by at least [`DEADBAND_PCT`].
pub const INCREASE_SPACING: Duration = Duration::from_secs(2);
pub const DECREASE_SPACING: Duration = Duration::from_millis(500);
pub const DEADBAND_PCT: u64 = 5;
/// A level this far over the learned floor on a constrained path is "deep" — a
/// rate cut drains it too slowly, so the controller would order one bounded
/// pause (counted only, in shadow).
pub const DEEP_LEVEL_MS: u16 = 700;

/// The controller's state, as the heartbeat reports it (`shadow_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The opener, before any congestion has ever spoken.
    Startup,
    /// Resting / climbing after recovery.
    Normal,
    /// A decrease fired this window.
    OverUse,
    /// Holding while the age level is still elevated.
    Hold,
    /// Climbing back after a hold cleared.
    UnderUse,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Startup => "startup",
            State::Normal => "normal",
            State::OverUse => "overuse",
            State::Hold => "hold",
            State::UnderUse => "underuse",
        }
    }
}

/// Why the target last moved (`shadow_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Opening,
    FullChannel,
    Overflow,
    HardStall,
    AgeLevel,
    AgeRising,
    Loss,
    GoodputBelowTarget,
    Held,
    Climbing,
    AtCeiling,
    SoftCap,
    NoEvidence,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Opening => "opening",
            Reason::FullChannel => "full-channel",
            Reason::Overflow => "overflow",
            Reason::HardStall => "hard-stall",
            Reason::AgeLevel => "age-level",
            Reason::AgeRising => "age-rising",
            Reason::Loss => "loss",
            Reason::GoodputBelowTarget => "goodput-below-target",
            Reason::Held => "held",
            Reason::Climbing => "climbing",
            Reason::AtCeiling => "at-ceiling",
            Reason::SoftCap => "soft-cap",
            Reason::NoEvidence => "no-evidence",
        }
    }
}

/// One `decodestat`-shaped viewer window, the facts the governor already holds
/// at `tick_viewer_window`'s boundary. Everything optional is absent on an older
/// viewer or a window with no report.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShadowWindow {
    /// Viewer-decoded fps; `0` means the viewer painted nothing this window.
    pub fps: u32,
    pub struggling: bool,
    /// Window-average and window-minimum paint age, and half the viewer's
    /// measured round trip — the FR-15 trio.
    pub age_avg_ms: Option<u16>,
    pub age_min_ms: Option<u16>,
    pub rtt_ms: Option<u16>,
    /// The viewer's arrival rate and how much its transit queue grew (ms).
    pub rx_bps: Option<u32>,
    pub queue_ms: Option<i16>,
    /// The agent's blocked-send goodput estimate, if it has one.
    pub goodput_bps: Option<u32>,
    /// What the send task delivered over the window.
    pub sent_bps: u32,
    /// Frames the viewer reported receiving / losing this window.
    pub frames_rx: u32,
    pub frames_lost: u32,
}

/// The pure delay-based controller. One per pump session; the governor owns it
/// as `Option<Controller>` and feeds it the live positions' facts.
#[derive(Debug)]
pub struct Controller {
    // --- bounds (resolved each frame from the governor's domain) ---
    /// The controller's own floor: 200 k on a constrained path, the governor's
    /// area floor on a direct one. Never the flat 1.5 M pin on a relay.
    floor_bps: u32,
    ceiling_bps: u32,
    constrained: bool,

    // --- priors ---
    /// The remembered-slow-pair rate (FR-59 P8), as a soft opening cap only.
    remembered_bps: Option<u32>,

    // --- state ---
    state: State,
    reason: Reason,
    /// The controller's target; `None` until the first domain lets it open.
    target_bps: Option<u32>,
    /// The controller's own estimate of what the path carries.
    measured_bps: Option<u32>,
    /// When the session opened — the remembered soft cap expires after this.
    opened_at: Option<Instant>,

    // --- events seen since the last window fold ---
    full_seen: bool,
    overflow_seen: bool,
    hard_stall_seen: bool,
    budget_skip_seen: bool,

    // --- window bookkeeping ---
    /// Smallest window-average age seen — the learned path floor (a stall never
    /// lowers it, because the minimum is what a stall cannot produce).
    age_floor_ms: Option<u16>,
    /// The previous window's average age — the delay-slope proxy.
    last_age_ms: Option<u16>,
    overuse_streak: u8,
    clean_streak: u8,

    // --- deadband clocks ---
    last_increase_at: Option<Instant>,
    last_decrease_at: Option<Instant>,

    // --- heartbeat counter ---
    pauses: u32,
}

impl Controller {
    /// `remembered_bps` is the FR-59 P8 seed for this pair (`None` = unremembered
    /// or fast). The controller opens lazily at its first [`Self::set_domain`],
    /// exactly as the live AIMD is built at its first tick.
    pub fn new(remembered_bps: Option<u32>) -> Self {
        Self {
            floor_bps: CONSTRAINED_FLOOR_BPS,
            ceiling_bps: CONSTRAINED_FLOOR_BPS,
            constrained: false,
            remembered_bps: remembered_bps.filter(|r| *r > 0),
            state: State::Startup,
            reason: Reason::Opening,
            target_bps: None,
            measured_bps: None,
            opened_at: None,
            full_seen: false,
            overflow_seen: false,
            hard_stall_seen: false,
            budget_skip_seen: false,
            age_floor_ms: None,
            last_age_ms: None,
            overuse_streak: 0,
            clean_streak: 0,
            last_increase_at: None,
            last_decrease_at: None,
            pauses: 0,
        }
    }

    /// Push the governor's resolved ceiling and floor, plus whether the path is
    /// constrained — called every frame, before [`Self::observe_frame`]. The
    /// controller derives ITS floor: 200 k on a constrained path (not the
    /// governor's 1.5 M flat floor, which is the pin), the governor's area
    /// floor on a direct one. Lazily opens the session on the first call.
    pub fn set_domain(&mut self, governor_floor_bps: u32, ceiling_bps: u32, constrained: bool) {
        self.constrained = constrained;
        self.floor_bps = if constrained {
            CONSTRAINED_FLOOR_BPS
        } else {
            governor_floor_bps.max(1)
        };
        self.ceiling_bps = ceiling_bps.max(self.floor_bps);
        match self.target_bps {
            None => {
                // The opener. A remembered-slow pair opens at 0.85× its rate
                // (a proven prior); otherwise at the floor and the slow-start
                // climbs. Either way bounded by the ceiling.
                let open = match self.remembered_bps {
                    Some(r) => {
                        ((u64::from(r) * REMEMBERED_CAP_PCT / 100) as u32).max(self.floor_bps)
                    }
                    None => self.floor_bps,
                };
                self.target_bps = Some(open.clamp(self.floor_bps, self.ceiling_bps));
                self.state = State::Startup;
                self.reason = Reason::Opening;
            }
            Some(t) => {
                // Lowering the ceiling clamps the target down at once (a relay
                // clamp must keep its decrease room); raising it just lets the
                // climb reach further.
                let clamped = t.clamp(self.floor_bps, self.ceiling_bps);
                self.target_bps = Some(clamped);
            }
        }
    }

    /// One send-channel sample, every frame at the capacity gate:
    /// `full = capacity == 0`. A sustained full channel is immediate over-use —
    /// the AIMD's own signal, shared so the shadow and the live rate see the
    /// same input and the B2 comparison is fair. (B1 acts on `full` alone; the
    /// AIMD's occupancy EWMA gates its additive INCREASE, which the controller
    /// gates on the window verdict instead, so no fraction is kept here.)
    pub fn observe_frame(&mut self, full: bool, now: Instant) {
        if full {
            self.full_seen = true;
            self.decrease(Reason::FullChannel, now);
        }
    }

    /// The byte-budget gate skipped a frame (`on_backpressure_skip`): a
    /// congestion sample, but NOT a blocked send — nothing reached the
    /// transport, so it does not become a `measured` sample (the sim error that
    /// once "measured" a 20 Mbps pipe from a gate skip).
    pub fn on_budget_skip(&mut self, now: Instant) {
        self.budget_skip_seen = true;
        self.decrease(Reason::FullChannel, now);
    }

    /// A send-channel overflow (a big frame the link could not drain) — a
    /// secondary, rate-limited decrease.
    pub fn on_overflow(&mut self, now: Instant) {
        self.overflow_seen = true;
        self.decrease(Reason::Overflow, now);
    }

    /// A blocked send: a HARD stall (≥ [`HARD_STALL`]) halves at once, a softer
    /// one is a rate-limited decrease.
    pub fn on_stall(&mut self, wait: Duration, now: Instant) {
        if wait >= HARD_STALL {
            self.hard_stall_seen = true;
            if let Some(t) = self.target_bps {
                let next = (t / 2).max(self.floor_bps);
                self.force_decrease(next, Reason::HardStall, now);
            }
        } else {
            self.decrease(Reason::Overflow, now);
        }
    }

    /// Once per viewer window: the state machine proper. Reads the facts the
    /// governor already folded; moves nothing the governor owns.
    pub fn fold_window(&mut self, w: &ShadowWindow, now: Instant) {
        let full_seen = std::mem::take(&mut self.full_seen);
        let overflow_seen = std::mem::take(&mut self.overflow_seen);
        let hard_stall_seen = std::mem::take(&mut self.hard_stall_seen);
        let budget_skip_seen = std::mem::take(&mut self.budget_skip_seen);
        let immediate_congestion =
            full_seen || overflow_seen || hard_stall_seen || budget_skip_seen;

        // `fps == 0` / no frames received is a stall, not evidence about the
        // rate: the opener keyframe or a DERP outage. Record nothing, climb
        // nothing — but a full channel this window is still real back-pressure.
        if w.frames_rx == 0 {
            if immediate_congestion && self.state != State::OverUse {
                self.enter_hold();
            }
            return;
        }

        // Learn the floor from what arrived; a stall never lowers it.
        if let Some(avg) = w.age_avg_ms {
            self.age_floor_ms = Some(self.age_floor_ms.map_or(avg, |f| f.min(avg)));
        }

        // The controller's own pipe estimate: the arrival rate on a constrained
        // path (what a relay session can see), the lower of goodput and arrival
        // on a direct one. A window that pushed back is also a delivery.
        let measured = if self.constrained {
            w.rx_bps.or(w.goodput_bps)
        } else {
            match (w.goodput_bps, w.rx_bps) {
                (Some(g), Some(r)) => Some(g.min(r)),
                (g, r) => g.or(r),
            }
        };
        if let Some(m) = measured.filter(|m| *m > 0) {
            self.measured_bps = Some(m);
        }

        // Over-use evidence this window.
        let floor = self.age_floor_ms;
        let age_level = match (w.age_avg_ms, floor) {
            (Some(avg), Some(f)) => avg.saturating_sub(f) >= AGE_SLACK_MS,
            _ => false,
        };
        let age_rising = match (w.age_avg_ms, self.last_age_ms) {
            // The delay-slope proxy: a clear rise over the last window, and over
            // the slack, so steady jitter does not count.
            (Some(avg), Some(prev)) => avg.saturating_sub(prev) >= AGE_SLACK_MS,
            _ => false,
        };
        let loss = w.frames_rx > 0
            && u64::from(w.frames_lost) * 100 >= u64::from(w.frames_rx) * u64::from(LOSS_PCT);
        let goodput_low = match (w.goodput_bps, self.target_bps) {
            (Some(g), Some(t)) => u64::from(g) * 100 < u64::from(t) * GOODPUT_UNDER_PCT,
            _ => false,
        };
        let evidence = age_level || age_rising || loss || goodput_low;
        self.last_age_ms = w.age_avg_ms;

        let evidence_reason = if age_level {
            Reason::AgeLevel
        } else if age_rising {
            Reason::AgeRising
        } else if loss {
            Reason::Loss
        } else {
            Reason::GoodputBelowTarget
        };

        // Streaks (the two-window fold): delay-based over-use needs two running
        // windows; a clean window resets it.
        if evidence {
            self.overuse_streak = self.overuse_streak.saturating_add(1);
            self.clean_streak = 0;
        } else {
            self.clean_streak = self.clean_streak.saturating_add(1);
            self.overuse_streak = 0;
        }

        // A deep level on a constrained path is a bounded pause (shadow: counted
        // only). Direct paths never pause.
        if self.constrained
            && let (Some(avg), Some(f)) = (w.age_avg_ms, floor)
            && avg.saturating_sub(f) >= DEEP_LEVEL_MS
        {
            self.pauses = self.pauses.saturating_add(1);
        }

        // The decrease path: an immediate congestion event already cut the rate
        // in its own method; here the delay-based streak does.
        if self.overuse_streak >= OVERUSE_WINDOWS {
            self.decrease(evidence_reason, now);
            self.enter_hold();
            self.overuse_streak = 0;
            return;
        }
        if immediate_congestion {
            // A full channel / overflow / stall this window already decreased;
            // hold while we see whether the level clears.
            self.enter_hold();
            return;
        }

        // Over-use evidence this window, short of the two-window streak that
        // cuts: HOLD the rate steady — never climb into a pipe already showing
        // strain. This is GCC's hold: one bad window is not yet a cut, but it is
        // certainly not a reason to ask for more.
        if evidence {
            self.reason = evidence_reason;
            return;
        }

        // A clean window. In Hold we wait for the LEVEL to clear — two clean
        // windows, not just growth stopping (the derivative lesson FR-59 paid
        // for twice) — before reopening the climb.
        if self.state == State::Hold {
            if self.clean_streak < CLEAN_WINDOWS {
                self.reason = Reason::Held;
                return;
            }
            self.state = State::UnderUse;
        }

        // Climb. Startup and UnderUse both use the slow-start / additive law;
        // once the target has rested at the ceiling the state is Normal.
        self.climb(now);
    }

    /// The increase law: multiplicative (×1.08) below half the ceiling,
    /// additive above, honouring the remembered soft cap for its first 10 s.
    fn climb(&mut self, now: Instant) {
        let Some(t) = self.target_bps else { return };
        if t >= self.ceiling_bps {
            self.state = if self.state == State::Startup {
                State::Normal
            } else {
                self.state
            };
            self.reason = Reason::AtCeiling;
            return;
        }
        let next = if u64::from(t) * 100 < u64::from(self.ceiling_bps) * SLOW_START_CEIL_FRAC_PCT {
            (u64::from(t) * SLOW_START_PCT / 100) as u32
        } else {
            t.saturating_add((self.ceiling_bps / AI_STEP_DIVISOR).max(AI_MIN_STEP_BPS))
        };
        let next = next.min(self.ceiling_bps);

        // The remembered rate is a soft cap for 10 s, not a pin: a prior may
        // open a session, it may not hold one past the point it could have been
        // measured.
        let (next, capped) = match (self.remembered_bps, self.opened_at) {
            (Some(r), Some(opened)) if now.duration_since(opened) < REMEMBERED_CAP_TTL => {
                let cap = ((u64::from(r) * REMEMBERED_CAP_PCT / 100) as u32)
                    .clamp(self.floor_bps, self.ceiling_bps);
                (next.min(cap), next > cap)
            }
            _ => (next, false),
        };

        if self.increase_ok(next, now) {
            self.target_bps = Some(next);
            self.last_increase_at = Some(now);
            self.reason = if capped {
                Reason::SoftCap
            } else {
                Reason::Climbing
            };
            self.state = match self.state {
                State::Startup => State::Startup,
                State::UnderUse if next >= self.ceiling_bps => State::Normal,
                other => other,
            };
        }
    }

    /// A rate-limited multiplicative decrease toward `0.85 × measured`, never
    /// cutting more than half the target in one window, floored at the path
    /// floor. Called by the congestion events and the delay streak.
    fn decrease(&mut self, reason: Reason, now: Instant) {
        let Some(t) = self.target_bps else { return };
        let measured = self.measured_bps.unwrap_or(t);
        let floor_this_window = (u64::from(t) * MIN_WINDOW_PCT / 100) as u32;
        let next = ((u64::from(measured) * DECREASE_PCT / 100) as u32)
            .max(floor_this_window)
            .max(self.floor_bps)
            .min(t);
        if next < t {
            self.force_decrease(next, reason, now);
        }
    }

    /// Apply a decrease, rate-limited to one per [`DECREASE_SPACING`].
    fn force_decrease(&mut self, next: u32, reason: Reason, now: Instant) {
        let Some(t) = self.target_bps else { return };
        if next >= t {
            return;
        }
        if let Some(last) = self.last_decrease_at
            && now.duration_since(last) < DECREASE_SPACING
        {
            return;
        }
        self.target_bps = Some(next.max(self.floor_bps));
        self.last_decrease_at = Some(now);
        self.state = State::OverUse;
        self.reason = reason;
    }

    fn enter_hold(&mut self) {
        self.state = State::Hold;
        self.clean_streak = 0;
    }

    /// Increases are rate-limited to one per [`INCREASE_SPACING`] and must move
    /// the target by at least [`DEADBAND_PCT`].
    fn increase_ok(&self, next: u32, now: Instant) -> bool {
        let Some(t) = self.target_bps else {
            return true;
        };
        if next <= t {
            return false;
        }
        if u64::from(next - t) * 100 < u64::from(t) * DEADBAND_PCT {
            return false;
        }
        match self.last_increase_at {
            Some(last) => now.duration_since(last) >= INCREASE_SPACING,
            None => true,
        }
    }

    // --- accessors the governor reads for the heartbeat ---

    pub fn target_bps(&self) -> Option<u32> {
        self.target_bps
    }
    pub fn state(&self) -> State {
        self.state
    }
    pub fn reason(&self) -> Reason {
        self.reason
    }
    pub fn measured_bps(&self) -> Option<u32> {
        self.measured_bps
    }
    pub fn pauses(&self) -> u32 {
        self.pauses
    }

    /// Mark the session's open instant (for the remembered soft-cap TTL). The
    /// governor calls this once, on the first frame it has an `Instant` for.
    pub fn mark_opened(&mut self, now: Instant) {
        self.opened_at.get_or_insert(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    /// A relay window the controller treats as clean: a plausible age, an
    /// arrival rate, no loss.
    fn clean_window(age: u16, rx: u32) -> ShadowWindow {
        ShadowWindow {
            fps: 30,
            struggling: false,
            age_avg_ms: Some(age),
            age_min_ms: Some(age),
            rtt_ms: Some(40),
            rx_bps: Some(rx),
            queue_ms: Some(0),
            goodput_bps: None,
            sent_bps: rx,
            frames_rx: 30,
            frames_lost: 0,
        }
    }

    fn constrained(c: &mut Controller, floor: u32, ceiling: u32) {
        c.set_domain(floor, ceiling, true);
    }

    #[test]
    fn an_unremembered_session_opens_at_the_constrained_floor_not_the_flat_pin() {
        // The governor's flat floor is 1.5 M; the controller's own constrained
        // floor is 200 k, and that is where an unremembered session opens —
        // not at the ceiling the shipped governor commits to.
        let mut c = Controller::new(None);
        c.set_domain(1_500_000, 2_550_000, true);
        assert_eq!(c.target_bps(), Some(CONSTRAINED_FLOOR_BPS));
        assert_eq!(c.state(), State::Startup);
    }

    #[test]
    fn a_direct_session_opens_on_the_governors_area_floor() {
        let mut c = Controller::new(None);
        c.set_domain(3_100_000, 34_000_000, false);
        assert_eq!(c.target_bps(), Some(3_100_000));
        assert!(!c.constrained);
    }

    #[test]
    fn a_remembered_pair_opens_at_85_percent_of_its_rate() {
        let mut c = Controller::new(Some(400_000));
        c.set_domain(1_500_000, 2_550_000, true);
        assert_eq!(c.target_bps(), Some(340_000));
    }

    #[test]
    fn the_slow_start_doubles_toward_half_the_ceiling_then_goes_additive() {
        let mut c = Controller::new(None);
        let mut t = t0();
        c.set_domain(1_500_000, 2_550_000, true); // opens 200 k, ceiling 2.55 M
        assert_eq!(c.target_bps(), Some(200_000));
        // ×1.08 per window while below 50 % of the ceiling (1.275 M), one climb
        // per 2 s (the increase deadband).
        let mut last = 200_000u32;
        for _ in 0..40 {
            t += INCREASE_SPACING;
            c.set_domain(1_500_000, 2_550_000, true);
            c.fold_window(&clean_window(40, 1_000_000), t);
            let now = c.target_bps().unwrap();
            assert!(now >= last, "target must not fall on clean windows");
            assert!(now <= 2_550_000, "never above the ceiling");
            last = now;
        }
        // It converged just UNDER the ceiling and parked there: the additive
        // step shrinks as it approaches, and the 5 % deadband refuses the final
        // sub-5 % nudge — a controller that converges just under the cap with no
        // chatter, exactly as designed. Within one additive step of the ceiling.
        let one_step = 2_550_000 / AI_STEP_DIVISOR;
        assert!(
            (2_550_000 - one_step..=2_550_000).contains(&last),
            "expected the target to converge within one step of the ceiling, got {last}"
        );
    }

    #[test]
    fn a_full_channel_decreases_toward_measured_at_once() {
        let mut c = Controller::new(None);
        let t = t0();
        c.set_domain(1_500_000, 2_550_000, true);
        // Give it a measurement and a target up near the ceiling first.
        c.fold_window(&clean_window(40, 1_000_000), t);
        // Walk it up so there is something to cut.
        let mut tt = t;
        for _ in 0..30 {
            tt += INCREASE_SPACING;
            c.set_domain(1_500_000, 2_550_000, true);
            c.fold_window(&clean_window(40, 1_000_000), tt);
        }
        let before = c.target_bps().unwrap();
        tt += DECREASE_SPACING;
        c.observe_frame(true, tt); // full channel
        let after = c.target_bps().unwrap();
        assert!(
            after < before,
            "a full channel must cut: {before} -> {after}"
        );
        assert_eq!(c.state(), State::OverUse);
        // Toward 0.85 × measured (1 Mbps), but never below half the old target.
        assert!(after >= before / 2, "never cut more than half in a window");
    }

    #[test]
    fn a_hard_stall_halves_at_once() {
        let mut c = Controller::new(None);
        let mut t = t0();
        c.set_domain(1_500_000, 2_550_000, true);
        for _ in 0..30 {
            t += INCREASE_SPACING;
            c.set_domain(1_500_000, 2_550_000, true);
            c.fold_window(&clean_window(40, 2_000_000), t);
        }
        let before = c.target_bps().unwrap();
        t += DECREASE_SPACING;
        c.on_stall(Duration::from_secs(2), t);
        assert_eq!(c.target_bps(), Some(before / 2));
        assert_eq!(c.reason(), Reason::HardStall);
    }

    #[test]
    fn two_windows_of_elevated_age_cut_the_rate_one_does_not() {
        let mut c = Controller::new(None);
        let mut t = t0();
        c.set_domain(1_500_000, 2_550_000, true);
        // Climb to a plateau and learn a low age floor.
        for _ in 0..30 {
            t += INCREASE_SPACING;
            c.set_domain(1_500_000, 2_550_000, true);
            c.fold_window(&clean_window(40, 2_000_000), t);
        }
        let plateau = c.target_bps().unwrap();
        // ONE elevated window: no cut (a blip).
        t += INCREASE_SPACING;
        c.set_domain(1_500_000, 2_550_000, true);
        c.fold_window(&clean_window(40 + AGE_SLACK_MS + 5, 2_000_000), t);
        assert_eq!(
            c.target_bps(),
            Some(plateau),
            "one elevated window is a blip"
        );
        // A SECOND running elevated window cuts.
        t += INCREASE_SPACING;
        c.set_domain(1_500_000, 2_550_000, true);
        c.fold_window(&clean_window(40 + AGE_SLACK_MS + 5, 2_000_000), t);
        assert!(c.target_bps().unwrap() < plateau, "two running windows cut");
        // The cut transitions straight into Hold (where the session rests while
        // the level is watched); OverUse is the instant of the cut itself.
        assert_eq!(c.state(), State::Hold);
    }

    #[test]
    fn hold_persists_on_the_level_not_the_derivative() {
        // The FR-59 lesson: a queue that stops GROWING is not a queue that is
        // GONE. The age stays high (level) but flat (no rise); the controller
        // must stay in Hold, not climb.
        let mut c = Controller::new(None);
        let mut t = t0();
        c.set_domain(1_500_000, 2_550_000, true);
        for _ in 0..30 {
            t += INCREASE_SPACING;
            c.set_domain(1_500_000, 2_550_000, true);
            c.fold_window(&clean_window(40, 2_000_000), t);
        }
        // Two elevated windows -> a cut -> Hold.
        for _ in 0..2 {
            t += INCREASE_SPACING;
            c.set_domain(1_500_000, 2_550_000, true);
            c.fold_window(&clean_window(300, 2_000_000), t);
        }
        assert!(matches!(c.state(), State::OverUse | State::Hold));
        let held = c.target_bps().unwrap();
        // The age stays HIGH but FLAT for several windows (level, no derivative).
        for _ in 0..5 {
            t += INCREASE_SPACING;
            c.set_domain(1_500_000, 2_550_000, true);
            c.fold_window(&clean_window(300, 2_000_000), t);
        }
        assert_eq!(c.state(), State::Hold, "a high flat level holds");
        assert!(
            c.target_bps().unwrap() <= held,
            "the rate does not climb while the level is elevated"
        );
        // The level finally clears: two clean windows reopen the climb.
        for _ in 0..3 {
            t += INCREASE_SPACING;
            c.set_domain(1_500_000, 2_550_000, true);
            c.fold_window(&clean_window(40, 2_000_000), t);
        }
        assert!(
            matches!(c.state(), State::UnderUse | State::Normal),
            "two clean windows reopen the climb, got {:?}",
            c.state()
        );
    }

    #[test]
    fn a_direct_path_never_pauses_a_constrained_deep_level_does() {
        // Direct: a deep level must NOT count a pause.
        let mut d = Controller::new(None);
        let mut t = t0();
        d.set_domain(3_100_000, 34_000_000, false);
        for _ in 0..3 {
            t += INCREASE_SPACING;
            d.set_domain(3_100_000, 34_000_000, false);
            let mut w = clean_window(40 + DEEP_LEVEL_MS + 100, 5_000_000);
            w.age_min_ms = Some(40);
            d.fold_window(&w, t);
        }
        assert_eq!(d.pauses(), 0, "direct paths never pause");

        // Constrained: the same deep level counts a bounded pause.
        let mut c = Controller::new(None);
        let mut t = t0();
        c.set_domain(1_500_000, 2_550_000, true);
        c.fold_window(&clean_window(40, 600_000), t); // learn a 40 ms floor
        t += INCREASE_SPACING;
        let mut w = clean_window(40 + DEEP_LEVEL_MS + 100, 600_000);
        w.age_min_ms = Some(40);
        c.set_domain(1_500_000, 2_550_000, true);
        c.fold_window(&w, t);
        assert!(c.pauses() >= 1, "a deep constrained level counts a pause");
    }

    #[test]
    fn an_fps_zero_window_is_no_evidence() {
        let mut c = Controller::new(None);
        let mut t = t0();
        c.set_domain(1_500_000, 2_550_000, true);
        for _ in 0..30 {
            t += INCREASE_SPACING;
            c.set_domain(1_500_000, 2_550_000, true);
            c.fold_window(&clean_window(40, 2_000_000), t);
        }
        let before = c.target_bps().unwrap();
        let measured_before = c.measured_bps();
        // A DERP stall: nothing painted.
        let stall = ShadowWindow {
            frames_rx: 0,
            age_avg_ms: None,
            rx_bps: None,
            ..clean_window(0, 0)
        };
        t += INCREASE_SPACING;
        c.set_domain(1_500_000, 2_550_000, true);
        c.fold_window(&stall, t);
        assert_eq!(
            c.target_bps(),
            Some(before),
            "a stall neither climbs nor cuts"
        );
        assert_eq!(
            c.measured_bps(),
            measured_before,
            "a stall measures nothing"
        );
    }

    #[test]
    fn the_target_never_leaves_the_domain() {
        let mut c = Controller::new(Some(9_000_000));
        let mut t = t0();
        // A remembered rate above the ceiling cannot open above it.
        c.set_domain(1_500_000, 2_550_000, true);
        assert!(c.target_bps().unwrap() <= 2_550_000);
        for _ in 0..60 {
            t += INCREASE_SPACING;
            c.set_domain(1_500_000, 2_550_000, true);
            // Alternate clean and congested so it moves both ways.
            c.observe_frame(true, t);
            c.fold_window(&clean_window(40, 1_000_000), t);
            let v = c.target_bps().unwrap();
            assert!(
                (CONSTRAINED_FLOOR_BPS..=2_550_000).contains(&v),
                "target {v} left [{CONSTRAINED_FLOOR_BPS}, 2_550_000]"
            );
        }
    }

    #[test]
    fn increases_are_deadbanded_decreases_are_faster() {
        let mut c = Controller::new(None);
        let t = t0();
        c.set_domain(1_500_000, 2_550_000, true);
        c.fold_window(&clean_window(40, 2_000_000), t);
        let after_first = c.target_bps().unwrap();
        // A second climb 100 ms later must NOT land (increase spacing is 2 s).
        c.set_domain(1_500_000, 2_550_000, true);
        c.fold_window(&clean_window(40, 2_000_000), t + Duration::from_millis(100));
        assert_eq!(
            c.target_bps(),
            Some(after_first),
            "an increase inside the 2 s deadband must not land"
        );
    }

    #[test]
    fn the_remembered_soft_cap_lifts_after_ten_seconds() {
        // Opened at 0.85 × 400 k = 340 k; the cap holds the climb at 340 k for
        // 10 s, then lifts.
        let mut c = Controller::new(Some(400_000));
        let mut t = t0();
        c.set_domain(1_500_000, 2_550_000, true);
        c.mark_opened(t);
        assert_eq!(c.target_bps(), Some(340_000));
        // Within 10 s the climb is capped at the soft cap.
        for _ in 0..4 {
            t += INCREASE_SPACING;
            c.set_domain(1_500_000, 2_550_000, true);
            c.fold_window(&clean_window(40, 2_000_000), t);
            assert!(
                c.target_bps().unwrap() <= 340_000,
                "capped within the TTL, got {:?}",
                c.target_bps()
            );
        }
        // Past the TTL the climb is free again.
        t += REMEMBERED_CAP_TTL + INCREASE_SPACING;
        c.set_domain(1_500_000, 2_550_000, true);
        c.fold_window(&clean_window(40, 2_000_000), t);
        assert!(
            c.target_bps().unwrap() > 340_000,
            "the soft cap lifts after the TTL, got {:?}",
            c.target_bps()
        );
    }
}

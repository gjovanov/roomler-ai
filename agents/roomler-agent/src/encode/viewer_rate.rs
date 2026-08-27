//! Viewer-reported sustainable-rate controller for the DataChannel video pumps.
//!
//! Replaces the rc.184 keyframe-request-*rate* `DecodePressure` heuristic. That
//! design inferred viewer distress from how OFTEN the browser sent `rc:keyframe`
//! resync requests — but the browser debounces those to ~4/s (250 ms) while the
//! shed needed ≥4/s to escalate, so the two never coordinated: on a weak viewer
//! (Iris Xe) the agent kept firehosing 60 fps, the viewer's WebCodecs decode
//! queue backed up, it dropped deltas + asked for a (heavy) IDR, which was even
//! HARDER to decode → the periodic 1-2 s freeze the field reported on dragging a
//! window. An RTX-5090 viewer of the SAME host never stuttered — pure
//! viewer-decode binding, not capture/encode.
//!
//! Now the VIEWER measures its own decoded fps + whether it dropped frames to a
//! backlog this window and sends `{fps, struggling}` over the control DC
//! (`rc:decodestat`). This controller folds that DIRECT, measured signal into an
//! fps cap. When struggling, it clamps the cap to just below what the viewer
//! actually sustained, so the agent immediately sends fewer frames; after a run
//! of clean windows it probes the cap lazily back toward the capture rate (so a
//! transient dip recovers, but a viewer sitting just under its ceiling doesn't
//! oscillate).
//!
//! The pump converts the cap into the existing frame-skip divisor
//! (`ceil(capture_fps / cap_fps)`, keyframes never skipped), so the agent
//! SETTLES at the viewer's real sustainable fps. During a sustained window-drag
//! the viewer struggles every window → the cap holds → smooth reduced fps;
//! motion stops → it recovers. Because the divisor quantises (caps 31..60 all
//! map to 30 fps until the cap reaches capture_fps exactly), active use naturally
//! parks at the reduced rate and only climbs back to full fps after a long idle.
//!
//! Pure (no webrtc / capture / ffmpeg types) → unit-tests on the default
//! `cargo test --lib`. The pump features are what USE it, hence the dead_code
//! allow on the signalling-only build (mirrors `aimd` / `encode_pressure`).

use tunnel_core::env::node_env;

fn env_u32(suffix: &str, default: u32) -> u32 {
    node_env(suffix)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Lowest fps the controller will cap down to. Below this, motion is a
/// slideshow; deeper relief is the (manual) resolution lever's job, not more
/// fps shedding. Env `ROOMLER_AGENT_VIEWER_RATE_MIN_FPS` (default 12).
fn min_fps() -> u32 {
    env_u32("VIEWER_RATE_MIN_FPS", 12).max(1)
}

/// fps step per adjustment — down on struggle, and (unless overridden) up on
/// recovery. Env `ROOMLER_AGENT_VIEWER_RATE_STEP` (default 10).
fn fps_step() -> u32 {
    env_u32("VIEWER_RATE_STEP", 10).max(1)
}

/// P6 — separate fps step for the recovery climb, so burst-recovery is fast
/// without coarsening the shed clamp. Env `ROOMLER_AGENT_VIEWER_RATE_STEP_UP`;
/// defaults to 2× the down-step (P6 field bake 2026-07-26 — with recover=3
/// this recovers a shallow clamp in one ~3 s probe and a deep clamp in ~9 s;
/// the shed still steps down by the small `step`).
fn fps_step_up(default_step: u32) -> u32 {
    env_u32("VIEWER_RATE_STEP_UP", default_step.saturating_mul(2)).max(1)
}

/// Consecutive clean windows before the cap probes back UP one step. Lazy so a
/// viewer parked just under its ceiling doesn't oscillate every window.
/// Env `ROOMLER_AGENT_VIEWER_RATE_RECOVER` (default 3 — P6 field bake
/// 2026-07-26: the old 6 measured 15.5 s deep-clamp recovery on the canonical
/// pair (cap 12 → divisor 1, probes every 3.1 s, model-exact); 3 plus the 2×
/// up-step brings the common shallow clamp to ≤3 s and a deep clamp to ~9 s.
/// The viewer's P6 sustained-window struggle rule (2 consecutive bad windows)
/// damps the ceiling-oscillation risk that originally motivated 6).
fn recover_windows() -> u32 {
    env_u32("VIEWER_RATE_RECOVER", 3).max(1)
}

/// Slow-start recovery climb (shelf item, 2026-07-27). Only the FIRST probe
/// after a struggle is lazy (`recover` clean windows — confirmation the
/// struggle really ended); once climbing, every further clean window probes
/// again and each probe takes the larger of `step_up` and HALF THE REMAINING
/// GAP to the capture rate. Deep clamps recover in a handful of windows
/// (12→36→56→60 at 60 capture: ~5 s where the additive climb took ~9 s)
/// while the ceiling-parked oscillation guard is preserved — a re-struggle
/// drops out of slow-start and the next climb needs full confirmation again.
/// Env `ROOMLER_AGENT_VIEWER_RATE_SLOW_START=0` restores the pure additive
/// climb.
fn slow_start_enabled() -> bool {
    !matches!(
        node_env("VIEWER_RATE_SLOW_START").as_deref(),
        Some("0") | Some("false")
    )
}

/// Turns a stream of viewer decode reports into a send-fps cap for one DC pump.
/// Step it once per ~1 s observation window with `(reported_fps, struggling)`.
pub struct ViewerRateController {
    /// Current agreed send-fps cap. Starts at the capture rate (no cap).
    cap_fps: u32,
    /// The pump's capture target — the ceiling the cap can never exceed.
    capture_fps: u32,
    clean_streak: u32,
    min_fps: u32,
    step: u32,
    /// P6 — recovery climb step (defaults to `step`; env-tunable separately).
    step_up: u32,
    recover: u32,
    /// Slow-start state: true while a recovery climb is in progress (between
    /// the first post-struggle probe and reaching the capture rate). While
    /// climbing, probes fire every clean window instead of every `recover`.
    climbing: bool,
    /// Env kill for the slow-start climb (`VIEWER_RATE_SLOW_START=0`).
    slow_start: bool,
    enabled: bool,
}

impl ViewerRateController {
    pub fn new(capture_fps: u32) -> Self {
        let capture_fps = capture_fps.max(1);
        let step = fps_step();
        Self {
            cap_fps: capture_fps,
            capture_fps,
            clean_streak: 0,
            min_fps: min_fps().min(capture_fps),
            step,
            step_up: fps_step_up(step),
            recover: recover_windows(),
            climbing: false,
            slow_start: slow_start_enabled(),
            // Kill switch — default ON; `ROOMLER_AGENT_VIEWER_RATE=0` (or
            // `false`) pins the cap at the capture rate (divisor 1, no shedding)
            // so a misbehaving field host reverts without a rebuild.
            enabled: !matches!(
                node_env("VIEWER_RATE").as_deref(),
                Some("0") | Some("false")
            ),
        }
    }

    /// Fold one viewer report into the cap and return the frame-skip divisor the
    /// pump should apply. `reported_fps` = frames the viewer DECODED last window
    /// (0 if it sent no useful number); `struggling` = it dropped frames to a
    /// decode backlog (or its queue was backing up). `capture_fps` is passed each
    /// call so a mid-session capture-rate change (e.g. the SW auto-cap) re-seeds
    /// the ceiling.
    pub fn observe(&mut self, reported_fps: u32, struggling: bool, capture_fps: u32) -> u32 {
        self.capture_fps = capture_fps.max(1);
        self.min_fps = self.min_fps.min(self.capture_fps);
        // Keep the cap within the (possibly changed) bounds before deciding.
        self.cap_fps = self.cap_fps.clamp(self.min_fps, self.capture_fps);
        if !self.enabled {
            self.cap_fps = self.capture_fps;
            return 1;
        }
        if struggling {
            // Clamp to just below what the viewer actually managed. A nonsense
            // (0) report falls back to stepping the current cap down. Never
            // below min_fps, never above capture_fps.
            let managed = if reported_fps > 0 {
                reported_fps
            } else {
                self.cap_fps
            };
            let target = managed.min(self.cap_fps).saturating_sub(self.step);
            self.cap_fps = target.clamp(self.min_fps, self.capture_fps);
            self.clean_streak = 0;
            // A re-struggle ends any climb: the next one needs full
            // confirmation (`recover` clean windows) again.
            self.climbing = false;
        } else {
            self.clean_streak += 1;
            // Slow-start: only the FIRST probe after a struggle is lazy;
            // while climbing, every clean window probes again.
            let need = if self.climbing && self.slow_start {
                1
            } else {
                self.recover
            };
            if self.clean_streak >= need {
                let up = if self.slow_start {
                    // Half the remaining gap, floored at step_up, so deep
                    // clamps close fast and the climb still terminates.
                    self.step_up
                        .max(self.capture_fps.saturating_sub(self.cap_fps) / 2)
                } else {
                    self.step_up
                };
                self.cap_fps = (self.cap_fps + up).min(self.capture_fps);
                self.clean_streak = 0;
                self.climbing = self.cap_fps < self.capture_fps;
            }
        }
        self.divisor()
    }

    /// `ceil(capture_fps / cap_fps)`, clamped ≥ 1. Ceil (not round) guarantees
    /// the effective fps (`capture_fps / divisor`) never EXCEEDS the cap, so we
    /// stay at-or-under what the viewer said it can take.
    pub fn divisor(&self) -> u32 {
        let cap = self.cap_fps.max(1);
        self.capture_fps.div_ceil(cap).max(1)
    }

    /// Current cap, for the heartbeat log.
    pub fn cap_fps(&self) -> u32 {
        self.cap_fps
    }
}

/// Bit set in the packed report atomic when the viewer flagged a decode backlog
/// this window. The low 16 bits carry the reported decoded fps.
pub const STRUGGLE_BIT: u32 = 1 << 16;

/// Pack a viewer decode report into the shared atomic the control handler writes
/// and the pumps read. `fps` is clamped to 16 bits (ample for any real rate).
pub fn pack_report(fps: u32, struggling: bool) -> u32 {
    fps.min(0xFFFF) | if struggling { STRUGGLE_BIT } else { 0 }
}

/// Inverse of [`pack_report`]. The `0` swap-reset value (no report this window)
/// decodes to `(0, false)` — a clean window, which the controller treats as a
/// recovery tick.
pub fn unpack_report(raw: u32) -> (u32, bool) {
    (raw & 0xFFFF, raw & STRUGGLE_BIT != 0)
}

// ── FR-15 — viewer paint-age feedback ───────────────────────────────────────
// The FR-1 P7 age pill measures true end-to-end frame age at the viewer; the
// viewer piggybacks each window's avg + min onto `rc:decodestat`. On a
// CONSTRAINED transport that age is the only sensor that sees the whole path:
// the 2026-08-27 field session showed a 1000 ms age against a 26 KB agent
// queue — the backlog lives in the WG-over-DERP/TCP legs, below every agent
// counter. The loop here turns sustained age growth into the same responses
// the rest of the stack already knows: an fps cap (instant, no re-open) and a
// multiplicative decrease (applied under the FR-10 quiet/spacing rules).

/// Pack a viewer age report (window avg + window min, ms) into the shared
/// `viewer_age` atomic. `0` is the swap-reset / no-report sentinel, so a
/// genuine measurement is floored at 1 ms — sub-ms end-to-end age is not
/// physically possible, so nothing real is lost.
pub fn pack_age(age_ms: u16, age_min_ms: u16) -> u32 {
    (age_ms.max(1) as u32) | ((age_min_ms.max(1) as u32) << 16)
}

/// Inverse of [`pack_age`]; `0` (no report this window) → `None`.
pub fn unpack_age(raw: u32) -> Option<(u16, u16)> {
    if raw == 0 {
        return None;
    }
    Some(((raw & 0xFFFF) as u16, (raw >> 16) as u16))
}

/// Everything one viewer tells the agent about how the stream is actually
/// landing: the rc.188 decode report (fps + struggling) and the FR-15 paint
/// age. ONE shared cell per session — the control handler writes, the DC
/// pump swaps once a viewer window, and a follower's is read by the shared
/// pipeline's fold. Both slots use `0` as "nothing reported this window",
/// so a viewer that goes quiet decays to no-signal instead of pinning a
/// stale value.
#[derive(Debug, Default)]
pub struct ViewerFeedback {
    report: std::sync::atomic::AtomicU32,
    age: std::sync::atomic::AtomicU32,
}

impl ViewerFeedback {
    pub fn new() -> Self {
        Self::default()
    }

    /// Overwrite the decode report (see [`pack_report`]).
    pub fn store_report(&self, packed: u32) {
        self.report
            .store(packed, std::sync::atomic::Ordering::Relaxed);
    }

    /// Consume the decode report, leaving "no signal" behind.
    pub fn take_report(&self) -> u32 {
        self.report.swap(0, std::sync::atomic::Ordering::Relaxed)
    }

    /// The reported fps WITHOUT consuming the report — session telemetry
    /// reads what the user saw; the rate loop is the only consumer that
    /// may take it.
    pub fn peek_fps(&self) -> u32 {
        self.report.load(std::sync::atomic::Ordering::Relaxed) & 0xFFFF
    }

    /// FR-15 — overwrite the paint-age report (see [`pack_age`]).
    pub fn store_age(&self, packed: u32) {
        self.age.store(packed, std::sync::atomic::Ordering::Relaxed);
    }

    /// FR-15 — consume the paint-age report.
    pub fn take_age(&self) -> u32 {
        self.age.swap(0, std::sync::atomic::Ordering::Relaxed)
    }
}

/// Age excess over the learned floor that counts a window as over-rate.
/// Below this the climb is within jitter noise; the field baseline that
/// motivated the loop was +60 ms felt as sluggish on an 85 ms-RTT relay.
pub const AGE_SLACK_MS: u16 = 70;
/// Consecutive over-rate windows before the loop reacts (mirrors the
/// viewer-side struggle fold: one bad window is a blip, not a trend).
const AGE_OVER_WINDOWS: u8 = 2;
/// Windows of floor memory (~30 s at the 1 s viewer window): long enough to
/// hold the floor through a sustained drag, short enough that a genuine
/// path change (VPN re-route raising the floor) re-baselines in half a
/// minute instead of over-triggering forever.
const AGE_FLOOR_RING: usize = 30;

/// FR-15 — the constrained-transport age loop. Feed one `observe` per
/// viewer window; it learns the session's age floor (min over the ring —
/// the floor is propagation+decode, not queue) and reports `true` once the
/// excess has persisted [`AGE_OVER_WINDOWS`] consecutive windows, staying
/// `true` each window while the overload lasts (the caller's MD is
/// rate-limited internally, so "true every window" is exactly the intended
/// ×0.85-per-window pressure).
pub struct AgeLoop {
    ring: [u16; AGE_FLOOR_RING],
    len: usize,
    idx: usize,
    over_streak: u8,
}

impl Default for AgeLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl AgeLoop {
    pub fn new() -> Self {
        Self {
            ring: [0; AGE_FLOOR_RING],
            len: 0,
            idx: 0,
            over_streak: 0,
        }
    }

    /// Fold one viewer window. `age_ms` = window average, `age_min_ms` =
    /// window minimum (the floor sample — even a queued window usually
    /// contains one frame that rode a momentarily drained pipe).
    pub fn observe(&mut self, age_ms: u16, age_min_ms: u16) -> bool {
        self.ring[self.idx] = age_min_ms.max(1);
        self.idx = (self.idx + 1) % AGE_FLOOR_RING;
        if self.len < AGE_FLOOR_RING {
            self.len += 1;
        }
        let floor = self.floor_ms().unwrap_or(age_min_ms.max(1));
        let over = age_ms.saturating_sub(floor) >= AGE_SLACK_MS;
        self.over_streak = if over {
            self.over_streak.saturating_add(1)
        } else {
            0
        };
        self.over_streak >= AGE_OVER_WINDOWS
    }

    /// The learned path floor (min of the ring), None before any report.
    pub fn floor_ms(&self) -> Option<u16> {
        self.ring[..self.len].iter().copied().min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deterministic controller regardless of ambient env. `slow_start` is
    // pinned OFF here so these tests keep locking the pure additive climb;
    // the slow-start behaviour has its own fixture + tests below.
    fn ctrl(capture_fps: u32) -> ViewerRateController {
        ViewerRateController {
            cap_fps: capture_fps,
            capture_fps,
            clean_streak: 0,
            min_fps: 12,
            step: 10,
            step_up: 10,
            recover: 6,
            climbing: false,
            slow_start: false,
            enabled: true,
        }
    }

    // Slow-start fixture at the shipped defaults (recover 3, step_up 2×step).
    fn ctrl_ss(capture_fps: u32) -> ViewerRateController {
        ViewerRateController {
            cap_fps: capture_fps,
            capture_fps,
            clean_streak: 0,
            min_fps: 12,
            step: 10,
            step_up: 20,
            recover: 3,
            climbing: false,
            slow_start: true,
            enabled: true,
        }
    }

    #[test]
    fn no_struggle_stays_at_full_rate() {
        let mut c = ctrl(60);
        // A clean window keeps the cap at capture → divisor 1 (no skip).
        assert_eq!(c.observe(60, false, 60), 1);
        assert_eq!(c.cap_fps(), 60);
    }

    #[test]
    fn struggle_caps_below_managed_and_raises_divisor() {
        let mut c = ctrl(60);
        // Viewer was sent 60 but only decoded 35 and dropped frames → cap to
        // 35-10=25 → ceil(60/25)=3 (20 fps, safely under 25).
        let div = c.observe(35, true, 60);
        assert_eq!(c.cap_fps(), 25);
        assert_eq!(div, 3);
        assert!(60 / div <= c.cap_fps(), "effective fps must not exceed cap");
    }

    #[test]
    fn zero_report_while_struggling_steps_current_cap_down() {
        let mut c = ctrl(60);
        // No usable fps number but struggling → step the current cap (60) down.
        let div = c.observe(0, true, 60);
        assert_eq!(c.cap_fps(), 50);
        assert_eq!(div, 2); // ceil(60/50) = 2 → 30 fps
    }

    #[test]
    fn cap_floors_at_min_fps() {
        let mut c = ctrl(60);
        for _ in 0..20 {
            c.observe(5, true, 60);
        }
        assert_eq!(c.cap_fps(), 12, "never drops below min_fps");
        assert_eq!(c.divisor(), 5); // ceil(60/12)
    }

    #[test]
    fn recovery_probes_up_only_after_a_run_of_clean_windows() {
        let mut c = ctrl(60);
        c.observe(30, true, 60); // cap 20
        assert_eq!(c.cap_fps(), 20);
        // 5 clean windows: still parked (recover=6 not yet reached).
        for _ in 0..5 {
            c.observe(20, false, 60);
        }
        assert_eq!(c.cap_fps(), 20, "lazy recovery holds until the streak");
        // 6th clean window trips one +step probe.
        c.observe(20, false, 60);
        assert_eq!(c.cap_fps(), 30);
    }

    #[test]
    fn recovery_climbs_back_to_full_and_pins_divisor_1() {
        let mut c = ctrl(60);
        c.observe(0, true, 60); // cap 50
        // Enough clean windows to walk 50 → 60 (one +10 step per `recover`).
        for _ in 0..(6 * 2) {
            c.observe(60, false, 60);
        }
        assert_eq!(c.cap_fps(), 60);
        assert_eq!(c.divisor(), 1);
    }

    #[test]
    fn asymmetric_step_up_climbs_faster_without_coarsening_the_shed() {
        let mut c = ctrl(60);
        c.step_up = 30;
        // Shed still clamps by the (small) down-step: 35-10=25.
        c.observe(35, true, 60);
        assert_eq!(c.cap_fps(), 25);
        // One recovery streak climbs by the (big) up-step: 25+30=55.
        for _ in 0..6 {
            c.observe(25, false, 60);
        }
        assert_eq!(c.cap_fps(), 55);
        // Ceiling still respected on the next probe.
        for _ in 0..6 {
            c.observe(55, false, 60);
        }
        assert_eq!(c.cap_fps(), 60);
    }

    #[test]
    fn disabled_pins_divisor_1() {
        let mut c = ctrl(60);
        c.enabled = false;
        assert_eq!(c.observe(5, true, 60), 1);
        assert_eq!(c.observe(5, true, 60), 1);
        assert_eq!(c.cap_fps(), 60);
    }

    #[test]
    fn slow_start_recovers_a_deep_clamp_in_five_windows() {
        let mut c = ctrl_ss(60);
        // Hard struggle: managed 5 → clamp floors at min_fps 12.
        c.observe(5, true, 60);
        assert_eq!(c.cap_fps(), 12);
        // Confirmation phase: the first probe is still lazy (recover=3).
        c.observe(12, false, 60);
        c.observe(12, false, 60);
        assert_eq!(c.cap_fps(), 12, "held until the streak confirms");
        // w3: first probe = max(step_up 20, gap 48/2 = 24) → 36, climbing.
        c.observe(12, false, 60);
        assert_eq!(c.cap_fps(), 36);
        // w4: climbing → probe EVERY window: max(20, 24/2) = 20 → 56.
        c.observe(36, false, 60);
        assert_eq!(c.cap_fps(), 56);
        // w5: max(20, 4/2) = 20, capped at capture → 60, climb over.
        c.observe(56, false, 60);
        assert_eq!(c.cap_fps(), 60);
        assert_eq!(c.divisor(), 1);
    }

    #[test]
    fn restruggle_mid_climb_requires_full_confirmation_again() {
        let mut c = ctrl_ss(60);
        c.observe(5, true, 60); // cap 12
        for _ in 0..3 {
            c.observe(12, false, 60);
        }
        assert_eq!(c.cap_fps(), 36, "climb started");
        // The climb overshot the viewer → it struggles again.
        c.observe(30, true, 60);
        assert_eq!(c.cap_fps(), 20); // 30.min(36) - 10
        // Two clean windows do NOT probe — the climb latch was reset and
        // the first probe is lazy again (oscillation guard).
        c.observe(20, false, 60);
        c.observe(20, false, 60);
        assert_eq!(c.cap_fps(), 20);
        c.observe(20, false, 60);
        assert_eq!(c.cap_fps(), 40, "third clean window probes (gap 40/2 = 20)");
    }

    #[test]
    fn pack_unpack_round_trips() {
        assert_eq!(unpack_report(pack_report(30, true)), (30, true));
        assert_eq!(unpack_report(pack_report(58, false)), (58, false));
        // Swap-reset / no-signal decodes to a clean window.
        assert_eq!(unpack_report(0), (0, false));
        // fps saturates at 16 bits, struggle bit survives.
        assert_eq!(unpack_report(pack_report(999_999, true)), (0xFFFF, true));
    }

    #[test]
    fn mid_session_capture_fps_drop_reclamps_cap() {
        let mut c = ctrl(60);
        c.observe(30, true, 60); // cap 20 at 60 fps capture
        // SW auto-cap drops capture to 30; the cap (20) is still valid, divisor
        // recomputes against 30 → ceil(30/20)=2 (15 fps).
        let div = c.observe(20, false, 30);
        assert_eq!(c.divisor(), 2);
        assert_eq!(div, 2);
        assert!(c.cap_fps() <= 30);
    }

    // ── FR-15 — age packing + loop ──────────────────────────────────────

    #[test]
    fn age_pack_roundtrips_and_zero_is_no_report() {
        assert_eq!(unpack_age(pack_age(120, 62)), Some((120, 62)));
        assert_eq!(unpack_age(0), None);
        // A genuine 0 ms input is floored to 1 so it cannot collide with
        // the swap-reset sentinel.
        assert_eq!(unpack_age(pack_age(0, 0)), Some((1, 1)));
        assert_eq!(
            unpack_age(pack_age(u16::MAX, u16::MAX)),
            Some((u16::MAX, u16::MAX))
        );
    }

    #[test]
    fn age_loop_needs_two_consecutive_over_windows() {
        let mut l = AgeLoop::new();
        // Establish a ~60 ms floor.
        assert!(!l.observe(62, 60));
        assert!(!l.observe(64, 61));
        assert_eq!(l.floor_ms(), Some(60));
        // One over-window (140 vs floor 60 = +80 ≥ slack 70) is a blip…
        assert!(!l.observe(140, 62));
        // …a clean window resets the streak…
        assert!(!l.observe(65, 61));
        assert!(!l.observe(150, 63));
        // …and only the SECOND consecutive over-window triggers.
        assert!(l.observe(155, 64));
        // Staying over keeps triggering (one MD per window by design).
        assert!(l.observe(160, 64));
        // Recovery clears it immediately.
        assert!(!l.observe(70, 61));
    }

    #[test]
    fn age_loop_floor_rebaselines_after_a_path_change() {
        let mut l = AgeLoop::new();
        l.observe(60, 55);
        // A path change raises the true floor to ~200 ms. The stale 55 ms
        // sample ages out of the 30-window ring, after which 205 vs the new
        // floor is within slack and the loop stops treating it as overload.
        for _ in 0..30 {
            l.observe(205, 200);
        }
        assert_eq!(l.floor_ms(), Some(200));
        assert!(!l.observe(205, 200));
    }

    #[test]
    fn age_loop_within_slack_never_triggers() {
        let mut l = AgeLoop::new();
        l.observe(60, 58);
        for _ in 0..50 {
            // +65 ms of climb stays under the 70 ms slack.
            assert!(!l.observe(123, 58));
        }
    }
}

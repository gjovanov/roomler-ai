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

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Lowest fps the controller will cap down to. Below this, motion is a
/// slideshow; deeper relief is the (manual) resolution lever's job, not more
/// fps shedding. Env `ROOMLER_AGENT_VIEWER_RATE_MIN_FPS` (default 12).
fn min_fps() -> u32 {
    env_u32("ROOMLER_AGENT_VIEWER_RATE_MIN_FPS", 12).max(1)
}

/// fps step per adjustment — down on struggle, up on recovery.
/// Env `ROOMLER_AGENT_VIEWER_RATE_STEP` (default 10).
fn fps_step() -> u32 {
    env_u32("ROOMLER_AGENT_VIEWER_RATE_STEP", 10).max(1)
}

/// Consecutive clean windows before the cap probes back UP one step. Lazy so a
/// viewer parked just under its ceiling doesn't oscillate every window.
/// Env `ROOMLER_AGENT_VIEWER_RATE_RECOVER` (default 6).
fn recover_windows() -> u32 {
    env_u32("ROOMLER_AGENT_VIEWER_RATE_RECOVER", 6).max(1)
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
    recover: u32,
    enabled: bool,
}

impl ViewerRateController {
    pub fn new(capture_fps: u32) -> Self {
        let capture_fps = capture_fps.max(1);
        Self {
            cap_fps: capture_fps,
            capture_fps,
            clean_streak: 0,
            min_fps: min_fps().min(capture_fps),
            step: fps_step(),
            recover: recover_windows(),
            // Kill switch — default ON; `ROOMLER_AGENT_VIEWER_RATE=0` (or
            // `false`) pins the cap at the capture rate (divisor 1, no shedding)
            // so a misbehaving field host reverts without a rebuild.
            enabled: !matches!(
                std::env::var("ROOMLER_AGENT_VIEWER_RATE").ok().as_deref(),
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
        } else {
            self.clean_streak += 1;
            if self.clean_streak >= self.recover {
                self.cap_fps = (self.cap_fps + self.step).min(self.capture_fps);
                self.clean_streak = 0;
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

#[cfg(test)]
mod tests {
    use super::*;

    // Deterministic controller regardless of ambient env.
    fn ctrl(capture_fps: u32) -> ViewerRateController {
        ViewerRateController {
            cap_fps: capture_fps,
            capture_fps,
            clean_streak: 0,
            min_fps: 12,
            step: 10,
            recover: 6,
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
    fn disabled_pins_divisor_1() {
        let mut c = ctrl(60);
        c.enabled = false;
        assert_eq!(c.observe(5, true, 60), 1);
        assert_eq!(c.observe(5, true, 60), 1);
        assert_eq!(c.cap_fps(), 60);
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
}

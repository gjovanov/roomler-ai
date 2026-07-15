//! Viewer decode-pressure controller for the DataChannel video pumps.
//!
//! The browser sends `rc:keyframe` over the control DC whenever its
//! WebCodecs decode queue backs up and it drops deltas to resync. A weak
//! viewer (slow iGPU) under high motion — e.g. dragging a window — spirals:
//! decode-behind → keyframe request → the agent forces a large IDR → which
//! is HEAVIER to decode → further behind → the periodic 1-2 s hang the field
//! reported on Iris-Xe / hybrid laptops (while an RTX 5090 viewer of the same
//! host never stutters — proving it's the viewer's decoder, not capture).
//!
//! This controller turns the keyframe-request RATE into a discrete pressure
//! `level`, and — fps-first, per the operator's choice — maps the level to a
//! frame-skip divisor so the pump sheds *effective* fps (fewer frames for the
//! weak decoder to chew) before anything touches resolution. Shedding frames
//! also drains the viewer's decode queue, so it stops requesting keyframes and
//! the spiral unwinds on its own; the level then decays back to 0 (full fps).
//!
//! Pure: no webrtc / capture / ffmpeg types, so it unit-tests on the default
//! `cargo test --lib`. The pump features are what USE it, hence the dead_code
//! allow on the signalling-only build (mirrors `aimd`).

/// Highest pressure level. Levels 1..=2 shed fps (the implemented tier);
/// 3..=4 are headroom for the resolution tier (rc.185).
pub const MAX_LEVEL: u8 = 4;

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Escalate one level when a 1-second window sees ≥ this many keyframe
/// requests. Env `ROOMLER_AGENT_DECODE_PRESSURE_RISE` (default 4) so the
/// trigger point can be tuned in the field without a rebuild.
fn rise_threshold() -> u32 {
    env_u32("ROOMLER_AGENT_DECODE_PRESSURE_RISE", 4)
}

/// De-escalate one level when a window sees ≤ this many requests. Env
/// `ROOMLER_AGENT_DECODE_PRESSURE_FALL` (default 1).
fn fall_threshold() -> u32 {
    env_u32("ROOMLER_AGENT_DECODE_PRESSURE_FALL", 1)
}

/// Discrete decode-pressure state. Step it once per observation window with
/// the keyframe-request count seen in that window.
pub struct DecodePressure {
    level: u8,
    rise: u32,
    fall: u32,
    enabled: bool,
}

impl DecodePressure {
    pub fn new() -> Self {
        Self {
            level: 0,
            rise: rise_threshold().max(1),
            fall: fall_threshold(),
            // Kill switch — default ON; `ROOMLER_AGENT_DECODE_PRESSURE=0`
            // (or `false`) pins the level at 0 (no skipping) so a
            // misbehaving field host can be reverted without a rebuild.
            enabled: !matches!(
                std::env::var("ROOMLER_AGENT_DECODE_PRESSURE")
                    .ok()
                    .as_deref(),
                Some("0") | Some("false")
            ),
        }
    }

    /// Advance the level from the keyframe-request count observed in one
    /// window. Rise/fall are hysteretic (a dead zone between `fall` and
    /// `rise` holds the level steady, so a session hovering near the
    /// threshold doesn't oscillate). Returns the new level. When disabled
    /// the level is pinned at 0 (fps_divisor 1 → no skipping).
    pub fn step(&mut self, requests_in_window: u32) -> u8 {
        if !self.enabled {
            return 0;
        }
        if requests_in_window >= self.rise {
            self.level = (self.level + 1).min(MAX_LEVEL);
        } else if requests_in_window <= self.fall {
            self.level = self.level.saturating_sub(1);
        }
        self.level
    }

    pub fn level(&self) -> u8 {
        self.level
    }

    /// fps-first frame-skip divisor: encode+send 1 of every N captured
    /// *delta* frames (keyframes are never skipped). Level 0 = 1 (no skip),
    /// 1 = 2 (~half fps), 2+ = 3 (~third). Capped at 3 so skipping alone
    /// never starves motion below ~20 fps — deeper relief is the resolution
    /// tier's job (rc.185), not more aggressive skipping.
    pub fn fps_divisor(level: u8) -> u32 {
        match level {
            0 => 1,
            1 => 2,
            _ => 3,
        }
    }
}

impl Default for DecodePressure {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Force deterministic thresholds regardless of the ambient env.
    fn ctrl() -> DecodePressure {
        DecodePressure {
            level: 0,
            rise: 4,
            fall: 1,
            enabled: true,
        }
    }

    #[test]
    fn disabled_pins_level_at_zero() {
        let mut c = DecodePressure {
            level: 0,
            rise: 4,
            fall: 1,
            enabled: false,
        };
        assert_eq!(c.step(100), 0);
        assert_eq!(c.step(100), 0);
        assert_eq!(DecodePressure::fps_divisor(c.level()), 1);
    }

    #[test]
    fn rises_one_level_per_busy_window_and_caps() {
        let mut c = ctrl();
        assert_eq!(c.step(10), 1);
        assert_eq!(c.step(10), 2);
        assert_eq!(c.step(10), 3);
        assert_eq!(c.step(10), 4);
        // Capped at MAX_LEVEL.
        assert_eq!(c.step(10), MAX_LEVEL);
    }

    #[test]
    fn falls_one_level_per_quiet_window_and_floors() {
        let mut c = ctrl();
        c.step(10);
        c.step(10); // level 2
        assert_eq!(c.step(0), 1);
        assert_eq!(c.step(1), 0); // fall threshold is inclusive
        assert_eq!(c.step(0), 0); // floors at 0
    }

    #[test]
    fn dead_zone_holds_level_steady() {
        let mut c = ctrl();
        c.step(10); // level 1
        // 2 requests: between fall(1) and rise(4) → no change (no oscillation).
        assert_eq!(c.step(2), 1);
        assert_eq!(c.step(3), 1);
    }

    #[test]
    fn fps_divisor_is_fps_first_and_capped() {
        assert_eq!(DecodePressure::fps_divisor(0), 1);
        assert_eq!(DecodePressure::fps_divisor(1), 2);
        assert_eq!(DecodePressure::fps_divisor(2), 3);
        assert_eq!(DecodePressure::fps_divisor(3), 3);
        assert_eq!(DecodePressure::fps_divisor(4), 3);
    }
}

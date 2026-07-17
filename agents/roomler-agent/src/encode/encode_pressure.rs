//! Encode-pressure controller — auto-reduces the encoder's bitrate ceiling
//! when the *encoder itself* can't keep up, so a weak sender GPU stops
//! saturating and the periodic freeze goes away.
//!
//! Field root cause (PC50045 Iris Xe, `hevc_qsv`, 1920×1200@60): the shared
//! iGPU does DXGI capture AND HEVC encode, and under sustained window-drag
//! motion `avg_encode_ms` climbs from ~11 ms to 40-194 ms — the 194 ms
//! windows are the 1-2 s hangs. The operator's manual `FFMPEG_FPS=30` fixed
//! it: that halved the maxrate (`ffmpeg_maxrate_bps` = w×h×fps×0.07), so the
//! encoder emits smaller frames → ~3× faster encode → steady 11 ms, no
//! spikes. This controller does that automatically and per-session: it
//! watches the encode time and pulls a maxrate SCALE FACTOR down when the
//! encoder saturates, back up when it recovers — so a fast host / static
//! screen keeps full quality and only a struggling encoder gets throttled.
//!
//! Bitrate-first (not fps/resolution): lowering the ceiling is the least
//! visible lever (slightly more compression under motion, cleans up when
//! static — no framerate drop, no resize) and the field proved it's what
//! actually cut the encode time. fps / resolution tiers can layer on later
//! if the ceiling floor isn't enough.
//!
//! Pure (no ffmpeg/webrtc types) → unit-tested on the default `cargo test
//! --lib`. The pump multiplies its per-resolution maxrate ceiling by
//! `factor()` before feeding the AIMD, which then tracks the link down from
//! the reduced ceiling as usual (so the effective rate is the min of the
//! encode-limited and network-limited ceilings).

/// Never throttle the ceiling below this fraction of the resolution maxrate —
/// below ~40% the picture degrades more than the freeze it prevents.
pub const FACTOR_FLOOR: f32 = 0.4;

use tunnel_core::env::node_env;

fn env_f32(suffix: &str, default: f32) -> f32 {
    node_env(suffix)
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(default)
}

pub struct EncodePressure {
    ewma_ms: f32,
    factor: f32,
    high_ms: f32,
    low_ms: f32,
    enabled: bool,
}

impl EncodePressure {
    pub fn new() -> Self {
        Self {
            ewma_ms: 0.0,
            // Saturate above `high_ms` (encoder can't hold ~40 fps), recover
            // below `low_ms`. Env-tunable so the field trigger can move
            // without a rebuild; kill switch pins the factor at 1.0.
            high_ms: env_f32("ENCODE_PRESSURE_HIGH_MS", 25.0),
            low_ms: env_f32("ENCODE_PRESSURE_LOW_MS", 15.0),
            factor: 1.0,
            enabled: !matches!(
                node_env("ENCODE_PRESSURE").as_deref(),
                Some("0") | Some("false")
            ),
        }
    }

    /// Step once per heartbeat window with that window's average encode time
    /// (ms). Returns the maxrate scale factor in `[FACTOR_FLOOR, 1.0]`.
    /// Hysteretic: a dead zone between `low_ms` and `high_ms` holds the
    /// factor steady so a session hovering near the threshold doesn't
    /// oscillate. Multiplicative down (fast relief) / up (lazy recovery).
    pub fn observe(&mut self, avg_encode_ms: f32) -> f32 {
        if !self.enabled {
            return 1.0;
        }
        const ALPHA: f32 = 0.4;
        // Seed on the first sample so we don't ramp slowly up from 0.
        if self.ewma_ms <= 0.0 {
            self.ewma_ms = avg_encode_ms;
        } else {
            self.ewma_ms = (1.0 - ALPHA) * self.ewma_ms + ALPHA * avg_encode_ms;
        }
        if self.ewma_ms > self.high_ms {
            self.factor = (self.factor * 0.8).max(FACTOR_FLOOR);
        } else if self.ewma_ms < self.low_ms {
            self.factor = (self.factor * 1.15).min(1.0);
        }
        self.factor
    }

    pub fn factor(&self) -> f32 {
        self.factor
    }

    pub fn ewma_ms(&self) -> f32 {
        self.ewma_ms
    }
}

impl Default for EncodePressure {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl() -> EncodePressure {
        EncodePressure {
            ewma_ms: 0.0,
            factor: 1.0,
            high_ms: 25.0,
            low_ms: 15.0,
            enabled: true,
        }
    }

    #[test]
    fn saturation_pulls_factor_down_to_floor() {
        let mut c = ctrl();
        // Sustained slow encode (~50 ms) → factor ratchets down.
        let mut last = 1.0;
        for _ in 0..30 {
            last = c.observe(50.0);
        }
        assert!(
            (last - FACTOR_FLOOR).abs() < 1e-3,
            "factor should floor at {FACTOR_FLOOR}, got {last}"
        );
    }

    #[test]
    fn recovery_returns_factor_to_one() {
        let mut c = ctrl();
        for _ in 0..30 {
            c.observe(50.0);
        }
        assert!(c.factor() < 1.0);
        // Encoder recovers (fast, ~8 ms) → factor climbs back to 1.0.
        let mut last = c.factor();
        for _ in 0..40 {
            last = c.observe(8.0);
        }
        assert!(
            (last - 1.0).abs() < 1e-3,
            "should recover to 1.0, got {last}"
        );
    }

    #[test]
    fn dead_zone_holds_factor_steady() {
        let mut c = ctrl();
        // One spike drops the factor; then feed dead-zone samples (20 ms,
        // between low=15 and high=25) until the EWMA settles below `high` and
        // the factor stops moving.
        c.observe(40.0);
        for _ in 0..6 {
            c.observe(20.0);
        }
        let settled = c.factor();
        assert!(settled < 1.0, "should have throttled under the spike");
        // EWMA now parked in the dead zone → factor holds steady.
        assert_eq!(c.observe(20.0), settled);
        assert_eq!(c.observe(20.0), settled);
    }

    #[test]
    fn disabled_pins_factor_at_one() {
        let mut c = EncodePressure {
            ewma_ms: 0.0,
            factor: 1.0,
            high_ms: 25.0,
            low_ms: 15.0,
            enabled: false,
        };
        assert_eq!(c.observe(200.0), 1.0);
        assert_eq!(c.observe(200.0), 1.0);
        assert_eq!(c.factor(), 1.0);
    }
}

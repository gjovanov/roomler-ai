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

    /// Classify the current pressure for the resolution tier tracker
    /// (shelf item 2026-07-27, "encode-bound auto-downscale"). Computed here
    /// because the EWMA + thresholds live here:
    ///
    /// * `Down` — the bitrate lever is EXHAUSTED (factor at the floor) and
    ///   the encoder is still saturated → shrinking pixels is the only lever
    ///   left.
    /// * `Up` — the encoder has DEEP headroom: factor fully recovered and
    ///   the EWMA is low enough that ~2.5× the work (one tier up ≈ 1.8-2.3×
    ///   the pixels) would still sit under `high_ms`. The predictive margin
    ///   is what prevents the down↔up ping-pong: "recovered at 1080p" is not
    ///   enough, it must be *fast* at 1080p.
    /// * `Hold` — anything in between.
    pub fn tier_signal(&self) -> TierSignal {
        const UP_HEADROOM: f32 = 2.5;
        if !self.enabled || self.ewma_ms <= 0.0 {
            return TierSignal::Hold;
        }
        if self.factor <= FACTOR_FLOOR + 1e-3 && self.ewma_ms > self.high_ms {
            TierSignal::Down
        } else if self.factor >= 1.0 - 1e-3 && self.ewma_ms * UP_HEADROOM < self.high_ms {
            TierSignal::Up
        } else {
            TierSignal::Hold
        }
    }
}

/// Pressure classification for [`DownscaleTier`]. See
/// [`EncodePressure::tier_signal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierSignal {
    Down,
    Up,
    Hold,
}

/// Long-edge caps per tier: native → ~1440p-class → ~1080p-class floor.
/// Fed to `effective_target_resolution`'s SOFT slot, so an explicit
/// controller-side Fixed pick always wins (the auto tier only shapes
/// `Native`/auto sessions) and the relay hard cap still composes after.
pub const TIER_LONG_EDGES: [Option<u32>; 3] = [None, Some(2560), Some(1920)];

/// Consecutive saturated heartbeat windows (≈2 s each) before stepping a
/// tier DOWN — ≈10 s of bitrate-floor saturation.
pub const TIER_DOWN_WINDOWS: u32 = 5;

/// Consecutive deep-headroom windows before stepping back UP — ≈60 s of
/// proven-fast encode at the smaller resolution.
pub const TIER_UP_WINDOWS: u32 = 30;

/// Minimum spacing between ANY two tier changes. Each change is an encoder
/// rebuild + IDR + a wire dims change; churn is worse than either steady
/// state.
pub const TIER_CHANGE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

/// Resolution tier tracker layered on [`EncodePressure`] (the docstring's
/// "fps / resolution tiers can layer on later" — this is that layer, for
/// GEAL8N6-class hosts whose 4K panel saturates the encoder even at the
/// bitrate floor). Pure: step once per heartbeat window with the current
/// [`TierSignal`]; emits `Some(new_long_edge_cap)` exactly when the tier
/// changes. Kill switch `ROOMLER_AGENT_AUTO_DOWNSCALE=0`.
pub struct DownscaleTier {
    tier: usize,
    down_run: u32,
    up_run: u32,
    last_change: Option<std::time::Instant>,
    enabled: bool,
}

impl DownscaleTier {
    pub fn new() -> Self {
        Self {
            tier: 0,
            down_run: 0,
            up_run: 0,
            last_change: None,
            enabled: !matches!(
                node_env("AUTO_DOWNSCALE").as_deref(),
                Some("0") | Some("false")
            ),
        }
    }

    /// Current cap (`None` = native).
    pub fn cap(&self) -> Option<u32> {
        TIER_LONG_EDGES[self.tier]
    }

    /// Fold one heartbeat window's signal. Returns the NEW cap when the tier
    /// steps, `None` when it holds.
    pub fn observe(&mut self, signal: TierSignal, now: std::time::Instant) -> Option<Option<u32>> {
        if !self.enabled {
            return None;
        }
        match signal {
            TierSignal::Down => {
                self.down_run += 1;
                self.up_run = 0;
            }
            TierSignal::Up => {
                self.up_run += 1;
                self.down_run = 0;
            }
            TierSignal::Hold => {
                self.down_run = 0;
                self.up_run = 0;
            }
        }
        let cooled = self
            .last_change
            .is_none_or(|t| now.duration_since(t) >= TIER_CHANGE_COOLDOWN);
        if self.down_run >= TIER_DOWN_WINDOWS && self.tier + 1 < TIER_LONG_EDGES.len() && cooled {
            self.tier += 1;
            self.down_run = 0;
            self.last_change = Some(now);
            return Some(self.cap());
        }
        if self.up_run >= TIER_UP_WINDOWS && self.tier > 0 && cooled {
            self.tier -= 1;
            self.up_run = 0;
            self.last_change = Some(now);
            return Some(self.cap());
        }
        None
    }
}

impl Default for DownscaleTier {
    fn default() -> Self {
        Self::new()
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

    fn pressure(ewma_ms: f32, factor: f32) -> EncodePressure {
        EncodePressure {
            ewma_ms,
            factor,
            high_ms: 25.0,
            low_ms: 15.0,
            enabled: true,
        }
    }

    #[test]
    fn tier_signal_classifies_the_three_states() {
        // Bitrate lever exhausted + still saturated → Down.
        assert_eq!(pressure(40.0, FACTOR_FLOOR).tier_signal(), TierSignal::Down);
        // Saturated but the factor still has room → Hold (bitrate lever first).
        assert_eq!(pressure(40.0, 0.8).tier_signal(), TierSignal::Hold);
        // Fully recovered AND deep headroom (9 × 2.5 = 22.5 < 25) → Up.
        assert_eq!(pressure(9.0, 1.0).tier_signal(), TierSignal::Up);
        // Recovered but merely "ok" (12 × 2.5 = 30 > 25) → Hold, not Up —
        // the predictive margin that stops the down↔up ping-pong.
        assert_eq!(pressure(12.0, 1.0).tier_signal(), TierSignal::Hold);
        // No samples yet → Hold.
        assert_eq!(pressure(0.0, 1.0).tier_signal(), TierSignal::Hold);
    }

    fn tier() -> DownscaleTier {
        DownscaleTier {
            tier: 0,
            down_run: 0,
            up_run: 0,
            last_change: None,
            enabled: true,
        }
    }

    #[test]
    fn sustained_saturation_steps_down_one_tier_then_cooldown_blocks() {
        let mut t = tier();
        let now = std::time::Instant::now();
        for _ in 0..(TIER_DOWN_WINDOWS - 1) {
            assert_eq!(t.observe(TierSignal::Down, now), None);
        }
        assert_eq!(t.observe(TierSignal::Down, now), Some(Some(2560)));
        // Still saturated at the smaller res — cooldown blocks the next step.
        for _ in 0..(TIER_DOWN_WINDOWS * 2) {
            assert_eq!(t.observe(TierSignal::Down, now), None);
        }
        // After the cooldown the STILL-saturated run fires immediately —
        // FlipTracker semantics: the cooldown defers a persisted condition,
        // it doesn't demand 5 fresh windows of re-confirmation (the run was
        // continuous throughout; a Hold window would have reset it).
        let later = now + TIER_CHANGE_COOLDOWN + std::time::Duration::from_secs(1);
        assert_eq!(t.observe(TierSignal::Down, later), Some(Some(1920)));
        // 1080p-class is the floor — never steps below.
        let much_later = later + TIER_CHANGE_COOLDOWN + std::time::Duration::from_secs(1);
        for _ in 0..(TIER_DOWN_WINDOWS * 3) {
            assert_eq!(t.observe(TierSignal::Down, much_later), None);
        }
        assert_eq!(t.cap(), Some(1920));
    }

    #[test]
    fn a_hold_window_resets_both_runs() {
        let mut t = tier();
        let now = std::time::Instant::now();
        for _ in 0..(TIER_DOWN_WINDOWS - 1) {
            t.observe(TierSignal::Down, now);
        }
        t.observe(TierSignal::Hold, now); // saturation not consecutive
        for _ in 0..(TIER_DOWN_WINDOWS - 1) {
            assert_eq!(t.observe(TierSignal::Down, now), None);
        }
        assert_eq!(t.observe(TierSignal::Down, now), Some(Some(2560)));
    }

    #[test]
    fn sustained_deep_headroom_steps_back_up() {
        let mut t = DownscaleTier {
            tier: 2,
            down_run: 0,
            up_run: 0,
            last_change: None,
            enabled: true,
        };
        let now = std::time::Instant::now();
        for _ in 0..(TIER_UP_WINDOWS - 1) {
            assert_eq!(t.observe(TierSignal::Up, now), None);
        }
        assert_eq!(t.observe(TierSignal::Up, now), Some(Some(2560)));
        // Native requires another full run + cooldown.
        let later = now + TIER_CHANGE_COOLDOWN + std::time::Duration::from_secs(1);
        for _ in 0..(TIER_UP_WINDOWS - 1) {
            assert_eq!(t.observe(TierSignal::Up, later), None);
        }
        assert_eq!(t.observe(TierSignal::Up, later), Some(None));
        assert_eq!(t.cap(), None);
    }

    #[test]
    fn disabled_tier_never_steps() {
        let mut t = DownscaleTier {
            tier: 0,
            down_run: 0,
            up_run: 0,
            last_change: None,
            enabled: false,
        };
        let now = std::time::Instant::now();
        for _ in 0..(TIER_DOWN_WINDOWS * 4) {
            assert_eq!(t.observe(TierSignal::Down, now), None);
        }
        assert_eq!(t.cap(), None);
    }
}

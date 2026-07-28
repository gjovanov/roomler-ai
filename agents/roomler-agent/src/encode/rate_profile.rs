//! P3 (Parsec-class plan) — transport/codec-aware rate-profile helpers for
//! the DC video pumps. Pure (no ffmpeg/webrtc/tokio types, explicit
//! `Instant`s), so everything here unit-tests on the default feature build
//! even though the callers are `ffmpeg-encoder`-gated.
//!
//! Three concerns live here:
//!
//! 1. **Persisted-flip rebuild** ([`FlipTracker`]): a mid-session
//!    relay↔direct ICE renomination already re-clamps the AIMD ceiling
//!    LIVE, but the encoder's fps/bufsize and the capture pacer were baked
//!    at pump start — a session that STARTED on the relay stayed at 30 fps
//!    forever after upgrading to direct (peer.rs pump-start
//!    `ffmpeg_target_fps(constrained)` + `capture::open_default`). The
//!    tracker decides when a flip has persisted long enough to be worth a
//!    full encoder rebuild + capture reopen (each costs an IDR + a brief
//!    hiccup, so: 2 consecutive 5 s checks to debounce ICE flapping, and at
//!    most one rebuild per 60 s — the rc.217 "recovery that re-triggers its
//!    own trigger needs a cooldown" lesson applied from day one).
//!
//! 2. **Codec rate factor** ([`codec_rate_factor_pct`]): the maxrate
//!    ceiling was codec-agnostic (0.07 bpp/s for everyone), but H.264
//!    needs ~1.5× the bits of HEVC/AV1 for the same screen-content text
//!    sharpness. Field 2026-07-26 (P2 rollout, PC50045/GEAL8N6/PC55331):
//!    H.264-DC motion "very smooth" but "text gets blurred from time to
//!    time" — transients exhausting the HEVC-sized budget. H.264 gets a
//!    150% ceiling; the relay clamp still applies AFTER the factor (pipe
//!    physics don't care which codec fills them).
//!
//! 3. **H.264 CQ adjustment** ([`h264_cq_adjust`]): at equal nominal
//!    quality numbers H.264 codes text visibly softer than HEVC (different
//!    QP scale + weaker intra prediction). The h264_* encoders get a
//!    2-step sharper constant-quality target off the shared `FFMPEG_CQ`
//!    base (env still wins as the base; the adjustment is relative).

use std::time::{Duration, Instant};

/// Consecutive transport-recheck observations (5 s apart in the pumps) that
/// must agree before a flip triggers a rebuild. 2 → a single flapping check
/// never rebuilds; a real renomination rebuilds within ~10 s.
pub const FLIP_REQUIRED_CONSECUTIVE: u8 = 2;

/// Minimum spacing between flip-rebuilds. An ICE path oscillating faster
/// than this keeps the live AIMD clamp (which follows every flip) but stops
/// paying the IDR + hiccup cost of a rebuild each time.
pub const FLIP_REBUILD_COOLDOWN: Duration = Duration::from_secs(60);

/// Debounced mid-session transport-flip → rebuild decision. `stable` is the
/// state the pump last BUILT for; `observe` is fed every transport recheck
/// with the currently-detected state and returns `Some(new_state)` exactly
/// when the pump should rebuild (encoder + capture pacer) for it.
#[derive(Debug)]
pub struct FlipTracker {
    stable: bool,
    pending: Option<(bool, u8)>,
    last_rebuild: Option<Instant>,
}

impl FlipTracker {
    pub fn new(initial_constrained: bool) -> Self {
        Self {
            stable: initial_constrained,
            pending: None,
            last_rebuild: None,
        }
    }

    pub fn observe(&mut self, detected: bool, now: Instant) -> Option<bool> {
        if detected == self.stable {
            // Back to (or still at) the built-for state — a flap resolved
            // itself; drop any pending count.
            self.pending = None;
            return None;
        }
        let count = match self.pending {
            Some((dir, n)) if dir == detected => n + 1,
            // First observation of this direction (or a direction change
            // mid-count — restart the count for the new direction).
            _ => 1,
        };
        self.pending = Some((detected, count));
        if count < FLIP_REQUIRED_CONSECUTIVE {
            return None;
        }
        if let Some(t) = self.last_rebuild
            && now.duration_since(t) < FLIP_REBUILD_COOLDOWN
        {
            // Persisted, but we rebuilt too recently — keep the pending
            // count saturated; the next observe after cooldown fires.
            self.pending = Some((detected, FLIP_REQUIRED_CONSECUTIVE));
            return None;
        }
        self.stable = detected;
        self.pending = None;
        self.last_rebuild = Some(now);
        Some(detected)
    }
}

/// Per-codec maxrate ceiling factor, in percent. Keyed by the pump's
/// `FfmpegDcCodec::label()` vocabulary ("HEVC" / "VP9" / "AV1" / "H264").
///
/// Built-ins (2026-07-28 field, NEO16→DESKTOP-69T5HUD/UHD 620): H264 keeps
/// its 150 % band (equal text sharpness genuinely needs ~1.5× the bits);
/// HEVC moves 100 → **125** — the "HEVC needs ⅔ the bits of H.264"
/// efficiency assumption fails for desktop drag-motion on realtime QSV
/// presets, and the starved band was the measured reason H.264 looked
/// visibly smoother than HEVC on the same direct path (13.1 vs 8.7 Mbps;
/// encode times were fine for both).
///
/// Operator override per codec: `ROOMLER_NODE_RATE_FACTOR_<LABEL>` (legacy
/// `ROOMLER_AGENT_` prefix + the `rate_factor_*` config keys accepted via
/// [`tunnel_core::env::node_env`]). Clamped to 50–400; garbage falls back
/// to the built-in. The pump re-reads per frame, so a REAL env var applies
/// live; config-key changes need a service restart (set-once fallback map).
pub fn codec_rate_factor_pct(codec_label: &str) -> usize {
    let builtin: usize = match codec_label {
        "H264" => 150,
        "HEVC" => 125,
        _ => 100,
    };
    match tunnel_core::env::node_env(&format!("RATE_FACTOR_{codec_label}")) {
        Some(v) => match v.trim().parse::<usize>() {
            Ok(pct) => pct.clamp(50, 400),
            Err(_) => builtin,
        },
        None => builtin,
    }
}

/// H.264 constant-quality adjustment: 2 steps sharper than the shared CQ
/// base, floored at the global minimum (10). No-op for every other encoder.
pub fn h264_cq_adjust(encoder_name: &str, cq: u32) -> u32 {
    if encoder_name.contains("h264") {
        cq.saturating_sub(2).max(10)
    } else {
        cq
    }
}

/// P4 — vp9_qsv runtime-forced-IDR verdict → `(long_gop, low_power)`.
/// `verdict` = `Some((honors_lp1, honors_lp0))` from the startup probe
/// (does the encoder emit a key-flagged packet after a runtime force, per
/// `low_power` mode?), `None` when the probe is disabled or never ran.
/// `forced_lp` = the `ROOMLER_AGENT_QSV_LOW_POWER` operator override.
///
/// The long GOP (on-demand-only keys — kills the residual ~1 Hz natural-key
/// pulse on VP9-over-DC Intel hosts) is granted ONLY for a mode the probe
/// MEASURED as honouring runtime forcing; everything else keeps the rc.219
/// containment (60-frame natural GOP + VDEnc). Preference order when both
/// modes honour: low_power=1 (the Iris Xe fps-unlock path).
pub fn vp9_qsv_config(verdict: Option<(bool, bool)>, forced_lp: Option<bool>) -> (bool, bool) {
    if let Some(lp) = forced_lp {
        let honors = match (verdict, lp) {
            (Some((h1, _)), true) => h1,
            (Some((_, h0)), false) => h0,
            (None, _) => false,
        };
        return (honors, lp);
    }
    match verdict {
        Some((true, _)) => (true, true),
        Some((false, true)) => (true, false),
        Some((false, false)) | None => (false, true),
    }
}

/// Real frames since the last settle that make a motion episode "a burst"
/// worth an idle-settle resync IDR. A window-drag produces hundreds; a caret
/// blink, a clock tick, or a couple of keystrokes produce 1-3 and must NOT
/// qualify. Env `ROOMLER_AGENT_SETTLE_KF_MIN_BURST` overrides; `0` restores
/// the legacy rc.187 fire-on-every-settle behaviour.
pub const SETTLE_KF_MIN_BURST: u32 = 10;

/// Minimum spacing between settle IDRs — a scroll-pause-scroll pattern
/// re-settles every second or two and shouldn't pay an IDR each time.
pub const SETTLE_KF_MIN_GAP: Duration = Duration::from_secs(5);

/// Gate for the rc.187 idle-settle keyframe (field 2026-07-27, NEO16 viewing
/// PC55331): the settle IDR fired 60 ms after EVERY real frame, and a
/// blinking text caret (Windows default ~530 ms toggle) produces a real frame
/// per toggle — a ~2 Hz forced-IDR metronome, visible as text pulsing
/// blur→crystal on every codec (worst on av1_nvenc, whose budget-capped IDRs
/// are coarsest relative to their refinement). rc.187's actual purpose — a
/// standalone resync frame after MOTION where a viewer may have dropped
/// frames — only needs the IDR after a real burst, so: fire on the first
/// settle of an episode only if the episode carried `min_burst`+ real frames
/// AND the last settle IDR is `min_gap` in the past. Isolated blips ride as
/// ordinary tiny deltas (which is all they ever were).
#[derive(Debug)]
pub struct SettleKeyframeGate {
    min_burst: u32,
    min_gap: Duration,
    /// Real frames in the current motion episode (reset at each settle).
    burst: u32,
    /// One decision per episode: set at the first settle, cleared by the
    /// next real frame. Keeps the 60 ms keepalive ticks from re-deciding.
    decided_this_episode: bool,
    last_fired: Option<Instant>,
}

impl SettleKeyframeGate {
    pub fn new(min_burst: u32, min_gap: Duration) -> Self {
        Self {
            min_burst,
            min_gap,
            burst: 0,
            decided_this_episode: false,
            last_fired: None,
        }
    }

    /// Defaults + the `ROOMLER_AGENT_SETTLE_KF_MIN_BURST` override. `0` =
    /// legacy (fire on the first settle of every episode, no cooldown).
    pub fn from_env() -> Self {
        let min_burst = tunnel_core::env::node_env("SETTLE_KF_MIN_BURST")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(SETTLE_KF_MIN_BURST);
        let min_gap = if min_burst == 0 {
            Duration::ZERO
        } else {
            SETTLE_KF_MIN_GAP
        };
        Self::new(min_burst, min_gap)
    }

    /// A real (damage-carrying) frame arrived — the episode continues.
    pub fn note_real_frame(&mut self) {
        self.burst = self.burst.saturating_add(1);
        self.decided_this_episode = false;
    }

    /// Call on every idle-keepalive tick. Returns `Some(burst)` exactly when
    /// this settle should carry the resync IDR (the burst size is for the
    /// log); `None` otherwise. The first tick of a settle consumes the
    /// episode's burst and decides; later ticks are no-ops.
    pub fn should_fire_on_settle(&mut self, now: Instant) -> Option<u32> {
        if self.decided_this_episode {
            return None;
        }
        self.decided_this_episode = true;
        let burst = std::mem::take(&mut self.burst);
        if burst < self.min_burst {
            return None;
        }
        if let Some(t) = self.last_fired
            && now.duration_since(t) < self.min_gap
        {
            return None;
        }
        self.last_fired = Some(now);
        Some(burst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn flip_needs_two_consecutive_observations() {
        let mut f = FlipTracker::new(true); // built for relay
        let now = t0();
        assert_eq!(f.observe(false, now), None); // 1st direct sighting
        assert_eq!(f.observe(false, now), Some(false)); // 2nd → rebuild
        // Now built for direct; steady state is quiet.
        assert_eq!(f.observe(false, now), None);
    }

    #[test]
    fn a_single_flap_never_rebuilds() {
        let mut f = FlipTracker::new(true);
        let now = t0();
        assert_eq!(f.observe(false, now), None); // blip
        assert_eq!(f.observe(true, now), None); // back to stable → reset
        assert_eq!(f.observe(false, now), None); // count restarts at 1
        assert_eq!(f.observe(false, now), Some(false));
    }

    #[test]
    fn direction_change_mid_count_restarts_the_count() {
        let mut f = FlipTracker::new(false); // built for direct
        let now = t0();
        assert_eq!(f.observe(true, now), None); // relay ×1
        // (a detected==stable in between resets; tested above — here we jump
        // straight to a second relay sighting)
        assert_eq!(f.observe(true, now), Some(true));
    }

    #[test]
    fn cooldown_defers_but_does_not_lose_a_persisted_flip() {
        let mut f = FlipTracker::new(true);
        let now = t0();
        assert_eq!(f.observe(false, now), None);
        assert_eq!(f.observe(false, now), Some(false)); // rebuild at `now`
        // Path flips back to relay 10 s later — persisted, but inside the
        // 60 s cooldown → deferred.
        let later = now + Duration::from_secs(10);
        assert_eq!(f.observe(true, later), None);
        assert_eq!(f.observe(true, later), None); // count satisfied, cooldown blocks
        // After the cooldown the very next observation fires.
        let after = now + FLIP_REBUILD_COOLDOWN + Duration::from_secs(1);
        assert_eq!(f.observe(true, after), Some(true));
    }

    /// One test fn on purpose: the override assertions mutate process env
    /// and cargo runs #[test] fns in parallel threads.
    #[test]
    fn codec_factor_defaults_and_env_override() {
        // Built-in matrix (2026-07-28: HEVC 100 → 125, field-measured).
        assert_eq!(codec_rate_factor_pct("H264"), 150);
        assert_eq!(codec_rate_factor_pct("HEVC"), 125);
        assert_eq!(codec_rate_factor_pct("VP9"), 100);
        assert_eq!(codec_rate_factor_pct("AV1"), 100);

        // SAFETY: no other test in this crate touches these vars (set_var
        // is unsafe in edition 2024 because of cross-thread env races).
        unsafe { std::env::set_var("ROOMLER_AGENT_RATE_FACTOR_HEVC", "140") };
        assert_eq!(codec_rate_factor_pct("HEVC"), 140);
        // Clamped to 50–400.
        unsafe { std::env::set_var("ROOMLER_AGENT_RATE_FACTOR_HEVC", "10") };
        assert_eq!(codec_rate_factor_pct("HEVC"), 50);
        unsafe { std::env::set_var("ROOMLER_AGENT_RATE_FACTOR_HEVC", "9000") };
        assert_eq!(codec_rate_factor_pct("HEVC"), 400);
        // Garbage falls back to the built-in.
        unsafe { std::env::set_var("ROOMLER_AGENT_RATE_FACTOR_HEVC", "fast") };
        assert_eq!(codec_rate_factor_pct("HEVC"), 125);
        unsafe { std::env::remove_var("ROOMLER_AGENT_RATE_FACTOR_HEVC") };
        // Other codecs untouched by the HEVC var.
        assert_eq!(codec_rate_factor_pct("H264"), 150);
    }

    #[test]
    fn qsv_config_grants_long_gop_only_for_a_measured_honouring_mode() {
        // Unprobed → rc.219 containment.
        assert_eq!(vp9_qsv_config(None, None), (false, true));
        // Both modes honour → prefer low_power.
        assert_eq!(vp9_qsv_config(Some((true, true)), None), (true, true));
        // Only VDEnc honours.
        assert_eq!(vp9_qsv_config(Some((true, false)), None), (true, true));
        // Only VME (low_power=0) honours → long GOP on VME.
        assert_eq!(vp9_qsv_config(Some((false, true)), None), (true, false));
        // Neither honours → containment.
        assert_eq!(vp9_qsv_config(Some((false, false)), None), (false, true));
    }

    #[test]
    fn qsv_config_operator_override_wins_on_mode_but_not_on_gop_safety() {
        // Forced VME on a host whose VME honours → long GOP.
        assert_eq!(
            vp9_qsv_config(Some((false, true)), Some(false)),
            (true, false)
        );
        // Forced VDEnc on a host whose VDEnc ignores → containment GOP, VDEnc.
        assert_eq!(
            vp9_qsv_config(Some((false, true)), Some(true)),
            (false, true)
        );
        // Forced anything without a verdict → containment GOP in that mode.
        assert_eq!(vp9_qsv_config(None, Some(false)), (false, false));
        assert_eq!(vp9_qsv_config(None, Some(true)), (false, true));
    }

    #[test]
    fn h264_cq_is_two_steps_sharper_with_a_floor() {
        assert_eq!(h264_cq_adjust("h264_nvenc", 22), 20);
        assert_eq!(h264_cq_adjust("h264_qsv", 11), 10);
        assert_eq!(h264_cq_adjust("h264_amf", 10), 10);
        assert_eq!(h264_cq_adjust("hevc_qsv", 22), 22);
        assert_eq!(h264_cq_adjust("vp9_qsv", 22), 22);
    }

    fn gate() -> SettleKeyframeGate {
        SettleKeyframeGate::new(SETTLE_KF_MIN_BURST, SETTLE_KF_MIN_GAP)
    }

    #[test]
    fn caret_blink_never_fires_a_settle_keyframe() {
        // The field pattern: one real frame per caret toggle (~530 ms), a
        // settle 60 ms later, repeated forever. Pre-gate this forced ~2 IDRs
        // per second; the gate must fire ZERO.
        let mut g = gate();
        let mut now = t0();
        for _ in 0..20 {
            g.note_real_frame(); // the toggle's single damage frame
            assert_eq!(g.should_fire_on_settle(now), None); // settle +60 ms
            // keepalive ticks between toggles decide nothing further
            assert_eq!(g.should_fire_on_settle(now), None);
            now += Duration::from_millis(530);
        }
    }

    #[test]
    fn a_drag_burst_fires_exactly_once_on_the_first_settle() {
        let mut g = gate();
        let now = t0();
        for _ in 0..60 {
            g.note_real_frame(); // 1 s of real motion
        }
        assert_eq!(g.should_fire_on_settle(now), Some(60)); // first settle → IDR
        assert_eq!(g.should_fire_on_settle(now), None); // 60 ms later: nothing
        assert_eq!(g.should_fire_on_settle(now), None);
    }

    #[test]
    fn typing_trickle_below_the_burst_threshold_stays_quiet() {
        let mut g = gate();
        let now = t0();
        for _ in 0..(SETTLE_KF_MIN_BURST - 1) {
            g.note_real_frame();
        }
        assert_eq!(g.should_fire_on_settle(now), None);
        // The undersized burst is consumed, not accumulated: another small
        // trickle still doesn't reach the threshold.
        for _ in 0..(SETTLE_KF_MIN_BURST - 1) {
            g.note_real_frame();
        }
        assert_eq!(g.should_fire_on_settle(now), None);
    }

    #[test]
    fn cooldown_suppresses_a_second_burst_settle() {
        let mut g = gate();
        let now = t0();
        for _ in 0..30 {
            g.note_real_frame();
        }
        assert_eq!(g.should_fire_on_settle(now), Some(30));
        // Scroll-pause-scroll 2 s later: burst qualifies, cooldown blocks.
        let later = now + Duration::from_secs(2);
        for _ in 0..30 {
            g.note_real_frame();
        }
        assert_eq!(g.should_fire_on_settle(later), None);
        // Past the cooldown a fresh burst fires again.
        let after = now + SETTLE_KF_MIN_GAP + Duration::from_secs(1);
        for _ in 0..30 {
            g.note_real_frame();
        }
        assert_eq!(g.should_fire_on_settle(after), Some(30));
    }

    #[test]
    fn legacy_hatch_min_burst_zero_fires_every_episode() {
        // min_burst 0 + zero gap = rc.187 behaviour: first settle of EVERY
        // episode fires, keepalive ticks after it don't.
        let mut g = SettleKeyframeGate::new(0, Duration::ZERO);
        let mut now = t0();
        for _ in 0..5 {
            g.note_real_frame();
            assert!(g.should_fire_on_settle(now).is_some());
            assert_eq!(g.should_fire_on_settle(now), None); // same episode
            now += Duration::from_millis(530);
        }
    }
}

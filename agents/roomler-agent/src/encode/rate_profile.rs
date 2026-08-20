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
//!
//! 4. **Idle-settle keyframe gate** ([`SettleKeyframeGate`]): the rc.187
//!    settle IDR, burst-gated so caret blinks stop metronoming forced IDRs.
//!
//! 5. **Scale-aware CQ bias** ([`scale_cq_bias`], P7): deep resolution
//!    rungs run far below the [3, 12] Mbps maxrate floor's bpp budget —
//!    spend the headroom on text sharpness instead of leaving it unused.
//!
//! 6. **Idle native-rung refinement** ([`IdleRefine`], P7): when the ONLY
//!    reason the encode is below native is a resolution cap and the scene
//!    has settled, lift the cap so the encoder rebuilds at native and ships
//!    one crisp still; the first motion burst restores the cap in ~300 ms.

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
pub fn codec_rate_factor_pct(codec_label: &str) -> usize {
    match codec_label {
        "H264" => 150,
        _ => 100,
    }
}

/// P7 — chroma ceiling factor, composed multiplicatively with
/// [`codec_rate_factor_pct`]: 4:4:4 carries 2× the chroma samples, so give
/// it the same ×1.5 band the libvpx VP9-444 pump ships. The relay clamp
/// still applies AFTER the composed factor (pipe physics don't grow with
/// the chroma), so a relayed 4:4:4 session stays at `relay_max_bps`.
pub fn chroma_rate_factor_pct(chroma444: bool) -> usize {
    if chroma444 { 150 } else { 100 }
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

/// P7 (2026-08-20) — CQ sharpening steps for deep resolution rungs. When a
/// cap has shrunk the encode area well below native, the [3, 12] Mbps
/// maxrate floor grants 1.4-2.2× the 0.07-bpp design budget — headroom that
/// CQ-driven VBR never spends (it uses only what the quality target
/// demands). Trade it for text sharpness. Ladder on AREA ratio:
///   ≤ 32% area (~0.57 linear) → max_steps    (Smoother 1024 rung:
///                                             1920×1200→1024×640 = 28%)
///   ≤ 50% area (~0.71 linear) → max_steps/2  (Balanced relay 1280 rung:
///                                             1920×1200→1280×800 = 44%)
///   else 0 (near-native rungs already run at the design bpp).
/// NVENC/QSV quality steps cost ~7-10% bits each, so the default 4 steps ≈
/// 1.3-1.5× sustained bits — inside the floor headroom with margin, and the
/// UNCHANGED maxrate ceiling + HRD still bound the worst case (the bias can
/// only spend budget the design already allocated).
pub fn scale_cq_bias(enc_w: u32, enc_h: u32, native_w: u32, native_h: u32, max_steps: u32) -> u32 {
    let enc_area = enc_w as u64 * enc_h as u64;
    let native_area = native_w as u64 * native_h as u64;
    if enc_area == 0 || native_area == 0 || enc_area >= native_area {
        return 0;
    }
    // Integer percent avoids f32 wobble at the ladder boundaries.
    let pct = enc_area * 100 / native_area;
    if pct <= 32 {
        max_steps
    } else if pct <= 50 {
        max_steps / 2
    } else {
        0
    }
}

/// Env-resolved `max_steps` for [`scale_cq_bias`]:
/// `ROOMLER_AGENT_SCALE_CQ_BOOST`, default 4, `0` disables the bias.
pub fn scale_cq_boost_steps() -> u32 {
    tunnel_core::env::node_env("SCALE_CQ_BOOST")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(4)
}

/// Apply a CQ bias with the shared global floor (10 — below that is
/// near-lossless blow-out, see `ffmpeg_cq`). Composes with
/// [`h264_cq_adjust`]: 22 → 20 (h264) → 16 (deep rung); one shared floor.
pub fn apply_cq_bias(cq: u32, steps: u32) -> u32 {
    cq.saturating_sub(steps).max(10)
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

/// P7 (2026-08-20) — idle native-rung refinement ("crisp at rest").
///
/// The Smoother/relay resolution caps trade pixels for motion smoothness —
/// but when the user STOPS to READ, the stream stays at the low rung and
/// 9-10 pt text remains mush (the display_match.rs thesis: the only truly
/// crisp chain is 1:1 end-to-end). A settled desktop costs the link nothing
/// (the 60 ms keepalive re-encodes near-zero-byte deltas at ANY rung), so:
/// once the scene settles, lift the cap → the dims-keyed encoder rebuild
/// ships one crisp native IDR (~150-400 KB ⇒ 0.4-1.1 s progressive
/// crystallize over a 3 Mbps relay; HRD bufsize 750 KB fits it); the first
/// motion burst restores the cap within ~300 ms, before the relay melts.
///
/// Pure state machine, frame-cadence signals only — dirty-rect damage was
/// rejected because only the WGC backend populates rects (scrap/DXGI emit
/// none), so damage-based motion detection would silently misbehave on the
/// main field path. `Instant`s are passed in (the FlipTracker pattern) so
/// every behaviour below is unit-tested.
///
/// Window length for the frames-per-window rate rules. Doubles as the
/// emergent settle threshold: after motion stops, the window drains in
/// exactly this long, so the up-flip fires ~1 s after the last burst.
pub const REFINE_WINDOW: Duration = Duration::from_secs(1);

/// Real frames per window still considered "quiet" for the UP-flip. A caret
/// blink (~1.9 Hz → ≤2 frames/s) must refine; typing at ≥3 cps must not
/// (each up-flip is an encoder rebuild + native IDR — not free mid-typing).
pub const REFINE_SPARSE_MAX: u32 = 2;

/// Inter-arrival gap that CHAINS a motion run (≤80 ms ⇒ ≥12.5 fps damage —
/// a scroll/drag; typing produces 100-200 ms gaps and never chains).
pub const REFINE_MOTION_GAP: Duration = Duration::from_millis(80);

/// Chained-run length that DOWN-flips a refined session: 8 frames at
/// ≥12.5 fps ≈ 270 ms of sustained motion — fast enough that a scroll
/// doesn't melt a 3 Mbps relay with native-sized deltas.
pub const REFINE_DOWN_RUN: u32 = 8;

/// Frames-per-window rate that DOWN-flips regardless of chaining — catches
/// 80-250 ms-gap motion (10-12 fps window animations) within ≤1 s. Note the
/// asymmetry with `REFINE_SPARSE_MAX`: sustained 3-9 fps damage (typing,
/// slow spinners) neither re-refines NOR down-flips — once crisp, typing
/// stays crisp; the relay carries <10 fps of native deltas fine.
pub const REFINE_RATE_DOWN: u32 = 10;

/// Minimum spacing from ANY flip to the next UP-flip. Bounds the worst-case
/// churn (type-pause-type) to one rebuild pair per 10 s.
pub const REFINE_UP_COOLDOWN: Duration = Duration::from_secs(10);

/// What the pump should do about the resolution cap this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineFlip {
    /// Scene settled — lift the cap (encoder rebuilds at native, crisp IDR).
    Up,
    /// Motion burst — restore the cap (encoder rebuilds at the low rung).
    Down,
}

/// See the module notes above. Owned by the ffmpeg DC pump; `note_real_frame`
/// is called for every damage-carrying capture, `on_keepalive` on every idle
/// keepalive tick (≥60 ms after the last real frame by construction).
#[derive(Debug)]
pub struct IdleRefine {
    enabled: bool,
    refined: bool,
    /// Length of the current ≤`REFINE_MOTION_GAP`-chained run.
    run: u32,
    last_real: Option<Instant>,
    /// Real-frame arrivals within the trailing `REFINE_WINDOW`.
    window: std::collections::VecDeque<Instant>,
    last_flip: Option<Instant>,
}

impl IdleRefine {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            refined: false,
            run: 0,
            last_real: None,
            window: std::collections::VecDeque::new(),
            last_flip: None,
        }
    }

    /// Kill switch `ROOMLER_AGENT_IDLE_REFINE=0` (or `false`).
    pub fn from_env() -> Self {
        let enabled = !matches!(
            tunnel_core::env::node_env("IDLE_REFINE")
                .as_deref()
                .map(str::trim),
            Some("0") | Some("false")
        );
        Self::new(enabled)
    }

    /// Whether the pump should currently run WITHOUT the resolution cap.
    pub fn refined(&self) -> bool {
        self.refined
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&t) = self.window.front() {
            if now.duration_since(t) > REFINE_WINDOW {
                self.window.pop_front();
            } else {
                break;
            }
        }
    }

    /// A real (damage-carrying) frame arrived. Returns `Some(Down)` exactly
    /// when a refined session must drop back to the capped rung.
    pub fn note_real_frame(&mut self, now: Instant) -> Option<RefineFlip> {
        if !self.enabled {
            return None;
        }
        self.run = match self.last_real {
            Some(t) if now.duration_since(t) <= REFINE_MOTION_GAP => self.run.saturating_add(1),
            _ => 1,
        };
        self.last_real = Some(now);
        self.prune(now);
        // Bounded: pruning keeps this at ~fps entries; the hard cap only
        // matters if a backend ever bursts far above real time.
        if self.window.len() >= 240 {
            self.window.pop_front();
        }
        self.window.push_back(now);
        if self.refined
            && (self.run >= REFINE_DOWN_RUN || self.window.len() as u32 >= REFINE_RATE_DOWN)
        {
            self.refined = false;
            self.last_flip = Some(now);
            return Some(RefineFlip::Down);
        }
        None
    }

    /// An idle-keepalive tick. `eligible` = a cap below native is currently
    /// in force AND the scope rules allow refinement (see
    /// `encode::idle_refine_applies`; the pump also requires the controller
    /// to have left resolution at Native — an explicit pick is the user's).
    /// Returns `Some(Up)` when the cap should lift; `eligible=false` clears
    /// `refined` silently (the cap situation changed externally — e.g. the
    /// dial moved to Sharper — so there is nothing to restore).
    pub fn on_keepalive(&mut self, eligible: bool, now: Instant) -> Option<RefineFlip> {
        if !self.enabled {
            return None;
        }
        self.prune(now);
        if !eligible {
            self.refined = false;
            return None;
        }
        if self.refined {
            return None;
        }
        if self.window.len() as u32 <= REFINE_SPARSE_MAX
            && self
                .last_flip
                .is_none_or(|t| now.duration_since(t) >= REFINE_UP_COOLDOWN)
        {
            self.refined = true;
            self.last_flip = Some(now);
            return Some(RefineFlip::Up);
        }
        None
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

    #[test]
    fn codec_factor_boosts_h264_only() {
        assert_eq!(codec_rate_factor_pct("H264"), 150);
        assert_eq!(codec_rate_factor_pct("HEVC"), 100);
        assert_eq!(codec_rate_factor_pct("VP9"), 100);
        assert_eq!(codec_rate_factor_pct("AV1"), 100);
    }

    // P7 — chroma factor composes multiplicatively with the codec factor.
    #[test]
    fn chroma_factor_composes_with_codec_factor() {
        assert_eq!(chroma_rate_factor_pct(true), 150);
        assert_eq!(chroma_rate_factor_pct(false), 100);
        // HEVC 4:4:4 → 150; HEVC 4:2:0 → 100 (the pump's compose rule).
        assert_eq!(
            codec_rate_factor_pct("HEVC") * chroma_rate_factor_pct(true) / 100,
            150
        );
        assert_eq!(
            codec_rate_factor_pct("HEVC") * chroma_rate_factor_pct(false) / 100,
            100
        );
    }

    #[test]
    fn h264_cq_is_two_steps_sharper_with_a_floor() {
        assert_eq!(h264_cq_adjust("h264_nvenc", 22), 20);
        assert_eq!(h264_cq_adjust("h264_qsv", 11), 10);
        assert_eq!(h264_cq_adjust("h264_amf", 10), 10);
        assert_eq!(h264_cq_adjust("hevc_qsv", 22), 22);
        assert_eq!(h264_cq_adjust("vp9_qsv", 22), 22);
    }

    // P7 — scale-aware CQ bias ladder.
    #[test]
    fn scale_cq_bias_full_at_smoother_rung() {
        // 1920×1200 → 1024×640 = 28% area; 2560×1600 → 1024×640 = 16%.
        assert_eq!(scale_cq_bias(1024, 640, 1920, 1200, 4), 4);
        assert_eq!(scale_cq_bias(1024, 640, 2560, 1600, 4), 4);
    }

    #[test]
    fn scale_cq_bias_half_at_relay_rung() {
        // 1920×1200 → 1280×800 = 44% area.
        assert_eq!(scale_cq_bias(1280, 800, 1920, 1200, 4), 2);
    }

    #[test]
    fn scale_cq_bias_zero_near_native() {
        // Snap-native leftovers and small trims spend nothing (1836×1148 =
        // 91% area), and equal dims are exactly zero.
        assert_eq!(scale_cq_bias(1836, 1148, 1920, 1200, 4), 0);
        assert_eq!(scale_cq_bias(1920, 1200, 1920, 1200, 4), 0);
    }

    #[test]
    fn scale_cq_bias_zero_dims_and_zero_steps_safe() {
        // Unpublished native dims (0) or a disabled knob (max_steps 0)
        // must never bias.
        assert_eq!(scale_cq_bias(1024, 640, 0, 0, 4), 0);
        assert_eq!(scale_cq_bias(0, 0, 1920, 1200, 4), 0);
        assert_eq!(scale_cq_bias(1024, 640, 1920, 1200, 0), 0);
        assert_eq!(scale_cq_bias(1280, 800, 1920, 1200, 0), 0);
    }

    #[test]
    fn cq_bias_composes_with_h264_adjust_at_the_floor() {
        // Floor is shared: 14 → 12 (h264) → 10 (bias clamps at the floor).
        assert_eq!(apply_cq_bias(h264_cq_adjust("h264_nvenc", 14), 4), 10);
        // Nominal cases: 22 → 20 (h264) → 16; HEVC skips the codec adjust.
        assert_eq!(apply_cq_bias(h264_cq_adjust("h264_nvenc", 22), 4), 16);
        assert_eq!(apply_cq_bias(h264_cq_adjust("hevc_nvenc", 22), 4), 18);
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

    // ── P7 — IdleRefine ────────────────────────────────────────────────

    /// Drive `n` real frames at a fixed `gap`, asserting no Down fires.
    fn feed_quiet(r: &mut IdleRefine, mut now: Instant, n: u32, gap: Duration) -> Instant {
        for _ in 0..n {
            assert_eq!(r.note_real_frame(now), None);
            now += gap;
        }
        now
    }

    /// Keepalives every 60 ms until the first Up (or `limit` elapses);
    /// returns (time of the Up, elapsed since start).
    fn tick_until_up(
        r: &mut IdleRefine,
        mut now: Instant,
        limit: Duration,
    ) -> Option<(Instant, Duration)> {
        let start = now;
        while now.duration_since(start) <= limit {
            if r.on_keepalive(true, now) == Some(RefineFlip::Up) {
                return Some((now, now.duration_since(start)));
            }
            now += Duration::from_millis(60);
        }
        None
    }

    #[test]
    fn refine_fires_about_1s_after_a_scroll_settles() {
        let mut r = IdleRefine::new(true);
        // 1 s scroll at 30 fps.
        let now = feed_quiet(&mut r, t0(), 30, Duration::from_millis(33));
        // Keepalives start 60 ms after the last real frame; the window must
        // drain (~1 s) before the up-flip fires.
        let (_, elapsed) = tick_until_up(&mut r, now, Duration::from_secs(3)).expect("must refine");
        assert!(
            elapsed >= Duration::from_millis(700) && elapsed <= Duration::from_millis(1300),
            "settle-to-refine took {elapsed:?} (want ≈1 s)"
        );
        assert!(r.refined());
    }

    #[test]
    fn caret_blink_neither_blocks_refine_nor_downflips() {
        let mut r = IdleRefine::new(true);
        let mut now = t0();
        // Refine first (quiet from the start).
        assert_eq!(r.on_keepalive(true, now), Some(RefineFlip::Up));
        // 20 s of caret blinks (~1.9 Hz): single frames, 530 ms apart, with
        // keepalives in between — must stay refined throughout.
        for _ in 0..38 {
            assert_eq!(
                r.note_real_frame(now),
                None,
                "caret blink must not down-flip"
            );
            for k in 1..=8 {
                assert_eq!(
                    r.on_keepalive(true, now + Duration::from_millis(60 * k)),
                    None
                );
            }
            now += Duration::from_millis(530);
        }
        assert!(r.refined());
    }

    #[test]
    fn typing_trickle_blocks_upflip() {
        let mut r = IdleRefine::new(true);
        // Typing at ~6 cps: 160 ms gaps → >2 frames in every 1 s window.
        // Warm the window first (a cold ≤2-entry window refining is the
        // fresh-session behaviour, locked by first_upflip_needs_no_prior_flip).
        let mut now = t0();
        let _ = r.note_real_frame(now);
        now += Duration::from_millis(160);
        let _ = r.note_real_frame(now);
        for _ in 0..30 {
            now += Duration::from_millis(160);
            let _ = r.note_real_frame(now);
            // The keepalive between keystrokes must NOT refine (window > 2).
            assert_eq!(r.on_keepalive(true, now + Duration::from_millis(60)), None);
        }
        assert!(!r.refined());
    }

    #[test]
    fn scroll_burst_downflips_within_300ms() {
        let mut r = IdleRefine::new(true);
        let now = t0();
        assert_eq!(r.on_keepalive(true, now), Some(RefineFlip::Up));
        // 30 fps drag: the chained-run rule must fire by frame 8 (~270 ms).
        let mut t = now + Duration::from_secs(2);
        let mut fired_at = None;
        for i in 0..30 {
            if let Some(RefineFlip::Down) = r.note_real_frame(t) {
                fired_at = Some(i);
                break;
            }
            t += Duration::from_millis(33);
        }
        let frames = fired_at.expect("sustained scroll must down-flip");
        assert!(
            frames < 10,
            "down-flip took {frames} frames (want <10 ≈ 300 ms)"
        );
        assert!(!r.refined());
    }

    #[test]
    fn slow_motion_downflips_via_window_rate() {
        let mut r = IdleRefine::new(true);
        let now = t0();
        assert_eq!(r.on_keepalive(true, now), Some(RefineFlip::Up));
        // ~10.5 fps damage (95 ms gaps > the 80 ms chain gap): the run rule
        // never fires, the frames-per-window rate rule must within ≤1 s.
        let mut t = now + Duration::from_secs(2);
        let start = t;
        let mut fired = None;
        for _ in 0..40 {
            if let Some(RefineFlip::Down) = r.note_real_frame(t) {
                fired = Some(t.duration_since(start));
                break;
            }
            t += Duration::from_millis(95);
        }
        let took = fired.expect("10 fps sustained motion must down-flip");
        assert!(
            took <= Duration::from_millis(1100),
            "took {took:?} (want ≤ ~1 s)"
        );
    }

    #[test]
    fn window_animation_never_rerefines() {
        let mut r = IdleRefine::new(true);
        // A steady 5 fps spinner (200 ms gaps): >2 frames per window forever
        // → the up-flip must never fire, no matter how long it runs. Warm
        // the window past the sparse gate first (see typing test).
        let mut now = t0();
        for _ in 0..3 {
            let _ = r.note_real_frame(now);
            now += Duration::from_millis(200);
        }
        for _ in 0..100 {
            let _ = r.note_real_frame(now);
            assert_eq!(r.on_keepalive(true, now + Duration::from_millis(100)), None);
            now += Duration::from_millis(200);
        }
        assert!(!r.refined());
    }

    #[test]
    fn up_cooldown_bounds_flip_pairs() {
        let mut r = IdleRefine::new(true);
        let now = t0();
        assert_eq!(r.on_keepalive(true, now), Some(RefineFlip::Up));
        // Burst → down.
        let mut t = now + Duration::from_secs(1);
        let mut downed = false;
        for _ in 0..12 {
            if r.note_real_frame(t) == Some(RefineFlip::Down) {
                downed = true;
                break;
            }
            t += Duration::from_millis(33);
        }
        assert!(downed);
        // Quiet again immediately: the window drains in 1 s but the 10 s
        // up-cooldown must hold the next Up until ≥10 s after the Down.
        let up = tick_until_up(
            &mut r,
            t + Duration::from_millis(60),
            Duration::from_secs(15),
        )
        .expect("must eventually re-refine");
        assert!(
            up.0.duration_since(t) >= REFINE_UP_COOLDOWN,
            "re-refined {:?} after the down-flip (cooldown {:?})",
            up.0.duration_since(t),
            REFINE_UP_COOLDOWN
        );
    }

    #[test]
    fn eligible_false_clears_refined_silently() {
        let mut r = IdleRefine::new(true);
        let now = t0();
        assert_eq!(r.on_keepalive(true, now), Some(RefineFlip::Up));
        assert!(r.refined());
        // Dial moved to Sharper (no cap to lift): silent clear, no Down.
        assert_eq!(r.on_keepalive(false, now + Duration::from_millis(60)), None);
        assert!(!r.refined());
    }

    #[test]
    fn disabled_never_flips() {
        let mut r = IdleRefine::new(false);
        let mut now = t0();
        for _ in 0..50 {
            assert_eq!(r.on_keepalive(true, now), None);
            assert_eq!(r.note_real_frame(now), None);
            now += Duration::from_millis(60);
        }
        assert!(!r.refined());
    }

    #[test]
    fn first_upflip_needs_no_prior_flip() {
        // A session that starts quiet refines on the very first keepalive —
        // the cooldown only spaces SUBSEQUENT flips.
        let mut r = IdleRefine::new(true);
        assert_eq!(r.on_keepalive(true, t0()), Some(RefineFlip::Up));
    }
}

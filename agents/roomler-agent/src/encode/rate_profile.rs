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
pub fn codec_rate_factor_pct(codec_label: &str) -> usize {
    match codec_label {
        "H264" => 150,
        _ => 100,
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

    #[test]
    fn h264_cq_is_two_steps_sharper_with_a_floor() {
        assert_eq!(h264_cq_adjust("h264_nvenc", 22), 20);
        assert_eq!(h264_cq_adjust("h264_qsv", 11), 10);
        assert_eq!(h264_cq_adjust("h264_amf", 10), 10);
        assert_eq!(h264_cq_adjust("hevc_qsv", 22), 22);
        assert_eq!(h264_cq_adjust("vp9_qsv", 22), 22);
    }
}

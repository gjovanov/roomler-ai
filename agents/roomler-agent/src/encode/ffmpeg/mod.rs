//! FFmpeg-based encoder backend (rc.66 onwards).
//!
//! Wraps `ffmpeg-next` to dispatch HEVC + H.264 + VP9 to vendor HW
//! encoders (NVENC, Intel QSV / oneVPL, AMD AMF) without going through
//! Windows Media Foundation. Bypasses two confirmed MF bugs:
//!
//! 1. `ActivateObject` returns 0x8000FFFF on RTX 5090 Blackwell for all
//!    HEVC / H.264 / AV1 MFTs (Known Issues, CLAUDE.md).
//! 2. Intel Iris Xe HEVC MFT is async-only on Tiger Lake; our MF cascade
//!    does not handle the async-MFT path correctly.
//!
//! As a bonus, FFmpeg's `vp9_qsv` encoder unlocks HW VP9 on Intel iGPUs
//! (unavailable via MF at all).
//!
//! ## Phased rollout (Option B plan)
//!
//! - rc.64: module declared, `available()` returns false. No FFmpeg
//!   link, no CI plumbing. Safe back-out window.
//! - **rc.65 (this RC)**: `ffmpeg-next` 8.1 added as optional dep gated
//!   on the `ffmpeg-encoder` feature. CI Windows job installs FFmpeg
//!   via vcpkg + sets PKG_CONFIG_PATH so libavcodec links into the
//!   Windows MSI. `linked_libavcodec_version()` exercises a symbol from
//!   the dep so the linker can't dead-strip the FFmpeg lib. Test asserts
//!   the version constant is in the FFmpeg 7+ range, locking the
//!   binding ↔ FFmpeg version pair.
//! - rc.66: `FfmpegEncoder` implements `VideoEncoder` trait. D3D11VA
//!   zero-copy from day 1 (critique fix). Behind
//!   `ROOMLER_AGENT_USE_FFMPEG=1` env var; MF cascade still default.
//! - rc.67: `caps::detect` advertises `data-channel-hevc`; new
//!   `hevc_dc_framer.rs` reuses 13-byte header from VP9-444 path.
//!   Anti-IDR-storm coalescer included (critique fix).
//! - rc.68: browser HEVC worker (pre-flight spike confirmed Annex-B
//!   no-description round-trip).
//! - rc.69: `vp9_qsv` HW path for Intel iGPU.
//! - rc.70: single codec-selector dropdown.
//! - rc.71+: AIMD tuning + field hotfixes.
//! - rc.74: Linux VAAPI.
//! - rc.75: macOS VideoToolbox.
//!
//! Pre-flight spike + design lock memorialised in
//! `~/.claude/projects/.../memory/project_hevc_webcodecs_go.md`.

/// rc.65 link probe. Returns libavcodec's compile-time version (e.g.
/// `0x3D6364` for libavcodec 61.99.100 = FFmpeg 7.1) by calling into
/// `ffmpeg-next`'s thin FFI re-export. The function exists for one
/// purpose: force the linker to keep the FFmpeg static libs in the
/// final binary so a missing or version-mismatched FFmpeg fails at
/// link time, not at the rc.66 encoder construction time.
///
/// Encoded as `(major << 16) | (minor << 8) | micro`. FFmpeg 7.x uses
/// libavcodec 61.x; FFmpeg 8.x uses libavcodec 62.x. We assert >= 61
/// in the test so a binding regression to FFmpeg 6.x fails CI.
#[cfg(feature = "ffmpeg-encoder")]
pub fn linked_libavcodec_version() -> u32 {
    // Safety: avcodec_version is a pure C function returning a u32
    // constant set at libavcodec build time; no state, no allocation.
    unsafe { ffmpeg_next::ffi::avcodec_version() }
}

/// rc.66 placeholder. Returns `false` until the real `FfmpegEncoder`
/// lands. Callers compile + link against the `ffmpeg-encoder` feature
/// today (so CI exercises the FFmpeg link); they just don't yet route
/// frames through this backend.
pub fn available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_returns_false_until_rc66() {
        assert!(
            !available(),
            "rc.65 ships only the link probe; rc.66 flips this to true"
        );
    }

    /// rc.65 link verification. Exercises the FFmpeg dep so a missing
    /// or symbol-broken link fails locally + in CI before any encoder
    /// code runs. Asserts libavcodec major >= 61 (FFmpeg 7.0+), which
    /// is the version pair we depend on for `hevc_qsv` + `vp9_qsv`
    /// being reachable.
    #[cfg(feature = "ffmpeg-encoder")]
    #[test]
    fn libavcodec_version_is_ffmpeg_7_or_newer() {
        let v = linked_libavcodec_version();
        let major = (v >> 16) & 0xFF;
        assert!(
            major >= 61,
            "libavcodec {} is too old; need FFmpeg 7+ (libavcodec 61+) for hevc_qsv + vp9_qsv. \
             Raw version constant: 0x{:06X}",
            major,
            v
        );
    }
}

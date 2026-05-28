//! FFmpeg-based encoder backend (rc.72 onwards).
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
//! ## Phased rollout (Option B plan, +7 shifted after vcpkg hotfixes)
//!
//! - rc.64–rc.71: scaffolding + CI plumbing (vcpkg ffmpeg install,
//!   ffmpeg-next link verification, DLL staging — 7 RCs of CI iteration
//!   to land a working FFmpeg 8.1.1 bundle in the agent MSI).
//! - **rc.72 (this RC)**: `FfmpegEncoder` lands in `encoder.rs`. Hooks
//!   into `open_for_codec_hevc` behind `ROOMLER_AGENT_USE_FFMPEG=1` env
//!   var. CPU BGRA→NV12 conversion via dcv_color_primitives. MF cascade
//!   still default.
//! - rc.73: D3D11VA zero-copy (capture's D3D11 texture fed directly to
//!   NVENC/QSV/AMF without CPU readback).
//! - rc.74: caps probe + HEVC DC framer + anti-IDR-storm coalescer.
//! - rc.75: browser HEVC worker.
//! - rc.76+: vp9_qsv, codec dropdown, AIMD, Linux/macOS.
//!
//! Pre-flight WebCodecs spike (2026-05-26) locked the wire design:
//! Annex-B + 4-byte start codes + no decoder description. See
//! `~/.claude/projects/.../memory/project_hevc_webcodecs_go.md`.

#[cfg(feature = "ffmpeg-encoder")]
pub mod encoder;

#[cfg(feature = "ffmpeg-encoder")]
pub use encoder::FfmpegEncoder;

/// rc.66 link probe. Returns libavcodec's compile-time version (e.g.
/// `0x3E0000` for libavcodec 62.x = FFmpeg 8.1). Forces the linker to
/// keep the FFmpeg static libs in the final binary so a missing or
/// version-mismatched FFmpeg fails at link time, not at encoder
/// construction time.
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

/// Whether the FFmpeg encoder backend is opted-in for this process.
/// Set `ROOMLER_AGENT_USE_FFMPEG=1` to enable (or `true`, `yes`, `on`).
/// Any other value (or unset) leaves the MF cascade as the default.
///
/// rc.72: only `open_for_codec_hevc` consults this. H.264 and AV1 paths
/// stay on MF. rc.73+ extends to AV1 once D3D11VA zero-copy lands.
#[cfg(feature = "ffmpeg-encoder")]
pub fn ffmpeg_backend_enabled() -> bool {
    std::env::var("ROOMLER_AGENT_USE_FFMPEG")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// rc.72 entrypoint: returns true when the FFmpeg backend is opted in
/// AND a real `FfmpegEncoder` is reachable. `available()` is consulted
/// by `encode::open_for_codec_hevc` to decide whether to try the FFmpeg
/// path before falling through to MF.
///
/// In the `#[cfg(not(feature = "ffmpeg-encoder"))]` build, this always
/// returns false — callers compile against the same signature so the
/// dispatch site doesn't need extra cfg gates.
#[cfg(feature = "ffmpeg-encoder")]
pub fn available() -> bool {
    ffmpeg_backend_enabled()
}

#[cfg(not(feature = "ffmpeg-encoder"))]
pub fn available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_false_when_env_unset() {
        // SAFETY: tests share the process env; this module is the only
        // one touching ROOMLER_AGENT_USE_FFMPEG so no concurrent reads.
        unsafe { std::env::remove_var("ROOMLER_AGENT_USE_FFMPEG") };
        assert!(
            !available(),
            "available() must default off so MF cascade stays the default"
        );
    }

    #[cfg(feature = "ffmpeg-encoder")]
    #[test]
    fn ffmpeg_backend_enabled_reads_truthy_values() {
        unsafe { std::env::remove_var("ROOMLER_AGENT_USE_FFMPEG") };
        assert!(!ffmpeg_backend_enabled(), "unset → off");
        for truthy in ["1", "true", "TRUE", "yes", "On"] {
            unsafe { std::env::set_var("ROOMLER_AGENT_USE_FFMPEG", truthy) };
            assert!(
                ffmpeg_backend_enabled(),
                "value {truthy:?} should enable the FFmpeg backend"
            );
        }
        for falsy in ["0", "false", "no", "off", ""] {
            unsafe { std::env::set_var("ROOMLER_AGENT_USE_FFMPEG", falsy) };
            assert!(
                !ffmpeg_backend_enabled(),
                "value {falsy:?} should leave MF cascade as default"
            );
        }
        unsafe { std::env::remove_var("ROOMLER_AGENT_USE_FFMPEG") };
    }

    /// rc.66 link verification. Exercises the FFmpeg dep so a missing
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

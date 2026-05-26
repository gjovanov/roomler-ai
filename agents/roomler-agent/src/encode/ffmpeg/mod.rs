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
//! - rc.64 (this RC): module declared, `available()` returns false. No
//!   FFmpeg link, no CI plumbing. Safe back-out window.
//! - rc.65: vcpkg stripped-FFmpeg install in `release-agent.yml`,
//!   verify `ffmpeg-next` 7.x compiles against FFmpeg 7.1 with our
//!   target symbols (`hevc_qsv`, `vp9_qsv`, etc.) reachable.
//! - rc.66: `FfmpegEncoder` implements `VideoEncoder` trait. D3D11VA
//!   zero-copy from day 1 (critique fix — was deferred too late in v1).
//!   Behind `ROOMLER_AGENT_USE_FFMPEG=1` env var; MF cascade still
//!   default.
//! - rc.67: `caps::detect` advertises `data-channel-hevc` when probe
//!   succeeds; new `hevc_dc_framer.rs` reuses 13-byte header from
//!   VP9-444 path. Anti-IDR-storm coalescer included (critique fix).
//! - rc.68: browser HEVC worker decodes the DC bytes via WebCodecs
//!   (pre-flight spike on 2026-05-26 confirmed Annex-B no-description
//!   round-trip works in Chrome + Edge).
//! - rc.69: `vp9_qsv` HW path for Intel iGPU. NVIDIA/AMD stay libvpx
//!   SW for VP9 (NVENC/AMF never added VP9 encode).
//! - rc.70: single codec-selector dropdown collapses chroma + codec.
//! - rc.71+: AIMD tuning, codec-aware bitrate floors, field hotfixes.
//! - rc.74: Linux VAAPI via `hevc_vaapi`.
//! - rc.75: macOS VideoToolbox via `hevc_videotoolbox`.
//!
//! Pre-flight spike + design lock memorialised in
//! `~/.claude/projects/.../memory/project_hevc_webcodecs_go.md`.

/// rc.64 placeholder. Returns `false` until rc.66 lands the real
/// backend. Lets callers compile and link against the
/// `ffmpeg-encoder` feature without yet depending on any
/// `ffmpeg-next` symbol — the dep is declared optional in Cargo.toml
/// for the same reason.
pub fn available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_returns_false_in_rc64() {
        assert!(
            !available(),
            "rc.64 ships header-only; rc.66 flips this to true"
        );
    }
}

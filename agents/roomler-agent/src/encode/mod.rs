//! Video encoder abstraction.
//!
//! Encoders consume `capture::Frame` values and produce NAL-unit-delimited
//! byte runs ready to feed into a WebRTC `TrackLocalStaticSample`.
//!
//! Backends are feature-gated so the agent builds on any host without
//! dragging in their system deps:
//!
//! - `openh264-encoder` → [`openh264_backend::Openh264Encoder`] (software)
//!
//! Future backends: `nvenc` / `qsv` / `vaapi` / `videotoolbox` / `mf`.

use std::sync::Arc;

use anyhow::Result;

use crate::capture::{DirtyRect, Frame};

pub mod caps;
pub mod color;

#[cfg(feature = "openh264-encoder")]
pub mod openh264_backend;

#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
pub mod mf;

// ---------------------------------------------------------------------
// Shared helpers usable by every backend.
// ---------------------------------------------------------------------

/// Resolution-scaled initial bitrate target.
///
/// A fixed bitrate across all sizes (which 0.1.10 used at 8 Mbps) is
/// either overkill or underkill at any resolution other than the one it
/// was tuned for; we derive from dims × fps × bpp/s. Desktop-content
/// bpp/s bumped to 0.15 in the RustDesk-parity sprint: we measured
/// RustDesk at ~0.14 bpp/s and decided perceptual parity on fine text
/// trumps a 30% bandwidth save. At 60 fps 1080p that's ≈18.7 Mbps
/// uncapped, which the 25 Mbps MAX now accommodates.
///
/// MAX bumped 15→25 Mbps so 4K60 HEVC isn't permanently clipped on
/// LAN/gigabit links. Adaptive bitrate driven by REMB still pulls the
/// effective bitrate down under congestion; this value is a ceiling,
/// not a target.
#[cfg_attr(
    not(any(
        feature = "openh264-encoder",
        all(target_os = "windows", feature = "mf-encoder")
    )),
    allow(dead_code)
)]
pub(crate) fn initial_bitrate_for(width: u32, height: u32) -> u32 {
    initial_bitrate_for_fps(width, height, 30)
}

/// Like `initial_bitrate_for` but parameterised on fps. Backends that
/// know their target rate (peer.rs sets it per-session via
/// target_fps_for) pass their real value; the default-30 form above is
/// kept for call sites that don't have fps in scope.
#[cfg_attr(
    not(any(
        feature = "openh264-encoder",
        all(target_os = "windows", feature = "mf-encoder")
    )),
    allow(dead_code)
)]
/// Legibility floor — below this bitrate, heavy codecs (HEVC / AV1) at
/// 1080p produce green chroma artefacts and unreadable terminal text
/// (2026-04-24 field report). Consulted by peer.rs as the REMB-safety
/// minimum so a collapsing REMB signal can't drop encode quality into
/// unusability while the link is still technically up.
pub const MIN_BITRATE_BPS: u32 = 1_500_000;
pub const MAX_BITRATE_BPS: u32 = 25_000_000;

pub(crate) fn initial_bitrate_for_fps(width: u32, height: u32, fps: u32) -> u32 {
    const DESKTOP_BPP_PER_SECOND: f64 = 0.15;
    let pixels = width as f64 * height as f64;
    let raw = (pixels * fps as f64 * DESKTOP_BPP_PER_SECOND) as u32;
    raw.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS)
}

#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    pub duration_us: u64,
}

#[async_trait::async_trait]
pub trait VideoEncoder: Send {
    /// Takes `Arc<Frame>` so the media_pump's last-good-frame cache can
    /// share ownership with the encode call without cloning the BGRA
    /// buffer (up to 33 MB at 4K, 8 MB at 1080p). The backend reads the
    /// frame and doesn't need to mutate it.
    async fn encode(&mut self, frame: Arc<Frame>) -> Result<Vec<EncodedPacket>>;
    /// Force the next frame to be a keyframe (IDR).
    fn request_keyframe(&mut self);
    /// Dynamically adjust bitrate in response to TWCC/REMB feedback.
    fn set_bitrate(&mut self, bps: u32);
    /// Recover from packet loss by invalidating the previous frame as
    /// a reference and forcing the next frame to be intra-coded
    /// (without necessarily being a full IDR). Default impl falls
    /// back to `request_keyframe`, which is correct but heavier
    /// (an IDR at 1080p is 60-100 KB vs an intra-refresh slice at
    /// ~5-15 KB). Backends that expose intra-only / non-IDR controls
    /// (NVENC's reference-frame invalidation, openh264's slice-level
    /// intra) can override to send a smaller recovery frame and
    /// avoid the bitrate spike that plays badly with congestion
    /// control. `lost_frame_number` is the RTP sequence number that
    /// was reported lost, for backends that want to invalidate a
    /// specific past frame as the reference.
    fn request_reference_invalidation(&mut self, lost_frame_number: u32) {
        let _ = lost_frame_number;
        self.request_keyframe();
    }
    /// Hint at per-region encoding priority for the next encoded
    /// frame. `rects` are the regions that changed since the previous
    /// frame; backends that expose ROI delta-QP (NVENC ROI maps,
    /// VideoToolbox attachments) should give those regions a low
    /// (high-quality) QP and the unchanged macroblocks a high
    /// (low-bitrate) QP. The single biggest efficiency lever for
    /// desktop content per `docs/streaming-options.md` §5.1 — typical
    /// idle desktops drop 5-10× in bandwidth at the same perceived
    /// quality. `frame_dims` is the encode resolution (post-downscale)
    /// so backends can clip rects to the encoder grid.
    ///
    /// Default impl is a no-op. openh264 0.9.3 has no public ROI hook;
    /// MF + windows 0.58 only exposes `AVEncVideoROIEnabled` boolean
    /// (the per-frame map setter sits behind a non-exported GUID),
    /// so MF override today is also no-op-with-debug-log. Real ROI
    /// landed in HW backends will plug in here without touching the
    /// caller.
    fn set_roi_hints(&mut self, rects: &[DirtyRect], frame_dims: (u32, u32)) {
        let _ = (rects, frame_dims);
    }
    /// Stable name for logging, e.g. `"openh264"`, `"nvenc-h264"`.
    fn name(&self) -> &'static str;

    /// Whether this backend is running on dedicated video-encode
    /// hardware (NVENC, QSV, AMF, Apple VideoToolbox). Defaults to
    /// `false` — only the MF path overrides when the cascade lands
    /// on a HW MFT. Callers use this to decide whether to apply the
    /// auto-downscale fallback: a SW HEVC encoder at 4K on an iGPU
    /// box can't sustain 30 fps, and forcing Fit@1080p is a much
    /// better default than asking the operator to notice and fix it.
    fn is_hardware(&self) -> bool {
        false
    }
}

pub struct NoopEncoder;

#[async_trait::async_trait]
impl VideoEncoder for NoopEncoder {
    async fn encode(&mut self, _frame: Arc<Frame>) -> Result<Vec<EncodedPacket>> {
        Ok(Vec::new())
    }
    fn request_keyframe(&mut self) {}
    fn set_bitrate(&mut self, _bps: u32) {}
    fn request_reference_invalidation(&mut self, _lost_frame_number: u32) {}
    fn set_roi_hints(&mut self, _rects: &[DirtyRect], _frame_dims: (u32, u32)) {}
    fn name(&self) -> &'static str {
        "noop"
    }
}

/// Operator preference for encoder selection. Defaults to `Auto` which
/// picks the fastest working backend: MF on Windows when available, else
/// openh264, else Noop. `Hardware` forces HW first and falls back to SW;
/// `Software` forces openh264 and never tries HW. Mostly a debug/escape-
/// hatch for drivers with known artefacts at our target bitrates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EncoderPreference {
    #[default]
    Auto,
    Hardware,
    Software,
}

impl std::str::FromStr for EncoderPreference {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(Self::Auto),
            "hardware" | "hw" | "mf" => Ok(Self::Hardware),
            "software" | "sw" | "openh264" => Ok(Self::Software),
            other => Err(format!("unknown encoder preference: {other:?}")),
        }
    }
}

/// Open the best-available encoder for the given input size.
///
/// Selection cascade:
///
/// | Preference | Order tried                                                 |
/// |------------|-------------------------------------------------------------|
/// | Auto       | mf (Windows with mf-encoder feature) → openh264 → Noop      |
/// | Hardware   | mf (required on Windows) → openh264 → Noop                  |
/// | Software   | openh264 → Noop                                             |
///
/// Auto now prefers MF-HW on Windows thanks to the probe-and-rollback
/// cascade in commit 1A.1 (adapter × MFT enumeration + single-frame
/// probe) — the failure modes that demoted MF from Auto in 0.1.25
/// (rate-control overshoot on the SW MFT, NVENC activation without
/// adapter matching, QSV async-only starvation) are all handled:
/// the SW MFT's async delegation is caught by blanket
/// MF_TRANSFORM_ASYNC_UNLOCK, adapter-bound D3D devices let NVENC
/// bind to the right GPU, and async-only MFTs route to the async
/// pipeline (commit 1A.2) or get skipped cleanly. The final fallback
/// inside the cascade is still the default-adapter SW MFT, so any
/// box with a working CLSID_MSH264EncoderMFT produces output.
///
/// Escape hatch: setting `ROOMLER_AGENT_HW_AUTO=0` reverts Auto to
/// openh264-first (for diagnosing regressions in the field without
/// a rebuild). `--encoder software` and `encoder_preference=software`
/// still force openh264 unconditionally.
///
/// Each fallback is logged; the picked backend reports via
/// `.name()` so pump-level observability can attribute.
pub fn open_default(
    width: u32,
    height: u32,
    preference: EncoderPreference,
) -> Box<dyn VideoEncoder> {
    // Auto prefers MF-HW on Windows unless the operator flips the
    // escape hatch. Hardware always tries MF first regardless. Software
    // skips MF entirely.
    let try_mf_first = match preference {
        EncoderPreference::Hardware => true,
        EncoderPreference::Auto => !hw_auto_disabled(),
        EncoderPreference::Software => false,
    };

    if try_mf_first {
        #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
        {
            match mf::MfEncoder::new(width, height) {
                Ok(e) => {
                    tracing::info!(
                        width,
                        height,
                        preference = ?preference,
                        "encoder selected: mf-h264 (hardware)"
                    );
                    return Box::new(e);
                }
                Err(e) => {
                    tracing::warn!(
                        %e,
                        "mf-encoder init failed — falling back to openh264"
                    );
                }
            }
        }
        #[cfg(not(all(target_os = "windows", feature = "mf-encoder")))]
        {
            if preference == EncoderPreference::Hardware {
                tracing::warn!(
                    "Hardware encoder requested but this build has no HW backend \
                     compiled in (rebuild with --features mf-encoder on Windows); \
                     falling back to software"
                );
            }
            // On Auto with no mf-encoder feature, fall through silently —
            // openh264 is the expected default for Linux/macOS and for
            // Windows builds that didn't opt into MF.
        }
    } else if preference == EncoderPreference::Auto {
        tracing::info!(
            "ROOMLER_AGENT_HW_AUTO=0 — skipping MF-HW on Auto, going straight to openh264"
        );
    }

    #[cfg(feature = "openh264-encoder")]
    {
        match openh264_backend::Openh264Encoder::new(width, height) {
            Ok(e) => {
                tracing::info!(width, height, "encoder selected: openh264 (software)");
                return Box::new(e);
            }
            Err(e) => tracing::warn!(%e, "openh264 init failed — falling back to NoopEncoder"),
        }
    }
    #[cfg(not(feature = "openh264-encoder"))]
    {
        let _ = (width, height);
        tracing::info!(
            "built without openh264-encoder feature — using NoopEncoder. \
             Rebuild with `--features openh264-encoder` (or `--features media`)."
        );
    }
    Box::new(NoopEncoder)
}

/// Open a codec-specific encoder, falling back to H.264 if the
/// requested codec has no compiled-in backend on this host.
///
/// `codec` is the MIME-style short name from
/// `caps::pick_best_codec` (`"h264"`, `"h265"`, `"av1"`, etc.).
/// Today only `"h264"` and `"h265"` have encoder backends; anything
/// else demotes to H.264 with a warning so the session still works
/// (the browser negotiated H.264 too, that's the universal default).
///
/// H.265 path: gated on `target_os = "windows"` + `mf-encoder` feature.
/// The HEVC cascade is HW-only (Windows ships no software HEVC encoder
/// CLSID); on failure we fall back to `open_default` which still walks
/// the H.264 cascade + openh264 fallback. The browser is already told
/// (via `set_codec_preferences`) which codec to expect — demotion at
/// this layer means the peer must re-advertise H.264 in the SDP
/// answer, which the caller in `peer.rs` handles.
pub fn open_for_codec(
    codec: &str,
    width: u32,
    height: u32,
    preference: EncoderPreference,
) -> (Box<dyn VideoEncoder>, &'static str) {
    let normalised = codec.to_ascii_lowercase();
    match normalised.as_str() {
        "av1" => open_for_codec_av1(width, height),
        "h265" | "hevc" => open_for_codec_hevc(width, height),
        _ => {
            if normalised != "h264" {
                tracing::warn!(
                    codec = %normalised,
                    "encoder: unknown codec — defaulting to H.264 (may not match negotiated track)"
                );
            }
            (open_default(width, height, preference), "h264")
        }
    }
}

/// AV1 opener, factored out so the `#[cfg]` branches don't clutter the
/// main match. See `open_for_codec` for fail-closed reasoning — when
/// AV1 init fails we return a `NoopEncoder` rather than demoting to
/// HEVC/H.264 bytes because the track is already bound to `video/AV1`
/// in the peer and substituting a different codec's bitstream would
/// produce decoder garbage on the other end.
fn open_for_codec_av1(width: u32, height: u32) -> (Box<dyn VideoEncoder>, &'static str) {
    #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
    {
        match mf::MfEncoder::new_av1(width, height) {
            Ok(e) => {
                tracing::info!(width, height, "encoder selected: mf-av1 (hardware)");
                (Box::new(e), "av1")
            }
            Err(e) => {
                tracing::warn!(
                    %e,
                    "mf-av1 init failed; track is bound to video/AV1 so no bitstream demotion is safe. Session will have no video until reconnect with a lower Quality preference."
                );
                (Box::new(NoopEncoder), "av1")
            }
        }
    }
    #[cfg(not(all(target_os = "windows", feature = "mf-encoder")))]
    {
        let _ = (width, height);
        tracing::warn!(
            "AV1 requested but this build has no MF AV1 backend — session will have no video until reconnect with a lower Quality preference."
        );
        (Box::new(NoopEncoder), "av1")
    }
}

/// HEVC opener — same fail-closed semantics as `open_for_codec_av1`.
fn open_for_codec_hevc(width: u32, height: u32) -> (Box<dyn VideoEncoder>, &'static str) {
    #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
    {
        match mf::MfEncoder::new_hevc(width, height) {
            Ok(e) => {
                tracing::info!(width, height, "encoder selected: mf-h265 (hardware)");
                (Box::new(e), "h265")
            }
            Err(e) => {
                tracing::warn!(
                    %e,
                    "mf-h265 init failed; track is bound to video/HEVC so no bitstream demotion is safe. Session will have no video until reconnect with a lower Quality preference."
                );
                (Box::new(NoopEncoder), "h265")
            }
        }
    }
    #[cfg(not(all(target_os = "windows", feature = "mf-encoder")))]
    {
        let _ = (width, height);
        tracing::warn!(
            "HEVC requested but this build has no MF HEVC backend — session will have no video until reconnect with a lower Quality preference."
        );
        (Box::new(NoopEncoder), "h265")
    }
}

/// Check the `ROOMLER_AGENT_HW_AUTO` escape hatch. Any value equal to
/// `"0"`, `"false"`, `"no"`, or `"off"` (case-insensitive) disables the
/// MF-HW-first branch of the Auto cascade. Unset or any other value
/// leaves the default (MF-HW first) in place.
fn hw_auto_disabled() -> bool {
    std::env::var("ROOMLER_AGENT_HW_AUTO")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hw_auto_disabled_reads_env() {
        // Race-free: set → read → unset. Tests share the process env,
        // so avoid overlapping with other tests that touch the same
        // var (none today).
        // SAFETY: set_var/remove_var are unsafe in Rust 2024 because
        // concurrent reads from other threads can race. Our test suite
        // is single-threaded in practice (cargo test default is
        // parallel but this module has one test) and no other code in
        // this crate touches ROOMLER_AGENT_HW_AUTO at test time.
        unsafe { std::env::remove_var("ROOMLER_AGENT_HW_AUTO") };
        assert!(!hw_auto_disabled(), "unset defaults to MF-first");
        for truthy in ["0", "false", "FALSE", "No", "off"] {
            unsafe { std::env::set_var("ROOMLER_AGENT_HW_AUTO", truthy) };
            assert!(
                hw_auto_disabled(),
                "value {truthy:?} should disable the MF-first branch"
            );
        }
        for enabled in ["1", "true", "yes", "on", ""] {
            unsafe { std::env::set_var("ROOMLER_AGENT_HW_AUTO", enabled) };
            assert!(
                !hw_auto_disabled(),
                "value {enabled:?} should leave MF-first active"
            );
        }
        unsafe { std::env::remove_var("ROOMLER_AGENT_HW_AUTO") };
    }
}

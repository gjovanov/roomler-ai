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

use crate::capture::Frame;

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
/// was tuned for; we derive from dims. Adaptive bitrate based on
/// TWCC/REMB remains future work — this just picks a better *starting
/// point*. Desktop-content target = 0.07 bpp/s gives ≈6 Mbps at 1080p
/// and ≈10 Mbps at 1440p; 4K clamps to MAX.
#[cfg_attr(
    not(any(
        feature = "openh264-encoder",
        all(target_os = "windows", feature = "mf-encoder")
    )),
    allow(dead_code)
)]
pub(crate) fn initial_bitrate_for(width: u32, height: u32) -> u32 {
    const MIN_BITRATE_BPS: u32 = 1_000_000;
    const MAX_BITRATE_BPS: u32 = 12_000_000;
    const DESKTOP_BPP_PER_SECOND: f64 = 0.07;
    const FPS: f64 = 30.0;
    let pixels = width as f64 * height as f64;
    let raw = (pixels * FPS * DESKTOP_BPP_PER_SECOND) as u32;
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
    /// Stable name for logging, e.g. `"openh264"`, `"nvenc-h264"`.
    fn name(&self) -> &'static str;
}

pub struct NoopEncoder;

#[async_trait::async_trait]
impl VideoEncoder for NoopEncoder {
    async fn encode(&mut self, _frame: Arc<Frame>) -> Result<Vec<EncodedPacket>> {
        Ok(Vec::new())
    }
    fn request_keyframe(&mut self) {}
    fn set_bitrate(&mut self, _bps: u32) {}
    fn name(&self) -> &'static str { "noop" }
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
/// | Preference | Order tried                                              |
/// |------------|----------------------------------------------------------|
/// | Auto       | openh264 → Noop   (MF is opt-in until phase 3 lands)     |
/// | Hardware   | mf (required on Windows) → openh264 → Noop               |
/// | Software   | openh264 → Noop                                          |
///
/// MF is demoted from the Auto path because on mixed-GPU systems
/// (NVIDIA + Intel iGPU) the MS SW MFT produces catastrophic frame
/// sizes (20+ Mbps where 4 Mbps was requested — rate-control config
/// is silently ignored) and the HW MFT path needs adapter-matching
/// and an async event loop (phase 3). openh264 with LowDelay /
/// max_frame_rate=30 has worked consistently across every host
/// we've tested. Users who want to experiment with the MF path can
/// set `encoder_preference=hardware` via CLI/env/config.
///
/// Each fallback is logged; the picked backend reports via
/// `.name()` so pump-level observability can attribute.
pub fn open_default(
    width: u32,
    height: u32,
    preference: EncoderPreference,
) -> Box<dyn VideoEncoder> {
    if preference == EncoderPreference::Hardware {
        #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
        {
            match mf::MfEncoder::new(width, height) {
                Ok(e) => {
                    tracing::info!(
                        width,
                        height,
                        "encoder selected: mf-h264 (hardware — experimental)"
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
            tracing::warn!(
                "Hardware encoder requested but this build has no HW backend \
                 compiled in (rebuild with --features mf-encoder on Windows); \
                 falling back to software"
            );
        }
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

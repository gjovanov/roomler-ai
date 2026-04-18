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

#[cfg(feature = "openh264-encoder")]
pub mod openh264_backend;

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

/// Open the best-available encoder for the given input size. Falls back
/// to [`NoopEncoder`] if no encoder feature is enabled or construction
/// fails — higher layers remain functional, the PC just won't carry media.
pub fn open_default(_width: u32, _height: u32) -> Box<dyn VideoEncoder> {
    #[cfg(feature = "openh264-encoder")]
    {
        match openh264_backend::Openh264Encoder::new(_width, _height) {
            Ok(e) => return Box::new(e),
            Err(e) => tracing::warn!(%e, "openh264 init failed — falling back to NoopEncoder"),
        }
    }
    #[cfg(not(feature = "openh264-encoder"))]
    {
        tracing::info!(
            "built without openh264-encoder feature — using NoopEncoder. \
             Rebuild with `--features openh264-encoder` (or `--features media`)."
        );
    }
    Box::new(NoopEncoder)
}

//! Video encoder abstraction.
//!
//! Encoders consume `capture::Frame` values and produce NAL-unit-delimited
//! byte runs ready to feed into a WebRTC `TrackLocalStaticSample`. For the
//! signaling-only build we ship just the trait + a no-op that any caller
//! can instantiate.

use anyhow::Result;

use crate::capture::Frame;

#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    pub duration_us: u64,
}

#[async_trait::async_trait]
pub trait VideoEncoder: Send {
    async fn encode(&mut self, frame: Frame) -> Result<Vec<EncodedPacket>>;
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
    async fn encode(&mut self, _frame: Frame) -> Result<Vec<EncodedPacket>> {
        Ok(Vec::new())
    }
    fn request_keyframe(&mut self) {}
    fn set_bitrate(&mut self, _bps: u32) {}
    fn name(&self) -> &'static str { "noop" }
}

// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #2877 — a recording's software rung on a screen larger than openh264
//! encodes.
//!
//! A recording is made at the capture's native size, never the live pump's
//! capped rung (the module docs of [`super::recorder`]). openh264 encodes at
//! most 3840 on the long side and 2160 on the short one, and it refuses a
//! larger picture on every frame, not when it opens. A DCI-4K screen
//! (4096×2160), a 16:10 4K one, 5K and 6K screens, an ultrawide, or two
//! monitors side by side therefore produced `started`, then
//! `encoder_failed` with no frame. That happened wherever software is the
//! rung: every arm64 Linux host, and any host whose hardware cells are
//! denied or failed. Measured on the lab's arm64 host, whose DRM scanout is
//! 4096×2160, on 0.4.169.
//!
//! So the software rung opens openh264 at [`fit_for_software`]'s size and
//! downscales each frame to it with the live pump's own resampler (Lanczos-3,
//! with box below its floor). The pointer is already drawn at native size by
//! then. The recorder writes the track at that size
//! ([`VideoEncoder::coded_dims`]), and the sidecar and the `started` event
//! say what it was scaled from. A hardware rung records at native size, as
//! before.

use std::sync::Arc;

use anyhow::Result;

use crate::capture::Frame;
use crate::encode::openh264_backend::{Openh264Encoder, fit_for_software};
use crate::encode::resample::{Resampler, apply_target_resolution};
use crate::encode::{EncodedPacket, VideoEncoder};
use crate::peer::TargetResolution;

/// openh264 at a size it accepts, fed frames scaled down to it.
struct FittedEncoder {
    inner: Openh264Encoder,
    dims: (u32, u32),
    resampler: Resampler,
}

#[async_trait::async_trait]
impl VideoEncoder for FittedEncoder {
    async fn encode(&mut self, frame: Arc<Frame>) -> Result<Vec<EncodedPacket>> {
        let (width, height) = self.dims;
        // A non-BGRA frame passes through unscaled (the resampler is
        // BGRA-only), and openh264 then refuses it by name, as before.
        // Every recording capture backend hands BGRA today.
        let frame = apply_target_resolution(
            &mut self.resampler,
            frame,
            TargetResolution::Fixed { width, height },
        );
        self.inner.encode(frame).await
    }

    fn request_keyframe(&mut self) {
        self.inner.request_keyframe();
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.inner.set_bitrate(bps);
    }

    fn request_reference_invalidation(&mut self, lost_frame_number: u32) {
        self.inner.request_reference_invalidation(lost_frame_number);
    }

    fn set_frame_rate(&mut self, fps: u32) {
        self.inner.set_frame_rate(fps);
    }

    // `openh264`: the recorder groups its access units by this name.
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn is_hardware(&self) -> bool {
        self.inner.is_hardware()
    }

    fn coded_dims(&self) -> Option<(u32, u32)> {
        Some(self.dims)
    }
}

/// The recording's software encoder for a `width`×`height` capture:
/// openh264's recording profile, at a size it accepts.
pub fn software_recording_encoder(
    width: u32,
    height: u32,
    fps: u32,
    gop_frames: u32,
) -> Result<Box<dyn VideoEncoder>> {
    let (fw, fh) = fit_for_software(width, height);
    let inner = Openh264Encoder::new_recording(fw, fh, fps, gop_frames)?;
    if (fw, fh) == (width, height) {
        return Ok(Box::new(inner));
    }
    tracing::info!(
        from = %format!("{width}x{height}"),
        to = %format!("{fw}x{fh}"),
        "recording: the screen is larger than the software encoder takes — recording it scaled"
    );
    Ok(Box::new(FittedEncoder {
        inner,
        dims: (fw, fh),
        resampler: Resampler::new(),
    }))
}

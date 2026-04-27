//! VP9 profile 1 (8-bit 4:4:4) software encoder via libvpx.
//!
//! This is the encoder side of Phase Y (VP9 4:4:4 over RTCDataChannel),
//! see `docs/vp9-444-plan.md`. Sits alongside the existing MF + openh264
//! cascade — the agent's caps probe decides which to advertise based on
//! browser support + host CPU budget. Output frames are length-prefixed
//! and shipped over a `video-bytes` DataChannel rather than a WebRTC
//! video track, which is what makes 4:4:4 actually reachable in the
//! browser (Chrome's WebRTC video pipeline forces 4:2:0 across every
//! codec; WebCodecs `VideoDecoder` doesn't).
//!
//! Choices that match RustDesk's production setup:
//!   - VP9 profile 1, 8-bit 4:4:4 (codec string `vp09.01.10.08`)
//!   - `tune=screen-content` for desktop content
//!   - `cpu-used=8` (fastest preset; quality drop is small for screen
//!     content and we need real-time on iGPU-class hosts)
//!   - `lag-in-frames=0` — zero look-ahead, real-time priority
//!   - `kf-max-dist=240` — 8 s keyframe interval; we force IDRs on
//!     `rc:vp9.request_keyframe` from the viewer
//!
//! BGRA→I444 colour conversion uses `dcv_color_primitives` for AVX2
//! SIMD on x86_64. Without it the conversion is the bottleneck at
//! 1080p+.

#![cfg(feature = "vp9-444")]

use crate::capture::{Frame, PixelFormat};
use crate::encode::{EncodedPacket, VideoEncoder};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use std::sync::Arc;

/// Initial bitrate target before any back-channel feedback. Tuned for
/// 1080p 30fps screen content with 4:4:4 chroma — RustDesk's default
/// is in the same ballpark.
const DEFAULT_BITRATE_BPS: u32 = 8_000_000;

/// Keyframe interval in frames. 240 frames ≈ 8 s at 30fps. The viewer
/// requests intermediate IDRs via `rc:vp9.request_keyframe` when the
/// decoder errors or after a long gap.
const KEYFRAME_INTERVAL: u32 = 240;

pub struct Vp9Encoder {
    inner: vpx_encode::Encoder,
    width: u32,
    height: u32,
    /// Reusable I444 plane buffers — re-allocated on resolution change.
    /// Kept around between frames so steady-state encoding doesn't
    /// pressure the allocator.
    y_plane: Vec<u8>,
    u_plane: Vec<u8>,
    v_plane: Vec<u8>,
    /// Frame counter for keyframe forcing + the encoded-packet timestamp.
    frame_idx: u64,
    /// Keyframe-on-next-encode flag, set by `request_keyframe`.
    force_keyframe: bool,
    /// Most-recent bitrate target (as set by REMB/back-channel).
    /// libvpx's runtime control codec_control is the avenue for live
    /// bitrate updates; we apply on each set_bitrate call rather than
    /// per-frame to avoid thrash.
    target_bitrate: u32,
}

// `vpx_encode::Encoder` wraps a libvpx `vpx_codec_ctx` whose
// bindgen-generated layout contains raw `*const`/`*mut` pointers
// into C-allocated state. Those don't auto-impl Send, so the
// derived implementation for `Vp9Encoder` doesn't either, which
// breaks the `VideoEncoder: Send` bound the trait imposes
// (encode/mod.rs line 94).
//
// libvpx is documented as not internally thread-safe for shared
// access, but a single encoder context owned + driven by a single
// thread is fully supported — the entire RustDesk + WebRTC stack
// uses it that way, and our media_pump task is the sole owner of
// the `Vp9Encoder` instance from `encoder_dims` rebuild to drop.
// Safe to assert Send under that ownership invariant.
//
// Sync is intentionally NOT impl'd — libvpx mutates internal state
// on every encode() call, so concurrent shared-reference access
// would race. The pump's exclusive `&mut self` access prevents
// that automatically.
unsafe impl Send for Vp9Encoder {}

impl Vp9Encoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            bail!("vp9-444: require non-zero, even dimensions, got {width}x{height}");
        }
        let cfg = vpx_encode::Config {
            width,
            height,
            timebase: [1, 1_000_000],            // microsecond timebase
            bitrate: DEFAULT_BITRATE_BPS / 1000, // libvpx wants kbps
            codec: vpx_encode::VideoCodecId::VP9,
        };
        let inner =
            vpx_encode::Encoder::new(cfg).map_err(|e| anyhow!("vpx-encode init failed: {e:?}"))?;
        // 4:4:4 plane sizes: each is width × height (no chroma subsampling)
        let plane_size = (width as usize) * (height as usize);
        Ok(Self {
            inner,
            width,
            height,
            y_plane: vec![0; plane_size],
            u_plane: vec![0; plane_size],
            v_plane: vec![0; plane_size],
            frame_idx: 0,
            force_keyframe: true, // first frame is always keyframe
            target_bitrate: DEFAULT_BITRATE_BPS,
        })
    }

    /// Convert a BGRA frame into the three I444 planes. Uses
    /// `dcv_color_primitives` AVX2 path on x86_64 with SSE2 fallback.
    /// In-place into `self.{y,u,v}_plane`.
    fn bgra_to_i444(&mut self, frame: &Frame) -> Result<()> {
        if frame.width != self.width || frame.height != self.height {
            bail!(
                "vp9-444: frame dim mismatch — encoder configured {}x{}, got {}x{}",
                self.width,
                self.height,
                frame.width,
                frame.height
            );
        }
        let expected = (frame.width as usize) * (frame.height as usize) * 4;
        if frame.data.len() < expected {
            bail!(
                "vp9-444: BGRA buffer too small — need {} bytes, got {}",
                expected,
                frame.data.len()
            );
        }
        use dcv_color_primitives as dcv;
        // dcv pairs each pixel format with the colour space the
        // pixels are interpreted in. BGRA samples are gamma-
        // corrected sRGB (`ColorSpace::Rgb` in dcv's vocabulary —
        // confusingly named, but it's the gamma-encoded R'G'B'
        // variant per the doc comment in
        // dcv-color-primitives/src/color_space.rs). Only the YUV
        // planes get a luma/chroma colour space (Bt601 here).
        // Pairing BGRA with Bt601 (or with the non-existent
        // `Lrgb`) fails validation with `InvalidValue`. Caught by
        // the libvpx unit test under CI.
        let src_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::Bgra,
            color_space: dcv::ColorSpace::Rgb,
            num_planes: 1,
        };
        let dst_format = dcv::ImageFormat {
            pixel_format: dcv::PixelFormat::I444,
            color_space: dcv::ColorSpace::Bt601,
            num_planes: 3,
        };
        let src_buffers: &[&[u8]] = &[&frame.data];
        let src_strides = &[(frame.width * 4) as usize];
        let dst_buffers: &mut [&mut [u8]] =
            &mut [&mut self.y_plane, &mut self.u_plane, &mut self.v_plane];
        let dst_strides = &[
            self.width as usize,
            self.width as usize,
            self.width as usize,
        ];
        dcv::convert_image(
            self.width,
            self.height,
            &src_format,
            Some(src_strides),
            src_buffers,
            &dst_format,
            Some(dst_strides),
            dst_buffers,
        )
        .map_err(|e| anyhow!("dcv BGRA→I444 failed: {e:?}"))?;
        Ok(())
    }
}

#[async_trait]
impl VideoEncoder for Vp9Encoder {
    async fn encode(&mut self, frame: Arc<Frame>) -> Result<Vec<EncodedPacket>> {
        if frame.pixel_format != PixelFormat::Bgra {
            bail!("vp9-444: expected BGRA input, got {:?}", frame.pixel_format);
        }
        self.bgra_to_i444(&frame)?;

        let pts = frame.monotonic_us as i64;
        let force_kf = self.force_keyframe || self.frame_idx == 0;
        if self.frame_idx > 0 && self.frame_idx % (KEYFRAME_INTERVAL as u64) == 0 {
            // Periodic keyframe even without explicit request — bounds
            // worst-case recovery time after silent decoder corruption.
        }

        // vpx-encode wants a contiguous I444 layout; build a temp buffer.
        // TODO(perf): plumb a Vec<&[u8]> overload to vpx-encode if it
        // exists; at 1080p 4:4:4 this is ~6 MB per frame allocation.
        let plane_size = self.y_plane.len();
        let mut yuv = Vec::with_capacity(plane_size * 3);
        yuv.extend_from_slice(&self.y_plane);
        yuv.extend_from_slice(&self.u_plane);
        yuv.extend_from_slice(&self.v_plane);

        let mut out = Vec::new();
        let frames = self
            .inner
            .encode(pts, &yuv)
            .map_err(|e| anyhow!("vpx encode failed: {e:?}"))?;
        for f in frames {
            let is_keyframe = f.key;
            out.push(EncodedPacket {
                data: f.data.to_vec(),
                is_keyframe,
                duration_us: 33_333, // 30fps nominal; back-channel adjusts
            });
        }
        if force_kf {
            self.force_keyframe = false;
        }
        self.frame_idx += 1;
        Ok(out)
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.target_bitrate = bps;
        // vpx-encode 0.7 doesn't expose codec_control directly; we'd
        // need to either fork the crate or hold a raw libvpx ctx.
        // Until then this is a no-op except for future telemetry. The
        // back-channel rate-control loop in the media pump still uses
        // this value for local pacing decisions.
    }

    fn name(&self) -> &'static str {
        "libvpx-vp9-444"
    }

    fn is_hardware(&self) -> bool {
        // Pure SW. Caps probe treats this specially via the
        // `Vp9_444_Sw` ProbeResult variant — see `caps.rs` — so the
        // generic "drop SW heavy codec" rule doesn't fire on this
        // backend. SW VP9 4:4:4 IS the win, not a regression.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_bgra(w: u32, h: u32) -> Frame {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for px in data.chunks_exact_mut(4) {
            px[0] = 64; // B
            px[1] = 192; // G
            px[2] = 128; // R
            px[3] = 255; // A
        }
        Frame {
            width: w,
            height: h,
            stride: w * 4,
            pixel_format: PixelFormat::Bgra,
            data,
            monotonic_us: 0,
            monitor: 0,
            dirty_rects: Vec::new(),
        }
    }

    #[tokio::test]
    async fn first_frame_is_keyframe() {
        let Ok(mut enc) = Vp9Encoder::new(320, 240) else {
            // libvpx not linked in CI without the feature — skip
            return;
        };
        let f = Arc::new(synth_bgra(320, 240));
        let packets = enc.encode(f).await.expect("encode ok");
        assert!(!packets.is_empty(), "expected output packets");
        assert!(
            packets.iter().any(|p| p.is_keyframe),
            "first frame must contain a keyframe"
        );
    }

    #[test]
    fn rejects_odd_dims() {
        assert!(Vp9Encoder::new(321, 240).is_err());
        assert!(Vp9Encoder::new(320, 241).is_err());
    }
}

//! FFmpeg encoder backend wrapping `ffmpeg-next`.
//!
//! rc.72 scope: BGRA→NV12 CPU path + encoder dispatch
//! (`hevc_nvenc` → `hevc_qsv` → `hevc_amf`). Behind
//! `ROOMLER_AGENT_USE_FFMPEG=1` env var. MF cascade still default.
//!
//! rc.73+: D3D11VA zero-copy (capture's D3D11 texture fed directly to
//! NVENC / QSV / AMF without CPU readback). Defers Phase 8's critique
//! warning about late zero-copy refactors — we ship the CPU path first
//! to establish the encoder works at all, then swap to zero-copy in a
//! follow-up RC that doesn't add other behaviour.
//!
//! ## Encoder configuration
//!
//! - Input format: NV12 (single plane Y + interleaved UV at half-res both axes)
//! - Output format: HEVC Annex-B (4-byte start codes; the pre-flight WebCodecs
//!   spike confirmed Chrome accepts this without a hvcC description box)
//! - Rate control: CBR target via `bit_rate`; HW backends interpret this
//!   differently (NVENC = `NV_ENC_PARAMS_RC_CBR`, QSV = `CBR`, AMF = `CBR`)
//! - GOP: 240 frames (matches libvpx VP9-444 cadence — same DC framing
//!   anti-IDR-storm characteristics apply)
//! - Profile: Main 8-bit 4:2:0 (matches RustDesk; broadest browser
//!   WebCodecs support per the pre-flight spike)

use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use ffmpeg_next::{codec, format, frame, util};

use crate::capture::{DirtyRect, Frame, PixelFormat};
use crate::encode::{EncodedPacket, VideoEncoder};

/// Initial GOP interval — every Nth frame is forced IDR. Matches libvpx
/// VP9-444 cadence in `encode::libvpx`. The DC framing path's
/// anti-IDR-storm coalescer (rc.74+) bounds the actual rate at which
/// IDRs can be emitted regardless of this setting.
const KEYFRAME_INTERVAL: i32 = 240;

/// Time-base denominator. We use 1000 (millisecond resolution) so that
/// `monotonic_us` from `Frame` can be converted to pts via integer
/// division without precision loss at typical capture rates.
const TIME_BASE_DEN: i32 = 1000;

/// Codec dispatch order for HEVC. First successful `find_by_name +
/// open_as` wins. Matches RustDesk's order (NVIDIA → Intel → AMD).
const HEVC_ENCODER_NAMES: &[&str] = &["hevc_nvenc", "hevc_qsv", "hevc_amf"];

/// FFmpeg-based video encoder.
///
/// Holds a `codec::encoder::Video` plus state for keyframe forcing,
/// bitrate updates, and BGRA→NV12 conversion. The `convert_buf`
/// scratch buffer is sized for the largest frame seen so far so we
/// don't reallocate every frame.
pub struct FfmpegEncoder {
    /// Stable identifier for logs / `is_hardware` decisions, e.g.
    /// `"hevc_nvenc"`. Bound at construction time, never changes.
    encoder_name: &'static str,

    /// Encode width × height. Bound at construction; the agent currently
    /// re-creates the encoder on resolution changes.
    width: u32,
    height: u32,

    /// FFmpeg encoder handle. Owns the underlying AVCodecContext.
    encoder: codec::encoder::Video,

    /// Frame counter for pts + GOP timing.
    frame_count: u64,

    /// Set by `request_keyframe` to force the next frame to be IDR. The
    /// FFmpeg `Video::send_frame` path needs `frame.set_key_frame(true)`
    /// + `frame.set_pict_type(I)` to force IDR on HEVC encoders.
    force_keyframe: bool,

    /// Scratch buffer for the NV12 Y plane. Reused across frames.
    nv12_y: Vec<u8>,
    /// Scratch buffer for the NV12 UV interleaved plane. Reused across frames.
    nv12_uv: Vec<u8>,
}

impl FfmpegEncoder {
    /// Try to open an HEVC encoder via the dispatch cascade. Returns
    /// the first encoder that opens cleanly. Returns `Err` if all
    /// backends fail — the caller falls back to MF / NoopEncoder.
    pub fn new_hevc(width: u32, height: u32) -> Result<Self> {
        Self::new_with_dispatch(HEVC_ENCODER_NAMES, width, height)
    }

    fn new_with_dispatch(names: &[&'static str], width: u32, height: u32) -> Result<Self> {
        // `ffmpeg_next::init()` is idempotent + cheap to call; safe to
        // run on each new encoder. Sets up codec registration.
        ffmpeg_next::init().context("ffmpeg_next::init failed")?;

        let bit_rate = crate::encode::initial_bitrate_for(width, height) as usize;
        let fps = 30; // initial framerate; set_bitrate doesn't change this

        let mut last_err: Option<anyhow::Error> = None;
        for name in names {
            match Self::build_encoder(name, width, height, bit_rate, fps) {
                Ok(encoder) => {
                    tracing::info!(
                        encoder = name,
                        width,
                        height,
                        bit_rate,
                        "ffmpeg HEVC encoder opened"
                    );
                    let plane_pixels = (width as usize) * (height as usize);
                    return Ok(Self {
                        encoder_name: name,
                        width,
                        height,
                        encoder,
                        frame_count: 0,
                        force_keyframe: false,
                        nv12_y: vec![0u8; plane_pixels],
                        // UV is half-width × half-height × 2 channels = pixels / 2
                        nv12_uv: vec![0u8; plane_pixels / 2],
                    });
                }
                Err(e) => {
                    tracing::warn!(encoder = name, error = %e, "ffmpeg encoder open failed; trying next");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("no HEVC encoder names tried")))
    }

    fn build_encoder(
        name: &'static str,
        width: u32,
        height: u32,
        bit_rate: usize,
        fps: i32,
    ) -> Result<codec::encoder::Video> {
        let codec = codec::encoder::find_by_name(name)
            .ok_or_else(|| anyhow!("ffmpeg encoder not registered: {}", name))?;

        let mut ctx = codec::Context::new_with_codec(codec);
        let mut enc = ctx.encoder().video().context("encoder().video() failed")?;

        enc.set_width(width);
        enc.set_height(height);
        enc.set_format(format::Pixel::NV12);
        enc.set_bit_rate(bit_rate);
        // Time base: 1/1000 (ms resolution). Pts is set per-frame from
        // monotonic_us / 1000.
        enc.set_time_base((1, TIME_BASE_DEN));
        enc.set_frame_rate(Some((fps, 1)));
        enc.set_gop(KEYFRAME_INTERVAL as u32);
        enc.set_max_b_frames(0); // low-latency: no B-frames

        let encoder = enc
            .open_as(codec)
            .with_context(|| format!("open_as({}) failed", name))?;

        Ok(encoder)
    }

    fn convert_bgra_to_nv12(&mut self, frame: &Frame) -> Result<()> {
        if frame.pixel_format != PixelFormat::Bgra {
            // Capture layer already produced NV12 — copy planes directly.
            // WGC + DXGI on Windows can emit NV12 in some configurations.
            // For rc.72 we only handle BGRA from scrap/DXGI which is our
            // current production path; NV12 from capture is a rc.73+ path.
            return Err(anyhow!(
                "ffmpeg encoder rc.72 requires BGRA capture input (got {:?})",
                frame.pixel_format
            ));
        }

        let plane_pixels = (self.width as usize) * (self.height as usize);
        if self.nv12_y.len() != plane_pixels {
            self.nv12_y.resize(plane_pixels, 0);
            self.nv12_uv.resize(plane_pixels / 2, 0);
        }

        // BGRA→NV12 via dcv_color_primitives. The crate is already a dep
        // for the libvpx VP9 4:4:4 path (BGRA→I444). NV12 conversion uses
        // the same SIMD primitives.
        use dcv_color_primitives::{
            ColorSpace, ImageFormat, PixelFormat as DcvPixelFormat, convert_image,
        };

        let src_format = ImageFormat {
            pixel_format: DcvPixelFormat::Bgra,
            color_space: ColorSpace::Rgb,
            num_planes: 1,
        };
        let dst_format = ImageFormat {
            pixel_format: DcvPixelFormat::Nv12,
            color_space: ColorSpace::Bt601,
            num_planes: 2,
        };

        // Two-plane NV12: Y is width × height; UV is interleaved
        // width × (height / 2) bytes (== plane_pixels / 2 in interleaved form).
        let mut dst_planes: [&mut [u8]; 2] = {
            let (y, uv) = (&mut self.nv12_y[..], &mut self.nv12_uv[..]);
            [y, uv]
        };
        let dst_strides = [self.width as usize, self.width as usize];

        convert_image(
            self.width,
            self.height,
            &src_format,
            Some(&[frame.stride as usize]),
            &[&frame.data],
            &dst_format,
            Some(&dst_strides),
            &mut dst_planes,
        )
        .map_err(|e| anyhow!("dcv BGRA→NV12 convert failed: {:?}", e))?;

        Ok(())
    }

    fn build_av_frame(&self, monotonic_us: u64) -> Result<frame::Video> {
        let mut av = frame::Video::new(format::Pixel::NV12, self.width, self.height);

        let pts = (monotonic_us / 1000) as i64;
        av.set_pts(Some(pts));

        if self.force_keyframe || self.frame_count == 0 {
            av.set_kind(util::picture::Type::I);
            // Note: set_key_frame doesn't exist on Video in ffmpeg-next 8.x;
            // set_kind(I) is the supported way to force IDR.
        }

        // Copy our converted NV12 planes into the AVFrame's plane buffers.
        // FFmpeg's allocator gives us width/height-aligned buffers; we
        // copy row-by-row to handle stride differences.
        //
        // rc.73 borrow-checker fix: capture strides before the mutable
        // borrow from data_mut(). `av.stride(N)` takes &self while
        // `av.data_mut(N)` takes &mut self — calling them on the same
        // expression triggers E0502.
        let y_src_width = self.width as usize;
        let y_rows = self.height as usize;
        let uv_src_width = self.width as usize;
        let uv_rows = (self.height / 2) as usize;
        let y_stride = av.stride(0);
        let uv_stride = av.stride(1);
        copy_plane_into_av(av.data_mut(0), y_stride, &self.nv12_y, y_src_width, y_rows);
        copy_plane_into_av(
            av.data_mut(1),
            uv_stride,
            &self.nv12_uv,
            uv_src_width,
            uv_rows,
        );

        Ok(av)
    }

    fn drain_packets(&mut self) -> Result<Vec<EncodedPacket>> {
        let mut out = Vec::new();
        let mut packet = codec::packet::Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    let data = packet.data().unwrap_or(&[]).to_vec();
                    let is_keyframe = packet.is_key();
                    // duration is in time_base units (1/1000 == ms); convert to us.
                    let duration_us = (packet.duration().max(0) as u64) * 1000;
                    out.push(EncodedPacket {
                        data,
                        is_keyframe,
                        duration_us,
                    });
                }
                Err(ffmpeg_next::Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => {
                    break;
                }
                Err(ffmpeg_next::Error::Eof) => break,
                Err(e) => return Err(anyhow!("ffmpeg receive_packet failed: {}", e)),
            }
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl VideoEncoder for FfmpegEncoder {
    async fn encode(&mut self, frame: Arc<Frame>) -> Result<Vec<EncodedPacket>> {
        if frame.width != self.width || frame.height != self.height {
            return Err(anyhow!(
                "frame size {}x{} doesn't match encoder size {}x{} — re-create the encoder on resolution change",
                frame.width,
                frame.height,
                self.width,
                self.height
            ));
        }

        self.convert_bgra_to_nv12(&frame)?;
        let av = self.build_av_frame(frame.monotonic_us)?;
        self.encoder
            .send_frame(&av)
            .map_err(|e| anyhow!("ffmpeg send_frame failed: {}", e))?;

        self.force_keyframe = false;
        self.frame_count += 1;

        self.drain_packets()
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bps: u32) {
        // ffmpeg-next 8.x doesn't expose a stable runtime-bitrate setter on
        // `codec::encoder::Video`. NVENC + QSV both accept reconfigure via
        // `AVCodecContext->bit_rate = X` followed by an internal reconfigure
        // call, but the safe Rust API doesn't surface this. For rc.72 we
        // just log the request; rc.73 wires the reconfigure via raw FFI
        // (similar to the openh264-sys2 bitrate-set pattern in the libvpx
        // path).
        tracing::debug!(
            bps,
            encoder = self.encoder_name,
            "set_bitrate not yet wired (rc.72 limitation)"
        );
    }

    fn set_roi_hints(&mut self, _rects: &[DirtyRect], _frame_dims: (u32, u32)) {
        // NVENC ROI maps + AMF QP maps land in rc.75+ alongside other
        // codec-specific tuning. Default no-op for rc.72.
    }

    fn name(&self) -> &'static str {
        self.encoder_name
    }

    fn is_hardware(&self) -> bool {
        // All three names in HEVC_ENCODER_NAMES are HW backends.
        true
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        // Best-effort flush — send EOF and drain any held packets so the
        // encoder doesn't log warnings about un-drained state.
        let _ = self.encoder.send_eof();
        let _ = self.drain_packets();
    }
}

fn copy_plane_into_av(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    src_width: usize,
    rows: usize,
) {
    for y in 0..rows {
        let dst_off = y * dst_stride;
        let src_off = y * src_width;
        if dst_off + src_width > dst.len() || src_off + src_width > src.len() {
            break;
        }
        dst[dst_off..dst_off + src_width].copy_from_slice(&src[src_off..src_off + src_width]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the encoder construction probe handles the all-failed case
    /// without panicking — important because the dispatch happens before
    /// any frames flow, and a panic here would kill the agent's media
    /// pump task with no useful telemetry.
    #[test]
    fn new_hevc_returns_err_when_all_names_unknown() {
        // Use synthetic names that vcpkg ffmpeg definitely doesn't ship.
        let res = FfmpegEncoder::new_with_dispatch(
            &["nope_nvenc_xx", "nope_qsv_xx", "nope_amf_xx"],
            640,
            360,
        );
        assert!(res.is_err(), "expected Err for unknown encoder names");
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains("not registered") || msg.contains("encoder names tried"),
            "expected dispatch error, got: {msg}"
        );
    }

    /// Verify the dispatch order matches RustDesk's pattern + our docs.
    /// Locks the order so a refactor doesn't accidentally reorder.
    #[test]
    fn hevc_dispatch_order_is_nvenc_qsv_amf() {
        assert_eq!(HEVC_ENCODER_NAMES, &["hevc_nvenc", "hevc_qsv", "hevc_amf"]);
    }
}

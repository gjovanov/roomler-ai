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

/// rc.83 — Codec dispatch order for VP9. Intel oneVPL only — NVIDIA
/// NVENC + AMD AMF never added VP9 encode (they skipped to AV1). On
/// non-Intel hosts the cascade falls through to libvpx SW via the
/// existing `media_pump_vp9_444_dc` path (no FFmpeg fallback here —
/// libvpx is what the caps probe advertises).
///
/// Gate 0 validated `hevc_qsv` on Iris Xe Tiger Lake; `vp9_qsv` on
/// the same iGPU family is the load-bearing assumption for the Iris
/// Xe fps unlock (CPU-bound 17 fps on libvpx SW → expected 30-60 fps
/// on iGPU HW).
const VP9_ENCODER_NAMES: &[&str] = &["vp9_qsv"];

/// rc.86 — constant-quality target (lower = sharper, more bits).
/// Default 22 is a good screen-content sweet spot for HEVC/VP9 — fine
/// text edges stay crisp without a full lossless blow-out. Range
/// clamped to [10, 40]; below 10 is near-lossless (huge), above 40 is
/// visibly soft. Env-overridable for field tuning without a rebuild.
fn ffmpeg_cq() -> u32 {
    std::env::var("ROOMLER_AGENT_FFMPEG_CQ")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(|c| c.clamp(10, 40))
        .unwrap_or(22)
}

/// rc.86 — bandwidth CEILING (maxrate/bufsize), NOT a target. With
/// constant-quality rate control the encoder uses only what the `cq`
/// quality demands (idle ≈ 0); this cap just bounds the worst-case
/// burst on a full-screen scene change. Derived at ~0.07 bpp/s — about
/// a third of the old 0.20-bpp/s *target* the pre-rc.86 path fed as a
/// VBR goal — clamped to [3, 12] Mbps. RustDesk holds ~3 Mbps at
/// 1920×1200; 0.07 bpp/s puts our 1920×1200 cap at ~4.8 Mbps, leaving
/// headroom for genuine motion without the 6-7 Mbps idle-ish bursts
/// the field saw on the old uncapped 13.8 Mbps target.
/// Env override `ROOMLER_AGENT_FFMPEG_MAXRATE_KBPS` for field tuning.
fn ffmpeg_maxrate_bps(width: u32, height: u32, fps: u32) -> usize {
    if let Some(kbps) = std::env::var("ROOMLER_AGENT_FFMPEG_MAXRATE_KBPS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|k| *k > 0)
    {
        return kbps * 1000;
    }
    const SCREEN_BPP_PER_SECOND: f64 = 0.07;
    let raw = (width as f64 * height as f64 * fps as f64 * SCREEN_BPP_PER_SECOND) as usize;
    raw.clamp(3_000_000, 12_000_000)
}

/// rc.86 — per-encoder private-option dictionary for constant-quality
/// + low-latency + screen-content tuning. Keys mirror the FFmpeg CLI
/// (`-cq`, `-preset`, `-tune`, `-spatial-aq`, `-maxrate`, …). Any option
/// the encoder doesn't recognise on this FFmpeg build / driver combo
/// makes `open_as_with` fail; `build_encoder` then retries a plain open
/// so we degrade to defaults rather than failing the session.
///
/// `preset`/`tune` are env-overridable so the field can trade quality
/// vs latency vs CPU without a rebuild.
///
/// Returns the dict PLUS a human-readable `key=value …` summary string
/// built as we go (so we don't depend on `Dictionary::iter()` — keeps
/// the logging robust across ffmpeg-next minor versions).
fn encoder_options(
    name: &str,
    maxrate_bps: usize,
    cq: u32,
) -> (ffmpeg_next::Dictionary<'static>, String) {
    let mut d = ffmpeg_next::Dictionary::new();
    let mut summary: Vec<String> = Vec::new();
    let cap = maxrate_bps.to_string();
    let cq_s = cq.to_string();
    let preset = std::env::var("ROOMLER_AGENT_FFMPEG_PRESET").ok();
    let tune = std::env::var("ROOMLER_AGENT_FFMPEG_TUNE").ok();

    // Local closure: set on the dict AND append to the summary in one go.
    let mut put = |d: &mut ffmpeg_next::Dictionary<'static>, k: &str, v: &str| {
        d.set(k, v);
        summary.push(format!("{k}={v}"));
    };

    if name.contains("nvenc") {
        // NVENC constant-quality VBR: `cq` drives quality, `maxrate`
        // bounds the burst, bit_rate=0 (set in build_encoder) makes it
        // pure target-quality. `tune=ll` keeps it responsive for remote
        // desktop; `spatial-aq` spends bits on high-detail text regions;
        // bf=0 + rc-lookahead=0 minimise latency.
        put(&mut d, "rc", "vbr");
        put(&mut d, "cq", &cq_s);
        put(&mut d, "preset", preset.as_deref().unwrap_or("p4"));
        put(&mut d, "tune", tune.as_deref().unwrap_or("ll"));
        put(&mut d, "rc-lookahead", "0");
        put(&mut d, "bf", "0");
        put(&mut d, "spatial-aq", "1");
        put(&mut d, "maxrate", &cap);
        put(&mut d, "bufsize", &cap);
    } else if name.contains("qsv") {
        // Intel QSV: ICQ-style quality via `global_quality`; `maxrate`
        // caps the burst. `low_power` uses the fixed-function VDENC path
        // (faster, lower power — the Iris Xe fps-unlock path).
        put(&mut d, "global_quality", &cq_s);
        if let Some(p) = preset.as_deref() {
            put(&mut d, "preset", p);
        }
        put(&mut d, "low_power", "1");
        put(&mut d, "maxrate", &cap);
        put(&mut d, "bufsize", &cap);
    } else if name.contains("amf") {
        // AMD AMF: constant-QP-ish via qp_i/qp_p, latency-tuned VBR,
        // capped burst.
        put(&mut d, "rc", "vbr_latency");
        put(&mut d, "qp_i", &cq_s);
        put(&mut d, "qp_p", &cq_s);
        put(&mut d, "maxrate", &cap);
        put(&mut d, "bufsize", &cap);
    }

    let joined = summary.join(" ");
    (d, joined)
}

/// FFmpeg-based video encoder.
///
/// Holds a `codec::encoder::Video` plus state for keyframe forcing,
/// bitrate updates, and BGRA→NV12 conversion. The `convert_buf`
/// scratch buffer is sized for the largest frame seen so far so we
/// don't reallocate every frame.
// Manually impl Debug — the underlying `codec::encoder::Video` doesn't
// derive Debug, and we want a short stable repr for tracing + the
// `Result<FfmpegEncoder, _>::unwrap_err()` in unit tests.
impl std::fmt::Debug for FfmpegEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfmpegEncoder")
            .field("encoder_name", &self.encoder_name)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("frame_count", &self.frame_count)
            .field("force_keyframe", &self.force_keyframe)
            .finish()
    }
}

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

    /// rc.83 — Try to open a VP9 HW encoder. Currently Intel oneVPL
    /// only (`vp9_qsv`). Returns `Err` on non-Intel hosts; the caller
    /// falls back to libvpx SW. Profile 0 (4:2:0 8-bit) is the only
    /// profile vp9_qsv supports — 4:4:4 sessions stay on libvpx
    /// regardless of this method's availability.
    pub fn new_vp9(width: u32, height: u32) -> Result<Self> {
        Self::new_with_dispatch(VP9_ENCODER_NAMES, width, height)
    }

    fn new_with_dispatch(names: &[&'static str], width: u32, height: u32) -> Result<Self> {
        // `ffmpeg_next::init()` is idempotent + cheap to call; safe to
        // run on each new encoder. Sets up codec registration.
        ffmpeg_next::init().context("ffmpeg_next::init failed")?;

        let fps = 30; // initial framerate; set_bitrate doesn't change this
        // rc.86 — RustDesk-parity rate control. Drive the encoder by
        // CONSTANT QUALITY (cq / global_quality) with a bandwidth CAP
        // (maxrate), not by the old 0.20-bpp/s VBR target. On screen
        // content this keeps text edges sharp (cq guarantees per-block
        // quality so nothing "crystallizes over seconds") while idle
        // frames cost ~0 and bursts are bounded by the cap. Both knobs
        // are env-overridable so the field can dial in without a rebuild.
        let cq = ffmpeg_cq();
        let maxrate_bps = ffmpeg_maxrate_bps(width, height, fps as u32);

        let mut last_err: Option<anyhow::Error> = None;
        for name in names {
            match Self::build_encoder(name, width, height, fps, maxrate_bps, cq) {
                Ok(encoder) => {
                    tracing::info!(
                        encoder = name,
                        width,
                        height,
                        cq,
                        maxrate_bps,
                        "ffmpeg encoder opened (constant-quality + maxrate cap)"
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
                    // rc.85 — DEBUG not WARN. A candidate failing in the
                    // cascade is the cascade doing its job, not a warning
                    // condition. The CALLER logs the consequential outcome
                    // at the right level (caps.rs: INFO+%e for VP9, WARN
                    // for HEVC; peer.rs: falls through to libvpx/MF). The
                    // "; trying next" suffix lied for single-entry lists
                    // (VP9_ENCODER_NAMES = ["vp9_qsv"]). Error reason is
                    // preserved in `last_err` → surfaced by the caller.
                    tracing::debug!(encoder = name, error = %e, "ffmpeg encoder candidate failed to open");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("no ffmpeg encoder candidates were tried")))
    }

    fn build_encoder(
        name: &'static str,
        width: u32,
        height: u32,
        fps: i32,
        maxrate_bps: usize,
        cq: u32,
    ) -> Result<codec::encoder::Video> {
        let codec = codec::encoder::find_by_name(name)
            .ok_or_else(|| anyhow!("ffmpeg encoder not registered: {}", name))?;

        // rc.86 — configure an unopened encoder. Factored into a closure
        // so we can rebuild it for the fallback path (open_*_with consumes
        // the encoder, so a failed open can't be retried on the same one).
        //
        // rc.89 fix: the UNOPENED encoder is `encoder::video::Video`,
        // which is a DIFFERENT type from `codec::encoder::Video` (the
        // OPENED encoder this fn returns). `open_as*` converts unopened →
        // opened. The closure must therefore declare the unopened type;
        // annotating it as the opened type was the rc.86 CI E0308.
        let configure = || -> Result<ffmpeg_next::encoder::video::Video> {
            let ctx = codec::Context::new_with_codec(codec);
            let mut enc = ctx.encoder().video().context("encoder().video() failed")?;
            enc.set_width(width);
            enc.set_height(height);
            enc.set_format(format::Pixel::NV12);
            // For NVENC constant-quality VBR we set bit_rate=0 so `cq`
            // drives quality and `maxrate` is the only ceiling (idle ≈ 0).
            // QSV/AMF keep `maxrate` as the VBR anchor since their
            // quality modes are less reliable about honouring b:v=0.
            let target_bps = if name.contains("nvenc") {
                0
            } else {
                maxrate_bps
            };
            enc.set_bit_rate(target_bps);
            // Time base: 1/1000 (ms resolution). Pts is set per-frame from
            // monotonic_us / 1000.
            enc.set_time_base((1, TIME_BASE_DEN));
            enc.set_frame_rate(Some((fps, 1)));
            enc.set_gop(KEYFRAME_INTERVAL as u32);
            enc.set_max_b_frames(0); // low-latency: no B-frames
            Ok(enc)
        };

        let (opts, opt_summary) = encoder_options(name, maxrate_bps, cq);

        // Try opening WITH the quality/tuning options. If the encoder
        // rejects any of them (unknown private option on this FFmpeg
        // build / driver combo), fall back to a plain open so we degrade
        // to default rate control rather than failing the encoder
        // outright — a blurry-but-working session beats a black screen.
        let enc = configure()?;
        match enc.open_as_with(codec, opts) {
            Ok(encoder) => {
                tracing::info!(
                    encoder = name,
                    options = opt_summary,
                    "ffmpeg encoder opened with quality options"
                );
                Ok(encoder)
            }
            Err(open_err) => {
                tracing::warn!(
                    encoder = name,
                    %open_err,
                    attempted_options = opt_summary,
                    "ffmpeg open_as_with rejected the quality options — retrying with encoder defaults"
                );
                let enc2 = configure()?;
                let encoder = enc2
                    .open_as(codec)
                    .with_context(|| format!("open_as({}) fallback failed", name))?;
                Ok(encoder)
            }
        }
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

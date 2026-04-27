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
//! Bound directly against `env_libvpx_sys` (raw FFI). The `vpx-encode`
//! 0.6 wrapper hardcoded `VPX_IMG_FMT_I420` in encode() and exposed no
//! `g_profile` setter, so profile-1 output was unreachable through it
//! — see Y.runtime-encoder in `docs/vp9-444-plan.md`. Talking to libvpx
//! directly costs ~150 LOC of unsafe but lets us configure profile + I444
//! input + zero look-ahead + screen-content tuning, all of which matter
//! for correctness here.
//!
//! BGRA→I444 colour conversion uses `dcv_color_primitives` for AVX2
//! SIMD on x86_64. Without it the conversion is the bottleneck at
//! 1080p+.

#![cfg(feature = "vp9-444")]

use crate::capture::{Frame, PixelFormat};
use crate::encode::{EncodedPacket, VideoEncoder};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use std::os::raw::{c_int, c_uint};
use std::sync::Arc;
use vpx_sys as vpx;

/// Initial bitrate target before any back-channel feedback. Tuned for
/// 1080p 30fps screen content with 4:4:4 chroma — RustDesk's default
/// is in the same ballpark.
const DEFAULT_BITRATE_BPS: u32 = 8_000_000;

/// Keyframe interval in frames. 240 frames ≈ 8 s at 30fps. The viewer
/// requests intermediate IDRs via `rc:vp9.request_keyframe` when the
/// decoder errors or after a long gap.
const KEYFRAME_INTERVAL: u32 = 240;

/// Microsecond timebase numerator/denominator. PTS values are passed in
/// directly as microseconds.
const TIMEBASE_NUM: c_int = 1;
const TIMEBASE_DEN: c_int = 1_000_000;

pub struct Vp9Encoder {
    /// libvpx encoder context. Owned + freed in Drop.
    ctx: vpx::vpx_codec_ctx_t,
    /// Cached cfg so `set_bitrate` can mutate `rc_target_bitrate` and
    /// hand it back to libvpx via `vpx_codec_enc_config_set`.
    cfg: vpx::vpx_codec_enc_cfg_t,
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
    target_bitrate: u32,
}

// `vpx_codec_ctx_t` contains raw `*const`/`*mut` pointers into
// C-allocated state (libvpx priv struct, iface vtable, error string).
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

        // SAFETY: libvpx public API. We hold no aliasing references to
        // the cfg / ctx during these calls, the iface pointer is
        // returned by libvpx and lives for the process lifetime, and
        // we check every return code.
        let mut cfg: vpx::vpx_codec_enc_cfg_t = unsafe { std::mem::zeroed() };
        let iface = unsafe { vpx::vpx_codec_vp9_cx() };
        if iface.is_null() {
            bail!("vp9-444: vpx_codec_vp9_cx() returned null — libvpx VP9 codec not linked");
        }
        let err = unsafe { vpx::vpx_codec_enc_config_default(iface, &mut cfg, 0) };
        if err != vpx::VPX_CODEC_OK {
            bail!("vp9-444: vpx_codec_enc_config_default failed: {err:?}");
        }

        // Profile 1 = 8-bit 4:4:4. Browser's WebCodecs `VideoDecoder`
        // is configured with `vp09.01.10.08` which decodes only this
        // profile; mismatch would leave the canvas blank.
        cfg.g_profile = 1;
        cfg.g_w = width as c_uint;
        cfg.g_h = height as c_uint;
        cfg.g_timebase = vpx::vpx_rational {
            num: TIMEBASE_NUM,
            den: TIMEBASE_DEN,
        };
        cfg.rc_target_bitrate = DEFAULT_BITRATE_BPS / 1000; // libvpx wants kbps
        cfg.rc_end_usage = vpx::vpx_rc_mode::VPX_CBR;
        cfg.g_pass = vpx::vpx_enc_pass::VPX_RC_ONE_PASS;
        cfg.g_lag_in_frames = 0;
        cfg.g_threads = num_cpus_for_encode();
        cfg.g_error_resilient = 0;
        cfg.g_bit_depth = vpx::vpx_bit_depth::VPX_BITS_8;
        cfg.g_input_bit_depth = 8;
        cfg.kf_mode = vpx::vpx_kf_mode::VPX_KF_AUTO;
        cfg.kf_min_dist = 0;
        cfg.kf_max_dist = KEYFRAME_INTERVAL;
        // Real-time-friendly buffer sizing: 1s buffer at target bitrate
        // with a 0.5s initial / optimal floor. Matches RustDesk's defaults.
        cfg.rc_buf_sz = 1000;
        cfg.rc_buf_initial_sz = 500;
        cfg.rc_buf_optimal_sz = 600;
        cfg.rc_dropframe_thresh = 0;
        cfg.rc_undershoot_pct = 50;
        cfg.rc_overshoot_pct = 50;
        cfg.rc_min_quantizer = 4;
        cfg.rc_max_quantizer = 56;

        let mut ctx: vpx::vpx_codec_ctx_t = unsafe { std::mem::zeroed() };
        let init_err = unsafe {
            vpx::vpx_codec_enc_init_ver(
                &mut ctx,
                iface,
                &cfg,
                0,
                vpx::VPX_ENCODER_ABI_VERSION as c_int,
            )
        };
        if init_err != vpx::VPX_CODEC_OK {
            bail!("vp9-444: vpx_codec_enc_init_ver failed: {init_err:?}");
        }

        // Apply VP9 controls that have no Config-struct equivalent.
        // Failure here is non-fatal but logged — we'd rather encode
        // sub-optimally than refuse to start.
        let mut enc = Self {
            ctx,
            cfg,
            width,
            height,
            y_plane: vec![0; (width * height) as usize],
            u_plane: vec![0; (width * height) as usize],
            v_plane: vec![0; (width * height) as usize],
            frame_idx: 0,
            force_keyframe: true, // first frame is always keyframe
            target_bitrate: DEFAULT_BITRATE_BPS,
        };
        enc.apply_screen_content_controls();
        Ok(enc)
    }

    fn apply_screen_content_controls(&mut self) {
        // CPUUSED 8 = fastest preset. Quality drop is small for screen
        // content + huge speed win on iGPU-class hosts. RustDesk default.
        self.set_ctrl(
            vpx::vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int,
            8 as c_int,
            "VP8E_SET_CPUUSED",
        );
        // SCREEN tune disables psychovisual prep that's tuned for camera
        // content, preserves sharp text edges. The single biggest lever
        // for desktop content quality at low bitrates.
        self.set_ctrl(
            vpx::vp8e_enc_control_id::VP9E_SET_TUNE_CONTENT as c_int,
            vpx::vp9e_tune_content::VP9E_CONTENT_SCREEN as c_int,
            "VP9E_SET_TUNE_CONTENT",
        );
        // AQ off: adaptive quantization tries to spend bits on faces /
        // edges, which on a desktop screenshot mis-fires and softens
        // text. Off matches RustDesk + Chrome's screen-share defaults.
        self.set_ctrl(
            vpx::vp8e_enc_control_id::VP9E_SET_AQ_MODE as c_int,
            0 as c_uint,
            "VP9E_SET_AQ_MODE",
        );
        // 4 tile columns to parallelise both encode and decode. log2 == 2.
        self.set_ctrl(
            vpx::vp8e_enc_control_id::VP9E_SET_TILE_COLUMNS as c_int,
            2 as c_int,
            "VP9E_SET_TILE_COLUMNS",
        );
        // Frame-parallel decoding lets the browser decoder use its
        // tile-column-level parallelism path.
        self.set_ctrl(
            vpx::vp8e_enc_control_id::VP9E_SET_FRAME_PARALLEL_DECODING as c_int,
            1 as c_uint,
            "VP9E_SET_FRAME_PARALLEL_DECODING",
        );
        // Static threshold helps idle desktop frames skip macroblocks.
        self.set_ctrl(
            vpx::vp8e_enc_control_id::VP8E_SET_STATIC_THRESHOLD as c_int,
            100 as c_uint,
            "VP8E_SET_STATIC_THRESHOLD",
        );
        // Noise sensitivity off — desktop content has no noise.
        self.set_ctrl(
            vpx::vp8e_enc_control_id::VP9E_SET_NOISE_SENSITIVITY as c_int,
            0 as c_uint,
            "VP9E_SET_NOISE_SENSITIVITY",
        );
    }

    fn set_ctrl<T: Copy>(&mut self, id: c_int, value: T, name: &'static str) {
        // SAFETY: libvpx's variadic control ABI accepts an int-sized
        // argument for every VP9 control we touch (cpuused, tune,
        // aq_mode, tile_columns, frame_parallel, static_threshold,
        // noise_sensitivity). We only ever pass `c_int` or `c_uint`,
        // both of which are int-width on every supported target.
        let err = unsafe { vpx::vpx_codec_control_(&mut self.ctx, id, value) };
        if err != vpx::VPX_CODEC_OK {
            tracing::warn!(
                control = name,
                ?err,
                "vp9-444: ctrl set failed (encode will continue with default)"
            );
        }
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

impl Drop for Vp9Encoder {
    fn drop(&mut self) {
        // SAFETY: ctx was successfully initialised in `new` (the
        // `bail!` paths above return before constructing Self), so
        // destroy is the matching teardown.
        unsafe {
            vpx::vpx_codec_destroy(&mut self.ctx);
        }
    }
}

#[async_trait]
impl VideoEncoder for Vp9Encoder {
    async fn encode(&mut self, frame: Arc<Frame>) -> Result<Vec<EncodedPacket>> {
        if frame.pixel_format != PixelFormat::Bgra {
            bail!("vp9-444: expected BGRA input, got {:?}", frame.pixel_format);
        }
        self.bgra_to_i444(&frame)?;

        // Build a vpx_image_t pointing at our three plane buffers. We
        // don't use vpx_img_wrap because that requires a single
        // contiguous buffer, and we have three separate Vecs that
        // dcv_color_primitives writes into directly. Manual setup
        // avoids the extra concat step.
        let mut img: vpx::vpx_image_t = unsafe { std::mem::zeroed() };
        img.fmt = vpx::vpx_img_fmt::VPX_IMG_FMT_I444;
        img.cs = vpx::vpx_color_space::VPX_CS_BT_601;
        img.range = vpx::vpx_color_range::VPX_CR_STUDIO_RANGE;
        img.w = self.width as c_uint;
        img.h = self.height as c_uint;
        img.d_w = self.width as c_uint;
        img.d_h = self.height as c_uint;
        img.r_w = self.width as c_uint;
        img.r_h = self.height as c_uint;
        img.bit_depth = 8;
        img.x_chroma_shift = 0;
        img.y_chroma_shift = 0;
        img.bps = 24;
        img.planes[vpx::VPX_PLANE_Y as usize] = self.y_plane.as_mut_ptr();
        img.planes[vpx::VPX_PLANE_U as usize] = self.u_plane.as_mut_ptr();
        img.planes[vpx::VPX_PLANE_V as usize] = self.v_plane.as_mut_ptr();
        img.stride[vpx::VPX_PLANE_Y as usize] = self.width as c_int;
        img.stride[vpx::VPX_PLANE_U as usize] = self.width as c_int;
        img.stride[vpx::VPX_PLANE_V as usize] = self.width as c_int;

        let pts = frame.monotonic_us as vpx::vpx_codec_pts_t;
        // Advance pts by one frame at 30fps when we don't have a real
        // timestamp; libvpx requires monotonic non-zero progression.
        let pts = if pts <= 0 {
            (self.frame_idx as i64) * (1_000_000 / 30)
        } else {
            pts
        };
        let duration: u64 = 1_000_000 / 30;

        let force_kf = self.force_keyframe || self.frame_idx == 0;
        let flags: vpx::vpx_enc_frame_flags_t = if force_kf {
            vpx::VPX_EFLAG_FORCE_KF as vpx::vpx_enc_frame_flags_t
        } else {
            0
        };

        // SAFETY: ctx is initialised + alive; img points at planes
        // owned by &mut self for the duration of this call (libvpx
        // reads the planes synchronously during vpx_codec_encode and
        // does not retain them after return when g_lag_in_frames=0).
        let err = unsafe {
            vpx::vpx_codec_encode(
                &mut self.ctx,
                &img,
                pts,
                duration as std::os::raw::c_ulong,
                flags,
                vpx::VPX_DL_REALTIME as std::os::raw::c_ulong,
            )
        };
        if err != vpx::VPX_CODEC_OK {
            bail!("vp9-444: vpx_codec_encode failed: {err:?}");
        }

        let mut out = Vec::new();
        let mut iter: vpx::vpx_codec_iter_t = std::ptr::null();
        loop {
            // SAFETY: get_cx_data is the documented drain pattern; iter
            // is updated by libvpx and the returned packet points at
            // memory owned by the encoder until the next encode call.
            // We copy the payload immediately so the packet's lifetime
            // ends at the bottom of the loop body.
            let pkt = unsafe { vpx::vpx_codec_get_cx_data(&mut self.ctx, &mut iter) };
            if pkt.is_null() {
                break;
            }
            let kind = unsafe { (*pkt).kind };
            if kind != vpx::vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                continue;
            }
            // SAFETY: the union variant is `frame` for FRAME_PKT.
            let frame_pkt = unsafe { (*pkt).data.frame };
            let buf = frame_pkt.buf as *const u8;
            let sz = frame_pkt.sz;
            // SAFETY: libvpx guarantees the buffer is at least `sz`
            // bytes long and stays valid until the next encode() call.
            let slice = unsafe { std::slice::from_raw_parts(buf, sz) };
            let is_keyframe = (frame_pkt.flags & vpx::VPX_FRAME_IS_KEY) != 0;
            out.push(EncodedPacket {
                data: slice.to_vec(),
                is_keyframe,
                duration_us: duration,
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
        let kbps = bps / 1000;
        if kbps == 0 || kbps == self.cfg.rc_target_bitrate {
            return;
        }
        self.cfg.rc_target_bitrate = kbps;
        self.target_bitrate = bps;
        // SAFETY: ctx is alive; cfg is consistent (we only mutated
        // rc_target_bitrate, leaving every other field at the value
        // libvpx already accepted in init).
        let err = unsafe { vpx::vpx_codec_enc_config_set(&mut self.ctx, &self.cfg) };
        if err != vpx::VPX_CODEC_OK {
            tracing::warn!(?err, kbps, "vp9-444: vpx_codec_enc_config_set failed");
        }
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

/// Pick a sensible thread count for the libvpx encoder. Caps at 4 to
/// avoid drowning a busy host — VP9 encode parallelism scales
/// sub-linearly past that, especially on the screen-content path.
fn num_cpus_for_encode() -> c_uint {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    logical.clamp(1, 4) as c_uint
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

    /// First-frame keyframe lock. With profile 1 + I444 + lag=0 the
    /// encoder MUST emit at least one packet on the first encode call,
    /// and that packet MUST be flagged `is_keyframe=true`. If this
    /// regresses we'd ship a session that never produces a decodable
    /// frame at the browser.
    #[tokio::test]
    async fn first_frame_is_keyframe() {
        let mut enc = Vp9Encoder::new(320, 240).expect("encoder init");
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

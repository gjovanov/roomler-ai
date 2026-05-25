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
//!   - `cpu-used` defaults to 6, env-overridable via
//!     `ROOMLER_AGENT_VP9_CPU_USED` in 4..=9. Was 8 (fastest) pre-rc.33;
//!     6 matches RustDesk's default and doubles the per-macroblock
//!     mode-search budget so motion frames stop falling back to
//!     SKIP/INTRA at the CBR ceiling.
//!   - `lag-in-frames=0` — zero look-ahead, real-time priority
//!   - `kf-max-dist=∞` — periodic IDR disabled (rc.33). The 8 s
//!     periodic IDR competed with motion-frame bit budget; we now
//!     rely on `rc:vp9.request_keyframe` from the viewer + libvpx's
//!     internal scene-change detection (built into the screen-content
//!     tune). Matches RustDesk's pattern.
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

/// Initial bitrate target before any back-channel feedback. The
/// resolution-aware ceiling now lives in the VP9-444 pump (peer.rs):
/// it calls `set_bitrate(encode::initial_bitrate_for_fps(w, h, fps) *
/// quality_factor)` after every encoder rebuild so the encoder runs
/// at e.g. 25–30 Mbps at 4K Quality=High instead of the legacy 8 Mbps
/// flat cap. This constant remains as the boot-time default until the
/// first set_bitrate lands (typically the same loop iteration).
const DEFAULT_BITRATE_BPS: u32 = 8_000_000;

/// Keyframe interval in frames. 240 frames ≈ 8 s at 30 fps / 4 s at
/// 60 fps. Restored in rc.36 after the rc.33 `u32::MAX` value turned
/// out to produce visible "uncover-then-stabilise" blur on common
/// screen-content events (window maximize, app launch, Outlook open).
///
/// Why u32::MAX wasn't enough on its own (rc.33→rc.35): libvpx's
/// screen-content tune has internal scene-change detection that *can*
/// force an IDR on uncovered content, but its threshold is conservative
/// and we observed it not firing on common cases (window-restore,
/// notification toast, modal open). The post-event frame budget is
/// then spread across many deltas — each delta progressively adds
/// detail, producing the "blurry at first, sharp after a second" UX.
///
/// Restoring the periodic IDR every 240 frames is the simplest fix:
/// trades ~100 kbps of overhead for a hard quality-refresh floor.
/// Field-confirmed (the field-test host / a second field-test host, 2026-05-17).
///
/// Followup if this still leaves visible blur on rapid scene changes:
/// switch from `VPX_CBR` to `VPX_VBR` so the encoder can burst above
/// target on scene-change frames + dial in `VP9E_SET_CQ_LEVEL` for
/// content-aware QP.
const KEYFRAME_INTERVAL: u32 = 240;

/// Microsecond timebase numerator/denominator. PTS values are passed in
/// directly as microseconds.
const TIMEBASE_NUM: c_int = 1;
const TIMEBASE_DEN: c_int = 1_000_000;

/// VP9 chroma subsampling. Selects the encoder profile + the BGRA→YUV
/// conversion path + the size of the U/V plane buffers.
///
/// rc.61 — added so the operator can pick between sharpest-text
/// `Yuv444` (profile 1, default, ~1.5× bandwidth) and lower-bandwidth
/// `Yuv420` (profile 0, ~30% bandwidth saving, slight ClearType
/// softening on small text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vp9Chroma {
    /// VP9 profile 0 — horizontal+vertical 2:1 chroma subsampling. Same
    /// as WebRTC's default and what RustDesk uses by default.
    Yuv420,
    /// VP9 profile 1 — full 4:4:4 chroma. Currently the default; sharpest
    /// text rendering, needed for unaltered Windows ClearType.
    Yuv444,
}

impl Vp9Chroma {
    /// libvpx `g_profile` value (0 or 1).
    pub fn vpx_profile(self) -> c_uint {
        match self {
            Self::Yuv420 => 0,
            Self::Yuv444 => 1,
        }
    }
    /// String token used in `AgentCaps::vp9_chroma` + the browser side.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yuv420 => "yuv420",
            Self::Yuv444 => "yuv444",
        }
    }
}

pub struct Vp9Encoder {
    /// libvpx encoder context. Owned + freed in Drop.
    ctx: vpx::vpx_codec_ctx_t,
    /// Cached cfg so `set_bitrate` can mutate `rc_target_bitrate` and
    /// hand it back to libvpx via `vpx_codec_enc_config_set`.
    cfg: vpx::vpx_codec_enc_cfg_t,
    width: u32,
    height: u32,
    /// Target framerate this encoder was built for. Used to derive
    /// per-packet `duration_us` (and the synthetic PTS fallback when
    /// the input frame has no monotonic timestamp). Set at
    /// construction; the pump rebuilds the encoder on fps changes.
    target_fps: u32,
    /// Chroma format this encoder was built for. Drives plane sizing,
    /// BGRA→YUV conversion path, and the `img.fmt` / chroma-shift
    /// fields populated for each `vpx_codec_encode` call.
    chroma: Vp9Chroma,
    /// Reusable YUV plane buffers — re-allocated on resolution change.
    /// Y is always W*H; U/V are W*H for 4:4:4 or (W/2)*(H/2) for 4:2:0.
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
    /// Construct at the default 30 fps target + Yuv444 chroma (current
    /// default). Kept for tests and any caller that doesn't have an
    /// fps / chroma in scope.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        Self::new_with_fps_chroma(width, height, 30, Vp9Chroma::Yuv444)
    }

    /// Construct with an explicit target framerate. Defaults chroma to
    /// `Yuv444` (pre-rc.61 behaviour). Caller in `peer.rs` resolves
    /// chroma from env + handshake and uses [`new_with_fps_chroma`].
    pub fn new_with_fps(width: u32, height: u32, target_fps: u32) -> Result<Self> {
        Self::new_with_fps_chroma(width, height, target_fps, Vp9Chroma::Yuv444)
    }

    /// Construct with an explicit target framerate AND chroma format.
    /// rc.61 — the chroma param is new; it picks libvpx profile 0
    /// (`Yuv420`) or 1 (`Yuv444`) and drives the U/V plane sizing.
    pub fn new_with_fps_chroma(
        width: u32,
        height: u32,
        target_fps: u32,
        chroma: Vp9Chroma,
    ) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            bail!("vp9-444: require non-zero, even dimensions, got {width}x{height}");
        }
        let target_fps = target_fps.clamp(1, 240);

        // SAFETY: libvpx public API. We hold no aliasing references to
        // the cfg / ctx during these calls, the iface pointer is
        // returned by libvpx and lives for the process lifetime, and
        // we check every return code.
        //
        // `vpx_codec_enc_cfg_t` contains `g_bit_depth: vpx_bit_depth_t`
        // which is an enum with no 0 variant (starts at VPX_BITS_8 = 8),
        // so `mem::zeroed()` would produce an invalid niche and panic
        // under the rustc 1.95 invalid-value-in-niche check. Use
        // MaybeUninit and let `vpx_codec_enc_config_default` write the
        // whole struct before we treat any fields as initialised.
        let iface = unsafe { vpx::vpx_codec_vp9_cx() };
        if iface.is_null() {
            bail!("vp9-444: vpx_codec_vp9_cx() returned null — libvpx VP9 codec not linked");
        }
        let mut cfg_uninit = std::mem::MaybeUninit::<vpx::vpx_codec_enc_cfg_t>::uninit();
        let err = unsafe { vpx::vpx_codec_enc_config_default(iface, cfg_uninit.as_mut_ptr(), 0) };
        if err != vpx::VPX_CODEC_OK {
            bail!("vp9-444: vpx_codec_enc_config_default failed: {err:?}");
        }
        // SAFETY: vpx_codec_enc_config_default returned OK, which by
        // libvpx's contract means it fully initialised the cfg struct.
        let mut cfg: vpx::vpx_codec_enc_cfg_t = unsafe { cfg_uninit.assume_init() };

        // VP9 profile selects the chroma format the encoder accepts +
        // emits. Profile 0 = 8-bit 4:2:0, Profile 1 = 8-bit 4:4:4.
        // Browser's `VideoDecoder` MUST be configured with the matching
        // codec string (`vp09.00.10.08` vs `vp09.01.10.08`); a mismatch
        // leaves the canvas blank. rc.61 — picked from the `chroma`
        // parameter (env-var-driven via `peer.rs::vp9_chroma_from_env`).
        cfg.g_profile = chroma.vpx_profile();
        cfg.g_w = width as c_uint;
        cfg.g_h = height as c_uint;
        cfg.g_timebase = vpx::vpx_rational {
            num: TIMEBASE_NUM,
            den: TIMEBASE_DEN,
        };
        cfg.rc_target_bitrate = DEFAULT_BITRATE_BPS / 1000; // libvpx wants kbps
        // rc.42 — rate-control mode env-var opt-in. Default stays CBR
        // (pre-rc.42 behaviour); ROOMLER_AGENT_VP9_RC_MODE=vbr lets the
        // encoder burst on scene changes at the cost of a spikier
        // bitrate envelope. Plan rc.43 flips the default after one
        // cycle of field data. See [[plan]] cycle 1.1.
        let rc_mode = rc_mode_from_env();
        cfg.rc_end_usage = rc_mode;
        cfg.g_pass = vpx::vpx_enc_pass::VPX_RC_ONE_PASS;
        cfg.g_lag_in_frames = 0;
        cfg.g_threads = num_cpus_for_encode();
        cfg.g_error_resilient = 0;
        cfg.g_bit_depth = vpx::vpx_bit_depth::VPX_BITS_8;
        cfg.g_input_bit_depth = 8;
        cfg.kf_mode = vpx::vpx_kf_mode::VPX_KF_AUTO;
        cfg.kf_min_dist = 0;
        // rc.38 — scale kf_max_dist with target_fps so time-between-IDRs
        // stays roughly constant (~3 s) regardless of the actual
        // encoder frame rate. Field-observed on the field-test host (rc.36): with
        // target_fps=60 but encoder-bound at 17 actual fps, a 240-frame
        // kf_max_dist meant ~14 s between IDRs and visible "blur takes
        // >1s to clear" on window-uncover events. KEYFRAME_INTERVAL
        // is kept as a sanity floor in case target_fps is somehow zero.
        let kf_max_dist = (target_fps.saturating_mul(3))
            .max(60)
            .min(KEYFRAME_INTERVAL);
        cfg.kf_max_dist = kf_max_dist;
        // Real-time-friendly buffer sizing: 1s buffer at target bitrate
        // with a 0.5s initial / optimal floor. Matches RustDesk's defaults.
        cfg.rc_buf_sz = 1000;
        cfg.rc_buf_initial_sz = 500;
        cfg.rc_buf_optimal_sz = 600;
        cfg.rc_dropframe_thresh = 0;
        // RustDesk-aligned (rc.33): permissive undershoot, tight overshoot,
        // full QP range. Pre-rc.33 the 50/50 + max_q=56 combo forced libvpx
        // to drop *frames* once it hit the buffer ceiling under motion (it
        // couldn't raise QP past 56). Allowing the encoder to ride QP up
        // to 63 trades a momentary text-softness for a continuous frame
        // cadence — RustDesk's failure mode, materially smoother on
        // window-drag motion at 4K than our pre-rc.33 stutter mode.
        cfg.rc_undershoot_pct = 95;
        // rc.42 — VBR mode wants more overshoot headroom so scene-change
        // frames can splurge. CBR keeps the tight 25 % overshoot from
        // rc.33. Both modes share the same permissive 95 % undershoot
        // (idle frames go small no matter what).
        cfg.rc_overshoot_pct = if rc_mode == vpx::vpx_rc_mode::VPX_VBR {
            50
        } else {
            25
        };
        cfg.rc_min_quantizer = 0;
        cfg.rc_max_quantizer = 63;

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
        // Plane allocation. Y is always full-resolution W×H. U/V are
        // full-resolution for 4:4:4, quarter-resolution for 4:2:0
        // (horizontal+vertical 2:1 subsampling). For odd W/H we round
        // UP via integer division on `(W+1)/2 * (H+1)/2` so we don't
        // underrun on a (theoretically possible) odd-dim probe.
        let y_plane_size = (width as usize) * (height as usize);
        let uv_plane_size = match chroma {
            Vp9Chroma::Yuv444 => y_plane_size,
            Vp9Chroma::Yuv420 => {
                let cw = (width as usize).div_ceil(2);
                let ch = (height as usize).div_ceil(2);
                cw * ch
            }
        };
        let mut enc = Self {
            ctx,
            cfg,
            width,
            height,
            target_fps,
            chroma,
            y_plane: vec![0; y_plane_size],
            u_plane: vec![0; uv_plane_size],
            v_plane: vec![0; uv_plane_size],
            frame_idx: 0,
            force_keyframe: true, // first frame is always keyframe
            target_bitrate: DEFAULT_BITRATE_BPS,
        };
        enc.apply_screen_content_controls();
        Ok(enc)
    }

    fn apply_screen_content_controls(&mut self) {
        // CPUUSED default 6 (RustDesk-aligned). Doubles the mode-search
        // budget per macroblock vs the pre-rc.33 default of 8, which
        // restores motion-frame quality at the cost of ~35-45% more
        // encode CPU on the encode thread. Operator escape hatch via
        // `ROOMLER_AGENT_VP9_CPU_USED` (clamped 4..=9) lets a CPU-
        // starved host roll back to 7 or 8 without a rebuild.
        let cpu_used = cpu_used_from_env();
        self.set_ctrl(
            vpx::vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int,
            cpu_used,
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
        // Resolution-adaptive tile-columns (rc.33). At 4K width, fixing
        // log2=2 (4 tile columns / 960px each) bottlenecked deadline
        // mode encoding on the slowest tile. log2=4 (16 cols / 240px)
        // halves the per-tile width and lets encode + decode threads
        // chew through tiles in parallel. RustDesk uses the same ladder.
        let tile_cols_log2: c_int = if self.width >= 3840 {
            4
        } else if self.width >= 1920 {
            2
        } else {
            1
        };
        self.set_ctrl(
            vpx::vp8e_enc_control_id::VP9E_SET_TILE_COLUMNS as c_int,
            tile_cols_log2,
            "VP9E_SET_TILE_COLUMNS",
        );
        // Row-MT (rc.33). Parallelises encode within each tile column;
        // combined with the wider tile-column count at 4K this gives
        // ~2x encode-fps headroom on the realtime deadline. Defaulted
        // off in libvpx pre-1.7, but we ship 1.12 via env-libvpx-sys.
        self.set_ctrl(
            vpx::vp8e_enc_control_id::VP9E_SET_ROW_MT as c_int,
            1 as c_uint,
            "VP9E_SET_ROW_MT",
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

    /// Runtime cpu-used override. Used by the pump's motion-driven
    /// dynamic-cpu-used heuristic: bump from base (typically 6, env-
    /// configurable) to a faster preset (typically 8) when a
    /// scene-change spike fires, then drop back after motion subsides.
    /// Saves ~40-60 % per-frame encode time on iGPU-class hardware
    /// (the field-test host's Iris Xe is the field-validated case) where SW VP9
    /// 4:4:4 at 1920×1200 is CPU-bound to 8-12 fps with cpu-used=6.
    /// Quality drop is ~20 % per-frame, mostly invisible during motion
    /// (which is when this fires).
    pub fn set_speed(&mut self, cpu_used: c_int) {
        let clamped = cpu_used.clamp(0, 9);
        self.set_ctrl(
            vpx::vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int,
            clamped,
            "VP8E_SET_CPUUSED (runtime)",
        );
    }

    /// Uses `dcv_color_primitives` AVX2 path on x86_64 with SSE2
    /// fallback. In-place into `self.{y,u,v}_plane`.
    ///
    /// Convert BGRA → I444 or I420 into our reusable plane buffers,
    /// selected by `self.chroma`. Pre-rc.61 this was `bgra_to_i444`;
    /// the I420 path was added in rc.61 to support VP9 profile 0 for
    /// lower-bandwidth sessions.
    fn bgra_to_yuv(&mut self, frame: &Frame) -> Result<()> {
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
        let (dst_pixfmt, uv_stride) = match self.chroma {
            Vp9Chroma::Yuv444 => (dcv::PixelFormat::I444, self.width as usize),
            Vp9Chroma::Yuv420 => (dcv::PixelFormat::I420, (self.width as usize).div_ceil(2)),
        };
        let dst_format = dcv::ImageFormat {
            pixel_format: dst_pixfmt,
            color_space: dcv::ColorSpace::Bt601,
            num_planes: 3,
        };
        let src_buffers: &[&[u8]] = &[&frame.data];
        let src_strides = &[(frame.width * 4) as usize];
        let dst_buffers: &mut [&mut [u8]] =
            &mut [&mut self.y_plane, &mut self.u_plane, &mut self.v_plane];
        let dst_strides = &[self.width as usize, uv_stride, uv_stride];
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
        .map_err(|e| anyhow!("dcv BGRA→YUV failed: {e:?}"))?;
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
        self.bgra_to_yuv(&frame)?;

        // Build a vpx_image_t pointing at our three plane buffers. We
        // don't use vpx_img_wrap because that requires a single
        // contiguous buffer, and we have three separate Vecs that
        // dcv_color_primitives writes into directly. Manual setup
        // avoids the extra concat step.
        //
        // Chroma-dependent fields:
        //   - fmt: I444 vs I420
        //   - x_chroma_shift / y_chroma_shift: 0 for full chroma,
        //     1 for half-resolution chroma (each shift = ÷2)
        //   - bps: 24 (4:4:4 has 1 luma + 2 chroma samples per pixel)
        //          vs 12 (4:2:0 averages chroma over 4 pixels)
        //   - U/V stride: width for 4:4:4, width/2 for 4:2:0
        let mut img: vpx::vpx_image_t = unsafe { std::mem::zeroed() };
        let (img_fmt, x_shift, y_shift, bps, uv_stride) = match self.chroma {
            Vp9Chroma::Yuv444 => (
                vpx::vpx_img_fmt::VPX_IMG_FMT_I444,
                0u32,
                0u32,
                24,
                self.width as c_int,
            ),
            Vp9Chroma::Yuv420 => (
                vpx::vpx_img_fmt::VPX_IMG_FMT_I420,
                1u32,
                1u32,
                12,
                ((self.width as usize).div_ceil(2)) as c_int,
            ),
        };
        img.fmt = img_fmt;
        img.cs = vpx::vpx_color_space::VPX_CS_BT_601;
        img.range = vpx::vpx_color_range::VPX_CR_STUDIO_RANGE;
        img.w = self.width as c_uint;
        img.h = self.height as c_uint;
        img.d_w = self.width as c_uint;
        img.d_h = self.height as c_uint;
        img.r_w = self.width as c_uint;
        img.r_h = self.height as c_uint;
        img.bit_depth = 8;
        img.x_chroma_shift = x_shift;
        img.y_chroma_shift = y_shift;
        img.bps = bps;
        img.planes[vpx::VPX_PLANE_Y as usize] = self.y_plane.as_mut_ptr();
        img.planes[vpx::VPX_PLANE_U as usize] = self.u_plane.as_mut_ptr();
        img.planes[vpx::VPX_PLANE_V as usize] = self.v_plane.as_mut_ptr();
        img.stride[vpx::VPX_PLANE_Y as usize] = self.width as c_int;
        img.stride[vpx::VPX_PLANE_U as usize] = uv_stride;
        img.stride[vpx::VPX_PLANE_V as usize] = uv_stride;

        let pts = frame.monotonic_us as vpx::vpx_codec_pts_t;
        // Advance pts by one frame at the configured target fps when we
        // don't have a real timestamp; libvpx requires monotonic
        // non-zero progression.
        let frame_duration_us: i64 = 1_000_000 / (self.target_fps as i64).max(1);
        let pts = if pts <= 0 {
            (self.frame_idx as i64) * frame_duration_us
        } else {
            pts
        };
        let duration: u64 = frame_duration_us as u64;

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
            // `sz` field width varies across env-libvpx-sys's
            // pre-generated bindings (u32 on the 1.12 binding used by
            // the Windows release path; usize on 1.13 + the
            // bindgen-against-system-headers path used on Linux/macOS).
            // Cast through usize unconditionally — the value is a byte
            // count of an encoder packet, no realistic risk of overflow
            // on 64-bit hosts.
            let sz = frame_pkt.sz as usize;
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

/// Pick a sensible thread count for the libvpx encoder. Cap raised
/// from 4 → 8 in rc.33: with the resolution-adaptive tile-columns
/// (log2=4 at 4K → 16 tile columns) plus row-mt, encode now scales
/// usefully past 4 threads. The 8-thread ceiling still protects
/// a busy host from drowning under VP9 work — past 8 threads VP9
/// encode parallelism flatlines on the screen-content path.
fn num_cpus_for_encode() -> c_uint {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    logical.clamp(1, 8) as c_uint
}

/// Read the `ROOMLER_AGENT_VP9_CPU_USED` escape hatch. Default 6
/// (RustDesk-aligned, motion-quality optimal). Clamp to 4..=9 — values
/// outside that range are libvpx-internally undefined for VP9. Any
/// unparseable / unset value falls back to the default.
pub(crate) fn cpu_used_from_env() -> c_int {
    const DEFAULT_CPU_USED: i32 = 6;
    let raw = std::env::var("ROOMLER_AGENT_VP9_CPU_USED")
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .unwrap_or(DEFAULT_CPU_USED);
    raw.clamp(4, 9) as c_int
}

/// Read the `ROOMLER_AGENT_VP9_RC_MODE` env var. Default `cbr` (pre-
/// rc.42 behaviour). Accepted: `cbr` | `vbr` | `cq`. Any unrecognised
/// value falls back to `cbr` with a debug-log line.
///
/// rc.42 ships this behind an env var (default cbr); rc.43 flips the
/// default to vbr after one field cycle on the field-test host confirms the
/// envelope is acceptable.
/// Resolve the VP9 chroma format for this session. rc.61.
///
/// Env var `ROOMLER_AGENT_VP9_CHROMA` values:
///   - `""` / `"yuv444"` / `"444"` → [`Vp9Chroma::Yuv444`] (default).
///   - `"yuv420"` / `"420"` → [`Vp9Chroma::Yuv420`].
///
/// Lowercase + whitespace tolerant; anything else logs at debug and
/// falls back to Yuv444 (the pre-rc.61 default — preserves behaviour
/// for hosts that don't know about the new knob).
pub fn vp9_chroma_from_env() -> Vp9Chroma {
    let raw = std::env::var("ROOMLER_AGENT_VP9_CHROMA").unwrap_or_default();
    let parsed = match raw.trim().to_ascii_lowercase().as_str() {
        "" | "yuv444" | "444" => Vp9Chroma::Yuv444,
        "yuv420" | "420" => Vp9Chroma::Yuv420,
        other => {
            tracing::debug!(
                value = other,
                "vp9-444: unrecognised ROOMLER_AGENT_VP9_CHROMA — falling back to yuv444"
            );
            Vp9Chroma::Yuv444
        }
    };
    tracing::info!(
        chroma = parsed.as_str(),
        env_value = %raw,
        "vp9-444: chroma format selected"
    );
    parsed
}

fn rc_mode_from_env() -> vpx::vpx_rc_mode {
    let raw = std::env::var("ROOMLER_AGENT_VP9_RC_MODE").unwrap_or_default();
    let parsed = match raw.trim().to_ascii_lowercase().as_str() {
        "" | "cbr" => vpx::vpx_rc_mode::VPX_CBR,
        "vbr" => vpx::vpx_rc_mode::VPX_VBR,
        "cq" => vpx::vpx_rc_mode::VPX_CQ,
        other => {
            tracing::debug!(
                value = other,
                "vp9-444: unrecognised ROOMLER_AGENT_VP9_RC_MODE — falling back to cbr"
            );
            vpx::vpx_rc_mode::VPX_CBR
        }
    };
    tracing::info!(
        mode = ?parsed,
        env_value = %raw,
        "vp9-444: rate-control mode selected"
    );
    parsed
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

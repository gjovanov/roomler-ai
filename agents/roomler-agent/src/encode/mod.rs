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
use tunnel_core::env::node_env;

use crate::capture::{DirtyRect, Frame};

pub mod caps;
pub mod color;

#[cfg(feature = "openh264-encoder")]
pub mod openh264_backend;

#[cfg(feature = "vp9-444")]
pub mod libvpx;

#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
pub mod mf;

// rc.64 — Option B HEVC plan. `ffmpeg-encoder` ships header-only here:
// the module declares zero callers and `available()` returns false, so
// every release build with the feature flipped on or off behaves
// identically. The CI plumbing that links stripped FFmpeg + libmfx is
// rc.65; the actual encoder backend is rc.66. See
// `docs/hevc-dc-plan.md` (rc.64) for the phased rollout.
#[cfg(feature = "ffmpeg-encoder")]
pub mod ffmpeg;

// Shared AIMD bitrate controller for the DataChannel pumps (VP9-444 +
// FFmpeg). Always compiled — it's pure (no ffmpeg/webrtc types), so its
// unit tests run on the default `cargo test --lib`. The pump features are
// what USE it, so allow dead_code on the signalling-only build to keep
// `clippy -D warnings` clean (mirrors `transport_is_constrained` below).
#[cfg_attr(
    not(any(feature = "vp9-444", feature = "ffmpeg-encoder")),
    allow(dead_code)
)]
pub mod aimd;

// Viewer-rate controller (rc.188) — folds the browser's measured `rc:decodestat`
// (decoded fps + struggling) into a send-fps cap for the DC pumps, so the agent
// settles at the viewer's real sustainable rate instead of firehosing 60 fps and
// relying on the (harmful) keyframe-storm the rc.184 `decode_pressure` heuristic
// tried and failed to break. Pure (unit-tested on the default build); only the
// pump features USE it.
#[cfg_attr(
    not(any(feature = "vp9-444", feature = "ffmpeg-encoder")),
    allow(dead_code)
)]
pub mod viewer_rate;

// Encode-pressure controller — auto-reduces the maxrate ceiling when the
// SENDER's encoder saturates (avg encode time high), the dynamic version of
// the field-proven `FFMPEG_FPS=30`. Pure (unit-tested on the default build);
// only `media_pump_ffmpeg_dc` (the ffmpeg-encoder feature) USES it, so the
// dead_code allow is keyed on that feature alone.
#[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
pub mod encode_pressure;

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
/// MAX bumped 25→40 Mbps in rc.36. Field-confirmed (the field-test host) that
/// rc.35 at 1920×1200 Quality=High was content-bound around 13 Mbps,
/// well under the 25 Mbps cap — but `Quality=High × 1.5` math could
/// land above 25 Mbps at 4K@60 and was getting clipped. Lifting the
/// MAX gives Quality=High more room before the post-multiply clamp
/// in `quality::target_bitrate` kicks in, and gives the AIMD's
/// additive-increase headroom on the DC backpressure controller.
pub const MAX_BITRATE_BPS: u32 = 40_000_000;

pub(crate) fn initial_bitrate_for_fps(width: u32, height: u32, fps: u32) -> u32 {
    // bpp/s bumped 0.15 → 0.20 in rc.36. RustDesk's published default is
    // ≈ 0.14–0.18; field reports (the field-test host, a second field-test host, 2026-05-17)
    // showed that 0.15 left desktop content visibly under-bitted at
    // 1920×1200 — fine text on Outlook / Start menu / Notepad++ took
    // multiple frames to sharpen after a window-uncover event. 0.20
    // gives the encoder ~33 % more bits, which combined with the
    // restored 240-frame keyframe interval lets a refresh land sharp.
    const DESKTOP_BPP_PER_SECOND: f64 = 0.20;
    let pixels = width as f64 * height as f64;
    let raw = (pixels * fps as f64 * DESKTOP_BPP_PER_SECOND) as u32;
    raw.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS)
}

/// True when the ICE transport is forced to a TURN relay — on TCP (WSL,
/// corp-UDP-blocked nets) that path is bandwidth- + head-of-line-constrained.
/// Set by virtual-desktop mode and the corp path via ROOMLER_AGENT_ICE_RELAY_TCP.
///
/// Only the VP9-444 DC pump (`vp9-444`) and the FFmpeg DC pump
/// (`ffmpeg-encoder`) consume this; the default-feature build has neither, so
/// the `dead_code` allow keeps the signalling-only CI build warning-clean
/// (mirrors `initial_bitrate_for`'s feature guard above).
#[cfg_attr(
    not(any(feature = "vp9-444", feature = "ffmpeg-encoder")),
    allow(dead_code)
)]
pub(crate) fn transport_is_constrained() -> bool {
    node_env("ICE_RELAY_TCP")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Bitrate ceiling (bps) for a constrained relay-TCP transport. Default 3 Mbps;
/// override with ROOMLER_AGENT_RELAY_MAX_KBPS. A single TURN-TCP relay carries
/// ~1-4 Mbps; the VP9-444 0.20-bpp ~12 Mbps target collapses it (27s freeze).
///
/// See `transport_is_constrained` for why the `dead_code` allow is keyed on the
/// pump features (the `mod tests` use below does not count for a non-test build).
#[cfg_attr(
    not(any(feature = "vp9-444", feature = "ffmpeg-encoder")),
    allow(dead_code)
)]
pub(crate) fn relay_max_bps() -> u32 {
    node_env("RELAY_MAX_KBPS")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|k| *k > 0)
        .map(|k| k.saturating_mul(1000))
        .unwrap_or(3_000_000)
}

/// rc.190 (B1) — long-edge RESOLUTION cap for a constrained relay-TCP
/// transport. `relay_max_bps` caps the bitrate at ~3 Mbps, but a 2560×1600
/// stream at 3 Mbps starves into the blur↔crystallize AIMD sawtooth (field
/// NEO16→PC50045 2026-07-16) — fewer pixels per bit is the actual fix, so the
/// DC pumps also cap the encode resolution. Default 1280 long edge (≈1280×800
/// at 3 Mbps is smooth); env `ROOMLER_AGENT_RELAY_MAX_EDGE`, `0` disables.
/// Hard cap: clamps even an explicit controller pick (it's link physics).
#[cfg_attr(
    not(any(feature = "vp9-444", feature = "ffmpeg-encoder")),
    allow(dead_code)
)]
pub(crate) fn relay_res_cap_long_edge() -> Option<u32> {
    let v = std::env::var("ROOMLER_AGENT_RELAY_MAX_EDGE")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(1280);
    (v > 0).then_some(v)
}

/// rc.190 (B2) — long-edge RESOLUTION cap for the SOFTWARE-encoded DC pump
/// (libvpx). Mirrors the RTP pump's SW auto-downscale: a 4K panel through
/// libvpx crawls (~25 fps at cpu-used 6, host CPU pinned — field GEAL8N6
/// 2026-07-16). Default 1920 long edge; env `ROOMLER_AGENT_SW_MAX_EDGE`,
/// `0` disables. Soft cap: fills in only when the controller left resolution
/// at Native — an explicit rc:resolution pick wins ("operator can override",
/// same contract as the RTP pump's auto-downscale).
#[cfg_attr(not(feature = "vp9-444"), allow(dead_code))]
pub(crate) fn sw_res_cap_long_edge() -> Option<u32> {
    let v = std::env::var("ROOMLER_AGENT_SW_MAX_EDGE")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(1920);
    (v > 0).then_some(v)
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
///
/// rc.72: when `ROOMLER_AGENT_USE_FFMPEG=1` is set AND the
/// `ffmpeg-encoder` feature is compiled in, try the FFmpeg backend first
/// (`hevc_nvenc` → `hevc_qsv` → `hevc_amf`). Falls through to MF on
/// FFmpeg construction failure so a misconfigured opt-in doesn't break
/// existing sessions. Unset = MF default (preserves rc.71 behaviour).
fn open_for_codec_hevc(width: u32, height: u32) -> (Box<dyn VideoEncoder>, &'static str) {
    #[cfg(feature = "ffmpeg-encoder")]
    {
        if ffmpeg::available() {
            match ffmpeg::FfmpegEncoder::new_hevc(width, height) {
                Ok(e) => {
                    tracing::info!(
                        width,
                        height,
                        encoder = e.name(),
                        "encoder selected: ffmpeg HEVC (hardware via vendor SDK)"
                    );
                    return (Box::new(e), "h265");
                }
                Err(err) => {
                    tracing::warn!(
                        %err,
                        "ROOMLER_AGENT_USE_FFMPEG=1 but ffmpeg HEVC construction failed; falling back to MF cascade"
                    );
                }
            }
        }
    }
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
    node_env("HW_AUTO")
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

    // rc.191 — BOTH tests below read/write ROOMLER_AGENT_RELAY_MAX_KBPS;
    // cargo's parallel runner interleaved them once the peer::tests grew
    // (field flake 2026-07-16: the clamp test observed the reader test's
    // mid-flight "4200" write). Serialise them on one lock.
    static RELAY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn relay_max_bps_reads_env() {
        let _guard = RELAY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Hermetic: save the prior value, exercise set/unset, then restore.
        // SAFETY: same reasoning as `hw_auto_disabled_reads_env` — no other
        // code in this crate touches ROOMLER_AGENT_RELAY_MAX_KBPS at test
        // time, and the tests that do share RELAY_ENV_LOCK.
        let prior = std::env::var("ROOMLER_AGENT_RELAY_MAX_KBPS").ok();

        unsafe { std::env::remove_var("ROOMLER_AGENT_RELAY_MAX_KBPS") };
        assert_eq!(
            relay_max_bps(),
            3_000_000,
            "unset defaults to the 3 Mbps relay ceiling"
        );

        unsafe { std::env::set_var("ROOMLER_AGENT_RELAY_MAX_KBPS", "1500") };
        assert_eq!(relay_max_bps(), 1_500_000, "kbps env is multiplied by 1000");

        // Whitespace-trimmed + a 0 / garbage value falls back to the default.
        unsafe { std::env::set_var("ROOMLER_AGENT_RELAY_MAX_KBPS", "  4200 ") };
        assert_eq!(relay_max_bps(), 4_200_000, "value is trimmed before parse");
        unsafe { std::env::set_var("ROOMLER_AGENT_RELAY_MAX_KBPS", "0") };
        assert_eq!(relay_max_bps(), 3_000_000, "0 is rejected → default");
        unsafe { std::env::set_var("ROOMLER_AGENT_RELAY_MAX_KBPS", "nope") };
        assert_eq!(relay_max_bps(), 3_000_000, "non-numeric → default");

        // Restore the pre-test environment.
        match prior {
            Some(v) => unsafe { std::env::set_var("ROOMLER_AGENT_RELAY_MAX_KBPS", v) },
            None => unsafe { std::env::remove_var("ROOMLER_AGENT_RELAY_MAX_KBPS") },
        }
    }

    #[test]
    fn relay_clamp_caps_vp9_444_target() {
        let _guard = RELAY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The `x.min(relay_max_bps())` clamp the pump applies must pull a
        // 0.20-bpp 2560×1600@30 VP9-444 target (12_441_600 bps) down to
        // the 3 Mbps relay ceiling.
        let prior = std::env::var("ROOMLER_AGENT_RELAY_MAX_KBPS").ok();
        unsafe { std::env::remove_var("ROOMLER_AGENT_RELAY_MAX_KBPS") };

        let vp9_444_target: u32 = 12_441_600;
        assert_eq!(vp9_444_target.min(relay_max_bps()), 3_000_000);

        match prior {
            Some(v) => unsafe { std::env::set_var("ROOMLER_AGENT_RELAY_MAX_KBPS", v) },
            None => unsafe { std::env::remove_var("ROOMLER_AGENT_RELAY_MAX_KBPS") },
        }
    }
}

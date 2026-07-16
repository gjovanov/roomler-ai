//! Screen capture abstraction.
//!
//! Trait + concrete backends. `scrap_backend::ScrapCapture` is the default
//! for any OS scrap supports (Linux/X11 via XShm, Windows via DXGI,
//! macOS via CGDisplayStream); `NoopCapture` is a fallback that never
//! yields frames, used when a display is not available.
//!
//! Higher layers pick via `capture::open_default()`; individual backends
//! can also be constructed directly for tests.

use anyhow::Result;

#[cfg(feature = "scrap-capture")]
pub mod scrap_backend;

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod wgc_backend;

/// Phase 1 — Linux CI / agent-e2e Pod path. Substitutes a deterministic
/// 320×240 BGRA frame source for scrap-capture so a headless Pod
/// without an X server can still drive the encode + WebRTC pipeline.
/// `open_default` short-circuits to this backend when the runtime
/// env var `ROOMLER_AGENT_SYNTHETIC_FRAMES=1` is set AND the binary
/// was compiled with `--features synthetic-frame-source`.
#[cfg(feature = "synthetic-frame-source")]
pub mod synthetic_backend;

pub mod cursor;

/// A captured frame, in an encoder-agnostic representation.
///
/// We don't commit to a specific colour space in the trait — backends can
/// emit BGRA (WGC/XShm default) and the encoder converts. Width/height may
/// change mid-session (e.g. laptop dock) which is why they're per-frame.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: PixelFormat,
    pub data: Vec<u8>,
    pub monotonic_us: u64,
    /// Screen index that produced this frame. Matches `DisplayInfo::index`
    /// in the `rc:agent.hello` message.
    pub monitor: u8,
    /// Per-frame dirty regions. Empty = unknown / full-frame; the
    /// encoder treats every macroblock as potentially dirty in that
    /// case (matches scrap behaviour today). Backends that expose a
    /// dirty-rect API (Windows.Graphics.Capture, PipeWire damage
    /// events) populate this so the encoder can apply ROI delta-QP
    /// or skip encode entirely on idle frames (1F.1 / 1D.1).
    pub dirty_rects: Vec<DirtyRect>,
}

/// A rectangular region of a frame that changed since the previous
/// frame. Coordinates are in source pixels (post-downscale if the
/// capture backend downscales). Width/height are exclusive — the
/// rect covers `[x, x+w)` × `[y, y+h)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Shape + hotspot of an OS cursor. The agent emits this once per
/// shape change; the browser caches by the `shape_id` in the wire
/// message so it only decodes the ARGB bitmap once per shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorInfo {
    pub width: u32,
    pub height: u32,
    /// Hotspot offset in pixels relative to the top-left of the bitmap.
    /// Arrow cursors usually have (0, 0); I-beam is near the centre.
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    /// 32-bit BGRA pixels, top-down (row 0 = top).
    pub bgra: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Nv12,
    I420,
}

/// Whether the capture layer should downscale high-resolution sources
/// before handing frames to the encoder.
///
/// - `Auto`: the backend picks — scrap currently triggers a 2× box
///   downsample above ~3.5 Mpx because software openh264 can't keep up
///   at native 4K.
/// - `Always`: force the 2× downsample regardless of source size
///   (reserved for debugging / low-bandwidth modes).
/// - `Never`: always send native resolution. Use this only when the
///   chosen encoder can sustain the source rate — MF / NVENC / VAAPI
///   handle 4K fine; openh264 software does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DownscalePolicy {
    #[default]
    Auto,
    Always,
    Never,
}

#[async_trait::async_trait]
pub trait ScreenCapture: Send {
    async fn next_frame(&mut self) -> Result<Option<Frame>>;
    fn monitor_count(&self) -> u8;
}

/// A capture backend that never produces frames. Used when no display is
/// available (headless host, CI with no $DISPLAY) so higher layers can keep
/// ticking without panicking.
pub struct NoopCapture;

#[async_trait::async_trait]
impl ScreenCapture for NoopCapture {
    async fn next_frame(&mut self) -> Result<Option<Frame>> {
        // Park the task — real backends would block on a GPU fence or a
        // PipeWire readable.
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        Ok(None)
    }
    fn monitor_count(&self) -> u8 {
        0
    }
}

/// Open the best-available capture backend for the current host. Falls
/// back to [`NoopCapture`] if no display is reachable or the crate was
/// built without a capture backend feature.
///
/// `downscale` controls whether the backend runs its 2× box filter on
/// high-resolution sources. Pass `DownscalePolicy::Never` when a
/// hardware encoder is handling the frame; pass `Auto` (the default)
/// when the encoder is software openh264.
pub fn open_default(_target_fps: u32, _downscale: DownscalePolicy) -> Box<dyn ScreenCapture> {
    // Phase 1 — synthetic-frame-source short-circuit. When the agent
    // is running inside the agent-e2e Pod (or any headless CI
    // context that sets the env var), bypass the scrap / WGC /
    // system-context cascade entirely. The synthetic backend has no
    // system deps and produces deterministic 320×240 BGRA frames
    // so encode + WebRTC end-to-end can be exercised without an X
    // server or a real screen. Production agents never compile the
    // feature in; even with the feature, the env var must be set —
    // belt-and-suspenders so a stray production env var can't silently
    // replace real capture with a synthetic stream.
    #[cfg(feature = "synthetic-frame-source")]
    {
        if synthetic_env_enabled() {
            let cap = synthetic_backend::primary(_target_fps, _downscale);
            tracing::info!(
                width = synthetic_backend::FRAME_W,
                height = synthetic_backend::FRAME_H,
                "capture: backend=synthetic (ROOMLER_AGENT_SYNTHETIC_FRAMES=1, CI / agent-e2e Pod)"
            );
            return Box::new(cap);
        }
    }

    // M3 A1: when the worker is running as SYSTEM (LocalSystem,
    // S-1-5-18) — i.e. spawned by the SCM service via
    // `winlogon_token::spawn_system_in_session` — WGC's WinRT
    // activation chain returns `0x80070424 (ERROR_SERVICE_DOES_NOT_
    // EXIST)` because the activation service doesn't exist in
    // session 0's namespace. Route directly to DXGI Desktop
    // Duplication (with GDI BitBlt fallback) via the
    // `system_context::capture_pump` bridge. User-context workers
    // continue to take the WGC / scrap path below.
    #[cfg(all(feature = "system-context", target_os = "windows"))]
    {
        use crate::system_context::worker_role::{WorkerRole, probe_self};
        match probe_self() {
            Ok(WorkerRole::SystemContext) => {
                match crate::system_context::capture_pump::SystemContextCapture::primary(
                    _target_fps,
                    _downscale,
                ) {
                    Ok(c) => {
                        tracing::info!(
                            width = c.width(),
                            height = c.height(),
                            "capture: backend=system-context (DXGI + GDI fallback for SYSTEM-context worker)"
                        );
                        return Box::new(c);
                    }
                    Err(e) => {
                        tracing::warn!(%e, "system-context capture init failed — falling back to standard backend cascade");
                    }
                }
            }
            Ok(WorkerRole::User) => {
                // Normal path. Fall through to the WGC / scrap cascade
                // below — same behaviour as a build without the
                // `system-context` feature.
            }
            Err(e) => {
                tracing::warn!(%e, "worker_role::probe_self failed — assuming user-context, falling through to standard cascade");
            }
        }
    }

    // Windows: prefer WGC (captures HW cursors + supports dirty rects
    // on Win 11 22000+). Fall back to scrap (DXGI) if WGC init fails
    // — e.g. on Windows versions without the Graphics.Capture runtime
    // or broken WinRT. Escape hatch: `ROOMLER_AGENT_CAPTURE=scrap` forces
    // the DXGI path without a rebuild.
    #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
    {
        if !capture_env_prefers_scrap() {
            match wgc_backend::WgcCapture::primary(_target_fps, _downscale) {
                Ok(c) => {
                    tracing::info!(
                        width = c.width(),
                        height = c.height(),
                        "capture: backend=wgc (Windows.Graphics.Capture)"
                    );
                    return Box::new(c);
                }
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "wgc capture unavailable — falling back to scrap (DXGI)"
                    );
                }
            }
        } else {
            tracing::info!("ROOMLER_AGENT_CAPTURE=scrap — skipping WGC, using DXGI via scrap");
        }
    }
    #[cfg(feature = "scrap-capture")]
    {
        match scrap_backend::ScrapCapture::primary(_target_fps, _downscale) {
            Ok(c) => {
                tracing::info!(
                    width = c.width(),
                    height = c.height(),
                    "capture: backend=scrap (DXGI/XShm/CoreGraphics)"
                );
                return Box::new(c);
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "scrap capture unavailable — falling back to NoopCapture"
                );
            }
        }
    }
    #[cfg(not(feature = "scrap-capture"))]
    {
        tracing::info!(
            "built without scrap-capture feature — using NoopCapture. \
             Rebuild with `--features scrap-capture` for real screen capture."
        );
    }
    Box::new(NoopCapture)
}

/// Escape hatch: `ROOMLER_AGENT_CAPTURE=scrap` (case-insensitive) forces
/// the DXGI path even on builds that include WGC. Useful for diagnosing
/// WGC-specific regressions in the field without a rebuild.
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn capture_env_prefers_scrap() -> bool {
    use tunnel_core::env::node_env;
    node_env("CAPTURE")
        .map(|v| v.trim().eq_ignore_ascii_case("scrap"))
        .unwrap_or(false)
}

/// Phase 1 — runtime gate for the synthetic-frame-source backend.
/// True iff `ROOMLER_AGENT_SYNTHETIC_FRAMES` parses as truthy
/// (`1` / `true` / `yes` / `on`, case-insensitive). Anything else
/// (unset, `0`, garbage) falls back to the normal cascade.
#[cfg(feature = "synthetic-frame-source")]
fn synthetic_env_enabled() -> bool {
    use tunnel_core::env::node_env;
    match node_env("SYNTHETIC_FRAMES") {
        Some(v) => {
            let t = v.trim();
            t.eq_ignore_ascii_case("1")
                || t.eq_ignore_ascii_case("true")
                || t.eq_ignore_ascii_case("yes")
                || t.eq_ignore_ascii_case("on")
        }
        None => false,
    }
}

#[cfg(all(test, feature = "synthetic-frame-source"))]
mod synthetic_env_tests {
    use super::synthetic_env_enabled;

    /// SAFETY: env tests must run serially because `std::env::set_var`
    /// is process-wide. We use a Mutex to enforce that — Rust's
    /// `#[test]` doesn't guarantee serial execution per-module by
    /// default.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env<F: FnOnce()>(key: &str, val: Option<&str>, f: F) {
        let _guard = LOCK.lock().unwrap();
        let prior = std::env::var(key).ok();
        match val {
            // SAFETY: serialised by LOCK; restored before the guard
            // is dropped. Std flags set_var as unsafe in 2024 ed.
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
        f();
        match prior {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    #[test]
    fn unset_returns_false() {
        with_env("ROOMLER_AGENT_SYNTHETIC_FRAMES", None, || {
            assert!(!synthetic_env_enabled());
        });
    }

    #[test]
    fn truthy_values_accepted() {
        for v in &["1", "true", "TRUE", "yes", "On"] {
            with_env("ROOMLER_AGENT_SYNTHETIC_FRAMES", Some(v), || {
                assert!(synthetic_env_enabled(), "value {v:?} should be truthy");
            });
        }
    }

    #[test]
    fn explicit_zero_or_garbage_is_false() {
        for v in &["0", "false", "no", "off", "anything-else"] {
            with_env("ROOMLER_AGENT_SYNTHETIC_FRAMES", Some(v), || {
                assert!(!synthetic_env_enabled(), "value {v:?} should be falsy");
            });
        }
    }
}

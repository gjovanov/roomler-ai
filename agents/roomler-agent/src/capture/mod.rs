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
    fn monitor_count(&self) -> u8 { 0 }
}

/// Open the best-available capture backend for the current host. Falls
/// back to [`NoopCapture`] if no display is reachable or the crate was
/// built without a capture backend feature.
///
/// `downscale` controls whether the backend runs its 2× box filter on
/// high-resolution sources. Pass `DownscalePolicy::Never` when a
/// hardware encoder is handling the frame; pass `Auto` (the default)
/// when the encoder is software openh264.
pub fn open_default(
    _target_fps: u32,
    _downscale: DownscalePolicy,
) -> Box<dyn ScreenCapture> {
    #[cfg(feature = "scrap-capture")]
    {
        match scrap_backend::ScrapCapture::primary(_target_fps, _downscale) {
            Ok(c) => return Box::new(c),
            Err(e) => {
                tracing::warn!(%e, "scrap capture unavailable — falling back to NoopCapture");
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

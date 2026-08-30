// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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

/// FR-29 P1 — the "did anything change?" answer XShm cannot give.
/// Linux-only: every other platform's backend already has an equivalent
/// (DXGI reports `WouldBlock`; WGC reports DirtyRegions).
#[cfg(all(target_os = "linux", feature = "scrap-capture"))]
pub mod x11_damage;

/// FR-36 P1 — DRM/KMS capture, below the compositor. Linux-only by
/// construction: this reads the kernel's scanout plane, which has no analogue
/// on Windows or macOS. The only backend that can see a Wayland desktop.
#[cfg(all(target_os = "linux", feature = "drm-capture"))]
pub mod drm_backend;

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod wgc_backend;

/// Phase 1 — Linux CI / agent-e2e Pod path. Substitutes a deterministic
/// 320×240 BGRA frame source for scrap-capture so a headless Pod
/// without an X server can still drive the encode + WebRTC pipeline.
/// `open_default` short-circuits to this backend when the runtime
/// env var `ROOMLERD_SYNTHETIC_FRAMES=1` is set AND the binary
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
    /// Per-frame damage truth (P8a). An enum rather than a bare rect
    /// list because the two empty states mean OPPOSITE things: no
    /// information (treat every pixel as potentially dirty — the
    /// pre-P8a contract) vs an authoritative "provably nothing
    /// changed". Per-FRAME rather than a capturer capability because
    /// the SystemContext pump swaps Dxgi↔Gdi backends mid-session
    /// without the media pump reopening the capturer.
    pub damage: Damage,
    /// HW-downscale Phase B — the NATIVE capture dims when this frame
    /// was scaled below them before delivery (GPU scale-before-readback,
    /// or the pump's CPU resample). `None` = `width`/`height` ARE the
    /// native dims. Consumers that need capture truth (cursor mapping,
    /// the dims plan, the CQ-bias area ratio) read [`Frame::native_dims`]
    /// — reading `width`/`height` directly on a scaled frame is the bug
    /// class this field exists to kill (a cursor that lands wrong exactly
    /// and only when GPU scale engages).
    pub source: Option<(u32, u32)>,
}

impl Frame {
    /// The native capture dims this frame represents — its own dims
    /// unless a downscale recorded the pre-scale size in `source`.
    pub fn native_dims(&self) -> (u32, u32) {
        self.source.unwrap_or((self.width, self.height))
    }
}

/// What the capture backend can say about WHICH pixels changed in this
/// frame relative to the previous delivered frame.
#[derive(Clone, Debug)]
pub enum Damage {
    /// No damage information — the backend can't say what changed
    /// (scrap on every OS, GDI BitBlt, WGC on builds where
    /// `DirtyRegions` is unavailable, or any per-frame metadata
    /// anomaly on an otherwise-tracking backend). Consumers must treat
    /// every pixel as potentially dirty.
    Unknown,
    /// Authoritative damage: `rects` is exactly what changed. An EMPTY
    /// list means "provably nothing changed" (e.g. a frozen still) —
    /// the inverse of `Unknown`, which is why this is not a plain Vec.
    Tracked(Vec<DirtyRect>),
}

impl Damage {
    /// The damage rects, empty for `Unknown` — callers that only need
    /// "which regions might I skip" semantics (ROI hints) can treat
    /// both empty cases identically; callers that need the tracked/
    /// unknown distinction must match instead.
    pub fn rects(&self) -> &[DirtyRect] {
        match self {
            Damage::Unknown => &[],
            Damage::Tracked(v) => v,
        }
    }

    /// Damaged area in permille of `frame_area` px. `None` when the
    /// backend reported no damage info. Non-empty damage floors at 1 ‰
    /// so "any tracked damage" thresholds can distinguish it from a
    /// provably-unchanged frame (0 ‰). Sum of rect areas, capped at the
    /// frame — overlap over-counts, which errs toward "significant"
    /// (the safe direction for motion detection); producers guarantee
    /// clipped rects.
    pub fn area_permille(&self, frame_area: u64) -> Option<u32> {
        match self {
            Damage::Unknown => None,
            Damage::Tracked(v) if v.is_empty() => Some(0),
            Damage::Tracked(v) => {
                if frame_area == 0 {
                    return Some(0);
                }
                let sum: u64 = v
                    .iter()
                    .map(|r| r.w as u64 * r.h as u64)
                    .fold(0u64, u64::saturating_add)
                    .min(frame_area);
                Some(((sum.saturating_mul(1000) / frame_area) as u32).clamp(1, 1000))
            }
        }
    }

    /// UNION of the damaged area in permille — the fraction of the frame a
    /// perfect per-rect readback would actually have to touch.
    ///
    /// ⚠️ This is NOT [`Self::area_permille`]. That one SUMS rect areas, so
    /// overlapping rectangles over-count and it saturates at 1000 ‰ on any
    /// busy screen — useful as "is there significant motion", useless for
    /// "how much would we have to read". FR-29 P3's entire premise is that
    /// the union is much smaller than the frame; this is the number that
    /// decides whether that premise holds, so it must not be conflated.
    ///
    /// Computed on a coarse grid rather than exactly: a partially covered
    /// cell counts as fully covered, so the answer is an over-estimate — the
    /// safe direction, matching every other producer here. That also bounds
    /// the cost at O(cells) regardless of how the rects overlap.
    pub fn union_permille(&self, w: u32, h: u32) -> Option<u32> {
        const GX: u32 = 128;
        const GY: u32 = 72;
        const WORDS: usize = ((GX * GY) as usize).div_ceil(64);
        match self {
            Damage::Unknown => None,
            Damage::Tracked(v) if v.is_empty() => Some(0),
            Damage::Tracked(v) => {
                if w == 0 || h == 0 {
                    return Some(0);
                }
                let mut grid = [0u64; WORDS];
                for r in v {
                    // Round the covered cell range OUTWARD, so a rect that
                    // clips a cell marks the whole cell dirty.
                    let cx0 = (r.x * GX / w).min(GX - 1);
                    let cy0 = (r.y * GY / h).min(GY - 1);
                    let cx1 = (r.x.saturating_add(r.w).saturating_mul(GX).div_ceil(w)).min(GX);
                    let cy1 = (r.y.saturating_add(r.h).saturating_mul(GY).div_ceil(h)).min(GY);
                    for cy in cy0..cy1.max(cy0 + 1) {
                        for cx in cx0..cx1.max(cx0 + 1) {
                            let bit = (cy * GX + cx) as usize;
                            grid[bit / 64] |= 1u64 << (bit % 64);
                        }
                    }
                }
                let set: u32 = grid.iter().map(|word| word.count_ones()).sum();
                Some(((u64::from(set) * 1000 / u64::from(GX * GY)) as u32).clamp(1, 1000))
            }
        }
    }

    /// Bounding-box area in permille — what the SIMPLE form of a partial
    /// readback (one `GetImage` over the enclosing rect) would cost. Read it
    /// beside [`Self::union_permille`]: if the union is small but the box is
    /// large, the damage is scattered and a bbox readback buys nothing, which
    /// is a design input for P3 rather than a detail.
    pub fn bbox_permille(&self, w: u32, h: u32) -> Option<u32> {
        match self {
            Damage::Unknown => None,
            Damage::Tracked(v) if v.is_empty() => Some(0),
            Damage::Tracked(v) => {
                let area = u64::from(w) * u64::from(h);
                if area == 0 {
                    return Some(0);
                }
                let x0 = v.iter().map(|r| r.x).min().unwrap_or(0);
                let y0 = v.iter().map(|r| r.y).min().unwrap_or(0);
                let x1 = v.iter().map(|r| r.x.saturating_add(r.w)).max().unwrap_or(0);
                let y1 = v.iter().map(|r| r.y.saturating_add(r.h)).max().unwrap_or(0);
                let bb = u64::from(x1.saturating_sub(x0)) * u64::from(y1.saturating_sub(y0));
                Some(((bb.min(area).saturating_mul(1000) / area) as u32).clamp(1, 1000))
            }
        }
    }
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
    /// Standard OS system-cursor CSS keyword (`"text"`, `"default"`,
    /// `"pointer"`, `"ew-resize"`, …) when the active cursor matches a
    /// known `IDC_*` handle, else `None` for app-custom cursors. Lets
    /// the browser render the viewer's real native cursor instead of
    /// the streamed bitmap (zero-latency, exactly the OS cursor).
    /// Cross-platform field; only the Windows tracker populates it today.
    pub css: Option<&'static str>,
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

/// Downsample 2× when the source has more pixels than this threshold.
/// Software openh264 at 4K SW encode caps out around 6–12 fps on a
/// typical desktop CPU; halving each dimension cuts pixel work by 4×
/// and typically brings us back to 25–30 fps, which matters far more
/// for perceived smoothness than the extra detail.
///
/// Measured in pixels (not width) so QHD 2560×1440 panels (3.7 Mpx)
/// trigger the downscale as well — an earlier width-only threshold
/// missed them because QHD width=2560 fell under the 2561 cutoff.
///
/// Lives here rather than in a backend because FR-36's DRM backend applies
/// the SAME policy: a host must not get a different picture size purely
/// because it switched capture backend.
///
/// ⚠️ Gated on its two consumers. This used to live INSIDE `scrap_backend`, so
/// the module's own feature gate covered it implicitly; moving it here to share
/// it with the DRM backend removed that cover and made it dead code in every
/// feature set that has neither backend. CI caught it on three lanes.
#[cfg(any(
    feature = "scrap-capture",
    all(target_os = "linux", feature = "drm-capture")
))]
pub(crate) const DOWNSCALE_TRIGGER_PIXELS: u64 = 3_500_000;

/// 2×2 box downsample over BGRA. Output dimensions are floor(w/2), floor(h/2).
/// Averages each 2×2 block per channel with a +2/4 round. Naive scalar
/// loop — at 4K (8.3 Mpx in, 2.1 Mpx out) this runs in ~15 ms in release
/// mode on a desktop CPU, well under the ~30 ms budget per frame at 30 fps
/// and comfortably less than openh264 would have spent encoding the full
/// 4K frame it replaces.
///
/// ⚠️ Gated on `scrap-capture` ALONE. It used to live inside `scrap_backend`,
/// where the module gate covered it implicitly; moving it here to share it cost
/// that cover and made it dead code in feature sets with neither backend (CI
/// caught it on three lanes). It then STOPPED being shared: FR-36 fused the
/// downscale into the DRM repack, so widening the gate back would recreate the
/// same dead-code failure in a DRM-only build — the same trap, in reverse.
#[cfg(feature = "scrap-capture")]
pub(crate) fn downscale_bgra_2x(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    src_stride: u32,
) -> (Vec<u8>, u32, u32) {
    let dw = src_w / 2;
    let dh = src_h / 2;
    let sw = src_stride as usize;
    let mut dst = vec![0u8; (dw * dh * 4) as usize];
    for y in 0..dh as usize {
        let row0 = 2 * y * sw;
        let row1 = (2 * y + 1) * sw;
        for x in 0..dw as usize {
            let sx = 2 * x * 4;
            let dx = (y * dw as usize + x) * 4;
            for c in 0..4 {
                let p00 = src[row0 + sx + c] as u32;
                let p10 = src[row0 + sx + 4 + c] as u32;
                let p01 = src[row1 + sx + c] as u32;
                let p11 = src[row1 + sx + 4 + c] as u32;
                dst[dx + c] = ((p00 + p10 + p01 + p11 + 2) / 4) as u8;
            }
        }
    }
    (dst, dw, dh)
}

#[async_trait::async_trait]
pub trait ScreenCapture: Send {
    async fn next_frame(&mut self) -> Result<Option<Frame>>;
    fn monitor_count(&self) -> u8;
    /// HW-downscale Phase B — the pump's CURRENT effective encode box
    /// (aspect-preserved, even dims), so a GPU-capable backend can scale
    /// BEFORE the CPU readback (shrinking the readback itself). `None` =
    /// deliver native. Advisory: backends without a GPU path (scrap,
    /// GDI, synthetic, Noop) inherit this no-op and keep delivering
    /// native frames — the pump's CPU resample remains the fallback and
    /// the truth (a backend-scaled frame simply passes through it).
    /// Applies from the NEXT frame (one-frame lag, same-in-kind as the
    /// dim-change encoder rebuild).
    fn set_output_cap(&mut self, _target: Option<(u32, u32)>) {}

    /// FR-29 — cumulative frames the backend declined to produce because it
    /// PROVED the screen was unchanged.
    ///
    /// ⚠️ Deliberately NOT folded into the pump's `frames_empty`. The two
    /// look identical at the call site (both are `Ok(None)`) and mean opposite
    /// things: `frames_empty` ≫ `frames_encoded` is the documented
    /// "pump is frame-production-bound" symptom — a host in trouble — whereas
    /// a high `frames_unchanged` is a host doing exactly the right thing on an
    /// idle desktop. Merging them would silently retire a working diagnostic.
    /// Backends that cannot tell (the pre-FR-29 default) report 0.
    fn frames_unchanged(&self) -> u64 {
        0
    }
}

/// P8a — re-project tracked damage through a downscale. Per-EDGE
/// floor/ceil (`x0=floor(x·r)`, `x1=ceil((x+w)·r)`) so coverage never
/// shrinks below the true footprint (floor-x/ceil-w under-covers when
/// the scaled origin lands mid-pixel). `Unknown` passes through.
///
/// Phase B moved this HERE from the pump-side resampler: the codebase
/// invariant is that damage rects share the FRAME's coordinate space
/// (`area_permille(w*h)`, ROI hints), so a backend that GPU-scales
/// before delivery must scale its damage too — a small frame carrying
/// native-coord rects would read every caret as full-screen motion.
pub(crate) fn scale_damage(
    damage: &Damage,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Damage {
    let Damage::Tracked(rects) = damage else {
        return Damage::Unknown;
    };
    if src_w == 0 || src_h == 0 {
        return Damage::Unknown;
    }
    let rx = dst_w as f64 / src_w as f64;
    let ry = dst_h as f64 / src_h as f64;
    let out = rects
        .iter()
        .filter_map(|r| {
            let x0 = ((r.x as f64 * rx).floor() as u32).min(dst_w);
            let y0 = ((r.y as f64 * ry).floor() as u32).min(dst_h);
            let x1 = (((r.x + r.w) as f64 * rx).ceil() as u32).min(dst_w);
            let y1 = (((r.y + r.h) as f64 * ry).ceil() as u32).min(dst_h);
            (x1 > x0 && y1 > y0).then_some(DirtyRect {
                x: x0,
                y: y0,
                w: x1 - x0,
                h: y1 - y0,
            })
        })
        .collect();
    Damage::Tracked(out)
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
/// Multi-user P3 — bounded backoff for the media pumps' capture-error
/// REOPEN path. Pre-P3 every pump retried at a fixed 500 ms; with N
/// concurrent sessions each running its own capturer, a PERSISTENT
/// denial (the DXGI Desktop-Duplication per-output app limit, a
/// session-0 desktop the worker can't reach, a wedged driver) had every
/// affected pump re-running the full open cascade twice a second,
/// forever. Grows 500 ms → 1 s → … → 10 s per CONSECUTIVE failure; a
/// quiet spell (> 30 s since the last error — i.e. capture recovered
/// and ran) resets to 500 ms so an isolated mode-change hiccup keeps
/// the old snappy recovery.
pub struct ReopenBackoff {
    cur: std::time::Duration,
    last_error: Option<std::time::Instant>,
}

impl ReopenBackoff {
    const FLOOR: std::time::Duration = std::time::Duration::from_millis(500);
    const CEIL: std::time::Duration = std::time::Duration::from_secs(10);
    const QUIET_RESET: std::time::Duration = std::time::Duration::from_secs(30);

    pub fn new() -> Self {
        Self {
            cur: Self::FLOOR,
            last_error: None,
        }
    }

    /// Record an error NOW and return how long to sleep before reopening.
    pub fn delay(&mut self) -> std::time::Duration {
        let now = std::time::Instant::now();
        let consecutive = self
            .last_error
            .is_some_and(|t| now.duration_since(t) <= Self::QUIET_RESET);
        self.cur = if consecutive {
            (self.cur * 2).min(Self::CEIL)
        } else {
            Self::FLOOR
        };
        self.last_error = Some(now);
        self.cur
    }
}

impl Default for ReopenBackoff {
    fn default() -> Self {
        Self::new()
    }
}

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
                "capture: backend=synthetic (ROOMLERD_SYNTHETIC_FRAMES=1, CI / agent-e2e Pod)"
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
    // or broken WinRT. Escape hatch: `ROOMLERD_CAPTURE=scrap` forces
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
            tracing::info!("ROOMLERD_CAPTURE=scrap — skipping WGC, using DXGI via scrap");
        }
    }
    // FR-36 — DRM/KMS: read the scanout plane below the compositor. This is
    // the ONLY backend that can see a Wayland desktop (XShm needs an X root
    // window) or a locked one (the portal refuses both).
    //
    // ⚠️ OPT-IN, not the Linux default, and the inverse of this repo's usual
    // kill-switch shape. DRM reports no damage at all, so defaulting it on
    // would silently undo FR-29 — which took a Linux host's idle capture from
    // 45.8 % of a core to 2.8 % precisely BY not grabbing an unchanged screen.
    // A Wayland or headless host sets `ROOMLERD_DRM_CAPTURE=1`; everyone else
    // keeps the existing cascade byte-for-byte.
    #[cfg(all(target_os = "linux", feature = "drm-capture"))]
    {
        if drm_backend::env_enabled() {
            match drm_backend::DrmCapture::primary(_target_fps, _downscale) {
                Ok(c) => {
                    tracing::info!(
                        width = c.width(),
                        height = c.height(),
                        "capture: backend=drm (ROOMLERD_DRM_CAPTURE=1, scanout below the compositor)"
                    );
                    return Box::new(c);
                }
                Err(e) => {
                    // Deliberately loud: the operator asked for DRM explicitly,
                    // so silently serving them X11 would look like the feature
                    // simply not working.
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "ROOMLERD_DRM_CAPTURE=1 but DRM capture could not open — falling through to the standard cascade"
                    );
                }
            }
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

/// Escape hatch: `ROOMLERD_CAPTURE=scrap` (case-insensitive) forces
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
/// True iff `ROOMLERD_SYNTHETIC_FRAMES` parses as truthy
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
        with_env("ROOMLERD_SYNTHETIC_FRAMES", None, || {
            assert!(!synthetic_env_enabled());
        });
    }

    #[test]
    fn truthy_values_accepted() {
        for v in &["1", "true", "TRUE", "yes", "On"] {
            with_env("ROOMLERD_SYNTHETIC_FRAMES", Some(v), || {
                assert!(synthetic_env_enabled(), "value {v:?} should be truthy");
            });
        }
    }

    #[test]
    fn explicit_zero_or_garbage_is_false() {
        for v in &["0", "false", "no", "off", "anything-else"] {
            with_env("ROOMLERD_SYNTHETIC_FRAMES", Some(v), || {
                assert!(!synthetic_env_enabled(), "value {v:?} should be falsy");
            });
        }
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    // P8a — damage re-projection through a downscale (moved here with
    // scale_damage in Phase B).
    #[test]
    fn scale_damage_per_edge_covers_and_unknown_passes_through() {
        // Per-edge floor/ceil: at ratio 0.1 a rect covering source px
        // 19..21 (x=19, w=2) maps to scaled px 1..3 — floor-x/ceil-w
        // (1 + ceil(0.2)=1) would cover px 1 only; per-edge covers 1..3.
        let d = Damage::Tracked(vec![DirtyRect {
            x: 19,
            y: 0,
            w: 2,
            h: 10,
        }]);
        let scaled = scale_damage(&d, 100, 100, 10, 10);
        let Damage::Tracked(v) = &scaled else {
            panic!("tracked must stay tracked");
        };
        assert_eq!(v.len(), 1);
        assert_eq!(
            (v[0].x, v[0].w),
            (1, 2),
            "per-edge scaling must not under-cover"
        );
        assert_eq!((v[0].y, v[0].h), (0, 1));
        // Unknown passes through as Unknown.
        assert!(matches!(
            scale_damage(&Damage::Unknown, 100, 100, 10, 10),
            Damage::Unknown
        ));
        // Tracked-empty stays tracked-empty (provably unchanged).
        let empty = scale_damage(&Damage::Tracked(vec![]), 100, 100, 10, 10);
        assert!(matches!(empty, Damage::Tracked(ref v) if v.is_empty()));
    }

    // ── P8a — Damage::area_permille ────────────────────────────────────

    /// The whole point of `union_permille` is that it does NOT do what
    /// `area_permille` does. Four identical overlapping rects saturate the
    /// summed measure to 1000 ‰ while the union is still just that one rect —
    /// if this ever stops holding, FR-29 P3's viability signal is silently
    /// reading the wrong thing again.
    #[test]
    fn union_permille_does_not_double_count_overlap() {
        let (w, h) = (1920u32, 1080u32);
        let half = DirtyRect {
            x: 0,
            y: 0,
            w: 1920,
            h: 540,
        };
        let four_copies = Damage::Tracked(vec![half, half, half, half]);
        assert_eq!(
            four_copies.area_permille(u64::from(w) * u64::from(h)),
            Some(1000),
            "summed measure saturates on overlap"
        );
        let u = four_copies.union_permille(w, h).unwrap();
        assert!(
            (480..=520).contains(&u),
            "union of four identical half-screen rects is ~500 permille, got {u}"
        );
    }

    /// Scattered damage: a small union but a bounding box covering the whole
    /// frame. This is the case where a bbox-based partial readback buys
    /// nothing, so the two numbers must be able to disagree loudly.
    #[test]
    fn bbox_and_union_diverge_on_scattered_damage() {
        let (w, h) = (1920u32, 1080u32);
        let d = Damage::Tracked(vec![
            DirtyRect {
                x: 0,
                y: 0,
                w: 16,
                h: 16,
            },
            DirtyRect {
                x: 1904,
                y: 1064,
                w: 16,
                h: 16,
            },
        ]);
        let u = d.union_permille(w, h).unwrap();
        let b = d.bbox_permille(w, h).unwrap();
        assert!(u <= 20, "two tiny corners are a tiny union, got {u}");
        assert!(
            b >= 900,
            "…but their bounding box is the whole frame, got {b}"
        );
    }

    /// `Unknown` must stay unmeasurable on both, and provably-unchanged must
    /// read 0 rather than being confused with it.
    #[test]
    fn union_and_bbox_preserve_the_two_empty_states() {
        assert_eq!(Damage::Unknown.union_permille(100, 100), None);
        assert_eq!(Damage::Unknown.bbox_permille(100, 100), None);
        assert_eq!(Damage::Tracked(vec![]).union_permille(100, 100), Some(0));
        assert_eq!(Damage::Tracked(vec![]).bbox_permille(100, 100), Some(0));
    }

    #[test]
    fn damage_area_permille_semantics() {
        const AREA: u64 = 1920 * 1200;
        // Unknown: no information, not zero.
        assert_eq!(Damage::Unknown.area_permille(AREA), None);
        // Tracked-empty: provably nothing changed.
        assert_eq!(Damage::Tracked(vec![]).area_permille(AREA), Some(0));
        // A caret-sized rect (16×32 = 0.02 ‰) floors at 1 ‰ so "any
        // tracked damage" thresholds can tell it from provably-unchanged.
        let caret = Damage::Tracked(vec![DirtyRect {
            x: 100,
            y: 100,
            w: 16,
            h: 32,
        }]);
        assert_eq!(caret.area_permille(AREA), Some(1));
        // A quarter-screen scroll ≈ 250 ‰.
        let quarter = Damage::Tracked(vec![DirtyRect {
            x: 0,
            y: 0,
            w: 960,
            h: 600,
        }]);
        assert_eq!(quarter.area_permille(AREA), Some(250));
        // Overlapping rects over-count but cap at the frame (1000 ‰).
        let full = DirtyRect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1200,
        };
        assert_eq!(
            Damage::Tracked(vec![full, full]).area_permille(AREA),
            Some(1000)
        );
        // Degenerate frame area never divides by zero.
        assert_eq!(Damage::Tracked(vec![full]).area_permille(0), Some(0));
    }

    // Phase B — native-dims truth on scaled frames.
    #[test]
    fn native_dims_prefers_source() {
        let mut f = Frame {
            width: 1024,
            height: 640,
            stride: 4096,
            pixel_format: PixelFormat::Bgra,
            data: vec![],
            monotonic_us: 0,
            monitor: 0,
            damage: Damage::Unknown,
            source: None,
        };
        assert_eq!(f.native_dims(), (1024, 640), "no source = own dims");
        f.source = Some((1920, 1200));
        assert_eq!(f.native_dims(), (1920, 1200));
    }
}

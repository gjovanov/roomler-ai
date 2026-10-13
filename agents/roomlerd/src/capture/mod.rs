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

/// FR-45 P1 — desktop-portal availability. Detection only: whether ScreenCast
/// exists at all, and why not. The capture path itself is P2/P3.
#[cfg(all(target_os = "linux", feature = "portal-capture"))]
pub mod portal;

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

/// FR-85 P1d — the mouse pointer in a recording: whether each backend draws
/// it into the frame, and where it is when the recorder has to draw it.
pub mod pointer;

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

    /// FR-80 — why this backend will never produce a frame, when it knows.
    /// `None` from every real backend: a capturer that yields frames has no
    /// reason to give, and the pumps read this exactly once at open to tell
    /// the controller what the operator would otherwise have to find in the
    /// host's log.
    fn unavailable(&self) -> Option<&CaptureUnavailable> {
        None
    }

    /// FR-85 P1d — how the mouse pointer gets into a RECORDING made from this
    /// capturer. Asked once, by the recorder, after the first frame. The live
    /// pumps never ask: they stream the pointer on its own channel.
    ///
    /// The default is honest for a backend that cannot tell: no pointer, and
    /// the recording's sidecar says so rather than leaving someone to wonder.
    fn recorded_pointer(&mut self) -> pointer::RecordedPointer {
        pointer::RecordedPointer::Absent
    }

    /// FR-93 P2 — which Linux arm is serving this session, for the controller
    /// (`rc:video-info` `capture_arm`, and `attended`). `None` wherever there
    /// is no choice worth naming: Windows, macOS, a capturer with no pixels.
    fn arm(&self) -> Option<CaptureArm> {
        None
    }
}

/// FR-93 P2 — the backends a Linux session can be served by. They differ in
/// what the controller should know: DRM and mutter capture without asking
/// anyone, the portal asked the person at the screen, and X11 is the
/// pre-Wayland path.
///
/// ⚠️ The wire spellings are a compatibility surface, like every `rc:*`
/// string: renaming one does not fail loudly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureArm {
    Drm,
    Mutter,
    Portal,
    X11,
}

impl CaptureArm {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Drm => "drm",
            Self::Mutter => "mutter",
            Self::Portal => "portal",
            Self::X11 => "x11",
        }
    }

    /// Did a person at the screen have to allow this capture? Only the portal
    /// asks.
    pub fn attended(self) -> bool {
        matches!(self, Self::Portal)
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

/// FR-80 — why a capture backend produced nothing, when the answer is
/// knowable. A closed set: the controller keys its message off this, so it is
/// a compatibility surface like every other `rc:*` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureUnavailableCode {
    /// The OS can capture and this process is not permitted to: a macOS
    /// Screen Recording (TCC) grant that does not match, a denied portal
    /// request. The operator can fix it, which is why it is worth naming.
    Permission,
    /// Nothing to capture — a headless host, no `DISPLAY`/`WAYLAND_DISPLAY`.
    NoDisplay,
    /// This build carries no capture backend at all (signalling-only agent).
    NotBuilt,
    /// The backend failed and we did not attribute it. `detail` carries its
    /// own words; "we do not know" is a better answer than a guess.
    BackendError,
    /// FR-93 — a Wayland desktop is right there, and every arm that could
    /// capture it is switched off on this device. Before this code the root
    /// daemon called such a host "headless" (`no_display`) and the per-user
    /// daemon streamed Xwayland's empty root: a live black picture with no
    /// reason at all (the FR-93 lab, 0.4.125).
    WaylandNotEnabled,
}

impl CaptureUnavailableCode {
    /// The wire spelling. ⚠️ Renaming one does not fail loudly — it changes
    /// what an operator reads on a screen that exists to explain a failure.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::NoDisplay => "no_display",
            Self::NotBuilt => "not_built",
            Self::BackendError => "backend_error",
            Self::WaylandNotEnabled => "wayland_not_enabled",
        }
    }

    /// The operator-facing sentence. Built HERE and sent on the wire, not
    /// composed in the viewer, because only the agent knows which OS it is
    /// on — the same reason the consent surface names itself (FR-27).
    pub fn hint(self) -> &'static str {
        match self {
            Self::Permission => {
                if cfg!(target_os = "macos") {
                    "Grant Screen Recording to Roomler in System Settings → Privacy & Security, \
                     then reconnect. A macOS update or a change to the app's code signature \
                     invalidates an existing grant."
                } else {
                    "The host denied this agent permission to capture the screen. Grant it on \
                     the host, then reconnect."
                }
            }
            Self::NoDisplay => {
                if cfg!(target_os = "macos") {
                    "This agent runs as a system daemon, which has no desktop session and \
                     therefore no screen to capture. Connect to this machine's user-session \
                     agent instead. Remote shell and file transfer work here."
                } else {
                    "This host has no display to capture (headless, or no desktop session). \
                     Remote shell and file transfer still work."
                }
            }
            Self::NotBuilt => {
                "This agent was built without a capture backend — it can signal, but never \
                 send pixels. Install a full agent build."
            }
            Self::BackendError => {
                "The host's capture backend failed to start. Its own error is in the detail; \
                 the agent log has the full line."
            }
            Self::WaylandNotEnabled => wayland_hint(running_as_root()),
        }
    }
}

/// The `wayland_not_enabled` sentence, by install type: only a root daemon can
/// read the scanout (DRM) or create a uinput device. A per-user daemon in its
/// own Wayland session captures by itself (mutter on GNOME, else the portal,
/// FR-93 P2), so it reads this only when the operator switched that off: the
/// kill switch, or a capture key set.
///
/// ⚠️ A hint goes on the wire as a JSON literal, unescaped
/// (`capture_unavailable_payload`): no double quote, no backslash.
fn wayland_hint(root: bool) -> &'static str {
    if root {
        "This computer is showing a Wayland desktop, and capturing it is switched off on this \
         device. On the host an administrator can run sudo roomler config set drm_capture true \
         and sudo roomler config set uinput true, then sudo roomler restart. That capture reads \
         the screen below the desktop, the lock screen included. Remote shell and file transfer \
         still work."
    } else {
        "This computer is showing a Wayland desktop, and capturing it is switched off on this \
         device. On the host, run roomler config clear wayland_auto, and roomler config clear for \
         each capture key that roomler config ls shows as set (portal_capture, mutter_capture, \
         window_capture), then roomler restart. GNOME is then captured through its own \
         screencast, which asks nobody, and other desktops ask the person at the screen once. \
         Remote shell and file transfer still work."
    }
}

/// Root or not decides which keys a hint can honestly name: only a root daemon
/// can read the scanout (DRM) or create a uinput device.
fn running_as_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// FR-93 P1 kill switch (`ROOMLERD_WAYLAND_AWARE` / `wayland_aware`), default
/// ON. It enables no capture; it only stops a Wayland desktop with nothing
/// switched on from reading as "headless" or streaming black. `false` gives
/// back 0.4.125's behaviour byte for byte.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn wayland_aware() -> bool {
    tunnel_core::env::flag("WAYLAND_AWARE", true)
}

/// FR-93 P2 kill switch (`ROOMLERD_WAYLAND_AUTO` / `wayland_auto`), default
/// ON. `false` gives back 0.4.128's cascade: a per-user daemon in a Wayland
/// session captures only what a key switches on.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn wayland_auto() -> bool {
    tunnel_core::env::flag("WAYLAND_AUTO", true)
}

/// FR-93 P2 — the keys that choose Linux capture by hand. With any one of them
/// set, to anything, the operator owns the cascade and it runs exactly as it
/// did before P2: `drm_capture = false` removes that arm, `= true` forces it.
/// `window_capture` counts too. It means something only to the portal, so an
/// operator who set it has a portal in mind.
#[cfg(target_os = "linux")]
const CAPTURE_KEYS: [&str; 4] = [
    "DRM_CAPTURE",
    "PORTAL_CAPTURE",
    "MUTTER_CAPTURE",
    "WINDOW_CAPTURE",
];

#[cfg(target_os = "linux")]
fn capture_keys_set() -> bool {
    CAPTURE_KEYS
        .iter()
        .any(|k| tunnel_core::env::node_env(k).is_some())
}

/// FR-93 P2 — where this daemon's own user is, as far as the automatic arm
/// cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum OwnSession {
    /// Signed in to an active Wayland session.
    Wayland,
    /// Anything else: X11, only a tty, nobody signed in, no logind to ask.
    Other,
}

/// FR-93 P2 (decision D1) — what the cascade may switch on by itself.
///
/// `Auto` tries mutter first (P2a: GNOME's own screencast, which asks nobody),
/// and where mutter is absent the desktop portal (P2d-3: KDE and the rest),
/// which asks the person at the screen once and then restores their answer.
///
/// ⚠️ The portal waited for P2d. Run automatically before it (the FR-93 lab,
/// KDE, 2026-10-11), it stacked a new "Share screen with" dialog every 10–20 s
/// while the viewer's watchdog re-created its session, a late answer started a
/// capture for a session already gone, and each pending open parked a runtime
/// worker for up to `HANDSHAKE_DEADLINE`, until the daemon's watchdog killed
/// it. P2d-1 made it one request per host on an open that parks no worker, and
/// P2d-2 tells the viewer to wait for the person instead of re-creating the
/// session (`linux-capture.md` §4.6, §4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(all(target_os = "linux", feature = "portal-capture")),
    allow(dead_code)
)]
enum WaylandPlan {
    /// The cascade as it was before P2.
    AsConfigured,
    /// A per-user daemon in its own Wayland session, with no capture key set:
    /// try mutter, else the portal, whether or not the screen is locked. What
    /// a locked session shows is the compositor's to decide.
    Auto,
}

/// D1 as a pure function. Cheap checks first: `session` runs loginctl, so it
/// is asked only when everything else allows auto.
///
/// - `root`: D1's system half stays opt-in until a desktop companion ships for
///   non-Debian hosts, so a root daemon never switches an arm on by itself.
/// - `keys_set`: an explicit capture key always wins.
/// - `virtual_desktop`: the daemon's own Xvfb is what it captures.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn wayland_plan(
    aware: bool,
    auto: bool,
    root: bool,
    keys_set: bool,
    virtual_desktop: bool,
    session: impl FnOnce() -> OwnSession,
) -> WaylandPlan {
    if !aware || !auto || root || keys_set || virtual_desktop {
        return WaylandPlan::AsConfigured;
    }
    match session() {
        OwnSession::Wayland => WaylandPlan::Auto,
        OwnSession::Other => WaylandPlan::AsConfigured,
    }
}

/// This daemon's user's active Wayland session, from logind. A user can have
/// several sessions (an SSH login beside the desktop); only an active
/// graphical one of class `user` counts, never a greeter.
#[cfg(target_os = "linux")]
fn own_session() -> OwnSession {
    // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
    let uid = unsafe { libc::geteuid() };
    crate::keep_busy::logind::sessions()
        .map(|all| own_session_in(&all, uid))
        .unwrap_or(OwnSession::Other)
}

/// [`own_session`] over sessions already read, so a caller that has them
/// (the desktop brief) does not run loginctl twice.
#[cfg(target_os = "linux")]
fn own_session_in(all: &[crate::keep_busy::logind::Session], uid: u32) -> OwnSession {
    if all
        .iter()
        .any(|s| s.uid == uid && s.is_user() && s.active && s.kind == "wayland")
    {
        OwnSession::Wayland
    } else {
        OwnSession::Other
    }
}

/// FR-93 P2b — the facts the cascade's first choice is made from.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ArmFacts {
    /// [`wayland_plan`] said `Auto`, and mutter answered on this session's bus.
    pub auto_mutter: bool,
    /// P2d-3 — [`wayland_plan`] said `Auto`, mutter did not answer, and a
    /// desktop portal is on this session's bus, running or installed to start
    /// on demand.
    pub auto_portal: bool,
    /// `drm_capture`, in a build with the DRM backend.
    pub drm: bool,
    /// `portal_capture`, in a build with the portal backend.
    pub portal: bool,
    /// `mutter_capture`: the portal arm runs on mutter's API instead.
    pub portal_via_mutter: bool,
    /// `portal_input`: a portal (or configured mutter) arm carries input.
    pub portal_input: bool,
    /// FR-93 P4b — the automatic portal asks for remote control
    /// ([`auto_portal_input`]: on unless `portal_input = false`).
    pub auto_portal_input: bool,
    /// FR-93 P4 — `mutter_input` (default on): a mutter arm's input rides
    /// mutter's RemoteDesktop.
    pub mutter_input: bool,
    /// The daemon captures its own Xvfb.
    pub virtual_desktop: bool,
    /// The session a controller would see is an X11 one.
    pub x11_session: bool,
    /// A build with the X11 backend.
    pub x11_built: bool,
}

/// FR-93 P2b — the arm a session opened now would try first, in
/// [`open_with`]'s own order (keep the two together): the automatic arm
/// (mutter, else the portal, P2d-3), then `drm_capture`, then `portal_capture`
/// (on mutter's API with `mutter_capture`), then X11, which a Wayland desktop
/// refuses. So nothing switched on there means nothing captures (`None`). A
/// plan, not a promise: an arm that fails at session time falls through, and
/// that session's `rc:video-info` names what served it.
#[cfg(target_os = "linux")]
pub(crate) fn planned_arm(f: &ArmFacts) -> Option<CaptureArm> {
    if f.auto_mutter {
        return Some(CaptureArm::Mutter);
    }
    if f.auto_portal {
        return Some(CaptureArm::Portal);
    }
    if f.drm {
        return Some(CaptureArm::Drm);
    }
    if f.portal {
        return Some(if f.portal_via_mutter {
            CaptureArm::Mutter
        } else {
            CaptureArm::Portal
        });
    }
    ((f.virtual_desktop || f.x11_session) && f.x11_built).then_some(CaptureArm::X11)
}

/// FR-93 P4 — the RemoteDesktop route a session opened now would register, in
/// the arm's own terms: the automatic mutter arm carries mutter's input unless
/// `mutter_input` is off; a configured mutter arm carries it with
/// `portal_input` too; the automatic portal carries the portal's unless
/// `portal_input = false` (P4b); a configured portal arm carries it with
/// `portal_input`. `None`: the OS injector decides, as before.
#[cfg(target_os = "linux")]
pub(crate) fn planned_input_route(f: &ArmFacts) -> Option<crate::input::reach::RouteKind> {
    use crate::input::reach::RouteKind;
    match planned_arm(f)? {
        CaptureArm::Mutter if f.auto_mutter => f.mutter_input.then_some(RouteKind::Mutter),
        CaptureArm::Mutter => (f.portal_input && f.mutter_input).then_some(RouteKind::Mutter),
        CaptureArm::Portal if f.auto_portal => f.auto_portal_input.then_some(RouteKind::Portal),
        CaptureArm::Portal => f.portal_input.then_some(RouteKind::Portal),
        CaptureArm::Drm | CaptureArm::X11 => None,
    }
}

/// FR-93 P2b — gather [`ArmFacts`] the way [`open_with`] reads them. `sessions`
/// is logind's list (read once by the caller); mutter, then the portal, are
/// asked only when the plan is `Auto`, as the cascade asks them. The portal is
/// asked without being started ([`portal::reachable`]): this runs every minute.
#[cfg(target_os = "linux")]
pub(crate) fn arm_facts(
    sessions: Option<&[crate::keep_busy::logind::Session]>,
    root: bool,
    uid: u32,
    x11_session: bool,
) -> ArmFacts {
    let virtual_desktop = crate::virtual_desktop::requested();
    #[allow(unused_variables)] // read only with portal-capture
    let plan = wayland_plan(
        wayland_aware(),
        wayland_auto(),
        root,
        capture_keys_set(),
        virtual_desktop,
        || sessions.map_or(OwnSession::Other, |all| own_session_in(all, uid)),
    );
    #[cfg(feature = "portal-capture")]
    let (auto_mutter, auto_portal) = if plan == WaylandPlan::Auto {
        let mutter = portal::mutter::available().is_ok();
        (mutter, !mutter && portal::reachable().is_ok())
    } else {
        (false, false)
    };
    #[cfg(not(feature = "portal-capture"))]
    let (auto_mutter, auto_portal) = (false, false);
    #[cfg(feature = "portal-capture")]
    let mutter_input = portal::mutter::input_enabled();
    #[cfg(not(feature = "portal-capture"))]
    let mutter_input = false;
    #[cfg(feature = "drm-capture")]
    let drm = drm_backend::env_enabled();
    #[cfg(not(feature = "drm-capture"))]
    let drm = false;
    ArmFacts {
        auto_mutter,
        auto_portal,
        drm,
        portal: cfg!(feature = "portal-capture") && tunnel_core::env::flag("PORTAL_CAPTURE", false),
        portal_via_mutter: tunnel_core::env::flag("MUTTER_CAPTURE", false),
        portal_input: tunnel_core::env::flag("PORTAL_INPUT", false),
        auto_portal_input: auto_portal_input(),
        mutter_input,
        virtual_desktop,
        x11_session,
        x11_built: cfg!(feature = "scrap-capture"),
    }
}

/// FR-93 P2 — what the automatic mutter arm came to.
#[cfg(all(target_os = "linux", feature = "portal-capture"))]
enum AutoMutter {
    Opened(Box<dyn ScreenCapture>),
    /// mutter does not answer on this session's bus: not GNOME. Nothing was
    /// switched on, so the automatic portal is tried next (P2d-3).
    NotGnome(String),
    /// mutter answered, and its screencast still failed. An arm that switched
    /// itself on failed, so the controller reads `backend_error` with this
    /// reason, never "switch it on" (P1c's rule for an arm the operator
    /// switched on).
    Failed(String),
}

#[cfg(all(target_os = "linux", feature = "portal-capture"))]
fn open_auto_mutter(target_fps: u32) -> AutoMutter {
    use portal::backend::{PortalCapture, PortalOpen};
    if let Err(why) = portal::mutter::available() {
        return AutoMutter::NotGnome(why);
    }
    // FR-93 P4 — mutter's own RemoteDesktop drives the screencast and takes the
    // viewer's input. It asks nobody, like the screencast it drives, so it
    // follows the arm: on unless `mutter_input = false`.
    let opts = PortalOpen {
        mutter: true,
        input: portal::mutter::input_enabled(),
        window: false,
    };
    match PortalCapture::open_with(target_fps, opts) {
        Ok(c) => {
            tracing::warn!(
                width = c.width(),
                height = c.height(),
                "capture: backend=portal via org.gnome.Mutter.ScreenCast, AUTOMATIC (FR-93 D1: a \
                 per-user Wayland session with no capture key set). It asks nobody. \
                 ROOMLERD_WAYLAND_AUTO=0, or any capture key, switches it off"
            );
            AutoMutter::Opened(Box::new(c))
        }
        Err(e) => AutoMutter::Failed(format!(
            "mutter capture switched itself on (a per-user GNOME session with no capture key \
             set), and its screencast could not open: {e:#}"
        )),
    }
}

/// FR-93 P2d-3 — what the automatic portal arm came to. Either way the arm
/// switched itself on, so a failure reads `backend_error` with its reason
/// (P1c's rule), never "switch it on".
#[cfg(all(target_os = "linux", feature = "portal-capture"))]
enum AutoPortal {
    Opened(Box<dyn ScreenCapture>),
    /// No portal on this desktop can capture it, or the person at the screen
    /// did not share it (refused, or nobody answered within the request's
    /// deadline). The reason, short enough for the viewer's detail.
    Failed(String),
}

/// FR-93 P4b (operator, 2026-10-13) — does the automatic portal ask for remote
/// control (a RemoteDesktop session: see AND control) rather than a screen
/// share? Yes unless `portal_input = false`. KDE's dialog names both
/// privileges plainly ("See what's on the screen · Control input devices"), and
/// its answer is restored like the screen share's. A configured portal arm
/// keeps `portal_input` opt-in: there the operator chose the arm.
///
/// A person who refuses remote control is asked once more, for the screen
/// only: input never costs capture (`portal::helper`'s rule).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn auto_portal_input() -> bool {
    tunnel_core::env::flag("PORTAL_INPUT", true)
}

/// How long the automatic portal arm waits for the portal to say whether it
/// serves ScreenCast, before any dialog.
#[cfg(all(target_os = "linux", feature = "portal-capture"))]
const AUTO_PORTAL_DETECT: std::time::Duration = std::time::Duration::from_secs(5);

/// FR-93 P2d-3 — the portal where mutter is absent: KDE, and every other
/// desktop with an xdg-desktop-portal ScreenCast backend. It is ATTENDED. The
/// first open shows the person at the screen a dialog and waits for them, as
/// one request per host through the broker (P2d-1), while the viewer is told
/// it is waiting (P2d-2). Their answer is kept as a restore token, so later
/// sessions open with no dialog.
#[cfg(all(target_os = "linux", feature = "portal-capture"))]
fn open_auto_portal(target_fps: u32) -> AutoPortal {
    use portal::backend::PortalOpen;
    if let Some(why) = auto_portal_unusable(&portal::detect_within(AUTO_PORTAL_DETECT)) {
        return AutoPortal::Failed(why);
    }
    // FR-93 P4b (operator, 2026-10-13): the one dialog asks for remote control,
    // seeing and controlling, so the person at the screen decides once.
    let opts = PortalOpen {
        mutter: false,
        input: auto_portal_input(),
        window: false,
    };
    match portal::pending::open_with(target_fps, opts) {
        Ok(c) => {
            tracing::warn!(
                width = c.width(),
                height = c.height(),
                "capture: backend=portal, AUTOMATIC (FR-93 P2d-3: a per-user Wayland session with \
                 no capture key set, and no mutter). ATTENDED: the person at the screen shared it, \
                 now or earlier (the restore token). ROOMLERD_WAYLAND_AUTO=0, or any capture key, \
                 switches it off"
            );
            AutoPortal::Opened(Box::new(c))
        }
        Err(e) => AutoPortal::Failed(format!(
            "the desktop portal did not share the screen: {e:#}"
        )),
    }
}

/// Why the automatic portal arm raises no dialog on this desktop, or `None`
/// when it may. Capture needs ScreenCast only; RemoteDesktop is input's. The
/// status comes first, so the viewer's capped detail still names it.
#[cfg(all(target_os = "linux", feature = "portal-capture"))]
fn auto_portal_unusable(status: &portal::PortalStatus) -> Option<String> {
    (!status.usable_for_capture()).then(|| {
        format!(
            "no desktop portal can capture this desktop ({status}): {}",
            status.advice()
        )
    })
}

/// FR-93 — what to call "every arm failed" on Linux, as a pure function.
///
/// - `display` / `wayland_display`: the DAEMON's own variables. A root systemd
///   daemon never has them, which is why they cannot answer "is there a
///   desktop" on their own — the seat is asked (`seat_is_wayland`, lazily: it
///   runs loginctl).
/// - `arm_failed`: an operator switched a Wayland arm on (DRM or the portal)
///   and it could not open. Then the true answer is that arm's own error, not
///   "switched off".
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_no_pixels_code(
    display: bool,
    wayland_display: bool,
    aware: bool,
    arm_failed: bool,
    seat_is_wayland: impl FnOnce() -> bool,
) -> CaptureUnavailableCode {
    use CaptureUnavailableCode::*;
    if !aware {
        // 0.4.125's rule, unchanged.
        return if !display && !wayland_display {
            NoDisplay
        } else {
            BackendError
        };
    }
    if arm_failed || display {
        // An arm the operator asked for, or an X display scrap could not
        // open: a real error, in its own words.
        return BackendError;
    }
    if wayland_display || seat_is_wayland() {
        WaylandNotEnabled
    } else {
        NoDisplay
    }
}

/// FR-93 — the `detail` of a `wayland_not_enabled` the classifier reached:
/// what the agent FOUND, ahead of X11's own words. The viewer prints the
/// detail under the reason, and on its own X11's "no primary display:
/// connection refused" reads like a network fault, on a host whose only
/// problem is a switched-off key (the FR-93 lab, system guest).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn wayland_not_enabled_detail(wayland_display: bool, x11_error: &str) -> String {
    let found = if wayland_display {
        "this daemon runs inside a Wayland session (WAYLAND_DISPLAY is set)"
    } else {
        "logind shows a Wayland session on this host, and this daemon has no display of its own"
    };
    format!("{found}; X11 capture said: {x11_error}")
}

/// FR-93 — scrap OPENED, on an X server that is Xwayland. On a Wayland desktop
/// that root window is empty, so the "success" is a live black stream.
#[cfg_attr(
    not(all(target_os = "linux", feature = "scrap-capture")),
    allow(dead_code)
)]
fn xwayland_verdict(
    is_xwayland: bool,
    aware: bool,
    arm_failed: bool,
) -> Option<CaptureUnavailableCode> {
    if !aware || !is_xwayland {
        return None;
    }
    Some(if arm_failed {
        CaptureUnavailableCode::BackendError
    } else {
        CaptureUnavailableCode::WaylandNotEnabled
    })
}

/// Is a graphical session on this host Wayland? logind knows; the daemon's
/// environment does not (a root unit has none of the session's variables).
#[cfg(target_os = "linux")]
fn seat_shows_wayland() -> bool {
    crate::companion::graphical_session()
        .map(|s| s.wayland_display.is_some())
        .unwrap_or(false)
}

/// Is the X server on `display` Xwayland? Asked of the server itself rather
/// than inferred from the session type, so the daemon's own Xvfb (the virtual
/// desktop) and a real Xorg are never mistaken for it. An unreachable server
/// is "no": scrap then reports its own error.
///
/// Two signals, the same two keep busy reads (`keep_busy::host_x11`, FR-92):
/// the `XWAYLAND` extension, which Xwayland has carried since 23.1, and before
/// that its RandR monitors, all named `XWAYLAND<n>` — so a 22.04-era GNOME is
/// recognised too, not streamed black.
#[cfg(all(target_os = "linux", feature = "scrap-capture"))]
pub(crate) fn x_server_is_xwayland(display: &str) -> bool {
    x_server_xwayland(display) == Some(true)
}

/// [`x_server_is_xwayland`] with the third answer it folds into "no": `None`
/// when no Unix socket reaches an X server at all. `input::reach` needs it,
/// because "no X display" and "a real X server" reach very different things.
#[cfg(all(
    target_os = "linux",
    any(feature = "scrap-capture", feature = "enigo-input")
))]
pub(crate) fn x_server_xwayland(display: &str) -> Option<bool> {
    x_server_query(display, |conn, root| {
        x_has_extension(conn, "XWAYLAND") || x_has_xwayland_monitor(conn, root)
    })
}

#[cfg(all(
    target_os = "linux",
    any(feature = "scrap-capture", feature = "enigo-input")
))]
fn x_has_extension(conn: &x11rb::rust_connection::RustConnection, name: &'static str) -> bool {
    use x11rb::connection::RequestConnection as _;
    matches!(conn.extension_information(name), Ok(Some(_)))
}

#[cfg(all(
    target_os = "linux",
    any(feature = "scrap-capture", feature = "enigo-input")
))]
fn x_has_xwayland_monitor(
    conn: &x11rb::rust_connection::RustConnection,
    root: x11rb::protocol::xproto::Window,
) -> bool {
    use x11rb::protocol::randr::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::ConnectionExt as _;
    if !x_has_extension(conn, randr::X11_EXTENSION_NAME) {
        return false;
    }
    let monitors_known = conn
        .randr_query_version(1, 5)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|v| (v.major_version, v.minor_version) >= (1, 5));
    if !monitors_known {
        return false;
    }
    let Some(reply) = conn
        .randr_get_monitors(root, true)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return false;
    };
    reply.monitors.iter().any(|m| {
        conn.get_atom_name(m.name)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.name.starts_with(b"XWAYLAND"))
    })
}

/// Ask the X server on `display` one question. `None` when no Unix socket
/// reaches it, or it refuses us.
///
/// ⚠️ Unix sockets ONLY — Xwayland is a local server. `x11rb::connect` falls
/// back to TCP `localhost:6000+N` when the socket is missing, and where a
/// localhost connect to a closed port hangs instead of being refused (WSL2's
/// mirrored networking, with systemd's /tmp hiding WSLg's X0 socket) that
/// fallback stalled this probe for over a minute, measured, in front of every
/// capture open. A display reached over TCP is simply not probed.
#[cfg(all(
    target_os = "linux",
    any(feature = "scrap-capture", feature = "enigo-input")
))]
fn x_server_query(
    display: &str,
    ask: impl FnOnce(&x11rb::rust_connection::RustConnection, x11rb::protocol::xproto::Window) -> bool,
) -> Option<bool> {
    use x11rb::connection::Connection as _;
    use x11rb::reexports::x11rb_protocol::parse_display::{ConnectAddress, parse_display};
    use x11rb::reexports::x11rb_protocol::xauth::get_auth;
    use x11rb::rust_connection::{DefaultStream, RustConnection};

    let parsed = parse_display(Some(display)).ok()?;
    for addr in parsed.connect_instruction() {
        if !matches!(addr, ConnectAddress::Socket(_)) {
            continue;
        }
        let Ok((stream, (family, address))) = DefaultStream::connect(&addr) else {
            continue;
        };
        // The same auth lookup x11rb's own connect does; Xwayland under GNOME
        // and KDE requires the session's cookie.
        let (auth_name, auth_data) = get_auth(family, &address, parsed.display)
            .ok()
            .flatten()
            .unwrap_or_default();
        let conn = RustConnection::connect_to_stream_with_auth_info(
            stream,
            parsed.screen.into(),
            auth_name,
            auth_data,
        )
        .ok()?;
        let root = conn.setup().roots.get(usize::from(parsed.screen))?.root;
        return Some(ask(&conn, root));
    }
    None
}

/// FR-80 — the reason a session will produce no pixels, carried by the
/// fallback capturer so every pump can ask instead of each one re-deriving it.
#[derive(Debug, Clone)]
pub struct CaptureUnavailable {
    pub code: CaptureUnavailableCode,
    /// The backend's own words, capped. A quote, never a paraphrase.
    pub detail: String,
}

impl CaptureUnavailable {
    /// The cap keeps one backend's verbose error from dominating a control
    /// message the viewer renders inline. Truncates on a char boundary.
    const DETAIL_MAX: usize = 200;

    pub fn new(code: CaptureUnavailableCode, detail: impl Into<String>) -> Self {
        let detail: String = detail.into();
        let detail = if detail.chars().count() > Self::DETAIL_MAX {
            detail.chars().take(Self::DETAIL_MAX).collect()
        } else {
            detail
        };
        Self { code, detail }
    }
}

/// A capture backend that never produces frames. Used when no display is
/// available (headless host, CI with no $DISPLAY) so higher layers can keep
/// ticking without panicking.
///
/// FR-80 — it carries WHY. Before that this was a unit struct and
/// `open_default` logged the backend's error and dropped it, so a host that
/// could not capture told the controller nothing and the viewer could only
/// show a stall (2026-09-08: a macOS Screen Recording grant lost to a
/// signing-identity change read as "the codec picker is broken").
pub struct NoopCapture {
    reason: Option<CaptureUnavailable>,
}

impl NoopCapture {
    /// A frameless capturer with no reason to give — the shape the older
    /// unit struct had. Kept for callers that mean "nothing here" rather
    /// than "here is what went wrong".
    pub fn new() -> Self {
        Self { reason: None }
    }

    /// A frameless capturer that knows why it is frameless.
    pub fn unavailable(reason: CaptureUnavailable) -> Self {
        Self {
            reason: Some(reason),
        }
    }
}

impl Default for NoopCapture {
    fn default() -> Self {
        Self::new()
    }
}

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
    fn unavailable(&self) -> Option<&CaptureUnavailable> {
        self.reason.as_ref()
    }
}

/// FR-80 — attribute a capture-open failure, or admit we cannot.
///
/// The macOS arm asks the OS through the existing TCC preflight
/// (`tcc::screen_recording_granted`, the same probe `scrap_backend` uses to
/// warn) rather than pattern-matching the backend's error: scrap reports a
/// bare `other error` for a denied grant and for a broken display alike, so a
/// guess would replace one unattributable failure with another. Preflight
/// only — never the prompting variant, which belongs at capture-open where it
/// already lives, not in a classifier.
///
/// Only the scrap arm calls it today (scrap is the cascade's last real
/// backend), so a build without that feature has nothing to classify.
#[cfg_attr(not(feature = "scrap-capture"), allow(dead_code))]
pub(crate) fn classify_capture_failure(
    detail: String,
    wayland_arm_error: Option<&str>,
) -> CaptureUnavailable {
    #[cfg(target_os = "macos")]
    {
        // ORDER IS THE ANSWER. A root LaunchDaemon is in session 0, which has
        // no WindowServer and never will, and its TCC preflight can still
        // answer "granted" — so asking about permission first would tell the
        // operator to go flip a toggle that changes nothing. `tcc`'s own
        // doctrine, and the shape of the MacBook's daemon row: same machine,
        // one agent captures and one cannot, for different reasons.
        if !crate::tcc::has_gui_session() {
            return CaptureUnavailable::new(CaptureUnavailableCode::NoDisplay, detail);
        }
        if !crate::tcc::screen_recording_granted() {
            return CaptureUnavailable::new(CaptureUnavailableCode::Permission, detail);
        }
    }
    // Only reached once every backend has already failed. "No display
    // variables" describes the DAEMON, not the host: a root systemd daemon
    // never has them, even with a Wayland desktop on the seat — so before FR-93
    // a GNOME host with someone at it read as "headless". The seat is asked
    // (linux_no_pixels_code), and an arm the operator switched on that failed
    // gets to say why in its own words.
    #[cfg(target_os = "linux")]
    let (code, detail) = {
        let aware = wayland_aware();
        let wayland_display = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let code = linux_no_pixels_code(
            std::env::var_os("DISPLAY").is_some(),
            wayland_display,
            aware,
            wayland_arm_error.is_some(),
            seat_shows_wayland,
        );
        let detail = match (code, wayland_arm_error) {
            (CaptureUnavailableCode::BackendError, Some(err)) if aware => err.to_string(),
            (CaptureUnavailableCode::WaylandNotEnabled, _) => {
                wayland_not_enabled_detail(wayland_display, &detail)
            }
            _ => detail,
        };
        (code, detail)
    };
    #[cfg(not(target_os = "linux"))]
    let code = {
        let _ = wayland_arm_error;
        CaptureUnavailableCode::BackendError
    };
    CaptureUnavailable::new(code, detail)
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
///
/// The live path: the pointer stays OUT of the frames wherever a backend can
/// leave it out, because it travels on its own channel (`cursor.rs`).
pub fn open_default(target_fps: u32, downscale: DownscalePolicy) -> Box<dyn ScreenCapture> {
    open_with(target_fps, downscale, pointer::PointerRequest::Separate)
}

/// FR-93 P2d — [`open_default`] for the media pumps, off the runtime's workers.
///
/// An attended portal open waits for the person at the screen for up to two
/// minutes (`portal::backend::HANDSHAKE_DEADLINE`), and the pumps called
/// `open_default` inline, so each unanswered dialog held one runtime worker for
/// the whole wait. Four stacked dialogs (seen on the FR-93 lab's KDE guest) is
/// every worker of a 4-core host, with signalling and heartbeats behind them.
/// On Linux the open now runs on the blocking pool. The pump also stays
/// droppable while it waits: a capture that opens after its session is gone is
/// dropped as it arrives. Elsewhere every open is quick, so it stays inline,
/// byte for byte.
///
/// P2d-2 — `on_wait` is called at most once, from the blocking pool, if the
/// open parks on a request for the person at the screen (the portal's dialog),
/// so the pump can tell the controller instead of streaming silence. An open
/// that asks nobody never calls it.
pub async fn open_default_async(
    target_fps: u32,
    downscale: DownscalePolicy,
    on_wait: OnOpenWait,
) -> Box<dyn ScreenCapture> {
    #[cfg(target_os = "linux")]
    {
        match tokio::task::spawn_blocking(move || {
            let _hook = ParkHook::install(on_wait);
            open_default(target_fps, downscale)
        })
        .await
        {
            Ok(capturer) => capturer,
            Err(e) => {
                tracing::warn!(error = %e, "capture: the open did not finish (it panicked, or the runtime is stopping)");
                Box::new(NoopCapture::unavailable(CaptureUnavailable::new(
                    CaptureUnavailableCode::BackendError,
                    format!("the capture open did not finish: {e}"),
                )))
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No open here waits for a person: there is no portal off Linux.
        drop(on_wait);
        open_default(target_fps, downscale)
    }
}

/// FR-93 P2d-2 — an open that is waiting for the person at the screen, as the
/// controller is told it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenWait {
    /// How long until the request gives up on its own. A duration rather than
    /// a time of day, so the viewer reads it on its own clock.
    pub gives_up_in: std::time::Duration,
}

/// What a pump hands [`open_default_async`]: called at most once, if and when
/// its open parks on a request for the person at the screen.
pub type OnOpenWait = Box<dyn FnOnce(OpenWait) + Send + 'static>;

#[cfg(target_os = "linux")]
thread_local! {
    /// The hook of the open running on THIS blocking thread. Only [`ParkHook`]
    /// sets it, and its drop clears it, so a pooled thread never carries one
    /// into the next open, not even after a panic.
    static PARK_HOOK: std::cell::RefCell<Option<OnOpenWait>> =
        const { std::cell::RefCell::new(None) };
}

/// Holds an open's [`OnOpenWait`] for as long as that open runs on this thread.
#[cfg(target_os = "linux")]
struct ParkHook;

#[cfg(target_os = "linux")]
impl ParkHook {
    fn install(hook: OnOpenWait) -> Self {
        PARK_HOOK.with(|h| *h.borrow_mut() = Some(hook));
        Self
    }
}

#[cfg(target_os = "linux")]
impl Drop for ParkHook {
    fn drop(&mut self) {
        PARK_HOOK.with(|h| h.borrow_mut().take());
    }
}

/// FR-93 P2d-2 — the portal broker's word that the open on this thread has
/// parked on a request for the person at the screen. Fires that open's hook,
/// once. A thread with no hook (the recorder, a probe, a test) does nothing.
#[cfg(target_os = "linux")]
#[cfg_attr(not(feature = "portal-capture"), allow(dead_code))]
pub(crate) fn tell_open_waiting(wait: OpenWait) {
    if let Some(hook) = PARK_HOOK.with(|h| h.borrow_mut().take()) {
        hook(wait);
    }
}

/// FR-85 P1d — the recorder's capturer: native resolution, and the pointer
/// drawn INTO the frames wherever the backend can draw it (WGC's session is
/// opened with cursor capture on; the live path's never is). The backends
/// that cannot draw it report where it is instead
/// ([`ScreenCapture::recorded_pointer`]), and the recorder draws it.
pub fn open_for_recording(target_fps: u32) -> Box<dyn ScreenCapture> {
    open_with(
        target_fps,
        DownscalePolicy::Never,
        pointer::PointerRequest::InFrame,
    )
}

/// The backend cascade, shared by the live path and the recorder.
fn open_with(
    _target_fps: u32,
    _downscale: DownscalePolicy,
    _pointer: pointer::PointerRequest,
) -> Box<dyn ScreenCapture> {
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
            match wgc_backend::WgcCapture::primary_with(_target_fps, _downscale, _pointer) {
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
    // FR-93 — the Wayland arms the operator switched ON and that failed, in
    // operator words. On a Wayland desktop the fallback below is then a real
    // error worth naming, not "capture is switched off".
    #[cfg(target_os = "linux")]
    #[allow(unused_mut, unused_variables)] // assigned/read only in featured builds
    let mut wayland_arm_error: Option<String> = None;

    // FR-93 P2 (decision D1): a per-user daemon in its own Wayland session,
    // with no capture key set, switches capture on by itself: mutter (GNOME's
    // own screencast, no dialog, the unattended peer of DRM), else the desktop
    // portal, which asks the person at the screen (P2d-3, see `WaylandPlan`).
    // Everything else (root, a key set, X11, nobody signed in, the kill switch)
    // runs the cascade below as before. With no key set, the DRM and portal
    // arms below are off anyway.
    #[cfg(target_os = "linux")]
    #[allow(unused_variables)] // read only with portal-capture
    let plan = wayland_plan(
        wayland_aware(),
        wayland_auto(),
        running_as_root(),
        capture_keys_set(),
        crate::virtual_desktop::requested(),
        own_session,
    );
    #[cfg(all(target_os = "linux", feature = "portal-capture"))]
    if plan == WaylandPlan::Auto {
        match open_auto_mutter(_target_fps) {
            AutoMutter::Opened(c) => return c,
            AutoMutter::NotGnome(why) => {
                tracing::info!(
                    %why,
                    "capture: no mutter on this desktop; trying the desktop portal, which asks \
                     the person at the screen (FR-93 P2d-3)"
                );
                match open_auto_portal(_target_fps) {
                    AutoPortal::Opened(c) => return c,
                    AutoPortal::Failed(detail) => {
                        tracing::warn!(%detail, "capture: the automatic portal arm failed (FR-93 P2d-3)");
                        wayland_arm_error = Some(detail);
                    }
                }
            }
            AutoMutter::Failed(detail) => {
                tracing::warn!(%detail, "capture: the automatic mutter arm failed (FR-93 P2)");
                wayland_arm_error = Some(detail);
            }
        }
    }

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
                    wayland_arm_error = Some(format!(
                        "drm_capture is on, but DRM capture could not open: {e:#}"
                    ));
                }
            }
        }
    }

    // FR-45 P3c-ii — the portal, tried AFTER DRM and BEFORE X11.
    //
    // ⚠️ OPT-IN, like DRM, and for a sharper reason: this path is ATTENDED
    // ONLY. It needs a logged-in user, and the first use shows that user a
    // consent dialog. Defaulting it on would mean an unattended host silently
    // waiting on a dialog nobody will ever answer — a hang dressed up as a
    // feature. A host that wants it sets `ROOMLERD_PORTAL_CAPTURE=1`.
    //
    // Ordering: DRM sees a locked screen and a greeter and needs no consent,
    // so it stays first wherever there is a CRTC. The portal is what serves
    // the hosts DRM cannot reach at all.
    #[cfg(all(target_os = "linux", feature = "portal-capture"))]
    {
        if tunnel_core::env::flag("PORTAL_CAPTURE", false) {
            match portal::pending::open(_target_fps) {
                Ok(c) => {
                    tracing::info!(
                        width = c.width(),
                        height = c.height(),
                        "capture: backend=portal (ROOMLERD_PORTAL_CAPTURE=1, xdg-desktop-portal \
                         ScreenCast — ATTENDED)"
                    );
                    return Box::new(c);
                }
                Err(e) => {
                    // Loud, like DRM's: the operator asked for this explicitly,
                    // so falling through silently would look like the feature
                    // simply not working.
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "ROOMLERD_PORTAL_CAPTURE=1 but portal capture could not open — falling \
                         through to the standard cascade"
                    );
                    let portal =
                        format!("portal_capture is on, but the portal could not open: {e:#}");
                    wayland_arm_error = Some(match wayland_arm_error.take() {
                        Some(drm) => format!("{drm}; {portal}"),
                        None => portal,
                    });
                }
            }
        }
    }

    // FR-80 — the reason the fallback exists, kept for the controller. scrap
    // is the last real backend in the cascade, so its error is the last word.
    // Each arm BINDS the reason rather than assigning into a pre-declared
    // `None`: on the success path this function returns, so an initial value
    // would be one no path ever reads.
    // FR-93 — never stream Xwayland's empty root. On a Wayland desktop a
    // per-user daemon's DISPLAY is Xwayland, XShm on it OPENS, and every frame
    // is black: the FR-93 lab measured `backend=scrap · lit=0.0 %` on a stock
    // GNOME 50 install, and the controller was told nothing. Ask the X server
    // whether it is Xwayland; if so, refuse it and say why.
    #[cfg(all(target_os = "linux", feature = "scrap-capture"))]
    if let Some(display) = std::env::var_os("DISPLAY") {
        let display = display.to_string_lossy().into_owned();
        let aware = wayland_aware();
        let is_xwayland = aware && x_server_is_xwayland(&display);
        if let Some(code) = xwayland_verdict(is_xwayland, aware, wayland_arm_error.is_some()) {
            let detail = match (code, wayland_arm_error.as_deref()) {
                (CaptureUnavailableCode::BackendError, Some(err)) => err.to_string(),
                _ => format!(
                    "DISPLAY {display} is Xwayland: on a Wayland desktop its root window is \
                     empty, so XShm would stream a black picture"
                ),
            };
            let reason = CaptureUnavailable::new(code, detail);
            tracing::warn!(
                code = reason.code.wire(),
                detail = %reason.detail,
                "capture: this is a Wayland desktop — refusing XShm on Xwayland's empty root (FR-93)"
            );
            return Box::new(NoopCapture::unavailable(reason));
        }
    }

    #[cfg(all(target_os = "linux", feature = "scrap-capture"))]
    let wayland_arm_failure = wayland_arm_error.as_deref();
    #[cfg(all(not(target_os = "linux"), feature = "scrap-capture"))]
    let wayland_arm_failure: Option<&str> = None;
    #[cfg(feature = "scrap-capture")]
    let reason = match scrap_backend::ScrapCapture::primary(_target_fps, _downscale) {
        Ok(c) => {
            tracing::info!(
                width = c.width(),
                height = c.height(),
                "capture: backend=scrap (DXGI/XShm/CoreGraphics)"
            );
            return Box::new(c);
        }
        Err(e) => {
            let detail = format!("{e:#}");
            let classified = classify_capture_failure(detail.clone(), wayland_arm_failure);
            tracing::warn!(
                error = %detail,
                code = classified.code.wire(),
                "scrap capture unavailable — falling back to NoopCapture"
            );
            classified
        }
    };
    #[cfg(not(feature = "scrap-capture"))]
    let reason = {
        tracing::info!(
            "built without scrap-capture feature — using NoopCapture. \
             Rebuild with `--features scrap-capture` for real screen capture."
        );
        CaptureUnavailable::new(
            CaptureUnavailableCode::NotBuilt,
            "this build carries no capture backend",
        )
    };
    Box::new(NoopCapture::unavailable(reason))
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

/// FR-85 decision 6 — does this process capture the synthetic test source?
/// Always `false` in a build without `synthetic-frame-source`, whatever the
/// environment says: the capture ignores the variable there, so trusting it
/// would let a stray variable call a real screen "no one's" (the recorder's
/// unattended exception reads this).
pub fn synthetic_frames_requested() -> bool {
    #[cfg(feature = "synthetic-frame-source")]
    {
        synthetic_env_enabled()
    }
    #[cfg(not(feature = "synthetic-frame-source"))]
    {
        false
    }
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

/// FR-93 P1 — the verdicts, as tables. Pure on purpose: each test that set
/// `DISPLAY` or `WAYLAND_DISPLAY` would race every other test in the binary.
#[cfg(test)]
mod wayland_reason_tests {
    use super::CaptureUnavailableCode::{self, *};
    use super::{linux_no_pixels_code, wayland_hint, wayland_not_enabled_detail, xwayland_verdict};

    fn seat(answer: bool) -> impl FnOnce() -> bool {
        move || answer
    }

    fn seat_unasked() -> impl FnOnce() -> bool {
        || -> bool { panic!("logind was asked, and this answer does not depend on the seat") }
    }

    /// The root-daemon bug (FR-93 AC2): a systemd unit has no display
    /// variables, so before FR-93 a host with GNOME on its seat read as
    /// headless. Only the SEAT can tell the two apart.
    #[test]
    fn a_root_daemon_asks_the_seat_before_calling_a_host_headless() {
        assert_eq!(
            linux_no_pixels_code(false, false, true, false, seat(true)),
            WaylandNotEnabled
        );
        assert_eq!(
            linux_no_pixels_code(false, false, true, false, seat(false)),
            NoDisplay,
            "a host with no graphical session is still headless"
        );
    }

    #[test]
    fn the_daemons_own_wayland_display_needs_no_seat_lookup() {
        assert_eq!(
            linux_no_pixels_code(false, true, true, false, seat_unasked()),
            WaylandNotEnabled
        );
    }

    /// An arm the operator switched on that failed is a real error, in its own
    /// words — never "switched off", which would tell them to set a key they
    /// already set.
    #[test]
    fn a_wayland_arm_that_failed_is_an_error_not_switched_off() {
        for (display, wayland_display) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            assert_eq!(
                linux_no_pixels_code(display, wayland_display, true, true, seat_unasked()),
                BackendError,
                "DISPLAY={display} WAYLAND_DISPLAY={wayland_display}"
            );
        }
    }

    /// An X display that scrap could not open is scrap's error. (One that
    /// OPENED on Xwayland never reaches the classifier: `xwayland_verdict`.)
    #[test]
    fn an_x_display_scrap_could_not_open_stays_a_backend_error() {
        assert_eq!(
            linux_no_pixels_code(true, false, true, false, seat_unasked()),
            BackendError
        );
    }

    /// The kill switch gives back the rule before FR-93 exactly, and never
    /// asks logind.
    #[test]
    fn wayland_aware_off_is_the_old_rule_and_asks_nobody() {
        let old = |display: bool, wayland_display: bool| {
            if !display && !wayland_display {
                NoDisplay
            } else {
                BackendError
            }
        };
        for arm_failed in [false, true] {
            for (display, wayland_display) in
                [(false, false), (false, true), (true, false), (true, true)]
            {
                assert_eq!(
                    linux_no_pixels_code(
                        display,
                        wayland_display,
                        false,
                        arm_failed,
                        seat_unasked()
                    ),
                    old(display, wayland_display),
                    "DISPLAY={display} WAYLAND_DISPLAY={wayland_display} arm_failed={arm_failed}"
                );
            }
        }
    }

    /// The per-user-daemon bug (FR-93 AC1): XShm OPENS on Xwayland and
    /// streams its empty root — black.
    #[test]
    fn xshm_on_xwayland_is_refused_with_a_reason() {
        assert_eq!(xwayland_verdict(true, true, false), Some(WaylandNotEnabled));
        assert_eq!(
            xwayland_verdict(true, true, true),
            Some(BackendError),
            "an arm that failed says why, rather than streaming black"
        );
        assert_eq!(
            xwayland_verdict(false, true, false),
            None,
            "a real X server is captured"
        );
        assert_eq!(xwayland_verdict(false, true, true), None);
        assert_eq!(
            xwayland_verdict(true, false, false),
            None,
            "the kill switch"
        );
        assert_eq!(xwayland_verdict(true, false, true), None);
    }

    /// The viewer prints the detail under the reason, and X11's own words
    /// alone ("connection refused") read like a network fault. What the agent
    /// found comes first, and fits the wire's 200-char cap with room to spare.
    #[test]
    fn the_detail_says_what_was_found_before_x11s_words() {
        let x11 = "no primary display: connection refused";
        let seat = wayland_not_enabled_detail(false, x11);
        assert!(seat.starts_with("logind shows a Wayland session"), "{seat}");
        assert!(seat.ends_with(x11), "{seat}");
        assert!(seat.chars().count() <= 200, "{seat}");
        let own = wayland_not_enabled_detail(true, x11);
        assert!(own.contains("WAYLAND_DISPLAY"), "{own}");
    }

    /// The hint is spliced into the `rc:media-unavailable` JSON unescaped
    /// (`peer::capture_unavailable_payload`), and `hint()` shows only the
    /// variant for THIS process's euid — so both variants are checked here.
    #[test]
    fn every_hint_is_a_safe_json_literal() {
        let codes: [CaptureUnavailableCode; 5] = [
            Permission,
            NoDisplay,
            NotBuilt,
            BackendError,
            WaylandNotEnabled,
        ];
        let mut hints: Vec<&str> = codes.iter().map(|c| c.hint()).collect();
        hints.push(wayland_hint(true));
        hints.push(wayland_hint(false));
        for hint in hints {
            assert!(
                !hint
                    .chars()
                    .any(|c| c == '"' || c == '\\' || (c as u32) < 0x20),
                "a hint would break the payload: {hint}"
            );
        }
    }

    /// Each install type is told the keys it can actually use: only root can
    /// read the scanout or create a uinput device. A per-user daemon captures
    /// by itself (FR-93 P2: mutter, else the portal), so it reads this only
    /// when that was switched off, and is told how to give the choice back.
    #[test]
    fn the_hint_names_the_keys_this_install_can_use() {
        let root = wayland_hint(true);
        assert!(
            root.contains("sudo roomler config set drm_capture true"),
            "{root}"
        );
        assert!(root.contains("uinput"), "{root}");
        assert!(!root.contains("portal_capture"), "{root}");

        let user = wayland_hint(false);
        assert!(user.contains("roomler config clear wayland_auto"), "{user}");
        assert!(user.contains("portal_capture"), "{user}");
        assert!(user.contains("mutter_capture"), "{user}");
        assert!(!user.contains("sudo"), "{user}");
        assert!(!user.contains("drm_capture"), "{user}");
    }
}

/// FR-93 P2 — decision D1 as a table. Pure: the session is a closure, so no
/// test touches the environment or logind (that would race every other test
/// in the binary). The order of the cheap checks is part of the contract: a
/// daemon that may not go automatic never runs loginctl.
#[cfg(test)]
mod wayland_auto_tests {
    use super::{OwnSession, WaylandPlan, wayland_plan};

    fn session(s: OwnSession) -> impl FnOnce() -> OwnSession {
        move || s
    }

    fn session_unasked() -> impl FnOnce() -> OwnSession {
        || -> OwnSession { panic!("logind was asked, and this answer does not depend on it") }
    }

    /// D1's per-user half: a Wayland session of its own and no key set. The
    /// automatic arm is mutter, else the portal (P2d-3, see `WaylandPlan`).
    #[test]
    fn a_per_user_daemon_in_its_own_wayland_session_goes_automatic() {
        assert_eq!(
            wayland_plan(
                true,
                true,
                false,
                false,
                false,
                session(OwnSession::Wayland)
            ),
            WaylandPlan::Auto
        );
    }

    /// D1's system half: DRM and uinput stay opt-in on a root install.
    #[test]
    fn a_root_daemon_never_switches_an_arm_on_by_itself() {
        assert_eq!(
            wayland_plan(true, true, true, false, false, session_unasked()),
            WaylandPlan::AsConfigured
        );
    }

    /// Any capture key, to any value, hands the cascade back to the operator.
    #[test]
    fn an_explicit_key_always_wins() {
        assert_eq!(
            wayland_plan(true, true, false, true, false, session_unasked()),
            WaylandPlan::AsConfigured
        );
    }

    /// Both kill switches give back the cascade before P2, byte for byte.
    #[test]
    fn either_kill_switch_gives_back_the_old_cascade() {
        assert_eq!(
            wayland_plan(true, false, false, false, false, session_unasked()),
            WaylandPlan::AsConfigured,
            "wayland_auto = false"
        );
        assert_eq!(
            wayland_plan(false, true, false, false, false, session_unasked()),
            WaylandPlan::AsConfigured,
            "wayland_aware = false"
        );
    }

    /// The daemon's own Xvfb is what it captures, Wayland session or not.
    #[test]
    fn a_virtual_desktop_is_never_replaced() {
        assert_eq!(
            wayland_plan(true, true, false, false, true, session_unasked()),
            WaylandPlan::AsConfigured
        );
    }

    /// X11 keeps FR-29's numbers: auto applies only where logind says Wayland.
    #[test]
    fn x11_or_nobody_signed_in_is_not_automatic() {
        assert_eq!(
            wayland_plan(true, true, false, false, false, session(OwnSession::Other)),
            WaylandPlan::AsConfigured
        );
    }

    /// "Any capture key takes the choice back" holds only if every key in the
    /// list reaches `node_env` from the config. A name that is not bridged
    /// would be a `roomler config set` that the cascade never sees, and auto
    /// would stay on under an operator who switched it off.
    #[cfg(target_os = "linux")]
    #[test]
    fn every_capture_key_is_a_bridged_config_key() {
        let bridged: Vec<&str> = crate::config::env_bridge_bools(&crate::config::test_fixture())
            .iter()
            .map(|(k, _)| *k)
            .collect();
        for key in super::CAPTURE_KEYS
            .iter()
            .chain(["WAYLAND_AUTO", "MUTTER_INPUT"].iter())
        {
            assert!(bridged.contains(key), "{key} is not in env_bridge_bools");
        }
    }
}

/// FR-93 P2d-3 — the automatic portal raises a dialog only where a portal
/// serves ScreenCast, and every other answer becomes a reason the viewer can
/// read in full.
#[cfg(all(test, target_os = "linux", feature = "portal-capture"))]
mod auto_portal_tests {
    use super::portal::PortalStatus;
    use super::{CaptureUnavailable, CaptureUnavailableCode, auto_portal_unusable};

    #[test]
    fn a_portal_that_serves_screencast_is_tried_and_every_other_answer_is_a_reason() {
        for usable in [
            PortalStatus::Available {
                remote_desktop: true,
                version: Some(5),
            },
            // Capture needs no RemoteDesktop (wlroots has none).
            PortalStatus::Available {
                remote_desktop: false,
                version: None,
            },
        ] {
            assert_eq!(auto_portal_unusable(&usable), None, "{usable}");
        }
        for status in [
            PortalStatus::NoScreenCast,
            PortalStatus::PortalAbsent,
            PortalStatus::NoSessionBus,
            PortalStatus::Unknown("org.freedesktop.portal.Desktop did not answer within 5s".into()),
        ] {
            let why = auto_portal_unusable(&status).expect("a reason, and no dialog");
            // What the viewer is shown is capped; the status must survive it.
            let shown = CaptureUnavailable::new(CaptureUnavailableCode::BackendError, why);
            assert!(
                shown.detail.contains(&status.to_string()),
                "{status}: {}",
                shown.detail
            );
        }
    }
}

/// FR-93 P2b — the desktop brief's `capture_arm` as a table, in the cascade's
/// order. Pure, like the plan above: no test reads the environment or logind.
#[cfg(all(test, target_os = "linux"))]
mod planned_arm_tests {
    use super::{ArmFacts, CaptureArm, OwnSession, own_session_in, planned_arm};
    use crate::keep_busy::logind::Session;

    fn built() -> ArmFacts {
        ArmFacts {
            x11_built: true,
            ..Default::default()
        }
    }

    #[test]
    fn the_cascade_order_decides_and_the_first_arm_switched_on_wins() {
        let all = ArmFacts {
            auto_mutter: true,
            drm: true,
            portal: true,
            x11_session: true,
            ..built()
        };
        assert_eq!(planned_arm(&all), Some(CaptureArm::Mutter), "auto first");
        let keys = ArmFacts {
            auto_mutter: false,
            ..all
        };
        assert_eq!(
            planned_arm(&keys),
            Some(CaptureArm::Drm),
            "then drm_capture"
        );
        let portal = ArmFacts { drm: false, ..keys };
        assert_eq!(planned_arm(&portal), Some(CaptureArm::Portal));
        let via_mutter = ArmFacts {
            portal_via_mutter: true,
            ..portal
        };
        assert_eq!(planned_arm(&via_mutter), Some(CaptureArm::Mutter));
        // `mutter_capture` alone opens nothing: the portal block is gated on
        // `portal_capture`.
        let mutter_key_only = ArmFacts {
            portal: false,
            portal_via_mutter: true,
            x11_session: false,
            ..built()
        };
        assert_eq!(planned_arm(&mutter_key_only), None);
    }

    /// P2d-3 — where mutter is absent the automatic arm is the portal, and it
    /// comes before every configured arm, as in `open_with`. mutter, which asks
    /// nobody, still comes first.
    #[test]
    fn the_automatic_portal_is_planned_where_mutter_is_absent() {
        let auto_portal = ArmFacts {
            auto_portal: true,
            ..built()
        };
        assert_eq!(planned_arm(&auto_portal), Some(CaptureArm::Portal));
        assert!(
            CaptureArm::Portal.attended(),
            "the one automatic arm that asks the person at the screen"
        );
        let both = ArmFacts {
            auto_mutter: true,
            ..auto_portal
        };
        assert_eq!(planned_arm(&both), Some(CaptureArm::Mutter), "mutter first");
        let with_drm = ArmFacts {
            drm: true,
            x11_session: true,
            ..auto_portal
        };
        assert_eq!(
            planned_arm(&with_drm),
            Some(CaptureArm::Portal),
            "the automatic arm runs before the configured ones"
        );
    }

    /// FR-93 P4 — the route a planned arm would open. The automatic mutter arm
    /// carries mutter's input unless `mutter_input` is off; every other arm
    /// keeps input opt-in (`portal_input`), as before P4.
    #[test]
    fn the_planned_input_route_follows_the_arm() {
        use super::planned_input_route;
        use crate::input::reach::RouteKind;
        let auto = ArmFacts {
            auto_mutter: true,
            mutter_input: true,
            ..built()
        };
        assert_eq!(planned_input_route(&auto), Some(RouteKind::Mutter));
        let view_only = ArmFacts {
            mutter_input: false,
            ..auto
        };
        assert_eq!(planned_input_route(&view_only), None, "the kill switch");

        // P4b — the automatic portal asks for remote control unless
        // `portal_input = false`. Unset, `portal_input` reads false for a
        // configured arm and true for the automatic one.
        let auto_portal = ArmFacts {
            auto_portal: true,
            auto_portal_input: true,
            portal_input: false,
            mutter_input: true,
            ..built()
        };
        assert_eq!(planned_input_route(&auto_portal), Some(RouteKind::Portal));
        let view_only_portal = ArmFacts {
            auto_portal_input: false,
            ..auto_portal
        };
        assert_eq!(
            planned_input_route(&view_only_portal),
            None,
            "portal_input = false keeps the automatic portal view-only"
        );

        let configured_mutter = ArmFacts {
            portal: true,
            portal_via_mutter: true,
            mutter_input: true,
            ..built()
        };
        assert_eq!(
            planned_input_route(&configured_mutter),
            None,
            "a configured arm keeps input opt-in"
        );
        let configured_mutter_input = ArmFacts {
            portal_input: true,
            ..configured_mutter
        };
        assert_eq!(
            planned_input_route(&configured_mutter_input),
            Some(RouteKind::Mutter)
        );
        assert_eq!(
            planned_input_route(&ArmFacts {
                mutter_input: false,
                ..configured_mutter_input
            }),
            None
        );

        let drm = ArmFacts {
            drm: true,
            portal_input: true,
            mutter_input: true,
            ..built()
        };
        assert_eq!(
            planned_input_route(&drm),
            None,
            "DRM's input is uinput's, not a route"
        );
    }

    #[test]
    fn a_wayland_desktop_with_nothing_switched_on_captures_nothing() {
        // Not X11, no arm: the Xwayland guard refuses scrap, so `none`.
        assert_eq!(planned_arm(&built()), None);
        // An X11 session, or the daemon's own Xvfb, is X11's.
        let x11 = ArmFacts {
            x11_session: true,
            ..built()
        };
        assert_eq!(planned_arm(&x11), Some(CaptureArm::X11));
        let xvfb = ArmFacts {
            virtual_desktop: true,
            ..built()
        };
        assert_eq!(planned_arm(&xvfb), Some(CaptureArm::X11));
        // …in a build that has the X11 backend at all.
        let unbuilt = ArmFacts {
            x11_built: false,
            ..x11
        };
        assert_eq!(planned_arm(&unbuilt), None);
    }

    #[test]
    fn own_session_in_counts_only_this_users_active_wayland_desktop() {
        let s = |uid: u32, class: &str, active: bool, kind: &str| Session {
            id: "1".into(),
            uid,
            class: class.into(),
            active,
            locked: false,
            display: None,
            kind: kind.into(),
            desktop: None,
        };
        let mine = [s(1000, "user", true, "wayland")];
        assert_eq!(own_session_in(&mine, 1000), OwnSession::Wayland);
        assert_eq!(own_session_in(&mine, 1001), OwnSession::Other);
        let greeter = [s(1000, "greeter", true, "wayland")];
        assert_eq!(own_session_in(&greeter, 1000), OwnSession::Other);
        let idle = [s(1000, "user", false, "wayland")];
        assert_eq!(own_session_in(&idle, 1000), OwnSession::Other);
        let x11 = [s(1000, "user", true, "x11")];
        assert_eq!(own_session_in(&x11, 1000), OwnSession::Other);
    }
}

/// FR-93 — the Xwayland guard against a REAL X server, the half CI can run:
/// a server that is NOT Xwayland must never be refused, or every X11 host and
/// the daemon's own virtual desktop would lose capture (AC11). The Xwayland
/// half is proven on the FR-93 lab's GNOME guests. `ROOMLERD_TEST_X11=1` makes
/// a missing server a failure, as in `capture::pointer`'s lane.
#[cfg(all(test, target_os = "linux", feature = "scrap-capture"))]
mod xwayland_probe_tests {
    use super::{x_has_extension, x_server_is_xwayland, x_server_query};
    use std::time::{Duration, Instant};

    /// Only the CI lane knows its server is Xvfb; a developer's own session
    /// may well BE Xwayland.
    fn xvfb_display() -> Option<String> {
        if std::env::var("ROOMLERD_TEST_X11").as_deref() != Ok("1") {
            eprintln!("ROOMLERD_TEST_X11 unset — skipping (the Xvfb lane sets it)");
            return None;
        }
        Some(std::env::var("DISPLAY").expect("ROOMLERD_TEST_X11=1 but DISPLAY is unset"))
    }

    #[test]
    fn an_x_server_that_is_not_xwayland_is_captured() {
        let Some(display) = xvfb_display() else {
            return;
        };
        // The control: the probe's own Unix-only path REACHES the server and
        // reads a real answer. Without it, a probe that could never connect
        // would pass the assertion below by saying "no" to everything.
        assert_eq!(
            x_server_query(&display, |c, _| x_has_extension(c, "XFIXES")),
            Some(true),
            "the probe could not read {display}'s extensions"
        );
        assert!(
            !x_server_is_xwayland(&display),
            "Xvfb on {display} read as Xwayland: the guard would refuse every X11 host"
        );
        println!("x11-real: Xvfb does not advertise XWAYLAND");
    }

    /// Xwayland before 23.1 has no extension of its own, and the lab's GNOME 50
    /// cannot show that half — so it is shown here: RandR 1.5 lets any client
    /// define a monitor, and Xvfb lacks the extension, so ONLY the monitor
    /// fallback can see this one.
    #[test]
    fn an_old_xwayland_is_known_by_its_monitors() {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::randr::{ConnectionExt as _, MonitorInfo};
        use x11rb::protocol::xproto::ConnectionExt as _;
        use x11rb::wrapper::ConnectionExt as _;
        let Some(display) = xvfb_display() else {
            return;
        };
        let (conn, screen) = x11rb::connect(Some(&display)).expect("the Xvfb");
        let root = conn.setup().roots[screen].root;
        conn.randr_query_version(1, 5).unwrap().reply().unwrap();
        let name = conn
            .intern_atom(false, b"XWAYLAND0")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let monitor = MonitorInfo {
            name,
            primary: false,
            automatic: false,
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            width_in_millimeters: 10,
            height_in_millimeters: 10,
            outputs: Vec::new(),
        };
        conn.randr_set_monitor(root, monitor).unwrap();
        conn.sync().unwrap();
        let seen = x_server_is_xwayland(&display);
        conn.randr_delete_monitor(root, name).unwrap();
        conn.sync().unwrap();
        assert!(seen, "an XWAYLAND0 monitor did not read as Xwayland");
        assert!(
            !x_server_is_xwayland(&display),
            "and once it is gone, the server is plain X again"
        );
        println!("x11-real: an XWAYLAND monitor reads as Xwayland");
    }

    /// The stall the Unix-only rule exists for: `x11rb::connect` would try
    /// TCP for this display, and a TEST-NET-1 address never answers a SYN.
    #[test]
    fn a_tcp_display_is_never_dialled() {
        let start = Instant::now();
        assert_eq!(x_server_query("192.0.2.1:0", |_, _| true), None);
        assert!(!x_server_is_xwayland("192.0.2.1:0"));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the probe dialled TCP: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn an_x_server_with_no_socket_is_not_xwayland() {
        // Scrap then reports its own connection error, which is the truth.
        assert_eq!(x_server_query(":4242", |_, _| true), None);
        assert!(!x_server_is_xwayland(":4242"));
    }
}

// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! OS-native cursor capture (position + shape).
//!
//! The capture backend delivers screen pixels but historically omitted
//! the mouse cursor — DXGI Desktop Duplication hides the cursor in its
//! frames by default because the cursor is a compositor overlay, not a
//! window. This module fills that gap: it polls the OS for the current
//! cursor position + shape, caches shape bitmaps by handle so we don't
//! re-serialise on every frame, and surfaces a `CursorTick` struct the
//! peer layer turns into `cursor:*` data-channel messages.
//!
//! Wire protocol (over the `cursor` data channel, reliable + ordered):
//!
//! ```text
//! { "t": "cursor:pos",   "id": u64, "x": i32, "y": i32 }
//! { "t": "cursor:shape", "id": u64, "w": u32, "h": u32, "hx": i32,
//!                        "hy": i32, "bgra": "<base64>", "css": str|null }
//! { "t": "cursor:hide" }
//! ```
//!
//! `id` is the hash of the cursor HCURSOR handle — constant for the
//! duration of a shape, changes on I-beam / resize / link click etc.
//! The browser maps `id` → `ImageBitmap` cache so it only decodes each
//! shape once per session, and never replaces a cached id.
//!
//! On Linux the DRM arm reads the hardware cursor plane (FR-93 P3c,
//! `capture::drm_cursor`), and the X11 arm reads XFixes into the same slot
//! (FR-97 P3a, `capture::x11_cursor`). There `id` is a hash of the image and
//! its hotspot, since neither has a handle that names a shape.

use crate::capture::CursorInfo;
use std::time::Duration;

/// FR-98 C4 — the cursor poller's period unless `ROOMLERD_CURSOR_POLL_MS`
/// says otherwise: 8 ms, 125 Hz (rc.38).
pub const DEFAULT_POLL: Duration = Duration::from_millis(8);

/// The poller's period from `ROOMLERD_CURSOR_POLL_MS`, read once per
/// session. Clamped to 2–50 ms: below 2 the poll and its data-channel send
/// stop being cheap, above 50 the remote cursor visibly trails the pointer.
/// Anything unparseable keeps the default. A knob for the field A/B: FR-98
/// keeps a cadence change only if L4 shows it moving input to photon.
pub fn poll_interval(raw: Option<&str>) -> Duration {
    match raw.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(ms) => Duration::from_millis(ms.clamp(2, 50)),
        None => DEFAULT_POLL,
    }
}

#[cfg(test)]
mod poll_tests {
    use super::*;

    #[test]
    fn the_poll_period_is_8_ms_unless_set_and_stays_in_2_to_50() {
        assert_eq!(poll_interval(None), DEFAULT_POLL);
        assert_eq!(DEFAULT_POLL, Duration::from_millis(8));
        assert_eq!(poll_interval(Some("4")), Duration::from_millis(4));
        assert_eq!(poll_interval(Some(" 16 ")), Duration::from_millis(16));
        assert_eq!(poll_interval(Some("0")), Duration::from_millis(2));
        assert_eq!(poll_interval(Some("1000")), Duration::from_millis(50));
        for junk in ["", "fast", "-4", "4ms", "4.5"] {
            assert_eq!(poll_interval(Some(junk)), DEFAULT_POLL, "{junk:?}");
        }
    }
}

/// Per-poll result. `None` means the cursor isn't visible (fullscreen
/// video, hidden by software). `Some` returns the current position;
/// `shape_bgra` is `Some(bytes)` only on the first poll that sees a
/// given `shape_id` — subsequent polls at the same id omit the
/// bitmap so the data channel stays cheap.
#[derive(Debug, Clone)]
pub struct CursorTick {
    pub x: i32,
    pub y: i32,
    pub shape_id: u64,
    /// Populated once per shape change. Includes `w`, `h`,
    /// `hotspot_x`, `hotspot_y` and the ARGB pixel buffer.
    pub shape: Option<CursorInfo>,
}

/// Poll-driven cursor tracker. `poll()` returns `None` when the
/// cursor isn't visible; `Some(CursorTick)` otherwise. Keeps a small
/// cache of the last-seen `shape_id` so the same shape isn't
/// serialised twice in a row.
pub struct CursorTracker {
    #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
    inner: windows_impl::WindowsCursorTracker,
    /// Whether the caller has already received a `shape` payload for
    /// the current `shape_id`. Resets when the handle changes. Only
    /// consulted on Windows with the mf-encoder feature, and on Linux
    /// with DRM or X11 capture (FR-93 P3c, FR-97 P3a); other builds keep
    /// the field but never read it. Allowed dead.
    #[allow(dead_code)]
    last_advertised_shape: Option<u64>,
}

impl CursorTracker {
    pub fn new() -> Self {
        Self {
            #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
            inner: windows_impl::WindowsCursorTracker::new(),
            last_advertised_shape: None,
        }
    }

    /// Non-blocking poll. Called at the capture cadence (30 Hz today).
    /// Cheap — the Windows implementation is two user32 syscalls per
    /// frame plus a GetDIBits when the shape changes.
    pub fn poll(&mut self) -> Option<CursorTick> {
        #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
        {
            let raw = self.inner.poll()?;
            let new_shape = self.last_advertised_shape != Some(raw.shape_id);
            let shape = if new_shape { raw.shape.clone() } else { None };
            if shape.is_some() {
                self.last_advertised_shape = Some(raw.shape_id);
            }
            Some(CursorTick {
                x: raw.x,
                y: raw.y,
                shape_id: raw.shape_id,
                shape,
            })
        }
        #[cfg(all(
            target_os = "linux",
            any(feature = "drm-capture", feature = "scrap-capture")
        ))]
        {
            // FR-93 P3c — the DRM arm's hardware cursor, which its worker reads
            // off the cursor plane every frame (`capture::drm_cursor`), and
            // from FR-97 P3a the X11 arm's pointer, which its worker reads from
            // XFixes into the same slot (`capture::x11_cursor`). mutter and the
            // portal put the pointer in their frames and publish nothing, so
            // this stays `None` for them: the viewer's badge, as before, and
            // never a second pointer over the streamed one.
            let snapshot = super::drm_cursor::latest()?;
            let new_shape = self.last_advertised_shape != Some(snapshot.shape_id);
            let shape = new_shape.then(|| (*snapshot.shape).clone());
            if shape.is_some() {
                self.last_advertised_shape = Some(snapshot.shape_id);
            }
            Some(CursorTick {
                x: snapshot.x,
                y: snapshot.y,
                shape_id: snapshot.shape_id,
                shape,
            })
        }
        #[cfg(not(any(
            all(target_os = "windows", feature = "mf-encoder"),
            all(
                target_os = "linux",
                any(feature = "drm-capture", feature = "scrap-capture")
            )
        )))]
        {
            // macOS, and Linux builds with neither DRM nor X11 capture: not
            // wired. NSCursor would slot in here. Returning None falls back to
            // the browser's synthetic-badge rendering.
            None
        }
    }

    /// FR-85 P1d — the same poll with the shape on EVERY tick, for a
    /// recording, which draws the pointer itself and keeps its own cache by
    /// `shape_id` (`capture::pointer::WindowsPointer`). Leaves the "already
    /// advertised" bookkeeping of [`Self::poll`] alone.
    #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
    pub fn poll_with_shape(&mut self) -> Option<CursorTick> {
        let raw = self.inner.poll()?;
        Some(CursorTick {
            x: raw.x,
            y: raw.y,
            shape_id: raw.shape_id,
            shape: raw.shape,
        })
    }
}

impl Default for CursorTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
mod windows_impl {
    //! GetCursorInfo + GetIconInfo + GetDIBits backed tracker.
    //!
    //! The cursor shape is stored by `HCURSOR` handle — the OS
    //! recycles a small pool of them (arrow, I-beam, hand, resize,
    //! etc.), so caching by handle value lets us re-use decoded
    //! bitmaps without repeated `GetDIBits` calls.

    use crate::capture::CursorInfo;
    use std::collections::HashMap;

    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC,
        GetDIBits, GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, CURSORINFO_FLAGS, GetCursorInfo, GetIconInfo, HCURSOR, HICON,
        ICONINFO, IDC_APPSTARTING, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO,
        IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_UPARROW, IDC_WAIT,
        LoadCursorW,
    };
    use windows::core::PCWSTR;

    /// Matches the module-level `CursorTick` but with the shape always
    /// carried (the outer [`super::CursorTracker`] filters by
    /// "already-advertised" to decide whether to forward the bitmap).
    pub(super) struct RawCursorTick {
        pub(super) x: i32,
        pub(super) y: i32,
        pub(super) shape_id: u64,
        pub(super) shape: Option<CursorInfo>,
    }

    pub(super) struct WindowsCursorTracker {
        /// Cached shapes by HCURSOR handle. Windows recycles handles
        /// within a session so this stays small (~10 entries).
        shapes: HashMap<u64, CursorInfo>,
        /// Handles of the standard stock cursors → their CSS `cursor`
        /// keyword. Loaded once; `GetCursorInfo` reports the same shared
        /// HCURSOR when an app selects a stock cursor, so a handle match
        /// lets the browser render the viewer's real OS cursor.
        system_cursors: HashMap<u64, &'static str>,
    }

    impl WindowsCursorTracker {
        pub(super) fn new() -> Self {
            Self {
                shapes: HashMap::new(),
                system_cursors: load_system_cursors(),
            }
        }

        /// Map a live HCURSOR to a CSS cursor keyword when it is one of
        /// the standard stock cursors, else `None` (app-custom cursor →
        /// the streamed bitmap is used).
        fn classify(&self, hcursor: HCURSOR) -> Option<&'static str> {
            self.system_cursors.get(&handle_to_id(hcursor)).copied()
        }

        pub(super) fn poll(&mut self) -> Option<RawCursorTick> {
            unsafe {
                let mut ci = CURSORINFO {
                    cbSize: size_of::<CURSORINFO>() as u32,
                    flags: CURSORINFO_FLAGS(0),
                    hCursor: HCURSOR::default(),
                    ptScreenPos: POINT::default(),
                };
                if GetCursorInfo(&mut ci).is_err() {
                    return None;
                }
                // Cursor hidden — don't advertise a tick; caller sends
                // cursor:hide.
                if (ci.flags.0 & CURSOR_SHOWING.0) == 0 || ci.hCursor.is_invalid() {
                    return None;
                }
                let shape_id = handle_to_id(ci.hCursor);
                // Classify the live cursor against the stock set so the
                // browser can render the viewer's real OS cursor for
                // standard shapes (I-beam, arrow, hand, resize, …). The
                // hint is cached with the shape (same HCURSOR → same
                // type) and set even on the fallback, so a stock cursor
                // GetDIBits can't decode still renders natively.
                let css = self.classify(ci.hCursor);
                let shape = if let Some(cached) = self.shapes.get(&shape_id) {
                    Some(cached.clone())
                } else if let Some(mut info) = extract_shape(ci.hCursor) {
                    info.css = css;
                    self.shapes.insert(shape_id, info.clone());
                    Some(info)
                } else {
                    // extract_shape failed for this HCURSOR (unusual mask
                    // bitmaps, custom app cursors, or bitmaps GetDIBits
                    // can't decode). Without a shape the browser hides
                    // the canvas overlay and the controller sees the
                    // cursor "disappear". Fall back to a hardcoded
                    // black-outline arrow so the position indicator is
                    // always visible — better UX than a vanishing
                    // cursor. Cache under this shape_id so we don't
                    // rebuild the fallback on every poll.
                    let mut fallback = synthetic_arrow();
                    fallback.css = css;
                    self.shapes.insert(shape_id, fallback.clone());
                    Some(fallback)
                };
                Some(RawCursorTick {
                    x: ci.ptScreenPos.x,
                    y: ci.ptScreenPos.y,
                    shape_id,
                    shape,
                })
            }
        }
    }

    fn handle_to_id(h: HCURSOR) -> u64 {
        h.0 as usize as u64
    }

    /// Load the process-shared handles of the standard stock cursors and
    /// map each to its CSS `cursor` keyword. Stock cursors are shared
    /// system resources, so `LoadCursorW` returns the same handle the
    /// active window selected via `SetCursor` — a handle match therefore
    /// identifies the cursor TYPE without inspecting pixels. App-custom
    /// cursors never match and fall through to the bitmap path.
    fn load_system_cursors() -> HashMap<u64, &'static str> {
        let table: &[(PCWSTR, &'static str)] = &[
            (IDC_ARROW, "default"),
            (IDC_IBEAM, "text"),
            (IDC_HAND, "pointer"),
            (IDC_WAIT, "wait"),
            (IDC_APPSTARTING, "progress"),
            (IDC_CROSS, "crosshair"),
            (IDC_SIZEWE, "ew-resize"),
            (IDC_SIZENS, "ns-resize"),
            (IDC_SIZENESW, "nesw-resize"),
            (IDC_SIZENWSE, "nwse-resize"),
            (IDC_SIZEALL, "move"),
            (IDC_NO, "not-allowed"),
            (IDC_HELP, "help"),
            (IDC_UPARROW, "default"),
        ];
        let mut map = HashMap::with_capacity(table.len());
        for (idc, css) in table {
            unsafe {
                if let Ok(h) = LoadCursorW(None, *idc)
                    && !h.is_invalid()
                {
                    map.insert(handle_to_id(h), *css);
                }
            }
        }
        map
    }

    /// Decode a cursor's shape into an ARGB bitmap + hotspot. Returns
    /// None on any OS error — the caller keeps the cached entry or
    /// skips emitting a shape this poll.
    ///
    /// Three cursor flavours we have to handle:
    /// 1. **Modern alpha cursor** (e.g. Windows 11 desktop arrow):
    ///    `hbmColor` is 32-bit BGRA with a valid alpha channel; ignore
    ///    `hbmMask`. ~99% of cursors on modern desktops.
    /// 2. **Legacy color+mask cursor** (e.g. some app I-beams, the
    ///    classic Win11 system I-beam): `hbmColor` is 32-bit BGRA with
    ///    alpha=0 everywhere — the alpha lives in `hbmMask` (1bpp AND
    ///    mask: 0=opaque cursor body, 1=transparent). The colour
    ///    bitmap alone renders as fully transparent → invisible
    ///    cursor. **Field repro the field-test host rc.7+rc.8: I-beam invisible
    ///    over Notepad++** — both monochrome (rc.7) and color+mask
    ///    (rc.8) paths have to be right.
    /// 3. **Pure monochrome cursor** (legacy / classic system I-beam
    ///    on older Windows): `hbmColor` is null; `hbmMask` holds AND
    ///    + XOR stacked vertically (height = 2 × cursor_height).
    unsafe fn extract_shape(hcursor: HCURSOR) -> Option<CursorInfo> {
        unsafe {
            let mut icon_info = ICONINFO::default();
            if GetIconInfo(HICON(hcursor.0), &mut icon_info).is_err() {
                return None;
            }
            let result = if !icon_info.hbmColor.is_invalid() {
                extract_color_cursor(&icon_info)
            } else if !icon_info.hbmMask.is_invalid() {
                extract_mono_cursor(&icon_info)
            } else {
                None
            };
            cleanup_icon_info(&icon_info);
            result
        }
    }

    /// Extract a 32-bit colour cursor. Detects the legacy "alpha=0
    /// everywhere + separate AND mask" variant and applies the mask
    /// as alpha so cursors like the Win11 system I-beam render
    /// visibly instead of as a transparent void.
    unsafe fn extract_color_cursor(icon_info: &ICONINFO) -> Option<CursorInfo> {
        unsafe {
            let bmp_handle = icon_info.hbmColor;
            let mut bmp = BITMAP::default();
            if GetObjectW(
                HGDIOBJ(bmp_handle.0),
                size_of::<BITMAP>() as i32,
                Some(&mut bmp as *mut BITMAP as *mut _),
            ) == 0
            {
                return None;
            }
            let width = bmp.bmWidth.max(0) as u32;
            let height = bmp.bmHeight.max(0) as u32;
            if width == 0 || height == 0 {
                return None;
            }
            let mut bgra = read_dib_bgra(bmp_handle, width, height)?;

            // Detect "all alpha is zero" — the giveaway for a legacy
            // color+mask cursor. Modern alpha cursors paint at least
            // a few pixels with non-zero alpha (the body of the
            // cursor); legacy ones leave alpha=0 and rely on
            // `hbmMask` to define visibility. Skip the cheap check
            // when no mask is available — there's nothing to fall
            // back to.
            let any_alpha = bgra.chunks_exact(4).any(|p| p[3] != 0);
            if !any_alpha
                && !icon_info.hbmMask.is_invalid()
                && let Some(mask_alpha) = read_mask_as_alpha(icon_info.hbmMask, width, height)
            {
                // Apply mask: AND-mask=0 means opaque (cursor
                // body), AND-mask≠0 means transparent.
                for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
                    if i < mask_alpha.len() {
                        px[3] = mask_alpha[i];
                    }
                }
                // If even after mask application every pixel is
                // still alpha=0 the cursor would render fully
                // transparent. That's a degenerate cursor (e.g. an
                // empty bitmap from a buggy app); fall through and
                // synthesise an outline so the controller still
                // sees a pointer indicator.
                let now_visible = bgra.chunks_exact(4).any(|p| p[3] != 0);
                if !now_visible {
                    return None;
                }
                // Many legacy color+mask cursors paint the cursor
                // body in pure black (RGB=0). After applying mask
                // alpha, those pixels are opaque-black which is
                // invisible on Notepad++ dark theme. Add a 1-pixel
                // white outline around the opaque region so the
                // cursor stays visible on any background.
                add_white_outline_to_opaque_black(&mut bgra, width, height);
            }

            Some(CursorInfo {
                width,
                height,
                hotspot_x: icon_info.xHotspot as i32,
                hotspot_y: icon_info.yHotspot as i32,
                bgra,
                css: None,
            })
        }
    }

    /// Extract a pure-monochrome cursor (no `hbmColor`). The mask
    /// holds AND + XOR halves stacked vertically; combined they
    /// give the classic black-outline + white-fill rendering.
    unsafe fn extract_mono_cursor(icon_info: &ICONINFO) -> Option<CursorInfo> {
        unsafe {
            let bmp_handle = icon_info.hbmMask;
            let mut bmp = BITMAP::default();
            if GetObjectW(
                HGDIOBJ(bmp_handle.0),
                size_of::<BITMAP>() as i32,
                Some(&mut bmp as *mut BITMAP as *mut _),
            ) == 0
            {
                return None;
            }
            let width = bmp.bmWidth.max(0) as u32;
            // Mono mask is AND on top + XOR on bottom; cursor height
            // is half the bitmap height.
            let height = (bmp.bmHeight.max(0) as u32) / 2;
            if width == 0 || height == 0 {
                return None;
            }
            let raw = read_dib_bgra(bmp_handle, width, height * 2)?;

            // Compose AND + XOR into final BGRA per the standard
            // monochrome cursor semantic:
            //   AND=1, XOR=0 → transparent
            //   AND=0, XOR=0 → opaque black
            //   AND=0, XOR=1 → opaque white
            //   AND=1, XOR=1 → invert
            //
            // A static frame can't XOR-invert against the live
            // background, so the invert region — which for the classic
            // I-beam IS the whole glyph — is rendered as opaque BLACK
            // (its body) rather than the old opaque WHITE that produced
            // a solid white "I". A white outline is added afterwards so
            // the black glyph stays visible on dark backgrounds too.
            // (System cursors also carry a `css` keyword now, so the
            // browser renders the native OS cursor for the common case
            // and only falls back to this bitmap for odd app cursors.)
            let pixel_count = (width * height) as usize;
            let mut out = vec![0u8; pixel_count * 4];
            let xor_offset = pixel_count * 4;
            for i in 0..pixel_count {
                let and_idx = i * 4;
                let xor_idx = xor_offset + i * 4;
                let and_bit = raw[and_idx] != 0;
                let xor_bit = raw[xor_idx] != 0;
                let out_idx = i * 4;
                match (and_bit, xor_bit) {
                    (true, false) => {
                        // Transparent. Already zeroed.
                    }
                    (false, false) | (true, true) => {
                        // Opaque black: the outline AND the invert body.
                        out[out_idx] = 0;
                        out[out_idx + 1] = 0;
                        out[out_idx + 2] = 0;
                        out[out_idx + 3] = 255;
                    }
                    (false, true) => {
                        // Opaque white fill.
                        out[out_idx] = 255;
                        out[out_idx + 1] = 255;
                        out[out_idx + 2] = 255;
                        out[out_idx + 3] = 255;
                    }
                }
            }
            // Keep the black glyph visible on dark backgrounds by
            // haloing it in white (reuses the colour-path helper).
            add_white_outline_to_opaque_black(&mut out, width, height);

            Some(CursorInfo {
                width,
                height,
                hotspot_x: icon_info.xHotspot as i32,
                hotspot_y: icon_info.yHotspot as i32,
                bgra: out,
                css: None,
            })
        }
    }

    /// `GetDIBits(hbm, BGR_RGB, 32bpp, top-down)` → owned BGRA buffer
    /// of `width × height` pixels. Returns `None` on any OS error.
    unsafe fn read_dib_bgra(
        bmp_handle: windows::Win32::Graphics::Gdi::HBITMAP,
        width: u32,
        height: u32,
    ) -> Option<Vec<u8>> {
        unsafe {
            let pixel_count = (width * height) as usize;
            let mut buf = vec![0u8; pixel_count * 4];
            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = width as i32;
            // Negative height → top-down (matches our row layout).
            bi.bmiHeader.biHeight = -(height as i32);
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;
            let hdc = GetDC(None);
            let read = GetDIBits(
                hdc,
                bmp_handle,
                0,
                height,
                Some(buf.as_mut_ptr() as *mut _),
                &mut bi,
                DIB_RGB_COLORS,
            );
            ReleaseDC(None, hdc);
            if read == 0 { None } else { Some(buf) }
        }
    }

    /// Read the AND mask of a color+mask cursor and convert it to a
    /// per-pixel alpha vector (length = width × height bytes). 0 in
    /// the mask = opaque (alpha=255), non-zero = transparent (alpha=0).
    unsafe fn read_mask_as_alpha(
        mask_handle: windows::Win32::Graphics::Gdi::HBITMAP,
        width: u32,
        height: u32,
    ) -> Option<Vec<u8>> {
        unsafe {
            let mut bmp = BITMAP::default();
            if GetObjectW(
                HGDIOBJ(mask_handle.0),
                size_of::<BITMAP>() as i32,
                Some(&mut bmp as *mut BITMAP as *mut _),
            ) == 0
            {
                return None;
            }
            // Color+mask cursors typically have a mask that's the
            // same dimensions as the color bitmap (no AND+XOR
            // stacking). But some legacy ones still ship the mask as
            // 2× height for compatibility with monochrome readers.
            // We only need the AND portion (top `height` rows).
            let mask_h = bmp.bmHeight.max(0) as u32;
            let read_h = mask_h.min(height);
            let raw = read_dib_bgra(mask_handle, width, read_h)?;
            let pixel_count = (width * height) as usize;
            let mut alpha = vec![0u8; pixel_count];
            // For pixels we couldn't read (read_h < height) fall back
            // to opaque so we don't accidentally hide cursor body.
            let read_pixels = (width * read_h) as usize;
            for i in 0..pixel_count {
                if i < read_pixels {
                    let and_pixel = raw[i * 4];
                    alpha[i] = if and_pixel == 0 { 255 } else { 0 };
                } else {
                    alpha[i] = 255;
                }
            }
            Some(alpha)
        }
    }

    /// Add a 1-pixel white outline around any opaque-black region in
    /// `bgra`. Mutates in-place. The outline lands on currently-
    /// transparent pixels adjacent to opaque-black ones, so it never
    /// overwrites cursor body pixels.
    ///
    /// Used to keep legacy color+mask cursors (which paint the body
    /// in pure black) visible on dark-themed apps. Without it, the
    /// I-beam over a Notepad++ dark editor renders as opaque black
    /// pixels on a near-black background — invisible.
    fn add_white_outline_to_opaque_black(bgra: &mut [u8], width: u32, height: u32) {
        let w = width as i32;
        let h = height as i32;
        let stride = (width * 4) as usize;
        // First, snapshot the alpha+RGB so we can read while writing.
        let snapshot = bgra.to_vec();
        let is_opaque_black = |x: i32, y: i32| -> bool {
            if x < 0 || y < 0 || x >= w || y >= h {
                return false;
            }
            let idx = (y as usize) * stride + (x as usize) * 4;
            // BGRA format. Alpha must be opaque AND BGR all zero.
            snapshot[idx] == 0
                && snapshot[idx + 1] == 0
                && snapshot[idx + 2] == 0
                && snapshot[idx + 3] >= 200
        };
        for y in 0..h {
            for x in 0..w {
                let idx = (y as usize) * stride + (x as usize) * 4;
                // Only paint over currently-transparent pixels.
                if snapshot[idx + 3] >= 64 {
                    continue;
                }
                // Adjacent to an opaque-black pixel?
                let adj = is_opaque_black(x - 1, y)
                    || is_opaque_black(x + 1, y)
                    || is_opaque_black(x, y - 1)
                    || is_opaque_black(x, y + 1)
                    || is_opaque_black(x - 1, y - 1)
                    || is_opaque_black(x + 1, y - 1)
                    || is_opaque_black(x - 1, y + 1)
                    || is_opaque_black(x + 1, y + 1);
                if adj {
                    bgra[idx] = 255;
                    bgra[idx + 1] = 255;
                    bgra[idx + 2] = 255;
                    bgra[idx + 3] = 255;
                }
            }
        }
    }

    /// GetIconInfo returns owned GDI bitmap handles; leaking them
    /// exhausts the process-wide GDI handle pool within ~10k polls.
    unsafe fn cleanup_icon_info(info: &ICONINFO) {
        unsafe {
            if !info.hbmMask.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
            }
            if !info.hbmColor.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
            }
        }
    }

    /// Fallback cursor bitmap when `extract_shape` can't decode the
    /// OS cursor (unusual mask bitmaps, some app-custom cursors, or
    /// any `GetDIBits` failure). A classic 11×17 Windows-style arrow
    /// with a 1-pixel black outline and white fill — always visible
    /// on dark and light backgrounds so the controller never "loses"
    /// the remote pointer.
    ///
    /// Pattern characters: `#` = black outline, `.` = white fill,
    /// space = transparent. Hotspot is (0, 0) — the arrow tip.
    fn synthetic_arrow() -> CursorInfo {
        const W: u32 = 11;
        const H: u32 = 17;
        // Each row is W chars. Painted top-to-bottom, left-to-right.
        const PATTERN: &[&[u8]] = &[
            b"#          ",
            b"##         ",
            b"#.#        ",
            b"#..#       ",
            b"#...#      ",
            b"#....#     ",
            b"#.....#    ",
            b"#......#   ",
            b"#.......#  ",
            b"#........# ",
            b"#.....#####",
            b"#..#..#    ",
            b"#.# #..#   ",
            b"##  #..#   ",
            b"#    #..#  ",
            b"     #..#  ",
            b"      ##   ",
        ];

        let mut bgra = vec![0u8; (W * H * 4) as usize];
        for (y, row) in PATTERN.iter().enumerate() {
            for (x, ch) in row.iter().enumerate() {
                let idx = (y * W as usize + x) * 4;
                match ch {
                    b'#' => {
                        // Black outline, fully opaque.
                        bgra[idx] = 0;
                        bgra[idx + 1] = 0;
                        bgra[idx + 2] = 0;
                        bgra[idx + 3] = 255;
                    }
                    b'.' => {
                        // White fill, fully opaque.
                        bgra[idx] = 255;
                        bgra[idx + 1] = 255;
                        bgra[idx + 2] = 255;
                        bgra[idx + 3] = 255;
                    }
                    _ => {
                        // Transparent (alpha 0). BGR stays 0.
                    }
                }
            }
        }

        CursorInfo {
            width: W,
            height: H,
            hotspot_x: 0,
            hotspot_y: 0,
            bgra,
            css: None,
        }
    }

    // Silence unused-import warnings from the HBITMAP re-export on
    // non-Windows builds.
    #[allow(dead_code)]
    fn _unused_hbitmap_import(_h: HBITMAP) {}

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn stock_cursors_map_to_css_keywords() {
            let map = load_system_cursors();
            // Arrow + I-beam resolve on any interactive desktop; the
            // values must be the CSS keywords the browser applies.
            assert!(!map.is_empty(), "no stock cursors loaded");
            assert!(map.values().any(|v| *v == "text"), "missing text");
            assert!(map.values().any(|v| *v == "default"), "missing default");
        }

        #[test]
        fn ibeam_handle_classifies_as_text() {
            let tracker = WindowsCursorTracker::new();
            let ibeam = unsafe { LoadCursorW(None, IDC_IBEAM) }.expect("load IDC_IBEAM");
            assert_eq!(tracker.classify(ibeam), Some("text"));
        }

        #[test]
        fn custom_handle_is_unclassified() {
            let tracker = WindowsCursorTracker::new();
            // A handle that is not one of the stock cursors → None, so
            // the browser uses the streamed bitmap.
            assert_eq!(tracker.classify(HCURSOR(std::ptr::null_mut())), None);
        }
    }
}

/// What one data-channel message carries to every peer: a webrtc-rs reader
/// reads each message into a 65 535-byte buffer (`DATA_CHANNEL_BUFFER_SIZE`
/// in the vendored `webrtc` crate), and the agent's SCTP stack refuses to
/// send one over 65 536 bytes (`ErrOutboundPacketTooLarge`). #4121 measured
/// both.
pub const PEER_MESSAGE_MAX: usize = 65_535;

/// #4207 — the most a `cursor:shape` message may serialize to. The fit
/// measures the whole serialized message, so the margin under
/// [`PEER_MESSAGE_MAX`] is not slack for an estimate: it keeps the message
/// off the limit's exact edge, and costs a scaled pointer about 1% of its
/// edge (an edge scales with the square root of the budget).
pub const SHAPE_MESSAGE_BUDGET: usize = 64_000;

const _: () = assert!(SHAPE_MESSAGE_BUDGET < PEER_MESSAGE_MAX);

/// #4207 — the `cursor:shape` message for one shape, as the pump in
/// `peer.rs` sends it on the `cursor` channel when the tracker reports a new
/// shape. It always fits one data-channel message.
///
/// A shape whose message would pass [`SHAPE_MESSAGE_BUDGET`] (a bitmap of
/// about 110×110 px or more: an enlarged accessibility pointer, a large
/// application cursor, a high DPI) goes out with its bitmap scaled down,
/// keeping its aspect, until the message fits. `w`, `h`, `hx`, `hy` and
/// `bgra` then all describe the scaled bitmap, so the viewer draws it as it
/// draws any other, only smaller than the host's pointer, and `css` is
/// kept. A shape that fits goes out exactly as before: the wire is
/// unchanged.
pub fn shape_message(shape_id: u64, shape: &CursorInfo) -> String {
    let as_is = shape_message_as_is(shape_id, shape);
    if as_is.len() <= SHAPE_MESSAGE_BUDGET || shape.width == 0 || shape.height == 0 {
        return as_is;
    }
    // Everything in the message but the bitmap's base64 stays (its numbers
    // only lose digits), and base64 spends 4 characters on 3 bytes, at 4
    // bytes a pixel: the pixels the rest of the budget holds.
    let rest = as_is.len().saturating_sub(shape.bgra.len().div_ceil(3) * 4);
    let pixels = SHAPE_MESSAGE_BUDGET.saturating_sub(rest) / 4 * 3 / 4;
    let (w, h) = (shape.width, shape.height);
    let f = (pixels as f64 / (f64::from(w) * f64::from(h))).sqrt();
    let edge = |n: u32| ((f64::from(n) * f).floor() as u32).clamp(1, n);
    let (mut sw, mut sh) = (edge(w), edge(h));
    loop {
        let msg = shape_message_as_is(shape_id, &scaled(shape, sw, sh));
        // The arithmetic fits on the first pass, except where an edge
        // clamps to one pixel (a pointer hundreds of times wider than
        // tall); shrinking both edges by an eighth ends it whatever the
        // shape.
        if msg.len() <= SHAPE_MESSAGE_BUDGET || (sw, sh) == (1, 1) {
            tracing::debug!(
                shape_id,
                w,
                h,
                scaled_w = sw,
                scaled_h = sh,
                bytes = msg.len(),
                "cursor:shape scaled down to fit one data-channel message (#4207)"
            );
            return msg;
        }
        let shrink = |n: u32| (n - n / 8).saturating_sub(1).max(1);
        (sw, sh) = (shrink(sw), shrink(sh));
    }
}

/// `shape` with its bitmap scaled down to `w`×`h`, the hotspot with it.
fn scaled(shape: &CursorInfo, w: u32, h: u32) -> CursorInfo {
    CursorInfo {
        width: w,
        height: h,
        hotspot_x: scaled_hotspot(shape.hotspot_x, shape.width, w),
        hotspot_y: scaled_hotspot(shape.hotspot_y, shape.height, h),
        bgra: area_average(&shape.bgra, (shape.width, shape.height), (w, h)),
        css: shape.css,
    }
}

/// A hotspot coordinate on an edge of `from` pixels, moved to the same
/// edge scaled to `to`: rounded, and kept inside the image.
fn scaled_hotspot(at: i32, from: u32, to: u32) -> i32 {
    let at = (f64::from(at) * f64::from(to) / f64::from(from)).round();
    at.clamp(0.0, f64::from(to - 1)) as i32
}

/// A top-down, straight-alpha BGRA bitmap of `from` (width, height) scaled
/// down to `to`: each output pixel is the mean of the source pixels it
/// covers, each weighted by the share of it that it fills (a box filter,
/// which does not alias on a downscale).
///
/// The mean is taken over PREMULTIPLIED colour, then divided back by the
/// mean alpha. A mean of straight colour would mix the colour of the
/// transparent pixels round a pointer (black, in most) into its edge, and
/// leave a dark fringe on every pointer drawn on a transparent ground.
fn area_average(src: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    let cols = coverage(from.0, to.0);
    let rows = coverage(from.1, to.1);
    let mut out = Vec::with_capacity(to.0 as usize * to.1 as usize * 4);
    for row in &rows {
        for col in &cols {
            // Premultiplied blue, green and red, then alpha.
            let mut sum = [0.0f64; 4];
            for &(y, wy) in row {
                for &(x, wx) in col {
                    let at = (y as usize * from.0 as usize + x as usize) * 4;
                    // A bitmap shorter than its size reads as transparent.
                    let Some(px) = src.get(at..at + 4) else {
                        continue;
                    };
                    let a = f64::from(px[3]) * wy * wx;
                    for (s, &c) in sum.iter_mut().zip(&px[..3]) {
                        *s += f64::from(c) * a;
                    }
                    sum[3] += a;
                }
            }
            let alpha = sum[3];
            if alpha > 0.0 {
                out.extend(
                    sum[..3]
                        .iter()
                        .map(|s| (s / alpha).round().min(255.0) as u8),
                );
            } else {
                out.extend_from_slice(&[0, 0, 0]);
            }
            out.push(alpha.round().min(255.0) as u8);
        }
    }
    out
}

/// For each of the `to` output pixels along an edge of `from` source
/// pixels, the source pixels it covers, each with the share of the output
/// pixel it fills (the shares of one output pixel sum to 1).
fn coverage(from: u32, to: u32) -> Vec<Vec<(u32, f64)>> {
    let step = f64::from(from) / f64::from(to);
    (0..to)
        .map(|i| {
            let (start, end) = (f64::from(i) * step, f64::from(i + 1) * step);
            let last = (end.ceil() as u32).min(from);
            (start.floor() as u32..last)
                .filter_map(|j| {
                    let share = (end.min(f64::from(j + 1)) - start.max(f64::from(j))) / step;
                    (share > 0.0).then_some((j, share))
                })
                .collect()
        })
        .collect()
}

/// The message with the bitmap as the host drew it.
fn shape_message_as_is(shape_id: u64, shape: &CursorInfo) -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    serde_json::json!({
        "t": "cursor:shape",
        "id": shape_id,
        "w": shape.width,
        "h": shape.height,
        "hx": shape.hotspot_x,
        "hy": shape.hotspot_y,
        "bgra": BASE64.encode(&shape.bgra),
        // Optional CSS `cursor` keyword for stock system cursors ("text",
        // "default", …); null for app-custom cursors. Lets the browser
        // render the viewer's native OS cursor instead of this bitmap.
        // Additive — old browsers ignore it.
        "css": shape.css,
    })
    .to_string()
}

#[cfg(test)]
mod shape_message_tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    /// What one data-channel message carries to every peer (#4121): a
    /// webrtc-rs reader reads each message into a 65 535-byte buffer, and
    /// the agent's SCTP stack refuses to send one over 65 536 bytes. The
    /// tests keep their own copy rather than trust the code's.
    const ONE_MESSAGE: usize = 65_535;

    /// Every pixel opaque, the colours varied across the image.
    fn opaque(w: u32, h: u32) -> CursorInfo {
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                bgra.extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8, 255]);
            }
        }
        CursorInfo {
            width: w,
            height: h,
            hotspot_x: 0,
            hotspot_y: 0,
            bgra,
            css: None,
        }
    }

    /// An opaque white disc on fully transparent black: the edge a resample
    /// must not darken.
    fn transparent_edged(w: u32, h: u32) -> CursorInfo {
        let (cx, cy) = (f64::from(w) / 2.0, f64::from(h) / 2.0);
        let r = f64::from(w.min(h)) / 2.0 - 2.0;
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let (dx, dy) = (f64::from(x) + 0.5 - cx, f64::from(y) + 0.5 - cy);
                let inside = dx * dx + dy * dy <= r * r;
                bgra.extend_from_slice(&if inside { [255; 4] } else { [0; 4] });
            }
        }
        CursorInfo {
            width: w,
            height: h,
            hotspot_x: 0,
            hotspot_y: 0,
            bgra,
            css: None,
        }
    }

    /// The message for `shape`, checked to be one `cursor:shape` that a peer
    /// takes in one message and whose bitmap is `w`×`h`×4 bytes, as the
    /// viewer reads it (`applyCursorShape` drops a shorter one).
    fn sent(shape: &CursorInfo, what: &str) -> (serde_json::Value, Vec<u8>) {
        let msg = shape_message(7, shape);
        assert!(
            msg.len() <= ONE_MESSAGE,
            "{what}: the message is {} bytes, over the {ONE_MESSAGE} a peer takes in one",
            msg.len()
        );
        let v: serde_json::Value = serde_json::from_str(&msg).expect("the message is JSON");
        assert_eq!(v["t"], "cursor:shape", "{what}");
        assert_eq!(v["id"], 7, "{what}");
        let (w, h) = (v["w"].as_u64().unwrap(), v["h"].as_u64().unwrap());
        assert!(w >= 1 && h >= 1, "{what}: {w}x{h}");
        let bgra = BASE64.decode(v["bgra"].as_str().unwrap()).unwrap();
        assert_eq!(
            bgra.len() as u64,
            w * h * 4,
            "{what}: bgra is not w*h*4 bytes"
        );
        (v, bgra)
    }

    #[test]
    fn a_128_px_pointer_fits_one_message() {
        sent(&opaque(128, 128), "opaque 128x128");
        sent(&transparent_edged(128, 128), "transparent-edged 128x128");
    }

    #[test]
    fn a_256_px_pointer_fits_one_message() {
        sent(&opaque(256, 256), "opaque 256x256");
        sent(&transparent_edged(256, 256), "transparent-edged 256x256");
    }

    #[test]
    fn a_64_px_pointer_is_sent_byte_for_byte_as_before() {
        for css in [None, Some("text")] {
            let mut shape = transparent_edged(64, 64);
            (shape.hotspot_x, shape.hotspot_y, shape.css) = (3, 5, css);
            // The pump's own build before #4207 (`peer.rs`, `6168d66fa`).
            let before = serde_json::to_string(&serde_json::json!({
                "t": "cursor:shape",
                "id": 42u64,
                "w": shape.width,
                "h": shape.height,
                "hx": shape.hotspot_x,
                "hy": shape.hotspot_y,
                "bgra": BASE64.encode(&shape.bgra),
                "css": shape.css,
            }))
            .unwrap();
            assert_eq!(shape_message(42, &shape), before, "css {css:?}");
            sent(&shape, "64x64");
        }
    }

    #[test]
    fn a_pointer_that_fits_is_sent_unchanged() {
        // 100×100 is 40 000 bytes of bitmap, 53 336 in base64: it fits.
        let mut shape = transparent_edged(100, 100);
        (shape.hotspot_x, shape.hotspot_y, shape.css) = (50, 7, Some("pointer"));
        let (v, bgra) = sent(&shape, "100x100");
        assert_eq!((v["w"].as_u64(), v["h"].as_u64()), (Some(100), Some(100)));
        assert_eq!((v["hx"].as_i64(), v["hy"].as_i64()), (Some(50), Some(7)));
        assert_eq!(v["css"], "pointer");
        assert_eq!(bgra, shape.bgra);
        assert_eq!(shape_message(7, &shape), shape_message_as_is(7, &shape));
    }

    #[test]
    fn a_scaled_pointer_keeps_its_aspect_and_its_hotspot_inside() {
        // The top-left stays put, the middle stays the middle, the last
        // pixel stays the last, and a hotspot off the image comes inside it.
        for (hx, hy) in [(0, 0), (128, 128), (255, 255), (300, -4)] {
            let mut shape = opaque(256, 256);
            (shape.hotspot_x, shape.hotspot_y) = (hx, hy);
            let (v, _) = sent(&shape, "256x256");
            let (w, h) = (v["w"].as_i64().unwrap(), v["h"].as_i64().unwrap());
            let (sx, sy) = (v["hx"].as_i64().unwrap(), v["hy"].as_i64().unwrap());
            assert!(w < 256 && w == h, "{w}x{h}");
            assert!(
                (0..w).contains(&sx) && (0..h).contains(&sy),
                "({hx}, {hy}) became ({sx}, {sy}), outside {w}x{h}"
            );
            match (hx, hy) {
                (0, 0) => assert_eq!((sx, sy), (0, 0)),
                (128, 128) => assert!(
                    (sx - w / 2).abs() <= 1 && (sy - h / 2).abs() <= 1,
                    "the middle became ({sx}, {sy}) in {w}x{h}"
                ),
                (255, 255) => assert_eq!((sx, sy), (w - 1, h - 1)),
                _ => assert_eq!((sx, sy), (w - 1, 0)),
            }
        }
        // A wide pointer stays four times as wide as it is tall.
        let (v, _) = sent(&opaque(512, 128), "512x128");
        let (w, h) = (v["w"].as_f64().unwrap(), v["h"].as_f64().unwrap());
        assert!(w < 512.0 && (w / h - 4.0).abs() < 0.1, "{w}x{h}");
    }

    #[test]
    fn the_css_keyword_survives_the_scale() {
        for css in [Some("text"), Some("nwse-resize"), None] {
            let mut shape = opaque(256, 256);
            shape.css = css;
            let (v, _) = sent(&shape, "256x256");
            assert!(v["w"].as_u64().unwrap() < 256, "the bitmap was scaled");
            assert_eq!(v["css"].as_str(), css);
        }
    }

    #[test]
    fn a_scaled_pointer_has_no_dark_fringe() {
        let (v, bgra) = sent(&transparent_edged(256, 256), "transparent-edged 256x256");
        let w = v["w"].as_u64().unwrap() as usize;
        assert!(w < 256, "the bitmap was scaled");
        // A mean of straight colour would turn the rim grey: white mixed
        // with the transparent black round it.
        let mut rim = 0;
        for px in bgra.chunks_exact(4).filter(|px| px[3] > 0) {
            assert_eq!(
                px[..3],
                [255, 255, 255],
                "a visible pixel is not white: {px:?}"
            );
            rim += usize::from(px[3] < 255);
        }
        assert!(rim > 0, "the disc's rim is partly covered");
        let middle = (w / 2 * w + w / 2) * 4;
        assert_eq!(
            (bgra[middle + 3], bgra[3]),
            (255, 0),
            "the middle opaque, a corner clear"
        );
    }

    #[test]
    fn the_fit_ends_on_any_size() {
        // Large, and hundreds of times wider than tall: there an edge clamps
        // to one pixel, so the first guess does not fit and the loop shrinks.
        for (w, h) in [(1024, 1024), (40_000, 1), (1, 40_000), (300, 70)] {
            sent(&opaque(w, h), &format!("{w}x{h}"));
        }
        // A bitmap shorter than its size says: the missing pixels are clear.
        let mut shape = opaque(256, 256);
        shape.bgra.truncate(100_000);
        let (_, bgra) = sent(&shape, "256x256 with 100 000 bytes");
        assert!(bgra.chunks_exact(4).any(|px| px[3] == 255));
        assert!(bgra.chunks_exact(4).any(|px| px[3] == 0));
    }
}

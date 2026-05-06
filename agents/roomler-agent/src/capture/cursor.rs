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
//! { "t": "cursor:shape", "id": u64, "w": u32, "h": u32,
//!                        "hx": i32, "hy": i32, "bgra": "<base64>" }
//! { "t": "cursor:hide" }
//! ```
//!
//! `id` is the hash of the cursor HCURSOR handle — constant for the
//! duration of a shape, changes on I-beam / resize / link click etc.
//! The browser maps `id` → `ImageBitmap` cache so it only decodes each
//! shape once per session.

use crate::capture::CursorInfo;

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
    /// consulted on Windows with the mf-encoder feature; non-Windows
    /// and signalling-only builds keep the field but never read it
    /// (the windows_impl `poll()` is cfg-gated). Allowed dead.
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
        #[cfg(not(all(target_os = "windows", feature = "mf-encoder")))]
        {
            // Linux + macOS: not yet wired. XFixesGetCursorImage +
            // NSCursor would slot in here. Returning None falls back
            // to the browser's synthetic-badge rendering.
            None
        }
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
        ICONINFO,
    };

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
    }

    impl WindowsCursorTracker {
        pub(super) fn new() -> Self {
            Self {
                shapes: HashMap::new(),
            }
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
                let shape = if let Some(cached) = self.shapes.get(&shape_id) {
                    Some(cached.clone())
                } else if let Some(info) = extract_shape(ci.hCursor) {
                    self.shapes.insert(shape_id, info.clone());
                    Some(info)
                } else {
                    // extract_shape failed for this HCURSOR (I-beam
                    // variants, custom app cursors, or bitmaps
                    // GetDIBits can't decode). Without a shape the
                    // browser hides the canvas overlay and the
                    // controller sees the cursor "disappear". Fall
                    // back to a hardcoded white-with-black-outline
                    // arrow so the position indicator is always
                    // visible — better UX than a vanishing cursor.
                    // Cache under this shape_id so we don't rebuild
                    // the fallback on every poll.
                    let fallback = synthetic_arrow();
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

    /// Decode a cursor's shape into an ARGB bitmap + hotspot. Returns
    /// None on any OS error — the caller keeps the cached entry or
    /// skips emitting a shape this poll.
    unsafe fn extract_shape(hcursor: HCURSOR) -> Option<CursorInfo> {
        unsafe {
            let mut icon_info = ICONINFO::default();
            // HCURSOR and HICON are typedef'd to the same HANDLE in
            // Win32 headers but windows-rs exposes them as distinct
            // newtypes; HICON(hcursor.0) re-wraps the raw pointer.
            if GetIconInfo(HICON(hcursor.0), &mut icon_info).is_err() {
                return None;
            }
            // Try the colour bitmap first (32-bit cursors) — e.g. the
            // modern Windows arrow. Fall back to the mask bitmap for
            // classic monochrome cursors where hbmColor is null.
            let (bmp_handle, is_monochrome) = if !icon_info.hbmColor.is_invalid() {
                (icon_info.hbmColor, false)
            } else if !icon_info.hbmMask.is_invalid() {
                (icon_info.hbmMask, true)
            } else {
                cleanup_icon_info(&icon_info);
                return None;
            };

            let mut bmp = BITMAP::default();
            let bmp_size = size_of::<BITMAP>() as i32;
            if GetObjectW(
                HGDIOBJ(bmp_handle.0),
                bmp_size,
                Some(&mut bmp as *mut BITMAP as *mut _),
            ) == 0
            {
                cleanup_icon_info(&icon_info);
                return None;
            }

            let width = bmp.bmWidth.max(0) as u32;
            // Monochrome mask bitmap holds AND+XOR stacked vertically,
            // so its real cursor height is bmHeight/2.
            let height = if is_monochrome {
                (bmp.bmHeight.max(0) as u32) / 2
            } else {
                bmp.bmHeight.max(0) as u32
            };
            if width == 0 || height == 0 {
                cleanup_icon_info(&icon_info);
                return None;
            }

            // For monochrome cursors we need BOTH halves of the mask
            // (AND + XOR) — only the AND mask paints the I-beam as a
            // pure black silhouette, invisible on dark backgrounds
            // (field repro PC50045 rc.7: cursor disappears over
            // Notepad++ text area). The combined mask gives black
            // outline + white fill, which the GUI Win32 cursors set
            // expects.
            let read_height = if is_monochrome { height * 2 } else { height };
            let read_pixels = (width * read_height) as usize;
            let mut raw = vec![0u8; read_pixels * 4];

            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = width as i32;
            // Negative height = top-down DIB. GetDIBits default is
            // bottom-up (matches DIB on-wire convention) which would
            // require us to flip before encoding; asking top-down
            // directly is simpler.
            bi.bmiHeader.biHeight = -(read_height as i32);
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;

            let hdc = GetDC(None);
            let read = GetDIBits(
                hdc,
                bmp_handle,
                0,
                read_height,
                Some(raw.as_mut_ptr() as *mut _),
                &mut bi,
                DIB_RGB_COLORS,
            );
            ReleaseDC(None, hdc);

            if read == 0 {
                cleanup_icon_info(&icon_info);
                return None;
            }

            // Compose the final BGRA buffer from the raw GetDIBits read.
            // For colour cursors `raw` already IS the final buffer;
            // for monochrome we have AND in the top half + XOR in the
            // bottom half, both as 32-bit white-or-black pixels. Map
            // each (AND, XOR) pair to the standard semantic:
            //
            //   AND=1, XOR=0 → transparent (most pixels)
            //   AND=0, XOR=0 → opaque black (cursor outline)
            //   AND=0, XOR=1 → opaque white (cursor fill)
            //   AND=1, XOR=1 → inverse — approximate as opaque white
            //                 (proper alpha-blend with screen needs the
            //                 destination pixel which the browser
            //                 doesn't have over the WebRTC frame)
            //
            // White-fill + black-outline survives both light and dark
            // backgrounds; the I-beam becomes visible everywhere.
            let bgra = if is_monochrome {
                let pixel_count = (width * height) as usize;
                let mut out = vec![0u8; pixel_count * 4];
                let xor_offset = pixel_count * 4;
                for i in 0..pixel_count {
                    let and_idx = i * 4;
                    let xor_idx = xor_offset + i * 4;
                    // GetDIBits gave us 32-bit pixels: 0xFFFFFFFF for
                    // white, 0x00000000 for black. Sample any channel.
                    let and_bit = raw[and_idx] != 0;
                    let xor_bit = raw[xor_idx] != 0;
                    let out_idx = i * 4;
                    match (and_bit, xor_bit) {
                        (true, false) => {
                            // Transparent. Already zeroed.
                        }
                        (false, false) => {
                            // Black outline.
                            out[out_idx] = 0;
                            out[out_idx + 1] = 0;
                            out[out_idx + 2] = 0;
                            out[out_idx + 3] = 255;
                        }
                        (false, true) | (true, true) => {
                            // White fill (or invert → approximate white).
                            out[out_idx] = 255;
                            out[out_idx + 1] = 255;
                            out[out_idx + 2] = 255;
                            out[out_idx + 3] = 255;
                        }
                    }
                }
                out
            } else {
                raw
            };

            let info = CursorInfo {
                width,
                height,
                hotspot_x: icon_info.xHotspot as i32,
                hotspot_y: icon_info.yHotspot as i32,
                bgra,
            };
            cleanup_icon_info(&icon_info);
            Some(info)
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
        }
    }

    // Silence unused-import warnings from the HBITMAP re-export on
    // non-Windows builds.
    #[allow(dead_code)]
    fn _unused_hbitmap_import(_h: HBITMAP) {}
}

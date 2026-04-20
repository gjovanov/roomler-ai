//! Windows implementation of the viewer-indicator overlay.
//!
//! Architecture:
//!
//! - A dedicated OS thread owns a layered, click-through, topmost
//!   window positioned over the primary monitor. The thread runs a
//!   standard Win32 message pump so the window gets WM_PAINT etc.
//! - The state (active sessions → controller names) lives in
//!   `Arc<Mutex<State>>` shared between the public handle and the
//!   window proc. Mutating the state issues `InvalidateRect` +
//!   `PostMessageW(WM_APP_REDRAW)` so the pump repaints on the
//!   correct thread.
//! - `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` is the
//!   load-bearing piece: it keeps the overlay out of any screen-
//!   capture source (our own WGC backend, DXGI duplication, BitBlt,
//!   third-party tools). Without it, the overlay would show up in
//!   the RTP stream and the two peers would see a recursive picture-
//!   frame of red borders.
//! - Transparency is done via a single COLORKEY — the whole window
//!   is painted with a background that the compositor treats as
//!   transparent, then we draw the actual visible pixels (the border
//!   + caption) in non-key colors on top.

#![cfg(all(target_os = "windows", feature = "viewer-indicator"))]

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::thread;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DT_CENTER, DT_SINGLELINE, DT_TOP, DeleteObject,
    DrawTextW, EndPaint, FW_BOLD, FillRect, HGDIOBJ, InvalidateRect, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, HMENU, HWND_TOPMOST,
    IDC_ARROW, LWA_COLORKEY, LoadCursorW, MSG, PostMessageW, PostQuitMessage, RegisterClassExW,
    SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    SetLayeredWindowAttributes, SetWindowDisplayAffinity, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TranslateMessage, WDA_EXCLUDEFROMCAPTURE, WM_APP, WM_DESTROY, WM_PAINT,
    WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

// Custom window messages. WM_APP + N is reserved for application use;
// Windows promises not to send anything in this range to our WndProc.
const WM_APP_REDRAW: u32 = WM_APP + 1;
const WM_APP_SHUTDOWN: u32 = WM_APP + 2;

/// Border thickness in pixels. 6 px is slim enough to not occlude real
/// window chrome, thick enough to be noticed from across a room.
const BORDER_PX: i32 = 6;
/// Magenta — the COLORKEY. Any pixel painted exactly this color is
/// rendered transparent by the layered-window compositor, so we paint
/// the window body magenta and draw the visible bits (red border, text)
/// in colors that don't collide.
const COLORKEY_RGB: u32 = 0x00FF00FF;
/// Red (0xFF3333) for the visible border.
const BORDER_RGB: u32 = 0x003333FF; // COLORREF is 0x00BBGGRR
/// Controller-name caption text color — white.
const TEXT_RGB: u32 = 0x00FFFFFF;
/// Height of the top caption band in pixels.
const CAPTION_H: i32 = 28;

#[derive(Default)]
struct State {
    /// Active sessions mapped to the controller's display name.
    sessions: HashMap<String, String>,
}

#[derive(Clone)]
pub(super) struct Inner {
    state: Arc<Mutex<State>>,
    hwnd: Arc<Mutex<Option<isize>>>, // raw HWND as isize for Send+Sync
    tx: std_mpsc::Sender<Cmd>,
}

enum Cmd {
    Redraw,
    Shutdown,
}

impl Inner {
    pub(super) fn new() -> Result<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let hwnd_cell: Arc<Mutex<Option<isize>>> = Arc::new(Mutex::new(None));
        let (tx, rx) = std_mpsc::channel::<Cmd>();

        let state_for_thread = state.clone();
        let hwnd_for_thread = hwnd_cell.clone();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();

        thread::Builder::new()
            .name("roomler-agent-indicator".into())
            .spawn(
                move || match run_pump(state_for_thread, hwnd_for_thread, rx) {
                    Ok(()) => {
                        let _ = ready_tx.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                },
            )
            .context("spawning viewer-indicator thread")?;

        // The pump writes the HWND into `hwnd_for_thread` before it
        // enters its GetMessage loop, so as soon as hwnd_cell is set we
        // know window creation succeeded. Spin briefly (up to 500 ms)
        // rather than blocking on ready_rx — ready_rx only fires on
        // *termination*.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            if hwnd_cell.lock().unwrap().is_some() {
                break;
            }
            // The pump may also have failed before ever setting the
            // handle — in which case ready_rx will have an Err for us.
            if let Ok(res) = ready_rx.try_recv() {
                return res.map(|_| Inner {
                    state,
                    hwnd: hwnd_cell,
                    tx,
                });
            }
            if std::time::Instant::now() >= deadline {
                return Err(anyhow!("viewer-indicator thread did not create window"));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        Ok(Inner {
            state,
            hwnd: hwnd_cell,
            tx,
        })
    }

    /// Construct a no-op inner — the handle keeps the same public
    /// surface but drops all show/hide calls. Used when window
    /// creation fails on the real Windows path so callers don't have
    /// to branch.
    pub(super) fn disabled() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            hwnd: Arc::new(Mutex::new(None)),
            tx: std_mpsc::channel::<Cmd>().0,
        }
    }

    pub(super) fn show(&self, session_id: String, controller_name: String) {
        {
            let mut s = self.state.lock().unwrap();
            s.sessions.insert(session_id, controller_name);
        }
        self.post_redraw();
    }

    pub(super) fn hide(&self, session_id: String) {
        {
            let mut s = self.state.lock().unwrap();
            s.sessions.remove(&session_id);
        }
        self.post_redraw();
    }

    fn post_redraw(&self) {
        // Decouple the Arc<Mutex<_>> hwnd read from the send so the
        // compiler doesn't borrow across the unsafe call.
        let hwnd_isize = match *self.hwnd.lock().unwrap() {
            Some(h) => h,
            None => return,
        };
        let hwnd = HWND(hwnd_isize as *mut c_void);
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_REDRAW, WPARAM(0), LPARAM(0));
        }
        // Also notify the command channel — redundant with the
        // PostMessage above, but keeps Drop semantics clean: dropping
        // the last Inner sends Shutdown and the pump exits.
        let _ = self.tx.send(Cmd::Redraw);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Only notify on the *last* Inner drop. Arc ref count of 1
        // (plus the sender we're about to use == 0 after this) means
        // we're it.
        if Arc::strong_count(&self.state) == 1 {
            let _ = self.tx.send(Cmd::Shutdown);
            let hwnd_isize = *self.hwnd.lock().unwrap();
            if let Some(h) = hwnd_isize {
                let hwnd = HWND(h as *mut c_void);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_SHUTDOWN, WPARAM(0), LPARAM(0));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Win32 pump (runs on a dedicated thread)

fn run_pump(
    state: Arc<Mutex<State>>,
    hwnd_out: Arc<Mutex<Option<isize>>>,
    _rx: std_mpsc::Receiver<Cmd>,
) -> Result<()> {
    unsafe {
        let hinstance = GetModuleHandleW(None).context("GetModuleHandleW")?;

        let class_name = w!("RoomlerIndicatorWClass");
        let magenta_brush = CreateSolidBrush(COLORREF(COLORKEY_RGB));

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: magenta_brush,
            hIcon: Default::default(),
            hIconSm: Default::default(),
            lpszClassName: class_name,
            lpszMenuName: PCWSTR::null(),
        };

        if RegisterClassExW(&wc) == 0 {
            // ERROR_CLASS_ALREADY_EXISTS is fine — the class survives
            // between pump restarts within the same process.
            let err = windows::Win32::Foundation::GetLastError();
            if err.0 != 1410 {
                // 1410 = ERROR_CLASS_ALREADY_EXISTS
                let _ = DeleteObject(HGDIOBJ(magenta_brush.0));
                return Err(anyhow!("RegisterClassExW failed: {:?}", err));
            }
        }

        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name,
            w!("Roomler viewer indicator"),
            WS_POPUP,
            0,
            0,
            screen_w,
            screen_h,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .context("CreateWindowExW")?;

        // Store the state pointer in GWLP_USERDATA so the WndProc can
        // reach it without globals.
        let state_ptr = Arc::into_raw(state.clone()) as isize;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr);

        // COLORKEY transparency — any magenta pixel in the window
        // body becomes see-through. LWA_COLORKEY bypasses the alpha
        // path entirely (which a pure-alpha layered window would have
        // needed UpdateLayeredWindow + a premultiplied DIB).
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(COLORKEY_RGB), 0, LWA_COLORKEY);

        // Critical: hide the overlay from every screen-capture path,
        // including our own WGC backend, so the viewer doesn't see
        // the indicator in the stream.
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);

        *hwnd_out.lock().unwrap() = Some(hwnd.0 as isize);

        // Start hidden; shown on first show() call via ShowWindow in
        // WM_APP_REDRAW.
        let _ = ShowWindow(hwnd, SW_HIDE);

        // Classic message pump.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Clean up.
        let _ = DestroyWindow(hwnd);
        let _ = DeleteObject(HGDIOBJ(magenta_brush.0));
        // Reclaim the Arc we leaked into GWLP_USERDATA.
        if state_ptr != 0 {
            let _ = Arc::from_raw(state_ptr as *const Mutex<State>);
        }
        *hwnd_out.lock().unwrap() = None;
        Ok(())
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_APP_REDRAW => unsafe {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Mutex<State>;
            let show = if state_ptr.is_null() {
                false
            } else {
                let s = (*state_ptr).lock().unwrap();
                !s.sessions.is_empty()
            };
            if show {
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
                );
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                let _ = InvalidateRect(hwnd, None, true);
            } else {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        },
        WM_APP_SHUTDOWN => unsafe {
            PostQuitMessage(0);
            LRESULT(0)
        },
        WM_PAINT => unsafe {
            paint(hwnd);
            LRESULT(0)
        },
        WM_DESTROY => unsafe {
            PostQuitMessage(0);
            LRESULT(0)
        },
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() {
            return;
        }

        // Fill entire client area with the COLORKEY (transparent).
        let magenta = CreateSolidBrush(COLORREF(COLORKEY_RGB));
        FillRect(hdc, &ps.rcPaint, magenta);
        let _ = DeleteObject(HGDIOBJ(magenta.0));

        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);

        // Four red rectangles forming the border.
        let red = CreateSolidBrush(COLORREF(BORDER_RGB));
        let top = RECT {
            left: 0,
            top: 0,
            right: screen_w,
            bottom: BORDER_PX,
        };
        let bottom = RECT {
            left: 0,
            top: screen_h - BORDER_PX,
            right: screen_w,
            bottom: screen_h,
        };
        let left = RECT {
            left: 0,
            top: 0,
            right: BORDER_PX,
            bottom: screen_h,
        };
        let right = RECT {
            left: screen_w - BORDER_PX,
            top: 0,
            right: screen_w,
            bottom: screen_h,
        };
        FillRect(hdc, &top, red);
        FillRect(hdc, &bottom, red);
        FillRect(hdc, &left, red);
        FillRect(hdc, &right, red);
        let _ = DeleteObject(HGDIOBJ(red.0));

        // Caption band: draw a small red pill centered at the top so
        // the controller-name text has a readable background rather
        // than showing through to whatever's below.
        let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Mutex<State>;
        if !state_ptr.is_null() {
            let state = (*state_ptr).lock().unwrap();
            if !state.sessions.is_empty() {
                let names: Vec<&str> = state.sessions.values().map(String::as_str).collect();
                let joined = names.join(", ");
                let caption = format!("Being viewed by: {joined}");

                let caption_band_w = 520.min(screen_w - BORDER_PX * 2 - 20);
                let band = RECT {
                    left: (screen_w - caption_band_w) / 2,
                    top: BORDER_PX,
                    right: (screen_w + caption_band_w) / 2,
                    bottom: BORDER_PX + CAPTION_H,
                };
                let band_brush = CreateSolidBrush(COLORREF(BORDER_RGB));
                FillRect(hdc, &band, band_brush);
                let _ = DeleteObject(HGDIOBJ(band_brush.0));

                // Draw caption text centered inside the band.
                let font = CreateFontW(
                    -14,
                    0,
                    0,
                    0,
                    FW_BOLD.0 as i32,
                    0,
                    0,
                    0,
                    0,
                    OUT_DEFAULT_PRECIS.0 as u32,
                    0,
                    0,
                    0,
                    w!("Segoe UI"),
                );
                let prev_font = SelectObject(hdc, HGDIOBJ(font.0));
                let _ = SetBkMode(hdc, TRANSPARENT);
                let _ = SetTextColor(hdc, COLORREF(TEXT_RGB));

                let wide: Vec<u16> = caption.encode_utf16().chain(std::iter::once(0)).collect();
                let mut text_rect = band;
                let _ = DrawTextW(
                    hdc,
                    &mut wide[..wide.len() - 1].to_vec(),
                    &mut text_rect,
                    DT_CENTER | DT_SINGLELINE | DT_TOP,
                );

                let _ = SelectObject(hdc, prev_font);
                let _ = DeleteObject(HGDIOBJ(font.0));
            }
        }

        let _ = EndPaint(hwnd, &ps);
    }
}

//! Windows implementation of the viewer-indicator overlay.
//!
//! Two topmost, capture-excluded windows on the primary monitor:
//!
//! - A **thin, click-through border** (`WS_EX_TRANSPARENT`) that is
//!   always visible while a session is active — the passive "someone is
//!   watching" cue (Parsec / Moonlight style).
//! - A small **interactive badge** carrying the viewer's initials, name
//!   and a **Disconnect** control. It stays hidden until the user parks
//!   the pointer at the top edge for ~1.2 s (RDP / RustDesk reveal), is
//!   **draggable** so the viewee can move it off something they need to
//!   read, and auto-hides shortly after the pointer leaves. Clicking
//!   Disconnect fires the session's `ObjectId` through the `KillSender`
//!   back to the signaling loop, which tears the session down.
//!
//! `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` is the
//! load-bearing piece on BOTH windows: DWM composites them on the local
//! screen but omits them from every capture path (our WGC backend, DXGI
//! duplication, BitBlt), so the overlay — including the Disconnect
//! button — never leaks into the RTP stream going back to the viewer.
//!
//! The border uses a single COLORKEY (magenta) for transparency; the
//! badge is an ordinary opaque popup.

#![cfg(all(target_os = "windows", feature = "viewer-indicator"))]

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::thread;
use std::time::Instant;

use bson::oid::ObjectId;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE,
    DT_VCENTER, DeleteObject, DrawTextW, Ellipse, EndPaint, FW_BOLD, FW_NORMAL, FillRect,
    FrameRect, GetStockObject, HBRUSH, HDC, HGDIOBJ, InvalidateRect, NULL_PEN, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect,
    HMENU, HTCAPTION, HWND_TOPMOST, IDC_ARROW, KillTimer, LWA_COLORKEY, LoadCursorW, MSG,
    PostMessageW, PostQuitMessage, RegisterClassExW, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SendMessageW,
    SetLayeredWindowAttributes, SetTimer, SetWindowDisplayAffinity, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TranslateMessage, WDA_EXCLUDEFROMCAPTURE, WM_APP, WM_DESTROY,
    WM_EXITSIZEMOVE, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_NCLBUTTONDOWN, WM_PAINT, WM_TIMER,
    WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::w;

use super::{KillSender, initials_of};

// Custom window messages. WM_APP + N is reserved for application use;
// Windows promises not to send anything in this range to our WndProc.
const WM_APP_REDRAW: u32 = WM_APP + 1;
const WM_APP_SHUTDOWN: u32 = WM_APP + 2;

/// Border thickness in pixels. Kept thin (2 px) — a visible-but-subtle
/// frame that doesn't occlude window chrome; the reveal-on-hover badge
/// carries the who/Disconnect detail.
const BORDER_PX: i32 = 2;
/// Magenta — the COLORKEY for the border window. Any pixel painted
/// exactly this color renders transparent.
const COLORKEY_RGB: u32 = 0x00FF00FF;
/// Red (0xFF3333) for the visible border, initials chip, Disconnect
/// button and badge frame. COLORREF is 0x00BBGGRR.
const BORDER_RGB: u32 = 0x003333FF;
/// White caption / button text.
const TEXT_RGB: u32 = 0x00FFFFFF;
/// Muted grey for the small "Being viewed by" label.
const LABEL_RGB: u32 = 0x00B0B0B0;
/// Dark badge background.
const BADGE_BG_RGB: u32 = 0x001E1E1E;

// Badge geometry (device pixels; DPI-naive, matching the border).
const BADGE_W: i32 = 300;
const BADGE_H: i32 = 48;
const BTN_W: i32 = 96;
const BTN_H: i32 = 30;
const BTN_MARGIN: i32 = 8;
const CHIP_MARGIN: i32 = 8;
const CHIP_DIA: i32 = 32;

// Reveal-on-hover tuning.
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 120;
/// Pointer within this many pixels of the top edge counts as "at top".
const TOP_ZONE_PX: i32 = 4;
/// Dwell at the top edge before the badge appears.
const REVEAL_DWELL_MS: u128 = 1200;
/// Grace period after the pointer leaves before the badge hides.
const HIDE_DELAY_MS: u128 = 2500;

/// Disconnect button rectangle in badge client coordinates. Fixed, so
/// both the painter and the hit-test agree without stashing state.
fn button_rect() -> RECT {
    let top = (BADGE_H - BTN_H) / 2;
    RECT {
        left: BADGE_W - BTN_W - BTN_MARGIN,
        top,
        right: BADGE_W - BTN_MARGIN,
        bottom: top + BTN_H,
    }
}

/// Half-open point-in-rect test (client coordinates).
fn pt_in(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

#[derive(Default)]
struct State {
    /// Active sessions: `session_id_hex → controller display name`.
    sessions: HashMap<String, String>,
}

#[derive(Clone)]
pub(super) struct Inner {
    state: Arc<Mutex<State>>,
    hwnd: Arc<Mutex<Option<isize>>>, // border HWND as isize for Send+Sync
    tx: std_mpsc::Sender<Cmd>,
}

enum Cmd {
    Redraw,
    Shutdown,
}

/// Everything the pump thread's two window procs need. Owned solely by
/// the pump thread (never sent across threads); a raw pointer to it is
/// stashed in BOTH windows' `GWLP_USERDATA`. Reclaimed once on teardown.
///
/// SOUNDNESS: window procs must not hold a `&mut PumpState` across a
/// Win32 call that can synchronously re-enter the pump (the drag's
/// `SendMessage(WM_NCLBUTTONDOWN)`, `ShowWindow`, `SetWindowPos`). The
/// code computes decisions inside a short borrow, drops it, then runs
/// the window ops through helpers that touch fields only momentarily.
struct PumpState {
    shared: Arc<Mutex<State>>,
    kill_tx: KillSender,
    badge_hwnd: HWND,
    badge_pos: (i32, i32),
    badge_visible: bool,
    dragging: bool,
    dwell_since: Option<Instant>,
    hide_since: Option<Instant>,
}

impl Inner {
    pub(super) fn new(kill_tx: KillSender) -> Result<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let hwnd_cell: Arc<Mutex<Option<isize>>> = Arc::new(Mutex::new(None));
        let (tx, rx) = std_mpsc::channel::<Cmd>();

        let state_for_thread = state.clone();
        let hwnd_for_thread = hwnd_cell.clone();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();

        thread::Builder::new()
            .name("roomler-agent-indicator".into())
            .spawn(
                move || match run_pump(state_for_thread, hwnd_for_thread, rx, kill_tx) {
                    Ok(()) => {
                        let _ = ready_tx.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                },
            )
            .context("spawning viewer-indicator thread")?;

        // The pump writes the border HWND into `hwnd_for_thread` before
        // it enters its GetMessage loop; spin briefly (up to 500 ms) for
        // it rather than blocking on ready_rx (which only fires on exit).
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            if hwnd_cell.lock().unwrap().is_some() {
                break;
            }
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

    /// Construct a no-op inner — same public surface, drops show/hide.
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
        let hwnd_isize = match *self.hwnd.lock().unwrap() {
            Some(h) => h,
            None => return,
        };
        let hwnd = HWND(hwnd_isize as *mut c_void);
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_REDRAW, WPARAM(0), LPARAM(0));
        }
        let _ = self.tx.send(Cmd::Redraw);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
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
    kill_tx: KillSender,
) -> Result<()> {
    unsafe {
        let hinstance = GetModuleHandleW(None).context("GetModuleHandleW")?;

        // Border class (colorkey, click-through).
        let border_class = w!("RoomlerIndicatorWClass");
        let magenta_brush = CreateSolidBrush(COLORREF(COLORKEY_RGB));
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(border_wnd_proc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: magenta_brush,
            lpszClassName: border_class,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            let err = windows::Win32::Foundation::GetLastError();
            if err.0 != 1410 {
                let _ = DeleteObject(HGDIOBJ(magenta_brush.0));
                return Err(anyhow!("RegisterClassExW(border) failed: {:?}", err));
            }
        }

        // Badge class (opaque, interactive).
        let badge_class = w!("RoomlerIndicatorBadgeWClass");
        let badge_wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(badge_wnd_proc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH::default(),
            lpszClassName: badge_class,
            ..Default::default()
        };
        if RegisterClassExW(&badge_wc) == 0 {
            let err = windows::Win32::Foundation::GetLastError();
            if err.0 != 1410 {
                let _ = DeleteObject(HGDIOBJ(magenta_brush.0));
                return Err(anyhow!("RegisterClassExW(badge) failed: {:?}", err));
            }
        }

        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);

        // Full-screen click-through border.
        let border_hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            border_class,
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
        .context("CreateWindowExW(border)")?;

        // Interactive badge, initially hidden, default top-centre.
        let badge_x = (screen_w - BADGE_W) / 2;
        let badge_y = BORDER_PX + 2;
        let badge_hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            badge_class,
            w!("Roomler viewer badge"),
            WS_POPUP,
            badge_x,
            badge_y,
            BADGE_W,
            BADGE_H,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .context("CreateWindowExW(badge)")?;

        // Colorkey transparency + capture exclusion on the border.
        let _ = SetLayeredWindowAttributes(border_hwnd, COLORREF(COLORKEY_RGB), 0, LWA_COLORKEY);
        let _ = SetWindowDisplayAffinity(border_hwnd, WDA_EXCLUDEFROMCAPTURE);
        // Capture exclusion on the badge too — its Disconnect button must
        // never appear in the RTP stream, and the viewer's injected input
        // can't intentionally target what it can't see.
        let _ = SetWindowDisplayAffinity(badge_hwnd, WDA_EXCLUDEFROMCAPTURE);

        // Build the shared pump state and hand a raw pointer to BOTH procs.
        let pump = Box::new(PumpState {
            shared: state,
            kill_tx,
            badge_hwnd,
            badge_pos: (badge_x, badge_y),
            badge_visible: false,
            dragging: false,
            dwell_since: None,
            hide_since: None,
        });
        let pump_ptr_val = Box::into_raw(pump) as isize;
        SetWindowLongPtrW(border_hwnd, GWLP_USERDATA, pump_ptr_val);
        SetWindowLongPtrW(badge_hwnd, GWLP_USERDATA, pump_ptr_val);

        *hwnd_out.lock().unwrap() = Some(border_hwnd.0 as isize);

        let _ = ShowWindow(border_hwnd, SW_HIDE);

        // Classic message pump — dispatches to both window procs.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Clean up.
        let _ = KillTimer(border_hwnd, TIMER_ID);
        let _ = DestroyWindow(badge_hwnd);
        let _ = DestroyWindow(border_hwnd);
        let _ = DeleteObject(HGDIOBJ(magenta_brush.0));
        if pump_ptr_val != 0 {
            drop(Box::from_raw(pump_ptr_val as *mut PumpState));
        }
        *hwnd_out.lock().unwrap() = None;
        Ok(())
    }
}

/// Read the pump-state pointer from a window's `GWLP_USERDATA`.
unsafe fn pump_ptr(hwnd: HWND) -> *mut PumpState {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PumpState }
}

/// True while ≥1 session is active.
unsafe fn has_session(p: *mut PumpState) -> bool {
    unsafe { !(*p).shared.lock().unwrap().sessions.is_empty() }
}

unsafe fn show_badge(p: *mut PumpState) {
    unsafe {
        let hwnd = (*p).badge_hwnd;
        let (x, y) = (*p).badge_pos;
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            BADGE_W,
            BADGE_H,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = InvalidateRect(hwnd, None, true);
        (*p).badge_visible = true;
        (*p).hide_since = None;
    }
}

unsafe fn hide_badge(p: *mut PumpState) {
    unsafe {
        let hwnd = (*p).badge_hwnd;
        let _ = ShowWindow(hwnd, SW_HIDE);
        (*p).badge_visible = false;
        (*p).hide_since = None;
        (*p).dwell_since = None;
    }
}

/// Fire the current session's teardown through the kill channel.
unsafe fn kill_current_session(p: *mut PumpState) {
    unsafe {
        let hex = {
            let guard = (*p).shared.lock().unwrap();
            guard.sessions.keys().next().cloned()
        };
        if let Some(hex) = hex
            && let Ok(oid) = ObjectId::parse_str(&hex)
        {
            let _ = (*p).kill_tx.try_send(oid);
        }
        hide_badge(p);
    }
}

// ---------------------------------------------------------------------------
// Border window proc: paints the thin frame + drives the reveal timer.

unsafe extern "system" fn border_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_APP_REDRAW => {
                let p = pump_ptr(hwnd);
                if p.is_null() {
                    return LRESULT(0);
                }
                if has_session(p) {
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
                    let _ = SetTimer(hwnd, TIMER_ID, TIMER_MS, None);
                    if (*p).badge_visible {
                        // Name may have changed; refresh the revealed badge.
                        let _ = InvalidateRect((*p).badge_hwnd, None, true);
                    }
                } else {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    let _ = KillTimer(hwnd, TIMER_ID);
                    hide_badge(p);
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_ID => {
                let p = pump_ptr(hwnd);
                if p.is_null() {
                    return LRESULT(0);
                }
                if !has_session(p) {
                    if (*p).badge_visible {
                        hide_badge(p);
                    }
                    return LRESULT(0);
                }
                // Decide inside a short borrow, then run window ops after
                // it is dropped (they can re-enter the pump).
                let (do_show, do_hide) = {
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let in_top = pt.y <= TOP_ZONE_PX;
                    let visible = (*p).badge_visible;
                    let (bx, by) = (*p).badge_pos;
                    let over_badge = visible
                        && pt.x >= bx
                        && pt.x < bx + BADGE_W
                        && pt.y >= by
                        && pt.y < by + BADGE_H;
                    let dragging = (*p).dragging;
                    let now = Instant::now();

                    let mut do_show = false;
                    if in_top {
                        match (*p).dwell_since {
                            None => (*p).dwell_since = Some(now),
                            Some(t) => {
                                if !visible && now.duration_since(t).as_millis() >= REVEAL_DWELL_MS
                                {
                                    do_show = true;
                                }
                            }
                        }
                    } else {
                        (*p).dwell_since = None;
                    }

                    let mut do_hide = false;
                    if visible && !in_top && !over_badge && !dragging {
                        match (*p).hide_since {
                            None => (*p).hide_since = Some(now),
                            Some(t) => {
                                if now.duration_since(t).as_millis() >= HIDE_DELAY_MS {
                                    do_hide = true;
                                }
                            }
                        }
                    } else {
                        (*p).hide_since = None;
                    }
                    (do_show, do_hide)
                };
                if do_show {
                    show_badge(p);
                }
                if do_hide {
                    hide_badge(p);
                }
                LRESULT(0)
            }
            WM_APP_SHUTDOWN => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_PAINT => {
                paint_border(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn paint_border(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() {
            return;
        }
        let magenta = CreateSolidBrush(COLORREF(COLORKEY_RGB));
        FillRect(hdc, &ps.rcPaint, magenta);
        let _ = DeleteObject(HGDIOBJ(magenta.0));

        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let red = CreateSolidBrush(COLORREF(BORDER_RGB));
        let bars = [
            RECT {
                left: 0,
                top: 0,
                right: screen_w,
                bottom: BORDER_PX,
            },
            RECT {
                left: 0,
                top: screen_h - BORDER_PX,
                right: screen_w,
                bottom: screen_h,
            },
            RECT {
                left: 0,
                top: 0,
                right: BORDER_PX,
                bottom: screen_h,
            },
            RECT {
                left: screen_w - BORDER_PX,
                top: 0,
                right: screen_w,
                bottom: screen_h,
            },
        ];
        for b in &bars {
            FillRect(hdc, b, red);
        }
        let _ = DeleteObject(HGDIOBJ(red.0));
        let _ = EndPaint(hwnd, &ps);
    }
}

// ---------------------------------------------------------------------------
// Badge window proc: paints the initials + name + Disconnect, handles
// dragging and the Disconnect click.

unsafe extern "system" fn badge_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                paint_badge(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let p = pump_ptr(hwnd);
                if p.is_null() {
                    return LRESULT(0);
                }
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let on_button = pt_in(&button_rect(), x, y);
                if on_button {
                    // Act on button-up so a press-then-drag-away cancels.
                    return LRESULT(0);
                }
                // Start a drag. Mark it, THEN SendMessage (which runs a
                // modal move loop that re-enters the pump) — no borrow held.
                (*p).dragging = true;
                let _ = ReleaseCapture();
                SendMessageW(
                    hwnd,
                    WM_NCLBUTTONDOWN,
                    WPARAM(HTCAPTION as usize),
                    LPARAM(0),
                );
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let p = pump_ptr(hwnd);
                if p.is_null() {
                    return LRESULT(0);
                }
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if pt_in(&button_rect(), x, y) {
                    kill_current_session(p);
                }
                LRESULT(0)
            }
            WM_EXITSIZEMOVE => {
                // Drag finished — remember the new position.
                let p = pump_ptr(hwnd);
                if !p.is_null() {
                    (*p).dragging = false;
                    let mut r = RECT::default();
                    if GetWindowRect(hwnd, &mut r).is_ok() {
                        (*p).badge_pos = (r.left, r.top);
                    }
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn paint_badge(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() {
            return;
        }

        let full = RECT {
            left: 0,
            top: 0,
            right: BADGE_W,
            bottom: BADGE_H,
        };

        // Background + 1px red frame.
        let bg = CreateSolidBrush(COLORREF(BADGE_BG_RGB));
        FillRect(hdc, &full, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));
        let red = CreateSolidBrush(COLORREF(BORDER_RGB));
        FrameRect(hdc, &full, red);

        // Initials chip: filled red circle (no outline).
        let old_brush = SelectObject(hdc, HGDIOBJ(red.0));
        let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
        let chip_top = (BADGE_H - CHIP_DIA) / 2;
        let _ = Ellipse(
            hdc,
            CHIP_MARGIN,
            chip_top,
            CHIP_MARGIN + CHIP_DIA,
            chip_top + CHIP_DIA,
        );
        let _ = SelectObject(hdc, old_pen);
        let _ = SelectObject(hdc, old_brush);

        // Read the current controller name (first/only session).
        let name = {
            let p = pump_ptr(hwnd);
            if p.is_null() {
                String::new()
            } else {
                (*p).shared
                    .lock()
                    .unwrap()
                    .sessions
                    .values()
                    .next()
                    .cloned()
                    .unwrap_or_default()
            }
        };
        let inits = initials_of(&name);

        let chip_rect = RECT {
            left: CHIP_MARGIN,
            top: chip_top,
            right: CHIP_MARGIN + CHIP_DIA,
            bottom: chip_top + CHIP_DIA,
        };
        draw_text(
            hdc,
            &inits,
            chip_rect,
            -14,
            FW_BOLD.0 as i32,
            TEXT_RGB,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );

        // Text column between the chip and the button.
        let text_left = CHIP_MARGIN + CHIP_DIA + 8;
        let text_right = button_rect().left - 8;
        draw_text(
            hdc,
            "Being viewed by",
            RECT {
                left: text_left,
                top: 6,
                right: text_right,
                bottom: 24,
            },
            -11,
            FW_NORMAL.0 as i32,
            LABEL_RGB,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
        draw_text(
            hdc,
            &name,
            RECT {
                left: text_left,
                top: 23,
                right: text_right,
                bottom: 43,
            },
            -15,
            FW_BOLD.0 as i32,
            TEXT_RGB,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );

        // Disconnect button.
        let btn = button_rect();
        FillRect(hdc, &btn, red);
        draw_text(
            hdc,
            "Disconnect",
            btn,
            -13,
            FW_BOLD.0 as i32,
            TEXT_RGB,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );

        let _ = DeleteObject(HGDIOBJ(red.0));
        let _ = EndPaint(hwnd, &ps);
    }
}

/// Draw a single run of text with a freshly-created Segoe UI font.
unsafe fn draw_text(
    hdc: HDC,
    text: &str,
    rect: RECT,
    height: i32,
    weight: i32,
    color: u32,
    format: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
) {
    unsafe {
        let font = CreateFontW(
            height,
            0,
            0,
            0,
            weight,
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
        let prev = SelectObject(hdc, HGDIOBJ(font.0));
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(color));
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut r = rect;
        if !wide.is_empty() {
            DrawTextW(hdc, &mut wide, &mut r, format);
        }
        let _ = SelectObject(hdc, prev);
        let _ = DeleteObject(HGDIOBJ(font.0));
    }
}

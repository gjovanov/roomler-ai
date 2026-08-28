//! FR-27 — the two small always-on-top panels: the consent prompt, and the
//! "Being viewed by …" session banner.
//!
//! Why separate windows and not the main one: the main window is a 1100×740
//! SPA, and throwing that over whatever someone is doing to ask a single
//! yes/no is the wrong shape for a prompt. It is also the wrong shape for a
//! banner, which has to be visible *while* they keep working.
//!
//! Platform reality, stated plainly because it decides what these are for:
//!
//! - **Windows** has a native, capture-excluded overlay in the daemon
//!   (`indicator/win.rs`) and keeps it — `SetWindowDisplayAffinity` keeps it
//!   out of the video going back to the viewer, which a webview window only
//!   gets if we ask for it too (we do, below). The banner here is therefore
//!   opt-in on Windows and the default everywhere else.
//! - **macOS** honours `NSWindowSharingNone` for ScreenCaptureKit and
//!   `CGWindowListCreateImage`, but NOT for `CGDisplayStream` — which is what
//!   `capture/scrap_backend.rs` uses. ⚠️ The macOS banner is therefore expected
//!   to appear in the captured stream until capture moves to ScreenCaptureKit.
//!   Applied anyway: it costs nothing and becomes correct the day capture moves.
//! - **Linux/X11** has no equivalent at all. The banner appears in the stream.
//!
//! Both windows are created on demand and hidden rather than destroyed —
//! rebuilding a webview per prompt would put a visible delay in front of a
//! 30-second decision.

use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};

pub const CONSENT: &str = "consent";
pub const VIEWING: &str = "viewing";

/// Escape hatch, both ways. `ROOMLER_DESKTOP_BANNER=0` turns the banner off
/// where the native overlay already covers it; `=1` forces it on for an A/B
/// against that overlay.
///
/// Default: ON everywhere except Windows, which has the better one already.
pub fn banner_enabled() -> bool {
    match std::env::var("ROOMLER_DESKTOP_BANNER").ok().as_deref() {
        Some("0") | Some("false") | Some("off") => false,
        Some(_) => true,
        None => !cfg!(windows),
    }
}

/// Bring the consent prompt up. Idempotent — a second pending request while
/// one is already shown just re-focuses; the page renders whatever the daemon
/// currently lists.
pub fn show_consent<R: Runtime>(app: &AppHandle<R>) {
    show(
        app,
        CONSENT,
        "panel-consent.html",
        "Roomler — permission needed",
        460.0,
        260.0,
        // Focused: this one is a question, and a prompt nobody's keyboard can
        // reach is the failure mode we are here to fix.
        true,
    );
}

/// Bring the session banner up. Never focused and never in the taskbar — it is
/// a status indicator beside whatever the person is actually doing.
pub fn show_banner<R: Runtime>(app: &AppHandle<R>) {
    show(
        app,
        VIEWING,
        "panel-viewing.html",
        "Roomler — session active",
        360.0,
        76.0,
        false,
    );
}

pub fn hide<R: Runtime>(app: &AppHandle<R>, label: &str) {
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.hide();
    }
}

#[allow(clippy::too_many_arguments)]
fn show<R: Runtime>(
    app: &AppHandle<R>,
    label: &str,
    page: &str,
    title: &str,
    w: f64,
    h: f64,
    focus: bool,
) {
    if let Some(win) = app.get_webview_window(label) {
        let _ = win.show();
        if focus {
            let _ = win.set_focus();
        }
        return;
    }
    let built = WebviewWindowBuilder::new(app, label, WebviewUrl::App(page.into()))
        .title(title)
        .inner_size(w, h)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(focus)
        .visible(false)
        .build();
    let win = match built {
        Ok(win) => win,
        Err(e) => {
            // Loud: the fallback for a failed panel is the in-window modal the
            // SPA still carries, and a silent failure here would look exactly
            // like "consent does not work" — the bug this FR exists to close.
            tracing::error!(%label, %e, "could not create the panel window — falling back to the main window's modal");
            if let Some(main) = app.get_webview_window("main") {
                let _ = main.show();
                let _ = main.set_focus();
            }
            return;
        }
    };
    exclude_from_capture(&win);
    position_top_centre(&win, w);
    let _ = win.show();
    if focus {
        let _ = win.set_focus();
    }
}

/// Top-centre of the primary monitor: out of the way of most content, and the
/// place a notification is expected on all three platforms. Best-effort — a
/// failure just leaves the window wherever the OS put it.
fn position_top_centre<R: Runtime>(win: &tauri::WebviewWindow<R>, logical_w: f64) {
    let Ok(Some(monitor)) = win.primary_monitor() else {
        return;
    };
    let scale = monitor.scale_factor();
    let size = monitor.size().to_logical::<f64>(scale);
    let pos = monitor.position().to_logical::<f64>(scale);
    let x = pos.x + (size.width - logical_w) / 2.0;
    let y = pos.y + 48.0;
    let _ = win.set_position(tauri::LogicalPosition::new(x, y));
}

/// Ask the OS to keep this window out of screen capture.
///
/// Windows honours it outright. macOS honours it for ScreenCaptureKit and
/// window-list captures but NOT for `CGDisplayStream`, which is what our
/// capture backend uses today — so the call is correct and currently
/// ineffective there; see the module docs. X11 has nothing to ask.
#[cfg(windows)]
fn exclude_from_capture<R: Runtime>(win: &tauri::WebviewWindow<R>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
    };
    if let Ok(handle) = win.hwnd() {
        // SAFETY: `handle` is a live HWND owned by this window for the
        // duration of the call; the flag is a documented constant.
        unsafe {
            SetWindowDisplayAffinity(handle.0 as _, WDA_EXCLUDEFROMCAPTURE);
        }
    }
}

#[cfg(not(windows))]
fn exclude_from_capture<R: Runtime>(_win: &tauri::WebviewWindow<R>) {}

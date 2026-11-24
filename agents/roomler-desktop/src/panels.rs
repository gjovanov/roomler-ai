// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};

pub const CONSENT: &str = "consent";
pub const VIEWING: &str = "viewing";

/// The banner window's title. #2286's KWin script finds the banner by it.
pub const BANNER_TITLE: &str = "Roomler — session active";

// ── FR-27 P9: the banner's Windows manners, here ──────────────────────────
//
// The native Windows badge (`roomlerd/src/indicator/win.rs`) hides until the
// pointer rests at the top edge, can be dragged, and leaves a thin red frame
// round the screen for the whole session. This banner showed and STAYED, and
// on macOS every show took keyboard focus from whatever the person was using.
// The numbers below are the Windows badge's, so the two behave alike.

/// The pointer within this many logical pixels of the top edge is "at the top".
const TOP_ZONE: f64 = 4.0;
/// How long the pointer rests at the top before the banner comes back.
const REVEAL_DWELL: Duration = Duration::from_millis(1200);
/// How long the banner stays once the pointer has left it.
const HIDE_DELAY: Duration = Duration::from_millis(2500);
/// How long a session's banner stays the first time, pointer or not: the
/// person must SEE that a session started before it tucks itself away.
const FIRST_SHOW: Duration = Duration::from_millis(4000);
/// The watch's tick: the Windows badge's timer.
const TICK: Duration = Duration::from_millis(120);

/// A session wants the banner (the session watch in `main.rs` says so).
static BANNER_WANTED: AtomicBool = AtomicBool::new(false);
/// #2815 — the consent prompt window could not be created, so the main
/// window's modal must ask instead. Cleared the moment the window exists.
static CONSENT_PANEL_FAILED: AtomicBool = AtomicBool::new(false);
/// The banner was just brought up for a new session: the reveal watch starts
/// from "visible, shown now".
static BANNER_FRESH: AtomicBool = AtomicBool::new(false);
static REVEAL_WATCH: Mutex<bool> = Mutex::new(false);

/// `=0` (or `false`/`off`) turns a P9 behaviour off — each is its own kill
/// switch: `ROOMLER_DESKTOP_BANNER_AUTOHIDE` (the banner then stays, as
/// before P9) and `ROOMLER_DESKTOP_FRAME` (no red frame).
fn switched_off(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("0") | Some("false") | Some("off")
    )
}

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
        CONSENT_W,
        CONSENT_H,
        // Focused: this one is a question, and a prompt nobody's keyboard can
        // reach is the failure mode we are here to fix.
        true,
    );
}

/// #2815 — whether the main window's consent modal must render the prompts
/// the companion is asked to show. Only as the fallback the panel was always
/// meant to have: the prompt window could not be created. Both rendered
/// before, so each question came up twice. Until the window has been tried,
/// the modal waits; the watch in `main.rs` opens it within one tick.
pub fn consent_modal_needed() -> bool {
    CONSENT_PANEL_FAILED.load(Ordering::SeqCst)
}

/// Bring the session banner up. Never focused and never in the taskbar — it is
/// a status indicator beside whatever the person is actually doing.
///
/// FR-27 P9: from here the reveal watch owns it — it tucks itself away after
/// [`FIRST_SHOW`], comes back when the pointer rests at the top edge, and
/// stays while anything records. The red frame comes up with it (macOS) and
/// stays for the whole session.
pub fn show_banner<R: Runtime>(app: &AppHandle<R>) {
    show(
        app,
        VIEWING,
        "panel-viewing.html",
        BANNER_TITLE,
        360.0,
        76.0,
        false,
    );
    // #2286 — on KDE, KWin places it: at the top, above everything. Off this
    // thread, because it is a blocking D-Bus round trip.
    #[cfg(target_os = "linux")]
    std::thread::spawn(|| crate::pin::ensure_kwin_pin(BANNER_TITLE));
    BANNER_FRESH.store(true, Ordering::SeqCst);
    BANNER_WANTED.store(true, Ordering::SeqCst);
    if !switched_off("ROOMLER_DESKTOP_FRAME") {
        frame::show(app);
    }
    if !switched_off("ROOMLER_DESKTOP_BANNER_AUTOHIDE") {
        spawn_reveal_watch(app);
    }
}

pub fn hide<R: Runtime>(app: &AppHandle<R>, label: &str) {
    if label == VIEWING {
        // The session is over: the watch must not bring the banner back, and
        // the frame goes with it.
        BANNER_WANTED.store(false, Ordering::SeqCst);
        frame::hide(app);
    }
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.hide();
    }
}

/// Start the reveal watch once; it idles while no session wants the banner.
fn spawn_reveal_watch<R: Runtime>(app: &AppHandle<R>) {
    let mut started = match REVEAL_WATCH.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if *started {
        return;
    }
    let app = app.clone();
    if std::thread::Builder::new()
        .name("banner-reveal".into())
        .spawn(move || reveal_watch(app))
        .is_ok()
    {
        *started = true;
    }
}

fn reveal_watch<R: Runtime>(app: AppHandle<R>) {
    let mut state = Reveal::default();
    loop {
        std::thread::sleep(TICK);
        if !BANNER_WANTED.load(Ordering::SeqCst) {
            state = Reveal::default();
            continue;
        }
        let Some(win) = app.get_webview_window(VIEWING) else {
            continue;
        };
        let now = Instant::now();
        if BANNER_FRESH.swap(false, Ordering::SeqCst) {
            state = Reveal::shown(now);
        }
        match state.step(now, Sample::read(&win), crate::tray::recording_now()) {
            Some(true) => present(&win, false),
            Some(false) => {
                let _ = win.hide();
            }
            None => {}
        }
    }
}

/// One look at the pointer. `None` when it cannot be read (a Wayland session
/// keeps the global pointer to itself) — and then the banner simply stays,
/// because an indicator that hid with no way back would be worse than one that
/// never hides.
#[derive(Clone, Copy, Debug)]
struct Sample {
    /// Within [`TOP_ZONE`] of the primary screen's top edge.
    at_top: bool,
    /// Over the banner, or within a small margin of it (a drag in progress).
    over_banner: bool,
}

impl Sample {
    fn read<R: Runtime>(win: &tauri::WebviewWindow<R>) -> Option<Self> {
        // #2286 — under Xwayland the pointer is known only over X11 windows:
        // at GNOME's top bar it reads where it last was, so the top-edge
        // gesture would never bring a tucked-away banner back. Unreadable,
        // then, and the banner stays where it is pinned.
        #[cfg(target_os = "linux")]
        if crate::pin::under_xwayland() {
            return None;
        }
        let p = win.cursor_position().ok()?;
        let mon = win.primary_monitor().ok().flatten()?;
        let scale = mon.scale_factor();
        let (mx, my) = (f64::from(mon.position().x), f64::from(mon.position().y));
        let mw = f64::from(mon.size().width);
        let at_top = p.x >= mx && p.x < mx + mw && p.y <= my + TOP_ZONE * scale;
        let over_banner = match (win.outer_position(), win.outer_size()) {
            (Ok(o), Ok(sz)) => {
                let margin = 8.0 * scale;
                let (ox, oy) = (f64::from(o.x), f64::from(o.y));
                p.x >= ox - margin
                    && p.x <= ox + f64::from(sz.width) + margin
                    && p.y >= oy - margin
                    && p.y <= oy + f64::from(sz.height) + margin
            }
            _ => false,
        };
        Some(Self {
            at_top,
            over_banner,
        })
    }
}

/// The reveal watch's state machine: when to show and when to hide.
#[derive(Debug, Default)]
struct Reveal {
    visible: bool,
    shown_at: Option<Instant>,
    /// Since when the pointer has rested at the top (banner hidden).
    dwell_since: Option<Instant>,
    /// Since when the pointer has been away from the banner (banner shown).
    away_since: Option<Instant>,
}

impl Reveal {
    fn shown(now: Instant) -> Self {
        Self {
            visible: true,
            shown_at: Some(now),
            ..Self::default()
        }
    }

    /// One tick. `Some(true)` shows the banner, `Some(false)` hides it.
    fn step(&mut self, now: Instant, sample: Option<Sample>, recording: bool) -> Option<bool> {
        if !self.visible {
            // A recording brings it back at once and holds it: that is not
            // something to be told about only on request.
            let reveal = match sample {
                None => true,
                Some(_) if recording => true,
                Some(s) if s.at_top => {
                    let since = *self.dwell_since.get_or_insert(now);
                    now.duration_since(since) >= REVEAL_DWELL
                }
                Some(_) => {
                    self.dwell_since = None;
                    false
                }
            };
            if reveal {
                *self = Self::shown(now);
                return Some(true);
            }
            return None;
        }
        let first_show = self
            .shown_at
            .is_some_and(|t| now.duration_since(t) < FIRST_SHOW);
        let keep = recording || first_show || sample.is_none_or(|s| s.over_banner || s.at_top);
        if keep {
            self.away_since = None;
            return None;
        }
        let since = *self.away_since.get_or_insert(now);
        if now.duration_since(since) >= HIDE_DELAY {
            *self = Self::default();
            return Some(false);
        }
        None
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
        if label == CONSENT {
            CONSENT_PANEL_FAILED.store(false, Ordering::SeqCst);
            // #3987 — placed again only when it comes up from hidden; its
            // question comes to the front either way.
            if placed_on_show(win.is_visible().ok()) {
                place_consent(&win, w, h);
            }
            note_consent_raised();
        }
        present(&win, focus);
        return;
    }
    let built = WebviewWindowBuilder::new(app, label, WebviewUrl::App(page.into()))
        .title(title)
        .inner_size(w, h)
        // #2286 — and no bigger. On Linux a NON-resizable GTK window takes
        // its child's natural size, and a webview's is ~200 px: the banner
        // asked for 76 px and came out 198 px tall, the rest of it empty.
        // Resizable with min = max is the same fixed size without that.
        .min_inner_size(w, h)
        .max_inner_size(w, h)
        .resizable(cfg!(target_os = "linux"))
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(focus)
        // #1852, #3987 — a panel takes its first click, or that click is
        // spent making the window key and the button under it does nothing:
        // the banner's Disconnect needed two clicks (FR-27 P9 field check,
        // 2026-10-08), and so did the prompt's Approve once the person had
        // answered the daemon's panel (0.4.224 lab). macOS only.
        .accept_first_mouse(takes_first_click(label))
        .visible(false)
        .build();
    let win = match built {
        Ok(win) => {
            if label == CONSENT {
                CONSENT_PANEL_FAILED.store(false, Ordering::SeqCst);
            }
            win
        }
        Err(e) => {
            // Loud: the fallback for a failed panel is the in-window modal the
            // SPA still carries, and a silent failure here would look exactly
            // like "consent does not work" — the bug this FR exists to close.
            tracing::error!(%label, %e, "could not create the panel window — falling back to the main window's modal");
            if label == CONSENT {
                CONSENT_PANEL_FAILED.store(true, Ordering::SeqCst);
            }
            if let Some(main) = app.get_webview_window("main") {
                let _ = main.show();
                let _ = main.set_focus();
                if label == CONSENT {
                    // #3987 — the fallback modal's question came to the front.
                    note_consent_raised();
                }
            }
            return;
        }
    };
    exclude_from_capture(&win);
    // #2286, #3987 — the banner sits at the top. The consent prompt goes in
    // the middle, clear of the daemon's own panel, which holds the top centre
    // on every OS (`consent_origin`).
    if label == VIEWING {
        position_top_centre(&win, w);
    } else {
        place_consent(&win, w, h);
        note_consent_raised();
    }
    present(&win, focus);
}

/// #2286, #3987 — the consent prompt where [`consent_origin`] puts it on the
/// primary monitor, computed as the top centre is. (`WebviewWindow::center`
/// put the window's CORNER at the middle under Xwayland at 2×, half a window
/// off.)
fn position_consent<R: Runtime>(win: &tauri::WebviewWindow<R>, logical_w: f64, logical_h: f64) {
    let Ok(Some(monitor)) = win.primary_monitor() else {
        return;
    };
    let scale = monitor.scale_factor();
    let size = monitor.size().to_logical::<f64>(scale);
    let pos = monitor.position().to_logical::<f64>(scale);
    let screen = Area {
        x: pos.x,
        y: pos.y,
        w: size.width,
        h: size.height,
    };
    let (x, y) = consent_origin(screen, logical_w, logical_h, daemon_panel(screen));
    let _ = win.set_position(tauri::LogicalPosition::new(x, y));
}

/// Whether a panel's webview takes the FIRST click while its window is
/// inactive (`accept_first_mouse`). Both panels do, and neither lets a first
/// click grant anything without the app's arming. The banner is shown
/// without focus, so a click INTO it from another app must act (#1852), and
/// its buttons only end a session or a recording. The consent prompt does
/// since #3987: a first click spent on making it key also ate a deliberate
/// one seconds later, once the person had clicked anything else (the
/// daemon's own panel, in the 0.4.224 lab). Its guard against a click as it
/// appears is the app's arming ([`click_armed`]). Any other window keeps
/// AppKit's default.
fn takes_first_click(label: &str) -> bool {
    label == VIEWING || label == CONSENT
}

/// Show a panel. A focused one takes the keyboard (a question needs it); an
/// unfocused one must not.
fn present<R: Runtime>(win: &tauri::WebviewWindow<R>, focus: bool) {
    if focus {
        let _ = win.show();
        let _ = win.set_focus();
    } else {
        show_without_focus(win);
    }
}

/// ⚠️ macOS: `show()` makes the window KEY whatever `focused(false)` said at
/// build time, so every session's banner took key status from the window the
/// person was using, and their next click into it only made it key again — it
/// did nothing else (measured 2026-10-06: the first click into roomler-desktop
/// after each new session was lost). `orderFrontRegardless` shows it without
/// that, the way the native consent panel (`indicator/mac.rs`) shows itself.
#[cfg(target_os = "macos")]
fn show_without_focus<R: Runtime>(win: &tauri::WebviewWindow<R>) {
    let w = win.clone();
    let shown = win.run_on_main_thread(move || {
        if let Ok(ptr) = w.ns_window() {
            // SAFETY: `ns_window` is this live window's own `NSWindow`, and
            // this runs on the main thread, where AppKit must be called.
            let ns = unsafe { &*ptr.cast::<objc2_app_kit::NSWindow>() };
            ns.orderFrontRegardless();
        }
    });
    if shown.is_err() {
        let _ = win.show();
    }
}

#[cfg(not(target_os = "macos"))]
fn show_without_focus<R: Runtime>(win: &tauri::WebviewWindow<R>) {
    let _ = win.show();
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

/// macOS: `NSWindowSharingNone`. Correct and, for our `CGDisplayStream`
/// capture, currently ineffective (see the module docs) — set because it
/// costs nothing and becomes true the day capture moves to ScreenCaptureKit.
/// Until FR-27 P9 this was an empty function despite the docs above.
#[cfg(target_os = "macos")]
fn exclude_from_capture<R: Runtime>(win: &tauri::WebviewWindow<R>) {
    let w = win.clone();
    let _ = win.run_on_main_thread(move || {
        if let Ok(ptr) = w.ns_window() {
            // SAFETY: as in `show_without_focus`.
            let ns = unsafe { &*ptr.cast::<objc2_app_kit::NSWindow>() };
            ns.setSharingType(objc2_app_kit::NSWindowSharingType::None);
        }
    });
}

#[cfg(not(any(windows, target_os = "macos")))]
fn exclude_from_capture<R: Runtime>(_win: &tauri::WebviewWindow<R>) {}

/// FR-27 P9 — the red frame's thickness, in points: the native Windows
/// border's 2 px.
#[cfg(any(target_os = "macos", test))]
const FRAME_BORDER: f64 = 2.0;

/// FR-27 P9, #4376 — the red frame's four strips round a screen: top, bottom,
/// left and right, each `thickness` thick and flush with its own edge, the
/// top and bottom ones as wide as the screen and the sides as tall as it.
/// `screen` is the screen's frame in AppKit's global coordinates
/// (`NSScreen::frame`: its bottom-left corner, y up), and each strip comes
/// back as its window's frame in the same coordinates. A second display has
/// an origin of its own, and the strips start from it.
#[cfg(any(target_os = "macos", test))]
fn frame_strips(
    screen: roomler_node_core::consent_screen::Frame,
    thickness: f64,
) -> [roomler_node_core::consent_screen::Frame; 4] {
    use roomler_node_core::consent_screen::Frame;
    let Frame { x, y, w, h } = screen;
    [
        // y counts up from the bottom: the top strip ends at the top edge.
        Frame {
            x,
            y: y + h - thickness,
            w,
            h: thickness,
        },
        Frame {
            x,
            y,
            w,
            h: thickness,
        },
        Frame {
            x,
            y,
            w: thickness,
            h,
        },
        Frame {
            x: x + w - thickness,
            y,
            w: thickness,
            h,
        },
    ]
}

/// FR-27 P9 — the red frame round the screen while a session is up: the
/// native Windows border's 2 px, as four click-through strips (no layer, no
/// transparency: an opaque strip needs nothing beyond what `indicator/mac.rs`
/// already uses). ⚠️ It WILL appear in the captured stream, like the banner:
/// our macOS capture is `CGDisplayStream`, which ignores
/// `NSWindowSharingNone` (the operator's call, 2026-10-06: ship it anyway).
///
/// #4376 — it runs round the main screen as that screen is now. Each show
/// lays the strips out from it, and while the frame is up an observer lays
/// them out again at each change of a display's mode, the arrangement or the
/// set of displays. display-match (FR-108) switches the mode at a session's
/// start and hands it back later. Until #4376 the strips were laid out once a
/// process, at the frame's first show: one first shown during display-match
/// stayed at 400 × 300 pt in a corner after the hand-back, and for every
/// later session, until the companion restarted.
#[cfg(target_os = "macos")]
mod frame {
    use std::cell::RefCell;

    use objc2::rc::Retained;
    use objc2::runtime::NSObject;
    // `MainThreadOnly` provides `NSWindow::alloc` and `ScreenObserver::alloc`
    // — load-bearing import.
    use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    // `NSApplicationDidChangeScreenParametersNotification` is behind
    // objc2-app-kit's `NSApplication` feature, which tauri's window layer
    // (tao) turns on in this build; the Foundation half (the notification
    // centre) comes with this crate's own `NSScreen` and `NSWindow` features.
    use objc2_app_kit::{
        NSApplicationDidChangeScreenParametersNotification, NSBackingStoreType, NSColor, NSScreen,
        NSScreenSaverWindowLevel, NSWindow, NSWindowCollectionBehavior, NSWindowSharingType,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSNotification, NSNotificationCenter, NSPoint, NSRect, NSSize};
    use roomler_node_core::consent_screen::Frame;
    use tauri::{AppHandle, Runtime};

    use super::{FRAME_BORDER, frame_strips};

    /// The frame's state. Only the main thread touches it, so it lives in
    /// that thread's thread-local, for the companion process's whole life.
    struct State {
        /// The four strips: built at the first show that finds a screen,
        /// then kept and moved.
        strips: Vec<Retained<NSWindow>>,
        /// From a `show` to the next `hide`. The screen-change observer acts
        /// only while it is set.
        shown: bool,
        /// #4376 — the screen-change observer, registered at the first show
        /// and never let go. The notification centre does not retain its
        /// observers: this is the one strong reference.
        observer: Option<Retained<ScreenObserver>>,
    }

    thread_local! {
        static STATE: RefCell<State> = const {
            RefCell::new(State {
                strips: Vec::new(),
                shown: false,
                observer: None,
            })
        };
    }

    pub fn show<R: Runtime>(app: &AppHandle<R>) {
        let _ = app.run_on_main_thread(|| {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            STATE.with(|state| {
                let mut state = state.borrow_mut();
                state.shown = true;
                if state.observer.is_none() {
                    state.observer = Some(observe_screen_changes(mtm));
                }
                put_up(mtm, &mut state.strips);
            });
        });
    }

    pub fn hide<R: Runtime>(app: &AppHandle<R>) {
        let _ = app.run_on_main_thread(|| {
            STATE.with(|state| {
                let mut state = state.borrow_mut();
                // Before the strips go: from here the observer leaves them be.
                state.shown = false;
                for s in state.strips.iter() {
                    s.orderOut(None);
                }
            });
        });
    }

    /// #4376 — the strips round the main screen as it is now, ordered front:
    /// built the first time, moved every later time. With no screen to read
    /// they stay as they are, and the next screen change puts them up.
    fn put_up(mtm: MainThreadMarker, strips: &mut Vec<Retained<NSWindow>>) {
        let Some(screen) = NSScreen::mainScreen(mtm) else {
            return;
        };
        let f = screen.frame();
        let screen = Frame {
            x: f.origin.x,
            y: f.origin.y,
            w: f.size.width,
            h: f.size.height,
        };
        let rects = frame_strips(screen, FRAME_BORDER)
            .map(|r| NSRect::new(NSPoint::new(r.x, r.y), NSSize::new(r.w, r.h)));
        if strips.is_empty() {
            *strips = rects.into_iter().map(|r| strip(mtm, r)).collect();
        } else {
            for (s, r) in strips.iter().zip(rects) {
                s.setFrame_display(r, true);
            }
        }
        for s in strips.iter() {
            s.orderFrontRegardless();
        }
    }

    define_class!(
        // SAFETY: `NSObject` has no subclassing requirements, and this class
        // has no `Drop`.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        struct ScreenObserver;

        impl ScreenObserver {
            // SAFETY: the notification centre sends this selector with one
            // argument, the notification, and wants nothing back.
            #[unsafe(method(screenParametersChanged:))]
            fn screen_parameters_changed(&self, _notification: &NSNotification) {
                on_screen_change();
            }
        }
    );

    /// #4376 — registers the screen-change observer with the default
    /// notification centre. Called once a process, by the first show, on the
    /// main thread (`mtm`); `State::observer` keeps what it returns.
    fn observe_screen_changes(mtm: MainThreadMarker) -> Retained<ScreenObserver> {
        let this = ScreenObserver::alloc(mtm).set_ivars(());
        // SAFETY: `NSObject`'s `init`, on an instance just allocated.
        let observer: Retained<ScreenObserver> = unsafe { msg_send![super(this), init] };
        // SAFETY: `observer` answers `screenParametersChanged:` with the one
        // argument the centre sends (`define_class!` above). The centre holds
        // it without retaining it, and `State` keeps it alive for the
        // process's life. The name is AppKit's own constant, posted on the
        // main thread when a display's mode, the arrangement or the set of
        // displays changes.
        unsafe {
            NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
                &observer,
                sel!(screenParametersChanged:),
                Some(NSApplicationDidChangeScreenParametersNotification),
                None,
            );
        }
        observer
    }

    /// #4376 — a screen changed: while the frame is up, lay it out again
    /// round the main screen as it is now. Nothing here may panic, because
    /// it runs inside AppKit's call: the state is tried, never taken. Off the
    /// main thread (AppKit posts this on it) it does nothing, and the next
    /// show lays the frame out.
    fn on_screen_change() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let _ = STATE.try_with(|state| {
            // Held only by a show or a hide on this thread while they call
            // AppKit, and those lay the frame out, or take it down, themselves.
            let Ok(mut state) = state.try_borrow_mut() else {
                return;
            };
            if state.shown {
                put_up(mtm, &mut state.strips);
            }
        });
    }

    fn strip(mtm: MainThreadMarker, rect: NSRect) -> Retained<NSWindow> {
        // SAFETY: a plain borderless window on the main thread (`mtm`).
        let w = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: we hold the window (`Retained`) and never `close()` it;
        // AppKit must not ALSO release it if something ever does.
        unsafe { w.setReleasedWhenClosed(false) };
        // Above full-screen apps and every Space, like the consent panel.
        w.setLevel(NSScreenSaverWindowLevel);
        w.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        w.setSharingType(NSWindowSharingType::None);
        w.setOpaque(true);
        w.setHasShadow(false);
        w.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            1.0, 0.2, 0.2, 1.0,
        )));
        // Click-through: a frame that ate clicks along the screen edges would
        // break the Dock, the menu bar and every edge-snapped window.
        w.setIgnoresMouseEvents(true);
        w.setHidesOnDeactivate(false);
        w
    }
}

#[cfg(not(target_os = "macos"))]
mod frame {
    use tauri::{AppHandle, Runtime};

    pub fn show<R: Runtime>(_app: &AppHandle<R>) {}
    pub fn hide<R: Runtime>(_app: &AppHandle<R>) {}
}

// ── #3987: where the consent prompt opens, and a click as it appears ──────

/// The consent prompt's size, in logical units.
const CONSENT_W: f64 = 460.0;
const CONSENT_H: f64 = 260.0;

/// #3987 — a rectangle on the screen in logical units, its origin top-left.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Area {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Area {
    fn bottom(self) -> f64 {
        self.y + self.h
    }

    /// Whether the two overlap, or come within `gap` of each other.
    fn near(self, other: Area, gap: f64) -> bool {
        self.x < other.x + other.w + gap
            && other.x < self.x + self.w + gap
            && self.y < other.bottom() + gap
            && other.y < self.bottom() + gap
    }
}

/// #3987 — the box the daemon's own consent panel sits in, on every OS: the
/// top centre of the main screen, 48 down, at most 480 × 240 logical. macOS
/// draws 480 × 240 pt (`indicator/mac.rs`), X11 480 × 236 at the desktop's
/// scale (`indicator/x11.rs`), Windows 460 × 232 physical px
/// (`indicator/win.rs`), which is no larger in logical units.
const PANEL_TOP: f64 = 48.0;
const PANEL_W: f64 = 480.0;
const PANEL_H: f64 = 240.0;
/// #3987 — the room the prompt keeps from the daemon's panel.
const PANEL_GAP: f64 = 16.0;

fn daemon_panel(screen: Area) -> Area {
    Area {
        x: screen.x + (screen.w - PANEL_W) / 2.0,
        y: screen.y + PANEL_TOP,
        w: PANEL_W,
        h: PANEL_H,
    }
}

/// #3987 — where the consent prompt opens, on every OS: the middle of the
/// screen, unless that comes within [`PANEL_GAP`] of the daemon's panel; then
/// that far below it. On a screen too short for that, its bottom stays on the
/// screen, and its buttons with it.
///
/// Until #3987 it opened at the top centre, 48 down, on macOS and Windows,
/// where the daemon's panel is too. On a Mac that panel is at the screen-saver
/// level and covered the prompt but for a 20-pt strip; on Windows the prompt,
/// shown later and focused, covered the overlay. Linux had the middle since
/// #2286, which on a 1280 × 800 screen still meets the X11 panel.
fn consent_origin(screen: Area, w: f64, h: f64, panel: Area) -> (f64, f64) {
    let x = screen.x + (screen.w - w) / 2.0;
    let middle = screen.y + (screen.h - h) / 2.0;
    let y = if (Area { x, y: middle, w, h }).near(panel, PANEL_GAP) {
        panel.bottom() + PANEL_GAP
    } else {
        middle
    };
    (x, y.min(screen.bottom() - h).max(screen.y))
}

/// #3987 — whether showing the prompt places it. A prompt that was hidden is
/// placed again: GTK forgets a hidden window's position, and the window
/// manager would put it where it likes (on GNOME, #3973's top-left is a likely
/// case). One that is up stays where it is, where the person may have moved
/// it. Not known counts as hidden.
fn placed_on_show(visible: Option<bool>) -> bool {
    visible != Some(true)
}

/// #3987 — how long a question must be in front of the person before a click
/// on it is taken. A sooner click may have been aimed at what was under the
/// pointer before the question came up: one already on its way when it
/// appeared (a reaction takes about a quarter of a second), or the second
/// half of a double click (0.5 s by default on macOS and Windows). Nobody
/// reads a question and answers it that fast.
pub const CONSENT_ARMING: Duration = Duration::from_secs(1);

/// #3987 — when the app last showed a consent question's window or brought
/// it to the front: the prompt, or the main window when the prompt could not
/// be created (#2815).
static CONSENT_RAISED_AT: Mutex<Option<Instant>> = Mutex::new(None);

fn note_consent_raised() {
    *CONSENT_RAISED_AT.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
}

/// #3987 — how long ago, as of `now`, the app last showed or raised a
/// consent question's window; `None` before it ever did.
pub fn consent_raised_ago(now: Instant) -> Option<Duration> {
    CONSENT_RAISED_AT
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .map(|t| now.saturating_duration_since(t))
}

/// #3987 — how long the question a click answers has been in front of the
/// person: since the later of the app's last show or raise of its window
/// (`raised`) and the page's putting that question up (`question`). A new
/// question in a window that stays up is new to the person.
pub fn in_front_for(raised: Option<Duration>, question: Option<Duration>) -> Option<Duration> {
    match (raised, question) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// #3987 — whether a click is taken: its question has been in front of the
/// person for [`CONSENT_ARMING`] at least. Nothing known holds no click back.
pub fn click_armed(in_front: Option<Duration>) -> bool {
    in_front.is_none_or(|d| d >= CONSENT_ARMING)
}

// ── #3999: on a Mac, the screen the prompt opens on ───────────────────────

/// #3999 — the consent prompt on its screen, where [`consent_origin`] puts it
/// there. On a Mac that screen is the one holding the pointer, else the
/// primary: the rule the daemon's own panel follows
/// (`roomler_node_core::consent_screen`). While the pointer stays on one
/// screen, the two open there, and the prompt keeps clear of the panel on it.
/// Everywhere else, and on a Mac whose screens cannot be read, the primary
/// monitor, as before ([`position_consent`]).
fn place_consent<R: Runtime>(win: &tauri::WebviewWindow<R>, logical_w: f64, logical_h: f64) {
    // AppKit only on the main thread. The job runs before the show that
    // `present` queues after it: both reach the main thread in order.
    #[cfg(target_os = "macos")]
    {
        let w = win.clone();
        let queued = win.run_on_main_thread(move || {
            if !place_on_pointer_screen(&w, logical_w, logical_h) {
                position_consent(&w, logical_w, logical_h);
            }
        });
        if queued.is_ok() {
            return;
        }
    }
    position_consent(win, logical_w, logical_h);
}

/// #3999 — on the main thread: the pointer and the screens as AppKit gives
/// them, the screen [`roomler_node_core::consent_screen::choose`] picks, and
/// the prompt's window moved there through AppKit. Not Tauri's monitors and
/// positions: across screens of different scale, their pixels are not
/// AppKit's points. `false` when the window or a screen cannot be had.
#[cfg(target_os = "macos")]
fn place_on_pointer_screen<R: Runtime>(win: &tauri::WebviewWindow<R>, w: f64, h: f64) -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSEvent, NSScreen, NSWindow};
    use objc2_foundation::NSPoint;
    use roomler_node_core::consent_screen::{self, Frame, Point};

    let (Some(mtm), Ok(ptr)) = (MainThreadMarker::new(), win.ns_window()) else {
        return false;
    };
    let screens: Vec<Frame> = NSScreen::screens(mtm)
        .to_vec()
        .iter()
        .map(|s| {
            let f = s.frame();
            Frame {
                x: f.origin.x,
                y: f.origin.y,
                w: f.size.width,
                h: f.size.height,
            }
        })
        .collect();
    let p = NSEvent::mouseLocation();
    let chosen = consent_screen::choose(Point { x: p.x, y: p.y }, &screens);
    let Some(screen) = chosen.and_then(|i| screens.get(i).copied()) else {
        return false;
    };
    let (x, y) = appkit_origin(screen, w, h);
    // SAFETY: as in `show_without_focus`.
    let ns = unsafe { &*ptr.cast::<NSWindow>() };
    ns.setFrameOrigin(NSPoint::new(x, y));
    true
}

/// #3999 — where the prompt's window goes on a Mac's screen, given that
/// screen's frame in AppKit's coordinates (its bottom-left corner, y up), as
/// AppKit's frame origin: [`consent_origin`] on that screen, clear of where
/// [`daemon_panel`] says the daemon's panel is, worked out with y down and
/// turned back.
#[cfg(any(target_os = "macos", test))]
fn appkit_origin(screen: roomler_node_core::consent_screen::Frame, w: f64, h: f64) -> (f64, f64) {
    // AppKit's y negated is y down: the screen's top edge at -(y + h).
    let area = Area {
        x: screen.x,
        y: -(screen.y + screen.h),
        w: screen.w,
        h: screen.h,
    };
    let (x, top) = consent_origin(area, w, h, daemon_panel(area));
    // The prompt's top edge, y down, back to its bottom-left corner, y up.
    (x, -top - h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn away() -> Option<Sample> {
        Some(Sample {
            at_top: false,
            over_banner: false,
        })
    }

    fn top() -> Option<Sample> {
        Some(Sample {
            at_top: true,
            over_banner: false,
        })
    }

    fn over() -> Option<Sample> {
        Some(Sample {
            at_top: false,
            over_banner: true,
        })
    }

    #[test]
    fn a_new_sessions_banner_stays_its_first_show_then_tucks_away() {
        let t0 = Instant::now();
        let mut r = Reveal::shown(t0);
        // Pointer nowhere near it: still shown for the whole first show.
        assert_eq!(
            r.step(t0 + Duration::from_millis(3900), away(), false),
            None
        );
        // Then the hide delay runs from the first tick it is allowed to hide.
        assert_eq!(r.step(t0 + FIRST_SHOW, away(), false), None);
        assert_eq!(
            r.step(t0 + FIRST_SHOW + HIDE_DELAY, away(), false),
            Some(false)
        );
        assert!(!r.visible);
    }

    #[test]
    fn resting_at_the_top_brings_it_back_only_after_the_dwell() {
        let t0 = Instant::now();
        let mut r = Reveal::default();
        assert_eq!(r.step(t0, top(), false), None);
        assert_eq!(r.step(t0 + Duration::from_millis(1100), top(), false), None);
        assert_eq!(r.step(t0 + REVEAL_DWELL, top(), false), Some(true));
        assert!(r.visible);
    }

    #[test]
    fn leaving_the_top_resets_the_dwell() {
        let t0 = Instant::now();
        let mut r = Reveal::default();
        r.step(t0, top(), false);
        r.step(t0 + Duration::from_millis(1000), away(), false);
        // The rest starts again: 1.0 s more at the top is not enough.
        assert_eq!(r.step(t0 + Duration::from_millis(1100), top(), false), None);
        assert_eq!(r.step(t0 + Duration::from_millis(2100), top(), false), None);
        assert_eq!(
            r.step(
                t0 + Duration::from_millis(1100) + REVEAL_DWELL,
                top(),
                false
            ),
            Some(true)
        );
    }

    #[test]
    fn the_pointer_on_the_banner_keeps_it_however_long() {
        let t0 = Instant::now();
        let mut r = Reveal::shown(t0);
        for s in 0..60 {
            assert_eq!(r.step(t0 + Duration::from_secs(s), over(), false), None);
        }
        assert!(r.visible);
    }

    #[test]
    fn a_recording_shows_it_at_once_and_holds_it() {
        let t0 = Instant::now();
        let mut r = Reveal::default();
        assert_eq!(r.step(t0, away(), true), Some(true));
        for s in 1..60 {
            assert_eq!(r.step(t0 + Duration::from_secs(s), away(), true), None);
        }
        assert!(r.visible);
    }

    #[test]
    fn an_unreadable_pointer_never_hides_it() {
        // A Wayland session keeps the global pointer to itself: hiding with no
        // way to bring the banner back would leave the person with nothing.
        let t0 = Instant::now();
        let mut r = Reveal::shown(t0);
        for s in 0..60 {
            assert_eq!(r.step(t0 + Duration::from_secs(s), None, false), None);
        }
        let mut hidden = Reveal::default();
        assert_eq!(hidden.step(t0, None, false), Some(true));
    }

    /// #1852, #3987 — an inactive macOS window spends its first click on
    /// activation unless its view accepts first mouse. Both panels take it,
    /// and neither opens a click-through on something that grants without
    /// the app's arming: the banner's buttons only end a session or a
    /// recording, and the prompt's answers go through `answer_click`, which
    /// holds back a click that comes too soon (`commands.rs` tests). Any
    /// other window keeps AppKit's default. The builder needs a live Tauri
    /// runtime, so the decision is what this pins.
    #[test]
    fn every_panel_takes_its_first_click() {
        assert!(takes_first_click(VIEWING), "the banner");
        assert!(takes_first_click(CONSENT), "the consent prompt (#3987)");
        assert!(!takes_first_click("main"), "a window that is no panel");

        // The banner's page grants nothing: every command it calls reads the
        // sessions, or ends one or its recording.
        let banner = include_str!("front/panel-viewing.js");
        for call in banner.split("invoke('").skip(1) {
            let name = call.split('\'').next().unwrap_or_default();
            assert!(
                ["cmd_rc_sessions", "cmd_record_stop", "cmd_rc_disconnect"].contains(&name),
                "the banner calls {name}: a click into it from another app acts on the \
                 first click, so nothing on it may grant"
            );
        }
        // The prompt's page answers through the two consent commands alone.
        let prompt = include_str!("front/panel-consent.js");
        assert!(prompt.contains("invoke(approve ? 'cmd_consent_approve' : 'cmd_consent_deny'"));
        for call in prompt.split("invoke('").skip(1) {
            let name = call.split('\'').next().unwrap_or_default();
            assert_eq!(name, "cmd_get_pending_consents", "the prompt calls {name}");
        }
    }

    fn screen(w: f64, h: f64) -> Area {
        Area {
            x: 0.0,
            y: 0.0,
            w,
            h,
        }
    }

    fn prompt_on(screen: Area) -> Area {
        let (x, y) = consent_origin(screen, CONSENT_W, CONSENT_H, daemon_panel(screen));
        Area {
            x,
            y,
            w: CONSENT_W,
            h: CONSENT_H,
        }
    }

    /// #3987 — the 0.4.224 lab's Mac: a 1024-pt-wide main screen, the
    /// daemon's panel 480 × 240 at (272, 48) and the app's prompt 460 × 260 at
    /// (282, 48), under it but for a 20-pt strip. Even the middle of a
    /// 768-pt screen (y 254) meets the panel, so the prompt opens below it.
    #[test]
    fn the_prompt_opens_clear_of_the_daemons_panel_on_the_labs_mac() {
        let s = screen(1024.0, 768.0);
        assert_eq!(
            daemon_panel(s),
            Area {
                x: 272.0,
                y: 48.0,
                w: 480.0,
                h: 240.0
            },
            "the panel the lab read"
        );
        let p = prompt_on(s);
        assert_eq!((p.x, p.y), (282.0, 304.0));
        assert!(!p.near(daemon_panel(s), PANEL_GAP - 0.5), "{p:?}");
        assert!(p.bottom() <= s.bottom(), "{p:?}");
    }

    /// #2286, #3987 — where the middle of the screen is clear of the panel,
    /// the prompt opens there, as on Linux since #2286.
    #[test]
    fn the_prompt_opens_in_the_middle_where_that_is_clear_of_the_panel() {
        assert_eq!(
            prompt_on(screen(1920.0, 1080.0)),
            Area {
                x: 730.0,
                y: 410.0,
                w: CONSENT_W,
                h: CONSENT_H
            }
        );
        // A second monitor's coordinates: the same rule, from its corner.
        let side = Area {
            x: -1440.0,
            y: -100.0,
            w: 1440.0,
            h: 900.0,
        };
        let p = prompt_on(side);
        assert_eq!((p.x, p.y), (-1440.0 + 490.0, -100.0 + 320.0));
    }

    /// #3987 — a screen too short to hold the prompt below the panel keeps
    /// the prompt's bottom on it, and with it the row of buttons, which then
    /// stays clear of the panel.
    #[test]
    fn on_a_short_screen_the_prompts_buttons_stay_on_it() {
        let s = screen(1024.0, 500.0);
        let p = prompt_on(s);
        assert_eq!(p.bottom(), s.bottom(), "{p:?}");
        // The buttons' row: the bottom 48 of the prompt.
        let buttons = Area {
            y: p.bottom() - 48.0,
            h: 48.0,
            ..p
        };
        assert!(!buttons.near(daemon_panel(s), 0.0), "{p:?}");
    }

    /// #3987 — on every screen tall enough for both, the prompt keeps clear
    /// of the daemon's panel and stays on the screen.
    #[test]
    fn the_prompt_never_meets_the_panel_on_a_screen_tall_enough_for_both() {
        let tall_enough = PANEL_TOP + PANEL_H + PANEL_GAP + CONSENT_H;
        for (ox, oy) in [(0.0, 0.0), (-1280.0, 0.0), (1920.0, -360.0)] {
            for w in (800..=3840).step_by(64) {
                for h in (tall_enough as i32..=2160).step_by(4) {
                    let s = Area {
                        x: ox,
                        y: oy,
                        w: f64::from(w),
                        h: f64::from(h),
                    };
                    let p = prompt_on(s);
                    assert!(!p.near(daemon_panel(s), PANEL_GAP - 0.5), "{s:?}: {p:?}");
                    assert!(p.y >= s.y && p.bottom() <= s.bottom(), "{s:?}: {p:?}");
                    assert!(p.x >= s.x && p.x + p.w <= s.x + s.w, "{s:?}: {p:?}");
                }
            }
        }
    }

    /// The number a declaration in a daemon source file starts with.
    fn declared(src: &str, decl: &str) -> f64 {
        let at = src
            .find(decl)
            .unwrap_or_else(|| panic!("no `{decl}` in the daemon's panel"));
        src[at + decl.len()..]
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("`{decl}` is not a number"))
    }

    /// #3987 — the box the prompt keeps clear of holds the daemon's own
    /// consent panel on every OS: the top centre, 48 down, no wider or taller
    /// than the box. Read from the daemon's sources, so a panel that moves or
    /// grows fails here, not on a person's screen. (Windows draws in physical
    /// pixels, which are no larger in logical units.)
    #[test]
    fn the_daemons_panels_fit_the_box_the_prompt_keeps_clear_of() {
        let flat = |src: &str| src.split_whitespace().collect::<String>();
        let mac = include_str!("../../roomlerd/src/indicator/mac.rs");
        let x11 = include_str!("../../roomlerd/src/indicator/x11.rs");
        let win = include_str!("../../roomlerd/src/indicator/win.rs");
        let sizes = [
            (
                "macOS",
                declared(mac, "const W: f64 = "),
                declared(mac, "const H: f64 = "),
            ),
            (
                "X11",
                declared(x11, "const W: u16 = "),
                declared(x11, "const H: u16 = "),
            ),
            (
                "Windows",
                declared(win, "const CONSENT_W: i32 = "),
                declared(win, "const CONSENT_H: i32 = "),
            ),
        ];
        for (os, w, h) in sizes {
            assert!(w <= PANEL_W && h <= PANEL_H, "{os}: {w} × {h}");
        }
        // Each at the top centre, PANEL_TOP down.
        let top = PANEL_TOP.to_string();
        assert!(flat(mac).contains("frame.origin.x+(frame.size.width-W)/2.0"));
        assert!(flat(mac).contains(&format!("frame.origin.y+frame.size.height-H-{top}.0")));
        assert!(flat(x11).contains("(screen.width_in_pixelsasi32-(W*k)asi32)/2"));
        assert!(flat(x11).contains(&format!("({top}*k)asi16")));
        assert!(flat(win).contains(&format!("(screen_w-CONSENT_W)/2,{top},")));
    }

    /// #3987 — a prompt that was hidden is placed again when it comes back
    /// (GTK forgets a hidden window's position); one that is up stays where
    /// it is, where the person may have moved it.
    #[test]
    fn a_prompt_already_up_stays_where_it_is_and_a_hidden_one_is_placed_again() {
        assert!(!placed_on_show(Some(true)), "up: left where it is");
        assert!(placed_on_show(Some(false)), "hidden: placed again");
        assert!(placed_on_show(None), "not known: placed");
    }

    /// #1852, #3987 — a click is taken once its question has been in front
    /// of the person for the arming time; one sooner may have been aimed at
    /// what was under the pointer before the question came up.
    #[test]
    fn a_click_within_the_arming_time_is_not_taken() {
        assert_eq!(CONSENT_ARMING, Duration::from_secs(1));
        assert!(!click_armed(Some(Duration::ZERO)));
        assert!(!click_armed(Some(Duration::from_millis(300))));
        assert!(!click_armed(Some(Duration::from_millis(999))));
        assert!(click_armed(Some(CONSENT_ARMING)));
        assert!(click_armed(Some(Duration::from_secs(55))));
        assert!(click_armed(None), "nothing known holds no click back");
    }

    /// #3987 — what arms the prompt again: the app showing it or bringing it
    /// to the front, and a new question in a window that stays up, the one
    /// now under the person's pointer. The later of the two counts.
    #[test]
    fn a_new_question_under_the_pointer_arms_the_prompt_again() {
        let long = Some(Duration::from_secs(60));
        let just = Some(Duration::from_millis(300));
        // The window up for a minute; its question replaced 0.3 s ago.
        assert_eq!(in_front_for(long, just), just);
        assert!(!click_armed(in_front_for(long, just)));
        // The question shown for a minute; the window raised 0.3 s ago.
        assert_eq!(in_front_for(just, long), just);
        assert!(!click_armed(in_front_for(just, long)));
        // Both a minute ago: taken.
        assert!(click_armed(in_front_for(long, long)));
        // Only one known: that one counts.
        assert_eq!(in_front_for(None, just), just);
        assert_eq!(in_front_for(just, None), just);
    }

    /// #3999 — a frame in AppKit's coordinates, y up, as the y-down [`Area`]
    /// that [`Area::near`] reads: y negated.
    fn y_down(x: f64, y: f64, w: f64, h: f64) -> Area {
        Area {
            x,
            y: -(y + h),
            w,
            h,
        }
    }

    /// #3999 — on a Mac the prompt is placed through AppKit, on the screen
    /// `consent_screen::choose` picks. On a secondary with an origin of its
    /// own (right and lower, left and higher, above), it lies inside that
    /// screen and clear of the area the daemon's panel takes there, placed as
    /// `indicator/mac.rs` places it: the top centre, 48 down, in AppKit's y.
    #[test]
    fn on_a_secondary_with_its_own_origin_the_prompt_opens_on_it_clear_of_the_daemons_panel() {
        use roomler_node_core::consent_screen::Frame;
        let secondaries = [
            Frame {
                x: 1440.0,
                y: -200.0,
                w: 1920.0,
                h: 1080.0,
            },
            Frame {
                x: -1024.0,
                y: 300.0,
                w: 1024.0,
                h: 768.0,
            },
            Frame {
                x: -240.0,
                y: 900.0,
                w: 2560.0,
                h: 1440.0,
            },
        ];
        for s in secondaries {
            let (x, y) = appkit_origin(s, CONSENT_W, CONSENT_H);
            assert!(x >= s.x && x + CONSENT_W <= s.x + s.w, "{s:?}: ({x}, {y})");
            assert!(y >= s.y && y + CONSENT_H <= s.y + s.h, "{s:?}: ({x}, {y})");
            let panel = y_down(
                s.x + (s.w - PANEL_W) / 2.0,
                s.y + s.h - PANEL_H - PANEL_TOP,
                PANEL_W,
                PANEL_H,
            );
            let prompt = y_down(x, y, CONSENT_W, CONSENT_H);
            assert!(!prompt.near(panel, PANEL_GAP - 0.5), "{s:?}: ({x}, {y})");
        }
        // The 768-pt secondary meets the panel at its middle, like the lab's
        // Mac: 16 below the panel, its top edge 304 below the screen's.
        assert_eq!(
            appkit_origin(secondaries[1], CONSENT_W, CONSENT_H),
            (-1024.0 + 282.0, 300.0 + 768.0 - 304.0 - CONSENT_H)
        );
        // On the primary, the place the prompt had before #3999, in AppKit's
        // terms: (282, 304) from the top-left of a 1024 × 768 screen.
        let primary = Frame {
            x: 0.0,
            y: 0.0,
            w: 1024.0,
            h: 768.0,
        };
        assert_eq!(
            appkit_origin(primary, CONSENT_W, CONSENT_H),
            (282.0, 768.0 - 304.0 - CONSENT_H)
        );
    }

    /// #3999 — the daemon's panel and the app's prompt choose their screen by
    /// one rule: each reads the pointer and the screens from AppKit and asks
    /// `consent_screen::choose`, whose own tests are the rule. Read from the
    /// sources (the daemon's panel compiles only on a Mac), so a call site
    /// that goes back to the focused window's screen fails here.
    #[test]
    fn the_daemons_panel_and_the_prompt_choose_their_screen_by_one_rule() {
        let flat = |src: &str| src.split_whitespace().collect::<String>();
        let mac = flat(include_str!("../../roomlerd/src/indicator/mac.rs"));
        let app = include_str!("panels.rs");
        let app = flat(&app[..app.find("mod tests {").expect("the tests module")]);
        for (who, src) in [("the daemon's panel", &mac), ("the app's prompt", &app)] {
            for call in [
                "consent_screen::choose(",
                "NSEvent::mouseLocation()",
                "NSScreen::screens(mtm)",
            ] {
                assert!(src.contains(call), "{who} does not call {call}");
            }
        }
        assert!(
            !mac.contains("NSScreen::mainScreen"),
            "the daemon's panel: the focused window's screen"
        );
    }

    /// #4376 — a frame in AppKit's global coordinates, written short.
    fn at(x: f64, y: f64, w: f64, h: f64) -> roomler_node_core::consent_screen::Frame {
        roomler_node_core::consent_screen::Frame { x, y, w, h }
    }

    /// #4376 — the lab Mac's screen, 1024 × 768 pt at the origin, and the
    /// 400 × 300 pt screen display-match's 800 × 600 px mode makes of it at
    /// 2×: on each, the frame runs round the whole screen, 2 pt thick. In
    /// order: top, bottom, left, right; y counts up from the bottom edge.
    #[test]
    fn the_frame_runs_round_the_whole_of_a_screen_at_the_origin() {
        assert_eq!(FRAME_BORDER, 2.0, "the native Windows border's 2 px");
        assert_eq!(
            frame_strips(at(0.0, 0.0, 1024.0, 768.0), FRAME_BORDER),
            [
                at(0.0, 766.0, 1024.0, 2.0),
                at(0.0, 0.0, 1024.0, 2.0),
                at(0.0, 0.0, 2.0, 768.0),
                at(1022.0, 0.0, 2.0, 768.0),
            ]
        );
        assert_eq!(
            frame_strips(at(0.0, 0.0, 400.0, 300.0), FRAME_BORDER),
            [
                at(0.0, 298.0, 400.0, 2.0),
                at(0.0, 0.0, 400.0, 2.0),
                at(0.0, 0.0, 2.0, 300.0),
                at(398.0, 0.0, 2.0, 300.0),
            ]
        );
    }

    /// #4376 — a second display's frame has an origin of its own: right of
    /// the primary and lower, or left of it and higher. The frame runs round
    /// that screen from its own corner, not from the primary's.
    #[test]
    fn on_a_second_display_the_frame_runs_round_it_from_its_own_corner() {
        assert_eq!(
            frame_strips(at(1440.0, -200.0, 1920.0, 1080.0), FRAME_BORDER),
            [
                at(1440.0, 878.0, 1920.0, 2.0),
                at(1440.0, -200.0, 1920.0, 2.0),
                at(1440.0, -200.0, 2.0, 1080.0),
                at(3358.0, -200.0, 2.0, 1080.0),
            ]
        );
        assert_eq!(
            frame_strips(at(-1024.0, 300.0, 1024.0, 768.0), FRAME_BORDER),
            [
                at(-1024.0, 1066.0, 1024.0, 2.0),
                at(-1024.0, 300.0, 1024.0, 2.0),
                at(-1024.0, 300.0, 2.0, 768.0),
                at(-2.0, 300.0, 2.0, 768.0),
            ]
        );
    }

    /// #4376 — on every screen, at the origin or not, each strip is 2 pt
    /// thick, lies on the screen, sits flush with its own edge and runs that
    /// edge's whole length.
    #[test]
    fn every_strip_is_2_pt_thick_flush_with_its_edge_and_as_long_as_it() {
        for (ox, oy) in [
            (0.0, 0.0),
            (1440.0, -200.0),
            (-1024.0, 300.0),
            (-240.0, 900.0),
        ] {
            for w in (320..=3840).step_by(160) {
                for h in (240..=2160).step_by(120) {
                    let s = at(ox, oy, f64::from(w), f64::from(h));
                    let strips = frame_strips(s, FRAME_BORDER);
                    for r in strips {
                        assert!(r.x >= s.x && r.x + r.w <= s.x + s.w, "{s:?}: {r:?}");
                        assert!(r.y >= s.y && r.y + r.h <= s.y + s.h, "{s:?}: {r:?}");
                    }
                    let [top_strip, bottom_strip, left_strip, right_strip] = strips;
                    // Top and bottom: as wide as the screen, 2 pt tall.
                    assert_eq!(
                        (top_strip.x, top_strip.w, top_strip.h),
                        (s.x, s.w, FRAME_BORDER),
                        "{s:?}"
                    );
                    assert_eq!(top_strip.y + top_strip.h, s.y + s.h, "{s:?}: the top edge");
                    assert_eq!(
                        (bottom_strip.x, bottom_strip.w, bottom_strip.h),
                        (s.x, s.w, FRAME_BORDER),
                        "{s:?}"
                    );
                    assert_eq!(bottom_strip.y, s.y, "{s:?}: the bottom edge");
                    // Left and right: as tall as the screen, 2 pt wide.
                    assert_eq!(
                        (left_strip.y, left_strip.h, left_strip.w),
                        (s.y, s.h, FRAME_BORDER),
                        "{s:?}"
                    );
                    assert_eq!(left_strip.x, s.x, "{s:?}: the left edge");
                    assert_eq!(
                        (right_strip.y, right_strip.h, right_strip.w),
                        (s.y, s.h, FRAME_BORDER),
                        "{s:?}"
                    );
                    assert_eq!(
                        right_strip.x + right_strip.w,
                        s.x + s.w,
                        "{s:?}: the right edge"
                    );
                }
            }
        }
    }
}

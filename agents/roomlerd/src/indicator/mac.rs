// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-27 — the native consent panel on macOS (AppKit).
//!
//! Like the X11 backend this is the PROMPT only; the "Being viewed by …"
//! banner on macOS stays with the desktop companion.
//!
//! ## Why this did not exist before
//!
//! Not for want of writing it. `agents/roomlerd/src/main.rs` was
//! `#[tokio::main]`, which parks the main thread in `block_on` for the
//! daemon's whole life — and AppKit delivers every event, including the click
//! on an Approve button, on the main run loop. A window created from that
//! process shape appears and then never responds. The fix is in `main.rs`: on
//! this configuration the runtime moves to a worker thread and the main thread
//! is handed to [`run_main_loop`].
//!
//! ## The threading rule, and how it is kept
//!
//! Every `NSWindow` / `NSView` call must happen on the main thread. The
//! signalling loop is on a tokio worker, so [`Inner::prompt`] does NOT touch
//! AppKit at all: it parks the request in shared state, and the main thread
//! picks it up on its next pass. Everything that touches a window lives in
//! [`pump`], which takes a `MainThreadMarker` — so the rule is enforced by the
//! type system rather than by remembering it.
//!
//! ## The application delegate (#3164)
//!
//! Because this process is an `NSApplication`, macOS can ask it to quit with a
//! Quit AppleEvent, and does: System Settings' "Quit & Reopen" after a Screen
//! Recording grant. AppKit's own answer is exit 0, which launchd never
//! relaunches. The one delegate method here ([`AppDelegate`],
//! `applicationShouldTerminate:`) turns such a quit into the daemon's graceful
//! restart and keeps a log-out's quit AppKit's (`crate::macos_quit` has the
//! table).
//!
//! ⚠️ **The panel WILL appear in the captured stream.** `NSWindowSharingNone`
//! is honoured by ScreenCaptureKit and `CGWindowListCreateImage`, but NOT by
//! `CGDisplayStream`, which is what `capture/scrap_backend.rs` uses. It is set
//! anyway — it costs nothing and becomes correct the day capture moves — but
//! it is not true today, and the Windows panel's capture exclusion has no
//! equivalent here. The exposure is bounded: the panel is only up while a
//! prompt is outstanding, before any media flows for THAT session.

#![cfg(all(target_os = "macos", feature = "viewer-indicator-macos"))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Result, anyhow};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
// `MainThreadOnly` is what provides `NSWindow::alloc` — an inherent-looking
// call that is actually a trait method, so the import is load-bearing.
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply, NSBackingStoreType, NSBezelStyle, NSButton, NSButtonType, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSEventMask, NSFont, NSScreen,
    NSScreenSaverWindowLevel, NSTextField, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowSharingType, NSWindowStyleMask,
};
use objc2_foundation::{
    NSAppleEventDescriptor, NSAppleEventManager, NSDate, NSDefaultRunLoopMode, NSObject,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use super::{ConsentSender, PromptView};
use crate::macos_quit::{self, QuitReply};

const W: f64 = 480.0;
const H: f64 = 240.0;
const PAD: f64 = 16.0;
const BTN_W: f64 = 108.0;
const BTN_H: f64 = 30.0;

/// Everything the main thread and the worker share.
///
/// The worker only ever WRITES a request or clears one; the main thread reads
/// it and owns every AppKit object. Nothing `Retained` crosses this boundary.
#[derive(Default)]
struct Shared {
    prompt: Option<PromptView>,
    /// Set when `prompt`/`dismiss` changed something the main thread has not
    /// applied yet.
    dirty: bool,
    /// Where a click goes back. Set once, at construction.
    tx: Option<ConsentSender>,
}

fn shared() -> &'static Mutex<Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(Shared::default()))
}

/// Whether [`run_main_loop`] is actually pumping. Until it is, a prompt would
/// be parked and never drawn — so `prompt` must report `false` and let the
/// caller fall back to the companion.
static PUMPING: AtomicBool = AtomicBool::new(false);

/// FR-97 P2d-2a — work that must run on the MAIN thread, handed to [`pump`]
/// the way a prompt is: queued here, run on its next pass, its answer sent
/// back. Text Input Sources (the keyboard layout) needs it: recent macOS
/// asserts those calls are made on the main thread, which is AppKit's.
type MainJob = Box<dyn FnOnce(MainThreadMarker) + Send>;

fn main_jobs() -> &'static Mutex<Vec<MainJob>> {
    static JOBS: OnceLock<Mutex<Vec<MainJob>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Whether [`on_main`] has a main thread to run on: [`run_main_loop`] is
/// pumping in this process (not in a CLI call, nor in a test).
pub fn main_loop_running() -> bool {
    PUMPING.load(Ordering::SeqCst)
}

/// Run `f` on the main thread and wait up to `timeout` for its answer, which
/// comes on the pump's next pass (it blocks at most 120 ms between passes).
/// `None` without a pumping main loop, when the answer is late, and when
/// called FROM the main thread, where it would wait for itself.
pub fn on_main<R: Send + 'static>(
    timeout: std::time::Duration,
    f: impl FnOnce(MainThreadMarker) -> R + Send + 'static,
) -> Option<R> {
    if !main_loop_running() || MainThreadMarker::new().is_some() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    main_jobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Box::new(move |mtm| {
            let _ = tx.send(f(mtm));
        }));
    rx.recv_timeout(timeout).ok()
}

#[derive(Clone, Default)]
pub(super) struct Inner;

impl Inner {
    pub(super) fn new(consent_tx: ConsentSender) -> Result<Self> {
        // Only useful if `main` handed the main thread to AppKit. It does that
        // under this same feature, so a mismatch means someone changed one
        // half without the other — worth failing loudly rather than parking
        // prompts nothing will draw.
        if !PUMPING.load(Ordering::SeqCst) {
            return Err(anyhow!(
                "the AppKit main loop is not running — `main` must call \
                 indicator::mac::run_main_loop() (FR-27 phase 3.4)"
            ));
        }
        shared().lock().unwrap().tx = Some(consent_tx);
        tracing::info!("macOS consent panel available");
        Ok(Self)
    }

    pub(super) fn prompt(&self, view: PromptView) -> bool {
        if !PUMPING.load(Ordering::SeqCst) {
            return false;
        }
        let mut s = shared().lock().unwrap();
        // At most one on screen — two overlapping Approve/Deny panels is how
        // someone approves the wrong thing.
        if s.prompt.is_some() {
            return false;
        }
        s.prompt = Some(view);
        s.dirty = true;
        true
    }

    pub(super) fn dismiss(&self, session_hex: &str) {
        let mut s = shared().lock().unwrap();
        if s.prompt.as_ref().map(|p| p.session_hex.as_str()) != Some(session_hex) {
            return;
        }
        s.prompt = None;
        s.dirty = true;
    }
}

define_class!(
    // SAFETY:
    // - The superclass NSObject has no subclassing requirements.
    // - `AppDelegate` does not implement `Drop`, and has no ivars.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RoomlerAppDelegate"]
    struct AppDelegate;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for AppDelegate {}

    // SAFETY: `NSApplicationDelegate` has no safety requirements, and the one
    // method below has the protocol's signature. Every other method stays
    // unimplemented, so AppKit's defaults hold for all of them.
    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationShouldTerminate:))]
        fn application_should_terminate(
            &self,
            _sender: &NSApplication,
        ) -> NSApplicationTerminateReply {
            should_terminate()
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's designated initialiser, on a freshly allocated
        // instance (it has no ivars to set).
        unsafe { msg_send![super(this), init] }
    }
}

/// #3164 — `applicationShouldTerminate:`. A Quit AppleEvent that is not a
/// session ending restarts the daemon under its supervisor instead of letting
/// AppKit exit 0, which launchd's `KeepAlive {SuccessfulExit: false}` never
/// relaunches. The table is `macos_quit`'s; this is only its AppKit end.
fn should_terminate() -> NSApplicationTerminateReply {
    let quit = current_apple_event();
    let sender = macos_quit::sender_of(quit.sender_path.as_deref());
    let intent = macos_quit::classify(quit.event, quit.reason, sender);
    let reason_text = quit
        .reason
        .map_or_else(|| "none".to_string(), macos_quit::fourcc_text);
    let sender_text = match (quit.sender_pid, quit.sender_path.as_deref()) {
        (Some(pid), Some(path)) => format!("{path} (pid {pid})"),
        (Some(pid), None) => format!("pid {pid}"),
        _ => "not named".to_string(),
    };
    let mut armed = None;
    let reply = macos_quit::decide(intent, || {
        armed = macos_quit::arm_restart_for_quit(&format!(
            "macOS asked roomlerd to quit (a Quit AppleEvent from {sender_text}, quit reason \
             {reason_text})"
        ));
        armed.is_some()
    });
    match (reply, armed) {
        (QuitReply::RestartLater, Some(code)) => {
            tracing::warn!(
                quit_reason = %reason_text,
                sender = %sender_text,
                exit_code = code,
                "a Quit AppleEvent (System Settings' Quit & Reopen, the Dock, Activity Monitor, a \
                 script): restarting under the supervisor instead of exiting 0, which launchd \
                 would not relaunch (#3164)"
            );
            // The graceful path is bounded; this makes the relaunch certain
            // even if something on it hangs while AppKit waits.
            let _ = std::thread::Builder::new()
                .name("roomlerd-quit-backstop".into())
                .spawn(move || {
                    std::thread::sleep(macos_quit::QUIT_RESTART_BACKSTOP);
                    tracing::error!(
                        exit_code = code,
                        "the restart a Quit AppleEvent started has not finished in time — leaving \
                         now with the supervisor's code (#3164)"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    std::process::exit(code);
                });
            NSApplicationTerminateReply::TerminateLater
        }
        _ => {
            tracing::info!(
                ?intent,
                quit_reason = %reason_text,
                sender = %sender_text,
                "terminating the way AppKit does: exit 0, which launchd does not relaunch (#3164)"
            );
            // AppKit exits as soon as this returns; the log's file layer
            // writes from its own thread, so give it a moment to keep this
            // line (a log-out's quit is diagnosed from it).
            std::thread::sleep(std::time::Duration::from_millis(150));
            NSApplicationTerminateReply::TerminateNow
        }
    }
}

/// What [`current_apple_event`] read off the event AppKit is handling.
#[derive(Default)]
struct QuitEvent {
    /// (class, id); `None` when no Apple Event is being handled.
    event: Option<(u32, u32)>,
    /// `kAEQuitReason`, as an attribute or a parameter.
    reason: Option<u32>,
    /// `keySenderPIDAttr`.
    sender_pid: Option<i32>,
    /// That pid's executable (`proc_pidpath`), which says whether it is
    /// loginwindow.
    sender_path: Option<String>,
}

/// The Apple Event AppKit is handling right now: (class, id), its
/// `kAEQuitReason`, and who sent it. `AERegistry.h` calls the reason a
/// parameter and Apple's samples read it as an attribute, so both are asked.
/// `msg_send!` because objc2-foundation gates these methods on its
/// `objc2-core-services` feature, a crate this lockfile does not carry; each
/// takes or returns a FourCharCode.
fn current_apple_event() -> QuitEvent {
    let Some(event) = NSAppleEventManager::sharedAppleEventManager().currentAppleEvent() else {
        return QuitEvent::default();
    };
    // SAFETY: `eventClass` and `eventID` take nothing and return an
    // `AEEventClass` / `AEEventID`, each a FourCharCode (UInt32).
    let class: u32 = unsafe { msg_send![&*event, eventClass] };
    let id: u32 = unsafe { msg_send![&*event, eventID] };
    let keyword = macos_quit::QUIT_REASON_KEYWORD;
    // SAFETY: both take an `AEKeyword` (a FourCharCode) and return a
    // descriptor, or nil when the event has no such key.
    let attribute: Option<Retained<NSAppleEventDescriptor>> =
        unsafe { msg_send![&*event, attributeDescriptorForKeyword: keyword] };
    let parameter: Option<Retained<NSAppleEventDescriptor>> =
        unsafe { msg_send![&*event, paramDescriptorForKeyword: keyword] };
    let reason = attribute.or(parameter).map(|d| {
        // A type code, or an enumerated one: whichever the sender used.
        let code = d.typeCodeValue();
        if code != 0 { code } else { d.enumCodeValue() }
    });
    let sender_keyword = macos_quit::SENDER_PID_KEYWORD;
    // SAFETY: as above; the attribute is a typeSInt32 when present.
    let sender: Option<Retained<NSAppleEventDescriptor>> =
        unsafe { msg_send![&*event, attributeDescriptorForKeyword: sender_keyword] };
    let sender_pid = sender.map(|d| d.int32Value()).filter(|pid| *pid > 0);
    QuitEvent {
        event: Some((class, id)),
        reason,
        sender_pid,
        sender_path: sender_pid.and_then(process_path),
    }
}

/// A process's executable path, or `None` when it cannot be read (gone, or
/// not ours to see).
fn process_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `proc_pidpath` writes at most `buffersize` bytes into `buf`,
    // which is that long, and returns how many it wrote (0 or less on error).
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok()
}

/// Hand the calling thread to AppKit. Never returns.
///
/// Called from `main` on the MAIN thread only. `Accessory` keeps the daemon
/// out of the Dock and the app switcher; the shipped `.app` already sets
/// `LSUIElement`, and this makes it true for a non-bundled dev run too.
pub fn run_main_loop() -> ! {
    let mtm = MainThreadMarker::new()
        .expect("indicator::mac::run_main_loop must be called on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    // #3164 — before `finishLaunching`, so AppKit consults it from the first
    // event. `setDelegate` does not retain: this local lives as long as the
    // loop below, which is the process.
    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    PUMPING.store(true, Ordering::SeqCst);

    // `NSApp.run()` would never give control back, so this is the manual pump
    // it wraps: block for one event or 120 ms, dispatch it, then step our own
    // state. 120 ms matches the Windows pump — instant to a human, negligible
    // to run for the daemon's whole life.
    //
    // Deliberately NOT an `NSTimer` with a Rust callback: a timer needs a
    // TARGET object, i.e. a custom Objective-C class, i.e. `define_class!` and
    // a second unsafe surface with its own `Retained` lifetime story — to
    // arrange a wake-up a bounded `nextEventMatchingMask:` already gives.
    //
    // ⚠️ `finishLaunching` is what `run()` would have called. Skipping it
    // leaves NSApplication half-initialised and windows behave erratically.
    app.finishLaunching();

    let mut panel: Option<Panel> = None;
    loop {
        pump(mtm, &mut panel);
        unsafe {
            let until = NSDate::dateWithTimeIntervalSinceNow(0.12);
            if let Some(ev) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&until),
                NSDefaultRunLoopMode,
                true,
            ) {
                app.sendEvent(&ev);
            }
        }
    }
}

/// One AppKit-side step. MAIN THREAD ONLY (the `MainThreadMarker` proves it).
fn pump(mtm: MainThreadMarker, panel: &mut Option<Panel>) {
    // FR-97 P2d-2a — queued main-thread work ([`on_main`]) first. A job that
    // panics is contained: this thread is AppKit's for the daemon's whole life.
    let jobs = std::mem::take(&mut *main_jobs().lock().unwrap_or_else(|e| e.into_inner()));
    for job in jobs {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(mtm))).is_err() {
            tracing::warn!("indicator: a main-thread job panicked");
        }
    }

    let (view, dirty) = {
        let mut s = shared().lock().unwrap();
        let d = std::mem::take(&mut s.dirty);
        (s.prompt.clone(), d)
    };

    // A click is reported by the panel itself; harvest it before anything else
    // so a decision is never delayed by a redraw.
    if let Some(p) = panel.as_ref()
        && let Some(allow) = p.take_click()
    {
        let session = {
            let mut s = shared().lock().unwrap();
            s.dirty = true;
            s.prompt.take().map(|v| v.session_hex)
        };
        if let Some(session) = session {
            // Report, never resolve: the broker is the single decision point.
            let tx = shared().lock().unwrap().tx.clone();
            if let Some(tx) = tx {
                let _ = tx.try_send((session, allow));
            }
        }
    }

    match (&view, panel.is_some()) {
        (Some(v), false) => match Panel::new(mtm, v) {
            Ok(p) => *panel = Some(p),
            Err(e) => tracing::warn!(%e, "could not build the macOS consent panel"),
        },
        (Some(v), true) => {
            if dirty {
                panel.as_ref().unwrap().update(v);
            } else {
                panel.as_ref().unwrap().tick_countdown(v);
            }
        }
        (None, true) => {
            panel.take();
        }
        (None, false) => {}
    }
}

/// The window and the labels whose text changes. Owned solely by the main
/// thread; dropping it closes the window.
struct Panel {
    window: Retained<NSWindow>,
    countdown: Retained<NSTextField>,
    approve: Retained<NSButton>,
    deny: Retained<NSButton>,
    secs_shown: std::cell::Cell<u64>,
}

impl Panel {
    fn new(mtm: MainThreadMarker, v: &PromptView) -> Result<Self> {
        unsafe {
            // Top-centre of the main screen: out of the way of most content,
            // and where a notification is expected. AppKit's origin is
            // BOTTOM-left, so "48 px below the top" is `max_y - H - 48`.
            let frame = NSScreen::mainScreen(mtm)
                .map(|s| s.frame())
                .unwrap_or(NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(1440.0, 900.0),
                ));
            let origin = NSPoint::new(
                frame.origin.x + (frame.size.width - W) / 2.0,
                frame.origin.y + frame.size.height - H - 48.0,
            );
            let window = NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(origin, NSSize::new(W, H)),
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            );
            window.setLevel(NSScreenSaverWindowLevel);
            // Visible on every Space and over a fullscreen app: a prompt that
            // only exists on the desktop someone happens to be looking at is
            // not a prompt.
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary
                    | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
            // Correct, and currently ineffective — see the module docs.
            window.setSharingType(NSWindowSharingType::None);
            window.setOpaque(true);
            window.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
                0.117, 0.117, 0.117, 1.0,
            )));
            window.setIgnoresMouseEvents(false);
            // Never steal focus: the person may be mid-sentence in something
            // else, and a prompt that eats keystrokes is worse than a late one.
            window.setHidesOnDeactivate(false);

            let content = window
                .contentView()
                .ok_or_else(|| anyhow!("NSWindow has no content view"))?;

            let mut y = H - PAD - 22.0;
            add_label(
                mtm,
                &content,
                &v.title,
                PAD,
                y,
                W - 2.0 * PAD,
                22.0,
                15.0,
                true,
                1.0,
            );
            y -= 26.0;
            add_label(
                mtm,
                &content,
                &v.lead,
                PAD,
                y,
                W - 2.0 * PAD,
                20.0,
                13.0,
                false,
                1.0,
            );
            y -= 24.0;
            if !v.org.is_empty() {
                add_label(
                    mtm,
                    &content,
                    &format!("On behalf of {}", v.org),
                    PAD,
                    y,
                    W - 2.0 * PAD,
                    18.0,
                    11.5,
                    false,
                    0.69,
                );
                y -= 22.0;
            }
            if !v.detail.is_empty() {
                add_label(
                    mtm,
                    &content,
                    &v.detail,
                    PAD,
                    y - 16.0,
                    W - 2.0 * PAD,
                    36.0,
                    11.5,
                    false,
                    1.0,
                );
                y -= 40.0;
            }
            if !v.permissions.is_empty() {
                add_label(
                    mtm,
                    &content,
                    &format!("Permissions: {}", v.permissions),
                    PAD,
                    y,
                    W - 2.0 * PAD,
                    18.0,
                    11.5,
                    false,
                    0.69,
                );
            }

            let btn_y = PAD;
            let approve = add_button(
                mtm,
                &content,
                "Approve",
                W - PAD - BTN_W,
                btn_y,
                TAG_APPROVE,
            );
            let deny = add_button(
                mtm,
                &content,
                "Deny",
                W - PAD - BTN_W - 8.0 - BTN_W,
                btn_y,
                TAG_DENY,
            );
            let countdown = add_label(
                mtm,
                &content,
                "",
                PAD,
                btn_y + 6.0,
                W - 2.0 * PAD - 2.0 * BTN_W - 24.0,
                18.0,
                11.5,
                false,
                0.69,
            );

            // `orderFrontRegardless` rather than `makeKeyAndOrderFront`: show
            // it without becoming the active app.
            window.orderFrontRegardless();

            let p = Panel {
                window,
                countdown,
                approve,
                deny,
                secs_shown: std::cell::Cell::new(u64::MAX),
            };
            p.tick_countdown(v);
            Ok(p)
        }
    }

    /// Rebuild-free refresh for the countdown. Repaints only when the whole
    /// second changes — the pump ticks 8×/s and rewriting the same string that
    /// often is churn for nothing.
    fn tick_countdown(&self, v: &PromptView) {
        let secs = v
            .expires_at
            .saturating_duration_since(Instant::now())
            .as_secs();
        if secs == self.secs_shown.get() {
            return;
        }
        self.secs_shown.set(secs);
        unsafe {
            self.countdown.setStringValue(&NSString::from_str(&format!(
                "Expires in {}",
                super::format_countdown(secs)
            )));
        }
    }

    /// The prompt's content changed under us. Rare enough (at most one panel
    /// at a time, and a new question means a new panel) that a full rebuild
    /// would be simpler — but the CALLER holds the panel, so just refresh what
    /// can move.
    fn update(&self, v: &PromptView) {
        self.secs_shown.set(u64::MAX);
        self.tick_countdown(v);
    }

    /// Has either button been pressed since the last check?
    ///
    /// Polled rather than delivered, because a target/action callback needs an
    /// Objective-C class to be the target — `define_class!`, a second unsafe
    /// surface, and a lifetime tangle with `Retained` — to carry information a
    /// button already holds: `NSButton::state`. The pump reads it 8×/s, which
    /// is well inside human reaction time.
    fn take_click(&self) -> Option<bool> {
        unsafe {
            if self.approve.state() == NSControlStateValueOn {
                self.approve.setState(NSControlStateValueOff);
                return Some(true);
            }
            if self.deny.state() == NSControlStateValueOn {
                self.deny.setState(NSControlStateValueOff);
                return Some(false);
            }
        }
        None
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        // Main thread by construction: `Panel` is only ever held by `pump`.
        self.window.close();
    }
}

const TAG_APPROVE: isize = 1;
const TAG_DENY: isize = 2;

#[allow(clippy::too_many_arguments)]
unsafe fn add_label(
    mtm: MainThreadMarker,
    parent: &NSView,
    text: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    size: f64,
    bold: bool,
    white: f64,
) -> Retained<NSTextField> {
    unsafe {
        let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        label.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)));
        let font = if bold {
            NSFont::boldSystemFontOfSize(size)
        } else {
            NSFont::systemFontOfSize(size)
        };
        label.setFont(Some(&font));
        label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            white, white, white, 1.0,
        )));
        // Wrap rather than clip: the `detail` line carries the command an
        // `exec` prompt would run, and a command truncated at the card edge is
        // one somebody approves without having read it. `0` = no limit, which
        // the caller bounds by the frame height it passes.
        label.setMaximumNumberOfLines(0);
        parent.addSubview(&label);
        label
    }
}

unsafe fn add_button(
    mtm: MainThreadMarker,
    parent: &NSView,
    title: &str,
    x: f64,
    y: f64,
    tag: isize,
) -> Retained<NSButton> {
    unsafe {
        let b =
            NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, mtm);
        b.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(BTN_W, BTN_H)));
        b.setBezelStyle(NSBezelStyle::Rounded);
        b.setTag(tag);
        // A PUSH-ON-PUSH-OFF button so its `state` records the press for the
        // poller to find; `take_click` clears it. A momentary button would
        // have nothing to read after the click returns.
        b.setButtonType(NSButtonType::PushOnPushOff);
        parent.addSubview(&b);
        b
    }
}

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
//! process shape appears and then never responds. The fix is in `app.rs`: on
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
//! type system rather than by remembering it. The one window a question makes
//! before that is #4053's probe ([`window_device`]), a job the main thread
//! runs too ([`on_main`]).
//!
//! ## When the panel takes no question (#4053, #4119)
//!
//! A root daemon that launchd started before anyone signed in was refused by
//! the window server, and a log-out detaches one that was served: either way
//! its windows reach no screen for the rest of its life. So `prompt` asks
//! [`super::panel_gate`] first: in a root process, whether another console
//! session has started since the pump's start ([`super::console_watch`]) and
//! whether the seat has emptied or changed hands since ([`seat_looked`], from
//! the person socket's poll), and only then whether a probe window gets a
//! window device. Its `false` is a taken panel's, and the question goes to
//! the person's app.
//!
//! #4119 — that order is load-bearing. A log-out can also KILL the
//! window-server port, and a window made on a dead port is where AppKit ends
//! the process with exit 0, which launchd does not relaunch: the probe itself
//! took a two-row Mac's root half down at its first question after a log-out
//! (0.4.228). So the session and the seat answer before any window is made,
//! and the net ([`super::exit_net`]) is under them: an exit AppKit starts
//! inside one of these window calls, on this thread, that our code did not
//! choose, leaves with a code of its own, which launchd relaunches.
//!
//! ## The root daemon's questions, on a screen worker's panel (FR-149 P1)
//!
//! On a one-row Mac the screen worker (`run --supervised --unenrolled`) runs
//! as the person, in the person's GUI session, where the window server serves
//! it after a boot and after a log-out, when the root daemon's own panel
//! cannot. The root daemon asks it over the delegation channel (`PanelAsk`,
//! `indicator::session_panel`), and [`show_root_question`] answers: the
//! worker's gate (its own account owns `/dev/console`, the panel shows no
//! other question, a 24-hex id, time left), then ONE main-thread job that
//! puts the panel up and reads its window number. Above 0 is `shown`; 0 or
//! less takes the panel down again and is `not_up`. The question then sits in
//! the one slot every question shares, so there is never a second panel, and
//! a click on it goes out on the same `ConsentSender`: the signalling loop
//! sends it on the person's app's road to the root half
//! (`consent_relay::panel_answered`), never to this process's own broker,
//! which is not asking it. The slot is cleared at the click, so a click the
//! root half refused is said to the root daemon, which moves the question to
//! the person's app. The root daemon takes it down (`PanelHide`,
//! [`dismiss_root_question`]); so does the pump at its time's end, and the
//! worker when its channel ends. The panel is built with no lock held
//! (`put_up`), so a window server that stalls the build never stalls the
//! worker's frame loop.
//!
//! ## The main loop's promise (FR-149 P1, C2)
//!
//! `main` starts the daemon's thread and then hands its own to AppKit, so the
//! daemon could build its panel before [`run_main_loop`] pumped, and a panel
//! refused then stayed refused for the process's life (P0's read on
//! 0.4.228). `main` now promises the loop first ([`promise_main_loop`]): the
//! promise builds the panel, and the loop's first pass opens it to questions
//! (`panel_gate::MainLoop`). A question before that pass goes to the
//! person's app; none after it does for that reason.
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

use std::sync::atomic::Ordering;
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

use super::console_watch::ConsoleWatch;
use super::exit_net;
use super::panel_gate::{Connection, MainLoop, PanelGate, Screen, Seat, reads_console_session};
use super::session_panel::{PanelQuestion, PanelState};
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
    /// FR-149 P1 — `prompt` is the root daemon's question, put up by
    /// [`show_root_question`]: the pump takes it down at its time's end.
    root: bool,
}

impl Shared {
    /// The slot is empty again, whoever's question it held.
    fn clear(&mut self) {
        self.prompt = None;
        self.root = false;
        self.dirty = true;
    }
}

fn shared() -> &'static Mutex<Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(Shared::default()))
}

/// Where [`run_main_loop`] stands: promised by `main`, then pumping. Until it
/// pumps, a prompt would be parked and never drawn — so `prompt` must report
/// `false` and let the caller fall back to the companion. FR-149 P1 (C2) —
/// but a panel built after the promise is kept, and takes questions from the
/// loop's first pass.
static MAIN_LOOP: MainLoop = MainLoop::new();

/// FR-149 P1 (C2) — `main` will hand the main thread to AppKit
/// ([`run_main_loop`]). Called before it starts the daemon's thread, so a
/// panel the daemon builds first is built for the loop to come.
pub fn promise_main_loop() {
    MAIN_LOOP.promise();
}

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
    MAIN_LOOP.takes_a_question()
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
        // prompts nothing will draw. FR-149 P1 (C2) — `main` promises the
        // loop before it starts the daemon's thread, so a panel built before
        // the loop pumps is kept, and takes questions from its first pass.
        if !MAIN_LOOP.builds_a_panel() {
            return Err(anyhow!(
                "the AppKit main loop is not running — `main` must call \
                 indicator::mac::run_main_loop() (FR-27 phase 3.4)"
            ));
        }
        shared().lock().unwrap().tx = Some(consent_tx);
        // #4053 — "available", or in its place one WARN: a panel the window
        // server refused when the pump started shows no question at all.
        // Built before the pump started, the pump says it instead.
        if let Some(screen) = PANEL_SCREEN.get() {
            screen.gate.announce();
        }
        Ok(Self)
    }

    pub(super) fn prompt(&self, view: PromptView) -> bool {
        if !MAIN_LOOP.takes_a_question() {
            return false;
        }
        // #4053 — not where this process cannot reach the screen now: the
        // window server refused it at the start, its connection is gone (a
        // log-out), another console session has started since, or (#4119)
        // the seat has emptied or changed hands since. Asked
        // before the shared lock is taken, never under it: the connection's
        // read waits for the main thread, `PROBE_WAIT` at most, and the pump
        // and `dismiss` take that lock. This `false` is a taken panel's: the
        // caller sends the question to the person's app.
        if !PANEL_SCREEN.get().is_some_and(PanelScreen::shows_now) {
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
        s.clear();
    }
}

/// FR-149 P1 — how long the screen worker waits for the main thread to put a
/// root daemon's question up: inside the root daemon's own wait
/// (`session_panel::PANEL_WITHIN`, 500 ms), with the channel's two crossings
/// to spare. The pump comes round every 120 ms at most.
const SHOW_WITHIN: std::time::Duration = std::time::Duration::from_millis(400);

/// FR-149 P1 — the root daemon's question `id` on this screen worker's
/// panel: the worker's gate ([`super::session_panel::worker_gate`]), then
/// ONE main-thread job that puts the panel up and reads its window number.
/// Above 0 is `shown`; 0 or less takes the panel down at once and is
/// `not_up`. Answered within [`SHOW_WITHIN`] ([`put_up_within`]): a later
/// panel would be a second surface beside the app's, since the answer said
/// `not_up` (`main_thread_late`). Blocking: the caller runs it on the
/// blocking pool.
pub(crate) fn show_root_question(id: &str, question: &PanelQuestion) -> PanelState {
    // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
    let own = unsafe { libc::geteuid() };
    let free = shared().lock().unwrap().prompt.is_none();
    if let Err(state) = super::session_panel::worker_gate(
        own,
        super::session_panel::console_owner(),
        free,
        id,
        std::time::Duration::from_millis(question.left_ms),
    ) {
        return state;
    }
    if !MAIN_LOOP.takes_a_question() {
        return PanelState::NotUp {
            why: "no_main_loop".into(),
        };
    }
    let view = question.view(id, Instant::now());
    put_up_within(SHOW_WITHIN, view).unwrap_or_else(|| PanelState::NotUp {
        why: "main_thread_late".into(),
    })
}

/// FR-149 P1 — on the main thread: the root daemon's question into the one
/// slot, and its panel up, when the slot is still free. The slot is read, and
/// then filled, under the shared lock, and the window is built between the
/// two with no lock held: a window server that stalls the build stalls the
/// main thread alone, never a thread that takes the lock (the worker's frame
/// loop, whose `PanelHide` takes the question down, and the next question's
/// gate).
fn put_up(mtm: MainThreadMarker, view: PromptView) -> PanelState {
    if shared().lock().unwrap().prompt.is_some() {
        return PanelState::Busy;
    }
    let _inside = exit_net::inside("the panel's window");
    let panel = match Panel::new(mtm, &view) {
        Ok(panel) => panel,
        Err(e) => {
            return PanelState::NotUp {
                why: format!("no_panel: {e:#}"),
            };
        }
    };
    let number = panel.window.windowNumber();
    if number <= 0 {
        // Dropping it closes the window.
        drop(panel);
        return PanelState::NotUp {
            why: format!("no_window_device: window number {number}"),
        };
    }
    let filled = {
        let mut s = shared().lock().unwrap();
        // This process's own question took the slot while the window was
        // built (`Inner::prompt`): it is that question's.
        let free = s.prompt.is_none();
        if free {
            s.prompt = Some(view);
            s.root = true;
            s.dirty = false;
        }
        free
    };
    if !filled {
        // Dropped with no lock held: it closes the window.
        drop(panel);
        return PanelState::Busy;
    }
    PANEL.with_borrow_mut(|p| *p = Some(panel));
    PanelState::Shown
}

/// FR-149 P1 — whether this process's panel holds a question now. On a
/// one-row Mac's root daemon the session panel is then not asked: at most
/// one panel on the screen (`session_panel::chain`).
pub(crate) fn shows_a_question() -> bool {
    shared().lock().unwrap().prompt.is_some()
}

/// FR-149 P1 — take the root daemon's question down: `id`'s (`PanelHide`),
/// or whichever is up (`None`, when the channel ends). This process's own
/// questions stay.
pub(crate) fn dismiss_root_question(id: Option<&str>) {
    let mut s = shared().lock().unwrap();
    let held = s.prompt.as_ref().map(|p| p.session_hex.as_str());
    if s.root && (id.is_none() || held == id) {
        s.clear();
    }
}

/// [`put_up`] on the main thread ([`on_main`]'s queue), answered within
/// `timeout` at most, and always what the screen holds. A job the main thread
/// has not begun when the caller stops waiting never runs; one it is running
/// then takes its panel down again at its end, since the caller's answer is
/// `not_up`. `None` when no answer came in time, or with no pumping main loop.
fn put_up_within(timeout: std::time::Duration, view: PromptView) -> Option<PanelState> {
    use std::sync::atomic::AtomicU8;
    const WAITING: u8 = 0;
    const BEGUN: u8 = 1;
    const DONE: u8 = 2;
    const GIVEN_UP: u8 = 3;
    if !main_loop_running() || MainThreadMarker::new().is_some() {
        return None;
    }
    let claim = std::sync::Arc::new(AtomicU8::new(WAITING));
    let theirs = claim.clone();
    let id = view.session_hex.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    main_jobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Box::new(move |mtm| {
            if theirs
                .compare_exchange(WAITING, BEGUN, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return;
            }
            let state = put_up(mtm, view);
            if theirs
                .compare_exchange(BEGUN, DONE, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let _ = tx.send(state);
            } else if state == PanelState::Shown {
                // The caller stopped waiting while the panel went up, and
                // answered `not_up`: a panel the root daemon did not count
                // never stays.
                dismiss_root_question(Some(&id));
                PANEL.with_borrow_mut(|panel| *panel = None);
            }
        }));
    match rx.recv_timeout(timeout) {
        Ok(state) => Some(state),
        Err(_) => {
            let given_up = claim
                .compare_exchange(WAITING, GIVEN_UP, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
                || claim
                    .compare_exchange(BEGUN, GIVEN_UP, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok();
            if given_up {
                None
            } else {
                // Done just as the wait ran out: the job sends its answer in
                // its next step, with nothing between that could wait.
                rx.recv().ok()
            }
        }
    }
}

thread_local! {
    /// The panel on screen. The main thread's alone: [`pump`] and the jobs
    /// it runs ([`put_up`], [`put_up_within`]'s) are the only ones that touch
    /// it.
    static PANEL: std::cell::RefCell<Option<Panel>> = const { std::cell::RefCell::new(None) };
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
                "a Quit AppleEvent (System Settings' Quit & Reopen, a script): restarting under \
                 the supervisor instead of exiting 0, which launchd would not relaunch (#3164)"
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
            // #4119 — this exit 0 is ours (a log-out's quit from loginwindow,
            // a quit under no supervisor): the exit net leaves it alone.
            exit_net::exit_chosen();
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

    // #4119 — the exit net (`exit_net`), in a root process only: armed before
    // this thread's first window call (the reads below), and after launching,
    // as `exit` runs the last-registered hook first: ahead of any launching made.
    // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
    if exit_net::arms_in(unsafe { libc::geteuid() }) {
        exit_net::arm();
    }

    // #4053 — the panel's first reads, after `finishLaunching` and never
    // before it: whether the window server serves this process, and the
    // console session it serves. Made before the loop pumps, so a panel that
    // finds the loop pumping finds them.
    let screen = PANEL_SCREEN.get_or_init(|| PanelScreen::at_start(mtm));
    MAIN_LOOP.pumps();
    // FR-149 P1 (C2) — a panel built on `main`'s promise, before these reads,
    // could not say what they found: it is said now, once.
    if shared().lock().unwrap().tx.is_some() {
        screen.gate.announce();
    }

    loop {
        pump(mtm);
        // #4119 — the event fetch and its dispatch, where AppKit runs what it
        // queued (the delegate's answer to a quit included): the net's.
        let _inside = exit_net::inside("the event fetch and dispatch");
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
fn pump(mtm: MainThreadMarker) {
    // FR-97 P2d-2a — queued main-thread work ([`on_main`]) first. A job that
    // panics is contained: this thread is AppKit's for the daemon's whole life.
    let jobs = std::mem::take(&mut *main_jobs().lock().unwrap_or_else(|e| e.into_inner()));
    for job in jobs {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(mtm))).is_err() {
            tracing::warn!("indicator: a main-thread job panicked");
        }
    }
    PANEL.with_borrow_mut(|panel| step_the_panel(mtm, panel));
}

/// The panel's part of [`pump`]: made, refreshed, read for a click, and
/// closed. MAIN THREAD ONLY.
fn step_the_panel(mtm: MainThreadMarker, panel: &mut Option<Panel>) {
    // #4119 — from here on, the panel's own window: made, refreshed, read for
    // a click, and closed. The net's (`exit_net`).
    let _inside = exit_net::inside("the panel's window");
    let (view, dirty) = {
        let mut s = shared().lock().unwrap();
        // FR-149 P1 — the root daemon's question comes down at its time's
        // end, whatever became of its `PanelHide`.
        if s.root
            && s.prompt
                .as_ref()
                .is_some_and(|v| Instant::now() >= v.expires_at)
        {
            s.clear();
        }
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
            let session = s.prompt.take().map(|v| v.session_hex);
            s.clear();
            session
        };
        if let Some(session) = session {
            // Report, never resolve: the broker is the single decision point.
            let tx = shared().lock().unwrap().tx.clone();
            if let Some(tx) = tx {
                let _ = tx.try_send((session, allow));
            }
        }
    }

    // FR-149 P1 — a panel left from another question (taken down, and the
    // slot filled again, between two passes) is never reused for this one:
    // its text is the other question's.
    if let (Some(v), Some(p)) = (&view, panel.as_ref())
        && p.shows != v.session_hex
    {
        panel.take();
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
    /// FR-149 P1 — the question it shows, by id: a panel is never refreshed
    /// for another one.
    shows: String,
}

impl Panel {
    fn new(mtm: MainThreadMarker, v: &PromptView) -> Result<Self> {
        unsafe {
            // #3999 — the top centre of the screen that holds the pointer,
            // else of the primary (`question_screen`), as the app's prompt
            // chooses: out of the way of most content, and where a
            // notification is expected. AppKit's origin is BOTTOM-left, so
            // "48 px below the top" is `max_y - H - 48`.
            let frame = question_screen(mtm).unwrap_or(NSRect::new(
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
            // A plain NSWindow is released when it closes, and its `Retained`
            // releases it again when `Panel` drops it after `close` (`Drop`
            // below): objc2-app-kit requires this call for a window made
            // outside a window controller. An NSPanel defaults to it.
            window.setReleasedWhenClosed(false);
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
                shows: v.session_hex.clone(),
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

/// #4053 — how long a question waits for the main thread to read the
/// connection. The pump comes round every 120 ms at most; past this the
/// question goes to the person's app, said once (`panel_gate`'s `NotKnown`).
const PROBE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// #4053 — the panel's gate ([`PanelGate`], the same on every lane), and the
/// two readers it asks at each question.
struct PanelScreen {
    gate: PanelGate,
    /// notify(3) on the window server's console-session names, from the
    /// pump's start, in a root process only ([`reads_console_session`]).
    console: Option<ConsoleWatch>,
}

/// Made on the main thread after `finishLaunching`, before the loop pumps
/// ([`run_main_loop`]).
static PANEL_SCREEN: OnceLock<PanelScreen> = OnceLock::new();

impl PanelScreen {
    /// The reads at the pump's start, on the main thread. A per-user process
    /// (a two-row Mac's per-user half) makes no console reader and follows
    /// no seat: its gate starts with no console session, and the connection
    /// alone decides. A root process's gate follows the seat too (#4119).
    fn at_start(mtm: MainThreadMarker) -> Self {
        // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
        let euid = unsafe { libc::geteuid() };
        let root = reads_console_session(euid);
        let console = root.then(ConsoleWatch::console);
        let gate = PanelGate::new(
            window_device(mtm),
            console.as_ref().and_then(ConsoleWatch::session),
        )
        .following_the_seat(root);
        Self { gate, console }
    }

    /// Whether the panel can show a question now ([`PanelGate::verdict`]).
    fn shows_now(&self) -> bool {
        self.gate.verdict(self).shows()
    }
}

impl Screen for PanelScreen {
    fn connection(&self) -> Connection {
        match MainThreadMarker::new() {
            // No caller asks from the main thread today, and one that did
            // would wait for itself through `on_main`.
            Some(mtm) => probe(mtm),
            None => on_main(PROBE_WAIT, probe).unwrap_or(Connection::NotKnown),
        }
    }

    fn console_session(&self) -> Option<u64> {
        self.console.as_ref().and_then(ConsoleWatch::session)
    }
}

/// #4119 — the probe window, unless the panel ended while the question
/// waited for the main thread (the seat, told meanwhile by the person
/// socket's poll): then no window is made, and the gate answers what ended
/// it. On the main thread.
fn probe(mtm: MainThreadMarker) -> Connection {
    if PANEL_SCREEN.get().is_some_and(|s| s.gate.has_ended()) {
        return Connection::NotKnown;
    }
    window_device(mtm)
}

/// #4119 — the person socket's look at the seat, from its poll in a root
/// process (`person_socket::follow_the_seat`, every 5 s): a seat that empties
/// or changes hands ends the panel for this process's life
/// ([`PanelGate::seat_looked`]), and a look that could not tell changes
/// nothing. Any thread: no AppKit call and no window. A look before the pump
/// has started has no gate to tell; the gate's first look is then a later one.
pub(crate) fn seat_looked(look: &crate::person_socket::Look) {
    if let (Some(screen), Some(seat)) = (PANEL_SCREEN.get(), Seat::of(look)) {
        screen.gate.seat_looked(seat);
    }
}

/// #4053 — whether this process can put a window on the window server now:
/// a 1×1 borderless window made with its window device at once (`defer:
/// false`), never shown, then closed. AppKit gives a window a number above 0
/// only with a window device, so a process the window server refused, before
/// login or after a log-out, reads 0 or less. The panel's own window is made
/// the same way, at each question it shows (`Panel::new`).
///
/// ⚠️ #4119 — a log-out that KILLS the window-server port leaves no number
/// to read: AppKit ends the process while it makes this window (0.4.228,
/// `_CGSCreateWindow returned 268435459`, exit 0). So the gate asks the
/// console session and the seat first, and makes this window only when
/// neither has changed; the net ([`exit_net`]) turns an exit begun here into
/// one that launchd relaunches, with a code of its own.
fn window_device(mtm: MainThreadMarker) -> Connection {
    let _inside = exit_net::inside("the probe window");
    // SAFETY: a window made outside a window controller must not be released
    // when it closes (objc2-app-kit), and `setReleasedWhenClosed(false)` right
    // after it is made keeps that: its `Retained` releases it once, here.
    let number = unsafe {
        let window = NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0)),
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        );
        window.setReleasedWhenClosed(false);
        let number = window.windowNumber();
        window.close();
        number
    };
    if number > 0 {
        Connection::Works(number)
    } else {
        Connection::Refused(number)
    }
}

/// #3999 — the frame of the screen a question opens on: the one that holds
/// the pointer, else the primary screen, by the rule the app's prompt follows
/// (`roomler_node_core::consent_screen`). Read on the main thread, as the app
/// reads them. `None` with no screen at all.
fn question_screen(mtm: MainThreadMarker) -> Option<NSRect> {
    use roomler_node_core::consent_screen::{self, Frame, Point};
    let frames: Vec<NSRect> = NSScreen::screens(mtm)
        .to_vec()
        .iter()
        .map(|s| s.frame())
        .collect();
    let screens: Vec<Frame> = frames
        .iter()
        .map(|f| Frame {
            x: f.origin.x,
            y: f.origin.y,
            w: f.size.width,
            h: f.size.height,
        })
        .collect();
    let p = objc2_app_kit::NSEvent::mouseLocation();
    let chosen = consent_screen::choose(Point { x: p.x, y: p.y }, &screens)?;
    frames.get(chosen).copied()
}

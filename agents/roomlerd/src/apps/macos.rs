// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-97 P2c — the macOS backend for Remote Apps, without AppKit.
//!
//! * **list**: the window server's own list (`CGWindowListCopyWindowInfo`),
//!   filtered by [`super::quartz_listing`]. A window's title needs the Screen
//!   Recording grant, which the process serving the session already holds to
//!   capture.
//! * **focus**: the Accessibility API (`AXUIElement`). The window's owner comes
//!   from the window server, never from the browser. Its AX window is matched by
//!   `_AXUIElementGetWindow`, else by title and frame. A hidden app is unhidden
//!   and a minimised window restored, each waited out until the window is on
//!   screen. Then `AXFrontmost` + `AXRaise` + `AXMain`, checked against the
//!   window server's front window, with one retry.
//! * **launch**: the allowlist's argv, run as this process, which is the person
//!   in their GUI session, so `open -b <bundle id>` goes through LaunchServices.
//!   A terminal entry opens a new Terminal window through a self-deleting
//!   `.command` script in the person's own temporary directory. No AppleScript,
//!   so no Automation prompt.
//!
//! It runs where a session's peer runs: a per-user agent, or a supervised Mac's
//! GUI worker (FR-43). The root daemon has no window-server session, and
//! [`availability`] says so.
//!
//! Measured on a tart Mac (macOS 15.7.7, FR-97 §8) before it was written:
//! * The on-screen list runs front to back and held exactly the person's three
//!   windows. The full list added 18 untitled helper windows (menu strips,
//!   cursor views, a panel service), next to the one titled window that was
//!   minimised. Hence the filter: layer 0 and on screen, or off screen WITH a
//!   title.
//! * `kCGWindowListOptionIncludingWindow` alone finds nothing for a minimised
//!   window, so a lookup by id scans the full list.
//! * Calculator's window refuses `AXRaise` (error -25205), and `AXFrontmost`
//!   alone brought it to the front. So no single call's error fails a focus:
//!   the window server's front window decides.
//! * Raising a window of a HIDDEN app returned success on every call and left
//!   the app unhidden BEHIND the front one. Unhiding first, then waiting until
//!   the window was on screen (50 ms), brought it to the front.
//! * `open -b com.apple.Terminal` opens a window only when Terminal is not
//!   running. A `.command` script opens a new one every time, and it had
//!   deleted itself (`rm -f -- "$0"`) by the time it ran.

use std::ffi::{CStr, OsStr, c_char, c_void};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};

use super::{
    Coverage, LaunchOutcome, QuartzWindow, ResolvedApp, Unavailable, WindowInfo, WindowManager,
};
use crate::tcc;

type CFTypeRef = *const c_void;
type AXError = i32;

const AX_SUCCESS: AXError = 0;
const K_CG_WINDOW_LIST_OPTION_ALL: u32 = 0;
const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1 << 0;
const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;
const ON_SCREEN: u32 =
    K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS;
const K_CG_NULL_WINDOW_ID: u32 = 0;
const K_CF_NUMBER_SINT64_TYPE: isize = 4;
const K_CF_NUMBER_FLOAT64_TYPE: isize = 6;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_AX_VALUE_CG_POINT_TYPE: u32 = 1;
const K_AX_VALUE_CG_SIZE_TYPE: u32 = 2;

/// Terminal.app, which every Mac has. A person's own terminal (iTerm2 and the
/// like) would need its own way to be handed a command.
const TERMINAL_BUNDLE_ID: &str = "com.apple.Terminal";
/// Absolute, because a supervised worker's `PATH` is whatever `sudo` left it.
const OPEN: &str = "/usr/bin/open";
/// How long a launch waits for its launcher to answer. `open` returns once
/// LaunchServices has the request, well inside this; a directly run program
/// that is still running at the deadline is taken as launched.
const LAUNCH_WAIT: Duration = Duration::from_secs(5);
/// How long a focus waits for each of its steps to show in the window server.
const STEP_WAIT: Duration = Duration::from_millis(1500);
const POLL: Duration = Duration::from_millis(50);

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFTypeRef;
    fn CGRectMakeWithDictionaryRepresentation(dict: CFTypeRef, rect: *mut CGRect) -> bool;
    static kCGWindowNumber: CFTypeRef;
    static kCGWindowLayer: CFTypeRef;
    static kCGWindowOwnerPID: CFTypeRef;
    static kCGWindowOwnerName: CFTypeRef;
    static kCGWindowName: CFTypeRef;
    static kCGWindowIsOnscreen: CFTypeRef;
    static kCGWindowBounds: CFTypeRef;
    static kCGWindowAlpha: CFTypeRef;
}

// CoreFoundation's `Boolean` is an unsigned char, not a C99 `_Bool`, so the
// calls that return or take one are declared with `u8` (as `tcc` does).
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
    fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberGetValue(number: CFTypeRef, the_type: isize, value: *mut c_void) -> u8;
    fn CFStringGetLength(string: CFTypeRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(string: CFTypeRef, buffer: *mut c_char, size: isize, encoding: u32)
    -> u8;
    fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        len: isize,
        encoding: u32,
        is_external: u8,
    ) -> CFTypeRef;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFBooleanGetValue(boolean: CFTypeRef) -> u8;
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFTypeRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementSetAttributeValue(
        element: CFTypeRef,
        attribute: CFTypeRef,
        value: CFTypeRef,
    ) -> AXError;
    fn AXUIElementPerformAction(element: CFTypeRef, action: CFTypeRef) -> AXError;
    fn AXValueGetValue(value: CFTypeRef, the_type: u32, out: *mut c_void) -> u8;
}

/// A CoreFoundation object this code owns (a Create/Copy result, or one it
/// retained), released on drop.
struct Owned(CFTypeRef);

impl Owned {
    fn new(cf: CFTypeRef) -> Option<Owned> {
        (!cf.is_null()).then_some(Owned(cf))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: a non-null object this code owns, released exactly once.
        unsafe { CFRelease(self.0) };
    }
}

fn cf_string(s: &str) -> Result<Owned> {
    // SAFETY: `s` is valid UTF-8 of the given length; the result is a Create,
    // ours.
    let cf = unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            s.as_ptr(),
            s.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
            0,
        )
    };
    Owned::new(cf).ok_or_else(|| anyhow!("CFStringCreateWithBytes({s}) failed"))
}

/// A borrowed CFString as a Rust string; `None` for null or another type.
fn string_of(cf: CFTypeRef) -> Option<String> {
    // SAFETY: a type check on a live object (or null, ruled out first), then
    // the string getters on what is known to be a CFString, into a buffer of
    // the size CoreFoundation itself asked for.
    unsafe {
        if cf.is_null() || CFGetTypeID(cf) != CFStringGetTypeID() {
            return None;
        }
        let cap =
            CFStringGetMaximumSizeForEncoding(CFStringGetLength(cf), K_CF_STRING_ENCODING_UTF8);
        if cap < 0 {
            return None;
        }
        let mut buf = vec![0u8; cap as usize + 1];
        if CFStringGetCString(
            cf,
            buf.as_mut_ptr().cast(),
            buf.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
        ) == 0
        {
            return None;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(end);
        Some(String::from_utf8_lossy(&buf).into_owned())
    }
}

fn number_i64(cf: CFTypeRef) -> Option<i64> {
    let mut v: i64 = 0;
    // SAFETY: a type check on a live object (or null, ruled out first), then a
    // read into a local of the type asked for.
    unsafe {
        if cf.is_null() || CFGetTypeID(cf) != CFNumberGetTypeID() {
            return None;
        }
        CFNumberGetValue(cf, K_CF_NUMBER_SINT64_TYPE, (&mut v as *mut i64).cast());
    }
    Some(v)
}

fn number_f64(cf: CFTypeRef) -> Option<f64> {
    let mut v: f64 = 0.0;
    // SAFETY: as `number_i64`.
    unsafe {
        if cf.is_null() || CFGetTypeID(cf) != CFNumberGetTypeID() {
            return None;
        }
        CFNumberGetValue(cf, K_CF_NUMBER_FLOAT64_TYPE, (&mut v as *mut f64).cast());
    }
    Some(v)
}

fn boolean(cf: CFTypeRef) -> Option<bool> {
    // SAFETY: a type check on a live object (or null, ruled out first).
    unsafe {
        if cf.is_null() || CFGetTypeID(cf) != CFBooleanGetTypeID() {
            return None;
        }
        Some(CFBooleanGetValue(cf) != 0)
    }
}

/// The window server's list: [`ON_SCREEN`] front to back, or every window.
fn quartz_windows(option: u32) -> Result<Vec<QuartzWindow>> {
    // SAFETY: a Copy, ours; null is handled.
    let list = Owned::new(unsafe { CGWindowListCopyWindowInfo(option, K_CG_NULL_WINDOW_ID) })
        .ok_or_else(|| anyhow!("the window server returned no window list"))?;
    // SAFETY: a live array; its dictionaries are borrowed (Get rule) and read
    // only while it lives, with the framework's own key constants.
    unsafe {
        let n = CFArrayGetCount(list.0);
        let mut out = Vec::with_capacity(n.max(0) as usize);
        for i in 0..n {
            let d = CFArrayGetValueAtIndex(list.0, i);
            let get = |key: CFTypeRef| CFDictionaryGetValue(d, key);
            let mut r = CGRect::default();
            let b = get(kCGWindowBounds);
            if !b.is_null() {
                CGRectMakeWithDictionaryRepresentation(b, &mut r);
            }
            out.push(QuartzWindow {
                id: number_i64(get(kCGWindowNumber)).unwrap_or(0) as u32,
                pid: number_i64(get(kCGWindowOwnerPID)).unwrap_or(-1) as i32,
                layer: number_i64(get(kCGWindowLayer)).unwrap_or(-1),
                owner: string_of(get(kCGWindowOwnerName)).unwrap_or_default(),
                name: string_of(get(kCGWindowName)),
                on_screen: boolean(get(kCGWindowIsOnscreen)).unwrap_or(false),
                alpha: number_f64(get(kCGWindowAlpha)).unwrap_or(1.0),
                bounds: (r.origin.x, r.origin.y, r.size.width, r.size.height),
            });
        }
        Ok(out)
    }
}

/// The owner pid of the window in front: the first layer-0 window on screen.
fn front_pid() -> Option<i32> {
    quartz_windows(ON_SCREEN)
        .ok()?
        .into_iter()
        .find(|w| w.layer == 0)
        .map(|w| w.pid)
}

fn on_screen(id: u32) -> bool {
    quartz_windows(ON_SCREEN).is_ok_and(|ws| ws.iter().any(|w| w.id == id))
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// The AX names a focus uses, made once per focus.
struct Names {
    windows: Owned,
    title: Owned,
    position: Owned,
    size: Owned,
    minimized: Owned,
    hidden: Owned,
    frontmost: Owned,
    main: Owned,
    raise: Owned,
}

impl Names {
    fn new() -> Result<Names> {
        Ok(Names {
            windows: cf_string("AXWindows")?,
            title: cf_string("AXTitle")?,
            position: cf_string("AXPosition")?,
            size: cf_string("AXSize")?,
            minimized: cf_string("AXMinimized")?,
            hidden: cf_string("AXHidden")?,
            frontmost: cf_string("AXFrontmost")?,
            main: cf_string("AXMain")?,
            raise: cf_string("AXRaise")?,
        })
    }
}

fn ax_copy(element: CFTypeRef, attribute: &Owned) -> Option<Owned> {
    let mut v: CFTypeRef = std::ptr::null();
    // SAFETY: a live element and attribute name; a successful Copy is ours.
    let e = unsafe { AXUIElementCopyAttributeValue(element, attribute.0, &mut v) };
    if e == AX_SUCCESS { Owned::new(v) } else { None }
}

fn ax_bool(element: CFTypeRef, attribute: &Owned) -> Option<bool> {
    ax_copy(element, attribute).and_then(|v| boolean(v.0))
}

fn ax_set_bool(element: CFTypeRef, attribute: &Owned, value: bool) -> AXError {
    // SAFETY: a live element and attribute name, and CoreFoundation's own
    // boolean constants.
    unsafe {
        let v = if value {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        };
        AXUIElementSetAttributeValue(element, attribute.0, v)
    }
}

/// An AX point or size attribute as two numbers.
fn ax_pair(element: CFTypeRef, attribute: &Owned, the_type: u32) -> Option<(f64, f64)> {
    let v = ax_copy(element, attribute)?;
    let mut out = [0f64; 2];
    // SAFETY: CGPoint and CGSize are both two f64s, the layout of `out`.
    let ok = unsafe { AXValueGetValue(v.0, the_type, out.as_mut_ptr().cast()) } != 0;
    ok.then_some((out[0], out[1]))
}

type GetWindow = unsafe extern "C" fn(CFTypeRef, *mut u32) -> AXError;

/// `_AXUIElementGetWindow`: the CGWindowID behind an AX window. Private, but
/// what every window manager on the Mac uses, and present since 10.x. Looked
/// up at run time rather than linked, so a macOS without it costs the exact
/// match (the title and frame still find the window), never the agent's start.
fn get_window_fn() -> Option<GetWindow> {
    struct Sym(*mut c_void);
    // SAFETY: a code address, immutable for the life of the process.
    unsafe impl Send for Sym {}
    unsafe impl Sync for Sym {}
    static SYM: OnceLock<Sym> = OnceLock::new();
    let sym = SYM.get_or_init(|| {
        // SAFETY: a lookup in the already-loaded images by a NUL-terminated name.
        Sym(unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"_AXUIElementGetWindow".as_ptr()) })
    });
    // SAFETY: when present, the symbol has this signature (HIServices'
    // `AXError _AXUIElementGetWindow(AXUIElementRef, CGWindowID *)`).
    (!sym.0.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, GetWindow>(sym.0) })
}

/// The AX window that is the window server's `target`, retained.
fn ax_window(app: &Owned, target: &QuartzWindow, n: &Names) -> Result<Owned> {
    let windows = ax_copy(app.0, &n.windows).ok_or_else(|| {
        anyhow!(
            "{} does not list its windows to the Accessibility API",
            target.owner
        )
    })?;
    // SAFETY: a type check, then reads of a live array whose elements are
    // borrowed and retained before the array goes.
    unsafe {
        if CFGetTypeID(windows.0) != CFArrayGetTypeID() {
            bail!("{}'s AXWindows is not a list", target.owner);
        }
        let get = get_window_fn();
        let mut by_frame: CFTypeRef = std::ptr::null();
        for i in 0..CFArrayGetCount(windows.0) {
            let w = CFArrayGetValueAtIndex(windows.0, i);
            if let Some(f) = get {
                let mut id = 0u32;
                if f(w, &mut id) == AX_SUCCESS && id == target.id {
                    return Ok(Owned(CFRetain(w)));
                }
            }
            if by_frame.is_null() && same_window(w, target, n) {
                by_frame = w;
            }
        }
        if by_frame.is_null() {
            bail!(
                "{}'s window {} is not one the Accessibility API can reach",
                target.owner,
                target.id
            );
        }
        Ok(Owned(CFRetain(by_frame)))
    }
}

/// The match without `_AXUIElementGetWindow`: the same title and the same
/// frame. Both APIs give the frame in global points from the top left
/// (measured equal).
fn same_window(w: CFTypeRef, target: &QuartzWindow, n: &Names) -> bool {
    let title = ax_copy(w, &n.title).and_then(|t| string_of(t.0));
    let (x, y, width, height) = target.bounds;
    title.as_deref() == target.name.as_deref()
        && ax_pair(w, &n.position, K_AX_VALUE_CG_POINT_TYPE)
            .is_some_and(|(px, py)| px.round() == x.round() && py.round() == y.round())
        && ax_pair(w, &n.size, K_AX_VALUE_CG_SIZE_TYPE)
            .is_some_and(|(sw, sh)| sw.round() == width.round() && sh.round() == height.round())
}

/// The person's own temporary directory (`/var/folders/…/T/`, mode 0700), the
/// one `getconf DARWIN_USER_TEMP_DIR` prints. `TMPDIR` cannot be trusted to
/// carry it: `sudo` resets a supervised worker's environment.
fn person_temp_dir() -> PathBuf {
    let mut buf = vec![0 as c_char; 1024];
    // SAFETY: the buffer and its length; `confstr` NUL-terminates within it
    // and returns the size it needed, checked before the buffer is read.
    let need =
        unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
    if need > 0 && need <= buf.len() {
        // SAFETY: NUL-terminated inside `buf`, per the size check above.
        let dir = unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    std::env::temp_dir()
}

/// Write a terminal entry's script, executable by the person alone, under a
/// name nobody else has taken (`create_new` refuses an existing file, or a
/// symlink planted where the file would go).
fn write_script(key: &str, body: &str) -> Result<PathBuf> {
    let dir = person_temp_dir();
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (u64::from(std::process::id()) << 32);
    for attempt in 0..8u64 {
        let path = dir.join(super::terminal_script_name(key, base.wrapping_add(attempt)));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(body.as_bytes())
                    .with_context(|| format!("writing {}", path.display()))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("creating {}", path.display())));
            }
        }
    }
    bail!("could not create a launch script in {}", dir.display())
}

/// Run a launcher and judge it within [`LAUNCH_WAIT`]: an exit is its answer
/// (a failure says what it wrote to stderr), and one still running at the
/// deadline is launched. Either way the child is waited for, so a launch never
/// leaves a zombie behind.
fn run_launcher<S: AsRef<OsStr>>(program: &str, args: &[S]) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    // Read stderr on its own thread: the first 4 KB go back over a channel, and
    // the rest is drained, so a chatty long-running program never blocks on a
    // full pipe. ⚠️ Never joined: a program that leaves a child of its own
    // holding the pipe would keep the thread reading past its own exit, so the
    // answer waits for the first 4 KB a bounded time and no longer.
    let stderr = child.stderr.take();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        if let Some(mut e) = stderr {
            let mut kept = Vec::new();
            let _ = (&mut e).take(4096).read_to_end(&mut kept);
            let _ = tx.send(String::from_utf8_lossy(&kept).into_owned());
            let _ = std::io::copy(&mut e, &mut std::io::sink());
        }
    });
    let end = Instant::now() + LAUNCH_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let said = rx
                    .recv_timeout(Duration::from_millis(500))
                    .unwrap_or_default();
                let said = said.trim();
                if said.is_empty() {
                    bail!("{program} exited with {status}");
                }
                bail!("{program} exited with {status}: {said}");
            }
            Ok(None) if Instant::now() < end => std::thread::sleep(POLL),
            Ok(None) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
            Err(e) => return Err(anyhow::Error::new(e).context(format!("waiting for {program}"))),
        }
    }
}

/// Whether this process can serve Remote Apps: only in a window-server
/// session. A supervised Mac's root daemon is never in one, and while somebody
/// is logged in its GUI worker serves them; the daemon's hello carries the
/// worker's words (`DelegateHost::effective_apps`).
pub(super) fn availability() -> Result<(), Unavailable> {
    if tcc::has_gui_session() {
        Ok(())
    } else {
        Err(Unavailable::NoSession {
            detail: "this process has no window-server session: it is a Mac's root daemon, and \
                     while somebody is logged in, the GUI worker in their session serves Remote \
                     Apps"
                .to_string(),
        })
    }
}

pub struct MacWm;

impl WindowManager for MacWm {
    /// The window server lists every window, so nothing is unlisted, except
    /// the titles when this process has no Screen Recording grant: the list
    /// then names each window's app only.
    fn coverage(&self) -> Coverage {
        Coverage {
            sources: vec!["quartz"],
            unlisted: (!tcc::screen_recording_granted()).then(|| {
                "window titles: this agent has no Screen Recording permission, so macOS names \
                 each window's app only (System Settings → Privacy & Security → Screen \
                 Recording)"
                    .to_string()
            }),
            missing_tools: Vec::new(),
        }
    }

    fn list(&self) -> Result<Vec<WindowInfo>> {
        let front = quartz_windows(ON_SCREEN)?;
        let all = quartz_windows(K_CG_WINDOW_LIST_OPTION_ALL)?;
        Ok(super::quartz_listing(&front, &all))
    }

    fn focus(&self, window_id: &str) -> Result<()> {
        let id =
            super::parse_quartz_window_id(window_id).ok_or_else(|| anyhow!("invalid window id"))?;
        if !tcc::accessibility_trusted() {
            bail!(
                "this agent has no Accessibility permission, and bringing another app's window \
                 forward needs it (System Settings → Privacy & Security → Accessibility)"
            );
        }
        let target = quartz_windows(K_CG_WINDOW_LIST_OPTION_ALL)?
            .into_iter()
            .find(|w| w.id == id)
            .ok_or_else(|| anyhow!("window {id} is gone; refresh the list"))?;
        let n = Names::new()?;
        // SAFETY: a Create, ours; null is handled.
        let app = Owned::new(unsafe { AXUIElementCreateApplication(target.pid) })
            .ok_or_else(|| anyhow!("no Accessibility element for {}", target.owner))?;
        let win = ax_window(&app, &target, &n)?;
        let minimised = ax_bool(win.0, &n.minimized) == Some(true);
        if ax_bool(app.0, &n.hidden) == Some(true) {
            ax_set_bool(app.0, &n.hidden, false);
            // A minimised window stays off screen until it is restored, below.
            if !minimised {
                wait_until(STEP_WAIT, || on_screen(id));
            }
        }
        if minimised {
            ax_set_bool(win.0, &n.minimized, false);
            wait_until(STEP_WAIT, || on_screen(id));
        }
        let mut said = (0, 0, 0);
        for _ in 0..2 {
            let front = ax_set_bool(app.0, &n.frontmost, true);
            // SAFETY: a live element and action name.
            let raise = unsafe { AXUIElementPerformAction(win.0, n.raise.0) };
            let main = ax_set_bool(win.0, &n.main, true);
            said = (front, raise, main);
            if wait_until(Duration::from_secs(1), || front_pid() == Some(target.pid)) {
                return Ok(());
            }
        }
        bail!(
            "macOS did not bring {} forward (AXFrontmost {}, AXRaise {}, AXMain {})",
            super::quartz_title(&target.owner, target.name.as_deref()),
            said.0,
            said.1,
            said.2
        )
    }

    fn launch(&self, app: &ResolvedApp) -> Result<LaunchOutcome> {
        if app.terminal || app.tmux {
            // ⚠️ `tmux` is not used on a Mac: the stock system has none, and a
            // shell in a Terminal window is what the entry asks for. Its
            // `session` stays `None`.
            let body = super::terminal_script(&app.key, &app.command)
                .ok_or_else(|| anyhow!("empty command"))?;
            let path = write_script(&app.key, &body)?;
            let opened = run_launcher(
                OPEN,
                &[
                    OsStr::new("-b"),
                    OsStr::new(TERMINAL_BUNDLE_ID),
                    path.as_os_str(),
                ],
            );
            if opened.is_err() {
                // Terminal never ran it, so it never deleted itself.
                let _ = std::fs::remove_file(&path);
            }
            opened?;
            return Ok(LaunchOutcome::default());
        }
        let (program, rest) = app
            .command
            .split_first()
            .ok_or_else(|| anyhow!("empty command"))?;
        run_launcher(program, rest)?;
        Ok(LaunchOutcome::default())
    }
}

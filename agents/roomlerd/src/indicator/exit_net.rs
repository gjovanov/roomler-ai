// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #4119 — the net under the macOS consent panel: an exit that AppKit starts
//! inside one of our window calls, on the thread making that call, and that
//! our own code did not choose, leaves with a code of its own instead of 0
//! ([`crate::watchdog::APPKIT_ENDED_EXIT_CODE`]).
//!
//! After a log-out that killed its window-server port, a two-row Mac's root
//! half made a window at its next question (#4053's probe), and AppKit ended
//! the process with exit 0 while it made it (0.4.228 release lab, two runs:
//! `_CGSCreateWindow returned 268435459`, then launchd's `last exit code =
//! 0`, with no line of ours). launchd relaunches the daemon only after a
//! non-zero exit (`KeepAlive {SuccessfulExit: false}`), so the Mac's root
//! daemon stayed down until the next boot. The panel's gate now makes no
//! window once it knows of a log-out ([`super::panel_gate`]); this is the net
//! under it, for a log-out the gate did not see.
//!
//! - [`inside`] marks the calling thread as inside one of our AppKit calls
//!   for as long as its guard lives: the probe window, the panel's window,
//!   and the pump's event fetch and dispatch (`super::mac`). Only the main
//!   thread makes those calls.
//! - [`arm`] registers an `atexit` hook, once, at the AppKit pump's start, in
//!   a root process only ([`arms_in`]).
//! - `exit` runs that hook on the thread that called `exit`. The hook acts
//!   only when THAT thread is inside one of our calls, and the exit was not
//!   chosen ([`exit_chosen`]): it writes one line to stderr with a raw
//!   `write(2)` (the plist sends stderr to `daemon.log`, and the log's file
//!   layer may not flush in time), then `_exit`s with [`NET_EXIT_CODE`].
//!
//! ⚠️ **The thread, never the mark alone.** The daemon runs on its
//! `roomlerd-main` thread, and its clean stop after a SIGTERM (#3195) calls
//! `exit(0)` there, while the main thread sits in the event fetch nearly all
//! the time. A mark the whole process shared would turn that stop into a
//! restart. The mark is the main thread's own, so the stop's hook finds none
//! on `roomlerd-main` and leaves its 0, as it leaves the requested restart's
//! code, an error's 1, the quit backstop's and the watchdog's.
//!
//! A log-out's quit from loginwindow ends with AppKit's exit 0 on purpose
//! (#3164's table in `crate::macos_quit`): the delegate marks that exit
//! chosen before it answers `NSTerminateNow`, and the hook leaves it alone.
//!
//! The code is the net's own, never the requested restart's 9, which says no
//! failure happened: the net marks no clean shutdown, so a run it ends before
//! the clean-run threshold counts as a crash at the next start, as it should
//! ([`crate::watchdog::APPKIT_ENDED_EXIT_CODE`] has the reasons and the value).
//!
//! What the net cannot see: an `_exit` (atexit hooks do not run), a signal,
//! and an exit from a thread that is not inside our calls. If AppKit ends a
//! process that way, the gate's two reads before any window (the console
//! session, the seat) are what hold. The mark and the chosen flag are
//! read in an `atexit` hook, so both are plain values: a `const` thread-local
//! cell with no destructor, and an atomic.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

thread_local! {
    /// The AppKit call this thread is inside, while it is inside one. A
    /// `const` cell of a type with no destructor: no destructor is
    /// registered for it, so the hook reads it during `exit`, after the
    /// calling thread's destructors ran.
    static INSIDE: Cell<Option<&'static str>> = const { Cell::new(None) };
}

/// Set when our own code chose the process's end: the delegate's answer to
/// a quit it lets AppKit carry out (`NSTerminateNow`), after which AppKit
/// exits 0 on purpose.
static CHOSEN: AtomicBool = AtomicBool::new(false);

/// The code the net leaves with: its own, never the requested restart's 9.
/// launchd's `KeepAlive {SuccessfulExit: false}` relaunches it after its
/// throttle, as it does any non-zero exit.
const NET_EXIT_CODE: i32 = crate::watchdog::APPKIT_ENDED_EXIT_CODE;

/// Whether a process with this effective uid arms the net: a root process
/// only (a one-row Mac's daemon, a two-row Mac's root half), whose exit 0
/// leaves it down until the next boot. A per-user process's exit 0 at a
/// log-out is what its session's end should bring, and `RunAtLoad` starts it
/// at the next login (#3164's loginwindow row): a relaunch there would put
/// it back into a session that is ending.
pub(crate) fn arms_in(euid: u32) -> bool {
    euid == 0
}

/// Registers the hook, once a process. Before the first window call the
/// main thread makes ([`super::mac`]'s `run_main_loop`).
pub(crate) fn arm() {
    static ARMED: Once = Once::new();
    ARMED.call_once(|| {
        // SAFETY: `the_net` is an `extern "C" fn()` that lives as long as the
        // program, which is what `atexit` needs.
        let status = unsafe { libc::atexit(the_net) };
        if status != 0 {
            tracing::warn!(
                status,
                "macOS consent panel: the exit net could not be registered, so an exit AppKit \
                 starts in the panel's window calls keeps AppKit's code (#4119)"
            );
        }
    });
}

/// While this lives, the thread that made it is inside `call`, one of our
/// AppKit calls. Dropped on that thread: the mark is that thread's.
#[must_use = "the call is marked only while the guard lives"]
pub(crate) struct Inside {
    was: Option<&'static str>,
    _this_thread: PhantomData<*const ()>,
}

/// Marks the calling thread as inside `call` until the guard drops. Nests:
/// the guard puts back what it found.
pub(crate) fn inside(call: &'static str) -> Inside {
    let was = INSIDE.with(|mark| mark.replace(Some(call)));
    Inside {
        was,
        _this_thread: PhantomData,
    }
}

impl Drop for Inside {
    fn drop(&mut self) {
        INSIDE.with(|mark| mark.set(self.was));
    }
}

/// The process's end is ours: an exit from here on keeps its code, wherever
/// it starts. The delegate's `NSTerminateNow` (#3164).
pub(crate) fn exit_chosen() {
    CHOSEN.store(true, Ordering::SeqCst);
}

/// The `atexit` hook. On the thread that called `exit`: inside one of our
/// calls, and not chosen, it says so and leaves with [`NET_EXIT_CODE`];
/// otherwise it does nothing, and the exit goes on with the code it began with.
extern "C" fn the_net() {
    let Some(call) = INSIDE.try_with(Cell::get).ok().flatten() else {
        return;
    };
    if CHOSEN.load(Ordering::SeqCst) {
        return;
    }
    say_on_stderr(call);
    // SAFETY: `_exit` ends the process at once, running no other hook. That
    // is the point: the exit had already begun, and its code is replaced.
    unsafe { libc::_exit(NET_EXIT_CODE) }
}

// The line below names the code: renumber both, or neither.
const _: () = assert!(NET_EXIT_CODE == 32);

/// One line on stderr, in one raw `write(2)`: no lock, no allocation.
fn say_on_stderr(call: &str) {
    const HEAD: &[u8] = b"roomlerd: an exit began inside ";
    const TAIL: &[u8] = b" on the AppKit thread, and roomlerd did not choose it (AppKit, on a \
        window-server port a log-out killed); leaving with 32 (APPKIT_ENDED_EXIT_CODE) so launchd \
        starts roomlerd again (#4119)\n";
    let mut line = [0u8; 512];
    let mut len = 0;
    for part in [HEAD, call.as_bytes(), TAIL] {
        let take = part.len().min(line.len() - len);
        line[len..len + take].copy_from_slice(&part[..take]);
        len += take;
    }
    // SAFETY: fd 2, and a buffer of `len` initialised bytes.
    let _ = unsafe { libc::write(2, line.as_ptr().cast(), len) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watchdog::{APPKIT_ENDED_EXIT_CODE, RESTART_REQUESTED_EXIT_CODE};
    use std::process::{Command, Stdio};

    /// Which exit a copy of this test binary plays.
    const MODE: &str = "ROOMLERD_TEST_EXIT_NET";
    /// The test that plays it.
    const STAND_IN: &str = "indicator::exit_net::tests::the_stand_in";
    /// What the stand-in marks, as `super::mac` marks a window call.
    const CALL: &str = "a test's window";
    /// The modes where another thread is marked while the daemon thread
    /// exits, followed by the code it exits with.
    const ANOTHER_THREAD: &str = "another thread inside, exit ";

    /// In a copy of this test binary, with [`MODE`] set: arm the net, then
    /// leave the way the mode says. Without it, nothing. AppKit calls
    /// libc's `exit` itself; the daemon's own stops call `std::process::exit`.
    #[test]
    fn the_stand_in() {
        let Ok(mode) = std::env::var(MODE) else {
            return;
        };
        arm();
        arm();
        match mode.as_str() {
            "inside" => {
                let _inside = inside(CALL);
                // SAFETY: ends this stand-in process.
                unsafe { libc::exit(0) }
            }
            "outside" => {
                // SAFETY: ends this stand-in process.
                unsafe { libc::exit(0) }
            }
            "after the call" => {
                drop(inside(CALL));
                // SAFETY: ends this stand-in process.
                unsafe { libc::exit(0) }
            }
            "inside, chosen" => {
                let _inside = inside(CALL);
                exit_chosen();
                // SAFETY: ends this stand-in process.
                unsafe { libc::exit(0) }
            }
            other if other.starts_with(ANOTHER_THREAD) => {
                let code: i32 = other
                    .strip_prefix(ANOTHER_THREAD)
                    .and_then(|code| code.parse().ok())
                    .expect("the code the daemon thread exits with");
                let (marked, is_marked) = std::sync::mpsc::channel();
                // The AppKit thread, inside the event fetch.
                std::thread::spawn(move || {
                    let _inside = inside(CALL);
                    marked.send(()).expect("the test waits for the mark");
                    loop {
                        std::thread::park();
                    }
                });
                is_marked.recv().expect("the mark");
                // The daemon thread's own exit (`app.rs`'s `main`): its clean
                // stop, a requested restart, an error.
                std::process::exit(code)
            }
            other => panic!("no such mode: {other}"),
        }
    }

    /// How a stand-in playing `mode` ended: its exit code, and its stderr.
    fn played(mode: &str) -> (Option<i32>, String) {
        let out = Command::new(std::env::current_exe().expect("this test binary"))
            .args([STAND_IN, "--exact", "--nocapture", "--test-threads=1"])
            .env(MODE, mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("a copy of this test binary");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        (out.status.code(), stderr)
    }

    /// #4119 (c) — an exit that begins inside one of our AppKit calls, on
    /// the thread making it, and that our code did not choose (AppKit's
    /// exit 0 on a dead window-server port), leaves with the net's own code,
    /// [`APPKIT_ENDED_EXIT_CODE`], which launchd relaunches, after ONE line on
    /// stderr that names the call and the code. Armed twice, it is still one
    /// line.
    ///
    /// Red before #4119: the stand-in left with 0, which launchd does not
    /// relaunch. Red with the net's first code: it left with 9, the requested
    /// restart's, which says no failure happened.
    #[test]
    fn an_exit_inside_our_call_that_we_did_not_choose_leaves_with_its_own_code() {
        let (code, stderr) = played("inside");
        assert_eq!(code, Some(APPKIT_ENDED_EXIT_CODE), "{stderr}");
        let said: Vec<&str> = stderr
            .lines()
            .filter(|l| l.contains("an exit began inside"))
            .collect();
        assert_eq!(said.len(), 1, "{stderr}");
        assert!(said[0].contains(CALL), "{stderr}");
        assert!(
            said[0].contains(&format!("leaving with {APPKIT_ENDED_EXIT_CODE} ")),
            "{stderr}"
        );
    }

    /// #4119 (c) — every exit the net must leave alone keeps its code, and
    /// says nothing: one outside our calls, one after the call returned, one
    /// our code chose (the delegate's `NSTerminateNow` for a log-out's quit),
    /// and the daemon thread's exits while the AppKit thread is inside the
    /// event fetch: the clean stop after a SIGTERM (#3195), a requested
    /// restart's [`RESTART_REQUESTED_EXIT_CODE`], and another code.
    #[test]
    fn an_exit_the_net_leaves_alone_keeps_its_code() {
        let mut modes = vec![
            ("outside".to_string(), 0),
            ("after the call".to_string(), 0),
            ("inside, chosen".to_string(), 0),
        ];
        for code in [0, RESTART_REQUESTED_EXIT_CODE, 3] {
            modes.push((format!("{ANOTHER_THREAD}{code}"), code));
        }
        for (mode, expected) in modes {
            let (code, stderr) = played(&mode);
            assert_eq!(code, Some(expected), "{mode}: {stderr}");
            assert!(!stderr.contains("an exit began inside"), "{mode}: {stderr}");
        }
    }

    /// #4119 — only a root process arms the net: a one-row Mac's daemon and
    /// a two-row Mac's root half run as root (0), a per-user half as its
    /// person (501).
    #[test]
    fn only_a_root_process_arms_the_net() {
        assert!(arms_in(0), "a root process");
        assert!(!arms_in(501), "a per-user process");
    }
}

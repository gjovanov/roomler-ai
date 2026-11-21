// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-150 P2 (#3295) — what the launcher does when the daemon ends, and how
//! long it waits before it starts it again. Pure and on every OS, so it is
//! tested everywhere; `main.rs` is the Windows glue around it.
//!
//! The table is one more row of `Install`'s (`agents/roomlerd/src/service.rs`),
//! on purpose. roomlerd's `the_launchers_ends_agree_with_the_install_table`
//! compiles this file and holds each code to the daemon's own constant, so the
//! two cannot drift. This file therefore names no crate and has no tests of its
//! own: they are in `main.rs`.

use std::ffi::OsStr;
use std::time::Duration;

/// A clean stop: `service stop`, the MSI's `StopOldDaemon`, an update's exit.
pub const CLEAN_EXIT: i32 = 0;
/// The server removed the device (`watchdog::AGENT_DELETED_EXIT_CODE`). The
/// launcher leaves, as the Linux units do (FR-150 D3).
pub const AGENT_DELETED: i32 = 7;
/// Another daemon holds the lock (`watchdog::ALREADY_RUNNING_EXIT_CODE`).
pub const ALREADY_RUNNING: i32 = 8;
/// A requested restart, whose caller starts the task again
/// (`watchdog::RESTART_REQUESTED_EXIT_CODE`).
pub const RESTART_REQUESTED: i32 = 9;
/// No enrollment (`daemon_error::NO_ENROLLMENT_EXIT_CODE`): nothing to run
/// until `roomlerd enroll`.
pub const NO_ENROLLMENT: i32 = 78;
/// An install in flight at the daemon's start
/// (`daemon_error::INSTALL_IN_FLIGHT_EXIT_CODE`): the new task's one-shots
/// start the daemon after the install.
pub const INSTALL_IN_FLIGHT: i32 = 75;
/// The launcher could not start the daemon at all. 127 is the shell's
/// command-not-found, as in `roomler-cli-shim`.
pub const NO_DAEMON: i32 = 127;

/// The backoff before a restart: the service supervisor's
/// (`win_service::supervisor::next_backoff`), 2 s doubling to 60 s.
pub const BACKOFF_INITIAL: Duration = Duration::from_secs(2);
/// See [`BACKOFF_INITIAL`].
pub const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// A run at least this long was healthy, so the crash that ends it starts the
/// backoff again from [`BACKOFF_INITIAL`]: the daemon's own clean-run
/// threshold, after which it no longer counts its run as a crash either.
pub const HEALTHY_RUN: Duration = Duration::from_secs(300);

/// FR-150 D4, the kill switch: `ROOMLERDW_SUPERVISE=0` in the person's
/// environment, which the task's process inherits, makes the launcher wait for
/// the daemon once and leave with its code, as before P2.
pub const SUPERVISE_ENV: &str = "ROOMLERDW_SUPERVISE";

/// What the launcher does when the daemon has ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// Leave with the daemon's code: a deliberate end, or a session that is
    /// ending. Named for the tests.
    Leave(&'static str),
    /// A crash: start the daemon again after the backoff.
    Restart,
}

/// The launcher's answer to the daemon's exit `code`. `session_ending` is
/// `SM_SHUTTINGDOWN`, read after the exit: a daemon is never started into a
/// closing session, whatever its code.
pub fn end_of(code: i32, session_ending: bool) -> End {
    if session_ending {
        return End::Leave("session-ending");
    }
    match code {
        CLEAN_EXIT => End::Leave("clean-exit"),
        AGENT_DELETED => End::Leave("agent-deleted"),
        ALREADY_RUNNING => End::Leave("already-running"),
        RESTART_REQUESTED => End::Leave("restart-requested"),
        NO_ENROLLMENT => End::Leave("no-enrollment"),
        INSTALL_IN_FLIGHT => End::Leave("install-in-flight"),
        // 1, a stall's 2, a panic's 101, an exception code: a crash.
        _ => End::Restart,
    }
}

/// The wait before the next start after `failures` failures in a row: zero for
/// none, else 2 s doubling, capped at 60 s.
pub fn backoff(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let factor = 1u64
        .checked_shl(failures.saturating_sub(1))
        .unwrap_or(u64::MAX);
    Duration::from_secs(BACKOFF_INITIAL.as_secs().saturating_mul(factor)).min(BACKOFF_CAP)
}

/// Whether the launcher was asked to run the daemon itself, which is the one
/// start it supervises: exactly `run --supervisor task`, compared by equality.
/// The launcher also runs `--version` for the launcher check and
/// `service start` for the start task; each of those is waited for once, as
/// before, because a loop would run a failed one again for ever.
pub fn is_the_daemons_run<S: AsRef<OsStr>>(args: &[S]) -> bool {
    let want = ["run", "--supervisor", "task"];
    args.len() == want.len() && args.iter().zip(want).all(|(a, w)| a.as_ref() == w)
}

/// [`SUPERVISE_ENV`]'s value: anything but `0` keeps the loop, and unset is
/// the default.
pub fn supervise_wanted(value: Option<&OsStr>) -> bool {
    value != Some(OsStr::new("0"))
}

/// What the loop needs from the OS, so the tests can stand in for it.
pub trait Seam {
    /// A started daemon.
    type Daemon;
    /// Start the daemon; `None` when it could not be started.
    fn spawn(&mut self) -> Option<Self::Daemon>;
    /// Wait for it to end, and its exit code.
    fn wait(&mut self, daemon: Self::Daemon) -> i32;
    /// `SM_SHUTTINGDOWN`: the session is ending.
    fn session_ending(&self) -> bool;
    /// Wait before the next start.
    fn sleep(&mut self, wait: Duration);
    /// A monotonic clock.
    fn now(&self) -> Duration;
}

/// The launcher, given its arguments and [`SUPERVISE_ENV`]'s value: the
/// daemon's own run is supervised ([`supervise`]) unless the switch is off;
/// every other verb, and the daemon's run with the switch off, is waited for
/// once ([`wait_once`]).
pub fn launch<S: Seam, A: AsRef<OsStr>>(args: &[A], switch: Option<&OsStr>, seam: &mut S) -> i32 {
    if is_the_daemons_run(args) && supervise_wanted(switch) {
        supervise(seam)
    } else {
        wait_once(seam)
    }
}

/// Start the daemon, wait for it once, and leave with its code: the launcher
/// before P2, and still every verb but the daemon's run.
pub fn wait_once<S: Seam>(seam: &mut S) -> i32 {
    match seam.spawn() {
        Some(daemon) => seam.wait(daemon),
        None => NO_DAEMON,
    }
}

/// FR-150 P2 — run the daemon until it ends in a way that is not a crash, and
/// leave with its code. A crash, or a start that failed, is retried after the
/// backoff; a run of [`HEALTHY_RUN`] or more starts the backoff again.
pub fn supervise<S: Seam>(seam: &mut S) -> i32 {
    let mut failures: u32 = 0;
    // The code to leave with if the session ends while the launcher waits to
    // start again: the last daemon's, or [`NO_DAEMON`] after a failed start.
    let mut last = NO_DAEMON;
    loop {
        if seam.session_ending() {
            return last;
        }
        let started = seam.now();
        let Some(daemon) = seam.spawn() else {
            last = NO_DAEMON;
            failures = failures.saturating_add(1);
            seam.sleep(backoff(failures));
            continue;
        };
        let code = seam.wait(daemon);
        last = code;
        match end_of(code, seam.session_ending()) {
            End::Leave(_) => return code,
            End::Restart => {
                let ran = seam.now().saturating_sub(started);
                failures = if ran >= HEALTHY_RUN {
                    1
                } else {
                    failures.saturating_add(1)
                };
                seam.sleep(backoff(failures));
            }
        }
    }
}

// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #4292 — a deliberate end of the daemon is never counted as a crash.
//!
//! Every start marks its run unhealthy and saves the mark
//! (`config::mark_run_starting`, in `app.rs`'s `run_cmd`). Two things take
//! the mark off: a clean stop (`config::mark_clean_shutdown`, at the end of
//! `run_cmd`'s graceful path) and a run that reaches the clean-run threshold
//! (`config::record_clean_run_at`). The next start reads a mark that is still
//! on as a crash on the same boot (`config::previous_run`), and three of those
//! within ten minutes run the crash-loop handling (`config::should_rollback`).
//!
//! Three ends are the daemon's own choice, and none of them goes through the
//! graceful path:
//!
//! | The end | Where | Exit code | Here |
//! |---|---|---|---|
//! | The server removed or refused this device (`rc:goodbye` with `AgentDeleted` or `PolicyRejected`) | `signaling.rs`, the primary loop's fatal-goodbye arm | 7 | [`exit_removed`] |
//! | The third displacement of a duplicate-instance duel, with `ws_replaced_exit = true` | `signaling.rs`, the duel's escalation | 7 | [`exit_duel_lost`] |
//! | macOS asked the app to quit and the delegate let it: a log-out's quit from `loginwindow`, a quit under no supervisor | `indicator/mac.rs`, `should_terminate`'s `NSTerminateNow` | AppKit's 0 | [`mark_quit_by_macos`] |
//!
//! Each one marks its run clean here first, the way a clean stop does: under
//! the daemon-wide write lock, on the file read again, `mark_clean_shutdown`,
//! then a save ([`mark_deliberate_end`], [`exit_deliberately`]). Before #4292
//! none of them did. A device removed in its run's first five minutes kept the
//! mark, enrolling it again kept the file's bookkeeping, and its next start
//! counted a crash: `crash_count` 1 (FR-150's C14 cell).
//!
//! Every other end either marks itself already or is a failure that must keep
//! counting: the watchdog's two exits, the exit net's 32, a stop that wedged
//! past its backstop, a panic, a kill from outside. None of them calls this.
//!
//! ## What a mark never does
//!
//! - **Hold the exit past [`MARK_BUDGET`].** The mark runs on a thread of its
//!   own, and the exit waits for it that long at most, then goes on. A save
//!   left behind ends with the process, and the file stays whole: a save
//!   writes a temporary file and renames it into place. That run then counts
//!   as a crash, as each of these ends did before #4292.
//! - **Bring a config back.** It reads the file with
//!   `config::read_if_present`, never with `config::load`, which restores a
//!   missing or unreadable file from its last good copy and writes that copy
//!   back. A removal that arrives after an uninstall took the file away must
//!   not put the enrollment back. With nothing to read, nothing is written; the
//!   next start's own `load` clears the run marks of a copy it restores
//!   (#4157).
//! - **Write a view.** A GUI worker with no enrollment runs on a view of the
//!   person's file, and its start writes nothing (`StartupWrites::read_only`).
//!   It opens no book here, so its end marks nothing either.
//! - **Write what is already so.** A run past the clean-run threshold, or one
//!   already marked clean, is left as it is.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::config::{self, WriteLock};

/// How long a deliberate end waits for its run's mark before it exits anyway.
/// A mark takes milliseconds: the write lock is held only across another
/// writer's load and save, and the file is small. Two seconds leave room for a
/// slow disk and for the Windows machine folder's repair before a save. After
/// the mark, [`exit_deliberately`] waits only for the log's last lines, about
/// a second at most (`logging::finish`).
pub const MARK_BUDGET: Duration = Duration::from_secs(2);

/// What a deliberate end did to its run's mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marked {
    /// Marked clean and saved: the next start reads the run as `Clean`.
    Clean,
    /// The file already says the run was clean: nothing was written.
    AlreadyClean,
    /// No run to mark: this process is not the daemon, its start has not
    /// marked a run yet, or it runs on a view it never writes.
    NoRun,
    /// No config could be read at the path: nothing was written.
    NoConfig,
    /// The save failed, and why. The next start counts the run as a crash.
    SaveFailed(String),
    /// Not finished within the budget. The next start counts the run as a
    /// crash.
    OutOfTime,
}

/// The run this process marked as starting, and what writing its end takes.
struct Book {
    path: PathBuf,
    /// The daemon-wide write lock (P6), once `run_cmd` has made it.
    lock: OnceLock<WriteLock>,
}

static BOOK: OnceLock<Book> = OnceLock::new();

/// `run_cmd` calls this once the start's mark is saved: from here, a
/// deliberate end marks this run clean. Once a process; a later call changes
/// nothing.
pub fn open(config_path: &Path) {
    let _ = BOOK.set(Book {
        path: config_path.to_path_buf(),
        lock: OnceLock::new(),
    });
}

/// The daemon-wide write lock, so that a mark never lands between another
/// writer's load and its save. Before `run_cmd` makes it, no other writer of
/// this process runs.
pub fn share_write_lock(lock: &WriteLock) {
    if let Some(book) = BOOK.get() {
        let _ = book.lock.set(lock.clone());
    }
}

/// Before a deliberate end: mark this run clean, at most [`MARK_BUDGET`],
/// and say in the log what came of it. `why` names the end.
pub fn mark_deliberate_end(why: &str) -> Marked {
    let Some(book) = BOOK.get() else {
        return Marked::NoRun;
    };
    let marked = mark_within(book.path.clone(), book.lock.get().cloned(), MARK_BUDGET);
    say(&marked, why);
    marked
}

/// End the process with `code`, for a reason of its own: mark the run clean
/// ([`mark_deliberate_end`]), write the log's last lines, then exit. Nothing
/// that a failed mark can do keeps the process from exiting.
pub fn exit_deliberately(code: i32, why: &str) -> ! {
    mark_deliberate_end(why);
    // `exit` drops nothing, and the file layer writes from its own thread.
    crate::logging::finish();
    std::process::exit(code)
}

/// The server removed this device or refused it (`rc:goodbye` with
/// `AgentDeleted` or `PolicyRejected`): the primary loop leaves with
/// [`crate::watchdog::AGENT_DELETED_EXIT_CODE`].
pub fn exit_removed() -> ! {
    exit_deliberately(
        crate::watchdog::AGENT_DELETED_EXIT_CODE,
        "the server removed or refused this device (a fatal goodbye)",
    )
}

/// A third displacement by another process using this `agent_id`, with
/// `ws_replaced_exit = true`: the primary loop leaves with
/// [`crate::watchdog::AGENT_DELETED_EXIT_CODE`].
pub fn exit_duel_lost() -> ! {
    exit_deliberately(
        crate::watchdog::AGENT_DELETED_EXIT_CODE,
        "a third displacement by another process using this agent_id",
    )
}

/// macOS asked the app to quit, and the delegate lets AppKit exit 0: a
/// log-out's quit from `loginwindow`, or a quit with no supervisor to start
/// the daemon again (`macos_quit`'s table). AppKit exits as soon as the
/// delegate answers, so this only marks.
pub fn mark_quit_by_macos() {
    mark_deliberate_end("macOS asked roomlerd to quit, and AppKit exits 0");
}

/// Mark the run clean in the config at `path`, holding `lock` when one is
/// given, and wait for that at most `budget`.
pub(crate) fn mark_within(path: PathBuf, lock: Option<WriteLock>, budget: Duration) -> Marked {
    let (done, marked) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("roomlerd-deliberate-end".into())
        .spawn(move || {
            // A thread of its own, outside any runtime, so the wait for the
            // lock blocks nothing else, and an exit may leave it behind.
            let _guard = lock.as_ref().map(|lock| lock.blocking_lock());
            let _ = done.send(mark(path));
        });
    if let Err(e) = spawned {
        return Marked::SaveFailed(format!(
            "could not start the thread that marks the run: {e}"
        ));
    }
    marked.recv_timeout(budget).unwrap_or(Marked::OutOfTime)
}

/// Mark the run clean in the file as it is now.
fn mark(path: PathBuf) -> Marked {
    let Some(mut cfg) = config::read_if_present(&path) else {
        return Marked::NoConfig;
    };
    if !cfg.last_run_unhealthy {
        return Marked::AlreadyClean;
    }
    config::mark_clean_shutdown(&mut cfg);
    match config::save(&path, &cfg) {
        Ok(()) => Marked::Clean,
        Err(e) => Marked::SaveFailed(format!("{e:#}")),
    }
}

/// The one line a deliberate end leaves about its run's mark.
fn say(marked: &Marked, why: &str) {
    match marked {
        Marked::Clean => tracing::info!(
            why,
            "a deliberate end: this run is marked clean, so the next start does not count it as \
             a crash (#4292)"
        ),
        Marked::AlreadyClean => tracing::info!(
            why,
            "a deliberate end: this run was already marked clean (#4292)"
        ),
        Marked::NoRun => {}
        Marked::NoConfig => tracing::warn!(
            why,
            "a deliberate end: no config could be read, so no run is marked (#4292)"
        ),
        Marked::SaveFailed(error) => tracing::warn!(
            why,
            %error,
            "a deliberate end: this run could not be marked clean, so the next start counts it \
             as a crash (#4292)"
        ),
        Marked::OutOfTime => tracing::warn!(
            why,
            budget_ms = MARK_BUDGET.as_millis() as u64,
            "a deliberate end: marking this run clean took longer than its budget, so the exit \
             goes on and the next start counts the run as a crash (#4292)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, PreviousRun};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::Instant;

    /// The boot every start of these tests runs in: one boot, so a mark that
    /// is still on reads as a crash.
    const BOOT: &str = "boot-7";
    /// A start's clock.
    const NOW: u64 = 1_800_000_000;

    fn write_lock() -> WriteLock {
        Arc::new(tokio::sync::Mutex::new(()))
    }

    /// A config file holding an enrollment, as a device's daemon runs on.
    fn enrolled(dir: &Path) -> PathBuf {
        let path = dir.join("config.toml");
        config::save(&path, &config::test_fixture()).expect("the fixture saves");
        path
    }

    /// One start, through the calls `run_cmd` makes at the top of a run
    /// (`app.rs`): read how the last run ended and count it if it crashed,
    /// then mark this run starting and save. Says how the last run ended.
    fn start_a_run(path: &Path, now: u64) -> PreviousRun {
        let path = path.to_path_buf();
        let mut cfg = config::load(&path).expect("the config loads");
        let previous = config::previous_run(&cfg, Some(BOOT), false);
        if previous == PreviousRun::Crashed {
            config::record_crash_at(&mut cfg, now);
        }
        config::mark_run_starting(&mut cfg);
        cfg.last_run_boot = Some(BOOT.to_string());
        config::save(&path, &cfg).expect("the start's save");
        previous
    }

    fn on_disk(path: &Path) -> AgentConfig {
        config::load(&path.to_path_buf()).expect("the config loads")
    }

    /// #4292 — the bug's cell, as the lab ran it (FR-150's C14): a run
    /// starts, the server removes the device in the run's first five minutes
    /// and the daemon leaves with 7, the device is enrolled again (which
    /// keeps the file's bookkeeping), and it starts. That start reads the last
    /// run as clean, and counts nothing.
    ///
    /// Red with master's line put back in [`exit_removed`] (a bare
    /// `std::process::exit` with the removal's code): the copy still left
    /// with 7, and the start after the enrollment read `Crashed`, at
    /// `crash_count` 1.
    #[test]
    fn a_removal_then_an_enrollment_starts_the_next_run_at_crash_count_0() {
        let dir = tempfile::tempdir().unwrap();
        let path = enrolled(dir.path());
        assert_eq!(start_a_run(&path, NOW), PreviousRun::Clean, "a first start");
        assert!(on_disk(&path).last_run_unhealthy, "the start marks its run");

        let (code, stderr) = ended_in_a_copy("removed", dir.path());
        assert_eq!(
            code,
            Some(crate::watchdog::AGENT_DELETED_EXIT_CODE),
            "{stderr}"
        );

        // Enrolled again on the same server and tenant: a fresh enrollment
        // folded into the file, which keeps its bookkeeping (`..existing`).
        let mut fresh = config::test_fixture();
        fresh.agent_token = "a-new-token".into();
        fresh.agent_id = "a-new-agent-id".into();
        let (folded, outcome) =
            crate::enrollment::apply_enrollment(Some(on_disk(&path)), fresh, None, false)
                .expect("the fold");
        assert_eq!(outcome, crate::enrollment::EnrollOutcome::RefreshedPrimary);
        config::save(&path, &folded).expect("the enrollment's save");

        assert_eq!(
            start_a_run(&path, NOW + 330),
            PreviousRun::Clean,
            "the start after the enrollment; the copy said: {stderr}"
        );
        let cfg = on_disk(&path);
        assert_eq!(cfg.crash_count, 0);
        assert_eq!(cfg.last_crash_unix, 0);
    }

    /// The duel's end leaves with 7 as well, and marks its run the same way.
    #[test]
    fn a_lost_duel_leaves_with_7_and_its_run_marked_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = enrolled(dir.path());
        start_a_run(&path, NOW);
        let (code, stderr) = ended_in_a_copy("duel", dir.path());
        assert_eq!(
            code,
            Some(crate::watchdog::AGENT_DELETED_EXIT_CODE),
            "{stderr}"
        );
        assert_eq!(start_a_run(&path, NOW + 60), PreviousRun::Clean, "{stderr}");
        assert_eq!(on_disk(&path).crash_count, 0);
    }

    /// A quit macOS asked for: the delegate marks the run, then AppKit exits
    /// 0, and the next login's start counts nothing.
    #[test]
    fn a_quit_macos_asked_for_marks_its_run_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = enrolled(dir.path());
        start_a_run(&path, NOW);
        let (code, stderr) = ended_in_a_copy("macos", dir.path());
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(start_a_run(&path, NOW + 60), PreviousRun::Clean, "{stderr}");
        assert_eq!(on_disk(&path).crash_count, 0);
    }

    /// The control: the same run ended the way master ended it, with no mark
    /// before the exit. The next start counts a crash. Without this, a test
    /// above that reads `Clean` could be reading a start that never counts.
    #[test]
    fn an_end_that_marks_nothing_counts_as_a_crash_at_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = enrolled(dir.path());
        start_a_run(&path, NOW);
        assert_eq!(start_a_run(&path, NOW + 330), PreviousRun::Crashed);
        assert_eq!(on_disk(&path).crash_count, 1);
    }

    /// A crash before the deliberate end is history the mark keeps: it takes
    /// off this run's mark, as a clean stop does, and leaves the count.
    #[test]
    fn the_mark_keeps_the_crashes_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = enrolled(dir.path());
        start_a_run(&path, NOW);
        assert_eq!(start_a_run(&path, NOW + 60), PreviousRun::Crashed);
        assert_eq!(mark_within(path.clone(), None, MARK_BUDGET), Marked::Clean);
        assert_eq!(start_a_run(&path, NOW + 120), PreviousRun::Clean);
        let cfg = on_disk(&path);
        assert_eq!(cfg.crash_count, 1, "the crash before the end still counts");
        assert_eq!(cfg.last_crash_unix, NOW + 60);
    }

    /// Deliberate ends never fill the crash window: any number of them in a
    /// row, each inside its run's first minute, on a version not yet
    /// promoted, leave the crash count at 0 and the crash-loop handling with
    /// nothing to do.
    #[test]
    fn deliberate_ends_never_fill_the_crash_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = enrolled(dir.path());
        let mut cfg = on_disk(&path);
        cfg.last_known_good_version = Some("0.4.1".into());
        config::save(&path, &cfg).expect("the save");
        let mut now = NOW;
        for n in 0..6 {
            assert_eq!(start_a_run(&path, now), PreviousRun::Clean, "start {n}");
            assert_eq!(
                mark_within(path.clone(), Some(write_lock()), MARK_BUDGET),
                Marked::Clean,
                "end {n}"
            );
            now += 30;
        }
        assert_eq!(start_a_run(&path, now), PreviousRun::Clean);
        let cfg = on_disk(&path);
        assert_eq!(cfg.crash_count, 0);
        assert_eq!(cfg.last_crash_unix, 0);
        assert!(!config::should_rollback(&cfg, "0.4.2", now));
    }

    /// The mark waits for the daemon's write lock, so it never lands between
    /// another writer's load and save, but never past its budget: with the
    /// lock held, the end goes on without the mark, writes nothing, and the
    /// run counts as a crash, as before #4292.
    #[test]
    fn the_mark_waits_for_the_write_lock_but_never_past_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = enrolled(dir.path());
        start_a_run(&path, NOW);
        let lock = write_lock();
        let held = lock.try_lock().expect("the test holds the lock");
        let started = Instant::now();
        assert_eq!(
            mark_within(path.clone(), Some(lock.clone()), Duration::from_millis(300)),
            Marked::OutOfTime
        );
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(300), "{waited:?}");
        assert!(waited < Duration::from_secs(10), "{waited:?}");
        assert!(
            on_disk(&path).last_run_unhealthy,
            "nothing is written while another writer holds the lock"
        );
        // The thread left behind waits on the lock for the rest of this test
        // binary, so it never writes into the folder this test removes.
        std::mem::forget(held);
    }

    /// A removal that lands after an uninstall took the config away must not
    /// put the enrollment back: `config::load` would restore the last good
    /// copy and write it in place. The mark reads, and finds nothing.
    #[test]
    fn a_missing_config_is_never_brought_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = enrolled(dir.path());
        start_a_run(&path, NOW);
        std::fs::rename(&path, config::prev_path(&path)).unwrap();
        assert_eq!(
            mark_within(path.clone(), Some(write_lock()), MARK_BUDGET),
            Marked::NoConfig
        );
        assert!(!path.exists(), "the mark must not reinstate the config");
    }

    /// A live file that does not parse is left for the next start's `load`,
    /// which restores the last good copy and clears its run marks (#4157).
    #[test]
    fn an_unreadable_config_is_left_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = enrolled(dir.path());
        start_a_run(&path, NOW);
        std::fs::copy(&path, config::prev_path(&path)).unwrap();
        std::fs::write(&path, b"\0\0\0\0").unwrap();
        assert_eq!(
            mark_within(path.clone(), None, MARK_BUDGET),
            Marked::NoConfig
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"\0\0\0\0");
    }

    /// A run already marked clean (past the clean-run threshold, or by an
    /// earlier end) is not written again.
    #[test]
    fn a_run_already_marked_clean_is_not_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = enrolled(dir.path());
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            mark_within(path.clone(), None, MARK_BUDGET),
            Marked::AlreadyClean
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// Which deliberate end a copy of this test binary plays.
    const ROLE: &str = "ROOMLERD_TEST_RUN_END";
    /// The folder that holds the copy's config.
    const DIR: &str = "ROOMLERD_TEST_RUN_END_DIR";
    /// The test that plays it.
    const STAND_IN: &str = "run_end::tests::the_stand_in";

    /// In a copy of this test binary, with [`ROLE`] and [`DIR`] set: the
    /// daemon's side of a deliberate end, as `run_cmd` and the end's own code
    /// make it. Open the book, share the lock, then end the way [`ROLE`]
    /// names. Without the variables, nothing.
    #[test]
    fn the_stand_in() {
        let (Some(role), Some(dir)) = (std::env::var_os(ROLE), std::env::var_os(DIR)) else {
            return;
        };
        open(&PathBuf::from(dir).join("config.toml"));
        share_write_lock(&write_lock());
        match role.to_str() {
            Some("removed") => exit_removed(),
            Some("duel") => exit_duel_lost(),
            Some("macos") => {
                mark_quit_by_macos();
                // AppKit's own exit, once the delegate has answered.
                std::process::exit(0)
            }
            other => panic!("no such end: {other:?}"),
        }
    }

    /// Play `role` in a copy of this test binary, on the config in `dir`:
    /// how the copy exited, and what it wrote to stderr.
    fn ended_in_a_copy(role: &str, dir: &Path) -> (Option<i32>, String) {
        let out = Command::new(std::env::current_exe().expect("this test binary"))
            .args([STAND_IN, "--exact", "--nocapture", "--test-threads=1"])
            .env(ROLE, role)
            .env(DIR, dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("a copy of this test binary");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        (out.status.code(), stderr)
    }

    /// A process that never opened a run (not the daemon, or before its
    /// start's mark) marks nothing.
    #[test]
    fn no_open_run_no_mark() {
        if BOOK.get().is_some() {
            // Only `the_stand_in` opens one, and only in its own copy.
            return;
        }
        assert_eq!(mark_deliberate_end("a test"), Marked::NoRun);
    }
}

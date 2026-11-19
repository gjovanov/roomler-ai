// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3802 — the error the daemon ends with, on stderr, where std would put it.
//!
//! A `main` that returns `Err` has std print `Error: {err:?}` on stderr, and
//! the process exits 1. The AppKit `main` of the macOS release build
//! (`viewer-indicator-macos`) cannot return: its main thread belongs to AppKit
//! for good, so its daemon thread ends the process with `exit(1)` itself.
//! Until #3802 that thread said the error through tracing only, and a screen
//! worker whose folder another account owns has no log file for tracing to
//! write: its error (the path, the owner, the OS error) reached no file.
//! [`say`] also writes what std would have written, on the stderr a supervised
//! worker inherits from the root daemon.
//!
//! #4023 — and the one end that is a state of the device, not a fault: `run`
//! with no config to run ([`NoConfig`]). Neither a Windows worker that ends with it
//! nor its service host records a crash ([`record_worker_exit`], #4028 [`exit_code`]).

use std::io::Write;
use std::path::{Path, PathBuf};

/// The daemon thread's last words before `exit(1)`: the error on stderr, as std
/// prints it for the other `main` ([`write_to`]), and through tracing, as before.
/// Stderr first, so nothing tracing does can keep the line from it.
pub fn say(error: &anyhow::Error) {
    write_to(std::io::stderr(), error);
    tracing::error!(error = %format!("{error:#}"), "daemon exited with an error");
}

/// What std prints for a `main` that returns `Err(error)`, written to `out`:
/// `Error: ` and anyhow's `Debug`, the message and then every cause under
/// `Caused by:`. A failed write is ignored, and the function returns.
pub fn write_to(mut out: impl Write, error: &anyhow::Error) {
    // Never `eprintln!`: it panics when the write fails, and a panic on the daemon thread skips
    // `exit(1)`: the main thread stays in AppKit, and no supervisor ever sees the process end.
    let _ = writeln!(out, "Error: {error:?}");
    let _ = out.flush();
}

/// #4023 — `run` found no config at the path it resolved: this account has no
/// enrollment on this device yet. A state of the device, which `roomlerd
/// enroll` ends, and not a fault. A type of its own, so that a worker's exit
/// for it is told apart by type, never by its words ([`record_worker_exit`]).
/// Its words are the ones `run` has always said.
#[derive(Debug)]
pub struct NoConfig {
    path: PathBuf,
    /// #4028 — the `run` that found no config is the worker the Windows service
    /// host started ([`NoConfig::of_run`]), so it ends with its own exit code.
    service_worker: bool,
}

impl NoConfig {
    /// No config at `path`.
    pub fn at(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            service_worker: false,
        }
    }

    /// #4028 — no config at `path`, found by the `run` whose spawner `supervisor`
    /// names (`run --supervisor`, a hidden flag). Only the Windows service host's
    /// word, `scm`, makes it the service's worker, and only on Windows: the one
    /// rule `supervision::detect_here` keeps for every `run`. FR-43's macOS flag
    /// can never make that answer, so it is not asked for.
    pub fn of_run(path: &Path, supervisor: Option<&str>) -> Self {
        use crate::supervision::{Supervision, detect_here};
        Self {
            service_worker: detect_here(supervisor, false) == Supervision::WindowsScm,
            ..Self::at(path)
        }
    }
}

impl std::fmt::Display for NoConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no config found at {}. Run `roomlerd enroll` first.",
            self.path.display()
        )
    }
}

impl std::error::Error for NoConfig {}

/// #4023 — a worker's crash record for the error its `run` ended with. `record`
/// writes it, with the summary it always had (`worker exit: ` and the whole
/// chain), for every error but [`NoConfig`], even under a context. That one is
/// the device's state and not a crash: one line says why, and nothing is
/// recorded. The exit code is the caller's and stays what it was. On Windows
/// `record` is the crash recorder (`app.rs`, `record_worker_exit_failure`).
pub fn record_worker_exit(err: &anyhow::Error, record: impl FnOnce(&str)) {
    if let Some(no_config) = err.downcast_ref::<NoConfig>() {
        tracing::info!(
            config = %no_config.path.display(),
            "worker exit: no config to run, so this account has no enrollment on this device \
             yet. That is the device's state, not a crash: no crash record is written"
        );
        return;
    }
    record(&format!("worker exit: {err:#}"));
}

/// #4028 — the code the worker the Windows service host started exits with when
/// its `run` ends with [`NoConfig`]: its account has no enrollment on this device.
/// The host tells that exit apart by this code and by no text. It writes no crash
/// record for it (`win_service::supervisor::should_record_supervisor_crash`), and
/// it backs off exactly as for an exit 1 (`decide_exit_reaction`'s generic arm).
///
/// **The Windows service host's worker only** ([`NoConfig::of_run`]). Every other
/// `run` that finds no config exits 1, as it always has: systemd's units, launchd,
/// FR-43's macOS supervisor, a container's runtime and the per-user Scheduled Task
/// read the exit code too, and none of them is told of this one.
///
/// Why 78. No other code in the tree is 78: not 0 or 1, the watchdog's sentinels
/// (2, 7, 8, 9), the stand-in's 10, the probe and portal helpers' 3 to 5, a
/// panic's 101, the launchers' 127, nor the session teardown 0x40010004 the host
/// names. Windows cannot mistake it either. 0x4E is no NTSTATUS at all: below
/// 0x100 the defined ones are the wait results 0x00–0x3F and 0x80–0xBF, 0xC0 and
/// 0xFF. And every exception code, which is what a process an exception kills
/// exits with, has its severity bits set: 0x40000000 and up. 78 fits an 8-bit exit
/// status unchanged. It is BSD `sysexits.h`'s `EX_CONFIG` ("something was found in
/// an unconfigured or misconfigured state"), a mnemonic only.
pub const NO_ENROLLMENT_EXIT_CODE: i32 = 78;

/// #4028 — the code a process whose `main` ends with `error` exits with, when it
/// is not std's 1: [`NO_ENROLLMENT_EXIT_CODE`] for the Windows service host's
/// worker that found no config ([`NoConfig::of_run`]), bare or under a context.
/// `None` for every other error and every other process: they exit 1.
pub fn exit_code(error: &anyhow::Error) -> Option<i32> {
    error
        .downcast_ref::<NoConfig>()
        .filter(|no_config| no_config.service_worker)
        .map(|_| NO_ENROLLMENT_EXIT_CODE)
}

/// #4028 — how `main` ends, off the AppKit build: `res` goes back to std, which
/// writes an error as `Error: {e:?}` on stderr and exits 1, unless the error has a
/// code of its own ([`exit_code`]). Then this writes the same line, then every log
/// line still queued (an `exit` drops nothing), and exits with that code.
pub fn leave(res: anyhow::Result<()>) -> anyhow::Result<()> {
    leave_through(res, std::io::stderr(), |code| {
        crate::logging::finish();
        std::process::exit(code)
    })
}

/// [`leave`], with where the line goes and what ends the process given, so a
/// test can watch both. `exit` never returns in [`leave`].
fn leave_through(
    res: anyhow::Result<()>,
    out: impl Write,
    exit: impl FnOnce(i32),
) -> anyhow::Result<()> {
    if let Err(error) = &res
        && let Some(code) = exit_code(error)
    {
        write_to(out, error);
        exit(code);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::write_to;
    use std::io;

    /// A chain shaped like the screen worker's: the config write naming its
    /// folder and owner, a step under it, and the OS error at the bottom.
    fn worker_error() -> anyhow::Error {
        let os = io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Permission denied (os error 13)",
        );
        anyhow::Error::new(os)
            .context("creating /Users/p/Library/Application Support/live.roomler.roomler")
            .context(
                "writing the person's config /Users/p/Library/Application Support/\
                 live.roomler.roomler/config.toml (its folder /Users/p/Library/Application \
                 Support/live.roomler.roomler is uid 0's, mode 755)",
            )
    }

    #[test]
    fn the_whole_chain_is_written_as_std_prints_it() {
        let e = worker_error();
        let mut out = Vec::new();
        write_to(&mut out, &e);
        let text = String::from_utf8(out).expect("the line is UTF-8");
        // What std's `Termination` prints for a `main` that returns `Err(e)`.
        assert_eq!(text, format!("Error: {e:?}\n"));
        assert!(text.contains("\n\nCaused by:\n"), "{text}");
        for cause in e.chain() {
            let cause = cause.to_string();
            assert!(text.contains(&cause), "{cause:?} is missing from {text:?}");
        }
    }

    #[test]
    fn a_writer_that_fails_makes_no_panic_and_the_function_returns() {
        /// A stderr whose reader is gone: every write and flush is refused.
        struct Refuses;
        impl io::Write for Refuses {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the reader is gone",
                ))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the reader is gone",
                ))
            }
        }
        /// A stderr that takes no byte: `write_all` fails with `WriteZero`.
        struct TakesNothing;
        impl io::Write for TakesNothing {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Ok(0)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let e = worker_error();
        let refused =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write_to(Refuses, &e)));
        assert!(
            refused.is_ok(),
            "a refused write panicked: the daemon thread would never reach exit(1)"
        );
        let zero =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write_to(TakesNothing, &e)));
        assert!(
            zero.is_ok(),
            "a write that took nothing panicked: the daemon thread would never reach exit(1)"
        );
    }
}

#[cfg(test)]
mod worker_exit_tests {
    use super::{NoConfig, record_worker_exit};
    use std::path::Path;

    /// What `f` logs, as the daemon's log file writes it.
    fn logged(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let writer = buf.clone();
        // Two dispatchers, never one: with one, a callsite's interest is
        // taken once for every thread.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned()
    }

    /// #4023 — `run`'s words for a device with no config are the ones it has
    /// always said: only the type is new.
    #[test]
    fn no_config_says_what_run_always_said() {
        let path = Path::new("/home/p/.config/roomler/config.toml");
        let err = anyhow::Error::from(NoConfig::at(path));
        assert_eq!(
            err.to_string(),
            "no config found at /home/p/.config/roomler/config.toml. Run `roomlerd enroll` first."
        );
    }

    /// #4023 — the field record: a person's worker on an attended per-Machine
    /// device started with no enrollment, and its exit was recorded as a crash.
    /// A worker that ends with no config writes no crash record, under a
    /// context too, and one line says why. Every other error still writes its
    /// record with the summary it always had, one that only says the same words
    /// included: the type decides, never the text.
    #[test]
    fn a_worker_with_no_config_records_no_crash_and_any_other_error_still_does() {
        let path = Path::new("/home/p/.config/roomler/config.toml");
        let mut written: Vec<String> = Vec::new();

        let no_config = anyhow::Error::from(NoConfig::at(path));
        let log = logged(|| record_worker_exit(&no_config, |s| written.push(s.to_owned())));
        assert!(written.is_empty(), "{written:?}");
        assert_eq!(log.lines().count(), 1, "{log}");
        assert!(log.contains("no crash record is written"), "{log}");
        assert!(log.contains(&path.display().to_string()), "{log}");

        let under_a_context =
            anyhow::Error::from(NoConfig::at(path)).context("starting the worker");
        record_worker_exit(&under_a_context, |s| written.push(s.to_owned()));
        assert!(written.is_empty(), "{written:?}");

        let fault = anyhow::anyhow!("access denied").context("acquiring single-instance lock");
        record_worker_exit(&fault, |s| written.push(s.to_owned()));
        let same_words = anyhow::anyhow!("{}", NoConfig::at(path));
        record_worker_exit(&same_words, |s| written.push(s.to_owned()));
        assert_eq!(
            written,
            [
                "worker exit: acquiring single-instance lock: access denied".to_string(),
                format!("worker exit: {}", NoConfig::at(path)),
            ]
        );
    }
}

#[cfg(test)]
mod exit_code_tests {
    use super::{NO_ENROLLMENT_EXIT_CODE, NoConfig, exit_code, leave_through, record_worker_exit};
    use std::path::Path;

    /// A person's config, where the service host's worker reads it.
    const PATH: &str = r"C:\Users\p\AppData\Local\roomler\roomler\config.toml";

    /// The code the service host's worker that found no config leaves with on
    /// THIS platform: its own on Windows, and none elsewhere, where no such host
    /// exists and the process exits 1 as it always has.
    fn service_code() -> Option<i32> {
        cfg!(windows).then_some(NO_ENROLLMENT_EXIT_CODE)
    }

    /// #4028 — the worker the Windows service host started (`run --supervisor
    /// scm`) that finds no config has its own exit code, under a context too.
    /// The per-user Task's worker, a bare `run`, a word no spawner says, and
    /// every platform but Windows exit 1, as they always have. So does an error
    /// of any other kind, one that only says the same words included.
    #[test]
    fn only_the_service_hosts_worker_with_no_config_has_its_own_code() {
        let path = Path::new(PATH);
        let service = anyhow::Error::from(NoConfig::of_run(path, Some("scm")));
        assert_eq!(exit_code(&service), service_code());
        let under_a_context =
            anyhow::Error::from(NoConfig::of_run(path, Some("scm"))).context("starting the worker");
        assert_eq!(exit_code(&under_a_context), service_code());

        for supervisor in [Some("task"), None, Some("nssm"), Some("")] {
            let err = anyhow::Error::from(NoConfig::of_run(path, supervisor));
            assert_eq!(exit_code(&err), None, "{supervisor:?}");
        }
        assert_eq!(exit_code(&NoConfig::at(path).into()), None);

        let fault = anyhow::anyhow!("access denied").context("acquiring single-instance lock");
        assert_eq!(exit_code(&fault), None);
        let same_words = anyhow::anyhow!("{}", NoConfig::of_run(path, Some("scm")));
        assert_eq!(exit_code(&same_words), None);
    }

    /// #4028 — the service host's worker says what `run` has always said, and
    /// still writes no crash record of its own (#4023).
    #[test]
    fn the_service_hosts_worker_says_the_same_words_and_records_no_crash() {
        let path = Path::new(PATH);
        assert_eq!(
            anyhow::Error::from(NoConfig::of_run(path, Some("scm"))).to_string(),
            anyhow::Error::from(NoConfig::at(path)).to_string()
        );
        let mut written: Vec<String> = Vec::new();
        record_worker_exit(&NoConfig::of_run(path, Some("scm")).into(), |s| {
            written.push(s.to_owned())
        });
        assert!(written.is_empty(), "{written:?}");
    }

    /// #4028 — how `main` ends. The service host's worker that found no config
    /// writes on stderr exactly what std writes for an error, and exits with its
    /// own code. Any other error goes back to std untouched, which writes the
    /// same line and exits 1, and so does a clean end.
    #[test]
    fn main_leaves_with_the_errors_own_code_and_the_line_std_writes() {
        let path = Path::new(PATH);
        let mut out = Vec::new();
        let mut exited = None;
        let res = leave_through(
            Err(NoConfig::of_run(path, Some("scm")).into()),
            &mut out,
            |code| exited = Some(code),
        );
        let err = res.expect_err("the error comes back only because this exit returns");
        assert_eq!(exited, service_code());
        let line = if cfg!(windows) {
            format!("Error: {err:?}\n")
        } else {
            String::new()
        };
        assert_eq!(String::from_utf8(out).expect("UTF-8"), line);

        let others: [anyhow::Result<()>; 3] = [
            Err(anyhow::anyhow!("access denied").context("acquiring single-instance lock")),
            Err(NoConfig::of_run(path, Some("task")).into()),
            Ok(()),
        ];
        for res in others {
            let words = res.as_ref().err().map(|e| format!("{e:#}"));
            let mut out = Vec::new();
            let mut exited = None;
            let back = leave_through(res, &mut out, |code| exited = Some(code));
            assert_eq!(exited, None, "{words:?}");
            assert!(out.is_empty(), "{words:?}");
            assert_eq!(back.err().map(|e| format!("{e:#}")), words);
        }
    }

    /// #4028 — the code is 78, pinned: the service log shows it, and the docs
    /// and the lab name it. It is no other code in the tree and no Windows
    /// status.
    #[test]
    fn the_no_enrollment_code_is_pinned_and_collides_with_nothing() {
        assert_eq!(NO_ENROLLMENT_EXIT_CODE, 78);
        let taken = [
            0,
            1,
            crate::watchdog::STALL_EXIT_CODE,
            crate::watchdog::AGENT_DELETED_EXIT_CODE,
            crate::watchdog::ALREADY_RUNNING_EXIT_CODE,
            crate::watchdog::RESTART_REQUESTED_EXIT_CODE,
            crate::unenrolled::STAND_IN_LOST_EXIT_CODE,
            // The caps probe's and the portal helper's own exits.
            3,
            4,
            5,
            // A panic on the main thread.
            101,
            // `roomlerdw` and the CLI shim with no daemon to run.
            127,
        ];
        assert!(!taken.contains(&NO_ENROLLMENT_EXIT_CODE), "{taken:?}");
        let status = NO_ENROLLMENT_EXIT_CODE as u32;
        // Between the wait results (0x00-0x3F) and the abandoned waits
        // (0x80-0xBF), where no NTSTATUS is defined, and under every exception
        // code (0x40000000 and up).
        assert!((0x40..0x80).contains(&status), "{status:#x}");
        // An 8-bit exit status carries it unchanged.
        assert_eq!(status & 0xFF, status);
    }
}

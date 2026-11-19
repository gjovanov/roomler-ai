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
//! with no config to run ([`NoConfig`]). A Windows worker that ends with it
//! writes no crash record ([`record_worker_exit`]).

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
}

impl NoConfig {
    /// No config at `path`.
    pub fn at(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
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

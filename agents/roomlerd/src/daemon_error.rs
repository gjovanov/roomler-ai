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

use std::io::Write;

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

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #1731 — where the server's log lines go, and why they may be dropped.
//!
//! Every log call used to write stdout synchronously, under the process-wide
//! stdout lock (the fmt layer's default writer). When the container's log
//! pipe stopped draining, the first worker to log blocked in `write(1, …)`
//! holding that lock, and every other worker blocked on the lock. The server
//! runs as many tokio workers as its CPU limit (two in production), so that
//! was the whole runtime: `/health` unanswered and the log silent, from the
//! same instant. Reproduced on the real binary, with the stall watchdog's dump
//! showing exactly that (issue #1731).
//!
//! Now a log call only enqueues its line into a bounded queue, and never
//! waits. One thread owns stdout and drains the queue. If stdout stops, that
//! thread waits alone, the queue fills, and further lines are DROPPED and
//! counted instead of stopping the server. A dropped line is a lost log line.
//! A blocked worker was a dead pod.

use std::ffi::OsStr;
use std::io::Write;

use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::fmt::{
    self, MakeWriter,
    format::{DefaultFields, Format},
};
use tracing_subscriber::registry::LookupSpan;

/// Lines held while the output is not draining, before new ones are dropped.
/// About 5 MB at a typical line length. At the server's usual rate that
/// covers minutes of a stalled pipe, and a burst while the pipe is slow.
pub const LOG_QUEUE_LINES: usize = 16_384;

/// A writer for the fmt layer that never blocks its caller. Keep the guard for
/// the life of the process: dropping it flushes (with a bounded wait) and
/// stops the writer thread.
pub fn nonblocking<W: Write + Send + 'static>(output: W) -> (NonBlocking, WorkerGuard) {
    NonBlockingBuilder::default()
        .lossy(true)
        .buffered_lines_limit(LOG_QUEUE_LINES)
        .thread_name("log-writer")
        .finish(output)
}

/// #4402 — whether the server's log lines carry ANSI colour codes: only when
/// stdout is a terminal, and never when `NO_COLOR` holds a value
/// (<https://no-color.org>: any non-empty value; an empty one counts as unset).
///
/// The fmt layer never asks what its output is. Left to itself it coloured
/// every line, so the pods printed the codes into their log pipe, and the node
/// stored them: 41 % of the busier pod's bytes in the hour read after #4414,
/// which kept its log a few hours short of a day. A terminal is a person
/// reading; a pipe is a container runtime, a file or another program.
///
/// `main` asks the real stdout once at start, beside the `nonblocking` call:
/// the layer writes into that writer, never into stdout, so it cannot ask.
pub fn colour(stdout_is_terminal: bool, no_color: Option<&OsStr>) -> bool {
    stdout_is_terminal && no_color.is_none_or(OsStr::is_empty)
}

/// The fmt layer that writes the server's log lines into `writer`, coloured
/// only when `colour` says so (see [`colour`]). The line's text is the same
/// either way: timestamp, level, target, message and fields.
#[expect(
    clippy::disallowed_methods,
    reason = "the server's console: its colour is the caller's, from colour (root clippy.toml, #3938)"
)]
pub fn layer<S, W>(writer: W, colour: bool) -> fmt::Layer<S, DefaultFields, Format, W>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + 'static,
{
    fmt::layer().with_writer(writer).with_ansi(colour)
}

/// Report dropped lines once a minute, through the log itself. If the output
/// is still stopped this report is dropped as well, and the next one carries
/// the total, so the gap is visible as soon as the pipe drains.
pub fn report_dropped_lines(counter: ErrorCounter) {
    tokio::spawn(async move {
        let mut reported = 0usize;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            let total = counter.dropped_lines();
            if total > reported {
                // `since_last_report`, not "dropped now": the previous report may
                // itself have been dropped while the output was still stopped;
                // `total` is the number that always holds.
                tracing::warn!(
                    since_last_report = total - reported,
                    total,
                    "log output was not draining: lines were dropped so the server kept serving (#1731)"
                );
                reported = total;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tracing_subscriber::layer::SubscriberExt;

    /// An output that accepts nothing until released: a log pipe nobody reads.
    struct Stuck(mpsc::Receiver<()>);

    impl Write for Stuck {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The #1731 property: with the output stopped, logging still returns at
    /// once, the overflow is dropped and counted, and nothing blocks.
    #[test]
    fn a_stopped_output_never_blocks_the_caller_and_the_overflow_is_counted() {
        let (release, stuck) = mpsc::channel();
        let (writer, guard) = nonblocking(Stuck(stuck));
        let counter = writer.error_counter();
        let lines = LOG_QUEUE_LINES * 2;
        // Log from a thread of its own, so a writer that DOES block (the old
        // behaviour) fails this test in bounded time instead of hanging it.
        let (done_tx, done_rx) = mpsc::channel();
        let mut w = writer.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            for i in 0..lines {
                w.write_all(format!("line {i}\n").as_bytes())
                    .expect("write");
            }
            let _ = done_tx.send(started.elapsed());
        });
        let took = done_rx.recv_timeout(Duration::from_secs(10));
        assert!(
            took.is_ok_and(|t| t < Duration::from_secs(5)),
            "{lines} lines into a stopped output did not return promptly ({took:?}): logging blocked"
        );
        assert!(
            counter.dropped_lines() >= lines - LOG_QUEUE_LINES - 1,
            "the overflow is dropped and counted: {} of {lines}",
            counter.dropped_lines()
        );
        // Let the writer thread go so the guard's shutdown is not waiting on it.
        drop(release);
        drop(guard);
    }

    /// An in-memory output for the layer: what a pipe to the container
    /// runtime would have received.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// One event with a field, written by the layer `main` builds, under a
    /// scoped subscriber (the test binary shares its process).
    fn written(colour: bool) -> String {
        let out = Captured::default();
        let subscriber = tracing_subscriber::registry().with(layer(out.clone(), colour));
        roomler_log_capture::with_default(subscriber, || {
            tracing::info!(peers = 3, "a line with a field");
        });
        let bytes = out.0.lock().unwrap().clone();
        String::from_utf8(bytes).expect("the log is UTF-8")
    }

    /// #4402: colour codes were 41 % of what the busier pod printed into its
    /// log pipe. With colour off the line holds no ESC byte, and the text a
    /// read counts is the same.
    #[test]
    fn a_log_without_colour_holds_no_escape_byte() {
        let line = written(false);
        assert!(
            line.contains("a line with a field"),
            "the event is written: {line:?}"
        );
        assert!(
            !line.contains('\x1b'),
            "no colour code when colour is off: {line:?}"
        );
        assert!(
            line.contains(" INFO "),
            "the level reads as before: {line:?}"
        );
        assert!(
            line.contains("peers=3"),
            "the field reads as before: {line:?}"
        );
    }

    /// The other direction: on a terminal the same layer still colours.
    #[test]
    fn a_log_with_colour_holds_escape_codes() {
        let line = written(true);
        assert!(
            line.contains("a line with a field"),
            "the event is written: {line:?}"
        );
        assert!(
            line.contains('\x1b'),
            "colour codes when colour is on: {line:?}"
        );
    }

    /// The decision, every input: colour only on a terminal, and `NO_COLOR`
    /// with any non-empty value turns it off; empty counts as unset.
    #[test]
    fn colour_only_on_a_terminal_and_never_under_no_color() {
        let cases: [(bool, Option<&str>, bool); 8] = [
            (true, None, true),
            (true, Some(""), true),
            (true, Some("1"), false),
            (true, Some("0"), false),
            (false, None, false),
            (false, Some(""), false),
            (false, Some("1"), false),
            (false, Some("0"), false),
        ];
        for (terminal, no_color, want) in cases {
            assert_eq!(
                colour(terminal, no_color.map(OsStr::new)),
                want,
                "terminal={terminal} NO_COLOR={no_color:?}"
            );
        }
    }
}

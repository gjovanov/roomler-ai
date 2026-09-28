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

use std::io::Write;

use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};

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
    use std::time::{Duration, Instant};

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
}

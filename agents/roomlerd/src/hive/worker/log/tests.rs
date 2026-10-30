// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
use std::sync::Mutex;

#[cfg(unix)]
use tokio::net::UnixStream;
use tracing_subscriber::layer::SubscriberExt;

#[cfg(unix)]
use super::super::recv;
use super::*;

/// What `f` logs through a layer of its own, `room` lines long: the lines it
/// queued, and how many it dropped. The layer is gone at the end, so its
/// queue ends once drained.
fn logged(room: usize, f: impl FnOnce()) -> Lines {
    let (layer, lines) = layer_of(room);
    // Two dispatchers, never one (`hive::view`'s tests: with one, a
    // callsite's interest is taken once for every thread).
    let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), f);
    lines
}

/// Every line in `lines`, in order.
fn all(mut lines: Lines) -> Vec<Line> {
    let mut out = Vec::new();
    while let Ok(line) = lines.rx.try_recv() {
        out.push(line);
    }
    out
}

/// What the daemon logs, as text.
#[derive(Clone, Default)]
pub(in crate::hive::worker) struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    pub(in crate::hive::worker) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }

    /// Everything this thread logs while what it returns lives, kept by
    /// `filter` and written as the daemon's file writes it: no target, no
    /// colour.
    pub(in crate::hive::worker) fn capture(
        &self,
        filter: &str,
    ) -> (tracing::Dispatch, tracing::dispatcher::DefaultGuard) {
        let second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let logs = self.clone();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
                .with_target(false)
                .with_ansi(false)
                .without_time()
                .with_writer(move || logs.clone())
                .finish(),
        );
        (second, guard)
    }
}

/// A worker's line is its event's message and fields, at its level, where
/// it was logged; a `log` crate's record, where that record was logged.
#[test]
fn a_workers_line_is_its_message_its_fields_and_where_it_was_logged() {
    let lines = all(logged(8, || {
        tracing::info!(
            target: "roomlerd::hive::egress",
            host = "example.test",
            port = 443u16,
            "hive: egress refused"
        );
        tracing::warn!(n = 2, "two");
        tracing::error!(
            log.target = "rustls::client",
            log.module_path = "rustls::client",
            log.line = 7u32,
            "a log record"
        );
    }));
    let line = |level: &str, target: &str, text: &str| Line {
        level: level.into(),
        target: target.into(),
        text: text.into(),
    };
    assert_eq!(
        lines,
        vec![
            line(
                "INFO",
                "roomlerd::hive::egress",
                "hive: egress refused host=example.test port=443"
            ),
            line("WARN", module_path!(), "two n=2"),
            line("ERROR", "rustls::client", "a log record"),
        ]
    );
}

/// A line past its bound is cut there, at a character's boundary, and says
/// it was cut; one within it crosses whole.
#[test]
fn a_line_past_its_bound_is_cut_at_a_character() {
    let mut s = "é".repeat(MAX_LINE);
    clip(&mut s, MAX_LINE + 1);
    assert_eq!(
        s.len(),
        MAX_LINE + '…'.len_utf8(),
        "cut at {MAX_LINE}, not mid-character"
    );
    assert!(s.ends_with('…'));

    let long = "x".repeat(3 * MAX_LINE);
    let lines = all(logged(8, || tracing::info!("{long}")));
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0].text.len(),
        MAX_LINE + '…'.len_utf8(),
        "the layer cuts it"
    );

    let lines = all(logged(8, || tracing::info!("short")));
    assert_eq!(lines[0].text, "short");
}

/// A line that finds the worker's queue full is dropped, never waited on,
/// and counted; after the next line that crosses, a line says how many.
#[cfg(unix)]
#[tokio::test]
async fn a_full_queue_drops_a_line_and_after_the_next_a_line_says_how_many() {
    let lines = logged(1, || {
        for n in 1..=3 {
            tracing::info!(n, "line");
        }
    });
    let (mut daemon, worker) = UnixStream::pair().unwrap();
    let (_rd, wr) = worker.into_split();
    tokio::time::timeout(
        Duration::from_secs(5),
        forward(lines, Arc::new(Link::worker(wr))),
    )
    .await
    .expect("it ends where its queue does");
    assert_eq!(
        recv(&mut daemon).await.unwrap(),
        Some(Msg::Log {
            level: "INFO".into(),
            target: module_path!().into(),
            line: "line n=1".into(),
        })
    );
    match recv(&mut daemon).await.unwrap() {
        Some(Msg::Log { level, line, .. }) => {
            assert_eq!(level, "WARN");
            assert!(line.contains("dropped 2 lines"), "{line}");
        }
        other => panic!("not the note of what was dropped: {other:?}"),
    }
    assert_eq!(recv(&mut daemon).await.unwrap(), None, "and nothing after");
}

/// The daemon writes at most [`PER_SECOND`] of one worker's lines a second:
/// the rest are dropped and counted, and the first line of a later second
/// says how many were.
#[test]
fn the_daemon_writes_at_most_its_bound_a_second_and_then_says_how_many_went() {
    let mut r = Relay::new(7);
    let t0 = r.second;
    for _ in 0..PER_SECOND {
        assert_eq!(r.admit(t0), Admit::Write { dropped_before: 0 });
    }
    assert_eq!(r.admit(t0), Admit::Drop);
    assert_eq!(r.admit(t0 + Duration::from_millis(999)), Admit::Drop);
    let t1 = t0 + Duration::from_secs(1);
    assert_eq!(r.admit(t1), Admit::Write { dropped_before: 2 });
    assert_eq!(r.admit(t1), Admit::Write { dropped_before: 0 });
}

/// The daemon keeps a worker's line as it keeps its own: at its level, by
/// its filter as asked about where the worker logged it — so a module the
/// filter turns up is up in the worker too, and one it leaves down is down.
#[test]
fn the_daemon_keeps_a_workers_line_as_it_keeps_its_own() {
    let logs = Logs::default();
    let (_second, _guard) = logs.capture("tunnel_core=debug,info");
    let mut r = Relay::new(7);
    r.write("DEBUG", "tunnel_core::overlay", "kept: its module is up");
    r.write(
        "DEBUG",
        "roomlerd::hive::egress",
        "dropped: its module is not",
    );
    r.write(
        "INFO",
        "roomlerd::hive::egress",
        "kept: at the filter's level",
    );
    r.write(
        "TRACE",
        "tunnel_core::overlay",
        "dropped: below its module's",
    );
    let text = logs.text();
    assert!(
        text.contains("DEBUG kept: its module is up worker=7 origin=tunnel_core::overlay"),
        "{text}"
    );
    assert!(
        text.contains("INFO kept: at the filter's level worker=7 origin=roomlerd::hive::egress"),
        "{text}"
    );
    assert!(!text.contains("dropped:"), "{text}");
}

/// A worker's line is one line in the daemon's log, whatever it holds: its
/// control characters, and its origin's, escaped.
#[test]
fn a_workers_line_is_one_line_in_the_daemons_log() {
    let logs = Logs::default();
    let (_second, _guard) = logs.capture("trace");
    let mut r = Relay::new(7);
    r.write(
        "WARN",
        "roomlerd::hive::egress\nERROR forged",
        "refused\n2026-10-27T00:00:00Z ERROR roomlerd: forged \u{1b}[2J",
    );
    let text = logs.text();
    assert_eq!(text.matches('\n').count(), 1, "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");
    assert!(text.contains("refused\\n2026-10-27"), "{text}");
    assert!(
        text.contains("origin=roomlerd::hive::egress\\nERROR"),
        "{text}"
    );
}

/// A value the worker holds names nothing in a line, logged by mistake as
/// it may be.
#[cfg(worker_calls)]
#[test]
fn a_value_logged_by_mistake_names_nothing() {
    use crate::hive::vault_connect::handed::Secret;
    let held = Secret(zeroize::Zeroizing::new(format!("{}-{}", "held", line!())));
    let lines = all(logged(
        8,
        || tracing::warn!(value = ?held, "hive: by mistake"),
    ));
    assert_eq!(lines.len(), 1);
    assert!(!lines[0].text.contains(held.0.as_str()), "{:?}", lines[0]);
}

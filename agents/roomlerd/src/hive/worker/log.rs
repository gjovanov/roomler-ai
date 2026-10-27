// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2a-5 — the worker's log, in the daemon's.
//!
//! `roomlerd hive-worker` runs none of the daemon's start-up, its logging
//! among it (`main.rs`), so without this its lines went nowhere: an
//! endpoint's or the egress proxy's refusal there was enforced, and logged
//! nowhere. Each of its lines now crosses the channel as a `log` frame, and
//! the daemon writes it into its own log, with its file, its upload and its
//! filter, beside the worker's process.
//!
//! | Piece | As built |
//! |---|---|
//! | the worker's lines | what its filter keeps, the daemon's own (`crate::logging::filter`, from the same `RUST_LOG`): each event's message and fields, as one line of at most [`MAX_LINE`] bytes; a `log` crate's record, where it was logged |
//! | the queue | [`QUEUE`] lines, never waited on: a line that finds it full is dropped and counted, and after the next that goes, a line says how many were |
//! | when they go | from the worker's `ready` on, so the handshake is never a log line, and the daemon takes none for one |
//! | a panic | a line too, as it happens, and on the worker's stderr as before |
//! | the daemon's side | each line at the worker's level, kept by the daemon's filter as asked about where the worker logged it, its `worker` (the process) and `origin` beside it; one line, its control characters escaped; at most [`PER_SECOND`] a second from one worker, the rest counted and said once the second is over |
//!
//! ⚠️ The daemon asks its filter about the line's `origin`, never about this
//! module: the `tracing` macros would ask about the module they are written
//! in, and a `RUST_LOG` that turns one of the worker's modules up
//! (`tunnel_core=debug`) would then keep the line in the worker and drop it
//! in the daemon. Hence five callsites of its own, one a level, as
//! `tracing-log` makes its own for the `log` crate's records.
//!
//! ⚠️ Nothing the worker holds is ever logged: a value is a `Secret`, whose
//! `Debug` names nothing, and the frames are wiped after use as every frame
//! is.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::{Event, Level, Metadata, Subscriber, error, warn};
use tracing_core::callsite::Callsite;
use tracing_core::field::{FieldSet, Value};
use tracing_core::identify_callsite;
use tracing_core::metadata::Kind;
use tracing_core::subscriber::Interest;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use super::Msg;
use super::link::Link;

/// The longest line that crosses: a longer one is cut there.
pub(crate) const MAX_LINE: usize = 8 * 1024;
/// The longest `origin` the daemon writes.
const MAX_ORIGIN: usize = 256;
/// The lines a worker holds before its channel takes them.
pub(crate) const QUEUE: usize = 1024;
/// The most lines a second the daemon writes from one worker.
pub(crate) const PER_SECOND: u32 = 200;

/// One line of the worker's log, as its layer made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    /// `ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE`.
    pub(crate) level: String,
    /// Where it was logged.
    pub(crate) target: String,
    pub(crate) text: String,
}

/// The worker's tracing layer: each event, a [`Line`] into the queue.
pub(crate) struct ToChannel {
    tx: mpsc::Sender<Line>,
    dropped: Arc<AtomicU64>,
}

/// The other end of the queue: what the channel sends, and what the queue
/// dropped.
pub(crate) struct Lines {
    rx: mpsc::Receiver<Line>,
    dropped: Arc<AtomicU64>,
}

/// A layer and the lines it makes, its queue [`QUEUE`] long.
pub(crate) fn layer() -> (ToChannel, Lines) {
    layer_of(QUEUE)
}

fn layer_of(room: usize) -> (ToChannel, Lines) {
    let (tx, rx) = mpsc::channel(room);
    let dropped = Arc::new(AtomicU64::new(0));
    let lines = Lines {
        rx,
        dropped: Arc::clone(&dropped),
    };
    (ToChannel { tx, dropped }, lines)
}

impl<S: Subscriber> Layer<S> for ToChannel {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut fields = Fields::default();
        event.record(&mut fields);
        let mut text = fields.message;
        if !fields.rest.is_empty() {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&fields.rest);
        }
        clip(&mut text, MAX_LINE);
        let line = Line {
            level: meta.level().to_string(),
            target: fields.target.unwrap_or_else(|| meta.target().to_string()),
            text,
        };
        if self.tx.try_send(line).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// An event's message, and its other fields as `name=value`; a `log`
/// crate's record's, its target.
#[derive(Default)]
struct Fields {
    message: String,
    rest: String,
    target: Option<String>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message.push_str(value),
            // A `log` record, as `tracing-log` hands it on: where it was
            // logged, and the rest of where, which a line has no room for.
            "log.target" => self.target = Some(value.to_string()),
            name if name.starts_with("log.") => {}
            _ => self.field(field, format_args!("{value}")),
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => {
                let _ = write!(self.message, "{value:?}");
            }
            name if name.starts_with("log.") => {}
            _ => self.field(field, format_args!("{value:?}")),
        }
    }
}

impl Fields {
    fn field(&mut self, field: &Field, value: std::fmt::Arguments<'_>) {
        if !self.rest.is_empty() {
            self.rest.push(' ');
        }
        let _ = write!(self.rest, "{}={value}", field.name());
    }
}

/// `s` at most `max` bytes, cut at a character's boundary.
fn clip(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
}

/// Install `layer` as the worker's one subscriber, under the daemon's own
/// filter, and a panic hook that logs the panic through it: the worker
/// process's, once.
pub(crate) fn install(layer: ToChannel) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let _ = tracing_subscriber::registry()
        .with(crate::logging::filter())
        .with(layer)
        .try_init();
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error!(panic = %info, "hive: the session's worker panicked");
        before(info);
    }));
}

/// Send each line across `link`, and after one that went while some were
/// dropped, a line that says how many, until the channel goes.
pub(crate) async fn forward(mut lines: Lines, link: Arc<Link>) {
    while let Some(line) = lines.rx.recv().await {
        let frame = Msg::Log {
            level: line.level,
            target: line.target,
            line: line.text,
        };
        if link.send(&frame).await.is_err() {
            return;
        }
        // Each was dropped with the queue full, so after every line in it:
        // after this one.
        let dropped = lines.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let note = Msg::Log {
                level: "WARN".into(),
                target: module_path!().into(),
                line: format!("hive: the worker's log dropped {dropped} lines: its queue was full"),
            };
            if link.send(&note).await.is_err() {
                return;
            }
        }
    }
}

// ─── The daemon's side ──────────────────────────────────────────────────────

/// A worker's lines into the daemon's log, at most [`PER_SECOND`] a second.
pub(crate) struct Relay {
    pid: u32,
    second: Instant,
    written: u32,
    dropped: u64,
}

/// What [`Relay::admit`] says of one line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admit {
    /// Written; and before it, where the second before dropped some, how many.
    Write { dropped_before: u64 },
    /// Past this second's lines: dropped, and counted.
    Drop,
}

impl Relay {
    pub(crate) fn new(pid: u32) -> Self {
        Self {
            pid,
            second: Instant::now(),
            written: 0,
            dropped: 0,
        }
    }

    /// Whether a line that comes at `now` is written.
    pub(crate) fn admit(&mut self, now: Instant) -> Admit {
        let mut dropped_before = 0;
        if now.duration_since(self.second) >= Duration::from_secs(1) {
            dropped_before = std::mem::take(&mut self.dropped);
            self.second = now;
            self.written = 0;
        }
        if self.written >= PER_SECOND {
            self.dropped += 1;
            return Admit::Drop;
        }
        self.written += 1;
        Admit::Write { dropped_before }
    }

    /// Write one line the worker sent, at its level, where the daemon's
    /// filter keeps it.
    pub(crate) fn write(&mut self, level: &str, origin: &str, line: &str) {
        let Admit::Write { dropped_before } = self.admit(Instant::now()) else {
            return;
        };
        let worker = self.pid;
        if dropped_before > 0 {
            warn!(
                worker,
                dropped = dropped_before,
                "hive: a session's worker logged more than {PER_SECOND} lines a second: the rest were dropped"
            );
        }
        let level = match level {
            "ERROR" => Level::ERROR,
            "WARN" => Level::WARN,
            "INFO" => Level::INFO,
            "DEBUG" => Level::DEBUG,
            _ => Level::TRACE,
        };
        let mut origin = one_line(origin);
        clip(&mut origin, MAX_ORIGIN);
        let mut line = one_line(line);
        clip(&mut line, MAX_LINE);
        relay(level, &origin, worker, &line);
    }
}

/// `s` as one line: each control character escaped, so a worker's line can
/// never pass for one of the daemon's own.
fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// One of the daemon's callsites for its workers' lines, at one level.
macro_rules! relayed {
    ($site:ident, $ty:ident, $meta:ident, $level:expr) => {
        struct $ty;
        static $site: $ty = $ty;
        static $meta: Metadata<'static> = Metadata::new(
            "hive: a session's worker logged",
            module_path!(),
            $level,
            Some(file!()),
            Some(line!()),
            Some(module_path!()),
            FieldSet::new(FIELDS, identify_callsite!(&$site)),
            Kind::EVENT,
        );
        impl Callsite for $ty {
            fn set_interest(&self, _: Interest) {}
            fn metadata(&self) -> &Metadata<'_> {
                &$meta
            }
        }
    };
}

/// A relayed line's fields.
const FIELDS: &[&str] = &["message", "worker", "origin"];

relayed!(ERROR_SITE, ErrorSite, ERROR_META, Level::ERROR);
relayed!(WARN_SITE, WarnSite, WARN_META, Level::WARN);
relayed!(INFO_SITE, InfoSite, INFO_META, Level::INFO);
relayed!(DEBUG_SITE, DebugSite, DEBUG_META, Level::DEBUG);
relayed!(TRACE_SITE, TraceSite, TRACE_META, Level::TRACE);

/// Write `line` at `level` where the daemon's filter keeps a line logged at
/// `origin`.
fn relay(level: Level, origin: &str, worker: u32, line: &str) {
    if level > LevelFilter::current() {
        return;
    }
    let meta: &'static Metadata<'static> = match level {
        Level::ERROR => &ERROR_META,
        Level::WARN => &WARN_META,
        Level::INFO => &INFO_META,
        Level::DEBUG => &DEBUG_META,
        _ => &TRACE_META,
    };
    // What the filter is asked about: the line as the worker logged it.
    let asked = Metadata::new(
        meta.name(),
        origin,
        level,
        None,
        None,
        None,
        FieldSet::new(&[], meta.callsite()),
        Kind::EVENT,
    );
    let fields = meta.fields();
    let (Some(message), Some(by), Some(at)) = (
        fields.field("message"),
        fields.field("worker"),
        fields.field("origin"),
    ) else {
        return;
    };
    let shown = tracing::field::display(origin);
    tracing_core::dispatcher::get_default(|d| {
        if !d.enabled(&asked) {
            return;
        }
        let values = [
            (&message, Some(&line as &dyn Value)),
            (&by, Some(&worker as &dyn Value)),
            (&at, Some(&shown as &dyn Value)),
        ];
        d.event(&Event::new(meta, &fields.value_set(&values)));
    });
}

#[cfg(test)]
pub(super) mod tests;

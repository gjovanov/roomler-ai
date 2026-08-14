//! rc.58 — centralized log uploader.
//!
//! A `tracing-subscriber::Layer` that captures every emitted event,
//! converts it into a [`LogLine`], and forwards it to a tokio mpsc
//! channel. A background task drains the channel every 5 s and POSTs
//! batches of up to 500 lines to
//! `POST /api/tenant/{tenant_id}/agent/{agent_id}/logs` (auth: agent
//! JWT).
//!
//! Default ON. Kill switch: `ROOMLER_AGENT_LOGS_UPLOAD_DISABLED=1`.
//!
//! Failure modes:
//! - Channel full (uploader too slow / network dead) → newest events
//!   dropped at the layer (`try_send` returns Err). Acceptable for
//!   diagnostic data; the rolling on-disk file logs retain everything.
//! - 401 / 403 from server → batch dropped, longer backoff (likely
//!   JWT expired / agent unenrolled).
//! - Other transient failures → batch retained, exponential backoff
//!   capped at 60 s.

use bson::{DateTime, Document};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// Wire types — mirror the shape in `crates/db/src/models/agent_log.rs`
// so the agent doesn't need a `roomler-ai-db` dependency (which would
// drag the MongoDB driver into the agent binary). Serde reps below
// MUST stay in sync with the db-side types; the integration test
// `crates/tests/src/agent_log.rs` will lock both.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogSource {
    Agent,
    Service,
    Installer,
    Crash,
    Updater,
    Browser,
}

impl LogSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Service => "service",
            Self::Installer => "installer",
            Self::Crash => "crash",
            Self::Updater => "updater",
            Self::Browser => "browser",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub ts: DateTime,
    pub level: LogLevel,
    pub target: String,
    pub msg: String,
    #[serde(default, skip_serializing_if = "Document::is_empty")]
    pub fields: Document,
}
use tokio::sync::mpsc;
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Channel capacity. ~10 000 lines × ~200 B = ~2 MB of in-memory
/// backpressure before the layer starts dropping events. Sized to
/// survive ~8 minutes of busy-session log volume (20 lines/sec) if
/// the network is dead, then degrade gracefully.
pub const CHANNEL_CAPACITY: usize = 10_000;

/// Flush cadence. Every N seconds the uploader drains the channel up
/// to [`MAX_BATCH_LINES`] and posts a batch.
pub const FLUSH_INTERVAL_SECS: u64 = 5;

/// Per-batch line cap. Matches the server-side
/// `AgentLogBatch::MAX_LINES_PER_BATCH` so a batch never gets rejected
/// for being too large.
pub const MAX_BATCH_LINES: usize = 500;

/// Initial retry delay on transient upload failure. Doubles on each
/// failure up to [`MAX_RETRY_BACKOFF_SECS`], resets on success.
pub const INITIAL_RETRY_BACKOFF_SECS: u64 = 5;

pub const MAX_RETRY_BACKOFF_SECS: u64 = 60;

/// Tracing layer that captures events and forwards them to the
/// uploader task via an mpsc channel. Construct with [`new`] which
/// returns the layer + the consumer end of the channel; pass the
/// receiver into [`run_uploader`] once config is loaded.
pub struct LogUploadLayer {
    tx: mpsc::Sender<LogLine>,
}

impl LogUploadLayer {
    /// Construct a layer + receiver pair. Receiver is given to the
    /// background uploader task; if no uploader is started, the layer
    /// silently drops events when the channel fills.
    pub fn new() -> (Self, mpsc::Receiver<LogLine>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        (Self { tx }, rx)
    }
}

impl<S> Layer<S> for LogUploadLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let level = match *metadata.level() {
            tracing::Level::TRACE => LogLevel::Trace,
            tracing::Level::DEBUG => LogLevel::Debug,
            tracing::Level::INFO => LogLevel::Info,
            tracing::Level::WARN => LogLevel::Warn,
            tracing::Level::ERROR => LogLevel::Error,
        };
        let target = metadata.target().to_string();

        // Skip our own uploader's failure logs to avoid feedback loops
        // ("upload failed" → another event → "upload failed" → ...).
        // Target prefix matches this module's tracing::span! site.
        if target == "roomler_agent::logs_upload" {
            return;
        }

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let line = LogLine {
            ts: DateTime::now(),
            level,
            target,
            msg: visitor.message,
            fields: visitor.fields,
        };
        // Non-blocking send — drop on full channel. The on-disk file
        // log retains everything regardless.
        let _ = self.tx.try_send(line);
    }
}

/// Visitor that splits a tracing event's fields into the conventional
/// `message` (the `"some literal {}"` interpolated arg) plus the
/// structured key=value pairs.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: Document,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let name = field.name();
        let s = format!("{value:?}");
        if name == "message" {
            // tracing wraps Debug output in quotes for &str; strip them
            // so the persisted msg matches the visible log line.
            self.message = strip_outer_quotes(&s);
        } else {
            self.fields.insert(name, s);
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.insert(field.name(), value.to_string());
        }
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields.insert(field.name(), value);
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        // BSON i64 is the closest fit; clamp on overflow to keep the
        // round-trip lossless for the common case (Unix timestamps,
        // byte counts under 2^63).
        let v = if value <= i64::MAX as u64 {
            value as i64
        } else {
            i64::MAX
        };
        self.fields.insert(field.name(), v);
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.fields.insert(field.name(), value);
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields.insert(field.name(), value);
    }
}

fn strip_outer_quotes(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Configuration captured AFTER the agent config is loaded. Held by
/// the uploader task for the life of the process; if the agent JWT
/// rolls, the task should be restarted (currently a process restart
/// is required — rotation is rare).
#[derive(Debug, Clone)]
pub struct UploadConfig {
    pub server_url: String,
    pub tenant_id: String,
    pub agent_id: String,
    pub agent_jwt: String,
    pub agent_version: String,
    pub host_id_hash: String,
    pub source: LogSource,
}

/// Long-running task: drain the channel every [`FLUSH_INTERVAL_SECS`]
/// and POST batches up to [`MAX_BATCH_LINES`] lines.
///
/// Exits cleanly when the layer is dropped (channel closes).
pub async fn run_uploader(mut rx: mpsc::Receiver<LogLine>, config: UploadConfig) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            // Without a client we can't upload anything. Drain the
            // channel forever so the layer doesn't backpressure.
            tracing::warn!(target: "roomler_agent::logs_upload", %e, "reqwest client build failed; logs upload disabled");
            while rx.recv().await.is_some() {}
            return;
        }
    };
    let url = format!(
        "{}/api/tenant/{}/agent/{}/logs",
        config.server_url.trim_end_matches('/'),
        config.tenant_id,
        config.agent_id,
    );
    let mut buf: Vec<LogLine> = Vec::with_capacity(MAX_BATCH_LINES);
    let mut backoff_secs = INITIAL_RETRY_BACKOFF_SECS;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(FLUSH_INTERVAL_SECS)).await;

        // Drain up to MAX_BATCH_LINES from the channel without
        // blocking on an empty channel.
        let mut closed = false;
        while buf.len() < MAX_BATCH_LINES {
            match rx.try_recv() {
                Ok(line) => buf.push(line),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        if buf.is_empty() {
            if closed {
                return;
            }
            continue;
        }

        let payload = serde_json::json!({
            "source": config.source.as_str(),
            "agent_version": config.agent_version,
            "host_id_hash": config.host_id_hash,
            "lines": &buf,
        });

        let send = client
            .post(&url)
            .bearer_auth(&config.agent_jwt)
            .json(&payload)
            .send()
            .await;

        match send {
            Ok(resp) if resp.status().is_success() => {
                buf.clear();
                backoff_secs = INITIAL_RETRY_BACKOFF_SECS;
            }
            Ok(resp)
                if resp.status() == reqwest::StatusCode::UNAUTHORIZED
                    || resp.status() == reqwest::StatusCode::FORBIDDEN =>
            {
                // Permanent for this JWT — drop the batch to free buf.
                // Common cause: agent unenrolled or token rotated.
                buf.clear();
                backoff_secs = MAX_RETRY_BACKOFF_SECS;
            }
            Ok(resp) => {
                // Other non-success (500, 422, etc.) — keep batch, backoff.
                // 422 means the server rejected our shape; logging it at
                // WARN lets us spot persistent malformed batches without
                // a tight retry loop.
                let status = resp.status();
                tracing::warn!(target: "roomler_agent::logs_upload", %status, "logs upload non-success");
                tokio::time::sleep(std::time::Duration::from_secs(
                    backoff_secs.saturating_sub(FLUSH_INTERVAL_SECS),
                ))
                .await;
                backoff_secs = (backoff_secs * 2).min(MAX_RETRY_BACKOFF_SECS);
            }
            Err(e) => {
                // Transport-level failure (DNS / connect / timeout) —
                // keep batch, backoff. Don't log every time: that would
                // re-fire on every tick when offline. Suppress to
                // tracing::debug so the rolling log stays useful but
                // doesn't flood.
                tracing::debug!(target: "roomler_agent::logs_upload", %e, "logs upload error");
                tokio::time::sleep(std::time::Duration::from_secs(
                    backoff_secs.saturating_sub(FLUSH_INTERVAL_SECS),
                ))
                .await;
                backoff_secs = (backoff_secs * 2).min(MAX_RETRY_BACKOFF_SECS);
            }
        }

        // If batch hits the cap after a failure (queue keeps growing),
        // shed the oldest half so the buf doesn't grow unboundedly.
        // Lost diagnostic data is preferable to OOM.
        if buf.len() >= MAX_BATCH_LINES {
            let drop_n = buf.len() / 2;
            buf.drain(0..drop_n);
        }
    }
}

/// Parse the `ROOMLER_AGENT_LOGS_UPLOAD_DISABLED` env-var. Accepts
/// `1`, `true`, `yes`, `on` (case-insensitive, trimmed) as truthy;
/// anything else (including `None`) is false — i.e. uploads stay ON
/// per the rc.58 default-on policy.
pub fn parse_disable_flag(value: Option<&str>) -> bool {
    match value {
        Some(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        None => false,
    }
}

/// SHA256-hash the hostname (lowercased, trimmed). Hex-encoded; 64
/// chars. Used as the `host_id_hash` field on uploaded batches so the
/// admin UI can group lines by host without revealing the raw name.
/// Mirrors the rule established in `ed69c6c` (PII scrub): code never
/// holds the raw hostname; telemetry doesn't either.
pub fn hash_hostname(name: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(name.trim().to_ascii_lowercase().as_bytes());
    let out = hasher.finalize();
    hex::encode(out)
}

#[allow(dead_code)]
fn _arc_assert_send_sync<T: Send + Sync>(_: Arc<T>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_disable_flag_accepts_truthy() {
        assert!(parse_disable_flag(Some("1")));
        assert!(parse_disable_flag(Some("true")));
        assert!(parse_disable_flag(Some("True")));
        assert!(parse_disable_flag(Some("yes")));
        assert!(parse_disable_flag(Some("on")));
        assert!(parse_disable_flag(Some("  ON  ")));
    }

    #[test]
    fn parse_disable_flag_rejects_falsy() {
        assert!(!parse_disable_flag(None));
        assert!(!parse_disable_flag(Some("")));
        assert!(!parse_disable_flag(Some("0")));
        assert!(!parse_disable_flag(Some("false")));
        assert!(!parse_disable_flag(Some("no")));
        assert!(!parse_disable_flag(Some("off")));
        assert!(!parse_disable_flag(Some("disabled")));
    }

    #[test]
    fn hash_hostname_is_stable() {
        // Same input → same hash; trim + lowercase normalisation.
        assert_eq!(hash_hostname("PC50045"), hash_hostname("pc50045"));
        assert_eq!(hash_hostname("PC50045"), hash_hostname("  PC50045  "));
        assert_ne!(hash_hostname("PC50045"), hash_hostname("PC50046"));
        // Output length is the SHA256 hex (64 chars).
        assert_eq!(hash_hostname("PC50045").len(), 64);
    }

    #[test]
    fn strip_outer_quotes_handles_short_strings() {
        assert_eq!(strip_outer_quotes(""), "");
        assert_eq!(strip_outer_quotes("\""), "\"");
        assert_eq!(strip_outer_quotes("\"\""), "");
        assert_eq!(strip_outer_quotes("\"abc\""), "abc");
        assert_eq!(strip_outer_quotes("abc"), "abc");
    }

    /// Smoke test: layer constructor returns a working channel.
    #[tokio::test]
    async fn layer_construction_round_trip() {
        let (_layer, mut rx) = LogUploadLayer::new();
        // Nothing should be in the channel yet.
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}

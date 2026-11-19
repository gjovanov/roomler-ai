// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! rc.58 — centralized log uploader.
//!
//! A `tracing-subscriber::Layer` that captures every emitted event,
//! converts it into a [`LogLine`], and forwards it to a tokio mpsc
//! channel. A background task drains the channel every 5 s and POSTs
//! batches of up to 500 lines to
//! `POST /api/tenant/{tenant_id}/agent/{agent_id}/logs` (auth: agent
//! JWT).
//!
//! Default ON. Kill switch: `ROOMLERD_LOGS_UPLOAD_DISABLED=1`.
//!
//! Failure modes:
//! - Channel full (uploader too slow / network dead) → newest events
//!   dropped at the layer (`try_send` returns Err). Acceptable for
//!   diagnostic data; the rolling on-disk file logs retain everything.
//!   #3272 — but counted, and the next batch carries a `dropped=N` line, so a
//!   fleet read can tell a quiet host from one that overflowed this channel. It
//!   counts this drop alone: a batch shed below or refused 401/403 is not in it,
//!   and that batch can take the `dropped=N` line with it.
//! - 401 / 403 from server → batch dropped, longer backoff (likely
//!   JWT expired / agent unenrolled).
//! - #2512: the control socket heard the server refuse this enrollment's
//!   token → nothing is posted (batches held, shed when full) until a new
//!   token or the socket's next accepted upgrade.
//! - Other transient failures → batch retained, exponential backoff
//!   capped at 60 s.

use bson::{DateTime, Document};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// #4067 — how long an idle connection may wait in the uploader's pool for
/// the next batch. reqwest's default is 90 s, and the server closes an idle
/// HTTP/1.1 connection after 30 s (#4018 measured it). A batch goes out on the
/// first flush tick that finds lines queued, so a batch six ticks after the
/// last one rode a connection idle for about 30 s, just as the server closed
/// it: a request written in the last few tens of milliseconds before the
/// server's FIN gets no answer. 10 s is a third of that close, the value of
/// the self-view client's `POOL_IDLE_TIMEOUT`
/// (`agents/roomlerd/src/self_view.rs`, #4066). A batch after a quiet spell
/// dials a new connection, and a busy log's batches, one per tick, still share
/// one. No POST is asked again at once: a failed batch is kept and sent after
/// the backoff.
pub const POOL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const _: () = assert!(
    POOL_IDLE_TIMEOUT.as_millis() <= 20_000,
    "the pool must drop an idle connection well before the server's 30 s idle close (#4018)"
);

/// #3272 — lines the layer has dropped since the last batch said so. One
/// process has one layer and one uploader, so a static joins them without
/// changing what either hands over. A flood skews every fleet read off
/// `agent_logs`: one spinning host had 52 heartbeats on disk and 15 uploaded,
/// and nothing said so.
static LINES_DROPPED: AtomicU64 = AtomicU64::new(0);

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

    /// Non-blocking send — drop on a full channel, and count it. The on-disk
    /// file log retains everything regardless.
    fn forward(&self, line: LogLine, dropped: &AtomicU64) {
        if self.tx.try_send(line).is_err() {
            dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The line that tells a reader how many went missing, added to `buf` if any
/// did. Taken from the counter (it restarts at zero) only as it goes into a
/// batch, and a batch always has room: the buffer starts every flush under
/// [`MAX_BATCH_LINES`].
fn note_dropped_lines(buf: &mut Vec<LogLine>, dropped: &AtomicU64) {
    let n = dropped.swap(0, Ordering::Relaxed);
    if n == 0 {
        return;
    }
    let mut fields = Document::new();
    fields.insert("dropped", i64::try_from(n).unwrap_or(i64::MAX));
    buf.push(LogLine {
        ts: DateTime::now(),
        level: LogLevel::Warn,
        target: "roomlerd::logs_upload".to_string(),
        msg: "log upload channel was full: lines dropped since the last batch".to_string(),
        fields,
    });
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
        if target == "roomlerd::logs_upload" {
            return;
        }

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let line = LogLine {
            ts: DateTime::now(),
            level,
            target,
            msg: visitor.message.unwrap_or_default(),
            fields: visitor.fields,
        };
        self.forward(line, &LINES_DROPPED);
    }
}

/// Splits an event's fields into its text, the FIRST `message` (tracing's macros put the format
/// string first), and the key=value pairs. #3816 — a later field named `message` is one of the
/// pairs: keeping the last let a field replace an uploaded line's text.
#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    fields: Document,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let name = field.name();
        let s = format!("{value:?}");
        if name == "message" && self.message.is_none() {
            // tracing wraps Debug output in quotes for &str; strip them
            // so the persisted msg matches the visible log line.
            self.message = Some(strip_outer_quotes(&s));
        } else {
            self.fields.insert(name, s);
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" && self.message.is_none() {
            self.message = Some(value.to_string());
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
/// the uploader task for the life of the process.
#[derive(Debug, Clone)]
pub struct UploadConfig {
    pub server_url: String,
    pub tenant_id: String,
    pub agent_id: String,
    /// FR-103 — the enrollment's LIVE token, read per batch, so a renewal
    /// reaches the uploader without a restart.
    pub agent_jwt: crate::token_slot::TokenSlot,
    pub agent_version: String,
    pub host_id_hash: String,
    pub source: LogSource,
}

/// The uploader's clock: [`Pace::PRODUCTION`], or a fast one in a test.
#[derive(Debug, Clone, Copy)]
struct Pace {
    flush: std::time::Duration,
    initial_backoff: std::time::Duration,
    max_backoff: std::time::Duration,
}

impl Pace {
    const PRODUCTION: Pace = Pace {
        flush: std::time::Duration::from_secs(FLUSH_INTERVAL_SECS),
        initial_backoff: std::time::Duration::from_secs(INITIAL_RETRY_BACKOFF_SECS),
        max_backoff: std::time::Duration::from_secs(MAX_RETRY_BACKOFF_SECS),
    };
}

/// Long-running task: drain the channel every [`FLUSH_INTERVAL_SECS`]
/// and POST batches up to [`MAX_BATCH_LINES`] lines.
///
/// Exits cleanly when the layer is dropped (channel closes).
pub async fn run_uploader(rx: mpsc::Receiver<LogLine>, config: UploadConfig) {
    run_uploader_paced(rx, config, Pace::PRODUCTION).await
}

async fn run_uploader_paced(mut rx: mpsc::Receiver<LogLine>, config: UploadConfig, pace: Pace) {
    let client = match client_builder(POOL_IDLE_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            // Without a client we can't upload anything. Drain the
            // channel forever so the layer doesn't backpressure.
            tracing::warn!(target: "roomlerd::logs_upload", %e, "reqwest client build failed; logs upload disabled");
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
    let mut backoff = pace.initial_backoff;

    loop {
        tokio::time::sleep(pace.flush).await;

        note_dropped_lines(&mut buf, &LINES_DROPPED);

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

        // #2512 — the server refuses this enrollment's token, as the control
        // socket heard it (`TokenSlot::is_refused`). A POST now is one it
        // will only 401, and a device whose row was gone posted every 5 s,
        // forever. Hold the batch (shedding as below) until a new token or
        // the socket's next accepted upgrade.
        if config.agent_jwt.is_refused() {
            if closed {
                return;
            }
            shed_oldest_half_if_full(&mut buf);
            continue;
        }

        let send = post_batch(&client, &url, &config, &buf).await;

        match send {
            Ok(resp) if resp.status().is_success() => {
                buf.clear();
                backoff = pace.initial_backoff;
            }
            Ok(resp)
                if resp.status() == reqwest::StatusCode::UNAUTHORIZED
                    || resp.status() == reqwest::StatusCode::FORBIDDEN =>
            {
                // Permanent for this JWT — drop the batch to free buf.
                // Common cause: agent unenrolled or token rotated.
                buf.clear();
                // #2512 — and wait the long backoff out, as the module doc
                // promises. This arm used to set it and go straight back to
                // the flush tick, so nothing ever waited on it.
                backoff = pace.max_backoff;
                tokio::time::sleep(backoff.saturating_sub(pace.flush)).await;
            }
            Ok(resp) => {
                // Other non-success (500, 422, etc.) — keep batch, backoff.
                // 422 means the server rejected our shape; logging it at
                // WARN lets us spot persistent malformed batches without
                // a tight retry loop.
                let status = resp.status();
                tracing::warn!(target: "roomlerd::logs_upload", %status, "logs upload non-success");
                tokio::time::sleep(backoff.saturating_sub(pace.flush)).await;
                backoff = (backoff * 2).min(pace.max_backoff);
            }
            Err(e) => {
                // Transport-level failure (DNS / connect / timeout) —
                // keep batch, backoff. Don't log every time: that would
                // re-fire on every tick when offline. Suppress to
                // tracing::debug so the rolling log stays useful but
                // doesn't flood.
                tracing::debug!(target: "roomlerd::logs_upload", %e, "logs upload error");
                tokio::time::sleep(backoff.saturating_sub(pace.flush)).await;
                backoff = (backoff * 2).min(pace.max_backoff);
            }
        }

        shed_oldest_half_if_full(&mut buf);
    }
}

/// The uploader's HTTP client, with `pool_idle` as its pool's idle timeout:
/// [`POOL_IDLE_TIMEOUT`] in production, a short one in a test.
fn client_builder(pool_idle: std::time::Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .pool_idle_timeout(pool_idle)
}

/// POSTs one batch of `lines` to `url`, with the enrollment's live token. What
/// the answer, or a failed send, does to the batch is the caller's to decide.
async fn post_batch(
    client: &reqwest::Client,
    url: &str,
    config: &UploadConfig,
    lines: &[LogLine],
) -> reqwest::Result<reqwest::Response> {
    let payload = serde_json::json!({
        "source": config.source.as_str(),
        "agent_version": config.agent_version,
        "host_id_hash": config.host_id_hash,
        "lines": lines,
    });
    client
        .post(url)
        .bearer_auth(config.agent_jwt.get())
        .json(&payload)
        .send()
        .await
}

/// If the batch hits the cap after a failure (the queue keeps growing),
/// shed the oldest half so the buf doesn't grow unboundedly. Lost diagnostic
/// data is preferable to OOM.
fn shed_oldest_half_if_full(buf: &mut Vec<LogLine>) {
    if buf.len() >= MAX_BATCH_LINES {
        let drop_n = buf.len() / 2;
        buf.drain(0..drop_n);
    }
}

/// Parse the `ROOMLERD_LOGS_UPLOAD_DISABLED` env-var. Accepts
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
        assert_eq!(hash_hostname("WINHOST-A"), hash_hostname("winhost-a"));
        assert_eq!(hash_hostname("WINHOST-A"), hash_hostname("  WINHOST-A  "));
        assert_ne!(hash_hostname("WINHOST-A"), hash_hostname("WINHOST-D"));
        // Output length is the SHA256 hex (64 chars).
        assert_eq!(hash_hostname("WINHOST-A").len(), 64);
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

    /// #3272 — a full channel loses the line and counts it. `forward` takes the
    /// counter by reference, so this test does not touch the process's own.
    #[tokio::test]
    async fn a_full_channel_counts_what_it_drops() {
        let (tx, mut rx) = mpsc::channel(2);
        let layer = LogUploadLayer { tx };
        let dropped = AtomicU64::new(0);
        let line = || LogLine {
            ts: DateTime::now(),
            level: LogLevel::Info,
            target: "test".into(),
            msg: "x".into(),
            fields: Document::new(),
        };
        for _ in 0..5 {
            layer.forward(line(), &dropped);
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 3);
        assert!(rx.try_recv().is_ok());
        layer.forward(line(), &dropped);
        assert_eq!(dropped.load(Ordering::Relaxed), 3, "room again, none lost");
    }

    /// #3272 — the count reaches a batch as one line, with the count in its
    /// fields, and restarts. No drops, no line: a healthy host's batches do not
    /// change at all.
    #[test]
    fn the_count_goes_into_the_batch_once_and_only_when_there_is_one() {
        let dropped = AtomicU64::new(0);
        let mut buf = Vec::new();
        note_dropped_lines(&mut buf, &dropped);
        assert!(buf.is_empty());

        dropped.store(1234, Ordering::Relaxed);
        note_dropped_lines(&mut buf, &dropped);
        assert_eq!(buf.len(), 1);
        assert_eq!(buf[0].level, LogLevel::Warn);
        assert_eq!(buf[0].fields.get_i64("dropped"), Ok(1234));
        assert_eq!(dropped.load(Ordering::Relaxed), 0);

        note_dropped_lines(&mut buf, &dropped);
        assert_eq!(buf.len(), 1, "a count is said once");
    }

    /// A batch stays within the server's cap with the line in it. The buffer
    /// starts every flush below the cap, because a failed POST sheds a full
    /// buffer, so the one line always fits before the drain tops it up.
    #[test]
    fn the_line_never_pushes_a_batch_over_the_cap() {
        let line = LogLine {
            ts: DateTime::now(),
            level: LogLevel::Info,
            target: "test".into(),
            msg: "x".into(),
            fields: Document::new(),
        };
        let mut buf = vec![line; MAX_BATCH_LINES];
        shed_oldest_half_if_full(&mut buf);
        assert!(buf.len() < MAX_BATCH_LINES);
        let dropped = AtomicU64::new(7);
        note_dropped_lines(&mut buf, &dropped);
        assert!(buf.len() <= MAX_BATCH_LINES);
    }

    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// The production pace scaled down 100×: flush every 50 ms, back off
    /// from 50 ms up to 600 ms.
    const FAST: Pace = Pace {
        flush: Duration::from_millis(50),
        initial_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(600),
    };

    /// Reads one whole request off `s`, its head and then a body of its
    /// Content-Length, and takes it out of `buf`, which keeps any bytes past
    /// it for the next one. `false` when the connection ends first.
    async fn read_request(s: &mut tokio::net::TcpStream, buf: &mut Vec<u8>) -> bool {
        use tokio::io::AsyncReadExt;
        let mut chunk = [0u8; 8192];
        let mut want: Option<usize> = None;
        loop {
            if want.is_none()
                && let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n")
            {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                want = Some(end + 4 + len);
            }
            if let Some(total) = want
                && buf.len() >= total
            {
                buf.drain(..total);
                return true;
            }
            match s.read(&mut chunk).await {
                Ok(0) | Err(_) => return false,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// A loopback HTTP server that reads each whole request, counts it, and
    /// answers `status` with an empty body. Returns its base URL and the count.
    async fn counting_server(status: &'static str) -> (String, Arc<AtomicUsize>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let posts = Arc::new(AtomicUsize::new(0));
        let seen = posts.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    if !read_request(&mut s, &mut Vec::new()).await {
                        return;
                    }
                    seen.fetch_add(1, Ordering::SeqCst);
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = s.write_all(reply.as_bytes()).await;
                });
            }
        });
        (base, posts)
    }

    struct KeptServer {
        base: String,
        connections: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
    }

    impl KeptServer {
        /// `(connections accepted, requests read)` so far.
        fn seen(&self) -> (usize, usize) {
            (
                self.connections.load(Ordering::SeqCst),
                self.requests.load(Ordering::SeqCst),
            )
        }
    }

    /// #4067 — a loopback HTTP/1.1 server that answers every batch as the
    /// server does, 201 and a short JSON body, and keeps the connection, so
    /// the client's pool alone decides when one closes.
    async fn keep_alive_server() -> KeptServer {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        {
            let connections = connections.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                while let Ok((mut s, _)) = listener.accept().await {
                    connections.fetch_add(1, Ordering::SeqCst);
                    let requests = requests.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        while read_request(&mut s, &mut buf).await {
                            requests.fetch_add(1, Ordering::SeqCst);
                            let body = r#"{"status":"accepted"}"#;
                            let reply = format!(
                                "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\n\
                                 content-length: {}\r\n\r\n{body}",
                                body.len()
                            );
                            if s.write_all(reply.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
        }
        KeptServer {
            base,
            connections,
            requests,
        }
    }

    fn config(base: &str, token: &crate::token_slot::TokenSlot) -> UploadConfig {
        UploadConfig {
            server_url: base.to_string(),
            tenant_id: "t1".into(),
            agent_id: "a1".into(),
            agent_jwt: token.clone(),
            agent_version: "0.0.0-test".into(),
            host_id_hash: hash_hostname("host"),
            source: LogSource::Agent,
        }
    }

    /// Feed one line every 10 ms until the channel closes, as a busy daemon's
    /// log does, so every flush finds a batch.
    fn feed(tx: mpsc::Sender<LogLine>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let line = LogLine {
                    ts: DateTime::now(),
                    level: LogLevel::Warn,
                    target: "test".into(),
                    msg: "overlay derp: /derp connect failed".into(),
                    fields: Document::new(),
                };
                if tx.send(line).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    }

    /// #2512 — a 401 waits out the long backoff, as the module doc says.
    /// Before, the arm set the backoff and went straight back to the flush
    /// tick: one POST per flush, forever (88 in 13 minutes in the field).
    #[tokio::test]
    async fn a_401_waits_the_long_backoff_out() {
        let (base, posts) = counting_server("401 Unauthorized").await;
        let token = crate::token_slot::TokenSlot::new("gone");
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let feeder = feed(tx);
        let uploader = tokio::spawn(run_uploader_paced(rx, config(&base, &token), FAST));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        uploader.abort();
        feeder.abort();
        let n = posts.load(Ordering::SeqCst);
        // At ~50 ms, ~650 ms and ~1250 ms, where the flush tick alone made ~30.
        assert!((1..=4).contains(&n), "{n} POSTs in 1.5 s against a 401");
    }

    /// #2512 — while the control socket's verdict on the token is "refused",
    /// nothing is posted at all. A renewed token resumes the uploads.
    #[tokio::test]
    async fn a_refused_token_posts_nothing_until_it_changes() {
        let (base, posts) = counting_server("204 No Content").await;
        let token = crate::token_slot::TokenSlot::new("gone");
        assert!(token.mark_refused("gone"));
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let feeder = feed(tx);
        let uploader = tokio::spawn(run_uploader_paced(rx, config(&base, &token), FAST));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            posts.load(Ordering::SeqCst),
            0,
            "a refused token posts nothing"
        );
        assert!(token.set("renewed".into()));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while posts.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "a renewed token's uploads resume on the next flush"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        uploader.abort();
        feeder.abort();
    }

    /// #4067 — the pool's idle timeout is the builder's, as #4066's is for the
    /// self-view client: two batches 50 ms apart share one connection, and a
    /// batch after the connection sat idle longer than the timeout dials a new
    /// one. Production's value is held at 20 s or less by a const assertion
    /// beside [`POOL_IDLE_TIMEOUT`]. The server answers as ours does, 201 with
    /// a body nobody reads, so a connection is kept as it is in the field. The
    /// timeout is 2 s so that a loaded CI runner would have to stall for almost
    /// 2 s between the first two batches to split them.
    #[tokio::test]
    async fn a_connection_idle_past_the_pool_timeout_never_carries_a_batch() {
        const IDLE: Duration = Duration::from_secs(2);
        let srv = keep_alive_server().await;
        let client = client_builder(IDLE).no_proxy().build().unwrap();
        let token = crate::token_slot::TokenSlot::new("tok");
        let cfg = config(&srv.base, &token);
        let url = format!("{}/api/tenant/t1/agent/a1/logs", srv.base);
        let lines = [LogLine {
            ts: DateTime::now(),
            level: LogLevel::Info,
            target: "test".into(),
            msg: "x".into(),
            fields: Document::new(),
        }];
        let sent = post_batch(&client, &url, &cfg, &lines).await;
        assert!(sent.unwrap().status().is_success());
        tokio::time::sleep(Duration::from_millis(50)).await;
        let sent = post_batch(&client, &url, &cfg, &lines).await;
        assert!(sent.unwrap().status().is_success());
        assert_eq!(srv.seen(), (1, 2), "50 ms apart: one connection");
        tokio::time::sleep(IDLE + Duration::from_millis(500)).await;
        let sent = post_batch(&client, &url, &cfg, &lines).await;
        assert!(sent.unwrap().status().is_success());
        assert_eq!(
            srv.seen(),
            (2, 3),
            "idle past the timeout: a new connection"
        );
    }

    /// #3816 — an uploaded line keeps its own text beside a field named
    /// `message`. tracing calls a line's text `message` too, and its macros put
    /// the text first, so the first `message` is the text and a later one is a
    /// field. The visitor used to keep the last: the clipboard's "rich write
    /// failed" line went up with its error for its text. A `message` with no
    /// text beside it is still the line's text.
    #[test]
    fn a_field_named_message_never_replaces_the_lines_own_text() {
        use tracing_subscriber::layer::SubscriberExt as _;
        let (layer, mut rx) = LogUploadLayer::new();
        let message = String::from("rich transfer n1 ended at 0 of 2 declared bytes");
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            tracing::warn!(id = "n1", ?message, "clipboard: rich write failed");
            tracing::warn!(id = "n1", %message, "clipboard: rich write failed");
            tracing::warn!(message = "a field's text", "the line's text");
            tracing::info!(id = "n1", message = %message);
        });
        let mut next = || rx.try_recv().expect("the layer forwards every line");

        let debug = next();
        assert_eq!(debug.msg, "clipboard: rich write failed");
        assert_eq!(
            debug.fields.get_str("message"),
            Ok("\"rich transfer n1 ended at 0 of 2 declared bytes\"")
        );
        assert_eq!(debug.fields.get_str("id"), Ok("n1"));

        let display = next();
        assert_eq!(display.msg, "clipboard: rich write failed");
        assert_eq!(
            display.fields.get_str("message"),
            Ok("rich transfer n1 ended at 0 of 2 declared bytes")
        );

        let text = next();
        assert_eq!(text.msg, "the line's text");
        assert_eq!(text.fields.get_str("message"), Ok("a field's text"));

        let lone = next();
        assert_eq!(lone.msg, "rich transfer n1 ended at 0 of 2 declared bytes");
        assert_eq!(
            lone.fields.get("message"),
            None,
            "a lone message is the text"
        );
    }
}

//! Operator-consent broker (Plan 3 of the file-DC v2 follow-on
//! cycle).
//!
//! Replaces the agent's session-level auto-grant in `signaling.rs`
//! with a decision flow that — for org-controlled fleets — keeps a
//! human in the loop before a remote-control session starts. Today's
//! self-hosted user typically just controls their own machines, so
//! the default `auto_grant_session = true` preserves the historical
//! behaviour: the broker resolves to `Decision::Granted` immediately
//! and the wire flow is unchanged.
//!
//! When `auto_grant_session = false`, the broker waits for a sentinel
//! file under `<log_dir>/consent/` to appear. The sentinel is dropped
//! by an out-of-process operator running:
//!
//! ```text
//! roomler-agent consent --session <hex_id> --approve
//! roomler-agent consent --session <hex_id> --deny
//! ```
//!
//! 30 s timeout → auto-deny. Outcome propagates back through the
//! [`ConsentBroker::request`] future the signaling layer awaits.
//!
//! The tray popup (Phase 3) now renders this prompt on attended
//! sessions: the signaling layer calls [`ConsentBroker::write_pending`]
//! to drop a `<session>.pending` marker in this same dir, the
//! `roomler-agent-tray` companion watches for it and shows an
//! Approve/Deny modal, and the operator's choice writes the same
//! `.approve`/`.deny` sentinel the poll loop below already consumes.
//! The CLI subcommand remains a headless fallback.
//!
//! Audit lifecycle: hub.rs already emits
//! [`AuditKind::ConsentPrompted`] on `rc:request` send and
//! `ConsentGranted` / `ConsentDenied` on `rc:consent` receive, plus
//! `ConsentTimedOut` on its own server-side timeout
//! (`DEFAULT_CONSENT_TIMEOUT`). The agent simply emits one of two
//! `rc:consent` shapes (granted=true / granted=false) — no new wire
//! types needed.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Default operator-decision timeout when an operator-consent prompt
/// is required. 30 s matches the planner's spec; deliberately on the
/// short side because the controller's own
/// `consent_timeout_secs` (sent via `rc:request`) is the upper bound
/// past which the controller gives up. Both sides converging on the
/// same wall-clock means the operator's ~30 s window aligns with the
/// browser's own progress UI.
pub const DEFAULT_PROMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Polling interval when watching for a sentinel-file decision. Fast
/// enough that the operator perceives "click → resolve" as instant
/// (sub-second) without burning CPU on a permanent poll loop.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Outcome of a consent prompt. The agent sends `rc:consent
/// { granted }` based on this; if Timeout fires, the agent sends
/// `granted: false` (the safe-default closes the Hub's wait).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Granted,
    Denied,
    /// Operator did not respond within the prompt timeout. Treat as
    /// denial on the wire (the hub's own audit pipeline records this
    /// distinctly from an explicit deny).
    Timeout,
}

impl Decision {
    /// Map to the `granted` boolean on `ClientMsg::Consent`. Both
    /// `Denied` and `Timeout` produce `false` — the hub's server-side
    /// audit / heuristic distinguishes them via its own timer.
    pub fn granted(self) -> bool {
        matches!(self, Decision::Granted)
    }
}

/// Pure helper deciding what to do BEFORE consulting any
/// out-of-process state. Lifted out of [`ConsentBroker::request`] so
/// the contract is easy to lock with unit tests:
///
/// - `auto_grant=true` → immediate grant, no prompt, no sentinel.
/// - `auto_grant=false` → prompt path; broker watches the sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Skip the prompt, immediate `Decision::Granted`.
    AutoGrant,
    /// Wait for an out-of-process decision via the sentinel.
    Prompt { timeout: Duration },
}

impl Mode {
    pub fn from_config(auto_grant: bool) -> Self {
        if auto_grant {
            Mode::AutoGrant
        } else {
            Mode::Prompt {
                timeout: DEFAULT_PROMPT_TIMEOUT,
            }
        }
    }
}

/// Cross-platform consent broker. One instance per agent process.
/// Cheap to clone — internal state is `Arc<Mutex<...>>`. Thread-safe.
#[derive(Clone)]
pub struct ConsentBroker {
    inner: Arc<BrokerInner>,
}

struct BrokerInner {
    mode: Mode,
    /// Directory where sentinel files are read/written. One file
    /// per pending session: `<sentinel_dir>/<session_hex>.{approve|deny}`.
    /// The CLI subcommand creates these with a tiny "now()" payload
    /// that the broker's poll loop discovers.
    sentinel_dir: PathBuf,
    /// Sessions currently awaiting a decision. Populated when
    /// [`request`] is called, cleared when the sentinel arrives or
    /// the timeout fires. A stale entry past timeout would simply
    /// see its watcher exit naturally — there's no leaked task.
    pending: Mutex<std::collections::HashSet<String>>,
}

impl ConsentBroker {
    /// Build a new broker. `sentinel_dir` is created if absent. On
    /// Unix the directory is `chmod 700` to match `config.toml`'s
    /// 0600 posture (the sentinel files leak only "yes/no" decisions,
    /// but the convention is "agent state lives at 700").
    pub fn new(mode: Mode, sentinel_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&sentinel_dir)
            .with_context(|| format!("creating sentinel dir {}", sentinel_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&sentinel_dir)?.permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(&sentinel_dir, perms)?;
        }
        Ok(Self {
            inner: Arc::new(BrokerInner {
                mode,
                sentinel_dir,
                pending: Mutex::new(std::collections::HashSet::new()),
            }),
        })
    }

    /// Convenience: build the default sentinel directory under the
    /// agent's log dir. Mirrors the path layout used by the
    /// `last-install.json`/`needs-attention.txt` sentinels in
    /// `updater.rs` so operators only have to remember one location.
    pub fn default_sentinel_dir() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("live", "roomler", "roomler-agent")
            .context("could not resolve a platform data directory")?;
        Ok(dirs.data_dir().join("logs").join("consent"))
    }

    /// Path of the sentinel file for `(session_hex, decision)`.
    /// Public so the CLI subcommand can write to it.
    pub fn sentinel_path(&self, session_hex: &str, kind: SentinelKind) -> PathBuf {
        self.inner
            .sentinel_dir
            .join(format!("{}.{}", session_hex, kind.suffix()))
    }

    /// Sentinel directory in use. Public so the CLI can list pending
    /// sessions.
    pub fn sentinel_dir(&self) -> &Path {
        &self.inner.sentinel_dir
    }

    /// Mode the broker was built with. Mostly used by unit tests +
    /// the signaling-layer log line.
    pub fn mode(&self) -> Mode {
        self.inner.mode
    }

    /// Request consent for `session_hex`. Resolves to `Granted`
    /// immediately when configured for auto-grant; otherwise polls
    /// the sentinel directory until an `.approve` / `.deny` appears
    /// or the prompt timeout expires.
    ///
    /// On success the matching sentinel files (both approve+deny
    /// with the same session id) are cleaned up — operators dropping
    /// stale `.approve` files won't accidentally pre-approve a
    /// future session.
    pub async fn request(&self, session_hex: &str) -> Decision {
        self.request_with_mode(session_hex, self.inner.mode).await
    }

    /// Like [`request`], but drives the decision with an explicit per-session
    /// `mode` — the server's Phase-2 consent directive
    /// (`ServerMsg::Request { consent_mode }`) — instead of the broker's startup
    /// mode. Server-authoritative consent: the agent obeys the server rather
    /// than its local `auto_grant_session`. The sentinel dir + poll loop are
    /// shared state, so a broker built for `AutoGrant` can still prompt on
    /// demand when the server directs a `Prompt`.
    pub async fn request_with_mode(&self, session_hex: &str, mode: Mode) -> Decision {
        // Reject anything that doesn't look like a hex session id.
        // Stops a stray empty-string request from scanning the
        // entire sentinel dir.
        if session_hex.is_empty() || session_hex.len() > 64 {
            tracing::warn!(session = session_hex, "consent request with implausible id");
            return Decision::Denied;
        }
        match mode {
            Mode::AutoGrant => Decision::Granted,
            Mode::Prompt { timeout } => self.run_prompt(session_hex, timeout).await,
        }
    }

    async fn run_prompt(&self, session_hex: &str, timeout: Duration) -> Decision {
        // Stamp this session as pending; safe to clear unconditionally
        // at exit because run_prompt fully owns its own decision flow.
        {
            let mut pending = self.inner.pending.lock().await;
            pending.insert(session_hex.to_string());
        }
        tracing::info!(
            session = session_hex,
            timeout_secs = timeout.as_secs(),
            sentinel_dir = %self.inner.sentinel_dir.display(),
            "operator consent required — drop a sentinel via `roomler-agent consent --session {} --approve|--deny`",
            session_hex
        );

        let approve = self.sentinel_path(session_hex, SentinelKind::Approve);
        let deny = self.sentinel_path(session_hex, SentinelKind::Deny);
        let deadline = Instant::now() + timeout;
        let outcome = loop {
            if approve.exists() {
                break Decision::Granted;
            }
            if deny.exists() {
                break Decision::Denied;
            }
            if Instant::now() >= deadline {
                break Decision::Timeout;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        };

        // Clean up so a future re-request of the same session id
        // doesn't see a stale decision.
        let _ = std::fs::remove_file(&approve);
        let _ = std::fs::remove_file(&deny);
        // Also clear the `.pending` request marker the tray watches, so a
        // resolved prompt disappears from the tray immediately (best-effort;
        // absent when the request came without one — e.g. the CLI path).
        let _ = std::fs::remove_file(self.pending_path(session_hex));
        {
            let mut pending = self.inner.pending.lock().await;
            pending.remove(session_hex);
        }
        tracing::info!(session = session_hex, ?outcome, "operator consent decision");
        outcome
    }

    /// Drop a sentinel file for the given session. Used by the CLI
    /// subcommand. Returns the path written so the caller can surface
    /// it to the operator.
    pub fn write_sentinel(&self, session_hex: &str, kind: SentinelKind) -> Result<PathBuf> {
        let path = self.sentinel_path(session_hex, kind);
        // Body: unix timestamp (string). Just so the file is never
        // empty, easier to grep / debug. The polling loop only checks
        // existence, not content.
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        std::fs::write(&path, format!("{now_ts}\n"))
            .with_context(|| format!("writing sentinel {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms)?;
        }
        Ok(path)
    }

    /// Path of the `.pending` request marker for `session_hex`. Distinct from the
    /// `.approve`/`.deny` decision sentinels: this one is written by the AGENT
    /// (not the operator) when an attended session is awaiting a decision, so the
    /// tray can discover it and pop a prompt. Removed when the decision resolves.
    pub fn pending_path(&self, session_hex: &str) -> PathBuf {
        self.inner
            .sentinel_dir
            .join(format!("{session_hex}.pending"))
    }

    /// Write the `.pending` request marker (`body` = JSON describing the request:
    /// controller, permissions, timeout) for `session_hex`. Called by the
    /// signaling layer, which holds that metadata, right before an attended
    /// prompt begins. The tray watches for these files; the poll loop removes
    /// this one when the decision resolves. Best-effort — a write failure just
    /// means the tray falls back to its CLI-less state; the CLI path still works.
    pub fn write_pending(&self, session_hex: &str, body: &str) -> Result<PathBuf> {
        let path = self.pending_path(session_hex);
        std::fs::write(&path, body)
            .with_context(|| format!("writing pending marker {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms)?;
        }
        Ok(path)
    }
}

/// Sentinel-file flavour. The two-file layout (separate `.approve` /
/// `.deny`) means an operator-typo'd command won't accidentally flip
/// a previous decision: each is its own touch-create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentinelKind {
    Approve,
    Deny,
}

impl SentinelKind {
    fn suffix(self) -> &'static str {
        match self {
            SentinelKind::Approve => "approve",
            SentinelKind::Deny => "deny",
        }
    }

    pub fn from_flags(approve: bool, deny: bool) -> Result<Self> {
        match (approve, deny) {
            (true, false) => Ok(SentinelKind::Approve),
            (false, true) => Ok(SentinelKind::Deny),
            (true, true) => Err(anyhow::anyhow!(
                "pass exactly one of --approve / --deny, not both"
            )),
            (false, false) => Err(anyhow::anyhow!("pass exactly one of --approve / --deny")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("roomler-consent-{name}-{nanos}"))
    }

    #[test]
    fn mode_from_config_default_is_auto_grant() {
        // Default agent config has auto_grant_session = true; this
        // contract is what keeps the auto-grant unchanged for
        // self-hosted users on the upgrade.
        assert_eq!(Mode::from_config(true), Mode::AutoGrant);
    }

    #[test]
    fn mode_from_config_disabled_uses_prompt_timeout() {
        match Mode::from_config(false) {
            Mode::Prompt { timeout } => assert_eq!(timeout, DEFAULT_PROMPT_TIMEOUT),
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn decision_granted_maps_to_true() {
        assert!(Decision::Granted.granted());
        assert!(!Decision::Denied.granted());
        assert!(!Decision::Timeout.granted());
    }

    #[test]
    fn sentinel_kind_from_flags_validates_exclusivity() {
        assert_eq!(
            SentinelKind::from_flags(true, false).unwrap(),
            SentinelKind::Approve
        );
        assert_eq!(
            SentinelKind::from_flags(false, true).unwrap(),
            SentinelKind::Deny
        );
        assert!(SentinelKind::from_flags(true, true).is_err());
        assert!(SentinelKind::from_flags(false, false).is_err());
    }

    #[tokio::test]
    async fn auto_grant_resolves_immediately() {
        let dir = fixture_dir("auto");
        let broker = ConsentBroker::new(Mode::AutoGrant, dir.clone()).unwrap();
        let start = Instant::now();
        let decision = broker.request("abc123").await;
        assert_eq!(decision, Decision::Granted);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "auto-grant must not block on any I/O"
        );
        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn request_with_mode_overrides_startup_mode() {
        // Server-authoritative consent (Phase 2): the per-session directive from
        // `ServerMsg::Request { consent_mode }` wins over the broker's startup
        // (`auto_grant_session`) mode in BOTH directions.
        let dir = fixture_dir("directive");

        // Startup = AutoGrant, but the server directs Prompt → we take the prompt
        // path and (no sentinel) time out, proving auto-grant was NOT used.
        let auto_broker = ConsentBroker::new(Mode::AutoGrant, dir.clone()).unwrap();
        let d = auto_broker
            .request_with_mode(
                "aaaaaaaaaaaaaaaaaaaaaaaa",
                Mode::Prompt {
                    timeout: Duration::from_millis(80),
                },
            )
            .await;
        assert_eq!(
            d,
            Decision::Timeout,
            "Prompt directive must override startup AutoGrant"
        );

        // Startup = Prompt, but the server directs Auto → immediate grant, no
        // sentinel wait.
        let prompt_broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_secs(30),
            },
            dir.clone(),
        )
        .unwrap();
        let start = Instant::now();
        let d = prompt_broker
            .request_with_mode("bbbbbbbbbbbbbbbbbbbbbbbb", Mode::AutoGrant)
            .await;
        assert_eq!(
            d,
            Decision::Granted,
            "Auto directive must override startup Prompt"
        );
        assert!(start.elapsed() < Duration::from_millis(200));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pending_marker_written_and_cleared_on_resolve() {
        // Phase 3 — the signaling layer drops a `.pending` marker so the tray
        // can prompt; the broker's poll loop clears it once the decision lands.
        let dir = fixture_dir("pending");
        let broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_secs(5),
            },
            dir.clone(),
        )
        .unwrap();
        let session = "cccccccccccccccccccccccc";

        let pending = broker
            .write_pending(session, r#"{"session_id":"c","controller_name":"x"}"#)
            .unwrap();
        assert!(pending.exists(), ".pending must exist while awaiting");
        assert_eq!(broker.pending_path(session), pending);

        // Operator approves → run the prompt to resolution, which clears .pending.
        broker
            .write_sentinel(session, SentinelKind::Approve)
            .unwrap();
        let decision = broker
            .request_with_mode(
                session,
                Mode::Prompt {
                    timeout: Duration::from_secs(5),
                },
            )
            .await;
        assert_eq!(decision, Decision::Granted);
        assert!(!pending.exists(), ".pending must be cleared once resolved");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prompt_resolves_when_approve_sentinel_appears() {
        let dir = fixture_dir("approve");
        let broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_secs(5),
            },
            dir.clone(),
        )
        .unwrap();
        let session_hex = "deadbeef".to_string();
        // Schedule a sentinel write 100ms in the future. The
        // 250ms poll interval should pick it up on the second poll.
        let broker_for_writer = broker.clone();
        let session_for_writer = session_hex.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            broker_for_writer
                .write_sentinel(&session_for_writer, SentinelKind::Approve)
                .unwrap();
        });
        let decision = broker.request(&session_hex).await;
        writer.await.unwrap();
        assert_eq!(decision, Decision::Granted);
        // Sentinel was cleaned up.
        assert!(
            !broker
                .sentinel_path(&session_hex, SentinelKind::Approve)
                .exists()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prompt_resolves_when_deny_sentinel_appears() {
        let dir = fixture_dir("deny");
        let broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_secs(5),
            },
            dir.clone(),
        )
        .unwrap();
        let session_hex = "feed".to_string();
        let broker_for_writer = broker.clone();
        let session_for_writer = session_hex.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            broker_for_writer
                .write_sentinel(&session_for_writer, SentinelKind::Deny)
                .unwrap();
        });
        let decision = broker.request(&session_hex).await;
        assert_eq!(decision, Decision::Denied);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn prompt_times_out_when_no_sentinel_arrives() {
        let dir = fixture_dir("timeout");
        let broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_millis(400),
            },
            dir.clone(),
        )
        .unwrap();
        let start = Instant::now();
        let decision = broker.request("nopromptever").await;
        assert_eq!(decision, Decision::Timeout);
        assert!(
            start.elapsed() >= Duration::from_millis(400),
            "must respect timeout duration"
        );
        assert!(
            start.elapsed() < Duration::from_millis(900),
            "must not run far past timeout (got {:?})",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn request_with_empty_session_id_denies_safely() {
        // Defensive: a malformed signaling message that arrives with
        // an empty session_hex must NOT cause the broker to scan the
        // entire sentinel dir for a stale `.approve`. Default-deny.
        let dir = fixture_dir("empty");
        let broker = ConsentBroker::new(
            Mode::Prompt {
                timeout: Duration::from_secs(60),
            },
            dir.clone(),
        )
        .unwrap();
        let start = Instant::now();
        assert_eq!(broker.request("").await, Decision::Denied);
        assert!(start.elapsed() < Duration::from_millis(50));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

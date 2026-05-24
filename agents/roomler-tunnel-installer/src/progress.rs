//! Progress events streamed from `cmd_install` to the SPA.
//!
//! Mirrors the agent installer's `progress` module shape, with the
//! event vocabulary adapted to the tunnel install pipeline:
//!
//! - **No MSI events** (`MsiSpawned`, `MsiCompleted`, `UserCancel`).
//!   Tunnel install is archive-extract → enroll, no msiexec.
//! - **`ExtractStarted` / `ExtractDone`** for the new archive-handling
//!   phase.
//! - **`IntegrationStarted` / `IntegrationDone`** for the per-platform
//!   PATH / Start-Menu / .desktop work.
//! - **Same enrollment + done events** as the agent.
//!
//! The replay log mirrors the same backpressure pattern: every emit
//! lands in a process-wide `Mutex<Vec<ProgressEvent>>` so a late-
//! attaching SPA listener (one that wires its `ipc::Channel` listener
//! after the first event already fired) catches up via
//! [`replay_log`].

use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

/// One step of the install pipeline. Serialised to JSON via Tauri's
/// `ipc::Channel<ProgressEvent>` and consumed by the SPA's event
/// listener. The `#[serde(tag = "type", content = "data")]` wire form
/// produces shapes like `{"type":"Started","data":null}` /
/// `{"type":"DownloadProgress","data":{"receivedBytes":12345}}`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "data", rename_all = "PascalCase")]
pub enum ProgressEvent {
    /// Pipeline kickoff. SPA renders the "in progress" spinner.
    Started,
    /// Detect-existing-install scan begun.
    PreflightStarted,
    /// Detect scan finished. `existing` is a human-readable label such
    /// as `"clean"` / `"installed (0.3.0-rc.46) at ..."`.
    #[serde(rename_all = "camelCase")]
    PreflightOk { existing: String },
    /// Non-blocking warning surfaced by preflight (e.g. operator is
    /// reinstalling on top of an existing install — config will be
    /// overwritten).
    PreflightWarning { message: String },
    /// `/api/tunnel-wizard/<platform>/health` GET is in flight.
    #[serde(rename_all = "camelCase")]
    AssetResolving { platform: String },
    /// Manifest received; download is about to start.
    #[serde(rename_all = "camelCase")]
    AssetResolved {
        tag: String,
        size_bytes: u64,
        digest: Option<String>,
    },
    /// Download GET kicked off.
    #[serde(rename_all = "camelCase")]
    DownloadStarted { total_bytes: u64 },
    /// Per-chunk emit during download. Not mirrored into the replay
    /// log (fires many times per MB) — the SPA's live listener
    /// catches the stream. Only the terminal `DownloadVerified` event
    /// is replayed.
    #[serde(rename_all = "camelCase")]
    DownloadProgress { received_bytes: u64 },
    /// Download completed + SHA256 verified (or skipped on a pre-
    /// digest-field release with `sha256_match=true`).
    #[serde(rename_all = "camelCase")]
    DownloadVerified { sha256_match: bool },
    /// Archive extraction phase started.
    #[serde(rename_all = "camelCase")]
    ExtractStarted { archive: String },
    /// Extraction complete; `tunnel_binary` is the absolute path to
    /// the located `roomler-tunnel` executable.
    #[serde(rename_all = "camelCase")]
    ExtractDone { tunnel_binary: String },
    /// Per-platform integration (PATH / Start Menu / .desktop) begun.
    IntegrationStarted,
    /// Integration finished. `path_updated` / `shortcut_created` flags
    /// let the SPA render an honest "what we did" summary.
    #[serde(rename_all = "camelCase")]
    IntegrationDone {
        path_updated: bool,
        shortcut_created: bool,
    },
    /// `POST /api/tunnel-client/enroll` request kicked off.
    EnrollStarted,
    /// Enrollment exchange complete. `tunnel_client_id` is the hex
    /// ObjectId the server recorded.
    #[serde(rename_all = "camelCase")]
    EnrollOk {
        tunnel_client_id: String,
        tenant_id: String,
    },
    /// Pipeline succeeded end-to-end.
    Done,
    /// Terminal failure. `step` names the phase that failed
    /// (`"preflight"` / `"resolve"` / `"download"` / `"extract"` /
    /// `"integration"` / `"enroll"`); `message` carries the operator-
    /// visible error.
    Error { step: String, message: String },
}

/// Process-wide replay log. Push every emit so a late-attaching SPA
/// listener catches up; `reset` clears the log at the start of each
/// `run_install` call so a previous run's events don't leak in.
pub struct ReplayLog {
    inner: Mutex<Vec<ProgressEvent>>,
}

impl Default for ReplayLog {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayLog {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
        }
    }

    pub fn push(&self, event: ProgressEvent) {
        // Skip per-chunk download progress to keep the replay log
        // small. The SPA's live listener catches the stream; replay
        // is only for SPA listeners that attached AFTER the install
        // pipeline started (rare race).
        if matches!(event, ProgressEvent::DownloadProgress { .. }) {
            return;
        }
        if let Ok(mut log) = self.inner.lock() {
            log.push(event);
        }
    }

    pub fn snapshot(&self) -> Vec<ProgressEvent> {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn reset(&self) {
        if let Ok(mut log) = self.inner.lock() {
            log.clear();
        }
    }
}

/// Singleton accessor for the process-wide replay log.
pub fn replay_log() -> &'static ReplayLog {
    static LOG: OnceLock<ReplayLog> = OnceLock::new();
    LOG.get_or_init(ReplayLog::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_serialises_with_tag() {
        let event = ProgressEvent::Started;
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"Started""#));
    }

    #[test]
    fn download_progress_uses_camel_case_payload() {
        let event = ProgressEvent::DownloadProgress {
            received_bytes: 1024,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""receivedBytes":1024"#), "got {json}");
    }

    #[test]
    fn enroll_ok_carries_tunnel_client_id_camel_case() {
        let event = ProgressEvent::EnrollOk {
            tunnel_client_id: "507f1f77bcf86cd799439011".to_string(),
            tenant_id: "507f191e810c19729de860ea".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""tunnelClientId":"507f1f77bcf86cd799439011""#));
        assert!(json.contains(r#""tenantId":"507f191e810c19729de860ea""#));
    }

    #[test]
    fn replay_log_skips_download_progress() {
        let log = ReplayLog::new();
        log.push(ProgressEvent::Started);
        log.push(ProgressEvent::DownloadProgress { received_bytes: 1 });
        log.push(ProgressEvent::DownloadProgress { received_bytes: 2 });
        log.push(ProgressEvent::Done);
        let snap = log.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(matches!(snap[0], ProgressEvent::Started));
        assert!(matches!(snap[1], ProgressEvent::Done));
    }

    #[test]
    fn replay_log_reset_clears() {
        let log = ReplayLog::new();
        log.push(ProgressEvent::Started);
        log.reset();
        assert!(log.snapshot().is_empty());
    }
}

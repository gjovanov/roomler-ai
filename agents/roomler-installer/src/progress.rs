//! Install progress streaming.
//!
//! W5 in the rc.28 plan + HIGH-1 fix from the critique. The wizard's
//! `cmd_install` orchestrator emits one [`ProgressEvent`] per step
//! transition so the SPA renders a live checklist:
//!
//!   Resolving installer…  ✓
//!   Downloading 17 MB     ███████░░ 64 %
//!   Running MSI installer …  (UAC pending)
//!   Configuring service environment …
//!   Restarting service …
//!   Enrolling with Roomler …
//!   Done.
//!
//! ## Why both a Channel AND a replay log
//!
//! Tauri 2's `ipc::Channel<T>` is the primary delivery path —
//! guaranteed ordering, no drops between Rust → JS once the front-
//! end attaches the listener. BUT events emitted by `cmd_install`
//! BEFORE the SPA's listener finishes attaching are not delivered.
//! In practice the SPA installs the listener synchronously before
//! invoking `cmd_install`, so the race is small, but it's not
//! zero: the listener attach awaits a `__TAURI_INVOKE_*` round-trip
//! that can lose to the first few microseconds of the install task.
//!
//! [`ProgressLog`] mirrors every emit into an in-Rust `Vec<...>`. The
//! SPA can call `cmd_install_progress_replay()` on first listener
//! attach to fast-forward through any events it missed. Cost is
//! tiny — `ProgressEvent` is ~100 bytes per variant; a typical
//! install emits 10-15 events end-to-end.

use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// One step transition in the install pipeline. Tagged on `kind` so
/// the SPA matches with a string `switch`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProgressEvent {
    /// `cmd_install` started executing. Sent before any disk/network
    /// IO so the SPA can flip to the Install step UI.
    Started,

    /// Pre-flight registry probe began.
    PreflightStarted,
    /// Pre-flight finished. `existing` is the detection result
    /// summary so the SPA can show "Detected: perUser 0.3.0-rc.26".
    PreflightOk { existing: String },
    /// Non-fatal warning surfaced during pre-flight (cross-flavour
    /// switch, ambiguous install, etc.). SPA renders inline; install
    /// still proceeds.
    PreflightWarning { message: String },

    /// Resolving the MSI URL via `roomler.ai/api/agent/installer/
    /// {flavour}/health`. SPA shows "Resolving installer…".
    AssetResolving { flavour: String },
    /// Asset resolved. `tag` is the GitHub tag the wizard will pin
    /// to; `size_bytes` and `digest` come from the rc.27 health
    /// endpoint. SPA uses `size_bytes` to render the download
    /// progress bar's denominator.
    AssetResolved {
        tag: String,
        size_bytes: u64,
        digest: Option<String>,
    },

    /// Download stream opened. `total_bytes` mirrors `size_bytes`
    /// from `AssetResolved` for the SPA convenience.
    DownloadStarted { total_bytes: u64 },
    /// One progress tick during download. SPA throttles UI updates
    /// to ~10 Hz; the wizard's `asset_resolver` emits roughly per
    /// 64 KiB chunk so a 17 MB MSI produces ~270 ticks.
    DownloadProgress { received_bytes: u64 },
    /// Download complete; SHA256 verified (or `digest=None` and we
    /// skipped verification).
    DownloadVerified { sha256_match: bool },

    /// msiexec launched. `pid` is the OS PID for operator
    /// diagnostics. SPA relabels the Cancel button to "Force-kill
    /// installer" once this fires (H4 of the plan critique).
    MsiSpawned { pid: u32 },
    /// msiexec exited. `code` is the raw OS exit code; `decoded`
    /// is the human-friendly enum from `msi_runner::decode_msi_exit`.
    MsiCompleted { code: i32, decoded: String },

    /// SystemContext path only: writing
    /// `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP=1` to the SCM service
    /// Environment REG_MULTI_SZ.
    EnvVarWriting,
    /// Env var write done.
    EnvVarSet,

    /// Restarting the SCM service so the new env block takes
    /// effect.
    ServiceRestarting,
    /// Service back to RUNNING.
    ServiceRestarted,

    /// Calling `enrollment::enroll` against the configured server.
    EnrollStarted,
    /// Enrollment succeeded; `agent_id` + `tenant_id` are surfaced
    /// on the Done step's confirmation panel.
    EnrollOk { agent_id: String, tenant_id: String },

    /// Whole install finished. SPA transitions to the Done step.
    Done,

    /// Fatal error at some step. SPA surfaces the recovery panel
    /// scoped to `step` (`download`, `msi`, `service`, `enroll`).
    Error { step: String, message: String },
}

/// In-Rust replay log. `cmd_install` mirrors every emit here so the
/// SPA can catch up if its listener attached late.
#[derive(Default)]
pub struct ProgressLog {
    inner: Mutex<Vec<ProgressEvent>>,
}

impl ProgressLog {
    /// Append one event. Lock-poisoning is logged but otherwise
    /// ignored — a poisoned mutex shouldn't crash the install just
    /// because a panicking task previously held it.
    pub fn push(&self, event: ProgressEvent) {
        match self.inner.lock() {
            Ok(mut g) => g.push(event),
            Err(p) => {
                tracing::warn!("progress log mutex poisoned; recovering");
                p.into_inner().push(event);
            }
        }
    }

    /// Snapshot of all events emitted so far. The SPA's
    /// `cmd_install_progress_replay()` returns this verbatim.
    pub fn snapshot(&self) -> Vec<ProgressEvent> {
        match self.inner.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// Clear the log at the start of a fresh install.
    pub fn reset(&self) {
        match self.inner.lock() {
            Ok(mut g) => g.clear(),
            Err(p) => p.into_inner().clear(),
        }
    }
}

/// Process-wide replay log. Lazily initialised; cmd_install resets
/// + writes here, cmd_install_progress_replay reads.
pub fn replay_log() -> &'static ProgressLog {
    static LOG: OnceLock<ProgressLog> = OnceLock::new();
    LOG.get_or_init(ProgressLog::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_snapshot_returns_in_order() {
        let log = ProgressLog::default();
        log.push(ProgressEvent::Started);
        log.push(ProgressEvent::PreflightStarted);
        log.push(ProgressEvent::PreflightOk {
            existing: "clean".to_string(),
        });
        let snap = log.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0], ProgressEvent::Started);
        assert_eq!(snap[1], ProgressEvent::PreflightStarted);
        assert_eq!(
            snap[2],
            ProgressEvent::PreflightOk {
                existing: "clean".to_string()
            }
        );
    }

    #[test]
    fn reset_clears_log() {
        let log = ProgressLog::default();
        log.push(ProgressEvent::Started);
        assert_eq!(log.snapshot().len(), 1);
        log.reset();
        assert_eq!(log.snapshot().len(), 0);
    }

    #[test]
    fn snapshot_is_a_copy_not_a_lock() {
        // Concurrent pushes after a snapshot don't mutate the
        // returned Vec — required because the SPA renders the
        // snapshot at its own pace.
        let log = ProgressLog::default();
        log.push(ProgressEvent::Started);
        let snap = log.snapshot();
        log.push(ProgressEvent::Done);
        assert_eq!(snap.len(), 1);
        assert_eq!(log.snapshot().len(), 2);
    }

    #[test]
    fn serializes_to_kind_tagged_json() {
        // The SPA matches events via `event.kind === "asset_resolved"`
        // — locking the wire shape here protects the JS handler from
        // a Rust-side rename.
        let event = ProgressEvent::AssetResolved {
            tag: "agent-v0.3.0-rc.28".to_string(),
            size_bytes: 17_000_000,
            digest: Some("sha256:deadbeef".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""kind":"asset_resolved""#));
        assert!(json.contains(r#""tag":"agent-v0.3.0-rc.28""#));
        assert!(json.contains(r#""size_bytes":17000000"#));
        assert!(json.contains(r#""digest":"sha256:deadbeef""#));
    }

    #[test]
    fn replay_log_singleton() {
        // Same handle across calls; the cmd_install + replay command
        // both read/write the same backing Vec.
        let a = replay_log();
        let b = replay_log();
        assert!(std::ptr::eq(a, b));
    }
}

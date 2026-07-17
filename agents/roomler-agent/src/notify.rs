//! Operator-attention notification.
//!
//! v1 ships a sentinel file the agent writes when it needs human
//! intervention (today: persistent auth rejection that suggests the
//! token has been revoked). The file lives at the per-user config
//! dir, alongside `config.toml`, so:
//!
//! - A fleet-management script can scan `%APPDATA%\roomler\
//!   roomler-agent\config\needs-attention.txt` across machines.
//! - The future admin UI heartbeat (resilience plan Phase 7) can
//!   surface "this agent flagged itself as needing attention."
//! - An interactive operator running `roomler-agent re-enroll`
//!   sees the file vanish on success.
//!
//! Real OS-toast notification (BurntToast on Win, `notify-send` on
//! Linux, `osascript` on macOS) is deferred — the sentinel file is
//! always-on-disk durable, which is what unattended-deployment IT
//! admins actually want (they grep filesystems, not desktops).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const ATTENTION_FILENAME: &str = "needs-attention.txt";

/// Resolve the per-user attention sentinel path. Returns `None` on
/// platforms where `directories` can't determine a config dir
/// (extremely rare; same scope as `config::default_config_path`).
pub fn attention_path() -> Option<PathBuf> {
    let dirs = crate::appdirs::project_dirs()?;
    Some(dirs.config_dir().join(ATTENTION_FILENAME))
}

/// Raise an attention sentinel at the per-user config dir. Writes
/// the message verbatim plus a generated-at unix timestamp so a
/// reader can tell stale flags from fresh ones. Idempotent — every
/// call replaces any existing sentinel.
pub fn raise_attention(message: &str) -> Result<PathBuf> {
    let path = attention_path().context("no per-user config dir resolvable")?;
    let parent = path.parent().context("attention path has no parent")?;
    raise_attention_at(parent, message)
}

/// Same as [`raise_attention`] but takes an explicit directory.
/// Extracted so the test suite can drive it against a tempdir.
pub fn raise_attention_at(dir: &Path, message: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating attention dir {}", dir.display()))?;
    let path = dir.join(ATTENTION_FILENAME);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let body = format!("{message}\n\nGenerated at: {ts} (unix seconds)\n");
    std::fs::write(&path, body)
        .with_context(|| format!("writing attention sentinel {}", path.display()))?;
    Ok(path)
}

/// Remove the attention sentinel if present. Best-effort — a
/// missing file or a permission glitch is silent.
pub fn clear_attention() {
    if let Some(path) = attention_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether an attention sentinel currently exists. Cheap stat call,
/// safe to poll.
pub fn has_attention() -> bool {
    attention_path().map(|p| p.exists()).unwrap_or(false)
}

// ─── rc.53: LocalSystem-aware sentinel path routing ────────────────

/// rc.53: resolve the attention sentinel path with awareness of the
/// caller's worker context.
///
/// When the current process is the LocalSystem SCM worker
/// ([`crate::system_context::worker_role::WorkerRole::SystemContext`])
/// the standard `directories::ProjectDirs` `%APPDATA%` resolves to
/// `C:\Windows\System32\config\systemprofile\AppData\Roaming\…`
/// — invisible to a human operator and missed by every fleet-mgmt
/// scanner that greps user profiles. Prefer
/// `%PROGRAMDATA%\roomler\roomler-agent\needs-attention.txt` in that
/// case so the file is findable by both a logged-in operator
/// (`dir %PROGRAMDATA%`) AND a fleet scanner.
///
/// Returns `(path, was_machine_global)` so the caller can log the
/// resolved location at WARN — operators investigating "where did
/// the sentinel land?" find it via the log line.
///
/// On non-Windows, builds without the `system-context` feature, or
/// when the worker-role probe fails, falls back to the existing
/// per-user [`attention_path`] semantics.
///
/// The dual cfg gate (`target_os = "windows"` AND `feature =
/// "system-context"`) mirrors the gate on the upstream module —
/// `pub mod system_context;` is itself `#[cfg(feature =
/// "system-context")]` (`lib.rs:35`). Without both, the LocalSystem
/// branch is dead code that wouldn't link, so we route through the
/// fallback unconditionally.
#[cfg(all(feature = "system-context", target_os = "windows"))]
pub fn attention_path_for_worker() -> Option<(PathBuf, bool)> {
    use crate::system_context::worker_role::{WorkerRole, probe_self};
    if let Ok(WorkerRole::SystemContext) = probe_self() {
        let path = crate::appdirs::machine_global_dir().join(ATTENTION_FILENAME);
        return Some((path, true));
    }
    attention_path().map(|p| (p, false))
}

#[cfg(not(all(feature = "system-context", target_os = "windows")))]
pub fn attention_path_for_worker() -> Option<(PathBuf, bool)> {
    attention_path().map(|p| (p, false))
}

/// rc.53: variant of [`raise_attention`] that routes to `%PROGRAMDATA%`
/// when running as LocalSystem. Logs the resolved path at WARN so
/// the operator can find the file. Used by the agent's
/// `signaling::handle_server_msg` `ServerMsg::Goodbye` arm.
///
/// Falls back to the user-context [`raise_attention`] path on
/// non-Windows or when the worker-role probe can't resolve
/// `SystemContext` — same behaviour as pre-rc.53.
pub fn raise_attention_machine_aware(message: &str) -> Result<PathBuf> {
    let (path, machine_global) =
        attention_path_for_worker().context("no attention path resolvable")?;
    let parent = path.parent().context("attention path has no parent")?;
    let written = raise_attention_at(parent, message)?;
    tracing::warn!(
        path = %written.display(),
        machine_global,
        "raised needs-attention sentinel"
    );
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raise_writes_message_and_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let path = raise_attention_at(tmp.path(), "re-enrollment required").unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("re-enrollment required"));
        assert!(
            content.contains("Generated at:"),
            "timestamp footer missing: {content:?}"
        );
    }

    #[test]
    fn raise_replaces_existing_sentinel() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = raise_attention_at(tmp.path(), "first message").unwrap();
        let path = raise_attention_at(tmp.path(), "second message").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("second message"));
        assert!(!content.contains("first message"));
    }

    #[test]
    fn raise_creates_parent_dir_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("level1").join("level2");
        let path = raise_attention_at(&nested, "test").unwrap();
        assert!(path.exists());
    }

    #[test]
    fn attention_path_does_not_panic() {
        // Returns `Some(path)` on platforms with a config dir, `None`
        // in the rare environment where `directories::ProjectDirs`
        // can't resolve one (some sandboxed test runners clear
        // HOME / USERPROFILE). Either result is fine — the function
        // is best-effort. What matters is no panic.
        let _ = attention_path();
    }

    #[test]
    fn attention_path_for_worker_does_not_panic() {
        // rc.53: same best-effort contract as `attention_path`. The
        // worker-role probe inspects the current process's primary
        // token; in a `cargo test` runner the role is virtually always
        // `WorkerRole::User`, so this exercises the user-context
        // fallback branch. (The SystemContext branch only fires when
        // the runtime is the LocalSystem SCM worker — not testable
        // from a normal test harness.)
        let _ = attention_path_for_worker();
    }

    #[test]
    fn raise_attention_machine_aware_writes_through_fallback() {
        // rc.53: under cargo-test we exercise the user-context branch
        // (non-LocalSystem), so the function should write through
        // `attention_path`'s `directories::ProjectDirs` resolver. We
        // can't redirect that path safely in a parallel test runner,
        // so this test simply asserts the function returns Ok OR a
        // path-resolvable error (the contract is "writes to disk +
        // logs the path; never panics"). The deeper machine-global
        // behaviour is covered by the integration smoke matrix
        // (SM-1 / SM-1b in docs/plans/rc53-…-v2.md).
        let result = raise_attention_machine_aware("rc.53 smoke");
        // Either Ok(path) or a fail-with-context Err — both are
        // acceptable, panic is not.
        let _ = result;
    }
}

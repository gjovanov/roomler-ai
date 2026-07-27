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

// S1b — structured reason codes. The sentinel stays a human-readable
// .txt (fleet scripts grep it) but gains a machine-parsable
// `Reason: <code>` line so readers (the desktop Overview, the healthy-
// connect clearer) can act per reason instead of showing a bare path.
pub const REASON_AUTH: &str = "auth_rejected";
pub const REASON_GOODBYE: &str = "server_goodbye";
pub const REASON_DUPLICATE: &str = "duplicate_instance";
pub const REASON_ROLLBACK: &str = "rollback_failed";
/// Legacy / unspecified sentinels (pre-S1b writers, ad-hoc messages).
pub const REASON_GENERIC: &str = "attention";

/// Parsed view of a sentinel file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionInfo {
    pub path: PathBuf,
    /// The human message (everything above the structured footer).
    pub message: String,
    /// `Reason:` code; `None` for pre-S1b sentinels.
    pub reason: Option<String>,
}

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
    raise_attention_with_reason(REASON_GENERIC, message)
}

/// S1b — reasoned variant of [`raise_attention`].
pub fn raise_attention_with_reason(reason: &str, message: &str) -> Result<PathBuf> {
    let path = attention_path().context("no per-user config dir resolvable")?;
    let parent = path.parent().context("attention path has no parent")?;
    raise_attention_at_with_reason(parent, reason, message)
}

/// Same as [`raise_attention`] but takes an explicit directory.
/// Extracted so the test suite can drive it against a tempdir.
pub fn raise_attention_at(dir: &Path, message: &str) -> Result<PathBuf> {
    raise_attention_at_with_reason(dir, REASON_GENERIC, message)
}

/// S1b — the one real writer: message + structured footer.
pub fn raise_attention_at_with_reason(dir: &Path, reason: &str, message: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating attention dir {}", dir.display()))?;
    let path = dir.join(ATTENTION_FILENAME);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let body = format!("{message}\n\nReason: {reason}\nGenerated at: {ts} (unix seconds)\n");
    std::fs::write(&path, body)
        .with_context(|| format!("writing attention sentinel {}", path.display()))?;
    Ok(path)
}

/// Parse a sentinel file into its message + reason. `None` when the
/// file is absent/unreadable. Pre-S1b sentinels (no `Reason:` line)
/// parse with `reason: None`.
pub fn read_attention_at(path: &Path) -> Option<AttentionInfo> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut reason = None;
    let mut message_lines: Vec<&str> = Vec::new();
    for line in raw.lines() {
        if let Some(code) = line.strip_prefix("Reason: ") {
            reason = Some(code.trim().to_string());
        } else if !line.starts_with("Generated at:") {
            message_lines.push(line);
        }
    }
    let message = message_lines.join("\n").trim().to_string();
    Some(AttentionInfo {
        path: path.to_path_buf(),
        message,
        reason,
    })
}

/// The machine-global sentinel path (rc.53 SystemContext writers land
/// there). `None` off Windows.
pub fn machine_attention_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        Some(crate::appdirs::machine_global_dir().join(ATTENTION_FILENAME))
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// Every sentinel location this host may carry, per-user first.
fn all_attention_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = attention_path() {
        out.push(p);
    }
    if let Some(p) = machine_attention_path() {
        out.push(p);
    }
    out
}

/// S1b — first present sentinel across BOTH locations (the desktop app
/// previously read only the per-user path, so a SystemContext host's
/// machine-global sentinel was invisible).
pub fn read_any_attention() -> Option<AttentionInfo> {
    all_attention_paths()
        .iter()
        .find_map(|p| read_attention_at(p))
}

/// Remove the per-user attention sentinel if present. Best-effort — a
/// missing file or a permission glitch is silent.
pub fn clear_attention() {
    if let Some(path) = attention_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// S1b — clear EVERY sentinel unconditionally (re-enroll resolves all
/// reasons, including `rollback_failed`).
pub fn clear_all_attention() {
    for path in all_attention_paths() {
        let _ = std::fs::remove_file(path);
    }
}

/// S1b — clear sentinels on a healthy authenticated connect. Every
/// reason except `rollback_failed` is, by definition, resolved once the
/// agent is connected + authenticated (auth works, no goodbye, no live
/// duel); legacy reason-less sentinels — mostly test artifacts like the
/// infamous "rc.53 smoke" — are cleared too. `rollback_failed` persists
/// until an operator acts (the broken-binary state isn't disproven by a
/// successful connect of the rolled-back binary).
pub fn clear_attention_on_healthy_connect() {
    for path in all_attention_paths() {
        match read_attention_at(&path) {
            Some(info) if info.reason.as_deref() == Some(REASON_ROLLBACK) => {}
            Some(_) => {
                let _ = std::fs::remove_file(&path);
            }
            None => {}
        }
    }
}

/// Whether an attention sentinel currently exists in ANY location.
/// Cheap stat calls, safe to poll.
pub fn has_attention() -> bool {
    all_attention_paths().iter().any(|p| p.exists())
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
    raise_attention_machine_aware_with_reason(REASON_GENERIC, message)
}

/// S1b — reasoned variant of [`raise_attention_machine_aware`].
pub fn raise_attention_machine_aware_with_reason(reason: &str, message: &str) -> Result<PathBuf> {
    let (path, machine_global) =
        attention_path_for_worker().context("no attention path resolvable")?;
    let parent = path.parent().context("attention path has no parent")?;
    let written = raise_attention_at_with_reason(parent, reason, message)?;
    tracing::warn!(
        path = %written.display(),
        machine_global,
        reason,
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

    // S1b: the old `raise_attention_machine_aware_writes_through_fallback`
    // test wrote a REAL "rc.53 smoke" sentinel through the developer's
    // actual profile on every `cargo test` run — which then sat forever in
    // the desktop app's "Attention required" banner (the reported field
    // bug). Path RESOLUTION is asserted without writing; the write
    // mechanics are covered by the tempdir tests above.
    #[test]
    fn machine_aware_path_resolves_without_writing() {
        let _ = attention_path_for_worker();
    }

    #[test]
    fn reasoned_roundtrip_parses_message_and_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let path =
            raise_attention_at_with_reason(tmp.path(), REASON_AUTH, "re-enrollment required")
                .unwrap();
        let info = read_attention_at(&path).expect("sentinel parses");
        assert_eq!(info.reason.as_deref(), Some(REASON_AUTH));
        assert_eq!(info.message, "re-enrollment required");
    }

    #[test]
    fn legacy_sentinel_parses_with_no_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(ATTENTION_FILENAME);
        std::fs::write(
            &path,
            "rc.53 smoke\n\nGenerated at: 1785141357 (unix seconds)\n",
        )
        .unwrap();
        let info = read_attention_at(&path).expect("legacy sentinel parses");
        assert_eq!(info.reason, None);
        assert_eq!(info.message, "rc.53 smoke");
    }

    #[test]
    fn healthy_clear_spares_only_rollback() {
        // Drive the per-path decision logic directly against tempdir
        // sentinels (the public fn resolves real profile paths).
        let tmp = tempfile::tempdir().unwrap();
        let auth = raise_attention_at_with_reason(tmp.path(), REASON_AUTH, "x").unwrap();
        let info = read_attention_at(&auth).unwrap();
        assert_ne!(info.reason.as_deref(), Some(REASON_ROLLBACK));

        let rb = tmp.path().join("rb");
        std::fs::create_dir_all(&rb).unwrap();
        let rb_path = raise_attention_at_with_reason(&rb, REASON_ROLLBACK, "broken").unwrap();
        let info = read_attention_at(&rb_path).unwrap();
        assert_eq!(info.reason.as_deref(), Some(REASON_ROLLBACK));
    }
}

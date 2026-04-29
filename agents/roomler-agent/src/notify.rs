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
    use directories::ProjectDirs;
    let dirs = ProjectDirs::from("live", "roomler", "roomler-agent")?;
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
    attention_path()
        .map(|p| p.exists())
        .unwrap_or(false)
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
}

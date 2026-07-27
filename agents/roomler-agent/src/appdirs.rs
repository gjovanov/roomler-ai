//! Roomler node application directories with a legacy-segment fallback.
//!
//! The controlled-host daemon is being renamed `roomler-agent` -> `roomlerd`
//! (the unified device/node model — see the unification plan). Its per-user and
//! machine-global data trees historically live under an app segment
//! `roomler-agent` (`%APPDATA%\roomler\roomler-agent`,
//! `%PROGRAMDATA%\roomler\roomler-agent`, `~/.config/roomler-agent`, ...).
//!
//! Renaming that segment to `roomler` must **never** orphan a host's enrolled
//! `config.toml` (its bearer token) — that's the same class of silent
//! fleet-drop-off as the MajorUpgrade-drops-env-vars bug. So resolution reads
//! BOTH: it uses the NEW `roomler` segment when its tree already exists, else
//! keeps using the OLD `roomler-agent` tree if THAT exists, and only a genuinely
//! fresh install lands on the new segment. The decision is made once per process
//! (cached) and applied to every directory, so config / logs / crashes on a host
//! never split across two trees.
//!
//! S1b adds the follow-up: [`migrate_legacy_trees`] performs a one-shot
//! startup RENAME of the legacy trees onto the `roomler` segment (per-user
//! config/data roots + the Windows machine-global root). It runs before any
//! consumer caches a segment decision, never deletes, and on any failure
//! simply leaves the read-both resolution in charge (retry next start) — so
//! enrollment still cannot be lost.

use directories::ProjectDirs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Reverse-domain qualifier for the agent's per-user dirs (macOS uses it;
/// Windows/Linux ignore it). Historically "live" — preserved so existing
/// macOS dirs aren't orphaned.
const QUALIFIER: &str = "live";
const ORG: &str = "roomler";
/// New app segment (post-rename, fresh installs).
const NEW_APP: &str = "roomler";
/// Legacy app segment (pre-rename installs already in the field).
const OLD_APP: &str = "roomler-agent";

/// True if a NEW-segment `ProjectDirs` tree is present on disk.
fn tree_exists(app: &str) -> bool {
    ProjectDirs::from(QUALIFIER, ORG, app)
        .is_some_and(|d| d.config_dir().exists() || d.data_local_dir().exists())
}

/// Whether to use the OLD segment for the per-user tree. Cached: the filesystem
/// answer is stable within a process, and caching guarantees every consumer in
/// one run agrees (no split trees). NEW-if-present wins; else OLD-if-present;
/// else NEW (fresh install).
fn use_old_segment() -> bool {
    static DECISION: OnceLock<bool> = OnceLock::new();
    *DECISION.get_or_init(|| !tree_exists(NEW_APP) && tree_exists(OLD_APP))
}

/// The resolved per-user app segment ("roomler" for fresh/migrated hosts,
/// "roomler-agent" for a pre-rename install whose tree still exists).
fn app_segment() -> &'static str {
    if use_old_segment() { OLD_APP } else { NEW_APP }
}

/// The agent's `ProjectDirs`, resolved to the NEW segment unless a pre-rename
/// install is detected (then the OLD segment, so its enrolled config is never
/// orphaned). `None` only if the platform exposes no config dir at all.
pub fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from(QUALIFIER, ORG, app_segment())
}

/// Machine-global data root `%PROGRAMDATA%\roomler\<segment>` (Windows only).
/// Same new-then-old resolution as [`project_dirs`], keyed independently on the
/// machine-global tree (a perMachine/SystemContext host's enrolled config lives
/// here and must not be orphaned). Consumers `.join(...)` their subdir
/// (`config.toml`, `service-logs`, `crashes`, `staging`, ...).
#[cfg(target_os = "windows")]
pub fn machine_global_dir() -> PathBuf {
    static DECISION: OnceLock<PathBuf> = OnceLock::new();
    DECISION
        .get_or_init(|| {
            let base = std::env::var_os("PROGRAMDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
                .join(ORG);
            let new = base.join(NEW_APP);
            let old = base.join(OLD_APP);
            if !new.exists() && old.exists() {
                old // pre-rename install still present -> keep it
            } else {
                new // fresh install, or the new tree is already present
            }
        })
        .clone()
}

// ─── S1b: one-shot legacy-tree migration ───────────────────────────────────

/// Notes from the last [`migrate_legacy_trees`] run — logged by the caller
/// AFTER logging::init (migration must run before it, so it can't trace
/// directly). Empty = nothing noteworthy happened.
static MIGRATION_NOTES: OnceLock<Vec<String>> = OnceLock::new();

pub fn migration_notes() -> &'static [String] {
    MIGRATION_NOTES.get().map(|v| v.as_slice()).unwrap_or(&[])
}

/// One-shot startup migration of the legacy `roomler-agent` trees onto the
/// `roomler` segment. MUST run before `logging::init` and before ANY other
/// appdirs consumer (the segment decision is `OnceLock`-cached per process,
/// and log files open inside the tree).
///
/// Rules, per directory (per-user config/data roots + the Windows
/// machine-global root, each independent):
///   * legacy exists, new doesn't → atomic same-volume `fs::rename`
///   * both exist (split-brain from the self-heal era) → left in place
///     (NEW-wins resolution already applies; config cleanup is the
///     `ConfigCleanupStale` verb's job)
///   * legacy absent → nothing to do
///
/// Never deletes; a failed rename (locked file, ACL) is noted and the
/// read-both resolution keeps everything working until the next start.
///
/// `companion_running` (the roomler-desktop tray): skip entirely — the tray
/// may hold open handles inside the tree, and a pre-S1a tray would keep
/// writing the old tree after the move, re-manufacturing the split.
pub fn migrate_legacy_trees(companion_running: bool) {
    let mut notes = Vec::new();
    if companion_running {
        notes.push(
            "migration skipped: desktop companion is running (will retry next start)".to_string(),
        );
    } else {
        if let (Some(old), Some(new)) = (
            ProjectDirs::from(QUALIFIER, ORG, OLD_APP),
            ProjectDirs::from(QUALIFIER, ORG, NEW_APP),
        ) {
            let mut pairs: Vec<(PathBuf, PathBuf)> = vec![
                (
                    old.config_dir().to_path_buf(),
                    new.config_dir().to_path_buf(),
                ),
                (old.data_dir().to_path_buf(), new.data_dir().to_path_buf()),
                (
                    old.data_local_dir().to_path_buf(),
                    new.data_local_dir().to_path_buf(),
                ),
            ];
            // data_dir == data_local_dir on Linux; config may coincide too on
            // some platforms. Adjacent duplicates only.
            pairs.dedup();
            for (o, n) in &pairs {
                if let Some(note) = migrate_dir(o, n) {
                    notes.push(note);
                }
            }
            // Best-effort: drop the now-empty legacy shells (e.g. Windows
            // `%APPDATA%\roomler\roomler-agent` after its `config`/`data`
            // children moved out). `remove_dir` refuses non-empty dirs, so
            // anything left behind survives.
            for parent in pairs
                .iter()
                .filter_map(|(o, _)| o.parent().map(Path::to_path_buf))
            {
                if parent.file_name().is_some_and(|f| f == OLD_APP) {
                    let _ = std::fs::remove_dir(&parent);
                }
            }
        }
        #[cfg(target_os = "windows")]
        {
            let base = std::env::var_os("PROGRAMDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
                .join(ORG);
            if let Some(note) = migrate_dir(&base.join(OLD_APP), &base.join(NEW_APP)) {
                notes.push(note);
            }
        }
    }
    let _ = MIGRATION_NOTES.set(notes);
}

/// Apply the migration rules to one directory pair. Returns a note for
/// anything noteworthy (moved / both-present / failed), `None` for the
/// quiet no-legacy case.
fn migrate_dir(old: &Path, new: &Path) -> Option<String> {
    if !old.exists() {
        return None;
    }
    if new.exists() {
        return Some(format!(
            "legacy tree left in place (new tree also present): {}",
            old.display()
        ));
    }
    if let Some(parent) = new.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Some(format!(
            "migration failed (create {}): {e}",
            parent.display()
        ));
    }
    match std::fs::rename(old, new) {
        Ok(()) => Some(format!(
            "migrated legacy tree {} -> {}",
            old.display(),
            new.display()
        )),
        Err(e) => Some(format!(
            "migration failed ({} -> {}): {e} — read-both fallback stays in charge",
            old.display(),
            new.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `tree_exists` / segment resolution touch the real HOME/APPDATA, so we
    // don't assert on live paths here (that would be environment-dependent).
    // Instead lock the pure new-then-old PRECEDENCE with an injected predicate,
    // mirroring the `node_env` fallback-order test in tunnel-core.
    fn pick<'a>(new: &'a str, old: &'a str, exists: impl Fn(&str) -> bool) -> &'a str {
        if exists(new) {
            new
        } else if exists(old) {
            old
        } else {
            new
        }
    }

    #[test]
    fn new_then_old_then_new_precedence() {
        // NEW present -> NEW (even if OLD also present).
        assert_eq!(pick("new", "old", |s| s == "new" || s == "old"), "new");
        // only OLD present -> OLD (upgraded host keeps its tree).
        assert_eq!(pick("new", "old", |s| s == "old"), "old");
        // neither present -> NEW (fresh install).
        assert_eq!(pick("new", "old", |_| false), "new");
    }

    #[test]
    fn migrate_dir_moves_legacy_into_place() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("roomler-agent").join("config");
        let new = tmp.path().join("roomler").join("config");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("config.toml"), b"agent_token = \"x\"").unwrap();

        let note = migrate_dir(&old, &new).expect("a move produces a note");
        assert!(note.starts_with("migrated"), "{note}");
        assert!(!old.exists(), "legacy dir must be gone after the move");
        assert_eq!(
            std::fs::read_to_string(new.join("config.toml")).unwrap(),
            "agent_token = \"x\"",
            "contents ride along byte-identically"
        );
    }

    #[test]
    fn migrate_dir_leaves_split_brain_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("roomler-agent");
        let new = tmp.path().join("roomler");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("stale.txt"), b"old").unwrap();

        let note = migrate_dir(&old, &new).expect("both-present produces a note");
        assert!(note.contains("left in place"), "{note}");
        assert!(old.join("stale.txt").exists(), "nothing may be deleted");
    }

    #[test]
    fn migrate_dir_is_quiet_without_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            migrate_dir(
                &tmp.path().join("roomler-agent"),
                &tmp.path().join("roomler")
            )
            .is_none()
        );
    }
}

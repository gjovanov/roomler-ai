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
//! No copy/move is performed: an upgraded host simply keeps reading and writing
//! its existing tree, which is why enrollment cannot be lost. Renaming the tree
//! on existing hosts is a cosmetic follow-up, deliberately not done here.

use directories::ProjectDirs;
#[cfg(target_os = "windows")]
use std::path::PathBuf;
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
}

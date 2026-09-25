// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-85 — where a recording goes.
//!
//! **The default folder**, per user: Windows `Videos\Roomler` *only if* that
//! folder is local, not inside a cloud-sync root, and survives a real
//! create + write + delete probe — Defender's Controlled Folder Access lets
//! `metadata()` succeed and then blocks the write, and OneDrive's Known Folder
//! Move would upload gigabytes of screen recordings by default. Otherwise
//! `%USERPROFILE%\Roomler Recordings`. macOS `~/Movies/Roomler`; Linux the XDG
//! videos dir + `Roomler`. The reason for a fallback is carried to the UI.
//!
//! **The override** (`record_dir`) is validated by [`validate_record_dir`]: an
//! absolute local path, no `~`, no UNC or `\\?\` device path, no `..`, and no
//! symlink or junction in the part of the path a user controls.
//!
//! **The write rule.** The recorder writes into a folder only as that folder's
//! user (the identity rule — it runs as the console user at normal integrity),
//! staging in `<dir>/.roomler-partial/` and renaming on finish. When the folder
//! refuses the write (the Linux user unit's `ProtectHome=read-only`, CFA), it
//! stages in its own data dir instead, and roomler-desktop — the same user —
//! moves the file into place.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

/// The folder recordings go to, and why it is that one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderChoice {
    pub dir: PathBuf,
    /// Why the preferred folder was not used, for the UI. `None` = it was.
    pub reason: Option<String>,
}

/// Name of the in-folder staging directory.
pub const PARTIAL_DIR: &str = ".roomler-partial";
/// Suffix of a recording still being written.
pub const PARTIAL_SUFFIX: &str = ".partial";

/// The recorder's own data dir (`…/recordings`), used when the destination
/// folder refuses the write. Local, never roaming: a roaming profile would
/// sync gigabytes at logoff.
pub fn data_dir() -> Option<PathBuf> {
    roomler_node_core::appdirs::project_dirs().map(|p| p.data_local_dir().join("recordings"))
}

/// Resolve the folder for the next recording: `configured` when set and
/// usable, otherwise the per-OS default. Never fails outright — the last
/// resort is the recorder's own data dir.
pub fn resolve(configured: Option<&Path>) -> FolderChoice {
    if let Some(dir) = configured {
        match ensure_writable(dir) {
            Ok(()) => {
                return FolderChoice {
                    dir: dir.to_path_buf(),
                    reason: None,
                };
            }
            Err(e) => {
                let fallback = default_choice();
                return FolderChoice {
                    dir: fallback.dir,
                    reason: Some(format!(
                        "the configured folder {} is not writable ({e:#}); using {}",
                        dir.display(),
                        fallback
                            .reason
                            .as_deref()
                            .map(|r| format!("the fallback — {r}"))
                            .unwrap_or_else(|| "the default".into())
                    )),
                };
            }
        }
    }
    default_choice()
}

fn default_choice() -> FolderChoice {
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    let videos = directories::UserDirs::new().and_then(|u| u.video_dir().map(Path::to_path_buf));
    let preferred = videos.map(|v| v.join("Roomler"));
    let alt = home.as_ref().map(|h| h.join("Roomler Recordings"));

    // Why the preferred folder was passed over — or it is the answer.
    let passed_over = match &preferred {
        Some(p) => match cloud_synced_reason(p) {
            Some(why) => why,
            None => match ensure_writable(p) {
                Ok(()) => {
                    return FolderChoice {
                        dir: p.clone(),
                        reason: None,
                    };
                }
                Err(e) => format!(
                    "{} refused a test write ({e:#}) — Controlled Folder Access, or a folder policy",
                    p.display()
                ),
            },
        },
        None => "this account has no Videos folder".into(),
    };
    if let Some(alt) = alt
        && ensure_writable(&alt).is_ok()
    {
        return FolderChoice {
            dir: alt,
            reason: Some(passed_over),
        };
    }
    let dir = data_dir().unwrap_or_else(std::env::temp_dir);
    FolderChoice {
        dir,
        reason: Some(
            "no user folder accepted a test write; using the recorder's own data folder".into(),
        ),
    }
}

/// Why a folder must not be the DEFAULT: it sits inside a cloud-sync root or
/// on a network share. `None` = local.
pub fn cloud_synced_reason(dir: &Path) -> Option<String> {
    let roots: Vec<PathBuf> = ["OneDrive", "OneDriveCommercial", "OneDriveConsumer"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    cloud_synced_reason_in(dir, &roots)
}

/// [`cloud_synced_reason`] against explicit sync roots (testable without
/// touching the process environment).
fn cloud_synced_reason_in(dir: &Path, sync_roots: &[PathBuf]) -> Option<String> {
    let s = dir.to_string_lossy();
    if s.starts_with(r"\\") {
        return Some(format!("{} is on a network share", dir.display()));
    }
    sync_roots
        .iter()
        .find(|root| !root.as_os_str().is_empty() && starts_with_ci(dir, root))
        .map(|root| {
            format!(
                "Videos is inside OneDrive ({}) — recordings would be uploaded",
                root.display()
            )
        })
}

fn starts_with_ci(path: &Path, prefix: &Path) -> bool {
    let p = path.to_string_lossy().to_lowercase();
    let q = prefix.to_string_lossy().to_lowercase();
    let q = q.trim_end_matches(['\\', '/']);
    p == q || p.starts_with(&format!("{q}\\")) || p.starts_with(&format!("{q}/"))
}

/// Create `dir` if needed and prove it accepts a write: create a unique probe
/// file with `create_new`, write a byte, sync, delete. `metadata()` is not
/// evidence — Controlled Folder Access answers it and then refuses the write.
pub fn ensure_writable(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let probe = dir.join(format!(
        ".roomler-write-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let result = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        f.write_all(b"r")?;
        f.sync_all()
    })();
    let _ = std::fs::remove_file(&probe);
    result.with_context(|| format!("test write in {}", dir.display()))
}

/// Why a `record_dir` value is refused. The strings are the config surface's
/// error text, so they name the fix.
pub fn validate_record_dir(raw: &str) -> std::result::Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("record_dir: empty — unset the key to use the default folder".into());
    }
    if raw.starts_with('~') {
        return Err("record_dir: `~` is not expanded — give the full path".into());
    }
    if raw.starts_with(r"\\?\") || raw.starts_with(r"\\.\") || raw.starts_with("//?/") {
        return Err(
            "record_dir: device and verbatim paths (\\\\?\\, \\\\.\\) are not accepted".into(),
        );
    }
    if raw.starts_with(r"\\") || raw.starts_with("//") {
        return Err("record_dir: network shares are not accepted — a dropped link would cut a recording mid-write".into());
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err("record_dir: must be an absolute path".into());
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("record_dir: `..` is not accepted".into());
    }
    if let Some(link) = link_component(&path) {
        return Err(format!(
            "record_dir: {} is a symbolic link or junction — choose the folder it points to",
            link.display()
        ));
    }
    Ok(path)
}

/// The first existing component of `path` that is a symlink or a junction,
/// below the filesystem root. On Windows, `FileType::is_symlink` is true for
/// name-surrogate reparse points (symlinks AND junctions) and false for cloud
/// placeholders, which is exactly the distinction wanted: a placeholder does
/// not redirect a path, a junction does.
pub fn link_component(path: &Path) -> Option<PathBuf> {
    let mut cur = PathBuf::new();
    for (i, c) in path.components().enumerate() {
        cur.push(c.as_os_str());
        // Skip the prefix and root (`C:`, `\`, `/`).
        if i == 0 || matches!(c, Component::RootDir | Component::Prefix(_)) {
            continue;
        }
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => return Some(cur),
            Ok(_) => {}
            Err(_) => break, // the rest does not exist yet
        }
    }
    None
}

/// The file name for a recording started at `now` (local time), made unique
/// in `dir` by appending ` (2)`, ` (3)`, … — a recording never replaces
/// another one.
pub fn unique_recording_name(dir: &Path, now: chrono::DateTime<chrono::Local>) -> String {
    let stem = format!("Roomler Recording {}", now.format("%Y-%m-%d %H-%M-%S"));
    let taken = |name: &str| {
        dir.join(name).exists()
            || dir
                .join(PARTIAL_DIR)
                .join(format!("{name}{PARTIAL_SUFFIX}"))
                .exists()
    };
    let first = format!("{stem}.mp4");
    if !taken(&first) {
        return first;
    }
    (2u32..)
        .map(|n| format!("{stem} ({n}).mp4"))
        .find(|n| !taken(n))
        .unwrap_or(first)
}

/// Bytes free on the volume holding `dir` (the longest mount point that
/// prefixes its canonical path). `None` when it cannot be told.
pub fn available_space(dir: &Path) -> Option<u64> {
    let canon = std::fs::canonicalize(dir).ok()?;
    let canon = strip_verbatim(&canon);
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| starts_with_ci(&canon, d.mount_point()) || canon.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

/// `\\?\C:\x` → `C:\x`, so a canonical Windows path compares with a mount
/// point.
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "roomler-fr85-folder-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_writable_folder_passes_the_probe_and_keeps_nothing() {
        let d = scratch("probe");
        ensure_writable(&d).unwrap();
        assert_eq!(
            std::fs::read_dir(&d).unwrap().count(),
            0,
            "the probe file is removed"
        );
    }

    #[test]
    fn the_probe_creates_a_missing_folder() {
        let d = scratch("create").join("a").join("b");
        ensure_writable(&d).unwrap();
        assert!(d.is_dir());
    }

    #[test]
    fn record_dir_refuses_what_it_should() {
        assert!(validate_record_dir("").is_err());
        assert!(validate_record_dir("~/Videos").is_err());
        assert!(validate_record_dir("relative/dir").is_err());
        assert!(validate_record_dir(r"\\server\share\rec").is_err());
        assert!(validate_record_dir(r"\\?\C:\rec").is_err());
        assert!(validate_record_dir(r"\\.\PhysicalDrive0").is_err());
        let d = scratch("valid");
        let bad = d.join("..").join("x");
        assert!(validate_record_dir(&bad.to_string_lossy()).is_err());
    }

    #[test]
    fn record_dir_accepts_an_absolute_local_path_even_if_missing() {
        let d = scratch("ok").join("not-yet");
        let p = validate_record_dir(&d.to_string_lossy()).unwrap();
        assert_eq!(p, d);
    }

    #[cfg(unix)]
    #[test]
    fn record_dir_refuses_a_symlink_component() {
        let d = scratch("link");
        let target = d.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let link = d.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = validate_record_dir(&link.join("rec").to_string_lossy()).unwrap_err();
        assert!(err.contains("symbolic link"), "{err}");
        // Positive control: the real folder is fine.
        assert!(validate_record_dir(&target.join("rec").to_string_lossy()).is_ok());
    }

    #[test]
    fn names_never_collide() {
        let d = scratch("names");
        let now = chrono::Local::now();
        let a = unique_recording_name(&d, now);
        std::fs::write(d.join(&a), b"x").unwrap();
        let b = unique_recording_name(&d, now);
        assert_ne!(a, b);
        assert!(b.ends_with(" (2).mp4"), "{b}");
        // A partial with the next name also counts as taken.
        std::fs::create_dir_all(d.join(PARTIAL_DIR)).unwrap();
        std::fs::write(
            d.join(PARTIAL_DIR).join(format!("{b}{PARTIAL_SUFFIX}")),
            b"x",
        )
        .unwrap();
        let c = unique_recording_name(&d, now);
        assert!(c.ends_with(" (3).mp4"), "{c}");
    }

    #[test]
    fn a_onedrive_folder_is_not_a_default() {
        let root = PathBuf::from(r"C:\Users\someone\OneDrive - Contoso");
        let inside = root.join("Videos").join("Roomler");
        assert!(cloud_synced_reason_in(&inside, std::slice::from_ref(&root)).is_some());
        // Case-insensitive, as Windows paths are.
        let upper = PathBuf::from(r"C:\USERS\SOMEONE\ONEDRIVE - CONTOSO\Videos");
        assert!(cloud_synced_reason_in(&upper, std::slice::from_ref(&root)).is_some());
        // A sibling that merely shares a prefix is not inside it.
        let sibling = PathBuf::from(r"C:\Users\someone\OneDrive - Contoso Archive\Videos");
        assert!(cloud_synced_reason_in(&sibling, std::slice::from_ref(&root)).is_none());
        assert!(cloud_synced_reason_in(Path::new(r"C:\Users\someone\Videos"), &[root]).is_none());
    }

    #[test]
    fn a_network_share_is_not_a_default() {
        assert!(cloud_synced_reason_in(Path::new(r"\\nas\videos\Roomler"), &[]).is_some());
    }

    #[test]
    fn a_configured_folder_that_refuses_writes_falls_back_with_a_reason() {
        // A FILE where a folder is expected cannot be created or written into.
        let d = scratch("fallback");
        let not_a_dir = d.join("file");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let choice = resolve(Some(&not_a_dir));
        assert_ne!(choice.dir, not_a_dir);
        assert!(choice.reason.unwrap().contains("not writable"));
    }

    #[test]
    fn free_space_is_reported_for_a_real_folder() {
        let d = scratch("space");
        assert!(available_space(&d).is_some_and(|b| b > 0));
    }
}

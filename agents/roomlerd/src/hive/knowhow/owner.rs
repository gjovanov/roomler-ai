// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-3b-1 — whose eyes read a root (decision 38): the account that
//! owns it, never uid 0, and never an account a link chose.
//!
//! The daemon resolves each listed root one component at a time, as the
//! kernel would, and notes who owns every symbolic link on the way. A link
//! must belong to root or to the account that owns where it leads: otherwise
//! an account could point a listed folder of its own at another account's,
//! and the scan would read that one with its owner's eyes. The child,
//! running as the account, resolves the root again and refuses one that is
//! no longer its own ([`super::produce`]).

use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The most links one root may pass through: the kernel's own limit.
const MAX_LINKS: usize = 40;

/// What a root is, and so how it is read (decision 46): a folder is walked
/// for its checkouts, a file is read as a kubeconfig.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RootKind {
    #[default]
    Folder,
    Kubeconfig,
}

/// A root, ready to read: where it resolves, what it is, and as whom. `None`
/// reads it as the daemon itself, which is then the account that owns it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Root {
    pub path: PathBuf,
    pub kind: RootKind,
    pub account: Option<String>,
}

/// What resolving a listed root met: where it led, what that is, who owns
/// it, and the owner of every link on the way.
#[derive(Debug)]
pub(super) struct Resolved {
    pub path: PathBuf,
    pub owner: u32,
    pub is_dir: bool,
    pub is_file: bool,
    pub links: Vec<(PathBuf, u32)>,
}

/// `listed`, resolved, and the account that reads it, or why it is not read.
pub(super) fn prepare(listed: &Path, daemon_uid: u32) -> Result<Root, String> {
    if !listed.is_absolute() {
        return Err("not an absolute path".into());
    }
    let resolved = resolve(listed)?;
    let account = match whose(&resolved, daemon_uid)? {
        None => None,
        Some(uid) => Some(crate::exec::account_name(uid)?),
    };
    let kind = if resolved.is_dir {
        RootKind::Folder
    } else {
        RootKind::Kubeconfig
    };
    Ok(Root {
        path: resolved.path,
        kind,
        account,
    })
}

/// Whose eyes read a root: the rule alone, over what resolving it met.
/// `Some(uid)`: a root daemon drops to that account; `None`: the daemon reads
/// it as itself.
pub(super) fn whose(r: &Resolved, daemon_uid: u32) -> Result<Option<u32>, String> {
    if !r.is_dir && !r.is_file {
        return Err("neither a folder nor a file".into());
    }
    // Never as root: not because root could not read it, but because a scan
    // sees what one account sees, and root sees everything (a cluster's
    // `admin.conf` among it).
    if r.owner == 0 {
        return Err(if r.is_dir {
            "owned by root, so never read: list the folders below it"
        } else {
            "owned by root, so never read"
        }
        .into());
    }
    if let Some((link, _)) = r.links.iter().find(|(_, uid)| *uid != 0 && *uid != r.owner) {
        return Err(format!(
            "reached through {}, a link another account owns",
            link.display()
        ));
    }
    if daemon_uid == 0 {
        return Ok(Some(r.owner));
    }
    if r.owner == daemon_uid {
        return Ok(None);
    }
    Err(format!(
        "owned by uid {}, and this daemon runs as uid {daemon_uid}, which reads only its own",
        r.owner
    ))
}

/// One step of a path still to resolve.
enum Part {
    Root,
    Up,
    Name(OsString),
}

fn parts(p: &Path) -> Vec<Part> {
    p.components()
        .filter_map(|c| match c {
            Component::RootDir => Some(Part::Root),
            Component::ParentDir => Some(Part::Up),
            Component::Normal(n) => Some(Part::Name(n.to_os_string())),
            Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

/// Resolve `listed` one component at a time, noting the owner of every link
/// met. `..` steps back from where the path has really got to, as the kernel
/// does, never from what was spelled.
pub(super) fn resolve(listed: &Path) -> Result<Resolved, String> {
    // A stack, the next part on top.
    let mut todo = parts(listed);
    todo.reverse();
    let mut at = PathBuf::from("/");
    let mut links = Vec::new();
    while let Some(part) = todo.pop() {
        match part {
            Part::Root => at = PathBuf::from("/"),
            Part::Up => {
                at.pop();
            }
            Part::Name(name) => {
                let next = at.join(&name);
                let meta = std::fs::symlink_metadata(&next)
                    .map_err(|e| format!("{}: {e}", next.display()))?;
                if !meta.file_type().is_symlink() {
                    at = next;
                    continue;
                }
                if links.len() >= MAX_LINKS {
                    return Err(format!("{}: too many links", listed.display()));
                }
                let target =
                    std::fs::read_link(&next).map_err(|e| format!("{}: {e}", next.display()))?;
                links.push((next, meta.uid()));
                // The target's parts go first. A relative target resolves from
                // the link's own folder, where `at` still is; an absolute one
                // starts again at `/`, as its first part says.
                let mut more = parts(&target);
                more.reverse();
                todo.extend(more);
            }
        }
    }
    let meta = std::fs::metadata(&at).map_err(|e| format!("{}: {e}", at.display()))?;
    Ok(Resolved {
        path: at,
        owner: meta.uid(),
        is_dir: meta.is_dir(),
        is_file: meta.is_file(),
        links,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(owner: u32, links: &[u32]) -> Resolved {
        Resolved {
            path: PathBuf::from("/home/dev/src"),
            owner,
            is_dir: true,
            is_file: false,
            links: links
                .iter()
                .enumerate()
                .map(|(i, uid)| (PathBuf::from(format!("/link{i}")), *uid))
                .collect(),
        }
    }

    /// A root daemon reads a root as its owner; a daemon that is not root
    /// reads only its own roots, as itself.
    #[test]
    fn a_root_is_read_as_its_owner() {
        assert_eq!(whose(&resolved(1000, &[]), 0), Ok(Some(1000)));
        assert_eq!(whose(&resolved(1000, &[]), 1000), Ok(None));
        let why = whose(&resolved(1001, &[]), 1000).unwrap_err();
        assert!(why.contains("reads only its own"), "{why}");
    }

    /// Never as root, folder or file, and never what is neither.
    #[test]
    fn a_root_root_owns_is_never_read() {
        let why = whose(&resolved(0, &[]), 0).unwrap_err();
        assert!(why.contains("owned by root"), "{why}");
        let mut file = resolved(0, &[]);
        (file.is_dir, file.is_file) = (false, true);
        assert!(whose(&file, 0).unwrap_err().contains("owned by root"));
        let mut other = resolved(1000, &[]);
        other.is_dir = false;
        assert!(whose(&other, 0).is_err(), "a socket, a device");
    }

    /// Decision 46: a file listed as a root is read as a kubeconfig, by the
    /// account that owns it; a folder is walked.
    #[test]
    fn a_file_root_is_a_kubeconfig_and_a_folder_is_walked() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("config");
        std::fs::write(&file, "kind: Config\n").unwrap();
        let me = std::fs::metadata(&file).unwrap().uid();
        if me == 0 {
            // Every temp file is root's here, and never read.
            return;
        }
        let root = prepare(&file, me).unwrap();
        assert_eq!((root.kind, root.account), (RootKind::Kubeconfig, None));
        assert_eq!(prepare(tmp.path(), me).unwrap().kind, RootKind::Folder);
    }

    /// A link on the way belongs to root or to the root's owner, or the root
    /// is refused: no link chooses whose eyes read it.
    #[test]
    fn a_link_cannot_choose_the_account() {
        assert_eq!(whose(&resolved(1000, &[0, 1000]), 0), Ok(Some(1000)));
        let why = whose(&resolved(1000, &[0, 1001]), 0).unwrap_err();
        assert!(why.contains("a link another account owns"), "{why}");
        assert!(why.contains("/link1"), "names the link: {why}");
    }

    /// Resolving notes every link met, steps back from where a link led, and
    /// stops at a loop.
    #[test]
    fn resolving_notes_each_link_and_its_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let real = base.join("real").join("src");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("via")).unwrap();
        let me = std::fs::metadata(&base).unwrap().uid();

        let r = resolve(&base.join("via").join("src")).unwrap();
        assert_eq!(r.path, real);
        assert!(r.is_dir);
        assert_eq!(r.links, vec![(base.join("via"), me)]);

        // `..` after the link is the parent of where it led, not of the link.
        std::os::unix::fs::symlink("real/src", base.join("deep")).unwrap();
        let r = resolve(&base.join("deep").join("..").join("src")).unwrap();
        assert_eq!(r.path, real);

        std::os::unix::fs::symlink(base.join("loop"), base.join("loop")).unwrap();
        let why = resolve(&base.join("loop")).unwrap_err();
        assert!(why.contains("too many links"), "{why}");
        assert!(resolve(&base.join("missing")).is_err());
    }

    #[test]
    fn a_relative_root_is_refused() {
        assert!(prepare(Path::new("src"), 0).is_err());
    }
}

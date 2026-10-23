// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-3b-1 — the walk under one root, in the child, as the root's
//! account: where the git checkouts are.
//!
//! | Bound | Why |
//! |---|---|
//! | [`Bounds::max_depth`] levels below the root | a deliberate reach: what lies deeper is never found, the same at every scan |
//! | [`Bounds::max_entries`] entries | a tree that large is cut, and says so |
//! | [`Bounds::deadline`] | a slow disk is cut, and says so |
//! | the root's own filesystem | a hung network share mounted under it cannot stall the walk |
//! | no link followed | nothing outside the root is reached through one |
//! | sorted | a walk cut at a bound is cut at the same place each time |
//!
//! ⚠️ A cut walk is reported as cut, and the answer is then `truncated`, so
//! the server archives nothing it did not see again (`docs/knowhow.md`
//! §11.1). The depth bound is not a cut: it is the same at every scan.

use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Folders never walked into: build output and dependency trees, which
/// hold no checkout worth naming and can be enormous. Dot-folders are
/// skipped as well ([`skipped`]).
const SKIP: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "venv",
    "__pycache__",
    "bower_components",
    "dist",
    "build",
];

/// A walk's bounds.
pub(super) struct Bounds {
    pub max_depth: usize,
    pub max_entries: usize,
    pub deadline: Instant,
}

/// What a walk found.
#[derive(Debug, Default)]
pub(super) struct Walked {
    /// Each checkout's folder, in the order walked.
    pub checkouts: Vec<PathBuf>,
    /// It stopped at a bound or its deadline: what it found is not all there
    /// is.
    pub cut: bool,
    /// The entries it read.
    pub entries: usize,
}

/// Whether the walk passes `name` by: the manifests' walk too
/// ([`super::manifests`]).
pub(super) fn skipped(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with('.') || SKIP.contains(&name.as_ref())
}

/// A folder holding `.git`, a folder or a file naming one (a worktree's, a
/// submodule's), is a checkout. A `.git` that is a link is not followed.
pub(super) fn is_checkout(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(".git")).is_ok_and(|m| m.is_dir() || m.is_file())
}

/// Walk `root` for checkouts. A checkout is not walked into for others.
pub(super) fn walk(root: &Path, b: &Bounds) -> Walked {
    let mut out = Walked::default();
    let Ok(top) = std::fs::symlink_metadata(root) else {
        out.cut = true;
        return out;
    };
    let device = top.dev();
    // Depth first, each folder's children pushed in reverse, so they come off
    // the stack in sorted order.
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if Instant::now() >= b.deadline {
            out.cut = true;
            break;
        }
        if is_checkout(&dir) {
            out.checkouts.push(dir);
            continue;
        }
        if depth >= b.max_depth {
            continue;
        }
        // A folder this account cannot read holds nothing it can be shown:
        // passed by, the same at every scan, so not a cut.
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        names.sort();
        let mut below = Vec::new();
        for name in names {
            out.entries += 1;
            if out.entries > b.max_entries {
                out.cut = true;
                break;
            }
            if skipped(&name) {
                continue;
            }
            let path = dir.join(&name);
            // `symlink_metadata`: a link is never a folder here, so none is
            // followed.
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() && meta.dev() == device {
                below.push(path);
            }
        }
        if out.cut {
            break;
        }
        stack.extend(below.into_iter().rev().map(|p| (p, depth + 1)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn bounds() -> Bounds {
        Bounds {
            max_depth: 8,
            max_entries: 100_000,
            deadline: Instant::now() + Duration::from_secs(30),
        }
    }

    fn checkout(at: &Path) {
        std::fs::create_dir_all(at.join(".git")).unwrap();
    }

    fn rel(root: &Path, w: &Walked) -> Vec<String> {
        w.checkouts
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().display().to_string())
            .collect()
    }

    /// The checkouts under a root, in sorted order: a `.git` folder or file
    /// each, the root itself one when it is, none walked into.
    #[test]
    fn the_walk_finds_checkouts_in_order_and_never_walks_into_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        checkout(&root.join("b-api"));
        checkout(&root.join("a-web"));
        // Inside a checkout: never reached.
        checkout(&root.join("a-web").join("vendored"));
        // A worktree: `.git` is a file.
        std::fs::create_dir_all(root.join("c-wt")).unwrap();
        std::fs::write(root.join("c-wt").join(".git"), "gitdir: /elsewhere\n").unwrap();
        checkout(&root.join("group").join("d-deep"));
        std::fs::create_dir_all(root.join("plain")).unwrap();

        let w = walk(root, &bounds());
        assert_eq!(rel(root, &w), ["a-web", "b-api", "c-wt", "group/d-deep"]);
        assert!(!w.cut);

        let w = walk(&root.join("a-web"), &bounds());
        assert_eq!(w.checkouts, vec![root.join("a-web")], "the root itself");
    }

    /// Dot-folders, dependency trees and links are passed by.
    #[test]
    fn the_walk_skips_dot_folders_dependency_trees_and_links() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        checkout(&root.join(".hidden").join("x"));
        checkout(&root.join("node_modules").join("y"));
        checkout(&root.join("target").join("z"));
        let outside = tempfile::tempdir().unwrap();
        checkout(&outside.path().join("w"));
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        checkout(&root.join("kept"));

        let w = walk(root, &bounds());
        assert_eq!(rel(root, &w), ["kept"]);
    }

    /// The depth bound is a reach, not a cut; the entry bound and the
    /// deadline are cuts.
    #[test]
    fn the_walk_keeps_its_bounds() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        checkout(&root.join("one").join("two").join("three"));
        let shallow = Bounds {
            max_depth: 2,
            ..bounds()
        };
        let w = walk(root, &shallow);
        assert!(w.checkouts.is_empty(), "three is three levels down");
        assert!(!w.cut, "a reach, the same at every scan");
        let w = walk(
            root,
            &Bounds {
                max_depth: 3,
                ..bounds()
            },
        );
        assert_eq!(rel(root, &w), ["one/two/three"]);

        for i in 0..5 {
            std::fs::create_dir_all(root.join(format!("d{i}"))).unwrap();
        }
        let few = Bounds {
            max_entries: 3,
            ..bounds()
        };
        assert!(walk(root, &few).cut);

        let late = Bounds {
            deadline: Instant::now(),
            ..bounds()
        };
        assert!(walk(root, &late).cut);
        assert!(walk(&root.join("missing"), &bounds()).cut);
    }
}

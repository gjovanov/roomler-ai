// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-3b-1 — what a checkout says of itself, read as text, in the
//! child, as the root's account: its remotes, its branch, its first commit,
//! and (P4-3b-2) the services its manifests name ([`super::manifests`]).
//!
//! Never through git's porcelain, which reads a repository's config and may
//! run what it names. Only the first commit asks git, and only a real git
//! ([`find_git`]), with hooks and fsmonitor off, no prompt, no fetch, and 5 s.
//!
//! ⚠️ A remote's URL loses its user and password before anything else sees
//! it, and a remote that still holds a credential's shape is dropped
//! (`holds_credential`): the server refuses such a finding whole, and the
//! checkout should still be named.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use roomler_ai_remote_control::knowhow::{
    Checkout, MAX_PATH, MAX_REMOTES, MAX_TEXT, Remote, holds_credential,
};

/// The most bytes of a git file read: a config, a `HEAD`, a `.git` file.
const MAX_GIT_FILE: u64 = 256 * 1024;
/// How long git may take to name a checkout's first commit.
const GIT_LIMIT: Duration = Duration::from_secs(5);

/// A real git, by an absolute path, never `PATH`. On macOS `/usr/bin/git` is
/// left out: without the developer tools it is a stub that offers the
/// person at the console an install dialog. Their git is the tools' own.
pub(super) fn find_git() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    const CANDIDATES: &[&str] = &[
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
        "/Library/Developer/CommandLineTools/usr/bin/git",
        "/Applications/Xcode.app/Contents/Developer/usr/bin/git",
    ];
    #[cfg(not(target_os = "macos"))]
    const CANDIDATES: &[&str] = &["/usr/bin/git", "/usr/local/bin/git", "/bin/git"];
    CANDIDATES.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// What the checkout in `dir` says of itself. `None` when it is not one
/// after all, or its path is not text.
pub(super) fn read(dir: &Path, git: Option<&Path>, deadline: Instant) -> Option<Checkout> {
    let path = dir.to_str()?.to_string();
    if path.len() > MAX_PATH {
        return None;
    }
    let git_dir = git_dir(dir)?;
    let common = common_dir(&git_dir);
    let remotes = read_small(&common.join("config"))
        .map(|text| remotes(&text))
        .unwrap_or_default();
    let branch = read_small(&git_dir.join("HEAD")).and_then(|text| branch(&text));
    // A shallow clone's oldest commit is where the clone stopped, not where
    // the repository began.
    let root_commit = match git {
        Some(git) if !common.join("shallow").exists() => root_commit(git, dir, deadline),
        _ => None,
    };
    Some(Checkout {
        path,
        remotes,
        root_commit,
        branch,
        services: super::manifests::services(dir, deadline),
    })
}

/// The checkout's git folder: `.git` itself, or where a `.git` file points.
fn git_dir(dir: &Path) -> Option<PathBuf> {
    let dot = dir.join(".git");
    let meta = std::fs::symlink_metadata(&dot).ok()?;
    if meta.is_dir() {
        return Some(dot);
    }
    if !meta.is_file() {
        return None;
    }
    let text = read_small(&dot)?;
    let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        dir.join(target)
    })
}

/// Where a worktree's shared config and objects are (`commondir`), or the
/// git folder itself.
fn common_dir(git_dir: &Path) -> PathBuf {
    match read_small(&git_dir.join("commondir")) {
        Some(text) if !text.trim().is_empty() => {
            let common = Path::new(text.trim());
            if common.is_absolute() {
                common.to_path_buf()
            } else {
                git_dir.join(common)
            }
        }
        _ => git_dir.to_path_buf(),
    }
}

/// A small text file, or nothing: too large, unreadable or not UTF-8.
fn read_small(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_GIT_FILE + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_GIT_FILE {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Each remote's first `url` in a git config, cleaned ([`clean_url`]).
pub(super) fn remotes(config: &str) -> Vec<Remote> {
    let mut out: Vec<Remote> = Vec::new();
    let mut section: Option<String> = None;
    for raw in config.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            section = remote_section(line);
            continue;
        }
        let Some(name) = &section else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("url") || out.iter().any(|r| &r.name == name) {
            continue;
        }
        if let Some(url) = clean_url(&unquote(value)) {
            out.push(Remote {
                name: name.clone(),
                url,
            });
            if out.len() >= MAX_REMOTES {
                break;
            }
        }
    }
    out
}

/// `[remote "origin"]` names `origin`; any other section, nothing.
fn remote_section(line: &str) -> Option<String> {
    let inner = line.strip_prefix('[')?.split(']').next()?.trim();
    let (section, rest) = inner.split_once(char::is_whitespace)?;
    if !section.eq_ignore_ascii_case("remote") {
        return None;
    }
    let name = rest.trim().strip_prefix('"')?.strip_suffix('"')?;
    let name = name.replace("\\\"", "\"").replace("\\\\", "\\");
    (!name.is_empty() && name.len() <= MAX_TEXT).then_some(name)
}

/// A config value: a comment outside quotes cut off, the quotes taken out.
fn unquote(value: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    for c in value.trim().chars() {
        match c {
            '"' => quoted = !quoted,
            '#' | ';' if !quoted => break,
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// A remote's URL with its user and password, query and fragment taken out:
/// `https://user:token@host/a/b` is `https://host/a/b`, `git@host:a/b` is
/// `host:a/b`. `None` when nothing is left, it is too long, or it still holds
/// a credential's shape.
pub(super) fn clean_url(raw: &str) -> Option<String> {
    let url = raw.trim();
    let cleaned = if let Some((scheme, rest)) = url.split_once("://") {
        let rest = rest.split(['?', '#']).next().unwrap_or_default();
        let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        format!("{scheme}://{host}{path}")
    } else if let Some((left, path)) = scp_like(url) {
        let host = left.rsplit_once('@').map_or(left, |(_, host)| host);
        format!("{host}:{path}")
    } else {
        url.to_string()
    };
    if cleaned.is_empty() || cleaned.len() > MAX_TEXT || holds_credential(&cleaned) {
        return None;
    }
    Some(cleaned)
}

/// Git's scp-like form, `[user@]host:path`: a colon before any slash.
fn scp_like(url: &str) -> Option<(&str, &str)> {
    let colon = url.find(':')?;
    if url.find('/').is_some_and(|slash| slash < colon) {
        return None;
    }
    Some((&url[..colon], &url[colon + 1..]))
}

/// The branch `HEAD` names; `None` when it is detached.
pub(super) fn branch(head: &str) -> Option<String> {
    let name = head.lines().next()?.strip_prefix("ref:")?.trim();
    let name = name.strip_prefix("refs/heads/")?;
    (!name.is_empty() && name.len() <= MAX_TEXT).then(|| name.to_string())
}

/// The checkout's first commit, by `git rev-list --max-parents=0 HEAD`: the
/// smallest when its history has several. `None` when git cannot say within
/// its limit.
fn root_commit(git: &Path, dir: &Path, deadline: Instant) -> Option<String> {
    let limit = deadline
        .checked_duration_since(Instant::now())?
        .min(GIT_LIMIT);
    let mut child = Command::new(git)
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(["rev-list", "--max-parents=0", "HEAD"])
        // Neither the system's config nor the account's: a `safe.directory`
        // there would let git read a repository another account owns.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let until = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut out = String::new();
    child
        .stdout
        .take()?
        .take(64 * 1024)
        .read_to_string(&mut out)
        .ok()?;
    out.lines()
        .map(str::trim)
        .filter(|l| is_object_id(l))
        .min()
        .map(str::to_string)
}

/// A SHA-1 or SHA-256 object id, as git prints it.
fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each remote's first URL, quoted or not, a comment cut off, other
    /// sections passed by.
    #[test]
    fn remotes_are_read_from_the_config() {
        let config = "[core]\n\turl = not-a-remote\n\
                      [remote \"origin\"]\n\turl = https://github.com/acme/api.git\n\
                      \turl = https://mirror.example/acme/api.git\n\
                      \tfetch = +refs/heads/*:refs/remotes/origin/*\n\
                      [Remote \"up stream\"]\n\turl = \"git@github.com:up/api.git\" # theirs\n\
                      [branch \"main\"]\n\tremote = origin\n";
        let r = remotes(config);
        assert_eq!(
            r,
            vec![
                Remote {
                    name: "origin".into(),
                    url: "https://github.com/acme/api.git".into(),
                },
                Remote {
                    name: "up stream".into(),
                    url: "github.com:up/api.git".into(),
                },
            ]
        );
    }

    /// A URL loses its user and password, query and fragment; a local path
    /// stays as it is; one still holding a credential's shape is dropped.
    #[test]
    fn a_remote_loses_its_user_and_password() {
        let secret = ["to", "ken"].concat();
        assert_eq!(
            clean_url(&format!(
                "https://dev:{secret}@host.example:8443/a/b.git?x=1#f"
            )),
            Some("https://host.example:8443/a/b.git".into())
        );
        assert_eq!(
            clean_url("ssh://git@host.example/a/b"),
            Some("ssh://host.example/a/b".into())
        );
        assert_eq!(
            clean_url("git@github.com:acme/api.git"),
            Some("github.com:acme/api.git".into())
        );
        assert_eq!(
            clean_url("/srv/git/api.git"),
            Some("/srv/git/api.git".into())
        );
        assert_eq!(clean_url("../api"), Some("../api".into()));
        assert_eq!(clean_url("  "), None);
        // A known token's prefix in the path, which no user or password held.
        // Built here, never written down whole, as remote_control's tests do.
        let shaped = format!(
            "https://host.example/{}{}",
            ["gh", "p_"].concat(),
            "a1B2".repeat(9)
        );
        assert!(
            holds_credential(&shaped),
            "the fixture holds a token's shape"
        );
        assert_eq!(clean_url(&shaped), None);
    }

    /// `HEAD` names a branch, or nothing when detached.
    #[test]
    fn the_branch_comes_from_head() {
        assert_eq!(branch("ref: refs/heads/feat/x\n"), Some("feat/x".into()));
        assert_eq!(branch("0123456789abcdef0123456789abcdef01234567\n"), None);
        assert_eq!(branch("ref: refs/remotes/origin/main\n"), None);
    }

    /// A worktree's `.git` file points at its own git folder, whose
    /// `commondir` holds the shared config.
    #[test]
    fn a_worktree_reads_the_shared_config() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let common = main.join(".git");
        let own = common.join("worktrees").join("wt");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(
            common.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/acme/api\n",
        )
        .unwrap();
        std::fs::write(common.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(own.join("HEAD"), "ref: refs/heads/feat\n").unwrap();
        std::fs::write(own.join("commondir"), "../..\n").unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", own.display())).unwrap();

        let c = read(&wt, None, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(c.path, wt.to_str().unwrap());
        assert_eq!(c.branch.as_deref(), Some("feat"), "the worktree's own HEAD");
        assert_eq!(c.remotes.len(), 1, "the shared config");
        assert_eq!(c.remotes[0].url, "https://github.com/acme/api");
        assert_eq!(c.root_commit, None, "no git asked");
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new(find_git().unwrap())
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// The first commit, as git names it, and none for a shallow clone.
    #[test]
    fn the_first_commit_is_asked_of_git_and_never_of_a_shallow_clone() {
        let Some(git_bin) = find_git() else {
            eprintln!("no git on this machine: the first commit goes unread");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        for m in ["one", "two"] {
            git(
                &repo,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    m,
                ],
            );
        }
        let out = Command::new(&git_bin)
            .arg("-C")
            .arg(&repo)
            .args(["rev-list", "--max-parents=0", "HEAD"])
            .output()
            .unwrap();
        let first = String::from_utf8(out.stdout).unwrap().trim().to_string();
        let deadline = Instant::now() + Duration::from_secs(10);
        let c = read(&repo, Some(&git_bin), deadline).unwrap();
        assert_eq!(c.root_commit.as_deref(), Some(first.as_str()));
        assert_eq!(c.branch.as_deref(), Some("main"));

        std::fs::write(repo.join(".git").join("shallow"), format!("{first}\n")).unwrap();
        let c = read(&repo, Some(&git_bin), deadline).unwrap();
        assert_eq!(
            c.root_commit, None,
            "a shallow clone's boundary is not its root"
        );
    }

    #[test]
    fn an_object_id_is_forty_or_sixty_four_lowercase_hex() {
        assert!(is_object_id(&"a".repeat(40)));
        assert!(is_object_id(&"0".repeat(64)));
        assert!(!is_object_id(&"A".repeat(40)));
        assert!(!is_object_id(&"a".repeat(41)));
    }
}

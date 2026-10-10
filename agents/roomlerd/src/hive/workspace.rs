// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2b-2 — a session's workspace, checkpointed through the account's own
//! `git` (decision 14), never a git built into the daemon.
//!
//! Runs AS THE SESSION'S ACCOUNT, inside `roomlerd hive-checkpoint` (P2b-3): the
//! daemon never opens a path in the account's tree. Plumbing only, through a
//! temporary index, so the person's index, branches and work tree stay as they
//! were (design §6.3):
//!
//! ```text
//! gd     = git rev-parse --absolute-git-dir           a worktree's own, or a shadow
//! idx    = $gd/hive-<sid>.index, seeded from $gd/index
//! GIT_INDEX_FILE=$idx git add -A -- .                   the folder only; ignores honoured
//! tree   = GIT_INDEX_FILE=$idx git write-tree [--prefix=<the folder>/]
//! commit = git commit-tree $tree [-p <last checkpoint's>]
//! git update-ref refs/hive/<sid>/head $commit           kept through the person's gc
//! pack   = git pack-objects --revs --thin --stdout       <commit> ^<last checkpoint's>
//! ```
//!
//! | The folder is | Repository | Taken |
//! |---|---|---|
//! | a repository's top | its own | the whole tree |
//! | inside a repository | its own | the folder's subtree only (`--prefix`), the repository's ignores still honoured |
//! | in no repository | a shadow, `<state dir>/shadow.git` | the folder, and nothing is written into it |
//!
//! ⚠️ The first checkpoint's commit has no parent, so its pack holds the whole
//! tree and none of the person's history: a member holds nothing else to
//! resolve a delta against. Each later commit is the last checkpoint's child,
//! and its pack is thin against it. The person's `HEAD` is recorded beside it,
//! for a teleport into a clone (P2f).
//!
//! ⚠️ No hook runs (`core.hooksPath` names nowhere), the fsmonitor is off and
//! nothing is signed: plumbing in a person's repository must not run their
//! hooks, or wait on their signing key, on the agent's behalf.
//!
//! ⚠️ A workspace that cannot be taken never fails the checkpoint. The config
//! directory is what a resume needs, so the workspace is skipped, in words.

// P2b-3's `hive-checkpoint` is its caller; until then only its tests run it.
#![cfg_attr(not(test), allow(dead_code))]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use roomler_hive_node::checkpoint::{RepoKind, WorkspaceSnap, chunk_bytes};

/// The largest pack one checkpoint carries. A workspace whose first pack is
/// larger is skipped: it is not one a member can be handed each turn.
pub(crate) const MAX_PACK: usize = 512 * 1024 * 1024;

/// Who the checkpoint commits say made them: never the person, whose name
/// would then sign a commit they did not write.
const AUTHOR_NAME: &str = "Roomler Hive";
const AUTHOR_EMAIL: &str = "hive@roomler.invalid";

/// Environment a caller's `git` could be pointed elsewhere by.
const GIT_ENV: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CONFIG_PARAMETERS",
];

/// The `git` that runs: the account's, on its `PATH`; a test names another.
pub(crate) struct Git {
    program: PathBuf,
    /// Test-only: where discovery stops (`GIT_CEILING_DIRECTORIES`), set on
    /// each command so a repository around a temp directory cannot claim a
    /// folder, without touching the process's environment.
    ceiling: Option<PathBuf>,
}

impl Git {
    pub(crate) fn on_path() -> Self {
        Self {
            program: PathBuf::from("git"),
            ceiling: None,
        }
    }

    #[cfg(test)]
    fn at(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            ceiling: None,
        }
    }

    #[cfg(test)]
    fn under(ceiling: &Path) -> Self {
        Self {
            program: PathBuf::from("git"),
            ceiling: Some(ceiling.to_path_buf()),
        }
    }
}

/// What a workspace checkpoint came to.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The workspace and the pack's blobs, in chunk order.
    Taken {
        snap: WorkspaceSnap,
        blobs: Vec<Vec<u8>>,
    },
    /// Not taken, and why, in words for the checkpoint's `skipped`.
    Skipped(String),
}

/// The repository a folder's checkpoints are written to.
struct Repo {
    kind: RepoKind,
    git_dir: PathBuf,
    /// A shadow's work tree, the folder; an own repository finds its own.
    work_tree: Option<PathBuf>,
    /// The folder's place in its repository, `/`-terminated, or empty.
    prefix: String,
}

/// Checkpoint `folder` for the Hive session `sid` (checkpoint `n`, after turn
/// `turn`), from the last checkpoint's workspace. `state_dir` is the
/// session's own directory, where a shadow repository lives.
pub(crate) fn checkpoint(
    git: &Git,
    folder: &Path,
    state_dir: &Path,
    sid: &str,
    n: u64,
    turn: u32,
    prev: Option<&WorkspaceSnap>,
) -> Outcome {
    match take(git, folder, state_dir, sid, n, turn, prev) {
        Ok(outcome) => outcome,
        Err(why) => Outcome::Skipped(why),
    }
}

fn take(
    git: &Git,
    folder: &Path,
    state_dir: &Path,
    sid: &str,
    n: u64,
    turn: u32,
    prev: Option<&WorkspaceSnap>,
) -> Result<Outcome, String> {
    if sid.len() != 24
        || !sid
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(format!("{sid:?} is not a session id"));
    }
    let repo = find_repo(git, folder, state_dir)?;
    let index = repo.git_dir.join(format!("hive-{sid}.index"));
    if repo.kind == RepoKind::Own {
        // Seeded from the person's index, so only what changed re-hashes; the
        // person's own index is read, never written.
        let theirs = repo.git_dir.join("index");
        if theirs.is_file() {
            std::fs::copy(&theirs, &index).map_err(|e| format!("copying the index: {e}"))?;
        } else {
            let _ = std::fs::remove_file(&index);
        }
    }
    run(
        cmd(git, folder, Some(&repo), Some(&index)).args(["add", "-A", "--", "."]),
        "add",
    )?;
    let tree = write_tree(git, folder, &repo, &index)?;
    let head = match repo.kind {
        RepoKind::Own => run(
            cmd(git, folder, Some(&repo), None).args([
                "rev-parse",
                "--verify",
                "-q",
                "HEAD^{commit}",
            ]),
            "rev-parse HEAD",
        )
        .ok()
        .filter(|h| !h.is_empty()),
        RepoKind::Shadow => None,
    };
    let reference = format!("refs/hive/{sid}/head");
    let same_place = |p: &WorkspaceSnap| p.repo == repo.kind && p.prefix == repo.prefix;

    // A turn that changed no file writes no commit; the ref is put back if
    // someone removed it.
    if let Some(p) = prev.filter(|p| same_place(p) && p.tree == tree) {
        let kept = run(
            cmd(git, folder, Some(&repo), None).args(["update-ref", &reference, &p.commit]),
            "update-ref",
        );
        if kept.is_ok() {
            return Ok(Outcome::Taken {
                snap: WorkspaceSnap {
                    repo: repo.kind,
                    prefix: repo.prefix,
                    tree,
                    commit: p.commit.clone(),
                    base: None,
                    head,
                    pack: Vec::new(),
                },
                blobs: Vec::new(),
            });
        }
    }

    // The last checkpoint's commit is the parent while this repository still
    // holds it; otherwise the chain starts over, whole.
    let parent = prev
        .filter(|p| same_place(p))
        .map(|p| p.commit.clone())
        .filter(|c| {
            run(
                cmd(git, folder, Some(&repo), None).args([
                    "cat-file",
                    "-e",
                    &format!("{c}^{{commit}}"),
                ]),
                "cat-file",
            )
            .is_ok()
        });
    let message = format!("hive {sid} checkpoint {n} turn {turn}");
    let mut commit_tree = cmd(git, folder, Some(&repo), None);
    commit_tree
        .args(["commit-tree", "--no-gpg-sign", &tree, "-m", &message])
        .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
        .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL);
    if let Some(parent) = &parent {
        commit_tree.args(["-p", parent]);
    }
    let commit = run(&mut commit_tree, "commit-tree")?;
    run(
        cmd(git, folder, Some(&repo), None).args(["update-ref", &reference, &commit]),
        "update-ref",
    )?;
    let pack = pack_objects(git, folder, &repo, &commit, parent.as_deref())?;
    let (chunks, blobs) = chunk_bytes(&pack);
    Ok(Outcome::Taken {
        snap: WorkspaceSnap {
            repo: repo.kind,
            prefix: repo.prefix,
            tree,
            commit,
            base: parent,
            head,
            pack: chunks,
        },
        blobs,
    })
}

/// The repository `folder` is in, or its shadow in `state_dir`.
fn find_repo(git: &Git, folder: &Path, state_dir: &Path) -> Result<Repo, String> {
    let inside = cmd(git, folder, None, None)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(spawn_err)?;
    let answer = String::from_utf8_lossy(&inside.stdout).trim().to_string();
    if inside.status.success() && answer == "true" {
        let git_dir = run(
            cmd(git, folder, None, None).args(["rev-parse", "--absolute-git-dir"]),
            "rev-parse --absolute-git-dir",
        )?;
        let prefix = run(
            cmd(git, folder, None, None).args(["rev-parse", "--show-prefix"]),
            "rev-parse --show-prefix",
        )?;
        return Ok(Repo {
            kind: RepoKind::Own,
            git_dir: PathBuf::from(git_dir),
            work_tree: None,
            prefix,
        });
    }
    if inside.status.success() {
        // `false`: inside a repository's git directory, not its work tree.
        return Err("the folder is inside a repository's git directory".into());
    }
    let git_dir = state_dir.join("shadow.git");
    if !git_dir.join("HEAD").is_file() {
        std::fs::create_dir_all(state_dir)
            .map_err(|e| format!("making {}: {e}", state_dir.display()))?;
        run(
            cmd(git, state_dir, None, None)
                .args(["init", "--bare", "--quiet"])
                .arg(&git_dir),
            "init",
        )?;
    }
    Ok(Repo {
        kind: RepoKind::Shadow,
        git_dir,
        work_tree: Some(folder.to_path_buf()),
        prefix: String::new(),
    })
}

/// The folder's tree. Inside a repository that is the subtree at its prefix;
/// a folder with nothing in it has the empty tree.
fn write_tree(git: &Git, folder: &Path, repo: &Repo, index: &Path) -> Result<String, String> {
    let mut c = cmd(git, folder, Some(repo), Some(index));
    c.arg("write-tree");
    if !repo.prefix.is_empty() {
        c.arg(format!("--prefix={}", repo.prefix));
    }
    let out = c.output().map_err(spawn_err)?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let said = String::from_utf8_lossy(&out.stderr);
    if !repo.prefix.is_empty() && said.contains("not found") {
        // The index holds nothing under the prefix: the folder is empty.
        let mut empty = cmd(git, folder, Some(repo), None);
        empty
            .args(["hash-object", "-t", "tree", "-w", "--stdin"])
            .stdin(Stdio::null());
        return run(&mut empty, "hash-object");
    }
    Err(format!("git write-tree failed: {}", tail(&out.stderr)))
}

/// The pack that carries `commit`: thin against `base` when there is one,
/// whole otherwise. Refused when it grows past [`MAX_PACK`].
fn pack_objects(
    git: &Git,
    folder: &Path,
    repo: &Repo,
    commit: &str,
    base: Option<&str>,
) -> Result<Vec<u8>, String> {
    let mut c = cmd(git, folder, Some(repo), None);
    c.args(["pack-objects", "--revs", "--thin", "--stdout", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().map_err(spawn_err)?;
    let mut revs = format!("{commit}\n");
    if let Some(base) = base {
        revs.push_str(&format!("^{base}\n"));
    }
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(revs.as_bytes())
            .map_err(|e| format!("writing to git pack-objects: {e}"))?;
    }
    let mut stderr = child.stderr.take();
    let said = std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(e) = stderr.as_mut() {
            let _ = e.read_to_end(&mut s);
        }
        s
    });
    let mut pack = Vec::new();
    let mut stdout = child
        .stdout
        .take()
        .ok_or("git pack-objects has no stdout")?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let read = stdout
            .read(&mut buf)
            .map_err(|e| format!("reading git pack-objects: {e}"))?;
        if read == 0 {
            break;
        }
        if pack.len() + read > MAX_PACK {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "the workspace's pack is over {} MiB",
                MAX_PACK >> 20
            ));
        }
        pack.extend_from_slice(&buf[..read]);
    }
    let status = child
        .wait()
        .map_err(|e| format!("waiting for git pack-objects: {e}"))?;
    let said = said.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("git pack-objects failed: {}", tail(&said)));
    }
    Ok(pack)
}

/// `git`, run in `folder` against `repo` (and `index`), with nothing of the
/// caller's git environment, no hook, no fsmonitor and no automatic gc.
fn cmd(git: &Git, folder: &Path, repo: Option<&Repo>, index: Option<&Path>) -> Command {
    let mut c = Command::new(&git.program);
    c.current_dir(folder);
    for var in GIT_ENV {
        c.env_remove(var);
    }
    let no_hooks = repo.map_or_else(
        || folder.join(".roomler-hive-no-hooks"),
        |r| r.git_dir.join("hive-no-hooks"),
    );
    c.arg("-c")
        .arg(format!("core.hooksPath={}", no_hooks.display()))
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ])
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    if let Some(r) = repo
        && let Some(work_tree) = &r.work_tree
    {
        c.env("GIT_DIR", &r.git_dir).env("GIT_WORK_TREE", work_tree);
    }
    if let Some(index) = index {
        c.env("GIT_INDEX_FILE", index);
    }
    if let Some(ceiling) = &git.ceiling {
        c.env("GIT_CEILING_DIRECTORIES", ceiling);
    }
    c
}

/// Run a `git` and answer its stdout, trimmed; its stderr's tail on failure.
fn run(c: &mut Command, what: &str) -> Result<String, String> {
    let out = c.output().map_err(spawn_err)?;
    if !out.status.success() {
        return Err(format!("git {what} failed: {}", tail(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn spawn_err(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "git was not found".into()
    } else {
        format!("running git: {e}")
    }
}

fn tail(stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    let s = s.trim();
    let start = s.char_indices().rev().nth(299).map_or(0, |(i, _)| i);
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const SID: &str = "6aca8ef5dac84dd57e492eb7";

    /// `git` in `dir`, as a test sets up a person's repository: its own
    /// identity, no signing, no hooks, nothing from the caller's git env.
    fn person(dir: &Path, args: &[&str]) -> String {
        let mut c = Command::new("git");
        c.current_dir(dir);
        for var in GIT_ENV {
            c.env_remove(var);
        }
        let out = c
            .args([
                "-c",
                "user.name=Person",
                "-c",
                "user.email=person@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn have_git() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    /// The paths a tree holds, recursively.
    fn files_of(dir: &Path, tree: &str) -> BTreeSet<String> {
        person(dir, &["ls-tree", "-r", "--name-only", tree])
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn taken(o: Outcome) -> (WorkspaceSnap, Vec<Vec<u8>>) {
        match o {
            Outcome::Taken { snap, blobs } => (snap, blobs),
            Outcome::Skipped(why) => panic!("skipped: {why}"),
        }
    }

    fn repo_with_a_commit() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        person(dir, &["init", "-q"]);
        write(dir, "a.txt", "one\n");
        write(dir, ".gitignore", "secret.env\n");
        person(dir, &["add", "-A"]);
        person(dir, &["commit", "-q", "-m", "first"]);
        tmp
    }

    /// The person's index, branches, `HEAD` and status are as they were; the
    /// checkpoint is a parentless commit of the work tree as it is (tracked
    /// changes and untracked files, ignores honoured) under the session's ref.
    #[test]
    fn a_checkpoint_commits_the_work_tree_and_touches_nothing_of_the_persons() {
        if !have_git() {
            return eprintln!("no git here");
        }
        let tmp = repo_with_a_commit();
        let dir = tmp.path();
        let state = tempfile::tempdir().unwrap();
        write(dir, "a.txt", "one, changed\n");
        write(dir, "b.txt", "new\n");
        write(dir, "secret.env", "TOKEN=x\n");
        let git_dir = PathBuf::from(person(dir, &["rev-parse", "--absolute-git-dir"]));
        let index_before = std::fs::read(git_dir.join("index")).unwrap();
        let status_before = person(dir, &["status", "--porcelain"]);
        let branches_before = person(dir, &["for-each-ref", "refs/heads"]);
        let head_before = person(dir, &["rev-parse", "HEAD"]);

        let (snap, blobs) = taken(checkpoint(
            &Git::on_path(),
            dir,
            state.path(),
            SID,
            1,
            1,
            None,
        ));

        assert_eq!(snap.repo, RepoKind::Own);
        assert_eq!(snap.prefix, "");
        assert_eq!(snap.head.as_deref(), Some(head_before.as_str()));
        assert_eq!(snap.base, None, "the first has no parent");
        assert!(!blobs.is_empty() && !snap.pack.is_empty());
        let files = files_of(dir, &snap.tree);
        assert_eq!(
            files,
            [".gitignore", "a.txt", "b.txt"].map(String::from).into(),
            "untracked taken, ignored left out"
        );
        assert_eq!(
            person(dir, &["show", &format!("{}:a.txt", snap.tree)]),
            "one, changed",
            "the work tree's bytes, not the index's"
        );
        assert_eq!(
            person(dir, &["rev-parse", &format!("refs/hive/{SID}/head")]),
            snap.commit
        );
        assert_eq!(
            person(dir, &["rev-list", "--parents", "-n", "1", &snap.commit]),
            snap.commit,
            "no parent"
        );
        assert_eq!(
            person(dir, &["log", "-1", "--format=%an <%ae>", &snap.commit]),
            "Roomler Hive <hive@roomler.invalid>"
        );
        assert_eq!(std::fs::read(git_dir.join("index")).unwrap(), index_before);
        assert_eq!(person(dir, &["status", "--porcelain"]), status_before);
        assert_eq!(
            person(dir, &["for-each-ref", "refs/heads"]),
            branches_before
        );
        assert_eq!(person(dir, &["rev-parse", "HEAD"]), head_before);
    }

    /// A later checkpoint is the last one's child, its pack is thin against it,
    /// and a repository that applies the packs in order holds the tree; the
    /// thin pack alone does not resolve. A turn that changed nothing writes no
    /// commit.
    #[test]
    fn later_checkpoints_chain_their_packs_are_thin_and_an_idle_turn_writes_nothing() {
        if !have_git() {
            return eprintln!("no git here");
        }
        let tmp = repo_with_a_commit();
        let dir = tmp.path();
        let state = tempfile::tempdir().unwrap();
        let git = Git::on_path();
        let big: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        write(dir, "big.txt", &big);
        let (one, b1) = taken(checkpoint(&git, dir, state.path(), SID, 1, 1, None));
        write(dir, "big.txt", &format!("{big}one more\n"));
        let (two, b2) = taken(checkpoint(&git, dir, state.path(), SID, 2, 2, Some(&one)));
        assert_eq!(two.base.as_deref(), Some(one.commit.as_str()));
        assert_eq!(
            person(dir, &["rev-parse", &format!("{}^", two.commit)]),
            one.commit
        );
        let (pack1, pack2) = (b1.concat(), b2.concat());
        assert!(
            pack2.len() < pack1.len(),
            "thin: {} vs {}",
            pack2.len(),
            pack1.len()
        );

        // A member's target applies them in order (P2b-4's shape).
        let target = tempfile::tempdir().unwrap();
        person(target.path(), &["init", "-q", "--bare"]);
        for pack in [&pack1, &pack2] {
            let mut c = Command::new("git");
            c.current_dir(target.path())
                .args(["index-pack", "--stdin", "--fix-thin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            for var in GIT_ENV {
                c.env_remove(var);
            }
            let mut child = c.spawn().unwrap();
            child.stdin.take().unwrap().write_all(pack).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert_eq!(files_of(target.path(), &two.tree), files_of(dir, &two.tree));
        assert_eq!(
            person(target.path(), &["show", &format!("{}:big.txt", two.tree)]),
            format!("{big}one more").trim_end()
        );

        // The thin pack alone does not resolve: it is thin.
        let alone = tempfile::tempdir().unwrap();
        person(alone.path(), &["init", "-q", "--bare"]);
        let mut c = Command::new("git");
        c.current_dir(alone.path())
            .args(["index-pack", "--stdin", "--fix-thin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for var in GIT_ENV {
            c.env_remove(var);
        }
        let mut child = c.spawn().unwrap();
        child.stdin.take().unwrap().write_all(&pack2).unwrap();
        assert!(
            !child.wait().unwrap().success(),
            "a thin pack needs its base"
        );

        let (three, b3) = taken(checkpoint(&git, dir, state.path(), SID, 3, 3, Some(&two)));
        assert_eq!(three.commit, two.commit, "no commit for an idle turn");
        assert!(three.pack.is_empty() && b3.is_empty());
    }

    /// A folder inside a repository takes only its own subtree, with the
    /// repository's ignores still honoured; nothing outside it is in the pack.
    #[test]
    fn a_folder_inside_a_repository_takes_only_its_subtree() {
        if !have_git() {
            return eprintln!("no git here");
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        person(dir, &["init", "-q"]);
        write(dir, "top.txt", "outside the session\n");
        write(dir, ".gitignore", "*.env\n");
        write(dir, "sub/x.txt", "inside\n");
        person(dir, &["add", "-A"]);
        person(dir, &["commit", "-q", "-m", "first"]);
        write(dir, "sub/secret.env", "TOKEN=x\n");
        write(dir, "sub/y.txt", "new inside\n");
        let state = tempfile::tempdir().unwrap();
        let (snap, blobs) = taken(checkpoint(
            &Git::on_path(),
            &dir.join("sub"),
            state.path(),
            SID,
            1,
            1,
            None,
        ));
        assert_eq!(snap.prefix, "sub/");
        assert_eq!(
            files_of(dir, &snap.tree),
            ["x.txt", "y.txt"].map(String::from).into()
        );
        let pack = blobs.concat();
        assert!(
            !pack.windows(19).any(|w| w == b"outside the session"),
            "nothing outside the folder"
        );
    }

    /// A folder in no repository gets a shadow in the session's state
    /// directory, and nothing is written into the folder.
    #[test]
    fn a_folder_in_no_repository_gets_a_shadow() {
        if !have_git() {
            return eprintln!("no git here");
        }
        let folder = tempfile::tempdir().unwrap();
        write(folder.path(), "f.txt", "plain\n");
        let state = tempfile::tempdir().unwrap();
        // Discovery stops above the temp directory, so a repository around it
        // (a dotfiles checkout of a home directory) cannot claim the folder.
        let git = Git::under(folder.path().parent().unwrap());
        let (snap, _) = taken(checkpoint(
            &git,
            folder.path(),
            state.path(),
            SID,
            1,
            1,
            None,
        ));
        assert_eq!(snap.repo, RepoKind::Shadow);
        assert!(state.path().join("shadow.git/HEAD").is_file());
        assert!(
            !folder.path().join(".git").exists(),
            "nothing written into the folder"
        );
        let shadow = state.path().join("shadow.git");
        let listed = person(&shadow, &["ls-tree", "-r", "--name-only", &snap.tree]);
        assert_eq!(listed, "f.txt");
    }

    /// No `git`, or a session id that is not one, skips the workspace in words.
    #[test]
    fn no_git_or_a_bad_id_skips_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = Git::at(tmp.path().join("no-such-git"));
        match checkpoint(&missing, tmp.path(), tmp.path(), SID, 1, 1, None) {
            Outcome::Skipped(why) => assert_eq!(why, "git was not found"),
            other => panic!("{other:?}"),
        }
        match checkpoint(
            &Git::on_path(),
            tmp.path(),
            tmp.path(),
            "../etc",
            1,
            1,
            None,
        ) {
            Outcome::Skipped(why) => assert!(why.contains("not a session id"), "{why}"),
            other => panic!("{other:?}"),
        }
    }
}

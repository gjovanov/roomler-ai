// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2f-2a — `roomlerd hive-pack <request>`: git's have and want for a
//! session's workspace, AS THE SESSION'S ACCOUNT.
//!
//! A teleport's target with a clone of the session's repository needs only
//! what the session changed, not the whole tree its first checkpoint packed
//! (spec §3b, "the workspace"). Two questions get it there, each answered by
//! the account's own `git` ([`workspace`]), since the daemon never opens a
//! path in an account's tree (spec §3b):
//!
//! | Asked | Of | Answer |
//! |---|---|---|
//! | `holds {candidates}` | the device a copy goes to | which of `candidates` its folder's repository holds already ([`workspace::holds`]) |
//! | `pack {commit, haves}` | the device a copy comes from | the pack that brings `commit` into a repository holding `haves`: thin against the ones its own repository holds too, whole against none ([`workspace::pack_against`]) |
//!
//! The daemon writes the [`Request`] into the session's runtime directory and
//! starts `roomlerd hive-pack <request>` as the account, as `hive-checkpoint`
//! runs (P2b-3a), and reads what it writes:
//!
//! ```text
//! "HIVEPK1\n"                       the magic
//! u32 le · JSON answer              {"answer":"holds","held":[…]}, or
//!                                   {"answer":"pack","against":[…],"len":N}
//! N bytes                           the pack (a pack only)
//! ```
//!
//! ⚠️ **What comes back is the account's word, and the agent runs as that
//! account.** [`check`] holds it to the request: every commit it names is one
//! the request named, once; the pack within its limit, and a pack at all. Git
//! checks every object of it again where it is used, and the materializer
//! holds the commit's tree to the checkpoint's.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::checkpointer::Runner;
use super::workspace::{self, Git};

/// The hidden subcommand: `roomlerd hive-pack <request>`.
pub const PACK_SUBCOMMAND: &str = "hive-pack";

/// The first bytes of what the child writes.
const MAGIC: &[u8; 8] = b"HIVEPK1\n";

/// The most header bytes read: [`workspace::MAX_HAVES`] commit ids and a
/// length.
const MAX_HEADER: usize = 64 * 1024;

/// The most bytes the daemon reads from the child at all.
pub(crate) const MAX_OUTPUT: u64 = workspace::MAX_PACK as u64 + MAX_HEADER as u64 + 16;

/// How long the child may run: a whole pack of a large workspace is all of
/// it, compressed.
pub(crate) const PACK_TIMEOUT: Duration = Duration::from_secs(120);

/// Bumped when the fields change their meaning; a child refuses another.
pub(crate) const REQUEST_V: u32 = 1;

/// What the daemon asks of the account's git.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Request {
    pub v: u32,
    /// The session's folder on this device.
    pub folder: PathBuf,
    /// The session's state directory on this device: a shadow's home.
    pub state_dir: PathBuf,
    pub ask: Ask,
}

/// The question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Ask {
    /// Which of `candidates` the folder's repository holds.
    Holds { candidates: Vec<String> },
    /// The pack that brings `commit` into a repository holding `haves`.
    Pack { commit: String, haves: Vec<String> },
}

/// The account's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    Holds {
        held: Vec<String>,
    },
    /// `pack` is thin against `against`, every one of them a commit the
    /// request named.
    Pack {
        against: Vec<String>,
        pack: Vec<u8>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
enum Header {
    Holds { held: Vec<String> },
    Pack { against: Vec<String>, len: u64 },
}

/// The child's work, as the account.
pub(crate) fn produce(req: &Request, git: &Git) -> Result<Answer, String> {
    if req.v != REQUEST_V {
        return Err(format!(
            "a request of version {}; this build reads {REQUEST_V}",
            req.v
        ));
    }
    match &req.ask {
        Ask::Holds { candidates } => Ok(Answer::Holds {
            held: workspace::holds(git, &req.folder, &req.state_dir, candidates)?,
        }),
        Ask::Pack { commit, haves } => {
            let (pack, against) =
                workspace::pack_against(git, &req.folder, &req.state_dir, commit, haves)?;
            Ok(Answer::Pack { against, pack })
        }
    }
}

/// The child's stdout: [`MAGIC`], the header, then the pack.
pub(crate) fn write_frames(out: &mut impl Write, answer: &Answer) -> std::io::Result<()> {
    let header = match answer {
        Answer::Holds { held } => Header::Holds { held: held.clone() },
        Answer::Pack { against, pack } => Header::Pack {
            against: against.clone(),
            len: pack.len() as u64,
        },
    };
    let header = serde_json::to_vec(&header).map_err(std::io::Error::other)?;
    out.write_all(MAGIC)?;
    out.write_all(
        &u32::try_from(header.len())
            .map_err(std::io::Error::other)?
            .to_le_bytes(),
    )?;
    out.write_all(&header)?;
    if let Answer::Pack { pack, .. } = answer {
        out.write_all(pack)?;
    }
    out.flush()
}

/// What the child wrote, read back within the limits; [`check`] then holds it
/// to the request.
pub(crate) fn read_frames(bytes: &[u8]) -> Result<Answer, String> {
    let mut at = 0usize;
    let mut take = |n: usize, what: &str| -> Result<&[u8], String> {
        let end = at
            .checked_add(n)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| format!("the answer ends inside {what}"))?;
        let s = &bytes[at..end];
        at = end;
        Ok(s)
    };
    if take(MAGIC.len(), "its magic")? != MAGIC {
        return Err(format!("not {PACK_SUBCOMMAND}'s answer (no magic)"));
    }
    let header_len =
        u32::from_le_bytes(take(4, "the header's length")?.try_into().unwrap()) as usize;
    if header_len > MAX_HEADER {
        return Err(format!("a header of {header_len} bytes"));
    }
    let header: Header = serde_json::from_slice(take(header_len, "the header")?)
        .map_err(|e| format!("the header: {e}"))?;
    let answer = match header {
        Header::Holds { held } => Answer::Holds { held },
        Header::Pack { against, len } => {
            if len > workspace::MAX_PACK as u64 {
                return Err(format!("a pack of {len} bytes"));
            }
            Answer::Pack {
                against,
                pack: take(len as usize, "the pack")?.to_vec(),
            }
        }
    };
    if at != bytes.len() {
        return Err(format!("{} bytes after the answer", bytes.len() - at));
    }
    Ok(answer)
}

/// Hold the child's answer to the request: the answer to its question; every
/// commit it names one the request named, and once; a pack a pack.
pub(crate) fn check(req: &Request, answer: &Answer) -> Result<(), String> {
    fn named_once(named: &[String], asked: &[String], what: &str) -> Result<(), String> {
        for (i, id) in named.iter().enumerate() {
            if !asked.contains(id) {
                return Err(format!("{what} {id}, which the request did not name"));
            }
            if named[..i].contains(id) {
                return Err(format!("{what} {id} twice"));
            }
        }
        Ok(())
    }
    match (&req.ask, answer) {
        (Ask::Holds { candidates }, Answer::Holds { held }) => {
            named_once(held, candidates, "it holds")
        }
        (Ask::Pack { haves, .. }, Answer::Pack { against, pack }) => {
            named_once(against, haves, "a pack against")?;
            if pack.len() > workspace::MAX_PACK {
                return Err(format!("a pack of {} bytes", pack.len()));
            }
            if !pack.starts_with(b"PACK") {
                return Err("what came as the pack is not one".into());
            }
            Ok(())
        }
        _ => Err("an answer to another question".into()),
    }
}

// ─── the daemon's side ──────────────────────────────────────────────────────

/// Ask `req` of the session's account on this device (`who`), through `run`:
/// the child as the account, or in this process in tests. `runtime_dir` is
/// the session's own, where the request goes; `owner` the account's ids, to
/// hand it the request (Unix). The answer is held to the request.
#[cfg_attr(not(all(test, unix)), allow(dead_code))] // P2f-2b's stream asks it
pub(crate) async fn ask(
    run: &Runner,
    runtime_dir: &Path,
    who: &super::materializer::Who,
    owner: Option<(u32, u32)>,
    req: &Request,
) -> Result<Answer, String> {
    match run {
        #[cfg(all(unix, any(test, feature = "hive-test-launcher")))]
        Runner::InProcess { ceiling } => {
            let answer = produce(req, &Git::under(ceiling))?;
            let mut bytes = Vec::new();
            write_frames(&mut bytes, &answer).map_err(|e| e.to_string())?;
            let answer = read_frames(&bytes)?;
            check(req, &answer)?;
            Ok(answer)
        }
        #[cfg(all(test, unix))]
        Runner::Failing(why) => Err((*why).to_string()),
        #[cfg(all(test, unix))]
        Runner::Hangs => std::future::pending().await,
        Runner::Subprocess => {
            let bytes = serde_json::to_vec(req).map_err(|e| e.to_string())?;
            let path = super::checkpointer::write_private(runtime_dir, "pack.json", &bytes, owner)?;
            let exe = PathBuf::from(super::supervisor::own_exe()?);
            #[cfg(unix)]
            {
                run_as(
                    &exe,
                    &path,
                    req,
                    who.account.as_deref(),
                    who.env.clone(),
                    PACK_TIMEOUT,
                )
                .await
            }
            #[cfg(windows)]
            {
                let account = who.account.clone().ok_or("no account to run as")?;
                let req = req.clone();
                tokio::task::spawn_blocking(move || {
                    run_as_console_user(&exe, &path, &req, &account, PACK_TIMEOUT)
                })
                .await
                .map_err(|e| format!("the pack's thread: {e}"))?
            }
        }
    }
}

/// The daemon's side on Unix: start `exe hive-pack <request_path>` as
/// `account` (none in tests: as the daemon), with `env` and nothing of the
/// daemon's own, and hold what it writes to `req`.
#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))] // P2f-2b's stream asks it
pub(crate) async fn run_as(
    exe: &Path,
    request_path: &Path,
    req: &Request,
    account: Option<&str>,
    env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    timeout: Duration,
) -> Result<Answer, String> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg(PACK_SUBCOMMAND)
        .arg(request_path)
        .env_clear()
        .envs(env)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Its own group, which a timeout ends whole, the git it started
        // included ([`super::child::run_fed`]).
        .process_group(0);
    if let Some(account) = account {
        crate::exec::apply_session_run_as(&mut cmd, account)?;
    }
    let out = super::child::run_fed(
        cmd,
        PACK_SUBCOMMAND,
        super::child::nothing,
        timeout,
        MAX_OUTPUT,
    )
    .await?;
    let answer = read_frames(&out)?;
    check(req, &answer)?;
    Ok(answer)
}

/// The daemon's side on Windows: start `exe hive-pack <request_path>` as the
/// user signed in at the console, `account` naming them as `hive_accounts`
/// does, and hold what it writes to `req`. Blocking: the caller runs it off
/// the runtime's threads.
#[cfg(windows)]
#[allow(dead_code)] // P2f-2b's stream asks it
pub(crate) fn run_as_console_user(
    exe: &Path,
    request_path: &Path,
    req: &Request,
    account: &str,
    timeout: Duration,
) -> Result<Answer, String> {
    use crate::hive_win;
    let console = hive_win::console_user().map_err(|(_, e)| e)?;
    hive_win::check_mapping(account, &console).map_err(|(_, e)| e)?;
    let line = hive_win::pack_command_line(exe, request_path)?;
    // SAFETY: `console` holds the token `spawn_as` borrows, across the call.
    let out = unsafe { hive_win::run_checkpoint(console.spawn_as(), &line, timeout, MAX_OUTPUT) };
    drop(console);
    let answer = read_frames(&out?)?;
    check(req, &answer)?;
    Ok(answer)
}

/// This process's own request, when it was started as `roomlerd hive-pack
/// <request>`.
pub fn pack_args() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != PACK_SUBCOMMAND {
        return None;
    }
    let request = args.next()?;
    args.next().is_none().then(|| PathBuf::from(request))
}

/// The child: read the request, answer it as whoever this process runs as
/// (the session's account), write the answer to stdout. 0 on success; 1 with
/// a word on stderr otherwise.
pub fn pack_main(request: &Path) -> i32 {
    let run = || -> Result<(), String> {
        let text =
            std::fs::read(request).map_err(|e| format!("reading {}: {e}", request.display()))?;
        if text.len() > MAX_HEADER {
            return Err("the request is too large".into());
        }
        let req: Request =
            serde_json::from_slice(&text).map_err(|e| format!("the request: {e}"))?;
        let answer = produce(&req, &Git::on_path())?;
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        write_frames(&mut out, &answer).map_err(|e| format!("writing the answer: {e}"))
    };
    match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("roomlerd {PACK_SUBCOMMAND}: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`super::run_as`], again while the script it runs is busy
    /// ([`super::super::child::unbusy`]): a test here writes the child it
    /// runs, and another test of this binary may fork meanwhile.
    #[cfg(unix)]
    async fn run_child(
        exe: &Path,
        request_path: &Path,
        req: &Request,
        env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        timeout: Duration,
    ) -> Result<Answer, String> {
        super::super::child::unbusy(|| {
            super::run_as(exe, request_path, req, None, env.clone(), timeout)
        })
        .await
    }

    /// `git` as a person runs it in `dir`.
    #[cfg(unix)]
    fn person(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .args([
                "-c",
                "user.name=Person",
                "-c",
                "user.email=person@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
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

    #[cfg(unix)]
    fn have_git() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// A repository with two commits in `dir`, and their ids, oldest first.
    #[cfg(unix)]
    fn repository(dir: &Path) -> (String, String) {
        person(dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        person(dir, &["add", "-A"]);
        person(dir, &["commit", "-q", "-m", "first"]);
        let first = person(dir, &["rev-parse", "HEAD"]);
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        person(dir, &["add", "-A"]);
        person(dir, &["commit", "-q", "-m", "second"]);
        (first, person(dir, &["rev-parse", "HEAD"]))
    }

    fn request(dir: &Path, ask: Ask) -> Request {
        Request {
            v: REQUEST_V,
            folder: dir.to_path_buf(),
            state_dir: dir.join(".state"),
            ask,
        }
    }

    /// The bytes a pack starts with, and nothing after.
    fn pack_bytes() -> Vec<u8> {
        let mut p = b"PACK".to_vec();
        p.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 0]);
        p
    }

    /// Each answer's frames read back as it was; frames cut short, padded,
    /// too large or without the magic do not.
    #[test]
    fn each_answers_frames_read_back_and_nothing_else_does() {
        let answers = [
            Answer::Holds {
                held: vec!["a".repeat(40)],
            },
            Answer::Pack {
                against: vec!["b".repeat(40)],
                pack: pack_bytes(),
            },
        ];
        for answer in answers {
            let mut bytes = Vec::new();
            write_frames(&mut bytes, &answer).unwrap();
            assert_eq!(read_frames(&bytes).unwrap(), answer);
            assert!(read_frames(&bytes[..bytes.len() - 1]).is_err());
            let mut padded = bytes.clone();
            padded.push(0);
            let got = read_frames(&padded);
            assert!(
                matches!(&got, Err(e) if e.contains("after the answer")),
                "bytes after the answer are refused: {got:?}"
            );
        }
        let e = read_frames(b"HIVEPK2\n").unwrap_err();
        assert!(e.contains("magic"), "{e}");
        let mut big = MAGIC.to_vec();
        big.extend_from_slice(&((MAX_HEADER + 1) as u32).to_le_bytes());
        let e = read_frames(&big).unwrap_err();
        assert!(e.contains("a header of"), "{e}");
        let mut huge = MAGIC.to_vec();
        let header = format!(
            "{{\"answer\":\"pack\",\"against\":[],\"len\":{}}}",
            workspace::MAX_PACK + 1
        );
        huge.extend_from_slice(&(header.len() as u32).to_le_bytes());
        huge.extend_from_slice(header.as_bytes());
        let e = read_frames(&huge).unwrap_err();
        assert!(e.contains("a pack of"), "{e}");
    }

    /// The account's word is held to the request: only commits it named, and
    /// each once; an answer to its own question; a pack that is one.
    #[test]
    fn an_answer_is_held_to_its_request() {
        let (a, b, c) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        let dir = Path::new("/nowhere");
        let holds = request(
            dir,
            Ask::Holds {
                candidates: vec![a.clone(), b.clone()],
            },
        );
        let pack = request(
            dir,
            Ask::Pack {
                commit: c.clone(),
                haves: vec![a.clone()],
            },
        );
        let held = |held: Vec<String>| Answer::Holds { held };
        let packed = |against: Vec<String>, pack: Vec<u8>| Answer::Pack { against, pack };
        let refused = |r: Result<(), String>, word: &str| matches!(&r, Err(e) if e.contains(word));
        assert!(check(&holds, &held(vec![b.clone(), a.clone()])).is_ok());
        assert!(check(&holds, &held(Vec::new())).is_ok());
        assert!(
            refused(check(&holds, &held(vec![c.clone()])), "did not name"),
            "a commit the request did not name is refused"
        );
        assert!(
            refused(check(&holds, &held(vec![a.clone(), a.clone()])), "twice"),
            "a commit named twice is refused"
        );
        assert!(check(&pack, &packed(vec![a.clone()], pack_bytes())).is_ok());
        assert!(check(&pack, &packed(Vec::new(), pack_bytes())).is_ok());
        assert!(
            refused(check(&pack, &packed(vec![b], pack_bytes())), "did not name"),
            "a pack against a commit the request did not name is refused"
        );
        assert!(
            refused(
                check(&pack, &packed(Vec::new(), b"not a pack".to_vec())),
                "is not one"
            ),
            "a pack that is none is refused"
        );
        assert!(
            refused(check(&pack, &held(vec![a])), "another question"),
            "an answer to another question is refused"
        );
    }

    /// Asked in this process, through the same frames and checks: which
    /// commits the folder's repository holds, and a pack against one of them
    /// that brings the newer commit; another version's request is refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_account_answers_each_question_through_its_own_git() {
        if !have_git() {
            return eprintln!("no git here");
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("work");
        std::fs::create_dir_all(&dir).unwrap();
        let (first, second) = repository(&dir);
        let run = Runner::InProcess {
            ceiling: tmp.path().to_path_buf(),
        };
        let who = super::super::materializer::Who {
            account: None,
            env: Vec::new(),
        };
        let unknown = "e".repeat(40);
        let asked = request(
            &dir,
            Ask::Holds {
                candidates: vec![unknown.clone(), second.clone()],
            },
        );
        assert_eq!(
            ask(&run, tmp.path(), &who, None, &asked).await.unwrap(),
            Answer::Holds {
                held: vec![second.clone()]
            }
        );
        let asked = request(
            &dir,
            Ask::Pack {
                commit: second,
                haves: vec![unknown, first.clone()],
            },
        );
        let Answer::Pack { against, pack } =
            ask(&run, tmp.path(), &who, None, &asked).await.unwrap()
        else {
            panic!("a pack");
        };
        assert_eq!(against, vec![first]);
        // The second commit, its tree and `b.txt`: `a.txt` is the first one's.
        assert_eq!(u32::from_be_bytes(pack[8..12].try_into().unwrap()), 3);
        let mut other = asked.clone();
        other.v = REQUEST_V + 1;
        let got = ask(&run, tmp.path(), &who, None, &other).await;
        assert!(
            matches!(&got, Err(e) if e.contains("version")),
            "another version's request is refused: {got:?}"
        );
    }

    /// The daemon's side, through a real child: what it writes is read,
    /// checked and answered; a child that answers what was not asked, or
    /// fails, is refused in words.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_daemon_reads_and_checks_what_a_real_child_writes() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let held = "a".repeat(40);
        let req = request(
            tmp.path(),
            Ask::Holds {
                candidates: vec![held.clone()],
            },
        );
        let frames_of = |answer: &Answer, name: &str| {
            let mut bytes = Vec::new();
            write_frames(&mut bytes, answer).unwrap();
            let p = tmp.path().join(name);
            std::fs::write(&p, &bytes).unwrap();
            p
        };
        let good = frames_of(
            &Answer::Holds {
                held: vec![held.clone()],
            },
            "good.bin",
        );
        let lies = frames_of(
            &Answer::Holds {
                held: vec!["b".repeat(40)],
            },
            "lies.bin",
        );
        let child = |name: &str, body: &str| {
            let p = tmp.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let request_path = tmp.path().join("pack.json");
        let path = vec![("PATH".into(), "/usr/bin:/bin".into())];
        let fast = Duration::from_secs(10);

        let answers = child(
            "good.sh",
            &format!("[ \"$1\" = {PACK_SUBCOMMAND} ] && cat {}", good.display()),
        );
        assert_eq!(
            run_child(&answers, &request_path, &req, path.clone(), fast)
                .await
                .unwrap(),
            Answer::Holds { held: vec![held] }
        );
        let lying = child("lies.sh", &format!("cat {}", lies.display()));
        let got = run_child(&lying, &request_path, &req, path.clone(), fast).await;
        assert!(
            matches!(&got, Err(e) if e.contains("did not name")),
            "a child's answer is held to the request: {got:?}"
        );
        let fails = child("fails.sh", "echo no such folder >&2; exit 3");
        let e = run_child(&fails, &request_path, &req, path, fast)
            .await
            .unwrap_err();
        assert!(e.contains("no such folder"), "{e}");
    }
}

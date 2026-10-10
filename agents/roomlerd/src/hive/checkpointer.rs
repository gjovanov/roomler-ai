// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2b-3a — `roomlerd hive-checkpoint`: a session's checkpoint, taken AS
//! THE SESSION'S ACCOUNT and handed to the daemon on stdout.
//!
//! The daemon never opens a path in the account's tree (spec §3b). So it writes
//! a [`Request`] (where the session's config directory, folder and state
//! directory are, the checkpoint's number and turn, and the last checkpoint)
//! into the session's runtime directory, starts `roomlerd hive-checkpoint
//! <request>` as the account, as the Unix wrapper and Windows' `hive-prep` run,
//! and reads what it writes:
//!
//! ```text
//! "HIVECP1\n"                         the magic
//! u32 le · JSON {"checkpoint": …}     the header
//! u32 le                              how many blobs
//! ( u64 le · bytes ) × blobs          in the order the chunks name them:
//!                                     each file's, in path order, then the pack's
//! ```
//!
//! ⚠️ **What comes back is the account's word, and the agent runs as that
//! account.** [`read_frames`] and [`check`] hold it to the request before
//! anything reaches the store: the number and the turn; every path inside the
//! allowlist and shaped as it says; each blob the one its chunk names, in
//! order, at its length and hash; the sizes within the limits. A member checks
//! the whole of each file again when it assembles it.

// P2b-3b's turn-end trigger is the daemon side's caller; until then only its
// tests run it, and not all of it on every OS. The child side
// (`checkpoint_args`, `checkpoint_main`) is `main`'s. Remove with P2b-3b.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use roomler_hive_node::checkpoint::{
    self, CHUNK_MAX, Checkpoint, Digest, MAX_FILES, MAX_NEW_BYTES, Previous, Skipped,
};
use serde::{Deserialize, Serialize};

use super::workspace::{self, Git};

/// The hidden subcommand: `roomlerd hive-checkpoint <request>`.
pub const CHECKPOINT_SUBCOMMAND: &str = "hive-checkpoint";

/// The first bytes of what the child writes.
const MAGIC: &[u8; 8] = b"HIVECP1\n";

/// The most header bytes read: a manifest of [`MAX_FILES`] files.
const MAX_HEADER: usize = 16 * 1024 * 1024;

/// The most blobs one checkpoint names: every file in at most two chunks,
/// and a pack of [`workspace::MAX_PACK`].
const MAX_BLOBS: usize = 2 * MAX_FILES + workspace::MAX_PACK / CHUNK_MAX + 1;

/// The most bytes the daemon reads from the child at all.
pub(crate) const MAX_OUTPUT: u64 =
    MAX_NEW_BYTES + workspace::MAX_PACK as u64 + (MAX_HEADER as u64) + 16 * 1024 * 1024;

/// What the daemon asks of one checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Request {
    /// Bumped when the fields change their meaning; a child refuses another.
    pub v: u32,
    /// The Hive session id (24 hex): the workspace's ref name.
    pub sid: String,
    /// The harness's session id (a UUID): the config directory's names.
    pub harness_session: String,
    pub config_dir: PathBuf,
    pub folder: PathBuf,
    pub state_dir: PathBuf,
    pub n: u64,
    pub turn: u32,
    /// The last checkpoint, where this one starts; `None` for the first.
    #[serde(default)]
    pub previous: Option<Checkpoint>,
}

pub(crate) const REQUEST_V: u32 = 1;

/// The child's work, as the account: the config directory's allowlist and the
/// workspace, as one checkpoint, with its blobs in the order its chunks name
/// them. A workspace that cannot be taken is skipped, in words.
pub(crate) fn produce(req: &Request, git: &Git) -> Result<(Checkpoint, Vec<Vec<u8>>), String> {
    if req.v != REQUEST_V {
        return Err(format!(
            "a request of version {}; this build reads {REQUEST_V}",
            req.v
        ));
    }
    let prev = req
        .previous
        .as_ref()
        .map_or_else(Previous::none, Previous::from_checkpoint);
    let (mut cp, mut blobs) = checkpoint::take(
        &req.config_dir,
        &req.harness_session,
        req.n,
        req.turn,
        &prev,
    )
    .map_err(|e| e.to_string())?;
    match workspace::checkpoint(
        git,
        &req.folder,
        &req.state_dir,
        &req.sid,
        req.n,
        req.turn,
        prev.workspace(),
    ) {
        workspace::Outcome::Taken {
            snap,
            blobs: mut pack,
        } => {
            cp.workspace = Some(snap);
            blobs.append(&mut pack);
        }
        workspace::Outcome::Skipped(why) => cp.skipped.push(Skipped {
            path: "the workspace".into(),
            why,
        }),
    }
    Ok((cp, blobs))
}

#[derive(Serialize, Deserialize)]
struct Header {
    checkpoint: Checkpoint,
}

/// The child's stdout: [`MAGIC`], the header, then the blobs.
pub(crate) fn write_frames(
    out: &mut impl Write,
    cp: &Checkpoint,
    blobs: &[Vec<u8>],
) -> std::io::Result<()> {
    let header = serde_json::to_vec(&Header {
        checkpoint: cp.clone(),
    })
    .map_err(std::io::Error::other)?;
    out.write_all(MAGIC)?;
    out.write_all(
        &u32::try_from(header.len())
            .map_err(std::io::Error::other)?
            .to_le_bytes(),
    )?;
    out.write_all(&header)?;
    out.write_all(
        &u32::try_from(blobs.len())
            .map_err(std::io::Error::other)?
            .to_le_bytes(),
    )?;
    for blob in blobs {
        out.write_all(&(blob.len() as u64).to_le_bytes())?;
        out.write_all(blob)?;
    }
    out.flush()
}

/// What the child wrote, read back within the limits; [`check`] then holds it
/// to the request.
pub(crate) fn read_frames(bytes: &[u8]) -> Result<(Checkpoint, Vec<Vec<u8>>), String> {
    let mut at = 0usize;
    let mut take = |n: usize, what: &str| -> Result<&[u8], String> {
        let end = at
            .checked_add(n)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| format!("the checkpoint ends inside {what}"))?;
        let s = &bytes[at..end];
        at = end;
        Ok(s)
    };
    if take(MAGIC.len(), "its magic")? != MAGIC {
        return Err("not a checkpoint (no magic)".into());
    }
    let header_len =
        u32::from_le_bytes(take(4, "the header's length")?.try_into().unwrap()) as usize;
    if header_len > MAX_HEADER {
        return Err(format!("a header of {header_len} bytes"));
    }
    let header: Header = serde_json::from_slice(take(header_len, "the header")?)
        .map_err(|e| format!("the header: {e}"))?;
    let count = u32::from_le_bytes(take(4, "the blob count")?.try_into().unwrap()) as usize;
    if count > MAX_BLOBS {
        return Err(format!("{count} blobs"));
    }
    let mut blobs = Vec::with_capacity(count.min(1024));
    for i in 0..count {
        let len = u64::from_le_bytes(take(8, "a blob's length")?.try_into().unwrap());
        if len > CHUNK_MAX as u64 {
            return Err(format!("blob {i} is {len} bytes"));
        }
        blobs.push(take(len as usize, "a blob")?.to_vec());
    }
    if at != bytes.len() {
        return Err(format!("{} bytes after the last blob", bytes.len() - at));
    }
    Ok((header.checkpoint, blobs))
}

/// Hold what the child sent to the request: the number and turn; every path
/// inside the allowlist and shaped as it says; the blobs exactly the ones the
/// chunks name, in order; the workspace's names git's.
pub(crate) fn check(req: &Request, cp: &Checkpoint, blobs: &[Vec<u8>]) -> Result<(), String> {
    if cp.n != req.n || cp.turn != req.turn {
        return Err(format!(
            "checkpoint {} after turn {}, asked for {} after {}",
            cp.n, cp.turn, req.n, req.turn
        ));
    }
    let allow = checkpoint::allowlist(&req.harness_session).map_err(|e| e.to_string())?;
    let mut new_bytes = 0u64;
    let mut next = blobs.iter();
    let mut last_path: Option<&str> = None;
    for f in &cp.files {
        if last_path.is_some_and(|p| p >= f.path.as_str()) {
            return Err(format!("{} is out of order", f.path));
        }
        last_path = Some(&f.path);
        if !allowed(&f.path, &allow) {
            return Err(format!("{} is not in the allowlist", f.path));
        }
        if f.growth != checkpoint::growth_of(&f.path, &req.harness_session) {
            return Err(format!(
                "{} is not taken the way the allowlist says",
                f.path
            ));
        }
        if f.mode > 0o777 {
            return Err(format!("{} has mode {:o}", f.path, f.mode));
        }
        for chunk in &f.chunks {
            let blob = next
                .next()
                .ok_or_else(|| format!("{} names a blob that is missing", f.path))?;
            if blob.len() as u64 != chunk.len || Digest::of(blob) != chunk.hash {
                return Err(format!("{}: a blob is not the one its chunk names", f.path));
            }
            new_bytes += chunk.len;
        }
    }
    if let Some(ws) = &cp.workspace {
        let is_id =
            |s: &str| (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit());
        let ids_ok = is_id(&ws.tree)
            && is_id(&ws.commit)
            && ws.base.as_deref().is_none_or(is_id)
            && ws.head.as_deref().is_none_or(is_id);
        let prefix_ok = ws.prefix.is_empty()
            || (ws.prefix.ends_with('/')
                && !ws.prefix.starts_with('/')
                && ws
                    .prefix
                    .split('/')
                    .all(|c| c != ".." && c != "." && !c.contains('\\')));
        if !ids_ok || !prefix_ok {
            return Err("the workspace names something that is not git's".into());
        }
        let mut offset = 0u64;
        for chunk in &ws.pack {
            let blob = next
                .next()
                .ok_or("the workspace names a blob that is missing")?;
            if chunk.offset != offset
                || blob.len() as u64 != chunk.len
                || Digest::of(blob) != chunk.hash
            {
                return Err("the workspace's pack is not the blobs it names".into());
            }
            offset += chunk.len;
        }
        if offset > workspace::MAX_PACK as u64 {
            return Err("the workspace's pack is over its limit".into());
        }
    }
    if next.next().is_some() {
        return Err("more blobs than the checkpoint names".into());
    }
    if new_bytes > MAX_NEW_BYTES {
        return Err(format!("{new_bytes} new bytes"));
    }
    Ok(())
}

/// Whether `path` is one of the allowlist's files, or a file under one of its
/// directories, with nothing in it that could step outside.
fn allowed(path: &str, allow: &[checkpoint::AllowEntry]) -> bool {
    let shaped = !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..");
    shaped
        && allow.iter().any(|e| {
            if e.is_dir {
                path.strip_prefix(&e.rel)
                    .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1)
            } else {
                path == e.rel
            }
        })
}

/// How long a checkpoint may run before the daemon ends it: a first one of a
/// large workspace hashes and packs all of it.
pub(crate) const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(120);

/// The daemon's side on Unix: start `exe hive-checkpoint <request_path>` as
/// `account` (none in tests: as the daemon), with `env` and nothing of the
/// daemon's own, and hold what it writes to `req`.
#[cfg(unix)]
pub(crate) async fn run_as(
    exe: &Path,
    request_path: &Path,
    req: &Request,
    account: Option<&str>,
    env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    timeout: Duration,
) -> Result<(Checkpoint, Vec<Vec<u8>>), String> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg(CHECKPOINT_SUBCOMMAND)
        .arg(request_path)
        .env_clear()
        .envs(env)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Its own group, so a timeout reaches the git it started.
        .process_group(0);
    if let Some(account) = account {
        crate::exec::apply_session_run_as(&mut cmd, account)?;
    }
    let out = read_child(cmd, timeout, MAX_OUTPUT).await?;
    let (cp, blobs) = read_frames(&out)?;
    check(req, &cp, &blobs)?;
    Ok((cp, blobs))
}

/// Run `cmd` and answer its stdout, at most `max` bytes and within `timeout`;
/// a child that fails says why in its stderr's tail.
#[cfg(unix)]
async fn read_child(
    mut cmd: tokio::process::Command,
    timeout: Duration,
    max: u64,
) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("starting {CHECKPOINT_SUBCOMMAND}: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take();
    let said = tokio::spawn(async move {
        let mut s = Vec::new();
        if let Some(e) = stderr {
            let _ = e.take(4096).read_to_end(&mut s).await;
        }
        s
    });
    let done = tokio::time::timeout(timeout, async {
        let mut out = Vec::new();
        let read = (&mut stdout).take(max + 1).read_to_end(&mut out).await;
        let status = child.wait().await;
        (read.map(|_| out), status)
    })
    .await;
    let (out, status) = match done {
        Ok(v) => v,
        Err(_) => {
            let _ = child.start_kill();
            return Err(format!(
                "{CHECKPOINT_SUBCOMMAND} did not finish within {} s",
                timeout.as_secs()
            ));
        }
    };
    let out = out.map_err(|e| format!("reading {CHECKPOINT_SUBCOMMAND}: {e}"))?;
    let status = status.map_err(|e| format!("waiting for {CHECKPOINT_SUBCOMMAND}: {e}"))?;
    if out.len() as u64 > max {
        let _ = child.start_kill();
        return Err(format!(
            "{CHECKPOINT_SUBCOMMAND} wrote more than {max} bytes"
        ));
    }
    if !status.success() {
        let said = said.await.unwrap_or_default();
        let said = String::from_utf8_lossy(&said);
        return Err(format!(
            "{CHECKPOINT_SUBCOMMAND} failed ({status}): {}",
            said.trim()
        ));
    }
    Ok(out)
}

/// The daemon's side on Windows: start `exe hive-checkpoint <request_path>`
/// as the user signed in at the console, `account` naming them as
/// `hive_accounts` does, and hold what it writes to `req`. Blocking: the
/// caller runs it off the runtime's threads.
#[cfg(windows)]
pub(crate) fn run_as_console_user(
    exe: &Path,
    request_path: &Path,
    req: &Request,
    account: &str,
    timeout: Duration,
) -> Result<(Checkpoint, Vec<Vec<u8>>), String> {
    use crate::hive_win;
    let console = hive_win::console_user().map_err(|(_, e)| e)?;
    hive_win::check_mapping(account, &console).map_err(|(_, e)| e)?;
    let line = hive_win::checkpoint_command_line(exe, request_path)?;
    // SAFETY: `console` holds the token `spawn_as` borrows, across the call.
    let out = unsafe { hive_win::run_checkpoint(console.spawn_as(), &line, timeout, MAX_OUTPUT) };
    drop(console);
    let out = out?;
    let (cp, blobs) = read_frames(&out)?;
    check(req, &cp, &blobs)?;
    Ok((cp, blobs))
}

/// This process's own request, when it was started as `roomlerd
/// hive-checkpoint <request>`.
pub fn checkpoint_args() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != CHECKPOINT_SUBCOMMAND {
        return None;
    }
    let request = args.next()?;
    args.next().is_none().then(|| PathBuf::from(request))
}

/// The child: read the request, take the checkpoint as whoever this process
/// runs as (the session's account), write it to stdout. 0 on success; 1 with
/// a word on stderr otherwise.
pub fn checkpoint_main(request: &Path) -> i32 {
    let run = || -> Result<(), String> {
        let text =
            std::fs::read(request).map_err(|e| format!("reading {}: {e}", request.display()))?;
        if text.len() > MAX_HEADER {
            return Err("the request is too large".into());
        }
        let req: Request =
            serde_json::from_slice(&text).map_err(|e| format!("the request: {e}"))?;
        let (cp, blobs) = produce(&req, &Git::on_path())?;
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        write_frames(&mut out, &cp, &blobs).map_err(|e| format!("writing the checkpoint: {e}"))
    };
    match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("roomlerd {CHECKPOINT_SUBCOMMAND}: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HID: &str = "0b1c2d3e-4f50-4617-8293-a4b5c6d7e8f9";
    const SID: &str = "6aca8ef5dac84dd57e492eb7";

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn request(tmp: &Path, n: u64, previous: Option<Checkpoint>) -> Request {
        Request {
            v: REQUEST_V,
            sid: SID.into(),
            harness_session: HID.into(),
            config_dir: tmp.join("claude"),
            folder: tmp.join("work"),
            state_dir: tmp.join("state"),
            n,
            turn: n as u32,
            previous,
        }
    }

    fn session(tmp: &Path) {
        write(
            &tmp.join("claude"),
            &format!("projects/hive-{HID}/{HID}.jsonl"),
            "{\"t\":1}\n",
        );
        write(&tmp.join("claude"), "CLAUDE.md", "# rules\n");
        write(&tmp.join("work"), "main.rs", "fn main() {}\n");
    }

    /// The child's output, read back and held to the request, is what it
    /// took; a second checkpoint carries only what changed, and both resolve.
    #[test]
    fn a_checkpoint_round_trips_through_the_frames_and_holds_to_its_request() {
        let tmp = tempfile::tempdir().unwrap();
        session(tmp.path());
        let req = request(tmp.path(), 1, None);
        let (cp, blobs) = produce(&req, &Git::under(tmp.path())).unwrap();
        let mut bytes = Vec::new();
        write_frames(&mut bytes, &cp, &blobs).unwrap();
        let (back, back_blobs) = read_frames(&bytes).unwrap();
        assert_eq!(back, cp);
        assert_eq!(back_blobs, blobs);
        check(&req, &back, &back_blobs).unwrap();
        assert_eq!(cp.files.len(), 2);

        write(
            &tmp.path().join("claude"),
            &format!("projects/hive-{HID}/{HID}.jsonl"),
            "{\"t\":1}\n{\"t\":2}\n",
        );
        let req2 = request(tmp.path(), 2, Some(cp));
        let (cp2, blobs2) = produce(&req2, &Git::under(tmp.path())).unwrap();
        check(&req2, &cp2, &blobs2).unwrap();
        let history = cp2
            .files
            .iter()
            .find(|f| f.path.ends_with(".jsonl"))
            .unwrap();
        assert_eq!(history.chunks[0].offset, 8, "only the new line");
    }

    /// Each kind of lie the account could tell is refused before the store
    /// sees it.
    #[test]
    fn what_does_not_hold_to_the_request_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        session(tmp.path());
        let req = request(tmp.path(), 1, None);
        let (cp, blobs) = produce(&req, &Git::under(tmp.path())).unwrap();
        check(&req, &cp, &blobs).unwrap();

        let refused = |cp: &Checkpoint, blobs: &[Vec<u8>], needle: &str| {
            let e = check(&req, cp, blobs).unwrap_err();
            assert!(e.contains(needle), "{e}");
        };
        let mut other = cp.clone();
        other.n = 9;
        refused(&other, &blobs, "asked for 1");

        let mut outside = cp.clone();
        outside.files[0].path = "../.ssh/id_ed25519".into();
        refused(&outside, &blobs, "not in the allowlist");
        let mut root = cp.clone();
        root.files[0].path = "sessions/1.key".into();
        refused(&root, &blobs, "not in the allowlist");
        let mut dotdot = cp.clone();
        dotdot.files[0].path = format!("projects/hive-{HID}/memory/../../../x");
        refused(&dotdot, &blobs, "not in the allowlist");

        let mut swapped = blobs.clone();
        swapped[0] = b"something else\n".to_vec();
        refused(&cp, &swapped, "not the one its chunk names");
        let mut extra = blobs.clone();
        extra.push(b"more".to_vec());
        refused(&cp, &extra, "more blobs");
        refused(&cp, &blobs[..blobs.len() - 1], "missing");

        let mut ws = cp.clone();
        if let Some(w) = ws.workspace.as_mut() {
            w.prefix = "../".into();
            refused(&ws, &blobs, "not git's");
        }
    }

    /// Bytes that are not a whole checkpoint are refused, never half read.
    #[test]
    fn truncated_or_padded_frames_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        session(tmp.path());
        let req = request(tmp.path(), 1, None);
        let (cp, blobs) = produce(&req, &Git::under(tmp.path())).unwrap();
        let mut bytes = Vec::new();
        write_frames(&mut bytes, &cp, &blobs).unwrap();
        assert!(read_frames(&bytes[..bytes.len() - 1]).is_err());
        let mut padded = bytes.clone();
        padded.push(0);
        assert!(
            read_frames(&padded)
                .unwrap_err()
                .contains("after the last blob")
        );
        assert!(read_frames(b"HIVECP2\n").unwrap_err().contains("magic"));
        assert!(read_frames(b"").is_err());
    }

    /// The daemon's side, through a real child: what it writes is read,
    /// checked and answered; a child that writes something else, fails, or
    /// does not finish is refused in words.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_daemon_reads_and_checks_what_a_real_child_writes() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        session(tmp.path());
        let req = request(tmp.path(), 1, None);
        let (cp, blobs) = produce(&req, &Git::under(tmp.path())).unwrap();
        let frames = tmp.path().join("frames.bin");
        let mut bytes = Vec::new();
        write_frames(&mut bytes, &cp, &blobs).unwrap();
        std::fs::write(&frames, &bytes).unwrap();
        let child = |name: &str, body: &str| {
            let p = tmp.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let request_path = tmp.path().join("checkpoint.json");
        let path = vec![("PATH".into(), "/usr/bin:/bin".into())];
        let fast = Duration::from_secs(10);

        let good = child(
            "good.sh",
            &format!(
                "[ \"$1\" = {CHECKPOINT_SUBCOMMAND} ] && cat {}",
                frames.display()
            ),
        );
        let (got, got_blobs) = run_as(&good, &request_path, &req, None, path.clone(), fast)
            .await
            .unwrap();
        assert_eq!((got, got_blobs), (cp.clone(), blobs.clone()));

        let lies = child("lies.sh", "printf 'HIVECP1\\n'");
        assert!(
            run_as(&lies, &request_path, &req, None, path.clone(), fast)
                .await
                .is_err()
        );
        let fails = child("fails.sh", "echo no such folder >&2; exit 3");
        let e = run_as(&fails, &request_path, &req, None, path.clone(), fast)
            .await
            .unwrap_err();
        assert!(e.contains("no such folder"), "{e}");
        let hangs = child("hangs.sh", "sleep 30");
        let e = run_as(
            &hangs,
            &request_path,
            &req,
            None,
            path,
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(e.contains("did not finish"), "{e}");
    }

    /// A request of another version is refused, not read by guesswork.
    #[test]
    fn a_request_of_another_version_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut req = request(tmp.path(), 1, None);
        req.v = 2;
        assert!(
            produce(&req, &Git::under(tmp.path()))
                .unwrap_err()
                .contains("version 2")
        );
    }
}

// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-147 P1 — `roomlerd one-row-home <verb>`: the one process of a person's
//! that `sudo roomlerd one-row` starts to do what has to change inside that
//! person's home (`docs/fr/FR-147-root-out-of-homes-macos.md`, "Root and the
//! person's child"). What it makes there is born the person's, and root's own
//! records stay in root's tree whatever happens in a home.
//!
//! Root starts one per person, with the person's uid, their primary group and
//! no other group, an empty environment, `/` as its folder and its three pipes
//! only. It is not in `--help`. It answers from the raw argv as the first
//! statement of both `main`s ([`answer_if_asked`]), before any runtime, log
//! file, window or folder move: it starts no daemon, and its stderr is its log.
//!
//! | Verb | stdin | The reply's answer | Exit |
//! |---|---|---|---|
//! | `read` | none | `read`: the kind, owner and mode of the home (whether it is a plain absolute path, its device and inode), of `~/Library`, `~/Library/LaunchAgents` and the roomler folder (each also whether it can write there); the folder's segment; the config's and the LaunchAgent plist's bytes, owner and mode, or why not (a link reads as `not_regular`); the kind of what is at each name an older release kept a copy under | 0 |
//! | `convert --expect-config <sha256\|none> --expect-plist <sha256\|none>` | none | `convert`: the new config's sha256 (of the bytes it wrote), whether the plist was removed | 0; 3 when either live file is not the bytes named, or is no longer a regular file (both are checked before either changes) |
//! | `put-back` | [`Kept`]: the kept bytes and modes of both files | `put_back`: the sha256 of each file written | 0; 4 when a folder it writes is not writable |
//! | `put-back-legacy` | none | `put_back_legacy`: which old copies it renamed back | 0; 4 as `put-back` |
//! | every verb | | | 2: usage, a failed check of itself, or another version; 4: not writable (stderr: the path, its owner and mode); 1: any other error |
//!
//! The reply is one JSON line on stdout, written only with exit 0: `"v":1`,
//! `version`, and the `uid`, `gid` and `groups` the child read of itself, with
//! the verb's answer under the verb's own name. Every type is parsed with
//! unknown fields refused ([`Reply::parse`] is the parent's reading of it).
//!
//! **The child's check of itself**, in `run`, before it reads its verb's
//! arguments or any file; each failure is exit 2:
//!
//! 1. `umask(0o077)`: what it makes is born private.
//! 2. It reads its real and effective uid and gid, and the kernel's group list.
//! 3. Real uid = effective uid = `--uid` ≠ 0, real gid = effective gid =
//!    `--gid` ≠ 0, and the group list is exactly `[--gid]`.
//! 4. `setuid(0)` and `setgid(0)` both fail with `EPERM`.
//! 5. `--version` is its own version ([`VERSION`]).
//! 6. The account database names a home for its uid. That is its home: never
//!    an environment variable, never an argument. Nothing in any build relaxes
//!    a check: the child reads no environment at all.
//! 7. It runs the verb.
//!
//! **Bounds.** A config of [`CONFIG_BOUND`], a plist of [`PLIST_BOUND`], stdin
//! of [`STDIN_BOUND`]; anything larger is exit 1 (`read` reports it as
//! `too_large`), and so is a new config `convert` would write over its bound,
//! before anything changes. A read opens with `O_NONBLOCK` and `O_NOFOLLOW`, checks on the
//! open descriptor that it is a regular file, and reads at most the bound and
//! one byte: a FIFO, a device or a huge file is an error, never a wait. A
//! write is a new temporary file in the same folder, `fsync`, its mode set,
//! `rename`, as the daemon saves its config. `convert` writes only from the
//! bytes it checked, and reads no file again by path: the new config is the
//! text a save would write over them (`config::serialise_over`, which keeps
//! the keys this build does not know), and they are kept as the config's
//! `.prev` (0600), as a save keeps the outgoing file. `put-back` gives the
//! plist its recorded mode less group and other write (0644 when none was
//! recorded: launchd ignores a plist others can write) and the config its
//! recorded owner bits (0600 when none), and makes a missing
//! `~/Library/LaunchAgents` 0755.
//!
//! The logic is pure over [`Me`] and [`Fs`], so it runs in tests on every
//! platform; [`RealMe`] and [`RealFs`] are the thin Unix effects.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::one_row::{AGENT_PLIST, kept_path, without_enrollments};

/// The verb root starts the child with: `roomlerd one-row-home <verb> …`.
pub const VERB: &str = "one-row-home";
/// The version the child answers for, and the one root passes in `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The format of the reply and of `put-back`'s stdin.
pub const FORMAT: u32 = 1;
/// The largest config the child reads or writes.
pub const CONFIG_BOUND: usize = 256 * 1024;
/// The largest LaunchAgent plist the child reads or writes.
pub const PLIST_BOUND: usize = 64 * 1024;
/// The most stdin the child reads (`put-back`).
pub const STDIN_BOUND: usize = 1024 * 1024;
/// The config's name in the roomler folder.
const CONFIG: &str = "config.toml";

/// How the child ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// 0: done, and the reply is on stdout.
    Done,
    /// 1: any other error.
    Error,
    /// 2: usage, a failed check of itself, or another version.
    Refused,
    /// 3: a live file is not the bytes root named.
    Changed,
    /// 4: a folder it has to write is not writable.
    NotWritable,
}

impl Exit {
    pub const fn code(self) -> i32 {
        match self {
            Exit::Done => 0,
            Exit::Error => 1,
            Exit::Refused => 2,
            Exit::Changed => 3,
            Exit::NotWritable => 4,
        }
    }

    pub const fn from_code(code: i32) -> Option<Exit> {
        Some(match code {
            0 => Exit::Done,
            1 => Exit::Error,
            2 => Exit::Refused,
            3 => Exit::Changed,
            4 => Exit::NotWritable,
            _ => return None,
        })
    }
}

/// What `convert` expects a live file to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// Not there (`none`).
    Absent,
    /// These bytes, by their sha256 (64 lowercase hex digits).
    Sha256([u8; 32]),
}

impl Expect {
    /// What root saw: the bytes it kept, or none.
    pub fn of(bytes: Option<&[u8]>) -> Expect {
        match bytes {
            Some(bytes) => Expect::Sha256(Sha256::digest(bytes).into()),
            None => Expect::Absent,
        }
    }

    fn word(self) -> String {
        match self {
            Expect::Absent => "none".to_string(),
            Expect::Sha256(h) => hex::encode(h),
        }
    }

    fn parse(word: &str) -> Result<Expect, String> {
        if word == "none" {
            return Ok(Expect::Absent);
        }
        let lower_hex = word.len() == 64
            && word
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let mut h = [0u8; 32];
        if lower_hex && hex::decode_to_slice(word, &mut h).is_ok() {
            Ok(Expect::Sha256(h))
        } else {
            Err("an expectation is `none` or 64 lowercase hex digits".to_string())
        }
    }
}

/// The four verbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Read,
    Convert { config: Expect, plist: Expect },
    PutBack,
    PutBackLegacy,
}

impl Verb {
    pub fn word(self) -> &'static str {
        match self {
            Verb::Read => "read",
            Verb::Convert { .. } => "convert",
            Verb::PutBack => "put-back",
            Verb::PutBackLegacy => "put-back-legacy",
        }
    }
}

/// The arguments root starts the child with, after the program:
/// `one-row-home <verb> --uid <uid> --gid <gid> --version <VERSION>` and the
/// verb's own.
pub fn child_args(uid: u32, gid: u32, verb: Verb) -> Vec<String> {
    let mut args = vec![
        VERB.to_string(),
        verb.word().to_string(),
        "--uid".to_string(),
        uid.to_string(),
        "--gid".to_string(),
        gid.to_string(),
        "--version".to_string(),
        VERSION.to_string(),
    ];
    if let Verb::Convert { config, plist } = verb {
        args.extend([
            "--expect-config".to_string(),
            config.word(),
            "--expect-plist".to_string(),
            plist.word(),
        ]);
    }
    args
}

/// The account root asked for, and the version it was started by.
struct Asked {
    uid: u32,
    gid: u32,
    version: String,
}

/// `--name value` pairs, each name at most once; `args` is everything after
/// the verb's word.
fn flags(args: &[String]) -> Result<Vec<(&str, &str)>, String> {
    if !args.len().is_multiple_of(2) {
        return Err("every flag takes one value".to_string());
    }
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    for pair in args.chunks(2) {
        let name = pair[0]
            .strip_prefix("--")
            .filter(|n| !n.is_empty())
            .ok_or_else(|| format!("`{}` is not a flag", pair[0]))?;
        if pairs.iter().any(|(n, _)| *n == name) {
            return Err(format!("--{name} is given twice"));
        }
        pairs.push((name, pair[1].as_str()));
    }
    Ok(pairs)
}

fn flag<'a>(pairs: &[(&str, &'a str)], name: &str) -> Result<&'a str, String> {
    pairs
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| *v)
        .ok_or_else(|| format!("--{name} is missing"))
}

/// A uid or gid in plain decimal: no sign, no space.
fn id(word: &str) -> Result<u32, String> {
    let digits = !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit());
    digits
        .then(|| word.parse().ok())
        .flatten()
        .ok_or_else(|| format!("`{word}` is not an id"))
}

/// Step 3's arguments: `--uid`, `--gid` and `--version`. `args` starts with
/// the verb's word.
fn asked(args: &[String]) -> Result<Asked, String> {
    let pairs = flags(args.get(1..).unwrap_or_default())?;
    Ok(Asked {
        uid: id(flag(&pairs, "uid")?)?,
        gid: id(flag(&pairs, "gid")?)?,
        version: flag(&pairs, "version")?.to_string(),
    })
}

/// Step 7's arguments: the verb's word and its own flags; no flag a verb
/// does not take.
fn verb(args: &[String]) -> Result<Verb, String> {
    let word = args.first().map(String::as_str).unwrap_or_default();
    let pairs = flags(args.get(1..).unwrap_or_default())?;
    let (verb, own): (Verb, &[&str]) = match word {
        "read" => (Verb::Read, &[]),
        "convert" => (
            Verb::Convert {
                config: Expect::parse(flag(&pairs, "expect-config")?)?,
                plist: Expect::parse(flag(&pairs, "expect-plist")?)?,
            },
            &["expect-config", "expect-plist"],
        ),
        "put-back" => (Verb::PutBack, &[]),
        "put-back-legacy" => (Verb::PutBackLegacy, &[]),
        _ => return Err(format!("`{word}` is not a verb of {VERB}")),
    };
    for (name, _) in &pairs {
        if !["uid", "gid", "version"].contains(name) && !own.contains(name) {
            return Err(format!("{} takes no --{name}", verb.word()));
        }
    }
    Ok(verb)
}

/// What the kernel says this process is (step 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Creds {
    pub uid: u32,
    pub euid: u32,
    pub gid: u32,
    pub egid: u32,
    /// The kernel's group list.
    pub groups: Vec<u32>,
}

/// Step 3: why this process is not exactly the account root named, with its
/// own group alone; `None` when it is.
fn identity_refusal(me: &Creds, uid: u32, gid: u32) -> Option<String> {
    if uid == 0 || gid == 0 {
        return Some(format!("asked for uid {uid} gid {gid}: never 0"));
    }
    let is = format!(
        "uid {} (effective {}), gid {} (effective {}), groups {:?}",
        me.uid, me.euid, me.gid, me.egid, me.groups
    );
    let exact =
        me.uid == uid && me.euid == uid && me.gid == gid && me.egid == gid && me.groups == [gid];
    (!exact).then(|| format!("it runs as {is}, not uid {uid} gid {gid} groups [{gid}]"))
}

/// The process the child is: its credentials, stdin, and the account
/// database.
pub trait Me {
    /// `umask(0o077)`.
    fn umask_private(&mut self);
    /// Real and effective uid and gid, and the kernel's group list.
    fn creds(&self) -> io::Result<Creds>;
    /// Whether `setuid(0)` and `setgid(0)` both fail with `EPERM`.
    fn cannot_become_root(&mut self) -> bool;
    /// The account database's home for `uid`.
    fn home_of(&self, uid: u32) -> Option<PathBuf>;
    /// stdin to its end, at most `bound` bytes; `None` when there is more.
    fn stdin(&mut self, bound: usize) -> io::Result<Option<Vec<u8>>>;
}

/// What is at a path, as `lstat` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Dir,
    File,
    Link,
    /// Anything else, or a path that could not be looked at (no owner, no
    /// mode).
    Other,
    Absent,
}

/// `lstat` of a path: its kind, and its owner and mode when it is there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    pub uid: Option<u32>,
    pub mode: Option<u32>,
}

impl Stat {
    pub const ABSENT: Stat = Stat {
        kind: Kind::Absent,
        uid: None,
        mode: None,
    };
}

/// Why a file was not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadError {
    Eacces,
    TooLarge,
    NotRegular,
    Other,
}

/// One file as the child read it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Held {
    Absent,
    Present {
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
        uid: u32,
        mode: u32,
    },
    Unreadable {
        error: ReadError,
        uid: Option<u32>,
        mode: Option<u32>,
    },
}

/// Never the bytes: a config holds an enrollment's token and key.
impl std::fmt::Debug for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Held::Absent => f.write_str("Absent"),
            Held::Present { bytes, uid, mode } => write!(
                f,
                "Present {{ {} bytes, uid: {uid}, mode: {mode:o} }}",
                bytes.len()
            ),
            Held::Unreadable { error, uid, mode } => write!(
                f,
                "Unreadable {{ {error:?}, uid: {uid:?}, mode: {:?} }}",
                mode.map(|m| format!("{m:o}"))
            ),
        }
    }
}

/// The files inside the home, as the person's process reaches them.
pub trait Fs {
    /// `lstat`.
    fn lstat(&self, path: &Path) -> Stat;
    /// `access(W_OK)`.
    fn writable(&self, path: &Path) -> bool;
    /// `stat`, following links: device and inode; `None` when nothing is there.
    fn dev_ino(&self, path: &Path) -> Option<(u64, u64)>;
    /// At most `bound` bytes of a regular file, never a wait.
    fn read(&self, path: &Path, bound: usize) -> Held;
    /// A new file in the same folder, `fsync`, `mode`, `rename` over `path`.
    fn write(&mut self, path: &Path, bytes: &[u8], mode: u32) -> io::Result<()>;
    fn remove(&mut self, path: &Path) -> io::Result<()>;
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()>;
    /// `mkdir`, then `mode` (the umask took group and other bits off).
    fn make_dir(&mut self, path: &Path, mode: u32) -> io::Result<()>;
}

/// The reply: one JSON object on stdout, with exit 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub v: u32,
    pub version: String,
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<ReadAnswer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convert: Option<ConvertAnswer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub put_back: Option<PutBackAnswer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub put_back_legacy: Option<Legacy>,
}

impl Reply {
    /// How the parent reads a reply: one object with nothing unknown in it,
    /// of this format and version, from the account it started (`uid`, its
    /// `pw_gid` and that group alone), answering `verb` and nothing else.
    /// The error quotes nothing of the reply.
    pub fn parse(stdout: &[u8], uid: u32, gid: u32, verb: Verb) -> Result<Reply, String> {
        let reply: Reply = serde_json::from_slice(stdout).map_err(|e| {
            format!(
                "the reply does not parse ({:?} at line {}, column {})",
                e.classify(),
                e.line(),
                e.column()
            )
        })?;
        if reply.v != FORMAT || reply.version != VERSION {
            return Err("the reply is of another format or version".to_string());
        }
        if reply.uid != uid || reply.gid != gid || reply.groups != [gid] {
            return Err(format!(
                "the reply is from uid {} gid {} groups {:?}, not uid {uid} gid {gid} groups [{gid}]",
                reply.uid, reply.gid, reply.groups
            ));
        }
        let answers = [
            (reply.read.is_some(), matches!(verb, Verb::Read)),
            (
                reply.convert.is_some(),
                matches!(verb, Verb::Convert { .. }),
            ),
            (reply.put_back.is_some(), matches!(verb, Verb::PutBack)),
            (
                reply.put_back_legacy.is_some(),
                matches!(verb, Verb::PutBackLegacy),
            ),
        ];
        if answers.iter().any(|(has, wanted)| has != wanted) {
            return Err(format!("the reply does not answer `{}` alone", verb.word()));
        }
        Ok(reply)
    }
}

/// `read`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadAnswer {
    pub home: HomeEntry,
    pub library: Entry,
    pub agents_dir: Entry,
    pub folder: Entry,
    pub segment: Segment,
    pub config: Held,
    pub plist: Held,
    pub legacy: LegacyEntries,
}

/// The home itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeEntry {
    pub kind: Kind,
    pub uid: Option<u32>,
    pub mode: Option<u32>,
    /// It matches `/*[!/]`: absolute, and not ending in `/`.
    pub absolute: bool,
    pub dev: Option<u64>,
    pub ino: Option<u64>,
}

/// A folder in the home, or a name an old copy is kept under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub kind: Kind,
    pub uid: Option<u32>,
    pub mode: Option<u32>,
    /// `access(W_OK)` as the person; false when it is not there.
    pub writable: bool,
}

/// Which name the roomler folder has: the current one, or the pre-rename one
/// on a Mac that has only that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Segment {
    Current,
    Legacy,
}

/// What `lstat` finds at each name an older release kept its copies under in
/// the roomler folder (`config.toml.pre-one-row`,
/// `com.roomler.agent.plist.pre-one-row`), reported as a folder is: kind,
/// owner, mode and whether the person can write it. `absent` when there is
/// none; `other` with no owner and no mode when it could not be looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyEntries {
    pub config: Entry,
    pub plist: Entry,
}

/// `put-back-legacy`'s answer: which of those copies it renamed back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Legacy {
    pub config: bool,
    pub plist: bool,
}

/// `convert`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvertAnswer {
    /// The sha256 of the config it saved; `None` when there was none.
    pub config_sha256: Option<String>,
    pub plist_removed: bool,
}

/// `put-back`'s answer: the sha256 of each file it wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutBackAnswer {
    pub config_sha256: Option<String>,
    pub plist_sha256: Option<String>,
}

/// `put-back`'s stdin: what root kept of each file, and its mode then.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kept {
    pub v: u32,
    #[serde(with = "base64_opt")]
    pub config: Option<Vec<u8>>,
    pub config_mode: Option<u32>,
    #[serde(with = "base64_opt")]
    pub plist: Option<Vec<u8>>,
    pub plist_mode: Option<u32>,
}

/// Never the bytes, as [`Held`].
impl std::fmt::Debug for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let octal = |m: Option<u32>| m.map(|m| format!("{m:o}"));
        write!(
            f,
            "Kept {{ v: {}, config: {:?} bytes, config_mode: {:?}, plist: {:?} bytes, plist_mode: {:?} }}",
            self.v,
            self.config.as_ref().map(Vec::len),
            octal(self.config_mode),
            self.plist.as_ref().map(Vec::len),
            octal(self.plist_mode)
        )
    }
}

mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    pub fn serialize<S: serde::Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(d)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

mod base64_opt {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    pub fn serialize<S: serde::Serializer>(
        bytes: &Option<Vec<u8>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(bytes) => s.serialize_some(&STANDARD.encode(bytes)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        let text = <Option<String> as serde::Deserialize>::deserialize(d)?;
        text.map(|t| STANDARD.decode(t).map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// What the child writes and how it ends.
pub struct Outcome {
    pub exit: Exit,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Never the bytes, as [`Held`]: `read`'s reply carries the config.
impl std::fmt::Debug for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Outcome {{ exit: {:?}, stdout: {} bytes, stderr: {:?} }}",
            self.exit,
            self.stdout.len(),
            self.stderr
        )
    }
}

/// Why a verb stopped.
#[derive(Debug)]
enum Fail {
    /// 2.
    Usage(String),
    /// 3.
    Changed(String),
    /// 4: the folder, as `lstat` saw it.
    NotWritable(PathBuf, Stat),
    /// 1.
    Error(String),
}

impl Outcome {
    fn refused(why: String) -> Outcome {
        Outcome::failed(Fail::Usage(why))
    }

    fn failed(fail: Fail) -> Outcome {
        let (exit, stderr) = match fail {
            Fail::Usage(why) => (Exit::Refused, format!("refused: {why}")),
            Fail::Changed(why) => (Exit::Changed, format!("changed since it was read: {why}")),
            Fail::NotWritable(path, st) => (
                Exit::NotWritable,
                format!(
                    "not writable: {} (owner {}, mode {})",
                    path.display(),
                    st.uid.map_or("unknown".to_string(), |u| format!("uid {u}")),
                    st.mode
                        .map_or("unknown".to_string(), |m| format!("{m:04o}"))
                ),
            ),
            Fail::Error(why) => (Exit::Error, why),
        };
        Outcome {
            exit,
            stdout: Vec::new(),
            stderr,
        }
    }

    fn reply(me: &Creds, answer: Answer) -> Outcome {
        let mut reply = Reply {
            v: FORMAT,
            version: VERSION.to_string(),
            uid: me.uid,
            gid: me.gid,
            groups: me.groups.clone(),
            read: None,
            convert: None,
            put_back: None,
            put_back_legacy: None,
        };
        match answer {
            Answer::Read(a) => reply.read = Some(*a),
            Answer::Convert(a) => reply.convert = Some(a),
            Answer::PutBack(a) => reply.put_back = Some(a),
            Answer::PutBackLegacy(a) => reply.put_back_legacy = Some(a),
        }
        match serde_json::to_vec(&reply) {
            Ok(mut stdout) => {
                stdout.push(b'\n');
                Outcome {
                    exit: Exit::Done,
                    stdout,
                    stderr: String::new(),
                }
            }
            Err(e) => Outcome::failed(Fail::Error(format!("the reply did not serialise: {e}"))),
        }
    }

    /// `said` after what this says already, on its one stderr line.
    fn saying(mut self, said: &[String]) -> Outcome {
        for note in said {
            if !self.stderr.is_empty() {
                self.stderr.push_str("; ");
            }
            self.stderr.push_str(note);
        }
        self
    }
}

enum Answer {
    Read(Box<ReadAnswer>),
    Convert(ConvertAnswer),
    PutBack(PutBackAnswer),
    PutBackLegacy(Legacy),
}

/// The child: its check of itself, then the verb. `args` is what follows
/// `one-row-home` on the command line. Private: outside this module a verb
/// is reached only through [`answer_if_asked`], after every check, as the
/// process itself (`RealMe`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn run(args: &[OsString], me: &mut impl Me, fs: &mut impl Fs) -> Outcome {
    // 1. What it makes is born private, whoever started it and however.
    me.umask_private();
    // 2. What the kernel says this process is, before any argument is read.
    let creds = match me.creds() {
        Ok(creds) => creds,
        Err(e) => return Outcome::refused(format!("its credentials could not be read: {e}")),
    };
    let Some(args) = args
        .iter()
        .map(|a| a.to_str().map(str::to_owned))
        .collect::<Option<Vec<String>>>()
    else {
        return Outcome::refused("an argument is not UTF-8".to_string());
    };
    // 3. Exactly the account root named, with its own group alone.
    let asked = match asked(&args) {
        Ok(asked) => asked,
        Err(e) => return Outcome::refused(format!("usage: {e}")),
    };
    if let Some(why) = identity_refusal(&creds, asked.uid, asked.gid) {
        return Outcome::refused(why);
    }
    // 4. It cannot get root back.
    if !me.cannot_become_root() {
        return Outcome::refused("setuid(0) or setgid(0) did not fail with EPERM".to_string());
    }
    // 5. The build root is.
    if asked.version != VERSION {
        return Outcome::refused(format!(
            "started for version {}, and this is {VERSION}",
            asked.version
        ));
    }
    // 6. The account database's home for its own uid.
    let Some(home) = me.home_of(creds.uid) else {
        return Outcome::refused(format!(
            "the account database names no home for uid {}",
            creds.uid
        ));
    };
    // 7. The verb.
    let verb = match verb(&args) {
        Ok(verb) => verb,
        Err(e) => return Outcome::refused(format!("usage: {e}")),
    };
    let stdin = match verb {
        Verb::PutBack => match me.stdin(STDIN_BOUND) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                return Outcome::failed(Fail::Error(format!(
                    "its input is over {STDIN_BOUND} bytes"
                )));
            }
            Err(e) => return Outcome::failed(Fail::Error(format!("reading its input: {e}"))),
        },
        _ => Vec::new(),
    };
    answer(verb, &home, &creds, &stdin, fs)
}

/// The verb, below the checks. `home` is the account database's home for
/// `me.uid`, `stdin` what root sent. Private, as [`run`] is: the tests here
/// pass a home of their own.
fn answer(verb: Verb, home: &Path, me: &Creds, stdin: &[u8], fs: &mut impl Fs) -> Outcome {
    let at = Places::of(home, &*fs);
    // What a verb that ends well still says: the child has no log.
    let mut said = Vec::new();
    let answered = match verb {
        Verb::Read => Ok(Answer::Read(Box::new(read(&at, &*fs)))),
        Verb::Convert { config, plist } => {
            convert(&at, config, plist, fs, &mut said).map(Answer::Convert)
        }
        Verb::PutBack => put_back(&at, stdin, fs).map(Answer::PutBack),
        Verb::PutBackLegacy => put_back_legacy(&at, fs).map(Answer::PutBackLegacy),
    };
    match answered {
        Ok(answer) => Outcome::reply(me, answer).saying(&said),
        Err(fail) => Outcome::failed(fail),
    }
}

/// Every path the child reads or writes, from its home.
struct Places {
    home: PathBuf,
    library: PathBuf,
    /// `~/Library/Application Support`.
    support: PathBuf,
    agents_dir: PathBuf,
    plist: PathBuf,
    /// The roomler folder, named as the person's own agent names it.
    folder: PathBuf,
    segment: Segment,
    config: PathBuf,
    legacy_config: PathBuf,
    legacy_plist: PathBuf,
}

impl Places {
    fn of(home: &Path, fs: &impl Fs) -> Places {
        let library = home.join("Library");
        let agents_dir = library.join("LaunchAgents");
        let current = crate::appdirs::macos_config_dir_in_home(home, |_| false);
        let folder = crate::appdirs::macos_config_dir_in_home(home, |p| fs.dev_ino(p).is_some());
        let segment = if folder == current {
            Segment::Current
        } else {
            Segment::Legacy
        };
        let config = folder.join(CONFIG);
        Places {
            home: home.to_path_buf(),
            support: library.join("Application Support"),
            plist: agents_dir.join(AGENT_PLIST),
            legacy_config: kept_path(&config),
            legacy_plist: kept_path(&folder.join(AGENT_PLIST)),
            library,
            agents_dir,
            folder,
            segment,
            config,
        }
    }

    /// The folders down to `~/Library/LaunchAgents`, each with the mode it
    /// is made with when missing.
    fn to_agents_dir(&self) -> [(&Path, u32); 2] {
        [
            (self.library.as_path(), 0o700),
            (self.agents_dir.as_path(), 0o755),
        ]
    }
}

fn entry(fs: &impl Fs, path: &Path) -> Entry {
    let st = fs.lstat(path);
    Entry {
        kind: st.kind,
        uid: st.uid,
        mode: st.mode,
        writable: st.kind != Kind::Absent && fs.writable(path),
    }
}

/// `/*[!/]`.
fn plain_absolute(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    bytes.len() > 1 && bytes.first() == Some(&b'/') && bytes.last() != Some(&b'/')
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn read(at: &Places, fs: &impl Fs) -> ReadAnswer {
    let home = fs.lstat(&at.home);
    let id = fs.dev_ino(&at.home);
    ReadAnswer {
        home: HomeEntry {
            kind: home.kind,
            uid: home.uid,
            mode: home.mode,
            absolute: plain_absolute(&at.home),
            dev: id.map(|(dev, _)| dev),
            ino: id.map(|(_, ino)| ino),
        },
        library: entry(fs, &at.library),
        agents_dir: entry(fs, &at.agents_dir),
        folder: entry(fs, &at.folder),
        segment: at.segment,
        config: fs.read(&at.config, CONFIG_BOUND),
        plist: fs.read(&at.plist, PLIST_BOUND),
        legacy: LegacyEntries {
            config: entry(fs, &at.legacy_config),
            plist: entry(fs, &at.legacy_plist),
        },
    }
}

/// The live file at `path` against what root named: its bytes when they are
/// those, `None` when it is rightly absent.
fn live(fs: &impl Fs, path: &Path, bound: usize, want: Expect) -> Result<Option<Vec<u8>>, Fail> {
    match want {
        Expect::Absent => {
            let st = fs.lstat(path);
            match (st.kind, st.uid) {
                (Kind::Absent, _) => Ok(None),
                (_, None) => Err(Fail::Error(format!(
                    "{} could not be looked at",
                    path.display()
                ))),
                _ => Err(Fail::Changed(format!(
                    "{} is there, and was not",
                    path.display()
                ))),
            }
        }
        Expect::Sha256(h) => match fs.read(path, bound) {
            Held::Absent => Err(Fail::Changed(format!("{} is gone", path.display()))),
            Held::Present { bytes, .. } if Sha256::digest(&bytes)[..] == h[..] => Ok(Some(bytes)),
            Held::Present { .. } => Err(Fail::Changed(format!(
                "{} is not the bytes it was",
                path.display()
            ))),
            // A link, a FIFO or a folder where the file was: it changed, as
            // something where nothing was is under `none`.
            Held::Unreadable {
                error: ReadError::NotRegular,
                ..
            } => Err(Fail::Changed(format!(
                "{} is no longer a regular file",
                path.display()
            ))),
            unreadable => Err(Fail::Error(format!("{}: {unreadable:?}", path.display()))),
        },
    }
}

fn not_writable(fs: &impl Fs, path: &Path) -> Fail {
    Fail::NotWritable(path.to_path_buf(), fs.lstat(path))
}

/// `convert`: both live files checked against what root named, both folders
/// it changes checked writable, and the new config made from the bytes it
/// checked and held to its bound, before anything changes; then the plist
/// goes, those bytes are kept as the config's `.prev` and the new config is
/// written. The reply's sha256 is of the bytes it wrote: no file is read
/// again by path. The keys of the outgoing config it could not keep, and a
/// `.prev` it could not write, go in `said`.
fn convert(
    at: &Places,
    want_config: Expect,
    want_plist: Expect,
    fs: &mut impl Fs,
    said: &mut Vec<String>,
) -> Result<ConvertAnswer, Fail> {
    let config = live(&*fs, &at.config, CONFIG_BOUND, want_config)?;
    let plist = live(&*fs, &at.plist, PLIST_BOUND, want_plist)?;
    if config.is_some() && !fs.writable(&at.folder) {
        return Err(not_writable(&*fs, &at.folder));
    }
    if plist.is_some() && !fs.writable(&at.agents_dir) {
        return Err(not_writable(&*fs, &at.agents_dir));
    }
    let origin = at.config.display().to_string();
    let blank = match &config {
        Some(bytes) => {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| Fail::Error(format!("{origin} is not UTF-8")))?;
            let cfg = crate::config::parse_text(text, &origin)
                .map_err(|e| Fail::Error(format!("{e:#}")))?;
            let (blank, lost) =
                crate::config::serialise_over(&without_enrollments(&cfg), text, &at.config)
                    .map_err(|e| Fail::Error(format!("{e:#}")))?;
            // Within the bound root reads, before anything changes.
            if blank.len() > CONFIG_BOUND {
                return Err(Fail::Error(format!(
                    "the new {origin} would be over {CONFIG_BOUND} bytes"
                )));
            }
            if !lost.is_empty() {
                said.push(format!(
                    "keys of {origin} this build does not know were not kept: {}",
                    lost.join(", ")
                ));
            }
            Some((bytes, blank))
        }
        None => None,
    };
    let plist_removed = match plist {
        Some(_) => {
            fs.remove(&at.plist)
                .map_err(|e| Fail::Error(format!("removing {}: {e}", at.plist.display())))?;
            true
        }
        None => false,
    };
    let config_sha256 = match blank {
        Some((outgoing, blank)) => {
            // Kept as a save keeps the outgoing file: it parsed, and a copy
            // that cannot be made costs its recovery, not the verb.
            let prev = crate::config::prev_path(&at.config);
            if let Err(e) = fs.write(&prev, outgoing, 0o600) {
                said.push(format!(
                    "the outgoing config was not kept as {}: {e}",
                    prev.display()
                ));
            }
            fs.write(&at.config, blank.as_bytes(), 0o600)
                .map_err(|e| Fail::Error(format!("saving {origin}: {e}")))?;
            Some(sha256_hex(blank.as_bytes()))
        }
        None => None,
    };
    Ok(ConvertAnswer {
        config_sha256,
        plist_removed,
    })
}

/// Each folder of `chain`, outermost first, made with its mode where it is
/// missing; the last one must be writable.
fn make_dirs(fs: &mut impl Fs, chain: &[(&Path, u32)]) -> Result<(), Fail> {
    for (dir, mode) in chain {
        let st = fs.lstat(dir);
        let parent = dir.parent().unwrap_or(dir);
        match st.kind {
            Kind::Dir | Kind::Link => {}
            Kind::Absent => {
                if !fs.writable(parent) {
                    return Err(not_writable(&*fs, parent));
                }
                fs.make_dir(dir, *mode)
                    .map_err(|e| Fail::Error(format!("making {}: {e}", dir.display())))?;
            }
            // It could not be looked at: the folder it is in does not let
            // the person through.
            Kind::Other if st.uid.is_none() => return Err(not_writable(&*fs, parent)),
            Kind::File | Kind::Other => {
                return Err(Fail::Error(format!("{} is not a folder", dir.display())));
            }
        }
    }
    match chain.last() {
        Some((dir, _)) if !fs.writable(dir) => Err(not_writable(&*fs, dir)),
        _ => Ok(()),
    }
}

/// The config's mode: the recorded owner bits, 0600 when none was recorded.
fn config_mode(recorded: Option<u32>) -> u32 {
    recorded.map_or(0o600, |m| m & 0o700)
}

/// The plist's mode: the recorded one less group and other write, 0644 when
/// none was recorded. launchd ignores a plist that group or others can write.
fn plist_mode(recorded: Option<u32>) -> u32 {
    recorded.map_or(0o644, |m| m & 0o755)
}

/// `put-back`: root's kept bytes written back, the config first, whatever
/// the live files hold.
fn put_back(at: &Places, stdin: &[u8], fs: &mut impl Fs) -> Result<PutBackAnswer, Fail> {
    let kept: Kept = serde_json::from_slice(stdin).map_err(|e| {
        Fail::Usage(format!(
            "its input does not parse ({:?} at line {}, column {})",
            e.classify(),
            e.line(),
            e.column()
        ))
    })?;
    if kept.v != FORMAT {
        return Err(Fail::Usage(format!("its input is format {}", kept.v)));
    }
    let over =
        |bytes: &Option<Vec<u8>>, bound: usize| bytes.as_ref().is_some_and(|b| b.len() > bound);
    if over(&kept.config, CONFIG_BOUND) || over(&kept.plist, PLIST_BOUND) {
        return Err(Fail::Error("a kept file is over its bound".to_string()));
    }
    let config_sha256 = match &kept.config {
        Some(bytes) => {
            make_dirs(
                fs,
                &[
                    (at.library.as_path(), 0o700),
                    (at.support.as_path(), 0o755),
                    (at.folder.as_path(), 0o700),
                ],
            )?;
            fs.write(&at.config, bytes, config_mode(kept.config_mode))
                .map_err(|e| Fail::Error(format!("writing {}: {e}", at.config.display())))?;
            Some(sha256_hex(bytes))
        }
        None => None,
    };
    let plist_sha256 = match &kept.plist {
        Some(bytes) => {
            make_dirs(fs, &at.to_agents_dir())?;
            fs.write(&at.plist, bytes, plist_mode(kept.plist_mode))
                .map_err(|e| Fail::Error(format!("writing {}: {e}", at.plist.display())))?;
            Some(sha256_hex(bytes))
        }
        None => None,
    };
    Ok(PutBackAnswer {
        config_sha256,
        plist_sha256,
    })
}

/// `put-back-legacy`: the copies an older release kept in the roomler
/// folder, renamed back, the config first. A rename needs write on the two
/// folders only, whoever owns the file. A kept copy is a file: anything else
/// at either name is refused as changed (3), before anything is renamed.
fn put_back_legacy(at: &Places, fs: &mut impl Fs) -> Result<Legacy, Fail> {
    let config = fs.lstat(&at.legacy_config).kind;
    let plist = fs.lstat(&at.legacy_plist).kind;
    for (kind, path) in [(config, &at.legacy_config), (plist, &at.legacy_plist)] {
        if !matches!(kind, Kind::File | Kind::Absent) {
            return Err(Fail::Changed(format!("{} is not a file", path.display())));
        }
    }
    let mut moved = Legacy {
        config: false,
        plist: false,
    };
    if config == Kind::File {
        if !fs.writable(&at.folder) {
            return Err(not_writable(&*fs, &at.folder));
        }
        fs.rename(&at.legacy_config, &at.config)
            .map_err(|e| Fail::Error(format!("renaming {}: {e}", at.legacy_config.display())))?;
        moved.config = true;
    }
    if plist == Kind::File {
        if !fs.writable(&at.folder) {
            return Err(not_writable(&*fs, &at.folder));
        }
        make_dirs(fs, &at.to_agents_dir())?;
        fs.rename(&at.legacy_plist, &at.plist)
            .map_err(|e| Fail::Error(format!("renaming {}: {e}", at.legacy_plist.display())))?;
        moved.plist = true;
    }
    Ok(moved)
}

/// Answer `roomlerd one-row-home …` and exit; return at once, having read
/// nothing but the argv, for any other command line. The first statement of
/// both `main`s.
#[cfg(target_os = "macos")]
pub fn answer_if_asked() {
    use std::io::Write as _;
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).map(OsString::as_os_str) != Some(std::ffi::OsStr::new(VERB)) {
        return;
    }
    let mut out = run(&args[2..], &mut RealMe, &mut RealFs);
    let mut stdout = io::stdout().lock();
    let written = stdout.write_all(&out.stdout).and_then(|()| stdout.flush());
    drop(stdout);
    if let Err(e) = written {
        // 0 says the reply is on stdout: one that did not get there is not
        // a success.
        if out.exit == Exit::Done {
            out.exit = Exit::Error;
        }
        out = out.saying(&[format!("the reply could not be written: {e}")]);
    }
    if !out.stderr.is_empty() {
        let _ = writeln!(io::stderr(), "{VERB}: {}", out.stderr);
    }
    std::process::exit(out.exit.code());
}

/// The child exists on macOS only: elsewhere every command line goes on.
#[cfg(not(target_os = "macos"))]
pub fn answer_if_asked() {}

/// This process, through libc.
#[cfg(unix)]
pub struct RealMe;

#[cfg(unix)]
impl Me for RealMe {
    fn umask_private(&mut self) {
        // SAFETY: sets this process's file creation mask; no memory is involved.
        unsafe { libc::umask(0o077) };
    }

    fn creds(&self) -> io::Result<Creds> {
        // SAFETY: reads of this process's own credentials.
        let (uid, euid, gid, egid) = unsafe {
            (
                libc::getuid(),
                libc::geteuid(),
                libc::getgid(),
                libc::getegid(),
            )
        };
        Ok(Creds {
            uid,
            euid,
            gid,
            egid,
            groups: kernel_groups()?,
        })
    }

    fn cannot_become_root(&mut self) -> bool {
        let refused = |rc: libc::c_int| {
            rc == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        };
        // SAFETY: each asks the kernel for root and is answered; a success is
        // a refusal of the whole child, which then exits.
        refused(unsafe { libc::setuid(0) }) && refused(unsafe { libc::setgid(0) })
    }

    fn home_of(&self, uid: u32) -> Option<PathBuf> {
        crate::appdirs::account_home(uid)
    }

    fn stdin(&mut self, bound: usize) -> io::Result<Option<Vec<u8>>> {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        io::stdin()
            .lock()
            .take(bound as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok((bytes.len() <= bound).then_some(bytes))
    }
}

/// `getgroups`: the kernel's list for this process.
#[cfg(unix)]
fn kernel_groups() -> io::Result<Vec<u32>> {
    loop {
        // SAFETY: a count query writes nothing.
        let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut groups: Vec<libc::gid_t> = vec![0; n as usize];
        // SAFETY: `groups` holds `n` entries.
        let got = unsafe { libc::getgroups(n, groups.as_mut_ptr()) };
        if got < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINVAL) {
                // The list grew between the two calls.
                continue;
            }
            return Err(e);
        }
        groups.truncate(got as usize);
        return Ok(groups);
    }
}

/// The files, through the system.
#[cfg(unix)]
pub struct RealFs;

/// Nothing at the path: it, or a folder on the way to it, is not there.
#[cfg(unix)]
fn gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ENOTDIR)
}

#[cfg(unix)]
fn kind_of(t: std::fs::FileType) -> Kind {
    if t.is_dir() {
        Kind::Dir
    } else if t.is_file() {
        Kind::File
    } else if t.is_symlink() {
        Kind::Link
    } else {
        Kind::Other
    }
}

/// [`RealFs::write`]'s new file: made by this call (`create_new`, never a
/// file found under the name), its bytes, `mode` through the descriptor (the
/// umask has no say in it), `fsync`, then `rename` over `path`.
#[cfg(unix)]
fn write_temp(temp: &Path, path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(temp)?;
    file.write_all(bytes)?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    file.sync_all()?;
    std::fs::rename(temp, path)
}

#[cfg(unix)]
fn read_error(e: &io::Error) -> ReadError {
    match e.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => ReadError::Eacces,
        _ => ReadError::Other,
    }
}

#[cfg(unix)]
impl Fs for RealFs {
    fn lstat(&self, path: &Path) -> Stat {
        use std::os::unix::fs::MetadataExt;
        match std::fs::symlink_metadata(path) {
            Ok(m) => Stat {
                kind: kind_of(m.file_type()),
                uid: Some(m.uid()),
                mode: Some(m.mode() & 0o7777),
            },
            Err(e) if gone(&e) => Stat::ABSENT,
            Err(_) => Stat {
                kind: Kind::Other,
                uid: None,
                mode: None,
            },
        }
    }

    fn writable(&self, path: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: a NUL-terminated path that `access` only reads.
        unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
    }

    fn dev_ino(&self, path: &Path) -> Option<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
    }

    fn read(&self, path: &Path, bound: usize) -> Held {
        use std::io::Read as _;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let (uid, mode) = match std::fs::symlink_metadata(path) {
            Ok(m) => (Some(m.uid()), Some(m.mode() & 0o7777)),
            Err(e) if gone(&e) => return Held::Absent,
            Err(e) => {
                return Held::Unreadable {
                    error: read_error(&e),
                    uid: None,
                    mode: None,
                };
            }
        };
        // `O_NONBLOCK`: a FIFO or a device answers the open at once instead
        // of holding it. `O_NOFOLLOW`: the file at this name, not one a link
        // there names.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if gone(&e) => return Held::Absent,
            Err(e) => {
                let error = match e.raw_os_error() == Some(libc::ELOOP) {
                    true => ReadError::NotRegular,
                    false => read_error(&e),
                };
                return Held::Unreadable { error, uid, mode };
            }
        };
        let meta = match file.metadata() {
            Ok(meta) => meta,
            Err(e) => {
                return Held::Unreadable {
                    error: read_error(&e),
                    uid,
                    mode,
                };
            }
        };
        let (uid, mode) = (meta.uid(), meta.mode() & 0o7777);
        let unreadable = |error| Held::Unreadable {
            error,
            uid: Some(uid),
            mode: Some(mode),
        };
        if !meta.file_type().is_file() {
            return unreadable(ReadError::NotRegular);
        }
        if meta.len() > bound as u64 {
            return unreadable(ReadError::TooLarge);
        }
        let mut bytes = Vec::new();
        if let Err(e) = (&file).take(bound as u64 + 1).read_to_end(&mut bytes) {
            return unreadable(read_error(&e));
        }
        if bytes.len() > bound {
            return unreadable(ReadError::TooLarge);
        }
        Held::Present { bytes, uid, mode }
    }

    fn write(&mut self, path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::Error::other(format!(
                "{} names no file",
                path.display()
            )));
        };
        let mut temp = name.to_os_string();
        temp.push(format!(".tmp.{}", std::process::id()));
        let temp = dir.join(temp);
        if let Err(e) = std::fs::remove_file(&temp)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(e);
        }
        let written = write_temp(&temp, path, bytes, mode);
        if written.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        written?;
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    fn remove(&mut self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn make_dir(&mut self, path: &Path, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    const UID: u32 = 501;
    const GID: u32 = 20;

    fn creds() -> Creds {
        Creds {
            uid: UID,
            euid: UID,
            gid: GID,
            egid: GID,
            groups: vec![GID],
        }
    }

    fn os(args: &[String]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// The command line after `one-row-home`, as root builds it.
    fn line(verb: Verb) -> Vec<OsString> {
        os(&child_args(UID, GID, verb)[1..])
    }

    /// A process whose every answer the test sets, and which says what was
    /// asked of it, in order.
    struct FakeMe {
        creds: Creds,
        root_refused: bool,
        home: Option<PathBuf>,
        stdin: Vec<u8>,
        asked: RefCell<Vec<&'static str>>,
    }

    impl FakeMe {
        fn new() -> FakeMe {
            FakeMe {
                creds: creds(),
                root_refused: true,
                home: Some(PathBuf::from("/Users/person")),
                stdin: Vec::new(),
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl Me for FakeMe {
        fn umask_private(&mut self) {
            self.asked.borrow_mut().push("umask");
        }
        fn creds(&self) -> io::Result<Creds> {
            self.asked.borrow_mut().push("creds");
            Ok(self.creds.clone())
        }
        fn cannot_become_root(&mut self) -> bool {
            self.asked.borrow_mut().push("root");
            self.root_refused
        }
        fn home_of(&self, uid: u32) -> Option<PathBuf> {
            self.asked.borrow_mut().push("home");
            assert_eq!(uid, self.creds.uid, "the home of another uid");
            self.home.clone()
        }
        fn stdin(&mut self, bound: usize) -> io::Result<Option<Vec<u8>>> {
            self.asked.borrow_mut().push("stdin");
            Ok((self.stdin.len() <= bound).then(|| self.stdin.clone()))
        }
    }

    #[derive(Clone)]
    struct Node {
        kind: Kind,
        uid: u32,
        mode: u32,
        bytes: Vec<u8>,
        writable: bool,
        readable: bool,
    }

    /// A home in memory: owners, modes and kinds as the test sets them, and
    /// a log of every call.
    struct FakeFs {
        nodes: BTreeMap<PathBuf, Node>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeFs {
        fn new() -> FakeFs {
            FakeFs {
                nodes: BTreeMap::new(),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn dir(&mut self, path: &Path, uid: u32, mode: u32, writable: bool) {
            self.put(path, Kind::Dir, uid, mode, Vec::new(), writable);
        }

        fn file(&mut self, path: &Path, bytes: &[u8], mode: u32) {
            self.put(path, Kind::File, UID, mode, bytes.to_vec(), true);
        }

        fn put(
            &mut self,
            path: &Path,
            kind: Kind,
            uid: u32,
            mode: u32,
            bytes: Vec<u8>,
            writable: bool,
        ) {
            let node = Node {
                kind,
                uid,
                mode,
                bytes,
                writable,
                readable: true,
            };
            self.nodes.insert(path.to_path_buf(), node);
        }

        /// A name `lstat` cannot look at, as where its folder does not let
        /// the person through: no kind it can tell, no owner, no mode.
        fn hidden(&mut self, path: &Path) {
            self.put(path, Kind::Other, 0, 0, Vec::new(), false);
            if let Some(node) = self.nodes.get_mut(path) {
                node.readable = false;
            }
        }

        fn log(&self, what: String) {
            self.calls.borrow_mut().push(what);
        }

        fn changes(&self) -> Vec<String> {
            let changing = ["write", "remove", "rename", "make_dir"];
            self.calls
                .borrow()
                .iter()
                .filter(|c| changing.iter().any(|w| c.starts_with(w)))
                .cloned()
                .collect()
        }
    }

    impl Fs for FakeFs {
        fn lstat(&self, path: &Path) -> Stat {
            self.log(format!("lstat {}", path.display()));
            self.nodes
                .get(path)
                .map_or(Stat::ABSENT, |n| match (n.kind, n.readable) {
                    // See `hidden`.
                    (Kind::Other, false) => Stat {
                        kind: Kind::Other,
                        uid: None,
                        mode: None,
                    },
                    _ => Stat {
                        kind: n.kind,
                        uid: Some(n.uid),
                        mode: Some(n.mode),
                    },
                })
        }
        fn writable(&self, path: &Path) -> bool {
            self.log(format!("access {}", path.display()));
            self.nodes.get(path).is_some_and(|n| n.writable)
        }
        fn dev_ino(&self, path: &Path) -> Option<(u64, u64)> {
            self.log(format!("stat {}", path.display()));
            let n = self.nodes.keys().position(|p| p == path)?;
            Some((7, n as u64 + 100))
        }
        fn read(&self, path: &Path, bound: usize) -> Held {
            self.log(format!("read {}", path.display()));
            let Some(n) = self.nodes.get(path) else {
                return Held::Absent;
            };
            let unreadable = |error| Held::Unreadable {
                error,
                uid: Some(n.uid),
                mode: Some(n.mode),
            };
            match (n.kind, n.readable) {
                (Kind::File, false) => unreadable(ReadError::Eacces),
                (Kind::File, true) if n.bytes.len() > bound => unreadable(ReadError::TooLarge),
                (Kind::File, true) => Held::Present {
                    bytes: n.bytes.clone(),
                    uid: n.uid,
                    mode: n.mode,
                },
                _ => unreadable(ReadError::NotRegular),
            }
        }
        fn write(&mut self, path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
            self.log(format!("write {} {mode:o}", path.display()));
            self.put(path, Kind::File, UID, mode, bytes.to_vec(), true);
            Ok(())
        }
        fn remove(&mut self, path: &Path) -> io::Result<()> {
            self.log(format!("remove {}", path.display()));
            self.nodes
                .remove(path)
                .map(|_| ())
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
        fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
            self.log(format!("rename {} {}", from.display(), to.display()));
            let node = self
                .nodes
                .remove(from)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            self.nodes.insert(to.to_path_buf(), node);
            Ok(())
        }
        fn make_dir(&mut self, path: &Path, mode: u32) -> io::Result<()> {
            self.log(format!("make_dir {} {mode:o}", path.display()));
            self.dir(path, UID, mode, true);
            Ok(())
        }
    }

    const ENROLLED: &str = "server_url = \"https://roomler.example\"\nagent_token = \"tok-person\"\n\
        agent_id = \"a-1\"\ntenant_id = \"t-1\"\nmachine_id = \"m-1\"\nmachine_name = \"Mac\"\n";
    const PLIST: &[u8] = b"<?xml version=\"1.0\"?><plist><dict/></plist>";

    /// A person's home as the package leaves it: `~/Library`, its
    /// LaunchAgents with the plist, and the roomler folder with a config.
    fn a_home() -> (FakeFs, Places) {
        let mut fs = FakeFs::new();
        let home = PathBuf::from("/Users/person");
        let folder = crate::appdirs::macos_config_dir_in_home(&home, |_| false);
        fs.dir(&home, UID, 0o750, true);
        fs.dir(&home.join("Library"), UID, 0o700, true);
        fs.dir(
            &home.join("Library").join("Application Support"),
            UID,
            0o755,
            true,
        );
        fs.dir(&folder, UID, 0o755, true);
        fs.dir(&home.join("Library").join("LaunchAgents"), UID, 0o755, true);
        fs.file(&folder.join(CONFIG), ENROLLED.as_bytes(), 0o600);
        fs.file(
            &home.join("Library").join("LaunchAgents").join(AGENT_PLIST),
            PLIST,
            0o644,
        );
        let at = Places::of(&home, &fs);
        fs.calls.borrow_mut().clear();
        (fs, at)
    }

    fn run_with(args: Vec<OsString>, me: &mut FakeMe, fs: &mut FakeFs) -> Outcome {
        run(&args, me, fs)
    }

    #[test]
    fn child_args_parse_back() {
        let sha = Expect::of(Some(b"bytes"));
        for verb in [
            Verb::Read,
            Verb::Convert {
                config: sha,
                plist: Expect::Absent,
            },
            Verb::PutBack,
            Verb::PutBackLegacy,
        ] {
            let args = child_args(UID, GID, verb);
            assert_eq!(args[0], VERB);
            let after = &args[1..];
            let asked = asked(after).expect("asked");
            assert_eq!(
                (asked.uid, asked.gid, asked.version.as_str()),
                (UID, GID, VERSION)
            );
            assert_eq!(super::verb(after), Ok(verb));
        }
    }

    /// Steps 1 to 3: the umask before anything, the kernel's word before any
    /// argument, and each way of not being exactly the account root named is
    /// exit 2 with nothing read.
    #[test]
    fn child_refuses_an_identity_it_was_not_given() {
        let other_groups = |groups: Vec<u32>| Creds { groups, ..creds() };
        let cases: Vec<(&str, Creds, u32, u32)> = vec![
            ("another uid", creds(), UID + 1, GID),
            ("another gid", creds(), UID, GID + 1),
            (
                "effective uid differs",
                Creds { euid: 0, ..creds() },
                UID,
                GID,
            ),
            (
                "effective gid differs",
                Creds { egid: 0, ..creds() },
                UID,
                GID,
            ),
            ("real uid differs", Creds { uid: 0, ..creds() }, UID, GID),
            ("real gid differs", Creds { gid: 0, ..creds() }, UID, GID),
            ("one more group", other_groups(vec![GID, 12]), UID, GID),
            ("group 0 kept", other_groups(vec![0, GID]), UID, GID),
            ("no group at all", other_groups(vec![]), UID, GID),
            ("another group alone", other_groups(vec![12]), UID, GID),
            (
                "root asked for",
                Creds {
                    uid: 0,
                    euid: 0,
                    ..creds()
                },
                0,
                GID,
            ),
            (
                "group 0 asked for",
                Creds {
                    gid: 0,
                    egid: 0,
                    groups: vec![0],
                    ..creds()
                },
                UID,
                0,
            ),
        ];
        for (case, creds, uid, gid) in cases {
            let mut me = FakeMe {
                creds,
                ..FakeMe::new()
            };
            let (mut fs, _) = a_home();
            let out = run_with(os(&child_args(uid, gid, Verb::Read)[1..]), &mut me, &mut fs);
            assert_eq!(out.exit, Exit::Refused, "{case}: {}", out.stderr);
            assert!(out.stdout.is_empty(), "{case}");
            assert!(
                fs.calls.borrow().is_empty(),
                "{case}: {:?}",
                fs.calls.borrow()
            );
            assert_eq!(*me.asked.borrow(), ["umask", "creds"], "{case}");
        }
    }

    /// Steps 4 to 6, in order, each exit 2 with nothing read.
    #[test]
    fn child_refuses_another_version() {
        let mut args = line(Verb::Read);
        let at = args.iter().position(|a| a == "--version").unwrap() + 1;
        args[at] = OsString::from("0.4.1");
        let mut me = FakeMe::new();
        let (mut fs, _) = a_home();
        let out = run_with(args, &mut me, &mut fs);
        assert_eq!(out.exit, Exit::Refused, "{}", out.stderr);
        assert!(out.stderr.contains("version 0.4.1"), "{}", out.stderr);
        assert!(fs.calls.borrow().is_empty());
        assert_eq!(*me.asked.borrow(), ["umask", "creds", "root"]);

        let mut me = FakeMe {
            root_refused: false,
            ..FakeMe::new()
        };
        let out = run_with(line(Verb::Read), &mut me, &mut fs);
        assert_eq!(out.exit, Exit::Refused, "could become root again");
        assert_eq!(*me.asked.borrow(), ["umask", "creds", "root"]);

        let mut me = FakeMe {
            home: None,
            ..FakeMe::new()
        };
        let out = run_with(line(Verb::Read), &mut me, &mut fs);
        assert_eq!(out.exit, Exit::Refused, "no home in the database");
        assert_eq!(*me.asked.borrow(), ["umask", "creds", "root", "home"]);
        assert!(fs.calls.borrow().is_empty());
    }

    /// Every shape of command line that is not one root builds is exit 2,
    /// before anything is read.
    #[test]
    fn a_usage_error_is_refused_before_anything_is_read() {
        let with = |extra: &[&str]| {
            let mut args = line(Verb::Read);
            args.extend(extra.iter().map(OsString::from));
            args
        };
        let mut wrong: Vec<(&str, Vec<OsString>)> = vec![
            ("a flag no verb takes", with(&["--home", "/Users/other"])),
            ("a value with no flag", with(&["/Users/other"])),
            ("a flag twice", with(&["--uid", "501"])),
            (
                "an expectation for read",
                with(&["--expect-config", "none"]),
            ),
            ("no verb", os(&["--uid".into(), "501".into()])),
            ("an unknown verb", os(&["blank".into()])),
        ];
        let mut convert = line(Verb::Convert {
            config: Expect::Absent,
            plist: Expect::Absent,
        });
        let last = convert.len() - 1;
        convert[last] = OsString::from("NONE");
        wrong.push(("an expectation that is not one", convert));
        let mut short = line(Verb::Read);
        short.truncate(short.len() - 2);
        wrong.push(("no --version", short));
        let mut signed = line(Verb::Read);
        let at = signed.iter().position(|a| a == "--uid").unwrap() + 1;
        signed[at] = OsString::from("+501");
        wrong.push(("a signed uid", signed));
        for (case, args) in wrong {
            let mut me = FakeMe::new();
            let (mut fs, _) = a_home();
            let out = run_with(args, &mut me, &mut fs);
            assert_eq!(out.exit, Exit::Refused, "{case}: {}", out.stderr);
            assert!(
                fs.calls.borrow().is_empty(),
                "{case}: {:?}",
                fs.calls.borrow()
            );
        }
    }

    /// Its home is the account database's: a home on the command line is a
    /// usage error, and what it reads is under the database's home. And the
    /// module reads no environment at all.
    #[test]
    fn the_child_ignores_the_home_it_was_given() {
        let mut args = line(Verb::Read);
        args.extend([OsString::from("--home"), OsString::from("/Users/other")]);
        let (mut fs, _) = a_home();
        let out = run_with(args, &mut FakeMe::new(), &mut fs);
        assert_eq!(out.exit, Exit::Refused, "{}", out.stderr);
        assert!(fs.calls.borrow().is_empty());

        let (mut fs, _) = a_home();
        let out = run_with(line(Verb::Read), &mut FakeMe::new(), &mut fs);
        assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
        // Each call is `<what> <path>`: every path is the database's home or
        // under it, by whole components.
        let calls = fs.calls.borrow();
        assert!(!calls.is_empty());
        assert!(
            calls.iter().all(|c| c
                .split_once(' ')
                .is_some_and(|(_, path)| Path::new(path).starts_with("/Users/person"))),
            "{calls:?}"
        );

        let source = include_str!("one_row_home.rs");
        let code = source.split("\nmod tests {").next().unwrap();
        for word in ["env::var", "getenv", "env!(\"HOME", "home_dir"] {
            assert!(
                !code.contains(word),
                "the child reads the environment: `{word}`"
            );
        }
    }

    /// The child reads a file through [`Fs::read`] alone (bounded, on the
    /// open descriptor): nothing in it reads by path, and nothing saves or
    /// loads a config, which would read the live file again by path.
    #[test]
    fn the_child_reads_no_file_by_path() {
        let source = include_str!("one_row_home.rs");
        let code = source.split("\nmod tests {").next().unwrap();
        for word in [
            "config::save",
            "config::load",
            "read_to_string",
            "fs::read(",
        ] {
            assert!(!code.contains(word), "the child reads by path: `{word}`");
        }
    }

    #[test]
    fn read_reports_what_is_there_and_who_owns_it() {
        let (mut fs, at) = a_home();
        fs.file(&at.legacy_config, b"old", 0o600);
        // A name it cannot look at is reported as that, not as a copy.
        fs.hidden(&at.legacy_plist);
        fs.put(&at.agents_dir, Kind::Dir, 0, 0o755, Vec::new(), false);
        let out = answer(Verb::Read, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
        let reply = Reply::parse(&out.stdout, UID, GID, Verb::Read).expect("parses");
        let read = reply.read.expect("read");
        assert_eq!(
            read.config,
            Held::Present {
                bytes: ENROLLED.as_bytes().to_vec(),
                uid: UID,
                mode: 0o600
            }
        );
        assert_eq!(
            read.plist,
            Held::Present {
                bytes: PLIST.to_vec(),
                uid: UID,
                mode: 0o644
            }
        );
        assert_eq!(
            read.agents_dir,
            Entry {
                kind: Kind::Dir,
                uid: Some(0),
                mode: Some(0o755),
                writable: false
            }
        );
        assert!(read.folder.writable);
        assert_eq!(read.home.kind, Kind::Dir);
        assert!(read.home.absolute);
        assert!(read.home.dev.is_some() && read.home.ino.is_some());
        assert_eq!(read.segment, Segment::Current);
        assert_eq!(
            read.legacy,
            LegacyEntries {
                config: Entry {
                    kind: Kind::File,
                    uid: Some(UID),
                    mode: Some(0o600),
                    writable: true
                },
                plist: Entry {
                    kind: Kind::Other,
                    uid: None,
                    mode: None,
                    writable: false
                }
            }
        );
        assert!(fs.changes().is_empty(), "{:?}", fs.changes());
    }

    /// Root's side names the owner and the mode of a kept copy when it
    /// refuses, so both are on the wire, as a folder's are.
    #[test]
    fn read_reports_a_kept_copy_with_its_owner_and_mode() {
        let (mut fs, at) = a_home();
        fs.put(
            &at.legacy_config,
            Kind::File,
            0,
            0o600,
            b"old".to_vec(),
            false,
        );
        let out = answer(Verb::Read, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
        let reply: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let legacy = &reply["read"]["legacy"];
        assert_eq!(
            legacy["config"],
            serde_json::json!({"kind": "file", "uid": 0, "mode": 0o600, "writable": false}),
            "{legacy}"
        );
        assert_eq!(legacy["plist"]["kind"], "absent", "{legacy}");
    }

    #[test]
    fn read_names_a_file_it_cannot_read_and_why() {
        let (mut fs, at) = a_home();
        fs.nodes.get_mut(&at.config).unwrap().readable = false;
        fs.nodes.get_mut(&at.config).unwrap().uid = 0;
        fs.put(&at.plist, Kind::Other, UID, 0o644, Vec::new(), true);
        let out = answer(Verb::Read, &at.home, &creds(), &[], &mut fs);
        let read = Reply::parse(&out.stdout, UID, GID, Verb::Read)
            .unwrap()
            .read
            .unwrap();
        assert_eq!(
            read.config,
            Held::Unreadable {
                error: ReadError::Eacces,
                uid: Some(0),
                mode: Some(0o600)
            }
        );
        assert!(matches!(
            read.plist,
            Held::Unreadable {
                error: ReadError::NotRegular,
                ..
            }
        ));
    }

    fn shown(at: &Places, fs: &FakeFs) -> Verb {
        let bytes = |p: &Path| fs.nodes.get(p).map(|n| n.bytes.clone());
        Verb::Convert {
            config: Expect::of(bytes(&at.config).as_deref()),
            plist: Expect::of(bytes(&at.plist).as_deref()),
        }
    }

    #[test]
    fn convert_takes_the_enrollments_off_and_the_plist_away() {
        let (mut fs, at) = a_home();
        let verb = shown(&at, &fs);
        let out = answer(verb, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
        let reply = Reply::parse(&out.stdout, UID, GID, verb).expect("parses");
        let convert = reply.convert.expect("convert");
        assert!(convert.plist_removed);
        assert!(!fs.nodes.contains_key(&at.plist));
        let saved = &fs.nodes[&at.config];
        assert_eq!(convert.config_sha256, Some(sha256_hex(&saved.bytes)));
        assert_eq!(saved.mode, 0o600);
        let cfg: crate::config::AgentConfig =
            toml::from_str(std::str::from_utf8(&saved.bytes).unwrap()).unwrap();
        assert!(cfg.agent_token.is_empty() && cfg.tenant_id.is_empty());
        assert_eq!(cfg.machine_name, "Mac", "every other key kept");
        assert_eq!(
            fs.changes(),
            [
                format!("remove {}", at.plist.display()),
                format!(
                    "write {} 600",
                    crate::config::prev_path(&at.config).display()
                ),
                format!("write {} 600", at.config.display()),
            ]
        );
        assert!(out.stderr.is_empty(), "{}", out.stderr);
    }

    /// The new config is made from the bytes `convert` checked, and from
    /// nothing read later: the config is read through [`Fs::read`] once,
    /// before it is written, and the reply's sha256 is of the bytes written.
    /// A key this build does not know is kept, and one it cannot keep is said
    /// on stderr.
    #[test]
    fn convert_writes_from_the_bytes_it_checked() {
        let (mut fs, at) = a_home();
        let outgoing = format!(
            "{ENROLLED}a_key_from_a_newer_build = 1\n\n[[orgs]]\nlabel = \"other\"\n\
             server_url = \"https://roomler.example\"\nagent_token = \"tok-other\"\n\
             agent_id = \"a-2\"\ntenant_id = \"t-2\"\nan_org_key_from_a_newer_build = 2\n"
        );
        fs.file(&at.config, outgoing.as_bytes(), 0o600);
        let verb = shown(&at, &fs);
        let out = answer(verb, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
        let saved = String::from_utf8(fs.nodes[&at.config].bytes.clone()).unwrap();
        assert!(saved.contains("a_key_from_a_newer_build = 1"), "{saved}");
        assert!(!saved.contains("tok-"), "{saved}");
        assert!(
            out.stderr.contains("orgs.0.an_org_key_from_a_newer_build")
                && !out.stderr.contains('\n'),
            "{}",
            out.stderr
        );
        let config = at.config.display().to_string();
        let calls = fs.calls.borrow();
        let on_config: Vec<&str> = calls
            .iter()
            .filter_map(|c| {
                let (what, rest) = c.split_once(' ')?;
                let rest = rest.strip_prefix(config.as_str())?;
                (rest.is_empty() || rest.starts_with(' ')).then_some(what)
            })
            .collect();
        assert_eq!(on_config, ["read", "write"], "{calls:?}");
        let convert = Reply::parse(&out.stdout, UID, GID, verb)
            .unwrap()
            .convert
            .unwrap();
        assert_eq!(convert.config_sha256, Some(sha256_hex(saved.as_bytes())));
    }

    /// The outgoing config is kept as its `.prev`, 0600, as a save keeps it.
    #[test]
    fn convert_keeps_the_outgoing_config_as_prev_0600() {
        let (mut fs, at) = a_home();
        let verb = shown(&at, &fs);
        let out = answer(verb, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
        let prev = &fs.nodes[&crate::config::prev_path(&at.config)];
        assert_eq!(prev.bytes, ENROLLED.as_bytes());
        assert_eq!(prev.mode, 0o600);
    }

    /// Exit 3 for each way a live file can differ from what root was shown,
    /// with both checked before anything changes.
    #[test]
    fn convert_refuses_bytes_it_was_not_shown() {
        let other = Expect::of(Some(b"other bytes"));
        // Every `a_home()` holds the same bytes, so what one shows holds for
        // each.
        let (fs, at) = a_home();
        let Verb::Convert {
            config: seen_config,
            plist: seen_plist,
        } = shown(&at, &fs)
        else {
            unreachable!("shown() is a convert");
        };
        let cases = [
            (
                "the config differs",
                Verb::Convert {
                    config: other,
                    plist: seen_plist,
                },
            ),
            (
                "the plist differs, after a config that matches",
                Verb::Convert {
                    config: seen_config,
                    plist: other,
                },
            ),
            (
                "a plist where root saw none",
                Verb::Convert {
                    config: seen_config,
                    plist: Expect::Absent,
                },
            ),
            (
                "a config where root saw none",
                Verb::Convert {
                    config: Expect::Absent,
                    plist: seen_plist,
                },
            ),
        ];
        for (case, verb) in cases {
            let (mut fs, at) = a_home();
            let out = answer(verb, &at.home, &creds(), &[], &mut fs);
            assert_eq!(out.exit, Exit::Changed, "{case}: {}", out.stderr);
            assert!(out.stdout.is_empty(), "{case}");
            assert!(fs.changes().is_empty(), "{case}: {:?}", fs.changes());
        }
        let (mut fs, at) = a_home();
        let verb = shown(&at, &fs);
        fs.nodes.remove(&at.config);
        let out = answer(verb, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::Changed, "a config gone: {}", out.stderr);
        assert!(fs.changes().is_empty());
        // A link, a FIFO or a folder where root saw the config has changed
        // as well.
        for kind in [Kind::Link, Kind::Other, Kind::Dir] {
            let (mut fs, at) = a_home();
            let verb = shown(&at, &fs);
            fs.put(&at.config, kind, UID, 0o600, Vec::new(), true);
            let out = answer(verb, &at.home, &creds(), &[], &mut fs);
            assert_eq!(out.exit, Exit::Changed, "{kind:?}: {}", out.stderr);
            assert!(fs.changes().is_empty(), "{kind:?}: {:?}", fs.changes());
        }
    }

    /// A folder it would change that it cannot write is exit 4 before
    /// anything changes, naming the folder, its owner and its mode; a config
    /// over its bound, a new config that would be over it, or a config that
    /// does not parse is exit 1, also before anything changes.
    #[test]
    fn convert_that_cannot_finish_changes_nothing() {
        let (mut fs, at) = a_home();
        fs.put(&at.agents_dir, Kind::Dir, 0, 0o755, Vec::new(), false);
        let verb = shown(&at, &fs);
        let out = answer(verb, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::NotWritable, "{}", out.stderr);
        assert!(
            out.stderr.contains("owner uid 0, mode 0755"),
            "{}",
            out.stderr
        );
        assert!(fs.changes().is_empty(), "{:?}", fs.changes());

        let (mut fs, at) = a_home();
        let verb = shown(&at, &fs);
        fs.file(&at.config, &vec![b'#'; CONFIG_BOUND + 1], 0o600);
        let out = answer(verb, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::Error, "{}", out.stderr);
        assert!(fs.changes().is_empty());

        let (mut fs, at) = a_home();
        fs.file(&at.config, b"not = [toml", 0o600);
        let verb = shown(&at, &fs);
        let out = answer(verb, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::Error, "a config that does not parse");
        assert!(fs.changes().is_empty(), "{:?}", fs.changes());

        // Within its bound, but the text a save writes over it is not: the
        // keys a save adds, and the one this build does not know, kept.
        let (mut fs, at) = a_home();
        let line = "a_key_this_build_does_not_know = \"\"\n";
        let fill = "x".repeat(CONFIG_BOUND - ENROLLED.len() - line.len());
        let config = format!("{ENROLLED}a_key_this_build_does_not_know = \"{fill}\"\n");
        assert_eq!(config.len(), CONFIG_BOUND);
        fs.file(&at.config, config.as_bytes(), 0o600);
        let verb = shown(&at, &fs);
        let out = answer(verb, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::Error, "{}", out.stderr);
        assert!(out.stderr.contains("would be over"), "{}", out.stderr);
        assert!(fs.changes().is_empty(), "{:?}", fs.changes());
    }

    fn kept(
        config: Option<&[u8]>,
        config_mode: Option<u32>,
        plist: Option<&[u8]>,
        plist_mode: Option<u32>,
    ) -> Vec<u8> {
        serde_json::to_vec(&Kept {
            v: FORMAT,
            config: config.map(<[u8]>::to_vec),
            config_mode,
            plist: plist.map(<[u8]>::to_vec),
            plist_mode,
        })
        .unwrap()
    }

    /// The modes it asks for, on any platform: the plist's recorded mode
    /// less group and other write (0644 when none), the config's owner bits
    /// (0600 when none), and `~/Library/LaunchAgents` made 0755.
    #[test]
    fn put_back_asks_for_0644_and_a_0755_agents_dir() {
        let (mut fs, at) = a_home();
        fs.nodes.remove(&at.plist);
        fs.nodes.remove(&at.agents_dir);
        let stdin = kept(Some(b"config"), Some(0o640), Some(PLIST), Some(0o666));
        let out = answer(Verb::PutBack, &at.home, &creds(), &stdin, &mut fs);
        assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
        let reply = Reply::parse(&out.stdout, UID, GID, Verb::PutBack).unwrap();
        let put = reply.put_back.unwrap();
        assert_eq!(put.config_sha256, Some(sha256_hex(b"config")));
        assert_eq!(put.plist_sha256, Some(sha256_hex(PLIST)));
        assert_eq!(
            fs.changes(),
            [
                format!("write {} 600", at.config.display()),
                format!("make_dir {} 755", at.agents_dir.display()),
                format!("write {} 644", at.plist.display()),
            ]
        );
        assert_eq!(plist_mode(None), 0o644);
        assert_eq!(plist_mode(Some(0o100664)), 0o644);
        assert_eq!(plist_mode(Some(0o600)), 0o600);
        assert_eq!(config_mode(None), 0o600);
        assert_eq!(config_mode(Some(0o644)), 0o600);
        assert_eq!(config_mode(Some(0o400)), 0o400);
    }

    /// A home with no `~/Library`: the person's process makes it 0700, and
    /// each folder below it, before each file.
    #[test]
    fn put_back_makes_a_missing_library() {
        let (mut fs, at) = a_home();
        for path in [
            &at.config,
            &at.folder,
            &at.support,
            &at.plist,
            &at.agents_dir,
            &at.library,
        ] {
            fs.nodes.remove(path);
        }
        let stdin = kept(Some(b"config"), None, Some(PLIST), None);
        let out = answer(Verb::PutBack, &at.home, &creds(), &stdin, &mut fs);
        assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
        assert_eq!(
            fs.changes(),
            [
                format!("make_dir {} 700", at.library.display()),
                format!("make_dir {} 755", at.support.display()),
                format!("make_dir {} 700", at.folder.display()),
                format!("write {} 600", at.config.display()),
                format!("make_dir {} 755", at.agents_dir.display()),
                format!("write {} 644", at.plist.display()),
            ]
        );
    }

    /// A folder on the way that cannot be looked at: the folder it is in
    /// does not let the person through, so that one is named, exit 4.
    #[test]
    fn put_back_through_a_folder_it_cannot_look_at_is_exit_4() {
        let (mut fs, at) = a_home();
        fs.nodes.remove(&at.plist);
        fs.hidden(&at.agents_dir);
        let stdin = kept(None, None, Some(PLIST), None);
        let out = answer(Verb::PutBack, &at.home, &creds(), &stdin, &mut fs);
        assert_eq!(out.exit, Exit::NotWritable, "{}", out.stderr);
        let named = format!("not writable: {} (", at.library.display());
        assert!(out.stderr.contains(&named), "{}", out.stderr);
        assert!(fs.changes().is_empty(), "{:?}", fs.changes());
    }

    /// Where the person's folder is gone, the person's process makes it.
    #[test]
    fn put_back_makes_the_folder_it_finds_gone() {
        let (mut fs, at) = a_home();
        fs.nodes.remove(&at.config);
        fs.nodes.remove(&at.folder);
        let stdin = kept(Some(b"config"), None, None, None);
        let out = answer(Verb::PutBack, &at.home, &creds(), &stdin, &mut fs);
        assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
        assert_eq!(fs.nodes[&at.folder].mode, 0o700);
        assert_eq!(fs.nodes[&at.config].bytes, b"config");
    }

    /// `~/Library/LaunchAgents` not writable: the config still goes back,
    /// then exit 4 names the folder; a second run after the fix finishes.
    #[test]
    fn put_back_where_agents_dir_is_not_writable_is_exit_4_after_the_config() {
        let (mut fs, at) = a_home();
        fs.file(&at.config, b"blank", 0o600);
        fs.nodes.remove(&at.plist);
        fs.put(&at.agents_dir, Kind::Dir, 0, 0o755, Vec::new(), false);
        let stdin = kept(Some(b"config"), Some(0o600), Some(PLIST), Some(0o644));
        let out = answer(Verb::PutBack, &at.home, &creds(), &stdin, &mut fs);
        assert_eq!(out.exit, Exit::NotWritable, "{}", out.stderr);
        assert!(out.stderr.contains("LaunchAgents"), "{}", out.stderr);
        assert_eq!(fs.nodes[&at.config].bytes, b"config");
        assert!(!fs.nodes.contains_key(&at.plist));

        fs.nodes.get_mut(&at.agents_dir).unwrap().writable = true;
        let out = answer(Verb::PutBack, &at.home, &creds(), &stdin, &mut fs);
        assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
        assert_eq!(fs.nodes[&at.plist].bytes, PLIST);
    }

    #[test]
    fn put_back_refuses_input_it_does_not_read_and_bytes_over_their_bound() {
        let (mut fs, at) = a_home();
        let mut unknown: serde_json::Value =
            serde_json::from_slice(&kept(Some(b"c"), None, None, None)).unwrap();
        unknown["home"] = serde_json::json!("/Users/other");
        let cases: Vec<(&str, Vec<u8>, Exit)> = vec![
            ("not JSON", b"{".to_vec(), Exit::Refused),
            (
                "an unknown field",
                serde_json::to_vec(&unknown).unwrap(),
                Exit::Refused,
            ),
            (
                "a config over its bound",
                kept(Some(&vec![b'#'; CONFIG_BOUND + 1]), None, None, None),
                Exit::Error,
            ),
            (
                "a plist over its bound",
                kept(None, None, Some(&vec![b'<'; PLIST_BOUND + 1]), None),
                Exit::Error,
            ),
        ];
        for (case, stdin, exit) in cases {
            let out = answer(Verb::PutBack, &at.home, &creds(), &stdin, &mut fs);
            assert_eq!(out.exit, exit, "{case}: {}", out.stderr);
            assert!(
                !out.stderr.contains("/Users/other"),
                "{case}: {}",
                out.stderr
            );
            assert!(fs.changes().is_empty(), "{case}: {:?}", fs.changes());
        }
        let mut me = FakeMe {
            stdin: vec![b' '; STDIN_BOUND + 1],
            ..FakeMe::new()
        };
        let out = run_with(line(Verb::PutBack), &mut me, &mut fs);
        assert_eq!(out.exit, Exit::Error, "stdin over its bound");
        assert!(fs.changes().is_empty());
    }

    /// A Mac converted by an older release: its copies renamed back as the
    /// person; where `~/Library/LaunchAgents` is root's, the config goes back
    /// and the plist copy stays, exit 4.
    #[test]
    fn put_back_legacy_renames_the_old_copies_back() {
        let (mut fs, at) = a_home();
        fs.file(&at.config, b"blank", 0o600);
        fs.file(&at.legacy_config, ENROLLED.as_bytes(), 0o600);
        fs.nodes.remove(&at.plist);
        fs.file(&at.legacy_plist, PLIST, 0o644);
        let out = answer(Verb::PutBackLegacy, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
        let moved = Reply::parse(&out.stdout, UID, GID, Verb::PutBackLegacy)
            .unwrap()
            .put_back_legacy
            .unwrap();
        assert_eq!(
            moved,
            Legacy {
                config: true,
                plist: true
            }
        );
        assert_eq!(fs.nodes[&at.config].bytes, ENROLLED.as_bytes());
        assert_eq!(fs.nodes[&at.plist].bytes, PLIST);

        let (mut fs, at) = a_home();
        fs.file(&at.legacy_config, ENROLLED.as_bytes(), 0o600);
        fs.nodes.remove(&at.plist);
        fs.file(&at.legacy_plist, PLIST, 0o644);
        fs.put(&at.agents_dir, Kind::Dir, 0, 0o755, Vec::new(), false);
        let out = answer(Verb::PutBackLegacy, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::NotWritable, "{}", out.stderr);
        assert!(
            !fs.nodes.contains_key(&at.legacy_config),
            "the config went back"
        );
        assert!(
            fs.nodes.contains_key(&at.legacy_plist),
            "the plist copy stayed"
        );
    }

    /// The roomler folder not writable: nothing is renamed, and exit 4
    /// names the folder.
    #[test]
    fn put_back_legacy_where_the_folder_is_not_writable_is_exit_4() {
        let (mut fs, at) = a_home();
        fs.file(&at.legacy_config, ENROLLED.as_bytes(), 0o600);
        fs.put(&at.folder, Kind::Dir, 0, 0o755, Vec::new(), false);
        let out = answer(Verb::PutBackLegacy, &at.home, &creds(), &[], &mut fs);
        assert_eq!(out.exit, Exit::NotWritable, "{}", out.stderr);
        let named = format!("not writable: {} (", at.folder.display());
        assert!(out.stderr.contains(&named), "{}", out.stderr);
        assert!(fs.changes().is_empty(), "{:?}", fs.changes());
        assert!(fs.nodes.contains_key(&at.legacy_config));
    }

    fn a_reply() -> Vec<u8> {
        let (mut fs, at) = a_home();
        answer(Verb::Read, &at.home, &creds(), &[], &mut fs).stdout
    }

    /// Only a file at an old copy's name is put back. A folder or a link
    /// there is refused as changed (3), and nothing is renamed, the other
    /// copy included.
    #[test]
    fn put_back_legacy_refuses_a_folder_or_a_link() {
        for kind in [Kind::Dir, Kind::Link] {
            for config_is_it in [true, false] {
                let (mut fs, at) = a_home();
                fs.nodes.remove(&at.config);
                fs.nodes.remove(&at.plist);
                let (it, file) = match config_is_it {
                    true => (&at.legacy_config, &at.legacy_plist),
                    false => (&at.legacy_plist, &at.legacy_config),
                };
                fs.put(it, kind, UID, 0o755, Vec::new(), true);
                fs.file(file, b"old", 0o600);
                let out = answer(Verb::PutBackLegacy, &at.home, &creds(), &[], &mut fs);
                let at_it = it.display();
                assert_eq!(
                    out.exit,
                    Exit::Changed,
                    "{kind:?} at {at_it}: {}",
                    out.stderr
                );
                assert!(fs.changes().is_empty(), "{:?}", fs.changes());
            }
        }
    }

    #[test]
    fn reply_with_unknown_fields_is_refused() {
        assert!(Reply::parse(&a_reply(), UID, GID, Verb::Read).is_ok());
        let mut v: serde_json::Value = serde_json::from_slice(&a_reply()).unwrap();
        v["home_path"] = serde_json::json!("/Users/other");
        let e = Reply::parse(&serde_json::to_vec(&v).unwrap(), UID, GID, Verb::Read).unwrap_err();
        assert!(!e.contains("/Users/other"), "{e}");
        let mut v: serde_json::Value = serde_json::from_slice(&a_reply()).unwrap();
        v["read"]["config"]["present"]["path"] = serde_json::json!("x");
        assert!(Reply::parse(&serde_json::to_vec(&v).unwrap(), UID, GID, Verb::Read).is_err());
        let mut v: serde_json::Value = serde_json::from_slice(&a_reply()).unwrap();
        v["convert"] = serde_json::json!({"config_sha256": null, "plist_removed": true});
        assert!(
            Reply::parse(&serde_json::to_vec(&v).unwrap(), UID, GID, Verb::Read).is_err(),
            "a second answer"
        );
        let mut v: serde_json::Value = serde_json::from_slice(&a_reply()).unwrap();
        v["version"] = serde_json::json!("0.4.1");
        assert!(Reply::parse(&serde_json::to_vec(&v).unwrap(), UID, GID, Verb::Read).is_err());
        assert!(
            Reply::parse(&a_reply(), UID, GID, Verb::PutBack).is_err(),
            "another verb's"
        );
    }

    #[test]
    fn reply_with_other_credentials_is_refused() {
        assert!(Reply::parse(&a_reply(), UID + 1, GID, Verb::Read).is_err());
        assert!(Reply::parse(&a_reply(), UID, GID + 1, Verb::Read).is_err());
        for groups in [serde_json::json!([GID, 80]), serde_json::json!([0])] {
            let mut v: serde_json::Value = serde_json::from_slice(&a_reply()).unwrap();
            v["groups"] = groups;
            assert!(Reply::parse(&serde_json::to_vec(&v).unwrap(), UID, GID, Verb::Read).is_err());
        }
    }

    /// The bytes of a config never reach a `{:?}`: it holds a token and a key.
    #[test]
    fn a_held_file_prints_no_bytes() {
        let held = Held::Present {
            bytes: b"agent_token = \"tok-person\"".to_vec(),
            uid: UID,
            mode: 0o600,
        };
        let kept = Kept {
            v: FORMAT,
            config: Some(b"tok-person".to_vec()),
            config_mode: None,
            plist: None,
            plist_mode: None,
        };
        let said = format!("{held:?} {kept:?}");
        assert!(!said.contains("tok-person"), "{said}");
    }

    #[test]
    fn the_exit_codes_are_the_tables() {
        for (exit, code) in [
            (Exit::Done, 0),
            (Exit::Error, 1),
            (Exit::Refused, 2),
            (Exit::Changed, 3),
            (Exit::NotWritable, 4),
        ] {
            assert_eq!(exit.code(), code);
            assert_eq!(Exit::from_code(code), Some(exit));
        }
        assert_eq!(Exit::from_code(5), None);
    }

    /// The real files, on a temporary home, and the real check of itself in
    /// a process of its own (the Linux lanes run these). A check that passes
    /// needs another account, so the one here is refused.
    #[cfg(unix)]
    mod on_disk {
        use super::*;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::time::Duration;

        /// Set in the process [`the_real_check_refuses_an_account_it_was_not_given`]
        /// starts again from this test binary: that process is the child.
        const AS_CHILD: &str = "ROOMLER_TEST_ONE_ROW_HOME_CHILD";

        /// The check of itself through `RealMe`: the kernel's ids and group
        /// list, and `setuid(0)`. It runs in a process of its own, this test
        /// binary started again, because the umask and the ids are the whole
        /// process's. Asked for the uid after its own, it is exit 2 with
        /// nothing written, and its line names the group list the kernel
        /// gives it; where it is not root, it cannot become root.
        #[test]
        fn the_real_check_refuses_an_account_it_was_not_given() {
            if std::env::var_os(AS_CHILD).is_some() {
                // SAFETY: reads of this process's own ids.
                let (uid, euid, gid) = unsafe { (libc::getuid(), libc::geteuid(), libc::getgid()) };
                let args = os(&child_args(uid.wrapping_add(1).max(1), gid.max(1), Verb::Read)[1..]);
                let out = run(&args, &mut RealMe, &mut RealFs);
                let groups = kernel_groups().expect("the kernel's group list");
                assert_eq!(out.exit, Exit::Refused, "{}", out.stderr);
                assert!(out.stdout.is_empty());
                assert!(
                    out.stderr.contains(&format!("groups {groups:?}, not uid")),
                    "{} / {groups:?}",
                    out.stderr
                );
                if euid != 0 {
                    assert!(RealMe.cannot_become_root());
                }
                println!("one-row-home-child|refused|{groups:?}");
                return;
            }
            let test =
                "one_row_home::tests::on_disk::the_real_check_refuses_an_account_it_was_not_given";
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture", "--test-threads=1"])
                .env(AS_CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "the child failed: {stdout}{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                stdout.contains("one-row-home-child|refused|"),
                "the child did not run the check: {stdout}"
            );
        }

        fn mode(path: &Path) -> u32 {
            std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
        }

        fn chmod(path: &Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        /// A home as the package leaves it, on disk.
        fn a_home() -> (tempfile::TempDir, Places) {
            let dir = tempfile::tempdir().unwrap();
            let at = Places::of(dir.path(), &RealFs);
            std::fs::create_dir_all(&at.folder).unwrap();
            std::fs::create_dir_all(&at.agents_dir).unwrap();
            std::fs::write(&at.config, ENROLLED).unwrap();
            chmod(&at.config, 0o600);
            std::fs::write(&at.plist, PLIST).unwrap();
            chmod(&at.plist, 0o644);
            let at = Places::of(dir.path(), &RealFs);
            (dir, at)
        }

        fn listing(dir: &Path) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        #[test]
        fn read_reports_bytes_owners_and_legacy_names() {
            let (dir, at) = a_home();
            std::fs::write(&at.legacy_plist, PLIST).unwrap();
            let out = answer(Verb::Read, dir.path(), &creds(), &[], &mut RealFs);
            assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
            let read = Reply::parse(&out.stdout, UID, GID, Verb::Read)
                .unwrap()
                .read
                .unwrap();
            let owner = std::fs::metadata(&at.config).unwrap().uid();
            assert_eq!(
                read.config,
                Held::Present {
                    bytes: ENROLLED.as_bytes().to_vec(),
                    uid: owner,
                    mode: 0o600
                }
            );
            assert_eq!(
                read.plist,
                Held::Present {
                    bytes: PLIST.to_vec(),
                    uid: owner,
                    mode: 0o644
                }
            );
            assert_eq!(read.legacy.config.kind, Kind::Absent);
            assert_eq!(
                (read.legacy.plist.kind, read.legacy.plist.uid),
                (Kind::File, Some(owner))
            );
            assert_eq!(read.segment, Segment::Current);
            assert_eq!(read.agents_dir.kind, Kind::Dir);
            assert_eq!(read.folder.uid, Some(owner));
            let home = std::fs::metadata(dir.path()).unwrap();
            assert_eq!(
                (read.home.dev, read.home.ino),
                (Some(home.dev()), Some(home.ino()))
            );
            assert!(read.home.absolute);
        }

        /// `answer` on a thread, its reply waited for 5 s at most: a call
        /// that blocks (a FIFO opened without `O_NONBLOCK`) fails the test
        /// instead of holding the lane until the job's timeout.
        fn answer_within_5s(verb: Verb, home: &Path) -> Outcome {
            let home = home.to_path_buf();
            let (said, reply) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = said.send(answer(verb, &home, &creds(), &[], &mut RealFs));
            });
            reply
                .recv_timeout(Duration::from_secs(5))
                .expect("an answer within 5 s")
        }

        #[test]
        fn a_fifo_or_an_oversize_file_is_an_error_not_a_wait() {
            use std::os::unix::ffi::OsStrExt;
            let (dir, at) = a_home();
            std::fs::remove_file(&at.config).unwrap();
            let fifo = std::ffi::CString::new(at.config.as_os_str().as_bytes()).unwrap();
            // SAFETY: a NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            std::fs::write(&at.plist, vec![b'<'; PLIST_BOUND + 1]).unwrap();
            let out = answer_within_5s(Verb::Read, dir.path());
            let read = Reply::parse(&out.stdout, UID, GID, Verb::Read)
                .unwrap()
                .read
                .unwrap();
            assert!(
                matches!(
                    read.config,
                    Held::Unreadable {
                        error: ReadError::NotRegular,
                        ..
                    }
                ),
                "{:?}",
                read.config
            );
            assert!(
                matches!(
                    read.plist,
                    Held::Unreadable {
                        error: ReadError::TooLarge,
                        ..
                    }
                ),
                "{:?}",
                read.plist
            );

            // A link is not followed.
            std::fs::remove_file(&at.plist).unwrap();
            std::os::unix::fs::symlink(dir.path().join("elsewhere"), &at.plist).unwrap();
            std::fs::write(dir.path().join("elsewhere"), PLIST).unwrap();
            assert!(matches!(
                RealFs.read(&at.plist, PLIST_BOUND),
                Held::Unreadable {
                    error: ReadError::NotRegular,
                    ..
                }
            ));

            // `convert` over the FIFO where root saw a config: changed, at
            // once, and nothing changes.
            let verb = Verb::Convert {
                config: Expect::of(Some(ENROLLED.as_bytes())),
                plist: Expect::Absent,
            };
            let out = answer_within_5s(verb, dir.path());
            assert_eq!(out.exit, Exit::Changed, "{}", out.stderr);
        }

        #[test]
        fn convert_refuses_bytes_it_was_not_shown() {
            let (dir, at) = a_home();
            let before = (listing(&at.folder), listing(&at.agents_dir));
            let verb = Verb::Convert {
                config: Expect::of(Some(b"other bytes")),
                plist: Expect::of(Some(PLIST)),
            };
            let out = answer(verb, dir.path(), &creds(), &[], &mut RealFs);
            assert_eq!(out.exit, Exit::Changed, "{}", out.stderr);
            assert_eq!((listing(&at.folder), listing(&at.agents_dir)), before);
            assert_eq!(std::fs::read(&at.config).unwrap(), ENROLLED.as_bytes());

            let verb = Verb::Convert {
                config: Expect::of(Some(ENROLLED.as_bytes())),
                plist: Expect::of(Some(PLIST)),
            };
            let out = answer(verb, dir.path(), &creds(), &[], &mut RealFs);
            assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
            let convert = Reply::parse(&out.stdout, UID, GID, verb)
                .unwrap()
                .convert
                .unwrap();
            assert!(convert.plist_removed && !at.plist.exists());
            let saved = std::fs::read(&at.config).unwrap();
            assert_eq!(convert.config_sha256, Some(sha256_hex(&saved)));
            let cfg = crate::config::load(&at.config).unwrap();
            assert!(cfg.agent_token.is_empty());
            assert_eq!(mode(&at.config), 0o600);
        }

        /// `convert` on disk: the new config and the outgoing one, kept as its
        /// `.prev`, are both 0600; the `.prev` holds the bytes it checked; a
        /// key this build does not know is still in the config; and no
        /// temporary file is left.
        #[test]
        fn convert_keeps_the_outgoing_bytes_as_prev_and_the_unknown_key() {
            let (dir, at) = a_home();
            let outgoing = format!("{ENROLLED}a_key_from_a_newer_build = 1\n");
            std::fs::write(&at.config, &outgoing).unwrap();
            chmod(&at.config, 0o600);
            let verb = Verb::Convert {
                config: Expect::of(Some(outgoing.as_bytes())),
                plist: Expect::of(Some(PLIST)),
            };
            let out = answer(verb, dir.path(), &creds(), &[], &mut RealFs);
            assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
            let prev = crate::config::prev_path(&at.config);
            assert_eq!(mode(&at.config), 0o600);
            assert_eq!(mode(&prev), 0o600);
            assert_eq!(std::fs::read(&prev).unwrap(), outgoing.as_bytes());
            let saved = std::fs::read_to_string(&at.config).unwrap();
            assert!(saved.contains("a_key_from_a_newer_build = 1"), "{saved}");
            assert!(
                crate::config::load(&at.config)
                    .unwrap()
                    .agent_token
                    .is_empty()
            );
            let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
            assert_eq!(
                listing(&at.folder),
                [name(&at.config), name(&prev)],
                "no temporary file left"
            );
        }

        #[test]
        fn put_back_writes_0644_and_makes_agents_dir_0755() {
            let dir = tempfile::tempdir().unwrap();
            let at = Places::of(dir.path(), &RealFs);
            std::fs::create_dir_all(&at.library).unwrap();
            let stdin = kept(
                Some(ENROLLED.as_bytes()),
                Some(0o640),
                Some(PLIST),
                Some(0o664),
            );
            let out = answer(Verb::PutBack, dir.path(), &creds(), &stdin, &mut RealFs);
            assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
            assert_eq!(mode(&at.agents_dir), 0o755);
            assert_eq!(mode(&at.plist), 0o644);
            assert_eq!(mode(&at.config), 0o600);
            assert_eq!(mode(&at.folder), 0o700);
            assert_eq!(std::fs::read(&at.plist).unwrap(), PLIST);
            assert_eq!(std::fs::read(&at.config).unwrap(), ENROLLED.as_bytes());
            assert_eq!(
                listing(&at.agents_dir),
                [AGENT_PLIST],
                "no temporary file left"
            );

            let stdin = kept(None, None, Some(PLIST), None);
            let out = answer(Verb::PutBack, dir.path(), &creds(), &stdin, &mut RealFs);
            assert_eq!(out.exit, Exit::Done, "{}", out.stderr);
            assert_eq!(mode(&at.plist), 0o644, "0644 when no mode was recorded");
        }
    }
}

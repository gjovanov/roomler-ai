// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3688 — a received file's staging on Linux, held by handles.
//!
//! An upload stages beside the folder it lands in, as
//! `<base>/.roomler-partial/<id>/{meta.json,data}`. Everything there is its
//! stager's own: the thread's fsuid (`fs_identity::fsuid`), which is the
//! person's for the person at seat0 and the daemon's otherwise, and nobody
//! else may write it. It is made so with explicit modes, whatever the umask,
//! and checked so on every handle opened, a link never followed:
//!
//! * `.roomler-partial` is made 0700, or used when it is the stager's own
//!   (made 0700 again through its handle when it is open to others); a link,
//!   or another account's, is refused and left as it is.
//! * `<id>` is made 0700, `meta.json` 0600, `data` 0600 for the daemon alone
//!   and 0644 otherwise.
//! * A resume and the startup sweep trust a partial only when its folder and
//!   both files pass the same checks ([`reopen`], [`sweep`]).
//!
//! A transfer holds its folders ([`Held`]) from its begin, or its resume,
//! through its end: a folder moved aside mid-upload and another put in its
//! place changes nothing, because the end renames from the held `<id>` into
//! the held target ([`place`]).

use anyhow::{Context, Result, anyhow};
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::PartialMeta;

/// The staging folder's name, beside the folder an upload lands in.
pub(super) const STAGING: &str = ".roomler-partial";

/// The most of a partial's notes ever read: they are a few hundred bytes.
const MAX_NOTES: u64 = 64 * 1024;

/// A staged upload's folders, held from its begin (or resume) to its end.
pub(crate) struct Held {
    /// The folder `.roomler-partial` sits in.
    base: File,
    /// `<base>/.roomler-partial`, the stager's own.
    staging: File,
    /// `<base>/.roomler-partial/<id>`, the stager's own.
    dir: File,
    /// `<id>`: the name `dir` has in `staging`.
    id: String,
    /// The folder the file is placed in: `base`, unless a resume placed it
    /// where its notes say ([`Held::place_in`]).
    target: File,
}

impl Held {
    /// The folder the file is placed in.
    pub(super) fn target(&self) -> &File {
        &self.target
    }

    /// Place the file in `folder` instead of beside its staging: a resume
    /// of the person's, or a per-user daemon's, partial (FR-97 P3d).
    pub(super) fn place_in(&mut self, folder: File) {
        self.target = folder;
    }
}

/// The thread's fsuid: whoever this step runs as.
fn stager() -> u32 {
    crate::fs_identity::fsuid()
}

fn c_name(name: impl AsRef<OsStr>) -> io::Result<CString> {
    CString::new(name.as_ref().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// `open(path)`, close-on-exec.
fn open_path(path: &Path, flags: libc::c_int) -> io::Result<File> {
    let path = c_name(path)?;
    // SAFETY: a NUL-terminated path; the descriptor returned is ours alone.
    let fd = cvt(unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) })?;
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// `openat(dir, name)`, close-on-exec.
fn open_at(dir: &File, name: impl AsRef<OsStr>, flags: libc::c_int, mode: u32) -> io::Result<File> {
    let name = c_name(name)?;
    // SAFETY: a folder handle we hold and a NUL-terminated name; the
    // descriptor returned is ours alone.
    let fd = cvt(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    })?;
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn mkdir_at(dir: &File, name: impl AsRef<OsStr>, mode: u32) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: a folder handle we hold and a NUL-terminated name.
    cvt(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) }).map(drop)
}

fn unlink_at(dir: &File, name: impl AsRef<OsStr>, flags: libc::c_int) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: a folder handle we hold and a NUL-terminated name.
    cvt(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) }).map(drop)
}

fn rename_at(from: &File, from_name: &str, to: &File, to_name: &str) -> io::Result<()> {
    let (from_name, to_name) = (c_name(from_name)?, c_name(to_name)?);
    // SAFETY: two folder handles we hold and two NUL-terminated names.
    cvt(unsafe {
        libc::renameat(
            from.as_raw_fd(),
            from_name.as_ptr(),
            to.as_raw_fd(),
            to_name.as_ptr(),
        )
    })
    .map(drop)
}

/// `name` in `dir` itself, a link not followed: `None` when nothing is there.
fn stat_at(dir: &File, name: &str) -> io::Result<Option<libc::stat>> {
    let name = c_name(name)?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a folder handle we hold, a NUL-terminated name and room for
    // the answer, which is read only when the call succeeded.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        // SAFETY: filled in by the successful call above.
        return Ok(Some(unsafe { st.assume_init() }));
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(e)
    }
}

/// A folder by its path, links followed, held for use as the base of
/// `*at` calls: `O_PATH`, so a folder the writer may write but not list
/// still opens.
pub(super) fn open_folder(path: &Path) -> io::Result<File> {
    open_path(path, libc::O_PATH | libc::O_DIRECTORY)
}

/// #3688 rule 1 — whether the folder `held` may receive the daemon's own
/// files: a folder, `uid`'s, and, for root (`strict`), one no other account
/// may write; for a per-user daemon, one not every account may write
/// (Ubuntu's private-group umask makes `~/Downloads` 0775). `Err` says why
/// not, in the viewer's words.
pub(super) fn may_receive(held: &File, uid: u32, strict: bool) -> Result<(), String> {
    let md = held
        .metadata()
        .map_err(|e| format!("it could not be read ({e})"))?;
    if !md.is_dir() {
        return Err("it is not a folder".into());
    }
    if md.uid() != uid {
        return Err("it belongs to another account".into());
    }
    if strict && md.mode() & 0o022 != 0 {
        return Err("another account can write to it".into());
    }
    if md.mode() & 0o002 != 0 {
        return Err("every account can write to it".into());
    }
    Ok(())
}

/// Rules 2 and 3 — whether `held` is a folder (`dir`) or a plain file that
/// is the stager's own and that nobody else may write.
fn own(held: &File, dir: bool) -> Result<(), &'static str> {
    let md = held.metadata().map_err(|_| "it could not be read")?;
    if dir && !md.is_dir() {
        return Err("it is not a folder");
    }
    if !dir && !md.is_file() {
        return Err("it is not a plain file");
    }
    if md.uid() != stager() {
        return Err("it belongs to another account");
    }
    if md.mode() & 0o022 != 0 {
        return Err("another account can write to it");
    }
    Ok(())
}

/// Rule 2 — `<base>/.roomler-partial`, made 0700 when `create` and it is
/// not there, opened without following a link and checked: the stager's
/// own, or refused with a reason and left as it is; open to others, made
/// 0700 through its handle (R1: one a per-user daemon made under umask 0002
/// before this rule).
fn open_staging(base: &File, base_path: &Path, create: bool) -> Result<File> {
    let shown = base_path.join(STAGING);
    if create {
        match mkdir_at(base, STAGING, 0o700) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
            Err(e) => {
                return Err(anyhow::Error::from(e).context(format!("creating {}", shown.display())));
            }
        }
    }
    let staging = match open_at(
        base,
        STAGING,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    ) {
        Ok(staging) => staging,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
            return Err(anyhow!(
                "{} is not a folder, so file transfer does not stage there",
                shown.display()
            ));
        }
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("opening {}", shown.display())));
        }
    };
    let md = staging
        .metadata()
        .with_context(|| format!("reading {}", shown.display()))?;
    if md.uid() != stager() {
        return Err(anyhow!(
            "{} belongs to another account, so file transfer does not stage there",
            shown.display()
        ));
    }
    if md.mode() & 0o022 != 0 {
        // SAFETY: a folder handle we hold.
        cvt(unsafe { libc::fchmod(staging.as_raw_fd(), 0o700) })
            .with_context(|| format!("making {} its own again", shown.display()))?;
        tracing::info!(
            folder = %shown.display(),
            was = format!("{:o}", md.mode() & 0o7777),
            "files: the staging folder was open to other accounts — made 0700"
        );
    }
    Ok(staging)
}

/// Rule 2 — stage upload `id` beside `base` (held, at `base_path`): its
/// `<id>` made 0700, its notes (`meta`) written 0600, its `data` made
/// `data_mode`, each new and opened without following a link. Returns the
/// handles, and `data` open for writing.
pub(super) fn stage(
    base: File,
    base_path: &Path,
    id: &str,
    meta: &[u8],
    data_mode: u32,
) -> Result<(Held, File)> {
    let staging = open_staging(&base, base_path, true)?;
    let shown = base_path.join(STAGING).join(id);
    match mkdir_at(&staging, id, 0o700) {
        Ok(()) => {}
        // B1 — never adopted: the browser sends `files:resume` instead.
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
            return Err(anyhow!(
                "id {id} already has partial state; send files:resume"
            ));
        }
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("creating {}", shown.display())));
        }
    }
    let dir = open_at(
        &staging,
        id,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )
    .with_context(|| format!("opening {}", shown.display()))?;
    own(&dir, true).map_err(|why| anyhow!("{}: {why}", shown.display()))?;
    let new = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW;
    open_at(&dir, "meta.json", new, 0o600)
        .and_then(|mut notes| notes.write_all(meta))
        .with_context(|| format!("writing {}", shown.join("meta.json").display()))?;
    let data = open_at(&dir, "data", new, data_mode)
        .with_context(|| format!("creating {}", shown.join("data").display()))?;
    let target = base.try_clone().context("holding the target folder")?;
    let held = Held {
        base,
        staging,
        dir,
        id: id.to_string(),
        target,
    };
    Ok((held, data))
}

/// Rule 3 — a partial walked again from its notes' path
/// (`<base>/.roomler-partial/<id>/meta.json`): `<base>` (links followed: its
/// own rule), `.roomler-partial` (rule 2's check), and `<id>` without
/// following a link, the stager's own. `Err` = not a partial this stager may
/// trust, and why, for the log. Returns the handles and `<base>`'s path.
pub(super) fn reopen(meta_path: &Path, id: &str) -> Result<(Held, PathBuf)> {
    let dir_path = meta_path
        .parent()
        .filter(|dir| dir.file_name() == Some(OsStr::new(id)))
        .ok_or_else(|| anyhow!("{} is not a partial's notes", meta_path.display()))?;
    let base_path = dir_path
        .parent()
        .filter(|staging| staging.file_name() == Some(OsStr::new(STAGING)))
        .and_then(Path::parent)
        .ok_or_else(|| anyhow!("{} is not beside a folder", dir_path.display()))?;
    let base =
        open_folder(base_path).with_context(|| format!("opening {}", base_path.display()))?;
    let staging = open_staging(&base, base_path, false)?;
    let dir = open_at(
        &staging,
        id,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )
    .with_context(|| format!("opening {}", dir_path.display()))?;
    own(&dir, true).map_err(|why| anyhow!("{}: {why}", dir_path.display()))?;
    let target = base.try_clone().context("holding the target folder")?;
    let held = Held {
        base,
        staging,
        dir,
        id: id.to_string(),
        target,
    };
    Ok((held, base_path.to_path_buf()))
}

/// Rule 3 — a partial's notes, through a handle opened without following a
/// link (nor waiting on a FIFO): the stager's own plain file.
pub(super) fn read_notes(held: &Held) -> Result<Vec<u8>> {
    let notes = open_at(
        &held.dir,
        "meta.json",
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    )
    .context("opening meta.json")?;
    own(&notes, false).map_err(|why| anyhow!("meta.json: {why}"))?;
    let mut bytes = Vec::new();
    notes
        .take(MAX_NOTES)
        .read_to_end(&mut bytes)
        .context("reading meta.json")?;
    Ok(bytes)
}

/// Rule 3 — a partial's `data`, opened once without following a link, the
/// stager's own plain file, for appending; with its size. The caller
/// truncates it on this very handle.
pub(super) fn reopen_data(held: &Held) -> Result<(File, u64)> {
    let data = open_at(
        &held.dir,
        "data",
        libc::O_WRONLY | libc::O_APPEND | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    )
    .context("opening data")?;
    own(&data, false).map_err(|why| anyhow!("data: {why}"))?;
    let size = data.metadata().context("reading data")?.len();
    Ok((data, size))
}

/// A name nothing in `dir` has yet, a link counted (the M4 fix's
/// `unique_path`, on the held folder): `name`, else `stem (N).ext`.
fn unique_name_at(dir: &File, name: &str) -> String {
    // What cannot be looked at counts as taken: never placed over it.
    let taken = |candidate: &str| !matches!(stat_at(dir, candidate), Ok(None));
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = super::split_stem_ext(name);
    for n in 2..1000u32 {
        let candidate = if ext.is_empty() {
            format!("{stem} ({n})")
        } else {
            format!("{stem} ({n}).{ext}")
        };
        if !taken(&candidate) {
            return candidate;
        }
    }
    name.to_string()
}

/// Rule 6 — place the received file: `data` renamed from the held `<id>`
/// into the held target, or into a folder upload's subfolders, each made and
/// opened from the one before it (`no_links`: a link among them refuses),
/// under a name nothing there has yet; then the staging goes. `shown` is the
/// target's path, for the path the viewer is told. On a failure the staging
/// stays, for a resume.
pub(super) fn place(
    held: Held,
    shown: &Path,
    folders: &[String],
    name: &str,
    no_links: bool,
) -> Result<PathBuf> {
    let mut leaf_path = shown.to_path_buf();
    let mut leaf: Option<File> = None;
    for folder in folders {
        let parent = leaf.as_ref().unwrap_or(&held.target);
        leaf_path.push(folder);
        match mkdir_at(parent, folder, 0o777) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
            Err(e) => {
                return Err(anyhow::Error::from(e)
                    .context(format!("creating final parent {}", leaf_path.display())));
            }
        }
        let flags = if no_links {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW
        } else {
            libc::O_PATH | libc::O_DIRECTORY
        };
        let next = match open_at(parent, folder, flags, 0) {
            Ok(next) => next,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                return Err(anyhow!(
                    "{} is not a folder, so file transfer does not place the file there",
                    leaf_path.display()
                ));
            }
            Err(e) => {
                return Err(
                    anyhow::Error::from(e).context(format!("opening {}", leaf_path.display()))
                );
            }
        };
        leaf = Some(next);
    }
    let leaf = leaf.as_ref().unwrap_or(&held.target);
    let name = unique_name_at(leaf, name);
    let placed = leaf_path.join(&name);
    match rename_at(&held.dir, "data", leaf, &name) {
        Ok(()) => {}
        // A subfolder on another file system: copied across, through handles.
        Err(e) if super::is_cross_volume_error(&e) => copy_across(&held, leaf, &name, &placed)?,
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!(
                "renaming the received file to {}",
                placed.display()
            )));
        }
    }
    discard(held);
    Ok(placed)
}

/// `data` copied into a new file `name` in `leaf`, with the staged file's
/// mode; the copy removed when it fails, the staging kept.
fn copy_across(held: &Held, leaf: &File, name: &str, placed: &Path) -> Result<()> {
    tracing::info!(to = %placed.display(), "files: cross-volume staging → final; copy+remove fallback");
    let source = open_at(
        &held.dir,
        "data",
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    )
    .context("opening data")?;
    own(&source, false).map_err(|why| anyhow!("data: {why}"))?;
    let mode = source.metadata().context("reading data")?.mode() & 0o777;
    let mut out = open_at(
        leaf,
        name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
        mode,
    )
    .with_context(|| format!("creating {}", placed.display()))?;
    if let Err(e) = super::copy_into(source, &mut out) {
        drop(out);
        let _ = unlink_at(leaf, name, 0);
        return Err(anyhow::Error::from(e).context(format!("copying into {}", placed.display())));
    }
    let _ = unlink_at(&held.dir, "data", 0);
    Ok(())
}

/// The staging of a transfer that is over or cancelled: its `data` (if
/// still there), its notes, `<id>`, and `.roomler-partial` once empty.
/// Best-effort, through the held handles, nothing followed.
pub(super) fn discard(held: Held) {
    let _ = unlink_at(&held.dir, "data", 0);
    let _ = unlink_at(&held.dir, "meta.json", 0);
    let _ = unlink_at(&held.staging, &held.id, libc::AT_REMOVEDIR);
    let _ = unlink_at(&held.base, STAGING, libc::AT_REMOVEDIR);
}

/// What the sweep finds in a `<id>` that is its stager's own.
enum Found {
    /// It passes rule 3 in full, and these are its notes.
    Trusted(PartialMeta),
    /// The folder passes, but its notes are missing or do not parse.
    Corrupt,
    /// Something in it fails rule 3.
    Untrusted,
}

/// Whether `name` in `dir` is the stager's own plain file nobody else may
/// write, a link not followed.
fn own_file_at(dir: &File, name: &str) -> bool {
    matches!(
        stat_at(dir, name),
        Ok(Some(st)) if st.st_mode & libc::S_IFMT == libc::S_IFREG
            && st.st_uid == stager()
            && st.st_mode & 0o022 == 0
    )
}

fn found(dir: &File) -> Found {
    if own(dir, true).is_err() {
        return Found::Untrusted;
    }
    let notes = match open_at(
        dir,
        "meta.json",
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    ) {
        Ok(notes) => notes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Found::Corrupt,
        Err(_) => return Found::Untrusted,
    };
    if own(&notes, false).is_err() {
        return Found::Untrusted;
    }
    let mut bytes = Vec::new();
    if notes.take(MAX_NOTES).read_to_end(&mut bytes).is_err() {
        return Found::Untrusted;
    }
    let Ok(meta) = serde_json::from_slice::<PartialMeta>(&bytes) else {
        return Found::Corrupt;
    };
    if own_file_at(dir, "data") {
        Found::Trusted(meta)
    } else {
        Found::Untrusted
    }
}

/// One partial removed through handles: each entry of the held `<id>` (a
/// link removed itself, never followed; a subfolder left), then `<id>`.
fn remove(staging: &File, id: &str, dir: &File) {
    if let Ok(entries) = std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd())) {
        for entry in entries.flatten() {
            let _ = unlink_at(dir, entry.file_name(), 0);
        }
    }
    let _ = unlink_at(staging, id, libc::AT_REMOVEDIR);
}

/// Rule 3, R1 — the startup sweep of one staging folder (`dir`: a
/// `.roomler-partial`, or a staging root whose children are partials).
/// `dir` must be the stager's own (made 0700 when open to others); every
/// child opened without following a link. A partial passing rule 3 in full
/// is a survivor `(id, its notes' path)`, or removed when older than `ttl`
/// by its notes; the stager's own folder whose notes are missing or corrupt
/// is removed, as before; its own folder that fails rule 3 otherwise is
/// never a survivor and is removed once its folder is older than `ttl`; a
/// link, or anything another account owns, is left as it is. Returns the
/// survivors and how many were removed.
pub(super) fn sweep(dir_path: &Path, now: i64, ttl: i64) -> (Vec<(String, PathBuf)>, usize) {
    let mut survivors = Vec::new();
    let mut swept = 0usize;
    let staging = match open_path(
        dir_path,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    ) {
        Ok(staging) => staging,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            tracing::debug!(dir = %dir_path.display(), "sweep_orphans: dir absent; nothing to do");
            return (survivors, 0);
        }
        Err(e) => {
            tracing::warn!(dir = %dir_path.display(), %e, "sweep_orphans: not a folder of this stager's own; left as it is");
            return (survivors, 0);
        }
    };
    let md = match staging.metadata() {
        Ok(md) => md,
        Err(e) => {
            tracing::warn!(dir = %dir_path.display(), %e, "sweep_orphans: cannot read the staging folder");
            return (survivors, 0);
        }
    };
    if md.uid() != stager() {
        tracing::warn!(dir = %dir_path.display(), owner = md.uid(), "sweep_orphans: the staging folder belongs to another account; left as it is");
        return (survivors, 0);
    }
    if md.mode() & 0o022 != 0 {
        // SAFETY: a folder handle we hold.
        if let Err(e) = cvt(unsafe { libc::fchmod(staging.as_raw_fd(), 0o700) }) {
            tracing::warn!(dir = %dir_path.display(), %e, "sweep_orphans: the staging folder is open to other accounts and could not be made 0700; left as it is");
            return (survivors, 0);
        }
        tracing::info!(dir = %dir_path.display(), "sweep_orphans: the staging folder was open to other accounts — made 0700");
    }
    let entries = match std::fs::read_dir(format!("/proc/self/fd/{}", staging.as_raw_fd())) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(dir = %dir_path.display(), %e, "sweep_orphans: read_dir failed");
            return (survivors, 0);
        }
    };
    for entry in entries.flatten() {
        let Some(id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // Only a name an upload stages under (#3688, `transfer_id`).
        if super::transfer_id(&id).is_err() {
            continue;
        }
        // A link, or not a folder: left as it is.
        let Ok(dir) = open_at(
            &staging,
            &id,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        ) else {
            continue;
        };
        let Ok(dmd) = dir.metadata() else {
            continue;
        };
        if dmd.uid() != stager() {
            tracing::warn!(id = %id, owner = dmd.uid(), "sweep_orphans: a partial another account owns; left as it is");
            continue;
        }
        match found(&dir) {
            Found::Trusted(meta) if now - meta.created_at_unix > ttl => {
                tracing::info!(
                    id = %id,
                    age_h = (now - meta.created_at_unix) / 3600,
                    "sweep_orphans: removing orphaned partial"
                );
                remove(&staging, &id, &dir);
                swept += 1;
            }
            Found::Trusted(_) => survivors.push((id.clone(), dir_path.join(&id).join("meta.json"))),
            Found::Corrupt => {
                tracing::warn!(id = %id, "sweep_orphans: missing or corrupt meta.json; removing");
                remove(&staging, &id, &dir);
                swept += 1;
            }
            Found::Untrusted if now - dmd.mtime() > ttl => {
                tracing::info!(id = %id, "sweep_orphans: removing a stale partial that is not its stager's own alone");
                remove(&staging, &id, &dir);
                swept += 1;
            }
            Found::Untrusted => {
                tracing::warn!(id = %id, "sweep_orphans: a partial another account may write, or with a link in it; not registered");
            }
        }
    }
    (survivors, swept)
}

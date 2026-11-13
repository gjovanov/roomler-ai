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
//!
//! A folder on a file system that keeps no Unix owners or modes (a FAT
//! disk, a share mounted with fixed modes: [`keeps_modes`]) cannot hold a
//! staging folder of the stager's own. An upload into it stages in the
//! stager's own private folder instead, on a file system that does keep
//! them ([`stage_privately`]), and its end copies the file across.

use anyhow::{Context, Result, anyhow};
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use super::PartialMeta;

/// The staging folder's name, beside the folder an upload lands in.
pub(super) const STAGING: &str = ".roomler-partial";

/// The most of a partial's notes ever read: they are a few hundred bytes.
const MAX_NOTES: u64 = 64 * 1024;

/// A staged upload's folders, held from its begin (or resume) to its end.
pub(crate) struct Held {
    /// The folder `.roomler-partial` sits in, when the upload stages beside
    /// the folder it lands in; `None` when it stages in the stager's private
    /// folder ([`stage_privately`]), whose file the end copies across.
    beside: Option<File>,
    /// `<base>/.roomler-partial`, or the private folder: the stager's own.
    staging: File,
    /// `<id>` in `staging`, the stager's own.
    dir: File,
    /// `<id>`: the name `dir` has in `staging`.
    id: String,
    /// `dir`'s path, as the registry and the log name it.
    dir_path: PathBuf,
    /// The folder the file is placed in: the one it stages beside, or the
    /// one it staged privately for, unless a resume placed it where its
    /// notes say ([`Held::place_in`]). `None` only for a private partial
    /// walked again, until its resume names its folder.
    target: Option<File>,
}

impl Held {
    /// The folder the file is placed in.
    pub(super) fn target(&self) -> Option<&File> {
        self.target.as_ref()
    }

    /// Place the file in `folder`: a resume of the person's, or a per-user
    /// daemon's, partial (FR-97 P3d), or of a private partial.
    pub(super) fn place_in(&mut self, folder: File) {
        self.target = Some(folder);
    }

    /// The partial's own folder (`<id>`), by path.
    pub(super) fn dir_path(&self) -> &Path {
        &self.dir_path
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
/// files: a folder, never the temp dir; for root (`strict`), `uid`'s and
/// one no other account may write; for a per-user daemon, `uid`'s and one
/// not every account may write (Ubuntu's private-group umask makes
/// `~/Downloads` 0775). A file system that keeps no Unix owners or modes
/// ([`keeps_modes`]) shows every folder as some fixed owner and mode (a FAT
/// disk 0777, a VirtualBox shared folder `root:vboxsf`, a share mounted
/// with fixed modes), which say nothing: there a per-user
/// daemon's folder needs only to be one it can write into ([`writable`]),
/// since such an upload stages privately and is placed without following a
/// link. Root stays strict. `Err` says why not, in the viewer's words.
pub(super) fn may_receive(held: &File, uid: u32, strict: bool) -> Result<(), String> {
    let md = held
        .metadata()
        .map_err(|e| format!("it could not be read ({e})"))?;
    if !md.is_dir() {
        return Err("it is not a folder".into());
    }
    if is_the_temp_dir(&md) {
        return Err("it is the temp folder".into());
    }
    let theirs = md.uid() == uid;
    if strict {
        if !theirs {
            return Err("it belongs to another account".into());
        }
        if md.mode() & 0o022 != 0 {
            return Err("another account can write to it".into());
        }
        return Ok(());
    }
    if theirs && md.mode() & 0o002 == 0 {
        return Ok(());
    }
    if keeps_modes(held) {
        return Err(if theirs {
            "every account can write to it"
        } else {
            "it belongs to another account"
        }
        .into());
    }
    if !writable(held) {
        return Err("file transfer cannot write into it".into());
    }
    Ok(())
}

/// #3688 — whether the stager may make an entry in the folder `held`, as the
/// kernel answers for the thread's own fsuid, fsgid and groups
/// (`faccessat2` with `AT_EACCESS`), with nothing made. Where that call is
/// missing (a kernel before 5.8) or refused outright (a seccomp profile
/// older than it answers `EPERM`), an empty folder made there and removed
/// at once answers instead ([`probe`]).
fn writable(held: &File) -> bool {
    // SAFETY: a handle we hold, an empty NUL-terminated path and plain
    // flags; the call writes nothing back.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            held.as_raw_fd(),
            c"".as_ptr(),
            libc::W_OK | libc::X_OK,
            libc::AT_EACCESS | libc::AT_EMPTY_PATH,
        )
    };
    if rc == 0 {
        return true;
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ENOSYS | libc::EPERM | libc::EINVAL) => probe(held).is_some(),
        _ => false,
    }
}

/// Whether `md` is the temp dir's (`std::env::temp_dir()`), which no rule
/// gives as a folder to receive into.
fn is_the_temp_dir(md: &std::fs::Metadata) -> bool {
    std::fs::metadata(std::env::temp_dir())
        .is_ok_and(|temp| temp.dev() == md.dev() && temp.ino() == md.ino())
}

/// What [`keeps_modes`] found, by mount ([`mount_id`]), file system type
/// and stager.
type Kept = (((u64, bool), i64, u32), bool);

static KEEPS_MODES: Mutex<Vec<Kept>> = Mutex::new(Vec::new());

/// The name a probe folder starts with ([`probe`], [`sweep_probes`]).
const PROBE: &str = ".roomler-probe-";

/// R4 — whether the file system the folder `held` sits on keeps Unix owners
/// and modes for the stager: a folder made there 0700 reads back the
/// stager's own, with no write bit for anyone else (after one `fchmod`,
/// which some file systems need). A FAT or exFAT disk, NTFS without
/// permissions, WSL's drive mounts without metadata, a share mounted with
/// fixed modes keep neither: they show every folder with one owner and
/// mode, so a staging folder beside the target could not be the stager's
/// own, and an upload into it stages in the stager's private folder
/// ([`stage_privately`]). Asked by making an empty folder in `held` under a
/// random name and removing it at once ([`probe`]; nothing it makes stays,
/// and one a crash left is swept: [`sweep_probes`]). Only a definite answer
/// is remembered, per mount, file system type and stager; anything else
/// (the folder could not be made, did not open as made, or reads neither
/// way) answers yes, so the folder's owner and modes still count, and is
/// asked again next time.
pub(super) fn keeps_modes(held: &File) -> bool {
    #[cfg(test)]
    if let Some(kept) = super::stand_in::modes_kept_now() {
        return kept;
    }
    let key = mount_id(held).map(|mount| (mount, fs_type(held), stager()));
    let known = |found: &[Kept]| {
        let key = key?;
        found.iter().find(|(k, _)| *k == key).map(|(_, kept)| *kept)
    };
    if let Some(kept) = known(&KEEPS_MODES.lock().unwrap_or_else(PoisonError::into_inner)) {
        return kept;
    }
    let Some(kept) = probe(held) else {
        return true;
    };
    if !kept {
        tracing::info!(
            "files: a folder's file system keeps no Unix owners or modes — uploads into it stage \
             in the writer's own private folder"
        );
    }
    let mut found = KEEPS_MODES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(key) = key
        && known(&found).is_none()
    {
        found.push((key, kept));
    }
    kept
}

/// What [`keeps_modes`] found, forgotten: a test whose probe met another
/// account leaves no answer behind for the tests beside it.
#[cfg(test)]
pub(super) fn forget_what_keeps_modes() {
    KEEPS_MODES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

/// The mount `held` sits on: its unique ID where the kernel keeps one
/// (`STATX_MNT_ID_UNIQUE`, Linux 6.8; never reused), else its mount ID
/// (`STATX_MNT_ID`, Linux 5.8, else `mnt_id:` in `/proc/self/fdinfo`), which
/// a mount made after this one is gone may reuse. Asked through the system
/// call itself, so no newer C library is needed. `None` when none can be
/// read: then nothing is remembered.
fn mount_id(held: &File) -> Option<(u64, bool)> {
    const STATX_MNT_ID_UNIQUE: u32 = 0x4000;
    let mut stx = std::mem::MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: a handle we hold, an empty NUL-terminated path, and room for
    // the answer, read only when the call succeeded.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_statx,
            held.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_STATX_SYNC_AS_STAT,
            libc::STATX_MNT_ID | STATX_MNT_ID_UNIQUE,
            stx.as_mut_ptr(),
        )
    };
    if rc == 0 {
        // SAFETY: filled in by the successful call above (zeroed before).
        let stx = unsafe { stx.assume_init() };
        if stx.stx_mask & STATX_MNT_ID_UNIQUE != 0 {
            return Some((stx.stx_mnt_id, true));
        }
        if stx.stx_mask & libc::STATX_MNT_ID != 0 {
            return Some((stx.stx_mnt_id, false));
        }
    }
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", held.as_raw_fd())).ok()?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:")?.trim().parse().ok())
        .map(|id| (id, false))
}

/// #3734 — whether the handles `a` and `b` hold one folder: the same device
/// and inode, on the same mount ([`mount_id`]). Only then does
/// [`may_receive`] answer for one as it would for the other: through another
/// mount, one inode can show another owner (an idmapped mount) or be
/// read-only ([`writable`]). A handle whose file or mount cannot be read
/// holds no folder in common with another.
pub(super) fn same_folder(a: &File, b: &File) -> bool {
    let (Ok(x), Ok(y)) = (a.metadata(), b.metadata()) else {
        return false;
    };
    x.dev() == y.dev()
        && x.ino() == y.ino()
        && mount_id(a).is_some_and(|mount| mount_id(b) == Some(mount))
}

/// R5 — the empty probe folders [`keeps_modes`] left in `folder` (a crash
/// between a probe's make and its removal), removed through a handle on
/// `folder` (its links followed: the drop folder's own rule), each looked
/// at without following a link. One goes when it is a folder that is the
/// stager's own, or reads exactly like `folder` as a probe does where no
/// modes are kept, and is empty (the removal refuses one that is not); a
/// link, anything with something in it, and another account's stay.
/// Returns how many went.
pub(super) fn sweep_probes(folder: &Path) -> usize {
    let Ok(base) = open_path(folder, libc::O_RDONLY | libc::O_DIRECTORY) else {
        return 0;
    };
    let (Ok(md), Ok(entries)) = (
        base.metadata(),
        std::fs::read_dir(format!("/proc/self/fd/{}", base.as_raw_fd())),
    ) else {
        return 0;
    };
    let mut swept = 0;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !name.starts_with(PROBE) {
            continue;
        }
        let Ok(Some(st)) = stat_at(&base, &name) else {
            continue;
        };
        let its_own = st.st_uid == stager()
            || (st.st_uid == md.uid() && st.st_mode & 0o7777 == md.mode() & 0o7777);
        if st.st_mode & libc::S_IFMT == libc::S_IFDIR
            && its_own
            && unlink_at(&base, &name, libc::AT_REMOVEDIR).is_ok()
        {
            swept += 1;
        }
    }
    if swept > 0 {
        tracing::info!(folder = %folder.display(), swept, "sweep_orphans: removed empty probe folders left behind");
    }
    swept
}

/// The type of the file system `held` sits on (`fstatfs`), 0 if unread.
fn fs_type(held: &File) -> i64 {
    let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: a handle we hold and room for the answer, read only when the
    // call succeeded.
    if unsafe { libc::fstatfs(held.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return 0;
    }
    // SAFETY: filled in by the successful call above.
    #[allow(clippy::unnecessary_cast)]
    let kind = unsafe { st.assume_init() }.f_type as i64;
    kind
}

/// A probe folder's name: [`PROBE`] and 16 random hex digits (`getrandom`,
/// through the system call), so nobody can make it first. `None` when no
/// random bytes come.
fn probe_name() -> Option<String> {
    let mut bytes = [0u8; 8];
    // SAFETY: room for the bytes asked for, which the call fills.
    let got = unsafe { libc::syscall(libc::SYS_getrandom, bytes.as_mut_ptr(), bytes.len(), 0) };
    (got == bytes.len() as libc::c_long)
        .then(|| format!("{PROBE}{:016x}", u64::from_ne_bytes(bytes)))
}

/// [`keeps_modes`]'s question, asked (R5): an empty folder made 0700 in
/// `held` under a random name ([`probe_name`]; another one when it is
/// taken), opened without following a link, read back ([`read_back`]), and
/// removed. `None`, no answer: it could not be made, or did not open as
/// made (another account may have moved it, or put a folder of its own in
/// its place), or reads neither way.
fn probe(held: &File) -> Option<bool> {
    let target = held.metadata().ok()?;
    let mut made = None;
    for _ in 0..4 {
        let name = probe_name()?;
        match mkdir_at(held, &name, 0o700) {
            Ok(()) => {
                made = Some(name);
                break;
            }
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
            Err(_) => return None,
        }
    }
    let name = made?;
    #[cfg(test)]
    super::stand_in::the_probe_is_made(
        &Path::new(&format!("/proc/self/fd/{}", held.as_raw_fd())).join(&name),
    );
    let kept = open_at(
        held,
        &name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )
    .ok()
    .and_then(|folder| read_back(&folder, &target));
    let _ = unlink_at(held, &name, libc::AT_REMOVEDIR);
    kept
}

/// What the folder [`probe`] made says, opened (`made`), against the folder
/// it was made in (`target`): `Some(true)` when it reads as the stager's own
/// and nobody else may write it, after one `fchmod` (which some file systems
/// need) made only when it reads as the stager's; `Some(false)` when it
/// reads exactly like `target`, the same owner and permission bits, as a
/// file system that keeps no modes shows every folder; else `None`.
fn read_back(made: &File, target: &std::fs::Metadata) -> Option<bool> {
    if own(made, true).is_ok() {
        return Some(true);
    }
    if made.metadata().ok()?.uid() == stager() {
        // SAFETY: a folder handle we hold.
        unsafe { libc::fchmod(made.as_raw_fd(), 0o700) };
        if own(made, true).is_ok() {
            return Some(true);
        }
    }
    let md = made.metadata().ok()?;
    (md.is_dir() && md.uid() == target.uid() && md.mode() & 0o7777 == target.mode() & 0o7777)
        .then_some(false)
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
    open_own(base, base_path, OsStr::new(STAGING), create)
}

/// Rule 2 — folder `name` in `base` (held, at `base_path`): `.roomler-partial`,
/// or a private staging folder. Made 0700 when `create` and it is not there,
/// opened without following a link and checked: the stager's own, or refused
/// with a reason and left as it is; the stager's own but open to others, made
/// 0700 through its handle, and refused if that does not hold.
fn open_own(base: &File, base_path: &Path, name: &OsStr, create: bool) -> Result<File> {
    let shown = base_path.join(name);
    // Twice at most when making it: another upload's end removes an empty
    // `.roomler-partial`, and may do so between the make and the open.
    let mut tries = if create { 2 } else { 1 };
    let staging = loop {
        tries -= 1;
        if create {
            match mkdir_at(base, name, 0o700) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
                Err(e) => {
                    return Err(
                        anyhow::Error::from(e).context(format!("creating {}", shown.display()))
                    );
                }
            }
        }
        match open_at(
            base,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        ) {
            Err(e) if e.kind() == io::ErrorKind::NotFound && tries > 0 => continue,
            opened => break opened,
        }
    };
    let staging = match staging {
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
        if own(&staging, true).is_err() {
            return Err(anyhow!(
                "{} stays open to other accounts, so file transfer does not stage there",
                shown.display()
            ));
        }
        tracing::info!(
            folder = %shown.display(),
            was = format!("{:o}", md.mode() & 0o7777),
            "files: the staging folder was open to other accounts — made 0700"
        );
    }
    Ok(staging)
}

/// R4 — the stager's private staging folder `root` (on a file system that
/// keeps modes): its missing parents made 0700 when `create` (for root, its
/// own `/var/lib/roomler` instead: [`open_roots_own`]), then `root` itself
/// made, opened and checked as [`open_own`] does `.roomler-partial`.
fn open_private_root(root: &Path, create: bool) -> Result<File> {
    let (Some(parent), Some(name)) = (root.parent(), root.file_name()) else {
        return Err(anyhow!("{} is not a folder to stage in", root.display()));
    };
    let held = if stager() == 0 {
        open_roots_own(parent, create)?
    } else {
        if create {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        open_folder(parent).with_context(|| format!("opening {}", parent.display()))?
    };
    open_own(&held, parent, name, create)
}

/// R5 — root's `/var/lib/roomler` (`folder`), which root's private staging
/// folder sits in: opened from a handle on its parent without following a
/// link, and used only when it is root's and no other account may write it,
/// as the overlay host ledger takes it; made 0755 when absent. Anything else
/// refuses the staging, saying why.
fn open_roots_own(folder: &Path, create: bool) -> Result<File> {
    let (Some(parent), Some(name)) = (folder.parent(), folder.file_name()) else {
        return Err(anyhow!("{} is not a folder to stage in", folder.display()));
    };
    if create {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let lib = open_folder(parent).with_context(|| format!("opening {}", parent.display()))?;
    let made = create
        && match mkdir_at(&lib, name, 0o755) {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => false,
            Err(e) => {
                return Err(
                    anyhow::Error::from(e).context(format!("creating {}", folder.display()))
                );
            }
        };
    let not_here = |why: &str| {
        anyhow!(
            "{} {why}, so file transfer does not stage there",
            folder.display()
        )
    };
    let held = match open_at(
        &lib,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    ) {
        Ok(held) => held,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
            return Err(not_here("is not a folder"));
        }
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("opening {}", folder.display())));
        }
    };
    if held.metadata().map(|md| md.uid()).ok() != Some(0) {
        return Err(not_here("belongs to another account"));
    }
    if made {
        // SAFETY: a folder handle we hold, just made.
        unsafe { libc::fchmod(held.as_raw_fd(), 0o755) };
    }
    if held.metadata().map(|md| md.mode() & 0o022).ok() != Some(0) {
        return Err(not_here("is open to other accounts"));
    }
    Ok(held)
}

/// `<id>`'s make in `staging` (at `shown`) failed with `e`: an id that
/// already has a partial is never adopted (B1, the browser sends
/// `files:resume` instead); anything else says where.
fn id_not_made(e: io::Error, shown: &Path, id: &str) -> anyhow::Error {
    if e.raw_os_error() == Some(libc::EEXIST) {
        return anyhow!("id {id} already has partial state; send files:resume");
    }
    anyhow::Error::from(e).context(format!("creating {}", shown.join(id).display()))
}

/// Rule 2 — the new `<id>` in `staging` (at `shown`) opened without
/// following a link and checked the stager's own, its notes (`meta`) written
/// 0600 and its `data` made `data_mode`, each new. Returns `<id>`, and `data`
/// open for writing.
fn fill(
    staging: &File,
    shown: &Path,
    id: &str,
    meta: &[u8],
    data_mode: u32,
) -> Result<(File, File)> {
    let shown = shown.join(id);
    let dir = open_at(
        staging,
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
    Ok((dir, data))
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
    let shown = base_path.join(STAGING);
    // Twice at most: another upload's end removes an empty `.roomler-partial`,
    // and may do so between its open here and the make of `<id>` in it. Then
    // it is made and opened again.
    let mut tries = 2;
    let staging = loop {
        tries -= 1;
        let staging = open_staging(&base, base_path, true)?;
        #[cfg(test)]
        super::stand_in::before_the_id_is_made(base_path);
        match mkdir_at(&staging, id, 0o700) {
            Err(e) if e.kind() == io::ErrorKind::NotFound && tries > 0 => continue,
            made => break made.map(|()| staging),
        }
    }
    .map_err(|e| id_not_made(e, &shown, id))?;
    let (dir, data) = fill(&staging, &shown, id, meta, data_mode)?;
    let target = base.try_clone().context("holding the target folder")?;
    let held = Held {
        beside: Some(base),
        staging,
        dir,
        id: id.to_string(),
        dir_path: shown.join(id),
        target: Some(target),
    };
    Ok((held, data))
}

/// R4 — stage upload `id` for the folder held as `target` in the stager's
/// private staging folder `root`, not beside it: the folder it lands in
/// keeps no modes ([`keeps_modes`]), so a staging folder there could not be
/// the stager's own. `root` (and any missing parent) is made 0700 and
/// checked as rule 2 checks `.roomler-partial`; `<id>` and its files are
/// made as [`stage`] makes them. The end copies the file across into
/// `target` ([`place`]).
pub(super) fn stage_privately(
    target: File,
    root: &Path,
    id: &str,
    meta: &[u8],
    data_mode: u32,
) -> Result<(Held, File)> {
    let staging = open_private_root(root, true)?;
    mkdir_at(&staging, id, 0o700).map_err(|e| id_not_made(e, root, id))?;
    let (dir, data) = fill(&staging, root, id, meta, data_mode)?;
    let held = Held {
        beside: None,
        staging,
        dir,
        id: id.to_string(),
        dir_path: root.join(id),
        target: Some(target),
    };
    Ok((held, data))
}

/// Rule 3 — a partial walked again from its notes' path: in the stager's
/// private folder (`<private_root>/<id>/meta.json`, that folder checked as
/// rule 2 checks it), or beside a folder
/// (`<base>/.roomler-partial/<id>/meta.json`: `<base>`, links followed, its
/// own rule; `.roomler-partial`, rule 2's check); then `<id>` without
/// following a link, the stager's own. `Err` = not a partial this stager
/// may trust, and why, for the log. Returns the handles and, for one staged
/// beside a folder, that folder's path; a private one holds no folder to
/// place in until its resume names it ([`Held::place_in`]).
pub(super) fn reopen(
    meta_path: &Path,
    id: &str,
    private_root: Option<&Path>,
) -> Result<(Held, Option<PathBuf>)> {
    let dir_path = meta_path
        .parent()
        .filter(|dir| dir.file_name() == Some(OsStr::new(id)))
        .ok_or_else(|| anyhow!("{} is not a partial's notes", meta_path.display()))?;
    let open_id = |staging: &File| -> Result<File> {
        let dir = open_at(
            staging,
            id,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .with_context(|| format!("opening {}", dir_path.display()))?;
        own(&dir, true).map_err(|why| anyhow!("{}: {why}", dir_path.display()))?;
        Ok(dir)
    };
    if let Some(root) = private_root.filter(|root| dir_path.parent() == Some(*root)) {
        let staging = open_private_root(root, false)?;
        let dir = open_id(&staging)?;
        let held = Held {
            beside: None,
            staging,
            dir,
            id: id.to_string(),
            dir_path: dir_path.to_path_buf(),
            target: None,
        };
        return Ok((held, None));
    }
    let base_path = dir_path
        .parent()
        .filter(|staging| staging.file_name() == Some(OsStr::new(STAGING)))
        .and_then(Path::parent)
        .ok_or_else(|| anyhow!("{} is not beside a folder", dir_path.display()))?;
    let base =
        open_folder(base_path).with_context(|| format!("opening {}", base_path.display()))?;
    let staging = open_staging(&base, base_path, false)?;
    let dir = open_id(&staging)?;
    let target = base.try_clone().context("holding the target folder")?;
    let held = Held {
        beside: Some(base),
        staging,
        dir,
        id: id.to_string(),
        dir_path: dir_path.to_path_buf(),
        target: Some(target),
    };
    Ok((held, Some(base_path.to_path_buf())))
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
/// under a name nothing there has yet; then the staging goes. A private
/// partial's file is copied across instead (R4). `shown` is the target's
/// path, for the path the viewer is told. On a failure the staging stays,
/// for a resume.
pub(super) fn place(
    held: Held,
    shown: &Path,
    folders: &[String],
    name: &str,
    no_links: bool,
) -> Result<PathBuf> {
    let target = held
        .target
        .as_ref()
        .ok_or_else(|| anyhow!("no folder is held to place {name} in"))?;
    let mut leaf_path = shown.to_path_buf();
    let mut leaf: Option<File> = None;
    for folder in folders {
        let parent = leaf.as_ref().unwrap_or(target);
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
    let leaf = leaf.as_ref().unwrap_or(target);
    let name = unique_name_at(leaf, name);
    let placed = leaf_path.join(&name);
    if held.beside.is_none() {
        // R4 — staged privately, for a folder whose file system keeps no
        // modes: always copied across, through handles.
        copy_across(&held, leaf, &name, &placed)?;
    } else {
        match rename_at(&held.dir, "data", leaf, &name) {
            Ok(()) => {}
            // A subfolder on another file system: copied across, through
            // handles.
            Err(e) if super::is_cross_volume_error(&e) => copy_across(&held, leaf, &name, &placed)?,
            Err(e) => {
                return Err(anyhow::Error::from(e).context(format!(
                    "renaming the received file to {}",
                    placed.display()
                )));
            }
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
/// still there), its notes, `<id>`, and a `.roomler-partial` once empty (a
/// private folder stays). Best-effort, through the held handles, nothing
/// followed.
pub(super) fn discard(held: Held) {
    let _ = unlink_at(&held.dir, "data", 0);
    let _ = unlink_at(&held.dir, "meta.json", 0);
    let _ = unlink_at(&held.staging, &held.id, libc::AT_REMOVEDIR);
    if let Some(base) = &held.beside {
        let _ = unlink_at(base, STAGING, libc::AT_REMOVEDIR);
    }
}

/// R4 — `name` in `dir` (held; a link at its last step not followed, nor a
/// FIFO waited on), read whole when it is the stager's own plain file that
/// nobody else may write and is no bigger than notes ever are. `None`
/// otherwise.
pub(super) fn read_own_file(dir: &File, name: &str) -> Option<String> {
    let file = open_at(
        dir,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    )
    .ok()?;
    own(&file, false).ok()?;
    let mut text = String::new();
    file.take(MAX_NOTES).read_to_string(&mut text).ok()?;
    Some(text)
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

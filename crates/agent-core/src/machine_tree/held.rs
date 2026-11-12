// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-145 — a path below a known base, reached and acted on by handle only.
//!
//! > A process running as SYSTEM, or elevated, acts on a path below a known
//! > base only through handles. The base is opened as itself, and its final
//! > path must be the base's own name. Every component below it is opened
//! > relative to the handle above it, never through a link, and its final path
//! > must be its parent's plus its own name. A folder it makes is sealed when
//! > it is made. A file it writes is made fresh and published by a rename
//! > relative to the held folder. A removal deletes, by handle, what the same
//! > walk opened.
//!
//! [`HeldPath`] carries that rule, so a new SYSTEM writer gets it by calling
//! the helper rather than by re-deriving it:
//!
//! | Call | What it does |
//! |---|---|
//! | [`HeldPath::base`] | the base opened as itself (a reparse point refused); its final path must be the base's long name, or [`Why::Elsewhere`] |
//! | [`HeldPath::open_dir`], [`HeldPath::walk`] | each name opened below the handle above it (`NtCreateFile` with `RootDirectory`, `FILE_OPEN_REPARSE_POINT`), a link refused, the final path checked; `Ok(None)` when a name is not there. Makes nothing |
//! | [`HeldPath::sealed_dir`], [`HeldPath::walk_sealed`] | as `walk`, and a folder that is not there is made with the seal's descriptor in its `OBJECT_ATTRIBUTES`; the last name gets the leaf policy |
//! | [`HeldPath::machine`] | `base(machine_program_data())`, then down the names of a machine-tree folder; the machine folder and its parent are only opened (made sealed when absent) |
//! | [`HeldPath::write_file`] | a fresh temp file (`FILE_CREATE`), written and flushed, renamed onto the name relative to the held folder; on failure the temp is deleted by handle |
//! | [`HeldPath::append_file`] | for logs: a file there must be a plain file of one link owned by SYSTEM or Administrators, else it is deleted by handle and made again |
//! | [`HeldPath::read_file`], [`HeldPath::entries`] | read below the held folder, never through a link |
//! | [`HeldPath::remove_file`], [`HeldPath::remove_empty_dir`], [`HeldPath::rename_file`] | opened below the held folder and acted on through that handle |
//! | [`HeldPath::remove_tree`] | bottom-up by handle; a link is never followed and never deleted, and [`TreeRemoval::left`] names it |
//!
//! The leaf policy, for what [`HeldPath::sealed_dir`] finds at its name:
//!
//! | Found | What happens |
//! |---|---|
//! | nothing | made sealed |
//! | a folder SYSTEM or Administrators own | sealed in place, by handle, on the folder alone: nothing is handed down to what it holds ([`HeldPath::resealed`] says so, and the site seals the names it uses with [`HeldPath::keep_only_plain`]) |
//! | a folder another account owns | moved aside whole, by handle, as `<name>.moved-<unix>`, and a sealed one made |
//! | a link, or anything that is not a folder | the entry itself moved aside by handle, never followed, and a sealed folder made |
//!
//! `OBJ_DONT_REPARSE` goes on every relative open as a second lock beside
//! `FILE_OPEN_REPARSE_POINT` and the attribute check; a build that does not
//! know it answers `STATUS_INVALID_PARAMETER`, and the open is made again
//! without it (remembered for the process). Nothing relies on it alone.
//!
//! Windows only. The machine folder's own repair and check stay FR-94's
//! (`super::repair`, `super::check`).

use std::ffi::c_void;
use std::fs::File;
use std::io::Write as _;
use std::os::windows::fs::MetadataExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, RawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
    FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
    FileRenameInformation, NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{HANDLE, RtlNtStatusToDosError, UNICODE_STRING};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetTokenInformation, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, SetKernelObjectSecurity, TOKEN_TYPE, TokenImpersonation,
    TokenType,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_EXTD_DIR_INFO, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileDispositionInfoEx, FileIdExtdDirectoryInfo,
    FileIdExtdDirectoryRestartInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetFinalPathNameByHandleW, GetLongPathNameW, SetFileInformationByHandle, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use super::{ADMINISTRATORS, Ace, CONTAINER_INHERIT, HeldFolder, OBJECT_INHERIT, Sd, win};

// ─── access and status codes ──────────────────────────────────────────────

const LIST: u32 = 0x1; // FILE_LIST_DIRECTORY, FILE_READ_DATA on a file
const APPEND: u32 = 0x4;
const TRAVERSE: u32 = 0x20;
const READ_ATTRIBUTES: u32 = 0x80;
const DELETE: u32 = 0x1_0000;
const READ_CONTROL: u32 = 0x2_0000;
const WRITE_DAC: u32 = 0x4_0000;
const WRITE_OWNER: u32 = 0x8_0000;
const SYNCHRONIZE: u32 = 0x10_0000;
/// `FILE_GENERIC_WRITE`.
const GENERIC_WRITE: u32 = 0x0012_0116;

/// A folder a walk holds: listed, passed through, its owner and DACL read.
const FOLDER: u32 = LIST | TRAVERSE | READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE;
/// A leaf the policy may seal in place or move aside.
const LEAF: u32 = FOLDER | WRITE_DAC | WRITE_OWNER | DELETE;
/// What it takes to read a leaf's owner and move it aside, and nothing more:
/// for an entry whose DACL gives SYSTEM less than [`LEAF`].
const MOVE: u32 = READ_CONTROL | READ_ATTRIBUTES | SYNCHRONIZE | DELETE;
/// A folder a removal opens: listed, and deleted through this handle.
const TREE: u32 = FOLDER | DELETE;

const FILE_CREATED: usize = 2;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
/// The object manager fails an open that meets a reparse point instead of
/// following it. A second lock: see the module doc.
const OBJ_DONT_REPARSE: u32 = 0x1000;

const fn status(code: u32) -> i32 {
    code as i32
}
const STATUS_INVALID_PARAMETER: i32 = status(0xC000_000D);
const STATUS_ACCESS_DENIED: i32 = status(0xC000_0022);
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = status(0xC000_0034);
const STATUS_OBJECT_NAME_COLLISION: i32 = status(0xC000_0035);
const STATUS_OBJECT_PATH_NOT_FOUND: i32 = status(0xC000_003A);
const STATUS_DELETE_PENDING: i32 = status(0xC000_0056);
const STATUS_INVALID_OWNER: i32 = status(0xC000_005A);
const STATUS_FILE_IS_A_DIRECTORY: i32 = status(0xC000_00BA);
const STATUS_NOT_A_DIRECTORY: i32 = status(0xC000_0103);
const STATUS_REPARSE_POINT_ENCOUNTERED: i32 = status(0xC000_050B);

const ERROR_NO_MORE_FILES: i32 = 18;
const ERROR_DIR_NOT_EMPTY: i32 = 145;

// ─── the public shapes ────────────────────────────────────────────────────

/// Who besides SYSTEM and Administrators may read what SYSTEM makes. Nobody
/// else ever writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seal {
    /// Owner Administrators; protected; SYSTEM and Administrators full
    /// control, handed down to what is made below.
    Private,
    /// [`Seal::Private`], plus Users read, list and traverse (`0x1200a9`):
    /// `service-logs`, `crashes` (D10), the attempt record.
    Readable,
}

impl Seal {
    /// The seal's entries: `flags` on each (`OICI` on a folder, none on a
    /// file).
    pub fn aces(self, flags: u8) -> Vec<Ace> {
        let mut aces = super::sealed_aces(flags);
        if self == Self::Readable {
            aces.push(Ace {
                kind: super::ACE_ALLOW,
                flags,
                mask: super::READ_EXECUTE,
                sid: super::USERS_GROUP.into(),
            });
        }
        aces
    }

    /// The seal as SDDL, with Administrators as the owner when `owner`.
    fn sddl(self, folder: bool, owner: bool) -> String {
        let flags = if folder { "OICI" } else { "" };
        let mut sddl = String::from(if owner { "O:BAD:P" } else { "D:P" });
        sddl.push_str(&format!("(A;{flags};FA;;;SY)(A;{flags};FA;;;BA)"));
        if self == Self::Readable {
            sddl.push_str(&format!("(A;{flags};0x1200a9;;;BU)"));
        }
        sddl
    }
}

/// Why a step or an act was not done. `at` names the entry; a site logs it,
/// or puts it in a report line (`"{at} is a link"`).
#[derive(Debug)]
pub struct Refusal {
    pub at: PathBuf,
    pub why: Why,
}

/// What was found at [`Refusal::at`].
#[derive(Debug)]
pub enum Why {
    /// A junction or a symbolic link (any reparse point): never followed.
    Link,
    /// Not a folder where a folder was asked for.
    NotFolder,
    /// Not a plain file where a file was asked for.
    NotFile,
    /// Its final path is not the name it was reached by.
    Elsewhere { final_path: PathBuf },
    /// Owned by an account that is neither SYSTEM nor Administrators.
    Foreign { owner: String },
    /// A file with more than one name (a hard link) where one is written.
    TooManyLinks,
    /// Anything else: the system's own answer.
    Io(std::io::Error),
}

impl Clone for Why {
    fn clone(&self) -> Self {
        match self {
            Self::Link => Self::Link,
            Self::NotFolder => Self::NotFolder,
            Self::NotFile => Self::NotFile,
            Self::Elsewhere { final_path } => Self::Elsewhere {
                final_path: final_path.clone(),
            },
            Self::Foreign { owner } => Self::Foreign {
                owner: owner.clone(),
            },
            Self::TooManyLinks => Self::TooManyLinks,
            Self::Io(e) => Self::Io(match e.raw_os_error() {
                Some(code) => std::io::Error::from_raw_os_error(code),
                None => std::io::Error::new(e.kind(), e.to_string()),
            }),
        }
    }
}

impl Clone for Refusal {
    fn clone(&self) -> Self {
        Self {
            at: self.at.clone(),
            why: self.why.clone(),
        }
    }
}

impl std::fmt::Display for Why {
    /// The predicate of a report line: `"<at> {why}"`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Link => f.write_str("is a link (a junction or a symbolic link), never followed"),
            Self::NotFolder => f.write_str("is not a folder"),
            Self::NotFile => f.write_str("is not a plain file"),
            Self::Elsewhere { final_path } => {
                write!(f, "ends elsewhere ({})", final_path.display())
            }
            Self::Foreign { owner } => write!(f, "is owned by another account ({owner})"),
            Self::TooManyLinks => f.write_str("has more than one name (a hard link)"),
            Self::Io(e) => write!(f, "could not be used: {e}"),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.at.display(), self.why)
    }
}

impl std::error::Error for Refusal {}

impl From<Refusal> for std::io::Error {
    fn from(r: Refusal) -> Self {
        let kind = match &r.why {
            Why::Io(e) => e.kind(),
            _ => std::io::ErrorKind::PermissionDenied,
        };
        std::io::Error::new(kind, r.to_string())
    }
}

/// One entry of a held folder, as its listing gives it: never opened, never
/// followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldEntry {
    pub name: String,
    pub is_dir: bool,
    /// A junction or a symbolic link (any reparse point).
    pub is_link: bool,
    pub len: u64,
    /// The last write, in seconds since the Unix epoch.
    pub modified_unix: i64,
}

/// What [`HeldPath::remove_tree`] did.
#[derive(Debug, Default)]
pub struct TreeRemoval {
    /// The tree was there.
    pub present: bool,
    /// Files deleted (none on a dry run).
    pub removed_files: usize,
    /// What was left, and why: every link met, with the folders above it.
    pub left: Vec<(PathBuf, Why)>,
    /// The tree's own folder was deleted.
    pub root_removed: bool,
}

impl TreeRemoval {
    /// A dry run's answer: the tree is there and holds no link, so a real
    /// run would remove it.
    pub fn would_remove(&self) -> bool {
        self.present && self.left.is_empty()
    }
}

/// A folder held open by its own handle at the end of a checked way down from
/// a known base. Closed on drop. Its path, in log and report lines, is the
/// base as the caller named it plus the names below it; the checks compare
/// final paths ([`HeldPath::base`]).
pub struct HeldPath {
    folder: HeldFolder,
    /// Its final path as checked: the next name's must be this plus the name.
    checked: PathBuf,
    /// Opened for passing through only (`FILE_READ_ATTRIBUTES`): never listed
    /// or acted in.
    through: bool,
    /// The leaf was there, SYSTEM's or Administrators', and was sealed in
    /// place by this walk.
    resealed: bool,
}

impl std::fmt::Debug for HeldPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldPath")
            .field("path", &self.folder.path)
            .field("resealed", &self.resealed)
            .finish()
    }
}

/// How a walk opens the next name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Open it; a link refused. `through`: a name the walk passes through,
    /// which may be opened with `FILE_READ_ATTRIBUTES` alone when the full
    /// set is denied (a read walk only).
    Open { through: bool },
    /// Open it, or make it sealed when it is not there; a link refused. No
    /// leaf policy.
    Make(Seal),
    /// [`Step::Make`] with the leaf policy.
    Leaf(Seal),
}

impl HeldPath {
    /// The base, opened as itself (`FILE_FLAG_OPEN_REPARSE_POINT`; a reparse
    /// point refused). Its final path must equal `GetLongPathNameW(base)`,
    /// both without `\\?\` and compared without regard to case; a path on a
    /// network share is refused. D1: a base reached through a link (a
    /// relocated `ProgramData`, a redirected profile) is refused, naming both
    /// paths.
    pub fn base(base: &Path) -> Result<Self, Refusal> {
        use std::os::windows::fs::OpenOptionsExt as _;
        let refuse = |why| Refusal {
            at: base.to_path_buf(),
            why,
        };
        let file = std::fs::OpenOptions::new()
            .access_mode(FOLDER)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(base)
            .map_err(|e| refuse(Why::Io(e)))?;
        kind_of(&file, true).map_err(refuse)?;
        let long = long_path(base).map_err(|e| refuse(Why::Io(e)))?;
        let final_path = final_path(&file).map_err(|e| refuse(Why::Io(e)))?;
        if is_unc(&final_path) || !same_path(&final_path, &long) {
            return Err(refuse(Why::Elsewhere { final_path }));
        }
        Ok(Self {
            folder: HeldFolder {
                entry: win::Entry {
                    file,
                    writable: false,
                },
                path: base.to_path_buf(),
            },
            checked: final_path,
            through: false,
            resealed: false,
        })
    }

    /// `base(machine_program_data())`, then down the names of `target` below
    /// it, each made with `seal` when it is not there; the last name gets the
    /// leaf policy. The machine folder and its parent (the first two names)
    /// are only opened, or made sealed when they are not there: the leaf
    /// policy never moves or re-seals one of those two, which hold the
    /// machine config and are FR-94's to repair. A `target` that is not below
    /// the machine's `ProgramData` is refused.
    pub fn machine(target: &Path, seal: Seal) -> Result<Self, Refusal> {
        let base = crate::appdirs::machine_program_data();
        let names = names_of(target, &base)?;
        let mut held = Self::base(&base)?;
        for (i, name) in names.iter().enumerate() {
            let step = if i + 1 == names.len() && i >= 2 {
                Step::Leaf(seal)
            } else {
                Step::Make(seal)
            };
            held = held.step(name, step)?.ok_or_else(|| absent(&held, name))?;
        }
        Ok(held.ready())
    }

    /// `base(base)`, then [`HeldPath::walk`] down the names of `dir` below it.
    /// `Ok(None)`: a folder on the way, or `dir`, is not there. A `dir` not
    /// below `base` is refused.
    pub fn below(base: &Path, dir: &Path) -> Result<Option<Self>, Refusal> {
        let names = names_of(dir, base)?;
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        Self::base(base)?.walk(&names)
    }

    /// The folder `name` below this one, never through a link, its final
    /// path checked. `Ok(None)`: it is not there. Makes nothing.
    pub fn open_dir(&self, name: &str) -> Result<Option<Self>, Refusal> {
        Ok(self
            .step(name, Step::Open { through: false })?
            .map(Self::ready))
    }

    /// Down `names`, each opened below the folder above it as
    /// [`HeldPath::open_dir`] opens it. `Ok(None)`: a name is not there. A
    /// name passed through may be held with `FILE_READ_ATTRIBUTES` alone
    /// (granted by its parent's list right) when the full set is denied, so
    /// a reader running as a person reaches a folder Users may read below one
    /// they may not list; the last name is always held whole.
    pub fn walk(&self, names: &[&str]) -> Result<Option<Self>, Refusal> {
        let Some((last, through)) = names.split_last() else {
            return Err(self.refuse(Why::NotFolder));
        };
        let mut held: Option<Self> = None;
        for name in through {
            let at = held.as_ref().unwrap_or(self);
            match at.step(name, Step::Open { through: true })? {
                Some(next) => held = Some(next),
                None => return Ok(None),
            }
        }
        let at = held.as_ref().unwrap_or(self);
        Ok(at
            .step(last, Step::Open { through: false })?
            .map(Self::ready))
    }

    /// The folder `name` below this one, made with `seal`'s descriptor in its
    /// `OBJECT_ATTRIBUTES` when it is not there (sealed from its creation),
    /// and given the leaf policy when it is.
    pub fn sealed_dir(&self, name: &str, seal: Seal) -> Result<Self, Refusal> {
        let held = self
            .step(name, Step::Leaf(seal))?
            .ok_or_else(|| absent(self, name))?;
        Ok(held.ready())
    }

    /// Down `names`, each made with `seal` when it is not there; only the last
    /// gets the leaf policy.
    pub fn walk_sealed(&self, names: &[&str], seal: Seal) -> Result<Self, Refusal> {
        let Some((last, through)) = names.split_last() else {
            return Err(self.refuse(Why::NotFolder));
        };
        let mut held: Option<Self> = None;
        for name in through {
            let at = held.as_ref().unwrap_or(self);
            held = Some(
                at.step(name, Step::Make(seal))?
                    .ok_or_else(|| absent(at, name))?,
            );
        }
        let at = held.as_ref().unwrap_or(self);
        let leaf = at
            .step(last, Step::Leaf(seal))?
            .ok_or_else(|| absent(at, last))?;
        Ok(leaf.ready())
    }

    /// This folder's final path, as checked.
    pub fn path(&self) -> &Path {
        &self.folder.path
    }

    /// The leaf was there, SYSTEM's or Administrators', and this walk sealed
    /// it in place: what an earlier build left in it is still there, and a
    /// site scrubs the names it uses ([`HeldPath::keep_only_plain`]).
    pub fn resealed(&self) -> bool {
        self.resealed
    }

    /// The folder's own handle, for #3743's readers.
    pub fn into_folder(self) -> HeldFolder {
        self.folder
    }

    /// Write `bytes` as the file `name`: a fresh temp file below this folder
    /// (`FILE_CREATE`, so nothing already there is ever written into), made
    /// with `seal`'s descriptor, written and flushed (#2955), then renamed
    /// onto `name` with `FileRenameInformation` relative to this folder's
    /// handle. A file or a file link at `name` is replaced as a name, never
    /// written through; a folder or a junction there fails the rename, and
    /// the temp file is deleted by its handle.
    pub fn write_file(&self, name: &str, bytes: &[u8], seal: Seal) -> Result<(), Refusal> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let temp = format!(
            "{name}.{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let (file, _) = self.make(
            &temp,
            GENERIC_WRITE | DELETE | READ_CONTROL,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
            seal,
        )?;
        let written = (&file)
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| self.refuse_at(&temp, Why::Io(e)))
            .and_then(|()| {
                rename_by_handle(&file, self.handle(), name, true)
                    .map_err(|code| self.refuse_at(name, why_of(code, Kind::File)))
            });
        if written.is_err() {
            let _ = dispose(&file);
        }
        written
    }

    /// `FILE_CREATE` straight into `name`, made with `seal`'s descriptor: the
    /// first writer wins. `Ok(false)`: `name` was there, and is left as it is.
    pub fn write_file_new(&self, name: &str, bytes: &[u8], seal: Seal) -> Result<bool, Refusal> {
        let file = match self.make(
            name,
            GENERIC_WRITE | DELETE | READ_CONTROL,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE,
            seal,
        ) {
            Ok((file, _)) => file,
            Err(Refusal {
                why: Why::Io(e), ..
            }) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(r) => return Err(r),
        };
        let written = (&file)
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| self.refuse_at(name, Why::Io(e)));
        if written.is_err() {
            let _ = dispose(&file);
        }
        written.map(|()| true)
    }

    /// Open `name` for appending (logs), made with `seal`'s descriptor when
    /// it is not there. A file already there is appended to only when it is
    /// a plain file with one link, owned by SYSTEM or Administrators;
    /// anything else of that name is deleted by its handle (a link itself,
    /// never what it leads to) and the file made again.
    pub fn append_file(&self, name: &str, seal: Seal) -> Result<File, Refusal> {
        let access = APPEND | READ_ATTRIBUTES | READ_CONTROL | DELETE | SYNCHRONIZE;
        let (file, created) =
            self.make(name, access, FILE_OPEN_IF, FILE_NON_DIRECTORY_FILE, seal)?;
        if created || plain_and_ours(&file).is_ok() {
            return Ok(file);
        }
        dispose(&file).map_err(|e| self.refuse_at(name, Why::Io(e)))?;
        drop(file);
        let (file, _) = self.make(name, access, FILE_CREATE, FILE_NON_DIRECTORY_FILE, seal)?;
        Ok(file)
    }

    /// The file `name` (at most `max_len` bytes), read below this folder:
    /// never through a link, never a folder, never a file with more than one
    /// name. `Ok(None)`: it is not there.
    pub fn read_file(&self, name: &str, max_len: u64) -> Result<Option<Vec<u8>>, Refusal> {
        let path = self.path().join(name);
        let file = match win::open_below(self.handle(), name, &path, false) {
            Ok(Some(file)) => file,
            Ok(None) => return Ok(None),
            Err(e) => return Err(self.refuse_at(name, why_of_text(&e))),
        };
        if links_of(&file).map_err(|e| self.refuse_at(name, Why::Io(e)))? > 1 {
            return Err(self.refuse_at(name, Why::TooManyLinks));
        }
        win::read_to_end(&file, &path, max_len)
            .map(Some)
            .map_err(|e| self.refuse_at(name, Why::Io(std::io::Error::other(e))))
    }

    /// The entries of this folder, read through its handle
    /// (`GetFileInformationByHandleEx`, `FileIdExtdDirectoryInfo`): nothing
    /// is opened or followed.
    pub fn entries(&self) -> Result<Vec<HeldEntry>, Refusal> {
        list(self.handle()).map_err(|e| self.refuse(Why::Io(e)))
    }

    /// Delete the file `name` through its own handle, opened below this
    /// folder. `Ok(false)`: it is not there. A link at `name` is refused,
    /// never followed.
    pub fn remove_file(&self, name: &str) -> Result<bool, Refusal> {
        let path = self.path().join(name);
        match win::open_below(self.handle(), name, &path, true) {
            Ok(Some(file)) => dispose(&file)
                .map(|()| true)
                .map_err(|e| self.refuse_at(name, Why::Io(e))),
            Ok(None) => Ok(false),
            Err(e) => Err(self.refuse_at(name, why_of_text(&e))),
        }
    }

    /// Delete the empty folder `name` through its own handle, opened below
    /// this folder. `Ok(false)`: it is not there, or it is not empty (and
    /// stays). A link at `name` is refused, never followed.
    pub fn remove_empty_dir(&self, name: &str) -> Result<bool, Refusal> {
        let Some(dir) = self.open_entry(name, TREE, Kind::Dir)? else {
            return Ok(false);
        };
        match dispose(&dir) {
            Ok(()) => Ok(true),
            Err(e) if e.raw_os_error() == Some(ERROR_DIR_NOT_EMPTY) => Ok(false),
            Err(e) => Err(self.refuse_at(name, Why::Io(e))),
        }
    }

    /// Rename the file `from` of this folder to `to` in `into`, through the
    /// file's own handle (opened below this folder with `DELETE`, never
    /// through a link), with `FileRenameInformation` relative to `into`'s
    /// handle. `replace`: a file already at `to` is replaced.
    pub fn rename_file(
        &self,
        from: &str,
        into: &HeldPath,
        to: &str,
        replace: bool,
    ) -> Result<(), Refusal> {
        let file = self
            .open_entry(from, MOVE, Kind::File)?
            .ok_or_else(|| self.refuse_at(from, Why::Io(not_found())))?;
        rename_by_handle(&file, into.handle(), to, replace)
            .map_err(|code| into.refuse_at(to, why_of(code, Kind::File)))
    }

    /// After a reseal in place ([`HeldPath::resealed`]), the entries whose
    /// names `uses` matches, each through its own handle: a plain file of one
    /// link owned by SYSTEM or Administrators is kept and sealed with `seal`
    /// (on that file alone); anything else is deleted, a link as itself,
    /// never followed. The reseal hands nothing down, so names `uses` does
    /// not match are never opened and keep what they had: a hard link among
    /// them never carries a seal to a file outside. What was deleted,
    /// or could not be opened, sealed or deleted, for one WARN.
    pub fn keep_only_plain(&self, uses: impl Fn(&str) -> bool, seal: Seal) -> Vec<Refusal> {
        let mut out = Vec::new();
        let entries = match self.entries() {
            Ok(entries) => entries,
            Err(r) => return vec![r],
        };
        let open = |name: &str, access: u32| {
            open_below(
                self.handle(),
                name,
                access,
                FILE_OPEN,
                FILE_SYNCHRONOUS_IO_NONALERT,
                None,
            )
        };
        for entry in entries.into_iter().filter(|e| uses(&e.name)) {
            let at = self.path().join(&entry.name);
            // WRITE_DAC to seal what is kept: an owner's own right, so it is
            // missing only on an entry that is not kept anyway.
            let file = match open(&entry.name, MOVE | WRITE_DAC).or_else(|code| {
                if code == STATUS_ACCESS_DENIED {
                    open(&entry.name, MOVE)
                } else {
                    Err(code)
                }
            }) {
                Ok((file, _)) => file,
                Err(code) if is_absent(code) => continue,
                Err(code) => {
                    out.push(Refusal {
                        at,
                        why: why_of(code, Kind::File),
                    });
                    continue;
                }
            };
            let why = match plain_and_ours(&file) {
                Ok(()) => match set_own_sd(&file, &seal.sddl(false, false), false) {
                    Ok(()) => continue,
                    Err(e) => Why::Io(e),
                },
                Err(why) => match dispose(&file) {
                    Ok(()) => why,
                    Err(e) => Why::Io(e),
                },
            };
            out.push(Refusal { at, why });
        }
        out
    }

    /// Remove the tree `name` below this folder, bottom-up by handle: each
    /// entry opened below its folder's handle with `FILE_OPEN_REPARSE_POINT`
    /// and deleted through that handle (`FileDispositionInfoEx`: POSIX
    /// semantics, read-only ignored; `FileDispositionInfo` where a build does
    /// not know it). A link met anywhere is never followed and never deleted:
    /// it stays, with the folders above it, and [`TreeRemoval::left`] names
    /// it. A first pass leaves the whole tree when it holds a link; the
    /// delete pass leaves any link that appeared since. `dry_run`: the first
    /// pass only.
    pub fn remove_tree(&self, name: &str, dry_run: bool) -> Result<TreeRemoval, Refusal> {
        let mut out = TreeRemoval::default();
        let at = self.path().join(name);
        let root = match open_below(
            self.handle(),
            name,
            TREE,
            FILE_OPEN,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
        ) {
            Ok((root, _)) => root,
            Err(code) if is_absent(code) => return Ok(out),
            Err(code) => return Err(self.refuse_at(name, why_of(code, Kind::Dir))),
        };
        out.present = true;
        if is_link(&root).map_err(|e| self.refuse_at(name, Why::Io(e)))? {
            out.left.push((at, Why::Link));
            return Ok(out);
        }
        scan_links(&root, &at, &mut out.left);
        if !out.left.is_empty() || dry_run {
            return Ok(out);
        }
        if delete_below(&root, &at, &mut out) {
            match dispose(&root) {
                Ok(()) => out.root_removed = true,
                Err(e) => out.left.push((at, Why::Io(e))),
            }
        }
        Ok(out)
    }

    // ─── the steps ────────────────────────────────────────────────────────

    fn handle(&self) -> &File {
        &self.folder.entry.file
    }

    fn refuse(&self, why: Why) -> Refusal {
        Refusal {
            at: self.path().to_path_buf(),
            why,
        }
    }

    fn refuse_at(&self, name: &str, why: Why) -> Refusal {
        Refusal {
            at: self.path().join(name),
            why,
        }
    }

    /// Called once a walk holds its last name: the swap tests' moment
    /// (#3743's `between_check_and_use`).
    fn ready(self) -> Self {
        #[cfg(any(test, feature = "test-fixtures"))]
        super::test_support::run_between();
        self
    }

    /// The next name, as `step` says. `Ok(None)`: it is not there (an
    /// [`Step::Open`] only).
    fn step(&self, name: &str, step: Step) -> Result<Option<Self>, Refusal> {
        if self.through && !matches!(step, Step::Open { .. }) {
            return Err(self.refuse(Why::Io(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))));
        }
        let options = FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT;
        let (file, created, through) = match step {
            Step::Open { through } => {
                match open_below(self.handle(), name, FOLDER, FILE_OPEN, options, None) {
                    Ok((file, _)) => (file, false, false),
                    Err(code) if is_absent(code) => return Ok(None),
                    // A name passed through, which this account may not list:
                    // held for its name and attributes alone. No synchronous
                    // I/O (that takes SYNCHRONIZE, which the DACL would have
                    // to give), and nothing is ever listed or done in it.
                    Err(STATUS_ACCESS_DENIED) if through && read_through_on() => {
                        match open_below(
                            self.handle(),
                            name,
                            READ_ATTRIBUTES,
                            FILE_OPEN,
                            FILE_DIRECTORY_FILE,
                            None,
                        ) {
                            Ok((file, _)) => (file, false, true),
                            Err(code) => return Err(self.refuse_at(name, why_of(code, Kind::Dir))),
                        }
                    }
                    Err(code) => return Err(self.refuse_at(name, why_of(code, Kind::Dir))),
                }
            }
            Step::Make(seal) => {
                let (file, created) =
                    self.make(name, FOLDER, FILE_OPEN_IF, FILE_DIRECTORY_FILE, seal)?;
                (file, created, false)
            }
            Step::Leaf(seal) => return self.leaf(name, seal).map(Some),
        };
        if !created && is_link(&file).map_err(|e| self.refuse_at(name, Why::Io(e)))? {
            return Err(self.refuse_at(name, Why::Link));
        }
        self.hold(file, name, through, false).map(Some)
    }

    /// The leaf policy (D2): see the module doc.
    fn leaf(&self, name: &str, seal: Seal) -> Result<Self, Refusal> {
        let made = self.make(name, LEAF, FILE_OPEN_IF, FILE_DIRECTORY_FILE, seal);
        let found = match made {
            Ok((file, true)) => return self.hold(file, name, false, false),
            Ok((file, false)) => file,
            // Not a folder (a file, a file link): opened as itself to move it.
            Err(Refusal {
                why: Why::NotFolder,
                ..
            }) => {
                let file = self
                    .open_entry_raw(name, MOVE, FILE_NON_DIRECTORY_FILE)
                    .map_err(|code| self.refuse_at(name, why_of(code, Kind::File)))?;
                return self.move_aside_and_make(file, name, "is not a folder", seal);
            }
            // SYSTEM may not open it whole: enough to read its owner and
            // move it aside.
            Err(Refusal {
                why: Why::Io(ref e),
                ..
            }) if e.kind() == std::io::ErrorKind::PermissionDenied => self
                .open_entry_raw(name, MOVE, FILE_DIRECTORY_FILE)
                .map_err(|code| self.refuse_at(name, why_of(code, Kind::Dir)))?,
            Err(r) => return Err(r),
        };
        if is_link(&found).map_err(|e| self.refuse_at(name, Why::Io(e)))? {
            return self.move_aside_and_make(found, name, "is a link", seal);
        }
        let sd = win::read_sd(&found).map_err(|e| self.refuse_at(name, Why::Io(e)))?;
        if !super::trusted(&sd.owner) {
            let why = format!("is owned by another account ({})", sd.owner);
            return self.move_aside_and_make(found, name, &why, seal);
        }
        let aces = seal.aces(OBJECT_INHERIT | CONTAINER_INHERIT);
        let resealed = !(sd.owner == ADMINISTRATORS && super::is_exactly(&sd, &aces));
        if resealed {
            // On the folder alone: nothing is handed down to what it holds,
            // so a hard link left in it never carries the seal to a file
            // outside. The site seals the entries it uses by name
            // (`keep_only_plain`).
            set_own_sd(&found, &seal.sddl(true, true), true)
                .map_err(|e| self.refuse_at(name, Why::Io(e)))?;
        }
        self.hold(found, name, false, resealed)
    }

    /// Move the entry behind `found` aside as `<name>.moved-<unix>`, by its
    /// own handle, then make `name` again with `seal`. One WARN names it.
    fn move_aside_and_make(
        &self,
        found: File,
        name: &str,
        what: &str,
        seal: Seal,
    ) -> Result<Self, Refusal> {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut moved = None;
        for n in 0..10 {
            let aside = if n == 0 {
                format!("{name}.moved-{unix}")
            } else {
                format!("{name}.moved-{unix}-{n}")
            };
            match rename_by_handle(&found, self.handle(), &aside, false) {
                Ok(()) => {
                    moved = Some(aside);
                    break;
                }
                Err(STATUS_OBJECT_NAME_COLLISION) => continue,
                Err(code) => return Err(self.refuse_at(name, why_of(code, Kind::Dir))),
            }
        }
        let Some(aside) = moved else {
            return Err(self.refuse_at(name, Why::Io(std::io::ErrorKind::AlreadyExists.into())));
        };
        drop(found);
        tracing::warn!(
            path = %self.path().join(name).display(),
            moved_to = %self.path().join(&aside).display(),
            "machine tree: {what}; moved aside whole, nothing in it deleted, and a sealed \
             folder made in its place"
        );
        let (file, _) = self.make(name, LEAF, FILE_CREATE, FILE_DIRECTORY_FILE, seal)?;
        self.hold(file, name, false, false)
    }

    /// `file`, the folder `name` below this one, held: its final path must be
    /// this folder's checked final path plus `name` (the stored one, so a
    /// held folder renamed since makes every later step below it refuse).
    fn hold(&self, file: File, name: &str, through: bool, resealed: bool) -> Result<Self, Refusal> {
        let final_path = final_path(&file).map_err(|e| self.refuse_at(name, Why::Io(e)))?;
        if !same_path(&final_path, &self.checked.join(name)) {
            return Err(self.refuse_at(name, Why::Elsewhere { final_path }));
        }
        Ok(Self {
            folder: HeldFolder {
                entry: win::Entry {
                    file,
                    writable: false,
                },
                path: self.path().join(name),
            },
            checked: final_path,
            through,
            resealed,
        })
    }

    /// `NtCreateFile` of `name` below this folder with `disposition`, the
    /// seal's descriptor in its `OBJECT_ATTRIBUTES`: Administrators as the
    /// owner, or, for a token that may not name them (`STATUS_INVALID_OWNER`,
    /// an unelevated test), the caller. `kind` is `FILE_DIRECTORY_FILE` or
    /// `FILE_NON_DIRECTORY_FILE`. Whether it was made.
    fn make(
        &self,
        name: &str,
        access: u32,
        disposition: u32,
        kind: u32,
        seal: Seal,
    ) -> Result<(File, bool), Refusal> {
        let folder = kind == FILE_DIRECTORY_FILE;
        let kind_of_entry = if folder { Kind::Dir } else { Kind::File };
        let options = kind | FILE_SYNCHRONOUS_IO_NONALERT;
        let mut last = 0;
        for owner in [true, false] {
            let sd = win::Descriptor::from_sddl(&seal.sddl(folder, owner))
                .map_err(|e| self.refuse_at(name, Why::Io(e)))?;
            match open_below(self.handle(), name, access, disposition, options, Some(&sd)) {
                Ok(made) => return Ok(made),
                // Never on a thread that acts as another account (a panic
                // inside `as_session_user`, a transfer written as the person):
                // that account would own what SYSTEM makes.
                Err(STATUS_INVALID_OWNER) if owner && impersonating() => {
                    return Err(self.refuse_at(
                        name,
                        Why::Io(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "not made: this thread acts as another account, which would own it",
                        )),
                    ));
                }
                Err(STATUS_INVALID_OWNER) if owner => last = STATUS_INVALID_OWNER,
                Err(code) => return Err(self.refuse_at(name, why_of(code, kind_of_entry))),
            }
        }
        Err(self.refuse_at(name, why_of(last, kind_of_entry)))
    }

    /// The entry `name` below this folder, never through a link (a link is
    /// refused), opened with `access`. `Ok(None)`: it is not there.
    fn open_entry(&self, name: &str, access: u32, kind: Kind) -> Result<Option<File>, Refusal> {
        let options = match kind {
            Kind::Dir => FILE_DIRECTORY_FILE,
            Kind::File => FILE_NON_DIRECTORY_FILE,
        };
        let file = match self.open_entry_raw(name, access, options) {
            Ok(file) => file,
            Err(code) if is_absent(code) => return Ok(None),
            Err(code) => return Err(self.refuse_at(name, why_of(code, kind))),
        };
        if is_link(&file).map_err(|e| self.refuse_at(name, Why::Io(e)))? {
            return Err(self.refuse_at(name, Why::Link));
        }
        Ok(Some(file))
    }

    fn open_entry_raw(&self, name: &str, access: u32, options: u32) -> Result<File, i32> {
        open_below(
            self.handle(),
            name,
            access,
            FILE_OPEN,
            options | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
        )
        .map(|(file, _)| file)
    }
}

/// This thread acts as another account: it holds an impersonation token.
/// Read from the thread's effective token (`GetCurrentThreadEffectiveToken`,
/// the pseudo-handle -6); a query that fails counts as acting as another
/// account, so the caller-owned fallback is never taken on a doubt.
fn impersonating() -> bool {
    const THREAD_EFFECTIVE_TOKEN: HANDLE = -6isize as HANDLE;
    let mut kind: TOKEN_TYPE = 0;
    let mut len = 0u32;
    // SAFETY: a pseudo-handle the system resolves for this thread (Windows 8
    // and later); `kind` is the 4-byte answer of the `TokenType` class.
    let ok = unsafe {
        GetTokenInformation(
            THREAD_EFFECTIVE_TOKEN,
            TokenType,
            (&raw mut kind).cast(),
            std::mem::size_of::<TOKEN_TYPE>() as u32,
            &mut len,
        )
    };
    ok == 0 || kind == TokenImpersonation
}

#[cfg(test)]
thread_local! {
    /// Tests only: a read walk on this thread may not pass a folder with
    /// `FILE_READ_ATTRIBUTES` alone (D10's control case).
    static NO_READ_THROUGH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A read walk may pass a folder it may not list with `FILE_READ_ATTRIBUTES`
/// alone (D10). Always, outside the tests.
fn read_through_on() -> bool {
    #[cfg(test)]
    if NO_READ_THROUGH.with(std::cell::Cell::get) {
        return false;
    }
    true
}

/// The names from `base` down to `target`, each a plain name.
fn names_of(target: &Path, base: &Path) -> Result<Vec<String>, Refusal> {
    let not_below = || Refusal {
        at: target.to_path_buf(),
        why: Why::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("not below {}", base.display()),
        )),
    };
    let names = crate::appdirs::names_below(target, base).ok_or_else(not_below)?;
    if names.is_empty() {
        return Err(not_below());
    }
    names
        .into_iter()
        .map(|n| n.to_str().map(str::to_owned).ok_or_else(not_below))
        .collect()
}

fn absent(at: &HeldPath, name: &str) -> Refusal {
    at.refuse_at(name, Why::Io(not_found()))
}

fn not_found() -> std::io::Error {
    std::io::ErrorKind::NotFound.into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
}

fn is_absent(code: i32) -> bool {
    matches!(
        code,
        STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND | STATUS_DELETE_PENDING
    )
}

/// What an `NTSTATUS` from an open, a rename or a create says about the entry.
fn why_of(code: i32, kind: Kind) -> Why {
    match code {
        STATUS_NOT_A_DIRECTORY => Why::NotFolder,
        STATUS_FILE_IS_A_DIRECTORY if kind == Kind::File => Why::NotFile,
        STATUS_REPARSE_POINT_ENCOUNTERED => Why::Link,
        // SAFETY: a pure conversion of a status code.
        _ => Why::Io(std::io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(code) } as i32,
        )),
    }
}

/// #3743's `open_below` answers in words: a link and a folder by name.
fn why_of_text(text: &str) -> Why {
    if text.contains("is a link") {
        Why::Link
    } else if text.contains("is a folder") {
        Why::NotFile
    } else {
        Why::Io(std::io::Error::other(text.to_string()))
    }
}

/// `file` is a folder (`folder`) or a file, and not a link.
fn kind_of(file: &File, folder: bool) -> Result<(), Why> {
    let meta = file.metadata().map_err(Why::Io)?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(Why::Link);
    }
    match (meta.is_dir(), folder) {
        (true, true) | (false, false) => Ok(()),
        (false, true) => Err(Why::NotFolder),
        (true, false) => Err(Why::NotFile),
    }
}

fn is_link(file: &File) -> std::io::Result<bool> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a live handle; `info` is the struct the call fills in.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

fn links_of(file: &File) -> std::io::Result<u32> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a live handle; `info` is the struct the call fills in.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info.nNumberOfLinks)
}

/// A plain file of one link, owned by SYSTEM or Administrators: the only
/// entry a sealed folder's writer appends to or keeps.
fn plain_and_ours(file: &File) -> Result<(), Why> {
    kind_of(file, false)?;
    if links_of(file).map_err(Why::Io)? > 1 {
        return Err(Why::TooManyLinks);
    }
    let sd: Sd = win::read_sd(file).map_err(Why::Io)?;
    if !super::trusted(&sd.owner) {
        return Err(Why::Foreign { owner: sd.owner });
    }
    Ok(())
}

/// Set the protected DACL of `sddl` (and its owner, when `owner`) on the
/// entry behind `file`, through its handle and on that entry alone
/// (`SetKernelObjectSecurity`): unlike `SetSecurityInfo` and
/// `SetNamedSecurityInfo`, the system's own set never hands a folder's new
/// inheritable entries down to what it holds, on any build.
fn set_own_sd(file: &File, sddl: &str, owner: bool) -> std::io::Result<()> {
    let sd = win::Descriptor::from_sddl(sddl)?;
    let mut info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    if owner {
        info |= OWNER_SECURITY_INFORMATION;
    }
    // SAFETY: a live handle opened with WRITE_DAC (and WRITE_OWNER when
    // `owner`); `sd` is a valid descriptor, alive across the call.
    if unsafe { SetKernelObjectSecurity(file.as_raw_handle() as HANDLE, info, sd.0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ─── the calls ────────────────────────────────────────────────────────────

/// 0: not yet known; 2: this build answers `STATUS_INVALID_PARAMETER` to
/// `OBJ_DONT_REPARSE`, so it is left out.
static DONT_REPARSE: AtomicU8 = AtomicU8::new(0);

/// `NtCreateFile` of `name` below the folder `root` (its `RootDirectory`, so
/// no name above it is looked up again), with `FILE_OPEN_REPARSE_POINT` and,
/// where the build knows it, `OBJ_DONT_REPARSE`; shared for reading, writing
/// and deleting, so a held entry never blocks an operator's removal. `sd`: the
/// descriptor an entry it makes gets, from its creation. Whether it was made.
fn open_below(
    root: &File,
    name: &str,
    access: u32,
    disposition: u32,
    options: u32,
    sd: Option<&win::Descriptor>,
) -> Result<(File, bool), i32> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['\\', '/', ':']) {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    let Ok(bytes) = u16::try_from(wide.len() * 2) else {
        return Err(STATUS_INVALID_PARAMETER);
    };
    let object_name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: wide.as_mut_ptr(),
    };
    let attempt = |flags: u32| {
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: root.as_raw_handle() as HANDLE,
            ObjectName: &object_name,
            Attributes: flags,
            SecurityDescriptor: sd.map_or(std::ptr::null(), |d| d.0 as *const c_void),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle: HANDLE = std::ptr::null_mut();
        // SAFETY: a plain-data struct the call fills in; zeroes are valid.
        let mut io: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        // SAFETY: `attributes` points at `object_name` (into `wide`) and at
        // `sd`'s descriptor, which outlive the call; `root` is a live folder
        // handle. A handle written to `handle` is owned by the caller.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                access,
                &attributes,
                &mut io,
                std::ptr::null(),
                FILE_ATTRIBUTE_NORMAL,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                disposition,
                options | FILE_OPEN_REPARSE_POINT,
                std::ptr::null(),
                0,
            )
        };
        (status, handle, io.Information)
    };
    let with_flag = DONT_REPARSE.load(Ordering::Relaxed) != 2;
    let flags = OBJ_CASE_INSENSITIVE | if with_flag { OBJ_DONT_REPARSE } else { 0 };
    let (mut status, mut handle, mut info) = attempt(flags);
    if with_flag && status == STATUS_INVALID_PARAMETER {
        let again = attempt(OBJ_CASE_INSENSITIVE);
        if again.0 != STATUS_INVALID_PARAMETER {
            DONT_REPARSE.store(2, Ordering::Relaxed);
        }
        (status, handle, info) = again;
    } else if with_flag && status == STATUS_REPARSE_POINT_ENCOUNTERED {
        // The name itself is a reparse point: FILE_OPEN_REPARSE_POINT still
        // opens it as itself (never what it leads to), and the caller
        // refuses it or moves it aside.
        (status, handle, info) = attempt(OBJ_CASE_INSENSITIVE);
    }
    if status < 0 {
        return Err(status);
    }
    // SAFETY: the handle NtCreateFile just gave this process; the `File`
    // closes it, once.
    let file = unsafe { File::from_raw_handle(handle as RawHandle) };
    Ok((file, info == FILE_CREATED))
}

/// Rename the entry behind `file` (opened with `DELETE`) to `to` in the
/// folder behind `into`: `NtSetInformationFile(FileRenameInformation)` with
/// `RootDirectory` set, so the target is named relative to a held folder and
/// never looked up by path.
fn rename_by_handle(file: &File, into: &File, to: &str, replace: bool) -> Result<(), i32> {
    if to.is_empty() || to == "." || to == ".." || to.contains(['\\', '/', ':']) {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let name: Vec<u16> = to.encode_utf16().collect();
    let head = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    let size = head + name.len() * 2;
    // u64s, so the struct is aligned; zeroes are a valid start.
    let mut buf = vec![0u64; size.div_ceil(8) + 1];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: `buf` holds at least `size` bytes, aligned for the struct; the
    // name is copied into the space after its fixed part.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = u8::from(replace);
        (*info).RootDirectory = into.as_raw_handle() as HANDLE;
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            buf.as_mut_ptr().cast::<u8>().add(head).cast::<u16>(),
            name.len(),
        );
    }
    // SAFETY: a plain-data struct the call fills in; zeroes are valid.
    let mut io: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    // SAFETY: a live handle opened with DELETE, a buffer of `size` bytes
    // holding the class's struct, alive across the call.
    let status = unsafe {
        NtSetInformationFile(
            file.as_raw_handle() as HANDLE,
            &mut io,
            buf.as_ptr().cast(),
            size as u32,
            FileRenameInformation,
        )
    };
    if status < 0 { Err(status) } else { Ok(()) }
}

/// Delete the entry behind `file` (opened with `DELETE`) through its handle:
/// `FileDispositionInfoEx` with POSIX semantics (the name goes at once, even
/// while another process reads the file) and read-only ignored, or
/// #3743's `FileDispositionInfo` where the build or the file system does not
/// know it.
fn dispose(file: &File) -> std::io::Result<()> {
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: a live handle opened with DELETE; `info` is the class's struct,
    // alive across the call.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfoEx,
            (&raw const info).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    // ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, ERROR_INVALID_PARAMETER:
    // a build or a file system without it.
    if matches!(e.raw_os_error(), Some(1 | 50 | 87)) {
        return win::delete_by_handle(file);
    }
    Err(e)
}

/// The entries of the folder behind `dir`, through its handle.
fn list(dir: &File) -> std::io::Result<Vec<HeldEntry>> {
    const WINDOWS_TO_UNIX_100NS: i64 = 116_444_736_000_000_000;
    let mut buf = vec![0u64; 8 * 1024];
    let mut class = FileIdExtdDirectoryRestartInfo;
    let mut out = Vec::new();
    loop {
        // SAFETY: a live folder handle opened for listing and synchronous
        // I/O; `buf` is a buffer of the size given, aligned for the struct.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                dir.as_raw_handle() as HANDLE,
                class,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
            )
        };
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
                return Ok(out);
            }
            return Err(e);
        }
        class = FileIdExtdDirectoryInfo;
        let base = buf.as_ptr().cast::<u8>();
        let name_at = std::mem::offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
        let mut offset = 0usize;
        loop {
            // SAFETY: the call wrote whole entries into `buf`; each starts at
            // an 8-byte boundary the previous entry's offset names, and its
            // name of `FileNameLength` bytes follows the fixed part.
            let (entry, name) = unsafe {
                let at = base.add(offset);
                let entry: FILE_ID_EXTD_DIR_INFO = std::ptr::read_unaligned(at.cast());
                let units = entry.FileNameLength as usize / 2;
                let name = std::slice::from_raw_parts(at.add(name_at).cast::<u16>(), units);
                (entry, String::from_utf16_lossy(name))
            };
            if name != "." && name != ".." {
                out.push(HeldEntry {
                    name,
                    is_dir: entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                    is_link: entry.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
                    len: entry.EndOfFile.max(0) as u64,
                    modified_unix: (entry.LastWriteTime - WINDOWS_TO_UNIX_100NS) / 10_000_000,
                });
            }
            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += entry.NextEntryOffset as usize;
        }
    }
}

/// The removal's first pass: every link below `dir`, never followed.
fn scan_links(dir: &File, at: &Path, links: &mut Vec<(PathBuf, Why)>) {
    let entries = match list(dir) {
        Ok(entries) => entries,
        Err(e) => return links.push((at.to_path_buf(), Why::Io(e))),
    };
    for entry in entries {
        let path = at.join(&entry.name);
        if entry.is_link {
            links.push((path, Why::Link));
        } else if entry.is_dir {
            let options = FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT;
            match open_below(dir, &entry.name, FOLDER, FILE_OPEN, options, None) {
                Ok((sub, _)) if is_link(&sub).unwrap_or(true) => links.push((path, Why::Link)),
                Ok((sub, _)) => scan_links(&sub, &path, links),
                Err(code) if is_absent(code) => {}
                Err(code) => links.push((path, why_of(code, Kind::Dir))),
            }
        }
    }
}

/// The removal's delete pass, below `dir`: whether everything there went.
fn delete_below(dir: &File, at: &Path, out: &mut TreeRemoval) -> bool {
    let entries = match list(dir) {
        Ok(entries) => entries,
        Err(e) => {
            out.left.push((at.to_path_buf(), Why::Io(e)));
            return false;
        }
    };
    let mut all = true;
    for entry in entries {
        let path = at.join(&entry.name);
        let kind = if entry.is_dir {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE
        };
        let access = if entry.is_dir { TREE } else { MOVE };
        let found = match open_below(
            dir,
            &entry.name,
            access,
            FILE_OPEN,
            kind | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
        ) {
            Ok((found, _)) => found,
            Err(code) if is_absent(code) => continue,
            Err(code) => {
                out.left.push((path, why_of(code, Kind::Dir)));
                all = false;
                continue;
            }
        };
        if is_link(&found).unwrap_or(true) {
            out.left.push((path, Why::Link));
            all = false;
            continue;
        }
        if entry.is_dir && !delete_below(&found, &path, out) {
            all = false;
            continue;
        }
        match dispose(&found) {
            Ok(()) if !entry.is_dir => out.removed_files += 1,
            Ok(()) => {}
            Err(e) => {
                out.left.push((path, Why::Io(e)));
                all = false;
            }
        }
    }
    all
}

/// The final path of the entry behind `file` (`FILE_NAME_NORMALIZED`,
/// `VOLUME_NAME_DOS`), without `\\?\`; a network path keeps its `UNC\`.
fn final_path(file: &File) -> std::io::Result<PathBuf> {
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: a live handle; `buf` holds `len` units.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle() as HANDLE,
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if n == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            break;
        }
        buf.resize(n + 1, 0);
    }
    Ok(PathBuf::from(strip_verbatim(&String::from_utf16_lossy(
        &buf,
    ))))
}

/// `GetLongPathNameW(path)`: each component's own long name (an 8.3 short
/// name turned into the long one), a link's included, so a redirect still
/// shows as a mismatch with the final path.
fn long_path(path: &Path) -> std::io::Result<PathBuf> {
    use std::os::windows::ffi::OsStrExt as _;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: a NUL-terminated path; `buf` holds `len` units.
        let n =
            unsafe { GetLongPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            break;
        }
        buf.resize(n + 1, 0);
    }
    Ok(PathBuf::from(strip_verbatim(&String::from_utf16_lossy(
        &buf,
    ))))
}

fn strip_verbatim(text: &str) -> String {
    text.strip_prefix(r"\\?\").unwrap_or(text).to_string()
}

fn is_unc(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.starts_with(r"UNC\") || text.starts_with(r"\\")
}

/// The same path, as Windows names it: without regard to case, `\` and `/`
/// alike, a trailing separator ignored.
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// Tests only: whether this build honours `OBJ_DONT_REPARSE`, read by opening
/// `through` (a name with a link in its middle) below `root`.
#[cfg(test)]
pub(super) fn dont_reparse_answer(root: &File, through: &str) -> &'static str {
    let mut wide: Vec<u16> = through.encode_utf16().collect();
    let bytes = (wide.len() * 2) as u16;
    let object_name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root.as_raw_handle() as HANDLE,
        ObjectName: &object_name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: as in `open_below`.
    let mut io: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    // SAFETY: as in `open_below`; a handle it gives is closed below.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            READ_ATTRIBUTES | SYNCHRONIZE,
            &attributes,
            &mut io,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status >= 0 {
        // SAFETY: the handle the call just gave; closed once.
        drop(unsafe { File::from_raw_handle(handle as RawHandle) });
        "no: the open went through the link"
    } else if status == STATUS_REPARSE_POINT_ENCOUNTERED {
        "yes: STATUS_REPARSE_POINT_ENCOUNTERED"
    } else if status == STATUS_INVALID_PARAMETER {
        "no: STATUS_INVALID_PARAMETER (the build does not know the flag)"
    } else {
        "unknown: another status"
    }
}

#[cfg(test)]
mod tests;

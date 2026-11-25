// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Cross-platform single-instance lock for `roomlerd run`.
//!
//! Prevents an interactive `roomlerd run` from racing the
//! Scheduled-Task / systemd-launched copy in the same user session.
//! Two simultaneous agents would compete for the same agent JWT and
//! stomp each other every ~25 s as one's keepalive forces the
//! server-side WS to drop the other.
//!
//! - **Windows**: a pair of per-session named mutexes,
// RETIRED-NAME-ANCHOR-BEGIN
// The legacy mutex and lock-file names are held ON PURPOSE: an OLD instance
// only knows the old name, so dropping it would let two daemons run at once
// across an upgrade — the exact thing this module exists to prevent.
//!   `Local\Roomler-<sha>` (post-P3D-rename) AND `Local\RoomlerAgent-<sha>`
//!   (pre-rename). Acquiring both means an OLD `roomlerd run` and a
//!   NEW one can't both run during the rename window — either one already
//!   holding either name blocks the other. The `Local\` namespace scopes
//!   each mutex to one Terminal Services session, so on a multi-user host
//!   (rare for the agent's per-user install model, but possible on a
//!   shared workstation) each user has their own lock. `<sha>` is a
//!   SHA-256 prefix of the config path so different enrollments on one
//!   machine for one user are ALSO caught — those would collide on
//!   `agent_id` server-side and stomp each other in subtler ways.
//! - **Unix**: `flock(LOCK_EX | LOCK_NB)` on a file in the runtime dir (`$XDG_RUNTIME_DIR`, else
//!   the cache dir), opened without following a link and taken only as a regular file of the
//!   daemon's own uid with a single name; anything else is refused and left as it is. The kernel
//!   frees the lock when the FD closes (Drop) or the process is reaped: no cleanup after `kill -9`.
//!
//! Subcommands gated: **only `run`**. `enroll`, `service install/
//! uninstall`, `caps`, `displays`, `encoder-smoke`, `self-update` all
//! stay runnable alongside a live agent — they're either short-lived
//! diagnostics or modify external state, never the WS connection.
//!
//! #4403 (Windows): the service's own SYSTEM worker that holds the
//! machine's guard (`machine_guard::relaxes_config_lock`) takes this lock
//! with [`acquire_beside_guard`]. It takes every part that is free and
//! holds it, so every other start finds the lock as before, and it goes on
//! either way: for that worker the machine's guard decides.

#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
use std::path::Path;

/// Outcome of an acquire attempt. The lock is released when the
/// `Acquired` variant's payload is dropped (which happens on every
/// path out of `run_cmd`, including panic via the standard unwind).
pub enum AcquireOutcome {
    Acquired(InstanceLock),
    AlreadyRunning,
}

/// RAII guard. Holds the OS primitive that backs the lock; drop
/// releases it. Implementations are platform-specific and gated
/// below; the type is opaque to callers.
pub struct InstanceLock {
    // Both the new `Roomler-<id>` and legacy `RoomlerAgent-<id>` mutex
    // handles are held for the process lifetime; each `WinMutex::drop`
    // calls CloseHandle, releasing that name so a fresh acquire succeeds.
    // `acquire` holds both; `None` is a part `acquire_beside_guard` did not
    // take (#4403).
    #[cfg(target_os = "windows")]
    _win_new: Option<WinMutex>,
    #[cfg(target_os = "windows")]
    _win_legacy: Option<WinMutex>,
    #[cfg(unix)]
    _unix: std::fs::File,
}

/// Try to take the single-instance lock for the agent identified by
/// `config_path`. Two enrollments on the same host with different
/// config files get distinct locks; the same config from two
/// processes share one lock and the second sees `AlreadyRunning`.
pub fn acquire(config_path: &Path) -> Result<AcquireOutcome> {
    let id = lock_id(config_path);
    #[cfg(target_os = "windows")]
    {
        win_acquire(&id)
    }
    #[cfg(all(unix, not(target_os = "windows")))]
    {
        unix_acquire(&id)
    }
    #[cfg(not(any(target_os = "windows", unix)))]
    {
        // Unsupported platform — caller proceeds without a lock. The
        // agent doesn't ship anywhere outside Win/Linux/macOS today
        // so this branch is dead, kept for `cargo check` coverage.
        let _ = id;
        Ok(AcquireOutcome::Acquired(InstanceLock {}))
    }
}

/// The daemon instance a config path names: the fingerprint this lock is
/// keyed by. FR-97 P4d keys a per-user daemon's stop event by it too, so
/// `roomlerd service stop` reaches the daemon of the config it resolved, as
/// a second `run` would collide with that daemon and no other.
pub fn instance_id(config_path: &Path) -> String {
    lock_id(config_path)
}

/// Stable 12-hex-char fingerprint of the config path. Collisions are
/// astronomically unlikely; even if they occurred the worst-case is
/// "two agents with different configs share a lock" which is the
/// safer failure mode (one waits, neither stomps).
fn lock_id(config_path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(config_path.to_string_lossy().as_bytes());
    hex::encode(&h.finalize()[..6])
}

// ---------------------------------------------------------------------------
// Windows: named mutex via CreateMutexW + ERROR_ALREADY_EXISTS detection.
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
struct WinMutex(*mut std::ffi::c_void);

// SAFETY: WinMutex wraps a HANDLE; both CloseHandle and the wait
// primitives are thread-safe per MSDN. Send + Sync are sound for our
// use (we never touch the handle from multiple threads concurrently
// in the first place — the lock guard is held in `run_cmd`'s stack).
#[cfg(target_os = "windows")]
unsafe impl Send for WinMutex {}
#[cfg(target_os = "windows")]
unsafe impl Sync for WinMutex {}

#[cfg(target_os = "windows")]
impl Drop for WinMutex {
    fn drop(&mut self) {
        unsafe extern "system" {
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        // SAFETY: handle came from CreateMutexW; we own it.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(target_os = "windows")]
type WinHandle = *mut std::ffi::c_void;

#[cfg(target_os = "windows")]
unsafe extern "system" {
    fn CreateMutexW(
        lp_mutex_attributes: *mut std::ffi::c_void,
        b_initial_owner: i32,
        lp_name: *const u16,
    ) -> WinHandle;
    fn GetLastError() -> u32;
}

/// Create one named mutex. Returns:
///   - `Ok(Some(handle))` — we freshly created it (nobody else holds it);
///     the caller owns `handle` and must keep it alive / close it.
///   - `Ok(None)` — it already existed (another process holds it); the
///     handle we opened is closed here, nothing to keep.
///   - `Err` — CreateMutexW failed outright.
#[cfg(target_os = "windows")]
fn create_named_mutex(name: &str) -> Result<Option<WinHandle>> {
    create_named_mutex_code(name)
        .map_err(|err| anyhow::anyhow!("CreateMutexW failed for {name} (GetLastError={err})"))
}

/// [`create_named_mutex`], whose `Err` is CreateMutexW's `GetLastError` code.
#[cfg(target_os = "windows")]
fn create_named_mutex_code(name: &str) -> std::result::Result<Option<WinHandle>, u32> {
    const ERROR_ALREADY_EXISTS: u32 = 183;
    unsafe extern "system" {
        fn CloseHandle(handle: WinHandle) -> i32;
    }
    let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: NULL security attributes is allowed (default ACL); name
    // is null-terminated UTF-16; b_initial_owner=FALSE means we don't
    // hold the mutex (we don't need to — we just want existence
    // detection).
    let h = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name_w.as_ptr()) };
    if h.is_null() {
        return Err(unsafe { GetLastError() });
    }
    let last = unsafe { GetLastError() };
    if last == ERROR_ALREADY_EXISTS {
        // Some other process (or, on retry, ourselves) already owns the
        // mutex object. Close the handle we opened and report "taken".
        unsafe {
            CloseHandle(h);
        }
        return Ok(None);
    }
    Ok(Some(h))
}

/// A config's two names, the new one and the legacy one.
#[cfg(target_os = "windows")]
fn win_names(id: &str) -> (String, String) {
    (
        format!("Local\\Roomler-{id}"),
        format!("Local\\RoomlerAgent-{id}"),
    )
}

#[cfg(target_os = "windows")]
fn win_acquire(id: &str) -> Result<AcquireOutcome> {
    // Acquire BOTH names so an OLD `RoomlerAgent-<id>` instance and a NEW
    // `Roomler-<id>` instance can't both run during the rename window.
    let (new_name, legacy_name) = win_names(id);

    // New name first.
    let Some(new_h) = create_named_mutex(&new_name)? else {
        return Ok(AcquireOutcome::AlreadyRunning);
    };
    // Wrap immediately so that if the legacy acquire below reports
    // AlreadyRunning (or errors), this handle is dropped/closed via
    // WinMutex::drop and we don't leak the `Roomler-<id>` mutex.
    let new_mutex = WinMutex(new_h);

    let Some(legacy_h) = create_named_mutex(&legacy_name)? else {
        // Legacy name is held by an old instance. `new_mutex` drops here,
        // releasing the new name we just took.
        return Ok(AcquireOutcome::AlreadyRunning);
    };

    // Both freshly created — hold both handles for the process lifetime.
    Ok(AcquireOutcome::Acquired(InstanceLock {
        _win_new: Some(new_mutex),
        _win_legacy: Some(WinMutex(legacy_h)),
    }))
}

/// #4403 (Windows) — a part of the lock: its new name or its legacy one.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    New,
    Legacy,
}

/// #4403 (Windows) — a part [`acquire_beside_guard`] did not take.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotTaken {
    pub part: Part,
    /// `None`: another process holds it. `Some(code)`: CreateMutexW failed
    /// with this `GetLastError` code.
    pub error: Option<u32>,
}

#[cfg(target_os = "windows")]
impl std::fmt::Display for NotTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let part = match self.part {
            Part::New => "new",
            Part::Legacy => "legacy",
        };
        match self.error {
            None => write!(f, "{part}: held by another process"),
            Some(code) => write!(f, "{part}: CreateMutexW failed (GetLastError={code})"),
        }
    }
}

/// #4403 (Windows) — what [`acquire_beside_guard`] took.
#[cfg(target_os = "windows")]
pub struct BesideGuard {
    /// Holds each part it took for the process lifetime, as [`acquire`]'s
    /// lock does.
    pub lock: InstanceLock,
    /// Each part it did not take, the new one first.
    pub not_taken: Vec<NotTaken>,
}

/// #4403 (Windows) — this config's lock for the service's own SYSTEM worker
/// that holds the machine's guard (`machine_guard::relaxes_config_lock`). It
/// takes each part of the lock that is free and holds it for the process
/// lifetime, as [`acquire`] does, so every later start on this config finds
/// the lock as before. It never answers [`AcquireOutcome::AlreadyRunning`]:
/// for that worker the machine's guard, not this lock, decides whether it
/// runs. Every other run calls [`acquire`].
#[cfg(target_os = "windows")]
pub fn acquire_beside_guard(config_path: &Path) -> BesideGuard {
    let (new_name, legacy_name) = win_names(&lock_id(config_path));
    let mut not_taken = Vec::new();
    let mut take = |part: Part, name: &str| match create_named_mutex_code(name) {
        Ok(Some(handle)) => Some(WinMutex(handle)),
        Ok(None) => {
            not_taken.push(NotTaken { part, error: None });
            None
        }
        Err(code) => {
            not_taken.push(NotTaken {
                part,
                error: Some(code),
            });
            None
        }
    };
    let new = take(Part::New, &new_name);
    let legacy = take(Part::Legacy, &legacy_name);
    BesideGuard {
        lock: InstanceLock {
            _win_new: new,
            _win_legacy: legacy,
        },
        not_taken,
    }
}

// ---------------------------------------------------------------------------
// Unix: flock via libc.
// ---------------------------------------------------------------------------

#[cfg(all(unix, not(target_os = "windows")))]
fn unix_acquire(id: &str) -> Result<AcquireOutcome> {
    let path = unix_lock_path(id)?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // SAFETY: geteuid takes nothing and cannot fail.
    lock_at(&path, unsafe { libc::geteuid() })
}

/// The lock on the file at `path`, for a daemon whose effective uid is `euid`.
///
/// The open does not follow a link at the file's own name and does not wait
/// on a file that is not regular (`O_NOFOLLOW | O_NONBLOCK`; std adds
/// `O_CLOEXEC`), and it creates the file when nothing is there. Through that
/// descriptor (fstat) the file must be a regular file of `euid` with a single
/// name ([`lock_file_verdict`]). Anything else is refused, with what was
/// found, and left as it is. Only after these checks: the `flock`, then the
/// pid.
#[cfg(all(unix, not(target_os = "windows")))]
fn lock_at(path: &Path, euid: u32) -> Result<AcquireOutcome> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let opened = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let mut file = match opened {
        Ok(file) => file,
        Err(err) => {
            // The open failed. What is there, read without following a link,
            // names it by the same checks; an open that failed on a regular
            // file of the daemon's own, or on nothing, keeps the system's own
            // reason.
            if let Ok(md) = std::fs::symlink_metadata(path) {
                lock_file_verdict(FileKind::of(md.file_type()), md.uid(), md.nlink(), euid)
                    .map_err(|why| refused(path, &why))?;
            }
            return Err(err).with_context(|| format!("opening {} for lock", path.display()));
        }
    };
    let md = file
        .metadata()
        .with_context(|| format!("reading {} for lock", path.display()))?;
    lock_file_verdict(FileKind::of(md.file_type()), md.uid(), md.nlink(), euid)
        .map_err(|why| refused(path, &why))?;
    let fd = file.as_raw_fd();
    // SAFETY: fd is owned by `file` for the duration of this call;
    // flock with LOCK_NB returns immediately rather than blocking.
    let r = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno == libc::EWOULDBLOCK || errno == libc::EAGAIN {
            return Ok(AcquireOutcome::AlreadyRunning);
        }
        anyhow::bail!("flock failed (errno {errno}) on {}", path.display());
    }
    // Best-effort PID write so an operator inspecting the file sees
    // who's holding the lock. The lock itself is the kernel-side
    // flock; the file content is informational.
    let _ = file.set_len(0);
    let _ = writeln!(file, "{}", std::process::id());
    Ok(AcquireOutcome::Acquired(InstanceLock { _unix: file }))
}

#[cfg(all(unix, not(target_os = "windows")))]
fn unix_lock_path(id: &str) -> Result<std::path::PathBuf> {
    use directories::BaseDirs;
    let dirs = BaseDirs::new().context("could not resolve a base dir")?;
    let parent = dirs
        .runtime_dir()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| dirs.cache_dir().to_path_buf());
    Ok(parent.join(format!("roomler-agent-{id}.lock")))
}

/// A refused lock file's error: its path, what was found, and that it was
/// left as it is.
#[cfg(all(unix, not(target_os = "windows")))]
fn refused(path: &Path, why: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the lock file {} is not a regular file of this daemon's own: {why}; roomlerd left it as it is",
        path.display()
    )
}

/// What the lock file's fstat found (or, for an open that failed, its lstat).
#[cfg(all(unix, not(target_os = "windows")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Regular,
    Folder,
    Link,
    Fifo,
    Socket,
    CharDevice,
    BlockDevice,
    Other,
}

#[cfg(all(unix, not(target_os = "windows")))]
impl FileKind {
    fn of(file_type: std::fs::FileType) -> Self {
        use std::os::unix::fs::FileTypeExt;
        if file_type.is_file() {
            Self::Regular
        } else if file_type.is_dir() {
            Self::Folder
        } else if file_type.is_symlink() {
            Self::Link
        } else if file_type.is_fifo() {
            Self::Fifo
        } else if file_type.is_socket() {
            Self::Socket
        } else if file_type.is_char_device() {
            Self::CharDevice
        } else if file_type.is_block_device() {
            Self::BlockDevice
        } else {
            Self::Other
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Regular => "a regular file",
            Self::Folder => "a folder",
            Self::Link => "a link",
            Self::Fifo => "a FIFO",
            Self::Socket => "a socket",
            Self::CharDevice => "a character device",
            Self::BlockDevice => "a block device",
            Self::Other => "not a regular file",
        }
    }
}

/// The checks the lock file passes before anything is written to it: a
/// regular file, owned by the daemon's effective uid `euid`, with a single
/// name (`links`, the fstat's link count). `Err` says what was found instead.
#[cfg(all(unix, not(target_os = "windows")))]
fn lock_file_verdict(
    kind: FileKind,
    uid: u32,
    links: u64,
    euid: u32,
) -> std::result::Result<(), String> {
    if kind != FileKind::Regular {
        return Err(format!("it is {}", kind.name()));
    }
    if uid != euid {
        return Err(format!("it belongs to uid {uid}, not to uid {euid}"));
    }
    if links != 1 {
        return Err(format!("it has {links} names, not one"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_cfg_path(name: &str) -> PathBuf {
        // Each test gets its own cfg path so tests in parallel don't
        // collide on the lock_id-derived OS primitive.
        std::env::temp_dir().join(format!(
            "roomlerd-test-{}-{}-{}.toml",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ))
    }

    #[test]
    fn lock_id_is_deterministic() {
        let a = lock_id(Path::new("/some/path/config.toml"));
        let b = lock_id(Path::new("/some/path/config.toml"));
        assert_eq!(a, b);
        assert_eq!(a.len(), 12);
    }

    #[test]
    fn lock_id_differs_between_paths() {
        let a = lock_id(Path::new("/config-A.toml"));
        let b = lock_id(Path::new("/config-B.toml"));
        assert_ne!(a, b);
    }

    #[test]
    fn second_acquire_returns_already_running() {
        let cfg = test_cfg_path("second_acquire");
        let first = acquire(&cfg).unwrap();
        let AcquireOutcome::Acquired(_first_lock) = first else {
            panic!("first acquire should succeed");
        };
        let second = acquire(&cfg).unwrap();
        assert!(
            matches!(second, AcquireOutcome::AlreadyRunning),
            "second acquire on the same lock id must report AlreadyRunning"
        );
        // _first_lock drops at end of scope, releasing the OS primitive.
    }

    #[test]
    fn lock_releases_on_drop() {
        let cfg = test_cfg_path("release_on_drop");
        {
            let outcome = acquire(&cfg).unwrap();
            assert!(matches!(outcome, AcquireOutcome::Acquired(_)));
        } // <- guard drops here
        // The release itself is synchronous: flock goes with the last close
        // of the open file description, a named mutex with CloseHandle. What
        // this test does not control is a SIBLING test spawning a process:
        // on Unix the fork copies every fd, this lock's included, and the
        // copy (O_CLOEXEC) closes only at the child's exec, so for that
        // fork-to-exec window the lock is still held. An immediate
        // re-acquire failed that way once in a CI lane of 2,265 tests; two
        // threads spawning `/bin/true` beside a drop/re-acquire loop
        // reproduced it 29 times in 20,000, and never with no spawner. So
        // the re-acquire may wait a bounded while; a lock that never frees
        // still fails.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let after = loop {
            let outcome = acquire(&cfg).unwrap();
            if matches!(outcome, AcquireOutcome::Acquired(_))
                || std::time::Instant::now() >= deadline
            {
                break outcome;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(
            matches!(after, AcquireOutcome::Acquired(_)),
            "after drop, a fresh acquire should succeed"
        );
    }

    #[test]
    fn distinct_config_paths_get_distinct_locks() {
        let cfg_a = test_cfg_path("distinct_a");
        let cfg_b = test_cfg_path("distinct_b");
        let a = acquire(&cfg_a).unwrap();
        let b = acquire(&cfg_b).unwrap();
        assert!(matches!(a, AcquireOutcome::Acquired(_)));
        assert!(
            matches!(b, AcquireOutcome::Acquired(_)),
            "different config paths must take different locks; second must succeed even while first is held"
        );
    }

    #[cfg(unix)]
    fn my_euid() -> u32 {
        // SAFETY: geteuid takes nothing and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// A refused lock's error, as the daemon would print it.
    #[cfg(unix)]
    fn refusal(outcome: Result<AcquireOutcome>) -> String {
        match outcome {
            Err(err) => format!("{err:#}"),
            Ok(AcquireOutcome::Acquired(_)) => "acquired".to_string(),
            Ok(AcquireOutcome::AlreadyRunning) => "already running".to_string(),
        }
    }

    /// (a) A fresh path: the lock creates a regular file of this uid that
    /// holds this process's pid.
    #[cfg(unix)]
    #[test]
    fn a_fresh_path_becomes_a_regular_file_of_this_uid_holding_the_pid() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("instance.lock");
        let AcquireOutcome::Acquired(_lock) = lock_at(&path, my_euid()).expect("the lock") else {
            panic!("a fresh path is taken");
        };
        let md = std::fs::symlink_metadata(&path).expect("the lock file");
        assert!(md.file_type().is_file());
        assert_eq!(md.uid(), my_euid());
        assert_eq!(md.nlink(), 1);
        assert_eq!(
            std::fs::read_to_string(&path).expect("the pid"),
            format!("{}\n", std::process::id())
        );
    }

    /// (b) A regular file of this uid already there (an earlier run's) is the
    /// one reused and locked: the same file, now holding this pid, and a
    /// second take of it finds it held.
    #[cfg(unix)]
    #[test]
    fn a_regular_file_already_there_is_reused_and_locked() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("instance.lock");
        std::fs::write(&path, "4242\n").expect("an earlier run's file");
        let before = std::fs::metadata(&path).expect("the file").ino();
        let AcquireOutcome::Acquired(_lock) = lock_at(&path, my_euid()).expect("the lock") else {
            panic!("an earlier run's file is taken");
        };
        assert_eq!(std::fs::metadata(&path).expect("the file").ino(), before);
        assert_eq!(
            std::fs::read_to_string(&path).expect("the pid"),
            format!("{}\n", std::process::id())
        );
        assert_eq!(refusal(lock_at(&path, my_euid())), "already running");
    }

    /// (c) A FIFO at the lock path is refused, and it is still there, still
    /// the same FIFO.
    #[cfg(unix)]
    #[test]
    fn a_fifo_at_the_lock_path_is_refused_and_left_as_it_is() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("instance.lock");
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path");
        // SAFETY: a NUL-terminated path that lives across the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
        let before = std::fs::symlink_metadata(&path).expect("the FIFO");
        let error = refusal(lock_at(&path, my_euid()));
        assert!(
            error.contains(&path.display().to_string()) && error.contains("a FIFO"),
            "refused, naming the path and what was found: {error}"
        );
        let after = std::fs::symlink_metadata(&path).expect("the FIFO is still there");
        assert!(after.file_type().is_fifo());
        assert_eq!(
            (after.ino(), after.mode(), after.uid()),
            (before.ino(), before.mode(), before.uid())
        );
    }

    /// (d) A folder at the lock path is refused and left as it is, empty.
    #[cfg(unix)]
    #[test]
    fn a_folder_at_the_lock_path_is_refused_and_left_as_it_is() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("instance.lock");
        std::fs::create_dir(&path).expect("a folder");
        let before = std::fs::symlink_metadata(&path).expect("the folder");
        let error = refusal(lock_at(&path, my_euid()));
        assert!(
            error.contains(&path.display().to_string()) && error.contains("a folder"),
            "refused, naming the path and what was found: {error}"
        );
        let after = std::fs::symlink_metadata(&path).expect("the folder is still there");
        assert!(after.file_type().is_dir());
        assert_eq!(
            (after.ino(), after.mode(), after.uid()),
            (before.ino(), before.mode(), before.uid())
        );
        assert_eq!(std::fs::read_dir(&path).expect("the folder").count(), 0);
    }

    /// A regular file whose owner is not the daemon's uid is refused and left
    /// as it is: its contents, size, mode and time. The daemon's uid here is
    /// a stand-in that is not this process's own; no file changes owner.
    #[cfg(unix)]
    #[test]
    fn a_regular_file_of_another_uid_is_refused_and_left_as_it_is() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("instance.lock");
        std::fs::write(&path, "4242\n").expect("a file");
        let before = std::fs::metadata(&path).expect("the file");
        let daemon = my_euid().wrapping_add(1);
        let error = refusal(lock_at(&path, daemon));
        assert!(
            error.contains(&path.display().to_string())
                && error.contains(&format!(
                    "it belongs to uid {}, not to uid {daemon}",
                    my_euid()
                )),
            "refused, naming the path and what was found: {error}"
        );
        let after = std::fs::metadata(&path).expect("the file");
        assert_eq!(
            std::fs::read_to_string(&path).expect("its contents"),
            "4242\n"
        );
        assert_eq!(
            (
                after.ino(),
                after.len(),
                after.mode(),
                after.mtime(),
                after.mtime_nsec()
            ),
            (
                before.ino(),
                before.len(),
                before.mode(),
                before.mtime(),
                before.mtime_nsec()
            )
        );
    }

    /// The checks themselves, on stand-in values: only a regular file of the
    /// daemon's uid with exactly one name passes, and each refusal says what
    /// was found.
    #[cfg(unix)]
    #[test]
    fn only_a_regular_file_of_the_daemons_uid_with_one_name_passes() {
        let daemon = 1000;
        assert_eq!(
            lock_file_verdict(FileKind::Regular, daemon, 1, daemon),
            Ok(())
        );
        assert_eq!(lock_file_verdict(FileKind::Regular, 0, 1, 0), Ok(()));
        for (links, why) in [
            (0, "it has 0 names, not one"),
            (2, "it has 2 names, not one"),
        ] {
            assert_eq!(
                lock_file_verdict(FileKind::Regular, daemon, links, daemon),
                Err(why.to_string())
            );
        }
        assert_eq!(
            lock_file_verdict(FileKind::Regular, 0, 1, daemon),
            Err("it belongs to uid 0, not to uid 1000".to_string())
        );
        assert_eq!(
            lock_file_verdict(FileKind::Regular, daemon, 1, 0),
            Err("it belongs to uid 1000, not to uid 0".to_string())
        );
        for (kind, why) in [
            (FileKind::Folder, "it is a folder"),
            (FileKind::Link, "it is a link"),
            (FileKind::Fifo, "it is a FIFO"),
            (FileKind::Socket, "it is a socket"),
            (FileKind::CharDevice, "it is a character device"),
            (FileKind::BlockDevice, "it is a block device"),
            (FileKind::Other, "it is not a regular file"),
        ] {
            assert_eq!(
                lock_file_verdict(kind, daemon, 1, daemon),
                Err(why.to_string()),
                "{kind:?}"
            );
        }
        // What is checked first is what the error names.
        assert_eq!(
            lock_file_verdict(FileKind::Fifo, 0, 2, daemon),
            Err("it is a FIFO".to_string())
        );
        assert_eq!(
            lock_file_verdict(FileKind::Regular, 0, 2, daemon),
            Err("it belongs to uid 0, not to uid 1000".to_string())
        );
    }

    /// Whether a part's name exists, by the same create a start uses. A
    /// handle it opens is closed again at once.
    #[cfg(target_os = "windows")]
    fn exists(name: &str) -> bool {
        match create_named_mutex(name).expect("CreateMutexW") {
            Some(handle) => {
                drop(WinMutex(handle));
                false
            }
            None => true,
        }
    }

    /// #4403 — beside the machine's guard, a free lock is taken whole and held,
    /// so a later strict start on the same config is refused as before, and
    /// each part exists on its own for an older instance that knows only one.
    #[cfg(target_os = "windows")]
    #[test]
    fn beside_the_guard_takes_a_free_lock_and_a_later_start_is_refused() {
        let cfg = test_cfg_path("beside_free");
        let taken = acquire_beside_guard(&cfg);
        assert!(taken.not_taken.is_empty(), "{:?}", taken.not_taken);
        assert!(taken.lock._win_new.is_some() && taken.lock._win_legacy.is_some());
        assert!(
            matches!(acquire(&cfg).unwrap(), AcquireOutcome::AlreadyRunning),
            "a strict start after it must still report AlreadyRunning"
        );
        let (new_name, legacy_name) = win_names(&lock_id(&cfg));
        assert!(exists(&new_name) && exists(&legacy_name));
        drop(taken);
        assert!(
            matches!(acquire(&cfg).unwrap(), AcquireOutcome::Acquired(_)),
            "the lock is released with the worker"
        );
    }

    /// #4403 — beside the machine's guard, a part already held is not taken: the
    /// acquire goes on, holds each part that was free and names each part it
    /// did not take. Here this test holds the parts itself.
    #[cfg(target_os = "windows")]
    #[test]
    fn beside_the_guard_goes_on_and_names_each_part_it_did_not_take() {
        let held = NotTaken {
            part: Part::New,
            error: None,
        };
        // Both parts held: it holds none and names both.
        let cfg = test_cfg_path("beside_both_held");
        let AcquireOutcome::Acquired(_mine) = acquire(&cfg).unwrap() else {
            panic!("this test takes the lock first");
        };
        let taken = acquire_beside_guard(&cfg);
        assert!(taken.lock._win_new.is_none() && taken.lock._win_legacy.is_none());
        assert_eq!(
            taken.not_taken,
            vec![
                held,
                NotTaken {
                    part: Part::Legacy,
                    ..held
                }
            ]
        );
        assert_eq!(
            taken.not_taken[0].to_string(),
            "new: held by another process"
        );

        // Only the legacy part held: it holds the new one, and a strict start
        // after it is refused.
        let cfg = test_cfg_path("beside_legacy_held");
        let (_, legacy_name) = win_names(&lock_id(&cfg));
        let _legacy = WinMutex(
            create_named_mutex(&legacy_name)
                .unwrap()
                .expect("the legacy part is free"),
        );
        let taken = acquire_beside_guard(&cfg);
        assert!(taken.lock._win_new.is_some() && taken.lock._win_legacy.is_none());
        assert_eq!(
            taken.not_taken,
            vec![NotTaken {
                part: Part::Legacy,
                ..held
            }]
        );
        assert!(matches!(
            acquire(&cfg).unwrap(),
            AcquireOutcome::AlreadyRunning
        ));
    }

    /// #4403 — a part whose create failed is named with its code.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_part_not_created_is_named_with_its_code() {
        let failed = NotTaken {
            part: Part::Legacy,
            error: Some(5),
        };
        assert_eq!(
            failed.to_string(),
            "legacy: CreateMutexW failed (GetLastError=5)"
        );
    }
}
// RETIRED-NAME-ANCHOR-END

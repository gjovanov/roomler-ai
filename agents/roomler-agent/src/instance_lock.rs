//! Cross-platform single-instance lock for `roomler-agent run`.
//!
//! Prevents an interactive `roomler-agent run` from racing the
//! Scheduled-Task / systemd-launched copy in the same user session.
//! Two simultaneous agents would compete for the same agent JWT and
//! stomp each other every ~25 s as one's keepalive forces the
//! server-side WS to drop the other.
//!
//! - **Windows**: a per-session named mutex `Local\RoomlerAgent-<sha>`.
//!   The `Local\` namespace scopes the mutex to one Terminal Services
//!   session, so on a multi-user host (rare for the agent's per-user
//!   install model, but possible on a shared workstation) each user
//!   has their own lock. `<sha>` is a SHA-256 prefix of the config
//!   path so different enrollments on one machine for one user are
//!   ALSO caught — those would collide on `agent_id` server-side and
//!   stomp each other in subtler ways.
//! - **Unix**: `flock(LOCK_EX | LOCK_NB)` on a file in the runtime
//!   dir (`$XDG_RUNTIME_DIR` or `~/.cache/...` fallback). The kernel
//!   releases the lock when the FD is closed (Drop) or the process
//!   reaped — no stale-lock cleanup needed after a `kill -9`.
//!
//! Subcommands gated: **only `run`**. `enroll`, `service install/
//! uninstall`, `caps`, `displays`, `encoder-smoke`, `self-update` all
//! stay runnable alongside a live agent — they're either short-lived
//! diagnostics or modify external state, never the WS connection.

use anyhow::Result;
#[cfg(unix)]
use anyhow::Context;
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
    #[cfg(target_os = "windows")]
    _win: WinMutex,
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
fn win_acquire(id: &str) -> Result<AcquireOutcome> {
    type Handle = *mut std::ffi::c_void;
    const ERROR_ALREADY_EXISTS: u32 = 183;

    unsafe extern "system" {
        fn CreateMutexW(
            lp_mutex_attributes: *mut std::ffi::c_void,
            b_initial_owner: i32,
            lp_name: *const u16,
        ) -> Handle;
        fn GetLastError() -> u32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    let name = format!("Local\\RoomlerAgent-{id}");
    let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: NULL security attributes is allowed (default ACL); name
    // is null-terminated UTF-16; b_initial_owner=FALSE means we don't
    // hold the mutex (we don't need to — we just want existence
    // detection).
    let h = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name_w.as_ptr()) };
    if h.is_null() {
        let err = unsafe { GetLastError() };
        anyhow::bail!("CreateMutexW failed (GetLastError={err})");
    }
    let last = unsafe { GetLastError() };
    if last == ERROR_ALREADY_EXISTS {
        // Some other process (or, on retry, ourselves) already owns
        // the mutex object. Close our handle and report.
        unsafe {
            CloseHandle(h);
        }
        return Ok(AcquireOutcome::AlreadyRunning);
    }
    Ok(AcquireOutcome::Acquired(InstanceLock {
        _win: WinMutex(h),
    }))
}

// ---------------------------------------------------------------------------
// Unix: flock via libc.
// ---------------------------------------------------------------------------

#[cfg(all(unix, not(target_os = "windows")))]
fn unix_acquire(id: &str) -> Result<AcquireOutcome> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    let path = unix_lock_path(id)?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {} for lock", path.display()))?;
    let fd = file.as_raw_fd();
    // SAFETY: fd is owned by `file` for the duration of this call;
    // flock with LOCK_NB returns immediately rather than blocking.
    let r = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        let errno = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_cfg_path(name: &str) -> PathBuf {
        // Each test gets its own cfg path so tests in parallel don't
        // collide on the lock_id-derived OS primitive.
        std::env::temp_dir().join(format!(
            "roomler-agent-test-{}-{}-{}.toml",
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
        // Brief settle for the OS to finalize the close. flock release
        // is synchronous; named mutex CloseHandle is also synchronous;
        // but Windows occasionally needs a yield before another
        // CreateMutexW reports ERROR_ALREADY_EXISTS=0. Practically
        // this is sub-microsecond; the test passes without any sleep.
        let after = acquire(&cfg).unwrap();
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
}

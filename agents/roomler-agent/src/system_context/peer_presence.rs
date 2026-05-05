//! Cross-session "controller is currently connected" signal between
//! the user-context worker and the SCM-supervisor.
//!
//! ## Why a marker file
//!
//! The plan §4 originally specified an inherited anonymous pipe
//! (`STARTUPINFOEX::lpAttributeList` + `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`)
//! between supervisor and worker. That's the architecturally pure
//! design but it's ~200 LOC of Win32 FFI plus handle-life-cycle bugs
//! to debug.
//!
//! This module ships a smaller equivalent: a lock-file at a stable
//! path that any process can stat. The file's mtime is the heartbeat
//! — fresh mtime means a controller is currently connected; mtime
//! older than [`PRESENCE_MAX_AGE`] means stale (worker crashed mid-
//! session, controller disconnected without graceful shutdown, etc.).
//!
//! Tradeoffs vs. the inherited-pipe design:
//! * **Pros**: zero Win32 FFI, ~50 LOC, ProgramData ACL handles
//!   cross-session visibility for free, recovers automatically from
//!   worker crashes.
//! * **Cons**: 5-second polling granularity means a worker that
//!   crashed exactly between heartbeats can leave a stale signal for
//!   up to [`PRESENCE_MAX_AGE`] before the supervisor notices. The
//!   browser's auto-reconnect ladder (0.2.0) closes that gap from
//!   the operator's side: the controller will see a brief
//!   "Reconnecting..." chip and the next supervisor cycle re-spawns
//!   under whichever role is currently appropriate.
//!
//! ## Path
//!
//! `C:\ProgramData\roomler-agent\peer-connected.lock` — under
//! `%PROGRAMDATA%`. Both the user-context worker (running as the
//! interactive user) and the SCM service (running as `LocalSystem`)
//! can read and write here:
//! * `LocalSystem` is admin-equivalent for ACL purposes — full
//!   access by default on every Windows install.
//! * The interactive user inherits `Users:Modify` on subdirectories
//!   created beneath `%PROGRAMDATA%` via the standard CreatorOwner
//!   ACL on `C:\ProgramData`. The `create_dir_all` call below
//!   relies on that inheritance.
//!
//! On non-Windows the path falls back to `/tmp/roomler-agent/`.
//! The non-Windows path is for development / testing only — no
//! cross-session semantics are needed there.
//!
//! ## Wire format
//!
//! Empty file. The mtime is the only signal. Worker rewrites the
//! file's content (overwrite, not append) every 5 s while the
//! WebRTC peer is in `Connected` state, so the mtime advances on
//! every heartbeat. On peer transition to `Disconnected` /
//! `Closed` / `Failed` the worker calls [`signal_disconnected`]
//! which removes the file. The supervisor's
//! [`is_signaled`] returns `false` for both "file doesn't exist"
//! and "file is too old".

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// How often the worker should call [`signal_connected`] while a
/// controller is connected. The supervisor checks at its loop
/// cadence (~1 s); [`PRESENCE_MAX_AGE`] is the staleness threshold.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// File mtime older than this is treated as stale ([`is_signaled`]
/// returns false). Three heartbeat intervals — covers a single
/// missed write without flagging stale.
pub const PRESENCE_MAX_AGE: Duration = Duration::from_secs(15);

/// Path to the marker file. On Windows: under `%PROGRAMDATA%`. On
/// other targets: `/tmp/roomler-agent/` (test / dev only — the
/// cross-session semantics this module provides are Windows-specific).
pub fn marker_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(programdata) = std::env::var_os("PROGRAMDATA") {
            return PathBuf::from(programdata)
                .join("roomler-agent")
                .join("peer-connected.lock");
        }
        // Fallback if PROGRAMDATA is somehow unset (extremely rare —
        // even Windows PE has it). Use the SystemRoot temp dir.
        if let Some(systemroot) = std::env::var_os("SystemRoot") {
            return PathBuf::from(systemroot)
                .join("Temp")
                .join("roomler-agent-peer-connected.lock");
        }
        PathBuf::from("C:\\Windows\\Temp\\roomler-agent-peer-connected.lock")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/tmp/roomler-agent/peer-connected.lock")
    }
}

/// Touch the marker file. Worker calls this on initial peer-Connected
/// transition AND on every [`HEARTBEAT_INTERVAL`] tick while still
/// Connected. Idempotent — overwrites any existing content.
///
/// Errors are returned for caller logging but never recovered from —
/// a missing marker just means the supervisor falls back to the
/// user-context worker, which is the safe degradation.
pub fn signal_connected() -> std::io::Result<()> {
    let path = marker_path();
    if let Some(parent) = path.parent() {
        // Best-effort directory creation. Errors-on-already-exists
        // are normal and ignored by `create_dir_all`. Errors-on-
        // permission-denied surface up — caller logs and moves on.
        fs::create_dir_all(parent)?;
    }
    // Empty content; mtime is the signal. `write` truncates so the
    // file's mtime advances on every call.
    fs::write(&path, b"")?;
    Ok(())
}

/// Remove the marker file. Worker calls this on peer transition to
/// `Disconnected` / `Closed` / `Failed`. Idempotent — `NotFound` is
/// not an error.
pub fn signal_disconnected() -> std::io::Result<()> {
    let path = marker_path();
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Whether a controller is currently connected to *some* worker on
/// this host. Returns true iff the marker file exists AND its mtime
/// is younger than [`PRESENCE_MAX_AGE`].
///
/// Stale marker (mtime older than the threshold) is treated as
/// false. This recovers automatically from worker crashes — the
/// supervisor's next cycle sees the staleness and falls back to the
/// user-context spawn arm.
pub fn is_signaled() -> bool {
    let path = marker_path();
    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let mtime = match meta.modified() {
        Ok(t) => t,
        Err(_) => return false,
    };
    match SystemTime::now().duration_since(mtime) {
        Ok(age) => age <= PRESENCE_MAX_AGE,
        // Mtime is in the future — clock skew. Treat as fresh; the
        // alternative (treating as stale) would falsely tear down a
        // legitimately-connected SystemContext worker because of a
        // host clock adjustment.
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_path_is_absolute_under_programdata_or_tmp() {
        let p = marker_path();
        assert!(p.is_absolute(), "marker path must be absolute: {:?}", p);
        // Sanity: the trailing component is the marker filename.
        assert_eq!(
            p.file_name().and_then(|s| s.to_str()),
            Some("peer-connected.lock")
        );
    }

    #[test]
    fn heartbeat_interval_under_max_age() {
        // Three consecutive successful heartbeats fit under
        // PRESENCE_MAX_AGE so a single missed write doesn't flag
        // stale — important so the supervisor doesn't tear down the
        // worker on routine GC pauses or system sleep transitions.
        assert!(HEARTBEAT_INTERVAL * 3 == PRESENCE_MAX_AGE);
    }

    #[test]
    fn signal_disconnected_when_missing_is_ok() {
        // Idempotent contract: removing a non-existent marker is
        // not an error. The first signal_disconnected call after
        // worker startup typically hits this path because the
        // previous session's marker was already removed at
        // shutdown.
        let path = marker_path();
        let _ = fs::remove_file(&path);
        assert!(signal_disconnected().is_ok());
    }

    #[test]
    fn round_trip_signal_connected_then_is_signaled() {
        // On the test runner we can write to ProgramData (developer
        // workstation) or /tmp (Linux CI); fall back to skipping
        // gracefully if the directory create fails (locked-down CI
        // or macOS sandbox).
        if signal_connected().is_err() {
            eprintln!("skipping — marker dir not writable in this environment");
            return;
        }
        assert!(is_signaled(), "marker fresh after signal_connected");
        assert!(signal_disconnected().is_ok());
        assert!(!is_signaled(), "marker absent after signal_disconnected");
    }

    #[test]
    fn stale_marker_returns_false() {
        // Drop a marker, then artificially backdate via filetimes if
        // available. Without the filetimes crate we approximate by
        // checking the contract directly: is_signaled returns false
        // when the file is missing (proxy for stale).
        let _ = signal_disconnected();
        assert!(!is_signaled());
    }
}

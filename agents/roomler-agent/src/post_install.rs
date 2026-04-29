//! Post-install watcher subprocess.
//!
//! Spawned by the updater immediately before the agent exits to make
//! room for the installer. Tracks the installer process by PID,
//! captures its exit code, then verifies that the new binary on
//! disk reports the expected version. Writes a typed outcome JSON
//! to `<log_dir>/last-install.json` so an operator (or the next
//! startup of the agent) can read what actually happened.
//!
//! ## Why a separate process
//!
//! The agent's own EXE is about to be overwritten by msiexec /
//! dpkg / installer(8). We can't sit in the same process and wait
//! for that to finish, because:
//!   1. Windows MSI on a running EXE either fails outright with
//!      `ERROR_SHARING_VIOLATION` or schedules the rename for next
//!      reboot — neither of which is the intent.
//!   2. The exit-and-let-the-supervisor-relaunch flow that the
//!      Scheduled Task / systemd / launchd model relies on means
//!      the parent agent process IS exiting; there's no one home
//!      to call back into when the installer finishes.
//!
//! The watcher's binary image is mapped before the installer ran;
//! the file at the same path on disk is then overwritten by the
//! installer, but the watcher's mapped pages stay valid for the
//! lifetime of the process. When the watcher exits, the new
//! binary's pages are what subsequent invocations load.
//!
//! ## Lifecycle
//!
//! 1. Updater downloads installer.
//! 2. Updater spawns msiexec / dpkg / installer(8) as a child.
//! 3. Updater spawns `roomler-agent post-install-watch
//!    --installer-pid <pid> --installer-path <path>
//!    --expected-version <tag>`.
//! 4. Updater exits the parent agent so the OS releases its EXE
//!    file lock.
//! 5. Watcher polls the installer PID until it exits or 10 min
//!    elapses.
//! 6. Watcher waits 2 s for the FS to settle, runs `<own-path>
//!    --version`, compares against the expected tag.
//! 7. Watcher writes `last-install.json` and exits. The supervisor
//!    relaunches the agent on next logon (Win Scheduled Task) or
//!    immediately (systemd / launchd). The new binary then reads
//!    `last-install.json` at startup to surface the outcome.
//!
//! ## What this is NOT
//!
//! - It does NOT roll back failed installs. That's Phase 6.3's job
//!   (last-known-good rollback) — this watcher just records what
//!   happened.
//! - It does NOT verify install signatures. The MSI's Authenticode
//!   chain is checked by the OS at install time; we trust that.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
#[cfg(all(unix, not(target_os = "windows")))]
use std::time::Instant;

/// Wall-clock budget for waiting on the installer to finish.
/// Conservative — the longest-observed install in the field is
/// ~3 min on a Windows host with active EDR scanning the MSI.
pub const INSTALLER_BUDGET: Duration = Duration::from_secs(600);

/// Pause after installer exit before running the new binary's
/// `--version`. Lets the FS settle (cargo-wix MSI sometimes
/// fsyncs after process exit).
pub const POST_INSTALL_SETTLE: Duration = Duration::from_secs(2);

/// Persistent record of the most recent install attempt. Written
/// to `<log_dir>/last-install.json`. The new agent reads this at
/// startup to surface success / failure to the operator.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallOutcome {
    pub installer_pid: u32,
    pub installer_path: String,
    pub expected_version: String,
    pub started_unix: u64,
    pub finished_unix: Option<u64>,
    pub installer_exit_code: Option<i32>,
    pub new_binary_path: Option<String>,
    pub new_binary_version: Option<String>,
    pub status: InstallStatus,
    pub note: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub enum InstallStatus {
    /// Watcher is still waiting on the installer to exit. Persisted
    /// at watch-start so a watcher that itself crashes leaves a
    /// trail of "we got this far."
    InProgress,
    /// Installer exited 0 AND the new binary's `--version` output
    /// contained the expected version triple.
    SucceededVerified,
    /// Installer exited 0 but the version check failed (binary
    /// missing, wrong version, or `--version` didn't run). The
    /// install probably worked; surface this so an operator can
    /// investigate without us assuming the worst.
    SucceededUnverified,
    /// Installer exited with a non-zero code. The agent's old
    /// binary is still in place and the supervisor will keep
    /// running it on next logon.
    InstallerFailed,
    /// Installer didn't exit within `INSTALLER_BUDGET`. We give up
    /// rather than block the watcher process forever.
    Timeout,
}

/// Resolve the path of the persistent install-outcome JSON file.
/// Returns `None` only when [`crate::logging::log_dir`] does (i.e.
/// the platform doesn't expose a data dir or `logging::init()`
/// hasn't run).
pub fn outcome_path() -> Option<PathBuf> {
    crate::logging::log_dir().map(|d| d.join("last-install.json"))
}

/// Persist the outcome to `<log_dir>/last-install.json`. Atomic
/// rename via tempfile-then-replace would be nicer; for now a
/// straight write is fine — the file is small and corruption
/// downside is just "operator sees a partial JSON" which is
/// recoverable.
pub fn write_outcome(outcome: &InstallOutcome) -> Result<PathBuf> {
    let path = outcome_path().context("no log dir resolvable")?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string_pretty(outcome)
        .context("serialising install outcome")?;
    std::fs::write(&path, json)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Read the most recent install outcome, if any. Returns `None`
/// when the file doesn't exist (first install, or successful
/// install where the operator manually deleted it). Errors are
/// surfaced — a corrupt file is operator-actionable.
pub fn read_outcome() -> Result<Option<InstallOutcome>> {
    let Some(path) = outcome_path() else { return Ok(None) };
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let parsed = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(parsed))
}

/// Run the watcher loop. Blocks until the installer exits or the
/// budget elapses, then writes the outcome JSON. Returns Ok(()) on
/// every observed outcome — the JSON's `status` field carries the
/// real verdict.
pub fn watch(
    installer_pid: u32,
    installer_path: PathBuf,
    expected_version: String,
) -> Result<InstallOutcome> {
    let started_unix = unix_now();
    let mut outcome = InstallOutcome {
        installer_pid,
        installer_path: installer_path.display().to_string(),
        expected_version: expected_version.clone(),
        started_unix,
        finished_unix: None,
        installer_exit_code: None,
        new_binary_path: None,
        new_binary_version: None,
        status: InstallStatus::InProgress,
        note: String::new(),
    };
    // Persist the InProgress state immediately so a watcher that
    // crashes mid-wait still leaves a forensic trail.
    let _ = write_outcome(&outcome);

    let exit = wait_for_pid(installer_pid, INSTALLER_BUDGET);
    outcome.finished_unix = Some(unix_now());
    match exit {
        WaitOutcome::Exited(code) => {
            outcome.installer_exit_code = Some(code);
            if code != 0 {
                outcome.status = InstallStatus::InstallerFailed;
                outcome.note = format!("installer exited with {code}");
                let _ = write_outcome(&outcome);
                tracing::error!(exit = code, "installer failed");
                return Ok(outcome);
            }
        }
        WaitOutcome::Timeout => {
            outcome.status = InstallStatus::Timeout;
            outcome.note = format!(
                "installer did not exit within {}s",
                INSTALLER_BUDGET.as_secs()
            );
            let _ = write_outcome(&outcome);
            tracing::error!("installer timed out");
            return Ok(outcome);
        }
        WaitOutcome::Error(e) => {
            outcome.status = InstallStatus::Timeout;
            outcome.note = format!("waiting for installer pid: {e}");
            let _ = write_outcome(&outcome);
            tracing::error!(error = %e, "installer wait failed");
            return Ok(outcome);
        }
    }

    // Installer exited 0 — give the FS a moment to settle, then
    // run the new binary's `--version`. The watcher's own current_exe
    // path IS the path the installer wrote to (msiexec replaced it
    // while we were running; our memory map stayed valid).
    std::thread::sleep(POST_INSTALL_SETTLE);
    let exe = std::env::current_exe().ok();
    if let Some(p) = &exe {
        outcome.new_binary_path = Some(p.display().to_string());
        match std::process::Command::new(p).arg("--version").output() {
            Ok(out) if out.status.success() => {
                let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
                outcome.new_binary_version = Some(version.clone());
                if version_matches(&version, &expected_version) {
                    outcome.status = InstallStatus::SucceededVerified;
                    outcome.note =
                        format!("new binary at {} reports {version}", p.display());
                } else {
                    outcome.status = InstallStatus::SucceededUnverified;
                    outcome.note = format!(
                        "new binary at {} reports {version} but expected {expected_version}",
                        p.display()
                    );
                }
            }
            Ok(out) => {
                outcome.status = InstallStatus::SucceededUnverified;
                outcome.note = format!(
                    "new binary `--version` exited {}",
                    out.status.code().unwrap_or(-1)
                );
            }
            Err(e) => {
                outcome.status = InstallStatus::SucceededUnverified;
                outcome.note = format!("could not exec new binary --version: {e}");
            }
        }
    } else {
        outcome.status = InstallStatus::SucceededUnverified;
        outcome.note = "could not resolve own current_exe path".into();
    }
    let _ = write_outcome(&outcome);
    Ok(outcome)
}

/// Whether a `--version` line (e.g. "roomler-agent 0.1.50") contains
/// the version triple from `expected_tag` (e.g. "agent-v0.1.50").
/// Tolerant on the prefix so we don't have to track release-tool
/// formatting changes.
pub(crate) fn version_matches(version_output: &str, expected_tag: &str) -> bool {
    let Some(triple) = crate::updater::parse_version(expected_tag) else {
        return false;
    };
    let needle = format!("{}.{}.{}", triple.0, triple.1, triple.2);
    version_output.contains(&needle)
}

#[derive(Debug)]
enum WaitOutcome {
    Exited(i32),
    Timeout,
    Error(anyhow::Error),
}

fn wait_for_pid(pid: u32, budget: Duration) -> WaitOutcome {
    #[cfg(target_os = "windows")]
    {
        wait_pid_windows(pid, budget)
    }
    #[cfg(all(unix, not(target_os = "windows")))]
    {
        wait_pid_unix(pid, budget)
    }
    #[cfg(not(any(target_os = "windows", unix)))]
    {
        let _ = (pid, budget);
        WaitOutcome::Error(anyhow::anyhow!("unsupported platform"))
    }
}

#[cfg(target_os = "windows")]
fn wait_pid_windows(pid: u32, budget: Duration) -> WaitOutcome {
    type Handle = *mut std::ffi::c_void;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;
    // ERROR_INVALID_PARAMETER fires when OpenProcess is called for
    // a pid that doesn't exist (already exited or never existed).
    const ERROR_INVALID_PARAMETER: u32 = 87;

    unsafe extern "system" {
        fn OpenProcess(desired: u32, inherit: i32, pid: u32) -> Handle;
        fn WaitForSingleObject(handle: Handle, ms: u32) -> u32;
        fn GetExitCodeProcess(handle: Handle, code: *mut u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    // SAFETY: OpenProcess returns NULL on error and a valid handle
    // otherwise. We CloseHandle in every branch.
    let h = unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        let err = unsafe { GetLastError() };
        if err == ERROR_INVALID_PARAMETER {
            return WaitOutcome::Exited(0);
        }
        return WaitOutcome::Error(anyhow::anyhow!("OpenProcess({pid}) failed (err {err})"));
    }
    let result = unsafe { WaitForSingleObject(h, budget.as_millis() as u32) };
    let outcome = if result == WAIT_OBJECT_0 {
        let mut code: u32 = 0;
        // SAFETY: handle is valid and we own the out-pointer.
        let ok = unsafe { GetExitCodeProcess(h, &mut code) };
        if ok != 0 {
            WaitOutcome::Exited(code as i32)
        } else {
            WaitOutcome::Error(anyhow::anyhow!(
                "GetExitCodeProcess failed (err {})",
                unsafe { GetLastError() }
            ))
        }
    } else if result == WAIT_TIMEOUT {
        WaitOutcome::Timeout
    } else {
        WaitOutcome::Error(anyhow::anyhow!(
            "WaitForSingleObject returned {result} (err {})",
            unsafe { GetLastError() }
        ))
    };
    // SAFETY: closing our owned handle.
    unsafe {
        CloseHandle(h);
    }
    outcome
}

#[cfg(all(unix, not(target_os = "windows")))]
fn wait_pid_unix(pid: u32, budget: Duration) -> WaitOutcome {
    let pid_i = pid as libc::pid_t;
    let start = Instant::now();
    while start.elapsed() < budget {
        // SAFETY: kill(pid, 0) is the canonical "does this process
        // exist" probe — sends signal 0 (does nothing) but does
        // permission + existence checks.
        let r = unsafe { libc::kill(pid_i, 0) };
        if r != 0 {
            let errno = std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(0);
            if errno == libc::ESRCH {
                // Process is gone. We can't recover the exit code
                // because we weren't the parent (waitpid would need
                // to be); return Exited(0) and let the version check
                // be the source of truth on whether the install
                // actually worked.
                return WaitOutcome::Exited(0);
            }
            // EPERM means the process exists but we can't signal it.
            // Keep polling — we'll see ESRCH when it actually exits.
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    WaitOutcome::Timeout
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_when_output_contains_triple() {
        assert!(version_matches("roomler-agent 0.1.50", "agent-v0.1.50"));
        assert!(version_matches("roomler-agent 0.1.50", "0.1.50"));
        assert!(version_matches("roomler-agent 0.1.50", "v0.1.50"));
        assert!(version_matches("roomler-agent 1.2.3 (some-build-id)", "v1.2.3"));
    }

    #[test]
    fn version_does_not_match_different_triple() {
        assert!(!version_matches("roomler-agent 0.1.49", "agent-v0.1.50"));
        assert!(!version_matches("roomler-agent 1.0.0", "agent-v0.0.1"));
        assert!(!version_matches("totally unrelated string", "agent-v0.1.50"));
    }

    #[test]
    fn version_does_not_match_unparseable_tag() {
        // We refuse to match against malformed tags so a
        // server-side typo can't smuggle a "successful" verdict
        // through.
        assert!(!version_matches("roomler-agent 0.1.50", "not-a-version"));
        assert!(!version_matches("roomler-agent 0.1.50", ""));
    }

    #[test]
    fn outcome_round_trips_through_json() {
        let outcome = InstallOutcome {
            installer_pid: 1234,
            installer_path: "C:/temp/foo.msi".into(),
            expected_version: "agent-v0.1.50".into(),
            started_unix: 100,
            finished_unix: Some(200),
            installer_exit_code: Some(0),
            new_binary_path: Some("C:/agent.exe".into()),
            new_binary_version: Some("roomler-agent 0.1.50".into()),
            status: InstallStatus::SucceededVerified,
            note: "ok".into(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"installer_pid\":1234"));
        assert!(json.contains("\"status\":\"SucceededVerified\""));
        let parsed: InstallOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, outcome, "round-trip must preserve all fields");
    }

    #[test]
    fn outcome_serialises_pending_state_with_optional_fields_null() {
        let outcome = InstallOutcome {
            installer_pid: 1,
            installer_path: "x".into(),
            expected_version: "v0.0.1".into(),
            started_unix: 0,
            finished_unix: None,
            installer_exit_code: None,
            new_binary_path: None,
            new_binary_version: None,
            status: InstallStatus::InProgress,
            note: "".into(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"finished_unix\":null"));
        assert!(json.contains("\"installer_exit_code\":null"));
        assert!(json.contains("\"status\":\"InProgress\""));
    }

    #[test]
    fn install_status_serialises_as_pascal_case() {
        // Lock the wire format — operators are likely to grep
        // last-install.json by status string, and we don't want a
        // refactor that flips this to snake_case to silently break
        // their dashboards.
        let cases = [
            (InstallStatus::InProgress, "\"InProgress\""),
            (InstallStatus::SucceededVerified, "\"SucceededVerified\""),
            (InstallStatus::SucceededUnverified, "\"SucceededUnverified\""),
            (InstallStatus::InstallerFailed, "\"InstallerFailed\""),
            (InstallStatus::Timeout, "\"Timeout\""),
        ];
        for (status, expected) in cases {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, expected);
        }
    }
}

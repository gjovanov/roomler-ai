// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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
//! ## Why the watcher runs from a staged COPY on Windows
//!
//! Mapped pages survive the overwrite, but Windows Installer's
//! RestartManager doesn't care about pages — it enumerates the
//! *processes holding the files being replaced* and shuts down the
//! ones whose SID it can manage. A watcher spawned from
//! `<install>\roomlerd.exe` IS such a process, so RM killed it
//! seconds into every install that had contended files (field
//! forensic 2026-08-21 on the dev host: `last-install.json` frozen
//! at `InProgress`, watcher born 02:54:11, RM app-shutdown
//! 02:54:12) — the wedge-recovery and service-start safety nets
//! below never ran on exactly the installs that needed them. The
//! updater therefore copies the EXE into the update staging dir and
//! spawns the watcher from the copy (`updater::stage_watcher_exe`),
//! passing `--origin-exe <install-dir exe>` so flavour
//! classification and the version probe still target the real
//! install rather than the copy's %TEMP% location (which would
//! misclassify as PerUser — same trap as the install wizard,
//! see `spawn_installer_inner`'s doc).
//!
//! ## Lifecycle
//!
//! 1. Updater downloads installer.
//! 2. Updater spawns msiexec / dpkg / installer(8) as a child.
//! 3. Updater spawns `roomlerd post-install-watch
//!    --installer-pid <pid> --installer-path <path>
//!    --expected-version <tag>` — on Windows from a staged COPY of
//!    the EXE with `--origin-exe <install-dir exe>` appended (see
//!    "Why the watcher runs from a staged COPY" above).
//! 4. Updater exits the parent agent so the OS releases its EXE
//!    file lock.
//! 5. Watcher polls the installer PID until it exits or 10 min
//!    elapses. **Windows wedge recovery**: on timeout the watcher
//!    kills the wedged msiexec (plus any leftover msiexec worker
//!    holding the machine-wide `_MSIExecute` mutex) and re-runs the
//!    staged installer ONCE — the exact manual recovery that worked
//!    3/3 in the field (rc.226/rc.228/rc.232 self-update wedges).
//! 6. Watcher waits 2 s for the FS to settle, runs `<own-path>
//!    --version`, compares against the expected tag. Windows: when
//!    the own-path probe can't verify (P4b renamed the install
//!    folder, so a rename-hop watcher runs from the vacated dir),
//!    it falls back to probing `…\Roomler\roomlerd.exe` for the
//!    running flavour.
//! 7. Watcher writes `last-install.json` and exits. Windows
//!    perMachine: as a last act the watcher SETTLES the SCM service
//!    ([`settle_service`], #2711) — waits out a stop or start still in
//!    flight, starts it whenever it reads STOPPED, and believes RUNNING only
//!    once it has held for [`SERVICE_STABLE_FOR`]. Every observed wedge left
//!    the service STOPPED (the MSI never reached StartServices), Restart
//!    Manager can stop the NEW service after the install has finished, and a
//!    host that stays down is worse than any install verdict. Elsewhere the
//!    agent comes back within a minute of the install (the per-user Scheduled
//!    Task, whose registration by the new MSI adds the one-shot starts: #3292,
//!    `service::windows::start_soon_triggers`), within 5 s (systemd,
//!    `Restart=always`), or through the `.pkg`'s postinstall (launchd skips a
//!    clean exit); the new binary reads `last-install.json` at startup.
//! 8. A watcher can be stopped before step 7, and a release cannot reach the
//!    watcher of the release before it. So the next daemon, once healthy, settles
//!    a record still at `InProgress` ([`settle_unreported`], #3302).
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
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(all(unix, not(target_os = "windows")))]
use std::time::Instant;
use tunnel_core::localapi::LastInstallStatus;

/// Wall-clock budget for waiting on the installer to finish.
/// Conservative — the longest-observed install in the field is
/// ~3 min on a Windows host with active EDR scanning the MSI.
pub const INSTALLER_BUDGET: Duration = Duration::from_secs(600);

/// Pause after installer exit before running the new binary's
/// `--version`. Lets the FS settle (cargo-wix MSI sometimes
/// fsyncs after process exit).
pub const POST_INSTALL_SETTLE: Duration = Duration::from_secs(2);

/// #3302 — how long a record can sit at `InProgress` with a watcher still at work
/// on it: [`INSTALLER_BUDGET`], twice on a Windows wedge retry, and the service
/// settle. Past this nothing is going to write a verdict.
pub const WATCHER_LIFETIME: Duration = Duration::from_secs(25 * 60);

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
    /// The new binary answers the expected version. The installer went through
    /// ([`installer_went_through`]), or nobody read how it ended (#3302: gone before
    /// the watcher looked, or the watcher stopped first and the next daemon IS that
    /// version).
    SucceededVerified,
    /// Installer went through ([`installer_went_through`]) but the version check
    /// failed (binary missing, wrong version, or `--version` didn't run). The
    /// install probably worked; surface this so an operator can
    /// investigate without us assuming the worst.
    SucceededUnverified,
    /// Installer exited with a code that is no success, or it was gone before its
    /// exit could be read and the expected version is not on disk (#3302). The
    /// agent's old binary is still in place and the supervisor will keep
    /// running it on next logon.
    InstallerFailed,
    /// Installer didn't exit within `INSTALLER_BUDGET`. We give up
    /// rather than block the watcher process forever.
    Timeout,
    /// #3302 — nothing wrote a verdict in [`WATCHER_LIFETIME`], and the daemon that
    /// found the record is not the version it was for. Written by that daemon, never
    /// by a watcher.
    WatcherStopped,
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
    write_outcome_at(&path, outcome)?;
    Ok(path)
}

/// [`write_outcome`] into a given file: the settle writes a record back where it
/// found it.
fn write_outcome_at(path: &Path, outcome: &InstallOutcome) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string_pretty(outcome).context("serialising install outcome")?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))
}

/// #3302 — every file a record may be in, this process's own log dir first.
///
/// ⚠️ A SystemContext worker logs to `service-logs`, and the watcher it spawns has no
/// service role, so the watcher writes its record into SYSTEM's profile. Reading only
/// its own dir, the worker's settle never found a record: read on a per-machine
/// guest, the record stayed `InProgress` past the clean-run threshold. The watcher is
/// always the outgoing release's code, so the reader is the side that can be fixed.
fn outcome_paths() -> Vec<PathBuf> {
    record_paths_in(crate::logging::log_dir(), crate::logging::profile_log_dir())
}

/// [`outcome_paths`] from its two directories: each once, own first.
fn record_paths_in(own: Option<PathBuf>, profile: Option<PathBuf>) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for dir in [own, profile].into_iter().flatten() {
        let path = dir.join("last-install.json");
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// The record file most recently written among `paths`, if any exists.
fn newest_record_path(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut newest: Option<(&PathBuf, std::time::SystemTime)> = None;
    for path in paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        let written = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if newest.is_none_or(|(_, seen)| written > seen) {
            newest = Some((path, written));
        }
    }
    newest.map(|(path, _)| path.clone())
}

/// The record in one file.
fn read_outcome_at(path: &Path) -> Result<InstallOutcome> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// The record most recently written among `paths`, with its file. The newest file
/// speaks even when it doesn't parse, so an unreadable record is never hidden by an
/// older one that reads.
fn read_newest_outcome(paths: &[PathBuf]) -> Result<Option<(PathBuf, InstallOutcome)>> {
    let Some(path) = newest_record_path(paths) else {
        return Ok(None);
    };
    let outcome = read_outcome_at(&path)?;
    Ok(Some((path, outcome)))
}

/// Read the most recent install outcome, if any. Returns `None`
/// when the file doesn't exist (first install, or successful
/// install where the operator manually deleted it). Errors are
/// surfaced — a corrupt file is operator-actionable.
pub fn read_outcome() -> Result<Option<InstallOutcome>> {
    Ok(read_newest_outcome(&outcome_paths())?.map(|(_, outcome)| outcome))
}

/// Run the watcher loop. Blocks until the installer exits or the
/// budget elapses, then writes the outcome JSON. Returns Ok(()) on
/// every observed outcome — the JSON's `status` field carries the
/// real verdict.
///
/// `origin_exe`: the daemon EXE inside the real install dir, passed
/// when the watcher runs from a staged copy (see the module docs).
/// `None` = the watcher runs from the install dir itself, so its own
/// `current_exe` is the correct probe/flavour source.
/// Whether the watcher must wait on `installer_pid` at all.
///
/// ⚠️ Waiting is never correct for an install that already completed in the
/// daemon's own process. On Linux BOTH install paths are synchronous — the
/// tarball path does the work inline and hands back `std::process::id()`
/// (`updater.rs`, `install_tarball_linux`), and the `.deb` path `wait()`s on its
/// child and hands back an already-reaped pid. So the pid handed over is either
/// the DAEMON's or a corpse, and polling it means the watcher's only exit
/// condition is **its own parent's death** — which is also precisely what kills
/// it, because the daemon's exit tears down the systemd unit's cgroup.
///
/// That is why `last-install.json` sat at `InProgress` on every Linux host: one
/// still showed `agent-v0.4.16` written 2026-08-29 while the host was running
/// 0.4.50. See FR-67 (#1267).
///
/// ⚠️ Windows keeps the wait: msiexec is genuinely asynchronous and
/// `recover_wedged_install` depends on observing its exit.
fn should_wait_for_installer(installer_pid: u32, already_exited: bool) -> bool {
    if already_exited {
        return false;
    }
    // A zero pid is not waitable; `kill(0, 0)` addresses the caller's whole
    // process group, which would make the poll succeed forever.
    installer_pid != 0
}

pub fn watch(
    installer_pid: u32,
    installer_path: PathBuf,
    expected_version: String,
    origin_exe: Option<PathBuf>,
    already_exited: bool,
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

    let exit = if should_wait_for_installer(installer_pid, already_exited) {
        wait_for_pid(installer_pid, INSTALLER_BUDGET)
    } else {
        // Nothing to observe: the install finished before this process
        // existed. Treat it as a clean exit and go straight to verifying the
        // binary that is now on disk.
        tracing::info!(
            installer_pid,
            "installer already completed synchronously — verifying without waiting"
        );
        WaitOutcome::Exited(0)
    };
    match exit {
        WaitOutcome::Exited(code) => {
            outcome.installer_exit_code = Some(code);
            if installer_went_through(code) {
                verify_new_binary(
                    &mut outcome,
                    &expected_version,
                    origin_exe.as_deref(),
                    already_exited,
                );
                note_reboot_asked(&mut outcome, code);
            } else {
                outcome.status = InstallStatus::InstallerFailed;
                outcome.note = format!("installer exited with {code}");
                tracing::error!(exit = code, "installer failed");
            }
        }
        WaitOutcome::AlreadyGone => {
            verify_new_binary(
                &mut outcome,
                &expected_version,
                origin_exe.as_deref(),
                already_exited,
            );
            verdict_with_no_exit_code(&mut outcome);
            tracing::warn!(status = ?outcome.status, "installer was gone before its exit could be read");
        }
        WaitOutcome::Timeout => {
            // Windows: this is the field-reproduced msiexec wedge
            // (client alive forever, service left stopped). Kill it
            // and retry the staged installer once — see the module
            // docs, step 5.
            #[cfg(target_os = "windows")]
            recover_wedged_install(
                installer_pid,
                &installer_path,
                &expected_version,
                origin_exe.as_deref(),
                &mut outcome,
            );
            #[cfg(not(target_os = "windows"))]
            {
                outcome.status = InstallStatus::Timeout;
                outcome.note = format!(
                    "installer did not exit within {}s",
                    INSTALLER_BUDGET.as_secs()
                );
                tracing::error!("installer timed out");
            }
        }
        WaitOutcome::Error(e) => {
            outcome.status = InstallStatus::Timeout;
            outcome.note = format!("waiting for installer pid: {e}");
            tracing::error!(error = %e, "installer wait failed");
        }
    }
    outcome.finished_unix = Some(unix_now());
    // Last act on Windows perMachine: whatever the verdict, never
    // leave the host's SCM service stopped. The settle can take up to
    // SERVICE_SETTLE_BUDGET, so the install's own verdict is on disk
    // first: a watcher that dies mid-settle still leaves it behind.
    // perUser: a failed verdict starts the task.
    #[cfg(target_os = "windows")]
    {
        let _ = write_outcome(&outcome);
        ensure_service_running(&mut outcome, install_flavour(origin_exe.as_deref()));
    }
    let _ = write_outcome(&outcome);
    Ok(outcome)
}

/// The flavour of the install this watcher is watching. Prefers the
/// `--origin-exe` path (the real install dir) over the watcher's own
/// location — a staged-copy watcher runs from %TEMP%, which the path
/// heuristic would misclassify as PerUser and thereby skip the
/// perMachine service check AND pick the wrong elevation path on the
/// wedge retry.
#[cfg(target_os = "windows")]
fn install_flavour(origin: Option<&std::path::Path>) -> crate::updater::WindowsInstallFlavour {
    match origin {
        Some(p) => crate::updater::classify_install_flavour_from_path(p),
        None => crate::updater::current_install_flavour(),
    }
}

// RETIRED-NAME-ANCHOR(4): names the folder hosts installed BEFORE P4b; the
// migration reads it, so the old name is the input, not a leftover.
/// Installer exited 0 — give the FS a moment to settle, then run the
/// new binary's `--version`. Through rc.194 the watcher's own
/// current_exe path was ALSO the path the installer wrote to (msiexec
/// replaced the file in place while we were running; our memory map
/// stayed valid). P4b renamed the install folder (`roomler-agent` →
/// `Roomler`), so on the rename hop the watcher — spawned from the OLD
/// directory — only ever sees the stale pending-delete binary at its
/// own path; when that probe can't verify the expected version, fall
/// back to the flavour-derived RENAMED directory.
///
/// With `origin` set (staged-copy watcher) the probe targets the real
/// install path directly — the watcher's own path is a %TEMP% copy of
/// the OLD binary and would always report the pre-update version.
/// How long to let the filesystem settle before probing the new binary.
///
/// The pause exists for ONE reason, stated at [`POST_INSTALL_SETTLE`]: a
/// cargo-wix MSI sometimes fsyncs after the installer process exits. That is a
/// Windows concern about an installer we watched exit.
///
/// ⚠️ When the install completed synchronously it finished BEFORE this watcher
/// existed — there is nothing left to settle, and the wait is pure exposure.
/// Field-measured 2026-09-06 on three hosts: with the wait skipped (P1) the
/// watcher still died inside this 2 s sleep, killed by the cgroup teardown that
/// follows the daemon's exit, leaving `last-install.json` at `InProgress` even
/// though it had started and logged. Removing a sleep we never needed is the
/// cheap half of closing that race — far cheaper than moving the watcher into a
/// transient unit (FR-67 P6), which pays exit-path risk for the same outcome.
fn settle_before_probe(already_exited: bool) -> Duration {
    if already_exited {
        Duration::ZERO
    } else {
        POST_INSTALL_SETTLE
    }
}

fn verify_new_binary(
    outcome: &mut InstallOutcome,
    expected_version: &str,
    origin: Option<&std::path::Path>,
    already_exited: bool,
) {
    let settle = settle_before_probe(already_exited);
    if !settle.is_zero() {
        std::thread::sleep(settle);
    }
    let exe = origin
        .map(|p| p.to_path_buf())
        .or_else(|| std::env::current_exe().ok());
    if let Some(p) = &exe {
        outcome.new_binary_path = Some(p.display().to_string());
        match std::process::Command::new(p).arg("--version").output() {
            Ok(out) if out.status.success() => {
                let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
                outcome.new_binary_version = Some(version.clone());
                if version_matches(&version, expected_version) {
                    outcome.status = InstallStatus::SucceededVerified;
                    outcome.note = format!("new binary at {} reports {version}", p.display());
                } else {
                    outcome.status = InstallStatus::SucceededUnverified;
                    outcome.note = format!(
                        "new binary at {} reports {version} but expected {expected_version}",
                        p.display()
                    );
                    #[cfg(target_os = "windows")]
                    try_renamed_dir_fallback(outcome, expected_version, Some(p), origin);
                }
            }
            Ok(out) => {
                outcome.status = InstallStatus::SucceededUnverified;
                outcome.note = format!(
                    "new binary `--version` exited {}",
                    out.status.code().unwrap_or(-1)
                );
                #[cfg(target_os = "windows")]
                try_renamed_dir_fallback(outcome, expected_version, Some(p), origin);
            }
            Err(e) => {
                outcome.status = InstallStatus::SucceededUnverified;
                outcome.note = format!("could not exec new binary --version: {e}");
                #[cfg(target_os = "windows")]
                try_renamed_dir_fallback(outcome, expected_version, Some(p), origin);
            }
        }
    } else {
        outcome.status = InstallStatus::SucceededUnverified;
        outcome.note = "could not resolve own current_exe path".into();
        #[cfg(target_os = "windows")]
        try_renamed_dir_fallback(outcome, expected_version, None, origin);
    }
}

/// Windows wedge recovery: the initial msiexec sat past
/// [`INSTALLER_BUDGET`] without exiting. Field pattern (3/3 on the
/// dev host, rc.226→rc.232): the client msiexec is alive but inert,
/// the service is already stopped, and a manual "kill every msiexec,
/// re-run the staged MSI" recovered every single time — the re-run
/// completes in under a minute. Automate exactly that, once.
///
/// The population sweep (not just our PID) matters: the Windows
/// Installer service's own msiexec worker can outlive the client and
/// keep the machine-wide `_MSIExecute` mutex held, which would wedge
/// the retry the same way. Killing mid-transaction is safe here
/// because the retry re-enters Windows Installer, which rolls back /
/// completes any suspended state before installing (observed in all
/// three manual recoveries).
#[cfg(target_os = "windows")]
fn recover_wedged_install(
    wedged_pid: u32,
    installer_path: &std::path::Path,
    expected_version: &str,
    origin: Option<&std::path::Path>,
    outcome: &mut InstallOutcome,
) {
    tracing::error!(
        wedged_pid,
        budget_s = INSTALLER_BUDGET.as_secs(),
        "installer wedged — killing msiexec and retrying the staged installer once"
    );
    outcome.note = format!(
        "installer wedged (no exit in {}s); killing msiexec and retrying once",
        INSTALLER_BUDGET.as_secs()
    );
    let _ = write_outcome(outcome);

    run_recovery_cmd("taskkill", &["/F", "/T", "/PID", &wedged_pid.to_string()]);
    std::thread::sleep(Duration::from_secs(5));
    run_recovery_cmd("taskkill", &["/F", "/IM", "msiexec.exe"]);
    std::thread::sleep(Duration::from_secs(5));

    // #2107 — the one log that names the action this attempt stopped in.
    let kept_note = crate::logging::log_dir()
        .and_then(|dir| {
            let log = PathBuf::from(format!("{}.log", installer_path.display()));
            keep_wedged_log(&log, &dir)
        })
        .map_or_else(String::new, |kept| {
            tracing::warn!(path = %kept.display(), "kept the wedged attempt's MSI log");
            format!(
                "; the wedged attempt's MSI log is kept at {}",
                kept.display()
            )
        });

    // Spawn the retry against the ORIGIN install's flavour — a
    // staged-copy watcher's own path would classify PerUser and take
    // the non-elevated msiexec path, which a perMachine MSI rejects
    // (the 2026-05-15 wizard-class 1625 failure).
    let retry_pid =
        match crate::updater::spawn_installer_as_flavour(installer_path, install_flavour(origin)) {
            Ok(pid) => pid,
            Err(e) => {
                outcome.status = InstallStatus::Timeout;
                outcome.note = format!("installer wedged; retry spawn failed: {e:#}{kept_note}");
                tracing::error!(error = %e, "retry spawn after wedge failed");
                return;
            }
        };
    outcome.installer_pid = retry_pid;
    let _ = write_outcome(outcome);
    match wait_for_pid(retry_pid, INSTALLER_BUDGET) {
        WaitOutcome::Exited(code) if installer_went_through(code) => {
            outcome.installer_exit_code = Some(code);
            // `false`: this is the wedge-recovery retry, where we genuinely
            // waited on msiexec. That is exactly the case POST_INSTALL_SETTLE
            // was written for — cargo-wix can fsync after process exit — so the
            // pause is kept here.
            verify_new_binary(outcome, expected_version, origin, false);
            note_reboot_asked(outcome, code);
            outcome.note = format!(
                "recovered by kill+retry after initial {}s wedge; {}",
                INSTALLER_BUDGET.as_secs(),
                outcome.note
            );
            tracing::info!("retry after wedge succeeded");
        }
        WaitOutcome::Exited(code) => {
            outcome.installer_exit_code = Some(code);
            outcome.status = InstallStatus::InstallerFailed;
            outcome.note = format!("retry after wedge exited with {code}");
            tracing::error!(exit = code, "retry after wedge failed");
        }
        WaitOutcome::AlreadyGone => {
            verify_new_binary(outcome, expected_version, origin, false);
            verdict_with_no_exit_code(outcome);
            outcome.note = format!("retry after wedge: {}", outcome.note);
            tracing::warn!(status = ?outcome.status, "retry was gone before its exit could be read");
        }
        WaitOutcome::Timeout => {
            run_recovery_cmd("taskkill", &["/F", "/T", "/PID", &retry_pid.to_string()]);
            outcome.status = InstallStatus::Timeout;
            outcome.note = format!(
                "retry also wedged (no exit in {}s); killed — see the msiexec /l*v log next to the MSI",
                INSTALLER_BUDGET.as_secs()
            );
            tracing::error!("retry after wedge also timed out");
        }
        WaitOutcome::Error(e) => {
            outcome.status = InstallStatus::Timeout;
            outcome.note = format!("waiting for retry installer pid: {e}");
            tracing::error!(error = %e, "retry installer wait failed");
        }
    }
    outcome.note.push_str(&kept_note);
}

/// #2107 — a wedged attempt's log, copied out of the staging dir: the startup sweep
/// removes that dir an hour later. Two slots, so a host that wedges on every update
/// keeps the last two and nothing piles up. Best effort, and returns where it landed.
#[cfg(any(target_os = "windows", test))]
fn keep_wedged_log(log: &std::path::Path, dir: &std::path::Path) -> Option<PathBuf> {
    if !log.is_file() {
        return None;
    }
    let kept = dir.join("msi-wedged.log");
    let _ = std::fs::rename(&kept, dir.join("msi-wedged.prev.log"));
    std::fs::copy(log, &kept).ok()?;
    Some(kept)
}

/// Spawn a recovery shell-out, log its exit code, never fail the
/// watcher on it. taskkill exit codes are informational here (128 =
/// no such process — already gone, which is fine).
#[cfg(target_os = "windows")]
fn run_recovery_cmd(cmd: &str, args: &[&str]) {
    match std::process::Command::new(cmd).args(args).output() {
        Ok(out) => {
            tracing::info!(cmd, ?args, code = out.status.code(), "recovery command ran");
        }
        Err(e) => {
            tracing::warn!(cmd, ?args, error = %e, "recovery command failed to spawn");
        }
    }
}

/// Make sure the perMachine SCM service is running before the watcher
/// exits. Every observed wedge left the service STOPPED (msiexec never
/// reached StartServices), and nothing else restarts it until a
/// reboot: the host's own stop is a clean one, so the SCM's recovery
/// actions never fire.
///
/// Best-effort by design: on an attended host whose console user is not
/// an administrator, the updater — and therefore this watcher — runs in
/// a worker whose token lacks SERVICE_START on a LocalSystem service.
/// The verdict records that honestly ([`ServiceVerdict::Denied`])
/// instead of pretending the net exists where it can't act.
///
/// A per-user device has a task instead, [`start_user_daemon_after_a_failure`].
#[cfg(target_os = "windows")]
fn ensure_service_running(
    outcome: &mut InstallOutcome,
    flavour: crate::updater::WindowsInstallFlavour,
) {
    if flavour != crate::updater::WindowsInstallFlavour::PerMachine {
        start_user_daemon_after_a_failure(outcome);
        return;
    }
    let name = crate::win_service::NEW_SERVICE_NAME;
    let began = std::time::Instant::now();
    let settle = settle_service(
        name,
        &mut ScmService { name },
        || began.elapsed(),
        std::thread::sleep,
    );
    tracing::info!(
        service = name,
        verdict = settle.verdict.words(),
        starts = settle.starts,
        seen = %settle.trail(),
        "post-install service check"
    );
    if !outcome.note.is_empty() {
        outcome.note.push_str("; ");
    }
    outcome
        .note
        .push_str(&format!("service {name}: {}", settle.describe()));
}

/// #3292 — a per-user daemon has left for this installer, and only a successful
/// install has a `service install` whose one-shot triggers bring it back. A failed
/// one can have run nothing at all: msiexec exits 1618 while another install holds
/// the machine's mutex, so no rollback action had a turn, and the old task fires at
/// the next sign-in. Harmless if a rollback already started it: `IgnoreNew` drops
/// this one. This runs in the watcher of the release being updated FROM, so the
/// first update onto this code does not have it.
#[cfg(target_os = "windows")]
fn start_user_daemon_after_a_failure(outcome: &mut InstallOutcome) {
    if !user_daemon_needs_a_start(&outcome.status) {
        return;
    }
    let said = match crate::service::start() {
        Ok(()) => {
            tracing::info!("the install failed: asked the task to start the daemon");
            "asked the task to start the daemon".to_string()
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "the install failed, and the daemon could not be started");
            format!("could not start the daemon: {e:#}")
        }
    };
    if !outcome.note.is_empty() {
        outcome.note.push_str("; ");
    }
    outcome.note.push_str(&said);
}

/// Exit 0 means the new task is registered, verified or not, and its one-shots do
/// this. A start from here would race Restart Manager's late stop (#2711).
#[cfg(any(target_os = "windows", test))]
fn user_daemon_needs_a_start(status: &InstallStatus) -> bool {
    matches!(
        status,
        InstallStatus::InstallerFailed | InstallStatus::Timeout
    )
}

// ─── #2711 — settle the service, never probe it once ─────────────────────────
//
// Field, 2026-10-22, CORPLAP-1 (0.4.149 → 0.4.151): the install finished at
// 08:15:42.7 and the new service host was running, when Restart Manager — in
// msiexec's "restart previously shut down applications" step — stopped it
// again at 08:15:45.16 (RestartManager event 10002) and never restored it
// (no 10003). The stop took 8.4 s. At 08:15:52.53, one second before it
// completed, the watcher's single `sc start` got 1056 — which the SCM answers
// for a STOP_PENDING service too, since it still holds an instance — and
// recorded "already running". The host stayed down 30 minutes, until a
// person started it. On 10-19 the same late stop had landed 1 s after the
// new host started, and RM's own restore brought it back after 10 s: two of
// ~20 updates on that host, and timing alone decided which way each went.
//
// So the watcher reads the STATE, never a start's exit code; waits out a
// transition someone else has in flight; starts what it finds stopped; and
// believes RUNNING only once it has held. Everything up to the SCM calls is
// pure and runs in the tests on every OS ([`ServiceHandle`] is the seam).

/// How long the watcher keeps the service under watch once the installer
/// has exited. A stop already in flight runs the host's whole stop path —
/// #1683's 8 s graceful budget for the worker, then a terminate — and a
/// start the SCM's own; two minutes covers both, with the bounded starts.
#[cfg(any(target_os = "windows", test))]
const SERVICE_SETTLE_BUDGET: Duration = Duration::from_secs(120);

/// How long the service must read RUNNING, unbroken, before the watcher
/// believes it. A single RUNNING read proves only that no stop had reached
/// the SCM yet: on 2026-10-22 Restart Manager's arrived 3.9 s after the new
/// host started.
#[cfg(any(target_os = "windows", test))]
const SERVICE_STABLE_FOR: Duration = Duration::from_secs(15);

/// Between two reads of the service.
#[cfg(any(target_os = "windows", test))]
const SERVICE_POLL: Duration = Duration::from_millis(500);

/// Starts the watcher makes before it gives up on a service that keeps
/// stopping. A host that dies at start belongs to the SCM's recovery
/// actions (`win_service::apply_recovery_actions`), not to a loop here.
#[cfg(any(target_os = "windows", test))]
const MAX_SERVICE_STARTS: u32 = 3;

/// One read of the SCM service, as the watcher sees it.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceLook {
    Running,
    Stopped,
    StartPending,
    /// The #2711 read: a stop that is still in flight. `StartService`
    /// answers 1056 for it, exactly as for a running service.
    StopPending,
    /// PAUSED, PAUSE_PENDING or CONTINUE_PENDING — the host accepts no
    /// pause, so somebody else's transition, waited out like the others.
    Paused,
    /// No such service: a flavour that never registers one, or an
    /// uninstall that removed it meanwhile.
    Missing,
    /// The SCM would not answer this token, or could not be reached.
    Unreadable,
}

/// Why the watcher stopped watching.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceVerdict {
    /// RUNNING, unbroken, for [`SERVICE_STABLE_FOR`].
    Running,
    /// RUNNING when the budget ran out, but not yet for the whole window.
    RunningBriefly,
    /// STOPPED with every start spent, or when the budget ran out.
    Stopped,
    /// Still mid-transition when the budget ran out.
    Unsettled,
    NotInstalled,
    Unreadable,
    /// `ERROR_SERVICE_DISABLED` (1058): somebody disabled it; not ours to undo.
    Disabled,
    /// `ERROR_ACCESS_DENIED` (5): this context lacks SERVICE_START.
    Denied,
}

#[cfg(any(target_os = "windows", test))]
impl ServiceVerdict {
    fn words(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::RunningBriefly => "running, but not yet held for the stability window",
            Self::Stopped => "STOPPED — could not start it",
            Self::Unsettled => "still changing state when the watch ended",
            Self::NotInstalled => "not installed (non-SCM flavour)",
            Self::Unreadable => "unreadable (the SCM would not answer this context)",
            Self::Disabled => "disabled — not starting",
            Self::Denied => "access denied (watcher context lacks SERVICE_START)",
        }
    }
}

/// The next move of the settle loop.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, PartialEq, Eq)]
enum ServiceStep {
    /// Read again after [`SERVICE_POLL`].
    Wait,
    /// The service is STOPPED: start it.
    Start,
    Done(ServiceVerdict),
}

/// The decision, from one read and the watch so far. Pure.
///
/// ⚠️ A RUNNING read is never enough on its own (see
/// [`SERVICE_STABLE_FOR`]), and no transition is ever cut short: a
/// STOP_PENDING service is waited out until it reads STOPPED, and only
/// then started.
#[cfg(any(target_os = "windows", test))]
fn next_service_step(
    look: ServiceLook,
    running_for: Duration,
    starts: u32,
    elapsed: Duration,
) -> ServiceStep {
    let spent = elapsed >= SERVICE_SETTLE_BUDGET;
    match look {
        ServiceLook::Missing => ServiceStep::Done(ServiceVerdict::NotInstalled),
        ServiceLook::Unreadable => ServiceStep::Done(ServiceVerdict::Unreadable),
        ServiceLook::Running if running_for >= SERVICE_STABLE_FOR => {
            ServiceStep::Done(ServiceVerdict::Running)
        }
        ServiceLook::Running if spent => ServiceStep::Done(ServiceVerdict::RunningBriefly),
        ServiceLook::Running => ServiceStep::Wait,
        ServiceLook::Stopped if spent || starts >= MAX_SERVICE_STARTS => {
            ServiceStep::Done(ServiceVerdict::Stopped)
        }
        ServiceLook::Stopped => ServiceStep::Start,
        ServiceLook::StartPending | ServiceLook::StopPending | ServiceLook::Paused if spent => {
            ServiceStep::Done(ServiceVerdict::Unsettled)
        }
        ServiceLook::StartPending | ServiceLook::StopPending | ServiceLook::Paused => {
            ServiceStep::Wait
        }
    }
}

/// What a refused start means. `None` = keep watching: 1056
/// (`ERROR_SERVICE_ALREADY_RUNNING`) says only that the SCM still holds an
/// instance — a running one, or a STOP_PENDING one, the read #2711's
/// one-shot check got wrong — so the next read decides; anything unknown is
/// explained by the next read as well.
#[cfg(any(target_os = "windows", test))]
fn start_refusal(code: u32) -> Option<ServiceVerdict> {
    match code {
        5 => Some(ServiceVerdict::Denied),
        1058 => Some(ServiceVerdict::Disabled),
        // ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE.
        1060 | 1072 => Some(ServiceVerdict::NotInstalled),
        _ => None,
    }
}

/// The watcher's hands on the service: the SCM in production, a script
/// in the tests.
#[cfg(any(target_os = "windows", test))]
trait ServiceHandle {
    fn look(&mut self) -> ServiceLook;
    /// `Err` carries the raw win32 code.
    fn start(&mut self) -> Result<(), u32>;
}

/// What the settle saw and did.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug)]
struct ServiceSettle {
    verdict: ServiceVerdict,
    starts: u32,
    /// Every read, in order, a run of equal reads kept once.
    seen: Vec<ServiceLook>,
    /// The last win32 code a start was refused with, if any.
    refused: Option<u32>,
}

#[cfg(any(target_os = "windows", test))]
impl ServiceSettle {
    /// The reads, e.g. `StopPending→Stopped→StartPending→Running`, at most
    /// [`Self::TRAIL_MAX`] of them (a flapping service is cut short).
    fn trail(&self) -> String {
        let mut parts: Vec<String> = self
            .seen
            .iter()
            .take(Self::TRAIL_MAX)
            .map(|l| format!("{l:?}"))
            .collect();
        if self.seen.len() > Self::TRAIL_MAX {
            parts.push(format!("…+{}", self.seen.len() - Self::TRAIL_MAX));
        }
        parts.join("→")
    }

    const TRAIL_MAX: usize = 12;

    /// The `last-install.json` note: the verdict, the starts it took, and the
    /// reads that led there.
    fn describe(&self) -> String {
        let mut s = format!(
            "{} ({} start{}; seen {}",
            self.verdict.words(),
            self.starts,
            if self.starts == 1 { "" } else { "s" },
            self.trail()
        );
        if let Some(code) = self.refused {
            s.push_str(&format!("; last start refused with {code}"));
        }
        s.push(')');
        s
    }
}

/// Watch the service until it is settled: RUNNING for
/// [`SERVICE_STABLE_FOR`], or a verdict the watcher cannot act past.
/// `elapsed` is the time since the watch began; `pause` sleeps.
#[cfg(any(target_os = "windows", test))]
fn settle_service(
    name: &str,
    svc: &mut impl ServiceHandle,
    mut elapsed: impl FnMut() -> Duration,
    mut pause: impl FnMut(Duration),
) -> ServiceSettle {
    let mut starts = 0;
    let mut seen: Vec<ServiceLook> = Vec::new();
    let mut refused = None;
    let mut running_since: Option<Duration> = None;
    loop {
        let now = elapsed();
        let look = svc.look();
        if seen.last() != Some(&look) {
            seen.push(look);
            tracing::info!(service = name, state = ?look, at_ms = now.as_millis() as u64, "post-install service watch");
        }
        running_since = match look {
            ServiceLook::Running => running_since.or(Some(now)),
            _ => None,
        };
        let running_for = running_since.map_or(Duration::ZERO, |since| now.saturating_sub(since));
        match next_service_step(look, running_for, starts, now) {
            ServiceStep::Done(verdict) => {
                return ServiceSettle {
                    verdict,
                    starts,
                    seen,
                    refused,
                };
            }
            ServiceStep::Wait => {}
            ServiceStep::Start => {
                starts += 1;
                tracing::warn!(
                    service = name,
                    attempt = starts,
                    at_ms = now.as_millis() as u64,
                    "post-install: the service is STOPPED after the install — starting it"
                );
                if let Err(code) = svc.start() {
                    tracing::warn!(service = name, code, "post-install: service start refused");
                    refused = Some(code);
                    if let Some(verdict) = start_refusal(code) {
                        return ServiceSettle {
                            verdict,
                            starts,
                            seen,
                            refused,
                        };
                    }
                }
            }
        }
        pause(SERVICE_POLL);
    }
}

/// The SCM, through the `windows-service` crate. The state is read with
/// QUERY_STATUS alone, so a context that may not START the service still
/// reports what it saw.
#[cfg(target_os = "windows")]
struct ScmService {
    name: &'static str,
}

#[cfg(target_os = "windows")]
impl ServiceHandle for ScmService {
    fn look(&mut self) -> ServiceLook {
        use windows_service::service::{ServiceAccess, ServiceState};
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
        let Ok(manager) =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        else {
            return ServiceLook::Unreadable;
        };
        let service = match manager.open_service(self.name, ServiceAccess::QUERY_STATUS) {
            Ok(s) => s,
            // ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE.
            Err(e) if matches!(win32_code(&e), Some(1060 | 1072)) => return ServiceLook::Missing,
            Err(_) => return ServiceLook::Unreadable,
        };
        match service.query_status() {
            Ok(status) => match status.current_state {
                ServiceState::Running => ServiceLook::Running,
                ServiceState::Stopped => ServiceLook::Stopped,
                ServiceState::StartPending => ServiceLook::StartPending,
                ServiceState::StopPending => ServiceLook::StopPending,
                ServiceState::Paused
                | ServiceState::PausePending
                | ServiceState::ContinuePending => ServiceLook::Paused,
            },
            Err(_) => ServiceLook::Unreadable,
        }
    }

    fn start(&mut self) -> Result<(), u32> {
        use windows_service::service::ServiceAccess;
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
        // u32::MAX: a failure that carried no win32 code — unknown, so the
        // next read decides (`start_refusal` maps it to "keep watching").
        let code = |e: windows_service::Error| win32_code(&e).unwrap_or(u32::MAX);
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(code)?;
        let service = manager
            .open_service(self.name, ServiceAccess::START)
            .map_err(code)?;
        service.start::<&str>(&[]).map_err(code)
    }
}

#[cfg(target_os = "windows")]
fn win32_code(e: &windows_service::Error) -> Option<u32> {
    match e {
        windows_service::Error::Winapi(io) => io.raw_os_error().map(|c| c as u32),
        _ => None,
    }
}

// RETIRED-NAME-ANCHOR(4): same pre-P4b folder — the watcher must run from
// the directory being vacated, which still carries the retired name.
/// P4b folder-rename fallback: probe the daemon at the RENAMED
/// install directory for this flavour
/// (`…\Roomler\roomlerd.exe`) when the own-path probe couldn't
/// verify the expected version. On the rename-hop upgrade the
/// watcher runs from the vacated `roomler-agent\` directory, so its
/// own path never sees the freshly-installed binary. Record-only,
/// like the rest of the watcher: a version match here upgrades the
/// outcome to `SucceededVerified` with a rename-aware note; anything
/// else leaves the own-path verdict untouched.
#[cfg(target_os = "windows")]
fn try_renamed_dir_fallback(
    outcome: &mut InstallOutcome,
    expected_version: &str,
    own: Option<&std::path::Path>,
    origin: Option<&std::path::Path>,
) {
    let Some(candidate) = renamed_daemon_candidate(install_flavour(origin)) else {
        return;
    };
    if Some(candidate.as_path()) == own || !candidate.is_file() {
        return;
    }
    if let Ok(out) = std::process::Command::new(&candidate)
        .arg("--version")
        .output()
        && out.status.success()
    {
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if version_matches(&version, expected_version) {
            outcome.new_binary_path = Some(candidate.display().to_string());
            outcome.new_binary_version = Some(version.clone());
            outcome.status = InstallStatus::SucceededVerified;
            outcome.note = format!(
                "new binary verified at renamed install dir {} ({version}); watcher ran from the pre-rename path",
                candidate.display()
            );
        }
    }
}

/// The daemon EXE path inside the post-P4b (`Roomler\`) install dir
/// for the given flavour. Split out of [`try_renamed_dir_fallback`]
/// so the pure path derivation is unit-testable without shelling
/// anything. The flavour comes from [`install_flavour`] — origin-
/// aware, so a staged-copy watcher still probes the right root.
#[cfg(target_os = "windows")]
fn renamed_daemon_candidate(flavour: crate::updater::WindowsInstallFlavour) -> Option<PathBuf> {
    crate::updater::install_dir_with_name(flavour, crate::updater::INSTALL_FOLDER_NAME)
        .map(|dir| dir.join("roomlerd.exe"))
}

/// Whether a `--version` line (e.g. "roomlerd 0.1.50") contains
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

/// #2724 — what `roomlerd self-update` does about a daemon running BESIDE it,
/// once its own install has completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BesideDaemon {
    /// The installed binary answers as the release it was installed as: ask
    /// the daemon to restart onto it.
    Restart,
    /// Leave the daemon on the image it runs, for this reason.
    Leave(String),
}

/// #2724 — decide [`BesideDaemon`] from the installed binary's `--version`
/// answer (`None`: it did not answer). Only a binary that answers as the
/// release just installed is restarted onto, P2b's rule for the same question
/// (`replaced_binary.rs`): a file that cannot run would take the daemon down
/// with it.
pub fn beside_daemon_after_install(version_answer: Option<&str>, latest: &str) -> BesideDaemon {
    match version_answer.map(str::trim) {
        Some(answer) if version_matches(answer, latest) => BesideDaemon::Restart,
        Some(answer) => BesideDaemon::Leave(format!(
            "the installed binary answers {answer:?}, not {latest}"
        )),
        None => BesideDaemon::Leave("the installed binary did not answer --version".to_string()),
    }
}

/// #3340 — whether an installer's exit code means the install went through.
/// `msiexec` says 3010 (`ERROR_SUCCESS_REBOOT_REQUIRED`) when it replaced a file in
/// use, and 1641 (`ERROR_SUCCESS_REBOOT_INITIATED`) belongs to the same family. The
/// version on disk still decides the verdict.
fn installer_went_through(code: i32) -> bool {
    matches!(code, 0 | 3010 | 1641)
}

/// #3340 — say so when the installer went through and asked for a reboot.
fn note_reboot_asked(outcome: &mut InstallOutcome, code: i32) {
    if code != 0 {
        outcome.note = format!(
            "{}; installer exit {code}: a reboot was asked for",
            outcome.note
        );
    }
}

/// #3302 — the verdict when the installer was gone before anyone could read how it
/// ended, after [`verify_new_binary`] has looked at the disk. A refused MSI is over
/// in under a second and the watcher starts after it, so the expected version not
/// being there is a failed install, not a success nobody could confirm.
fn verdict_with_no_exit_code(outcome: &mut InstallOutcome) {
    outcome.installer_exit_code = None;
    if outcome.status == InstallStatus::SucceededVerified {
        outcome.note = format!(
            "{}; the installer was gone before its exit could be read",
            outcome.note
        );
    } else {
        outcome.status = InstallStatus::InstallerFailed;
        outcome.note = format!(
            "the installer was gone before its exit could be read, and {}",
            outcome.note
        );
    }
}

/// What [`settle_unreported`] did with a record.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Settle {
    /// It has a verdict already.
    HasVerdict,
    /// It now has one: write it back.
    Settled,
    /// A watcher could still be at work on it, for this long.
    TooSoon(Duration),
}

/// #3302 — give a record still at `InProgress` the verdict its watcher never wrote.
/// Called by a daemon that has reached the clean-run threshold, so no installer is
/// about to replace it and a live watcher has had its chance. A watcher that does
/// come back later overwrites this with what it knows.
///
/// The daemon being the expected version settles it at any age. Anything else waits
/// out [`WATCHER_LIFETIME`], and then says plainly that nobody will ever say more.
pub(crate) fn settle_unreported(
    outcome: &mut InstallOutcome,
    running_version: &str,
    now_unix: u64,
) -> Settle {
    if outcome.status != InstallStatus::InProgress {
        return Settle::HasVerdict;
    }
    if version_matches(running_version, &outcome.expected_version) {
        outcome.status = InstallStatus::SucceededVerified;
        outcome.new_binary_version = Some(format!("roomlerd {running_version}"));
        outcome.note = format!(
            "the watcher left no verdict; this daemon is {running_version}, and has stayed up on it"
        );
    } else {
        let age = now_unix.saturating_sub(outcome.started_unix);
        let life = WATCHER_LIFETIME.as_secs();
        if age < life {
            return Settle::TooSoon(Duration::from_secs(life - age));
        }
        outcome.status = InstallStatus::WatcherStopped;
        outcome.note = format!(
            "no verdict after {} min: the watcher was stopped first. This daemon is \
             {running_version}, and the install was for {}",
            age / 60,
            outcome.expected_version
        );
    }
    outcome.finished_unix = Some(now_unix);
    Settle::Settled
}

/// #3302 — [`settle_unreported`] on the record on disk. Returns how long to wait
/// before looking again, when a watcher could still be at work on it.
pub fn settle_record(running_version: &str) -> Option<Duration> {
    settle_record_in(&outcome_paths(), running_version, unix_now())
}

/// [`settle_record`] over given files, at a given time.
fn settle_record_in(paths: &[PathBuf], running_version: &str, now_unix: u64) -> Option<Duration> {
    let (path, mut outcome) = match read_newest_outcome(paths) {
        Ok(Some(found)) => found,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "could not read the install record");
            return None;
        }
    };
    match settle_unreported(&mut outcome, running_version, now_unix) {
        Settle::HasVerdict => None,
        Settle::TooSoon(wait) => Some(wait),
        Settle::Settled => {
            match write_outcome_at(&path, &outcome) {
                Ok(_) => tracing::warn!(
                    status = ?outcome.status,
                    expected = %outcome.expected_version,
                    "the install's watcher left no verdict; settled its record"
                ),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "could not settle the install record")
                }
            }
            None
        }
    }
}

#[derive(Debug)]
enum WaitOutcome {
    Exited(i32),
    /// #3302 — gone before this watcher looked, so nobody read how it ended. Windows
    /// only: `wait_pid_unix` is never the installer's parent, reads no exit at all,
    /// and keeps its stand-in 0.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    AlreadyGone,
    Timeout,
    // Constructed only in `wait_pid_windows` and the
    // non-windows-non-unix fallback; the unix wait path
    // never produces an error today.
    #[allow(dead_code)]
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
            return WaitOutcome::AlreadyGone;
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
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
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

// ---------------------------------------------------------------------------
// FR-67 P3 — the read half. Everything above writes `last-install.json`;
// nothing read it until this, on any platform: `read_outcome` had no caller
// (the module doc's "the new binary reads it at startup" was never wired),
// so the only hosts anyone looked at were the ones somebody SSHed into.
// ---------------------------------------------------------------------------

impl InstallStatus {
    /// The status word as the wire and the CLI spell it — the SAME spelling
    /// the file carries and the watcher's `post-install verdict:` line prints,
    /// so one grep finds all three surfaces. Locked to the serde spelling by
    /// `install_status_words_match_the_file_spelling`.
    pub fn word(&self) -> &'static str {
        match self {
            InstallStatus::InProgress => LastInstallStatus::IN_PROGRESS,
            InstallStatus::SucceededVerified => LastInstallStatus::SUCCEEDED_VERIFIED,
            InstallStatus::SucceededUnverified => LastInstallStatus::SUCCEEDED_UNVERIFIED,
            InstallStatus::InstallerFailed => LastInstallStatus::INSTALLER_FAILED,
            InstallStatus::Timeout => LastInstallStatus::TIMEOUT,
            InstallStatus::WatcherStopped => LastInstallStatus::WATCHER_STOPPED,
        }
    }
}

/// The record as this daemon reports it — in `roomler status`, in the
/// startup log, and (P5) in its hello. Read fresh from disk every time: on
/// Windows the new service starts while the watcher is still waiting on
/// msiexec, so the verdict lands AFTER this process began and a one-time
/// snapshot would show `InProgress` for the daemon's whole life.
///
/// ⚠️ A SystemContext worker logs to `service-logs`, and the watcher it spawns
/// writes into SYSTEM's profile, so the reader looks in both
/// ([`outcome_paths`], #3302) and `path` names the file the record came from,
/// or, with none, the first place it looked.
pub fn last_install_status() -> LastInstallStatus {
    let paths = outcome_paths();
    let path = newest_record_path(&paths).or_else(|| paths.first().cloned());
    let read = match &path {
        Some(p) if p.exists() => read_outcome_at(p).map(Some),
        _ => Ok(None),
    };
    summarize(read, path.as_deref())
}

/// Pure half of [`last_install_status`]: one word from what the read
/// returned, every field of the record carried along.
///
/// ⚠️ `Absent` and `InProgress` are DIFFERENT words on purpose. "No install
/// has ever been recorded here" and "an install was recorded and its verdict
/// never came" lead to opposite next actions, and the second is the exact
/// state this FR was opened on (`last-install.json` frozen at `InProgress`
/// for `agent-v0.4.16` on a host running 0.4.50). Folding them into one
/// "nothing to show" would hide that bug one layer up from where it was found.
pub(crate) fn summarize(
    read: Result<Option<InstallOutcome>>,
    path: Option<&std::path::Path>,
) -> LastInstallStatus {
    let path = path.map(|p| p.display().to_string());
    let empty = |status: &str| LastInstallStatus {
        status: status.to_string(),
        expected_version: None,
        new_binary_version: None,
        started_unix: None,
        finished_unix: None,
        installer_exit_code: None,
        note: None,
        path: path.clone(),
    };
    match read {
        Ok(None) => empty(LastInstallStatus::ABSENT),
        Ok(Some(o)) => LastInstallStatus {
            status: o.status.word().to_string(),
            expected_version: Some(o.expected_version),
            new_binary_version: o.new_binary_version,
            started_unix: Some(o.started_unix),
            finished_unix: o.finished_unix,
            installer_exit_code: o.installer_exit_code,
            note: (!o.note.is_empty()).then_some(o.note),
            path,
        },
        Err(e) => LastInstallStatus {
            // The chain, not just the outermost context: a context-less
            // one-liner is how 21 consecutive watcher spawn failures read as
            // noise for months (P2).
            note: Some(format!("{e:#}")),
            ..empty(LastInstallStatus::UNREADABLE)
        },
    }
}

/// The level a startup line about `li` belongs at. INFO is the two quiet
/// states — a verified success and no record at all — plus a watcher that
/// can still be at work; everything else is a WARN that wants a look,
/// including the record this FR was opened on.
pub(crate) fn startup_level(li: &LastInstallStatus, now_unix: u64) -> tracing::Level {
    match li.status.as_str() {
        s if s == LastInstallStatus::SUCCEEDED_VERIFIED || s == LastInstallStatus::ABSENT => {
            tracing::Level::INFO
        }
        s if s == LastInstallStatus::IN_PROGRESS && !li.never_reported(now_unix) => {
            tracing::Level::INFO
        }
        _ => tracing::Level::WARN,
    }
}

/// Say at startup what the last install came to — one line, always prefixed
/// `last install:`, so `journalctl -g "last install"` / `roomler logs --grep`
/// finds it whatever the verdict. The words of [`LastInstallStatus`] each
/// get their own sentence; the level is [`startup_level`]'s.
pub fn log_at_startup() {
    let li = last_install_status();
    let now = unix_now();
    let age_s = li.started_unix.map(|s| now.saturating_sub(s));
    let status = li.status.as_str();
    let expected = li.expected_version.as_deref().unwrap_or("-");
    let note = li.note.as_deref().unwrap_or("");
    let path = li.path.as_deref().unwrap_or("(no log dir)");
    if startup_level(&li, now) == tracing::Level::INFO {
        match status {
            s if s == LastInstallStatus::ABSENT => tracing::info!(
                path,
                "last install: no record — this daemon's updater has not installed anything yet"
            ),
            s if s == LastInstallStatus::IN_PROGRESS => tracing::info!(
                expected,
                age_s,
                "last install: in progress — the post-install watcher is still at work"
            ),
            _ => tracing::info!(
                expected,
                age_s,
                version = li.new_binary_version.as_deref().unwrap_or("-"),
                "last install: SucceededVerified"
            ),
        }
    } else {
        match status {
            s if s == LastInstallStatus::IN_PROGRESS => tracing::warn!(
                expected,
                age_s,
                path,
                "last install: NEVER REPORTED — the watcher wrote InProgress and no verdict followed; it was killed before it could write one (FR-67)"
            ),
            s if s == LastInstallStatus::UNREADABLE => {
                tracing::warn!(path, error = note, "last install: record UNREADABLE")
            }
            _ => tracing::warn!(
                status,
                expected,
                age_s,
                exit = ?li.installer_exit_code,
                note,
                "last install: not a verified success — needs a look"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3302 — a process gone before the wait began has no exit code anyone read.
    /// A 0 here made a refused MSI read as a success: the record said exit 0 and the
    /// MSI's own log said 1603.
    ///
    /// A pid no process can hold, not a child that has exited: anything else on the
    /// host with a handle to that child (a CI runner did) keeps its process object
    /// there to ask, and the wait then reads its true code, the next test's case.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_process_gone_before_the_wait_has_no_exit_code() {
        let out = wait_for_pid(0x7FFF_FFFC, Duration::from_secs(1));
        assert!(matches!(out, WaitOutcome::AlreadyGone), "{out:?}");
    }

    /// The other side of it: a process the wait did catch reports what it said.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_process_the_wait_caught_reports_its_exit_code() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit 1603"])
            .spawn()
            .expect("spawn cmd");
        // `child` keeps its handle open, so the process object is there to ask.
        let out = wait_for_pid(child.id(), Duration::from_secs(20));
        child.wait().expect("wait");
        assert!(matches!(out, WaitOutcome::Exited(1603)), "{out:?}");
    }

    fn unreported(expected: &str, started_unix: u64) -> InstallOutcome {
        InstallOutcome {
            installer_pid: 42,
            installer_path: "x.msi".into(),
            expected_version: expected.into(),
            started_unix,
            finished_unix: None,
            installer_exit_code: None,
            new_binary_path: None,
            new_binary_version: None,
            status: InstallStatus::InProgress,
            note: String::new(),
        }
    }

    /// #3302, the field record: a refused MSI, the old version still on disk. It
    /// read `SucceededUnverified` with an exit 0 nobody had read.
    #[test]
    fn an_installer_gone_with_the_old_version_on_disk_failed() {
        let mut o = unreported("agent-v0.4.196", 1000);
        o.status = InstallStatus::SucceededUnverified;
        o.note = "new binary reports roomlerd 0.4.197 but expected agent-v0.4.196".into();
        verdict_with_no_exit_code(&mut o);
        assert_eq!(o.status, InstallStatus::InstallerFailed);
        assert_eq!(o.installer_exit_code, None);
        assert!(o.note.contains("gone before its exit could be read"));
        assert!(o.note.contains("reports roomlerd 0.4.197"), "{}", o.note);
    }

    #[test]
    fn an_installer_gone_with_the_new_version_on_disk_succeeded() {
        let mut o = unreported("agent-v0.4.197", 1000);
        o.status = InstallStatus::SucceededVerified;
        o.note = "new binary reports roomlerd 0.4.197".into();
        verdict_with_no_exit_code(&mut o);
        assert_eq!(o.status, InstallStatus::SucceededVerified);
        assert_eq!(o.installer_exit_code, None);
        assert!(o.note.contains("gone before its exit could be read"));
    }

    /// #3340 — 3010 is a success that wants a reboot, measured on a self-update whose
    /// row was online on the new version 15 s later, and on the record as a failure.
    #[test]
    fn a_reboot_asked_for_is_no_failed_install() {
        for code in [0, 3010, 1641] {
            assert!(installer_went_through(code), "{code}");
        }
        for code in [1, -1, 1602, 1603, 1618, 1625] {
            assert!(!installer_went_through(code), "{code}");
        }
    }

    #[test]
    fn the_note_says_a_reboot_was_asked_for_and_only_then() {
        let mut o = unreported("agent-v0.4.197", 1000);
        o.note = "new binary reports roomlerd 0.4.197".into();
        note_reboot_asked(&mut o, 0);
        assert_eq!(o.note, "new binary reports roomlerd 0.4.197");
        note_reboot_asked(&mut o, 3010);
        assert!(o.note.contains("3010"), "{}", o.note);
        assert!(o.note.contains("reboot"), "{}", o.note);
    }

    #[test]
    fn a_daemon_on_the_expected_version_settles_the_record_at_any_age() {
        let mut o = unreported("agent-v0.4.197", 1000);
        assert_eq!(settle_unreported(&mut o, "0.4.197", 1010), Settle::Settled);
        assert_eq!(o.status, InstallStatus::SucceededVerified);
        assert_eq!(o.finished_unix, Some(1010));
        assert_eq!(o.new_binary_version.as_deref(), Some("roomlerd 0.4.197"));
        assert_eq!(o.installer_exit_code, None, "nobody read it");
        assert!(o.note.contains("no verdict"), "{}", o.note);
    }

    #[test]
    fn another_version_waits_out_the_watchers_life_before_it_says_so() {
        let life = WATCHER_LIFETIME.as_secs();
        let mut o = unreported("agent-v0.4.198", 1000);
        assert_eq!(
            settle_unreported(&mut o, "0.4.197", 1000 + life - 60),
            Settle::TooSoon(Duration::from_secs(60))
        );
        assert_eq!(o.status, InstallStatus::InProgress);
        assert_eq!(o.finished_unix, None);

        assert_eq!(
            settle_unreported(&mut o, "0.4.197", 1000 + life),
            Settle::Settled
        );
        assert_eq!(o.status, InstallStatus::WatcherStopped);
        assert_eq!(o.finished_unix, Some(1000 + life));
        assert!(o.note.contains("0.4.197"), "{}", o.note);
        assert!(o.note.contains("agent-v0.4.198"), "{}", o.note);
    }

    /// A clock stepped back must not settle a live watcher's record, and must not panic.
    #[test]
    fn a_start_in_the_future_reads_as_just_started() {
        let mut o = unreported("agent-v0.4.198", 5000);
        assert_eq!(
            settle_unreported(&mut o, "0.4.197", 1000),
            Settle::TooSoon(WATCHER_LIFETIME)
        );
        assert_eq!(o.status, InstallStatus::InProgress);
    }

    /// A verdict is the watcher's to give: nothing here rewrites one, whatever the
    /// version and however old.
    #[test]
    fn a_record_with_a_verdict_is_left_alone() {
        for status in [
            InstallStatus::SucceededVerified,
            InstallStatus::SucceededUnverified,
            InstallStatus::InstallerFailed,
            InstallStatus::Timeout,
            InstallStatus::WatcherStopped,
        ] {
            let mut o = unreported("agent-v0.4.198", 0);
            o.status = status.clone();
            o.note = "the watcher's word".into();
            assert_eq!(
                settle_unreported(&mut o, "0.4.197", 99_999),
                Settle::HasVerdict
            );
            assert_eq!(o.status, status);
            assert_eq!(o.note, "the watcher's word");
            assert_eq!(o.finished_unix, None);
        }
    }

    /// Give a file a last-write time, so which record is newer doesn't hang on the
    /// clock's resolution.
    fn written_at(path: &Path, secs: u64) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    /// #3302, the per-machine field read: a SystemContext worker logs to
    /// `service-logs`, its watcher into SYSTEM's profile, and the settle looked in
    /// its own dir only.
    #[test]
    fn a_record_is_looked_for_in_the_profile_too_and_each_dir_once() {
        let own = PathBuf::from(r"C:\ProgramData\roomler\roomler\service-logs");
        let profile = PathBuf::from(
            r"C:\Windows\System32\config\systemprofile\AppData\Local\roomler\roomler\data\logs",
        );
        let record = |d: &PathBuf| d.join("last-install.json");
        assert_eq!(
            record_paths_in(Some(own.clone()), Some(profile.clone())),
            vec![record(&own), record(&profile)]
        );
        assert_eq!(
            record_paths_in(Some(profile.clone()), Some(profile.clone())),
            vec![record(&profile)]
        );
        assert_eq!(
            record_paths_in(None, Some(profile.clone())),
            vec![record(&profile)]
        );
        assert!(record_paths_in(None, None).is_empty());
    }

    #[test]
    fn the_settle_finds_the_watchers_record_in_the_other_dir_and_leaves_it_there() {
        let own = tempfile::tempdir().unwrap();
        let profile = tempfile::tempdir().unwrap();
        let paths = record_paths_in(
            Some(own.path().to_path_buf()),
            Some(profile.path().to_path_buf()),
        );
        write_outcome_at(&paths[1], &unreported("agent-v0.4.221", 1000)).unwrap();

        assert_eq!(settle_record_in(&paths, "0.4.221", 1400), None);

        assert!(!paths[0].exists(), "nothing new in the worker's own dir");
        let (path, o) = read_newest_outcome(&paths).unwrap().unwrap();
        assert_eq!(path, paths[1]);
        assert_eq!(o.status, InstallStatus::SucceededVerified);
        assert_eq!(o.finished_unix, Some(1400));
    }

    #[test]
    fn the_most_recently_written_record_is_the_one_read() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let paths = record_paths_in(Some(a.path().to_path_buf()), Some(b.path().to_path_buf()));
        write_outcome_at(&paths[0], &unreported("agent-v0.4.220", 1000)).unwrap();
        write_outcome_at(&paths[1], &unreported("agent-v0.4.221", 2000)).unwrap();
        written_at(&paths[0], 5_000);
        written_at(&paths[1], 6_000);
        let (path, o) = read_newest_outcome(&paths).unwrap().unwrap();
        assert_eq!(
            (path, o.expected_version.as_str()),
            (paths[1].clone(), "agent-v0.4.221")
        );

        written_at(&paths[0], 7_000);
        let (path, o) = read_newest_outcome(&paths).unwrap().unwrap();
        assert_eq!(
            (path, o.expected_version.as_str()),
            (paths[0].clone(), "agent-v0.4.220")
        );
    }

    #[test]
    fn an_unreadable_newest_record_is_not_hidden_by_an_older_one() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let paths = record_paths_in(Some(a.path().to_path_buf()), Some(b.path().to_path_buf()));
        write_outcome_at(&paths[0], &unreported("agent-v0.4.220", 1000)).unwrap();
        std::fs::write(&paths[1], "{ \"status\": \"InProg").unwrap();
        written_at(&paths[0], 5_000);
        written_at(&paths[1], 6_000);
        assert!(read_newest_outcome(&paths).is_err());
        assert_eq!(settle_record_in(&paths, "0.4.220", 9_000), None);
        assert_eq!(
            std::fs::read_to_string(&paths[1]).unwrap(),
            "{ \"status\": \"InProg",
            "the settle writes nothing over a record it could not read"
        );
    }

    #[test]
    fn no_record_anywhere_is_nothing_to_settle() {
        let a = tempfile::tempdir().unwrap();
        let paths = record_paths_in(Some(a.path().to_path_buf()), None);
        assert!(read_newest_outcome(&paths).unwrap().is_none());
        assert_eq!(settle_record_in(&paths, "0.4.221", 1000), None);
        assert!(!paths[0].exists());
    }

    /// What the record holds its life by: the watcher's real budgets must fit in it.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_watchers_life_covers_its_budgets() {
        let budgets = INSTALLER_BUDGET * 2 + SERVICE_SETTLE_BUDGET + POST_INSTALL_SETTLE;
        assert!(budgets < WATCHER_LIFETIME, "{budgets:?}");
    }

    // P4b: pure path derivation behind the folder-rename fallback.
    // LOCALAPPDATA / ProgramFiles are always set on a real Windows
    // session (and on the Windows CI runners), so asserting suffixes
    // is host-state-independent.
    #[cfg(target_os = "windows")]
    #[test]
    fn renamed_daemon_candidate_targets_the_roomler_dir() {
        use crate::updater::WindowsInstallFlavour;
        let per_user = renamed_daemon_candidate(WindowsInstallFlavour::PerUser)
            .expect("install root env var set on Windows");
        assert!(
            per_user.ends_with(
                std::path::Path::new("Programs")
                    .join(crate::updater::INSTALL_FOLDER_NAME)
                    .join("roomlerd.exe")
            ),
            "unexpected perUser candidate {}",
            per_user.display()
        );
        let per_machine = renamed_daemon_candidate(WindowsInstallFlavour::PerMachine)
            .expect("ProgramFiles env var set on Windows");
        assert!(
            per_machine.ends_with(
                std::path::Path::new(crate::updater::INSTALL_FOLDER_NAME).join("roomlerd.exe")
            ) && per_machine
                .to_string_lossy()
                .to_lowercase()
                .contains("program files"),
            "unexpected perMachine candidate {}",
            per_machine.display()
        );
    }

    // Staged-copy watcher: the origin path decides the flavour; the
    // watcher's own %TEMP% location must not. `None` keeps the old
    // own-path classification (a test binary runs from target\ →
    // PerUser).
    #[cfg(target_os = "windows")]
    #[test]
    fn install_flavour_prefers_origin_path() {
        use crate::updater::WindowsInstallFlavour;
        assert_eq!(
            install_flavour(Some(std::path::Path::new(
                r"C:\Program Files\Roomler\roomlerd.exe"
            ))),
            WindowsInstallFlavour::PerMachine
        );
        assert_eq!(
            install_flavour(Some(std::path::Path::new(
                r"C:\Users\x\AppData\Local\Programs\Roomler\roomlerd.exe"
            ))),
            WindowsInstallFlavour::PerUser
        );
        assert_eq!(install_flavour(None), WindowsInstallFlavour::PerUser);
    }

    /// FR-67 P1 — the watcher must not wait for an install that already
    /// finished.
    ///
    /// On Linux both install paths complete inside the daemon: the tarball path
    /// installs inline and returns `std::process::id()`, and the `.deb` path
    /// `wait()`s on its child and returns an already-reaped pid. Polling that pid
    /// means the watcher's only exit condition is its own parent's death — and
    /// that death is exactly what tears down the unit's cgroup and kills it. The
    /// result was `last-install.json` frozen at `InProgress` on every Linux host,
    /// one of them still naming `agent-v0.4.16` while running 0.4.50.
    ///
    /// ⚠️ The first assertion is the load-bearing one and it is the shape of the
    /// real bug: the daemon's own pid, an install that is already done. It fails
    /// against the previous code, which called `wait_for_pid` unconditionally.
    ///
    /// ⚠️ The second pins what must NOT change — a genuinely asynchronous
    /// installer (msiexec, `installer -pkg`) is still waited on, because
    /// `recover_wedged_install` depends on observing its exit.
    /// FR-67 — the settle is exposure, not safety, once the install is done.
    ///
    /// `POST_INSTALL_SETTLE` exists for one documented reason: a cargo-wix MSI
    /// sometimes fsyncs after the installer process exits. That is a claim about
    /// an installer we *watched exit*. When the install completed synchronously
    /// it finished before this watcher existed, so there is nothing to settle.
    ///
    /// ⚠️ Field-measured on three hosts, 2026-09-06: with the wait skipped the
    /// watcher STILL died inside this 2 s sleep — the cgroup teardown that
    /// follows the daemon's exit landed first, and `last-install.json` stayed at
    /// `InProgress` despite the watcher having started and logged. The sleep was
    /// the whole remaining exposure window.
    ///
    /// ⚠️ The second assertion is the one that must not regress: an installer we
    /// genuinely waited on keeps the pause, because that is the case the
    /// constant was written for.
    #[test]
    fn a_synchronous_install_has_nothing_to_settle_for() {
        assert_eq!(
            settle_before_probe(true),
            Duration::ZERO,
            "the install finished before this process existed; the pause is pure              exposure to the teardown that is about to kill us"
        );

        assert_eq!(
            settle_before_probe(false),
            POST_INSTALL_SETTLE,
            "an installer we watched exit keeps the pause — cargo-wix can fsync              after process exit, which is why the constant exists"
        );
    }

    #[test]
    fn the_watcher_never_waits_on_an_already_completed_install() {
        assert!(
            !should_wait_for_installer(std::process::id(), true),
            "an install that already completed must not be waited on: the pid is this process or a corpse, and the wait ends only when we are killed"
        );

        assert!(
            should_wait_for_installer(4242, false),
            "a genuinely asynchronous installer must still be waited on; Windows wedge recovery depends on observing its exit"
        );

        assert!(
            !should_wait_for_installer(0, false),
            "pid 0 is not waitable — kill(0, 0) addresses the whole process group, so the poll would succeed forever"
        );
    }

    /// FR-67 P3 — the read half keeps "no record" and "a record whose verdict
    /// never came" as two words. The second is the shape this FR was opened
    /// on: `InProgress` for `agent-v0.4.16`, written 2026-08-29, still there
    /// on a host running 0.4.50. One word for both would have hidden it.
    #[test]
    fn summarize_keeps_absent_and_in_progress_apart() {
        let path = std::path::Path::new("/root/.local/share/roomler/logs/last-install.json");
        let absent = summarize(Ok(None), Some(path));
        assert_eq!(absent.status, LastInstallStatus::ABSENT);
        assert_eq!(absent.expected_version, None);
        assert_eq!(absent.started_unix, None);
        assert_eq!(
            absent.path.as_deref(),
            Some(path.display().to_string().as_str())
        );

        let frozen = summarize(
            Ok(Some(InstallOutcome {
                installer_pid: 4242,
                installer_path: "/tmp/roomlerd-0.4.16.deb".into(),
                expected_version: "agent-v0.4.16".into(),
                started_unix: 1_756_475_398,
                finished_unix: None,
                installer_exit_code: None,
                new_binary_path: None,
                new_binary_version: None,
                status: InstallStatus::InProgress,
                note: String::new(),
            })),
            Some(path),
        );
        assert_eq!(frozen.status, LastInstallStatus::IN_PROGRESS);
        assert_eq!(frozen.expected_version.as_deref(), Some("agent-v0.4.16"));
        assert_eq!(frozen.started_unix, Some(1_756_475_398));
        assert_eq!(frozen.finished_unix, None);
        assert_eq!(frozen.note, None, "an empty note is no note");
        assert_ne!(frozen.status, absent.status);
        // A day later it has not reported; a minute later it may still be
        // the watcher at work (Windows: waiting on msiexec).
        assert!(frozen.never_reported(1_756_475_398 + 86_400));
        assert!(!frozen.never_reported(1_756_475_398 + 60));
        assert!(!absent.never_reported(u64::MAX));

        // The verdict the fleet reads today (mars, 0.4.172, 2026-10-25).
        let ok = summarize(
            Ok(Some(InstallOutcome {
                installer_pid: 2_289_990,
                installer_path:
                    "/tmp/roomlerd-update.73eYDG/roomlerd-0.4.172-x86_64-unknown-linux-gnu.deb"
                        .into(),
                expected_version: "agent-v0.4.172".into(),
                started_unix: 1_792_967_966,
                finished_unix: Some(1_792_967_966),
                installer_exit_code: Some(0),
                new_binary_path: Some("/usr/bin/roomlerd".into()),
                new_binary_version: Some("roomlerd 0.4.172".into()),
                status: InstallStatus::SucceededVerified,
                note: "new binary at /usr/bin/roomlerd reports roomlerd 0.4.172".into(),
            })),
            Some(path),
        );
        assert_eq!(ok.status, LastInstallStatus::SUCCEEDED_VERIFIED);
        assert!(ok.is_verified_success());
        assert_eq!(ok.new_binary_version.as_deref(), Some("roomlerd 0.4.172"));
        assert_eq!(ok.installer_exit_code, Some(0));
        assert_eq!(
            ok.note.as_deref(),
            Some("new binary at /usr/bin/roomlerd reports roomlerd 0.4.172")
        );
    }

    /// FR-67 P3 — a record that is there and does not parse is its own word,
    /// carrying the error chain: not `Absent` (the file exists), not a
    /// verdict (there is none to read).
    #[test]
    fn summarize_unreadable_carries_the_error_chain() {
        let err = anyhow::anyhow!("expected value at line 1 column 1")
            .context("parsing /x/last-install.json");
        let li = summarize(Err(err), Some(std::path::Path::new("/x/last-install.json")));
        assert_eq!(li.status, LastInstallStatus::UNREADABLE);
        let note = li.note.as_deref().unwrap_or_default();
        assert!(
            note.contains("parsing /x/last-install.json") && note.contains("line 1 column 1"),
            "the whole chain, not the outermost context: {note}"
        );
        assert_eq!(li.expected_version, None);
    }

    /// #3302 — one watcher's life, in two crates. The reader calls a record stale
    /// at the moment the daemon may settle it.
    #[test]
    fn the_wire_and_the_watcher_agree_on_its_life() {
        assert_eq!(
            WATCHER_LIFETIME.as_secs(),
            LastInstallStatus::STALE_AFTER_SECS
        );
    }

    /// FR-67 P3 — the word on the wire IS the word in the file. The file is
    /// serde's spelling of `InstallStatus`; the wire is [`InstallStatus::word`];
    /// an operator grepping `SucceededVerified` must hit both, and the
    /// watcher's own `post-install verdict: {:?}` line prints the same.
    #[test]
    fn install_status_words_match_the_file_spelling() {
        for st in [
            InstallStatus::InProgress,
            InstallStatus::SucceededVerified,
            InstallStatus::SucceededUnverified,
            InstallStatus::InstallerFailed,
            InstallStatus::Timeout,
            InstallStatus::WatcherStopped,
        ] {
            let in_file = serde_json::to_string(&st).unwrap();
            assert_eq!(in_file, format!("\"{}\"", st.word()), "{st:?}");
            assert_eq!(
                format!("{st:?}"),
                st.word(),
                "the verdict log line spells it the same"
            );
        }
        // And the two words only the reader can say are not among them.
        assert_ne!(LastInstallStatus::ABSENT, InstallStatus::InProgress.word());
        assert!(serde_json::from_str::<InstallStatus>("\"Absent\"").is_err());
        assert!(serde_json::from_str::<InstallStatus>("\"Unreadable\"").is_err());
    }

    /// FR-67 P3 — what wakes an operator and what does not: a verified
    /// success and no record are INFO; so is a watcher that can still be at
    /// work. A record frozen past the watcher's whole budget, an unreadable
    /// file and every non-verified verdict are WARN.
    #[test]
    fn startup_level_warns_on_everything_but_the_quiet_states() {
        let base = LastInstallStatus {
            status: LastInstallStatus::SUCCEEDED_VERIFIED.into(),
            expected_version: Some("agent-v0.4.172".into()),
            new_binary_version: Some("roomlerd 0.4.172".into()),
            started_unix: Some(1_000_000),
            finished_unix: Some(1_000_000),
            installer_exit_code: Some(0),
            note: None,
            path: None,
        };
        let with = |status: &str| LastInstallStatus {
            status: status.into(),
            ..base.clone()
        };
        let now = 1_000_000 + 30;
        assert_eq!(startup_level(&base, now), tracing::Level::INFO);
        assert_eq!(
            startup_level(&with(LastInstallStatus::ABSENT), now),
            tracing::Level::INFO
        );
        assert_eq!(
            startup_level(&with(LastInstallStatus::IN_PROGRESS), now),
            tracing::Level::INFO,
            "30 s in, the watcher may still be waiting on the installer"
        );
        assert_eq!(
            startup_level(&with(LastInstallStatus::IN_PROGRESS), 1_000_000 + 86_400),
            tracing::Level::WARN,
            "a day later nothing is still writing it"
        );
        for st in [
            LastInstallStatus::SUCCEEDED_UNVERIFIED,
            LastInstallStatus::INSTALLER_FAILED,
            LastInstallStatus::TIMEOUT,
            LastInstallStatus::WATCHER_STOPPED,
            LastInstallStatus::UNREADABLE,
            "SomeFutureWord",
        ] {
            assert_eq!(startup_level(&with(st), now), tracing::Level::WARN, "{st}");
        }
    }

    /// #2107 — the copy is what outlives the staging sweep, and the slot before it
    /// is what a second wedge on the next update does not cost.
    #[test]
    fn a_wedged_log_is_kept_and_the_one_before_it_too() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("x.msi.log");
        let dir = tmp.path().join("logs");
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(keep_wedged_log(&log, &dir), None, "no log, nothing kept");

        std::fs::write(&log, "first wedge").unwrap();
        let kept = keep_wedged_log(&log, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "first wedge");

        std::fs::write(&log, "second wedge").unwrap();
        assert_eq!(keep_wedged_log(&log, &dir).unwrap(), kept);
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "second wedge");
        let prev = dir.join("msi-wedged.prev.log");
        assert_eq!(std::fs::read_to_string(&prev).unwrap(), "first wedge");

        std::fs::write(&log, "third wedge").unwrap();
        keep_wedged_log(&log, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(&prev).unwrap(), "second wedge");
    }

    #[test]
    fn version_matches_when_output_contains_triple() {
        assert!(version_matches("roomlerd 0.1.50", "agent-v0.1.50"));
        assert!(version_matches("roomlerd 0.1.50", "0.1.50"));
        assert!(version_matches("roomlerd 0.1.50", "v0.1.50"));
        assert!(version_matches("roomlerd 1.2.3 (some-build-id)", "v1.2.3"));
        // RETIRED-NAME-ANCHOR(2): a ROLLBACK re-runs the previous binary, which
        // still prints the pre-FR-21 name. Prefix tolerance is the contract.
        assert!(version_matches("roomler-agent 0.1.50", "agent-v0.1.50"));
    }

    /// #2724 — `self-update` restarts the daemon beside it only onto a
    /// binary that answers as the release it just installed.
    #[test]
    fn a_daemon_beside_self_update_is_restarted_only_onto_the_proven_release() {
        assert_eq!(
            beside_daemon_after_install(Some("roomlerd 0.4.156\n"), "0.4.156"),
            BesideDaemon::Restart
        );
        assert_eq!(
            beside_daemon_after_install(Some("roomlerd 0.4.156"), "agent-v0.4.156"),
            BesideDaemon::Restart
        );
        // The old binary still in place (the CLI ran from somewhere else).
        assert!(matches!(
            beside_daemon_after_install(Some("roomlerd 0.4.155"), "0.4.156"),
            BesideDaemon::Leave(why) if why.contains("0.4.155") && why.contains("0.4.156")
        ));
        // A file that does not run is never restarted onto.
        assert!(matches!(
            beside_daemon_after_install(None, "0.4.156"),
            BesideDaemon::Leave(why) if why.contains("did not answer")
        ));
    }

    #[test]
    fn version_does_not_match_different_triple() {
        assert!(!version_matches("roomlerd 0.1.49", "agent-v0.1.50"));
        assert!(!version_matches("roomlerd 1.0.0", "agent-v0.0.1"));
        assert!(!version_matches(
            "totally unrelated string",
            "agent-v0.1.50"
        ));
    }

    #[test]
    fn version_does_not_match_unparseable_tag() {
        // We refuse to match against malformed tags so a
        // server-side typo can't smuggle a "successful" verdict
        // through.
        assert!(!version_matches("roomlerd 0.1.50", "not-a-version"));
        assert!(!version_matches("roomlerd 0.1.50", ""));
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
            new_binary_version: Some("roomlerd 0.1.50".into()),
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
            (
                InstallStatus::SucceededUnverified,
                "\"SucceededUnverified\"",
            ),
            (InstallStatus::InstallerFailed, "\"InstallerFailed\""),
            (InstallStatus::WatcherStopped, "\"WatcherStopped\""),
            (InstallStatus::Timeout, "\"Timeout\""),
        ];
        for (status, expected) in cases {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn only_a_failed_install_starts_a_per_user_daemon() {
        for (status, want) in [
            (InstallStatus::InProgress, false),
            (InstallStatus::SucceededVerified, false),
            (InstallStatus::SucceededUnverified, false),
            (InstallStatus::InstallerFailed, true),
            (InstallStatus::Timeout, true),
        ] {
            assert_eq!(user_daemon_needs_a_start(&status), want, "{status:?}");
        }
    }

    // ── #2711 — the service settle ───────────────────────────────────────

    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    /// How long a start the fake accepts spends in START_PENDING.
    const FAKE_START_TAKES: Duration = Duration::from_millis(300);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A scripted SCM on a fake clock. `timeline` is the service's state
    /// from each instant on; a start the fake accepts owns the next
    /// [`FAKE_START_TAKES`] (START_PENDING, then RUNNING — or STOPPED again
    /// with `dies_on_start`), and later script entries still happen, which is
    /// how a stop arriving after a start is written.
    struct FakeScm {
        clock: Rc<Cell<Duration>>,
        timeline: Vec<(Duration, ServiceLook)>,
        /// What each start answers, in order; once empty, a start is accepted.
        answers: VecDeque<Result<(), u32>>,
        dies_on_start: bool,
        /// When each start came, and the state it found.
        starts: Vec<(Duration, ServiceLook)>,
    }

    impl FakeScm {
        fn new(clock: &Rc<Cell<Duration>>, timeline: &[(u64, ServiceLook)]) -> Self {
            Self {
                clock: clock.clone(),
                timeline: timeline.iter().map(|(t, l)| (ms(*t), *l)).collect(),
                answers: VecDeque::new(),
                dies_on_start: false,
                starts: Vec::new(),
            }
        }

        fn at(&self, t: Duration) -> ServiceLook {
            self.timeline
                .iter()
                .rev()
                .find(|(from, _)| *from <= t)
                .map_or(ServiceLook::Stopped, |(_, l)| *l)
        }
    }

    impl ServiceHandle for FakeScm {
        fn look(&mut self) -> ServiceLook {
            self.at(self.clock.get())
        }

        fn start(&mut self) -> Result<(), u32> {
            let now = self.clock.get();
            let state = self.at(now);
            self.starts.push((now, state));
            match (self.answers.pop_front().unwrap_or(Ok(())), state) {
                (Ok(()), ServiceLook::Stopped) => {
                    let settled = now + FAKE_START_TAKES;
                    let after = if self.dies_on_start {
                        ServiceLook::Stopped
                    } else {
                        ServiceLook::Running
                    };
                    self.timeline
                        .retain(|(from, _)| *from <= now || *from > settled);
                    self.timeline.push((now, ServiceLook::StartPending));
                    self.timeline.push((settled, after));
                    // Stable: at one instant, the entry pushed last wins.
                    self.timeline.sort_by_key(|(from, _)| *from);
                    Ok(())
                }
                // The SCM still holds an instance: a running one, or a
                // STOP_PENDING one — the same 1056 either way.
                (Ok(()), _) => Err(1056),
                (Err(code), _) => Err(code),
            }
        }
    }

    /// Run the settle against `scm`; returns it and when the watch ended.
    fn settle(scm: &mut FakeScm) -> (ServiceSettle, Duration) {
        let clock = scm.clock.clone();
        let tick = clock.clone();
        let s = settle_service("Roomler", scm, || clock.get(), |d| tick.set(tick.get() + d));
        (s, scm.clock.get())
    }

    /// The field read of 2026-10-22: the watcher's turn came while Restart
    /// Manager's late stop was still in flight (STOP_PENDING), and the stop
    /// completed 1.1 s later. The old one-shot `sc start` got 1056 here and
    /// recorded "already running"; the host stayed down 30 minutes.
    ///
    /// ⚠️ The load-bearing assertion is the second: nothing is started while
    /// the stop is pending (that answer would be 1056 and prove nothing),
    /// and the service IS started the moment it reads STOPPED.
    #[test]
    fn the_2711_race_a_pending_stop_is_waited_out_then_started() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let mut scm = FakeScm::new(
            &clock,
            &[(0, ServiceLook::StopPending), (1_100, ServiceLook::Stopped)],
        );
        let (s, ended) = settle(&mut scm);

        assert_eq!(s.verdict, ServiceVerdict::Running);
        assert_eq!(
            scm.starts,
            vec![(ms(1_500), ServiceLook::Stopped)],
            "started exactly once, at the first STOPPED read — never into the pending stop"
        );
        assert_eq!(
            s.seen,
            vec![
                ServiceLook::StopPending,
                ServiceLook::Stopped,
                ServiceLook::Running
            ]
        );
        assert!(
            ended >= ms(1_800) + SERVICE_STABLE_FOR,
            "RUNNING is believed only after it held for the window (ended at {ended:?})"
        );
        assert_eq!(
            s.describe(),
            "running (1 start; seen StopPending→Stopped→Running)"
        );
    }

    /// The other half of 2026-10-22: Restart Manager's stop reached the
    /// SCM 3.9 s AFTER the new host started. A watcher that believed its
    /// first RUNNING read would have walked away from a host about to stop.
    #[test]
    fn running_is_believed_only_once_it_has_held() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let mut scm = FakeScm::new(
            &clock,
            &[
                (0, ServiceLook::Running),
                (3_900, ServiceLook::StopPending),
                (12_300, ServiceLook::Stopped),
            ],
        );
        let (s, _) = settle(&mut scm);

        assert_eq!(s.verdict, ServiceVerdict::Running);
        assert_eq!(s.starts, 1, "the late stop is caught and undone once");
        assert_eq!(
            s.seen,
            vec![
                ServiceLook::Running,
                ServiceLook::StopPending,
                ServiceLook::Stopped,
                ServiceLook::Running
            ]
        );
    }

    /// The common install: the service runs, the watcher touches nothing.
    #[test]
    fn a_healthy_install_starts_nothing() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let mut scm = FakeScm::new(&clock, &[(0, ServiceLook::Running)]);
        let (s, ended) = settle(&mut scm);

        assert_eq!(s.verdict, ServiceVerdict::Running);
        assert!(scm.starts.is_empty());
        assert!(
            (SERVICE_STABLE_FOR..SERVICE_STABLE_FOR + ms(1_000)).contains(&ended),
            "{ended:?}"
        );
        assert_eq!(s.describe(), "running (0 starts; seen Running)");
    }

    /// 1056 is not "running": it means the SCM still holds an instance,
    /// which is also what it says for a STOP_PENDING service. So a 1056
    /// answer never ends the watch — the next read decides.
    #[test]
    fn a_1056_answer_never_ends_the_watch() {
        assert_eq!(start_refusal(1056), None);

        let clock = Rc::new(Cell::new(Duration::ZERO));
        let mut scm = FakeScm::new(&clock, &[(0, ServiceLook::Stopped)]);
        scm.answers.push_back(Err(1056));
        let (s, _) = settle(&mut scm);

        assert_eq!(s.verdict, ServiceVerdict::Running);
        assert_eq!(s.starts, 2, "the refused start is followed by a real one");
        assert_eq!(s.refused, Some(1056));
    }

    /// A refusal the watcher cannot act past ends the watch at once, and
    /// says which it was — never a silent "ok".
    #[test]
    fn refusals_end_the_watch_and_name_themselves() {
        for (code, verdict) in [
            (5, ServiceVerdict::Denied),
            (1058, ServiceVerdict::Disabled),
            (1060, ServiceVerdict::NotInstalled),
            (1072, ServiceVerdict::NotInstalled),
        ] {
            let clock = Rc::new(Cell::new(Duration::ZERO));
            let mut scm = FakeScm::new(&clock, &[(0, ServiceLook::Stopped)]);
            scm.answers.push_back(Err(code));
            let (s, ended) = settle(&mut scm);
            assert_eq!(s.verdict, verdict, "code {code}");
            assert_eq!((s.starts, s.refused), (1, Some(code)), "code {code}");
            assert_eq!(
                ended,
                Duration::ZERO,
                "code {code}: no waiting after a final answer"
            );
        }
    }

    /// A host that dies at every start is not the watcher's to loop on:
    /// [`MAX_SERVICE_STARTS`], then the SCM's recovery actions own it.
    #[test]
    fn a_service_that_keeps_stopping_gets_bounded_starts() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let mut scm = FakeScm::new(&clock, &[(0, ServiceLook::Stopped)]);
        scm.dies_on_start = true;
        let (s, _) = settle(&mut scm);

        assert_eq!(s.verdict, ServiceVerdict::Stopped);
        assert_eq!(s.starts, MAX_SERVICE_STARTS);
        assert!(
            s.describe()
                .starts_with("STOPPED — could not start it (3 starts;"),
            "{}",
            s.describe()
        );
    }

    /// Every wait is bounded by the budget, whatever the service does.
    #[test]
    fn the_budget_bounds_a_transition_that_never_ends() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let mut scm = FakeScm::new(&clock, &[(0, ServiceLook::StopPending)]);
        let (s, ended) = settle(&mut scm);

        assert_eq!(s.verdict, ServiceVerdict::Unsettled);
        assert!(
            scm.starts.is_empty(),
            "a pending stop is never started into"
        );
        assert!(
            (SERVICE_SETTLE_BUDGET..=SERVICE_SETTLE_BUDGET + SERVICE_POLL).contains(&ended),
            "{ended:?}"
        );
    }

    /// No service, or no way to read it: nothing to wait for.
    #[test]
    fn missing_or_unreadable_ends_at_once() {
        for (look, verdict) in [
            (ServiceLook::Missing, ServiceVerdict::NotInstalled),
            (ServiceLook::Unreadable, ServiceVerdict::Unreadable),
        ] {
            let clock = Rc::new(Cell::new(Duration::ZERO));
            let mut scm = FakeScm::new(&clock, &[(0, look)]);
            let (s, ended) = settle(&mut scm);
            assert_eq!((s.verdict, s.starts, ended), (verdict, 0, Duration::ZERO));
        }
    }

    /// The decision for every read, inside and at the edge of the windows.
    #[test]
    fn next_service_step_for_every_read() {
        use ServiceLook as L;
        use ServiceStep as S;
        use ServiceVerdict as V;
        let early = ms(1_000);
        let late = SERVICE_SETTLE_BUDGET;
        let cases = [
            (L::Running, Duration::ZERO, 0, early, S::Wait),
            (L::Running, SERVICE_STABLE_FOR - ms(1), 0, early, S::Wait),
            (
                L::Running,
                SERVICE_STABLE_FOR,
                0,
                early,
                S::Done(V::Running),
            ),
            (L::Running, ms(2_000), 0, late, S::Done(V::RunningBriefly)),
            (L::Stopped, Duration::ZERO, 0, early, S::Start),
            (
                L::Stopped,
                Duration::ZERO,
                MAX_SERVICE_STARTS - 1,
                early,
                S::Start,
            ),
            (
                L::Stopped,
                Duration::ZERO,
                MAX_SERVICE_STARTS,
                early,
                S::Done(V::Stopped),
            ),
            (L::Stopped, Duration::ZERO, 0, late, S::Done(V::Stopped)),
            (L::StopPending, Duration::ZERO, 0, early, S::Wait),
            (L::StartPending, Duration::ZERO, 0, early, S::Wait),
            (L::Paused, Duration::ZERO, 0, early, S::Wait),
            (
                L::StopPending,
                Duration::ZERO,
                0,
                late,
                S::Done(V::Unsettled),
            ),
            (L::Paused, Duration::ZERO, 0, late, S::Done(V::Unsettled)),
            (
                L::Missing,
                Duration::ZERO,
                0,
                early,
                S::Done(V::NotInstalled),
            ),
            (
                L::Unreadable,
                Duration::ZERO,
                0,
                early,
                S::Done(V::Unreadable),
            ),
        ];
        for (look, running_for, starts, elapsed, want) in cases {
            assert_eq!(
                next_service_step(look, running_for, starts, elapsed),
                want,
                "{look:?} running_for={running_for:?} starts={starts} elapsed={elapsed:?}"
            );
        }
    }

    /// A flapping service cannot grow the note without bound.
    #[test]
    fn the_trail_is_capped() {
        let seen: Vec<ServiceLook> = (0..30)
            .map(|i| {
                if i % 2 == 0 {
                    ServiceLook::Running
                } else {
                    ServiceLook::StopPending
                }
            })
            .collect();
        let s = ServiceSettle {
            verdict: ServiceVerdict::Unsettled,
            starts: 0,
            seen,
            refused: None,
        };
        let trail = s.trail();
        assert_eq!(trail.matches('→').count(), ServiceSettle::TRAIL_MAX);
        assert!(trail.ends_with("→…+18"), "{trail}");
    }
}

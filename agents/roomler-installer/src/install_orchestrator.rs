//! `cmd_install` orchestrator + cancel + force-kill plumbing.
//!
//! W6b in the rc.28 plan. Owns the 7-step pipeline:
//!
//!   1. Preflight: probe registry, surface cross-flavour warnings
//!   2. Resolve installer metadata via the roomler.ai proxy (rc.27)
//!   3. Stream MSI bytes to `%TEMP%` with progress emits
//!   4. Verify SHA256
//!   5. Spawn msiexec via `roomler_agent::updater::spawn_installer_inner`
//!      (perMachine gets UAC via ShellExecuteExW + verb=runas)
//!   6. Attach `MsiRunner` to the spawned PID + wait for exit (caller's
//!      Tauri-future cancel propagates through 250 ms-slice polling)
//!   7. Enroll the agent against the configured server, persist
//!      `config.toml`. Emits `EnrollOk` then `Done`.
//!
//! ## Cancel + force-kill
//!
//! Two cancellation modes, surfaced by separate commands per the
//! plan's H4 split:
//!   - `cmd_cancel_in_progress` flips `CANCEL_REQUESTED` — pre-spawn
//!     cancel only; orchestrator checks the flag at each await point
//!     and bails with `InstallError::Cancelled`. Polite, leaves no
//!     residue.
//!   - `cmd_force_kill_msi` calls `TerminateProcess(pid)` on the
//!     stored `ACTIVE_MSI_PID`. May leave a partial MSI install; the
//!     SPA warns the operator before exposing this button.
//!
//! ## SystemContext mode caveat
//!
//! v1 cmd_install supports `peruser` and `permachine` flavours.
//! `permachine-system-context` (which writes
//! `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP=1` into the SCM service env
//! block + restarts the service) needs admin-elevated registry
//! writes that the wizard's perMachine UAC path can't reach
//! synchronously. v1 returns a clear "use the CLI / re-run the rc.27
//! `set-service-env-var` from elevated PowerShell" error. Full
//! support deferred to rc.29 once we have a clean self-elevation
//! mechanism that doesn't surface a second UAC prompt mid-flow.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use roomler_agent::install_detect::{ExistingInstall, detect_existing_install};
use roomler_agent::updater::{WindowsInstallFlavour, spawn_installer_for_flavour};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;

use crate::asset_resolver;
use crate::msi_runner::{MsiExitDecoded, MsiRunner};
use crate::progress::{ProgressEvent, replay_log};

/// `true` while a pre-spawn cancel is pending. Orchestrator checks
/// at each await point.
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// PID of the active msiexec process. `0` = none (no msiexec
/// currently running under wizard supervision). `cmd_force_kill_msi`
/// reads this to attach + TerminateProcess.
static ACTIVE_MSI_PID: AtomicU32 = AtomicU32::new(0);

/// Output of a successful `cmd_install`. Surfaced on the Done step.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DoneReport {
    pub agent_id: String,
    pub tenant_id: String,
    pub flavour: String,
    pub tag: String,
}

/// Drive a full install end-to-end. Streams ProgressEvent over the
/// channel; mirrors every emit into the replay log.
pub async fn run_install(
    flavour_str: String,
    server: String,
    token: String,
    device_name: String,
    on_event: Channel<ProgressEvent>,
) -> Result<DoneReport, String> {
    // Reset state for a fresh run. CANCEL_REQUESTED stays cleared
    // unless the operator hits Cancel mid-flow.
    crate::INSTALL_IN_PROGRESS.store(true, Ordering::SeqCst);
    CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    ACTIVE_MSI_PID.store(0, Ordering::SeqCst);
    replay_log().reset();

    // Outer Result so we can always clear INSTALL_IN_PROGRESS on
    // any exit path.
    let outcome = run_install_inner(flavour_str, server, token, device_name, &on_event).await;

    crate::INSTALL_IN_PROGRESS.store(false, Ordering::SeqCst);
    ACTIVE_MSI_PID.store(0, Ordering::SeqCst);

    match outcome {
        Ok(r) => Ok(r),
        Err(e) => {
            // Echo terminal failures into both the channel + the
            // replay log so a late-attached SPA listener sees them.
            emit(
                &on_event,
                ProgressEvent::Error {
                    step: "install".to_string(),
                    message: e.clone(),
                },
            );
            Err(e)
        }
    }
}

async fn run_install_inner(
    flavour_str: String,
    server: String,
    token: String,
    device_name: String,
    on_event: &Channel<ProgressEvent>,
) -> Result<DoneReport, String> {
    emit(on_event, ProgressEvent::Started);

    // --- Step 1: preflight -----------------------------------------------
    check_cancel()?;
    emit(on_event, ProgressEvent::PreflightStarted);
    let detected = detect_existing_install();
    let existing_label = label_existing(&detected);
    emit(
        on_event,
        ProgressEvent::PreflightOk {
            existing: existing_label.clone(),
        },
    );

    // Surface cross-flavour switch as a warning. The SPA gates the
    // Continue button on an operator-acknowledgement checkbox per
    // BLOCKER-7 of the plan critique; by the time cmd_install runs,
    // that gate is passed.
    if let Some(warning) = cross_flavour_warning(&detected, &flavour_str) {
        emit(
            on_event,
            ProgressEvent::PreflightWarning { message: warning },
        );
    }

    // --- Parse flavour into the enum the rest of the pipeline uses -----
    let (wfx, is_system_context) = parse_flavour(&flavour_str)?;

    // SystemContext mode: run plain perMachine install (parse_flavour
    // already returns WindowsInstallFlavour::PerMachine for the
    // SystemContext variant). The SPA's Done page shows a manual
    // PowerShell snippet — `roomler-agent set-service-env-var` +
    // `restart-service` — that the operator runs from an elevated
    // shell to flip the SystemContext path on. Full MSI-side
    // automation (a WiX custom action gated on ENABLE_SYSTEM_CONTEXT=1)
    // is the next slice but not blocking the v1 ship.
    if is_system_context {
        emit(
            on_event,
            ProgressEvent::PreflightWarning {
                message: "SystemContext mode: installing plain perMachine MSI now. \
                          After Done, run the elevated `set-service-env-var` + \
                          `restart-service` snippet shown on the final step to \
                          flip the SystemContext path on."
                    .to_string(),
            },
        );
    }

    // --- Step 2: resolve installer ---------------------------------------
    check_cancel()?;
    let flavour_for_proxy = match wfx {
        WindowsInstallFlavour::PerUser => "peruser",
        WindowsInstallFlavour::PerMachine => "permachine",
    };
    emit(
        on_event,
        ProgressEvent::AssetResolving {
            flavour: flavour_for_proxy.to_string(),
        },
    );
    let health = asset_resolver::resolve(flavour_for_proxy, "latest")
        .await
        .map_err(|e| format!("resolve installer: {e}"))?;
    emit(
        on_event,
        ProgressEvent::AssetResolved {
            tag: health.tag.clone(),
            size_bytes: health.size,
            digest: health.digest.clone(),
        },
    );

    // --- Step 3: download ------------------------------------------------
    check_cancel()?;
    emit(
        on_event,
        ProgressEvent::DownloadStarted {
            total_bytes: health.size,
        },
    );
    let progress_emitter = on_event.clone();
    let staged = asset_resolver::download(&health, move |received| {
        // Skip the replay-log mirror here — DownloadProgress fires
        // hundreds of times per MB and would balloon the log. The
        // SPA's primary listener catches the live stream.
        let _ = progress_emitter.send(ProgressEvent::DownloadProgress {
            received_bytes: received,
        });
    })
    .await
    .map_err(|e| format!("download installer: {e}"))?;

    // --- Step 4: verify SHA256 ------------------------------------------
    check_cancel()?;
    let sha256_match = match health.digest.as_deref() {
        Some(digest) => asset_resolver::verify_sha256(&staged, digest)
            .map_err(|e| format!("sha256 verify: {e}"))?,
        None => {
            // Pre-Oct-2024 releases lack the digest field. Accept
            // the download as-is but flag it loudly.
            tracing::warn!(
                tag = %health.tag,
                "installer asset lacks sha256 digest — skipping verification"
            );
            true
        }
    };
    emit(on_event, ProgressEvent::DownloadVerified { sha256_match });
    if !sha256_match {
        let _ = std::fs::remove_file(&staged);
        return Err(format!(
            "SHA256 mismatch for {} — staged file deleted; please retry",
            staged.display()
        ));
    }

    // --- Step 5: spawn msiexec ------------------------------------------
    //
    // Pass the OPERATOR-SELECTED flavour (parsed from the SPA radio
    // cards at the top of this function) explicitly. DO NOT delegate
    // to `spawn_installer_inner` — that classifies the wizard EXE's
    // own location via `current_install_flavour`, which always
    // resolves to PerUser inside the wizard (the EXE runs from
    // wherever the operator double-clicked, never `\Program Files\`).
    // A perMachine MSI launched on the perUser branch (`/qn`, no
    // ShellExecuteExW runas) gets rejected by Windows Installer with
    // exit code 1625 ERROR_INSTALL_PACKAGE_REJECTED. Field repro
    // 2026-05-15 on GORAN-XMG-NEO16; BLOCKER B6 from the rc.27/rc.28
    // master plan.
    check_cancel()?;
    let pid =
        spawn_installer_for_flavour(&staged, wfx).map_err(|e| format!("spawn msiexec: {e}"))?;
    ACTIVE_MSI_PID.store(pid, Ordering::SeqCst);
    emit(on_event, ProgressEvent::MsiSpawned { pid });

    // --- Step 6: attach + wait + decode ----------------------------------
    let runner = MsiRunner::attach(pid).map_err(|e| format!("attach msiexec: {e}"))?;
    // 15-min budget covers slow corporate disks + Defender scans.
    let outcome = runner
        .wait_for_exit(Duration::from_secs(15 * 60))
        .await
        .map_err(|e| format!("wait for msiexec: {e}"))?;
    let outcome_str = format!("{outcome:?}");
    emit(
        on_event,
        ProgressEvent::MsiCompleted {
            code: msi_exit_code(&outcome),
            decoded: outcome_str.clone(),
        },
    );

    match outcome {
        MsiExitDecoded::Success | MsiExitDecoded::RebootRequired => {
            // Carry on to enrollment. RebootRequired is OK because the
            // agent's first start can pick up the new install state
            // after the OS reboots; enrollment is just an HTTP call
            // that doesn't depend on the agent binary running.
        }
        MsiExitDecoded::UserCancel => {
            return Err(
                "Installation cancelled (you clicked No on the UAC prompt). \
                 Try again or pick perUser mode if you can't get admin rights."
                    .to_string(),
            );
        }
        MsiExitDecoded::AnotherInstall => {
            return Err(
                "Another Windows Installer operation is already running on this \
                 host. Wait for it to finish (check Get-Process msiexec), \
                 then click Retry."
                    .to_string(),
            );
        }
        MsiExitDecoded::FatalError => {
            return Err(
                "MSI installer failed (exit 1603). Check the MSI log at %TEMP%\\MSI*.LOG for details."
                    .to_string(),
            );
        }
        MsiExitDecoded::Other(code) => {
            return Err(format!("MSI installer exited with unexpected code {code}"));
        }
    }

    // --- Step 7: enroll --------------------------------------------------
    check_cancel()?;
    emit(on_event, ProgressEvent::EnrollStarted);
    let config_path = roomler_agent::config::default_config_path()
        .map_err(|e| format!("resolve config path: {e}"))?;
    let machine_id = roomler_agent::machine::derive_machine_id(&config_path);
    let inputs = roomler_agent::enrollment::EnrollInputs {
        server_url: &server,
        enrollment_token: &token,
        machine_id: &machine_id,
        machine_name: &device_name,
    };
    let cfg = roomler_agent::enrollment::enroll(inputs)
        .await
        .map_err(|e| format!("enrollment: {e}"))?;
    let agent_id = cfg.agent_id.clone();
    let tenant_id = cfg.tenant_id.clone();
    roomler_agent::config::save(&config_path, &cfg)
        .map_err(|e| format!("write config.toml: {e}"))?;
    emit(
        on_event,
        ProgressEvent::EnrollOk {
            agent_id: agent_id.clone(),
            tenant_id: tenant_id.clone(),
        },
    );

    // --- Step 8: done ----------------------------------------------------
    emit(on_event, ProgressEvent::Done);

    Ok(DoneReport {
        agent_id,
        tenant_id,
        flavour: flavour_str,
        tag: health.tag,
    })
}

/// Pre-spawn cancel. Returns immediately; the orchestrator's next
/// `check_cancel()` bails. Once msiexec is spawned, use
/// `force_kill_msi` instead (this AtomicBool can't interrupt
/// msiexec).
pub fn request_cancel() {
    CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

/// Force-kill the currently-running msiexec. Returns `Ok(())` if a
/// PID was stored + TerminateProcess succeeded; `Err` otherwise.
/// Leaves Windows Installer in a potentially-rolled-back state; the
/// SPA must surface "may leave partial install" before invoking
/// this.
pub fn force_kill_msi() -> Result<(), String> {
    let pid = ACTIVE_MSI_PID.load(Ordering::SeqCst);
    if pid == 0 {
        return Err("no msiexec currently running".to_string());
    }
    let runner = MsiRunner::attach(pid).map_err(|e| format!("attach msiexec({pid}): {e}"))?;
    runner
        .terminate()
        .map_err(|e| format!("terminate msiexec({pid}): {e}"))
}

// ─── helpers ────────────────────────────────────────────────────────────────

fn emit(channel: &Channel<ProgressEvent>, event: ProgressEvent) {
    // Replay log first so a late-attaching listener can catch up
    // via cmd_install_progress_replay; the channel's send is best-
    // effort (Tauri 2 returns Err only when the receiver has been
    // closed, which means the SPA already moved on).
    replay_log().push(event.clone());
    let _ = channel.send(event);
}

fn check_cancel() -> Result<(), String> {
    if CANCEL_REQUESTED.load(Ordering::SeqCst) {
        Err("install cancelled by operator".to_string())
    } else {
        Ok(())
    }
}

fn label_existing(detected: &ExistingInstall) -> String {
    match detected {
        ExistingInstall::Clean => "clean".to_string(),
        ExistingInstall::PerUser(info) => format!(
            "perUser {}",
            info.version.as_deref().unwrap_or("(unknown version)")
        ),
        ExistingInstall::PerMachine(info) => format!(
            "perMachine {}",
            info.version.as_deref().unwrap_or("(unknown version)")
        ),
        ExistingInstall::Ambiguous {
            peruser,
            permachine,
        } => format!(
            "ambiguous (perUser {} + perMachine {})",
            peruser.version.as_deref().unwrap_or("?"),
            permachine.version.as_deref().unwrap_or("?"),
        ),
    }
}

/// Detect a cross-flavour switch; return a warning string the SPA
/// renders in a yellow banner.
pub fn cross_flavour_warning(detected: &ExistingInstall, requested: &str) -> Option<String> {
    let requested_is_permachine = requested.starts_with("permachine");
    let requested_is_peruser = requested == "peruser";
    match detected {
        ExistingInstall::PerUser(_) if requested_is_permachine => Some(
            "Switching from perUser → perMachine. Your existing enrollment will be lost; you'll need a fresh enrollment token from your administrator."
                .to_string(),
        ),
        ExistingInstall::PerMachine(_) if requested_is_peruser => Some(
            "Switching from perMachine → perUser. Your existing enrollment will be lost; you'll need a fresh enrollment token from your administrator."
                .to_string(),
        ),
        ExistingInstall::Ambiguous { .. } => Some(
            "Both perUser and perMachine installs detected. The MSI's cleanup custom action will remove the one not selected."
                .to_string(),
        ),
        _ => None,
    }
}

/// Parse the SPA's flavour string into the agent's enum + a flag
/// for the SystemContext branch.
pub fn parse_flavour(s: &str) -> Result<(WindowsInstallFlavour, bool), String> {
    match s {
        "peruser" => Ok((WindowsInstallFlavour::PerUser, false)),
        "permachine" => Ok((WindowsInstallFlavour::PerMachine, false)),
        "permachine-system-context" => Ok((WindowsInstallFlavour::PerMachine, true)),
        other => Err(format!(
            "unknown flavour {other:?}; expected peruser / permachine / permachine-system-context"
        )),
    }
}

/// Reverse-map a `MsiExitDecoded` back to a numeric code so
/// `ProgressEvent::MsiCompleted` carries both surfaces.
fn msi_exit_code(decoded: &MsiExitDecoded) -> i32 {
    match decoded {
        MsiExitDecoded::Success => 0,
        MsiExitDecoded::UserCancel => 1602,
        MsiExitDecoded::FatalError => 1603,
        MsiExitDecoded::AnotherInstall => 1618,
        MsiExitDecoded::RebootRequired => 3010,
        MsiExitDecoded::Other(code) => *code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_agent::install_detect::InstallInfo;

    fn info(version: &str) -> InstallInfo {
        InstallInfo {
            version: Some(version.to_string()),
            install_location: None,
        }
    }

    #[test]
    fn parse_flavour_accepts_known_values() {
        assert_eq!(
            parse_flavour("peruser").unwrap(),
            (WindowsInstallFlavour::PerUser, false)
        );
        assert_eq!(
            parse_flavour("permachine").unwrap(),
            (WindowsInstallFlavour::PerMachine, false)
        );
        assert_eq!(
            parse_flavour("permachine-system-context").unwrap(),
            (WindowsInstallFlavour::PerMachine, true)
        );
    }

    #[test]
    fn parse_flavour_rejects_unknown() {
        assert!(parse_flavour("system").is_err());
        assert!(parse_flavour("").is_err());
    }

    #[test]
    fn cross_flavour_warning_peruser_to_permachine() {
        let w = cross_flavour_warning(&ExistingInstall::PerUser(info("0.3.0-rc.26")), "permachine");
        assert!(w.is_some());
        let msg = w.unwrap();
        assert!(msg.contains("perUser"));
        assert!(msg.contains("perMachine"));
        assert!(msg.contains("fresh enrollment token"));
    }

    #[test]
    fn cross_flavour_warning_permachine_to_peruser() {
        let w = cross_flavour_warning(&ExistingInstall::PerMachine(info("0.3.0-rc.26")), "peruser");
        assert!(w.is_some());
    }

    #[test]
    fn cross_flavour_warning_silent_for_same_flavour() {
        // peruser → peruser is a same-flavour upgrade; no warning.
        let w = cross_flavour_warning(&ExistingInstall::PerUser(info("0.3.0-rc.26")), "peruser");
        assert!(w.is_none());
        let w = cross_flavour_warning(
            &ExistingInstall::PerMachine(info("0.3.0-rc.26")),
            "permachine",
        );
        assert!(w.is_none());
    }

    #[test]
    fn cross_flavour_warning_silent_for_clean() {
        let w = cross_flavour_warning(&ExistingInstall::Clean, "peruser");
        assert!(w.is_none());
    }

    #[test]
    fn cross_flavour_warning_for_ambiguous() {
        let w = cross_flavour_warning(
            &ExistingInstall::Ambiguous {
                peruser: info("0.3.0-rc.18"),
                permachine: info("0.3.0-rc.26"),
            },
            "permachine",
        );
        assert!(w.is_some());
        assert!(w.unwrap().contains("Both"));
    }

    #[test]
    fn label_existing_renders_each_variant() {
        assert_eq!(label_existing(&ExistingInstall::Clean), "clean");
        assert_eq!(
            label_existing(&ExistingInstall::PerUser(info("0.3.0-rc.26"))),
            "perUser 0.3.0-rc.26"
        );
        assert_eq!(
            label_existing(&ExistingInstall::PerMachine(info("0.3.0-rc.26"))),
            "perMachine 0.3.0-rc.26"
        );
    }

    #[test]
    fn label_existing_handles_missing_version() {
        assert_eq!(
            label_existing(&ExistingInstall::PerUser(InstallInfo::default())),
            "perUser (unknown version)"
        );
    }

    #[test]
    fn msi_exit_code_roundtrip() {
        assert_eq!(msi_exit_code(&MsiExitDecoded::Success), 0);
        assert_eq!(msi_exit_code(&MsiExitDecoded::UserCancel), 1602);
        assert_eq!(msi_exit_code(&MsiExitDecoded::FatalError), 1603);
        assert_eq!(msi_exit_code(&MsiExitDecoded::AnotherInstall), 1618);
        assert_eq!(msi_exit_code(&MsiExitDecoded::RebootRequired), 3010);
        assert_eq!(msi_exit_code(&MsiExitDecoded::Other(42)), 42);
    }

    #[test]
    fn request_cancel_then_check_cancel_returns_err() {
        // SAFETY: this test mutates a process-wide AtomicBool, so it
        // races with any other test that does the same. We restore
        // the flag at end so subsequent tests see a clean slate.
        let saved = CANCEL_REQUESTED.swap(false, Ordering::SeqCst);
        request_cancel();
        let result = check_cancel();
        // Restore the saved value before asserting (so the assertion
        // never short-circuits the cleanup).
        CANCEL_REQUESTED.store(saved, Ordering::SeqCst);
        assert!(result.is_err());
    }

    #[test]
    fn force_kill_msi_without_active_pid_returns_err() {
        let saved = ACTIVE_MSI_PID.swap(0, Ordering::SeqCst);
        let result = force_kill_msi();
        ACTIVE_MSI_PID.store(saved, Ordering::SeqCst);
        assert!(result.is_err());
    }

    // ----- B6 regression (1625 ERROR_INSTALL_PACKAGE_REJECTED) ---------

    #[test]
    fn parse_flavour_permachine_resolves_to_permachine_enum() {
        // Lock the contract that drives B6's fix: the SPA-provided
        // `permachine` string deterministically becomes
        // `WindowsInstallFlavour::PerMachine`, which is what the
        // orchestrator passes to `spawn_installer_for_flavour`. If
        // this drifts (e.g. someone introduces a new wrapper enum or
        // adds a fallback to `current_install_flavour`), the wizard's
        // perMachine spawn breaks again with 1625.
        let (wfx, sysctx) = parse_flavour("permachine").expect("parse");
        assert_eq!(wfx, WindowsInstallFlavour::PerMachine);
        assert!(!sysctx);
        let (wfx, sysctx) = parse_flavour("permachine-system-context").expect("parse");
        assert_eq!(wfx, WindowsInstallFlavour::PerMachine);
        assert!(sysctx);
    }
}

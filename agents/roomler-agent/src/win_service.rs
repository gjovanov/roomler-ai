//! Windows Service deployment mode (Effort 2).
//!
//! Optional alternative to the Scheduled Task model (`service install`,
//! the existing default). The Scheduled Task launches the agent in the
//! interactive user's session at logon and works for self-controlled
//! hosts. The Service mode targets fleet / unattended deployments where:
//!
//!   - the host should be reachable **before** anyone logs in (lock
//!     screen / pre-logon remote login),
//!   - the install should not depend on a particular user being created
//!     before the agent is registered,
//!   - and the operator (typically IT) wants a single SCM entry to
//!     manage state via Get-Service / sc.exe / Server Manager.
//!
//! Module surface for M1:
//!   - [`install`] / [`uninstall`] / [`status`] — service-manager API
//!     wrappers, exposed via the `service install --as-service`,
//!     `service uninstall --as-service`, `service status --as-service`
//!     CLI subcommands.
//!   - [`run_in_dispatcher`] — entry point for the SCM-launched
//!     `service-run` subcommand. Hands control to the SCM via
//!     [`service_dispatcher::start`].
//!
//! M2 (session-aware worker spawn) and M3 (pre-logon SYSTEM-context
//! capture) layer on top of this skeleton without changing the public
//! surface: the service main loop today is a stub that waits for the
//! SCM Stop control; M2 will replace the stub with worker supervision.

#![cfg(target_os = "windows")]

use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

/// SCM short name. The Scheduled Task is `RoomlerAgent`; the Service
/// is intentionally a different name so an operator can have the
/// installer roll out *one* of the two models cleanly without
/// fighting an existing autostart hook (the MSI's RegisterAutostart
/// custom action remains scoped to the Scheduled Task).
pub const SERVICE_NAME: &str = "RoomlerAgentService";

/// Display name shown in services.msc, Get-Service output, and Server Manager.
pub const SERVICE_DISPLAY_NAME: &str = "Roomler AI Remote-Control Agent";

/// One-line description shown in services.msc properties dialog.
pub const SERVICE_DESCRIPTION: &str = "Native remote-control agent for the Roomler AI platform. Maintains an outbound \
     WebSocket connection to the configured Roomler server and serves WebRTC peers \
     directly to authorised browser controllers. Managed by the Roomler MSI.";

/// Argument the SCM passes when starting the service. The agent's
/// `service-run` subcommand handler dispatches to [`run_in_dispatcher`]
/// which then hands the binary over to `service_dispatcher::start`.
pub const RUN_SUBCOMMAND: &str = "service-run";

/// Default service-mode log directory under `%PROGRAMDATA%`. The user-
/// scoped worker logs go elsewhere (under `%LOCALAPPDATA%`); the SCM-
/// launched service runs as SYSTEM and can't write there.
pub fn default_log_dir() -> Option<PathBuf> {
    std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .map(|p| p.join("roomler").join("roomler-agent").join("service-logs"))
}

/// Register `RoomlerAgentService` with the SCM, AutoStart, ServiceAccount
/// LocalSystem (the default for `account_name: None`). Idempotent in spirit
/// — re-running install when the service already exists returns
/// `AlreadyExists` rather than overwriting; callers should `uninstall`
/// first if they want to refresh the binary path.
///
/// Requires elevation (admin token). The MSI's custom action is the
/// natural caller; manual install via `roomler-agent service install
/// --as-service` will surface `Access is denied (os error 5)` from a
/// filtered token.
pub fn install(exe_path: &std::path::Path) -> Result<()> {
    let manager_access = ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE;
    let manager = ServiceManager::local_computer(None::<&str>, manager_access)
        .context("opening Service Control Manager")?;

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path.to_path_buf(),
        // The SCM passes these as argv on each start. `service-run`
        // is the agent's hidden subcommand that dispatches to
        // `run_in_dispatcher`.
        launch_arguments: vec![OsString::from(RUN_SUBCOMMAND)],
        // No dependencies — the agent is fine to start before the
        // network stack is fully up; reqwest + tokio retry on first
        // resolution failure.
        dependencies: vec![],
        // None = LocalSystem. M3 needs SYSTEM for capture-from-session-0
        // and CreateProcessAsUserW; running as a normal user account
        // would need WTSQueryUserToken privileges we don't get there.
        account_name: None,
        account_password: None,
    };

    let service = manager
        .create_service(&service_info, ServiceAccess::CHANGE_CONFIG)
        .context("create_service")?;
    service
        .set_description(SERVICE_DESCRIPTION)
        .context("set_description")?;
    Ok(())
}

/// Stop (best-effort) and delete the service. Used by `service uninstall
/// --as-service` and the MSI's `UnregisterAutostart` symmetric custom
/// action. Tolerates "service not installed" so a partially-installed
/// machine can still uninstall cleanly.
pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening SCM for uninstall")?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(s) => s,
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1060) => {
            // ERROR_SERVICE_DOES_NOT_EXIST — already gone. Idempotent
            // uninstall is what we want from MSI's symmetric custom
            // action and from operator scripts.
            tracing::info!(
                service = SERVICE_NAME,
                "uninstall: service not present, nothing to do"
            );
            return Ok(());
        }
        Err(e) => return Err(anyhow::anyhow!(e).context("open_service")),
    };

    if let Ok(s) = service.query_status()
        && s.current_state != ServiceState::Stopped
    {
        // Best-effort stop; if it doesn't reach Stopped within ~5 s we
        // proceed to delete anyway and let the SCM mark it pending.
        let _ = service.stop();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            if let Ok(s) = service.query_status()
                && s.current_state == ServiceState::Stopped
            {
                break;
            }
        }
    }

    service.delete().context("delete service")?;
    tracing::info!(service = SERVICE_NAME, "uninstalled");
    Ok(())
}

/// Whether the service is currently registered with the SCM. Cheap —
/// opens the manager + tries to open the service. Used by the MSI's
/// rollback / reinstall logic and by `service status --as-service`.
pub fn is_installed() -> bool {
    let Ok(manager) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    else {
        return false;
    };
    manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
        .is_ok()
}

/// Human-readable status snapshot for the `service status --as-service`
/// CLI. Returns a plain enum the caller formats however it likes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstalledStatus {
    NotInstalled,
    Stopped,
    StartPending,
    Running,
    StopPending,
    Other(u32),
}

pub fn status() -> Result<InstalledStatus> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening SCM for status")?;
    let service = match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(s) => s,
        Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1060) => {
            return Ok(InstalledStatus::NotInstalled);
        }
        Err(e) => bail!("open_service: {e}"),
    };
    let s = service.query_status().context("query_status")?;
    Ok(match s.current_state {
        ServiceState::Stopped => InstalledStatus::Stopped,
        ServiceState::StartPending => InstalledStatus::StartPending,
        ServiceState::Running => InstalledStatus::Running,
        ServiceState::StopPending => InstalledStatus::StopPending,
        other => InstalledStatus::Other(other as u32),
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Service runtime (SCM dispatcher + main loop).
// ────────────────────────────────────────────────────────────────────────────

/// Entry point for the `service-run` subcommand. Blocks for the lifetime
/// of the service: the SCM owns the process from here. Returns when the
/// dispatcher decides we should exit (Stop control) or on a fatal error.
///
/// M1: the service main loop is a stub that idles until Stop. M2 will
/// replace the body with worker spawning + session-change handling.
pub fn run_in_dispatcher() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("service_dispatcher::start")?;
    Ok(())
}

windows_service::define_windows_service!(ffi_service_main, service_main);

/// Body of the service. Called once by the SCM dispatcher when the
/// service starts. Must call `set_service_status` with `Running` quickly
/// (within 30 s, in practice within ~1 s) and again with `Stopped`
/// before returning, otherwise the SCM force-kills the process.
fn service_main(_args: Vec<OsString>) {
    if let Err(e) = service_main_inner() {
        // Best-effort log to the persistent log dir before the process
        // dies. The dispatcher swallows panics on its end so we have to
        // surface them explicitly.
        tracing::error!(error = %e, "service main failed");
    }
}

fn service_main_inner() -> Result<()> {
    // Bootstrap logging early — the SCM swallows stderr, so anything
    // we don't write to a file is gone. Reuse the agent's existing
    // `logging::init` which dual-writes stdout + a daily-rolling
    // file in the configured log dir.
    crate::logging::init();

    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Preshutdown => {
                tracing::info!(?control_event, "service stop requested");
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            // M2 will handle SessionChange here to swap the active
            // worker process when a user logs in / out.
            ServiceControl::SessionChange(_) => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
        .context("register control handler")?;

    status_handle
        .set_service_status(running_status())
        .context("set_service_status(Running)")?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        service = SERVICE_NAME,
        "service started; main loop is M1 stub (sleeps until Stop)"
    );

    // M1: idle until Stop. M2 will spawn + supervise the worker here.
    // The recv blocks the SCM dispatcher thread, which is correct —
    // dispatcher needs us to stay alive until the controls are done.
    // 1 s wake-up gives M2 a place to plug periodic worker-health
    // checks without restructuring this loop.
    loop {
        match shutdown_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(()) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    tracing::info!("service stopping");
    status_handle
        .set_service_status(stopped_status())
        .context("set_service_status(Stopped)")?;
    Ok(())
}

fn running_status() -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP
            | ServiceControlAccept::PRESHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }
}

fn stopped_status() -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_is_stable() {
        // Renaming is a wire break against any operator who scripted
        // `Get-Service RoomlerAgentService` or `sc.exe stop ...`.
        // Lock the constant against accidental change.
        assert_eq!(SERVICE_NAME, "RoomlerAgentService");
        assert_eq!(RUN_SUBCOMMAND, "service-run");
    }

    #[test]
    fn default_log_dir_uses_programdata() {
        let dir = default_log_dir();
        assert!(dir.is_some());
        let s = dir.unwrap().to_string_lossy().to_string();
        assert!(
            s.contains("roomler") && s.contains("service-logs"),
            "log dir layout drifted: {s}"
        );
    }

    #[test]
    fn running_status_accepts_stop_and_session_change() {
        // Lock the controls we accept. M2 wires SessionChange to swap
        // the worker; a regression that drops it from
        // `controls_accepted` would silently leave the SCM not
        // notifying us about logon/logoff.
        let s = running_status();
        assert!(s.controls_accepted.contains(ServiceControlAccept::STOP));
        assert!(
            s.controls_accepted
                .contains(ServiceControlAccept::SESSION_CHANGE),
            "SESSION_CHANGE must be accepted for M2 to receive logon notifications"
        );
        assert_eq!(s.current_state, ServiceState::Running);
    }
}

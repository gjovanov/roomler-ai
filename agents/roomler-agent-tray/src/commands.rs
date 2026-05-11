//! Tauri `invoke` command handlers — thin glue between the SPA's
//! JavaScript and the agent's library / CLI.
//!
//! Each #[tauri::command] returns a JSON-serialisable result; tauri
//! marshals Result<T, String> into a promise that resolves to T on
//! Ok and rejects with the String on Err. The HTML/JS layer in
//! `src/front/` consumes these via `window.__TAURI__.core.invoke`.

use roomler_agent::config::{self, AgentConfig};
use roomler_agent::enrollment::{self, EnrollInputs};
use roomler_agent::{logging, notify};
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

/// What the SPA shows on the status page. Returned from
/// [`cmd_status`]. All fields are JSON-friendly primitives so the
/// front-end doesn't need to know about Rust types.
#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub enrolled: bool,
    pub agent_id: Option<String>,
    pub tenant_id: Option<String>,
    pub server_url: Option<String>,
    pub device_name: Option<String>,
    pub agent_version: String,
    pub config_schema_version: Option<String>,
    pub service_running: bool,
    pub service_kind: String, // "scheduledTask" | "scmService" | "none"
    pub attention: Option<String>,
    pub log_dir: String,
    pub config_dir: String,
}

/// Read current agent config + probe service state for the status
/// view. Never errors — missing config = `enrolled: false`.
#[tauri::command]
pub fn cmd_status() -> StatusReport {
    let cfg = load_optional_config();
    let log_dir = logging::log_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "(unknown)".to_string());
    let config_dir = config::default_config_path()
        .map(|p| {
            p.parent()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|_| "(unknown)".to_string());
    let (service_kind, service_running) = probe_service_state();
    let attention = if notify::has_attention() {
        notify::attention_path().map(|p| p.to_string_lossy().into_owned())
    } else {
        None
    };
    StatusReport {
        enrolled: cfg.is_some(),
        agent_id: cfg.as_ref().map(|c| c.agent_id.clone()),
        tenant_id: cfg.as_ref().map(|c| c.tenant_id.clone()),
        server_url: cfg.as_ref().map(|c| c.server_url.clone()),
        device_name: cfg.as_ref().map(|c| c.machine_name.clone()),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        config_schema_version: cfg.as_ref().and_then(|c| c.config_schema_version.clone()),
        service_running,
        service_kind,
        attention,
        log_dir,
        config_dir,
    }
}

/// First-time enrollment flow. Args mirror the CLI's `roomler-agent
/// enroll --server --token --name`. On success writes config.toml +
/// returns a redacted `StatusReport` (no agent_token).
#[tauri::command]
pub async fn cmd_enroll(
    server: String,
    token: String,
    device_name: String,
) -> Result<StatusReport, String> {
    let trimmed_token = token.trim().to_string();
    let trimmed_name = device_name.trim().to_string();
    if trimmed_token.is_empty() {
        return Err("Enrollment token is empty".to_string());
    }
    if trimmed_name.is_empty() {
        return Err("Device name is empty".to_string());
    }
    let path = config::default_config_path().map_err(|e| format!("Config path: {e}"))?;
    let machine_id = roomler_agent::machine::derive_machine_id(&path);
    let cfg = enrollment::enroll(EnrollInputs {
        server_url: &server,
        enrollment_token: &trimmed_token,
        machine_id: &machine_id,
        machine_name: &trimmed_name,
    })
    .await
    .map_err(|e| format!("Enrollment failed: {e:#}"))?;
    config::save(&path, &cfg).map_err(|e| format!("Saving config: {e}"))?;
    Ok(cmd_status())
}

/// Refresh the token using an existing config. Mirrors the CLI's
/// `re-enroll --token` subcommand.
#[tauri::command]
pub async fn cmd_re_enroll(token: String) -> Result<StatusReport, String> {
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        return Err("Enrollment token is empty".to_string());
    }
    let path = config::default_config_path().map_err(|e| format!("Config path: {e}"))?;
    let existing = config::load(&path).map_err(|e| format!("Loading config: {e}"))?;
    let cfg = enrollment::enroll(EnrollInputs {
        server_url: &existing.server_url,
        enrollment_token: &trimmed,
        machine_id: &existing.machine_id,
        machine_name: &existing.machine_name,
    })
    .await
    .map_err(|e| format!("Re-enrollment failed: {e:#}"))?;
    config::save(&path, &cfg).map_err(|e| format!("Saving config: {e}"))?;
    Ok(cmd_status())
}

/// Update the device name on the persisted config. Effective on next
/// WS reconnect — the agent re-sends `rc:agent.hello` with the new
/// name. Doesn't touch the agent process itself.
#[tauri::command]
pub fn cmd_set_device_name(name: String) -> Result<StatusReport, String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err("Device name is empty".to_string());
    }
    let path = config::default_config_path().map_err(|e| format!("Config path: {e}"))?;
    let mut cfg = config::load(&path).map_err(|e| format!("Loading config: {e}"))?;
    cfg.machine_name = trimmed;
    config::save(&path, &cfg).map_err(|e| format!("Saving config: {e}"))?;
    Ok(cmd_status())
}

/// Default device name for first enrollment — the local hostname.
/// The SPA pre-fills the device-name field with this so the operator
/// usually accepts it as-is. Falls back to "my-device" if the OS
/// hostname call fails.
#[tauri::command]
pub fn cmd_default_device_name() -> String {
    gethostname::gethostname()
        .into_string()
        .unwrap_or_else(|_| "my-device".to_string())
}

/// Spawn `roomler-agent self-update --check-only` and parse the
/// stdout — looks for the "Update available" sentinel line and
/// extracts the version pair.
#[tauri::command]
pub fn cmd_check_update() -> Result<String, String> {
    let exe = agent_exe_path()?;
    let output = Command::new(&exe)
        .args(["self-update", "--check-only"])
        .output()
        .map_err(|e| format!("Spawning self-update: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "self-update --check-only exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Trigger the actual self-update. On perMachine installs this
/// surfaces UAC (Feature 1 from the rc.18 plan). The agent exits
/// after spawning msiexec so subsequent status polls show "service
/// not running" briefly while the installer runs.
#[tauri::command]
pub fn cmd_apply_update() -> Result<(), String> {
    let exe = agent_exe_path()?;
    // Detached spawn — agent does its own self-update + exits; we
    // don't want to block the tray's event loop.
    Command::new(&exe)
        .arg("self-update")
        .spawn()
        .map_err(|e| format!("Spawning self-update: {e}"))?;
    Ok(())
}

/// Register the agent for auto-start via either Scheduled Task
/// (perUser flavour) or SCM service (perMachine flavour). The CLI
/// figures out which one based on its own install flavour.
#[tauri::command]
pub fn cmd_service_install(as_service: bool) -> Result<(), String> {
    let exe = agent_exe_path()?;
    let mut cmd = Command::new(&exe);
    cmd.arg("service").arg("install");
    if as_service {
        cmd.arg("--as-service");
    }
    let status = cmd
        .status()
        .map_err(|e| format!("Spawning service install: {e}"))?;
    if !status.success() {
        return Err(format!("service install exited {:?}", status.code()));
    }
    Ok(())
}

/// Symmetric uninstall.
#[tauri::command]
pub fn cmd_service_uninstall(as_service: bool) -> Result<(), String> {
    let exe = agent_exe_path()?;
    let mut cmd = Command::new(&exe);
    cmd.arg("service").arg("uninstall");
    if as_service {
        cmd.arg("--as-service");
    }
    let status = cmd
        .status()
        .map_err(|e| format!("Spawning service uninstall: {e}"))?;
    if !status.success() {
        return Err(format!("service uninstall exited {:?}", status.code()));
    }
    Ok(())
}

/// Report the service's current state (Running / Stopped /
/// NotInstalled). Returns stdout verbatim — the SPA renders it as
/// a one-line status badge.
#[tauri::command]
pub fn cmd_service_status(as_service: bool) -> Result<String, String> {
    let exe = agent_exe_path()?;
    let mut cmd = Command::new(&exe);
    cmd.arg("service").arg("status");
    if as_service {
        cmd.arg("--as-service");
    }
    let out = cmd
        .output()
        .map_err(|e| format!("Spawning service status: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Open the agent's log directory in the OS file manager. Uses the
/// platform's default open verb (Explorer / Finder / xdg-open).
#[tauri::command]
pub fn cmd_open_log_dir() -> Result<(), String> {
    let path = logging::log_dir().ok_or_else(|| "log dir not resolvable".to_string())?;
    open_path_in_explorer(&path)
}

/// Open the agent's config directory in the OS file manager.
#[tauri::command]
pub fn cmd_open_config_dir() -> Result<(), String> {
    let path = config::default_config_path().map_err(|e| format!("Config path: {e}"))?;
    let dir = path
        .parent()
        .ok_or_else(|| "Config path has no parent".to_string())?;
    open_path_in_explorer(dir)
}

/// Approve a pending operator-consent prompt. Drops the sentinel
/// directly via the ConsentBroker's public helper — no subprocess.
#[tauri::command]
pub fn cmd_consent_approve(session: String) -> Result<String, String> {
    let dir = roomler_agent::consent::ConsentBroker::default_sentinel_dir()
        .map_err(|e| format!("Consent dir: {e}"))?;
    let broker = roomler_agent::consent::ConsentBroker::new(
        roomler_agent::consent::Mode::AutoGrant, // mode irrelevant — we only use write_sentinel
        dir,
    )
    .map_err(|e| format!("Opening consent broker: {e}"))?;
    let path = broker
        .write_sentinel(&session, roomler_agent::consent::SentinelKind::Approve)
        .map_err(|e| format!("Writing sentinel: {e}"))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Deny a pending operator-consent prompt.
#[tauri::command]
pub fn cmd_consent_deny(session: String) -> Result<String, String> {
    let dir = roomler_agent::consent::ConsentBroker::default_sentinel_dir()
        .map_err(|e| format!("Consent dir: {e}"))?;
    let broker =
        roomler_agent::consent::ConsentBroker::new(roomler_agent::consent::Mode::AutoGrant, dir)
            .map_err(|e| format!("Opening consent broker: {e}"))?;
    let path = broker
        .write_sentinel(&session, roomler_agent::consent::SentinelKind::Deny)
        .map_err(|e| format!("Writing sentinel: {e}"))?;
    Ok(path.to_string_lossy().into_owned())
}

// ─── helpers ───────────────────────────────────────────────────────

/// Load the agent config from its default path. Returns `None` on
/// "no config yet" (operator hasn't enrolled), which is the natural
/// pre-enrollment state. Errors during parse are also collapsed to
/// `None` — the status view shows "not enrolled" and the operator
/// re-onboards.
fn load_optional_config() -> Option<AgentConfig> {
    let path = config::default_config_path().ok()?;
    if !path.exists() {
        return None;
    }
    config::load(&path).ok()
}

/// Probe service state via the agent's own `service status` CLI.
/// Returns (kind, running). `kind` is "scheduledTask" on perUser and
/// "scmService" on perMachine. "none" when neither is registered.
fn probe_service_state() -> (String, bool) {
    let Ok(exe) = agent_exe_path() else {
        return ("none".to_string(), false);
    };
    // Scheduled Task probe — works for both flavours' status query.
    let task_status = Command::new(&exe).args(["service", "status"]).output().ok();
    if let Some(out) = task_status {
        let s = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
        if s.contains("running") {
            return ("scheduledTask".to_string(), true);
        }
    }
    // SCM service probe (perMachine).
    let svc_status = Command::new(&exe)
        .args(["service", "status", "--as-service"])
        .output()
        .ok();
    if let Some(out) = svc_status {
        let s = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
        if s.contains("running") {
            return ("scmService".to_string(), true);
        }
        if s.contains("stopped") {
            return ("scmService".to_string(), false);
        }
    }
    ("none".to_string(), false)
}

/// Resolve the agent's executable path. For a packaged install, the
/// tray and agent ship in the same dir (per the MSI layout). For dev
/// builds, fall back to PATH lookup.
fn agent_exe_path() -> Result<PathBuf, String> {
    let exe_name = if cfg!(windows) {
        "roomler-agent.exe"
    } else {
        "roomler-agent"
    };
    // Prefer same dir as the tray (production layout).
    if let Ok(tray_exe) = std::env::current_exe()
        && let Some(dir) = tray_exe.parent()
    {
        let candidate = dir.join(exe_name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    // Fall back to bare name — relies on PATH (dev runs / Linux
    // installs that put roomler-agent in /usr/bin).
    Ok(PathBuf::from(exe_name))
}

fn open_path_in_explorer(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        Command::new("explorer")
            .arg(path)
            .spawn()
            .map_err(|e| format!("explorer.exe: {e}"))?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("open: {e}"))?;
        Ok(())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("xdg-open: {e}"))?;
        Ok(())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        Err(format!(
            "Don't know how to open {} on this platform",
            path.display()
        ))
    }
}

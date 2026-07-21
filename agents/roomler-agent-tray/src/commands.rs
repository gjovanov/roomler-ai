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
use std::path::{Path, PathBuf};
use std::process::Command;
use tunnel_core::localapi::{self, ConsentRequest, FlowInfo, NodeStatus, PeerInfo};

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
    /// Both a machine-global AND a per-user config exist — a split-brain
    /// install (e.g. an old per-user enrollment left behind under an SCM
    /// service). The Settings view surfaces it so the stale copy gets
    /// cleaned up instead of silently shadowing.
    pub config_split: bool,
}

/// Read current agent config + probe service state for the status view. Never
/// errors — missing config = `enrolled: false`.
///
/// ASYNC so the blocking service-state probe runs OFF the main (UI) thread:
/// Tauri runs synchronous commands on the main thread, and `status.js` polls
/// this every 10 s. `probe_service_state()` spawns + waits on the console-mode
/// agent CLI TWICE, so a synchronous `cmd_status` froze the whole webview for a
/// couple of seconds every 10 s (field-observed on rc.156). Off-loading it to
/// the blocking pool keeps the tray responsive.
#[tauri::command]
pub async fn cmd_status() -> StatusReport {
    tokio::task::spawn_blocking(status_report)
        .await
        .unwrap_or_else(|_| status_report())
}

/// The blocking status-probe body — run on the blocking pool by [`cmd_status`],
/// and directly by the (already-async, user-triggered) enroll commands.
fn status_report() -> StatusReport {
    let (service_kind, service_running) = probe_service_state();
    let is_scm = service_kind == "scmService";
    let cfg = load_optional_config(is_scm);
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
        log_dir: resolve_log_dir_string(is_scm),
        config_dir: resolve_config_dir_string(is_scm),
        config_split: config_split_detected(),
    }
}

/// The daemon log directory to show / open. `logging::log_dir()` only works IN
/// the agent process (its `LOG_DIR` OnceLock); the desktop app never runs that
/// setup, so it computes the path directly. An SCM/SYSTEM service writes to the
/// deterministic machine-global dir (`win_service::default_log_dir` =
/// `%PROGRAMDATA%\...\service-logs`) — used exactly when an SCM service is the
/// registered flavour. Keyed on the flavour, NOT dir-existence: a
/// flavour-switched box can carry a stale (SYSTEM-ACL'd, undeletable) service
/// dir forever, which must not shadow the per-user daemon's real logs.
fn resolve_log_dir_path(is_scm: bool) -> Option<PathBuf> {
    #[cfg(windows)]
    if is_scm && let Some(service_logs) = roomler_agent::win_service::default_log_dir() {
        return Some(service_logs);
    }
    #[cfg(not(windows))]
    let _ = is_scm;
    logging::log_dir().or_else(logging::resolve_log_dir)
}

fn resolve_log_dir_string(is_scm: bool) -> String {
    resolve_log_dir_path(is_scm)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "(unknown)".to_string())
}

/// The config file the daemon on this host actually reads. Mirrors the role
/// rung of the daemon's own `pick_config_path` ladder: only a machine-wide
/// (SCM/SystemContext) service reads the machine-global `%PROGRAMDATA%` config
/// — a per-user daemon reads the per-user default ALWAYS, so a stale
/// machine-global file left behind by an old perMachine install must never
/// shadow it. Pure decision in [`choose_config_path`] so the precedence is
/// locked by a test.
fn active_config_path(is_scm: bool) -> Result<PathBuf, String> {
    let default = config::default_config_path().map_err(|e| format!("Config path: {e}"))?;
    #[cfg(windows)]
    {
        let mg = config::machine_global_config_path();
        let mg_exists = mg.exists();
        Ok(choose_config_path(is_scm, mg_exists, mg, default))
    }
    #[cfg(not(windows))]
    {
        let _ = is_scm;
        Ok(default)
    }
}

/// Machine-global only for an SCM-service flavour AND when the file exists
/// (an SCM install briefly runs on a per-user config until the daemon
/// self-heals it to machine-global); per-user in every other case.
#[cfg_attr(not(windows), allow(dead_code))]
fn choose_config_path(
    is_scm: bool,
    machine_global_exists: bool,
    machine_global: PathBuf,
    default: PathBuf,
) -> PathBuf {
    if is_scm && machine_global_exists {
        machine_global
    } else {
        default
    }
}

/// Both configs present ⇒ split-brain (see `StatusReport::config_split`).
fn config_split_detected() -> bool {
    #[cfg(windows)]
    {
        let per_user = config::default_config_path()
            .map(|p| p.exists())
            .unwrap_or(false);
        per_user && config::machine_global_config_path().exists()
    }
    #[cfg(not(windows))]
    false
}

/// The config directory to show / open — the parent of [`active_config_path`].
fn resolve_config_dir_path(is_scm: bool) -> Option<PathBuf> {
    active_config_path(is_scm)
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

fn resolve_config_dir_string(is_scm: bool) -> String {
    resolve_config_dir_path(is_scm)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "(unknown)".to_string())
}

/// Contextualise a config-save failure: writing the machine-global config from
/// a non-elevated desktop app is expected to be denied — say so instead of
/// leaving a bare os-error.
fn explain_save_error(err: impl std::fmt::Display, path: &Path, machine_global: bool) -> String {
    if machine_global {
        format!(
            "Saving config at {}: {err}. This is the machine-wide configuration — \
             administrator rights are required (run the desktop app elevated, or \
             use the `roomlerd` CLI from an elevated shell).",
            path.display()
        )
    } else {
        format!("Saving config at {}: {err}", path.display())
    }
}

/// What the "Devices" page renders (unification P2). Read from the running
/// daemon over the LocalAPI. `available` = the daemon's local control endpoint
/// was reachable; the nested `status.connected` is the SEPARATE daemon↔server
/// link. All JSON-friendly (the `localapi` wire types are `Serialize`).
#[derive(Debug, Serialize)]
pub struct DeviceView {
    /// The daemon's LocalAPI pipe/socket was reachable.
    pub available: bool,
    /// Why not, when `available` is false: `"daemon_unreachable"` (pipe absent —
    /// the agent isn't running) or `"connect_error"` (other I/O).
    pub reason: Option<String>,
    /// This node's status, when reachable.
    pub status: Option<NodeStatus>,
    /// Peers with their current connection type (empty when the overlay is off
    /// or the daemon is disconnected from the server).
    pub peers: Vec<PeerInfo>,
}

impl DeviceView {
    fn unavailable(reason: &str) -> Self {
        Self {
            available: false,
            reason: Some(reason.to_string()),
            status: None,
            peers: Vec::new(),
        }
    }
}

/// Read the live device view from the daemon over the LocalAPI. NEVER errors
/// (mirrors [`cmd_status`]): if the agent isn't running the pipe/socket is
/// absent, and this returns `available:false` + a `reason` so the SPA renders a
/// clean "device service not running" state instead of a rejected promise. On
/// success it issues `status` then `peers` on ONE connection.
#[tauri::command]
pub async fn cmd_device_view() -> DeviceView {
    let mut client = match localapi::connect().await {
        Ok(c) => c,
        Err(e) => {
            let reason = if e.kind() == std::io::ErrorKind::NotFound {
                "daemon_unreachable"
            } else {
                "connect_error"
            };
            return DeviceView::unavailable(reason);
        }
    };
    let status = match client.status().await {
        Ok(s) => s,
        // Reached the endpoint but the exchange failed (daemon shutting down,
        // protocol error) — treat as unreachable for the UI.
        Err(_) => return DeviceView::unavailable("daemon_unreachable"),
    };
    // Peers are best-effort: a status-ok / peers-fail shouldn't blank the view.
    let peers = client.peers().await.unwrap_or_default();
    DeviceView {
        available: true,
        reason: None,
        status: Some(status),
        peers,
    }
}

/// A live ICMP-ping result over the netstack — returned from [`cmd_ping`] for the
/// SPA's per-peer Ping button. `rtt_ms` is the userspace round-trip time.
#[derive(Debug, Serialize)]
pub struct PingResult {
    pub overlay_ip: String,
    pub rtt_ms: f64,
}

/// `cmd_ping(target, timeoutMs?, preferV6?)` — ICMP-ping an overlay peer (by
/// name or IP) over the userspace netstack via the daemon's LocalAPI. Mirrors
/// [`cmd_device_view`]'s connect pattern; a missing daemon or a daemon-side error
/// (unknown peer / timeout / "not a netstack node") rejects with a user-facing
/// string the SPA shows on the button. `preferV6` resolves a name target to the
/// peer's derived overlay IPv6.
#[tauri::command]
pub async fn cmd_ping(
    target: String,
    timeout_ms: Option<u64>,
    prefer_v6: Option<bool>,
) -> Result<PingResult, String> {
    let mut client = localapi::connect().await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "device service not running".to_string()
        } else {
            format!("connecting to the device service: {e}")
        }
    })?;
    let (overlay_ip, rtt_ms) = client
        .ping(
            &target,
            timeout_ms.unwrap_or(3000),
            prefer_v6.unwrap_or(false),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(PingResult { overlay_ip, rtt_ms })
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
    Ok(status_report())
}

/// Refresh the token using an existing config. Mirrors the CLI's
/// `re-enroll --token` subcommand. Targets the config the daemon
/// actually reads (machine-global first) — writing the per-user copy
/// under an SCM install would silently change nothing.
#[tauri::command]
pub async fn cmd_re_enroll(token: String) -> Result<StatusReport, String> {
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        return Err("Enrollment token is empty".to_string());
    }
    // The flavour probe shells out to the agent CLI — keep it off the async
    // runtime's worker thread.
    let is_scm = tokio::task::spawn_blocking(|| probe_service_state().0 == "scmService")
        .await
        .unwrap_or(false);
    let path = active_config_path(is_scm)?;
    let machine_global = cfg!(windows) && path == config::machine_global_config_path();
    let existing = config::load(&path).map_err(|e| format!("Loading config: {e}"))?;
    let cfg = enrollment::enroll(EnrollInputs {
        server_url: &existing.server_url,
        enrollment_token: &trimmed,
        machine_id: &existing.machine_id,
        machine_name: &existing.machine_name,
    })
    .await
    .map_err(|e| format!("Re-enrollment failed: {e:#}"))?;
    config::save(&path, &cfg).map_err(|e| explain_save_error(e, &path, machine_global))?;
    Ok(status_report())
}

/// Update the device name on the persisted config. Effective on next
/// WS reconnect — the agent re-sends `rc:agent.hello` with the new
/// name. Doesn't touch the agent process itself. Targets the config the
/// daemon actually reads. ASYNC + blocking-pool because the flavour
/// probe + final status report both shell out to the agent CLI (the
/// cmd_status lesson: sync commands run on the UI thread).
#[tauri::command]
pub async fn cmd_set_device_name(name: String) -> Result<StatusReport, String> {
    tokio::task::spawn_blocking(move || set_device_name_blocking(name))
        .await
        .map_err(|e| format!("task join: {e}"))?
}

fn set_device_name_blocking(name: String) -> Result<StatusReport, String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err("Device name is empty".to_string());
    }
    let is_scm = probe_service_state().0 == "scmService";
    let path = active_config_path(is_scm)?;
    let machine_global = cfg!(windows) && path == config::machine_global_config_path();
    let mut cfg = config::load(&path).map_err(|e| format!("Loading config: {e}"))?;
    cfg.machine_name = trimmed;
    config::save(&path, &cfg).map_err(|e| explain_save_error(e, &path, machine_global))?;
    Ok(status_report())
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
    let output = no_window_command(&exe)
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
    no_window_command(&exe)
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
    let mut cmd = no_window_command(&exe);
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
    let mut cmd = no_window_command(&exe);
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
    let mut cmd = no_window_command(&exe);
    cmd.arg("service").arg("status");
    if as_service {
        cmd.arg("--as-service");
    }
    let out = cmd
        .output()
        .map_err(|e| format!("Spawning service status: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Open the daemon's log directory in the OS file manager. ASYNC because
/// resolving the directory probes the service flavour (shells out to the
/// daemon CLI) — that must stay off the UI thread.
#[tauri::command]
pub async fn cmd_open_log_dir() -> Result<(), String> {
    tokio::task::spawn_blocking(open_log_dir_blocking)
        .await
        .map_err(|e| format!("task join: {e}"))?
}

/// The blocking body — also called from the tray menu (via its own
/// blocking-pool spawn in `tray.rs`).
pub fn open_log_dir_blocking() -> Result<(), String> {
    let is_scm = probe_service_state().0 == "scmService";
    let path = resolve_log_dir_path(is_scm).ok_or_else(|| "log dir not resolvable".to_string())?;
    // Create it if the daemon hasn't written a log here yet, so the folder
    // opens instead of failing. Best-effort: an SCM service-logs dir is
    // SYSTEM-created and already exists on a live install.
    let _ = std::fs::create_dir_all(&path);
    open_path_in_explorer(&path)
}

/// Open the daemon's config directory in the OS file manager. ASYNC for the
/// same flavour-probe reason as [`cmd_open_log_dir`].
#[tauri::command]
pub async fn cmd_open_config_dir() -> Result<(), String> {
    tokio::task::spawn_blocking(|| {
        let is_scm = probe_service_state().0 == "scmService";
        let dir = resolve_config_dir_path(is_scm)
            .ok_or_else(|| "config dir not resolvable".to_string())?;
        open_path_in_explorer(&dir)
    })
    .await
    .map_err(|e| format!("task join: {e}"))?
}

/// Approve a pending operator-consent prompt over the LocalAPI (P2b). The daemon
/// owns the profile-correct sentinel dir, so this works even when the agent runs
/// as SYSTEM — where the tray writing the sentinel itself would land in the
/// wrong profile and the agent would never see it.
#[tauri::command]
pub async fn cmd_consent_approve(session: String) -> Result<String, String> {
    consent_decide(&session, true).await
}

/// Deny a pending operator-consent prompt over the LocalAPI.
#[tauri::command]
pub async fn cmd_consent_deny(session: String) -> Result<String, String> {
    consent_decide(&session, false).await
}

/// Send an Approve/Deny decision to the daemon over the LocalAPI.
async fn consent_decide(session: &str, allow: bool) -> Result<String, String> {
    let mut client = localapi::connect()
        .await
        .map_err(|e| format!("Device service unreachable: {e}"))?;
    let ok = client
        .consent_decide(session, allow)
        .await
        .map_err(|e| format!("LocalAPI error: {e}"))?;
    if ok {
        Ok(if allow {
            "approved".into()
        } else {
            "denied".into()
        })
    } else {
        Err("The device service rejected the decision (unknown or invalid session).".into())
    }
}

/// List consent requests currently awaiting a decision — asked of the daemon
/// over the LocalAPI (it reads its own, profile-correct sentinel dir). The SPA
/// polls this to render the Approve/Deny modal. NEVER errors — the modal must
/// stay quiet when the daemon is down or nothing is pending. `ConsentRequest`
/// serialises to the same `{session_id, controller_name, permissions,
/// timeout_secs}` shape the SPA already consumes.
#[tauri::command]
pub async fn cmd_get_pending_consents() -> Vec<ConsentRequest> {
    match localapi::connect().await {
        Ok(mut c) => c.consent_pending().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

// ─── declared routes (P6 — the Tunnels pane) ───────────────────────

/// Declared routes + live state for the Tunnels pane. NEVER errors —
/// like [`cmd_get_pending_consents`], the pane shows its own zero-state
/// when the daemon is down (an empty list is indistinguishable from
/// "no routes", and the Devices section already surfaces daemon-down).
#[tauri::command]
pub async fn cmd_route_list() -> Vec<tunnel_core::localapi::RouteInfo> {
    match localapi::connect().await {
        Ok(mut c) => c.route_list().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Declare a daemon-supervised route. The daemon validates + persists it
/// (its config `[[tunnel_routes]]`) and reconciles it into a live flow;
/// its error strings (bad node, duplicate port, config write failure)
/// surface verbatim on the form. Returns the effective descriptor (id
/// generated when the form left it blank).
#[tauri::command]
pub async fn cmd_route_add(
    route: tunnel_core::localapi::RouteDescriptor,
) -> Result<tunnel_core::localapi::RouteDescriptor, String> {
    let mut client = localapi::connect().await.map_err(daemon_unreachable)?;
    client.route_add(route).await.map_err(|e| e.to_string())
}

/// Remove a declared route (kills its live flow, deletes it from the
/// daemon config). `Ok(false)` when the id was unknown.
#[tauri::command]
pub async fn cmd_route_remove(id: String) -> Result<bool, String> {
    let mut client = localapi::connect().await.map_err(daemon_unreachable)?;
    client.route_remove(&id).await.map_err(|e| e.to_string())
}

/// Enable/disable a declared route (enabling clears a terminal `failed`).
#[tauri::command]
pub async fn cmd_route_set_enabled(id: String, enabled: bool) -> Result<bool, String> {
    let mut client = localapi::connect().await.map_err(daemon_unreachable)?;
    client
        .route_set_enabled(&id, enabled)
        .await
        .map_err(|e| e.to_string())
}

/// Live forwards / SOCKS5 listeners with their per-flow byte counters —
/// the "watch its live bytes" surface (unification §4.3). Covers BOTH
/// daemon-supervised routes (each active route is backed by a flow) and
/// ephemeral CLI-created flows. NEVER errors (mirrors [`cmd_route_list`]):
/// daemon down ⇒ empty list, and the Devices section already surfaces
/// daemon-down explicitly.
#[tauri::command]
pub async fn cmd_flows() -> Vec<FlowInfo> {
    match localapi::connect().await {
        Ok(mut c) => c.flows().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// The shared connect-error mapping for the mutating route commands
/// (mirrors [`cmd_ping`]'s wording so the two surfaces read the same).
fn daemon_unreachable(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "device service not running".to_string()
    } else {
        format!("connecting to the device service: {e}")
    }
}

// ─── helpers ───────────────────────────────────────────────────────

/// Load the agent config from the path the daemon actually reads
/// ([`active_config_path`]). Returns `None` on "no config yet" (operator
/// hasn't enrolled), which is the natural pre-enrollment state. Errors
/// during parse are also collapsed to `None` — the status view shows
/// "not enrolled" and the operator re-onboards. Pre-overhaul this read
/// ONLY the per-user path, so an SCM-service install (machine-global
/// config, no per-user copy) showed "Not enrolled" while the service ran
/// enrolled.
fn load_optional_config(is_scm: bool) -> Option<AgentConfig> {
    let path = active_config_path(is_scm).ok()?;
    if !path.exists() {
        return None;
    }
    config::load(&path).ok()
}

/// A `Command` that never flashes a console window on Windows. The tray is a GUI
/// app (`windows_subsystem = "windows"`), so a plain `std::process::Command`
/// spawning the console-mode `roomler-agent` pops a console each time — and
/// `cmd_status` polls the service state every 10 s, so without this the tray
/// flashes a terminal every 10 s. No-op on non-Windows.
fn no_window_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Probe service state via the agent's own `service status` CLI.
/// Returns (kind, running). `kind` is "scheduledTask" on perUser and
/// "scmService" on perMachine. "none" when neither is registered.
fn probe_service_state() -> (String, bool) {
    let Ok(exe) = agent_exe_path() else {
        return ("none".to_string(), false);
    };
    // Scheduled Task probe — works for both flavours' status query.
    let task_status = no_window_command(&exe)
        .args(["service", "status"])
        .output()
        .ok();
    if let Some(out) = task_status {
        let s = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
        if s.contains("running") {
            return ("scheduledTask".to_string(), true);
        }
    }
    // SCM service probe (perMachine).
    let svc_status = no_window_command(&exe)
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

/// Resolve the agent daemon's executable path. For a packaged install, the
/// tray and daemon ship in the same dir (per the MSI layout). For dev
/// builds, fall back to PATH lookup.
///
/// P3d Slice B renamed the daemon OUTPUT binary `roomler-agent` -> `roomlerd`.
/// Resolution prefers a sibling `roomlerd[.exe]` (so a fresh tray spawns the
/// new daemon), then falls back to the legacy `roomler-agent[.exe]` (which the
/// MSI still ships as the inert `AgentExeAlias`, so a mixed / in-flight install
/// still resolves), then finally the bare new name relying on PATH.
fn agent_exe_path() -> Result<PathBuf, String> {
    let (new_name, old_name) = if cfg!(windows) {
        ("roomlerd.exe", "roomler-agent.exe")
    } else {
        ("roomlerd", "roomler-agent")
    };
    // Prefer same dir as the tray (production layout): new name first, then
    // the legacy alias so a mixed install still resolves.
    if let Ok(tray_exe) = std::env::current_exe()
        && let Some(dir) = tray_exe.parent()
    {
        for name in [new_name, old_name] {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    // Fall back to the bare new name — relies on PATH (dev runs / Linux
    // installs that put roomlerd in /usr/bin).
    Ok(PathBuf::from(new_name))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The split-brain lock: machine-global is read ONLY under an SCM
    /// flavour — a stale `%PROGRAMDATA%` config from an old perMachine
    /// install must never shadow a per-user daemon's real config, and an
    /// SCM install falls back to per-user until the daemon self-heals the
    /// machine-global copy into place.
    #[test]
    fn choose_config_prefers_machine_global_only_for_scm() {
        let mg = PathBuf::from("mg/config.toml");
        let user = PathBuf::from("user/config.toml");
        assert_eq!(
            choose_config_path(true, true, mg.clone(), user.clone()),
            mg,
            "SCM flavour + machine-global present must read machine-global"
        );
        assert_eq!(
            choose_config_path(true, false, mg.clone(), user.clone()),
            user,
            "SCM flavour before self-heal falls back to per-user"
        );
        assert_eq!(
            choose_config_path(false, true, mg.clone(), user.clone()),
            user,
            "a per-user daemon never reads a (stale) machine-global config"
        );
        assert_eq!(choose_config_path(false, false, mg, user.clone()), user);
    }

    #[test]
    fn save_error_mentions_elevation_only_for_machine_global() {
        let p = Path::new("C:/ProgramData/roomler/config.toml");
        let elevated = explain_save_error("denied", p, true);
        assert!(
            elevated.contains("administrator"),
            "machine-global failures must explain the elevation requirement: {elevated}"
        );
        let user = explain_save_error("denied", p, false);
        assert!(
            !user.contains("administrator"),
            "per-user failures must not claim elevation is needed: {user}"
        );
    }
}

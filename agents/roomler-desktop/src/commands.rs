// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Tauri `invoke` command handlers — thin glue between the SPA's
//! JavaScript and the agent's library / CLI.
//!
//! Each #[tauri::command] returns a JSON-serialisable result; tauri
//! marshals Result<T, String> into a promise that resolves to T on
//! Ok and rejects with the String on Err. The HTML/JS layer in
//! `src/front/` consumes these via `window.__TAURI__.core.invoke`.

use roomler_node_core::config::{self, AgentConfig};
use roomler_node_core::enrollment::{self, EnrollInputs};
use roomler_node_core::{logging, notify};
use roomler_localapi::{self as localapi, ConsentRequest, FlowInfo, NodeStatus, PeerInfo};
use serde::Serialize;
use std::path::{Path, PathBuf};
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
    /// S1b — the sentinel's human message + machine reason, so the
    /// Overview can say WHAT needs attention (and offer a re-enroll
    /// action) instead of just printing a file path.
    pub attention_message: Option<String>,
    pub attention_reason: Option<String>,
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
    // S1b — read BOTH sentinel locations (per-user + machine-global) and
    // parse message/reason; the old path-only read missed a SystemContext
    // host's machine-global sentinel entirely.
    let attention_info = notify::read_any_attention();
    let attention = attention_info
        .as_ref()
        .map(|i| i.path.to_string_lossy().into_owned());
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
        attention_message: attention_info.as_ref().map(|i| i.message.clone()),
        attention_reason: attention_info.and_then(|i| i.reason),
        log_dir: resolve_log_dir_string(is_scm),
        config_dir: resolve_config_dir_string(is_scm),
        config_split: config_split_detected(),
    }
}

/// The daemon log directory to show / open. `logging::log_dir()` only works IN
/// the agent process (its `LOG_DIR` OnceLock); the desktop app never runs that
/// setup, so it computes the path directly. An SCM/SYSTEM service writes to the
/// deterministic machine-global dir (`appdirs::service_log_dir` =
/// `%PROGRAMDATA%\...\service-logs`) — used exactly when an SCM service is the
/// registered flavour. Keyed on the flavour, NOT dir-existence: a
/// flavour-switched box can carry a stale (SYSTEM-ACL'd, undeletable) service
/// dir forever, which must not shadow the per-user daemon's real logs.
fn resolve_log_dir_path(is_scm: bool) -> Option<PathBuf> {
    #[cfg(windows)]
    if is_scm {
        return Some(roomler_node_core::appdirs::service_log_dir());
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

/// Is `path` the machine-global config? (`machine_global_config_path` is
/// Windows-only — a `cfg!(windows) &&` runtime check would still fail to
/// COMPILE the call on Linux CI.)
fn is_machine_global(path: &Path) -> bool {
    #[cfg(windows)]
    {
        path == config::machine_global_config_path()
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
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

/// First-time enrollment flow. Args mirror the CLI's `roomlerd
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
    // S1b — target the config the daemon actually READS. The old
    // unconditional per-user write meant "re-enroll" via Onboarding on an
    // SCM install landed in a file the daemon ignores — manufacturing the
    // exact split-brain the Settings banner warns about.
    let is_scm = tokio::task::spawn_blocking(|| probe_service_state().0 == "scmService")
        .await
        .unwrap_or(false);
    let path = match active_config_path(is_scm) {
        Ok(p) => p,
        Err(_) => config::default_config_path().map_err(|e| format!("Config path: {e}"))?,
    };
    let machine_id = roomler_node_core::machine::derive_machine_id(&path);
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
    let machine_global = is_machine_global(&path);
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

/// S1b — ask the RUNNING daemon to archive the stale config copy (the
/// split-config banner's button). Daemon-only by design: it knows which
/// copy it loaded, guards on matching identity, and has the rights an
/// unelevated desktop app lacks for `%PROGRAMDATA%`. Ok carries the
/// human detail ("archived … -> …"); Err carries why nothing was done.
#[tauri::command]
pub async fn cmd_config_cleanup() -> Result<String, String> {
    let mut client = localapi::connect()
        .await
        .map_err(|e| format!("daemon unreachable: {e}"))?;
    match client.config_cleanup_stale().await {
        Ok((true, detail)) => Ok(detail),
        Ok((false, detail)) => Err(detail),
        Err(e) => Err(e.to_string()),
    }
}

/// S2 — the editable config surface (key + current value + editor
/// metadata). Daemon-verb first (the daemon reads its OWN config, which
/// is profile-correct under SCM/SystemContext); direct-file fallback
/// reads the tray's active config so the editor still renders when the
/// daemon is down.
#[tauri::command]
pub async fn cmd_config_entries() -> Result<Vec<localapi::ConfigEntry>, String> {
    if let Ok(mut client) = localapi::connect().await
        && let Ok(entries) = client.config_entries().await
    {
        return Ok(entries);
    }
    tokio::task::spawn_blocking(|| {
        let is_scm = probe_service_state().0 == "scmService";
        let path = active_config_path(is_scm)?;
        let cfg = config::load(&path).map_err(|e| format!("Loading config: {e}"))?;
        Ok(roomler_node_core::config_surface::entries(&cfg))
    })
    .await
    .map_err(|e| format!("task join: {e}"))?
}

/// S2 — set (`value` present) or clear (`value` null) one editable
/// config key. Daemon-verb first, same rationale as
/// [`cmd_set_device_name`]; the direct-file fallback runs the SAME
/// per-key validator (`config_surface::apply`), so a validation error
/// reads identically on both paths and nothing skips validation.
/// Changes take effect on the next daemon restart.
#[tauri::command]
pub async fn cmd_config_set(
    key: String,
    value: Option<String>,
) -> Result<localapi::ConfigEntry, String> {
    let mut daemon_err: Option<String> = None;
    if let Ok(mut client) = localapi::connect().await {
        match client.config_set(&key, value.as_deref()).await {
            Ok(entry) => return Ok(entry),
            Err(e) => daemon_err = Some(e.to_string()),
        }
    }
    let direct = tokio::task::spawn_blocking(move || config_set_blocking(key, value))
        .await
        .map_err(|e| format!("task join: {e}"))?;
    // Prefer the daemon's message when the direct path ALSO failed — it
    // names the real gate (validation text, or why the verb refused).
    match (direct, daemon_err) {
        (Ok(entry), _) => Ok(entry),
        (Err(_), Some(de)) => Err(de),
        (Err(fe), None) => Err(fe),
    }
}

fn config_set_blocking(
    key: String,
    value: Option<String>,
) -> Result<localapi::ConfigEntry, String> {
    let is_scm = probe_service_state().0 == "scmService";
    let path = active_config_path(is_scm)?;
    let machine_global = is_machine_global(&path);
    let mut cfg = config::load(&path).map_err(|e| format!("Loading config: {e}"))?;
    roomler_node_core::config_surface::apply(&mut cfg, &key, value.as_deref())?;
    config::save(&path, &cfg).map_err(|e| explain_save_error(e, &path, machine_global))?;
    roomler_node_core::config_surface::entry_for(&cfg, &key)
        .ok_or_else(|| format!("unknown config key {key:?}"))
}

/// S7 — the embedded web window's label. The main tray window keeps its
/// close-to-hide behavior; a window with THIS label is destroyed on
/// close (see `main.rs`) so it never lingers as a hidden WebView2
/// process holding memory. Windows-only, like [`open_web_window`] — the
/// other platforms open the default browser instead.
#[cfg(windows)]
pub const WEB_WINDOW_LABEL: &str = "roomler-web";

/// S7 — open (or focus + navigate) the embedded WebView2 window on the
/// Roomler web app. The URL is an EXTERNAL origin, which Tauri keeps
/// outside the app's capability set — the page gets no `__TAURI__` IPC,
/// it's a plain Chromium view; sign-in persists in the webview profile
/// between opens.
#[cfg(windows)]
pub fn open_web_window<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    url: &str,
) -> Result<(), String> {
    use tauri::Manager;
    let parsed: tauri::Url = url.parse().map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("only http(s) URLs can be opened".to_string());
    }
    if let Some(existing) = app.get_webview_window(WEB_WINDOW_LABEL) {
        // Reuse the window (and its signed-in session): navigate in place.
        existing
            .navigate(parsed)
            .map_err(|e| format!("navigating the Roomler window: {e}"))?;
        let _ = existing.show();
        let _ = existing.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(app, WEB_WINDOW_LABEL, tauri::WebviewUrl::External(parsed))
        .title("Roomler")
        .inner_size(1280.0, 800.0)
        .build()
        .map_err(|e| format!("opening the Roomler window: {e}"))?;
    Ok(())
}

/// S7 — hybrid open policy: the in-app WebView2 window on Windows
/// (Chromium: the full WebRTC/WebCodecs viewer works), the default
/// browser elsewhere (WebKitGTK has no usable WebRTC; macOS WKWebView
/// loses the Chrome-tuned worker paths) and as the fallback when the
/// webview can't start (e.g. WebView2 runtime missing).
pub fn open_web_or_browser<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    url: &str,
) -> Result<(), String> {
    #[cfg(windows)]
    {
        match open_web_window(app, url) {
            Ok(()) => return Ok(()),
            Err(e) => tracing::warn!(%e, "webview window failed — falling back to the browser"),
        }
    }
    // shell::open is deprecated in favour of tauri-plugin-opener, but the
    // shell plugin is already shipped + initialised here; not worth a new
    // plugin dependency for the fallback path.
    #[allow(deprecated)]
    {
        use tauri_plugin_shell::ShellExt;
        app.shell()
            .open(url, None)
            .map_err(|e| format!("opening browser: {e}"))
    }
}

/// The configured server origin (normalised, validated) — the target for
/// the S7 "Open Roomler" entry points. Blocking (config load + service
/// probe): call from the blocking pool.
pub fn server_origin_blocking() -> Result<String, String> {
    let is_scm = probe_service_state().0 == "scmService";
    let path = active_config_path(is_scm)?;
    let cfg = config::load(&path)
        .map_err(|_| "This device isn't enrolled yet — no server to open.".to_string())?;
    let server = cfg.server_url.trim().trim_end_matches('/').to_string();
    if !server.starts_with("https://") && !server.starts_with("http://") {
        return Err("this device's config has no valid server URL".to_string());
    }
    Ok(server)
}

/// S7 — open the Roomler web app (this device's configured server) in
/// the embedded window / browser.
#[tauri::command]
pub async fn cmd_open_roomler(app: tauri::AppHandle) -> Result<(), String> {
    let url = tokio::task::spawn_blocking(server_origin_blocking)
        .await
        .map_err(|e| format!("task join: {e}"))??;
    open_web_or_browser(&app, &url)
}

/// S2/S7 — open the remote-control viewer for one of THIS tenant's
/// agent-backed devices (`{server}/tenant/{tid}/agent/{aid}/remote`) —
/// in-app on Windows, default browser elsewhere. The URL is constructed
/// ONLY from this device's own configured server origin + hex-validated
/// ids — never from peer-supplied strings — so a hostile device name
/// can't steer the view to a foreign site.
#[tauri::command]
pub async fn cmd_open_remote(app: tauri::AppHandle, agent_id: String) -> Result<(), String> {
    let aid = agent_id.trim().to_ascii_lowercase();
    if aid.len() != 24 || !aid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid agent id".to_string());
    }
    let url = tokio::task::spawn_blocking(move || {
        let is_scm = probe_service_state().0 == "scmService";
        let path = active_config_path(is_scm)?;
        let cfg = config::load(&path).map_err(|e| format!("Loading config: {e}"))?;
        let tid = cfg.tenant_id.trim().to_ascii_lowercase();
        if tid.len() != 24 || !tid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("this device's config has no valid tenant id".to_string());
        }
        let server = cfg.server_url.trim().trim_end_matches('/').to_string();
        if !server.starts_with("https://") && !server.starts_with("http://") {
            return Err("this device's config has no valid server URL".to_string());
        }
        Ok(format!("{server}/tenant/{tid}/agent/{aid}/remote"))
    })
    .await
    .map_err(|e| format!("task join: {e}"))??;
    open_web_or_browser(&app, &url)
}

/// S2 — what [`cmd_tail_log`] returns to the log-viewer card.
#[derive(Debug, Serialize)]
pub struct LogTailReport {
    pub path: String,
    pub size: u64,
    pub content: String,
}

/// S2 — bounded tail of a daemon log (`daemon` / `service` / `panic`).
/// Daemon-verb first (role-correct paths, incl. SYSTEM-profile files
/// this app can't read); direct-file fallback covers a stopped daemon
/// for the files that ARE readable (per-user + `service-logs`).
#[tauri::command]
pub async fn cmd_tail_log(source: String, max_bytes: Option<u64>) -> Result<LogTailReport, String> {
    if let Ok(mut client) = localapi::connect().await
        && let Ok((path, size, content)) = client.tail_log(&source, max_bytes).await
    {
        return Ok(LogTailReport {
            path,
            size,
            content,
        });
    }
    tokio::task::spawn_blocking(move || {
        let path = roomler_node_core::logging::tail_source_path(&source)
            .ok_or_else(|| format!("no log file found for source {source:?}"))?;
        let cap = max_bytes.unwrap_or(32 * 1024).clamp(512, 64 * 1024);
        let (size, content) = roomler_node_core::logging::read_tail(&path, cap)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        Ok(LogTailReport {
            path: path.display().to_string(),
            size,
            content,
        })
    })
    .await
    .map_err(|e| format!("task join: {e}"))?
}

/// Update the device name. Effective on next WS reconnect — the agent
/// re-sends `rc:agent.hello` with the new name.
///
/// Prefers the RUNNING daemon's `SetDeviceName` LocalAPI verb: the daemon
/// writes ITS OWN config, which is profile-correct AND needs no elevation
/// even for a machine-global SCM install (the split-brain fix's final
/// piece — an unelevated desktop app can't write `%PROGRAMDATA%` itself).
/// Falls back to the direct file write when no daemon is listening or it
/// predates the verb.
#[tauri::command]
pub async fn cmd_set_device_name(name: String) -> Result<StatusReport, String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err("Device name is empty".to_string());
    }
    // Daemon not running ⇒ skip to the direct write (the only path left).
    if let Ok(mut client) = localapi::connect().await {
        match client.set_device_name(&trimmed).await {
            Ok(_) => {
                return tokio::task::spawn_blocking(status_report)
                    .await
                    .map_err(|e| format!("task join: {e}"));
            }
            Err(e) => {
                // Old daemon without the verb (or a daemon-side failure) —
                // fall back to the direct write so a legacy install still
                // renames; the write path reports its own honest errors.
                tracing::warn!(%e, "daemon rename verb unavailable — using direct config write");
            }
        }
    }
    tokio::task::spawn_blocking(move || set_device_name_blocking(trimmed))
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
    let machine_global = is_machine_global(&path);
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

/// Spawn `roomlerd self-update --check-only` and parse the
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
///
/// FR-27 — returns what the daemon said, instead of nothing.
///
/// On Windows the daemon spawns msiexec and exits, so there is nothing to wait
/// for and the detached spawn is right. Everywhere else the command RETURNS,
/// and on macOS what it returns is the part that matters: a non-root
/// invocation queues the root update helper (`com.roomler.update`) and prints
/// where to watch. Discarding that left the button looking like it had done
/// nothing — on the one platform where it had in fact done the right thing.
#[tauri::command]
pub fn cmd_apply_update() -> Result<String, String> {
    let exe = agent_exe_path()?;
    #[cfg(windows)]
    {
        // Detached — the daemon hands off to msiexec and exits; blocking the
        // tray's event loop on that would freeze the UI mid-install.
        no_window_command(&exe)
            .arg("self-update")
            .spawn()
            .map_err(|e| format!("Spawning self-update: {e}"))?;
        Ok("Update started. The device service will restart when it finishes.".to_string())
    }
    #[cfg(not(windows))]
    {
        let out = no_window_command(&exe)
            .arg("self-update")
            .output()
            .map_err(|e| format!("Spawning self-update ({}): {e}", exe.display()))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(if stderr.is_empty() { stdout } else { stderr });
        }
        Ok(stdout)
    }
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

/// The host permissions the agent needs, and whether the OS has granted them.
///
/// Only macOS gates these, and it does so SILENTLY: without Screen Recording
/// the remote screen is wallpaper-only, and without Accessibility every
/// injected key and click is dropped — neither reports an error to anyone.
/// That is the entire reason this panel exists.
#[derive(serde::Serialize)]
pub struct PermissionState {
    /// False on platforms with no permission model, so the UI can hide the
    /// whole panel rather than showing two permanently-green rows.
    pub applicable: bool,
    pub screen_recording: bool,
    pub accessibility: bool,
}

#[tauri::command]
pub fn cmd_permissions() -> PermissionState {
    #[cfg(target_os = "macos")]
    {
        use roomler_node_core::tcc;
        PermissionState {
            applicable: true,
            screen_recording: tcc::screen_recording_granted(),
            accessibility: tcc::accessibility_trusted(),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionState {
            applicable: false,
            screen_recording: true,
            accessibility: true,
        }
    }
}

/// Ask for a permission: pop the system prompt if it has never been answered,
/// and open the Settings pane either way.
///
/// Both are needed. The prompt only ever appears ONCE per binary — after a
/// denial macOS never shows it again — and the pane is the only route back,
/// but landing the user on the right pane with the app already listed is the
/// difference between one toggle and a hunt through Settings.
///
/// `which` is `"screen"` or `"input"`.
#[tauri::command]
pub fn cmd_request_permission(which: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use roomler_node_core::tcc;
        match which.as_str() {
            "screen" => {
                let _ = tcc::request_screen_recording();
                tcc::open_settings_pane(tcc::PANE_SCREEN_RECORDING);
                Ok(())
            }
            "input" => {
                let _ = tcc::request_accessibility();
                tcc::open_settings_pane(tcc::PANE_ACCESSIBILITY);
                Ok(())
            }
            other => Err(format!("unknown permission {other:?}")),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = which;
        Err("this platform does not gate capture or input permissions".into())
    }
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

/// FR-27 — remote-control sessions currently LIVE on this device, for the
/// "Being viewed by …" banner.
///
/// Never errors, for the same reason as [`cmd_get_pending_consents`]: an empty
/// list and a daemon-down are the same *render* (no banner), and the Overview
/// already surfaces daemon-down properly.
#[tauri::command]
pub async fn cmd_rc_sessions() -> Vec<roomler_localapi::RcSessionInfo> {
    match localapi::connect().await {
        Ok(mut c) => c.rc_sessions().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// FR-27 — the banner's Disconnect. Ends a live session from the DEVICE side;
/// the daemon closes the peer and tells the server, exactly as the Windows
/// overlay's own button does.
#[tauri::command]
pub async fn cmd_rc_disconnect(session: String) -> Result<(), String> {
    let mut client = localapi::connect()
        .await
        .map_err(|e| format!("Device service unreachable: {e}"))?;
    let ok = client
        .rc_disconnect(&session)
        .await
        .map_err(|e| format!("LocalAPI error: {e}"))?;
    if ok {
        Ok(())
    } else {
        Err("That session is no longer active.".into())
    }
}

// ─── declared routes (P6 — the Tunnels pane) ───────────────────────

/// Declared routes + live state for the Tunnels pane. NEVER errors —
/// like [`cmd_get_pending_consents`], the pane shows its own zero-state
/// when the daemon is down (an empty list is indistinguishable from
/// "no routes", and the Devices section already surfaces daemon-down).
#[tauri::command]
pub async fn cmd_route_list() -> Vec<roomler_localapi::RouteInfo> {
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
    route: roomler_localapi::RouteDescriptor,
) -> Result<roomler_localapi::RouteDescriptor, String> {
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
/// spawning the console-mode `roomlerd` pops a console each time — and
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

// RETIRED-NAME-ANCHOR(4): the fallback targets a binary the field still has; dropping
// it strands them.
/// Resolve the agent daemon's executable path. For a packaged install, the
/// tray and daemon ship in the same dir (per the MSI layout). For dev
/// builds, fall back to PATH lookup.
///
/// P3d Slice B renamed the daemon OUTPUT binary `roomlerd` -> `roomlerd`.
/// Resolution prefers a sibling `roomlerd[.exe]` (so a fresh tray spawns the
/// new daemon), then falls back to the legacy `roomler-agent[.exe]` (which the
/// MSI still ships as the inert `AgentExeAlias`, so a mixed / in-flight install
/// still resolves), then finally the bare new name relying on PATH.
// RETIRED-NAME-ANCHOR(5): the legacy pair is the fallback that lets a mixed or
// in-flight install still resolve a daemon. See docs/fr/FR-21.
fn agent_exe_path() -> Result<PathBuf, String> {
    let (new_name, old_name) = if cfg!(windows) {
        ("roomlerd.exe", "roomler-agent.exe")
    } else {
        ("roomlerd", "roomler-agent")
    };
    let mut tried: Vec<PathBuf> = Vec::new();

    // Prefer same dir as the tray (the Windows / MSI layout): new name first,
    // then the legacy alias so a mixed install still resolves.
    if let Ok(tray_exe) = std::env::current_exe()
        && let Some(dir) = tray_exe.parent()
    {
        for name in [new_name, old_name] {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
            tried.push(candidate);
        }
    }

    // FR-27 — the INSTALLED locations, which on macOS and Linux are nowhere
    // near this app.
    //
    // The sibling probe above is a Windows assumption. The macOS .pkg puts the
    // companion in `/Applications/Roomler.app/Contents/MacOS/` and the daemon
    // in `/Library/Roomler/…` with a `/usr/local/bin/roomlerd` symlink, so the
    // probe always missed — and the PATH fallback missed too, because a
    // LaunchAgent inherits launchd's minimal PATH (`/usr/bin:/bin:/usr/sbin:
    // /sbin`), which does NOT include `/usr/local/bin`. That is the whole of
    // the reported `Spawning self-update: No such file or directory (os error
    // 2)`, and with it went service status, apply-update, service
    // install/uninstall and the log-dir probe — i.e. most of the app on macOS.
    for candidate in installed_daemon_candidates() {
        if candidate.exists() {
            return Ok(candidate);
        }
        tried.push(candidate);
    }

    // Last resort: the bare name on PATH (dev runs; a distro install that put
    // roomlerd somewhere we don't know about). Only reachable when nothing
    // above existed, so name what we looked at — a bare "not found" from the
    // spawn is what made this take so long to see.
    let bare = PathBuf::from(new_name);
    tracing::warn!(
        tried = ?tried,
        "no installed roomlerd found at any known path — falling back to {new_name} on PATH"
    );
    Ok(bare)
}

/// Where a packaged daemon lives on each platform. Ordered most- to
/// least-specific; every entry is a real path from `release-agent.yml` or the
/// .deb layout, not a guess.
fn installed_daemon_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![
            // The pkg's symlink into the bundle. Preferred: every launchd
            // plist and TCC grant keys on the bundle path it points at.
            PathBuf::from("/usr/local/bin/roomlerd"),
            // The bundle itself, if the symlink is missing. Every launchd plist
            // and TCC grant keys on this path, so it moves only with a rename that
            // moves the plists too (FR-46 P5b).
            PathBuf::from("/Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd"),
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // The .deb installs to /usr/bin; a tarball install lands in
        // /usr/local/bin.
        vec![
            PathBuf::from("/usr/bin/roomlerd"),
            PathBuf::from("/usr/local/bin/roomlerd"),
        ]
    }
    #[cfg(windows)]
    {
        // Windows genuinely ships them side by side, so the sibling probe
        // above is the whole answer; PATH remains the dev fallback.
        Vec::new()
    }
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

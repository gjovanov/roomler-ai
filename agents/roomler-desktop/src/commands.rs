// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Tauri `invoke` command handlers — thin glue between the SPA's
//! JavaScript and the agent's library / CLI.
//!
//! Each #[tauri::command] returns a JSON-serialisable result; tauri
//! marshals Result<T, String> into a promise that resolves to T on
//! Ok and rejects with the String on Err. The HTML/JS layer in
//! `src/front/` consumes these via `window.__TAURI__.core.invoke`.

use roomler_localapi::{self as localapi, ConsentRequest, FlowInfo, NodeStatus, PeerInfo};
use roomler_node_core::config::{self, AgentConfig};
use roomler_node_core::enrollment::{self, EnrollInputs};
use roomler_node_core::{logging, notify};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

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
    const SURFACE: &str = "device_view";
    let mut client = match localapi::connect().await {
        Ok(c) => c,
        Err(e) => {
            let reason = if e.kind() == std::io::ErrorKind::NotFound {
                "daemon_unreachable"
            } else {
                "connect_error"
            };
            refresh_failed(SURFACE, describe_io("connect", &e));
            return DeviceView::unavailable(reason);
        }
    };
    let status = match client.status().await {
        Ok(s) => s,
        // Reached the endpoint but the exchange failed (daemon shutting down,
        // protocol error) — treat as unreachable for the UI.
        Err(e) => {
            refresh_failed(SURFACE, describe_io("status", &e));
            return DeviceView::unavailable("daemon_unreachable");
        }
    };
    // Peers are best-effort: a status-ok / peers-fail shouldn't blank the view.
    let peers = match client.peers().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %describe_io("peers", &e), "device view: peers unavailable");
            Vec::new()
        }
    };
    refresh_ok(SURFACE);
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
/// The echoed entry's `restart_required` says whether the change is live
/// or waits for the next daemon restart (FR-84 D2) — the front renders
/// that, never a list of its own.
#[tauri::command]
pub async fn cmd_config_set(
    key: String,
    value: Option<String>,
) -> Result<localapi::ConfigEntry, String> {
    let mut daemon_err: Option<String> = None;
    if let Ok(mut client) = localapi::connect().await {
        match client.config_set(&key, value.as_deref()).await {
            Ok(entry) => return Ok(entry),
            // FR-85 — where recordings go is the DAEMON's to accept: its
            // listener refuses a `record_*` change from anyone but the
            // console user. When it answered, its answer stands; writing the
            // file behind its back would report a success it refused.
            Err(e) if is_daemon_owned_key(&key) => {
                return Err(explain_recording_error(&e.to_string()));
            }
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

/// FR-84 D1 — what the Routes view renders: the declared routes AND the live
/// flows, read on ONE LocalAPI connection, or `available: false` with a
/// `reason` the page shows verbatim.
///
/// Never rejects. Before FR-84 the view issued `cmd_route_list` and
/// `cmd_flows` as two parallel calls (two more pipe opens in the same
/// millisecond as the device-view poll), and each turned ANY error into
/// `[]` — which the page painted as "no routes yet". A failure is a STATE
/// here: the page keeps its last good data and says since when and why.
#[derive(Debug, Serialize)]
pub struct TunnelsView {
    /// Both reads succeeded on one connection.
    pub available: bool,
    /// Why not, when `available` is false — the stage that failed, the io
    /// error kind and the raw OS error (231 = `ERROR_PIPE_BUSY`).
    pub reason: Option<String>,
    pub routes: Vec<roomler_localapi::RouteInfo>,
    pub flows: Vec<FlowInfo>,
}

impl TunnelsView {
    fn unavailable(reason: String) -> Self {
        Self {
            available: false,
            reason: Some(reason),
            routes: Vec::new(),
            flows: Vec::new(),
        }
    }
}

#[tauri::command]
pub async fn cmd_tunnels_view() -> TunnelsView {
    const SURFACE: &str = "tunnels";
    let mut client = match localapi::connect().await {
        Ok(c) => c,
        Err(e) => {
            return TunnelsView::unavailable(refresh_failed(SURFACE, describe_io("connect", &e)));
        }
    };
    let routes = match client.route_list().await {
        Ok(r) => r,
        Err(e) => {
            return TunnelsView::unavailable(refresh_failed(
                SURFACE,
                describe_io("route_list", &e),
            ));
        }
    };
    let flows = match client.flows().await {
        Ok(f) => f,
        Err(e) => {
            return TunnelsView::unavailable(refresh_failed(SURFACE, describe_io("flows", &e)));
        }
    };
    refresh_ok(SURFACE);
    TunnelsView {
        available: true,
        reason: None,
        routes,
        flows,
    }
}

/// FR-84 D1 — replace a declared route in ONE step (`RouteUpdate`). The
/// daemon validates the replacement exactly like an add and refuses it
/// without touching the running route; its messages surface verbatim on the
/// form. A daemon older than the verb answers "unknown variant", which is
/// rendered as "predates route editing" rather than as a stack of JSON.
#[tauri::command]
pub async fn cmd_route_update(
    route: roomler_localapi::RouteDescriptor,
) -> Result<roomler_localapi::RouteDescriptor, String> {
    let mut client = localapi::connect().await.map_err(daemon_unreachable)?;
    client
        .route_update(route)
        .await
        .map_err(|e| explain_route_edit_error(&e.to_string()))
}

/// The one reply an old daemon gives to a verb it has never heard of.
fn explain_route_edit_error(message: &str) -> String {
    if message.contains("unknown variant") {
        "The device service predates route editing — update it, then try again.".to_string()
    } else {
        message.to_string()
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

/// The shared connect-error mapping for the mutating route commands
/// (mirrors [`cmd_ping`]'s wording so the two surfaces read the same).
fn daemon_unreachable(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "device service not running".to_string()
    } else {
        format!("connecting to the device service: {e}")
    }
}

// ─── FR-85 — screen recording (the Recordings view) ────────────────

/// What the Recordings view renders: the recorder's state, the folder with
/// its recordings, and the configured `record_dir`, read on ONE LocalAPI
/// connection (the FR-84 D1 lesson), or `available: false` with a reason
/// the page shows verbatim. Never rejects.
#[derive(Debug, Serialize)]
pub struct RecordingsView {
    pub available: bool,
    pub reason: Option<String>,
    /// The device service answers, but has no recorder: it predates the
    /// recording verbs or was built without them. The view says so instead
    /// of painting an empty list.
    pub unsupported: bool,
    pub state: Option<localapi::RecordingState>,
    pub listing: Option<localapi::RecordingsListing>,
    /// `record_dir` as configured; `None` = the default folder.
    pub record_dir: Option<String>,
}

impl RecordingsView {
    fn unavailable(reason: String) -> Self {
        Self {
            available: false,
            reason: Some(reason),
            unsupported: false,
            state: None,
            listing: None,
            record_dir: None,
        }
    }

    fn unsupported() -> Self {
        Self {
            available: true,
            reason: None,
            unsupported: true,
            state: None,
            listing: None,
            record_dir: None,
        }
    }
}

/// A service without a recorder: one that has never heard of the verb, or
/// one built without it (the trait default and the daemon both say "not
/// available").
fn recording_unsupported(message: &str) -> bool {
    message.contains("unknown variant") || message.contains("not available")
}

/// The daemon's words, minus the transport prefix; an old service's
/// "unknown variant" becomes a sentence a person can act on.
fn explain_recording_error(message: &str) -> String {
    if message.contains("unknown variant") {
        return "The device service predates screen recording — update it, then try again."
            .to_string();
    }
    message
        .strip_prefix("localapi error: ")
        .unwrap_or(message)
        .to_string()
}

#[tauri::command]
pub async fn cmd_recordings_view() -> RecordingsView {
    const SURFACE: &str = "recordings";
    let mut client = match localapi::connect().await {
        Ok(c) => c,
        Err(e) => {
            return RecordingsView::unavailable(refresh_failed(
                SURFACE,
                describe_io("connect", &e),
            ));
        }
    };
    let state = match client.record_status().await {
        Ok(s) => s,
        Err(e) if recording_unsupported(&e.to_string()) => {
            refresh_ok(SURFACE);
            return RecordingsView::unsupported();
        }
        Err(e) => {
            return RecordingsView::unavailable(refresh_failed(
                SURFACE,
                describe_io("record_status", &e),
            ));
        }
    };
    let listing = match client.recordings_list().await {
        Ok(l) => l,
        Err(e) if recording_unsupported(&e.to_string()) => {
            refresh_ok(SURFACE);
            return RecordingsView::unsupported();
        }
        Err(e) => {
            return RecordingsView::unavailable(refresh_failed(
                SURFACE,
                describe_io("recordings_list", &e),
            ));
        }
    };
    // The folder row shows whether a folder was CHOSEN; the listing says
    // which one is in use (and why, when it is not the chosen one).
    let record_dir = client
        .config_entries()
        .await
        .ok()
        .and_then(|entries| entries.into_iter().find(|e| e.key == "record_dir"))
        .and_then(|e| e.value)
        .filter(|v| !v.is_empty());
    refresh_ok(SURFACE);
    RecordingsView {
        available: true,
        reason: None,
        unsupported: false,
        state: Some(state),
        listing: Some(listing),
        record_dir,
    }
}

/// Keys whose change the daemon must accept itself: a refusal it ANSWERED
/// is final, never retried as a direct file write (`cmd_config_set`).
fn is_daemon_owned_key(key: &str) -> bool {
    key.starts_with("record_")
}

/// Start recording this device's screen. Resolves once the recorder is
/// encoding, with the file and encoder named; rejects with the daemon's
/// reason otherwise (not the console user, a SYSTEM/root service, no
/// encoder, no disk).
#[tauri::command]
pub async fn cmd_record_start(
    fps: Option<u32>,
    encoder: Option<String>,
) -> Result<localapi::RecordingState, String> {
    let mut client = localapi::connect().await.map_err(daemon_unreachable)?;
    client
        .record_start(localapi::RecordStartOpts {
            fps,
            encoder,
            ..Default::default()
        })
        .await
        .map_err(|e| explain_recording_error(&e.to_string()))
}

/// Stop the recording. Resolves once the file is final.
#[tauri::command]
pub async fn cmd_record_stop() -> Result<localapi::RecordingState, String> {
    let mut client = localapi::connect().await.map_err(daemon_unreachable)?;
    client
        .record_stop()
        .await
        .map_err(|e| explain_recording_error(&e.to_string()))
}

/// Delete a recording (and its sidecar) by file name.
#[tauri::command]
pub async fn cmd_recording_delete(name: String) -> Result<(), String> {
    let mut client = localapi::connect().await.map_err(daemon_unreachable)?;
    client
        .recording_delete(&name)
        .await
        .map_err(|e| explain_recording_error(&e.to_string()))
}

/// A recording's file name from the page: a bare `*.mp4` name, nothing
/// that could leave the folder. The daemon checks the same rule before a
/// delete; opening a file is the companion's own act, so it checks here.
fn check_recording_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.contains(['/', '\\', ':'])
        || name.contains("..")
        || !name.to_ascii_lowercase().ends_with(".mp4")
    {
        return Err(format!("{name:?} is not a recording's file name"));
    }
    Ok(())
}

/// Open the recordings folder (`name` = None), play a recording in the OS's
/// default player, or (`reveal`) show it selected in the file manager. The
/// path is always the DAEMON's folder joined with a checked bare name —
/// never a path the page supplies.
#[tauri::command]
pub async fn cmd_recording_open(name: Option<String>, reveal: bool) -> Result<(), String> {
    if let Some(n) = &name {
        check_recording_name(n)?;
    }
    let mut client = localapi::connect().await.map_err(daemon_unreachable)?;
    let listing = client
        .recordings_list()
        .await
        .map_err(|e| explain_recording_error(&e.to_string()))?;
    let dir = PathBuf::from(listing.dir);
    tokio::task::spawn_blocking(move || {
        let Some(name) = name else {
            return open_path_in_explorer(&dir);
        };
        let path = dir.join(name);
        let meta =
            std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !meta.file_type().is_file() {
            return Err(format!("{} is not a recording file", path.display()));
        }
        if reveal {
            reveal_in_file_manager(&path)
        } else {
            open_path_in_explorer(&path)
        }
    })
    .await
    .map_err(|e| format!("task join: {e}"))?
}

/// Show `path` selected in the OS file manager (Linux has no portable
/// "select": its folder opens instead).
fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // Recording names contain spaces, and explorer parses `/select,`
        // itself: the path must be quoted INSIDE the argument, which the
        // standard quoting (the whole argument in quotes) breaks.
        Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()
            .map_err(|e| format!("explorer.exe: {e}"))?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn()
            .map_err(|e| format!("open -R: {e}"))?;
        Ok(())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let dir = path.parent().unwrap_or(path);
        open_path_in_explorer(dir)
    }
}

/// The native folder picker for `record_dir`. Resolves to the chosen folder,
/// or `None` when the person cancelled; the page then saves it through
/// `cmd_config_set`, where the daemon validates it.
#[tauri::command]
pub async fn cmd_pick_record_dir(
    app: tauri::AppHandle,
    current: Option<String>,
) -> Result<Option<String>, String> {
    use tauri::Manager as _;
    use tauri_plugin_dialog::DialogExt as _;
    tokio::task::spawn_blocking(move || {
        let mut dialog = app
            .dialog()
            .file()
            .set_title("Where Roomler saves screen recordings");
        if let Some(dir) = current.as_deref().filter(|d| Path::new(d).is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(window) = app.get_webview_window("main") {
            dialog = dialog.set_parent(&window);
        }
        Ok(dialog
            .blocking_pick_folder()
            .and_then(|p| p.into_path().ok())
            .map(|p| p.to_string_lossy().into_owned()))
    })
    .await
    .map_err(|e| format!("task join: {e}"))?
}

// ─── refresh failures: said once, on the page and in the log ───────

/// One line naming WHAT failed (`stage`) and HOW — the io error kind and,
/// when the OS supplied one, its raw code (`231` = `ERROR_PIPE_BUSY`, the one
/// FR-84 chased). `NotFound` on connect is the daemon not running, said
/// plainly.
fn describe_io(stage: &str, e: &std::io::Error) -> String {
    if stage == "connect" && e.kind() == std::io::ErrorKind::NotFound {
        return "device service not running (no LocalAPI endpoint)".to_string();
    }
    let text = e.to_string();
    match e.raw_os_error() {
        Some(code) => {
            // std's Display already ends an OS error with "(os error N)";
            // keep the code once, after the kind.
            let suffix = format!(" (os error {code})");
            let text = text.strip_suffix(suffix.as_str()).unwrap_or(&text);
            format!("{stage}: {text} [{:?}, os error {code}]", e.kind())
        }
        None => format!("{stage}: {text} [{:?}]", e.kind()),
    }
}

/// How the refresh-failure ledger wants a failure reported.
#[derive(Debug, PartialEq, Eq)]
enum FailureLog {
    /// Write a warn line: the first failure of a streak, a changed reason,
    /// or the periodic summary (`suppressed` = lines withheld since the
    /// last one).
    Warn { failures: u32, suppressed: u32 },
    /// Same reason, inside the quiet window — count it, write nothing.
    Quiet,
}

struct FailureStreak {
    failures: u32,
    last_reason: String,
    last_logged: Instant,
    suppressed: u32,
}

/// FR-84 D1 — the ledger behind the desktop log's refresh lines: one warn
/// when a surface starts failing, another at once when the reason changes,
/// a summary at most every [`Self::QUIET`] while it keeps failing, and one
/// info line with the count when it recovers. Pure (takes `now`), so the
/// policy is testable; the statics below wrap it.
///
/// Without it every swallowed error was invisible — the page blanked and
/// nothing anywhere said why — and without the rate limit a 2 s poller
/// failing for a day would write 43 000 identical lines.
#[derive(Default)]
struct RefreshLedger {
    streaks: HashMap<&'static str, FailureStreak>,
}

impl RefreshLedger {
    /// Minimum gap between two lines for the same unchanged reason.
    const QUIET: Duration = Duration::from_secs(30);

    fn failed(&mut self, surface: &'static str, reason: &str, now: Instant) -> FailureLog {
        match self.streaks.get_mut(surface) {
            None => {
                self.streaks.insert(
                    surface,
                    FailureStreak {
                        failures: 1,
                        last_reason: reason.to_string(),
                        last_logged: now,
                        suppressed: 0,
                    },
                );
                FailureLog::Warn {
                    failures: 1,
                    suppressed: 0,
                }
            }
            Some(s) => {
                s.failures += 1;
                let changed = s.last_reason != reason;
                if changed || now.duration_since(s.last_logged) >= Self::QUIET {
                    let suppressed = s.suppressed;
                    s.suppressed = 0;
                    s.last_logged = now;
                    s.last_reason = reason.to_string();
                    FailureLog::Warn {
                        failures: s.failures,
                        suppressed,
                    }
                } else {
                    s.suppressed += 1;
                    FailureLog::Quiet
                }
            }
        }
    }

    /// A success ends the streak; returns how many failures it had, if any.
    fn recovered(&mut self, surface: &'static str) -> Option<u32> {
        self.streaks.remove(surface).map(|s| s.failures)
    }
}

static REFRESH_LEDGER: LazyLock<Mutex<RefreshLedger>> =
    LazyLock::new(|| Mutex::new(RefreshLedger::default()));

/// Record a failed refresh of `surface` and log it per the ledger's policy.
/// Returns `reason` so a caller can hand it straight to the page.
fn refresh_failed(surface: &'static str, reason: String) -> String {
    let action = REFRESH_LEDGER
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .failed(surface, &reason, Instant::now());
    if let FailureLog::Warn {
        failures,
        suppressed,
    } = action
    {
        tracing::warn!(surface, failures, suppressed, %reason, "refresh failed");
    }
    reason
}

/// Record a successful refresh of `surface`; logs the recovery once if it
/// ends a failure streak.
fn refresh_ok(surface: &'static str) {
    let ended = REFRESH_LEDGER
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .recovered(surface);
    if let Some(failures) = ended {
        tracing::info!(surface, failures, "refresh recovered");
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

    /// FR-85 — the page hands the companion a NAME to open, never a path.
    #[test]
    fn only_a_bare_mp4_name_is_opened() {
        assert!(check_recording_name("Roomler Recording 2026-09-25 14-30-12.mp4").is_ok());
        assert!(check_recording_name("x.MP4").is_ok());
        for bad in [
            "",
            "../x.mp4",
            "sub/x.mp4",
            r"sub\x.mp4",
            "C:x.mp4",
            "x.mp4.roomler.json",
            "notes.txt",
        ] {
            assert!(check_recording_name(bad).is_err(), "{bad}");
        }
    }

    /// FR-85 — a service without a recorder is "unsupported" (said on the
    /// page), not a failure (retried and logged as one).
    #[test]
    fn a_service_without_the_recorder_reads_as_unsupported() {
        assert!(recording_unsupported(
            "localapi: unexpected response: Error { message: \"bad request: unknown variant `record_status`\" }"
        ));
        assert!(recording_unsupported(
            "localapi error: screen recording is not available in this build"
        ));
        assert!(!recording_unsupported(
            "localapi error: a recording is already running"
        ));
        assert_eq!(
            explain_recording_error("localapi error: a recording is already running"),
            "a recording is already running"
        );
        assert!(explain_recording_error("… unknown variant `record_start` …").contains("predates"));
    }

    /// FR-85 — a refusal the daemon ANSWERED for a recording key is final.
    #[test]
    fn recording_keys_are_the_daemons_to_accept() {
        assert!(is_daemon_owned_key("record_dir"));
        assert!(!is_daemon_owned_key("exec_enabled"));
        assert!(!is_daemon_owned_key("files_dir"));
    }

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

    /// FR-84 D1 — the refresh-failure ledger: first failure logs, identical
    /// repeats inside the quiet window do not, a changed reason logs at
    /// once, the periodic summary carries the suppressed count, and a
    /// recovery reports the streak length exactly once.
    #[test]
    fn refresh_ledger_rate_limits_repeats_and_reports_recovery() {
        let mut ledger = RefreshLedger::default();
        let t0 = Instant::now();
        assert_eq!(
            ledger.failed("tunnels", "connect: busy", t0),
            FailureLog::Warn {
                failures: 1,
                suppressed: 0
            },
            "the first failure of a streak is always logged"
        );
        for i in 1..=5 {
            assert_eq!(
                ledger.failed("tunnels", "connect: busy", t0 + Duration::from_secs(i * 2)),
                FailureLog::Quiet,
                "an identical reason inside the quiet window is counted, not logged"
            );
        }
        assert_eq!(
            ledger.failed("tunnels", "route_list: eof", t0 + Duration::from_secs(12)),
            FailureLog::Warn {
                failures: 7,
                suppressed: 5
            },
            "a changed reason is logged at once, with the lines it superseded"
        );
        assert_eq!(
            ledger.failed("tunnels", "route_list: eof", t0 + Duration::from_secs(14)),
            FailureLog::Quiet
        );
        assert_eq!(
            ledger.failed(
                "tunnels",
                "route_list: eof",
                t0 + Duration::from_secs(12) + RefreshLedger::QUIET
            ),
            FailureLog::Warn {
                failures: 9,
                suppressed: 1
            },
            "the periodic summary fires once the quiet window has elapsed"
        );
        // Surfaces are independent streaks.
        assert_eq!(
            ledger.failed("device_view", "connect: busy", t0),
            FailureLog::Warn {
                failures: 1,
                suppressed: 0
            }
        );
        assert_eq!(ledger.recovered("tunnels"), Some(9));
        assert_eq!(
            ledger.recovered("tunnels"),
            None,
            "a recovery is reported once; steady success is silent"
        );
        assert_eq!(
            ledger.failed("tunnels", "connect: busy", t0 + Duration::from_secs(60)),
            FailureLog::Warn {
                failures: 1,
                suppressed: 0
            },
            "after a recovery the next failure starts a new streak"
        );
    }

    /// The reason the page shows names the stage, the kind and the raw OS
    /// code once — and says "not running" plainly for an absent endpoint.
    #[test]
    fn describe_io_names_stage_kind_and_os_code_once() {
        let busy = std::io::Error::from_raw_os_error(231);
        let s = describe_io("connect", &busy);
        assert!(s.starts_with("connect: "), "{s}");
        assert_eq!(
            s.matches("os error").count(),
            1,
            "std's own '(os error N)' suffix is folded into ours: {s}"
        );
        assert!(s.ends_with(", os error 231]"), "{s}");

        let gone = std::io::Error::new(std::io::ErrorKind::NotFound, "no pipe");
        assert_eq!(
            describe_io("connect", &gone),
            "device service not running (no LocalAPI endpoint)"
        );
        let eof = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed early");
        assert_eq!(
            describe_io("route_list", &eof),
            "route_list: closed early [UnexpectedEof]"
        );
    }

    #[test]
    fn old_daemon_reply_is_explained() {
        let old = explain_route_edit_error(
            "localapi: unexpected response: Error { message: \"bad request: unknown variant `route_update`, expected one of …\" }",
        );
        assert!(old.contains("predates route editing"), "{old}");
        assert_eq!(
            explain_route_edit_error("no declared route with id 'x'"),
            "no declared route with id 'x'",
            "a real daemon refusal surfaces verbatim"
        );
    }
}

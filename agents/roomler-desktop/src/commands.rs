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
    /// `"scmService"` (Windows' SCM service) · `"scheduledTask"` (a per-user
    /// Scheduled Task, or a Mac's LaunchAgent) · `"systemDaemon"` (#2871: a
    /// Linux `roomlerd.service` or a Mac's `com.roomler.daemon` root daemon)
    /// · `"none"`.
    pub service_kind: String,
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
    let mut report = tokio::task::spawn_blocking(status_report)
        .await
        .unwrap_or_else(|_| status_report());
    // #1701 — the file said "not enrolled"; ask the daemon before believing it.
    if !report.enrolled
        && let Some(id) = daemon_node_status()
            .await
            .as_ref()
            .and_then(daemon_identity)
    {
        apply_daemon_identity(&mut report, id);
    }
    report
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

/// What the daemon says about its own enrollment (#1701).
#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonIdentity {
    agent_id: String,
    tenant_id: String,
    server_url: Option<String>,
    device_name: String,
}

/// An enrolled daemon names its node and its tenant — it only serves the
/// LocalAPI once it has loaded an enrolled config — so both must be present.
/// The primary org carries the agent id and server the file would have.
fn daemon_identity(s: &NodeStatus) -> Option<DaemonIdentity> {
    let tenant_id = s.tenant_id.clone().filter(|t| !t.is_empty())?;
    if s.node_id.is_empty() {
        return None;
    }
    let primary = s.orgs.iter().find(|o| o.primary);
    Some(DaemonIdentity {
        agent_id: primary
            .and_then(|o| o.agent_id.clone())
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| s.node_id.clone()),
        tenant_id,
        server_url: primary
            .map(|o| o.server_url.clone())
            .filter(|u| !u.is_empty()),
        device_name: s.name.clone(),
    })
}

/// Fill a report the config FILE could not answer with the daemon's word.
///
/// ⚠️ #1701: on a SystemContext install the config sits in
/// `%PROGRAMDATA%\roomler\roomler\`, which W4(c) restricts to SYSTEM +
/// Administrators — rightly, it holds the agent token — so the companion,
/// the signed-in user on a filtered token, gets access denied, and
/// `enrolled` read `false` on an enrolled, connected device: the Welcome
/// said "Not enrolled yet" and disabled Enable. The file stays the first
/// source (the only one while the daemon is down); the daemon never
/// overrides it.
fn apply_daemon_identity(report: &mut StatusReport, id: DaemonIdentity) {
    if report.enrolled {
        return;
    }
    report.enrolled = true;
    report.agent_id = Some(id.agent_id);
    report.tenant_id = Some(id.tenant_id);
    report.server_url = id.server_url;
    report.device_name = Some(id.device_name);
}

/// The daemon's `status`, best-effort: a daemon that is down simply leaves
/// the file's answer standing.
async fn daemon_node_status() -> Option<NodeStatus> {
    let mut client = localapi::connect().await.ok()?;
    client.status().await.ok()
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
        // FR-102 P3 — a person pasted a token into the companion's form: the
        // package got here some other way, and this enroll is by hand.
        install: Some(enrollment::InstallInfo::new(
            enrollment::InstallMethod::Manual,
            enrollment::InstallScope::Unknown,
        )),
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
    let fresh = enrollment::enroll(EnrollInputs {
        server_url: &existing.server_url,
        enrollment_token: &trimmed,
        machine_id: &existing.machine_id,
        machine_name: &existing.machine_name,
        // A token refresh says nothing about how the binary got here; the
        // server keeps the row's recorded install.
        install: None,
    })
    .await
    .map_err(|e| format!("Re-enrollment failed: {e:#}"))?;
    // As `roomlerd re-enroll`: a token for another org is an ADD, which is
    // the Enroll flow's job, not a refresh of this one.
    if fresh.tenant_id != existing.tenant_id {
        return Err(
            "That enrollment token belongs to a different organization. Use Enroll to add \
             this device to another organization."
                .to_string(),
        );
    }
    // FR-103 — fold the fresh identity into the EXISTING config, as
    // `roomlerd re-enroll` does. Saving the fresh, default-everything config
    // wholesale dropped every operator setting and every `[[orgs]]`
    // enrollment on the machine.
    let (merged, _outcome) = enrollment::apply_enrollment(Some(existing), fresh, None, false)
        .map_err(|e| format!("Re-enrollment failed: {e:#}"))?;
    config::save(&path, &merged).map_err(|e| explain_save_error(e, &path, machine_global))?;
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

/// #2871 — what [`cmd_config_entries`] answers: the rows, and whether a
/// two-half Mac's root half contributed its machine-side rows over the person
/// socket it serves this account ([`merge_two_halves`]), so the page's banner
/// can say what changes here and what an administrator changes.
#[derive(Debug, Serialize, Default)]
pub struct ConfigListing {
    pub entries: Vec<localapi::ConfigEntry>,
    pub machine_merged: bool,
}

/// S2 — the editable config surface (key + current value + editor
/// metadata). Daemon-verb first (the daemon reads its OWN config, which
/// is profile-correct under SCM/SystemContext); direct-file fallback
/// reads the tray's active config so the editor still renders when the
/// daemon is down.
///
/// #2871 (design A) — on a two-half Mac (FR-43 through P2) the daemon
/// `connect()` reaches is the per-user half, whose listing is the SESSION's
/// truth; the machine-side rows (`config_surface::owner_for`) are taken from
/// the root half over the person socket it serves this account, which lists
/// every key, marks the ones it will not set `read_only`
/// (`localapi::person_view`) and sets the ones that are the person's own
/// call. On a Mac with a root half alone (#2891) that socket serves the whole
/// listing. A root half older than design A refuses the listing; the per-user
/// rows then stand alone, as before, and the banner says so.
#[tauri::command]
pub async fn cmd_config_entries() -> Result<ConfigListing, String> {
    match localapi::connect().await {
        Ok(mut client) => match client.config_entries().await {
            Ok(entries) => {
                return Ok(match machine_half_rows(&mut client).await {
                    Some(machine) => ConfigListing {
                        entries: merge_two_halves(entries, machine),
                        machine_merged: true,
                    },
                    None => ConfigListing {
                        entries,
                        machine_merged: false,
                    },
                });
            }
            // #2891 — a refusal from a ROOT daemon this unprivileged app
            // reached (a Linux system install's person socket, from a daemon
            // older than design A) is an answer, not an outage: the file the
            // fallback below reads is not that daemon's — a per-user path a
            // system install does not have, or a stale per-user copy no daemon
            // reads — so the daemon's words stand, and the page explains them
            // (`cmd_config_target`).
            Err(e) => {
                if refused_by_root_daemon(&mut client).await {
                    return Err(daemon_message(&e.to_string()));
                }
            }
        },
        // #2871 — nothing per-user answers: a Mac with a root half alone
        // (#2891) serves this person its person socket, and the listing over
        // it. A refusal there is an older root half's, and final: the per-user
        // file below is not its.
        Err(_) => {
            if let Some(answer) = root_half_alone_entries().await {
                return answer.map(|entries| ConfigListing {
                    entries,
                    machine_merged: false,
                });
            }
        }
    }
    tokio::task::spawn_blocking(|| {
        let is_scm = probe_service_state().0 == "scmService";
        let path = active_config_path(is_scm)?;
        let cfg = config::load(&path).map_err(|e| format!("Loading config: {e}"))?;
        Ok(ConfigListing {
            entries: roomler_node_core::config_surface::entries(&cfg),
            machine_merged: false,
        })
    })
    .await
    .map_err(|e| format!("task join: {e}"))?
}

/// #2871 — is `key` the root half's on a two-half Mac
/// (`config_surface::owner_for`)? By this build's registry — the companion
/// and the daemon move together (FR-117) — else by `group`, the group the
/// daemon listed the row with, else the session half's, as the page always
/// treated every key. ONE decision for both directions (#2871's review): the
/// listing takes a row from the half this names ([`merge_two_halves`]), and
/// [`cmd_config_set`] sends its change to the same half, by the same group —
/// a key a newer root half lists that this build does not know would
/// otherwise be shown from the root half and written to the per-user file,
/// #2871's own failure.
fn is_machine_side(key: &str, group: &str) -> bool {
    use roomler_node_core::config_surface::{Owner, owner_for};
    owner_for(key, group) == Owner::Machine
}

/// #2871 — pure: a two-half Mac's rows. The per-user (session) half's
/// listing, with every MACHINE-side key's row taken from the root half's —
/// which carries the root half's value and its `read_only` mark — in the
/// per-user listing's order; a machine-side key only the root half lists is
/// appended, in the root half's order; a session-side key only the root half
/// lists is dropped, because the session half's listing is the session's
/// truth. Which side a key is on: `config_surface::owner_for`, by the key,
/// else by the group the daemon sent with it.
fn merge_two_halves(
    reached: Vec<localapi::ConfigEntry>,
    machine: Vec<localapi::ConfigEntry>,
) -> Vec<localapi::ConfigEntry> {
    use std::collections::{HashMap, HashSet};
    let machine_side = |e: &localapi::ConfigEntry| is_machine_side(&e.key, &e.group);
    let by_key: HashMap<&str, &localapi::ConfigEntry> = machine
        .iter()
        .filter(|e| machine_side(e))
        .map(|e| (e.key.as_str(), e))
        .collect();
    let reached_keys: HashSet<String> = reached.iter().map(|e| e.key.clone()).collect();
    let mut out: Vec<localapi::ConfigEntry> = reached
        .into_iter()
        .map(|e| {
            if machine_side(&e) {
                by_key
                    .get(e.key.as_str())
                    .map(|m| (*m).clone())
                    .unwrap_or(e)
            } else {
                e
            }
        })
        .collect();
    out.extend(
        machine
            .iter()
            .filter(|e| machine_side(e) && !reached_keys.contains(&e.key))
            .cloned(),
    );
    out
}

/// How long the root half has for an answer over its person socket: as the
/// per-user half's own reads of it (FR-97 P2h). A slow root half leaves the
/// per-user rows standing for this load.
#[cfg(target_os = "macos")]
const ROOT_HALF_READ: Duration = Duration::from_millis(1500);

/// …and for a `ConfigSet`, which the person clicked for: long enough for a
/// config load → validate → save on the root half, short enough that a
/// wedged root half answers the click with an error, not a hang.
#[cfg(target_os = "macos")]
const ROOT_HALF_WRITE: Duration = Duration::from_secs(5);

/// #2871 — a client on the root half's person socket for this account
/// (`/var/run/roomler-person/<uid>.sock`, FR-97 P2g), kept only when ROOT
/// serves it (`connect_unix_served_by_root`). `None` when no root half serves
/// this person one.
#[cfg(target_os = "macos")]
async fn root_half_client() -> Option<localapi::Client> {
    let path = localapi::own_person_socket_path()?;
    if !path.exists() {
        return None;
    }
    tokio::time::timeout(ROOT_HALF_READ, localapi::connect_unix_served_by_root(path))
        .await
        .ok()?
        .ok()
}

/// #2871 — the root half's listing, when `reached` is the per-user half of a
/// two-half Mac ([`two_halves`], from both halves' `Status`) and the root half
/// serves it over its person socket (design A). `None` keeps the per-user rows
/// alone: no root half, a root half older than design A (it refuses the
/// listing), or one that does not answer in time.
#[cfg(target_os = "macos")]
async fn machine_half_rows(reached: &mut localapi::Client) -> Option<Vec<localapi::ConfigEntry>> {
    let mut root = root_half_client().await?;
    let ask = async {
        let reached_brief = DaemonBrief::from(&reached.status().await.ok()?);
        let machine_brief = DaemonBrief::from(&root.status().await.ok()?);
        if !two_halves(&reached_brief, Some(&machine_brief)) {
            return None;
        }
        root.config_entries().await.ok()
    };
    tokio::time::timeout(ROOT_HALF_READ, ask)
        .await
        .ok()
        .flatten()
}

/// Off macOS one daemon lists every key: Windows runs one, and a Linux
/// system install's companion reaches the root daemon itself (its person
/// socket is the first path `connect()` tries), whose listing arrives marked.
#[cfg(not(target_os = "macos"))]
async fn machine_half_rows(_reached: &mut localapi::Client) -> Option<Vec<localapi::ConfigEntry>> {
    None
}

/// #2871 — a Mac with a root half alone (#2891): the listing over its person
/// socket. `Some(Err)` is that half's refusal — a root half older than design
/// A — and final; `None` = no root half serves this person one, or it did not
/// answer in time.
#[cfg(target_os = "macos")]
async fn root_half_alone_entries() -> Option<Result<Vec<localapi::ConfigEntry>, String>> {
    let mut root = root_half_client().await?;
    match tokio::time::timeout(ROOT_HALF_READ, root.config_entries()).await {
        Ok(Ok(entries)) => Some(Ok(entries)),
        Ok(Err(e)) => Some(Err(daemon_message(&e.to_string()))),
        Err(_) => None,
    }
}

#[cfg(not(target_os = "macos"))]
async fn root_half_alone_entries() -> Option<Result<Vec<localapi::ConfigEntry>, String>> {
    None
}

/// #2871 — on a two-half Mac, a `ConfigSet` of a machine-side key, answered
/// by the root half over its person socket: it sets the keys that are the
/// person's own call (`external_*`) and refuses the rest by name with the
/// administrator's command. Either answer is FINAL — the per-user half's copy
/// is a file the Mac's device never reads. `None` = not that layout: the
/// caller's own path.
#[cfg(target_os = "macos")]
async fn root_half_config_set_beside(
    reached: &mut localapi::Client,
    key: &str,
    value: Option<&str>,
) -> Option<Result<localapi::ConfigEntry, String>> {
    let mut root = root_half_client().await?;
    let reached_brief = DaemonBrief::from(&reached.status().await.ok()?);
    let machine_brief = DaemonBrief::from(&root.status().await.ok()?);
    if !two_halves(&reached_brief, Some(&machine_brief)) {
        return None;
    }
    Some(root_half_set(&mut root, key, value).await)
}

#[cfg(not(target_os = "macos"))]
async fn root_half_config_set_beside(
    _reached: &mut localapi::Client,
    _key: &str,
    _value: Option<&str>,
) -> Option<Result<localapi::ConfigEntry, String>> {
    None
}

/// #2871 — a Mac with a root half alone (#2891): a `ConfigSet` over its
/// person socket, for any key; its answer is final. `None` = no root half
/// serves this person one.
#[cfg(target_os = "macos")]
async fn root_half_config_set_alone(
    key: &str,
    value: Option<&str>,
) -> Option<Result<localapi::ConfigEntry, String>> {
    let mut root = root_half_client().await?;
    Some(root_half_set(&mut root, key, value).await)
}

#[cfg(not(target_os = "macos"))]
async fn root_half_config_set_alone(
    _key: &str,
    _value: Option<&str>,
) -> Option<Result<localapi::ConfigEntry, String>> {
    None
}

/// #2871 — the `ConfigSet` itself, bounded: the root half's answer in its
/// own words, or a timeout the person can read.
#[cfg(target_os = "macos")]
async fn root_half_set(
    root: &mut localapi::Client,
    key: &str,
    value: Option<&str>,
) -> Result<localapi::ConfigEntry, String> {
    match tokio::time::timeout(ROOT_HALF_WRITE, root.config_set(key, value)).await {
        Ok(Ok(entry)) => Ok(entry),
        Ok(Err(e)) => Err(daemon_message(&e.to_string())),
        Err(_) => Err(format!(
            "the root daemon did not answer within {} s; nothing was changed here",
            ROOT_HALF_WRITE.as_secs()
        )),
    }
}

/// S2 — set (`value` present) or clear (`value` null) one editable
/// config key. Daemon-verb first, same rationale as
/// [`cmd_set_device_name`]; the direct-file fallback runs the SAME
/// per-key validator (`config_surface::apply`), so a validation error
/// reads identically on both paths and nothing skips validation.
/// The echoed entry's `restart_required` says whether the change is live
/// or waits for the next daemon restart (FR-84 D2) — the front renders
/// that, never a list of its own.
///
/// `group` is the group the row was LISTED with (`ConfigEntry::group`;
/// the Settings page sends it, the pages that set a key of this build's
/// own do not need to), so a change goes to the half its row came from
/// ([`is_machine_side`]).
#[tauri::command]
pub async fn cmd_config_set(
    key: String,
    value: Option<String>,
    group: Option<String>,
) -> Result<localapi::ConfigEntry, String> {
    let mut daemon_err: Option<String> = None;
    match localapi::connect().await {
        Ok(mut client) => {
            // #2871 (design A) — on a two-half Mac a machine-side key is the
            // root half's (`is_machine_side`, by the group its row was listed
            // with): it goes to the root half over the person socket, whose
            // answer is final — set, or refused by name with the
            // administrator's command. The per-user half's copy of such a key
            // is a file the Mac's device never reads (#2871's failure), so it
            // is not written.
            if is_machine_side(&key, group.as_deref().unwrap_or(""))
                && let Some(answer) =
                    root_half_config_set_beside(&mut client, &key, value.as_deref()).await
            {
                return answer;
            }
            match client.config_set(&key, value.as_deref()).await {
                Ok(entry) => return Ok(entry),
                // FR-85 — where recordings go is the DAEMON's to accept: its
                // listener refuses a `record_*` change from anyone but the
                // console user. When it answered, its answer stands; writing
                // the file behind its back would report a success it refused.
                // FR-84 D4 — the same for `files_dir` (see
                // `is_daemon_owned_key`).
                Err(e) if is_daemon_owned_key(&key) => {
                    return Err(explain_daemon_owned_error(&key, &e.to_string()));
                }
                Err(e) => {
                    // #2891 — the same for EVERY key when a ROOT daemon
                    // refused this unprivileged app (a Linux system install's
                    // person socket): the per-user file below is not its, and
                    // writing it would report a success the daemon refused.
                    if refused_by_root_daemon(&mut client).await {
                        return Err(daemon_message(&e.to_string()));
                    }
                    daemon_err = Some(e.to_string());
                }
            }
        }
        // #2871 — nothing per-user answers: a Mac with a root half alone
        // (#2891) takes the change over its person socket, for any key, and
        // its answer is final.
        Err(_) => {
            if let Some(answer) = root_half_config_set_alone(&key, value.as_deref()).await {
                return answer;
            }
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

// ─── #2871 — which daemon the Device configuration page edits ──────────

/// #2871 — the daemon the Device configuration page reads and writes, as it
/// names itself, and, on a Mac running both halves (FR-43), the ROOT half it
/// does NOT edit. Returned from [`cmd_config_target`]. Never rejects: with no
/// daemon reachable `reached` is empty, and the page already says so.
///
/// Why the page needs this at all: `localapi::connect()` prefers the per-user
/// socket, so on a supervised Mac the companion edits the GUI worker's own
/// `config.toml` — a second enrollment with a config of its own (FR-43 keeps
/// it through P2) — while the Overview shows the ROOT half's overlay address
/// (FR-97 P2h lends it). A person flipping `ssh_enabled` or `exec_enabled`
/// here changed a file the mesh node never reads, and nothing said so
/// (measured on 0.4.169: the worker's file read `ssh_enabled = false` while
/// the root half served SSH).
#[derive(Debug, Serialize, Default)]
pub struct ConfigTarget {
    /// The daemon [`cmd_config_entries`] / [`cmd_config_set`] reach — the one
    /// `localapi::connect()` resolves, which on a Mac is the per-user half.
    pub reached: Option<DaemonBrief>,
    /// macOS only: the root half, read over the person socket it serves this
    /// account. `Status` is on that socket's allowlist; since design A
    /// (#2871) so are the listing and `ConfigSet` of the keys that are the
    /// person's own call (`localapi::person_may`), so [`cmd_config_entries`]
    /// takes the machine-side rows from it and [`cmd_config_set`] sends them
    /// there. `None` on every other OS, when the reached daemon is itself
    /// privileged, and when no root half answers. Asked for beside a
    /// per-user daemon (a two-half Mac) and when nothing was reached (#2891:
    /// a Mac with a root half alone).
    pub machine: Option<DaemonBrief>,
    /// `reached` is an unprivileged daemon and `machine` a root one of another
    /// identity: the page shows both halves and must say so ([`two_halves`]).
    pub two_halves: bool,
    /// #2891 — the daemon whose configuration this page shows runs as root
    /// and this app does not: a Linux system install's root daemon, reached
    /// over its person socket, or a Mac with a root half alone
    /// ([`root_daemon_only`]). Since design A that socket serves the listing,
    /// marked per key; a daemon older than it refuses, and the page shows the
    /// administrator's path in place of rows. Never on Windows, whose pipe
    /// admits every interactive user to those verbs.
    pub root_only: bool,
    /// What the banner says when `two_halves` ([`two_halves_note`]) or
    /// `root_only` ([`root_only_note`]) and the root daemon served its rows:
    /// what changes here, what an administrator changes.
    pub note: Option<String>,
    /// What it says instead when the root daemon predates design A and
    /// refused its listing ([`two_halves_legacy_note`],
    /// [`root_only_legacy_note`]): the rows are the per-user half's alone, or
    /// there are none, and the root daemon's switches are an administrator's.
    pub legacy_note: Option<String>,
}

/// How a daemon names itself for [`ConfigTarget`]: the fields of its `Status`
/// the page renders. No token, no address.
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct DaemonBrief {
    pub name: String,
    pub node_id: String,
    pub version: String,
    /// The config file it loaded (S1b); `None` from an older daemon.
    pub config_path: Option<String>,
    /// `Some(true)` = root or SYSTEM, `Some(false)` = the signed-in user,
    /// `None` = a daemon too old to say.
    pub privileged: Option<bool>,
}

impl From<&NodeStatus> for DaemonBrief {
    fn from(s: &NodeStatus) -> Self {
        Self {
            name: s.name.clone(),
            node_id: s.node_id.clone(),
            version: s.version.clone(),
            config_path: s.config_path.clone(),
            privileged: s.privileged,
        }
    }
}

/// #2871 — pure: is this the two-half Mac the banner is for? The reached
/// daemon must SAY it runs unprivileged (an older daemon says nothing, and
/// nothing is claimed for it), and the machine daemon must be root AND another
/// node: a root daemon's legacy per-user bind answers for the SAME process
/// (`localapi::serve_unix`), and one daemon is not two halves.
fn two_halves(reached: &DaemonBrief, machine: Option<&DaemonBrief>) -> bool {
    reached.privileged == Some(false)
        && machine.is_some_and(|m| m.privileged == Some(true) && m.node_id != reached.node_id)
}

/// The file a daemon names for itself, or a stand-in from an older one.
fn config_file_of(d: &DaemonBrief) -> String {
    d.config_path
        .clone()
        .unwrap_or_else(|| "its own config.toml".to_string())
}

/// #2871 (design A) — the banner's text on a two-half Mac whose root half
/// serves its listing over the person socket: which half each kind of row
/// comes from and goes to ([`merge_two_halves`]), both files, so a person can
/// tell which one a `roomler config ls` printed, and the administrator's
/// command for the rows marked.
fn two_halves_note(reached: &DaemonBrief, machine: &DaemonBrief) -> String {
    format!(
        "This Mac runs two Roomler daemons, and this page shows both. Remote desktop, capture, \
         video and file settings are the per-user half's ({}, {}), which serves the session, \
         and change there. The private network, Roomler SSH, remote commands, access and the \
         service itself are the root half's ({}, {}) — the device that `roomler ssh` and \
         `roomler exec` reach — and are shown from its file: the ones that are the person at \
         this Mac's own call (external access) change here; the rest, marked administrator's \
         (overlay_enabled, ssh_enabled, exec_enabled, remote_config_enabled, …), are changed in \
         a terminal: `sudo roomler config set <key> <value>`.",
        reached.name,
        config_file_of(reached),
        machine.name,
        config_file_of(machine)
    )
}

/// #2871 — the banner's text on a two-half Mac whose root half predates
/// design A and refuses the listing over its person socket: the rows are the
/// per-user half's alone, and the root half's switches are an administrator's
/// — the words the banner carried from #2879.
fn two_halves_legacy_note(reached: &DaemonBrief, machine: &DaemonBrief) -> String {
    format!(
        "This Mac runs two Roomler daemons. The switches on this page change the per-user half \
         ({}, {}), which captures the screen for remote desktop. The private network, Roomler SSH \
         and remote commands are served by the root half ({}, {}) — the device that `roomler ssh` \
         and `roomler exec` reach — and its switches (overlay_enabled, ssh_enabled, exec_enabled, \
         remote_config_enabled, …) are changed by an administrator in a terminal: \
         `sudo roomler config set <key> <value>`.",
        reached.name,
        config_file_of(reached),
        machine.name,
        config_file_of(machine)
    )
}

/// #2891 — pure: the ROOT daemon whose configuration this app may neither
/// read nor change, if the layout has one. Two layouts have: a root daemon
/// REACHED by an unprivileged companion — a Linux system install, whose person
/// socket is the first path `connect()` tries and whose `person_may` refuses
/// every config verb — and a Mac with a root half and no per-user half, where
/// nothing is reached and the root half answers its person socket alone.
/// `None` when the companion itself runs as root (it reaches the daemon's own
/// socket), on Windows (`pipe_admits_users`: the pipe's `IU` ace lets every
/// interactive user at those verbs), for a per-user daemon, and for a daemon
/// too old to say which account it runs as — nothing is claimed for it.
fn root_daemon_only<'a>(
    reached: Option<&'a DaemonBrief>,
    machine: Option<&'a DaemonBrief>,
    companion_is_root: bool,
    pipe_admits_users: bool,
) -> Option<&'a DaemonBrief> {
    if companion_is_root || pipe_admits_users {
        return None;
    }
    match reached {
        Some(r) => (r.privileged == Some(true)).then_some(r),
        None => machine.filter(|m| m.privileged == Some(true)),
    }
}

/// The file a root daemon names for itself, or the packaged install's.
fn root_config_file_of(daemon: &DaemonBrief) -> String {
    daemon.config_path.clone().unwrap_or_else(|| {
        "its own config.toml (/etc/roomler/config.toml on a packaged install)".to_string()
    })
}

/// #2871 (design A) — the banner's text when [`root_daemon_only`] names a
/// daemon and it served its listing over the person socket (a Linux system
/// install; a Mac with a root half alone, #2891): which file, which rows
/// change here — the keys that are the person at the device's own call — and
/// the administrator's command for the rows marked.
fn root_only_note(daemon: &DaemonBrief) -> String {
    format!(
        "This computer's Roomler service runs as root ({}, {}) — a machine-wide install — and \
         its settings are read from {}. The ones that are the person at this computer's own \
         call change here: where incoming files and recordings land, what a remote session may \
         use (clipboard, files, audio), privacy mode, lock on end, display match, the browser \
         check, external access. The rest — the device-owned switches (exec_enabled, \
         ssh_enabled, remote_config_enabled), the private network, Roomler SSH, the updater — \
         are marked administrator's and are changed in a terminal: `sudo roomler config set \
         <key> <value>`.",
        daemon.name,
        daemon.version,
        root_config_file_of(daemon)
    )
}

/// #2891 — the card's text when [`root_daemon_only`] names a daemon that
/// REFUSED its listing over the person socket (one older than design A on
/// #2871): which daemon runs, which file its settings are read from, and the
/// administrator's commands — in place of the "Could not load the
/// configuration" the page showed when the fallback read a per-user file a
/// system install does not have.
fn root_only_legacy_note(daemon: &DaemonBrief) -> String {
    format!(
        "This computer's Roomler service runs as root ({}, {}) — a machine-wide install — and \
         its settings are read from {}. They are an administrator's to change, in a terminal: \
         `sudo roomler config ls` lists them, `sudo roomler config set <key> <value>` changes \
         one. This app reaches that service through the person-at-the-device socket, which \
         shows who is viewing this screen, ends a session, answers consent prompts and lists \
         the devices this machine's private network reaches; this version of the service serves \
         no configuration over it, so the device-owned switches (exec_enabled, ssh_enabled, \
         remote_config_enabled, …) have no in-app path here.",
        daemon.name,
        daemon.version,
        root_config_file_of(daemon)
    )
}

/// #2891 — does this companion run as root? Only then does `connect()` reach
/// a root daemon's own socket; otherwise a root daemon it reaches is behind
/// its person socket, where no config verb is served.
fn process_is_root() -> bool {
    #[cfg(unix)]
    {
        localapi::process_is_root()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// #2891 — did a config verb's refusal come from a ROOT daemon this
/// unprivileged companion reached — a Linux system install's person socket,
/// which serves `Status` and refuses every config verb? Then the per-user file
/// the callers would fall back to is not that daemon's, and may be a stale
/// copy no daemon reads: writing it reports a success the daemon refused
/// (#2871's failure, on Linux). One `Status` on the same connection.
async fn refused_by_root_daemon(client: &mut localapi::Client) -> bool {
    let Ok(status) = client.status().await else {
        return false;
    };
    let reached = DaemonBrief::from(&status);
    root_daemon_only(Some(&reached), None, process_is_root(), cfg!(windows)).is_some()
}

/// #2871 — the root half's `Status`, over the person socket it serves this
/// account (`/var/run/roomler-person/<uid>.sock`, FR-97 P2g). macOS only.
/// Kept only when ROOT serves the socket (`connect_unix_served_by_root`), and
/// bounded like the per-user half's own reads of it: a root half that does
/// not answer in 1.5 s is not named.
#[cfg(target_os = "macos")]
async fn machine_half_brief() -> Option<DaemonBrief> {
    let path = localapi::own_person_socket_path()?;
    if !path.exists() {
        return None;
    }
    let ask = async {
        let mut client = localapi::connect_unix_served_by_root(path).await.ok()?;
        client.status().await.ok()
    };
    tokio::time::timeout(Duration::from_millis(1500), ask)
        .await
        .ok()
        .flatten()
        .map(|s| DaemonBrief::from(&s))
}

/// Off macOS there is no second half to name: Windows runs one daemon, and a
/// Linux system install's companion reaches the root daemon itself (its
/// person socket is the first path `connect()` tries).
#[cfg(not(target_os = "macos"))]
async fn machine_half_brief() -> Option<DaemonBrief> {
    None
}

/// #2871 — which daemon the Settings page's Device configuration edits, for
/// the banner it shows on a two-half Mac; #2891 — and whether that daemon's
/// configuration is root's alone (a Linux system install, a Mac with a root
/// half alone), for the explanation it shows in place of rows. Read-only: one
/// `Status` from the daemon the page edits, and on a Mac one from the root
/// half's person socket, which admits nothing else this needs.
#[tauri::command]
pub async fn cmd_config_target() -> ConfigTarget {
    let reached = daemon_node_status().await.map(|s| DaemonBrief::from(&s));
    let machine = match &reached {
        // A per-user daemon: is a root half beside it (a two-half Mac)?
        Some(r) if r.privileged == Some(false) => machine_half_brief().await,
        // #2891 — nothing reached: a Mac with a root half alone still serves
        // this person its person socket, and that half is the one to name.
        None => machine_half_brief().await,
        Some(_) => None,
    };
    let two = reached
        .as_ref()
        .is_some_and(|r| two_halves(r, machine.as_ref()));
    let root = root_daemon_only(
        reached.as_ref(),
        machine.as_ref(),
        process_is_root(),
        cfg!(windows),
    );
    let (note, legacy_note) = match (&reached, &machine, root) {
        (Some(r), Some(m), _) if two => (
            Some(two_halves_note(r, m)),
            Some(two_halves_legacy_note(r, m)),
        ),
        (_, _, Some(d)) => (Some(root_only_note(d)), Some(root_only_legacy_note(d))),
        _ => (None, None),
    };
    ConfigTarget {
        root_only: root.is_some(),
        reached,
        machine,
        two_halves: two,
        note,
        legacy_note,
    }
}

/// The LocalAPI client wraps a daemon's own `Error { message }` as
/// `localapi error: <message>`; the person reads the daemon's words.
fn daemon_message(message: &str) -> String {
    message
        .strip_prefix("localapi error: ")
        .unwrap_or(message)
        .to_string()
}

// ─── FR-84 D4 — the Overview: encoder matrix, incoming-files folder ────

/// What the "Hardware video encoding" card renders. Never rejects: a
/// failure is a state the card words (`reason`).
#[derive(Debug, Serialize)]
pub struct EncoderCapsView {
    /// The daemon answered the verb.
    pub available: bool,
    /// Why not: `daemon_unreachable`, `old_daemon` (predates the verb), or
    /// the failing stage and error.
    pub reason: Option<String>,
    /// The daemon's answer, verbatim (`state`, `cells`, `denied`, …).
    pub caps: Option<localapi::EncoderCapsSummary>,
    /// The `denied` entries this app can place in the matrix.
    pub denied_cells: Vec<DeniedCell>,
}

/// One `encoder_cells_deny` entry, in the matrix's vocabulary.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DeniedCell {
    /// `h264` · `hevc` · `av1` · `vp9` — the cell codec wire names.
    pub codec: String,
    /// A `VideoBackend` wire name (`d3d12`, not FFmpeg's `d3d12va`).
    pub backend: String,
    /// `yuv420` | `yuv444`.
    pub chroma: String,
    /// The entry as the denylist spells it (`hevc_qsv:yuv444`).
    pub entry: String,
}

/// Place a denylist entry (`<ffmpeg name>:<chroma>`) in the matrix — the
/// same split `VideoBackend::from_ffmpeg_name` makes in the daemon
/// (crates/remote_control/src/models.rs): codec before the first `_`, the
/// FFmpeg backend suffix after it, `d3d12va` meaning the `d3d12` column.
/// `None` for anything else (a backend newer than this app, a typo in
/// the key) — the card still lists those, verbatim, under the matrix.
fn place_denied(entry: &str) -> Option<DeniedCell> {
    let (name, chroma) = entry.trim().split_once(':')?;
    let (codec, suffix) = name.split_once('_')?;
    if !matches!(codec, "h264" | "hevc" | "av1" | "vp9") || !matches!(chroma, "yuv420" | "yuv444") {
        return None;
    }
    let backend = match suffix {
        "nvenc" | "qsv" | "amf" | "videotoolbox" | "vaapi" | "vulkan" => suffix,
        "d3d12va" => "d3d12",
        _ => return None,
    };
    Some(DeniedCell {
        codec: codec.to_string(),
        backend: backend.to_string(),
        chroma: chroma.to_string(),
        entry: entry.trim().to_string(),
    })
}

impl EncoderCapsView {
    fn unavailable(reason: String) -> Self {
        Self {
            available: false,
            reason: Some(reason),
            caps: None,
            denied_cells: Vec::new(),
        }
    }

    fn from_summary(caps: localapi::EncoderCapsSummary) -> Self {
        let denied_cells = caps.denied.iter().filter_map(|d| place_denied(d)).collect();
        Self {
            available: true,
            reason: None,
            caps: Some(caps),
            denied_cells,
        }
    }
}

/// FR-84 D4 — what this device can encode, from the daemon's CACHED probe
/// (the verb never starts one). The card polls this only while the answer
/// is `not_probed`, and re-reads it when the daemon comes back or changes.
#[tauri::command]
pub async fn cmd_encoder_caps() -> EncoderCapsView {
    const SURFACE: &str = "encoder_caps";
    let mut client = match localapi::connect().await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return EncoderCapsView::unavailable("daemon_unreachable".into());
        }
        Err(e) => {
            return EncoderCapsView::unavailable(refresh_failed(
                SURFACE,
                describe_io("connect", &e),
            ));
        }
    };
    match client.encoder_caps().await {
        Ok(caps) => {
            refresh_ok(SURFACE);
            EncoderCapsView::from_summary(caps)
        }
        // A daemon older than the verb: not a failure, a version.
        Err(e) if e.to_string().contains("unknown variant") => {
            EncoderCapsView::unavailable("old_daemon".into())
        }
        Err(e) => {
            EncoderCapsView::unavailable(refresh_failed(SURFACE, describe_io("encoder_caps", &e)))
        }
    }
}

/// What the "Incoming files" card renders.
#[derive(Debug, Serialize)]
pub struct FilesDirView {
    /// The daemon answered.
    pub available: bool,
    /// Why not (`daemon_unreachable`, or the failing stage and error).
    pub reason: Option<String>,
    /// The daemon knows `files_dir` (its config surface lists the key). An
    /// older one does not: the card says "update the service" and offers
    /// no controls.
    pub supported: bool,
    /// Where a drop would land right now, as the daemon reports it.
    pub effective: Option<String>,
    /// The configured value, as stored (`~\Drops`, `D:\In`), or `None` for
    /// the default.
    pub configured: Option<String>,
    /// `configured` resolved for THIS user (`~` expanded), so the card can
    /// tell "in use" from "set, but refused right now".
    pub configured_resolved: Option<String>,
}

impl FilesDirView {
    fn unavailable(reason: String) -> Self {
        Self {
            available: false,
            reason: Some(reason),
            supported: false,
            effective: None,
            configured: None,
            configured_resolved: None,
        }
    }
}

/// This app's own user's home — the profile a machine-wide daemon treats
/// as "active" while this person is the one signed in.
fn own_home() -> Option<String> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(var).ok().filter(|h| !h.trim().is_empty())
}

/// FR-84 D4 — the card's data: the daemon's effective folder (status) and
/// the configured value (its config surface), on ONE connection.
#[tauri::command]
pub async fn cmd_files_dir_view() -> FilesDirView {
    const SURFACE: &str = "files_dir";
    let mut client = match localapi::connect().await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return FilesDirView::unavailable("daemon_unreachable".into());
        }
        Err(e) => {
            return FilesDirView::unavailable(refresh_failed(SURFACE, describe_io("connect", &e)));
        }
    };
    let status = match client.status().await {
        Ok(s) => s,
        Err(e) => {
            return FilesDirView::unavailable(refresh_failed(SURFACE, describe_io("status", &e)));
        }
    };
    let entries = match client.config_entries().await {
        Ok(e) => e,
        Err(e) => {
            return FilesDirView::unavailable(refresh_failed(
                SURFACE,
                describe_io("config_entries", &e),
            ));
        }
    };
    refresh_ok(SURFACE);
    let entry = entries.into_iter().find(|e| e.key == "files_dir");
    let configured = entry.as_ref().and_then(|e| e.value.clone());
    let configured_resolved = configured.as_deref().and_then(|raw| {
        use roomler_node_core::files_dir::{Rules, Writer, resolve};
        resolve(raw, &Writer::unprivileged(own_home()), &Rules::from_env())
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    });
    FilesDirView {
        available: true,
        reason: None,
        supported: entry.is_some(),
        effective: status.files_dir,
        configured,
        configured_resolved,
    }
}

/// The result of "Change…".
#[derive(Debug, Serialize)]
pub struct FilesDirChange {
    /// The picker was closed without choosing — nothing was sent.
    pub cancelled: bool,
    /// What was sent: a folder inside this user's own profile goes as
    /// `~\…` (see [`files_dir_value_for`]).
    pub value: Option<String>,
    /// The daemon's echo of the saved key.
    pub entry: Option<localapi::ConfigEntry>,
}

/// The config value for a picked folder: `~`-relative when it sits inside
/// `home` (by the daemon's own component rules), else the path as picked.
/// On a machine-wide install the config is one file for every user, and
/// an absolute folder inside one user's profile is refused for every other
/// user by the SYSTEM rule; `~` lands each of them in their own.
fn files_dir_value_for(picked: &Path, home: Option<&str>) -> String {
    let spelled = picked.to_string_lossy().into_owned();
    home.and_then(|h| {
        roomler_node_core::files_dir::tilde_form(
            &spelled,
            h,
            &roomler_node_core::files_dir::Rules::from_env(),
        )
    })
    .unwrap_or(spelled)
}

/// `files_dir` through the DAEMON only. It is the one that knows who will
/// write, as whom, into which profile; its refusal is the answer, shown in
/// its own words — never a cue to write the file directly.
async fn set_files_dir_via_daemon(value: Option<&str>) -> Result<localapi::ConfigEntry, String> {
    let mut client = localapi::connect().await.map_err(daemon_unreachable)?;
    client
        .config_set("files_dir", value)
        .await
        .map_err(|e| explain_files_dir_error(&e.to_string()))
}

fn explain_files_dir_error(message: &str) -> String {
    if message.contains("unknown or non-editable config key") || message.contains("unknown variant")
    {
        "The device service predates this setting — update it, then try again.".to_string()
    } else {
        daemon_message(message)
    }
}

/// FR-84 D4 — "Change…": the native folder picker (it can create a new
/// folder), then `ConfigSet files_dir` through the daemon.
///
/// The picker is driven from Rust: the dialog plugin is initialised but the
/// app grants the webview no plugin capabilities, so the JS dialog API is
/// not reachable — and should not need to be for one folder choice.
/// `blocking_pick_folder` runs the dialog on the main thread and blocks the
/// CALLER until it closes, so it is called on a blocking-pool thread.
#[tauri::command]
pub async fn cmd_pick_files_dir(app: tauri::AppHandle) -> Result<FilesDirChange, String> {
    // Start where drops land now — best effort; the picker opens anyway.
    let start = match localapi::connect().await {
        Ok(mut c) => c.status().await.ok().and_then(|s| s.files_dir),
        Err(_) => None,
    };
    let picked = tauri::async_runtime::spawn_blocking(move || {
        use tauri::Manager;
        use tauri_plugin_dialog::DialogExt;
        let mut dialog = app
            .dialog()
            .file()
            .set_title("Where should files dropped onto this device land?")
            .set_can_create_directories(true);
        if let Some(dir) = start.filter(|d| Path::new(d).is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(window) = app.get_webview_window("main") {
            dialog = dialog.set_parent(&window);
        }
        dialog.blocking_pick_folder()
    })
    .await
    .map_err(|e| format!("folder picker: {e}"))?;
    let Some(picked) = picked else {
        return Ok(FilesDirChange {
            cancelled: true,
            value: None,
            entry: None,
        });
    };
    let path = picked
        .into_path()
        .map_err(|e| format!("folder picker: {e}"))?;
    let value = files_dir_value_for(&path, own_home().as_deref());
    let entry = set_files_dir_via_daemon(Some(&value)).await?;
    Ok(FilesDirChange {
        cancelled: false,
        value: Some(value),
        entry: Some(entry),
    })
}

/// FR-84 D4 — "Use default": clear `files_dir` (the active user's
/// Downloads), through the daemon like every change to this key.
#[tauri::command]
pub async fn cmd_files_dir_default() -> Result<localapi::ConfigEntry, String> {
    set_files_dir_via_daemon(None).await
}

/// FR-84 D4 — "Open folder": the folder the DAEMON says drops land in, or —
/// before the first drop has created it — its nearest existing parent.
/// Takes no path from the page, and opens only a DIRECTORY: `explorer
/// <file>` / `open <file>` would launch a file rather than show it.
#[tauri::command]
pub async fn cmd_open_files_dir() -> Result<(), String> {
    let mut client = localapi::connect().await.map_err(daemon_unreachable)?;
    let status = client
        .status()
        .await
        .map_err(|e| daemon_message(&e.to_string()))?;
    let dir = status.files_dir.ok_or_else(|| {
        "The device service does not report where files land — update it, then try again."
            .to_string()
    })?;
    tokio::task::spawn_blocking(move || {
        let target = Path::new(&dir)
            .ancestors()
            .find(|a| a.is_dir())
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("{dir} does not exist, nor does any folder above it"))?;
        open_path_in_explorer(&target)
    })
    .await
    .map_err(|e| format!("task join: {e}"))?
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
    open_in_browser(app, url)
}

/// The default browser, for everything but Linux. shell::open is deprecated
/// in favour of tauri-plugin-opener, but the shell plugin is already shipped
/// + initialised here; not worth a new plugin dependency for this path.
#[cfg(not(target_os = "linux"))]
fn open_in_browser<R: tauri::Runtime>(app: &tauri::AppHandle<R>, url: &str) -> Result<(), String> {
    #[allow(deprecated)]
    {
        use tauri_plugin_shell::ShellExt;
        app.shell()
            .open(url, None)
            .map_err(|e| format!("opening browser: {e}"))
    }
}

/// #2713 — Linux opens a link ONLY with the app registered for its scheme,
/// which for `https` is the person's web browser, and otherwise says there is
/// none, in the page and in a dialog.
///
/// ⚠️ Not `xdg-open` (what `shell::open` runs). When no app claims the
/// scheme, xdg-open (GNOME: `gio open`) falls back to the URL's CONTENT
/// type: it fetches the page, sees `text/html` and hands the URL to the
/// `text/html` default. Measured 2026-10-22 on a Fedora Asahi GNOME box with
/// no browser installed: View screen opened the viewer's URL in GNOME Text
/// Editor, which showed the page's HTML. A missing browser has to be an
/// error the person can act on, never a text editor.
///
/// ⚠️ The lookup reads the desktop's defaults through `XDG_CURRENT_DESKTOP`
/// (`gnome-mimeapps.list`, `kde-mimeapps.list`) and `XDG_DATA_DIRS` (where a
/// per-user Flatpak browser is exported). A companion the daemon started
/// had neither until the daemon began passing the session's environment on
/// (`roomlerd`'s `systemd_run_command`, same issue).
///
/// ⚠️ #2729 — the launch runs on the GTK main thread, because the activation
/// token that brings the browser forward comes from GDK
/// ([`launch_default_handler`]).
#[cfg(target_os = "linux")]
fn open_in_browser<R: tauri::Runtime>(app: &tauri::AppHandle<R>, url: &str) -> Result<(), String> {
    let link = url.to_string();
    let result = on_main_thread(app, move || launch_default_handler(&link)).and_then(|r| r);
    if let Err(e) = &result {
        tracing::warn!(%e, "opening a link failed");
        use tauri_plugin_dialog::{DialogExt as _, MessageDialogKind};
        app.dialog()
            .message(e.as_str())
            .title("Roomler")
            .kind(MessageDialogKind::Warning)
            .show(|_| {});
    }
    result
}

/// The app registered for the link's scheme, launched with an activation
/// token. ⚠️ GTK main thread only: the token comes from GDK.
///
/// ⚠️ #2729 — without a token, a browser that is ALREADY running opens the
/// tab and stays behind the companion: GNOME's focus-stealing prevention
/// will not raise a window for a request that names no user action, so to
/// the person View screen did nothing. The token (Wayland's
/// `xdg_activation_v1`, X11's startup id) ties the request to the click that
/// made it; the launched browser hands it to its running instance, which may
/// then come forward. It goes in the environment EXPLICITLY, in a plain GIO
/// context: GLib asks a launch context for one only when the app's desktop
/// file says `StartupNotify=true`, and Fedora's Chromium's does not.
/// Measured 2026-10-22 on the lab's Fedora Asahi GNOME box (GTK 3.24.52,
/// GLib 2.84, mutter 48, Chromium 148, the browser behind another window):
/// no context and GDK's own context both left it behind; the explicit
/// token brought it forward.
///
/// ⚠️ A PLAIN context, never GDK's own with the token set on it: on Wayland
/// GDK's answers `get_display` with the compositor socket's name
/// (`wayland-0`), and GIO writes that into the launched program's `DISPLAY`,
/// where an X11 client the browser starts would look for an X server that
/// does not exist. Measured on the same box: GDK's `'wayland-0'`, a plain
/// context's `None`.
#[cfg(target_os = "linux")]
fn launch_default_handler(url: &str) -> Result<(), String> {
    use gio::prelude::*;
    let scheme = url_scheme(url).ok_or_else(|| format!("not a link that can be opened: {url}"))?;
    let handler =
        gio::AppInfo::default_for_uri_scheme(scheme).ok_or_else(|| no_handler_message(scheme))?;
    let context = gio::AppLaunchContext::new();
    if let Some(token) = activation_token(&handler) {
        context.setenv("XDG_ACTIVATION_TOKEN", token.as_str());
        context.setenv("DESKTOP_STARTUP_ID", token.as_str());
    }
    handler
        .launch_uris(&[url], Some(&context))
        .map_err(|e| format!("{} could not open the link: {e}", handler.name()))
}

/// A token from the display server for launching `handler`, or `None` where
/// there is none to ask (no GDK display, or not on its thread). Best-effort:
/// the link opens either way.
#[cfg(target_os = "linux")]
fn activation_token(handler: &gio::AppInfo) -> Option<gio::glib::GString> {
    use gio::prelude::*;
    if !gtk::is_initialized_main_thread() {
        return None;
    }
    gtk::gdk::Display::default()?
        .app_launch_context()?
        .startup_notify_id(handler, &[])
}

/// Run `f` on the app's main thread and return its answer. Tauri runs the
/// task in place when the caller IS the main thread (the tray's Open
/// Roomler) and queues it otherwise (a command), so the wait cannot deadlock.
#[cfg(target_os = "linux")]
fn on_main_thread<R: tauri::Runtime, T: Send + 'static>(
    app: &tauri::AppHandle<R>,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(f());
    })
    .map_err(|e| format!("could not reach the app's main thread: {e}"))?;
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| "the app's main thread did not answer within 10 s".to_string())
}

/// A link's scheme (`https` of `https://…`), when it has a valid one: per
/// RFC 3986, a letter followed by letters, digits, `+`, `-` or `.`.
#[cfg(any(target_os = "linux", test))]
fn url_scheme(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once(':')?;
    let mut bytes = scheme.bytes();
    let valid = bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
    (valid && !rest.is_empty()).then_some(scheme)
}

/// What the person reads when nothing on this computer opens `scheme` links.
#[cfg(any(target_os = "linux", test))]
fn no_handler_message(scheme: &str) -> String {
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        "No web browser is set up on this computer, so the link cannot open. Install one \
         (Firefox, for example) or choose a default web browser in your desktop's settings, \
         then try again."
            .to_string()
    } else {
        format!("Nothing on this computer opens {scheme}: links.")
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

/// S2/S7 — open the remote-control viewer for one of the agent-backed
/// devices this one can see (`{server}/tenant/{tid}/agent/{aid}/remote`) —
/// in-app on Windows, default browser elsewhere. The URL is constructed
/// ONLY from this device's own configured server origin + hex-validated
/// ids — never from peer-supplied strings — so a hostile device name
/// can't steer the view to a foreign site.
///
/// FR-84 D5c — `org` names the enrollment the device belongs to (the Devices
/// page lists one org at a time). A secondary org's device lives on THAT
/// org's server and tenant, which are read from this device's own
/// `[[orgs]]` entry — still never from anything the page or a peer supplied.
/// Absent / empty / `primary` = the primary enrollment, as before.
#[tauri::command]
pub async fn cmd_open_remote(
    app: tauri::AppHandle,
    agent_id: String,
    org: Option<String>,
) -> Result<(), String> {
    let aid = agent_id.trim().to_ascii_lowercase();
    if aid.len() != 24 || !aid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid agent id".to_string());
    }
    let url = tokio::task::spawn_blocking(move || {
        let is_scm = probe_service_state().0 == "scmService";
        let path = active_config_path(is_scm)?;
        let cfg = config::load(&path).map_err(|e| format!("Loading config: {e}"))?;
        let (server, tid) = remote_target(&cfg, org.as_deref())?;
        Ok::<String, String>(format!("{server}/tenant/{tid}/agent/{aid}/remote"))
    })
    .await
    .map_err(|e| format!("task join: {e}"))??;
    open_web_or_browser(&app, &url)
}

/// The `(server origin, tenant id)` a "View screen" for a device in `org`
/// opens against — the primary's scalar identity, or the named `[[orgs]]`
/// entry's. Both validated: an http(s) origin and a 24-hex tenant id. Pure
/// over the loaded config, so the resolution is unit-tested.
fn remote_target(cfg: &AgentConfig, org: Option<&str>) -> Result<(String, String), String> {
    let label = org
        .map(str::trim)
        .filter(|o| !o.is_empty() && *o != config::PRIMARY_ORG_LABEL);
    let (server, tid) = match label {
        None => (cfg.server_url.as_str(), cfg.tenant_id.as_str()),
        Some(label) => {
            let entry = cfg
                .orgs
                .iter()
                .find(|o| o.label == label)
                .ok_or_else(|| format!("this device has no enrollment labelled {label:?}"))?;
            (entry.server_url.as_str(), entry.tenant_id.as_str())
        }
    };
    let tid = tid.trim().to_ascii_lowercase();
    if tid.len() != 24 || !tid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("this device's config has no valid tenant id for that organization".into());
    }
    let server = server.trim().trim_end_matches('/').to_string();
    if !server.starts_with("https://") && !server.starts_with("http://") {
        return Err("this device's config has no valid server URL for that organization".into());
    }
    Ok((server, tid))
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
                // #2891 — a ROOT daemon that refused this unprivileged app (a
                // Linux system install's person socket): its words stand. The
                // per-user file below is not its, and may be a stale copy
                // whose rename no daemon would ever read.
                if refused_by_root_daemon(&mut client).await {
                    return Err(daemon_message(&e.to_string()));
                }
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

/// FR-84 D3 — what the daemon said to "Apply now".
#[derive(Debug, Serialize)]
pub struct RestartReport {
    /// Accepted: the daemon is on its way out.
    pub started: bool,
    /// `scm` | `task` | `systemd` | `launchd` | `macos-supervisor`.
    pub supervisor: String,
    /// `supervisor` | `caller` — see [`cmd_restart_wait`].
    pub restart_by: String,
    pub exit_code: i32,
    /// The process that is leaving (pid + start time, `0` = not reported);
    /// [`cmd_restart_wait`] waits for another.
    pub pid: u32,
    pub started_at_ms: u64,
    /// #1684 — how long the supervisor says the relaunch may take, in seconds
    /// (systemd: `TimeoutStopSec + RestartSec`); `None` for a non-systemd or
    /// older daemon. [`cmd_restart_wait`] waits at least this long.
    pub restart_within_s: Option<u64>,
    /// Refused: the daemon's own reason, shown verbatim.
    pub refusal: Option<String>,
    /// The daemon predates the verb.
    pub predates: bool,
}

/// FR-84 D3 — ask the daemon to restart itself through its supervisor. The
/// daemon decides: a refusal (no supervisor it can prove, the device's
/// `local_restart_enabled` off, a recording running, a restart moments ago)
/// comes back as `refusal`, never as an error; `Err` means the service could
/// not be asked at all.
#[tauri::command]
pub async fn cmd_restart_daemon(reason: String) -> Result<RestartReport, String> {
    let mut client = localapi::connect().await.map_err(daemon_unreachable)?;
    let answer = client
        .restart_daemon(&reason)
        .await
        .map_err(|e| format!("asking the device service to restart: {e}"))?;
    let mut report = RestartReport {
        started: false,
        supervisor: String::new(),
        restart_by: String::new(),
        exit_code: 0,
        pid: 0,
        started_at_ms: 0,
        restart_within_s: None,
        refusal: None,
        predates: false,
    };
    match answer {
        localapi::RestartAnswer::Accepted {
            supervisor,
            restart_by,
            exit_code,
            leaving,
            restart_within_s,
        } => {
            tracing::info!(%supervisor, %restart_by, exit_code, pid = ?leaving.pid, restart_within_s, %reason,
                "restart accepted by the device service");
            report.started = true;
            report.supervisor = supervisor;
            report.restart_by = restart_by;
            report.exit_code = exit_code;
            report.pid = leaving.pid.unwrap_or(0);
            report.started_at_ms = leaving.started_at_ms.unwrap_or(0);
            report.restart_within_s = restart_within_s;
        }
        localapi::RestartAnswer::Refused(why) => {
            tracing::info!(refusal = %why, "restart refused by the device service");
            report.refusal = Some(why);
        }
        localapi::RestartAnswer::Unsupported(_) => report.predates = true,
    }
    Ok(report)
}

/// FR-84 D3 — wait (≤ 60 s) until a daemon process other than the one that
/// accepted (`old_pid` + `old_started_at_ms`, from [`RestartReport`]) answers,
/// returning its pid. When the relaunch is ours (`restart_by = "caller"`, the
/// Windows Scheduled Task), run `roomlerd service start` whenever nothing
/// answers, again every few seconds until something does — the task's
/// `IgnoreNew` drops a start that lands while the old instance is still
/// exiting (`localapi::wait_for_restart`).
#[tauri::command]
pub async fn cmd_restart_wait(
    old_pid: u32,
    old_started_at_ms: u64,
    restart_by: String,
    // #1684 — the supervisor's stop+relaunch bound from `cmd_restart_daemon`'s
    // report (`None` for a non-systemd/older daemon). Optional so an older
    // front that omits it still behaves as before. The wait never shortens
    // below the 60 s floor.
    restart_within_s: Option<u64>,
) -> Result<Option<u32>, String> {
    let start = || async {
        tokio::task::spawn_blocking(run_service_start)
            .await
            .map_err(|e| format!("service start task: {e}"))?
    };
    let wait = localapi::restart_wait(restart_within_s, std::time::Duration::from_secs(60));
    localapi::wait_for_restart(
        localapi::DaemonInstance::leaving(old_pid, old_started_at_ms),
        restart_by == "caller",
        start,
        wait,
    )
    .await
    .inspect_err(|why| tracing::warn!(%why, "restart: the device service did not come back"))
}

/// `roomlerd service start`, the caller half of a restart under the Scheduled
/// Task. Blocking; bounded at 20 s.
fn run_service_start() -> Result<(), String> {
    let exe = agent_exe_path()?;
    #[cfg(windows)]
    {
        match spawn_no_inherit_and_wait(&exe, "service start", std::time::Duration::from_secs(20))?
        {
            0 => Ok(()),
            code => Err(format!("`roomlerd service start` exited {code}")),
        }
    }
    #[cfg(not(windows))]
    {
        let out = no_window_command(&exe)
            .args(["service", "start"])
            .output()
            .map_err(|e| format!("Spawning service start ({}): {e}", exe.display()))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }
}

/// Run `exe args` with NO handle inheritance and no console window, and wait
/// for it (killing it past `timeout`). Returns its exit code.
///
/// `std::process::Command` on Windows always passes `bInheritHandles = TRUE`,
/// so a child gets every inheritable handle this process holds — the class of
/// leak #1035 found (a companion holding a daemon's route ports). The child
/// here is short-lived and the daemon it starts is the Task Scheduler's child,
/// not ours; this makes "nothing of ours travels" true by construction rather
/// than by that argument.
#[cfg(windows)]
fn spawn_no_inherit_and_wait(
    exe: &std::path::Path,
    args: &str,
    timeout: std::time::Duration,
) -> Result<u32, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessW, GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW,
        TerminateProcess, WaitForSingleObject,
    };
    let app: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    // CreateProcessW may write into the command-line buffer, so it is ours
    // and mutable. The executable is quoted (a path with spaces), the
    // arguments are fixed words.
    let mut cmdline: Vec<u16> = format!("\"{}\" {args}", exe.display())
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // SAFETY: plain-old-data structs, all-zero is their documented initial
    // state (with `cb` set below).
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    // SAFETY: as above.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: every pointer is valid for the call — `app` and `cmdline` are
    // NUL-terminated UTF-16 buffers that outlive it, the optional ones are
    // null, and `si` / `pi` are live locals.
    let ok = unsafe {
        CreateProcessW(
            app.as_ptr(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0, // bInheritHandles = FALSE
            CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(format!(
            "starting {}: {}",
            exe.display(),
            std::io::Error::last_os_error()
        ));
    }
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: `pi` holds live handles CreateProcessW just returned; each is
    // closed exactly once below.
    unsafe {
        CloseHandle(pi.hThread);
        let result = if WaitForSingleObject(pi.hProcess, millis) == WAIT_OBJECT_0 {
            let mut code = 0u32;
            if GetExitCodeProcess(pi.hProcess, &mut code) != 0 {
                Ok(code)
            } else {
                Err(format!(
                    "reading the exit code: {}",
                    std::io::Error::last_os_error()
                ))
            }
        } else {
            TerminateProcess(pi.hProcess, 1);
            Err(format!(
                "`{} {args}` did not finish within {} s",
                exe.display(),
                timeout.as_secs()
            ))
        };
        CloseHandle(pi.hProcess);
        result
    }
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

/// #2815 — must the main window's modal ask the question itself? Only when
/// the prompt window could not be opened. Otherwise that window is asking,
/// and a modal under it would put two Approve buttons on one decision.
#[tauri::command]
pub fn cmd_consent_modal_needed() -> bool {
    crate::panels::consent_modal_needed()
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
pub(crate) fn daemon_unreachable(e: std::io::Error) -> String {
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
    /// FR-85 P3b — the owner's remote-recording gates, `None` against a
    /// service that predates them (the view then offers no toggle).
    pub remote: Option<RemoteRecordingGates>,
}

/// FR-85 P3b — `record_remote_enabled` / `record_remote_audio`, as the
/// service reports them.
#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
pub struct RemoteRecordingGates {
    pub enabled: bool,
    pub audio: bool,
}

/// The remote gates out of a config listing; `None` unless the service
/// knows the enabling key.
fn remote_gates(entries: &[localapi::ConfigEntry]) -> Option<RemoteRecordingGates> {
    let flag = |key: &str| {
        entries
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.value.as_deref() == Some("true"))
    };
    Some(RemoteRecordingGates {
        enabled: flag("record_remote_enabled")?,
        audio: flag("record_remote_audio").unwrap_or(false),
    })
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
            remote: None,
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
            remote: None,
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
pub(crate) fn explain_recording_error(message: &str) -> String {
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
    let entries = client.config_entries().await.unwrap_or_default();
    let record_dir = entries
        .iter()
        .find(|e| e.key == "record_dir")
        .and_then(|e| e.value.clone())
        .filter(|v| !v.is_empty());
    refresh_ok(SURFACE);
    RecordingsView {
        available: true,
        reason: None,
        unsupported: false,
        state: Some(state),
        listing: Some(listing),
        record_dir,
        remote: remote_gates(&entries),
    }
}

/// Keys whose change the daemon must accept itself: a refusal it ANSWERED
/// is final, never retried as a direct file write (`cmd_config_set`).
///
/// FR-84 D4 — `files_dir` too, matched EXACTLY: the daemon checks it against
/// what this app cannot see (the identity it writes as — SYSTEM on a machine
/// install — the console user, its own disk), while the direct-file path can
/// check only the shape. Falling back after a refusal would save, wherever
/// the user can write the config, exactly the value the daemon refused and
/// hide why; and a daemon too old to know the key would ignore the file and
/// be reported "in effect now".
///
/// FR-52 — and every `external_*` key, by prefix like `record_`: who may open
/// this device to people OUTSIDE its organization is the console person's
/// call, which only the daemon can check (the pipe admits RDP guests too).
/// Writing the file after it refused would open gate 3 — or set how strangers
/// are let in — against the daemon's word.
fn is_daemon_owned_key(key: &str) -> bool {
    key.starts_with("record_") || key == "files_dir" || key.starts_with("external_")
}

/// The words for a daemon-owned key's refusal.
fn explain_daemon_owned_error(key: &str, message: &str) -> String {
    if key == "files_dir" {
        explain_files_dir_error(message)
    } else if key.starts_with("external_") {
        // The daemon's refusal already says who may change it.
        message.to_string()
    } else {
        explain_recording_error(message)
    }
}

/// Start recording this device's screen. Resolves once the recorder is
/// encoding, with the file and encoder named; rejects with the daemon's
/// reason otherwise (not the console user, a SYSTEM/root service, no
/// encoder, no disk).
///
/// FR-85 P1c — `system_audio` / `microphone` (JS `systemAudio` /
/// `microphone`) are OFF unless the page passes `true`.
#[tauri::command]
pub async fn cmd_record_start(
    fps: Option<u32>,
    encoder: Option<String>,
    system_audio: Option<bool>,
    microphone: Option<bool>,
) -> Result<localapi::RecordingState, String> {
    let mut client = localapi::connect().await.map_err(daemon_unreachable)?;
    client
        .record_start(localapi::RecordStartOpts {
            fps,
            encoder,
            system_audio: system_audio.unwrap_or(false),
            microphone: microphone.unwrap_or(false),
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
pub(crate) fn check_recording_name(name: &str) -> Result<(), String> {
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

// ─── the Devices page (FR-84 D5c) ──────────────────────────────────

/// What a rejected [`cmd_devices`] / [`cmd_mesh`] carries: a JSON string
/// `{code, message, status?}`, so the page picks its fallback by CAUSE — an
/// old service or an old server falls back to the peers table with an
/// "update" note; anything else keeps the last good page and says why —
/// instead of sniffing prose.
#[derive(Debug, Serialize)]
struct DirectoryFailure<'a> {
    code: &'a str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
}

/// Shape a [`localapi::DirectoryError`] for the page, and record it in the
/// refresh ledger so a failing list is in the desktop log (rate-limited).
fn directory_failure(surface: &'static str, stage: &str, e: localapi::DirectoryError) -> String {
    let message = match &e {
        localapi::DirectoryError::Upstream { message, .. } => message.clone(),
        localapi::DirectoryError::UnsupportedDaemon { .. } => {
            "The device service on this machine predates the device list — update it to see \
             every device your network lets this one see."
                .to_string()
        }
        localapi::DirectoryError::Io(io) => describe_io(stage, io),
    };
    let failure = DirectoryFailure {
        code: e.code(),
        message,
        status: e.status(),
    };
    refresh_failed(surface, format!("{}: {}", failure.code, failure.message));
    serde_json::to_string(&failure).unwrap_or_else(|_| {
        r#"{"code":"daemon_error","message":"unencodable failure"}"#.to_string()
    })
}

/// FR-84 D5c — one page of the devices THIS device's private network lets
/// it see, itself included, as the org's server lists them (asked by the
/// daemon with the org's agent token — the companion never holds one).
/// Search, sort and paging run on the server. `org` = an enrollment label;
/// absent = the primary.
///
/// Rejects with a [`DirectoryFailure`] JSON string. An old daemon's "unknown
/// variant" arrives as `unsupported_daemon`, a server without the route as
/// `unsupported_server`.
#[tauri::command]
pub async fn cmd_devices(
    org: Option<String>,
    page: Option<u64>,
    per_page: Option<u64>,
    q: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
) -> Result<localapi::DevicesPage, String> {
    const SURFACE: &str = "devices";
    let mut client = localapi::connect()
        .await
        .map_err(|e| directory_failure(SURFACE, "connect", e.into()))?;
    let query = localapi::DevicesQuery {
        page: page.unwrap_or(0),
        per_page: per_page.unwrap_or(0),
        q,
        sort,
        dir,
    };
    match client.devices(org.as_deref().unwrap_or(""), &query).await {
        Ok(p) => {
            refresh_ok(SURFACE);
            Ok(p)
        }
        Err(e) => Err(directory_failure(SURFACE, "devices", e)),
    }
}

/// FR-84 D5c — the mesh graph for the same set. `enabled: false` in the
/// answer is the server's statistics being off (data, not a failure).
#[tauri::command]
pub async fn cmd_mesh(org: Option<String>) -> Result<localapi::MeshView, String> {
    const SURFACE: &str = "mesh";
    let mut client = localapi::connect()
        .await
        .map_err(|e| directory_failure(SURFACE, "connect", e.into()))?;
    match client.mesh(org.as_deref().unwrap_or("")).await {
        Ok(v) => {
            refresh_ok(SURFACE);
            Ok(v)
        }
        Err(e) => Err(directory_failure(SURFACE, "mesh", e)),
    }
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
pub(crate) fn no_window_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
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
/// "scmService" on perMachine; "systemDaemon" for a Linux or macOS root
/// daemon (#2871). "none" when nothing is registered.
fn probe_service_state() -> (String, bool) {
    let Ok(exe) = agent_exe_path() else {
        return ("none".to_string(), false);
    };
    let stdout_of = |args: &[&str]| {
        no_window_command(&exe)
            .args(args)
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    // The SCM service first: when one is registered it is this machine's
    // daemon, whatever else a cross-flavour history left behind.
    let scm = stdout_of(&["service", "status", "--as-service"]);
    if matches!(
        scm.as_deref().and_then(scm_answer),
        Some(ScmAnswer::Running)
    ) {
        return ("scmService".to_string(), true);
    }
    let task = stdout_of(&["service", "status"]);
    classify_service_state(task.as_deref(), scm.as_deref())
}

/// What `roomlerd service status --as-service` answered: its
/// `Roomler: <InstalledStatus:?>` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScmAnswer {
    Running,
    /// Registered but not running (stopped, start/stop pending, other).
    Registered,
    NotInstalled,
}

/// #1681 — a probe is read from its ANSWER line only. `roomlerd` writes its
/// tracing INFO lines to STDOUT for these subcommands, and one of them —
/// logged on every start while this companion runs, i.e. on every probe —
/// says "migration skipped: desktop companion is running". The substring
/// match this replaced read that as the service state, so every Windows
/// host looked like a running per-user task: a SystemContext install (its
/// config under %PROGRAMDATA%) read the per-user config and showed "Not
/// enrolled", and the Welcome offered a perMachine host userspace mode.
fn answer_line(stdout: &str, prefix: &str) -> Option<String> {
    stdout.lines().rev().find_map(|line| {
        let lower = line.trim().to_ascii_lowercase();
        lower
            .strip_prefix(prefix)
            .map(|rest| rest.trim().to_string())
    })
}

fn scm_answer(stdout: &str) -> Option<ScmAnswer> {
    // `println!("{}: {:?}", NEW_SERVICE_NAME, status)` — "Roomler: Running".
    let state = answer_line(stdout, "roomler:")?;
    Some(match state.as_str() {
        "running" => ScmAnswer::Running,
        "notinstalled" => ScmAnswer::NotInstalled,
        _ => ScmAnswer::Registered,
    })
}

/// #2871 — what `roomlerd service status --as-service` answered on Linux or
/// macOS: the root daemon's own line, `roomlerd.service: <active>, <enabled>`
/// or `com.roomler.daemon: <state>, <enabled>` (`service::system::status_line`
/// in roomlerd). ⚠️ Never a `Roomler:` line — that is Windows' SCM service,
/// and the daemon pins the distinction with a test of its own.
///
/// Until this was read, a Mac whose per-user LaunchAgent the FR-43 supervisor
/// had booted out (P2d) showed **"Service: not installed"** on the Overview
/// while its root daemon ran and served the mesh (measured on 0.4.169), and
/// every Linux system install read the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemAnswer {
    Running,
    /// Registered but not running: `not loaded, enabled` after a
    /// `service stop --as-service` on a Mac, `inactive, enabled` on Linux.
    Registered,
    /// Neither loaded nor enabled: no root daemon here.
    NotInstalled,
}

fn system_answer(stdout: &str) -> Option<SystemAnswer> {
    let rest = answer_line(stdout, "com.roomler.daemon:")
        .or_else(|| answer_line(stdout, "roomlerd.service:"))?;
    let (state, enabled) = match rest.split_once(',') {
        Some((s, e)) => (s.trim(), e.trim()),
        None => (rest.trim(), ""),
    };
    Some(match state {
        // launchd's `state = running`; systemd's `is-active` = active.
        "running" | "active" => SystemAnswer::Running,
        // Not even registered — the only answer a Mac with the LaunchAgent
        // alone (the pre-FR-43 layout) ever gives for the root daemon.
        "not loaded" | "not-found" if enabled != "enabled" => SystemAnswer::NotInstalled,
        _ => SystemAnswer::Registered,
    })
}

/// `service status` prints `Auto-start: installed | not installed | unknown`
/// (`AutostartStatus`'s Display) — it never says "running". Whether the
/// task's daemon runs is not in its answer, so it is not claimed.
fn classify_service_state(task: Option<&str>, scm: Option<&str>) -> (String, bool) {
    // One `--as-service` answer, read for both shapes: `Roomler:` is Windows'
    // SCM service, the unit line is a Unix root daemon (#2871). A host prints
    // one or the other, never both.
    let system = scm.and_then(system_answer);
    let scm = scm.and_then(scm_answer);
    let task_installed =
        task.and_then(|t| answer_line(t, "auto-start:")).as_deref() == Some("installed");
    let (kind, running) = match (scm, system, task_installed) {
        // A running machine-wide service IS this machine's daemon.
        (Some(ScmAnswer::Running), _, _) => ("scmService", true),
        // #2871 — and so is a running root daemon: on a supervised Mac it also
        // owns the GUI worker the companion talks to, and the LaunchAgent that
        // the per-user probe looks for is gone by design (FR-43 P2d).
        (_, Some(SystemAnswer::Running), _) => ("systemDaemon", true),
        // Otherwise a registered per-user task is (a stopped SCM service
        // beside it is a cross-flavour leftover).
        (_, _, true) => ("scheduledTask", false),
        (Some(ScmAnswer::Registered), _, false) => ("scmService", false),
        (_, Some(SystemAnswer::Registered), false) => ("systemDaemon", false),
        _ => ("none", false),
    };
    (kind.to_string(), running)
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
pub(crate) fn agent_exe_path() -> Result<PathBuf, String> {
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

    /// A `status` answer shaped like the one the SystemContext daemon gave the
    /// signed-in user over the LocalAPI in the field (#1701).
    fn daemon_status(node_id: &str, tenant: Option<&str>) -> NodeStatus {
        let tenant_json = tenant
            .map(|t| format!(r#","tenant_id":"{t}""#))
            .unwrap_or_default();
        serde_json::from_str(&format!(
            r#"{{"node_id":"{node_id}","name":"vmtest-w-ins-sys-104457","version":"0.4.104",
                "mode":"service","connected":true{tenant_json},
                "orgs":[{{"label":"primary","server_url":"https://roomler.ai","agent_id":"{node_id}",
                          "primary":true,"enabled":true,"connected":true}}]}}"#
        ))
        .expect("a NodeStatus the daemon could send")
    }

    fn report(enrolled: bool) -> StatusReport {
        StatusReport {
            enrolled,
            agent_id: enrolled.then(|| "from-file".to_string()),
            tenant_id: enrolled.then(|| "tenant-from-file".to_string()),
            server_url: enrolled.then(|| "https://file.example".to_string()),
            device_name: enrolled.then(|| "file-name".to_string()),
            agent_version: "0.4.104".to_string(),
            config_schema_version: None,
            service_running: true,
            service_kind: "scmService".to_string(),
            attention: None,
            attention_message: None,
            attention_reason: None,
            log_dir: String::new(),
            config_dir: String::new(),
            config_split: false,
        }
    }

    // #1701 — a SystemContext install keeps its config where only SYSTEM +
    // Administrators may read it (W4(c)), so the companion — the signed-in
    // user — cannot load it. The daemon, asked over the LocalAPI, says it is
    // enrolled; the companion must believe it, not print "Not enrolled yet".
    #[test]
    fn an_unreadable_config_is_answered_by_the_daemon() {
        let mut r = report(false);
        let id = daemon_identity(&daemon_status(
            "6ab7a24f352abff7547c5062",
            Some("6a975e17971e2d55ddcdedb2"),
        ));
        assert!(id.is_some(), "an enrolled daemon names its node and tenant");
        apply_daemon_identity(&mut r, id.unwrap());
        assert!(r.enrolled);
        assert_eq!(r.agent_id.as_deref(), Some("6ab7a24f352abff7547c5062"));
        assert_eq!(r.tenant_id.as_deref(), Some("6a975e17971e2d55ddcdedb2"));
        assert_eq!(r.server_url.as_deref(), Some("https://roomler.ai"));
        assert_eq!(r.device_name.as_deref(), Some("vmtest-w-ins-sys-104457"));
    }

    #[test]
    fn a_daemon_without_a_node_or_tenant_is_not_an_enrollment() {
        assert!(daemon_identity(&daemon_status("", Some("t"))).is_none());
        assert!(daemon_identity(&daemon_status("6ab7", None)).is_none());
    }

    #[test]
    fn a_readable_config_stays_the_answer() {
        // The file is the first source — the only one while the daemon is
        // down — and a daemon answer never overwrites it.
        let mut r = report(true);
        apply_daemon_identity(
            &mut r,
            daemon_identity(&daemon_status("6ab7", Some("t"))).unwrap(),
        );
        assert_eq!(r.agent_id.as_deref(), Some("from-file"));
        assert_eq!(r.server_url.as_deref(), Some("https://file.example"));
    }

    // #1681 — stdout as `roomlerd service status [--as-service]` really prints
    // it: tracing INFO lines (ANSI and all) on STDOUT, the answer last. The
    // noise line is the one every probe carries while this companion runs.
    const NOISE: &str = "\u{1b}[2m2026-09-26T08:23:47.814093Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \
        appdirs legacy-tree migration \u{1b}[3mnote\u{1b}[0m\u{1b}[2m=\u{1b}[0mmigration skipped: \
        desktop companion is running (will retry next start)\n\
        \u{1b}[2m2026-09-26T08:23:47.814973Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m config: resolved load path\n";

    fn out(answer: &str) -> String {
        format!("{NOISE}{answer}\n")
    }

    #[test]
    fn a_running_machine_wide_service_is_not_read_as_a_per_user_task() {
        // The SystemContext / attended perMachine host the vmtest probe caught:
        // no task, the SCM service running, the noise in BOTH outputs.
        let got = classify_service_state(
            Some(&out("Auto-start: not installed")),
            Some(&out("Roomler: Running")),
        );
        assert_eq!(got, ("scmService".to_string(), true));
    }

    #[test]
    fn a_per_user_task_is_read_from_its_answer_line() {
        let got = classify_service_state(
            Some(&out("Auto-start: installed")),
            Some(&out("Roomler: NotInstalled")),
        );
        assert_eq!(got, ("scheduledTask".to_string(), false));
        // `--as-service` failing outright (no answer line at all) is the same.
        let got = classify_service_state(Some(&out("Auto-start: installed")), Some(NOISE));
        assert_eq!(got, ("scheduledTask".to_string(), false));
    }

    #[test]
    fn log_noise_alone_is_no_service_at_all() {
        // Neither probe answered: the word "running" in a log line is not a state.
        assert_eq!(
            classify_service_state(Some(NOISE), Some(NOISE)),
            ("none".to_string(), false)
        );
        assert_eq!(
            classify_service_state(
                Some(&out("Auto-start: not installed")),
                Some(&out("Roomler: NotInstalled"))
            ),
            ("none".to_string(), false)
        );
        assert_eq!(
            classify_service_state(None, None),
            ("none".to_string(), false)
        );
    }

    #[test]
    fn a_stopped_or_pending_machine_wide_service_is_still_the_service() {
        for state in ["Stopped", "StartPending", "StopPending", "Other(7)"] {
            assert_eq!(
                classify_service_state(
                    Some(&out("Auto-start: not installed")),
                    Some(&out(&format!("Roomler: {state}")))
                ),
                ("scmService".to_string(), false),
                "{state}"
            );
        }
        // A registered task beside a STOPPED service is the daemon; a running
        // service beside a task is.
        assert_eq!(
            classify_service_state(
                Some(&out("Auto-start: installed")),
                Some(&out("Roomler: Stopped"))
            ),
            ("scheduledTask".to_string(), false)
        );
        assert_eq!(
            classify_service_state(
                Some(&out("Auto-start: installed")),
                Some(&out("Roomler: Running"))
            ),
            ("scmService".to_string(), true)
        );
    }

    /// #2871 — a Mac's or a Linux host's root daemon answers `--as-service`
    /// with its OWN unit's line, never `Roomler:`, and the companion read that
    /// as no service at all: a supervised Mac, whose LaunchAgent FR-43 P2d
    /// booted out, showed "Service: not installed" while its root daemon ran
    /// (0.4.169). The answers below are the daemon's `status_line` shapes.
    #[test]
    fn a_running_root_daemon_is_the_machines_daemon_on_unix() {
        let none = out("Auto-start: not installed");
        for running in [
            "com.roomler.daemon: running, enabled",
            "com.roomler.daemon: running, not enabled",
            "roomlerd.service: active, enabled",
        ] {
            assert_eq!(
                classify_service_state(Some(&none), Some(&out(running))),
                ("systemDaemon".to_string(), true),
                "{running}"
            );
        }
        // Registered but stopped (`service stop --as-service`): the daemon,
        // not running.
        for stopped in [
            "com.roomler.daemon: not loaded, enabled",
            "roomlerd.service: inactive, enabled",
            "roomlerd.service: failed, enabled",
        ] {
            assert_eq!(
                classify_service_state(Some(&none), Some(&out(stopped))),
                ("systemDaemon".to_string(), false),
                "{stopped}"
            );
        }
        // Neither loaded nor enabled: no root daemon — the only answer a Mac
        // with the LaunchAgent alone ever gives, and the pre-#2871 reading.
        for absent in [
            "com.roomler.daemon: not loaded, not enabled",
            "roomlerd.service: not-found, not-found",
        ] {
            assert_eq!(
                classify_service_state(Some(&none), Some(&out(absent))),
                ("none".to_string(), false),
                "{absent}"
            );
        }
        // An unsupervised two-half Mac: the LaunchAgent beside a RUNNING root
        // daemon is the running daemon's machine; beside a stopped one, the
        // LaunchAgent is what runs.
        let agent = out("Auto-start: installed");
        assert_eq!(
            classify_service_state(
                Some(&agent),
                Some(&out("com.roomler.daemon: running, enabled"))
            ),
            ("systemDaemon".to_string(), true)
        );
        assert_eq!(
            classify_service_state(
                Some(&agent),
                Some(&out("com.roomler.daemon: not loaded, enabled"))
            ),
            ("scheduledTask".to_string(), false)
        );
        // The unit line is never mistaken for Windows' SCM service, nor the
        // reverse: each shape reaches its own reader only.
        assert_eq!(
            scm_answer(&out("com.roomler.daemon: running, enabled")),
            None
        );
        assert_eq!(system_answer(&out("Roomler: Running")), None);
    }

    fn brief(node_id: &str, privileged: Option<bool>, config_path: &str) -> DaemonBrief {
        DaemonBrief {
            name: format!("mac-{node_id}"),
            node_id: node_id.to_string(),
            version: "0.4.169".to_string(),
            config_path: Some(config_path.to_string()),
            privileged,
        }
    }

    const USER_FILE: &str =
        "/Users/someone/Library/Application Support/live.roomler.roomler/config.toml";
    const ROOT_FILE: &str = "/etc/roomler/config.toml";

    /// #2871 — the banner is for exactly one layout: an UNPRIVILEGED daemon
    /// reached, and a ROOT daemon of another identity behind the person socket.
    #[test]
    fn only_a_per_user_half_beside_a_root_half_is_two_halves() {
        let user = brief("u", Some(false), USER_FILE);
        let root = brief("d", Some(true), ROOT_FILE);
        assert!(two_halves(&user, Some(&root)), "FR-43's P2 layout");
        // A per-user install: no root half answers, nothing to say.
        assert!(!two_halves(&user, None));
        // The reached daemon IS root (a Linux system install's person socket,
        // a companion run under sudo): it edits the machine's own file.
        assert!(!two_halves(&root, Some(&root)));
        // The same node behind both sockets is one daemon (the root daemon's
        // legacy per-user bind), not two halves.
        assert!(!two_halves(&user, Some(&brief("u", Some(true), ROOT_FILE))));
        // A daemon too old to say which account it runs as claims nothing.
        assert!(!two_halves(&brief("u", None, USER_FILE), Some(&root)));
        // A "machine" daemon that is not root is not the root half.
        assert!(!two_halves(
            &user,
            Some(&brief("d", Some(false), USER_FILE))
        ));
    }

    /// #2871 — both notes name both files, so a person can tell which one
    /// `roomler config ls` printed, and the administrator's command for the
    /// root half's switches. Design A's note says the page shows both halves
    /// and which rows change here; the legacy note (a root half that refuses
    /// its listing) says the rows are the per-user half's alone.
    #[test]
    fn the_two_halves_notes_name_both_files_and_the_root_command() {
        let user = brief("u", Some(false), USER_FILE);
        let root = brief("d", Some(true), ROOT_FILE);
        let common = [
            "per-user half",
            "root half",
            USER_FILE,
            ROOT_FILE,
            "mac-u",
            "mac-d",
            "sudo roomler config set",
            "exec_enabled",
            "ssh_enabled",
            "remote_config_enabled",
        ];
        let note = two_halves_note(&user, &root);
        for must in common
            .iter()
            .chain(&["shows both", "change here", "external access"])
        {
            assert!(note.contains(must), "the note must say {must:?}: {note}");
        }
        let legacy = two_halves_legacy_note(&user, &root);
        for must in common {
            assert!(
                legacy.contains(must),
                "the legacy note must say {must:?}: {legacy}"
            );
        }
        assert!(!legacy.contains("change here"), "{legacy}");
        // An older daemon without `config_path` still gets a sentence, not a
        // hole where the file name was.
        let old = brief("u", Some(false), USER_FILE);
        let old = DaemonBrief {
            config_path: None,
            ..old
        };
        assert!(two_halves_note(&old, &root).contains("its own config.toml"));
        assert!(two_halves_legacy_note(&old, &root).contains("its own config.toml"));
    }

    /// #2891 — the root-only layouts, and everything that is NOT one. The
    /// reached daemon must SAY it runs as root; the companion must not; and
    /// Windows' pipe admits its interactive users, so nothing is root-only
    /// there. A root daemon named with nothing reached is a Mac with no
    /// per-user half.
    #[test]
    fn only_a_root_daemon_reached_unprivileged_is_root_only() {
        let user = brief("u", Some(false), USER_FILE);
        let root = brief("d", Some(true), ROOT_FILE);
        // A Linux system install: the root daemon, over its person socket.
        assert_eq!(
            root_daemon_only(Some(&root), None, false, false),
            Some(&root)
        );
        // A Mac with a root half alone: nothing reached, the root half answers.
        assert_eq!(
            root_daemon_only(None, Some(&root), false, false),
            Some(&root)
        );
        // The companion itself runs as root: it reaches the daemon's own socket.
        assert_eq!(root_daemon_only(Some(&root), None, true, false), None);
        // Windows: the SCM service is privileged AND its pipe admits the user.
        assert_eq!(root_daemon_only(Some(&root), None, false, true), None);
        // A per-user daemon, with or without a root half beside it (#2871's
        // two halves): the page edits it.
        assert_eq!(root_daemon_only(Some(&user), None, false, false), None);
        assert_eq!(
            root_daemon_only(Some(&user), Some(&root), false, false),
            None
        );
        // Nothing reached and nothing root answers: no claim.
        assert_eq!(root_daemon_only(None, None, false, false), None);
        assert_eq!(root_daemon_only(None, Some(&user), false, false), None);
        // A daemon too old to say which account it runs as claims nothing.
        assert_eq!(
            root_daemon_only(Some(&brief("d", None, ROOT_FILE)), None, false, false),
            None
        );
        // The two layouts never overlap: FR-43's is two halves, not root-only.
        assert!(two_halves(&user, Some(&root)));
        assert_eq!(
            root_daemon_only(Some(&user), Some(&root), false, false),
            None
        );
    }

    /// #2891 — the notes name the daemon, its file and the administrator's
    /// command. #2871 (design A): the note for a daemon that served its rows
    /// says what changes here and which rows are marked; the legacy note, for
    /// a daemon that refused them, says why the app has no path.
    #[test]
    fn the_root_only_notes_name_the_file_and_the_sudo_commands() {
        let root = brief("d", Some(true), ROOT_FILE);
        let note = root_only_note(&root);
        for must in [
            "mac-d",
            "0.4.169",
            ROOT_FILE,
            "sudo roomler config set <key> <value>",
            "change here",
            "external access",
            "marked administrator's",
            "exec_enabled",
            "ssh_enabled",
            "remote_config_enabled",
        ] {
            assert!(note.contains(must), "the note must say {must:?}: {note}");
        }
        let legacy = root_only_legacy_note(&root);
        for must in [
            "mac-d",
            "0.4.169",
            ROOT_FILE,
            "sudo roomler config ls",
            "sudo roomler config set <key> <value>",
            "person-at-the-device socket",
            "no configuration",
            "exec_enabled",
            "ssh_enabled",
            "remote_config_enabled",
        ] {
            assert!(
                legacy.contains(must),
                "the legacy note must say {must:?}: {legacy}"
            );
        }
        assert!(!legacy.contains("change here"), "{legacy}");
        // An older daemon without `config_path` still names a file.
        let old = DaemonBrief {
            config_path: None,
            ..root
        };
        assert!(root_only_note(&old).contains("/etc/roomler/config.toml"));
        assert!(root_only_legacy_note(&old).contains("/etc/roomler/config.toml"));
    }

    /// #2871 (design A) — which half of a two-half Mac a key is sent to, by
    /// this build's registry: the machine-side groups to the root half, the
    /// session-side ones (and a key this build has never heard of) to the
    /// per-user half, as the page always did. #2871's review — and a key this
    /// build does not know goes where the group its row was LISTED with puts
    /// it, the same decision the listing made.
    #[test]
    fn a_machine_side_key_goes_to_the_root_half_and_a_session_side_key_to_the_worker() {
        for key in [
            "exec_enabled",
            "ssh_enabled",
            "remote_config_enabled",
            "overlay_enabled",
            "external_access_enabled",
            "external_consent_mode",
            "auto_grant_session",
            "auto_update",
        ] {
            assert!(is_machine_side(key, ""), "{key} is the root half's");
        }
        for key in [
            "record_dir",
            "files_dir",
            "rc_clipboard",
            "privacy_mode_allowed",
            "lock_on_end",
            "display_match",
            "peer_auth",
            "keep_busy_enabled",
            "encoder_preference",
            "brand_new_key",
        ] {
            assert!(!is_machine_side(key, ""), "{key} is the session half's");
        }
        // A key a newer daemon lists and this build does not know: by the
        // group it came with — so the set follows the row (`merge_two_halves`
        // takes it from the root half for exactly these groups).
        assert!(is_machine_side("brand_new_key", "access"));
        assert!(is_machine_side("brand_new_key", "ssh"));
        assert!(!is_machine_side("brand_new_key", "files"));
        assert!(!is_machine_side("brand_new_key", "no_such_group"));
        // A key this build knows is placed by the registry, whatever a row says.
        assert!(!is_machine_side("rc_clipboard", "access"));
        assert!(is_machine_side("exec_enabled", "files"));
        let row = |key: &str, group: &str| {
            serde_json::from_value::<localapi::ConfigEntry>(serde_json::json!({
                "key": key, "value": "x", "kind": "string", "restart_required": true,
                "description": "", "group": group,
            }))
            .unwrap()
        };
        let merged = merge_two_halves(
            vec![row("rc_clipboard", "remote_desktop")],
            vec![
                row("brand_new_key", "access"),
                row("brand_new_key_2", "files"),
            ],
        );
        let keys: Vec<&str> = merged.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            keys,
            ["rc_clipboard", "brand_new_key"],
            "the listing took the unknown machine-side row from the root half"
        );
        for e in &merged {
            assert_eq!(
                is_machine_side(&e.key, &e.group),
                e.key == "brand_new_key",
                "{}: the set goes where the row came from",
                e.key
            );
        }
    }

    /// #2871 (design A) — a two-half Mac's rows: machine-side keys from the
    /// root half (its value, its `read_only` mark), session-side keys from the
    /// per-user half, in the per-user listing's order; a machine-side key only
    /// the root half lists is appended; a session-side key only the root half
    /// lists is dropped.
    #[test]
    fn a_two_half_listing_takes_machine_rows_from_the_root_half_and_session_rows_from_the_worker() {
        let entry = |key: &str, group: &str, value: &str, read_only: Option<&str>| {
            serde_json::from_value::<localapi::ConfigEntry>(serde_json::json!({
                "key": key, "value": value, "kind": "bool", "restart_required": false,
                "description": "", "group": group, "read_only": read_only,
            }))
            .unwrap()
        };
        // The per-user half's file, as #2871 measured it: `exec_enabled` on,
        // `ssh_enabled` off — while the root half served SSH.
        let worker = vec![
            entry("exec_enabled", "access", "true", None),
            entry("rc_clipboard", "remote_desktop", "false", None),
            entry("ssh_enabled", "ssh", "false", None),
            entry("record_dir", "files", "/Users/a/Movies", None),
        ];
        let root = vec![
            entry("exec_enabled", "access", "false", Some("root_only")),
            entry("rc_clipboard", "remote_desktop", "true", None),
            entry("ssh_enabled", "ssh", "true", Some("root_only")),
            entry("external_access_enabled", "access", "false", None),
            entry("record_dir", "files", "/var/root/Movies", None),
        ];
        let merged = merge_two_halves(worker, root);
        let keys: Vec<&str> = merged.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "exec_enabled",
                "rc_clipboard",
                "ssh_enabled",
                "record_dir",
                "external_access_enabled"
            ]
        );
        let by_key = |k: &str| merged.iter().find(|e| e.key == k).unwrap();
        // The root half's switches, as the root half has them, marked.
        assert_eq!(by_key("exec_enabled").value.as_deref(), Some("false"));
        assert_eq!(
            by_key("exec_enabled").read_only.as_deref(),
            Some("root_only")
        );
        assert_eq!(by_key("ssh_enabled").value.as_deref(), Some("true"));
        assert_eq!(
            by_key("ssh_enabled").read_only.as_deref(),
            Some("root_only")
        );
        // The external-access surface: the root half's, and editable there.
        assert_eq!(
            by_key("external_access_enabled").value.as_deref(),
            Some("false")
        );
        assert_eq!(by_key("external_access_enabled").read_only, None);
        // The session's keys stay the per-user half's, whatever the root has.
        assert_eq!(by_key("rc_clipboard").value.as_deref(), Some("false"));
        assert_eq!(
            by_key("record_dir").value.as_deref(),
            Some("/Users/a/Movies")
        );
        // No root half's rows (an older root half): the listing is the
        // per-user half's, untouched.
        let worker = vec![entry("exec_enabled", "access", "true", None)];
        assert_eq!(merge_two_halves(worker.clone(), Vec::new()), worker);
    }

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
        assert!(is_daemon_owned_key("record_remote_enabled"));
        assert!(is_daemon_owned_key("record_remote_audio"));
        assert!(!is_daemon_owned_key("exec_enabled"));
        assert!(!is_daemon_owned_key("enable_remote_browse"));
        // FR-84 D4 made `files_dir` daemon-owned as well (see
        // `a_daemon_refusal_of_files_dir_is_final`).
        assert!(is_daemon_owned_key("files_dir"));
        // FR-52 — the external-access surface: the console person's call,
        // which a write behind the daemon's back would route around.
        for key in [
            "external_access_enabled",
            "external_consent_mode",
            "external_max_permissions",
        ] {
            assert!(is_daemon_owned_key(key), "{key}");
        }
        let refusal = "only the person at this device's console can open it to people outside \
                       the organization, or change how";
        assert_eq!(
            explain_daemon_owned_error("external_access_enabled", refusal),
            refusal
        );
    }

    /// FR-85 P3b — the remote gates come from the service's own listing, and
    /// a service that predates them offers no toggle at all (rather than an
    /// "off" that could never be switched on).
    #[test]
    fn remote_gates_come_from_the_listing_and_are_absent_on_an_older_service() {
        let entry = |key: &str, value: Option<&str>| -> localapi::ConfigEntry {
            serde_json::from_value(serde_json::json!({
                "key": key, "value": value, "kind": "bool",
                "restart_required": false, "description": "",
            }))
            .unwrap()
        };
        assert_eq!(remote_gates(&[entry("record_dir", None)]), None);
        assert_eq!(
            remote_gates(&[
                entry("record_remote_enabled", Some("true")),
                entry("record_remote_audio", Some("false")),
            ]),
            Some(RemoteRecordingGates {
                enabled: true,
                audio: false
            })
        );
        assert_eq!(
            remote_gates(&[entry("record_remote_enabled", Some("false"))]),
            Some(RemoteRecordingGates {
                enabled: false,
                audio: false
            })
        );
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

    fn two_org_config() -> AgentConfig {
        serde_json::from_value(serde_json::json!({
            "server_url": "https://roomler.ai/",
            "agent_token": "tok",
            "agent_id": "a1a1a1a1a1a1a1a1a1a1a1a1",
            "tenant_id": "AAAAAAAAAAAAAAAAAAAAAAAA",
            "machine_id": "m",
            "machine_name": "neo",
            "orgs": [{
                "label": "acme",
                "server_url": "https://acme.example",
                "agent_token": "tok-acme",
                "agent_id": "b2b2b2b2b2b2b2b2b2b2b2b2",
                "tenant_id": "bbbbbbbbbbbbbbbbbbbbbbbb"
            }, {
                "label": "broken",
                "server_url": "ftp://nope",
                "agent_token": "t",
                "agent_id": "c3c3c3c3c3c3c3c3c3c3c3c3",
                "tenant_id": "not-hex"
            }]
        }))
        .expect("a minimal two-org config parses")
    }

    /// FR-84 D5c — "View screen" on a device of a SECONDARY org opens that
    /// org's server and tenant, read from this device's own `[[orgs]]` entry;
    /// no org (or `primary`) is the primary, exactly as before.
    #[test]
    fn remote_target_follows_the_devices_org() {
        let cfg = two_org_config();
        let primary = (
            "https://roomler.ai".to_string(),
            "aaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        );
        assert_eq!(remote_target(&cfg, None).unwrap(), primary);
        assert_eq!(remote_target(&cfg, Some("")).unwrap(), primary);
        assert_eq!(remote_target(&cfg, Some("primary")).unwrap(), primary);
        assert_eq!(
            remote_target(&cfg, Some("acme")).unwrap(),
            (
                "https://acme.example".to_string(),
                "bbbbbbbbbbbbbbbbbbbbbbbb".to_string()
            )
        );
        let unknown = remote_target(&cfg, Some("ghost")).unwrap_err();
        assert!(unknown.contains("no enrollment labelled"), "{unknown}");
        // A malformed entry is refused, never opened.
        assert!(remote_target(&cfg, Some("broken")).is_err());
    }

    /// FR-84 D5c — a rejected `cmd_devices` is `{code, message, status?}`,
    /// with the cause the page keys its fallback on.
    #[test]
    fn directory_failures_are_json_with_the_cause() {
        let parse = |s: String| -> serde_json::Value { serde_json::from_str(&s).unwrap() };

        let v = parse(directory_failure(
            "devices",
            "devices",
            localapi::DirectoryError::Upstream {
                code: "unauthorized".into(),
                message: "refused".into(),
                status: Some(401),
            },
        ));
        assert_eq!(
            v,
            serde_json::json!({"code": "unauthorized", "message": "refused", "status": 401})
        );

        let v = parse(directory_failure(
            "devices",
            "devices",
            localapi::DirectoryError::UnsupportedDaemon {
                message: "bad request: unknown variant `devices`".into(),
            },
        ));
        assert_eq!(v["code"], "unsupported_daemon");
        assert!(v.get("status").is_none());
        assert!(v["message"].as_str().unwrap().contains("predates"));

        let v = parse(directory_failure(
            "mesh",
            "connect",
            std::io::Error::new(std::io::ErrorKind::NotFound, "no pipe").into(),
        ));
        assert_eq!(v["code"], "daemon_unreachable");
        assert_eq!(
            v["message"],
            "device service not running (no LocalAPI endpoint)"
        );
    }

    /// FR-84 D4 — `files_dir` is DAEMON-OWNED: once a daemon answered, its
    /// refusal is final. It checks the folder against the identity it writes
    /// as (SYSTEM on a machine install), the console user and its own disk;
    /// the direct-file fallback checks only the shape, so running it after a
    /// refusal would save the refused value wherever the user can write the
    /// config and swallow the reason — and for a daemon too old to know the
    /// key, save a value it ignores and call it "in effect now". Matched
    /// exactly; FR-85's `record_*` keys are unchanged; other keys still fall
    /// back.
    #[test]
    fn a_daemon_refusal_of_files_dir_is_final() {
        assert!(is_daemon_owned_key("files_dir"));
        assert!(
            !is_daemon_owned_key("files_dir_x"),
            "equality, not a prefix"
        );
        assert!(!is_daemon_owned_key("overlay_quic"));
        assert!(is_daemon_owned_key("record_dir"), "FR-85's rule, unchanged");

        let refused = "localapi error: the service runs as SYSTEM/root, so files_dir must be inside the active user's profile (C:\\Users\\alice); D:\\Drops is outside it";
        // What the person reads is the daemon's own sentence.
        assert_eq!(
            explain_daemon_owned_error("files_dir", refused),
            "the service runs as SYSTEM/root, so files_dir must be inside the active user's profile (C:\\Users\\alice); D:\\Drops is outside it"
        );
        let console = "localapi error: only the person at this device's console can change where incoming files land";
        assert_eq!(
            explain_daemon_owned_error("files_dir", console),
            "only the person at this device's console can change where incoming files land"
        );
        // An older daemon (key or verb unknown): a plain "update", not the
        // raw parse error — and still no direct write.
        for old in [
            "localapi error: unknown or non-editable config key \"files_dir\"",
            "localapi error: bad request: unknown variant `config_set`",
        ] {
            assert!(
                explain_daemon_owned_error("files_dir", old).contains("predates this setting"),
                "{old}"
            );
        }
        assert_eq!(daemon_message("no prefix here"), "no prefix here");
    }

    /// FR-84 D4 — denylist entries land in the matrix by the daemon's own
    /// split (`VideoBackend::from_ffmpeg_name`): codec before the first `_`,
    /// FFmpeg's `d3d12va` is the `d3d12` column; anything else is not
    /// placed (and is still listed verbatim by the card).
    #[test]
    fn denied_entries_are_placed_like_the_daemon_splits_them() {
        let p = place_denied("hevc_qsv:yuv444").unwrap();
        assert_eq!(
            (p.codec.as_str(), p.backend.as_str(), p.chroma.as_str()),
            ("hevc", "qsv", "yuv444")
        );
        assert_eq!(p.entry, "hevc_qsv:yuv444");
        assert_eq!(place_denied("av1_d3d12va:yuv420").unwrap().backend, "d3d12");
        assert_eq!(
            place_denied(" av1_vulkan:yuv420 ").unwrap().backend,
            "vulkan"
        );
        for unplaceable in [
            "hevc_qsv",             // no chroma
            "hevc_qsv:yuv422",      // no such chroma
            "mpeg2_qsv:yuv420",     // no such codec
            "hevc_newthing:yuv420", // a backend newer than this app
            "none",
            "",
        ] {
            assert_eq!(place_denied(unplaceable), None, "{unplaceable:?}");
        }
        let view = EncoderCapsView::from_summary(localapi::EncoderCapsSummary {
            state: "ready".into(),
            denied: vec!["hevc_qsv:yuv444".into(), "bogus".into()],
            ..Default::default()
        });
        assert!(view.available);
        assert_eq!(view.denied_cells.len(), 1);
        assert_eq!(
            view.caps.as_ref().unwrap().denied.len(),
            2,
            "the verbatim list travels whole"
        );
    }

    /// FR-84 D4 — a picked folder inside the picker's own profile is stored
    /// `~`-relative (the daemon expands it per user at drop time); anything
    /// else as picked.
    #[test]
    fn a_picked_folder_inside_home_is_stored_relative_to_it() {
        if cfg!(windows) {
            let home = Some(r"C:\Users\alice");
            assert_eq!(
                files_dir_value_for(Path::new(r"C:\Users\alice\Desktop\In"), home),
                r"~\Desktop\In"
            );
            assert_eq!(files_dir_value_for(Path::new(r"C:\Users\alice"), home), "~");
            assert_eq!(
                files_dir_value_for(Path::new(r"D:\Drops"), home),
                r"D:\Drops"
            );
            assert_eq!(
                files_dir_value_for(Path::new(r"C:\Users\alice2\x"), home),
                r"C:\Users\alice2\x"
            );
            assert_eq!(
                files_dir_value_for(Path::new(r"C:\Users\alice\x"), None),
                r"C:\Users\alice\x",
                "no known home: as picked"
            );
        } else {
            let home = Some("/home/bob");
            assert_eq!(files_dir_value_for(Path::new("/home/bob/in"), home), "~/in");
            assert_eq!(files_dir_value_for(Path::new("/srv/in"), home), "/srv/in");
        }
    }

    /// #2713 — the Linux opener asks for the app registered for a link's
    /// SCHEME, so the scheme has to come out exactly; anything that is not a
    /// link yields none and becomes an error, never a guess.
    #[test]
    fn a_link_scheme_comes_out_exactly() {
        assert_eq!(
            url_scheme("https://roomler.ai/tenant/t/agent/a/remote"),
            Some("https")
        );
        assert_eq!(url_scheme("http://127.0.0.1:5000/"), Some("http"));
        assert_eq!(url_scheme("web+roomler:x"), Some("web+roomler"));
        for not_a_link in [
            "",
            "https",
            "https:",
            "://x",
            "1https://x",
            "ht tps://x",
            "/tmp/a.html",
        ] {
            assert_eq!(url_scheme(not_a_link), None, "{not_a_link:?}");
        }
    }

    /// #2713 — with no browser the person reads what to do about it, whatever
    /// the scheme's case; another scheme is named as it is.
    #[test]
    fn no_browser_says_what_to_do() {
        for scheme in ["https", "http", "HTTPS"] {
            let m = no_handler_message(scheme);
            assert!(m.starts_with("No web browser is set up"), "{m}");
            assert!(m.contains("default web browser"), "{m}");
        }
        assert_eq!(
            no_handler_message("mailto"),
            "Nothing on this computer opens mailto: links."
        );
    }
}

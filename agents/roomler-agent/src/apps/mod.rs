//! Remote **app selection & launch** for virtual-desktop hosts.
//!
//! When a browser controls a headless agent that runs in built-in
//! virtual-desktop mode (`ROOMLER_AGENT_VIRTUAL_DESKTOP=1` →
//! Xvfb + WM + startup apps, see [`crate::virtual_desktop`]), the
//! operator has no way to see what's running on that desktop, focus a
//! specific window, **attach to an existing shell session**, or **start
//! a new one**. This module adds that: list windows, focus one, and
//! launch a new *allowlisted* app.
//!
//! ## Transport
//! Pure P2P over the existing WebRTC `control` data channel — no server
//! change. [`crate::peer::attach_control_handler`] routes three
//! envelopes here and sends the returned reply back over the same DC,
//! mirroring the `rc:logs-fetch` request/response precedent:
//!
//! ```text
//! rc:apps.list   {id}                    → rc:apps.list.reply   {id, ok, supported, windows, launchable}
//! rc:apps.focus  {id, window_id}         → rc:apps.focus.reply  {id, ok, error?}
//! rc:apps.launch {id, app_key}           → rc:apps.launch.reply {id, ok, window_id?, session?, error?}
//! ```
//!
//! `window_id` is an **opaque string** the browser only round-trips
//! (X11 hex on Linux, HWND decimal on Windows). Launch takes an
//! allowlist **key** only — never a command line — so a compromised
//! browser can start only operator-approved apps, and `command` is run
//! as argv (no shell), so there is no injection surface.
//!
//! ## Platform backends
//! * `linux` — `wmctrl` (list/focus) + `tmux` (bash sessions) + `xterm`.
//!   The flagship: a bash "session" is a tmux session, surfaced as an
//!   xterm attached to it — survives agent restart/disconnect and is
//!   ssh-attachable. Windows are matched back to their session/app via a
//!   `roomler:tmux:<s>` / `roomler:app:<key>` xterm-title convention.
//! * `windows` — Phase 2 (`EnumWindows` / `SetForegroundWindow` /
//!   `CreateProcessAsUser` in the active session). Not built yet.
//!
//! The pure parsers + resolvers live here (portable, unit-tested); the
//! platform I/O lives in the cfg-gated submodules.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

// ---------------------------------------------------------------------------
// Wire types (agent → browser)
// ---------------------------------------------------------------------------

/// One enumerated desktop window (an entry in `rc:apps.list.reply`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WindowInfo {
    /// Opaque window handle: X11 hex id (`0x03400007`) on Linux, HWND
    /// decimal on Windows. The browser round-trips it in `rc:apps.focus`.
    pub window_id: String,
    /// Human-friendly title (our `roomler:*` prefix stripped).
    pub title: String,
    /// Matched allowlist key, when the window is a known launched app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_key: Option<String>,
    /// tmux session name (Linux bash flagship); absent for GUI windows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Whether this is the active window on the desktop.
    pub focused: bool,
}

/// An allowlisted app the browser may launch (an entry in `launchable`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaunchableApp {
    pub key: String,
    pub label: String,
}

/// Outcome of a launch, used to build `rc:apps.launch.reply`. Both
/// fields are best-effort — a backend that can't resolve the new
/// window synchronously returns `None` and the browser re-lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchOutcome {
    pub window_id: Option<String>,
    pub session: Option<String>,
}

/// An allowlist entry resolved from config by [`resolve_app`] and handed
/// to a backend's [`WindowManager::launch`]. The command is already
/// validated to be non-empty; it is run as argv (never via a shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedApp {
    pub key: String,
    pub command: Vec<String>,
    /// Wrap the command in a terminal (Linux `xterm -e …`) — for TUIs.
    pub terminal: bool,
    /// This entry is a tmux-backed shell: launch creates/attaches a
    /// persistent tmux session instead of a bare process.
    pub tmux: bool,
}

/// Platform window-manager backend. All methods block (shell-out / FFI),
/// so [`crate::peer`] invokes [`handle_control_message`] from
/// `tokio::task::spawn_blocking`.
pub trait WindowManager: Send + Sync {
    /// Enumerate the desktop's windows (already classified against the
    /// title convention / allowlist).
    fn list(&self) -> Result<Vec<WindowInfo>>;
    /// Raise + focus a previously-enumerated window by its opaque id.
    fn focus(&self, window_id: &str) -> Result<()>;
    /// Launch (or, for a tmux entry, create+attach) an allowlisted app.
    fn launch(&self, app: &ResolvedApp) -> Result<LaunchOutcome>;
}

// ---------------------------------------------------------------------------
// Config (`[virtual_desktop_apps]` in the agent config.toml)
// ---------------------------------------------------------------------------

/// Remote app-launch config. Default: enabled with a seeded bash/tmux
/// entry so a fresh VD host offers "New bash session" out of the box.
/// Operators add htop/mc/… per host. Mirrors the [`crate::tunnel::acl`]
/// allowlist pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualDesktopAppsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Friendly key → command spec. The browser sends only the KEY.
    #[serde(default)]
    pub allowlist: BTreeMap<String, AppSpec>,
}

/// One launchable app. `command` is argv (no shell interpolation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSpec {
    /// argv[0..] — executed directly, never through a shell.
    pub command: Vec<String>,
    /// Display label; falls back to the key when absent.
    #[serde(default)]
    pub label: Option<String>,
    /// Wrap in a terminal (Linux `xterm -e …`) — for TUI apps.
    #[serde(default)]
    pub terminal: bool,
    /// tmux-backed shell: launch creates/attaches a persistent session.
    #[serde(default)]
    pub tmux: bool,
}

fn default_true() -> bool {
    true
}

impl Default for VirtualDesktopAppsConfig {
    fn default() -> Self {
        let mut allowlist = BTreeMap::new();
        // Seed an OS-appropriate shell so a fresh host has one launchable
        // entry out of the box; operators add more in the TOML.
        #[cfg(target_os = "windows")]
        allowlist.insert(
            "cmd".to_string(),
            AppSpec {
                command: vec!["cmd.exe".to_string()],
                label: Some("New Command Prompt".to_string()),
                terminal: false,
                tmux: false,
            },
        );
        #[cfg(not(target_os = "windows"))]
        allowlist.insert(
            "bash".to_string(),
            AppSpec {
                command: vec!["bash".to_string()],
                label: Some("New bash session".to_string()),
                terminal: true,
                tmux: true,
            },
        );
        Self {
            enabled: true,
            allowlist,
        }
    }
}

// ---------------------------------------------------------------------------
// Process-global config install (mirrors `files::set_remote_browse_enabled`)
// ---------------------------------------------------------------------------

static APPS_CONFIG: OnceLock<VirtualDesktopAppsConfig> = OnceLock::new();

/// Install the config once at startup (from `main::run_cmd`). Later calls
/// are ignored — the config is immutable for the process lifetime, same
/// as the remote-browse flag.
pub fn set_apps_config(cfg: VirtualDesktopAppsConfig) {
    let _ = APPS_CONFIG.set(cfg);
}

/// The installed config, or the default (seeded bash entry) if
/// `set_apps_config` was never called (e.g. unit tests, old flows).
pub fn apps_config() -> VirtualDesktopAppsConfig {
    APPS_CONFIG.get().cloned().unwrap_or_default()
}

/// True when this process can actually manage a desktop AND apps are
/// enabled — the signal the caps builder advertises to the browser.
/// Linux: only in virtual-desktop mode (a `DISPLAY` is set). Windows:
/// Phase 2 (no backend yet) → false.
pub fn apps_supported() -> bool {
    if !apps_config().enabled {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        // Linux: only in virtual-desktop mode (a DISPLAY is set).
        std::env::var_os("DISPLAY").is_some()
    }
    #[cfg(target_os = "windows")]
    {
        // Windows: the agent always drives the active user's desktop.
        true
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

/// Construct the platform backend for the given X11 `display` (Linux),
/// or `None` when apps can't be managed on this host/build. Windows lands
/// in Phase 2.
pub fn backend(display: Option<&str>) -> Option<Box<dyn WindowManager>> {
    #[cfg(target_os = "linux")]
    {
        display.map(|d| Box::new(linux::LinuxWm::new(d.to_string())) as Box<dyn WindowManager>)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = display;
        Some(Box::new(windows::WindowsWm) as Box<dyn WindowManager>)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = display;
        None
    }
}

// ---------------------------------------------------------------------------
// Control-DC entrypoint
// ---------------------------------------------------------------------------

/// Handle one `rc:apps.*` control-DC envelope and return the reply
/// `Value` to send back. Blocking (shells out / FFI) — call from
/// `spawn_blocking`. Never panics: every path returns a well-formed
/// `*.reply`.
pub fn handle_control_message(val: &Value) -> Value {
    let cfg = apps_config();
    // The VD display is set process-global by `virtual_desktop` at
    // startup (`main.rs`); the backend dials it. When apps are disabled
    // we build no backend → replies say `supported:false`.
    let display = std::env::var("DISPLAY").ok();
    let be = if cfg.enabled {
        backend(display.as_deref())
    } else {
        None
    };
    dispatch(val, &cfg, be.as_deref())
}

/// Pure dispatch over a supplied (possibly fake) backend — the unit-test
/// seam.
fn dispatch(
    val: &Value,
    cfg: &VirtualDesktopAppsConfig,
    backend: Option<&dyn WindowManager>,
) -> Value {
    let id = msg_id(val);
    match val.get("t").and_then(|v| v.as_str()).unwrap_or("") {
        "rc:apps.list" => build_list_reply(id, cfg, backend),
        "rc:apps.focus" => build_focus_reply(id, backend, val),
        "rc:apps.launch" => build_launch_reply(id, cfg, backend, val),
        other => json!({
            "t": "rc:apps.error", "id": id, "ok": false,
            "error": format!("unknown apps message: {other}"),
        }),
    }
}

fn msg_id(val: &Value) -> Option<String> {
    val.get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn build_list_reply(
    id: Option<String>,
    cfg: &VirtualDesktopAppsConfig,
    backend: Option<&dyn WindowManager>,
) -> Value {
    let Some(backend) = backend else {
        return json!({
            "t": "rc:apps.list.reply", "id": id, "ok": true, "supported": false,
            "windows": [], "launchable": [],
        });
    };
    match backend.list() {
        Ok(mut windows) => {
            // Defensive bound so a pathological desktop can't blow the
            // 65536 SCTP single-message limit (200 × ~120 B ≈ 24 KB).
            windows.truncate(200);
            json!({
                "t": "rc:apps.list.reply", "id": id, "ok": true, "supported": true,
                "windows": windows, "launchable": launchable_list(cfg),
            })
        }
        Err(e) => json!({
            "t": "rc:apps.list.reply", "id": id, "ok": false, "supported": true,
            "windows": [], "launchable": launchable_list(cfg), "error": format!("{e:#}"),
        }),
    }
}

fn build_focus_reply(
    id: Option<String>,
    backend: Option<&dyn WindowManager>,
    val: &Value,
) -> Value {
    let Some(backend) = backend else {
        return action_error("rc:apps.focus.reply", id, "apps not supported on this host");
    };
    let Some(window_id) = val.get("window_id").and_then(|v| v.as_str()) else {
        return action_error("rc:apps.focus.reply", id, "missing window_id");
    };
    match backend.focus(window_id) {
        Ok(()) => json!({ "t": "rc:apps.focus.reply", "id": id, "ok": true }),
        Err(e) => action_error("rc:apps.focus.reply", id, &format!("{e:#}")),
    }
}

fn build_launch_reply(
    id: Option<String>,
    cfg: &VirtualDesktopAppsConfig,
    backend: Option<&dyn WindowManager>,
    val: &Value,
) -> Value {
    let Some(backend) = backend else {
        return action_error(
            "rc:apps.launch.reply",
            id,
            "apps not supported on this host",
        );
    };
    let Some(app_key) = val.get("app_key").and_then(|v| v.as_str()) else {
        return action_error("rc:apps.launch.reply", id, "missing app_key");
    };
    let Some(resolved) = resolve_app(cfg, app_key) else {
        return action_error("rc:apps.launch.reply", id, "app_key not in allowlist");
    };
    match backend.launch(&resolved) {
        Ok(outcome) => json!({
            "t": "rc:apps.launch.reply", "id": id, "ok": true,
            "app_key": app_key, "window_id": outcome.window_id, "session": outcome.session,
        }),
        Err(e) => action_error("rc:apps.launch.reply", id, &format!("{e:#}")),
    }
}

fn action_error(t: &str, id: Option<String>, error: &str) -> Value {
    json!({ "t": t, "id": id, "ok": false, "error": error })
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested; portable so they run on the Windows dev box)
// ---------------------------------------------------------------------------

/// Resolve a browser-supplied allowlist **key** to a runnable spec, or
/// `None` when apps are disabled / the key is unknown / the command is
/// empty. This is the security gate: the browser never sends a command.
pub fn resolve_app(cfg: &VirtualDesktopAppsConfig, key: &str) -> Option<ResolvedApp> {
    if !cfg.enabled {
        return None;
    }
    let spec = cfg.allowlist.get(key)?;
    if spec.command.is_empty() {
        return None;
    }
    Some(ResolvedApp {
        key: key.to_string(),
        command: spec.command.clone(),
        terminal: spec.terminal,
        tmux: spec.tmux,
    })
}

/// The `launchable` list for `rc:apps.list.reply` — stable order
/// (BTreeMap), skipping empty-command entries, label falling back to key.
pub fn launchable_list(cfg: &VirtualDesktopAppsConfig) -> Vec<LaunchableApp> {
    if !cfg.enabled {
        return Vec::new();
    }
    cfg.allowlist
        .iter()
        .filter(|(_, s)| !s.command.is_empty())
        .map(|(k, s)| LaunchableApp {
            key: k.clone(),
            label: s.label.clone().unwrap_or_else(|| k.clone()),
        })
        .collect()
}

/// First free `s<N>` tmux session name not already present.
pub fn next_tmux_session_name(existing: &[String]) -> String {
    for n in 1..=100_000u32 {
        let cand = format!("s{n}");
        if !existing.iter().any(|e| e == &cand) {
            return cand;
        }
    }
    // Astronomically unreachable (100k live sessions); keep total.
    "s0".to_string()
}

/// A raw `wmctrl -l` row before title classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawWindow {
    pub window_id: String,
    pub title: String,
}

/// Parse `wmctrl -l` output. Format per line:
/// `<hex-id> <desktop> <client-host> <title with spaces…>`. Blank lines
/// skipped; a window with an empty title yields `title == ""`.
pub fn parse_wmctrl_list(out: &str) -> Vec<RawWindow> {
    let mut v = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((wid, rest)) = split_first_token(line) else {
            continue;
        };
        let Some((_desktop, rest)) = split_first_token(rest) else {
            continue;
        };
        let Some((_host, rest)) = split_first_token(rest) else {
            continue;
        };
        v.push(RawWindow {
            window_id: wid.to_string(),
            title: rest.trim_start().to_string(),
        });
    }
    v
}

/// Split off the first whitespace-delimited token, returning
/// `(token, remainder)`. Leading whitespace is trimmed first.
fn split_first_token(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    match s.split_once(|c: char| c.is_whitespace()) {
        Some((tok, rest)) => Some((tok, rest)),
        None => Some((s, "")),
    }
}

/// Parse `tmux list-sessions -F '#{session_name}'` output → session names.
pub fn parse_tmux_sessions(out: &str) -> Vec<String> {
    out.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// A window title classified against the `roomler:*` convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedTitle {
    /// Friendly title (our prefix stripped/prettified).
    pub title: String,
    pub session: Option<String>,
    pub app_key: Option<String>,
}

/// Classify a raw window title. We launch our own windows with a known
/// title so the list can map a window back to its tmux session or
/// allowlist key without fragile pid/xprop correlation:
/// * `roomler:tmux:<s>` → a bash/tmux session
/// * `roomler:app:<key>` → an allowlisted app
/// * anything else → an unmanaged window (reported as-is).
pub fn classify_title(raw: &str) -> ClassifiedTitle {
    if let Some(s) = raw.strip_prefix("roomler:tmux:") {
        let s = s.trim();
        return ClassifiedTitle {
            title: format!("Terminal ({s})"),
            session: Some(s.to_string()),
            app_key: None,
        };
    }
    if let Some(k) = raw.strip_prefix("roomler:app:") {
        let k = k.trim();
        return ClassifiedTitle {
            title: k.to_string(),
            session: None,
            app_key: Some(k.to_string()),
        };
    }
    ClassifiedTitle {
        title: raw.to_string(),
        session: None,
        app_key: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(keys: &[(&str, bool)]) -> VirtualDesktopAppsConfig {
        let mut allowlist = BTreeMap::new();
        for (k, tmux) in keys {
            allowlist.insert(
                k.to_string(),
                AppSpec {
                    command: vec![k.to_string()],
                    label: None,
                    terminal: *tmux,
                    tmux: *tmux,
                },
            );
        }
        VirtualDesktopAppsConfig {
            enabled: true,
            allowlist,
        }
    }

    /// A canned backend so the reply-envelope shapes are asserted with no
    /// real X server.
    struct FakeWm {
        windows: Vec<WindowInfo>,
        fail: bool,
    }
    impl WindowManager for FakeWm {
        fn list(&self) -> Result<Vec<WindowInfo>> {
            if self.fail {
                anyhow::bail!("no display");
            }
            Ok(self.windows.clone())
        }
        fn focus(&self, window_id: &str) -> Result<()> {
            if window_id == "0xBAD" {
                anyhow::bail!("no such window");
            }
            Ok(())
        }
        fn launch(&self, app: &ResolvedApp) -> Result<LaunchOutcome> {
            Ok(LaunchOutcome {
                window_id: Some("0xNEW".to_string()),
                session: app.tmux.then(|| "s2".to_string()),
            })
        }
    }

    #[test]
    fn default_config_seeds_a_shell() {
        let cfg = VirtualDesktopAppsConfig::default();
        assert!(cfg.enabled);
        #[cfg(target_os = "windows")]
        {
            let cmd = cfg.allowlist.get("cmd").expect("cmd seeded");
            assert_eq!(cmd.command, vec!["cmd.exe".to_string()]);
        }
        #[cfg(not(target_os = "windows"))]
        {
            let bash = cfg.allowlist.get("bash").expect("bash seeded");
            assert_eq!(bash.command, vec!["bash".to_string()]);
            assert!(bash.tmux && bash.terminal);
        }
    }

    #[test]
    fn resolve_app_gates_on_allowlist_and_enabled() {
        let cfg = cfg_with(&[("bash", true), ("htop", false)]);
        assert_eq!(resolve_app(&cfg, "bash").unwrap().key, "bash");
        assert!(resolve_app(&cfg, "htop").is_some());
        assert!(resolve_app(&cfg, "rm-rf").is_none(), "unknown key rejected");

        let mut disabled = cfg.clone();
        disabled.enabled = false;
        assert!(
            resolve_app(&disabled, "bash").is_none(),
            "disabled → nothing launches"
        );
    }

    #[test]
    fn resolve_app_rejects_empty_command() {
        let mut cfg = cfg_with(&[]);
        cfg.allowlist.insert(
            "empty".to_string(),
            AppSpec {
                command: vec![],
                label: None,
                terminal: false,
                tmux: false,
            },
        );
        assert!(resolve_app(&cfg, "empty").is_none());
    }

    #[test]
    fn launchable_list_stable_and_labelled() {
        let mut cfg = cfg_with(&[("zed", false), ("bash", true)]);
        cfg.allowlist.get_mut("bash").unwrap().label = Some("New bash session".to_string());
        let list = launchable_list(&cfg);
        // BTreeMap → alphabetical: bash, zed
        assert_eq!(list[0].key, "bash");
        assert_eq!(list[0].label, "New bash session");
        assert_eq!(list[1].key, "zed");
        assert_eq!(list[1].label, "zed", "label falls back to key");
    }

    #[test]
    fn next_tmux_session_name_picks_first_free() {
        assert_eq!(next_tmux_session_name(&[]), "s1");
        assert_eq!(
            next_tmux_session_name(&["s1".to_string(), "s2".to_string()]),
            "s3"
        );
        assert_eq!(
            next_tmux_session_name(&["s1".to_string(), "s3".to_string()]),
            "s2",
            "fills gaps"
        );
        assert_eq!(
            next_tmux_session_name(&["main".to_string()]),
            "s1",
            "ignores non-s<N> names"
        );
    }

    #[test]
    fn parse_wmctrl_list_handles_spaces_and_blanks() {
        let out = "0x03400007  0 host roomler:tmux:main\n\
                   0x03600004 0 host  a title with spaces\n\
                   \n\
                   0x03800009 -1 host \n"; // empty title
        let ws = parse_wmctrl_list(out);
        assert_eq!(ws.len(), 3);
        assert_eq!(ws[0].window_id, "0x03400007");
        assert_eq!(ws[0].title, "roomler:tmux:main");
        assert_eq!(ws[1].title, "a title with spaces");
        assert_eq!(ws[2].title, "", "empty title tolerated");
    }

    #[test]
    fn parse_tmux_sessions_trims_and_filters() {
        assert_eq!(
            parse_tmux_sessions("main\n s2 \n\n"),
            vec!["main".to_string(), "s2".to_string()]
        );
        assert!(parse_tmux_sessions("").is_empty(), "no server → empty");
    }

    #[test]
    fn classify_title_maps_convention() {
        let t = classify_title("roomler:tmux:main");
        assert_eq!(t.session.as_deref(), Some("main"));
        assert!(t.app_key.is_none());
        assert_eq!(t.title, "Terminal (main)");

        let a = classify_title("roomler:app:htop");
        assert_eq!(a.app_key.as_deref(), Some("htop"));
        assert!(a.session.is_none());
        assert_eq!(a.title, "htop");

        let p = classify_title("Mozilla Firefox");
        assert!(p.session.is_none() && p.app_key.is_none());
        assert_eq!(p.title, "Mozilla Firefox");
    }

    #[test]
    fn dispatch_list_unsupported_when_no_backend() {
        let cfg = VirtualDesktopAppsConfig::default();
        let reply = dispatch(&json!({"t": "rc:apps.list", "id": "a1"}), &cfg, None);
        assert_eq!(reply["t"], "rc:apps.list.reply");
        assert_eq!(reply["id"], "a1");
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["supported"], false);
        assert_eq!(reply["windows"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn dispatch_list_ok_with_backend() {
        let cfg = cfg_with(&[("bash", true)]);
        let wm = FakeWm {
            windows: vec![WindowInfo {
                window_id: "0x1".to_string(),
                title: "Terminal (main)".to_string(),
                app_key: None,
                session: Some("main".to_string()),
                focused: true,
            }],
            fail: false,
        };
        let reply = dispatch(&json!({"t": "rc:apps.list", "id": "a2"}), &cfg, Some(&wm));
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["supported"], true);
        assert_eq!(reply["windows"][0]["window_id"], "0x1");
        assert_eq!(reply["windows"][0]["session"], "main");
        assert_eq!(reply["launchable"][0]["key"], "bash");
    }

    #[test]
    fn dispatch_list_error_is_wellformed() {
        let cfg = cfg_with(&[("bash", true)]);
        let wm = FakeWm {
            windows: vec![],
            fail: true,
        };
        let reply = dispatch(&json!({"t": "rc:apps.list", "id": "a3"}), &cfg, Some(&wm));
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["supported"], true);
        assert!(reply["error"].as_str().unwrap().contains("no display"));
    }

    #[test]
    fn dispatch_focus_roundtrips_and_errors() {
        let cfg = cfg_with(&[]);
        let wm = FakeWm {
            windows: vec![],
            fail: false,
        };
        let ok = dispatch(
            &json!({"t": "rc:apps.focus", "id": "f1", "window_id": "0x5"}),
            &cfg,
            Some(&wm),
        );
        assert_eq!(ok["t"], "rc:apps.focus.reply");
        assert_eq!(ok["ok"], true);

        let bad = dispatch(
            &json!({"t": "rc:apps.focus", "id": "f2", "window_id": "0xBAD"}),
            &cfg,
            Some(&wm),
        );
        assert_eq!(bad["ok"], false);
        assert!(bad["error"].as_str().unwrap().contains("no such window"));

        let missing = dispatch(&json!({"t": "rc:apps.focus", "id": "f3"}), &cfg, Some(&wm));
        assert_eq!(missing["ok"], false);
        assert!(missing["error"].as_str().unwrap().contains("window_id"));
    }

    #[test]
    fn dispatch_launch_allowlist_gate() {
        let cfg = cfg_with(&[("bash", true)]);
        let wm = FakeWm {
            windows: vec![],
            fail: false,
        };
        let ok = dispatch(
            &json!({"t": "rc:apps.launch", "id": "l1", "app_key": "bash"}),
            &cfg,
            Some(&wm),
        );
        assert_eq!(ok["t"], "rc:apps.launch.reply");
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["window_id"], "0xNEW");
        assert_eq!(ok["session"], "s2");

        let denied = dispatch(
            &json!({"t": "rc:apps.launch", "id": "l2", "app_key": "evil"}),
            &cfg,
            Some(&wm),
        );
        assert_eq!(denied["ok"], false);
        assert!(denied["error"].as_str().unwrap().contains("allowlist"));
    }

    #[test]
    fn dispatch_focus_unsupported_without_backend() {
        let cfg = VirtualDesktopAppsConfig::default();
        let reply = dispatch(
            &json!({"t": "rc:apps.focus", "id": "x", "window_id": "0x1"}),
            &cfg,
            None,
        );
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("not supported"));
    }

    #[test]
    fn dispatch_null_id_tolerated() {
        let cfg = cfg_with(&[]);
        let reply = dispatch(&json!({"t": "rc:apps.list"}), &cfg, None);
        assert!(reply["id"].is_null());
        assert_eq!(reply["ok"], true);
    }
}

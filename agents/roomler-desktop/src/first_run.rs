// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-84 D6 — how the companion starts: what its own command line asks for,
//! whether this person has seen the Welcome tour, and whether a login start
//! should stay in the tray, open the tour, or leave again at once.
//!
//! Before D6 only the SECOND instance read argv (the single-instance
//! callback in `main.rs`): the first process ignored `--view=` entirely, and
//! nothing could tell a login start from a person opening the app.
//!
//! #3887 — nor a start the daemon made to show a consent question or a
//! banner: such a start keeps the tour for the person's own start.
//!
//! Also the Tauri commands behind the Welcome view and the "Start at login"
//! card: the launch intent, the per-user state, the login start itself, and
//! a free loopback port for userspace mode.

use roomler_node_core::companion_autostart::{AUTOSTART_ARG, FIRST_RUN_ARG, FOR_PROMPT_ARG};
use roomler_node_core::desktop_state;
use serde::Serialize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// What the command line asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchArgs {
    /// `--view=<name>`: open on this view. Whitelisted to ASCII alphanumerics
    /// (it is routed into the page), else ignored.
    pub view: Option<String>,
    /// `--autostart`: a login start.
    pub autostart: bool,
    /// `--first-run`: the daemon's one-shot post-install launch.
    pub first_run: bool,
    /// `--for-prompt`: the daemon's start to show a consent question or a
    /// banner (#3887). On macOS, where launchd's start carries no argument,
    /// set from the daemon's answer instead ([`asks_the_daemon`]).
    pub for_prompt: bool,
}

/// Parse the companion's own arguments (`args[0]` is the program). Each flag
/// is read by equality; anything else is ignored, so an older companion
/// starts on a newer daemon's argument as it always did.
pub fn parse_launch_args<S: AsRef<str>>(args: &[S]) -> LaunchArgs {
    let mut out = LaunchArgs::default();
    for arg in args.iter().skip(1).map(AsRef::as_ref) {
        if arg == AUTOSTART_ARG {
            out.autostart = true;
        } else if arg == FIRST_RUN_ARG {
            out.first_run = true;
        } else if arg == FOR_PROMPT_ARG {
            out.for_prompt = true;
        } else if let Some(view) = arg.strip_prefix("--view=")
            && !view.is_empty()
            && view.chars().all(|c| c.is_ascii_alphanumeric())
        {
            out.view = Some(view.to_string());
        }
    }
    out
}

/// What the FIRST process does at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupAction {
    /// Leave at once: a login start for a person who switched it off.
    Exit,
    /// Stay in the tray, window hidden (today's behaviour).
    Tray,
    /// Show the window on this view.
    Show(String),
}

/// Decide the first process's start. `first_run_done` / `opted_out` come
/// from the per-user desktop state.
pub fn decide_startup(args: &LaunchArgs, first_run_done: bool, opted_out: bool) -> StartupAction {
    // An explicit view is a person (or a script) asking for exactly that.
    if let Some(view) = &args.view {
        return StartupAction::Show(view.clone());
    }
    // #3887 — the daemon's start for a consent question or a banner stays in
    // the tray: each has a window of its own, and the question has a
    // deadline. A tour that is not done waits for the person's own start
    // ([`holds_the_tour`]). Never `Exit` either: this is no login start.
    if args.for_prompt {
        return StartupAction::Tray;
    }
    // The tour shows until it is finished or skipped, however a person, a
    // login or the installer started this — a login start included: an
    // install done over RMM with nobody logged on meets its person at their
    // first login.
    if !first_run_done {
        return StartupAction::Show("welcome".to_string());
    }
    if args.first_run {
        return StartupAction::Show("overview".to_string());
    }
    if args.autostart && opted_out {
        return StartupAction::Exit;
    }
    StartupAction::Tray
}

/// What a SECOND launch, forwarded to the running instance, does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecondLaunch {
    /// Nothing at all: a login start must not pop the window of a companion
    /// that is already up.
    Ignore,
    /// Show and focus the window where it is (a double-click).
    Show,
    /// Show the window on this view.
    ShowView(String),
}

/// Decide a forwarded launch. `tour_held`: this companion was started for a
/// prompt while the tour was not done, and has not shown it yet
/// ([`tour_held`]).
pub fn decide_second_instance(
    args: &LaunchArgs,
    first_run_done: bool,
    tour_held: bool,
) -> SecondLaunch {
    if let Some(view) = &args.view {
        return SecondLaunch::ShowView(view.clone());
    }
    if args.first_run {
        let view = if first_run_done {
            "overview"
        } else {
            "welcome"
        };
        return SecondLaunch::ShowView(view.to_string());
    }
    // Neither a login start nor the daemon's start for a prompt pops the
    // window; the question and the banner have their own (#3887).
    if args.autostart || args.for_prompt {
        return SecondLaunch::Ignore;
    }
    // A person's start: the tour a start for a prompt held back (#3887).
    if tour_held && !first_run_done {
        return SecondLaunch::ShowView("welcome".to_string());
    }
    SecondLaunch::Show
}

// ─── the tour a start for a prompt held back (#3887) ──────────────────────

/// Set at a start for a prompt while the tour was not done: the person's own
/// first opening of the window shows the tour, once.
static TOUR_HELD: AtomicBool = AtomicBool::new(false);

/// Whether this start holds the tour back: it stays in the tray with the
/// tour not done, which only a start for a prompt does ([`decide_startup`]).
pub fn holds_the_tour(action: &StartupAction, first_run_done: bool) -> bool {
    !first_run_done && *action == StartupAction::Tray
}

/// Hold the tour back for the person's own opening of the window.
pub fn hold_the_tour() {
    TOUR_HELD.store(true, Ordering::SeqCst);
}

/// The tour is held back and has not been shown yet.
pub fn tour_held() -> bool {
    TOUR_HELD.load(Ordering::SeqCst)
}

/// The window is being shown on `view`: once that is the tour, it is held no
/// more, and the window opens as it always did.
pub fn showing(view: &str) {
    if view == "welcome" {
        TOUR_HELD.store(false, Ordering::SeqCst);
    }
}

/// The view the tray's icon and its Device status open: the tour a start for
/// a prompt held back while it is not done, else the Overview.
pub fn tray_open_view(tour_held: bool, first_run_done: bool) -> &'static str {
    if tour_held && !first_run_done {
        "welcome"
    } else {
        "overview"
    }
}

/// [`tray_open_view`] now, as the window's path. The per-user state is read
/// only while the tour is held: a tray click reads no file otherwise.
pub fn tray_path() -> String {
    let held = tour_held();
    let view = tray_open_view(held, held && desktop_state::load().first_run_done);
    format!("/{view}")
}

/// #3887 — whether this start asks the daemon why it came. Only on macOS,
/// where the daemon's start for a prompt is launchd's (`launchctl
/// kickstart`), which carries no argument and so looks like a login start or
/// a person's. Only a start with no argument at all, for a person whose tour
/// is not done: the tour is all the answer can change. Windows and Linux are
/// told (`--for-prompt`).
pub fn asks_the_daemon(args: &LaunchArgs, first_run_done: bool, on_macos: bool) -> bool {
    on_macos && !first_run_done && *args == LaunchArgs::default()
}

/// #3887 — the daemon's answer: whether the companion has something to show
/// right now, by the watch's own rules (`consent_watch_loop` in `main.rs`): a
/// question the daemon does not show natively, or a session the banner is
/// for.
pub fn something_to_show(
    pending: &[roomler_localapi::ConsentRequest],
    live: &[roomler_localapi::RcSessionInfo],
) -> bool {
    pending.iter().any(|p| p.surface != "native") || crate::wants_banner(live)
}

/// How long a start waits for the daemon's answer, all of it: the connection
/// and both lists.
const DAEMON_ANSWER_WAIT: std::time::Duration = std::time::Duration::from_millis(300);

/// #3887 — ask the daemon over the LocalAPI ([`DAEMON_ANSWER_WAIT`] at most)
/// whether it has something for this companion to show ([`something_to_show`]).
/// No answer is no reason: the start shows the tour as it always did.
pub fn daemon_has_something_to_show() -> bool {
    let ask = async {
        let mut client = roomler_localapi::connect().await.ok()?;
        // A list that fails is an empty one here: it holds no tour back.
        let pending = client.consent_pending().await.unwrap_or_default();
        let live = client.rc_sessions().await.unwrap_or_default();
        Some((pending, live))
    };
    let answer = tauri::async_runtime::block_on(async {
        tokio::time::timeout(DAEMON_ANSWER_WAIT, ask)
            .await
            .ok()
            .flatten()
    });
    let Some((pending, live)) = answer else {
        tracing::info!(
            wait = ?DAEMON_ANSWER_WAIT,
            "a start with no argument asked the daemon why it came, and had no answer: \
             the tour shows as before (#3887)"
        );
        return false;
    };
    let for_prompt = something_to_show(&pending, &live);
    tracing::info!(
        pending = pending.len(),
        live = live.len(),
        for_prompt,
        "a start with no argument asked the daemon why it came (#3887)"
    );
    for_prompt
}

// ─── a person's opening of the running companion (#3941) ─────────────────

/// #3941 — what a reopen does. On macOS, opening the app while it runs (a
/// double-click in Finder or Launchpad, Spotlight, `open -a`) starts no
/// second process, so no launch is forwarded to it, as one is on Windows and
/// Linux: AppKit hands the running app a reopen instead. That is a person's
/// own start, and it carries no argument, so it is decided as a forwarded
/// launch with none: the window where the person left it, or the tour a
/// start for a prompt held back (#3887). Never a login start's path or a
/// prompt's: only an argument asks for those.
pub fn decide_reopen(first_run_done: bool, tour_held: bool) -> SecondLaunch {
    decide_second_instance(&LaunchArgs::default(), first_run_done, tour_held)
}

/// What a person's opening of the running companion shows, in the words of
/// its log line.
fn what_it_shows(decision: &SecondLaunch) -> &'static str {
    match decision {
        SecondLaunch::Ignore => "nothing",
        SecondLaunch::ShowView(view) if view == "welcome" => "the tour",
        SecondLaunch::Show | SecondLaunch::ShowView(_) => "the main window",
    }
}

/// #3941 — the reopen's one line, beside a second launch's. It names what the
/// reopen shows: the macOS lab's second witness, beside the window itself.
/// `has_visible_windows` is AppKit's, logged and not obeyed: the banner and
/// the consent panel are windows too, so it can be true while the main
/// window is closed.
pub fn log_reopen(
    decision: &SecondLaunch,
    first_run_done: bool,
    tour_held: bool,
    has_visible_windows: bool,
) {
    tracing::info!(
        first_run_done,
        tour_held,
        has_visible_windows,
        ?decision,
        "a reopen shows {}",
        what_it_shows(decision)
    );
}

/// #3996 — the tray's one line when its icon or its Device status opens the
/// window, beside a reopen's ([`log_reopen`]): what it shows, the tour a start
/// for a prompt held back or the Overview ([`tray_open_view`]). Until #3996
/// that click logged nothing.
pub fn log_tray_open(item: &str, tour_held: bool, view: &str) {
    tracing::info!(
        item,
        tour_held,
        view,
        "a tray click shows {}",
        if view == "welcome" {
            "the tour"
        } else {
            "the Overview"
        }
    );
}

/// Show the window as a person's opening of the running companion decided
/// (#3941): a second launch forwarded on Windows and Linux, a reopen on
/// macOS, one code for both. `route` sends the page to a view, `show` shows
/// and focuses the window. A tour shown here is held no more ([`showing`]),
/// so it is offered once.
pub fn open_window(decision: SecondLaunch, route: impl FnOnce(&str), show: impl FnOnce()) {
    match decision {
        SecondLaunch::Ignore => {}
        SecondLaunch::Show => show(),
        SecondLaunch::ShowView(view) => {
            showing(&view);
            route(&view);
            show();
        }
    }
}

/// The first port in `start..start+span` that is not in `exclude` and that
/// `can_bind` accepts. Userspace mode's SOCKS front needs a loopback port
/// that is free NOW and not claimed by a declared route that is merely down
/// at the moment — 1080 is exactly the port such a route tends to hold.
pub fn pick_free_port(
    start: u16,
    span: u16,
    exclude: &[u16],
    can_bind: impl Fn(u16) -> bool,
) -> Option<u16> {
    (0..span)
        .map_while(|i| start.checked_add(i))
        .find(|p| *p != 0 && !exclude.contains(p) && can_bind(*p))
}

// ─── the launch intent ─────────────────────────────────────────────────────

/// The view this process was launched on, handed to the page ONCE. Set in
/// `main` before the window exists; the page asks for it on load, which is
/// the one moment it is certain to be listening (an eval from `setup` can run
/// against a page that is not there yet).
static LAUNCH_VIEW: Mutex<Option<String>> = Mutex::new(None);

pub fn set_launch_view(view: Option<String>) {
    *LAUNCH_VIEW.lock().unwrap_or_else(|p| p.into_inner()) = view;
}

#[derive(Debug, Serialize)]
pub struct LaunchIntent {
    pub view: Option<String>,
}

/// Taken, not read: a reload of the page must not jump back to the launch
/// view after the person moved on.
#[tauri::command]
pub fn cmd_launch_intent() -> LaunchIntent {
    LaunchIntent {
        view: LAUNCH_VIEW.lock().unwrap_or_else(|p| p.into_inner()).take(),
    }
}

// ─── per-user state ────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct DesktopStateView {
    pub first_run_done: bool,
    pub autostart_opt_out: bool,
    /// `windows` | `linux` | `macos` — the tour words the private-network
    /// step per platform.
    pub platform: &'static str,
    /// macOS: the privileged half (`com.roomler.daemon`, root) is installed.
    /// On a Mac the MESH belongs to that half — its own enrollment — while
    /// this app talks to the per-user capture half; offering to put THIS half
    /// on the mesh too would make the Mac two nodes.
    pub macos_privileged_half: bool,
}

#[tauri::command]
pub fn cmd_desktop_state() -> DesktopStateView {
    let s = desktop_state::load();
    DesktopStateView {
        first_run_done: s.first_run_done,
        autostart_opt_out: s.autostart_opt_out,
        platform: if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        },
        // The plist is world-readable; `/etc/roomler` (0700) is not.
        macos_privileged_half: cfg!(target_os = "macos")
            && std::path::Path::new("/Library/LaunchDaemons/com.roomler.daemon.plist").exists(),
    }
}

/// The tour is finished (or skipped): it stops opening by itself.
#[tauri::command]
pub fn cmd_first_run_done() -> Result<(), String> {
    let mut s = desktop_state::load();
    if s.first_run_done {
        return Ok(());
    }
    s.first_run_done = true;
    desktop_state::save(&s).map_err(|e| format!("saving the desktop state: {e}"))
}

// ─── free port for userspace mode ──────────────────────────────────────────

/// A loopback port for userspace mode's SOCKS5 front: free right now and not
/// one of `exclude` (the declared routes' local ports). Probed upward from
/// `preferred`, never at 1080 by default.
#[tauri::command]
pub fn cmd_free_port(preferred: Option<u16>, exclude: Option<Vec<u16>>) -> Result<u16, String> {
    let start = preferred.filter(|p| *p >= 1024).unwrap_or(41080);
    let exclude = exclude.unwrap_or_default();
    pick_free_port(start, 200, &exclude, |p| {
        std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
    })
    .ok_or_else(|| {
        format!(
            "no free loopback port in {start}..{}",
            start.saturating_add(200)
        )
    })
}

// ─── start at login ────────────────────────────────────────────────────────

/// What the "Start at login" toggle shows.
#[derive(Debug, Serialize)]
pub struct AutostartState {
    /// The toggle can be changed here.
    pub supported: bool,
    /// Roomler starts when this person signs in.
    pub enabled: bool,
    /// `machine` | `user` | `xdg` | `macos` | `none`.
    pub scope: String,
    /// One line under the toggle.
    pub note: String,
}

/// The device's `companion_autostart`, asked of the running daemon; `None`
/// when the daemon is not reachable or predates the key (treated as ON).
pub async fn device_autostart_switch() -> Option<bool> {
    let ask = async {
        let mut client = roomler_localapi::connect().await.ok()?;
        let entries = client.config_entries().await.ok()?;
        entries
            .into_iter()
            .find(|e| e.key == "companion_autostart")
            .and_then(|e| e.value)
            .map(|v| v == "true")
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), ask)
        .await
        .ok()
        .flatten()
}

// macOS never shows it: Login Items are the OS's, not ours.
#[cfg(not(target_os = "macos"))]
const DEVICE_OFF_NOTE: &str = "Turned off for this device (the companion_autostart setting). \
     An administrator can turn it back on in Settings → Device configuration.";

/// Checks that need the daemon, run once after start:
///
/// * a LOGIN start on a device whose `companion_autostart` is off leaves —
///   the device's switch governs every login-start mechanism, including the
///   Linux package's XDG entry, which the daemon does not own and so cannot
///   remove;
/// * a first run registers the per-user login start (the belt behind the
///   worker's own registration).
pub async fn startup_checks<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    autostart: bool,
    first_run_done: bool,
) {
    let device = device_autostart_switch().await;
    if autostart && device == Some(false) {
        tracing::info!("login start on a device whose companion_autostart is off — leaving");
        app.exit(0);
        return;
    }
    if !first_run_done && device != Some(false) {
        let _ = tokio::task::spawn_blocking(ensure_user_login_start).await;
    }
}

#[tauri::command]
pub async fn cmd_autostart_get() -> AutostartState {
    let device_on = device_autostart_switch().await != Some(false);
    tokio::task::spawn_blocking(move || autostart_state(device_on))
        .await
        .unwrap_or_else(|e| AutostartState {
            supported: false,
            enabled: false,
            scope: "none".into(),
            note: format!("Unavailable: {e}"),
        })
}

#[tauri::command]
pub async fn cmd_autostart_set(enabled: bool) -> Result<AutostartState, String> {
    let device_on = device_autostart_switch().await != Some(false);
    tokio::task::spawn_blocking(move || {
        set_autostart(enabled, device_on)?;
        Ok(autostart_state(device_on))
    })
    .await
    .map_err(|e| format!("task join: {e}"))?
}

/// Record a person's choice in their desktop state.
#[cfg(not(target_os = "macos"))]
fn record_opt_out(opted_out: bool) -> Result<(), String> {
    let mut s = desktop_state::load();
    s.autostart_opt_out = opted_out;
    desktop_state::save(&s).map_err(|e| format!("saving the desktop state: {e}"))
}

#[cfg(windows)]
fn autostart_state(device_on: bool) -> AutostartState {
    use roomler_node_core::companion_autostart::{
        InstallScope, WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME, program_files_dirs,
        registry::{Hive, read_value},
        windows_scope_for_exe,
    };
    let opted_out = desktop_state::load().autostart_opt_out;
    let Ok(exe) = std::env::current_exe() else {
        return AutostartState {
            supported: false,
            enabled: false,
            scope: "none".into(),
            note: "Cannot locate this app.".into(),
        };
    };
    match windows_scope_for_exe(&exe, &program_files_dirs()) {
        InstallScope::Machine => {
            let registered = read_value(
                Hive::LocalMachine,
                WINDOWS_RUN_SUBKEY,
                WINDOWS_RUN_VALUE_NAME,
            )
            .ok()
            .flatten()
            .is_some();
            AutostartState {
                supported: registered && device_on,
                enabled: registered && device_on && !opted_out,
                scope: "machine".into(),
                note: if !device_on || !registered {
                    DEVICE_OFF_NOTE.into()
                } else {
                    "Roomler is installed for everyone on this computer; this choice is for your \
                     account only."
                        .into()
                },
            }
        }
        InstallScope::User => {
            let registered = read_value(
                Hive::CurrentUser,
                WINDOWS_RUN_SUBKEY,
                WINDOWS_RUN_VALUE_NAME,
            )
            .ok()
            .flatten()
            .is_some();
            AutostartState {
                supported: device_on,
                enabled: device_on && registered && !opted_out,
                scope: "user".into(),
                note: if device_on {
                    "Installed for your account only.".into()
                } else {
                    DEVICE_OFF_NOTE.into()
                },
            }
        }
    }
}

#[cfg(windows)]
fn set_autostart(enabled: bool, device_on: bool) -> Result<(), String> {
    use roomler_node_core::companion_autostart::{
        InstallScope, WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME, program_files_dirs,
        registry::{Hive, delete_value, write_value},
        windows_run_command, windows_scope_for_exe,
    };
    if !device_on && enabled {
        return Err(DEVICE_OFF_NOTE.into());
    }
    let exe = std::env::current_exe().map_err(|e| format!("locating this app: {e}"))?;
    if windows_scope_for_exe(&exe, &program_files_dirs()) == InstallScope::User {
        // Per-user: the value is this person's to write or delete.
        if enabled {
            write_value(
                Hive::CurrentUser,
                WINDOWS_RUN_SUBKEY,
                WINDOWS_RUN_VALUE_NAME,
                &windows_run_command(&exe),
            )
            .map_err(|e| format!("registering the login start: {e}"))?;
        } else {
            delete_value(
                Hive::CurrentUser,
                WINDOWS_RUN_SUBKEY,
                WINDOWS_RUN_VALUE_NAME,
            )
            .map_err(|e| format!("removing the login start: {e}"))?;
        }
    }
    // Machine-wide: the HKLM value stays (other accounts), and the choice is
    // honoured at `--autostart`. Recorded either way, so `service install` on
    // a per-user host does not put back what the person took out.
    record_opt_out(!enabled)
}

/// The first run's belt for a per-user install: the worker registers the
/// login start at each start, but the companion may exist before the worker
/// next runs.
#[cfg(windows)]
pub fn ensure_user_login_start() {
    use roomler_node_core::companion_autostart::{
        InstallScope, WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME, desired_run_value,
        program_files_dirs,
        registry::{Hive, sync_value},
        windows_scope_for_exe,
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if windows_scope_for_exe(&exe, &program_files_dirs()) != InstallScope::User {
        return;
    }
    let opted_out = desktop_state::load().autostart_opt_out;
    let desired = desired_run_value(true, &exe, true, opted_out);
    match sync_value(
        Hive::CurrentUser,
        WINDOWS_RUN_SUBKEY,
        WINDOWS_RUN_VALUE_NAME,
        desired.as_deref(),
    ) {
        Ok(action) => tracing::debug!(?action, "login start (HKCU) checked"),
        Err(e) => tracing::warn!(error = %e, "login start (HKCU): could not register"),
    }
}

#[cfg(not(windows))]
pub fn ensure_user_login_start() {}

#[cfg(all(unix, not(target_os = "macos")))]
fn xdg_user_file() -> Option<std::path::PathBuf> {
    use roomler_node_core::companion_autostart::{XDG_ENTRY_FILE, xdg_user_autostart_dir};
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let cfg = std::env::var("XDG_CONFIG_HOME").ok();
    xdg_user_autostart_dir(cfg.as_deref(), home.as_deref()).map(|d| d.join(XDG_ENTRY_FILE))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn autostart_state(device_on: bool) -> AutostartState {
    use roomler_node_core::companion_autostart::{XDG_SYSTEM_AUTOSTART, xdg_entry_hidden};
    let system = std::path::Path::new(XDG_SYSTEM_AUTOSTART).exists();
    let user = xdg_user_file().and_then(|p| std::fs::read_to_string(p).ok());
    let enabled = match &user {
        Some(content) => !xdg_entry_hidden(content),
        None => system,
    };
    AutostartState {
        supported: device_on && xdg_user_file().is_some(),
        enabled: device_on && enabled,
        scope: "xdg".into(),
        note: if device_on {
            "Your desktop's autostart list, for your account only.".into()
        } else {
            DEVICE_OFF_NOTE.into()
        },
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn set_autostart(enabled: bool, device_on: bool) -> Result<(), String> {
    use roomler_node_core::companion_autostart::{XDG_SYSTEM_AUTOSTART, xdg_user_entry};
    if !device_on && enabled {
        return Err(DEVICE_OFF_NOTE.into());
    }
    let path = xdg_user_file().ok_or("no home directory")?;
    let system = std::path::Path::new(XDG_SYSTEM_AUTOSTART).exists();
    let ours = std::fs::read_to_string(&path)
        .map(|c| c.contains("X-Roomler-Managed=true"))
        .unwrap_or(false);
    if enabled && system && (ours || !path.exists()) {
        // The package's entry applies again once our override is gone.
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| format!("removing {}: {e}", path.display()))?;
        }
    } else {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/usr/bin/roomler-desktop".to_string());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        std::fs::write(&path, xdg_user_entry(&exe, !enabled))
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    record_opt_out(!enabled)
}

#[cfg(target_os = "macos")]
fn autostart_state(_device_on: bool) -> AutostartState {
    AutostartState {
        supported: false,
        enabled: true,
        scope: "macos".into(),
        note: "macOS manages this: System Settings → General → Login Items.".into(),
    }
}

#[cfg(target_os = "macos")]
fn set_autostart(_enabled: bool, _device_on: bool) -> Result<(), String> {
    Err("macOS manages this: System Settings → General → Login Items.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        std::iter::once("roomler-desktop")
            .chain(v.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn argv_is_parsed_for_the_first_process_too() {
        assert_eq!(parse_launch_args(&args(&[])), LaunchArgs::default());
        assert_eq!(
            parse_launch_args(&args(&["--autostart"])),
            LaunchArgs {
                autostart: true,
                ..LaunchArgs::default()
            }
        );
        assert_eq!(
            parse_launch_args(&args(&["--first-run"])),
            LaunchArgs {
                first_run: true,
                ..LaunchArgs::default()
            }
        );
        assert_eq!(
            parse_launch_args(&args(&["--view=devices", "--autostart"])),
            LaunchArgs {
                view: Some("devices".into()),
                autostart: true,
                ..LaunchArgs::default()
            }
        );
        // The view goes into the page: anything but ASCII alphanumerics is
        // dropped, not escaped.
        for bad in [
            "--view=",
            "--view=a'b",
            "--view=../x",
            "--view=welcome;alert(1)",
            "--view=wélcome",
        ] {
            assert_eq!(parse_launch_args(&args(&[bad])).view, None, "{bad}");
        }
        // The program name is never an argument, whatever it is.
        assert_eq!(
            parse_launch_args(&["--first-run".to_string()]),
            LaunchArgs::default()
        );
    }

    #[test]
    fn startup_table() {
        use StartupAction::*;
        let a = |v: &[&str]| parse_launch_args(&args(v));
        let welcome = || Show("welcome".to_string());
        // (args, first_run_done, opted_out) → action
        let table: Vec<(Vec<&str>, bool, bool, StartupAction)> = vec![
            // A person who never finished the tour sees it, however a person,
            // a login or the installer launched it.
            (vec![], false, false, welcome()),
            (vec!["--autostart"], false, false, welcome()),
            (vec!["--autostart"], false, true, welcome()),
            (vec!["--first-run"], false, false, welcome()),
            // #3887 — the daemon's start for a question or a banner stays in
            // the tray, the tour done or not: the question has a deadline. It
            // never leaves either, whatever the login-start switch says.
            (vec!["--for-prompt"], false, false, Tray),
            (vec!["--for-prompt"], false, true, Tray),
            (vec!["--for-prompt"], true, false, Tray),
            (vec!["--for-prompt"], true, true, Tray),
            // After the tour: login starts stay in the tray…
            (vec!["--autostart"], true, false, Tray),
            // …or leave at once for a person who switched login start off.
            (vec!["--autostart"], true, true, Exit),
            // A plain start (a daemon respawn, a double-click) keeps today's
            // tray-only behaviour.
            (vec![], true, false, Tray),
            (vec![], true, true, Tray),
            // The installer's launch on an account that already did the tour
            // still opens the window — something was just installed.
            (
                vec!["--first-run"],
                true,
                false,
                Show("overview".to_string()),
            ),
            // An explicit view always wins, even over the tour.
            (
                vec!["--view=devices"],
                false,
                false,
                Show("devices".to_string()),
            ),
            (
                vec!["--view=settings", "--autostart"],
                true,
                true,
                Show("settings".to_string()),
            ),
        ];
        for (v, done, opted_out, want) in table {
            assert_eq!(
                decide_startup(&a(&v), done, opted_out),
                want,
                "{v:?} done={done} opted_out={opted_out}"
            );
        }
    }

    #[test]
    fn a_forwarded_launch_never_pops_a_login_start() {
        use SecondLaunch::*;
        let a = |v: &[&str]| parse_launch_args(&args(v));
        assert_eq!(
            decide_second_instance(&a(&["--autostart"]), true, false),
            Ignore
        );
        assert_eq!(
            decide_second_instance(&a(&["--autostart"]), false, false),
            Ignore
        );
        assert_eq!(
            decide_second_instance(&a(&["--view=devices"]), true, false),
            ShowView("devices".into())
        );
        assert_eq!(
            decide_second_instance(&a(&["--first-run"]), false, false),
            ShowView("welcome".into())
        );
        assert_eq!(
            decide_second_instance(&a(&["--first-run"]), true, false),
            ShowView("overview".into())
        );
        // A plain second launch (a double-click) shows the window where it is.
        assert_eq!(decide_second_instance(&a(&[]), true, false), Show);
        assert_eq!(decide_second_instance(&a(&[]), false, false), Show);
    }

    /// #3887 — `--for-prompt` is read by equality, as the other arguments
    /// are: a longer or a different spelling is not it.
    #[test]
    fn a_start_for_a_prompt_is_read_by_equality() {
        assert_eq!(
            parse_launch_args(&args(&["--for-prompt"])),
            LaunchArgs {
                for_prompt: true,
                ..LaunchArgs::default()
            }
        );
        for near in [
            "--for-prompt=1",
            "--for-prompts",
            "--for-promp",
            "--FOR-PROMPT",
            "-for-prompt",
            "for-prompt",
        ] {
            assert_eq!(
                parse_launch_args(&args(&[near])),
                LaunchArgs::default(),
                "{near}"
            );
        }
        assert_eq!(
            parse_launch_args(&["--for-prompt".to_string()]),
            LaunchArgs::default(),
            "the program name is never an argument"
        );
    }

    /// #3887 — a start for a question holds the tour back, and the person's
    /// own opening of the window shows it: a double-click (forwarded), the
    /// tray's icon or its Device status. Once, and only while it is not done.
    #[test]
    fn a_persons_start_after_a_start_for_a_prompt_shows_the_tour() {
        use SecondLaunch::*;
        let a = |v: &[&str]| parse_launch_args(&args(v));
        // The daemon starts the companion for a question; the tour is not done.
        let first = decide_startup(&a(&["--for-prompt"]), false, false);
        assert_eq!(first, StartupAction::Tray);
        assert!(
            holds_the_tour(&first, false),
            "the tour waits for the person"
        );
        // The person double-clicks the app: the running companion gets it.
        assert_eq!(
            decide_second_instance(&a(&[]), false, true),
            ShowView("welcome".into())
        );
        // …or clicks the tray icon, or Device status.
        assert_eq!(tray_open_view(true, false), "welcome");
        // The daemon's next start for a prompt, forwarded (it raced the
        // running check): nothing pops. The question has its own window.
        assert_eq!(
            decide_second_instance(&a(&["--for-prompt"]), false, true),
            Ignore
        );
        assert_eq!(
            decide_second_instance(&a(&["--for-prompt"]), true, false),
            Ignore
        );
        // A login start forwarded still pops nothing.
        assert_eq!(
            decide_second_instance(&a(&["--autostart"]), false, true),
            Ignore
        );
        // Finished or skipped meanwhile: the window opens as it always did.
        assert_eq!(decide_second_instance(&a(&[]), true, true), Show);
        assert_eq!(tray_open_view(true, true), "overview");
        // Nothing held: as before.
        assert_eq!(tray_open_view(false, false), "overview");

        // Only a start that kept a tour that is not done in the tray holds it:
        // a person's start, a login start and the installer's show it at once.
        for v in [vec![], vec!["--autostart"], vec!["--first-run"]] {
            let action = decide_startup(&a(&v), false, true);
            assert!(!holds_the_tour(&action, false), "{v:?} → {action:?}");
        }
        assert!(
            !holds_the_tour(&decide_startup(&a(&["--for-prompt"]), true, false), true),
            "a finished tour is not held"
        );
    }

    /// The tests that hold the tour back or release it share the process's
    /// one hold ([`TOUR_HELD`]), and the harness runs tests on parallel
    /// threads: they take turns, or one releases what another asserts.
    static THE_HOLD: Mutex<()> = Mutex::new(());

    /// #3887 — the tour held back is offered once: the opening that shows it
    /// releases it, and another view leaves it held.
    #[test]
    fn the_held_tour_is_offered_once() {
        let _turn = THE_HOLD.lock().unwrap_or_else(|p| p.into_inner());
        hold_the_tour();
        assert!(tour_held());
        showing("overview");
        assert!(tour_held(), "another view leaves it held");
        showing("welcome");
        assert!(!tour_held(), "shown once, held no more");
    }

    /// What [`open_window`] did with `decision`, in order: `#/<view>` for the
    /// page routed to a view, `shown` for the window shown and focused.
    fn opens(decision: SecondLaunch) -> Vec<String> {
        let done = std::cell::RefCell::new(Vec::new());
        open_window(
            decision,
            |view| done.borrow_mut().push(format!("#/{view}")),
            || done.borrow_mut().push("shown".to_string()),
        );
        done.into_inner()
    }

    /// #3941 — a reopen (macOS: a double-click in Finder or Launchpad,
    /// Spotlight, `open -a` on the running app) with the window closed shows
    /// the main window where it is, as a plain second launch does on Windows
    /// and Linux. It never does nothing: with no argument it can be neither a
    /// login start nor a start for a prompt.
    #[test]
    fn a_reopen_shows_the_main_window() {
        for done in [true, false] {
            assert_eq!(
                decide_reopen(done, false),
                SecondLaunch::Show,
                "done={done}"
            );
            assert_eq!(
                decide_reopen(done, false),
                decide_second_instance(&parse_launch_args(&args(&[])), done, false),
                "decided as a second launch with no argument, done={done}"
            );
            assert_eq!(opens(decide_reopen(done, false)), ["shown"]);
        }
        for (done, held) in [(true, false), (true, true), (false, false), (false, true)] {
            assert_ne!(
                decide_reopen(done, held),
                SecondLaunch::Ignore,
                "done={done} held={held}"
            );
        }
    }

    /// #3941 — while a start for a prompt holds the tour back (#3887), the
    /// first reopen shows the tour and releases it, and the next shows the
    /// main window where it is: the tour once, as a second launch and the
    /// tray offer it.
    #[test]
    fn a_reopen_shows_the_held_tour_once() {
        let _turn = THE_HOLD.lock().unwrap_or_else(|p| p.into_inner());
        // The daemon started the companion for a question; the tour is not
        // done, so the start held it back.
        hold_the_tour();
        assert_eq!(
            opens(decide_reopen(false, tour_held())),
            ["#/welcome", "shown"],
            "the person opens the app again: the tour"
        );
        assert!(!tour_held(), "shown, so held no more");
        assert_eq!(
            opens(decide_reopen(false, tour_held())),
            ["shown"],
            "and again: the main window, not the tour a second time"
        );
    }

    /// #3941 — the second launch's window code, which the reopen now shares,
    /// does what it did: nothing for a login start or a start for a prompt,
    /// the window where it is for a double-click, and an asked-for view
    /// routed before the window shows.
    #[test]
    fn a_second_launch_opens_the_window_as_before() {
        let a = |v: &[&str]| parse_launch_args(&args(v));
        assert!(opens(decide_second_instance(&a(&["--autostart"]), true, false)).is_empty());
        assert!(opens(decide_second_instance(&a(&["--for-prompt"]), false, true)).is_empty());
        assert_eq!(
            opens(decide_second_instance(&a(&[]), true, false)),
            ["shown"]
        );
        assert_eq!(
            opens(decide_second_instance(&a(&["--view=devices"]), true, false)),
            ["#/devices", "shown"]
        );
    }

    /// Where the tests read what the companion logged.
    #[derive(Clone, Default)]
    struct LogSink(std::sync::Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
        type Writer = LogSink;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// #3941 — one line per reopen, beside a second launch's, naming what it
    /// shows: the tour while a start for a prompt holds it back, else the
    /// main window. The macOS lab reads it as the second witness beside the
    /// window itself.
    #[test]
    fn a_reopen_logs_what_it_shows() {
        // A second live dispatcher for the whole test. With only one
        // registered, a callsite that another test thread reaches first, with
        // no subscriber of its own, is cached as `never`, and the subscriber
        // below would miss the line (`agent-core`'s pairing_terminal tests
        // hold one for the same reason).
        let _second = tracing::Dispatch::new(tracing_subscriber::registry());
        let sink = LogSink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_target(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            log_reopen(&decide_reopen(false, true), false, true, false);
            log_reopen(&decide_reopen(true, false), true, false, true);
        });
        let log = String::from_utf8(sink.0.lock().unwrap_or_else(|p| p.into_inner()).clone())
            .expect("the log is UTF-8");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "one line per reopen:\n{log}");
        assert!(lines[0].contains("a reopen shows the tour"), "{}", lines[0]);
        assert!(lines[0].contains("tour_held=true"), "{}", lines[0]);
        assert!(
            lines[1].contains("a reopen shows the main window"),
            "{}",
            lines[1]
        );
        assert!(
            lines[1].contains("has_visible_windows=true"),
            "{}",
            lines[1]
        );
    }

    /// #3996 — one line per click on the tray's icon or its Device status,
    /// beside a reopen's, naming what it shows: the tour a start for a prompt
    /// held back, else the Overview. Until #3996 that click logged nothing.
    #[test]
    fn a_tray_click_logs_what_it_shows() {
        let _second = tracing::Dispatch::new(tracing_subscriber::registry());
        let sink = LogSink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_target(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            log_tray_open("icon", true, tray_open_view(true, false));
            log_tray_open("Device status", false, tray_open_view(false, true));
        });
        let log = String::from_utf8(sink.0.lock().unwrap_or_else(|p| p.into_inner()).clone())
            .expect("the log is UTF-8");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "one line per click:\n{log}");
        assert!(
            lines[0].contains("a tray click shows the tour")
                && lines[0].contains(r#"item="icon""#)
                && lines[0].contains("tour_held=true"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("a tray click shows the Overview")
                && lines[1].contains(r#"item="Device status""#)
                && lines[1].contains(r#"view="overview""#),
            "{}",
            lines[1]
        );
    }

    /// #3887 — on macOS, where the daemon's start (`launchctl kickstart`)
    /// carries no argument, a start asks the daemon why it came: only a start
    /// with no argument at all, for a person whose tour is not done. Never on
    /// Windows or Linux, where the start says it.
    #[test]
    fn only_a_bare_start_on_a_mac_with_the_tour_not_done_asks_the_daemon() {
        let a = |v: &[&str]| parse_launch_args(&args(v));
        assert!(asks_the_daemon(&a(&[]), false, true));
        assert!(
            !asks_the_daemon(&a(&[]), true, true),
            "a finished tour has nothing to hold back"
        );
        assert!(
            !asks_the_daemon(&a(&[]), false, false),
            "Windows and Linux never ask"
        );
        for v in [
            ["--autostart"],
            ["--first-run"],
            ["--for-prompt"],
            ["--view=devices"],
        ] {
            assert!(
                !asks_the_daemon(&a(&v), false, true),
                "{v:?}: an argument decides alone"
            );
        }
        // An argument the companion does not know (an old Finder's `-psn_…`)
        // is no argument.
        assert!(asks_the_daemon(&a(&["-psn_0_12345"]), false, true));
    }

    /// #3887 — what the daemon answers on a Mac: a start for a prompt when
    /// the companion has something to show right now, by the watcher's own
    /// rules (a question the daemon does not show natively, a session the
    /// banner is for).
    #[test]
    fn the_daemons_answer_is_what_the_companion_would_show() {
        let asked = |surface: &str| roomler_localapi::ConsentRequest {
            session_id: "0".repeat(24),
            controller_name: "ana".into(),
            permissions: "VIEW".into(),
            timeout_secs: 30,
            kind: "rc".into(),
            detail: String::new(),
            expires_at_ms: 0,
            surface: surface.into(),
            org: String::new(),
        };
        let live = |asking| roomler_localapi::RcSessionInfo {
            session_id: "1".repeat(24),
            asking,
            ..Default::default()
        };
        assert!(
            !something_to_show(&[], &[]),
            "nothing asked, nothing live: a person's or a login start"
        );
        assert!(something_to_show(&[asked("companion")], &[]));
        assert!(
            something_to_show(&[asked("")], &[]),
            "an older daemon names no surface: the companion's question"
        );
        assert!(
            !something_to_show(&[asked("native")], &[]),
            "the daemon shows that one itself"
        );
        assert!(
            something_to_show(&[asked("native"), asked("companion")], &[]),
            "one question for the companion is enough"
        );
        assert!(
            something_to_show(&[], &[live(false)]),
            "a session that sees the screen: the banner"
        );
        assert!(
            !something_to_show(&[], &[live(true)]),
            "the desktop's own dialog still asks: no banner yet"
        );
    }

    #[test]
    fn free_port_skips_excluded_and_taken_ports() {
        // Fake binder: 41080 and 41081 are taken by someone.
        let taken = [41080u16, 41081];
        let can_bind = |p: u16| !taken.contains(&p);
        assert_eq!(pick_free_port(41080, 10, &[], can_bind), Some(41082));
        // 41082 is a declared route's port (down right now, so bindable) —
        // still not ours to take.
        assert_eq!(pick_free_port(41080, 10, &[41082], can_bind), Some(41083));
        assert_eq!(
            pick_free_port(41080, 2, &[], can_bind),
            None,
            "span exhausted"
        );
        // Never wraps past 65535.
        assert_eq!(pick_free_port(65535, 10, &[], |_| false), None);
        assert_eq!(pick_free_port(65535, 10, &[], |_| true), Some(65535));
    }

    #[test]
    fn free_port_really_skips_a_bound_port() {
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let got = pick_free_port(port, 50, &[], |p| {
            std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
        })
        .expect("a free port in the span");
        assert_ne!(got, port, "the held port must be skipped");
        assert!(
            got > port && u32::from(got) < u32::from(port) + 50,
            "{got} is not inside the requested span starting at {port}"
        );
    }
}

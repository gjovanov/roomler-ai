// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! S1a PR-A — keep the `roomler-desktop` companion EXE at the daemon's
//! version.
//!
//! The desktop app is in NEITHER MSI flavour: it's a standalone release
//! asset placed beside the daemon by the setup wizard / `install.ps1`
//! (GAP-A), and the auto-updater only ever downloads `.msi` — so every
//! daemon self-update left `roomler-desktop.exe` stale forever (the
//! reported item 1 of the consolidation roadmap). This module runs at
//! daemon startup: when the sibling desktop EXE's recorded version
//! differs from our own `CARGO_PKG_VERSION`, download the matching
//! `roomler-desktop-*` asset from OUR OWN release tag (not latest — the
//! companion tracks the daemon, not the feed), rename-swap it, and
//! restart it if it was running.
//!
//! Version tracking is a sidecar marker (`roomler-desktop.exe.version`)
//! written on every successful swap — no PE version parsing. A
//! wizard-placed EXE with no marker counts as stale and gets swapped
//! once, after which the marker tracks.
//!
//! Ownership: ONE refresher per install (#1686). Under a running SCM service
//! the service host refreshes at its own start, and the worker does not —
//! the worker runs elevated by default, so "it cannot write `%ProgramFiles%`
//! and skips" stopped being true, and the two used to swap concurrently,
//! each renaming the other's fresh copy under a live companion. A per-user
//! task daemon owns `%LOCALAPPDATA%\Programs\Roomler` and refreshes itself.
//! Running companions are found by the path they were started from and
//! stopped by pid (`win_service::companion_procs`), never by image name, and
//! only once the new EXE is in place (#3824): a swap that fails leaves the
//! running tray running. Failures are always skip-and-retry, never fatal.

// FR-27: unconditional now — `ensure_running` has a real body on every
// platform, where before this module was Windows-only.
use anyhow::{Context, Result};

/// FR-84 D6 — the one-shot post-install launch: its decision table and the
/// marker that makes it one-shot.
mod launch_once;

pub use launch_once::LaunchOwner;

/// FR-117 P1 — on Linux the companion's PACKAGE tracks the daemon. The
/// decisions are pure and tested everywhere; the I/O is Linux-only.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
pub(crate) mod package_sync;

/// FR-117 P1 — move an installed companion package to this daemon's own
/// version. Linux only: there the companion is a separate `.deb`/`.rpm` that
/// nothing else moves (the macOS `.pkg` carries the app; Windows has
/// [`refresh_if_stale`]). The updater calls it as the host's ROOT update owner;
/// it is never fatal, and a failure is retried on the updater's next tick.
#[cfg(target_os = "linux")]
pub async fn sync_package() {
    let _ = package_sync::sync().await;
}

/// FR-117 P1 — would [`sync_package`] move the companion now? A per-user
/// daemon, which can install nothing, asks it to wake its root helper.
#[cfg(target_os = "linux")]
pub fn package_needs_sync() -> bool {
    package_sync::needs_sync()
}

/// FR-117 — the switch for what FR-117 added (`ROOMLERD_COMPANION_SYNC=0`): the
/// Linux package sync and the Windows retry. Windows' refresh at start is older
/// than FR-117 and stays either way.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn sync_enabled() -> bool {
    tunnel_core::env::flag(SYNC_SWITCH, true)
}

/// FR-117 P2 — [`refresh_if_stale`] again every [`REFRESH_RETRY`], for as long
/// as the process that owns the refresh runs (#1686: the SCM host, or the
/// worker). Before this a failed refresh waited for the next restart, which on
/// a laptop that is never restarted is never, and nothing said why (a corp
/// laptop sat a version behind on 2026-10-20). One file read when the version
/// marker matches.
///
/// #3781 — sooner while the companion is too old to connect to this daemon
/// ([`RefreshSchedule`]), and one log line per wait while it is.
///
/// #3824 — sooner too while a tray this process's refresh stopped is not
/// running again ([`TrayLost`]): each refresh tries to start it.
#[cfg(target_os = "windows")]
pub async fn refresh_periodically(respawn: RespawnContext) {
    retry_refresh(
        sync_enabled,
        sibling_marker,
        || TRAY_LOST.is_set(),
        || refresh_if_stale(respawn),
        |next| {
            tracing::warn!(
                installed = next.installed.as_deref().unwrap_or("<none>"),
                needs = MIN_COMPANION_VERSION,
                next_try_in_min = next.wait.as_secs() / 60,
                "desktop companion is too old to connect to this daemon; its refresh is retried"
            )
        },
    )
    .await
}

/// FR-117 P2 — the retry's period: the updater's own default interval.
const REFRESH_RETRY: std::time::Duration = crate::updater::CHECK_INTERVAL;

/// #3781 — the waits before each refresh while the companion is too old to
/// connect: 2, 5, 10 and 15 minutes, then 15 to the end of the first hour, so
/// the tries land 2, 7, 17, 32, 47 and 62 minutes in; after them the
/// updater's [`REFRESH_RETRY`]. A tray that cannot connect leaves the person
/// with no status, tunnels or settings, and what stops its refresh (a
/// download a middlebox dropped, an EXE endpoint protection holds) often
/// clears within minutes. A block that outlasts the hour gets the old period,
/// not a tight loop.
const FAST_REFRESH_WAITS: [std::time::Duration; 6] = [
    minutes(2),
    minutes(5),
    minutes(10),
    minutes(15),
    minutes(15),
    minutes(15),
];

/// #3824 — the waits while a tray this process's refresh stopped is not
/// running again ([`TrayLost`]): 1, 5 and 15 minutes, or the schedule's own
/// wait when that is shorter; after them, the schedule. Each refresh tries to
/// start the tray, so a lost tray is tried again within a minute.
const LOST_TRAY_WAITS: [std::time::Duration; 3] = [minutes(1), minutes(5), minutes(15)];

const fn minutes(n: u64) -> std::time::Duration {
    std::time::Duration::from_secs(n * 60)
}

/// #3736 — the oldest companion (tray) this daemon's LocalAPI pipe accepts
/// from a person's session: `agent-v0.4.216`, the first release whose
/// companion opens the pipe with exactly `localapi::PIPE_CLIENT_ACCESS`
/// (#3742; `agent-v0.4.215` was cut before it). Since #3760 the pipe gives
/// Interactive Users only that access, so an older companion, which opens it
/// with `GENERIC_WRITE`, is refused on every connect by a daemon that runs
/// elevated or as SYSTEM. Moves only with that DACL or the client's open.
pub(crate) const MIN_COMPANION_VERSION: &str = "0.4.216";

/// #3781 — is the companion whose version marker reads `marker` too old to
/// connect to this daemon? No marker, an unreadable one and one that names no
/// version all count as too old: nothing vouches for the open such a tray
/// makes.
pub(crate) fn predates_min_companion(marker: Option<&str>) -> bool {
    let parse = crate::updater::parse_version;
    let installed = marker_version(marker);
    match (
        installed.as_deref().and_then(parse),
        parse(MIN_COMPANION_VERSION),
    ) {
        (Some(installed), Some(minimum)) => installed < minimum,
        _ => true,
    }
}

/// #3781 — the version a marker names, bounded for a log line and the wire
/// ([`sanitize_version`]); `None` when it names none.
fn marker_version(marker: Option<&str>) -> Option<String> {
    marker
        .and_then(sanitize_version)
        .filter(|v| crate::updater::parse_version(v).is_some())
}

/// #3781 — the retry loop's memory: how many refreshes it has made in a row
/// while the companion read too old to connect, and (#3824) while a tray
/// this process stopped was not running again.
#[derive(Debug, Default)]
pub(crate) struct RefreshSchedule {
    tries: u32,
    lost_tries: u32,
}

/// #3781 — one wait of the retry loop, as [`RefreshSchedule::next`] plans it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // the log line reads it on Windows
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NextRefresh {
    /// How long the loop sleeps before its next refresh.
    pub(crate) wait: std::time::Duration,
    /// The companion is too old to connect: the loop logs one line for this
    /// wait.
    pub(crate) too_old: bool,
    /// The version its marker names; `None` when no marker reads as one.
    pub(crate) installed: Option<String>,
}

impl RefreshSchedule {
    /// The next wait, from the version marker as it reads now and whether a
    /// tray this process stopped is still not running (`tray_lost`). Called
    /// once per wait, so the loop logs at most one line per wait. A marker
    /// that reads current starts the fast waits over for the next time one
    /// reads too old.
    pub(crate) fn next(&mut self, marker: Option<&str>, tray_lost: bool) -> NextRefresh {
        let too_old = predates_min_companion(marker);
        let mut wait = next_refresh_wait(marker, self.tries);
        if tray_lost && let Some(&sooner) = LOST_TRAY_WAITS.get(self.lost_tries as usize) {
            wait = wait.min(sooner);
        }
        self.tries = if too_old {
            self.tries.saturating_add(1)
        } else {
            0
        };
        self.lost_tries = if tray_lost {
            self.lost_tries.saturating_add(1)
        } else {
            0
        };
        NextRefresh {
            wait,
            too_old,
            installed: marker_version(marker),
        }
    }
}

/// #3781 — how long the retry loop waits before its next refresh, keyed on
/// the companion's version marker: [`FAST_REFRESH_WAITS`] while it is too old
/// to connect (`tries` = the refreshes the loop has already made while it
/// read too old), else, and after them, [`REFRESH_RETRY`].
pub(crate) fn next_refresh_wait(marker: Option<&str>, tries: u32) -> std::time::Duration {
    if !predates_min_companion(marker) {
        return REFRESH_RETRY;
    }
    FAST_REFRESH_WAITS
        .get(tries as usize)
        .copied()
        .unwrap_or(REFRESH_RETRY)
}

/// #3781 — the retry loop, over what it does to the world: read the version
/// marker, refresh, log. Generic so its rhythm is tested on every OS, on
/// tokio's paused clock. One [`RefreshSchedule::next`] per wait, so one line
/// per wait while the companion is too old to connect, and never one per
/// tick: there is none.
///
/// #3824 — `lost` says whether a tray this process's refresh stopped is still
/// not running ([`TrayLost`]); the refresh itself tries to start it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) async fn retry_refresh<E, M, K, R, F, L>(
    mut enabled: E,
    mut marker: M,
    mut lost: K,
    mut refresh: R,
    mut log: L,
) where
    E: FnMut() -> bool,
    M: FnMut() -> Option<String>,
    K: FnMut() -> bool,
    R: FnMut() -> F,
    F: std::future::Future<Output = ()>,
    L: FnMut(&NextRefresh),
{
    let mut schedule = RefreshSchedule::default();
    loop {
        // `ROOMLERD_COMPANION_SYNC=0` turns the whole retry off, the faster
        // ones with it (#3824's for a lost tray too): the old period, no
        // refresh, and no line saying one is coming.
        let next = if enabled() {
            schedule.next(marker().as_deref(), lost())
        } else {
            schedule = RefreshSchedule::default();
            NextRefresh {
                wait: REFRESH_RETRY,
                too_old: false,
                installed: None,
            }
        };
        if next.too_old {
            log(&next);
        }
        tokio::time::sleep(next.wait).await;
        if enabled() {
            refresh().await;
        }
    }
}

/// #3781 — what `roomler status` says about the companion beside this daemon
/// (`localapi::NodeStatus::companion_too_old`): `Some` when the person's
/// tray cannot connect to it, `None` when it connects or this does not apply.
///
/// - `exe_present`: a companion sits beside the daemon. With none there is no
///   tray to refuse.
/// - `refuses_old`: this daemon runs elevated or as SYSTEM. Its pipe's owner
///   is then the Administrators group or SYSTEM, so the person's tray, started
///   in their own session, reaches the pipe through the Interactive Users
///   entry alone (#3760). A daemon that runs as the person owns its pipe, and
///   their tray connects through the owner's entry, whatever its age.
/// - `marker`: the version marker as read. One that names no version, or none
///   at all, is reported without `installed`, never left out.
/// - `by_service`: the machine-wide service refreshes the tray, not this
///   process.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn too_old_to_connect(
    exe_present: bool,
    refuses_old: bool,
    marker: Option<&str>,
    by_service: bool,
) -> Option<tunnel_core::localapi::CompanionTooOld> {
    (exe_present && refuses_old && predates_min_companion(marker)).then(|| {
        tunnel_core::localapi::CompanionTooOld {
            installed: marker_version(marker),
            minimum: MIN_COMPANION_VERSION.to_string(),
            by_service,
            retry_off: false,
        }
    })
}

/// #3814 — is `ROOMLERD_COMPANION_SYNC=0` set where the refresh's retry runs?
/// `roomler status` then says the tray is replaced only at a start
/// (`CompanionTooOld::retry_off`). A process reads the switch from its own
/// environment, so this asks the one the retry reads:
/// - `by_service` false: this process runs the retry, so `own_switch`
///   ([`sync_enabled`]).
/// - `local_system`: the SystemContext worker. The SCM host starts it with the
///   host's own environment block (`spawn_system_in_session` passes none of
///   its own), so `own_switch` as well.
/// - Otherwise: a worker the host started as the person, from the person's own
///   block (`win_service::supervisor::spawn_in_session`), which leaves the
///   service's `Environment` value out. So `service_switch`
///   (`service_sync_enabled`, on Windows), never this process's own.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn retry_off_where_it_runs(
    by_service: bool,
    local_system: Option<bool>,
    own_switch: impl FnOnce() -> bool,
    service_switch: impl FnOnce() -> bool,
) -> bool {
    let enabled = if !by_service || local_system == Some(true) {
        own_switch()
    } else {
        service_switch()
    };
    !enabled
}

/// What [`sync_enabled`] reads, after one of `tunnel_core::env::PREFIXES`.
const SYNC_SWITCH: &str = "COMPANION_SYNC";

/// #3814 — [`sync_enabled`] over the service's `Environment` value: the switch
/// under the first of `tunnel_core::env::PREFIXES` it is named with, in any
/// case (Windows names a variable in any case), read as `flag` reads it. A
/// value that could not be read (`None`), or one that does not name the
/// switch, leaves the retry on: `status` never says it is off on a guess.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn sync_enabled_in(service_env: Option<&[(String, String)]>) -> bool {
    let value = service_env.and_then(|env| {
        tunnel_core::env::PREFIXES.iter().find_map(|prefix| {
            let name = format!("{prefix}{SYNC_SWITCH}");
            env.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(&name))
                .map(|(_, v)| v.as_str())
        })
    });
    tunnel_core::env::flag_value(value, true)
}

/// #3814 — [`sync_enabled_in`] for this install's service, as the SCM host was
/// given it. The host's environment is fixed from its start, and a stop of the
/// service ends its worker, so the first read that succeeds stands for this
/// process's life. A read that fails leaves the retry on, and the next answer
/// reads again.
#[cfg(target_os = "windows")]
fn service_sync_enabled() -> bool {
    static READ: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if let Some(enabled) = READ.get() {
        return *enabled;
    }
    match crate::win_service::environment::read_service_env() {
        Ok(env) => *READ.get_or_init(|| sync_enabled_in(Some(&env))),
        Err(_) => sync_enabled_in(None),
    }
}

/// #3781 — [`too_old_to_connect`] for the companion beside this process, read
/// fresh on every status answer: on a machine-wide install the service host,
/// another process, may have just refreshed it. What this process cannot read
/// counts as reportable, so the field's absence never stands for "could not
/// tell". #3814: with whether the retry is off where it runs
/// ([`retry_off_where_it_runs`]), read only for a tray reported too old.
#[cfg(target_os = "windows")]
pub(crate) fn too_old_here() -> Option<tunnel_core::localapi::CompanionTooOld> {
    if !pipe_refuses_old_companions() {
        return None;
    }
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf));
    let (exe_present, marker) = match &dir {
        Some(dir) => (
            dir.join(DESKTOP_EXE).try_exists().unwrap_or(true),
            std::fs::read_to_string(dir.join(VERSION_MARKER)).ok(),
        ),
        // No folder of its own to look in: a tray of unknown version.
        None => (true, None),
    };
    let by_service =
        crate::supervision::current() == Some(crate::supervision::Supervision::WindowsScm);
    too_old_to_connect(exe_present, true, marker.as_deref(), by_service).map(|reported| {
        tunnel_core::localapi::CompanionTooOld {
            retry_off: retry_off_where_it_runs(
                by_service,
                crate::win_identity::own_token_is_local_system(),
                sync_enabled,
                service_sync_enabled,
            ),
            ..reported
        }
    })
}

/// #3781 — does this daemon's pipe refuse a companion older than
/// [`MIN_COMPANION_VERSION`]? Yes when it runs as LocalSystem or elevated:
/// its pipe's owner is then SYSTEM or the Administrators group, not the
/// person. A token this process cannot read counts as refusing. Read once: a
/// process's token never changes.
#[cfg(target_os = "windows")]
fn pipe_refuses_old_companions() -> bool {
    static REFUSES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REFUSES.get_or_init(|| match crate::win_identity::own_token_is_local_system() {
        Some(false) => crate::win_identity::process_is_elevated(),
        Some(true) | None => true,
    })
}

/// The version marker beside this daemon, as read; `None` when it cannot be.
#[cfg(target_os = "windows")]
fn sibling_marker() -> Option<String> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    std::fs::read_to_string(dir.join(VERSION_MARKER)).ok()
}

/// FR-84 D6 — open the companion ONCE after a fresh install, from whichever
/// process owns that on this host: the SCM host on a perMachine Windows
/// install, the worker everywhere else (per-user Scheduled Task, systemd
/// unit). Spawn-and-forget; waits up to ten minutes for what a fresh install
/// is still missing (the enrollment, the companion, a logged-on user), and
/// never launches twice — or at all on an install that ran before. See
/// `companion/launch_once.rs`.
pub async fn launch_once_after_install(owner: LaunchOwner) {
    launch_once::run(owner).await
}

/// FR-84 D6 — what the SCM service host does for the companion at every
/// start, after the version refresh: bring the machine-wide Run value in line
/// with the device switch and the installed EXE (the self-heal), then run the
/// post-install launcher.
#[cfg(target_os = "windows")]
pub async fn scm_host_startup() {
    sync_machine_autostart_logged(scm_companion_autostart());
    launch_once_after_install(LaunchOwner::ScmHost).await;
}

/// The device's `companion_autostart` as the SCM host can see it: from the
/// config the worker uses (machine-global, else the console user's); ON when
/// none is readable yet (a fresh install before enrollment).
#[cfg(target_os = "windows")]
pub fn scm_companion_autostart() -> bool {
    use crate::win_service::supervisor;
    let token = supervisor::active_console_session_id()
        .and_then(|sid| supervisor::query_user_token(sid).ok().flatten());
    launch_once::scm_worker_config(token.as_ref()).is_none_or(|c| c.companion_autostart)
}

/// FR-84 D6 — `HKLM\…\Run\Roomler Desktop`: present iff the device switch is
/// on and the companion sits beside this daemon. Elevated callers only (the
/// SCM host, `service install --as-service`). Logs a change, not a no-op.
#[cfg(target_os = "windows")]
pub fn sync_machine_autostart_logged(companion_autostart: bool) {
    use roomler_node_core::companion_autostart::registry::Hive;
    // A person's opt-out never removes the MACHINE value — it serves every
    // account; the companion honours the opt-out at `--autostart` instead.
    sync_login_autostart(Hive::LocalMachine, companion_autostart, false);
}

/// FR-84 D6 — `HKCU\…\Run\Roomler Desktop` for a per-user install: present
/// iff the device switch is on, the companion is installed, and THIS person
/// has not switched "Start at login" off (the companion's own state).
#[cfg(target_os = "windows")]
pub fn sync_user_autostart_logged(companion_autostart: bool) {
    use roomler_node_core::companion_autostart::registry::Hive;
    let opted_out = roomler_node_core::desktop_state::load().autostart_opt_out;
    sync_login_autostart(Hive::CurrentUser, companion_autostart, opted_out);
}

#[cfg(target_os = "windows")]
fn sync_login_autostart(
    hive: roomler_node_core::companion_autostart::registry::Hive,
    companion_autostart: bool,
    opted_out: bool,
) {
    use roomler_node_core::companion_autostart::{
        RunKeyAction, WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME, desired_run_value,
        registry::sync_value,
    };
    let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(DESKTOP_EXE)))
    else {
        return;
    };
    let desired = desired_run_value(companion_autostart, &exe, exe.exists(), opted_out);
    match sync_value(
        hive,
        WINDOWS_RUN_SUBKEY,
        WINDOWS_RUN_VALUE_NAME,
        desired.as_deref(),
    ) {
        Ok(RunKeyAction::Keep) => {}
        Ok(action) => tracing::info!(?hive, ?action, "companion login start (Run value) updated"),
        Err(e) => {
            tracing::warn!(?hive, error = %e, "companion login start (Run value): could not update")
        }
    }
}

/// FR-84 D6 — remove the Run value an uninstall leaves behind (`Ok(true)` =
/// there was one).
#[cfg(target_os = "windows")]
pub fn remove_login_autostart(machine: bool) -> Result<bool> {
    use roomler_node_core::companion_autostart::{
        WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME,
        registry::{Hive, delete_value},
    };
    let hive = if machine {
        Hive::LocalMachine
    } else {
        Hive::CurrentUser
    };
    delete_value(hive, WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME)
        .with_context(|| format!("removing the {hive:?} Run value"))
}

/// Who is calling the refresh — decides how a running desktop gets
/// respawned after the swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespawnContext {
    /// Caller runs as LocalSystem (SCM service host / SystemContext
    /// worker): respawn into the active interactive session via
    /// `CreateProcessAsUserW`.
    SystemService,
    /// Caller runs as the interactive user (perUser task daemon,
    /// attended run): plain spawn.
    UserSession,
}

#[cfg(target_os = "windows")]
pub const DESKTOP_EXE: &str = "roomler-desktop.exe";
#[cfg(target_os = "windows")]
const VERSION_MARKER: &str = "roomler-desktop.exe.version";
#[cfg(target_os = "windows")]
const OLD_SUFFIX: &str = "roomler-desktop.exe.old";
#[cfg(target_os = "windows")]
const NEW_SUFFIX: &str = "roomler-desktop.exe.new";

/// FR-27 — make sure the desktop companion is RUNNING, because it is the only
/// thing that renders a consent prompt.
///
/// Called when an attended prompt begins. Before this nothing started it: a
/// device set to `Prompt on host` whose operator had quit the menu-bar app (or
/// never launched it) showed nothing at all, sat out its 30 s window, and told
/// the controller "the user denied your request".
///
/// Best-effort by construction, and deliberately not fatal. Every failure path
/// logs WHY — that log line, plus the `no_prompt_surface` the caller then sends,
/// is the difference between a diagnosable refusal and a mystery.
///
/// ⚠️ Rate-limited process-wide. A burst of session requests must not turn into
/// a burst of `launchctl kickstart` / `CreateProcessAsUserW` calls; one attempt
/// per [`ENSURE_COOLDOWN`] is plenty, since a launch that works is visible
/// within a second and one that doesn't will not work any better on retry.
/// Returns whether a human can now, in fact, be shown a prompt on this host.
///
/// The caller reports `no_prompt_surface` on `false`, which is why this is a
/// verdict and not a fire-and-forget: without it the agent could only ever
/// report a TIMEOUT, and "nobody answered in 30 s" and "there is nobody to
/// ask, and there never was" need different things done about them.
pub async fn ensure_running() -> bool {
    use std::sync::Mutex;
    use std::time::Instant;
    // Both halves of the rate limit: when we last tried, and what we concluded.
    // A cooldown that returned an optimistic default would report a surface
    // that does not exist — worse than not rate-limiting at all.
    static LAST: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

    if let Some((at, verdict)) = *LAST.lock().unwrap()
        && at.elapsed() < ENSURE_COOLDOWN
    {
        return verdict;
    }

    let verdict = match ensure_running_inner().await {
        Ok(EnsureOutcome::AlreadyRunning) => {
            tracing::debug!("desktop companion is already running");
            true
        }
        Ok(EnsureOutcome::Started) => {
            tracing::info!("started the desktop companion for a prompt");
            true
        }
        Ok(EnsureOutcome::Unsupported) => {
            tracing::info!(
                "no desktop companion on this host — an on-screen prompt is not possible; \
                 answer with `roomlerd consent --list` / `--approve`, or set this device to \
                 email/push consent"
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "could not start the desktop companion — an on-screen prompt is not possible"
            );
            false
        }
    };
    *LAST.lock().unwrap() = Some((Instant::now(), verdict));
    verdict
}

/// One attempt per this interval, process-wide.
const ENSURE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(20);

/// Whether the desktop companion can show a consent question, and a
/// recording's banner. A remote recording asks through it, and so does an SSH
/// session (#3442), so a test can hold the companion's start open while the
/// question is already on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Companion {
    /// Ask the system: start the companion if it is installed and not
    /// running ([`ensure_running`], bounded).
    System,
    /// A fixed answer — for tests, which must never launch the real app.
    Fixed(bool),
    /// Up, and waited for as [`Self::System`] is, on a test's own clock.
    Timed {
        poll: std::time::Duration,
        ceiling: std::time::Duration,
    },
    /// Up once this long has passed: a companion that had to be started, which
    /// [`Self::System`] waits for (up to its start budget). For tests, which
    /// need that wait while the question is already on the screen.
    Starting(std::time::Duration),
}

impl Companion {
    /// Is the companion up to show the question? [`Self::System`] waits for
    /// a start at most [`crate::signaling::COMPANION_START_BUDGET`].
    ///
    /// Compiled where something asks through it: a remote recording, and an
    /// SSH session. Built without either, it is dead code, and every
    /// `-D warnings` lane on such a build fails.
    #[cfg(any(feature = "recording", feature = "ssh-server"))]
    pub(crate) async fn up(self) -> bool {
        match self {
            Self::System => {
                tokio::time::timeout(crate::signaling::COMPANION_START_BUDGET, ensure_running())
                    .await
                    .unwrap_or(false)
            }
            Self::Fixed(up) => up,
            Self::Timed { .. } => true,
            Self::Starting(after) => {
                tokio::time::sleep(after).await;
                true
            }
        }
    }
}

/// FR-27 — the version of the companion INSTALLED on this host, for the
/// heartbeat, or `None` when there is none / it cannot be read.
///
/// This exists because the daemon and the companion update through different
/// mechanisms on all three platforms (see [`refresh_if_stale`]), so a fleet
/// "Update all" moving `agent_version` says nothing about the companion. The
/// operator's report was exactly that: the daemon went forward and the desktop
/// stayed behind, with nothing on screen to say so.
///
/// ⚠️ Deliberately does NOT run the companion binary. `--version` on a GUI
/// binary is a process spawn on every heartbeat and, on Linux, a GTK-linked
/// executable started by a root daemon with no display; both are worse than a
/// file read. Every arm reads metadata the installer already wrote.
///
/// Cached for [`VERSION_TTL`]: the answer only changes when a package manager
/// runs, and the heartbeat is every 30 s.
pub fn installed_version() -> Option<String> {
    use std::time::Instant;

    if let Some((at, ref v)) = *VERSION_CACHE.lock().unwrap()
        && at.elapsed() < VERSION_TTL
    {
        return v.clone();
    }
    let v = installed_version_inner();
    *VERSION_CACHE.lock().unwrap() = Some((Instant::now(), v.clone()));
    v
}

const VERSION_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// [`installed_version`]'s cache: when it last read, and what.
static VERSION_CACHE: std::sync::Mutex<Option<(std::time::Instant, Option<String>)>> =
    std::sync::Mutex::new(None);

/// FR-117 — drop the cached version after something moved the companion, so
/// the next heartbeat reports the new one instead of a 10-minute-old answer
/// (the grid's skew would otherwise outlive the fix by that long).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn forget_installed_version() {
    *VERSION_CACHE.lock().unwrap() = None;
}

/// Windows: the sidecar marker [`refresh_if_stale`] writes on every swap. A
/// wizard-placed EXE with no marker reads as `None`, which is honest — the
/// daemon genuinely does not know what version it is until it swaps it once.
#[cfg(target_os = "windows")]
fn installed_version_inner() -> Option<String> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    if !dir.join(DESKTOP_EXE).exists() {
        return None;
    }
    let raw = std::fs::read_to_string(dir.join(VERSION_MARKER)).ok()?;
    sanitize_version(&raw)
}

/// macOS: `CFBundleShortVersionString` out of the app bundle's `Info.plist`.
/// Scanned as text rather than parsed: the plist is one we generate ourselves
/// (`release-agent.yml`), in the XML form, and a plist crate for one string
/// would be a dependency the thin-client work (P3e lever E) just spent effort
/// removing.
#[cfg(target_os = "macos")]
fn installed_version_inner() -> Option<String> {
    const PLIST: &str = "/Applications/Roomler.app/Contents/Info.plist";
    let text = std::fs::read_to_string(PLIST).ok()?;
    let after = text.split("<key>CFBundleShortVersionString</key>").nth(1)?;
    let open = after.find("<string>")? + "<string>".len();
    let close = after[open..].find("</string>")? + open;
    sanitize_version(&after[open..close])
}

/// Linux: ask the package database that OWNS the companion here: dpkg for its
/// `roomler-desktop` .deb, rpm for the .rpm (FR-93 P6d). Reading the package
/// database is the one answer that stays true when the package manager
/// upgrades the companion without the daemon noticing.
#[cfg(all(unix, not(target_os = "macos")))]
fn installed_version_inner() -> Option<String> {
    installed_package().map(|(_, v)| v)
}

/// The companion's package on this host: which database owns it, and its
/// version as the release it names. FR-117 needs the first half too, because
/// the format of the package that moves it forward is that database's.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn installed_package() -> Option<(package_sync::PkgDb, String)> {
    // Not installed, or no such tool on this host: "no companion we can name",
    // not an error worth logging every 10 min.
    let query = |cmd: &str, args: &[&str]| -> Option<String> {
        let out = std::process::Command::new(cmd).args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    if let Some(raw) = query(
        "dpkg-query",
        &["-W", "-f=${Status}|${Version}", "roomler-desktop"],
    ) {
        // ⚠️ `dpkg-query -W` also answers for a package dpkg merely REMEMBERS:
        // removed with its config kept (`deinstall ok config-files`), version
        // and all. That is not an installed companion, and FR-117 must never
        // put back one a person removed.
        if let Some(v) = package_sync::dpkg_installed_version(&raw) {
            // cargo-deb appends a Debian revision (`0.4.16-1`); report the
            // upstream half so the grid can compare it with `agent_version`.
            return sanitize_version(v.split('-').next().unwrap_or(""))
                .map(|v| (package_sync::PkgDb::Dpkg, v));
        }
    }
    // `%{VERSION}` is the upstream half already (the release is its own tag).
    query("rpm", &["-q", "--qf", "%{VERSION}", "roomler-desktop"])
        .and_then(|raw| sanitize_version(&raw))
        .map(|v| (package_sync::PkgDb::Rpm, v))
}

/// A version string is about to be persisted on the device row and rendered in
/// the grid, and every arm above reads it off the host — so bound it. Empty
/// stays `None` (absent and blank must not look different downstream).
fn sanitize_version(raw: &str) -> Option<String> {
    let v: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
        .take(32)
        .collect();
    (!v.is_empty()).then_some(v)
}

#[cfg(test)]
mod version_tests {
    use super::sanitize_version;

    #[test]
    fn blank_and_whitespace_are_absent_not_empty() {
        assert_eq!(sanitize_version(""), None);
        assert_eq!(sanitize_version("   \n"), None);
    }

    #[test]
    fn ordinary_versions_survive_intact() {
        assert_eq!(sanitize_version("0.4.16\n"), Some("0.4.16".into()));
        assert_eq!(
            sanitize_version("0.3.0-rc.483"),
            Some("0.3.0-rc.483".into())
        );
    }

    #[test]
    fn host_supplied_junk_cannot_reach_the_grid() {
        // The Windows arm reads a file anyone able to write next to the daemon
        // could edit; the value ends up in a Vue table.
        assert_eq!(
            sanitize_version("<script>alert(1)</script>"),
            Some("scriptalert1script".into())
        );
        assert_eq!(sanitize_version(&"9".repeat(200)).unwrap().len(), 32);
    }
}

/// FR-97 P5a — who is at the screen, read from what `loginctl` and `stat`
/// print. Pure, so every platform's test build runs them.
#[cfg(test)]
mod console_tests {
    use super::{
        console_owner_person, seat0_active_session, seat0_person_uid, session_account,
        session_person, session_person_uid, show_field,
    };

    /// `key=value` lines, the whole key and nothing but it: `NameX=` is not
    /// `Name`, and an empty value is no value.
    #[test]
    fn a_show_field_is_the_whole_key() {
        let text = "NameX=wrong\nName=ana\nDisplay=\nType=wayland\n";
        assert_eq!(show_field(text, "Name").as_deref(), Some("ana"));
        assert_eq!(show_field(text, "Display"), None);
        assert_eq!(show_field(text, "Seat"), None);
    }

    /// `loginctl show-seat seat0` with a session showing, and with none
    /// (a headless server, or a VM before anyone logged in).
    #[test]
    fn seat0s_active_session_or_nobody() {
        let seat = "Id=seat0\nActiveSession=2\nCanGraphical=yes\nSessions=2 c1\n";
        assert_eq!(seat0_active_session(seat).unwrap(), "2");
        let empty = "Id=seat0\nActiveSession=\nCanGraphical=no\nSessions=\n";
        let e = seat0_active_session(empty).unwrap_err().to_string();
        assert!(e.contains("seat0 has no active session"), "{e}");
    }

    /// The session's person: a user's graphical or text-console login is a
    /// person; GDM's greeter is the login screen; another class is nobody's.
    #[test]
    fn the_session_showing_is_a_person_or_the_login_screen() {
        let user = "Id=2\nName=ana\nUser=1000\nSeat=seat0\nType=wayland\nClass=user\nActive=yes\n";
        assert_eq!(session_person("2", user).unwrap(), "ana");
        let tty = "Id=5\nName=ops\nUser=1001\nSeat=seat0\nType=tty\nClass=user\nActive=yes\n";
        assert_eq!(session_person("5", tty).unwrap(), "ops");

        let greeter = "Id=c1\nName=gdm\nUser=120\nSeat=seat0\nType=wayland\nClass=greeter\n";
        let e = session_person("c1", greeter).unwrap_err().to_string();
        assert!(e.contains("the login screen is showing"), "{e}");

        let lock = "Id=9\nName=x\nUser=1000\nType=x11\nClass=lock-screen\n";
        let e = session_person("9", lock).unwrap_err().to_string();
        assert!(e.contains("class lock-screen, not a person's"), "{e}");

        let unnamed = "Id=3\nUser=1000\nType=x11\nClass=user\n";
        let e = session_person("3", unnamed).unwrap_err().to_string();
        assert!(e.contains("names no account"), "{e}");
    }

    /// #3688 — a GNOME or KDE lock keeps the person's own session showing on
    /// seat0, marked `LockedHint=yes`: it is still their person, for a
    /// viewer's reads as for a received file. LightDM's light-locker switches
    /// seat0 to a greeter session instead, which is nobody, as the login
    /// screen is. The uid logind recorded comes with the name, so root
    /// signed in at the screen is told from a person.
    #[test]
    fn a_locked_screen_keeps_its_person() {
        let seat = "Id=seat0\nActiveSession=2\nCanGraphical=yes\nSessions=2\n";
        assert_eq!(seat0_active_session(seat).unwrap(), "2");
        let locked = "Id=2\nName=ana\nUser=1000\nSeat=seat0\nType=wayland\nClass=user\n\
                      Active=yes\nState=active\nLockedHint=yes\nRemote=no\n";
        assert_eq!(session_person("2", locked).unwrap(), "ana");
        assert_eq!(
            session_account("2", locked).unwrap(),
            ("ana".to_string(), Some(1000))
        );

        let light_locker = "Id=c2\nName=lightdm\nUser=111\nSeat=seat0\nType=x11\nClass=greeter\n\
                            Active=yes\nRemote=no\n";
        let e = session_account("c2", light_locker).unwrap_err().to_string();
        assert!(e.contains("the login screen is showing"), "{e}");

        let root = "Id=4\nName=root\nUser=0\nSeat=seat0\nType=tty\nClass=user\nRemote=no\n";
        assert_eq!(
            session_account("4", root).unwrap(),
            ("root".to_string(), Some(0))
        );
        let no_uid = "Id=6\nName=ops\nSeat=seat0\nType=tty\nClass=user\n";
        assert_eq!(
            session_account("6", no_uid).unwrap(),
            ("ops".to_string(), None)
        );
    }

    /// #2871 (review) — whom a root daemon's person socket is made for:
    /// seat0's person, and never a remote desktop's session (xrdp, GNOME
    /// Remote Login: no seat, `Remote=yes`, and active, as logind calls every
    /// seatless session), whatever order logind lists it in. The first active
    /// graphical session anywhere, which the socket took before this, was
    /// the remote one whenever logind listed it first.
    #[test]
    fn a_seatless_remote_session_is_never_at_the_seat() {
        // bob (1001) in an xrdp session logind lists FIRST; ana (1000) at
        // seat0's screen; GDM's greeter on seat0 when nobody is signed in.
        let xrdp = "Id=1\nUser=1001\nName=bob\nType=x11\nClass=user\nActive=yes\nState=active\n\
                    Remote=yes\nRemoteHost=192.0.2.9\nService=xrdp-sesman\n";
        let ana = "Id=2\nUser=1000\nName=ana\nSeat=seat0\nType=wayland\nClass=user\nActive=yes\n\
                   Remote=no\n";
        let gdm = "Id=c1\nUser=120\nName=gdm\nSeat=seat0\nType=wayland\nClass=greeter\n\
                   Active=yes\nRemote=no\n";
        let read = std::cell::RefCell::new(Vec::<String>::new());
        let show = |id: &str| -> anyhow::Result<String> {
            read.borrow_mut().push(id.to_string());
            Ok(match id {
                "1" => xrdp,
                "2" => ana,
                "c1" => gdm,
                other => anyhow::bail!("no session {other}"),
            }
            .to_string())
        };
        // seat0 shows ana: she is at the seat.
        let seat = "Id=seat0\nActiveSession=2\nCanGraphical=yes\nSessions=2 c1\n";
        assert_eq!(seat0_person_uid(seat, show).unwrap(), 1000);
        // seat0 shows the login screen: nobody is at the seat — not bob,
        // although his is the only person's session on the machine.
        let seat = "Id=seat0\nActiveSession=c1\nCanGraphical=yes\nSessions=c1\n";
        let e = seat0_person_uid(seat, show).unwrap_err().to_string();
        assert!(e.contains("the login screen is showing"), "{e}");
        // A headless box whose only sessions are remote: nobody.
        let seat = "Id=seat0\nActiveSession=\nCanGraphical=no\nSessions=\n";
        assert!(seat0_person_uid(seat, show).is_err());
        // The remote session was never even read.
        assert_eq!(*read.borrow(), ["2", "c1"]);
        // Were seat0 ever to name a remote or a seatless session, it is
        // refused by name, not served.
        let e = session_person_uid("1", xrdp).unwrap_err().to_string();
        assert!(e.contains("remote login"), "{e}");
        let seatless = "Id=3\nUser=1002\nName=carol\nType=x11\nClass=user\nActive=yes\nRemote=no\n";
        let e = session_person_uid("3", seatless).unwrap_err().to_string();
        assert!(e.contains("not on seat0"), "{e}");
        // The uid is logind's, from the session the name comes from; a text
        // console at seat0 is the person at the device too (`seat0_account`).
        assert_eq!(session_person_uid("2", ana).unwrap(), 1000);
        let tty = "Id=5\nName=ops\nUser=1003\nSeat=seat0\nType=tty\nClass=user\nRemote=no\n";
        assert_eq!(session_person_uid("5", tty).unwrap(), 1003);
        let no_uid = "Id=6\nName=x\nSeat=seat0\nType=x11\nClass=user\nRemote=no\n";
        let e = session_person_uid("6", no_uid).unwrap_err().to_string();
        assert!(e.contains("names no uid"), "{e}");
    }

    /// `/dev/console`'s owner: a person, the login window (root), or a
    /// system account during Setup Assistant.
    #[test]
    fn the_consoles_owner_is_a_person_or_nobody() {
        assert_eq!(console_owner_person("ana\n").unwrap(), "ana");
        for login_window in ["root\n", "", "  \n"] {
            let e = console_owner_person(login_window).unwrap_err().to_string();
            assert!(e.contains("the login window is showing"), "{e}");
        }
        let e = console_owner_person("_mbsetupuser\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("system account _mbsetupuser"), "{e}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnsureOutcome {
    AlreadyRunning,
    Started,
    /// No companion ships for this platform / this install shape.
    Unsupported,
}

#[cfg(target_os = "windows")]
async fn ensure_running_inner() -> Result<EnsureOutcome> {
    if desktop_running() {
        return Ok(EnsureOutcome::AlreadyRunning);
    }
    let exe_dir = std::env::current_exe()
        .context("locating own exe")?
        .parent()
        .context("own exe has no parent dir")?
        .to_path_buf();
    let dest = exe_dir.join(DESKTOP_EXE);
    if !dest.exists() {
        // Not an error: `install.ps1 -SkipDesktop` is a supported choice, and
        // so is a daemon-only deployment. The caller reports
        // `no_prompt_surface` either way — this just keeps the log honest
        // about which of the two happened.
        return Ok(EnsureOutcome::Unsupported);
    }
    // A SYSTEM daemon has no desktop of its own — the companion has to be
    // launched INTO the interactive session, which is exactly what the
    // update-respawn path already does.
    let ctx = respawn_context_for_self();
    started_outcome(start_desktop(ctx, &dest, &[]))
}

/// #3824 — the consent path's verdict on a start: `Started` only when the
/// process was created. A failed start, or nobody signed in to start it for,
/// is an error that [`ensure_running`] logs, and the prompt has no surface.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn started_outcome(start: TrayStart) -> Result<EnsureOutcome> {
    match start {
        TrayStart::Started { .. } => Ok(EnsureOutcome::Started),
        not_started => Err(anyhow::anyhow!("{}", not_started.describe())),
    }
}

/// Which spawn shape this process needs. Mirrors the probe `app.rs` does for
/// the update path, kept here so the consent path cannot pick a different one.
#[cfg(target_os = "windows")]
fn respawn_context_for_self() -> RespawnContext {
    #[cfg(feature = "system-context")]
    {
        use crate::system_context::worker_role;
        if matches!(
            worker_role::probe_self(),
            Ok(worker_role::WorkerRole::SystemContext)
        ) {
            return RespawnContext::SystemService;
        }
    }
    RespawnContext::UserSession
}

#[cfg(target_os = "macos")]
async fn ensure_running_inner() -> Result<EnsureOutcome> {
    const BUNDLE: &str = "/Applications/Roomler.app";
    const LABEL: &str = "com.roomler.desktop";

    if !std::path::Path::new(BUNDLE).exists() {
        // Not an error, for the same reason as the Windows arm: a daemon-only
        // install is a supported shape, and the caller reports
        // `no_prompt_surface` either way. Saying WHICH keeps the log honest.
        return Ok(EnsureOutcome::Unsupported);
    }
    if pgrep_running("roomler-desktop") {
        return Ok(EnsureOutcome::AlreadyRunning);
    }
    // The companion is a LaunchAgent in the console user's GUI domain. Ask
    // launchd rather than spawning it ourselves: the daemon half runs as root,
    // and a root-spawned GUI app would land in the wrong session with the
    // wrong TCC identity. `kickstart` is also idempotent.
    //
    // The console user is resolved the way the pkg's postinstall does
    // (`stat -f %Su /dev/console`) so the two agree by construction.
    let uid = console_user_uid().context("resolving the console user")?;
    let target = format!("gui/{uid}/{LABEL}");
    let out = std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .output()
        .context("spawning launchctl")?;
    if !out.status.success() {
        anyhow::bail!(
            "launchctl kickstart {target} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(EnsureOutcome::Started)
}

/// The uid of whoever owns the console (the GUI session). `None` at the login
/// window or over SSH — where there is, correctly, nobody to prompt.
#[cfg(target_os = "macos")]
fn console_user_uid() -> Result<u32> {
    let user = console_user_name()?;
    let out = std::process::Command::new("id")
        .args(["-u", &user])
        .output()
        .context("id -u")?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u32>()
        .with_context(|| format!("parsing the uid of console user '{user}'"))
}

/// The account of the person signed in at this Mac's screen: the owner of
/// `/dev/console`, which loginwindow hands to whoever logs in at the GUI and
/// takes back to root at the login window. Read the way the pkg's
/// postinstall reads it (`stat -f %Su /dev/console`), so the companion's
/// launch, the postinstall and FR-97 P5a's `console_user` agree by
/// construction. `Err` at the login window, and while the console belongs to
/// a system account (`_mbsetupuser` runs Setup Assistant).
#[cfg(target_os = "macos")]
pub(crate) fn console_user_name() -> Result<String> {
    let out = std::process::Command::new("stat")
        .args(["-f", "%Su", "/dev/console"])
        .output()
        .context("stat /dev/console")?;
    console_owner_person(&String::from_utf8_lossy(&out.stdout))
}

/// The pure half of [`console_user_name`]: `/dev/console`'s owner as `stat`
/// printed it, a person's account or why there is none. macOS names every
/// system account with a leading `_`.
#[cfg(any(target_os = "macos", test))]
fn console_owner_person(owner: &str) -> Result<String> {
    let user = owner.trim();
    if user.is_empty() || user == "root" {
        anyhow::bail!("nobody is signed in at this Mac's screen (the login window is showing)");
    }
    if user.starts_with('_') {
        anyhow::bail!(
            "nobody is signed in at this Mac's screen (the console belongs to the system \
             account {user})"
        );
    }
    Ok(user.to_string())
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn ensure_running_inner() -> Result<EnsureOutcome> {
    #[cfg(test)]
    let steps = tray_stand_in::now().map_or_else(TraySteps::real, |stood| stood.steps);
    #[cfg(not(test))]
    let steps = TraySteps::real();
    start_tray(&steps).map_err(anyhow::Error::msg)
}

/// #3855 — the blocking steps of the Linux tray's start for a prompt: where
/// the companion is installed, the graphical session (logind), whether that
/// person's tray runs (the process list), the session's environment for a
/// root daemon's unit (the person's user manager, reached as them through the
/// account database) and systemd-run itself. Each is a seam a test stands in;
/// the daemon's are [`TraySteps::real`].
#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Clone)]
struct TraySteps {
    path: std::sync::Arc<PathStep>,
    session: std::sync::Arc<SessionStep>,
    running: std::sync::Arc<RunningStep>,
    environment: std::sync::Arc<EnvironmentStep>,
    systemd_run: std::sync::Arc<SystemdRunStep>,
    /// A per-user daemon starts the tray through its own user manager, a
    /// root one through the system manager as the person ([`start_tray`]).
    user_manager: bool,
}

/// Where the companion is installed, if it is.
#[cfg(all(unix, not(target_os = "macos")))]
type PathStep = dyn Fn() -> Option<std::path::PathBuf> + Send + Sync;
/// The graphical session to start it in, or why there is none.
#[cfg(all(unix, not(target_os = "macos")))]
type SessionStep = dyn Fn() -> Result<GraphicalSession> + Send + Sync;
/// Whether the person with this uid already runs it.
#[cfg(all(unix, not(target_os = "macos")))]
type RunningStep = dyn Fn(u32) -> bool + Send + Sync;
/// The session's environment, as `systemctl --user show-environment` prints it.
#[cfg(all(unix, not(target_os = "macos")))]
type EnvironmentStep = dyn Fn(&GraphicalSession) -> Option<String> + Send + Sync;
/// systemd-run, run.
#[cfg(all(unix, not(target_os = "macos")))]
type SystemdRunStep =
    dyn Fn(std::process::Command) -> std::io::Result<std::process::Output> + Send + Sync;

#[cfg(all(unix, not(target_os = "macos")))]
impl TraySteps {
    /// The daemon's own steps.
    fn real() -> Self {
        Self {
            path: std::sync::Arc::new(linux_companion_path),
            session: std::sync::Arc::new(graphical_session),
            running: std::sync::Arc::new(companion_running_for_uid),
            environment: std::sync::Arc::new(user_manager_environment),
            systemd_run: std::sync::Arc::new(|mut cmd: std::process::Command| cmd.output()),
            // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
            user_manager: unsafe { libc::geteuid() } != 0,
        }
    }
}

/// The Linux tray's start, blocking: its steps in the order the start has
/// always taken them. `Started` only when systemd-run exited 0.
#[cfg(all(unix, not(target_os = "macos")))]
fn start_tray(steps: &TraySteps) -> std::result::Result<EnsureOutcome, String> {
    let Some(path) = (steps.path)() else {
        // Until FR-27's packaging phase there is no Linux companion at all,
        // and after it there still won't be on a headless server — which is
        // the correct state for a machine with no screen, not a fault.
        return Ok(EnsureOutcome::Unsupported);
    };

    // A root systemd daemon has no display of its own. Find the graphical
    // session's owner and its bus/display, then spawn as that user.
    let sess =
        (steps.session)().map_err(|e| format!("finding a graphical login session: {e:#}"))?;
    // FR-93 P6b — THAT user's companion (`pgrep -u`: another user's on a
    // shared machine is not this person's), started through the manager this
    // daemon may use. A per-user daemon has its own user manager; the system
    // manager would ask polkit for a password nobody is there to type. The
    // shape `launch_once` already has.
    if (steps.running)(sess.uid) {
        return Ok(EnsureOutcome::AlreadyRunning);
    }
    // #2713 — a root daemon's system unit carries the desktop session's own
    // environment ([`systemd_run_command`]); a per-user daemon's `--user`
    // unit inherits its manager's.
    let session_env = if steps.user_manager {
        Vec::new()
    } else {
        (steps.environment)(&sess)
            .map(|text| session_env_pairs(&text))
            .unwrap_or_default()
    };
    let cmd = systemd_run_command_with(&sess, &path, &[], steps.user_manager, &session_env);
    let out = (steps.systemd_run)(cmd).map_err(|e| format!("spawning systemd-run: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "systemd-run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(EnsureOutcome::Started)
}

/// #3855 — how long a prompt waits for the Linux tray's start.
#[cfg(all(unix, not(target_os = "macos")))]
#[cfg_attr(not(test), allow(dead_code))]
const TRAY_START_WAIT: std::time::Duration = std::time::Duration::from_millis(2500);

/// #3855 — the tray's start stood in for the tasks of one test: its steps,
/// in place of the daemon's ([`TraySteps::real`]). A task-local, so the tests
/// beside it, and the daemon's own starts, are never stood in.
#[cfg(all(test, unix, not(target_os = "macos")))]
mod tray_stand_in {
    use super::TraySteps;

    #[derive(Clone)]
    pub(super) struct StandIn {
        pub(super) steps: TraySteps,
    }

    impl StandIn {
        pub(super) fn new(steps: TraySteps) -> Self {
            Self { steps }
        }
    }

    tokio::task_local! {
        static STOOD: StandIn;
    }

    /// `run`, its tray starts taking `stood`'s steps.
    pub(super) async fn asking<F: std::future::Future>(stood: StandIn, run: F) -> F::Output {
        STOOD.scope(stood, run).await
    }

    /// The start stood in for this task, if there is one.
    pub(super) fn now() -> Option<StandIn> {
        STOOD.try_with(StandIn::clone).ok()
    }
}

/// #3855 — the Linux tray's start for a prompt with its steps stood in
/// ([`tray_stand_in`]): a stopped logind, a session environment that does not
/// come (the account database or the person's user manager) and a systemd-run
/// that hangs each hold one step. Everything above the steps is the daemon's.
/// Each probe runs as a task on a two-worker runtime, the daemon's shape, and
/// is waited for from the test's own thread (#3836's shape).
#[cfg(all(test, unix, not(target_os = "macos")))]
mod tray_start_tests {
    use super::tray_stand_in::{StandIn, asking};
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::{Arc, Condvar, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    /// What a failure may take past the start's own wait on a busy runner.
    const SLACK: Duration = Duration::from_secs(1);
    /// What a start that answers may take on a busy runner.
    const RUNS_WITHIN: Duration = Duration::from_secs(10);
    /// How late "on time" may be on a busy runner: #3836's bar.
    const AT_ONCE: Duration = Duration::from_millis(500);
    /// Where the stood-in companion is installed.
    const COMPANION: &str = "/usr/bin/roomler-desktop";

    /// What a start answered: the outcome, or why it failed.
    type Answer = std::result::Result<EnsureOutcome, String>;

    /// The step a test holds up.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Step {
        /// logind, asked for the graphical session.
        Session,
        /// The session's environment: the account database, then the
        /// person's user manager.
        Environment,
        /// systemd-run itself.
        SystemdRun,
    }

    /// The held step: each start that reaches it is counted, and held until
    /// the gate opens. Then it goes on as if it had answered.
    #[derive(Clone, Default)]
    struct Held(Arc<HeldState>);

    #[derive(Default)]
    struct HeldState {
        open: Mutex<bool>,
        opened: Condvar,
        began: AtomicUsize,
    }

    impl Held {
        fn hold(&self) {
            self.0.began.fetch_add(1, SeqCst);
            let open = self.0.open.lock().unwrap_or_else(PoisonError::into_inner);
            drop(
                self.0
                    .opened
                    .wait_while(open, |open| !*open)
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }

        fn open(&self) {
            *self.0.open.lock().unwrap_or_else(PoisonError::into_inner) = true;
            self.0.opened.notify_all();
        }

        /// Starts that reached the held step.
        fn began(&self) -> usize {
            self.0.began.load(SeqCst)
        }
    }

    /// Opens the gate when dropped: a failed assertion leaves nothing held,
    /// and the runtime declared before it can shut down.
    struct OpenedAtTheEnd(Held);

    impl Drop for OpenedAtTheEnd {
        fn drop(&mut self) {
            self.0.open();
        }
    }

    /// What systemd-run was asked to run, each time.
    #[derive(Clone, Default)]
    struct Ran(Arc<Mutex<Vec<Vec<String>>>>);

    impl Ran {
        fn record(&self, cmd: &std::process::Command) {
            let mut argv = vec![cmd.get_program().to_string_lossy().into_owned()];
            argv.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(argv);
        }

        fn argvs(&self) -> Vec<Vec<String>> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    /// The person at the screen, as logind reports them.
    fn ana() -> GraphicalSession {
        GraphicalSession {
            uid: 4242,
            name: "ana".into(),
            display: Some(":0".into()),
            wayland_display: None,
        }
    }

    /// A child that exited `code`, having written `stderr`.
    fn exited(code: i32, stderr: &str) -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// A root daemon's start on a host with the companion installed, the
    /// person at the screen and their tray not running. Every step answers
    /// at once, except `hold`'s, which waits for `held`. systemd-run's argv
    /// goes to `ran`, and it exits 0.
    fn steps(hold: Option<Step>, held: &Held, ran: &Ran) -> TraySteps {
        let holds = move |step: Step, held: &Held| {
            if hold == Some(step) {
                held.hold();
            }
        };
        let (at_session, at_environment, at_run) = (held.clone(), held.clone(), held.clone());
        let ran = ran.clone();
        TraySteps {
            path: Arc::new(|| Some(std::path::PathBuf::from(COMPANION))),
            session: Arc::new(move || -> Result<GraphicalSession> {
                holds(Step::Session, &at_session);
                Ok(ana())
            }),
            running: Arc::new(|_: u32| false),
            environment: Arc::new(move |_: &GraphicalSession| -> Option<String> {
                holds(Step::Environment, &at_environment);
                Some("XDG_CURRENT_DESKTOP=GNOME\nLD_PRELOAD=/not/allowed.so\n".into())
            }),
            systemd_run: Arc::new(
                move |cmd: std::process::Command| -> std::io::Result<std::process::Output> {
                    holds(Step::SystemdRun, &at_run);
                    ran.record(&cmd);
                    Ok(exited(0, ""))
                },
            ),
            user_manager: false,
        }
    }

    /// A runtime like the daemon's: more than one worker, timers and I/O.
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime")
    }

    /// The tray's start as a prompt asks for it ([`ensure_running_inner`]),
    /// as a task on `rt` taking `stood`'s steps: what it answered and how
    /// long that took, waited for `bound` at most from THIS thread.
    fn start_on_a_worker(
        rt: &tokio::runtime::Runtime,
        stood: &StandIn,
        bound: Duration,
    ) -> std::result::Result<(Answer, Duration), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let asked = Instant::now();
        rt.spawn(asking(stood.clone(), async move {
            let started = ensure_running_inner().await.map_err(|e| format!("{e:#}"));
            let _ = tx.send((started, asked.elapsed()));
        }));
        rx.recv_timeout(bound)
            .map_err(|_| format!("no answer within {bound:?}"))
    }

    /// #3855 — with logind, the session's environment (the account
    /// database, the person's user manager) or systemd-run held up, a start
    /// for a prompt fails within its own wait ([`TRAY_START_WAIT`]), never
    /// `Started`, and its reason names the step that did not answer.
    ///
    /// Red before #3855: the start ran on the worker that asked, for as long
    /// as the step was held.
    #[test]
    fn a_held_step_fails_the_start_within_its_wait_naming_it() {
        let rt = runtime();
        for (step, names) in [
            (Step::Session, "logind"),
            (Step::Environment, "user manager"),
            (Step::SystemdRun, "systemd-run"),
        ] {
            let held = Held::default();
            let _opened = OpenedAtTheEnd(held.clone());
            let ran = Ran::default();
            let stood = StandIn::new(steps(Some(step), &held, &ran));
            let (started, waited) = start_on_a_worker(&rt, &stood, TRAY_START_WAIT + SLACK)
                .unwrap_or_else(|e| panic!("{step:?}: {e}"));
            let why = match started {
                Err(why) => why,
                Ok(outcome) => panic!("{step:?} held, and the start answered {outcome:?}"),
            };
            assert!(
                waited >= TRAY_START_WAIT,
                "{step:?}: the start failed in {waited:?}, before its wait"
            );
            assert!(
                why.contains("did not finish within"),
                "{step:?}: the reason says the start ran out of time: {why}"
            );
            assert!(
                why.contains(names),
                "{step:?}: the reason names the step: {why}"
            );
        }
    }

    /// #3855 — a held start holds no async worker. On a two-worker runtime,
    /// tasks asking again and again while logind holds the start leave a
    /// 10 ms timer waking on time (#3836's measure), and the hold holds one
    /// start, however many ask.
    ///
    /// Red before #3855: each ask held the worker it ran on, and two of them
    /// held both.
    #[test]
    fn a_held_start_delays_no_timer() {
        let rt = runtime();
        let held = Held::default();
        let _opened = OpenedAtTheEnd(held.clone());
        let ran = Ran::default();
        let stood = StandIn::new(steps(Some(Step::Session), &held, &ran));
        // More askers than the runtime has workers.
        for _ in 0..4 {
            rt.spawn(asking(stood.clone(), async {
                for _ in 0..1_000 {
                    let _ = ensure_running_inner().await;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }));
        }
        let asked = Instant::now();
        while held.began() == 0 {
            assert!(asked.elapsed() < RUNS_WITHIN, "no start reached logind");
            std::thread::sleep(Duration::from_millis(1));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        rt.spawn(async move {
            let tick = Duration::from_millis(10);
            let mut worst = Duration::ZERO;
            for _ in 0..100 {
                let asked = tokio::time::Instant::now();
                tokio::time::sleep(tick).await;
                worst = worst.max(asked.elapsed().saturating_sub(tick));
            }
            let _ = tx.send(worst);
        });
        let worst = rx
            .recv_timeout(RUNS_WITHIN)
            .expect("a 10 ms timer kept waking while logind held the start");
        assert!(
            worst < AT_ONCE,
            "a 10 ms timer woke {worst:?} late while logind held the start"
        );
        assert_eq!(held.began(), 1, "one start in flight, however many asked");
    }

    /// #3855 — while logind holds a start, asks at once and in a row share
    /// it: each fails within its own wait, and none starts another, so the
    /// hold holds one thread. Once logind answers, the late start answers
    /// nobody, and the next ask starts afresh and is answered.
    ///
    /// Red before #3855: every ask was a start of its own, on its own worker.
    #[test]
    fn asks_while_a_start_hangs_share_it() {
        let rt = runtime();
        let held = Held::default();
        let _opened = OpenedAtTheEnd(held.clone());
        let ran = Ran::default();
        let stood = StandIn::new(steps(Some(Step::Session), &held, &ran));
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..4 {
            let tx = tx.clone();
            rt.spawn(asking(stood.clone(), async move {
                let _ = tx.send(ensure_running_inner().await.map_err(|e| format!("{e:#}")));
            }));
        }
        for _ in 0..4 {
            let started = rx
                .recv_timeout(TRAY_START_WAIT + SLACK)
                .expect("every ask at once is answered while logind holds the start");
            assert!(started.is_err(), "nothing starts while held: {started:?}");
        }
        for _ in 0..2 {
            let (started, _) = start_on_a_worker(&rt, &stood, TRAY_START_WAIT + SLACK)
                .expect("an ask in a row is answered while logind holds the start");
            assert!(started.is_err(), "nothing starts while held: {started:?}");
        }
        assert_eq!(held.began(), 1, "one start, however many asked");
        assert!(ran.argvs().is_empty(), "systemd-run never ran while held");
        held.open();
        let (started, _) =
            start_on_a_worker(&rt, &stood, RUNS_WITHIN).expect("the next ask is answered");
        assert_eq!(started, Ok(EnsureOutcome::Started), "a start of its own");
        assert_eq!(held.began(), 2, "the held start, and the next ask's own");
    }

    /// The control: a start whose steps answer in time does what it always
    /// did. A root daemon starts the tray through the system manager as the
    /// person, with the session's allowlisted environment; a per-user daemon
    /// through its own manager, without reading it. A tray already running,
    /// no companion installed, nobody at the screen and a failed systemd-run
    /// answer as before, and only a systemd-run that exited 0 is `Started`.
    #[test]
    fn a_start_in_time_works_as_today() {
        let rt = runtime();
        let held = Held::default();
        let ask = |taken: TraySteps| {
            start_on_a_worker(&rt, &StandIn::new(taken), RUNS_WITHIN)
                .expect("a start in time is answered")
                .0
        };
        let argv = |args: &[&str]| -> Vec<String> { args.iter().map(|a| a.to_string()).collect() };

        let ran = Ran::default();
        assert_eq!(ask(steps(None, &held, &ran)), Ok(EnsureOutcome::Started));
        assert_eq!(
            ran.argvs(),
            vec![argv(&[
                "systemd-run",
                "--quiet",
                "--collect",
                "--property=KillMode=process",
                "--uid=4242",
                "--setenv=XDG_RUNTIME_DIR=/run/user/4242",
                "--setenv=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/4242/bus",
                "--setenv=DISPLAY=:0",
                "--setenv=XDG_CURRENT_DESKTOP=GNOME",
                COMPANION,
            ])],
            "a root daemon: the system manager, as the person, with the allowlisted environment"
        );

        let ran = Ran::default();
        let read = Arc::new(AtomicUsize::new(0));
        let mut per_user = steps(None, &held, &ran);
        per_user.user_manager = true;
        per_user.environment = {
            let read = read.clone();
            Arc::new(move |_: &GraphicalSession| -> Option<String> {
                read.fetch_add(1, SeqCst);
                None
            })
        };
        assert_eq!(ask(per_user), Ok(EnsureOutcome::Started));
        assert_eq!(
            ran.argvs(),
            vec![argv(&[
                "systemd-run",
                "--user",
                "--quiet",
                "--collect",
                "--property=KillMode=process",
                "--setenv=XDG_RUNTIME_DIR=/run/user/4242",
                "--setenv=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/4242/bus",
                "--setenv=DISPLAY=:0",
                COMPANION,
            ])],
            "a per-user daemon: its own manager"
        );
        assert_eq!(
            read.load(SeqCst),
            0,
            "a per-user daemon's unit inherits its manager's environment"
        );

        let ran = Ran::default();
        let mut running = steps(None, &held, &ran);
        running.running = Arc::new(|uid: u32| uid == 4242);
        assert_eq!(ask(running), Ok(EnsureOutcome::AlreadyRunning));
        assert!(
            ran.argvs().is_empty(),
            "a running tray is not started again"
        );

        let ran = Ran::default();
        let mut not_installed = steps(None, &held, &ran);
        not_installed.path = Arc::new(|| -> Option<std::path::PathBuf> { None });
        assert_eq!(ask(not_installed), Ok(EnsureOutcome::Unsupported));
        assert!(ran.argvs().is_empty());

        let ran = Ran::default();
        let mut nobody = steps(None, &held, &ran);
        nobody.session = Arc::new(|| -> Result<GraphicalSession> {
            anyhow::bail!("no active graphical session — nobody is at this machine's screen")
        });
        let why = ask(nobody).expect_err("nobody at the screen is no start");
        assert_eq!(
            why,
            "finding a graphical login session: no active graphical session — nobody is at this \
             machine's screen"
        );
        assert!(ran.argvs().is_empty());

        let ran = Ran::default();
        let mut refused = steps(None, &held, &ran);
        refused.systemd_run = {
            let ran = ran.clone();
            Arc::new(
                move |cmd: std::process::Command| -> std::io::Result<std::process::Output> {
                    ran.record(&cmd);
                    Ok(exited(
                        1,
                        "Failed to start transient service unit: Access denied\n",
                    ))
                },
            )
        };
        assert_eq!(
            ask(refused),
            Err("systemd-run failed: Failed to start transient service unit: Access denied".into()),
            "only a systemd-run that exited 0 is a start"
        );
        assert_eq!(ran.argvs().len(), 1);
    }
}

/// Where the Linux companion lives: `/usr/bin` from its .deb, `/usr/local/bin`
/// for a hand install. One list, so every caller agrees on which file is
/// "the" companion.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn linux_companion_path() -> Option<std::path::PathBuf> {
    ["/usr/bin", "/usr/local/bin"]
        .iter()
        .map(|d| std::path::Path::new(d).join("roomler-desktop"))
        .find(|p| p.exists())
}

/// `systemd-run` for the companion in `sess`, as its own transient unit — so
/// it is NOT a child in the daemon's cgroup, which systemd kills with the
/// daemon on every restart or update.
///
/// * `user_manager = false` — a ROOT daemon: a system transient unit running
///   as the session's uid (`--uid=`), the shape FR-27's consent path has used
///   since it shipped.
/// * `user_manager = true` — a per-user daemon: the user's own manager
///   (`--user`); the system manager would ask polkit for a password nobody is
///   there to type.
///
/// Either way the session's bus and display go along as `--setenv`: a
/// transient unit starts with an empty environment.
///
/// ⚠️ #2713 — and a ROOT daemon's unit also gets the desktop session's own
/// environment (an allowlist, [`SESSION_ENV_KEYS`]), read from the user's
/// systemd manager. A system unit inherits nothing of the desktop, so a
/// companion started this way had no `XDG_CURRENT_DESKTOP` (the desktop's
/// default apps live in `<desktop>-mimeapps.list`), no per-user Flatpak dir
/// in `XDG_DATA_DIRS`, the system locale and no `XAUTHORITY`; measured
/// 2026-10-22 on a Fedora Asahi GNOME box, where that companion is what the
/// person clicked View screen in. A per-user daemon's `--user` unit inherits
/// the manager's environment already.
///
/// ⚠️ #2729 — and either unit is `KillMode=process`. The companion is the
/// unit's main process, and what it launches lands in the unit's cgroup: the
/// person's web browser, when View screen or Open Roomler started it. Under
/// the default `KillMode=control-group`, a `Type=simple` unit whose main
/// process exits is stopped and everything left in it killed, so quitting the
/// companion, a crash, or FR-117's restart after every update (a SIGTERM to
/// the old companion) closed that browser with every window it had. Measured
/// 2026-10-22 on the same box: Chromium in the companion's
/// `system.slice/run-…service`, and in a replica of this unit a child killed
/// when the main process exited, alive under `KillMode=process`. The
/// companion's own helpers need no killing: WebKit's web and network
/// processes exit when their UI process's connection closes.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn systemd_run_command(
    sess: &GraphicalSession,
    exe: &std::path::Path,
    args: &[&str],
    user_manager: bool,
) -> std::process::Command {
    let session_env = if user_manager {
        Vec::new()
    } else {
        user_manager_environment(sess)
            .map(|text| session_env_pairs(&text))
            .unwrap_or_default()
    };
    systemd_run_command_with(sess, exe, args, user_manager, &session_env)
}

/// #2713 — the keys of the desktop session's environment that a companion
/// the daemon starts must carry, to behave like one the desktop started:
/// which desktop it is, where apps and their defaults are found, X11 auth,
/// input methods, theme, a chosen browser and the locale (plus every `LC_*`,
/// see [`session_env_pairs`]).
///
/// ⚠️ An ALLOWLIST, not the whole environment. The values are the user's own
/// (no privilege crosses: the companion runs as that user), but the bus,
/// display and runtime dir keep coming from logind as before, and loader
/// plumbing (`LD_*`, `GTK_MODULES`) has no business in a process the daemon
/// starts. Widen it only with a measured need.
#[cfg(all(unix, not(target_os = "macos")))]
const SESSION_ENV_KEYS: &[&str] = &[
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
    "XDG_SESSION_TYPE",
    "DESKTOP_SESSION",
    "XDG_MENU_PREFIX",
    "XDG_DATA_DIRS",
    "XDG_CONFIG_DIRS",
    "XAUTHORITY",
    "KDE_FULL_SESSION",
    "KDE_SESSION_VERSION",
    "GTK_THEME",
    "GTK_IM_MODULE",
    "QT_IM_MODULE",
    "XMODIFIERS",
    "QT_QPA_PLATFORMTHEME",
    "BROWSER",
    "LANG",
    "LANGUAGE",
];

/// #2713 — the allowlisted `KEY=VALUE` pairs out of `systemctl --user
/// show-environment`, plus every `LC_*`. A value systemd had to shell-escape
/// (`KEY=$'…'`), an empty one, or one with a control character is skipped,
/// never decoded: nothing on the allowlist legitimately needs one.
#[cfg(all(unix, not(target_os = "macos")))]
fn session_env_pairs(show_environment: &str) -> Vec<(String, String)> {
    show_environment
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| {
            SESSION_ENV_KEYS.contains(key)
                || key.strip_prefix("LC_").is_some_and(|rest| {
                    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
                })
        })
        .filter(|(_, value)| {
            !value.is_empty()
                && value.len() <= 4096
                && !value.starts_with("$'")
                && !value.chars().any(char::is_control)
        })
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// #2713 — the session user's systemd manager environment as text, read by
/// running `systemctl --user show-environment` AS that user (a root daemon
/// drops through [`crate::exec::drop_to_std`], the one privilege story),
/// bounded at 5 s. `None` when it cannot be read: the companion then starts
/// as it always did, with logind's four variables.
#[cfg(all(unix, not(target_os = "macos")))]
fn user_manager_environment(sess: &GraphicalSession) -> Option<String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = sess;
        None
    }
    #[cfg(target_os = "linux")]
    {
        use std::process::Stdio;
        let mut cmd = std::process::Command::new("systemctl");
        cmd.args(["--user", "show-environment"])
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("XDG_RUNTIME_DIR", format!("/run/user/{}", sess.uid))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path=/run/user/{}/bus", sess.uid),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: geteuid reads our own credentials; it cannot fail.
        if unsafe { libc::geteuid() } == 0
            && let Err(e) = crate::exec::drop_to_std(&mut cmd, &sess.name)
        {
            tracing::debug!(%e, "companion: cannot read the session's environment");
            return None;
        }
        let text = stdout_bounded(cmd, std::time::Duration::from_secs(5))?;
        Some(text)
    }
}

/// Run `cmd` (stdout piped) and return its stdout when it exits 0 within
/// `limit`; a child still running at the deadline is killed. A wedged user
/// manager must not hold the companion's launch hostage.
#[cfg(target_os = "linux")]
fn stdout_bounded(mut cmd: std::process::Command, limit: std::time::Duration) -> Option<String> {
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().unwrap_or_default();
                return status
                    .success()
                    .then(|| String::from_utf8_lossy(&out).into_owned());
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::debug!("companion: the session's environment did not arrive in time");
                return None;
            }
        }
    }
}

/// [`systemd_run_command`] over an already-read session environment: the
/// pure half, so the argv is tested without a user manager to ask.
#[cfg(all(unix, not(target_os = "macos")))]
fn systemd_run_command_with(
    sess: &GraphicalSession,
    exe: &std::path::Path,
    args: &[&str],
    user_manager: bool,
    session_env: &[(String, String)],
) -> std::process::Command {
    let mut cmd = std::process::Command::new("systemd-run");
    if user_manager {
        cmd.arg("--user");
    }
    // #2729 — a browser the companion opened outlives the companion.
    cmd.args(["--quiet", "--collect", "--property=KillMode=process"]);
    if !user_manager {
        cmd.arg(format!("--uid={}", sess.uid));
    }
    cmd.args([
        format!("--setenv=XDG_RUNTIME_DIR=/run/user/{}", sess.uid),
        format!(
            "--setenv=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{}/bus",
            sess.uid
        ),
    ]);
    if let Some(display) = &sess.display {
        cmd.arg(format!("--setenv=DISPLAY={display}"));
    }
    if let Some(wayland) = &sess.wayland_display {
        cmd.arg(format!("--setenv=WAYLAND_DISPLAY={wayland}"));
    }
    for (key, value) in session_env {
        cmd.arg(format!("--setenv={key}={value}"));
    }
    cmd.arg(exe.as_os_str());
    cmd.args(args);
    cmd
}

/// Is a `roomler-desktop` of THIS user running? `pgrep -u` so another user's
/// companion on a shared machine does not count as this person's.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn companion_running_for_uid(uid: u32) -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", "-u", &uid.to_string(), "roomler-desktop"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// ⚠️ `pub(crate)` because FR-45's portal helper needs the same answer: the
/// portal is per-user-session and the daemon is root, so *both* the consent
/// companion and the capture helper have to find whoever is at the screen.
/// Two copies of this `loginctl` walk would be two things to keep in step
/// with a compositor that reports its session differently.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) struct GraphicalSession {
    pub(crate) uid: u32,
    /// The account NAME. `systemd-run --uid=` takes the number, but the
    /// verified privilege drop resolves by name (`getpwnam` gives it the home
    /// directory and supplementary groups a uid alone cannot), so both are
    /// carried rather than re-derived at each call site.
    ///
    /// ⚠️ The allow is scoped to lanes with no reader rather than blanket, so
    /// if its readers (the portal helper, and since #2713 the read of the
    /// session's environment on every Linux build) ever stop using this, the
    /// warning comes back instead of staying suppressed forever. Kept
    /// unconditional (rather than `cfg`-gated) because gating one field
    /// fragments the struct, its single constructor and the guard that fills
    /// it, for a string that costs nothing to carry.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) name: String,
    pub(crate) display: Option<String>,
    pub(crate) wayland_display: Option<String>,
}

/// Session ids out of `loginctl list-sessions --no-legend`.
///
/// ⚠️ **Only the first column is parsed, and that is the whole point.** The
/// rest of the table is not stable across systemd releases — measured on the
/// fleet, systemd 255 lays it out `SESSION UID USER SEAT TTY STATE IDLE SINCE`
/// and systemd 257 lays it out `SESSION UID USER SEAT LEADER CLASS TTY IDLE
/// SINCE`. Anything read positionally from those would be a different field
/// per host. The id is column one on both; every real property comes from
/// `show-session`, which is key=value and version-stable.
///
/// ⚠️ Ids are NOT numeric — logind hands out `c1`, `c2` for greeter sessions —
/// so they stay strings.
#[cfg(all(unix, not(target_os = "macos")))]
fn session_ids(list_output: &str) -> Vec<&str> {
    list_output
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect()
}

/// The active graphical login session, via `loginctl`. `None` on a headless
/// box — again, the correct answer, not a failure.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn graphical_session() -> Result<GraphicalSession> {
    graphical_session_matching(None, false)
}

/// [`graphical_session`], optionally restricted to one user's sessions — a
/// per-user daemon may only open something in its OWN user's session — and
/// optionally to `Class=user`: a display manager's greeter (GDM runs one as
/// uid `gdm` at the login screen) IS an active graphical session, which the
/// consent path wants and the post-install launch must never open into.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn graphical_session_matching(
    want_uid: Option<u32>,
    users_only: bool,
) -> Result<GraphicalSession> {
    graphical_session_via(want_uid, users_only, &loginctl_stdout)
}

/// `loginctl <args> --no-pager`'s stdout. The exit status is not read: a
/// `show-session` of a session that ended mid-walk prints nothing, and the walk
/// goes on to the next one.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn loginctl_stdout(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("loginctl")
        .args(args)
        .arg("--no-pager")
        .output()
        .with_context(|| format!("spawning loginctl {}", args.join(" ")))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// [`graphical_session_matching`] with `loginctl` injected: which session wins
/// is a decision, and one that needs a real greeter to exercise is one nobody
/// re-checks.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn graphical_session_via(
    want_uid: Option<u32>,
    users_only: bool,
    loginctl: &dyn Fn(&[&str]) -> Result<String>,
) -> Result<GraphicalSession> {
    // ⚠️ NOT `-o value -p Id`. Those are `show-*` options; `list-sessions`
    // rejects them with `Unknown output 'value'` and exits 1, which this
    // function then read as an empty session list — i.e. "nobody is at the
    // screen" on every Linux host, always. Measured on systemd 255 (Ubuntu)
    // and 257 (Fedora): rejected on both, so it never worked anywhere rather
    // than regressing. The visible symptom was the FR-27 consent companion
    // silently never starting, which only shows up where the companion is the
    // chosen surface — GNOME/KDE Wayland, exactly the hosts FR-45 targets.
    let list = loginctl(&["list-sessions", "--no-legend"])?;
    let mut login_screen = false;
    for id in session_ids(&list) {
        let text = loginctl(&["show-session", id])?;
        let field = |k: &str| show_field(&text, k);
        let ty = field("Type").unwrap_or_default();
        if ty != "x11" && ty != "wayland" {
            continue;
        }
        if field("Active").as_deref() != Some("yes") {
            continue;
        }
        if users_only && field("Class").as_deref() != Some("user") {
            login_screen |= field("Class").as_deref() == Some("greeter");
            continue;
        }
        let Some(uid) = field("User").and_then(|u| u.parse::<u32>().ok()) else {
            continue;
        };
        if want_uid.is_some_and(|w| w != uid) {
            continue;
        }
        // A session with no `Name` is not usable by the privilege drop, so
        // skip it rather than return one that will fail later — the next
        // session in the list may well be serviceable.
        let Some(name) = field("Name") else {
            continue;
        };
        return Ok(GraphicalSession {
            uid,
            name,
            display: field("Display"),
            wayland_display: (ty == "wayland").then(|| {
                // loginctl does not report WAYLAND_DISPLAY; the near-universal
                // default is what the compositor sets, and a wrong guess just
                // makes the app fall back to its own discovery.
                "wayland-0".to_string()
            }),
        });
    }
    if login_screen {
        anyhow::bail!("the login screen is showing — nobody is signed in at this machine's screen")
    }
    anyhow::bail!("no active graphical session — nobody is at this machine's screen")
}

/// One property out of `loginctl show-*` output, which is `key=value` per
/// line and stable across systemd releases (see [`session_ids`]). `None`
/// when absent or empty.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn show_field(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// FR-97 P5a — the account of the person signed in at this machine's own
/// screen and keyboard: the user of logind's `ActiveSession` on `seat0`, when
/// that session is a person's (`Class=user`), graphical or a text console.
///
/// Not [`graphical_session`], which takes the first active graphical session
/// ANYWHERE: a remote desktop's session (xrdp) sits on no seat, and logind
/// calls every seatless session active, so it would name someone who is not
/// at the screen at all. `seat0` is the machine's own screen by definition,
/// and its active session is the one showing, as Windows' console session
/// is. `Err` says why nobody is there: no logind, an empty seat, the login
/// screen.
///
/// #3688 — with the uid logind recorded for that session: a root daemon's
/// file steps tell root itself at the screen, whose files are the daemon's
/// own, from a person and from nobody. #3822 — exec's and SSH's
/// `console_user` ask it through the same lookup a received file does
/// (`files::ask_seat0`), so a session and a file take one person by one rule.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn seat0_account() -> Result<(String, Option<u32>)> {
    let id = seat0_active_session(&loginctl_show(&["show-seat", "seat0"])?)?;
    session_account(&id, &loginctl_show(&["show-session", &id])?)
}

/// #2871 (review) — the uid of the person signed in at seat0, by
/// [`seat0_account`]'s rule: whom a root daemon's person-at-the-device socket is
/// made for, and who must still be there when a config change arrives on it.
/// Never [`graphical_session_matching`]'s first active graphical session
/// anywhere, which is whoever's remote login logind happens to list first.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn seat0_uid() -> Result<u32> {
    seat0_person_uid(&loginctl_show(&["show-seat", "seat0"])?, |id| {
        loginctl_show(&["show-session", id])
    })
}

/// `loginctl <args> --no-pager`'s output, or why there is none.
#[cfg(all(unix, not(target_os = "macos")))]
fn loginctl_show(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("loginctl")
        .args(args)
        .arg("--no-pager")
        .output()
        .map_err(loginctl_did_not_start)?;
    if !out.status.success() {
        return Err(loginctl_failed(args, &String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The pure half of [`loginctl_show`] when `loginctl` did not start. #3688 —
/// not installed at all, it is a host with no logind, so no seat: nobody is at
/// its screen ([`NobodyAtTheScreen`]). Any other failure to start it cannot
/// tell who is there.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
pub(crate) fn loginctl_did_not_start(e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        return nobody_at_the_screen(format!(
            "nobody is signed in at this machine's screen (it has no seat: loginctl is not \
             installed, {e})"
        ));
    }
    anyhow::Error::new(e).context("spawning loginctl (is systemd-logind running?)")
}

/// The pure half of [`loginctl_show`] when `loginctl <args>` failed, from
/// what it wrote to stderr. #3688 — a host systemd did not boot (a container,
/// WSL without systemd) has no logind, so no seat: nobody is at its screen
/// ([`NobodyAtTheScreen`]). Any other failure (the bus or logind not
/// answering, a timeout) cannot tell who is there.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
pub(crate) fn loginctl_failed(args: &[&str], stderr: &str) -> anyhow::Error {
    let failed = format!("loginctl {} failed: {}", args.join(" "), stderr.trim());
    if stderr.contains("System has not been booted with systemd") {
        return nobody_at_the_screen(format!(
            "nobody is signed in at this machine's screen (it has no seat: {failed})"
        ));
    }
    anyhow::anyhow!(failed)
}

/// The pure half of [`seat0_uid`]: seat0's active session out of `show-seat
/// seat0`, then that one session's person out of `show_session(id)`. No other
/// session is read, so a seatless one — a remote desktop's, which logind
/// calls active — is never "at the seat", whatever order logind lists it in.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn seat0_person_uid(seat: &str, show_session: impl FnOnce(&str) -> Result<String>) -> Result<u32> {
    let id = seat0_active_session(seat)?;
    session_person_uid(&id, &show_session(&id)?)
}

/// The pure half of [`seat0_uid`] for the session seat0 shows:
/// [`session_person`]'s rule (a person's session that names an account), on
/// seat0 and not a remote login, and the uid logind recorded for it. logind
/// never makes a remote or seatless session seat0's active one, so the last
/// two refuse only a state it does not produce, by name rather than by serving
/// it.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn session_person_uid(id: &str, session: &str) -> Result<u32> {
    session_person(id, session)?;
    if show_field(session, "Remote").as_deref() == Some("yes") {
        anyhow::bail!("session {id} is a remote login, not the person at this machine's screen");
    }
    if show_field(session, "Seat").as_deref() != Some("seat0") {
        anyhow::bail!("session {id} is not on seat0, so not this machine's own screen");
    }
    show_field(session, "User")
        .and_then(|u| u.parse::<u32>().ok())
        .ok_or_else(|| anyhow::anyhow!("seat0's session {id} names no uid"))
}

/// #3688 — seat0's own answer that nobody is at this machine's screen: an
/// empty seat, the login screen, a session that is not a person's; or a host
/// with no logind at all, so no seat ([`loginctl_did_not_start`],
/// [`loginctl_failed`]). Any other lookup that failed (logind did not answer,
/// a session that names no account) is not one: who is there is then not
/// known, and a viewer's read says so, rather than that nobody is.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
#[derive(Debug)]
struct NobodyAtTheScreen(String);

#[cfg(any(all(unix, not(target_os = "macos")), test))]
impl std::fmt::Display for NobodyAtTheScreen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(any(all(unix, not(target_os = "macos")), test))]
impl std::error::Error for NobodyAtTheScreen {}

/// `why`, as seat0's own answer that nobody is at the screen.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
pub(crate) fn nobody_at_the_screen(why: String) -> anyhow::Error {
    anyhow::Error::new(NobodyAtTheScreen(why))
}

/// #3688 — whether `e` is seat0's own answer that nobody is at the screen
/// ([`NobodyAtTheScreen`]), anywhere in its chain.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
pub(crate) fn is_nobody_at_the_screen(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| cause.is::<NobodyAtTheScreen>())
}

/// The pure half of [`seat0_account`]: the active session out of `loginctl
/// show-seat seat0`.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn seat0_active_session(seat: &str) -> Result<String> {
    show_field(seat, "ActiveSession").ok_or_else(|| {
        nobody_at_the_screen(
            "nobody is signed in at this machine's screen (seat0 has no active session)".into(),
        )
    })
}

/// The pure half of [`seat0_account`]: the person whose session `id` is, out of
/// `loginctl show-session <id>`, or why it is nobody's. A display manager's
/// greeter (GDM runs one as `gdm` at the login screen) is `Class=greeter`.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn session_person(id: &str, session: &str) -> Result<String> {
    match show_field(session, "Class").as_deref() {
        Some("user") => {}
        Some("greeter") => {
            return Err(nobody_at_the_screen(format!(
                "nobody is signed in at this machine's screen (the login screen is showing, \
                 session {id})"
            )));
        }
        other => {
            return Err(nobody_at_the_screen(format!(
                "nobody is signed in at this machine's screen (seat0's session {id} is of class \
                 {}, not a person's)",
                other.unwrap_or("unknown")
            )));
        }
    }
    show_field(session, "Name")
        .ok_or_else(|| anyhow::anyhow!("seat0's session {id} names no account"))
}

/// The pure half of [`seat0_account`]: [`session_person`]'s person, with the
/// uid logind recorded for the session when it names one.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn session_account(id: &str, session: &str) -> Result<(String, Option<u32>)> {
    let name = session_person(id, session)?;
    Ok((
        name,
        show_field(session, "User").and_then(|u| u.parse().ok()),
    ))
}

/// Is a process with this exact name running? An exact-name match (`pgrep -x`)
/// cannot be fooled by a command line that merely mentions the binary. macOS
/// only: Linux asks for the session user's own (`companion_running_for_uid`,
/// FR-93 P6b).
#[cfg(target_os = "macos")]
fn pgrep_running(name: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", name])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

#[cfg(not(any(target_os = "windows", unix)))]
async fn ensure_running_inner() -> Result<EnsureOutcome> {
    Ok(EnsureOutcome::Unsupported)
}

/// Entry point — spawn-and-forget from daemon startup. Never fails the
/// caller; every error path logs and returns (retry on next start).
///
/// #3824 — then, in every pass that stopped no tray (nothing to swap, a swap
/// with none running, one that gave up before any stop, a failed download),
/// a tray an earlier pass stopped and could not start again is started
/// ([`restore_lost_tray`]).
pub async fn refresh_if_stale(respawn: RespawnContext) {
    #[cfg(target_os = "windows")]
    {
        let pass = refresh_inner(respawn).await;
        let stopped_a_tray = matches!(&pass, Ok(Some(swapped)) if swapped.stopped_a_tray());
        match pass {
            Ok(Some(swapped)) => log_refresh(&swapped),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "desktop companion refresh skipped")
            }
        }
        bring_back_lost_tray(respawn, stopped_a_tray).await;
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Windows-only BY DESIGN, not by omission — and FR-27 did not change
        // that even though the companion now ships on all three platforms.
        //
        // This function exists because the Windows companion is a standalone
        // EXE placed BESIDE the daemon by the wizard / install.ps1, in neither
        // MSI, so nothing else would ever move it forward. The macOS .pkg
        // carries `/Applications/Roomler.app`. On Linux the companion is its
        // own `.deb`/`.rpm`, which ⚠️ NOTHING upgraded (there is no apt or dnf
        // repository): FR-117 moves it through the host's package manager, in
        // the updater's loop (`sync_package`), never by swapping a file the
        // package manager owns.
        let _ = respawn;
    }
}

/// One refresh: `Ok(None)` when there is nothing to swap, the swap's outcome
/// when it ran, an error when it could not start (the release, the download).
#[cfg(target_os = "windows")]
async fn refresh_inner(respawn: RespawnContext) -> Result<Option<SwapOutcome>> {
    let own_version = env!("CARGO_PKG_VERSION");
    let exe_dir = std::env::current_exe()
        .context("locating own exe")?
        .parent()
        .context("own exe has no parent dir")?
        .to_path_buf();
    let dest = exe_dir.join(DESKTOP_EXE);
    let marker = exe_dir.join(VERSION_MARKER);

    // Fresh marker + EXE present → nothing to do (the common case on
    // every startup after the first successful swap).
    let recorded = std::fs::read_to_string(&marker)
        .ok()
        .map(|s| s.trim().to_string());
    if dest.exists() && recorded.as_deref() == Some(own_version) {
        return Ok(None);
    }
    tracing::info!(
        own = own_version,
        recorded = recorded.as_deref().unwrap_or("<none>"),
        present = dest.exists(),
        "desktop companion is stale/missing — refreshing"
    );

    // Resolve the desktop asset from OUR OWN release tag. `agent-v<ver>`
    // is the tag scheme release-agent.yml pushes; a dev build whose
    // version was never released just logs a clean skip here.
    let tag = format!("agent-v{own_version}");
    let release = crate::updater::fetch_release_by_tag(&tag)
        .await
        .with_context(|| format!("fetching release {tag}"))?;
    let asset = pick_desktop_asset(&release.assets)
        .with_context(|| format!("no roomler-desktop asset in release {tag}"))?;
    // The tag is OUR OWN version, so there is no version claim to be lied to
    // about here — but pass it anyway: the day the desktop EXE gains a
    // readable version binding, this call site should not need finding.
    let staged = crate::updater::download_asset(asset, &tag)
        .await
        .with_context(|| format!("downloading {}", asset.name))?;

    // #1686, #3824 — the swap, in the order `swap` pins (see there), on the
    // blocking pool: it stops processes and waits for them.
    let outcome = tokio::task::spawn_blocking(move || {
        let mut ops = FsSwap {
            new: exe_dir.join(NEW_SUFFIX),
            old: exe_dir.join(OLD_SUFFIX),
            aside: None,
            staged,
            dest,
            marker,
            version: own_version,
            respawn,
        };
        swap(&mut ops, &TRAY_LOST)
    })
    .await
    .context("the companion swap task")?;
    Ok(Some(outcome))
}

/// #3824 — the refresh's line for a swap, as [`refresh_line`] renders it.
/// The prefixes are the ones the lab kits grep.
#[cfg(target_os = "windows")]
fn log_refresh(outcome: &SwapOutcome) {
    let version = env!("CARGO_PKG_VERSION");
    let stopped = outcome.stopped();
    match refresh_line(outcome) {
        RefreshLine::Refreshed { tray } => {
            tracing::info!(version, ?stopped, tray = %tray, "desktop companion refreshed")
        }
        RefreshLine::TrayNotBack { error } => tracing::warn!(
            version,
            ?stopped,
            error = %error,
            "desktop companion refreshed, but the tray could not be started again"
        ),
        RefreshLine::Skipped { error } => {
            tracing::warn!(error = %error, "desktop companion refresh skipped")
        }
    }
}

/// #3824 — after a pass that stopped no tray, start one an earlier pass lost
/// ([`restore_lost_tray`]). One read of a flag when none was lost.
#[cfg(target_os = "windows")]
async fn bring_back_lost_tray(respawn: RespawnContext, pass_stopped_a_tray: bool) {
    if !TRAY_LOST.is_set() {
        return;
    }
    let Some(dest) = sibling_desktop() else {
        return;
    };
    // A registry read, a process list and a start: the blocking pool.
    let line = tokio::task::spawn_blocking(move || {
        restore_lost_tray(pass_stopped_a_tray, &TRAY_LOST, || {
            start_lost_tray(respawn, &dest)
        })
    })
    .await;
    const STILL_DOWN: &str =
        "desktop companion is still not running after a refresh stopped it; it is tried again";
    match line {
        Ok(Some(LostTrayLine::Back { how })) => tracing::info!(
            how = %how,
            "desktop companion is back after a refresh stopped it"
        ),
        Ok(Some(LostTrayLine::StillDown { why, first: true })) => {
            tracing::warn!(why = %why, "{STILL_DOWN}")
        }
        Ok(Some(LostTrayLine::StillDown { why, first: false })) => {
            tracing::debug!(why = %why, "{STILL_DOWN}")
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, "desktop companion: the lost tray's start did not run")
        }
    }
}

/// #1686, #3824 — what the tray swap does to the disk and to running
/// processes, behind a seam so `swap`'s ORDER is tested on every platform.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) trait SwapOps {
    /// Copy the new EXE in BESIDE the installed one (`…exe.new`); the
    /// installed one and every running tray are untouched.
    fn stage(&mut self) -> Result<()>;
    /// Installed EXE → `…exe.old`, or the first free `…exe.old.<n>` when a
    /// copy an earlier swap left there cannot be removed.
    fn rename_old_aside(&mut self) -> Result<()>;
    /// `…exe.new` → installed EXE (same directory: an atomic rename).
    fn promote_new(&mut self) -> Result<()>;
    /// The EXE moved aside → installed EXE (rollback), over the new one when
    /// that is in place.
    fn restore_old(&mut self) -> Result<()>;
    /// Remove `…exe.new`.
    fn discard_new(&mut self);
    /// Running trays (pids) started from the installed path, whatever their
    /// file has been renamed to since.
    fn running(&mut self) -> Vec<u32>;
    /// Stop those pids — each re-verified on the handle that kills it — and
    /// return the ones confirmed exited.
    fn stop(&mut self, pids: &[u32]) -> Vec<u32>;
    fn write_marker(&mut self);
    /// Start the tray at the installed path (whichever EXE is there), as the
    /// refresh always has: no arguments.
    fn start(&mut self) -> TrayStart;
    /// Remove the copies moved aside, best-effort: this swap's, and any an
    /// earlier swap left.
    fn remove_old(&mut self);
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwapOutcome {
    /// The new EXE is installed, and the marker names it.
    Swapped {
        /// The trays this swap stopped: every one that ran.
        stopped: Vec<u32>,
        /// Their start; `None` when none was stopped.
        start: Option<TrayStart>,
    },
    /// The marker is as it was, and so is the installed EXE unless `reason`
    /// says putting it back failed.
    Aborted {
        reason: String,
        /// The trays it stopped before it gave up.
        stopped: Vec<u32>,
        /// Their start; `None` when none was stopped.
        start: Option<TrayStart>,
    },
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl SwapOutcome {
    /// The trays the swap stopped.
    pub(crate) fn stopped(&self) -> &[u32] {
        match self {
            Self::Swapped { stopped, .. } | Self::Aborted { stopped, .. } => stopped,
        }
    }

    /// Did the swap stop a tray? Its own start then decided whether the tray
    /// is lost ([`TrayLost`]).
    pub(crate) fn stopped_a_tray(&self) -> bool {
        !self.stopped().is_empty()
    }
}

/// #1686, #3824 — the tray swap, in the order that never stops a tray before
/// the new EXE is in place:
///
/// 1. stage the new copy beside the installed one;
/// 2. rename the installed EXE aside while a tray runs from it: NTFS renames a
///    running image (`win_service::companion_procs`' test measures it). What
///    refuses is an ACL, a handle without `FILE_SHARE_DELETE`, or endpoint
///    protection;
/// 3. rename the new copy into place. A failure in 1-3 puts back what moved
///    and stops nothing: the tray runs on, and the retry tries again. Until
///    #3824 the tray was stopped first, and a refused rename, with its start
///    from the old EXE refused as well, left the person with none;
/// 4. only now stop the running trays, found by the path they were started
///    from, which no rename changes (#1686);
/// 5. one that would not stop keeps the single-instance lock: put the old EXE
///    back over the new one, which nothing runs, start what was stopped, and
///    leave the marker, so the retry tries again;
/// 6. else write the marker, start what was stopped, and remove the old copy.
///
/// A stop whose start does not bring the tray back marks it lost, and a start
/// that works forgets that ([`TrayLost`]).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // exercised by `swap_tests` everywhere
pub(crate) fn swap<O: SwapOps>(ops: &mut O, lost: &TrayLost) -> SwapOutcome {
    let gave_up = |reason: String| SwapOutcome::Aborted {
        reason,
        stopped: Vec::new(),
        start: None,
    };
    if let Err(e) = ops.stage() {
        return gave_up(format!("staging the new companion failed: {e:#}"));
    }
    if let Err(e) = ops.rename_old_aside() {
        ops.discard_new();
        return gave_up(format!("moving the old companion aside failed: {e:#}"));
    }
    if let Err(e) = ops.promote_new() {
        let failed = format!("putting the new companion in place failed: {e:#}");
        let reason = match ops.restore_old() {
            Ok(()) => failed,
            Err(back) => format!("{failed}; putting the old one back failed too: {back:#}"),
        };
        ops.discard_new();
        return gave_up(reason);
    }
    let pids = ops.running();
    let stopped = if pids.is_empty() {
        Vec::new()
    } else {
        ops.stop(&pids)
    };
    if stopped.len() != pids.len() {
        let survived = format!(
            "could not stop every running companion (running {pids:?}, stopped {stopped:?})"
        );
        let reason = match ops.restore_old() {
            Ok(()) => survived,
            Err(back) => format!("{survived}; putting the old one back failed: {back:#}"),
        };
        let start = start_stopped(ops, &stopped, lost);
        return SwapOutcome::Aborted {
            reason,
            stopped,
            start,
        };
    }
    ops.write_marker();
    let start = start_stopped(ops, &stopped, lost);
    ops.remove_old();
    SwapOutcome::Swapped { stopped, start }
}

/// #3824 — start what the swap stopped, and record whether that brought the
/// tray back.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn start_stopped<O: SwapOps>(ops: &mut O, stopped: &[u32], lost: &TrayLost) -> Option<TrayStart> {
    if stopped.is_empty() {
        return None;
    }
    let start = ops.start();
    if matches!(start, TrayStart::Started { .. }) {
        lost.clear();
    } else {
        lost.set();
    }
    Some(start)
}

/// #3824 — what a start of the tray came to. The refresh's line and the
/// consent prompt's verdict read it, so neither says a tray started when none
/// did.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrayStart {
    /// The process was created: its pid, and its session when known.
    Started { pid: u32, session: Option<u32> },
    /// Nobody is signed in where it would start: a service starts the tray
    /// for the person at the console.
    NobodySignedIn,
    /// The start failed, and why (`CreateProcessAsUserW … (err 5)`).
    Failed(String),
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl TrayStart {
    /// What came of it, for a log line.
    fn describe(&self) -> String {
        match self {
            Self::Started {
                pid,
                session: Some(session),
            } => format!("pid {pid}, session {session}"),
            Self::Started { pid, session: None } => format!("pid {pid}"),
            Self::NobodySignedIn => "nobody is signed in at this computer's screen".to_string(),
            Self::Failed(e) => e.clone(),
        }
    }
}

/// #3824 — a tray this process's refresh stopped and could not start again.
///
/// Set when a swap stopped the tray and its start did not bring it back
/// (nobody signed in to start it for counts); forgotten when a start works or
/// a tray is seen running. Only while it is set does the refresh start a tray
/// that is not running: a tray the person quit, or one that never ran, stays
/// as it is. In memory, on purpose: after a restart the sign-in start (the
/// `Roomler Desktop` Run value) owns the tray.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Default)]
pub(crate) struct TrayLost {
    lost: std::sync::atomic::AtomicBool,
    /// Tries since it was lost that did not bring it back: the first one is
    /// a warn line, the rest debug.
    failed_tries: std::sync::atomic::AtomicU32,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl TrayLost {
    pub(crate) const fn new() -> Self {
        Self {
            lost: std::sync::atomic::AtomicBool::new(false),
            failed_tries: std::sync::atomic::AtomicU32::new(0),
        }
    }

    pub(crate) fn is_set(&self) -> bool {
        self.lost.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A swap stopped the tray, and its start did not bring it back.
    fn set(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        self.failed_tries.store(0, SeqCst);
        self.lost.store(true, SeqCst);
    }

    /// A start worked, or a tray is running.
    fn clear(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        self.lost.store(false, SeqCst);
        self.failed_tries.store(0, SeqCst);
    }

    /// One more try that did not bring it back: `true` for the first.
    fn failed_try(&self) -> bool {
        self.failed_tries
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
    }
}

/// This process's [`TrayLost`].
#[cfg(target_os = "windows")]
static TRAY_LOST: TrayLost = TrayLost::new();

/// #3824 — the refresh's line for a swap. One renderer, tested, so a start
/// that failed is a warning with its error, and "respawned" is said only of a
/// start that worked.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefreshLine {
    /// INFO `desktop companion refreshed`, with `tray=`: what became of it.
    Refreshed { tray: String },
    /// WARN `desktop companion refreshed, but the tray could not be started
    /// again`, with `error=`.
    TrayNotBack { error: String },
    /// WARN `desktop companion refresh skipped`, with `error=`: why, then
    /// what became of the tray.
    Skipped { error: String },
}

/// #3824 — [`RefreshLine`] for a swap's outcome.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn refresh_line(outcome: &SwapOutcome) -> RefreshLine {
    match outcome {
        SwapOutcome::Swapped { start: None, .. } => RefreshLine::Refreshed {
            tray: "none was running".to_string(),
        },
        SwapOutcome::Swapped {
            start: Some(started @ TrayStart::Started { .. }),
            ..
        } => RefreshLine::Refreshed {
            tray: format!("respawned ({})", started.describe()),
        },
        SwapOutcome::Swapped {
            start: Some(not_started),
            ..
        } => RefreshLine::TrayNotBack {
            error: not_started.describe(),
        },
        SwapOutcome::Aborted { reason, start, .. } => RefreshLine::Skipped {
            error: match start {
                None => format!("{reason} (no tray was stopped)"),
                Some(started @ TrayStart::Started { .. }) => format!(
                    "{reason} (the tray it had stopped was respawned: {})",
                    started.describe()
                ),
                Some(not_started) => format!(
                    "{reason} (the tray it had stopped could not be started again: {})",
                    not_started.describe()
                ),
            },
        },
    }
}

/// #3824 — what a try to bring back a lost tray found or did.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LostTrayStart {
    /// A tray runs in the session it would start in.
    Running,
    /// No `Roomler Desktop` Run value for this install: the tray is not set
    /// to start at sign-in, so it is not started either.
    NotSetToStart,
    /// The start, or nobody signed in to start it for.
    Tried(TrayStart),
}

/// #3824 — the line for a try to bring back a lost tray.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LostTrayLine {
    /// INFO `desktop companion is back after a refresh stopped it`, with
    /// `how=`.
    Back { how: String },
    /// `desktop companion is still not running after a refresh stopped it;
    /// it is tried again`, with `why=`: WARN for the first try that did not
    /// bring it back, DEBUG for the rest.
    StillDown { why: String, first: bool },
}

/// #3824 — after a refresh pass, start a tray an earlier pass stopped and
/// could not start again. Nothing at all when none was lost: a tray the
/// person quit, or one that never ran, stays as it is. Nothing either when
/// this pass stopped a tray: its own start has tried already, or marked the
/// tray lost. A start that works, or a tray seen running, forgets the loss;
/// anything else leaves it for the next pass.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn restore_lost_tray(
    pass_stopped_a_tray: bool,
    lost: &TrayLost,
    start: impl FnOnce() -> LostTrayStart,
) -> Option<LostTrayLine> {
    if pass_stopped_a_tray || !lost.is_set() {
        return None;
    }
    let back = |how: String| {
        lost.clear();
        Some(LostTrayLine::Back { how })
    };
    let still_down = |why: String| {
        Some(LostTrayLine::StillDown {
            why,
            first: lost.failed_try(),
        })
    };
    match start() {
        LostTrayStart::Running => back("one is running again".to_string()),
        LostTrayStart::Tried(started @ TrayStart::Started { .. }) => {
            back(format!("started again ({})", started.describe()))
        }
        LostTrayStart::NotSetToStart => still_down(
            "it is not set to start at sign-in here (no `Roomler Desktop` Run value)".to_string(),
        ),
        LostTrayStart::Tried(not_started) => still_down(not_started.describe()),
    }
}

/// #3824 — how many `…exe.old.<n>` names a swap tries after `…exe.old`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const ASIDE_NAMES: u32 = 8;

/// #3824 — the name to move the installed EXE aside to: `old` when `free`
/// says it is free (it may free it), else the first free `old.<n>`. A copy
/// still mapped by a tray, or held by a scanner, refuses a delete and a
/// replace onto it alike, and used to fail the next swap's rename. `None`
/// when every name is held.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn free_aside_name(
    old: &std::path::Path,
    mut free: impl FnMut(&std::path::Path) -> bool,
) -> Option<std::path::PathBuf> {
    std::iter::once(old.to_path_buf())
        .chain((1..=ASIDE_NAMES).map(|n| {
            let mut name = old.as_os_str().to_os_string();
            name.push(format!(".{n}"));
            std::path::PathBuf::from(name)
        }))
        .find(|name| free(name))
}

/// #3824 — is `name` a copy a swap moved aside to `base` (`base` itself or
/// `base.<n>`)? Never the version marker, the staged `.new` or the EXE.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn is_aside_name(name: &str, base: &str) -> bool {
    let (name, base) = (name.to_ascii_lowercase(), base.to_ascii_lowercase());
    match name.strip_prefix(base.as_str()) {
        Some("") => true,
        Some(rest) => rest
            .strip_prefix('.')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        None => false,
    }
}

#[cfg(target_os = "windows")]
struct FsSwap {
    staged: std::path::PathBuf,
    dest: std::path::PathBuf,
    new: std::path::PathBuf,
    /// `…exe.old`: the first name the installed EXE is moved aside to.
    old: std::path::PathBuf,
    /// Where this swap moved it, once it has.
    aside: Option<std::path::PathBuf>,
    marker: std::path::PathBuf,
    version: &'static str,
    respawn: RespawnContext,
}

#[cfg(target_os = "windows")]
impl SwapOps for FsSwap {
    fn stage(&mut self) -> Result<()> {
        let _ = std::fs::remove_file(&self.new);
        std::fs::copy(&self.staged, &self.new)
            .map(|_| ())
            .context("copying the new desktop EXE beside the installed one")
    }
    fn rename_old_aside(&mut self) -> Result<()> {
        if !self.dest.exists() {
            return Ok(());
        }
        // A copy an earlier swap left is usually free by now. One that is not
        // (a tray that would not stop still runs from it, a scanner holds it)
        // refuses a delete and a replace onto it alike: the next free name.
        let aside = free_aside_name(&self.old, |p| {
            !p.exists() || std::fs::remove_file(p).is_ok()
        })
        .context("no free name to move the desktop EXE aside to")?;
        std::fs::rename(&self.dest, &aside).context("renaming the desktop EXE aside")?;
        self.aside = Some(aside);
        Ok(())
    }
    fn promote_new(&mut self) -> Result<()> {
        std::fs::rename(&self.new, &self.dest).context("renaming the new desktop EXE into place")
    }
    fn restore_old(&mut self) -> Result<()> {
        let Some(aside) = self.aside.take() else {
            return Ok(());
        };
        if let Err(e) = std::fs::rename(&aside, &self.dest) {
            self.aside = Some(aside);
            return Err(e).context("renaming the old desktop EXE back into place");
        }
        Ok(())
    }
    fn discard_new(&mut self) {
        let _ = std::fs::remove_file(&self.new);
    }
    fn running(&mut self) -> Vec<u32> {
        crate::win_service::companion_procs::find(&self.dest)
            .iter()
            .map(|p| p.pid)
            .collect()
    }
    fn stop(&mut self, pids: &[u32]) -> Vec<u32> {
        crate::win_service::companion_procs::terminate_verified(
            &self.dest,
            pids,
            std::time::Duration::from_secs(10),
        )
    }
    fn write_marker(&mut self) {
        if let Err(e) = std::fs::write(&self.marker, format!("{}\n", self.version)) {
            tracing::warn!(error = %e, "could not write desktop version marker");
        }
    }
    fn start(&mut self) -> TrayStart {
        start_desktop(self.respawn, &self.dest, &[])
    }
    fn remove_old(&mut self) {
        // The tray just stopped may take a moment to release its image: a few
        // short retries, then the next swap's sweep.
        if let Some(aside) = &self.aside {
            for _ in 0..3 {
                if std::fs::remove_file(aside).is_ok() || !aside.exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        // The copies earlier swaps left, best-effort.
        let Some(entries) = self.dest.parent().and_then(|d| std::fs::read_dir(d).ok()) else {
            return;
        };
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| is_aside_name(name, OLD_SUFFIX))
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Prefer the signed EXE (no `-unsigned` infix) when both are present.
#[cfg(target_os = "windows")]
fn pick_desktop_asset(
    assets: &[crate::updater::GithubAsset],
) -> Option<&crate::updater::GithubAsset> {
    let is_desktop = |name: &str| {
        let lower = name.to_lowercase();
        lower.starts_with("roomler-desktop-") && lower.ends_with(".exe")
    };
    assets
        .iter()
        .find(|a| is_desktop(&a.name) && !a.name.to_lowercase().contains("-unsigned"))
        .or_else(|| assets.iter().find(|a| is_desktop(&a.name)))
}

/// The companion beside this daemon — the one install whose companion this
/// process refreshes, launches and asks about.
#[cfg(target_os = "windows")]
fn sibling_desktop() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(DESKTOP_EXE)))
}

/// Is the companion beside this daemon running? Matched by the path it was
/// STARTED from (#1686), not by image name: a companion whose file an update
/// renamed or deleted keeps running under a file-id image name, and a
/// `tasklist IMAGENAME` check stops seeing it. `pub`: the S1b appdirs
/// migration skips while the desktop runs (it may hold open handles inside
/// the tree being moved).
#[cfg(target_os = "windows")]
pub fn desktop_running() -> bool {
    sibling_desktop().is_some_and(|exe| !crate::win_service::companion_procs::find(&exe).is_empty())
}

/// Is the companion running in Terminal Services session `session`? The
/// launcher's question is "does THIS user already have it open", which a
/// machine-wide listing cannot answer on a host with more than one session.
#[cfg(target_os = "windows")]
pub(crate) fn desktop_running_in_session(session: u32) -> bool {
    sibling_desktop().is_some_and(|exe| {
        crate::win_service::companion_procs::find(&exe)
            .iter()
            .any(|p| p.session == Some(session))
    })
}

/// Start the tray at `dest` with `args`: from a service, into the console
/// session as its person; from the person's own daemon, a plain detached
/// start. Says what came of it and logs nothing: the caller's line does
/// (#3824).
#[cfg(target_os = "windows")]
fn start_desktop(respawn: RespawnContext, dest: &std::path::Path, args: &[&str]) -> TrayStart {
    match respawn {
        // FR-84 D6 / #1035 — NOT `std::process::Command`: that passes
        // `bInheritHandles = TRUE`, and a companion lives for months holding
        // whatever this daemon had marked inheritable at the spawn instant.
        RespawnContext::UserSession => {
            match crate::win_service::companion_spawn::spawn_detached_no_inherit(dest, args) {
                Ok(pid) => TrayStart::Started {
                    pid,
                    session: crate::win_service::companion_spawn::own_session_id(),
                },
                Err(e) => TrayStart::Failed(format!("{e:#}")),
            }
        }
        RespawnContext::SystemService => {
            use crate::win_service::supervisor;
            let Some(session) = supervisor::active_console_session_id() else {
                return TrayStart::NobodySignedIn;
            };
            match supervisor::query_user_token(session) {
                Ok(Some(token)) => {
                    // SAFETY: `token` is a live user token from
                    // WTSQueryUserToken, held for the duration of the call.
                    let child = unsafe { supervisor::spawn_in_session(token.raw(), dest, args) };
                    match child {
                        Ok(child) => TrayStart::Started {
                            pid: child.pid,
                            session: Some(session),
                        },
                        Err(e) => TrayStart::Failed(format!("{e:#}")),
                    }
                }
                Ok(None) => TrayStart::NobodySignedIn,
                Err(e) => TrayStart::Failed(format!("{e:#}")),
            }
        }
    }
}

/// #3824 — start a tray this process's refresh stopped and could not start
/// again: where someone is signed in, none runs, and the tray is set to start
/// at sign-in. With `--autostart`, as the sign-in start does, so a person who
/// switched that off sees nothing (`decide_startup` exits).
#[cfg(target_os = "windows")]
fn start_lost_tray(respawn: RespawnContext, dest: &std::path::Path) -> LostTrayStart {
    use roomler_node_core::companion_autostart::AUTOSTART_ARG;
    let Some(session) = signed_in_session(respawn) else {
        return LostTrayStart::Tried(TrayStart::NobodySignedIn);
    };
    if desktop_running_in_session(session) {
        return LostTrayStart::Running;
    }
    if !set_to_start_at_sign_in(dest) {
        return LostTrayStart::NotSetToStart;
    }
    LostTrayStart::Tried(start_desktop(respawn, dest, &[AUTOSTART_ARG]))
}

/// The session a start would go to, when someone is signed in there: the
/// console session's person for a service, the daemon's own session for a
/// daemon that runs as the person.
#[cfg(target_os = "windows")]
fn signed_in_session(respawn: RespawnContext) -> Option<u32> {
    use crate::win_service::{companion_spawn, supervisor};
    match respawn {
        RespawnContext::SystemService => {
            let session = supervisor::active_console_session_id()?;
            matches!(supervisor::query_user_token(session), Ok(Some(_))).then_some(session)
        }
        // Session 0 is services' session: nothing there is visible.
        RespawnContext::UserSession => companion_spawn::own_session_id().filter(|s| *s != 0),
    }
}

/// Is the tray set to start at sign-in: a `Roomler Desktop` Run value in the
/// hive this install owns? `HKLM` for a machine-wide install, present while
/// the device's switch is on (a person's own opt-out is `--autostart`'s to
/// honour); `HKCU` for a per-user one, gone once its person opted out.
#[cfg(target_os = "windows")]
fn set_to_start_at_sign_in(dest: &std::path::Path) -> bool {
    use roomler_node_core::companion_autostart::{
        InstallScope, WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME, program_files_dirs,
        registry::{Hive, read_value},
        windows_scope_for_exe,
    };
    let hive = match windows_scope_for_exe(dest, &program_files_dirs()) {
        InstallScope::Machine => Hive::LocalMachine,
        InstallScope::User => Hive::CurrentUser,
    };
    matches!(
        read_value(hive, WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME),
        Ok(Some(command)) if !command.trim().is_empty()
    )
}

/// #1686, #3824 — the tray swap's ORDER, a tray lost to a failed start, and
/// the lines the refresh logs, on every platform (the Windows ops themselves
/// are exercised by `win_service::companion_procs`' tests, which run only
/// where Windows does).
#[cfg(test)]
mod swap_tests {
    use super::{
        EnsureOutcome, LostTrayLine, LostTrayStart, RefreshLine, SwapOps, SwapOutcome, TrayLost,
        TrayStart, free_aside_name, is_aside_name, refresh_line, restore_lost_tray,
        started_outcome, swap,
    };
    use anyhow::{Result, anyhow};
    use std::path::{Path, PathBuf};

    const DENIED: &str = "Access is denied. (os error 5)";

    fn started() -> TrayStart {
        TrayStart::Started {
            pid: 100,
            session: Some(1),
        }
    }

    /// The release lab's respawn failure (#3824).
    fn refused() -> TrayStart {
        TrayStart::Failed(
            r"CreateProcessAsUserW(C:\Program Files\Roomler\roomler-desktop.exe) failed (err 5)"
                .to_string(),
        )
    }

    struct Mock {
        calls: Vec<&'static str>,
        stage_fails: bool,
        aside_fails: bool,
        promote_fails: bool,
        restore_fails: bool,
        running: Vec<u32>,
        /// `None` = stop everything asked.
        stops_only: Option<Vec<u32>>,
        start: TrayStart,
    }

    impl Default for Mock {
        fn default() -> Self {
            Self {
                calls: Vec::new(),
                stage_fails: false,
                aside_fails: false,
                promote_fails: false,
                restore_fails: false,
                running: Vec::new(),
                stops_only: None,
                start: started(),
            }
        }
    }

    fn fails(fail: bool, why: &str) -> Result<()> {
        if fail { Err(anyhow!("{why}")) } else { Ok(()) }
    }

    impl SwapOps for Mock {
        fn stage(&mut self) -> Result<()> {
            self.calls.push("stage");
            fails(self.stage_fails, "disk full")
        }
        fn rename_old_aside(&mut self) -> Result<()> {
            self.calls.push("rename_old_aside");
            fails(self.aside_fails, DENIED)
        }
        fn promote_new(&mut self) -> Result<()> {
            self.calls.push("promote_new");
            fails(self.promote_fails, DENIED)
        }
        fn restore_old(&mut self) -> Result<()> {
            self.calls.push("restore_old");
            fails(self.restore_fails, DENIED)
        }
        fn discard_new(&mut self) {
            self.calls.push("discard_new");
        }
        fn running(&mut self) -> Vec<u32> {
            self.calls.push("running");
            self.running.clone()
        }
        fn stop(&mut self, pids: &[u32]) -> Vec<u32> {
            self.calls.push("stop");
            self.stops_only.clone().unwrap_or_else(|| pids.to_vec())
        }
        fn write_marker(&mut self) {
            self.calls.push("write_marker");
        }
        fn start(&mut self) -> TrayStart {
            self.calls.push("start");
            self.start.clone()
        }
        fn remove_old(&mut self) {
            self.calls.push("remove_old");
        }
    }

    /// #3824: the tray is stopped only once the new EXE is in place (NTFS
    /// renames a running image aside), then started again.
    #[test]
    fn a_running_tray_is_stopped_only_after_the_new_exe_is_in_place() {
        let lost = TrayLost::default();
        let mut m = Mock {
            running: vec![7, 9],
            ..Default::default()
        };
        let out = swap(&mut m, &lost);
        assert_eq!(
            out,
            SwapOutcome::Swapped {
                stopped: vec![7, 9],
                start: Some(started())
            }
        );
        assert_eq!(
            m.calls,
            [
                "stage",
                "rename_old_aside",
                "promote_new",
                "running",
                "stop",
                "write_marker",
                "start",
                "remove_old"
            ]
        );
        assert!(!lost.is_set());
    }

    /// A first run (a wizard-placed EXE with no marker), or a tray the person
    /// quit: none runs, so none is stopped, none is started, and none counts
    /// as lost (the post-install launcher owns the first launch).
    #[test]
    fn a_swap_with_no_tray_running_starts_none() {
        let lost = TrayLost::default();
        let mut m = Mock::default();
        let out = swap(&mut m, &lost);
        assert_eq!(
            out,
            SwapOutcome::Swapped {
                stopped: vec![],
                start: None
            }
        );
        assert!(!m.calls.contains(&"stop") && !m.calls.contains(&"start"));
        assert!(!lost.is_set());
        assert_eq!(
            restore_lost_tray(out.stopped_a_tray(), &lost, || unreachable!(
                "no tray was lost"
            )),
            None
        );
    }

    /// A failed copy leaves everything as it was.
    #[test]
    fn a_failed_stage_touches_nothing_and_stops_nothing() {
        let lost = TrayLost::default();
        let mut m = Mock {
            stage_fails: true,
            running: vec![7],
            ..Default::default()
        };
        let out = swap(&mut m, &lost);
        assert!(!out.stopped_a_tray(), "{out:?}");
        assert!(matches!(out, SwapOutcome::Aborted { start: None, .. }));
        assert_eq!(m.calls, ["stage"]);
    }

    /// #3824, the release lab's case: the rename aside refused (a deny ACE
    /// there; a scanner's handle or endpoint protection in the field). The
    /// tray used to be stopped by then, and its start from the old EXE
    /// failed. Now it is left running, and nothing is started.
    #[test]
    fn a_refused_rename_aside_leaves_the_running_tray_running() {
        let lost = TrayLost::default();
        let mut m = Mock {
            running: vec![7],
            aside_fails: true,
            ..Default::default()
        };
        let out = swap(&mut m, &lost);
        assert_eq!(m.calls, ["stage", "rename_old_aside", "discard_new"]);
        let SwapOutcome::Aborted {
            reason,
            stopped,
            start,
        } = out
        else {
            panic!("{out:?}")
        };
        assert!(
            reason.starts_with("moving the old companion aside failed: Access is denied"),
            "{reason}"
        );
        assert_eq!((stopped, start), (vec![], None));
        assert!(!lost.is_set());
    }

    /// A promote that fails puts the old EXE back, and the tray, never
    /// stopped, runs on.
    #[test]
    fn a_failed_promote_puts_the_old_exe_back_and_leaves_the_tray_running() {
        let lost = TrayLost::default();
        let mut m = Mock {
            running: vec![7],
            promote_fails: true,
            ..Default::default()
        };
        let out = swap(&mut m, &lost);
        assert_eq!(
            m.calls,
            [
                "stage",
                "rename_old_aside",
                "promote_new",
                "restore_old",
                "discard_new"
            ]
        );
        assert!(!out.stopped_a_tray(), "{out:?}");
        assert!(matches!(out, SwapOutcome::Aborted { start: None, .. }));
    }

    /// Both renames refused: no EXE at the installed path until the retry,
    /// and the line says so. The tray still runs from the copy moved aside.
    #[test]
    fn a_put_back_that_fails_is_said() {
        let lost = TrayLost::default();
        let mut m = Mock {
            running: vec![7],
            promote_fails: true,
            restore_fails: true,
            ..Default::default()
        };
        let out = swap(&mut m, &lost);
        let SwapOutcome::Aborted {
            reason, stopped, ..
        } = out
        else {
            panic!("{out:?}")
        };
        assert!(
            reason.contains("; putting the old one back failed too: Access is denied"),
            "{reason}"
        );
        assert!(stopped.is_empty() && !m.calls.contains(&"stop"));
    }

    /// A tray that would not stop keeps the single-instance lock: the old EXE
    /// goes back over the new one, which nothing runs, what was stopped is
    /// started again, and the marker is left for the retry.
    #[test]
    fn a_tray_that_would_not_stop_gets_the_old_exe_back() {
        let lost = TrayLost::default();
        let mut m = Mock {
            running: vec![7, 9],
            stops_only: Some(vec![7]),
            ..Default::default()
        };
        let out = swap(&mut m, &lost);
        assert_eq!(
            m.calls,
            [
                "stage",
                "rename_old_aside",
                "promote_new",
                "running",
                "stop",
                "restore_old",
                "start"
            ]
        );
        let SwapOutcome::Aborted {
            reason,
            stopped,
            start,
        } = out
        else {
            panic!("{out:?}")
        };
        assert!(
            reason.starts_with("could not stop every running companion"),
            "{reason}"
        );
        assert_eq!((stopped, start), (vec![7], Some(started())));
        assert!(!lost.is_set());
    }

    /// #3824: a tray the swap stopped and could not start again is lost, and
    /// so is one stopped with nobody signed in to start it for. A start that
    /// works forgets it.
    #[test]
    fn a_failed_start_after_a_stop_marks_the_tray_lost() {
        let lost = TrayLost::default();
        let mut m = Mock {
            running: vec![7],
            start: refused(),
            ..Default::default()
        };
        assert_eq!(
            swap(&mut m, &lost),
            SwapOutcome::Swapped {
                stopped: vec![7],
                start: Some(refused())
            }
        );
        assert!(lost.is_set(), "a stop whose start failed");

        let nobody = TrayLost::default();
        let mut m = Mock {
            running: vec![7],
            start: TrayStart::NobodySignedIn,
            ..Default::default()
        };
        swap(&mut m, &nobody);
        assert!(nobody.is_set(), "nobody signed in to start it for");

        let mut m = Mock {
            running: vec![7],
            ..Default::default()
        };
        swap(&mut m, &lost);
        assert!(!lost.is_set(), "a start that works forgets it");
    }

    /// #3824, the issue's second point: after a failed start, the next pass
    /// that stops no tray (here a swap with none running) starts it. Once the
    /// tray is back, a later pass starts none: the person may quit it.
    #[test]
    fn a_tray_lost_to_a_failed_start_is_started_by_the_next_pass() {
        let lost = TrayLost::default();
        let mut first = Mock {
            running: vec![7],
            start: refused(),
            ..Default::default()
        };
        swap(&mut first, &lost);
        let mut retry = Mock::default();
        let out = swap(&mut retry, &lost);
        let line = restore_lost_tray(out.stopped_a_tray(), &lost, || {
            LostTrayStart::Tried(started())
        });
        assert_eq!(
            line,
            Some(LostTrayLine::Back {
                how: "started again (pid 100, session 1)".to_string()
            })
        );
        assert!(!lost.is_set());
        assert_eq!(
            restore_lost_tray(false, &lost, || unreachable!("a quit tray stays quit")),
            None
        );
    }

    /// #3824, the field case after the fix: the swap lands and its start
    /// fails, so the marker reads current and the passes after it have
    /// nothing to swap. Each tries the start, with one warning; a tray seen
    /// running ends the tries.
    #[test]
    fn a_pass_with_nothing_to_swap_tries_the_lost_tray_until_it_runs() {
        let lost = TrayLost::default();
        swap(
            &mut Mock {
                running: vec![7],
                start: refused(),
                ..Default::default()
            },
            &lost,
        );
        let still_down = |line: Option<LostTrayLine>| match line {
            Some(LostTrayLine::StillDown { why, first }) => (why, first),
            other => panic!("{other:?}"),
        };
        let (why, first) = still_down(restore_lost_tray(false, &lost, || {
            LostTrayStart::Tried(refused())
        }));
        assert!(first && why.contains("(err 5)"), "{why}");
        let (why, first) = still_down(restore_lost_tray(false, &lost, || {
            LostTrayStart::Tried(TrayStart::NobodySignedIn)
        }));
        assert!(!first && why.contains("nobody is signed in"), "{why}");
        let (why, first) = still_down(restore_lost_tray(false, &lost, || {
            LostTrayStart::NotSetToStart
        }));
        assert!(!first && why.contains("not set to start"), "{why}");
        assert!(lost.is_set());
        // The person signs in, and the sign-in start brings it back.
        assert_eq!(
            restore_lost_tray(false, &lost, || LostTrayStart::Running),
            Some(LostTrayLine::Back {
                how: "one is running again".to_string()
            })
        );
        assert!(!lost.is_set());
        assert_eq!(
            restore_lost_tray(false, &lost, || unreachable!("forgotten")),
            None
        );
    }

    /// A pass that stopped a tray leaves the tray to its own start: no second
    /// start, and no second warning, in the same pass.
    #[test]
    fn a_pass_that_stopped_a_tray_tries_no_second_start() {
        let lost = TrayLost::default();
        let mut m = Mock {
            running: vec![7],
            start: refused(),
            ..Default::default()
        };
        let out = swap(&mut m, &lost);
        assert!(out.stopped_a_tray() && lost.is_set());
        assert_eq!(
            restore_lost_tray(out.stopped_a_tray(), &lost, || unreachable!(
                "one start a pass"
            )),
            None
        );
    }

    /// #3824: the line says whether the start worked, and "respawned" only
    /// when it did. It used to read `respawned it: true` after a failed one.
    #[test]
    fn the_refresh_line_never_says_respawned_after_a_failed_start() {
        for outcome in [
            SwapOutcome::Swapped {
                stopped: vec![7],
                start: Some(refused()),
            },
            SwapOutcome::Swapped {
                stopped: vec![7],
                start: Some(TrayStart::NobodySignedIn),
            },
            SwapOutcome::Aborted {
                reason: "could not stop every running companion (running [7, 9], stopped [7])"
                    .to_string(),
                stopped: vec![7],
                start: Some(refused()),
            },
        ] {
            let line = refresh_line(&outcome);
            assert!(!format!("{line:?}").contains("respawned"), "{line:?}");
        }
        assert_eq!(
            refresh_line(&SwapOutcome::Swapped {
                stopped: vec![7],
                start: Some(refused())
            }),
            RefreshLine::TrayNotBack {
                error: r"CreateProcessAsUserW(C:\Program Files\Roomler\roomler-desktop.exe) failed (err 5)"
                    .to_string()
            }
        );
    }

    /// #3824: what the line says became of the tray, in each case.
    #[test]
    fn the_refresh_line_says_what_became_of_the_tray() {
        assert_eq!(
            refresh_line(&SwapOutcome::Swapped {
                stopped: vec![7],
                start: Some(started())
            }),
            RefreshLine::Refreshed {
                tray: "respawned (pid 100, session 1)".to_string()
            }
        );
        assert_eq!(
            refresh_line(&SwapOutcome::Swapped {
                stopped: vec![],
                start: None
            }),
            RefreshLine::Refreshed {
                tray: "none was running".to_string()
            }
        );
        assert_eq!(
            refresh_line(&SwapOutcome::Aborted {
                reason: format!("moving the old companion aside failed: {DENIED}"),
                stopped: vec![],
                start: None
            }),
            RefreshLine::Skipped {
                error: format!(
                    "moving the old companion aside failed: {DENIED} (no tray was stopped)"
                )
            }
        );
        assert_eq!(
            refresh_line(&SwapOutcome::Aborted {
                reason: "could not stop every running companion (running [7, 9], stopped [7])"
                    .to_string(),
                stopped: vec![7],
                start: Some(started())
            }),
            RefreshLine::Skipped {
                error: "could not stop every running companion (running [7, 9], stopped [7]) \
                        (the tray it had stopped was respawned: pid 100, session 1)"
                    .to_string()
            }
        );
        let RefreshLine::Skipped { error } = refresh_line(&SwapOutcome::Aborted {
            reason: "could not stop every running companion (running [7, 9], stopped [7])"
                .to_string(),
            stopped: vec![7],
            start: Some(refused()),
        }) else {
            panic!("a skip")
        };
        assert!(
            error.ends_with(
                "(the tray it had stopped could not be started again: \
                 CreateProcessAsUserW(C:\\Program Files\\Roomler\\roomler-desktop.exe) failed (err 5))"
            ),
            "{error}"
        );
    }

    /// #3824: the consent prompt is told a tray started only when one did; it
    /// then reports `no_prompt_surface` instead of waiting out its window.
    #[test]
    fn ensure_says_started_only_after_a_start_that_worked() {
        assert_eq!(
            started_outcome(started()).expect("a start that worked"),
            EnsureOutcome::Started
        );
        let e = started_outcome(refused())
            .expect_err("a failed start")
            .to_string();
        assert!(e.contains("(err 5)"), "{e}");
        let e = started_outcome(TrayStart::NobodySignedIn)
            .expect_err("nobody to start it for")
            .to_string();
        assert!(e.contains("nobody is signed in"), "{e}");
    }

    /// #3824: a copy an earlier swap left that cannot be removed (a tray or a
    /// scanner still holds it, so a delete and a replace onto it are both
    /// refused) does not block the swap: the next free name takes this one.
    #[test]
    fn the_old_exe_goes_aside_under_the_first_free_name() {
        let old = Path::new(r"C:\Program Files\Roomler\roomler-desktop.exe.old");
        let numbered = |n: u32| PathBuf::from(format!("{}.{n}", old.display()));
        assert_eq!(free_aside_name(old, |_| true), Some(old.to_path_buf()));
        let held = [old.to_path_buf(), numbered(1)];
        assert_eq!(
            free_aside_name(old, |p| !held.iter().any(|h| h == p)),
            Some(numbered(2))
        );
        let mut asked = Vec::new();
        assert_eq!(
            free_aside_name(old, |p| {
                asked.push(p.to_path_buf());
                false
            }),
            None,
            "every name held: the swap stops nothing and says why"
        );
        assert_eq!(asked.first(), Some(&old.to_path_buf()));
        assert_eq!(asked.last(), Some(&numbered(8)));
    }

    /// The sweep takes the copies moved aside, never the marker, the staged
    /// copy or the installed EXE.
    #[test]
    fn only_copies_moved_aside_are_swept() {
        let base = "roomler-desktop.exe.old";
        for name in [
            "roomler-desktop.exe.old",
            "ROOMLER-DESKTOP.EXE.OLD",
            "roomler-desktop.exe.old.1",
            "roomler-desktop.exe.old.12",
        ] {
            assert!(is_aside_name(name, base), "{name}");
        }
        for name in [
            "roomler-desktop.exe",
            "roomler-desktop.exe.version",
            "roomler-desktop.exe.new",
            "roomler-desktop.exe.old.",
            "roomler-desktop.exe.old.x",
            "roomler-desktop.exe.old.1.bak",
            "roomler-desktop.exe.older",
        ] {
            assert!(!is_aside_name(name, base), "{name}");
        }
    }
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::{graphical_session_via, session_ids, show_field};

    const GREETER: &str =
        "Id=c1\nName=gdm\nUser=124\nSeat=seat0\nType=wayland\nClass=greeter\nActive=yes\n";
    const PERSON: &str =
        "Id=2\nName=ana\nUser=1000\nSeat=seat0\nType=wayland\nClass=user\nActive=yes\n";

    /// A `loginctl` that knows these `show-session` outputs, listed in order.
    fn loginctl<'a>(sessions: &'a [&'a str]) -> impl Fn(&[&str]) -> anyhow::Result<String> + 'a {
        move |args: &[&str]| {
            Ok(match args {
                ["list-sessions", ..] => sessions
                    .iter()
                    .filter_map(|s| show_field(s, "Id"))
                    .map(|id| format!("{id} 0 someone seat0\n"))
                    .collect(),
                ["show-session", id, ..] => sessions
                    .iter()
                    .find(|s| show_field(s, "Id").as_deref() == Some(*id))
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                other => panic!("unexpected loginctl {other:?}"),
            })
        }
    }

    /// FR-56 P7 — a greeter answers the lookup that wants one (consent), never
    /// one that asks for a person, and being the reason nobody qualifies is
    /// said out loud. A person's session wins beside it in either order.
    #[test]
    fn a_greeter_answers_only_the_lookup_that_wants_one() {
        let session = graphical_session_via(None, false, &loginctl(&[GREETER])).unwrap();
        assert_eq!(session.name, "gdm");

        let e = graphical_session_via(None, true, &loginctl(&[GREETER]))
            .map(|s| s.name)
            .unwrap_err()
            .to_string();
        assert!(e.contains("the login screen is showing"), "{e}");

        for order in [[GREETER, PERSON], [PERSON, GREETER]] {
            let session = graphical_session_via(None, true, &loginctl(&order)).unwrap();
            assert_eq!(session.name, "ana");
        }
    }

    /// Only an ACTIVE greeter is what the screen shows; with no greeter at all
    /// the sentence stays the one a headless server has always had.
    #[test]
    fn no_login_screen_keeps_the_headless_sentence() {
        let hidden = GREETER.replace("Active=yes", "Active=no");
        for sessions in [vec![], vec![hidden.as_str()]] {
            let e = graphical_session_via(None, true, &loginctl(&sessions))
                .map(|s| s.name)
                .unwrap_err()
                .to_string();
            assert!(e.contains("no active graphical session"), "{e}");
        }
    }

    /// FR-84 D6 — the companion runs as its OWN transient unit, never in the
    /// daemon's cgroup; a root daemon targets the session's uid, a per-user
    /// daemon its own manager (no polkit prompt nobody can answer), and the
    /// session's bus + display travel along.
    #[test]
    fn systemd_run_command_for_root_and_for_a_user_daemon() {
        let sess = super::GraphicalSession {
            uid: 1000,
            name: "ana".into(),
            display: Some(":0".into()),
            wayland_display: Some("wayland-0".into()),
        };
        let argv = |user: bool| -> Vec<String> {
            // The pure half: no user manager is asked from a test.
            let cmd = super::systemd_run_command_with(
                &sess,
                std::path::Path::new("/usr/bin/roomler-desktop"),
                &["--first-run"],
                user,
                &[],
            );
            assert_eq!(cmd.get_program(), "systemd-run");
            cmd.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let root = argv(false);
        assert!(root.contains(&"--uid=1000".to_string()), "{root:?}");
        assert!(!root.contains(&"--user".to_string()), "{root:?}");
        for want in [
            "--collect",
            "--property=KillMode=process",
            "--setenv=XDG_RUNTIME_DIR=/run/user/1000",
            "--setenv=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus",
            "--setenv=DISPLAY=:0",
            "--setenv=WAYLAND_DISPLAY=wayland-0",
        ] {
            assert!(root.contains(&want.to_string()), "{want} missing: {root:?}");
        }
        assert_eq!(
            &root[root.len() - 2..],
            ["/usr/bin/roomler-desktop", "--first-run"],
            "the program, then its own arguments, last"
        );
        let user = argv(true);
        assert_eq!(user[0], "--user");
        assert!(!user.iter().any(|a| a.starts_with("--uid=")), "{user:?}");
        assert!(user.contains(&"--setenv=DISPLAY=:0".to_string()));
        // #2729 — under either manager, a browser the companion opened is not
        // killed with it.
        assert!(
            user.contains(&"--property=KillMode=process".to_string()),
            "{user:?}"
        );
    }

    /// #2713 — REAL `systemctl --user show-environment` output, captured on
    /// 2026-10-22 from the lab's Fedora Asahi GNOME box (its `HOSTNAME` line
    /// left out): the box where View screen, in a companion the daemon had
    /// started, opened a text editor.
    /// The allowlist keeps the desktop's identity, its data dirs (with the
    /// user's own Flatpak exports), X auth, the locale and the input method,
    /// and drops the rest, every `$'…'`-escaped value included.
    #[test]
    fn the_session_environment_keeps_the_allowlist_only() {
        const REAL: &str = "\
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1001/bus
HOME=/home/wlab
LANG=C.UTF-8
LOGNAME=wlab
PATH=/home/wlab/.cargo/bin:/home/wlab/.local/bin:/home/wlab/bin:/usr/local/bin:/usr/bin:/usr/local/sbin
SHELL=/bin/bash
USER=wlab
XDG_DATA_DIRS=/home/wlab/.local/share/flatpak/exports/share:/var/lib/flatpak/exports/share:/usr/local/share/:/usr/share/
XDG_RUNTIME_DIR=/run/user/1001
DEBUGINFOD_IMA_CERT_PATH=/etc/keys/ima:
DEBUGINFOD_URLS=$'https://debuginfod.fedoraproject.org/ '
DESKTOP_SESSION=gnome
DISPLAY=:0
EDITOR=/usr/bin/nano
GDMSESSION=gnome
GDM_LANG=C.UTF-8
GNOME_SETUP_DISPLAY=:1
GPG_TTY=$'not a tty'
HISTCONTROL=ignoredups
HISTSIZE=1000
KDEDIRS=/usr
LESSOPEN=$'||/usr/bin/lesspipe.sh %s'
MAIL=/var/spool/mail/wlab
PWD=/home/wlab
QT_IM_MODULE=ibus
SESSION_MANAGER=local/unix:@/tmp/.ICE-unix/2160711,unix/unix:/tmp/.ICE-unix/2160711
SHLVL=0
SSH_ASKPASS=/usr/bin/ksshaskpass
SSH_AUTH_SOCK=/run/user/1001/keyring/ssh
USERNAME=wlab
WAYLAND_DISPLAY=wayland-0
XAUTHORITY=/run/user/1001/.mutter-Xwaylandauth.XEYMX3
XDG_CURRENT_DESKTOP=GNOME
XDG_MENU_PREFIX=gnome-
XDG_SESSION_CLASS=user
XDG_SESSION_DESKTOP=gnome
XDG_SESSION_TYPE=wayland
XMODIFIERS=@im=ibus
";
        let pairs = super::session_env_pairs(REAL);
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "LANG",
                "XDG_DATA_DIRS",
                "DESKTOP_SESSION",
                "QT_IM_MODULE",
                "XAUTHORITY",
                "XDG_CURRENT_DESKTOP",
                "XDG_MENU_PREFIX",
                "XDG_SESSION_DESKTOP",
                "XDG_SESSION_TYPE",
                "XMODIFIERS",
            ]
        );
        let value = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(
            value("XDG_DATA_DIRS"),
            Some(
                "/home/wlab/.local/share/flatpak/exports/share:/var/lib/flatpak/exports/share:\
                 /usr/local/share/:/usr/share/"
            ),
            "the user's own Flatpak exports, which the companion's old unit lacked"
        );
        assert_eq!(
            value("XMODIFIERS"),
            Some("@im=ibus"),
            "a value keeps its own '='"
        );

        // Every LC_* is kept; an odd LC_ name, an escaped value and an empty
        // one are not.
        assert_eq!(
            super::session_env_pairs(
                "LC_TIME=de_AT.UTF-8\nLC_=x\nLC_time=x\nLANGUAGE=$'de\\x1b'\nBROWSER=\n"
            ),
            [("LC_TIME".to_string(), "de_AT.UTF-8".to_string())]
        );
    }

    /// #2713 — a root daemon's companion carries the session's pairs as
    /// `--setenv`, after logind's and BEFORE the program, so systemd-run
    /// reads them as its own options rather than as the companion's
    /// arguments.
    #[test]
    fn a_root_daemons_companion_carries_the_session_environment() {
        let sess = super::GraphicalSession {
            uid: 1000,
            name: "ana".into(),
            display: None,
            wayland_display: Some("wayland-0".into()),
        };
        let env = [
            ("XDG_CURRENT_DESKTOP".to_string(), "GNOME".to_string()),
            (
                "XDG_DATA_DIRS".to_string(),
                "/home/ana/.local/share/flatpak/exports/share:/usr/share/".to_string(),
            ),
        ];
        let cmd = super::systemd_run_command_with(
            &sess,
            std::path::Path::new("/usr/bin/roomler-desktop"),
            &[],
            false,
            &env,
        );
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = |a: &str| {
            argv.iter()
                .position(|x| x == a)
                .unwrap_or_else(|| panic!("{a} missing: {argv:?}"))
        };
        let program = at("/usr/bin/roomler-desktop");
        assert_eq!(program, argv.len() - 1, "the program last: {argv:?}");
        assert!(
            at("--setenv=WAYLAND_DISPLAY=wayland-0") < at("--setenv=XDG_CURRENT_DESKTOP=GNOME")
        );
        assert!(at("--setenv=XDG_CURRENT_DESKTOP=GNOME") < program);
        assert!(
            at("--setenv=XDG_DATA_DIRS=/home/ana/.local/share/flatpak/exports/share:/usr/share/")
                < program
        );
    }

    /// Both samples are REAL `loginctl list-sessions --no-legend --no-pager`
    /// output, captured from fleet hosts on 2026-08-31. They are here because
    /// the column layout genuinely differs between the two releases, and the
    /// previous code's failure was invisible: it asked for a `show-*` output
    /// mode that `list-sessions` rejects, got an empty list, and reported
    /// "nobody is at this machine's screen" on a host with a logged-in user.
    /// A test over invented output would have passed just as happily.
    #[test]
    fn session_ids_survive_both_observed_column_layouts() {
        // systemd 255 (Ubuntu 24.04): SESSION UID USER SEAT TTY STATE IDLE SINCE
        let s255 = "7173 1000 gjovanov - -     active no  -\n\
                    7631 1000 gjovanov - pts/5 active yes 17h ago\n\
                    7767 1000 gjovanov - -     active no  -\n";
        assert_eq!(session_ids(s255), ["7173", "7631", "7767"]);

        // systemd 257 (Fedora 42): a LEADER and a CLASS column appear, so
        // every positional field after the first one shifts.
        let s257 = "1 1000 m1 -     1447   manager - no -\n\
                    3 1000 m1 seat0 101848 user    - no -\n\
                    5 1000 m1 -     434840 user    - no -\n";
        assert_eq!(session_ids(s257), ["1", "3", "5"]);
    }

    /// A headless host lists nothing, and blank lines must not become empty
    /// ids that then get handed to `show-session`.
    #[test]
    fn empty_and_blank_output_yields_no_ids() {
        assert!(session_ids("").is_empty());
        assert!(session_ids("\n   \n\t\n").is_empty());
    }

    /// Greeter sessions are `c1`, `c2`… — ids are strings, and a numeric
    /// parse would silently drop exactly the session a lock-screen prompt
    /// needs to find.
    #[test]
    fn non_numeric_greeter_ids_are_kept() {
        assert_eq!(session_ids("c1 42 gdm seat0 - active no -\n"), ["c1"]);
    }
}

/// #3688 — seat0's own "nobody" told from a lookup that failed, on every OS,
/// as `console_tests` are. At the end of the file, so no line above moves.
#[cfg(test)]
mod nobody_at_the_screen_tests {
    use super::{is_nobody_at_the_screen, seat0_active_session, session_account};

    /// The states that really are nobody at the screen say so by their type:
    /// an empty seat, the login screen, a session that is not a person's. A
    /// session that names no account, and a lookup that failed, are not
    /// nobody: who is there is not known, and a viewer is told that.
    #[test]
    fn nobody_at_the_screen_is_told_from_a_lookup_that_failed() {
        let empty = "Id=seat0\nActiveSession=\nCanGraphical=no\nSessions=\n";
        let greeter = "Id=c1\nName=gdm\nUser=120\nSeat=seat0\nType=wayland\nClass=greeter\n";
        let lock = "Id=9\nName=x\nUser=1000\nType=x11\nClass=lock-screen\n";
        let unnamed = "Id=3\nUser=1000\nType=x11\nClass=user\n";
        assert!(
            is_nobody_at_the_screen(&seat0_active_session(empty).unwrap_err()),
            "an empty seat"
        );
        assert!(
            is_nobody_at_the_screen(&session_account("c1", greeter).unwrap_err()),
            "the login screen"
        );
        assert!(
            is_nobody_at_the_screen(&session_account("9", lock).unwrap_err()),
            "a session that is not a person's"
        );
        assert!(
            !is_nobody_at_the_screen(&session_account("3", unnamed).unwrap_err()),
            "a session that names no account"
        );
        let failed = anyhow::anyhow!("loginctl show-seat seat0 failed: Failed to connect to bus");
        assert!(!is_nobody_at_the_screen(&failed), "a lookup that failed");
    }

    /// A host with no logind at all has no seat, so nobody is at its screen,
    /// and its lookup says so by its type: `loginctl` is not installed, or
    /// systemd is not what booted the host (a container, WSL without systemd,
    /// a minimal VM). Every other failure of the lookup is not nobody: a bus
    /// or a logind that does not answer, a `loginctl` that would not start for
    /// another reason. Who is there is then not known.
    #[test]
    fn a_host_with_no_logind_has_nobody_at_the_screen() {
        use super::{loginctl_did_not_start, loginctl_failed};
        let seat = ["show-seat", "seat0"];
        let not_installed = loginctl_did_not_start(std::io::ErrorKind::NotFound.into());
        assert!(
            is_nobody_at_the_screen(&not_installed),
            "loginctl is not installed: {not_installed:#}"
        );
        let not_booted = loginctl_failed(
            &seat,
            "System has not been booted with systemd as init system (PID 1). Can't operate.\n\
             Failed to connect to bus: Host is down\n",
        );
        assert!(
            is_nobody_at_the_screen(&not_booted),
            "systemd did not boot this host: {not_booted:#}"
        );
        for stderr in [
            "Failed to connect to bus: Connection refused",
            "Failed to get properties: Connection timed out",
            "",
        ] {
            let failed = loginctl_failed(&seat, stderr);
            assert!(!is_nobody_at_the_screen(&failed), "{stderr:?}: {failed:#}");
        }
        let would_not_start = loginctl_did_not_start(std::io::ErrorKind::PermissionDenied.into());
        assert!(
            !is_nobody_at_the_screen(&would_not_start),
            "loginctl would not start: {would_not_start:#}"
        );
    }
}

/// #3781 — the retry schedule, keyed on the companion's version marker, the
/// loop's rhythm on tokio's paused clock, and what `roomler status` is told:
/// pure, on every OS.
#[cfg(test)]
mod too_old_tests {
    use super::{
        MIN_COMPANION_VERSION, NextRefresh, REFRESH_RETRY, RefreshSchedule, next_refresh_wait,
        retry_off_where_it_runs, retry_refresh, sync_enabled_in, too_old_to_connect,
    };
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tunnel_core::localapi::CompanionTooOld;

    const MINUTE: Duration = Duration::from_secs(60);

    /// The first eight waits the loop would take for `marker`, in minutes.
    fn waits(marker: Option<&str>) -> Vec<u64> {
        (0..8)
            .map(|tries| next_refresh_wait(marker, tries).as_secs() / 60)
            .collect()
    }

    /// A tray older than the minimum is refreshed again after 2, 5, 10 and 15
    /// minutes, then every 15 to the end of the first hour, then every 4 h.
    #[test]
    fn a_tray_too_old_to_connect_is_retried_within_minutes_for_its_first_hour() {
        assert!(crate::updater::parse_version(MIN_COMPANION_VERSION).is_some());
        assert_eq!(REFRESH_RETRY, crate::updater::CHECK_INTERVAL);
        assert_eq!(waits(Some("0.4.212")), [2, 5, 10, 15, 15, 15, 240, 240]);
        assert_eq!(waits(Some("0.4.215\r\n")), [2, 5, 10, 15, 15, 15, 240, 240]);
        let first_hour: u64 = waits(Some("0.4.212"))[..6].iter().sum();
        assert_eq!(
            first_hour, 62,
            "the sixth try lands at the end of the first hour"
        );
    }

    /// Nothing vouches for the open a tray with no readable version makes.
    #[test]
    fn no_marker_or_one_that_names_no_version_counts_as_too_old() {
        for marker in [None, Some(""), Some("  \r\n"), Some("garbage"), Some("v")] {
            assert_eq!(waits(marker)[..2], [2, 5], "{marker:?}");
        }
    }

    /// From the minimum on the tray connects, and the period stays the
    /// updater's. A pre-release of the minimum predates its final.
    #[test]
    fn a_tray_from_the_minimum_on_keeps_the_normal_period() {
        for marker in [
            Some(MIN_COMPANION_VERSION),
            Some("0.4.216\n"),
            Some("0.4.217"),
            Some("0.5.0"),
            Some("1.0.0"),
        ] {
            assert_eq!(waits(marker), [240; 8], "{marker:?}");
        }
        assert_eq!(waits(Some("0.4.216-rc.1"))[0], 2);
    }

    /// The fast waits count refreshes made in a row while the tray read too
    /// old: one that reads current starts them over.
    #[test]
    fn the_schedule_starts_over_when_the_tray_reads_current_again() {
        let mut s = RefreshSchedule::default();
        let minutes = |n: &NextRefresh| n.wait.as_secs() / 60;
        let first = s.next(Some("0.4.212"), false);
        assert_eq!(
            (minutes(&first), first.too_old, first.installed.as_deref()),
            (2, true, Some("0.4.212"))
        );
        assert_eq!(minutes(&s.next(Some("0.4.212"), false)), 5);
        let current = s.next(Some("0.4.218"), false);
        assert_eq!((minutes(&current), current.too_old), (240, false));
        let again = s.next(None, false);
        assert_eq!(
            (minutes(&again), again.too_old, again.installed),
            (2, true, None)
        );
    }

    /// #3824: while a tray this process stopped is not running again, the
    /// next refresh comes after 1, 5 and 15 minutes, then on the schedule.
    /// Without one, the waits are today's.
    #[test]
    fn a_lost_tray_is_tried_again_after_1_5_and_15_minutes_then_on_the_schedule() {
        let waits = |marker: Option<&str>, lost: &[bool]| -> Vec<u64> {
            let mut s = RefreshSchedule::default();
            lost.iter()
                .map(|&lost| s.next(marker, lost).wait.as_secs() / 60)
                .collect()
        };
        let current = Some("0.4.221");
        assert_eq!(waits(current, &[true; 5]), [1, 5, 15, 240, 240]);
        assert_eq!(waits(current, &[false; 5]), [240; 5], "today's period");
        // Too old to connect as well: the sooner of the two schedules.
        assert_eq!(
            waits(Some("0.4.212"), &[true; 8]),
            [1, 5, 10, 15, 15, 15, 240, 240]
        );
        assert_eq!(
            waits(Some("0.4.212"), &[false; 8]),
            [2, 5, 10, 15, 15, 15, 240, 240],
            "today's fast waits"
        );
        // Back, then lost again: the waits start over.
        assert_eq!(waits(current, &[true, true, false, true]), [1, 5, 240, 1]);
    }

    /// #3824: the loop tries a lost tray within a minute, then 5 and 15
    /// minutes later, then on the normal period.
    #[tokio::test(start_paused = true)]
    async fn the_loop_tries_a_lost_tray_within_a_minute_then_at_5_and_15() {
        let refreshes = Arc::new(AtomicU32::new(0));
        let task = tokio::spawn(retry_refresh(
            || true,
            || Some("0.4.221".to_string()),
            || true,
            {
                let refreshes = refreshes.clone();
                move || {
                    refreshes.fetch_add(1, SeqCst);
                    std::future::ready(())
                }
            },
            |_: &NextRefresh| {},
        ));
        let tries = || refreshes.load(SeqCst);
        tokio::time::sleep(MINUTE + Duration::from_secs(1)).await;
        assert_eq!(tries(), 1, "within a minute");
        tokio::time::sleep(5 * MINUTE).await;
        assert_eq!(tries(), 2, "5 minutes later");
        tokio::time::sleep(15 * MINUTE).await;
        assert_eq!(tries(), 3, "15 minutes after that");
        tokio::time::sleep(239 * MINUTE).await;
        assert_eq!(tries(), 3, "then the normal period");
        tokio::time::sleep(2 * MINUTE).await;
        assert_eq!(tries(), 4);
        task.abort();
    }

    /// The pin for the log: ONE line per wait while the tray is too old, each
    /// before its wait and naming the version and the wait; never one per
    /// tick, and none once the tray reads current.
    #[tokio::test(start_paused = true)]
    async fn the_too_old_line_is_logged_once_per_wait() {
        let marker = Arc::new(Mutex::new(Some("0.4.212".to_string())));
        let refreshes = Arc::new(AtomicU32::new(0));
        let lines: Arc<Mutex<Vec<NextRefresh>>> = Arc::default();
        let task = tokio::spawn(retry_refresh(
            || true,
            {
                let marker = marker.clone();
                move || marker.lock().unwrap().clone()
            },
            || false,
            {
                let refreshes = refreshes.clone();
                move || {
                    refreshes.fetch_add(1, SeqCst);
                    std::future::ready(())
                }
            },
            {
                let lines = lines.clone();
                move |next: &NextRefresh| lines.lock().unwrap().push(next.clone())
            },
        ));
        let seen = || (lines.lock().unwrap().len(), refreshes.load(SeqCst));
        let last = || {
            let lines = lines.lock().unwrap();
            lines
                .last()
                .map(|n| (n.wait.as_secs() / 60, n.installed.clone()))
        };

        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(seen(), (1, 0), "the first wait's line, before it");
        tokio::time::sleep(MINUTE).await;
        assert_eq!(seen(), (1, 0), "a minute into a wait logs nothing more");
        tokio::time::sleep(MINUTE).await;
        assert_eq!(seen(), (2, 1), "a refresh, then the next wait's line");
        tokio::time::sleep(60 * MINUTE).await;
        assert_eq!(seen(), (7, 6), "62 minutes: six tries, seven waits");
        let logged: Vec<u64> = lines
            .lock()
            .unwrap()
            .iter()
            .map(|n| n.wait.as_secs() / 60)
            .collect();
        assert_eq!(logged, [2, 5, 10, 15, 15, 15, 240]);
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .all(|n| n.too_old && n.installed.as_deref() == Some("0.4.212"))
        );
        tokio::time::sleep(120 * MINUTE).await;
        assert_eq!(seen(), (7, 6), "two hours into the 4-h wait: nothing");

        // The refresh lands: the tray reads current, and the lines stop.
        *marker.lock().unwrap() = Some("0.4.218".into());
        tokio::time::sleep(120 * MINUTE).await;
        assert_eq!(seen(), (7, 7));
        // A tray that reads too old again starts the fast waits over.
        *marker.lock().unwrap() = None;
        tokio::time::sleep(240 * MINUTE).await;
        assert_eq!(seen(), (8, 8));
        assert_eq!(last(), Some((2, None)));
        task.abort();
    }

    /// `ROOMLERD_COMPANION_SYNC=0` turns the whole retry off, the faster ones
    /// with it, a lost tray's (#3824) too: no refresh, and no line saying one
    /// is coming.
    #[tokio::test(start_paused = true)]
    async fn with_the_switch_off_nothing_is_refreshed_or_logged() {
        let refreshes = Arc::new(AtomicU32::new(0));
        let lines = Arc::new(AtomicU32::new(0));
        let task = tokio::spawn(retry_refresh(
            || false,
            || Some("0.4.212".to_string()),
            || true,
            {
                let refreshes = refreshes.clone();
                move || {
                    refreshes.fetch_add(1, SeqCst);
                    std::future::ready(())
                }
            },
            {
                let lines = lines.clone();
                move |_: &NextRefresh| {
                    lines.fetch_add(1, SeqCst);
                }
            },
        ));
        tokio::time::sleep(9 * 60 * MINUTE).await;
        assert_eq!((refreshes.load(SeqCst), lines.load(SeqCst)), (0, 0));
        task.abort();
    }

    /// A tray older than the minimum, where this daemon's pipe refuses it, is
    /// reported with its version, the minimum, and who refreshes it.
    #[test]
    fn a_tray_too_old_to_connect_is_reported_where_the_pipe_refuses_it() {
        assert_eq!(
            too_old_to_connect(true, true, Some("0.4.212\r\n"), true),
            Some(CompanionTooOld {
                installed: Some("0.4.212".into()),
                minimum: "0.4.216".into(),
                by_service: true,
                retry_off: false,
            })
        );
        let own = too_old_to_connect(true, true, Some("0.4.215"), false).expect("reported");
        assert!(!own.by_service);
    }

    /// Nothing for a tray that connects, for no tray, and for a daemon that
    /// runs as the person: it owns its pipe, and their old tray still
    /// connects through the owner's entry.
    #[test]
    fn nothing_is_reported_for_a_tray_that_connects_or_is_not_there() {
        assert_eq!(too_old_to_connect(true, true, Some("0.4.216"), true), None);
        assert_eq!(
            too_old_to_connect(true, true, Some("0.4.220\n"), true),
            None
        );
        assert_eq!(too_old_to_connect(false, true, Some("0.4.212"), true), None);
        assert_eq!(too_old_to_connect(false, true, None, true), None);
        assert_eq!(
            too_old_to_connect(true, false, Some("0.4.212"), false),
            None
        );
        assert_eq!(too_old_to_connect(true, false, None, false), None);
    }

    /// A tray whose version nobody can vouch for is reported, never hidden,
    /// and never with what a marker says when it names no version.
    #[test]
    fn a_tray_of_unknown_version_is_reported_without_one() {
        for marker in [None, Some(""), Some("garbage"), Some("<script>")] {
            let reported = too_old_to_connect(true, true, marker, true).expect("reported");
            assert_eq!(reported.installed, None, "{marker:?}");
            assert_eq!(reported.minimum, MIN_COMPANION_VERSION);
        }
    }

    /// #3814 — `status` reads the switch where the retry runs. A daemon that
    /// runs the retry itself reads its own, and so does the SystemContext
    /// worker: the SCM host starts it with the host's own environment block.
    /// A worker the host starts as the person gets the person's block, which
    /// leaves the service's `Environment` value out, so it reads that value,
    /// never its own. So does one whose token cannot be read.
    #[test]
    fn the_switch_is_read_where_the_retry_runs() {
        let service = || -> bool { panic!("the service's value is read") };
        let own = || -> bool { panic!("its own environment is read") };
        for local_system in [Some(true), Some(false), None] {
            assert!(
                retry_off_where_it_runs(false, local_system, || false, service),
                "{local_system:?}"
            );
            assert!(!retry_off_where_it_runs(
                false,
                local_system,
                || true,
                service
            ));
        }
        assert!(retry_off_where_it_runs(true, Some(true), || false, service));
        assert!(!retry_off_where_it_runs(true, Some(true), || true, service));
        for local_system in [Some(false), None] {
            assert!(
                retry_off_where_it_runs(true, local_system, own, || false),
                "{local_system:?}"
            );
            assert!(!retry_off_where_it_runs(true, local_system, own, || true));
        }
    }

    /// #3814 — for that worker, the service's `Environment` value says it, in
    /// `node_env`'s prefix order and read as `sync_enabled` reads the switch.
    /// Windows names an environment variable in any case.
    #[test]
    fn the_services_environment_says_whether_its_retry_is_off() {
        let on = |entries: &[(&str, &str)]| {
            let env: Vec<(String, String)> = entries
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect();
            sync_enabled_in(Some(&env))
        };
        for value in ["0", "off", "No", " false "] {
            assert!(
                !on(&[("Path", "x"), ("ROOMLERD_COMPANION_SYNC", value)]),
                "{value:?}"
            );
        }
        assert!(!on(&[("roomlerd_companion_sync", "0")]));
        assert!(!on(&[("ROOMLER_NODE_COMPANION_SYNC", "0")]));
        assert!(on(&[
            ("ROOMLERD_COMPANION_SYNC", "1"),
            ("ROOMLER_NODE_COMPANION_SYNC", "0"),
        ]));
        for value in ["1", "", "garbage"] {
            assert!(on(&[("ROOMLERD_COMPANION_SYNC", value)]), "{value:?}");
        }
        // A retired spelling is never read, and a value that names no switch
        // leaves the retry on.
        for retired in tunnel_core::env::RETIRED_PREFIXES {
            let name = format!("{retired}COMPANION_SYNC");
            assert!(on(&[(name.as_str(), "0")]), "{name}");
        }
        assert!(on(&[("ROOMLERD_ENABLE_SYSTEM_SWAP", "1")]));
        assert!(on(&[]));
    }

    /// #3814 — a service `Environment` value that cannot be read (no key, no
    /// access, not a REG_MULTI_SZ) is today's answer: the retry is on, and
    /// `status` says it is retried. It never says the retry is off on a guess.
    #[test]
    fn a_service_environment_that_cannot_be_read_leaves_the_retry_on() {
        assert!(sync_enabled_in(None));
        for local_system in [Some(false), None] {
            assert!(!retry_off_where_it_runs(
                true,
                local_system,
                || false,
                || sync_enabled_in(None)
            ));
        }
    }
}

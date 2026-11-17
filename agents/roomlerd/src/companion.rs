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
//! written on every successful swap. When it does not name the daemon's
//! version (missing, unreadable, or one a swap could not write), the EXE's
//! own version resource decides (#3850): an EXE already at the daemon's
//! version is not swapped again, and its marker is written again. So a
//! wizard-placed EXE with no marker is swapped only when it is another
//! version.
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
///
/// #3850 — "too old" is read from the companion's version as this daemon can
/// know it ([`sibling_version`]): the EXE's own when the marker does not name
/// the daemon's version.
#[cfg(target_os = "windows")]
pub async fn refresh_periodically(respawn: RespawnContext) {
    retry_refresh(
        sync_enabled,
        sibling_version,
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

/// #3850 — the companion's version as this daemon can know it: what its
/// marker names when that is this daemon's own version (one small file read,
/// the common case), else what the EXE states in its own version resource
/// (`exe_version`), else what the marker names. A marker an earlier swap could
/// not write is then no reason to swap again, or to count the tray too old to
/// connect.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn known_version(
    own: &str,
    marker: Option<&str>,
    exe_version: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let recorded = marker.and_then(sanitize_version);
    if recorded.as_deref() == Some(own) {
        return recorded;
    }
    exe_version().or(recorded)
}

/// #3850 — what a refresh pass finds beside the daemon, before any download.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Installed {
    /// This daemon's version: by its marker, or, when the marker is missing,
    /// unreadable or names another version, by the EXE's own version
    /// resource (`by_its_version`). Nothing to swap; in the second case the
    /// marker is written again.
    Current { by_its_version: bool },
    /// Not there, or another version by both: the refresh swaps it.
    Stale {
        /// What the marker names.
        recorded: Option<String>,
        /// What the EXE's version resource states, when it was read.
        its_version: Option<String>,
    },
}

/// #3850 — [`Installed`] for the companion beside the daemon: whether its EXE
/// is `present`, what its `marker` reads, and what the EXE states, read only
/// when the marker does not name `own`. A version that cannot be read is not
/// known, which is stale, as before: the refresh swaps.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn installed_state(
    own: &str,
    present: bool,
    marker: Option<&str>,
    exe_version: impl FnOnce() -> Option<String>,
) -> Installed {
    let recorded = marker.and_then(sanitize_version);
    if !present {
        return Installed::Stale {
            recorded,
            its_version: None,
        };
    }
    if recorded.as_deref() == Some(own) {
        return Installed::Current {
            by_its_version: false,
        };
    }
    let its_version = exe_version();
    if its_version.as_deref() == Some(own) {
        return Installed::Current {
            by_its_version: true,
        };
    }
    Installed::Stale {
        recorded,
        its_version,
    }
}

/// #3850 — whether this process's last write of the version marker failed, so
/// a marker that cannot be written is said once at WARN, then at DEBUG, until
/// a write works again. One for the process ([`MARKER_WRITES`]): the swap's
/// write and the passes after it share it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Default)]
pub(crate) struct MarkerWrites {
    failing: std::sync::atomic::AtomicBool,
}

/// #3850 — the line for a write of the version marker.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MarkerLine {
    /// `could not write desktop version marker; …`, with `error=`: WARN the
    /// first time, DEBUG while it keeps failing.
    NotWritten { error: String, first: bool },
    /// INFO `desktop version marker written again`: a write that worked after
    /// one that did not.
    WrittenAgain,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl MarkerWrites {
    pub(crate) const fn new() -> Self {
        Self {
            failing: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The line for a write that came to `wrote`; `None` for one that worked,
    /// as the last one did.
    pub(crate) fn wrote(&self, wrote: std::result::Result<(), String>) -> Option<MarkerLine> {
        use std::sync::atomic::Ordering::SeqCst;
        match wrote {
            Ok(()) => self
                .failing
                .swap(false, SeqCst)
                .then_some(MarkerLine::WrittenAgain),
            Err(error) => Some(MarkerLine::NotWritten {
                error,
                first: !self.failing.swap(true, SeqCst),
            }),
        }
    }
}

/// This process's [`MarkerWrites`].
#[cfg(target_os = "windows")]
static MARKER_WRITES: MarkerWrites = MarkerWrites::new();

/// #3850 — the version a `VS_VERSIONINFO` resource states: its
/// `ProductVersion` string, else its `FileVersion`, from the first string
/// table that has one, up to its first space and bounded as a marker is
/// ([`sanitize_version`]). Every offset and length is checked against the
/// block it sits in, and anything malformed is `None`: not known, which the
/// refresh reads as it always did, stale, so it swaps. Never a guess: a
/// version is read only from a resource whose every block it walks parsed.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn version_resource_version(res: &[u8]) -> Option<String> {
    let root = ver_block(res, 0, res.len())?;
    if root.key != "VS_VERSION_INFO" {
        return None;
    }
    let (mut product, mut file) = (None, None);
    for info in ver_children(res, &root)? {
        if info.key != "StringFileInfo" {
            continue;
        }
        for table in ver_children(res, &info)? {
            for string in ver_children(res, &table)? {
                let slot = match string.key.as_str() {
                    "ProductVersion" => &mut product,
                    "FileVersion" => &mut file,
                    _ => continue,
                };
                if slot.is_none() {
                    *slot = ver_text(res.get(string.value.clone())?);
                }
            }
        }
    }
    product.or(file).as_deref().and_then(sanitize_version)
}

/// #3850 — one block of a version resource: its key, and the byte ranges of
/// its value and of its children, each inside the block.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug)]
struct VerBlock {
    key: String,
    value: std::ops::Range<usize>,
    children: std::ops::Range<usize>,
    /// Where the next block may start: past this one, on a 32-bit boundary.
    next: usize,
}

/// #3850 — the block at `at`, which must end by `end`: its length, the
/// length of its value (in words for text, in bytes for binary), its type,
/// its key up to a terminator inside the block, then its value and its
/// children, each on a 32-bit boundary. `None` for anything that does not
/// fit where it must.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn ver_block(res: &[u8], at: usize, end: usize) -> Option<VerBlock> {
    let word = |at: usize| -> Option<usize> {
        let bytes = res.get(at..at.checked_add(2)?)?;
        Some(usize::from(u16::from_le_bytes([bytes[0], bytes[1]])))
    };
    let len = word(at)?;
    let block_end = at.checked_add(len)?;
    // Its three header words and a key's terminator, at least.
    if len < 8 || block_end > end || block_end > res.len() {
        return None;
    }
    let value_len = word(at.checked_add(2)?)?;
    let text = match word(at.checked_add(4)?)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let mut units = Vec::new();
    let mut p = at.checked_add(6)?;
    loop {
        if p.checked_add(2)? > block_end {
            return None;
        }
        let unit = word(p)?;
        p += 2;
        if unit == 0 {
            break;
        }
        units.push(u16::try_from(unit).ok()?);
    }
    let key = String::from_utf16(&units).ok()?;
    let value_start = align4(p)?.min(block_end);
    let value_bytes = if text {
        value_len.checked_mul(2)?
    } else {
        value_len
    };
    let value_end = value_start.checked_add(value_bytes)?;
    if value_end > block_end {
        return None;
    }
    Some(VerBlock {
        key,
        value: value_start..value_end,
        children: align4(value_end)?.min(block_end)..block_end,
        next: align4(block_end)?,
    })
}

/// #3850 — the blocks that make up `parent`'s children, every one of which
/// must parse.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn ver_children(res: &[u8], parent: &VerBlock) -> Option<Vec<VerBlock>> {
    let mut children = Vec::new();
    let mut at = parent.children.start;
    while at < parent.children.end {
        let child = ver_block(res, at, parent.children.end)?;
        at = child.next;
        children.push(child);
    }
    Some(children)
}

/// The next 32-bit boundary at or after `at`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn align4(at: usize) -> Option<usize> {
    Some(at.checked_add(3)? & !3)
}

/// #3850 — a string value: UTF-16 up to its terminator, up to its first
/// space (`10.0.26100.1 (WinBuild…)` states `10.0.26100.1`); `None` when it
/// is empty or not UTF-16.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn ver_text(value: &[u8]) -> Option<String> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = value
        .chunks_exact(2)
        .map(|w| u16::from_le_bytes([w[0], w[1]]))
        .take_while(|&unit| unit != 0)
        .collect();
    let text = String::from_utf16(&units).ok()?;
    text.split_whitespace().next().map(str::to_string)
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
///
/// #3850 — `marker` answers the companion's version as the daemon can know it
/// ([`known_version`]), and is awaited: the read runs off the async worker.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) async fn retry_refresh<E, M, MF, K, R, F, L>(
    mut enabled: E,
    mut marker: M,
    mut lost: K,
    mut refresh: R,
    mut log: L,
) where
    E: FnMut() -> bool,
    M: FnMut() -> MF,
    MF: std::future::Future<Output = Option<String>>,
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
            let version = marker().await;
            schedule.next(version.as_deref(), lost())
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
///
/// #3908 — the tray's version is the one the refresh goes by
/// ([`known_beside`]): the EXE's own when the marker does not name this
/// daemon's.
#[cfg(target_os = "windows")]
pub(crate) fn too_old_here() -> Option<tunnel_core::localapi::CompanionTooOld> {
    if !pipe_refuses_old_companions() {
        return None;
    }
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf));
    let by_service =
        crate::supervision::current() == Some(crate::supervision::Supervision::WindowsScm);
    too_old_beside(
        dir.as_deref(),
        env!("CARGO_PKG_VERSION"),
        by_service,
        &EXE_VERSIONS,
    )
    .map(|reported| tunnel_core::localapi::CompanionTooOld {
        retry_off: retry_off_where_it_runs(
            by_service,
            crate::win_identity::own_token_is_local_system(),
            sync_enabled,
            service_sync_enabled,
        ),
        ..reported
    })
}

/// #3908 — [`too_old_to_connect`] for the companion in `dir`, by the version
/// [`known_beside`] knows for it now. `None` for `dir`: no folder of its own
/// to look in, so a tray of unknown version. With no EXE there is no tray to
/// refuse, and nothing is read.
#[cfg(target_os = "windows")]
fn too_old_beside(
    dir: Option<&std::path::Path>,
    own: &str,
    by_service: bool,
    versions: &'static ExeVersions,
) -> Option<tunnel_core::localapi::CompanionTooOld> {
    let (exe_present, version) = match dir {
        Some(dir) => {
            let present = dir.join(DESKTOP_EXE).try_exists().unwrap_or(true);
            let version = present
                .then(|| known_beside(dir, own, versions).version)
                .flatten();
            (present, version)
        }
        None => (true, None),
    };
    too_old_to_connect(exe_present, true, version.as_deref(), by_service)
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

/// #3850 — how long a refresh waits for what it reads beside the daemon (the
/// marker, and the EXE's version resource) on a blocking thread. Both are
/// small reads of local files that take milliseconds; a filter driver that
/// holds them past this skips the pass, and the retry reads again.
#[cfg(target_os = "windows")]
const LOOK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// #3850 — the companion's version as the retry loop reads it
/// ([`known_version`]), off the async worker, [`LOOK_WAIT`] at most: `None`,
/// not known, when it cannot be read in time.
#[cfg(target_os = "windows")]
async fn sibling_version() -> Option<String> {
    let read = tokio::task::spawn_blocking(|| {
        let marker = sibling_marker();
        let exe = sibling_desktop();
        known_version(env!("CARGO_PKG_VERSION"), marker.as_deref(), || {
            exe.as_deref().and_then(exe_version)
        })
    });
    tokio::time::timeout(LOOK_WAIT, read).await.ok()?.ok()?
}

/// #3908 — what the companion EXE beside this daemon states in its own
/// version resource, for the readers that may not wait for that read:
/// `roomler status` ([`too_old_here`]) and the heartbeat
/// ([`installed_version`]). One for the process, shared by both.
#[cfg(target_os = "windows")]
static EXE_VERSIONS: ExeVersions = ExeVersions::new("companion-ver", exe_version);

/// #3908 — the EXE's version as its last read stated it, for the files that
/// read was made for ([`Stamp`]), and the read itself ([`exe_version`] in the
/// daemon): on a thread of its own, one at a time
/// ([`crate::one_in_flight::OneInFlight`]), waited for by nobody. A read that
/// hangs (a filter driver holding the EXE) holds that one thread however
/// often the readers ask, and they answer from the marker meanwhile.
#[cfg(target_os = "windows")]
struct ExeVersions {
    read: fn(&std::path::Path) -> Option<String>,
    /// The last read's answer and the files it was made for. Locked only to
    /// look at it or to replace it, never across a read.
    last: std::sync::Mutex<Option<(Stamp, Option<String>)>>,
    reads: crate::one_in_flight::OneInFlight<()>,
}

#[cfg(target_os = "windows")]
impl ExeVersions {
    const fn new(thread_name: &'static str, read: fn(&std::path::Path) -> Option<String>) -> Self {
        Self {
            read,
            last: std::sync::Mutex::new(None),
            reads: crate::one_in_flight::OneInFlight::new(thread_name),
        }
    }

    /// What the EXE in `dir` states, as read for the files there now, and
    /// never a wait: `None` when no read was made for them yet. The read then
    /// starts, unless one is running, and its answer is kept as it ends.
    fn now(&'static self, dir: &std::path::Path) -> Option<Option<String>> {
        let stamp = Stamp::of(dir);
        let last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some((read_for, stated)) = last
            && read_for == stamp
        {
            return Some(stated);
        }
        let (read, exe) = (self.read, dir.join(DESKTOP_EXE));
        self.reads.start_unless_running(move || {
            let stated = read(&exe);
            *self
                .last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((stamp, stated));
        });
        None
    }

    /// Once the read running now, if any, has ended: a test's folder is
    /// removed with nothing in it mapped.
    #[cfg(test)]
    fn drained(&self) {
        let _ = self
            .reads
            .ask_blocking(std::time::Duration::from_secs(10), || ());
    }
}

/// #3908 — the files a read of the EXE's version was made for: the marker's
/// last write, and the EXE's last write and size. A swap whose marker write
/// fails changes the EXE and not the marker, so the marker's alone would keep
/// the version read before that swap for the process's life.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    marker: Option<std::time::SystemTime>,
    exe: Option<(std::time::SystemTime, u64)>,
}

#[cfg(target_os = "windows")]
impl Stamp {
    fn of(dir: &std::path::Path) -> Self {
        let written = |name: &str| {
            let meta = std::fs::metadata(dir.join(name)).ok()?;
            Some((meta.modified().ok()?, meta.len()))
        };
        Self {
            marker: written(VERSION_MARKER).map(|(at, _)| at),
            exe: written(DESKTOP_EXE),
        }
    }
}

/// #3908 — the companion's version as [`known_beside`] reads it now.
/// `settled` is false while the EXE's version, which the answer needs, is
/// still being read: the answer is then the marker's.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct BesideNow {
    version: Option<String>,
    settled: bool,
}

/// #3908 — the companion's version in `dir` by the refresh's rule
/// ([`known_version`], #3850), for the readers that may not wait: the marker
/// when it names `own` (one small file read, the common case); else what the
/// EXE states, as `versions` read it for the files there now; else the
/// marker's own words. With no answer for those files yet, the marker's
/// words, not settled, and the read starts.
#[cfg(target_os = "windows")]
fn known_beside(dir: &std::path::Path, own: &str, versions: &'static ExeVersions) -> BesideNow {
    let marker = std::fs::read_to_string(dir.join(VERSION_MARKER)).ok();
    let mut settled = true;
    let version = known_version(own, marker.as_deref(), || {
        let stated = versions.now(dir);
        settled = stated.is_some();
        stated.flatten()
    });
    BesideNow { version, settled }
}

/// #3850 — the version the EXE at `exe` states in its own version resource
/// ([`version_resource_version`]): the release's companion carries the
/// workspace version there, as its marker does. The file is mapped as data
/// and as an image resource only, so nothing in it runs and its imports are
/// not resolved, and it is freed on every path out. Through kernel32 alone,
/// which the daemon already imports: `GetFileVersionInfoW` would add
/// `version.dll` to what the SYSTEM service loads. `None` when it states
/// none or cannot be read.
#[cfg(target_os = "windows")]
fn exe_version(exe: &std::path::Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::{
        FindResourceW, LOAD_LIBRARY_AS_DATAFILE, LOAD_LIBRARY_AS_IMAGE_RESOURCE, LoadLibraryExW,
        LoadResource, LockResource, SizeofResource,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::RT_VERSION;

    /// The mapped file, freed when it goes out of scope, on every path.
    struct AsData(HMODULE);
    impl Drop for AsData {
        fn drop(&mut self) {
            // SAFETY: a module LoadLibraryExW returned, freed exactly once.
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }

    let wide: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call. As data and as
    // an image resource only: nothing in the file runs.
    let module = unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE,
        )
    };
    if module.is_null() {
        return None;
    }
    let module = AsData(module);
    // VS_VERSION_INFO is resource 1 of type RT_VERSION.
    // SAFETY: `module` is live, and the name is an integer resource id.
    let found = unsafe { FindResourceW(module.0, std::ptr::without_provenance(1), RT_VERSION) };
    if found.is_null() {
        return None;
    }
    // SAFETY: `found` is a resource of `module`, which is live.
    let (size, loaded) = unsafe {
        (
            SizeofResource(module.0, found),
            LoadResource(module.0, found),
        )
    };
    if loaded.is_null() || size == 0 {
        return None;
    }
    // SAFETY: a resource LoadResource returned; LockResource gives its address.
    let data = unsafe { LockResource(loaded) }.cast::<u8>();
    if data.is_null() {
        return None;
    }
    // SAFETY: the resource's `size` bytes at `data`, mapped while `module`
    // lives; the slice is read, and what it states copied out, before
    // `module` is freed at the end of this function.
    let res = unsafe { std::slice::from_raw_parts(data, size as usize) };
    version_resource_version(res)
}

/// #3850 — what a refresh pass finds beside the daemon ([`installed_state`]),
/// on a blocking thread. When the EXE is this version and only its marker says
/// otherwise, an earlier swap could not write the marker: it is written again,
/// once a pass, and nothing is swapped either way.
#[cfg(target_os = "windows")]
fn look_beside(own: &'static str, dest: &std::path::Path, marker: &std::path::Path) -> Installed {
    let recorded = std::fs::read_to_string(marker).ok();
    let found = installed_state(own, dest.exists(), recorded.as_deref(), || {
        exe_version(dest)
    });
    if found
        == (Installed::Current {
            by_its_version: true,
        })
    {
        let wrote = std::fs::write(marker, format!("{own}\n")).map_err(|e| e.to_string());
        match MARKER_WRITES.wrote(wrote) {
            None => tracing::info!(
                version = own,
                recorded = recorded
                    .as_deref()
                    .and_then(sanitize_version)
                    .as_deref()
                    .unwrap_or("<none>"),
                "desktop companion is already this version by its own version resource; its \
                 version marker is written again, and nothing is replaced"
            ),
            line => log_marker_line(line, marker, own),
        }
    }
    found
}

/// #3850 — the line for a write of the version marker ([`MarkerWrites`]).
#[cfg(target_os = "windows")]
fn log_marker_line(line: Option<MarkerLine>, marker: &std::path::Path, version: &str) {
    const NOT_WRITTEN: &str = "could not write desktop version marker; the refresh goes by the \
                               companion EXE's own version until it can, and does not replace \
                               that EXE again";
    let marker = marker.display();
    match line {
        Some(MarkerLine::NotWritten { error, first: true }) => {
            tracing::warn!(error = %error, marker = %marker, version, "{NOT_WRITTEN}")
        }
        Some(MarkerLine::NotWritten {
            error,
            first: false,
        }) => {
            tracing::debug!(error = %error, marker = %marker, version, "{NOT_WRITTEN}")
        }
        Some(MarkerLine::WrittenAgain) => {
            tracing::info!(marker = %marker, version, "desktop version marker written again")
        }
        None => {}
    }
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
/// a burst of `launchctl kickstart` / `CreateProcessAsUserW` calls. A verdict
/// that cannot change soon stands [`ENSURE_COOLDOWN`]: a tray that is up (a
/// launch that works is visible within a second), or no companion on this
/// host. #3884 — a start that failed or ran out of its wait stands
/// [`ENSURE_RETRY_AFTER_ERROR`] only: it goes on on its thread and may bring
/// the tray up just after, and the person may sign in meanwhile, so the prompt
/// that refusal invites looks again.
/// Returns whether a human can now, in fact, be shown a prompt on this host.
///
/// The caller reports `no_prompt_surface` on `false`, which is why this is a
/// verdict and not a fire-and-forget: without it the agent could only ever
/// report a TIMEOUT, and "nobody answered in 30 s" and "there is nobody to
/// ask, and there never was" need different things done about them.
pub async fn ensure_running() -> bool {
    let verdicts = ensure_verdicts();
    if let Some(up) = verdicts.standing() {
        return up && reaches_the_tray().await;
    }

    let (up, stands) = match ensure_running_inner().await {
        Ok(EnsureOutcome::AlreadyRunning) => {
            tracing::debug!("desktop companion is already running");
            (true, ENSURE_COOLDOWN)
        }
        Ok(EnsureOutcome::Started) => {
            tracing::info!("started the desktop companion for a prompt");
            (true, ENSURE_COOLDOWN)
        }
        Ok(EnsureOutcome::Unsupported) => {
            tracing::info!(
                "no desktop companion on this host — an on-screen prompt is not possible; \
                 answer with `roomlerd consent --list` / `--approve`, or set this device to \
                 email/push consent"
            );
            (false, ENSURE_COOLDOWN)
        }
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                asks_again_in = ?ENSURE_RETRY_AFTER_ERROR,
                "could not start the desktop companion — an on-screen prompt is not possible"
            );
            (false, ENSURE_RETRY_AFTER_ERROR)
        }
    };
    verdicts.keep(up, stands);
    up && reaches_the_tray().await
}

/// One attempt per this interval, process-wide, after a verdict that cannot
/// change soon: the tray is up, or there is no companion on this host.
const ENSURE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(20);

/// #3884 — how long a start that failed, or ran out of its wait, stands: a few
/// seconds, not [`ENSURE_COOLDOWN`]. Such a start goes on on its thread and may
/// bring the tray up just after, and the person may sign in meanwhile, so the
/// prompt the refusal invites asks again. Under the 2.5 s the start waits
/// (`TRAY_START_WAIT`): a start that ran out of time answers 2.5 s after its
/// prompt and stands until 4.5 s, so a prompt 5 s after that one asks afresh.
/// Still a rate limit: the prompts in a burst after a failure are answered
/// from it, with no start of their own, and one that asks while a start still
/// hangs joins that start (`TRAY_STARTS`, one in flight).
const ENSURE_RETRY_AFTER_ERROR: std::time::Duration = std::time::Duration::from_secs(2);

/// #3884 — [`ensure_running`]'s last verdict, and how long it stands: both
/// halves of the rate limit. A cooldown that returned an optimistic default
/// would report a surface that does not exist — worse than not rate-limiting
/// at all.
struct EnsureVerdicts(std::sync::Mutex<Option<EnsureVerdict>>);

/// A verdict of [`ensure_running`]: when it was reached, whether the companion
/// is up, and how long that stands.
#[derive(Debug, Clone, Copy)]
struct EnsureVerdict {
    at: std::time::Instant,
    up: bool,
    stands: std::time::Duration,
}

impl EnsureVerdicts {
    const fn new() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    /// The last verdict, while it stands.
    fn standing(&self) -> Option<bool> {
        let last = *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        last.filter(|v| v.at.elapsed() < v.stands).map(|v| v.up)
    }

    /// `up`, standing `stands` from now.
    fn keep(&self, up: bool, stands: std::time::Duration) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(EnsureVerdict {
            at: std::time::Instant::now(),
            up,
            stands,
        });
    }
}

/// The verdicts [`ensure_running`] keeps: the daemon's, or under a test's
/// stand-in ([`tray_stand_in`]) that test's own.
fn ensure_verdicts() -> &'static EnsureVerdicts {
    static VERDICTS: EnsureVerdicts = EnsureVerdicts::new();
    #[cfg(all(test, any(target_os = "windows", unix)))]
    if let Some(stood) = tray_stand_in::now() {
        return stood.verdicts;
    }
    &VERDICTS
}

/// #3888 — the companion's start for a consent prompt, as the WS loop asks
/// for it ([`ensure_running`], [`crate::signaling::COMPANION_START_BUDGET`]
/// at most): STARTED, on a task of its own, and never waited for here. The
/// start asks other processes (logind, the process list, launchd), and the
/// loop reads its next frame meanwhile. Its verdict is the session's consent
/// task's to wait for ([`Starting::up`]), before the question's window
/// starts, so a start that failed or ran out of time is still
/// `no_prompt_surface`, as when the loop waited for it.
pub(crate) async fn start_for_prompt() -> Starting {
    let start = tokio::time::timeout(crate::signaling::COMPANION_START_BUDGET, ensure_running());
    // A test stands the tray's start in for its own task; the start runs on a
    // task of its own, so the stand-in goes with it.
    #[cfg(all(test, any(target_os = "windows", unix)))]
    let start = {
        let stood = tray_stand_in::now();
        async move {
            match stood {
                Some(stood) => tray_stand_in::asking(stood, start).await,
                None => start.await,
            }
        }
    };
    Starting(tokio::spawn(async move { start.await.unwrap_or(false) }))
}

/// #3888 — a companion's start for a prompt ([`start_for_prompt`]).
pub(crate) struct Starting(tokio::task::JoinHandle<bool>);

impl Starting {
    /// Whether the companion is up to show the question: its start's
    /// verdict. `false` for a start that failed, that did not answer within
    /// its budget, or that ended with no answer.
    pub(crate) async fn up(self) -> bool {
        self.0.await.unwrap_or(false)
    }
}

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
/// file read. Every arm reads metadata the installer already wrote. #3908: on
/// Windows that includes the EXE's own version resource, mapped as data on a
/// thread of its own, never run.
///
/// Cached for [`VERSION_TTL`]: the answer only changes when a package manager
/// runs, and the heartbeat is every 30 s. #3908: an answer that is not
/// settled is not kept ([`kept_version`]).
pub fn installed_version() -> Option<String> {
    kept_version(&VERSION_CACHE, || {
        #[cfg(target_os = "windows")]
        {
            let now = installed_version_inner();
            (now.version, now.settled)
        }
        #[cfg(not(target_os = "windows"))]
        {
            (installed_version_inner(), true)
        }
    })
}

/// [`installed_version`] over `cache`: the answer it holds while that is
/// younger than [`VERSION_TTL`], else `read`'s. #3908: `read`'s is kept only
/// when it says the answer is settled. A Windows answer from the marker while
/// the EXE's version is still being read goes out once, and the next
/// heartbeat reads again, instead of sending the marker's version for ten
/// minutes after each start.
fn kept_version(
    cache: &std::sync::Mutex<Option<(std::time::Instant, Option<String>)>>,
    read: impl FnOnce() -> (Option<String>, bool),
) -> Option<String> {
    if let Some((at, ref v)) = *cache.lock().unwrap()
        && at.elapsed() < VERSION_TTL
    {
        return v.clone();
    }
    let (v, settled) = read();
    if settled {
        *cache.lock().unwrap() = Some((std::time::Instant::now(), v.clone()));
    }
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

/// Windows: the version the refresh goes by (#3908, [`known_beside`]): the
/// sidecar marker [`refresh_if_stale`] writes on every swap when it names
/// this daemon's version, else what the EXE states in its own version
/// resource, else the marker's words. `None` with no EXE, or when neither
/// names a version.
#[cfg(target_os = "windows")]
fn installed_version_inner() -> BesideNow {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
    else {
        return BesideNow {
            version: None,
            settled: true,
        };
    };
    installed_beside(&dir, env!("CARGO_PKG_VERSION"), &EXE_VERSIONS)
}

/// #3908 — [`installed_version`]'s answer for the companion in `dir`.
#[cfg(target_os = "windows")]
fn installed_beside(dir: &std::path::Path, own: &str, versions: &'static ExeVersions) -> BesideNow {
    if !dir.join(DESKTOP_EXE).exists() {
        return BesideNow {
            version: None,
            settled: true,
        };
    }
    known_beside(dir, own, versions)
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
    use super::{kept_version, sanitize_version};

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

    /// #3908 — the heartbeat keeps an answer for `VERSION_TTL` only once it
    /// is settled. One read from the marker while the EXE's version is still
    /// being read goes out once, and the next heartbeat reads again.
    ///
    /// Red before #3908: every answer was kept, so the marker's version went
    /// out for ten minutes after each start.
    #[test]
    fn the_heartbeat_keeps_only_a_settled_answer() {
        let cache = std::sync::Mutex::new(None);
        assert_eq!(
            kept_version(&cache, || (Some("0.4.212".into()), false)).as_deref(),
            Some("0.4.212")
        );
        assert_eq!(
            kept_version(&cache, || (Some("0.4.223".into()), true)).as_deref(),
            Some("0.4.223"),
            "the marker's answer was not kept"
        );
        assert_eq!(
            kept_version(&cache, || panic!("a settled answer is kept")).as_deref(),
            Some("0.4.223")
        );
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

/// #3855, #3858 — the tray's start for a prompt, off the async worker, on
/// Linux, Windows and macOS alike: every step ([`TraySteps`], each OS's own)
/// runs on the thread of the one start in flight ([`TRAY_STARTS`],
/// `tray-start`), and this waits [`TRAY_START_WAIT`] at most. Before, this
/// async function never awaited, so each step ran on the worker that polled
/// it, for as long as it took (logind, the account database or systemd-run on
/// Linux; the process list or `CreateProcessAsUserW` on Windows; `pgrep`,
/// `stat`, `id` or `launchctl` on macOS), and the callers' 3 s timeout could
/// not fire.
#[cfg(any(target_os = "windows", unix))]
async fn ensure_running_inner() -> Result<EnsureOutcome> {
    // A test stands the steps in for its own task, with starts in flight of
    // its own.
    #[cfg(test)]
    if let Some(stood) = tray_stand_in::now() {
        return stood.starts.start(stood.steps, TRAY_START_WAIT).await;
    }
    TRAY_STARTS.start(TraySteps::real(), TRAY_START_WAIT).await
}

/// The steps of the Windows tray's start, in order, as [`TrayStarts::at`]
/// holds them.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum TrayStep {
    Running,
    Path,
    Context,
    StartInSession,
    StartHere,
}

#[cfg(target_os = "windows")]
impl TrayStep {
    fn from_u8(at: u8) -> Self {
        match at {
            1 => Self::Path,
            2 => Self::Context,
            3 => Self::StartInSession,
            4 => Self::StartHere,
            _ => Self::Running,
        }
    }

    /// What a start at this step waits on, in a refusal's words.
    fn waiting_on(self) -> &'static str {
        match self {
            Self::Running => {
                "the process list (`companion_procs`: a process snapshot, then each candidate's \
                 loaded image)"
            }
            Self::Path => "the companion's install path",
            Self::Context => "the daemon's own token (whether it runs as the SYSTEM service)",
            Self::StartInSession => {
                "the start in the console session (`WTSQueryUserToken`, then \
                 `CreateEnvironmentBlock` and `CreateProcessAsUserW`)"
            }
            Self::StartHere => "the start in the daemon's own session (`CreateProcessW`)",
        }
    }
}

/// #3858 — the blocking steps of the Windows tray's start for a prompt:
/// whether the tray beside this daemon runs (the process list), where it is
/// installed, which start this daemon makes (its own token), and the start
/// itself ([`start_desktop`], #3848's, with its session choice). Each is a
/// seam a test stands in; the daemon's are [`TraySteps::real`].
#[cfg(target_os = "windows")]
#[derive(Clone)]
struct TraySteps {
    running: std::sync::Arc<RunningStep>,
    path: std::sync::Arc<PathStep>,
    context: std::sync::Arc<ContextStep>,
    start: std::sync::Arc<StartStep>,
}

/// Whether the tray beside this daemon runs.
#[cfg(target_os = "windows")]
type RunningStep = dyn Fn() -> bool + Send + Sync;
/// Where the tray is installed, if it is.
#[cfg(target_os = "windows")]
type PathStep = dyn Fn() -> Result<Option<std::path::PathBuf>> + Send + Sync;
/// Which start this daemon makes.
#[cfg(target_os = "windows")]
type ContextStep = dyn Fn() -> RespawnContext + Send + Sync;
/// The start, at a path, with arguments.
#[cfg(target_os = "windows")]
type StartStep = dyn Fn(RespawnContext, &std::path::Path, &[&str]) -> TrayStart + Send + Sync;

#[cfg(target_os = "windows")]
impl TraySteps {
    /// The daemon's own steps.
    fn real() -> Self {
        Self {
            running: std::sync::Arc::new(desktop_running),
            path: std::sync::Arc::new(installed_desktop),
            context: std::sync::Arc::new(respawn_context_for_self),
            start: std::sync::Arc::new(start_desktop),
        }
    }
}

/// The Windows tray's start, blocking, on the start in flight's thread: its
/// steps in the order the start has always taken them, each recorded in `at`
/// as it begins. `Started` only when the process was created
/// ([`started_outcome`]). The companion is told it is for a prompt (#3887).
#[cfg(target_os = "windows")]
fn start_tray(
    steps: &TraySteps,
    at: &std::sync::atomic::AtomicU8,
) -> std::result::Result<EnsureOutcome, String> {
    let step = |now: TrayStep| at.store(now as u8, std::sync::atomic::Ordering::SeqCst);
    step(TrayStep::Running);
    if (steps.running)() {
        return Ok(EnsureOutcome::AlreadyRunning);
    }
    step(TrayStep::Path);
    let Some(dest) = (steps.path)().map_err(|e| format!("{e:#}"))? else {
        // Not an error: `install.ps1 -SkipDesktop` is a supported choice, and
        // so is a daemon-only deployment. The caller reports
        // `no_prompt_surface` either way — this just keeps the log honest
        // about which of the two happened.
        return Ok(EnsureOutcome::Unsupported);
    };
    // A SYSTEM daemon has no desktop of its own: the companion is launched INTO the interactive
    // session, as the update respawn does. #3887: for a prompt, so the tour waits for the person.
    let args = [roomler_node_core::companion_autostart::FOR_PROMPT_ARG];
    step(TrayStep::Context);
    let ctx = (steps.context)();
    step(match ctx {
        RespawnContext::SystemService => TrayStep::StartInSession,
        RespawnContext::UserSession => TrayStep::StartHere,
    });
    started_outcome((steps.start)(ctx, &dest, &args)).map_err(|e| format!("{e:#}"))
}

/// The tray beside this daemon, when it is installed there.
#[cfg(target_os = "windows")]
fn installed_desktop() -> Result<Option<std::path::PathBuf>> {
    let exe_dir = std::env::current_exe()
        .context("locating own exe")?
        .parent()
        .context("own exe has no parent dir")?
        .to_path_buf();
    let dest = exe_dir.join(DESKTOP_EXE);
    Ok(dest.exists().then_some(dest))
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

/// The steps of the macOS tray's start, in order, as [`TrayStarts::at`]
/// holds them.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum TrayStep {
    Bundle,
    Running,
    Console,
    Account,
    Launchctl,
}

#[cfg(target_os = "macos")]
impl TrayStep {
    fn from_u8(at: u8) -> Self {
        match at {
            1 => Self::Running,
            2 => Self::Console,
            3 => Self::Account,
            4 => Self::Launchctl,
            _ => Self::Bundle,
        }
    }

    /// What a start at this step waits on, in a refusal's words.
    fn waiting_on(self) -> &'static str {
        match self {
            Self::Bundle => "the companion's app bundle (`/Applications/Roomler.app`)",
            Self::Running => "the process list (`pgrep`)",
            Self::Console => "the console's owner (`stat /dev/console`)",
            Self::Account => "the account database (`id -u`, for the console user's uid)",
            Self::Launchctl => "launchd (`launchctl kickstart`)",
        }
    }
}

/// #3858 — the blocking steps of the macOS tray's start for a prompt: whether
/// the app is installed, whether a tray runs (the process list), who owns the
/// console, that account's uid (the account database) and launchd's start of
/// the LaunchAgent. Each is a seam a test stands in; the daemon's are
/// [`TraySteps::real`].
#[cfg(target_os = "macos")]
#[derive(Clone)]
struct TraySteps {
    installed: std::sync::Arc<InstalledStep>,
    running: std::sync::Arc<RunningStep>,
    console: std::sync::Arc<ConsoleStep>,
    uid: std::sync::Arc<UidStep>,
    launchctl: std::sync::Arc<LaunchctlStep>,
}

/// Whether the app bundle is installed.
#[cfg(target_os = "macos")]
type InstalledStep = dyn Fn() -> bool + Send + Sync;
/// Whether a tray runs.
#[cfg(target_os = "macos")]
type RunningStep = dyn Fn() -> bool + Send + Sync;
/// The console's owner, a person's account, or why there is none.
#[cfg(target_os = "macos")]
type ConsoleStep = dyn Fn() -> Result<String> + Send + Sync;
/// That account's uid.
#[cfg(target_os = "macos")]
type UidStep = dyn Fn(&str) -> Result<u32> + Send + Sync;
/// launchctl, run.
#[cfg(target_os = "macos")]
type LaunchctlStep =
    dyn Fn(std::process::Command) -> std::io::Result<std::process::Output> + Send + Sync;

#[cfg(target_os = "macos")]
impl TraySteps {
    /// The daemon's own steps.
    fn real() -> Self {
        const BUNDLE: &str = "/Applications/Roomler.app";
        Self {
            installed: std::sync::Arc::new(|| std::path::Path::new(BUNDLE).exists()),
            running: std::sync::Arc::new(|| pgrep_running("roomler-desktop")),
            console: std::sync::Arc::new(console_user_name),
            uid: std::sync::Arc::new(account_uid),
            launchctl: std::sync::Arc::new(|mut cmd: std::process::Command| cmd.output()),
        }
    }
}

/// The macOS tray's start, blocking, on the start in flight's thread: its
/// steps in the order the start has always taken them, each recorded in `at`
/// as it begins. `Started` only when launchctl exited 0.
#[cfg(target_os = "macos")]
fn start_tray(
    steps: &TraySteps,
    at: &std::sync::atomic::AtomicU8,
) -> std::result::Result<EnsureOutcome, String> {
    const LABEL: &str = "com.roomler.desktop";
    let step = |now: TrayStep| at.store(now as u8, std::sync::atomic::Ordering::SeqCst);
    step(TrayStep::Bundle);
    if !(steps.installed)() {
        // Not an error, for the same reason as the Windows arm: a daemon-only
        // install is a supported shape, and the caller reports
        // `no_prompt_surface` either way. Saying WHICH keeps the log honest.
        return Ok(EnsureOutcome::Unsupported);
    }
    step(TrayStep::Running);
    if (steps.running)() {
        return Ok(EnsureOutcome::AlreadyRunning);
    }
    // The companion is a LaunchAgent in the console user's GUI domain. Ask
    // launchd rather than spawning it ourselves: the daemon half runs as root,
    // and a root-spawned GUI app would land in the wrong session with the
    // wrong TCC identity. `kickstart` is also idempotent.
    //
    // The console user is resolved the way the pkg's postinstall does
    // (`stat -f %Su /dev/console`) so the two agree by construction.
    step(TrayStep::Console);
    let user = (steps.console)().map_err(|e| format!("resolving the console user: {e:#}"))?;
    step(TrayStep::Account);
    let uid =
        (steps.uid)(user.as_str()).map_err(|e| format!("resolving the console user: {e:#}"))?;
    let target = format!("gui/{uid}/{LABEL}");
    let mut kickstart = std::process::Command::new("launchctl");
    kickstart.args(["kickstart", "-k", &target]);
    step(TrayStep::Launchctl);
    let out = (steps.launchctl)(kickstart).map_err(|e| format!("spawning launchctl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "launchctl kickstart {target} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(EnsureOutcome::Started)
}

/// The uid of `user`, the console's owner ([`console_user_name`]), from the
/// account database (`id -u`).
#[cfg(target_os = "macos")]
fn account_uid(user: &str) -> Result<u32> {
    let out = std::process::Command::new("id")
        .args(["-u", user])
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

/// #3855, #3858 — the tray's starts for prompts: at most one in flight, on a
/// thread named `tray-start` (`one_in_flight`'s rules). While a step hangs
/// (logind, the account database, the person's user manager or systemd-run
/// on Linux; the process list, the console session's token or the process
/// start on Windows; the console's owner, the account database or launchd on
/// macOS), every prompt that asks waits on that one start, within its own
/// wait, and starts none of its own, so the hang holds one thread however
/// many prompts ask.
#[cfg(any(target_os = "windows", unix))]
static TRAY_STARTS: TrayStarts = TrayStarts::new("tray-start");

/// One start in flight, and the step it is at.
#[cfg(any(target_os = "windows", unix))]
struct TrayStarts {
    flight: crate::one_in_flight::OneInFlight<std::result::Result<EnsureOutcome, String>>,
    /// The step the running start (or the last one) is at, a [`TrayStep`]
    /// (each OS's own): what a prompt that ran out of time says it was
    /// waiting on.
    at: std::sync::atomic::AtomicU8,
}

#[cfg(any(target_os = "windows", unix))]
impl TrayStarts {
    const fn new(thread_name: &'static str) -> Self {
        Self {
            flight: crate::one_in_flight::OneInFlight::new(thread_name),
            // Every OS's first step is its `TrayStep`'s first variant, 0.
            at: std::sync::atomic::AtomicU8::new(0),
        }
    }

    /// A start with `steps`, on the start in flight's thread, waited for
    /// `wait` at most. Out of time it is an error naming the step the start
    /// was still waiting on; the start goes on, and its late answer is
    /// nobody's.
    async fn start(
        &'static self,
        steps: TraySteps,
        wait: std::time::Duration,
    ) -> Result<EnsureOutcome> {
        let at = &self.at;
        let asked = self.flight.ask(wait, move || start_tray(&steps, at)).await;
        started_in_time(
            asked,
            wait,
            TrayStep::from_u8(at.load(std::sync::atomic::Ordering::SeqCst)),
        )
    }
}

/// The steps of the tray's start, in order, as [`TrayStarts::at`] holds them.
#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum TrayStep {
    Path,
    Session,
    Running,
    Environment,
    SystemdRun,
}

#[cfg(all(unix, not(target_os = "macos")))]
impl TrayStep {
    fn from_u8(at: u8) -> Self {
        match at {
            1 => Self::Session,
            2 => Self::Running,
            3 => Self::Environment,
            4 => Self::SystemdRun,
            _ => Self::Path,
        }
    }

    /// What a start at this step waits on, in a refusal's words.
    fn waiting_on(self) -> &'static str {
        match self {
            Self::Path => "the companion's install path",
            Self::Session => "logind (`loginctl`, for the graphical session)",
            Self::Running => "the process list (`pgrep`)",
            Self::Environment => {
                "the session's environment (the account database, then the person's user manager)"
            }
            Self::SystemdRun => "systemd-run",
        }
    }
}

/// #3855, #3858 — what a prompt that waited `wait` takes from the start in
/// flight: its outcome, or an error with the reason. A start's own failure
/// keeps its words; no answer in time names the step the start was at.
#[cfg(any(target_os = "windows", unix))]
fn started_in_time(
    asked: crate::one_in_flight::Asked<std::result::Result<EnsureOutcome, String>>,
    wait: std::time::Duration,
    at: TrayStep,
) -> Result<EnsureOutcome> {
    use crate::one_in_flight::Asked;
    match asked {
        Asked::Answer(Ok(outcome)) => Ok(outcome),
        Asked::Answer(Err(why)) => Err(anyhow::anyhow!(why)),
        Asked::Failed(why) => Err(anyhow::anyhow!(
            "the tray's start ended with no answer: {why}"
        )),
        Asked::OutOfTime => Err(anyhow::anyhow!(
            "the tray's start did not finish within {wait:?}: it was still waiting on {}; it goes \
             on on its own thread, and the next prompt asks again",
            at.waiting_on()
        )),
    }
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

/// The Linux tray's start, blocking, on the start in flight's thread: its
/// steps in the order the start has always taken them, each recorded in `at`
/// as it begins. `Started` only when systemd-run exited 0. For a prompt (#3887).
#[cfg(all(unix, not(target_os = "macos")))]
fn start_tray(
    steps: &TraySteps,
    at: &std::sync::atomic::AtomicU8,
) -> std::result::Result<EnsureOutcome, String> {
    let step = |now: TrayStep| at.store(now as u8, std::sync::atomic::Ordering::SeqCst);
    step(TrayStep::Path);
    let Some(path) = (steps.path)() else {
        // Until FR-27's packaging phase there is no Linux companion at all,
        // and after it there still won't be on a headless server — which is
        // the correct state for a machine with no screen, not a fault.
        return Ok(EnsureOutcome::Unsupported);
    };

    // A root systemd daemon has no display of its own. Find the graphical
    // session's owner and its bus/display, then spawn as that user.
    step(TrayStep::Session);
    let sess =
        (steps.session)().map_err(|e| format!("finding a graphical login session: {e:#}"))?;
    // FR-93 P6b — THAT user's companion (`pgrep -u`: another user's on a
    // shared machine is not this person's), started through the manager this
    // daemon may use. A per-user daemon has its own user manager; the system
    // manager would ask polkit for a password nobody is there to type. The
    // shape `launch_once` already has.
    step(TrayStep::Running);
    if (steps.running)(sess.uid) {
        return Ok(EnsureOutcome::AlreadyRunning);
    }
    let args = [roomler_node_core::companion_autostart::FOR_PROMPT_ARG]; // #3887: for a prompt
    // #2713 — a root daemon's system unit carries the desktop session's own environment
    // ([`systemd_run_command`]); a per-user daemon's `--user` unit inherits its manager's.
    let session_env = if steps.user_manager {
        Vec::new()
    } else {
        step(TrayStep::Environment);
        (steps.environment)(&sess)
            .map(|text| session_env_pairs(&text))
            .unwrap_or_default()
    };
    let cmd = systemd_run_command_with(&sess, &path, &args, steps.user_manager, &session_env);
    step(TrayStep::SystemdRun);
    let out = (steps.systemd_run)(cmd).map_err(|e| format!("spawning systemd-run: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "systemd-run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(EnsureOutcome::Started)
}

/// #3855, #3858 — how long a prompt waits for the tray's start: 2.5 s, under
/// the 3 s its callers give it (`signaling::COMPANION_START_BUDGET`), so the
/// start's own reason lands before their fallback fires (the hub's lesson
/// with equal timers, FR-27). A start that works (`loginctl`, `pgrep`, the
/// session's environment, `systemd-run` on Linux; the process list and the
/// process start on Windows; `pgrep`, `stat`, `id` and `launchctl kickstart`
/// on macOS: tens to a few hundred milliseconds) takes well under half of
/// it, which one start in flight needs to answer a prompt that asks just
/// after another start began (`one_in_flight`'s cost).
#[cfg(any(target_os = "windows", unix))]
const TRAY_START_WAIT: std::time::Duration = std::time::Duration::from_millis(2500);

/// #3864 — the rest of [`ensure_running`]'s verdict, asked fresh at every
/// prompt, beside the tray's own: whether a question this daemon asks gets to
/// the tray at all. Off a one-row Mac's root daemon the tray asks this daemon
/// itself, and it does. On one, the tray asks the worker, which lists the
/// root daemon's questions only while the road is open
/// ([`crate::consent_relay::reach`]): closed, the tray is not the question's
/// surface, and a window nobody answers ends as `no_prompt_surface`.
async fn reaches_the_tray() -> bool {
    // A test stands the road in with the tray's start.
    #[cfg(all(test, any(target_os = "windows", unix)))]
    let reach = match tray_stand_in::now().and_then(|stood| stood.road) {
        Some(road) => road.reach().await,
        None => crate::consent_relay::reach().await,
    };
    #[cfg(not(all(test, any(target_os = "windows", unix))))]
    let reach = crate::consent_relay::reach().await;
    if let crate::consent_relay::Reach::Closed(why) = &reach {
        tracing::info!(
            why = %why,
            "a one-row Mac's question cannot reach the menu-bar app, which is not its surface \
             (#3864); `roomlerd consent` still answers it"
        );
    }
    reach.open()
}

/// #3855, #3858 — the tray's start stood in for the tasks of one test: its
/// steps, in place of the daemon's ([`TraySteps::real`]), and starts in
/// flight of its own. A task-local, so the tests beside it, and the daemon's
/// own starts, are never stood in.
#[cfg(all(test, any(target_os = "windows", unix)))]
mod tray_stand_in {
    use super::{EnsureVerdicts, TrayStarts, TraySteps};

    #[derive(Clone)]
    pub(super) struct StandIn {
        pub(super) steps: TraySteps,
        pub(super) starts: &'static TrayStarts,
        /// #3884 — [`super::ensure_running`]'s verdicts, of this test's own.
        pub(super) verdicts: &'static EnsureVerdicts,
        /// #3864 — a one-row Mac's road from the root daemon's question to
        /// the tray, stood in. `None`: this daemon is not a one-row Mac's
        /// root, and the tray asks it directly.
        pub(super) road: Option<crate::consent_relay::stand_in::Road>,
    }

    impl StandIn {
        pub(super) fn new(steps: TraySteps) -> Self {
            Self {
                steps,
                starts: Box::leak(Box::new(TrayStarts::new("tray-start"))),
                verdicts: Box::leak(Box::new(EnsureVerdicts::new())),
                road: None,
            }
        }

        /// #3864 — this daemon as a one-row Mac's root, with `road`.
        pub(super) fn on_one_row(self, road: crate::consent_relay::stand_in::Road) -> Self {
            Self {
                road: Some(road),
                ..self
            }
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

/// #3855, #3858 — the tray's start for a prompt with its steps stood in
/// ([`tray_stand_in`]): each OS's own steps (`on_linux`, `on_windows`,
/// `on_mac`), any one of which a test holds up, and everything above them the
/// daemon's. On Linux a stopped logind, a session environment that does not
/// come (the account database or the person's user manager) and a
/// systemd-run that hangs; on Windows the process list and the start into the
/// console session; on macOS the process list, the console's owner, the
/// account database and launchd. Each probe runs as a task on a two-worker
/// runtime, the daemon's shape, and is waited for from the test's own thread
/// (#3836's shape).
#[cfg(all(test, any(target_os = "windows", unix)))]
mod tray_start_tests {
    use super::tray_stand_in::{StandIn, asking};
    use super::*;
    #[cfg(all(unix, not(target_os = "macos")))]
    use on_linux::host_steps;
    #[cfg(all(unix, not(target_os = "macos")))]
    use on_linux::{COMPANION, HELD, steps};
    #[cfg(target_os = "macos")]
    use on_mac::host_steps;
    #[cfg(target_os = "macos")]
    use on_mac::{HELD, steps};
    #[cfg(target_os = "windows")]
    use on_windows::host_steps;
    #[cfg(target_os = "windows")]
    use on_windows::{DEST, HELD, steps};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    use std::sync::{Arc, Condvar, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    /// What a failure may take past the start's own wait on a busy runner.
    const SLACK: Duration = Duration::from_secs(1);
    /// What a start that answers may take on a busy runner.
    const RUNS_WITHIN: Duration = Duration::from_secs(10);
    /// How late "on time" may be on a busy runner: #3836's bar.
    const AT_ONCE: Duration = Duration::from_millis(500);

    /// What a start answered: the outcome, or why it failed.
    type Answer = std::result::Result<EnsureOutcome, String>;

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

    /// What the start ran, each time: systemd-run's or launchctl's argv, or
    /// on Windows the start's context, path and arguments.
    #[derive(Clone, Default)]
    struct Ran(Arc<Mutex<Vec<Vec<String>>>>);

    impl Ran {
        #[cfg(unix)]
        fn record(&self, cmd: &std::process::Command) {
            let mut argv = vec![cmd.get_program().to_string_lossy().into_owned()];
            argv.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
            self.push(argv);
        }

        fn push(&self, ran: Vec<String>) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(ran);
        }

        fn argvs(&self) -> Vec<Vec<String>> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    /// A child that exited `code`, having written `stderr`.
    #[cfg(unix)]
    fn exited(code: i32, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
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

    /// #3855, #3858 — with a step held up ([`HELD`]: on Linux logind, the
    /// session's environment (the account database, the person's user
    /// manager) or systemd-run; on Windows the process list or the start into
    /// the console session; on macOS the process list, the console's owner,
    /// the account database or launchd), a start for a prompt fails within
    /// its own wait ([`TRAY_START_WAIT`]), never `Started`, and its reason
    /// names the step that did not answer.
    ///
    /// Red before #3855 on Linux, and before #3858 on Windows and macOS: the
    /// start ran on the worker that asked, for as long as the step was held.
    #[test]
    fn a_held_step_fails_the_start_within_its_wait_naming_it() {
        let rt = runtime();
        for (step, names) in HELD {
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

    /// #3855, #3858 — a held start holds no async worker. On a two-worker
    /// runtime, tasks asking again and again while the OS's first held step
    /// (`HELD[0]`: logind on Linux, the process list on Windows and macOS)
    /// holds the start leave a 10 ms timer waking on time (#3836's measure),
    /// and the hold holds one start, however many ask.
    ///
    /// Red before #3855 on Linux, and before #3858 on Windows and macOS: each
    /// ask held the worker it ran on, and two of them held both.
    #[test]
    fn a_held_start_delays_no_timer() {
        let (step, _) = HELD[0];
        let rt = runtime();
        let held = Held::default();
        let _opened = OpenedAtTheEnd(held.clone());
        let ran = Ran::default();
        let stood = StandIn::new(steps(Some(step), &held, &ran));
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
            assert!(asked.elapsed() < RUNS_WITHIN, "no start reached {step:?}");
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
        let worst = rx.recv_timeout(RUNS_WITHIN).unwrap_or_else(|_| {
            panic!("a 10 ms timer stopped waking while {step:?} held the start")
        });
        assert!(
            worst < AT_ONCE,
            "a 10 ms timer woke {worst:?} late while {step:?} held the start"
        );
        assert_eq!(held.began(), 1, "one start in flight, however many asked");
    }

    /// #3855, #3858 — while the OS's first held step (`HELD[0]`) holds a
    /// start, asks at once and in a row share it: each fails within its own
    /// wait, and none starts another, so the hold holds one thread. Once the
    /// step answers, the late start answers nobody, and the next ask starts
    /// afresh and is answered.
    ///
    /// Red before #3855 on Linux, and before #3858 on Windows and macOS:
    /// every ask was a start of its own, on its own worker.
    #[test]
    fn asks_while_a_start_hangs_share_it() {
        let (step, _) = HELD[0];
        let rt = runtime();
        let held = Held::default();
        let _opened = OpenedAtTheEnd(held.clone());
        let ran = Ran::default();
        let stood = StandIn::new(steps(Some(step), &held, &ran));
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
                .unwrap_or_else(|_| {
                    panic!("every ask at once is answered while {step:?} holds the start")
                });
            assert!(started.is_err(), "nothing starts while held: {started:?}");
        }
        for _ in 0..2 {
            let (started, _) = start_on_a_worker(&rt, &stood, TRAY_START_WAIT + SLACK)
                .unwrap_or_else(|e| {
                    panic!("an ask in a row is answered while {step:?} holds the start: {e}")
                });
            assert!(started.is_err(), "nothing starts while held: {started:?}");
        }
        assert_eq!(held.began(), 1, "one start, however many asked");
        assert!(ran.argvs().is_empty(), "nothing was started while held");
        held.open();
        let (started, _) =
            start_on_a_worker(&rt, &stood, RUNS_WITHIN).expect("the next ask is answered");
        assert_eq!(started, Ok(EnsureOutcome::Started), "a start of its own");
        assert_eq!(held.began(), 2, "the held start, and the next ask's own");
    }

    /// #3884, #3888 — a host the tray's start runs on, as each OS's
    /// `host_steps` stands it in: whether the companion is installed, whether
    /// someone is signed in at the screen, and whether the tray runs, which a
    /// start that works brings about. The start itself (systemd-run, the
    /// process start, launchctl) waits for `held` when there is one. Each
    /// start is counted at its first step.
    #[derive(Clone, Default)]
    struct Host {
        not_installed: Arc<AtomicBool>,
        nobody: Arc<AtomicBool>,
        up: Arc<AtomicBool>,
        held: Option<Held>,
        starts: Arc<AtomicUsize>,
    }

    impl Host {
        /// The starts that began.
        fn starts(&self) -> usize {
            self.starts.load(SeqCst)
        }

        /// Whether the tray runs.
        fn up(&self) -> bool {
            self.up.load(SeqCst)
        }
    }

    /// What [`ensure_running`] answered a prompt, as a task on `rt` taking
    /// `stood`'s steps, starts in flight and verdicts, and how long that
    /// took: waited for `bound` at most from THIS thread.
    fn ensure_on_a_worker(
        rt: &tokio::runtime::Runtime,
        stood: &StandIn,
        bound: Duration,
    ) -> std::result::Result<(bool, Duration), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let asked = Instant::now();
        rt.spawn(asking(stood.clone(), async move {
            let up = ensure_running().await;
            let _ = tx.send((up, asked.elapsed()));
        }));
        rx.recv_timeout(bound)
            .map_err(|_| format!("no answer within {bound:?}"))
    }

    /// #3884 — a start that ran out of its wait goes on on its thread, and
    /// here brings the tray up just after. The prompt the refusal invites, a
    /// few seconds later ([`ENSURE_RETRY_AFTER_ERROR`]), asks again and finds
    /// the tray up, so its question shows.
    ///
    /// Red before #3884: the refusal stood [`ENSURE_COOLDOWN`], 20 s, and that
    /// prompt was answered `false` from it, with no look.
    #[test]
    fn a_prompt_after_a_start_that_ran_out_of_time_finds_the_tray_it_brought_up() {
        let rt = runtime();
        let held = Held::default();
        let _opened = OpenedAtTheEnd(held.clone());
        let host = Host {
            held: Some(held.clone()),
            ..Host::default()
        };
        let stood = StandIn::new(host_steps(&host));
        let (up, waited) = ensure_on_a_worker(&rt, &stood, TRAY_START_WAIT + SLACK)
            .expect("a start that runs out of time is answered within its wait");
        assert!(!up, "a start that ran out of time shows no question");
        assert!(
            waited >= TRAY_START_WAIT,
            "the start was waited for: {waited:?}"
        );
        // The start goes on, and brings the tray up.
        held.open();
        let opened = Instant::now();
        while !host.up() {
            assert!(
                opened.elapsed() < RUNS_WITHIN,
                "the late start never brought the tray up"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(ENSURE_RETRY_AFTER_ERROR);
        let (up, _) =
            ensure_on_a_worker(&rt, &stood, RUNS_WITHIN).expect("the next prompt is answered");
        assert!(
            up,
            "a prompt a few seconds later finds the tray the late start brought up"
        );
        assert_eq!(
            host.starts(),
            2,
            "it asked again: the start that ran out of time, then its own"
        );
    }

    /// #3884 — with nobody at the screen the start fails. A prompt right
    /// after it is answered from that verdict, with no start of its own: a
    /// failure is still rate-limited. A few seconds on
    /// ([`ENSURE_RETRY_AFTER_ERROR`]) a prompt asks again, finds the person
    /// who signed in meanwhile, and the tray is started for them.
    ///
    /// Red before #3884: the failure stood 20 s.
    #[test]
    fn a_failed_start_is_tried_again_after_a_few_seconds_not_per_prompt() {
        let rt = runtime();
        let host = Host::default();
        host.nobody.store(true, SeqCst);
        let stood = StandIn::new(host_steps(&host));
        let ask = || {
            ensure_on_a_worker(&rt, &stood, RUNS_WITHIN)
                .expect("a prompt is answered")
                .0
        };
        assert!(!ask(), "nobody at the screen: no question shows");
        assert_eq!(host.starts(), 1);
        assert!(!ask(), "a prompt at once is answered from that start");
        assert_eq!(host.starts(), 1, "with no start of its own");
        host.nobody.store(false, SeqCst);
        std::thread::sleep(ENSURE_RETRY_AFTER_ERROR);
        assert!(
            ask(),
            "a prompt a few seconds later finds the person who signed in"
        );
        assert_eq!(host.starts(), 2, "it asked again");
        assert!(host.up(), "and the tray was started for them");
    }

    /// #3884 — the control: what cannot change soon stands
    /// [`ENSURE_COOLDOWN`], past a failure's few seconds, and no prompt in it
    /// starts anything. On a host with no companion the answer stays `false`,
    /// and with the tray up it stays `true`.
    #[test]
    fn no_companion_and_a_tray_that_is_up_stand_with_no_start_per_prompt() {
        let rt = runtime();
        let none = Host::default();
        none.not_installed.store(true, SeqCst);
        let none_stood = StandIn::new(host_steps(&none));
        let up = Host::default();
        up.up.store(true, SeqCst);
        let up_stood = StandIn::new(host_steps(&up));
        let ask = |stood: &StandIn| {
            ensure_on_a_worker(&rt, stood, RUNS_WITHIN)
                .expect("a prompt is answered")
                .0
        };
        assert!(!ask(&none_stood), "no companion: no question shows");
        assert!(ask(&up_stood), "the tray is up: the question shows");
        std::thread::sleep(ENSURE_RETRY_AFTER_ERROR + SLACK);
        assert!(!ask(&none_stood), "still no companion");
        assert!(ask(&up_stood), "still up");
        assert_eq!(
            none.starts(),
            1,
            "no companion is not looked for again per prompt"
        );
        assert_eq!(
            up.starts(),
            1,
            "a tray that is up is not looked for again per prompt"
        );
    }

    /// An RC prompt's question, open for 30 s.
    fn rc_view(session_hex: &str) -> crate::indicator::PromptView {
        crate::indicator::PromptView {
            session_hex: session_hex.into(),
            title: "Remote control request".into(),
            lead: "Alice is requesting to control this device.".into(),
            detail: String::new(),
            permissions: "VIEW|INPUT".into(),
            org: String::new(),
            expires_at: Instant::now() + Duration::from_secs(30),
        }
    }

    /// #3888 — while the tray's start is held, the WS loop handles its next
    /// frame at once: it starts the companion's start ([`start_for_prompt`])
    /// and hands it to the consent task, which hears within the start's wait
    /// that no companion came up. The loop and the consent task are modelled
    /// as they run, as #3857's tests model them: a frame is handled before the
    /// next one is read.
    ///
    /// Red before #3888: the loop waited for the start, up to its 2.5 s wait,
    /// and read no frame meanwhile.
    #[test]
    fn the_loop_handles_its_next_frame_while_the_tray_start_is_held() {
        let rt = runtime();
        let held = Held::default();
        let _opened = OpenedAtTheEnd(held.clone());
        let host = Host {
            held: Some(held.clone()),
            ..Host::default()
        };
        let stood = StandIn::new(host_steps(&host));
        let (frames, mut frames_rx) = tokio::sync::mpsc::channel::<Option<u32>>(4);
        let (handled_tx, handled) = std::sync::mpsc::channel::<(u32, Instant)>();
        let (verdict_tx, verdict) = std::sync::mpsc::channel::<(bool, Duration)>();
        rt.spawn(asking(stood, async move {
            // The WS loop, as far as it goes here. `None` is an RC request
            // whose question no native panel shows.
            while let Some(frame) = frames_rx.recv().await {
                let Some(n) = frame else {
                    let asked = Instant::now();
                    let starting = start_for_prompt().await;
                    // The consent task.
                    let verdict_tx = verdict_tx.clone();
                    tokio::spawn(async move {
                        let _ = verdict_tx.send((starting.up().await, asked.elapsed()));
                    });
                    continue;
                };
                let _ = handled_tx.send((n, Instant::now()));
            }
        }));
        frames.blocking_send(None).expect("the loop reads frames");
        let asked = Instant::now();
        while held.began() == 0 {
            assert!(
                asked.elapsed() < RUNS_WITHIN,
                "no start reached the held step"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let sent = Instant::now();
        frames
            .blocking_send(Some(7))
            .expect("the loop reads frames");
        let (n, at) = handled
            .recv_timeout(crate::signaling::COMPANION_START_BUDGET + SLACK)
            .expect("a frame sent while the tray's start is held is handled");
        assert_eq!(n, 7);
        assert!(
            at.duration_since(sent) < AT_ONCE,
            "the frame waited {:?} behind the tray's start",
            at.duration_since(sent)
        );
        let (up, waited) = verdict
            .recv_timeout(crate::signaling::COMPANION_START_BUDGET + SLACK)
            .expect("the consent task hears within the start's budget");
        assert!(!up, "a start held past its wait is no companion");
        assert!(
            waited >= TRAY_START_WAIT,
            "the verdict came in {waited:?}, before the start's wait"
        );
    }

    /// #3888 — a start that fails, or runs out of its wait, is still
    /// `no_prompt_surface` once the window is out, never a timeout: the
    /// consent task waits for its verdict before the window starts
    /// ([`crate::signaling::rc_surfaces_up`]). The control beside them: a
    /// start that works is a surface, and the window's end is then the
    /// person's timeout.
    #[test]
    fn a_start_that_fails_or_runs_out_of_time_is_still_no_prompt_surface() {
        use crate::consent::Decision;
        use roomler_ai_remote_control::consent::ConsentDenyReason;
        let rt = runtime();
        let held = Held::default();
        let _opened = OpenedAtTheEnd(held.clone());
        let ran_out = Host {
            held: Some(held.clone()),
            ..Host::default()
        };
        let failed = Host::default();
        failed.nobody.store(true, SeqCst);
        let works = Host::default();
        // No notification: the companion's verdict alone decides.
        let ind = crate::indicator::ViewerIndicator::disabled();
        let reason_at_the_windows_end = |host: &Host| {
            let ind = ind.clone();
            rt.block_on(asking(StandIn::new(host_steps(host)), async move {
                // The loop's side.
                let surfaces = crate::signaling::RcSurfaces {
                    native: false,
                    marker: true,
                    behind: Some((start_for_prompt().await, rc_view("f6"))),
                    mark_up: None,
                };
                // The consent task's.
                let (outbound, _inbox) = tokio::sync::mpsc::channel(1);
                let have_surface =
                    crate::signaling::rc_surfaces_up(surfaces, &ind, || true, "f6", &outbound)
                        .await;
                crate::signaling::consent_reply(Decision::Timeout, have_surface)
                    .and_then(|reply| reply.reason)
            }))
        };
        assert_eq!(
            reason_at_the_windows_end(&ran_out),
            Some(ConsentDenyReason::NoPromptSurface),
            "a start that ran out of time: nobody could be asked"
        );
        assert_eq!(
            reason_at_the_windows_end(&failed),
            Some(ConsentDenyReason::NoPromptSurface),
            "a start that failed: nobody could be asked"
        );
        assert_eq!(
            reason_at_the_windows_end(&works),
            Some(ConsentDenyReason::HostTimeout),
            "a start that worked: nobody answered"
        );
    }

    /// #3864 — on a one-row Mac the root daemon asks, and the tray reads the
    /// worker's socket, so a tray that is up is the question's surface only
    /// while the road between them is open: a worker that said
    /// `relay-consent`, for a known account, whose person socket answers in
    /// time. Closed, it is not counted, and a window nobody answered ends as
    /// `no_prompt_surface`: the floor every host has, nothing shared without
    /// an answer. The control: a daemon that is not a one-row Mac's root (a
    /// two-row Mac's halves, Linux, Windows) counts its tray as before.
    ///
    /// Red before #3864: a tray that was up was counted whatever the road,
    /// and the window's end read as the person's timeout.
    #[test]
    fn a_one_row_tray_is_the_surface_only_while_its_road_is_open() {
        use crate::consent::Decision;
        use crate::consent_relay::Worker;
        use crate::consent_relay::stand_in::{Answers, Road};
        use roomler_ai_remote_control::consent::ConsentDenyReason;
        let rt = runtime();
        let works = Host::default();
        let ind = crate::indicator::ViewerIndicator::disabled();
        let reason_at_the_windows_end = |road: Option<Road>| {
            let ind = ind.clone();
            let stood = StandIn::new(host_steps(&works));
            let stood = match road {
                Some(road) => stood.on_one_row(road),
                None => stood,
            };
            rt.block_on(asking(stood, async move {
                let surfaces = crate::signaling::RcSurfaces {
                    native: false,
                    marker: true,
                    behind: Some((start_for_prompt().await, rc_view("h8"))),
                    mark_up: None,
                };
                let (outbound, _inbox) = tokio::sync::mpsc::channel(1);
                let have_surface =
                    crate::signaling::rc_surfaces_up(surfaces, &ind, || true, "h8", &outbound)
                        .await;
                crate::signaling::consent_reply(Decision::Timeout, have_surface)
                    .and_then(|reply| reply.reason)
            }))
        };
        let relaying = Worker {
            relays: true,
            uid: Some(501),
        };
        let open = Road {
            worker: relaying,
            answers: Answers::Yes,
        };
        assert_eq!(
            reason_at_the_windows_end(None),
            Some(ConsentDenyReason::HostTimeout),
            "not a one-row Mac's root: the tray is the surface, as before"
        );
        assert_eq!(
            reason_at_the_windows_end(Some(open)),
            Some(ConsentDenyReason::HostTimeout),
            "the road open: the tray shows the question, and nobody answered"
        );
        for (road, what) in [
            (
                Road {
                    worker: Worker {
                        relays: false,
                        ..relaying
                    },
                    ..open
                },
                "a worker that did not say relay-consent (or none)",
            ),
            (
                Road {
                    worker: Worker {
                        uid: None,
                        ..relaying
                    },
                    ..open
                },
                "a worker whose account is not known",
            ),
            (
                Road {
                    answers: Answers::No,
                    ..open
                },
                "a person socket that does not answer (gone)",
            ),
            (
                Road {
                    answers: Answers::Never,
                    ..open
                },
                "a person socket that never answers (stalled)",
            ),
        ] {
            assert_eq!(
                reason_at_the_windows_end(Some(road)),
                Some(ConsentDenyReason::NoPromptSurface),
                "{what}: nobody could be asked"
            );
        }
    }

    /// #3888 — a question withdrawn while the tray's start is held (a
    /// `Terminate` the loop reads meanwhile, which finds nothing on the screen
    /// to take down) gets no notification once the start runs out of time: no
    /// show starts for it, so logind is not even asked, and nothing comes up.
    ///
    /// The loop used to read no frame while the start ran, so no question
    /// could end then.
    #[cfg(all(target_os = "linux", feature = "portal-capture"))]
    #[test]
    fn a_question_withdrawn_while_the_tray_start_is_held_gets_no_notification() {
        use super::session_walk_stand_in as walk;
        let rt = runtime();
        let held = Held::default();
        let _opened = OpenedAtTheEnd(held.clone());
        let host = Host {
            held: Some(held.clone()),
            ..Host::default()
        };
        let logind = walk::Held::default();
        let _logind_opened = walk::OpenedAtTheEnd(logind.clone());
        let (kill_tx, _disconnects) = tokio::sync::mpsc::channel(1);
        let (consent_tx, _answers) = tokio::sync::mpsc::channel(4);
        let ind = crate::indicator::ViewerIndicator::new(
            kill_tx,
            crate::rc_sessions::RcSessionRegistry::new(),
            consent_tx,
        )
        .expect("an indicator");
        let withdrawn = Arc::new(AtomicBool::new(false));
        let (verdict_tx, verdict) = std::sync::mpsc::channel::<bool>();
        {
            let (ind, withdrawn) = (ind.clone(), withdrawn.clone());
            let walking = walk::StandIn::held(&logind, walk::ana());
            rt.spawn(asking(
                StandIn::new(host_steps(&host)),
                walk::asking(walking, async move {
                    let surfaces = crate::signaling::RcSurfaces {
                        native: false,
                        marker: true,
                        behind: Some((start_for_prompt().await, rc_view("g7"))),
                        mark_up: None,
                    };
                    let (outbound, _inbox) = tokio::sync::mpsc::channel(1);
                    let have_surface = crate::signaling::rc_surfaces_up(
                        surfaces,
                        &ind,
                        move || !withdrawn.load(SeqCst),
                        "g7",
                        &outbound,
                    )
                    .await;
                    let _ = verdict_tx.send(have_surface);
                }),
            ));
        }
        let asked = Instant::now();
        while held.began() == 0 {
            assert!(
                asked.elapsed() < RUNS_WITHIN,
                "no start reached the held step"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        // The `Terminate`: the question off the books, and its prompts taken
        // down, of which none is up yet.
        withdrawn.store(true, SeqCst);
        ind.hide_prompt("g7");
        let have_surface = verdict
            .recv_timeout(TRAY_START_WAIT + SLACK)
            .expect("the consent task hears once the start runs out of time");
        assert!(!have_surface, "a question that is over has no surface");
        logind.open();
        std::thread::sleep(AT_ONCE);
        assert_eq!(
            logind.began(),
            0,
            "no notification was started for a question that is over"
        );
        assert!(!ind.notification_is_up("g7"), "nothing is up for it");
    }

    /// The control on Linux: a start in time does what it always did, and says it is for a
    /// prompt (`--for-prompt`, #3887). A root daemon starts the tray through the system manager
    /// as the person, with the session's allowlisted environment; a per-user daemon through its
    /// own manager, without reading it. A running tray, no companion, nobody at the screen and a
    /// failed systemd-run answer as before; only a systemd-run that exited 0 is `Started`.
    #[cfg(all(unix, not(target_os = "macos")))]
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
                "--for-prompt",
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
                "--for-prompt",
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

    /// The control on Windows: a start whose steps answer in time does what
    /// it always did. A SYSTEM service starts the tray into the console
    /// session as its person, and a daemon that runs as the person a plain
    /// start in its own session, each from the companion beside the daemon
    /// with `--for-prompt` (#3887). A tray already running, no companion, a
    /// daemon that cannot find itself, nobody signed in and a refused start
    /// answer as before, and only a start that created the process is
    /// `Started` (#3824).
    #[cfg(target_os = "windows")]
    #[test]
    fn a_start_in_time_works_as_today() {
        let rt = runtime();
        let held = Held::default();
        let ask = |taken: TraySteps| {
            start_on_a_worker(&rt, &StandIn::new(taken), RUNS_WITHIN)
                .expect("a start in time is answered")
                .0
        };
        let call = |ran: &[&str]| -> Vec<String> { ran.iter().map(|a| a.to_string()).collect() };

        let ran = Ran::default();
        assert_eq!(ask(steps(None, &held, &ran)), Ok(EnsureOutcome::Started));
        assert_eq!(
            ran.argvs(),
            vec![call(&["SystemService", DEST, "--for-prompt"])],
            "a SYSTEM service: into the console session, the companion beside it, for a prompt"
        );

        let ran = Ran::default();
        let mut own = steps(None, &held, &ran);
        own.context = Arc::new(|| RespawnContext::UserSession);
        assert_eq!(ask(own), Ok(EnsureOutcome::Started));
        assert_eq!(
            ran.argvs(),
            vec![call(&["UserSession", DEST, "--for-prompt"])],
            "the person's own daemon: a plain start in its own session, for a prompt"
        );

        let ran = Ran::default();
        let mut running = steps(None, &held, &ran);
        running.running = Arc::new(|| true);
        assert_eq!(ask(running), Ok(EnsureOutcome::AlreadyRunning));
        assert!(
            ran.argvs().is_empty(),
            "a running tray is not started again"
        );

        let ran = Ran::default();
        let mut not_installed = steps(None, &held, &ran);
        not_installed.path = Arc::new(|| -> Result<Option<std::path::PathBuf>> { Ok(None) });
        assert_eq!(ask(not_installed), Ok(EnsureOutcome::Unsupported));
        assert!(ran.argvs().is_empty());

        let ran = Ran::default();
        let mut lost = steps(None, &held, &ran);
        lost.path = Arc::new(|| -> Result<Option<std::path::PathBuf>> {
            Err(
                anyhow::anyhow!("The system cannot find the file specified. (os error 2)")
                    .context("locating own exe"),
            )
        });
        assert_eq!(
            ask(lost),
            Err("locating own exe: The system cannot find the file specified. (os error 2)".into())
        );
        assert!(ran.argvs().is_empty());

        let ran = Ran::default();
        let mut nobody = steps(None, &held, &ran);
        nobody.start = Arc::new(|_: RespawnContext, _: &std::path::Path, _: &[&str]| {
            TrayStart::NobodySignedIn
        });
        assert_eq!(
            ask(nobody),
            Err("nobody is signed in at this computer's screen".into()),
            "nobody to start it for is no start"
        );

        let ran = Ran::default();
        let mut refused = steps(None, &held, &ran);
        refused.start = Arc::new(|_: RespawnContext, _: &std::path::Path, _: &[&str]| {
            TrayStart::Failed(
                r"CreateProcessAsUserW(C:\Program Files\Roomler\roomler-desktop.exe) failed (err 5)"
                    .into(),
            )
        });
        assert_eq!(
            ask(refused),
            Err(
                r"CreateProcessAsUserW(C:\Program Files\Roomler\roomler-desktop.exe) failed (err 5)"
                    .into()
            ),
            "only a start that created the process is a start"
        );
    }

    /// The control on macOS: a start whose steps answer in time does what it
    /// always did. launchd starts the LaunchAgent in the console user's GUI
    /// domain (`launchctl kickstart -k gui/<uid>/com.roomler.desktop`). A tray
    /// already running, no app installed, the login window at the screen and
    /// a refused kickstart answer as before, and only a launchctl that exited
    /// 0 is `Started`.
    #[cfg(target_os = "macos")]
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
                "launchctl",
                "kickstart",
                "-k",
                "gui/501/com.roomler.desktop"
            ])],
            "launchd starts the LaunchAgent in the console user's GUI domain"
        );

        let ran = Ran::default();
        let mut running = steps(None, &held, &ran);
        running.running = Arc::new(|| true);
        assert_eq!(ask(running), Ok(EnsureOutcome::AlreadyRunning));
        assert!(
            ran.argvs().is_empty(),
            "a running tray is not started again"
        );

        let ran = Ran::default();
        let mut not_installed = steps(None, &held, &ran);
        not_installed.installed = Arc::new(|| false);
        assert_eq!(ask(not_installed), Ok(EnsureOutcome::Unsupported));
        assert!(ran.argvs().is_empty());

        let ran = Ran::default();
        let mut login_window = steps(None, &held, &ran);
        login_window.console = Arc::new(|| console_owner_person("root\n"));
        assert_eq!(
            ask(login_window),
            Err(
                "resolving the console user: nobody is signed in at this Mac's screen (the login \
                 window is showing)"
                    .into()
            )
        );
        assert!(ran.argvs().is_empty());

        let ran = Ran::default();
        let mut refused = steps(None, &held, &ran);
        refused.launchctl = {
            let ran = ran.clone();
            Arc::new(
                move |cmd: std::process::Command| -> std::io::Result<std::process::Output> {
                    ran.record(&cmd);
                    Ok(exited(
                        113,
                        "Could not find service \"com.roomler.desktop\" in domain for user gui: 501\n",
                    ))
                },
            )
        };
        assert_eq!(
            ask(refused),
            Err(
                "launchctl kickstart gui/501/com.roomler.desktop failed: Could not find service \
                 \"com.roomler.desktop\" in domain for user gui: 501"
                    .into()
            ),
            "only a launchctl that exited 0 is a start"
        );
        assert_eq!(ran.argvs().len(), 1);
    }

    /// Linux's steps, stood in.
    #[cfg(all(unix, not(target_os = "macos")))]
    mod on_linux {
        use super::*;

        /// Where the stood-in companion is installed.
        pub(super) const COMPANION: &str = "/usr/bin/roomler-desktop";

        /// The step a test holds up.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(super) enum Step {
            /// logind, asked for the graphical session.
            Session,
            /// The session's environment: the account database, then the
            /// person's user manager.
            Environment,
            /// systemd-run itself.
            SystemdRun,
        }

        /// Each step a test holds up, and a word its refusal names it by.
        pub(super) const HELD: [(Step, &str); 3] = [
            (Step::Session, "logind"),
            (Step::Environment, "user manager"),
            (Step::SystemdRun, "systemd-run"),
        ];

        /// The person at the screen, as logind reports them.
        fn ana() -> GraphicalSession {
            GraphicalSession {
                uid: 4242,
                name: "ana".into(),
                display: Some(":0".into()),
                wayland_display: None,
            }
        }

        /// A root daemon's start on a host with the companion installed, the
        /// person at the screen and their tray not running. Every step
        /// answers at once, except `hold`'s, which waits for `held`.
        /// systemd-run's argv goes to `ran`, and it exits 0.
        pub(super) fn steps(hold: Option<Step>, held: &Held, ran: &Ran) -> TraySteps {
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

        /// #3884, #3888 — a root daemon's start on `host`: its install path
        /// (each start's first step, counted), logind's session, the tray
        /// check and systemd-run, which waits for `host.held` and brings the
        /// tray up.
        pub(super) fn host_steps(host: &Host) -> TraySteps {
            let (at_path, at_session, at_running, at_run) =
                (host.clone(), host.clone(), host.clone(), host.clone());
            TraySteps {
                path: Arc::new(move || -> Option<std::path::PathBuf> {
                    at_path.starts.fetch_add(1, SeqCst);
                    (!at_path.not_installed.load(SeqCst)).then(|| COMPANION.into())
                }),
                session: Arc::new(move || -> Result<GraphicalSession> {
                    if at_session.nobody.load(SeqCst) {
                        anyhow::bail!(
                            "no active graphical session — nobody is at this machine's screen"
                        );
                    }
                    Ok(ana())
                }),
                running: Arc::new(move |_: u32| at_running.up()),
                environment: Arc::new(|_: &GraphicalSession| -> Option<String> { None }),
                systemd_run: Arc::new(
                    move |_: std::process::Command| -> std::io::Result<std::process::Output> {
                        if let Some(held) = &at_run.held {
                            held.hold();
                        }
                        at_run.up.store(true, SeqCst);
                        Ok(exited(0, ""))
                    },
                ),
                user_manager: false,
            }
        }
    }

    /// Windows' steps, stood in.
    #[cfg(target_os = "windows")]
    mod on_windows {
        use super::*;

        /// Where the stood-in companion is installed, beside the daemon.
        pub(super) const DEST: &str = r"C:\Program Files\Roomler\roomler-desktop.exe";

        /// The step a test holds up.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(super) enum Step {
            /// The process list, asked whether the tray runs.
            Running,
            /// The start into the console session.
            Start,
        }

        /// Each step a test holds up, and a word its refusal names it by.
        pub(super) const HELD: [(Step, &str); 2] = [
            (Step::Running, "process list"),
            (Step::Start, "CreateProcessAsUserW"),
        ];

        /// A SYSTEM service's start on a host with the companion installed
        /// beside it, a person signed in at the console and their tray not
        /// running. Every step answers at once, except `hold`'s, which waits
        /// for `held`. The start's context, path and arguments go to `ran`,
        /// and it creates the process.
        pub(super) fn steps(hold: Option<Step>, held: &Held, ran: &Ran) -> TraySteps {
            let holds = move |step: Step, held: &Held| {
                if hold == Some(step) {
                    held.hold();
                }
            };
            let (at_running, at_start) = (held.clone(), held.clone());
            let ran = ran.clone();
            TraySteps {
                running: Arc::new(move || {
                    holds(Step::Running, &at_running);
                    false
                }),
                path: Arc::new(|| -> Result<Option<std::path::PathBuf>> { Ok(Some(DEST.into())) }),
                context: Arc::new(|| RespawnContext::SystemService),
                start: Arc::new(
                    move |context: RespawnContext, path: &std::path::Path, args: &[&str]| {
                        holds(Step::Start, &at_start);
                        let mut call = vec![format!("{context:?}"), path.display().to_string()];
                        call.extend(args.iter().map(|a| a.to_string()));
                        ran.push(call);
                        TrayStart::Started {
                            pid: 4242,
                            session: Some(1),
                        }
                    },
                ),
            }
        }

        /// #3884, #3888 — a SYSTEM service's start on `host`: the tray check
        /// (each start's first step, counted), the install path, and the
        /// start into the console session, which waits for `host.held`, finds
        /// nobody signed in or brings the tray up.
        pub(super) fn host_steps(host: &Host) -> TraySteps {
            let (at_running, at_path, at_start) = (host.clone(), host.clone(), host.clone());
            TraySteps {
                running: Arc::new(move || {
                    at_running.starts.fetch_add(1, SeqCst);
                    at_running.up()
                }),
                path: Arc::new(move || -> Result<Option<std::path::PathBuf>> {
                    Ok((!at_path.not_installed.load(SeqCst)).then(|| DEST.into()))
                }),
                context: Arc::new(|| RespawnContext::SystemService),
                start: Arc::new(move |_: RespawnContext, _: &std::path::Path, _: &[&str]| {
                    if let Some(held) = &at_start.held {
                        held.hold();
                    }
                    if at_start.nobody.load(SeqCst) {
                        return TrayStart::NobodySignedIn;
                    }
                    at_start.up.store(true, SeqCst);
                    TrayStart::Started {
                        pid: 4242,
                        session: Some(1),
                    }
                }),
            }
        }
    }

    /// macOS' steps, stood in.
    #[cfg(target_os = "macos")]
    mod on_mac {
        use super::*;

        /// The step a test holds up.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(super) enum Step {
            /// The process list (`pgrep`).
            Running,
            /// The console's owner (`stat /dev/console`).
            Console,
            /// The account database (`id -u`).
            Account,
            /// launchd (`launchctl kickstart`).
            Launchctl,
        }

        /// Each step a test holds up, and a word its refusal names it by.
        pub(super) const HELD: [(Step, &str); 4] = [
            (Step::Running, "pgrep"),
            (Step::Console, "/dev/console"),
            (Step::Account, "account database"),
            (Step::Launchctl, "launchctl"),
        ];

        /// The start on a Mac with the app installed, `ana` (uid 501) at the
        /// screen and no tray running. Every step answers at once, except
        /// `hold`'s, which waits for `held`. launchctl's argv goes to `ran`,
        /// and it exits 0.
        pub(super) fn steps(hold: Option<Step>, held: &Held, ran: &Ran) -> TraySteps {
            let holds = move |step: Step, held: &Held| {
                if hold == Some(step) {
                    held.hold();
                }
            };
            let (at_running, at_console, at_account, at_launchctl) =
                (held.clone(), held.clone(), held.clone(), held.clone());
            let ran = ran.clone();
            TraySteps {
                installed: Arc::new(|| true),
                running: Arc::new(move || {
                    holds(Step::Running, &at_running);
                    false
                }),
                console: Arc::new(move || -> Result<String> {
                    holds(Step::Console, &at_console);
                    Ok("ana".into())
                }),
                uid: Arc::new(move |user: &str| -> Result<u32> {
                    holds(Step::Account, &at_account);
                    match user {
                        "ana" => Ok(501),
                        other => anyhow::bail!("no such account: {other}"),
                    }
                }),
                launchctl: Arc::new(
                    move |cmd: std::process::Command| -> std::io::Result<std::process::Output> {
                        holds(Step::Launchctl, &at_launchctl);
                        ran.record(&cmd);
                        Ok(exited(0, ""))
                    },
                ),
            }
        }

        /// #3884, #3888 — the start on a Mac that is `host`: the app (each
        /// start's first step, counted), the tray check, the console's owner
        /// (the login window while nobody is signed in), and launchctl, which
        /// waits for `host.held` and brings the tray up.
        pub(super) fn host_steps(host: &Host) -> TraySteps {
            let (at_installed, at_running, at_console, at_launchctl) =
                (host.clone(), host.clone(), host.clone(), host.clone());
            TraySteps {
                installed: Arc::new(move || {
                    at_installed.starts.fetch_add(1, SeqCst);
                    !at_installed.not_installed.load(SeqCst)
                }),
                running: Arc::new(move || at_running.up()),
                console: Arc::new(move || -> Result<String> {
                    if at_console.nobody.load(SeqCst) {
                        return console_owner_person("root\n");
                    }
                    Ok("ana".into())
                }),
                uid: Arc::new(|_: &str| -> Result<u32> { Ok(501) }),
                launchctl: Arc::new(
                    move |_: std::process::Command| -> std::io::Result<std::process::Output> {
                        if let Some(held) = &at_launchctl.held {
                            held.hold();
                        }
                        at_launchctl.up.store(true, SeqCst);
                        Ok(exited(0, ""))
                    },
                ),
            }
        }
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
///
/// #3857 — `Clone`: one walk's answer is shared by every caller that asked
/// while it ran ([`graphical_session_within`]).
#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Debug, Clone, PartialEq, Eq)]
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
    // every startup after the first successful swap). #3850: a marker that
    // does not name this version is checked against the EXE's own version
    // resource, and an EXE already at this version is not swapped again. Off
    // the async worker, LOOK_WAIT at most.
    let look = {
        let (dest, marker) = (dest.clone(), marker.clone());
        tokio::task::spawn_blocking(move || look_beside(own_version, &dest, &marker))
    };
    let found = tokio::time::timeout(LOOK_WAIT, look)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "reading the installed companion's version marker and version took longer than \
                 {LOOK_WAIT:?}"
            )
        })?
        .context("reading the installed companion's version")?;
    let Installed::Stale {
        recorded,
        its_version,
    } = found
    else {
        return Ok(None);
    };
    tracing::info!(
        own = own_version,
        recorded = recorded.as_deref().unwrap_or("<none>"),
        installed = its_version.as_deref().unwrap_or("<not known>"),
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
/// ([`restore_lost_tray`]). One read of a flag when none was lost. #3844: in
/// the session it was lost in, one line per session.
#[cfg(target_os = "windows")]
async fn bring_back_lost_tray(respawn: RespawnContext, pass_stopped_a_tray: bool) {
    if !TRAY_LOST.is_set() {
        return;
    }
    let Some(dest) = sibling_desktop() else {
        return;
    };
    // A registry read, a process list and a start, each start bounded
    // (`start_again`): the blocking pool.
    let lines = tokio::task::spawn_blocking(move || {
        restore_lost_tray(pass_stopped_a_tray, &TRAY_LOST, |at| {
            start_lost_tray(respawn, &dest, at)
        })
    })
    .await;
    const STILL_DOWN: &str =
        "desktop companion is still not running after a refresh stopped it; it is tried again";
    let lines = match lines {
        Ok(lines) => lines,
        Err(e) => {
            tracing::warn!(error = %e, "desktop companion: the lost tray's start did not run");
            return;
        }
    };
    for line in lines {
        match line {
            LostTrayLine::Back { at, how } => tracing::info!(
                session = %session_words(respawn, at),
                how = %how,
                "desktop companion is back after a refresh stopped it"
            ),
            LostTrayLine::StillDown {
                at,
                why,
                first: true,
            } => tracing::warn!(session = %session_words(respawn, at), why = %why, "{STILL_DOWN}"),
            LostTrayLine::StillDown {
                at,
                why,
                first: false,
            } => tracing::debug!(session = %session_words(respawn, at), why = %why, "{STILL_DOWN}"),
            LostTrayLine::SignedOut { at } => tracing::info!(
                session = %session_words(respawn, at),
                "desktop companion is not started again after a refresh stopped it: nobody is \
                 signed in to that session any more, and the sign-in start owns their next tray"
            ),
        }
    }
}

/// #3844 — where a start goes, in a log line's `session=`: the session's
/// number, or where the start has always gone.
#[cfg(target_os = "windows")]
fn session_words(respawn: RespawnContext, at: Option<u32>) -> String {
    match (at, respawn) {
        (Some(session), _) => session.to_string(),
        (None, RespawnContext::SystemService) => "the console's".to_string(),
        (None, RespawnContext::UserSession) => "the daemon's own".to_string(),
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
    /// Running trays started from the installed path, whatever their file
    /// has been renamed to since, each with its session when known (#3844).
    fn running(&mut self) -> Vec<RunningTray>;
    /// Stop those pids — each re-verified on the handle that kills it — and
    /// return the ones confirmed exited.
    fn stop(&mut self, pids: &[u32]) -> Vec<u32>;
    fn write_marker(&mut self);
    /// Who starts the trays again ([`restart_plan`]).
    fn respawn(&self) -> RespawnContext;
    /// Start the tray at the installed path (whichever EXE is there), as the
    /// refresh always has: no arguments. #3844: in session `at`, with that
    /// session's own person's token; `None` where the start has always gone.
    fn start(&mut self, at: Option<u32>) -> TrayStart;
    /// Remove the copies moved aside, best-effort: this swap's, and any an
    /// earlier swap left.
    fn remove_old(&mut self);
}

/// #3844 — a running tray the swap found: its pid, and its Terminal Services
/// session when that could be read.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunningTray {
    pub(crate) pid: u32,
    pub(crate) session: Option<u32>,
}

/// #3844 — one start of the tray after a swap stopped it: where, and what
/// came of it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrayRestart {
    /// The session it was started in, or `None` where the start has always
    /// gone ([`restart_plan`]).
    pub(crate) at: Option<u32>,
    pub(crate) start: TrayStart,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl TrayRestart {
    /// Did it leave a tray where one was stopped, or nobody to leave one for?
    /// Anything else is a tray lost ([`TrayLost`]).
    fn settled(&self) -> bool {
        matches!(self.start, TrayStart::Started { .. } | TrayStart::SignedOut)
    }

    /// What came of it, for a log line, naming the session.
    fn describe(&self) -> String {
        match (&self.start, self.at) {
            (started @ TrayStart::Started { .. }, _) => {
                format!("respawned ({})", started.describe())
            }
            (not_started, Some(session)) => {
                format!("session {session}: {}", not_started.describe())
            }
            (not_started, None) => not_started.describe(),
        }
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwapOutcome {
    /// The new EXE is installed, and the marker names it unless it could not
    /// be written (#3850).
    Swapped {
        /// The trays this swap stopped: every one that ran.
        stopped: Vec<u32>,
        /// Their starts, one per session ([`restart_plan`]); none when none
        /// was stopped.
        starts: Vec<TrayRestart>,
    },
    /// The marker is as it was, and so is the installed EXE unless `reason`
    /// says putting it back failed.
    Aborted {
        reason: String,
        /// The trays it stopped before it gave up.
        stopped: Vec<u32>,
        /// Their starts; none when none was stopped.
        starts: Vec<TrayRestart>,
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
        starts: Vec::new(),
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
    let running = ops.running();
    let pids: Vec<u32> = running.iter().map(|tray| tray.pid).collect();
    let stopped = if pids.is_empty() {
        Vec::new()
    } else {
        ops.stop(&pids)
    };
    // #3844 — the trays that stopped, each with the session it ran in.
    let stopped_trays: Vec<RunningTray> = running
        .iter()
        .filter(|tray| stopped.contains(&tray.pid))
        .copied()
        .collect();
    if stopped.len() != pids.len() {
        let survived = format!(
            "could not stop every running companion (running {pids:?}, stopped {stopped:?})"
        );
        let reason = match ops.restore_old() {
            Ok(()) => survived,
            Err(back) => format!("{survived}; putting the old one back failed: {back:#}"),
        };
        let starts = start_stopped(ops, &stopped_trays, lost);
        return SwapOutcome::Aborted {
            reason,
            stopped,
            starts,
        };
    }
    ops.write_marker();
    let starts = start_stopped(ops, &stopped_trays, lost);
    ops.remove_old();
    SwapOutcome::Swapped { stopped, starts }
}

/// #3824 — start what the swap stopped, and record whether that brought the
/// tray back. #3844: one start per session one was stopped in
/// ([`restart_plan`]), and the loss recorded per session.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn start_stopped<O: SwapOps>(
    ops: &mut O,
    stopped: &[RunningTray],
    lost: &TrayLost,
) -> Vec<TrayRestart> {
    restart_plan(ops.respawn(), stopped)
        .into_iter()
        .map(|at| {
            let restart = TrayRestart {
                at,
                start: ops.start(at),
            };
            if restart.settled() {
                lost.clear(at);
            } else {
                lost.set(at);
            }
            restart
        })
        .collect()
}

/// #3844 — where the trays a swap stopped start again. For a service, once
/// in each session one of them ran in, in order, and nowhere else; a tray
/// whose session could not be read starts where the start has always gone
/// (`None`: the console session's person). A daemon that runs as the person
/// starts one, in its own session, as before. None stopped, none started.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn restart_plan(respawn: RespawnContext, stopped: &[RunningTray]) -> Vec<Option<u32>> {
    if stopped.is_empty() {
        return Vec::new();
    }
    match respawn {
        RespawnContext::UserSession => vec![None],
        RespawnContext::SystemService => {
            let mut at: Vec<Option<u32>> = stopped.iter().map(|tray| tray.session).collect();
            // The sessions in order, then a session that could not be read.
            at.sort_by_key(|session| (session.is_none(), *session));
            at.dedup();
            at
        }
    }
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
    /// #3844 — nobody is signed in to the session a stopped tray ran in any
    /// more: its person signed out since, so there is nobody to start it for.
    /// No process was created.
    SignedOut,
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
            Self::SignedOut => {
                "nobody is signed in to that session any more, so none was started".to_string()
            }
            Self::Failed(e) => e.clone(),
        }
    }
}

/// #3824 — a tray this process's refresh stopped and could not start again.
///
/// Set when a swap stopped the tray and its start did not bring it back
/// (nobody signed in at the console to start it for counts); forgotten when a
/// start works or a tray is seen running. Only while it is set does the
/// refresh start a tray that is not running: a tray the person quit, or one
/// that never ran, stays as it is. In memory, on purpose: after a restart the
/// sign-in start (the `Roomler Desktop` Run value) owns the tray.
///
/// #3844 — per session: where each lost tray would start again
/// ([`TrayRestart::at`]), so it is tried again there and nowhere else. A
/// session whose person signed out since lost nothing: the sign-in start owns
/// their next tray.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Default)]
pub(crate) struct TrayLost {
    lost: std::sync::Mutex<std::collections::BTreeSet<Option<u32>>>,
    /// Tries since it was lost that did not bring it back: the first one is
    /// a warn line, the rest debug.
    failed_tries: std::sync::atomic::AtomicU32,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl TrayLost {
    pub(crate) const fn new() -> Self {
        Self {
            lost: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            failed_tries: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn places(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeSet<Option<u32>>> {
        self.lost
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn is_set(&self) -> bool {
        !self.places().is_empty()
    }

    /// Where the lost trays would start again, in order.
    pub(crate) fn lost_at(&self) -> Vec<Option<u32>> {
        self.places().iter().copied().collect()
    }

    /// A swap stopped the tray at `at`, and its start did not bring it back.
    fn set(&self, at: Option<u32>) {
        use std::sync::atomic::Ordering::SeqCst;
        self.failed_tries.store(0, SeqCst);
        self.places().insert(at);
    }

    /// A start at `at` worked, a tray runs there, or nobody is signed in
    /// there any more.
    fn clear(&self, at: Option<u32>) {
        use std::sync::atomic::Ordering::SeqCst;
        let mut lost = self.places();
        lost.remove(&at);
        if lost.is_empty() {
            self.failed_tries.store(0, SeqCst);
        }
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
    // #3844 — one part per session, each saying what came of its start.
    let said = |starts: &[TrayRestart]| {
        starts
            .iter()
            .map(TrayRestart::describe)
            .collect::<Vec<_>>()
            .join("; ")
    };
    match outcome {
        SwapOutcome::Swapped { starts, .. } if starts.is_empty() => RefreshLine::Refreshed {
            tray: "none was running".to_string(),
        },
        SwapOutcome::Swapped { starts, .. } if starts.iter().all(TrayRestart::settled) => {
            RefreshLine::Refreshed { tray: said(starts) }
        }
        SwapOutcome::Swapped { starts, .. } => RefreshLine::TrayNotBack {
            error: said(starts),
        },
        SwapOutcome::Aborted { reason, starts, .. } => RefreshLine::Skipped {
            error: if starts.is_empty() {
                format!("{reason} (no tray was stopped)")
            } else {
                format!("{reason} (the trays it had stopped: {})", said(starts))
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
    /// `session=` and `how=`.
    Back { at: Option<u32>, how: String },
    /// `desktop companion is still not running after a refresh stopped it;
    /// it is tried again`, with `session=` and `why=`: WARN for the first try
    /// that did not bring it back, DEBUG for the rest.
    StillDown {
        at: Option<u32>,
        why: String,
        first: bool,
    },
    /// #3844 — INFO `desktop companion is not started again after a refresh
    /// stopped it: nobody is signed in to that session any more …`, with
    /// `session=`: the loss is forgotten.
    SignedOut { at: Option<u32> },
}

/// #3824 — after a refresh pass, start a tray an earlier pass stopped and
/// could not start again. Nothing at all when none was lost: a tray the
/// person quit, or one that never ran, stays as it is. Nothing either when
/// this pass stopped a tray: its own start has tried already, or marked the
/// tray lost. A start that works, or a tray seen running, forgets the loss;
/// anything else leaves it for the next pass.
///
/// #3844 — `start` is asked once per place a tray was lost
/// ([`TrayLost::lost_at`]): the session it ran in, or `None` where the start
/// has always gone. A session nobody is signed in to any more is forgotten.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn restore_lost_tray(
    pass_stopped_a_tray: bool,
    lost: &TrayLost,
    mut start: impl FnMut(Option<u32>) -> LostTrayStart,
) -> Vec<LostTrayLine> {
    if pass_stopped_a_tray || !lost.is_set() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut down = Vec::new();
    for at in lost.lost_at() {
        match start(at) {
            LostTrayStart::Running => {
                lost.clear(at);
                lines.push(LostTrayLine::Back {
                    at,
                    how: "one is running again".to_string(),
                });
            }
            LostTrayStart::Tried(started @ TrayStart::Started { .. }) => {
                lost.clear(at);
                lines.push(LostTrayLine::Back {
                    at,
                    how: format!("started again ({})", started.describe()),
                });
            }
            LostTrayStart::Tried(TrayStart::SignedOut) => {
                lost.clear(at);
                lines.push(LostTrayLine::SignedOut { at });
            }
            LostTrayStart::NotSetToStart => down.push((
                at,
                "it is not set to start at sign-in here (no `Roomler Desktop` Run value)"
                    .to_string(),
            )),
            LostTrayStart::Tried(not_started) => down.push((at, not_started.describe())),
        }
    }
    if !down.is_empty() {
        // One verdict for the pass: WARN for its first try that did not bring
        // a tray back, DEBUG after.
        let first = lost.failed_try();
        lines.extend(
            down.into_iter()
                .map(|(at, why)| LostTrayLine::StillDown { at, why, first }),
        );
    }
    lines
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
    fn running(&mut self) -> Vec<RunningTray> {
        crate::win_service::companion_procs::find(&self.dest)
            .iter()
            .map(|p| RunningTray {
                pid: p.pid,
                session: p.session,
            })
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
        // #3850 — a write that fails is said once (the passes after it go by
        // the EXE's own version, and do not swap it again).
        let wrote =
            std::fs::write(&self.marker, format!("{}\n", self.version)).map_err(|e| e.to_string());
        log_marker_line(MARKER_WRITES.wrote(wrote), &self.marker, self.version);
    }
    fn respawn(&self) -> RespawnContext {
        self.respawn
    }
    fn start(&mut self, at: Option<u32>) -> TrayStart {
        start_again(self.respawn, &self.dest, at, &[])
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

/// #3844 — the tray's start in session `session`, with that session's own
/// person's token: what a service starts for a tray it stopped there. Nobody
/// signed in there any more (no token, or the session is gone) is
/// `SignedOut`, and nothing is created.
#[cfg(target_os = "windows")]
fn start_desktop_in_session(session: u32, dest: &std::path::Path, args: &[&str]) -> TrayStart {
    use crate::win_service::supervisor;
    match supervisor::query_session_user_token(session) {
        Ok(Some(token)) => {
            // SAFETY: `token` is a live user token from WTSQueryUserToken,
            // held for the duration of the call.
            match unsafe { supervisor::spawn_in_session(token.raw(), dest, args) } {
                Ok(child) => TrayStart::Started {
                    pid: child.pid,
                    session: Some(session),
                },
                Err(e) => TrayStart::Failed(format!("{e:#}")),
            }
        }
        Ok(None) => TrayStart::SignedOut,
        Err(e) => TrayStart::Failed(format!("{e:#}")),
    }
}

/// #3844 — a start at `at` ([`TrayRestart::at`]): a service's in that
/// session, else where the start has always gone ([`start_desktop`]).
#[cfg(target_os = "windows")]
fn start_tray_at(
    respawn: RespawnContext,
    dest: &std::path::Path,
    at: Option<u32>,
    args: &[&str],
) -> TrayStart {
    match (respawn, at) {
        (RespawnContext::SystemService, Some(session)) => {
            start_desktop_in_session(session, dest, args)
        }
        _ => start_desktop(respawn, dest, args),
    }
}

/// #3844 — how long the refresh waits for one start of the tray. A start
/// that works (`WTSQueryUserToken`, `CreateEnvironmentBlock`,
/// `CreateProcessAsUserW`) takes tens to a few hundred milliseconds, more with
/// endpoint protection looking at it; no caller waits on the refresh, so the
/// wait is generous, and a start that hangs past it holds one thread for its
/// session while the next session's start goes ahead.
#[cfg(target_os = "windows")]
const RESPAWN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// #3844 — the refresh's starts of the tray in flight, at most one per place
/// ([`TrayRestart::at`]), each on a thread named `tray-respawn`
/// (`one_in_flight`'s rules).
#[cfg(target_os = "windows")]
static RESPAWNS: crate::one_in_flight::OneInFlightPerKey<Option<u32>, TrayStart> =
    crate::one_in_flight::OneInFlightPerKey::new("tray-respawn");

/// #3844 — the tray's start at `at` ([`start_tray_at`]) for the refresh, on
/// the start in flight's thread, [`RESPAWN_WAIT`] at most ([`start_within`]).
#[cfg(target_os = "windows")]
fn start_again(
    respawn: RespawnContext,
    dest: &std::path::Path,
    at: Option<u32>,
    args: &'static [&'static str],
) -> TrayStart {
    let dest = dest.to_path_buf();
    start_within(&RESPAWNS, at, RESPAWN_WAIT, move || {
        start_tray_at(respawn, &dest, at, args)
    })
}

/// #3844 — `start`, on the thread of the one start in flight for `at` in
/// `starts`, waited for `wait` at most: what came of it, or a failure saying
/// it gave no answer in time. That start goes on on its own thread, and its
/// late answer is nobody's; while it hangs, a later start for the same place
/// waits on it and starts none of its own, so a hung start holds one thread,
/// however many refreshes ask. Never on the async worker: the swap and the
/// lost-tray retry call it from the blocking pool.
#[cfg(any(target_os = "windows", test))]
fn start_within(
    starts: &'static crate::one_in_flight::OneInFlightPerKey<Option<u32>, TrayStart>,
    at: Option<u32>,
    wait: std::time::Duration,
    start: impl FnOnce() -> TrayStart + Send + 'static,
) -> TrayStart {
    use crate::one_in_flight::Asked;
    match starts.ask_blocking(at, wait, start) {
        Asked::Answer(started) => started,
        Asked::Failed(why) => TrayStart::Failed(format!("the start ended with no answer: {why}")),
        Asked::OutOfTime => TrayStart::Failed(format!(
            "the start gave no answer within {wait:?} (`WTSQueryUserToken`, \
             `CreateEnvironmentBlock` or `CreateProcessAsUserW`); it goes on on its own thread"
        )),
    }
}

/// #3824 — start a tray this process's refresh stopped and could not start
/// again: where someone is signed in, none runs, and the tray is set to start
/// at sign-in. With `--autostart`, as the sign-in start does, so a person who
/// switched that off sees nothing (`decide_startup` exits).
#[cfg(target_os = "windows")]
fn start_lost_tray(
    respawn: RespawnContext,
    dest: &std::path::Path,
    at: Option<u32>,
) -> LostTrayStart {
    use roomler_node_core::companion_autostart::AUTOSTART_ARG;
    let session = match (respawn, at) {
        // #3844 — the session it was lost in, while its person is still
        // signed in there; never the console's, whose person may be another.
        (RespawnContext::SystemService, Some(session)) => {
            match crate::win_service::supervisor::query_session_user_token(session) {
                Ok(Some(_)) => session,
                Ok(None) => return LostTrayStart::Tried(TrayStart::SignedOut),
                Err(e) => return LostTrayStart::Tried(TrayStart::Failed(format!("{e:#}"))),
            }
        }
        _ => match signed_in_session(respawn) {
            Some(session) => session,
            None => return LostTrayStart::Tried(TrayStart::NobodySignedIn),
        },
    };
    if desktop_running_in_session(session) {
        return LostTrayStart::Running;
    }
    if !set_to_start_at_sign_in(dest) {
        return LostTrayStart::NotSetToStart;
    }
    LostTrayStart::Tried(start_again(respawn, dest, at, &[AUTOSTART_ARG]))
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
        EnsureOutcome, Installed, LostTrayLine, LostTrayStart, MarkerLine, MarkerWrites,
        REFRESH_RETRY, RefreshLine, RefreshSchedule, RespawnContext, RunningTray, SwapOps,
        SwapOutcome, TrayLost, TrayRestart, TrayStart, free_aside_name, installed_state,
        is_aside_name, known_version, refresh_line, restart_plan, restore_lost_tray,
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

    /// A start that worked in `session`, as the Mock answers it.
    fn started_in(session: u32) -> TrayStart {
        TrayStart::Started {
            pid: 100,
            session: Some(session),
        }
    }

    /// One start after a swap, in `session`.
    fn restart(session: u32, start: TrayStart) -> TrayRestart {
        TrayRestart {
            at: Some(session),
            start,
        }
    }

    /// A running tray, as the process list names one.
    fn tray(pid: u32, session: Option<u32>) -> RunningTray {
        RunningTray { pid, session }
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
        /// The session each running tray is in: session 1 unless a test says.
        session_of: fn(u32) -> Option<u32>,
        /// `None` = stop everything asked.
        stops_only: Option<Vec<u32>>,
        respawn: RespawnContext,
        /// What a start answers; one that works is `Started` in the session
        /// it was asked for.
        start: TrayStart,
        /// Sessions whose person has signed out since: a start there answers
        /// `SignedOut`, and creates nothing.
        signed_out: Vec<u32>,
        /// Sessions where a start fails, refused.
        refused_in: Vec<u32>,
        /// Where each start was asked for, in order.
        started_at: Vec<Option<u32>>,
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
                session_of: |_| Some(1),
                stops_only: None,
                respawn: RespawnContext::SystemService,
                start: started(),
                signed_out: Vec::new(),
                refused_in: Vec::new(),
                started_at: Vec::new(),
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
        fn running(&mut self) -> Vec<RunningTray> {
            self.calls.push("running");
            self.running
                .iter()
                .map(|&pid| tray(pid, (self.session_of)(pid)))
                .collect()
        }
        fn stop(&mut self, pids: &[u32]) -> Vec<u32> {
            self.calls.push("stop");
            self.stops_only.clone().unwrap_or_else(|| pids.to_vec())
        }
        fn write_marker(&mut self) {
            self.calls.push("write_marker");
        }
        fn respawn(&self) -> RespawnContext {
            self.respawn
        }
        fn start(&mut self, at: Option<u32>) -> TrayStart {
            self.calls.push("start");
            self.started_at.push(at);
            match at {
                Some(session) if self.signed_out.contains(&session) => TrayStart::SignedOut,
                Some(session) if self.refused_in.contains(&session) => refused(),
                _ => match &self.start {
                    TrayStart::Started { pid, session } => TrayStart::Started {
                        pid: *pid,
                        session: at.or(*session),
                    },
                    other => other.clone(),
                },
            }
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
                starts: vec![restart(1, started())]
            },
            "both ran in session 1: one start, there"
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
                starts: vec![]
            }
        );
        assert!(!m.calls.contains(&"stop") && !m.calls.contains(&"start"));
        assert!(!lost.is_set());
        assert_eq!(
            restore_lost_tray(out.stopped_a_tray(), &lost, |_| unreachable!(
                "no tray was lost"
            )),
            vec![]
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
        assert!(matches!(&out, SwapOutcome::Aborted { starts, .. } if starts.is_empty()));
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
            starts,
        } = out
        else {
            panic!("{out:?}")
        };
        assert!(
            reason.starts_with("moving the old companion aside failed: Access is denied"),
            "{reason}"
        );
        assert_eq!((stopped, starts), (vec![], vec![]));
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
        assert!(matches!(&out, SwapOutcome::Aborted { starts, .. } if starts.is_empty()));
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
            starts,
        } = out
        else {
            panic!("{out:?}")
        };
        assert!(
            reason.starts_with("could not stop every running companion"),
            "{reason}"
        );
        assert_eq!((stopped, starts), (vec![7], vec![restart(1, started())]));
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
                starts: vec![restart(1, refused())]
            }
        );
        assert!(lost.is_set(), "a stop whose start failed");
        assert_eq!(lost.lost_at(), [Some(1)], "lost in the session it ran in");

        // A tray whose session could not be read starts where the start has
        // always gone, the console; nobody signed in there is a tray lost.
        let nobody = TrayLost::default();
        let mut m = Mock {
            running: vec![7],
            session_of: |_| None,
            start: TrayStart::NobodySignedIn,
            ..Default::default()
        };
        swap(&mut m, &nobody);
        assert!(nobody.is_set(), "nobody signed in to start it for");
        assert_eq!(nobody.lost_at(), [None]);

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
        let line = restore_lost_tray(out.stopped_a_tray(), &lost, |_| {
            LostTrayStart::Tried(started())
        });
        assert_eq!(
            line,
            vec![LostTrayLine::Back {
                at: Some(1),
                how: "started again (pid 100, session 1)".to_string()
            }]
        );
        assert!(!lost.is_set());
        assert_eq!(
            restore_lost_tray(false, &lost, |_| unreachable!("a quit tray stays quit")),
            vec![]
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
        let still_down = |lines: Vec<LostTrayLine>| match lines.as_slice() {
            [LostTrayLine::StillDown { why, first, .. }] => (why.clone(), *first),
            other => panic!("{other:?}"),
        };
        let (why, first) = still_down(restore_lost_tray(false, &lost, |_| {
            LostTrayStart::Tried(refused())
        }));
        assert!(first && why.contains("(err 5)"), "{why}");
        let (why, first) = still_down(restore_lost_tray(false, &lost, |_| {
            LostTrayStart::Tried(TrayStart::NobodySignedIn)
        }));
        assert!(!first && why.contains("nobody is signed in"), "{why}");
        let (why, first) = still_down(restore_lost_tray(false, &lost, |_| {
            LostTrayStart::NotSetToStart
        }));
        assert!(!first && why.contains("not set to start"), "{why}");
        assert!(lost.is_set());
        // The person signs in, and the sign-in start brings it back.
        assert_eq!(
            restore_lost_tray(false, &lost, |_| LostTrayStart::Running),
            vec![LostTrayLine::Back {
                at: Some(1),
                how: "one is running again".to_string()
            }]
        );
        assert!(!lost.is_set());
        assert_eq!(
            restore_lost_tray(false, &lost, |_| unreachable!("forgotten")),
            vec![]
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
            restore_lost_tray(out.stopped_a_tray(), &lost, |_| unreachable!(
                "one start a pass"
            )),
            vec![]
        );
    }

    /// #3824: the line says whether the start worked, and "respawned" only
    /// when it did. It used to read `respawned it: true` after a failed one.
    #[test]
    fn the_refresh_line_never_says_respawned_after_a_failed_start() {
        for outcome in [
            SwapOutcome::Swapped {
                stopped: vec![7],
                starts: vec![restart(1, refused())],
            },
            SwapOutcome::Swapped {
                stopped: vec![7],
                starts: vec![TrayRestart {
                    at: None,
                    start: TrayStart::NobodySignedIn,
                }],
            },
            SwapOutcome::Aborted {
                reason: "could not stop every running companion (running [7, 9], stopped [7])"
                    .to_string(),
                stopped: vec![7],
                starts: vec![restart(1, refused())],
            },
        ] {
            let line = refresh_line(&outcome);
            assert!(!format!("{line:?}").contains("respawned"), "{line:?}");
        }
        assert_eq!(
            refresh_line(&SwapOutcome::Swapped {
                stopped: vec![7],
                starts: vec![restart(1, refused())]
            }),
            RefreshLine::TrayNotBack {
                error: r"session 1: CreateProcessAsUserW(C:\Program Files\Roomler\roomler-desktop.exe) failed (err 5)"
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
                starts: vec![restart(1, started())]
            }),
            RefreshLine::Refreshed {
                tray: "respawned (pid 100, session 1)".to_string()
            }
        );
        assert_eq!(
            refresh_line(&SwapOutcome::Swapped {
                stopped: vec![],
                starts: vec![]
            }),
            RefreshLine::Refreshed {
                tray: "none was running".to_string()
            }
        );
        assert_eq!(
            refresh_line(&SwapOutcome::Aborted {
                reason: format!("moving the old companion aside failed: {DENIED}"),
                stopped: vec![],
                starts: vec![]
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
                starts: vec![restart(1, started())]
            }),
            RefreshLine::Skipped {
                error: "could not stop every running companion (running [7, 9], stopped [7]) \
                        (the trays it had stopped: respawned (pid 100, session 1))"
                    .to_string()
            }
        );
        let RefreshLine::Skipped { error } = refresh_line(&SwapOutcome::Aborted {
            reason: "could not stop every running companion (running [7, 9], stopped [7])"
                .to_string(),
            stopped: vec![7],
            starts: vec![restart(1, refused())],
        }) else {
            panic!("a skip")
        };
        assert!(
            error.ends_with(
                "(the trays it had stopped: session 1: \
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

    /// This daemon's version in the #3850 tests.
    const OWN: &str = "0.4.223";

    /// #3850, the issue's case: a swap puts this version's EXE in place, and
    /// its marker cannot be written (read-only, a deny, a scanner). The next
    /// passes read the old marker, or none, and the EXE's own version: this
    /// one. Nothing is swapped again, so no tray is stopped again; the marker
    /// is written again each pass, and its failure is ONE warning, then debug
    /// lines. Nor are the fast waits chosen, whatever the marker reads.
    ///
    /// Red before #3850: every later pass swapped again (a stop and a start
    /// of the person's tray each time), every failed write warned, and a
    /// missing marker chose the waits of 2 to 15 minutes.
    #[test]
    fn a_marker_that_cannot_be_written_is_no_reason_to_swap_again() {
        let lost = TrayLost::default();
        let writes = MarkerWrites::default();
        let denied = || Err(DENIED.to_string());

        // Pass 1: the marker and the EXE both say 0.4.222. A swap, whose
        // marker write fails.
        assert_eq!(
            installed_state(OWN, true, Some("0.4.222\n"), || Some("0.4.222".into())),
            Installed::Stale {
                recorded: Some("0.4.222".into()),
                its_version: Some("0.4.222".into())
            }
        );
        let mut first = Mock {
            running: vec![7],
            ..Default::default()
        };
        assert!(matches!(
            swap(&mut first, &lost),
            SwapOutcome::Swapped { .. }
        ));
        assert!(first.calls.contains(&"write_marker"));
        let mut lines = vec![writes.wrote(denied())];

        // Passes 2 to 4: the marker still says 0.4.222, or reads as nothing,
        // and the EXE says this version. Nothing to swap; the marker is
        // written again, and fails again.
        for marker in [Some("0.4.222\n"), None, Some("")] {
            assert_eq!(
                installed_state(OWN, true, marker, || Some(OWN.into())),
                Installed::Current {
                    by_its_version: true
                },
                "no second swap ({marker:?})"
            );
            lines.push(writes.wrote(denied()));
        }
        let warned =
            |l: &Option<MarkerLine>| matches!(l, Some(MarkerLine::NotWritten { first: true, .. }));
        assert_eq!(
            lines.iter().filter(|l| warned(l)).count(),
            1,
            "one WARN: {lines:?}"
        );
        assert!(warned(&lines[0]), "the swap's own failure warns");
        assert!(
            lines[1..]
                .iter()
                .all(|l| matches!(l, Some(MarkerLine::NotWritten { first: false, error }) if error == DENIED)),
            "then debug lines, with the error: {lines:?}"
        );

        // The schedule: the EXE vouches for this version, so the waits are
        // the normal period, never the fast ones, whatever the marker reads.
        for marker in [
            None,
            Some(""),
            Some("garbage"),
            Some("0.4.212"),
            Some("0.4.222"),
        ] {
            let version = known_version(OWN, marker, || Some(OWN.into()));
            assert_eq!(version.as_deref(), Some(OWN), "{marker:?}");
            let next = RefreshSchedule::default().next(version.as_deref(), false);
            assert_eq!(
                (next.wait, next.too_old),
                (REFRESH_RETRY, false),
                "no fast waits ({marker:?})"
            );
        }

        // A write that works again says so, once.
        assert_eq!(writes.wrote(Ok(())), Some(MarkerLine::WrittenAgain));
        assert_eq!(writes.wrote(Ok(())), None);
    }

    /// #3850, the control: what the version check swaps, it always swapped.
    /// The common case is one small file read: the EXE is not read while the
    /// marker names this version. A version that cannot be read is not
    /// known, which is stale: the refresh swaps, as before.
    #[test]
    fn the_version_check_swaps_what_it_always_did() {
        let never = || -> Option<String> { panic!("the EXE is not read") };
        assert_eq!(
            installed_state(OWN, true, Some("0.4.223\r\n"), never),
            Installed::Current {
                by_its_version: false
            }
        );
        assert_eq!(
            known_version(OWN, Some("0.4.223\n"), never).as_deref(),
            Some(OWN)
        );
        // No EXE: the refresh puts one in place, as it always has.
        assert_eq!(
            installed_state(OWN, false, Some(OWN), never),
            Installed::Stale {
                recorded: Some(OWN.into()),
                its_version: None
            }
        );
        // Another version by both, or one the EXE does not state.
        for (marker, its) in [
            (Some("0.4.222"), Some("0.4.222")),
            (None, Some("0.4.222")),
            (Some("0.4.222"), None),
            (None, None),
            (Some("0.4.224"), Some("0.4.223-rc.1")),
        ] {
            assert!(
                matches!(
                    installed_state(OWN, true, marker, || its.map(String::from)),
                    Installed::Stale { .. }
                ),
                "{marker:?} {its:?}"
            );
        }
        // What the retry loop reads: the EXE's version when the marker does
        // not name this one, else the marker's own words.
        assert_eq!(
            known_version(OWN, Some("0.4.212"), || Some("0.4.222".into())).as_deref(),
            Some("0.4.222")
        );
        assert_eq!(
            known_version(OWN, Some("0.4.212\n"), || None).as_deref(),
            Some("0.4.212")
        );
        assert_eq!(known_version(OWN, None, || None), None);
    }

    /// #3844, the issue's case: trays stopped in sessions 1 and 3 (two in 3)
    /// start again in 1 and in 3, once each, and nowhere else, and the line
    /// names both.
    ///
    /// Red before #3844: one start, where the start has always gone (the
    /// console session), whichever sessions the stopped trays ran in.
    #[test]
    fn trays_stopped_in_two_sessions_start_again_in_those_two() {
        let lost = TrayLost::default();
        let mut m = Mock {
            running: vec![7, 9, 11],
            session_of: |pid| Some(if pid == 9 { 1 } else { 3 }),
            ..Default::default()
        };
        let out = swap(&mut m, &lost);
        assert_eq!(m.started_at, [Some(1), Some(3)], "1 and 3, and only there");
        assert_eq!(
            out,
            SwapOutcome::Swapped {
                stopped: vec![7, 9, 11],
                starts: vec![restart(1, started_in(1)), restart(3, started_in(3))]
            }
        );
        assert!(!lost.is_set());
        assert_eq!(
            refresh_line(&out),
            RefreshLine::Refreshed {
                tray: "respawned (pid 100, session 1); respawned (pid 100, session 3)".to_string()
            }
        );
    }

    /// #3844: a session whose person has signed out since gets no tray (its
    /// start finds nobody to start one for, and creates nothing), and the
    /// line says so. That is no tray lost: the sign-in start owns their next
    /// one.
    #[test]
    fn a_session_signed_out_of_since_gets_no_start_and_the_line_says_so() {
        let lost = TrayLost::default();
        let mut m = Mock {
            running: vec![7, 9],
            session_of: |pid| Some(if pid == 7 { 1 } else { 3 }),
            signed_out: vec![3],
            ..Default::default()
        };
        let out = swap(&mut m, &lost);
        assert_eq!(
            out,
            SwapOutcome::Swapped {
                stopped: vec![7, 9],
                starts: vec![restart(1, started_in(1)), restart(3, TrayStart::SignedOut)]
            }
        );
        assert!(!lost.is_set(), "a person who signed out lost no tray");
        assert_eq!(
            refresh_line(&out),
            RefreshLine::Refreshed {
                tray: "respawned (pid 100, session 1); session 3: nobody is signed in to that \
                       session any more, so none was started"
                    .to_string()
            }
        );
        // A start that failed in one session is said, and only that session's
        // tray is lost.
        let mut m = Mock {
            running: vec![7, 9],
            session_of: |pid| Some(if pid == 7 { 1 } else { 3 }),
            refused_in: vec![3],
            ..Default::default()
        };
        let RefreshLine::TrayNotBack { error } = refresh_line(&swap(&mut m, &lost)) else {
            panic!("a start that failed is a warning")
        };
        assert!(
            error.starts_with("respawned (pid 100, session 1); session 3: CreateProcessAsUserW("),
            "{error}"
        );
        assert_eq!(lost.lost_at(), [Some(3)]);
    }

    /// #3844: none stopped, none started; a daemon that runs as the person
    /// starts one in its own session, as before; and a tray whose session
    /// could not be read starts where the start has always gone (`None`).
    #[test]
    fn where_no_session_is_known_the_start_goes_where_it_always_went() {
        use RespawnContext::{SystemService, UserSession};
        assert_eq!(restart_plan(SystemService, &[]), []);
        assert_eq!(restart_plan(UserSession, &[]), []);
        assert_eq!(
            restart_plan(UserSession, &[tray(7, Some(1)), tray(9, Some(2))]),
            [None],
            "a per-user install: unchanged"
        );
        assert_eq!(restart_plan(SystemService, &[tray(7, None)]), [None]);
        assert_eq!(
            restart_plan(
                SystemService,
                &[
                    tray(7, Some(3)),
                    tray(8, None),
                    tray(9, Some(1)),
                    tray(10, Some(3))
                ]
            ),
            [Some(1), Some(3), None]
        );
        let mut m = Mock {
            running: vec![7],
            respawn: UserSession,
            ..Default::default()
        };
        swap(&mut m, &TrayLost::default());
        assert_eq!(m.started_at, [None]);
    }

    /// #3844: a tray lost in session 3 is tried again in session 3, never at
    /// the console, whose own tray runs; a tray lost where the start always
    /// goes is tried there, as before; a session nobody is signed in to any
    /// more is forgotten, with its line.
    ///
    /// Red before #3844: the retry went to the console, found that person's
    /// tray running, and said the lost one was back.
    #[test]
    fn a_lost_tray_is_tried_again_in_the_session_it_was_lost_in() {
        let lost = TrayLost::default();
        swap(
            &mut Mock {
                running: vec![7, 9],
                session_of: |pid| Some(if pid == 7 { 1 } else { 3 }),
                refused_in: vec![3],
                ..Default::default()
            },
            &lost,
        );
        assert_eq!(lost.lost_at(), [Some(3)]);
        let mut asked = Vec::new();
        let lines = restore_lost_tray(false, &lost, |at| {
            asked.push(at);
            LostTrayStart::Tried(TrayStart::Started {
                pid: 300,
                session: at,
            })
        });
        assert_eq!(asked, [Some(3)], "session 3 only");
        assert_eq!(
            lines,
            vec![LostTrayLine::Back {
                at: Some(3),
                how: "started again (pid 300, session 3)".to_string()
            }]
        );
        assert!(!lost.is_set());

        // Lost where the start always goes (a tray whose session could not
        // be read, nobody at the console), and in session 4, whose person
        // then signs out.
        swap(
            &mut Mock {
                running: vec![7],
                session_of: |_| None,
                start: TrayStart::NobodySignedIn,
                ..Default::default()
            },
            &lost,
        );
        swap(
            &mut Mock {
                running: vec![9],
                session_of: |_| Some(4),
                refused_in: vec![4],
                ..Default::default()
            },
            &lost,
        );
        assert_eq!(lost.lost_at(), [None, Some(4)]);
        let mut asked = Vec::new();
        let lines = restore_lost_tray(false, &lost, |at| {
            asked.push(at);
            match at {
                None => LostTrayStart::Tried(started()),
                Some(_) => LostTrayStart::Tried(TrayStart::SignedOut),
            }
        });
        assert_eq!(asked, [None, Some(4)]);
        assert_eq!(
            lines,
            vec![
                LostTrayLine::Back {
                    at: None,
                    how: "started again (pid 100, session 1)".to_string()
                },
                LostTrayLine::SignedOut { at: Some(4) }
            ]
        );
        assert!(!lost.is_set());
    }
}

/// #3850 — the companion EXE's own version, read from its `VS_VERSIONINFO`
/// resource: what a good one states, and that a truncated, garbage or lying
/// one states nothing, never a panic and never a guess. On every OS; the read
/// of a real EXE on Windows.
#[cfg(test)]
mod version_resource_tests {
    use super::version_resource_version;

    enum Value<'a> {
        Text(&'a str),
        Bytes(&'a [u8]),
        None,
    }

    /// A block as rc.exe lays one out: its three header words, its key, its
    /// value and its children, each on a 32-bit boundary, its length counting
    /// up to its last byte.
    fn block(key: &str, value: Value, children: &[Vec<u8>]) -> Vec<u8> {
        fn pad(b: &mut Vec<u8>) {
            while !b.len().is_multiple_of(4) {
                b.push(0);
            }
        }
        let mut b = vec![0u8; 6];
        for unit in key.encode_utf16().chain([0]) {
            b.extend(unit.to_le_bytes());
        }
        pad(&mut b);
        let (value_len, kind) = match value {
            Value::Text(text) => {
                let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
                for unit in &units {
                    b.extend(unit.to_le_bytes());
                }
                (units.len(), 1u16)
            }
            Value::Bytes(bytes) => {
                b.extend_from_slice(bytes);
                (bytes.len(), 0)
            }
            Value::None => (0, 1),
        };
        for child in children {
            pad(&mut b);
            b.extend(child);
        }
        let len = u16::try_from(b.len()).expect("a test block fits a word");
        b[0..2].copy_from_slice(&len.to_le_bytes());
        b[2..4].copy_from_slice(&(value_len as u16).to_le_bytes());
        b[4..6].copy_from_slice(&kind.to_le_bytes());
        b
    }

    /// A resource with `strings` in its one string table, as the release's
    /// companion carries one: `VS_FIXEDFILEINFO`, then `StringFileInfo` and
    /// `VarFileInfo`. #3908: also the version resource of the EXEs
    /// `beside_version_tests` builds.
    pub(super) fn resource(strings: &[(&str, &str)]) -> Vec<u8> {
        let mut fixed = 0xFEEF_04BDu32.to_le_bytes().to_vec();
        fixed.resize(52, 0);
        let strings: Vec<Vec<u8>> = strings
            .iter()
            .map(|(key, text)| block(key, Value::Text(text), &[]))
            .collect();
        block(
            "VS_VERSION_INFO",
            Value::Bytes(&fixed),
            &[
                block(
                    "StringFileInfo",
                    Value::None,
                    &[block("040904b0", Value::None, &strings)],
                ),
                block(
                    "VarFileInfo",
                    Value::None,
                    &[block(
                        "Translation",
                        Value::Bytes(&[0x09, 0x04, 0xb0, 0x04]),
                        &[],
                    )],
                ),
            ],
        )
    }

    /// Where the block keyed `key` starts in `res`.
    fn block_at(res: &[u8], key: &str) -> usize {
        let wanted: Vec<u8> = key.encode_utf16().flat_map(u16::to_le_bytes).collect();
        res.windows(wanted.len())
            .position(|w| w == wanted.as_slice())
            .expect("the key is in the resource")
            - 6
    }

    fn set_word(res: &mut [u8], at: usize, word: u16) {
        res[at..at + 2].copy_from_slice(&word.to_le_bytes());
    }

    /// The release's companion states its version in both strings
    /// (`0.4.221` on the installed 0.4.221 tray); `ProductVersion` is read
    /// first, `FileVersion` when it is missing.
    #[test]
    fn a_resource_states_its_product_version_else_its_file_version() {
        let good = resource(&[
            ("CompanyName", "G ROX LTD"),
            ("FileDescription", "Roomler"),
            ("FileVersion", "0.4.223"),
            ("ProductName", "Roomler"),
            ("ProductVersion", "0.4.223"),
        ]);
        assert_eq!(version_resource_version(&good).as_deref(), Some("0.4.223"));
        assert_eq!(
            version_resource_version(&resource(&[("FileVersion", "0.4.223")])).as_deref(),
            Some("0.4.223")
        );
        assert_eq!(
            version_resource_version(&resource(&[
                ("FileVersion", "0.4.222"),
                ("ProductVersion", "0.4.223")
            ]))
            .as_deref(),
            Some("0.4.223")
        );
        assert_eq!(
            version_resource_version(&resource(&[(
                "FileVersion",
                "10.0.26100.1 (WinBuild.160101.0800)"
            )]))
            .as_deref(),
            Some("10.0.26100.1")
        );
        assert_eq!(
            version_resource_version(&resource(&[("ProductName", "Roomler")])),
            None,
            "no version stated"
        );
        assert_eq!(
            version_resource_version(&resource(&[("ProductVersion", "  ")])),
            None
        );
    }

    /// Anything malformed is not known: a resource cut anywhere, garbage, a
    /// length that overruns its parent or the resource, one too short for a
    /// header, a key with no terminator inside its block, a value longer than
    /// its block, an unknown type, a root that is not `VS_VERSION_INFO`.
    /// None of it panics, and no byte flipped anywhere does.
    #[test]
    fn a_truncated_garbage_or_lying_resource_states_no_version() {
        let good = resource(&[("FileVersion", "0.4.223"), ("ProductVersion", "0.4.223")]);
        assert_eq!(
            version_resource_version(&good).as_deref(),
            Some("0.4.223"),
            "the control"
        );
        for cut in 0..good.len() {
            assert_eq!(version_resource_version(&good[..cut]), None, "cut at {cut}");
        }

        let mut seed = 0x2545_F491u32;
        let noise: Vec<u8> = (0..4096)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed.to_le_bytes()[0]
            })
            .collect();
        for junk in [&[][..], &[0u8; 64][..], &[0xFFu8; 64][..], &noise[..]] {
            assert_eq!(version_resource_version(junk), None);
        }

        let lying = |edit: &dyn Fn(&mut Vec<u8>)| {
            let mut res = good.clone();
            edit(&mut res);
            version_resource_version(&res)
        };
        let info = block_at(&good, "StringFileInfo");
        let product = block_at(&good, "ProductVersion");
        assert_eq!(
            lying(&|r| set_word(r, info, 0xFFF0)),
            None,
            "past its parent"
        );
        assert_eq!(
            lying(&|r| set_word(r, 0, 0xFFF0)),
            None,
            "past the resource"
        );
        assert_eq!(
            lying(&|r| set_word(r, info, 4)),
            None,
            "shorter than a header"
        );
        assert_eq!(
            lying(&|r| set_word(r, product, 6 + 8)),
            None,
            "its key cut by its own length"
        );
        assert_eq!(
            lying(&|r| set_word(r, product + 2, 0x7FFF)),
            None,
            "a value longer than its block"
        );
        assert_eq!(lying(&|r| set_word(r, product + 4, 7)), None, "a type of 7");
        assert_eq!(
            lying(&|r| r[6] = b'X'),
            None,
            "a root that is not VS_VERSION_INFO"
        );

        for at in 0..good.len() {
            for flip in [0x01u8, 0x80, 0xFF] {
                let mut res = good.clone();
                res[at] ^= flip;
                if let Some(version) = version_resource_version(&res) {
                    assert!(version.len() <= 32, "bounded: {version:?}");
                }
            }
        }
    }

    /// The read of a real EXE: a system program states its version, a file
    /// that is no program states none, and the file is not held afterwards
    /// (a mapped image refuses a delete, so a delete that works proves the
    /// read freed it).
    #[cfg(target_os = "windows")]
    #[test]
    fn a_real_exe_states_its_version_and_is_not_held_after() {
        let system32 = std::path::PathBuf::from(
            std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()),
        )
        .join("System32");
        let dir = tempfile::tempdir().expect("tempdir");
        let copy = dir.path().join("roomler-desktop.exe");
        std::fs::copy(system32.join("cmd.exe"), &copy).expect("copy cmd.exe");
        let stated = super::exe_version(&copy).expect("cmd.exe states its version");
        assert!(stated.starts_with("10.0."), "{stated}");
        std::fs::remove_file(&copy).expect("the read freed the file");

        let text = dir.path().join("roomler-desktop.exe");
        std::fs::write(&text, "not a program").expect("write");
        assert_eq!(super::exe_version(&text), None);
        assert_eq!(super::exe_version(&dir.path().join("missing.exe")), None);
    }
}

/// #3908 — `roomler status` ([`too_old_here`], through `too_old_beside`) and
/// the heartbeat ([`installed_version`], through `installed_beside`) read the
/// companion's version by the refresh's rule ([`known_version`], #3850): the
/// marker when it names this daemon's version, else what the EXE states in
/// its own version resource, else the marker's words. Each test has a folder
/// of its own with a real EXE in it: a copy of this test's own executable,
/// given the version resource the test needs with `UpdateResourceW`.
///
/// Red before #3908: both read the marker alone.
#[cfg(all(test, target_os = "windows"))]
mod beside_version_tests {
    use super::version_resource_tests::resource;
    use super::{
        BesideNow, DESKTOP_EXE, ExeVersions, MIN_COMPANION_VERSION, Stamp, VERSION_MARKER,
        installed_beside, known_version, too_old_beside,
    };
    use std::path::{Path, PathBuf};
    use std::sync::PoisonError;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::time::{Duration, Instant};
    use tunnel_core::localapi::CompanionTooOld;

    /// This daemon's version, which the release's current tray states.
    const OWN: &str = env!("CARGO_PKG_VERSION");
    /// A tray too old to connect: `MIN_COMPANION_VERSION` is 0.4.216.
    const OLD: &str = "0.4.212";
    /// A tray of another version that connects.
    const EARLIER: &str = "0.4.220";
    /// How long a test waits for a read of the EXE to land.
    const LAND: Duration = Duration::from_secs(20);

    /// A folder as a system install has the companion beside the daemon: its
    /// EXE and its version marker, read through a cache of the folder's own.
    /// Removed on every path: once no read runs, with its marker writable
    /// again.
    struct Beside {
        dir: tempfile::TempDir,
        versions: &'static ExeVersions,
    }

    impl Beside {
        fn new(read: fn(&Path) -> Option<String>) -> Self {
            Self {
                dir: tempfile::tempdir().expect("tempdir"),
                versions: Box::leak(Box::new(ExeVersions::new("test-exe-ver", read))),
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        /// The EXE: a copy of this test's executable whose one resource is
        /// `res`, as its version resource, written beside and renamed into
        /// place as a swap puts one there. Stamped with its own last write: a
        /// copy keeps the source's.
        fn exe(&self, res: &[u8]) -> PathBuf {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::System::LibraryLoader::{
                BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW,
            };
            use windows_sys::Win32::UI::WindowsAndMessaging::RT_VERSION;

            let staged = self.path().join("staged.exe");
            let this = std::env::current_exe().expect("this test's executable");
            std::fs::copy(this, &staged).expect("copy this test's executable");
            let wide: Vec<u16> = staged
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let len = u32::try_from(res.len()).expect("a test resource fits a u32");
            // SAFETY: `wide` is NUL-terminated and outlives every call, the
            // update is ended exactly once, and UpdateResourceW copies `res`,
            // which outlives it. TRUE: the copy's own resources all go, so
            // the version resource is the test's alone.
            unsafe {
                let update = BeginUpdateResourceW(wide.as_ptr(), 1);
                assert!(
                    !update.is_null(),
                    "BeginUpdateResourceW: {}",
                    std::io::Error::last_os_error()
                );
                let put = UpdateResourceW(
                    update,
                    RT_VERSION,
                    std::ptr::without_provenance(1),
                    0x0409,
                    res.as_ptr().cast(),
                    len,
                );
                let not_put = std::io::Error::last_os_error();
                let ended = EndUpdateResourceW(update, i32::from(put == 0));
                assert!(put != 0, "UpdateResourceW: {not_put}");
                assert!(
                    ended != 0,
                    "EndUpdateResourceW: {}",
                    std::io::Error::last_os_error()
                );
            }
            let exe = self.path().join(DESKTOP_EXE);
            retried("stamp the EXE's last write", || {
                std::fs::File::options()
                    .write(true)
                    .open(&staged)?
                    .set_modified(std::time::SystemTime::now())
            });
            retried("rename the EXE into place", || {
                std::fs::rename(&staged, &exe)
            });
            exe
        }

        /// The marker, reading `text`, read-only as on the device in #3850:
        /// the daemon cannot write it.
        fn read_only_marker(&self, text: &str) {
            self.writable_marker();
            let marker = self.path().join(VERSION_MARKER);
            std::fs::write(&marker, text).expect("write the marker");
            let mut perms = std::fs::metadata(&marker).expect("marker").permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&marker, perms).expect("make the marker read-only");
        }

        /// The marker writable again, when there is one.
        #[expect(
            clippy::permissions_set_readonly_false,
            reason = "on Windows it clears the read-only attribute alone"
        )]
        fn writable_marker(&self) {
            let marker = self.path().join(VERSION_MARKER);
            if let Ok(meta) = std::fs::metadata(&marker) {
                let mut perms = meta.permissions();
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(&marker, perms);
            }
        }

        /// What `installed_version` reads for this folder now.
        fn installed(&self) -> BesideNow {
            installed_beside(self.path(), OWN, self.versions)
        }

        /// What `status` is told about this folder now, from a daemon whose
        /// pipe refuses an old tray, under the service.
        fn too_old(&self) -> Option<CompanionTooOld> {
            too_old_beside(Some(self.path()), OWN, true, self.versions)
        }

        /// [`Self::installed`] once the read of the EXE for the files there
        /// now has landed, [`LAND`] at most.
        fn settled(&self) -> BesideNow {
            let deadline = Instant::now() + LAND;
            loop {
                let now = self.installed();
                if now.settled || Instant::now() >= deadline {
                    return now;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl Drop for Beside {
        fn drop(&mut self) {
            self.versions.drained();
            self.writable_marker();
        }
    }

    /// `op` until it works, [`LAND`] at most: a scanner may hold a new EXE
    /// for a moment after it is written.
    fn retried(what: &str, mut op: impl FnMut() -> std::io::Result<()>) {
        let deadline = Instant::now() + LAND;
        while let Err(e) = op() {
            assert!(Instant::now() < deadline, "{what}: {e}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn settled(version: &str) -> BesideNow {
        BesideNow {
            version: Some(version.into()),
            settled: true,
        }
    }

    /// #3908, the issue's case: a read-only marker naming an old version,
    /// beside an EXE of this daemon's version. Once the EXE's read lands,
    /// status is told of no tray too old, and the heartbeat reports the EXE's
    /// version: what the refresh reads from the same files.
    #[test]
    fn a_read_only_old_marker_beside_a_current_exe_reads_as_the_exe() {
        let beside = Beside::new(super::exe_version);
        let exe = beside.exe(&resource(&[("FileVersion", OWN), ("ProductVersion", OWN)]));
        beside.read_only_marker(&format!("{OLD}\n"));

        assert_eq!(beside.settled(), settled(OWN));
        assert_eq!(beside.too_old(), None, "no tray too old");

        let marker = std::fs::read_to_string(beside.path().join(VERSION_MARKER)).ok();
        assert_eq!(
            known_version(OWN, marker.as_deref(), || super::exe_version(&exe)).as_deref(),
            Some(OWN),
            "what the refresh reads"
        );
    }

    /// #3908, the control: a marker naming this daemon's version reads as
    /// today, and the EXE is not read at all, whatever it states. With no EXE,
    /// as today too: no tray to refuse, no version to report, nothing read.
    #[test]
    fn a_marker_naming_this_version_reads_as_today() {
        static READS: AtomicUsize = AtomicUsize::new(0);
        fn counted(exe: &Path) -> Option<String> {
            READS.fetch_add(1, SeqCst);
            super::exe_version(exe)
        }
        let beside = Beside::new(counted);
        let exe = beside.exe(&resource(&[("ProductVersion", OLD)]));
        for marker in [format!("{OWN}\n"), format!("{OWN}\r\n"), OWN.to_string()] {
            beside.read_only_marker(&marker);
            assert_eq!(beside.installed(), settled(OWN), "{marker:?}");
            assert_eq!(beside.too_old(), None, "{marker:?}");
        }

        std::fs::remove_file(exe).expect("remove the EXE");
        beside.read_only_marker(&format!("{OLD}\n"));
        assert_eq!(
            beside.installed(),
            BesideNow {
                version: None,
                settled: true
            },
            "no EXE"
        );
        assert_eq!(beside.too_old(), None, "no EXE");
        assert_eq!(READS.load(SeqCst), 0, "the EXE is not read");
    }

    /// #3908, the control: an EXE whose version resource is malformed states
    /// no version, so both read by the marker, as today. A marker naming an
    /// old version is too old, by that version; with no marker, the
    /// heartbeat reports none and status a tray of no version.
    #[test]
    fn a_malformed_resource_reads_by_the_marker_as_today() {
        let good = resource(&[("FileVersion", OWN), ("ProductVersion", OWN)]);
        for (what, res) in [
            ("garbage", vec![0xFFu8; 64]),
            ("cut short", good[..good.len() / 2].to_vec()),
        ] {
            let beside = Beside::new(super::exe_version);
            let exe = beside.exe(&res);
            assert_eq!(super::exe_version(&exe), None, "{what}: states no version");

            beside.read_only_marker(&format!("{OLD}\n"));
            assert_eq!(beside.settled(), settled(OLD), "{what}");
            assert_eq!(
                beside.too_old(),
                Some(CompanionTooOld {
                    installed: Some(OLD.into()),
                    minimum: MIN_COMPANION_VERSION.into(),
                    by_service: true,
                    retry_off: false,
                }),
                "{what}"
            );

            beside.writable_marker();
            std::fs::remove_file(beside.path().join(VERSION_MARKER)).expect("remove the marker");
            assert_eq!(
                beside.settled(),
                BesideNow {
                    version: None,
                    settled: true
                },
                "{what}, no marker"
            );
            let reported = beside.too_old().expect("a tray of no version");
            assert_eq!(reported.installed, None, "{what}, no marker");
        }
    }

    /// #3908 — status and the heartbeat never wait for the EXE. While its
    /// read is held (a scanner holding the file), every ask answers from the
    /// marker at once and is not settled, and one read runs however often
    /// they ask. Once it lands, the EXE's version.
    #[test]
    fn a_cold_read_answers_from_the_marker_and_never_waits_for_the_exe() {
        static OPEN: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
        static OPENED: std::sync::Condvar = std::sync::Condvar::new();
        static BEGAN: AtomicUsize = AtomicUsize::new(0);
        fn open() {
            *OPEN.lock().unwrap_or_else(PoisonError::into_inner) = true;
            OPENED.notify_all();
        }
        fn held(exe: &Path) -> Option<String> {
            BEGAN.fetch_add(1, SeqCst);
            let gate = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
            drop(
                OPENED
                    .wait_while(gate, |open| !*open)
                    .unwrap_or_else(PoisonError::into_inner),
            );
            super::exe_version(exe)
        }
        /// Opens the gate as the test ends, before its folder goes, so a
        /// failed assertion leaves no read held.
        struct OpenedAtTheEnd;
        impl Drop for OpenedAtTheEnd {
            fn drop(&mut self) {
                open();
            }
        }

        let beside = Beside::new(held);
        let _opened = OpenedAtTheEnd;
        beside.exe(&resource(&[("ProductVersion", OWN)]));
        beside.read_only_marker(&format!("{OLD}\n"));

        // A reader that waited for the read would wait forever: this turns
        // that wait into a failure of the timing below.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(10));
            open();
        });
        let from_the_marker = BesideNow {
            version: Some(OLD.into()),
            settled: false,
        };
        for _ in 0..5 {
            let asked = Instant::now();
            assert_eq!(beside.installed(), from_the_marker);
            let reported = beside.too_old().expect("too old by the marker meanwhile");
            assert_eq!(reported.installed.as_deref(), Some(OLD));
            let took = asked.elapsed();
            assert!(
                took < Duration::from_secs(2),
                "no wait for the EXE: {took:?}"
            );
        }
        let deadline = Instant::now() + LAND;
        while BEGAN.load(SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = (beside.installed(), beside.too_old());
        assert_eq!(BEGAN.load(SeqCst), 1, "one read, however often asked");

        open();
        assert_eq!(beside.settled(), settled(OWN));
        assert_eq!(beside.too_old(), None);
        assert_eq!(BEGAN.load(SeqCst), 1, "and none after it");
    }

    /// #3908 — the refresh swaps the EXE and cannot write the marker
    /// (#3850): the EXE is read again, by its own last write, and both
    /// readers move to the new version. Keyed on the marker alone, they would
    /// keep the version read before the swap for the process's life.
    #[test]
    fn an_exe_swapped_under_a_marker_that_cannot_be_written_is_read_again() {
        let beside = Beside::new(super::exe_version);
        beside.exe(&resource(&[("ProductVersion", EARLIER)]));
        beside.read_only_marker(&format!("{OLD}\n"));
        assert_eq!(beside.settled(), settled(EARLIER));
        assert_eq!(beside.too_old(), None, "{EARLIER} connects");

        let before = Stamp::of(beside.path());
        beside.exe(&resource(&[("ProductVersion", OWN)]));
        let after = Stamp::of(beside.path());
        assert_eq!(after.marker, before.marker, "the marker was not written");
        assert_ne!(after.exe, before.exe, "the EXE was");

        assert_eq!(beside.settled(), settled(OWN));
        assert_eq!(beside.too_old(), None);
    }
}

/// #3844 — each start of the tray again is bounded: a start that hangs in
/// one session fails within its wait, saying so, and holds that session
/// alone. The next session's start goes ahead at once, and another ask for
/// the held session starts none of its own until the held one ends.
#[cfg(test)]
mod start_within_tests {
    use super::{TrayStart, start_within};
    use crate::one_in_flight::OneInFlightPerKey;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    const WAIT: Duration = Duration::from_millis(300);
    /// What a wait may take past itself on a busy runner.
    const SLACK: Duration = Duration::from_secs(2);

    #[test]
    fn a_start_that_hangs_holds_its_session_alone() {
        let starts: &'static OneInFlightPerKey<Option<u32>, TrayStart> =
            Box::leak(Box::new(OneInFlightPerKey::new("tray-respawn")));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let began = Arc::new(AtomicUsize::new(0));
        let held = |gate: &Arc<(Mutex<bool>, Condvar)>, began: &Arc<AtomicUsize>| {
            let (gate, began) = (gate.clone(), began.clone());
            move || {
                began.fetch_add(1, SeqCst);
                let (open, opened) = &*gate;
                let _open = opened
                    .wait_while(open.lock().unwrap(), |open| !*open)
                    .unwrap();
                TrayStart::Started {
                    pid: 1,
                    session: Some(1),
                }
            }
        };
        /// Opens the gate however the test ends.
        struct Open(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Open {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_all();
            }
        }
        let open = Open(gate.clone());

        let asked = Instant::now();
        let first = start_within(starts, Some(1), WAIT, held(&gate, &began));
        let waited = asked.elapsed();
        assert!(
            matches!(&first, TrayStart::Failed(why) if why.contains("no answer within 300ms")),
            "{first:?}"
        );
        assert!((WAIT..WAIT + SLACK).contains(&waited), "{waited:?}");

        let asked = Instant::now();
        let other = start_within(starts, Some(3), WAIT, || TrayStart::Started {
            pid: 3,
            session: Some(3),
        });
        assert_eq!(
            other,
            TrayStart::Started {
                pid: 3,
                session: Some(3)
            }
        );
        assert!(
            asked.elapsed() < WAIT,
            "the next session goes ahead at once"
        );

        let again = start_within(starts, Some(1), WAIT, held(&gate, &began));
        assert!(matches!(again, TrayStart::Failed(_)), "{again:?}");
        assert_eq!(
            began.load(SeqCst),
            1,
            "one start for session 1 while it hangs"
        );

        drop(open);
        let deadline = Instant::now() + SLACK;
        while starts.keys_in_flight() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let after = start_within(starts, Some(1), WAIT, || TrayStart::Started {
            pid: 11,
            session: Some(1),
        });
        assert_eq!(
            after,
            TrayStart::Started {
                pid: 11,
                session: Some(1)
            },
            "once it ends, the next start is its own"
        );
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
            || std::future::ready(Some("0.4.221".to_string())),
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
                move || std::future::ready(marker.lock().unwrap().clone())
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
            || std::future::ready(Some("0.4.212".to_string())),
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

/// #3857 — what [`graphical_session_within`] gave instead of a session.
#[cfg(all(
    target_os = "linux",
    any(feature = "portal-capture", feature = "recording")
))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoGraphicalSession {
    /// logind answered, and no session matched: the walk's own words
    /// ([`graphical_session_via`]), as they always were. Nobody is at the
    /// screen, or only the login screen is.
    Answered(String),
    /// No answer in time, or a walk that ended with none. Who is at the
    /// screen is not known, which is never "nobody" (#3688).
    NotKnown(String),
}

#[cfg(all(
    target_os = "linux",
    any(feature = "portal-capture", feature = "recording")
))]
impl NoGraphicalSession {
    /// Why there is no session, in words: what the recorder's launch says.
    #[cfg_attr(not(feature = "recording"), allow(dead_code))]
    pub(crate) fn words(&self) -> &str {
        match self {
            Self::Answered(why) | Self::NotKnown(why) => why,
        }
    }
}

/// #3857 — how long a caller on a blocking thread waits for the graphical
/// session walk ([`graphical_session_within`]): the seat and account lookups'
/// 1.5 s (`files::SEAT_LOOKUP_WAIT`, `exec::ACCOUNT_LOOKUP_WAIT`). logind's
/// own D-Bus timeout is 25 s, and every `loginctl` waits it out. A healthy
/// walk is a few `loginctl` processes, tens of milliseconds, far less than
/// half of this, and half is what one walk in flight needs to answer every
/// caller (`one_in_flight`'s cost). On the consent path the tray's start
/// (2.5 s at most) and this wait together stay inside the 5 s the hub waits
/// past a prompt's window (`CONSENT_VERDICT_GRACE`), so the agent's
/// `no_prompt_surface` reaches the controller ahead of the hub's own timer.
#[cfg(all(
    target_os = "linux",
    any(feature = "portal-capture", feature = "recording")
))]
pub(crate) const SESSION_WALK_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

/// #3857 — the graphical session for a caller on a blocking thread: the
/// portal's session helper (`capture::portal::helper::spawn_child`, so the
/// capture, the audio, keep busy and the notification prompt) and the
/// recorder's launch (`recording::launch::unix::spawn_session_user`).
///
/// The walk ([`graphical_session_matching`]) runs on the thread of the one
/// walk in flight for what is asked ([`SESSION_WALKS`]), and this waits
/// [`SESSION_WALK_WAIT`] at most. Before, each caller walked on its own
/// thread for as long as logind took, and a stopped logind answers each
/// `loginctl` only at its D-Bus timeout, about 25 s.
#[cfg(all(
    target_os = "linux",
    any(feature = "portal-capture", feature = "recording")
))]
pub(crate) fn graphical_session_within(
    want_uid: Option<u32>,
    users_only: bool,
) -> std::result::Result<GraphicalSession, NoGraphicalSession> {
    let wait = SESSION_WALK_WAIT;
    // A test stands the walk in for its own thread or task, with walks in
    // flight of its own.
    #[cfg(test)]
    if let Some(stood) = session_walk_stand_in::now() {
        return walked_in_time(stood.ask_blocking(want_uid, users_only, wait), wait);
    }
    let asked = SESSION_WALKS.ask_blocking((want_uid, users_only), wait, move || {
        graphical_session_matching(want_uid, users_only).map_err(|e| format!("{e:#}"))
    });
    walked_in_time(asked, wait)
}

/// #3857 — the graphical session walks in flight, one for each kind of ask
/// (the uid asked for, and whether only a person's session will do), each on
/// a thread named `session-walk` (`one_in_flight`'s rules). While logind
/// hangs, the callers that ask the same hold that one walk's thread however
/// many they are, and each waits [`SESSION_WALK_WAIT`] at most. A root
/// daemon's portal helper asks `(None, false)`, a per-user daemon's its own
/// uid, and the recorder the person's uid, persons only.
#[cfg(all(
    target_os = "linux",
    any(feature = "portal-capture", feature = "recording")
))]
static SESSION_WALKS: crate::one_in_flight::OneInFlightPerKey<
    (Option<u32>, bool),
    std::result::Result<GraphicalSession, String>,
> = crate::one_in_flight::OneInFlightPerKey::new("session-walk");

/// #3857 — what a caller that waited `wait` takes from the walk in flight:
/// the session, logind's own words when none matched, or "not known" with
/// the reason when the walk gave no answer in time or ended with none. The
/// walk goes on on its own thread, and its late answer reaches no one.
#[cfg(all(
    target_os = "linux",
    any(feature = "portal-capture", feature = "recording")
))]
fn walked_in_time(
    asked: crate::one_in_flight::Asked<std::result::Result<GraphicalSession, String>>,
    wait: std::time::Duration,
) -> std::result::Result<GraphicalSession, NoGraphicalSession> {
    use crate::one_in_flight::Asked;
    match asked {
        Asked::Answer(Ok(session)) => Ok(session),
        Asked::Answer(Err(words)) => Err(NoGraphicalSession::Answered(words)),
        Asked::Failed(why) => Err(NoGraphicalSession::NotKnown(format!(
            "the graphical session walk ended with no answer: {why}"
        ))),
        Asked::OutOfTime => Err(NoGraphicalSession::NotKnown(format!(
            "logind (`loginctl`) gave no answer about the graphical session within {wait:?}; the \
             walk goes on on its own thread, and the next ask waits for it"
        ))),
    }
}

/// #3857 — the graphical session walk stood in for one test: `walk` in place
/// of `loginctl`'s ([`graphical_session_matching`]). Installed for a thread
/// ([`blocking`]: the portal helper's and the recorder's callers ask from
/// their own) or for a task ([`asking`]: the notification prompt's start,
/// which carries it onto the blocking thread its show runs on). Never the
/// daemon's own walks, and never the tests beside it.
#[cfg(all(
    test,
    target_os = "linux",
    any(feature = "portal-capture", feature = "recording")
))]
pub(crate) mod session_walk_stand_in {
    use super::GraphicalSession;
    use crate::one_in_flight::{Asked, OneInFlightPerKey};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::{Arc, Condvar, Mutex, PoisonError};

    /// The walk: `(want_uid, users_only)` in, a session or logind's words out.
    type Walk = dyn Fn(Option<u32>, bool) -> Result<GraphicalSession, String> + Send + Sync;

    /// Walks in flight, one per kind of ask, as the daemon's.
    type Walks = OneInFlightPerKey<(Option<u32>, bool), Result<GraphicalSession, String>>;

    /// A walk stood in, with walks in flight of its own: one a test holds up
    /// never holds up the tests beside it, which ask the daemon's.
    #[derive(Clone)]
    pub(crate) struct StandIn {
        walk: Arc<Walk>,
        walks: &'static Walks,
    }

    impl StandIn {
        pub(crate) fn new(
            walk: impl Fn(Option<u32>, bool) -> Result<GraphicalSession, String> + Send + Sync + 'static,
        ) -> Self {
            Self {
                walk: Arc::new(walk),
                walks: Box::leak(Box::new(OneInFlightPerKey::new("session-walk"))),
            }
        }

        /// A walk that logind holds until `held` opens, then answers
        /// `session`. Each walk that begins is counted in `held`.
        pub(crate) fn held(held: &Held, session: GraphicalSession) -> Self {
            let held = held.clone();
            Self::new(move |_, _| {
                held.hold();
                Ok(session.clone())
            })
        }

        /// The walk, through this stand-in's walks in flight, as the
        /// daemon's goes through its own: `wait` at most.
        pub(super) fn ask_blocking(
            &self,
            want_uid: Option<u32>,
            users_only: bool,
            wait: std::time::Duration,
        ) -> Asked<Result<GraphicalSession, String>> {
            let walk = self.walk.clone();
            self.walks
                .ask_blocking((want_uid, users_only), wait, move || {
                    walk(want_uid, users_only)
                })
        }
    }

    /// A held logind: each walk that reaches it is counted, and held until
    /// the gate opens.
    #[derive(Clone, Default)]
    pub(crate) struct Held(Arc<HeldState>);

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

        pub(crate) fn open(&self) {
            *self.0.open.lock().unwrap_or_else(PoisonError::into_inner) = true;
            self.0.opened.notify_all();
        }

        /// Walks that reached logind.
        pub(crate) fn began(&self) -> usize {
            self.0.began.load(SeqCst)
        }
    }

    /// Opens the gate when dropped: a failed assertion leaves nothing held,
    /// and a runtime declared before it can shut down.
    pub(crate) struct OpenedAtTheEnd(pub(crate) Held);

    impl Drop for OpenedAtTheEnd {
        fn drop(&mut self) {
            self.0.open();
        }
    }

    /// The person at the screen, as logind reports them.
    pub(crate) fn ana() -> GraphicalSession {
        GraphicalSession {
            uid: 4242,
            name: "ana".into(),
            display: Some(":0".into()),
            wayland_display: None,
        }
    }

    tokio::task_local! {
        static STOOD: StandIn;
    }

    std::thread_local! {
        static HERE: std::cell::RefCell<Option<StandIn>> = const { std::cell::RefCell::new(None) };
    }

    /// `run`, the walks its task asks for taking `stood`'s: the notification
    /// prompt's tests, so a build without the notification has no reader.
    #[cfg_attr(not(feature = "portal-capture"), allow(dead_code))]
    pub(crate) async fn asking<F: std::future::Future>(stood: StandIn, run: F) -> F::Output {
        STOOD.scope(stood, run).await
    }

    /// `run` on THIS thread, the walks it asks for taking `stood`'s. What
    /// stood here before comes back after.
    pub(crate) fn blocking<R>(stood: &StandIn, run: impl FnOnce() -> R) -> R {
        /// Puts back what stood here before, however `run` ends.
        struct Restore(Option<StandIn>);
        impl Drop for Restore {
            fn drop(&mut self) {
                let before = self.0.take();
                HERE.with(|here| *here.borrow_mut() = before);
            }
        }
        let _restore = Restore(HERE.with(|here| here.replace(Some(stood.clone()))));
        run()
    }

    /// The walk stood in for this thread, or else for this task.
    pub(crate) fn now() -> Option<StandIn> {
        HERE.with(|here| here.borrow().clone())
            .or_else(|| STOOD.try_with(StandIn::clone).ok())
    }
}

/// #3857 — the graphical session walk of the callers on blocking threads,
/// with logind stood in ([`session_walk_stand_in`]). Each ask runs on a
/// thread of its own, as the portal helper's and the recorder's callers ask.
#[cfg(all(
    test,
    target_os = "linux",
    any(feature = "portal-capture", feature = "recording")
))]
mod session_walk_tests {
    use super::session_walk_stand_in::{Held, OpenedAtTheEnd, StandIn, ana, blocking};
    use super::{
        GraphicalSession, NoGraphicalSession, SESSION_WALK_WAIT, graphical_session_within,
    };
    use std::time::{Duration, Instant};

    /// What an ask may take past the walk's wait on a busy runner.
    const SLACK: Duration = Duration::from_secs(1);
    /// What a walk that answers may take on a busy runner.
    const RUNS_WITHIN: Duration = Duration::from_secs(10);

    type Walked = Result<GraphicalSession, NoGraphicalSession>;

    /// `graphical_session_within` on a thread of its own, taking `stood`'s
    /// walk: what it answered, and how long that took.
    fn ask_on_a_thread(
        stood: &StandIn,
        want_uid: Option<u32>,
        users_only: bool,
    ) -> std::sync::mpsc::Receiver<(Walked, Duration)> {
        let (tx, rx) = std::sync::mpsc::channel();
        let stood = stood.clone();
        std::thread::spawn(move || {
            let asked = Instant::now();
            let walked = blocking(&stood, || graphical_session_within(want_uid, users_only));
            let _ = tx.send((walked, asked.elapsed()));
        });
        rx
    }

    /// #3857 — while logind holds a walk, asks at once and in a row share
    /// it: each is told the session is not known within its own wait, never
    /// that nobody is there, and none starts another walk. Once logind
    /// answers, the late walk answers nobody, and the next ask walks afresh.
    ///
    /// Red before #3857: every ask walked on its own thread, for as long as
    /// logind held it.
    #[test]
    fn asks_while_a_walk_hangs_share_it() {
        let held = Held::default();
        let _opened = OpenedAtTheEnd(held.clone());
        let stood = StandIn::held(&held, ana());
        let at_once: Vec<_> = (0..4)
            .map(|_| ask_on_a_thread(&stood, None, false))
            .collect();
        for asked in at_once {
            let (walked, waited) = asked
                .recv_timeout(SESSION_WALK_WAIT + SLACK)
                .expect("every ask at once is answered while logind holds the walk");
            match walked {
                Err(NoGraphicalSession::NotKnown(why)) => assert!(
                    why.contains("no answer"),
                    "the reason says logind gave no answer: {why}"
                ),
                other => panic!("logind held the walk, and it answered {other:?}"),
            }
            assert!(
                waited >= SESSION_WALK_WAIT,
                "an ask gave up in {waited:?}, before its wait"
            );
        }
        for _ in 0..2 {
            let (walked, _) = ask_on_a_thread(&stood, None, false)
                .recv_timeout(SESSION_WALK_WAIT + SLACK)
                .expect("an ask in a row is answered while logind holds the walk");
            assert!(
                matches!(walked, Err(NoGraphicalSession::NotKnown(_))),
                "logind held the walk, and it answered {walked:?}"
            );
        }
        assert_eq!(held.began(), 1, "one walk, however many asked");
        held.open();
        let (walked, _) = ask_on_a_thread(&stood, None, false)
            .recv_timeout(RUNS_WITHIN)
            .expect("the next ask is answered");
        assert_eq!(walked, Ok(ana()), "a walk of its own");
        assert_eq!(held.began(), 2, "the held walk, and the next ask's own");
    }

    /// The control: a walk in time answers as before. The session it found,
    /// for the uid and the class each caller asks for, and logind's own words
    /// when nobody is there.
    #[test]
    fn a_walk_in_time_answers_as_today() {
        const NOBODY: &str = "no active graphical session — nobody is at this machine's screen";
        let stood = StandIn::new(|want_uid, users_only| match (want_uid, users_only) {
            (None, false) | (Some(4242), _) => Ok(ana()),
            _ => Err(NOBODY.into()),
        });
        let ask = |want_uid, users_only| {
            ask_on_a_thread(&stood, want_uid, users_only)
                .recv_timeout(RUNS_WITHIN)
                .expect("a walk in time is answered")
                .0
        };
        assert_eq!(ask(None, false), Ok(ana()), "root's walk: anyone's session");
        assert_eq!(ask(Some(4242), true), Ok(ana()), "the person's own");
        assert_eq!(
            ask(Some(1000), true),
            Err(NoGraphicalSession::Answered(NOBODY.into())),
            "logind's own words when nobody is there"
        );
    }
}

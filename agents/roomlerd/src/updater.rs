// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Self-update against GitHub Releases.
//!
//! Polls `https://api.github.com/repos/gjovanov/roomler-ai/releases/latest`
//! every ~6 h, compares the release tag to the running binary's
//! `CARGO_PKG_VERSION`, and — when newer — downloads the platform-
//! appropriate installer (MSI / .deb / .pkg) and spawns it detached.
//!
//! Scope: the agent exits after spawning the installer so the installer
//! can overwrite the binary without `ERROR_SHARING_VIOLATION`. The
//! Scheduled Task / systemd unit / LaunchAgent registered via
//! `roomlerd service install` re-launches the new version on
//! the next login (Windows) or immediately (Restart=on-failure on
//! Linux, KeepAlive on macOS).
//!
//! Trust model: we assume GitHub-over-TLS is sufficient for now. No
//! signature check beyond the MSI's cargo-wix / codesign identity
//! (which the OS verifies at install time).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;
use tunnel_core::env::node_env;

/// GitHub "Releases" repo slug. Centralised here so a fork can redirect
/// its update feed without grepping the codebase.
pub const RELEASES_REPO: &str = "gjovanov/roomler-ai";

/// Default proxy endpoint that caches GitHub's releases response on
/// the roomler-ai API server. Eliminates the per-IP GitHub rate
/// limit (60 req/hr unauth) that bites fleets of agents behind one
/// NAT. Override via `ROOMLERD_UPDATE_URL` env var for self-
/// hosted deployments or to bypass the proxy in dev. When the proxy
/// is unreachable we fall back to direct GitHub.
pub const DEFAULT_PROXY_URL: &str = "https://roomler.ai/api/agent/latest-release";

/// How often `run_periodic` wakes up and checks for a newer release.
///
/// A5 (2026-08-02): 24 h → **4 h**. The 24 h default was the fleet's
/// wedge-heal ceiling: an agent whose control WS was wedged/split missed
/// the server-pushed `rc:agent.update` (it rides that WS) and then sat
/// broken for up to a day (field: winhost-a/winhost-b ran a 4-day-old build
/// through the S6 split because their next check was hours away). The
/// original 24 h choice guarded GitHub's 60-req/IP/h unauthenticated
/// quota (field 2026-04-27: 8 MSI installs across 5 NAT'd boxes hit 403)
/// — but the check has been roomler.ai-proxy-first for a long time
/// (`DEFAULT_PROXY_URL`, server-side 1 h cache), so GitHub only sees
/// fallback traffic. 4 h keeps even a large NAT'd office far under the
/// quota on the fallback path. Operators tune via
/// `update_check_interval_h` / `ROOMLERD_UPDATE_INTERVAL_H`.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(4 * 3600);

/// Minimum download size before we trust an installer artifact. A
/// GitHub redirect to a deleted asset returns a tiny HTML page; this
/// guards against running that as an installer.
pub const MIN_INSTALLER_BYTES: usize = 1_000_000;

/// At-startup update-check cooldown. If we last spawned an installer
/// within this window, skip the immediate `check_once` and proceed
/// straight to the periodic interval. Prevents the install-storm
/// failure mode found on host `operator` (2026-05-02): SCM service
/// supervisor + auto-updater + freshly-downloaded MSI = each newly
/// spawned worker re-detects the same pending update, fires another
/// installer, exits clean (code=0), supervisor respawns, repeat. The
/// 0.1.61 supervisor patch (code=0 -> immediate respawn, no backoff)
/// makes the cycle tighter (~1.5 s per turn). 5 minutes is more than
/// enough headroom for a Win11 MSI to land + the new binary to start
/// up + reach the clean-run threshold.
pub const STARTUP_UPDATE_COOLDOWN: Duration = Duration::from_secs(300);

/// rc.19: short-cycle interval when the auto-updater defers because
/// file transfers are in flight. Keeps checking every hour so the
/// install fires shortly after the operator's last upload completes,
/// without waiting the full 24h CHECK_INTERVAL between checks.
pub const TRANSFER_DEFER_RECHECK: Duration = Duration::from_secs(3600);

/// rc.19: max number of consecutive defers before the auto-updater
/// fires anyway. Prevents a power-user who uploads every few hours
/// from indefinitely delaying security patches. 7 consecutive
/// defers at 1h cadence = ~7h max delay window (after the original
/// 24h interval elapsed); 7 at 24h = ~7 days.
pub const MAX_CONSECUTIVE_DEFERS: u32 = 7;

/// R5b — the network moved within this window ⇒ defer the periodic
/// update. An install restarts the daemon, which forfeits every
/// ESTABLISHED (grandfathered) flow — and on a corp path that
/// blackholes fresh TLS, a restart adjacent to a VPN transition locks
/// the machine out until the next transition (field 2026-08-16: both
/// control WSs RST at capture; only established flows rode through).
pub const NET_TRANSITION_WINDOW: Duration = Duration::from_secs(600);

/// R5b — short-cycle recheck while net-transition-deferred (the
/// transition settles in minutes, unlike an operator's upload session).
/// With the shared MAX_CONSECUTIVE_DEFERS cap the worst added delay is
/// ~14 min before the update forces through.
pub const NET_DEFER_RECHECK: Duration = Duration::from_secs(120);

/// rc.19: gating decision for the periodic loop given the current
/// active-transfer count and consecutive-defer counter. Pure helper
/// so the gating logic is unit-testable without spinning the full
/// loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferDecision {
    /// Run the update check this cycle.
    Proceed,
    /// Skip this cycle; re-check in `TRANSFER_DEFER_RECHECK`.
    DeferOnce,
    /// Force the update through (consecutive defer limit reached).
    /// Logs warn at the call site.
    ForceAfterDefers,
}

/// Decide whether to gate the update check this cycle. Returns
/// `Proceed` when nothing gates (no transfers active, no fresh network
/// transition) OR the consecutive-defer limit has been reached. Pure /
/// no I/O — tested directly.
pub fn decide_defer(active: usize, net_transition: bool, consecutive_defers: u32) -> DeferDecision {
    if active == 0 && !net_transition {
        DeferDecision::Proceed
    } else if consecutive_defers >= MAX_CONSECUTIVE_DEFERS {
        DeferDecision::ForceAfterDefers
    } else {
        DeferDecision::DeferOnce
    }
}

/// R5b — did a material MAJOR network change land inside
/// [`NET_TRANSITION_WINDOW`]? Feature-shaped: no-overlay builds have no
/// netstate and never defer on this input.
fn net_transition_recent() -> bool {
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    {
        tunnel_core::overlay::netstate::last_major_within(NET_TRANSITION_WINDOW)
    }
    #[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
    {
        false
    }
}

/// Marker file the agent touches *before* spawning the installer.
/// Path lives next to `last-install.json` so all update-related
/// state is in one directory and gets cleaned up by the same log-
/// retention policy. Returns `None` only when the platform doesn't
/// expose a data dir (very-stripped-down environments + tests
/// without `init()`).
pub fn update_attempt_marker_path() -> Option<PathBuf> {
    crate::logging::log_dir().map(|d| d.join("update-attempt"))
}

/// Touch the update-attempt marker. Call right before
/// `spawn_installer_inner` so the cooldown starts ticking from the
/// moment the installer process is launched. Best-effort: any I/O
/// failure is logged but does not block the update path (we'd
/// rather have a working install with a noisy crash counter than
/// no install at all).
fn record_update_attempt() {
    let Some(p) = update_attempt_marker_path() else {
        return;
    };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&p, format!("{}\n", chrono::Utc::now().to_rfc3339())) {
        tracing::warn!(error = %e, path = %p.display(), "could not write update-attempt marker");
    }
}

/// Whether an update was attempted within the last `cooldown` seconds.
/// Read by `run_periodic` on its first iteration to suppress the
/// at-startup check when an install is already in flight.
fn recent_update_attempt(cooldown: Duration) -> bool {
    update_attempt_marker_path().is_some_and(|p| recent_update_attempt_at(&p, cooldown))
}

/// Inner pure-fn variant of `recent_update_attempt`. Takes the marker
/// path as an explicit argument so unit tests can drive it against a
/// `tempfile::TempDir` without depending on `logging::init()`.
fn recent_update_attempt_at(marker_path: &std::path::Path, cooldown: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(marker_path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    let elapsed = std::time::SystemTime::now()
        .duration_since(mtime)
        .unwrap_or_default();
    elapsed < cooldown
}

/// A parsed release from the GitHub API. Only the fields we need.
#[derive(Debug, Deserialize)]
pub struct GithubRelease {
    pub tag_name: String,
    pub assets: Vec<GithubAsset>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    #[allow(dead_code)]
    pub prerelease: bool,
}

#[derive(Debug, Deserialize)]
pub struct GithubAsset {
    pub name: String,
    pub browser_download_url: String,
    /// Kept in the wire deserialisation so future logic (e.g.
    /// comparing against a content-length header) can consult it.
    /// Not currently read by the in-loop path.
    #[serde(default)]
    #[allow(dead_code)]
    pub size: u64,
    /// GitHub Releases API exposes a `digest` field per asset of
    /// the form `"sha256:<hex>"` (added late 2024). When present,
    /// [`download_asset`] verifies the bytes' SHA256 against this
    /// hash and rejects mismatches. Absent on pre-2024 releases or
    /// when the proxy isn't forwarding it (older API server) — in
    /// that case we fall through to the [`MIN_INSTALLER_BYTES`]
    /// size floor as the only integrity gate.
    #[serde(default)]
    pub digest: Option<String>,
}

/// The outcome of a single check cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Running the latest (or newer) version; nothing to do.
    UpToDate { current: String, latest: String },
    /// Newer release found; installer downloaded to `installer_path`.
    /// Caller is responsible for spawning it and exiting.
    UpdateReady {
        current: String,
        latest: String,
        installer_path: PathBuf,
    },
    /// Check failed for an expected reason (network, GitHub 403, no
    /// matching asset for this platform). Logged but non-fatal.
    Skipped(String),
}

/// Parse a git tag like `agent-v0.1.36`, `v0.1.36`, or
/// `agent-v0.3.0-rc.4` into a 4-tuple `(major, minor, patch, pre)`
/// for ordering. The `pre` field is `u64::MAX` for a non-pre-release
/// (final) version and the rc number for `-rc.N` / `-rcN` /
/// `-rc-N` pre-releases. This makes the natural tuple ordering match
/// semver: `0.3.0-rc.1 < 0.3.0-rc.4 < 0.3.0-rc.99 < 0.3.0`.
///
/// Unparseable tags compare as None and are treated as "not newer"
/// so a malformed server-side tag can't force a downgrade.
///
/// Field bug 2026-05-06: pre-0.3.0 implementation only returned a
/// 3-tuple; rc.3 vs rc.4 both parsed to `(0, 3, 0)` and
/// `is_newer(rc.4, rc.3)` returned false. The auto-updater logged
/// "up to date current=rc.3 latest=rc.4" indefinitely.
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64, u64)> {
    let stripped = tag.trim_start_matches("agent-");
    let stripped = stripped.trim_start_matches('v');

    // Split on the FIRST '-' so the core (major.minor.patch) and the
    // pre-release suffix (rc.N / build.42 / etc.) are isolated.
    let (core, pre) = match stripped.find('-') {
        Some(i) => (&stripped[..i], Some(&stripped[i + 1..])),
        None => (stripped, None),
    };

    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() < 3 {
        return None;
    }
    let major = parts[0].parse::<u64>().ok()?;
    let minor = parts[1].parse::<u64>().ok()?;
    // After the '-' split, the patch is bare digits. If anything
    // non-digit-trailing snuck through (e.g. a build-metadata "+42"
    // that the find('-') missed), strip it for tolerance.
    let patch_str = parts[2].split(|c: char| !c.is_ascii_digit()).next()?;
    let patch = patch_str.parse::<u64>().ok()?;

    // Pre-release rank. Final (no pre-release) is highest so it
    // outranks every rc.N. Unknown pre-release labels also rank
    // u64::MAX so a forward-compat tag like `1.0.0-beta.5` doesn't
    // accidentally rank below an rc.
    let pre_rank = match pre {
        None => u64::MAX,
        Some(p) => parse_rc_rank(p).unwrap_or(u64::MAX),
    };

    Some((major, minor, patch, pre_rank))
}

/// Parse the pre-release suffix portion (after the leading `-`) for
/// `rc.N` / `rcN` / `rc-N` shapes. Returns `None` for non-rc
/// pre-releases — caller treats those as final-equivalent.
fn parse_rc_rank(pre: &str) -> Option<u64> {
    let after_rc = pre
        .strip_prefix("rc.")
        .or_else(|| pre.strip_prefix("rc-"))
        .or_else(|| pre.strip_prefix("rc"))?;
    after_rc.parse::<u64>().ok()
}

/// Return true if `latest` strictly outranks `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Which Windows MSI flavour the running agent was installed with.
/// Used by [`pick_asset_for_platform`] to download the matching MSI
/// for in-place upgrade — installing the wrong flavour silently fails
/// the launch-condition check shipped in 0.2.5 and the auto-update
/// loop never makes forward progress (field repro: the field-test host
/// 2026-05-02, perUser agent on 0.2.0 picked the perMachine 0.2.5 MSI
/// alphabetically; UAC-elevated install rejected by the cross-flavour
/// guard; agent restarted at 0.2.0 forever).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsInstallFlavour {
    PerUser,
    PerMachine,
}

/// Discover this agent's install flavour from the running exe path.
/// Heuristic: anything under `\Program Files` (with or without ` (x86)`)
/// is perMachine; everything else (including `%LOCALAPPDATA%\Programs\`)
/// is perUser. Defaults to perUser on lookup failure — that matches the
/// historical install mode shipped before 0.2.1, and is the safe-side
/// guess because the perUser MSI installs without UAC and works against
/// any account.
#[cfg(target_os = "windows")]
pub fn current_install_flavour() -> WindowsInstallFlavour {
    let Ok(exe) = std::env::current_exe() else {
        return WindowsInstallFlavour::PerUser;
    };
    classify_install_flavour_from_path(&exe)
}

/// Pure-fn variant of [`current_install_flavour`] so unit tests can
/// drive it without a real filesystem. Lowercases for case-insensitive
/// match against the Windows convention `C:\Program Files\…`.
#[cfg(target_os = "windows")]
pub fn classify_install_flavour_from_path(p: &std::path::Path) -> WindowsInstallFlavour {
    let lower = p.to_string_lossy().to_lowercase();
    // Match both `\program files\` and `\program files (x86)\`. Use
    // path-separator-bracketed substring so a project literally named
    // "ProgramFiles" elsewhere on disk doesn't trip the check.
    if lower.contains("\\program files (x86)\\") || lower.contains("\\program files\\") {
        WindowsInstallFlavour::PerMachine
    } else {
        WindowsInstallFlavour::PerUser
    }
}

/// The MSI install folder name since P4b — both wxs files set
/// `APPLICATIONFOLDER Name='Roomler'`. `pub` (not `pub(crate)`): the
/// roomler-setup wizard's daemon orchestrator derives the CLI's
/// post-install path from it (same visibility-lift precedent as the
/// rc.27 wizard helpers).
#[cfg(target_os = "windows")]
pub const INSTALL_FOLDER_NAME: &str = "Roomler";

/// The pre-P4b install folder name both MSI flavours used through
/// rc.194. The `cleanup-legacy-install` vacated-dir sweep targets
/// this after a rename-hop upgrade.
#[cfg(target_os = "windows")]
// RETIRED-NAME-ANCHOR: the cleanup sweep needs the OLD folder name to find the
// vacated directory after a rename-hop upgrade. See docs/fr/FR-21.
pub(crate) const LEGACY_INSTALL_FOLDER_NAME: &str = "roomler-agent";

/// Resolve a flavour's MSI install directory for a given folder name:
/// perUser → `%LOCALAPPDATA%\Programs\<name>`, perMachine →
/// `%ProgramFiles%\<name>` (the same roots the two wxs files install
/// under). Callers pass [`INSTALL_FOLDER_NAME`] to find the current
/// install (post-install watcher fallback) or
/// [`LEGACY_INSTALL_FOLDER_NAME`] to find the directory a rename-hop
/// upgrade vacated (`install_cleanup` sweep). `None` when the root
/// env var is unset (effectively never on a real Windows session).
#[cfg(target_os = "windows")]
pub fn install_dir_with_name(
    flavour: WindowsInstallFlavour,
    folder_name: &str,
) -> Option<std::path::PathBuf> {
    match flavour {
        WindowsInstallFlavour::PerUser => std::env::var_os("LOCALAPPDATA").map(|root| {
            std::path::PathBuf::from(root)
                .join("Programs")
                .join(folder_name)
        }),
        WindowsInstallFlavour::PerMachine => std::env::var_os("ProgramFiles")
            .map(|root| std::path::PathBuf::from(root).join(folder_name)),
    }
}

/// Pick the asset that matches this build's platform. Returns an
/// explicit `None` when there's no match so the caller can log + skip
/// rather than downloading something wrong.
///
/// On Windows the GitHub Release ships two MSI flavours per tag
/// (perUser + perMachine); pick the one matching the running install
/// so the in-place upgrade actually lands. See
/// [`WindowsInstallFlavour`] for the why.
pub fn pick_asset_for_platform(assets: &[GithubAsset]) -> Option<&GithubAsset> {
    #[cfg(target_os = "windows")]
    {
        pick_asset_for_windows(assets, current_install_flavour())
    }
    #[cfg(not(target_os = "windows"))]
    {
        pick_asset_for_unix(assets)
    }
}

// RETIRED-NAME-ANCHOR(2): PUBLISHED asset names. Already-released files cannot be
// renamed and the picker matches them, so they stay verbatim. docs/fr/FR-21
/// Pure Windows asset picker. Filters by the `-perMachine-` infix in
/// the asset filename (cargo-wix names them
/// `roomler-agent-<v>-perMachine-x86_64-…msi`; the perUser MSI uses
/// `roomler-agent-<v>-x86_64-…msi` with no infix). Falls back to "any
/// MSI" only if the matching flavour is missing — better to attempt a
/// cross-flavour install (which will silently no-op) than to skip the
/// update entirely on a release that, for whatever reason, only shipped
/// one flavour.
#[cfg(any(target_os = "windows", test))]
pub fn pick_asset_for_windows(
    assets: &[GithubAsset],
    flavour: WindowsInstallFlavour,
) -> Option<&GithubAsset> {
    let want_per_machine = matches!(flavour, WindowsInstallFlavour::PerMachine);
    // First pass: prefer the matching flavour.
    for a in assets {
        let lower = a.name.to_lowercase();
        if !lower.ends_with(".msi") {
            continue;
        }
        let is_per_machine = lower.contains("-permachine-");
        if is_per_machine == want_per_machine {
            return Some(a);
        }
    }
    // Fallback: any MSI. Logged at warn so the field can see when the
    // release is missing the matching flavour.
    for a in assets {
        if a.name.to_lowercase().ends_with(".msi") {
            tracing::warn!(
                asset = %a.name,
                flavour = ?flavour,
                "no MSI matching install flavour; falling back to any MSI"
            );
            return Some(a);
        }
    }
    None
}

/// Pure non-Windows asset picker. .deb on Linux, .pkg on macOS. Kept
/// separate from the Windows path so the flavour-discovery branch
/// doesn't compile on platforms that don't need it. `allow(dead_code)`
/// because Windows test builds compile this for symmetry but don't
/// call it (`pick_asset_for_platform` short-circuits to the Windows
/// path on Windows).
#[cfg(any(not(target_os = "windows"), test))]
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub fn pick_asset_for_unix(assets: &[GithubAsset]) -> Option<&GithubAsset> {
    let mac = cfg!(target_os = "macos");
    let linux = cfg!(target_os = "linux");
    // The .deb match MUST be arch-qualified. It used to accept any `.deb`,
    // which was harmless only while exactly one Linux .deb existed per
    // release; the moment a second architecture ships, an x86_64 agent
    // would happily download an arm64 package (asset order is GitHub's,
    // not ours) and dpkg it. Match the arch token the release filenames
    // already carry (`…-x86_64-unknown-linux-gnu.deb`), keeping the
    // legacy `_amd64.deb` cargo-deb spelling for old releases.
    let arch_tokens: &[&str] = if cfg!(target_arch = "x86_64") {
        &["x86_64", "amd64"]
    } else if cfg!(target_arch = "aarch64") {
        &["aarch64", "arm64"]
    } else {
        &[]
    };
    if mac {
        return assets
            .iter()
            .find(|a| a.name.to_lowercase().ends_with(".pkg"));
    }
    if !linux {
        return None;
    }
    pick_linux_asset(assets, arch_tokens, host_has_debian_tooling())
}

/// Is there anything on this host that can install a `.deb`?
///
/// Probes for the TOOL rather than reading `/etc/os-release`, because the
/// question that decides the download is "can the install actually run",
/// and a distro ID is only a proxy for it (a Debian derivative with dpkg
/// removed, or a Fedora box with dpkg added, both answer correctly here).
#[cfg(any(not(target_os = "windows"), test))]
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn host_has_debian_tooling() -> bool {
    ["dpkg", "apt-get"].iter().any(|bin| {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
            .unwrap_or(false)
    })
}

// RETIRED-NAME-ANCHOR: published asset names, as above. docs/fr/FR-21
/// Is this release asset the DAEMON's own package, as opposed to something
/// else in the same release that happens to be a `.deb` for the same arch?
///
/// FR-27 made this necessary: the Linux desktop companion now ships as its own
/// `roomler-desktop-…-x86_64-unknown-linux-gnu.deb`, so a release carries two
/// arch-matching `.deb`s and asset order is GitHub's, not ours. Without this
/// the daemon could download the COMPANION, `dpkg -i` it — which succeeds —
/// and never update itself: a silent, permanent freeze that looks like a
/// working update. Exactly the shape the arch guard in [`pick_linux_asset`]
/// was added for, one axis over.
///
/// Positive match, not a denylist of names we happen to know today. The
/// daemon's published asset names are a deliberately immutable surface
/// (`roomler-agent-…`, plus cargo-deb's `roomlerd_…` spelling on old
/// releases), so naming them is safe and a new sibling asset cannot creep
/// through by being unlisted. Locked by
/// `pick_linux_asset_never_takes_the_desktop_companion`.
///
/// Gated like its only caller: a Windows build has no Linux picker to call it.
#[cfg(any(not(target_os = "windows"), test))]
#[cfg_attr(target_os = "windows", allow(dead_code))]
// RETIRED-NAME-ANCHOR(2): the legacy arm. Since FR-46 (#1051) new releases publish
// `roomlerd-…`, so this prefix names only ALREADY-PUBLISHED assets — but it is a LIVE
// fallback, not decoration: a host updating from a release cut before the rename still
// finds its own `.deb` through it. Deletable once no such release is a candidate.
// Locked by `pick_linux_asset_takes_the_renamed_daemon_deb`. docs/fr/FR-46
fn is_daemon_asset(lower_name: &str) -> bool {
    lower_name.starts_with("roomler-agent") || lower_name.starts_with("roomlerd")
}

/// Choose the Linux asset: the right ARCH always, and the format this host
/// can actually install.
///
/// A `.deb` is preferred where dpkg/apt exist so the distro's package
/// manager stays the source of truth; everywhere else the self-contained
/// tarball is the only installable form. Falling back to the tarball when
/// a Debian host's `.deb` is missing keeps an absent artifact from becoming
/// a dead end. See `docs/linux-self-update.md`.
#[cfg(any(not(target_os = "windows"), test))]
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn pick_linux_asset<'a>(
    assets: &'a [GithubAsset],
    arch_tokens: &[&str],
    debian_tooling: bool,
) -> Option<&'a GithubAsset> {
    let for_arch = |suffix: &str| {
        assets.iter().find(|a| {
            let lower = a.name.to_lowercase();
            lower.ends_with(suffix)
                && arch_tokens.iter().any(|t| lower.contains(t))
                && is_daemon_asset(&lower)
        })
    };
    let deb = for_arch(".deb");
    let tar = for_arch(".tar.gz");
    let picked = if debian_tooling {
        deb.or(tar)
    } else {
        // No dpkg/apt: a .deb here would download and then fail to install,
        // which is how `Update all` looked like a silent no-op on a Fedora
        // host (2026-08-15).
        tar
    };
    match picked {
        Some(a) => tracing::debug!(
            asset = %a.name,
            debian_tooling,
            deb_available = deb.is_some(),
            tar_available = tar.is_some(),
            "update: picked Linux asset"
        ),
        None => tracing::warn!(
            debian_tooling,
            deb_available = deb.is_some(),
            tar_available = tar.is_some(),
            "update: no installable Linux asset for this host — \
             a .deb needs dpkg/apt, otherwise a .tar.gz is required"
        ),
    }
    picked
}

/// Fetch the list of releases. Uses the roomler-ai backend proxy by
/// default (caches GitHub's response for 1h on the API server, so a
/// fleet of agents shares a single upstream call), falls back to
/// direct GitHub when the proxy is unreachable. Override via
/// `ROOMLERD_UPDATE_URL` env var for self-hosted deployments.
///
/// We do NOT use GitHub's `/releases/latest` because that endpoint
/// excludes prereleases unconditionally, and our v0.x policy briefly
/// marked everything as prerelease — agents shipped with 0.1.36
/// silently 404'd on every check until the proxy + workflow fix
/// landed. Always pull the full list and let `pick_latest_release`
/// apply our own filter (draft=false + tag prefix + parseable).
async fn fetch_latest_release() -> Result<GithubRelease> {
    let proxy_url = node_env("UPDATE_URL").unwrap_or_else(|| DEFAULT_PROXY_URL.to_string());
    // Proxy first — handles rate limiting, returns the same JSON shape
    // as GitHub's /releases endpoint (slimmed to fields we read).
    match fetch_releases_from(&proxy_url).await {
        Ok(release) => return Ok(release),
        Err(e) => {
            tracing::info!(
                proxy = %proxy_url,
                error = %format!("{e:#}"),
                "update proxy unreachable; trying direct GitHub"
            );
        }
    }
    // Fallback — direct GitHub. Subject to the 60/hr unauth quota
    // but fine for occasional use when the proxy is offline.
    let github_url = format!("https://api.github.com/repos/{RELEASES_REPO}/releases?per_page=30");
    fetch_releases_from(&github_url).await
}

async fn fetch_releases_from(url: &str) -> Result<GithubRelease> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("roomlerd/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("building reqwest client")?;
    let resp = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("GET releases")?;
    if !resp.status().is_success() {
        // 403 from GitHub's REST API is the unauthenticated 60-req-per-
        // IP-per-hour quota tripping. Surface the reset window from
        // the rate-limit headers so the operator can see "wait 47
        // minutes" instead of just "got 403". Headers may be absent
        // on edge-network errors; default to a vague message when
        // they are.
        let status = resp.status();
        if status.as_u16() == 403 {
            let limit = resp
                .headers()
                .get("x-ratelimit-limit")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("?")
                .to_string();
            let remaining = resp
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("?")
                .to_string();
            let reset_unix = resp
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let resets_in_secs = reset_unix
                .map(|t| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    t.saturating_sub(now)
                })
                .unwrap_or(0);
            bail!(
                "GitHub API returned 403 Forbidden — rate-limited (limit={limit}, remaining={remaining}, resets in {resets_in_secs}s). Multiple agents on one IP share the unauthenticated 60/hr quota; cadence has been bumped to 24h to stay under it."
            );
        }
        bail!("GitHub API returned {}", status);
    }
    let releases: Vec<GithubRelease> = resp.json().await.context("parsing GitHub releases JSON")?;
    pick_latest_release(releases).context("no published agent-v* release found")
}

/// Given a vector of releases from GitHub (newest-first per API
/// contract), pick the highest-versioned `agent-v*` that isn't a
/// draft. Prereleases are tolerated because our 0.x history marked
/// them all that way and we still want those agents to update.
/// Exported for tests so the selection rule is locked.
pub fn pick_latest_release(mut releases: Vec<GithubRelease>) -> Option<GithubRelease> {
    releases.retain(|r| {
        !r.draft && r.tag_name.starts_with("agent-v") && parse_version(&r.tag_name).is_some()
    });
    if releases.is_empty() {
        return None;
    }
    releases.sort_by_key(|r| std::cmp::Reverse(parse_version(&r.tag_name)));
    releases.into_iter().next()
}

/// Download an asset to a temp file and return the path. Verifies the
/// downloaded size against the asset metadata + the minimum plausible
/// size so we don't run a ~200 byte HTML error page as an installer.
/// `pub(crate)`: the S1a companion-refresh (`crate::companion`) reuses
/// the same verified-download plumbing for `roomler-desktop.exe`.
///
/// `claimed_release` is the tag the manifest offered this asset AS
/// (`agent-v0.3.0-rc.458`). It is not decoration: the artifact has to
/// self-identify as that release or it is discarded — see
/// [`crate::artifact_version`] for the downgrade this closes.
pub(crate) async fn download_asset(asset: &GithubAsset, claimed_release: &str) -> Result<PathBuf> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("roomlerd/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(600))
        .build()
        .context("building download client")?;
    let resp = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .context("GET asset")?;
    if !resp.status().is_success() {
        bail!("asset download returned {}", resp.status());
    }
    let bytes = resp.bytes().await.context("reading asset body")?;
    if bytes.len() < MIN_INSTALLER_BYTES {
        bail!(
            "asset {} is implausibly small: {} bytes (minimum {})",
            asset.name,
            bytes.len(),
            MIN_INSTALLER_BYTES
        );
    }
    // Transport-integrity check: when GitHub / our proxy gave us a digest,
    // verify the downloaded bytes match. This catches corruption mid-flight
    // (rare with TLS but possible with broken middleboxes).
    //
    // ⚠️ It is NOT a tamper anchor, which is why the signature check below
    // exists: this digest arrives in the SAME manifest, from the SAME origin,
    // as the URL we just fetched — anyone able to serve one can serve the
    // other, so a hostile mirror simply supplies a matching hash.
    if let Some(digest) = asset.digest.as_deref() {
        verify_sha256(&bytes, digest)
            .with_context(|| format!("verifying digest for {}", asset.name))?;
    } else {
        tracing::warn!(
            asset = %asset.name,
            "no digest field on asset; falling through to size floor only"
        );
    }
    let dir = std::env::temp_dir().join("roomlerd-update");
    std::fs::create_dir_all(&dir).context("creating temp update dir")?;
    let path = dir.join(&asset.name);
    std::fs::write(&path, &bytes).context("writing installer to disk")?;

    // Publisher check — the anchor the serving channel cannot forge. Whatever
    // is on disk here is about to run as SYSTEM, so it must be attributable to
    // us and not merely to "someone with a code-signing certificate": a valid
    // Authenticode signature alone would let any commercial cert holder
    // through, so the signer's name is checked too.
    //
    // Fail-closed, and the failure is SKIPPING the update: the file is
    // discarded and the agent keeps running the version it already has. That
    // is deliberately the safe direction — a false negative costs an update
    // cycle, a false positive costs the fleet.
    //
    // Windows: Authenticode + the signer-name check.
    #[cfg(windows)]
    {
        use crate::code_signature::{EXPECTED_PUBLISHER, verify_publisher};
        match verify_publisher(&path, EXPECTED_PUBLISHER) {
            Ok(signer) => {
                tracing::info!(asset = %asset.name, %signer, "installer signature verified");
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                bail!(
                    "refusing to install {}: {e}. The download was discarded and this \
                     agent keeps its current version.",
                    asset.name
                );
            }
        }
    }

    // Linux/macOS: the detached GPG `.asc` verified against the key PINNED
    // in this binary (`pgp_verify`). Same fail-closed direction as Windows,
    // and the sidecar is REQUIRED: every release since rc.458 ships one, the
    // updater only ever moves FORWARD, and "the .asc is missing" means the
    // release's signing job failed — freezing on it, loudly, is correct.
    // (⚠️ the release workflow skips GPG signing gracefully when its secret
    // is absent; from this change on, that grace would freeze Linux/macOS
    // updates — the freeze is visible here as a refusal + sentinel, which is
    // the intended alarm, not a silent stall.)
    //
    // The sidecar URL is derived (`<asset-url>.asc` — GitHub names release
    // assets verbatim) and fetched over the SAME channel as the artifact.
    // That is fine: the point of the pin is that the channel cannot mint a
    // signature, only withhold one — and withholding is a refusal, not an
    // install.
    #[cfg(not(windows))]
    {
        let asc_url = format!("{}.asc", asset.browser_download_url);
        let asc = async {
            let resp = client
                .get(&asc_url)
                .send()
                .await
                .context("GET .asc sidecar")?;
            if !resp.status().is_success() {
                bail!("sidecar download returned {}", resp.status());
            }
            resp.text().await.context("reading .asc body")
        }
        .await;
        let verdict = match asc {
            Ok(asc) => crate::pgp_verify::verify_release_artifact(&bytes, &asc),
            Err(e) => Err(e.context(format!("fetching {asc_url}"))),
        };
        match verdict {
            Ok(()) => {
                tracing::info!(
                    asset = %asset.name,
                    "installer .asc verified against the pinned release signing key"
                );
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                bail!(
                    "refusing to install {}: {e:#}. The download was discarded and this \
                     agent keeps its current version.",
                    asset.name
                );
            }
        }
    }

    // Version binding — the check above proves the bytes are OURS, this one
    // proves they are the RELEASE we were told to install. Both are needed:
    // `is_newer` decided to upgrade by reading the MANIFEST's tag, while the
    // signature verified the ARTIFACT, and until now nothing connected the
    // two. See `crate::artifact_version` for the full downgrade.
    //
    // Same fail-closed direction as the signature check, for the same reason:
    // a refusal costs an update cycle, a wrongly-accepted payload costs the
    // fleet.
    match crate::artifact_version::verify_artifact_version(&path, claimed_release) {
        Ok(found) => {
            tracing::info!(
                asset = %asset.name,
                claimed = %claimed_release,
                artifact_version = %found,
                "artifact self-identifies as the release it was offered as"
            );
        }
        // Not a refusal: the format carries no signature for a version to be
        // anchored to, so enforcing one would check a claim against a claim.
        // Logged at INFO rather than WARN — it is the expected steady state on
        // Linux/macOS today, and a warning per update check would train
        // operators to ignore it.
        Err(e @ crate::artifact_version::VersionError::Unsupported(_)) => {
            tracing::info!(asset = %asset.name, reason = %e, "no artifact-version binding available");
        }
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            bail!(
                "refusing to install {}: {e}. The download was discarded and this \
                 agent keeps its current version.",
                asset.name
            );
        }
    }

    Ok(path)
}

/// Verify a payload's SHA256 against a `"<algo>:<hex>"` formatted
/// digest string (GitHub's convention as of late 2024). Returns
/// `Err` on mismatch, unsupported algorithm, or malformed digest.
/// Pure function — no I/O — so the test suite can drive it without
/// network or filesystem.
pub(crate) fn verify_sha256(bytes: &[u8], digest: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    // Today only sha256 is in scope. Reject anything else explicitly
    // so a future GitHub change to e.g. `"sha512:..."` doesn't
    // silently disable verification — we'd rather fail loud and
    // ship a fix.
    let Some(expected_hex) = digest.strip_prefix("sha256:") else {
        bail!("unsupported digest algorithm in {digest:?}; expected sha256:<hex>");
    };
    if expected_hex.len() != 64 {
        bail!(
            "malformed sha256 digest length: got {} hex chars, want 64",
            expected_hex.len()
        );
    }
    let mut h = Sha256::new();
    h.update(bytes);
    let computed_hex = hex::encode(h.finalize());
    if !computed_hex.eq_ignore_ascii_case(expected_hex) {
        bail!("sha256 mismatch: computed {computed_hex}, expected {expected_hex}",);
    }
    Ok(())
}

/// Fetch a specific release by tag from GitHub. Bypasses the
/// roomler-ai proxy because pinning is rare (per-agent crash-loop
/// recovery, not a fleet-wide poll), so the proxy's per-IP rate-
/// limit insulation isn't needed and the round-trip via our backend
/// would just add latency to a path that's already on the slow side
/// of the agent's failure recovery.
pub(crate) async fn fetch_release_by_tag(tag: &str) -> Result<GithubRelease> {
    let url = format!("https://api.github.com/repos/{RELEASES_REPO}/releases/tags/{tag}");
    let client = reqwest::Client::builder()
        .user_agent(concat!("roomlerd/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("building reqwest client")?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("GET release by tag")?;
    if !resp.status().is_success() {
        bail!("GitHub returned {} for tag {tag}", resp.status());
    }
    let release: GithubRelease = resp.json().await.context("parsing release JSON")?;
    Ok(release)
}

/// Pin to a specific release tag. Used by the rollback path when
/// the crash-loop detector decides the current version is broken
/// and the last known-good version should be reinstalled.
///
/// Returns `CheckOutcome::UpdateReady` with an installer path on
/// success — caller spawns the installer. Returns `Skipped` on any
/// fetch / asset-pick / download failure so the agent can keep
/// running (broken rollback is better than a hard exit because
/// "the rollback recovery itself failed").
///
/// Network errors fold into `Skipped` like the rest of the
/// updater paths so a flaky link can't crash the agent.
pub async fn pin_version(tag: &str) -> CheckOutcome {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let release = match fetch_release_by_tag(tag).await {
        Ok(r) => r,
        Err(e) => return CheckOutcome::Skipped(format!("pin fetch {tag}: {e:#}")),
    };
    let asset = match pick_asset_for_platform(&release.assets) {
        Some(a) => a,
        None => {
            return CheckOutcome::Skipped(format!("no platform installer in release {tag}"));
        }
    };
    // Bound before the match so the borrow for the version check ends before
    // the arm moves the tag into the outcome.
    let downloaded = download_asset(asset, &release.tag_name).await;
    match downloaded {
        Ok(path) => CheckOutcome::UpdateReady {
            current,
            latest: release.tag_name,
            installer_path: path,
        },
        Err(e) => CheckOutcome::Skipped(format!("pin download {tag}: {e:#}")),
    }
}

/// Run one check cycle: GET releases → compare → download if needed.
/// Returns the outcome so the caller can log + decide whether to
/// spawn the installer. Never panics; network errors fold into
/// `Skipped(...)` so a flaky link doesn't crash the agent.
pub async fn check_once() -> CheckOutcome {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let release = match fetch_latest_release().await {
        Ok(r) => r,
        Err(e) => return CheckOutcome::Skipped(format!("fetch: {e:#}")),
    };
    // Drafts are always skipped; prereleases are tolerated because
    // our 0.x release history marked them all `prerelease: true` and
    // we want those agents to update even though GitHub's own
    // /releases/latest endpoint excludes them. pick_latest_release
    // has already filtered by tag prefix.
    if release.draft {
        return CheckOutcome::Skipped(format!("latest release is draft: {}", release.tag_name));
    }
    let latest_parsed = match parse_version(&release.tag_name) {
        Some(_) => release.tag_name.clone(),
        None => return CheckOutcome::Skipped(format!("unparseable tag {}", release.tag_name)),
    };
    if !is_newer(&latest_parsed, &current) {
        return CheckOutcome::UpToDate {
            current,
            latest: latest_parsed,
        };
    }
    let asset = match pick_asset_for_platform(&release.assets) {
        Some(a) => a,
        None => {
            return CheckOutcome::Skipped(format!(
                "no installer asset for this platform in release {latest_parsed}"
            ));
        }
    };
    let downloaded = download_asset(asset, &latest_parsed).await;
    match downloaded {
        Ok(path) => CheckOutcome::UpdateReady {
            current,
            latest: latest_parsed,
            installer_path: path,
        },
        Err(e) => CheckOutcome::Skipped(format!("download: {e:#}")),
    }
}

/// Spawn the installer detached. Returns after the installer is
/// running so the caller can `std::process::exit(0)` — the agent's
/// binary is about to be overwritten.
///
/// - **Windows**: `msiexec /i <path> /qn /norestart`. Requires
///   per-user MSI (no UAC) — which is what cargo-wix emits by
///   default for our install mode.
/// - **Linux**: `pkexec apt-get install -y <path>`. Requires policykit
///   plus sudo-equivalent; a non-interactive fallback uses
///   `dpkg --install` directly (works when run as the user who
///   owns /usr/bin, e.g. in a cargo-installed dev env).
/// - **macOS**: `installer -pkg <path> -target /` — the same target
///   `scripts/install.sh` uses. It must be the volume root: the payload
///   paths are absolute and the launchd plists name them literally.
pub fn spawn_installer(installer_path: &std::path::Path) -> Result<()> {
    spawn_installer_with_watch(installer_path, None)
}

/// Spawn the installer for `installer_path` AND, when an
/// `expected_version` tag is provided, spawn a sibling
/// `roomlerd post-install-watch` process that captures the
/// installer's exit code + verifies the new binary's `--version`.
///
/// The watcher must be spawned *before* this function returns so the
/// installer's PID is still in the process table; once the parent
/// agent exits the installer is reparented to init/explorer and the
/// watcher polls it from there.
///
/// `expected_version=None` keeps the legacy "fire and forget" path —
/// useful for tests and the manual `self-update` CLI where the
/// outcome JSON adds nothing the operator can't see directly.
pub fn spawn_installer_with_watch(
    installer_path: &std::path::Path,
    expected_version: Option<&str>,
) -> Result<()> {
    // Touch the cooldown marker BEFORE spawning the installer. The
    // run_periodic loop in any newly-spawned sibling worker (typical
    // under SCM supervision) reads this marker on its first iteration
    // to skip the immediate update check. Without this, the worker
    // detects the same pending update, spawns another installer, and
    // we get an install-storm. Field repro: operator 2026-05-02.
    record_update_attempt();
    let spawned = spawn_installer_inner(installer_path)?;
    if let Some(tag) = expected_version
        && let Err(e) = spawn_watcher(spawned, installer_path, tag)
    {
        // Don't fail the whole self-update flow on a watcher spawn
        // failure — the installer is already running and the agent
        // is about to exit; we lose the outcome JSON but the user
        // still gets the upgrade.
        // `{:#}` not `%e`: the anyhow *chain*. The bare display shows only
        // "spawning post-install-watch subprocess" and hides the ENOENT that
        // actually explains it — which is why 21 consecutive failures read as
        // one uninformative line.
        tracing::warn!(
            error = format!("{e:#}"),
            "post-install watcher spawn failed"
        );
    }
    Ok(())
}

/// Build the msiexec argv for this MSI flavour. Pure so it's unit-
/// testable without spawning processes.
///
/// - **perUser**: `/qn` (fully silent — no UI, no UAC). The MSI's
///   `InstallScope='perUser'` doesn't require elevation, so silent
///   install works from any user-token process (Scheduled Task,
///   interactive shell).
/// - **perMachine**: ALSO `/qn` since rc.236. History: rc.18 chose
///   `/qb!` because a perMachine MSI spawned non-elevated with `/qn`
///   silently fails (2026-05-10 field repro — msiexec can't raise
///   UAC in silent mode). That rationale is obsolete: every
///   perMachine spawn now goes through `spawn_msiexec_elevated`
///   (`ShellExecuteExW` verb=runas), so elevation happens BEFORE
///   msiexec runs and `/qn` has no UAC left to suppress. And `/qb!`
///   turned out to be the root cause of the 5/5-reproduced
///   self-update wedge (rc.226→rc.235 on the dev host): upgrading
///   over the RUNNING service trips FilesInUse (MSI error 1607,
///   confirmed via /l*v — the server loops on `SELECT Message FROM
///   Error WHERE Error = 1607`), and basic UI can neither display
///   nor auto-answer that dialog → msiexec sits at "Gathering
///   required information…" forever. `/qn` lets Windows Installer
///   auto-resolve FilesInUse via RestartManager — the exact manual
///   recovery that worked 4/4.
///
/// Both flavours append `/l*v <installer>.log` — a verbose MSI log
/// next to the staged installer. Three field wedges (rc.226, rc.228,
/// rc.232 self-updates on the dev host: msiexec alive but never
/// exiting, service left stopped) were undiagnosable because silent
/// installs leave no trace; the log names the exact action a wedged
/// run was in. The staging dir is recycled per update, so the log
/// doesn't accumulate.
#[cfg(any(target_os = "windows", test))]
pub fn msiexec_argv(installer: &std::path::Path, flavour: WindowsInstallFlavour) -> Vec<String> {
    let path = installer.to_string_lossy().into_owned();
    let ui = match flavour {
        WindowsInstallFlavour::PerUser => "/qn",
        WindowsInstallFlavour::PerMachine => "/qn",
    };
    let log = format!("{path}.log");
    vec![
        "/i".to_string(),
        path,
        ui.to_string(),
        "/norestart".to_string(),
        "/l*v".to_string(),
        log,
    ]
}

/// rc.44: variant of [`msiexec_argv`] that appends operator-supplied
/// MSI public properties (`KEY=VALUE`) after the base argv. Used by
/// the install wizard to flip `ENABLE_SYSTEM_CONTEXT=1` when the
/// operator picks the `permachine-system-context` flavour.
///
/// Properties are appended verbatim — no shell-quoting, no escaping.
/// Callers must ensure `VALUE` contains no whitespace; msiexec's
/// public-property grammar is space-separated `KEY=VALUE` tokens.
///
/// cfg-gated to match [`msiexec_argv`] (which it calls) — `msiexec_argv`
/// is `cfg(any(windows, test))`, so an ungated wrapper fails the
/// Linux/macOS release build with `E0425: cannot find function
/// msiexec_argv`. The `test` arm keeps the unit tests below buildable
/// on every CI runner.
#[cfg(any(target_os = "windows", test))]
pub fn msiexec_argv_with_properties(
    installer: &std::path::Path,
    flavour: WindowsInstallFlavour,
    properties: &[(&str, &str)],
) -> Vec<String> {
    let mut argv = msiexec_argv(installer, flavour);
    for (k, v) in properties {
        argv.push(format!("{k}={v}"));
    }
    argv
}

/// What [`spawn_installer_inner`] observed: the installer's pid, and whether it
/// had ALREADY finished by the time we returned.
///
/// ⚠️ The flag is reported by the dispatch itself rather than re-derived from the
/// artifact name, so the two cannot drift apart — the same coupling rule as
/// `install_path_before_rename` and the installer's own `.prev` rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallerSpawn {
    pub pid: u32,
    /// True when the install completed synchronously inside this call, so the
    /// pid names either this process or a corpse. Waiting on it is never
    /// correct — see `post_install::should_wait_for_installer`.
    pub already_exited: bool,
}

pub fn spawn_installer_inner(installer_path: &std::path::Path) -> Result<InstallerSpawn> {
    // Auto-updater entry point: classify the RUNNING agent EXE's
    // location to decide whether the in-place MSI swap needs UAC
    // elevation. Correct for self-update because the running EXE
    // IS the install whose flavour we're matching.
    //
    // **DO NOT** call this from the install wizard. The wizard's EXE
    // runs from wherever the operator double-clicked (`%TEMP%`,
    // Downloads, Desktop) so [`current_install_flavour`] returns
    // `PerUser` regardless of the host install state OR the operator-
    // selected flavour, and a perMachine MSI launched non-elevated
    // exits with `1625 ERROR_INSTALL_PACKAGE_REJECTED`. Wizard callers
    // must use [`spawn_installer_for_flavour`] with the SPA-selected
    // [`WindowsInstallFlavour`]. Field repro 2026-05-15 on a Windows
    // field-test host; see BLOCKER B6 in the rc.27/rc.28 master plan.
    #[cfg(target_os = "windows")]
    {
        // msiexec runs asynchronously; the watcher must observe its exit and
        // `recover_wedged_install` depends on that wait.
        spawn_installer_as_flavour(installer_path, current_install_flavour()).map(|pid| {
            InstallerSpawn {
                pid,
                already_exited: false,
            }
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        spawn_installer_for_flavour_inner(installer_path).map(|pid| InstallerSpawn {
            pid,
            // ⚠️ Linux only. BOTH arms of the Linux dispatch complete before
            // returning — the tarball path installs inline and hands back
            // `std::process::id()`, the `.deb` path `wait()`s on its child.
            // macOS spawns `installer -pkg` and genuinely must be waited on.
            already_exited: cfg!(target_os = "linux"),
        })
    }
}

// RETIRED-NAME-ANCHOR: the pre-rename machine-global path this cleanup exists to
// find. docs/fr/FR-21
/// Windows auto-update spawn for a caller that already KNOWS the
/// flavour of the install being replaced. [`spawn_installer_inner`]
/// delegates here with the running EXE's classification; the
/// post-install watcher's wedge retry passes the `--origin-exe`
/// flavour instead, because the watcher runs from a staged copy in
/// the %TEMP% staging dir and its own path would misclassify as
/// PerUser (the same trap as the wizard, documented above).
///
/// rc.56 SystemContext-preserve hotfix lives here so BOTH callers
/// get it.
///
/// Without this branch, the WiX `DisableSystemContext` deferred
/// CA fires on every auto-update msiexec because the WiX
/// `ENABLE_SYSTEM_CONTEXT` property defaults to `'0'` and the
/// CA is conditioned on `ENABLE_SYSTEM_CONTEXT="0" AND NOT
/// (REMOVE="ALL")` (see `wix-perMachine/main.wxs:344-346`).
/// The CA runs `roomlerd disable-system-context` which
/// strips `ROOMLERD_ENABLE_SYSTEM_SWAP` from the SCM
/// Environment block and restarts the service. After auto-update
/// the supervisor sees env-var-off, doesn't perform the M3 A1
/// winlogon-token swap, and the resulting LocalSystem worker
/// tries to read its config from `%APPDATA%` (LocalSystem
/// profile, empty) instead of the rc.52 machine-global
/// `%PROGRAMDATA%\roomler\roomler-agent\config.toml` path. Net
/// effect: every SystemContext-enabled host that auto-updates
/// loses pre-logon capability and crash-loops until an operator
/// re-runs the wizard. Field-reproduced 2026-05-24 on WINHOST-A
/// (rc.55 → reinstall).
///
/// Fix: detect the env var BEFORE invoking msiexec and pass
/// `ENABLE_SYSTEM_CONTEXT=1` so the WiX `EnableSystemContext`
/// CA fires instead. The detection reads the SCM Environment
/// REG_MULTI_SZ via the rc.27 helper — robust whether the
/// SystemContext was enabled via the wizard, the
/// `enable-system-context` CLI subcommand, or a prior MSI run.
#[cfg(target_os = "windows")]
pub fn spawn_installer_as_flavour(
    installer_path: &std::path::Path,
    flavour: WindowsInstallFlavour,
) -> Result<u32> {
    let properties = preserve_system_context_property(flavour);
    let props_ref: Vec<(&str, &str)> = properties
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    spawn_installer_for_flavour_with_properties(installer_path, flavour, &props_ref)
}

/// rc.56: read the current SCM Environment block. If the perMachine
/// service has `ROOMLERD_ENABLE_SYSTEM_SWAP=1` set, return
/// `vec![("ENABLE_SYSTEM_CONTEXT", "1")]` so the auto-update msiexec
/// invocation re-asserts the env var via the WiX `EnableSystemContext`
/// CA instead of stripping it via `DisableSystemContext`.
///
/// Thin wrapper around [`preserve_system_context_property_for`] that
/// reads the SCM env block; the inner pure helper is unit-tested
/// against synthetic env-var values without an SCM dependency.
#[cfg(target_os = "windows")]
fn preserve_system_context_property(flavour: WindowsInstallFlavour) -> Vec<(String, String)> {
    let env_value =
        crate::win_service::environment::read_service_env_var("ROOMLERD_ENABLE_SYSTEM_SWAP")
            .map_err(|e| {
                tracing::warn!(
                    error = %e,
                    "updater: failed to read SCM env var ROOMLERD_ENABLE_SYSTEM_SWAP; \
                     not passing ENABLE_SYSTEM_CONTEXT to msiexec (auto-update will proceed \
                     but may strip SystemContext if it was enabled)"
                );
                format!("{e}")
            });
    let result = preserve_system_context_property_for(flavour, env_value);
    if !result.is_empty() {
        tracing::info!(
            "updater: detected ROOMLERD_ENABLE_SYSTEM_SWAP=1 in service env; \
             passing ENABLE_SYSTEM_CONTEXT=1 to msiexec to preserve SystemContext mode \
             across auto-update (rc.56 hotfix)"
        );
    }
    result
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
fn preserve_system_context_property(_flavour: WindowsInstallFlavour) -> Vec<(String, String)> {
    Vec::new()
}

/// rc.56 pure decision logic. Split from the SCM read so the rules
/// are unit-testable without mocking the registry.
///
/// Returns `vec![("ENABLE_SYSTEM_CONTEXT", "1")]` iff:
///   * flavour is PerMachine (perUser installs have no SCM service)
///   * env_value == Ok(Some("1")) — the SCM block has the swap on
///
/// All other paths return an empty vec (pass nothing to msiexec):
///   * PerUser flavour
///   * env var absent (plain perMachine install — DisableSystemContext
///     is a harmless idempotent no-op)
///   * env var present but != "1" (operator may have set a non-"1"
///     truthy variant the agent's `system_swap_enabled` accepts, but
///     the WiX CA condition compares strictly against the string
///     `"1"`; we follow WiX's strictness here)
///   * SCM read error (best-effort; falling back matches pre-rc.56
///     behaviour — `enable-system-context` CLI remains the manual
///     escape hatch)
//
// cfg-gate so the function is only compiled where it's actually
// reachable: on Windows (the SCM wrapper at line 869 calls it) and
// under `cfg(test)` (the unit tests at lines 1959+ exercise it
// directly without going through the Windows wrapper). Without the
// gate, the Linux clippy job fails with `-D dead_code` because the
// non-Windows `preserve_system_context_property` stub returns early
// and never reaches this helper. Pre-existing master regression
// repro'd in CI run 26375071253; fix landed alongside the
// tunnel-wizard feature so the branch's CI can be green.
#[cfg(any(target_os = "windows", test))]
fn preserve_system_context_property_for(
    flavour: WindowsInstallFlavour,
    env_value: Result<Option<String>, String>,
) -> Vec<(String, String)> {
    if flavour != WindowsInstallFlavour::PerMachine {
        return Vec::new();
    }
    match env_value {
        Ok(Some(v)) if v == "1" => vec![("ENABLE_SYSTEM_CONTEXT".to_string(), "1".to_string())],
        _ => Vec::new(),
    }
}

/// Launch the platform installer for a downloaded artefact, using the
/// CALLER-SUPPLIED flavour to choose the elevation path on Windows.
/// Non-Windows ignores the flavour (Linux + macOS don't have perUser /
/// perMachine MSI variants).
///
/// Use this entry point from the install wizard, which already knows
/// the operator-selected flavour from the SPA's radio cards. The
/// auto-updater path stays on [`spawn_installer_inner`] which infers
/// the flavour from the running EXE — correct for self-update because
/// the running EXE IS the install being replaced.
pub fn spawn_installer_for_flavour(
    installer_path: &std::path::Path,
    #[cfg(target_os = "windows")] flavour: WindowsInstallFlavour,
    #[cfg(not(target_os = "windows"))] _flavour: WindowsInstallFlavour,
) -> Result<u32> {
    #[cfg(target_os = "windows")]
    {
        spawn_installer_for_flavour_with_properties(installer_path, flavour, &[])
    }
    #[cfg(not(target_os = "windows"))]
    {
        spawn_installer_for_flavour_inner(installer_path)
    }
}

/// rc.44: variant of [`spawn_installer_for_flavour`] that passes
/// MSI public properties (e.g. `ENABLE_SYSTEM_CONTEXT=1`) to msiexec.
/// Used by the install wizard to drive the WiX
/// `EnableSystemContext` / `DisableSystemContext` deferred custom
/// actions added in rc.44 P2.
///
/// Non-Windows ignores both the flavour and the properties — there's
/// no msiexec on Linux/macOS, so the properties are not surfaceable
/// to the platform installer anyway.
pub fn spawn_installer_for_flavour_with_properties(
    installer_path: &std::path::Path,
    #[cfg(target_os = "windows")] flavour: WindowsInstallFlavour,
    #[cfg(not(target_os = "windows"))] _flavour: WindowsInstallFlavour,
    #[cfg(target_os = "windows")] properties: &[(&str, &str)],
    #[cfg(not(target_os = "windows"))] _properties: &[(&str, &str)],
) -> Result<u32> {
    #[cfg(target_os = "windows")]
    {
        let argv = msiexec_argv_with_properties(installer_path, flavour, properties);
        match flavour {
            WindowsInstallFlavour::PerUser => {
                // /qn silent install; non-elevated msiexec inherits
                // the user token and writes under %LOCALAPPDATA%.
                let child = std::process::Command::new("msiexec")
                    .args(argv.iter().map(String::as_str).collect::<Vec<_>>())
                    .spawn()
                    .context("spawning msiexec (perUser)")?;
                Ok(child.id())
            }
            WindowsInstallFlavour::PerMachine => {
                // Elevate via ShellExecuteExW + verb="runas" so UAC
                // prompts (when caller is non-elevated) AND so msiexec
                // gets the admin token it needs to write under
                // %ProgramFiles%. Field bug 2026-05-10 the field-test host: plain
                // `Command::new("msiexec").args(["/i", path, "/qn"])`
                // silently fails because /qn suppresses the UAC
                // prompt on a perMachine manifest.
                spawn_msiexec_elevated(&argv)
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        spawn_installer_for_flavour_inner(installer_path)
    }
}

/// The ordered `.deb` install commands to try, for a given effective uid.
///
/// Pure so the ordering is unit-testable without spawning anything (same
/// shape as [`msiexec_argv`] on the Windows side).
///
/// * **root** — install directly. No polkit, no sudo, nothing that can
///   prompt. `apt-get` first because it resolves the package's `depends`;
///   `dpkg` as the offline fallback when apt's lists are unusable.
/// * **non-root** — escalate. `pkexec` needs a polkit *authentication
///   agent*, which does NOT exist in a systemd service / headless / WSL
///   context; `sudo -n` needs a passwordless rule. Both routinely fail,
///   which is exactly why the caller must check the exit status.
///
/// The historical bug this replaces: the old code did `pkexec …spawn()`
/// and treated a successful *spawn* as a successful *install*, falling
/// back to `dpkg` only when the pkexec BINARY was missing. On any host
/// with pkexec installed but no polkit agent (every headless box), the
/// spawn succeeded, pkexec died immediately, nothing was installed, and
/// the agent exited anyway — a silent no-op that left field hosts pinned
/// to an old build across many update attempts.
#[cfg(any(target_os = "linux", test))]
fn linux_install_candidates(euid: u32, path: &str) -> Vec<(&'static str, Vec<String>)> {
    let apt = |prefix: Vec<&str>| {
        let mut a: Vec<String> = prefix.into_iter().map(String::from).collect();
        a.extend(["apt-get", "install", "-y", path].map(String::from));
        a
    };
    if euid == 0 {
        vec![
            ("apt-get", vec!["install".into(), "-y".into(), path.into()]),
            ("dpkg", vec!["--install".into(), path.into()]),
        ]
    } else {
        vec![("pkexec", apt(vec![])), ("sudo", apt(vec!["-n"]))]
    }
}

/// Run [`linux_install_candidates`] in order and return the pid of the
/// first one that exits 0.
///
/// Deliberately WAITS on each child instead of firing and forgetting:
/// dpkg replacing a running `/usr/bin/roomlerd` is safe on Linux (it
/// unlinks and recreates) and takes seconds, and waiting is the only way
/// to know the install actually happened. The returned pid belongs to an
/// already-exited process, which is what the post-install watcher expects
/// on Unix anyway: `wait_pid_unix` reads `ESRCH`, reports `Exited(0)`, and
/// `verify_new_binary` re-runs `--version` — now a truthful check, because
/// the install really did finish before we got here.
///
/// Every candidate failing is a hard `Err`, so `run_periodic`'s existing
/// "installer spawn failed; will retry next cycle" branch keeps the
/// daemon ALIVE rather than exiting into a pointless restart.
/// The payload members a tarball MUST carry, relative to its prefix dir.
/// Asserted before anything installed is touched — a truncated download that
/// cleared the size floor must not half-install a host.
#[cfg(target_os = "linux")]
const TARBALL_REQUIRED: &[&str] = &["usr/bin/roomlerd", "usr/bin/roomler"];

/// Units ship in the tarball but are only written when ABSENT. An upgrade
/// must never revert a unit the operator edited — the field host carries a
/// hand-written unit with `ROOMLERD_VIRTUAL_DESKTOP=1`, and silently
/// reverting that would be a data-loss-class bug. A package manager does not
/// clobber modified conffiles either.
#[cfg(target_os = "linux")]
const TARBALL_INSTALL_IF_ABSENT: &[&str] = &[
    "usr/lib/systemd/system/roomlerd.service",
    "usr/lib/systemd/user/roomler.service",
    // RETIRED-NAME-ANCHOR: a path INSIDE the shipped .deb; moving it needs the
    // package layout to move with it. docs/fr/FR-21
    "usr/share/doc/roomler-agent/README.Debian",
];

/// Install the universal tarball — the path for hosts with no dpkg/apt.
///
/// Extract to a staging dir, verify the payload, then move each file into
/// place. Replacing a RUNNING executable is safe on Linux when done by
/// rename-over: the running process keeps its inode and only new execs see
/// the new file. That is what dpkg does, and why the .deb path has always
/// been able to replace a live `/usr/bin/roomlerd`.
///
/// Returns a pid to match `run_linux_install_candidates`' contract; the work
/// is synchronous, so it reports our own.
#[cfg(target_os = "linux")]
fn install_tarball_linux(euid: u32, tarball: &std::path::Path) -> Result<u32> {
    use std::path::Path;

    if euid != 0 {
        // Unlike the .deb path there is no pkexec/sudo helper to delegate to:
        // extracting into /usr needs the privilege directly. Say so plainly
        // rather than failing deep inside a copy.
        bail!(
            "the tarball install needs root (euid {euid}); run the agent as a \
             system service, or install {} manually",
            tarball.display()
        );
    }

    let stage = tarball.with_extension("stage");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).context("creating the staging dir")?;

    let st = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(&stage)
        .status()
        .context("spawning tar (is it installed?)")?;
    if !st.success() {
        bail!(
            "tar exited {:?} extracting {}",
            st.code(),
            tarball.display()
        );
    }

    // The archive carries a single versioned prefix dir.
    let prefix = std::fs::read_dir(&stage)
        .context("reading the staging dir")?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .with_context(|| format!("{} contained no payload dir", tarball.display()))?;

    for rel in TARBALL_REQUIRED {
        let src = prefix.join(rel);
        if !src.is_file() {
            bail!(
                "tarball is missing {rel} — refusing to install a partial payload \
                 from {}",
                tarball.display()
            );
        }
    }

    let place = |rel: &str, only_if_absent: bool| -> Result<bool> {
        let src = prefix.join(rel);
        if !src.is_file() {
            return Ok(false);
        }
        let dst = Path::new("/").join(rel);
        if only_if_absent && dst.exists() {
            tracing::info!(path = %dst.display(), "update: keeping the existing file");
            return Ok(false);
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        // Stage beside the target so the rename is same-filesystem (atomic),
        // then swap. Copy-then-rename, never write-in-place: a partial write
        // over a live binary would brick the host.
        let tmp = dst.with_extension("new");
        std::fs::copy(&src, &tmp).with_context(|| format!("copying {rel}"))?;
        let mode = std::fs::metadata(&src)?.permissions();
        std::fs::set_permissions(&tmp, mode).with_context(|| format!("chmod {rel}"))?;
        // Keep the outgoing binary for the rollback path.
        if dst.exists() && rel.starts_with("usr/bin/") {
            let _ = std::fs::rename(&dst, dst.with_extension("prev"));
        }
        std::fs::rename(&tmp, &dst).with_context(|| format!("installing {rel}"))?;
        Ok(true)
    };

    let mut installed = 0usize;
    for rel in TARBALL_REQUIRED {
        if place(rel, false)? {
            installed += 1;
        }
    }
    // Bundled libs are discovered, not enumerated: the set differs per arch
    // (FFmpeg + libvpx on x86_64, libvpx alone on aarch64) and shrinks as
    // linkage is trimmed.
    // RETIRED-NAME-ANCHOR: RPATH-bound — the binary resolves its bundled FFmpeg from
    // this directory, so the name cannot move without relinking. docs/fr/FR-21
    let libdir = prefix.join("usr/lib/roomler-agent");
    if libdir.is_dir() {
        for entry in std::fs::read_dir(&libdir).context("reading the bundled libs")? {
            let entry = entry?;
            if entry.path().is_file() {
                let rel = format!(
                    // RETIRED-NAME-ANCHOR(2): the bundled-library directory is
                    // baked into the binary as an RPATH. It cannot move without
                    // a relink, so the updater must stage into the same name.
                    "usr/lib/roomler-agent/{}",
                    entry.file_name().to_string_lossy()
                );
                if place(&rel, false)? {
                    installed += 1;
                }
            }
        }
    }
    for rel in TARBALL_INSTALL_IF_ABSENT {
        if place(rel, true)? {
            installed += 1;
        }
    }

    let _ = std::fs::remove_dir_all(&stage);
    tracing::info!(
        installed,
        tarball = %tarball.display(),
        "update install: tarball installed"
    );
    Ok(std::process::id())
}

#[cfg(target_os = "linux")]
fn run_linux_install_candidates(euid: u32, path: &str) -> Result<u32> {
    let mut attempts: Vec<String> = Vec::new();
    for (bin, args) in linux_install_candidates(euid, path) {
        let mut child = match std::process::Command::new(bin).args(&args).spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(bin, error = %e, "update install: candidate not spawnable");
                attempts.push(format!("{bin}: not spawnable ({e})"));
                continue;
            }
        };
        let pid = child.id();
        match child.wait() {
            Ok(st) if st.success() => {
                tracing::info!(bin, euid, "update install: package installed");
                return Ok(pid);
            }
            Ok(st) => {
                // pkexec exits 127 when it cannot find an authentication
                // agent — the headless case that used to pass silently.
                tracing::warn!(bin, code = st.code(), "update install: candidate failed");
                attempts.push(format!("{bin}: exit {:?}", st.code()));
            }
            Err(e) => {
                tracing::warn!(bin, error = %e, "update install: wait failed");
                attempts.push(format!("{bin}: wait failed ({e})"));
            }
        }
    }
    bail!(
        "no usable way to install the .deb as uid {euid} (tried {}). \
         Run the daemon as root via the packaged `roomlerd.service` system \
         unit, or install manually: sudo dpkg -i {path}",
        attempts.join("; ")
    )
}

/// Linux + macOS spawn body shared between
/// [`spawn_installer_inner`] and [`spawn_installer_for_flavour`].
/// Flavour is irrelevant outside Windows.
#[cfg(not(target_os = "windows"))]
fn spawn_installer_for_flavour_inner(installer_path: &std::path::Path) -> Result<u32> {
    #[cfg(target_os = "linux")]
    {
        let path_str = installer_path.to_string_lossy().into_owned();
        // SAFETY: `geteuid` is always-succeeding and takes no arguments.
        let euid = unsafe { libc::geteuid() };
        if path_str.ends_with(".tar.gz") {
            install_tarball_linux(euid, installer_path)
        } else {
            run_linux_install_candidates(euid, &path_str)
        }
    }
    #[cfg(target_os = "macos")]
    {
        let path_str = installer_path.to_string_lossy().into_owned();

        // `installer -target /` writes /Library and /usr/local, so it REFUSES
        // to run as anyone but root — "Must be run as root to install this
        // package" — and exits immediately.
        //
        // Checking first is not defensive politeness. `Command::spawn`
        // SUCCEEDS here regardless, because the process really does start; it
        // is the installer that then fails. So without this check the agent
        // reads a guaranteed failure as a successful handoff and exits 0 —
        // and the LaunchAgent's `KeepAlive{SuccessfulExit=false}` deliberately
        // does NOT restart a clean exit. The two behaviours are individually
        // right and together they take the device off the network until a
        // human intervenes.
        //
        // Field-hit 2026-08-25: a per-user agent attempting rc.467 -> rc.468
        // put a MacBook offline for ~25 minutes. `/var/log/install.log` had no
        // entry at all, because the installer never got far enough to log one
        // — which is also why it looked like a mysterious "failed handoff"
        // rather than a permission error.
        //
        // The per-user half legitimately CANNOT self-update on macOS: the pkg
        // is a system install. Returning Err routes into the caller's existing
        // failure path, which logs, raises the operator sentinel, and keeps
        // running on the current version — the correct outcome for an agent
        // that cannot update. On a two-half install the ROOT daemon updates
        // the shared bundle, so the host still moves forward.
        let euid = unsafe { libc::geteuid() };
        // RETIRED-NAME-ANCHOR(21): the pre-P4b /Applications/roomler-agent.app path
        // this sweep still has to name, plus the .app bundle itself, which keys the
        // TCC grants and stays frozen until P5b. The macOS LOG path is no longer
        // among them — FR-46 P5a moved it to /var/log/roomler. docs/fr/FR-46
        if euid != 0 {
            bail!(
                "refusing to self-update: `installer -target /` requires root, but this \
                 agent runs as uid {euid} (the per-user LaunchAgent). Exiting here would \
                 take the agent offline without installing anything. Updates on macOS are \
                 owned by the root update helper (com.roomler.update) — wake it with \
                 `touch {MACOS_UPDATE_TRIGGER}` and watch /var/log/roomler/update.log, \
                 or install by hand: `sudo installer -pkg <pkg> -target /`."
            );
        }

        // `-target /`, NOT `CurrentUserHomeDirectory`.
        //
        // RETIRED-NAME-ANCHOR(3): the PRE-P4b /Applications bundle, swept on upgrade.
        // Without the old name the vacated .app is orphaned forever. docs/fr/FR-21
        // The pkg's payload is absolute: /Applications/roomler-agent.app plus
        // /usr/local/bin/roomler, and BOTH launchd plists name the bundle path
        // literally. A home-directory install relocates the payload under
        // ~/Applications, so the update "succeeds" and launchd keeps starting
        // the OLD binary at the path it still points to — an update that
        // silently does nothing, on the one platform whose grants are keyed to
        // the binary. `/` is also what scripts/install.sh uses, so the update
        // path and the install path finally agree.
        let child = std::process::Command::new("installer")
            .args(["-pkg", &path_str, "-target", "/"])
            .spawn()
            .context("spawning installer(8)")?;
        Ok(child.id())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        bail!(
            "self-update spawn is not implemented on this platform ({:?})",
            installer_path
        )
    }
}

/// Launch msiexec with UAC elevation via `ShellExecuteExW` + verb=
/// "runas". This is the perMachine path: the MSI's perMachine manifest
/// requires admin rights, the Scheduled-Task / interactive-shell caller
/// holds only a Limited token, so the OS must prompt for consent
/// before msiexec gets a privileged token.
///
/// Returns the spawned msiexec's PID for the post-install watcher.
/// Returns `Err` when UAC was declined OR no interactive session is
/// present to display the dialog — the caller surfaces this to the
/// operator (CLI: stderr message; service mode: log + retry next
/// cycle).
#[cfg(target_os = "windows")]
pub fn spawn_msiexec_elevated(argv: &[String]) -> Result<u32> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::GetProcessId;
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };

    // Build wide-string buffers for ShellExecuteExW. The Win32 API
    // wants null-terminated UTF-16; OsStr::encode_wide returns the
    // codepoints sans terminator so we append a 0.
    fn to_wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    // msiexec expects the args concatenated as a single string with
    // spaces between. Paths are quoted to survive any embedded spaces
    // in the temp-dir / installer name. Other args are bare.
    let parameters = argv
        .iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let verb_w = to_wide("runas");
    let file_w = to_wide("msiexec.exe");
    let params_w = to_wide(&parameters);

    // SAFETY: SHELLEXECUTEINFOW is a plain POD struct; zero-init is
    // valid. cbSize must match the struct size. All pointer fields
    // outlive the call (locals on the stack live until after
    // ShellExecuteExW returns). hProcess is initialised to NULL and
    // populated by the API on success because SEE_MASK_NOCLOSEPROCESS
    // is set.
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    sei.lpVerb = verb_w.as_ptr();
    sei.lpFile = file_w.as_ptr();
    sei.lpParameters = params_w.as_ptr();
    sei.nShow = 1; // SW_SHOWNORMAL — harmless under /qn (no UI to draw)

    // SAFETY: all required fields populated above; passing a valid
    // pointer to a stack-allocated struct.
    let ok = unsafe { ShellExecuteExW(&mut sei) };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        // ERROR_CANCELLED = 1223 is the UAC-declined code. Other
        // errors (file not found, etc.) surface as "elevation
        // failed (err N)" — operator can paste into the issue.
        if err == 1223 {
            bail!("UAC consent declined — install not started");
        }
        bail!("ShellExecuteExW(runas) failed (err {err})");
    }
    if sei.hProcess.is_null() {
        // The API returned success but didn't give us a process
        // handle. Documented behaviour for cases like: no interactive
        // session, the verb didn't actually launch anything, the
        // launched program is already running and DDE-merged into
        // an existing instance. None of those are recoverable here.
        bail!("ShellExecuteExW returned success but no process handle (no interactive session?)");
    }
    // SAFETY: hProcess is a valid HANDLE from a successful
    // SEE_MASK_NOCLOSEPROCESS call. We MUST CloseHandle after
    // GetProcessId; the OS would otherwise leak the kernel object
    // for the elevated process until the agent itself exits.
    let pid = unsafe { GetProcessId(sei.hProcess) };
    unsafe { CloseHandle(sei.hProcess) };
    if pid == 0 {
        bail!("GetProcessId on elevated msiexec returned 0");
    }
    Ok(pid)
}

/// The real install path, when `exe` is the outgoing binary the installer
/// has just renamed aside.
///
/// The Unix tarball installer keeps the previous image for rollback with
/// `rename(dst, dst.with_extension("prev"))`, and only *then* is the watcher
/// spawned — so `current_exe()` resolves through `/proc/self/exe` to
/// `<name>.prev`, the OLD binary. A watcher that verifies its own path
/// therefore reads the pre-update version and reports a mismatch for a
/// perfectly good install.
///
/// Stripping the suffix is the exact inverse of the rename above, so the two
/// cannot drift apart without this function's test failing.
///
/// Returns `None` when `exe` is not a `.prev` path — there is then nothing to
/// correct and the caller keeps its existing behaviour.
#[cfg(not(target_os = "windows"))]
fn install_path_before_rename(exe: &std::path::Path) -> Option<PathBuf> {
    if exe.extension().and_then(|e| e.to_str()) != Some("prev") {
        return None;
    }
    Some(exe.with_extension(""))
}

/// Recover the real path when `/proc/self/exe` has picked up Linux's
/// `" (deleted)"` marker.
///
/// ⚠️ This is why the `.deb` path has never had a post-install watcher. `apt`
/// replaces `/usr/bin/roomlerd`, which unlinks the running image, so from that
/// moment `current_exe()` reads `/usr/bin/roomlerd (deleted)` — a path that does
/// not exist. `Command::spawn` on it fails ENOENT and the watcher is never born.
/// Field-measured 2026-09-06: **21 `post-install watcher spawn failed` in 7 days
/// and zero `post-install watcher started`**, with `last-install.json` frozen
/// since August on every host on that path.
///
/// The marker is appended by the kernel to the readlink result, so stripping it
/// is the exact inverse and the two cannot drift.
///
/// Returns `None` when there is no marker to strip — the caller then keeps the
/// path it already had.
#[cfg(not(target_os = "windows"))]
fn strip_deleted_suffix(exe: &std::path::Path) -> Option<PathBuf> {
    const MARKER: &str = " (deleted)";
    let s = exe.to_str()?;
    let stripped = s.strip_suffix(MARKER)?;
    if stripped.is_empty() {
        return None;
    }
    Some(PathBuf::from(stripped))
}

fn spawn_watcher(
    spawned: InstallerSpawn,
    installer_path: &std::path::Path,
    expected_version: &str,
) -> Result<()> {
    let installer_pid = spawned.pid;
    let exe = std::env::current_exe().context("locating own exe for watcher spawn")?;
    // Windows: launch the watcher from a staged COPY so it doesn't
    // hold the install-dir image — RestartManager shuts down every
    // manageable process holding a file the MSI replaces, and a
    // watcher killed at install start defends nothing (field
    // forensic 2026-08-21: last-install.json frozen at InProgress,
    // watcher born 02:54:11, RM app-shutdown 02:54:12). Unix package
    // managers replace files without stopping readers, so the
    // in-place spawn stays correct there.
    #[cfg(target_os = "windows")]
    let (launch, staged) = stage_watcher_exe(&exe, installer_path);
    #[cfg(not(target_os = "windows"))]
    let (launch, staged) = (exe.clone(), false);
    // The image we are about to re-exec may have been replaced by the install
    // that just ran. Recover the real path, and refuse rather than hand
    // `Command` something that is not there — an ENOENT here is silent apart
    // from one context-less WARN, which is how this went unnoticed for months.
    #[cfg(not(target_os = "windows"))]
    let launch = match strip_deleted_suffix(&launch) {
        Some(real) if real.is_file() => {
            tracing::info!(
                was = %launch.display(),
                now = %real.display(),
                "watcher exe was replaced by the install; using the new image"
            );
            real
        }
        _ => launch,
    };
    #[cfg(not(target_os = "windows"))]
    if !launch.is_file() {
        anyhow::bail!(
            "watcher exe {} does not exist (the install replaced it and the path              could not be recovered)",
            launch.display()
        );
    }
    let mut cmd = std::process::Command::new(&launch);
    cmd.arg("post-install-watch")
        .arg("--installer-pid")
        .arg(installer_pid.to_string())
        .arg("--installer-path")
        .arg(installer_path)
        .arg("--expected-version")
        .arg(expected_version);
    if spawned.already_exited {
        cmd.arg("--installer-already-exited");
    }
    if staged {
        // The copy's own path misclassifies flavour (%TEMP% →
        // PerUser) and probes the wrong binary; hand it the real
        // install path explicitly.
        cmd.arg("--origin-exe").arg(&exe);
    }
    // Unix has the SAME problem for a different reason (#1206): the tarball
    // installer renames the outgoing binary to `<name>.prev` BEFORE spawning
    // the watcher, so the watcher's own path is the old image. Without this
    // the verdict is always `SucceededUnverified` — which is worse than
    // useless, because a genuinely broken install then looks exactly like a
    // healthy one.
    #[cfg(not(target_os = "windows"))]
    if let Some(origin) = install_path_before_rename(&exe).filter(|_| !staged) {
        cmd.arg("--origin-exe").arg(&origin);
    }
    let _child = cmd
        .spawn()
        .context("spawning post-install-watch subprocess")?;
    tracing::info!(
        watcher_exe = %launch.display(),
        staged,
        "post-install watcher spawned"
    );
    // We deliberately don't capture the Child — when the parent
    // agent exits, the watcher is reparented to init/explorer
    // (Unix) / orphaned (Windows, where there's no init). Either
    // way it runs to completion on its own.
    Ok(())
}

/// Copy the running daemon EXE into the update staging directory
/// (the installer's own directory) and return the path to launch the
/// watcher from, plus whether it IS a staged copy. Fallback chain:
/// fixed name → PID-suffixed name (a previous update's watcher may
/// still be running from the fixed name; copying onto a running
/// image fails with a sharing violation) → the original EXE
/// (pre-fix behaviour — a watcher RestartManager may kill beats no
/// watcher at all).
///
/// The name deliberately avoids the "install"/"setup"/"update"/
/// "patch" substrings (Windows UAC's installer-detection heuristic
/// auto-elevates EXEs matching them — the P4 lib-naming rule).
#[cfg(target_os = "windows")]
fn stage_watcher_exe(
    exe: &std::path::Path,
    installer_path: &std::path::Path,
) -> (std::path::PathBuf, bool) {
    let Some(staging) = installer_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    else {
        return (exe.to_path_buf(), false);
    };
    let fixed = staging.join("roomlerd-watch.exe");
    match std::fs::copy(exe, &fixed) {
        Ok(_) => return (fixed, true),
        Err(e) => {
            tracing::debug!(error = %e, dest = %fixed.display(), "watcher stage copy (fixed name) failed; trying pid-suffixed name")
        }
    }
    let suffixed = staging.join(format!("roomlerd-watch-{}.exe", std::process::id()));
    match std::fs::copy(exe, &suffixed) {
        Ok(_) => (suffixed, true),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "staging watcher copy failed; spawning watcher from the install-dir EXE \
                 (RestartManager may shut it down mid-install)"
            );
            (exe.to_path_buf(), false)
        }
    }
}

/// Resolve the effective update-check cadence for this run. Order:
///
/// 1. `ROOMLERD_UPDATE_INTERVAL_H` env var (parses an unsigned
///    integer count of hours; non-positive or non-numeric is ignored
///    so a typo can't accidentally disable updates).
/// 2. `update_check_interval_h` field on `AgentConfig`, if set.
/// 3. Built-in [`CHECK_INTERVAL`] (24 h).
///
/// Logged at startup for operator transparency. Pure resolver lives
/// in [`resolve_check_interval_with`] so tests don't have to mutate
/// process env (which races between parallel test runs).
pub fn resolve_check_interval(cfg: &crate::config::AgentConfig) -> Duration {
    let env_val = node_env("UPDATE_INTERVAL_H");
    resolve_check_interval_with(env_val.as_deref(), cfg.update_check_interval_h)
}

/// Pure cadence resolver. Mirrors the precedence documented on
/// [`resolve_check_interval`]; `env_value` is whatever the env var
/// would have parsed to (caller's responsibility), `cfg_value` is
/// the config-file field. Both default-to-fall-through on invalid
/// input so a typo in either layer can't disable updates.
pub(crate) fn resolve_check_interval_with(
    env_value: Option<&str>,
    cfg_value: Option<u32>,
) -> Duration {
    if let Some(s) = env_value
        && let Ok(h) = s.trim().parse::<u32>()
        && h > 0
    {
        return Duration::from_secs(u64::from(h) * 3600);
    }
    if let Some(h) = cfg_value
        && h > 0
    {
        return Duration::from_secs(u64::from(h) * 3600);
    }
    CHECK_INTERVAL
}

/// S1a — process-wide forced-update trigger. The WS handler
/// (`rc:agent.update`) calls [`request_update_now`]; the periodic loop
/// consumes the channel. A `OnceLock` (not a threaded parameter) so the
/// signaling stack's signatures stay untouched — one updater per
/// process makes the singleton honest.
static UPDATE_TRIGGER: std::sync::OnceLock<tokio::sync::mpsc::Sender<Option<String>>> =
    std::sync::OnceLock::new();

/// Create the forced-update trigger channel and register its sender.
/// Called once from `run_cmd`; the returned receiver goes to
/// [`run_periodic`]. When auto-update is disabled the receiver is
/// simply dropped and [`request_update_now`] reports `false`.
pub fn install_update_trigger() -> tokio::sync::mpsc::Receiver<Option<String>> {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let _ = UPDATE_TRIGGER.set(tx);
    rx
}

/// Request an immediate update cycle (server-pushed `rc:agent.update`).
/// `pin` = optional release tag; `None` = latest. Returns `false` when
/// nothing is listening (auto-update disabled, trigger not installed)
/// or the small queue is full.
pub fn request_update_now(pin: Option<String>) -> bool {
    match UPDATE_TRIGGER.get() {
        Some(tx) => tx.try_send(pin).is_ok(),
        None => false,
    }
}

/// Act on a check outcome: log, and on `UpdateReady` spawn the
/// installer + signal shutdown. Returns `true` when the loop should
/// exit (installer running, agent about to be replaced). Shared by the
/// periodic and forced paths.
fn act_on_outcome(outcome: CheckOutcome, shutdown_tx: &tokio::sync::watch::Sender<bool>) -> bool {
    match outcome {
        CheckOutcome::UpToDate { current, latest } => {
            tracing::info!(current = %current, latest = %latest, "up to date");
            false
        }
        CheckOutcome::UpdateReady {
            current,
            latest,
            installer_path,
        } => {
            tracing::warn!(
                current = %current,
                latest = %latest,
                path = %installer_path.display(),
                "new release available — spawning installer and exiting"
            );
            if let Err(e) = spawn_installer_with_watch(&installer_path, Some(&latest)) {
                tracing::error!(error = %e, "installer spawn failed; will retry next cycle");
                // A failed install is otherwise INVISIBLE: the version the
                // server sees simply never moves, which reads as "the update
                // button does nothing". Raise the same operator sentinel the
                // fatal-Goodbye path uses so the host says why.
                let _ = crate::notify::raise_attention_machine_aware(&format!(
                    "Self-update to {latest} failed and this node is still on {current}. {e}"
                ));
                return false;
            }
            let _ = shutdown_tx.send(true);
            true
        }
        CheckOutcome::Skipped(reason) => {
            tracing::info!(reason = %reason, "update check skipped");
            false
        }
    }
}

/// Periodic update loop. Returns only on shutdown. Runs `check_once`
/// immediately, then on a fixed cadence. On `UpdateReady` the loop
/// spawns the installer and sends `true` on the shutdown channel so
/// the rest of the agent tears down cleanly.
///
/// `trigger_rx` (S1a) delivers operator-forced cycles pushed over the
/// WS (`rc:agent.update`): those bypass the transfer-defer gate (the
/// operator asked NOW; a warn is logged if transfers are active) but
/// still honour the install-storm cooldown marker.
pub async fn run_periodic(
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    interval: Duration,
    mut trigger_rx: tokio::sync::mpsc::Receiver<Option<String>>,
) {
    // macOS: the agent processes install NOTHING — `com.roomler.update`
    // (the root update helper) owns check+download+verify+install, and this
    // loop reduces to forwarding rc:agent.update triggers to its wake file.
    // A runtime `cfg!` (not an attribute) so both bodies stay type-checked
    // on every platform.
    if cfg!(target_os = "macos") {
        return macos_forward_triggers(shutdown, trigger_rx).await;
    }
    let mut first = true;
    let mut consecutive_defers: u32 = 0;
    // The recheck the LAST defer chose (fast for a net transition, the
    // hour for transfers); `None` = full periodic interval.
    let mut defer_recheck: Option<Duration> = None;
    loop {
        if *shutdown.borrow() {
            return;
        }
        // Cooldown carve-out: if this worker started inside the
        // recent-install window (a previous instance just spawned an
        // installer), skip the immediate check and treat the loop as
        // if the periodic interval had already elapsed once. Prevents
        // the install-storm — see STARTUP_UPDATE_COOLDOWN doc.
        //
        // The log line is intentionally emitted *before* the sleep so
        // operators verifying the storm fix in the field can grep the
        // log for "suppressed by recent-install cooldown" within the
        // 5-min window — the previous (0.1.62) ordering put the log
        // *after* the 24h sleep, which made the suppression invisible
        // until the next periodic wake-up. Field repro on operator
        // 2026-05-02: cooldown was working (no storm) but verification
        // by grep failed because the line hadn't been written yet.
        let skip_first_check = first && recent_update_attempt(STARTUP_UPDATE_COOLDOWN);
        if skip_first_check {
            tracing::info!(
                cooldown_secs = STARTUP_UPDATE_COOLDOWN.as_secs(),
                "auto-updater: at-startup check suppressed by recent-install cooldown"
            );
        }
        // Last-iteration's defer (if any) shortens the sleep — the hour
        // for transfers, [`NET_DEFER_RECHECK`] for a network transition —
        // so the install fires soon after the gate clears instead of
        // waiting the full periodic interval again.
        let next_interval = defer_recheck.take().unwrap_or(interval);
        let mut forced: Option<Option<String>> = None;
        if !first || skip_first_check {
            tokio::select! {
                _ = tokio::time::sleep(next_interval) => {},
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                },
                maybe = trigger_rx.recv() => {
                    // Sender lives in the process-wide OnceLock, so None
                    // (channel closed) is unreachable in practice; guard
                    // anyway so a closed channel can't hot-loop us.
                    if let Some(pin) = maybe {
                        forced = Some(pin);
                    }
                },
            }
        }
        first = false;

        // S1a — operator-forced cycle (rc:agent.update). Bypasses the
        // transfer-defer gate (explicit operator intent) but keeps the
        // install-storm cooldown; a pinned tag skips the is-newer check
        // (pin_version semantics — the admin chose the version).
        if let Some(pin) = forced {
            if recent_update_attempt(STARTUP_UPDATE_COOLDOWN) {
                tracing::info!(
                    "forced update suppressed by recent-install cooldown; try again shortly"
                );
                continue;
            }
            let active = crate::files::active_transfer_count();
            if active > 0 {
                tracing::warn!(
                    active,
                    "forced update proceeding despite in-flight file transfers"
                );
            }
            let outcome = match pin {
                Some(tag) => pin_version(&tag).await,
                None => check_once().await,
            };
            if act_on_outcome(outcome, &shutdown_tx) {
                return;
            }
            continue;
        }

        // rc.19 gate: don't fire an installer while file transfers
        // are in flight. Pair with resumable transfers — even with
        // resume the install + restart kills the WebRTC peer for
        // several seconds, which is a UX nuisance. Skip the cycle
        // when active > 0 unless we've deferred MAX_CONSECUTIVE_DEFERS
        // times in a row (security-vs-uptime trade-off documented in
        // the rc.19 plan M3 fix).
        let active = crate::files::active_transfer_count();
        let net_transition = net_transition_recent();
        match decide_defer(active, net_transition, consecutive_defers) {
            DeferDecision::DeferOnce => {
                consecutive_defers = consecutive_defers.saturating_add(1);
                // R5b — a transition defer rechecks fast (the network
                // settles in minutes); a transfer defer keeps its hour.
                let recheck = if net_transition && active == 0 {
                    NET_DEFER_RECHECK
                } else {
                    TRANSFER_DEFER_RECHECK
                };
                defer_recheck = Some(recheck);
                tracing::info!(
                    active,
                    net_transition,
                    consecutive_defers,
                    next_check_secs = recheck.as_secs(),
                    "auto-updater: deferring — transfers in flight or the network just moved"
                );
                continue;
            }
            DeferDecision::ForceAfterDefers => {
                tracing::warn!(
                    active,
                    consecutive_defers,
                    "auto-updater: forcing update after {} consecutive defers",
                    MAX_CONSECUTIVE_DEFERS
                );
                consecutive_defers = 0;
                // Fall through to check_once.
            }
            DeferDecision::Proceed => {
                consecutive_defers = 0;
            }
        }

        let outcome = check_once().await;
        if act_on_outcome(outcome, &shutdown_tx) {
            return;
        }
    }
}

// ─── macOS update half (`com.roomler.update` + `update-helper`) ──────────
//
// `installer -target /` needs root and the DEFAULT macOS install is the
// per-user LaunchAgent alone, so an in-process self-update is structurally
// impossible there (and on two-half Macs the exit-to-update dance raced
// launchd — field 2026-08-24/25: four UpdateNow pushes knocked the MacBook
// offline). The fix is a THIRD launchd unit the pkg installs by default:
// a root, non-long-running helper that owns check+download+verify+install,
// with the agents reduced to touching a wake file. No agent process is ever
// the installer's parent and none exits for an update — the pkg postinstall
// re-bootstraps both halves.

/// The wake file the agents touch and `com.roomler.update`'s `WatchPaths`
/// watches. Lives in the sticky world-writable /var/tmp ON PURPOSE: the
/// per-user agent is not root, and the file is a pure WAKE SIGNAL —
/// **its content is deliberately ignored**. Anything else would hand every
/// local user a primitive: a pin honoured from here would let an
/// unprivileged writer make root install a GENUINE-BUT-OLD release (the
/// exact downgrade class `artifact_version` closed on Windows). Whoever
/// writes it can cause an update CHECK, never choose what gets installed.
pub const MACOS_UPDATE_TRIGGER: &str = "/private/var/tmp/roomler-update-check";

/// Touch the update-helper wake file. Portable (plain fs) so the forwarder
/// type-checks on every platform; only ever called on macOS at runtime.
///
/// O_NOFOLLOW + O_EXCL-free create: /var/tmp is sticky, another local user
/// could have planted a symlink at this exact name — refuse to write
/// through it rather than clobber whatever it points at with our bytes.
pub fn macos_queue_update_check() -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = opts.open(MACOS_UPDATE_TRIGGER)?;
    // Timestamp only — human courtesy for whoever cats the file. The helper
    // never reads it (see the const doc: content is not trusted).
    writeln!(f, "{}", chrono::Utc::now().to_rfc3339())
}

/// macOS replacement for the [`run_periodic`] body: forward every
/// `rc:agent.update` trigger to the wake file and do nothing else. The
/// periodic cadence lives in the helper's own `StartInterval` — ONE owner
/// of "check+download+verify+install", zero in-process installs.
///
/// Compiles on every platform (portable tokio + fs) so the whole updater
/// keeps type-checking in cross-platform CI lanes; only macOS takes it.
async fn macos_forward_triggers(
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    mut trigger_rx: tokio::sync::mpsc::Receiver<Option<String>>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
            maybe = trigger_rx.recv() => {
                let Some(pin) = maybe else { return };
                if let Some(tag) = pin {
                    // Deliberate: pins do NOT cross the trigger boundary
                    // (see MACOS_UPDATE_TRIGGER — content is untrusted).
                    // The helper installs LATEST; a pinned downgrade on
                    // macOS is a manual `sudo installer -pkg` by design.
                    tracing::warn!(
                        %tag,
                        "rc:agent.update pin ignored on macOS — the update helper installs \
                         the latest release; pinned installs are manual (sudo installer -pkg …)"
                    );
                }
                // RETIRED-NAME-ANCHOR(2): the anchor now covers only this comment's own
                // mention of the legacy paths. FR-46 P5a moved the live strings below to
                // /var/log/roomler and /etc/roomler; the legacy /var/log/roomler-agent and
                // /etc/roomler-agent are migrated by the .pkg postinstall. docs/fr/FR-46
                match macos_queue_update_check() {
                    Ok(()) => tracing::info!(
                        trigger = MACOS_UPDATE_TRIGGER,
                        "rc:agent.update queued for the root update helper (com.roomler.update); \
                         watch /var/log/roomler/update.log"
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        "could not write the update-helper wake file — if this Mac has \
                         /etc/roomler/disable-auto-update set, updates are manual"
                    ),
                }
            }
        }
    }
}

/// `roomlerd update-helper` — the body of `com.roomler.update`.
///
/// Root-only, single-shot: consume the wake file, honour the opt-out marker
/// and the install-storm cooldown, then check → download → verify →
/// `installer -pkg … -target /` **waiting** on it (unlike every agent-side
/// spawn there is nothing here the pkg replaces-and-restarts, so waiting is
/// safe and gives us the real exit code). The pkg's postinstall restarts
/// the agent halves; this process just reports and exits.
#[cfg(target_os = "macos")]
pub async fn run_update_helper() -> anyhow::Result<()> {
    use anyhow::{Context, bail};

    // SAFETY: geteuid takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        bail!(
            "update-helper must run as root (launchd system domain); euid={euid}. \
             This subcommand is the body of com.roomler.update — it is not meant \
             to be run by hand except as `sudo roomlerd update-helper`."
        );
    }

    // Consume the wake file FIRST, unconditionally, content unread —
    // `remove_file` does not follow symlinks, so a planted link is deleted,
    // never dereferenced. Removing before the gates means a wake that loses
    // to the cooldown doesn't re-fire the WatchPaths job in a tight loop.
    let _ = std::fs::remove_file(MACOS_UPDATE_TRIGGER);

    // The operator's opt-out. postinstall removes the launchd unit when this
    // marker is set, but a loaded job can linger until reboot (it must not
    // bootout its own ancestor mid-install) — so the helper honours the
    // marker itself, making the lingering job harmless.
    // FR-46 P5a — dual-read, and the SENSE is what makes it mandatory: this is
    // an OPT-OUT. Reading only the current path would auto-update a Mac whose
    // operator opted out under the legacy one, which is the one outcome the
    // marker exists to prevent. The .pkg postinstall migrates the directory, so
    // the legacy arm is normally dead; it covers a host whose move could not
    // happen and a helper still running from before the migration.
    // RETIRED-NAME-ANCHOR(6): the legacy opt-out marker, honoured as a fallback.
    // Deletable once no Mac can still have one. docs/fr/FR-46
    const OPT_OUT: [&str; 2] = [
        "/etc/roomler/disable-auto-update",
        "/etc/roomler-agent/disable-auto-update",
    ];
    if let Some(marker) = OPT_OUT.iter().find(|p| std::path::Path::new(p).exists()) {
        tracing::info!(marker = %marker, "auto-update opted out — exiting");
        return Ok(());
    }

    if recent_update_attempt(STARTUP_UPDATE_COOLDOWN) {
        tracing::info!(
            cooldown_secs = STARTUP_UPDATE_COOLDOWN.as_secs(),
            "update-helper: suppressed by recent-install cooldown"
        );
        return Ok(());
    }

    match check_once().await {
        CheckOutcome::UpToDate { current, latest } => {
            tracing::info!(%current, %latest, "update-helper: up to date");
            Ok(())
        }
        CheckOutcome::Skipped(reason) => {
            tracing::info!(%reason, "update-helper: check skipped");
            Ok(())
        }
        CheckOutcome::UpdateReady {
            current,
            latest,
            installer_path,
        } => {
            record_update_attempt();
            tracing::warn!(
                %current,
                %latest,
                pkg = %installer_path.display(),
                "update-helper: installing (installer -pkg … -target /)"
            );
            let status = std::process::Command::new("installer")
                .args(["-pkg"])
                .arg(&installer_path)
                .args(["-target", "/"])
                .status()
                .context("running installer(8)")?;
            if !status.success() {
                // Same operator sentinel act_on_outcome raises: a failed
                // install is otherwise invisible — the fleet version simply
                // never moves.
                let _ = crate::notify::raise_attention_machine_aware(&format!(
                    "Self-update to {latest} failed (installer exited {status}) and this \
                     Mac is still on {current}. See /var/log/install.log."
                ));
                bail!(
                    "installer(8) exited {status} installing {latest}; this Mac stays on \
                     {current}. Details: /var/log/install.log"
                );
            }
            tracing::info!(
                %latest,
                "update-helper: installed — postinstall re-bootstrapped the agent halves"
            );
            Ok(())
        }
    }
}

// RETIRED-NAME-ANCHOR-BEGIN
// The fixtures below are REAL published release-asset names. Rewriting them would
// make these tests assert against files that never existed, and the picker they
// exercise matches on exactly those strings.
// INVARIANT: a retired name here must be one that was actually shipped.
// docs/fr/FR-21
#[cfg(test)]
mod tests {
    /// #1206 — the post-install watcher must verify the INSTALL path, never
    /// its own path.
    ///
    /// The Unix tarball installer renames the outgoing binary to `<name>.prev`
    /// for rollback and only then spawns the watcher, so `current_exe()`
    /// resolves to the OLD image. Verifying that path made every healthy
    /// tarball update record `SucceededUnverified` — and a verdict that is
    /// always the same cannot distinguish a broken install from a good one,
    /// which is the entire reason the check exists.
    ///
    /// ⚠️ The first assertion is the load-bearing one: it is the exact input
    /// observed in the field (`/usr/bin/roomlerd.prev`, expected 0.4.48,
    /// reported 0.4.45). The second guards the inverse — a path that was NOT
    /// renamed must yield `None` so the caller's existing behaviour is
    /// untouched on every other platform and flow.
    /// FR-67 P2 — the `.deb` path had no watcher at all, and this is why.
    ///
    /// `apt` replaces `/usr/bin/roomlerd`, unlinking the running image, so from
    /// that instant `current_exe()` reads `/usr/bin/roomlerd (deleted)`.
    /// `Command::spawn` on that path fails ENOENT and the watcher is never born.
    ///
    /// Field-measured on a cluster node, 2026-09-06: **21 `post-install watcher
    /// spawn failed` in seven days and ZERO `post-install watcher started`**,
    /// with `last-install.json` untouched since 2026-08-29 while the host had
    /// moved from 0.4.16 to 0.4.73.
    ///
    /// ⚠️ The suffix is appended by the kernel to the readlink result, so
    /// stripping it is the exact inverse — the same coupling rule that keeps
    /// `install_path_before_rename` honest against the installer's own rename.
    ///
    /// ⚠️ The negative cases matter as much: a path that merely *contains* the
    /// word, or ends in it without the leading space, must be left alone.
    /// Mangling a real path would turn a spawn failure into a spawn of the
    /// wrong binary, which is worse.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn a_deleted_exe_path_is_recovered_so_the_watcher_can_spawn() {
        use std::path::{Path, PathBuf};

        assert_eq!(
            strip_deleted_suffix(Path::new("/usr/bin/roomlerd (deleted)")),
            Some(PathBuf::from("/usr/bin/roomlerd")),
            "the field case: apt replaced the image, so /proc/self/exe carries the marker"
        );

        assert_eq!(
            strip_deleted_suffix(Path::new("/usr/bin/roomlerd")),
            None,
            "an ordinary path has nothing to strip and must be left alone"
        );

        assert_eq!(
            strip_deleted_suffix(Path::new("/opt/my (deleted) tools/roomlerd")),
            None,
            "the marker only counts as a SUFFIX; a path that merely contains it is real"
        );

        assert_eq!(
            strip_deleted_suffix(Path::new("/usr/bin/roomlerd(deleted)")),
            None,
            "without the separating space this is a legitimate filename, not a marker"
        );

        assert_eq!(
            strip_deleted_suffix(Path::new(" (deleted)")),
            None,
            "stripping must never yield an empty path"
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn watcher_verifies_the_install_path_not_its_own_renamed_path() {
        use super::install_path_before_rename;
        use std::path::{Path, PathBuf};

        // The field case.
        assert_eq!(
            install_path_before_rename(Path::new("/usr/bin/roomlerd.prev")),
            Some(PathBuf::from("/usr/bin/roomlerd")),
            "a `.prev` watcher path must resolve to the binary the installer wrote"
        );

        // Not renamed => nothing to correct.
        assert_eq!(
            install_path_before_rename(Path::new("/usr/bin/roomlerd")),
            None,
            "a non-`.prev` path must be left alone rather than mangled"
        );

        // The inverse of the installer's own rename, so the two cannot drift.
        let dst = Path::new("/usr/bin/roomlerd");
        let renamed = dst.with_extension("prev");
        assert_eq!(
            install_path_before_rename(&renamed).as_deref(),
            Some(dst),
            "must be the exact inverse of the installer's with_extension(\"prev\")"
        );
    }

    use super::*;

    /// A non-root macOS agent must FAIL the spawn rather than report a
    /// successful handoff.
    ///
    /// This is the whole bug in one assertion: `installer -target /` cannot
    /// run as the per-user LaunchAgent, but `Command::spawn` succeeds anyway
    /// (the process starts, then dies), so the agent used to exit 0 — and
    /// `KeepAlive{SuccessfulExit=false}` then declines to restart it. An
    /// error here is what keeps the agent alive on the old version.
    #[cfg(target_os = "macos")]
    #[test]
    fn refuses_to_self_update_when_not_root() {
        // Running as root is a legitimate configuration (the system daemon),
        // and there the guard correctly does not apply — so skip rather than
        // assert the opposite and make the test environment-dependent.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        // The path is never touched: the euid check comes first, which is the
        // point — a guaranteed-doomed install must not even be attempted.
        let err = spawn_installer_inner(std::path::Path::new("/tmp/nonexistent-roomler.pkg"))
            .expect_err("a non-root self-update must be an error, not a handoff");
        let msg = err.to_string();
        assert!(
            msg.contains("requires root"),
            "the refusal must say WHY, so an operator reading the log knows the \
             update did not silently fail — got: {msg}"
        );
    }

    #[test]
    fn resolve_check_interval_default_is_24h() {
        assert_eq!(
            resolve_check_interval_with(None, None),
            CHECK_INTERVAL,
            "no env, no config → built-in default"
        );
    }

    #[test]
    fn resolve_check_interval_uses_config_field_when_no_env() {
        assert_eq!(
            resolve_check_interval_with(None, Some(168)),
            Duration::from_secs(168 * 3600),
            "weekly via config field"
        );
    }

    #[test]
    fn resolve_check_interval_env_overrides_config() {
        assert_eq!(
            resolve_check_interval_with(Some("6"), Some(168)),
            Duration::from_secs(6 * 3600),
            "env must win over config when both set"
        );
    }

    #[test]
    fn resolve_check_interval_ignores_invalid_env() {
        // A typo in the env var must NOT silently fall back to "no
        // updates" — it falls through to the config / default layers.
        assert_eq!(
            resolve_check_interval_with(Some("not-a-number"), Some(48)),
            Duration::from_secs(48 * 3600)
        );
    }

    #[test]
    fn resolve_check_interval_ignores_zero_env_and_zero_config() {
        // Zero is ambiguous ("disable?" vs "tight loop?"). Both
        // layers fall through; the built-in default ultimately wins.
        assert_eq!(
            resolve_check_interval_with(Some("0"), Some(48)),
            Duration::from_secs(48 * 3600),
            "zero env → fall through to config"
        );
        assert_eq!(
            resolve_check_interval_with(None, Some(0)),
            CHECK_INTERVAL,
            "zero config → fall through to default"
        );
    }

    #[test]
    fn resolve_check_interval_trims_env_whitespace() {
        assert_eq!(
            resolve_check_interval_with(Some(" 12 "), None),
            Duration::from_secs(12 * 3600)
        );
    }

    #[test]
    fn recent_update_attempt_at_returns_false_when_marker_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("update-attempt");
        // No file at `p`: the OS returns ENOENT, function returns false.
        assert!(!recent_update_attempt_at(&p, Duration::from_secs(300)));
    }

    #[test]
    fn recent_update_attempt_at_returns_true_when_marker_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("update-attempt");
        std::fs::write(&p, b"now").expect("write marker");
        // File just written: mtime is roughly Instant::now(); a 5-min
        // cooldown definitely covers a sub-millisecond elapsed.
        assert!(recent_update_attempt_at(&p, Duration::from_secs(300)));
    }

    #[test]
    fn recent_update_attempt_at_returns_false_when_cooldown_too_short() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("update-attempt");
        std::fs::write(&p, b"now").expect("write marker");
        // Cooldown == 0: no window can be fresh enough. Locks the
        // boundary: a pathological zero must not bypass the gate.
        assert!(!recent_update_attempt_at(&p, Duration::ZERO));
    }

    #[test]
    fn recent_update_attempt_at_returns_false_when_marker_old() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("update-attempt");
        std::fs::write(&p, b"now").expect("write marker");
        // Sleep past the (tiny) cooldown so elapsed > cooldown.
        std::thread::sleep(Duration::from_millis(60));
        assert!(!recent_update_attempt_at(&p, Duration::from_millis(20)));
    }

    #[test]
    fn startup_update_cooldown_is_five_minutes() {
        // Lock the value: any future "make it shorter to retry faster"
        // change should require an explicit reason to land. A too-short
        // cooldown re-opens the install-storm window from operator.
        assert_eq!(STARTUP_UPDATE_COOLDOWN, Duration::from_secs(300));
    }

    #[test]
    fn verify_sha256_accepts_matching_digest() {
        // Known SHA256 of "hello" (sha256sum gives this).
        let bytes = b"hello";
        let digest = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_sha256(bytes, digest).is_ok());
    }

    #[test]
    fn verify_sha256_is_case_insensitive_on_hex() {
        let bytes = b"hello";
        let digest = "sha256:2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        assert!(verify_sha256(bytes, digest).is_ok());
    }

    #[test]
    fn verify_sha256_rejects_mismatch() {
        let bytes = b"hello";
        let digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let err = verify_sha256(bytes, digest).unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"));
    }

    #[test]
    fn verify_sha256_rejects_wrong_algorithm() {
        let bytes = b"hello";
        // sha512 of "hello" is *much* longer than 64 hex chars but
        // we don't even reach that check — the prefix mismatch
        // fires first.
        let digest = "sha512:abc";
        let err = verify_sha256(bytes, digest).unwrap_err();
        assert!(err.to_string().contains("unsupported digest algorithm"));
    }

    #[test]
    fn verify_sha256_rejects_malformed_length() {
        let bytes = b"hello";
        let digest = "sha256:abc"; // far too short
        let err = verify_sha256(bytes, digest).unwrap_err();
        assert!(err.to_string().contains("malformed sha256 digest length"));
    }

    #[test]
    fn verify_sha256_rejects_missing_prefix() {
        // A bare hex string without the `sha256:` prefix would slip
        // past a naive `strip_prefix`. Reject explicitly.
        let bytes = b"hello";
        let digest = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let err = verify_sha256(bytes, digest).unwrap_err();
        assert!(err.to_string().contains("unsupported digest algorithm"));
    }

    #[test]
    fn parse_version_handles_agent_prefix_and_v_prefix() {
        assert_eq!(parse_version("agent-v0.1.36"), Some((0, 1, 36, u64::MAX)));
        assert_eq!(parse_version("v0.1.36"), Some((0, 1, 36, u64::MAX)));
        assert_eq!(parse_version("0.1.36"), Some((0, 1, 36, u64::MAX)));
    }

    #[test]
    fn parse_version_handles_final_and_rc_shapes() {
        // Final versions: pre rank = u64::MAX so they outrank rc.N.
        assert_eq!(parse_version("agent-v1.2.3"), Some((1, 2, 3, u64::MAX)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3, u64::MAX)));
        // rc.N with dot separator (current convention as of 0.3.0).
        assert_eq!(parse_version("agent-v0.3.0-rc.1"), Some((0, 3, 0, 1)));
        assert_eq!(parse_version("agent-v0.3.0-rc.4"), Some((0, 3, 0, 4)));
        // rc.N without dot separator (legacy `0.1.36-rc1` shape).
        assert_eq!(parse_version("v1.2.3-rc1"), Some((1, 2, 3, 1)));
        // rc.N with hyphen separator (semver-ish).
        assert_eq!(parse_version("v1.2.3-rc-7"), Some((1, 2, 3, 7)));
        // Build metadata or other pre-release labels rank as final
        // so a forward-compat `-beta.5` tag doesn't accidentally
        // rank below an rc.
        assert_eq!(parse_version("v1.2.3-beta.5"), Some((1, 2, 3, u64::MAX)));
    }

    #[test]
    fn parse_version_rejects_malformed() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("not-a-version"), None);
        assert_eq!(parse_version("v1.2.x"), None);
    }

    #[test]
    fn is_newer_compares_major_minor_patch() {
        assert!(is_newer("agent-v0.2.0", "agent-v0.1.99"));
        assert!(is_newer("agent-v0.1.36", "agent-v0.1.35"));
        assert!(is_newer("agent-v1.0.0", "agent-v0.99.99"));
        assert!(!is_newer("agent-v0.1.35", "agent-v0.1.35"));
        assert!(!is_newer("agent-v0.1.34", "agent-v0.1.35"));
    }

    #[test]
    fn is_newer_orders_rc_within_same_release() {
        // Field bug 2026-05-06: rc.3 vs rc.4 both parsed to (0,3,0)
        // and `is_newer(rc.4, rc.3)` returned false. Lock the contract.
        assert!(is_newer("agent-v0.3.0-rc.4", "agent-v0.3.0-rc.3"));
        assert!(is_newer("agent-v0.3.0-rc.10", "agent-v0.3.0-rc.9"));
        assert!(!is_newer("agent-v0.3.0-rc.3", "agent-v0.3.0-rc.4"));
        assert!(!is_newer("agent-v0.3.0-rc.4", "agent-v0.3.0-rc.4"));
    }

    #[test]
    fn is_newer_ranks_final_above_rc_of_same_release() {
        // Final 0.3.0 is newer than every 0.3.0-rc.N.
        assert!(is_newer("agent-v0.3.0", "agent-v0.3.0-rc.4"));
        assert!(is_newer("agent-v0.3.0", "agent-v0.3.0-rc.99"));
        // And final does NOT trigger downgrade if the running version
        // is already final.
        assert!(!is_newer("agent-v0.3.0-rc.99", "agent-v0.3.0"));
    }

    #[test]
    fn is_newer_handles_cross_release_with_rc() {
        // 0.2.7 (final) < 0.3.0-rc.1 (early rc of next minor).
        assert!(is_newer("agent-v0.3.0-rc.1", "agent-v0.2.7"));
        // 0.3.0-rc.99 (very late rc) < 0.3.1 (next patch).
        assert!(is_newer("agent-v0.3.1", "agent-v0.3.0-rc.99"));
    }

    #[test]
    fn is_newer_refuses_downgrade_on_parse_failure() {
        // A malformed "latest" tag must NOT trigger a downgrade.
        assert!(!is_newer("bogus", "agent-v0.1.35"));
        assert!(!is_newer("agent-v0.1.36", "bogus"));
    }

    #[test]
    fn pick_asset_matches_platform_extension() {
        let assets = vec![
            GithubAsset {
                name: "roomler-agent-0.1.36-x86_64-pc-windows-msvc-unsigned.msi".into(),
                browser_download_url: "https://example.invalid/foo.msi".into(),
                size: 1234,
                digest: None,
            },
            GithubAsset {
                name: "roomler-agent-0.1.36_amd64.deb".into(),
                browser_download_url: "https://example.invalid/foo.deb".into(),
                size: 2345,
                digest: None,
            },
            // Both Linux arches, so this stays meaningful wherever it runs.
            // The fixture used to carry only the amd64 .deb, which quietly
            // assumed an x86_64 host — it fails on an aarch64 Linux box
            // (found by running this suite on a real arm64 agent, a target
            // CI has no job for).
            GithubAsset {
                name: "roomler-agent-0.1.36-aarch64-unknown-linux-gnu.deb".into(),
                browser_download_url: "https://example.invalid/foo-arm64.deb".into(),
                size: 2346,
                digest: None,
            },
            // Both Linux FORMATS too. `pick_asset_for_platform` consults the
            // host for dpkg/apt, so a .deb-only fixture makes this test pass
            // on Debian CI and fail on a Fedora box — an environment-dependent
            // test, which is how the arch assumption above hid for so long.
            // Carrying both keeps the assertion about the PLATFORM, not about
            // whatever package manager the runner happens to have.
            GithubAsset {
                name: "roomler-agent-0.1.36-x86_64-unknown-linux-gnu.tar.gz".into(),
                browser_download_url: "https://example.invalid/foo.tgz".into(),
                size: 2347,
                digest: None,
            },
            GithubAsset {
                name: "roomler-agent-0.1.36-aarch64-unknown-linux-gnu.tar.gz".into(),
                browser_download_url: "https://example.invalid/foo-arm64.tgz".into(),
                size: 2348,
                digest: None,
            },
            GithubAsset {
                name: "roomler-agent-0.1.36-x86_64-apple-darwin.pkg".into(),
                browser_download_url: "https://example.invalid/foo.pkg".into(),
                size: 3456,
                digest: None,
            },
        ];
        let pick = pick_asset_for_platform(&assets);
        assert!(pick.is_some(), "expected a pick on this platform");
        let name = &pick.unwrap().name;
        #[cfg(target_os = "windows")]
        assert!(name.ends_with(".msi"));
        #[cfg(target_os = "linux")]
        {
            // Either Linux format is correct — which one depends on whether
            // THIS host can run dpkg, which is not what this test is about.
            assert!(
                name.ends_with(".deb") || name.ends_with(".tar.gz"),
                "linux host took {name}"
            );
            // …and the one built for THIS arch, never the sibling.
            #[cfg(target_arch = "x86_64")]
            assert!(!name.contains("aarch64"), "x86_64 host picked {name}");
            #[cfg(target_arch = "aarch64")]
            assert!(name.contains("aarch64"), "aarch64 host picked {name}");
        }
        #[cfg(target_os = "macos")]
        assert!(name.ends_with(".pkg"));
        let _ = name; // silence unused warning on non-matched targets
    }

    // ── Linux .deb install candidates ───────────────────────────────
    // The regression these lock: a non-root daemon must not be able to
    // conclude "installed" from pkexec merely being spawnable, and a
    // root daemon must never route through an escalator that can block
    // on a polkit agent that does not exist on a headless host.

    const DEB: &str = "/tmp/roomler-agent-update/roomler-agent.deb";

    #[test]
    fn linux_install_root_never_escalates() {
        let c = linux_install_candidates(0, DEB);
        assert!(
            c.iter().all(|(bin, _)| *bin != "pkexec" && *bin != "sudo"),
            "root must install directly — an escalator can hang on a \
             missing polkit agent: {c:?}"
        );
        assert_eq!(c[0].0, "apt-get", "apt-get first — it resolves depends");
        assert_eq!(c[1].0, "dpkg", "dpkg is the offline fallback");
    }

    #[test]
    fn linux_install_non_root_escalates_pkexec_then_sudo() {
        let c = linux_install_candidates(1000, DEB);
        let bins: Vec<&str> = c.iter().map(|(b, _)| *b).collect();
        assert_eq!(bins, vec!["pkexec", "sudo"]);
        assert_eq!(
            c[1].1.first().map(String::as_str),
            Some("-n"),
            "sudo must be non-interactive — a service has no tty to prompt on"
        );
    }

    #[test]
    fn linux_install_candidates_always_end_with_the_package_path() {
        for euid in [0, 1000] {
            for (bin, args) in linux_install_candidates(euid, DEB) {
                assert_eq!(
                    args.last().map(String::as_str),
                    Some(DEB),
                    "{bin} must receive the .deb path as its last argument"
                );
            }
        }
    }

    fn mk_release(tag: &str, draft: bool, prerelease: bool) -> GithubRelease {
        GithubRelease {
            tag_name: tag.to_string(),
            assets: vec![],
            draft,
            prerelease,
        }
    }

    #[test]
    fn pick_latest_release_picks_highest_agent_tag() {
        // GitHub returns newest-first but we shouldn't rely on that.
        // Mix them up on purpose.
        let releases = vec![
            mk_release("agent-v0.1.30", false, true),
            mk_release("agent-v0.1.36", false, true),
            mk_release("agent-v0.1.35", false, true),
            mk_release("agent-v0.2.0", false, true),
        ];
        let picked = pick_latest_release(releases).expect("should pick one");
        assert_eq!(picked.tag_name, "agent-v0.2.0");
    }

    #[test]
    fn pick_latest_release_skips_drafts() {
        let releases = vec![
            mk_release("agent-v0.2.0", true, false),
            mk_release("agent-v0.1.36", false, true),
        ];
        let picked = pick_latest_release(releases).expect("should pick non-draft");
        assert_eq!(picked.tag_name, "agent-v0.1.36");
    }

    #[test]
    fn pick_latest_release_tolerates_prereleases() {
        // Our 0.x policy marked every release as prerelease. The
        // picker must NOT filter them out — otherwise auto-update
        // is stuck at "no release found" for every existing agent.
        let releases = vec![mk_release("agent-v0.1.37", false, true)];
        assert_eq!(
            pick_latest_release(releases).map(|r| r.tag_name),
            Some("agent-v0.1.37".to_string())
        );
    }

    #[test]
    fn pick_latest_release_ignores_non_agent_tags() {
        // Stray tags from other subsystems on the same repo must be
        // ignored — we only consume agent-v* releases.
        let releases = vec![
            mk_release("v1.2.3", false, false),
            mk_release("backend-v9.9.9", false, false),
            mk_release("agent-v0.1.36", false, true),
        ];
        let picked = pick_latest_release(releases).expect("should pick agent tag");
        assert_eq!(picked.tag_name, "agent-v0.1.36");
    }

    #[test]
    fn pick_latest_release_returns_none_when_nothing_matches() {
        assert!(pick_latest_release(vec![]).is_none());
        assert!(pick_latest_release(vec![mk_release("random-1.0.0", false, false)]).is_none());
        assert!(pick_latest_release(vec![mk_release("agent-v0.1.0", true, false)]).is_none());
    }

    /// A release carrying BOTH Linux architectures must never hand an
    /// agent the other one. Before the arch qualifier, `ends_with(".deb")`
    /// matched whichever .deb GitHub listed first, so publishing an arm64
    /// package would have made every x86_64 agent dpkg a foreign binary.
    #[cfg(target_os = "linux")]
    #[test]
    fn pick_asset_never_crosses_linux_architectures() {
        let mk = |name: &str| GithubAsset {
            name: name.into(),
            browser_download_url: "https://example.invalid/x.deb".into(),
            size: 1,
            digest: None,
        };
        // arm64 deliberately FIRST, so a naive picker would take it. Both
        // FORMATS are present so the fixture is installable whether or not
        // this host has dpkg — `pick_asset_for_unix` probes for it, and a
        // .deb-only fixture would make the test's outcome depend on the
        // runner's distro instead of on the arch logic under test.
        let assets = vec![
            mk("roomler-agent-0.3.0-rc.366-aarch64-unknown-linux-gnu.deb"),
            mk("roomler-agent-0.3.0-rc.366-aarch64-unknown-linux-gnu.tar.gz"),
            mk("roomler-agent-0.3.0-rc.366-x86_64-unknown-linux-gnu.deb"),
            mk("roomler-agent-0.3.0-rc.366-x86_64-unknown-linux-gnu.tar.gz"),
        ];
        let name = &pick_asset_for_unix(&assets)
            .expect("a Linux asset must match")
            .name;
        #[cfg(target_arch = "x86_64")]
        assert!(name.contains("x86_64"), "x86_64 agent took {name}");
        #[cfg(target_arch = "aarch64")]
        assert!(name.contains("aarch64"), "aarch64 agent took {name}");
        let _ = name;
    }

    /// An x86_64-only release must be SKIPPED on arm64 rather than
    /// installed — the field case that made `Update all` a silent no-op
    /// on the aarch64 host (2026-08-15).
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn pick_asset_skips_x86_only_release_on_arm64() {
        let assets = vec![GithubAsset {
            name: "roomler-agent-0.3.0-rc.366-x86_64-unknown-linux-gnu.deb".into(),
            browser_download_url: "https://example.invalid/x.deb".into(),
            size: 1,
            digest: None,
        }];
        assert!(pick_asset_for_unix(&assets).is_none());
    }

    /// The format preference matrix. `.deb` where dpkg/apt exist so the
    /// distro's package manager stays the source of truth; the tarball
    /// everywhere else, because a .deb there downloads and then fails to
    /// install — exactly how `Update all` looked like a silent no-op on the
    /// Fedora field host.
    #[test]
    fn pick_linux_asset_prefers_the_installable_format() {
        let mk = |name: &str| GithubAsset {
            name: name.into(),
            browser_download_url: "https://example.invalid/x".into(),
            size: 1,
            digest: None,
        };
        let both = vec![
            mk("roomler-agent-0.3.0-x86_64-unknown-linux-gnu.deb"),
            mk("roomler-agent-0.3.0-x86_64-unknown-linux-gnu.tar.gz"),
        ];
        let x86 = &["x86_64", "amd64"][..];

        let debian = pick_linux_asset(&both, x86, true).expect("deb host picks something");
        assert!(
            debian.name.ends_with(".deb"),
            "debian host took {}",
            debian.name
        );

        let other = pick_linux_asset(&both, x86, false).expect("non-deb host picks something");
        assert!(
            other.name.ends_with(".tar.gz"),
            "fedora-like host took {}",
            other.name
        );

        // A Debian host whose .deb is missing still updates via the tarball
        // rather than treating an absent artifact as a dead end.
        let tar_only = vec![mk("roomler-agent-0.3.0-x86_64-unknown-linux-gnu.tar.gz")];
        assert!(
            pick_linux_asset(&tar_only, x86, true)
                .expect("falls back")
                .name
                .ends_with(".tar.gz")
        );

        // …but a host with no dpkg and only a .deb published must SKIP, not
        // download something it cannot install.
        let deb_only = vec![mk("roomler-agent-0.3.0-x86_64-unknown-linux-gnu.deb")];
        assert!(pick_linux_asset(&deb_only, x86, false).is_none());
    }

    /// FR-27 — a release now carries TWO arch-matching Linux `.deb`s: the
    /// daemon's and the desktop companion's. Installing the companion as a
    /// daemon update would succeed at the dpkg level and freeze the daemon on
    /// its old version forever, which is worse than a failed update because
    /// nothing reports it.
    #[test]
    fn pick_linux_asset_never_takes_the_desktop_companion() {
        let mk = |name: &str| GithubAsset {
            name: name.into(),
            browser_download_url: "https://example.invalid/x".into(),
            size: 1,
            digest: None,
        };
        let x86 = &["x86_64", "amd64"][..];

        // The companion listed FIRST — asset order is GitHub's, not ours, so
        // "the daemon's happens to come first" is not a property we may rely on.
        // RETIRED-NAME-ANCHOR(8): these are real PUBLISHED asset names, frozen by
        // release-agent.yml as an immutable surface. Renaming them here would make the
        // test pass against names no release actually carries. docs/fr/FR-21
        let release = vec![
            mk("roomler-desktop-0.4.15-x86_64-unknown-linux-gnu.deb"),
            mk("roomler-agent-0.4.15-x86_64-unknown-linux-gnu.deb"),
            mk("roomler-agent-0.4.15-x86_64-unknown-linux-gnu.tar.gz"),
        ];
        let picked = pick_linux_asset(&release, x86, true).expect("must still find the daemon deb");
        assert_eq!(
            picked.name,
            "roomler-agent-0.4.15-x86_64-unknown-linux-gnu.deb"
        );

        // With ONLY the companion published, there is nothing installable —
        // skipping is right; taking it is the freeze.
        let companion_only = vec![mk("roomler-desktop-0.4.15-x86_64-unknown-linux-gnu.deb")];
        assert!(pick_linux_asset(&companion_only, x86, true).is_none());

        // cargo-deb's own spelling, as old releases carry it, still counts.
        // RETIRED-NAME-ANCHOR(4): same published-asset surface as above. docs/fr/FR-21
        assert!(is_daemon_asset("roomlerd_0.4.15-1_amd64.deb"));
        assert!(is_daemon_asset(
            "roomler-agent-0.4.15-x86_64-unknown-linux-gnu.tar.gz"
        ));
        assert!(!is_daemon_asset(
            "roomler-desktop-0.4.15-x86_64-unknown-linux-gnu.deb"
        ));
    }

    /// FR-46 (#1051): the daemon's published asset is named `roomlerd-…` from
    /// this release on. The rename was argued from reading every picker; this
    /// asserts it instead, on the one path where the prefix is load-bearing.
    ///
    /// The other three paths cannot regress from a rename because they never
    /// look at the prefix (Windows keys on `.msi` + `-permachine-`, macOS on
    /// `.pkg`, `scripts/install.sh` on an arch+format suffix), and a pre-0.4.16
    /// agent takes the first arch-matching `.deb`, which the server orders
    /// daemon-first via a `roomler-desktop-` DENYLIST — so it is unaffected by
    /// what the daemon is called.
    ///
    /// ⚠️ The legacy arm must stay: already-published releases carry the old
    /// name and an older host may still be updating from one.
    #[test]
    fn pick_linux_asset_takes_the_renamed_daemon_deb() {
        let mk = |name: &str| GithubAsset {
            name: name.into(),
            browser_download_url: "https://example.invalid/x".into(),
            size: 1,
            digest: None,
        };
        let x86 = &["x86_64", "amd64"][..];

        // Companion FIRST again — asset order is GitHub's, not ours.
        let release = vec![
            mk("roomler-desktop-0.4.38-x86_64-unknown-linux-gnu.deb"),
            mk("roomlerd-0.4.38-x86_64-unknown-linux-gnu.deb"),
            mk("roomlerd-0.4.38-x86_64-unknown-linux-gnu.tar.gz"),
        ];
        let picked =
            pick_linux_asset(&release, x86, true).expect("must find the renamed daemon deb");
        assert_eq!(picked.name, "roomlerd-0.4.38-x86_64-unknown-linux-gnu.deb");

        assert!(is_daemon_asset(
            "roomlerd-0.4.38-x86_64-unknown-linux-gnu.deb"
        ));
        assert!(is_daemon_asset(
            "roomlerd-0.4.38-x86_64-unknown-linux-gnu.tar.gz"
        ));

        // RETIRED-NAME-ANCHOR(3): the legacy arm is a LIVE fallback for releases
        // already published under the old name — not decoration. docs/fr/FR-46
        assert!(is_daemon_asset(
            "roomler-agent-0.4.15-x86_64-unknown-linux-gnu.deb"
        ));
    }

    /// Format preference must never override architecture: a tarball for the
    /// wrong arch is as unusable as a .deb for the wrong arch.
    #[test]
    fn pick_linux_asset_never_crosses_arch_for_either_format() {
        let mk = |name: &str| GithubAsset {
            name: name.into(),
            browser_download_url: "https://example.invalid/x".into(),
            size: 1,
            digest: None,
        };
        // Wrong arch listed FIRST for both formats.
        let assets = vec![
            mk("roomler-agent-0.3.0-aarch64-unknown-linux-gnu.tar.gz"),
            mk("roomler-agent-0.3.0-aarch64-unknown-linux-gnu.deb"),
            mk("roomler-agent-0.3.0-x86_64-unknown-linux-gnu.tar.gz"),
            mk("roomler-agent-0.3.0-x86_64-unknown-linux-gnu.deb"),
        ];
        let x86 = &["x86_64", "amd64"][..];
        for tooling in [true, false] {
            let got = pick_linux_asset(&assets, x86, tooling).expect("a pick");
            assert!(got.name.contains("x86_64"), "x86_64 host took {}", got.name);
        }
        let arm = &["aarch64", "arm64"][..];
        for tooling in [true, false] {
            let got = pick_linux_asset(&assets, arm, tooling).expect("a pick");
            assert!(got.name.contains("aarch64"), "arm64 host took {}", got.name);
        }
    }

    #[test]
    fn pick_asset_returns_none_when_no_platform_match() {
        let assets = vec![GithubAsset {
            name: "roomler-agent-0.1.36.tar.gz".into(),
            browser_download_url: "https://example.invalid/foo.tgz".into(),
            size: 10,
            digest: None,
        }];
        assert!(pick_asset_for_platform(&assets).is_none());
    }

    fn mk_msi(name: &str) -> GithubAsset {
        GithubAsset {
            name: name.into(),
            browser_download_url: format!("https://example.invalid/{name}"),
            size: 2_000_000,
            digest: None,
        }
    }

    /// Field repro: the GitHub release listing returns assets in
    /// alphabetical order, which puts `…-perMachine-…msi` ahead of
    /// the plain `…-x86_64-…msi`. A perUser agent calling the OLD
    /// (pre-0.2.6) picker happily returned the perMachine MSI as the
    /// "first .msi" and the cross-flavour launch condition silently
    /// rejected the install. Lock the new behaviour: perUser flavour
    /// picks the perUser MSI even when perMachine is alphabetically
    /// first.
    #[test]
    fn pick_asset_per_user_skips_per_machine_msi() {
        let assets = vec![
            mk_msi("roomler-agent-0.2.5-perMachine-x86_64-pc-windows-msvc-unsigned.msi"),
            mk_msi("roomler-agent-0.2.5-x86_64-pc-windows-msvc-unsigned.msi"),
        ];
        let pick = pick_asset_for_windows(&assets, WindowsInstallFlavour::PerUser)
            .expect("perUser must find its MSI");
        assert!(
            !pick.name.to_lowercase().contains("-permachine-"),
            "perUser picked {}",
            pick.name
        );
    }

    #[test]
    fn pick_asset_per_machine_picks_per_machine_msi() {
        let assets = vec![
            mk_msi("roomler-agent-0.2.5-x86_64-pc-windows-msvc-unsigned.msi"),
            mk_msi("roomler-agent-0.2.5-perMachine-x86_64-pc-windows-msvc-unsigned.msi"),
        ];
        let pick = pick_asset_for_windows(&assets, WindowsInstallFlavour::PerMachine)
            .expect("perMachine must find its MSI");
        assert!(
            pick.name.to_lowercase().contains("-permachine-"),
            "perMachine picked {}",
            pick.name
        );
    }

    /// Defensive fallback: if the release ships only one MSI flavour
    /// (e.g. an old 0.2.0 tag that predates the perMachine MSI), the
    /// agent should still self-update against the available MSI rather
    /// than skip the release entirely. The cross-flavour install will
    /// silently no-op against the launch condition; that's strictly
    /// better than agents stuck on an old version forever.
    #[test]
    fn pick_asset_per_machine_falls_back_when_only_per_user_present() {
        let assets = vec![mk_msi(
            "roomler-agent-0.2.0-x86_64-pc-windows-msvc-unsigned.msi",
        )];
        let pick = pick_asset_for_windows(&assets, WindowsInstallFlavour::PerMachine)
            .expect("fallback must produce something");
        assert!(pick.name.to_lowercase().ends_with(".msi"));
    }

    #[test]
    fn pick_asset_per_user_falls_back_when_only_per_machine_present() {
        let assets = vec![mk_msi(
            "roomler-agent-0.2.5-perMachine-x86_64-pc-windows-msvc-unsigned.msi",
        )];
        let pick = pick_asset_for_windows(&assets, WindowsInstallFlavour::PerUser)
            .expect("fallback must produce something");
        assert!(pick.name.to_lowercase().ends_with(".msi"));
    }

    #[test]
    fn pick_asset_per_user_returns_none_when_no_msi_at_all() {
        let assets = vec![GithubAsset {
            name: "roomler-agent-0.2.5_amd64.deb".into(),
            browser_download_url: "https://example.invalid/foo.deb".into(),
            size: 2_000_000,
            digest: None,
        }];
        assert!(pick_asset_for_windows(&assets, WindowsInstallFlavour::PerUser).is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn classify_install_flavour_recognises_program_files() {
        assert_eq!(
            classify_install_flavour_from_path(std::path::Path::new(
                r"C:\Program Files\roomler-agent\roomler-agent.exe"
            )),
            WindowsInstallFlavour::PerMachine
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn classify_install_flavour_recognises_program_files_x86() {
        // 32-bit installer on a 64-bit host lands here. We don't
        // ship one today but the path matcher must cover it so a
        // future 32-bit MSI doesn't get mis-classified as perUser.
        assert_eq!(
            classify_install_flavour_from_path(std::path::Path::new(
                r"C:\Program Files (x86)\roomler-agent\roomler-agent.exe"
            )),
            WindowsInstallFlavour::PerMachine
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn classify_install_flavour_recognises_localappdata() {
        // Default cargo-wix perUser destination on Win11.
        assert_eq!(
            classify_install_flavour_from_path(std::path::Path::new(
                r"C:\Users\operator\AppData\Local\Programs\roomler-agent\roomler-agent.exe"
            )),
            WindowsInstallFlavour::PerUser
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn classify_install_flavour_is_case_insensitive() {
        // Win32 paths are case-insensitive; a `PROGRAM FILES` spelling
        // (rare but possible from a misbehaving installer or
        // GetModuleFileName quirk) must still classify as perMachine.
        assert_eq!(
            classify_install_flavour_from_path(std::path::Path::new(
                r"C:\PROGRAM FILES\roomler-agent\roomler-agent.exe"
            )),
            WindowsInstallFlavour::PerMachine
        );
    }

    // ----- B6 regression: wizard-EXE locations are NOT classifiable ---

    #[cfg(target_os = "windows")]
    #[test]
    fn classify_wizard_exe_locations_returns_peruser_not_permachine() {
        // The install wizard EXE runs from wherever the operator
        // double-clicked it. Each of these paths classifies as
        // PerUser — which is wrong for the host's install state when
        // the operator picked perMachine in the wizard. Locks the
        // contract that the wizard MUST NOT rely on
        // `current_install_flavour`/`classify_install_flavour_from_path`
        // when deciding the spawn elevation path; instead use
        // `spawn_installer_for_flavour(path, operator_selected_flavour)`.
        // Field repro 2026-05-15 on a Windows field-test host: wizard ran from
        // %TEMP%, classified as PerUser, spawned msiexec /qn against
        // a perMachine MSI, Windows Installer returned 1625
        // ERROR_INSTALL_PACKAGE_REJECTED.
        let wizard_paths = [
            r"C:\Users\operator\Downloads\roomler-installer-0.3.0-rc.28-x86_64-pc-windows-msvc.exe",
            r"C:\Users\operator\Desktop\roomler-installer.exe",
            r"C:\Users\operator\AppData\Local\Temp\roomler-installer.exe",
            r"D:\Installers\roomler-installer.exe",
        ];
        for p in wizard_paths {
            assert_eq!(
                classify_install_flavour_from_path(std::path::Path::new(p)),
                WindowsInstallFlavour::PerUser,
                "wizard EXE at {p} unexpectedly classified as PerMachine"
            );
        }
    }

    // ----- stage_watcher_exe (RM-survivable watcher, 2026-08-21) ------

    #[cfg(target_os = "windows")]
    #[test]
    fn stage_watcher_exe_copies_next_to_the_installer() {
        let staging = std::env::temp_dir().join(format!("rw-stage-test-{}", std::process::id()));
        std::fs::create_dir_all(&staging).unwrap();
        let fake_exe = staging.join("fake-daemon.exe");
        std::fs::write(&fake_exe, b"MZ-fake").unwrap();
        let installer = staging.join("pkg.msi");

        let (launch, staged) = stage_watcher_exe(&fake_exe, &installer);
        assert!(staged, "copy into an existing staging dir must stage");
        assert_eq!(launch, staging.join("roomlerd-watch.exe"));
        assert_eq!(std::fs::read(&launch).unwrap(), b"MZ-fake");

        // A parent-less installer path has no staging dir — fall back
        // to spawning from the original EXE (pre-fix behaviour).
        let (fallback, staged) = stage_watcher_exe(&fake_exe, std::path::Path::new("pkg.msi"));
        assert!(!staged);
        assert_eq!(fallback, fake_exe);

        let _ = std::fs::remove_dir_all(&staging);
    }

    // ----- msiexec_argv (Plan rc.18 P1) -------------------------------

    #[test]
    fn msiexec_argv_per_user_uses_qn() {
        // perUser MSI installs without UAC; /qn (fully silent) is
        // correct AND the historical default — must not regress.
        let argv = msiexec_argv(
            std::path::Path::new(r"C:\Temp\roomler-agent.msi"),
            WindowsInstallFlavour::PerUser,
        );
        assert_eq!(argv[0], "/i");
        assert_eq!(argv[1], r"C:\Temp\roomler-agent.msi");
        assert_eq!(argv[2], "/qn");
        assert_eq!(argv[3], "/norestart");
        assert_eq!(argv[4], "/l*v");
        assert_eq!(argv[5], r"C:\Temp\roomler-agent.msi.log");
        assert_eq!(argv.len(), 6);
    }

    #[test]
    fn msiexec_argv_per_machine_uses_qn() {
        // rc.236: /qn, NOT /qb!. Basic UI can't display or answer the
        // FilesInUse dialog (MSI error 1607) raised when upgrading
        // over the running service → the 5/5-reproduced self-update
        // wedge. Elevation is handled by ShellExecuteExW(runas)
        // BEFORE msiexec runs, so /qn has no UAC to suppress — the
        // original rc.18 reason for /qb! no longer applies. Locks
        // against a well-meaning revert.
        let argv = msiexec_argv(
            std::path::Path::new(r"C:\Temp\roomler-agent-perMachine.msi"),
            WindowsInstallFlavour::PerMachine,
        );
        assert_eq!(argv[2], "/qn");
        // Other args identical to perUser shape.
        assert_eq!(argv[0], "/i");
        assert_eq!(argv[3], "/norestart");
    }

    #[test]
    fn msiexec_argv_always_includes_norestart() {
        // Both flavours: /norestart prevents a surprise reboot during
        // a service-context install. The post-install watcher restarts
        // the agent itself; the OS reboot is never needed.
        for flavour in [
            WindowsInstallFlavour::PerUser,
            WindowsInstallFlavour::PerMachine,
        ] {
            let argv = msiexec_argv(std::path::Path::new(r"C:\x.msi"), flavour);
            assert!(
                argv.iter().any(|a| a == "/norestart"),
                "missing /norestart for {flavour:?}"
            );
        }
    }

    #[test]
    fn msiexec_argv_path_is_second_arg() {
        // Argv order matters: msiexec expects `/i <path>` as the
        // first two tokens. Locks against a future refactor that
        // accidentally reorders.
        let argv = msiexec_argv(
            std::path::Path::new(r"D:\roomler-agent.msi"),
            WindowsInstallFlavour::PerUser,
        );
        assert_eq!(argv[0], "/i");
        assert_eq!(argv[1], r"D:\roomler-agent.msi");
    }

    // ----- rc.44 P4: msiexec_argv_with_properties (SystemContext) ----

    #[test]
    fn msiexec_argv_with_properties_appends_kv() {
        // Operator-supplied properties land AFTER the base /i path /qn
        // /norestart shape. Locks the wire format for the WiX CA's
        // condition (`ENABLE_SYSTEM_CONTEXT="1"`).
        let argv = msiexec_argv_with_properties(
            std::path::Path::new(r"C:\Temp\roomler-agent-perMachine.msi"),
            WindowsInstallFlavour::PerMachine,
            &[("ENABLE_SYSTEM_CONTEXT", "1")],
        );
        // Base shape preserved.
        assert_eq!(argv[0], "/i");
        assert_eq!(argv[2], "/qn");
        assert_eq!(argv[3], "/norestart");
        assert_eq!(argv[4], "/l*v");
        // Property appended verbatim after the base argv (incl. the
        // verbose-log pair), no shell-quoting.
        assert_eq!(argv[6], "ENABLE_SYSTEM_CONTEXT=1");
        assert_eq!(argv.len(), 7);
    }

    #[test]
    fn msiexec_argv_with_properties_explicit_disable_passes_zero() {
        // Plain perMachine over a SystemContext-on host: wizard passes
        // =0 explicitly so the WiX DisableSystemContext CA's condition
        // (`ENABLE_SYSTEM_CONTEXT="0"`) matches. Verifies the explicit
        // value survives the formatter.
        let argv = msiexec_argv_with_properties(
            std::path::Path::new(r"C:\Temp\roomler-agent-perMachine.msi"),
            WindowsInstallFlavour::PerMachine,
            &[("ENABLE_SYSTEM_CONTEXT", "0")],
        );
        assert!(argv.iter().any(|s| s == "ENABLE_SYSTEM_CONTEXT=0"));
    }

    #[test]
    fn msiexec_argv_with_properties_empty_matches_base() {
        // When the wizard passes no properties (perUser flavour), the
        // wrapper must produce byte-identical output to the base
        // msiexec_argv — no trailing whitespace, no extra tokens.
        let p = std::path::Path::new(r"C:\Temp\roomler-agent.msi");
        let base = msiexec_argv(p, WindowsInstallFlavour::PerUser);
        let with_empty = msiexec_argv_with_properties(p, WindowsInstallFlavour::PerUser, &[]);
        assert_eq!(base, with_empty);
    }

    #[test]
    fn msiexec_argv_with_properties_appends_multiple_in_order() {
        // Forward-compat: WiX may need additional public properties
        // in a future cycle (e.g. ENABLE_SYSTEM_CONTEXT=1 +
        // SYSTEM_CONTEXT_TIMEOUT_SECS=180). Order must be preserved.
        let argv = msiexec_argv_with_properties(
            std::path::Path::new(r"C:\Temp\roomler-agent-perMachine.msi"),
            WindowsInstallFlavour::PerMachine,
            &[("A", "1"), ("B", "two"), ("C", "3")],
        );
        let tail: Vec<&String> = argv.iter().skip(6).collect();
        assert_eq!(tail[0], "A=1");
        assert_eq!(tail[1], "B=two");
        assert_eq!(tail[2], "C=3");
    }

    // ----- rc.56 SystemContext-preserve hotfix (preserve_system_context_property_for) --

    #[test]
    fn preserve_system_context_per_user_always_empty() {
        // perUser installs have no SCM service to read from; even if the
        // env_value somehow returns "1" (impossible in production but
        // the test asserts robustness) the function must NOT pass the
        // property because perUser MSIs don't define the WiX
        // `EnableSystemContext` CA at all.
        assert_eq!(
            preserve_system_context_property_for(
                WindowsInstallFlavour::PerUser,
                Ok(Some("1".into())),
            ),
            Vec::<(String, String)>::new()
        );
        assert_eq!(
            preserve_system_context_property_for(WindowsInstallFlavour::PerUser, Ok(None),),
            Vec::<(String, String)>::new()
        );
        assert_eq!(
            preserve_system_context_property_for(
                WindowsInstallFlavour::PerUser,
                Err("nope".into()),
            ),
            Vec::<(String, String)>::new()
        );
    }

    #[test]
    fn preserve_system_context_per_machine_with_env_one_passes_property() {
        // THE headline rc.56 case: perMachine + service env var "1" ⇒
        // pass `ENABLE_SYSTEM_CONTEXT=1`. Without this assertion the
        // WiX DisableSystemContext CA would strip the env var on every
        // auto-update (field-repro WINHOST-A 2026-05-24, rc.55 self-update).
        let result = preserve_system_context_property_for(
            WindowsInstallFlavour::PerMachine,
            Ok(Some("1".into())),
        );
        assert_eq!(
            result,
            vec![("ENABLE_SYSTEM_CONTEXT".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn preserve_system_context_per_machine_with_env_absent_returns_empty() {
        // Plain perMachine install (no SystemContext ever enabled):
        // env var absent ⇒ pass nothing. WiX default property `'0'`
        // runs DisableSystemContext which is an idempotent no-op when
        // there's nothing to disable.
        assert_eq!(
            preserve_system_context_property_for(WindowsInstallFlavour::PerMachine, Ok(None),),
            Vec::<(String, String)>::new()
        );
    }

    #[test]
    fn preserve_system_context_per_machine_with_env_non_one_returns_empty() {
        // Defensive: the WiX CA condition is strict (`="1"`). If the
        // operator set the env var to "true" / "yes" / "on" (which
        // `system_swap_enabled` ACCEPTS as truthy), we conservatively
        // do NOT pass the property — WiX wouldn't match the condition
        // anyway. The mismatch between agent truthy-parse and WiX
        // strict-compare is documented; rc.57 candidate: tighten
        // `system_swap_enabled` to also be strict.
        for v in ["0", "true", "yes", "on", "TRUE", "", "anything"] {
            assert_eq!(
                preserve_system_context_property_for(
                    WindowsInstallFlavour::PerMachine,
                    Ok(Some(v.into())),
                ),
                Vec::<(String, String)>::new(),
                "non-\"1\" env value {v:?} must NOT pass ENABLE_SYSTEM_CONTEXT"
            );
        }
    }

    #[test]
    fn preserve_system_context_per_machine_with_read_error_returns_empty() {
        // SCM read error (e.g. service masked or missing): fall back to
        // pre-rc.56 behaviour rather than blocking the auto-update.
        // Operator can re-enable SystemContext manually via
        // `roomlerd enable-system-context` if needed.
        assert_eq!(
            preserve_system_context_property_for(
                WindowsInstallFlavour::PerMachine,
                Err("SCM RegOpenKeyEx returned 5 ERROR_ACCESS_DENIED".into()),
            ),
            Vec::<(String, String)>::new()
        );
    }

    // ---- rc.19 P3 — transfer-gated update timing ----

    #[test]
    fn decide_defer_proceeds_with_no_active_transfers() {
        assert_eq!(decide_defer(0, false, 0), DeferDecision::Proceed);
        // Defers counter is irrelevant when active=0 — the gate
        // resets at every Proceed.
        assert_eq!(decide_defer(0, false, 5), DeferDecision::Proceed);
    }

    #[test]
    fn decide_defer_defers_when_active_and_under_limit() {
        assert_eq!(decide_defer(1, false, 0), DeferDecision::DeferOnce);
        assert_eq!(decide_defer(3, false, 6), DeferDecision::DeferOnce);
    }

    /// R5b — a fresh network transition gates exactly like an active
    /// transfer: an install restart adjacent to a VPN capture forfeits
    /// every grandfathered flow (field 2026-08-16), so the cycle defers
    /// (fast recheck) until the window passes or the defer cap forces it.
    #[test]
    fn decide_defer_defers_on_a_fresh_network_transition() {
        assert_eq!(decide_defer(0, true, 0), DeferDecision::DeferOnce);
        assert_eq!(decide_defer(0, true, 6), DeferDecision::DeferOnce);
        assert_eq!(
            decide_defer(0, true, MAX_CONSECUTIVE_DEFERS),
            DeferDecision::ForceAfterDefers
        );
        // Both gates at once still just defers once per cycle.
        assert_eq!(decide_defer(2, true, 0), DeferDecision::DeferOnce);
    }

    #[test]
    fn decide_defer_forces_at_max_defers() {
        // Exactly at MAX_CONSECUTIVE_DEFERS should force.
        assert_eq!(
            decide_defer(1, false, MAX_CONSECUTIVE_DEFERS),
            DeferDecision::ForceAfterDefers
        );
        // And anything above it.
        assert_eq!(
            decide_defer(99, true, MAX_CONSECUTIVE_DEFERS + 50),
            DeferDecision::ForceAfterDefers
        );
    }

    #[test]
    // Compile-time contract lock — the constant-ness IS the point.
    #[allow(clippy::assertions_on_constants)]
    fn defer_constants_have_sensible_values() {
        // TRANSFER_DEFER_RECHECK should be shorter than CHECK_INTERVAL
        // (the whole point — react faster when uploads finish).
        assert!(TRANSFER_DEFER_RECHECK < CHECK_INTERVAL);
        // MAX_CONSECUTIVE_DEFERS at 1h cadence + 24h initial = at
        // most ~31h between successful update fires for a
        // chronically-busy host. Bounded.
        assert!(MAX_CONSECUTIVE_DEFERS >= 3 && MAX_CONSECUTIVE_DEFERS <= 24);
    }
}
// RETIRED-NAME-ANCHOR-END

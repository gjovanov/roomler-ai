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
//! `roomler-agent service install` re-launches the new version on
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

/// GitHub "Releases" repo slug. Centralised here so a fork can redirect
/// its update feed without grepping the codebase.
pub const RELEASES_REPO: &str = "gjovanov/roomler-ai";

/// Default proxy endpoint that caches GitHub's releases response on
/// the roomler-ai API server. Eliminates the per-IP GitHub rate
/// limit (60 req/hr unauth) that bites fleets of agents behind one
/// NAT. Override via `ROOMLER_AGENT_UPDATE_URL` env var for self-
/// hosted deployments or to bypass the proxy in dev. When the proxy
/// is unreachable we fall back to direct GitHub.
pub const DEFAULT_PROXY_URL: &str = "https://roomler.ai/api/agent/latest-release";

/// How often `run_periodic` wakes up and checks for a newer release.
/// 24 hours — matches the cadence of "operator deploys a fix and
/// wants the field to pick it up next day" without burning through
/// GitHub's 60-req-per-IP-per-hour unauthenticated REST quota when
/// many agents share a public IP (NAT'd offices, multiple boxes
/// behind one home router during rapid testing). Field report
/// 2026-04-27: 8 successive MSI installs across 5 boxes hit
/// `403 Forbidden` from GitHub before the hour reset.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// Minimum download size before we trust an installer artifact. A
/// GitHub redirect to a deleted asset returns a tiny HTML page; this
/// guards against running that as an installer.
pub const MIN_INSTALLER_BYTES: usize = 1_000_000;

/// At-startup update-check cooldown. If we last spawned an installer
/// within this window, skip the immediate `check_once` and proceed
/// straight to the periodic interval. Prevents the install-storm
/// failure mode found on host `e069019l` (2026-05-02): SCM service
/// supervisor + auto-updater + freshly-downloaded MSI = each newly
/// spawned worker re-detects the same pending update, fires another
/// installer, exits clean (code=0), supervisor respawns, repeat. The
/// 0.1.61 supervisor patch (code=0 -> immediate respawn, no backoff)
/// makes the cycle tighter (~1.5 s per turn). 5 minutes is more than
/// enough headroom for a Win11 MSI to land + the new binary to start
/// up + reach the clean-run threshold.
pub const STARTUP_UPDATE_COOLDOWN: Duration = Duration::from_secs(300);

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
/// loop never makes forward progress (field repro: PC50045
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
pub(crate) fn classify_install_flavour_from_path(p: &std::path::Path) -> WindowsInstallFlavour {
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

/// Pure Windows asset picker. Filters by the `-perMachine-` infix in
/// the asset filename (cargo-wix names them
/// `roomler-agent-<v>-perMachine-x86_64-…msi`; the perUser MSI uses
/// `roomler-agent-<v>-x86_64-…msi` with no infix). Falls back to "any
/// MSI" only if the matching flavour is missing — better to attempt a
/// cross-flavour install (which will silently no-op) than to skip the
/// update entirely on a release that, for whatever reason, only shipped
/// one flavour.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn pick_asset_for_windows(
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
pub(crate) fn pick_asset_for_unix(assets: &[GithubAsset]) -> Option<&GithubAsset> {
    let arch_linux = cfg!(all(target_os = "linux", target_arch = "x86_64"));
    let arch_mac = cfg!(target_os = "macos");
    for a in assets {
        let lower = a.name.to_lowercase();
        if arch_linux && (lower.ends_with("_amd64.deb") || lower.ends_with(".deb")) {
            return Some(a);
        }
        if arch_mac && lower.ends_with(".pkg") {
            return Some(a);
        }
    }
    None
}

/// Fetch the list of releases. Uses the roomler-ai backend proxy by
/// default (caches GitHub's response for 1h on the API server, so a
/// fleet of agents shares a single upstream call), falls back to
/// direct GitHub when the proxy is unreachable. Override via
/// `ROOMLER_AGENT_UPDATE_URL` env var for self-hosted deployments.
///
/// We do NOT use GitHub's `/releases/latest` because that endpoint
/// excludes prereleases unconditionally, and our v0.x policy briefly
/// marked everything as prerelease — agents shipped with 0.1.36
/// silently 404'd on every check until the proxy + workflow fix
/// landed. Always pull the full list and let `pick_latest_release`
/// apply our own filter (draft=false + tag prefix + parseable).
async fn fetch_latest_release() -> Result<GithubRelease> {
    let proxy_url =
        std::env::var("ROOMLER_AGENT_UPDATE_URL").unwrap_or_else(|_| DEFAULT_PROXY_URL.to_string());
    // Proxy first — handles rate limiting, returns the same JSON shape
    // as GitHub's /releases endpoint (slimmed to fields we read).
    match fetch_releases_from(&proxy_url).await {
        Ok(release) => return Ok(release),
        Err(e) => {
            tracing::info!(
                proxy = %proxy_url,
                error = %e,
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
        .user_agent(concat!("roomler-agent/", env!("CARGO_PKG_VERSION")))
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
async fn download_asset(asset: &GithubAsset) -> Result<PathBuf> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("roomler-agent/", env!("CARGO_PKG_VERSION")))
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
    // Integrity check: when GitHub / our proxy gave us a digest,
    // verify the downloaded bytes match. This catches both
    // corruption mid-flight (rare with TLS but possible with broken
    // middleboxes) and tampering by anyone who can serve responses
    // on the asset URL. Mismatched downloads do NOT touch disk —
    // we'd rather skip an update than run a wrong installer.
    if let Some(digest) = asset.digest.as_deref() {
        verify_sha256(&bytes, digest)
            .with_context(|| format!("verifying digest for {}", asset.name))?;
    } else {
        tracing::warn!(
            asset = %asset.name,
            "no digest field on asset; falling through to size floor only"
        );
    }
    let dir = std::env::temp_dir().join("roomler-agent-update");
    std::fs::create_dir_all(&dir).context("creating temp update dir")?;
    let path = dir.join(&asset.name);
    std::fs::write(&path, &bytes).context("writing installer to disk")?;
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
async fn fetch_release_by_tag(tag: &str) -> Result<GithubRelease> {
    let url = format!("https://api.github.com/repos/{RELEASES_REPO}/releases/tags/{tag}");
    let client = reqwest::Client::builder()
        .user_agent(concat!("roomler-agent/", env!("CARGO_PKG_VERSION")))
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
        Err(e) => return CheckOutcome::Skipped(format!("pin fetch {tag}: {e}")),
    };
    let asset = match pick_asset_for_platform(&release.assets) {
        Some(a) => a,
        None => {
            return CheckOutcome::Skipped(format!("no platform installer in release {tag}"));
        }
    };
    match download_asset(asset).await {
        Ok(path) => CheckOutcome::UpdateReady {
            current,
            latest: release.tag_name,
            installer_path: path,
        },
        Err(e) => CheckOutcome::Skipped(format!("pin download {tag}: {e}")),
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
        Err(e) => return CheckOutcome::Skipped(format!("fetch: {e}")),
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
    match download_asset(asset).await {
        Ok(path) => CheckOutcome::UpdateReady {
            current,
            latest: latest_parsed,
            installer_path: path,
        },
        Err(e) => CheckOutcome::Skipped(format!("download: {e}")),
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
/// - **macOS**: `installer -pkg <path> -target CurrentUserHomeDirectory`
///   runs the receipt-based install; prompts for auth if the pkg
///   uses /Library paths.
pub fn spawn_installer(installer_path: &std::path::Path) -> Result<()> {
    spawn_installer_with_watch(installer_path, None)
}

/// Spawn the installer for `installer_path` AND, when an
/// `expected_version` tag is provided, spawn a sibling
/// `roomler-agent post-install-watch` process that captures the
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
    // we get an install-storm. Field repro: e069019l 2026-05-02.
    record_update_attempt();
    let installer_pid = spawn_installer_inner(installer_path)?;
    if let Some(tag) = expected_version
        && let Err(e) = spawn_watcher(installer_pid, installer_path, tag)
    {
        // Don't fail the whole self-update flow on a watcher spawn
        // failure — the installer is already running and the agent
        // is about to exit; we lose the outcome JSON but the user
        // still gets the upgrade.
        tracing::warn!(error = %e, "post-install watcher spawn failed");
    }
    Ok(())
}

fn spawn_installer_inner(installer_path: &std::path::Path) -> Result<u32> {
    #[cfg(target_os = "windows")]
    {
        let path_str = installer_path.to_string_lossy().into_owned();
        let child = std::process::Command::new("msiexec")
            .args(["/i", &path_str, "/qn", "/norestart"])
            .spawn()
            .context("spawning msiexec")?;
        Ok(child.id())
    }
    #[cfg(target_os = "linux")]
    {
        let path_str = installer_path.to_string_lossy().into_owned();
        // Try pkexec first for an interactive password prompt; fall
        // back to direct dpkg if pkexec isn't installed.
        match std::process::Command::new("pkexec")
            .args(["apt-get", "install", "-y", &path_str])
            .spawn()
        {
            Ok(child) => Ok(child.id()),
            Err(_) => {
                let child = std::process::Command::new("dpkg")
                    .args(["--install", &path_str])
                    .spawn()
                    .context("spawning dpkg")?;
                Ok(child.id())
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let path_str = installer_path.to_string_lossy().into_owned();
        let child = std::process::Command::new("installer")
            .args(["-pkg", &path_str, "-target", "CurrentUserHomeDirectory"])
            .spawn()
            .context("spawning installer(8)")?;
        Ok(child.id())
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        bail!(
            "self-update spawn is not implemented on this platform ({:?})",
            installer_path
        )
    }
}

fn spawn_watcher(
    installer_pid: u32,
    installer_path: &std::path::Path,
    expected_version: &str,
) -> Result<()> {
    let exe = std::env::current_exe().context("locating own exe for watcher spawn")?;
    let _child = std::process::Command::new(&exe)
        .arg("post-install-watch")
        .arg("--installer-pid")
        .arg(installer_pid.to_string())
        .arg("--installer-path")
        .arg(installer_path)
        .arg("--expected-version")
        .arg(expected_version)
        .spawn()
        .context("spawning post-install-watch subprocess")?;
    // We deliberately don't capture the Child — when the parent
    // agent exits, the watcher is reparented to init/explorer
    // (Unix) / orphaned (Windows, where there's no init). Either
    // way it runs to completion on its own.
    Ok(())
}

/// Resolve the effective update-check cadence for this run. Order:
///
/// 1. `ROOMLER_AGENT_UPDATE_INTERVAL_H` env var (parses an unsigned
///    integer count of hours; non-positive or non-numeric is ignored
///    so a typo can't accidentally disable updates).
/// 2. `update_check_interval_h` field on `AgentConfig`, if set.
/// 3. Built-in [`CHECK_INTERVAL`] (24 h).
///
/// Logged at startup for operator transparency. Pure resolver lives
/// in [`resolve_check_interval_with`] so tests don't have to mutate
/// process env (which races between parallel test runs).
pub fn resolve_check_interval(cfg: &crate::config::AgentConfig) -> Duration {
    let env_val = std::env::var("ROOMLER_AGENT_UPDATE_INTERVAL_H").ok();
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

/// Periodic update loop. Returns only on shutdown. Runs `check_once`
/// immediately, then on a fixed cadence. On `UpdateReady` the loop
/// spawns the installer and sends `true` on the shutdown channel so
/// the rest of the agent tears down cleanly.
pub async fn run_periodic(
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    interval: Duration,
) {
    let mut first = true;
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
        // until the next periodic wake-up. Field repro on e069019l
        // 2026-05-02: cooldown was working (no storm) but verification
        // by grep failed because the line hadn't been written yet.
        let skip_first_check = first && recent_update_attempt(STARTUP_UPDATE_COOLDOWN);
        if skip_first_check {
            tracing::info!(
                cooldown_secs = STARTUP_UPDATE_COOLDOWN.as_secs(),
                "auto-updater: at-startup check suppressed by recent-install cooldown"
            );
        }
        if !first || skip_first_check {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {},
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                },
            }
        }
        first = false;
        let outcome = check_once().await;
        match outcome {
            CheckOutcome::UpToDate { current, latest } => {
                tracing::info!(current = %current, latest = %latest, "up to date");
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
                    continue;
                }
                let _ = shutdown_tx.send(true);
                return;
            }
            CheckOutcome::Skipped(reason) => {
                tracing::info!(reason = %reason, "update check skipped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // cooldown re-opens the install-storm window from e069019l.
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
        assert!(name.ends_with(".deb"));
        #[cfg(target_os = "macos")]
        assert!(name.ends_with(".pkg"));
        let _ = name; // silence unused warning on non-matched targets
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
                r"C:\Users\e069019l\AppData\Local\Programs\roomler-agent\roomler-agent.exe"
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
}

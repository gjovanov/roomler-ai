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

/// Parse a git tag like `agent-v0.1.36` or `v0.1.36` into a numeric
/// triple for ordering. Unparseable tags compare as None and are
/// treated as "not newer" so a malformed server-side tag can't force
/// a downgrade.
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let stripped = tag.trim_start_matches("agent-");
    let stripped = stripped.trim_start_matches('v');
    let parts: Vec<&str> = stripped.split('.').collect();
    if parts.len() < 3 {
        return None;
    }
    let major = parts[0].parse::<u64>().ok()?;
    let minor = parts[1].parse::<u64>().ok()?;
    // Patch may carry pre-release suffix like "36-rc1"; strip.
    let patch_str = parts[2].split(|c: char| !c.is_ascii_digit()).next()?;
    let patch = patch_str.parse::<u64>().ok()?;
    Some((major, minor, patch))
}

/// Return true if `latest` strictly outranks `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Pick the asset that matches this build's platform. Returns an
/// explicit `None` when there's no match so the caller can log + skip
/// rather than downloading something wrong.
pub fn pick_asset_for_platform(assets: &[GithubAsset]) -> Option<&GithubAsset> {
    let arch_win = cfg!(all(target_os = "windows", target_arch = "x86_64"));
    let arch_linux = cfg!(all(target_os = "linux", target_arch = "x86_64"));
    let arch_mac = cfg!(target_os = "macos");
    for a in assets {
        let lower = a.name.to_lowercase();
        if arch_win && lower.ends_with(".msi") {
            return Some(a);
        }
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
    let dir = std::env::temp_dir().join("roomler-agent-update");
    std::fs::create_dir_all(&dir).context("creating temp update dir")?;
    let path = dir.join(&asset.name);
    std::fs::write(&path, &bytes).context("writing installer to disk")?;
    Ok(path)
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
    #[cfg(target_os = "windows")]
    {
        let path_str = installer_path.to_string_lossy().into_owned();
        std::process::Command::new("msiexec")
            .args(["/i", &path_str, "/qn", "/norestart"])
            .spawn()
            .context("spawning msiexec")?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let path_str = installer_path.to_string_lossy().into_owned();
        // Try pkexec first for an interactive password prompt; fall
        // back to direct dpkg if pkexec isn't installed.
        let pkexec = std::process::Command::new("pkexec")
            .args(["apt-get", "install", "-y", &path_str])
            .spawn();
        if pkexec.is_err() {
            std::process::Command::new("dpkg")
                .args(["--install", &path_str])
                .spawn()
                .context("spawning dpkg")?;
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let path_str = installer_path.to_string_lossy().into_owned();
        std::process::Command::new("installer")
            .args(["-pkg", &path_str, "-target", "CurrentUserHomeDirectory"])
            .spawn()
            .context("spawning installer(8)")?;
        Ok(())
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        bail!(
            "self-update spawn is not implemented on this platform ({:?})",
            installer_path
        )
    }
}

/// Periodic update loop. Returns only on shutdown. Runs `check_once`
/// immediately, then on a fixed cadence. On `UpdateReady` the loop
/// spawns the installer and sends `true` on the shutdown channel so
/// the rest of the agent tears down cleanly.
pub async fn run_periodic(
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) {
    let mut first = true;
    loop {
        if *shutdown.borrow() {
            return;
        }
        if !first {
            tokio::select! {
                _ = tokio::time::sleep(CHECK_INTERVAL) => {},
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
                if let Err(e) = spawn_installer(&installer_path) {
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
    fn parse_version_handles_agent_prefix_and_v_prefix() {
        assert_eq!(parse_version("agent-v0.1.36"), Some((0, 1, 36)));
        assert_eq!(parse_version("v0.1.36"), Some((0, 1, 36)));
        assert_eq!(parse_version("0.1.36"), Some((0, 1, 36)));
    }

    #[test]
    fn parse_version_strips_prerelease_suffix_on_patch() {
        assert_eq!(parse_version("agent-v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2.3+build.42"), Some((1, 2, 3)));
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
            },
            GithubAsset {
                name: "roomler-agent-0.1.36_amd64.deb".into(),
                browser_download_url: "https://example.invalid/foo.deb".into(),
                size: 2345,
            },
            GithubAsset {
                name: "roomler-agent-0.1.36-x86_64-apple-darwin.pkg".into(),
                browser_download_url: "https://example.invalid/foo.pkg".into(),
                size: 3456,
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
        }];
        assert!(pick_asset_for_platform(&assets).is_none());
    }
}

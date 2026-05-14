//! `/api/agent/latest-release` — cached proxy of the GitHub releases
//! list for the agent's auto-updater.
//!
//! Why we proxy: GitHub's unauthenticated REST API allows 60 requests
//! per IP per hour. With many agents behind a single NAT (offices,
//! home networks during rapid testing) the quota gets exhausted in a
//! burst — every agent then sees `403 Forbidden` until the rate
//! resets. Field log 2026-04-27 hit exactly this after 8 successive
//! MSI installs across 5 boxes. By proxying through this endpoint:
//!
//!   - All agents share one cached response per cache window.
//!   - Our API server's IP gets the 60/hr quota (one cache miss per
//!     hour worst-case → trivially under the limit).
//!   - Stale-on-error: if GitHub is down, we serve the last cached
//!     value rather than failing every agent's check simultaneously.
//!
//! Cache lifecycle: lazy + TTL. First request after a cold cache
//! triggers a fetch; subsequent requests within `CACHE_TTL` return
//! the cached payload without touching GitHub. On a fetch error
//! after the TTL has expired we fall back to the stale value (with
//! a warn-level log) to keep the field path working through
//! upstream blips.
//!
//! No auth: agents call this endpoint before they have a session
//! and pretty much all the data is already public anyway via
//! github.com/gjovanov/roomler-ai/releases. CORS-OK by default.

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::{error::ApiError, state::AppState};

/// GitHub repo slug. A fork can override here without touching
/// agents.
const RELEASES_REPO: &str = "gjovanov/roomler-ai";

/// Cache TTL — 1 hour. With agents on a 24h poll cadence (post-0.1.44)
/// the back-pressure on this endpoint is dominated by the install
/// burst right after a tag push, so any window > a few minutes
/// effectively coalesces into one upstream call.
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// Cap on releases we'll return — same per_page used by the agent's
/// pre-proxy fetch path.
const RELEASES_PER_PAGE: usize = 30;

/// Subset of GitHub's release JSON the agent actually consults. We
/// don't need authors, body, html_url, or hundreds of bytes of CI
/// metadata. Slimming the response also makes the cache cheap.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
    /// GitHub Releases API exposes a `digest` field per asset of
    /// the form `"sha256:<hex>"` (added late 2024). Forwarded so
    /// the agent can verify the downloaded MSI / .deb / .pkg
    /// against this hash and reject corrupt or tampered files.
    /// Absent on releases that pre-date GitHub adding the field;
    /// the agent falls through to the size-floor check in that
    /// case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentRelease {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<AgentReleaseAsset>,
}

struct CacheEntry {
    fetched_at: Instant,
    payload: Vec<AgentRelease>,
}

/// Latest-release cache lives on AppState. Shared `Arc` so cloning
/// AppState is cheap; `RwLock` so concurrent agent reads don't
/// serialize through a mutex.
pub struct LatestReleaseCache {
    inner: RwLock<Option<CacheEntry>>,
}

impl LatestReleaseCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(None),
        })
    }
}

/// `GET /api/agent/latest-release` — returns the cached releases
/// list. No auth.
///
/// Response shape: `Vec<AgentRelease>`, mimicking the agent's
/// existing GitHub-shape parser so the agent-side code change is
/// just a URL swap.
pub async fn latest_release(
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentRelease>>, ApiError> {
    let cache = state.latest_release_cache.clone();

    // Fast path: serve a fresh cache without any upstream call.
    {
        let g = cache.inner.read().await;
        if let Some(entry) = g.as_ref()
            && entry.fetched_at.elapsed() < CACHE_TTL
        {
            return Ok(Json(entry.payload.clone()));
        }
    }

    // Slow path: TTL expired (or cold cache). Refetch from GitHub.
    match fetch_releases().await {
        Ok(releases) => {
            let mut g = cache.inner.write().await;
            *g = Some(CacheEntry {
                fetched_at: Instant::now(),
                payload: releases.clone(),
            });
            Ok(Json(releases))
        }
        Err(e) => {
            // Stale-on-error: if we have any prior payload, serve
            // it instead of breaking every agent's check on a
            // single GitHub blip. Log so the operator can see
            // upstream is unhappy.
            tracing::warn!(error = %e, "GitHub releases fetch failed; serving stale cache if any");
            let g = cache.inner.read().await;
            if let Some(entry) = g.as_ref() {
                return Ok(Json(entry.payload.clone()));
            }
            Err(ApiError::Internal(format!(
                "upstream releases fetch failed and no cache: {e}"
            )))
        }
    }
}

// ─── installer download proxy ─────────────────────────────────────────────────
//
// `GET /api/agent/installer/{flavour}/health` — JSON metadata about
// the MSI the wizard would download for this flavour.
//
// `GET /api/agent/installer/{flavour}` — streams the matching MSI
// from GitHub releases through our domain. Two reasons to proxy
// instead of redirecting to github.com:
//   1. Corporate ESET / Defender allow-lists are typically per-domain.
//      `roomler.ai`'s TLS cert is already in IT-managed allow-lists
//      (the agent's signaling traffic uses it); github.com is often
//      blocked outright in locked-down environments. PC50045 field
//      repro 2026-05-11.
//   2. Single source-of-truth for asset selection (perUser vs
//      perMachine). The wizard hits one URL per flavour, never picks
//      the wrong MSI alphabetically.
// Cache-Control: public, max-age=3600 so a CDN in front of roomler.ai
// can coalesce identical requests during a fleet rollout.

/// Query parameter for both `/installer/{flavour}` and
/// `/installer/{flavour}/health`. `version=latest` (default) picks
/// the most recent non-prerelease tag; an explicit tag name pins.
#[derive(Clone, Debug, Deserialize)]
pub struct InstallerQuery {
    #[serde(default = "default_version_latest")]
    pub version: String,
}

fn default_version_latest() -> String {
    "latest".to_string()
}

/// JSON returned by `/installer/{flavour}/health`. The wizard uses
/// `size` to render a download progress bar and `digest` to verify
/// the MSI bytes match the advertised hash before launching msiexec.
#[derive(Clone, Debug, Serialize)]
pub struct InstallerHealth {
    /// Resolved tag, e.g. `agent-v0.3.0-rc.27`.
    pub tag: String,
    /// Normalised flavour: `"peruser"` or `"permachine"`.
    pub flavour: String,
    /// Canonical asset filename, e.g.
    /// `roomler-agent-0.3.0-rc.27-perMachine-x86_64-pc-windows-msvc.msi`.
    pub filename: String,
    /// Asset size in bytes.
    pub size: u64,
    /// `"sha256:<hex>"` from GitHub's `digest` field. `None` on
    /// releases that pre-date the field.
    pub digest: Option<String>,
    /// The URI that, when GET'd, streams the MSI bytes. Always
    /// relative to the API root so the wizard composes it under
    /// roomler.ai (or staging.roomler.ai) transparently.
    pub uri: String,
}

/// `GET /api/agent/installer/{flavour}/health`.
pub async fn installer_health(
    State(state): State<AppState>,
    Path(flavour): Path<String>,
    Query(params): Query<InstallerQuery>,
) -> Result<Json<InstallerHealth>, ApiError> {
    let normalised = normalise_flavour(&flavour)?;
    let releases = ensure_releases_cached(&state).await?;
    let release = pick_release(&releases, &params.version).ok_or_else(|| {
        ApiError::NotFound(format!("no release matching version={}", params.version))
    })?;
    let asset = pick_installer_asset(&release.assets, normalised).ok_or_else(|| {
        ApiError::NotFound(format!(
            "no MSI asset for flavour {} in tag {}",
            normalised, release.tag_name
        ))
    })?;
    Ok(Json(InstallerHealth {
        tag: release.tag_name.clone(),
        flavour: normalised.to_string(),
        filename: asset.name.clone(),
        size: asset.size,
        digest: asset.digest.clone(),
        uri: format!(
            "/api/agent/installer/{}?version={}",
            normalised, params.version
        ),
    }))
}

/// `GET /api/agent/installer/{flavour}` — streams the MSI bytes.
pub async fn installer_proxy(
    State(state): State<AppState>,
    Path(flavour): Path<String>,
    Query(params): Query<InstallerQuery>,
) -> Result<Response, ApiError> {
    let normalised = normalise_flavour(&flavour)?;
    let releases = ensure_releases_cached(&state).await?;
    let release = pick_release(&releases, &params.version).ok_or_else(|| {
        ApiError::NotFound(format!("no release matching version={}", params.version))
    })?;
    let asset = pick_installer_asset(&release.assets, normalised).ok_or_else(|| {
        ApiError::NotFound(format!(
            "no MSI asset for flavour {} in tag {}",
            normalised, release.tag_name
        ))
    })?;

    let client = reqwest::Client::builder()
        .user_agent(concat!("roomler-ai-api/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| ApiError::Internal(format!("reqwest client build: {e}")))?;
    let upstream = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("upstream MSI fetch failed: {e}")))?;

    let status = upstream.status();
    if !status.is_success() {
        return Err(ApiError::Internal(format!(
            "upstream MSI fetch returned {}",
            status
        )));
    }
    let content_length = upstream.content_length();

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-msi")
        .header(
            "Content-Disposition",
            HeaderValue::from_str(&format!(
                "attachment; filename=\"{}\"",
                sanitise_header_value(&asset.name)
            ))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
        )
        .header("Cache-Control", "public, max-age=3600");
    if let Some(len) = content_length {
        builder = builder.header("Content-Length", len);
    }
    let body = Body::from_stream(upstream.bytes_stream());
    builder
        .body(body)
        .map_err(|e| ApiError::Internal(format!("response build failed: {e}")))
}

/// Strip CR/LF and quote characters from a header value to avoid
/// HTTP header injection if an upstream filename ever contains them.
fn sanitise_header_value(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\r' | '\n' | '"'))
        .collect()
}

fn normalise_flavour(s: &str) -> Result<&'static str, ApiError> {
    match s.to_ascii_lowercase().as_str() {
        "peruser" => Ok("peruser"),
        "permachine" => Ok("permachine"),
        other => Err(ApiError::BadRequest(format!(
            "unknown flavour {other:?}; expected peruser or permachine"
        ))),
    }
}

fn pick_release<'a>(releases: &'a [AgentRelease], version: &str) -> Option<&'a AgentRelease> {
    if version == "latest" {
        releases
            .iter()
            .find(|r| !r.draft && !r.prerelease)
            .or_else(|| releases.iter().find(|r| !r.draft))
    } else {
        let target_with_prefix = format!("agent-v{}", version.trim_start_matches("agent-v"));
        let target_bare = version.trim_start_matches("agent-v");
        releases.iter().find(|r| {
            r.tag_name == target_with_prefix || r.tag_name == target_bare || r.tag_name == version
        })
    }
}

/// Pick the matching MSI asset for the given normalised flavour.
/// Mirrors the agent-side `pick_asset_for_windows` decision: an asset
/// with the literal infix `-perMachine-` (any case) is perMachine; any
/// other `.msi` is perUser. Returns `None` when no `.msi` matches.
pub fn pick_installer_asset<'a>(
    assets: &'a [AgentReleaseAsset],
    flavour: &str,
) -> Option<&'a AgentReleaseAsset> {
    assets.iter().find(|a| {
        let name = a.name.to_lowercase();
        if !name.ends_with(".msi") {
            return false;
        }
        let is_permachine = name.contains("-permachine-") || name.contains("permachine.");
        match flavour {
            "permachine" => is_permachine,
            "peruser" => !is_permachine,
            _ => false,
        }
    })
}

/// Ensure the release cache is populated and fresh. Reuses the same
/// fast-path / slow-path / stale-on-error semantics as
/// [`latest_release`].
async fn ensure_releases_cached(state: &AppState) -> Result<Vec<AgentRelease>, ApiError> {
    let cache = state.latest_release_cache.clone();
    {
        let g = cache.inner.read().await;
        if let Some(entry) = g.as_ref()
            && entry.fetched_at.elapsed() < CACHE_TTL
        {
            return Ok(entry.payload.clone());
        }
    }
    match fetch_releases().await {
        Ok(releases) => {
            let mut g = cache.inner.write().await;
            *g = Some(CacheEntry {
                fetched_at: Instant::now(),
                payload: releases.clone(),
            });
            Ok(releases)
        }
        Err(e) => {
            let g = cache.inner.read().await;
            if let Some(entry) = g.as_ref() {
                tracing::warn!(error = %e, "GitHub releases fetch failed; serving stale cache");
                return Ok(entry.payload.clone());
            }
            Err(ApiError::Internal(format!(
                "upstream releases fetch failed and no cache: {e}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> AgentReleaseAsset {
        AgentReleaseAsset {
            name: name.to_string(),
            browser_download_url: format!("https://example/{name}"),
            size: 1024,
            digest: Some("sha256:deadbeef".to_string()),
        }
    }

    fn release(tag: &str, prerelease: bool, asset_names: &[&str]) -> AgentRelease {
        AgentRelease {
            tag_name: tag.to_string(),
            draft: false,
            prerelease,
            published_at: None,
            assets: asset_names.iter().map(|n| asset(n)).collect(),
        }
    }

    #[test]
    fn normalise_flavour_accepts_known_values_case_insensitively() {
        assert_eq!(normalise_flavour("peruser").unwrap(), "peruser");
        assert_eq!(normalise_flavour("PERUSER").unwrap(), "peruser");
        assert_eq!(normalise_flavour("PerUser").unwrap(), "peruser");
        assert_eq!(normalise_flavour("permachine").unwrap(), "permachine");
        assert_eq!(normalise_flavour("PerMachine").unwrap(), "permachine");
    }

    #[test]
    fn normalise_flavour_rejects_unknown() {
        let err = normalise_flavour("system").unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn pick_installer_asset_picks_permachine_by_infix() {
        let assets = [
            asset("roomler-agent-0.3.0-x86_64-pc-windows-msvc.msi"),
            asset("roomler-agent-0.3.0-perMachine-x86_64-pc-windows-msvc.msi"),
        ];
        let picked = pick_installer_asset(&assets, "permachine").unwrap();
        assert!(picked.name.contains("perMachine"));
    }

    #[test]
    fn pick_installer_asset_picks_peruser_when_no_permachine_infix() {
        let assets = [
            asset("roomler-agent-0.3.0-perMachine-x86_64-pc-windows-msvc.msi"),
            asset("roomler-agent-0.3.0-x86_64-pc-windows-msvc.msi"),
        ];
        let picked = pick_installer_asset(&assets, "peruser").unwrap();
        assert!(!picked.name.contains("perMachine"));
    }

    #[test]
    fn pick_installer_asset_ignores_non_msi() {
        let assets = [
            asset("roomler-agent-0.3.0-x86_64-unknown-linux-gnu.deb"),
            asset("roomler-agent-0.3.0-tray-x86_64.exe"),
        ];
        assert!(pick_installer_asset(&assets, "peruser").is_none());
    }

    #[test]
    fn pick_installer_asset_returns_none_when_no_match() {
        let assets = [asset(
            "roomler-agent-0.3.0-perMachine-x86_64-pc-windows-msvc.msi",
        )];
        assert!(pick_installer_asset(&assets, "peruser").is_none());
    }

    #[test]
    fn pick_release_latest_skips_prereleases_by_default() {
        let releases = vec![
            release("agent-v0.3.0-rc.27", true, &[]),
            release("agent-v0.2.6", false, &[]),
        ];
        let picked = pick_release(&releases, "latest").unwrap();
        assert_eq!(picked.tag_name, "agent-v0.2.6");
    }

    #[test]
    fn pick_release_latest_falls_back_to_prerelease_when_no_stable() {
        let releases = vec![release("agent-v0.3.0-rc.27", true, &[])];
        let picked = pick_release(&releases, "latest").unwrap();
        assert_eq!(picked.tag_name, "agent-v0.3.0-rc.27");
    }

    #[test]
    fn pick_release_explicit_tag_with_prefix() {
        let releases = vec![release("agent-v0.3.0-rc.27", true, &[])];
        let picked = pick_release(&releases, "agent-v0.3.0-rc.27").unwrap();
        assert_eq!(picked.tag_name, "agent-v0.3.0-rc.27");
    }

    #[test]
    fn pick_release_explicit_tag_without_prefix() {
        let releases = vec![release("agent-v0.3.0-rc.27", true, &[])];
        let picked = pick_release(&releases, "0.3.0-rc.27").unwrap();
        assert_eq!(picked.tag_name, "agent-v0.3.0-rc.27");
    }

    #[test]
    fn pick_release_returns_none_for_unknown_tag() {
        let releases = vec![release("agent-v0.3.0-rc.27", true, &[])];
        assert!(pick_release(&releases, "agent-v9.9.9").is_none());
    }

    #[test]
    fn sanitise_header_value_strips_crlf_and_quotes() {
        assert_eq!(
            sanitise_header_value("evil\r\n\"injection\".msi"),
            "evilinjection.msi"
        );
    }
}

async fn fetch_releases() -> anyhow::Result<Vec<AgentRelease>> {
    let url = format!(
        "https://api.github.com/repos/{RELEASES_REPO}/releases?per_page={RELEASES_PER_PAGE}"
    );
    let client = reqwest::Client::builder()
        .user_agent(concat!("roomler-ai-api/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("GitHub returned {}", resp.status());
    }
    let releases: Vec<AgentRelease> = resp.json().await?;
    Ok(releases)
}

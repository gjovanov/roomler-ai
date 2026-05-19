//! `/api/tunnel/{latest-release,installer/{platform}}` — cached
//! GitHub-Releases proxy for the `roomler-tunnel` binary. Mirrors
//! [`routes::agent_release`] but targets `tunnel-v*` tags and
//! platform-keyed archives instead of perUser/perMachine MSI flavours.
//!
//! Why proxy: same reasoning as the agent side. Operators behind
//! corporate proxies can download a single archive via `roomler.ai`
//! that the AV layer trusts, without pinning to a specific GitHub IP
//! or hitting the 60/hr unauthenticated REST quota.

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode},
    response::Response,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::{
    error::ApiError,
    routes::agent_release::{AgentRelease, AgentReleaseAsset},
    state::AppState,
};

/// GitHub repo slug — same repo as the agent.
const RELEASES_REPO: &str = "gjovanov/roomler-ai";

/// Cache TTL — 1 hour. Operators download once during install; spike
/// happens on each tunnel-v* tag. One upstream fetch per hour is
/// plenty.
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

const RELEASES_PER_PAGE: usize = 30;

struct CacheEntry {
    fetched_at: Instant,
    payload: Vec<AgentRelease>,
}

pub struct LatestTunnelReleaseCache {
    inner: RwLock<Option<CacheEntry>>,
}

impl LatestTunnelReleaseCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(None),
        })
    }
}

#[derive(Deserialize)]
pub struct InstallerQuery {
    #[serde(default = "default_version_latest")]
    pub version: String,
}

fn default_version_latest() -> String {
    "latest".to_string()
}

#[derive(Debug, Serialize)]
pub struct InstallerHealth {
    pub tag: String,
    pub platform: String,
    pub filename: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub uri: String,
}

/// `GET /api/tunnel/latest-release` — same wire shape as the agent's
/// `latest-release` so the CLI / installer logic can reuse one parser.
pub async fn latest_release(
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentRelease>>, ApiError> {
    let releases = ensure_releases_cached(&state).await?;
    Ok(Json(releases))
}

/// `GET /api/tunnel/installer/{platform}/health` — manifest (tag,
/// filename, size, digest, download URI) for the asset matching the
/// requested platform + version.
pub async fn installer_health(
    State(state): State<AppState>,
    Path(platform): Path<String>,
    Query(params): Query<InstallerQuery>,
) -> Result<Json<InstallerHealth>, ApiError> {
    let normalised = normalise_platform(&platform)?;
    let releases = ensure_releases_cached(&state).await?;
    let release = pick_release(&releases, &params.version).ok_or_else(|| {
        ApiError::NotFound(format!("no release matching version={}", params.version))
    })?;
    let asset = pick_installer_asset(&release.assets, normalised).ok_or_else(|| {
        ApiError::NotFound(format!(
            "no asset for platform {} in tag {}",
            normalised, release.tag_name
        ))
    })?;
    Ok(Json(InstallerHealth {
        tag: release.tag_name.clone(),
        platform: normalised.to_string(),
        filename: asset.name.clone(),
        size: asset.size,
        digest: asset.digest.clone(),
        uri: format!(
            "/api/tunnel/installer/{}?version={}",
            normalised, params.version
        ),
    }))
}

/// `GET /api/tunnel/installer/{platform}` — streams the archive
/// bytes. Content-Type derived from filename suffix.
pub async fn installer_proxy(
    State(state): State<AppState>,
    Path(platform): Path<String>,
    Query(params): Query<InstallerQuery>,
) -> Result<Response, ApiError> {
    let normalised = normalise_platform(&platform)?;
    let releases = ensure_releases_cached(&state).await?;
    let release = pick_release(&releases, &params.version).ok_or_else(|| {
        ApiError::NotFound(format!("no release matching version={}", params.version))
    })?;
    let asset = pick_installer_asset(&release.assets, normalised).ok_or_else(|| {
        ApiError::NotFound(format!(
            "no asset for platform {} in tag {}",
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
        .map_err(|e| ApiError::Internal(format!("upstream archive fetch failed: {e}")))?;

    let status = upstream.status();
    if !status.is_success() {
        return Err(ApiError::Internal(format!(
            "upstream archive fetch returned {}",
            status
        )));
    }
    let content_length = upstream.content_length();
    let content_type = content_type_for(&asset.name);

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", content_type)
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

fn sanitise_header_value(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\r' | '\n' | '"'))
        .collect()
}

fn content_type_for(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        "application/gzip"
    } else if lower.ends_with(".deb") {
        "application/vnd.debian.binary-package"
    } else if lower.ends_with(".zip") {
        "application/zip"
    } else {
        "application/octet-stream"
    }
}

fn normalise_platform(s: &str) -> Result<&'static str, ApiError> {
    match s.to_ascii_lowercase().as_str() {
        "linux-x86_64" | "linux" | "linux-tar" => Ok("linux-x86_64"),
        "linux-deb" | "deb" => Ok("linux-deb"),
        "macos" | "macos-universal" | "darwin" => Ok("macos"),
        "windows-x86_64" | "windows" | "win" => Ok("windows-x86_64"),
        other => Err(ApiError::BadRequest(format!(
            "unknown platform {other:?}; expected one of linux-x86_64 / linux-deb / macos / windows-x86_64"
        ))),
    }
}

fn pick_release<'a>(releases: &'a [AgentRelease], version: &str) -> Option<&'a AgentRelease> {
    if version == "latest" {
        releases
            .iter()
            .find(|r| !r.draft && !r.prerelease && r.tag_name.starts_with("tunnel-v"))
            .or_else(|| {
                releases
                    .iter()
                    .find(|r| !r.draft && r.tag_name.starts_with("tunnel-v"))
            })
    } else {
        let target_with_prefix = format!("tunnel-v{}", version.trim_start_matches("tunnel-v"));
        let target_bare = version.trim_start_matches("tunnel-v");
        releases.iter().find(|r| {
            r.tag_name == target_with_prefix || r.tag_name == target_bare || r.tag_name == version
        })
    }
}

/// Pick the asset matching the requested platform. Filename
/// conventions follow `release-tunnel.yml`:
///   - linux-x86_64 → `*-x86_64-unknown-linux-gnu.tar.gz`
///   - linux-deb    → `*-x86_64-unknown-linux-gnu.deb`
///   - macos        → `*-universal-apple-darwin.tar.gz`
///   - windows-x86_64 → `*-x86_64-pc-windows-msvc.zip`
pub fn pick_installer_asset<'a>(
    assets: &'a [AgentReleaseAsset],
    platform: &str,
) -> Option<&'a AgentReleaseAsset> {
    assets.iter().find(|a| {
        let name = a.name.to_ascii_lowercase();
        // Exclude sha256 sidecars.
        if name.ends_with(".sha256") {
            return false;
        }
        match platform {
            "linux-x86_64" => {
                name.contains("x86_64-unknown-linux-gnu") && name.ends_with(".tar.gz")
            }
            "linux-deb" => name.contains("x86_64-unknown-linux-gnu") && name.ends_with(".deb"),
            "macos" => name.contains("universal-apple-darwin") && name.ends_with(".tar.gz"),
            "windows-x86_64" => name.contains("x86_64-pc-windows-msvc") && name.ends_with(".zip"),
            _ => false,
        }
    })
}

async fn ensure_releases_cached(state: &AppState) -> Result<Vec<AgentRelease>, ApiError> {
    let cache = state.tunnel_release_cache.clone();
    {
        let g = cache.inner.read().await;
        if let Some(entry) = g.as_ref()
            && entry.fetched_at.elapsed() < CACHE_TTL
        {
            return Ok(entry.payload.clone());
        }
    }
    let url = format!(
        "https://api.github.com/repos/{}/releases?per_page={}",
        RELEASES_REPO, RELEASES_PER_PAGE
    );
    let client = reqwest::Client::builder()
        .user_agent(concat!("roomler-ai-api/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| ApiError::Internal(format!("reqwest client build: {e}")))?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await;
    let releases = match resp {
        Ok(r) if r.status().is_success() => r
            .json::<Vec<AgentRelease>>()
            .await
            .map_err(|e| ApiError::Internal(format!("github releases parse: {e}")))?,
        Ok(r) => {
            // Stale-on-error: if we have a prior cache entry, return
            // it. Otherwise propagate.
            let g = cache.inner.read().await;
            if let Some(entry) = g.as_ref() {
                tracing::warn!(
                    status = %r.status(),
                    "tunnel releases upstream fetch returned non-success; serving stale cache"
                );
                return Ok(entry.payload.clone());
            }
            return Err(ApiError::Internal(format!(
                "tunnel releases upstream returned {}",
                r.status()
            )));
        }
        Err(e) => {
            let g = cache.inner.read().await;
            if let Some(entry) = g.as_ref() {
                tracing::warn!(%e, "tunnel releases upstream fetch errored; serving stale cache");
                return Ok(entry.payload.clone());
            }
            return Err(ApiError::Internal(format!(
                "tunnel releases upstream fetch failed: {e}"
            )));
        }
    };
    {
        let mut g = cache.inner.write().await;
        *g = Some(CacheEntry {
            fetched_at: Instant::now(),
            payload: releases.clone(),
        });
    }
    Ok(releases)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> AgentReleaseAsset {
        AgentReleaseAsset {
            name: name.to_string(),
            browser_download_url: format!("https://example.com/{name}"),
            size: 1024,
            digest: None,
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
    fn normalise_platform_accepts_aliases() {
        assert_eq!(normalise_platform("linux").unwrap(), "linux-x86_64");
        assert_eq!(normalise_platform("LINUX-X86_64").unwrap(), "linux-x86_64");
        assert_eq!(normalise_platform("deb").unwrap(), "linux-deb");
        assert_eq!(normalise_platform("Darwin").unwrap(), "macos");
        assert_eq!(normalise_platform("WIN").unwrap(), "windows-x86_64");
    }

    #[test]
    fn normalise_platform_rejects_unknown() {
        assert!(normalise_platform("freebsd").is_err());
        assert!(normalise_platform("").is_err());
    }

    #[test]
    fn pick_installer_asset_matches_by_triple_and_suffix() {
        let assets = vec![
            asset("roomler-tunnel-0.3.0-x86_64-unknown-linux-gnu.tar.gz"),
            asset("roomler-tunnel-0.3.0-x86_64-unknown-linux-gnu.tar.gz.sha256"),
            asset("roomler-tunnel-0.3.0-x86_64-unknown-linux-gnu.deb"),
            asset("roomler-tunnel-0.3.0-universal-apple-darwin.tar.gz"),
            asset("roomler-tunnel-0.3.0-x86_64-pc-windows-msvc.zip"),
        ];
        assert_eq!(
            pick_installer_asset(&assets, "linux-x86_64").unwrap().name,
            "roomler-tunnel-0.3.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            pick_installer_asset(&assets, "linux-deb").unwrap().name,
            "roomler-tunnel-0.3.0-x86_64-unknown-linux-gnu.deb"
        );
        assert_eq!(
            pick_installer_asset(&assets, "macos").unwrap().name,
            "roomler-tunnel-0.3.0-universal-apple-darwin.tar.gz"
        );
        assert_eq!(
            pick_installer_asset(&assets, "windows-x86_64")
                .unwrap()
                .name,
            "roomler-tunnel-0.3.0-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn pick_installer_asset_skips_sha256_sidecars() {
        let assets = vec![asset(
            "roomler-tunnel-0.3.0-x86_64-unknown-linux-gnu.tar.gz.sha256",
        )];
        assert!(pick_installer_asset(&assets, "linux-x86_64").is_none());
    }

    #[test]
    fn pick_release_latest_filters_to_tunnel_tags() {
        // Mixed releases — pick_release(version=latest) should ignore
        // agent-v* tags and prefer non-prerelease tunnel-v*.
        let releases = vec![
            release("agent-v0.3.0-rc.46", true, &[]),
            release(
                "tunnel-v0.3.0-rc.5",
                true,
                &["roomler-tunnel-0.3.0-rc.5-x86_64-pc-windows-msvc.zip"],
            ),
            release(
                "tunnel-v0.3.0",
                false,
                &["roomler-tunnel-0.3.0-x86_64-pc-windows-msvc.zip"],
            ),
            release("agent-v0.3.0", false, &[]),
        ];
        let picked = pick_release(&releases, "latest").unwrap();
        assert_eq!(picked.tag_name, "tunnel-v0.3.0");
    }

    #[test]
    fn pick_release_latest_falls_back_to_prerelease_when_no_stable() {
        let releases = vec![
            release("tunnel-v0.3.0-rc.5", true, &[]),
            release("agent-v0.3.0", false, &[]),
        ];
        let picked = pick_release(&releases, "latest").unwrap();
        assert_eq!(picked.tag_name, "tunnel-v0.3.0-rc.5");
    }

    #[test]
    fn pick_release_specific_version_accepts_with_or_without_prefix() {
        let releases = vec![release("tunnel-v0.3.0", false, &[])];
        assert!(pick_release(&releases, "0.3.0").is_some());
        assert!(pick_release(&releases, "tunnel-v0.3.0").is_some());
        assert!(pick_release(&releases, "0.99.0").is_none());
    }
}

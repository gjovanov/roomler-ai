//! `/api/tunnel-wizard/{latest-release,installer/{platform}}` —
//! cached GitHub-Releases proxy for the `roomler-tunnel-installer`
//! Tauri 2 wizard EXE. Parallel to [`crate::routes::tunnel_release`]
//! (which serves the bare CLI tarball); this endpoint family serves
//! the wizard that downloads + installs the CLI.
//!
//! Two endpoints downstream of the GitHub-Releases cache:
//!   - `GET /api/tunnel-wizard/latest-release` → JSON list of recent
//!     `tunnel-wizard-v*` tags (debug + diagnostics).
//!   - `GET /api/tunnel-wizard/{platform}/health` → manifest (tag,
//!     filename, size, digest, uri) for the platform's wizard EXE.
//!   - `GET /api/tunnel-wizard/{platform}` → streams the wizard
//!     archive bytes.
//!
//! Tag prefix: `tunnel-wizard-v*` — separate from `tunnel-v*` so the
//! wizard can iterate independently of the CLI. Asset name filter
//! matches `roomler-tunnel-installer-*` per the release-tunnel.yml
//! wizard matrix.

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

/// GitHub repo slug — same repo as agent + tunnel CLI.
const RELEASES_REPO: &str = "gjovanov/roomler-ai";

/// Cache TTL — 1 hour. Mirrors the CLI / agent caches. Operators
/// download once during install; the wizard release cadence is
/// per-tag (rare).
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

const RELEASES_PER_PAGE: usize = 30;

struct CacheEntry {
    fetched_at: Instant,
    payload: Vec<AgentRelease>,
}

pub struct LatestTunnelWizardReleaseCache {
    inner: RwLock<Option<CacheEntry>>,
}

impl LatestTunnelWizardReleaseCache {
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

/// `GET /api/tunnel-wizard/latest-release` — same wire shape as the
/// CLI / agent latest-release endpoints. Returns ALL recent releases
/// (mixed tag families) so a caller can grep for `tunnel-wizard-v*`
/// themselves if needed.
pub async fn latest_release(
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentRelease>>, ApiError> {
    let releases = ensure_releases_cached(&state).await?;
    Ok(Json(releases))
}

/// `GET /api/tunnel-wizard/{platform}/health` — manifest for the
/// wizard EXE matching the requested platform + version.
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
    let asset = pick_wizard_asset(&release.assets, normalised).ok_or_else(|| {
        ApiError::NotFound(format!(
            "no wizard asset for platform {} in tag {}",
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
            "/api/tunnel-wizard/{}?version={}",
            normalised, params.version
        ),
    }))
}

/// `GET /api/tunnel-wizard/{platform}` — streams the wizard archive
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
    let asset = pick_wizard_asset(&release.assets, normalised).ok_or_else(|| {
        ApiError::NotFound(format!(
            "no wizard asset for platform {} in tag {}",
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
        .map_err(|e| ApiError::Internal(format!("upstream wizard fetch failed: {e}")))?;

    let status = upstream.status();
    if !status.is_success() {
        return Err(ApiError::Internal(format!(
            "upstream wizard fetch returned {status}"
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
    } else if lower.ends_with(".zip") {
        "application/zip"
    } else if lower.ends_with(".exe") {
        "application/vnd.microsoft.portable-executable"
    } else {
        "application/octet-stream"
    }
}

fn normalise_platform(s: &str) -> Result<&'static str, ApiError> {
    match s.to_ascii_lowercase().as_str() {
        "linux-x86_64" | "linux" | "linux-tar" => Ok("linux-x86_64"),
        "macos" | "macos-universal" | "darwin" => Ok("macos"),
        "windows-x86_64" | "windows" | "win" => Ok("windows-x86_64"),
        other => Err(ApiError::BadRequest(format!(
            "unknown platform {other:?}; expected one of linux-x86_64 / macos / windows-x86_64"
        ))),
    }
}

fn pick_release<'a>(releases: &'a [AgentRelease], version: &str) -> Option<&'a AgentRelease> {
    if version == "latest" {
        releases
            .iter()
            .find(|r| !r.draft && !r.prerelease && r.tag_name.starts_with("tunnel-wizard-v"))
            .or_else(|| {
                releases
                    .iter()
                    .find(|r| !r.draft && r.tag_name.starts_with("tunnel-wizard-v"))
            })
    } else {
        let target_with_prefix = format!(
            "tunnel-wizard-v{}",
            version.trim_start_matches("tunnel-wizard-v")
        );
        let target_bare = version.trim_start_matches("tunnel-wizard-v");
        releases.iter().find(|r| {
            r.tag_name == target_with_prefix || r.tag_name == target_bare || r.tag_name == version
        })
    }
}

/// Pick the wizard asset matching the requested platform. Asset
/// naming conventions follow `release-tunnel.yml`'s wizard matrix:
///   - linux-x86_64 — `roomler-tunnel-installer-*-x86_64-unknown-linux-gnu.tar.gz`
///   - macos — `roomler-tunnel-installer-*-universal-apple-darwin.tar.gz`
///   - windows-x86_64 — `roomler-tunnel-installer-*-x86_64-pc-windows-msvc.zip`
///     (with `roomler-tunnel-installer.exe` inside).
///
/// The wizard's filename always starts with `roomler-tunnel-installer-`
/// so the matcher can disambiguate from the CLI tarball, which uses
/// `roomler-tunnel-` (no `installer-` segment).
pub fn pick_wizard_asset<'a>(
    assets: &'a [AgentReleaseAsset],
    platform: &str,
) -> Option<&'a AgentReleaseAsset> {
    assets.iter().find(|a| {
        let name = a.name.to_ascii_lowercase();
        if name.ends_with(".sha256") {
            return false;
        }
        if !name.starts_with("roomler-tunnel-installer-") {
            return false;
        }
        match platform {
            "linux-x86_64" => {
                name.contains("x86_64-unknown-linux-gnu") && name.ends_with(".tar.gz")
            }
            "macos" => name.contains("universal-apple-darwin") && name.ends_with(".tar.gz"),
            "windows-x86_64" => name.contains("x86_64-pc-windows-msvc") && name.ends_with(".zip"),
            _ => false,
        }
    })
}

async fn ensure_releases_cached(state: &AppState) -> Result<Vec<AgentRelease>, ApiError> {
    let cache = state.tunnel_wizard_release_cache.clone();
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
            let g = cache.inner.read().await;
            if let Some(entry) = g.as_ref() {
                tracing::warn!(
                    status = %r.status(),
                    "tunnel-wizard releases upstream returned non-success; serving stale cache"
                );
                return Ok(entry.payload.clone());
            }
            return Err(ApiError::Internal(format!(
                "tunnel-wizard releases upstream returned {}",
                r.status()
            )));
        }
        Err(e) => {
            let g = cache.inner.read().await;
            if let Some(entry) = g.as_ref() {
                tracing::warn!(
                    %e,
                    "tunnel-wizard releases upstream fetch errored; serving stale cache"
                );
                return Ok(entry.payload.clone());
            }
            return Err(ApiError::Internal(format!(
                "tunnel-wizard releases upstream fetch failed: {e}"
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
        assert_eq!(normalise_platform("Darwin").unwrap(), "macos");
        assert_eq!(normalise_platform("WIN").unwrap(), "windows-x86_64");
    }

    #[test]
    fn normalise_platform_rejects_deb_for_wizard() {
        // The wizard EXE isn't packaged as a .deb (operators run it
        // interactively, not via dpkg). Reject the alias outright so
        // a confused caller gets a clear error instead of a 404 on
        // pick_wizard_asset.
        assert!(normalise_platform("linux-deb").is_err());
        assert!(normalise_platform("deb").is_err());
    }

    #[test]
    fn normalise_platform_rejects_unknown() {
        assert!(normalise_platform("freebsd").is_err());
        assert!(normalise_platform("").is_err());
    }

    #[test]
    fn pick_wizard_asset_requires_installer_prefix() {
        // CLI tarball matches the triple but lacks the `installer-`
        // segment; wizard pick must refuse it so a misconfigured CI
        // matrix can't accidentally serve the wrong binary as the
        // wizard.
        let assets = vec![
            asset("roomler-tunnel-0.3.0-x86_64-pc-windows-msvc.zip"),
            asset("roomler-tunnel-installer-0.3.0-x86_64-pc-windows-msvc.zip"),
        ];
        let picked = pick_wizard_asset(&assets, "windows-x86_64").unwrap();
        assert!(picked.name.contains("installer"));
    }

    #[test]
    fn pick_wizard_asset_matches_by_triple_and_suffix() {
        let assets = vec![
            asset("roomler-tunnel-installer-0.3.0-x86_64-unknown-linux-gnu.tar.gz"),
            asset("roomler-tunnel-installer-0.3.0-x86_64-unknown-linux-gnu.tar.gz.sha256"),
            asset("roomler-tunnel-installer-0.3.0-universal-apple-darwin.tar.gz"),
            asset("roomler-tunnel-installer-0.3.0-x86_64-pc-windows-msvc.zip"),
        ];
        assert_eq!(
            pick_wizard_asset(&assets, "linux-x86_64").unwrap().name,
            "roomler-tunnel-installer-0.3.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            pick_wizard_asset(&assets, "macos").unwrap().name,
            "roomler-tunnel-installer-0.3.0-universal-apple-darwin.tar.gz"
        );
        assert_eq!(
            pick_wizard_asset(&assets, "windows-x86_64").unwrap().name,
            "roomler-tunnel-installer-0.3.0-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn pick_wizard_asset_skips_sha256_sidecars() {
        let assets = vec![asset(
            "roomler-tunnel-installer-0.3.0-x86_64-pc-windows-msvc.zip.sha256",
        )];
        assert!(pick_wizard_asset(&assets, "windows-x86_64").is_none());
    }

    #[test]
    fn pick_release_latest_filters_to_tunnel_wizard_tags() {
        let releases = vec![
            release("agent-v0.3.0-rc.46", true, &[]),
            release("tunnel-v0.3.0-rc.46", false, &[]),
            release(
                "tunnel-wizard-v0.3.0-rc.1",
                false,
                &["roomler-tunnel-installer-0.3.0-x86_64-pc-windows-msvc.zip"],
            ),
            release("tunnel-wizard-v0.3.0-rc.2-pre", true, &[]),
        ];
        let picked = pick_release(&releases, "latest").unwrap();
        assert_eq!(picked.tag_name, "tunnel-wizard-v0.3.0-rc.1");
    }

    #[test]
    fn pick_release_latest_falls_back_to_prerelease_when_no_stable() {
        let releases = vec![
            release("tunnel-wizard-v0.3.0-rc.1", true, &[]),
            release("agent-v0.3.0", false, &[]),
        ];
        let picked = pick_release(&releases, "latest").unwrap();
        assert_eq!(picked.tag_name, "tunnel-wizard-v0.3.0-rc.1");
    }

    #[test]
    fn pick_release_specific_version_accepts_with_or_without_prefix() {
        let releases = vec![release("tunnel-wizard-v0.3.0-rc.1", false, &[])];
        assert!(pick_release(&releases, "0.3.0-rc.1").is_some());
        assert!(pick_release(&releases, "tunnel-wizard-v0.3.0-rc.1").is_some());
        assert!(pick_release(&releases, "0.99.0").is_none());
    }

    #[test]
    fn content_type_for_known_extensions() {
        assert_eq!(content_type_for("foo.zip"), "application/zip");
        assert_eq!(content_type_for("foo.tar.gz"), "application/gzip");
        assert_eq!(content_type_for("foo.tgz"), "application/gzip");
        assert_eq!(
            content_type_for("foo.exe"),
            "application/vnd.microsoft.portable-executable"
        );
        assert_eq!(content_type_for("foo.bin"), "application/octet-stream");
    }
}

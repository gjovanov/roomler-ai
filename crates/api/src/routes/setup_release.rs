//! `/api/setup/*` — cached GitHub-Releases proxy for the unified
//! `roomler-setup` wizard (tag `setup-v*`, asset prefix
//! `roomler-setup-`) plus the terminal installers
//! `/api/setup/install.{sh,ps1}`. Renamed from
//! `tunnel_wizard_release.rs` in P4b; the legacy `/api/tunnel-wizard/*`
//! family it also served was retired in P4c-2 (its release family
//! stopped at tunnel-wizard-v0.3.0-rc.59 and had been 404ing off the
//! release-page window for months).
//!
//! `GET /api/setup/{latest-release,{platform}/health,{platform}}` —
//! latest-release returns the `setup-v*`-filtered list; health
//! resolves a platform asset manifest; the bare platform route
//! streams the artifact bytes.
//!
//! Parallel to [`crate::routes::tunnel_release`] (bare CLI tarball)
//! and [`crate::routes::agent_release`] (agent MSIs).

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

/// GitHub page size. 100 (the API max) rather than the old 30:
/// `agent-v*` tags ship several times a day, so the rarely-tagged
/// `setup-v*` family falls off a 30-deep first page within days
/// (observed with the retired tunnel-wizard family, whose last tag
/// was long gone from the top 30 within weeks). One page of 100
/// buys weeks of agent cadence; a per-family fetch is the follow-up
/// if that ever becomes insufficient.
const RELEASES_PER_PAGE: usize = 100;

struct CacheEntry {
    fetched_at: Instant,
    payload: Vec<AgentRelease>,
}

pub struct LatestSetupReleaseCache {
    inner: RwLock<Option<CacheEntry>>,
}

impl LatestSetupReleaseCache {
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

// ─── /api/setup — the unified roomler-setup wizard ──────────────────────────

/// `GET /api/setup/latest-release` — recent `setup-v*` releases ONLY
/// (filtered server-side from the mixed tag list the cache holds).
pub async fn setup_latest_release(
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentRelease>>, ApiError> {
    let releases = ensure_releases_cached(&state).await?;
    let filtered: Vec<AgentRelease> = releases
        .into_iter()
        .filter(|r| r.tag_name.starts_with("setup-v"))
        .collect();
    Ok(Json(filtered))
}

/// `GET /api/setup/{platform}/health` — manifest for the unified
/// wizard EXE matching the requested platform + version. 404s until
/// P4c tags the first `setup-v*` release.
pub async fn setup_installer_health(
    State(state): State<AppState>,
    Path(platform): Path<String>,
    Query(params): Query<InstallerQuery>,
) -> Result<Json<InstallerHealth>, ApiError> {
    let normalised = normalise_platform(&platform)?;
    let releases = ensure_releases_cached(&state).await?;
    let release = pick_release_for(&releases, &params.version, "setup-v").ok_or_else(|| {
        ApiError::NotFound(format!(
            "no setup release matching version={}",
            params.version
        ))
    })?;
    let asset = pick_setup_asset(&release.assets, normalised).ok_or_else(|| {
        ApiError::NotFound(format!(
            "no setup asset for platform {} in tag {}",
            normalised, release.tag_name
        ))
    })?;
    Ok(Json(InstallerHealth {
        tag: release.tag_name.clone(),
        platform: normalised.to_string(),
        filename: asset.name.clone(),
        size: asset.size,
        digest: asset.digest.clone(),
        uri: format!("/api/setup/{}?version={}", normalised, params.version),
    }))
}

/// `GET /api/setup/{platform}` — streams the unified wizard bytes.
pub async fn setup_installer_proxy(
    State(state): State<AppState>,
    Path(platform): Path<String>,
    Query(params): Query<InstallerQuery>,
) -> Result<Response, ApiError> {
    let normalised = normalise_platform(&platform)?;
    let releases = ensure_releases_cached(&state).await?;
    let release = pick_release_for(&releases, &params.version, "setup-v").ok_or_else(|| {
        ApiError::NotFound(format!(
            "no setup release matching version={}",
            params.version
        ))
    })?;
    let asset = pick_setup_asset(&release.assets, normalised).ok_or_else(|| {
        ApiError::NotFound(format!(
            "no setup asset for platform {} in tag {}",
            normalised, release.tag_name
        ))
    })?;

    stream_asset(asset).await
}

/// Pick the unified-wizard asset for a platform. Asset naming follows
/// the release-setup.yml matrix with the `roomler-setup-` prefix:
///   - linux-x86_64 — `roomler-setup-*-x86_64-unknown-linux-gnu.tar.gz`
///   - macos — `roomler-setup-*-universal-apple-darwin.tar.gz`
///   - windows-x86_64 — `roomler-setup-*-x86_64-pc-windows-msvc.zip`
pub fn pick_setup_asset<'a>(
    assets: &'a [AgentReleaseAsset],
    platform: &str,
) -> Option<&'a AgentReleaseAsset> {
    assets.iter().find(|a| {
        let name = a.name.to_ascii_lowercase();
        if name.ends_with(".sha256") {
            return false;
        }
        if !name.starts_with("roomler-setup-") {
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

/// The terminal-driven installers (scripts/install.sh + install.ps1),
/// embedded at COMPILE time so the API serves them with no runtime
/// filesystem dependency. Canonical usage:
///
///   curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
///       --role daemon --token <jwt>
///
/// and the PowerShell twin via `/api/setup/install.ps1`. These are the
/// no-GUI equivalent of the roomler-setup wizard (same resolve →
/// download → verify → install → enroll steps). Short cache so script
/// fixes roll out within minutes of a web deploy.
const INSTALL_SH: &str = include_str!("../../../../scripts/install.sh");
const INSTALL_PS1: &str = include_str!("../../../../scripts/install.ps1");

fn script_response(body: &'static str, content_type: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", content_type)
        .header("Cache-Control", "public, max-age=300")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::from(body)))
}

/// `GET /api/setup/install.sh` — the Linux/macOS terminal installer.
pub async fn install_script_sh() -> Response {
    script_response(INSTALL_SH, "text/x-shellscript; charset=utf-8")
}

/// `GET /api/setup/install.ps1` — the Windows terminal installer.
pub async fn install_script_ps1() -> Response {
    script_response(INSTALL_PS1, "text/plain; charset=utf-8")
}

/// Stream a release asset's bytes through the proxy with download
/// headers.
async fn stream_asset(asset: &AgentReleaseAsset) -> Result<Response, ApiError> {
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

/// Pick a release for a given tag family. `version == "latest"`
/// prefers stable over prerelease within the family; an explicit
/// version matches with or without the family prefix.
fn pick_release_for<'a>(
    releases: &'a [AgentRelease],
    version: &str,
    tag_prefix: &str,
) -> Option<&'a AgentRelease> {
    if version == "latest" {
        releases
            .iter()
            .find(|r| !r.draft && !r.prerelease && r.tag_name.starts_with(tag_prefix))
            .or_else(|| {
                releases
                    .iter()
                    .find(|r| !r.draft && r.tag_name.starts_with(tag_prefix))
            })
    } else {
        let target_with_prefix = format!("{tag_prefix}{}", version.trim_start_matches(tag_prefix));
        let target_bare = version.trim_start_matches(tag_prefix);
        releases.iter().find(|r| {
            r.tag_name == target_with_prefix || r.tag_name == target_bare || r.tag_name == version
        })
    }
}

async fn ensure_releases_cached(state: &AppState) -> Result<Vec<AgentRelease>, ApiError> {
    let cache = state.setup_release_cache.clone();
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
                    "setup releases upstream returned non-success; serving stale cache"
                );
                return Ok(entry.payload.clone());
            }
            return Err(ApiError::Internal(format!(
                "setup releases upstream returned {}",
                r.status()
            )));
        }
        Err(e) => {
            let g = cache.inner.read().await;
            if let Some(entry) = g.as_ref() {
                tracing::warn!(
                    %e,
                    "setup releases upstream fetch errored; serving stale cache"
                );
                return Ok(entry.payload.clone());
            }
            return Err(ApiError::Internal(format!(
                "setup releases upstream fetch failed: {e}"
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
        // pick_setup_asset.
        assert!(normalise_platform("linux-deb").is_err());
        assert!(normalise_platform("deb").is_err());
    }

    #[test]
    fn normalise_platform_rejects_unknown() {
        assert!(normalise_platform("freebsd").is_err());
        assert!(normalise_platform("").is_err());
    }

    #[test]
    fn pick_release_latest_falls_back_to_prerelease_when_no_stable() {
        let releases = vec![
            release("setup-v0.3.0-rc.1", true, &[]),
            release("agent-v0.3.0", false, &[]),
        ];
        let picked = pick_release_for(&releases, "latest", "setup-v").unwrap();
        assert_eq!(picked.tag_name, "setup-v0.3.0-rc.1");
    }

    #[test]
    fn pick_release_specific_version_accepts_with_or_without_prefix() {
        let releases = vec![release("setup-v0.3.0-rc.1", false, &[])];
        assert!(pick_release_for(&releases, "0.3.0-rc.1", "setup-v").is_some());
        assert!(pick_release_for(&releases, "setup-v0.3.0-rc.1", "setup-v").is_some());
        assert!(pick_release_for(&releases, "0.99.0", "setup-v").is_none());
    }

    #[test]
    fn pick_release_for_setup_prefix_ignores_other_families() {
        let releases = vec![
            release("agent-v0.3.0-rc.195", false, &[]),
            release("tunnel-wizard-v0.3.0-rc.59", false, &[]),
            release(
                "setup-v0.3.0-rc.200",
                false,
                &["roomler-setup-0.3.0-rc.200-x86_64-pc-windows-msvc.zip"],
            ),
        ];
        let picked = pick_release_for(&releases, "latest", "setup-v").unwrap();
        assert_eq!(picked.tag_name, "setup-v0.3.0-rc.200");
        // Dark until the family exists: no setup-v tag → None (the
        // handlers turn this into a clean 404).
        let none = pick_release_for(&releases[..2], "latest", "setup-v");
        assert!(none.is_none());
    }

    #[test]
    fn pick_setup_asset_requires_setup_prefix_and_matches_triples() {
        let assets = vec![
            asset("roomler-tunnel-installer-0.3.0-x86_64-pc-windows-msvc.zip"),
            asset("roomler-setup-0.3.0-x86_64-pc-windows-msvc.zip"),
            asset("roomler-setup-0.3.0-x86_64-pc-windows-msvc.zip.sha256"),
            asset("roomler-setup-0.3.0-x86_64-unknown-linux-gnu.tar.gz"),
            asset("roomler-setup-0.3.0-universal-apple-darwin.tar.gz"),
        ];
        assert_eq!(
            pick_setup_asset(&assets, "windows-x86_64").unwrap().name,
            "roomler-setup-0.3.0-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            pick_setup_asset(&assets, "linux-x86_64").unwrap().name,
            "roomler-setup-0.3.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            pick_setup_asset(&assets, "macos").unwrap().name,
            "roomler-setup-0.3.0-universal-apple-darwin.tar.gz"
        );
        // The legacy wizard asset must never satisfy the setup pick.
        let legacy_only = vec![asset(
            "roomler-tunnel-installer-0.3.0-x86_64-pc-windows-msvc.zip",
        )];
        assert!(pick_setup_asset(&legacy_only, "windows-x86_64").is_none());
    }

    #[test]
    fn embedded_install_scripts_look_right() {
        // Shape locks on the compile-time-embedded terminal installers
        // — a broken include path or an emptied script fails here, not
        // in production.
        assert!(INSTALL_SH.starts_with("#!/bin/sh"), "install.sh shebang");
        assert!(INSTALL_SH.contains("/api/agent/latest-release"));
        assert!(INSTALL_SH.contains("--role"));
        assert!(INSTALL_PS1.contains("daemon-user"));
        assert!(INSTALL_PS1.contains("/api/agent/installer/"));
        assert!(INSTALL_PS1.contains("tunnel-client"));
        // The served scripts must never embed a JWT (base64url JWTs
        // start with "eyJ" — the {"typ"/{"alg" header).
        assert!(!INSTALL_SH.contains("eyJ"));
        assert!(!INSTALL_PS1.contains("eyJ"));
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

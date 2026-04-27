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

use axum::{Json, extract::State};
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

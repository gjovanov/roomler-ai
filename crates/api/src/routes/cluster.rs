//! C-6 — `GET /api/cluster/status`: this pod's identity, cluster health
//! and the rehome/fallback counters + live gauges (see
//! `cluster::metrics`). Auth-gated (any valid access token) — it exposes
//! operational counters only, no tenant data. With the tenant-affinity
//! LB, hit a specific pod via its `?tid` pinning or the pod IP directly.

use axum::{Json, extract::State};

use crate::{extractors::auth::AuthUser, state::AppState};

pub async fn status(State(state): State<AppState>, _auth: AuthUser) -> Json<serde_json::Value> {
    Json(crate::cluster::metrics::snapshot(&state).await)
}

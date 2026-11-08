// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! C-6 — `GET /api/cluster/status`: this pod's identity, cluster health
//! and the rehome/fallback counters + live gauges (see
//! `cluster::metrics`). With the tenant-affinity LB, hit a specific pod via
//! its `?tid` pinning or the pod IP directly.
//!
//! Platform admins only, like the rest of the operator surface
//! (`/api/admin/*`): the snapshot names every live meeting room on the pod,
//! from every org (`local.media_rooms_detail`), the pods' ids, which are
//! their host addresses under hostNetwork, and deployment-wide gauges. Anyone
//! else gets the 404 `require_platform_admin` gives, so the route does not
//! say it exists.

use axum::{Json, extract::State};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

pub async fn status(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    roomler_core::guards::require_platform_admin(&state, &auth)?;
    Ok(Json(crate::cluster::metrics::snapshot(&state).await))
}

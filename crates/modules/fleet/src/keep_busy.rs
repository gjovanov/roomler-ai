// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-92 — the org's keep-busy DENY (`docs/fr/FR-92-keep-busy.md`).
//!
//! Keep busy is ON by default (operator decision 2026-10-10): a controller
//! with input can leave a device's pointer moving in a pattern to keep it
//! active. Some orgs ban that — it defeats a screen-lock policy — so an org
//! owner can deny it here.
//!
//! The server never enforces it on a data path (the toggle rides the P2P
//! control channel). It DELIVERS it: a standing `rc:agent.keep_busy_policy`
//! on every connect and, from here, on every change — to this pod's agents
//! directly and to every other pod's over the rc ctrl lane. The device
//! enforces it, strictest of every org it is enrolled in, and persists it.

use axum::Json;
use axum::extract::{Path, State};
use bson::oid::ObjectId;
use roomler_ai_db::models::role::permissions;
use roomler_core::{ApiError, extractors::auth::AuthUser, guards::require_permission};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::FleetState;

fn tenant_of(tenant_id: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(tenant_id).map_err(|_| ApiError::BadRequest("Invalid tenant_id".into()))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OrgKeepBusySettings {
    /// `true` = no device in this org may run keep busy, and any already
    /// running stops. `false` (the default) = allowed.
    pub keep_busy_denied: bool,
}

/// `GET /api/tenant/{tenant_id}/keep-busy-settings` — readable by any
/// `MANAGE_AGENTS` admin, so the device console can say why keep busy is
/// greyed out everywhere.
pub async fn get_org_settings(
    State(state): State<FleetState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<OrgKeepBusySettings>, ApiError> {
    let tid = tenant_of(&tenant_id)?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;
    let tenant = state.tenants.base.find_by_id(tid).await?;
    Ok(Json(OrgKeepBusySettings {
        keep_busy_denied: tenant.settings.keep_busy_denied,
    }))
}

/// `PUT /api/tenant/{tenant_id}/keep-busy-settings` — set the deny.
///
/// `MANAGE_TENANT`, like the exec and SSH switches: it decides for every
/// device in the org at once. The change reaches devices that are online now
/// (this pod directly, other pods over the ctrl lane) and every other device
/// at its next connect.
pub async fn set_org_settings(
    State(state): State<FleetState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<OrgKeepBusySettings>,
) -> Result<Json<OrgKeepBusySettings>, ApiError> {
    let tid = tenant_of(&tenant_id)?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_TENANT,
        "MANAGE_TENANT",
    )
    .await?;
    let tenant = state
        .tenants
        .set_keep_busy_denied(tid, body.keep_busy_denied)
        .await?;
    let denied = tenant.settings.keep_busy_denied;
    let here = state.rc_hub.push_keep_busy_policy(tid, denied);
    crate::ctrl::publish_rc_ctrl(
        &state,
        "keep_busy_policy",
        serde_json::json!({ "tenant_id": tid.to_hex(), "denied": denied }),
    )
    .await;
    warn!(
        tenant = %tenant_id, admin = %auth.user_id, denied, pushed_here = here,
        "keep-busy: org policy changed"
    );
    Ok(Json(OrgKeepBusySettings {
        keep_busy_denied: denied,
    }))
}

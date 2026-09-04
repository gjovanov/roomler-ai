// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Remote-control session routes (get / terminate / audit), TURN credentials
//! and the relay-region listing — the `remote` half of the file that also
//! held the agent routes until FR-69 P5a moved those into the `fleet` module.
//! Bodies unchanged; `remote` takes these over in P6.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use bson::oid::ObjectId;
use roomler_ai_db::models::role::permissions;
use roomler_ai_mod_fleet::agent::fmt_dt;
use roomler_ai_remote_control::{
    models::RemoteSession, permissions::Permissions, turn_creds::ice_servers_for,
};
use roomler_ai_services::dao::base::PaginationParams;
use roomler_core::guards::require_permission;
use serde::Serialize;

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

// ────────────────────────────────────────────────────────────────────────────
// Sessions
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub id: String,
    pub agent_id: String,
    pub tenant_id: String,
    pub controller_user_id: String,
    pub permissions: Permissions,
    pub phase: roomler_ai_remote_control::models::SessionPhase,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

pub async fn get_session(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
) -> Result<Json<SessionResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let sid = ObjectId::parse_str(&session_id)
        .map_err(|_| ApiError::BadRequest("Invalid session_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let session = state.remote_sessions.find_in_tenant(tid, sid).await?;
    Ok(Json(to_session_response(session)))
}

pub async fn terminate_session(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let sid = ObjectId::parse_str(&session_id)
        .map_err(|_| ApiError::BadRequest("Invalid session_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    // P3 security — bare tenant membership no longer suffices: only the
    // session's OWN controller or a member holding REMOTE_CONTROL may
    // force-close it. Party/tenant facts come from the LIVE session when this
    // pod holds it; a session homed on ANOTHER pod (rc sessions are pod-local
    // under S6) can't be inspected here, so non-controllers fall back to the
    // permission check against the ROUTE tenant — the ctrl event below only
    // acts on a hub that actually holds the session, and its tenant is
    // re-checked there.
    let live = state.rc_hub.session_snapshot(sid);
    if let Some((live_tid, _)) = live
        && live_tid != tid
    {
        // The path tenant must own the session it names.
        return Err(ApiError::NotFound("Session not found".to_string()));
    }
    let is_controller = matches!(live, Some((_, controller)) if controller == auth.user_id);
    if !is_controller {
        require_permission(
            &state,
            tid,
            auth.user_id,
            permissions::REMOTE_CONTROL,
            "remote-control",
        )
        .await?;
    }

    // Force-close via Hub. The Hub pushes a Terminate to both peers and audits.
    let terminated_here = state
        .rc_hub
        .terminate(
            sid,
            roomler_ai_remote_control::models::EndReason::AdminTerminated,
        )
        .is_ok();
    // P3 — rc sessions are pod-local (S6): when the session is homed on a
    // different pod the local terminate is a no-op that previously STILL
    // returned `{"terminated": true}`. Broadcast an idempotent ctrl event so
    // the owning pod applies it; `terminated` now reports only what THIS
    // request could verify locally.
    crate::ws::remote_control::publish_rc_ctrl(
        &state,
        "terminate",
        serde_json::json!({
            "session_id": sid.to_hex(),
            "tenant_id": tid.to_hex(),
        }),
    )
    .await;
    Ok(Json(serde_json::json!({
        "terminated": terminated_here && live.is_some(),
        "broadcast": true,
    })))
}

#[derive(Debug, Serialize)]
pub struct AuditListResponse {
    pub items: Vec<RemoteAuditEvent>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

pub async fn session_audit(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<AuditListResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let sid = ObjectId::parse_str(&session_id)
        .map_err(|_| ApiError::BadRequest("Invalid session_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::VIEW_REMOTE_AUDIT,
        "VIEW_REMOTE_AUDIT",
    )
    .await?;

    // Ensure the session actually belongs to this tenant.
    let _ = state.remote_sessions.find_in_tenant(tid, sid).await?;

    let page = state.remote_audit.list_for_session(sid, &params).await?;
    Ok(Json(AuditListResponse {
        items: page.items,
        total: page.total,
        page: page.page,
        per_page: page.per_page,
        total_pages: page.total_pages,
    }))
}

// ────────────────────────────────────────────────────────────────────────────
// TURN credentials
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TurnCredentialsResponse {
    pub ice_servers: Vec<IceServer>,
}

#[derive(Debug, Serialize)]
pub struct RelayRegionsResponse {
    pub regions_enabled: bool,
    pub regions: Vec<RelayRegionSummary>,
}

#[derive(Debug, Serialize)]
pub struct RelayRegionSummary {
    pub id: String,
    pub turn_url: String,
    pub derp_url: Option<String>,
    pub enabled: bool,
    /// P6b — the poller's latest load snapshot (`None` = never sampled).
    pub load: Option<roomler_ai_remote_control::turn_creds::RegionLoad>,
    /// P6b — currently steered around by the load-aware pick (fresh-busy).
    pub busy: bool,
}

/// GET /api/relay/regions — the configured relay PoP topology (region ids +
/// endpoints; never secrets). Authed users only; the same hostnames every
/// TURN grant already exposes to clients.
pub async fn relay_regions(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Result<Json<RelayRegionsResponse>, ApiError> {
    let regions = state
        .turn_map
        .specs
        .iter()
        .map(|s| RelayRegionSummary {
            load: state.relay_load.get(&s.id).map(|l| l.value().clone()),
            busy: roomler_ai_remote_control::turn_creds::region_busy(&state.relay_load, &s.id),
            id: s.id.clone(),
            turn_url: s.turn_url.clone(),
            derp_url: s.derp_url.clone(),
            enabled: s.enabled,
        })
        .collect();
    Ok(Json(RelayRegionsResponse {
        regions_enabled: state.turn_map.enabled,
        regions,
    }))
}

/// GET /api/turn/credentials — user-scoped, returns short-lived (10 min) TURN
/// creds plus a STUN fallback. Used by the browser controller and by the
/// native agent when it needs to trickle ICE.
pub async fn turn_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<TurnCredentialsResponse>, ApiError> {
    // This route is session-less (a pre-fetch), so it issues the default
    // region's generic URL list; the per-session same-worker affinity and the
    // region pick happen on the Hub's issuance paths.
    let ice_servers = ice_servers_for(&auth.user_id.to_hex(), state.turn_map.cfg_for(None));
    Ok(Json(TurnCredentialsResponse { ice_servers }))
}

// ────────────────────────────────────────────────────────────────────────────
fn to_session_response(s: RemoteSession) -> SessionResponse {
    SessionResponse {
        id: s.id.map(|i| i.to_hex()).unwrap_or_default(),
        agent_id: s.agent_id.to_hex(),
        tenant_id: s.tenant_id.to_hex(),
        controller_user_id: s.controller_user_id.to_hex(),
        permissions: s.permissions,
        phase: s.phase,
        created_at: fmt_dt(s.created_at),
        started_at: s.started_at.map(fmt_dt),
        ended_at: s.ended_at.map(fmt_dt),
    }
}


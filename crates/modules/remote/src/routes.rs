// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Remote-control session routes (get / terminate / audit), TURN credentials
//! and the relay-region listing — the `remote` half of the file that also
//! held the agent routes until FR-69 P5a moved those into the `fleet` module.
//! Bodies unchanged; `remote` takes these over in P6.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bson::{DateTime, oid::ObjectId};
use roomler_ai_db::models::role::permissions;
use roomler_ai_mod_fleet::agent::fmt_dt;
use roomler_ai_remote_control::{
    models::{RemoteAuditEvent, RemoteSession, SessionFeedback, SessionFeedbackRequest},
    permissions::Permissions,
    signaling::IceServer,
    turn_creds::{self, ice_servers_for},
};
use roomler_ai_services::dao::{base::PaginationParams, session_feedback::Recorded};
use roomler_core::guards::require_permission;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use roomler_core::{ApiError, extractors::auth::AuthUser};

use crate::RemoteState;

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
    State(state): State<RemoteState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
) -> Result<Json<SessionResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let sid = ObjectId::parse_str(&session_id)
        .map_err(|_| ApiError::BadRequest("Invalid session_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::NotAMember);
    }

    let session = state.remote_sessions.find_in_tenant(tid, sid).await?;
    Ok(Json(to_session_response(session)))
}

pub async fn terminate_session(
    State(state): State<RemoteState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let sid = ObjectId::parse_str(&session_id)
        .map_err(|_| ApiError::BadRequest("Invalid session_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::NotAMember);
    }

    // P3 security — bare tenant membership no longer suffices: only the
    // session's OWN controller or a member holding REMOTE_CONTROL may
    // force-close it. Party/tenant facts come from the LIVE session when this
    // pod holds it; a session homed on ANOTHER pod (rc sessions are pod-local
    // under S6) can't be inspected here, so non-controllers fall back to the
    // permission check against the ROUTE tenant — the ctrl event below only
    // acts on a hub that actually holds the session, and its tenant is
    // re-checked there.
    let live = state.fleet.rc_hub.session_snapshot(sid);
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
        .fleet
        .rc_hub
        .terminate(
            sid,
            roomler_ai_remote_control::models::EndReason::AdminTerminated,
        )
        .is_ok();
    // FR-138 P1b-2 — no hub on this pod holds the session: its end is written
    // to its row in this org, while the row is live, BEFORE the control event
    // below. A session kept through its device's socket loss is held by no
    // pod at all, and its device's claim, or its viewer's rejoin, reads the
    // end there. Nothing at window 0.
    let row_end = if live.is_none() {
        crate::row_end::admin_end(&state, tid, sid).await
    } else {
        crate::row_end::Written::NotLive
    };
    // P3 — rc sessions are pod-local (S6): when the session is homed on a
    // different pod the local terminate is a no-op that previously STILL
    // returned `{"terminated": true}`. Broadcast an idempotent ctrl event so
    // the owning pod applies it; `terminated` now reports only what THIS
    // request could verify locally.
    roomler_ai_mod_fleet::ctrl::publish_rc_ctrl(
        &state,
        "terminate",
        serde_json::json!({
            "session_id": sid.to_hex(),
            "tenant_id": tid.to_hex(),
        }),
    )
    .await;
    // Review 2, N10 — the end could not be written to a live row no hub
    // here holds: the device's claim may still resume the session, so the
    // admin is not told it ended. The event above still reaches a pod that
    // holds it; asking again writes it, or finds it ended.
    if row_end == crate::row_end::Written::Failed {
        return Err(ApiError::ServiceUnavailable(
            "the session's end could not be recorded; try again".into(),
        ));
    }
    Ok(Json(serde_json::json!({
        "terminated": terminated_here && live.is_some(),
        // FR-138 — this request wrote the end to the session's row itself.
        "row_ended": matches!(row_end, crate::row_end::Written::Wrote { .. }),
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
    State(state): State<RemoteState>,
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
// Session feedback (FR-102 P9b)
// ────────────────────────────────────────────────────────────────────────────

/// A refused rating: a stable code for the viewer's log, never the value
/// that was refused.
#[derive(Debug, Serialize)]
struct Refusal {
    error: &'static str,
}

pub(crate) fn refuse(status: StatusCode, error: &'static str) -> Response {
    (status, Json(Refusal { error })).into_response()
}

/// What a stored rating answers: what was stored, nothing about the person.
#[derive(Debug, Serialize)]
pub struct SessionFeedbackResponse {
    pub session_id: String,
    pub rating: &'static str,
    pub reasons: Vec<String>,
    pub ts: String,
}

/// FR-102 P9b — `POST /api/tenant/{tenant_id}/session/{session_id}/feedback`:
/// the viewer's post-session rating (spec §3f, D10) — `{rating: up|down,
/// reasons: [codes]}` from a person who was in the session.
///
/// | Check | Answer |
/// |---|---|
/// | `growth.feedback = false` | the **empty 404** of a path that does not exist (the kill switch; the viewer never offers the prompt either) |
/// | the caller not a member of the path's org | 403 `not_a_member` |
/// | the session not in that org | 404 — resolved within the tenant, never by id alone |
/// | the caller neither the session's controller nor a watcher | 403 |
/// | posted sooner than 30 s after the session was requested | 409 `too_early` — the viewer never offers the prompt before that, so an earlier post did not come from it |
/// | not JSON, an unknown key (a comment), a rating or a reason outside the vocabulary, too many reasons | 422 `invalid_json` · `unknown_rating` · `unknown_reason` · `too_many_reasons` — a code, never the value, nothing stored |
/// | a body over 1 KiB | 413 |
/// | this person already rated the session | 409 `already_rated` — the unique `(session_id, user_id)` index decides, so two posts by one person store one, while the controller and each watcher rate once each |
/// | stored | 201 with what was stored |
///
/// `user_id` comes from the authenticated request and `agent_id` from the
/// session row — never from the body.
pub async fn session_feedback(
    State(state): State<RemoteState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if !state.core.settings.growth.feedback {
        return StatusCode::NOT_FOUND.into_response();
    }
    match feedback_inner(&state, auth, &tenant_id, &session_id, &body).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn feedback_inner(
    state: &RemoteState,
    auth: AuthUser,
    tenant_id: &str,
    session_id: &str,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let tid = ObjectId::parse_str(tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let sid = ObjectId::parse_str(session_id)
        .map_err(|_| ApiError::BadRequest("Invalid session_id".to_string()))?;
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::NotAMember);
    }
    // Within the tenant: a foreign session is a 404 and leaks nothing.
    let session = state.remote_sessions.find_in_tenant(tid, sid).await?;
    let party =
        session.controller_user_id == auth.user_id || session.watchers.contains(&auth.user_id);
    if !party {
        return Err(ApiError::Forbidden(
            "Only a person who was in the session may rate it".to_string(),
        ));
    }
    let now = DateTime::now();
    let age_secs = (now.timestamp_millis() - session.created_at.timestamp_millis()) / 1000;
    if age_secs < SessionFeedback::MIN_SESSION_SECS {
        return Ok(refuse(StatusCode::CONFLICT, "too_early"));
    }
    let request: SessionFeedbackRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(_) => return Ok(refuse(StatusCode::UNPROCESSABLE_ENTITY, "invalid_json")),
    };
    let (rating, reasons) = match request.validate() {
        Ok(v) => v,
        Err(code) => return Ok(refuse(StatusCode::UNPROCESSABLE_ENTITY, code)),
    };
    let row = SessionFeedback {
        id: None,
        session_id: sid,
        tenant_id: tid,
        user_id: auth.user_id,
        agent_id: session.agent_id,
        rating,
        reasons: reasons.clone(),
        ts: now,
    };
    match state.session_feedback.record(&row).await {
        Ok(Recorded::Stored(_)) => {
            info!(session = %sid, rating = rating.as_str(), reasons = ?reasons, "session feedback stored");
            Ok((
                StatusCode::CREATED,
                Json(SessionFeedbackResponse {
                    session_id: sid.to_hex(),
                    rating: rating.as_str(),
                    reasons,
                    ts: fmt_dt(now),
                }),
            )
                .into_response())
        }
        Ok(Recorded::AlreadyRated) => Ok(refuse(StatusCode::CONFLICT, "already_rated")),
        Err(e) => {
            warn!(session = %sid, error = %e, "session feedback: write failed");
            Ok(refuse(StatusCode::SERVICE_UNAVAILABLE, "unavailable"))
        }
    }
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
    State(state): State<RemoteState>,
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

/// FR-115 P4b — the device a controller is about to connect to, and its org.
#[derive(Debug, Default, Deserialize)]
pub struct TurnCredentialsQuery {
    pub tenant_id: Option<String>,
    pub agent_id: Option<String>,
}

/// GET /api/turn/credentials — user-scoped, returns short-lived (10 min) TURN
/// creds plus a STUN fallback. Used by the browser controller and by the
/// native agent when it needs to trickle ICE.
///
/// FR-115 P4b — `?tenant_id=…&agent_id=…` charges the credentials to the
/// device's org ([`charge_org`]). The RC viewer builds its peer connection
/// from THESE credentials, not from the ones `rc:ready` carries, so without
/// this the browser leg of every relayed session is unattributed.
pub async fn turn_credentials(
    State(state): State<RemoteState>,
    auth: AuthUser,
    Query(q): Query<TurnCredentialsQuery>,
) -> Result<Json<TurnCredentialsResponse>, ApiError> {
    // This route is session-less (a pre-fetch), so it issues the default
    // region's generic URL list; the per-session same-worker affinity and the
    // region pick happen on the Hub's issuance paths.
    let user = match charge_org(&state, auth.user_id, &q).await {
        Some(org) => turn_creds::charged(&auth.user_id.to_hex(), &org.to_hex()),
        None => auth.user_id.to_hex(),
    };
    let ice_servers = ice_servers_for(&user, state.turn_map.cfg_for(None));
    Ok(Json(TurnCredentialsResponse { ice_servers }))
}

/// FR-115 P4b — the org a pre-fetch is charged to: the named org, when the
/// caller is a member of it and the named device is live in it. Any miss (an
/// id missing or malformed, not a member, a device elsewhere or removed, a
/// lookup error) is `None`, and the credentials are issued uncharged, as
/// before: a pre-fetch is never refused over whom to charge.
async fn charge_org(
    state: &RemoteState,
    user_id: ObjectId,
    q: &TurnCredentialsQuery,
) -> Option<ObjectId> {
    let tid = ObjectId::parse_str(q.tenant_id.as_deref()?).ok()?;
    let aid = ObjectId::parse_str(q.agent_id.as_deref()?).ok()?;
    if !state.tenants.is_member(tid, user_id).await.ok()? {
        return None;
    }
    state
        .fleet
        .agents
        .find_live_in_tenant(tid, aid)
        .await
        .ok()?;
    Some(tid)
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

/// FR-85 P3 — one remote-recording activity row, as the UI reads it (hex ids,
/// RFC 3339 time — never a raw BSON `{"$date":…}` object).
#[derive(Debug, Serialize)]
pub struct RecordingActivityDto {
    pub id: String,
    pub session_id: String,
    pub controller_user_id: String,
    pub kind: roomler_ai_remote_control::models::RecordingActivityKind,
    pub name: Option<String>,
    pub bytes: Option<u64>,
    pub duration_ms: Option<u64>,
    pub reason: Option<String>,
    pub at: String,
}

#[derive(Debug, Serialize)]
pub struct RecordingActivityListResponse {
    pub items: Vec<RecordingActivityDto>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

/// FR-85 P3 — what a device reported about remote recordings, newest first.
/// Gated like the audit it is read with (`VIEW_REMOTE_AUDIT`); the device is
/// resolved WITHIN the tenant, so a foreign id 404s.
pub async fn recording_activity(
    State(state): State<RemoteState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<RecordingActivityListResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::VIEW_REMOTE_AUDIT,
        "VIEW_REMOTE_AUDIT",
    )
    .await?;
    // ANY, not live (#1821): this is an audit view, and a removed device's
    // recording trail must stay readable — otherwise removing the device
    // would be the way to hide what it reported. The lookup is only the
    // tenant scope (a foreign or bogus id is a 404); nothing of the row is
    // returned.
    let _ = state.fleet.agents.find_any_in_tenant(tid, aid).await?;

    let page = state
        .recording_activity
        .list_for_agent(tid, aid, &params)
        .await?;
    Ok(Json(RecordingActivityListResponse {
        items: page
            .items
            .into_iter()
            .map(|e| RecordingActivityDto {
                id: e.id.map(|i| i.to_hex()).unwrap_or_default(),
                session_id: e.session_id.to_hex(),
                controller_user_id: e.controller_user_id.to_hex(),
                kind: e.kind,
                name: e.name,
                bytes: e.bytes,
                duration_ms: e.duration_ms,
                reason: e.reason,
                at: fmt_dt(e.at),
            })
            .collect(),
        total: page.total,
        page: page.page,
        per_page: page.per_page,
        total_pages: page.total_pages,
    }))
}

#[derive(Debug, Serialize)]
pub struct TransferActivityDto {
    pub id: String,
    pub session_id: String,
    pub controller_user_id: String,
    pub kind: roomler_ai_remote_control::models::TransferActivityKind,
    pub count: u64,
    pub bytes: u64,
    pub name_sha256: Option<String>,
    pub at: String,
}

#[derive(Debug, Serialize)]
pub struct TransferActivityListResponse {
    pub items: Vec<TransferActivityDto>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

/// FR-101 P3 — the clipboard and file traffic a device reported, newest
/// first. Read like the recording trail beside it: under `VIEW_REMOTE_AUDIT`,
/// with the device resolved WITHIN the tenant (any, not live: removing a
/// device must not be the way to hide what it reported).
pub async fn transfer_activity(
    State(state): State<RemoteState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<TransferActivityListResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::VIEW_REMOTE_AUDIT,
        "VIEW_REMOTE_AUDIT",
    )
    .await?;
    let _ = state.fleet.agents.find_any_in_tenant(tid, aid).await?;

    let page = state
        .transfer_activity
        .list_for_agent(tid, aid, &params)
        .await?;
    Ok(Json(TransferActivityListResponse {
        items: page
            .items
            .into_iter()
            .map(|e| TransferActivityDto {
                id: e.id.map(|i| i.to_hex()).unwrap_or_default(),
                session_id: e.session_id.to_hex(),
                controller_user_id: e.controller_user_id.to_hex(),
                kind: e.kind,
                count: e.count,
                bytes: e.bytes,
                name_sha256: e.name_sha256,
                at: fmt_dt(e.at),
            })
            .collect(),
        total: page.total,
        page: page.page,
        per_page: page.per_page,
        total_pages: page.total_pages,
    }))
}

#[derive(Debug, Serialize)]
pub struct PrivacyActivityDto {
    pub id: String,
    pub session_id: String,
    pub controller_user_id: String,
    pub kind: roomler_ai_remote_control::models::PrivacyActivityKind,
    pub reason: Option<String>,
    pub at: String,
}

#[derive(Debug, Serialize)]
pub struct PrivacyActivityListResponse {
    pub items: Vec<PrivacyActivityDto>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

/// FR-99 P1d — what a device reported about privacy mode, newest first: when
/// its screen was covered, for whose session, and why the curtain came down.
/// Read like the recording and transfer trails beside it: under
/// `VIEW_REMOTE_AUDIT`, with the device resolved WITHIN the tenant (any, not
/// live: removing a device must not be the way to hide what it reported).
pub async fn privacy_activity(
    State(state): State<RemoteState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<PrivacyActivityListResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::VIEW_REMOTE_AUDIT,
        "VIEW_REMOTE_AUDIT",
    )
    .await?;
    let _ = state.fleet.agents.find_any_in_tenant(tid, aid).await?;

    let page = state
        .privacy_activity
        .list_for_agent(tid, aid, &params)
        .await?;
    Ok(Json(PrivacyActivityListResponse {
        items: page
            .items
            .into_iter()
            .map(|e| PrivacyActivityDto {
                id: e.id.map(|i| i.to_hex()).unwrap_or_default(),
                session_id: e.session_id.to_hex(),
                controller_user_id: e.controller_user_id.to_hex(),
                kind: e.kind,
                reason: e.reason,
                at: fmt_dt(e.at),
            })
            .collect(),
        total: page.total,
        page: page.page,
        per_page: page.per_page,
        total_pages: page.total_pages,
    }))
}

#[cfg(test)]
mod tests {
    //! #4189 — what the session audit answer (`GET
    //! …/session/{sid}/audit`) carries on the wire: every id the 24-hex
    //! string and every time RFC 3339, never bson's `{"$oid": …}` or
    //! `{"$date": …}`, the ids and times inside an event's kind included.

    use super::*;
    use roomler_ai_remote_control::models::{AuditKind, EndReason};
    use serde_json::{Value, json};

    fn is_hex24(s: &str) -> bool {
        s.len() == 24 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }

    fn at(s: &str) -> DateTime {
        DateTime::parse_rfc3339_str(s).expect("a test time")
    }

    /// What is wrong with `got` as an id: not a string, not 24 lowercase hex,
    /// or not the stored one.
    fn id_problem(what: &str, got: &Value, want: ObjectId) -> Option<String> {
        match got.as_str() {
            None => Some(format!("{what} is not a string: {got}")),
            Some(s) if !is_hex24(s) => Some(format!("{what} is not 24 lowercase hex: {s}")),
            Some(s) if s != want.to_hex() => Some(format!("{what} {s} is not the stored {want}")),
            Some(_) => None,
        }
    }

    /// What is wrong with `got` as a time: not a string, not RFC 3339, or
    /// not the stored instant.
    fn time_problem(what: &str, got: &Value, want: DateTime) -> Option<String> {
        match got.as_str() {
            None => Some(format!("{what} is not a string: {got}")),
            Some(s) => match DateTime::parse_rfc3339_str(s) {
                Err(_) => Some(format!("{what} is not RFC 3339: {s}")),
                Ok(back) if back != want => Some(format!("{what} {s} is not the stored time")),
                Ok(_) => None,
            },
        }
    }

    /// Every kind that carries an id or a time, and one that carries
    /// neither: the answer names every id as hex and every time as RFC 3339,
    /// and leaves the rest of each event as it was.
    #[test]
    fn a_session_audit_answer_carries_every_id_and_time_as_a_string() {
        let (session, agent, tenant) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
        let (controller, watcher) = (ObjectId::new(), ObjectId::new());
        let resume_by = at("2026-11-21T07:01:00.250Z");
        let ended_at = at("2026-11-21T07:02:00Z");
        let kinds = vec![
            AuditKind::SessionRequested {
                controller_user_id: controller,
                controller_name: "ops".into(),
                permissions: Permissions::VIEW | Permissions::INPUT,
                record_stripped: None,
                privacy_stripped: None,
                features_stripped: None,
                idle_secs: Some(600),
                idle_enforced: Some(true),
                lock_on_end: None,
                lock_enforced: None,
                external: false,
            },
            AuditKind::WatcherJoined { user_id: watcher },
            AuditKind::WatcherLeft { user_id: watcher },
            AuditKind::SessionDetached {
                attach_seq: 3,
                resume_by,
            },
            AuditKind::SessionEnded {
                reason: EndReason::AgentDisconnect,
                limit: None,
                attach_seq: Some(4),
                ended_at: Some(ended_at),
                end_source: None,
            },
            AuditKind::ConsentGranted,
        ];
        let times: Vec<DateTime> = (0..kinds.len())
            .map(|i| DateTime::from_millis(1_763_708_400_000 + 1_000 * i as i64 + 7))
            .collect();
        let events: Vec<RemoteAuditEvent> = kinds
            .iter()
            .cloned()
            .zip(&times)
            .map(|(event, at)| RemoteAuditEvent {
                id: Some(ObjectId::new()),
                session_id: session,
                agent_id: agent,
                tenant_id: tenant,
                at: *at,
                event,
            })
            .collect();
        let answer = AuditListResponse {
            items: events.clone().into_iter().map(Into::into).collect(),
            total: events.len() as u64,
            page: 1,
            per_page: 50,
            total_pages: 1,
        };
        let answer = serde_json::to_value(&answer).expect("serialises");

        let items = answer["items"].as_array().cloned().unwrap_or_default();
        let mut wrong: Vec<String> = Vec::new();
        if items.len() != events.len() {
            wrong.push(format!(
                "{} items, want {}: {answer}",
                items.len(),
                events.len()
            ));
        }
        for (i, (item, stored)) in items.iter().zip(&events).enumerate() {
            let p = |field: &str| format!("items[{i}].{field}");
            wrong.extend(id_problem(&p("_id"), &item["_id"], stored.id.unwrap()));
            wrong.extend(id_problem(&p("session_id"), &item["session_id"], session));
            wrong.extend(id_problem(&p("agent_id"), &item["agent_id"], agent));
            wrong.extend(id_problem(&p("tenant_id"), &item["tenant_id"], tenant));
            wrong.extend(time_problem(&p("at"), &item["at"], stored.at));
        }
        let event = |i: usize| {
            items
                .get(i)
                .map(|it| it["event"].clone())
                .unwrap_or_default()
        };
        wrong.extend(id_problem(
            "the request's controller_user_id",
            &event(0)["controller_user_id"],
            controller,
        ));
        wrong.extend(id_problem(
            "watcher_joined.user_id",
            &event(1)["user_id"],
            watcher,
        ));
        wrong.extend(id_problem(
            "watcher_left.user_id",
            &event(2)["user_id"],
            watcher,
        ));
        wrong.extend(time_problem(
            "session_detached.resume_by",
            &event(3)["resume_by"],
            resume_by,
        ));
        wrong.extend(time_problem(
            "session_ended.ended_at",
            &event(4)["ended_at"],
            ended_at,
        ));

        // What is not an id or a time is as it was: each kind's other keys,
        // and a kind with neither, whole.
        let request = event(0);
        let want_request = json!({
            "kind": "session_requested",
            "controller_name": "ops",
            "permissions": serde_json::to_value(Permissions::VIEW | Permissions::INPUT).unwrap(),
            "idle_secs": 600,
            "idle_enforced": true,
        });
        for (key, want) in want_request.as_object().unwrap() {
            if request.get(key) != Some(want) {
                wrong.push(format!("the request's {key} changed: {request}"));
            }
        }
        if event(3)["kind"] != "session_detached" || event(3)["attach_seq"] != 3 {
            wrong.push(format!(
                "the detach's kind or attach_seq changed: {}",
                event(3)
            ));
        }
        if event(4)["reason"] != "agent_disconnect" || event(4)["attach_seq"] != 4 {
            wrong.push(format!(
                "the end's reason or attach_seq changed: {}",
                event(4)
            ));
        }
        if event(4).get("limit").is_some() || event(4).get("end_source").is_some() {
            wrong.push(format!(
                "the end's unset fields are left out, as before: {}",
                event(4)
            ));
        }
        if event(5) != json!({ "kind": "consent_granted" }) {
            wrong.push(format!("a kind with no id or time changed: {}", event(5)));
        }
        let text = answer.to_string();
        for raw in ["$oid", "$date"] {
            if text.contains(raw) {
                wrong.push(format!("the answer carries `{raw}`: {text}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }
}

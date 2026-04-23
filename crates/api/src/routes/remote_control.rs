//! REST surface for the remote-control subsystem.
//!
//! Per `docs/remote-control.md` §9.1. Signaling (SDP/ICE) happens over the
//! WebSocket; this module is everything else: agent enrollment, CRUD, session
//! introspection, TURN credential issuance.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use bson::{DateTime, oid::ObjectId};
use roomler_ai_remote_control::{
    models::{AccessPolicy, AgentStatus, OsKind, RemoteAuditEvent, RemoteSession},
    permissions::Permissions,
    signaling::IceServer,
    turn_creds::ice_servers_for,
};
use roomler_ai_services::dao::base::PaginationParams;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

const ENROLLMENT_TTL_SECS: u64 = 600; // 10 minutes per §11.1

// ────────────────────────────────────────────────────────────────────────────
// Agent enrollment
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct EnrollmentTokenResponse {
    pub enrollment_token: String,
    pub expires_in: u64,
    pub jti: String,
}

/// POST /api/tenant/{tenant_id}/agent/enroll-token — admin issues an enrollment
/// token that a new agent binary exchanges (once, within 10 min) for a
/// long-lived agent token.
pub async fn issue_enrollment_token(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<EnrollmentTokenResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let (token, jti) = state
        .auth
        .issue_enrollment_token(auth.user_id, tid, ENROLLMENT_TTL_SECS)?;

    Ok(Json(EnrollmentTokenResponse {
        enrollment_token: token,
        expires_in: ENROLLMENT_TTL_SECS,
        jti,
    }))
}

#[derive(Debug, Deserialize)]
pub struct EnrollRequest {
    pub enrollment_token: String,
    pub machine_id: String,
    pub machine_name: String,
    pub os: OsKind,
    pub agent_version: String,
}

#[derive(Debug, Serialize)]
pub struct EnrollResponse {
    pub agent_id: String,
    pub tenant_id: String,
    pub agent_token: String,
}

/// POST /api/agent/enroll — public (no user JWT); authenticates via the
/// enrollment token instead. Creates or rehydrates the Agent row and returns
/// a long-lived agent JWT.
pub async fn enroll_agent(
    State(state): State<AppState>,
    Json(body): Json<EnrollRequest>,
) -> Result<Json<EnrollResponse>, ApiError> {
    let claims = state.auth.verify_enrollment_token(&body.enrollment_token)?;
    let tid = ObjectId::parse_str(&claims.tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id claim".to_string()))?;
    let admin_uid = ObjectId::parse_str(&claims.sub)
        .map_err(|_| ApiError::BadRequest("Invalid admin user id claim".to_string()))?;

    // If a row already exists for (tenant_id, machine_id), re-issue a token
    // against it — supports reinstall / re-enroll cycles without leaking stale
    // rows. Machine_id should be a stable HMAC of DMI + MAC.
    let existing = state
        .agents
        .find_by_tenant_and_machine(tid, &body.machine_id)
        .await?;
    let agent = match existing {
        Some(a) => a,
        None => {
            state
                .agents
                .create(
                    tid,
                    admin_uid,
                    body.machine_name,
                    body.machine_id,
                    body.os,
                    body.agent_version,
                    String::new(), // agent_token_hash unused in JWT-only scheme
                )
                .await?
        }
    };

    let agent_id = agent
        .id
        .ok_or_else(|| ApiError::Internal("agent missing _id".to_string()))?;
    let agent_token = state.auth.issue_agent_token(agent_id, tid, None)?;

    Ok(Json(EnrollResponse {
        agent_id: agent_id.to_hex(),
        tenant_id: tid.to_hex(),
        agent_token,
    }))
}

// ────────────────────────────────────────────────────────────────────────────
// Agent CRUD
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AgentResponse {
    pub id: String,
    pub tenant_id: String,
    pub owner_user_id: String,
    pub name: String,
    pub machine_id: String,
    pub os: OsKind,
    pub agent_version: String,
    pub status: AgentStatus,
    /// Live `true` when the Hub holds an active WS to this agent, independent
    /// of the persisted `status` field (which can drift across restarts).
    pub is_online: bool,
    pub last_seen_at: String,
    pub access_policy: AccessPolicy,
    /// Codec + HW backend availability advertised by the agent in its
    /// most recent rc:agent.hello. Default empty for pre-2A.1 agents
    /// that haven't reconnected since the schema change.
    pub capabilities: roomler_ai_remote_control::models::AgentCaps,
}

pub async fn list_agents(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let page = state.agents.list_for_tenant(tid, &params).await?;
    let items: Vec<AgentResponse> = page
        .items
        .into_iter()
        .map(|a| to_agent_response(&state, a))
        .collect();

    Ok(Json(serde_json::json!({
        "items": items,
        "total": page.total,
        "page": page.page,
        "per_page": page.per_page,
        "total_pages": page.total_pages,
    })))
}

pub async fn get_agent(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
) -> Result<Json<AgentResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let agent = state.agents.find_in_tenant(tid, aid).await?;
    Ok(Json(to_agent_response(&state, agent)))
}

#[derive(Debug, Deserialize)]
pub struct UpdateAgentRequest {
    pub name: Option<String>,
    pub access_policy: Option<AccessPolicy>,
}

pub async fn update_agent(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    Json(body): Json<UpdateAgentRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    if let Some(name) = body.name {
        state.agents.rename(tid, aid, &name).await?;
    }
    if let Some(policy) = body.access_policy {
        state.agents.update_access_policy(tid, aid, &policy).await?;
    }

    Ok(Json(serde_json::json!({ "updated": true })))
}

pub async fn delete_agent(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state.agents.soft_delete(tid, aid).await?;
    state.rc_hub.unregister_agent(aid); // kick any live WS
    Ok(Json(serde_json::json!({ "deleted": true })))
}

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

    // Force-close via Hub. The Hub pushes a Terminate to both peers and audits.
    let _ = state.rc_hub.terminate(
        sid,
        roomler_ai_remote_control::models::EndReason::AdminTerminated,
    );
    Ok(Json(serde_json::json!({ "terminated": true })))
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

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

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

/// GET /api/turn/credentials — user-scoped, returns short-lived (10 min) TURN
/// creds plus a STUN fallback. Used by the browser controller and by the
/// native agent when it needs to trickle ICE.
pub async fn turn_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<TurnCredentialsResponse>, ApiError> {
    // Build a fresh TurnConfig view the same way AppState does. We can't hold
    // a TurnConfig in AppState because it's owned by the Hub; query it here
    // via a small helper.
    let turn_cfg = build_turn_config(&state.settings.turn);
    let ice_servers = ice_servers_for(&auth.user_id.to_hex(), turn_cfg.as_ref());
    Ok(Json(TurnCredentialsResponse { ice_servers }))
}

fn build_turn_config(
    turn: &roomler_ai_config::TurnSettings,
) -> Option<roomler_ai_remote_control::turn_creds::TurnConfig> {
    let secret = turn.shared_secret.as_ref()?.clone();
    let base = turn.url.as_deref()?;
    let mut urls = vec![base.to_string()];
    if base.starts_with("turn:") && !base.contains("?transport=") {
        urls.push(format!("{}?transport=tcp", base));
        let turns_5349 = base
            .replacen("turn:", "turns:", 1)
            .replace(":3478", ":5349");
        urls.push(format!("{}?transport=tcp", turns_5349));
        let turns_443 = base.replacen("turn:", "turns:", 1).replace(":3478", ":443");
        urls.push(format!("{}?transport=tcp", turns_443));
    }
    Some(roomler_ai_remote_control::turn_creds::TurnConfig {
        urls,
        shared_secret: secret,
        ttl_secs: 600,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

fn to_agent_response(
    state: &AppState,
    a: roomler_ai_remote_control::models::Agent,
) -> AgentResponse {
    let id = a.id.map(|i| i.to_hex()).unwrap_or_default();
    let is_online =
        a.id.map(|i| state.rc_hub.is_agent_online(i))
            .unwrap_or(false);
    AgentResponse {
        id,
        tenant_id: a.tenant_id.to_hex(),
        owner_user_id: a.owner_user_id.to_hex(),
        name: a.name,
        machine_id: a.machine_id,
        os: a.os,
        agent_version: a.agent_version,
        status: a.status,
        is_online,
        last_seen_at: fmt_dt(a.last_seen_at),
        access_policy: a.access_policy,
        capabilities: a.capabilities,
    }
}

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

fn fmt_dt(dt: DateTime) -> String {
    dt.try_to_rfc3339_string()
        .unwrap_or_else(|_| dt.timestamp_millis().to_string())
}

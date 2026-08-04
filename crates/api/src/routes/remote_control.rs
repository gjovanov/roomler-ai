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
use roomler_ai_db::models::role::permissions;
use roomler_ai_remote_control::{
    models::{AccessPolicy, AgentStatus, NodeRef, OsKind, RemoteAuditEvent, RemoteSession},
    permissions::Permissions,
    signaling::IceServer,
    turn_creds::ice_servers_for,
};
use roomler_ai_services::dao::base::PaginationParams;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

const ENROLLMENT_TTL_SECS: u64 = 600; // 10 minutes per §11.1

/// Require a `role::permissions` bit for `user_id` in `tenant_id`. Doubles as
/// the membership check — `get_member_permissions` returns `Forbidden` for a
/// non-member. `owner` always passes via the `ADMINISTRATOR` bypass in `has`.
///
/// `pub(crate)` so the sibling device-management surfaces in the same subsystem
/// (`routes::overlay_route`, `routes::tunnel`) gate their destructive endpoints
/// on the same `MANAGE_AGENTS` bit rather than re-deriving the check.
pub(crate) async fn require_permission(
    state: &AppState,
    tenant_id: ObjectId,
    user_id: ObjectId,
    flag: u64,
    label: &str,
) -> Result<(), ApiError> {
    let perms = state
        .tenants
        .get_member_permissions(tenant_id, user_id)
        .await?;
    if !permissions::has(perms, flag) {
        return Err(ApiError::Forbidden(format!("Missing {label} permission")));
    }
    Ok(())
}

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

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

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

    // If a row already exists for (tenant_id, machine_id), rehydrate it —
    // refresh name / os / agent_version, clear `deleted_at` if soft-deleted,
    // and re-issue a token against the same _id. The unique index on
    // (tenant_id, machine_id) covers soft-deleted rows too, so we must look
    // them up *including* tombstones and revive in place rather than create.
    let existing = state
        .agents
        .find_by_tenant_and_machine(tid, &body.machine_id)
        .await?;
    let agent = match existing {
        Some(a) => {
            let id =
                a.id.ok_or_else(|| ApiError::Internal("agent missing _id".to_string()))?;
            state
                .agents
                .rehydrate(id, &body.machine_name, body.os, &body.agent_version)
                .await?
        }
        None => {
            // S5 — plan device cap. Only a NEW row consumes a slot; a
            // known machine rehydrates in place above regardless of the
            // cap (re-enrolling your existing fleet must never brick).
            let tenant = state.tenants.base.find_by_id(tid).await?;
            let max = tenant.plan.limits().max_devices as u64;
            let used = state.agents.count_active_for_tenant(tid).await?;
            if used >= max {
                return Err(ApiError::Forbidden(format!(
                    "Device limit reached for the {:?} plan ({used} of {max} devices used). \
                     Upgrade the plan or remove a device first.",
                    tenant.plan
                )));
            }
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

/// Phase A-1 three-state presence (see `to_agent_response`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentPresence {
    /// An rc socket is registered on some pod — Connect will work.
    Online,
    /// Heartbeat trail fresh but no pod claims the socket — amber.
    Stale,
    Offline,
}

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
    /// Three-state truth: `online` | `stale` | `offline` (Phase A-1).
    pub presence: AgentPresence,
    /// Back-compat bool = `presence == online` (a socket is actually
    /// registered; pre-A-1 this also counted heartbeat-only agents,
    /// which is what showed green on dead sockets).
    pub is_online: bool,
    pub last_seen_at: String,
    pub access_policy: AccessPolicy,
    /// Subnet-router CIDRs this agent advertises for the mesh (Phase 2). The
    /// `roomler-tunnel socks5` mesh longest-prefix-matches a LAN target IP
    /// against these to pick the covering agent, which then dials the real
    /// IP. Admin-managed here; still gated by the tenant's `tunnel_policies`.
    pub routes: Vec<String>,
    /// Subnet CIDRs the agent itself ADVERTISES it can route (from its
    /// `advertise_routes` config, sent on `rc:agent.hello`). Untrusted
    /// suggestions the admin approves into `routes`. Empty for pre-feature agents.
    pub advertised_routes: Vec<String>,
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
    let redis_fresh = agent_presence_batch(&state, &page.items).await;
    let items: Vec<AgentResponse> = page
        .items
        .into_iter()
        .map(|a| {
            let fresh = a.id.map(|i| redis_fresh.contains(&i)).unwrap_or(false);
            to_agent_response(&state, a, fresh)
        })
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
    let redis_fresh = agent_presence_batch(&state, std::slice::from_ref(&agent)).await;
    let fresh = agent.id.map(|i| redis_fresh.contains(&i)).unwrap_or(false);
    Ok(Json(to_agent_response(&state, agent, fresh)))
}

#[derive(Debug, Deserialize)]
pub struct UpdateAgentRequest {
    pub name: Option<String>,
    pub access_policy: Option<AccessPolicy>,
    /// Reassign the device owner (hex user id). `MANAGE_AGENTS` only.
    pub owner_user_id: Option<String>,
    /// Replace the advertised subnet-router CIDRs (mesh Phase 2). Each entry
    /// is validated + canonicalized by `normalize_routes`; an invalid CIDR
    /// fails the whole request with 400. `MANAGE_AGENTS` only.
    pub routes: Option<Vec<String>>,
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

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

    if let Some(owner) = body.owner_user_id {
        let owner_id = ObjectId::parse_str(&owner)
            .map_err(|_| ApiError::BadRequest("Invalid owner_user_id".to_string()))?;
        state.agents.update_owner(tid, aid, owner_id).await?;
    }
    if let Some(name) = body.name {
        state.agents.rename(tid, aid, &name).await?;
    }
    if let Some(policy) = body.access_policy {
        state.agents.update_access_policy(tid, aid, &policy).await?;
    }
    if let Some(routes) = body.routes {
        let normalized = normalize_routes(routes)?;
        state.agents.update_routes(tid, aid, &normalized).await?;
    }

    Ok(Json(serde_json::json!({ "updated": true })))
}

/// DELETE /api/tenant/{tid}/agent/{agent_id} — remove a device from the fleet.
///
/// Cascades to the overlay: the agent's mesh node is evicted (peers get a
/// `removes` delta) and its overlay IP goes back to the tenant's free pool for
/// reuse. The agent binary stays installed on the host and may be enrolled
/// again — but it comes back as a NEW mesh node with a fresh address.
pub async fn delete_agent(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

    // Read first: gives a real 404 for a bogus agent id (`soft_delete` returns
    // a bool the handler used to discard, so deleting a nonexistent agent
    // reported `{"deleted": true}`), and yields the machine_id the overlay node
    // is keyed by.
    let agent = state.agents.find_in_tenant(tid, aid).await?;

    // Release the overlay lease BEFORE the soft-delete and BEFORE the kick. The
    // kick tears down the WS, whose teardown calls `handle_overlay_leave` — with
    // the node already tombstoned that is a clean no-op instead of a second
    // `removes` fan racing the release's compare-and-swap.
    let released = crate::ws::overlay::release_overlay_node_for(
        &state,
        tid,
        &agent.machine_id,
        &NodeRef::Agent { agent_id: aid },
        "agent_delete",
    )
    .await;

    state.agents.soft_delete(tid, aid).await?;
    // rc.53: admin-driven kick — no tx identity to thread through;
    // pass None to force unconditional removal (the identity gate is
    // only for the displaced-handler unregister race, not for
    // operator kicks).
    state.rc_hub.unregister_agent(aid, None); // kick any live WS
    // C-2 — the agent's WS may be homed on another pod; broadcast the
    // kick so every hub drops it (idempotent no-op where absent).
    crate::ws::remote_control::publish_rc_ctrl(
        &state,
        "kick",
        serde_json::json!({ "agent_id": aid.to_hex() }),
    )
    .await;
    Ok(Json(serde_json::json!({
        "deleted": true,
        "overlay_released": released.is_some(),
        "overlay_ip": released.as_ref().map(|r| r.overlay_ip.clone()),
    })))
}

// ────────────────────────────────────────────────────────────────────────────
// Forced self-update (S1a)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct TriggerUpdateRequest {
    /// Optional release tag to pin (e.g. `agent-v0.3.0-rc.260`); omitted =
    /// latest. Forwarded verbatim to the agent.
    #[serde(default)]
    pub pin: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TriggerUpdateResult {
    pub agent_id: String,
    /// Whether the message reached a live agent WS. `false` = offline (or a
    /// full outbound queue); offline agents pick the release up on their own
    /// periodic check anyway.
    pub delivered: bool,
}

/// POST /api/tenant/{tid}/agent/{agent_id}/update — push `rc:agent.update`
/// to one agent. MANAGE_AGENTS. Pre-S1a agents ignore the unknown message
/// (decode-and-drop, same contract as `rc:goodbye`).
pub async fn trigger_agent_update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    // Body is required (send `{}` for defaults) — plain `Json` keeps us off
    // axum 0.8's `OptionalFromRequest` surface, which the codebase doesn't
    // use anywhere else.
    Json(body): Json<TriggerUpdateRequest>,
) -> Result<Json<TriggerUpdateResult>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

    // Tenant-scope the target (404 for a foreign agent id).
    let _ = state.agents.find_in_tenant(tid, aid).await?;

    let pin = body.pin;
    let delivered = state
        .rc_hub
        .send_to_agent(
            aid,
            roomler_ai_remote_control::signaling::ServerMsg::UpdateNow { pin: pin.clone() },
        )
        .is_ok();
    tracing::info!(
        admin = %auth.user_id, agent = %aid, ?pin, delivered,
        "operator-triggered agent self-update"
    );
    Ok(Json(TriggerUpdateResult {
        agent_id: aid.to_hex(),
        delivered,
    }))
}

#[derive(Debug, Deserialize, Default)]
pub struct BulkTriggerUpdateRequest {
    /// Explicit targets (hex agent ids). Omitted/empty = every agent in the
    /// tenant.
    #[serde(default)]
    pub agent_ids: Option<Vec<String>>,
    #[serde(default)]
    pub pin: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BulkTriggerUpdateResponse {
    pub results: Vec<TriggerUpdateResult>,
    pub requested: usize,
    pub delivered: usize,
}

/// POST /api/tenant/{tid}/agent/update — push `rc:agent.update` to selected
/// (or all) agents in the tenant. MANAGE_AGENTS. Fleet-side stampede control
/// is the agents' own 5-min install-storm cooldown + the release proxy cache.
pub async fn trigger_agents_update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<BulkTriggerUpdateRequest>,
) -> Result<Json<BulkTriggerUpdateResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

    let target_ids: Vec<ObjectId> = match body.agent_ids {
        Some(ids) if !ids.is_empty() => {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                let aid = ObjectId::parse_str(&id)
                    .map_err(|_| ApiError::BadRequest(format!("Invalid agent_id: {id}")))?;
                // Tenant-scope every explicit target.
                let _ = state.agents.find_in_tenant(tid, aid).await?;
                out.push(aid);
            }
            out
        }
        _ => {
            let page = state
                .agents
                .list_for_tenant(
                    tid,
                    &PaginationParams {
                        page: 1,
                        per_page: 1000,
                        before: None,
                    },
                )
                .await?;
            page.items.into_iter().filter_map(|a| a.id).collect()
        }
    };

    let mut results = Vec::with_capacity(target_ids.len());
    let mut delivered = 0usize;
    for aid in &target_ids {
        let ok = state
            .rc_hub
            .send_to_agent(
                *aid,
                roomler_ai_remote_control::signaling::ServerMsg::UpdateNow {
                    pin: body.pin.clone(),
                },
            )
            .is_ok();
        if ok {
            delivered += 1;
        }
        results.push(TriggerUpdateResult {
            agent_id: aid.to_hex(),
            delivered: ok,
        });
    }
    tracing::info!(
        admin = %auth.user_id, tenant = %tid,
        requested = target_ids.len(), delivered, pin = ?body.pin,
        "operator-triggered bulk agent self-update"
    );
    Ok(Json(BulkTriggerUpdateResponse {
        requested: results.len(),
        delivered,
        results,
    }))
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
// Helpers
// ────────────────────────────────────────────────────────────────────────────

/// Batched cross-pod presence read for the listing (Phase A-1): one MGET
/// under a 200 ms budget. Returns the set of agent ids with a FRESH
/// (TTL-enforced) directory record on ANY pod. Redis down / timeout ⇒
/// empty set — `to_agent_response` then degrades to the pre-A-1
/// heartbeat disjunction, never to hard-offline.
async fn agent_presence_batch(
    state: &AppState,
    agents: &[roomler_ai_remote_control::models::Agent],
) -> std::collections::HashSet<ObjectId> {
    let Some(redis) = &state.redis_pubsub else {
        return Default::default();
    };
    let ids: Vec<ObjectId> = agents.iter().filter_map(|a| a.id).collect();
    let hexes: Vec<String> = ids.iter().map(|i| i.to_hex()).collect();
    match tokio::time::timeout(
        std::time::Duration::from_millis(200),
        redis.agent_presence_get_many(&hexes),
    )
    .await
    {
        Ok(Ok(vals)) => ids
            .into_iter()
            .zip(vals)
            .filter_map(|(id, v)| v.map(|_| id))
            .collect(),
        Ok(Err(e)) => {
            tracing::debug!(%e, "agent presence MGET failed; degrading to heartbeat disjunction");
            Default::default()
        }
        Err(_) => {
            tracing::debug!("agent presence MGET timed out; degrading to heartbeat disjunction");
            Default::default()
        }
    }
}

fn to_agent_response(
    state: &AppState,
    a: roomler_ai_remote_control::models::Agent,
    redis_fresh: bool,
) -> AgentResponse {
    let id = a.id.map(|i| i.to_hex()).unwrap_or_default();
    // Phase A-1 three-state presence. "online" = an rc socket is
    // REGISTERED somewhere (this pod's hub, or any pod's fresh Redis
    // directory record) — the state in which Connect will actually work.
    // "stale" = the Mongo heartbeat trail is fresh but no pod claims the
    // socket (half-open middlebox leg, directory outage, or a pod that
    // died without cleanup) — visible, amber, Connect disabled. "offline"
    // = neither. With Redis down `redis_fresh` is always false, so the
    // presence degrades to the pre-A-1 heartbeat disjunction (a cross-pod
    // agent shows "stale" instead of "online" — degraded, never lying
    // green on a dead socket ONLY, and never hard-offline).
    let hub_online =
        a.id.map(|i| state.rc_hub.is_agent_online(i))
            .unwrap_or(false);
    let recently_seen = matches!(
        a.status,
        roomler_ai_remote_control::models::AgentStatus::Online
    ) && {
        let age_ms = bson::DateTime::now().timestamp_millis() - a.last_seen_at.timestamp_millis();
        age_ms < 90_000
    };
    let presence = if hub_online || redis_fresh {
        AgentPresence::Online
    } else if recently_seen {
        AgentPresence::Stale
    } else {
        AgentPresence::Offline
    };
    // Back-compat: `is_online` = the reachable state only. (Pre-A-1 it
    // was `hub_online || recently_seen`, which is what lied green.)
    let is_online = matches!(presence, AgentPresence::Online);
    AgentResponse {
        presence,
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
        routes: a.routes,
        advertised_routes: a.advertised_routes,
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

/// Validate + canonicalize the subnet-route CIDRs an admin assigns to an agent
/// for the mesh subnet-router (Phase 2). Every entry must be valid CIDR
/// notation — IPv4 or IPv6, e.g. `10.66.24.0/24` or a single host
/// `10.66.24.53/32`. Host bits are masked to the network address, blank entries
/// are dropped, and duplicates removed. A bare IP or any unparseable entry
/// fails the whole request with 400 so the admin UI shows a clear error instead
/// of silently storing a route the mesh client would skip. Mirrors the
/// client-side parse in `roomler-tunnel`'s `mesh.rs` (both use `ipnet::IpNet`).
fn normalize_routes(raw: Vec<String>) -> Result<Vec<String>, ApiError> {
    use std::str::FromStr;
    const MAX_ROUTES: usize = 64;
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for entry in raw {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let net = ipnet::IpNet::from_str(trimmed).map_err(|_| {
            ApiError::BadRequest(format!(
                "Invalid route CIDR '{trimmed}' — use CIDR notation, \
                 e.g. 10.66.24.0/24 or a single host 10.66.24.53/32"
            ))
        })?;
        let canonical = net.trunc().to_string();
        if !out.contains(&canonical) {
            out.push(canonical);
        }
    }
    if out.len() > MAX_ROUTES {
        return Err(ApiError::BadRequest(format!(
            "Too many routes ({}); max {MAX_ROUTES}",
            out.len()
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::normalize_routes;

    #[test]
    fn normalize_routes_canonicalizes_and_dedups() {
        // host bits masked to the network; duplicate collapsed; blanks dropped
        let out = normalize_routes(vec![
            "10.66.24.53/24".to_string(),
            "  ".to_string(),
            "10.66.24.0/24".to_string(),
            "192.168.1.0/24".to_string(),
        ])
        .unwrap();
        assert_eq!(out, vec!["10.66.24.0/24", "192.168.1.0/24"]);
    }

    #[test]
    fn normalize_routes_accepts_host_route_and_ipv6() {
        let out = normalize_routes(vec![
            "10.66.24.53/32".to_string(),
            "2001:db8::/32".to_string(),
        ])
        .unwrap();
        assert_eq!(out, vec!["10.66.24.53/32", "2001:db8::/32"]);
    }

    #[test]
    fn normalize_routes_rejects_bare_ip_and_garbage() {
        // a bare IP (no prefix) is rejected — CIDR notation is required so the
        // stored value always parses on the mesh client side.
        assert!(normalize_routes(vec!["10.66.24.53".to_string()]).is_err());
        assert!(normalize_routes(vec!["not-a-cidr".to_string()]).is_err());
        assert!(normalize_routes(vec!["10.66.24.0/33".to_string()]).is_err());
    }

    #[test]
    fn normalize_routes_empty_is_ok() {
        assert_eq!(normalize_routes(vec![]).unwrap(), Vec::<String>::new());
        assert_eq!(
            normalize_routes(vec!["".to_string(), "   ".to_string()]).unwrap(),
            Vec::<String>::new()
        );
    }
}

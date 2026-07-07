//! REST surface for the `roomler-tunnel` subsystem.
//!
//! Mirrors `crates/api/src/routes/remote_control.rs` for the
//! enrollment flow. The tunnel client (a laptop running
//! `roomler-tunnel`) is enrolled in the same two-step shape as a
//! remote-control agent: admin issues a single-use `TunnelEnrollment`
//! token, then the operator runs `roomler-tunnel enroll` which
//! exchanges it for a long-lived `TunnelClient` token.
//!
//! Signalling, forwarding, audit + policy CRUD land in T2.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use bson::oid::ObjectId;
use roomler_ai_remote_control::models::{
    AgentStatus, DestinationRule, OsKind, PolicySubject, PolicyTarget,
};
use roomler_ai_services::dao::base::PaginationParams;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

/// 10 minutes — same TTL as the agent enrollment token (see plan
/// §"Security model"; matches `ENROLLMENT_TTL_SECS` in
/// `routes/remote_control.rs`).
const TUNNEL_ENROLLMENT_TTL_SECS: u64 = 600;

// ────────────────────────────────────────────────────────────────────────────
// Tunnel-client enrollment
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TunnelEnrollmentTokenResponse {
    pub enrollment_token: String,
    pub expires_in: u64,
    pub jti: String,
}

/// POST /api/tenant/{tenant_id}/tunnel-client/enroll-token —
/// admin/member issues a single-use token the operator pastes into
/// `roomler-tunnel enroll`. Mirrors `issue_enrollment_token` for
/// agents.
pub async fn issue_tunnel_enrollment_token(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<TunnelEnrollmentTokenResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let (token, jti) =
        state
            .auth
            .issue_tunnel_enrollment_token(auth.user_id, tid, TUNNEL_ENROLLMENT_TTL_SECS)?;

    Ok(Json(TunnelEnrollmentTokenResponse {
        enrollment_token: token,
        expires_in: TUNNEL_ENROLLMENT_TTL_SECS,
        jti,
    }))
}

#[derive(Debug, Deserialize)]
pub struct TunnelEnrollRequest {
    pub enrollment_token: String,
    pub machine_id: String,
    pub machine_name: String,
    pub os: OsKind,
    pub client_version: String,
}

#[derive(Debug, Serialize)]
pub struct TunnelEnrollResponse {
    pub tunnel_client_id: String,
    pub tenant_id: String,
    pub tunnel_client_token: String,
}

/// POST /api/tunnel-client/enroll — public (no user JWT);
/// authenticates via the tunnel-enrollment token and returns a
/// long-lived TunnelClient JWT. Rehydrates an existing
/// `(tenant_id, machine_id)` row if one is found (covers soft-deleted
/// tombstones too — see [`TunnelClientDao::find_by_tenant_and_machine`]).
///
/// `owner_user_id` is taken from the enrollment claim's `sub` — the
/// admin who issued the token. v1 limitation: this records the
/// admin's identity rather than the operator's. v1.1 can extend the
/// enrollment request with an operator user-id once we have the
/// admin-UI flow to enter it.
pub async fn enroll_tunnel_client(
    State(state): State<AppState>,
    Json(body): Json<TunnelEnrollRequest>,
) -> Result<Json<TunnelEnrollResponse>, ApiError> {
    let claims = state
        .auth
        .verify_tunnel_enrollment_token(&body.enrollment_token)?;
    let tid = ObjectId::parse_str(&claims.tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id claim".to_string()))?;
    let admin_uid = ObjectId::parse_str(&claims.sub)
        .map_err(|_| ApiError::BadRequest("Invalid admin user id claim".to_string()))?;

    let existing = state
        .tunnel_clients
        .find_by_tenant_and_machine(tid, &body.machine_id)
        .await?;
    let client = match existing {
        Some(c) => {
            let id =
                c.id.ok_or_else(|| ApiError::Internal("tunnel client missing _id".to_string()))?;
            state
                .tunnel_clients
                .rehydrate(id, &body.machine_name, body.os, &body.client_version)
                .await?
        }
        None => {
            state
                .tunnel_clients
                .create(
                    tid,
                    admin_uid,
                    body.machine_name,
                    body.machine_id,
                    body.os,
                    body.client_version,
                )
                .await?
        }
    };

    let client_id = client
        .id
        .ok_or_else(|| ApiError::Internal("tunnel client missing _id".to_string()))?;
    let tunnel_client_token = state
        .auth
        .issue_tunnel_client_token(client_id, tid, admin_uid, None)?;

    Ok(Json(TunnelEnrollResponse {
        tunnel_client_id: client_id.to_hex(),
        tenant_id: tid.to_hex(),
        tunnel_client_token,
    }))
}

// ────────────────────────────────────────────────────────────────────────────
// Tunnel-client agent roster (SOCKS mesh routing)
// ────────────────────────────────────────────────────────────────────────────

/// One agent in the tenant, for the SOCKS mesh's name → agent-id routing.
#[derive(Debug, Serialize)]
pub struct TunnelAgentInfo {
    pub agent_id: String,
    pub name: String,
    pub online: bool,
}

/// GET /api/tunnel-client/agents — the tenant's agent roster, so
/// `roomler-tunnel socks5` (mesh mode) can route a CONNECT by friendly agent
/// name instead of the raw 24-hex id. Authenticated by the caller's TunnelClient
/// JWT (`Authorization: Bearer <token>`) and scoped to that token's tenant.
pub async fn list_tenant_agents(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Vec<TunnelAgentInfo>>, ApiError> {
    let token = bearer_token(&headers)
        .ok_or_else(|| ApiError::Unauthorized("missing tunnel-client bearer token".to_string()))?;
    let claims = state
        .auth
        .verify_tunnel_client_token(token)
        .map_err(|_| ApiError::Unauthorized("invalid tunnel-client token".to_string()))?;
    let tid = ObjectId::parse_str(&claims.tenant_id)
        .map_err(|_| ApiError::BadRequest("invalid tenant_id claim".to_string()))?;

    // Tenants have tens of agents; one large page covers them.
    let params = PaginationParams {
        per_page: 500,
        ..Default::default()
    };
    let page = state.agents.list_for_tenant(tid, &params).await?;
    let agents = page
        .items
        .into_iter()
        .map(|a| {
            let online =
                a.id.map(|i| state.rc_hub.is_agent_online(i))
                    .unwrap_or(false);
            TunnelAgentInfo {
                agent_id: a.id.map(|i| i.to_hex()).unwrap_or_default(),
                name: a.name,
                online,
            }
        })
        .collect();
    Ok(Json(agents))
}

/// Extract the `Bearer <token>` value from the `Authorization` header.
fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

// ────────────────────────────────────────────────────────────────────────────
// Tunnel-client listing (admin UI)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TunnelClientResponse {
    pub id: String,
    pub tenant_id: String,
    pub owner_user_id: String,
    pub name: String,
    pub machine_id: String,
    pub os: OsKind,
    pub client_version: String,
    pub status: AgentStatus,
    pub last_seen_at: String,
}

/// GET /api/tenant/{tenant_id}/tunnel-client — paginated list of
/// enrolled tunnel clients for the tenant. Mirrors `list_agents`.
/// T2 extends with the WS-live `is_online` derivation.
pub async fn list_tunnel_clients(
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

    let page = state.tunnel_clients.list_for_tenant(tid, &params).await?;
    let items: Vec<TunnelClientResponse> = page
        .items
        .into_iter()
        .map(|c| TunnelClientResponse {
            id: c.id.map(|i| i.to_hex()).unwrap_or_default(),
            tenant_id: c.tenant_id.to_hex(),
            owner_user_id: c.owner_user_id.to_hex(),
            name: c.name,
            machine_id: c.machine_id,
            os: c.os,
            client_version: c.client_version,
            status: c.status,
            last_seen_at: c.last_seen_at.try_to_rfc3339_string().unwrap_or_default(),
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

// ────────────────────────────────────────────────────────────────────────────
// Tunnel policy CRUD (admin UI)
// ────────────────────────────────────────────────────────────────────────────

/// Wire shape for create + update. `subjects`, `targets`, and
/// `allowlist` re-use the BSON serde representation of the
/// `roomler_ai_remote_control::models` types — adjacently-tagged
/// (`{kind: "...", id: "..."}` / `{kind: "...", value: "..."}`).
#[derive(Debug, Deserialize)]
pub struct TunnelPolicyCreateRequest {
    pub name: String,
    pub subjects: Vec<PolicySubject>,
    pub targets: Vec<PolicyTarget>,
    pub allowlist: Vec<DestinationRule>,
    #[serde(default)]
    pub max_concurrent_flows: Option<u32>,
    #[serde(default)]
    pub max_bytes_per_session: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct TunnelPolicyUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub subjects: Option<Vec<PolicySubject>>,
    #[serde(default)]
    pub targets: Option<Vec<PolicyTarget>>,
    #[serde(default)]
    pub allowlist: Option<Vec<DestinationRule>>,
    /// `Some(None)` clears the ceiling; `Some(Some(v))` sets it;
    /// absent leaves it unchanged. Two-level Option matches the
    /// DAO's three-way intent. JSON wire: `null` to clear,
    /// integer to set, omit field to leave alone.
    #[serde(default, deserialize_with = "deserialize_clearable_u32")]
    pub max_concurrent_flows: Option<Option<u32>>,
    #[serde(default, deserialize_with = "deserialize_clearable_u64")]
    pub max_bytes_per_session: Option<Option<u64>>,
}

fn deserialize_clearable_u32<'de, D>(de: D) -> Result<Option<Option<u32>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<u32> = Option::deserialize(de)?;
    Ok(Some(v))
}

fn deserialize_clearable_u64<'de, D>(de: D) -> Result<Option<Option<u64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<u64> = Option::deserialize(de)?;
    Ok(Some(v))
}

#[derive(Debug, Serialize)]
pub struct TunnelPolicyResponse {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub subjects: Vec<PolicySubject>,
    pub targets: Vec<PolicyTarget>,
    pub allowlist: Vec<DestinationRule>,
    pub max_concurrent_flows: Option<u32>,
    pub max_bytes_per_session: Option<u64>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<roomler_ai_remote_control::models::TunnelPolicy> for TunnelPolicyResponse {
    fn from(p: roomler_ai_remote_control::models::TunnelPolicy) -> Self {
        Self {
            id: p.id.map(|i| i.to_hex()).unwrap_or_default(),
            tenant_id: p.tenant_id.to_hex(),
            name: p.name,
            subjects: p.subjects,
            targets: p.targets,
            allowlist: p.allowlist,
            max_concurrent_flows: p.max_concurrent_flows,
            max_bytes_per_session: p.max_bytes_per_session,
            created_at: p.created_at.try_to_rfc3339_string().unwrap_or_default(),
            updated_at: p.updated_at.try_to_rfc3339_string().unwrap_or_default(),
        }
    }
}

/// Reject empty / malformed inputs before they reach the DAO. Surface
/// a clean 400 rather than relying on Mongo to bounce malformed BSON.
fn validate_policy_input(
    name: &str,
    subjects: &[PolicySubject],
    targets: &[PolicyTarget],
    allowlist: &[DestinationRule],
) -> Result<(), ApiError> {
    if name.trim().is_empty() {
        return Err(ApiError::BadRequest("name must not be empty".into()));
    }
    if subjects.is_empty() {
        return Err(ApiError::BadRequest(
            "subjects must contain at least one entry (use AllUsers for catch-all)".into(),
        ));
    }
    if targets.is_empty() {
        return Err(ApiError::BadRequest(
            "targets must contain at least one entry (use AllAgents for catch-all)".into(),
        ));
    }
    if allowlist.is_empty() {
        return Err(ApiError::BadRequest(
            "allowlist must contain at least one DestinationRule".into(),
        ));
    }
    for r in allowlist {
        if r.port_range.low == 0 || r.port_range.high < r.port_range.low {
            return Err(ApiError::BadRequest(format!(
                "invalid port_range: low={}, high={}",
                r.port_range.low, r.port_range.high
            )));
        }
    }
    Ok(())
}

/// POST /api/tenant/{tenant_id}/tunnel-policy — create a new policy.
pub async fn create_tunnel_policy(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<TunnelPolicyCreateRequest>,
) -> Result<(StatusCode, Json<TunnelPolicyResponse>), ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }
    validate_policy_input(&body.name, &body.subjects, &body.targets, &body.allowlist)?;

    let policy = state
        .tunnel_policies
        .create(
            tid,
            body.name,
            body.subjects,
            body.targets,
            body.allowlist,
            body.max_concurrent_flows,
            body.max_bytes_per_session,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(policy.into())))
}

/// GET /api/tenant/{tenant_id}/tunnel-policy — paginated list of live
/// policies for the tenant. Soft-deleted rows are excluded.
pub async fn list_tunnel_policies(
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
    let page = state.tunnel_policies.list_for_tenant(tid, &params).await?;
    let items: Vec<TunnelPolicyResponse> = page.items.into_iter().map(Into::into).collect();
    Ok(Json(serde_json::json!({
        "items": items,
        "total": page.total,
        "page": page.page,
        "per_page": page.per_page,
        "total_pages": page.total_pages,
    })))
}

/// GET /api/tenant/{tenant_id}/tunnel-policy/{policy_id} — fetch one.
pub async fn get_tunnel_policy(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, policy_id)): Path<(String, String)>,
) -> Result<Json<TunnelPolicyResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let pid = ObjectId::parse_str(&policy_id)
        .map_err(|_| ApiError::BadRequest("Invalid policy_id".to_string()))?;
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }
    let policy = state.tunnel_policies.find_in_tenant(tid, pid).await?;
    Ok(Json(policy.into()))
}

/// PUT /api/tenant/{tenant_id}/tunnel-policy/{policy_id} — partial
/// update. Any field omitted from the body stays unchanged.
pub async fn update_tunnel_policy(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, policy_id)): Path<(String, String)>,
    Json(body): Json<TunnelPolicyUpdateRequest>,
) -> Result<Json<TunnelPolicyResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let pid = ObjectId::parse_str(&policy_id)
        .map_err(|_| ApiError::BadRequest("Invalid policy_id".to_string()))?;
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }
    // Validate only the fields that ARE being updated.
    if let Some(n) = &body.name
        && n.trim().is_empty()
    {
        return Err(ApiError::BadRequest("name must not be empty".into()));
    }
    if let Some(s) = &body.subjects
        && s.is_empty()
    {
        return Err(ApiError::BadRequest(
            "subjects must contain at least one entry".into(),
        ));
    }
    if let Some(t) = &body.targets
        && t.is_empty()
    {
        return Err(ApiError::BadRequest(
            "targets must contain at least one entry".into(),
        ));
    }
    if let Some(a) = &body.allowlist {
        if a.is_empty() {
            return Err(ApiError::BadRequest(
                "allowlist must contain at least one DestinationRule".into(),
            ));
        }
        for r in a {
            if r.port_range.low == 0 || r.port_range.high < r.port_range.low {
                return Err(ApiError::BadRequest(format!(
                    "invalid port_range: low={}, high={}",
                    r.port_range.low, r.port_range.high
                )));
            }
        }
    }
    let changed = state
        .tunnel_policies
        .update(
            tid,
            pid,
            body.name,
            body.subjects,
            body.targets,
            body.allowlist,
            body.max_concurrent_flows,
            body.max_bytes_per_session,
        )
        .await?;
    if !changed {
        return Err(ApiError::NotFound("Tunnel policy not found".into()));
    }
    let policy = state.tunnel_policies.find_in_tenant(tid, pid).await?;
    Ok(Json(policy.into()))
}

/// DELETE /api/tenant/{tenant_id}/tunnel-policy/{policy_id} —
/// soft-delete. Existing flows on live sessions are not killed; new
/// `TcpForwardRequest`s using only this policy will start being
/// denied at the next policy fetch (every request — there's no
/// cache, see `list_active_for_tenant`).
pub async fn delete_tunnel_policy(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, policy_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let pid = ObjectId::parse_str(&policy_id)
        .map_err(|_| ApiError::BadRequest("Invalid policy_id".to_string()))?;
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }
    let deleted = state.tunnel_policies.soft_delete(tid, pid).await?;
    if !deleted {
        return Err(ApiError::NotFound("Tunnel policy not found".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

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
};
use bson::oid::ObjectId;
use roomler_ai_remote_control::models::{AgentStatus, OsKind};
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

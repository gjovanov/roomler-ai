use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use crate::{
    error::ApiError,
    extractors::auth::{AuthUser, OptionalAuthUser},
    state::AppState,
};
use roomler2_db::models::role::permissions;
use roomler2_services::dao::{
    base::PaginationParams,
    invite::CreateInviteParams,
};

// ─── Response types ──────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct InviteInfoResponse {
    pub code: String,
    pub tenant_name: String,
    pub tenant_slug: String,
    pub inviter_name: String,
    pub is_valid: bool,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub already_member: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct InviteResponse {
    pub id: String,
    pub code: String,
    pub tenant_id: String,
    pub inviter_id: String,
    pub target_email: Option<String>,
    pub max_uses: Option<u32>,
    pub use_count: u32,
    pub status: String,
    pub assign_role_ids: Vec<String>,
    pub expires_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct AcceptInviteResponse {
    pub tenant_id: String,
    pub tenant_name: String,
    pub tenant_slug: String,
}

// ─── Request types ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateInviteRequest {
    pub target_email: Option<String>,
    pub max_uses: Option<u32>,
    pub expires_in_hours: Option<u64>,
    #[serde(default)]
    pub assign_role_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddMemberRequest {
    pub user_id: String,
    #[serde(default)]
    pub role_ids: Vec<String>,
}

// ─── Public handlers ────────────────────────────────────────────

/// GET /api/invite/{code} — public invite info
pub async fn get_invite_info(
    State(state): State<AppState>,
    optional_auth: OptionalAuthUser,
    Path(code): Path<String>,
) -> Result<Json<InviteInfoResponse>, ApiError> {
    let invite = state
        .invites
        .find_by_code(&code)
        .await
        .map_err(|_| ApiError::NotFound("Invite not found".to_string()))?;

    let is_valid = state.invites.validate(&invite).is_ok();

    let tenant = state.tenants.base.find_by_id(invite.tenant_id).await?;
    let inviter = state.users.base.find_by_id(invite.inviter_id).await?;

    let already_member = if let Some(auth) = &optional_auth.0 {
        Some(state.tenants.is_member(invite.tenant_id, auth.user_id).await?)
    } else {
        None
    };

    let status = format!("{:?}", invite.status).to_lowercase();

    Ok(Json(InviteInfoResponse {
        code: invite.code,
        tenant_name: tenant.name,
        tenant_slug: tenant.slug,
        inviter_name: inviter.display_name,
        is_valid,
        status,
        already_member,
    }))
}

/// POST /api/invite/{code}/accept — accept invite (requires auth)
pub async fn accept_invite(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(code): Path<String>,
) -> Result<Json<AcceptInviteResponse>, ApiError> {
    let invite = state.invites.find_by_code(&code).await?;

    // Validate the invite is still usable
    state
        .invites
        .validate(&invite)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // Check target_email constraint
    if let Some(ref target_email) = invite.target_email {
        if target_email != &auth.email {
            return Err(ApiError::Forbidden(
                "This invite is for a different email address".to_string(),
            ));
        }
    }

    // Check not already a member
    if state.tenants.is_member(invite.tenant_id, auth.user_id).await? {
        return Err(ApiError::Conflict("Already a member of this tenant".to_string()));
    }

    // Determine roles to assign (default to "member" role if none specified)
    let role_ids = if invite.assign_role_ids.is_empty() {
        let member_role = state
            .tenants
            .get_role_by_name(invite.tenant_id, "member")
            .await?;
        vec![member_role.id.unwrap()]
    } else {
        invite.assign_role_ids.clone()
    };

    // Add the user to the tenant
    state
        .tenants
        .add_member(invite.tenant_id, auth.user_id, role_ids, Some(invite.inviter_id))
        .await?;

    // Atomically increment the use count
    state
        .invites
        .increment_use_count(invite.id.unwrap())
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let tenant = state.tenants.base.find_by_id(invite.tenant_id).await?;

    Ok(Json(AcceptInviteResponse {
        tenant_id: tenant.id.unwrap().to_hex(),
        tenant_name: tenant.name,
        tenant_slug: tenant.slug,
    }))
}

// ─── Tenant-scoped handlers (require INVITE_MEMBERS) ───────────

/// GET /api/tenant/{tenant_id}/invite — list tenant invites
pub async fn list_invites(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = parse_oid(&tenant_id)?;
    require_invite_permission(&state, tid, auth.user_id).await?;

    let result = state.invites.list_by_tenant(tid, &params).await?;

    let items: Vec<InviteResponse> = result
        .items
        .into_iter()
        .map(invite_to_response)
        .collect();

    Ok(Json(serde_json::json!({
        "items": items,
        "total": result.total,
        "page": result.page,
        "per_page": result.per_page,
        "total_pages": result.total_pages,
    })))
}

/// POST /api/tenant/{tenant_id}/invite — create invite
pub async fn create_invite(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<CreateInviteRequest>,
) -> Result<(StatusCode, Json<InviteResponse>), ApiError> {
    let tid = parse_oid(&tenant_id)?;
    require_invite_permission(&state, tid, auth.user_id).await?;

    let assign_role_ids: Vec<ObjectId> = body
        .assign_role_ids
        .iter()
        .map(|s| parse_oid(s))
        .collect::<Result<Vec<_>, _>>()?;

    let expires_in_hours = body.expires_in_hours.or(Some(168)); // default 7 days

    let invite = state
        .invites
        .create(
            tid,
            auth.user_id,
            CreateInviteParams {
                target_email: body.target_email,
                max_uses: body.max_uses,
                expires_in_hours,
                assign_role_ids,
            },
        )
        .await?;

    Ok((StatusCode::CREATED, Json(invite_to_response(invite))))
}

/// DELETE /api/tenant/{tenant_id}/invite/{invite_id} — revoke invite
pub async fn revoke_invite(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, invite_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = parse_oid(&tenant_id)?;
    let iid = parse_oid(&invite_id)?;
    require_invite_permission(&state, tid, auth.user_id).await?;

    state.invites.revoke(iid, tid).await?;

    Ok(Json(serde_json::json!({ "revoked": true })))
}

/// POST /api/tenant/{tenant_id}/member — direct add member
pub async fn add_member(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<AddMemberRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let tid = parse_oid(&tenant_id)?;
    require_invite_permission(&state, tid, auth.user_id).await?;

    let user_id = parse_oid(&body.user_id)?;

    // Check not already a member
    if state.tenants.is_member(tid, user_id).await? {
        return Err(ApiError::Conflict("User is already a member".to_string()));
    }

    let role_ids: Vec<ObjectId> = if body.role_ids.is_empty() {
        let member_role = state.tenants.get_role_by_name(tid, "member").await?;
        vec![member_role.id.unwrap()]
    } else {
        body.role_ids
            .iter()
            .map(|s| parse_oid(s))
            .collect::<Result<Vec<_>, _>>()?
    };

    let member = state
        .tenants
        .add_member(tid, user_id, role_ids, Some(auth.user_id))
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": member.id.unwrap().to_hex(),
            "user_id": member.user_id.to_hex(),
            "tenant_id": member.tenant_id.to_hex(),
        })),
    ))
}

// ─── Helpers ────────────────────────────────────────────────────

fn parse_oid(s: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(s).map_err(|_| ApiError::BadRequest(format!("Invalid ObjectId: {}", s)))
}

async fn require_invite_permission(
    state: &AppState,
    tenant_id: ObjectId,
    user_id: ObjectId,
) -> Result<(), ApiError> {
    let perms = state
        .tenants
        .get_member_permissions(tenant_id, user_id)
        .await?;
    if !permissions::has(perms, permissions::INVITE_MEMBERS) {
        return Err(ApiError::Forbidden(
            "Missing INVITE_MEMBERS permission".to_string(),
        ));
    }
    Ok(())
}

fn invite_to_response(invite: roomler2_db::models::Invite) -> InviteResponse {
    InviteResponse {
        id: invite.id.unwrap().to_hex(),
        code: invite.code,
        tenant_id: invite.tenant_id.to_hex(),
        inviter_id: invite.inviter_id.to_hex(),
        target_email: invite.target_email,
        max_uses: invite.max_uses,
        use_count: invite.use_count,
        status: format!("{:?}", invite.status).to_lowercase(),
        assign_role_ids: invite.assign_role_ids.iter().map(|id| id.to_hex()).collect(),
        expires_at: invite
            .expires_at
            .map(|d| d.try_to_rfc3339_string().unwrap_or_default()),
        created_at: invite
            .created_at
            .try_to_rfc3339_string()
            .unwrap_or_default(),
    }
}

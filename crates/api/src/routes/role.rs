use axum::{Json, extract::{Path, State}};
use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

#[derive(Debug, Serialize)]
pub struct RoleResponse {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub description: Option<String>,
    pub color: Option<u32>,
    pub position: u32,
    pub permissions: u64,
    pub is_default: bool,
    pub is_managed: bool,
    pub is_mentionable: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateRoleRequest {
    pub name: String,
    pub description: Option<String>,
    pub color: Option<u32>,
    pub permissions: Option<u64>,
    pub position: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRoleRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub color: Option<u32>,
    pub permissions: Option<u64>,
    pub position: Option<u32>,
}

pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<Vec<RoleResponse>>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let roles = state.roles.find_for_tenant(tid).await?;
    let response: Vec<RoleResponse> = roles.into_iter().map(to_response).collect();

    Ok(Json(response))
}

pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<CreateRoleRequest>,
) -> Result<Json<RoleResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let role = state
        .roles
        .create(
            tid,
            body.name,
            body.description,
            body.color,
            body.permissions.unwrap_or(0),
            false,
            false,
            body.position.unwrap_or(100),
        )
        .await?;

    Ok(Json(to_response(role)))
}

pub async fn update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, role_id)): Path<(String, String)>,
    Json(body): Json<UpdateRoleRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&role_id)
        .map_err(|_| ApiError::BadRequest("Invalid role_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state
        .roles
        .update(rid, tid, body.name, body.description, body.color, body.permissions, body.position)
        .await?;

    Ok(Json(serde_json::json!({ "updated": true })))
}

pub async fn delete(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, role_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&role_id)
        .map_err(|_| ApiError::BadRequest("Invalid role_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state.roles.delete(rid, tid).await?;

    Ok(Json(serde_json::json!({ "deleted": true })))
}

pub async fn assign(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, role_id, user_id)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&role_id)
        .map_err(|_| ApiError::BadRequest("Invalid role_id".to_string()))?;
    let uid = ObjectId::parse_str(&user_id)
        .map_err(|_| ApiError::BadRequest("Invalid user_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state.tenants.assign_role(tid, uid, rid).await?;

    Ok(Json(serde_json::json!({ "assigned": true })))
}

pub async fn unassign(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, role_id, user_id)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&role_id)
        .map_err(|_| ApiError::BadRequest("Invalid role_id".to_string()))?;
    let uid = ObjectId::parse_str(&user_id)
        .map_err(|_| ApiError::BadRequest("Invalid user_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state.tenants.remove_role(tid, uid, rid).await?;

    Ok(Json(serde_json::json!({ "removed": true })))
}

fn to_response(r: roomler2_db::models::Role) -> RoleResponse {
    RoleResponse {
        id: r.id.unwrap().to_hex(),
        tenant_id: r.tenant_id.to_hex(),
        name: r.name,
        description: r.description,
        color: r.color,
        position: r.position,
        permissions: r.permissions,
        is_default: r.is_default,
        is_managed: r.is_managed,
        is_mentionable: r.is_mentionable,
    }
}

use axum::{Json, extract::{Path, Query, State}};
use bson::{doc, oid::ObjectId};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler2_services::dao::base::PaginationParams;

#[derive(Debug, Serialize)]
pub struct MemberResponse {
    pub id: String,
    pub user_id: String,
    pub nickname: Option<String>,
    pub role_ids: Vec<String>,
    pub joined_at: String,
}

#[derive(Debug, Serialize)]
pub struct ProfileResponse {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub avatar: Option<String>,
    pub bio: Option<String>,
    pub presence: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProfileRequest {
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub avatar: Option<String>,
    pub locale: Option<String>,
    pub timezone: Option<String>,
}

pub async fn list_members(
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

    let result = state
        .tenants
        .members
        .find_paginated(
            doc! { "tenant_id": tid },
            Some(doc! { "joined_at": 1 }),
            &params,
        )
        .await?;

    let items: Vec<MemberResponse> = result
        .items
        .into_iter()
        .map(|m| MemberResponse {
            id: m.id.unwrap().to_hex(),
            user_id: m.user_id.to_hex(),
            nickname: m.nickname,
            role_ids: m.role_ids.iter().map(|r| r.to_hex()).collect(),
            joined_at: m.joined_at.try_to_rfc3339_string().unwrap_or_default(),
        })
        .collect();

    Ok(Json(serde_json::json!({
        "items": items,
        "total": result.total,
        "page": result.page,
        "per_page": result.per_page,
        "total_pages": result.total_pages,
    })))
}

pub async fn get_profile(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(user_id): Path<String>,
) -> Result<Json<ProfileResponse>, ApiError> {
    let uid = ObjectId::parse_str(&user_id)
        .map_err(|_| ApiError::BadRequest("Invalid user_id".to_string()))?;

    let user = state.users.base.find_by_id(uid).await?;

    Ok(Json(ProfileResponse {
        id: user.id.unwrap().to_hex(),
        username: user.username,
        display_name: user.display_name,
        avatar: user.avatar,
        bio: user.bio,
        presence: format!("{:?}", user.presence).to_lowercase(),
        created_at: user.created_at.try_to_rfc3339_string().unwrap_or_default(),
    }))
}

pub async fn update_profile(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<UpdateProfileRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .users
        .update_profile(
            auth.user_id,
            body.display_name,
            body.bio,
            body.avatar,
            body.locale,
            body.timezone,
        )
        .await?;

    Ok(Json(serde_json::json!({ "updated": true })))
}

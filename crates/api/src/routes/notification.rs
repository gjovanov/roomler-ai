use axum::{Json, extract::{Path, Query, State}};
use bson::oid::ObjectId;
use serde::Serialize;

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler2_services::dao::base::PaginationParams;

#[derive(Debug, Serialize)]
pub struct NotificationResponse {
    pub id: String,
    pub notification_type: String,
    pub title: String,
    pub body: String,
    pub link: Option<String>,
    pub is_read: bool,
    pub created_at: String,
}

pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let result = state.notifications.find_for_user(auth.user_id, &params).await?;

    let items: Vec<NotificationResponse> = result
        .items
        .into_iter()
        .map(to_response)
        .collect();

    Ok(Json(serde_json::json!({
        "items": items,
        "total": result.total,
        "page": result.page,
        "per_page": result.per_page,
        "total_pages": result.total_pages,
    })))
}

pub async fn unread(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let result = state.notifications.find_unread_for_user(auth.user_id, &params).await?;

    let items: Vec<NotificationResponse> = result
        .items
        .into_iter()
        .map(to_response)
        .collect();

    Ok(Json(serde_json::json!({
        "items": items,
        "total": result.total,
        "page": result.page,
        "per_page": result.per_page,
        "total_pages": result.total_pages,
    })))
}

pub async fn unread_count(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    let count = state.notifications.unread_count(auth.user_id).await?;
    Ok(Json(serde_json::json!({ "count": count })))
}

pub async fn mark_read(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(notification_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let nid = ObjectId::parse_str(&notification_id)
        .map_err(|_| ApiError::BadRequest("Invalid notification_id".to_string()))?;

    state.notifications.mark_read(nid, auth.user_id).await?;

    Ok(Json(serde_json::json!({ "read": true })))
}

pub async fn mark_all_read(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    let count = state.notifications.mark_all_read(auth.user_id).await?;
    Ok(Json(serde_json::json!({ "marked": count })))
}

fn to_response(n: roomler2_db::models::Notification) -> NotificationResponse {
    NotificationResponse {
        id: n.id.unwrap().to_hex(),
        notification_type: format!("{:?}", n.notification_type).to_lowercase(),
        title: n.title,
        body: n.body,
        link: n.link,
        is_read: n.is_read,
        created_at: n.created_at.try_to_rfc3339_string().unwrap_or_default(),
    }
}

use axum::{Json, extract::{Path, Query, State}};
use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler2_services::dao::base::PaginationParams;

#[derive(Debug, Deserialize)]
pub struct CreateMessageRequest {
    pub content: String,
    pub thread_id: Option<String>,
    pub referenced_message_id: Option<String>,
    pub nonce: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMessageRequest {
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub id: String,
    pub channel_id: String,
    pub author_id: String,
    pub content: String,
    pub message_type: String,
    pub is_pinned: bool,
    pub is_edited: bool,
    pub thread_id: Option<String>,
    pub referenced_message_id: Option<String>,
    pub reaction_summary: Vec<ReactionSummaryResponse>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct ReactionSummaryResponse {
    pub emoji: String,
    pub count: u32,
}

pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, channel_id)): Path<(String, String)>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let cid = ObjectId::parse_str(&channel_id)
        .map_err(|_| ApiError::BadRequest("Invalid channel_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let result = state.messages.find_in_channel(cid, &params).await?;

    let items: Vec<MessageResponse> = result
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

pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, channel_id)): Path<(String, String)>,
    Json(body): Json<CreateMessageRequest>,
) -> Result<Json<MessageResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let cid = ObjectId::parse_str(&channel_id)
        .map_err(|_| ApiError::BadRequest("Invalid channel_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let thread_id = body
        .thread_id
        .as_ref()
        .map(|t| ObjectId::parse_str(t))
        .transpose()
        .map_err(|_| ApiError::BadRequest("Invalid thread_id".to_string()))?;

    let ref_msg_id = body
        .referenced_message_id
        .as_ref()
        .map(|r| ObjectId::parse_str(r))
        .transpose()
        .map_err(|_| ApiError::BadRequest("Invalid referenced_message_id".to_string()))?;

    let message = state
        .messages
        .create(
            tid,
            cid,
            auth.user_id,
            body.content,
            thread_id,
            ref_msg_id,
            body.nonce,
        )
        .await?;

    // Broadcast via WebSocket to channel members (exclude sender to avoid duplicates)
    let response = to_response(message);
    let member_ids: Vec<ObjectId> = state
        .channels
        .find_member_user_ids(cid)
        .await?
        .into_iter()
        .filter(|id| *id != auth.user_id)
        .collect();
    let event = serde_json::json!({
        "type": "message:create",
        "data": &response,
    });
    crate::ws::dispatcher::broadcast(&state.ws_storage, &member_ids, &event).await;

    Ok(Json(response))
}

pub async fn update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, channel_id, message_id)): Path<(String, String, String)>,
    Json(body): Json<UpdateMessageRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let cid = ObjectId::parse_str(&channel_id)
        .map_err(|_| ApiError::BadRequest("Invalid channel_id".to_string()))?;
    let mid = ObjectId::parse_str(&message_id)
        .map_err(|_| ApiError::BadRequest("Invalid message_id".to_string()))?;

    state
        .messages
        .update_content(tid, mid, auth.user_id, body.content.clone())
        .await?;

    // Broadcast update to channel members
    let member_ids = state.channels.find_member_user_ids(cid).await?;
    let event = serde_json::json!({
        "type": "message:update",
        "data": {
            "message_id": message_id,
            "channel_id": channel_id,
            "content": body.content,
            "user_id": auth.user_id.to_hex(),
        }
    });
    crate::ws::dispatcher::broadcast(&state.ws_storage, &member_ids, &event).await;

    Ok(Json(serde_json::json!({ "updated": true })))
}

pub async fn delete(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path((tenant_id, channel_id, message_id)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let cid = ObjectId::parse_str(&channel_id)
        .map_err(|_| ApiError::BadRequest("Invalid channel_id".to_string()))?;
    let mid = ObjectId::parse_str(&message_id)
        .map_err(|_| ApiError::BadRequest("Invalid message_id".to_string()))?;

    state
        .messages
        .base
        .soft_delete_in_tenant(tid, mid)
        .await?;

    // Broadcast delete to channel members
    let member_ids = state.channels.find_member_user_ids(cid).await?;
    let event = serde_json::json!({
        "type": "message:delete",
        "data": {
            "message_id": message_id,
            "channel_id": channel_id,
        }
    });
    crate::ws::dispatcher::broadcast(&state.ws_storage, &member_ids, &event).await;

    Ok(Json(serde_json::json!({ "deleted": true })))
}

pub async fn pinned(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, channel_id)): Path<(String, String)>,
) -> Result<Json<Vec<MessageResponse>>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let cid = ObjectId::parse_str(&channel_id)
        .map_err(|_| ApiError::BadRequest("Invalid channel_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let messages = state.messages.find_pinned(cid).await?;
    let response: Vec<MessageResponse> = messages.into_iter().map(to_response).collect();

    Ok(Json(response))
}

#[derive(Debug, Deserialize)]
pub struct TogglePinRequest {
    pub pinned: bool,
}

pub async fn toggle_pin(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, channel_id, message_id)): Path<(String, String, String)>,
    Json(body): Json<TogglePinRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let cid = ObjectId::parse_str(&channel_id)
        .map_err(|_| ApiError::BadRequest("Invalid channel_id".to_string()))?;
    let mid = ObjectId::parse_str(&message_id)
        .map_err(|_| ApiError::BadRequest("Invalid message_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state.messages.toggle_pin(tid, mid, body.pinned).await?;

    // Broadcast pin event to channel members
    let member_ids = state.channels.find_member_user_ids(cid).await?;
    let event = serde_json::json!({
        "type": if body.pinned { "message:pin" } else { "message:unpin" },
        "data": {
            "message_id": message_id,
            "channel_id": channel_id,
            "pinned": body.pinned,
        }
    });
    crate::ws::dispatcher::broadcast(&state.ws_storage, &member_ids, &event).await;

    Ok(Json(serde_json::json!({ "pinned": body.pinned })))
}

pub async fn thread_replies(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, _channel_id, message_id)): Path<(String, String, String)>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let mid = ObjectId::parse_str(&message_id)
        .map_err(|_| ApiError::BadRequest("Invalid message_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let result = state.messages.find_thread_replies(mid, &params).await?;

    let items: Vec<MessageResponse> = result
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

fn to_response(m: roomler2_db::models::Message) -> MessageResponse {
    MessageResponse {
        id: m.id.unwrap().to_hex(),
        channel_id: m.channel_id.to_hex(),
        author_id: m.author_id.to_hex(),
        content: m.content,
        message_type: format!("{:?}", m.message_type),
        is_pinned: m.is_pinned,
        is_edited: m.is_edited,
        thread_id: m.thread_id.map(|t| t.to_hex()),
        referenced_message_id: m.referenced_message_id.map(|r| r.to_hex()),
        reaction_summary: m
            .reaction_summary
            .into_iter()
            .map(|r| ReactionSummaryResponse {
                emoji: r.emoji,
                count: r.count,
            })
            .collect(),
        created_at: m.created_at.try_to_rfc3339_string().unwrap_or_default(),
    }
}

use axum::{Json, extract::{Path, State}};
use bson::oid::ObjectId;
use serde::Deserialize;

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

#[derive(Debug, Deserialize)]
pub struct AddReactionRequest {
    pub emoji: String,
}

pub async fn add(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id, message_id)): Path<(String, String, String)>,
    Json(body): Json<AddReactionRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;
    let mid = ObjectId::parse_str(&message_id)
        .map_err(|_| ApiError::BadRequest("Invalid message_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let reaction = state
        .reactions
        .add_and_update_summary(&state.messages, tid, rid, mid, auth.user_id, body.emoji)
        .await?;

    let member_ids = state.rooms.find_member_user_ids(rid).await?;
    let event = serde_json::json!({
        "type": "message:reaction",
        "data": {
            "action": "add",
            "message_id": message_id,
            "room_id": room_id,
            "user_id": auth.user_id.to_hex(),
            "emoji": reaction.emoji.value,
        }
    });
    crate::ws::dispatcher::broadcast(&state.ws_storage, &member_ids, &event).await;

    Ok(Json(serde_json::json!({ "added": true })))
}

pub async fn remove(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id, message_id, emoji)): Path<(String, String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let mid = ObjectId::parse_str(&message_id)
        .map_err(|_| ApiError::BadRequest("Invalid message_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let removed = state
        .reactions
        .remove_and_update_summary(&state.messages, mid, auth.user_id, &emoji)
        .await?;

    if removed {
        let rid = ObjectId::parse_str(&room_id)
            .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;
        let member_ids = state.rooms.find_member_user_ids(rid).await?;
        let event = serde_json::json!({
            "type": "message:reaction",
            "data": {
                "action": "remove",
                "message_id": message_id,
                "room_id": room_id,
                "user_id": auth.user_id.to_hex(),
                "emoji": emoji,
            }
        });
        crate::ws::dispatcher::broadcast(&state.ws_storage, &member_ids, &event).await;
    }

    Ok(Json(serde_json::json!({ "removed": removed })))
}

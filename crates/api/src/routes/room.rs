use axum::{Json, extract::{Path, Query, State}};
use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler2_db::models::MediaSettings;
use roomler2_services::dao::base::PaginationParams;

#[derive(Debug, Deserialize)]
pub struct CreateRoomRequest {
    pub name: String,
    pub parent_id: Option<String>,
    #[serde(default)]
    pub is_open: bool,
    pub media_settings: Option<MediaSettings>,
}

#[derive(Debug, Serialize)]
pub struct RoomResponse {
    pub id: String,
    pub name: String,
    pub path: String,
    pub parent_id: Option<String>,
    pub is_open: bool,
    pub member_count: u32,
    pub message_count: u64,
    pub has_media: bool,
    pub conference_status: Option<String>,
    pub meeting_code: Option<String>,
    pub participant_count: u32,
}

pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<Vec<RoomResponse>>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let rooms = state.rooms.find_by_tenant(tid).await?;
    let response: Vec<RoomResponse> = rooms.into_iter().map(to_response).collect();

    Ok(Json(response))
}

pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<CreateRoomRequest>,
) -> Result<Json<RoomResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let parent_id = body
        .parent_id
        .as_ref()
        .map(ObjectId::parse_str)
        .transpose()
        .map_err(|_| ApiError::BadRequest("Invalid parent_id".to_string()))?;

    let room = state
        .rooms
        .create(
            tid,
            body.name,
            parent_id,
            auth.user_id,
            body.is_open,
            body.media_settings,
            None,
        )
        .await?;

    Ok(Json(to_response(room)))
}

pub async fn join(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    state.rooms.join(tid, rid, auth.user_id).await?;

    Ok(Json(serde_json::json!({ "joined": true })))
}

pub async fn leave(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    state.rooms.leave(tid, rid, auth.user_id).await?;

    Ok(Json(serde_json::json!({ "left": true })))
}

pub async fn get(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
) -> Result<Json<RoomResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let room = state.rooms.base.find_by_id_in_tenant(tid, rid).await?;

    Ok(Json(to_response(room)))
}

#[derive(Debug, Deserialize)]
pub struct UpdateRoomRequest {
    pub name: Option<String>,
    pub topic: Option<String>,
    pub purpose: Option<String>,
    pub is_open: Option<bool>,
    pub is_archived: Option<bool>,
    pub is_read_only: Option<bool>,
}

pub async fn update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
    Json(body): Json<UpdateRoomRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state
        .rooms
        .update(
            tid,
            rid,
            body.name,
            body.topic,
            body.purpose,
            body.is_open,
            body.is_archived,
            body.is_read_only,
        )
        .await?;

    Ok(Json(serde_json::json!({ "updated": true })))
}

pub async fn delete(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state.rooms.soft_delete(tid, rid).await?;

    Ok(Json(serde_json::json!({ "deleted": true })))
}

pub async fn members(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let result = state.rooms.list_members(rid, &params).await?;

    let items: Vec<serde_json::Value> = result
        .items
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id.unwrap().to_hex(),
                "user_id": m.user_id.map(|u| u.to_hex()),
                "room_id": m.room_id.to_hex(),
                "display_name": m.display_name,
                "joined_at": m.joined_at.try_to_rfc3339_string().unwrap_or_default(),
                "unread_count": m.unread_count,
                "is_muted": m.is_muted,
            })
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

#[derive(Debug, Deserialize)]
pub struct ExploreQuery {
    pub q: String,
}

pub async fn explore(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(query): Query<ExploreQuery>,
) -> Result<Json<Vec<RoomResponse>>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let rooms = state.rooms.explore(tid, &query.q).await?;
    let response: Vec<RoomResponse> = rooms.into_iter().map(to_response).collect();

    Ok(Json(response))
}

// ── Call endpoints ──────────────────────────────────────────────

pub async fn call_start(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state.rooms.start_call(rid).await?;
    let rtp_capabilities = state
        .room_manager
        .create_room(rid)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to create media room: {}", e)))?;

    // Notify all room members about the call
    let member_ids = state.rooms.find_member_user_ids(rid).await.unwrap_or_default();
    if !member_ids.is_empty() {
        let room = state.rooms.base.find_by_id_in_tenant(tid, rid).await.ok();
        let room_name = room.map(|r| r.name).unwrap_or_default();
        let event = serde_json::json!({
            "type": "room:call_started",
            "data": {
                "room_id": rid.to_hex(),
                "room_name": room_name,
                "started_by": auth.user_id.to_hex(),
            }
        });
        crate::ws::dispatcher::broadcast(&state.ws_storage, &member_ids, &event).await;
    }

    Ok(Json(serde_json::json!({
        "started": true,
        "rtp_capabilities": rtp_capabilities,
    })))
}

pub async fn call_join(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let user = state.users.base.find_by_id(auth.user_id).await?;

    let member = state
        .rooms
        .join_participant(tid, rid, auth.user_id, user.display_name, "web".to_string())
        .await?;

    // Notify room members about updated participant count
    let room = state.rooms.base.find_by_id_in_tenant(tid, rid).await.ok();
    let participant_count = room.as_ref().map(|r| r.participant_count).unwrap_or(0);
    let member_ids = state.rooms.find_member_user_ids(rid).await.unwrap_or_default();
    if !member_ids.is_empty() {
        let event = serde_json::json!({
            "type": "room:call_updated",
            "data": {
                "room_id": rid.to_hex(),
                "participant_count": participant_count,
                "conference_status": "InProgress",
            }
        });
        crate::ws::dispatcher::broadcast(&state.ws_storage, &member_ids, &event).await;
    }

    Ok(Json(serde_json::json!({
        "member_id": member.id.unwrap().to_hex(),
        "joined": true,
    })))
}

pub async fn call_leave(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    // Clean up media before DB leave
    state.room_manager.close_participant_by_user(&rid, &auth.user_id);

    // Broadcast peer_left to remaining participants
    let remaining = state.room_manager.get_participant_user_ids(&rid);
    if !remaining.is_empty() {
        let event = serde_json::json!({
            "type": "media:peer_left",
            "data": {
                "room_id": rid.to_hex(),
                "user_id": auth.user_id.to_hex(),
            }
        });
        crate::ws::dispatcher::broadcast(&state.ws_storage, &remaining, &event).await;
    }

    state.rooms.leave_participant(rid, auth.user_id).await?;
    Ok(Json(serde_json::json!({ "left": true })))
}

pub async fn call_end(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state.rooms.end_call(rid).await?;
    state.room_manager.remove_room(&rid);

    let remaining = state.room_manager.get_participant_user_ids(&rid);
    if !remaining.is_empty() {
        let event = serde_json::json!({
            "type": "media:room_closed",
            "data": { "room_id": rid.to_hex() }
        });
        crate::ws::dispatcher::broadcast(&state.ws_storage, &remaining, &event).await;
    }

    // Notify all room members that the call has ended
    let member_ids = state.rooms.find_member_user_ids(rid).await.unwrap_or_default();
    if !member_ids.is_empty() {
        let event = serde_json::json!({
            "type": "room:call_ended",
            "data": {
                "room_id": rid.to_hex(),
            }
        });
        crate::ws::dispatcher::broadcast(&state.ws_storage, &member_ids, &event).await;
    }

    Ok(Json(serde_json::json!({ "ended": true })))
}

pub async fn participants(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let parts = state.rooms.list_participants(rid).await?;
    let items: Vec<serde_json::Value> = parts
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id.unwrap().to_hex(),
                "user_id": p.user_id.map(|u| u.to_hex()),
                "display_name": p.display_name,
                "role": p.role.as_ref().map(|r| format!("{:?}", r)),
                "is_muted": p.is_muted,
                "is_video_on": p.is_video_on,
                "is_screen_sharing": p.is_screen_sharing,
                "is_hand_raised": p.is_hand_raised,
            })
        })
        .collect();

    Ok(Json(items))
}

// ── Call chat message endpoints ─────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateCallMessageRequest {
    pub content: String,
}

pub async fn call_messages(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let result = state.rooms.find_chat_messages(rid, &params).await?;
    let items: Vec<serde_json::Value> = result
        .items
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id.unwrap().to_hex(),
                "room_id": m.room_id.to_hex(),
                "author_id": m.author_id.to_hex(),
                "display_name": m.display_name,
                "content": m.content,
                "created_at": m.created_at.try_to_rfc3339_string().unwrap_or_default(),
            })
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

pub async fn create_call_message(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
    Json(body): Json<CreateCallMessageRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let user = state.users.base.find_by_id(auth.user_id).await?;
    let msg = state
        .rooms
        .create_chat_message(tid, rid, auth.user_id, user.display_name.clone(), body.content)
        .await?;

    let response = serde_json::json!({
        "id": msg.id.unwrap().to_hex(),
        "room_id": msg.room_id.to_hex(),
        "author_id": msg.author_id.to_hex(),
        "display_name": msg.display_name,
        "content": msg.content,
        "created_at": msg.created_at.try_to_rfc3339_string().unwrap_or_default(),
    });

    // Broadcast to other room members via WS
    let member_ids = state.rooms.find_member_user_ids(rid).await.unwrap_or_default();
    if !member_ids.is_empty() {
        let event = serde_json::json!({
            "type": "call:message:create",
            "data": &response,
        });
        crate::ws::dispatcher::broadcast(&state.ws_storage, &member_ids, &event).await;
    }

    Ok(Json(response))
}

fn to_response(r: roomler2_db::models::Room) -> RoomResponse {
    RoomResponse {
        id: r.id.unwrap().to_hex(),
        name: r.name,
        path: r.path,
        parent_id: r.parent_id.map(|p| p.to_hex()),
        is_open: r.is_open,
        member_count: r.member_count,
        message_count: r.message_count,
        has_media: r.media_settings.is_some(),
        conference_status: r.conference_status.map(|s| format!("{:?}", s)),
        meeting_code: r.meeting_code,
        participant_count: r.participant_count,
    }
}

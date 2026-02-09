use axum::{Json, extract::{Path, Query, State}};
use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler2_db::models::ConferenceType;
use roomler2_services::dao::base::PaginationParams;

#[derive(Debug, Deserialize)]
pub struct CreateConferenceRequest {
    pub subject: String,
    #[serde(default)]
    pub conference_type: ConferenceType,
    pub channel_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ConferenceResponse {
    pub id: String,
    pub subject: String,
    pub status: String,
    pub conference_type: String,
    pub meeting_code: String,
    pub join_url: String,
    pub organizer_id: String,
    pub participant_count: u32,
}

pub async fn list(
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

    let result = state.conferences.list_by_tenant(tid, &params).await?;

    let items: Vec<ConferenceResponse> = result.items.into_iter().map(to_response).collect();

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
    Path(tenant_id): Path<String>,
    Json(body): Json<CreateConferenceRequest>,
) -> Result<Json<ConferenceResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let channel_id = body
        .channel_id
        .as_ref()
        .map(|c| ObjectId::parse_str(c))
        .transpose()
        .map_err(|_| ApiError::BadRequest("Invalid channel_id".to_string()))?;

    let conference = state
        .conferences
        .create(tid, auth.user_id, body.subject, body.conference_type, channel_id)
        .await?;

    Ok(Json(to_response(conference)))
}

pub async fn start(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, conference_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let confid = ObjectId::parse_str(&conference_id)
        .map_err(|_| ApiError::BadRequest("Invalid conference_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state.conferences.start(confid).await?;
    let rtp_capabilities = state
        .room_manager
        .create_room(confid)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to create media room: {}", e)))?;

    Ok(Json(serde_json::json!({
        "started": true,
        "rtp_capabilities": rtp_capabilities,
    })))
}

pub async fn join(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, conference_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let confid = ObjectId::parse_str(&conference_id)
        .map_err(|_| ApiError::BadRequest("Invalid conference_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let user = state.users.base.find_by_id(auth.user_id).await?;

    let participant = state
        .conferences
        .join_participant(tid, confid, auth.user_id, user.display_name, "web".to_string())
        .await?;

    // Transport creation is handled via WS media:join signaling
    Ok(Json(serde_json::json!({
        "participant_id": participant.id.unwrap().to_hex(),
        "joined": true,
    })))
}

pub async fn end(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, conference_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let confid = ObjectId::parse_str(&conference_id)
        .map_err(|_| ApiError::BadRequest("Invalid conference_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state.conferences.end(confid).await?;
    state.room_manager.remove_room(&confid);
    // Broadcast peer_left to all remaining WS connections in the room
    let remaining = state.room_manager.get_participant_user_ids(&confid);
    if !remaining.is_empty() {
        let event = serde_json::json!({
            "type": "media:room_closed",
            "data": { "conference_id": confid.to_hex() }
        });
        crate::ws::dispatcher::broadcast(&state.ws_storage, &remaining, &event).await;
    }

    Ok(Json(serde_json::json!({ "ended": true })))
}

pub async fn get(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, conference_id)): Path<(String, String)>,
) -> Result<Json<ConferenceResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let confid = ObjectId::parse_str(&conference_id)
        .map_err(|_| ApiError::BadRequest("Invalid conference_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let conference = state.conferences.base.find_by_id_in_tenant(tid, confid).await?;
    Ok(Json(to_response(conference)))
}

pub async fn leave(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, conference_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let confid = ObjectId::parse_str(&conference_id)
        .map_err(|_| ApiError::BadRequest("Invalid conference_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    // Clean up media before DB leave (removes all connections for this user)
    state.room_manager.close_participant_by_user(&confid, &auth.user_id);

    // Broadcast peer_left to remaining participants
    let remaining = state.room_manager.get_participant_user_ids(&confid);
    if !remaining.is_empty() {
        let event = serde_json::json!({
            "type": "media:peer_left",
            "data": {
                "conference_id": confid.to_hex(),
                "user_id": auth.user_id.to_hex(),
            }
        });
        crate::ws::dispatcher::broadcast(&state.ws_storage, &remaining, &event).await;
    }

    state.conferences.leave_participant(confid, auth.user_id).await?;
    Ok(Json(serde_json::json!({ "left": true })))
}

pub async fn participants(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, conference_id)): Path<(String, String)>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let confid = ObjectId::parse_str(&conference_id)
        .map_err(|_| ApiError::BadRequest("Invalid conference_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let parts = state.conferences.list_participants(confid).await?;
    let items: Vec<serde_json::Value> = parts
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id.unwrap().to_hex(),
                "user_id": p.user_id.map(|u| u.to_hex()),
                "display_name": p.display_name,
                "role": format!("{:?}", p.role),
                "is_muted": p.is_muted,
                "is_video_on": p.is_video_on,
                "is_screen_sharing": p.is_screen_sharing,
                "is_hand_raised": p.is_hand_raised,
            })
        })
        .collect();

    Ok(Json(items))
}

fn to_response(c: roomler2_db::models::Conference) -> ConferenceResponse {
    ConferenceResponse {
        id: c.id.unwrap().to_hex(),
        subject: c.subject,
        status: format!("{:?}", c.status),
        conference_type: format!("{:?}", c.conference_type),
        meeting_code: c.meeting_code,
        join_url: c.join_url,
        organizer_id: c.organizer_id.to_hex(),
        participant_count: c.participant_count,
    }
}

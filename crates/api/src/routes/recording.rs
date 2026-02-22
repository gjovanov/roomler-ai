use axum::{Json, extract::{Path, Query, State}};
use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler2_services::dao::base::PaginationParams;

#[derive(Debug, Serialize)]
pub struct RecordingResponse {
    pub id: String,
    pub room_id: String,
    pub recording_type: String,
    pub status: String,
    pub content_type: String,
    pub size: u64,
    pub duration: u32,
    pub created_at: String,
}

pub async fn list(
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

    let result = state.recordings.find_by_room(rid, &params).await?;
    let items: Vec<RecordingResponse> = result.items.into_iter().map(to_response).collect();

    Ok(Json(serde_json::json!({
        "items": items,
        "total": result.total,
        "page": result.page,
        "per_page": result.per_page,
        "total_pages": result.total_pages,
    })))
}

#[derive(Debug, Deserialize)]
pub struct CreateRecordingRequest {
    pub recording_type: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
    Json(body): Json<CreateRecordingRequest>,
) -> Result<Json<RecordingResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let recording_type = match body.recording_type.as_deref() {
        Some("audio") => roomler2_db::models::recording::RecordingType::Audio,
        Some("screen_share") => roomler2_db::models::recording::RecordingType::ScreenShare,
        _ => roomler2_db::models::recording::RecordingType::Video,
    };

    let now = bson::DateTime::now();
    let storage_file = roomler2_db::models::recording::StorageFile {
        storage_provider: roomler2_db::models::recording::StorageProvider::Local,
        bucket: "recordings".to_string(),
        key: format!("{}/{}/{}", tid.to_hex(), rid.to_hex(), uuid::Uuid::new_v4()),
        url: String::new(),
        content_type: "video/webm".to_string(),
        size: 0,
        duration: 0,
        resolution: None,
    };

    let recording = state
        .recordings
        .create(tid, rid, recording_type, storage_file, now, now)
        .await?;

    Ok(Json(to_response(recording)))
}

pub async fn delete(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, _room_id, recording_id)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rec_id = ObjectId::parse_str(&recording_id)
        .map_err(|_| ApiError::BadRequest("Invalid recording_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state.recordings.soft_delete(tid, rec_id).await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

fn to_response(r: roomler2_db::models::Recording) -> RecordingResponse {
    RecordingResponse {
        id: r.id.unwrap().to_hex(),
        room_id: r.room_id.to_hex(),
        recording_type: format!("{:?}", r.recording_type),
        status: format!("{:?}", r.status),
        content_type: r.file.content_type,
        size: r.file.size,
        duration: r.file.duration,
        created_at: r.created_at.try_to_rfc3339_string().unwrap_or_default(),
    }
}

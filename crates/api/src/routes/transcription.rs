use axum::{Json, extract::{Path, Query, State}};
use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler2_services::dao::base::PaginationParams;

#[derive(Debug, Serialize)]
pub struct TranscriptionResponse {
    pub id: String,
    pub room_id: String,
    pub status: String,
    pub language: String,
    pub format: String,
    pub summary: Option<String>,
    pub segment_count: usize,
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

    let result = state
        .transcriptions
        .find_by_room(rid, &params)
        .await?;
    let items: Vec<TranscriptionResponse> = result.items.into_iter().map(to_response).collect();

    Ok(Json(serde_json::json!({
        "items": items,
        "total": result.total,
        "page": result.page,
        "per_page": result.per_page,
        "total_pages": result.total_pages,
    })))
}

#[derive(Debug, Deserialize)]
pub struct CreateTranscriptionRequest {
    pub language: Option<String>,
    pub recording_id: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
    Json(body): Json<CreateTranscriptionRequest>,
) -> Result<Json<TranscriptionResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let recording_id = body
        .recording_id
        .as_ref()
        .map(ObjectId::parse_str)
        .transpose()
        .map_err(|_| ApiError::BadRequest("Invalid recording_id".to_string()))?;

    let language = body.language.unwrap_or_else(|| "en-US".to_string());

    let transcription = state
        .transcriptions
        .create(tid, rid, recording_id, language)
        .await?;

    Ok(Json(to_response(transcription)))
}

pub async fn get(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, _room_id, transcription_id)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let trans_id = ObjectId::parse_str(&transcription_id)
        .map_err(|_| ApiError::BadRequest("Invalid transcription_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let t = state.transcriptions.base.find_by_id(trans_id).await?;
    let segments: Vec<serde_json::Value> = t
        .segments
        .iter()
        .map(|s| {
            serde_json::json!({
                "speaker_name": s.speaker_name,
                "start_time": s.start_time,
                "end_time": s.end_time,
                "text": s.text,
                "confidence": s.confidence,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "id": t.id.unwrap().to_hex(),
        "room_id": t.room_id.to_hex(),
        "status": format!("{:?}", t.status),
        "language": t.language,
        "format": format!("{:?}", t.format),
        "segments": segments,
        "summary": t.summary,
        "action_items": t.action_items.iter().map(|a| serde_json::json!({
            "text": a.text,
            "assignee": a.assignee,
            "due_date": a.due_date,
            "completed": a.completed,
        })).collect::<Vec<_>>(),
        "created_at": t.created_at.try_to_rfc3339_string().unwrap_or_default(),
    })))
}

fn to_response(t: roomler2_db::models::Transcription) -> TranscriptionResponse {
    TranscriptionResponse {
        id: t.id.unwrap().to_hex(),
        room_id: t.room_id.to_hex(),
        status: format!("{:?}", t.status),
        language: t.language,
        format: format!("{:?}", t.format),
        summary: t.summary,
        segment_count: t.segments.len(),
        created_at: t.created_at.try_to_rfc3339_string().unwrap_or_default(),
    }
}

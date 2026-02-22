use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, Query, State},
    response::Response,
};
use bson::oid::ObjectId;
use serde::Serialize;
use std::path::PathBuf;
use tokio::io::AsyncReadExt;

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler2_db::models::{FileContext, FileContextType};
use roomler2_services::dao::base::PaginationParams;

#[derive(Debug, Serialize)]
pub struct FileResponse {
    pub id: String,
    pub filename: String,
    pub content_type: String,
    pub size: u64,
    pub url: String,
    pub uploaded_by: String,
    pub created_at: String,
}

fn to_response(f: roomler2_db::models::File) -> FileResponse {
    FileResponse {
        id: f.id.unwrap().to_hex(),
        filename: f.filename,
        content_type: f.content_type,
        size: f.size,
        url: f.url,
        uploaded_by: f.uploaded_by.to_hex(),
        created_at: f.created_at.try_to_rfc3339_string().unwrap_or_default(),
    }
}

/// List files for a room.
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

    let result = state.files.find_by_room(tid, rid, &params).await?;

    let items: Vec<FileResponse> = result.items.into_iter().map(to_response).collect();

    Ok(Json(serde_json::json!({
        "items": items,
        "total": result.total,
        "page": result.page,
        "per_page": result.per_page,
        "total_pages": result.total_pages,
    })))
}

/// Upload a file via multipart form data.
/// Fields: `file` (binary), `room_id` (text)
pub async fn upload(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<FileResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let mut file_data: Option<(String, String, Vec<u8>)> = None;
    let mut room_id_str: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        ApiError::BadRequest(format!("Multipart error: {}", e))
    })? {
        let name = field.name().unwrap_or("").to_string();

        match name.as_str() {
            "file" => {
                let filename = field
                    .file_name()
                    .unwrap_or("unnamed")
                    .to_string();
                let content_type = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::BadRequest(format!("Failed to read file: {}", e)))?;
                file_data = Some((filename, content_type, bytes.to_vec()));
            }
            "room_id" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| ApiError::BadRequest(format!("Failed to read field: {}", e)))?;
                room_id_str = Some(text);
            }
            _ => {}
        }
    }

    let (filename, content_type, bytes) = file_data
        .ok_or_else(|| ApiError::BadRequest("Missing 'file' field".to_string()))?;
    let room_id_val = room_id_str
        .ok_or_else(|| ApiError::BadRequest("Missing 'room_id' field".to_string()))?;
    let rid = ObjectId::parse_str(&room_id_val)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    let size = bytes.len() as u64;

    let upload_dir = upload_dir();
    tokio::fs::create_dir_all(&upload_dir).await.map_err(|e| {
        ApiError::Internal(format!("Failed to create upload dir: {}", e))
    })?;

    let storage_key = format!("{}/{}/{}", tid.to_hex(), rid.to_hex(), uuid::Uuid::new_v4());
    let file_path = upload_dir.join(&storage_key);

    if let Some(parent) = file_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            ApiError::Internal(format!("Failed to create dirs: {}", e))
        })?;
    }

    tokio::fs::write(&file_path, &bytes).await.map_err(|e| {
        ApiError::Internal(format!("Failed to write file: {}", e))
    })?;

    let url = format!("/api/tenant/{}/file/{}", tid.to_hex(), storage_key);

    let context = FileContext {
        context_type: FileContextType::Room,
        entity_id: rid,
        room_id: Some(rid),
    };

    let file = state
        .files
        .create(
            tid,
            auth.user_id,
            context,
            filename,
            content_type,
            size,
            "local".to_string(),
            storage_key,
            url,
        )
        .await?;

    Ok(Json(to_response(file)))
}

pub async fn get(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, file_id)): Path<(String, String)>,
) -> Result<Json<FileResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let fid = ObjectId::parse_str(&file_id)
        .map_err(|_| ApiError::BadRequest("Invalid file_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let file = state.files.base.find_by_id_in_tenant(tid, fid).await?;
    Ok(Json(to_response(file)))
}

pub async fn download(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, file_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let fid = ObjectId::parse_str(&file_id)
        .map_err(|_| ApiError::BadRequest("Invalid file_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let file = state.files.base.find_by_id_in_tenant(tid, fid).await?;
    let file_path = upload_dir().join(&file.storage_key);

    let mut contents = Vec::new();
    let mut f = tokio::fs::File::open(&file_path).await.map_err(|_| {
        ApiError::NotFound("File not found on disk".to_string())
    })?;
    f.read_to_end(&mut contents).await.map_err(|e| {
        ApiError::Internal(format!("Failed to read file: {}", e))
    })?;

    Ok(Response::builder()
        .header("Content-Type", &file.content_type)
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", file.filename),
        )
        .body(Body::from(contents))
        .unwrap())
}

pub async fn delete(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, file_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let fid = ObjectId::parse_str(&file_id)
        .map_err(|_| ApiError::BadRequest("Invalid file_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    state.files.soft_delete(tid, fid).await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// Upload a file attached to a room (with 100MB body limit).
pub async fn upload_room(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, room_id)): Path<(String, String)>,
    mut multipart: Multipart,
) -> Result<Json<FileResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let mut file_data: Option<(String, String, Vec<u8>)> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        ApiError::BadRequest(format!("Multipart error: {}", e))
    })? {
        let name = field.name().unwrap_or("").to_string();

        if name == "file" {
            let filename = field
                .file_name()
                .unwrap_or("unnamed")
                .to_string();
            let content_type = field
                .content_type()
                .unwrap_or("application/octet-stream")
                .to_string();
            let bytes = field
                .bytes()
                .await
                .map_err(|e| ApiError::BadRequest(format!("Failed to read file: {}", e)))?;
            file_data = Some((filename, content_type, bytes.to_vec()));
        }
    }

    let (filename, content_type, bytes) = file_data
        .ok_or_else(|| ApiError::BadRequest("Missing 'file' field".to_string()))?;

    let size = bytes.len() as u64;

    let upload_dir = upload_dir();
    tokio::fs::create_dir_all(&upload_dir).await.map_err(|e| {
        ApiError::Internal(format!("Failed to create upload dir: {}", e))
    })?;

    let storage_key = format!(
        "{}/room/{}/{}",
        tid.to_hex(),
        rid.to_hex(),
        uuid::Uuid::new_v4()
    );
    let file_path = upload_dir.join(&storage_key);

    if let Some(parent) = file_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            ApiError::Internal(format!("Failed to create dirs: {}", e))
        })?;
    }

    tokio::fs::write(&file_path, &bytes).await.map_err(|e| {
        ApiError::Internal(format!("Failed to write file: {}", e))
    })?;

    let url = format!(
        "/api/tenant/{}/file/{}",
        tid.to_hex(),
        storage_key
    );

    let context = FileContext {
        context_type: FileContextType::Room,
        entity_id: rid,
        room_id: Some(rid),
    };

    let file = state
        .files
        .create(
            tid,
            auth.user_id,
            context,
            filename,
            content_type,
            size,
            "local".to_string(),
            storage_key,
            url,
        )
        .await?;

    Ok(Json(to_response(file)))
}

fn upload_dir() -> PathBuf {
    let dir = std::env::var("ROOMLER_UPLOAD_DIR").unwrap_or_else(|_| "/tmp/roomler2-uploads".to_string());
    PathBuf::from(dir)
}

use axum::{Json, extract::{Path, State}};
use bson::oid::ObjectId;
use serde::Deserialize;
use std::sync::Arc;

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler2_db::models::TaskCategory;

/// POST /api/tenant/:tid/file/:fid/recognize
/// Trigger AI document recognition for an uploaded file.
pub async fn recognize_file(
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

    if !state.recognition.is_available() {
        return Err(ApiError::BadRequest(
            "Document recognition not configured (missing Claude API key)".to_string(),
        ));
    }

    let file = state.files.base.find_by_id_in_tenant(tid, fid).await?;

    // Create background task
    let task = state
        .tasks
        .create_task(
            tid,
            auth.user_id,
            "document_recognition".to_string(),
            TaskCategory::Recognition,
            serde_json::json!({ "file_id": file_id }),
        )
        .await?;

    let task_id = task.id.unwrap();
    let recognition = state.recognition.clone();
    let files_dao = Arc::clone(&state.files);
    let task_store = Arc::clone(state.tasks.store());

    let upload_dir = std::env::var("ROOMLER_UPLOAD_DIR")
        .unwrap_or_else(|_| "/tmp/roomler2-uploads".to_string());
    let file_path = std::path::PathBuf::from(upload_dir).join(&file.storage_key);
    let content_type = file.content_type.clone();

    state.tasks.spawn_task(task_id, async move {
        task_store
            .update_progress(task_id, 10, Some("Reading file".to_string()))
            .await
            .map_err(|e| format!("{}", e))?;

        let file_bytes = tokio::fs::read(&file_path)
            .await
            .map_err(|e| format!("Failed to read file: {}", e))?;

        task_store
            .update_progress(task_id, 30, Some("Sending to Claude API".to_string()))
            .await
            .map_err(|e| format!("{}", e))?;

        let result = recognition
            .recognize(&file_bytes, &content_type)
            .await?;

        task_store
            .update_progress(task_id, 80, Some("Updating file record".to_string()))
            .await
            .map_err(|e| format!("{}", e))?;

        // Update file with recognized content
        let recognized = roomler2_db::models::RecognizedContent {
            raw_text: result.raw_text,
            structured_data: result.structured_data,
            document_type: result.document_type,
            confidence: result.confidence,
            processed_at: bson::DateTime::now(),
        };

        let recognized_bson = bson::to_bson(&recognized)
            .map_err(|e| format!("Failed to serialize recognized content: {}", e))?;

        files_dao
            .base
            .update_by_id(
                fid,
                bson::doc! { "$set": { "recognized_content": recognized_bson } },
            )
            .await
            .map_err(|e| format!("Failed to update file: {}", e))?;

        task_store
            .complete(task_id, None, None)
            .await
            .map_err(|e| format!("{}", e))?;

        Ok(())
    });

    Ok(Json(serde_json::json!({
        "task_id": task_id.to_hex(),
        "status": "pending",
    })))
}

/// POST /api/tenant/:tid/export/conversation-pdf
/// Export conversation as PDF (background task).
#[derive(Debug, Deserialize)]
pub struct ExportPdfRequest {
    pub room_id: String,
}

pub async fn export_conversation_pdf(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<ExportPdfRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&body.room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let task = state
        .tasks
        .create_task(
            tid,
            auth.user_id,
            "export_conversation_pdf".to_string(),
            TaskCategory::Export,
            serde_json::json!({ "room_id": body.room_id, "format": "pdf" }),
        )
        .await?;

    let task_id = task.id.unwrap();
    let messages_dao = Arc::clone(&state.messages);
    let users_dao = Arc::clone(&state.users);
    let task_store = Arc::clone(state.tasks.store());

    state.tasks.spawn_task(task_id, async move {
        let params = roomler2_services::dao::base::PaginationParams {
            page: 1,
            per_page: 10000,
        };
        let result = messages_dao
            .find_in_room(rid, &params)
            .await
            .map_err(|e| format!("Failed to fetch messages: {}", e))?;

        task_store
            .update_progress(task_id, 30, Some("Fetched messages".to_string()))
            .await
            .map_err(|e| format!("{}", e))?;

        let author_ids: Vec<ObjectId> = result
            .items
            .iter()
            .map(|m| m.author_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let mut user_map = std::collections::HashMap::new();
        for uid in &author_ids {
            if let Ok(user) = users_dao.base.find_by_id(*uid).await {
                user_map.insert(*uid, user);
            }
        }

        task_store
            .update_progress(task_id, 60, Some("Generating PDF".to_string()))
            .await
            .map_err(|e| format!("{}", e))?;

        let bytes = roomler2_services::export::pdf::export_conversation(&result.items, &user_map)?;

        let export_dir = std::env::var("ROOMLER_UPLOAD_DIR")
            .unwrap_or_else(|_| "/tmp/roomler2-uploads".to_string());
        let export_dir = std::path::PathBuf::from(export_dir).join("exports");
        tokio::fs::create_dir_all(&export_dir)
            .await
            .map_err(|e| format!("Failed to create export dir: {}", e))?;

        let file_name = format!("conversation-export-{}.pdf", task_id.to_hex());
        let file_path = export_dir.join(&file_name);
        tokio::fs::write(&file_path, &bytes)
            .await
            .map_err(|e| format!("Failed to write PDF: {}", e))?;

        task_store
            .complete(
                task_id,
                Some(file_path.to_string_lossy().to_string()),
                Some(file_name),
            )
            .await
            .map_err(|e| format!("{}", e))?;

        Ok(())
    });

    Ok(Json(serde_json::json!({
        "task_id": task_id.to_hex(),
        "status": "pending",
    })))
}

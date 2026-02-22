use axum::{Json, extract::{Path, State}};
use bson::oid::ObjectId;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler2_db::models::TaskCategory;
use roomler2_services::dao::base::PaginationParams;

#[derive(Debug, Deserialize)]
pub struct ExportConversationRequest {
    pub room_id: String,
}

pub async fn export_conversation(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<ExportConversationRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let rid = ObjectId::parse_str(&body.room_id)
        .map_err(|_| ApiError::BadRequest("Invalid room_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    // Create background task
    let task = state
        .tasks
        .create_task(
            tid,
            auth.user_id,
            "export_conversation".to_string(),
            TaskCategory::Export,
            serde_json::json!({ "room_id": body.room_id }),
        )
        .await?;

    let task_id = task.id.unwrap();

    // Spawn async export work
    let messages_dao = Arc::clone(&state.messages);
    let users_dao = Arc::clone(&state.users);
    let task_store = Arc::clone(state.tasks.store());

    state.tasks.spawn_task(task_id, async move {
        // Fetch all messages in room (up to 10000)
        let params = PaginationParams { page: 1, per_page: 10000 };
        let result = messages_dao
            .find_in_room(rid, &params)
            .await
            .map_err(|e| format!("Failed to fetch messages: {}", e))?;

        task_store
            .update_progress(task_id, 30, Some("Fetched messages".to_string()))
            .await
            .map_err(|e| format!("Failed to update progress: {}", e))?;

        // Collect unique author IDs and fetch users
        let author_ids: Vec<ObjectId> = result
            .items
            .iter()
            .map(|m| m.author_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let mut user_map = HashMap::new();
        for uid in &author_ids {
            if let Ok(user) = users_dao.base.find_by_id(*uid).await {
                user_map.insert(*uid, user);
            }
        }

        task_store
            .update_progress(task_id, 60, Some("Fetched user data".to_string()))
            .await
            .map_err(|e| format!("Failed to update progress: {}", e))?;

        // Generate Excel
        let bytes = roomler2_services::export::excel::export_conversation(
            &result.items,
            &user_map,
        )
        .map_err(|e| format!("Excel export failed: {}", e))?;

        // Write to temp file
        let export_dir = std::env::var("ROOMLER_UPLOAD_DIR")
            .unwrap_or_else(|_| "/tmp/roomler2-uploads".to_string());
        let export_dir = std::path::PathBuf::from(export_dir).join("exports");
        tokio::fs::create_dir_all(&export_dir)
            .await
            .map_err(|e| format!("Failed to create export dir: {}", e))?;

        let file_name = format!("conversation-export-{}.xlsx", task_id.to_hex());
        let file_path = export_dir.join(&file_name);
        tokio::fs::write(&file_path, &bytes)
            .await
            .map_err(|e| format!("Failed to write export file: {}", e))?;

        task_store
            .complete(
                task_id,
                Some(file_path.to_string_lossy().to_string()),
                Some(file_name),
            )
            .await
            .map_err(|e| format!("Failed to complete task: {}", e))?;

        Ok(())
    });

    Ok(Json(serde_json::json!({
        "task_id": task_id.to_hex(),
        "status": "pending",
    })))
}

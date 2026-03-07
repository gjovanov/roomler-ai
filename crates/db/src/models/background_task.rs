use bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTask {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub user_id: ObjectId,
    pub task_type: String,
    pub category: TaskCategory,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default)]
    pub logs: Vec<String>,
    #[serde(default)]
    pub progress: u8,
    pub file_path: Option<String>,
    pub file_name: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<DateTime>,
    pub completed_at: Option<DateTime>,
    pub expires_at: DateTime,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCategory {
    Recording,
    Export,
    Import,
    Recognition,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    Processing,
    Completed,
    Failed,
    Expired,
}

impl BackgroundTask {
    pub const COLLECTION: &'static str = "background_tasks";
}

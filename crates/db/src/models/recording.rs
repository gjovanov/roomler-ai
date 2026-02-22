use bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recording {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub room_id: ObjectId,
    pub recording_type: RecordingType,
    pub status: RecordingStatus,
    pub file: StorageFile,
    pub started_at: DateTime,
    pub ended_at: DateTime,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default = "bool_true")]
    pub allow_download: bool,
    pub expires_at: Option<DateTime>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecordingType {
    #[default]
    Video,
    Audio,
    ScreenShare,
    ChatLog,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStatus {
    #[default]
    Processing,
    Available,
    Failed,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageFile {
    pub storage_provider: StorageProvider,
    pub bucket: String,
    pub key: String,
    pub url: String,
    pub content_type: String,
    pub size: u64,
    pub duration: u32,
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StorageProvider {
    S3,
    #[default]
    MinIO,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Private,
    Members,
    Organization,
}

fn bool_true() -> bool {
    true
}

impl Recording {
    pub const COLLECTION: &'static str = "recordings";
}

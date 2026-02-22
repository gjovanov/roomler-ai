use bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};

use super::recording::{StorageProvider, Visibility};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct File {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub uploaded_by: ObjectId,
    pub context: FileContext,
    pub filename: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub storage_provider: StorageProvider,
    pub storage_bucket: String,
    pub storage_key: String,
    pub url: String,
    pub content_type: String,
    pub size: u64,
    pub checksum: Option<String>,
    pub dimensions: Option<Dimensions>,
    pub duration: Option<u32>,
    #[serde(default)]
    pub thumbnails: Vec<Thumbnail>,
    #[serde(default = "default_version")]
    pub version: u32,
    pub previous_version_id: Option<ObjectId>,
    #[serde(default = "bool_true")]
    pub is_current_version: bool,
    pub external_source: Option<ExternalSource>,
    #[serde(default)]
    pub scan_status: ScanStatus,
    #[serde(default)]
    pub visibility: Visibility,
    pub recognized_content: Option<RecognizedContent>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContext {
    pub context_type: FileContextType,
    pub entity_id: ObjectId,
    pub room_id: Option<ObjectId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileContextType {
    Message,
    Document,
    Profile,
    Room,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thumbnail {
    pub size: String,
    pub url: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalSource {
    pub provider: CloudProvider,
    pub external_id: String,
    pub external_url: String,
    #[serde(default)]
    pub sync_status: SyncStatus,
    pub last_synced_at: Option<DateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudProvider {
    GoogleDrive,
    OneDrive,
    Dropbox,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    #[default]
    Pending,
    Synced,
    Failed,
    Outdated,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScanStatus {
    #[default]
    Pending,
    Clean,
    Malware,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecognizedContent {
    pub raw_text: String,
    pub structured_data: Option<serde_json::Value>,
    pub document_type: Option<String>,
    pub confidence: f64,
    pub processed_at: DateTime,
}

fn default_version() -> u32 {
    1
}

fn bool_true() -> bool {
    true
}

impl File {
    pub const COLLECTION: &'static str = "files";
}

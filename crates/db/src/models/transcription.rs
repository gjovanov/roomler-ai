use bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcription {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub room_id: ObjectId,
    pub recording_id: Option<ObjectId>,
    #[serde(default)]
    pub status: TranscriptionStatus,
    pub language: String,
    #[serde(default)]
    pub format: TranscriptFormat,
    pub content_url: String,
    #[serde(default)]
    pub segments: Vec<TranscriptSegment>,
    pub summary: Option<String>,
    #[serde(default)]
    pub action_items: Vec<ActionItem>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptionStatus {
    #[default]
    Processing,
    Available,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptFormat {
    Vtt,
    Srt,
    #[default]
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub speaker_id: Option<ObjectId>,
    pub speaker_name: String,
    pub start_time: f64,
    pub end_time: f64,
    pub text: String,
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionItem {
    pub text: String,
    pub assignee: Option<String>,
    pub due_date: Option<String>,
    pub completed: bool,
}

impl Transcription {
    pub const COLLECTION: &'static str = "transcriptions";
}

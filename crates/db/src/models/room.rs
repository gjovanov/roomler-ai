use bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub parent_id: Option<ObjectId>,
    pub name: String,
    pub path: String,
    pub emoji: Option<String>,
    pub topic: Option<String>,
    pub purpose: Option<String>,
    pub icon: Option<String>,
    #[serde(default)]
    pub position: i32,
    #[serde(default)]
    pub is_open: bool,
    #[serde(default)]
    pub is_archived: bool,
    #[serde(default)]
    pub is_read_only: bool,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub permission_overwrites: Vec<PermissionOverwrite>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub media_settings: Option<MediaSettings>,
    pub conference_settings: Option<ConferenceSettings>,
    pub conference_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meeting_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_url: Option<String>,
    pub organizer_id: Option<ObjectId>,
    #[serde(default)]
    pub co_organizer_ids: Vec<ObjectId>,
    pub creator_id: ObjectId,
    pub last_message_id: Option<ObjectId>,
    pub last_activity_at: Option<DateTime>,
    #[serde(default)]
    pub member_count: u32,
    #[serde(default)]
    pub message_count: u64,
    #[serde(default)]
    pub participant_count: u32,
    #[serde(default)]
    pub peak_participant_count: u32,
    pub actual_start_time: Option<DateTime>,
    pub actual_end_time: Option<DateTime>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl Room {
    pub const COLLECTION: &'static str = "rooms";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionOverwrite {
    pub target_id: ObjectId,
    pub target_type: String,
    #[serde(default)]
    pub allow: u64,
    #[serde(default)]
    pub deny: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaSettings {
    #[serde(default)]
    pub audio_enabled: bool,
    #[serde(default)]
    pub video_enabled: bool,
    #[serde(default)]
    pub screen_share_enabled: bool,
    #[serde(default)]
    pub recording_enabled: bool,
    #[serde(default)]
    pub transcription_enabled: bool,
    pub max_participants: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConferenceSettings {
    pub scheduled_start: Option<DateTime>,
    pub scheduled_end: Option<DateTime>,
    pub recurrence: Option<String>,
    pub timezone: Option<String>,
    #[serde(default)]
    pub lobby_enabled: bool,
    #[serde(default)]
    pub auto_record: bool,
}

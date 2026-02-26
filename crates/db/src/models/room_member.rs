use bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomMember {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub room_id: ObjectId,
    pub user_id: Option<ObjectId>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    #[serde(default)]
    pub is_external: bool,
    pub role: Option<ParticipantRole>,
    #[serde(default)]
    pub sessions: Vec<ParticipantSession>,
    pub joined_at: DateTime,
    pub last_read_message_id: Option<ObjectId>,
    pub last_read_at: Option<DateTime>,
    #[serde(default)]
    pub unread_count: i64,
    #[serde(default)]
    pub mention_count: i64,
    pub notification_override: Option<String>,
    #[serde(default)]
    pub is_muted: bool,
    #[serde(default)]
    pub is_pinned: bool,
    #[serde(default)]
    pub is_video_on: bool,
    #[serde(default)]
    pub is_screen_sharing: bool,
    #[serde(default)]
    pub is_hand_raised: bool,
    #[serde(default)]
    pub total_duration: i64,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

impl RoomMember {
    pub const COLLECTION: &'static str = "room_members";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantRole {
    Organizer,
    CoOrganizer,
    Presenter,
    Attendee,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParticipantSession {
    pub joined_at: DateTime,
    pub left_at: Option<DateTime>,
    pub duration: Option<i64>,
    pub device_type: String,
}

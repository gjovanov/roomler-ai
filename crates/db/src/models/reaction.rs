use bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reaction {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub room_id: ObjectId,
    pub message_id: ObjectId,
    pub user_id: ObjectId,
    pub emoji: EmojiRef,
    pub created_at: DateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmojiRef {
    pub emoji_type: EmojiType,
    pub value: String,
    pub custom_emoji_id: Option<ObjectId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EmojiType {
    #[default]
    Unicode,
    Custom,
}

impl Reaction {
    pub const COLLECTION: &'static str = "reactions";
}

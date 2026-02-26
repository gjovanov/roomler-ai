use bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallChatMessage {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub room_id: ObjectId,
    pub author_id: ObjectId,
    pub display_name: String,
    pub content: String,
    pub created_at: DateTime,
}

impl CallChatMessage {
    pub const COLLECTION: &'static str = "call_chat_messages";
}

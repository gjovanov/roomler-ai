use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomEmoji {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub name: String,
    pub image_url: String,
    #[serde(default)]
    pub is_animated: bool,
    pub creator_id: ObjectId,
    pub allowed_role_ids: Option<Vec<ObjectId>>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

impl CustomEmoji {
    pub const COLLECTION: &'static str = "custom_emojis";
}

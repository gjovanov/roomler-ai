use bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invite {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub room_id: Option<ObjectId>,
    pub code: String,
    pub inviter_id: ObjectId,
    pub target_email: Option<String>,
    pub target_user_id: Option<ObjectId>,
    pub max_uses: Option<u32>,
    #[serde(default)]
    pub use_count: u32,
    pub expires_at: Option<DateTime>,
    #[serde(default)]
    pub assign_role_ids: Vec<ObjectId>,
    #[serde(default)]
    pub status: InviteStatus,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InviteStatus {
    #[default]
    Active,
    Expired,
    Revoked,
    Exhausted,
}

impl Invite {
    pub const COLLECTION: &'static str = "invites";
}

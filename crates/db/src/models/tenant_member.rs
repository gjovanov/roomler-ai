use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

use super::tenant::NotificationLevel;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantMember {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub user_id: ObjectId,
    pub nickname: Option<String>,
    #[serde(default)]
    pub role_ids: Vec<ObjectId>,
    pub joined_at: DateTime,
    #[serde(default)]
    pub is_pending: bool,
    #[serde(default)]
    pub is_muted: bool,
    pub notification_override: Option<NotificationLevel>,
    pub invited_by: Option<ObjectId>,
    pub last_seen_at: Option<DateTime>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

impl TenantMember {
    pub const COLLECTION: &'static str = "tenant_members";
}

use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub user_id: ObjectId,
    pub notification_type: NotificationType,
    pub title: String,
    pub body: String,
    pub link: Option<String>,
    pub source: NotificationSource,
    #[serde(default)]
    pub is_read: bool,
    pub read_at: Option<DateTime>,
    pub created_at: DateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationType {
    Message,
    Mention,
    Reaction,
    Invite,
    Call,
    TaskComplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationSource {
    pub entity_type: String,
    pub entity_id: ObjectId,
    pub actor_id: Option<ObjectId>,
}

impl Notification {
    pub const COLLECTION: &'static str = "notifications";
}

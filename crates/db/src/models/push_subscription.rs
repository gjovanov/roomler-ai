use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSubscription {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub user_id: ObjectId,
    pub endpoint: String,
    pub keys: PushKeys,
    pub created_at: DateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushKeys {
    pub auth: String,
    pub p256dh: String,
}

impl PushSubscription {
    pub const COLLECTION: &'static str = "push_subscriptions";
}

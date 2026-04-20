use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLog {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub actor_id: Option<ObjectId>,
    #[serde(default)]
    pub actor_type: ActorType,
    pub action: String,
    pub target_type: String,
    pub target_id: Option<ObjectId>,
    #[serde(default)]
    pub changes: Vec<AuditChange>,
    #[serde(default)]
    pub metadata: AuditMetadata,
    pub created_at: DateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActorType {
    #[default]
    User,
    Bot,
    System,
    Webhook,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditChange {
    pub field: String,
    pub old_value: Option<serde_json::Value>,
    pub new_value: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuditMetadata {
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub reason: Option<String>,
}

impl AuditLog {
    pub const COLLECTION: &'static str = "audit_logs";
}

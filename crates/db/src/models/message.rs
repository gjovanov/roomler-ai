use bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub room_id: ObjectId,
    pub thread_id: Option<ObjectId>,
    #[serde(default)]
    pub is_thread_root: bool,
    pub thread_metadata: Option<ThreadMetadata>,
    pub author_id: ObjectId,
    #[serde(default)]
    pub author_type: AuthorType,
    pub content: String,
    #[serde(default)]
    pub content_type: ContentType,
    #[serde(default)]
    pub message_type: MessageType,
    #[serde(default)]
    pub embeds: Vec<Embed>,
    #[serde(default)]
    pub attachments: Vec<MessageAttachment>,
    #[serde(default)]
    pub mentions: Mentions,
    #[serde(default)]
    pub reaction_summary: Vec<ReactionSummary>,
    pub referenced_message_id: Option<ObjectId>,
    #[serde(default)]
    pub is_pinned: bool,
    #[serde(default)]
    pub is_edited: bool,
    pub edited_at: Option<DateTime>,
    pub nonce: Option<String>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadMetadata {
    #[serde(default)]
    pub reply_count: u32,
    pub last_reply_at: Option<DateTime>,
    pub last_reply_user_id: Option<ObjectId>,
    #[serde(default)]
    pub participant_ids: Vec<ObjectId>,
    #[serde(default)]
    pub is_locked: bool,
    #[serde(default)]
    pub is_archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthorType {
    #[default]
    User,
    Bot,
    Webhook,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    #[default]
    Text,
    Markdown,
    RichText,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    #[default]
    Default,
    SystemJoin,
    SystemLeave,
    SystemPin,
    Call,
    Reply,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Embed {
    pub embed_type: String,
    pub url: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub color: Option<u32>,
    pub thumbnail_url: Option<String>,
    pub author_name: Option<String>,
    pub provider_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAttachment {
    pub file_id: ObjectId,
    pub filename: String,
    pub content_type: String,
    pub size: u64,
    pub url: String,
    pub thumbnail_url: Option<String>,
    #[serde(default)]
    pub is_spoiler: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Mentions {
    #[serde(default)]
    pub users: Vec<ObjectId>,
    #[serde(default)]
    pub roles: Vec<ObjectId>,
    #[serde(default)]
    pub rooms: Vec<ObjectId>,
    #[serde(default)]
    pub everyone: bool,
    #[serde(default)]
    pub here: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionSummary {
    pub emoji: String,
    pub count: u32,
}

impl Message {
    pub const COLLECTION: &'static str = "messages";
}

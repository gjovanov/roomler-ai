use bson::{oid::ObjectId, DateTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub email: String,
    pub username: String,
    pub display_name: String,
    pub avatar: Option<String>,
    pub bio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    #[serde(default)]
    pub status: UserStatusInfo,
    #[serde(default)]
    pub presence: Presence,
    #[serde(default = "default_locale")]
    pub locale: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub is_verified: bool,
    #[serde(default)]
    pub is_mfa_enabled: bool,
    pub last_active_at: Option<DateTime>,
    #[serde(default)]
    pub oauth_providers: Vec<OAuthProvider>,
    #[serde(default)]
    pub notification_preferences: NotificationPrefs,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserStatusInfo {
    pub text: Option<String>,
    pub emoji: Option<String>,
    pub expires_at: Option<DateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Online,
    Idle,
    Dnd,
    #[default]
    Offline,
    Invisible,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthProvider {
    pub provider: String,
    pub provider_id: String,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationPrefs {
    #[serde(default = "bool_true")]
    pub email: bool,
    #[serde(default = "bool_true")]
    pub push: bool,
    #[serde(default = "bool_true")]
    pub desktop: bool,
    #[serde(default)]
    pub mute_all: bool,
}

impl Default for NotificationPrefs {
    fn default() -> Self {
        Self {
            email: true,
            push: true,
            desktop: true,
            mute_all: false,
        }
    }
}

fn bool_true() -> bool {
    true
}

fn default_locale() -> String {
    "en-US".to_string()
}

fn default_timezone() -> String {
    "UTC".to_string()
}

impl User {
    pub const COLLECTION: &'static str = "users";
}

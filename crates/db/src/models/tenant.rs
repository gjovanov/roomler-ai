use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub owner_id: ObjectId,
    pub plan: Plan,
    pub features: Vec<String>,
    pub settings: TenantSettings,
    pub billing: Option<BillingInfo>,
    pub integrations: Option<IntegrationSettings>,
    #[serde(default)]
    pub is_archived: bool,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    #[default]
    Free,
    Pro,
    Business,
    Enterprise,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantSettings {
    #[serde(default = "default_locale")]
    pub default_locale: String,
    #[serde(default)]
    pub default_message_notifications: NotificationLevel,
    #[serde(default)]
    pub mfa_required: bool,
    #[serde(default)]
    pub allow_guest_access: bool,
    #[serde(default = "default_max_members")]
    pub max_members: u32,
    #[serde(default = "default_file_upload_limit")]
    pub file_upload_limit: u64,
}

impl Default for TenantSettings {
    fn default() -> Self {
        Self {
            default_locale: default_locale(),
            default_message_notifications: NotificationLevel::default(),
            mfa_required: false,
            allow_guest_access: false,
            max_members: default_max_members(),
            file_upload_limit: default_file_upload_limit(),
        }
    }
}

fn default_locale() -> String {
    "en-US".to_string()
}

fn default_max_members() -> u32 {
    100
}

fn default_file_upload_limit() -> u64 {
    10 * 1024 * 1024 // 10 MB
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    #[default]
    All,
    Mentions,
    Nothing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingInfo {
    pub customer_id: Option<String>,
    pub subscription_id: Option<String>,
    pub current_period_end: Option<DateTime>,
    #[serde(default)]
    pub status: SubscriptionStatus,
    #[serde(default)]
    pub cancel_at_period_end: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    #[default]
    Active,
    PastDue,
    Canceled,
    Trialing,
    Incomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationSettings {
    pub google_drive: Option<OAuthCredential>,
    pub onedrive: Option<OAuthCredential>,
    pub dropbox: Option<OAuthCredential>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime>,
}

impl Tenant {
    pub const COLLECTION: &'static str = "tenants";
}

#[derive(Debug, Serialize)]
pub struct PlanLimits {
    pub max_members: u32,
    pub max_channels: u32,
    pub max_message_history: i64,
    pub storage_bytes: u64,
    pub video_max_participants: u32,
    pub cloud_integrations: bool,
    pub ai_recognition: bool,
    pub recordings: bool,
}

impl Plan {
    pub fn limits(&self) -> PlanLimits {
        match self {
            Plan::Free => PlanLimits {
                max_members: 10,
                max_channels: 5,
                max_message_history: 5_000,
                storage_bytes: 100 * 1024 * 1024,
                video_max_participants: 0,
                cloud_integrations: false,
                ai_recognition: false,
                recordings: false,
            },
            Plan::Pro => PlanLimits {
                max_members: u32::MAX,
                max_channels: u32::MAX,
                max_message_history: -1,
                storage_bytes: 10 * 1024 * 1024 * 1024,
                video_max_participants: 10,
                cloud_integrations: true,
                ai_recognition: false,
                recordings: false,
            },
            Plan::Business | Plan::Enterprise => PlanLimits {
                max_members: u32::MAX,
                max_channels: u32::MAX,
                max_message_history: -1,
                storage_bytes: 100 * 1024 * 1024 * 1024,
                video_max_participants: 100,
                cloud_integrations: true,
                ai_recognition: true,
                recordings: true,
            },
        }
    }

    pub fn price_monthly_cents(&self) -> u32 {
        match self {
            Plan::Free => 0,
            Plan::Pro => 800,
            Plan::Business => 1600,
            Plan::Enterprise => 1600,
        }
    }
}

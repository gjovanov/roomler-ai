use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

use crate::permissions::Permissions;

// ────────────────────────────────────────────────────────────────────────────
// Agent
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OsKind {
    Linux,
    Macos,
    Windows,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Online,
    Offline,
    Unenrolled,
    Quarantined,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DisplayInfo {
    pub index: u8,
    pub name: String,
    pub width_px: u32,
    pub height_px: u32,
    pub scale: f32,
    pub primary: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AgentCaps {
    pub hw_encoders: Vec<String>,
    pub codecs: Vec<String>,
    pub has_input_permission: bool,
    pub supports_clipboard: bool,
    pub supports_file_transfer: bool,
    pub max_simultaneous_sessions: u8,
    /// Video transport modes the agent supports beyond the default
    /// WebRTC video track. Empty / unset means WebRTC video only
    /// (the legacy default; older agents that don't know about
    /// this field deserialize that way via serde default).
    ///
    /// Known value: `data-channel-vp9-444` — VP9 profile 1
    /// (8-bit 4:4:4) frames over an RTCDataChannel named
    /// `video-bytes`. Bypasses the browser's WebRTC video pipeline
    /// which enforces 4:2:0 across every codec. See
    /// `docs/vp9-444-plan.md` for the rationale and the wire
    /// format spec.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transports: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AccessPolicy {
    pub require_consent: bool,
    #[serde(default)]
    pub allowed_role_ids: Vec<ObjectId>,
    #[serde(default)]
    pub allowed_user_ids: Vec<ObjectId>,
    pub auto_terminate_idle_minutes: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Agent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub owner_user_id: ObjectId,
    pub name: String,
    pub machine_id: String,
    pub os: OsKind,
    pub agent_version: String,
    pub agent_token_hash: String,
    pub status: AgentStatus,
    pub last_seen_at: DateTime,
    #[serde(default)]
    pub displays: Vec<DisplayInfo>,
    #[serde(default)]
    pub capabilities: AgentCaps,
    #[serde(default)]
    pub access_policy: AccessPolicy,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl Agent {
    pub const COLLECTION: &'static str = "agents";
}

// ────────────────────────────────────────────────────────────────────────────
// Session
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Pending,
    AwaitingConsent,
    Negotiating,
    Active,
    Closed,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    ControllerHangup,
    AgentHangup,
    UserDenied,
    ConsentTimeout,
    AgentDisconnect,
    AdminTerminated,
    IdleTimeout,
    Error,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SessionStats {
    pub bytes_sent: u64,
    pub bytes_recv: u64,
    pub peak_fps: f32,
    pub avg_rtt_ms: f32,
    pub keyframe_requests: u32,
    pub input_events: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoteSession {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub controller_user_id: ObjectId,
    #[serde(default)]
    pub watchers: Vec<ObjectId>,
    pub permissions: Permissions,
    pub phase: SessionPhase,
    pub created_at: DateTime,
    pub started_at: Option<DateTime>,
    pub ended_at: Option<DateTime>,
    pub end_reason: Option<EndReason>,
    pub recording_url: Option<String>,
    #[serde(default)]
    pub stats: SessionStats,
}

impl RemoteSession {
    pub const COLLECTION: &'static str = "remote_sessions";
}

// ────────────────────────────────────────────────────────────────────────────
// Audit
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditKind {
    SessionRequested,
    ConsentPrompted,
    ConsentGranted,
    ConsentDenied,
    ConsentTimedOut,
    SessionStarted,
    SessionEnded { reason: EndReason },
    ClipboardWriteToHost { bytes: u32 },
    ClipboardReadFromHost { bytes: u32 },
    FileSentToHost { name: String, bytes: u64 },
    FileSentFromHost { name: String, bytes: u64 },
    KeyframeRequested,
    PermissionsChanged { permissions: Permissions },
    WatcherJoined { user_id: ObjectId },
    WatcherLeft { user_id: ObjectId },
    Error { message: String },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoteAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub session_id: ObjectId,
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub at: DateTime,
    pub event: AuditKind,
}

impl RemoteAuditEvent {
    pub const COLLECTION: &'static str = "remote_audit";
}

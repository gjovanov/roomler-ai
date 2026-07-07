use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub name: String,
    pub description: Option<String>,
    pub color: Option<u32>,
    #[serde(default)]
    pub position: u32,
    #[serde(default)]
    pub permissions: u64,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub is_managed: bool,
    #[serde(default)]
    pub is_mentionable: bool,
    #[serde(default)]
    pub is_hoisted: bool,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

/// Permission bits (u64 bitfield)
#[allow(dead_code)]
pub mod permissions {
    pub const VIEW_CHANNELS: u64 = 1 << 0;
    pub const MANAGE_CHANNELS: u64 = 1 << 1;
    pub const MANAGE_ROLES: u64 = 1 << 2;
    pub const MANAGE_TENANT: u64 = 1 << 3;
    pub const KICK_MEMBERS: u64 = 1 << 4;
    pub const BAN_MEMBERS: u64 = 1 << 5;
    pub const INVITE_MEMBERS: u64 = 1 << 6;
    pub const SEND_MESSAGES: u64 = 1 << 7;
    pub const SEND_THREADS: u64 = 1 << 8;
    pub const EMBED_LINKS: u64 = 1 << 9;
    pub const ATTACH_FILES: u64 = 1 << 10;
    pub const READ_HISTORY: u64 = 1 << 11;
    pub const MENTION_EVERYONE: u64 = 1 << 12;
    pub const MANAGE_MESSAGES: u64 = 1 << 13;
    pub const ADD_REACTIONS: u64 = 1 << 14;
    pub const CONNECT_VOICE: u64 = 1 << 15;
    pub const SPEAK: u64 = 1 << 16;
    pub const STREAM_VIDEO: u64 = 1 << 17;
    pub const MUTE_MEMBERS: u64 = 1 << 18;
    pub const DEAFEN_MEMBERS: u64 = 1 << 19;
    pub const MOVE_MEMBERS: u64 = 1 << 20;
    pub const MANAGE_MEETINGS: u64 = 1 << 21;
    pub const MANAGE_DOCUMENTS: u64 = 1 << 22;
    pub const ADMINISTRATOR: u64 = 1 << 23;
    /// Enroll / rename / delete / assign-owner / set-policy for remote-control
    /// agents (devices).
    pub const MANAGE_AGENTS: u64 = 1 << 24;
    /// Initiate a remote-control session against a device you do NOT own.
    /// (Controlling your OWN device — `controller == owner_user_id` — never
    /// needs this; it's gated only by the device's consent mode.)
    pub const REMOTE_CONTROL: u64 = 1 << 25;
    /// View the remote-control audit log (`remote_audit`).
    pub const VIEW_REMOTE_AUDIT: u64 = 1 << 26;

    /// Default member permissions
    pub const DEFAULT_MEMBER: u64 = VIEW_CHANNELS
        | SEND_MESSAGES
        | SEND_THREADS
        | EMBED_LINKS
        | ATTACH_FILES
        | READ_HISTORY
        | ADD_REACTIONS
        | CONNECT_VOICE
        | SPEAK
        | STREAM_VIDEO;

    /// Admin permissions (all except ADMINISTRATOR)
    pub const DEFAULT_ADMIN: u64 = DEFAULT_MEMBER
        | MANAGE_CHANNELS
        | MANAGE_ROLES
        | KICK_MEMBERS
        | BAN_MEMBERS
        | INVITE_MEMBERS
        | MENTION_EVERYONE
        | MANAGE_MESSAGES
        | MUTE_MEMBERS
        | DEAFEN_MEMBERS
        | MOVE_MEMBERS
        | MANAGE_MEETINGS
        | MANAGE_DOCUMENTS
        | MANAGE_AGENTS
        | REMOTE_CONTROL
        | VIEW_REMOTE_AUDIT;

    /// Owner permissions (everything). Bump the mask whenever a new bit is
    /// added above so `ALL` literally contains every defined permission (owner
    /// also passes via the `ADMINISTRATOR` bypass in `has`, but keep this exact).
    pub const ALL: u64 = (1 << 27) - 1;

    pub fn has(permissions: u64, flag: u64) -> bool {
        permissions & ADMINISTRATOR != 0 || permissions & flag == flag
    }
}

impl Role {
    pub const COLLECTION: &'static str = "roles";
}

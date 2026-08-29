// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

/// Who may READ a room.
///
/// ## Why this exists
///
/// Before it, every member of a tenant could read every room in that tenant,
/// and there was no way to say otherwise. `is_open` looked like it said
/// otherwise — the sidebar renders a padlock for `!is_open`, and rooms created
/// through the API default to `is_open: false` — but nothing enforced it. A
/// padlock on a room anyone in the org can read is worse than no padlock.
///
/// `is_open` is NOT this field and keeps its own meaning (whether the room is
/// listed in Explore). Discoverability and readability are different questions
/// and conflating them is how the padlock started lying.
///
/// ## Default is Public, deliberately
///
/// `#[serde(default)]`, so every existing document reads back as `Public` and
/// behaviour is unchanged on the day this ships. Anything else would have been
/// a silent mass revocation. `room_members` rows exist for a room's CREATOR
/// (`RoomDao::create` auto-joins them), for anyone who pressed join in Explore,
/// and for conference participants — but NOT for someone who simply opens a
/// room from the sidebar and reads it, which is how most reading happens. So
/// "members only" applied retroactively would have cut real users off from
/// rooms they use daily, with no signal beforehand about who.
///
/// A room becomes non-Public only by a deliberate `MANAGE_CHANNELS` action,
/// and that action adds the actor as a member so nobody can lock themselves
/// out of their own room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoomVisibility {
    /// Any member of the tenant may read it.
    #[default]
    Public,
    /// Only users with a `room_members` row. Still listed, so people know it
    /// exists and can ask to be let in.
    Private,
    /// Only users with a `room_members` row, and not listed at all — someone
    /// who is not a member has no way to learn it exists.
    Secret,
}

impl RoomVisibility {
    /// Does reading this room require a `room_members` row, on top of tenant
    /// membership?
    pub fn requires_membership(self) -> bool {
        !matches!(self, Self::Public)
    }

    /// Should this room be hidden from listings for a non-member?
    pub fn hidden_from_non_members(self) -> bool {
        matches!(self, Self::Secret)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub parent_id: Option<ObjectId>,
    pub name: String,
    pub path: String,
    pub emoji: Option<String>,
    pub topic: Option<String>,
    pub purpose: Option<String>,
    pub icon: Option<String>,
    #[serde(default)]
    pub position: i32,
    /// Whether the room is listed in Explore. NOT an access control — see
    /// [`RoomVisibility`], which is. Kept separate on purpose: "can people
    /// find it" and "can people read it" are different questions.
    #[serde(default)]
    pub is_open: bool,
    /// Who may read this room. See [`RoomVisibility`]; defaults to `Public`,
    /// which is what every pre-existing document reads back as.
    #[serde(default)]
    pub visibility: RoomVisibility,
    #[serde(default)]
    pub is_archived: bool,
    #[serde(default)]
    pub is_read_only: bool,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub permission_overwrites: Vec<PermissionOverwrite>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub media_settings: Option<MediaSettings>,
    pub conference_settings: Option<ConferenceSettings>,
    pub conference_status: Option<String>,
    /// Stats PR-2 — the `call_sessions` document of the call currently in
    /// progress. Set by the transition-gated `start_call`, cleared on end;
    /// lets join/leave attribute minutes to a call INSTANCE (the
    /// status/start/end triple above is overwritten by every new call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_call_id: Option<ObjectId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meeting_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_url: Option<String>,
    pub organizer_id: Option<ObjectId>,
    #[serde(default)]
    pub co_organizer_ids: Vec<ObjectId>,
    pub creator_id: ObjectId,
    pub last_message_id: Option<ObjectId>,
    pub last_activity_at: Option<DateTime>,
    #[serde(default)]
    pub member_count: u32,
    #[serde(default)]
    pub message_count: u64,
    #[serde(default)]
    pub participant_count: u32,
    #[serde(default)]
    pub peak_participant_count: u32,
    pub actual_start_time: Option<DateTime>,
    pub actual_end_time: Option<DateTime>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl Room {
    pub const COLLECTION: &'static str = "rooms";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionOverwrite {
    pub target_id: ObjectId,
    pub target_type: String,
    #[serde(default)]
    pub allow: u64,
    #[serde(default)]
    pub deny: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaSettings {
    #[serde(default)]
    pub audio_enabled: bool,
    #[serde(default)]
    pub video_enabled: bool,
    #[serde(default)]
    pub screen_share_enabled: bool,
    #[serde(default)]
    pub recording_enabled: bool,
    pub max_participants: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConferenceSettings {
    pub scheduled_start: Option<DateTime>,
    pub scheduled_end: Option<DateTime>,
    pub recurrence: Option<String>,
    pub timezone: Option<String>,
    #[serde(default)]
    pub lobby_enabled: bool,
    #[serde(default)]
    pub auto_record: bool,
}

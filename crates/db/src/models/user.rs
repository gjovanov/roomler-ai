// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// The account's address, and a UNIQUE index — so whatever is here is
    /// *reserved*. Only an address the account has PROVEN belongs here (a
    /// password sign-up that completed activation, or an OAuth provider that
    /// verified it). Everything else goes to `unverified_email`.
    pub email: String,
    /// An address that was claimed but never proven: asserted by an OAuth
    /// provider without a verification claim, or evicted from an account that
    /// held it without proof. Kept for support and for a future claim flow —
    /// deliberately NOT indexed and never a lookup key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unverified_email: Option<String>,
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
    /// FR-12 P3 — the tutorial's own state, mirrored here so it follows the
    /// PERSON rather than the browser profile. `#[serde(default)]` because
    /// every user document predates this field.
    #[serde(default)]
    pub tutorial: TutorialState,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

/// FR-12 P3 — tutorial progress, server-side.
///
/// The client keeps the same state in `localStorage` and works entirely
/// without this; the mirror exists so someone who did the tour on their
/// laptop is not walked through it again on their phone.
///
/// ⚠️ `done` is CLIENT-SUPPLIED and lands on the caller's own user document,
/// so it is bounded on write (`MAX_TUTORIAL_CHAPTERS` ids, each
/// `MAX_TUTORIAL_CHAPTER_ID_LEN`). Without a bound this is an unbounded
/// write primitive against your own row — small, but there is no reason to
/// leave it open.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TutorialState {
    /// Chapter ids the user has ticked off. A SET in spirit; stored as a
    /// list because that is what the client holds and the order it ticks
    /// them in is mildly interesting.
    #[serde(default)]
    pub done: Vec<String>,
    /// When the welcome tour first auto-opened for this user. Presence is
    /// the whole signal — it gates the auto-open, and it is never cleared by
    /// normal use, so nobody is ambushed twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_at: Option<DateTime>,
}

/// A tutorial has chapters in the single digits; 64 is room to grow with a
/// ceiling that is still obviously a ceiling.
pub const MAX_TUTORIAL_CHAPTERS: usize = 64;
/// Chapter ids are slugs (`get-started`, `remote-desktop`).
pub const MAX_TUTORIAL_CHAPTER_ID_LEN: usize = 64;

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

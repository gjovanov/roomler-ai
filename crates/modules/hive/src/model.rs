// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! What the server stores about an agent session — and, by omission, what it
//! does not. No field here holds a prompt, a tool call, an output or a line
//! of transcript; the session's content lives on the devices that run and
//! replicate it (`docs/roomler-hive-design.md` §3.3).

use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

/// Where a session is in its life, as far as the server knows.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// The start was sent; the harness is not up yet. `accepted_at` says
    /// whether the device has answered.
    Starting,
    /// The harness is up and waiting for a prompt.
    Idle,
    /// A turn is running.
    Running,
    /// A tool call is waiting for a person's answer.
    AwaitingApproval,
    /// A stop was ordered; the device has not confirmed the harness exited.
    Stopping,
    /// Over; `end_reason` says how.
    Ended,
    /// The device refused the start; `refusal` names its gate.
    Refused,
    /// The start was never answered and will not be sent again.
    Lost,
}

impl SessionStatus {
    /// The spelling on the wire and in the database. Locked by test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Running => "running",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Stopping => "stopping",
            Self::Ended => "ended",
            Self::Refused => "refused",
            Self::Lost => "lost",
        }
    }

    /// Nothing moves a session out of these.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Ended | Self::Refused | Self::Lost)
    }

    /// Every status a session can still leave — the filter every cascade and
    /// every stop matches on.
    pub const LIVE: [SessionStatus; 5] = [
        Self::Starting,
        Self::Idle,
        Self::Running,
        Self::AwaitingApproval,
        Self::Stopping,
    ];

    /// The statuses a device's run-state report may move a session out of.
    /// Not `stopping`: a stop is in flight, and an `idle` that crossed it on
    /// the wire must not undo it — only `ended` may follow.
    pub const REPORTABLE: [SessionStatus; 4] = [
        Self::Starting,
        Self::Idle,
        Self::Running,
        Self::AwaitingApproval,
    ];

    /// The live statuses only a device's own run-state report sets — its
    /// word that it launched the session, as good as its answer to the
    /// start: the state usually lands first, and the answer can be lost.
    pub const LAUNCHED: [SessionStatus; 3] = [Self::Idle, Self::Running, Self::AwaitingApproval];
}

/// The harness a session runs, and its own id for the conversation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HarnessRef {
    /// [`roomler_ai_remote_control::hive::HARNESS_CLAUDE_CODE`] in P0.
    pub id: String,
    /// The harness's session id (a UUID), minted here so every replica
    /// resumes the same conversation.
    pub session: String,
}

/// Where a session runs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionLocation {
    pub device_id: ObjectId,
    /// The device's name when the session started — how the session's room
    /// names its author ("Claude · mars").
    #[serde(default)]
    pub device_name: String,
    /// As the starter typed it; the device resolves it and confines it to
    /// its own `hive_roots`.
    pub folder: String,
    /// The local account the device says it mapped the starter to — set on
    /// acceptance. A claim by the device; the server never chooses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// One agent session (`agent_sessions`).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentSession {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// Who started it: always a driver, and the only one who may stop it or
    /// change who else drives.
    pub owner_id: ObjectId,
    /// FR-90 P1c — who ELSE drives it (may prompt it and answer its
    /// approvals): members of its room the owner named, each holding
    /// `HIVE_RUN` when named. The owner is never listed. At most
    /// [`MAX_DRIVERS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drivers: Vec<ObjectId>,
    pub title: String,
    /// The session's room (P0d): `Secret`, bound to this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_id: Option<ObjectId>,
    /// The newest turn with a stub in the room, and that stub's message —
    /// the next report for the same turn updates it instead of adding one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn: Option<TurnStub>,
    pub harness: HarnessRef,
    pub location: SessionLocation,
    pub status: SessionStatus,
    /// The lease fence the device runs it under. BSON has no unsigned
    /// integer, so it is stored signed; it starts at 1 and only a promotion
    /// (P2) raises it.
    pub fence: i64,
    /// When the device accepted the start. `None` while `starting` means the
    /// start is still unanswered — what reconcile-on-connect re-sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<DateTime>,
    /// The device's refusal word, for `refused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// How it ended (or will end, once `stopping` completes): `stopped`,
    /// `exited`, `member_removed`, `device_removed`, `tenant_archived`,
    /// `never_answered`, `device_unsupported`, `not_on_device` (P1b: its
    /// device reconnected without it — a restart, or an end it never told).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    /// The device's last few words about it (a refusal's or an exit's), capped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime>,
    /// FR-90 P1e — the org's brain revision the session's core memory was
    /// rendered from, pinned when it was created: a fact written later
    /// reaches the next session, never this one. Absent before P1e.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brain_rev: Option<i64>,
    /// FR-90 P1j — how the session came to be. Absent = started from Roomler
    /// (every record before P1j).
    #[serde(default, skip_serializing_if = "SessionOrigin::is_started")]
    pub origin: SessionOrigin,
}

/// FR-90 P1j — how a session came to be.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    /// Its owner started it from Roomler, through every start gate.
    #[default]
    Started,
    /// A terminal session its owner adopted on a device (`roomler hive
    /// adopt`, decision 11). Read-only: the terminal holds the harness, so
    /// nobody drives it from here, the owner included, and it passed none of
    /// the start gates.
    Adopted,
}

impl SessionOrigin {
    /// The spelling in the database and on the wire. Locked by test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Adopted => "adopted",
        }
    }

    /// Serde's `skip_serializing_if`: a started session stores no origin, so
    /// its record reads as every record before P1j did.
    pub fn is_started(&self) -> bool {
        *self == Self::Started
    }
}

/// The most drivers a session may have besides its owner.
pub const MAX_DRIVERS: usize = 16;

impl AgentSession {
    pub const COLLECTION: &'static str = "agent_sessions";

    /// Whether `user` drives this session: its owner, or one the owner named.
    /// ⚠️ Not enough to prompt it: the session must be live and the driver
    /// still able to read it ([`crate::access`]).
    pub fn drives(&self, user: ObjectId) -> bool {
        user == self.owner_id || self.drivers.contains(&user)
    }

    /// Everyone who drives it: the owner first.
    pub fn all_drivers(&self) -> Vec<ObjectId> {
        let mut all = vec![self.owner_id];
        all.extend(self.drivers.iter().filter(|d| **d != self.owner_id));
        all
    }

    /// How the session authors its room's messages.
    pub fn author_display(&self) -> String {
        if self.location.device_name.is_empty() {
            "Claude".to_string()
        } else {
            format!("Claude · {}", self.location.device_name)
        }
    }
}

/// A turn's stub message in the session's room.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnStub {
    pub turn: u32,
    pub message_id: ObjectId,
}

/// What the API answers about a session: ids as hex, times as RFC 3339.
/// Never the stored document itself — a `bson::DateTime` serialises to JSON as
/// an object, and a client reading it as a string gets something truthy and
/// wrong.
#[derive(Serialize, Debug, Clone)]
pub struct SessionView {
    pub id: String,
    pub owner_id: String,
    /// P1c — who else drives it (hex), the owner not listed.
    pub drivers: Vec<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_id: Option<String>,
    pub harness: String,
    pub harness_session: String,
    pub device_id: String,
    /// The device's name when the session started (P0d-1), for a list that
    /// should not need a second lookup per row. Empty for an older record.
    pub device_name: String,
    pub folder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub status: SessionStatus,
    pub fence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// P1e — the brain revision its core memory was rendered from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brain_rev: Option<i64>,
    /// P1j — `started` or `adopted` (read-only: nobody prompts it).
    pub origin: SessionOrigin,
}

fn rfc3339(t: DateTime) -> String {
    t.try_to_rfc3339_string().unwrap_or_default()
}

impl From<&AgentSession> for SessionView {
    fn from(s: &AgentSession) -> Self {
        Self {
            id: s.id.map(|i| i.to_hex()).unwrap_or_default(),
            owner_id: s.owner_id.to_hex(),
            drivers: s.drivers.iter().map(|d| d.to_hex()).collect(),
            title: s.title.clone(),
            room_id: s.room_id.map(|r| r.to_hex()),
            harness: s.harness.id.clone(),
            harness_session: s.harness.session.clone(),
            device_id: s.location.device_id.to_hex(),
            device_name: s.location.device_name.clone(),
            folder: s.location.folder.clone(),
            account: s.location.account.clone(),
            status: s.status,
            fence: u64::try_from(s.fence).unwrap_or_default(),
            refusal: s.refusal.clone(),
            end_reason: s.end_reason.clone(),
            detail: s.detail.clone(),
            created_at: rfc3339(s.created_at),
            updated_at: rfc3339(s.updated_at),
            accepted_at: s.accepted_at.map(rfc3339),
            ended_at: s.ended_at.map(rfc3339),
            brain_rev: s.brain_rev,
            origin: s.origin,
        }
    }
}

/// FR-90 P1a-2 — one approval in a session (`agent_approvals`): that a tool
/// call waited for a driver, how it ended, who answered and when — and never
/// which tool or what it would do, which the device's frame cannot carry.
///
/// ⚠️ `answered_by` is the DEVICE's report of the grant whose answer it took:
/// the answer travels over the viewer peer, so no server decision stands
/// behind it the way one stands behind `hive_audit` (the `ssh_activity` rule).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentApproval {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub session_id: ObjectId,
    pub device_id: ObjectId,
    /// The device's id for it — unique within the session.
    pub approval_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// [`roomler_ai_remote_control::hive::HiveApprovalStatus::as_str`].
    pub status: String,
    /// The stub in the session's room, edited when it ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<ObjectId>,
    pub requested_at: DateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime>,
}

impl AgentApproval {
    pub const COLLECTION: &'static str = "agent_approvals";
}

/// One server decision about a session (`hive_audit`): every start ATTEMPT,
/// refused or sent, every stop, every view, and every change of who takes
/// part. Written by the server from its own decision — authoritative, unlike
/// what a device reports. 90-day TTL.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HiveAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// The acting user.
    pub user_id: ObjectId,
    /// Whom the action was about, when not the actor: the participant added,
    /// changed or removed (P1c); the fact written (P1e).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<ObjectId>,
    /// The device a session event concerns, or a device-scope fact's device.
    /// Absent for an org or a user fact (P1e), which concern none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<ObjectId>,
    /// Absent for a start refused before a session was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<ObjectId>,
    /// `start` | `stop` | `view` | `participant` | `brain` | `policy` (P2c-2a).
    pub action: String,
    /// `sent` | `queued` | `refused`; for `participant`, the role given —
    /// `driver` | `reader` — or `removed`; for `brain`, `added` | `edited` |
    /// `archived` | `refused`; for `policy`, `set` | `refused`.
    pub outcome: String,
    /// Why, for `refused` — the server's own reason word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub at: DateTime,
}

impl HiveAuditEvent {
    pub const COLLECTION: &'static str = "hive_audit";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_spellings_are_locked_and_match_serde() {
        let all = [
            (SessionStatus::Starting, "starting"),
            (SessionStatus::Idle, "idle"),
            (SessionStatus::Running, "running"),
            (SessionStatus::AwaitingApproval, "awaiting_approval"),
            (SessionStatus::Stopping, "stopping"),
            (SessionStatus::Ended, "ended"),
            (SessionStatus::Refused, "refused"),
            (SessionStatus::Lost, "lost"),
        ];
        for (s, w) in all {
            assert_eq!(s.as_str(), w);
            assert_eq!(serde_json::to_value(s).unwrap(), serde_json::json!(w));
            assert_eq!(
                bson::to_bson(&s).unwrap(),
                bson::Bson::String(w.into()),
                "the database spelling is the filter spelling"
            );
        }
    }

    /// LIVE and the terminal set partition the statuses, and a run-state
    /// report can never move a session that is being stopped.
    #[test]
    fn live_and_terminal_partition_and_stopping_is_not_reportable() {
        for s in SessionStatus::LIVE {
            assert!(!s.is_terminal(), "{s:?}");
        }
        for s in [
            SessionStatus::Ended,
            SessionStatus::Refused,
            SessionStatus::Lost,
        ] {
            assert!(s.is_terminal());
            assert!(!SessionStatus::LIVE.contains(&s));
        }
        assert!(!SessionStatus::REPORTABLE.contains(&SessionStatus::Stopping));
        for s in SessionStatus::REPORTABLE {
            assert!(SessionStatus::LIVE.contains(&s));
        }
    }

    /// What counts as the device's word that it launched a session: only
    /// statuses its own report sets. Never `starting` (the server's, when it
    /// sends the start) nor `stopping` (the server's, when it orders the
    /// stop) — a start the device has said nothing about stays reconcile's.
    #[test]
    fn launched_is_what_only_a_device_report_sets() {
        for s in SessionStatus::LAUNCHED {
            assert!(SessionStatus::REPORTABLE.contains(&s), "{s:?}");
        }
        assert!(!SessionStatus::LAUNCHED.contains(&SessionStatus::Starting));
        assert!(!SessionStatus::LAUNCHED.contains(&SessionStatus::Stopping));
    }

    #[test]
    fn the_view_renders_times_as_strings_and_ids_as_hex() {
        let now = DateTime::now();
        let s = AgentSession {
            id: Some(ObjectId::new()),
            tenant_id: ObjectId::new(),
            owner_id: ObjectId::new(),
            drivers: Vec::new(),
            title: "t".into(),
            room_id: None,
            last_turn: None,
            harness: HarnessRef {
                id: "claude-code".into(),
                session: "u".into(),
            },
            location: SessionLocation {
                device_id: ObjectId::new(),
                device_name: "mars".into(),
                folder: "/src".into(),
                account: None,
            },
            status: SessionStatus::Starting,
            fence: 1,
            accepted_at: None,
            refusal: None,
            end_reason: None,
            detail: None,
            created_at: now,
            updated_at: now,
            ended_at: None,
            brain_rev: None,
            origin: SessionOrigin::Started,
        };
        let v = serde_json::to_value(SessionView::from(&s)).unwrap();
        assert!(v["created_at"].is_string(), "{v}");
        assert_eq!(v["id"], s.id.unwrap().to_hex());
        assert_eq!(v["device_id"], s.location.device_id.to_hex());
        assert_eq!(v["fence"], 1);
        assert!(v.get("accepted_at").is_none(), "absent, not null: {v}");
        assert_eq!(v["drivers"], serde_json::json!([]), "always a list: {v}");
        assert_eq!(s.author_display(), "Claude · mars");
        let mut nameless = s.clone();
        nameless.location.device_name.clear();
        assert_eq!(nameless.author_display(), "Claude");
    }

    /// The owner always drives and is never stored as a driver; an older
    /// record with no `drivers` field reads as "the owner alone".
    #[test]
    fn the_owner_always_drives_and_others_only_when_named() {
        let owner = ObjectId::new();
        let named = ObjectId::new();
        let stranger = ObjectId::new();
        let raw = bson::doc! {
            "tenant_id": ObjectId::new(),
            "owner_id": owner,
            "title": "t",
            "harness": { "id": "claude-code", "session": "u" },
            "location": { "device_id": ObjectId::new(), "folder": "/src" },
            "status": "idle",
            "fence": 1_i64,
            "created_at": DateTime::now(),
            "updated_at": DateTime::now(),
        };
        let mut s: AgentSession = bson::from_document(raw).expect("a P0 record still reads");
        assert!(s.drivers.is_empty());
        assert!(s.drives(owner));
        assert!(!s.drives(named));
        assert_eq!(s.all_drivers(), vec![owner]);

        s.drivers.push(named);
        assert!(s.drives(named));
        assert!(!s.drives(stranger));
        assert_eq!(s.all_drivers(), vec![owner, named]);

        // An owner listed by mistake is not counted twice.
        s.drivers.push(owner);
        assert_eq!(s.all_drivers(), vec![owner, named]);
    }

    /// FR-90 P1j — the origin is stored only for an adopted session, so every
    /// started record reads as before; and an adopted one is driven by
    /// nobody, its owner included: the terminal holds the harness.
    #[test]
    fn an_adopted_session_is_driven_by_nobody_and_a_started_one_stores_no_origin() {
        let owner = ObjectId::new();
        let raw = bson::doc! {
            "tenant_id": ObjectId::new(),
            "owner_id": owner,
            "title": "t",
            "harness": { "id": "claude-code", "session": "u" },
            "location": { "device_id": ObjectId::new(), "folder": "/src" },
            "status": "idle",
            "fence": 1_i64,
            "created_at": DateTime::now(),
            "updated_at": DateTime::now(),
        };
        let mut s: AgentSession = bson::from_document(raw).expect("a record before P1j reads");
        assert_eq!(s.origin, SessionOrigin::Started);
        assert!(
            !bson::to_document(&s).unwrap().contains_key("origin"),
            "a started session stores no origin"
        );
        assert!(crate::access::drives_now(&s, owner));

        s.origin = SessionOrigin::Adopted;
        assert_eq!(
            bson::to_document(&s).unwrap().get_str("origin").unwrap(),
            "adopted"
        );
        assert!(
            !crate::access::drives_now(&s, owner),
            "its owner does not drive an adopted session"
        );
        assert_eq!(
            SessionView::from(&s).origin,
            SessionOrigin::Adopted,
            "and the UI is told"
        );
    }
}

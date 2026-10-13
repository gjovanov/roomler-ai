// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2f-3b — a fork: `POST …/hive/session/{session_id}/fork`, a new
//! session made from this one's conversation, its owner's, on a device that
//! holds a copy of this one, while this one goes on (design §6.5, spec §3b
//! "Fork"). The device's half is P2f-3a: a start that names the session it
//! forks (`rc:hive.start`'s `fork_of`) is put in place from that device's
//! copy, under the fork's own ids.
//!
//! # Its gates, in order
//!
//! 1. membership, and the replicaset's switch (`hive.replicaset`): off, the
//!    route is not there, since only a replicated session keeps the
//!    checkpoints a fork is made from;
//! 2. the session, the caller's own — 404 otherwise, as for a promotion;
//! 3. the device, LIVE in this org — 404 otherwise;
//! 4. the org is not archived — `org_archived`; `HIVE_RUN` — `no_permission`;
//! 5. a session that can be forked: no adopted terminal session —
//!    `not_forkable`; one with copies — `not_replicated`; a checkpoint of a
//!    turn kept — `no_conversation`;
//! 6. no move of it in flight — `in_progress`;
//! 7. the device holds a copy: the session's primary, or a member that took
//!    its copy — `no_copy` (a device with none is P2f-3c's);
//! 8. not the session's own folder on its own device — `same_folder`;
//! 9. the device carries every restricted tag the session carries —
//!    `restricted`: a fork of a restricted session is restricted;
//! 10. the device is connected here and starts forks (`hive-fork`) —
//!     `device_offline` / `device_unsupported`;
//! 11. the per-(user, device) ceiling — `rate_limited`, LAST.
//!
//! Then a member's copy is waited for, within `hive.fork_copy_secs`, until it
//! holds the last checkpoint the session's primary reported: a fork is made
//! at the last checkpoint the device's copy holds — `copy_behind` otherwise.
//! The primary's own copy is current by definition.
//!
//! From 4 on, a refusal is a 200 carrying the reason, and is audited
//! (`fork`, naming the session it forks), as a start's is. The fork itself
//! is made and started as any session is ([`crate::routes::begin`]): its own
//! harness id and room, its own replicaset under the rules, with the
//! parent's restricted tags, and every start of it naming its parent.

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State},
};
use bson::oid::ObjectId;
use roomler_ai_db::models::role::permissions;
use roomler_ai_remote_control::hive::hive_limits;
use roomler_core::{ApiError, extractors::auth::AuthUser};
use serde::Deserialize;
use tracing::info;

use crate::HiveState;
use crate::model::{AgentSession, MemberState, ParentLink, SessionOrigin};
use crate::placement::{MemberRole, carries};
use crate::room;
use crate::routes::{
    Audit, NewSession, StartResponse, clean_folder, clean_title, member_tenant, parse_oid,
};

/// `POST …/hive/session/{session_id}/fork`.
#[derive(Debug, Deserialize)]
pub struct ForkBody {
    /// A device that holds a copy of the session: its primary, or a member.
    pub device_id: String,
    /// Where the fork runs there, as typed. The device resolves it.
    pub folder: String,
    /// Defaults to "Fork of <the session's title>".
    #[serde(default)]
    pub title: Option<String>,
}

/// Why the SERVER refused a fork. The device's own refusals are the start's
/// words, on the fork's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkDenyReason {
    OrgArchived,
    NoPermission,
    NotForkable,
    NotReplicated,
    NoConversation,
    InProgress,
    NoCopy,
    SameFolder,
    Restricted,
    CopyBehind,
    DeviceOffline,
    DeviceUnsupported,
    RateLimited,
}

impl ForkDenyReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OrgArchived => "org_archived",
            Self::NoPermission => "no_permission",
            Self::NotForkable => "not_forkable",
            Self::NotReplicated => "not_replicated",
            Self::NoConversation => "no_conversation",
            Self::InProgress => "in_progress",
            Self::NoCopy => "no_copy",
            Self::SameFolder => "same_folder",
            Self::Restricted => "restricted",
            Self::CopyBehind => "copy_behind",
            Self::DeviceOffline => "device_offline",
            Self::DeviceUnsupported => "device_unsupported",
            Self::RateLimited => "rate_limited",
        }
    }

    /// What the person is told — each names its own next step.
    pub const fn message(self) -> &'static str {
        match self {
            Self::OrgArchived => "this organization is archived; new sessions are blocked",
            Self::NoPermission => {
                "your role does not include \"Run agent sessions\" (HIVE_RUN); \
                 an owner can grant it"
            }
            Self::NotForkable => {
                "the session runs in a terminal (adopted), so no copy of it is kept to fork from"
            }
            Self::NotReplicated => {
                "the session has no copies, so no checkpoint of it is kept to fork from"
            }
            Self::NoConversation => "the session has kept no turn yet: there is nothing to fork",
            Self::InProgress => "the session is moving; fork it once the move is done",
            Self::NoCopy => {
                "the device holds no copy of the session: fork it on its device, or on a \
                 device that holds a copy"
            }
            Self::SameFolder => {
                "a fork runs in a folder of its own: name another folder than the session's"
            }
            Self::Restricted => "the device lacks a tag the session is restricted to",
            Self::CopyBehind => {
                "the device's copy did not reach the session's last checkpoint in time; \
                 try again in a moment"
            }
            Self::DeviceOffline => "the device is offline",
            Self::DeviceUnsupported => {
                "the device's agent does not start forks — update it, or it was built \
                 without agent sessions"
            }
            Self::RateLimited => "too many sessions started on this device in the last minute",
        }
    }
}

/// The last checkpoint the copy of `s` on `device` holds, as `device` last
/// reported it: its `seq`.
fn checkpoint_on(s: &AgentSession, device: ObjectId) -> Option<i64> {
    s.replicaset
        .as_ref()?
        .members
        .iter()
        .find(|m| m.device_id == device)?
        .tip
        .as_ref()?
        .checkpoint_seq
}

/// Whether `device` holds a copy of `s`: its primary, or a member that took
/// its copy.
fn holds_copy(s: &AgentSession, device: ObjectId) -> bool {
    device == s.location.device_id
        || s.replicaset.as_ref().is_some_and(|r| {
            r.members.iter().any(|m| {
                m.device_id == device
                    && m.role != MemberRole::Primary.as_str()
                    && m.state == MemberState::Joined
            })
        })
}

/// The restricted tags `s` carries.
fn restricted_of(s: &AgentSession) -> Vec<String> {
    s.replicaset
        .as_ref()
        .map(|r| r.restricted_tags.clone())
        .unwrap_or_default()
}

/// Gates 4 on, past the session and the device lookups. The outer `Result`
/// is the infrastructure, the inner one the policy — as a start's.
async fn authorize(
    state: &HiveState,
    s: &AgentSession,
    target: ObjectId,
    target_tags: &[String],
    folder: &str,
    user_id: ObjectId,
) -> Result<Result<(), ForkDenyReason>, ApiError> {
    let tid = s.tenant_id;
    let tenant = state.tenants.base.find_by_id(tid).await?;
    if tenant.is_archived {
        return Ok(Err(ForkDenyReason::OrgArchived));
    }
    let perms = state.tenants.get_member_permissions(tid, user_id).await?;
    if !permissions::has(perms, permissions::HIVE_RUN) {
        return Ok(Err(ForkDenyReason::NoPermission));
    }
    if s.origin == SessionOrigin::Adopted {
        return Ok(Err(ForkDenyReason::NotForkable));
    }
    if !crate::replica::replicated(s) {
        return Ok(Err(ForkDenyReason::NotReplicated));
    }
    // A turn with a stub, and a checkpoint its primary kept: a checkpoint
    // taken before the first turn holds no conversation.
    if s.last_turn.is_none() || checkpoint_on(s, s.location.device_id).is_none() {
        return Ok(Err(ForkDenyReason::NoConversation));
    }
    if s.promotion
        .as_ref()
        .is_some_and(|p| p.in_flight(bson::DateTime::now()))
    {
        return Ok(Err(ForkDenyReason::InProgress));
    }
    if !holds_copy(s, target) {
        return Ok(Err(ForkDenyReason::NoCopy));
    }
    if target == s.location.device_id && folder == s.location.folder {
        return Ok(Err(ForkDenyReason::SameFolder));
    }
    if !restricted_of(s).iter().all(|t| carries(target_tags, t)) {
        return Ok(Err(ForkDenyReason::Restricted));
    }
    match state.fleet.rc_hub.agent_supports_hive_fork(target, tid) {
        None => return Ok(Err(ForkDenyReason::DeviceOffline)),
        Some(false) => return Ok(Err(ForkDenyReason::DeviceUnsupported)),
        Some(true) => {}
    }
    // LAST: only a fork that would reach the device spends a token.
    if !state
        .start_limiter
        .check(user_id, target, hive_limits::START_RATE_PER_MINUTE)
    {
        return Ok(Err(ForkDenyReason::RateLimited));
    }
    Ok(Ok(()))
}

/// How often a fork reads the record again while it waits for the device's
/// copy.
const COPY_POLL: Duration = Duration::from_millis(250);

/// Wait, within `state.fork_copy`, until the copy of `sid` on `target` holds
/// `wanted`, the last checkpoint the session's primary reported when the fork
/// was asked. `true` once it does.
async fn copy_current(
    state: &HiveState,
    sid: ObjectId,
    target: ObjectId,
    wanted: i64,
) -> Result<bool, ApiError> {
    let deadline = tokio::time::Instant::now() + state.fork_copy;
    loop {
        let Some(s) = state.sessions.find(sid).await? else {
            return Ok(false);
        };
        if checkpoint_on(&s, target).is_some_and(|held| held >= wanted) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(COPY_POLL).await;
    }
}

/// `POST /api/tenant/{tenant_id}/hive/session/{session_id}/fork` — the owner
/// makes a new session from this one's conversation, on a device that holds
/// a copy of it. Answered as a start is, once the device said whether it
/// launched the fork (or has not, within the start's bound).
pub async fn fork(
    State(state): State<HiveState>,
    auth: AuthUser,
    Path((tenant_id, session_id)): Path<(String, String)>,
    Json(body): Json<ForkBody>,
) -> Result<Json<StartResponse>, ApiError> {
    let tid = member_tenant(&state, &tenant_id, &auth).await?;
    if !state.replicaset {
        return Err(ApiError::NotFound(
            "forking sessions is off on this server".into(),
        ));
    }
    let sid = parse_oid(&session_id, "session_id")?;
    // The owner's alone: anyone else's read of the id is a 404.
    let parent = state.sessions.find_owned(tid, auth.user_id, sid).await?;
    let target = parse_oid(&body.device_id, "device_id")?;
    let folder = clean_folder(&body.folder)?;
    let default_title = format!("Fork of {}", parent.title);
    let title = clean_title(
        Some(body.title.as_deref().unwrap_or(&default_title)),
        &folder,
    )?;
    // LIVE in this org: a removed or foreign device answers like a bogus one.
    let device = state.fleet.agents.find_live_in_tenant(tid, target).await?;
    let refused = |reason: ForkDenyReason| Audit {
        tenant_id: tid,
        user_id: auth.user_id,
        target_id: Some(sid),
        device_id: target,
        session_id: None,
        action: "fork",
        outcome: "refused",
        reason: Some(reason.as_str()),
    };
    let decided =
        match authorize(&state, &parent, target, &device.tags, &folder, auth.user_id).await? {
            Ok(()) => {
                // A member's copy must hold the session's last checkpoint first:
                // the fork is made at the last one it holds.
                let wanted = checkpoint_on(&parent, parent.location.device_id).unwrap_or(0);
                if target == parent.location.device_id
                    || copy_current(&state, sid, target, wanted).await?
                {
                    Ok(())
                } else {
                    Err(ForkDenyReason::CopyBehind)
                }
            }
            Err(reason) => Err(reason),
        };
    if let Err(reason) = decided {
        info!(
            tenant = %tid, session = %sid, device = %target, user = %auth.user_id,
            reason = reason.as_str(), "hive: fork refused"
        );
        refused(reason).write(&state).await;
        return Ok(Json(StartResponse::refused_with(
            reason.as_str(),
            reason.message(),
        )));
    }

    let by = state
        .users
        .base
        .find_by_id(auth.user_id)
        .await?
        .display_name;
    let restricted = restricted_of(&parent);
    let new = NewSession {
        tid,
        owner: auth.user_id,
        device_id: target,
        device: &device,
        folder: folder.clone(),
        title,
        harness: parent.harness.id.clone(),
        parent: Some(ParentLink {
            session_id: sid,
            harness_session: parent.harness.session.clone(),
        }),
        restricted: &restricted,
        action: "fork",
        opening: Some(room::forked_from_note(&parent, &by)),
    };
    let answer = crate::routes::begin(&state, new).await?;
    // The parent's room hears of a fork its device took, or has yet to
    // answer; one refused is the fork's own room's to say.
    if let Some(fork) = answer
        .session
        .as_ref()
        .filter(|_| answer.outcome != "refused")
    {
        room::note(
            &state,
            &parent,
            room::forked_note(&fork.title, &device.name, &folder, &by),
        )
        .await;
    }
    Ok(Json(answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MemberTip, ReplicaMember, Replicaset};
    use bson::DateTime;

    fn tip(checkpoint: Option<i64>) -> Option<MemberTip> {
        Some(MemberTip {
            fence: 1,
            seq: 40,
            hash: "a".repeat(64),
            checkpoint_seq: checkpoint,
            checkpoint_hash: checkpoint.map(|_| "b".repeat(64)),
            at: DateTime::now(),
        })
    }

    fn member(device: ObjectId, role: MemberRole, state: MemberState) -> ReplicaMember {
        ReplicaMember {
            device_id: device,
            role: role.as_str().to_string(),
            state,
            refusal: None,
            answered_at: None,
            tip: tip(Some(30)),
        }
    }

    fn session(primary: ObjectId, members: Vec<ReplicaMember>) -> AgentSession {
        use crate::model::{HarnessRef, SessionLocation, SessionStatus};
        let now = DateTime::now();
        AgentSession {
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
                device_id: primary,
                device_name: String::new(),
                folder: "/src".into(),
                account: None,
            },
            status: SessionStatus::Idle,
            fence: 1,
            accepted_at: Some(now),
            refusal: None,
            end_reason: None,
            detail: None,
            created_at: now,
            updated_at: now,
            ended_at: None,
            brain_rev: None,
            origin: SessionOrigin::Started,
            replicaset: Some(Replicaset {
                policy_revision: 0,
                min: 2,
                max: 3,
                restricted_tags: vec!["prod".into()],
                members,
                short: None,
                placed_at: now,
            }),
            promotion: None,
            fences: Vec::new(),
            moved: None,
            parent: None,
        }
    }

    /// A device holds a copy as the session's primary, or as a member that
    /// took its copy; never as one still to join, or leaving.
    #[test]
    fn a_copy_is_held_by_the_primary_or_a_member_that_took_it() {
        let (primary, joined, pending) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
        let s = session(
            primary,
            vec![
                member(primary, MemberRole::Primary, MemberState::Joined),
                member(joined, MemberRole::Owner, MemberState::Joined),
                member(pending, MemberRole::Owner, MemberState::Pending),
            ],
        );
        assert!(holds_copy(&s, primary));
        assert!(holds_copy(&s, joined));
        assert!(!holds_copy(&s, pending), "still to join: no copy yet");
        assert!(!holds_copy(&s, ObjectId::new()));
        assert_eq!(checkpoint_on(&s, joined), Some(30));
        assert_eq!(checkpoint_on(&s, ObjectId::new()), None);
        assert_eq!(restricted_of(&s), ["prod"]);
    }

    /// Every refusal word is its own, and names a next step.
    #[test]
    fn every_refusal_has_its_own_word_and_words() {
        use ForkDenyReason::*;
        let all = [
            OrgArchived,
            NoPermission,
            NotForkable,
            NotReplicated,
            NoConversation,
            InProgress,
            NoCopy,
            SameFolder,
            Restricted,
            CopyBehind,
            DeviceOffline,
            DeviceUnsupported,
            RateLimited,
        ];
        let words: std::collections::HashSet<&str> = all.iter().map(|r| r.as_str()).collect();
        assert_eq!(words.len(), all.len(), "each word once");
        assert!(all.iter().all(|r| !r.message().is_empty()));
    }
}

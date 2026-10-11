// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2c-3a — the server's half of membership: the joins it sends a
//! session's members, and what it learns back, each member's answer and the
//! tip of its copy (spec §3b, "What the server learns").
//!
//! | When | What |
//! |---|---|
//! | the primary accepts a start | each member placement chose is sent `rc:hive.replica.join` ([`send_joins`]) |
//! | a device that holds copies connects | every join it has still to answer is sent again ([`reconcile_joins`]) |
//! | `rc:hive.replica.join_ack` | the member is `joined`, or `refused` with the device's word ([`on_join_ack`]) |
//! | `rc:hive.replica.tip`, and each entry of `rc:hive.replica.manifest` | the member's tip is kept ([`on_tip`]) |
//!
//! ⚠️ A join is pushed only to a connection advertising `hive-replica`: the
//! device answers it, and its owner lets it hold copies. Any other connection
//! receives nothing, and the join waits, pending, for one that does.
//! ⚠️ Everything a device says here is a claim, applied only where the record
//! names that device as a member: a frame about someone else's session, or
//! from a device placement did not choose, matches nothing.

use bson::{DateTime, oid::ObjectId};
use roomler_ai_remote_control::{
    hive::{
        HiveJoinRefusal, HiveReplicaManifestEntry, HiveReplicaRole, is_hash_hex, replica_limits,
    },
    signaling::ServerMsg,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::HiveState;
use crate::agent_socket::clamp_words;
use crate::model::{AgentSession, MemberState, MemberTip};
use crate::placement::MemberRole;

/// The role a member was placed in, as the join names it. `None` for the
/// primary, which is never sent one, and for a word this build cannot name.
fn wire_role(role: &str) -> Option<HiveReplicaRole> {
    if role == MemberRole::Archive.as_str() {
        Some(HiveReplicaRole::Archive)
    } else if role == MemberRole::Owner.as_str() {
        Some(HiveReplicaRole::Owner)
    } else {
        None
    }
}

/// Whether the session's start tells its primary to checkpoint: placement
/// chose at least one member besides it.
pub(crate) fn replicated(s: &AgentSession) -> bool {
    s.replicaset.as_ref().is_some_and(|r| {
        r.members
            .iter()
            .any(|m| m.role != MemberRole::Primary.as_str())
    })
}

/// The joins of `s` still to send: its members other than the primary that
/// have not answered, each with its role.
fn pending(s: &AgentSession) -> Vec<(ObjectId, HiveReplicaRole)> {
    let Some(r) = &s.replicaset else {
        return Vec::new();
    };
    r.members
        .iter()
        .filter(|m| m.state == MemberState::Pending)
        .filter_map(|m| Some((m.device_id, wire_role(&m.role)?)))
        .collect()
}

/// Push one join. `false` = it did not leave: the device is not connected
/// here, or this connection does not hold copies. It stays pending either
/// way, and goes when a connection that holds copies asks.
fn push_join(
    state: &HiveState,
    tenant_id: ObjectId,
    device_id: ObjectId,
    session_id: ObjectId,
    fence: u64,
    role: HiveReplicaRole,
) -> bool {
    let msg = ServerMsg::HiveReplicaJoin {
        session_id,
        fence,
        role: Some(role),
    };
    match state
        .fleet
        .rc_hub
        .push_hive_replica(device_id, tenant_id, msg)
    {
        Ok(()) => {
            info!(
                session = %session_id, device = %device_id, role = role.as_str(),
                "hive: join sent"
            );
            true
        }
        Err(e) => {
            debug!(session = %session_id, device = %device_id, %e, "hive: join not sent — it stays pending");
            false
        }
    }
}

/// The primary accepted the session's start: send each member placement
/// chose its join. Before that the session may never run, and a member would
/// hold a copy of nothing.
pub(crate) async fn send_joins(state: &HiveState, session_id: ObjectId) {
    let s = match state.sessions.find(session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            warn!(session = %session_id, %e, "hive: a session's members were not read — its joins wait");
            return;
        }
    };
    let fence = u64::try_from(s.fence).unwrap_or_default();
    for (device_id, role) in pending(&s) {
        push_join(state, s.tenant_id, device_id, session_id, fence, role);
    }
}

/// A device connected: send it every join it has still to answer, if THIS
/// connection holds copies. `conn` is the connection's sender in the hub,
/// read as `reconcile_on_connect` reads it: a connection a newer one
/// displaced decides nothing.
pub(crate) async fn reconcile_joins(
    state: &HiveState,
    tenant_id: ObjectId,
    device_id: ObjectId,
    conn: &mpsc::Sender<ServerMsg>,
) {
    if state
        .fleet
        .rc_hub
        .agent_supports_hive_replica_on(device_id, conn)
        != Some(true)
    {
        return;
    }
    let sessions = match state.sessions.pending_joins(tenant_id, device_id).await {
        Ok(s) => s,
        Err(e) => {
            warn!(device = %device_id, %e, "hive: the device's pending joins were not read");
            return;
        }
    };
    for s in sessions {
        let Some(sid) = s.id else { continue };
        let fence = u64::try_from(s.fence).unwrap_or_default();
        for (member, role) in pending(&s) {
            if member == device_id {
                push_join(state, tenant_id, device_id, sid, fence, role);
            }
        }
    }
}

/// `rc:hive.replica.join_ack` — the member's answer, kept on the record.
pub(crate) async fn on_join_ack(
    state: &HiveState,
    device_id: ObjectId,
    session_id: ObjectId,
    fence: u64,
    refused: Option<HiveJoinRefusal>,
    detail: Option<String>,
) {
    let detail = clamp_words(
        detail,
        roomler_ai_remote_control::hive::hive_limits::MAX_DETAIL_LEN,
    );
    match state
        .sessions
        .member_answer(
            session_id,
            device_id,
            fence,
            refused.map(HiveJoinRefusal::as_str),
        )
        .await
    {
        Ok(true) => info!(
            session = %session_id, device = %device_id,
            refused = refused.map(HiveJoinRefusal::as_str), detail = detail.as_deref(),
            "hive: a member answered its join"
        ),
        Ok(false) => debug!(
            session = %session_id, device = %device_id, fence,
            "hive: a join answer matched no member — not this device's, or a stale fence"
        ),
        Err(e) => warn!(session = %session_id, %e, "hive: a join answer was not recorded"),
    }
}

/// `rc:hive.replica.tip`, or one entry of a manifest: the member's tip, kept
/// on the record. A hash that is not one is dropped with its report, never
/// stored.
pub(crate) async fn on_tip(state: &HiveState, device_id: ObjectId, tip: HiveReplicaManifestEntry) {
    let well_formed =
        is_hash_hex(&tip.hash) && tip.checkpoint.as_ref().is_none_or(|c| is_hash_hex(&c.hash));
    if !well_formed {
        debug!(session = %tip.session_id, device = %device_id, "hive: a tip with a malformed hash — dropped");
        return;
    }
    let stored = MemberTip {
        fence: i64::try_from(tip.fence).unwrap_or(i64::MAX),
        seq: i64::try_from(tip.seq).unwrap_or(i64::MAX),
        hash: tip.hash,
        checkpoint_seq: tip
            .checkpoint
            .as_ref()
            .map(|c| i64::try_from(c.seq).unwrap_or(i64::MAX)),
        checkpoint_hash: tip.checkpoint.map(|c| c.hash),
        at: DateTime::now(),
    };
    match state
        .sessions
        .member_tip(tip.session_id, device_id, &stored)
        .await
    {
        Ok(true) => debug!(
            session = %tip.session_id, device = %device_id, seq = stored.seq,
            "hive: a member's tip"
        ),
        Ok(false) => debug!(
            session = %tip.session_id, device = %device_id,
            "hive: a tip from a device the session does not hold as a member"
        ),
        Err(e) => warn!(session = %tip.session_id, %e, "hive: a tip was not recorded"),
    }
}

/// `rc:hive.replica.manifest` — every session the device holds a copy of,
/// each tip kept as [`on_tip`] keeps one. An oversized list is no device's,
/// and changes nothing.
pub(crate) async fn on_manifest(
    state: &HiveState,
    device_id: ObjectId,
    sessions: Vec<HiveReplicaManifestEntry>,
) {
    if sessions.len() > replica_limits::MAX_REPLICA_MANIFEST {
        warn!(device = %device_id, entries = sessions.len(), "hive: an oversized replica manifest — ignored");
        return;
    }
    for tip in sessions {
        on_tip(state, device_id, tip).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        HarnessRef, ReplicaMember, Replicaset, SessionLocation, SessionOrigin, SessionStatus,
    };

    fn member(role: MemberRole, state: MemberState) -> ReplicaMember {
        ReplicaMember {
            device_id: ObjectId::new(),
            role: role.as_str().into(),
            state,
            refusal: None,
            answered_at: None,
            tip: None,
        }
    }

    fn session(members: Vec<ReplicaMember>) -> AgentSession {
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
                device_id: members[0].device_id,
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
                max: 4,
                restricted_tags: Vec::new(),
                members,
                short: None,
                placed_at: now,
            }),
        }
    }

    /// A session alone on its primary is not replicated; one with any other
    /// member is, whatever that member answered.
    #[test]
    fn a_session_is_replicated_when_it_has_another_member() {
        let alone = session(vec![member(MemberRole::Primary, MemberState::Joined)]);
        assert!(!replicated(&alone));
        let mut unplaced = alone.clone();
        unplaced.replicaset = None;
        assert!(!replicated(&unplaced));
        let two = session(vec![
            member(MemberRole::Primary, MemberState::Joined),
            member(MemberRole::Owner, MemberState::Refused),
        ]);
        assert!(replicated(&two));
    }

    /// Only members still pending get a join, each in its own role; the
    /// primary never does, nor a member that answered.
    #[test]
    fn only_pending_members_other_than_the_primary_are_joined() {
        let s = session(vec![
            member(MemberRole::Primary, MemberState::Pending),
            member(MemberRole::Archive, MemberState::Pending),
            member(MemberRole::Owner, MemberState::Joined),
            member(MemberRole::Owner, MemberState::Refused),
            member(MemberRole::Owner, MemberState::Pending),
        ]);
        let m = &s.replicaset.as_ref().unwrap().members;
        assert_eq!(
            pending(&s),
            [
                (m[1].device_id, HiveReplicaRole::Archive),
                (m[4].device_id, HiveReplicaRole::Owner),
            ]
        );
    }
}

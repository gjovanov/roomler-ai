// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! `agent_sessions`, `agent_approvals` and `hive_audit`.
//!
//! Every transition a DEVICE drives is a compare-and-set on three things the
//! frame cannot choose: the session's device (the authenticated socket's, not
//! a field in the frame), its fence, and the statuses it may leave. A stale
//! frame, a frame from another device, or one that crossed a stop on the wire
//! matches nothing and changes nothing — session ids are ObjectIds,
//! structured, not secret.

use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_remote_control::hive::{HiveApprovalStatus, HiveRunState};
use roomler_ai_services::dao::base::{
    BaseDao, DaoError, DaoResult, PaginatedResult, PaginationParams,
};

use crate::model::{
    AgentApproval, AgentSession, HiveAuditEvent, MemberState, MemberTip, Promotion, PromotionState,
    ReplicaMember, Replicaset, SessionStatus,
};
use crate::placement::MemberRole;

fn statuses(set: &[SessionStatus]) -> Bson {
    let words: Vec<Bson> = set.iter().map(|s| Bson::from(s.as_str())).collect();
    Bson::Document(doc! { "$in": words })
}

/// A wire fence as the database stores it. `None` = no stored session could
/// hold it (above `i64::MAX`), so the frame matches nothing.
fn stored_fence(fence: u64) -> Option<i64> {
    i64::try_from(fence).ok()
}

/// FR-90 P2e-3 — `fence` ended at the promotion's drained tip, as the record
/// keeps it (`agent_sessions.fences`). `None` before the primary drained.
fn fence_mark(p: &Promotion, fence: i64) -> Option<Bson> {
    let d = p.drained.as_ref()?;
    bson::to_bson(&crate::model::FenceMark {
        fence,
        seq: d.seq,
        hash: d.hash.clone(),
    })
    .ok()
}

pub struct AgentSessionDao {
    pub base: BaseDao<AgentSession>,
}

impl AgentSessionDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, AgentSession::COLLECTION),
        }
    }

    pub async fn create(&self, session: &AgentSession) -> DaoResult<ObjectId> {
        self.base.insert_one(session).await
    }

    /// One session, only if `owner_id` started it — anyone else's id is
    /// `NotFound`, the answer a bogus id gets, so a session's existence does
    /// not leak to other members.
    pub async fn find_owned(
        &self,
        tenant_id: ObjectId,
        owner_id: ObjectId,
        id: ObjectId,
    ) -> DaoResult<AgentSession> {
        self.base
            .find_one(doc! { "_id": id, "tenant_id": tenant_id, "owner_id": owner_id })
            .await?
            .ok_or(roomler_ai_services::dao::base::DaoError::NotFound)
    }

    /// The caller's own sessions, newest first.
    pub async fn list_owned(
        &self,
        tenant_id: ObjectId,
        owner_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<AgentSession>> {
        self.base
            .find_paginated(
                doc! { "tenant_id": tenant_id, "owner_id": owner_id },
                Some(doc! { "created_at": -1 }),
                params,
            )
            .await
    }

    pub async fn find(&self, id: ObjectId) -> DaoResult<Option<AgentSession>> {
        self.base.find_one(doc! { "_id": id }).await
    }

    /// One session of `tenant_id`, whoever started it — for a route that then
    /// decides who may see it ([`crate::access`]). Another org's id is
    /// `NotFound`.
    pub async fn find_in_tenant(
        &self,
        tenant_id: ObjectId,
        id: ObjectId,
    ) -> DaoResult<AgentSession> {
        self.base
            .find_one(doc! { "_id": id, "tenant_id": tenant_id })
            .await?
            .ok_or(DaoError::NotFound)
    }

    /// FR-90 P1c — name `user` a driver of a session `owner_id` started, or
    /// stop them driving it. Adding is held to [`crate::model::MAX_DRIVERS`]
    /// by the filter itself, so two owners' tabs racing cannot pass the cap.
    /// `false` = no such session of the owner's, or the cap is reached.
    pub async fn set_driver(
        &self,
        tenant_id: ObjectId,
        owner_id: ObjectId,
        id: ObjectId,
        user: ObjectId,
        drives: bool,
    ) -> DaoResult<bool> {
        let mut filter = doc! { "_id": id, "tenant_id": tenant_id, "owner_id": owner_id };
        let update = if drives {
            filter.insert(
                format!("drivers.{}", crate::model::MAX_DRIVERS - 1),
                doc! { "$exists": false },
            );
            doc! { "$addToSet": { "drivers": user } }
        } else {
            doc! { "$pull": { "drivers": user } }
        };
        self.base.update_one(filter, update).await
    }

    /// FR-90 P1c — `user` drives nothing in `tenant_id` any more: they left
    /// or were removed from the org. Returns how many sessions named them.
    pub async fn drop_driver_everywhere(
        &self,
        tenant_id: ObjectId,
        user: ObjectId,
    ) -> DaoResult<u64> {
        self.base
            .update_many(
                doc! { "tenant_id": tenant_id, "drivers": user },
                doc! { "$pull": { "drivers": user } },
            )
            .await
    }

    /// The device answered the start: accepted. Only an UNANSWERED start
    /// moves (`accepted_at` unset — a duplicate answer, the device having
    /// seen the start twice after a reconnect, changes nothing), and a stop
    /// already ordered stays ordered: the status is left as it is.
    ///
    /// ⚠️ ANY live status takes the answer, not only `starting`: the daemon
    /// reports `idle` the moment its harness is up and answers the start
    /// right after, so the state usually lands FIRST. Field, 2026-10-07:
    /// with `starting` alone every start lost its answer — the caller waited
    /// out the whole bound, the account was never recorded and the room got
    /// no started note. A terminal session still takes nothing.
    pub async fn accept(
        &self,
        id: ObjectId,
        device_id: ObjectId,
        fence: u64,
        account: Option<String>,
    ) -> DaoResult<bool> {
        let Some(fence) = stored_fence(fence) else {
            return Ok(false);
        };
        let mut set = doc! { "accepted_at": DateTime::now() };
        if let Some(a) = account {
            set.insert("location.account", a);
        }
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "location.device_id": device_id,
                    "fence": fence,
                    "status": statuses(&[
                        SessionStatus::Starting,
                        SessionStatus::Idle,
                        SessionStatus::Running,
                        SessionStatus::AwaitingApproval,
                        SessionStatus::Stopping,
                    ]),
                    "accepted_at": Bson::Null,
                },
                doc! { "$set": set },
            )
            .await
    }

    /// The device answered the start: refused, and by which gate. Terminal.
    /// An accepted start cannot be refused afterwards — a late, duplicated
    /// or forged refusal must not end a session that is running.
    pub async fn refuse(
        &self,
        id: ObjectId,
        device_id: ObjectId,
        fence: u64,
        word: &str,
        detail: Option<String>,
    ) -> DaoResult<bool> {
        let Some(fence) = stored_fence(fence) else {
            return Ok(false);
        };
        let now = DateTime::now();
        let mut set = doc! {
            "status": SessionStatus::Refused.as_str(),
            "refusal": word,
            "ended_at": now,
        };
        if let Some(d) = detail {
            set.insert("detail", d);
        }
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "location.device_id": device_id,
                    "fence": fence,
                    "status": statuses(&[SessionStatus::Starting, SessionStatus::Stopping]),
                    "accepted_at": Bson::Null,
                },
                doc! { "$set": set },
            )
            .await
    }

    /// The device reports a run state. `ended` has its own path
    /// ([`Self::report_ended`]); the others move only a session that is not
    /// being stopped.
    pub async fn report_state(
        &self,
        id: ObjectId,
        device_id: ObjectId,
        fence: u64,
        state: HiveRunState,
    ) -> DaoResult<bool> {
        let status = match state {
            HiveRunState::Idle => SessionStatus::Idle,
            HiveRunState::Running => SessionStatus::Running,
            HiveRunState::AwaitingApproval => SessionStatus::AwaitingApproval,
            HiveRunState::Ended => return Ok(false),
        };
        let Some(fence) = stored_fence(fence) else {
            return Ok(false);
        };
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "location.device_id": device_id,
                    "fence": fence,
                    "status": statuses(&SessionStatus::REPORTABLE),
                },
                doc! { "$set": { "status": status.as_str() } },
            )
            .await
    }

    /// The device reports the harness gone. A session that was being
    /// stopped ends `stopped` (the reason the stop recorded); any other live
    /// one ended on its own: `exited`.
    pub async fn report_ended(
        &self,
        id: ObjectId,
        device_id: ObjectId,
        fence: u64,
        detail: Option<String>,
    ) -> DaoResult<bool> {
        let Some(fence) = stored_fence(fence) else {
            return Ok(false);
        };
        let now = DateTime::now();
        let mut set = doc! { "status": SessionStatus::Ended.as_str(), "ended_at": now };
        if let Some(d) = &detail {
            set.insert("detail", d.clone());
        }
        let base = doc! { "_id": id, "location.device_id": device_id, "fence": fence };

        let mut stopping = base.clone();
        stopping.insert("status", SessionStatus::Stopping.as_str());
        if self
            .base
            .update_one(stopping, doc! { "$set": set.clone() })
            .await?
        {
            return Ok(true);
        }

        let mut live = base;
        live.insert("status", statuses(&SessionStatus::REPORTABLE));
        set.insert("end_reason", "exited");
        self.base.update_one(live, doc! { "$set": set }).await
    }

    /// The owner ordered a stop. `end_reason` is recorded now — it is why the
    /// session WILL end — and kept when the device confirms.
    pub async fn mark_stopping(
        &self,
        tenant_id: ObjectId,
        owner_id: ObjectId,
        id: ObjectId,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "tenant_id": tenant_id,
                    "owner_id": owner_id,
                    "status": statuses(&SessionStatus::REPORTABLE),
                },
                doc! { "$set": { "status": SessionStatus::Stopping.as_str(), "end_reason": "stopped" } },
            )
            .await
    }

    /// A start that will never be answered: the push failed, or its device
    /// came back after the redelivery window. Only an unanswered `starting`
    /// session moves.
    pub async fn mark_lost(&self, id: ObjectId, detail: &str) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "status": SessionStatus::Starting.as_str(),
                    "accepted_at": Bson::Null,
                },
                doc! { "$set": {
                    "status": SessionStatus::Lost.as_str(),
                    "end_reason": "never_answered",
                    "detail": detail,
                    "ended_at": DateTime::now(),
                } },
            )
            .await
    }

    /// End one live session from the server's side, without the device —
    /// for a device that can no longer be asked.
    pub async fn end_now(&self, id: ObjectId, end_reason: &str, detail: &str) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": id, "status": statuses(&SessionStatus::LIVE) },
                doc! { "$set": {
                    "status": SessionStatus::Ended.as_str(),
                    "end_reason": end_reason,
                    "detail": detail,
                    "ended_at": DateTime::now(),
                } },
            )
            .await
    }

    /// The live sessions matching `filter` (a tenant, an owner, a device).
    pub async fn live_matching(&self, mut filter: Document) -> DaoResult<Vec<AgentSession>> {
        filter.insert("status", statuses(&SessionStatus::LIVE));
        self.base.find_many(filter, None).await
    }

    /// End every live session matching `filter` — a removal cascade, which
    /// cannot wait for devices to confirm. Returns how many it ended.
    pub async fn end_all_matching(&self, mut filter: Document, end_reason: &str) -> DaoResult<u64> {
        filter.insert("status", statuses(&SessionStatus::LIVE));
        self.base
            .update_many(
                filter,
                doc! { "$set": {
                    "status": SessionStatus::Ended.as_str(),
                    "end_reason": end_reason,
                    "ended_at": DateTime::now(),
                } },
            )
            .await
    }

    /// Record the newest turn's stub — only when it IS newer, so two reports
    /// racing across pods cannot move it backwards.
    pub async fn set_last_turn(
        &self,
        id: ObjectId,
        stub: crate::model::TurnStub,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "$or": [
                        { "last_turn": Bson::Null },
                        { "last_turn.turn": { "$lt": i64::from(stub.turn) } },
                    ],
                },
                doc! { "$set": { "last_turn": {
                    "turn": i64::from(stub.turn),
                    "message_id": stub.message_id,
                } } },
            )
            .await
    }

    /// FR-90 P2e-1 — the sessions of `tenant_id` among `ids`, in one read: a
    /// manifest's names that the device does not run here, whose holder it
    /// may have to be told is someone else.
    pub async fn find_many_in_tenant(
        &self,
        tenant_id: ObjectId,
        ids: &[ObjectId],
    ) -> DaoResult<Vec<AgentSession>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<Bson> = ids.iter().map(|id| Bson::ObjectId(*id)).collect();
        self.base
            .find_many(doc! { "tenant_id": tenant_id, "_id": { "$in": ids } }, None)
            .await
    }

    /// FR-90 P2e-3 — a promotion begins: only for a session `owner_id`
    /// started, still live and launched on `from` at `fence`, with no
    /// promotion in flight (one past its deadline is replaced: the pod that
    /// started it is gone). `false` = one of those no longer holds — two
    /// promotions raced, and this one lost.
    pub async fn begin_promotion(
        &self,
        tenant_id: ObjectId,
        owner_id: ObjectId,
        id: ObjectId,
        p: &Promotion,
    ) -> DaoResult<bool> {
        let now = DateTime::now();
        let promotion = bson::to_bson(p)?;
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "tenant_id": tenant_id,
                    "owner_id": owner_id,
                    "fence": p.from_fence,
                    "location.device_id": p.from.device_id,
                    "status": statuses(&SessionStatus::REPORTABLE),
                    "$and": [
                        { "$or": [
                            { "accepted_at": { "$type": "date" } },
                            { "status": statuses(&SessionStatus::LAUNCHED) },
                        ] },
                        { "$or": [
                            { "promotion": Bson::Null },
                            { "promotion.deadline": { "$lt": now } },
                        ] },
                    ],
                },
                doc! { "$set": { "promotion": promotion } },
            )
            .await
    }

    /// FR-90 P2e-3 — a promotion takes its next step: only the one named,
    /// only from the state it is in, with the new state's deadline and what
    /// the step adds (`set`, the drained tip). `false` = it is not there any
    /// more, or took the step already.
    pub async fn advance_promotion(
        &self,
        id: ObjectId,
        pid: ObjectId,
        from: PromotionState,
        to: PromotionState,
        deadline: DateTime,
        mut set: Document,
    ) -> DaoResult<bool> {
        set.insert("promotion.state", to.as_str());
        set.insert("promotion.deadline", deadline);
        self.base
            .update_one(
                doc! { "_id": id, "promotion.id": pid, "promotion.state": from.as_str() },
                doc! { "$set": set },
            )
            .await
    }

    /// FR-90 P2e-3 — a promotion ends, done or failed: only the one named,
    /// only while it is in one of `states`. The record as it stood before,
    /// promotion and all, for the room's note; `None` = it is not there any
    /// more, or not in those states (another pod's frame ended it first).
    pub async fn end_promotion(
        &self,
        id: ObjectId,
        pid: ObjectId,
        states: &[PromotionState],
    ) -> DaoResult<Option<AgentSession>> {
        let words: Vec<Bson> = states.iter().map(|s| Bson::from(s.as_str())).collect();
        Ok(self
            .base
            .collection()
            .find_one_and_update(
                doc! { "_id": id, "promotion.id": pid, "promotion.state": { "$in": words } },
                doc! {
                    "$unset": { "promotion": "" },
                    "$set": { "updated_at": DateTime::now() },
                },
            )
            .return_document(mongodb::options::ReturnDocument::Before)
            .await?)
    }

    /// FR-90 P2e-3 — THE move: the lease goes from the primary at `fence` to
    /// the member the promotion names, at `fence + 1`, in one compare-and-set
    /// on everything the promotion read: the session still live and on the
    /// primary at that fence, the promotion still the one syncing, the member
    /// still a joined one. The member becomes the primary, owed its join at
    /// the new fence; the old primary becomes `old_role` in `old_state`;
    /// every other member that held or was taking its copy is owed its join
    /// at the new fence. The start is unanswered from here, as any start is.
    ///
    /// ⚠️ By array filters, never by rewriting the member list: members
    /// report their tips every few seconds, and a list read before the move
    /// would undo the reports that landed meanwhile, or miss the move.
    pub async fn move_lease(
        &self,
        id: ObjectId,
        p: &Promotion,
        old_role: &str,
        old_state: MemberState,
        deadline: DateTime,
    ) -> DaoResult<bool> {
        // Syncing holds the drained tip: none, and nothing moves.
        let Some(mark) = fence_mark(p, p.from_fence) else {
            return Ok(false);
        };
        let (to, from) = (p.to, p.from.device_id);
        let opts = mongodb::options::UpdateOptions::builder()
            .array_filters(vec![
                doc! { "to.device_id": to },
                doc! { "from.device_id": from },
                doc! {
                    "other.device_id": { "$nin": [to, from] },
                    "other.state": { "$in": [
                        MemberState::Joined.as_str(),
                        MemberState::Pending.as_str(),
                        Bson::Null,
                    ] },
                },
            ])
            .build();
        let r = self
            .base
            .collection()
            .update_one(
                doc! {
                    "_id": id,
                    "fence": p.from_fence,
                    "location.device_id": from,
                    "status": statuses(&SessionStatus::REPORTABLE),
                    "promotion.id": p.id,
                    "promotion.state": PromotionState::Syncing.as_str(),
                    "replicaset.members": { "$elemMatch": {
                        "device_id": to,
                        "role": { "$ne": MemberRole::Primary.as_str() },
                        "state": MemberState::Joined.as_str(),
                    } },
                },
                doc! {
                    "$set": {
                        "fence": p.from_fence + 1,
                        "location.device_id": to,
                        "location.device_name": &p.to_name,
                        "location.folder": &p.folder,
                        "status": SessionStatus::Starting.as_str(),
                        "replicaset.members.$[to].role": MemberRole::Primary.as_str(),
                        "replicaset.members.$[to].state": MemberState::Pending.as_str(),
                        "replicaset.members.$[from].role": old_role,
                        "replicaset.members.$[from].state": old_state.as_str(),
                        "replicaset.members.$[other].state": MemberState::Pending.as_str(),
                        "promotion.state": PromotionState::Moving.as_str(),
                        "promotion.deadline": deadline,
                        "updated_at": DateTime::now(),
                    },
                    // The fence it supersedes ends at the drained tip.
                    "$push": { "fences": mark },
                    "$unset": {
                        "accepted_at": "",
                        "location.account": "",
                        "refusal": "",
                        "detail": "",
                        "replicaset.members.$[to].refusal": "",
                        "replicaset.members.$[to].answered_at": "",
                        "replicaset.members.$[from].refusal": "",
                        "replicaset.members.$[from].answered_at": "",
                        "replicaset.members.$[other].answered_at": "",
                    },
                },
            )
            .with_options(opts)
            .await?;
        Ok(r.modified_count > 0)
    }

    /// FR-90 P2e-3 — the member refused the start after the lease moved: the
    /// lease moves back to where the session ran, at the fence AFTER the
    /// member's (`fence + 2`): a fence once handed out is never handed out
    /// again, so nothing the member may yet do at its own can count. Only
    /// while the promotion is `moving` and the member's start unanswered.
    /// The member is a member again, in its old role; the old primary is the
    /// primary again; both, and every other member that held its copy, are
    /// owed their join at the new fence. The promotion ends with it.
    pub async fn move_lease_back(&self, id: ObjectId, p: &Promotion) -> DaoResult<bool> {
        // The member's fence ends where it began: it ran nothing.
        let Some(mark) = fence_mark(p, p.from_fence + 1) else {
            return Ok(false);
        };
        let (to, from) = (p.to, p.from.device_id);
        let opts = mongodb::options::UpdateOptions::builder()
            .array_filters(vec![
                doc! { "to.device_id": to },
                doc! { "from.device_id": from },
                doc! {
                    "other.device_id": { "$nin": [to, from] },
                    "other.state": { "$in": [
                        MemberState::Joined.as_str(),
                        MemberState::Pending.as_str(),
                        Bson::Null,
                    ] },
                },
            ])
            .build();
        let r = self
            .base
            .collection()
            .update_one(
                doc! {
                    "_id": id,
                    "fence": p.from_fence + 1,
                    "location.device_id": to,
                    "status": SessionStatus::Starting.as_str(),
                    "accepted_at": Bson::Null,
                    "promotion.id": p.id,
                    "promotion.state": PromotionState::Moving.as_str(),
                },
                doc! {
                    "$set": {
                        "fence": p.from_fence + 2,
                        "location.device_id": from,
                        "location.device_name": &p.from.device_name,
                        "location.folder": &p.from.folder,
                        "replicaset.members.$[to].role": &p.to_role,
                        "replicaset.members.$[to].state": MemberState::Pending.as_str(),
                        "replicaset.members.$[from].role": MemberRole::Primary.as_str(),
                        "replicaset.members.$[from].state": MemberState::Pending.as_str(),
                        "replicaset.members.$[other].state": MemberState::Pending.as_str(),
                        "updated_at": DateTime::now(),
                    },
                    "$push": { "fences": mark },
                    "$unset": {
                        "promotion": "",
                        "location.account": "",
                        "replicaset.members.$[to].answered_at": "",
                        "replicaset.members.$[from].refusal": "",
                        "replicaset.members.$[from].answered_at": "",
                        "replicaset.members.$[other].answered_at": "",
                    },
                },
            )
            .with_options(opts)
            .await?;
        Ok(r.modified_count > 0)
    }

    /// FR-90 P1b — the sessions the server holds as running on `device_id`:
    /// live, and launched there on the device's own word — its answer to the
    /// start, or a run state only it reports ([`SessionStatus::LAUNCHED`]).
    /// A start it has said nothing about is not here — it may not be launched
    /// yet, and reconcile owns it.
    ///
    /// ⚠️ Not `accepted_at` alone. The state usually lands BEFORE the answer
    /// ([`Self::accept`]), so a socket that drops between the two leaves the
    /// session `idle` with no `accepted_at` — which reconcile never re-sends,
    /// being no longer `starting`, and which would then outlive its harness
    /// for ever. Field, 2026-10-08: the one record the first P1b run left.
    pub async fn running_on_device(
        &self,
        tenant_id: ObjectId,
        device_id: ObjectId,
    ) -> DaoResult<Vec<AgentSession>> {
        self.base
            .find_many(
                doc! {
                    "tenant_id": tenant_id,
                    "location.device_id": device_id,
                    "status": statuses(&SessionStatus::LIVE),
                    "$or": [
                        { "accepted_at": { "$type": "date" } },
                        { "status": statuses(&SessionStatus::LAUNCHED) },
                    ],
                },
                None,
            )
            .await
    }

    /// FR-90 P1b — end a session its device says it does not run: still
    /// live, still this device's, still at `fence`. A session being stopped
    /// ends `stopped` (its harness is gone either way); any other,
    /// `not_on_device`.
    pub async fn end_not_on_device(
        &self,
        id: ObjectId,
        device_id: ObjectId,
        fence: i64,
        stopping: bool,
    ) -> DaoResult<bool> {
        let reason = if stopping { "stopped" } else { "not_on_device" };
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "location.device_id": device_id,
                    "fence": fence,
                    "status": statuses(&SessionStatus::LIVE),
                },
                doc! { "$set": {
                    "status": SessionStatus::Ended.as_str(),
                    "end_reason": reason,
                    "detail": "the device reconnected without it",
                    "ended_at": DateTime::now(),
                } },
            )
            .await
    }

    /// What a device must be told when it connects: stops it has not
    /// confirmed, and starts it never answered.
    pub async fn needing_delivery(
        &self,
        tenant_id: ObjectId,
        device_id: ObjectId,
    ) -> DaoResult<Vec<AgentSession>> {
        self.base
            .find_many(
                doc! {
                    "tenant_id": tenant_id,
                    "location.device_id": device_id,
                    "$or": [
                        { "status": SessionStatus::Stopping.as_str() },
                        { "status": SessionStatus::Starting.as_str(), "accepted_at": Bson::Null },
                    ],
                },
                Some(doc! { "created_at": 1 }),
            )
            .await
    }

    /// FR-90 P2c-3 — a member's answer to its join: `joined`, or `refused`
    /// with the device's word. Only for a member the replicaset names as
    /// THIS device, and only at the session's fence: an answer to an older
    /// join changes nothing. The primary's counts only while it is owed its
    /// join (P2c-4, `pending` or `refused`): one the start told, `joined`
    /// from its placement, cannot be talked out of it.
    pub async fn member_answer(
        &self,
        id: ObjectId,
        device_id: ObjectId,
        fence: u64,
        refused: Option<&str>,
    ) -> DaoResult<bool> {
        let Some(fence) = stored_fence(fence) else {
            return Ok(false);
        };
        let now = DateTime::now();
        let update = match refused {
            None => doc! {
                "$set": {
                    "replicaset.members.$.state": MemberState::Joined.as_str(),
                    "replicaset.members.$.answered_at": now,
                },
                "$unset": { "replicaset.members.$.refusal": "" },
            },
            Some(word) => doc! {
                "$set": {
                    "replicaset.members.$.state": MemberState::Refused.as_str(),
                    "replicaset.members.$.refusal": word,
                    "replicaset.members.$.answered_at": now,
                },
            },
        };
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "fence": fence,
                    "replicaset.members": { "$elemMatch": {
                        "device_id": device_id,
                        // P2c-4 — an answer in flight when placement let the
                        // member go does not take it back.
                        "state": { "$ne": MemberState::Leaving.as_str() },
                        "$or": [
                            { "role": { "$ne": MemberRole::Primary.as_str() } },
                            { "state": { "$in": [
                                MemberState::Pending.as_str(),
                                MemberState::Refused.as_str(),
                            ] } },
                        ],
                    } },
                },
                update,
            )
            .await
    }

    /// FR-90 P2c-3 — where a member's copy of a session ends, as the member
    /// says: any member the replicaset names as THIS device, the primary
    /// included, but not one that refused its join — by its own word it holds
    /// nothing.
    pub async fn member_tip(
        &self,
        id: ObjectId,
        device_id: ObjectId,
        tip: &MemberTip,
    ) -> DaoResult<bool> {
        let tip = bson::to_bson(tip)?;
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "replicaset.members": { "$elemMatch": {
                        "device_id": device_id,
                        "state": { "$ne": MemberState::Refused.as_str() },
                    } },
                },
                doc! { "$set": { "replicaset.members.$.tip": tip } },
            )
            .await
    }

    /// FR-90 P2c-3 — the sessions whose joins this device has still to
    /// answer: it is a member other than the primary, still `pending`, of a
    /// session its primary launched and nobody is stopping. What a device
    /// that holds copies is sent when it connects. A member placed before
    /// P2c-3 has no `state`, and is pending.
    pub async fn pending_joins(
        &self,
        tenant_id: ObjectId,
        device_id: ObjectId,
    ) -> DaoResult<Vec<AgentSession>> {
        self.base
            .find_many(
                doc! {
                    "tenant_id": tenant_id,
                    "status": statuses(&SessionStatus::REPORTABLE),
                    "$or": [
                        { "accepted_at": { "$type": "date" } },
                        { "status": statuses(&SessionStatus::LAUNCHED) },
                    ],
                    "replicaset.members": { "$elemMatch": {
                        "device_id": device_id,
                        "role": { "$ne": MemberRole::Primary.as_str() },
                        "state": { "$in": [MemberState::Pending.as_str(), Bson::Null] },
                    } },
                },
                Some(doc! { "created_at": 1 }),
            )
            .await
    }

    /// FR-90 P2c-4 — the sessions this device runs whose primary's join it
    /// has still to answer `joined`: it is their primary, `pending` or
    /// `refused`, while they have a member besides it, launched and not
    /// being stopped. A primary that refused is asked again at every
    /// connection: its build may have learned the word since.
    pub async fn primary_joins_owed(
        &self,
        tenant_id: ObjectId,
        device_id: ObjectId,
    ) -> DaoResult<Vec<AgentSession>> {
        self.base
            .find_many(
                doc! {
                    "tenant_id": tenant_id,
                    "location.device_id": device_id,
                    "status": statuses(&SessionStatus::REPORTABLE),
                    "$or": [
                        { "accepted_at": { "$type": "date" } },
                        { "status": statuses(&SessionStatus::LAUNCHED) },
                    ],
                    "$and": [
                        { "replicaset.members": { "$elemMatch": {
                            "device_id": device_id,
                            "role": MemberRole::Primary.as_str(),
                            "state": { "$in": [
                                MemberState::Pending.as_str(),
                                MemberState::Refused.as_str(),
                            ] },
                        } } },
                        { "replicaset.members": { "$elemMatch": {
                            "role": { "$ne": MemberRole::Primary.as_str() },
                        } } },
                    ],
                },
                Some(doc! { "created_at": 1 }),
            )
            .await
    }

    /// FR-90 P2c-4 — the session gained its first member after its start
    /// told the primary it had none: the primary is owed its join now.
    /// `false` = it was not `joined` (already owed one, or none named).
    pub async fn owe_primary_join(&self, id: ObjectId) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "replicaset.members": { "$elemMatch": {
                        "role": MemberRole::Primary.as_str(),
                        "state": MemberState::Joined.as_str(),
                    } },
                },
                doc! {
                    "$set": { "replicaset.members.$.state": MemberState::Pending.as_str() },
                    "$unset": {
                        "replicaset.members.$.refusal": "",
                        "replicaset.members.$.answered_at": "",
                    },
                },
            )
            .await
    }

    /// FR-90 P2c-4 — the live sessions this device runs that were never
    /// placed: started before P2, or while `hive.replicaset` was off. Never
    /// an adopted one, which is placed nowhere until P2f.
    pub async fn unplaced_on(
        &self,
        tenant_id: ObjectId,
        device_id: ObjectId,
    ) -> DaoResult<Vec<AgentSession>> {
        self.base
            .find_many(
                doc! {
                    "tenant_id": tenant_id,
                    "location.device_id": device_id,
                    "status": statuses(&SessionStatus::REPORTABLE),
                    "replicaset": Bson::Null,
                    "origin": { "$ne": crate::model::SessionOrigin::Adopted.as_str() },
                },
                Some(doc! { "created_at": 1 }),
            )
            .await
    }

    /// FR-90 P2c-4 — the first placement of a session that had none, unless
    /// another connection placed it first. `false` = one did.
    pub async fn place_unplaced(&self, id: ObjectId, replicaset: &Replicaset) -> DaoResult<bool> {
        let r = bson::to_bson(replicaset)?;
        self.base
            .update_one(
                doc! { "_id": id, "replicaset": Bson::Null },
                doc! { "$set": { "replicaset": r } },
            )
            .await
    }

    /// FR-90 P2c-4 — the live sessions of `tenant_id` that were placed: what
    /// placement decides again when something it reads changes. Not one being
    /// stopped, and never an adopted one, which is placed nowhere until P2f.
    pub async fn live_placed(&self, tenant_id: ObjectId) -> DaoResult<Vec<AgentSession>> {
        self.base
            .find_many(
                doc! {
                    "tenant_id": tenant_id,
                    "status": statuses(&SessionStatus::REPORTABLE),
                    "replicaset": { "$type": "object" },
                    "origin": { "$ne": crate::model::SessionOrigin::Adopted.as_str() },
                },
                Some(doc! { "created_at": 1 }),
            )
            .await
    }

    /// FR-90 P2c-4 — a member placement now chooses, added as it is given,
    /// unless the replicaset already names the device. `false` = it did.
    pub async fn add_member(&self, id: ObjectId, member: &ReplicaMember) -> DaoResult<bool> {
        let m = bson::to_bson(member)?;
        self.base
            .update_one(
                doc! { "_id": id, "replicaset.members.device_id": { "$ne": member.device_id } },
                doc! { "$push": { "replicaset.members": m } },
            )
            .await
    }

    /// FR-90 P2c-4 — a member placement no longer chooses: `leaving`, never
    /// the primary. Its copy goes with the purge (P2g).
    pub async fn mark_leaving(&self, id: ObjectId, device_id: ObjectId) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "replicaset.members": { "$elemMatch": {
                        "device_id": device_id,
                        "role": { "$ne": MemberRole::Primary.as_str() },
                        "state": { "$ne": MemberState::Leaving.as_str() },
                    } },
                },
                doc! { "$set": { "replicaset.members.$.state": MemberState::Leaving.as_str() } },
            )
            .await
    }

    /// FR-90 P2c-4 — a leaving member placement chooses again, in `role`:
    /// `pending`, so its join goes again, and the device answers it as it
    /// answered the first (its store holds the membership already).
    pub async fn rejoin(&self, id: ObjectId, device_id: ObjectId, role: &str) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "replicaset.members": { "$elemMatch": {
                        "device_id": device_id,
                        "state": MemberState::Leaving.as_str(),
                    } },
                },
                doc! {
                    "$set": {
                        "replicaset.members.$.state": MemberState::Pending.as_str(),
                        "replicaset.members.$.role": role,
                    },
                    "$unset": {
                        "replicaset.members.$.refusal": "",
                        "replicaset.members.$.answered_at": "",
                    },
                },
            )
            .await
    }

    /// FR-90 P2c-4 — what a placement decided besides its members: the
    /// policy it read, its bounds, the restricted tags (only ever more), and
    /// why it is short, when it is.
    pub async fn set_placement(
        &self,
        id: ObjectId,
        rules: &crate::policy::ReplicasetRules,
        policy_revision: i64,
        restricted_tags: &[String],
        short: Option<&str>,
    ) -> DaoResult<bool> {
        let mut set = doc! {
            "replicaset.policy_revision": policy_revision,
            "replicaset.min": i64::from(rules.min),
            "replicaset.max": i64::from(rules.max),
            "replicaset.restricted_tags": bson::to_bson(restricted_tags)?,
            "replicaset.placed_at": DateTime::now(),
        };
        let update = match short {
            Some(s) => {
                set.insert("replicaset.short", s);
                doc! { "$set": set }
            }
            None => doc! { "$set": set, "$unset": { "replicaset.short": "" } },
        };
        self.base.update_one(doc! { "_id": id }, update).await
    }
}

/// FR-90 P1a-2 — `agent_approvals`. One record per (session, approval id),
/// held to that by a unique index, so a replayed frame makes no second
/// record and no second stub.
pub struct AgentApprovalDao {
    pub base: BaseDao<AgentApproval>,
}

impl AgentApprovalDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, AgentApproval::COLLECTION),
        }
    }

    /// Record an approval. `false` when it is recorded already — a replayed
    /// frame, or an end that arrived before its opening.
    pub async fn insert(&self, a: &AgentApproval) -> DaoResult<bool> {
        match self.base.insert_one(a).await {
            Ok(_) => Ok(true),
            Err(DaoError::DuplicateKey(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub async fn find(
        &self,
        session_id: ObjectId,
        approval_id: &str,
    ) -> DaoResult<Option<AgentApproval>> {
        self.base
            .find_one(doc! { "session_id": session_id, "approval_id": approval_id })
            .await
    }

    /// The stub the approval's room message is.
    pub async fn set_message(
        &self,
        session_id: ObjectId,
        approval_id: &str,
        message_id: ObjectId,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "session_id": session_id, "approval_id": approval_id },
                doc! { "$set": { "message_id": message_id } },
            )
            .await
    }

    /// End an OPEN approval: the record as it now stands, or `None` when it
    /// is not open — ended already (a replay), or never recorded here.
    pub async fn resolve(
        &self,
        session_id: ObjectId,
        approval_id: &str,
        status: HiveApprovalStatus,
        answered_by: Option<ObjectId>,
    ) -> DaoResult<Option<AgentApproval>> {
        let mut set = doc! { "status": status.as_str(), "resolved_at": DateTime::now() };
        if let Some(by) = answered_by {
            set.insert("answered_by", by);
        }
        Ok(self
            .base
            .collection()
            .find_one_and_update(
                doc! {
                    "session_id": session_id,
                    "approval_id": approval_id,
                    "status": HiveApprovalStatus::Open.as_str(),
                },
                doc! { "$set": set },
            )
            .return_document(mongodb::options::ReturnDocument::After)
            .await?)
    }

    /// The session is over: every approval still open in it is withdrawn,
    /// and returned so their stubs can say so.
    pub async fn withdraw_open(&self, session_id: ObjectId) -> DaoResult<Vec<AgentApproval>> {
        let open = self
            .base
            .find_many(
                doc! { "session_id": session_id, "status": HiveApprovalStatus::Open.as_str() },
                None,
            )
            .await?;
        let mut ended = Vec::new();
        for a in open {
            if let Some(a) = self
                .resolve(
                    session_id,
                    &a.approval_id,
                    HiveApprovalStatus::Withdrawn,
                    None,
                )
                .await?
            {
                ended.push(a);
            }
        }
        Ok(ended)
    }
}

pub struct HiveAuditDao {
    pub base: BaseDao<HiveAuditEvent>,
}

impl HiveAuditDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, HiveAuditEvent::COLLECTION),
        }
    }

    pub async fn record(&self, event: HiveAuditEvent) -> DaoResult<ObjectId> {
        self.base.insert_one(&event).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fence_beyond_the_stored_range_matches_nothing() {
        assert_eq!(stored_fence(1), Some(1));
        assert_eq!(stored_fence(i64::MAX as u64), Some(i64::MAX));
        assert_eq!(stored_fence(u64::MAX), None);
    }

    #[test]
    fn the_status_filter_spells_what_serde_stores() {
        let b = statuses(&[SessionStatus::Starting, SessionStatus::AwaitingApproval]);
        assert_eq!(
            b,
            Bson::Document(doc! { "$in": ["starting", "awaiting_approval"] })
        );
    }
}

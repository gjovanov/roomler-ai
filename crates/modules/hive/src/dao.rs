// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! `agent_sessions` and `hive_audit`.
//!
//! Every transition a DEVICE drives is a compare-and-set on three things the
//! frame cannot choose: the session's device (the authenticated socket's, not
//! a field in the frame), its fence, and the statuses it may leave. A stale
//! frame, a frame from another device, or one that crossed a stop on the wire
//! matches nothing and changes nothing — session ids are ObjectIds,
//! structured, not secret.

use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_remote_control::hive::HiveRunState;
use roomler_ai_services::dao::base::{BaseDao, DaoResult, PaginatedResult, PaginationParams};

use crate::model::{AgentSession, HiveAuditEvent, SessionStatus};

fn statuses(set: &[SessionStatus]) -> Bson {
    let words: Vec<Bson> = set.iter().map(|s| Bson::from(s.as_str())).collect();
    Bson::Document(doc! { "$in": words })
}

/// A wire fence as the database stores it. `None` = no stored session could
/// hold it (above `i64::MAX`), so the frame matches nothing.
fn stored_fence(fence: u64) -> Option<i64> {
    i64::try_from(fence).ok()
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

    /// The device answered the start: accepted. Only an UNANSWERED start
    /// moves (a duplicate answer — the device saw the start twice after a
    /// reconnect — changes nothing), and a stop already ordered stays
    /// ordered: the status is left as it is.
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
                    "status": statuses(&[SessionStatus::Starting, SessionStatus::Stopping]),
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

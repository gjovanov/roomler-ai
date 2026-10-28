// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-129 A1 — `device_pairings`: the row IS the pairing's whole state.
//!
//! `start`, `approve` and `poll` may each land on a different pod (the front
//! LB hashes on the tenant, and a pairing has no tenant until it is
//! approved), so nothing about a pairing lives in memory — no waiter, no
//! map. Every transition is a compare-and-set on the stored `status`, and
//! winning the CAS is the right to act: the poll that flips
//! `approved → delivered` is the one that mints, and a second poll gets
//! nothing. Every filter that must only match a live pairing also compares
//! `expires_at` to the clock: the TTL monitor runs about a minute late and
//! is never relied on for correctness.

use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_db::models::{DevicePairing, PairingStatus};

use super::base::{BaseDao, DaoResult};

pub struct DevicePairingDao {
    pub base: BaseDao<DevicePairing>,
}

impl DevicePairingDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, DevicePairing::COLLECTION),
        }
    }

    /// Insert a fresh pairing. A `DuplicateKey` — the user code is held by
    /// another live `pending` row, or (in theory) the hash collides — is
    /// returned as is, so the caller regenerates both codes and retries.
    pub async fn create(&self, row: &DevicePairing) -> DaoResult<ObjectId> {
        self.base.insert_one(row).await
    }

    /// The pairing a device code names, whatever its status. The caller
    /// decides what the status and the clock mean.
    pub async fn find_by_hash(&self, device_code_hash: &str) -> DaoResult<Option<DevicePairing>> {
        self.base
            .find_one(doc! { "device_code_hash": device_code_hash })
            .await
    }

    /// The one `pending` row a user code names (the partial unique index
    /// allows at most one); the caller still checks `expires_at`.
    pub async fn find_pending_by_user_code(
        &self,
        user_code: &str,
    ) -> DaoResult<Option<DevicePairing>> {
        self.base
            .find_one(doc! { "user_code": user_code, "status": PairingStatus::Pending.wire() })
            .await
    }

    /// CAS `pending → approved` on a row that is still live at `now`. True
    /// only if THIS call made the transition.
    pub async fn approve(
        &self,
        id: ObjectId,
        approver_user_id: ObjectId,
        tenant_id: ObjectId,
        now: DateTime,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "status": PairingStatus::Pending.wire(),
                    "expires_at": { "$gt": now },
                },
                doc! { "$set": {
                    "status": PairingStatus::Approved.wire(),
                    "approver_user_id": approver_user_id,
                    "tenant_id": tenant_id,
                    "decided_at": now,
                } },
            )
            .await
    }

    /// CAS `pending → denied` by the signed-in person `by`.
    pub async fn deny(&self, id: ObjectId, by: ObjectId, now: DateTime) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": id, "status": PairingStatus::Pending.wire() },
                doc! { "$set": {
                    "status": PairingStatus::Denied.wire(),
                    "approver_user_id": by,
                    "decided_at": now,
                } },
            )
            .await
    }

    /// CAS `approved → denied`: the approver's gate was gone when the poll
    /// came to mint (an admin demoted between approve and delivery). The
    /// approver and the org stay on the row — they are the audit's subject.
    pub async fn deny_at_delivery(&self, id: ObjectId, now: DateTime) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": id, "status": PairingStatus::Approved.wire() },
                doc! { "$set": {
                    "status": PairingStatus::Denied.wire(),
                    "decided_at": now,
                } },
            )
            .await
    }

    /// CAS `approved → delivered` on a row still live at `now`. Winning it
    /// is the right to mint: the caller mints ONLY on `true`.
    pub async fn deliver(&self, id: ObjectId, now: DateTime) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "status": PairingStatus::Approved.wire(),
                    "expires_at": { "$gt": now },
                },
                doc! { "$set": {
                    "status": PairingStatus::Delivered.wire(),
                    "delivered_at": now,
                } },
            )
            .await
    }

    /// CAS `pending | approved → expired` for a row a reader found past its
    /// `expires_at` before the sweep took it. True once, so the expiry is
    /// audited exactly once.
    pub async fn expire(&self, id: ObjectId) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! {
                    "_id": id,
                    "status": { "$in": [PairingStatus::Pending.wire(), PairingStatus::Approved.wire()] },
                },
                doc! { "$set": { "status": PairingStatus::Expired.wire() } },
            )
            .await
    }

    /// Record a poll's arrival. The interval is not touched: only a
    /// `slow_down` moves it, and only upward ([`Self::slow_down`]).
    pub async fn note_poll(&self, id: ObjectId, now: DateTime) -> DaoResult<bool> {
        self.base
            .update_one(doc! { "_id": id }, doc! { "$set": { "last_poll_at": now } })
            .await
    }

    /// Record a poll that came too soon and the longer interval the
    /// installer was told. `$max`, never `$set`: two polls in flight both
    /// read the old interval and each computes old + 5, and a `$set` from
    /// the slower writer would race the interval back DOWN past a value
    /// another poll had already answered — the interval only ever grows
    /// within a pairing's life.
    pub async fn slow_down(
        &self,
        id: ObjectId,
        now: DateTime,
        poll_interval_secs: u32,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": id },
                slow_down_update(now, poll_interval_secs),
            )
            .await
    }
}

/// The update a [`DevicePairingDao::slow_down`] applies — pure, so the
/// `$max` is a unit test rather than a race.
pub fn slow_down_update(now: DateTime, poll_interval_secs: u32) -> bson::Document {
    doc! {
        "$set": { "last_poll_at": now },
        "$max": { "poll_interval_secs": poll_interval_secs },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F3 — the interval is raised with `$max`, so a concurrent writer that
    /// read an older value cannot lower what another poll already answered.
    #[test]
    fn a_slow_down_only_ever_raises_the_interval() {
        let now = DateTime::now();
        let u = slow_down_update(now, 15);
        let max = u.get_document("$max").expect("$max");
        // `doc!` stores a `u32` as the narrowest integer that holds it.
        let raised = max
            .get_i32("poll_interval_secs")
            .map(i64::from)
            .or_else(|_| max.get_i64("poll_interval_secs"))
            .ok();
        assert_eq!(raised, Some(15));
        let set = u.get_document("$set").expect("$set");
        assert!(set.contains_key("last_poll_at"));
        assert!(
            !set.contains_key("poll_interval_secs"),
            "the interval is never $set"
        );
    }
}

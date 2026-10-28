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

    /// Record a poll's arrival and the interval the installer now holds.
    pub async fn note_poll(
        &self,
        id: ObjectId,
        now: DateTime,
        poll_interval_secs: u32,
    ) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "_id": id },
                doc! { "$set": {
                    "last_poll_at": now,
                    "poll_interval_secs": poll_interval_secs,
                } },
            )
            .await
    }
}

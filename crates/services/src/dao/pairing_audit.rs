// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-129 A1 device-pairing decision log (`pairing_audit`).
//!
//! Every pairing route lands here, both arms of each: a `start` and the
//! address ceiling that refused one, a `lookup` and the miss it was, an
//! `approve` and the gate that refused it, a `deny`, the `deliver` that
//! minted and the poll that found the approver's gate gone, an `expire`.
//! The refused rows are the point — an account guessing codes, an address
//! flooding starts, a demoted admin's pairing — the same reason
//! `peer_relay_audit` and `exec_audit` exist.
//!
//! ⚠️ Never the device code, never a token. A pairing is named by its row
//! id, a person by theirs.
//!
//! Writes are best-effort and must never gate the request; the caller logs
//! a failed insert and proceeds. Rows TTL out after 90 days (the fleet
//! module's index plan), matching the other decision logs.

use bson::oid::ObjectId;
use mongodb::Database;
use roomler_ai_db::models::PairingAuditEvent;

use super::base::{BaseDao, DaoResult};

pub struct PairingAuditDao {
    pub base: BaseDao<PairingAuditEvent>,
}

impl PairingAuditDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, PairingAuditEvent::COLLECTION),
        }
    }

    /// Record one decision, granted or refused. The event carries its own
    /// `outcome`, so both arms come through here and a new refusal cannot
    /// forget to audit itself.
    pub async fn record(&self, event: PairingAuditEvent) -> DaoResult<ObjectId> {
        self.base.insert_one(&event).await
    }
}

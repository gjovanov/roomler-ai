// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-155 token-rotation audit log (`token_rotation_audit`).
//!
//! One row per decision of `POST /api/agent/self/rotate`
//! ([`TokenRotationAuditEvent`]), each with the caller's address and the
//! generations. The route writes every decision through one helper, so a new
//! refusal cannot forget to audit itself, as with `config_audit` and
//! `key_rotation_audit`.
//!
//! Writes are best-effort and must never gate the request. Rows TTL out after
//! 90 days (the fleet module's index plan), matching the other audit logs.

use bson::oid::ObjectId;
use mongodb::Database;
use roomler_ai_remote_control::models::TokenRotationAuditEvent;

use super::base::{BaseDao, DaoResult};

pub struct TokenRotationAuditDao {
    pub base: BaseDao<TokenRotationAuditEvent>,
}

impl TokenRotationAuditDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, TokenRotationAuditEvent::COLLECTION),
        }
    }

    /// Record one decision.
    pub async fn record(&self, event: &TokenRotationAuditEvent) -> DaoResult<ObjectId> {
        self.base.insert_one(event).await
    }
}

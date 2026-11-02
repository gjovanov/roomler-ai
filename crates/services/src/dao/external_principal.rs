// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-52 P5 — who has come in from outside (`external_principals`): one row per
//! person per device, kept for as long as the org is. `external_rc_audit` is the
//! log of decisions and TTLs out at 90 days; this is what stays.
//!
//! Two jobs. "The first session this person ever opened on this device" has to
//! survive the audit's TTL, and a revocation needs somewhere to sit.

use bson::{Bson, DateTime, doc, oid::ObjectId};
use mongodb::Database;
use mongodb::options::ReturnDocument;
use roomler_ai_remote_control::models::ExternalPrincipal;

use super::base::{BaseDao, DaoError, DaoResult, PaginatedResult, PaginationParams};

pub struct ExternalPrincipalDao {
    pub base: BaseDao<ExternalPrincipal>,
}

impl ExternalPrincipalDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, ExternalPrincipal::COLLECTION),
        }
    }

    /// Record a session this person opened on this device, and answer with the
    /// row as it stood BEFORE: `None` means this is their first ever.
    ///
    /// ONE write, so the unique `{agent_id, user_id}` index arbitrates two firsts
    /// on two pods: exactly one of them is told `None`. The row it returns is also
    /// a read of `revoked_at` that cannot miss a revoke made just before the
    /// session existed (`extauth::session_opened`).
    pub async fn note_session(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        user_id: ObjectId,
    ) -> DaoResult<Option<ExternalPrincipal>> {
        let now = DateTime::now();
        let filter = doc! { "agent_id": agent_id, "user_id": user_id };
        let update = doc! {
            "$setOnInsert": { "tenant_id": tenant_id, "first_session_at": now },
            "$set": { "last_session_at": now },
        };
        // The server retries an upsert that lost the race on this index, because
        // the filter is exactly its key. The second try here is for a server that
        // did not: it finds the winner's row, and the answer is the same.
        let mut lost_a_race = false;
        loop {
            let found = self
                .base
                .collection()
                .find_one_and_update(filter.clone(), update.clone())
                .upsert(true)
                .return_document(ReturnDocument::Before)
                .await;
            match found {
                Ok(before) => return Ok(before),
                Err(e) if !lost_a_race && is_duplicate_key(&e) => lost_a_race = true,
                Err(e) => return Err(DaoError::Mongo(e)),
            }
        }
    }

    /// Is this person barred from this device? `Err` is NOT "no": the caller's
    /// gate decides what an unreadable answer means, and it refuses.
    pub async fn is_revoked(&self, agent_id: ObjectId, user_id: ObjectId) -> DaoResult<bool> {
        let barred = self
            .base
            .count(doc! {
                "agent_id": agent_id,
                "user_id": user_id,
                "revoked_at": { "$type": "date" },
            })
            .await?;
        Ok(barred > 0)
    }

    /// Bar this person from this device, or let them back. Resolved inside the
    /// tenant, so another org's row is no row: `None`. Otherwise whether this call
    /// changed anything: a second revoke keeps the first one's stamp.
    pub async fn set_revoked(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        user_id: ObjectId,
        by: ObjectId,
        revoked: bool,
    ) -> DaoResult<Option<bool>> {
        let filter = doc! { "tenant_id": tenant_id, "agent_id": agent_id, "user_id": user_id };
        let Some(row) = self.base.find_one(filter.clone()).await? else {
            return Ok(None);
        };
        if row.revoked_at.is_some() == revoked {
            return Ok(Some(false));
        }
        let set = if revoked {
            doc! { "revoked_at": DateTime::now(), "revoked_by": by }
        } else {
            doc! { "revoked_at": Bson::Null, "revoked_by": Bson::Null }
        };
        self.base.update_one(filter, doc! { "$set": set }).await?;
        Ok(Some(true))
    }

    /// The org's people, the most recent first.
    pub async fn list_for_tenant(
        &self,
        tenant_id: ObjectId,
        params: &PaginationParams,
    ) -> DaoResult<PaginatedResult<ExternalPrincipal>> {
        self.base
            .find_paginated(
                doc! { "tenant_id": tenant_id },
                Some(doc! { "last_session_at": -1 }),
                params,
            )
            .await
    }
}

fn is_duplicate_key(e: &mongodb::error::Error) -> bool {
    use mongodb::error::{ErrorKind, WriteFailure};
    match e.kind.as_ref() {
        ErrorKind::Command(c) => c.code == 11000,
        ErrorKind::Write(WriteFailure::WriteError(w)) => w.code == 11000,
        _ => false,
    }
}

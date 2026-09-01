// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_db::models::{IssueCounts, NewsletterSend, SendStatus};

use super::base::{BaseDao, DaoError, DaoResult};

/// FR-58 — the per-recipient delivery ledger. See the model docs: the unique
/// `{issue_id, subscriber_id}` index is the at-most-once invariant; everything
/// here leans on it.
pub struct NewsletterSendDao {
    pub base: BaseDao<NewsletterSend>,
}

impl NewsletterSendDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, NewsletterSend::COLLECTION),
        }
    }

    /// Claim-first: insert the row BEFORE the send attempt. `Ok(None)` means
    /// another pass (or pod) already holds this recipient — skip, that is the
    /// idempotence working, not an error.
    pub async fn try_claim(
        &self,
        issue_id: ObjectId,
        subscriber_id: ObjectId,
        email: &str,
    ) -> DaoResult<Option<ObjectId>> {
        let row = NewsletterSend {
            id: None,
            issue_id,
            subscriber_id,
            email: email.to_string(),
            status: SendStatus::Claimed,
            error: None,
            claimed_at: DateTime::now(),
            updated_at: DateTime::now(),
            sent_at: None,
        };
        match self.base.insert_one(&row).await {
            Ok(id) => Ok(Some(id)),
            Err(DaoError::DuplicateKey(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn mark_sent(&self, id: ObjectId) -> DaoResult<bool> {
        self.base
            .update_by_id(
                id,
                doc! { "$set": { "status": "sent", "sent_at": DateTime::now() } },
            )
            .await
    }

    pub async fn mark_failed(&self, id: ObjectId, error: &str) -> DaoResult<bool> {
        self.base
            .update_by_id(id, doc! { "$set": { "status": "failed", "error": error } })
            .await
    }

    pub async fn mark_suppressed(&self, id: ObjectId) -> DaoResult<bool> {
        self.base
            .update_by_id(id, doc! { "$set": { "status": "suppressed" } })
            .await
    }

    /// Rows stuck `claimed` from before `cutoff` — the crash-window residue.
    pub async fn stale_rows(
        &self,
        issue_id: ObjectId,
        cutoff: DateTime,
    ) -> DaoResult<Vec<NewsletterSend>> {
        self.base
            .find_many(
                doc! { "issue_id": issue_id, "status": "claimed", "updated_at": { "$lt": cutoff } },
                Some(doc! { "claimed_at": 1 }),
            )
            .await
    }

    /// Per-row CAS so two resuming pods can't both re-attempt one recipient:
    /// only the caller that flips `updated_at` forward owns the retry.
    pub async fn reclaim(&self, id: ObjectId, cutoff: DateTime) -> DaoResult<bool> {
        let res = self
            .base
            .collection()
            .find_one_and_update(
                doc! { "_id": id, "status": "claimed", "updated_at": { "$lt": cutoff } },
                doc! { "$set": { "claimed_at": DateTime::now(), "updated_at": DateTime::now() } },
            )
            .await?;
        Ok(res.is_some())
    }

    /// A sample of failed rows for the status surface (capped by the caller).
    pub async fn failed_sample(
        &self,
        issue_id: ObjectId,
        limit: usize,
    ) -> DaoResult<Vec<NewsletterSend>> {
        let mut rows = self
            .base
            .find_many(
                doc! { "issue_id": issue_id, "status": "failed" },
                Some(doc! { "updated_at": 1 }),
            )
            .await?;
        rows.truncate(limit);
        Ok(rows)
    }

    /// The honest totals. `stale` is computed against `cutoff`, not merely
    /// "still claimed" — a row claimed two seconds ago is in flight, not stuck.
    pub async fn counts(&self, issue_id: ObjectId, cutoff: DateTime) -> DaoResult<IssueCounts> {
        let col = self.base.collection();
        Ok(IssueCounts {
            total: col.count_documents(doc! { "issue_id": issue_id }).await? as i64,
            sent: col
                .count_documents(doc! { "issue_id": issue_id, "status": "sent" })
                .await? as i64,
            failed: col
                .count_documents(doc! { "issue_id": issue_id, "status": "failed" })
                .await? as i64,
            suppressed: col
                .count_documents(doc! { "issue_id": issue_id, "status": "suppressed" })
                .await? as i64,
            stale: col
                .count_documents(
                    doc! { "issue_id": issue_id, "status": "claimed", "updated_at": { "$lt": cutoff } },
                )
                .await? as i64,
        })
    }
}

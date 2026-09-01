// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, doc};
use mongodb::Database;
use roomler_ai_db::models::NewsletterIssue;

use super::base::{BaseDao, DaoResult};

/// FR-58 — the newsletter issue store.
///
/// Create is explicit and update is filtered to drafts. Deliberately NOT an
/// upsert-by-slug: a typo'd slug on update must 404, never quietly mint a
/// second issue. Two concurrent creates on one slug are arbitrated by the
/// unique index (`DuplicateKey` → 409 at the route).
pub struct NewsletterIssueDao {
    pub base: BaseDao<NewsletterIssue>,
}

impl NewsletterIssueDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, NewsletterIssue::COLLECTION),
        }
    }

    pub async fn create(&self, issue: &NewsletterIssue) -> DaoResult<bson::oid::ObjectId> {
        self.base.insert_one(issue).await
    }

    pub async fn get_by_slug(&self, slug: &str) -> DaoResult<Option<NewsletterIssue>> {
        self.base.find_one(doc! { "slug": slug }).await
    }

    pub async fn list(&self) -> DaoResult<Vec<NewsletterIssue>> {
        self.base
            .find_many(doc! {}, Some(doc! { "created_at": -1 }))
            .await
    }

    /// Update the editable fields of a DRAFT. `false` = no draft row matched —
    /// the caller distinguishes "absent" (404) from "not a draft any more"
    /// (409) by reading the row, because the two have different fixes.
    pub async fn update_draft(&self, slug: &str, set: bson::Document) -> DaoResult<bool> {
        let mut set = set;
        set.insert("updated_at", DateTime::now());
        self.base
            .update_one(
                doc! { "slug": slug, "status": "draft" },
                doc! { "$set": set },
            )
            .await
    }
}

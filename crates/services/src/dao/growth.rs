// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 — the growth collections that are core-level (spec §3o): written
//! by the host's growth jobs and routes, read by every growth number.
//!
//! * [`GROWTH_EXCLUSIONS`] — the platform admin's explicit exclusion list
//!   (P1c): `{kind: tenant|user, id, reason, by, at}`, unique on
//!   `(kind, id)`. The implicit exclusions (every platform admin and the
//!   orgs they own) need no row; `roomler_core::growth::ExclusionSet` folds
//!   both into one set.
//!
//! Stateless over `core.db`, so a caller re-creates it (`GrowthDao::new`)
//! rather than threading a handle through `Core` (FR-69 rule 5).

use bson::{DateTime, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use mongodb::{Collection, Database};
use serde::{Deserialize, Serialize};

use super::base::DaoResult;

pub const GROWTH_EXCLUSIONS: &str = "growth_exclusions";

/// What an exclusion row names. The stored spelling is the snake_case
/// variant; a wire value outside the two is a 400 at the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionKind {
    Tenant,
    User,
}

impl ExclusionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ExclusionKind::Tenant => "tenant",
            ExclusionKind::User => "user",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "tenant" => Some(ExclusionKind::Tenant),
            "user" => Some(ExclusionKind::User),
            _ => None,
        }
    }
}

/// One explicit exclusion, as stored. `by` is the platform admin who wrote
/// the list (an id, never an address); `at` when.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrowthExclusion {
    pub kind: ExclusionKind,
    pub id: ObjectId,
    pub reason: String,
    pub by: ObjectId,
    pub at: DateTime,
}

pub struct GrowthDao {
    db: Database,
}

impl GrowthDao {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }

    fn exclusions(&self) -> Collection<GrowthExclusion> {
        self.db.collection::<GrowthExclusion>(GROWTH_EXCLUSIONS)
    }

    /// Every explicit row, tenants first, then by id — a stable order for
    /// the admin view and for a test that compares lists.
    pub async fn list_exclusions(&self) -> DaoResult<Vec<GrowthExclusion>> {
        let mut rows: Vec<GrowthExclusion> =
            self.exclusions().find(doc! {}).await?.try_collect().await?;
        rows.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.id.cmp(&b.id)));
        Ok(rows)
    }

    /// Replace the explicit list wholesale. Not transactional — two admins
    /// saving at once end with one of their lists, which is what "replace"
    /// means — and the unique `(kind, id)` index refuses a duplicate the
    /// caller failed to fold.
    pub async fn replace_exclusions(&self, rows: &[GrowthExclusion]) -> DaoResult<()> {
        self.db
            .collection::<Document>(GROWTH_EXCLUSIONS)
            .delete_many(doc! {})
            .await?;
        if !rows.is_empty() {
            self.exclusions().insert_many(rows).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stored spelling is a persisted key and a wire value: renaming a
    /// variant would make every existing row unreadable.
    #[test]
    fn kind_spellings_are_locked() {
        assert_eq!(ExclusionKind::Tenant.as_str(), "tenant");
        assert_eq!(ExclusionKind::User.as_str(), "user");
        assert_eq!(ExclusionKind::parse("tenant"), Some(ExclusionKind::Tenant));
        assert_eq!(ExclusionKind::parse("user"), Some(ExclusionKind::User));
        assert_eq!(ExclusionKind::parse("org"), None);
        assert_eq!(ExclusionKind::parse("Tenant"), None);
        assert_eq!(
            serde_json::to_string(&ExclusionKind::Tenant).unwrap(),
            "\"tenant\""
        );
    }

    /// The row round-trips through BSON as a struct — the insert path
    /// serialises it, so a field added later travels without a hand-built
    /// `doc!{}` to forget it in.
    #[test]
    fn a_row_round_trips_through_bson() {
        let row = GrowthExclusion {
            kind: ExclusionKind::User,
            id: ObjectId::new(),
            reason: "test account".into(),
            by: ObjectId::new(),
            at: DateTime::now(),
        };
        let doc = bson::to_document(&row).unwrap();
        assert_eq!(doc.get_str("kind").unwrap(), "user");
        let back: GrowthExclusion = bson::from_document(doc).unwrap();
        assert_eq!(back.kind, row.kind);
        assert_eq!(back.id, row.id);
        assert_eq!(back.reason, row.reason);
    }
}

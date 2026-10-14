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
//! * [`GROWTH_DAILY`] — the frozen daily totals (P3 onward): one row per
//!   `(source, metric, dims, day)`, `_id` the four joined, `value` the
//!   count. Written by whichever module owns the fact
//!   ([`GrowthDao::bump_daily`] — fleet's fetch counters today), read by the
//!   digest. A row carries dimensions and a number, never a person, a
//!   User-Agent or an address.
//!
//! Stateless over `core.db`, so a caller re-creates it (`GrowthDao::new`)
//! rather than threading a handle through `Core` (FR-69 rule 5).

use std::collections::BTreeMap;

use bson::{DateTime, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use mongodb::{Collection, Database};
use serde::{Deserialize, Serialize};

use super::base::DaoResult;

pub const GROWTH_EXCLUSIONS: &str = "growth_exclusions";
pub const GROWTH_DAILY: &str = "growth_daily";

/// P3 — the install trace's `(source, metric)`: written by the `fleet`
/// module's fetch counters, read by the host's digest. Named here, in the
/// crate both can see, because `fleet` is an optional dependency of the host
/// and the digest must read the rows whether or not that module is compiled.
pub const INSTALL_SOURCE: &str = "install";
pub const INSTALL_FETCH_METRIC: &str = "fetch";

/// One `growth_daily` row as stored. `dims` is a flat map of short
/// strings; `day` is the UTC day `YYYY-MM-DD`, a string so a range read is a
/// plain string comparison.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrowthDaily {
    #[serde(rename = "_id")]
    pub id: String,
    pub day: String,
    pub source: String,
    pub metric: String,
    #[serde(default)]
    pub dims: BTreeMap<String, String>,
    #[serde(default)]
    pub value: i64,
}

/// The `_id` of a daily row: `<source>:<metric>:<k=v,k=v>:<day>`, the
/// dimensions in key order so two writers of the same fact share one row.
/// A dimension VALUE is a short enum spelling by construction (a client
/// class, an asset name, an OS), never free text.
pub fn daily_id(source: &str, metric: &str, dims: &BTreeMap<String, String>, day: &str) -> String {
    let dims: Vec<String> = dims.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("{source}:{metric}:{}:{day}", dims.join(","))
}

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

    /// Add `by` to the `(source, metric, dims, day)` counter, creating the
    /// row on first sight. One upsert, atomic per row, so N pods counting
    /// the same fact never lose an increment to each other.
    pub async fn bump_daily(
        &self,
        day: &str,
        source: &str,
        metric: &str,
        dims: &BTreeMap<String, String>,
        by: i64,
    ) -> DaoResult<()> {
        let id = daily_id(source, metric, dims, day);
        let dims_doc: Document = dims
            .iter()
            .map(|(k, v)| (k.clone(), bson::Bson::String(v.clone())))
            .collect();
        self.db
            .collection::<Document>(GROWTH_DAILY)
            .update_one(
                doc! { "_id": id },
                doc! {
                    "$inc": { "value": by },
                    "$setOnInsert": {
                        "day": day,
                        "source": source,
                        "metric": metric,
                        "dims": dims_doc,
                    },
                },
            )
            .upsert(true)
            .await?;
        Ok(())
    }

    /// Every row of one `(source, metric)` with `from <= day <= to`
    /// (inclusive, `YYYY-MM-DD` strings).
    ///
    /// Lenient per row: a row that does not have this shape (a hand-written
    /// one, an older writer's) is skipped with a warning and costs its own
    /// count, never the whole digest — one bad row must not take the daily
    /// mail down.
    pub async fn read_daily(
        &self,
        source: &str,
        metric: &str,
        from: &str,
        to: &str,
    ) -> DaoResult<Vec<GrowthDaily>> {
        let docs: Vec<Document> = self
            .db
            .collection::<Document>(GROWTH_DAILY)
            .find(doc! {
                "source": source,
                "metric": metric,
                "day": { "$gte": from, "$lte": to },
            })
            .await?
            .try_collect()
            .await?;
        let mut rows = Vec::with_capacity(docs.len());
        for d in docs {
            match bson::from_document::<GrowthDaily>(d) {
                Ok(row) => rows.push(row),
                Err(e) => tracing::warn!(
                    source, metric, error = %e,
                    "growth_daily: skipping a row that does not have the daily shape"
                ),
            }
        }
        Ok(rows)
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

    /// The daily row's `_id` is the fact itself, dimensions in key order:
    /// two writers that disagree only on insertion order share one row.
    #[test]
    fn the_daily_id_is_canonical_over_dimension_order() {
        let mut a = BTreeMap::new();
        a.insert("os".to_string(), "windows".to_string());
        a.insert("asset".to_string(), "install.ps1".to_string());
        a.insert("client".to_string(), "script-ps1".to_string());
        let mut b = BTreeMap::new();
        b.insert("client".to_string(), "script-ps1".to_string());
        b.insert("asset".to_string(), "install.ps1".to_string());
        b.insert("os".to_string(), "windows".to_string());
        assert_eq!(
            daily_id("install", "fetch", &a, "2026-10-14"),
            "install:fetch:asset=install.ps1,client=script-ps1,os=windows:2026-10-14"
        );
        assert_eq!(
            daily_id("install", "fetch", &a, "2026-10-14"),
            daily_id("install", "fetch", &b, "2026-10-14")
        );
        assert_eq!(
            daily_id("x", "y", &BTreeMap::new(), "2026-01-01"),
            "x:y::2026-01-01"
        );
    }
}

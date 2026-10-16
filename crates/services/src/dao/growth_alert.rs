// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P10 — `growth_alerts`: one row per time a rule FIRED (spec §3o).
//!
//! An alert is `(rule, key)`: the rule's name and what it is about (a
//! source, a metric, `daily`). It opens once (`fired_at`), stays open while
//! the host's evaluator keeps finding the condition, and closes
//! (`cleared_at`) when the condition goes away; a later recurrence is a NEW
//! row, so the history of what fired and when is the collection itself.
//!
//! * **At most one open row per `(rule, key)`** — a partial unique index on
//!   `{rule, key}` where `cleared_at` is an explicit `null`. The index is
//!   the arbiter between two pods evaluating at once: the second insert is
//!   a duplicate key, which [`GrowthAlertDao::fire`] reads back as "already
//!   open", never as a second alert. ⚠️ `cleared_at` is therefore stored as
//!   `null` while open, never left absent — `$type: "null"` matches only
//!   the explicit null (CLAUDE.md, the tombstone rule).
//! * **Mailed at most once per rule+key per day**: `mailed_on` holds the
//!   UTC day of the last mail, and [`GrowthAlertDao::mark_mailed`] is a
//!   compare-and-set on it, so two pods that both see a fresh alert send
//!   one mail between them.
//! * `detail` is numbers, days and names of sources — never a person (an
//!   alert is about the product's numbers, and the digest's rule holds).
//! * Rows expire 90 days after `fired_at` (the TTL in core's index plan).

use bson::{DateTime, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use mongodb::{Collection, Database};
use serde::{Deserialize, Serialize};

use super::base::DaoResult;

pub const GROWTH_ALERTS: &str = "growth_alerts";
/// The most rows one listing answers.
pub const MAX_LISTED: i64 = 200;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrowthAlert {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    /// The rule's spelling (`digest_missing`, `source_not_measured`,
    /// `spike`, `drop`).
    pub rule: String,
    /// What the rule is about: a source, a metric, `daily`.
    pub key: String,
    pub fired_at: DateTime,
    /// `None` (stored as an explicit `null`) while open.
    pub cleared_at: Option<DateTime>,
    /// The numbers behind the decision, as the evaluator computed them.
    #[serde(default)]
    pub detail: Document,
    /// The UTC day (`YYYY-MM-DD`) this row was last mailed on.
    #[serde(default)]
    pub mailed_on: Option<String>,
}

impl GrowthAlert {
    pub fn is_open(&self) -> bool {
        self.cleared_at.is_none()
    }
}

/// What [`GrowthAlertDao::fire`] found.
#[derive(Debug, Clone, PartialEq)]
pub struct Fired {
    pub alert: GrowthAlert,
    /// This call opened the row (false: it was already open, on this pod or
    /// another).
    pub created: bool,
}

pub struct GrowthAlertDao {
    db: Database,
}

impl GrowthAlertDao {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }

    fn coll(&self) -> Collection<GrowthAlert> {
        self.db.collection::<GrowthAlert>(GROWTH_ALERTS)
    }

    fn docs(&self) -> Collection<Document> {
        self.db.collection::<Document>(GROWTH_ALERTS)
    }

    /// The open row of `(rule, key)`, if any.
    pub async fn open(&self, rule: &str, key: &str) -> DaoResult<Option<GrowthAlert>> {
        Ok(self
            .coll()
            .find_one(doc! { "rule": rule, "key": key, "cleared_at": bson::Bson::Null })
            .await?)
    }

    /// Open `(rule, key)` with `detail`, or find it already open. The
    /// partial unique index decides: a lost insert is read back, never
    /// retried into a second row.
    pub async fn fire(
        &self,
        rule: &str,
        key: &str,
        detail: Document,
        now: DateTime,
    ) -> DaoResult<Fired> {
        let fresh = GrowthAlert {
            id: ObjectId::new(),
            rule: rule.to_string(),
            key: key.to_string(),
            fired_at: now,
            cleared_at: None,
            detail,
            mailed_on: None,
        };
        match self.coll().insert_one(&fresh).await {
            Ok(_) => Ok(Fired {
                alert: fresh,
                created: true,
            }),
            Err(e) if super::stats::is_dup_key(&e) => match self.open(rule, key).await? {
                Some(alert) => Ok(Fired {
                    alert,
                    created: false,
                }),
                // Cleared between the insert and the read: the next
                // evaluation opens it again; this one reports it as found.
                None => Ok(Fired {
                    alert: fresh,
                    created: false,
                }),
            },
            Err(e) => Err(e.into()),
        }
    }

    /// Close every open row of `(rule, key)`; whether any was open.
    pub async fn clear(&self, rule: &str, key: &str, now: DateTime) -> DaoResult<bool> {
        let r = self
            .docs()
            .update_many(
                doc! { "rule": rule, "key": key, "cleared_at": bson::Bson::Null },
                doc! { "$set": { "cleared_at": now } },
            )
            .await?;
        Ok(r.modified_count > 0)
    }

    /// Every open row, newest first.
    pub async fn list_open(&self) -> DaoResult<Vec<GrowthAlert>> {
        Ok(self
            .coll()
            .find(doc! { "cleared_at": bson::Bson::Null })
            .sort(doc! { "fired_at": -1 })
            .limit(MAX_LISTED)
            .await?
            .try_collect()
            .await?)
    }

    /// Every row open now, fired since `from`, or cleared since `from` —
    /// newest first, at most [`MAX_LISTED`].
    pub async fn list_since(&self, from: DateTime) -> DaoResult<Vec<GrowthAlert>> {
        Ok(self
            .coll()
            .find(doc! { "$or": [
                { "cleared_at": bson::Bson::Null },
                { "fired_at": { "$gte": from } },
                { "cleared_at": { "$gte": from } },
            ] })
            .sort(doc! { "fired_at": -1 })
            .limit(MAX_LISTED)
            .await?
            .try_collect()
            .await?)
    }

    /// The rows fired or cleared inside `[from, to)` plus the ones open NOW
    /// — what a digest of that window reports: the window's events, and
    /// what is still open at the compute (a rule that fired this morning
    /// about yesterday belongs in yesterday's digest, computed this
    /// morning).
    pub async fn list_window(&self, from: DateTime, to: DateTime) -> DaoResult<Vec<GrowthAlert>> {
        Ok(self
            .coll()
            .find(doc! { "$or": [
                { "fired_at": { "$gte": from, "$lt": to } },
                { "cleared_at": { "$gte": from, "$lt": to } },
                { "cleared_at": bson::Bson::Null },
            ] })
            .sort(doc! { "fired_at": -1 })
            .limit(MAX_LISTED)
            .await?
            .try_collect()
            .await?)
    }

    /// Whether any row of `(rule, key)` — open or not — was mailed on
    /// `day` (`YYYY-MM-DD`): the once-per-day floor across a clear and a
    /// re-fire.
    pub async fn mailed_on(&self, rule: &str, key: &str, day: &str) -> DaoResult<bool> {
        Ok(self
            .docs()
            .count_documents(doc! { "rule": rule, "key": key, "mailed_on": day })
            .await?
            > 0)
    }

    /// Claim the mail of `id` for `day`: true exactly once per row and day,
    /// whichever pod asks first.
    pub async fn mark_mailed(&self, id: ObjectId, day: &str) -> DaoResult<bool> {
        let r = self
            .docs()
            .update_one(
                doc! { "_id": id, "mailed_on": { "$ne": day } },
                doc! { "$set": { "mailed_on": day } },
            )
            .await?;
        Ok(r.modified_count == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An open row stores `cleared_at` as an explicit null — what the
    /// partial unique index keys on — and round-trips.
    #[test]
    fn an_open_row_stores_an_explicit_null_cleared_at() {
        let a = GrowthAlert {
            id: ObjectId::new(),
            rule: "spike".into(),
            key: "signups".into(),
            fired_at: DateTime::now(),
            cleared_at: None,
            detail: doc! { "day": "2026-10-14", "value": 9_i64 },
            mailed_on: None,
        };
        let d = bson::to_document(&a).unwrap();
        assert_eq!(d.get("cleared_at"), Some(&bson::Bson::Null), "{d:?}");
        assert_eq!(d.get_str("rule").unwrap(), "spike");
        let back: GrowthAlert = bson::from_document(d).unwrap();
        assert_eq!(back, a);
        assert!(back.is_open());
        // A row written before `mailed_on` existed still reads.
        let old = doc! {
            "_id": ObjectId::new(), "rule": "r", "key": "k",
            "fired_at": DateTime::now(), "cleared_at": DateTime::now(),
        };
        let back: GrowthAlert = bson::from_document(old).unwrap();
        assert!(!back.is_open());
        assert!(back.detail.is_empty() && back.mailed_on.is_none());
    }
}

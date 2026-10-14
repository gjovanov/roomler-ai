// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P1d — `growth_digests`: one row per closed UTC day, `_id` the day.
//!
//! The once-only story (spec §3l, AC23) is the unique `_id`: a run first
//! INSERTS `{_id: "YYYY-MM-DD", status: computing}`, and the pod that loses
//! the insert stops. A `failed` or `preview` day, or a `computing` one whose
//! claim is older than the staleness cutoff (a pod died mid-run), is
//! re-claimed with a compare-and-set on exactly the status and `claimed_at`
//! the reader saw — so two re-claimers cannot both win. A `sent` day is never
//! claimed again.
//!
//! Every run — the scheduler's and the admin's manual one — holds a day
//! through [`DigestStore::claim`], in one of three [`ClaimMode`]s. That is
//! what keeps the two from colliding: a manual run that does not deliver
//! ends `preview`, a status the scheduler re-claims like `failed`, so looking
//! at a day never cancels its email; a manual run that delivers claims the
//! day exactly as the scheduler would, so the two cannot both mail it; and a
//! fresh `computing` claim refuses every other run (the route answers 409).
//! A terminal day (`sent`; `stored` for a preview) is recomputed IN PLACE —
//! its sections refreshed, its status never touched — so a manual run can
//! never turn a mailed day back into one the scheduler would mail again.
//!
//! The sections are stored as a BSON document the host serialises from its
//! typed `Sections` (never a hand-built `doc!{}`: a field added later must
//! travel). This crate does not know that type, so the store carries a
//! `Document`.

use bson::{DateTime, Document, doc};
use futures::TryStreamExt;
use mongodb::{Collection, Database};
use serde::{Deserialize, Serialize};

use super::base::DaoResult;

pub const GROWTH_DIGESTS: &str = "growth_digests";

/// The stored shape's version, for a reader that meets an older row.
/// 2 since FR-102 P3: `sections.install` became a section of its own and
/// `orgs_devices.by_install_method` a measured list; a version-1 row still
/// answers the GET as plain JSON (`digest::sections_json` falls back).
pub const DIGEST_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestStatus {
    /// Claimed; the compute (and the send) is in flight — or died.
    Computing,
    /// Computed and mailed to at least one recipient. Final.
    Sent,
    /// Computed by a delivering run; nothing to mail (no recipients, or no
    /// mail service). Final for the scheduler.
    Stored,
    /// The compute or every send failed; `error` says why. Retried.
    Failed,
    /// Computed by a manual run that did not deliver (`send=false`). NOT
    /// final: the scheduler re-claims it like `failed`, so a preview never
    /// cancels the day's email.
    Preview,
}

impl DigestStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DigestStatus::Computing => "computing",
            DigestStatus::Sent => "sent",
            DigestStatus::Stored => "stored",
            DigestStatus::Failed => "failed",
            DigestStatus::Preview => "preview",
        }
    }
}

/// How many users and orgs the sections left out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedCounts {
    pub users: u32,
    pub tenants: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestDoc {
    /// The UTC day, `YYYY-MM-DD`.
    #[serde(rename = "_id")]
    pub day: String,
    pub status: DigestStatus,
    pub version: u32,
    pub claimed_at: DateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computed_at: Option<DateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<DateTime>,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A COUNT of recipients, never the addresses.
    #[serde(default)]
    pub recipients: u32,
    #[serde(default)]
    pub excluded: ExcludedCounts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Document>,
}

/// Who is asking for the day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimMode {
    /// The scheduler: a finished day (`sent`, `stored`) is done.
    Scheduled,
    /// `POST …/run?send=false`: recompute without delivering. A `sent` or
    /// `stored` day is refreshed in place; anything else is claimed and
    /// ends `preview`.
    ManualPreview,
    /// `POST …/run?send=true`: recompute and deliver. A `sent` day is
    /// refreshed in place and never re-sent; a `stored` day (never mailed)
    /// is claimed like a `failed` one.
    ManualSend,
}

/// What a claim attempt found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This caller owns the day now; `attempts` counts this one; `prior`
    /// is the status it took the day from (`None` for a fresh insert).
    Claimed {
        attempts: u32,
        prior: Option<DigestStatus>,
    },
    /// A fresh `computing` claim: a run of this day is in progress on some
    /// pod (the manual route answers 409), or another re-claimer won.
    Busy,
    /// The scheduler's answer for a finished day: nothing to do.
    Done(DigestStatus),
    /// A manual run's answer for a terminal day: recompute the sections in
    /// place, and leave the status alone.
    InPlace(DigestStatus),
}

pub struct DigestStore {
    db: Database,
}

impl DigestStore {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }

    fn coll(&self) -> Collection<DigestDoc> {
        self.db.collection::<DigestDoc>(GROWTH_DIGESTS)
    }

    fn docs(&self) -> Collection<Document> {
        self.db.collection::<Document>(GROWTH_DIGESTS)
    }

    /// Claim `day` for a run in `mode`: insert-first, then the compare-and-
    /// set re-claims. `stale_secs` is the age past which a `computing`
    /// claim is presumed dead.
    ///
    /// | existing | scheduled | manual preview | manual send |
    /// |---|---|---|---|
    /// | none | claimed | claimed | claimed |
    /// | `computing`, fresh | busy | busy | busy |
    /// | `computing`, stale · `failed` · `preview` | re-claimed | re-claimed | re-claimed |
    /// | `stored` | done | in place | re-claimed |
    /// | `sent` | done | in place | in place |
    pub async fn claim(
        &self,
        day: &str,
        now: DateTime,
        stale_secs: i64,
        mode: ClaimMode,
    ) -> DaoResult<ClaimOutcome> {
        let fresh = DigestDoc {
            day: day.to_string(),
            status: DigestStatus::Computing,
            version: DIGEST_VERSION,
            claimed_at: now,
            computed_at: None,
            sent_at: None,
            attempts: 1,
            error: None,
            recipients: 0,
            excluded: ExcludedCounts::default(),
            sections: None,
        };
        match self.coll().insert_one(&fresh).await {
            Ok(_) => {
                return Ok(ClaimOutcome::Claimed {
                    attempts: 1,
                    prior: None,
                });
            }
            Err(e) if super::stats::is_dup_key(&e) => {}
            Err(e) => return Err(e.into()),
        }
        let Some(existing) = self.get(day).await? else {
            // Lost the insert to a row that vanished since: treat as busy;
            // the next tick sees whatever is there.
            return Ok(ClaimOutcome::Busy);
        };
        let cutoff = DateTime::from_millis(now.timestamp_millis() - stale_secs * 1000);
        let filter = match (existing.status, mode) {
            (DigestStatus::Sent, ClaimMode::Scheduled) => {
                return Ok(ClaimOutcome::Done(DigestStatus::Sent));
            }
            (DigestStatus::Sent, _) => return Ok(ClaimOutcome::InPlace(DigestStatus::Sent)),
            (DigestStatus::Stored, ClaimMode::Scheduled) => {
                return Ok(ClaimOutcome::Done(DigestStatus::Stored));
            }
            (DigestStatus::Stored, ClaimMode::ManualPreview) => {
                return Ok(ClaimOutcome::InPlace(DigestStatus::Stored));
            }
            (DigestStatus::Stored, ClaimMode::ManualSend) => {
                doc! { "_id": day, "status": "stored" }
            }
            (DigestStatus::Failed, _) => doc! { "_id": day, "status": "failed" },
            (DigestStatus::Preview, _) => doc! { "_id": day, "status": "preview" },
            (DigestStatus::Computing, _) => {
                if existing.claimed_at >= cutoff {
                    return Ok(ClaimOutcome::Busy);
                }
                doc! { "_id": day, "status": "computing", "claimed_at": existing.claimed_at }
            }
        };
        let updated = self
            .docs()
            .find_one_and_update(
                filter,
                doc! {
                    "$set": { "status": "computing", "claimed_at": now },
                    "$inc": { "attempts": 1 },
                    "$unset": { "error": "" },
                },
            )
            .return_document(mongodb::options::ReturnDocument::After)
            .await?;
        Ok(match updated {
            // ⚠️ Not `get_i32`: the typed insert stores `attempts` as Int64
            // and `$inc` keeps the width, so an i32 read answered 0 for every
            // re-claimed day (caught by the AC23 test).
            Some(d) => ClaimOutcome::Claimed {
                attempts: int_of(&d, "attempts").max(0) as u32,
                prior: Some(existing.status),
            },
            None => ClaimOutcome::Busy,
        })
    }

    /// Store a computed day's sections (overwriting any earlier ones).
    pub async fn store_sections(
        &self,
        day: &str,
        sections: Document,
        excluded: ExcludedCounts,
        recipients: u32,
        computed_at: DateTime,
    ) -> DaoResult<()> {
        self.docs()
            .update_one(
                doc! { "_id": day },
                doc! { "$set": {
                    "sections": sections,
                    "excluded": bson::to_bson(&excluded)?,
                    "recipients": recipients as i64,
                    "computed_at": computed_at,
                    "version": DIGEST_VERSION,
                } },
            )
            .await?;
        Ok(())
    }

    /// Move the day to its terminal status. `sent_at` only with `Sent`;
    /// `error` with `Failed` (or a partial-delivery note on `Sent`).
    pub async fn set_status(
        &self,
        day: &str,
        status: DigestStatus,
        sent_at: Option<DateTime>,
        error: Option<String>,
    ) -> DaoResult<()> {
        let mut set = doc! { "status": status.as_str() };
        let mut unset = Document::new();
        if let Some(at) = sent_at {
            set.insert("sent_at", at);
        }
        match error {
            Some(e) => {
                set.insert("error", e);
            }
            None => {
                unset.insert("error", "");
            }
        }
        let mut update = doc! { "$set": set };
        if !unset.is_empty() {
            update.insert("$unset", unset);
        }
        self.docs().update_one(doc! { "_id": day }, update).await?;
        Ok(())
    }

    pub async fn get(&self, day: &str) -> DaoResult<Option<DigestDoc>> {
        Ok(self.coll().find_one(doc! { "_id": day }).await?)
    }

    /// The newest day with a row, by `_id` (`YYYY-MM-DD` sorts as a date).
    pub async fn latest(&self) -> DaoResult<Option<DigestDoc>> {
        Ok(self
            .coll()
            .find_one(doc! {})
            .sort(doc! { "_id": -1 })
            .await?)
    }

    /// Days from `since` (inclusive) on, newest first, at most `limit`, with
    /// only the headline of the sections (the listing is a table, not a
    /// report).
    pub async fn list(&self, since: Option<&str>, limit: i64) -> DaoResult<Vec<DigestDoc>> {
        let filter = match since {
            Some(s) => doc! { "_id": { "$gte": s } },
            None => doc! {},
        };
        let rows: Vec<DigestDoc> = self
            .coll()
            .find(filter)
            .projection(doc! {
                "status": 1, "version": 1, "claimed_at": 1, "computed_at": 1, "sent_at": 1,
                "attempts": 1, "error": 1, "recipients": 1, "excluded": 1,
                "sections.headline": 1,
            })
            .sort(doc! { "_id": -1 })
            .limit(limit)
            .await?
            .try_collect()
            .await?;
        Ok(rows)
    }
}

/// A stored integer whatever width the driver chose for it.
fn int_of(d: &Document, key: &str) -> i64 {
    match d.get(key) {
        Some(bson::Bson::Int32(v)) => i64::from(*v),
        Some(bson::Bson::Int64(v)) => *v,
        Some(bson::Bson::Double(v)) => *v as i64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claim's attempt count is read at whatever width the driver
    /// stored it — the typed insert writes Int64, a `$inc` keeps it.
    #[test]
    fn attempts_read_at_any_integer_width() {
        assert_eq!(int_of(&doc! { "attempts": 2_i32 }, "attempts"), 2);
        assert_eq!(int_of(&doc! { "attempts": 2_i64 }, "attempts"), 2);
        assert_eq!(int_of(&doc! { "attempts": 2.0 }, "attempts"), 2);
        assert_eq!(int_of(&doc! {}, "attempts"), 0);
        let stored = bson::to_document(&DigestDoc {
            day: "d".into(),
            status: DigestStatus::Computing,
            version: DIGEST_VERSION,
            claimed_at: DateTime::now(),
            computed_at: None,
            sent_at: None,
            attempts: 3,
            error: None,
            recipients: 0,
            excluded: ExcludedCounts::default(),
            sections: None,
        })
        .unwrap();
        assert_eq!(int_of(&stored, "attempts"), 3, "{stored:?}");
    }

    #[test]
    fn status_spellings_are_locked() {
        for (s, w) in [
            (DigestStatus::Computing, "computing"),
            (DigestStatus::Sent, "sent"),
            (DigestStatus::Stored, "stored"),
            (DigestStatus::Failed, "failed"),
            (DigestStatus::Preview, "preview"),
        ] {
            assert_eq!(s.as_str(), w);
            assert_eq!(serde_json::to_string(&s).unwrap(), format!("\"{w}\""));
        }
    }

    /// The row round-trips as a struct, with the day as `_id`.
    #[test]
    fn the_row_round_trips_with_the_day_as_id() {
        let row = DigestDoc {
            day: "2026-10-13".into(),
            status: DigestStatus::Stored,
            version: DIGEST_VERSION,
            claimed_at: DateTime::now(),
            computed_at: Some(DateTime::now()),
            sent_at: None,
            attempts: 2,
            error: None,
            recipients: 3,
            excluded: ExcludedCounts {
                users: 1,
                tenants: 2,
            },
            sections: Some(doc! { "headline": { "signups": { "day": 1, "d7": 2, "d28": 3 } } }),
        };
        let doc = bson::to_document(&row).unwrap();
        assert_eq!(doc.get_str("_id").unwrap(), "2026-10-13");
        assert_eq!(doc.get_str("status").unwrap(), "stored");
        assert!(doc.get("sent_at").is_none(), "absent, not null");
        let back: DigestDoc = bson::from_document(doc).unwrap();
        assert_eq!(back.day, "2026-10-13");
        assert_eq!(back.status, DigestStatus::Stored);
        assert_eq!(back.recipients, 3);
        assert_eq!(back.excluded.tenants, 2);
    }
}

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
//! * [`GROWTH_ORG_DAILY`] — one row per org per UTC day with any activity
//!   (P2, spec §3d): `_id = "{tenant}:{yyyy-mm-dd}"`, the counters and the
//!   `value` flag. Written whole by the host's nightly job
//!   (`crates/api/src/growth/org_daily.rs`), read by the funnel, the
//!   cohorts and the org views. It exists because four of its sources
//!   expire at 90 days and retention needs a year: a row here outlives
//!   the audit rows it was computed from. Bounded by a 25-month TTL;
//!   [`GrowthDao::purge_tenant`] is the one call an org deletion must make
//!   to take an org's rows with it, though no org hard-delete path exists
//!   yet (orgs are only archived, reversibly), so until one does the TTL
//!   is what bounds the collection.
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

use std::collections::{BTreeMap, HashMap};

use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use mongodb::{Collection, Database};
use serde::{Deserialize, Serialize};

use super::base::DaoResult;

pub const GROWTH_EXCLUSIONS: &str = "growth_exclusions";
pub const GROWTH_ORG_DAILY: &str = "growth_org_daily";
/// The frozen daily totals (spec §3o): written by P3's install trace and
/// read by the digest; a tenant purge also takes an org's `dims.tenant`
/// rows from here.
pub const GROWTH_DAILY: &str = "growth_daily";

/// P3 — the install trace's `(source, metric)`: written by the `fleet`
/// module's fetch counters, read by the host's digest. Named here, in the
/// crate both can see, because `fleet` is an optional dependency of the host
/// and the digest must read the rows whether or not that module is compiled.
pub const INSTALL_SOURCE: &str = "install";
pub const INSTALL_FETCH_METRIC: &str = "fetch";

/// The most org-days one read loads — far above this deployment's volume
/// (every org active every day for 25 months is ~10 rows a day); a bound,
/// not a page.
pub const MAX_ORG_DAY_ROWS: i64 = 500_000;

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

/// One org's activity on one UTC day (spec §3d). Every counter is what
/// happened THAT day: sessions and calls are counted on the day they
/// started, minutes are each session's overlap with the day (a session
/// spanning midnight is split), and `value` says whether any of the four
/// value events happened — a remote-desktop session that reached Active
/// and lasted ≥ 30 s, a call with ≥ 2 participants, a tunnel flow, an SSH
/// session — with `value_kinds` naming which.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OrgDay {
    /// `"{tenant hex}:{YYYY-MM-DD}"` — the idempotent upsert key.
    #[serde(rename = "_id")]
    pub id: String,
    pub tenant_id: ObjectId,
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    /// The day's 00:00 UTC: the TTL anchor and the range key.
    pub day_start: DateTime,
    /// Remote-desktop sessions that reached Active and started that day.
    pub rc_sessions: i64,
    /// Minutes of remote-desktop sessions overlapping the day, to 0.1.
    pub rc_session_minutes: f64,
    /// Distinct controllers of the sessions that started that day.
    pub rc_controllers: i64,
    /// Calls that started that day.
    pub calls: i64,
    /// Participant-minutes of the calls that started that day.
    pub call_participant_minutes: f64,
    /// Tunnel flows accepted that day (`tunnel_audit.kind = tcp_accept`).
    pub tunnel_flows: i64,
    /// SSH grants issued that day (`ssh_audit` rows without `denied`).
    pub ssh_sessions: i64,
    /// Remote commands that ran that day (`exec_audit` rows without `denied`).
    pub exec_runs: i64,
    /// Distinct devices seen online that day (presence transitions, or a
    /// daily machine rollup with online minutes).
    pub devices_online: i64,
    /// Distinct members who opened a session or controlled a device that day.
    pub members_active: i64,
    /// A value event happened that day.
    pub value: bool,
    /// Which value events: `rc` · `call` · `tunnel` · `ssh`, sorted.
    #[serde(default)]
    pub value_kinds: Vec<String>,
    pub computed_at: DateTime,
}

impl OrgDay {
    /// The row id: `"{tenant hex}:{day}"`.
    pub fn id_of(tenant_id: &ObjectId, day: &str) -> String {
        format!("{}:{day}", tenant_id.to_hex())
    }
}

/// What a tenant purge removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PurgedCounts {
    pub org_days: u64,
    pub daily: u64,
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

    // ── growth_org_daily (P2) ────────────────────────────────────────────

    fn org_days(&self) -> Collection<OrgDay> {
        self.db.collection::<OrgDay>(GROWTH_ORG_DAILY)
    }

    fn org_day_docs(&self) -> Collection<Document> {
        self.db.collection::<Document>(GROWTH_ORG_DAILY)
    }

    /// Write one day's rows: each row replaced on its `_id` (an upsert), then
    /// every row of that day whose org is NOT in `rows` deleted — an org
    /// whose activity vanished on a recompute (its sessions were removed)
    /// must not keep yesterday's row. Replace-then-prune rather than
    /// delete-then-insert, so a reader never meets an empty day between the
    /// two. Returns the rows written.
    pub async fn replace_day(&self, day: &str, rows: &[OrgDay]) -> DaoResult<u64> {
        let coll = self.org_days();
        let mut ids: Vec<String> = Vec::with_capacity(rows.len());
        for row in rows {
            debug_assert_eq!(row.day, day, "a row written under the wrong day");
            coll.replace_one(doc! { "_id": &row.id }, row)
                .upsert(true)
                .await?;
            ids.push(row.id.clone());
        }
        self.org_day_docs()
            .delete_many(doc! { "day": day, "_id": { "$nin": ids } })
            .await?;
        Ok(rows.len() as u64)
    }

    /// The rows of `[from_day, to_day]` (inclusive, `YYYY-MM-DD`), for the
    /// given orgs (every org when `None`) minus `excluded`; only the value
    /// days when `value_only`. Oldest first, bounded by
    /// [`MAX_ORG_DAY_ROWS`].
    pub async fn org_days_between(
        &self,
        tenants: Option<&[ObjectId]>,
        excluded: &[ObjectId],
        from_day: &str,
        to_day: &str,
        value_only: bool,
    ) -> DaoResult<Vec<OrgDay>> {
        let mut filter = doc! { "day": { "$gte": from_day, "$lte": to_day } };
        match tenants {
            Some(ids) => {
                filter.insert("tenant_id", doc! { "$in": ids, "$nin": excluded });
            }
            None => {
                filter.insert("tenant_id", doc! { "$nin": excluded });
            }
        }
        if value_only {
            filter.insert("value", true);
        }
        let rows: Vec<OrgDay> = self
            .org_days()
            .find(filter)
            .sort(doc! { "day": 1, "tenant_id": 1 })
            .limit(MAX_ORG_DAY_ROWS)
            .await?
            .try_collect()
            .await?;
        Ok(rows)
    }

    /// Each org's FIRST value day and the kinds of that day — the funnel's
    /// first-value step and the timeline's milestone.
    pub async fn first_value_days(
        &self,
        tenants: &[ObjectId],
    ) -> DaoResult<HashMap<ObjectId, (String, Vec<String>)>> {
        if tenants.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<Document> = self
            .org_day_docs()
            .aggregate(vec![
                doc! { "$match": { "tenant_id": { "$in": tenants }, "value": true } },
                doc! { "$sort": { "tenant_id": 1, "day": 1 } },
                doc! { "$group": {
                    "_id": "$tenant_id",
                    "day": { "$first": "$day" },
                    "kinds": { "$first": "$value_kinds" },
                } },
            ])
            .await?
            .try_collect()
            .await?;
        Ok(rows
            .iter()
            .filter_map(|d| {
                let tenant = d.get_object_id("_id").ok()?;
                let day = d.get_str("day").ok()?.to_string();
                let kinds = d
                    .get_array("kinds")
                    .map(|a| {
                        a.iter()
                            .filter_map(|k| k.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                Some((tenant, (day, kinds)))
            })
            .collect())
    }

    /// Each org's LAST value day, over every org not excluded.
    pub async fn last_value_days(
        &self,
        excluded: &[ObjectId],
    ) -> DaoResult<HashMap<ObjectId, String>> {
        let rows: Vec<Document> = self
            .org_day_docs()
            .aggregate(vec![
                doc! { "$match": { "tenant_id": { "$nin": excluded }, "value": true } },
                doc! { "$group": { "_id": "$tenant_id", "day": { "$max": "$day" } } },
            ])
            .await?
            .try_collect()
            .await?;
        Ok(rows
            .iter()
            .filter_map(|d| {
                Some((
                    d.get_object_id("_id").ok()?,
                    d.get_str("day").ok()?.to_string(),
                ))
            })
            .collect())
    }

    /// The first and last day with any row — the horizon the views can see.
    pub async fn day_bounds(&self) -> DaoResult<Option<(String, String)>> {
        let first = self
            .org_day_docs()
            .find_one(doc! {})
            .projection(doc! { "day": 1 })
            .sort(doc! { "day": 1 })
            .await?;
        let Some(first) = first else {
            return Ok(None);
        };
        let last = self
            .org_day_docs()
            .find_one(doc! {})
            .projection(doc! { "day": 1 })
            .sort(doc! { "day": -1 })
            .await?;
        let day = |d: &Document| d.get_str("day").map(str::to_string).ok();
        Ok(match (day(&first), last.as_ref().and_then(day)) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        })
    }

    /// Everything the growth collections hold about one org: its
    /// `growth_org_daily` rows, and the `growth_daily` rows whose `dims`
    /// carry the tenant (spec principle 7 — rows never outlive their
    /// owner). This is the one call an org deletion must make for growth;
    /// no org hard-delete path exists yet (orgs are only archived,
    /// reversibly), so for now it has no production caller and the 25-month
    /// TTL is what bounds `growth_org_daily`. Idempotent; a second call
    /// deletes nothing.
    pub async fn purge_tenant(&self, tenant_id: ObjectId) -> DaoResult<PurgedCounts> {
        let org_days = self
            .org_day_docs()
            .delete_many(doc! { "tenant_id": tenant_id })
            .await?
            .deleted_count;
        // The frozen totals key the tenant as its hex id (the dims are
        // strings); an ObjectId is accepted too so a writer that stores
        // the id itself is purged just the same.
        let daily = self
            .db
            .collection::<Document>(GROWTH_DAILY)
            .delete_many(doc! { "$or": [
                { "dims.tenant": tenant_id.to_hex() },
                { "dims.tenant": Bson::ObjectId(tenant_id) },
            ] })
            .await?
            .deleted_count;
        Ok(PurgedCounts { org_days, daily })
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

    /// P2 — an org-day round-trips as a struct with the composite `_id`,
    /// and a row from before `value_kinds` existed still reads.
    #[test]
    fn an_org_day_round_trips_with_its_composite_id() {
        let tenant = ObjectId::new();
        let row = OrgDay {
            id: OrgDay::id_of(&tenant, "2026-10-01"),
            tenant_id: tenant,
            day: "2026-10-01".into(),
            day_start: DateTime::parse_rfc3339_str("2026-10-01T00:00:00Z").unwrap(),
            rc_sessions: 2,
            rc_session_minutes: 60.2,
            rc_controllers: 1,
            calls: 1,
            call_participant_minutes: 10.0,
            tunnel_flows: 3,
            ssh_sessions: 1,
            exec_runs: 0,
            devices_online: 2,
            members_active: 2,
            value: true,
            value_kinds: vec!["rc".into(), "tunnel".into()],
            computed_at: DateTime::now(),
        };
        assert_eq!(row.id, format!("{}:2026-10-01", tenant.to_hex()));
        let mut doc = bson::to_document(&row).unwrap();
        assert_eq!(doc.get_str("_id").unwrap(), row.id);
        let back: OrgDay = bson::from_document(doc.clone()).unwrap();
        assert_eq!(back, row);
        doc.remove("value_kinds");
        let older: OrgDay = bson::from_document(doc).unwrap();
        assert!(older.value_kinds.is_empty());
        assert!(older.value);
    }
}

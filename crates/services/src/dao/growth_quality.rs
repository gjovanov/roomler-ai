// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P9a — `growth_quality_daily`: remote-desktop quality per UTC day
//! and per agent version, frozen nightly from what the server already
//! records (spec §3f).
//!
//! One row per `(day, scope, agent_version)`, `_id` the three joined, written
//! whole by the host's nightly freeze (`crates/api/src/growth/quality.rs`)
//! and read by the Quality view, the digest and the weekly roll-up. It exists
//! because the one number that cannot be recomputed later lives in
//! `agent_logs`, which expire after **7 days**: FR-22's time to first frame
//! is a browser record there, and once the rows are gone the day's p50/p90
//! are gone with them. Crashes expire at 90 days and the agent version a
//! device ran on a given day is only known from that day's logs, so those
//! are frozen too; sessions are kept forever and are frozen beside them so
//! one row answers the whole question.
//!
//! * `scope` is `external` (the exclusion set applied, the view every number
//!   shows) or `all` (nothing excluded — `include_internal=true`). Both are
//!   frozen, with the exclusion set **as it stood at the freeze**, like the
//!   billing months; the recompute window and the backfill route re-freeze a
//!   day with the current set while its sources still exist.
//! * `agent_version` is a release (`0.4.137`), `unknown`, or [`ALL_VERSIONS`]
//!   (`*`) for the row that sums every version.
//! * A [`Dist`] carries the day's exact p50/p90 **and** a histogram over
//!   fixed edges, so a window's percentiles can be estimated from several
//!   days' rows without the samples (`Dist::pool`). The estimate says it is
//!   one (`Pooled::exact == false`).
//! * Every optional field is **`None` for "not measured"** — a module not
//!   mounted, or logs already expired when the day was frozen — never a
//!   zero (spec principle 4).
//!
//! No TTL and no person: aggregates about the product, kept indefinitely
//! like `growth_daily` (spec principle 7).
//!
//! Stateless over `core.db`, so a caller re-creates it (FR-69 rule 5).

use std::collections::BTreeMap;

use bson::{DateTime, Document, doc};
use futures::TryStreamExt;
use mongodb::{Collection, Database};
use serde::{Deserialize, Serialize};

use super::base::DaoResult;

pub const GROWTH_QUALITY_DAILY: &str = "growth_quality_daily";

/// The `agent_version` of the row that sums every version. Not a spelling a
/// release can have.
pub const ALL_VERSIONS: &str = "*";

/// The `agent_version` of a device whose version no source could say.
pub const UNKNOWN_VERSION: &str = "unknown";

/// Time to first frame, milliseconds: the edges the daily histogram is cut
/// at. A connect under 250 ms and one over 32 s are the two ends of what the
/// FR-22 field reads saw.
pub const TTFF_EDGES_MS: &[i64] = &[
    250, 500, 750, 1000, 1500, 2000, 3000, 4000, 6000, 8000, 12_000, 16_000, 24_000, 32_000,
];
/// An Active session's duration, milliseconds (10 s … 8 h).
pub const DURATION_EDGES_MS: &[i64] = &[
    10_000, 30_000, 60_000, 120_000, 300_000, 600_000, 1_200_000, 1_800_000, 3_600_000, 7_200_000,
    14_400_000, 28_800_000,
];
/// `stats.peak_fps`, rounded to whole frames.
pub const FPS_EDGES: &[i64] = &[5, 10, 15, 20, 25, 30, 40, 50, 60, 90, 120];
/// `stats.avg_rtt_ms`, rounded to whole milliseconds.
pub const RTT_EDGES_MS: &[i64] = &[10, 20, 30, 50, 75, 100, 150, 200, 300, 500, 1000];

/// Which view a row is: the exclusion set applied, or nothing excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    External,
    All,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::External => "external",
            Scope::All => "all",
        }
    }

    /// `include_internal=true` reads the `all` rows.
    pub fn of_include_internal(include_internal: bool) -> Self {
        if include_internal {
            Scope::All
        } else {
            Scope::External
        }
    }
}

/// The nearest-rank percentile of a SORTED sample: the smallest value with
/// at least `q · n` of the sample at or below it. `None` for an empty sample.
pub fn nearest_rank(sorted: &[i64], q: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

/// A distribution of integer samples: the exact p50 and p90, and a histogram
/// over `edges` — bucket `i` holds the samples in `(edges[i-1], edges[i]]`
/// (the first from 0), the last bucket everything above the last edge — so
/// that a window's percentiles can be estimated from several rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dist {
    pub n: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90: Option<i64>,
    pub edges: Vec<i64>,
    /// `edges.len() + 1` counts.
    pub hist: Vec<i64>,
}

impl Dist {
    /// The distribution of `samples` over `edges`.
    pub fn of(mut samples: Vec<i64>, edges: &[i64]) -> Self {
        samples.sort_unstable();
        let mut hist = vec![0i64; edges.len() + 1];
        for &s in &samples {
            let i = edges.iter().position(|&e| s <= e).unwrap_or(edges.len());
            hist[i] += 1;
        }
        Self {
            n: samples.len() as i64,
            p50: nearest_rank(&samples, 0.5),
            p90: nearest_rank(&samples, 0.9),
            edges: edges.to_vec(),
            hist,
        }
    }

    /// Pool several days' distributions into one window. With one part the
    /// result is that part's exact percentiles; with more, an estimate from
    /// the summed histogram, interpolated inside the bucket the rank falls
    /// in (the last bucket answers its lower edge: a floor, never a guess
    /// above it). Parts whose edges differ cannot be pooled: the counts are
    /// summed and the percentiles are `None`.
    pub fn pool<'a>(parts: impl IntoIterator<Item = &'a Dist>) -> Pooled {
        let parts: Vec<&Dist> = parts.into_iter().collect();
        let n: i64 = parts.iter().map(|p| p.n).sum();
        let mut pooled = Pooled {
            n,
            p50: None,
            p90: None,
            exact: parts.len() <= 1,
            parts: parts.len() as i64,
        };
        let Some(first) = parts.first() else {
            return pooled;
        };
        if parts.len() == 1 {
            pooled.p50 = first.p50;
            pooled.p90 = first.p90;
            return pooled;
        }
        if n == 0 || parts.iter().any(|p| p.edges != first.edges) {
            return pooled;
        }
        let mut hist = vec![0i64; first.edges.len() + 1];
        for p in &parts {
            for (i, c) in p.hist.iter().enumerate() {
                if let Some(slot) = hist.get_mut(i) {
                    *slot += c;
                }
            }
        }
        pooled.p50 = estimate(&hist, &first.edges, n, 0.5);
        pooled.p90 = estimate(&hist, &first.edges, n, 0.9);
        pooled
    }
}

/// The value at rank `ceil(q · n)` of a histogram, interpolated linearly
/// inside its bucket (midpoint rule).
fn estimate(hist: &[i64], edges: &[i64], n: i64, q: f64) -> Option<i64> {
    if n <= 0 {
        return None;
    }
    let rank = ((q * n as f64).ceil() as i64).clamp(1, n);
    let mut before = 0i64;
    for (i, &count) in hist.iter().enumerate() {
        if count > 0 && before + count >= rank {
            let lo = if i == 0 { 0 } else { edges[i - 1] };
            let Some(&hi) = edges.get(i) else {
                // The overflow bucket: its lower edge, a floor.
                return Some(lo);
            };
            let frac = ((rank - before) as f64 - 0.5) / count as f64;
            let frac = frac.clamp(0.0, 1.0);
            return Some((lo as f64 + frac * (hi - lo) as f64).round() as i64);
        }
        before += count;
    }
    None
}

/// Several days' [`Dist`]s read as one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pooled {
    pub n: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90: Option<i64>,
    /// `true` when the percentiles are a single day's exact values; `false`
    /// when estimated from the summed histograms.
    pub exact: bool,
    /// The days (rows) pooled.
    pub parts: i64,
}

/// One `(reached Active?, end_reason)` cell of a day's sessions. `reason` is
/// the stored `end_reason`, `open` for a session not ended when the day was
/// frozen (a running session on a day just closed; a leak once the day is
/// older than the recompute window).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndReasonCount {
    pub reached_active: bool,
    pub reason: String,
    pub count: i64,
}

/// FR-102 P9c — the Active sessions that rode one carrier, with their
/// distributions: whether TURN or DERP sessions feel different is the
/// question this answers. `carrier` is the agent's spelling (`direct` ·
/// `turn` · `turn_local` · `overlay_direct` · `overlay_turn` · `derp` ·
/// `org_relay`), or `unknown` for a row with none — a session from before
/// P9c, from an agent older than the field, or one that ended before the
/// observation landed. **Never read as direct.**
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarrierQuality {
    pub carrier: String,
    /// Sessions that reached Active on this carrier.
    pub reached_active: i64,
    pub duration_ms: Dist,
    pub peak_fps: Dist,
    pub avg_rtt_ms: Dist,
}

/// The sessions REQUESTED on the day (`remote_sessions.created_at`), by what
/// became of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionQuality {
    /// Sessions created that day.
    pub requested: i64,
    /// … that reached Active (`started_at` set), ended or still running.
    pub reached_active: i64,
    /// … that ended without ever reaching Active (a refusal, a connect
    /// failure, a controller that gave up waiting).
    pub not_reached: i64,
    /// … neither started nor ended when the day was frozen.
    pub pending: i64,
    pub by_end_reason: Vec<EndReasonCount>,
    /// Active sessions that ended: `ended_at − started_at`, milliseconds.
    pub duration_ms: Dist,
    /// Active sessions whose device reported a peak frame rate above 0.
    pub peak_fps: Dist,
    /// Active sessions whose device reported an average RTT above 0.
    pub avg_rtt_ms: Dist,
    /// P9c — the Active sessions by the carrier they rode; a row frozen
    /// before P9c has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_carrier: Vec<CarrierQuality>,
}

/// FR-22's time to first frame on the day, from the browser's `rc.connect`
/// records: the distribution of `ttff_ms` over the records that painted
/// (`first_frame`) in a **foreground** tab on a **first attempt** that did
/// not follow a drop — the filter the FR-22 doc mines with — and what was
/// set aside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtffQuality {
    pub dist: Dist,
    /// `first_frame` records from a hidden tab (invalid paint timing).
    pub hidden: i64,
    /// Foreground `first_frame` records of a later attempt or after a drop.
    pub later_attempts: i64,
    /// Every foreground record by outcome: `first_frame` · `abandoned` ·
    /// `closed` · `retried`.
    pub outcomes: BTreeMap<String, i64>,
}

/// One row: a day, a scope, a version. An absent part is **not measured**.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityDay {
    /// `"{day}:{scope}:{agent_version}"`.
    #[serde(rename = "_id")]
    pub id: String,
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    /// The day's 00:00 UTC.
    pub day_start: DateTime,
    pub scope: Scope,
    pub agent_version: String,
    /// `None` without the `remote` module.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<SessionQuality>,
    /// `None` without the `fleet` module, or when the day's `agent_logs` had
    /// started to expire before the freeze read them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttff: Option<TtffQuality>,
    /// Crash reports received that day (`agent_crashes.reported_at`), by the
    /// version the crashing agent said it ran. `None` without `fleet`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crashes: Option<i64>,
    /// Devices online that day (`stats_machine_1d.online_minutes > 0`).
    /// `None` without `fleet`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_days: Option<i64>,
    pub computed_at: DateTime,
}

impl QualityDay {
    pub fn id_of(day: &str, scope: Scope, agent_version: &str) -> String {
        format!("{day}:{}:{agent_version}", scope.as_str())
    }
}

pub struct GrowthQualityDao {
    db: Database,
}

impl GrowthQualityDao {
    pub fn new(db: &Database) -> Self {
        Self { db: db.clone() }
    }

    fn rows(&self) -> Collection<QualityDay> {
        self.db.collection::<QualityDay>(GROWTH_QUALITY_DAILY)
    }

    fn docs(&self) -> Collection<Document> {
        self.db.collection::<Document>(GROWTH_QUALITY_DAILY)
    }

    /// Write one `(day, scope)`'s rows: each replaced on its `_id`, then the
    /// day's rows of that scope for versions no longer present deleted.
    /// Replace-then-prune, so a reader never meets an empty day between the
    /// two. Returns the rows written.
    pub async fn replace_day(
        &self,
        day: &str,
        scope: Scope,
        rows: &[QualityDay],
    ) -> DaoResult<u64> {
        let coll = self.rows();
        let mut ids: Vec<String> = Vec::with_capacity(rows.len());
        for row in rows {
            debug_assert!(
                row.day == day && row.scope == scope,
                "a row written under the wrong (day, scope)"
            );
            coll.replace_one(doc! { "_id": &row.id }, row)
                .upsert(true)
                .await?;
            ids.push(row.id.clone());
        }
        self.docs()
            .delete_many(doc! { "day": day, "scope": scope.as_str(), "_id": { "$nin": ids } })
            .await?;
        Ok(rows.len() as u64)
    }

    /// Every row of `scope` with `from <= day <= to` (inclusive, `YYYY-MM-DD`),
    /// oldest first. A row that does not parse (a later shape) is skipped
    /// with a warning and costs itself only.
    pub async fn rows_between(
        &self,
        scope: Scope,
        from: &str,
        to: &str,
    ) -> DaoResult<Vec<QualityDay>> {
        let docs: Vec<Document> = self
            .docs()
            .find(doc! { "scope": scope.as_str(), "day": { "$gte": from, "$lte": to } })
            .sort(doc! { "day": 1, "agent_version": 1 })
            .await?
            .try_collect()
            .await?;
        let mut out = Vec::with_capacity(docs.len());
        for d in docs {
            match bson::from_document::<QualityDay>(d) {
                Ok(row) => out.push(row),
                Err(e) => tracing::warn!(
                    error = %e,
                    "growth_quality_daily: skipping a row that does not have the daily shape"
                ),
            }
        }
        Ok(out)
    }

    /// Whether any row exists — the first scheduled run fills a horizon.
    pub async fn is_empty(&self) -> DaoResult<bool> {
        Ok(self
            .docs()
            .find_one(doc! {})
            .projection(doc! { "_id": 1 })
            .await?
            .is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_rank_percentile_is_exact_on_small_samples() {
        assert_eq!(nearest_rank(&[], 0.5), None);
        assert_eq!(nearest_rank(&[7], 0.5), Some(7));
        assert_eq!(nearest_rank(&[7], 0.9), Some(7));
        assert_eq!(nearest_rank(&[1, 2], 0.5), Some(1));
        assert_eq!(nearest_rank(&[1, 2], 0.9), Some(2));
        let s: Vec<i64> = (1..=10).collect();
        assert_eq!(nearest_rank(&s, 0.5), Some(5));
        assert_eq!(nearest_rank(&s, 0.9), Some(9));
        assert_eq!(nearest_rank(&s, 1.0), Some(10));
    }

    #[test]
    fn a_distribution_buckets_its_samples_on_the_edges() {
        let d = Dist::of(vec![3000, 250, 251, 40_000, 1000], TTFF_EDGES_MS);
        assert_eq!(d.n, 5);
        assert_eq!(d.p50, Some(1000));
        assert_eq!(d.p90, Some(40_000));
        assert_eq!(d.hist.len(), TTFF_EDGES_MS.len() + 1);
        // 250 → (0, 250]; 251 → (250, 500]; 1000 → (750, 1000]; 3000 →
        // (2000, 3000]; 40 000 → above the last edge.
        assert_eq!(d.hist[0], 1);
        assert_eq!(d.hist[1], 1);
        assert_eq!(d.hist[3], 1);
        assert_eq!(d.hist[6], 1);
        assert_eq!(d.hist[TTFF_EDGES_MS.len()], 1);
        assert_eq!(d.hist.iter().sum::<i64>(), 5);
        let empty = Dist::of(vec![], FPS_EDGES);
        assert_eq!((empty.n, empty.p50, empty.p90), (0, None, None));
        assert!(empty.hist.iter().all(|&c| c == 0));
    }

    /// One part is exact; several are an estimate inside the right bucket;
    /// parts cut on different edges are counted but not estimated.
    #[test]
    fn pooling_is_exact_for_one_day_and_an_estimate_across_days() {
        let a = Dist::of(vec![1000, 1200, 1400], TTFF_EDGES_MS);
        let one = Dist::pool([&a]);
        assert_eq!(
            (one.n, one.p50, one.p90, one.exact, one.parts),
            (3, Some(1200), Some(1400), true, 1)
        );

        let b = Dist::of(vec![2500, 2600, 2700, 2800, 2900, 3000], TTFF_EDGES_MS);
        let two = Dist::pool([&a, &b]);
        assert_eq!((two.n, two.exact, two.parts), (9, false, 2));
        // Rank 5 of 9 is the second sample of the (2000, 3000] bucket, which
        // holds six: interpolated 2000 + (2 − 0.5)/6 · 1000 = 2250.
        assert_eq!(two.p50, Some(2250));
        // Rank 9 of 9 is that bucket's last: 2000 + 5.5/6 · 1000 ≈ 2917.
        assert_eq!(two.p90, Some(2917));

        // The overflow bucket answers its lower edge, a floor.
        let c = Dist::of(vec![50_000, 60_000], TTFF_EDGES_MS);
        let d = Dist::of(vec![70_000], TTFF_EDGES_MS);
        assert_eq!(Dist::pool([&c, &d]).p50, Some(32_000));

        let other = Dist::of(vec![30], FPS_EDGES);
        let mixed = Dist::pool([&a, &other]);
        assert_eq!((mixed.n, mixed.p50, mixed.p90), (4, None, None));

        let none = Dist::pool(std::iter::empty());
        assert_eq!(
            (none.n, none.p50, none.exact, none.parts),
            (0, None, true, 0)
        );
    }

    /// The row round-trips as a struct, an absent part reads as `None`, and
    /// the composite `_id` names the three keys.
    #[test]
    fn a_row_round_trips_and_an_absent_part_is_not_measured() {
        let row = QualityDay {
            id: QualityDay::id_of("2026-10-15", Scope::External, "0.4.137"),
            day: "2026-10-15".into(),
            day_start: DateTime::parse_rfc3339_str("2026-10-15T00:00:00Z").unwrap(),
            scope: Scope::External,
            agent_version: "0.4.137".into(),
            sessions: Some(SessionQuality {
                requested: 3,
                reached_active: 2,
                not_reached: 1,
                pending: 0,
                by_end_reason: vec![EndReasonCount {
                    reached_active: false,
                    reason: "consent_timeout".into(),
                    count: 1,
                }],
                duration_ms: Dist::of(vec![60_000, 120_000], DURATION_EDGES_MS),
                peak_fps: Dist::of(vec![30], FPS_EDGES),
                avg_rtt_ms: Dist::of(vec![], RTT_EDGES_MS),
                by_carrier: vec![CarrierQuality {
                    carrier: "derp".into(),
                    reached_active: 2,
                    duration_ms: Dist::of(vec![60_000, 120_000], DURATION_EDGES_MS),
                    peak_fps: Dist::of(vec![30], FPS_EDGES),
                    avg_rtt_ms: Dist::of(vec![], RTT_EDGES_MS),
                }],
            }),
            ttff: None,
            crashes: Some(1),
            device_days: Some(4),
            computed_at: DateTime::now(),
        };
        assert_eq!(row.id, "2026-10-15:external:0.4.137");
        assert_eq!(
            QualityDay::id_of("2026-10-15", Scope::All, ALL_VERSIONS),
            "2026-10-15:all:*"
        );
        let doc = bson::to_document(&row).unwrap();
        assert_eq!(doc.get_str("scope").unwrap(), "external");
        assert!(doc.get("ttff").is_none(), "an absent part is not written");
        let back: QualityDay = bson::from_document(doc.clone()).unwrap();
        assert_eq!(back, row);
        // P9c — a row frozen before the carrier existed reads with none.
        let mut older = doc;
        older
            .get_document_mut("sessions")
            .unwrap()
            .remove("by_carrier");
        let older: QualityDay = bson::from_document(older).unwrap();
        assert!(older.sessions.unwrap().by_carrier.is_empty());
        assert_eq!(Scope::of_include_internal(true), Scope::All);
        assert_eq!(Scope::of_include_internal(false), Scope::External);
    }
}

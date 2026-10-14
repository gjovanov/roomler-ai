// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P1d — the daily digest: every closed UTC day, computed once,
//! stored once, mailed once (spec §3l).
//!
//! # The shape
//!
//! Each section answers one of the operator's questions (spec §1), and every
//! count comes as a [`Figure`] — the day, and the trailing 7 and 28 days
//! ending on it — so a quiet day is read against a baseline, not alone. A
//! section whose source is not built, not mounted or not reachable is
//! [`Measured::NotMeasured`] with a reason (and the phase that brings it),
//! and is NEVER rendered as 0: *measured*, *not measured* and *zero* are
//! three cells (spec principle 4). Every number applies the
//! [`ExclusionSet`] (spec §3c.4); the footer says how many users and orgs
//! were left out, so nothing vanishes silently.
//!
//! # Once, with two pods (AC23)
//!
//! A scheduled run first claims the day: an INSERT of
//! `growth_digests {_id: "YYYY-MM-DD", status: computing}` whose unique
//! `_id` decides which pod goes on ([`DigestStore::claim`]). A `failed` day
//! or a stale `computing` one (a pod that died mid-run, past
//! [`STALE_CLAIM_SECS`]) is re-claimed with a compare-and-set; a `sent` day
//! is never sent again. The manual route (`POST …/digest/run`) recomputes
//! and overwrites a day's sections and mails only when asked AND the day
//! was not already `sent`.
//!
//! # What it never carries
//!
//! No address, name or IP: the sign-up rows list provider and campaign key,
//! the recipients are a COUNT, org names appear (as `/observability` shows
//! them) and nothing else about a person. A test asserts no seeded address
//! survives into the stored JSON or the rendered mail.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
};

use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use chrono::{NaiveDate, Timelike};
use futures::TryStreamExt;
use roomler_ai_db::models::tenant::Plan;
use roomler_ai_services::{
    EmailService,
    dao::{
        base::DaoError,
        growth_digest::{
            ClaimMode, ClaimOutcome, DigestDoc, DigestStatus, DigestStore, ExcludedCounts,
        },
    },
};
use roomler_core::{campaign, growth::ExclusionSet};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::purestat::{Filter, PurestatClient, PurestatError, Query, Row};
use crate::{error::ApiError, state::AppState};

/// A `computing` claim older than this is a dead pod's, and is re-claimed.
pub const STALE_CLAIM_SECS: i64 = 30 * 60;
/// The scheduler's cadence; a tick that finds the day done costs one read.
pub const LOOP_INTERVAL_SECS: u64 = 10 * 60;
/// A pod that just booted waits this long before its first tick, so a
/// rolling restart does not race its own predecessor's claim.
pub const FIRST_TICK_DELAY_SECS: u64 = 2 * 60;
/// A remote-desktop session is a VALUE event once it reached Active and
/// lasted this long (spec §3d).
pub const VALUE_SESSION_SECS: i64 = 30;
/// Sign-ups listed individually; past this they are aggregated by source.
pub const MAX_LISTED_SIGNUPS: usize = 20;
pub const TOP_ORGS: usize = 10;
pub const TOP_WEB_ROWS: usize = 10;
pub const TOP_BLOG_POSTS: usize = 20;
/// Below this n a rate is not shown — counts only (spec principle 5).
pub const SMALL_N: i64 = 30;
/// A ceiling on the rows one source query loads — far above this
/// deployment's volume; a bound, not a page.
const MAX_ROWS: i64 = 200_000;

// ── Errors ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct DigestError(pub String);

impl std::fmt::Display for DigestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DigestError {}

impl From<DaoError> for DigestError {
    fn from(e: DaoError) -> Self {
        Self(e.to_string())
    }
}

impl From<mongodb::error::Error> for DigestError {
    fn from(e: mongodb::error::Error) -> Self {
        Self(e.to_string())
    }
}

impl From<bson::ser::Error> for DigestError {
    fn from(e: bson::ser::Error) -> Self {
        Self(format!("sections did not serialise: {e}"))
    }
}

impl From<DigestError> for ApiError {
    fn from(e: DigestError) -> Self {
        ApiError::Internal(format!("digest: {e}"))
    }
}

// ── Days and windows ─────────────────────────────────────────────────────

/// `YYYY-MM-DD`, nothing else.
pub fn parse_day(raw: &str) -> Result<NaiveDate, String> {
    let raw = raw.trim();
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map_err(|_| format!("day must be YYYY-MM-DD, got {raw:?}"))
}

pub fn day_str(day: NaiveDate) -> String {
    day.format("%Y-%m-%d").to_string()
}

fn midnight_ms(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
        .timestamp_millis()
}

/// The UTC calendar day an instant falls in.
pub fn utc_date_of(at: DateTime) -> NaiveDate {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(at.timestamp_millis())
        .expect("a BSON date is in range")
        .date_naive()
}

/// The day a scheduled tick at `now` owes: yesterday (UTC), once the clock
/// has passed `today 00:00 + hour`. Never an older day — there is no
/// auto-backfill; the manual route does that on purpose.
pub fn due_day(now: DateTime, hour: u8) -> Option<NaiveDate> {
    let now_utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now.timestamp_millis())?;
    if now_utc.hour() < u32::from(hour) {
        return None;
    }
    now_utc.date_naive().pred_opt()
}

/// The three windows every figure is counted over, as `[start, end)`
/// millisecond ranges; all three end at the day's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Windows {
    pub day_start: i64,
    pub day_end: i64,
    pub d7_start: i64,
    pub d28_start: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    Day,
    D7,
    D28,
}

impl Windows {
    pub fn for_day(day: NaiveDate) -> Self {
        let day_start = midnight_ms(day);
        let one = 86_400_000;
        Self {
            day_start,
            day_end: day_start + one,
            d7_start: day_start - 6 * one,
            d28_start: day_start - 27 * one,
        }
    }

    pub fn range(&self, span: Span) -> (i64, i64) {
        match span {
            Span::Day => (self.day_start, self.day_end),
            Span::D7 => (self.d7_start, self.day_end),
            Span::D28 => (self.d28_start, self.day_end),
        }
    }

    pub fn contains(&self, span: Span, t: i64) -> bool {
        let (lo, hi) = self.range(span);
        (lo..hi).contains(&t)
    }

    /// Count the instants falling in each window.
    pub fn count<I: IntoIterator<Item = i64>>(&self, times: I) -> Figure {
        let mut f = Figure::default();
        for t in times {
            if self.contains(Span::Day, t) {
                f.day += 1;
            }
            if self.contains(Span::D7, t) {
                f.d7 += 1;
            }
            if self.contains(Span::D28, t) {
                f.d28 += 1;
            }
        }
        f
    }

    /// The day's date, and the first day of each window, for purestat.
    pub fn dates(&self) -> (String, String, String) {
        let day = |ms: i64| {
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
                .expect("in range")
                .format("%Y-%m-%d")
                .to_string()
        };
        (day(self.day_start), day(self.d7_start), day(self.d28_start))
    }

    fn start_dt(&self, span: Span) -> DateTime {
        DateTime::from_millis(self.range(span).0)
    }

    fn end_dt(&self) -> DateTime {
        DateTime::from_millis(self.day_end)
    }

    /// `{$gte: window start, $lt: day end}` for a date field.
    fn between(&self, span: Span) -> Document {
        doc! { "$gte": self.start_dt(span), "$lt": self.end_dt() }
    }
}

/// Milliseconds of `[start, end)` inside `[lo, hi)`.
pub fn overlap_ms(start: i64, end: i64, lo: i64, hi: i64) -> i64 {
    (end.min(hi) - start.max(lo)).max(0)
}

fn minutes(ms: i64) -> f64 {
    (ms as f64 / 60_000.0 * 10.0).round() / 10.0
}

// ── Figures ──────────────────────────────────────────────────────────────

/// A count of the day, and of the trailing 7 and 28 days ending that day.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Figure {
    pub day: i64,
    pub d7: i64,
    pub d28: i64,
}

/// Minutes, to one decimal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct FigureF {
    pub day: f64,
    pub d7: f64,
    pub d28: f64,
}

/// Why a figure is not there, and when it will be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotMeasured {
    pub not_measured: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrives_with: Option<String>,
}

impl NotMeasured {
    /// Not built yet: the phase that brings it.
    pub fn later(reason: &str, phase: &str) -> Self {
        Self {
            not_measured: reason.to_string(),
            arrives_with: Some(phase.to_string()),
        }
    }

    /// Built, but unavailable on this server or this run.
    pub fn now(reason: impl Into<String>) -> Self {
        Self {
            not_measured: reason.into(),
            arrives_with: None,
        }
    }
}

/// A value, or the reason there is none. Serialises as the value itself or
/// as `{"not_measured": …, "arrives_with": …}` — a reader can always tell
/// the two apart, and a zero is only ever a measured zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Measured<T> {
    NotMeasured(NotMeasured),
    Value(T),
}

impl<T> Measured<T> {
    pub fn unmounted(module: &str) -> Self {
        Measured::NotMeasured(NotMeasured::now(format!("{module} module not mounted")))
    }

    pub fn value(&self) -> Option<&T> {
        match self {
            Measured::Value(v) => Some(v),
            Measured::NotMeasured(_) => None,
        }
    }

    pub fn is_measured(&self) -> bool {
        matches!(self, Measured::Value(_))
    }
}

impl<T> From<Result<T, PurestatError>> for Measured<T> {
    fn from(r: Result<T, PurestatError>) -> Self {
        match r {
            Ok(v) => Measured::Value(v),
            Err(e) => Measured::NotMeasured(NotMeasured::now(e.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCount {
    pub key: String,
    pub count: i64,
}

fn key_counts(map: BTreeMap<String, i64>) -> Vec<KeyCount> {
    let mut out: Vec<KeyCount> = map
        .into_iter()
        .map(|(key, count)| KeyCount { key, count })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
    out
}

// ── Sections ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveOrgs {
    pub day: i64,
    pub d7: i64,
    pub d28: i64,
    /// Which value events were visible: `remote`, `conference`, or both.
    pub basis: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    /// purestat's `signup` goal conversions for the day.
    pub purestat_signup_goals: i64,
    /// The server's sign-ups for the day, exclusions applied.
    pub server_signups: i64,
    /// `goals / signups`, only once the denominator reaches [`SMALL_N`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    pub n_floor: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Headline {
    pub signups: Figure,
    pub orgs_created: Figure,
    pub devices_enrolled: Measured<Figure>,
    pub active_orgs: Measured<ActiveOrgs>,
    pub coverage: Measured<Coverage>,
}

/// One sign-up of the day: the provider and the campaign key — never the
/// address, the username or the name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignupRow {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referrer_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_reported: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signups {
    pub total: Figure,
    /// Sign-ups that recorded any attribution field.
    pub attributed: Figure,
    pub by_provider: Vec<KeyCount>,
    pub by_source: Vec<KeyCount>,
    /// The day's sign-ups, one each, when there are at most
    /// [`MAX_LISTED_SIGNUPS`]; otherwise absent and `by_source` carries them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<SignupRow>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrgsDevices {
    pub orgs_created: Figure,
    pub devices_enrolled: Measured<Figure>,
    pub devices_by_os: Measured<Vec<KeyCount>>,
    /// Orgs whose FIRST device was enrolled in the window.
    pub orgs_first_device: Measured<Figure>,
    pub by_install_method: NotMeasured,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteUsage {
    /// Sessions that reached Active in the window.
    pub sessions_started: Figure,
    /// Each session's overlap with the window, summed.
    pub session_minutes: FigureF,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallUsage {
    pub calls: Figure,
    /// Participant-minutes of the calls that started in the window.
    pub participant_minutes: FigureF,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopOrg {
    pub tenant_id: String,
    /// The org's name as `/observability` shows it — client-chosen text.
    pub name: String,
    pub minutes: f64,
    pub sessions: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub remote: Measured<RemoteUsage>,
    pub calls: Measured<CallUsage>,
    pub active_orgs: Measured<ActiveOrgs>,
    /// The day's top orgs by remote-desktop minutes.
    pub top_orgs: Measured<Vec<TopOrg>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferrerRow {
    pub host: String,
    pub channel: String,
    pub visitors: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathRow {
    pub path: String,
    pub visitors: i64,
    pub pageviews: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Website {
    pub visitors: Figure,
    pub visits: Figure,
    pub pageviews: Figure,
    /// The day's visitors by organic channel (`campaign::Channel`).
    pub channels: Vec<KeyCount>,
    pub top_referrers: Vec<ReferrerRow>,
    pub top_paths: Vec<PathRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlogPost {
    pub path: String,
    pub visitors: i64,
    pub pageviews: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub median_read_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Blog {
    pub posts: Vec<BlogPost>,
    pub top_sources: Vec<KeyCount>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CampaignGoals {
    pub signup: i64,
    pub subscribe: i64,
    pub install_copy: i64,
    pub github_outbound: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CampaignRow {
    pub source: String,
    pub medium: String,
    pub campaign: String,
    pub visitors: i64,
    pub pageviews: i64,
    pub goals: CampaignGoals,
    /// Roomler's own sign-ups of the day with the same key.
    pub signups: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Campaigns {
    pub rows: Vec<CampaignRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRow {
    pub source: String,
    pub new: i64,
    pub confirmed: i64,
    pub unsubscribed: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueCountsOut {
    pub total: i64,
    pub sent: i64,
    pub failed: i64,
    pub suppressed: i64,
    pub stale: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueRow {
    pub slug: String,
    pub subject: String,
    pub sent_at: String,
    pub counts: IssueCountsOut,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Newsletter {
    pub new: Figure,
    pub confirmed: Figure,
    pub unsubscribed: Figure,
    pub by_source: Vec<SourceRow>,
    pub issues: Vec<IssueRow>,
    pub delivery: NotMeasured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStatusRow {
    pub plan: String,
    pub status: String,
    pub orgs: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MrrEstimate {
    pub cents: f64,
    /// Always true: list price × seats, never billed revenue.
    pub estimate: bool,
    pub basis: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Revenue {
    pub orgs_by_plan_status: Vec<PlanStatusRow>,
    pub mrr: MrrEstimate,
    pub billed: NotMeasured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndReasonRow {
    pub reason: String,
    /// `hangup` · `refusal` · `failure` · `other`.
    pub class: String,
    pub count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteQuality {
    pub ended: Figure,
    pub hangups: Figure,
    pub refusals: Figure,
    pub failures: Figure,
    pub by_end_reason: Vec<EndReasonRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Crashes {
    pub total: Figure,
    pub by_agent_version: Vec<KeyCount>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quality {
    pub remote: Measured<RemoteQuality>,
    pub crashes: Measured<Crashes>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sections {
    pub headline: Headline,
    pub signups: Signups,
    pub orgs_devices: OrgsDevices,
    pub usage: Usage,
    pub website: Measured<Website>,
    pub blog: Measured<Blog>,
    pub campaigns: Measured<Campaigns>,
    pub newsletter: Measured<Newsletter>,
    pub revenue: Revenue,
    pub quality: Quality,
    pub install: NotMeasured,
    pub social: NotMeasured,
    pub search: NotMeasured,
}

/// How an `end_reason` reads in the quality section: a normal hang-up, a
/// refusal at the device, or a failure of the session itself.
pub fn end_reason_class(reason: &str) -> &'static str {
    match reason {
        "controller_hangup" | "agent_hangup" | "host_disconnect" | "admin_terminated"
        | "idle_timeout" => "hangup",
        "user_denied" | "consent_timeout" | "no_prompt_surface" => "refusal",
        "agent_disconnect" | "error" => "failure",
        _ => "other",
    }
}

// ── Reading the sources ──────────────────────────────────────────────────

fn mounted(state: &AppState, id: &str) -> bool {
    state.modules.mounted().contains(&id)
}

fn excluded_ids(set: &HashSet<ObjectId>) -> Vec<ObjectId> {
    set.iter().copied().collect()
}

fn dt_ms(d: &Document, key: &str) -> Option<i64> {
    d.get_datetime(key).ok().map(|t| t.timestamp_millis())
}

fn str_of(d: &Document, key: &str) -> Option<String> {
    d.get_str(key).ok().map(str::to_string)
}

fn i64_of(d: &Document, key: &str) -> i64 {
    match d.get(key) {
        Some(Bson::Int32(v)) => i64::from(*v),
        Some(Bson::Int64(v)) => *v,
        Some(Bson::Double(v)) => *v as i64,
        _ => 0,
    }
}

async fn find_docs(
    state: &AppState,
    coll: &str,
    filter: Document,
    projection: Document,
) -> Result<Vec<Document>, DigestError> {
    let rows: Vec<Document> = state
        .db
        .collection::<Document>(coll)
        .find(filter)
        .projection(projection)
        .limit(MAX_ROWS)
        .await?
        .try_collect()
        .await?;
    Ok(rows)
}

async fn aggregate(
    state: &AppState,
    coll: &str,
    pipeline: Vec<Document>,
) -> Result<Vec<Document>, DigestError> {
    let rows: Vec<Document> = state
        .db
        .collection::<Document>(coll)
        .aggregate(pipeline)
        .await?
        .try_collect()
        .await?;
    Ok(rows)
}

/// One run's purestat reads. After a fatal error (no key, 401, timeout,
/// unreachable) no further query is sent: the reason is the same and the
/// run should not wait out three more timeouts to learn it.
struct Purestat {
    client: Option<PurestatClient>,
    fatal: Option<PurestatError>,
}

impl Purestat {
    fn from_settings(state: &AppState) -> Self {
        let g = &state.settings.growth;
        Self {
            client: PurestatClient::new(&g.purestat_url, &g.purestat_api_key),
            fatal: None,
        }
    }

    async fn rows(&mut self, q: &Query) -> Result<Vec<Row>, PurestatError> {
        if let Some(e) = &self.fatal {
            return Err(e.clone());
        }
        let Some(client) = &self.client else {
            self.fatal = Some(PurestatError::NotConfigured);
            return Err(PurestatError::NotConfigured);
        };
        match client.query(q).await {
            Ok(rows) => Ok(rows),
            Err(e) => {
                if e.is_fatal() {
                    self.fatal = Some(e.clone());
                }
                Err(e)
            }
        }
    }
}

/// The sign-up rows of the 28-day window, as read: everything the sections
/// derive from users.
struct UserRows {
    created: Vec<i64>,
    attributed: Vec<i64>,
    day_rows: Vec<SignupRow>,
}

async fn read_users(
    state: &AppState,
    w: &Windows,
    set: &ExclusionSet,
) -> Result<UserRows, DigestError> {
    let docs = find_docs(
        state,
        "users",
        doc! {
            "created_at": w.between(Span::D28),
            "deleted_at": Bson::Null,
            "_id": { "$nin": excluded_ids(&set.users) },
        },
        doc! { "created_at": 1, "signup_attribution": 1, "oauth_providers.provider": 1 },
    )
    .await?;
    let mut out = UserRows {
        created: Vec::with_capacity(docs.len()),
        attributed: Vec::new(),
        day_rows: Vec::new(),
    };
    for d in &docs {
        let Some(t) = dt_ms(d, "created_at") else {
            continue;
        };
        out.created.push(t);
        let attr = d.get_document("signup_attribution").ok();
        if attr.is_some_and(|a| !a.is_empty()) {
            out.attributed.push(t);
        }
        if w.contains(Span::Day, t) {
            let provider = d
                .get_array("oauth_providers")
                .ok()
                .and_then(|a| a.first())
                .and_then(|p| p.as_document())
                .and_then(|p| str_of(p, "provider"))
                .unwrap_or_else(|| "email".to_string());
            let field = |k: &str| attr.and_then(|a| str_of(a, k));
            out.day_rows.push(SignupRow {
                provider,
                source: field("source"),
                medium: field("medium"),
                campaign: field("campaign"),
                content: field("content"),
                landing_path: field("landing_path"),
                referrer_host: field("referrer_host"),
                self_reported: field("self_reported"),
            });
        }
    }
    Ok(out)
}

fn signups_section(w: &Windows, users: &UserRows) -> Signups {
    let mut by_provider: BTreeMap<String, i64> = BTreeMap::new();
    let mut by_source: BTreeMap<String, i64> = BTreeMap::new();
    for r in &users.day_rows {
        *by_provider.entry(r.provider.clone()).or_insert(0) += 1;
        let source = r.source.clone().unwrap_or_else(|| "(none)".to_string());
        *by_source.entry(source).or_insert(0) += 1;
    }
    let rows = (users.day_rows.len() <= MAX_LISTED_SIGNUPS).then(|| users.day_rows.clone());
    Signups {
        total: w.count(users.created.iter().copied()),
        attributed: w.count(users.attributed.iter().copied()),
        by_provider: key_counts(by_provider),
        by_source: key_counts(by_source),
        rows,
    }
}

async fn orgs_created(
    state: &AppState,
    w: &Windows,
    set: &ExclusionSet,
) -> Result<Figure, DigestError> {
    let docs = find_docs(
        state,
        "tenants",
        doc! {
            "created_at": w.between(Span::D28),
            "_id": { "$nin": excluded_ids(&set.tenants) },
        },
        doc! { "created_at": 1 },
    )
    .await?;
    Ok(w.count(docs.iter().filter_map(|d| dt_ms(d, "created_at"))))
}

struct DeviceFacts {
    enrolled: Figure,
    by_os: Vec<KeyCount>,
    first_device: Figure,
}

async fn read_devices(
    state: &AppState,
    w: &Windows,
    set: &ExclusionSet,
) -> Result<DeviceFacts, DigestError> {
    let nin = excluded_ids(&set.tenants);
    // Tombstoned rows count: a device enrolled and removed the same day was
    // still enrolled.
    let docs = find_docs(
        state,
        "agents",
        doc! { "created_at": w.between(Span::D28), "tenant_id": { "$nin": &nin } },
        doc! { "created_at": 1, "os": 1 },
    )
    .await?;
    let mut by_os: BTreeMap<String, i64> = BTreeMap::new();
    for d in &docs {
        if dt_ms(d, "created_at").is_some_and(|t| w.contains(Span::Day, t)) {
            *by_os
                .entry(str_of(d, "os").unwrap_or_else(|| "unknown".into()))
                .or_insert(0) += 1;
        }
    }
    let firsts = aggregate(
        state,
        "agents",
        vec![
            doc! { "$match": { "tenant_id": { "$nin": &nin } } },
            doc! { "$group": { "_id": "$tenant_id", "first": { "$min": "$created_at" } } },
            doc! { "$match": { "first": w.between(Span::D28) } },
        ],
    )
    .await?;
    Ok(DeviceFacts {
        enrolled: w.count(docs.iter().filter_map(|d| dt_ms(d, "created_at"))),
        by_os: key_counts(by_os),
        first_device: w.count(firsts.iter().filter_map(|d| dt_ms(d, "first"))),
    })
}

/// Per-span tenant sets with a value event.
#[derive(Default)]
struct ValueOrgs {
    day: HashSet<ObjectId>,
    d7: HashSet<ObjectId>,
    d28: HashSet<ObjectId>,
}

impl ValueOrgs {
    fn add(&mut self, w: &Windows, t: i64, tenant: ObjectId) {
        if w.contains(Span::Day, t) {
            self.day.insert(tenant);
        }
        if w.contains(Span::D7, t) {
            self.d7.insert(tenant);
        }
        if w.contains(Span::D28, t) {
            self.d28.insert(tenant);
        }
    }

    fn union(&mut self, other: &ValueOrgs) {
        self.day.extend(&other.day);
        self.d7.extend(&other.d7);
        self.d28.extend(&other.d28);
    }
}

struct RemoteFacts {
    usage: RemoteUsage,
    value: ValueOrgs,
    /// tenant → (day minutes, day sessions started)
    per_org_day: HashMap<ObjectId, (i64, i64)>,
}

async fn read_remote_usage(
    state: &AppState,
    w: &Windows,
    set: &ExclusionSet,
    now_ms: i64,
) -> Result<RemoteFacts, DigestError> {
    let docs = find_docs(
        state,
        "remote_sessions",
        doc! {
            "started_at": { "$ne": Bson::Null, "$lt": w.end_dt() },
            "$or": [ { "ended_at": Bson::Null }, { "ended_at": { "$gte": w.start_dt(Span::D28) } } ],
            "tenant_id": { "$nin": excluded_ids(&set.tenants) },
        },
        doc! { "tenant_id": 1, "started_at": 1, "ended_at": 1 },
    )
    .await?;
    let mut started = Vec::new();
    let mut ms = [0i64; 3];
    let mut value = ValueOrgs::default();
    let mut per_org_day: HashMap<ObjectId, (i64, i64)> = HashMap::new();
    for d in &docs {
        let (Some(s), Ok(tenant)) = (dt_ms(d, "started_at"), d.get_object_id("tenant_id")) else {
            continue;
        };
        let e = dt_ms(d, "ended_at").unwrap_or(now_ms).max(s);
        started.push(s);
        for (i, span) in [Span::Day, Span::D7, Span::D28].into_iter().enumerate() {
            let (lo, hi) = w.range(span);
            ms[i] += overlap_ms(s, e, lo, hi);
        }
        if e - s >= VALUE_SESSION_SECS * 1000 {
            value.add(w, s, tenant);
        }
        let (lo, hi) = w.range(Span::Day);
        let day_ms = overlap_ms(s, e, lo, hi);
        if day_ms > 0 || w.contains(Span::Day, s) {
            let entry = per_org_day.entry(tenant).or_insert((0, 0));
            entry.0 += day_ms;
            if w.contains(Span::Day, s) {
                entry.1 += 1;
            }
        }
    }
    Ok(RemoteFacts {
        usage: RemoteUsage {
            sessions_started: w.count(started),
            session_minutes: FigureF {
                day: minutes(ms[0]),
                d7: minutes(ms[1]),
                d28: minutes(ms[2]),
            },
        },
        value,
        per_org_day,
    })
}

struct CallFacts {
    usage: CallUsage,
    value: ValueOrgs,
}

async fn read_calls(
    state: &AppState,
    w: &Windows,
    set: &ExclusionSet,
) -> Result<CallFacts, DigestError> {
    let docs = find_docs(
        state,
        "call_sessions",
        doc! {
            "started_at": w.between(Span::D28),
            "tenant_id": { "$nin": excluded_ids(&set.tenants) },
        },
        doc! { "tenant_id": 1, "started_at": 1, "peak_participants": 1, "participant_seconds": 1 },
    )
    .await?;
    let mut started = Vec::new();
    let mut secs = [0i64; 3];
    let mut value = ValueOrgs::default();
    for d in &docs {
        let (Some(s), Ok(tenant)) = (dt_ms(d, "started_at"), d.get_object_id("tenant_id")) else {
            continue;
        };
        started.push(s);
        let ps = i64_of(d, "participant_seconds");
        for (i, span) in [Span::Day, Span::D7, Span::D28].into_iter().enumerate() {
            if w.contains(span, s) {
                secs[i] += ps;
            }
        }
        if i64_of(d, "peak_participants") >= 2 {
            value.add(w, s, tenant);
        }
    }
    Ok(CallFacts {
        usage: CallUsage {
            calls: w.count(started),
            participant_minutes: FigureF {
                day: minutes(secs[0] * 1000),
                d7: minutes(secs[1] * 1000),
                d28: minutes(secs[2] * 1000),
            },
        },
        value,
    })
}

async fn org_names(
    state: &AppState,
    ids: &[ObjectId],
) -> Result<HashMap<ObjectId, String>, DigestError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let docs = find_docs(
        state,
        "tenants",
        doc! { "_id": { "$in": ids } },
        doc! { "name": 1 },
    )
    .await?;
    Ok(docs
        .iter()
        .filter_map(|d| {
            Some((
                d.get_object_id("_id").ok()?,
                str_of(d, "name").unwrap_or_default(),
            ))
        })
        .collect())
}

async fn newsletter_section(state: &AppState, w: &Windows) -> Result<Newsletter, DigestError> {
    let subs = find_docs(
        state,
        "subscribers",
        doc! { "$or": [
            { "created_at": w.between(Span::D28) },
            { "confirmed_at": w.between(Span::D28) },
            { "unsubscribed_at": w.between(Span::D28) },
        ] },
        doc! { "source": 1, "created_at": 1, "confirmed_at": 1, "unsubscribed_at": 1 },
    )
    .await?;
    let mut by_source: BTreeMap<String, SourceRow> = BTreeMap::new();
    let (mut new, mut confirmed, mut unsubscribed) = (Vec::new(), Vec::new(), Vec::new());
    for d in &subs {
        let source = str_of(d, "source").unwrap_or_else(|| "(none)".into());
        let row = by_source
            .entry(source.clone())
            .or_insert_with(|| SourceRow {
                source,
                new: 0,
                confirmed: 0,
                unsubscribed: 0,
            });
        if let Some(t) = dt_ms(d, "created_at") {
            new.push(t);
            if w.contains(Span::Day, t) {
                row.new += 1;
            }
        }
        if let Some(t) = dt_ms(d, "confirmed_at") {
            confirmed.push(t);
            if w.contains(Span::Day, t) {
                row.confirmed += 1;
            }
        }
        if let Some(t) = dt_ms(d, "unsubscribed_at") {
            unsubscribed.push(t);
            if w.contains(Span::Day, t) {
                row.unsubscribed += 1;
            }
        }
    }
    let issues = find_docs(
        state,
        "newsletter_issues",
        doc! { "sent_at": w.between(Span::Day) },
        doc! { "slug": 1, "subject": 1, "sent_at": 1, "counts": 1 },
    )
    .await?;
    let issues = issues
        .iter()
        .map(|d| {
            let c = d.get_document("counts").ok();
            let n = |k: &str| c.map(|c| i64_of(c, k)).unwrap_or(0);
            IssueRow {
                slug: str_of(d, "slug").unwrap_or_default(),
                subject: str_of(d, "subject").unwrap_or_default(),
                sent_at: d
                    .get_datetime("sent_at")
                    .ok()
                    .and_then(|t| t.try_to_rfc3339_string().ok())
                    .unwrap_or_default(),
                counts: IssueCountsOut {
                    total: n("total"),
                    sent: n("sent"),
                    failed: n("failed"),
                    suppressed: n("suppressed"),
                    stale: n("stale"),
                },
            }
        })
        .collect();
    let mut by_source: Vec<SourceRow> = by_source
        .into_values()
        .filter(|r| r.new + r.confirmed + r.unsubscribed > 0)
        .collect();
    by_source.sort_by(|a, b| b.new.cmp(&a.new).then_with(|| a.source.cmp(&b.source)));
    Ok(Newsletter {
        new: w.count(new),
        confirmed: w.count(confirmed),
        unsubscribed: w.count(unsubscribed),
        by_source,
        issues,
        delivery: NotMeasured::later("delivery, bounces and complaints", "P5"),
    })
}

async fn revenue_section(state: &AppState, set: &ExclusionSet) -> Result<Revenue, DigestError> {
    let nin = excluded_ids(&set.tenants);
    let tenants = find_docs(
        state,
        "tenants",
        doc! { "deleted_at": Bson::Null, "_id": { "$nin": &nin } },
        doc! { "plan": 1, "billing.status": 1 },
    )
    .await?;
    let seats = aggregate(
        state,
        "tenant_members",
        vec![
            doc! { "$match": { "tenant_id": { "$nin": &nin } } },
            doc! { "$group": { "_id": "$tenant_id", "seats": { "$sum": 1 } } },
        ],
    )
    .await?;
    let seats: HashMap<ObjectId, i64> = seats
        .iter()
        .filter_map(|d| Some((d.get_object_id("_id").ok()?, i64_of(d, "seats"))))
        .collect();
    let mut by: BTreeMap<(String, String), i64> = BTreeMap::new();
    let mut cents = 0.0;
    for t in &tenants {
        let plan_raw = str_of(t, "plan").unwrap_or_else(|| "free".into());
        let plan: Plan = bson::from_bson(Bson::String(plan_raw.clone())).unwrap_or(Plan::Free);
        let status = t
            .get_document("billing")
            .ok()
            .and_then(|b| str_of(b, "status"))
            .unwrap_or_else(|| "none".into());
        *by.entry((plan_raw, status)).or_insert(0) += 1;
        let n = t
            .get_object_id("_id")
            .ok()
            .and_then(|id| seats.get(&id).copied())
            .unwrap_or(0);
        cents += crate::routes::cost::mrr_estimate_cents(&plan, n as f64);
    }
    Ok(Revenue {
        orgs_by_plan_status: by
            .into_iter()
            .map(|((plan, status), orgs)| PlanStatusRow { plan, status, orgs })
            .collect(),
        mrr: MrrEstimate {
            cents,
            estimate: true,
            basis: "list price × seats (plan.price_monthly_cents), not billed revenue".into(),
        },
        billed: NotMeasured::later("billed revenue from Stripe invoices", "P4"),
    })
}

async fn remote_quality(
    state: &AppState,
    w: &Windows,
    set: &ExclusionSet,
) -> Result<RemoteQuality, DigestError> {
    let docs = find_docs(
        state,
        "remote_sessions",
        doc! {
            "ended_at": w.between(Span::D28),
            "tenant_id": { "$nin": excluded_ids(&set.tenants) },
        },
        doc! { "ended_at": 1, "end_reason": 1 },
    )
    .await?;
    let mut ended = Vec::new();
    let mut by_class: HashMap<&'static str, Vec<i64>> = HashMap::new();
    let mut day_reasons: BTreeMap<String, i64> = BTreeMap::new();
    for d in &docs {
        let Some(t) = dt_ms(d, "ended_at") else {
            continue;
        };
        ended.push(t);
        let reason = str_of(d, "end_reason").unwrap_or_else(|| "unknown".into());
        by_class
            .entry(end_reason_class(&reason))
            .or_default()
            .push(t);
        if w.contains(Span::Day, t) {
            *day_reasons.entry(reason).or_insert(0) += 1;
        }
    }
    let class_figure = |c: &str| w.count(by_class.get(c).into_iter().flatten().copied());
    let mut by_end_reason: Vec<EndReasonRow> = day_reasons
        .into_iter()
        .map(|(reason, count)| EndReasonRow {
            class: end_reason_class(&reason).to_string(),
            reason,
            count,
        })
        .collect();
    by_end_reason.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));
    Ok(RemoteQuality {
        ended: w.count(ended),
        hangups: class_figure("hangup"),
        refusals: class_figure("refusal"),
        failures: class_figure("failure"),
        by_end_reason,
    })
}

async fn crashes_section(
    state: &AppState,
    w: &Windows,
    set: &ExclusionSet,
) -> Result<Crashes, DigestError> {
    let docs = find_docs(
        state,
        "agent_crashes",
        doc! {
            "reported_at": w.between(Span::D28),
            "tenant_id": { "$nin": excluded_ids(&set.tenants) },
        },
        doc! { "reported_at": 1, "agent_version": 1 },
    )
    .await?;
    let mut by_version: BTreeMap<String, i64> = BTreeMap::new();
    for d in &docs {
        if dt_ms(d, "reported_at").is_some_and(|t| w.contains(Span::Day, t)) {
            *by_version
                .entry(str_of(d, "agent_version").unwrap_or_else(|| "unknown".into()))
                .or_insert(0) += 1;
        }
    }
    Ok(Crashes {
        total: w.count(docs.iter().filter_map(|d| dt_ms(d, "reported_at"))),
        by_agent_version: key_counts(by_version),
    })
}

// ── The web plane ────────────────────────────────────────────────────────

const WEB_TOTALS: [&str; 3] = ["visitors", "visits", "pageviews"];

async fn website_section(ps: &mut Purestat, w: &Windows) -> Result<Website, PurestatError> {
    let (day, d7, d28) = w.dates();
    let mut totals = [[0i64; 3]; 3];
    for (i, from) in [day.as_str(), d7.as_str(), d28.as_str()]
        .into_iter()
        .enumerate()
    {
        let rows = ps
            .rows(&Query::range(from, &day, &[], &WEB_TOTALS, &[]))
            .await?;
        if let Some(r) = rows.first() {
            for (j, m) in WEB_TOTALS.iter().enumerate() {
                totals[j][i] = r.count(m);
            }
        }
    }
    let figure = |j: usize| Figure {
        day: totals[j][0],
        d7: totals[j][1],
        d28: totals[j][2],
    };
    let referrers = ps
        .rows(&Query::day(&day, &["referrer_host"], &["visitors"], &[]))
        .await?;
    let mut channels: BTreeMap<String, i64> = BTreeMap::new();
    let mut top_referrers: Vec<ReferrerRow> = referrers
        .iter()
        .map(|r| {
            let host = r.dim("referrer_host");
            let channel = campaign::channel_for_referrer_host(host).as_str();
            *channels.entry(channel.to_string()).or_insert(0) += r.count("visitors");
            ReferrerRow {
                host: host.unwrap_or("(direct)").to_string(),
                channel: channel.to_string(),
                visitors: r.count("visitors"),
            }
        })
        .collect();
    top_referrers.sort_by(|a, b| {
        b.visitors
            .cmp(&a.visitors)
            .then_with(|| a.host.cmp(&b.host))
    });
    top_referrers.truncate(TOP_WEB_ROWS);
    let paths = ps
        .rows(&Query::day(
            &day,
            &["path"],
            &["visitors", "pageviews"],
            &[],
        ))
        .await?;
    let mut top_paths: Vec<PathRow> = paths
        .iter()
        .map(|r| PathRow {
            path: r.dim("path").unwrap_or("(none)").to_string(),
            visitors: r.count("visitors"),
            pageviews: r.count("pageviews"),
        })
        .collect();
    top_paths.sort_by(|a, b| {
        b.visitors
            .cmp(&a.visitors)
            .then_with(|| a.path.cmp(&b.path))
    });
    top_paths.truncate(TOP_WEB_ROWS);
    Ok(Website {
        visitors: figure(0),
        visits: figure(1),
        pageviews: figure(2),
        channels: key_counts(channels),
        top_referrers,
        top_paths,
    })
}

async fn blog_section(ps: &mut Purestat, w: &Windows) -> Result<Blog, PurestatError> {
    let (day, _, _) = w.dates();
    let blog = Filter::starts_with("path", "/blog/");
    let rows = ps
        .rows(
            &Query::day(
                &day,
                &["path"],
                &["visitors", "pageviews", "reads", "median_read_s"],
                &[],
            )
            .filter(blog.clone()),
        )
        .await?;
    let mut posts: Vec<BlogPost> = rows
        .iter()
        .map(|r| BlogPost {
            path: r.dim("path").unwrap_or("(none)").to_string(),
            visitors: r.count("visitors"),
            pageviews: r.count("pageviews"),
            reads: r.metric("reads").map(|v| v.round() as i64),
            median_read_s: r.metric("median_read_s"),
        })
        .collect();
    posts.sort_by(|a, b| {
        b.visitors
            .cmp(&a.visitors)
            .then_with(|| a.path.cmp(&b.path))
    });
    posts.truncate(TOP_BLOG_POSTS);
    let sources = ps
        .rows(&Query::day(&day, &["referrer_host"], &["visitors"], &[]).filter(blog))
        .await?;
    let mut by: BTreeMap<String, i64> = BTreeMap::new();
    for r in &sources {
        *by.entry(r.dim("referrer_host").unwrap_or("(direct)").to_string())
            .or_insert(0) += r.count("visitors");
    }
    let mut top_sources = key_counts(by);
    top_sources.truncate(TOP_WEB_ROWS);
    Ok(Blog { posts, top_sources })
}

const CAMPAIGN_GOALS: [&str; 4] = ["signup", "subscribe", "install-copy", "github-outbound"];

/// The join key: the taxonomy's spelling of source and medium (purestat's
/// rows are normalised here, roomler's were at ingress), the campaign
/// trimmed and lowercased.
fn campaign_key(
    source: &str,
    medium: Option<&str>,
    campaign: Option<&str>,
) -> (String, String, String) {
    (
        campaign::normalize_source(source),
        campaign::normalize_medium(medium.unwrap_or("")),
        campaign.unwrap_or("").trim().to_lowercase(),
    )
}

async fn campaigns_section(
    ps: &mut Purestat,
    w: &Windows,
    signups: &[SignupRow],
) -> Result<Campaigns, PurestatError> {
    let (day, _, _) = w.dates();
    let rows = ps
        .rows(&Query::day(
            &day,
            &["utm_source", "utm_medium", "utm_campaign"],
            &["visitors", "pageviews"],
            &CAMPAIGN_GOALS,
        ))
        .await?;
    let mut by: BTreeMap<(String, String, String), CampaignRow> = BTreeMap::new();
    for r in &rows {
        let Some(source) = r.dim("utm_source") else {
            continue;
        };
        let key = campaign_key(source, r.dim("utm_medium"), r.dim("utm_campaign"));
        let row = by.entry(key.clone()).or_insert_with(|| CampaignRow {
            source: key.0.clone(),
            medium: key.1.clone(),
            campaign: key.2.clone(),
            visitors: 0,
            pageviews: 0,
            goals: CampaignGoals::default(),
            signups: 0,
        });
        row.visitors += r.count("visitors");
        row.pageviews += r.count("pageviews");
        row.goals.signup += r.goal("signup");
        row.goals.subscribe += r.goal("subscribe");
        row.goals.install_copy += r.goal("install-copy");
        row.goals.github_outbound += r.goal("github-outbound");
    }
    for s in signups {
        let Some(source) = s.source.as_deref() else {
            continue;
        };
        let key = campaign_key(source, s.medium.as_deref(), s.campaign.as_deref());
        by.entry(key.clone())
            .or_insert_with(|| CampaignRow {
                source: key.0.clone(),
                medium: key.1.clone(),
                campaign: key.2.clone(),
                visitors: 0,
                pageviews: 0,
                goals: CampaignGoals::default(),
                signups: 0,
            })
            .signups += 1;
    }
    let mut rows: Vec<CampaignRow> = by.into_values().collect();
    rows.sort_by(|a, b| {
        b.visitors
            .cmp(&a.visitors)
            .then_with(|| b.signups.cmp(&a.signups))
            .then_with(|| a.source.cmp(&b.source))
    });
    Ok(Campaigns { rows })
}

async fn coverage_section(
    ps: &mut Purestat,
    w: &Windows,
    server_signups: i64,
) -> Result<Coverage, PurestatError> {
    let (day, _, _) = w.dates();
    let rows = ps
        .rows(&Query::day(&day, &[], &["visitors"], &["signup"]))
        .await?;
    let goals = rows.first().map(|r| r.goal("signup")).unwrap_or(0);
    Ok(Coverage {
        purestat_signup_goals: goals,
        server_signups,
        ratio: (server_signups >= SMALL_N).then(|| goals as f64 / server_signups as f64),
        n_floor: SMALL_N,
    })
}

// ── The compute ──────────────────────────────────────────────────────────

/// Every section for `day`, with `set` applied. Pure over the database and
/// purestat; stores nothing.
pub async fn compute(
    state: &AppState,
    day: NaiveDate,
    set: &ExclusionSet,
    now: DateTime,
) -> Result<Sections, DigestError> {
    let w = Windows::for_day(day);
    let now_ms = now.timestamp_millis();

    let users = read_users(state, &w, set).await?;
    let signups = signups_section(&w, &users);
    let orgs = orgs_created(state, &w, set).await?;

    let devices = if mounted(state, "fleet") {
        Some(read_devices(state, &w, set).await?)
    } else {
        None
    };
    let remote = if mounted(state, "remote") {
        Some(read_remote_usage(state, &w, set, now_ms).await?)
    } else {
        None
    };
    let calls = if mounted(state, "conference") {
        Some(read_calls(state, &w, set).await?)
    } else {
        None
    };

    let active_orgs = match (&remote, &calls) {
        (None, None) => Measured::unmounted("remote and conference"),
        _ => {
            let mut v = ValueOrgs::default();
            let mut basis = Vec::new();
            if let Some(r) = &remote {
                v.union(&r.value);
                basis.push("remote".to_string());
            }
            if let Some(c) = &calls {
                v.union(&c.value);
                basis.push("conference".to_string());
            }
            Measured::Value(ActiveOrgs {
                day: v.day.len() as i64,
                d7: v.d7.len() as i64,
                d28: v.d28.len() as i64,
                basis,
            })
        }
    };

    let top_orgs = match &remote {
        None => Measured::unmounted("remote"),
        Some(r) => {
            let mut ranked: Vec<(ObjectId, i64, i64)> = r
                .per_org_day
                .iter()
                .map(|(t, (ms, n))| (*t, *ms, *n))
                .collect();
            ranked.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then_with(|| b.2.cmp(&a.2))
                    .then_with(|| a.0.cmp(&b.0))
            });
            ranked.truncate(TOP_ORGS);
            let ids: Vec<ObjectId> = ranked.iter().map(|r| r.0).collect();
            let names = org_names(state, &ids).await?;
            Measured::Value(
                ranked
                    .into_iter()
                    .map(|(t, ms, n)| TopOrg {
                        tenant_id: t.to_hex(),
                        name: names.get(&t).cloned().unwrap_or_default(),
                        minutes: minutes(ms),
                        sessions: n,
                    })
                    .collect(),
            )
        }
    };

    let newsletter = if mounted(state, "saas") {
        Measured::Value(newsletter_section(state, &w).await?)
    } else {
        Measured::unmounted("saas")
    };
    let revenue = revenue_section(state, set).await?;
    let quality = Quality {
        remote: match &remote {
            Some(_) => Measured::Value(remote_quality(state, &w, set).await?),
            None => Measured::unmounted("remote"),
        },
        crashes: match &devices {
            Some(_) => Measured::Value(crashes_section(state, &w, set).await?),
            None => Measured::unmounted("fleet"),
        },
    };

    // The web plane last, and each piece on its own: a purestat failure
    // costs the web sections and nothing else.
    let mut ps = Purestat::from_settings(state);
    let website: Measured<Website> = website_section(&mut ps, &w).await.into();
    let blog: Measured<Blog> = blog_section(&mut ps, &w).await.into();
    let campaigns: Measured<Campaigns> =
        campaigns_section(&mut ps, &w, &users.day_rows).await.into();
    let coverage: Measured<Coverage> = coverage_section(&mut ps, &w, signups.total.day)
        .await
        .into();

    Ok(Sections {
        headline: Headline {
            signups: signups.total,
            orgs_created: orgs,
            devices_enrolled: devices
                .as_ref()
                .map(|d| Measured::Value(d.enrolled))
                .unwrap_or_else(|| Measured::unmounted("fleet")),
            active_orgs: active_orgs.clone(),
            coverage,
        },
        signups,
        orgs_devices: OrgsDevices {
            orgs_created: orgs,
            devices_enrolled: devices
                .as_ref()
                .map(|d| Measured::Value(d.enrolled))
                .unwrap_or_else(|| Measured::unmounted("fleet")),
            devices_by_os: devices
                .as_ref()
                .map(|d| Measured::Value(d.by_os.clone()))
                .unwrap_or_else(|| Measured::unmounted("fleet")),
            orgs_first_device: devices
                .as_ref()
                .map(|d| Measured::Value(d.first_device))
                .unwrap_or_else(|| Measured::unmounted("fleet")),
            by_install_method: NotMeasured::later("devices by install method", "P3"),
        },
        usage: Usage {
            remote: remote
                .as_ref()
                .map(|r| Measured::Value(r.usage.clone()))
                .unwrap_or_else(|| Measured::unmounted("remote")),
            calls: calls
                .as_ref()
                .map(|c| Measured::Value(c.usage.clone()))
                .unwrap_or_else(|| Measured::unmounted("conference")),
            active_orgs,
            top_orgs,
        },
        website,
        blog,
        campaigns,
        newsletter,
        revenue,
        quality,
        install: NotMeasured::later("fetches by client class, tokens issued → redeemed", "P3"),
        social: NotMeasured::later("each platform's own numbers per post", "P8"),
        search: NotMeasured::later("clicks, impressions, top queries and pages", "P7"),
    })
}

// ── Delivery ─────────────────────────────────────────────────────────────

/// The one send a digest makes, behind a trait so a test can count attempts
/// without a mail backend.
pub trait DigestMailer: Send + Sync {
    fn send<'a>(
        &'a self,
        to: &'a str,
        subject: &'a str,
        html: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// The production mailer: the server's email service. It carries HTML
/// only (the service has no multipart send), so the plain text is the
/// stored twin for a reader without HTML, not an alternative part.
pub struct ServiceMailer(pub Arc<EmailService>);

impl DigestMailer for ServiceMailer {
    fn send<'a>(
        &'a self,
        to: &'a str,
        subject: &'a str,
        html: &'a str,
        _text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.0
                .send(to, subject, html)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// `Some` when the server has a mail backend configured.
pub fn service_mailer(state: &AppState) -> Option<ServiceMailer> {
    state.email.clone().map(ServiceMailer)
}

/// `digest_to` when set, else the platform admins' proven addresses
/// (`users.email` holds only those; a `.invalid` placeholder is an unproven
/// claim and is skipped).
pub async fn recipients(state: &AppState) -> Result<Vec<String>, DigestError> {
    let configured = state.settings.growth.digest_recipients();
    if !configured.is_empty() {
        return Ok(configured);
    }
    let admins: Vec<ObjectId> = state.platform_admins.iter().copied().collect();
    if admins.is_empty() {
        return Ok(Vec::new());
    }
    let docs = find_docs(
        state,
        "users",
        doc! { "_id": { "$in": admins }, "deleted_at": Bson::Null },
        doc! { "email": 1 },
    )
    .await?;
    let mut out: Vec<String> = docs
        .iter()
        .filter_map(|d| str_of(d, "email"))
        .filter(|e| e.contains('@') && !e.ends_with(".invalid"))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// What a run did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunReport {
    pub day: String,
    /// False when another pod held the day, or it was already done.
    pub claimed: bool,
    pub status: DigestStatus,
    pub attempts: u32,
    pub recipients: usize,
    pub send_attempts: usize,
    pub sent: usize,
}

/// Why a manual run did not run.
#[derive(Debug, Clone, PartialEq)]
pub enum RunError {
    /// A fresh `computing` claim: a run of this day is in progress on some
    /// pod. The route answers 409; the caller tries again later.
    Busy,
    Failed(DigestError),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("a run of this day is in progress"),
            Self::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<DigestError> for RunError {
    fn from(e: DigestError) -> Self {
        Self::Failed(e)
    }
}

impl From<DaoError> for RunError {
    fn from(e: DaoError) -> Self {
        Self::Failed(e.into())
    }
}

impl From<RunError> for ApiError {
    fn from(e: RunError) -> Self {
        match e {
            RunError::Busy => ApiError::Conflict("a run of this day is in progress".into()),
            RunError::Failed(e) => e.into(),
        }
    }
}

/// How a run holds the day: through the claim (`claimed`), or in place on
/// a terminal day whose status it must not touch (`prior` says which).
struct Hold {
    mode: ClaimMode,
    claimed: bool,
    prior: Option<DigestStatus>,
}

fn not_run(day: String, status: DigestStatus) -> RunReport {
    RunReport {
        day,
        claimed: false,
        status,
        attempts: 0,
        recipients: 0,
        send_attempts: 0,
        sent: 0,
    }
}

/// The scheduled path: claim-first, then compute, store and deliver. A
/// finished day (`sent`, `stored`) and a fresh claim are left alone.
pub async fn run_scheduled(
    state: &AppState,
    day: NaiveDate,
    mailer: Option<&dyn DigestMailer>,
    now: DateTime,
) -> Result<RunReport, DigestError> {
    let store = DigestStore::new(&state.db);
    let key = day_str(day);
    let attempts = match store
        .claim(&key, now, STALE_CLAIM_SECS, ClaimMode::Scheduled)
        .await?
    {
        ClaimOutcome::Claimed { attempts, .. } => attempts,
        ClaimOutcome::Busy => return Ok(not_run(key, DigestStatus::Computing)),
        ClaimOutcome::Done(status) | ClaimOutcome::InPlace(status) => {
            return Ok(not_run(key, status));
        }
    };
    let hold = Hold {
        mode: ClaimMode::Scheduled,
        claimed: true,
        prior: None,
    };
    finish(state, &store, day, attempts, hold, mailer, now).await
}

/// The manual path (`POST …/digest/run`): recompute and overwrite the
/// day's sections, through the SAME claim the scheduler uses.
///
/// * `send=false` — a day the scheduler still owes ends `preview`, which
///   the scheduler re-claims like `failed`: a look at a day never cancels
///   its email. A `sent` or `stored` day is refreshed in place and keeps
///   its status.
/// * `send=true` — the day is claimed as the scheduler would claim it (a
///   `stored` day was never mailed and is re-claimed too), so the two can
///   never both mail it. A `sent` day is refreshed in place and never
///   re-sent.
/// * A fresh `computing` claim refuses the run ([`RunError::Busy`], 409).
pub async fn run_manual(
    state: &AppState,
    day: NaiveDate,
    send: bool,
    mailer: Option<&dyn DigestMailer>,
    now: DateTime,
) -> Result<RunReport, RunError> {
    let store = DigestStore::new(&state.db);
    let key = day_str(day);
    let mode = if send {
        ClaimMode::ManualSend
    } else {
        ClaimMode::ManualPreview
    };
    let (attempts, hold) = match store.claim(&key, now, STALE_CLAIM_SECS, mode).await? {
        ClaimOutcome::Claimed { attempts, prior } => (
            attempts,
            Hold {
                mode,
                claimed: true,
                prior,
            },
        ),
        ClaimOutcome::Busy => return Err(RunError::Busy),
        ClaimOutcome::InPlace(status) | ClaimOutcome::Done(status) => (
            0,
            Hold {
                mode,
                claimed: false,
                prior: Some(status),
            },
        ),
    };
    Ok(finish(state, &store, day, attempts, hold, mailer, now).await?)
}

async fn finish(
    state: &AppState,
    store: &DigestStore,
    day: NaiveDate,
    attempts: u32,
    hold: Hold,
    mailer: Option<&dyn DigestMailer>,
    now: DateTime,
) -> Result<RunReport, DigestError> {
    let key = day_str(day);
    let set = ExclusionSet::load(&state.core).await?;
    let sections = match compute(state, day, &set, now).await {
        Ok(s) => s,
        Err(e) => {
            // Only a held day may be marked failed: an in-place refresh of
            // a `sent` day that fails must not hand that day back to the
            // scheduler, which would mail it again.
            if hold.claimed {
                store
                    .set_status(&key, DigestStatus::Failed, None, Some(e.to_string()))
                    .await?;
            }
            return Err(e);
        }
    };
    let recipients = recipients(state).await?;
    let (users, tenants) = set.counts();
    let excluded = ExcludedCounts {
        users: users as u32,
        tenants: tenants as u32,
    };
    store
        .store_sections(
            &key,
            bson::to_document(&sections)?,
            excluded,
            recipients.len() as u32,
            now,
        )
        .await?;

    // What this run may do now:
    //  * scheduled, or a manual send, holding the claim: deliver, then
    //    `sent` / `failed` / `stored`;
    //  * a manual preview holding the claim: `preview` — the status the
    //    scheduler re-claims, so a look at a day never cancels its mail;
    //  * in place (a `sent` day; a `stored` one for a preview): the sections
    //    are refreshed and the status is not touched.
    let deliver = hold.claimed && matches!(hold.mode, ClaimMode::Scheduled | ClaimMode::ManualSend);
    let mut report = RunReport {
        day: key.clone(),
        claimed: hold.claimed,
        status: hold.prior.unwrap_or(DigestStatus::Stored),
        attempts,
        recipients: recipients.len(),
        send_attempts: 0,
        sent: 0,
    };
    match (hold.claimed, deliver, mailer, recipients.is_empty()) {
        (false, _, _, _) => {}
        (true, false, _, _) => {
            store
                .set_status(&key, DigestStatus::Preview, None, None)
                .await?;
            report.status = DigestStatus::Preview;
        }
        (true, true, Some(mailer), false) => {
            let doc = DigestDoc {
                day: key.clone(),
                status: DigestStatus::Computing,
                version: roomler_ai_services::dao::growth_digest::DIGEST_VERSION,
                claimed_at: now,
                computed_at: Some(now),
                sent_at: None,
                attempts,
                error: None,
                recipients: recipients.len() as u32,
                excluded,
                sections: None,
            };
            let subject = super::render::subject(&key);
            let html = super::render::render_html(&doc, &sections);
            let text = super::render::render_text(&doc, &sections);
            let mut first_error: Option<String> = None;
            for to in &recipients {
                report.send_attempts += 1;
                match mailer.send(to, &subject, &html, &text).await {
                    Ok(()) => report.sent += 1,
                    Err(e) => {
                        // The error is the mailer's text about our request;
                        // it never carries the recipient.
                        warn!(day = %key, %e, "digest send failed");
                        first_error.get_or_insert(e);
                    }
                }
            }
            if report.sent > 0 {
                let note = first_error.map(|e| {
                    format!(
                        "{} of {} sends failed: {e}",
                        report.send_attempts - report.sent,
                        report.send_attempts
                    )
                });
                store
                    .set_status(&key, DigestStatus::Sent, Some(now), note)
                    .await?;
                report.status = DigestStatus::Sent;
            } else {
                let e = first_error.unwrap_or_else(|| "every send failed".into());
                store
                    .set_status(&key, DigestStatus::Failed, None, Some(e))
                    .await?;
                report.status = DigestStatus::Failed;
            }
        }
        (true, true, _, _) => {
            // Delivering, with nobody to deliver to (no recipients, or no
            // mail service): the digest exists, and the day is done.
            store
                .set_status(&key, DigestStatus::Stored, None, None)
                .await?;
            report.status = DigestStatus::Stored;
        }
    }
    info!(
        day = %key,
        status = report.status.as_str(),
        recipients = report.recipients,
        sent = report.sent,
        excluded_users = excluded.users,
        excluded_tenants = excluded.tenants,
        "growth digest run"
    );
    Ok(report)
}

/// The scheduler: every [`LOOP_INTERVAL_SECS`], once the clock has passed
/// `digest_hour_utc`, make sure yesterday's digest exists and was handled.
/// Only with `growth.digest_enabled`; the claim in `growth_digests` is what
/// keeps two pods from both sending.
pub fn spawn_digest_loop(state: AppState) {
    if !state.settings.growth.digest_enabled {
        return;
    }
    let hour = state.settings.growth.digest_hour();
    info!(hour, "growth digest scheduler starting");
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(FIRST_TICK_DELAY_SECS)).await;
        loop {
            let now = DateTime::now();
            if let Some(day) = due_day(now, hour) {
                let mailer = service_mailer(&state);
                let mailer_ref: Option<&dyn DigestMailer> =
                    mailer.as_ref().map(|m| m as &dyn DigestMailer);
                match run_scheduled(&state, day, mailer_ref, now).await {
                    Ok(r) if r.claimed => {
                        info!(day = %r.day, status = r.status.as_str(), sent = r.sent, "growth digest done")
                    }
                    Ok(_) => {}
                    Err(e) => warn!(day = %day_str(day), %e, "growth digest run failed"),
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(LOOP_INTERVAL_SECS)).await;
        }
    });
}

// ── The JSON view ────────────────────────────────────────────────────────

/// A stored digest as the routes answer it: dates as RFC 3339 strings (a
/// raw `bson::DateTime` would serialise as a truthy `{"$date": …}`), the
/// sections as plain JSON.
#[derive(Debug, Clone, Serialize)]
pub struct DigestJson {
    pub day: String,
    pub status: DigestStatus,
    pub version: u32,
    pub claimed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub computed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<String>,
    pub attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub recipients: u32,
    pub excluded: ExcludedCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sections: Option<serde_json::Value>,
}

fn rfc3339(t: DateTime) -> String {
    t.try_to_rfc3339_string().unwrap_or_default()
}

/// The stored sections, typed when they parse (every row this build wrote)
/// and as plain JSON otherwise (a row from a later shape version).
pub fn sections_json(doc: &Document) -> serde_json::Value {
    match bson::from_document::<Sections>(doc.clone()) {
        Ok(s) => serde_json::to_value(s).unwrap_or(serde_json::Value::Null),
        Err(_) => serde_json::to_value(doc).unwrap_or(serde_json::Value::Null),
    }
}

impl DigestJson {
    pub fn of(doc: &DigestDoc) -> Self {
        Self {
            day: doc.day.clone(),
            status: doc.status,
            version: doc.version,
            claimed_at: rfc3339(doc.claimed_at),
            computed_at: doc.computed_at.map(rfc3339),
            sent_at: doc.sent_at.map(rfc3339),
            attempts: doc.attempts,
            error: doc.error.clone(),
            recipients: doc.recipients,
            excluded: doc.excluded,
            sections: doc.sections.as_ref().map(sections_json),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        parse_day(s).unwrap()
    }

    #[test]
    fn days_parse_strictly() {
        assert_eq!(day_str(d("2026-10-13")), "2026-10-13");
        for bad in [
            "2026-13-01",
            "20261013",
            "2026-10-13T00:00:00Z",
            "yesterday",
            "",
        ] {
            assert!(parse_day(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_windows_end_at_the_days_end_and_count_inclusively_from_their_start() {
        let w = Windows::for_day(d("2026-10-13"));
        let one = 86_400_000;
        assert_eq!(w.day_end - w.day_start, one);
        assert_eq!(w.day_start - w.d7_start, 6 * one);
        assert_eq!(w.day_start - w.d28_start, 27 * one);
        let f = w.count([
            w.day_start,     // in all three
            w.day_end - 1,   // last ms of the day
            w.day_end,       // the next day: in none
            w.d7_start,      // first ms of the 7-day window
            w.d7_start - 1,  // the day before: 28-day only
            w.d28_start,     // first ms of the 28-day window
            w.d28_start - 1, // out
        ]);
        assert_eq!(
            f,
            Figure {
                day: 2,
                d7: 3,
                d28: 5
            }
        );
        assert_eq!(
            w.dates(),
            (
                "2026-10-13".to_string(),
                "2026-10-07".to_string(),
                "2026-09-16".to_string()
            )
        );
    }

    #[test]
    fn a_session_counts_only_its_overlap_with_the_window() {
        assert_eq!(overlap_ms(0, 100, 50, 200), 50);
        assert_eq!(overlap_ms(0, 100, 150, 200), 0);
        assert_eq!(overlap_ms(60, 80, 0, 100), 20);
        assert_eq!(overlap_ms(0, 1000, 100, 200), 100);
        assert_eq!(minutes(90_000), 1.5);
        assert_eq!(minutes(0), 0.0);
    }

    /// The scheduler owes yesterday once the hour has passed, and never an
    /// older day.
    #[test]
    fn the_due_day_is_yesterday_after_the_hour_and_nothing_before() {
        let at = |s: &str| DateTime::parse_rfc3339_str(s).unwrap();
        assert_eq!(due_day(at("2026-10-14T05:59:59Z"), 6), None);
        assert_eq!(
            due_day(at("2026-10-14T06:00:00Z"), 6),
            Some(d("2026-10-13"))
        );
        assert_eq!(
            due_day(at("2026-10-14T23:30:00Z"), 6),
            Some(d("2026-10-13"))
        );
        assert_eq!(
            due_day(at("2026-10-14T00:10:00Z"), 0),
            Some(d("2026-10-13"))
        );
        assert_eq!(utc_date_of(at("2026-10-14T23:59:59Z")), d("2026-10-14"));
    }

    /// `Measured` tells a value from its absence on the wire, and a stored
    /// section round-trips through BSON typed — the shape the GET answers.
    #[test]
    fn measured_serialises_as_the_value_or_the_reason() {
        let v: Measured<Figure> = Measured::Value(Figure {
            day: 1,
            d7: 2,
            d28: 3,
        });
        assert_eq!(
            serde_json::to_value(&v).unwrap(),
            serde_json::json!({ "day": 1, "d7": 2, "d28": 3 })
        );
        let n: Measured<Figure> = Measured::NotMeasured(NotMeasured::later("install", "P3"));
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            serde_json::json!({ "not_measured": "install", "arrives_with": "P3" })
        );
        let u: Measured<Figure> = Measured::unmounted("fleet");
        assert_eq!(
            serde_json::to_value(&u).unwrap(),
            serde_json::json!({ "not_measured": "fleet module not mounted" })
        );
        // Through BSON and back, both arms.
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Both {
            a: Measured<Figure>,
            b: Measured<Vec<KeyCount>>,
            c: Measured<Figure>,
        }
        let both = Both {
            a: v,
            b: Measured::Value(vec![KeyCount {
                key: "linux".into(),
                count: 2,
            }]),
            c: n,
        };
        let doc = bson::to_document(&both).unwrap();
        let back: Both = bson::from_document(doc).unwrap();
        assert_eq!(back, both);
        assert!(back.a.is_measured() && !back.c.is_measured());
    }

    #[test]
    fn end_reasons_classify_into_hangup_refusal_failure() {
        assert_eq!(end_reason_class("controller_hangup"), "hangup");
        assert_eq!(end_reason_class("host_disconnect"), "hangup");
        assert_eq!(end_reason_class("user_denied"), "refusal");
        assert_eq!(end_reason_class("no_prompt_surface"), "refusal");
        assert_eq!(end_reason_class("error"), "failure");
        assert_eq!(end_reason_class("agent_disconnect"), "failure");
        assert_eq!(end_reason_class("something_new"), "other");
    }

    #[test]
    fn the_campaign_join_key_is_the_taxonomys_spelling() {
        assert_eq!(
            campaign_key("YouTub", Some("Video"), Some(" 20261019-Slug ")),
            ("youtube".into(), "video".into(), "20261019-slug".into())
        );
        assert_eq!(
            campaign_key("twitter", None, None),
            ("x".into(), "".into(), "".into())
        );
    }

    #[test]
    fn the_json_view_carries_rfc3339_dates_and_no_bson() {
        let doc = DigestDoc {
            day: "2026-10-13".into(),
            status: DigestStatus::Stored,
            version: 1,
            claimed_at: DateTime::parse_rfc3339_str("2026-10-14T06:00:00Z").unwrap(),
            computed_at: Some(DateTime::parse_rfc3339_str("2026-10-14T06:00:05Z").unwrap()),
            sent_at: None,
            attempts: 1,
            error: None,
            recipients: 0,
            excluded: ExcludedCounts {
                users: 1,
                tenants: 2,
            },
            sections: Some(doc! { "headline": { "signups": { "day": 1, "d7": 2, "d28": 3 } } }),
        };
        let json = serde_json::to_value(DigestJson::of(&doc)).unwrap();
        assert_eq!(json["claimed_at"], "2026-10-14T06:00:00Z");
        assert_eq!(json["computed_at"], "2026-10-14T06:00:05Z");
        assert!(json.get("sent_at").is_none());
        assert_eq!(json["sections"]["headline"]["signups"]["d28"], 3);
        let text = json.to_string();
        assert!(
            !text.contains("$date") && !text.contains("$numberLong"),
            "{text}"
        );
    }
}

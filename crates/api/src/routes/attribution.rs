// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-88 P1a — `GET /api/admin/stats/attribution?since=&until=`: what the
//! promotion brought, as COUNTS.
//!
//! Sign-ups in a window, grouped by where they said they came from
//! (`source`, `medium`, `campaign`, `self_reported`), and how many of them
//! ACTIVATED — the person's org enrolled at least one device within
//! [`ACTIVATION_WINDOW_DAYS`] of the account's creation. Never a user list:
//! the projection the query reads has no name or address in it, and the
//! payload has no per-user row.
//!
//! It is the HOST's view, like the device listing: users are core, agents
//! are the `fleet` module's. Without `fleet` — not compiled, or switched off
//! — every `activated` is `null`, never `0`: "we cannot see devices" and "no
//! one enrolled one" are different answers and the dashboard must not
//! flatten them. Same gate as its siblings: 404 to anyone who is not a
//! platform admin.

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::{
    Json,
    extract::{Query, State},
};
use bson::{DateTime, oid::ObjectId};
use roomler_ai_services::dao::user::{MAX_SIGNUP_ROWS, SignupRow};
use serde::{Deserialize, Serialize};

use crate::{
    error::ApiError,
    extractors::auth::AuthUser,
    routes::stats::{disabled_payload, require_platform_admin},
    state::AppState,
};

/// A sign-up counts as activated when its org enrolls a device within this
/// many days of the account's `created_at` (FR-88 §3c).
pub const ACTIVATION_WINDOW_DAYS: i64 = 7;
/// The default `since` when the query names none.
const DEFAULT_WINDOW_DAYS: i64 = 30;
/// Buckets kept per dimension, most sign-ups first. A campaign list longer
/// than this is a tagging problem, not a dashboard one.
const MAX_BUCKETS: usize = 100;

#[derive(Debug, Deserialize)]
pub struct AttributionQuery {
    /// Inclusive. RFC 3339 (`2026-09-01T00:00:00Z`), a date (`2026-09-01`,
    /// midnight UTC) or unix seconds. Default: 30 days before `until`.
    #[serde(default)]
    pub since: Option<String>,
    /// Exclusive. Same forms. Default: now.
    #[serde(default)]
    pub until: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Bucket {
    /// The value sign-ups were grouped on; `null` is the sign-ups that
    /// recorded nothing for this dimension.
    pub key: Option<String>,
    pub signups: u64,
    /// `null` when the server cannot see devices (no `fleet`).
    pub activated: Option<u64>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct AttributionReport {
    pub enabled: bool,
    pub since: String,
    pub until: String,
    pub activation_window_days: i64,
    /// True when the window held more sign-ups than one query loads
    /// (`MAX_SIGNUP_ROWS`); the counts then cover the oldest rows only.
    pub truncated: bool,
    pub signups: u64,
    /// Sign-ups that recorded ANY attribution field.
    pub attributed: u64,
    pub activated: Option<u64>,
    pub by_source: Vec<Bucket>,
    pub by_medium: Vec<Bucket>,
    pub by_campaign: Vec<Bucket>,
    pub by_self_reported: Vec<Bucket>,
}

/// GET /api/admin/stats/attribution?since=&until=
pub async fn admin_attribution(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(q): Query<AttributionQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_platform_admin(&state, &auth)?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }
    let until = match q.until.as_deref() {
        Some(s) => parse_instant(s, "until")?,
        None => DateTime::now(),
    };
    let since = match q.since.as_deref() {
        Some(s) => parse_instant(s, "since")?,
        None => DateTime::from_millis(until.timestamp_millis() - DEFAULT_WINDOW_DAYS * 86_400_000),
    };
    if since >= until {
        return Err(ApiError::BadRequest("since must be before until".into()));
    }

    let rows = state.users.signups_between(since, until).await?;
    let truncated = rows.len() as i64 >= MAX_SIGNUP_ROWS;
    let user_ids: Vec<ObjectId> = rows.iter().map(|r| r.id).collect();
    let memberships = state.tenants.memberships_for_users(&user_ids).await?;
    let tenant_ids: Vec<ObjectId> = memberships
        .iter()
        .map(|(_, tid)| *tid)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let enrollments = fleet_enrollments(&state, &tenant_ids).await?;

    let report = tally(
        &rows,
        &memberships,
        enrollments.as_deref(),
        since,
        until,
        truncated,
    );
    Ok(Json(serde_json::to_value(report).map_err(|e| {
        ApiError::Internal(format!("attribution report did not serialise: {e}"))
    })?))
}

/// `(tenant_id, enrolled_at)` for every device in `tenant_ids` — the one
/// place this handler reaches past core. `Some` only when the server can
/// see devices: `modules.fleet` is `None` when the operator switched the
/// module off, and the whole field is absent when the build never linked it.
/// Both read as "cannot see", never as "none".
#[cfg(feature = "fleet")]
async fn fleet_enrollments(
    state: &AppState,
    tenant_ids: &[ObjectId],
) -> Result<Option<Vec<(ObjectId, DateTime)>>, ApiError> {
    match state.modules.fleet.as_ref() {
        Some(fleet) => Ok(Some(
            fleet.agents.enrollments_for_tenants(tenant_ids).await?,
        )),
        None => Ok(None),
    }
}

#[cfg(not(feature = "fleet"))]
async fn fleet_enrollments(
    _state: &AppState,
    _tenant_ids: &[ObjectId],
) -> Result<Option<Vec<(ObjectId, DateTime)>>, ApiError> {
    Ok(None)
}

/// RFC 3339, `YYYY-MM-DD` (midnight UTC) or unix seconds; anything else is
/// a 400 naming the parameter.
fn parse_instant(raw: &str, param: &str) -> Result<DateTime, ApiError> {
    let raw = raw.trim();
    let bad = || {
        ApiError::BadRequest(format!(
            "{param} must be RFC 3339, YYYY-MM-DD or unix seconds, got {raw:?}"
        ))
    };
    if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) {
        let secs: i64 = raw.parse().map_err(|_| bad())?;
        return secs
            .checked_mul(1000)
            .map(DateTime::from_millis)
            .ok_or_else(bad);
    }
    if raw.len() == 10 && raw.as_bytes()[4] == b'-' && raw.as_bytes()[7] == b'-' {
        return DateTime::parse_rfc3339_str(format!("{raw}T00:00:00Z")).map_err(|_| bad());
    }
    DateTime::parse_rfc3339_str(raw).map_err(|_| bad())
}

/// The join and the counting, pure so it is testable without a database.
///
/// `enrollments` is `Some` only when the server can see devices; each entry
/// is `(tenant_id, enrolled_at)`. A sign-up is activated when any org it is
/// a member of enrolled a device in
/// `[created_at, created_at + ACTIVATION_WINDOW_DAYS]`. Devices enrolled
/// BEFORE the account existed do not count — joining an org that already had
/// a fleet is not this sign-up's activation.
pub(crate) fn tally(
    rows: &[SignupRow],
    memberships: &[(ObjectId, ObjectId)],
    enrollments: Option<&[(ObjectId, DateTime)]>,
    since: DateTime,
    until: DateTime,
    truncated: bool,
) -> AttributionReport {
    let mut tenants_of: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    for (uid, tid) in memberships {
        tenants_of.entry(*uid).or_default().push(*tid);
    }
    let enrolled_at: Option<HashMap<ObjectId, Vec<i64>>> = enrollments.map(|list| {
        let mut m: HashMap<ObjectId, Vec<i64>> = HashMap::new();
        for (tid, at) in list {
            m.entry(*tid).or_default().push(at.timestamp_millis());
        }
        m
    });
    let window_ms = ACTIVATION_WINDOW_DAYS * 86_400_000;

    let activated_for = |row: &SignupRow| -> Option<bool> {
        let enrolled_at = enrolled_at.as_ref()?;
        let start = row.created_at.timestamp_millis();
        let end = start + window_ms;
        let hit = tenants_of
            .get(&row.id)
            .into_iter()
            .flatten()
            .filter_map(|tid| enrolled_at.get(tid))
            .flatten()
            .any(|at| (start..=end).contains(at));
        Some(hit)
    };

    #[derive(Default)]
    struct Tallies {
        signups: u64,
        activated: u64,
    }
    // BTreeMap so equal counts render in a stable order.
    let mut by: [BTreeMap<Option<String>, Tallies>; 4] = Default::default();
    let (mut signups, mut attributed, mut activated_total) = (0u64, 0u64, 0u64);

    for row in rows {
        signups += 1;
        let hit = activated_for(row);
        if hit == Some(true) {
            activated_total += 1;
        }
        let attr = row.signup_attribution.as_ref();
        if attr.is_some_and(|a| !a.is_empty()) {
            attributed += 1;
        }
        let keys = [
            attr.and_then(|a| a.source.clone()),
            attr.and_then(|a| a.medium.clone()),
            attr.and_then(|a| a.campaign.clone()),
            attr.and_then(|a| a.self_reported.clone()),
        ];
        for (dim, key) in keys.into_iter().enumerate() {
            let t = by[dim].entry(key).or_default();
            t.signups += 1;
            if hit == Some(true) {
                t.activated += 1;
            }
        }
    }

    let can_see = enrolled_at.is_some();
    let render = |m: BTreeMap<Option<String>, Tallies>| -> Vec<Bucket> {
        let mut out: Vec<Bucket> = m
            .into_iter()
            .map(|(key, t)| Bucket {
                key,
                signups: t.signups,
                activated: can_see.then_some(t.activated),
            })
            .collect();
        out.sort_by(|a, b| b.signups.cmp(&a.signups).then_with(|| a.key.cmp(&b.key)));
        out.truncate(MAX_BUCKETS);
        out
    };
    let [source, medium, campaign, self_reported] = by;

    AttributionReport {
        enabled: true,
        since: since.try_to_rfc3339_string().unwrap_or_default(),
        until: until.try_to_rfc3339_string().unwrap_or_default(),
        activation_window_days: ACTIVATION_WINDOW_DAYS,
        truncated,
        signups,
        attributed,
        activated: can_see.then_some(activated_total),
        by_source: render(source),
        by_medium: render(medium),
        by_campaign: render(campaign),
        by_self_reported: render(self_reported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_ai_db::models::SignupAttribution;

    const DAY: i64 = 86_400_000;

    fn row(
        id: ObjectId,
        created_ms: i64,
        source: Option<&str>,
        campaign: Option<&str>,
    ) -> SignupRow {
        SignupRow {
            id,
            created_at: DateTime::from_millis(created_ms),
            signup_attribution: source.or(campaign).map(|_| SignupAttribution {
                source: source.map(str::to_string),
                campaign: campaign.map(str::to_string),
                ..Default::default()
            }),
        }
    }

    fn bucket<'a>(buckets: &'a [Bucket], key: Option<&str>) -> &'a Bucket {
        buckets
            .iter()
            .find(|b| b.key.as_deref() == key)
            .unwrap_or_else(|| panic!("no bucket {key:?} in {buckets:?}"))
    }

    /// Three sign-ups, two orgs, one device enrolled in time, one too late,
    /// one before the account existed.
    #[test]
    fn activation_is_a_device_within_seven_days_of_the_account() {
        let (a, b, c) = (ObjectId::new(), ObjectId::new(), ObjectId::new());
        let (org_a, org_c) = (ObjectId::new(), ObjectId::new());
        let t0 = 1_700_000_000_000;
        let rows = vec![
            row(a, t0, Some("youtube"), Some("c1")),
            row(b, t0 + DAY, Some("youtube"), None),
            row(c, t0 + 2 * DAY, None, None),
        ];
        let memberships = vec![(a, org_a), (c, org_c)];
        let enrollments = vec![
            // a's org: one device on day 3 — inside the window.
            (org_a, DateTime::from_millis(t0 + 3 * DAY)),
            // c's org: a device from BEFORE c signed up, and one on day 8 —
            // neither counts.
            (org_c, DateTime::from_millis(t0)),
            (org_c, DateTime::from_millis(t0 + 2 * DAY + 8 * DAY)),
        ];
        let r = tally(
            &rows,
            &memberships,
            Some(&enrollments),
            DateTime::from_millis(t0),
            DateTime::from_millis(t0 + 10 * DAY),
            false,
        );
        assert_eq!(r.signups, 3);
        assert_eq!(r.attributed, 2);
        assert_eq!(r.activated, Some(1));
        let yt = bucket(&r.by_source, Some("youtube"));
        assert_eq!((yt.signups, yt.activated), (2, Some(1)));
        let none = bucket(&r.by_source, None);
        assert_eq!((none.signups, none.activated), (1, Some(0)));
        let c1 = bucket(&r.by_campaign, Some("c1"));
        assert_eq!((c1.signups, c1.activated), (1, Some(1)));
        assert_eq!(bucket(&r.by_campaign, None).signups, 2);
        // Most sign-ups first.
        assert_eq!(r.by_source[0].key.as_deref(), Some("youtube"));
        assert!(!r.truncated);
    }

    /// The boundary is inclusive at both ends of the window.
    #[test]
    fn the_window_edges_are_inclusive() {
        let a = ObjectId::new();
        let org = ObjectId::new();
        let t0 = 1_700_000_000_000;
        let rows = vec![row(a, t0, Some("x"), None)];
        let at_edge = vec![(org, DateTime::from_millis(t0 + 7 * DAY))];
        let r = tally(
            &rows,
            &[(a, org)],
            Some(&at_edge),
            DateTime::from_millis(t0),
            DateTime::from_millis(t0 + DAY),
            false,
        );
        assert_eq!(r.activated, Some(1));
        let past_edge = vec![(org, DateTime::from_millis(t0 + 7 * DAY + 1))];
        let r = tally(
            &rows,
            &[(a, org)],
            Some(&past_edge),
            DateTime::from_millis(t0),
            DateTime::from_millis(t0 + DAY),
            false,
        );
        assert_eq!(r.activated, Some(0));
    }

    /// Without `fleet` the answer is "cannot see", never "nobody did".
    #[test]
    fn without_fleet_every_activated_is_null_never_zero() {
        let a = ObjectId::new();
        let rows = vec![row(a, 1_700_000_000_000, Some("tiktok"), None)];
        let r = tally(
            &rows,
            &[],
            None,
            DateTime::from_millis(0),
            DateTime::from_millis(1),
            false,
        );
        assert_eq!(r.signups, 1);
        assert_eq!(r.activated, None);
        assert_eq!(bucket(&r.by_source, Some("tiktok")).activated, None);
        assert_eq!(bucket(&r.by_medium, None).activated, None);
        // ...and WITH fleet but no devices at all it is a real zero.
        let r = tally(
            &rows,
            &[],
            Some(&[]),
            DateTime::from_millis(0),
            DateTime::from_millis(1),
            false,
        );
        assert_eq!(r.activated, Some(0));
        assert_eq!(bucket(&r.by_source, Some("tiktok")).activated, Some(0));
    }

    #[test]
    fn the_report_never_carries_a_user() {
        let a = ObjectId::new();
        let rows = vec![row(a, 1_700_000_000_000, Some("youtube"), Some("c1"))];
        let r = tally(
            &rows,
            &[],
            Some(&[]),
            DateTime::from_millis(0),
            DateTime::from_millis(1),
            false,
        );
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains(&a.to_hex()), "a user id leaked: {json}");
        for key in ["email", "username", "user_id", "users"] {
            assert!(!json.contains(key), "{key} in {json}");
        }
    }

    #[test]
    fn instants_parse_in_three_forms_and_nothing_else() {
        assert_eq!(
            parse_instant("2026-09-01", "since")
                .unwrap()
                .timestamp_millis(),
            DateTime::parse_rfc3339_str("2026-09-01T00:00:00Z")
                .unwrap()
                .timestamp_millis()
        );
        assert_eq!(
            parse_instant(" 2026-09-01T12:30:00Z ", "since")
                .unwrap()
                .timestamp_millis(),
            DateTime::parse_rfc3339_str("2026-09-01T12:30:00Z")
                .unwrap()
                .timestamp_millis()
        );
        assert_eq!(
            parse_instant("1700000000", "since")
                .unwrap()
                .timestamp_millis(),
            1_700_000_000_000
        );
        for bad in [
            "",
            "yesterday",
            "2026-13-01",
            "2026-09-01T",
            "-5",
            "99999999999999999999",
        ] {
            let err = parse_instant(bad, "until").unwrap_err();
            assert!(
                matches!(err, ApiError::BadRequest(ref m) if m.contains("until")),
                "{bad:?}: {err}"
            );
        }
    }
}

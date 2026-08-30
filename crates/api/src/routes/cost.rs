// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-20 P5 — what the metered resources cost, per org.
//!
//! Reads the `stats_usage*` ledger built in P1/P2/P4, prices it from
//! `config/relay-costs.toml`, and hands `/observability` the per-org cost and
//! margin inputs. It computes nothing it has not measured.
//!
//! # Three things this module refuses to fabricate
//!
//! 1. **A cost with no price.** Every unit cost is `Option`; unset yields
//!    `null`, which the UI renders as *not priced*. A defaulted 0.00 would
//!    read as "this org is free to serve" and imply 100 % margin.
//! 2. **A zero for an uncollected meter.** `turn_bytes` has no writer at all
//!    (FR-20 P3 is blocked on coturn emitting no `user` label), so it reports
//!    `monitored: false`. *No traffic* and *not measured* are different facts
//!    and must never share a cell.
//! 3. **A relayed fraction with no reporters.** An empty denominator yields
//!    `null`, not `0`, which would otherwise read as a flawless mesh.
//!
//! # `mrr_cents` is a list-price ESTIMATE, not billed revenue
//!
//! What a customer is actually charged lives in Stripe; `BillingInfo` stores
//! only ids and a status, no amount. So MRR here is
//! `Plan::price_monthly_cents` x seats, and it ignores discounts, annual
//! terms, trials and proration. `subscription_status` rides along precisely so
//! a reader can see that a `canceled` or `trialing` org's MRR is notional.
//! Margin inherits every one of those caveats.

use axum::{
    Json,
    extract::{Query, State},
};
use bson::{Bson, doc};
use std::collections::HashMap;

use super::stats::{
    RangeQuery, agg, disabled_payload, floor_dt, range_spec, require_platform_admin, tier_coll,
};
use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler_ai_config::settings::RelayCosts;
use roomler_ai_db::models::tenant::Plan;

/// Meters this deployment actually collects. A meter absent from this list is
/// reported `monitored: false` rather than `0` — see the module docs.
///
/// `turn_bytes` is deliberately NOT here: the meter exists in the `Meter` enum
/// so the ledger shape is complete, but nothing writes it.
const MONITORED: &[&str] = &["derp_bytes", "sfu_participant_seconds"];

/// Cost of `total` units of `meter`, or `None` when that meter has no price
/// configured. The conversion to a billable unit lives here so the ledger can
/// keep storing raw, additive counts.
fn cost_of(meter: &str, total: f64, c: &RelayCosts) -> Option<f64> {
    match meter {
        "derp_bytes" => c.derp_gb.map(|p| total / 1e9 * p),
        "turn_bytes" => c.turn_gb.map(|p| total / 1e9 * p),
        "sfu_participant_seconds" => c.sfu_participant_hour.map(|p| total / 3600.0 * p),
        _ => None,
    }
}

fn f64_of(v: &serde_json::Value, k: &str) -> f64 {
    v.get(k).and_then(serde_json::Value::as_f64).unwrap_or(0.0)
}

fn str_of(v: &serde_json::Value, k: &str) -> String {
    v.get(k)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `GET /api/admin/stats/cost?range=24h|7d|30d|1y` — per-org metered cost,
/// list-price MRR, and the fleet carrier mix.
pub async fn admin_cost(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(q): Query<RangeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_platform_admin(&state, &auth)?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }
    let (window, tier) = range_spec(q.range.as_deref())?;
    let costs = &state.settings.relay_costs;

    // ── the ledger, summed per (tenant, meter) over the range ──────────────
    let usage = agg(
        &state,
        &tier_coll("stats_usage", tier),
        vec![
            doc! { "$match": { "ts": { "$gte": floor_dt(window) } } },
            doc! { "$group": {
                "_id": { "t": "$tenant_id", "m": "$meter" },
                "total": { "$sum": "$value" },
            }},
            doc! { "$set": { "tenant_id": { "$toString": "$_id.t" }, "meter": "$_id.m" } },
            doc! { "$unset": "_id" },
        ],
    )
    .await?;

    let tenants = agg(
        &state,
        "tenants",
        vec![
            doc! { "$match": { "deleted_at": Bson::Null } },
            doc! { "$set": { "id": { "$toString": "$_id" } } },
            doc! { "$project": {
                "_id": 0, "id": 1, "name": 1, "slug": 1, "plan": 1,
                "subscription_status": "$billing.status",
            }},
            doc! { "$limit": 500 },
        ],
    )
    .await?;
    let members = agg(
        &state,
        "tenant_members",
        vec![
            doc! { "$group": { "_id": "$tenant_id", "seats": { "$sum": 1 } } },
            doc! { "$set": { "tenant_id": { "$toString": "$_id" } } },
            doc! { "$unset": "_id" },
        ],
    )
    .await?;

    // ── assemble per org ──────────────────────────────────────────────────
    let mut seats: HashMap<String, f64> = HashMap::new();
    for m in &members {
        seats.insert(str_of(m, "tenant_id"), f64_of(m, "seats"));
    }
    let mut per_org: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut fleet: HashMap<String, f64> = HashMap::new();
    for u in &usage {
        let meter = str_of(u, "meter");
        let total = f64_of(u, "total");
        *per_org
            .entry(str_of(u, "tenant_id"))
            .or_default()
            .entry(meter.clone())
            .or_insert(0.0) += total;
        *fleet.entry(meter).or_insert(0.0) += total;
    }

    let mut orgs: Vec<serde_json::Value> = Vec::new();
    for t in &tenants {
        let id = str_of(t, "id");
        let mine = per_org.get(&id);
        // An org with no ledger rows is included with explicit zeros: for a
        // MONITORED meter, zero is a measurement ("relayed nothing"), which is
        // exactly the fact an operator wants to see.
        let mut meters = serde_json::Map::new();
        let mut cost_total = 0.0;
        let mut any_priced = false;
        for m in MONITORED {
            let total = mine.and_then(|h| h.get(*m)).copied().unwrap_or(0.0);
            let cost = cost_of(m, total, costs);
            if let Some(c) = cost {
                cost_total += c;
                any_priced = true;
            }
            meters.insert(
                (*m).to_string(),
                serde_json::json!({ "total": total, "cost": cost }),
            );
        }
        let plan: Plan =
            serde_json::from_value(t.get("plan").cloned().unwrap_or(serde_json::Value::Null))
                .unwrap_or(Plan::Free);
        let seats_n = seats.get(&id).copied().unwrap_or(0.0);
        let mrr_cents = plan.price_monthly_cents() as f64 * seats_n;
        // Cost is measured over `range` while MRR is monthly, so subtracting
        // them here would be wrong for every range but 30d. The margin is left
        // to the caller, which knows what it asked for — and `window_secs`
        // travels in the payload so it can normalise.
        orgs.push(serde_json::json!({
            "tenant_id": id,
            "name": t.get("name").cloned().unwrap_or(serde_json::Value::Null),
            "slug": t.get("slug").cloned().unwrap_or(serde_json::Value::Null),
            "plan": t.get("plan").cloned().unwrap_or(serde_json::Value::Null),
            "subscription_status": t.get("subscription_status").cloned()
                .unwrap_or(serde_json::Value::Null),
            "seats": seats_n,
            "mrr_cents": mrr_cents,
            "meters": meters,
            // `null`, not 0, when nothing this org used carries a price.
            "cost": if any_priced { Some(cost_total) } else { None },
        }));
    }

    // ── fleet carrier mix: the NAT-traversal alarm ────────────────────────
    //
    // Deliberately NOT a byte fraction. Direct bytes are measured nowhere —
    // that is the property the whole FR rests on — so a byte-level "relayed
    // fraction" is not computable, and inventing one would be exactly the
    // class of confident-looking wrong number this design exists to avoid.
    // This counts CONNECTIONS by carrier, which agents already report, and it
    // reads the raw tier over the last hour because it is a *now* signal.
    //
    // ⚠ Agent-reported, so it is a claim by the fleet, not a server
    // measurement. Good enough to raise an alarm; it must never price a bill.
    let mix = agg(
        &state,
        "stats_machine",
        vec![
            doc! { "$match": { "ts": { "$gte": floor_dt(3_600) } } },
            doc! { "$group": {
                "_id": Bson::Null,
                "direct": { "$sum": "$sys.transports.direct" },
                "relay":  { "$sum": "$sys.transports.relay" },
                "derp":   { "$sum": "$sys.transports.derp" },
            }},
            doc! { "$unset": "_id" },
        ],
    )
    .await?;
    let carrier_mix = mix.first().map(|m| {
        let (d, r, p) = (f64_of(m, "direct"), f64_of(m, "relay"), f64_of(m, "derp"));
        let denom = d + r + p;
        serde_json::json!({
            "direct": d, "relay": r, "derp": p,
            // No reporters ⇒ `null`. A 0.0 here would read as a flawless mesh.
            "relayed_fraction": if denom > 0.0 { Some((r + p) / denom) } else { None },
            "basis": "connections",
            "window": "1h",
        })
    });

    let fleet_meters: serde_json::Map<String, serde_json::Value> = MONITORED
        .iter()
        .map(|m| {
            let total = fleet.get(*m).copied().unwrap_or(0.0);
            (
                (*m).to_string(),
                serde_json::json!({
                    "total": total,
                    "cost": cost_of(m, total, costs),
                    "monitored": true,
                }),
            )
        })
        // Everything the ledger CAN carry but this deployment does not
        // collect, reported honestly rather than omitted or zeroed.
        .chain(std::iter::once((
            "turn_bytes".to_string(),
            serde_json::json!({
                "total": serde_json::Value::Null,
                "cost": serde_json::Value::Null,
                "monitored": false,
                "why": "coturn 4.17.2 emits no user label, so relayed bytes \
                        cannot be attributed to a tenant (FR-20 P3)",
            }),
        )))
        .collect();

    Ok(Json(serde_json::json!({
        "enabled": true,
        "range": q.range.unwrap_or_else(|| "24h".into()),
        "window_secs": window,
        "currency": costs.currency,
        "priced": costs.derp_gb.is_some() || costs.sfu_participant_hour.is_some(),
        "unit_costs": {
            "derp_gb": costs.derp_gb,
            "turn_gb": costs.turn_gb,
            "sfu_participant_hour": costs.sfu_participant_hour,
        },
        "meters": fleet_meters,
        "orgs": orgs,
        "carrier_mix": carrier_mix,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced() -> RelayCosts {
        RelayCosts {
            currency: Some("EUR".into()),
            derp_gb: Some(0.010),
            turn_gb: Some(0.010),
            sfu_participant_hour: Some(0.0040),
        }
    }

    /// The whole point of `Option`: an unpriced meter must not become 0.00.
    /// A zero cost renders as "free to serve" and implies 100 % margin, which
    /// is a worse error than a visibly missing number.
    #[test]
    fn an_unpriced_meter_yields_none_not_zero() {
        let empty = RelayCosts::default();
        assert_eq!(cost_of("derp_bytes", 5e9, &empty), None);
        assert_eq!(cost_of("sfu_participant_seconds", 7200.0, &empty), None);
        // ...and with a price it is a real number, so the `None`s above are a
        // property of the config rather than of the function.
        assert!(cost_of("derp_bytes", 5e9, &priced()).is_some());
    }

    /// Guards the unit conversions, the easiest thing here to get wrong by a
    /// factor of 1000 (GB vs GiB) or 60 (minutes vs hours).
    #[test]
    fn units_convert_at_the_documented_scale() {
        // 5 GB at 0.010/GB = 0.05
        let c = cost_of("derp_bytes", 5e9, &priced()).unwrap();
        assert!((c - 0.05).abs() < 1e-9, "{c}");
        // 2 participant-hours at 0.0040/h = 0.008
        let c = cost_of("sfu_participant_seconds", 7200.0, &priced()).unwrap();
        assert!((c - 0.008).abs() < 1e-9, "{c}");
    }

    /// `turn_bytes` must never appear as monitored: it has a price and an enum
    /// variant but no writer, and reporting `0` would assert that coturn
    /// relayed nothing when the truth is that nobody counted.
    #[test]
    fn turn_bytes_is_not_a_monitored_meter() {
        assert!(!MONITORED.contains(&"turn_bytes"));
        assert!(MONITORED.contains(&"derp_bytes"));
        assert!(MONITORED.contains(&"sfu_participant_seconds"));
    }

    /// An unknown meter is unpriced rather than silently free — if a meter is
    /// added to the ledger and not to `cost_of`, the surface must say so.
    #[test]
    fn an_unknown_meter_is_never_priced() {
        assert_eq!(cost_of("something_new", 1e12, &priced()), None);
    }
}

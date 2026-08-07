//! Stats PR-3 — observability query APIs.
//!
//! Two families:
//!
//! - `/api/admin/stats/*` — platform-operator dashboards (relay fleet,
//!   cross-org). Gated by the `platform_admins` ObjectId allowlist and
//!   answering **404** (never 403) on missing authority: the web client
//!   force-logs-out on any 403, and a hidden surface beats an
//!   acknowledged one.
//! - `/api/tenant/{tid}/stats/*` — org dashboards. `overview` is
//!   member-visible (powers the dashboard Insights panel); the queryable
//!   series (`machines`/`calls`/`tunnels`) require `MANAGE_AGENTS` (the
//!   fleet bit seeded Admin roles hold) — permission failures are also
//!   404 for the same logout reason.
//!
//! Series contract: every pipeline projects `t` (unix SECONDS) plus plain
//! numbers/strings only — no BSON dates or ObjectIds leak into the JSON
//! (bson's serde would render them as `{"$date":…}`/`{"$oid":…}`).
//! Range → source tier: `24h` = raw grouped to 5 min, `7d`/`30d` = `_1h`,
//! `1y` = `_1d`.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use serde::Deserialize;
use std::collections::HashMap;

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};
use roomler_ai_db::models::role::permissions;

// ── Guards ──────────────────────────────────────────────────────────────

/// Platform-operator gate. 404 by design (see module docs).
fn require_platform_admin(state: &AppState, auth: &AuthUser) -> Result<(), ApiError> {
    if state.platform_admins.contains(&auth.user_id) {
        Ok(())
    } else {
        Err(ApiError::NotFound("Not found".to_string()))
    }
}

/// Tenant-scope gate: membership (+ optionally MANAGE_AGENTS). Failures
/// are 404, not 403 — the web client wipes tokens on 403, and a member
/// removed from the org mid-poll must not be logged out of everything.
async fn require_tenant_stats(
    state: &AppState,
    tenant_id: ObjectId,
    user_id: ObjectId,
    need_manage: bool,
) -> Result<(), ApiError> {
    let perms = state
        .tenants
        .get_member_permissions(tenant_id, user_id)
        .await
        .map_err(|_| ApiError::NotFound("Not found".to_string()))?;
    if need_manage && !permissions::has(perms, permissions::MANAGE_AGENTS) {
        return Err(ApiError::NotFound("Not found".to_string()));
    }
    Ok(())
}

fn parse_tid(tenant_id: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(tenant_id).map_err(|_| ApiError::BadRequest("Invalid tenant_id".into()))
}

// ── Range plumbing ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RangeQuery {
    #[serde(default)]
    pub range: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub tenant_id: Option<String>,
}

/// (window secs, tier). Tier picks the source collection suffix.
fn range_spec(range: Option<&str>) -> Result<(i64, Tier), ApiError> {
    match range.unwrap_or("24h") {
        "24h" => Ok((86_400, Tier::Raw)),
        "7d" => Ok((7 * 86_400, Tier::Hour)),
        "30d" => Ok((30 * 86_400, Tier::Hour)),
        "1y" => Ok((365 * 86_400, Tier::Day)),
        other => Err(ApiError::BadRequest(format!(
            "invalid range '{other}' (24h|7d|30d|1y)"
        ))),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tier {
    Raw,
    Hour,
    Day,
}

fn floor_dt(window_secs: i64) -> DateTime {
    DateTime::from_millis(DateTime::now().timestamp_millis() - window_secs * 1000)
}

/// Group key for the raw tier: 5-minute bins; rollup tiers are already
/// bucketed, group by their own ts.
fn bucket_expr(tier: Tier) -> Document {
    match tier {
        Tier::Raw => doc! { "$dateTrunc": { "date": "$ts", "unit": "minute", "binSize": 5 } },
        Tier::Hour | Tier::Day => doc! { "$dateTrunc": { "date": "$ts", "unit": "hour" } },
    }
}

/// `_id` (a date) → `t` unix seconds.
fn t_secs() -> Document {
    doc! { "$toLong": { "$divide": [ { "$toLong": "$_id" }, 1000 ] } }
}

async fn agg(
    state: &AppState,
    coll: &str,
    pipeline: Vec<Document>,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let mut cur = state
        .db
        .collection::<Document>(coll)
        .aggregate(pipeline)
        .await
        .map_err(|e| ApiError::Internal(format!("stats query failed: {e}")))?;
    let mut out = Vec::new();
    while let Some(d) = cur
        .try_next()
        .await
        .map_err(|e| ApiError::Internal(format!("stats cursor failed: {e}")))?
    {
        out.push(serde_json::to_value(&d).unwrap_or(serde_json::Value::Null));
    }
    Ok(out)
}

fn disabled_payload() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "enabled": false }))
}

// ── Series pipelines ────────────────────────────────────────────────────

fn machine_series_pipeline(tenant: ObjectId, floor: DateTime, tier: Tier) -> Vec<Document> {
    let group = match tier {
        Tier::Raw => doc! {
            "_id": bucket_expr(tier),
            "online_set": { "$addToSet": "$agent_id" },
            "active_sessions": { "$avg": "$active_sessions" },
            "cpu_pct": { "$avg": "$sys.cpu_pct" },
            "rss_mb": { "$avg": "$sys.rss_mb" },
            "peer_rtt_ms": { "$avg": "$sys.peer_rtt_ms" },
            "direct": { "$avg": "$sys.transports.direct" },
            "relay": { "$avg": "$sys.transports.relay" },
            "derp": { "$avg": "$sys.transports.derp" },
            "tunnel_flows": { "$avg": "$sys.tunnel_flows" },
            "rc_sessions": { "$avg": "$sys.rc_sessions" },
        },
        _ => doc! {
            "_id": "$ts",
            "online": { "$sum": { "$cond": [ { "$gt": [ "$online_minutes", 0 ] }, 1, 0 ] } },
            "online_minutes": { "$sum": "$online_minutes" },
            "active_sessions": { "$avg": "$active_sessions_max" },
            "cpu_pct": { "$avg": "$cpu_pct" },
            "rss_mb": { "$avg": "$rss_mb" },
            "peer_rtt_ms": { "$avg": "$peer_rtt_ms" },
            "direct": { "$avg": "$direct" },
            "relay": { "$avg": "$relay" },
            "derp": { "$avg": "$derp" },
            "tunnel_flows": { "$avg": "$tunnel_flows" },
            "rc_sessions": { "$avg": "$rc_sessions" },
        },
    };
    let mut set = doc! { "t": t_secs() };
    if tier == Tier::Raw {
        set.insert("online", doc! { "$size": "$online_set" });
    }
    vec![
        doc! { "$match": { "tenant_id": tenant, "ts": { "$gte": floor } } },
        doc! { "$group": group },
        doc! { "$set": set },
        doc! { "$unset": [ "_id", "online_set" ] },
        doc! { "$sort": { "t": 1 } },
    ]
}

fn call_series_pipeline(tenant: Option<ObjectId>, floor: DateTime, tier: Tier) -> Vec<Document> {
    let mut match_doc = doc! { "ts": { "$gte": floor } };
    if let Some(t) = tenant {
        match_doc.insert("tenant_id", t);
    }
    let group = match tier {
        Tier::Raw => doc! {
            "_id": bucket_expr(tier),
            "participant_seconds": { "$sum": { "$multiply": [ "$participants", 30 ] } },
            "relayed_seconds": { "$sum": { "$multiply": [ "$relayed", 30 ] } },
            "direct_seconds": { "$sum": { "$multiply": [ "$direct", 30 ] } },
            "call_seconds": { "$sum": 30 },
            "peak_participants": { "$max": "$participants" },
            "send_bps": { "$avg": "$send_bps" },
            "recv_bps": { "$avg": "$recv_bps" },
            "loss_pct": { "$avg": "$loss_pct" },
            "rooms": { "$addToSet": "$room_id" },
        },
        _ => doc! {
            "_id": "$ts",
            "participant_seconds": { "$sum": "$participant_seconds" },
            "relayed_seconds": { "$sum": "$relayed_seconds" },
            "direct_seconds": { "$sum": "$direct_seconds" },
            "call_seconds": { "$sum": "$call_seconds" },
            "peak_participants": { "$max": "$peak_participants" },
            "send_bps": { "$avg": "$send_bps" },
            "recv_bps": { "$avg": "$recv_bps" },
            "loss_pct": { "$avg": "$loss_pct" },
        },
    };
    let mut set = doc! { "t": t_secs() };
    if tier == Tier::Raw {
        set.insert("distinct_rooms", doc! { "$size": "$rooms" });
    }
    vec![
        doc! { "$match": match_doc },
        doc! { "$group": group },
        doc! { "$set": set },
        doc! { "$unset": [ "_id", "rooms" ] },
        doc! { "$sort": { "t": 1 } },
    ]
}

fn relay_series_pipeline(region: &str, floor: DateTime, tier: Tier) -> Vec<Document> {
    let group = match tier {
        Tier::Raw => doc! {
            "_id": bucket_expr(tier),
            "sample_count": { "$sum": 1 },
            "healthy_count": { "$sum": { "$cond": [ "$healthy", 1, 0 ] } },
            "poll_rtt_ms": { "$avg": "$poll_rtt_ms" },
            "load1": { "$avg": "$load1" },
            "cpus": { "$max": "$cpus" },
            "mem_available_pct": { "$avg": { "$cond": [
                { "$gt": [ "$mem_total_kb", 0 ] },
                { "$divide": [ "$mem_available_kb", "$mem_total_kb" ] },
                Bson::Null,
            ] } },
            "rx_mbps": { "$avg": "$rx_mbps" },
            "tx_mbps": { "$avg": "$tx_mbps" },
            "tx_mbps_max": { "$max": "$tx_mbps" },
            "allocations": { "$avg": "$allocations" },
            "coturn_sessions": { "$avg": "$coturn_sessions" },
            "derp_registrations": { "$avg": "$derp_registrations" },
        },
        _ => doc! {
            "_id": "$ts",
            "sample_count": { "$sum": "$sample_count" },
            "healthy_count": { "$sum": "$healthy_count" },
            "poll_rtt_ms": { "$avg": "$poll_rtt_ms" },
            "load1": { "$avg": "$load1" },
            "cpus": { "$max": "$cpus" },
            "mem_available_pct": { "$avg": "$mem_available_pct" },
            "rx_mbps": { "$avg": "$rx_mbps" },
            "tx_mbps": { "$avg": "$tx_mbps" },
            "tx_mbps_max": { "$max": "$tx_mbps_max" },
            "allocations": { "$avg": "$allocations" },
            "coturn_sessions": { "$avg": "$coturn_sessions" },
            "derp_registrations": { "$avg": "$derp_registrations" },
        },
    };
    vec![
        doc! { "$match": { "region": region, "ts": { "$gte": floor } } },
        doc! { "$group": group },
        doc! { "$set": {
            "t": t_secs(),
            "healthy_pct": { "$cond": [
                { "$gt": [ "$sample_count", 0 ] },
                { "$multiply": [ { "$divide": [ "$healthy_count", "$sample_count" ] }, 100.0 ] },
                Bson::Null,
            ] },
        }},
        doc! { "$unset": "_id" },
        doc! { "$sort": { "t": 1 } },
    ]
}

/// Suffix for the tier's source collection.
fn tier_coll(base: &str, tier: Tier) -> String {
    match tier {
        Tier::Raw => base.to_string(),
        Tier::Hour => format!("{base}_1h"),
        Tier::Day => format!("{base}_1d"),
    }
}

// ── Tenant endpoints ────────────────────────────────────────────────────

/// GET /api/tenant/{tid}/stats/overview — ANY member. Powers the org
/// dashboard Insights panel: current counts + two small sparklines.
pub async fn tenant_overview(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = parse_tid(&tenant_id)?;
    require_tenant_stats(&state, tid, auth.user_id, false).await?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }

    let machines_total = state
        .db
        .collection::<Document>("agents")
        .count_documents(doc! { "tenant_id": tid, "deleted_at": Bson::Null })
        .await
        .unwrap_or(0);
    let machines_online = state
        .db
        .collection::<Document>("agents")
        .count_documents(
            doc! { "tenant_id": tid, "deleted_at": Bson::Null, "last_presence": "online" },
        )
        .await
        .unwrap_or(0);
    let calls_active = state
        .db
        .collection::<Document>("rooms")
        .count_documents(doc! { "tenant_id": tid, "conference_status": "in_progress" })
        .await
        .unwrap_or(0);

    // Minutes today + 7d daily sparkline from call_sessions (live calls
    // count up to now).
    let day_floor = floor_dt(7 * 86_400);
    let today_floor = {
        let now = DateTime::now().timestamp_millis() / 1000;
        DateTime::from_millis((now - now.rem_euclid(86_400)) * 1000)
    };
    let minutes_rows = agg(
        &state,
        "call_sessions",
        vec![
            doc! { "$match": { "tenant_id": tid, "started_at": { "$gte": day_floor } } },
            doc! { "$set": {
                "end_eff": { "$ifNull": [ "$ended_at", "$$NOW" ] },
            }},
            doc! { "$group": {
                "_id": { "$dateTrunc": { "date": "$started_at", "unit": "day" } },
                "minutes": { "$sum": { "$divide": [
                    { "$subtract": [ "$end_eff", "$started_at" ] },
                    60_000,
                ] } },
                "calls": { "$sum": 1 },
            }},
            doc! { "$set": { "t": t_secs() } },
            doc! { "$unset": "_id" },
            doc! { "$sort": { "t": 1 } },
        ],
    )
    .await?;
    let minutes_today = agg(
        &state,
        "call_sessions",
        vec![
            doc! { "$match": { "tenant_id": tid, "started_at": { "$gte": today_floor } } },
            doc! { "$set": { "end_eff": { "$ifNull": [ "$ended_at", "$$NOW" ] } } },
            doc! { "$group": {
                "_id": Bson::Null,
                "minutes": { "$sum": { "$divide": [
                    { "$subtract": [ "$end_eff", "$started_at" ] },
                    60_000,
                ] } },
            }},
            doc! { "$unset": "_id" },
        ],
    )
    .await?
    .first()
    .and_then(|v| v.get("minutes").and_then(|m| m.as_f64()))
    .unwrap_or(0.0);

    // 24 h machines-online sparkline from the hourly rollup.
    let spark_machines = agg(
        &state,
        "stats_machine_1h",
        machine_series_pipeline(tid, floor_dt(86_400), Tier::Hour),
    )
    .await?;

    Ok(Json(serde_json::json!({
        "enabled": true,
        "machines": { "online": machines_online, "total": machines_total },
        "calls": { "active": calls_active, "minutes_today": minutes_today },
        "spark_machines": spark_machines,
        "spark_minutes": minutes_rows,
    })))
}

/// GET /api/tenant/{tid}/stats/machines?range= — MANAGE_AGENTS.
pub async fn tenant_machines(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = parse_tid(&tenant_id)?;
    require_tenant_stats(&state, tid, auth.user_id, true).await?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }
    machines_payload(&state, tid, q.range.as_deref()).await
}

async fn machines_payload(
    state: &AppState,
    tid: ObjectId,
    range: Option<&str>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (window, tier) = range_spec(range)?;
    let floor = floor_dt(window);
    let series = agg(
        state,
        &tier_coll("stats_machine", tier),
        machine_series_pipeline(tid, floor, tier),
    )
    .await?;
    // Per-agent totals over the window (names resolve client-side from
    // the agents store the UI already holds).
    let per_agent = agg(
        state,
        &tier_coll("stats_machine", tier),
        vec![
            doc! { "$match": { "tenant_id": tid, "ts": { "$gte": floor } } },
            doc! { "$group": {
                "_id": "$agent_id",
                "online_minutes": { "$sum": if tier == Tier::Raw {
                    doc! { "$cond": [ "$online", 1, 0 ] }.into()
                } else {
                    Bson::String("$online_minutes".to_string())
                } },
                "cpu_pct": { "$avg": if tier == Tier::Raw { "$sys.cpu_pct" } else { "$cpu_pct" } },
                "peer_rtt_ms": { "$avg": if tier == Tier::Raw { "$sys.peer_rtt_ms" } else { "$peer_rtt_ms" } },
            }},
            doc! { "$set": { "agent_id": { "$toString": "$_id" } } },
            doc! { "$unset": "_id" },
            doc! { "$sort": { "online_minutes": -1 } },
            doc! { "$limit": 100 },
        ],
    )
    .await?;
    let uptime = uptime_intervals(state, tid, floor).await?;
    Ok(Json(serde_json::json!({
        "enabled": true,
        "range": range.unwrap_or("24h"),
        "series": series,
        "agents": per_agent,
        "uptime": uptime,
    })))
}

/// Cap on agents/intervals returned by the uptime view — a fleet-wide
/// year query must not turn into an unbounded payload.
const UPTIME_MAX_AGENTS: usize = 100;
const UPTIME_MAX_INTERVALS: usize = 500;

/// Per-agent presence intervals over the window, reconstructed from the
/// `stats_events` transition ledger.
///
/// The ledger records CHANGES, so three pieces are needed to render a
/// continuous strip: the state the agent was already in when the window
/// opened (its last transition BEFORE the floor), the transitions inside
/// the window, and the live tail (`agents.last_presence`) closing the
/// final interval at "now". An agent with no prior transition starts as
/// `unknown` — honest about "we weren't recording yet" rather than
/// back-filling a state we never observed.
async fn uptime_intervals(
    state: &AppState,
    tid: ObjectId,
    floor: DateTime,
) -> Result<Vec<serde_json::Value>, ApiError> {
    // Live tail + display names, and the agent set we report on.
    let agents = agg(
        state,
        "agents",
        vec![
            doc! { "$match": { "tenant_id": tid, "deleted_at": Bson::Null } },
            doc! { "$set": { "id": { "$toString": "$_id" } } },
            doc! { "$project": { "_id": 0, "id": 1, "name": 1, "last_presence": 1 } },
            doc! { "$limit": UPTIME_MAX_AGENTS as i64 },
        ],
    )
    .await?;
    if agents.is_empty() {
        return Ok(Vec::new());
    }

    // State at window open: the newest transition strictly before floor.
    let mut prior: HashMap<String, String> = HashMap::new();
    for d in agg(
        state,
        roomler_ai_services::dao::stats::STATS_EVENTS,
        vec![
            doc! { "$match": { "tenant_id": tid, "ts": { "$lt": floor } } },
            doc! { "$sort": { "ts": -1 } },
            doc! { "$group": { "_id": "$agent_id", "presence": { "$first": "$presence" } } },
            doc! { "$set": { "agent_id": { "$toString": "$_id" } } },
            doc! { "$unset": "_id" },
        ],
    )
    .await?
    {
        if let (Some(a), Some(p)) = (
            d.get("agent_id").and_then(|v| v.as_str()),
            d.get("presence").and_then(|v| v.as_str()),
        ) {
            prior.insert(a.to_string(), p.to_string());
        }
    }

    // Transitions inside the window, oldest first per agent.
    let mut events: HashMap<String, Vec<(i64, String)>> = HashMap::new();
    for d in agg(
        state,
        roomler_ai_services::dao::stats::STATS_EVENTS,
        vec![
            doc! { "$match": { "tenant_id": tid, "ts": { "$gte": floor } } },
            doc! { "$sort": { "ts": 1 } },
            doc! { "$set": {
                "agent_id": { "$toString": "$agent_id" },
                "t": { "$toLong": { "$divide": [ { "$toLong": "$ts" }, 1000 ] } },
            }},
            doc! { "$project": { "_id": 0, "agent_id": 1, "t": 1, "presence": 1 } },
        ],
    )
    .await?
    {
        if let (Some(a), Some(t), Some(p)) = (
            d.get("agent_id").and_then(|v| v.as_str()),
            d.get("t").and_then(|v| v.as_i64()),
            d.get("presence").and_then(|v| v.as_str()),
        ) {
            events
                .entry(a.to_string())
                .or_default()
                .push((t, p.to_string()));
        }
    }

    // When the ledger itself starts. Before this instant we recorded
    // nothing, so no agent's state can be claimed for that stretch — it
    // renders `unknown` rather than back-filling today's state across
    // history that predates the feature. AFTER it, silence is real
    // information: `note_transition` records every change, so an agent
    // with no events simply never changed state.
    let ledger_start = agg(
        state,
        roomler_ai_services::dao::stats::STATS_EVENTS,
        vec![
            doc! { "$match": { "tenant_id": tid } },
            doc! { "$group": { "_id": Bson::Null, "first": { "$min": "$ts" } } },
            doc! { "$set": { "t": { "$toLong": { "$divide": [ { "$toLong": "$first" }, 1000 ] } } } },
            doc! { "$project": { "_id": 0, "t": 1 } },
        ],
    )
    .await?
    .first()
    .and_then(|d| d.get("t").and_then(|v| v.as_i64()));

    let from = floor.timestamp_millis() / 1000;
    let now = DateTime::now().timestamp_millis() / 1000;
    let mut out = Vec::with_capacity(agents.len());
    for a in &agents {
        let Some(id) = a.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let has_prior = prior.contains_key(id);
        let mut cursor = from;
        let mut state_now = prior
            .get(id)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        let mut intervals: Vec<serde_json::Value> = Vec::new();
        // No transition before the window ⇒ the state at window-open is
        // only knowable from when the ledger began recording.
        if !has_prior
            && let Some(start) = ledger_start
            && start > from
        {
            let start = start.min(now);
            intervals.push(serde_json::json!({
                "from": from, "to": start, "presence": "unknown",
            }));
            cursor = start;
        }
        for (t, presence) in events.get(id).map(Vec::as_slice).unwrap_or(&[]) {
            let t = (*t).clamp(from, now);
            if t > cursor {
                intervals.push(serde_json::json!({
                    "from": cursor, "to": t, "presence": state_now,
                }));
                cursor = t;
            }
            state_now = presence.clone();
            if intervals.len() >= UPTIME_MAX_INTERVALS {
                break;
            }
        }
        // Live tail: the ledger's last word is authoritative for history,
        // but the CURRENT state comes from the agents row (the sweeper
        // heals stale/offline there even when no event was queued).
        let tail = a
            .get("last_presence")
            .and_then(|v| v.as_str())
            .unwrap_or(&state_now)
            .to_string();
        if now > cursor {
            intervals.push(serde_json::json!({
                "from": cursor, "to": now, "presence": tail,
            }));
        }
        out.push(serde_json::json!({
            "agent_id": id,
            "name": a.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            "intervals": intervals,
        }));
    }
    Ok(out)
}

/// Carrier ranking for the pessimistic edge merge: when the two ends
/// disagree about how they reach each other, the WORSE opinion wins.
/// One end believing it has a direct carrier while the other is still
/// relaying means the pair is, in practice, relaying.
fn carrier_rank(c: &str) -> u8 {
    match c {
        "direct" => 0,
        "relay" => 1,
        "derp" => 2,
        "tunnel" => 3,
        "blocked" => 4,
        _ => 5, // offline / unknown
    }
}

/// GET /api/tenant/{tid}/stats/mesh — the org's overlay topology as a
/// graph: the control plane at the centre, one node per device, and
/// edges for both the control-plane WS and the peer-to-peer carriers.
///
/// Member-visible (read-only, no addresses or secrets — carrier kind and
/// latency only), because the dashboard panel it feeds is.
pub async fn tenant_mesh(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = parse_tid(&tenant_id)?;
    require_tenant_stats(&state, tid, auth.user_id, false).await?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }

    // Nodes: every live device, with the overlay identity the mesh
    // edges are keyed by.
    let nodes = agg(
        &state,
        "overlay_nodes",
        vec![
            doc! { "$match": { "tenant_id": tid, "deleted_at": Bson::Null } },
            doc! { "$set": {
                "id": { "$toString": "$_id" },
                // An overlay node points at its owner through
                // `node_ref: {kind, id}` — there is NO flat `agent_id`.
                // Reading the wrong field yielded nulls, which silently
                // emptied the agent→node map and drew a graph with no
                // edges at all. Tunnel-client nodes have no agent, so
                // they resolve to null here by design.
                "agent_id_hex": { "$cond": [
                    { "$eq": [ "$node_ref.kind", "agent" ] },
                    { "$toString": "$node_ref.id" },
                    Bson::Null,
                ]},
            }},
            doc! { "$project": {
                "_id": 0, "id": 1, "agent_id_hex": 1, "name": 1,
                "overlay_ip": 1, "relay_home": 1, "status": 1, "last_seen_at": 1,
            }},
            doc! { "$limit": 500 },
        ],
    )
    .await?;

    // Agents carry presence + version; joined client-side by hex id so
    // the graph can grey out a device that is enrolled but offline.
    let agents = agg(
        &state,
        "agents",
        vec![
            doc! { "$match": { "tenant_id": tid, "deleted_at": Bson::Null } },
            doc! { "$set": { "id": { "$toString": "$_id" } } },
            doc! { "$project": {
                "_id": 0, "id": 1, "name": 1, "last_presence": 1,
                "agent_version": 1, "relay_home": 1, "os": 1,
            }},
            doc! { "$limit": 500 },
        ],
    )
    .await?;

    // Edges: merge the two ends' snapshots. Both report the same pair,
    // and they can legitimately disagree — take the worse carrier and
    // the lower RTT, and remember whether both ends actually agreed.
    let snapshots = agg(
        &state,
        roomler_ai_services::dao::stats::STATS_MESH,
        vec![
            doc! { "$match": { "tenant_id": tid } },
            doc! { "$set": { "from": { "$toString": "$agent_id" } } },
            doc! { "$project": { "_id": 0, "from": 1, "links": 1 } },
        ],
    )
    .await?;

    #[derive(Default)]
    struct Edge {
        carrier: String,
        rtt_ms: Option<i64>,
        stalled: bool,
        reports: u8,
    }
    // The two ends of an edge arrive in DIFFERENT id spaces: a snapshot
    // is keyed by the reporting AGENT, while the links inside it name
    // peers by their OVERLAY NODE id (that's what the overlay runtime
    // knows). Translate the reporter into node-space first, or every
    // edge silently fails to match a node and the graph draws nothing.
    let node_of_agent: HashMap<String, String> = nodes
        .iter()
        .filter_map(|n| {
            Some((
                n.get("agent_id_hex")?.as_str()?.to_string(),
                n.get("id")?.as_str()?.to_string(),
            ))
        })
        .collect();

    let mut edges: HashMap<(String, String), Edge> = HashMap::new();
    for snap in &snapshots {
        let Some(from) = snap
            .get("from")
            .and_then(|v| v.as_str())
            .and_then(|a| node_of_agent.get(a))
            .map(String::as_str)
        else {
            // A snapshot from an agent with no live overlay node (left
            // the mesh, or reported before joining) has nothing to
            // anchor its edges to.
            continue;
        };
        let Some(links) = snap.get("links").and_then(|v| v.as_array()) else {
            continue;
        };
        for l in links {
            let (Some(node), Some(carrier)) = (
                l.get("node").and_then(|v| v.as_str()),
                l.get("carrier").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            // Undirected: key on the sorted pair so both ends land on
            // the same entry.
            let key = if from <= node {
                (from.to_string(), node.to_string())
            } else {
                (node.to_string(), from.to_string())
            };
            let rtt = l.get("rtt_ms").and_then(|v| v.as_i64());
            let stalled = l.get("stalled").and_then(|v| v.as_bool()).unwrap_or(false);
            let e = edges.entry(key).or_default();
            e.reports += 1;
            e.stalled |= stalled;
            if e.carrier.is_empty() || carrier_rank(carrier) > carrier_rank(&e.carrier) {
                e.carrier = carrier.to_string();
            }
            e.rtt_ms = match (e.rtt_ms, rtt) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
    }
    let peer_edges: Vec<serde_json::Value> = edges
        .into_iter()
        .map(|((a, b), e)| {
            serde_json::json!({
                "kind": "peer",
                "from": a,
                "to": b,
                "carrier": e.carrier,
                "rtt_ms": e.rtt_ms,
                "stalled": e.stalled,
                // 1 = only one end reported this pair (the other is
                // offline, or pre-mesh). Worth surfacing: a one-sided
                // edge is a weaker claim than a corroborated one.
                "reports": e.reports,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "enabled": true,
        "center": { "id": "control-plane", "name": "roomler.ai" },
        "nodes": nodes,
        "agents": agents,
        "edges": peer_edges,
    })))
}

/// GET /api/tenant/{tid}/stats/calls?range= — MANAGE_AGENTS.
pub async fn tenant_calls(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = parse_tid(&tenant_id)?;
    require_tenant_stats(&state, tid, auth.user_id, true).await?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }
    calls_payload(&state, Some(tid), q.range.as_deref()).await
}

async fn calls_payload(
    state: &AppState,
    tid: Option<ObjectId>,
    range: Option<&str>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (window, tier) = range_spec(range)?;
    let floor = floor_dt(window);
    let series = agg(
        state,
        &tier_coll("stats_call", tier),
        call_series_pipeline(tid, floor, tier),
    )
    .await?;
    // Totals from the call ledger (exact regardless of tier).
    let mut match_doc = doc! { "started_at": { "$gte": floor } };
    if let Some(t) = tid {
        match_doc.insert("tenant_id", t);
    }
    let totals = agg(
        state,
        "call_sessions",
        vec![
            doc! { "$match": match_doc },
            doc! { "$set": { "end_eff": { "$ifNull": [ "$ended_at", "$$NOW" ] } } },
            doc! { "$group": {
                "_id": Bson::Null,
                "calls": { "$sum": 1 },
                "minutes": { "$sum": { "$divide": [
                    { "$subtract": [ "$end_eff", "$started_at" ] },
                    60_000,
                ] } },
                "participant_minutes": { "$sum": { "$divide": [ "$participant_seconds", 60 ] } },
                "peak_participants": { "$max": "$peak_participants" },
            }},
            doc! { "$unset": "_id" },
        ],
    )
    .await?;
    Ok(Json(serde_json::json!({
        "enabled": true,
        "range": range.unwrap_or("24h"),
        "series": series,
        "totals": totals.into_iter().next().unwrap_or(serde_json::json!({
            "calls": 0, "minutes": 0.0, "participant_minutes": 0.0,
        })),
    })))
}

/// GET /api/tenant/{tid}/stats/tunnels?range= — MANAGE_AGENTS. Aggregates
/// the existing tunnel_audit ledger (bytes + RelayMode per closed flow;
/// 90 d TTL bounds `1y` to what exists).
pub async fn tenant_tunnels(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tid = parse_tid(&tenant_id)?;
    require_tenant_stats(&state, tid, auth.user_id, true).await?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }
    let (window, tier) = range_spec(q.range.as_deref())?;
    let floor = floor_dt(window);
    let unit = match tier {
        Tier::Raw => doc! { "$dateTrunc": { "date": "$at", "unit": "minute", "binSize": 5 } },
        Tier::Hour => doc! { "$dateTrunc": { "date": "$at", "unit": "hour" } },
        Tier::Day => doc! { "$dateTrunc": { "date": "$at", "unit": "day" } },
    };
    let series = agg(
        &state,
        "tunnel_audit",
        vec![
            doc! { "$match": { "tenant_id": tid, "at": { "$gte": floor } } },
            doc! { "$group": {
                "_id": unit,
                "bytes_in": { "$sum": "$bytes_in" },
                "bytes_out": { "$sum": "$bytes_out" },
                "events": { "$sum": 1 },
                // RelayMode serializes snake_case ("direct"|"turn_udp"|"turn_tcp").
                "direct": { "$sum": { "$cond": [ { "$eq": [ "$relay", "direct" ] }, 1, 0 ] } },
                "relayed": { "$sum": { "$cond": [ { "$in": [ "$relay", [ "turn_udp", "turn_tcp" ] ] }, 1, 0 ] } },
            }},
            doc! { "$set": { "t": t_secs() } },
            doc! { "$unset": "_id" },
            doc! { "$sort": { "t": 1 } },
        ],
    )
    .await?;
    Ok(Json(serde_json::json!({
        "enabled": true,
        "range": q.range.as_deref().unwrap_or("24h"),
        "series": series,
    })))
}

// ── Page-view beacon ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PageViewBody {
    /// Client route paths, batched. NORMALISED server-side — ids never
    /// reach an analytics row (see `user_analytics::normalize_path`).
    pub paths: Vec<String>,
    #[serde(default)]
    pub tenant_id: Option<String>,
}

/// POST /api/stats/pageview — the SPA's route-change beacon.
///
/// Authenticated (so a view is attributable to a user without any
/// cookie of our own) and deliberately dumb: it records the normalised
/// route and the timestamp, nothing else. Batches are capped so a
/// misbehaving client can't turn this into a write amplifier.
pub async fn page_view(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<PageViewBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.settings.stats.enabled {
        return Ok(Json(serde_json::json!({ "enabled": false })));
    }
    const MAX_BATCH: usize = 50;
    let tenant = body.tenant_id.as_deref().and_then(|t| parse_tid(t).ok());
    let now = DateTime::now();
    let docs: Vec<Document> = body
        .paths
        .iter()
        .take(MAX_BATCH)
        .map(|p| {
            doc! {
                "user_id": auth.user_id,
                "tenant_id": tenant,
                "path": crate::user_analytics::normalize_path(p),
                "ts": now,
            }
        })
        .collect();
    if docs.is_empty() {
        return Ok(Json(serde_json::json!({ "recorded": 0 })));
    }
    let n = docs.len();
    if let Err(e) = state
        .db
        .collection::<Document>(crate::user_analytics::PAGE_VIEWS)
        .insert_many(docs)
        .await
    {
        // Analytics must never fail a user's navigation.
        tracing::debug!(%e, "page view persist failed");
        return Ok(Json(serde_json::json!({ "recorded": 0 })));
    }
    Ok(Json(serde_json::json!({ "recorded": n })))
}

/// GET /api/admin/stats/users?range= — platform user analytics: WS
/// sessions over time, durations, browsers, platforms, countries, pages,
/// and the per-org split.
pub async fn admin_users(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(q): Query<RangeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_platform_admin(&state, &auth)?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }
    let (window, tier) = range_spec(q.range.as_deref())?;
    let floor = floor_dt(window);
    let unit = match tier {
        Tier::Raw => doc! { "$dateTrunc": { "date": "$started_at", "unit": "hour" } },
        Tier::Hour => doc! { "$dateTrunc": { "date": "$started_at", "unit": "hour" } },
        Tier::Day => doc! { "$dateTrunc": { "date": "$started_at", "unit": "day" } },
    };
    let ws = crate::user_analytics::WS_SESSIONS;

    // Sessions + distinct users per bucket. A still-open session counts
    // toward "now" via $$NOW so a live connection isn't invisible.
    let series = agg(
        &state,
        ws,
        vec![
            doc! { "$match": { "started_at": { "$gte": floor } } },
            doc! { "$set": { "end_eff": { "$ifNull": [ "$ended_at", "$$NOW" ] } } },
            doc! { "$group": {
                "_id": unit,
                "sessions": { "$sum": 1 },
                "users": { "$addToSet": "$user_id" },
                "avg_duration_s": { "$avg": { "$divide": [
                    { "$subtract": [ "$end_eff", "$started_at" ] }, 1000,
                ] } },
            }},
            doc! { "$set": { "t": t_secs(), "users": { "$size": "$users" } } },
            doc! { "$unset": "_id" },
            doc! { "$sort": { "t": 1 } },
        ],
    )
    .await?;

    // One breakdown shape, three fields.
    let breakdown = |field: &str| {
        vec![
            doc! { "$match": { "started_at": { "$gte": floor } } },
            doc! { "$group": { "_id": format!("${field}"), "sessions": { "$sum": 1 } } },
            doc! { "$set": { "key": { "$ifNull": [ "$_id", "unknown" ] } } },
            doc! { "$unset": "_id" },
            doc! { "$sort": { "sessions": -1 } },
            doc! { "$limit": 25 },
        ]
    };
    let browsers = agg(&state, ws, breakdown("browser")).await?;
    let platforms = agg(&state, ws, breakdown("platform")).await?;
    let countries = agg(&state, ws, breakdown("country")).await?;

    // Per-org: sessions, distinct users, total connected time.
    let orgs = agg(
        &state,
        ws,
        vec![
            doc! { "$match": { "started_at": { "$gte": floor }, "tenant_id": { "$ne": Bson::Null } } },
            doc! { "$set": { "end_eff": { "$ifNull": [ "$ended_at", "$$NOW" ] } } },
            doc! { "$group": {
                "_id": "$tenant_id",
                "sessions": { "$sum": 1 },
                "users": { "$addToSet": "$user_id" },
                "connected_minutes": { "$sum": { "$divide": [
                    { "$subtract": [ "$end_eff", "$started_at" ] }, 60_000,
                ] } },
            }},
            doc! { "$set": { "tenant_id": { "$toString": "$_id" }, "users": { "$size": "$users" } } },
            doc! { "$unset": "_id" },
            doc! { "$sort": { "connected_minutes": -1 } },
            doc! { "$limit": 100 },
        ],
    )
    .await?;

    let pages = agg(
        &state,
        crate::user_analytics::PAGE_VIEWS,
        vec![
            doc! { "$match": { "ts": { "$gte": floor } } },
            doc! { "$group": {
                "_id": "$path",
                "views": { "$sum": 1 },
                "users": { "$addToSet": "$user_id" },
            }},
            doc! { "$set": { "path": "$_id", "users": { "$size": "$users" } } },
            doc! { "$unset": "_id" },
            doc! { "$sort": { "views": -1 } },
            doc! { "$limit": 25 },
        ],
    )
    .await?;

    // Duration histogram — the shape of a session, not just its mean.
    let durations = agg(
        &state,
        ws,
        vec![
            doc! { "$match": { "started_at": { "$gte": floor } } },
            doc! { "$set": { "end_eff": { "$ifNull": [ "$ended_at", "$$NOW" ] } } },
            doc! { "$set": { "secs": { "$divide": [
                { "$subtract": [ "$end_eff", "$started_at" ] }, 1000,
            ] } } },
            doc! { "$bucket": {
                "groupBy": "$secs",
                "boundaries": [0, 60, 300, 900, 3600, 14400, 86_400],
                "default": "86400+",
                "output": { "sessions": { "$sum": 1 } },
            }},
            doc! { "$set": { "bucket": { "$toString": "$_id" } } },
            doc! { "$unset": "_id" },
        ],
    )
    .await?;

    Ok(Json(serde_json::json!({
        "enabled": true,
        "range": q.range.as_deref().unwrap_or("24h"),
        "geoip": state.geoip.enabled(),
        "series": series,
        "browsers": browsers,
        "platforms": platforms,
        "countries": countries,
        "orgs": orgs,
        "pages": pages,
        "durations": durations,
    })))
}

// ── Admin endpoints ─────────────────────────────────────────────────────

/// GET /api/admin/stats/relay/current — realtime cluster view: newest
/// persisted bucket per region (cluster-wide truth — the in-memory map is
/// per-pod), busy from this pod's load map, and the fleet-eye latency
/// (avg of agents' probe RTTs per region).
pub async fn admin_relay_current(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_platform_admin(&state, &auth)?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }

    // Newest sample per region (any region seen in the last 10 min).
    let latest = agg(
        &state,
        "stats_relay",
        vec![
            doc! { "$match": { "ts": { "$gte": floor_dt(600) } } },
            doc! { "$sort": { "ts": -1 } },
            doc! { "$group": { "_id": "$region", "doc": { "$first": "$$ROOT" } } },
            doc! { "$replaceRoot": { "newRoot": "$doc" } },
            doc! { "$set": { "t": { "$toLong": { "$divide": [ { "$toLong": "$ts" }, 1000 ] } } } },
            doc! { "$unset": [ "_id", "ts" ] },
        ],
    )
    .await?;

    // Fleet-eye RTT per region from persisted agent probe tables.
    let agent_rtt = agg(
        &state,
        "agents",
        vec![
            doc! { "$match": { "deleted_at": Bson::Null, "relay_rtt": { "$type": "array" } } },
            doc! { "$unwind": "$relay_rtt" },
            doc! { "$match": { "relay_rtt.rtt_ms": { "$ne": Bson::Null } } },
            doc! { "$group": {
                "_id": "$relay_rtt.region",
                "rtt_avg_ms": { "$avg": "$relay_rtt.rtt_ms" },
                "agents": { "$sum": 1 },
            }},
            doc! { "$set": { "region": "$_id" } },
            doc! { "$unset": "_id" },
        ],
    )
    .await?;

    // Region roster from config, with busy from the live map.
    let regions: Vec<serde_json::Value> = state
        .turn_map
        .specs
        .iter()
        .map(|s| {
            let busy = roomler_ai_remote_control::turn_creds::region_busy(&state.relay_load, &s.id);
            serde_json::json!({
                "id": s.id,
                "enabled": s.enabled,
                // Monitored = the poller has somewhere to fetch: a
                // regional DERP host, or the explicit `stats_urls`
                // override (multi-worker regions like the central fleet).
                "monitored": s.derp_url.is_some() || !s.stats_urls.is_empty(),
                "workers": (s.stats_urls.len().max(1)) as i64,
                "busy": busy,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "enabled": true,
        "regions_enabled": state.turn_map.enabled,
        "regions": regions,
        "latest": latest,
        "agent_rtt": agent_rtt,
    })))
}

/// GET /api/admin/stats/relay/history?region&range
pub async fn admin_relay_history(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(q): Query<RangeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_platform_admin(&state, &auth)?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }
    let region = q
        .region
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("region is required".into()))?;
    let (window, tier) = range_spec(q.range.as_deref())?;
    let series = agg(
        &state,
        &tier_coll("stats_relay", tier),
        relay_series_pipeline(region, floor_dt(window), tier),
    )
    .await?;
    Ok(Json(serde_json::json!({
        "enabled": true,
        "region": region,
        "range": q.range.as_deref().unwrap_or("24h"),
        "series": series,
    })))
}

/// GET /api/admin/stats/orgs — per-tenant machine + call rollup.
pub async fn admin_orgs(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_platform_admin(&state, &auth)?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }

    let tenants = agg(
        &state,
        "tenants",
        vec![
            doc! { "$match": { "deleted_at": Bson::Null } },
            doc! { "$set": { "id": { "$toString": "$_id" }, "created": "$created_at" } },
            doc! { "$project": { "_id": 0, "id": 1, "name": 1, "slug": 1, "created": 1 } },
            doc! { "$limit": 500 },
        ],
    )
    .await?;
    // Members per org — the third activity signal (an org with one
    // member, no devices and no calls is almost always a test artifact;
    // this deployment carries ~60 of them from integration runs).
    let members = agg(
        &state,
        "tenant_members",
        vec![
            doc! { "$group": { "_id": "$tenant_id", "members": { "$sum": 1 } } },
            doc! { "$set": { "tenant_id": { "$toString": "$_id" } } },
            doc! { "$unset": "_id" },
        ],
    )
    .await?;
    let machines = agg(
        &state,
        "agents",
        vec![
            doc! { "$match": { "deleted_at": Bson::Null } },
            doc! { "$group": {
                "_id": "$tenant_id",
                "total": { "$sum": 1 },
                "online": { "$sum": { "$cond": [ { "$eq": [ "$last_presence", "online" ] }, 1, 0 ] } },
            }},
            doc! { "$set": { "tenant_id": { "$toString": "$_id" } } },
            doc! { "$unset": "_id" },
        ],
    )
    .await?;
    let calls = agg(
        &state,
        "call_sessions",
        vec![
            doc! { "$match": { "started_at": { "$gte": floor_dt(30 * 86_400) } } },
            doc! { "$set": { "end_eff": { "$ifNull": [ "$ended_at", "$$NOW" ] } } },
            doc! { "$group": {
                "_id": "$tenant_id",
                "calls_30d": { "$sum": 1 },
                "minutes_30d": { "$sum": { "$divide": [
                    { "$subtract": [ "$end_eff", "$started_at" ] },
                    60_000,
                ] } },
            }},
            doc! { "$set": { "tenant_id": { "$toString": "$_id" } } },
            doc! { "$unset": "_id" },
        ],
    )
    .await?;
    Ok(Json(serde_json::json!({
        "enabled": true,
        "tenants": tenants,
        "machines": machines,
        "members": members,
        "calls": calls,
    })))
}

/// GET /api/admin/stats/machines?tenant_id&range — cross-org machine
/// series for a chosen tenant.
pub async fn admin_machines(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(q): Query<RangeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_platform_admin(&state, &auth)?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }
    let tid = q
        .tenant_id
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("tenant_id is required".into()))
        .and_then(parse_tid)?;
    machines_payload(&state, tid, q.range.as_deref()).await
}

/// GET /api/admin/stats/calls?tenant_id&range — tenant_id optional
/// (omitted ⇒ platform-wide).
pub async fn admin_calls(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(q): Query<RangeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_platform_admin(&state, &auth)?;
    if !state.settings.stats.enabled {
        return Ok(disabled_payload());
    }
    let tid = match q.tenant_id.as_deref() {
        Some(t) => Some(parse_tid(t)?),
        None => None,
    };
    calls_payload(&state, tid, q.range.as_deref()).await
}

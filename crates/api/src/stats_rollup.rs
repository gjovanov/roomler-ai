//! Stats PR-1 — the rollup compactor: raw samples → `_1h` (90 d) → `_1d`
//! (730 d), so year-scale queries survive the 7-day raw TTL.
//!
//! Mechanics (each protects against a specific failure):
//!
//! - **Cluster-singleton per cycle** via the same DB-name-scoped Redis NX
//!   claim the presence sweeper uses — prod pods share a DB (one roller),
//!   each test's UUID database claims independently. No Redis ⇒ single
//!   pod ⇒ run locally.
//! - **Per-family watermark** in `stats_meta`, advanced only AFTER that
//!   family's `$merge` succeeded — a death mid-run simply re-merges, and
//!   `$merge (whenMatched: replace)` with whole-bucket recomputation is
//!   idempotent.
//! - **The open bucket is recomputed every run**: the watermark is set to
//!   the START of the current hour/day, never past it, so a partially
//!   elapsed bucket keeps refreshing until it closes instead of freezing
//!   undercounted.
//! - The floor is clamped to 6 days back — bounded work after long
//!   downtime, still within the raw TTL window.
//!
//! `_1d` is built FROM `_1h` (raw TTL 7 d < 1 y). Averages of averages are
//! an accepted approximation for gauges; counts (`sample_count`,
//! `healthy_count`, `online_minutes`, `*_seconds`) are sums all the way
//! down and stay exact.

use bson::{Bson, DateTime, Document, doc};
use tracing::{debug, info};

use crate::state::AppState;
use roomler_ai_services::dao::stats::{STATS_CALL, STATS_CALL_USER, STATS_MACHINE, STATS_RELAY};

const ROLLUP_INTERVAL_SECS: u64 = 900; // 15 min
const HOUR: i64 = 3600;
const DAY: i64 = 86_400;
/// Never reprocess more than this much history in one run (bounds a
/// post-downtime catch-up; still inside the 7 d raw TTL).
const MAX_LOOKBACK_SECS: i64 = 6 * DAY;
/// First-ever run backfills this much.
const DEFAULT_LOOKBACK_SECS: i64 = 48 * HOUR;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// `"$_id.b"` (a bucket-start date) → the unix-seconds string used in the
/// deterministic rollup `_id`.
fn bucket_secs_str() -> Document {
    doc! { "$toString": { "$toLong": { "$divide": [ { "$toLong": "$_id.b" }, 1000 ] } } }
}

/// `$merge` terminal stage — whole-bucket replace keyed on `_id`.
fn merge_into(coll: &str) -> Document {
    doc! { "$merge": {
        "into": coll,
        "on": "_id",
        "whenMatched": "replace",
        "whenNotMatched": "insert",
    }}
}

fn relay_pipeline(floor: DateTime, unit: &str, src_is_rollup: bool, into: &str) -> Vec<Document> {
    // Raw samples carry healthy/load/… directly; the 1h→1d pass re-sums
    // the counts and averages the averages.
    let group = if !src_is_rollup {
        doc! {
            "_id": { "k": "$region", "b": { "$dateTrunc": { "date": "$ts", "unit": unit } } },
            "sample_count": { "$sum": 1 },
            "healthy_count": { "$sum": { "$cond": [ "$healthy", 1, 0 ] } },
            "poll_rtt_ms": { "$avg": "$poll_rtt_ms" },
            "load1": { "$avg": "$load1" },
            "load5_max": { "$max": "$load5" },
            "cpus": { "$max": "$cpus" },
            "mem_available_pct": { "$avg": { "$cond": [
                { "$gt": [ "$mem_total_kb", 0 ] },
                { "$divide": [ "$mem_available_kb", "$mem_total_kb" ] },
                Bson::Null,
            ] } },
            "rx_mbps": { "$avg": "$rx_mbps" },
            "rx_mbps_max": { "$max": "$rx_mbps" },
            "tx_mbps": { "$avg": "$tx_mbps" },
            "tx_mbps_max": { "$max": "$tx_mbps" },
            "allocations": { "$avg": "$allocations" },
            "allocations_max": { "$max": "$allocations" },
            "coturn_sessions": { "$avg": "$coturn_sessions" },
            "derp_registrations": { "$avg": "$derp_registrations" },
        }
    } else {
        doc! {
            "_id": { "k": "$region", "b": { "$dateTrunc": { "date": "$ts", "unit": unit } } },
            "sample_count": { "$sum": "$sample_count" },
            "healthy_count": { "$sum": "$healthy_count" },
            "poll_rtt_ms": { "$avg": "$poll_rtt_ms" },
            "load1": { "$avg": "$load1" },
            "load5_max": { "$max": "$load5_max" },
            "cpus": { "$max": "$cpus" },
            "mem_available_pct": { "$avg": "$mem_available_pct" },
            "rx_mbps": { "$avg": "$rx_mbps" },
            "rx_mbps_max": { "$max": "$rx_mbps_max" },
            "tx_mbps": { "$avg": "$tx_mbps" },
            "tx_mbps_max": { "$max": "$tx_mbps_max" },
            "allocations": { "$avg": "$allocations" },
            "allocations_max": { "$max": "$allocations_max" },
            "coturn_sessions": { "$avg": "$coturn_sessions" },
            "derp_registrations": { "$avg": "$derp_registrations" },
        }
    };
    vec![
        doc! { "$match": { "ts": { "$gte": floor } } },
        doc! { "$group": group },
        doc! { "$set": {
            "region": "$_id.k",
            "ts": "$_id.b",
            "_id": { "$concat": [ "$_id.k", ":", bucket_secs_str() ] },
        }},
        merge_into(into),
    ]
}

fn machine_pipeline(floor: DateTime, unit: &str, src_is_rollup: bool, into: &str) -> Vec<Document> {
    let group = if !src_is_rollup {
        doc! {
            "_id": { "k": "$agent_id", "b": { "$dateTrunc": { "date": "$ts", "unit": unit } } },
            "tenant_id": { "$first": "$tenant_id" },
            "sample_count": { "$sum": 1 },
            "online_minutes": { "$sum": { "$cond": [ "$online", 1, 0 ] } },
            "active_sessions_max": { "$max": "$active_sessions" },
            "cpu_pct": { "$avg": "$sys.cpu_pct" },
            "rss_mb": { "$avg": "$sys.rss_mb" },
            "net_rx_bytes_max": { "$max": "$sys.net_rx_bytes" },
            "net_rx_bytes_min": { "$min": "$sys.net_rx_bytes" },
            "net_tx_bytes_max": { "$max": "$sys.net_tx_bytes" },
            "net_tx_bytes_min": { "$min": "$sys.net_tx_bytes" },
            // Wave 3 — the overlay's own share of that traffic, same
            // cumulative-counter treatment (read side differences max-min).
            "overlay_rx_bytes_max": { "$max": "$sys.overlay_rx_bytes" },
            "overlay_rx_bytes_min": { "$min": "$sys.overlay_rx_bytes" },
            "overlay_tx_bytes_max": { "$max": "$sys.overlay_tx_bytes" },
            "overlay_tx_bytes_min": { "$min": "$sys.overlay_tx_bytes" },
            "tunnel_rx_bytes_max": { "$max": "$sys.tunnel_rx_bytes" },
            "tunnel_rx_bytes_min": { "$min": "$sys.tunnel_rx_bytes" },
            "tunnel_tx_bytes_max": { "$max": "$sys.tunnel_tx_bytes" },
            "tunnel_tx_bytes_min": { "$min": "$sys.tunnel_tx_bytes" },
            "direct": { "$avg": "$sys.transports.direct" },
            "relay": { "$avg": "$sys.transports.relay" },
            "derp": { "$avg": "$sys.transports.derp" },
            "tunnel_flows": { "$avg": "$sys.tunnel_flows" },
            "rc_sessions": { "$avg": "$sys.rc_sessions" },
            "peer_rtt_ms": { "$avg": "$sys.peer_rtt_ms" },
        }
    } else {
        doc! {
            "_id": { "k": "$agent_id", "b": { "$dateTrunc": { "date": "$ts", "unit": unit } } },
            "tenant_id": { "$first": "$tenant_id" },
            "sample_count": { "$sum": "$sample_count" },
            "online_minutes": { "$sum": "$online_minutes" },
            "active_sessions_max": { "$max": "$active_sessions_max" },
            "cpu_pct": { "$avg": "$cpu_pct" },
            "rss_mb": { "$avg": "$rss_mb" },
            "net_rx_bytes_max": { "$max": "$net_rx_bytes_max" },
            "net_rx_bytes_min": { "$min": "$net_rx_bytes_min" },
            "net_tx_bytes_max": { "$max": "$net_tx_bytes_max" },
            "net_tx_bytes_min": { "$min": "$net_tx_bytes_min" },
            "overlay_rx_bytes_max": { "$max": "$overlay_rx_bytes_max" },
            "overlay_rx_bytes_min": { "$min": "$overlay_rx_bytes_min" },
            "overlay_tx_bytes_max": { "$max": "$overlay_tx_bytes_max" },
            "overlay_tx_bytes_min": { "$min": "$overlay_tx_bytes_min" },
            "tunnel_rx_bytes_max": { "$max": "$tunnel_rx_bytes_max" },
            "tunnel_rx_bytes_min": { "$min": "$tunnel_rx_bytes_min" },
            "tunnel_tx_bytes_max": { "$max": "$tunnel_tx_bytes_max" },
            "tunnel_tx_bytes_min": { "$min": "$tunnel_tx_bytes_min" },
            "direct": { "$avg": "$direct" },
            "relay": { "$avg": "$relay" },
            "derp": { "$avg": "$derp" },
            "tunnel_flows": { "$avg": "$tunnel_flows" },
            "rc_sessions": { "$avg": "$rc_sessions" },
            "peer_rtt_ms": { "$avg": "$peer_rtt_ms" },
        }
    };
    vec![
        doc! { "$match": { "ts": { "$gte": floor } } },
        doc! { "$group": group },
        doc! { "$set": {
            "agent_id": "$_id.k",
            "ts": "$_id.b",
            "_id": { "$concat": [ { "$toString": "$_id.k" }, ":", bucket_secs_str() ] },
        }},
        merge_into(into),
    ]
}

fn call_pipeline(floor: DateTime, unit: &str, src_is_rollup: bool, into: &str) -> Vec<Document> {
    // Raw buckets are per-room 30 s gauges; the rollup is per-TENANT (the
    // org graphs' shape). Seconds counters weight each room-bucket by its
    // 30 s width, so concurrent rooms sum correctly.
    if !src_is_rollup {
        vec![
            doc! { "$match": { "ts": { "$gte": floor } } },
            doc! { "$group": {
                "_id": { "k": "$tenant_id", "b": { "$dateTrunc": { "date": "$ts", "unit": unit } } },
                "sample_count": { "$sum": 1 },
                "participant_seconds": { "$sum": { "$multiply": [ "$participants", 30 ] } },
                "relayed_seconds": { "$sum": { "$multiply": [ "$relayed", 30 ] } },
                "direct_seconds": { "$sum": { "$multiply": [ "$direct", 30 ] } },
                "call_seconds": { "$sum": 30 },
                "peak_participants": { "$max": "$participants" },
                "send_bps": { "$avg": "$send_bps" },
                "recv_bps": { "$avg": "$recv_bps" },
                "loss_pct": { "$avg": "$loss_pct" },
                "rooms": { "$addToSet": "$room_id" },
            }},
            doc! { "$set": {
                "tenant_id": "$_id.k",
                "ts": "$_id.b",
                "distinct_rooms": { "$size": "$rooms" },
                "_id": { "$concat": [ { "$toString": "$_id.k" }, ":", bucket_secs_str() ] },
            }},
            doc! { "$unset": "rooms" },
            merge_into(into),
        ]
    } else {
        vec![
            doc! { "$match": { "ts": { "$gte": floor } } },
            doc! { "$group": {
                "_id": { "k": "$tenant_id", "b": { "$dateTrunc": { "date": "$ts", "unit": unit } } },
                "sample_count": { "$sum": "$sample_count" },
                "participant_seconds": { "$sum": "$participant_seconds" },
                "relayed_seconds": { "$sum": "$relayed_seconds" },
                "direct_seconds": { "$sum": "$direct_seconds" },
                "call_seconds": { "$sum": "$call_seconds" },
                "peak_participants": { "$max": "$peak_participants" },
                "send_bps": { "$avg": "$send_bps" },
                "recv_bps": { "$avg": "$recv_bps" },
                "loss_pct": { "$avg": "$loss_pct" },
            }},
            doc! { "$set": {
                "tenant_id": "$_id.k",
                "ts": "$_id.b",
                "_id": { "$concat": [ { "$toString": "$_id.k" }, ":", bucket_secs_str() ] },
            }},
            merge_into(into),
        ]
    }
}

/// Wave 3 — per-(tenant, user) call usage.
///
/// Rates in, **bytes out**: a 30 s gauge multiplied by its own bucket width
/// is an exact byte count, and unlike the averaged gauges elsewhere in this
/// file both columns are pure sums, so they stay exact through raw → 1h → 1d.
/// That matters here in a way it doesn't for a dashboard line: these numbers
/// are the usage ledger, and an average-of-averages would quietly misreport
/// a user who was only in a call for part of the bucket.
fn call_user_pipeline(
    floor: DateTime,
    unit: &str,
    src_is_rollup: bool,
    into: &str,
) -> Vec<Document> {
    let key = doc! { "$concat": [
        { "$toString": "$tenant_id" }, ":", { "$toString": "$user_id" },
    ]};
    let group = if !src_is_rollup {
        doc! {
            "_id": { "k": key.clone(), "b": { "$dateTrunc": { "date": "$ts", "unit": unit } } },
            "tenant_id": { "$first": "$tenant_id" },
            "user_id": { "$first": "$user_id" },
            "seconds": { "$sum": 30 },
            "up_bytes": { "$sum": { "$divide": [ { "$multiply": [ "$up_bps", 30 ] }, 8 ] } },
            "down_bytes": { "$sum": { "$divide": [ { "$multiply": [ "$down_bps", 30 ] }, 8 ] } },
        }
    } else {
        doc! {
            "_id": { "k": key.clone(), "b": { "$dateTrunc": { "date": "$ts", "unit": unit } } },
            "tenant_id": { "$first": "$tenant_id" },
            "user_id": { "$first": "$user_id" },
            "seconds": { "$sum": "$seconds" },
            "up_bytes": { "$sum": "$up_bytes" },
            "down_bytes": { "$sum": "$down_bytes" },
        }
    };
    vec![
        doc! { "$match": { "ts": { "$gte": floor } } },
        doc! { "$group": group },
        doc! { "$set": {
            "ts": "$_id.b",
            "_id": { "$concat": [ "$_id.k", ":", bucket_secs_str() ] },
        }},
        merge_into(into),
    ]
}

/// One (src → dst) compaction pass with its watermark dance. Returns true
/// when the `$merge` ran (and the watermark advanced).
async fn roll_family(
    state: &AppState,
    key: &str,
    src: &str,
    pipeline: Vec<Document>,
    bucket_secs: i64,
) -> bool {
    let now = unix_now();
    match state
        .db
        .collection::<Document>(src)
        .aggregate(pipeline)
        .await
    {
        Ok(mut cursor) => {
            // $merge emits no documents; drain to surface any late error.
            loop {
                match cursor.advance().await {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(e) => {
                        debug!(%key, %e, "stats rollup cursor error");
                        return false;
                    }
                }
            }
            // Watermark = start of the CURRENT open bucket, so the next
            // run recomputes it until it closes.
            let open_bucket_ms = (now - now.rem_euclid(bucket_secs)) * 1000;
            if let Err(e) = state.stats.set_rollup_watermark(key, open_bucket_ms).await {
                debug!(%key, %e, "stats rollup watermark write failed");
            }
            true
        }
        Err(e) => {
            debug!(%key, %e, "stats rollup aggregate failed");
            false
        }
    }
}

/// Compute this run's floor for a family from its persisted watermark.
async fn family_floor(state: &AppState, key: &str) -> DateTime {
    let now = unix_now();
    let default_floor = now - DEFAULT_LOOKBACK_SECS;
    let wm_secs = match state.stats.rollup_watermark(key).await {
        Ok(Some(ms)) => ms / 1000,
        _ => default_floor,
    };
    DateTime::from_millis(wm_secs.max(now - MAX_LOOKBACK_SECS) * 1000)
}

/// Run every family's 1h + 1d compaction once. Public so integration tests
/// drive it directly (the spawned loop's first tick is a full interval
/// out). Returns the number of `$merge` passes that completed.
pub async fn run_stats_rollup_once(state: &AppState) -> usize {
    if !state.settings.stats.enabled {
        return 0;
    }
    let mut done = 0usize;

    // relay: raw → 1h → 1d
    let f = family_floor(state, "relay:1h").await;
    done += roll_family(
        state,
        "relay:1h",
        STATS_RELAY,
        relay_pipeline(f, "hour", false, "stats_relay_1h"),
        HOUR,
    )
    .await as usize;
    let f = family_floor(state, "relay:1d").await;
    done += roll_family(
        state,
        "relay:1d",
        "stats_relay_1h",
        relay_pipeline(f, "day", true, "stats_relay_1d"),
        DAY,
    )
    .await as usize;

    // machine: raw → 1h → 1d
    let f = family_floor(state, "machine:1h").await;
    done += roll_family(
        state,
        "machine:1h",
        STATS_MACHINE,
        machine_pipeline(f, "hour", false, "stats_machine_1h"),
        HOUR,
    )
    .await as usize;
    let f = family_floor(state, "machine:1d").await;
    done += roll_family(
        state,
        "machine:1d",
        "stats_machine_1h",
        machine_pipeline(f, "day", true, "stats_machine_1d"),
        DAY,
    )
    .await as usize;

    // call: raw → 1h → 1d
    let f = family_floor(state, "call:1h").await;
    done += roll_family(
        state,
        "call:1h",
        STATS_CALL,
        call_pipeline(f, "hour", false, "stats_call_1h"),
        HOUR,
    )
    .await as usize;
    let f = family_floor(state, "call:1d").await;
    done += roll_family(
        state,
        "call:1d",
        "stats_call_1h",
        call_pipeline(f, "day", true, "stats_call_1d"),
        DAY,
    )
    .await as usize;

    // call-user (usage ledger): raw → 1h → 1d
    let f = family_floor(state, "call_user:1h").await;
    done += roll_family(
        state,
        "call_user:1h",
        STATS_CALL_USER,
        call_user_pipeline(f, "hour", false, "stats_call_user_1h"),
        HOUR,
    )
    .await as usize;
    let f = family_floor(state, "call_user:1d").await;
    done += roll_family(
        state,
        "call_user:1d",
        "stats_call_user_1h",
        call_user_pipeline(f, "day", true, "stats_call_user_1d"),
        DAY,
    )
    .await as usize;

    done
}

/// Stats PR-2 — close orphaned call state: `call_sessions` still open
/// whose room is no longer `in_progress`, and member sessions still open
/// in such rooms (a pod crash has no leave moment; the startup stale-reset
/// only runs on boot, this runs every rollup cycle). Durations are stamped
/// with the close time — bounded error ≤ one cycle. Returns
/// `(calls_closed, member_docs_touched)`.
pub async fn close_orphaned_call_state(state: &AppState) -> (u64, u64) {
    let in_progress: Vec<Bson> = state
        .db
        .collection::<Document>("rooms")
        .distinct("_id", doc! { "conference_status": "in_progress" })
        .await
        .unwrap_or_default();
    let now = DateTime::now();
    let calls = state
        .db
        .collection::<Document>(roomler_ai_services::dao::stats::CALL_SESSIONS)
        .update_many(
            doc! { "ended_at": Bson::Null, "room_id": { "$nin": in_progress.clone() } },
            doc! { "$set": { "ended_at": now, "end_reason": "stale_reset" } },
        )
        .await
        .map(|r| r.modified_count)
        .unwrap_or_else(|e| {
            debug!(%e, "orphan sweep: call close failed");
            0
        });
    let now_b = Bson::DateTime(now);
    let sessions = state
        .db
        .collection::<Document>("room_members")
        .update_many(
            doc! { "sessions.left_at": Bson::Null, "room_id": { "$nin": in_progress } },
            vec![doc! { "$set": {
                "sessions": { "$map": { "input": "$sessions", "as": "s", "in": {
                    "$cond": [
                        { "$eq": [ "$$s.left_at", null ] },
                        { "$mergeObjects": [ "$$s", {
                            "left_at": now_b.clone(),
                            "duration": { "$toLong": { "$divide": [
                                { "$subtract": [ now_b.clone(), "$$s.joined_at" ] },
                                1000,
                            ] } },
                        } ] },
                        "$$s",
                    ]
                }}},
                "updated_at": now_b.clone(),
            }}],
        )
        .await
        .map(|r| r.modified_count)
        .unwrap_or_else(|e| {
            debug!(%e, "orphan sweep: session close failed");
            0
        });
    if calls > 0 || sessions > 0 {
        info!(calls, sessions, "orphaned call state closed");
    }
    (calls, sessions)
}

/// Spawn the periodic compactor. First tick a full interval out (so
/// short-lived TestApps never race a test driving `run_stats_rollup_once`
/// directly); cluster-singleton per cycle via the Redis NX claim.
pub fn spawn_stats_rollup(state: AppState) {
    if !state.settings.stats.enabled {
        return;
    }
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(ROLLUP_INTERVAL_SECS);
        loop {
            tokio::time::sleep(interval).await;
            if let Some(redis) = &state.redis_pubsub {
                let key = format!("roomler:stats-rollup:{}", state.db.name());
                let ttl = ROLLUP_INTERVAL_SECS.saturating_sub(5);
                match redis.try_claim(&key, ttl).await {
                    Ok(true) => {}
                    Ok(false) => continue, // another pod rolled this cycle
                    Err(e) => {
                        debug!(%e, "stats rollup claim failed; skipping cycle");
                        continue;
                    }
                }
            }
            let done = run_stats_rollup_once(&state).await;
            close_orphaned_call_state(&state).await;
            info!(passes = done, "stats rollup cycle complete");
        }
    });
}

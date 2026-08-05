//! Relay-PoP load poller — the server half of load-aware region routing.
//!
//! Every `relay.stats_poll_secs` (default 30 s) it fetches each enabled
//! region's `https://<derp-host>/stats` (served by the PoP's derp-relay,
//! P6a) and writes a [`RegionLoad`] into the shared [`RelayLoadMap`] the Hub
//! and the overlay broker consult at issuance time. Busy-ness is advisory
//! and deliberately conservative:
//!
//! - `load5 / cpus > relay.busy_load` (default 1.5), or
//! - available memory below ~8 % of total, or
//! - sustained egress above `relay.busy_tx_mbps` (default 400 — sized for
//!   the 500 Mbps-capped OVH PoPs), computed from successive monotonic
//!   counter samples.
//!
//! A fetch failure leaves the region's last sample in place; consumers
//! ignore samples older than `REGION_LOAD_FRESH_SECS` (fail-open — a stats
//! blip must never mass-reroute traffic off a healthy PoP).
//!
//! Stats PR-1: when a [`StatsDao`] sink is provided, every tick is also
//! persisted as a `stats_relay` bucket — success as a full `$set` sample
//! (incl. poll RTT + the fields busy-ness doesn't need: rx rate, uptime,
//! DERP registrations, coturn sessions), failure as a `$setOnInsert`-only
//! `healthy:false` marker so the OTHER pod's success always wins the
//! bucket. Regions without a `derp_url` (e.g. the central fleet) are not
//! polled and therefore render as "not monitored", never as down.

use roomler_ai_remote_control::turn_creds::{RegionLoad, RelayLoadMap, TurnMap};
use roomler_ai_services::dao::stats::{RelaySample, StatsDao};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Derive the stats URL from a region's `derp_url`
/// (`wss://derp-x.roomler.ai/derp` → `https://derp-x.roomler.ai/stats`).
fn stats_url(derp_url: &str) -> Option<String> {
    let rest = derp_url
        .strip_prefix("wss://")
        .or_else(|| derp_url.strip_prefix("ws://"))?;
    let host = rest.split('/').next()?;
    Some(format!("https://{host}/stats"))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Rate in Mbps from successive monotonic byte counters; 0.0 on the first
/// sample or a counter reset (PoP reboot).
fn rate_mbps(prev: Option<(u64, Instant)>, bytes: u64, now: Instant) -> f64 {
    match prev {
        Some((p, at)) if bytes >= p && now > at => {
            (bytes - p) as f64 * 8.0 / now.duration_since(at).as_secs_f64() / 1_000_000.0
        }
        _ => 0.0,
    }
}

/// Spawn the poller. No-op (never spawns) when regions are disabled or none
/// carries a `derp_url`, or when `relay.stats_poll_secs` is 0.
pub fn spawn_poller(
    turn_map: Arc<TurnMap>,
    load: RelayLoadMap,
    settings: &roomler_ai_config::RelaySettings,
    stats: Option<Arc<StatsDao>>,
) {
    let poll_secs = settings.stats_poll_secs;
    let busy_load = settings.busy_load;
    let busy_tx_mbps = settings.busy_tx_mbps;
    let targets: Vec<(String, String)> = turn_map
        .specs
        .iter()
        .filter(|s| s.enabled && turn_map.regions.contains_key(&s.id))
        .filter_map(|s| Some((s.id.clone(), stats_url(s.derp_url.as_deref()?)?)))
        .collect();
    if !turn_map.enabled || targets.is_empty() || poll_secs == 0 {
        return;
    }
    info!(
        regions = targets.len(),
        poll_secs,
        busy_load,
        busy_tx_mbps,
        persist = stats.is_some(),
        "relay load poller starting"
    );
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!(%e, "relay load poller: client build failed; poller disabled");
                return;
            }
        };
        // region → ((tx_bytes, at), (rx_bytes, at)) for rate derivation
        // across samples.
        let mut prev_tx: HashMap<String, (u64, Instant)> = HashMap::new();
        let mut prev_rx: HashMap<String, (u64, Instant)> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_secs(poll_secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            for (region, url) in &targets {
                let started = Instant::now();
                let body = match client.get(url).send().await {
                    Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
                        Ok(v) => v,
                        Err(e) => {
                            debug!(%region, %e, "relay load: bad stats body");
                            record_unreachable(&stats, region).await;
                            continue;
                        }
                    },
                    Ok(r) => {
                        debug!(%region, status = %r.status(), "relay load: stats fetch non-200");
                        record_unreachable(&stats, region).await;
                        continue;
                    }
                    Err(e) => {
                        debug!(%region, %e, "relay load: stats fetch failed");
                        record_unreachable(&stats, region).await;
                        continue;
                    }
                };
                let poll_rtt_ms = started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                let f = |k: &str| body[k].as_f64().unwrap_or(0.0);
                let cpus = f("cpus").max(1.0);
                let load5 = f("load5");
                let mem_total = f("mem_total_kb");
                let mem_avail = f("mem_available_kb");
                let tx_bytes = body["net_tx_bytes"].as_u64().unwrap_or(0);
                let rx_bytes = body["net_rx_bytes"].as_u64().unwrap_or(0);
                let now = Instant::now();
                let tx_mbps = rate_mbps(
                    prev_tx.insert(region.clone(), (tx_bytes, now)),
                    tx_bytes,
                    now,
                );
                let rx_mbps = rate_mbps(
                    prev_rx.insert(region.clone(), (rx_bytes, now)),
                    rx_bytes,
                    now,
                );
                let busy = load5 / cpus > busy_load
                    || (mem_total > 0.0 && mem_avail / mem_total < 0.08)
                    || tx_mbps > busy_tx_mbps;
                if busy {
                    info!(%region, load5, cpus, tx_mbps, "relay load: region marked BUSY");
                }
                load.insert(
                    region.clone(),
                    RegionLoad {
                        busy,
                        load1: f("load1"),
                        tx_mbps,
                        coturn_allocations: body["coturn"]["allocations"].as_f64().unwrap_or(0.0),
                        updated_unix: unix_now() as u64,
                    },
                );
                if let Some(st) = &stats {
                    let sample = RelaySample {
                        region: region.clone(),
                        unix: unix_now(),
                        poll_rtt_ms,
                        cpus,
                        load1: f("load1"),
                        load5,
                        mem_total_kb: mem_total,
                        mem_available_kb: mem_avail,
                        rx_mbps,
                        tx_mbps,
                        allocations: body["coturn"]["allocations"].as_f64().unwrap_or(0.0),
                        coturn_sessions: body["coturn"]["sessions"].as_f64().unwrap_or(0.0),
                        derp_registrations: f("derp_registrations"),
                        uptime_s: f("uptime_s"),
                    };
                    if let Err(e) = st.upsert_relay_sample(&sample).await {
                        debug!(%region, %e, "relay load: sample persist failed");
                    }
                }
            }
        }
    });
}

/// Persist a `healthy:false` marker for this tick's bucket ($setOnInsert
/// only — must never clobber the other pod's successful sample).
async fn record_unreachable(stats: &Option<Arc<StatsDao>>, region: &str) {
    if let Some(st) = stats
        && let Err(e) = st.upsert_relay_unreachable(region, unix_now()).await
    {
        debug!(%region, %e, "relay load: unreachable marker persist failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_url_derives_from_derp_url() {
        assert_eq!(
            stats_url("wss://derp-us-east.roomler.ai/derp").as_deref(),
            Some("https://derp-us-east.roomler.ai/stats")
        );
        assert_eq!(stats_url("https://not-ws.example/derp"), None);
    }

    #[test]
    fn rate_mbps_handles_first_sample_and_counter_reset() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(10);
        // First sample → no rate.
        assert_eq!(rate_mbps(None, 1_000_000, t1), 0.0);
        // 10 MB over 10 s = 8 Mbps.
        let r = rate_mbps(Some((0, t0)), 10_000_000, t1);
        assert!((r - 8.0).abs() < 0.01, "got {r}");
        // Counter reset (reboot) → no rate, not a negative one.
        assert_eq!(rate_mbps(Some((5_000_000, t0)), 1_000_000, t1), 0.0);
    }
}

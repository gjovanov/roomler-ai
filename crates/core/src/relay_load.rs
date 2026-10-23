// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
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
//! bucket.
//!
//! Stats follow-up: a region can also declare explicit `stats_urls` —
//! needed by the CENTRAL fleet, which serves TURN from N coturn workers
//! and carries no `derp_url` (its `/derp` is JWT-authed). Multiple
//! endpoints are polled per tick and aggregated by [`Agg`]. A region with
//! neither source stays unpolled and renders "not monitored", never down.

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

/// Aggregate of one region's `/stats` endpoints for a single tick.
///
/// A region can be one PoP (the usual case — every rule below is the
/// identity on a single sample) or N coturn workers (the central fleet).
/// The combining rules are deliberately pessimistic on pressure and
/// additive on volume, so a busy worker can't hide behind idle siblings:
///
/// - capacity/pressure (`load1`, `load5`, `poll_rtt_ms`) → **max**
/// - headroom (`mem_available_kb` as a fraction, `uptime_s`) → **worst**
///   (the least free memory, the most recently restarted worker)
/// - volume (`net_rx/tx_bytes`, allocations, sessions, DERP registrations)
///   → **sum** (region totals)
/// - `cpus` → max, so the `load5 / cpus` busy ratio compares like with like
#[derive(Debug, Default)]
struct Agg {
    answered: u32,
    poll_rtt_ms: u32,
    cpus: f64,
    load1: f64,
    load5: f64,
    mem_total_kb: f64,
    mem_available_kb: f64,
    net_rx_bytes: u64,
    net_tx_bytes: u64,
    allocations: f64,
    coturn_sessions: f64,
    derp_registrations: f64,
    uptime_s: f64,
}

impl Agg {
    fn add(&mut self, body: &serde_json::Value, rtt_ms: u32) {
        let f = |k: &str| body[k].as_f64().unwrap_or(0.0);
        let mem_total = f("mem_total_kb");
        let mem_avail = f("mem_available_kb");
        // Worst free-memory FRACTION wins, so mixed-size workers compare
        // fairly; carry that worker's pair so the ratio stays consistent.
        let worse_mem = self.answered == 0
            || (mem_total > 0.0
                && (self.mem_total_kb <= 0.0
                    || mem_avail / mem_total < self.mem_available_kb / self.mem_total_kb));
        if worse_mem {
            self.mem_total_kb = mem_total;
            self.mem_available_kb = mem_avail;
        }
        let uptime = f("uptime_s");
        if self.answered == 0 || (uptime > 0.0 && uptime < self.uptime_s) {
            self.uptime_s = uptime;
        }
        self.answered += 1;
        self.poll_rtt_ms = self.poll_rtt_ms.max(rtt_ms);
        self.cpus = self.cpus.max(f("cpus"));
        self.load1 = self.load1.max(f("load1"));
        self.load5 = self.load5.max(f("load5"));
        self.net_rx_bytes = self
            .net_rx_bytes
            .saturating_add(body["net_rx_bytes"].as_u64().unwrap_or(0));
        self.net_tx_bytes = self
            .net_tx_bytes
            .saturating_add(body["net_tx_bytes"].as_u64().unwrap_or(0));
        self.allocations += body["coturn"]["allocations"].as_f64().unwrap_or(0.0);
        self.coturn_sessions += body["coturn"]["sessions"].as_f64().unwrap_or(0.0);
        self.derp_registrations += f("derp_registrations");
    }
}

/// A region's `/stats` endpoints: the explicit `stats_urls` override when
/// set (multi-worker regions like the central fleet, which has no
/// `derp_url` because the central `/derp` is JWT-authed), else the single
/// URL derived from `derp_url`. Regions with neither stay unpolled and
/// render as "not monitored" — never as down.
fn region_stats_urls(spec: &roomler_ai_remote_control::turn_creds::RelayRegionSpec) -> Vec<String> {
    if !spec.stats_urls.is_empty() {
        return spec.stats_urls.clone();
    }
    spec.derp_url
        .as_deref()
        .and_then(stats_url)
        .into_iter()
        .collect()
}

/// FR-115 P4a-2 — does this `/stats` body come from a derp-relay that takes
/// the allowance push? Only one that reports the set it holds (`allowance`,
/// an object, since P4a-2) does. A central worker's host exporter, or a PoP
/// from before P4a-2, has nothing to verify a set with and answers the
/// `POST` 405, so it is never sent one.
fn takes_allowance_push(body: &serde_json::Value) -> bool {
    body.get("allowance")
        .is_some_and(serde_json::Value::is_object)
}

/// Each region's endpoints that are in `set`: those whose last answered `GET`
/// took the push ([`takes_allowance_push`]), or keep coturn's totals
/// ([`keeps_turn_usage`]). A region left with none is dropped.
fn endpoints_in(
    targets: &[(String, Vec<String>)],
    takers: &std::collections::HashSet<String>,
) -> Vec<(String, Vec<String>)> {
    targets
        .iter()
        .map(|(region, urls)| {
            let urls: Vec<String> = urls
                .iter()
                .filter(|u| takers.contains(*u))
                .cloned()
                .collect();
            (region.clone(), urls)
        })
        .filter(|(_, urls)| !urls.is_empty())
        .collect()
}

/// FR-115 P4b-2 — does this `/stats` body come from a derp-relay that keeps
/// coturn's totals for the API to pull? Only one whose coturn publishes to the
/// PoP's own Redis reports `turn_usage` (an object). Every other endpoint is
/// never pulled.
fn keeps_turn_usage(body: &serde_json::Value) -> bool {
    body.get("turn_usage")
        .is_some_and(serde_json::Value::is_object)
}

/// The pull for one endpoint: its `/stats` path plus `/turn-usage` (under the
/// prefix every PoP's nginx already proxies), after the cursor this pod holds
/// for it.
fn turn_usage_url(stats_url: &str, cursor: Option<&(String, u64)>) -> String {
    let base = format!("{}/turn-usage", stats_url.trim_end_matches('/'));
    match cursor {
        Some((boot, after)) => format!("{base}?boot={boot}&after={after}"),
        None => base,
    }
}

/// FR-115 P4a-2 — the push of the over-allowance set to the PoPs, on the
/// poll the API already makes (`POST` to the same `/stats`; the `GET` is
/// unchanged). The set is the one the throttle's job publishes in Redis
/// (`roomler_ai_services::relay_allowance::set_key`), signed with the DERP
/// ticket key the PoP already verifies tickets with, so the PoP stays DB-free
/// and secret-free.
///
/// Every way this can fail leaves the PoPs throttling NOTHING: a PoP keeps a
/// set three polls after it arrives and then lets it lapse, so a stopped
/// push and an unreachable PoP both fail open. An empty or absent set is
/// pushed too: that is how the API's kill switch reaches a PoP within one poll.
///
/// It goes only to endpoints whose last `GET /stats` reported an
/// `allowance` ([`takes_allowance_push`]). A region's `stats_urls` may name
/// what is not a derp-relay at all (the central coturn workers' host
/// exporters), and those, like a PoP from before P4a-2, answer the `POST`
/// 405 on every poll.
pub struct AllowancePush {
    redis: Arc<crate::ws::redis_pubsub::RedisPubSub>,
    key: String,
    signer: roomler_ai_remote_control::derp_ticket::DerpTicketSigner,
}

impl AllowancePush {
    /// `None` without a usable ticket key: no PoP could verify a set.
    pub fn new(
        settings: &roomler_ai_config::RelaySettings,
        redis: Arc<crate::ws::redis_pubsub::RedisPubSub>,
        db_name: &str,
    ) -> Option<Self> {
        let private_b64 = settings.derp_ticket_private_key.as_deref()?;
        match roomler_ai_remote_control::derp_ticket::DerpTicketSigner::from_pkcs8_b64(private_b64)
        {
            Ok(signer) => Some(Self {
                redis,
                key: roomler_ai_services::relay_allowance::set_key(db_name),
                signer,
            }),
            Err(e) => {
                warn!(%e, "relay allowance push: no usable ticket key; PoPs will not throttle");
                None
            }
        }
    }

    /// Sign the current set and `POST` it to every endpoint at once (one dead
    /// PoP must not stretch the tick by its timeout per endpoint).
    async fn push(&self, client: &reqwest::Client, targets: &[(String, Vec<String>)], ttl: u64) {
        let raw = match self.redis.kv_get(&self.key).await {
            Ok(raw) => raw,
            Err(e) => {
                // Unreadable: push nothing, rather than an empty set over the
                // one another pod just pushed. The PoPs lapse on their own.
                debug!(%e, "relay allowance push: the set is unreadable; skipping this poll");
                return;
            }
        };
        // Absent = nobody is over (the job deletes an empty set); a set that
        // does not parse is treated the same way, which fails open.
        let set: std::collections::BTreeMap<String, u64> = raw
            .and_then(|r| serde_json::from_str(&r).ok())
            .unwrap_or_default();
        let token = match self.signer.sign_allowance_set(set, ttl) {
            Ok(t) => t,
            Err(e) => {
                warn!(%e, "relay allowance push: signing failed");
                return;
            }
        };
        let token = &token;
        let pushes = targets.iter().flat_map(|(region, urls)| {
            urls.iter().map(move |url| {
                let req = client
                    .post(url)
                    .header("content-type", "application/jwt")
                    .body(token.clone());
                async move { (region, url, req.send().await) }
            })
        });
        for (region, url, res) in futures::future::join_all(pushes).await {
            match res {
                Ok(r) if r.status().is_success() => {}
                // An older PoP answers 405: it has no throttle to feed.
                Ok(r) => {
                    debug!(%region, %url, status = %r.status(), "relay allowance push: refused")
                }
                Err(e) => debug!(%region, %url, %e, "relay allowance push: failed"),
            }
        }
    }
}

/// The most pages one endpoint is read in one poll; the rest wait for the next.
const TURN_USAGE_PAGES_PER_POLL: usize = 10;

/// FR-115 P4b-2 — TURN bytes from the regional PoPs' coturn. Each PoP's
/// `derp-relay` keeps the per-allocation totals its coturn publishes to the
/// PoP's own Redis. On its `/stats` poll, every pod pulls them from each PoP
/// that reports `turn_usage` ([`keeps_turn_usage`]), after the cursor it
/// holds, and books each through [`crate::turn_bytes::record`]: once across
/// pods (the PoP's region is in the claim), and never guessed.
///
/// Why a pull of the totals themselves, not a per-org counter on `/stats`:
/// every pod polls every PoP, and a cumulative counter diffed by two pods is
/// booked twice (#2763). And unlike `/stats` the pull is not public, because
/// the usernames name orgs and principals: each request is signed with the
/// ticket key for that PoP's region (`derp_ticket::sign_turn_usage_request`),
/// which the PoP checks with the public key it already holds.
///
/// Every failure under-reports and nothing is retried. The claim makes a
/// re-read harmless (a pod that restarts reads the PoP's whole hour again, and
/// books only what no pod has), but a failed bucket write is dropped.
pub struct TurnUsagePull {
    redis: Arc<crate::ws::redis_pubsub::RedisPubSub>,
    stats: Arc<StatsDao>,
    db_name: String,
    signer: roomler_ai_remote_control::derp_ticket::DerpTicketSigner,
}

/// One page of a PoP's kept totals, as `derp-relay` answers it.
#[derive(serde::Deserialize)]
struct TurnUsagePage {
    boot: String,
    totals: Vec<PulledTotal>,
    #[serde(default)]
    more: bool,
}

#[derive(serde::Deserialize)]
struct PulledTotal {
    seq: u64,
    ch: String,
    p: String,
}

impl TurnUsagePull {
    /// `None` without a usable ticket key: no PoP would answer a pull.
    pub fn new(
        settings: &roomler_ai_config::RelaySettings,
        redis: Arc<crate::ws::redis_pubsub::RedisPubSub>,
        stats: Arc<StatsDao>,
        db_name: &str,
    ) -> Option<Self> {
        let private_b64 = settings.derp_ticket_private_key.as_deref()?;
        match roomler_ai_remote_control::derp_ticket::DerpTicketSigner::from_pkcs8_b64(private_b64)
        {
            Ok(signer) => Some(Self {
                redis,
                stats,
                db_name: db_name.to_string(),
                signer,
            }),
            Err(e) => {
                warn!(%e, "turn usage pull: no usable ticket key; PoP TURN bytes are not attributed");
                None
            }
        }
    }

    /// Read one endpoint's totals after `cursor`, a page at a time, and book
    /// them. The cursor advances past every total READ.
    async fn pull(
        &self,
        client: &reqwest::Client,
        region: &str,
        stats_url: &str,
        cursor: &mut Option<(String, u64)>,
    ) {
        for _ in 0..TURN_USAGE_PAGES_PER_POLL {
            let token = match self.signer.sign_turn_usage_request(region) {
                Ok(t) => t,
                Err(e) => {
                    warn!(%e, "turn usage pull: signing failed");
                    return;
                }
            };
            let url = turn_usage_url(stats_url, cursor.as_ref());
            let page = match client.get(&url).bearer_auth(&token).send().await {
                Ok(r) if r.status().is_success() => match r.json::<TurnUsagePage>().await {
                    Ok(page) => page,
                    Err(e) => {
                        debug!(%region, %stats_url, %e, "turn usage pull: bad body");
                        return;
                    }
                },
                Ok(r) => {
                    debug!(%region, %stats_url, status = %r.status(), "turn usage pull: refused");
                    return;
                }
                Err(e) => {
                    debug!(%region, %stats_url, %e, "turn usage pull: failed");
                    return;
                }
            };
            for t in &page.totals {
                let Some(r) = roomler_ai_remote_control::turn_stats::parse(&t.ch, &t.p) else {
                    continue;
                };
                use crate::turn_bytes::{Outcome, Source};
                match crate::turn_bytes::record(
                    &self.redis,
                    &self.stats,
                    &self.db_name,
                    Source::Pop(region),
                    &r,
                )
                .await
                {
                    Outcome::Unattributed => info!(
                        %region,
                        username = %r.username,
                        bytes = r.sentb,
                        "turn usage pull: a username naming no org (unattributed, never guessed)"
                    ),
                    Outcome::Failed(e) => {
                        warn!(%region, %e, "turn usage pull: not recorded (under-reports)")
                    }
                    Outcome::Booked(_) | Outcome::AlreadyBooked | Outcome::Nothing => {}
                }
            }
            if let Some(last) = page.totals.last() {
                *cursor = Some((page.boot.clone(), last.seq));
            }
            if !page.more {
                return;
            }
        }
    }
}

/// How long a PoP's per-network baseline outlives its last sample: long, so a
/// PoP the API cannot reach for days still books everything it relayed
/// meanwhile, by its unchanged process's counter.
const NETWORK_BASELINE_TTL_SECS: u64 = 7 * 86_400;

/// FR-20 B, exact (#2763) — what books a PoP's relayed bytes to the network's
/// tenant, once across pods. Every pod polls every PoP, and a cumulative
/// counter that each pod diffs against its own last sample is booked once PER
/// POD. So the baseline lives in Redis, one per (region, endpoint, network),
/// and only the pod whose sample advances it books the difference
/// (`RedisPubSub::advance_baseline`).
///
/// ⚠ A stats URL must name ONE process. Behind a load balancer the boots
/// alternate, every sample starts a new baseline, and nothing is booked.
pub struct PopNetworkBytes {
    /// Resolves an overlay network id to its tenant.
    networks: Arc<roomler_ai_services::dao::overlay_network::OverlayNetworkDao>,
    redis: Arc<crate::ws::redis_pubsub::RedisPubSub>,
    db_name: String,
    /// Endpoints already warned about for reporting bytes with no `boot`.
    warned: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl PopNetworkBytes {
    pub fn new(
        networks: Arc<roomler_ai_services::dao::overlay_network::OverlayNetworkDao>,
        redis: Arc<crate::ws::redis_pubsub::RedisPubSub>,
        db_name: &str,
    ) -> Self {
        Self {
            networks,
            redis,
            db_name: db_name.to_string(),
            warned: Default::default(),
        }
    }

    fn key(&self, region: &str, url: &str, network: &str) -> String {
        format!(
            "roomler:pop_net_baseline:{}:{region}:{url}:{network}",
            self.db_name
        )
    }
}

/// One endpoint's per-network byte counters, with the process (`boot`) they
/// belong to. `None` when it reports none (a PoP from before FR-20 B is not
/// monitored, which is not the same as "relayed nothing"), or reports them
/// without a `boot`, which would leave a restart indistinguishable from a
/// stale sample.
fn network_counters(body: &serde_json::Value) -> Option<(&str, Vec<(&str, u64)>)> {
    let map = body["per_network"].as_object()?;
    let boot = body["boot"].as_str().filter(|b| !b.is_empty())?;
    let counters = map
        .iter()
        .filter_map(|(network, v)| Some((network.as_str(), v.as_u64()?)))
        .collect();
    Some((boot, counters))
}

/// FR-20 B, exact (#2763) — book one endpoint's relayed bytes per network,
/// once across pods: each counter advances the shared baseline, and only
/// what it advanced by is booked, to the network's tenant.
async fn book_network_bytes(
    stats: &StatsDao,
    pop: &PopNetworkBytes,
    region: &str,
    url: &str,
    body: &serde_json::Value,
) {
    let Some((boot, counters)) = network_counters(body) else {
        // A derp-relay from before #2763: its bytes go unbooked until it is
        // updated. Said once per endpoint, so the gap is not silent.
        if body["per_network"]
            .as_object()
            .is_some_and(|m| !m.is_empty())
            && pop
                .warned
                .lock()
                .is_ok_and(|mut w| w.insert(url.to_string()))
        {
            warn!(%region, %url, "fr-20: PoP reports bytes without a boot; not booked until it is updated (#2763)");
        }
        return;
    };
    for (network, cumulative) in counters {
        let key = pop.key(region, url, network);
        let delta = match pop
            .redis
            .advance_baseline(&key, boot, cumulative, NETWORK_BASELINE_TTL_SECS)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                debug!(%region, %e, "fr-20: baseline unavailable; not booked (under-reports)");
                continue;
            }
        };
        if delta == 0 {
            continue;
        }
        let Ok(nid) = bson::oid::ObjectId::parse_str(network) else {
            continue;
        };
        match pop.networks.base.find_by_id(nid).await {
            Ok(n) => {
                if let Err(e) = stats
                    .add_usage(
                        n.tenant_id,
                        roomler_ai_services::dao::stats::Meter::DerpBytes,
                        unix_now(),
                        delta as i64,
                    )
                    .await
                {
                    debug!(%region, %e, "fr-20: PoP usage bucket write failed");
                }
            }
            // Unresolvable ⇒ logged and dropped. An unattributed byte must
            // never become a wrongly attributed one.
            Err(_) => debug!(
                %region, %network,
                "fr-20: PoP bytes for an unknown network — unattributed"
            ),
        }
    }
}

/// Spawn the poller. No-op (never spawns) when regions are disabled or none
/// exposes a `/stats` endpoint, or when `relay.stats_poll_secs` is 0.
pub fn spawn_poller(
    turn_map: Arc<TurnMap>,
    load: RelayLoadMap,
    settings: &roomler_ai_config::RelaySettings,
    stats: Option<Arc<StatsDao>>,
    // FR-20 B — books PoP-relayed bytes to the network's tenant, once across
    // pods (#2763). `None` ⇒ not booked: metering is off, or there is no
    // Redis to share the baselines, which would otherwise book them once per
    // pod. The load poller itself is unaffected.
    pop_bytes: Option<PopNetworkBytes>,
    // FR-115 P4a-2 — `None` ⇒ no push, and so no PoP throttles (fail open).
    allowance_push: Option<AllowancePush>,
    // FR-115 P4b-2 — `None` ⇒ the PoPs' TURN bytes are not attributed.
    turn_pull: Option<TurnUsagePull>,
) {
    let poll_secs = settings.stats_poll_secs;
    let busy_load = settings.busy_load;
    let busy_tx_mbps = settings.busy_tx_mbps;
    let targets: Vec<(String, Vec<String>)> = turn_map
        .specs
        .iter()
        .filter(|s| s.enabled && turn_map.regions.contains_key(&s.id))
        .map(|s| (s.id.clone(), region_stats_urls(s)))
        .filter(|(_, urls)| !urls.is_empty())
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
        // FR-115 P4a-2 — the endpoints whose last answered `GET` reported an
        // `allowance`: the only ones the set is pushed to.
        let mut takers: std::collections::HashSet<String> = std::collections::HashSet::new();
        // FR-115 P4b-2 — the endpoints whose last answered `GET` reported
        // `turn_usage`, and the cursor (boot, last seq) this pod holds for each.
        let mut turn_keepers: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut turn_cursors: HashMap<String, (String, u64)> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_secs(poll_secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            for (region, urls) in &targets {
                // Multi-worker region: poll every endpoint and aggregate
                // pessimistically (see `Agg`). One unreachable worker of
                // three is NOT a down region — but its load no longer
                // hides behind its healthy siblings either.
                let mut agg = Agg::default();
                for url in urls {
                    let started = Instant::now();
                    let body = match client.get(url).send().await {
                        Ok(r) if r.status().is_success() => {
                            match r.json::<serde_json::Value>().await {
                                Ok(v) => v,
                                Err(e) => {
                                    debug!(%region, %url, %e, "relay load: bad stats body");
                                    continue;
                                }
                            }
                        }
                        Ok(r) => {
                            debug!(%region, %url, status = %r.status(), "relay load: stats fetch non-200");
                            continue;
                        }
                        Err(e) => {
                            debug!(%region, %url, %e, "relay load: stats fetch failed");
                            continue;
                        }
                    };
                    // Only an answered GET changes what an endpoint is; one
                    // that did not answer keeps its last known state.
                    if takes_allowance_push(&body) {
                        takers.insert(url.clone());
                    } else {
                        takers.remove(url);
                    }
                    if keeps_turn_usage(&body) {
                        turn_keepers.insert(url.clone());
                    } else {
                        turn_keepers.remove(url);
                    }
                    // FR-20 B, booked per endpoint and once across pods (#2763).
                    if let (Some(st), Some(pop)) = (&stats, &pop_bytes) {
                        book_network_bytes(st, pop, region, url, &body).await;
                    }
                    agg.add(
                        &body,
                        started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32,
                    );
                }
                if agg.answered == 0 {
                    record_unreachable(&stats, region).await;
                    continue;
                }
                let poll_rtt_ms = agg.poll_rtt_ms;
                let cpus = agg.cpus.max(1.0);
                let load5 = agg.load5;
                let mem_total = agg.mem_total_kb;
                let mem_avail = agg.mem_available_kb;
                let tx_bytes = agg.net_tx_bytes;
                let rx_bytes = agg.net_rx_bytes;
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
                        load1: agg.load1,
                        tx_mbps,
                        coturn_allocations: agg.allocations,
                        updated_unix: unix_now() as u64,
                    },
                );
                if let Some(st) = &stats {
                    let sample = RelaySample {
                        region: region.clone(),
                        unix: unix_now(),
                        poll_rtt_ms,
                        cpus,
                        load1: agg.load1,
                        load5,
                        mem_total_kb: mem_total,
                        mem_available_kb: mem_avail,
                        rx_mbps,
                        tx_mbps,
                        allocations: agg.allocations,
                        coturn_sessions: agg.coturn_sessions,
                        derp_registrations: agg.derp_registrations,
                        uptime_s: agg.uptime_s,
                    };
                    if let Err(e) = st.upsert_relay_sample(&sample).await {
                        debug!(%region, %e, "relay load: sample persist failed");
                    }
                }
            }
            // FR-115 P4a-2 — a PoP keeps the set three polls after it arrives.
            // After the GETs, so even the first poll pushes only to the
            // endpoints that take it.
            if let Some(push) = &allowance_push {
                let to = endpoints_in(&targets, &takers);
                if !to.is_empty() {
                    push.push(&client, &to, poll_secs.saturating_mul(3)).await;
                }
            }
            // FR-115 P4b-2 — pull every PoP that keeps coturn's totals, all at
            // once, so one dead PoP does not stretch the tick.
            if let Some(pull) = &turn_pull {
                let pulls = endpoints_in(&targets, &turn_keepers)
                    .into_iter()
                    .flat_map(|(region, urls)| {
                        urls.into_iter().map(move |url| (region.clone(), url))
                    })
                    .map(|(region, url)| {
                        let mut cursor = turn_cursors.get(&url).cloned();
                        let client = &client;
                        async move {
                            pull.pull(client, &region, &url, &mut cursor).await;
                            (url, cursor)
                        }
                    });
                for (url, cursor) in futures::future::join_all(pulls).await {
                    if let Some(c) = cursor {
                        turn_cursors.insert(url, c);
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

    fn body(
        load5: f64,
        cpus: f64,
        mem_total: f64,
        mem_avail: f64,
        tx: u64,
        allocs: f64,
    ) -> serde_json::Value {
        serde_json::json!({
            "cpus": cpus, "load1": load5 / 2.0, "load5": load5,
            "mem_total_kb": mem_total, "mem_available_kb": mem_avail,
            "net_rx_bytes": 10u64, "net_tx_bytes": tx,
            "uptime_s": 5000.0, "derp_registrations": 1.0,
            "coturn": { "allocations": allocs, "sessions": 2.0 },
        })
    }

    #[test]
    fn agg_of_one_endpoint_is_the_identity() {
        let mut a = Agg::default();
        a.add(&body(1.5, 2.0, 4_000_000.0, 1_000_000.0, 900, 7.0), 42);
        assert_eq!(a.answered, 1);
        assert_eq!(a.poll_rtt_ms, 42);
        assert_eq!(a.load5, 1.5);
        assert_eq!(a.cpus, 2.0);
        assert_eq!(a.mem_available_kb, 1_000_000.0);
        assert_eq!(a.net_tx_bytes, 900);
        assert_eq!(a.allocations, 7.0);
        assert_eq!(a.coturn_sessions, 2.0);
        assert_eq!(a.uptime_s, 5000.0);
    }

    #[test]
    fn agg_takes_worst_pressure_and_sums_volume() {
        // Three central coturn workers: one busy + memory-tight, two idle.
        let mut a = Agg::default();
        a.add(&body(0.1, 4.0, 8_000_000.0, 6_000_000.0, 100, 1.0), 10);
        a.add(&body(3.9, 2.0, 4_000_000.0, 200_000.0, 500, 4.0), 55);
        a.add(&body(0.2, 4.0, 8_000_000.0, 7_000_000.0, 300, 2.0), 12);

        assert_eq!(a.answered, 3);
        assert_eq!(a.poll_rtt_ms, 55, "worst RTT wins");
        assert_eq!(a.load5, 3.9, "busiest worker's load wins");
        assert_eq!(a.cpus, 4.0);
        // Tightest worker's memory pair is carried whole (5% free).
        assert_eq!(a.mem_total_kb, 4_000_000.0);
        assert_eq!(a.mem_available_kb, 200_000.0);
        // Volume is region-wide.
        assert_eq!(a.net_tx_bytes, 900);
        assert_eq!(a.net_rx_bytes, 30);
        assert_eq!(a.allocations, 7.0);
        assert_eq!(a.coturn_sessions, 6.0);
        assert_eq!(a.derp_registrations, 3.0);
    }

    #[test]
    fn region_stats_urls_prefers_explicit_override() {
        use roomler_ai_remote_control::turn_creds::RelayRegionSpec;
        let spec = |derp: Option<&str>, stats: Vec<&str>| RelayRegionSpec {
            id: "r".into(),
            turn_url: "turn:x:3478".into(),
            worker_urls: vec![],
            derp_url: derp.map(str::to_string),
            stats_urls: stats.into_iter().map(str::to_string).collect(),
            shared_secret: None,
            caps: Default::default(),
            relay_band: None,
            enabled: true,
        };
        // PoP: derived from derp_url (unchanged behaviour).
        assert_eq!(
            region_stats_urls(&spec(Some("wss://derp-x.example/derp"), vec![])),
            vec!["https://derp-x.example/stats".to_string()]
        );
        // Central fleet: no derp_url, explicit multi-worker endpoints.
        assert_eq!(
            region_stats_urls(&spec(None, vec!["http://a/stats", "http://b/stats"])).len(),
            2
        );
        // Override beats derivation when both are present.
        assert_eq!(
            region_stats_urls(&spec(
                Some("wss://derp-x.example/derp"),
                vec!["http://only/stats"]
            )),
            vec!["http://only/stats".to_string()]
        );
        // Neither ⇒ unpolled ("not monitored", never "down").
        assert!(region_stats_urls(&spec(None, vec![])).is_empty());
    }

    #[test]
    fn only_a_body_reporting_its_allowance_takes_the_push() {
        // A derp-relay since P4a-2: it reports the set it holds.
        assert!(takes_allowance_push(&serde_json::json!({
            "region": "us-east",
            "allowance": { "networks": 0, "age_s": null, "shaped_total": 0 }
        })));
        // A central worker's host exporter, and a PoP from before P4a-2: no
        // `allowance` at all. Both answered every push 405.
        assert!(!takes_allowance_push(
            &serde_json::json!({ "cpus": 2, "load1": 0.1 })
        ));
        // Present but not an object is not a derp-relay's report either.
        assert!(!takes_allowance_push(
            &serde_json::json!({ "allowance": null })
        ));
        assert!(!takes_allowance_push(
            &serde_json::json!({ "allowance": 3 })
        ));
    }

    /// FR-115 P4b-2 — only a derp-relay that keeps coturn's totals is
    /// pulled: one whose `/stats` carries a `turn_usage` object. A PoP whose
    /// coturn publishes nowhere reports `null`; an older one, nothing.
    #[test]
    fn only_a_body_reporting_turn_usage_is_pulled() {
        assert!(keeps_turn_usage(&serde_json::json!({
            "turn_usage": { "boot": "18f2-1a", "kept": 0, "last_seq": 0 }
        })));
        assert!(!keeps_turn_usage(
            &serde_json::json!({ "turn_usage": null })
        ));
        assert!(!keeps_turn_usage(&serde_json::json!({ "cpus": 2 })));
    }

    /// The pull goes under the `/stats` prefix the PoPs' nginx proxies, with
    /// the cursor once this pod holds one.
    #[test]
    fn the_pull_is_under_stats_with_the_cursor() {
        assert_eq!(
            turn_usage_url("https://derp-us-east.roomler.ai/stats", None),
            "https://derp-us-east.roomler.ai/stats/turn-usage"
        );
        assert_eq!(
            turn_usage_url(
                "https://derp-us-east.roomler.ai/stats/",
                Some(&("18f2-1a".to_string(), 42))
            ),
            "https://derp-us-east.roomler.ai/stats/turn-usage?boot=18f2-1a&after=42"
        );
    }

    #[test]
    fn push_targets_keep_only_the_endpoints_that_take_it() {
        let targets = vec![
            (
                "eu-central".to_string(),
                vec!["http://w1/stats".to_string(), "http://w2/stats".to_string()],
            ),
            (
                "us-east".to_string(),
                vec!["https://derp-us-east/stats".to_string()],
            ),
        ];
        let takers: std::collections::HashSet<String> =
            std::iter::once("https://derp-us-east/stats".to_string()).collect();
        assert_eq!(
            endpoints_in(&targets, &takers),
            vec![(
                "us-east".to_string(),
                vec!["https://derp-us-east/stats".to_string()]
            )],
            "a region left with no taker is dropped, not pushed to"
        );
        // Before any GET has answered, nobody is pushed to.
        assert!(endpoints_in(&targets, &Default::default()).is_empty());
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

#[cfg(test)]
mod fr20_tests {
    use super::network_counters;

    /// The PoP counters are cumulative, so what gets billed is the DIFF, and
    /// since #2763 the diff is against a baseline both pods share
    /// (`RedisPubSub::advance_baseline`, pinned by `usage_ledger_tests`
    /// against a real Redis). A counter is read only with the process it
    /// belongs to: without `boot`, a restart could not be told from another
    /// pod's stale sample, so it is not booked at all.
    #[test]
    fn counters_are_read_only_with_their_process() {
        let both = serde_json::json!({
            "boot": "18e11ef727b2f264-1",
            "per_network": { "6a54bf440b4fd609a7356f97": 5_000, "bad": "x" }
        });
        let (boot, counters) = network_counters(&both).unwrap();
        assert_eq!(boot, "18e11ef727b2f264-1");
        assert_eq!(counters, vec![("6a54bf440b4fd609a7356f97", 5_000)]);

        let no_boot = serde_json::json!({ "per_network": { "n": 5_000 } });
        assert!(network_counters(&no_boot).is_none(), "no boot, not booked");
        let empty_boot = serde_json::json!({ "boot": "", "per_network": { "n": 1 } });
        assert!(network_counters(&empty_boot).is_none());
    }

    /// A region with no `per_network` in its payload is a PoP that predates
    /// the field: "not monitored", which must not be confused with "relayed
    /// zero bytes". It produces no buckets at all; a zero would produce a row
    /// asserting the tenant used nothing.
    #[test]
    fn a_missing_per_network_object_yields_no_buckets() {
        let body = serde_json::json!({ "region": "eu", "boot": "b", "net_rx_bytes": 1 });
        assert!(network_counters(&body).is_none());
    }
}

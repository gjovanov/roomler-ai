//! Short-lived TURN credentials following the coturn REST API convention
//! ("draft-uberti-behave-turn-rest").
//!
//! Username format:  `<unix_expiry>:<user_id>`
//! Password:         base64( HMAC-SHA1(shared_secret, username) )
//!
//! The shared secret is the value passed as `--static-auth-secret` to coturn,
//! also set as `RC_TURN_SECRET` in roomler env.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::signaling::IceServer;
use crate::worker_pick::pick_index_fnv1a;

type HmacSha1 = Hmac<Sha1>;

#[derive(Debug, Clone)]
pub struct TurnConfig {
    /// e.g. ["turn:coturn.roomler.live:3478?transport=udp",
    ///        "turn:coturn.roomler.live:3478?transport=tcp",
    ///        "turns:coturn.roomler.live:5349?transport=tcp"]
    pub urls: Vec<String>,
    /// Same-worker TURN affinity (2026-07-14): per-worker URL variant lists,
    /// one inner `Vec` per coturn worker (each expanded like `urls`). The
    /// generic hostname is 3 DNS A records (one per worker), so each ICE side
    /// resolves independently and relay↔relay sessions routinely straddle two
    /// workers — and cross-worker flows involving the dual-IP worker drop
    /// packets under its SNAT asymmetry (the rc.112 tunnel same-worker-pin
    /// lesson; field 2026-07-14: corp↔corp double-relay stall-bursts). When
    /// ≥2 workers are configured (`ROOMLER__TURN__WORKER_URLS`),
    /// [`Self::issue_for_session`] deterministically picks ONE worker per
    /// session for BOTH sides — worker URLs first, generic-hostname fallback
    /// retained. Empty (default) = exactly the old behaviour.
    pub workers: Vec<Vec<String>>,
    pub shared_secret: String,
    pub ttl_secs: u32,
}

impl TurnConfig {
    pub fn issue(&self, user_id: &str) -> IceServer {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let expiry = now_secs + self.ttl_secs as u64;
        let username = format!("{expiry}:{user_id}");

        let mut mac = HmacSha1::new_from_slice(self.shared_secret.as_bytes())
            .expect("HMAC accepts any key length");
        mac.update(username.as_bytes());
        let credential = B64.encode(mac.finalize().into_bytes());

        IceServer {
            urls: self.urls.clone(),
            username: Some(username),
            credential: Some(credential),
        }
    }

    /// Same-worker affinity issuance: like [`Self::issue`] but, when ≥2
    /// workers are configured, puts ONE session-selected worker's URLs first
    /// (both ICE stacks weight earlier servers higher in candidate priority,
    /// so both sides converge on the same worker) with the generic-hostname
    /// `urls` kept after as the worker-down fallback. The selection is a
    /// stable hash of `session_key`, so the controller and the agent — issued
    /// creds independently by the Hub — pick the SAME worker. With <2 workers
    /// this is exactly [`Self::issue`].
    pub fn issue_for_session(&self, user_id: &str, session_key: &str) -> IceServer {
        let mut server = self.issue(user_id);
        if self.workers.len() >= 2
            && let Some(idx) = pick_index_fnv1a(session_key, self.workers.len())
        {
            let mut urls = self.workers[idx].clone();
            urls.extend(server.urls);
            server.urls = urls;
        }
        server
    }
}

/// One relay region as configured in `ROOMLER__RELAY__REGIONS` (a JSON array
/// of these). Mirrors the legacy `turn.*` settings shape per region: a generic
/// base URL, optional per-worker URLs for same-worker affinity, and the
/// variant capability set the PoP actually serves.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RelayRegionSpec {
    /// Region id, e.g. `"us-east"` — the value carried in `relay_home`.
    pub id: String,
    /// Generic base URL, e.g. `"turn:coturn-us-east.roomler.ai:3478"`.
    pub turn_url: String,
    /// Optional per-worker base URLs (multi-worker regions), like
    /// `ROOMLER__TURN__WORKER_URLS`.
    #[serde(default)]
    pub worker_urls: Vec<String>,
    /// The region's DERP relay endpoint, e.g.
    /// `"wss://derp-us-east.roomler.ai/derp"`. Absent = the region has no
    /// regional DERP; pairs fall back to the central `/derp`.
    #[serde(default)]
    pub derp_url: Option<String>,
    /// Per-region coturn `static-auth-secret`; falls back to the global
    /// `turn.shared_secret` when absent (one secret fleet-wide, v1).
    #[serde(default)]
    pub shared_secret: Option<String>,
    /// Which transport variants this PoP serves (absent fields = true).
    #[serde(default)]
    pub caps: crate::turn_url::VariantCaps,
    /// Parsed-but-disabled regions never issue (procurement staging).
    #[serde(default = "spec_enabled_default")]
    pub enabled: bool,
}

fn spec_enabled_default() -> bool {
    true
}

/// Region-keyed TURN issuance. `default` is the legacy single-region
/// `turn.*` config; `regions` are the PoPs from `ROOMLER__RELAY__REGIONS`.
/// With `enabled == false` (the default) `cfg_for` ALWAYS answers `default`,
/// so every pre-region call path is provably unchanged.
#[derive(Debug, Clone, Default)]
pub struct TurnMap {
    pub default: Option<TurnConfig>,
    pub regions: std::collections::HashMap<String, TurnConfig>,
    /// The parsed specs (DERP endpoints, caps) for the wire push (PR3) and
    /// the regional-DERP side (PR4).
    pub specs: Vec<RelayRegionSpec>,
    /// `relay.regions_enabled` — the master gate.
    pub enabled: bool,
}

impl TurnMap {
    /// Legacy single-region map (tests + the pre-region constructor path).
    pub fn single(default: Option<TurnConfig>) -> Self {
        Self {
            default,
            ..Self::default()
        }
    }

    /// The issuing config for `region`: the region's own config when the map
    /// is enabled and knows it; the legacy default otherwise (unknown region
    /// ids and `None` both degrade safely).
    pub fn cfg_for(&self, region: Option<&str>) -> Option<&TurnConfig> {
        if self.enabled
            && let Some(r) = region
            && let Some(cfg) = self.regions.get(r)
        {
            return Some(cfg);
        }
        self.default.as_ref()
    }
}

/// Build the [`crate::signaling::ServerMsg::RelayRegions`] payload: one probe
/// target per enabled+usable region. `None` when the map is disabled or
/// nothing is probe-able — the capability-flagged agent then simply never
/// probes. `rev` is FNV-1a over the serialized list: deterministic across
/// pods/processes (both build from the same env), so an agent only re-probes
/// when the region set actually changes.
pub fn relay_regions_wire(map: &TurnMap) -> Option<(Vec<crate::signaling::RelayRegionInfo>, u64)> {
    if !map.enabled {
        return None;
    }
    let mut out = Vec::new();
    for spec in &map.specs {
        if !spec.enabled || !map.regions.contains_key(&spec.id) {
            continue;
        }
        let Some((host, port)) = crate::turn_url::host_port(&spec.turn_url) else {
            continue;
        };
        out.push(crate::signaling::RelayRegionInfo {
            id: spec.id.clone(),
            stun: format!("{host}:{port}"),
            derp_url: spec.derp_url.clone(),
        });
    }
    if out.is_empty() {
        return None;
    }
    let json = serde_json::to_string(&out).unwrap_or_default();
    Some((out, fnv1a64(json.as_bytes())))
}

/// FNV-1a 64-bit — stable across processes (unlike `RandomState` hashing),
/// which is all `rev` needs.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// One region's live load snapshot, written by the API's `/stats` poller and
/// consulted at issuance time. Everything here is advisory: a missing or
/// STALE entry means "not busy" (fail-open — a stats blip must never
/// mass-reroute traffic off a healthy PoP).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RegionLoad {
    pub busy: bool,
    pub load1: f64,
    pub tx_mbps: f64,
    pub coturn_allocations: f64,
    /// Unix seconds of the sample; entries older than
    /// [`REGION_LOAD_FRESH_SECS`] are ignored.
    pub updated_unix: u64,
}

/// How long a load sample stays authoritative (3 missed 30 s polls).
pub const REGION_LOAD_FRESH_SECS: u64 = 120;

/// Shared region-load table: region id → latest snapshot.
pub type RelayLoadMap = std::sync::Arc<dashmap::DashMap<String, RegionLoad>>;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Is `region` currently marked busy by a FRESH sample?
pub fn region_busy(load: &RelayLoadMap, region: &str) -> bool {
    load.get(region)
        .map(|l| l.busy && now_unix().saturating_sub(l.updated_unix) <= REGION_LOAD_FRESH_SECS)
        .unwrap_or(false)
}

/// Load-aware single-node region pick (RC sessions / tunnel): the agent's
/// nearest region unless it's busy, then the agent's next-nearest non-busy
/// region from `prefs` (its RTT-ordered probe results). Every region busy →
/// the nearest anyway (nearest-busy still beats far-and-idle for latency,
/// and thresholds are advisory). `None` = the default region.
pub fn pick_region_with_load(
    map: &TurnMap,
    load: &RelayLoadMap,
    home: Option<&str>,
    prefs: &[String],
) -> Option<String> {
    if !map.enabled {
        return None;
    }
    let home = home.filter(|r| map.regions.contains_key(*r))?;
    if !region_busy(load, home) {
        return Some(home.to_string());
    }
    prefs
        .iter()
        .find(|r| map.regions.contains_key(*r) && !region_busy(load, r))
        .map(|r| r.to_string())
        .or(Some(home.to_string()))
}

/// Pick the relay region for a PAIR from the two ends' `relay_home`s.
/// Interim policy until RTT tables land: agreement wins; a single known home
/// wins over none; two DIFFERENT homes fall to a deterministic FNV pick over
/// the pair key (either end's PoP beats the default-region hairpin for a
/// same-continent pair, and the broker computes this once so both grants
/// agree). `None` = use the default region.
pub fn select_pair_region(
    map: &TurnMap,
    load: &RelayLoadMap,
    home_a: Option<&str>,
    home_b: Option<&str>,
    pair_key: &str,
) -> Option<String> {
    if !map.enabled {
        return None;
    }
    let known_a = home_a.filter(|r| map.regions.contains_key(*r));
    let known_b = home_b.filter(|r| map.regions.contains_key(*r));
    let chosen = match (known_a, known_b) {
        (Some(a), Some(b)) if a == b => Some(a.to_string()),
        (Some(a), Some(b)) => {
            // Sorted so the pick is symmetric in argument order.
            let mut pair = [a, b];
            pair.sort_unstable();
            pick_index_fnv1a(pair_key, 2).map(|i| pair[i].to_string())
        }
        (Some(one), None) | (None, Some(one)) => Some(one.to_string()),
        (None, None) => None,
    }?;
    // Load-aware fallback for pairs: a busy pick moves to the OTHER end's
    // non-busy home, else to the default region (`None`) — per-node RTT
    // tables aren't available on this path, so "next closest" degrades to
    // "the other end's closest, then central".
    if region_busy(load, &chosen) {
        let other = [known_a, known_b]
            .into_iter()
            .flatten()
            .find(|r| *r != chosen && !region_busy(load, r));
        return other.map(|r| r.to_string());
    }
    Some(chosen)
}

/// Convenience: also include public STUN servers so trickle ICE has something
/// to work with even before TURN auth completes.
pub fn ice_servers_for(user_id: &str, turn: Option<&TurnConfig>) -> Vec<IceServer> {
    let mut out = vec![IceServer {
        urls: vec!["stun:stun.l.google.com:19302".into()],
        username: None,
        credential: None,
    }];
    if let Some(t) = turn {
        out.push(t.issue(user_id));
    }
    out
}

/// Session-aware variant of [`ice_servers_for`] — used by the Hub on the two
/// per-session issuance paths (`Ready` → controller, `SdpOffer` → agent) so
/// both sides receive the SAME worker-first TURN ordering for one session.
pub fn ice_servers_for_session(
    user_id: &str,
    session_key: &str,
    turn: Option<&TurnConfig>,
) -> Vec<IceServer> {
    let mut out = vec![IceServer {
        urls: vec!["stun:stun.l.google.com:19302".into()],
        username: None,
        credential: None,
    }];
    if let Some(t) = turn {
        out.push(t.issue_for_session(user_id, session_key));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn affinity_cfg() -> TurnConfig {
        TurnConfig {
            urls: vec!["turn:coturn.example:3478".into()],
            workers: vec![
                vec!["turn:coturn-1.example:3478".into()],
                vec!["turn:coturn-2.example:3478".into()],
                vec!["turn:coturn-3.example:3478".into()],
            ],
            shared_secret: "topsecret".into(),
            ttl_secs: 600,
        }
    }

    #[test]
    fn issue_for_session_is_deterministic_and_worker_first() {
        let cfg = affinity_cfg();
        // Same session key → same worker, on every call (this is what makes
        // BOTH sides of one session land on the same coturn worker).
        let a = cfg.issue_for_session("controller", "6a54bf440b4fd609a7356f97");
        let b = cfg.issue_for_session("agent", "6a54bf440b4fd609a7356f97");
        assert_eq!(a.urls[0], b.urls[0], "both sides must pick the same worker");
        assert!(a.urls[0].contains("coturn-"), "worker URL must come first");
        // Generic hostname retained as the fallback tail.
        assert_eq!(a.urls.last().unwrap(), "turn:coturn.example:3478");
    }

    /// worker-pick golden vector — the exact literals pinned in
    /// `worker_pick::tests::worker_pick_golden_vector` (invariant I6),
    /// re-asserted through this consumer's call path.
    #[test]
    fn session_worker_pick_matches_golden_vector() {
        let cfg = affinity_cfg();
        // FNV-1a("6a54bf440b4fd609a7356f97") % 3 == 0 → worker 1 first.
        let s = cfg.issue_for_session("u", "6a54bf440b4fd609a7356f97");
        assert_eq!(s.urls[0], "turn:coturn-1.example:3478");
    }

    #[test]
    fn issue_for_session_spreads_across_workers() {
        let cfg = affinity_cfg();
        // Different sessions should not all land on one worker.
        let mut seen = std::collections::HashSet::new();
        for i in 0..32 {
            let s = cfg.issue_for_session("u", &format!("session-{i}"));
            seen.insert(s.urls[0].clone());
        }
        assert!(seen.len() >= 2, "hash must spread sessions across workers");
    }

    #[test]
    fn issue_for_session_without_workers_matches_issue() {
        let cfg = TurnConfig {
            urls: vec!["turn:coturn.example:3478".into()],
            workers: vec![],
            shared_secret: "topsecret".into(),
            ttl_secs: 600,
        };
        let s = cfg.issue_for_session("u1", "any-session");
        assert_eq!(s.urls, vec!["turn:coturn.example:3478".to_string()]);
    }

    #[test]
    fn issues_well_formed_creds() {
        let cfg = TurnConfig {
            urls: vec!["turn:coturn.example:3478".into()],
            workers: vec![],
            shared_secret: "topsecret".into(),
            ttl_secs: 600,
        };
        let cred = cfg.issue("user_42");
        let username = cred.username.unwrap();
        let mut parts = username.splitn(2, ':');
        let expiry: u64 = parts.next().unwrap().parse().unwrap();
        let uid = parts.next().unwrap();
        assert_eq!(uid, "user_42");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(expiry > now && expiry <= now + 600);
        assert!(cred.credential.unwrap().len() >= 20);
    }

    #[test]
    fn ice_list_includes_stun() {
        let list = ice_servers_for("u1", None);
        assert!(list[0].urls[0].starts_with("stun:"));
    }

    fn region_cfg(host: &str) -> TurnConfig {
        TurnConfig {
            urls: vec![format!("turn:{host}:3478")],
            workers: vec![],
            shared_secret: "topsecret".into(),
            ttl_secs: 600,
        }
    }

    fn no_load() -> RelayLoadMap {
        std::sync::Arc::new(dashmap::DashMap::new())
    }

    fn load_with(region: &str, busy: bool, age_s: u64) -> RelayLoadMap {
        let m = no_load();
        m.insert(
            region.to_string(),
            RegionLoad {
                busy,
                load1: 3.0,
                tx_mbps: 450.0,
                coturn_allocations: 10.0,
                updated_unix: now_unix().saturating_sub(age_s),
            },
        );
        m
    }

    fn two_region_map(enabled: bool) -> TurnMap {
        let mut regions = std::collections::HashMap::new();
        regions.insert("us-east".to_string(), region_cfg("coturn-us-east.example"));
        regions.insert("eu-west".to_string(), region_cfg("coturn-eu-west.example"));
        TurnMap {
            default: Some(region_cfg("coturn.example")),
            regions,
            specs: vec![],
            enabled,
        }
    }

    /// The flag-off no-op guarantee: a DISABLED map answers the legacy
    /// default for every input, including a known region id.
    #[test]
    fn disabled_map_always_answers_default() {
        let map = two_region_map(false);
        for region in [None, Some("us-east"), Some("nope")] {
            let cfg = map.cfg_for(region).expect("default present");
            assert_eq!(cfg.urls[0], "turn:coturn.example:3478");
        }
        assert_eq!(
            select_pair_region(&map, &no_load(), Some("us-east"), Some("us-east"), "k"),
            None
        );
    }

    #[test]
    fn enabled_map_resolves_regions_and_degrades() {
        let map = two_region_map(true);
        assert_eq!(
            map.cfg_for(Some("us-east")).unwrap().urls[0],
            "turn:coturn-us-east.example:3478"
        );
        // Unknown region + None both degrade to the default.
        assert_eq!(
            map.cfg_for(Some("mars")).unwrap().urls[0],
            "turn:coturn.example:3478"
        );
        assert_eq!(
            map.cfg_for(None).unwrap().urls[0],
            "turn:coturn.example:3478"
        );
    }

    #[test]
    fn pair_region_agreement_and_fallbacks() {
        let map = two_region_map(true);
        // Agreement.
        assert_eq!(
            select_pair_region(&map, &no_load(), Some("us-east"), Some("us-east"), "k").as_deref(),
            Some("us-east")
        );
        // One known home wins over none / over an UNKNOWN home.
        assert_eq!(
            select_pair_region(&map, &no_load(), Some("eu-west"), None, "k").as_deref(),
            Some("eu-west")
        );
        assert_eq!(
            select_pair_region(&map, &no_load(), Some("gone"), Some("eu-west"), "k").as_deref(),
            Some("eu-west")
        );
        // No homes → default region.
        assert_eq!(select_pair_region(&map, &no_load(), None, None, "k"), None);
        // Differing homes: deterministic AND symmetric in argument order.
        let ab = select_pair_region(&map, &no_load(), Some("us-east"), Some("eu-west"), "pair-1");
        let ba = select_pair_region(&map, &no_load(), Some("eu-west"), Some("us-east"), "pair-1");
        assert_eq!(ab, ba);
        assert!(ab.as_deref() == Some("us-east") || ab.as_deref() == Some("eu-west"));
    }

    /// Load-aware picks: fresh-busy home falls to the next non-busy pref;
    /// stale-busy is IGNORED (fail-open); all-busy keeps the nearest.
    #[test]
    fn load_aware_pick_and_freshness() {
        let map = two_region_map(true);
        let prefs = vec!["us-east".to_string(), "eu-west".to_string()];

        // Not busy → home.
        assert_eq!(
            pick_region_with_load(&map, &no_load(), Some("us-east"), &prefs).as_deref(),
            Some("us-east")
        );
        // Fresh busy home → next non-busy pref.
        let busy = load_with("us-east", true, 0);
        assert_eq!(
            pick_region_with_load(&map, &busy, Some("us-east"), &prefs).as_deref(),
            Some("eu-west")
        );
        // STALE busy sample → ignored (fail-open).
        let stale = load_with("us-east", true, REGION_LOAD_FRESH_SECS + 30);
        assert_eq!(
            pick_region_with_load(&map, &stale, Some("us-east"), &prefs).as_deref(),
            Some("us-east")
        );
        // Every candidate busy → nearest anyway.
        let all = load_with("us-east", true, 0);
        all.insert(
            "eu-west".into(),
            RegionLoad {
                busy: true,
                load1: 9.0,
                tx_mbps: 480.0,
                coturn_allocations: 99.0,
                updated_unix: now_unix(),
            },
        );
        assert_eq!(
            pick_region_with_load(&map, &all, Some("us-east"), &prefs).as_deref(),
            Some("us-east")
        );
        // Disabled map / unknown home → default region.
        assert_eq!(
            pick_region_with_load(&two_region_map(false), &no_load(), Some("us-east"), &prefs),
            None
        );
        assert_eq!(
            pick_region_with_load(&map, &no_load(), Some("mars"), &prefs),
            None
        );

        // Pair fallback: busy pick moves to the other end's home, else default.
        let busy_us = load_with("us-east", true, 0);
        assert_eq!(
            select_pair_region(&map, &busy_us, Some("us-east"), Some("eu-west"), "k").as_deref(),
            Some("eu-west")
        );
        assert_eq!(
            select_pair_region(&map, &busy_us, Some("us-east"), Some("us-east"), "k"),
            None,
            "both ends on the busy region → default"
        );
    }

    #[test]
    fn relay_regions_wire_builds_probe_targets() {
        let spec = |id: &str, url: &str, enabled: bool| RelayRegionSpec {
            id: id.into(),
            turn_url: url.into(),
            worker_urls: vec![],
            derp_url: Some(format!("wss://derp-{id}.example/derp")),
            shared_secret: None,
            caps: Default::default(),
            enabled,
        };
        let mut map = two_region_map(true);
        map.specs = vec![
            spec("us-east", "turn:coturn-us-east.example:3478", true),
            // Disabled spec: parsed but never pushed.
            spec("eu-west", "turn:coturn-eu-west.example:3478", false),
            // Spec without a compiled region (e.g. no secret): never pushed.
            spec("ghost", "turn:ghost.example:3478", true),
        ];
        let (regions, rev) = relay_regions_wire(&map).expect("one probe target");
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].id, "us-east");
        assert_eq!(regions[0].stun, "coturn-us-east.example:3478");
        // rev is stable across calls (deterministic hash).
        assert_eq!(relay_regions_wire(&map).unwrap().1, rev);
        // Master flag off ⇒ nothing is ever pushed.
        map.enabled = false;
        assert!(relay_regions_wire(&map).is_none());
    }

    #[test]
    fn region_spec_parses_with_defaults() {
        let spec: RelayRegionSpec = serde_json::from_str(
            r#"{"id":"us-east","turn_url":"turn:coturn-us-east.roomler.ai:3478",
                "derp_url":"wss://derp-us-east.roomler.ai/derp",
                "caps":{"tls_443_tcp":false}}"#,
        )
        .unwrap();
        assert_eq!(spec.id, "us-east");
        assert!(spec.enabled);
        assert!(spec.worker_urls.is_empty());
        assert!(spec.shared_secret.is_none());
        assert!(!spec.caps.tls_443_tcp);
        assert!(spec.caps.udp_443);
    }
}

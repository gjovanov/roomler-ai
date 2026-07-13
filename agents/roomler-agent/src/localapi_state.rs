//! The agent daemon's [`LocalApiState`] — the live data the LocalAPI serves.
//!
//! Thin adapter (unification P1): turns the agent's enrolled identity + a
//! "connected" flag + the overlay runtime's published [`OverlayView`] into the
//! read-only `status` / `peers` / `flows` that the CLI (`roomler`) and the
//! desktop app read over the local pipe/socket (`tunnel_core::localapi::serve`).
//!
//! Wired in `run_cmd`: the connected flag and the overlay `watch` channel are
//! created there (stable across WS reconnects), the signaling loop updates them,
//! and the listener reads this state.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::watch;
use tunnel_core::localapi::{
    ConnectionType, ConsentRequest, DaemonMode, FlowInfo, LocalApiState, NodeStatus, OverlayView,
    PeerInfo, Response,
};

/// How often the RTT prober ICMP-pings each carrier-reachable peer (P3b-3).
pub const RTT_PROBE_INTERVAL: Duration = Duration::from_secs(15);
/// A cached RTT older than this is dropped from the peer view (the peer stopped
/// answering) so the column fades to "—" rather than showing a stale number.
/// ~3 missed probes.
pub const RTT_STALE: Duration = Duration::from_secs(45);
/// Per-peer ICMP probe timeout — short so one unreachable peer can't stretch the
/// sequential cycle past [`RTT_PROBE_INTERVAL`].
const RTT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Shared RTT cache: overlay-ip → (last measured RTT ms, when measured). Written
/// by the prober task, read by [`DaemonState::peers`].
pub type RttCache = Arc<Mutex<HashMap<String, (u32, Instant)>>>;

/// The netstack ICMP backend behind the `ping` verb, abstracted so
/// [`DaemonState`] never names the feature-gated `NetstackHandle` type. The
/// concrete impl lives in `crate::overlay` (feature `overlay-netstack`); `None`
/// on any node not running the userspace stack.
#[async_trait]
pub trait NetstackPinger: Send + Sync {
    /// ICMP-ping `dst` (already resolved, either family) over the netstack;
    /// `Ok(rtt)` on reply.
    async fn ping(&self, dst: IpAddr, timeout: Duration) -> Result<Duration, String>;
}

/// Live daemon state behind the LocalAPI. Built once in `run_cmd`, wrapped in an
/// `Arc<dyn LocalApiState>` for the listener; reads are cheap clones off a
/// `watch` borrow + an atomic load.
pub struct DaemonState {
    node_id: String,
    name: String,
    version: String,
    mode: DaemonMode,
    tenant_id: Option<String>,
    /// Set while a signaling WS connection is live (updated by the signaling
    /// loop's per-connection guard). `peers()` reports none when this is false,
    /// since the overlay carriers are torn down on WS drop and the last view is
    /// stale.
    connected: Arc<AtomicBool>,
    /// The overlay runtime's latest view. An empty `Default` when overlay is
    /// disabled or this build lacks `overlay-l3` (nothing publishes) — so
    /// `peers()` is simply empty there.
    overlay: watch::Receiver<OverlayView>,
    /// The SAME consent broker the signaling loop prompts on (injected from
    /// `run_cmd`) — so `consent_decide` gates on the LIVE pending set and reads
    /// the broker's own sentinel dir, rather than a throwaway broker over a
    /// re-resolved path.
    consent: crate::consent::ConsentBroker,
    /// The netstack ICMP backend for the `ping` verb. `None` on a node not
    /// running the userspace stack (OS-TUN or non-overlay build).
    pinger: Option<Arc<dyn NetstackPinger>>,
    /// The tunnel-client hub (P3b-2 PR-C) — the same instance the signaling loop
    /// publishes its egress into. Backs the `flows` / `create_forward` /
    /// `create_socks5` / `kill_flow` verbs; the daemon originates tunnels over
    /// its own agent WS.
    tunnel_hub: crate::tunnel::client_mgr::TunnelClientHub,
    /// Per-peer RTT cache filled by the ICMP prober task (P3b-3), read by
    /// `peers()` to populate `rtt_ms`. Empty on a node without the netstack
    /// pinger (no prober runs) → `rtt_ms` stays `None`.
    rtt_cache: RttCache,
}

impl DaemonState {
    /// Build from the enrolled config identity + the live handles. `mode` is the
    /// privilege the daemon runs at (today's agent is always the full "be
    /// accessed" service node → [`DaemonMode::Service`]; the unprivileged
    /// user-mode daemon arrives with the binary unification at P3).
    #[allow(clippy::too_many_arguments)] // a daemon-state constructor; grouping would obscure
    pub fn new(
        node_id: String,
        name: String,
        mode: DaemonMode,
        tenant_id: Option<String>,
        connected: Arc<AtomicBool>,
        overlay: watch::Receiver<OverlayView>,
        consent: crate::consent::ConsentBroker,
        pinger: Option<Arc<dyn NetstackPinger>>,
        tunnel_hub: crate::tunnel::client_mgr::TunnelClientHub,
        rtt_cache: RttCache,
    ) -> Self {
        Self {
            node_id,
            name,
            version: env!("CARGO_PKG_VERSION").to_string(),
            mode,
            tenant_id,
            connected,
            overlay,
            consent,
            pinger,
            tunnel_hub,
            rtt_cache,
        }
    }

    /// Resolve a `ping` target — a literal overlay IP (either family) or a peer
    /// **name** — against the live mesh view. Mirrors the netstack SOCKS
    /// front's resolver (bare label / first DNS label), but reads the view
    /// `DaemonState` holds. A name resolves to the peer's IPv4 by default, or
    /// its *derived* overlay IPv6 (published by the runtime) with `prefer_v6`;
    /// a literal is used as-is (an unroutable v6 fails cleanly at the send).
    fn resolve_overlay(&self, target: &str, prefer_v6: bool) -> Option<IpAddr> {
        if let Ok(ip) = target.parse::<IpAddr>() {
            return Some(ip);
        }
        let tl = target.to_ascii_lowercase();
        let bare = tl.split('.').next().unwrap_or(&tl).to_string();
        self.overlay.borrow().peers.iter().find_map(|p| {
            let n = p.name.to_ascii_lowercase();
            if !p.name.is_empty() && (n == tl || n == bare) {
                let v4 = p.overlay_ip.as_deref();
                let pick = if prefer_v6 {
                    // Fall back to v4 if no published v6 (shouldn't happen —
                    // the runtime derives one whenever the v4 exists).
                    p.overlay_ip6.as_deref().or(v4)
                } else {
                    v4
                };
                pick.and_then(|s| s.parse::<IpAddr>().ok())
            } else {
                None
            }
        })
    }
}

#[async_trait]
impl LocalApiState for DaemonState {
    fn status(&self) -> NodeStatus {
        NodeStatus {
            node_id: self.node_id.clone(),
            name: self.name.clone(),
            version: self.version.clone(),
            mode: self.mode,
            tenant_id: self.tenant_id.clone(),
            // The overlay IP the runtime last assigned us — a stable identity,
            // so it's kept even across a brief disconnect.
            overlay_ip: self.overlay.borrow().self_ip.clone(),
            overlay_ip6: self.overlay.borrow().self_ip6.clone(),
            connected: self.connected.load(Ordering::Relaxed),
        }
    }

    fn peers(&self) -> Vec<PeerInfo> {
        // A peer list from a dropped connection is misleading — the carriers are
        // gone. Report none until reconnected + re-synced.
        if !self.connected.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let mut peers = self.overlay.borrow().peers.clone();
        // P3b-3: overlay a peer as `Tunnel` when its WG carrier is down and the
        // daemon reaches its backing agent via a live tunnel flow.
        apply_tunnel_override(&mut peers, &self.tunnel_hub.active_flow_agent_ids());
        // P3b-3: fill `rtt_ms` from the ICMP prober cache (netstack nodes only).
        // A stale entry (peer stopped answering) is dropped so the column fades
        // to "—" rather than lying. Empty cache (no prober) → all stay `None`.
        if let Ok(cache) = self.rtt_cache.lock()
            && !cache.is_empty()
        {
            for p in &mut peers {
                if let Some(ip) = p.overlay_ip.as_deref()
                    && let Some((ms, at)) = cache.get(ip)
                    && at.elapsed() < RTT_STALE
                {
                    p.rtt_ms = Some(*ms);
                }
            }
        }
        peers
    }

    fn flows(&self) -> Vec<FlowInfo> {
        // P3b-2 PR-C: the tunnel data plane folded into the daemon — report the
        // supervised forwards / SOCKS5 listeners it originates over its agent WS.
        self.tunnel_hub.flows_snapshot()
    }

    fn consent_pending(&self) -> Vec<ConsentRequest> {
        // Read the broker's OWN sentinel dir — resolved in-process, so it's the
        // daemon's real profile even under SystemContext, where the interactive-
        // user tray reading the dir directly would look in the WRONG profile (the
        // P2b bug fix). Same parse the tray's cmd_get_pending_consents used to do.
        let Ok(entries) = std::fs::read_dir(self.consent.sentinel_dir()) else {
            return Vec::new(); // dir not created yet ⇒ nothing pending
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("pending") {
                continue;
            }
            if let Ok(body) = std::fs::read_to_string(&path)
                && let Ok(pc) = serde_json::from_str::<ConsentRequest>(&body)
            {
                out.push(pc);
            }
        }
        out
    }

    fn consent_decide(&self, session_id: &str, allow: bool) -> bool {
        // SECURITY: the session id becomes a sentinel FILE NAME, so reject
        // anything that isn't a 24-char hex ObjectId before it's used (path-
        // traversal / injection guard). The pipe SDDL already limits WHO can call
        // this (SYSTEM + Administrators + interactive user, ≥ medium integrity).
        if !is_hex_object_id(session_id) {
            tracing::warn!(
                session = %session_id,
                "localapi: rejecting consent decision — session id is not a 24-char hex ObjectId"
            );
            return false;
        }
        // Record via the LIVE broker: honored ONLY if the session is actively
        // being prompted (no pre-approval / confused-deputy — the decision is an
        // answer to a question the broker is currently asking).
        self.consent.record_decision(session_id, allow)
    }

    async fn ping(&self, target: &str, timeout_ms: u64, prefer_v6: bool) -> Response {
        let Some(pinger) = self.pinger.clone() else {
            return Response::Error {
                message: "ping requires netstack mode (this node isn't running the userspace \
                          stack)"
                    .into(),
            };
        };
        let Some(ip) = self.resolve_overlay(target, prefer_v6) else {
            return Response::Error {
                message: format!("no overlay peer named '{target}' — try an overlay IP or `peers`"),
            };
        };
        let timeout = Duration::from_millis(if timeout_ms == 0 { 3000 } else { timeout_ms });
        match pinger.ping(ip, timeout).await {
            Ok(rtt) => Response::Pong {
                target: target.to_string(),
                overlay_ip: ip.to_string(),
                rtt_micros: rtt.as_micros() as u64,
            },
            Err(message) => Response::Error { message },
        }
    }

    async fn create_forward(
        &self,
        node: &str,
        local: u16,
        remote: &str,
        transport: &str,
    ) -> Response {
        match self
            .tunnel_hub
            .create_forward(node, local, remote, transport)
            .await
        {
            Ok(id) => Response::FlowCreated { id },
            Err(message) => Response::Error { message },
        }
    }

    async fn create_socks5(&self, node: &str, local: u16, transport: &str) -> Response {
        match self.tunnel_hub.create_socks5(node, local, transport).await {
            Ok(id) => Response::FlowCreated { id },
            Err(message) => Response::Error { message },
        }
    }

    fn kill_flow(&self, id: &str) -> bool {
        self.tunnel_hub.kill_flow(id)
    }
}

/// Overlay [`ConnectionType::Tunnel`] onto every peer whose WG carrier is down
/// (`Blocked`/`Offline`) but whose backing `agent_id` is in `tunneled` — the set
/// of agent ids reached by a live daemon tunnel flow (P3b-3). Pure so the
/// precedence (Direct > Relay > **Tunnel** > Blocked > Offline — Tunnel never
/// masks a live WG carrier) is unit-tested without a hub. No-op on empty
/// `tunneled`.
fn apply_tunnel_override(peers: &mut [PeerInfo], tunneled: &HashSet<String>) {
    if tunneled.is_empty() {
        return;
    }
    for p in peers.iter_mut() {
        let gap = matches!(
            p.connection,
            ConnectionType::Blocked | ConnectionType::Offline
        );
        if gap && p.agent_id.as_deref().is_some_and(|a| tunneled.contains(a)) {
            p.connection = ConnectionType::Tunnel;
        }
    }
}

/// Spawn the RTT prober (P3b-3): every [`RTT_PROBE_INTERVAL`], ICMP-ping each
/// carrier-reachable peer over the userspace netstack and cache the round-trip
/// so `peers()` can surface `rtt_ms`. Only meaningful on a netstack node (the
/// caller spawns it only when a `pinger` exists); the cache stays empty
/// otherwise and every peer's `rtt_ms` is `None`.
///
/// Probes **only** `Direct`/`Relay` peers — a `Blocked`/`Offline` peer has no
/// working carrier, so a ping just burns the full timeout and would stretch the
/// sequential cycle. Pings sequentially so a burst of ICMP never hits the wire
/// at once; the worst-case cycle is (live-peer-count × [`RTT_PROBE_TIMEOUT`]),
/// comfortably under the interval for realistic meshes. Exits on `shutdown`.
pub fn spawn_rtt_prober(
    pinger: Arc<dyn NetstackPinger>,
    overlay: watch::Receiver<OverlayView>,
    cache: RttCache,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(RTT_PROBE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                _ = tick.tick() => {}
            }
            // Snapshot the peers to probe, then release the watch borrow before
            // any await (the borrow is not held across the ping).
            let targets: Vec<(String, IpAddr)> = overlay
                .borrow()
                .peers
                .iter()
                .filter(|p| matches!(p.connection, ConnectionType::Direct | ConnectionType::Relay))
                .filter_map(|p| {
                    let ip = p.overlay_ip.as_deref()?;
                    ip.parse::<IpAddr>().ok().map(|addr| (ip.to_string(), addr))
                })
                .collect();
            for (key, ip) in targets {
                if let Ok(rtt) = pinger.ping(ip, RTT_PROBE_TIMEOUT).await {
                    let ms = rtt.as_millis().min(u32::MAX as u128) as u32;
                    if let Ok(mut c) = cache.lock() {
                        c.insert(key, (ms, Instant::now()));
                    }
                }
            }
        }
    });
}

/// A 24-char hex ObjectId — the only shape a session id may take before it's
/// used as a sentinel filename. Guards [`DaemonState::consent_decide`] against a
/// caller smuggling path separators / traversal into the filename.
fn is_hex_object_id(s: &str) -> bool {
    s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tunnel_core::localapi::ConnectionType;

    fn view() -> OverlayView {
        OverlayView {
            self_ip: Some("100.64.0.2".into()),
            self_ip6: Some("fd72:6f6f:6d6c::6440:2".into()),
            peers: vec![PeerInfo {
                node_id: "n2".into(),
                name: "peer".into(),
                overlay_ip: Some("100.64.0.1".into()),
                overlay_ip6: Some("fd72:6f6f:6d6c::6440:1".into()),
                online: true,
                connection: ConnectionType::Relay,
                rtt_ms: None,
                last_seen_ms: None,
                agent_id: None,
            }],
        }
    }

    fn consent_broker(tag: &str) -> crate::consent::ConsentBroker {
        crate::consent::ConsentBroker::new(
            crate::consent::Mode::AutoGrant,
            std::env::temp_dir().join(format!("roomler-las-consent-{tag}-{}", std::process::id())),
        )
        .unwrap()
    }

    fn empty_rtt_cache() -> RttCache {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[tokio::test]
    async fn resolve_targets_and_ping_without_pinger_errors() {
        let (_tx, rx) = watch::channel(view());
        let st = DaemonState::new(
            "aid".into(),
            "host".into(),
            DaemonMode::Service,
            None,
            Arc::new(AtomicBool::new(true)),
            rx,
            consent_broker("ping"),
            None, // no netstack pinger
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            empty_rtt_cache(),
        );
        // Resolve by peer name (from `view`), by first label, and by literal IP.
        assert_eq!(
            st.resolve_overlay("peer", false),
            Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)))
        );
        assert_eq!(
            st.resolve_overlay("PEER.myorg.roomler.net", false),
            Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)))
        );
        assert_eq!(
            st.resolve_overlay("100.64.0.9", false),
            Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 9)))
        );
        // prefer_v6 picks the runtime-published derived v6 for a NAME target…
        assert_eq!(
            st.resolve_overlay("peer", true),
            Some("fd72:6f6f:6d6c::6440:1".parse().unwrap())
        );
        // …and a literal v6 target is accepted as-is.
        assert_eq!(
            st.resolve_overlay("fd72:6f6f:6d6c::6440:9", false),
            Some("fd72:6f6f:6d6c::6440:9".parse().unwrap())
        );
        assert_eq!(st.resolve_overlay("ghost", false), None);
        // With no pinger (not a netstack node) `ping` is a clean Error, not a Pong.
        assert!(matches!(
            st.ping("peer", 0, false).await,
            Response::Error { .. }
        ));
    }

    #[test]
    fn consent_decide_hex_guard_rejects_unsafe_session_ids() {
        // The guard fires BEFORE any filesystem write, so a bad id is a pure
        // no-op — traversal / separators / wrong-length are all rejected.
        assert!(is_hex_object_id("0123456789abcdef01234567"));
        assert!(is_hex_object_id("6A11682E804368D30EDF57C6")); // upper-case hex ok
        assert!(!is_hex_object_id("6a11682e804368d30edf57c")); // 23 chars
        assert!(!is_hex_object_id("6a11682e804368d30edf57c6z")); // 25 / non-hex
        assert!(!is_hex_object_id("../../etc/passwd"));
        assert!(!is_hex_object_id("6a11682e804368d30edf57c6.approve"));
        assert!(!is_hex_object_id(""));
    }

    #[test]
    fn status_and_peers_track_connected_flag() {
        let connected = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = watch::channel(view());
        let consent = crate::consent::ConsentBroker::new(
            crate::consent::Mode::AutoGrant,
            std::env::temp_dir().join(format!("roomler-las-consent-{}", std::process::id())),
        )
        .unwrap();
        let st = DaemonState::new(
            "aid".into(),
            "host".into(),
            DaemonMode::Service,
            Some("tid".into()),
            connected.clone(),
            rx,
            consent,
            None,
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            empty_rtt_cache(),
        );

        // Identity + overlay IP are always reported; connected reflects the flag.
        let s = st.status();
        assert_eq!(s.node_id, "aid");
        assert_eq!(s.name, "host");
        assert_eq!(s.tenant_id.as_deref(), Some("tid"));
        assert_eq!(s.overlay_ip.as_deref(), Some("100.64.0.2"));
        assert!(!s.connected);

        // Peers hidden while disconnected…
        assert!(st.peers().is_empty(), "no peers reported while WS is down");

        // …shown once connected.
        connected.store(true, Ordering::Relaxed);
        assert!(st.status().connected);
        let peers = st.peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].connection, ConnectionType::Relay);

        // Flows are empty on the agent side in P1.
        assert!(st.flows().is_empty());
    }

    #[test]
    fn peers_fill_rtt_from_fresh_cache_and_drop_stale() {
        // The `view()` peer is a Relay with overlay_ip 100.64.0.1.
        let (_tx, rx) = watch::channel(view());
        let cache = empty_rtt_cache();
        let st = DaemonState::new(
            "aid".into(),
            "host".into(),
            DaemonMode::Service,
            None,
            Arc::new(AtomicBool::new(true)),
            rx,
            consent_broker("rtt"),
            None,
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            cache.clone(),
        );

        // Fresh cache entry → surfaced as rtt_ms.
        cache
            .lock()
            .unwrap()
            .insert("100.64.0.1".into(), (52, Instant::now()));
        assert_eq!(st.peers()[0].rtt_ms, Some(52));

        // Stale entry (older than RTT_STALE) → dropped to None (fade to "—").
        let stale_at = Instant::now()
            .checked_sub(RTT_STALE + Duration::from_secs(1))
            .unwrap();
        cache
            .lock()
            .unwrap()
            .insert("100.64.0.1".into(), (52, stale_at));
        assert_eq!(st.peers()[0].rtt_ms, None);

        // Empty cache → None.
        cache.lock().unwrap().clear();
        assert_eq!(st.peers()[0].rtt_ms, None);
    }

    #[test]
    fn tunnel_override_only_fills_carrier_gaps_and_respects_precedence() {
        fn peer(agent: Option<&str>, conn: ConnectionType) -> PeerInfo {
            PeerInfo {
                node_id: "n".into(),
                name: "p".into(),
                overlay_ip: None,
                overlay_ip6: None,
                online: true,
                connection: conn,
                rtt_ms: None,
                last_seen_ms: None,
                agent_id: agent.map(|s| s.into()),
            }
        }
        let tunneled: HashSet<String> = ["aid-1".to_string()].into_iter().collect();
        let mut peers = vec![
            peer(Some("aid-1"), ConnectionType::Blocked), // → Tunnel (gap + live flow)
            peer(Some("aid-1"), ConnectionType::Offline), // → Tunnel (gap + live flow)
            peer(Some("aid-1"), ConnectionType::Direct),  // stays Direct (carrier wins)
            peer(Some("aid-1"), ConnectionType::Relay),   // stays Relay (carrier wins)
            peer(Some("aid-2"), ConnectionType::Blocked), // stays Blocked (no flow)
            peer(None, ConnectionType::Blocked),          // stays Blocked (no agent_id)
        ];
        apply_tunnel_override(&mut peers, &tunneled);
        assert_eq!(peers[0].connection, ConnectionType::Tunnel);
        assert_eq!(peers[1].connection, ConnectionType::Tunnel);
        assert_eq!(peers[2].connection, ConnectionType::Direct);
        assert_eq!(peers[3].connection, ConnectionType::Relay);
        assert_eq!(peers[4].connection, ConnectionType::Blocked);
        assert_eq!(peers[5].connection, ConnectionType::Blocked);

        // Empty tunneled set → untouched.
        let mut p2 = vec![peer(Some("aid-1"), ConnectionType::Blocked)];
        apply_tunnel_override(&mut p2, &HashSet::new());
        assert_eq!(p2[0].connection, ConnectionType::Blocked);
    }
}

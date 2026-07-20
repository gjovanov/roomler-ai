//! Overlay-network broker — the control plane for the Tailscale-style
//! L3 mesh (Phase 1 + 2).
//!
//! Overlay `rc:overlay.*` messages arrive on **both** WS roles (the
//! agent WS in [`super::remote_control`] and the tunnel-client WS in
//! [`super::tunnel`]), so this module exposes role-agnostic handlers
//! keyed by a [`NodeIdentity`] rather than owning its own socket loop.
//! Both read loops route their parsed overlay variants through
//! [`relay_overlay_msg_from_node`].
//!
//! Responsibilities:
//! * **IPAM** — allocate (or rehydrate) a stable overlay IP per node
//!   from the tenant's [`OverlayNetwork`].
//! * **Netmap distribution** — reply a full `rc:overlay.netmap` to a
//!   joiner and fan `rc:overlay.netmap_delta` upserts/removes to its
//!   permitted peers on join/endpoint-change/leave.
//! * **Relay grants** — mint short-lived coturn creds (keyed by a
//!   symmetric `pair_key`) on demand for a WG-over-coturn relay leg.
//!
//! The broker is **never** in the data path: the netmap travels the
//! authenticated TLS+JWT WS channel; the WG ciphertext rides UDP /
//! coturn directly between nodes.
//!
//! Reachability is ACL-precomputed server-side. Phase 1 ships
//! `reachable = same tenant + same network` (peers are sourced from a
//! tenant+network-scoped query, so the cross-tenant gate is structural);
//! Phase 4 swaps in `policy::evaluate_overlay`.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bson::{DateTime, oid::ObjectId};
use roomler_ai_remote_control::{
    models::{AgentStatus, NodeRef, OverlayNode},
    signaling::{ClientMsg, IceServer, NetmapPeer, OverlayNetworkInfo, ServerMsg},
    turn_creds,
};
use tokio::net::lookup_host;
use tracing::{debug, warn};

use crate::state::{AppState, build_turn_config};

/// Which underlying host an overlay message arrived from, captured at
/// the WS handler so the broker can resolve the node + route replies.
#[derive(Debug, Clone, Copy)]
pub enum NodeIdentity {
    Agent(ObjectId),
    TunnelClient(ObjectId),
}

impl NodeIdentity {
    fn node_ref(self) -> NodeRef {
        match self {
            NodeIdentity::Agent(id) => NodeRef::Agent { agent_id: id },
            NodeIdentity::TunnelClient(id) => NodeRef::TunnelClient {
                tunnel_client_id: id,
            },
        }
    }
}

/// Intercept `rc:overlay.*` variants and drive the broker. Returns
/// `None` when the message was consumed, or `Some(parsed)` so the
/// caller's existing dispatch handles non-overlay traffic. Shared by
/// both WS read loops.
pub async fn relay_overlay_msg_from_node(
    state: &AppState,
    ident: NodeIdentity,
    parsed: ClientMsg,
) -> Option<ClientMsg> {
    match parsed {
        ClientMsg::OverlayJoin {
            wg_public_key,
            key_epoch,
            endpoints,
            supports_quic,
            supports_relay_single,
            advertised_routes,
            ..
        } => {
            handle_overlay_join(
                state,
                ident,
                wg_public_key,
                key_epoch,
                endpoints,
                supports_quic,
                supports_relay_single,
                advertised_routes,
            )
            .await;
            None
        }
        ClientMsg::OverlayEndpoints { candidates } => {
            handle_overlay_endpoints(state, ident, candidates).await;
            None
        }
        ClientMsg::OverlaySrflx { candidates, nat } => {
            handle_overlay_srflx(state, ident, candidates, nat).await;
            None
        }
        ClientMsg::OverlayLeave {} => {
            handle_overlay_leave(state, ident).await;
            None
        }
        ClientMsg::OverlayRelayRequest { peer_node_id } => {
            handle_overlay_relay_request(state, ident, peer_node_id).await;
            None
        }
        other => Some(other),
    }
}

/// Join: IPAM (allocate or rehydrate) → persist → full netmap to the
/// joiner → upsert delta to each permitted peer.
#[allow(clippy::too_many_arguments)]
async fn handle_overlay_join(
    state: &AppState,
    ident: NodeIdentity,
    wg_public_key: String,
    key_epoch: u32,
    endpoints: Vec<String>,
    supports_quic: bool,
    supports_relay_single: bool,
    advertised_routes: Vec<String>,
) {
    let node_ref = ident.node_ref();
    let Some((tenant_id, machine_id, display_name)) =
        resolve_tenant_and_machine(state, ident).await
    else {
        warn!(?ident, "overlay.join from an unknown node; ignoring");
        return;
    };
    // Phase 0 — the DNS-safe base label from the node's display name.
    let base_name = dns_label(&display_name, &machine_id);
    // Phase 1 — drop malformed CIDRs so a bad advertisement can't poison state.
    let advertised_routes = sanitize_cidrs(advertised_routes);

    let network = match state.overlay_networks.get_or_create(tenant_id).await {
        Ok(n) => n,
        Err(e) => {
            warn!(%tenant_id, %e, "overlay.join: get_or_create network failed");
            return;
        }
    };
    let Some(network_id) = network.id else {
        warn!(%tenant_id, "overlay network missing _id");
        return;
    };

    // Rehydrate-on-rejoin (keeps the leased IP) or allocate a fresh one.
    let self_node = match state
        .overlay_nodes
        .find_by_tenant_and_machine(tenant_id, &machine_id)
        .await
    {
        Ok(Some(existing)) => {
            let Some(id) = existing.id else { return };
            // Keep the existing stable name (DNS mustn't churn on rejoin);
            // backfill a freshly-deduped one for a pre-Phase-0 empty row.
            let name = if existing.name.is_empty() {
                unique_node_name(state, tenant_id, network_id, &base_name, Some(id)).await
            } else {
                existing.name.clone()
            };
            match state
                .overlay_nodes
                .rehydrate(
                    id,
                    &node_ref,
                    &name,
                    &wg_public_key,
                    key_epoch,
                    &endpoints,
                    supports_quic,
                    supports_relay_single,
                    &advertised_routes,
                )
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    warn!(%tenant_id, %e, "overlay.join: rehydrate failed");
                    return;
                }
            }
        }
        Ok(None) => {
            // Fresh node — a per-network-unique name from the base label.
            let name = unique_node_name(state, tenant_id, network_id, &base_name, None).await;
            let host = match state.overlay_networks.allocate_host(network_id).await {
                Ok(h) => h,
                Err(e) => {
                    warn!(%tenant_id, %e, "overlay.join: IPAM allocate failed");
                    return;
                }
            };
            let Some(overlay_ip) = overlay_ip(&network.cidr, host) else {
                warn!(%tenant_id, cidr = %network.cidr, host, "overlay.join: bad CIDR/host");
                return;
            };
            match state
                .overlay_nodes
                .create(
                    tenant_id,
                    node_ref,
                    network_id,
                    machine_id,
                    name,
                    overlay_ip,
                    wg_public_key,
                    key_epoch,
                    endpoints,
                    supports_quic,
                    supports_relay_single,
                    advertised_routes,
                )
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    warn!(%tenant_id, %e, "overlay.join: node create failed");
                    return;
                }
            }
        }
        Err(e) => {
            warn!(%tenant_id, %e, "overlay.join: node lookup failed");
            return;
        }
    };

    let all = match state
        .overlay_nodes
        .list_active_in_network(tenant_id, network_id)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(%tenant_id, %e, "overlay.join: peer list failed");
            return;
        }
    };

    let epoch = next_epoch();
    let peers: Vec<NetmapPeer> = all
        .iter()
        .filter(|n| n.id != self_node.id)
        .map(to_netmap_peer)
        .collect();

    // Phase 2 MagicDNS — carry the tenant's DNS suffix + upstreams so the node
    // brings up its split-DNS resolver. Absent tenant settings → MagicDNS off.
    let (magic_domain, nameservers) = match state.tenants.base.find_by_id(tenant_id).await {
        Ok(t) => (
            t.settings.magic_dns_domain.clone(),
            t.settings.magic_dns_nameservers.clone(),
        ),
        Err(e) => {
            debug!(%tenant_id, %e, "overlay.join: tenant fetch for MagicDNS failed; DNS off");
            (None, Vec::new())
        }
    };

    // Full netmap → joiner.
    send_to_node(
        state,
        &self_node,
        ServerMsg::OverlayNetmap {
            self_ip: self_node.overlay_ip.clone(),
            network: OverlayNetworkInfo {
                cidr: network.cidr.clone(),
                mtu: network.mtu,
                magic_domain,
                nameservers,
                // NAT-traversal Phase B — the STUN endpoints a node queries to
                // gather its srflx candidates, derived from the configured
                // coturn workers (a `turn:host:port` UDP listener also answers
                // STUN Binding). Empty when TURN is unconfigured → srflx inert.
                stun_urls: stun_urls_from_turn(state),
            },
            peers,
            epoch,
        },
    )
    .await;

    // Upsert delta → every peer.
    let upsert = to_netmap_peer(&self_node);
    for peer in all.iter().filter(|n| n.id != self_node.id) {
        send_to_node(
            state,
            peer,
            ServerMsg::OverlayNetmapDelta {
                epoch,
                upserts: vec![upsert.clone()],
                removes: vec![],
            },
        )
        .await;
    }
}

/// Trickle: update the node's candidates → fan an upsert delta so peers
/// learn the new endpoints.
async fn handle_overlay_endpoints(state: &AppState, ident: NodeIdentity, candidates: Vec<String>) {
    let Some(self_node) = current_node(state, ident).await else {
        debug!(?ident, "overlay.endpoints before join; ignoring");
        return;
    };
    let Some(self_id) = self_node.id else { return };
    if let Err(e) = state
        .overlay_nodes
        .update_endpoints(self_id, &candidates)
        .await
    {
        warn!(%self_id, %e, "overlay.endpoints: update failed");
        return;
    }

    let mut updated = self_node;
    updated.endpoints = candidates;
    let epoch = next_epoch();
    let upsert = to_netmap_peer(&updated);
    fan_delta_to_peers(state, &updated, epoch, vec![upsert], vec![]).await;
}

/// NAT-traversal Phase B/C — the node trickled its server-reflexive (srflx)
/// candidates (and, Phase C, its probed `nat` type). Store them in the SEPARATE
/// `srflx_endpoints`/`srflx_nat` bucket (so a relay trickle can't clobber them)
/// → fan an upsert delta so peers learn the srflx + NAT type and can dial this
/// node directly through its NAT (skipping the punch only when both ends are
/// symmetric). Stored verbatim: the dial side already filters to public IPv4
/// (`direct::pick_public_endpoint`), and a peer only dials the srflx of an
/// ACL-authorised netmap peer — same trust model as `endpoints`/`lan_endpoints`.
async fn handle_overlay_srflx(
    state: &AppState,
    ident: NodeIdentity,
    candidates: Vec<String>,
    nat: Option<String>,
) {
    let Some(self_node) = current_node(state, ident).await else {
        debug!(?ident, "overlay.srflx before join; ignoring");
        return;
    };
    let Some(self_id) = self_node.id else { return };
    if let Err(e) = state
        .overlay_nodes
        .update_srflx_endpoints(self_id, &candidates, nat.as_deref())
        .await
    {
        warn!(%self_id, %e, "overlay.srflx: update failed");
        return;
    }

    let mut updated = self_node;
    updated.srflx_endpoints = candidates;
    updated.srflx_nat = nat;
    let epoch = next_epoch();
    let upsert = to_netmap_peer(&updated);
    fan_delta_to_peers(state, &updated, epoch, vec![upsert], vec![]).await;
}

/// Graceful leave (or WS teardown): mark offline + tell peers to drop.
pub async fn handle_overlay_leave(state: &AppState, ident: NodeIdentity) {
    let Some(self_node) = current_node(state, ident).await else {
        return; // never joined the overlay — nothing to tear down
    };
    let Some(self_id) = self_node.id else { return };
    let _ = state
        .overlay_nodes
        .mark_status(self_id, AgentStatus::Offline)
        .await;
    let epoch = next_epoch();
    fan_delta_to_peers(state, &self_node, epoch, vec![], vec![self_id]).await;
}

/// Mint symmetric coturn creds for a relay leg to `peer_node_id`.
async fn handle_overlay_relay_request(
    state: &AppState,
    ident: NodeIdentity,
    peer_node_id: ObjectId,
) {
    let Some(self_node) = current_node(state, ident).await else {
        debug!(?ident, "overlay.relay_request before join; ignoring");
        return;
    };
    let Some(self_id) = self_node.id else { return };

    // Cross-tenant gate: the peer must be in the requester's tenant.
    let peer = match state.overlay_nodes.base.find_by_id(peer_node_id).await {
        Ok(p) if p.tenant_id == self_node.tenant_id => p,
        Ok(p) => {
            warn!(%self_id, peer = %peer_node_id, peer_tenant = %p.tenant_id,
                "overlay.relay_request across tenants; refusing");
            return;
        }
        Err(e) => {
            debug!(peer = %peer_node_id, %e, "overlay.relay_request: peer not found");
            return;
        }
    };

    let pair_key = pair_key(self_id, peer_node_id);
    // Both ends derive identical creds from the symmetric pair_key, AND the
    // broker pins them to a single deterministic coturn worker (see
    // `overlay_ice_servers`) so the relay-to-relay leg is an intra-worker
    // hairpin that never crosses mars's dual-public-IP SNAT.
    let ice_servers = overlay_ice_servers(state, &pair_key).await;

    send_to_node(
        state,
        &self_node,
        ServerMsg::OverlayRelayGrant {
            ice_servers,
            peer_node_id: peer.id.unwrap_or(peer_node_id),
            pair_key,
        },
    )
    .await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Re-fan a node's current netmap entry to its peers as an upsert delta — used
/// when something OUT of band changes the node's wire shape (Phase 1: an admin
/// approving/revoking its subnet `routes`), so peers pick it up immediately
/// instead of waiting for the next join. Best-effort.
pub(crate) async fn refan_node(state: &AppState, node: &OverlayNode) {
    let upsert = to_netmap_peer(node);
    fan_delta_to_peers(state, node, next_epoch(), vec![upsert], vec![]).await;
}

/// Fan a delta to every active peer of `self_node` in its network.
async fn fan_delta_to_peers(
    state: &AppState,
    self_node: &OverlayNode,
    epoch: u64,
    upserts: Vec<NetmapPeer>,
    removes: Vec<ObjectId>,
) {
    let peers = match state
        .overlay_nodes
        .list_active_in_network(self_node.tenant_id, self_node.network_id)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(%e, "overlay: peer list for delta fan-out failed");
            return;
        }
    };
    for peer in peers.iter().filter(|n| n.id != self_node.id) {
        send_to_node(
            state,
            peer,
            ServerMsg::OverlayNetmapDelta {
                epoch,
                upserts: upserts.clone(),
                removes: removes.clone(),
            },
        )
        .await;
    }
}

/// Deliver a `ServerMsg` to one overlay node, resolving its `node_ref`:
/// agent nodes go through the Hub, tunnel-client nodes through the
/// connection-lifetime registry. Best-effort — an offline node is
/// simply skipped (it re-syncs on its next join).
async fn send_to_node(state: &AppState, node: &OverlayNode, msg: ServerMsg) {
    match &node.node_ref {
        NodeRef::Agent { agent_id } => {
            if let Err(e) = state.rc_hub.send_to_agent(*agent_id, msg) {
                debug!(agent_id = %agent_id, %e, "overlay: agent node unreachable; skipped");
            }
        }
        NodeRef::TunnelClient { tunnel_client_id } => {
            // Clone the Sender out of the DashMap Ref so the shard guard
            // isn't held across the `.await` (the established pattern in
            // `remote_control::relay_to_client`).
            let tx = state
                .overlay_nodes_by_id
                .get(tunnel_client_id)
                .map(|e| e.value().clone());
            match tx {
                Some(tx) => {
                    if let Err(e) = tx.send(msg).await {
                        debug!(%tunnel_client_id, %e, "overlay: client node channel closed; skipped");
                    }
                }
                None => debug!(%tunnel_client_id, "overlay: client node not connected; skipped"),
            }
        }
    }
}

/// Resolve the underlying `(tenant_id, machine_id)` for a node identity.
/// Returns `(tenant_id, machine_id, display_name)` for the identity — the
/// display name is the underlying agent/tunnel-client `name` (Phase 0, for the
/// overlay node name / MagicDNS).
async fn resolve_tenant_and_machine(
    state: &AppState,
    ident: NodeIdentity,
) -> Option<(ObjectId, String, String)> {
    match ident {
        NodeIdentity::Agent(id) => state
            .agents
            .base
            .find_by_id(id)
            .await
            .ok()
            .map(|a| (a.tenant_id, a.machine_id, a.name)),
        NodeIdentity::TunnelClient(id) => state
            .tunnel_clients
            .base
            .find_by_id(id)
            .await
            .ok()
            .map(|c| (c.tenant_id, c.machine_id, c.name)),
    }
}

/// Fetch the joined `OverlayNode` row for an identity (post-join ops).
async fn current_node(state: &AppState, ident: NodeIdentity) -> Option<OverlayNode> {
    let (tenant_id, machine_id, _name) = resolve_tenant_and_machine(state, ident).await?;
    state
        .overlay_nodes
        .find_by_tenant_and_machine(tenant_id, &machine_id)
        .await
        .ok()
        .flatten()
        .filter(|n| n.deleted_at.is_none())
}

/// Sanitize a display name to a single DNS label — lowercase `[a-z0-9-]`, no
/// leading/trailing dashes, no dash runs, ≤63 chars. Falls back to `fallback`
/// (the machine_id) then `"node"` when the name yields no usable characters.
fn dns_label(display: &str, fallback: &str) -> String {
    fn sanitize(s: &str) -> String {
        let mut out = String::new();
        let mut prev_dash = false;
        for c in s.chars() {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() {
                out.push(c);
                prev_dash = false;
            } else if !out.is_empty() && !prev_dash {
                out.push('-');
                prev_dash = true;
            }
        }
        out.truncate(63);
        while out.ends_with('-') {
            out.pop();
        }
        out
    }
    let primary = sanitize(display);
    if !primary.is_empty() {
        return primary;
    }
    let fb = sanitize(fallback);
    if !fb.is_empty() {
        return fb;
    }
    "node".to_string()
}

/// Make `base` unique among the network's node names (append `-2`, `-3`, …),
/// ignoring `exclude` (self, when backfilling). Best-effort — a lost race is
/// still caught by the unique `(tenant,network,name)` index.
async fn unique_node_name(
    state: &AppState,
    tenant_id: ObjectId,
    network_id: ObjectId,
    base: &str,
    exclude: Option<ObjectId>,
) -> String {
    let taken: std::collections::HashSet<String> = state
        .overlay_nodes
        .list_active_in_network(tenant_id, network_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.id != exclude)
        .map(|n| n.name)
        .filter(|s| !s.is_empty())
        .collect();
    if !taken.contains(base) {
        return base.to_string();
    }
    for i in 2..1000 {
        let candidate = format!("{base}-{i}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    format!("{base}-{}", next_epoch())
}

/// Phase 1 reachability is structural (same tenant + network), so every
/// peer the node receives is `reachable = true`. Phase 4 sets this from
/// `policy::evaluate_overlay`.
fn to_netmap_peer(node: &OverlayNode) -> NetmapPeer {
    NetmapPeer {
        node_id: node.id.unwrap_or_default(),
        overlay_ip: node.overlay_ip.clone(),
        name: node.name.clone(),
        wg_public_key: node.wg_public_key.clone(),
        // rc.135 — union the DIRECT LAN bucket with the trickled (srflx/relay)
        // bucket, LAN first, deduped. The relay trickle REPLACES `endpoints`,
        // so a node that allocated a relay would otherwise advertise no LAN
        // address and every peer would fall back to the relay path. Keeping
        // `lan_endpoints` separate and unioning here lets a same-subnet peer
        // always find the LAN candidate (field fix 2026-06-27).
        endpoints: union_endpoints(&node.lan_endpoints, &node.endpoints),
        // NAT-traversal Phase A — surface the join-time NIC bucket VERBATIM
        // (NOT unioned with the relay trickle). A globally-routable address in
        // here tells a peer this node's NIC holds a public IP, so it can be
        // dialed directly without STUN (the direct-to-public tier). It must stay
        // separate from `endpoints` because that union also carries coturn
        // relayed addresses, and on this fleet the coturn worker IPs are the
        // host public IPs — indistinguishable from a real public-on-NIC endpoint
        // in the union. Empty for a client that advertised no public endpoint.
        lan_endpoints: node.lan_endpoints.clone(),
        // NAT-traversal Phase B — surface the srflx bucket VERBATIM (its own
        // provenance, like `lan_endpoints`): a peer behind a different NAT dials
        // these to reach a 1:1/cone-NAT'd node directly. Empty until the node
        // gathers + trickles srflx (`rc:overlay.srflx`).
        srflx_endpoints: node.srflx_endpoints.clone(),
        // Phase C — surface the node's probed NAT type so a dialer can skip a
        // futile both-symmetric punch (VERBATIM, like srflx_endpoints).
        srflx_nat: node.srflx_nat.clone(),
        relay_home: node.relay_home.clone(),
        reachable: true,
        supports_quic: node.supports_quic,
        // Phase D — surface the node's single-relay capability so a peer only
        // picks single-relay when both ends advertise it (else both-allocate).
        supports_relay_single: node.supports_relay_single,
        // Phase 1 — only the admin-APPROVED routes reach peers.
        routes: node.approved_routes.clone(),
        // P3b-3 — expose the backing agent id (bridging overlay-node-id →
        // agents._id) so a controlling node can join this peer to a
        // daemon-originated tunnel flow and label it `ConnectionType::Tunnel`.
        // `None` for a tunnel-client node.
        agent_id: match &node.node_ref {
            NodeRef::Agent { agent_id } => Some(*agent_id),
            NodeRef::TunnelClient { .. } => None,
        },
    }
}

/// Keep only well-formed IPv4 CIDR strings (`a.b.c.d/nn`, prefix ≤ 32) so a
/// malformed or malicious advertisement can't poison the stored/distributed
/// route set. (Phase 1.)
fn sanitize_cidrs(routes: Vec<String>) -> Vec<String> {
    routes
        .into_iter()
        .filter(|r| {
            let Some((ip, pfx)) = r.split_once('/') else {
                return false;
            };
            ip.parse::<std::net::Ipv4Addr>().is_ok() && pfx.parse::<u8>().is_ok_and(|p| p <= 32)
        })
        .collect()
}

/// `lan ∪ rest`, LAN first, order-preserving dedup.
fn union_endpoints(lan: &[String], rest: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lan.len() + rest.len());
    for ep in lan.iter().chain(rest.iter()) {
        if !out.contains(ep) {
            out.push(ep.clone());
        }
    }
    out
}

/// `base_cidr` + host number → dotted overlay IP. e.g.
/// `("100.64.0.0/10", 7) → "100.64.0.7"`. No `ipnet` needed — the host
/// number is added to the network base address as a `u32`.
fn overlay_ip(cidr: &str, host: u32) -> Option<String> {
    let (base, _prefix) = cidr.split_once('/')?;
    let base: Ipv4Addr = base.parse().ok()?;
    let addr = Ipv4Addr::from(u32::from(base).checked_add(host)?);
    Some(addr.to_string())
}

/// Symmetric per-pair key so both ends mint identical coturn creds.
fn pair_key(a: ObjectId, b: ObjectId) -> String {
    let (x, y) = (a.to_hex(), b.to_hex());
    if x <= y {
        format!("{x}:{y}")
    } else {
        format!("{y}:{x}")
    }
}

/// Monotonic-enough netmap epoch. A wall-clock millisecond stamp avoids
/// per-network shared state; Phase 5 (resync) replaces this with a
/// persisted per-network counter.
fn next_epoch() -> u64 {
    DateTime::now().timestamp_millis().max(0) as u64
}

/// Overlay relay creds, pinned to ONE coturn worker for this pair.
///
/// The relay-to-relay leg must hairpin on a single worker — cross-worker
/// traffic drops under mars's dual-public-IP SNAT (the flakiness the QUIC
/// tunnel pinned around in rc.112). The agent's own deterministic pick
/// (`relay_link::pick_worker`) can't co-locate the two nodes because they
/// resolve `coturn.roomler.ai` to *different* IP sets per host. The broker
/// resolves it ONCE and picks one worker by `pair_key`, so its choice is
/// authoritative for both peers → guaranteed intra-worker hairpin. Falls back
/// to the hostname-based servers (pre-fix behaviour) with no TURN config or on
/// DNS failure.
async fn overlay_ice_servers(state: &AppState, pair_key: &str) -> Vec<IceServer> {
    let Some(turn_cfg) = build_turn_config(&state.settings.turn) else {
        return turn_creds::ice_servers_for(pair_key, None);
    };
    let servers = turn_creds::ice_servers_for(pair_key, Some(&turn_cfg));
    let Some(host) = turn_cfg.urls.first().and_then(|u| turn_url_host(u)) else {
        return servers;
    };
    let Some(ip) = resolve_pick_worker(&host, pair_key).await else {
        warn!(%host, "overlay relay: coturn DNS resolve failed; not pinning a worker");
        return servers;
    };
    let ip_s = ip.to_string();
    rewrite_ice_hosts(servers, &host, &ip_s)
}

/// Rewrite the coturn hostname to the pinned worker `ip` in every ICE URL —
/// EXCEPT `turns:` (TLS) URLs, which keep the hostname so the agent's TLS SNI +
/// certificate verification match coturn's DNS-only cert. Pinning the worker IP
/// is correct for the UDP tier (no TLS); on the TURNS/TCP tier an IP host makes
/// rustls verify a DNS cert against an IP literal → `NotValidForName` → the
/// handshake fails on the UDP-blocked corp VPNs that are the ONLY nets to reach
/// Tier 3. The same-worker hairpin for TURNS is restored separately via a
/// `&pin=` dial hint (rc.140); here we simply leave `turns:` hostnames intact.
fn rewrite_ice_hosts(servers: Vec<IceServer>, host: &str, ip: &str) -> Vec<IceServer> {
    servers
        .into_iter()
        .map(|mut s| {
            for u in s.urls.iter_mut() {
                if u.starts_with("turns:") {
                    // Keep the hostname for TLS SNI + cert verification, and
                    // append `&pin=<ip>` so the agent DIALS the pinned worker
                    // while still presenting the hostname coturn's cert matches
                    // (rc.140) — restores the same-worker hairpin over TURNS.
                    if !u.contains("pin=") {
                        let sep = if u.contains('?') { '&' } else { '?' };
                        u.push_str(&format!("{sep}pin={ip}"));
                    }
                    continue;
                }
                *u = u.replace(host, ip);
            }
            s
        })
        .collect()
}

/// Hostname of a `turn:`/`turns:` url (strips scheme + `:port` + `?query`).
fn turn_url_host(u: &str) -> Option<String> {
    let rest = u
        .strip_prefix("turns:")
        .or_else(|| u.strip_prefix("turn:"))?;
    let host = rest.split([':', '?']).next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// `host:port` of a `turn:`/`turns:` url (strips scheme + `?query`), e.g.
/// `turn:coturn.roomler.ai:3478?transport=udp` → `coturn.roomler.ai:3478`.
/// `None` if there's no `host:port` pair.
fn turn_url_host_port(u: &str) -> Option<String> {
    let rest = u
        .strip_prefix("turns:")
        .or_else(|| u.strip_prefix("turn:"))?;
    let hp = rest.split('?').next()?;
    (!hp.is_empty() && hp.contains(':')).then(|| hp.to_string())
}

/// NAT-traversal Phase B — the STUN endpoints a joining node queries to gather
/// its server-reflexive candidates, derived from the configured coturn workers.
/// A coturn `turn:host:port` UDP listener also answers STUN Binding requests, so
/// each UDP `turn:` URL maps to a `stun:host:port`. `turns:` (TLS) and
/// `?transport=tcp` variants are skipped — plain STUN is UDP. Deduped. Empty
/// when TURN isn't configured (dev), which leaves the srflx tier inert.
fn stun_urls_from_turn(state: &AppState) -> Vec<String> {
    match build_turn_config(&state.settings.turn) {
        Some(cfg) => stun_urls_from_turn_urls(&cfg.urls),
        None => Vec::new(),
    }
}

/// Pure core of [`stun_urls_from_turn`] — testable without an `AppState`.
fn stun_urls_from_turn_urls(turn_urls: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for u in turn_urls {
        if u.starts_with("turns:") || u.contains("transport=tcp") {
            continue;
        }
        let Some(hp) = turn_url_host_port(u) else {
            continue;
        };
        let stun = format!("stun:{hp}");
        if !out.contains(&stun) {
            out.push(stun);
        }
    }
    out
}

/// Short-TTL process cache of the resolved coturn worker IP set.
///
/// The relay pin MUST be identical for BOTH ends of a pair — they co-locate on
/// one coturn worker so the relay-to-relay leg is an intra-worker hairpin
/// (cross-worker traffic drops under mars's dual-public-IP SNAT). But
/// `lookup_host` can return a rotating subset/order per call, so two grants for
/// the same pair seconds apart could resolve **different-sized** IP sets and
/// `pick_worker_idx` (FNV `% len`) would then pick DIFFERENT workers — exactly
/// the field split (NEO16 on one worker, the VPN'd peer on another → 100% loss).
/// Resolving ONCE and caching for a short TTL makes every grant in the window
/// share one stable set → one pin. On a transient resolve failure we reuse the
/// last-good set rather than emit an unpinned grant that would round-robin the
/// pair apart. (roomler-ai runs a single API pod, so this process cache is
/// authoritative for every grant.)
static WORKER_SET_CACHE: Mutex<Option<(Instant, Vec<IpAddr>)>> = Mutex::new(None);
const WORKER_SET_TTL: Duration = Duration::from_secs(300);

/// Resolve the coturn worker IPs through [`WORKER_SET_CACHE`] so the pin is
/// stable across grants. Returns the cached set while fresh; otherwise resolves,
/// caches, and returns; on resolve failure reuses the last-good set (may be
/// empty only before the first successful resolve).
async fn resolve_workers_cached(host: &str) -> Vec<IpAddr> {
    {
        let guard = WORKER_SET_CACHE.lock().unwrap();
        if let Some((at, ips)) = guard.as_ref()
            && at.elapsed() < WORKER_SET_TTL
            && !ips.is_empty()
        {
            return ips.clone();
        }
    }
    let mut ips: Vec<IpAddr> = match lookup_host((host, 3478u16)).await {
        Ok(addrs) => addrs.map(|s| s.ip()).collect(),
        Err(_) => Vec::new(),
    };
    ips.sort();
    ips.dedup();
    if !ips.is_empty() {
        *WORKER_SET_CACHE.lock().unwrap() = Some((Instant::now(), ips.clone()));
        return ips;
    }
    // Transient resolve failure: reuse the last-good set (even if past TTL) so a
    // DNS blip doesn't unpin grants and split pairs across workers.
    WORKER_SET_CACHE
        .lock()
        .unwrap()
        .as_ref()
        .map(|(_, ips)| ips.clone())
        .unwrap_or_default()
}

/// Resolve `host` (cached, stable) and pick one IPv4 worker, indexed by
/// `pair_key`. Both ends of a pair get the identical result → intra-worker
/// hairpin.
async fn resolve_pick_worker(host: &str, pair_key: &str) -> Option<IpAddr> {
    let ips = resolve_workers_cached(host).await;
    pick_worker_idx(pair_key, ips)
}

/// Pure pick: sort+dedup the IPv4 candidates and index by a stable hash of
/// `pair_key`. Both peers of a pair share the `pair_key`, and the broker
/// hands them the SAME single result, so they co-locate.
fn pick_worker_idx(pair_key: &str, mut ips: Vec<IpAddr>) -> Option<IpAddr> {
    ips.retain(IpAddr::is_ipv4);
    ips.sort();
    ips.dedup();
    if ips.is_empty() {
        return None;
    }
    let idx = (fnv1a(pair_key.as_bytes()) % ips.len() as u64) as usize;
    Some(ips[idx])
}

/// Stable 64-bit FNV-1a (process-independent, unlike the stdlib hasher).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_ip_adds_host_to_cgnat_base() {
        assert_eq!(
            overlay_ip("100.64.0.0/10", 7).as_deref(),
            Some("100.64.0.7")
        );
        assert_eq!(
            overlay_ip("100.64.0.0/10", 256).as_deref(),
            Some("100.64.1.0")
        );
        assert_eq!(overlay_ip("10.0.0.0/8", 1).as_deref(), Some("10.0.0.1"));
        assert!(overlay_ip("not-a-cidr", 1).is_none());
    }

    #[test]
    fn pair_key_is_symmetric() {
        let a = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        let b = ObjectId::parse_str("507f1f77bcf86cd799439012").unwrap();
        assert_eq!(pair_key(a, b), pair_key(b, a));
        assert!(pair_key(a, b).contains(&a.to_hex()));
    }

    #[test]
    fn turn_url_host_strips_scheme_port_query() {
        assert_eq!(
            turn_url_host("turn:coturn.roomler.ai:3478?transport=udp").as_deref(),
            Some("coturn.roomler.ai")
        );
        assert_eq!(
            turn_url_host("turns:coturn.roomler.ai:5349?transport=tcp").as_deref(),
            Some("coturn.roomler.ai")
        );
        assert_eq!(turn_url_host("stun:stun.l.google.com:19302"), None);
    }

    #[test]
    fn turn_url_host_port_keeps_the_port() {
        assert_eq!(
            turn_url_host_port("turn:coturn.roomler.ai:3478?transport=udp").as_deref(),
            Some("coturn.roomler.ai:3478")
        );
        assert_eq!(
            turn_url_host_port("turn:coturn.roomler.ai:443").as_deref(),
            Some("coturn.roomler.ai:443")
        );
        // No port → None (STUN needs an explicit endpoint).
        assert_eq!(turn_url_host_port("turn:coturn.roomler.ai"), None);
    }

    #[test]
    fn stun_urls_derives_udp_turn_only() {
        // The full expansion `build_turn_config` produces from a plain base URL:
        // UDP `turn:` on 3478 + 443, plus TCP + TLS variants. STUN wants only the
        // UDP `turn:` listeners → two `stun:` URLs, deduped, TLS/TCP skipped.
        let turn_urls = vec![
            "turn:coturn.roomler.ai:3478".to_string(),
            "turn:coturn.roomler.ai:443?transport=udp".to_string(),
            "turn:coturn.roomler.ai:3478?transport=tcp".to_string(),
            "turns:coturn.roomler.ai:5349?transport=tcp".to_string(),
            "turns:coturn.roomler.ai:443?transport=udp".to_string(),
            "turns:coturn.roomler.ai:443?transport=tcp".to_string(),
        ];
        assert_eq!(
            stun_urls_from_turn_urls(&turn_urls),
            vec![
                "stun:coturn.roomler.ai:3478".to_string(),
                "stun:coturn.roomler.ai:443".to_string(),
            ]
        );
        // No TURN configured → empty (srflx tier inert).
        assert!(stun_urls_from_turn_urls(&[]).is_empty());
        // A same host:port on both UDP transports dedupes.
        assert_eq!(
            stun_urls_from_turn_urls(&[
                "turn:1.2.3.4:3478".to_string(),
                "turn:1.2.3.4:3478?transport=udp".to_string(),
            ]),
            vec!["stun:1.2.3.4:3478".to_string()]
        );
    }

    #[test]
    fn rewrite_ice_hosts_pins_udp_ip_but_turns_keeps_hostname_plus_pin() {
        // The pinned worker IP replaces the hostname on UDP/STUN URLs (no TLS),
        // but `turns:` (TLS) URLs keep the hostname for SNI/cert verification and
        // instead get a `&pin=<ip>` dial hint — an IP host would fail cert
        // verification (NotValidForName), yet we still need the same-worker pin.
        let servers = vec![IceServer {
            urls: vec![
                "stun:coturn.roomler.ai:3478".to_string(),
                "turn:coturn.roomler.ai:3478?transport=udp".to_string(),
                "turn:coturn.roomler.ai:443?transport=udp".to_string(),
                "turns:coturn.roomler.ai:443?transport=tcp".to_string(),
                "turns:coturn.roomler.ai:5349?transport=tcp".to_string(),
            ],
            username: Some("u".to_string()),
            credential: Some("c".to_string()),
        }];
        let out = rewrite_ice_hosts(servers, "coturn.roomler.ai", "94.130.141.74");
        assert_eq!(
            out[0].urls,
            vec![
                "stun:94.130.141.74:3478".to_string(),
                "turn:94.130.141.74:3478?transport=udp".to_string(),
                "turn:94.130.141.74:443?transport=udp".to_string(),
                "turns:coturn.roomler.ai:443?transport=tcp&pin=94.130.141.74".to_string(),
                "turns:coturn.roomler.ai:5349?transport=tcp&pin=94.130.141.74".to_string(),
            ]
        );
    }

    #[test]
    fn worker_pick_is_deterministic_and_order_independent() {
        // The broker hands BOTH peers the same single result, so it just has
        // to be stable per pair_key regardless of DNS ordering.
        let a: IpAddr = "5.9.157.221".parse().unwrap();
        let b: IpAddr = "5.9.157.226".parse().unwrap();
        let c: IpAddr = "94.130.141.74".parse().unwrap();
        let key = "507f1f77bcf86cd799439011:507f1f77bcf86cd799439012";
        let p = pick_worker_idx(key, vec![a, b, c]).unwrap();
        assert_eq!(p, pick_worker_idx(key, vec![c, a, b]).unwrap()); // shuffled
        assert_eq!(p, pick_worker_idx(key, vec![b, a, c, b]).unwrap()); // +dup
        assert!([a, b, c].contains(&p));
        // ipv6 filtered; empty → None
        let v6: IpAddr = "::1".parse().unwrap();
        assert_eq!(pick_worker_idx(key, vec![v6, a]).unwrap(), a);
        assert!(pick_worker_idx(key, vec![v6]).is_none());
        assert!(pick_worker_idx(key, vec![]).is_none());
    }
}

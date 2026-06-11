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
            ..
        } => {
            handle_overlay_join(state, ident, wg_public_key, key_epoch, endpoints).await;
            None
        }
        ClientMsg::OverlayEndpoints { candidates } => {
            handle_overlay_endpoints(state, ident, candidates).await;
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
async fn handle_overlay_join(
    state: &AppState,
    ident: NodeIdentity,
    wg_public_key: String,
    key_epoch: u32,
    endpoints: Vec<String>,
) {
    let node_ref = ident.node_ref();
    let Some((tenant_id, machine_id)) = resolve_tenant_and_machine(state, ident).await else {
        warn!(?ident, "overlay.join from an unknown node; ignoring");
        return;
    };

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
            match state
                .overlay_nodes
                .rehydrate(id, &node_ref, &wg_public_key, key_epoch, &endpoints)
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
                    overlay_ip,
                    wg_public_key,
                    key_epoch,
                    endpoints,
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

    // Full netmap → joiner.
    send_to_node(
        state,
        &self_node,
        ServerMsg::OverlayNetmap {
            self_ip: self_node.overlay_ip.clone(),
            network: OverlayNetworkInfo {
                cidr: network.cidr.clone(),
                mtu: network.mtu,
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
async fn resolve_tenant_and_machine(
    state: &AppState,
    ident: NodeIdentity,
) -> Option<(ObjectId, String)> {
    match ident {
        NodeIdentity::Agent(id) => state
            .agents
            .base
            .find_by_id(id)
            .await
            .ok()
            .map(|a| (a.tenant_id, a.machine_id)),
        NodeIdentity::TunnelClient(id) => state
            .tunnel_clients
            .base
            .find_by_id(id)
            .await
            .ok()
            .map(|c| (c.tenant_id, c.machine_id)),
    }
}

/// Fetch the joined `OverlayNode` row for an identity (post-join ops).
async fn current_node(state: &AppState, ident: NodeIdentity) -> Option<OverlayNode> {
    let (tenant_id, machine_id) = resolve_tenant_and_machine(state, ident).await?;
    state
        .overlay_nodes
        .find_by_tenant_and_machine(tenant_id, &machine_id)
        .await
        .ok()
        .flatten()
        .filter(|n| n.deleted_at.is_none())
}

/// Phase 1 reachability is structural (same tenant + network), so every
/// peer the node receives is `reachable = true`. Phase 4 sets this from
/// `policy::evaluate_overlay`.
fn to_netmap_peer(node: &OverlayNode) -> NetmapPeer {
    NetmapPeer {
        node_id: node.id.unwrap_or_default(),
        overlay_ip: node.overlay_ip.clone(),
        wg_public_key: node.wg_public_key.clone(),
        endpoints: node.endpoints.clone(),
        relay_home: node.relay_home.clone(),
        reachable: true,
    }
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
    servers
        .into_iter()
        .map(|mut s| {
            for u in s.urls.iter_mut() {
                *u = u.replace(&host, &ip_s);
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

/// Resolve `host` and pick one IPv4 worker, indexed by `pair_key`.
async fn resolve_pick_worker(host: &str, pair_key: &str) -> Option<IpAddr> {
    let ips: Vec<IpAddr> = lookup_host((host, 3478u16))
        .await
        .ok()?
        .map(|s| s.ip())
        .collect();
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

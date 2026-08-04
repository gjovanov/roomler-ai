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

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::{DateTime, oid::ObjectId};
use roomler_ai_remote_control::models::{OverlayAclMode, OverlayPolicy};
use roomler_ai_remote_control::{
    models::{AgentStatus, NodeRef, OverlayNode, overlay_ip},
    signaling::{ClientMsg, IceServer, NetmapPeer, OverlayNetworkInfo, ServerMsg},
    turn_creds,
    worker_pick::pick_worker_fnv1a,
};
use roomler_ai_services::dao::base::DaoError;
use tokio::net::lookup_host;
use tracing::{debug, warn};
use tunnel_core::policy::{
    OverlayPeerRef, OverlaySource, evaluate_overlay, evaluate_overlay_ingress,
};

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
            supports_derp,
            supports_forced_derp,
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
                supports_derp,
                supports_forced_derp,
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
    supports_derp: bool,
    supports_forced_derp: bool,
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

    // Rehydrate-on-rejoin (keeps the leased IP) or allocate a fresh one. The
    // lookup is LIVE-scoped: a machine that was removed from the fleet had its
    // node tombstoned and its host number recycled, so it must come back as a
    // brand-new node with a fresh lease rather than reviving the tombstone.
    let self_node = match state
        .overlay_nodes
        .find_live_by_tenant_and_machine(tenant_id, &machine_id)
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
                    supports_derp,
                    supports_forced_derp,
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
            // Bounded retry: a `DuplicateKey` here means the address we were
            // handed is somehow already held by a live row (a poisoned free
            // pool). Before the retry this logged and returned, silently
            // locking the device out of the overlay for the daemon's lifetime;
            // now the bad entry is already consumed off the pool and the next
            // allocate gets a clean one.
            const CREATE_ATTEMPTS: usize = 3;
            let mut created = None;
            for attempt in 1..=CREATE_ATTEMPTS {
                // Multi-org P2a: the allocation is bounded by the network's
                // OWN block ceiling — exhaustion refuses the join loudly
                // instead of leasing into a neighbor tenant's block.
                let host = match state
                    .overlay_networks
                    .allocate_host(network_id, network.max_host())
                    .await
                {
                    Ok(h) => h,
                    Err(e) => {
                        warn!(%tenant_id, %e, "overlay.join: IPAM allocate failed");
                        return;
                    }
                };
                let Some(ip) = overlay_ip(&network.cidr, host) else {
                    warn!(%tenant_id, cidr = %network.cidr, host, "overlay.join: bad CIDR/host");
                    return;
                };
                match state
                    .overlay_nodes
                    .create(
                        tenant_id,
                        node_ref.clone(),
                        network_id,
                        machine_id.clone(),
                        name.clone(),
                        ip.clone(),
                        wg_public_key.clone(),
                        key_epoch,
                        endpoints.clone(),
                        supports_quic,
                        supports_relay_single,
                        supports_derp,
                        supports_forced_derp,
                        advertised_routes.clone(),
                    )
                    .await
                {
                    Ok(n) => {
                        created = Some(n);
                        break;
                    }
                    Err(DaoError::DuplicateKey(e)) if attempt < CREATE_ATTEMPTS => {
                        warn!(%tenant_id, overlay_ip = %ip, attempt, %e,
                            "overlay.join: allocated address is already taken; re-allocating");
                    }
                    Err(e) => {
                        warn!(%tenant_id, %e, "overlay.join: node create failed");
                        return;
                    }
                }
            }
            match created {
                Some(n) => n,
                None => return,
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
    // P9 — presence: ghost rows (clean-leave Offline, or an agent whose
    // heartbeat went stale) ship `reachable = false` in the FULL netmap
    // instead of resurrecting as dialable peers on every rejoin.
    let reach = reachability(state, &all).await;
    // Overlay ACL — the joiner's view is shaped for the JOINER. This half is
    // naturally per-recipient because the full netmap is built for exactly one
    // node; the delta fan-out below is the half that had to be un-broadcast.
    let acl = load_acl(state, tenant_id).await;
    let joiner_src = overlay_source_of(state, &self_node).await;
    let mut peers: Vec<NetmapPeer> = Vec::new();
    for n in all.iter().filter(|n| n.id != self_node.id) {
        // P4 — resolving the peer's own identity costs up to 2 reads, so do it
        // ONLY when the tenant is enforcing and the rules will actually ship.
        // An `off`/`warn` tenant's join path is unchanged.
        let peer_src = if acl.enforcing() {
            Some(overlay_source_of(state, n).await)
        } else {
            None
        };
        if let Some(p) = shape_peer(
            &acl,
            &joiner_src,
            n,
            peer_src.as_ref(),
            is_reachable(&reach, n),
        ) {
            peers.push(p);
        }
    }

    // Overlay ACL — refresh the DERP relay allow table off the join path. A
    // join is when a NEW pubkey can enter the network, and the table is keyed by
    // pubkey, so it would otherwise stay stale (fail-open) for that node until
    // the next policy edit. Spawned: it re-reads every node, and the joiner must
    // not wait on that to get its netmap.
    if acl.gating() {
        let st = state.clone();
        tokio::spawn(async move {
            super::derp_acl::rebuild(&st, tenant_id, network_id).await;
        });
    }

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

    // Upsert delta → every peer, shaped PER RECIPIENT. The joiner is live by
    // construction. A recipient that may not see the joiner gets an explicit
    // `removes` rather than mere omission: omitting a peer (or shipping
    // `reachable: false`) is a no-op against an already-installed peer — only
    // the removes branch tears down the WG peer, its route and its carrier.
    for peer in all.iter().filter(|n| n.id != self_node.id) {
        let peer_src = overlay_source_of(state, peer).await;
        // The shaped peer here is the JOINER, whose identity we already have.
        let joiner_ingress = acl.enforcing().then_some(&joiner_src);
        let (upserts, removes) = match shape_peer(&acl, &peer_src, &self_node, joiner_ingress, true)
        {
            Some(u) => (vec![u], vec![]),
            None => (vec![], vec![self_node.id.unwrap_or_default()]),
        };
        send_to_node(
            state,
            peer,
            ServerMsg::OverlayNetmapDelta {
                epoch,
                upserts,
                removes,
            },
        )
        .await;
    }

    // P7 — a node that restarted mid-escalation lost its client-side DERP pin
    // (it lives in process memory); re-push any unexpired forced pairs so it
    // can't resume the broken TURN tier against a still-pinned peer. This is
    // the ONLY re-delivery path a single-relay DIALER has — it never sends a
    // relay_request, so it can't pick the pin back up reactively.
    repush_forced_pairs_on_join(state, &self_node).await;
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
    fan_upsert_shaped(state, &updated, epoch, true).await;
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
    fan_upsert_shaped(state, &updated, epoch, true).await;
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
    // Grey the row out; do NOT delete it. A `removes` delta made every
    // connected peer drop the node from `current_peers` entirely, so a host
    // that merely restarted VANISHED from `roomler peers` — while a host that
    // was already down when you joined rendered a normal `offline` row (it
    // arrives in the full netmap with P9 presence). Same server state, two
    // renderings, decided purely by when you happened to join.
    //
    // An offline upsert is also strictly less work for the receiver than the
    // remove was: `peer_config_from_netmap` drops `reachable = false` peers,
    // so peers stop dialing/probing this node IMMEDIATELY rather than at
    // their next rejoin — which is the same goal P9 pursued from the join
    // side. `removes` keeps its one honest producer, `release_overlay_node`:
    // a device that was actually released does disappear.
    fan_upsert_shaped(state, &self_node, epoch, false).await;
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

    // Overlay ACL — a TURN grant is a carrier for the very pair the netmap may
    // have just denied. Without this check a denied pair simply asks for relay
    // credentials and routes around the netmap, which would make the whole ACL
    // decorative. Enforced only in `Enforce` mode; `Warn` logs and grants.
    let acl = load_acl(state, self_node.tenant_id).await;
    if !matches!(acl.mode, OverlayAclMode::Off) {
        let src = overlay_source_of(state, &self_node).await;
        let access = evaluate_overlay(
            &acl.policies,
            &src,
            OverlayPeerRef {
                node_id: peer_node_id,
                overlay_ip: &peer.overlay_ip,
                approved_routes: &peer.approved_routes,
            },
        );
        if !access.visible {
            if acl.enforcing() {
                warn!(%self_id, peer = %peer_node_id,
                    "overlay acl: relay_request for a peer this node may not reach; refusing");
                return;
            }
            debug!(%self_id, peer = %peer_node_id,
                "overlay acl [warn]: would refuse relay_request");
        }
    }

    let pair_key = pair_key(self_id, peer_node_id);

    // P7 — forced-DERP escalation: a request that lands shortly after a prior
    // grant for the same pair means that grant's carrier already died (the
    // corp-middlebox TURN-churn signature). Count those grant→re-request
    // cycles; past the threshold, push `rc:overlay.force_derp` to BOTH ends
    // (never grant-borne — the single-relay DIALER never requests, so it
    // would never see a grant field) and skip the TURN grant entirely. Gated
    // on BOTH ends advertising the capability, so a mixed-version pair can
    // never split tiers.
    let pair_supports_forced_derp = forced_derp_enabled()
        && self_node.supports_forced_derp
        && self_node.supports_derp
        && peer.supports_forced_derp
        && peer.supports_derp;
    if pair_supports_forced_derp && note_relay_request(state, &pair_key) {
        tracing::info!(
            %pair_key, requester = %self_id, peer = %peer_node_id,
            "overlay relay: TURN churn threshold — escalating pair to forced DERP"
        );
        let ttl_ms = FORCED_DERP_TTL.as_millis() as u64;
        send_to_node(
            state,
            &self_node,
            ServerMsg::OverlayForceDerp {
                peer_node_id: peer.id.unwrap_or(peer_node_id),
                ttl_ms,
            },
        )
        .await;
        send_to_node(
            state,
            &peer,
            ServerMsg::OverlayForceDerp {
                peer_node_id: self_id,
                ttl_ms,
            },
        )
        .await;
        return;
    }

    // Both ends derive identical creds from the symmetric pair_key, AND the
    // broker pins them to a single deterministic coturn worker (see
    // `overlay_ice_servers`) so the relay-to-relay leg is an intra-worker
    // hairpin that never crosses mars's dual-public-IP SNAT.
    let ice_servers = overlay_ice_servers(
        state,
        &pair_key,
        self_node.relay_home.as_deref(),
        peer.relay_home.as_deref(),
    )
    .await;

    // P7 — arm the churn detector: a re-request for this pair arriving after
    // this grant (past the dedup gap) counts as one churn cycle. Armed before
    // the send so `pair_key` can move into the grant.
    note_grant_sent(state, &pair_key);
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
// P7 — forced-DERP escalation (per-pair TURN-churn tracking)
// ─────────────────────────────────────────────────────────────────────────────

/// Sliding window a pair's churn cycles are counted within.
const CHURN_WINDOW: Duration = Duration::from_secs(600);
/// Grant→re-request cycles inside [`CHURN_WINDOW`] that trigger escalation.
/// At the client's ~30 s teardown/re-request cadence this is ~2 min of
/// sustained churn — fast enough to matter, slow enough that a transient
/// blip never escalates.
const CHURN_CYCLES_TO_ESCALATE: u32 = 3;
/// How long an escalated pair stays pinned to DERP (server TTL, mirrored to
/// both clients in `ttl_ms`). Stands alone — a forced pair stops sending
/// relay_requests, so nothing refreshes it; expiry simply lets the next
/// establishment cycle try TURN again.
const FORCED_DERP_TTL: Duration = Duration::from_secs(1800);
/// A request arriving within this gap of the pair's last grant is a client
/// retry/duplicate, not a died-carrier cycle.
const CYCLE_MIN_GAP: Duration = Duration::from_secs(5);
/// Hard cap on tracked pairs — a `retain` sweep drops stale entries past it
/// (reset-on-access TTLs alone never evict idle keys).
const CHURN_MAP_CAP: usize = 10_000;

/// P7 — per-pair TURN-relay churn state (see `AppState::relay_pair_churn`).
#[derive(Debug, Default)]
pub struct PairChurn {
    /// Completed grant→re-request cycles inside the current window.
    cycles: u32,
    /// When the current counting window opened.
    window_start: Option<Instant>,
    /// When the last TURN grant for this pair was sent; a re-request after it
    /// (past [`CYCLE_MIN_GAP`]) = one churn cycle. Cleared once consumed so a
    /// burst of retries counts once per grant.
    last_grant_at: Option<Instant>,
    /// While unexpired, the pair is escalated (mirrors the client-side pin).
    forced_until: Option<Instant>,
}

/// P7 — is this pair currently under an unexpired forced-DERP TTL?
fn forced_active(pc: &PairChurn, now: Instant) -> bool {
    pc.forced_until.is_some_and(|t| now < t)
}

/// P7 — pure decision core: note a relay_request for a pair at `now`.
/// Returns `true` when the pair should be (or already is) escalated — the
/// caller then pushes `force_derp` to both ends instead of granting. Kept
/// state-free of `AppState` so the threshold arithmetic is unit-tested
/// directly.
pub(crate) fn churn_note_request(pc: &mut PairChurn, now: Instant) -> bool {
    if forced_active(pc, now) {
        // Mid-TTL re-request = a client that restarted and lost its pin;
        // re-escalate it (the peer keeps its own pin).
        return true;
    }
    pc.forced_until = None;
    let Some(granted) = pc.last_grant_at else {
        // No grant on record (fresh pair / post-restart / post-expiry first
        // contact) — a first request is never churn.
        return false;
    };
    if now.duration_since(granted) < CYCLE_MIN_GAP {
        return false; // client retry burst, not a died carrier
    }
    pc.last_grant_at = None; // consume: one cycle per grant
    match pc.window_start {
        Some(ws) if now.duration_since(ws) <= CHURN_WINDOW => pc.cycles += 1,
        _ => {
            pc.window_start = Some(now);
            pc.cycles = 1;
        }
    }
    if pc.cycles >= CHURN_CYCLES_TO_ESCALATE {
        pc.forced_until = Some(now + FORCED_DERP_TTL);
        pc.cycles = 0;
        pc.window_start = None;
        return true;
    }
    false
}

/// P7 — kill-switch: `ROOMLER__OVERLAY__FORCED_DERP=0|false` disables the
/// escalation entirely (default ON — it is additionally gated on both ends'
/// advertised capability and the churn threshold).
fn forced_derp_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            std::env::var("ROOMLER__OVERLAY__FORCED_DERP").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// P7 — record + decide for a live request (the impure wrapper around
/// [`churn_note_request`]), with the size-capped stale sweep.
fn note_relay_request(state: &AppState, pair_key: &str) -> bool {
    let now = Instant::now();
    if state.relay_pair_churn.len() > CHURN_MAP_CAP {
        state.relay_pair_churn.retain(|_, pc| {
            forced_active(pc, now)
                || pc
                    .window_start
                    .is_some_and(|ws| now.duration_since(ws) <= CHURN_WINDOW)
                || pc
                    .last_grant_at
                    .is_some_and(|g| now.duration_since(g) <= CHURN_WINDOW)
        });
    }
    let mut entry = state
        .relay_pair_churn
        .entry(pair_key.to_string())
        .or_default();
    churn_note_request(entry.value_mut(), now)
}

/// P7 — arm the cycle detector after sending a TURN grant.
fn note_grant_sent(state: &AppState, pair_key: &str) {
    if let Some(mut pc) = state.relay_pair_churn.get_mut(pair_key) {
        pc.last_grant_at = Some(Instant::now());
    } else {
        state.relay_pair_churn.insert(
            pair_key.to_string(),
            PairChurn {
                last_grant_at: Some(Instant::now()),
                ..Default::default()
            },
        );
    }
}

/// P7 — on (re)join, re-push any unexpired forced-DERP pins involving this
/// node (its in-process pin died with the old process; the single-relay
/// DIALER has no reactive path to relearn it). Scans the pair map — tiny in
/// practice (forced pairs are rare + capped).
async fn repush_forced_pairs_on_join(state: &AppState, node: &OverlayNode) {
    if !forced_derp_enabled() || !node.supports_forced_derp || !node.supports_derp {
        return;
    }
    let Some(self_id) = node.id else { return };
    let self_hex = self_id.to_hex();
    let now = Instant::now();
    let peers: Vec<(ObjectId, u64)> = state
        .relay_pair_churn
        .iter()
        .filter_map(|e| {
            // Remaining (not full) TTL, so both ends keep expiring together.
            let remaining = e.value().forced_until?.checked_duration_since(now)?;
            let (a, b) = e.key().split_once(':')?;
            let peer = if a == self_hex {
                ObjectId::parse_str(b).ok()?
            } else if b == self_hex {
                ObjectId::parse_str(a).ok()?
            } else {
                return None;
            };
            Some((peer, remaining.as_millis() as u64))
        })
        .collect();
    for (peer_id, ttl_ms) in peers {
        tracing::info!(node = %self_id, peer = %peer_id, ttl_ms,
            "overlay relay: re-pushing forced-DERP pin on rejoin");
        send_to_node(
            state,
            node,
            ServerMsg::OverlayForceDerp {
                peer_node_id: peer_id,
                ttl_ms,
            },
        )
        .await;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Re-fan a node's current netmap entry to its peers as an upsert delta — used
/// when something OUT of band changes the node's wire shape (Phase 1: an admin
/// approving/revoking its subnet `routes`), so peers pick it up immediately
/// instead of waiting for the next join. Best-effort.
pub(crate) async fn refan_node(state: &AppState, node: &OverlayNode) {
    // P9 — an out-of-band refan (admin route approval) can target a node
    // that is currently offline; carry its real presence, not a blanket true.
    let reach = reachability(state, std::slice::from_ref(node)).await;
    fan_upsert_shaped(state, node, next_epoch(), is_reachable(&reach, node)).await;
}

/// What a successful [`release_overlay_node`] freed — for the route responses
/// and the operator-facing log line.
pub(crate) struct ReleasedNode {
    pub node_id: ObjectId,
    pub name: String,
    pub overlay_ip: String,
    /// `false` when the host number could NOT be returned to the pool (CIDR
    /// drift, or the pool is at its cap). The release still succeeded — the
    /// address just leaks out of the `/10` instead of being recycled.
    pub host_recycled: bool,
}

/// Release ONE overlay node: evict it from the mesh and return its host number
/// to the tenant's free pool. The single writer behind every removal path
/// (agent DELETE, admin evict, tunnel-client DELETE).
///
/// Idempotent — `None` means the node was already released and the caller must
/// do nothing further.
///
/// THE ORDER IS LOAD-BEARING:
///
/// 1. Read the peer list FIRST, while the node is still live. Afterwards it is
///    gone from `list_active_in_network`.
///
/// 2. CAS-tombstone via [`OverlayNodeDao::release`]. Winning that CAS is the
///    release TOKEN: only the winner proceeds, so two concurrent removal paths
///    can never both pool the same host number.
///
/// 3. Return the host to the pool ONLY AFTER the tombstone commits. NEVER
///    before — a crash between "pooled" and "tombstoned" would hand the address
///    to a second joiner while the first row still holds it, and the unique
///    `(tenant, network, overlay_ip)` index would then lock that joiner out of
///    the overlay for good. A crash in this order merely LEAKS one host number
///    out of a 4.2 M-address `/10`.
///
/// 4. Fan `removes` to the pinned peers, and to the released node itself so a
///    prune-aware client tears its carriers down instead of squatting an
///    address that is now recyclable.
pub(crate) async fn release_overlay_node(
    state: &AppState,
    node: &OverlayNode,
    reason: &str,
) -> Option<ReleasedNode> {
    let node_id = node.id?;

    // 1 — peers, read while the node is still live.
    let peers = state
        .overlay_nodes
        .list_active_in_network(node.tenant_id, node.network_id)
        .await
        .unwrap_or_default();

    // 2 — CAS. Losing it means someone else already released this node: do NOT
    //     re-fan and do NOT re-pool.
    let before = match state.overlay_nodes.release(node_id).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            debug!(%node_id, "overlay release: already released; no-op");
            return None;
        }
        Err(e) => {
            warn!(%node_id, %e, "overlay release: tombstone failed");
            return None;
        }
    };

    // 3 — pool the host. Best-effort: a failure leaks, never conflicts.
    let host = state
        .overlay_networks
        .base
        .find_by_id(before.network_id)
        .await
        .ok()
        .and_then(|net| net.host_of_ip(&before.overlay_ip));
    let host_recycled = match host {
        Some(h) => state
            .overlay_networks
            .release_host(before.network_id, h)
            .await
            .unwrap_or(false),
        None => {
            warn!(%node_id, ip = %before.overlay_ip,
                "overlay release: address is not derivable from the network CIDR; not recycling");
            false
        }
    };

    // 4 — tell the peers, then the node itself.
    let epoch = next_epoch();
    fan_delta_to(state, &peers, Some(node_id), epoch, vec![], vec![node_id]).await;
    send_to_node(
        state,
        &before,
        ServerMsg::OverlayNetmapDelta {
            epoch,
            upserts: vec![],
            removes: vec![node_id],
        },
    )
    .await;

    // Drop any DERP registration this node holds. Already inert (it is keyed by
    // pubkey and no peer carries this node's key any more, and `handle_derp_socket`
    // resolves through the live-only `current_node`), but tidy. C-5: also fire
    // the cancel so the socket actually closes (its teardown then releases the
    // directory record; a re-dial fails registration — the node row is gone).
    if let Ok(pk) = BASE64.decode(&before.wg_public_key)
        && let Ok(pk) = <[u8; 32]>::try_from(pk.as_slice())
    {
        state.derp_registry.remove(&(before.network_id, pk));
        if let Some(cancel) = state.derp_cancels.get(&(before.network_id, pk)) {
            cancel.notify_one();
        }
    }

    tracing::info!(
        tenant_id = %before.tenant_id, %node_id, name = %before.name,
        overlay_ip = %before.overlay_ip, host_recycled, reason,
        "overlay node released"
    );

    Some(ReleasedNode {
        node_id,
        name: before.name,
        overlay_ip: before.overlay_ip,
        host_recycled,
    })
}

/// Release the live overlay node backing `expect` on `machine_id`, if any.
///
/// Resolves via the indexed `(tenant_id, machine_id)` lookup and THEN verifies
/// `node_ref`: an agent and a tunnel-client on the same box share a
/// `machine_id`, and the unique index means only ONE of them can own the
/// overlay node — so deleting the agent must not release a node the
/// still-enrolled tunnel-client owns.
pub(crate) async fn release_overlay_node_for(
    state: &AppState,
    tenant_id: ObjectId,
    machine_id: &str,
    expect: &NodeRef,
    reason: &str,
) -> Option<ReleasedNode> {
    let node = state
        .overlay_nodes
        .find_live_by_tenant_and_machine(tenant_id, machine_id)
        .await
        .ok()
        .flatten()?;
    if &node.node_ref != expect {
        debug!(%tenant_id, machine_id,
            "overlay release: the node on this machine belongs to another role; skipping");
        return None;
    }
    release_overlay_node(state, &node, reason).await
}

/// Fan a delta to every active peer of `self_node` in its network.
/// Fan an upsert of `node` to its peers, **shaped per recipient** by the
/// tenant's overlay ACL.
///
/// Replaces the old build-once-clone-N-times fan-out: the entry a recipient
/// receives now depends on what that recipient is allowed to see. A recipient
/// that may not see `node` is sent an explicit `removes` rather than simply
/// being skipped — omitting a peer from a delta (or shipping it with
/// `reachable: false`) does NOT tear down an already-installed peer; only the
/// removes branch drops the WG peer, its route and its carrier.
async fn fan_upsert_shaped(state: &AppState, node: &OverlayNode, epoch: u64, reachable: bool) {
    let peers = match state
        .overlay_nodes
        .list_active_in_network(node.tenant_id, node.network_id)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(%e, "overlay: peer list for delta fan-out failed");
            return;
        }
    };
    let acl = load_acl(state, node.tenant_id).await;
    // P4 — the fanned node is the SHAPED peer for every recipient, so its
    // identity is resolved once, not per recipient.
    let node_src = if acl.enforcing() {
        Some(overlay_source_of(state, node).await)
    } else {
        None
    };
    for peer in peers.iter().filter(|p| p.id != node.id) {
        let src = overlay_source_of(state, peer).await;
        let (upserts, removes) = match shape_peer(&acl, &src, node, node_src.as_ref(), reachable) {
            Some(u) => (vec![u], vec![]),
            None => (vec![], vec![node.id.unwrap_or_default()]),
        };
        send_to_node(
            state,
            peer,
            ServerMsg::OverlayNetmapDelta {
                epoch,
                upserts,
                removes,
            },
        )
        .await;
    }
}

/// Fan a delta to an EXPLICIT peer list, skipping `exclude`.
///
/// Split out of [`fan_delta_to_peers`] for the release path, which must read
/// its peers BEFORE the node is tombstoned — afterwards the released node is
/// gone from `list_active_in_network`. Pinning the list makes that ordering
/// visible in the code instead of implied, and saves a round-trip.
async fn fan_delta_to(
    state: &AppState,
    peers: &[OverlayNode],
    exclude: Option<ObjectId>,
    epoch: u64,
    upserts: Vec<NetmapPeer>,
    removes: Vec<ObjectId>,
) {
    for peer in peers.iter().filter(|n| n.id != exclude) {
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
///
/// Live-only, so every post-join handler (endpoints, srflx, leave,
/// relay_request, DERP registration) silently no-ops for a RELEASED node — which
/// is correct: the release already fanned its `removes`. The `deleted_at` filter
/// below is redundant now that the lookup is scoped; kept as belt and braces.
pub(crate) async fn current_node(state: &AppState, ident: NodeIdentity) -> Option<OverlayNode> {
    let (tenant_id, machine_id, _name) = resolve_tenant_and_machine(state, ident).await?;
    state
        .overlay_nodes
        .find_live_by_tenant_and_machine(tenant_id, &machine_id)
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

/// P9 — how stale a backing agent's heartbeat may be before its overlay row
/// ships `reachable = false`. Heartbeats refresh `agents.last_seen_at` every
/// ~30 s ([`roomler_ai_services::dao::AgentDao::touch_heartbeat`]); 120 s =
/// four missed beats, tolerant of a transient stall but killing day-old
/// ghosts on the first netmap that carries them.
const NODE_STALE_AFTER_MS: i64 = 120_000;

/// P9 — presence for the netmap: `reachable = false` for a row that cleanly
/// left (status `Offline` — the leave handler's mark) or whose backing
/// agent's heartbeat went stale. Peers render such rows `offline` and stop
/// burning dials / probes / REKEY handshakes on them (field 2026-07-28: a
/// dead duplicate enrollment sat "blocked" in every netmap for a day while
/// both sides hammered its stale endpoints — full netmaps resurrected it as
/// dialable on every rejoin, undoing the leave-time remove delta).
/// Tunnel-client rows have no heartbeat trail — status alone decides (a pod
/// crash can leave one Online-but-gone until its next clean leave; a
/// periodic stale-sweep is the v2). FAIL-OPEN: a freshness-query error reads
/// as "everything reachable" — a DB blip must not mark the fleet offline.
async fn reachability(state: &AppState, nodes: &[OverlayNode]) -> HashMap<ObjectId, bool> {
    let agent_ids: Vec<ObjectId> = nodes
        .iter()
        .filter_map(|n| match &n.node_ref {
            NodeRef::Agent { agent_id } => Some(*agent_id),
            NodeRef::TunnelClient { .. } => None,
        })
        .collect();
    let fresh = match state
        .agents
        .last_seen_fresh(&agent_ids, NODE_STALE_AFTER_MS)
        .await
    {
        Ok(m) => Some(m),
        Err(e) => {
            warn!(%e, "overlay: agent freshness query failed; failing open");
            None
        }
    };
    nodes
        .iter()
        .filter_map(|n| {
            let id = n.id?;
            let up = matches!(n.status, AgentStatus::Online)
                && match &n.node_ref {
                    NodeRef::Agent { agent_id } => fresh
                        .as_ref()
                        .map(|m| m.get(agent_id).copied().unwrap_or(false))
                        .unwrap_or(true),
                    NodeRef::TunnelClient { .. } => true,
                };
            Some((id, up))
        })
        .collect()
}

/// The `reachable` a node's netmap row ships, from a [`reachability`] map.
/// Fail-open for a row that somehow missed the map (no `_id`).
fn is_reachable(reach: &HashMap<ObjectId, bool>, node: &OverlayNode) -> bool {
    node.id.and_then(|i| reach.get(&i)).copied().unwrap_or(true)
}

// ────────────────────────────────────────────────────────────────────────────
// Overlay ACL (L3)
// ────────────────────────────────────────────────────────────────────────────

/// One tenant's ACL posture + rules, loaded once per netmap event.
///
/// Cheap to build and short-lived on purpose: netmap events are joins, leaves,
/// endpoint trickles and admin edits — orders of magnitude rarer than the
/// per-flow tunnel gate, so there is nothing to cache yet.
pub(crate) struct AclCtx {
    mode: OverlayAclMode,
    pub(crate) policies: Vec<OverlayPolicy>,
}

impl AclCtx {
    fn off() -> Self {
        Self {
            mode: OverlayAclMode::Off,
            policies: Vec::new(),
        }
    }
    pub(crate) fn enforcing(&self) -> bool {
        matches!(self.mode, OverlayAclMode::Enforce)
    }
    /// `true` when the ACL is doing anything at all (`Warn` or `Enforce`).
    /// `Warn` still evaluates — that's what produces the pre-cutover evidence —
    /// so "is a table worth building?" is this, not [`enforcing`](Self::enforcing).
    pub(crate) fn gating(&self) -> bool {
        !matches!(self.mode, OverlayAclMode::Off)
    }
}

/// Load the tenant's ACL posture and rules.
///
/// **Fails OPEN, deliberately.** The tunnel gate defaults to deny because a
/// denied flow is one broken connection; here a spurious deny would withhold
/// every peer and tear down the tenant's whole mesh on a transient Mongo blip.
/// The same reasoning already governs `reachability()` above ("failing open").
/// A load failure is logged at ERROR so it is never silent.
pub(crate) async fn load_acl(state: &AppState, tenant_id: ObjectId) -> AclCtx {
    let mode = match state.overlay_networks.get_or_create(tenant_id).await {
        Ok(n) => n.acl_mode,
        Err(e) => {
            tracing::error!(%tenant_id, %e, "overlay acl: network read failed; failing OPEN");
            return AclCtx::off();
        }
    };
    if matches!(mode, OverlayAclMode::Off) {
        return AclCtx::off();
    }
    match state
        .overlay_policies
        .list_active_for_tenant(tenant_id)
        .await
    {
        Ok(policies) => AclCtx { mode, policies },
        Err(e) => {
            tracing::error!(%tenant_id, %e, "overlay acl: policy read failed; failing OPEN");
            AclCtx::off()
        }
    }
}

/// Resolve the identity a netmap is being built FOR: the node itself, plus the
/// owner + roles of its backing agent / tunnel client so `UserId` / `RoleId`
/// selectors can match.
pub(crate) async fn overlay_source_of(state: &AppState, node: &OverlayNode) -> OverlaySource {
    let owner_user_id = match &node.node_ref {
        NodeRef::Agent { agent_id } => state
            .agents
            .base
            .find_by_id(*agent_id)
            .await
            .ok()
            .map(|a| a.owner_user_id),
        NodeRef::TunnelClient { tunnel_client_id } => state
            .tunnel_clients
            .base
            .find_by_id(*tunnel_client_id)
            .await
            .ok()
            .map(|c| c.owner_user_id),
    };
    let role_ids = match owner_user_id {
        Some(uid) => state
            .tenants
            .member_role_ids(node.tenant_id, uid)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    OverlaySource {
        node_id: node.id.unwrap_or_default(),
        owner_user_id,
        role_ids,
    }
}

/// Shape one peer for one recipient. `None` = withhold it entirely.
///
/// In `Warn` mode the permissive peer is returned but every difference is
/// logged, which is what makes the cutover to `Enforce` evidence-driven rather
/// than a leap of faith.
/// `peer_src` is the PEER's own identity, needed to compile the ingress rules it
/// may use against this recipient — the reverse direction of the visibility
/// question. `None` ⇒ ship no rules (`ingress_rules: None`), which the node
/// reads as "the ACL compiled nothing, fall back to the coarse local scope".
/// Callers pass `None` unless the tenant is ENFORCING, so `warn` can never cause
/// a node to drop: warn's whole contract is that it observes and never denies.
fn shape_peer(
    acl: &AclCtx,
    src: &OverlaySource,
    peer: &OverlayNode,
    peer_src: Option<&OverlaySource>,
    reachable: bool,
) -> Option<NetmapPeer> {
    if matches!(acl.mode, OverlayAclMode::Off) {
        return Some(to_netmap_peer(peer, reachable));
    }
    let access = evaluate_overlay(
        &acl.policies,
        src,
        OverlayPeerRef {
            node_id: peer.id.unwrap_or_default(),
            overlay_ip: &peer.overlay_ip,
            approved_routes: &peer.approved_routes,
        },
    );
    if !acl.enforcing() {
        if !access.visible {
            debug!(source = %src.node_id, peer = %peer.name,
                "overlay acl [warn]: would WITHHOLD peer");
        } else if access.routes.len() != peer.approved_routes.len() {
            debug!(source = %src.node_id, peer = %peer.name,
                granted = access.routes.len(), approved = peer.approved_routes.len(),
                "overlay acl [warn]: would narrow routes");
        }
        return Some(to_netmap_peer(peer, reachable));
    }
    if !access.visible {
        return None;
    }
    let mut shaped = to_netmap_peer(peer, reachable);
    shaped.routes = access.routes;
    // P4 — per-source ingress rules: what THIS peer may address through the
    // recipient, including the port/proto dimensions `routes` cannot carry.
    // Always `Some` when compiled, even when empty — an empty grant is a DENY
    // and must not be confused with "no ACL".
    if let Some(ps) = peer_src {
        shaped.ingress_rules = Some(evaluate_overlay_ingress(&acl.policies, ps, src.node_id));
    }
    Some(shaped)
}

/// Structural fields come from the row; `reachable` is the caller's presence
/// verdict ([`reachability`] for peer lists; `true` for a node's OWN upsert —
/// the sender of a live WS message is reachable by construction).
///
/// This is the PERMISSIVE shape. Access control is applied by [`shape_peer`],
/// which decides whether a given recipient sees this peer at all and narrows
/// `routes` to the subset that recipient may install — `reachable` deliberately
/// stays a presence signal, because shipping `reachable: false` does not tear
/// down an already-installed peer (only a `removes` does).
fn to_netmap_peer(node: &OverlayNode, reachable: bool) -> NetmapPeer {
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
        reachable,
        supports_quic: node.supports_quic,
        // Phase D — surface the node's single-relay capability so a peer only
        // picks single-relay when both ends advertise it (else both-allocate).
        supports_relay_single: node.supports_relay_single,
        // Phase D (DERP) — surface the node's DERP capability so a both-UDP-blocked
        // pair only picks DERP when both ends advertise it.
        supports_derp: node.supports_derp,
        // Only the admin-APPROVED routes reach peers — and, once the tenant's
        // overlay ACL is enforcing, only the subset THIS recipient may install
        // (see `shape_peer`). `to_netmap_peer` keeps the permissive default so
        // every legacy call site is unchanged.
        routes: node.approved_routes.clone(),
        // P3b-3 — expose the backing agent id (bridging overlay-node-id →
        // agents._id) so a controlling node can join this peer to a
        // daemon-originated tunnel flow and label it `ConnectionType::Tunnel`.
        // `None` for a tunnel-client node.
        agent_id: match &node.node_ref {
            NodeRef::Agent { agent_id } => Some(*agent_id),
            NodeRef::TunnelClient { .. } => None,
        },
        // P4 — the PERMISSIVE shape carries no ACL. `shape_peer` fills this in
        // (with `Some`, possibly empty) only when the tenant is enforcing;
        // leaving it `None` here is what makes an `off`/`warn` tenant's netmap
        // byte-identical to a pre-P4 server's.
        ingress_rules: None,
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
async fn overlay_ice_servers(
    state: &AppState,
    pair_key: &str,
    home_a: Option<&str>,
    home_b: Option<&str>,
) -> Vec<IceServer> {
    let region = sticky_pair_region(&state.turn_map, pair_key, home_a, home_b);
    if region.is_some() {
        crate::cluster::metrics::bump(&crate::cluster::metrics::RELAY_REGION_PICK_TOTAL);
    }
    let Some(turn_cfg) = state.turn_map.cfg_for(region.as_deref()) else {
        return turn_creds::ice_servers_for(pair_key, None);
    };
    let servers = turn_creds::ice_servers_for(pair_key, Some(turn_cfg));
    let Some((host, port)) = turn_cfg
        .urls
        .first()
        .and_then(|u| roomler_ai_remote_control::turn_url::host_port(u))
    else {
        return servers;
    };
    let Some(ip) = resolve_pick_worker(&host, port, pair_key).await else {
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

/// Short-TTL process cache of resolved coturn worker IP sets, keyed by host
/// (one entry per region's coturn hostname).
///
/// The relay pin MUST be identical for BOTH ends of a pair — they co-locate on
/// one coturn worker so the relay-to-relay leg is an intra-worker hairpin
/// (cross-worker traffic drops under mars's dual-public-IP SNAT). But
/// `lookup_host` can return a rotating subset/order per call, so two grants for
/// the same pair seconds apart could resolve **different-sized** IP sets and
/// `pick_worker_fnv1a` (FNV `% len`) would then pick DIFFERENT workers — exactly
/// the field split (NEO16 on one worker, the VPN'd peer on another → 100% loss).
/// Resolving ONCE and caching for a short TTL makes every grant in the window
/// share one stable set → one pin. On a transient resolve failure we reuse the
/// last-good set rather than emit an unpinned grant that would round-robin the
/// pair apart. (Both peers of a pair land on the SAME pod — the front LB
/// hashes on tenant — so this process cache is authoritative per pair.)
static WORKER_SET_CACHE: Mutex<Option<std::collections::HashMap<String, (Instant, Vec<IpAddr>)>>> =
    Mutex::new(None);
const WORKER_SET_TTL: Duration = Duration::from_secs(300);

/// Resolve a coturn host's worker IPs through [`WORKER_SET_CACHE`] so the pin
/// is stable across grants. Returns the cached set while fresh; otherwise
/// resolves, caches, and returns; on resolve failure reuses the host's
/// last-good set (empty only before its first successful resolve).
async fn resolve_workers_cached(host: &str, port: u16) -> Vec<IpAddr> {
    {
        let mut guard = WORKER_SET_CACHE.lock().unwrap();
        let cache = guard.get_or_insert_with(Default::default);
        if let Some((at, ips)) = cache.get(host)
            && at.elapsed() < WORKER_SET_TTL
            && !ips.is_empty()
        {
            return ips.clone();
        }
    }
    let mut ips: Vec<IpAddr> = match lookup_host((host, port)).await {
        Ok(addrs) => addrs.map(|s| s.ip()).collect(),
        Err(_) => Vec::new(),
    };
    ips.sort();
    ips.dedup();
    let mut guard = WORKER_SET_CACHE.lock().unwrap();
    let cache = guard.get_or_insert_with(Default::default);
    if !ips.is_empty() {
        cache.insert(host.to_string(), (Instant::now(), ips.clone()));
        return ips;
    }
    // Transient resolve failure: reuse the last-good set (even if past TTL) so a
    // DNS blip doesn't unpin grants and split pairs across workers.
    cache
        .get(host)
        .map(|(_, ips)| ips.clone())
        .unwrap_or_default()
}

/// Sticky per-pair relay region. A re-grant inside the TTL reuses the prior
/// choice even if a probe report moved a home meanwhile — the worker pin and
/// the region must stay stable for a LIVE pair, or its two ends land on
/// different PoPs and the relay carries nothing. TTL-expired entries recompute
/// (a dead pair's next establishment cycle may then move regions).
const PAIR_REGION_TTL: Duration = Duration::from_secs(600);
#[allow(clippy::type_complexity)]
static PAIR_REGION_CACHE: Mutex<
    Option<std::collections::HashMap<String, (Option<String>, Instant)>>,
> = Mutex::new(None);

fn sticky_pair_region(
    map: &roomler_ai_remote_control::turn_creds::TurnMap,
    pair_key: &str,
    home_a: Option<&str>,
    home_b: Option<&str>,
) -> Option<String> {
    if !map.enabled {
        return None;
    }
    let mut guard = PAIR_REGION_CACHE.lock().unwrap();
    let cache = guard.get_or_insert_with(Default::default);
    if let Some((region, at)) = cache.get(pair_key)
        && at.elapsed() < PAIR_REGION_TTL
    {
        return region.clone();
    }
    // Opportunistic sweep so dead pairs don't accrete on a long-lived pod.
    if cache.len() > 2048 {
        cache.retain(|_, (_, at)| at.elapsed() < PAIR_REGION_TTL);
    }
    let region = turn_creds::select_pair_region(map, home_a, home_b, pair_key);
    cache.insert(pair_key.to_string(), (region.clone(), Instant::now()));
    region
}

/// Resolve `host` (cached, stable) and pick one IPv4 worker, indexed by
/// `pair_key`. Both ends of a pair get the identical result → intra-worker
/// hairpin.
async fn resolve_pick_worker(host: &str, port: u16, pair_key: &str) -> Option<IpAddr> {
    let ips = resolve_workers_cached(host, port).await;
    // Both peers of a pair share the `pair_key`, and the broker hands them
    // the SAME single result, so they co-locate. The pick is the ONE shared
    // `remote_control::worker_pick` implementation (invariant I6) — the
    // overlay client computes its own fallback pick with the same fn.
    pick_worker_fnv1a(pair_key, ips)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `overlay_ip` and its inverse `overlay_host` now live in
    // `roomler_ai_remote_control::models` (the free pool needs the inverse, and
    // the model crate is shared with `services` and the agent). Their tests —
    // including the round-trip that pins the two together — moved with them.

    /// A node row with just enough shape for `to_netmap_peer`.
    fn node(name: &str, ip: &str) -> OverlayNode {
        let now = bson::DateTime::now();
        OverlayNode {
            id: Some(ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap()),
            tenant_id: ObjectId::new(),
            node_ref: NodeRef::Agent {
                agent_id: ObjectId::new(),
            },
            network_id: ObjectId::new(),
            machine_id: "machine".into(),
            name: name.into(),
            overlay_ip: ip.into(),
            wg_public_key: "cHVia2V5".into(),
            key_epoch: 0,
            endpoints: vec!["1.2.3.4:1234".into()],
            lan_endpoints: vec!["192.168.1.5:41641".into()],
            srflx_endpoints: vec!["5.6.7.8:5678".into()],
            srflx_nat: Some("cone".into()),
            relay_home: None,
            supports_quic: true,
            supports_relay_single: true,
            supports_derp: true,
            supports_forced_derp: true,
            advertised_routes: vec![],
            approved_routes: vec![],
            is_exit_node: false,
            status: AgentStatus::Online,
            last_seen_at: now,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }

    /// The leave path fans THIS shape. `reachable = false` is what makes a
    /// receiving peer render the row `offline` (`online: np.reachable`) and
    /// stop dialing it (`peer_config_from_netmap` drops unreachable peers) —
    /// instead of the row vanishing, which is what a `removes` delta did.
    #[test]
    fn leave_upsert_is_the_same_row_marked_unreachable() {
        let n = node("clk00017265-wsl", "100.64.0.7");
        let peer = to_netmap_peer(&n, false);

        assert!(
            !peer.reachable,
            "a node that just left must not be dialable"
        );
        // Identity must survive, or the receiver can't match it to the row it
        // already holds and the delta silently creates a duplicate/no-op.
        assert_eq!(peer.node_id, n.id.unwrap());
        assert_eq!(peer.overlay_ip, "100.64.0.7");
        assert_eq!(peer.name, "clk00017265-wsl");
        assert_eq!(peer.wg_public_key, "cHVia2V5");
    }

    /// Guards the join path against regressing to a blanket `true`: presence
    /// is the caller's verdict, and both verdicts must be expressible.
    #[test]
    fn to_netmap_peer_carries_presence_verbatim() {
        let n = node("mars", "100.64.0.14");
        assert!(to_netmap_peer(&n, true).reachable);
        assert!(!to_netmap_peer(&n, false).reachable);
    }

    #[test]
    fn pair_key_is_symmetric() {
        let a = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        let b = ObjectId::parse_str("507f1f77bcf86cd799439012").unwrap();
        assert_eq!(pair_key(a, b), pair_key(b, a));
        assert!(pair_key(a, b).contains(&a.to_hex()));
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

    /// worker-pick golden vector (invariant I6): the broker's pick is
    /// byte-pinned to the exact value the shared `remote_control::worker_pick`
    /// suite pins — the overlay client's fallback pick asserts the same
    /// literals, so broker↔client agreement can't silently drift.
    #[test]
    fn worker_pick_agrees_with_golden_vector() {
        let a: IpAddr = "5.9.157.221".parse().unwrap();
        let b: IpAddr = "5.9.157.226".parse().unwrap();
        let c: IpAddr = "94.130.141.74".parse().unwrap();
        let key = "507f1f77bcf86cd799439011:507f1f77bcf86cd799439012";
        // FNV-1a(key) = 0xad37_bde0_cdd9_5470; % 3 = 2 → third sorted IPv4.
        assert_eq!(pick_worker_fnv1a(key, vec![a, b, c]), Some(c));
        assert_eq!(pick_worker_fnv1a(key, vec![c, a, b, b]), Some(c)); // shuffled + dup
        // ipv6 filtered; empty → None
        let v6: IpAddr = "::1".parse().unwrap();
        assert_eq!(pick_worker_fnv1a(key, vec![v6, a]), Some(a));
        assert!(pick_worker_fnv1a(key, vec![]).is_none());
    }

    /// P7 — the churn detector's arithmetic: only grant→re-request cycles
    /// count, retry bursts and restart-storms don't, the threshold escalates,
    /// and a mid-TTL request re-escalates without re-counting.
    #[test]
    fn churn_cycles_escalate_and_restarts_do_not() {
        let t0 = Instant::now();
        let mut pc = PairChurn::default();

        // A request with NO prior grant (fresh pair / agent restart) never
        // counts — a crash-looping agent can re-request forever harmlessly.
        assert!(!churn_note_request(&mut pc, t0));
        assert_eq!(pc.cycles, 0);

        // grant → quick retry inside the dedup gap: not a cycle.
        pc.last_grant_at = Some(t0);
        assert!(!churn_note_request(&mut pc, t0 + Duration::from_secs(2)));
        assert_eq!(pc.cycles, 0);

        // Three real grant→re-request cycles ⇒ escalate on the third. (The
        // reassignments below model `note_grant_sent` arming the detector.)
        pc.last_grant_at = Some(t0);
        assert!(!churn_note_request(&mut pc, t0 + Duration::from_secs(30)));
        assert_eq!(pc.cycles, 1);
        // One cycle per grant: a second re-request without a new grant is inert.
        assert!(!churn_note_request(&mut pc, t0 + Duration::from_secs(40)));
        assert_eq!(pc.cycles, 1);
        pc.last_grant_at = Some(t0 + Duration::from_secs(45));
        assert!(!churn_note_request(&mut pc, t0 + Duration::from_secs(75)));
        assert_eq!(pc.cycles, 2);
        pc.last_grant_at = Some(t0 + Duration::from_secs(80));
        assert!(
            churn_note_request(&mut pc, t0 + Duration::from_secs(110)),
            "third cycle inside the window must escalate"
        );
        assert!(pc.forced_until.is_some(), "TTL stamped");

        // Mid-TTL request (a restarted end re-requesting): re-escalate.
        assert!(churn_note_request(&mut pc, t0 + Duration::from_secs(120)));

        // Past the TTL: back to normal counting (first request post-expiry
        // has no grant on record ⇒ not churn).
        let after = t0 + Duration::from_secs(110) + FORCED_DERP_TTL + Duration::from_secs(1);
        assert!(!churn_note_request(&mut pc, after));
        assert_eq!(pc.cycles, 0);
    }

    /// P7 — cycles outside the sliding window don't accumulate.
    #[test]
    fn churn_window_resets_stale_counts() {
        let t0 = Instant::now();
        let mut pc = PairChurn {
            last_grant_at: Some(t0),
            ..Default::default()
        };
        assert!(!churn_note_request(&mut pc, t0 + Duration::from_secs(30)));
        assert_eq!(pc.cycles, 1);
        // Next cycle lands AFTER the window: the count restarts at 1.
        let late = t0 + CHURN_WINDOW + Duration::from_secs(60);
        pc.last_grant_at = Some(late - Duration::from_secs(30));
        assert!(!churn_note_request(&mut pc, late));
        assert_eq!(pc.cycles, 1, "stale window must reset, not accumulate");
    }
}

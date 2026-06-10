//! Coturn-relay carrier coordination for the overlay runtime (Phase 3b).
//!
//! **Same-worker pin (rc.125).** The first field bring-up failed because
//! the two nodes allocated on *different* coturn workers, and cross-worker
//! relay-to-relay drops under mars's dual-public-IP SNAT (the exact issue
//! the QUIC tunnel fixed in rc.112). The fix here mirrors that pin: the
//! deterministic **initiator** (smaller WG public key) allocates its relay
//! round-robin and advertises it first; the **responder** then allocates on
//! the *initiator's* coturn worker — an intra-worker hairpin with no
//! cross-worker SNAT. See `agents/roomler-tunnel/src/forward.rs`
//! (`setup_quic_over_relay`) for the QUIC original.
//!
//! Per-peer flow (each side does this symmetrically):
//! 1. peer appears → [`request`](RelayCoordinator::request) sends
//!    `rc:overlay.relay_request` and stashes the peer config + whether we
//!    initiate.
//! 2. `rc:overlay.relay_grant` → [`on_grant`](RelayCoordinator::on_grant):
//!    the initiator allocates now (round-robin) + advertises; the responder
//!    defers until it knows the initiator's relayed address.
//! 3. the peer's relayed address arrives in a netmap delta →
//!    [`maybe_complete`](RelayCoordinator::maybe_complete): the responder
//!    allocates *pinned to that worker* + advertises, and both sides build
//!    the `Carrier::relay`.
//!
//! **Still field-pending:** validated only against live coturn + two hosts.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use bson::oid::ObjectId;
use tracing::{debug, info, warn};

use super::netmap::PeerConfig;
use super::wg::Carrier;
use crate::transport::relay::{RelayConn, allocate_relay_from_ice};
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer};

/// A peer link whose carrier is ready to install.
pub struct ReadyLink {
    pub node_id: ObjectId,
    pub public_key: [u8; 32],
    pub overlay_ip: Ipv4Addr,
    pub carrier: Arc<Carrier>,
}

/// A peer we're coordinating a relay link to, before our allocation exists.
struct PendingPeer {
    peer: PeerConfig,
    /// Do *we* initiate this link (our WG pubkey is the smaller)? The
    /// initiator allocates round-robin first; the responder pins to the
    /// initiator's worker.
    initiate: bool,
    /// coturn creds from `relay_grant` (`None` until granted).
    ice: Option<Vec<IceServer>>,
}

/// A relay allocation made for one peer, awaiting that peer's relayed
/// address before the carrier can be built.
struct Allocated {
    conn: Arc<dyn RelayConn>,
    peer: PeerConfig,
}

/// Drives the relay handshake for every peer the node wants to reach.
pub struct RelayCoordinator {
    outbound: tokio::sync::mpsc::Sender<ClientMsg>,
    /// Requested (and maybe granted), not yet allocated.
    pending: HashMap<ObjectId, PendingPeer>,
    /// Allocated + advertised; awaiting the peer's relayed address.
    allocated: HashMap<ObjectId, Allocated>,
    /// Our relayed address **per peer** (peer node_id → the relay we
    /// allocated for that link). Keyed so a re-allocation *replaces* and
    /// [`forget`](Self::forget) *prunes* the entry — a flat append-only
    /// list let a relay torn down in an earlier churn cycle linger in the
    /// advertised set, and the peer (which dials `endpoints[0]`) then sent
    /// WireGuard to a dead allocation forever. That was the rc.125 field
    /// failure (a node dialed `:11110`, a relay closed three churn cycles
    /// earlier). Each `endpoints` trickle carries every *current* value.
    advertised: HashMap<ObjectId, String>,
}

impl RelayCoordinator {
    pub fn new(outbound: tokio::sync::mpsc::Sender<ClientMsg>) -> Self {
        Self {
            outbound,
            pending: HashMap::new(),
            allocated: HashMap::new(),
            advertised: HashMap::new(),
        }
    }

    /// Already coordinating a link to this peer (pending or allocated)?
    pub fn is_tracking(&self, node_id: &ObjectId) -> bool {
        self.pending.contains_key(node_id) || self.allocated.contains_key(node_id)
    }

    /// Kick off a relay link: ask the server for coturn creds, stashing the
    /// peer config + whether we initiate (drives the same-worker pin).
    pub async fn request(&mut self, node_id: ObjectId, peer: PeerConfig, initiate: bool) {
        if self.is_tracking(&node_id) {
            return;
        }
        if self
            .outbound
            .send(ClientMsg::OverlayRelayRequest {
                peer_node_id: node_id,
            })
            .await
            .is_err()
        {
            warn!(peer = %node_id, "overlay relay: control channel closed; cannot request");
            return;
        }
        self.pending.insert(
            node_id,
            PendingPeer {
                peer,
                initiate,
                ice: None,
            },
        );
        debug!(peer = %node_id, initiate, "overlay relay: requested coturn creds");
    }

    /// Got coturn creds. The initiator allocates immediately (round-robin);
    /// the responder waits until it knows the initiator's relayed address so
    /// it can pin to the same coturn worker.
    pub async fn on_grant(
        &mut self,
        node_id: ObjectId,
        ice_servers: Vec<IceServer>,
    ) -> Option<ReadyLink> {
        let ready_to_alloc = {
            let pp = self.pending.get_mut(&node_id)?;
            pp.ice = Some(ice_servers);
            pp.initiate || peer_worker_ip(&pp.peer).is_some()
        };
        if ready_to_alloc {
            self.allocate_and_store(node_id).await
        } else {
            debug!(peer = %node_id, "overlay relay: responder waiting for the initiator's relay addr before pinning");
            None
        }
    }

    /// A fresh netmap view arrived. Refresh the peer config; if we're a
    /// responder that was waiting for the initiator's relayed address, it may
    /// now be known — allocate (pinned) and build the carrier.
    pub async fn maybe_complete(
        &mut self,
        node_id: ObjectId,
        peer: &PeerConfig,
    ) -> Option<ReadyLink> {
        if let Some(a) = self.allocated.get_mut(&node_id) {
            a.peer = peer.clone();
            return self.try_build(&node_id);
        }
        let should_alloc = if let Some(pp) = self.pending.get_mut(&node_id) {
            pp.peer = peer.clone();
            pp.ice.is_some() && !pp.initiate && peer_worker_ip(&pp.peer).is_some()
        } else {
            false
        };
        if should_alloc {
            return self.allocate_and_store(node_id).await;
        }
        None
    }

    /// Allocate this peer's relay (pinned to its coturn worker iff we're the
    /// responder and know its address), advertise it, move it to `allocated`,
    /// and try to build the carrier.
    async fn allocate_and_store(&mut self, node_id: ObjectId) -> Option<ReadyLink> {
        let (ice, peer, initiate) = {
            let pp = self.pending.get(&node_id)?;
            (pp.ice.clone()?, pp.peer.clone(), pp.initiate)
        };
        // Same-worker pin: a responder follows the initiator onto its worker.
        let pin = if initiate {
            None
        } else {
            peer_worker_ip(&peer)
        };
        let conn = self.allocate(&ice, pin).await?;
        if let Ok(own) = conn.local_addr() {
            info!(peer = %node_id, %own, pinned = pin.is_some(), "overlay relay: allocated");
            // Per-peer (not append-only) so this replaces any prior relay we
            // allocated for the same peer across a churn cycle — see the
            // `advertised` field doc. A peer reads `endpoints[0]`, so a stale
            // relay must never outlive its allocation here.
            self.advertised.insert(node_id, own.to_string());
            let _ = self
                .outbound
                .send(ClientMsg::OverlayEndpoints {
                    candidates: self.advertised.values().cloned().collect(),
                })
                .await;
        }
        self.pending.remove(&node_id);
        self.allocated.insert(node_id, Allocated { conn, peer });
        self.try_build(&node_id)
    }

    /// Allocate a coturn relay. With `pin = Some(ip)` the peer's coturn
    /// worker is tried first (UDP, then TURNS/TCP for UDP-blocked corp
    /// hosts), so the relay-to-relay path becomes an intra-worker hairpin.
    async fn allocate(&self, ice: &[IceServer], pin: Option<IpAddr>) -> Option<Arc<dyn RelayConn>> {
        let (urls, user, cred) = turn_creds(ice)?;
        let urls = match pin {
            Some(ip) => {
                let h = if ip.is_ipv6() {
                    format!("[{ip}]")
                } else {
                    ip.to_string()
                };
                let mut pinned = vec![
                    format!("turn:{h}:3478?transport=udp"),
                    format!("turns:{h}:443?transport=tcp"),
                ];
                pinned.extend(urls);
                pinned
            }
            None => urls,
        };
        match allocate_relay_from_ice(&urls, &user, &cred).await {
            Ok(c) => Some(Arc::new(c) as Arc<dyn RelayConn>),
            Err(e) => {
                warn!(%e, pinned = pin.is_some(), "overlay relay: allocate failed");
                None
            }
        }
    }

    /// Build the carrier once we have an allocation AND a dialable peer
    /// address. On success the link leaves `allocated`.
    fn try_build(&mut self, node_id: &ObjectId) -> Option<ReadyLink> {
        let a = self.allocated.get(node_id)?;
        let dst: SocketAddr = a.peer.endpoints.iter().find_map(|e| e.parse().ok())?;
        let carrier = Carrier::relay(a.conn.clone(), dst);
        let link = ReadyLink {
            node_id: *node_id,
            public_key: a.peer.public_key,
            overlay_ip: a.peer.overlay_ip,
            carrier,
        };
        self.allocated.remove(node_id);
        info!(peer = %node_id, %dst, "overlay relay: link ready");
        Some(link)
    }

    /// Drop all state for a peer (it left the netmap), including the relay
    /// we advertised for it — so when the peer's WG carrier is torn down
    /// (`wg.remove_peer`) and the underlying allocation closes, we stop
    /// advertising that now-dead address. Without this the next
    /// `OverlayEndpoints` trickle still carries the stale relay and a
    /// re-joining peer dials it (the rc.125 accumulation bug).
    pub fn forget(&mut self, node_id: &ObjectId) {
        self.pending.remove(node_id);
        self.allocated.remove(node_id);
        self.advertised.remove(node_id);
    }
}

/// Pull `(urls, username, credential)` out of the first ICE server that
/// carries REST-API short-lived TURN creds (the coturn entry).
fn turn_creds(ice_servers: &[IceServer]) -> Option<(Vec<String>, String, String)> {
    ice_servers.iter().find_map(|s| {
        let user = s.username.clone()?;
        let cred = s.credential.clone()?;
        Some((s.urls.clone(), user, cred))
    })
}

/// The coturn worker IP a peer is on, from its advertised relayed address.
fn peer_worker_ip(peer: &PeerConfig) -> Option<IpAddr> {
    peer.endpoints
        .iter()
        .find_map(|e| e.parse::<SocketAddr>().ok())
        .map(|s| s.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ice(url: &str) -> IceServer {
        IceServer {
            urls: vec![url.into()],
            username: Some("u".into()),
            credential: Some("c".into()),
        }
    }

    #[test]
    fn turn_creds_picks_the_authed_entry() {
        let servers = vec![
            IceServer {
                urls: vec!["stun:stun.example:3478".into()],
                username: None,
                credential: None,
            },
            ice("turn:coturn.example:3478?transport=udp"),
        ];
        let (urls, u, c) = turn_creds(&servers).expect("authed entry");
        assert_eq!(urls, vec!["turn:coturn.example:3478?transport=udp"]);
        assert_eq!((u.as_str(), c.as_str()), ("u", "c"));
        assert!(turn_creds(&[]).is_none());
    }

    #[test]
    fn peer_worker_ip_reads_first_endpoint() {
        let peer = PeerConfig {
            public_key: [1u8; 32],
            overlay_ip: Ipv4Addr::new(100, 64, 0, 9),
            endpoints: vec!["5.9.157.226:11696".into()],
        };
        assert_eq!(peer_worker_ip(&peer), Some("5.9.157.226".parse().unwrap()));
        let no_ep = PeerConfig {
            endpoints: vec![],
            ..peer
        };
        assert_eq!(peer_worker_ip(&no_ep), None);
    }

    #[tokio::test]
    async fn request_is_idempotent_and_sends_one_relay_request() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let mut coord = RelayCoordinator::new(tx);
        let node = ObjectId::new();
        let peer = PeerConfig {
            public_key: [1u8; 32],
            overlay_ip: Ipv4Addr::new(100, 64, 0, 9),
            endpoints: vec![],
        };
        coord.request(node, peer.clone(), true).await;
        coord.request(node, peer, true).await; // de-duped
        assert!(coord.is_tracking(&node));
        assert!(matches!(
            rx.recv().await,
            Some(ClientMsg::OverlayRelayRequest { peer_node_id }) if peer_node_id == node
        ));
        assert!(rx.try_recv().is_err()); // only one request sent
    }

    #[test]
    fn forget_prunes_the_advertised_relay() {
        // rc.126 regression lock: a churn-removed peer must drop the relay
        // we advertised for it, or the next `OverlayEndpoints` trickle keeps
        // carrying a now-dead allocation and the peer dials it forever.
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let mut coord = RelayCoordinator::new(tx);
        let node = ObjectId::new();
        coord.advertised.insert(node, "94.130.141.74:11085".into());
        coord.pending.insert(
            node,
            PendingPeer {
                peer: PeerConfig {
                    public_key: [2u8; 32],
                    overlay_ip: Ipv4Addr::new(100, 64, 0, 9),
                    endpoints: vec![],
                },
                initiate: false,
                ice: None,
            },
        );
        assert!(coord.is_tracking(&node));
        coord.forget(&node);
        assert!(!coord.is_tracking(&node));
        assert!(
            coord.advertised.is_empty(),
            "forget must prune the advertised relay so a re-joining peer can't dial a dead allocation"
        );
    }
}

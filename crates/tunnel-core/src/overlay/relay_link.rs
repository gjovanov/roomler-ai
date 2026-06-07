//! Coturn-relay carrier coordination for the overlay runtime (Phase 3b).
//!
//! **FIELD-PENDING.** Unlike the rest of the overlay (loopback-proven),
//! the relay carrier can only be validated on real hosts against live
//! coturn — exactly like the QUIC tunnel's relay path, which took
//! several RCs of field iteration (the same-worker pin, the dual-IP SNAT
//! quirk). This module ships the *mechanics* (request → grant → allocate
//! → advertise → build) compile-verified; the live address-exchange
//! timing, endpoint disambiguation, and same-worker pin are tuned in the
//! field. First-cut simplifications are marked `FIELD:` below.
//!
//! Per-peer flow (both ends do this symmetrically):
//! 1. peer appears in the netmap → [`RelayCoordinator::request`] sends
//!    `rc:overlay.relay_request` and stashes the peer's config.
//! 2. server replies `rc:overlay.relay_grant` → [`RelayCoordinator::on_grant`]
//!    allocates a coturn relay (`allocate_relay_from_ice`), advertises
//!    its own relayed address via `rc:overlay.endpoints` so the peer can
//!    dial it, and stores the allocation.
//! 3. the peer's relayed address arrives in a later netmap (it advertised
//!    its own) → [`RelayCoordinator::maybe_complete`] builds the
//!    `Carrier::relay` and yields a [`ReadyLink`] the runtime installs.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
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

/// A relay allocation made for one peer, awaiting that peer's relayed
/// address before the carrier can be built.
struct Allocated {
    conn: Arc<dyn RelayConn>,
    peer: PeerConfig,
}

/// Drives the relay handshake for every peer the node wants to reach.
pub struct RelayCoordinator {
    outbound: tokio::sync::mpsc::Sender<ClientMsg>,
    /// Requested a grant; awaiting `relay_grant`. Stores the peer config
    /// so `on_grant` knows the pubkey / overlay IP.
    pending: HashMap<ObjectId, PeerConfig>,
    /// Allocated + advertised; awaiting the peer's relayed address.
    allocated: HashMap<ObjectId, Allocated>,
    /// Every relayed address we've allocated this session — each
    /// `endpoints` trickle carries all of them so a peer never misses one.
    advertised: Vec<String>,
}

impl RelayCoordinator {
    pub fn new(outbound: tokio::sync::mpsc::Sender<ClientMsg>) -> Self {
        Self {
            outbound,
            pending: HashMap::new(),
            allocated: HashMap::new(),
            advertised: Vec::new(),
        }
    }

    /// Already coordinating a link to this peer (pending or allocated)?
    pub fn is_tracking(&self, node_id: &ObjectId) -> bool {
        self.pending.contains_key(node_id) || self.allocated.contains_key(node_id)
    }

    /// Kick off a relay link: ask the server for short-lived coturn creds
    /// for this peer, stashing its config for `on_grant`.
    pub async fn request(&mut self, node_id: ObjectId, peer: PeerConfig) {
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
        self.pending.insert(node_id, peer);
        debug!(peer = %node_id, "overlay relay: requested coturn creds");
    }

    /// Got coturn creds for `node_id`: allocate our own relay, advertise
    /// its address (so the peer can dial us), and try to complete the link
    /// if the peer already advertised its address.
    pub async fn on_grant(
        &mut self,
        node_id: ObjectId,
        ice_servers: Vec<IceServer>,
    ) -> Option<ReadyLink> {
        let Some(peer) = self.pending.remove(&node_id) else {
            debug!(peer = %node_id, "overlay relay: grant for an untracked/linked peer; ignoring");
            return None;
        };
        let (urls, user, cred) = turn_creds(&ice_servers)?;
        let conn: Arc<dyn RelayConn> = match allocate_relay_from_ice(&urls, &user, &cred).await {
            Ok(c) => Arc::new(c),
            Err(e) => {
                warn!(peer = %node_id, %e, "overlay relay: allocate failed");
                return None;
            }
        };
        // Advertise our relayed address so the peer can dial it.
        if let Ok(own) = conn.local_addr() {
            let own = own.to_string();
            if !self.advertised.contains(&own) {
                self.advertised.push(own);
            }
            let _ = self
                .outbound
                .send(ClientMsg::OverlayEndpoints {
                    candidates: self.advertised.clone(),
                })
                .await;
        }
        self.allocated.insert(node_id, Allocated { conn, peer });
        self.try_build(&node_id)
    }

    /// A fresh netmap view of `node_id` arrived (possibly now carrying its
    /// relayed address). Refresh the stored peer + try to finish the link.
    pub fn maybe_complete(&mut self, node_id: ObjectId, peer: &PeerConfig) -> Option<ReadyLink> {
        if let Some(a) = self.allocated.get_mut(&node_id) {
            a.peer = peer.clone();
            return self.try_build(&node_id);
        }
        None
    }

    /// Build the carrier once we have an allocation AND a dialable peer
    /// address. On success the link leaves `allocated`.
    fn try_build(&mut self, node_id: &ObjectId) -> Option<ReadyLink> {
        let a = self.allocated.get(node_id)?;
        // FIELD: relay-only peers advertise just their relayed address via
        // `rc:overlay.endpoints`, so the first parseable endpoint is it. A
        // peer that also carries direct candidates needs server-side relay
        // tagging to disambiguate — a later cut.
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

    /// Drop all state for a peer (it left the netmap).
    pub fn forget(&mut self, node_id: &ObjectId) {
        self.pending.remove(node_id);
        self.allocated.remove(node_id);
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
        coord.request(node, peer.clone()).await;
        coord.request(node, peer).await; // de-duped
        assert!(coord.is_tracking(&node));
        assert!(matches!(
            rx.recv().await,
            Some(ClientMsg::OverlayRelayRequest { peer_node_id }) if peer_node_id == node
        ));
        assert!(rx.try_recv().is_err()); // only one request sent
    }
}

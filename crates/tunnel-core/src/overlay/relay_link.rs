//! Coturn-relay carrier coordination for the overlay runtime (Phase 3b).
//!
//! **Deterministic worker (rc.127).** The relay-to-relay leg must hairpin on
//! ONE coturn worker: cross-worker relay traffic drops under mars's
//! dual-public-IP SNAT (the issue the QUIC tunnel fixed in rc.112). rc.125
//! pinned the *responder* onto the *initiator's* worker by reading the
//! initiator's advertised relayed address — but that read is racy: on
//! (re)start the initiator's current relay hasn't propagated yet, so the
//! responder pinned to a **stale** worker and never re-pinned, leaving the
//! pair split and the WireGuard handshake timing out forever (field bring-up
//! 2026-06-10: a restart merely *swapped* which side read stale).
//!
//! rc.127 removes the dependence on the peer's endpoint entirely: **both ends
//! pick the same coturn worker deterministically from the shared `pair_key`**
//! — a stable hash over the *resolved* coturn worker IPs. Same `pair_key`
//! (the server sends an identical `sorted(a,b)` to both) + same DNS record →
//! same sorted IP list → same index → same worker, with zero dependence on
//! propagation timing. No race, no latch; the hairpin is guaranteed.
//!
//! Per-peer flow (symmetric on both sides):
//! 1. peer appears → [`request`](RelayCoordinator::request) sends
//!    `rc:overlay.relay_request`.
//! 2. `rc:overlay.relay_grant` (coturn creds + `pair_key`) →
//!    [`on_grant`](RelayCoordinator::on_grant): allocate immediately, pinned
//!    to the deterministic worker, and advertise our relayed address.
//! 3. the peer's relayed address arrives in a netmap delta →
//!    [`maybe_complete`](RelayCoordinator::maybe_complete): build the
//!    `Carrier::relay` dialing it.
//!
//! **Still field-pending.**

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use bson::oid::ObjectId;
use tokio::net::lookup_host;
use tracing::{debug, info, warn};

use super::netmap::PeerConfig;
use super::wg::Carrier;
use crate::transport::relay::{RelayConn, allocate_relay_from_ice};
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer};

/// A peer link whose carrier is ready to install.
pub struct ReadyLink {
    pub node_id: ObjectId,
    pub public_key: [u8; 32],
    pub overlay_ip: std::net::Ipv4Addr,
    pub carrier: Arc<Carrier>,
}

/// A peer we're coordinating a relay link to, before our allocation exists.
struct PendingPeer {
    peer: PeerConfig,
    /// coturn creds from `relay_grant` (`None` until granted).
    ice: Option<Vec<IceServer>>,
    /// symmetric per-pair key from `relay_grant` — drives the deterministic
    /// worker pick so both ends land on the same coturn worker (`None` until
    /// granted).
    pair_key: Option<String>,
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
    /// [`forget`](Self::forget) *prunes* the entry — a flat append-only list
    /// let a relay torn down in an earlier churn cycle linger in the
    /// advertised set, and the peer (which dials `endpoints[0]`) then sent
    /// WireGuard to a dead allocation forever (the rc.125→126 field failure).
    /// Each `endpoints` trickle carries every *current* value.
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

    /// Kick off a relay link: ask the server for coturn creds + the pair_key.
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
        self.pending.insert(
            node_id,
            PendingPeer {
                peer,
                ice: None,
                pair_key: None,
            },
        );
        debug!(peer = %node_id, "overlay relay: requested coturn creds");
    }

    /// Got coturn creds + `pair_key`. Allocate immediately, pinned to the
    /// deterministic worker (identical on both ends — no dependence on the
    /// peer's advertised endpoint), advertise our relayed address, and try to
    /// build the carrier.
    pub async fn on_grant(
        &mut self,
        node_id: ObjectId,
        ice_servers: Vec<IceServer>,
        pair_key: String,
    ) -> Option<ReadyLink> {
        {
            let pp = self.pending.get_mut(&node_id)?;
            pp.ice = Some(ice_servers);
            pp.pair_key = Some(pair_key);
        }
        self.allocate_and_store(node_id).await
    }

    /// A fresh netmap view arrived. Refresh the peer config; if we've already
    /// allocated, the peer's relayed address may now be known — build.
    pub fn maybe_complete(&mut self, node_id: ObjectId, peer: &PeerConfig) -> Option<ReadyLink> {
        if let Some(a) = self.allocated.get_mut(&node_id) {
            a.peer = peer.clone();
            return self.try_build(&node_id);
        }
        if let Some(pp) = self.pending.get_mut(&node_id) {
            pp.peer = peer.clone();
        }
        None
    }

    /// Allocate this peer's relay pinned to the deterministic worker,
    /// advertise it, move it to `allocated`, and try to build the carrier.
    async fn allocate_and_store(&mut self, node_id: ObjectId) -> Option<ReadyLink> {
        let (ice, peer, pair_key) = {
            let pp = self.pending.get(&node_id)?;
            (pp.ice.clone()?, pp.peer.clone(), pp.pair_key.clone()?)
        };
        // Deterministic same-worker pick: both ends derive the identical
        // worker from the shared pair_key, with NO dependence on the peer's
        // (racy) advertised endpoint.
        let pin = pick_worker(&pair_key, &ice).await;
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

    /// Allocate a coturn relay. With `pin = Some(ip)` that worker is tried
    /// first (UDP, then TURNS/TCP for UDP-blocked corp hosts), so the
    /// relay-to-relay path becomes an intra-worker hairpin.
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

/// Hostname of the first `turn:`/`turns:` ICE url (e.g. `coturn.roomler.ai`).
/// Strips the scheme + `:port` + `?query`.
fn turn_host(ice: &[IceServer]) -> Option<String> {
    ice.iter().flat_map(|s| s.urls.iter()).find_map(|u| {
        let rest = u
            .strip_prefix("turns:")
            .or_else(|| u.strip_prefix("turn:"))?;
        let host = rest.split([':', '?']).next()?;
        (!host.is_empty()).then(|| host.to_string())
    })
}

/// Stable 64-bit FNV-1a — deterministic across nodes (unlike the stdlib
/// `DefaultHasher`, which is seeded per-process). Both peers MUST compute the
/// same worker index for the same `pair_key`, so the hash has to be fixed.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Pick ONE coturn worker IP from the (sorted, deduped) candidate set,
/// indexed by `pair_key`. Pure + deterministic — the testable core of
/// [`pick_worker`].
fn pick_from_ips(pair_key: &str, mut ips: Vec<IpAddr>) -> Option<IpAddr> {
    ips.retain(IpAddr::is_ipv4);
    ips.sort();
    ips.dedup();
    if ips.is_empty() {
        return None;
    }
    let idx = (fnv1a(pair_key.as_bytes()) % ips.len() as u64) as usize;
    Some(ips[idx])
}

/// Resolve the coturn host from the ICE creds and pick ONE worker IP
/// deterministically from `pair_key`, so both peers of the pair independently
/// choose the same worker (intra-worker relay hairpin — no cross-worker
/// SNAT). `None` (→ no pin → round-robin) when there's no TURN url or DNS
/// resolution fails, which degrades to the pre-rc.125 behaviour rather than
/// failing the allocation.
async fn pick_worker(pair_key: &str, ice: &[IceServer]) -> Option<IpAddr> {
    let host = turn_host(ice)?;
    let ips: Vec<IpAddr> = match lookup_host((host.as_str(), 3478u16)).await {
        Ok(addrs) => addrs.map(|s| s.ip()).collect(),
        Err(e) => {
            warn!(%host, %e, "overlay relay: coturn DNS resolve failed; not pinning a worker");
            return None;
        }
    };
    let pick = pick_from_ips(pair_key, ips);
    if let Some(ip) = pick {
        debug!(%host, worker = %ip, "overlay relay: deterministic worker picked");
    }
    pick
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

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
    fn turn_host_strips_scheme_port_query() {
        let servers = vec![
            IceServer {
                urls: vec!["stun:stun.l.google.com:19302".into()],
                username: None,
                credential: None,
            },
            ice("turn:coturn.roomler.ai:3478?transport=udp"),
        ];
        assert_eq!(turn_host(&servers).as_deref(), Some("coturn.roomler.ai"));
        assert_eq!(
            turn_host(&[ice("turns:coturn.roomler.ai:443?transport=tcp")]).as_deref(),
            Some("coturn.roomler.ai")
        );
        // stun-only / empty → no host
        assert_eq!(
            turn_host(&[IceServer {
                urls: vec!["stun:stun.example:3478".into()],
                username: None,
                credential: None,
            }]),
            None
        );
    }

    #[test]
    fn deterministic_worker_pick_is_stable_and_symmetric() {
        // The same pair_key always selects the same worker, regardless of the
        // order DNS returned the IPs (both peers MUST agree).
        let a = "5.9.157.221".parse().unwrap();
        let b = "5.9.157.226".parse().unwrap();
        let c = "94.130.141.74".parse().unwrap();
        let key = "507f1f77bcf86cd799439011:507f1f77bcf86cd799439012";
        let p1 = pick_from_ips(key, vec![a, b, c]).unwrap();
        let p2 = pick_from_ips(key, vec![c, a, b]).unwrap(); // shuffled
        let p3 = pick_from_ips(key, vec![b, c, a, b]).unwrap(); // dup + shuffled
        assert_eq!(p1, p2, "order must not change the pick (sorted internally)");
        assert_eq!(p1, p3, "dups must not change the pick");
        assert!([a, b, c].contains(&p1));
        // a different pair_key may land elsewhere but is itself stable
        let other = pick_from_ips("aaa:bbb", vec![a, b, c]).unwrap();
        assert_eq!(other, pick_from_ips("aaa:bbb", vec![b, a, c]).unwrap());
        // ipv6 entries are filtered out
        let v6: IpAddr = "::1".parse().unwrap();
        assert_eq!(pick_from_ips(key, vec![v6, a]).unwrap(), a);
        assert!(pick_from_ips(key, vec![v6]).is_none());
        assert!(pick_from_ips(key, vec![]).is_none());
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
                ice: None,
                pair_key: None,
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

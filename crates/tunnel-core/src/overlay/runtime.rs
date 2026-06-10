//! Overlay node runtime (Phase 3b).
//!
//! Drives one node's membership in the overlay mesh: announces itself
//! (`rc:overlay.join`), applies the server's netmap (install / drop a
//! WireGuard peer per entry), brings up the TUN, and pumps packets
//! between the TUN and the [`WgDevice`](super::wg::WgDevice).
//!
//! The runtime **owns** the `WgDevice` and runs a single `select!` loop:
//! a TUN read (→ `send_ip_packet`) and a netmap event (→ `add_peer` /
//! `remove_peer`) never run concurrently, so the `&`/`&mut` borrows don't
//! collide and no interior mutability is needed. Only the inbound writer
//! (decrypted `tun_rx` → TUN) is a separate task — it never touches the
//! device.
//!
//! Carrier construction (direct UDP vs coturn relay) is delegated to a
//! [`LinkFactory`] so this orchestration is testable with loopback
//! carriers + a mock TUN, and so the corp-NAT relay path can be added
//! without reworking the runtime.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use async_trait::async_trait;
use bson::oid::ObjectId;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::WgKeypair;
use super::netmap::{PeerConfig, peer_config_from_netmap};
use super::relay_link::{ReadyLink, RelayCoordinator};
use super::tun::TunIo;
use super::wg::{Carrier, WgDevice};
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer, NetmapPeer, OverlayNetworkInfo};

/// Overlay control events the runtime consumes, fed in from the node's
/// signaling loop (the `ServerMsg::Overlay*` handlers forward these).
#[derive(Debug, Clone)]
pub enum OverlayEvent {
    /// Full snapshot — carries the node's own `self_ip`, so the first one
    /// triggers TUN bring-up.
    Netmap {
        self_ip: String,
        network: OverlayNetworkInfo,
        peers: Vec<NetmapPeer>,
    },
    /// Incremental update.
    NetmapDelta {
        upserts: Vec<NetmapPeer>,
        removes: Vec<ObjectId>,
    },
    /// Coturn creds for a relay leg to `peer_node_id` (relay mode only).
    /// `pair_key` is the server's symmetric `sorted(a,b)` key — both ends
    /// receive an identical value and use it to pick the same coturn worker.
    RelayGrant {
        peer_node_id: ObjectId,
        ice_servers: Vec<IceServer>,
        pair_key: String,
    },
}

/// Builds the WG carrier for a peer. Production wires a direct UDP socket
/// or a coturn relay; tests inject pre-wired loopback carriers. Returning
/// `None` skips the peer (it is retried on the next netmap that lists it).
#[async_trait]
pub trait LinkFactory: Send + Sync {
    async fn build_carrier(&self, peer: &PeerConfig) -> Option<Arc<Carrier>>;
}

/// Creates the TUN once the node's overlay IP is known. Production
/// returns `SystemTun`; tests return a mock. Boxed so the runtime stays
/// device-agnostic. Args: `(self_ip, netmask, mtu)`.
pub type TunFactory =
    Box<dyn Fn(Ipv4Addr, Ipv4Addr, u16) -> std::io::Result<Arc<dyn TunIo>> + Send + Sync>;

/// IPv4 netmask for a CIDR prefix length (e.g. `10` → `255.192.0.0`).
fn netmask_for_prefix(prefix: u8) -> Ipv4Addr {
    if prefix == 0 {
        return Ipv4Addr::UNSPECIFIED;
    }
    Ipv4Addr::from(!0u32 << (32 - u32::from(prefix.min(32))))
}

/// Prefix length out of a `"a.b.c.d/n"` CIDR string.
fn prefix_of_cidr(cidr: &str) -> Option<u8> {
    cidr.split_once('/')
        .and_then(|(_, p)| p.trim().parse().ok())
}

/// How the runtime obtains a carrier for each peer.
enum CarrierMode {
    /// Direct/test: a stateless [`LinkFactory`] builds the carrier
    /// immediately (loopback in tests).
    Direct(Arc<dyn LinkFactory>),
    /// Production: coturn relay coordination ([`RelayCoordinator`]) —
    /// field-pending.
    Relay,
}

/// One node's overlay runtime. Construct with [`OverlayRuntime::new`] (or
/// [`new_relay`](OverlayRuntime::new_relay)), then
/// `tokio::spawn(rt.run(events, endpoints))`.
pub struct OverlayRuntime {
    keypair: WgKeypair,
    outbound: mpsc::Sender<ClientMsg>,
    mode: CarrierMode,
    tun_factory: TunFactory,
    mtu: u16,
}

impl OverlayRuntime {
    /// Direct/test runtime: carriers come from `links`.
    pub fn new(
        keypair: WgKeypair,
        outbound: mpsc::Sender<ClientMsg>,
        links: Arc<dyn LinkFactory>,
        tun_factory: TunFactory,
        mtu: u16,
    ) -> Self {
        Self {
            keypair,
            outbound,
            mode: CarrierMode::Direct(links),
            tun_factory,
            mtu,
        }
    }

    /// Production runtime: carriers come from the coturn relay
    /// coordination (field-pending).
    pub fn new_relay(
        keypair: WgKeypair,
        outbound: mpsc::Sender<ClientMsg>,
        tun_factory: TunFactory,
        mtu: u16,
    ) -> Self {
        Self {
            keypair,
            outbound,
            mode: CarrierMode::Relay,
            tun_factory,
            mtu,
        }
    }

    /// Run until the event channel closes (WS disconnect). Sends
    /// `OverlayJoin`, waits for the first full netmap (which yields the
    /// node's overlay IP), brings up the TUN + inbound writer, then
    /// steady-state pumps TUN traffic and applies netmap deltas.
    pub async fn run(self, mut events: mpsc::Receiver<OverlayEvent>, endpoints: Vec<String>) {
        let join = ClientMsg::OverlayJoin {
            network_hint: None,
            wg_public_key: self.keypair.public_base64(),
            key_epoch: 0,
            supported: vec!["wireguard-v1".to_string()],
            mtu: self.mtu,
            endpoints,
        };
        if self.outbound.send(join).await.is_err() {
            warn!("overlay: control channel closed before join");
            return;
        }
        info!("overlay: rc:overlay.join sent");

        // Phase 1 — wait for the first full netmap (it carries self_ip).
        let (self_ip, network, first_peers) = loop {
            match events.recv().await {
                Some(OverlayEvent::Netmap {
                    self_ip,
                    network,
                    peers,
                }) => break (self_ip, network, peers),
                Some(OverlayEvent::NetmapDelta { .. }) => continue, // pre-netmap; ignore
                Some(OverlayEvent::RelayGrant { .. }) => continue,  // pre-netmap; ignore
                None => return,
            }
        };

        let Ok(self_v4) = self_ip.parse::<Ipv4Addr>() else {
            warn!(%self_ip, "overlay: server sent a non-IPv4 self_ip; aborting runtime");
            return;
        };
        let netmask = netmask_for_prefix(prefix_of_cidr(&network.cidr).unwrap_or(10));

        let (mut wg, tun_rx) = WgDevice::new(self.keypair.secret.clone());
        let tun: Arc<dyn TunIo> = match (self.tun_factory)(self_v4, netmask, self.mtu) {
            Ok(t) => t,
            Err(e) => {
                warn!(%e, %self_v4, "overlay: TUN bring-up failed; aborting runtime");
                return;
            }
        };
        info!(%self_v4, mtu = self.mtu, "overlay: TUN up");

        // Inbound writer: decrypted packets → TUN. Independent of the
        // device, so it's a plain spawned task.
        let writer_tun = tun.clone();
        let inbound = tokio::spawn(async move {
            let mut rx = tun_rx;
            while let Some(pkt) = rx.recv().await {
                if let Err(e) = writer_tun.write_packet(&pkt).await {
                    debug!(%e, "overlay: TUN write failed; inbound writer exiting");
                    break;
                }
            }
        });

        let mut by_node: HashMap<ObjectId, [u8; 32]> = HashMap::new();
        let mut relay = match self.mode {
            CarrierMode::Relay => Some(RelayCoordinator::new(self.outbound.clone())),
            CarrierMode::Direct(_) => None,
        };
        self.install_peers(&mut wg, &mut by_node, &mut relay, &first_peers)
            .await;

        // Phase 2 — steady state.
        loop {
            tokio::select! {
                read = tun.read_packet() => match read {
                    Ok(pkt) => { let _ = wg.send_ip_packet(&pkt).await; }
                    Err(e) => { debug!(%e, "overlay: TUN read ended; runtime exiting"); break; }
                },
                evt = events.recv() => match evt {
                    // Re-sync: install any newly-listed peers (deltas drive
                    // removals; a full diff/prune is a later refinement).
                    Some(OverlayEvent::Netmap { peers, .. }) => {
                        self.install_peers(&mut wg, &mut by_node, &mut relay, &peers).await;
                    }
                    Some(OverlayEvent::NetmapDelta { upserts, removes }) => {
                        self.install_peers(&mut wg, &mut by_node, &mut relay, &upserts).await;
                        for node_id in removes {
                            if let Some(pk) = by_node.remove(&node_id) {
                                wg.remove_peer(&pk);
                                info!(peer = %node_id, "overlay: peer removed");
                            }
                            if let Some(r) = relay.as_mut() {
                                r.forget(&node_id);
                            }
                        }
                    }
                    Some(OverlayEvent::RelayGrant { peer_node_id, ice_servers, pair_key }) => {
                        if let Some(r) = relay.as_mut()
                            && let Some(link) = r.on_grant(peer_node_id, ice_servers, pair_key).await
                        {
                            self.install_ready(&mut wg, &mut by_node, link);
                        }
                    }
                    None => break,
                },
            }
        }

        inbound.abort();
    }

    /// For each peer not already installed: in Direct mode build the
    /// carrier + install immediately; in Relay mode drive the coturn
    /// coordination (complete an in-flight allocation, else request creds).
    /// Dedup is by node id.
    async fn install_peers(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, [u8; 32]>,
        relay: &mut Option<RelayCoordinator>,
        peers: &[NetmapPeer],
    ) {
        for np in peers {
            if by_node.contains_key(&np.node_id) {
                continue; // already installed
            }
            let Some(cfg) = peer_config_from_netmap(np) else {
                continue;
            };
            match &self.mode {
                CarrierMode::Direct(links) => {
                    let Some(carrier) = links.build_carrier(&cfg).await else {
                        debug!(peer = %np.node_id, "overlay: no carrier built; retry next netmap");
                        continue;
                    };
                    self.install_ready(
                        wg,
                        by_node,
                        ReadyLink {
                            node_id: np.node_id,
                            public_key: cfg.public_key,
                            overlay_ip: cfg.overlay_ip,
                            carrier,
                        },
                    );
                }
                CarrierMode::Relay => {
                    if let Some(coord) = relay.as_mut() {
                        if let Some(link) = coord.maybe_complete(np.node_id, &cfg) {
                            self.install_ready(wg, by_node, link);
                        } else if !coord.is_tracking(&np.node_id) {
                            // Both ends pick the same coturn worker from the
                            // server's symmetric pair_key (in the grant), so no
                            // initiator/responder asymmetry is needed here — see
                            // relay_link.rs. The WG handshake still tie-breaks
                            // the dialer by pubkey in `install_ready`.
                            coord.request(np.node_id, cfg).await;
                        }
                    }
                }
            }
        }
    }

    /// Install a ready carrier as a WG peer + record it for later removal.
    fn install_ready(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, [u8; 32]>,
        link: ReadyLink,
    ) {
        // Deterministic single initiator per link: the lexicographically
        // smaller public key dials. Both ends compute this identically.
        let initiate = self.keypair.public.to_bytes() < link.public_key;
        wg.add_peer(link.public_key, link.overlay_ip, link.carrier, initiate);
        by_node.insert(link.node_id, link.public_key);
        info!(peer = %link.node_id, overlay_ip = %link.overlay_ip, initiate, "overlay: peer installed");
    }
}

#[cfg(test)]
mod tests {
    //! Phase 3b proof: two `OverlayRuntime`s, driven only by injected
    //! `rc:overlay.netmap` events + a loopback `LinkFactory`, bring up
    //! their WG peers and round-trip an IP packet between their mock
    //! TUNs — exercising join → netmap → add_peer → bridge end to end
    //! with no real device and no server.

    use super::*;
    use std::io;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::net::UdpSocket;
    use tokio::sync::Mutex;

    struct MockTun {
        inject: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
        delivered: mpsc::UnboundedSender<Vec<u8>>,
    }
    impl MockTun {
        fn new() -> (
            Arc<Self>,
            mpsc::UnboundedSender<Vec<u8>>,
            mpsc::UnboundedReceiver<Vec<u8>>,
        ) {
            let (i_tx, i_rx) = mpsc::unbounded_channel();
            let (d_tx, d_rx) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    inject: Mutex::new(i_rx),
                    delivered: d_tx,
                }),
                i_tx,
                d_rx,
            )
        }
    }
    #[async_trait]
    impl TunIo for MockTun {
        async fn read_packet(&self) -> io::Result<Vec<u8>> {
            self.inject
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| io::Error::other("mock inject closed"))
        }
        async fn write_packet(&self, packet: &[u8]) -> io::Result<()> {
            self.delivered
                .send(packet.to_vec())
                .map_err(|_| io::Error::other("mock delivered closed"))
        }
    }

    /// A factory that always hands back a fixed loopback carrier (one
    /// peer per node in the test).
    struct LoopbackLinks {
        sock: Arc<UdpSocket>,
        dst: SocketAddr,
    }
    #[async_trait]
    impl LinkFactory for LoopbackLinks {
        async fn build_carrier(&self, _peer: &PeerConfig) -> Option<Arc<Carrier>> {
            Some(Carrier::direct(self.sock.clone(), self.dst))
        }
    }

    fn synthetic_ipv4(src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
        let total = 20 + payload.len();
        let mut p = vec![0u8; total];
        p[0] = 0x45;
        p[2] = (total >> 8) as u8;
        p[3] = (total & 0xff) as u8;
        p[8] = 64;
        p[9] = 17;
        p[12..16].copy_from_slice(&src.octets());
        p[16..20].copy_from_slice(&dst.octets());
        p[20..].copy_from_slice(payload);
        p
    }

    fn net() -> OverlayNetworkInfo {
        OverlayNetworkInfo {
            cidr: "100.64.0.0/10".into(),
            mtu: 1280,
        }
    }
    fn peer(kp: &WgKeypair, ip: &str) -> NetmapPeer {
        NetmapPeer {
            node_id: ObjectId::new(),
            overlay_ip: ip.into(),
            wg_public_key: kp.public_base64(),
            endpoints: vec![],
            relay_home: None,
            reachable: true,
        }
    }

    const IP_A: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 1);
    const IP_B: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 2);

    #[tokio::test(flavor = "multi_thread")]
    async fn runtime_installs_peer_from_netmap_and_round_trips() {
        let a = WgKeypair::generate();
        let b = WgKeypair::generate();

        let sock_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let sock_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = sock_a.local_addr().unwrap();
        let addr_b = sock_b.local_addr().unwrap();

        let (out_a, mut out_a_rx) = mpsc::channel::<ClientMsg>(16);
        let (out_b, mut out_b_rx) = mpsc::channel::<ClientMsg>(16);
        let (evt_a, evt_a_rx) = mpsc::channel::<OverlayEvent>(16);
        let (evt_b, evt_b_rx) = mpsc::channel::<OverlayEvent>(16);

        let (mock_a, inject_a, _del_a) = MockTun::new();
        let (mock_b, _inj_b, mut del_b) = MockTun::new();
        let tf_a: TunFactory = {
            let m = mock_a.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };
        let tf_b: TunFactory = {
            let m = mock_b.clone();
            Box::new(move |_, _, _| Ok(m.clone() as Arc<dyn TunIo>))
        };

        let rt_a = OverlayRuntime::new(
            a.clone(),
            out_a,
            Arc::new(LoopbackLinks {
                sock: sock_a,
                dst: addr_b,
            }),
            tf_a,
            1280,
        );
        let rt_b = OverlayRuntime::new(
            b.clone(),
            out_b,
            Arc::new(LoopbackLinks {
                sock: sock_b,
                dst: addr_a,
            }),
            tf_b,
            1280,
        );
        tokio::spawn(rt_a.run(evt_a_rx, vec![]));
        tokio::spawn(rt_b.run(evt_b_rx, vec![]));

        // Both runtimes announce themselves first.
        assert!(matches!(
            out_a_rx.recv().await,
            Some(ClientMsg::OverlayJoin { .. })
        ));
        assert!(matches!(
            out_b_rx.recv().await,
            Some(ClientMsg::OverlayJoin { .. })
        ));

        // Server pushes each its netmap (the other node as the one peer).
        evt_a
            .send(OverlayEvent::Netmap {
                self_ip: "100.64.0.1".into(),
                network: net(),
                peers: vec![peer(&b, "100.64.0.2")],
            })
            .await
            .unwrap();
        evt_b
            .send(OverlayEvent::Netmap {
                self_ip: "100.64.0.2".into(),
                network: net(),
                peers: vec![peer(&a, "100.64.0.1")],
            })
            .await
            .unwrap();

        // App on A sends to B's overlay IP; assert it arrives on B's TUN.
        // Re-inject (best-effort send drops until the WG session is up).
        let pkt = synthetic_ipv4(IP_A, IP_B, b"runtime-loopback");
        for _ in 0..100 {
            let _ = inject_a.send(pkt.clone());
            if let Ok(Some(got)) =
                tokio::time::timeout(Duration::from_millis(150), del_b.recv()).await
            {
                assert_eq!(got, pkt, "packet must traverse the overlay runtime intact");
                return;
            }
        }
        panic!("packet did not traverse the runtime in time");
    }
}

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
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bson::oid::ObjectId;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::WgKeypair;
use super::direct;
use super::netmap::{PeerConfig, peer_config_from_netmap};
use super::relay_link::{ReadyLink, RelayCoordinator};
use super::tun::TunIo;
use super::wg::{Carrier, QUIC_BUILD_TIMEOUT, WG_OVERHEAD, WgDevice, overlay_quic_enabled};
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer, NetmapPeer, OverlayNetworkInfo};

/// rc.131/132 — direct LAN carrier context. A shared UDP socket peers dial,
/// this node's LAN IPs across ALL interfaces (for the same-subnet test), and
/// the `IP:port` endpoints we advertise (one per interface, all on the shared
/// socket's port) so a multi-homed peer can reach us on whichever subnet it
/// shares with us.
struct DirectCtx {
    sock: Arc<UdpSocket>,
    my_ips: Vec<Ipv4Addr>,
    endpoints: Vec<String>,
}

/// An installed peer carrier + the bookkeeping the direct→relay fallback
/// (rc.136/137) needs.
struct Installed {
    pubkey: [u8; 32],
    overlay_ip: Ipv4Addr,
    /// `true` if reached over the direct LAN socket, `false` over the relay.
    is_direct: bool,
    /// When this carrier was installed — for the warm-up grace period.
    since: Instant,
    /// Last `(tx, rx)` snapshot from the previous sweep (rc.137 lock-free
    /// health). Only meaningful for direct carriers.
    last_traffic: (u64, u64),
    /// Consecutive sweeps where we sent but received nothing (tx grew, rx
    /// flat). A few in a row ⇒ the direct carrier is one-way / dead.
    bad_sweeps: u32,
}

/// Grace after install before the fallback can fire — lets the bilateral
/// handshake + first packets flow before we judge the carrier.
const DIRECT_GRACE: Duration = Duration::from_secs(8);
/// Consecutive bad sweeps (sent, received nothing) before falling back. At the
/// 5 s tick that's ~15 s of one-way traffic — long enough to ignore a blip,
/// short enough that a VPN/AP-isolation break doesn't stay dark for long.
const BAD_SWEEPS_TO_FALLBACK: u32 = 3;
/// After a direct carrier fails, don't retry direct for this peer for this
/// long — it stays on relay, then re-attempts direct (auto-recovers when the
/// blocking condition clears, e.g. the VPN disconnects).
const DIRECT_COOLDOWN: Duration = Duration::from_secs(60);
/// rc.139 — a dead RELAY carrier (one-way, same `tx>rx` signal) is usually a
/// STALE coturn port: the peer re-allocated (restart/churn → new port) and we
/// kept dialing the old one. Refresh it (re-request → fresh allocation, re-dial
/// the peer's CURRENT address) — but not more than once per this window, so two
/// ends each refreshing don't ping-pong faster than they can converge.
const RELAY_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);
/// How often the carrier-health sweep runs. Cheap (lock-free atomic reads), so
/// a tighter cadence is fine and makes detection quicker.
const FALLBACK_TICK: Duration = Duration::from_secs(5);

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
        // rc.131 — direct LAN path: bind a shared UDP socket + discover our
        // LAN endpoint so a same-subnet peer dials us directly and skips the
        // relay. Off in Direct mode (the test/helper path) and when disabled.
        let direct_ctx = self.setup_direct().await;
        let mut advertised = endpoints;
        if let Some(ctx) = &direct_ctx {
            advertised.extend(ctx.endpoints.iter().cloned());
        }

        let join = ClientMsg::OverlayJoin {
            network_hint: None,
            wg_public_key: self.keypair.public_base64(),
            key_epoch: 0,
            supported: vec!["wireguard-v1".to_string()],
            mtu: self.mtu,
            endpoints: advertised,
            // rc.142 — advertise the QUIC-over-TURN capability so the server
            // only tells a peer to attempt QUIC when BOTH ends support it.
            supports_quic: overlay_quic_enabled(),
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

        // node_id → installed carrier (pubkey/IP/kind/install-time).
        let mut by_node: HashMap<ObjectId, Installed> = HashMap::new();
        // rc.136 — peers whose DIRECT carrier just failed: don't retry direct
        // until the Instant (they stay on relay). Auto-expires → direct retried.
        let mut direct_cooldown: HashMap<ObjectId, Instant> = HashMap::new();
        // rc.139 — peers whose stale relay was just refreshed (anti-ping-pong).
        let mut relay_refresh_cooldown: HashMap<ObjectId, Instant> = HashMap::new();
        // Latest netmap view (node_id → peer), so the fallback sweep can drive
        // the relay path for a downgraded peer without waiting for a netmap.
        let mut current_peers: HashMap<ObjectId, NetmapPeer> =
            first_peers.iter().map(|p| (p.node_id, p.clone())).collect();
        let mut fallback = tokio::time::interval(FALLBACK_TICK);
        let mut relay = match self.mode {
            // Pass our LAN endpoints so the relay-endpoint trickle re-includes
            // them (the server replaces, so they'd otherwise be clobbered —
            // rc.135). Empty when the direct path is off.
            CarrierMode::Relay => Some(RelayCoordinator::new(
                self.outbound.clone(),
                direct_ctx
                    .as_ref()
                    .map(|c| c.endpoints.clone())
                    .unwrap_or_default(),
            )),
            CarrierMode::Direct(_) => None,
        };
        self.install_peers(
            &mut wg,
            &mut by_node,
            &mut relay,
            &tun,
            &first_peers,
            direct_ctx.as_ref(),
            &direct_cooldown,
        )
        .await;

        // Phase 2 — steady state.
        loop {
            tokio::select! {
                read = tun.read_packet() => match read {
                    Ok(pkt) => { let _ = wg.send_ip_packet(&pkt).await; }
                    Err(e) => { debug!(%e, "overlay: TUN read ended; runtime exiting"); break; }
                },
                // rc.136 — direct→relay fallback sweep. A DIRECT carrier whose
                // handshake never completes (or dies mid-session) means the LAN
                // path only LOOKED viable (same subnet) but isn't actually
                // reachable — a corp full-tunnel VPN that hijacks routing, Wi-Fi
                // AP/client isolation, an asymmetric firewall. Tear it down and
                // switch the peer to relay (with a cooldown so the next netmap
                // doesn't immediately re-upgrade it to direct).
                _ = fallback.tick() => {
                    self.sweep_carrier_health(
                        &mut wg, &mut by_node, &mut relay, &tun,
                        &mut direct_cooldown, &mut relay_refresh_cooldown, &current_peers,
                    ).await;
                },
                evt = events.recv() => match evt {
                    // Re-sync: install any newly-listed peers (deltas drive
                    // removals; a full diff/prune is a later refinement).
                    Some(OverlayEvent::Netmap { peers, .. }) => {
                        current_peers = peers.iter().map(|p| (p.node_id, p.clone())).collect();
                        self.install_peers(&mut wg, &mut by_node, &mut relay, &tun, &peers, direct_ctx.as_ref(), &direct_cooldown).await;
                    }
                    Some(OverlayEvent::NetmapDelta { upserts, removes }) => {
                        for p in &upserts { current_peers.insert(p.node_id, p.clone()); }
                        self.install_peers(&mut wg, &mut by_node, &mut relay, &tun, &upserts, direct_ctx.as_ref(), &direct_cooldown).await;
                        for node_id in removes {
                            current_peers.remove(&node_id);
                            if let Some(e) = by_node.remove(&node_id) {
                                wg.remove_peer(&e.pubkey).await;
                                tun.del_peer_route(e.overlay_ip).await;
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
                            self.install_ready(&mut wg, &mut by_node, &tun, link).await;
                        }
                    }
                    None => break,
                },
            }
        }

        inbound.abort();
    }

    /// rc.137/139 — find carriers that are one-way / dead and repair them.
    /// Health is LOCK-FREE: each sweep snapshots `(tx, rx)` (atomic reads — no
    /// `Tunn` lock, so it can't stall the packet path like the rc.136
    /// handshake-age check did); a carrier where **tx climbed but rx stayed
    /// flat** for [`BAD_SWEEPS_TO_FALLBACK`] consecutive sweeps is dead (we're
    /// sending, nothing comes back). The repair depends on the carrier kind:
    /// - **direct** → fall back to relay (the LAN path only LOOKED viable —
    ///   corp VPN route hijack, Wi-Fi AP/client isolation, asymmetric firewall);
    ///   [`DIRECT_COOLDOWN`] keeps the next netmap from re-upgrading it.
    /// - **relay** (rc.139) → refresh it: the peer almost certainly
    ///   re-allocated its coturn port (restart/churn) and we're dialing a stale
    ///   one. Re-request so we re-allocate + re-dial the peer's CURRENT address
    ///   ([`RELAY_REFRESH_COOLDOWN`] bounds two ends ping-ponging).
    #[allow(clippy::too_many_arguments)]
    async fn sweep_carrier_health(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        relay: &mut Option<RelayCoordinator>,
        tun: &Arc<dyn TunIo>,
        direct_cooldown: &mut HashMap<ObjectId, Instant>,
        relay_refresh_cooldown: &mut HashMap<ObjectId, Instant>,
        current_peers: &HashMap<ObjectId, NetmapPeer>,
    ) {
        let now = Instant::now();
        // (node_id, was_direct)
        let mut dead: Vec<(ObjectId, bool)> = Vec::new();
        for (nid, e) in by_node.iter_mut() {
            let Some((tx, rx)) = wg.peer_traffic(&e.pubkey) else {
                continue;
            };
            let (last_tx, last_rx) = e.last_traffic;
            e.last_traffic = (tx, rx);
            // Warm-up grace: let the handshake + first packets flow.
            if e.since.elapsed() < DIRECT_GRACE {
                continue;
            }
            // Sent this interval but received nothing back ⇒ suspect. (If we
            // didn't send either, the link is just idle — no judgment.)
            if tx > last_tx && rx == last_rx {
                e.bad_sweeps += 1;
            } else {
                e.bad_sweeps = 0;
            }
            if e.bad_sweeps >= BAD_SWEEPS_TO_FALLBACK {
                // For a relay, hold off if we just refreshed it (anti-ping-pong).
                if !e.is_direct
                    && relay_refresh_cooldown
                        .get(nid)
                        .is_some_and(|&until| until > now)
                {
                    continue;
                }
                dead.push((*nid, e.is_direct));
            }
        }
        for (nid, was_direct) in dead {
            let Some(e) = by_node.remove(&nid) else {
                continue;
            };
            wg.remove_peer(&e.pubkey).await;
            tun.del_peer_route(e.overlay_ip).await;
            if was_direct {
                direct_cooldown.insert(nid, now + DIRECT_COOLDOWN);
                warn!(
                    peer = %nid,
                    "overlay: direct LAN carrier didn't establish (VPN / AP-isolation / firewall?) — falling back to relay"
                );
            } else {
                relay_refresh_cooldown.insert(nid, now + RELAY_REFRESH_COOLDOWN);
                warn!(
                    peer = %nid,
                    "overlay: relay carrier one-way (stale coturn port?) — re-allocating"
                );
            }
            // (Re)request the relay now (don't wait for the next netmap). For a
            // refresh we first forget the stale allocation so a fresh one is made.
            if let (Some(coord), Some(np)) = (relay.as_mut(), current_peers.get(&nid))
                && let Some(cfg) = peer_config_from_netmap(np)
            {
                if !was_direct {
                    coord.forget(&nid);
                }
                coord.request(nid, cfg).await;
            }
        }
    }

    /// rc.131 — bind the shared direct-carrier socket + discover our LAN
    /// endpoint. Only in Relay mode (Direct mode is the loopback test/helper
    /// path) and when `ROOMLER_AGENT_OVERLAY_DIRECT` isn't disabled. `None` if
    /// disabled, not relay mode, the bind fails, or there's no usable LAN IP
    /// (offline / CGNAT-only) — the node then stays relay-only as before.
    async fn setup_direct(&self) -> Option<DirectCtx> {
        if !matches!(self.mode, CarrierMode::Relay) || !direct::direct_enabled() {
            return None;
        }
        let my_ips = direct::gather_lan_ips();
        if my_ips.is_empty() {
            info!("overlay: no usable LAN interface; direct path off (relay only)");
            return None;
        }
        let sock = Arc::new(UdpSocket::bind("0.0.0.0:0").await.ok()?);
        let port = sock.local_addr().ok()?.port();
        // One endpoint per interface IP — all reach the single 0.0.0.0:port
        // socket; the peer dials whichever shares its subnet.
        let endpoints: Vec<String> = my_ips.iter().map(|ip| format!("{ip}:{port}")).collect();
        info!(
            endpoints = ?endpoints,
            "overlay: advertising direct LAN endpoints (same-subnet peers dial direct)"
        );
        Some(DirectCtx {
            sock,
            my_ips,
            endpoints,
        })
    }

    /// Reconcile the netmap into installed peers. NOT-yet-installed: Direct
    /// mode → build the loopback/test carrier; Relay mode → a DIRECT LAN
    /// carrier when the peer advertises a same-subnet endpoint (rc.131/134 — N
    /// peers share one socket via the device's source-address demux), else the
    /// coturn relay coordination. ALREADY-installed on RELAY but a same-subnet
    /// endpoint has since appeared → UPGRADE to direct (rc.134 re-evaluation:
    /// a peer first seen before its endpoint arrived would otherwise stay on
    /// relay forever). A peer in a [`DIRECT_COOLDOWN`] (its direct carrier just
    /// failed — rc.136) is kept on relay regardless of a same-subnet endpoint.
    #[allow(clippy::too_many_arguments)]
    async fn install_peers(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        relay: &mut Option<RelayCoordinator>,
        tun: &Arc<dyn TunIo>,
        peers: &[NetmapPeer],
        direct_ctx: Option<&DirectCtx>,
        direct_cooldown: &HashMap<ObjectId, Instant>,
    ) {
        for np in peers {
            let Some(cfg) = peer_config_from_netmap(np) else {
                continue;
            };
            // rc.136 — suppress direct while this peer is cooling down from a
            // failed direct carrier (treat as if it had no same-subnet endpoint
            // → relay). Expired entries fall through, so direct is retried.
            let in_cooldown = direct_cooldown
                .get(&np.node_id)
                .is_some_and(|&until| until > Instant::now());
            // A same-subnet direct endpoint for this peer, if any (Relay mode +
            // direct enabled + not cooling down + the peer advertised one on
            // one of our subnets).
            let direct_dst = if in_cooldown {
                None
            } else {
                direct_ctx
                    .and_then(|ctx| direct::pick_same_subnet_endpoint(&ctx.my_ips, &cfg.endpoints))
            };

            match by_node.get(&np.node_id).map(|e| (e.is_direct, e.pubkey)) {
                Some((true, _)) => continue, // already direct
                Some((false, pk)) => {
                    // Installed on RELAY — upgrade to direct now that a
                    // same-subnet endpoint has appeared (re-evaluation).
                    if let (Some(ctx), Some(dst)) = (direct_ctx, direct_dst) {
                        info!(peer = %np.node_id, %dst, "overlay: upgrading relay peer to direct LAN carrier");
                        wg.remove_peer(&pk).await;
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        self.install_direct(wg, by_node, tun, ctx, np.node_id, &cfg, dst)
                            .await;
                    }
                    continue;
                }
                None => {}
            }

            match &self.mode {
                CarrierMode::Direct(links) => {
                    let Some(carrier) = links.build_carrier(&cfg).await else {
                        debug!(peer = %np.node_id, "overlay: no carrier built; retry next netmap");
                        continue;
                    };
                    self.install_ready(
                        wg,
                        by_node,
                        tun,
                        ReadyLink {
                            node_id: np.node_id,
                            public_key: cfg.public_key,
                            overlay_ip: cfg.overlay_ip,
                            carrier,
                            relay_parts: None,
                            supports_quic: cfg.supports_quic,
                        },
                    )
                    .await;
                }
                CarrierMode::Relay => {
                    if let (Some(ctx), Some(dst)) = (direct_ctx, direct_dst) {
                        // Same-subnet → direct, skip the relay. Forget any
                        // pending relay request so a late grant can't later
                        // clobber the direct carrier.
                        if let Some(r) = relay.as_mut() {
                            r.forget(&np.node_id);
                        }
                        self.install_direct(wg, by_node, tun, ctx, np.node_id, &cfg, dst)
                            .await;
                    } else if let Some(coord) = relay.as_mut() {
                        if let Some(link) = coord.maybe_complete(np.node_id, &cfg) {
                            self.install_ready(wg, by_node, tun, link).await;
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

    /// rc.134 — install a peer over the SHARED direct-LAN socket (demuxed by
    /// source address, so any number of same-subnet peers coexist — no more
    /// "one direct peer" cap). Both ends initiate (bilateral hole-punch,
    /// rc.133). Adds the `/32` route + records it as `direct` in `by_node`.
    #[allow(clippy::too_many_arguments)]
    async fn install_direct(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        ctx: &DirectCtx,
        node_id: ObjectId,
        cfg: &PeerConfig,
        dst: std::net::SocketAddr,
    ) {
        wg.ensure_direct_demux(ctx.sock.clone());
        wg.add_direct_peer(cfg.public_key, cfg.overlay_ip, dst, true)
            .await;
        by_node.insert(
            node_id,
            Installed {
                pubkey: cfg.public_key,
                overlay_ip: cfg.overlay_ip,
                is_direct: true,
                since: Instant::now(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
            },
        );
        if let Err(e) = tun.add_peer_route(cfg.overlay_ip).await {
            debug!(peer = %node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
        info!(peer = %node_id, overlay_ip = %cfg.overlay_ip, %dst, "overlay: direct LAN carrier (same subnet) — skipping relay");
    }

    /// Install a ready carrier as a WG peer, add its `/32` route, and record
    /// it (pubkey + IP) for later removal.
    async fn install_ready(
        &self,
        wg: &mut WgDevice,
        by_node: &mut HashMap<ObjectId, Installed>,
        tun: &Arc<dyn TunIo>,
        link: ReadyLink,
    ) {
        // Handshake direction. RELAY carriers use a deterministic single
        // initiator (the lexicographically smaller pubkey dials; both ends
        // compute it identically) — fine because the relay forwards both ways.
        //
        // rc.133 — DIRECT carriers need BOTH ends to initiate (bilateral
        // hole-punch). A direct WG init is an UNSOLICITED inbound UDP on the
        // responder's PHYSICAL interface, which default Windows Firewall drops
        // (field: two same-LAN hosts, direct carrier built but
        // HANDSHAKE(REKEY_TIMEOUT) forever). When both ends initiate, each
        // side's outbound init opens a stateful firewall hole for the other's
        // inbound, so the handshake completes. The relay path never hit this
        // because its ciphertext rides the agent's OWN outbound TURN
        // connection (already a stateful hole).
        // Optional QUIC-over-TURN upgrade of a relay carrier (opt-in, default
        // OFF via `overlay_quic_enabled`). QUIC's congestion control smooths the
        // relay's buffer-bloat latency spikes and its keepalive holds the TURN
        // permission fresh. On ANY handshake failure/timeout we fall back to the
        // already-built raw relay carrier, so the upgrade can only improve —
        // never break — the link.
        let carrier = if overlay_quic_enabled() && link.supports_quic && link.relay_parts.is_some()
        {
            let (conn, dst) = link.relay_parts.clone().unwrap();
            // Deterministic role: the lexicographically-smaller pubkey serves
            // (same rule as the WG relay initiator, so both ends agree on who
            // dials vs accepts).
            let am_server = self.keypair.public.to_bytes() < link.public_key;
            match Carrier::quic_relay(
                conn,
                dst,
                am_server,
                self.mtu as usize + WG_OVERHEAD,
                QUIC_BUILD_TIMEOUT,
            )
            .await
            {
                Ok(q) => {
                    info!(peer = %link.node_id, %dst, "overlay: QUIC-over-TURN carrier up");
                    q
                }
                Err(e) => {
                    warn!(peer = %link.node_id, %e, "overlay: QUIC carrier build failed; using raw relay");
                    link.carrier
                }
            }
        } else {
            link.carrier
        };

        let initiate = carrier.is_direct() || self.keypair.public.to_bytes() < link.public_key;
        let is_direct = carrier.is_direct();
        wg.add_peer(link.public_key, link.overlay_ip, carrier, initiate);
        by_node.insert(
            link.node_id,
            Installed {
                pubkey: link.public_key,
                overlay_ip: link.overlay_ip,
                is_direct,
                since: Instant::now(),
                last_traffic: (0, 0),
                bad_sweeps: 0,
            },
        );
        // Host `/32` so overlay traffic to this peer beats any colliding
        // less-specific route on the uplink (e.g. a carrier CGNAT /10).
        // Best-effort — clean hosts route fine via the connected /10.
        if let Err(e) = tun.add_peer_route(link.overlay_ip).await {
            debug!(peer = %link.node_id, %e, "overlay: /32 peer route not installed (ok on clean hosts)");
        }
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
            supports_quic: false,
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

//! Userspace WireGuard device (boringtun) bridged to a pluggable
//! carrier.
//!
//! boringtun's [`Tunn`] is a pure packet transform — it owns neither a
//! socket nor a TUN. This module wires one `Tunn` per peer to a
//! [`Carrier`] (a direct UDP socket, or a coturn TURN allocation — the
//! DERP-equivalent), so the same WG ciphertext rides whichever path the
//! NAT-traversal tier selected, unchanged. Decrypted inbound IP packets
//! are delivered on an mpsc channel that the TUN bridge (Phase 3) — or
//! the Phase-2 tests — drains.
//!
//! Per-peer wiring:
//! * a **recv task** loops `carrier.recv()` → `Tunn::decapsulate` →
//!   either echoes handshake/cookie bytes back over the carrier or
//!   pushes a decrypted IP packet to `tun_tx`;
//! * a **timer task** ticks `Tunn::update_timers` (handshake retries +
//!   keepalives);
//! * `send_to_peer` / `send_ip_packet` `Tunn::encapsulate` an outbound
//!   IP packet and write the ciphertext over the carrier.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::router::Router;
use crate::transport::relay::RelayConn;

/// Scratch buffer size for encap/decap + carrier I/O. The overlay MTU is
/// 1280; a WG datagram adds ~32 B of overhead, so 2048 is comfortable
/// headroom and equals the relay's `MAX_DATAGRAM` cap (no fragmentation
/// at this size).
const WG_BUF: usize = 2048;

/// WireGuard persistent-keepalive interval. Keeps a relayed/NAT mapping
/// warm so a mostly-idle overlay link stays reachable.
const KEEPALIVE_SECS: u16 = 25;

/// How often the timer task drives `update_timers` (handshake retries +
/// keepalive scheduling). WG's own timers are second-granular; 250 ms
/// keeps handshake setup snappy without busy-looping.
const TIMER_TICK_MS: u64 = 250;

/// How a peer's WG datagrams reach it. Both arms are "send bytes to a
/// dst / recv bytes"; boringtun output rides either unchanged.
pub enum Carrier {
    /// Tier 1: direct UDP to the peer's (possibly hole-punched) endpoint.
    Direct {
        sock: Arc<UdpSocket>,
        dst: SocketAddr,
    },
    /// Tier 2/3: through a coturn TURN allocation (`RelayConn`), dialing
    /// the peer's relayed address. The DERP-equivalent carrier.
    Relay {
        conn: Arc<dyn RelayConn>,
        dst: SocketAddr,
    },
}

impl Carrier {
    pub fn direct(sock: Arc<UdpSocket>, dst: SocketAddr) -> Arc<Self> {
        Arc::new(Carrier::Direct { sock, dst })
    }

    pub fn relay(conn: Arc<dyn RelayConn>, dst: SocketAddr) -> Arc<Self> {
        Arc::new(Carrier::Relay { conn, dst })
    }

    /// A direct UDP carrier (vs a coturn relay). The runtime uses this to
    /// decide handshake direction: a direct carrier needs BOTH ends to
    /// initiate (bilateral hole-punch — see `install_ready`).
    pub fn is_direct(&self) -> bool {
        matches!(self, Carrier::Direct { .. })
    }

    async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Carrier::Direct { sock, dst } => sock.send_to(buf, *dst).await,
            Carrier::Relay { conn, dst } => conn.send_to(buf, *dst).await,
        }
    }

    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Carrier::Direct { sock, .. } => Ok(sock.recv_from(buf).await?.0),
            Carrier::Relay { conn, .. } => Ok(conn.recv_from(buf).await?.0),
        }
    }
}

/// One installed peer: its `Tunn`, its carrier, and the background tasks
/// that pump it. Dropping aborts the tasks.
struct Peer {
    tunn: Arc<Mutex<Tunn>>,
    carrier: Arc<Carrier>,
    overlay_ip: Ipv4Addr,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// A node's userspace WireGuard device: one static keypair, N peers,
/// and the `overlay_ip → pubkey` routing table.
pub struct WgDevice {
    secret: StaticSecret,
    public: PublicKey,
    peers: HashMap<[u8; 32], Peer>,
    router: Router,
    tun_tx: mpsc::Sender<Vec<u8>>,
    next_index: u32,
}

impl WgDevice {
    /// Build a device from a static secret. Returns the device plus the
    /// receiver for decrypted inbound IP packets (the TUN bridge / tests
    /// drain it).
    pub fn new(secret: StaticSecret) -> (Self, mpsc::Receiver<Vec<u8>>) {
        let public = PublicKey::from(&secret);
        let (tun_tx, tun_rx) = mpsc::channel(256);
        (
            Self {
                secret,
                public,
                peers: HashMap::new(),
                router: Router::new(),
                tun_tx,
                next_index: 1,
            },
            tun_rx,
        )
    }

    /// This node's public key.
    pub fn public(&self) -> PublicKey {
        self.public
    }

    /// Install a peer and spawn its recv + timer tasks. `initiate` ⇒
    /// proactively send a handshake initiation now (the dialing side);
    /// the other end reacts to the inbound init.
    pub fn add_peer(
        &mut self,
        peer_public: [u8; 32],
        overlay_ip: Ipv4Addr,
        carrier: Arc<Carrier>,
        initiate: bool,
    ) {
        let index = self.next_index;
        self.next_index = self.next_index.wrapping_add(1);

        let tunn = Tunn::new(
            self.secret.clone(),
            PublicKey::from(peer_public),
            None,
            Some(KEEPALIVE_SECS),
            index,
            None,
        );
        let tunn = Arc::new(Mutex::new(tunn));

        // recv task: carrier → decapsulate → tun / network echo.
        let recv_tunn = tunn.clone();
        let recv_carrier = carrier.clone();
        let tun_tx = self.tun_tx.clone();
        let recv_task = tokio::spawn(async move {
            let mut buf = vec![0u8; WG_BUF];
            loop {
                let n = match recv_carrier.recv(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        debug!(%e, "wg carrier recv ended; peer recv task exiting");
                        break;
                    }
                };
                let mut t = recv_tunn.lock().await;
                process_inbound(&mut t, n, &mut buf, &recv_carrier, &tun_tx).await;
            }
        });

        // timer task: drive handshake retries + keepalives.
        let timer_tunn = tunn.clone();
        let timer_carrier = carrier.clone();
        let timer_task = tokio::spawn(async move {
            let mut buf = vec![0u8; WG_BUF];
            let mut tick = tokio::time::interval(Duration::from_millis(TIMER_TICK_MS));
            loop {
                tick.tick().await;
                let mut t = timer_tunn.lock().await;
                if let TunnResult::WriteToNetwork(b) = t.update_timers(&mut buf) {
                    let _ = timer_carrier.send(b).await;
                }
            }
        });

        self.router.upsert(overlay_ip, peer_public);
        self.peers.insert(
            peer_public,
            Peer {
                tunn: tunn.clone(),
                carrier: carrier.clone(),
                overlay_ip,
                tasks: vec![recv_task, timer_task],
            },
        );

        if initiate {
            tokio::spawn(async move {
                let mut buf = vec![0u8; WG_BUF];
                let mut t = tunn.lock().await;
                if let TunnResult::WriteToNetwork(b) =
                    t.format_handshake_initiation(&mut buf, false)
                {
                    let _ = carrier.send(b).await;
                }
            });
        }
    }

    /// Remove a peer (drops its `Tunn` + aborts its tasks + clears its
    /// route). Used when the netmap drops a peer (ACL change / leave).
    pub fn remove_peer(&mut self, peer_public: &[u8; 32]) {
        if let Some(p) = self.peers.remove(peer_public) {
            self.router.remove(&p.overlay_ip);
        }
    }

    /// Number of installed peers.
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Encapsulate + send a raw IPv4 packet to whichever peer owns its
    /// destination overlay address. `false` if no route or no session.
    pub async fn send_ip_packet(&self, packet: &[u8]) -> bool {
        let Some(dst) = Router::dst_of_ipv4_packet(packet) else {
            return false;
        };
        let Some(pubkey) = self.router.route(&dst) else {
            return false;
        };
        self.send_to_peer(&pubkey, packet).await
    }

    /// Encapsulate + send `packet` to a specific peer. `false` if the
    /// peer is unknown, the handshake hasn't completed (boringtun
    /// returns `Done`, queuing nothing — the caller retries), or the
    /// carrier send failed.
    pub async fn send_to_peer(&self, peer_public: &[u8; 32], packet: &[u8]) -> bool {
        let Some(peer) = self.peers.get(peer_public) else {
            return false;
        };
        let mut buf = vec![0u8; WG_BUF];
        let mut t = peer.tunn.lock().await;
        match t.encapsulate(packet, &mut buf) {
            TunnResult::WriteToNetwork(b) => peer.carrier.send(b).await.is_ok(),
            TunnResult::Done => false,
            TunnResult::Err(e) => {
                warn!(?e, "wg encapsulate error");
                false
            }
            _ => false,
        }
    }

    /// Whether a WG session to `peer_public` has completed a handshake.
    /// Tests poll this to know when data will flow.
    pub async fn is_connected(&self, peer_public: &[u8; 32]) -> bool {
        let Some(peer) = self.peers.get(peer_public) else {
            return false;
        };
        peer.tunn.lock().await.time_since_last_handshake().is_some()
    }
}

/// Handle one inbound carrier datagram: decapsulate, echo any
/// handshake/cookie/queued bytes back over the carrier, and deliver a
/// decrypted IP packet to the TUN channel.
async fn process_inbound(
    t: &mut Tunn,
    n: usize,
    buf: &mut [u8],
    carrier: &Carrier,
    tun_tx: &mpsc::Sender<Vec<u8>>,
) {
    // Decapsulate writes into a separate scratch buffer so the borrow on
    // the result doesn't alias the inbound `buf`.
    let mut out = vec![0u8; WG_BUF];
    match t.decapsulate(None, &buf[..n], &mut out) {
        TunnResult::WriteToNetwork(b) => {
            let _ = carrier.send(b).await;
            // A handshake step can complete a session with queued data;
            // boringtun signals more to flush by returning WriteToNetwork
            // on empty-datagram decapsulate calls. Drain until Done.
            loop {
                let mut flush = vec![0u8; WG_BUF];
                match t.decapsulate(None, &[], &mut flush) {
                    TunnResult::WriteToNetwork(b2) => {
                        let _ = carrier.send(b2).await;
                    }
                    _ => break,
                }
            }
        }
        TunnResult::WriteToTunnelV4(pkt, _) => {
            let _ = tun_tx.send(pkt.to_vec()).await;
        }
        TunnResult::WriteToTunnelV6(pkt, _) => {
            let _ = tun_tx.send(pkt.to_vec()).await;
        }
        TunnResult::Done => {}
        TunnResult::Err(e) => debug!(?e, "wg decapsulate error"),
    }
}

#[cfg(test)]
mod tests {
    //! Phase 2 proof: two userspace WG devices complete a handshake and
    //! round-trip a synthetic IP packet over **both** carriers — a
    //! direct UDP socket and the [`RelayConn`] bridge — terminating at
    //! the device's `tun_rx` (no real TUN). Mirrors the structure of the
    //! QUIC two-allocation tests in [`crate::transport::relay`].

    use super::*;
    use crate::overlay::WgKeypair;
    use std::time::Duration;

    /// Minimal well-formed IPv4 packet so boringtun routes it to
    /// `WriteToTunnelV4` (correct version nibble + total-length field +
    /// dst at bytes 16..20).
    fn synthetic_ipv4(src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
        let total = 20 + payload.len();
        let mut p = vec![0u8; total];
        p[0] = 0x45; // IPv4, IHL=5
        p[2] = (total >> 8) as u8;
        p[3] = (total & 0xff) as u8;
        p[8] = 64; // TTL
        p[9] = 17; // UDP
        p[12..16].copy_from_slice(&src.octets());
        p[16..20].copy_from_slice(&dst.octets());
        p[20..].copy_from_slice(payload);
        p
    }

    /// Poll `is_connected` until the handshake completes or we give up.
    /// Generous budget so heavy parallel CI load (these are
    /// `multi_thread` tests sharing cores) can't starve the handshake
    /// tasks into a false failure.
    async fn wait_connected(dev: &WgDevice, peer: &[u8; 32]) {
        for _ in 0..300 {
            if dev.is_connected(peer).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("WG handshake did not complete in time");
    }

    /// Encapsulate-send with retry (the first packet right after the
    /// handshake occasionally races the session install).
    async fn send_until_ok(dev: &WgDevice, peer: &[u8; 32], pkt: &[u8]) {
        for _ in 0..100 {
            if dev.send_to_peer(peer, pkt).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("WG encapsulate never produced a network packet");
    }

    const IP_A: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 1);
    const IP_B: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 2);

    #[tokio::test(flavor = "multi_thread")]
    async fn wg_handshake_and_data_over_direct_udp() {
        let a = WgKeypair::generate();
        let b = WgKeypair::generate();

        let sock_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let sock_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = sock_a.local_addr().unwrap();
        let addr_b = sock_b.local_addr().unwrap();

        let (mut dev_a, _rx_a) = WgDevice::new(a.secret.clone());
        let (mut dev_b, mut rx_b) = WgDevice::new(b.secret.clone());

        dev_a.add_peer(
            b.public.to_bytes(),
            IP_B,
            Carrier::direct(sock_a.clone(), addr_b),
            true,
        );
        dev_b.add_peer(
            a.public.to_bytes(),
            IP_A,
            Carrier::direct(sock_b.clone(), addr_a),
            false,
        );

        wait_connected(&dev_a, &b.public.to_bytes()).await;

        let pkt = synthetic_ipv4(IP_A, IP_B, b"hello-over-direct-wg");
        send_until_ok(&dev_a, &b.public.to_bytes(), &pkt).await;

        let got = tokio::time::timeout(Duration::from_secs(15), rx_b.recv())
            .await
            .expect("B did not receive a decrypted packet in time")
            .expect("tun channel closed");
        assert_eq!(got, pkt, "decrypted IP packet must arrive intact");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wg_handshake_and_data_over_relay_conn() {
        use crate::transport::relay::UdpRelayConn;

        let a = WgKeypair::generate();
        let b = WgKeypair::generate();

        // Drive the carrier through the `RelayConn` trait (the same trait
        // a coturn `TurnRelayConn` implements) to prove boringtun rides
        // it directly — no `RelayUdpSocket` quinn wrapper.
        let sock_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sock_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr_a = sock_a.local_addr().unwrap();
        let addr_b = sock_b.local_addr().unwrap();
        let conn_a: Arc<dyn RelayConn> = Arc::new(UdpRelayConn(sock_a));
        let conn_b: Arc<dyn RelayConn> = Arc::new(UdpRelayConn(sock_b));

        let (mut dev_a, _rx_a) = WgDevice::new(a.secret.clone());
        let (mut dev_b, mut rx_b) = WgDevice::new(b.secret.clone());

        dev_a.add_peer(
            b.public.to_bytes(),
            IP_B,
            Carrier::relay(conn_a, addr_b),
            true,
        );
        dev_b.add_peer(
            a.public.to_bytes(),
            IP_A,
            Carrier::relay(conn_b, addr_a),
            false,
        );

        wait_connected(&dev_a, &b.public.to_bytes()).await;

        let pkt = synthetic_ipv4(IP_A, IP_B, b"hello-over-relay-wg");
        send_until_ok(&dev_a, &b.public.to_bytes(), &pkt).await;

        let got = tokio::time::timeout(Duration::from_secs(15), rx_b.recv())
            .await
            .expect("B did not receive a decrypted packet in time")
            .expect("tun channel closed");
        assert_eq!(
            got, pkt,
            "decrypted IP packet must arrive over the relay carrier"
        );
    }

    // ───────────── LIVE coturn smoke (two real TURN allocations) ─────────────
    //
    // Mirrors `relay::turn_tests::relay_against_real_coturn_udp`: two WG
    // devices, each riding its own coturn allocation, complete a
    // handshake + round-trip a packet over the live cluster. `#[ignore]`
    // + env-gated. Provide:
    //   ROOMLER_TEST_TURN_HOST   = coturn.roomler.ai
    //   ROOMLER_TEST_TURN_SECRET = coturn's static-auth-secret
    // and run on a host with outbound UDP/3478 (or TCP/443 for the TURNS
    // variant) to coturn:
    //   cargo test -p roomler-ai-tunnel-core --features overlay --ignored wg_against_real_coturn

    fn live_coturn_creds(
        url_fmt: impl Fn(&str) -> Vec<String>,
    ) -> Option<(Vec<String>, String, String)> {
        let host = std::env::var("ROOMLER_TEST_TURN_HOST").ok()?;
        let secret = std::env::var("ROOMLER_TEST_TURN_SECRET").ok()?;
        let cfg = roomler_ai_remote_control::turn_creds::TurnConfig {
            urls: url_fmt(&host),
            shared_secret: secret,
            ttl_secs: 600,
        };
        let ice = cfg.issue("wg-coturn-smoke");
        Some((ice.urls, ice.username?, ice.credential?))
    }

    async fn wg_over_two_live_allocations(urls: &[String], user: &str, cred: &str) {
        use crate::transport::relay::allocate_relay_from_ice;

        let a = WgKeypair::generate();
        let b = WgKeypair::generate();

        let relay_a = allocate_relay_from_ice(urls, user, cred)
            .await
            .expect("device A relay allocate (live coturn)");
        let relay_b = allocate_relay_from_ice(urls, user, cred)
            .await
            .expect("device B relay allocate (live coturn)");
        let r_a = relay_a.local_addr().unwrap();
        let r_b = relay_b.local_addr().unwrap();
        assert_ne!(r_a, r_b, "two allocations get distinct relay addrs");

        // Mutual TURN-permission bootstrap (one stray datagram each way).
        relay_a.send_to(b"\x00", r_b).await.unwrap();
        relay_b.send_to(b"\x00", r_a).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (mut dev_a, _rx_a) = WgDevice::new(a.secret.clone());
        let (mut dev_b, mut rx_b) = WgDevice::new(b.secret.clone());
        dev_a.add_peer(
            b.public.to_bytes(),
            IP_B,
            Carrier::relay(Arc::new(relay_a), r_b),
            true,
        );
        dev_b.add_peer(
            a.public.to_bytes(),
            IP_A,
            Carrier::relay(Arc::new(relay_b), r_a),
            false,
        );

        wait_connected(&dev_a, &b.public.to_bytes()).await;
        let pkt = synthetic_ipv4(IP_A, IP_B, b"hello-over-coturn-wg");
        send_until_ok(&dev_a, &b.public.to_bytes(), &pkt).await;

        let got = tokio::time::timeout(Duration::from_secs(30), rx_b.recv())
            .await
            .expect("B did not receive a decrypted packet over coturn in time")
            .expect("tun channel closed");
        assert_eq!(got, pkt, "WG packet must round-trip over live coturn");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "hits live coturn; set ROOMLER_TEST_TURN_HOST + ROOMLER_TEST_TURN_SECRET"]
    async fn wg_against_real_coturn_udp() {
        let Some((urls, user, cred)) =
            live_coturn_creds(|h| vec![format!("turn:{h}:3478?transport=udp")])
        else {
            eprintln!("SKIP wg_against_real_coturn_udp: env unset");
            return;
        };
        wg_over_two_live_allocations(&urls, &user, &cred).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "hits live coturn over TLS/TCP; set ROOMLER_TEST_TURN_HOST + ROOMLER_TEST_TURN_SECRET"]
    async fn wg_against_real_coturn_turns_tcp() {
        let Some((urls, user, cred)) =
            live_coturn_creds(|h| vec![format!("turns:{h}:443?transport=tcp")])
        else {
            eprintln!("SKIP wg_against_real_coturn_turns_tcp: env unset");
            return;
        };
        wg_over_two_live_allocations(&urls, &user, &cred).await;
    }
}

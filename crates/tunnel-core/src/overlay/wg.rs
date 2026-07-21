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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use boringtun::noise::{Packet, Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::router::Router;
use crate::transport::quic::QuicPeer;
use crate::transport::relay::{RelayConn, RelayUdpSocket};

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

/// Bytes a WG data message adds over the inner IP packet (16 B header +
/// 16 B Poly1305 tag). The QUIC datagram carrier's budget must fit
/// `overlay_mtu + WG_OVERHEAD`.
pub const WG_OVERHEAD: usize = 32;

/// How long to wait for the QUIC-over-TURN handshake before falling back to
/// the raw relay carrier. This is the rendezvous window: the QUIC server's
/// `accept()` and the client's `connect()` must overlap, but the two ends
/// install peers at slightly different times (sequential per-peer install +
/// staggered restarts), so a too-short window makes one side time out → raw
/// while the other lands on QUIC (a silent split). 8 s tolerates that skew;
/// residual skew self-heals via the health sweep (both sides see rx flat on a
/// split and re-request). Bounded so a dead relay still falls back promptly.
pub const QUIC_BUILD_TIMEOUT: Duration = Duration::from_secs(8);

/// Opt-in gate for the QUIC-over-TURN carrier (`ROOMLER_NODE_OVERLAY_QUIC`;
/// legacy `ROOMLER_AGENT_OVERLAY_QUIC` still honoured — see
/// [`crate::env::node_env`]). **Default OFF** — the raw relay is the proven
/// path; QUIC is enabled per-host only after field-proving (mirrors the
/// direct-path arc). Truthy = `1`/`true`/`yes`/`on` (case-insensitive);
/// anything else (incl. unset) is off.
pub fn overlay_quic_enabled() -> bool {
    match crate::env::node_env("OVERLAY_QUIC") {
        Some(v) => {
            let t = v.trim();
            t.eq_ignore_ascii_case("1")
                || t.eq_ignore_ascii_case("true")
                || t.eq_ignore_ascii_case("yes")
                || t.eq_ignore_ascii_case("on")
        }
        None => false,
    }
}

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
        /// rc.181 — latched when `send` hard-errors (a TURNS/TCP reset: the
        /// `tcp-turn write: connection reset` the next send returns after a
        /// corp middlebox reaps the idle control TCP). The runtime's health
        /// sweep reads it via [`WgDevice::peer_carrier_dead`] and re-allocates
        /// on the next tick, instead of waiting out the ~20 s rx-flat heuristic.
        dead: AtomicBool,
    },
    /// Tier 2/3 + QUIC (opt-in): WG datagrams ride an unreliable QUIC
    /// datagram stream over a coturn allocation. QUIC's congestion control
    /// smooths the relay's buffer-bloat latency spikes and its keepalive holds
    /// the TURN permission fresh — the carrier stays healthier on a hostile
    /// (corp-VPN) relay path than raw fire-and-forget. `_peer` owns the QUIC
    /// endpoint (→ `RelayUdpSocket` → `RelayConn` → TURN allocation); it is
    /// held only to keep that stack alive for the connection's lifetime.
    QuicRelay {
        _peer: QuicPeer,
        conn: quinn::Connection,
        /// rc.181 — latched when `send_datagram` reports `ConnectionLost` (the
        /// QUIC-over-TURN carrier died). Same fast re-allocate signal as
        /// [`Carrier::Relay`]'s `dead`.
        dead: AtomicBool,
    },
}

impl Carrier {
    pub fn direct(sock: Arc<UdpSocket>, dst: SocketAddr) -> Arc<Self> {
        Arc::new(Carrier::Direct { sock, dst })
    }

    pub fn relay(conn: Arc<dyn RelayConn>, dst: SocketAddr) -> Arc<Self> {
        Arc::new(Carrier::Relay {
            conn,
            dst,
            dead: AtomicBool::new(false),
        })
    }

    /// Build a QUIC-over-TURN carrier over an existing TURN allocation `conn`,
    /// with the peer's relayed `dst`. `am_server` (deterministic — the
    /// lexicographically-smaller pubkey serves) picks the QUIC role; the client
    /// accepts any cert because WireGuard authenticates end-to-end INSIDE the
    /// datagrams. Returns `Err` (→ the caller falls back to the raw
    /// `Carrier::relay`) on a handshake timeout, or if the negotiated datagram
    /// budget can't hold a WG packet (`min_datagram`). Both ends install the
    /// coturn permission (stray `\x00`) before the bidirectional handshake.
    pub async fn quic_relay(
        conn: Arc<dyn RelayConn>,
        dst: SocketAddr,
        am_server: bool,
        min_datagram: usize,
        timeout: Duration,
    ) -> anyhow::Result<Arc<Self>> {
        use anyhow::{Context as _, bail};

        let _ = conn.send_to(b"\x00", dst).await;
        let sock = Arc::new(RelayUdpSocket::new(conn)?);

        let (peer, quic) = if am_server {
            let peer = QuicPeer::server_over_relay_datagram(sock)?;
            let quic = tokio::time::timeout(timeout, peer.accept())
                .await
                .context("QUIC-over-TURN server accept timed out")?
                .context("QUIC-over-TURN server: no incoming connection")?
                .context("QUIC-over-TURN server handshake failed")?;
            (peer, quic)
        } else {
            let peer = QuicPeer::client_over_relay_datagram(sock)?;
            let quic = tokio::time::timeout(timeout, peer.connect(dst))
                .await
                .context("QUIC-over-TURN client connect timed out")?
                .context("QUIC-over-TURN client handshake failed")?;
            (peer, quic)
        };

        let budget = quic.max_datagram_size().unwrap_or(0);
        if budget < min_datagram {
            bail!(
                "QUIC datagram budget {budget} < WG packet {min_datagram}; falling back to raw relay"
            );
        }
        Ok(Arc::new(Carrier::QuicRelay {
            _peer: peer,
            conn: quic,
            dead: AtomicBool::new(false),
        }))
    }

    /// A direct UDP carrier (vs a coturn relay). The runtime uses this to
    /// decide handshake direction: a direct carrier needs BOTH ends to
    /// initiate (bilateral hole-punch — see `install_ready`).
    pub fn is_direct(&self) -> bool {
        matches!(self, Carrier::Direct { .. })
    }

    /// rc.181 — a relay carrier whose `send` hard-errored (a TURNS/TCP reset,
    /// or a lost QUIC-over-TURN connection). The runtime's health sweep treats
    /// this as an immediate carrier death and re-allocates on the next tick,
    /// without waiting out the multi-sweep rx-flat heuristic. Always `false`
    /// for a direct carrier (its `send` failing is a dropped UDP datagram, not
    /// a dead session).
    pub fn is_dead(&self) -> bool {
        match self {
            Carrier::Direct { .. } => false,
            Carrier::Relay { dead, .. } | Carrier::QuicRelay { dead, .. } => {
                dead.load(Ordering::Relaxed)
            }
        }
    }

    async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Carrier::Direct { sock, dst } => sock.send_to(buf, *dst).await,
            // A TURNS/TCP write error (`tcp-turn write: connection reset`) or a
            // dead UDP relay means the allocation is gone — latch `dead` so the
            // health sweep re-allocates promptly (next ≤5 s tick) instead of
            // waiting out the ~20 s rx-flat heuristic.
            Carrier::Relay { conn, dst, dead } => {
                let r = conn.send_to(buf, *dst).await;
                if r.is_err() {
                    dead.store(true, Ordering::Relaxed);
                }
                r
            }
            // send_datagram queues on the connection (quinn's driver flushes it
            // over the RelayUdpSocket). `ConnectionLost` = the carrier died →
            // latch `dead`; `TooLarge`/`Disabled`/`UnsupportedByPeer` are a
            // healthy conn refusing THIS datagram, so the WG layer just treats
            // them like any dropped datagram.
            Carrier::QuicRelay { conn, dead, .. } => {
                match conn.send_datagram(Bytes::copy_from_slice(buf)) {
                    Ok(()) => Ok(buf.len()),
                    Err(e) => {
                        if matches!(e, quinn::SendDatagramError::ConnectionLost(_)) {
                            dead.store(true, Ordering::Relaxed);
                        }
                        Err(io::Error::other(e))
                    }
                }
            }
        }
    }

    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Carrier::Direct { sock, .. } => Ok(sock.recv_from(buf).await?.0),
            Carrier::Relay { conn, .. } => Ok(conn.recv_from(buf).await?.0),
            // A dead QUIC connection errors here → the recv task exits → the
            // runtime's health sweep sees rx go flat and rebuilds the carrier.
            Carrier::QuicRelay { conn, .. } => {
                let d = conn.read_datagram().await.map_err(io::Error::other)?;
                let n = d.len().min(buf.len());
                buf[..n].copy_from_slice(&d[..n]);
                Ok(n)
            }
        }
    }
}

/// rc.137 — lock-free per-peer traffic counters. `tx` = IP packets we
/// encapsulated + sent; `rx` = inbound IP packets we decapsulated to the TUN.
/// The runtime's direct→relay fallback reads these WITHOUT locking the `Tunn`
/// (locking it on a timer inside the packet loop is what added a ~660 ms
/// latency spike every sweep in rc.136), and uses "tx climbing while rx is
/// flat" as a fast, lock-free "this carrier is one-way / dead" signal.
#[derive(Default)]
pub struct PeerStats {
    tx: AtomicU64,
    rx: AtomicU64,
    /// Phase C — latched `true` the moment a WG handshake to this peer
    /// completes (a session exists), for EITHER role (the responder establishes
    /// on receiving the init, the initiator on receiving the response). The
    /// srflx / public-direct health deadline reads this LOCK-FREE to tell a live
    /// punch from a pre-handshake zombie — whose `tx`/`rx` counters stay flat
    /// (handshake packets touch neither), so the rx-flat heuristic can't see it
    /// and boringtun stops even keepalives once the attempt expires at ~90 s.
    handshake: AtomicBool,
}

/// Demux routing table: a direct peer's source address → its `Tunn` + stats.
/// One shared map drives the single direct-socket recv loop (rc.134).
type DemuxRoutes = HashMap<SocketAddr, (Arc<Mutex<Tunn>>, Arc<PeerStats>)>;

/// Phase A — a WireGuard **handshake initiation** that arrived on a shared
/// direct socket from a source address NO demux route matches. Two real cases
/// produce this: a NAT'd peer dialling our advertised PUBLIC endpoint (its
/// NAT'd source can't be known in advance — the direct-to-public accept), and
/// an already-known peer that restarted/roamed onto a new ephemeral port (the
/// field-observed stale-port race: its 148-byte init arrived and was silently
/// dropped, so the handshake died until the "restart the exit last" workaround).
/// The demux can't act on it — it has no `&mut WgDevice` — so it forwards the
/// packet to the runtime's select loop, which authenticates it (a probe `Tunn`
/// performs the full Noise-IK validation; `parse_handshake_anon` alone is NOT
/// proof of identity) and only then installs/re-points the peer.
pub struct DirectInbound {
    pub src: SocketAddr,
    pub sock: Arc<UdpSocket>,
    pub packet: Vec<u8>,
}

/// Phase A — min interval between forwarded unknown-source initiations from
/// the SAME source, and the cap on tracked sources. WG retransmits an
/// unanswered init every ~5 s, so 2 s forwards every genuine attempt while a
/// junk flood collapses to ≤1 event per source per 2 s (and ≤64 sources).
const UNKNOWN_INIT_MIN_INTERVAL: Duration = Duration::from_secs(2);
const UNKNOWN_INIT_MAX_SOURCES: usize = 64;

/// One installed peer: its `Tunn`, its carrier, and the background tasks
/// that pump it. Dropping aborts the tasks.
struct Peer {
    tunn: Arc<Mutex<Tunn>>,
    carrier: Arc<Carrier>,
    overlay_ip: Ipv4Addr,
    tasks: Vec<JoinHandle<()>>,
    stats: Arc<PeerStats>,
    /// rc.134 — for a SHARED-direct peer, the source address it sends from
    /// (== the carrier dst). `Some` ⇒ its inbound is handled by the device's
    /// shared demux loop (no per-peer recv task), and `remove_peer` must
    /// un-register it from the demux routing table. `None` for relay /
    /// dedicated-socket carriers (those own a per-peer recv task).
    direct_src: Option<SocketAddr>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// rc.134/143 — shared-routes demux for N direct-LAN peers. One UDP socket
/// **per usable LAN interface** (each bound to its interface IP, not `0.0.0.0`
/// — rc.143), and one recv loop per socket, all routing into a single shared
/// `routes` map that dispatches each datagram to the peer whose endpoint matches
/// the source address (the WireGuard model). Many same-subnet peers can be
/// direct at once (lifting the rc.131 "one direct peer" cap), and each peer is
/// sent/received on the socket bound to the interface it shares a subnet with —
/// so a full-tunnel VPN can't hijack the egress (a single `0.0.0.0` socket let
/// the OS route the reply out the VPN, breaking direct on VPN'd hosts).
struct DirectDemux {
    routes: Arc<Mutex<DemuxRoutes>>,
    /// The per-interface sockets that have a live demux recv loop (deduped by
    /// local address so `ensure_direct_demux` is idempotent per interface).
    socks: Vec<Arc<UdpSocket>>,
    tasks: Vec<JoinHandle<()>>,
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
    /// rc.134 — the shared direct-LAN socket + demux loop, lazily created on
    /// the first direct peer (`ensure_direct_demux`). `None` until then.
    direct: Option<DirectDemux>,
    /// Phase A — unknown-source handshake initiations forwarded by the demux
    /// loops. The sender is cloned into each demux loop; the receiver is taken
    /// once by the runtime ([`take_direct_events`](Self::take_direct_events)).
    direct_events_tx: mpsc::Sender<DirectInbound>,
    direct_events_rx: Option<mpsc::Receiver<DirectInbound>>,
    /// Phase C — STUN Binding responses forwarded by the demux loops (a
    /// datagram carrying the STUN cookie that is not WG-shaped). The srflx
    /// keepalive task's query rides a shared direct socket whose `recv_from`
    /// the demux owns, so the response can't be read directly; it arrives here.
    /// Cloned into each demux loop; the receiver is taken once by the runtime
    /// ([`take_stun_events`](Self::take_stun_events)). Dropped harmlessly if
    /// nobody took it (the srflx tier is off).
    stun_events_tx: mpsc::Sender<crate::transport::stun::StunInbound>,
    stun_events_rx: Option<mpsc::Receiver<crate::transport::stun::StunInbound>>,
}

impl Drop for WgDevice {
    fn drop(&mut self) {
        if let Some(d) = &self.direct {
            for t in &d.tasks {
                t.abort();
            }
        }
    }
}

impl WgDevice {
    /// Build a device from a static secret. Returns the device plus the
    /// receiver for decrypted inbound IP packets (the TUN bridge / tests
    /// drain it).
    pub fn new(secret: StaticSecret) -> (Self, mpsc::Receiver<Vec<u8>>) {
        let public = PublicKey::from(&secret);
        let (tun_tx, tun_rx) = mpsc::channel(256);
        let (direct_events_tx, direct_events_rx) = mpsc::channel(16);
        let (stun_events_tx, stun_events_rx) = mpsc::channel(16);
        (
            Self {
                secret,
                public,
                peers: HashMap::new(),
                router: Router::new(),
                tun_tx,
                next_index: 1,
                direct: None,
                direct_events_tx,
                direct_events_rx: Some(direct_events_rx),
                stun_events_tx,
                stun_events_rx: Some(stun_events_rx),
            },
            tun_rx,
        )
    }

    /// Phase A — take the receiver for unknown-source handshake initiations
    /// (see [`DirectInbound`]). `None` after the first take. A device whose
    /// receiver is never taken just drops events once the small channel fills
    /// (`try_send` in the demux) — harmless for tests that don't care.
    pub fn take_direct_events(&mut self) -> Option<mpsc::Receiver<DirectInbound>> {
        self.direct_events_rx.take()
    }

    /// Phase C — take the receiver for demux-routed STUN Binding responses (see
    /// [`stun_events_tx`](Self::stun_events_tx) and
    /// [`crate::transport::stun::StunInbound`]). `None` after the first take.
    /// The srflx keepalive task owns it and matches responses to its own
    /// queries by source + transaction id.
    pub fn take_stun_events(
        &mut self,
    ) -> Option<mpsc::Receiver<crate::transport::stun::StunInbound>> {
        self.stun_events_rx.take()
    }

    /// Phase A — the demux source address a SHARED-direct peer is currently
    /// registered under (`None` for relay/dedicated-socket peers or unknown
    /// pubkeys). Lets the runtime tell a duplicate event for an
    /// already-current source from a genuine roam to a new one.
    pub fn direct_src_of(&self, peer_public: &[u8; 32]) -> Option<SocketAddr> {
        self.peers.get(peer_public)?.direct_src
    }

    /// Phase A — EXTRACT + AUTHENTICATE a WireGuard handshake INITIATION with
    /// no per-peer state, returning the initiator's static public key **only if
    /// the init is cryptographically genuine**. Used by the runtime before it
    /// acts on a [`DirectInbound`] (installs / re-points a peer's carrier to the
    /// packet's source), so a forger can't hijack a peer's route.
    ///
    /// Two steps, because they prove different things:
    /// 1. `parse_handshake_anon` decrypts the *claimed* static — but a forger
    ///    who copies a victim's PUBLIC key (public data) can craft an init that
    ///    parses to that key (the encrypted-static key derives from the
    ///    initiator's own ephemeral + our public static, both attacker-known).
    ///    So the parsed key is a CLAIM, not proof.
    /// 2. A throwaway [`Tunn`] built with that claimed key decapsulates the
    ///    init: WireGuard also AEAD-seals the timestamp under a key derived from
    ///    `DH(our_static_priv, initiator_static_pub)`. Only the holder of the
    ///    claimed key's PRIVATE half computes the matching
    ///    `DH(initiator_static_priv, our_static_pub)`; a forger cannot, so the
    ///    timestamp open fails and `decapsulate` returns `Done`/`Err`. A genuine
    ///    init yields a `WriteToNetwork` handshake RESPONSE ⇒ authenticated.
    ///
    /// The probe Tunn's response is discarded; the real installed Tunn re-runs
    /// the same init and emits the response that reaches the peer (each Tunn has
    /// independent anti-replay state, so the re-run isn't rejected).
    pub fn authenticate_init(&self, init: &[u8]) -> Option<[u8; 32]> {
        let claimed = {
            let Ok(Packet::HandshakeInit(hi)) = Tunn::parse_incoming_packet(init) else {
                return None;
            };
            boringtun::noise::handshake::parse_handshake_anon(&self.secret, &self.public, &hi)
                .ok()?
                .peer_static_public
        };
        let mut probe = Tunn::new(
            self.secret.clone(),
            PublicKey::from(claimed),
            None,
            None,
            0,
            None,
        );
        let mut out = vec![0u8; WG_BUF];
        match probe.decapsulate(None, init, &mut out) {
            TunnResult::WriteToNetwork(_) => Some(claimed),
            _ => None,
        }
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

        let stats = Arc::new(PeerStats::default());

        // recv task: carrier → decapsulate → tun / network echo.
        let recv_tunn = tunn.clone();
        let recv_carrier = carrier.clone();
        let tun_tx = self.tun_tx.clone();
        let recv_stats = stats.clone();
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
                process_inbound(&mut t, n, &mut buf, &recv_carrier, &tun_tx, &recv_stats).await;
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
                stats,
                direct_src: None,
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

    /// rc.134 — ensure the shared direct-LAN socket + its demux loop exist.
    /// Idempotent; the first direct peer triggers it. The demux loop reads the
    /// socket forever and routes each datagram to the peer matching its source
    /// address (replacing the per-peer recv loop for direct carriers).
    pub fn ensure_direct_demux(&mut self, sock: Arc<UdpSocket>) {
        let local = match sock.local_addr() {
            Ok(a) => a,
            Err(e) => {
                warn!(%e, "wg: direct socket has no local_addr; skipping demux loop");
                return;
            }
        };
        let tun_tx = self.tun_tx.clone();
        let demux = self.direct.get_or_insert_with(|| DirectDemux {
            routes: Arc::new(Mutex::new(HashMap::new())),
            socks: Vec::new(),
            tasks: Vec::new(),
        });
        // One recv loop per interface socket, all feeding the shared `routes`.
        // Idempotent per interface so repeated installs don't spawn duplicates.
        if demux
            .socks
            .iter()
            .any(|s| s.local_addr().map(|a| a == local).unwrap_or(false))
        {
            return;
        }
        let task = tokio::spawn(run_direct_demux(
            sock.clone(),
            demux.routes.clone(),
            tun_tx,
            self.direct_events_tx.clone(),
            self.stun_events_tx.clone(),
        ));
        demux.socks.push(sock);
        demux.tasks.push(task);
    }

    /// Phase A — process ONE datagram for an already-registered direct route
    /// (the runtime calls this right after installing/re-pointing a peer from a
    /// [`DirectInbound`] event, so the very initiation that triggered the event
    /// is answered immediately instead of waiting ~5 s for the initiator's
    /// retransmit). No-op if `src` has no route (nothing was installed).
    pub async fn feed_direct(&self, src: SocketAddr, sock: Arc<UdpSocket>, packet: &[u8]) {
        let Some(demux) = &self.direct else {
            return;
        };
        let entry = demux.routes.lock().await.get(&src).cloned();
        let Some((tunn, stats)) = entry else {
            return;
        };
        let reply = Carrier::Direct { sock, dst: src };
        let mut buf = packet.to_vec();
        let n = buf.len();
        let mut t = tunn.lock().await;
        process_inbound(&mut t, n, &mut buf, &reply, &self.tun_tx, &stats).await;
    }

    /// rc.134 — install a peer reached over the SHARED direct socket. Its
    /// inbound is handled by the device's single demux loop (routed by source
    /// address), so N direct peers share one socket without racing — unlike
    /// `add_peer`, which spawns a per-peer recv loop. `ensure_direct_demux`
    /// must have run. `initiate` ⇒ send a handshake init now (direct carriers
    /// initiate bilaterally so both firewalls open — rc.133).
    pub async fn add_direct_peer(
        &mut self,
        sock: Arc<UdpSocket>,
        peer_public: [u8; 32],
        overlay_ip: Ipv4Addr,
        dst: SocketAddr,
        initiate: bool,
    ) {
        let Some(demux) = &self.direct else {
            warn!("wg: add_direct_peer before ensure_direct_demux; ignoring");
            return;
        };
        // Send from the interface-bound socket that shares the peer's subnet
        // (rc.143) — forces egress out the right NIC past a full-tunnel VPN.
        let carrier = Carrier::direct(sock, dst);

        let index = self.next_index;
        self.next_index = self.next_index.wrapping_add(1);
        let tunn = Arc::new(Mutex::new(Tunn::new(
            self.secret.clone(),
            PublicKey::from(peer_public),
            None,
            Some(KEEPALIVE_SECS),
            index,
            None,
        )));

        let stats = Arc::new(PeerStats::default());
        // Register for demux BEFORE the handshake so inbound is routed.
        demux
            .routes
            .lock()
            .await
            .insert(dst, (tunn.clone(), stats.clone()));

        // Timer task only — no recv task; the shared demux loop delivers
        // this peer's inbound.
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
                tasks: vec![timer_task],
                stats,
                direct_src: Some(dst),
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
    /// route). Used when the netmap drops a peer (ACL change / leave) or to
    /// re-install it with a different carrier (relay→direct upgrade, rc.134).
    /// `async` because un-registering a shared-direct peer locks the demux
    /// routing table.
    pub async fn remove_peer(&mut self, peer_public: &[u8; 32]) {
        if let Some(p) = self.peers.remove(peer_public) {
            self.router.remove(&p.overlay_ip);
            if let (Some(src), Some(demux)) = (p.direct_src, &self.direct) {
                demux.routes.lock().await.remove(&src);
            }
            // P5/S3b — if this was the global-v6 exit peer, stop routing v6 to a
            // now-dead pubkey (defensive; the reconcile re-asserts on its next
            // pass if the peer is still the valid, reachable exit).
            if self.router.v6_exit() == Some(*peer_public) {
                self.router.set_v6_exit(None);
            }
        }
    }

    /// Phase 1 — set the subnet routes a peer is a router for, so packets to
    /// those CIDRs are encapsulated to it (longest-prefix after the host `/32`s).
    /// Replaces any previously-set subnets for the peer; empty clears them.
    pub fn set_peer_subnets(&mut self, peer_public: [u8; 32], subnets: &[super::router::Cidr]) {
        self.router.set_subnets(peer_public, subnets);
    }

    /// P5/S3b exit-node — route this node's GLOBAL IPv6 egress through `pubkey`
    /// (or `None` to clear → global v6 drops fail-closed). Set by the exit-routing
    /// reconcile once the v6 carrier exemptions are pinned.
    pub fn set_v6_exit(&mut self, pubkey: Option<[u8; 32]>) {
        self.router.set_v6_exit(pubkey);
    }

    /// The current global-v6 exit peer's pubkey, if any — lets the reconcile
    /// re-assert it (idempotent) after a `remove_peer` may have cleared it.
    pub fn v6_exit(&self) -> Option<[u8; 32]> {
        self.router.v6_exit()
    }

    /// Number of installed peers.
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Encapsulate + send a raw IP packet to whichever peer owns its destination.
    /// v4 (or an overlay-internal derived-ULA v6 — see
    /// [`Router::dst_of_ip_packet`]) routes on the v4 table. P5/S3b — a GLOBAL v6
    /// destination ([`Router::is_global_v6_dst`]) routes to the configured
    /// [`v6_exit`](Self::v6_exit) peer if set, else is dropped (fail-closed, no
    /// leak). `false` if no route or no session.
    pub async fn send_ip_packet(&self, packet: &[u8]) -> bool {
        // v4 or an overlay-internal derived-ULA v6 → the v4 routing table.
        if let Some(dst) = Router::dst_of_ip_packet(packet) {
            let Some(pubkey) = self.router.route(&dst) else {
                return false;
            };
            return self.send_to_peer(&pubkey, packet).await;
        }
        // A global (non-overlay) v6 destination → the v6 exit peer, if any.
        if Router::is_global_v6_dst(packet).is_some()
            && let Some(pubkey) = self.router.v6_exit()
        {
            return self.send_to_peer(&pubkey, packet).await;
        }
        false
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
            TunnResult::WriteToNetwork(b) => {
                let ok = peer.carrier.send(b).await.is_ok();
                if ok {
                    peer.stats.tx.fetch_add(1, Ordering::Relaxed);
                }
                ok
            }
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

    /// rc.137 — LOCK-FREE `(tx, rx)` IP-packet counts for `peer_public`
    /// (`None` if unknown). Read by the runtime's fallback sweep WITHOUT
    /// locking the `Tunn`, so the periodic health check can't stall the packet
    /// path (the rc.136 regression). `tx` climbing while `rx` is flat ⇒ the
    /// carrier is one-way / dead ⇒ fall back to relay.
    pub fn peer_traffic(&self, peer_public: &[u8; 32]) -> Option<(u64, u64)> {
        let peer = self.peers.get(peer_public)?;
        Some((
            peer.stats.tx.load(Ordering::Relaxed),
            peer.stats.rx.load(Ordering::Relaxed),
        ))
    }

    /// rc.181 — `true` if `peer_public`'s carrier has latched a hard send error
    /// (a TURNS/TCP reset or a lost QUIC-over-TURN connection); `false` for a
    /// healthy or direct carrier; `None` if the peer is unknown. Lock-free. The
    /// health sweep uses this as a FAST carrier-death signal — re-allocate on
    /// the next tick instead of waiting for the multi-sweep rx-flat heuristic.
    pub fn peer_carrier_dead(&self, peer_public: &[u8; 32]) -> Option<bool> {
        Some(self.peers.get(peer_public)?.carrier.is_dead())
    }

    /// Phase C — LOCK-FREE: has the WG handshake to `peer_public` completed (a
    /// session exists)? `None` if the peer is unknown. The health sweep reads
    /// this to time out a srflx / public-direct punch carrier that NEVER
    /// established — its `tx`/`rx` counters stay flat pre-handshake, so the
    /// rx-flat heuristic can't detect it. Latched by `process_inbound` the
    /// instant a session appears; never cleared for the carrier's life (a fresh
    /// carrier gets a fresh `PeerStats`).
    pub fn peer_handshake_done(&self, peer_public: &[u8; 32]) -> Option<bool> {
        Some(
            self.peers
                .get(peer_public)?
                .stats
                .handshake
                .load(Ordering::Relaxed),
        )
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
    stats: &PeerStats,
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
            stats.rx.fetch_add(1, Ordering::Relaxed);
            let _ = tun_tx.send(pkt.to_vec()).await;
        }
        TunnResult::WriteToTunnelV6(pkt, _) => {
            stats.rx.fetch_add(1, Ordering::Relaxed);
            let _ = tun_tx.send(pkt.to_vec()).await;
        }
        TunnResult::Done => {}
        TunnResult::Err(e) => debug!(?e, "wg decapsulate error"),
    }
    // Phase C — latch "handshake completed" the moment this peer's session is
    // live. `process_inbound` only runs on a packet FROM the peer, so a set flag
    // means the peer reached us AND a session exists (the responder establishes
    // on the init it just answered; the initiator on the response it just got) —
    // exactly the "punch succeeded" signal the health deadline needs. Set-once;
    // the Tunn lock is already held here (rc.137: never lock it from the sweep).
    if !stats.handshake.load(Ordering::Relaxed) && t.time_since_last_handshake().is_some() {
        stats.handshake.store(true, Ordering::Relaxed);
    }
}

/// Phase C — the WireGuard datagram shape: a 4-byte little-endian message type
/// in `1..=4` (Init / Response / Cookie / Data), so bytes `1..4` are always
/// zero. Used to EXCLUDE WG traffic from the STUN-cookie demux check — a WG data
/// packet whose receiver-index bytes happen to equal the STUN magic cookie must
/// still route as WG. A real STUN Binding message can never satisfy this (its
/// big-endian 16-bit type puts `0x01` in byte 1), so the two classes are
/// disjoint. Intentionally does NOT validate length/contents — it's only the
/// exclusion half of the STUN discriminator.
fn is_wg_shaped(pkt: &[u8]) -> bool {
    pkt.len() >= 4 && matches!(pkt[0], 1..=4) && pkt[1] == 0 && pkt[2] == 0 && pkt[3] == 0
}

/// rc.134 — the shared direct-socket recv loop. Reads every datagram and
/// routes it to the peer registered for its SOURCE address (a direct peer
/// sends from the same address we send to), processing it with that peer's
/// `Tunn` and replying over the same socket. One loop serves all direct peers,
/// so N same-subnet peers share one socket without racing. Exits when the
/// socket errors (device gone / dropped).
async fn run_direct_demux(
    sock: Arc<UdpSocket>,
    routes: Arc<Mutex<DemuxRoutes>>,
    tun_tx: mpsc::Sender<Vec<u8>>,
    events: mpsc::Sender<DirectInbound>,
    stun_events: mpsc::Sender<crate::transport::stun::StunInbound>,
) {
    let mut buf = vec![0u8; WG_BUF];
    // Phase A — per-source rate limit for forwarded unknown-source initiations
    // (local to this loop task: no lock needed, one map per interface socket).
    let mut recent_unknown: HashMap<SocketAddr, Instant> = HashMap::new();
    loop {
        let (n, src) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                debug!(%e, "wg direct demux recv ended; loop exiting");
                break;
            }
        };
        // Phase C — demux-routed STUN. A datagram carrying the STUN magic
        // cookie that is NOT WireGuard-shaped is a Binding response for the
        // srflx keepalive task, whose query rides this shared socket (which the
        // task can't `recv_from` itself). Forward it to the STUN sink and skip
        // WG routing. The two shapes are DISJOINT — WG's 4-byte LE type header
        // leaves bytes 1..4 == 0 while a real STUN Binding message always has
        // 0x01 in byte 1 — so this never steals a WG datagram (and a WG data
        // packet whose index bytes happen to equal the cookie is kept by the
        // `is_wg_shaped` exclusion). Checked BEFORE the routes lookup so a STUN
        // reply is never mistaken for peer traffic. `try_send` drops it if
        // nobody took the receiver (srflx tier off) — harmless.
        if crate::transport::stun::has_stun_cookie(&buf[..n]) && !is_wg_shaped(&buf[..n]) {
            let _ = stun_events.try_send(crate::transport::stun::StunInbound {
                src,
                packet: buf[..n].to_vec(),
            });
            continue;
        }
        // Clone the Arcs out under the routes lock, then release it before the
        // (potentially awaiting) process_inbound so the demux map stays
        // contended only briefly.
        let entry = routes.lock().await.get(&src).cloned();
        let Some((tunn, stats)) = entry else {
            // Phase A — an UNKNOWN source is no longer unconditionally dropped:
            // if the datagram is a well-formed WG handshake INITIATION, forward
            // it to the runtime (a NAT'd peer dialling our public endpoint, or
            // a known peer that restarted onto a new port — the stale-port
            // race). The runtime authenticates before acting; rate-limited so
            // a junk flood can't churn the channel. Anything else from an
            // unknown source stays dropped.
            if matches!(
                Tunn::parse_incoming_packet(&buf[..n]),
                Ok(Packet::HandshakeInit(_))
            ) {
                if recent_unknown.len() >= UNKNOWN_INIT_MAX_SOURCES {
                    recent_unknown.retain(|_, t| t.elapsed() < UNKNOWN_INIT_MIN_INTERVAL);
                }
                let fresh = recent_unknown
                    .get(&src)
                    .is_none_or(|t| t.elapsed() >= UNKNOWN_INIT_MIN_INTERVAL);
                if fresh && recent_unknown.len() < UNKNOWN_INIT_MAX_SOURCES {
                    recent_unknown.insert(src, Instant::now());
                    let _ = events.try_send(DirectInbound {
                        src,
                        sock: sock.clone(),
                        packet: buf[..n].to_vec(),
                    });
                }
            }
            continue;
        };
        let reply = Carrier::Direct {
            sock: sock.clone(),
            dst: src,
        };
        let mut t = tunn.lock().await;
        process_inbound(&mut t, n, &mut buf, &reply, &tun_tx, &stats).await;
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

    /// Phase C — `peer_handshake_done` latches once the WG session establishes
    /// (the lock-free signal the srflx/public health deadline reads). Unknown
    /// peers report `None`; a fresh peer reports `Some(false)` until the
    /// handshake completes, then `Some(true)` on BOTH the initiator and the
    /// responder (each sets it from an inbound packet).
    #[tokio::test(flavor = "multi_thread")]
    async fn peer_handshake_done_latches_on_session_establish() {
        let a = WgKeypair::generate();
        let b = WgKeypair::generate();
        let sock_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let sock_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = sock_a.local_addr().unwrap();
        let addr_b = sock_b.local_addr().unwrap();

        let (mut dev_a, _rx_a) = WgDevice::new(a.secret.clone());
        let (mut dev_b, _rx_b) = WgDevice::new(b.secret.clone());

        // Unknown peer → None.
        assert_eq!(dev_a.peer_handshake_done(&b.public.to_bytes()), None);

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

        // Freshly added, pre-handshake → Some(false).
        assert_eq!(dev_a.peer_handshake_done(&b.public.to_bytes()), Some(false));

        wait_connected(&dev_a, &b.public.to_bytes()).await;
        wait_connected(&dev_b, &a.public.to_bytes()).await;

        // Both ends latch true (each established via an inbound packet).
        assert_eq!(dev_a.peer_handshake_done(&b.public.to_bytes()), Some(true));
        assert_eq!(dev_b.peer_handshake_done(&a.public.to_bytes()), Some(true));
    }

    /// rc.134 — one HUB device serves TWO peers over a SINGLE shared socket
    /// (the source-address demux). Proves N direct peers coexist without the
    /// per-peer-recv-loop race the old "one direct peer" cap worked around:
    /// both handshakes complete and the hub's data reaches the correct peer.
    #[tokio::test(flavor = "multi_thread")]
    async fn shared_direct_socket_demuxes_multiple_peers() {
        const IP_C: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 3);
        let hub = WgKeypair::generate();
        let b = WgKeypair::generate();
        let c = WgKeypair::generate();

        let hub_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let sock_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let sock_c = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let hub_addr = hub_sock.local_addr().unwrap();
        let addr_b = sock_b.local_addr().unwrap();
        let addr_c = sock_c.local_addr().unwrap();

        let (mut dev_hub, _rx_hub) = WgDevice::new(hub.secret.clone());
        let (mut dev_b, mut rx_b) = WgDevice::new(b.secret.clone());
        let (mut dev_c, mut rx_c) = WgDevice::new(c.secret.clone());

        // Hub: BOTH peers over the ONE shared socket (inbound demuxed by src).
        dev_hub.ensure_direct_demux(hub_sock.clone());
        dev_hub
            .add_direct_peer(hub_sock.clone(), b.public.to_bytes(), IP_B, addr_b, true)
            .await;
        dev_hub
            .add_direct_peer(hub_sock.clone(), c.public.to_bytes(), IP_C, addr_c, true)
            .await;

        // Peers: dedicated sockets, respond to the hub's initiation.
        dev_b.add_peer(
            hub.public.to_bytes(),
            IP_A,
            Carrier::direct(sock_b.clone(), hub_addr),
            false,
        );
        dev_c.add_peer(
            hub.public.to_bytes(),
            IP_A,
            Carrier::direct(sock_c.clone(), hub_addr),
            false,
        );

        // Both handshakes complete THROUGH the one shared socket — the hub's
        // demux routed each peer's response to the right Tunn.
        wait_connected(&dev_hub, &b.public.to_bytes()).await;
        wait_connected(&dev_hub, &c.public.to_bytes()).await;

        // And the hub's data reaches the correct peer (no cross-talk).
        let pkt_b = synthetic_ipv4(IP_A, IP_B, b"hub-to-b");
        send_until_ok(&dev_hub, &b.public.to_bytes(), &pkt_b).await;
        let got_b = tokio::time::timeout(Duration::from_secs(15), rx_b.recv())
            .await
            .expect("B did not receive its packet")
            .expect("tun channel closed");
        assert_eq!(got_b, pkt_b);

        let pkt_c = synthetic_ipv4(IP_A, IP_C, b"hub-to-c");
        send_until_ok(&dev_hub, &c.public.to_bytes(), &pkt_c).await;
        let got_c = tokio::time::timeout(Duration::from_secs(15), rx_c.recv())
            .await
            .expect("C did not receive its packet")
            .expect("tun channel closed");
        assert_eq!(got_c, pkt_c);
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

    /// DERP carrier: two nodes carry WG over a pubkey-addressed relay with NO
    /// UDP anywhere (the both-UDP-blocked tier). Mirrors
    /// `wg_handshake_and_data_over_relay_conn`, but each side's `RelayConn` is a
    /// `DerpConn` fed by a `DerpMux`, and a mock in-process relay plays the
    /// server (`crate::ws::derp`): read a node's outbound `[dst||payload]`,
    /// deliver `[src||payload]` to the dst's mux. Proves RAW WG rides DERP both
    /// ways — the pubkey pinning makes the recv-source discard harmless.
    #[tokio::test(flavor = "multi_thread")]
    async fn wg_handshake_and_data_over_derp() {
        use crate::transport::derp::DerpMux;

        let a = WgKeypair::generate();
        let b = WgKeypair::generate();
        let a_pk = a.public.to_bytes();
        let b_pk = b.public.to_bytes();

        let (mux_a, mut a_out) = DerpMux::new(a_pk);
        let (mux_b, mut b_out) = DerpMux::new(b_pk);

        // Mock relay A→B: A frames [B||payload]; deliver to B as [A||payload].
        {
            let mux_b = Arc::clone(&mux_b);
            tokio::spawn(async move {
                while let Some(frame) = a_out.recv().await {
                    let mut out = a_pk.to_vec();
                    out.extend_from_slice(&frame[32..]);
                    mux_b.deliver(&out);
                }
            });
        }
        // Mock relay B→A.
        {
            let mux_a = Arc::clone(&mux_a);
            tokio::spawn(async move {
                while let Some(frame) = b_out.recv().await {
                    let mut out = b_pk.to_vec();
                    out.extend_from_slice(&frame[32..]);
                    mux_a.deliver(&out);
                }
            });
        }

        let conn_a: Arc<dyn RelayConn> = Arc::new(mux_a.conn_for(b_pk));
        let conn_b: Arc<dyn RelayConn> = Arc::new(mux_b.conn_for(a_pk));

        let (mut dev_a, _rx_a) = WgDevice::new(a.secret.clone());
        let (mut dev_b, mut rx_b) = WgDevice::new(b.secret.clone());

        // Synthetic dsts — `DerpConn` is pubkey-addressed and ignores them.
        let dst_b: SocketAddr = "100.64.0.2:51820".parse().unwrap();
        let dst_a: SocketAddr = "100.64.0.1:51820".parse().unwrap();

        dev_a.add_peer(b_pk, IP_B, Carrier::relay(conn_a, dst_b), true);
        dev_b.add_peer(a_pk, IP_A, Carrier::relay(conn_b, dst_a), false);

        wait_connected(&dev_a, &b_pk).await;

        let pkt = synthetic_ipv4(IP_A, IP_B, b"hello-over-derp");
        send_until_ok(&dev_a, &b_pk, &pkt).await;

        let got = tokio::time::timeout(Duration::from_secs(15), rx_b.recv())
            .await
            .expect("B did not receive a decrypted packet over DERP in time")
            .expect("tun channel closed");
        assert_eq!(
            got, pkt,
            "decrypted IP packet must arrive over the DERP carrier"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wg_handshake_and_data_over_quic_relay() {
        use crate::transport::relay::UdpRelayConn;

        let a = WgKeypair::generate();
        let b = WgKeypair::generate();

        let sock_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sock_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr_a = sock_a.local_addr().unwrap();
        let addr_b = sock_b.local_addr().unwrap();
        let conn_a: Arc<dyn RelayConn> = Arc::new(UdpRelayConn(sock_a));
        let conn_b: Arc<dyn RelayConn> = Arc::new(UdpRelayConn(sock_b));

        // Same deterministic role rule as `install_ready`: the smaller pubkey is
        // BOTH the WG initiator AND the QUIC server. Build both carriers
        // CONCURRENTLY — the server's `accept()` blocks on the client's dial.
        let a_is_server = a.public.to_bytes() < b.public.to_bytes();
        let (car_a, car_b) = tokio::join!(
            Carrier::quic_relay(conn_a, addr_b, a_is_server, 1312, Duration::from_secs(10)),
            Carrier::quic_relay(conn_b, addr_a, !a_is_server, 1312, Duration::from_secs(10)),
        );
        let car_a = car_a.expect("A: QUIC-over-relay carrier");
        let car_b = car_b.expect("B: QUIC-over-relay carrier");

        let (mut dev_a, mut rx_a) = WgDevice::new(a.secret.clone());
        let (mut dev_b, mut rx_b) = WgDevice::new(b.secret.clone());

        dev_a.add_peer(b.public.to_bytes(), IP_B, car_a, a_is_server);
        dev_b.add_peer(a.public.to_bytes(), IP_A, car_b, !a_is_server);

        wait_connected(&dev_a, &b.public.to_bytes()).await;

        // A → B
        let pkt_ab = synthetic_ipv4(IP_A, IP_B, b"hello-over-quic-relay");
        send_until_ok(&dev_a, &b.public.to_bytes(), &pkt_ab).await;
        let got_b = tokio::time::timeout(Duration::from_secs(15), rx_b.recv())
            .await
            .expect("B did not receive a decrypted packet over QUIC")
            .expect("tun channel closed");
        assert_eq!(got_b, pkt_ab);

        // B → A (bidirectional over the same QUIC datagram carrier)
        let pkt_ba = synthetic_ipv4(IP_B, IP_A, b"reply-over-quic-relay");
        send_until_ok(&dev_b, &a.public.to_bytes(), &pkt_ba).await;
        let got_a = tokio::time::timeout(Duration::from_secs(15), rx_a.recv())
            .await
            .expect("A did not receive a decrypted packet over QUIC")
            .expect("tun channel closed");
        assert_eq!(got_a, pkt_ba);
    }

    /// A `RelayConn` that models **coturn's PERMISSION rule**: an allocation
    /// only receives datagrams from a peer **IP** it has previously SENT to
    /// (coturn installs a permission on send, then drops inbound from any peer
    /// it has no permission for). A plain [`UdpRelayConn`] can't reproduce the
    /// cross-NAT relay deadlock because it has no such gate — this can.
    ///
    /// **Permissions are IP-only** (RFC 8656 §9 — the port is ignored; verified
    /// in webrtc-rs `turn` client `permission.rs` + server `allocation/mod.rs`).
    /// This is load-bearing for Phase D single-relay: a symmetric-NAT dialer's
    /// source PORT varies per destination, but the anchor's permission (keyed by
    /// the dialer's IP) still matches — so a permit installed by sending to one
    /// port accepts inbound from the SAME IP on a DIFFERENT port. Keying by full
    /// `SocketAddr` (the pre-fix bug) would wrongly drop it.
    struct PermissionedRelayConn {
        sock: UdpSocket,
        permitted: std::sync::Mutex<std::collections::HashSet<std::net::IpAddr>>,
    }
    impl PermissionedRelayConn {
        fn new(sock: UdpSocket) -> Self {
            Self {
                sock,
                permitted: std::sync::Mutex::new(std::collections::HashSet::new()),
            }
        }
    }
    #[async_trait::async_trait]
    impl RelayConn for PermissionedRelayConn {
        async fn send_to(&self, buf: &[u8], dst: std::net::SocketAddr) -> std::io::Result<usize> {
            // Sending to a peer opens (permits) its IP — exactly coturn's
            // CreatePermission-on-send behaviour (IP-only, port ignored).
            self.permitted.lock().unwrap().insert(dst.ip());
            self.sock.send_to(buf, dst).await
        }
        async fn recv_from(
            &self,
            buf: &mut [u8],
        ) -> std::io::Result<(usize, std::net::SocketAddr)> {
            loop {
                let (n, src) = self.sock.recv_from(buf).await?;
                if self.permitted.lock().unwrap().contains(&src.ip()) {
                    return Ok((n, src));
                }
                // Unpermitted peer IP → coturn drops it. Keep waiting.
            }
        }
        fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
            self.sock.local_addr()
        }
    }

    /// rc.199 regression — reproduces the cross-NAT relay deadlock and proves
    /// the mutual permission bootstrap fixes it. With coturn's permission model
    /// (`PermissionedRelayConn`) and a SINGLE WG initiator (the relay's rule),
    /// the passive responder never sends first → never permits the initiator →
    /// coturn drops the INIT → `HANDSHAKE(REKEY_TIMEOUT)` forever. `install_ready`
    /// now sends a stray `\x00` from BOTH ends before the handshake; here we
    /// prove (1) it deadlocks WITHOUT the bootstrap and (2) completes WITH it.
    #[tokio::test(flavor = "multi_thread")]
    async fn relay_wg_deadlocks_without_permission_bootstrap_and_works_with_it() {
        // ── (1) WITHOUT the bootstrap: single initiator deadlocks. ──
        {
            let a = WgKeypair::generate();
            let b = WgKeypair::generate();
            let sock_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let sock_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr_a = sock_a.local_addr().unwrap();
            let addr_b = sock_b.local_addr().unwrap();
            let conn_a: Arc<dyn RelayConn> = Arc::new(PermissionedRelayConn::new(sock_a));
            let conn_b: Arc<dyn RelayConn> = Arc::new(PermissionedRelayConn::new(sock_b));
            let a_init = a.public.to_bytes() < b.public.to_bytes();
            let (mut dev_a, _rx_a) = WgDevice::new(a.secret.clone());
            let (mut dev_b, _rx_b) = WgDevice::new(b.secret.clone());
            // Single initiator (the smaller pubkey), exactly like install_ready.
            dev_a.add_peer(
                b.public.to_bytes(),
                IP_B,
                Carrier::relay(conn_a, addr_b),
                a_init,
            );
            dev_b.add_peer(
                a.public.to_bytes(),
                IP_A,
                Carrier::relay(conn_b, addr_a),
                !a_init,
            );
            let initiator = if a_init { &dev_a } else { &dev_b };
            let peer = if a_init {
                b.public.to_bytes()
            } else {
                a.public.to_bytes()
            };
            // Must NOT connect: the responder's allocation never permits the
            // initiator, so coturn drops every INIT.
            let connected =
                tokio::time::timeout(Duration::from_secs(3), wait_connected(initiator, &peer))
                    .await
                    .is_ok();
            assert!(
                !connected,
                "single-initiator relay must DEADLOCK without the permission bootstrap"
            );
        }
        // ── (2) WITH the mutual bootstrap: the handshake completes. ──
        {
            let a = WgKeypair::generate();
            let b = WgKeypair::generate();
            let sock_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let sock_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr_a = sock_a.local_addr().unwrap();
            let addr_b = sock_b.local_addr().unwrap();
            let conn_a: Arc<dyn RelayConn> = Arc::new(PermissionedRelayConn::new(sock_a));
            let conn_b: Arc<dyn RelayConn> = Arc::new(PermissionedRelayConn::new(sock_b));
            // The rc.199 fix: BOTH ends bootstrap their coturn permission before
            // the handshake (what install_ready now does for every relay carrier).
            conn_a.send_to(b"\x00", addr_b).await.unwrap();
            conn_b.send_to(b"\x00", addr_a).await.unwrap();
            let a_init = a.public.to_bytes() < b.public.to_bytes();
            let (mut dev_a, _rx_a) = WgDevice::new(a.secret.clone());
            let (mut dev_b, mut rx_b) = WgDevice::new(b.secret.clone());
            // Register both, single initiator (exactly install_ready's rule).
            dev_a.add_peer(
                b.public.to_bytes(),
                IP_B,
                Carrier::relay(conn_a, addr_b),
                a_init,
            );
            dev_b.add_peer(
                a.public.to_bytes(),
                IP_A,
                Carrier::relay(conn_b, addr_a),
                !a_init,
            );
            // A → B completes now that both permissions are open.
            wait_connected(&dev_a, &b.public.to_bytes()).await;
            let pkt = synthetic_ipv4(IP_A, IP_B, b"relay-after-bootstrap");
            send_until_ok(&dev_a, &b.public.to_bytes(), &pkt).await;
            let got = tokio::time::timeout(Duration::from_secs(15), rx_b.recv())
                .await
                .expect("B did not receive a decrypted packet after the bootstrap")
                .expect("tun channel closed");
            assert_eq!(
                got, pkt,
                "handshake + data must flow once both ends bootstrap"
            );
        }
    }

    /// Phase D single-relay — WG completes over ONE coturn allocation with a RAW
    /// dialer whose source PORT differs from the port the anchor permitted (the
    /// symmetric-NAT-safe path). The ANCHOR (smaller pubkey) is the QUIC server
    /// over a `PermissionedRelayConn` (its "allocation"), permitting only the
    /// dialer's IP via a send to a DIFFERENT port (9); the DIALER (larger pubkey)
    /// is the QUIC client over a plain `UdpRelayConn` (no allocation) dialing the
    /// anchor's relayed addr. IP-only permission ⇒ the dialer's real-port traffic
    /// is accepted and WG handshakes + data flows both ways — no both-allocate
    /// hairpin, one permission. This is the wg-level guard for V-D1 (which proved
    /// the same live over coturn).
    #[tokio::test(flavor = "multi_thread")]
    async fn wg_single_relay_symmetric_over_quic() {
        use crate::transport::relay::UdpRelayConn;

        // Anchor = smaller pubkey, dialer = larger (the single-relay role rule,
        // which == install_ready's existing am_server/initiate rule, so v1 needs
        // no role decoupling).
        let (anchor, dialer) = {
            let k1 = WgKeypair::generate();
            let k2 = WgKeypair::generate();
            if k1.public.to_bytes() < k2.public.to_bytes() {
                (k1, k2)
            } else {
                (k2, k1)
            }
        };

        let sock_anchor = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sock_dialer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let r_anchor = sock_anchor.local_addr().unwrap(); // relayed addr the dialer dials
        let addr_dialer = sock_dialer.local_addr().unwrap();

        // Anchor's "allocation" enforces IP-only permissions; the dialer is a
        // plain raw socket (no allocation).
        let conn_anchor: Arc<dyn RelayConn> = Arc::new(PermissionedRelayConn::new(sock_anchor));
        let conn_dialer: Arc<dyn RelayConn> = Arc::new(UdpRelayConn(sock_dialer));

        // The anchor permits the dialer's IP by sending its \x00 bootstrap to a
        // DIFFERENT port than the dialer's real one (a live dummy socket, so no
        // ICMP-unreachable poisons the anchor's recv on Windows) — modelling a
        // symmetric NAT where coturn observes a port the advertised srflx didn't
        // name. IP-only permission ⇒ the dialer's real-port traffic is still
        // accepted (permit port ≠ accept port, same IP).
        let _permit_dummy = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let permit_target = _permit_dummy.local_addr().unwrap();
        assert_ne!(
            permit_target.port(),
            addr_dialer.port(),
            "permit port must differ from the dialer's real port to prove IP-only"
        );

        // Anchor = QUIC server + WG initiator; dialer = QUIC client + responder.
        // Build concurrently — the server's accept() blocks on the client's dial.
        let (car_anchor, car_dialer) = tokio::join!(
            Carrier::quic_relay(
                conn_anchor,
                permit_target,
                true,
                1312,
                Duration::from_secs(10)
            ),
            Carrier::quic_relay(conn_dialer, r_anchor, false, 1312, Duration::from_secs(10)),
        );
        let car_anchor = car_anchor.expect("anchor: QUIC-over-single-relay carrier");
        let car_dialer = car_dialer.expect("dialer: QUIC-over-single-relay carrier");

        let (mut dev_anchor, mut rx_anchor) = WgDevice::new(anchor.secret.clone());
        let (mut dev_dialer, mut rx_dialer) = WgDevice::new(dialer.secret.clone());

        dev_anchor.add_peer(dialer.public.to_bytes(), IP_B, car_anchor, true);
        dev_dialer.add_peer(anchor.public.to_bytes(), IP_A, car_dialer, false);

        wait_connected(&dev_anchor, &dialer.public.to_bytes()).await;

        // anchor → dialer
        let pkt_ad = synthetic_ipv4(IP_A, IP_B, b"hello-single-relay");
        send_until_ok(&dev_anchor, &dialer.public.to_bytes(), &pkt_ad).await;
        let got_d = tokio::time::timeout(Duration::from_secs(15), rx_dialer.recv())
            .await
            .expect("dialer received no decrypted packet")
            .expect("tun channel closed");
        assert_eq!(got_d, pkt_ad);

        // dialer → anchor (bidirectional over the single-relay carrier)
        let pkt_da = synthetic_ipv4(IP_B, IP_A, b"reply-single-relay");
        send_until_ok(&dev_dialer, &anchor.public.to_bytes(), &pkt_da).await;
        let got_a = tokio::time::timeout(Duration::from_secs(15), rx_anchor.recv())
            .await
            .expect("anchor received no decrypted packet")
            .expect("tun channel closed");
        assert_eq!(got_a, pkt_da);
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
            workers: vec![],
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

    /// ROOT-CAUSE DIAG for the both-allocate `REKEY_TIMEOUT`: full WG over TWO
    /// allocations pinned to the SAME worker (`ROOMLER_TEST_TURN_WORKER`). The
    /// raw relay-to-relay hairpin already flows (see relay.rs
    /// `relay_to_relay_hairpin_against_real_coturn`), so if THIS passes the
    /// both-allocate CARRIER is sound when both allocations co-locate — meaning
    /// the field REKEY is the two ends landing on DIFFERENT workers (worker-pin
    /// miss / cross-worker drop), not a carrier bug. If it REKEYs even pinned,
    /// there's a WG-layer both-allocate bug the single-allocation single-relay
    /// path sidesteps. Set HOST/SECRET + WORKER (off-host).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "hits live coturn; set ROOMLER_TEST_TURN_HOST/SECRET + ROOMLER_TEST_TURN_WORKER"]
    async fn wg_both_allocate_pinned_against_real_coturn() {
        let Some(worker) = std::env::var("ROOMLER_TEST_TURN_WORKER").ok() else {
            eprintln!("SKIP wg_both_allocate_pinned: ROOMLER_TEST_TURN_WORKER unset");
            return;
        };
        let Some((urls, user, cred)) =
            live_coturn_creds(|_h| vec![format!("turn:{worker}:3478?transport=udp")])
        else {
            eprintln!("SKIP wg_both_allocate_pinned: env unset");
            return;
        };
        eprintln!("wg_both_allocate_pinned: both allocations pinned to {worker}");
        wg_over_two_live_allocations(&urls, &user, &cred).await;
        eprintln!("wg_both_allocate_pinned: WG round-tripped over two SAME-worker allocations OK");
    }

    /// Phase D single-relay LIVE — WG completes over ONE real coturn allocation
    /// (the anchor) + a RAW-UDP dialer with NO allocation: the production
    /// single-relay carrier, end to end over prod coturn. The anchor allocates on
    /// a REMOTE worker (`ROOMLER_TEST_TURN_WORKER` — a worker that is NOT a local
    /// IP of this host, so the dialer's packet crosses the real network + coturn's
    /// PREROUTING DNAT rather than hair-pinning on loopback, the same-host artifact
    /// V-D1 hit). The dialer STUN-discovers its own public srflx so the anchor can
    /// install the IP-only coturn permission; both ends build QUIC-over-relay
    /// carriers (anchor server, dialer client) and WG handshakes + round-trips a
    /// packet BOTH ways. This is V-D1 (the raw-dialer QUIC round-trip, proven live
    /// 2026-07-20) PLUS the WG layer — exactly the leg the both-allocate relay
    /// carrier deadlocked on (`HANDSHAKE(REKEY_TIMEOUT)`), now over a single
    /// allocation with one IP-only permission.
    ///
    /// Run on a host with UDP/3478 to the worker (e.g. mars → jupiter's worker):
    ///   ROOMLER_TEST_TURN_HOST=coturn.roomler.ai \
    ///   ROOMLER_TEST_TURN_SECRET=<coturn static-auth-secret> \
    ///   ROOMLER_TEST_TURN_WORKER=5.9.157.221 \
    ///   cargo test -p roomler-ai-tunnel-core --features overlay-l3 \
    ///     --lib wg_single_relay_against_real_coturn_udp -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "hits live coturn; set ROOMLER_TEST_TURN_HOST/SECRET + ROOMLER_TEST_TURN_WORKER"]
    async fn wg_single_relay_against_real_coturn_udp() {
        use crate::transport::relay::{UdpRelayConn, allocate_relay_from_ice};
        use crate::transport::stun::srflx_query;

        let Some(worker) = std::env::var("ROOMLER_TEST_TURN_WORKER").ok() else {
            eprintln!(
                "SKIP wg_single_relay_against_real_coturn_udp: ROOMLER_TEST_TURN_WORKER unset"
            );
            return;
        };
        // Creds are host-independent HMAC; pin the URL to the remote worker.
        let Some((urls, user, cred)) =
            live_coturn_creds(|_h| vec![format!("turn:{worker}:3478?transport=udp")])
        else {
            eprintln!("SKIP wg_single_relay_against_real_coturn_udp: TURN env unset");
            return;
        };
        let worker_ip: std::net::IpAddr =
            worker.parse().expect("ROOMLER_TEST_TURN_WORKER is an IP");
        let stun_server = SocketAddr::new(worker_ip, 3478);

        // Anchor = smaller pubkey (QUIC server + WG initiator, install_ready's
        // rule); dialer = larger (QUIC client + WG responder).
        let (anchor, dialer) = {
            let k1 = WgKeypair::generate();
            let k2 = WgKeypair::generate();
            if k1.public.to_bytes() < k2.public.to_bytes() {
                (k1, k2)
            } else {
                (k2, k1)
            }
        };

        // Anchor: the SOLE real coturn allocation, on the remote worker.
        let alloc = allocate_relay_from_ice(&urls, &user, &cred)
            .await
            .expect("anchor: live coturn allocation");
        let r_anchor = alloc.local_addr().unwrap();
        eprintln!("single-relay-wg: anchor R={r_anchor}");

        // Dialer: a raw socket (NO allocation). Discover its public srflx so the
        // anchor can permit its IP (IP-only); the QUIC dial reuses this same
        // socket, so coturn observes the same source.
        let dialer_sock = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        let dialer_srflx = srflx_query(&dialer_sock, stun_server, Duration::from_secs(3))
            .await
            .expect("dialer: srflx via STUN on the worker");
        eprintln!("single-relay-wg: dialer srflx={dialer_srflx}");

        let conn_anchor: Arc<dyn RelayConn> = Arc::new(alloc);
        let conn_dialer: Arc<dyn RelayConn> = Arc::new(UdpRelayConn(dialer_sock));

        // Build concurrently (the server's accept() blocks on the client dial).
        // The anchor's quic_relay sends its own `\x00` to dialer_srflx →
        // installs the IP-only permission before the handshake; the dialer's
        // sends `\x00` to R → opens its path + reaches coturn (rc.199 bootstrap).
        // min_datagram = 1312 = OVERLAY_MTU(1280) + WG_OVERHEAD(32) — the exact
        // budget install_ready demands, so a pass here proves the live coturn
        // QUIC datagram budget suffices for the production carrier.
        let (car_anchor, car_dialer) = tokio::join!(
            Carrier::quic_relay(
                conn_anchor,
                dialer_srflx,
                true,
                1312,
                Duration::from_secs(15)
            ),
            Carrier::quic_relay(conn_dialer, r_anchor, false, 1312, Duration::from_secs(15)),
        );
        let car_anchor = car_anchor.expect("anchor: single-relay QUIC carrier over live coturn");
        let car_dialer = car_dialer.expect("dialer: single-relay QUIC carrier over live coturn");

        let (mut dev_anchor, mut rx_anchor) = WgDevice::new(anchor.secret.clone());
        let (mut dev_dialer, mut rx_dialer) = WgDevice::new(dialer.secret.clone());
        dev_anchor.add_peer(dialer.public.to_bytes(), IP_B, car_anchor, true);
        dev_dialer.add_peer(anchor.public.to_bytes(), IP_A, car_dialer, false);

        wait_connected(&dev_anchor, &dialer.public.to_bytes()).await;

        // anchor → dialer
        let pkt_ad = synthetic_ipv4(IP_A, IP_B, b"hello-single-relay-live");
        send_until_ok(&dev_anchor, &dialer.public.to_bytes(), &pkt_ad).await;
        let got_d = tokio::time::timeout(Duration::from_secs(30), rx_dialer.recv())
            .await
            .expect("dialer received no packet over live coturn")
            .expect("tun channel closed");
        assert_eq!(got_d, pkt_ad);

        // dialer → anchor (bidirectional over the single allocation)
        let pkt_da = synthetic_ipv4(IP_B, IP_A, b"reply-single-relay-live");
        send_until_ok(&dev_dialer, &anchor.public.to_bytes(), &pkt_da).await;
        let got_a = tokio::time::timeout(Duration::from_secs(30), rx_anchor.recv())
            .await
            .expect("anchor received no packet over live coturn")
            .expect("tun channel closed");
        assert_eq!(got_a, pkt_da);
        eprintln!("single-relay-wg: bidirectional WG over ONE live coturn allocation OK");
    }

    // rc.181 — a `RelayConn` whose `send_to` always hard-errors, mirroring a
    // TURNS/TCP `tcp-turn write: connection reset` after a corp middlebox reaps
    // the idle control TCP. `recv_from` parks forever; `local_addr` is a stub.
    struct FailingRelay;
    #[async_trait::async_trait]
    impl RelayConn for FailingRelay {
        async fn send_to(&self, _buf: &[u8], _dst: SocketAddr) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "tcp-turn write: connection reset (os error 10054)",
            ))
        }
        async fn recv_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
            std::future::pending().await
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok("127.0.0.1:0".parse().unwrap())
        }
    }

    /// A `RelayConn` whose `send_to` always succeeds.
    struct OkRelay;
    #[async_trait::async_trait]
    impl RelayConn for OkRelay {
        async fn send_to(&self, buf: &[u8], _dst: SocketAddr) -> io::Result<usize> {
            Ok(buf.len())
        }
        async fn recv_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
            std::future::pending().await
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok("127.0.0.1:0".parse().unwrap())
        }
    }

    #[tokio::test]
    async fn relay_send_error_latches_carrier_dead() {
        let dst: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let carrier = Carrier::relay(Arc::new(FailingRelay), dst);
        assert!(!carrier.is_dead(), "a fresh relay carrier is alive");
        let r = carrier.send(b"wg-datagram").await;
        assert!(r.is_err(), "the mock relay hard-errors on send");
        assert!(
            carrier.is_dead(),
            "a hard send error must latch the carrier dead so the sweep re-allocates on the next tick"
        );
    }

    #[tokio::test]
    async fn healthy_relay_send_keeps_carrier_alive() {
        let dst: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let carrier = Carrier::relay(Arc::new(OkRelay), dst);
        carrier.send(b"wg-datagram").await.expect("send ok");
        assert!(
            !carrier.is_dead(),
            "a successful send must not mark the carrier dead"
        );
    }

    #[tokio::test]
    async fn direct_carrier_is_never_dead() {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let dst: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let carrier = Carrier::direct(sock, dst);
        // A direct (UDP) carrier is never "dead": a failed datagram is dropped,
        // not a session death — so the sweep keeps using the rx-flat heuristic.
        let _ = carrier.send(b"x").await;
        assert!(!carrier.is_dead());
    }

    /// Phase A — `authenticate_init` extracts + AUTHENTICATES an inbound
    /// handshake initiation with no per-peer state: it must accept a genuine
    /// init (yielding the initiator's real key), and reject one sealed to a
    /// DIFFERENT responder (a forger can't have produced it) plus any garbage.
    #[tokio::test(flavor = "multi_thread")]
    async fn authenticate_init_accepts_genuine_rejects_misaddressed_and_garbage() {
        let a = WgKeypair::generate();
        let b = WgKeypair::generate();
        let c = WgKeypair::generate();
        let (dev_b, _rx_b) = WgDevice::new(b.secret.clone());
        let (dev_c, _rx_c) = WgDevice::new(c.secret.clone());

        // A genuine init from A, sealed to B's static public key.
        let mut tunn_ab = Tunn::new(a.secret.clone(), b.public, None, None, 1, None);
        let mut buf = vec![0u8; WG_BUF];
        let init = match tunn_ab.format_handshake_initiation(&mut buf, false) {
            TunnResult::WriteToNetwork(bytes) => bytes.to_vec(),
            _ => panic!("expected a handshake initiation"),
        };

        // B is the intended responder → authenticates it, recovering A's key.
        assert_eq!(dev_b.authenticate_init(&init), Some(a.public.to_bytes()));
        // C is NOT the intended responder → the init's DHs don't resolve under
        // C's secret, so the timestamp AEAD fails ⇒ rejected.
        assert_eq!(dev_c.authenticate_init(&init), None);
        // Garbage (right-length + short) is rejected, never panics.
        assert_eq!(dev_b.authenticate_init(&[0u8; 148]), None);
        assert_eq!(dev_b.authenticate_init(b"short"), None);
    }

    /// Phase A CORE — a CLIENT dials a HUB's endpoint and the HUB, which never
    /// dials back (`initiate=false`), completes the handshake purely by
    /// ACCEPTING the inbound init its demux forwarded as a [`DirectInbound`]
    /// event (the exit-side accept: the HUB can't know a NAT'd client's source
    /// ahead of time). Proves single-initiator direct works + data flows.
    #[tokio::test(flavor = "multi_thread")]
    async fn public_direct_single_initiator_accept_via_inbound_event() {
        let hub = WgKeypair::generate();
        let client = WgKeypair::generate();
        let sock_hub = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let sock_client = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let hub_addr = sock_hub.local_addr().unwrap();
        let client_addr = sock_client.local_addr().unwrap();

        let (mut dev_hub, mut rx_hub) = WgDevice::new(hub.secret.clone());
        let (mut dev_client, _rx_client) = WgDevice::new(client.secret.clone());

        // HUB: start the demux loop eagerly (as the runtime does when the tier
        // is on) and take the inbound-init receiver.
        dev_hub.ensure_direct_demux(sock_hub.clone());
        let mut events = dev_hub.take_direct_events().unwrap();

        // CLIENT dials HUB (initiate=true), keyed by HUB's addr (IP_B = hub).
        dev_client.ensure_direct_demux(sock_client.clone());
        dev_client
            .add_direct_peer(
                sock_client.clone(),
                hub.public.to_bytes(),
                IP_B,
                hub_addr,
                true,
            )
            .await;

        // HUB receives the init as an event from CLIENT's (unregistered) source.
        let inb = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("no inbound-init event")
            .expect("event channel closed");
        assert_eq!(inb.src, client_addr, "event carries the client's source");
        assert_eq!(
            dev_hub.authenticate_init(&inb.packet),
            Some(client.public.to_bytes()),
            "the forwarded init authenticates to the client's key"
        );

        // HUB installs the client direct (initiate=false — it only responds),
        // keyed by the init's source, then feeds the init so the response goes
        // out immediately (IP_A = client).
        dev_hub
            .add_direct_peer(
                inb.sock.clone(),
                client.public.to_bytes(),
                IP_A,
                inb.src,
                false,
            )
            .await;
        dev_hub
            .feed_direct(inb.src, inb.sock.clone(), &inb.packet)
            .await;

        // Both ends complete the handshake single-initiator.
        wait_connected(&dev_client, &hub.public.to_bytes()).await;
        wait_connected(&dev_hub, &client.public.to_bytes()).await;

        // CLIENT → HUB data arrives decrypted.
        let pkt = synthetic_ipv4(IP_A, IP_B, b"hello-public-direct");
        send_until_ok(&dev_client, &hub.public.to_bytes(), &pkt).await;
        let got = tokio::time::timeout(Duration::from_secs(15), rx_hub.recv())
            .await
            .expect("HUB never received the decrypted packet")
            .expect("tun channel closed");
        assert_eq!(got, pkt, "decrypted IP packet must arrive intact");
    }

    /// Phase C — the demux discriminates STUN Binding messages (→ the STUN
    /// sink, for the srflx keepalive) from WireGuard traffic (unknown-source
    /// inits → `direct_events`; everything else dropped), and a WG datagram
    /// whose receiver-index bytes collide with the STUN magic cookie is NOT
    /// mis-forwarded to the sink.
    #[tokio::test(flavor = "multi_thread")]
    async fn demux_routes_stun_to_sink_and_wg_to_events() {
        let hub = WgKeypair::generate();
        let client = WgKeypair::generate();
        let sock_hub = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let hub_addr = sock_hub.local_addr().unwrap();

        let (mut dev_hub, _rx_hub) = WgDevice::new(hub.secret.clone());
        dev_hub.ensure_direct_demux(sock_hub.clone());
        let mut direct_events = dev_hub.take_direct_events().unwrap();
        let mut stun_events = dev_hub.take_stun_events().unwrap();

        // An external socket standing in for the STUN server / a peer.
        let ext = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ext_addr = ext.local_addr().unwrap();

        // (1) A STUN Binding message → the STUN sink, carrying its source.
        let stun_msg = crate::transport::stun::encode_binding_request([7u8; 12]);
        ext.send_to(&stun_msg, hub_addr).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), stun_events.recv())
            .await
            .expect("STUN datagram never reached the sink")
            .expect("stun sink closed");
        assert_eq!(got.src, ext_addr, "sink item carries the STUN source");
        assert_eq!(
            got.packet,
            stun_msg.to_vec(),
            "sink item is the STUN datagram"
        );
        assert!(
            direct_events.try_recv().is_err(),
            "a STUN datagram must NOT reach direct_events"
        );

        // (2) A WG-shaped datagram whose index bytes equal the STUN cookie is
        // kept as WG (dropped here — unknown source, not an init), NOT sent to
        // the sink. Follow it with a sentinel STUN; FIFO per socket guarantees
        // the lookalike was handled first, so receiving the sentinel proves the
        // lookalike went nowhere.
        let mut wg_lookalike = vec![0u8; 48];
        wg_lookalike[0] = 4; // Data type (LE 4-byte type → bytes 1..4 == 0)
        wg_lookalike[4..8].copy_from_slice(&0x2112_A442u32.to_be_bytes()); // STUN cookie
        ext.send_to(&wg_lookalike, hub_addr).await.unwrap();
        let sentinel = crate::transport::stun::encode_binding_request([9u8; 12]);
        ext.send_to(&sentinel, hub_addr).await.unwrap();
        let got2 = tokio::time::timeout(Duration::from_secs(5), stun_events.recv())
            .await
            .expect("sentinel STUN never reached the sink")
            .expect("stun sink closed");
        assert_eq!(
            got2.packet,
            sentinel.to_vec(),
            "sink got the sentinel, not the WG-cookie-lookalike"
        );
        assert!(
            direct_events.try_recv().is_err(),
            "a WG-shaped datagram must NOT reach direct_events from an unknown source"
        );

        // (3) A genuine unknown-source WG handshake init → direct_events, and
        // NOT the STUN sink.
        let (mut dev_client, _rx_client) = WgDevice::new(client.secret.clone());
        let sock_client = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        dev_client.ensure_direct_demux(sock_client.clone());
        dev_client
            .add_direct_peer(
                sock_client.clone(),
                hub.public.to_bytes(),
                IP_B,
                hub_addr,
                true,
            )
            .await;
        let inb = tokio::time::timeout(Duration::from_secs(5), direct_events.recv())
            .await
            .expect("WG init never reached direct_events")
            .expect("direct_events closed");
        assert_eq!(
            dev_hub.authenticate_init(&inb.packet),
            Some(client.public.to_bytes()),
            "the forwarded init authenticates to the client's key"
        );
        assert!(
            stun_events.try_recv().is_err(),
            "a WG init must NOT reach the STUN sink"
        );
    }
}

//! DERP client carrier — a pubkey-addressed relay for the both-UDP-blocked
//! overlay tier (NAT-traversal Phase D).
//!
//! Two nodes BOTH on all-UDP-blocked networks (a strict corp firewall that
//! permits only TCP/TLS-443) can't use single-relay — exactly one side must be
//! the raw-UDP dialer and neither has UDP. DERP breaks it: both peers dial OUT
//! to a rendezvous relay ([`crate::ws::derp`] on the server, addressed by WG
//! pubkey), so no UDP, no inbound, no TURN permission model.
//!
//! # Two pieces
//!
//! - [`DerpConn`] — a [`RelayConn`] PINNED to one peer pubkey. `send_to` frames
//!   `[peer_pubkey || payload]`; `recv_from` yields that peer's demuxed
//!   payloads. Because it is pubkey-pinned, EVERY received datagram is from the
//!   one peer, so the [`Carrier::Relay`](crate::overlay::wg::Carrier)
//!   recv-source discard is always correct — this is exactly why RAW WG rides
//!   DERP (unlike single-relay, which needed QUIC to recover the observed
//!   source under symmetric NAT).
//! - [`DerpMux`] — the per-node demux + fan-out. ONE per node: it owns the
//!   shared outbound queue and the `src_pubkey → DerpConn` inbound registry,
//!   and vends a [`DerpConn`] per peer. It is transport-agnostic (pure
//!   channels) — the owner (the agent's WS task, DERP-3) drains
//!   [`DerpMux::outbound`] into the `/derp` WSS and feeds inbound WS frames to
//!   [`DerpMux::deliver`]. That keeps `tunnel-core` free of a WebSocket
//!   dependency and makes the whole thing unit-testable without a socket.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;

use crate::transport::relay::RelayConn;

/// 32-byte WireGuard public key — the DERP addressing unit.
pub type DerpPubKey = [u8; 32];

/// Depth of a node's shared outbound WS queue (frames waiting to hit the wire).
/// Bounded so a stalled WS can't grow memory without bound; overflow drops the
/// frame (WG/QUIC are loss-tolerant — a dropped carrier datagram retransmits).
const OUTBOUND_QUEUE: usize = 512;

/// Depth of a single peer's inbound payload queue. Same drop-on-overflow.
const INBOUND_QUEUE: usize = 256;

/// #27 — depth of the mux→runtime event queue. Tiny on purpose: the consumer
/// coalesces per peer anyway, and [`MuxEvent::Unrouted`] is level-triggered in
/// practice (a demoted peer keeps sending until we follow it), so dropping a
/// notice under burst costs at most one more inbound frame's worth of delay,
/// never the signal itself.
const EVENT_QUEUE: usize = 32;

/// #27/#28 — something happened on this mux that the RUNTIME must react to.
/// One channel rather than one per signal, so a mux acquired later (the
/// coordinator arms each as it arrives) can never be half-wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxEvent {
    /// #27 — a peer is relaying to us over DERP and we hold no [`DerpConn`] for
    /// it, which means that peer has demoted and we have not followed. Carries
    /// the RELAY-STAMPED source pubkey (see [`DerpMux::deliver`]).
    Unrouted(DerpPubKey),
    /// #28 — the `/derp` WS reconnected and re-registered after an outage.
    /// EDGE-triggered (a `mark_up` that changes nothing emits nothing), so the
    /// startup `mark_up` on a mux that was never down is silent.
    ///
    /// Worth waking the runtime for because every DERP build made while the WS
    /// was down was WITHHELD (`try_build_derp` refuses over a dead WS — a
    /// carrier born there convicts one-way and rebuilds forever), and those
    /// peers otherwise wait for the next establish walk. Field 2026-08-24: the
    /// WS was back in 1.5 s and the floor still took **5 s** more, because the
    /// walk runs on the 5 s tick.
    Recovered,
}

/// A [`RelayConn`] over a DERP relay, PINNED to one peer pubkey.
///
/// `send_to` ignores its `SocketAddr` argument (DERP is pubkey-addressed, not
/// IP-addressed) and frames `[peer_pubkey || payload]` onto the node's shared
/// outbound queue. `recv_from` returns this peer's next demuxed payload tagged
/// with a stable synthetic source address — the carrier only needs a
/// CONSISTENT remote, not a routable one.
pub struct DerpConn {
    peer_pubkey: DerpPubKey,
    /// The node's shared WS write queue (cloned from the [`DerpMux`]).
    ws_out: mpsc::Sender<Vec<u8>>,
    /// This peer's demuxed inbound payloads (the mux routes frames whose
    /// `src_pubkey == peer_pubkey` here).
    inbound: AsyncMutex<mpsc::Receiver<Vec<u8>>>,
    /// The node WS's liveness. When the WS is down, `send_to` returns `Err` so
    /// the [`Carrier::Relay`](crate::overlay::wg::Carrier) `dead` latch fires
    /// and the health sweep rebuilds — never silently queue onto a dead WS.
    alive: Arc<AtomicBool>,
    /// Stable synthetic addresses derived from the pubkeys. Same-family (v4),
    /// nonzero port, unique per peer — cosmetic for the raw carrier (the `dst`
    /// is discarded), but keeps a future QUIC-over-DERP path valid (quinn
    /// rejects a family-mismatched or zero-port remote).
    synth_local: SocketAddr,
    synth_peer: SocketAddr,
    /// #32 — the mux's route table, WHICH slot this conn owns, and OUR sender
    /// in it, so [`Drop`] can retire exactly our own route and never a newer
    /// one (nor the other consumer's).
    peers: PeerTable,
    route: RouteKind,
    self_tx: mpsc::Sender<Vec<u8>>,
}

/// #32 — which of a peer's two inbound routes a [`DerpConn`] owns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RouteKind {
    /// The WG/carrier consumer ([`DerpMux::conn_for`]).
    Wg,
    /// R4's tunnel consumer ([`DerpMux::tunnel_conn_for`]).
    Tunnel,
}

/// #32 — a route must not outlive the conn that consumes it.
///
/// The table was written and never cleaned: the vend methods insert, "last one
/// wins" replaces, and nothing ever removes. That is safe only while a dropped
/// `DerpConn` also closes its channel — and `deliver` only *reports* a miss
/// when `try_send` FAILS, so a route left pointing at a channel that is somehow
/// still open would swallow frames in a queue nobody drains, silently. Making
/// the route's lifetime the consumer's lifetime is the invariant `deliver`
/// already assumes; nothing enforced it.
///
/// Same class as "an `RTCPeerConnection` must be `close()`d": bookkeeping
/// outliving its owner.
impl Drop for DerpConn {
    fn drop(&mut self) {
        let mut peers = self.peers.lock().unwrap();
        let Some(routes) = peers.get_mut(&self.peer_pubkey) else {
            return;
        };
        // ⚠️ Only OUR slot, and only if it is STILL ours. A rebuild registers
        // the new conn BEFORE the old one drops ("last one wins"), so clearing
        // blindly would unregister the LIVE consumer — turning a hypothetical
        // silent drop into a guaranteed one. And the two slots are independent
        // consumers: a WG carrier going away must not take R4's tunnel route
        // with it.
        let slot = match self.route {
            RouteKind::Wg => &mut routes.wg,
            RouteKind::Tunnel => &mut routes.tunnel,
        };
        if slot
            .as_ref()
            .is_some_and(|tx| tx.same_channel(&self.self_tx))
        {
            *slot = None;
        }
        if routes.wg.is_none() && routes.tunnel.is_none() {
            peers.remove(&self.peer_pubkey);
        }
    }
}

/// #32 — the route table, shared with every [`DerpConn`] so a conn can retire
/// its own route on drop (see that `Drop` impl).
type PeerTable = Arc<Mutex<HashMap<DerpPubKey, PeerRoutes>>>;

/// One peer's inbound routes on the mux — the WG/carrier consumer (today's
/// [`DerpMux::conn_for`]) and, since R4, an optional TUNNEL consumer
/// ([`DerpMux::tunnel_conn_for`]) carrying QUIC for the tunnel's
/// `quic-derp-v1` flavor over the SAME established `/derp` WS. Split per
/// frame by [`classify_payload`]; with no tunnel consumer registered the
/// behavior is byte-identical to the pre-R4 single-route mux.
#[derive(Default)]
struct PeerRoutes {
    wg: Option<mpsc::Sender<Vec<u8>>>,
    tunnel: Option<mpsc::Sender<Vec<u8>>>,
}

/// Local copy of the disco magic (`overlay::disco::MAGIC`) — `transport` must
/// compile without the `overlay` feature, so it cannot reference the module;
/// a feature-gated test asserts the two never drift.
const DISCO_MAGIC: &[u8; 8] = b"RMDISCO1";

/// Which consumer an inbound DERP payload belongs to. WireGuard frames start
/// with their LE u32 message type — first byte 1..=4 — and disco frames with
/// the 8-byte `RMDISCO1` magic (designed disjoint from WG, doc'd in
/// `overlay::disco`); both belong to the carrier/WG consumer. EVERYTHING else
/// (QUIC long headers ≥0xC0, short headers 0x40..=0x7F, version-negotiation
/// 0x80..=0xBF) is tunnel traffic — but only when a tunnel consumer is
/// registered; otherwise it falls through to the WG consumer exactly as
/// before R4 (boringtun discards non-WG bytes — harmless, and preserves the
/// legacy path for anything unexpected). A QUIC short-header packet CAN start
/// with 0x52 ('R'), which is why the disco check is the full 8-byte magic and
/// runs first.
pub fn payload_is_wg_or_disco(payload: &[u8]) -> bool {
    matches!(payload.first(), Some(1..=4)) || payload.len() >= 8 && &payload[..8] == DISCO_MAGIC
}

/// R4 — everything the tunnel driver needs to run the `quic-derp-v1` flavor:
/// the node's established DERP mux + its own pubkey (hex, as it travels on
/// the wire). Built by the daemon (an overlay node); the standalone CLI has
/// no overlay identity and passes `None`, keeping the classic ladder.
#[derive(Clone)]
pub struct DerpTunnelHandle {
    pub mux: Arc<DerpMux>,
    pub self_pubkey_hex: String,
}

/// Parse a 64-char lowercase-hex DERP/WG pubkey off the wire. `None` on any
/// malformation — the caller falls back rather than panicking on peer input.
pub fn parse_pubkey_hex(s: &str) -> Option<DerpPubKey> {
    let bytes = hex::decode(s.trim()).ok()?;
    bytes.try_into().ok()
}

/// Hex-encode a DERP pubkey for the wire.
pub fn pubkey_hex(pk: &DerpPubKey) -> String {
    hex::encode(pk)
}

/// A stable, non-routable synthetic `SocketAddr` derived from a pubkey:
/// `127.<pk0>.<pk1>.<pk2|1>:<pk3pk4 | 0x8000>`. Deterministic + unique enough
/// per peer; only used as a carrier "remote" placeholder.
fn synth_addr(pk: &DerpPubKey) -> SocketAddr {
    let ip = Ipv4Addr::new(127, pk[0], pk[1], pk[2].max(1));
    let port = u16::from_be_bytes([pk[3], pk[4]]) | 0x8000;
    SocketAddr::new(IpAddr::V4(ip), port)
}

impl DerpConn {
    /// This conn's stable synthetic PEER address — the placeholder `dst` a
    /// [`Carrier::relay`](crate::overlay::wg::Carrier) is built with. The DERP
    /// carrier discards it on recv (pubkey-pinned), so it only needs to be
    /// consistent and valid.
    pub fn synth_peer(&self) -> SocketAddr {
        self.synth_peer
    }
}

#[async_trait]
impl RelayConn for DerpConn {
    // DERP is always the server's `/derp` WebSocket, i.e. TCP — which is why
    // it reads slower than a coturn UDP hop and why the two must not both
    // print a bare "relay". The mux owns the URL, not the per-peer conn, so
    // `relay_server` stays `None` here rather than guessing.
    fn relay_transport(&self) -> crate::transport::relay::RelayTransport {
        crate::transport::relay::RelayTransport::Tcp
    }

    async fn send_to(&self, buf: &[u8], _dst: SocketAddr) -> io::Result<usize> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "derp ws down",
            ));
        }
        let mut frame = Vec::with_capacity(32 + buf.len());
        frame.extend_from_slice(&self.peer_pubkey);
        frame.extend_from_slice(buf);
        match self.ws_out.try_send(frame) {
            Ok(()) => Ok(buf.len()),
            // Backpressure: drop this datagram (loss-tolerant carrier). NOT an
            // error — a full transient queue must not latch the carrier dead.
            Err(mpsc::error::TrySendError::Full(_)) => Ok(buf.len()),
            // The WS write task is gone → the carrier IS dead. Return `Err` so
            // the `dead` latch fires and the sweep rebuilds.
            Err(mpsc::error::TrySendError::Closed(_)) => Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "derp ws closed",
            )),
        }
    }

    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut rx = self.inbound.lock().await;
        match rx.recv().await {
            Some(payload) => {
                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                Ok((n, self.synth_peer))
            }
            None => Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "derp inbound closed",
            )),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.synth_local)
    }
}

/// The per-node DERP demux + fan-out. Owns the shared outbound queue and the
/// `src_pubkey → inbound` registry; vends a [`DerpConn`] per peer. ONE per node.
///
/// Transport-agnostic: the owner drives the actual WSS by draining the receiver
/// returned from [`DerpMux::new`] into the socket and feeding inbound WS frames
/// to [`DerpMux::deliver`]. On WS loss it calls [`DerpMux::mark_down`]; after a
/// reconnect + re-register, [`DerpMux::mark_up`]. The outbound receiver lives
/// for the mux's whole life (across reconnects), so a reconnect never severs
/// the `DerpConn`→WS path.
pub struct DerpMux {
    self_pubkey: DerpPubKey,
    ws_out: mpsc::Sender<Vec<u8>>,
    alive: Arc<AtomicBool>,
    /// When the CURRENT outage began (`None` while up). Lets the evidence
    /// path distinguish a reconnect blip from a sustained WSS outage — the
    /// U1 healer must not clear force-DERP pins on a flap (Phase A1).
    down_since: Mutex<Option<Instant>>,
    peers: PeerTable,
    /// #27/#28 — where this mux reports [`MuxEvent`]s to the runtime. `None`
    /// until the runtime arms it (tests, and the tunnel-only paths that never
    /// demote, leave it unset).
    events_tx: Mutex<Option<mpsc::Sender<MuxEvent>>>,
    /// Lifetime count of frames dropped by `deliver`, split by cause. Read by
    /// the LocalAPI/diagnostics; the point is that neither drop is silent any
    /// more (they both were — see the `deliver` doc).
    dropped_unrouted: Arc<std::sync::atomic::AtomicU64>,
    dropped_backpressure: Arc<std::sync::atomic::AtomicU64>,
}

impl DerpMux {
    /// Create a mux for a node with `self_pubkey`. Returns the mux and the
    /// outbound frame receiver the WS owner must drain for the mux's lifetime.
    /// Starts `alive = true` (the owner sets it `false`/`true` around a
    /// reconnect).
    pub fn new(self_pubkey: DerpPubKey) -> (Arc<Self>, mpsc::Receiver<Vec<u8>>) {
        let (ws_out, ws_out_rx) = mpsc::channel(OUTBOUND_QUEUE);
        let mux = Arc::new(Self {
            self_pubkey,
            ws_out,
            alive: Arc::new(AtomicBool::new(true)),
            down_since: Mutex::new(None),
            peers: Arc::new(Mutex::new(HashMap::new())),
            events_tx: Mutex::new(None),
            dropped_unrouted: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dropped_backpressure: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        });
        (mux, ws_out_rx)
    }

    /// #27/#28 — install the runtime's [`MuxEvent`] sink. A SETTER rather than
    /// a subscribe-and-return because a node holds several muxes (central + one
    /// per relay region) and the runtime wants ONE channel for all of them; the
    /// coordinator installs this on every mux as it arrives. A second call
    /// replaces the sender, so a runtime rebuild re-points cleanly.
    pub fn set_event_sink(&self, tx: mpsc::Sender<MuxEvent>) {
        *self.events_tx.lock().unwrap() = Some(tx);
    }

    /// The channel depth callers should use for [`Self::set_event_sink`].
    pub const EVENT_SINK_DEPTH: usize = EVENT_QUEUE;

    /// #27 — `(unrouted, backpressure)` inbound-drop counts for this mux,
    /// cumulative for its lifetime.
    pub fn drop_counts(&self) -> (u64, u64) {
        (
            self.dropped_unrouted.load(Ordering::Relaxed),
            self.dropped_backpressure.load(Ordering::Relaxed),
        )
    }

    /// The first frame to send on a fresh `/derp` WS: this node's own pubkey
    /// (the server validates it against the node's `overlay_nodes` row).
    pub fn registration_frame(&self) -> Vec<u8> {
        self.self_pubkey.to_vec()
    }

    /// Vend a [`DerpConn`] pinned to `peer_pubkey`, registering its inbound
    /// route. A later `conn_for` for the same peer (a carrier rebuild) replaces
    /// the route — last one wins, so stale inbound senders never accumulate.
    pub fn conn_for(&self, peer_pubkey: DerpPubKey) -> DerpConn {
        let (in_tx, in_rx) = mpsc::channel(INBOUND_QUEUE);
        self.peers
            .lock()
            .unwrap()
            .entry(peer_pubkey)
            .or_default()
            .wg = Some(in_tx.clone());
        self.build_conn(peer_pubkey, in_rx, RouteKind::Wg, in_tx)
    }

    /// R4 — vend a [`DerpConn`] for the TUNNEL's `quic-derp-v1` flavor toward
    /// `peer_pubkey`, sharing this mux's established `/derp` WS with the WG
    /// carrier for the same peer. Inbound frames are split per packet by
    /// [`payload_is_wg_or_disco`]; outbound framing is identical (the far
    /// end's mux does the same split). Last one wins per peer, like
    /// [`Self::conn_for`] — a new tunnel session replaces the previous
    /// session's route.
    pub fn tunnel_conn_for(&self, peer_pubkey: DerpPubKey) -> DerpConn {
        let (in_tx, in_rx) = mpsc::channel(INBOUND_QUEUE);
        self.peers
            .lock()
            .unwrap()
            .entry(peer_pubkey)
            .or_default()
            .tunnel = Some(in_tx.clone());
        self.build_conn(peer_pubkey, in_rx, RouteKind::Tunnel, in_tx)
    }

    fn build_conn(
        &self,
        peer_pubkey: DerpPubKey,
        in_rx: mpsc::Receiver<Vec<u8>>,
        route: RouteKind,
        self_tx: mpsc::Sender<Vec<u8>>,
    ) -> DerpConn {
        DerpConn {
            peer_pubkey,
            ws_out: self.ws_out.clone(),
            inbound: AsyncMutex::new(in_rx),
            alive: Arc::clone(&self.alive),
            synth_local: synth_addr(&self.self_pubkey),
            synth_peer: synth_addr(&peer_pubkey),
            peers: Arc::clone(&self.peers),
            route,
            self_tx,
        }
    }

    /// This node's own DERP pubkey (the WG identity the mux registered with).
    pub fn self_pubkey(&self) -> DerpPubKey {
        self.self_pubkey
    }

    /// Route one inbound relay frame `[src_pubkey(32) || payload]` to the
    /// [`DerpConn`] registered for `src_pubkey`.
    ///
    /// #27 — the no-conn branch used to be a bare `if let Some(…)` with no
    /// `else`, and the queue-full branch a `let _ = try_send`: **two silent
    /// drops in four lines**, and the first of them was a 100 %-loss black
    /// hole. A peer only ever relays to us once it has DEMOTED to DERP (both
    /// ends prefer direct), so a frame arriving for a peer we hold no
    /// [`DerpConn`] for means *we have not followed it yet* — measured
    /// 2026-08-24 as **69 s** of mutual blackout on a VPN transition, because
    /// the far end had no netstate Major of its own and sat out
    /// `POKE_SILENCE_AFTER` + its tier deadline before demoting independently.
    ///
    /// So the miss is now REPORTED, not dropped in silence: the src pubkey goes
    /// to [`Self::subscribe_unrouted`] and the runtime follows that peer onto
    /// DERP. Trusting it is sound because the relay STAMPS `src_pubkey` from
    /// the sender's authenticated registration — it is not sender-chosen (see
    /// `api::ws::derp::forward_frame`) — so this can only name a node that is
    /// registered in this network and permitted by its ACL to reach us.
    ///
    /// The frame itself is still dropped: we have nowhere to put it, and WG
    /// retransmits. The signal, not the payload, is what matters.
    pub fn deliver(&self, frame: &[u8]) {
        if frame.len() < 32 {
            return;
        }
        let mut src = [0u8; 32];
        src.copy_from_slice(&frame[..32]);
        let payload = &frame[32..];
        // R4 split: a non-WG/non-disco payload goes to the peer's TUNNEL
        // consumer when one is registered. Tunnel drops are counted but
        // never emit `Unrouted` — that event means "peer demoted, follow it
        // onto DERP" and would send the overlay runtime chasing a carrier
        // condition that doesn't exist; QUIC's own loss recovery owns the
        // tunnel side.
        let (wg_tx, tunnel_tx) = {
            let peers = self.peers.lock().unwrap();
            match peers.get(&src) {
                Some(r) => (r.wg.clone(), r.tunnel.clone()),
                None => (None, None),
            }
        };
        // No tunnel consumer ⇒ pre-R4 behavior: everything falls through to
        // the WG route (boringtun discards non-WG bytes; the Unrouted
        // semantics below stay exactly as shipped in #27).
        if !payload_is_wg_or_disco(payload)
            && let Some(tx) = tunnel_tx
        {
            if tx.try_send(payload.to_vec()).is_err() {
                self.dropped_backpressure.fetch_add(1, Ordering::Relaxed);
                crate::evidence::DERP_INBOUND_BACKPRESSURE.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        match wg_tx {
            Some(tx) => {
                if tx.try_send(payload.to_vec()).is_err() {
                    // Full (the peer's carrier is not draining) or closed (the
                    // `DerpConn` was dropped when a better tier replaced this
                    // carrier — the registration outlives it by design, "last
                    // one wins"). A CLOSED channel is the same demote-lag
                    // condition as a missing one, so it reports too.
                    self.dropped_backpressure.fetch_add(1, Ordering::Relaxed);
                    crate::evidence::DERP_INBOUND_BACKPRESSURE.fetch_add(1, Ordering::Relaxed);
                    self.emit(MuxEvent::Unrouted(src));
                }
            }
            None => {
                self.dropped_unrouted.fetch_add(1, Ordering::Relaxed);
                crate::evidence::DERP_INBOUND_UNROUTED.fetch_add(1, Ordering::Relaxed);
                self.emit(MuxEvent::Unrouted(src));
            }
        }
    }

    /// Report a [`MuxEvent`], if anyone is listening. Never blocks and never
    /// errors: a full queue means a signal is already pending, which is
    /// exactly what this one would have said.
    fn emit(&self, ev: MuxEvent) {
        let tx = self.events_tx.lock().unwrap().clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(ev);
        }
    }

    /// Mark the node WS down — subsequent `DerpConn::send_to` calls error so the
    /// carrier's `dead` latch fires and the sweep rebuilds. The first
    /// `mark_down` of an outage stamps `down_since`; repeats keep the
    /// original stamp so `down_for` measures the whole outage.
    pub fn mark_down(&self) {
        self.alive.store(false, Ordering::Relaxed);
        let mut since = self.down_since.lock().unwrap();
        if since.is_none() {
            *since = Some(Instant::now());
        }
    }

    /// Mark the node WS up again (after a reconnect + re-register).
    ///
    /// #28 — emits [`MuxEvent::Recovered`] on the down→up EDGE only. The WS
    /// owner calls this after every successful connect, including the first, so
    /// a level-triggered emit would fire a spurious walk at startup; `swap`
    /// makes the edge the condition rather than a comment.
    pub fn mark_up(&self) {
        let was_down = !self.alive.swap(true, Ordering::Relaxed);
        *self.down_since.lock().unwrap() = None;
        if was_down {
            self.emit(MuxEvent::Recovered);
        }
    }

    /// Whether the node WS is currently up.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// How long the WS has been down, or `None` while it is up.
    pub fn down_for(&self) -> Option<Duration> {
        if self.is_alive() {
            return None;
        }
        self.down_since.lock().unwrap().map(|t| t.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> DerpPubKey {
        [b; 32]
    }

    /// #32 — a route must not outlive the conn that consumes it. Once the last
    /// reference to a `DerpConn` goes, the next frame for that peer must be
    /// REPORTED (so the demote-follow can act) rather than routed into a queue
    /// nobody drains.
    ///
    /// The surviving-`Arc` shape is the one worth exercising: a carrier holds
    /// its conn behind an `Arc`, and until every clone is gone the conn is
    /// still a legitimate consumer.
    #[tokio::test]
    async fn a_dropped_conn_retires_its_route_even_with_a_live_arc() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let (tx, mut rx) = mpsc::channel(DerpMux::EVENT_SINK_DEPTH);
        mux.set_event_sink(tx);
        let base = mux.drop_counts();

        let carrier: Arc<DerpConn> = Arc::new(mux.conn_for(pk(0x02)));
        let survivor = Arc::clone(&carrier);

        // A WG frame (first byte 1..=4 ⇒ the WG/disco consumer).
        let mut frame = pk(0x02).to_vec();
        frame.extend_from_slice(&[1, 0, 0, 0]);
        mux.deliver(&frame);
        assert!(
            rx.try_recv().is_err(),
            "while the conn is live the frame routes and must stay silent"
        );

        drop(carrier); // the direct tier is promoted: the carrier is replaced…
        mux.deliver(&frame);
        assert!(
            rx.try_recv().is_err(),
            "a still-referenced conn is still a valid consumer"
        );

        drop(survivor); // …and now the last reference goes.
        mux.deliver(&frame);
        assert_eq!(
            rx.try_recv().ok(),
            Some(MuxEvent::Unrouted(pk(0x02))),
            "a retired route must make the next frame REPORT, not vanish"
        );
        assert_eq!(
            mux.drop_counts().0 - base.0,
            1,
            "and be counted as unrouted"
        );
    }

    /// #32 — "last one wins" must survive the retirement, and the two consumers
    /// are INDEPENDENT. A rebuild registers the new conn before the old one
    /// drops, so a blind clear would unregister the live one; and a WG carrier
    /// going away must not take R4's tunnel route with it.
    #[tokio::test]
    async fn retirement_spares_a_rebuild_and_the_other_consumer() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let (tx, mut rx) = mpsc::channel(DerpMux::EVENT_SINK_DEPTH);
        mux.set_event_sink(tx);

        // (a) rebuild: old drops AFTER new registered ⇒ new survives.
        let old = mux.conn_for(pk(0x02));
        let new = mux.conn_for(pk(0x02));
        drop(old);
        let mut wg = pk(0x02).to_vec();
        wg.extend_from_slice(&[1, 0, 0, 0]);
        mux.deliver(&wg);
        assert!(rx.try_recv().is_err(), "the NEW route is live — no report");
        let mut buf = [0u8; 32];
        let (n, _) = new.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], &[1, 0, 0, 0], "and the frame reaches it");

        // (b) independence: dropping the WG conn must leave the tunnel route.
        let tunnel = mux.tunnel_conn_for(pk(0x02));
        drop(new);
        let mut quic = pk(0x02).to_vec();
        quic.extend_from_slice(&[0xC0, 1, 2, 3]); // QUIC long header ⇒ tunnel
        mux.deliver(&quic);
        let (n, _) = tunnel.recv_from(&mut buf).await.unwrap();
        assert_eq!(
            &buf[..n],
            &[0xC0, 1, 2, 3],
            "the tunnel consumer must survive the WG carrier's retirement"
        );
    }

    fn frame(src: DerpPubKey, payload: &[u8]) -> Vec<u8> {
        let mut f = src.to_vec();
        f.extend_from_slice(payload);
        f
    }

    /// The local disco-magic copy must never drift from the canonical one —
    /// a drift would silently re-route disco frames into the tunnel consumer
    /// and break carrier liveness for exactly the peers using the derp leg.
    #[cfg(feature = "overlay")]
    #[test]
    fn disco_magic_matches_the_canonical_module() {
        assert_eq!(DISCO_MAGIC, crate::overlay::disco::MAGIC);
    }

    /// R4 — per-frame demux: WG (first byte 1..=4) and disco (8-byte magic)
    /// reach the WG consumer; QUIC-shaped payloads (short header 0x40+, long
    /// header 0xC0+) reach the TUNNEL consumer when registered. A QUIC short
    /// header may start with 0x52 ('R'), so the disco test is the full magic.
    #[tokio::test]
    async fn deliver_splits_wg_disco_and_tunnel_per_frame() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let peer = pk(0x02);
        let wg_conn = mux.conn_for(peer);
        let tun_conn = mux.tunnel_conn_for(peer);

        let wg_payload = [1u8, 0, 0, 0, 0xAA]; // WG handshake-init shape
        let quic_short = [0x52u8, 9, 9, 9, 9]; // 'R' but NOT the disco magic
        let quic_long = [0xC3u8, 0, 0, 0, 1];
        let mut disco = DISCO_MAGIC.to_vec();
        disco.extend_from_slice(&[7; 24]);

        mux.deliver(&frame(peer, &wg_payload));
        mux.deliver(&frame(peer, &quic_short));
        mux.deliver(&frame(peer, &quic_long));
        mux.deliver(&frame(peer, &disco));

        let mut buf = [0u8; 128];
        let (n, _) = wg_conn.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], &wg_payload, "WG frame → WG consumer");
        let (n, _) = tun_conn.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], &quic_short, "QUIC short header → tunnel");
        let (n, _) = tun_conn.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], &quic_long, "QUIC long header → tunnel");
        let (n, _) = wg_conn.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], &disco[..], "disco magic → WG consumer");
    }

    /// R4 — with NO tunnel consumer registered the mux behaves exactly as
    /// pre-R4: every payload (including QUIC-shaped ones) goes to the WG
    /// route, and misses report `Unrouted` as shipped in #27.
    #[tokio::test]
    async fn deliver_without_tunnel_consumer_is_pre_r4_behavior() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let peer = pk(0x02);
        let wg_conn = mux.conn_for(peer);

        let quic_short = [0x52u8, 9, 9, 9, 9];
        mux.deliver(&frame(peer, &quic_short));
        let mut buf = [0u8; 64];
        let (n, _) = wg_conn.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], &quic_short, "no tunnel route ⇒ WG consumer");

        // An unknown peer still reports Unrouted (the #27 contract).
        let (ev_tx, mut ev_rx) = mpsc::channel(DerpMux::EVENT_SINK_DEPTH);
        mux.set_event_sink(ev_tx);
        mux.deliver(&frame(pk(0x03), &quic_short));
        assert_eq!(ev_rx.try_recv().unwrap(), MuxEvent::Unrouted(pk(0x03)));
    }

    /// Phase A1 — `down_for` measures the WHOLE outage (repeated
    /// `mark_down`s keep the original stamp) and clears on `mark_up`, so
    /// the evidence path can apply hysteresis without flap noise.
    #[test]
    fn down_for_spans_the_outage_and_clears_on_up() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        assert!(mux.is_alive());
        assert_eq!(mux.down_for(), None, "up ⇒ no outage duration");

        mux.mark_down();
        let first = mux.down_for().expect("down ⇒ Some(elapsed)");
        std::thread::sleep(Duration::from_millis(15));
        mux.mark_down(); // a repeat must NOT reset the stamp
        let later = mux.down_for().expect("still down");
        assert!(
            later >= first + Duration::from_millis(10),
            "repeated mark_down must keep the original outage start"
        );

        mux.mark_up();
        assert!(mux.is_alive());
        assert_eq!(mux.down_for(), None, "up again ⇒ cleared");
        mux.mark_down();
        assert!(
            mux.down_for().expect("fresh outage") < later,
            "a NEW outage restarts the clock"
        );
    }

    /// #27 — the black hole. A frame from a peer we hold NO conn for used to be
    /// dropped with no log, no counter and no signal; it is the 100 %-loss half
    /// of a one-sided demote to DERP (measured 69 s in the field). It must now
    /// report the src, and a routable frame must NOT report.
    #[tokio::test]
    async fn unroutable_inbound_reports_the_src_and_routable_does_not() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let (tx, mut rx) = mpsc::channel(DerpMux::EVENT_SINK_DEPTH);
        mux.set_event_sink(tx);

        // No conn for 0x02 — the demote-lag case.
        let mut frame = pk(0x02).to_vec();
        frame.extend_from_slice(&[1, 2, 3]);
        mux.deliver(&frame);
        assert_eq!(
            rx.try_recv().ok(),
            Some(MuxEvent::Unrouted(pk(0x02))),
            "unroutable src reported"
        );
        assert_eq!(mux.drop_counts().0, 1, "counted as unrouted");

        // A live conn for 0x03 — routed, and deliberately SILENT.
        let _conn = mux.conn_for(pk(0x03));
        let mut frame = pk(0x03).to_vec();
        frame.extend_from_slice(&[4, 5, 6]);
        mux.deliver(&frame);
        assert!(
            rx.try_recv().is_err(),
            "a routable frame must never look like a demote signal"
        );
        assert_eq!(mux.drop_counts(), (1, 0), "no new drops");

        // A short frame is malformed, not a demote signal.
        mux.deliver(&[0u8; 8]);
        assert!(rx.try_recv().is_err());
    }

    /// #28 — `mark_up` emits `Recovered` on the down→up EDGE only. The WS owner
    /// calls it after EVERY successful connect, including the first on a mux
    /// that starts alive, so a level-triggered emit would fire a pointless
    /// establish walk at every startup.
    #[tokio::test]
    async fn mark_up_emits_recovered_only_on_the_down_to_up_edge() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let (tx, mut rx) = mpsc::channel(DerpMux::EVENT_SINK_DEPTH);
        mux.set_event_sink(tx);

        // Startup shape: the mux is born alive and the WS owner marks it up.
        assert!(mux.is_alive());
        mux.mark_up();
        assert!(
            rx.try_recv().is_err(),
            "a mark_up that changes nothing must not wake the runtime"
        );

        // A real outage → recovery.
        mux.mark_down();
        assert!(rx.try_recv().is_err(), "going down is not a recovery");
        mux.mark_up();
        assert_eq!(rx.try_recv().ok(), Some(MuxEvent::Recovered));
        assert_eq!(mux.down_for(), None, "and the outage clock cleared");

        // Redundant repeats stay quiet.
        mux.mark_up();
        assert!(rx.try_recv().is_err());
    }

    /// #27/#32 — a peer whose DERP carrier was replaced by a better tier must
    /// report, or exactly the peers that once used DERP stay silently dark.
    ///
    /// ⚠️ The CLASSIFICATION changed with #32, and that is the point. It used
    /// to land in the closed-channel (backpressure) branch, because the route
    /// outlived its conn and `deliver` only noticed when `try_send` failed. The
    /// route is now retired on drop, so this is a genuine MISS — which is also
    /// the only shape that reports when something still holds an `Arc` to the
    /// conn and the channel therefore is not closed at all.
    #[tokio::test]
    async fn a_replaced_carrier_reports_as_unrouted() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let (tx, mut rx) = mpsc::channel(DerpMux::EVENT_SINK_DEPTH);
        mux.set_event_sink(tx);
        let base = mux.drop_counts();

        let conn = mux.conn_for(pk(0x02));
        drop(conn); // a better tier replaced this carrier

        let mut frame = pk(0x02).to_vec();
        frame.extend_from_slice(&[1]);
        mux.deliver(&frame);

        assert_eq!(rx.try_recv().ok(), Some(MuxEvent::Unrouted(pk(0x02))));
        let now = mux.drop_counts();
        assert_eq!(
            (now.0 - base.0, now.1 - base.1),
            (1, 0),
            "the route is retired, so this is a MISS — not backpressure"
        );
    }

    #[tokio::test]
    async fn deliver_routes_by_src_and_conn_receives_payload() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let conn = mux.conn_for(pk(0x02)); // pinned to peer 0x02

        // A frame from peer 0x02 → routed to this conn as raw payload.
        let mut frame = pk(0x02).to_vec();
        frame.extend_from_slice(&[9, 8, 7]);
        mux.deliver(&frame);

        let mut buf = [0u8; 64];
        let (n, src) = conn.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], &[9, 8, 7]);
        assert_eq!(src, synth_addr(&pk(0x02)), "recv tagged with the peer addr");
    }

    #[tokio::test]
    async fn send_to_frames_peer_pubkey_prefix() {
        let (mux, mut out_rx) = DerpMux::new(pk(0x01));
        let conn = mux.conn_for(pk(0x02));

        conn.send_to(&[1, 2, 3], "127.0.0.1:9".parse().unwrap())
            .await
            .unwrap();

        let framed = out_rx.recv().await.unwrap();
        assert_eq!(&framed[..32], &pk(0x02), "outbound frame targets the peer");
        assert_eq!(&framed[32..], &[1, 2, 3]);
    }

    #[tokio::test]
    async fn send_to_errors_when_ws_down_so_dead_latch_fires() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let conn = mux.conn_for(pk(0x02));
        mux.mark_down();
        let err = conn
            .send_to(&[1], "127.0.0.1:9".parse().unwrap())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    }

    #[tokio::test]
    async fn deliver_drops_unknown_src_and_short_frame() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let _conn = mux.conn_for(pk(0x02));
        // Unknown src 0x03 → dropped (no panic, no delivery).
        let mut frame = pk(0x03).to_vec();
        frame.extend_from_slice(&[1]);
        mux.deliver(&frame);
        // Short frame (< 32) → dropped.
        mux.deliver(&[0u8; 10]);
    }

    #[test]
    fn synth_addr_is_v4_nonzero_port_and_peer_unique() {
        let a = synth_addr(&pk(0xAA));
        let b = synth_addr(&{
            let mut k = pk(0xAA);
            k[4] = 0x01; // differ in a byte the port derives from
            k
        });
        assert!(a.is_ipv4() && a.port() != 0);
        assert_ne!(a, b, "distinct pubkeys → distinct synthetic addrs");
    }
}

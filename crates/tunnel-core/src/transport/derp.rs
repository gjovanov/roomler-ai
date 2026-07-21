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
}

/// A stable, non-routable synthetic `SocketAddr` derived from a pubkey:
/// `127.<pk0>.<pk1>.<pk2|1>:<pk3pk4 | 0x8000>`. Deterministic + unique enough
/// per peer; only used as a carrier "remote" placeholder.
fn synth_addr(pk: &DerpPubKey) -> SocketAddr {
    let ip = Ipv4Addr::new(127, pk[0], pk[1], pk[2].max(1));
    let port = u16::from_be_bytes([pk[3], pk[4]]) | 0x8000;
    SocketAddr::new(IpAddr::V4(ip), port)
}

#[async_trait]
impl RelayConn for DerpConn {
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
    peers: Mutex<HashMap<DerpPubKey, mpsc::Sender<Vec<u8>>>>,
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
            peers: Mutex::new(HashMap::new()),
        });
        (mux, ws_out_rx)
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
        self.peers.lock().unwrap().insert(peer_pubkey, in_tx);
        DerpConn {
            peer_pubkey,
            ws_out: self.ws_out.clone(),
            inbound: AsyncMutex::new(in_rx),
            alive: Arc::clone(&self.alive),
            synth_local: synth_addr(&self.self_pubkey),
            synth_peer: synth_addr(&peer_pubkey),
        }
    }

    /// Route one inbound relay frame `[src_pubkey(32) || payload]` to the
    /// [`DerpConn`] registered for `src_pubkey`. Drops a short frame or an
    /// unknown src (no such peer conn); drops on a full inbound queue.
    pub fn deliver(&self, frame: &[u8]) {
        if frame.len() < 32 {
            return;
        }
        let mut src = [0u8; 32];
        src.copy_from_slice(&frame[..32]);
        let sender = self.peers.lock().unwrap().get(&src).cloned();
        if let Some(tx) = sender {
            let _ = tx.try_send(frame[32..].to_vec());
        }
    }

    /// Mark the node WS down — subsequent `DerpConn::send_to` calls error so the
    /// carrier's `dead` latch fires and the sweep rebuilds.
    pub fn mark_down(&self) {
        self.alive.store(false, Ordering::Relaxed);
    }

    /// Mark the node WS up again (after a reconnect + re-register).
    pub fn mark_up(&self) {
        self.alive.store(true, Ordering::Relaxed);
    }

    /// Whether the node WS is currently up.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> DerpPubKey {
        [b; 32]
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

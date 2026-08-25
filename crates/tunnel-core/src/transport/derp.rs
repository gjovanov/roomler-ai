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

/// #32 — the `src_pubkey → inbound` registry, shared with every [`DerpConn`]
/// so a conn can retire its own entry on drop (see that `Drop` impl).
type PeerTable = Arc<Mutex<HashMap<DerpPubKey, mpsc::Sender<Vec<u8>>>>>;

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
    /// #32 — the mux's registration table, plus OUR sender in it, so [`Drop`]
    /// can retire exactly our own entry and never a newer peer's.
    peers: PeerTable,
    self_tx: mpsc::Sender<Vec<u8>>,
}

/// #32 — a registration must not outlive the conn that consumes it.
///
/// The table was written once and never cleaned: `conn_for` inserted, "last one
/// wins" replaced, and nothing ever removed. That is fine only while a dropped
/// `DerpConn` also closes its channel — and it does NOT if any `Arc` clone of
/// it survives the carrier being replaced (the coordinator, a build-queue
/// entry, a `relay_parts` tuple). Then `deliver` finds a live sender,
/// `try_send` returns **Ok**, and the frame lands in a queue nobody drains:
/// silent, and invisible to the #27 signal, which only reports on send
/// FAILURE. Field 2026-08-25 (pc55331 VPN reconnect): the peer relayed to us
/// with `initiate=true` for 18 s and the far end neither followed nor reported
/// anything — this is the leading explanation.
///
/// Removing on drop makes the registration's lifetime the consumer's lifetime,
/// which is the invariant `deliver` already assumed. Same class as
/// "an `RTCPeerConnection` must be `close()`d": bookkeeping outliving its owner.
impl Drop for DerpConn {
    fn drop(&mut self) {
        let mut peers = self.peers.lock().unwrap();
        // ⚠️ Only if the entry is STILL ours. A rebuild registers the new conn
        // before the old one drops ("last one wins"), and retiring the entry
        // blindly would unregister the LIVE carrier — turning a fix for a
        // silent drop into a guaranteed one.
        if peers
            .get(&self.peer_pubkey)
            .is_some_and(|tx| tx.same_channel(&self.self_tx))
        {
            peers.remove(&self.peer_pubkey);
        }
    }
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
    /// #32 — THIS mux's inbound-drop counts. Kept alongside the process-global
    /// `evidence::DERP_INBOUND_*` (which is what the operator view reports, a
    /// node holding several muxes) because a per-instance counter is the only
    /// one a test can assert exactly — sharing one global made the unit tests
    /// race each other, failing 1 run in 3.
    dropped_unrouted: std::sync::atomic::AtomicU64,
    dropped_backpressure: std::sync::atomic::AtomicU64,
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
            dropped_unrouted: std::sync::atomic::AtomicU64::new(0),
            dropped_backpressure: std::sync::atomic::AtomicU64::new(0),
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

    /// #27/#32 — `(unrouted, backpressure)` inbound-drop counts, cumulative
    /// since daemon start. PROCESS-global, not per-mux: a node holds several
    /// (central + one per relay region) and the question an operator asks is
    /// about the node. Surfaced through `NodeStatus::derp_inbound_drops`.
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
    ///
    /// #32 — and the conn RETIRES its own registration on drop, so the route's
    /// lifetime is its consumer's. See `impl Drop for DerpConn`.
    pub fn conn_for(&self, peer_pubkey: DerpPubKey) -> DerpConn {
        let (in_tx, in_rx) = mpsc::channel(INBOUND_QUEUE);
        self.peers
            .lock()
            .unwrap()
            .insert(peer_pubkey, in_tx.clone());
        DerpConn {
            peer_pubkey,
            ws_out: self.ws_out.clone(),
            inbound: AsyncMutex::new(in_rx),
            alive: Arc::clone(&self.alive),
            synth_local: synth_addr(&self.self_pubkey),
            synth_peer: synth_addr(&peer_pubkey),
            peers: Arc::clone(&self.peers),
            self_tx: in_tx,
        }
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
        let sender = self.peers.lock().unwrap().get(&src).cloned();
        match sender {
            Some(tx) => {
                if tx.try_send(frame[32..].to_vec()).is_err() {
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

        // #32 — the counters are PROCESS-global and cumulative, and the test
        // binary shares them across parallel tests: diff, never assert an
        // absolute (the same rule the field summaries follow).
        let base = mux.drop_counts();

        // No conn for 0x02 — the demote-lag case.
        let mut frame = pk(0x02).to_vec();
        frame.extend_from_slice(&[1, 2, 3]);
        mux.deliver(&frame);
        assert_eq!(
            rx.try_recv().ok(),
            Some(MuxEvent::Unrouted(pk(0x02))),
            "unroutable src reported"
        );
        assert_eq!(mux.drop_counts().0 - base.0, 1, "counted as unrouted");

        // A live conn for 0x03 — routed, and deliberately SILENT.
        let _conn = mux.conn_for(pk(0x03));
        let mut frame = pk(0x03).to_vec();
        frame.extend_from_slice(&[4, 5, 6]);
        mux.deliver(&frame);
        assert!(
            rx.try_recv().is_err(),
            "a routable frame must never look like a demote signal"
        );
        let now = mux.drop_counts();
        assert_eq!((now.0 - base.0, now.1 - base.1), (1, 0), "no new drops");

        // A short frame is malformed, not a demote signal.
        mux.deliver(&[0u8; 8]);
        assert!(rx.try_recv().is_err());
    }

    /// #32 — the field case, reproduced. A `DerpConn` whose carrier was
    /// replaced but whose `Arc` is still held somewhere (the coordinator, a
    /// build-queue entry, a `relay_parts` tuple) used to leave a LIVE
    /// registration: `try_send` returned Ok, the frame went into a queue nobody
    /// drains, and the #27 signal — which only reports on send FAILURE — saw
    /// nothing. That is a black hole one layer deeper than the one #27 closed.
    ///
    /// Retiring the registration on drop is what makes the miss observable. The
    /// surviving-Arc case is the one that matters, so hold one explicitly.
    #[tokio::test]
    async fn a_dropped_conn_retires_its_registration_even_with_a_live_arc() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let (tx, mut rx) = mpsc::channel(DerpMux::EVENT_SINK_DEPTH);
        mux.set_event_sink(tx);

        // The carrier holds the conn behind an Arc, exactly as `Carrier::relay`
        // does; a clone survives the carrier being replaced.
        let base = mux.drop_counts();
        let carrier: Arc<DerpConn> = Arc::new(mux.conn_for(pk(0x02)));
        let survivor = Arc::clone(&carrier);

        let mut frame = pk(0x02).to_vec();
        frame.extend_from_slice(&[1]);
        mux.deliver(&frame);
        assert!(
            rx.try_recv().is_err(),
            "while the conn is live the frame routes and must stay silent"
        );

        // The direct tier is promoted: the carrier is replaced and dropped —
        // but `survivor` keeps the channel OPEN, which is precisely why the
        // closed-channel branch never fired in the field.
        drop(carrier);
        mux.deliver(&frame);
        assert!(
            rx.try_recv().is_err(),
            "a still-referenced conn is still a valid consumer"
        );

        // Now the last reference goes. The registration must go with it.
        drop(survivor);
        mux.deliver(&frame);
        assert_eq!(
            rx.try_recv().ok(),
            Some(MuxEvent::Unrouted(pk(0x02))),
            "a retired conn must make the next frame REPORT, not vanish"
        );
        assert_eq!(
            mux.drop_counts().0 - base.0,
            1,
            "and be counted as unrouted"
        );
    }

    /// #32 — "last one wins" must survive the retirement. A rebuild registers
    /// the NEW conn before the old one drops, so a blind remove-on-drop would
    /// unregister the live carrier — converting a silent drop into a guaranteed
    /// one. The identity check is what prevents that.
    #[tokio::test]
    async fn a_rebuild_keeps_the_new_registration_when_the_old_conn_drops() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let (tx, mut rx) = mpsc::channel(DerpMux::EVENT_SINK_DEPTH);
        mux.set_event_sink(tx);

        let old = mux.conn_for(pk(0x02));
        let new = mux.conn_for(pk(0x02)); // rebuild: last one wins
        drop(old); // …and only now does the old one go

        let mut frame = pk(0x02).to_vec();
        frame.extend_from_slice(&[7, 7]);
        mux.deliver(&frame);
        assert!(
            rx.try_recv().is_err(),
            "the surviving registration is the NEW conn's — nothing to report"
        );

        let mut buf = [0u8; 16];
        let (n, _) = new.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], &[7, 7], "and the frame reaches the live conn");
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
    /// ⚠️ The CLASSIFICATION changed with #32 and that is the point: this used
    /// to land in the closed-channel (backpressure) branch, because the
    /// registration outlived its conn. It is now `unrouted`, because the conn
    /// retires its registration on drop. The old shape only reported when NO
    /// `Arc` survived — and in the field one did, which is how a peer relayed
    /// to us for 18 s with nothing logged.
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
            "the registration is retired, so this is a MISS — not backpressure"
        );
    }

    /// #32 — the backpressure branch still exists and still reports: a LIVE
    /// conn whose consumer has stalled is a different fault from a retired one
    /// (the carrier is there but not draining), and collapsing the two would
    /// hide it. Fill the queue past `INBOUND_QUEUE` without reading.
    #[tokio::test]
    async fn a_full_inbound_queue_still_reports_as_backpressure() {
        let (mux, _out_rx) = DerpMux::new(pk(0x01));
        let (tx, mut rx) = mpsc::channel(DerpMux::EVENT_SINK_DEPTH);
        mux.set_event_sink(tx);

        let base = mux.drop_counts();
        let _conn = mux.conn_for(pk(0x02)); // held, never read from
        let mut frame = pk(0x02).to_vec();
        frame.extend_from_slice(&[9]);
        for _ in 0..(INBOUND_QUEUE + 2) {
            mux.deliver(&frame);
        }

        let now = mux.drop_counts();
        let (unrouted, backpressure) = (now.0 - base.0, now.1 - base.1);
        assert_eq!(unrouted, 0, "the conn is live — nothing is unrouted");
        assert!(backpressure >= 1, "a stalled consumer must be counted");
        assert_eq!(
            rx.try_recv().ok(),
            Some(MuxEvent::Unrouted(pk(0x02))),
            "and reported, so a wedged carrier is not silent either"
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

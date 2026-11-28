// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Agent-side QUIC session peer (`quic-v1` transport).
//!
//! The QUIC analogue of [`crate::tunnel::peer::AgentTunnelPeer`]: one
//! per active tunnel session. Where the WebRTC peer pre-negotiates a
//! fixed DC pool + demuxes by `flow_id` prefix, QUIC gives each flow a
//! native bidirectional stream — so this peer's job is just:
//!
//! 1. Stand up a quinn **server** endpoint with an ephemeral
//!    self-signed cert (fingerprint shipped to the client over signaling
//!    so it can pin — there's no CA).
//! 2. Run an **accept loop** that authenticates the one client
//!    connection by the server-minted token, then reads each inbound
//!    flow stream's `flow_id` preamble and hands the stream to whichever
//!    forward is waiting for it.
//! 3. Expose [`take_flow`] so [`crate::tunnel::acceptor`] can, after
//!    dialing the destination + sending `TcpForwardAccept`, grab the
//!    client-opened QUIC stream for that `flow_id` and drive
//!    [`tunnel_core::forward::run_flow_quic`].
//!
//! Lifecycle: `ServerMsg::TunnelQuicSetup` → [`setup`] → reply
//! `ClientMsg::TunnelQuicReady { cert_fingerprint, addrs }` →
//! `ServerMsg::TcpForwardForward` per flow → acceptor dials + `take_flow`
//! → `run_flow_quic`. `TunnelTerminate` → [`close`], or [`end`] when it asks
//! the exit to close the session's connection (`close_connection`): the flows
//! stop and the connection closes with [`SESSION_END_CODE`]. Or no terminate at all
//! (#1754): the accept loop serves exactly ONE connection, so when it ends —
//! the client closed, or quinn idle-timed the connection out 30 s after the
//! client's 8 s keepalives stopped — the session is over, and the loop
//! reports that on the org loop's [`super::reap`] channel so the signaling
//! loop drops the peer, and with it the endpoint's socket and, on the
//! relayed flavours, the TURN allocation.
//!
//! **Rendezvous** uses two maps so it's order-independent: a stream may
//! arrive before OR after the acceptor registers interest (the client
//! opens the stream right after `TcpForwardAccept`, but the accept loop
//! and the acceptor run concurrently). `waiters` holds pending forwards;
//! `ready` stashes streams that arrived first.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bson::oid::ObjectId;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};
use tracing::{debug, info, warn};
use tunnel_core::transport::quic::{self, QuicConnection, QuicPeer, RecvStream, SendStream};
use tunnel_core::transport::relay::{RelayConn, RelayUdpSocket};

use super::reap::{ReapSender, ReapSignal, TunnelReap};

/// The application close code [`AgentQuicPeer::end`] closes a session's
/// connection with. Distinct from the failed auth's `1`.
pub const SESSION_END_CODE: u32 = 2;

/// The close reason sent with [`SESSION_END_CODE`].
pub const SESSION_END_REASON: &[u8] = b"session ended";

/// The two stream halves of one QUIC flow, and the [`FlowHold`] its forward
/// keeps while the flow runs.
pub type FlowStreams = (SendStream, RecvStream, FlowHold);

/// What a forward keeps while its flow runs, so the session's end
/// ([`SessionLink::end`]) reaches the flow: the session's stop, which the
/// flow runs under.
pub struct FlowHold {
    _conn: Arc<QuicConnection>,
    stop: CancellationToken,
}

impl FlowHold {
    /// Resolves when the session ends ([`SessionLink::end`]).
    pub fn stopped(&self) -> WaitForCancellationFuture<'_> {
        self.stop.cancelled()
    }
}

/// How a session's end reaches its connection and its flows: a [`Weak`] to
/// the connection, and the stop every flow runs under ([`FlowHold`]). Cheap
/// to clone, and to keep after the session left the signaling loop's map
/// ([`EndedQuicSessions`]).
#[derive(Clone, Default)]
pub struct SessionLink(Arc<LinkInner>);

#[derive(Default)]
struct LinkInner {
    conn: std::sync::Mutex<Weak<QuicConnection>>,
    stop: CancellationToken,
}

impl SessionLink {
    /// The authenticated connection, from the accept loop.
    fn attach(&self, conn: &Arc<QuicConnection>) {
        *self.0.conn.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(conn);
    }

    /// What a flow on `conn` keeps while it runs.
    fn hold(&self, conn: &Arc<QuicConnection>) -> FlowHold {
        FlowHold {
            _conn: Arc::clone(conn),
            stop: self.0.stop.clone(),
        }
    }

    /// The session's connection, while it can still be reached.
    pub fn conn(&self) -> Option<Arc<QuicConnection>> {
        self.0
            .conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()
    }

    /// End the session: stop every flow, and close the connection with
    /// [`SESSION_END_CODE`]. Returns whether a connection was there to close.
    /// Idempotent.
    pub fn end(&self) -> bool {
        self.0.stop.cancel();
        match self.conn() {
            Some(conn) => {
                conn.close(SESSION_END_CODE.into(), SESSION_END_REASON);
                true
            }
            None => false,
        }
    }
}

/// How long a session a terminate took out of the map without closing its
/// connection stays reachable through [`EndedQuicSessions`].
pub const ENDED_SESSION_TTL: Duration = Duration::from_secs(60);

/// The most sessions [`EndedQuicSessions`] keeps; the oldest goes first.
pub const ENDED_SESSIONS_CAP: usize = 64;

/// The sessions a terminate took out of the map without closing their
/// connection, each by its [`SessionLink`], for [`ENDED_SESSION_TTL`]: a
/// terminate that asks to close the connection and arrives after one that did
/// not still reaches the session through here.
#[derive(Default)]
pub struct EndedQuicSessions {
    by_session: HashMap<ObjectId, (Instant, SessionLink)>,
}

impl EndedQuicSessions {
    /// Keep `link` for `session_id`.
    pub fn record(&mut self, session_id: ObjectId, link: SessionLink) {
        self.record_at(session_id, link, Instant::now());
    }

    fn record_at(&mut self, session_id: ObjectId, link: SessionLink, now: Instant) {
        self.prune(now);
        self.by_session.insert(session_id, (now, link));
        while self.by_session.len() > ENDED_SESSIONS_CAP {
            let Some(oldest) = self
                .by_session
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(sid, _)| *sid)
            else {
                break;
            };
            self.by_session.remove(&oldest);
        }
    }

    /// End the session recorded for `session_id` ([`SessionLink::end`]).
    /// Returns whether one was recorded.
    pub fn end(&mut self, session_id: ObjectId) -> bool {
        self.end_at(session_id, Instant::now())
    }

    fn end_at(&mut self, session_id: ObjectId, now: Instant) -> bool {
        self.prune(now);
        match self.by_session.remove(&session_id) {
            Some((_, link)) => {
                link.end();
                true
            }
            None => false,
        }
    }

    fn prune(&mut self, now: Instant) {
        self.by_session
            .retain(|_, (at, _)| now.saturating_duration_since(*at) < ENDED_SESSION_TTL);
    }

    /// How many sessions are kept.
    pub fn len(&self) -> usize {
        self.by_session.len()
    }

    /// Whether none is kept.
    pub fn is_empty(&self) -> bool {
        self.by_session.is_empty()
    }
}

/// Order-independent rendezvous between the accept loop (which produces
/// streams) and [`take_flow`] (which consumes them).
#[derive(Default)]
struct Rendezvous {
    /// Forwards awaiting their stream (registered by `take_flow`).
    waiters: HashMap<u32, oneshot::Sender<FlowStreams>>,
    /// Streams that arrived before a waiter registered.
    ready: HashMap<u32, FlowStreams>,
}

pub struct AgentQuicPeer {
    session_id: ObjectId,
    cert_fingerprint: String,
    local_addr: SocketAddr,
    rendezvous: Arc<Mutex<Rendezvous>>,
    accept_task: tokio::task::JoinHandle<()>,
    /// Keep the endpoint alive for the life of the session (dropping it
    /// closes the quinn endpoint). `_peer` is read only via the accept
    /// task's clone; held here so the session owns its lifetime.
    _peer: Arc<QuicPeer>,
    /// Phase 3d: present when this peer rides a TURN relay
    /// (QUIC-over-TURN). Held so [`permit`](Self::permit) can install a
    /// TURN permission for the client's relay address by sending one
    /// bootstrap datagram through the same allocation the endpoint uses.
    /// `None` for a direct (host-candidate) peer.
    relay: Option<Arc<dyn RelayConn>>,
    /// R4 — `Some(self pubkey hex)` when this peer serves the
    /// `quic-derp-v1` flavor; shipped in `TunnelQuicReady.derp_pubkey` so
    /// the client knows which DERP identity to dial. `None` on the
    /// TURN/direct paths.
    derp_pubkey_hex: Option<String>,
    /// The session's connection and the stop its flows run under, reached
    /// without holding either ([`Self::end`], [`Self::link`]).
    link: SessionLink,
}

/// Spawn the accept loop shared by [`AgentQuicPeer::setup`],
/// [`AgentQuicPeer::setup_over_relay`] and [`AgentQuicPeer::setup_over_derp`]:
/// accept ONE client connection, validate `quic_auth_token`, then
/// rendezvous each inbound flow stream to whichever
/// [`AgentQuicPeer::take_flow`] waiter wants it (stashing streams that
/// arrive before their waiter registers).
///
/// #1754 — when the loop ends, for any reason, it reports the session on
/// `reap_tx`. It serves exactly one connection, so its end is unambiguous:
/// the client closed, quinn idle-timed the connection out (30 s, with the
/// client's 8 s keepalives gone), the client failed auth, or the endpoint
/// closed under it. An idle but live client never ends it — quinn's
/// keepalive is below its idle timeout by design. Our own `close()` ABORTS
/// the task, so it never reports a close the loop already did.
fn spawn_accept_loop(
    peer: Arc<QuicPeer>,
    session_id: ObjectId,
    quic_auth_token: String,
    rendezvous: Arc<Mutex<Rendezvous>>,
    reap_tx: ReapSender,
    link: SessionLink,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        serve_one_connection(peer, session_id, quic_auth_token, rendezvous, link).await;
        // Already on a task of our own, so the send can simply be awaited;
        // it fails only when the org loop itself is gone.
        if reap_tx
            .send(TunnelReap {
                session_id,
                signal: ReapSignal::QuicConnEnded,
            })
            .await
            .is_err()
        {
            debug!(%session_id, "agent quic: connection ended but the org loop is gone");
        }
    })
}

/// The body of the accept task: one connection, authenticated, served
/// until it ends.
async fn serve_one_connection(
    peer: Arc<QuicPeer>,
    session_id: ObjectId,
    quic_auth_token: String,
    rendezvous: Arc<Mutex<Rendezvous>>,
    link: SessionLink,
) {
    let conn = match peer.accept().await {
        Some(Ok(c)) => c,
        Some(Err(e)) => {
            warn!(%session_id, %e, "agent quic: accept failed");
            return;
        }
        None => {
            debug!(%session_id, "agent quic: endpoint closed before connect");
            return;
        }
    };
    // The server is no longer in the byte path — this token is what
    // authorizes the dialing client (cert-pinning already
    // authenticated US to them).
    if let Err(e) = quic::server_authenticate(&conn, &quic_auth_token).await {
        warn!(%session_id, %e, "agent quic: client auth FAILED — dropping connection");
        conn.close(1u32.into(), b"auth failed");
        return;
    }
    info!(%session_id, "agent quic: client authenticated; serving flow streams");
    let conn = Arc::new(conn);
    link.attach(&conn);
    loop {
        match quic::accept_flow(&conn).await {
            Ok((flow_id, send, recv)) => {
                let streams = (send, recv, link.hold(&conn));
                let mut rdv = rendezvous.lock().await;
                if let Some(tx) = rdv.waiters.remove(&flow_id) {
                    // A forward is already waiting — hand it over.
                    if tx.send(streams).is_err() {
                        debug!(%session_id, flow_id, "agent quic: forward dropped before stream");
                    }
                } else {
                    // Stream beat the forward — stash it.
                    rdv.ready.insert(flow_id, streams);
                }
            }
            Err(e) => {
                debug!(%session_id, %e, "agent quic: accept_flow loop ended");
                break;
            }
        }
    }
}

impl AgentQuicPeer {
    /// Bind a quinn server endpoint for this session + spawn the accept
    /// loop. `bind` is the local socket to listen on (`0.0.0.0:0` in
    /// production so all interfaces are reachable; tests use
    /// `127.0.0.1:0`). The loop accepts ONE client connection,
    /// validates `quic_auth_token`, then rendezvouses inbound flow
    /// streams. Ship [`cert_fingerprint`] + [`addrs`] to the client in
    /// `ClientMsg::TunnelQuicReady`. `reap_tx` is the org loop's
    /// [`super::reap`] channel, reported on when the connection ends
    /// (#1754).
    pub fn setup(
        session_id: ObjectId,
        quic_auth_token: String,
        bind: SocketAddr,
        reap_tx: ReapSender,
    ) -> Result<Self> {
        let (peer, cert_fingerprint) =
            QuicPeer::server(bind).context("agent quic: server endpoint")?;
        let local_addr = peer.local_addr().context("agent quic: local_addr")?;
        let peer = Arc::new(peer);
        let rendezvous: Arc<Mutex<Rendezvous>> = Arc::new(Mutex::new(Rendezvous::default()));
        let link = SessionLink::default();
        let accept_task = spawn_accept_loop(
            Arc::clone(&peer),
            session_id,
            quic_auth_token,
            Arc::clone(&rendezvous),
            reap_tx,
            link.clone(),
        );

        Ok(Self {
            session_id,
            cert_fingerprint,
            local_addr,
            rendezvous,
            accept_task,
            _peer: peer,
            relay: None,
            derp_pubkey_hex: None,
            link,
        })
    }

    /// Phase 3d: like [`setup`](Self::setup) but stand the quinn server
    /// endpoint up over a TURN-relayed datagram conn (QUIC-over-TURN) for
    /// symmetric-NAT / UDP-restricted nets where a direct host candidate
    /// is unreachable. `relay` is a live allocation (from
    /// [`tunnel_core::transport::relay::allocate_relay_from_ice`]); we
    /// wrap it in a [`RelayUdpSocket`] for quinn and keep a clone so
    /// [`permit`](Self::permit) can bootstrap the client's TURN
    /// permission. [`addrs`](Self::addrs) then reports the **relayed**
    /// address coturn handed out — what the client dials (over its own
    /// relay). The accept loop + auth + rendezvous are identical to the
    /// direct path.
    pub fn setup_over_relay(
        session_id: ObjectId,
        quic_auth_token: String,
        relay: Arc<dyn RelayConn>,
        reap_tx: ReapSender,
    ) -> Result<Self> {
        let local_addr = relay
            .local_addr()
            .context("agent quic relay: relayed local_addr")?;
        let sock = Arc::new(
            RelayUdpSocket::new(Arc::clone(&relay)).context("agent quic relay: socket bridge")?,
        );
        let (peer, cert_fingerprint) = QuicPeer::server_over_abstract_socket(sock)
            .context("agent quic relay: server endpoint over relay")?;
        let peer = Arc::new(peer);
        let rendezvous: Arc<Mutex<Rendezvous>> = Arc::new(Mutex::new(Rendezvous::default()));
        let link = SessionLink::default();
        let accept_task = spawn_accept_loop(
            Arc::clone(&peer),
            session_id,
            quic_auth_token,
            Arc::clone(&rendezvous),
            reap_tx,
            link.clone(),
        );

        Ok(Self {
            session_id,
            cert_fingerprint,
            local_addr,
            rendezvous,
            accept_task,
            _peer: peer,
            relay: Some(relay),
            derp_pubkey_hex: None,
            link,
        })
    }

    /// R4 — like [`setup_over_relay`](Self::setup_over_relay) but the quinn
    /// server endpoint rides a DERP-backed conn (`DerpMux::tunnel_conn_for`
    /// toward the CLIENT's pubkey) with the MTU-clamped derp transport
    /// config. No TURN allocation, no [`permit`](Self::permit) step — DERP
    /// is pubkey-addressed and has no permission model. `self_pubkey_hex`
    /// is this node's own DERP identity, shipped in `TunnelQuicReady` so
    /// the client knows whom to dial back over its own mux.
    pub fn setup_over_derp(
        session_id: ObjectId,
        quic_auth_token: String,
        relay: Arc<dyn RelayConn>,
        self_pubkey_hex: String,
        reap_tx: ReapSender,
    ) -> Result<Self> {
        let local_addr = relay
            .local_addr()
            .context("agent quic derp: synth local_addr")?;
        let sock = Arc::new(
            RelayUdpSocket::new(Arc::clone(&relay)).context("agent quic derp: socket bridge")?,
        );
        let (peer, cert_fingerprint) = QuicPeer::server_over_derp(sock)
            .context("agent quic derp: server endpoint over derp conn")?;
        let peer = Arc::new(peer);
        let rendezvous: Arc<Mutex<Rendezvous>> = Arc::new(Mutex::new(Rendezvous::default()));
        let link = SessionLink::default();
        let accept_task = spawn_accept_loop(
            Arc::clone(&peer),
            session_id,
            quic_auth_token,
            Arc::clone(&rendezvous),
            reap_tx,
            link.clone(),
        );

        Ok(Self {
            session_id,
            cert_fingerprint,
            local_addr,
            rendezvous,
            accept_task,
            _peer: peer,
            relay: Some(relay),
            derp_pubkey_hex: Some(self_pubkey_hex),
            link,
        })
    }

    /// R4 — this peer's DERP identity when serving `quic-derp-v1`.
    pub fn derp_pubkey_hex(&self) -> Option<&str> {
        self.derp_pubkey_hex.as_deref()
    }

    /// SHA-256 fingerprint (hex) of the ephemeral cert — pinned by the
    /// client, shipped in `ClientMsg::TunnelQuicReady`.
    pub fn cert_fingerprint(&self) -> &str {
        &self.cert_fingerprint
    }

    /// Dialable candidate addresses for the client.
    ///
    /// Bound to a specific IP (tests / explicit bind) means it is
    /// already dialable, so advertise it as-is. Bound to `0.0.0.0`
    /// (production) means we enumerate real host candidates (the primary
    /// egress interface IP with the bound port) via
    /// [`quic::host_candidates`]; the bare `0.0.0.0:port` the endpoint
    /// listens on is NOT dialable by a remote client. This is the
    /// Phase-2 (Tier 1) host-candidate step; Phase 2b appends STUN
    /// server-reflexive candidates for NAT'd hosts. If no egress route
    /// is found we fall back to the bound address so same-host dials
    /// still resolve (a failed remote dial degrades to webrtc-dc-v1).
    pub fn addrs(&self) -> Vec<String> {
        if self.local_addr.ip().is_unspecified() {
            let cands = quic::host_candidates(self.local_addr.port());
            if cands.is_empty() {
                vec![self.local_addr.to_string()]
            } else {
                cands.into_iter().map(|a| a.to_string()).collect()
            }
        } else {
            vec![self.local_addr.to_string()]
        }
    }

    /// Phase 3d: install a TURN permission for `client_addr` by sending
    /// one bootstrap datagram to it through this peer's relay allocation.
    /// The agent is the QUIC *server* and never sends first, so without a
    /// pre-installed permission for the client's relay address coturn
    /// silently drops the client's opening Initials. Called from the
    /// signaling loop on `rc:tunnel.quic.candidate`. The stray byte is
    /// discarded by the client's quinn as a too-short packet; QUIC's
    /// Initial retransmission covers any install/handshake race. A no-op
    /// (`Ok`) for a direct (non-relay) peer — there is nothing to permit.
    pub async fn permit(&self, client_addr: SocketAddr) -> Result<()> {
        match &self.relay {
            Some(relay) => {
                relay
                    .send_to(b"\x00", client_addr)
                    .await
                    .with_context(|| format!("agent quic: permit bootstrap to {client_addr}"))?;
                debug!(session_id = %self.session_id, %client_addr, "agent quic: TURN permission installed");
                Ok(())
            }
            None => {
                debug!(session_id = %self.session_id, %client_addr, "agent quic: permit on direct peer — ignoring");
                Ok(())
            }
        }
    }

    /// Register interest in `flow_id`'s inbound QUIC stream and await it.
    /// The client opens the stream right after it receives
    /// `TcpForwardAccept`; this races the accept loop, so we check the
    /// `ready` stash first (stream already arrived) before parking a
    /// waiter. Times out so a client that never opens the stream doesn't
    /// leak the forward.
    pub async fn take_flow(&self, flow_id: u32, timeout: Duration) -> Result<FlowStreams> {
        let stop = self.link.0.stop.clone();
        let rx = {
            let mut rdv = self.rendezvous.lock().await;
            if stop.is_cancelled() {
                rdv.ready.remove(&flow_id);
                bail!("agent quic: flow {flow_id}: the session ended");
            }
            if let Some(streams) = rdv.ready.remove(&flow_id) {
                return Ok(streams);
            }
            let (tx, rx) = oneshot::channel();
            rdv.waiters.insert(flow_id, tx);
            rx
        };
        let waited = tokio::select! {
            waited = tokio::time::timeout(timeout, rx) => waited,
            () = stop.cancelled() => {
                self.rendezvous.lock().await.waiters.remove(&flow_id);
                bail!("agent quic: flow {flow_id}: the session ended")
            }
        };
        match waited {
            Ok(Ok(streams)) => Ok(streams),
            Ok(Err(_)) => {
                self.rendezvous.lock().await.waiters.remove(&flow_id);
                bail!("agent quic: flow {flow_id} rendezvous sender dropped")
            }
            Err(_) => {
                self.rendezvous.lock().await.waiters.remove(&flow_id);
                bail!("agent quic: flow {flow_id} stream not opened within {timeout:?}")
            }
        }
    }

    /// Tear down the accept loop. The endpoint closes when the last
    /// `Arc<QuicPeer>` drops with `self`. Idempotent.
    pub fn close(&self) {
        self.accept_task.abort();
        debug!(session_id = %self.session_id, "agent quic peer closed");
    }

    /// End the session: stop every flow (each aborts both halves, which
    /// releases its destination socket and its streams), close the
    /// connection with [`SESSION_END_CODE`], close the endpoint, and stop the
    /// accept loop. The endpoint's socket, and on the relayed flavours the
    /// TURN allocation, go when the closed connection drains and the last
    /// `Arc` of this peer drops. Idempotent.
    pub fn end(&self) {
        let had_connection = self.link.end();
        self._peer.close(SESSION_END_CODE, SESSION_END_REASON);
        self.accept_task.abort();
        info!(session_id = %self.session_id, had_connection, "agent quic: session ended; connection closed");
    }

    /// #1754 — the accept loop reported its end. When the connection ended
    /// too, or was never served, [`end`](Self::end) the session and return
    /// `true`. When the connection is still up, only [`close`](Self::close)
    /// and return `false`: the caller keeps [`link`](Self::link) so a later
    /// terminate can still end it.
    pub fn reap(&self) -> bool {
        match self.link.conn() {
            Some(conn) if conn.close_reason().is_none() => {
                self.close();
                false
            }
            _ => {
                self.end();
                true
            }
        }
    }

    /// The session's [`SessionLink`], to end it after this peer left the
    /// signaling loop's map.
    pub fn link(&self) -> SessionLink {
        self.link.clone()
    }

    /// #1754 — has the one connection this peer serves already ended?
    /// The accept task's end IS that event (see [`spawn_accept_loop`]);
    /// the task reports on the reap channel as its last act, so this
    /// reads true a poll after the report lands. Read by the R3 reclaim —
    /// a whole reconnect later — so a peer whose client went away while
    /// it was parked across a control-WS reattach is dropped rather than
    /// re-adopted; its report on the reap channel is answered by an
    /// empty map either way.
    pub fn accept_ended(&self) -> bool {
        self.accept_task.is_finished()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::error::TryRecvError;
    use tunnel_core::transport::quic::QuicPeer as ClientQuicPeer;

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    /// A reap channel for tests that only need to hand a sender over.
    fn reap_channel() -> (ReapSender, mpsc::Receiver<TunnelReap>) {
        mpsc::channel(super::super::reap::TUNNEL_REAP_CAP)
    }

    /// Full in-process exercise of the agent QUIC session machinery (no
    /// signaling server): set up the peer, connect a pinned + token-
    /// authed client, open a flow stream, and verify the agent's
    /// `take_flow` rendezvous yields the stream and the bytes arrive.
    /// Covers BOTH rendezvous orderings via the stash + waiter paths.
    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(
        windows,
        ignore = "quinn-udp probes GSO at the first send on a socket it binds to [::]:0, which raises a firewall prompt on Windows (#4500)"
    )]
    async fn agent_quic_rendezvous_delivers_authed_flow() {
        let session_id = ObjectId::new();
        let token = "session-token-xyz";
        let (reap_tx, _reap_rx) = reap_channel();
        let agent =
            AgentQuicPeer::setup(session_id, token.to_string(), loopback(), reap_tx).unwrap();
        let fingerprint = agent.cert_fingerprint().to_string();
        let addr: SocketAddr = agent.addrs()[0].parse().unwrap();

        // Client: pin the agent's cert, connect, authenticate.
        let client = ClientQuicPeer::client(loopback(), &fingerprint).unwrap();
        let conn = client.connect(addr).await.unwrap();
        quic::client_authenticate(&conn, token).await.unwrap();

        // Concurrently: agent waits for flow 5, client opens it + sends.
        // join! makes the ordering irrelevant — the two-map rendezvous
        // resolves whichever side lands first.
        let (taken, _opened) = tokio::join!(agent.take_flow(5, Duration::from_secs(10)), async {
            let (mut send, _recv) = quic::open_flow(&conn, 5).await.unwrap();
            send.write_all(b"hello over quic flow").await.unwrap();
            send.finish().unwrap();
        });

        let (_a_send, mut a_recv, _hold) = taken.expect("agent must receive flow 5's stream");
        // quinn's RecvStream has its own read_to_end(size_limit) → Vec.
        let buf = a_recv.read_to_end(64 * 1024).await.unwrap();
        assert_eq!(
            &buf, b"hello over quic flow",
            "flow bytes must arrive intact"
        );

        agent.close();
    }

    /// #1754 — a client that drops its connection and endpoint (a crash,
    /// a `roomler kill` on a CLI too old to send `rc:tunnel.terminate`)
    /// ends the accept loop, and the loop reports the session on the
    /// reap channel so the signaling loop drops the peer — endpoint socket
    /// and all. Nothing is reported while the client lives, and
    /// `accept_ended` flips with the report.
    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(
        windows,
        ignore = "quinn-udp probes GSO at the first send on a socket it binds to [::]:0, which raises a firewall prompt on Windows (#4500)"
    )]
    async fn a_client_that_drops_its_connection_is_reaped() {
        let session_id = ObjectId::new();
        let token = "session-token-gone";
        let (reap_tx, mut reap_rx) = reap_channel();
        let agent =
            AgentQuicPeer::setup(session_id, token.to_string(), loopback(), reap_tx).unwrap();
        let fingerprint = agent.cert_fingerprint().to_string();
        let addr: SocketAddr = agent.addrs()[0].parse().unwrap();

        let client = ClientQuicPeer::client(loopback(), &fingerprint).unwrap();
        let conn = client.connect(addr).await.unwrap();
        quic::client_authenticate(&conn, token).await.unwrap();
        // One flow, so the connection is unmistakably in service.
        let (taken, _opened) = tokio::join!(agent.take_flow(3, Duration::from_secs(10)), async {
            let (mut send, _recv) = quic::open_flow(&conn, 3).await.unwrap();
            send.write_all(b"one flow").await.unwrap();
            send.finish().unwrap();
        });
        taken.expect("agent must receive flow 3's stream");
        assert!(
            matches!(reap_rx.try_recv(), Err(TryRecvError::Empty)),
            "nothing is reaped while the client lives"
        );
        assert!(
            !agent.accept_ended(),
            "the accept loop serves a live client"
        );

        // The client goes away: its last connection handle and its
        // endpoint drop, with no terminate sent to anyone.
        drop(conn);
        drop(client);

        let reap = tokio::time::timeout(Duration::from_secs(10), reap_rx.recv())
            .await
            .expect("a dropped client is reported within 10 s")
            .expect("reap channel open");
        assert_eq!(reap.session_id, session_id);
        assert_eq!(reap.signal, ReapSignal::QuicConnEnded);
        // The task sends its report as its last act, so `accept_ended`
        // trails the message by the task's final poll — the reclaim reads
        // it a whole reconnect later, but here give it a moment.
        for _ in 0..100 {
            if agent.accept_ended() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            agent.accept_ended(),
            "the accept task ends with (just after) its report"
        );

        agent.close();
    }

    /// Bytes both ways on one flow: client to agent, then agent to client.
    async fn round_trip(
        client_send: &mut SendStream,
        agent_recv: &mut RecvStream,
        agent_send: &mut SendStream,
        client_recv: &mut RecvStream,
        tag: &[u8],
    ) {
        client_send.write_all(tag).await.unwrap();
        let mut got = vec![0u8; tag.len()];
        tokio::time::timeout(Duration::from_secs(5), agent_recv.read_exact(&mut got))
            .await
            .expect("client to agent within 5 s")
            .expect("client to agent read");
        assert_eq!(got, tag);
        agent_send.write_all(tag).await.unwrap();
        let mut back = vec![0u8; tag.len()];
        tokio::time::timeout(Duration::from_secs(5), client_recv.read_exact(&mut back))
            .await
            .expect("agent to client within 5 s")
            .expect("agent to client read");
        assert_eq!(back, tag);
    }

    /// #4501: one stream that ends before its 4-byte `flow_id` preamble
    /// arrives costs that stream, never the session. The client resets a
    /// flow's stream when its local TCP read fails
    /// (`tunnel_core::forward::run_flow_quic`), and a local app that aborts
    /// its connection right after the SOCKS reply does that to a brand-new
    /// flow. The accept loop reads each preamble inline and takes that error
    /// for the connection's end: it reports `QuicConnEnded`, the signalling
    /// loop reaps the session and tells the client, and every in-flight flow
    /// on it is cut. The field measured whole sessions ending this way under
    /// live traffic, each a few milliseconds after a flow that moved no byte.
    ///
    /// The stray stream never carries a whole preamble, so the outcome does
    /// not depend on which of its frames the agent reads first.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "#4501 RED on master: a stream reset before its preamble ends the accept loop, so the session is reaped"]
    async fn a_stream_reset_before_its_preamble_costs_only_that_stream() {
        let session_id = ObjectId::new();
        let token = "session-token-stray";
        let (reap_tx, mut reap_rx) = reap_channel();
        let agent =
            AgentQuicPeer::setup(session_id, token.to_string(), loopback(), reap_tx).unwrap();
        let fingerprint = agent.cert_fingerprint().to_string();
        let addr: SocketAddr = agent.addrs()[0].parse().unwrap();
        let client = ClientQuicPeer::client(loopback(), &fingerprint).unwrap();
        let conn = client.connect(addr).await.unwrap();
        quic::client_authenticate(&conn, token).await.unwrap();

        // Flow 1 is the in-flight connection: served, and moving bytes.
        let (taken, opened) = tokio::join!(
            agent.take_flow(1, Duration::from_secs(10)),
            quic::open_flow(&conn, 1)
        );
        let (mut agent_send, mut agent_recv, _hold) =
            taken.expect("agent must receive flow 1's stream");
        let (mut client_send, mut client_recv) = opened.expect("client opens flow 1");
        round_trip(
            &mut client_send,
            &mut agent_recv,
            &mut agent_send,
            &mut client_recv,
            b"before",
        )
        .await;

        // The stray: half a preamble, then a reset. A flow whose local app
        // went away before the agent read which flow it is.
        let (mut stray, _stray_recv) = conn.open_bi().await.unwrap();
        stray.write_all(&2u32.to_le_bytes()[..2]).await.unwrap();
        stray.reset(0u32.into()).unwrap();

        // A flow opened after the stray is still served, and nothing is
        // reaped while the client lives.
        let (third, opened3) = tokio::join!(
            agent.take_flow(3, Duration::from_secs(5)),
            quic::open_flow(&conn, 3)
        );
        let _client3 = opened3.expect("client opens flow 3");
        let reaped = reap_rx.try_recv().ok().map(|r| r.signal);
        assert!(
            third.is_ok() && reaped.is_none() && !agent.accept_ended(),
            "a stream reset before its preamble must cost only that stream: \
             flow 3 = {:?}, reaped = {reaped:?}, accept loop ended = {}",
            third.as_ref().map(|_| "served").map_err(|e| e.to_string()),
            agent.accept_ended()
        );

        // The in-flight flow still moves bytes both ways.
        round_trip(
            &mut client_send,
            &mut agent_recv,
            &mut agent_send,
            &mut client_recv,
            b"after",
        )
        .await;
        agent.close();
    }

    /// A client presenting the WRONG token must NOT get its flow served:
    /// the accept loop closes the connection after auth fails, so
    /// `take_flow` times out (no stream is ever rendezvoused).
    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(
        windows,
        ignore = "quinn-udp probes GSO at the first send on a socket it binds to [::]:0, which raises a firewall prompt on Windows (#4500)"
    )]
    async fn agent_quic_rejects_bad_token_so_no_flow() {
        let session_id = ObjectId::new();
        let (reap_tx, _reap_rx) = reap_channel();
        let agent = AgentQuicPeer::setup(
            session_id,
            "the-real-token".to_string(),
            loopback(),
            reap_tx,
        )
        .unwrap();
        let addr: SocketAddr = agent.addrs()[0].parse().unwrap();
        let client = ClientQuicPeer::client(loopback(), agent.cert_fingerprint()).unwrap();
        let conn = client.connect(addr).await.unwrap();
        // Wrong token → agent auth fails → connection closed.
        let _ = quic::client_authenticate(&conn, "WRONG-token").await;

        // No flow should ever be delivered; take_flow times out fast.
        let r = agent.take_flow(7, Duration::from_millis(800)).await;
        assert!(
            r.is_err(),
            "no flow may be served to an unauthenticated client"
        );
        agent.close();
    }

    /// Phase 3d: the agent's QUIC-over-relay path. The agent peer stands
    /// up its quinn server over a [`RelayConn`] (here two loopback UDP
    /// sockets stand in for two coturn allocations — the real
    /// permission-gated TURN path is proven in tunnel-core's
    /// `quinn_runs_over_two_turn_allocations`), reports its relay addr via
    /// `addrs()`, `permit`s the client's relay addr, and a client riding
    /// its own relay socket connects + authenticates + delivers a flow
    /// through the same rendezvous the direct path uses.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_quic_over_relay_delivers_flow() {
        use tunnel_core::transport::relay::UdpRelayConn;

        let agent_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let agent_relay_addr = agent_sock.local_addr().unwrap();
        let client_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_relay_addr = client_sock.local_addr().unwrap();
        let agent_relay: Arc<dyn RelayConn> = Arc::new(UdpRelayConn(agent_sock));
        let client_relay: Arc<dyn RelayConn> = Arc::new(UdpRelayConn(client_sock));

        let session_id = ObjectId::new();
        let token = "relay-session-token";
        let (reap_tx, _reap_rx) = reap_channel();
        let agent = AgentQuicPeer::setup_over_relay(
            session_id,
            token.to_string(),
            Arc::clone(&agent_relay),
            reap_tx,
        )
        .unwrap();
        assert_eq!(
            agent.addrs(),
            vec![agent_relay_addr.to_string()],
            "a relay peer advertises its relayed address (not 0.0.0.0/host)"
        );
        let fingerprint = agent.cert_fingerprint().to_string();

        // What the signaling candidate handler does: permit the client's
        // relay addr (no-op over plain UDP, but exercises the path).
        agent.permit(client_relay_addr).await.unwrap();

        // Client endpoint over ITS relay socket, pinned to the agent cert.
        let csock = Arc::new(RelayUdpSocket::new(Arc::clone(&client_relay)).unwrap());
        let client = ClientQuicPeer::client_over_abstract_socket(csock, &fingerprint).unwrap();

        let (taken, _drive) =
            tokio::join!(agent.take_flow(9, Duration::from_secs(10)), async move {
                let conn = client.connect(agent_relay_addr).await.unwrap();
                quic::client_authenticate(&conn, token).await.unwrap();
                let (mut send, _recv) = quic::open_flow(&conn, 9).await.unwrap();
                send.write_all(b"flow bytes over the relay").await.unwrap();
                send.finish().unwrap();
                // Hold the connection open until the agent has read the flow.
                tokio::time::sleep(Duration::from_millis(300)).await;
            });

        let (_a_send, mut a_recv, _hold) = taken.expect("agent must receive flow 9 over the relay");
        let buf = a_recv.read_to_end(64 * 1024).await.unwrap();
        assert_eq!(
            &buf, b"flow bytes over the relay",
            "flow bytes must survive the relay socket bridge"
        );
        agent.close();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(
        windows,
        ignore = "quinn-udp probes GSO at the first send on a socket it binds to [::]:0, which raises a firewall prompt on Windows (#4500)"
    )]
    async fn a_session_end_stops_a_forward_waiting_for_its_stream() {
        let session_id = ObjectId::new();
        let (reap_tx, _reap_rx) = reap_channel();
        let agent = Arc::new(
            AgentQuicPeer::setup(session_id, "tok".to_string(), loopback(), reap_tx).unwrap(),
        );
        let waiting = Arc::clone(&agent);
        let forward =
            tokio::spawn(async move { waiting.take_flow(5, Duration::from_secs(60)).await });
        tokio::time::sleep(Duration::from_millis(100)).await;

        agent.end();

        let taken = tokio::time::timeout(Duration::from_secs(5), forward)
            .await
            .expect("a forward waiting for its stream returns when the session ends")
            .unwrap();
        assert!(taken.is_err(), "no stream is handed over after the end");
        assert!(
            agent.take_flow(6, Duration::from_secs(60)).await.is_err(),
            "nor to a forward that asks after it"
        );
    }

    #[test]
    fn an_ended_session_is_kept_for_its_ttl_and_the_oldest_goes_first() {
        let t0 = Instant::now();
        let mut ended = EndedQuicSessions::default();
        let first = ObjectId::new();
        let link = SessionLink::default();
        ended.record_at(first, link.clone(), t0);
        assert!(
            ended.end_at(first, t0 + ENDED_SESSION_TTL - Duration::from_secs(1)),
            "a session is reachable inside its TTL"
        );
        assert!(
            !ended.end_at(first, t0),
            "and ending it takes it out, so a second end finds nothing"
        );

        ended.record_at(first, link, t0);
        assert!(
            !ended.end_at(first, t0 + ENDED_SESSION_TTL),
            "past its TTL it is gone"
        );

        let oldest = ObjectId::new();
        ended.record_at(oldest, SessionLink::default(), t0);
        for i in 0..ENDED_SESSIONS_CAP {
            ended.record_at(
                ObjectId::new(),
                SessionLink::default(),
                t0 + Duration::from_millis(1 + i as u64),
            );
        }
        assert_eq!(ended.len(), ENDED_SESSIONS_CAP, "the cap holds");
        assert!(
            !ended.end_at(oldest, t0 + Duration::from_secs(1)),
            "the oldest made room"
        );
    }
}

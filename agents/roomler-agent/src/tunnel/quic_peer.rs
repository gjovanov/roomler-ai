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
//! → `run_flow_quic`. `TunnelTerminate` → [`close`].
//!
//! **Rendezvous** uses two maps so it's order-independent: a stream may
//! arrive before OR after the acceptor registers interest (the client
//! opens the stream right after `TcpForwardAccept`, but the accept loop
//! and the acceptor run concurrently). `waiters` holds pending forwards;
//! `ready` stashes streams that arrived first.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bson::oid::ObjectId;
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, info, warn};
use tunnel_core::transport::quic::{self, QuicPeer, RecvStream, SendStream};
use tunnel_core::transport::relay::{RelayConn, RelayUdpSocket};

/// The two stream halves of one QUIC flow.
pub type FlowStreams = (SendStream, RecvStream);

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
}

/// Spawn the accept loop shared by [`AgentQuicPeer::setup`] and
/// [`AgentQuicPeer::setup_over_relay`]: accept ONE client connection,
/// validate `quic_auth_token`, then rendezvous each inbound flow stream
/// to whichever [`AgentQuicPeer::take_flow`] waiter wants it (stashing
/// streams that arrive before their waiter registers).
fn spawn_accept_loop(
    peer: Arc<QuicPeer>,
    session_id: ObjectId,
    quic_auth_token: String,
    rendezvous: Arc<Mutex<Rendezvous>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
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
        loop {
            match quic::accept_flow(&conn).await {
                Ok((flow_id, send, recv)) => {
                    let mut rdv = rendezvous.lock().await;
                    if let Some(tx) = rdv.waiters.remove(&flow_id) {
                        // A forward is already waiting — hand it over.
                        if tx.send((send, recv)).is_err() {
                            debug!(%session_id, flow_id, "agent quic: forward dropped before stream");
                        }
                    } else {
                        // Stream beat the forward — stash it.
                        rdv.ready.insert(flow_id, (send, recv));
                    }
                }
                Err(e) => {
                    debug!(%session_id, %e, "agent quic: accept_flow loop ended");
                    break;
                }
            }
        }
    })
}

impl AgentQuicPeer {
    /// Bind a quinn server endpoint for this session + spawn the accept
    /// loop. `bind` is the local socket to listen on (`0.0.0.0:0` in
    /// production so all interfaces are reachable; tests use
    /// `127.0.0.1:0`). The loop accepts ONE client connection,
    /// validates `quic_auth_token`, then rendezvouses inbound flow
    /// streams. Ship [`cert_fingerprint`] + [`addrs`] to the client in
    /// `ClientMsg::TunnelQuicReady`.
    pub fn setup(session_id: ObjectId, quic_auth_token: String, bind: SocketAddr) -> Result<Self> {
        let (peer, cert_fingerprint) =
            QuicPeer::server(bind).context("agent quic: server endpoint")?;
        let local_addr = peer.local_addr().context("agent quic: local_addr")?;
        let peer = Arc::new(peer);
        let rendezvous: Arc<Mutex<Rendezvous>> = Arc::new(Mutex::new(Rendezvous::default()));
        let accept_task = spawn_accept_loop(
            Arc::clone(&peer),
            session_id,
            quic_auth_token,
            Arc::clone(&rendezvous),
        );

        Ok(Self {
            session_id,
            cert_fingerprint,
            local_addr,
            rendezvous,
            accept_task,
            _peer: peer,
            relay: None,
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
        let accept_task = spawn_accept_loop(
            Arc::clone(&peer),
            session_id,
            quic_auth_token,
            Arc::clone(&rendezvous),
        );

        Ok(Self {
            session_id,
            cert_fingerprint,
            local_addr,
            rendezvous,
            accept_task,
            _peer: peer,
            relay: Some(relay),
        })
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
        let rx = {
            let mut rdv = self.rendezvous.lock().await;
            if let Some(streams) = rdv.ready.remove(&flow_id) {
                return Ok(streams);
            }
            let (tx, rx) = oneshot::channel();
            rdv.waiters.insert(flow_id, tx);
            rx
        };
        match tokio::time::timeout(timeout, rx).await {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tunnel_core::transport::quic::QuicPeer as ClientQuicPeer;

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    /// Full in-process exercise of the agent QUIC session machinery (no
    /// signaling server): set up the peer, connect a pinned + token-
    /// authed client, open a flow stream, and verify the agent's
    /// `take_flow` rendezvous yields the stream and the bytes arrive.
    /// Covers BOTH rendezvous orderings via the stash + waiter paths.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_quic_rendezvous_delivers_authed_flow() {
        let session_id = ObjectId::new();
        let token = "session-token-xyz";
        let agent = AgentQuicPeer::setup(session_id, token.to_string(), loopback()).unwrap();
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

        let (_a_send, mut a_recv) = taken.expect("agent must receive flow 5's stream");
        // quinn's RecvStream has its own read_to_end(size_limit) → Vec.
        let buf = a_recv.read_to_end(64 * 1024).await.unwrap();
        assert_eq!(
            &buf, b"hello over quic flow",
            "flow bytes must arrive intact"
        );

        agent.close();
    }

    /// A client presenting the WRONG token must NOT get its flow served:
    /// the accept loop closes the connection after auth fails, so
    /// `take_flow` times out (no stream is ever rendezvoused).
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_quic_rejects_bad_token_so_no_flow() {
        let session_id = ObjectId::new();
        let agent =
            AgentQuicPeer::setup(session_id, "the-real-token".to_string(), loopback()).unwrap();
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
        let agent = AgentQuicPeer::setup_over_relay(
            session_id,
            token.to_string(),
            Arc::clone(&agent_relay),
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

        let (_a_send, mut a_recv) = taken.expect("agent must receive flow 9 over the relay");
        let buf = a_recv.read_to_end(64 * 1024).await.unwrap();
        assert_eq!(
            &buf, b"flow bytes over the relay",
            "flow bytes must survive the relay socket bridge"
        );
        agent.close();
    }
}

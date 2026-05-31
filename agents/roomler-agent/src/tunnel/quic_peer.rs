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

        let peer_task = Arc::clone(&peer);
        let rdv_task = Arc::clone(&rendezvous);
        let accept_task = tokio::spawn(async move {
            let conn = match peer_task.accept().await {
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
            // The server is no longer in the byte path — this token is
            // what authorizes the dialing client (cert-pinning already
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
                        let mut rdv = rdv_task.lock().await;
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
        });

        Ok(Self {
            session_id,
            cert_fingerprint,
            local_addr,
            rendezvous,
            accept_task,
            _peer: peer,
        })
    }

    /// SHA-256 fingerprint (hex) of the ephemeral cert — pinned by the
    /// client, shipped in `ClientMsg::TunnelQuicReady`.
    pub fn cert_fingerprint(&self) -> &str {
        &self.cert_fingerprint
    }

    /// Dialable candidate addresses for the client.
    ///
    /// Phase 1 (direct-reachable) returns the bound local address. When
    /// bound to `0.0.0.0`, that is NOT directly dialable — Phase 2 will
    /// enumerate non-loopback interface IPs and add STUN
    /// server-reflexive candidates here. Today this is correct for the
    /// loopback test and for hosts dialed by a directly-reachable IP.
    pub fn addrs(&self) -> Vec<String> {
        vec![self.local_addr.to_string()]
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
}

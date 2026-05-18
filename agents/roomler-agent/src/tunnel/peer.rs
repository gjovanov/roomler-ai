//! Agent-side answerer for the `roomler-tunnel` WebRTC handshake.
//!
//! Mirror of `roomler-tunnel`'s offerer-side [`TunnelPeer`] usage —
//! same crate type, just the answerer half of the handshake. One
//! `AgentTunnelPeer` per active tunnel session (server-issued
//! `tunnel_session_id`).
//!
//! Lifecycle:
//!
//! 1. Server emits `ServerMsg::TunnelSdpOffer { session_id, sdp }`.
//!    Agent's signaling loop calls
//!    [`AgentTunnelPeer::accept_offer`] which constructs the peer,
//!    installs the ICE-candidate forwarder, sets remote-describe,
//!    generates the answer, and returns it for the caller to ship
//!    as `ClientMsg::TunnelSdpAnswer`.
//! 2. Server trickles ICE via `ServerMsg::TunnelIce`. Agent calls
//!    [`AgentTunnelPeer::add_remote_ice`] for each.
//! 3. The DC pool opens (both ends pre-negotiated identical stream
//!    ids). A background task `await`s
//!    [`tunnel_core::transport::webrtc_dc::TunnelPeer::wait_pool_open`],
//!    then installs a [`FlowDemux`] on each DC and parks them in the
//!    `flow_demuxes` field so the acceptor can register per-flow
//!    mailboxes.
//! 4. Server emits `ServerMsg::TcpForwardForward` for each new flow.
//!    `crate::tunnel::acceptor::handle_forward_request` consults the
//!    ACL, dials dst, calls [`AgentTunnelPeer::register_flow`] to
//!    bind the flow to a DC, and spawns
//!    `tunnel_core::forward::run_flow` to drive the bytes.
//! 5. Either side tears down the tunnel via `TunnelTerminate` →
//!    [`AgentTunnelPeer::close`] drops every flow + DC.

use std::sync::Arc;

use bson::oid::ObjectId;
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};
use tunnel_core::forward::FlowDemux;
use tunnel_core::transport::webrtc_dc::{PeerError, TunnelPeer};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;

/// Per-session answerer state. Cheap to construct (it just wraps the
/// peer) — heavy work happens inside [`accept_offer`] and the
/// background `wait_pool_open` task.
pub struct AgentTunnelPeer {
    session_id: ObjectId,
    peer: Arc<TunnelPeer>,
    /// `FlowDemux` per DC index. Populated by the `wait_pool_open`
    /// task once every DC reaches `open`. Empty until then.
    flow_demuxes: Arc<Mutex<Vec<FlowDemux>>>,
    /// Resolves once the pool is fully open and `flow_demuxes` is
    /// populated. Cloned out via [`pool_ready`] so the acceptor can
    /// wait for it before serving the first flow.
    pool_ready: Arc<tokio::sync::Notify>,
}

impl AgentTunnelPeer {
    /// Build the peer + accept the SDP offer + generate an SDP answer
    /// in one step. `ice_servers` is forwarded verbatim from the
    /// server's `TunnelOpened` (the server collects them centrally).
    /// `outbound_tx` is the agent's WS outbound channel — the peer
    /// uses it to trickle local ICE candidates back to the server.
    pub async fn accept_offer(
        session_id: ObjectId,
        offer_sdp: &str,
        ice_servers: Vec<IceServer>,
        outbound_tx: mpsc::Sender<ClientMsg>,
    ) -> Result<(Self, String), PeerError> {
        let rtc_ice_servers: Vec<RTCIceServer> = ice_servers
            .into_iter()
            .map(|s| RTCIceServer {
                urls: s.urls,
                username: s.username.unwrap_or_default(),
                credential: s.credential.unwrap_or_default(),
            })
            .collect();
        let peer = Arc::new(TunnelPeer::new(rtc_ice_servers).await?);

        // Trickle local candidates → outbound channel as
        // `rc:tunnel.ice`. Drop is fine on a closed channel — means
        // the WS is gone, and the next state transition will tear
        // the peer down.
        {
            let outbound = outbound_tx.clone();
            peer.on_local_ice_candidate(move |c| {
                let outbound = outbound.clone();
                Box::pin(async move {
                    let Some(c) = c else {
                        return;
                    };
                    let init = match c.to_json() {
                        Ok(i) => i,
                        Err(e) => {
                            warn!(%e, "local candidate to_json failed");
                            return;
                        }
                    };
                    let candidate = match serde_json::to_value(&init) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(%e, "local candidate serialise failed");
                            return;
                        }
                    };
                    if let Err(e) = outbound
                        .send(ClientMsg::TunnelIce {
                            session_id,
                            candidate,
                        })
                        .await
                    {
                        debug!(%session_id, %e, "tunnel ICE trickle dropped (channel closed)");
                    }
                })
            });
        }

        // Generate the answer. set_local_description happens inside.
        let answer = peer.accept_offer(offer_sdp).await?;

        let flow_demuxes = Arc::new(Mutex::new(Vec::new()));
        let pool_ready = Arc::new(tokio::sync::Notify::new());

        // Background task: wait for the DC pool to open, then build
        // the FlowDemuxes. Spawned here so the SDP/ICE path doesn't
        // block on the long async wait_pool_open call.
        let demuxes_for_task = Arc::clone(&flow_demuxes);
        let pool_ready_for_task = Arc::clone(&pool_ready);
        let peer_for_task = Arc::clone(&peer);
        tokio::spawn(async move {
            match peer_for_task.wait_pool_open().await {
                Ok(()) => {
                    let pool_size = peer_for_task.pool_size();
                    let mut demuxes = Vec::with_capacity(pool_size as usize);
                    for idx in 0..pool_size {
                        let Some(dc) = peer_for_task.dc(idx) else {
                            warn!(%session_id, idx, "pool_open succeeded but dc({idx}) None — pool corrupt");
                            return;
                        };
                        demuxes.push(FlowDemux::install(dc).await);
                    }
                    *demuxes_for_task.lock().await = demuxes;
                    info!(%session_id, pool_size, "agent tunnel DC pool open + demuxes installed");
                    pool_ready_for_task.notify_waiters();
                }
                Err(e) => {
                    warn!(%session_id, %e, "agent tunnel pool failed to open");
                }
            }
        });

        Ok((
            Self {
                session_id,
                peer,
                flow_demuxes,
                pool_ready,
            },
            answer.sdp,
        ))
    }

    /// Forward a remote ICE candidate from the server into the peer.
    pub async fn add_remote_ice(&self, candidate: serde_json::Value) -> Result<(), PeerError> {
        let init: RTCIceCandidateInit = serde_json::from_value(candidate).map_err(|e| {
            PeerError::InvalidIceCandidate(format!("candidate JSON shape mismatch: {e}"))
        })?;
        self.peer.add_remote_ice_candidate(init).await
    }

    /// Wait until the DC pool is fully open + every demux is
    /// installed. Idempotent; resolves immediately if already ready.
    pub async fn wait_pool_ready(&self, timeout: std::time::Duration) -> bool {
        if !self.flow_demuxes.lock().await.is_empty() {
            return true;
        }
        let notified = self.pool_ready.notified();
        tokio::pin!(notified);
        let waited = tokio::time::timeout(timeout, notified.as_mut()).await;
        if waited.is_err() {
            return false;
        }
        // Double-check — `notify_waiters` only wakes pending waiters,
        // not new ones, so a slow consumer that registered between
        // wake + check could see an empty Vec. Re-read to be sure.
        !self.flow_demuxes.lock().await.is_empty()
    }

    /// Number of DCs in the pool. Stable at [`tunnel_core::transport::
    /// webrtc_dc::POOL_SIZE`] once the pool opens; 0 before that.
    pub async fn pool_size(&self) -> u8 {
        self.flow_demuxes.lock().await.len() as u8
    }

    /// Borrow the [`FlowDemux`] for `dc_index`. None if the pool
    /// hasn't fully opened yet OR the index is out of range. Caller
    /// should use [`wait_pool_ready`] before invoking.
    pub async fn demux(&self, dc_index: u8) -> Option<FlowDemux> {
        let guard = self.flow_demuxes.lock().await;
        guard.get(dc_index as usize).cloned()
    }

    /// Build a `HalfCloseSink` that emits `ClientMsg::TcpHalfClose
    /// { direction: DstToSrc }` over the agent's WS outbound channel.
    /// The agent's "local" side of a flow is the dialed destination,
    /// so when its TCP read half hits EOF the half-close direction is
    /// always `DstToSrc` (destination → source = agent → client).
    pub fn half_close_sink(
        &self,
        outbound_tx: mpsc::Sender<ClientMsg>,
    ) -> tunnel_core::forward::HalfCloseSink {
        let session_id = self.session_id;
        Arc::new(move |flow_id: u32| {
            let outbound = outbound_tx.clone();
            tokio::spawn(async move {
                let _ = outbound
                    .send(ClientMsg::TcpHalfClose {
                        session_id,
                        flow_id,
                        direction: roomler_ai_remote_control::signaling::Direction::DstToSrc,
                    })
                    .await;
            });
        })
    }

    /// Close the peer + drop every flow. Idempotent. Caller is
    /// responsible for removing the peer from any agent-side session
    /// map before calling; this is just the resource teardown.
    pub async fn close(&self) {
        if let Err(e) = self.peer.peer_connection().close().await {
            debug!(session_id = %self.session_id, %e, "TunnelPeer close errored");
        }
        self.flow_demuxes.lock().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tunnel_core::transport::webrtc_dc::TunnelPeer as CoreTunnelPeer;

    /// Drive the full offerer (= CLI's role) + answerer (= this
    /// module's role) handshake locally with no signaling server.
    /// Bridges ICE by draining the answerer's outbound channel and
    /// installing a closure on the offerer. Verifies the answerer
    /// reaches `pool_ready` and exposes a non-empty pool.
    #[tokio::test(flavor = "multi_thread")]
    async fn answerer_reaches_pool_ready() {
        let session_id = ObjectId::new();

        // Build the offerer locally — same code path as the CLI.
        let offerer = CoreTunnelPeer::new(vec![]).await.unwrap();
        let offer = offerer.create_offer().await.unwrap();

        // Outbound channel the answerer uses to trickle its local
        // ICE candidates. Drain task below bridges them into the
        // offerer.
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<ClientMsg>(64);

        let (answerer_peer, answer_sdp) =
            AgentTunnelPeer::accept_offer(session_id, &offer.sdp, vec![], outbound_tx.clone())
                .await
                .unwrap();
        assert!(!answer_sdp.is_empty(), "answer SDP must be non-empty");

        offerer.accept_answer(&answer_sdp).await.unwrap();

        // Bridge offerer → answerer ICE candidates.
        let answerer_pc = answerer_peer.peer.peer_connection();
        offerer.on_local_ice_candidate(move |c| {
            let pc = Arc::clone(&answerer_pc);
            Box::pin(async move {
                if let Some(c) = c
                    && let Ok(init) = c.to_json()
                {
                    let _ = pc.add_ice_candidate(init).await;
                }
            })
        });

        // Drain answerer → offerer ICE candidates (the answerer's
        // accept_offer installed a closure that pushes them into
        // outbound_tx as ClientMsg::TunnelIce).
        let offerer_pc = offerer.peer_connection();
        let drain = tokio::spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                if let ClientMsg::TunnelIce { candidate, .. } = msg
                    && let Ok(init) = serde_json::from_value::<RTCIceCandidateInit>(candidate)
                {
                    let _ = offerer_pc.add_ice_candidate(init).await;
                }
            }
        });

        let ready = answerer_peer.wait_pool_ready(Duration::from_secs(15)).await;
        drain.abort();

        assert!(ready, "answerer pool did not reach ready within 15s");
        assert!(answerer_peer.pool_size().await > 0);
        assert!(answerer_peer.demux(0).await.is_some());
    }
}

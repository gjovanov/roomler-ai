//! Thin wrapper around a `webrtc-rs` `RTCPeerConnection`.
//!
//! Scope for this commit is *signaling-complete, media-empty*: the peer can
//! answer an SDP offer, trickle ICE in both directions, and accept data
//! channels opened by the browser controller (they're logged but not yet
//! used). No video/audio track is produced — screen capture and H.264
//! encoding land in a follow-up.
//!
//! Why that split: getting SDP + ICE correct is half the WebRTC battle and
//! is testable against a browser immediately. Adding a real video track
//! requires picking an encoder (openh264 / nvenc / vaapi) and feeding raw
//! NALUs into a `TrackLocalStaticSample`, which has its own debugging arc.

use anyhow::{Context, Result, anyhow};
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

/// A single remote-control session's peer. One of these exists per live
/// [`roomler_ai_remote_control::models::RemoteSession`] on the agent side.
pub struct AgentPeer {
    pc: Arc<RTCPeerConnection>,
    session_id: bson::oid::ObjectId,
}

impl AgentPeer {
    /// Build a new PC, register ICE + state callbacks that forward events
    /// through `outbound` as `ClientMsg` values (ICE candidates, terminate
    /// on fatal state). The caller is expected to pump `outbound` into the
    /// WebSocket.
    pub async fn new(
        session_id: bson::oid::ObjectId,
        ice_servers: &[IceServer],
        outbound: mpsc::Sender<ClientMsg>,
    ) -> Result<Self> {
        let mut engine = MediaEngine::default();
        engine
            .register_default_codecs()
            .context("register default codecs")?;

        let api = APIBuilder::new().with_media_engine(engine).build();

        let config = RTCConfiguration {
            ice_servers: map_ice_servers(ice_servers),
            ..Default::default()
        };

        let pc = Arc::new(
            api.new_peer_connection(config)
                .await
                .context("new_peer_connection")?,
        );

        // Forward locally-gathered ICE candidates back through the signaling
        // channel so the browser can trickle them into its remote description.
        {
            let tx = outbound.clone();
            pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                let tx = tx.clone();
                Box::pin(async move {
                    let Some(c) = c else { return };
                    let json = match c.to_json() {
                        Ok(j) => j,
                        Err(e) => {
                            warn!(%e, "failed to serialize ICE candidate");
                            return;
                        }
                    };
                    let Ok(candidate) = serde_json::to_value(&json) else { return };
                    let _ = tx
                        .send(ClientMsg::Ice {
                            session_id,
                            candidate,
                        })
                        .await;
                })
            }));
        }

        // Log connection state transitions so operators can see what's
        // happening during a session. On `Failed` / `Disconnected` we ask
        // the server to tear the session down; `Closed` is terminal and
        // driven by either side.
        {
            let tx = outbound.clone();
            pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                info!(session = %session_id, state = ?s, "PC state change");
                let tx = tx.clone();
                Box::pin(async move {
                    if matches!(s, RTCPeerConnectionState::Failed) {
                        let _ = tx
                            .send(ClientMsg::Terminate {
                                session_id,
                                reason: roomler_ai_remote_control::models::EndReason::Error,
                            })
                            .await;
                    }
                })
            }));
        }

        // Accept data channels opened by the controller. For now we just
        // log what comes in; future commits will attach:
        //   - "input"     → InputInjector
        //   - "control"   → cursor overlay, keyframe requests
        //   - "clipboard" → clipboard sync
        //   - "files"     → chunked file transfer
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let label = dc.label().to_string();
            info!(session = %session_id, %label, "data channel opened (not yet wired)");
            Box::pin(async move {
                dc.on_message(Box::new(move |msg| {
                    debug!(bytes = msg.data.len(), "data channel message (dropped)");
                    Box::pin(async {})
                }));
            })
        }));

        Ok(Self { pc, session_id })
    }

    /// Set the remote offer and produce our answer SDP. Local description
    /// gets set too so `on_ice_candidate` starts firing immediately.
    pub async fn handle_offer(&self, offer_sdp: String) -> Result<String> {
        let offer = RTCSessionDescription::offer(offer_sdp).context("parse offer")?;
        self.pc
            .set_remote_description(offer)
            .await
            .context("set_remote_description")?;

        let answer = self
            .pc
            .create_answer(None)
            .await
            .context("create_answer")?;
        self.pc
            .set_local_description(answer.clone())
            .await
            .context("set_local_description")?;

        // The caller sends this over the signaling channel as `rc:sdp.answer`.
        Ok(answer.sdp)
    }

    /// Feed a remote ICE candidate. Safe to call before `handle_offer`
    /// completes — webrtc-rs buffers these until the remote description
    /// is set.
    pub async fn add_remote_candidate(&self, candidate: serde_json::Value) -> Result<()> {
        // The browser sends either the `RTCIceCandidateInit` object shape
        // (`{candidate, sdpMid, sdpMLineIndex, usernameFragment}`) or a
        // bare string. Normalise.
        let init: RTCIceCandidateInit = match candidate {
            serde_json::Value::String(s) => RTCIceCandidateInit {
                candidate: s,
                ..Default::default()
            },
            other => serde_json::from_value(other)
                .map_err(|e| anyhow!("bad ICE candidate shape: {e}"))?,
        };
        self.pc
            .add_ice_candidate(init)
            .await
            .context("add_ice_candidate")
    }

    pub async fn close(&self) {
        if let Err(e) = self.pc.close().await {
            warn!(session = %self.session_id, %e, "PC close failed");
        }
    }
}

fn map_ice_servers(servers: &[IceServer]) -> Vec<RTCIceServer> {
    servers
        .iter()
        .map(|s| RTCIceServer {
            urls: s.urls.clone(),
            username: s.username.clone().unwrap_or_default(),
            credential: s.credential.clone().unwrap_or_default(),
        })
        .collect()
}

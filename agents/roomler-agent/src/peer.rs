//! Thin wrapper around a `webrtc-rs` `RTCPeerConnection`.
//!
//! Owns the per-session WebRTC state: codecs, ICE, data channels, and (when
//! a capture/encoder backend is compiled in) a video track that's fed from
//! a spawned media pump task.
//!
//! Media pump lifecycle:
//!   1. On new(): add an `H264` track and spawn the pump.
//!   2. The pump asks `capture::open_default` for frames; if the build
//!      doesn't include `scrap-capture`, it gets a NoopCapture and never
//!      emits anything — track is added but carries no samples. The
//!      browser still negotiates the m=video section.
//!   3. On each frame, `encode::open_default` produces H.264 NALUs that
//!      become a `webrtc::media::Sample`. Sample duration is derived from
//!      the capture rate.
//!   4. On close(): cancels the pump, closes the PC.

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::media::Sample;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::capture;
use crate::encode;
use crate::input;

/// Target capture rate. Aligns the sample pacing sent to the browser with
/// how fast the capturer emits frames.
const TARGET_FPS: u32 = 30;

pub struct AgentPeer {
    pc: Arc<RTCPeerConnection>,
    session_id: bson::oid::ObjectId,
    media_pump: Option<JoinHandle<()>>,
}

impl AgentPeer {
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

        // Add a sendonly H.264 video track up front so the SDP answer
        // advertises it. webrtc-rs picks compatible H.264 params from the
        // MediaEngine's default codec list.
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: "video/H264".to_string(),
                ..Default::default()
            },
            "video".to_string(),
            "roomler-agent".to_string(),
        ));
        pc.add_track(video_track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("add_track(video)")?;

        // Forward locally-gathered ICE candidates.
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

        // PC state → logs + fatal Terminate on Failed.
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

        // Route data channels by label. `input` goes to the OS injector;
        // the others (control/clipboard/files) are accepted but not yet
        // wired — they'll get their own handlers in follow-up phases.
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let label = dc.label().to_string();
            info!(session = %session_id, %label, "data channel opened");
            Box::pin(async move {
                match label.as_str() {
                    "input" => attach_input_handler(dc),
                    _ => attach_log_only(dc, session_id),
                }
            })
        }));

        // Start the capture→encode→track pump. The pump is self-regulating:
        // with no capture backend compiled in, open_default returns a Noop
        // that parks forever, producing no samples.
        let pump = tokio::spawn(media_pump(session_id, video_track));

        Ok(Self {
            pc,
            session_id,
            media_pump: Some(pump),
        })
    }

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

        Ok(answer.sdp)
    }

    pub async fn add_remote_candidate(&self, candidate: serde_json::Value) -> Result<()> {
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
        if let Some(pump) = &self.media_pump {
            pump.abort();
        }
        if let Err(e) = self.pc.close().await {
            warn!(session = %self.session_id, %e, "PC close failed");
        }
    }
}

/// Per-session media pump. Captures frames, encodes to H.264, writes
/// Samples into the WebRTC track. Rebuilds the encoder if the capture
/// resolution changes mid-session (e.g. dock/undock).
async fn media_pump(session_id: bson::oid::ObjectId, track: Arc<TrackLocalStaticSample>) {
    let mut capturer = capture::open_default(TARGET_FPS);
    let mut encoder: Option<Box<dyn encode::VideoEncoder>> = None;
    let mut encoder_dims: Option<(u32, u32)> = None;
    let frame_duration = Duration::from_micros(1_000_000 / TARGET_FPS as u64);

    loop {
        let frame = match capturer.next_frame().await {
            Ok(Some(f)) => f,
            Ok(None) => {
                // No frame produced within the backend's internal budget;
                // keep looping so shutdown via task abort() lands quickly.
                continue;
            }
            Err(e) => {
                warn!(%session_id, %e, "capture error — stopping media pump");
                return;
            }
        };

        // (Re)build the encoder if the frame dimensions change.
        if encoder_dims != Some((frame.width, frame.height)) {
            info!(
                %session_id,
                w = frame.width, h = frame.height,
                "initialising encoder for frame dims"
            );
            encoder = Some(encode::open_default(frame.width, frame.height));
            encoder_dims = Some((frame.width, frame.height));
        }

        let enc = encoder.as_mut().unwrap();
        let packets = match enc.encode(frame).await {
            Ok(p) => p,
            Err(e) => {
                warn!(%session_id, %e, "encode error — stopping media pump");
                return;
            }
        };

        for p in packets {
            let sample = Sample {
                data: Bytes::from(p.data),
                timestamp: SystemTime::now(),
                duration: frame_duration,
                packet_timestamp: 0,
                prev_dropped_packets: 0,
                prev_padding_packets: 0,
            };
            if let Err(e) = track.write_sample(&sample).await {
                // Benign when the PC hasn't finished ICE yet — drop and carry on.
                debug!(%session_id, %e, "write_sample");
            }
        }
    }
}

/// Attach the `input` data-channel message handler. Each inbound payload
/// is parsed as [`input::InputMsg`] and injected via the thread-pinned
/// OS backend. The injector is built once per channel (the first frame
/// may race with initialisation, but `open_default` is synchronous so
/// it's ready before the first real keystroke).
///
/// Unparseable payloads are dropped with a debug log — we don't want a
/// flood of warnings if the controller sends an unknown event type.
fn attach_input_handler(dc: Arc<RTCDataChannel>) {
    // Injector is wrapped in `parking_lot::Mutex`-equivalent-style — we
    // don't have parking_lot imported here, so fall back to tokio's
    // Mutex. The inject() call is fast (just a channel send), so lock
    // contention is not a concern.
    let injector = std::sync::Arc::new(tokio::sync::Mutex::new(input::open_default()));
    dc.on_message(Box::new(move |msg| {
        let injector = injector.clone();
        Box::pin(async move {
            let Ok(text) = std::str::from_utf8(&msg.data) else {
                debug!("input: non-utf8 payload dropped");
                return;
            };
            let parsed: input::InputMsg = match serde_json::from_str(text) {
                Ok(v) => v,
                Err(e) => {
                    debug!(%e, "input: parse failed");
                    return;
                }
            };
            let mut guard = injector.lock().await;
            if let Err(e) = guard.inject(parsed) {
                debug!(%e, "input: inject failed");
            }
        })
    }));
}

/// Placeholder handler for data channels that aren't wired to OS output
/// yet (`control`, `clipboard`, `files`). Logs message sizes so we can
/// see activity without spamming the log with contents.
fn attach_log_only(dc: Arc<RTCDataChannel>, session_id: bson::oid::ObjectId) {
    let label = dc.label().to_string();
    dc.on_message(Box::new(move |msg| {
        debug!(%session_id, %label, bytes = msg.data.len(), "DC msg (unhandled)");
        Box::pin(async {})
    }));
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

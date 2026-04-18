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
use webrtc::rtp_transceiver::RTCPFeedback;
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

        // Install NACK responder + TWCC + RTCP reports. Without these
        // interceptors the sender silently drops NACK retransmit requests,
        // so any lost RTP packet becomes a frozen decoder until the next
        // IDR. Browser observed 293 NACKs per minute with 0.1.4 going
        // nowhere — this is the missing piece.
        let mut registry = webrtc::interceptor::registry::Registry::new();
        registry = webrtc::api::interceptor_registry::register_default_interceptors(
            registry, &mut engine,
        )
        .context("register default interceptors")?;

        let api = APIBuilder::new()
            .with_media_engine(engine)
            .with_interceptor_registry(registry)
            .build();

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
        // advertises it. Match one of webrtc-rs's default H.264 codec
        // registrations exactly (clock_rate + fmtp line + rtcp_feedback),
        // otherwise the packetizer can't resolve a payload type for the
        // outgoing RTP and (even worse) NACK/PLI aren't negotiated so the
        // browser's retransmit requests and keyframe asks are ignored —
        // the stream freezes on every lost packet for ~10 s until openh264
        // emits its next natural IDR. Chosen: Constrained Baseline,
        // packetization-mode=1, profile-level-id=42e01f — matches payload
        // type 125 in webrtc-rs's default MediaEngine.
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: "video/H264".to_string(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                        .to_string(),
                rtcp_feedback: vec![
                    RTCPFeedback { typ: "goog-remb".to_string(),    parameter: String::new() },
                    RTCPFeedback { typ: "ccm".to_string(),          parameter: "fir".to_string() },
                    RTCPFeedback { typ: "nack".to_string(),         parameter: String::new() },
                    RTCPFeedback { typ: "nack".to_string(),         parameter: "pli".to_string() },
                    RTCPFeedback { typ: "transport-cc".to_string(), parameter: String::new() },
                ],
            },
            "video".to_string(),
            "roomler-agent".to_string(),
        ));
        let video_sender = pc
            .add_track(video_track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("add_track(video)")?;

        // Shared keyframe-request flag. The RTCP reader task flips it on
        // PLI / FIR; media_pump consumes it before each encode and calls
        // force_intra_frame() on the openh264 encoder. Without this, lost
        // packets freeze the decoder until the next periodic IDR.
        let keyframe_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let flag = keyframe_requested.clone();
            let sid = session_id;
            tokio::spawn(async move {
                use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
                use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
                loop {
                    match video_sender.read_rtcp().await {
                        Ok((pkts, _)) => {
                            for p in pkts {
                                let p_any = p.as_any();
                                if p_any.downcast_ref::<PictureLossIndication>().is_some()
                                    || p_any.downcast_ref::<FullIntraRequest>().is_some()
                                {
                                    info!(session = %sid, "PLI/FIR received → forcing keyframe");
                                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                        }
                        Err(_e) => {
                            // Sender closed; exit the reader.
                            return;
                        }
                    }
                }
            });
        }

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
        let pump = tokio::spawn(media_pump(session_id, video_track, keyframe_requested));

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
async fn media_pump(
    session_id: bson::oid::ObjectId,
    track: Arc<TrackLocalStaticSample>,
    keyframe_requested: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut capturer = capture::open_default(TARGET_FPS);
    let mut encoder: Option<Box<dyn encode::VideoEncoder>> = None;
    let mut encoder_dims: Option<(u32, u32)> = None;
    // Floor on the `duration` field of each Sample. DXGI Desktop Duplication
    // only emits a frame when the screen changes, so on an idle desktop the
    // real gap between two write_sample calls can be seconds. RTP timestamp
    // increments are `duration * clock_rate`; if duration stays at 33 ms
    // (30 fps nominal) while wallclock advances by 1 s, the browser's
    // playout clock starves and the video element goes black. Measure the
    // wallclock gap per frame and use that as the duration — the first
    // sample uses the nominal floor.
    let frame_duration_floor = Duration::from_micros(1_000_000 / TARGET_FPS as u64);
    let mut last_sample_at: Option<std::time::Instant> = None;

    // Observability: count frames in/out and bytes written, log every 30
    // encoded frames (~once per second at 30fps). Without this a silent
    // stall in capture or encode is indistinguishable from a working pump.
    let mut frames_captured: u64 = 0;
    let mut frames_empty: u64 = 0;
    let mut frames_encoded: u64 = 0;
    let mut bytes_written: u64 = 0;
    let mut write_errors: u64 = 0;

    loop {
        let frame = match capturer.next_frame().await {
            Ok(Some(f)) => {
                frames_captured += 1;
                f
            }
            Ok(None) => {
                frames_empty += 1;
                // Log every ~5s worth of empty polls so an idle desktop is
                // visible without flooding. DXGI only fires on screen change,
                // so this can spike briefly then settle.
                if frames_empty % 150 == 0 {
                    info!(%session_id, frames_empty, "capture produced no frame (idle screen)");
                }
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
        if keyframe_requested.swap(false, std::sync::atomic::Ordering::Relaxed) {
            enc.request_keyframe();
        }
        let packets = match enc.encode(frame).await {
            Ok(p) => p,
            Err(e) => {
                warn!(%session_id, %e, "encode error — stopping media pump");
                return;
            }
        };

        // Wallclock-based duration so RTP timestamps advance at real time,
        // not at an assumed 30 fps. First sample falls back to the nominal
        // floor (the track has nothing to reference from).
        let now = std::time::Instant::now();
        // Clamp: floor at the nominal frame duration, cap at 1 s so a
        // multi-second idle doesn't cause an enormous RTP timestamp jump.
        let wallclock_gap = match last_sample_at {
            Some(t) => now
                .duration_since(t)
                .clamp(frame_duration_floor, Duration::from_secs(1)),
            None => frame_duration_floor,
        };
        last_sample_at = Some(now);

        let mut packet_bytes: u64 = 0;
        for p in packets {
            packet_bytes += p.data.len() as u64;
            let sample = Sample {
                data: Bytes::from(p.data),
                timestamp: SystemTime::now(),
                duration: wallclock_gap,
                packet_timestamp: 0,
                prev_dropped_packets: 0,
                prev_padding_packets: 0,
            };
            if let Err(e) = track.write_sample(&sample).await {
                write_errors += 1;
                // Elevated from debug — silent drops were hiding the real
                // problem during first-bringup on Windows.
                warn!(%session_id, %e, write_errors, "write_sample failed");
            }
        }

        frames_encoded += 1;
        bytes_written += packet_bytes;

        if frames_encoded == 1 {
            info!(%session_id, first_frame_bytes = packet_bytes, "first encoded frame written to track");
        }
        if frames_encoded.is_multiple_of(30) {
            info!(
                %session_id,
                frames_captured, frames_empty, frames_encoded,
                bytes_written, write_errors,
                "media pump heartbeat (≈1s window)"
            );
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

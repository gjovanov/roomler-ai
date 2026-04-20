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

/// Quality preference advertised by the controller over the `control`
/// data channel. Encoded as `AtomicU8` so the media pump can poll it
/// per-frame without locking. Translated to a bitrate clamp on the
/// active encoder; future revisions may also clamp fps and downscale
/// when capture-side knobs (1F.1) are wired through.
mod quality {
    pub(super) const AUTO: u8 = 0;
    pub(super) const LOW: u8 = 1;
    pub(super) const HIGH: u8 = 2;

    /// Parse the wire-format string into the atomic value. Anything
    /// unrecognised maps to `AUTO` and is logged by the caller.
    pub(super) fn from_wire(s: &str) -> Option<u8> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Some(LOW),
            "auto" => Some(AUTO),
            "high" => Some(HIGH),
            _ => None,
        }
    }

    pub(super) fn label(v: u8) -> &'static str {
        match v {
            LOW => "low",
            HIGH => "high",
            _ => "auto",
        }
    }

    /// Map a quality preference to the bitrate target, scaled off the
    /// resolution-derived baseline. Low halves it (better fit for
    /// metered uplinks), High adds 50% (capped at 20 Mbps to keep the
    /// VBV buffer math reasonable on 4K).
    pub(super) fn target_bitrate(quality: u8, base_bps: u32) -> u32 {
        const MAX_HIGH_BPS: u32 = 20_000_000;
        match quality {
            LOW => (base_bps / 2).max(500_000),
            HIGH => base_bps.saturating_mul(3) / 2,
            _ => base_bps,
        }
        .min(MAX_HIGH_BPS)
    }
}

/// Pick the capture downscale policy consistent with an encoder
/// preference. HW encoders can eat 4K frames without breaking a sweat;
/// SW openh264 needs the 2× downsample to stay above ~30 fps at 1080p,
/// and can barely do 10 fps at native 4K without it.
fn downscale_for(pref: encode::EncoderPreference) -> capture::DownscalePolicy {
    match pref {
        encode::EncoderPreference::Software => capture::DownscalePolicy::Auto,
        encode::EncoderPreference::Hardware => capture::DownscalePolicy::Never,
        encode::EncoderPreference::Auto => {
            // Hardware MFT activation on mixed-GPU systems (e.g.
            // NVIDIA + Intel iGPU) still hits vendor-specific issues
            // — NVIDIA needs an explicit DXGI adapter match, Intel
            // QSV needs the async event loop. Both are phase 3 work.
            // Until then, Auto → Auto-downscale even on Windows, so
            // the MS SW MFT runs at 1080p where it sustains 30 fps.
            capture::DownscalePolicy::Auto
        }
    }
}

pub struct AgentPeer {
    pc: Arc<RTCPeerConnection>,
    session_id: bson::oid::ObjectId,
    media_pump: Option<JoinHandle<()>>,
    /// Reads RTCP from the video sender to handle PLI/FIR. Held so that
    /// `close()` can abort it — otherwise it outlives the AgentPeer and
    /// leaks under session churn until `video_sender.read_rtcp()` errors
    /// on its own, which isn't guaranteed to happen promptly.
    rtcp_reader: Option<JoinHandle<()>>,
}

impl AgentPeer {
    pub async fn new(
        session_id: bson::oid::ObjectId,
        ice_servers: &[IceServer],
        outbound: mpsc::Sender<ClientMsg>,
        encoder_preference: encode::EncoderPreference,
        chosen_codec: String,
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

        // Add a sendonly video track up front so the SDP answer
        // advertises it. The `chosen_codec` (`"h264"` / `"h265"`) is the
        // intersection result from `caps::pick_best_codec(browser,
        // agent)` computed in signaling. The capability selected here
        // must match one of webrtc-rs's `register_default_codecs`
        // entries byte-for-byte on clock_rate + fmtp line +
        // rtcp_feedback, otherwise the SDP negotiation fails to resolve
        // a payload type and the packetizer has nothing to emit.
        //
        // webrtc-rs's default H.265 registration is PT 126, no fmtp
        // line, same rtcp feedback as H.264 — matches Chrome
        // Canary/Beta/Stable 127+ which accept the same shape.
        let video_track = Arc::new(TrackLocalStaticSample::new(
            build_video_codec_cap(&chosen_codec),
            "video".to_string(),
            "roomler-agent".to_string(),
        ));
        let video_sender = pc
            .add_track(video_track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("add_track(video)")?;

        // Pin the SDP answer's m=video codec list to the chosen codec.
        // Without this, webrtc-rs offers H.264 + H.265 + AV1 + VP8 + VP9
        // in one m-section, and a browser free to pick its first
        // preference may negotiate a codec our encoder doesn't emit
        // (e.g. VP9 from Firefox). set_codec_preferences on the
        // transceiver filters the offered codec list in the SDP.
        // Find the transceiver that owns the sender we just created.
        // `t.sender()` returns a Future<Output = Arc<RTCRtpSender>>, so
        // the candidates have to be awaited one at a time inside the
        // loop. There's typically only one transceiver at this point
        // (we just added the single video track), so this is cheap.
        let mut matched_transceiver = None;
        for t in pc.get_transceivers().await {
            let sender = t.sender().await;
            if std::sync::Arc::ptr_eq(&sender, &video_sender) {
                matched_transceiver = Some(t);
                break;
            }
        }
        if let Some(transceiver) = matched_transceiver {
            let codec_params = codec_params_for(&chosen_codec);
            if let Err(e) = transceiver.set_codec_preferences(vec![codec_params]).await {
                // Not fatal — transceiver still works, SDP just offers
                // the default union. Log as warning so a field incident
                // is diagnosable.
                warn!(%session_id, %e, codec = %chosen_codec, "set_codec_preferences failed — SDP will carry default codec union");
            } else {
                info!(%session_id, codec = %chosen_codec, "SDP codec preferences pinned");
            }
        }

        // Shared keyframe-request flag. The RTCP reader task flips it on
        // PLI / FIR; media_pump consumes it before each encode and calls
        // force_intra_frame() on the openh264 encoder. Without this, lost
        // packets freeze the decoder until the next periodic IDR.
        //
        // Rate-limited: a browser under load can spam PLIs (we saw 43 in
        // a few seconds). Each keyframe at 4K is ~350 KB. Back-to-back
        // IDRs spike bandwidth → more loss → more PLI → collapse. Cap
        // keyframe responses to at most one per MIN_KEYFRAME_GAP.
        let keyframe_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Controller's quality preference, mutated by the `control`
        // data channel handler and polled by the media pump. AUTO is
        // the safe default until the controller advertises otherwise.
        let quality_state = Arc::new(std::sync::atomic::AtomicU8::new(quality::AUTO));
        // Latest receiver-estimated bitrate (REMB) in bps. 0 means no
        // hint yet; media_pump treats that as "use the resolution-
        // derived baseline + quality clamp". Modern Chromium often
        // sends TWCC instead of REMB, but advertises both — when REMB
        // arrives we honour it, when only TWCC arrives we currently
        // can't decode the bandwidth estimate (webrtc-rs 0.12 doesn't
        // expose its TWCC sender's BWE) and fall back to baseline.
        let remb_bps = Arc::new(std::sync::atomic::AtomicU32::new(0));
        // Reference-frame invalidation: set when the rtcp reader sees a
        // burst of NACK packets above a threshold within a short
        // window, indicating that the interceptor's retransmission
        // didn't recover the loss. Cheaper than a full IDR (which
        // adds 60-100 KB at 1080p and triggers TWCC throttling).
        // Default trait impl falls back to keyframe; backends that
        // expose proper intra-refresh override.
        let invalidation_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rtcp_reader = {
            let flag = keyframe_requested.clone();
            let remb = remb_bps.clone();
            let invalidate = invalidation_requested.clone();
            let sid = session_id;
            tokio::spawn(async move {
                use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
                use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
                use webrtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
                use webrtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;
                const MIN_KEYFRAME_GAP: Duration = Duration::from_millis(500);
                const MIN_INVALIDATION_GAP: Duration = Duration::from_millis(200);
                // NACK burst detector: trip invalidation when ≥ this
                // many NACKed sequence numbers arrive within the
                // window. Single-NACK is normal background loss the
                // interceptor handles via retransmission; bursts mean
                // the retransmission didn't recover and we need to
                // resync the decoder. Conservative threshold — too
                // sensitive triggers thrashing on edge networks.
                const NACK_BURST_THRESHOLD: u32 = 8;
                const NACK_WINDOW: Duration = Duration::from_secs(1);
                let mut last_keyframe = std::time::Instant::now() - MIN_KEYFRAME_GAP;
                let mut last_invalidation = std::time::Instant::now() - MIN_INVALIDATION_GAP;
                let mut nack_count_in_window: u32 = 0;
                let mut nack_window_started = std::time::Instant::now();
                loop {
                    match video_sender.read_rtcp().await {
                        Ok((pkts, _)) => {
                            let mut asks_keyframe = false;
                            for p in pkts {
                                let p_any = p.as_any();
                                if p_any.downcast_ref::<PictureLossIndication>().is_some()
                                    || p_any.downcast_ref::<FullIntraRequest>().is_some()
                                {
                                    asks_keyframe = true;
                                }
                                if let Some(remb_pkt) =
                                    p_any.downcast_ref::<ReceiverEstimatedMaximumBitrate>()
                                {
                                    // REMB carries the receiver's
                                    // bandwidth estimate in bps. Surface
                                    // verbatim; media_pump applies its
                                    // own safety factor + hysteresis.
                                    let bps = remb_pkt.bitrate as u32;
                                    if bps > 0 {
                                        debug!(session = %sid, remb_bps = bps, "REMB received");
                                        remb.store(bps, std::sync::atomic::Ordering::Relaxed);
                                    }
                                }
                                if let Some(nack) =
                                    p_any.downcast_ref::<TransportLayerNack>()
                                {
                                    // Reset the window if it's lapsed,
                                    // otherwise add to the count. Each
                                    // NACK packet contains nack_pairs
                                    // covering 1+ packet IDs; sum the
                                    // population count of each loss
                                    // bitmap as the actual loss count.
                                    let now = std::time::Instant::now();
                                    if now.duration_since(nack_window_started) > NACK_WINDOW {
                                        nack_window_started = now;
                                        nack_count_in_window = 0;
                                    }
                                    let lost: u32 = nack
                                        .nacks
                                        .iter()
                                        .map(|np| 1 + (np.lost_packets as u32).count_ones())
                                        .sum();
                                    nack_count_in_window =
                                        nack_count_in_window.saturating_add(lost);
                                    if nack_count_in_window >= NACK_BURST_THRESHOLD
                                        && now.duration_since(last_invalidation)
                                            >= MIN_INVALIDATION_GAP
                                    {
                                        info!(
                                            session = %sid,
                                            nack_count_in_window,
                                            "NACK burst → requesting reference invalidation"
                                        );
                                        invalidate.store(true, std::sync::atomic::Ordering::Relaxed);
                                        last_invalidation = now;
                                        // Reset the window so a single
                                        // burst doesn't keep firing.
                                        nack_window_started = now;
                                        nack_count_in_window = 0;
                                    }
                                }
                            }
                            if asks_keyframe {
                                let now = std::time::Instant::now();
                                if now.duration_since(last_keyframe) >= MIN_KEYFRAME_GAP {
                                    info!(session = %sid, "PLI/FIR → forcing keyframe");
                                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                                    last_keyframe = now;
                                }
                                // else: silently drop — we already sent
                                // an IDR within the last 500ms.
                            }
                        }
                        Err(_e) => {
                            // Sender closed; exit the reader.
                            return;
                        }
                    }
                }
            })
        };

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
        // `control` parses rc:* JSON (quality preference, etc.);
        // `cursor` receives an agent-driven stream of position / shape
        // messages pumped from CursorTracker; clipboard/files are
        // accepted but not yet wired.
        let quality_for_dc = quality_state.clone();
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let label = dc.label().to_string();
            info!(session = %session_id, %label, "data channel opened");
            let quality_for_dc = quality_for_dc.clone();
            Box::pin(async move {
                match label.as_str() {
                    "input" => attach_input_handler(dc),
                    "control" => attach_control_handler(dc, session_id, quality_for_dc),
                    "cursor" => attach_cursor_handler(dc, session_id),
                    _ => attach_log_only(dc, session_id),
                }
            })
        }));

        // Start the capture→encode→track pump. The pump is self-regulating:
        // with no capture backend compiled in, open_default returns a Noop
        // that parks forever, producing no samples.
        let pump = tokio::spawn(media_pump(
            session_id,
            video_track,
            keyframe_requested,
            invalidation_requested.clone(),
            quality_state.clone(),
            remb_bps.clone(),
            encoder_preference,
            chosen_codec,
        ));

        Ok(Self {
            pc,
            session_id,
            media_pump: Some(pump),
            rtcp_reader: Some(rtcp_reader),
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
        if let Some(reader) = &self.rtcp_reader {
            reader.abort();
        }
        if let Err(e) = self.pc.close().await {
            warn!(session = %self.session_id, %e, "PC close failed");
        }
    }
}

/// Per-session media pump. Captures frames, encodes to the negotiated
/// codec, writes Samples into the WebRTC track. Rebuilds the encoder
/// if the capture resolution changes mid-session (e.g. dock/undock).
#[allow(clippy::too_many_arguments)]
async fn media_pump(
    session_id: bson::oid::ObjectId,
    track: Arc<TrackLocalStaticSample>,
    keyframe_requested: Arc<std::sync::atomic::AtomicBool>,
    invalidation_requested: Arc<std::sync::atomic::AtomicBool>,
    quality_state: Arc<std::sync::atomic::AtomicU8>,
    remb_bps: Arc<std::sync::atomic::AtomicU32>,
    encoder_preference: encode::EncoderPreference,
    chosen_codec: String,
) {
    // Capture downscale policy mirrors the encoder preference. When the
    // HW encoder is in play (or will be, on Auto + Windows), we want
    // native-resolution frames; the HW path handles 4K fine and any
    // downscale here would discard detail for no gain. When the encoder
    // is software openh264, we keep the Auto policy so high-res sources
    // still get the 2× downsample to hit the encoder's throughput
    // ceiling.
    let downscale = downscale_for(encoder_preference);
    tracing::info!(
        %session_id,
        ?encoder_preference,
        ?downscale,
        "media pump starting"
    );
    let mut capturer = capture::open_default(TARGET_FPS, downscale);
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

    // Keep the most recent captured frame around so we can re-feed it to
    // the encoder during idle periods. DXGI Desktop Duplication only
    // signals when the screen changes — on an idle desktop the agent can
    // go seconds without producing a frame, which makes the browser's
    // decoder enter a pause state. The user then perceives several
    // seconds of lag when they finally do something, because the stream
    // has to resume from the pause. Re-encoding the last frame at the
    // idle floor keeps the RTP stream flowing and the decoder unpaused.
    // Arc<Frame> so repeated idle keepalives share the big BGRA buffer
    // with the encoder (which only reads). Without Arc, each keepalive
    // cloned the entire frame — up to 33 MB at 4K, 8 MB at 1080p —
    // every keepalive tick.
    let mut last_good_frame: Option<std::sync::Arc<crate::capture::Frame>> = None;
    // VFR (1F.1): idle floor at 1 fps. Was 500 ms (≈2 fps). The
    // browser's jitter buffer + the encoder's intra-refresh
    // (1B.1) tolerate the longer gap, and on a static desktop
    // there is nothing for the controller to react to anyway —
    // the only thing this duty cycle preserves is the RTP clock
    // and the decoder unpause. Once dirty-rect metadata lands
    // (1C.2 / WGC backend), this can drop further: re-encode
    // only when dirty_rects.is_empty() == false; otherwise emit
    // a NAL-free heartbeat tied to the wallclock.
    const IDLE_KEEPALIVE: Duration = Duration::from_millis(1_000);
    let mut last_capture_at = std::time::Instant::now();

    // Observability: count frames in/out and bytes written, log every 30
    // encoded frames (~once per second at 30fps). Without this a silent
    // stall in capture or encode is indistinguishable from a working pump.
    let mut frames_captured: u64 = 0;
    let mut frames_empty: u64 = 0;
    let mut frames_encoded: u64 = 0;
    let mut frames_keepalive: u64 = 0;
    let mut bytes_written: u64 = 0;
    let mut write_errors: u64 = 0;

    // Last applied quality preference. Initialised to a sentinel
    // (0xFF) so the first loop iteration unconditionally pushes the
    // current AUTO/Low/High choice into the encoder, even when no
    // controller message has arrived yet (covers the case where the
    // encoder is rebuilt mid-session and needs the bitrate re-applied).
    let mut last_applied_quality: u8 = 0xFF;
    // Last bitrate we pushed into the encoder. Used for hysteresis on
    // REMB-driven changes — reapply only if the new target moves
    // outside ±15% of the current one. Without hysteresis, REMB
    // wobble (every ~2 s) thrashes set_bitrate even on a stable link.
    let mut last_applied_bitrate: u32 = 0;
    // 0.85 safety factor against REMB so we don't drive right up to
    // the bandwidth ceiling — one congestion-control cycle later we'd
    // overshoot, packet loss spikes, REMB drops, oscillation.
    const REMB_SAFETY_FACTOR_NUM: u32 = 85;
    const REMB_SAFETY_FACTOR_DEN: u32 = 100;
    // Hysteresis band: only push a new bitrate if it differs from the
    // current applied one by more than this fraction.
    const HYSTERESIS_PCT: u32 = 15;

    loop {
        let frame: std::sync::Arc<crate::capture::Frame> = match capturer.next_frame().await {
            Ok(Some(f)) => {
                frames_captured += 1;
                last_capture_at = std::time::Instant::now();
                let arc = std::sync::Arc::new(f);
                last_good_frame = Some(arc.clone());
                arc
            }
            Ok(None) => {
                frames_empty += 1;
                // Log every ~5s worth of empty polls so an idle desktop is
                // visible without flooding. DXGI only fires on screen change,
                // so this can spike briefly then settle.
                if frames_empty.is_multiple_of(150) {
                    info!(%session_id, frames_empty, "capture produced no frame (idle screen)");
                }
                // If the screen has been idle for IDLE_KEEPALIVE and we
                // have a cached frame, re-encode it. openh264 will emit
                // a tiny (~tens of bytes) P-frame since nothing changed,
                // which keeps the browser's decoder unpaused.
                if last_capture_at.elapsed() >= IDLE_KEEPALIVE {
                    if let Some(ref f) = last_good_frame {
                        frames_keepalive += 1;
                        last_capture_at = std::time::Instant::now();
                        f.clone()
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            Err(e) => {
                // DXGI Desktop Duplication is fragile — it returns
                // transient errors on display-mode changes, DPI switches,
                // UAC dimmer entry/exit, lock screen transitions, RDP
                // takeover, fullscreen toggles, GPU driver recycles, etc.
                // These used to kill the pump, leaving the data channels
                // alive (mouse/keyboard still worked) but video frozen
                // forever until session reconnect. Rebuild the capturer
                // and the encoder, keep the pump running. 500ms backoff
                // so a genuine infinite error loop doesn't spin a core.
                warn!(%session_id, %e, "capture error — rebuilding capturer");
                tokio::time::sleep(Duration::from_millis(500)).await;
                capturer = capture::open_default(TARGET_FPS, downscale);
                // Force the encoder to rebuild on the next frame — new
                // capturer may come back at a different resolution (e.g.
                // after a DPI change) and openh264 can't be resized
                // mid-stream without re-init.
                encoder = None;
                encoder_dims = None;
                continue;
            }
        };

        // (Re)build the encoder if the frame dimensions change.
        if encoder_dims != Some((frame.width, frame.height)) {
            info!(
                %session_id,
                w = frame.width, h = frame.height,
                codec = %chosen_codec,
                "initialising encoder for frame dims"
            );
            let (enc, actual) = encode::open_for_codec(
                &chosen_codec,
                frame.width,
                frame.height,
                encoder_preference,
            );
            if actual != chosen_codec {
                // Runtime demotion (e.g. HEVC cascade failed at actual
                // dims despite enumeration passing). The track was
                // already bound to the negotiated codec's mime type
                // and the SDP answer sent — we can't switch mid-session.
                // Log loudly so a field incident is diagnosable, then
                // keep going: the browser will receive bytes it can't
                // decode and show a black frame. The controller can
                // reconnect or toggle Quality to re-negotiate.
                warn!(
                    %session_id,
                    requested = %chosen_codec,
                    actual = %actual,
                    "encoder demotion — browser will see undecodable stream until renegotiation"
                );
            }
            encoder = Some(enc);
            encoder_dims = Some((frame.width, frame.height));
            // Force the quality preference back through the new
            // encoder — set_bitrate state lives on the encoder
            // instance, so a rebuild starts from the resolution-
            // derived default until we re-apply.
            last_applied_quality = 0xFF;
        }

        let enc = encoder.as_mut().unwrap();
        if keyframe_requested.swap(false, std::sync::atomic::Ordering::Relaxed) {
            enc.request_keyframe();
        }
        if invalidation_requested.swap(false, std::sync::atomic::Ordering::Relaxed) {
            // 0 = "we don't know which frame was lost; just give us
            // an intra recovery". Backends with ref-tracking can use
            // a meaningful value once peer.rs surfaces it.
            enc.request_reference_invalidation(0);
        }
        // ROI hints from per-frame dirty rects. Empty for scrap
        // captures (no dirty-rect API); WGC backend (1C.1) will
        // populate these so MF/NVENC overrides can spend bits on
        // changed regions. Default trait impl is a no-op so this is
        // free for SW encoders.
        if !frame.dirty_rects.is_empty() {
            enc.set_roi_hints(&frame.dirty_rects, (frame.width, frame.height));
        }

        // Adaptive bitrate: combine quality preference (controller
        // intent) with REMB (network capacity) and apply on change
        // or out-of-hysteresis movement. MF + openh264 both honour
        // set_bitrate now (1F.2). Cheap on every frame: two atomic
        // loads + integer math + a single comparison.
        let q_now = quality_state.load(std::sync::atomic::Ordering::Relaxed);
        let remb_now = remb_bps.load(std::sync::atomic::Ordering::Relaxed);
        if let Some((w, h)) = encoder_dims {
            let base = encode::initial_bitrate_for(w, h);
            let quality_target = quality::target_bitrate(q_now, base);
            // If REMB hasn't reported, defer to the quality-derived
            // target. Once it does, take min(quality, remb*safety) so
            // the controller can ratchet down further on a metered
            // link but never push past what the receiver thinks the
            // path can carry.
            let target = if remb_now == 0 {
                quality_target
            } else {
                let remb_safe = (remb_now / REMB_SAFETY_FACTOR_DEN)
                    .saturating_mul(REMB_SAFETY_FACTOR_NUM);
                quality_target.min(remb_safe.max(500_000))
            };
            // Hysteresis: only push when quality changed (operator
            // input always wins immediately) OR target moves outside
            // ±HYSTERESIS_PCT of last applied.
            let quality_changed = q_now != last_applied_quality;
            let drift_too_big = if last_applied_bitrate == 0 {
                true // first apply: always push
            } else {
                let band = (last_applied_bitrate / 100).saturating_mul(HYSTERESIS_PCT);
                target.abs_diff(last_applied_bitrate) > band
            };
            if quality_changed || drift_too_big {
                enc.set_bitrate(target);
                info!(
                    %session_id,
                    quality = quality::label(q_now),
                    base_bps = base,
                    remb_bps = remb_now,
                    target_bps = target,
                    "applying adaptive bitrate"
                );
                last_applied_quality = q_now;
                last_applied_bitrate = target;
            }
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
            let backend = encoder.as_ref().map(|e| e.name()).unwrap_or("none");
            info!(
                %session_id,
                backend,
                first_frame_bytes = packet_bytes,
                "first encoded frame written to track"
            );
        }
        if frames_encoded.is_multiple_of(30) {
            let backend = encoder.as_ref().map(|e| e.name()).unwrap_or("none");
            info!(
                %session_id,
                backend,
                frames_captured, frames_empty, frames_encoded, frames_keepalive,
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

/// `control` data-channel handler. Parses JSON `rc:*` envelopes and
/// applies them. Today the only message is `rc:quality` (mutating the
/// shared atomic that the media pump polls before each encode); future
/// types (rc:cursor-shape from agent → controller, rc:bitrate-hint,
/// rc:dpi-change) layer on the same parse-by-`t` switch.
fn attach_control_handler(
    dc: Arc<RTCDataChannel>,
    session_id: bson::oid::ObjectId,
    quality_state: Arc<std::sync::atomic::AtomicU8>,
) {
    dc.on_message(Box::new(move |msg| {
        let quality_state = quality_state.clone();
        Box::pin(async move {
            // Trust-but-verify: a malformed message must never crash
            // the data-channel callback (it'd kill the channel for
            // the rest of the session). Every parse path silently
            // logs and returns on failure.
            let text = match std::str::from_utf8(&msg.data) {
                Ok(t) => t,
                Err(_) => {
                    debug!(%session_id, bytes = msg.data.len(), "control: non-UTF8 payload, dropped");
                    return;
                }
            };
            let val: serde_json::Value = match serde_json::from_str(text) {
                Ok(v) => v,
                Err(e) => {
                    debug!(%session_id, %e, "control: malformed JSON, dropped");
                    return;
                }
            };
            let Some(t) = val.get("t").and_then(|v| v.as_str()) else {
                debug!(%session_id, "control: message missing 't' tag, dropped");
                return;
            };
            match t {
                "rc:quality" => {
                    let Some(q_str) = val.get("quality").and_then(|v| v.as_str()) else {
                        debug!(%session_id, "control: rc:quality missing quality field");
                        return;
                    };
                    let Some(q_val) = quality::from_wire(q_str) else {
                        debug!(%session_id, q = q_str, "control: rc:quality unknown value");
                        return;
                    };
                    let prev = quality_state.swap(q_val, std::sync::atomic::Ordering::Relaxed);
                    if prev != q_val {
                        info!(
                            %session_id,
                            prev = quality::label(prev),
                            new = quality::label(q_val),
                            "control: rc:quality updated"
                        );
                    }
                }
                other => {
                    debug!(%session_id, t = other, "control: unknown message type");
                }
            }
        })
    }));
}

/// `cursor` data-channel handler. Spawns a pumper task that polls
/// the OS cursor at 30 Hz and sends `cursor:pos` / `cursor:shape` /
/// `cursor:hide` JSON messages over the DC. Exits when the DC closes
/// (the `send_text` call returns an error). The tracker caches shape
/// bitmaps by HCURSOR handle so repeated polls at the same shape only
/// send position updates — on a static cursor the bitmap pays for
/// itself once per shape change (arrow → I-beam → hand → etc.).
fn attach_cursor_handler(dc: Arc<RTCDataChannel>, session_id: bson::oid::ObjectId) {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    tokio::spawn(async move {
        // Wait for the DC to be open before starting the pump — a
        // just-constructed RTCDataChannel hasn't completed the SCTP
        // handshake yet.
        let mut tracker = crate::capture::cursor::CursorTracker::new();
        // ~30 Hz matches the capture pacing. Browsers smooth out any
        // jitter via RAF; tighter intervals would just burn DC
        // bandwidth for sub-pixel moves.
        let mut ticker = tokio::time::interval(Duration::from_millis(33));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Emit cursor:hide once when the cursor disappears so the
        // browser can clear its overlay; don't keep re-emitting.
        let mut last_hidden = false;
        loop {
            ticker.tick().await;
            if dc.ready_state()
                == webrtc::data_channel::data_channel_state::RTCDataChannelState::Closed
            {
                return;
            }
            match tracker.poll() {
                Some(tick) => {
                    last_hidden = false;
                    if let Some(shape) = &tick.shape {
                        let b64 = BASE64.encode(&shape.bgra);
                        let msg = serde_json::json!({
                            "t": "cursor:shape",
                            "id": tick.shape_id,
                            "w": shape.width,
                            "h": shape.height,
                            "hx": shape.hotspot_x,
                            "hy": shape.hotspot_y,
                            "bgra": b64,
                        });
                        if let Ok(s) = serde_json::to_string(&msg) {
                            let _ = dc.send_text(s).await;
                        }
                    }
                    let msg = serde_json::json!({
                        "t": "cursor:pos",
                        "id": tick.shape_id,
                        "x": tick.x,
                        "y": tick.y,
                    });
                    if let Ok(s) = serde_json::to_string(&msg)
                        && dc.send_text(s).await.is_err()
                    {
                        debug!(%session_id, "cursor DC closed — stopping pump");
                        return;
                    }
                }
                None => {
                    if !last_hidden {
                        last_hidden = true;
                        let msg = serde_json::json!({ "t": "cursor:hide" });
                        if let Ok(s) = serde_json::to_string(&msg) {
                            let _ = dc.send_text(s).await;
                        }
                    }
                }
            }
        }
    });
}

/// Placeholder handler for data channels that aren't wired to OS output
/// yet (`clipboard`, `files`). Logs message sizes so we can see
/// activity without spamming the log with contents.
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

/// Build the `RTCRtpCodecCapability` for the negotiated codec. Matches
/// webrtc-rs's `register_default_codecs` entries byte-for-byte so the
/// internal `payloader_for_codec` lookup resolves and the SDP answer
/// carries the expected payload type.
///
/// Default MediaEngine registrations (webrtc-rs 0.12):
///   video/H264 Constrained Baseline, packetization-mode=1,
///       profile-level-id=42e01f → PT 125
///   video/H265 empty fmtp              → PT 126
///   video/AV1  profile-id=0            → PT 41
///
/// Unknown codec → H.264 default (paranoia: should never hit because
/// `pick_best_codec` only returns codecs both sides advertise).
fn build_video_codec_cap(codec: &str) -> RTCRtpCodecCapability {
    let feedback = vec![
        RTCPFeedback { typ: "goog-remb".to_string(),    parameter: String::new() },
        RTCPFeedback { typ: "ccm".to_string(),          parameter: "fir".to_string() },
        RTCPFeedback { typ: "nack".to_string(),         parameter: String::new() },
        RTCPFeedback { typ: "nack".to_string(),         parameter: "pli".to_string() },
        RTCPFeedback { typ: "transport-cc".to_string(), parameter: String::new() },
    ];
    match codec.to_ascii_lowercase().as_str() {
        "av1" => RTCRtpCodecCapability {
            mime_type: "video/AV1".to_string(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "profile-id=0".to_string(),
            rtcp_feedback: feedback,
        },
        "h265" | "hevc" => RTCRtpCodecCapability {
            mime_type: "video/H265".to_string(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: String::new(),
            rtcp_feedback: feedback,
        },
        _ => RTCRtpCodecCapability {
            mime_type: "video/H264".to_string(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line:
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                    .to_string(),
            rtcp_feedback: feedback,
        },
    }
}

/// Build the `RTCRtpCodecParameters` pinned into the transceiver's
/// codec preferences. Same capability as the track carries; payload
/// type matches the default MediaEngine's PT for that codec.
fn codec_params_for(codec: &str) -> webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters {
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecParameters;
    let capability = build_video_codec_cap(codec);
    let payload_type = match codec.to_ascii_lowercase().as_str() {
        "av1" => 41,
        "h265" | "hevc" => 126,
        _ => 125,
    };
    RTCRtpCodecParameters {
        capability,
        payload_type,
        ..Default::default()
    }
}

#[cfg(test)]
mod codec_cap_tests {
    use super::{build_video_codec_cap, codec_params_for};

    #[test]
    fn h264_cap_matches_webrtc_default() {
        let cap = build_video_codec_cap("h264");
        assert_eq!(cap.mime_type, "video/H264");
        assert_eq!(cap.clock_rate, 90000);
        assert!(cap.sdp_fmtp_line.contains("profile-level-id=42e01f"));
        assert!(cap.sdp_fmtp_line.contains("packetization-mode=1"));
    }

    #[test]
    fn hevc_cap_has_no_fmtp_line() {
        let cap = build_video_codec_cap("h265");
        assert_eq!(cap.mime_type, "video/H265");
        assert!(cap.sdp_fmtp_line.is_empty());
        let alias = build_video_codec_cap("hevc");
        assert_eq!(alias.mime_type, "video/H265");
    }

    #[test]
    fn av1_cap_carries_profile_id() {
        let cap = build_video_codec_cap("av1");
        assert_eq!(cap.mime_type, "video/AV1");
        assert_eq!(cap.sdp_fmtp_line, "profile-id=0");
    }

    #[test]
    fn case_insensitive_selection() {
        assert_eq!(build_video_codec_cap("H264").mime_type, "video/H264");
        assert_eq!(build_video_codec_cap("AV1").mime_type, "video/AV1");
        assert_eq!(build_video_codec_cap("HEVC").mime_type, "video/H265");
    }

    #[test]
    fn unknown_codec_defaults_to_h264() {
        // Belt-and-braces: pick_best_codec should never hand us an
        // unknown codec, but if it does we must not panic.
        let cap = build_video_codec_cap("vp8");
        assert_eq!(cap.mime_type, "video/H264");
    }

    #[test]
    fn codec_params_payload_types_match_default_media_engine() {
        // webrtc-rs 0.12 defaults: H.264 PT 125, HEVC PT 126, AV1 PT 41.
        assert_eq!(codec_params_for("h264").payload_type, 125);
        assert_eq!(codec_params_for("h265").payload_type, 126);
        assert_eq!(codec_params_for("hevc").payload_type, 126);
        assert_eq!(codec_params_for("av1").payload_type, 41);
    }

    #[test]
    fn rtcp_feedback_includes_nack_pli() {
        // All three codecs need NACK+PLI so the browser can request
        // retransmission and keyframes; drop either one and the
        // stream freezes on any loss.
        for codec in ["h264", "h265", "av1"] {
            let cap = build_video_codec_cap(codec);
            assert!(
                cap.rtcp_feedback.iter().any(|f| f.typ == "nack" && f.parameter == "pli"),
                "codec {codec} missing nack pli"
            );
            assert!(
                cap.rtcp_feedback.iter().any(|f| f.typ == "transport-cc"),
                "codec {codec} missing transport-cc"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::quality::*;

    #[test]
    fn from_wire_accepts_known_values_case_insensitively() {
        assert_eq!(from_wire("low"), Some(LOW));
        assert_eq!(from_wire("LOW"), Some(LOW));
        assert_eq!(from_wire("Low"), Some(LOW));
        assert_eq!(from_wire("auto"), Some(AUTO));
        assert_eq!(from_wire("Auto"), Some(AUTO));
        assert_eq!(from_wire("high"), Some(HIGH));
        assert_eq!(from_wire("HIGH"), Some(HIGH));
    }

    #[test]
    fn from_wire_rejects_unknown_values() {
        assert_eq!(from_wire(""), None);
        assert_eq!(from_wire("medium"), None);
        assert_eq!(from_wire("ultra"), None);
        assert_eq!(from_wire("0"), None);
    }

    #[test]
    fn label_round_trips_known_values() {
        assert_eq!(label(LOW), "low");
        assert_eq!(label(AUTO), "auto");
        assert_eq!(label(HIGH), "high");
        // Sentinel + unknown values fall back to "auto" so logs stay
        // useful even when the atomic gets corrupted.
        assert_eq!(label(0xFF), "auto");
        assert_eq!(label(42), "auto");
    }

    #[test]
    fn target_bitrate_scales_per_quality() {
        // Base = 6 Mbps (rough 1080p target).
        let base = 6_000_000;
        assert_eq!(target_bitrate(LOW, base), 3_000_000);
        assert_eq!(target_bitrate(AUTO, base), 6_000_000);
        assert_eq!(target_bitrate(HIGH, base), 9_000_000);
    }

    #[test]
    fn target_bitrate_low_floors_at_500_kbps() {
        // Even on tiny resolutions Low must produce a usable stream.
        assert_eq!(target_bitrate(LOW, 100_000), 500_000);
        assert_eq!(target_bitrate(LOW, 1_000_000), 500_000);
        assert_eq!(target_bitrate(LOW, 1_500_000), 750_000);
    }

    #[test]
    fn target_bitrate_high_caps_at_20_mbps() {
        // 4K base is 12 Mbps clamped; High should add 50% capped at 20.
        assert_eq!(target_bitrate(HIGH, 12_000_000), 18_000_000);
        // Very high base: cap engages.
        assert_eq!(target_bitrate(HIGH, 20_000_000), 20_000_000);
        assert_eq!(target_bitrate(HIGH, 50_000_000), 20_000_000);
    }
}

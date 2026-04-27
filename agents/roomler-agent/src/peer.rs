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

/// Target capture rate on the **software** path. openh264 pegs a CPU core
/// above ~35 fps at 1080p; 30 is the stable ceiling. See `target_fps_for`
/// for the hardware path which lifts to 60.
const TARGET_FPS_SW: u32 = 30;

/// Target capture rate on the **hardware** path. MF-HW + WGC handle
/// 2560×1600 @ 60 and 4K @ 60 comfortably on RTX-class GPUs. Bumping the
/// capture rate is the single biggest perceptual win against RustDesk's
/// native 60 fps pipeline — halves motion blur / step latency on pointer
/// and scroll.
const TARGET_FPS_HW: u32 = 60;

/// Pick a target capture rate consistent with the chosen encoder. On
/// Auto with `mf-encoder` compiled in we assume the cascade will land on
/// MF-HW (probe-gated at startup, falls back cleanly) and bias toward
/// 60. Everywhere else the 30 fps SW floor stays.
fn target_fps_for(pref: encode::EncoderPreference) -> u32 {
    match pref {
        encode::EncoderPreference::Hardware => TARGET_FPS_HW,
        #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
        encode::EncoderPreference::Auto => TARGET_FPS_HW,
        _ => TARGET_FPS_SW,
    }
}

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
    /// metered uplinks), High adds 50%. Ceiling lifted 20 → 30 Mbps in
    /// the RustDesk-parity sprint so 4K60 HEVC at High can actually
    /// hit the 25 Mbps the base provides and still leave headroom
    /// for burst bits on a GOP boundary.
    pub(super) fn target_bitrate(quality: u8, base_bps: u32) -> u32 {
        const MAX_HIGH_BPS: u32 = 30_000_000;
        match quality {
            LOW => (base_bps / 2).max(500_000),
            HIGH => base_bps.saturating_mul(3) / 2,
            _ => base_bps,
        }
        .min(MAX_HIGH_BPS)
    }
}

/// Controller-requested encode resolution. `Native` keeps the agent's
/// monitor resolution; `Fixed` downscales post-capture to the target
/// dims before the encoder sees the frame. Lives in a shared
/// `Arc<Mutex<_>>` mutated by the `control` DC handler on `rc:resolution`
/// and polled by the media pump before each encode. The encoder's
/// existing dims-change rebuild path handles the teardown / reinit
/// when the effective frame size shifts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetResolution {
    /// Agent picks — whatever the capture backend produces natively.
    Native,
    /// Controller-specified target. Downscale native → (w, h) before
    /// encode. Upscaling is a no-op: we cap at native so an over-large
    /// request (Fit mode on a viewport bigger than the source) doesn't
    /// waste encoder budget on upsampled pixels.
    Fixed { width: u32, height: u32 },
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
            // On Windows with mf-encoder compiled in, the cascade picks
            // MF-HW first (probe-gated at startup, falls back to
            // openh264 cleanly if probe fails). The HW path handles 4K
            // at native resolution; the 2× CPU box filter is dead
            // weight that costs perceived resolution. Skip it — if the
            // cascade falls back to SW, the encoder itself will refuse
            // 4K@60 and the user still gets a working session at
            // degraded fps, which is strictly better than losing
            // native resolution unconditionally.
            #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
            {
                capture::DownscalePolicy::Never
            }
            #[cfg(not(all(target_os = "windows", feature = "mf-encoder")))]
            {
                capture::DownscalePolicy::Auto
            }
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
    /// Phase Y.3: `negotiated_transport` is the video transport
    /// chosen by signalling (`AgentCaps.transports` ∩ browser
    /// `preferred_transport`). `None` → legacy WebRTC video track.
    /// `Some("data-channel-vp9-444")` → media pump bypasses the
    /// track and writes length-prefixed VP9 frames into the
    /// `video-bytes` DC opened by the controller. See the
    /// `on_data_channel` branch in `new()` for where the DC
    /// handle is stashed.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        session_id: bson::oid::ObjectId,
        ice_servers: &[IceServer],
        outbound: mpsc::Sender<ClientMsg>,
        encoder_preference: encode::EncoderPreference,
        chosen_codec: String,
        negotiated_transport: Option<String>,
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
        registry =
            webrtc::api::interceptor_registry::register_default_interceptors(registry, &mut engine)
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
        // Controller-chosen encode resolution. Defaults to Native; the
        // `rc:resolution` control-DC message (Phase 2 of the viewer-
        // controls sprint) writes this and the media pump applies on
        // the next frame. Std Mutex (not tokio) because reads from the
        // sync pump loop and writes from the async DC callback are
        // both brief.
        let target_resolution = Arc::new(std::sync::Mutex::new(TargetResolution::Native));
        // Phase Y.3 (docs/vp9-444-plan.md). When the browser opens a
        // `video-bytes` data channel — only happens when both sides
        // negotiated `data-channel-vp9-444` transport in caps — we
        // stash the DC handle here so the media pump can write
        // length-prefixed VP9 frames into it instead of the WebRTC
        // video track. None until the channel arrives; the pump
        // checks each iteration. Tokio mutex because the on_data_channel
        // callback writes from an async context and the pump reads
        // from its own task — both brief, no contention.
        let video_bytes_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>> =
            Arc::new(tokio::sync::Mutex::new(None));
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
                                if let Some(nack) = p_any.downcast_ref::<TransportLayerNack>() {
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
                                        invalidate
                                            .store(true, std::sync::atomic::Ordering::Relaxed);
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
                    let Ok(candidate) = serde_json::to_value(&json) else {
                        return;
                    };
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
        // messages pumped from CursorTracker; `clipboard` round-trips
        // text between the agent's OS clipboard and the browser;
        // `files` accepts uploads that land in the controlled host's
        // Downloads folder.
        let quality_for_dc = quality_state.clone();
        let target_res_for_dc = target_resolution.clone();
        let video_bytes_dc_for_callback = video_bytes_dc.clone();
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let label = dc.label().to_string();
            info!(session = %session_id, %label, "data channel opened");
            let quality_for_dc = quality_for_dc.clone();
            let target_res_for_dc = target_res_for_dc.clone();
            let video_bytes_stash = video_bytes_dc_for_callback.clone();
            Box::pin(async move {
                match label.as_str() {
                    "input" => attach_input_handler(dc),
                    "control" => {
                        attach_control_handler(dc, session_id, quality_for_dc, target_res_for_dc)
                    }
                    "cursor" => attach_cursor_handler(dc, session_id),
                    #[cfg(feature = "clipboard")]
                    "clipboard" => attach_clipboard_handler(dc, session_id),
                    "files" => attach_files_handler(dc, session_id),
                    "video-bytes" => {
                        // Phase Y.3 stash. The media pump (when caps
                        // negotiated this transport) consults this
                        // handle each iteration and routes encoded
                        // frames here instead of the WebRTC video
                        // track. No-op today — full pump-side branch
                        // lands in a follow-up. Logging the open
                        // event so a future regression where the
                        // channel arrives but the pump doesn't see it
                        // is greppable.
                        info!(
                            session = %session_id,
                            "video-bytes DC stashed for Y.3 media-pump branch"
                        );
                        *video_bytes_stash.lock().await = Some(dc.clone());
                        attach_log_only(dc, session_id);
                    }
                    _ => attach_log_only(dc, session_id),
                }
            })
        }));

        // Start the capture→encode→track pump. The pump is self-regulating:
        // with no capture backend compiled in, open_default returns a Noop
        // that parks forever, producing no samples. Phase Y.3:
        // `negotiated_transport` + `video_bytes_dc` let the pump route
        // VP9 4:4:4 frames over the DC instead of the track when the
        // session negotiated `data-channel-vp9-444`.
        let pump = tokio::spawn(media_pump(
            session_id,
            video_track,
            keyframe_requested,
            invalidation_requested.clone(),
            quality_state.clone(),
            remb_bps.clone(),
            encoder_preference,
            chosen_codec,
            target_resolution.clone(),
            negotiated_transport,
            video_bytes_dc.clone(),
        ));

        Ok(Self {
            pc,
            session_id,
            media_pump: Some(pump),
            rtcp_reader: Some(rtcp_reader),
        })
    }

    pub async fn handle_offer(&self, offer_sdp: String) -> Result<String> {
        // SDP codec-name normalisation for H.265:
        // RFC 7798 specifies the SDP rtpmap subtype as `H265` ("H265/90000"),
        // and every browser (Chrome, Edge, Safari) emits exactly that in its
        // offer. But webrtc-rs 0.12's `register_default_codecs` keys its
        // internal HEVC entry on the mime string "video/HEVC" — and its
        // fuzzy-search is a naive string compare, not alias-aware
        // (video/H265 vs video/HEVC don't match case-insensitively). So a
        // raw Chrome H265 offer gets dropped during codec matching and
        // `create_answer` then fails because no video codec survived.
        //
        // Workaround: swap `H265` → `HEVC` in the incoming offer so the
        // webrtc-rs internal view uses the "video/HEVC" mime consistently,
        // and reverse the swap on the outgoing answer so the browser sees
        // spec-compliant rtpmap names. This is lossy only for the `name`
        // field of the rtpmap line; everything else (PT, clock rate, fmtp)
        // is untouched.
        let munged_offer = offer_sdp.replace("H265/90000", "HEVC/90000");
        let offer = RTCSessionDescription::offer(munged_offer).context("parse offer")?;
        self.pc
            .set_remote_description(offer)
            .await
            .context("set_remote_description")?;

        let answer = self.pc.create_answer(None).await.context("create_answer")?;
        self.pc
            .set_local_description(answer.clone())
            .await
            .context("set_local_description")?;

        // Reverse the HEVC → H265 munge on the outgoing answer so the
        // browser's SDP parser recognises the rtpmap subtype.
        let munged_answer = answer.sdp.replace("HEVC/90000", "H265/90000");
        Ok(munged_answer)
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
///
/// Phase Y.3: when `negotiated_transport == Some("data-channel-vp9-444")`
/// AND the `vp9-444` Cargo feature is compiled in, the pump runs an
/// alternate fast-path that builds a libvpx Vp9Encoder, length-prefixes
/// each encoded frame, and writes them into the `video-bytes`
/// RTCDataChannel that the controller opened (see peer.rs line ~494
/// `on_data_channel` arm and `docs/vp9-444-plan.md` for the wire
/// format). The webrtc track stays bound but receives no samples in
/// that mode — the browser side renders from the worker-decoded
/// canvas instead of `<video>`.
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
    target_resolution: Arc<std::sync::Mutex<TargetResolution>>,
    negotiated_transport: Option<String>,
    video_bytes_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
) {
    // Y.3 fork: route to the DC pump when the session negotiated VP9
    // 4:4:4 over the `video-bytes` channel. Falls through to the
    // legacy track-based pump otherwise — including when the feature
    // is compiled in but the negotiation didn't pick VP9 (mismatched
    // browser / older controller / operator override).
    if matches!(
        negotiated_transport.as_deref(),
        Some("data-channel-vp9-444")
    ) {
        #[cfg(feature = "vp9-444")]
        {
            tracing::info!(
                %session_id,
                "media pump: VP9-444 over DataChannel (Phase Y.3)"
            );
            return media_pump_vp9_444_dc(
                session_id,
                video_bytes_dc,
                keyframe_requested,
                target_resolution,
            )
            .await;
        }
        #[cfg(not(feature = "vp9-444"))]
        {
            tracing::warn!(
                %session_id,
                "negotiated_transport=data-channel-vp9-444 but agent was built without `vp9-444` feature — falling back to WebRTC video track"
            );
        }
    }
    // Suppress the "field never read" warning when the legacy path
    // ignores video_bytes_dc (no vp9-444 feature, or webrtc track
    // mode). The handle is still created in peer.rs because the
    // on_data_channel callback unconditionally stashes any DC named
    // `video-bytes` for forward-compat with future agent builds.
    let _ = &video_bytes_dc;
    // Capture downscale policy mirrors the encoder preference. When the
    // HW encoder is in play (or will be, on Auto + Windows), we want
    // native-resolution frames; the HW path handles 4K fine and any
    // downscale here would discard detail for no gain. When the encoder
    // is software openh264, we keep the Auto policy so high-res sources
    // still get the 2× downsample to hit the encoder's throughput
    // ceiling.
    let downscale = downscale_for(encoder_preference);
    // `target_fps` becomes mut because the auto-fps-cap heuristic (see
    // the auto_downscale_evaluated block below) may drop it from the
    // optimistic Auto-on-Windows 60 to 30 if the encoder cascade ends
    // up on a SW MFT. Keep it as the single source of truth so
    // `frame_duration_floor` stays consistent.
    let mut target_fps = target_fps_for(encoder_preference);
    tracing::info!(
        %session_id,
        ?encoder_preference,
        ?downscale,
        target_fps,
        "media pump starting"
    );
    let mut capturer = capture::open_default(target_fps, downscale);
    let mut encoder: Option<Box<dyn encode::VideoEncoder>> = None;
    let mut encoder_dims: Option<(u32, u32)> = None;
    // One-shot guard for the SW-HEVC-at-high-res auto-downscale
    // heuristic. Flips to true after the first encoder build so we
    // evaluate the policy once per session — a mid-session operator
    // override via `rc:resolution` must not be clobbered by a
    // re-evaluation on an incidental encoder rebuild (DPI flip, etc.).
    let mut auto_downscale_evaluated = false;
    // Floor on the `duration` field of each Sample. DXGI Desktop Duplication
    // only emits a frame when the screen changes, so on an idle desktop the
    // real gap between two write_sample calls can be seconds. RTP timestamp
    // increments are `duration * clock_rate`; if duration stays at target_fps
    // (16.6 ms at 60 fps, 33 ms at 30 fps) while wallclock advances by 1 s,
    // the browser's playout clock starves and the video element goes black.
    // Measure the wallclock gap per frame and use that as the duration — the
    // first sample uses the nominal floor derived from target_fps.
    let mut frame_duration_floor = Duration::from_micros(1_000_000 / target_fps as u64);
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
    // Per-stage wall-time accumulators (microseconds) so the heartbeat
    // can attribute the per-frame budget. When users report "only 7 fps"
    // the breakdown makes it obvious whether capture is blocking
    // (WGC CPU readback on iGPU) or encode is saturated (fallback to
    // a weak MFT after an adapter cascade demoted to Intel UHD).
    let mut capture_time_us: u64 = 0;
    let mut encode_time_us: u64 = 0;
    // Reset the accumulators at each heartbeat so averages are over
    // the preceding ~30-frame window, not the entire session.
    let mut heartbeat_frames_base: u64 = 0;
    let mut heartbeat_capture_us_base: u64 = 0;
    let mut heartbeat_encode_us_base: u64 = 0;

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
        let capture_started = std::time::Instant::now();
        let frame: std::sync::Arc<crate::capture::Frame> = match capturer.next_frame().await {
            Ok(Some(f)) => {
                capture_time_us =
                    capture_time_us.saturating_add(capture_started.elapsed().as_micros() as u64);
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
                capturer = capture::open_default(target_fps, downscale);
                // Force the encoder to rebuild on the next frame — new
                // capturer may come back at a different resolution (e.g.
                // after a DPI change) and openh264 can't be resized
                // mid-stream without re-init.
                encoder = None;
                encoder_dims = None;
                continue;
            }
        };

        // Apply the controller-chosen target resolution. Native = no
        // change. Fixed = downscale (upscaling is refused — we cap at
        // native since upsampling wastes encoder budget on interpolated
        // pixels that carry no new information). On resolution change
        // the `encoder_dims` check below rebuilds the encoder.
        let frame = apply_target_resolution(frame, *target_resolution.lock().unwrap());

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

            // Loudly surface the Noop case. Previously this only
            // showed up in the ~1 s heartbeat log as `backend="noop"`,
            // which looks like normal progress to anyone not
            // reading carefully. A Noop encoder means the browser
            // gets only SDP setup bytes and a permanent black
            // frame — it's the single biggest "session looks alive
            // but nothing works" footgun in the stack. Shout at
            // session-build time so field reports land on a log
            // line that explains the symptom in one read.
            if encoder.as_ref().map(|e| e.name()) == Some("noop") {
                warn!(
                    %session_id,
                    codec = %chosen_codec,
                    w = frame.width, h = frame.height,
                    "encoder resolved to NoopEncoder — NO VIDEO WILL SHIP for this session. Cascade above tells you why. Workarounds: toggle codec override to H.264 + reconnect, or switch Quality to `low` to force a smaller profile."
                );
            }

            // Auto-downscale heuristic. SW HEVC (MS's
            // HEVCVideoExtensionEncoder is the only SW HEVC on
            // Windows) can't sustain 30 fps at 4K on any machine we
            // have, and the cascade lands there whenever the HW
            // HEVC MFTs fail — NVENC Blackwell (0x8000FFFF), Intel
            // QSV async-only (0x80004005), AMD on shared-memory
            // configurations. We want the operator to see
            // smooth 30-60 fps out of the box rather than a
            // 7 fps stream they have to know how to fix. Cap the
            // CAPTURE resolution at 1920×1080 — that's the breakpoint
            // where SW HEVC on modern Intel/AMD laptops typically
            // sustains 30 fps. Only applies on first session start
            // (per `auto_downscale_evaluated`) and only when the
            // operator hasn't already set an explicit override
            // via `rc:resolution`.
            if !auto_downscale_evaluated {
                auto_downscale_evaluated = true;
                let enc_ref = encoder.as_ref().unwrap();
                let backend_is_sw = !enc_ref.is_hardware();
                // Tier the downscale by codec weight. HEVC + AV1
                // SW encode is ~3x heavier than H.264, so cap them
                // hard at 1080p. H.264 SW is faster but 1920x1200
                // at 30 fps still eats ~21 ms / frame on an Intel
                // iGPU — close to our 33 ms budget and leaving no
                // headroom for capture jitter (field log
                // 2026-04-27 from RoziLaptop -> Schetovodstvo-PZ
                // showed exactly that pattern: smooth 30 fps math
                // but visibly sluggish to the operator). Drop H.264
                // SW above 720p down to 1280x720 where encode is
                // comfortably under 12 ms / frame.
                let heavy_codec = chosen_codec == "h265" || chosen_codec == "av1";
                let h264 = chosen_codec == "h264";
                let above_1080p =
                    (frame.width as u64) * (frame.height as u64) > (1920u64 * 1080u64);
                let above_720p = (frame.width as u64) * (frame.height as u64) > (1280u64 * 720u64);
                if backend_is_sw && heavy_codec && above_1080p {
                    let mut guard = target_resolution.lock().unwrap();
                    if matches!(*guard, TargetResolution::Native) {
                        *guard = TargetResolution::Fixed {
                            width: 1920,
                            height: 1080,
                        };
                        tracing::warn!(
                            %session_id,
                            native_w = frame.width,
                            native_h = frame.height,
                            codec = %chosen_codec,
                            encoder = enc_ref.name(),
                            "auto-downscale: SW heavy codec on high-res source — capping capture at 1920x1080 to preserve fps. Operator can override via rc:resolution."
                        );
                    }
                } else if backend_is_sw && h264 && above_720p {
                    let mut guard = target_resolution.lock().unwrap();
                    if matches!(*guard, TargetResolution::Native) {
                        *guard = TargetResolution::Fixed {
                            width: 1280,
                            height: 720,
                        };
                        tracing::warn!(
                            %session_id,
                            native_w = frame.width,
                            native_h = frame.height,
                            codec = %chosen_codec,
                            encoder = enc_ref.name(),
                            "auto-downscale: SW H.264 on high-res source — capping capture at 1280x720 so encode stays under the 33 ms 30-fps budget. Operator can override via rc:resolution."
                        );
                    }
                }

                // Auto-fps-cap. When the H.264 cascade lands on a SW
                // MFT (Intel QSV defers to the as-yet-unbuilt async
                // pipeline, MS SW MFT wins by default), capture
                // becomes the bottleneck — the BGRA readback alone
                // is ~20 ms on Intel UHD-class iGPUs, against a
                // 16.6 ms budget at 60 fps. WGC then drops 35-45 %
                // of frames and the resulting jitter triggers
                // browser NACK bursts. Drop the rate to 30 fps
                // (33 ms budget) which absorbs the readback cost
                // and produces an even cadence. Field log
                // 2026-04-27 from RoziLaptop -> Schetovodstvo-PZ
                // (Intel UHD 730) — the same heuristic as the
                // resolution cap, just for the time axis. Skipped
                // when target_fps was already <= 30 (operator
                // chose Software preference, or capture-side
                // downcap from a future tier).
                if backend_is_sw && target_fps > 30 {
                    let new_fps: u32 = 30;
                    tracing::warn!(
                        %session_id,
                        old_fps = target_fps,
                        new_fps,
                        codec = %chosen_codec,
                        encoder = enc_ref.name(),
                        "auto-fps-cap: SW backend at >30 fps target — rebuilding capturer at 30 fps to clear the capture-bottleneck drop rate"
                    );
                    target_fps = new_fps;
                    frame_duration_floor = Duration::from_micros(1_000_000 / target_fps as u64);
                    capturer = capture::open_default(target_fps, downscale);
                }
            }
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
            let base = encode::initial_bitrate_for_fps(w, h, target_fps);
            let quality_target = quality::target_bitrate(q_now, base);
            // If REMB hasn't reported, defer to the quality-derived
            // target. Once it does, take min(quality, remb*safety) so
            // the controller can ratchet down further on a metered
            // link but never push past what the receiver thinks the
            // path can carry.
            let target = if remb_now == 0 {
                quality_target
            } else {
                let remb_safe =
                    (remb_now / REMB_SAFETY_FACTOR_DEN).saturating_mul(REMB_SAFETY_FACTOR_NUM);
                // Floor: 500 kbps was unreadable at 1080p HEVC (green
                // chroma artefacts, blurred PowerShell text — the
                // 2026-04-24 field report). Use the larger of a flat
                // MIN_BITRATE_BPS and 25 % of the resolution-derived
                // target. At 1080p this is ~2.5 Mbps (vs 500 kbps
                // previously) — still severely degraded on a bad
                // link but keeps small-font text legible. REMB
                // reports below this get clamped up; if the link
                // really can't carry that much we'll see packet loss
                // escalate which REMB then ratchets further down and
                // the hysteresis re-applies.
                let floor = encode::MIN_BITRATE_BPS.max(base / 4);
                quality_target.min(remb_safe.max(floor))
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
        let encode_started = std::time::Instant::now();
        let packets = match enc.encode(frame).await {
            Ok(p) => p,
            Err(e) => {
                warn!(%session_id, %e, "encode error — stopping media pump");
                return;
            }
        };
        encode_time_us = encode_time_us.saturating_add(encode_started.elapsed().as_micros() as u64);

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
            // Average per-stage microseconds over the preceding 30-frame
            // window (not the whole session), so transient stalls
            // don't get smeared away by hours of steady operation.
            let frames_in_window = frames_encoded.saturating_sub(heartbeat_frames_base).max(1);
            let capture_us_window = capture_time_us.saturating_sub(heartbeat_capture_us_base);
            let encode_us_window = encode_time_us.saturating_sub(heartbeat_encode_us_base);
            let avg_capture_ms = capture_us_window / (1_000 * frames_in_window);
            let avg_encode_ms = encode_us_window / (1_000 * frames_in_window);
            info!(
                %session_id,
                backend,
                frames_captured, frames_empty, frames_encoded, frames_keepalive,
                bytes_written, write_errors,
                avg_capture_ms, avg_encode_ms,
                "media pump heartbeat (≈1s window)"
            );
            heartbeat_frames_base = frames_encoded;
            heartbeat_capture_us_base = capture_time_us;
            heartbeat_encode_us_base = encode_time_us;
        }
    }
}

/// Length-prefix an encoded VP9 frame for the `video-bytes` DC. The
/// header layout matches `ui/src/workers/rc-vp9-444-worker.ts`
/// (lines 16-23 of that file):
///
/// ```text
/// u32 size_le;       // payload length, little-endian
/// u8  flags;         // bit 0 = keyframe
/// u64 timestamp_us;  // monotonic capture timestamp
/// [u8] payload;      // raw VP9 frame
/// ```
///
/// Exported `pub(crate)` so the unit tests can lock the wire format.
/// `dead_code` allowance is for builds without the `vp9-444` feature
/// where the function has no caller — the tests still exercise it
/// under either feature flag setting.
#[allow(dead_code)]
pub(crate) fn frame_video_bytes(payload: &[u8], is_keyframe: bool, timestamp_us: u64) -> Vec<u8> {
    const HEADER_BYTES: usize = 13;
    let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
    let size = payload.len() as u32;
    out.extend_from_slice(&size.to_le_bytes());
    out.push(if is_keyframe { 0x01 } else { 0x00 });
    out.extend_from_slice(&timestamp_us.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Phase Y.3 alternate media pump: capture → libvpx VP9 4:4:4 encode
/// → length-prefixed `video-bytes` DC. No webrtc track involvement.
///
/// Behaviour parity with the legacy pump where it matters:
/// - Resolution-change rebuild (encoder is keyed on (w, h))
/// - Keyframe-on-request (browser PLI / fresh-DC equivalent)
/// - Heartbeat log every ~30 frames so a stalled pump is greppable
/// - Idle keepalive at 1 fps so the decoder doesn't pause
///
/// What's intentionally absent:
/// - REMB-driven adaptive bitrate: REMB is a WebRTC-RTP feedback
///   mechanism. The DC has no equivalent; we'd need a back-channel
///   `rc:vp9.bandwidth` message from the controller (next sprint).
///   Today the encoder runs at its `DEFAULT_BITRATE_BPS` ceiling.
/// - SDP renegotiation: there's no track to renegotiate; the worker
///   reconfigures its `VideoDecoder` on each keyframe-with-new-dims
///   automatically.
#[cfg(feature = "vp9-444")]
async fn media_pump_vp9_444_dc(
    session_id: bson::oid::ObjectId,
    video_bytes_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    keyframe_requested: Arc<std::sync::atomic::AtomicBool>,
    target_resolution: Arc<std::sync::Mutex<TargetResolution>>,
) {
    use crate::encode::libvpx::Vp9Encoder;

    // VP9 4:4:4 is heavy CPU; we cap fps to 30 right out of the gate.
    // The `vp9-444` path is always SW (libvpx); no HW VP9 4:4:4
    // encoder is available cross-platform today.
    let target_fps: u32 = 30;
    let frame_duration_floor = Duration::from_micros(1_000_000 / target_fps as u64);
    // BGRA capture; never downscale (libvpx + dcv_color_primitives
    // BGRA→I444 is fast enough at 1080p without the 2× capture
    // downsample). Operator-controlled `rc:resolution` still applies
    // via target_resolution on the post-capture path.
    let downscale = crate::capture::DownscalePolicy::Never;
    info!(
        %session_id,
        target_fps,
        "VP9-444 DC pump starting"
    );
    let mut capturer = capture::open_default(target_fps, downscale);
    let mut encoder: Option<Vp9Encoder> = None;
    let mut encoder_dims: Option<(u32, u32)> = None;
    let mut last_capture_at = std::time::Instant::now();
    let mut last_good_frame: Option<std::sync::Arc<crate::capture::Frame>> = None;
    const IDLE_KEEPALIVE: Duration = Duration::from_millis(1_000);
    let start = std::time::Instant::now();

    let mut frames_captured: u64 = 0;
    let mut frames_encoded: u64 = 0;
    let mut frames_sent: u64 = 0;
    let mut bytes_written: u64 = 0;
    let mut send_errors: u64 = 0;
    let mut dc_unopen_drops: u64 = 0;

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
                if last_capture_at.elapsed() >= IDLE_KEEPALIVE {
                    if let Some(ref f) = last_good_frame {
                        last_capture_at = std::time::Instant::now();
                        f.clone()
                    } else {
                        tokio::time::sleep(frame_duration_floor).await;
                        continue;
                    }
                } else {
                    tokio::time::sleep(frame_duration_floor / 2).await;
                    continue;
                }
            }
            Err(e) => {
                warn!(%session_id, %e, "VP9-444 capture error — rebuilding capturer");
                tokio::time::sleep(Duration::from_millis(500)).await;
                capturer = capture::open_default(target_fps, downscale);
                encoder = None;
                encoder_dims = None;
                continue;
            }
        };

        // Apply controller-chosen resolution + the libvpx even-dim
        // requirement. The encoder rejects odd dims — round down by 1
        // to cover the rare case where the resolution control message
        // landed an odd value.
        let frame = apply_target_resolution(frame, *target_resolution.lock().unwrap());
        let w = frame.width & !1;
        let h = frame.height & !1;
        if w != frame.width || h != frame.height {
            // Drop this frame; the next one will arrive at-or-near the
            // same dims and we'll handle the rebuild then. Safer than
            // shrinking the buffer in-place and risking off-by-one.
            continue;
        }

        if encoder_dims != Some((w, h)) {
            info!(%session_id, w, h, "VP9-444 encoder rebuild for dims");
            match Vp9Encoder::new(w, h) {
                Ok(e) => {
                    encoder = Some(e);
                    encoder_dims = Some((w, h));
                }
                Err(e) => {
                    warn!(%session_id, %e, "Vp9Encoder::new failed — pump exits");
                    return;
                }
            }
        }
        let enc = encoder.as_mut().unwrap();
        if keyframe_requested.swap(false, std::sync::atomic::Ordering::Relaxed) {
            enc.request_keyframe();
        }

        let packets = match enc.encode(frame).await {
            Ok(p) => p,
            Err(e) => {
                warn!(%session_id, %e, "VP9-444 encode error — pump exits");
                return;
            }
        };
        frames_encoded += packets.len() as u64;

        // Pull the DC handle once per frame. `try_lock` would race
        // with the on_data_channel callback that stashes it; the
        // contention here is microseconds.
        let dc_opt = video_bytes_dc.lock().await.clone();
        let Some(dc) = dc_opt else {
            // DC not yet open — drop frames until the controller
            // opens it. Common during the first ~100 ms of a session
            // (offer/answer + ICE + SCTP handshake). Counted so a
            // controller that never opens the DC is greppable.
            dc_unopen_drops += packets.len() as u64;
            continue;
        };
        if dc.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {
            dc_unopen_drops += packets.len() as u64;
            continue;
        }

        for p in packets {
            let ts_us = start.elapsed().as_micros() as u64;
            let wire = frame_video_bytes(&p.data, p.is_keyframe, ts_us);
            let wire_len = wire.len() as u64;
            match dc.send(&Bytes::from(wire)).await {
                Ok(_) => {
                    frames_sent += 1;
                    bytes_written += wire_len;
                }
                Err(e) => {
                    send_errors += 1;
                    warn!(%session_id, %e, send_errors, "VP9-444 DC send failed");
                }
            }
        }

        if frames_encoded.is_multiple_of(30) {
            info!(
                %session_id,
                frames_captured, frames_encoded, frames_sent, bytes_written,
                send_errors, dc_unopen_drops,
                "VP9-444 DC pump heartbeat (≈1s window)"
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
    target_resolution: Arc<std::sync::Mutex<TargetResolution>>,
) {
    dc.on_message(Box::new(move |msg| {
        let quality_state = quality_state.clone();
        let target_resolution = target_resolution.clone();
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
                "rc:resolution" => {
                    let mode = val.get("mode").and_then(|v| v.as_str()).unwrap_or("");
                    let new_target = match mode {
                        "original" => TargetResolution::Native,
                        "fit" | "custom" => {
                            let raw_w = val.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                            let raw_h = val.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                            if raw_w == 0 || raw_h == 0 {
                                debug!(
                                    %session_id, mode,
                                    "control: rc:resolution missing/invalid width/height — dropped"
                                );
                                return;
                            }
                            // MF HEVC encoder requires even-dimensioned
                            // input. A browser sending Fit dimensions
                            // derived from a stage element at
                            // 2154×1077 (the 1077 is odd) would bomb
                            // `MfEncoder::new_hevc` at session rebuild
                            // time, which fail-closed demotes to
                            // NoopEncoder — black screen for the rest
                            // of the session with no way to recover
                            // short of reconnect. Floor to the
                            // nearest-lower even number here so a
                            // browser that forgot to round can't brick
                            // the encoder. Clamp minima to 160×90 —
                            // below that most hardware MFTs reject.
                            let w = (raw_w & !1).max(160);
                            let h = (raw_h & !1).max(90);
                            if (w, h) != (raw_w, raw_h) {
                                debug!(
                                    %session_id, mode,
                                    raw_w, raw_h, w, h,
                                    "control: rc:resolution rounded to even dims"
                                );
                            }
                            TargetResolution::Fixed {
                                width: w,
                                height: h,
                            }
                        }
                        other => {
                            debug!(
                                %session_id, mode = other,
                                "control: rc:resolution unknown mode — dropped"
                            );
                            return;
                        }
                    };
                    let mut slot = target_resolution.lock().unwrap();
                    let prev = *slot;
                    if prev != new_target {
                        *slot = new_target;
                        info!(
                            %session_id,
                            mode,
                            ?prev,
                            new_target = ?new_target,
                            "control: rc:resolution updated"
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

/// Downscale a captured frame to the controller-chosen target
/// resolution. `TargetResolution::Native` is a no-op; `Fixed` sizes
/// larger or equal to the capture are also no-ops (upscaling serves
/// no purpose — the encoder just gets interpolated pixels). Returns
/// the same `Arc<Frame>` when no work is needed, so idle sessions
/// don't pay the allocator cost.
fn apply_target_resolution(
    frame: std::sync::Arc<crate::capture::Frame>,
    target: TargetResolution,
) -> std::sync::Arc<crate::capture::Frame> {
    let (tw, th) = match target {
        TargetResolution::Native => return frame,
        TargetResolution::Fixed { width, height } => (width, height),
    };
    if tw >= frame.width && th >= frame.height {
        // Cap at native — don't upscale.
        return frame;
    }
    if tw == 0 || th == 0 {
        return frame;
    }
    if frame.pixel_format != crate::capture::PixelFormat::Bgra {
        // Non-BGRA frames shouldn't reach this point today (both scrap
        // and WGC emit BGRA), but be defensive — pass through rather
        // than produce a mis-formatted downscale.
        return frame;
    }
    let downscaled =
        downscale_bgra_box(&frame.data, frame.width, frame.height, frame.stride, tw, th);
    std::sync::Arc::new(crate::capture::Frame {
        width: tw,
        height: th,
        stride: tw * 4,
        pixel_format: crate::capture::PixelFormat::Bgra,
        data: downscaled,
        monotonic_us: frame.monotonic_us,
        monitor: frame.monitor,
        // Dirty rects at native scale; after downscale they'd need
        // re-projection. The encoder's ROI hook treats an empty list
        // as "unknown" which falls back to full-frame encoding — safe
        // default until we wire per-rect scaling.
        dirty_rects: Vec::new(),
    })
}

/// CPU box-filter downscale for BGRA frames. For each destination
/// pixel, averages the source pixels inside the mapped rectangle.
/// Handles non-integer ratios (e.g. 3840×2160 → 1920×1200). ~30 ms
/// on 4K→1080p on a modern laptop CPU; good enough for 30 fps and
/// tolerable at 60 fps. GPU path via VideoProcessorMFT is the
/// follow-up (deferred Tier C/1C.3 in the RustDesk-parity plan).
fn downscale_bgra_box(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    src_stride: u32,
    dst_w: u32,
    dst_h: u32,
) -> Vec<u8> {
    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    let src_w_u = src_w as u64;
    let src_h_u = src_h as u64;
    for dy in 0..dst_h {
        let sy_start = (dy as u64 * src_h_u / dst_h as u64) as u32;
        let sy_end_raw = ((dy as u64 + 1) * src_h_u).div_ceil(dst_h as u64) as u32;
        let sy_end = sy_end_raw.min(src_h);
        for dx in 0..dst_w {
            let sx_start = (dx as u64 * src_w_u / dst_w as u64) as u32;
            let sx_end_raw = ((dx as u64 + 1) * src_w_u).div_ceil(dst_w as u64) as u32;
            let sx_end = sx_end_raw.min(src_w);
            let mut b: u32 = 0;
            let mut g: u32 = 0;
            let mut r: u32 = 0;
            let mut a: u32 = 0;
            let mut n: u32 = 0;
            for sy in sy_start..sy_end {
                let row_base = (sy * src_stride) as usize;
                for sx in sx_start..sx_end {
                    let i = row_base + (sx as usize) * 4;
                    b += src[i] as u32;
                    g += src[i + 1] as u32;
                    r += src[i + 2] as u32;
                    a += src[i + 3] as u32;
                    n += 1;
                }
            }
            if let Some(divisor) = std::num::NonZeroU32::new(n) {
                let di = ((dy * dst_w + dx) as usize) * 4;
                dst[di] = (b / divisor.get()) as u8;
                dst[di + 1] = (g / divisor.get()) as u8;
                dst[di + 2] = (r / divisor.get()) as u8;
                dst[di + 3] = (a / divisor.get()) as u8;
            }
        }
    }
    dst
}

/// Placeholder handler for data channels that aren't wired to OS output
/// yet (`files`). Logs message sizes so we can see activity without
/// spamming the log with contents.
fn attach_log_only(dc: Arc<RTCDataChannel>, session_id: bson::oid::ObjectId) {
    let label = dc.label().to_string();
    dc.on_message(Box::new(move |msg| {
        debug!(%session_id, %label, bytes = msg.data.len(), "DC msg (unhandled)");
        Box::pin(async {})
    }));
}

/// Wire the `clipboard` DC to the agent's OS clipboard. Parses
/// inbound JSON as [`clipboard::ClipboardIncoming`] and dispatches:
///
/// - `clipboard:write { text }` — replace the OS clipboard with the
///   payload; no response (fire-and-forget).
/// - `clipboard:read { req_id? }` — read current OS clipboard text and
///   reply with `clipboard:content { text, req_id }`. Errors reply
///   with `clipboard:error { message }` so the browser can surface
///   the failure in a toast.
///
/// A single [`crate::clipboard::Clipboard`] is created per session; it
/// owns a thread-pinned `arboard::Clipboard`. On init failure we log
/// and leave the DC as a no-op (browser reads time out, writes are
/// silently dropped — no worse than pre-0.1.33).
#[cfg(feature = "clipboard")]
fn attach_clipboard_handler(dc: Arc<RTCDataChannel>, session_id: bson::oid::ObjectId) {
    let cb = match crate::clipboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            warn!(%session_id, %e, "clipboard: init failed — DC will no-op");
            return;
        }
    };
    let dc_for_handler = dc.clone();
    dc.on_message(Box::new(move |msg| {
        let dc = dc_for_handler.clone();
        let cb = cb.clone();
        Box::pin(async move {
            let Ok(text) = std::str::from_utf8(&msg.data) else {
                debug!(%session_id, bytes = msg.data.len(), "clipboard: non-UTF8 payload ignored");
                return;
            };
            let parsed: Result<crate::clipboard::ClipboardIncoming, _> = serde_json::from_str(text);
            let parsed = match parsed {
                Ok(p) => p,
                Err(e) => {
                    debug!(%session_id, %e, "clipboard: unparseable JSON");
                    return;
                }
            };
            match parsed {
                crate::clipboard::ClipboardIncoming::Write { text } => {
                    let bytes = text.len();
                    match cb.write(text).await {
                        Ok(()) => info!(%session_id, bytes, "clipboard: wrote to host"),
                        Err(e) => {
                            warn!(%session_id, %e, "clipboard: write failed");
                            let reply = serde_json::json!({
                                "t": "clipboard:error",
                                "message": format!("{e}"),
                            });
                            if let Ok(s) = serde_json::to_string(&reply) {
                                let _ = dc.send_text(s).await;
                            }
                        }
                    }
                }
                crate::clipboard::ClipboardIncoming::Read { req_id } => match cb.read().await {
                    Ok(text) => {
                        let bytes = text.len();
                        info!(%session_id, bytes, "clipboard: read from host");
                        let reply = serde_json::json!({
                            "t": "clipboard:content",
                            "text": text,
                            "req_id": req_id,
                        });
                        if let Ok(s) = serde_json::to_string(&reply) {
                            let _ = dc.send_text(s).await;
                        }
                    }
                    Err(e) => {
                        warn!(%session_id, %e, "clipboard: read failed");
                        let reply = serde_json::json!({
                            "t": "clipboard:error",
                            "message": format!("{e}"),
                            "req_id": req_id,
                        });
                        if let Ok(s) = serde_json::to_string(&reply) {
                            let _ = dc.send_text(s).await;
                        }
                    }
                },
            }
        })
    }));
}

/// Wire the `files` DC to a per-session file-transfer handler. Strings
/// carry control frames (`files:begin`/`files:end` + agent replies);
/// binary frames are chunk payloads appended to the current in-flight
/// transfer. The handler enforces one active transfer at a time and
/// replies with `files:accepted` / `files:progress` / `files:complete`
/// / `files:error` over the same channel.
fn attach_files_handler(dc: Arc<RTCDataChannel>, session_id: bson::oid::ObjectId) {
    let handler = crate::files::FilesHandler::new();
    let dc_for_handler = dc.clone();
    let handler_for_close = handler.clone();
    dc.on_close(Box::new(move || {
        let h = handler_for_close.clone();
        Box::pin(async move {
            h.abort().await;
        })
    }));
    dc.on_message(Box::new(move |msg| {
        let dc = dc_for_handler.clone();
        let handler = handler.clone();
        Box::pin(async move {
            if msg.is_string {
                handle_files_control(dc, handler, session_id, &msg.data).await;
            } else {
                handle_files_chunk(dc, handler, session_id, &msg.data).await;
            }
        })
    }));
}

async fn handle_files_control(
    dc: Arc<RTCDataChannel>,
    handler: crate::files::FilesHandler,
    session_id: bson::oid::ObjectId,
    data: &[u8],
) {
    let Ok(text) = std::str::from_utf8(data) else {
        debug!(%session_id, bytes = data.len(), "files: non-UTF8 control ignored");
        return;
    };
    let parsed: Result<crate::files::FilesIncoming, _> = serde_json::from_str(text);
    let parsed = match parsed {
        Ok(p) => p,
        Err(e) => {
            debug!(%session_id, %e, "files: unparseable control JSON");
            return;
        }
    };
    match parsed {
        crate::files::FilesIncoming::Begin {
            id,
            name,
            size,
            mime,
        } => {
            info!(%session_id, %id, %name, size, ?mime, "files: begin");
            match handler.begin(id.clone(), name, size).await {
                Ok(path) => {
                    let path_str = path.to_string_lossy();
                    send_files_json(
                        &dc,
                        &crate::files::FilesOutgoing::Accepted {
                            id: &id,
                            path: &path_str,
                        },
                    )
                    .await;
                }
                Err(e) => {
                    warn!(%session_id, %id, %e, "files: begin failed");
                    let msg = format!("{e}");
                    send_files_json(
                        &dc,
                        &crate::files::FilesOutgoing::Error {
                            id: &id,
                            message: &msg,
                        },
                    )
                    .await;
                }
            }
        }
        crate::files::FilesIncoming::End { id } => match handler.end(&id).await {
            Ok((path, bytes)) => {
                info!(%session_id, %id, bytes, path = %path.display(), "files: complete");
                let path_str = path.to_string_lossy();
                send_files_json(
                    &dc,
                    &crate::files::FilesOutgoing::Complete {
                        id: &id,
                        path: &path_str,
                        bytes,
                    },
                )
                .await;
            }
            Err(e) => {
                warn!(%session_id, %id, %e, "files: end failed");
                let msg = format!("{e}");
                send_files_json(
                    &dc,
                    &crate::files::FilesOutgoing::Error {
                        id: &id,
                        message: &msg,
                    },
                )
                .await;
            }
        },
    }
}

async fn handle_files_chunk(
    dc: Arc<RTCDataChannel>,
    handler: crate::files::FilesHandler,
    session_id: bson::oid::ObjectId,
    data: &[u8],
) {
    match handler.chunk(data).await {
        Ok(Some(progress)) => {
            send_files_json(
                &dc,
                &crate::files::FilesOutgoing::Progress {
                    id: &progress.id,
                    bytes: progress.bytes,
                },
            )
            .await;
        }
        Ok(None) => {
            // Below the progress-report threshold; nothing to send.
        }
        Err(e) => {
            warn!(%session_id, %e, "files: chunk failed");
            let msg = format!("{e}");
            send_files_json(
                &dc,
                &crate::files::FilesOutgoing::Error {
                    id: "",
                    message: &msg,
                },
            )
            .await;
            handler.abort().await;
        }
    }
}

async fn send_files_json(dc: &Arc<RTCDataChannel>, msg: &crate::files::FilesOutgoing<'_>) {
    if let Ok(s) = serde_json::to_string(msg) {
        let _ = dc.send_text(s).await;
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

/// Build the `RTCRtpCodecCapability` for the negotiated codec. Matches
/// webrtc-rs's `register_default_codecs` entries byte-for-byte so the
/// internal `payloader_for_codec` lookup resolves and the SDP answer
/// carries the expected payload type.
///
/// Default MediaEngine registrations (webrtc-rs 0.12):
///   video/H264 Constrained Baseline, packetization-mode=1,
///       profile-level-id=42e01f → PT 125
///   video/HEVC empty fmtp              → PT 126
///   video/AV1  profile-id=0            → PT 41
///
/// Unknown codec → H.264 default (paranoia: should never hit because
/// `pick_best_codec` only returns codecs both sides advertise).
fn build_video_codec_cap(codec: &str) -> RTCRtpCodecCapability {
    let feedback = vec![
        RTCPFeedback {
            typ: "goog-remb".to_string(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "ccm".to_string(),
            parameter: "fir".to_string(),
        },
        RTCPFeedback {
            typ: "nack".to_string(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "nack".to_string(),
            parameter: "pli".to_string(),
        },
        RTCPFeedback {
            typ: "transport-cc".to_string(),
            parameter: String::new(),
        },
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
            // MIME is "video/HEVC" to match webrtc-rs 0.12's
            // `MIME_TYPE_HEVC` constant (what `register_default_codecs`
            // registers and what `payloader_for_codec` looks up).
            // Using "video/H265" here fails the transceiver's codec
            // match with "unsupported codec type by this transceiver"
            // even though HEVC is identical to H.265 in the spec.
            mime_type: "video/HEVC".to_string(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: String::new(),
            rtcp_feedback: feedback,
        },
        _ => RTCRtpCodecCapability {
            mime_type: "video/H264".to_string(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
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
        assert_eq!(cap.mime_type, "video/HEVC");
        assert!(cap.sdp_fmtp_line.is_empty());
        let alias = build_video_codec_cap("hevc");
        assert_eq!(alias.mime_type, "video/HEVC");
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
        assert_eq!(build_video_codec_cap("HEVC").mime_type, "video/HEVC");
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
                cap.rtcp_feedback
                    .iter()
                    .any(|f| f.typ == "nack" && f.parameter == "pli"),
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
    fn target_bitrate_high_caps_at_30_mbps() {
        // 4K60 base is 25 Mbps clamped; High should add 50% capped at 30.
        assert_eq!(target_bitrate(HIGH, 12_000_000), 18_000_000);
        // Very high base: cap engages at the new 30M ceiling.
        assert_eq!(target_bitrate(HIGH, 30_000_000), 30_000_000);
        assert_eq!(target_bitrate(HIGH, 50_000_000), 30_000_000);
    }
}

#[cfg(test)]
mod video_bytes_wire_tests {
    use super::frame_video_bytes;

    /// Lock the exact byte layout that `rc-vp9-444-worker.ts`'s
    /// `parseFrameHeader` (lines 260-273 of that file) reads. A typo
    /// or endian flip on either side silently breaks decode; this
    /// test surfaces the mismatch in CI before the field does.
    ///
    /// Layout:
    ///   bytes [0..4)  payload-size, u32 little-endian
    ///   byte  [4]     flags (bit 0 = keyframe)
    ///   bytes [5..13) timestamp_us, u64 little-endian
    ///   bytes [13..)  payload
    #[test]
    fn header_layout_matches_worker_parser() {
        let payload = b"abcdef";
        let out = frame_video_bytes(payload, true, 0xDEAD_BEEF_CAFE_BABE);
        assert_eq!(out.len(), 13 + payload.len(), "header is 13 bytes");
        // size = 6, little-endian
        assert_eq!(&out[0..4], &[0x06, 0x00, 0x00, 0x00]);
        // flags = 0x01 (keyframe)
        assert_eq!(out[4], 0x01);
        // timestamp = 0xDEADBEEFCAFEBABE little-endian
        assert_eq!(
            &out[5..13],
            &[0xBE, 0xBA, 0xFE, 0xCA, 0xEF, 0xBE, 0xAD, 0xDE],
        );
        // payload follows verbatim
        assert_eq!(&out[13..], payload);
    }

    #[test]
    fn delta_frames_clear_keyframe_flag() {
        let out = frame_video_bytes(b"x", false, 0);
        assert_eq!(out[4], 0x00, "delta frame must not set the keyframe bit");
    }

    #[test]
    fn empty_payload_still_emits_full_13_byte_header() {
        // Edge case: libvpx can emit zero-byte show=0 hidden frames.
        // We pass them through; the worker drops them via the
        // `size === 0` branch.
        let out = frame_video_bytes(&[], true, 1);
        assert_eq!(out.len(), 13);
        assert_eq!(&out[0..4], &[0, 0, 0, 0]);
    }
}

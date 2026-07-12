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
// Only the VP9-444 DC pump consumes the per-session transport detection today
// (the FFmpeg pump joins in Phase B); keep the import off the signalling-only
// / FFmpeg-only builds so `clippy -D warnings` stays clean.
#[cfg(any(feature = "vp9-444", feature = "ffmpeg-encoder"))]
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::media::Sample;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::capture;
use crate::encode;
use crate::input;
use crate::lock_overlay;
use crate::lock_state;
use crate::logs_fetch;

/// rc.26 — true when the current process is running as the SystemContext
/// worker (LocalSystem in the user's interactive session). Captured
/// once per session; the answer doesn't change at runtime (process
/// identity is fixed at spawn time). Used to gate two lock-screen
/// policies that are correct for user-context but wrong for
/// SystemContext:
///
///  - **Capture overlay substitution** (`media_pump` / `media_pump_vp9_444_dc`):
///    user-context can't see Winlogon → we substitute a "Host is
///    locked" overlay frame so the operator sees something instead of
///    a frozen black image. SystemContext capture rebinds to
///    `winsta0\Winlogon` and produces real lock-screen pixels —
///    substituting an overlay over those wastes the work and prevents
///    the operator from seeing the password prompt.
///
///  - **Input suppression** (`attach_input_handler`): user-context
///    `SendInput` can't drive Winlogon (no SE_TCB privilege).
///    SystemContext (LocalSystem) holds SE_TCB and can. Suppressing
///    input under SystemContext blocks remote unlock for no security
///    reason — the operator already has agent access.
///
/// Compiled out on non-Windows; the gates collapse to "always false"
/// (matches the rc.25 behaviour on Linux/macOS, where there is no
/// lock-screen capture-rebind story).
#[cfg(all(feature = "system-context", target_os = "windows"))]
fn is_system_context_worker() -> bool {
    use crate::system_context::worker_role;
    matches!(
        worker_role::probe_self(),
        Ok(worker_role::WorkerRole::SystemContext)
    )
}

#[cfg(not(all(feature = "system-context", target_os = "windows")))]
fn is_system_context_worker() -> bool {
    false
}

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
    /// metered uplinks), High adds 50%. Ceiling lifted 30 → 50 Mbps in
    /// rc.36 (the field-test host / a second field-test host field test 2026-05-17) after the
    /// rc.33–rc.35 cycles still left fine-text legibility worse than
    /// RustDesk on common screen-content events (window-uncover,
    /// Outlook open). At 4K60 + High the resolution-derived base
    /// (`0.20 bpp × 3840×2160×60 ≈ 99.5 Mbps`) clamps to the
    /// `MAX_BITRATE_BPS = 40 Mbps` cap on the way in; `× 1.5` for
    /// High then lands on 50 Mbps after the post-multiply clamp —
    /// generous enough that scene-change frames can splurge without
    /// hitting the rate-control ceiling.
    pub(super) fn target_bitrate(quality: u8, base_bps: u32) -> u32 {
        const MAX_HIGH_BPS: u32 = 50_000_000;
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
    /// System-audio → Opus pump. `Some` only when the session
    /// negotiated `audio_enabled` AND the `audio` feature is compiled
    /// in. Held so `close()` can abort it alongside the video pump.
    #[cfg(feature = "audio")]
    audio_pump: Option<JoinHandle<()>>,
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
    ///
    /// rc.62 — `chroma_pref` is the per-session VP9 chroma override
    /// forwarded from `ClientMsg::SessionRequest::chroma_pref`. When
    /// `Some("yuv420" | "yuv444")` the VP9-444 pump uses it instead
    /// of the agent's `ROOMLER_AGENT_VP9_CHROMA` env var. `None` →
    /// fall back to env var (= operator default).
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        session_id: bson::oid::ObjectId,
        ice_servers: &[IceServer],
        outbound: mpsc::Sender<ClientMsg>,
        encoder_preference: encode::EncoderPreference,
        chosen_codec: String,
        negotiated_transport: Option<String>,
        chroma_pref: Option<String>,
        // Opt-in system-audio track. Only acted on when the `audio`
        // feature is compiled in; underscored-through otherwise so the
        // default-feature build doesn't warn on the unused binding.
        #[cfg_attr(not(feature = "audio"), allow(unused_variables))] audio_enabled: bool,
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

        // rc.162: hostile-NAT hosts (WSL2 + wsl-vpnkit, other userspace-VPN
        // stacks) mangle UDP source ports, breaking the TURN allocation
        // refresh — the media peer flaps Connected/Disconnected and the
        // desktop freezes. `ROOMLER_AGENT_ICE_RELAY_TCP=1` pins the media to
        // the TURNS/TCP relay (the vendored webrtc-ice TCP branch), a single
        // stable TCP connection that survives it — the same escape hatch the
        // tunnel uses on corp VPNs. Opt-in: the default path is unchanged.
        let relay_tcp = std::env::var("ROOMLER_AGENT_ICE_RELAY_TCP")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let mut config = RTCConfiguration {
            ice_servers: if relay_tcp {
                map_ice_servers_relay_tcp(ice_servers)
            } else {
                map_ice_servers(ice_servers)
            },
            ..Default::default()
        };
        if relay_tcp {
            config.ice_transport_policy = RTCIceTransportPolicy::Relay;
        }

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

        // Opt-in system-audio: add a sendonly Opus track + spawn the
        // audio pump. Gated on both the `audio` Cargo feature and the
        // per-session `audio_enabled` directive. The Opus capability
        // must match the MediaEngine's default Opus registration
        // byte-for-byte (see `build_audio_codec_cap`) or SDP negotiation
        // can't resolve PT 111. The track is added BEFORE the SDP answer
        // is created so the m=audio section is advertised; when audio is
        // off we add no track and the SDP carries video only (fully
        // backward-compatible with controllers that never request audio).
        #[cfg(feature = "audio")]
        let audio_pump_handle: Option<JoinHandle<()>> = if audio_enabled {
            let audio_track = Arc::new(TrackLocalStaticSample::new(
                build_audio_codec_cap(),
                "audio".to_owned(),
                "roomler-agent".to_owned(),
            ));
            pc.add_track(audio_track.clone() as Arc<dyn TrackLocal + Send + Sync>)
                .await
                .context("add_track(audio)")?;
            info!(%session_id, "audio: Opus track added — spawning audio pump");
            Some(tokio::spawn(audio_pump(session_id, audio_track)))
        } else {
            None
        };

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
        // Control-DC stash so the lock-state emitter task (spawned
        // alongside the media pump below) can write `rc:host_locked`
        // messages without a separate channel lookup. Tokio mutex
        // mirrors `video_bytes_dc`'s rationale: the on_data_channel
        // callback writes from an async context, the emitter reads
        // from its own task, both very briefly.
        let control_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>> =
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

        // PC state → logs + fatal Terminate on Failed + cross-process
        // peer-presence marker for the M3 A1 supervisor.
        //
        // The marker file (`%PROGRAMDATA%\roomler-agent\
        // peer-connected.lock`) is the supervisor's signal for
        // "swap user-context worker for SystemContext worker
        // because a controller is currently driving this host".
        // See `system_context::peer_presence` for the contract.
        // On `Connected` we touch the marker (and the periodic
        // refresher task below keeps the mtime fresh); on
        // `Disconnected` / `Closed` / `Failed` we remove it so the
        // supervisor's next cycle reverts to the user-context arm.
        {
            let tx = outbound.clone();
            pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                info!(session = %session_id, state = ?s, "PC state change");
                let tx = tx.clone();
                Box::pin(async move {
                    #[cfg(all(feature = "system-context", target_os = "windows"))]
                    {
                        match s {
                            RTCPeerConnectionState::Connected => {
                                if let Err(e) = crate::system_context::peer_presence::signal_connected() {
                                    tracing::warn!(%e, "peer_presence::signal_connected failed — supervisor cannot swap to SystemContext worker");
                                }
                            }
                            RTCPeerConnectionState::Disconnected
                            | RTCPeerConnectionState::Closed
                            | RTCPeerConnectionState::Failed => {
                                if let Err(e) = crate::system_context::peer_presence::signal_disconnected() {
                                    tracing::debug!(%e, "peer_presence::signal_disconnected — already gone or unreachable");
                                }
                            }
                            _ => {}
                        }
                    }
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

        // M3 A1 peer-presence heartbeat. Refreshes the marker file's
        // mtime every 5 s while the WebRTC peer is in `Connected`
        // state; the supervisor's `is_signaled` returns false once
        // the file's mtime is older than `PRESENCE_MAX_AGE` (15 s).
        // This task is spawned once per session and exits when the
        // peer connection drops or fails — its `Arc<RTCPeerConnection>`
        // weak-clone won't keep the connection alive.
        #[cfg(all(feature = "system-context", target_os = "windows"))]
        {
            let pc_for_heartbeat = std::sync::Arc::downgrade(&pc);
            tokio::spawn(async move {
                use crate::system_context::peer_presence;
                let mut tick = tokio::time::interval(peer_presence::HEARTBEAT_INTERVAL);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut ticks: u64 = 0;
                let mut had_success = false;
                loop {
                    tick.tick().await;
                    let Some(pc) = pc_for_heartbeat.upgrade() else {
                        // Peer connection dropped; remove the marker
                        // so the supervisor doesn't see a stale
                        // "connected" signal until PRESENCE_MAX_AGE
                        // expires.
                        let _ = peer_presence::signal_disconnected();
                        return;
                    };
                    if matches!(pc.connection_state(), RTCPeerConnectionState::Connected) {
                        match peer_presence::signal_connected() {
                            Ok(()) => {
                                ticks = ticks.saturating_add(1);
                                // Log the FIRST successful write loudly
                                // so a "supervisor never sees marker"
                                // investigation can immediately rule
                                // out "worker never wrote it". After
                                // that, every 12th tick (~60 s) so
                                // the log stays clean during a long
                                // session.
                                if !had_success {
                                    let path = peer_presence::marker_path().display().to_string();
                                    tracing::info!(
                                        marker_path = %path,
                                        "peer_presence: first heartbeat written successfully"
                                    );
                                    had_success = true;
                                } else if ticks.is_multiple_of(12) {
                                    tracing::debug!(ticks, "peer_presence: heartbeat still alive");
                                }
                            }
                            Err(e) => {
                                let path = peer_presence::marker_path().display().to_string();
                                tracing::warn!(
                                    %e,
                                    marker_path = %path,
                                    "peer_presence heartbeat write failed — supervisor cannot swap to SystemContext worker"
                                );
                            }
                        }
                    }
                }
            });
        }

        // Spawn the lock-screen monitor BEFORE wiring the data-channel
        // callback so the input handler can subscribe to LockState
        // transitions and drop input events early when the host is
        // locked. Without this the events would be dispatched to
        // SendInput which silently routes them to the wrong desktop
        // (the user-context worker is on `winsta0\Default`, but the
        // input desktop is `winsta0\Winlogon`) — they appear to "work"
        // from the WS side but achieve nothing on the host. Dropping
        // them in user-space avoids polluting `enigo` logs and lets
        // a future browser-side hint surface "input suppressed" to
        // the operator.
        let (lock_state_rx, _lock_state_handle) = lock_state::spawn_monitor();

        // Route data channels by label. `input` goes to the OS injector;
        // `control` parses rc:* JSON (quality preference, etc.);
        // `cursor` receives an agent-driven stream of position / shape
        // messages pumped from CursorTracker; `clipboard` round-trips
        // text between the agent's OS clipboard and the browser;
        // `files` accepts uploads that land in the controlled host's
        // Downloads folder.
        let quality_for_dc = quality_state.clone();
        let target_res_for_dc = target_resolution.clone();
        // rc.130 — the control DC handler forces an encoder keyframe on the
        // browser's `rc:keyframe` (sent when its decode queue backs up and it
        // drops deltas to resync). Same atomic the media pumps already poll.
        let keyframe_for_dc = keyframe_requested.clone();
        let video_bytes_dc_for_callback = video_bytes_dc.clone();
        let control_dc_for_callback = control_dc.clone();
        let lock_state_rx_for_dc = lock_state_rx.clone();
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let label = dc.label().to_string();
            info!(session = %session_id, %label, "data channel opened");
            let quality_for_dc = quality_for_dc.clone();
            let target_res_for_dc = target_res_for_dc.clone();
            let keyframe_for_dc = keyframe_for_dc.clone();
            let video_bytes_stash = video_bytes_dc_for_callback.clone();
            let control_stash = control_dc_for_callback.clone();
            let lock_state_rx_for_input = lock_state_rx_for_dc.clone();
            Box::pin(async move {
                match label.as_str() {
                    "input" => attach_input_handler(dc, lock_state_rx_for_input),
                    "control" => {
                        // Stash a clone for the lock-state emitter
                        // BEFORE handing the DC to the inbound handler.
                        // attach_control_handler consumes the Arc by
                        // value to install on_message; without the
                        // pre-clone-and-stash, the emitter task would
                        // have no way to write outbound messages.
                        *control_stash.lock().await = Some(dc.clone());
                        attach_control_handler(
                            dc,
                            session_id,
                            quality_for_dc,
                            target_res_for_dc,
                            keyframe_for_dc,
                        )
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

        // Spawn the host-locked emitter: watches the lock_state
        // monitor's transitions and emits `rc:host_locked` over the
        // `control` data channel so the viewer can render an explicit
        // toolbar badge alongside the in-stream padlock overlay.
        // The task self-terminates when the receiver closes (pump
        // exit) or when send to the DC fails (peer gone).
        {
            let mut rx = lock_state_rx.clone();
            let stash = control_dc.clone();
            tokio::spawn(async move {
                // Send the initial state once the control DC is
                // available. The first `changed().await` fires only
                // on subsequent transitions, but the operator's UI
                // needs to know if the host is *already* locked at
                // session start.
                let mut prev = *rx.borrow();
                emit_host_locked(&stash, prev == lock_state::LockState::Locked).await;
                while rx.changed().await.is_ok() {
                    let current = *rx.borrow();
                    if current != prev {
                        emit_host_locked(&stash, current == lock_state::LockState::Locked).await;
                        prev = current;
                    }
                }
            });
        }

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
            chroma_pref,
            video_bytes_dc.clone(),
            lock_state_rx,
            // rc.87 — control DC so the DC video pumps can emit
            // `rc:video-info` (real encoder/codec/chroma) to the browser
            // for an honest stats badge.
            control_dc.clone(),
            pc.clone(),
        ));

        Ok(Self {
            pc,
            session_id,
            media_pump: Some(pump),
            #[cfg(feature = "audio")]
            audio_pump: audio_pump_handle,
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
        #[cfg(feature = "audio")]
        if let Some(pump) = &self.audio_pump {
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

/// Detect whether THIS session's negotiated ICE path runs through a TURN
/// relay, by inspecting the selected candidate pair. A relayed path (TURN,
/// especially over TCP on WSL / corp-UDP-blocked nets) is bandwidth- and
/// head-of-line-constrained, so the DC pumps clamp their bitrate ceiling to
/// `relay_max_bps()` for it. Unlike the process-wide
/// `ROOMLER_AGENT_ICE_RELAY_TCP` env flag, this is PER SESSION — the same
/// agent process serves both direct-local and cross-host-relay controllers
/// (e.g. the WSL virtual-desktop agent advertises a direct mirrored-network
/// path to a LAN browser AND a TURN-relayed path to a remote one), so the
/// env flag mis-classifies one of them.
///
/// The explicit env flag still wins as an OVERRIDE: vd-mode / the corp path
/// force `ice_transport_policy=Relay` up front, so the path IS relayed and
/// there's nothing to detect. Otherwise poll the selected pair briefly (ICE
/// may not have nominated the instant the pump starts) and fall back to
/// "unconstrained" if it hasn't nominated within ~3 s — the AIMD converges
/// regardless of the initial guess.
///
/// Gated on `any(vp9-444, ffmpeg-encoder)` — both DataChannel pumps call it
/// (Phase B added the FFmpeg HEVC/vp9_qsv pump as the second caller).
#[cfg(any(feature = "vp9-444", feature = "ffmpeg-encoder"))]
async fn detect_constrained_transport(
    pc: &Arc<RTCPeerConnection>,
    session_id: bson::oid::ObjectId,
) -> bool {
    if crate::encode::transport_is_constrained() {
        return true;
    }
    // Bind each Arc so the borrowed `&RTCIceTransport` outlives the chain.
    let sctp = pc.sctp();
    let dtls = sctp.transport();
    let ice = dtls.ice_transport();
    for _ in 0..30 {
        if let Some(pair) = ice.get_selected_candidate_pair().await {
            let relay = pair.local().typ == RTCIceCandidateType::Relay
                || pair.remote().typ == RTCIceCandidateType::Relay;
            info!(
                %session_id,
                relay,
                local_typ = ?pair.local().typ,
                local_proto = ?pair.local().protocol,
                remote_typ = ?pair.remote().typ,
                remote_proto = ?pair.remote().protocol,
                "per-session ICE path detected (adaptive bitrate)"
            );
            return relay;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    warn!(
        %session_id,
        "ICE candidate pair not nominated within 3s — treating as direct (unconstrained)"
    );
    false
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
    chroma_pref: Option<String>,
    video_bytes_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    lock_state_rx: tokio::sync::watch::Receiver<lock_state::LockState>,
    control_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    // Adaptive bitrate — the peer connection, so the DC pumps can detect
    // THIS session's actual ICE path (relay vs direct) at runtime instead
    // of the process-wide `ROOMLER_AGENT_ICE_RELAY_TCP` env flag.
    pc: Arc<RTCPeerConnection>,
) {
    // `pc` is consumed only by the VP9-444 DC pump's per-session transport
    // detection (feature-gated); keep the signalling-only / non-vp9 build
    // warning-clean.
    #[cfg(not(feature = "vp9-444"))]
    let _ = &pc;
    // Tracks the lock-state value seen on the previous loop iteration
    // so we can request an encoder keyframe on each transition. The
    // browser decoder otherwise has to wait for the next periodic
    // intra-refresh to actually render the overlay (or the resumed
    // desktop on unlock), which on a live session can be 1-2 seconds
    // of stale-then-suddenly-correct frames.
    let mut was_locked_last_iter = matches!(*lock_state_rx.borrow(), lock_state::LockState::Locked);
    // rc.26 — probe SystemContext once at pump start. Captured into a
    // local bool so the per-frame check is a single comparison.
    // SystemContext capture rebinds to winsta0\Winlogon on lock; the
    // operator should see real lock-screen pixels (and be able to
    // type the password), NOT the "Host is locked" overlay placeholder.
    let sys_ctx_worker = is_system_context_worker();
    if sys_ctx_worker {
        info!(
            %session_id,
            "media_pump: SystemContext worker — lock overlay disabled (real Winlogon frames will stream)"
        );
    }
    // rc.77 — HEVC over DataChannel fork (Option B). Same shape as
    // the VP9-444 path below: when the session negotiated HEVC over
    // the `video-bytes` channel, route to the FFmpeg-encoder DC pump.
    // Falls through to the VP9-444 path or legacy track-based pump
    // when not selected — including when the feature is compiled in
    // but `ROOMLER_AGENT_USE_FFMPEG=1` isn't set on this process
    // (caps probe wouldn't have advertised the transport, but a
    // mismatched / old controller could still ask for it).
    if matches!(negotiated_transport.as_deref(), Some("data-channel-hevc")) {
        #[cfg(feature = "ffmpeg-encoder")]
        {
            if crate::encode::ffmpeg::available() {
                tracing::info!(
                    %session_id,
                    "media pump: HEVC over DataChannel (rc.77 — FFmpeg via vendor SDK)"
                );
                return media_pump_ffmpeg_dc(
                    FfmpegDcCodec::Hevc,
                    session_id,
                    video_bytes_dc,
                    keyframe_requested,
                    target_resolution,
                    lock_state_rx,
                    quality_state,
                    control_dc.clone(),
                    pc.clone(),
                )
                .await;
            }
            tracing::warn!(
                %session_id,
                "negotiated_transport=data-channel-hevc but ROOMLER_AGENT_USE_FFMPEG isn't set — falling back to WebRTC video track"
            );
        }
        #[cfg(not(feature = "ffmpeg-encoder"))]
        {
            tracing::warn!(
                %session_id,
                "negotiated_transport=data-channel-hevc but agent was built without `ffmpeg-encoder` feature — falling back to WebRTC video track"
            );
        }
    }
    // Y.3 fork: route to the DC pump when the session negotiated VP9
    // 4:4:4 over the `video-bytes` channel. Falls through to the
    // legacy track-based pump otherwise — including when the feature
    // is compiled in but the negotiation didn't pick VP9 (mismatched
    // browser / older controller / operator override).
    if matches!(
        negotiated_transport.as_deref(),
        Some("data-channel-vp9-444")
    ) {
        // rc.83 — Intel HW VP9 via FFmpeg vp9_qsv. When the env var is
        // set AND the operator's host has a working vp9_qsv encoder,
        // route the same `data-channel-vp9-444` transport through the
        // FFmpeg pump (Intel iGPU instead of libvpx SW). Probe before
        // we commit to this path so a missing-driver host transparently
        // falls back to libvpx. Profile constraint: vp9_qsv is 4:2:0-
        // only, so when the operator forced chroma=4:4:4 (via session
        // request OR env var) we keep the libvpx SW path which is the
        // only one that emits VP9 profile 1.
        #[cfg(feature = "ffmpeg-encoder")]
        {
            let wants_444 = matches!(chroma_pref.as_deref(), Some("yuv444"));
            if !wants_444 && crate::encode::ffmpeg::available() {
                // Quick probe at the standard caps probe resolution. If
                // it succeeds the host has a working vp9_qsv path.
                if let Ok(probe) = crate::encode::ffmpeg::FfmpegEncoder::new_vp9(480, 270) {
                    drop(probe);
                    tracing::info!(
                        %session_id,
                        "media pump: VP9 over DataChannel via FFmpeg vp9_qsv (Intel HW; rc.83 Iris Xe fps unlock)"
                    );
                    return media_pump_ffmpeg_dc(
                        FfmpegDcCodec::Vp9,
                        session_id,
                        video_bytes_dc,
                        keyframe_requested,
                        target_resolution,
                        lock_state_rx,
                        quality_state,
                        control_dc.clone(),
                        pc.clone(),
                    )
                    .await;
                }
            }
        }
        #[cfg(feature = "vp9-444")]
        {
            tracing::info!(
                %session_id,
                "media pump: VP9-444 over DataChannel (Phase Y.3 libvpx SW path)"
            );
            return media_pump_vp9_444_dc(
                session_id,
                video_bytes_dc,
                keyframe_requested,
                target_resolution,
                lock_state_rx,
                quality_state,
                chroma_pref,
                pc,
            )
            .await;
        }
        #[cfg(not(feature = "vp9-444"))]
        {
            let _ = chroma_pref;
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
    // rc.87 — control_dc is only consumed by the DC video pumps
    // (HEVC/VP9 FFmpeg paths) for the `rc:video-info` send. The legacy
    // WebRTC-track pump below doesn't use it; silence unused on builds
    // that fall through here (no ffmpeg-encoder feature, or webrtc
    // transport).
    let _ = &control_dc;
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

        // Lock-screen overlay (M3 phase 3, Z-path). When the user-
        // context worker can't see the real desktop (input desktop
        // has transitioned to `winsta0\Winlogon`), the captured
        // frame is black/stale and useless. Substitute a static
        // "Host is locked" overlay at the same dimensions so the
        // operator sees something distinctive instead of frozen
        // black, and the encoder pump keeps the RTP stream healthy.
        // Force a keyframe on the transition into Locked so the
        // browser decoder doesn't need to wait for the next intra-
        // refresh to render the overlay.
        //
        // rc.26 — `sys_ctx_worker` short-circuits the overlay: under
        // SystemContext the capture has already rebound to Winlogon
        // and the real lock-screen pixels are in `frame`. Still pulse
        // a keyframe on each transition so the new captured surface
        // snaps into view.
        let frame = if *lock_state_rx.borrow() == lock_state::LockState::Locked {
            if !was_locked_last_iter {
                keyframe_requested.store(true, std::sync::atomic::Ordering::Relaxed);
                was_locked_last_iter = true;
            }
            if sys_ctx_worker {
                frame
            } else {
                lock_overlay::produce(frame.width, frame.height, frame.monotonic_us, frame.monitor)
            }
        } else {
            if was_locked_last_iter {
                // Force a keyframe on the unlock transition too so
                // the resumed real desktop snaps into view at full
                // quality immediately.
                keyframe_requested.store(true, std::sync::atomic::Ordering::Relaxed);
                was_locked_last_iter = false;
            }
            frame
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
                // hard at 1080p-class. H.264 SW is faster but 1920x1200
                // at 30 fps still eats ~21 ms / frame on an Intel
                // iGPU — close to our 33 ms budget and leaving no
                // headroom for capture jitter. Drop H.264 SW above
                // 720p-class down to a 720p-equivalent where encode
                // is comfortably under 12 ms / frame.
                //
                // rc.38 — preserve source aspect when picking the
                // target. Pre-rc.38 used fixed 1920x1080 / 1280x720
                // targets which stretch a 16:10 source (1920x1200
                // panels, common on ThinkPads + ProBooks) into 16:9,
                // visibly distorting the captured desktop and shifting
                // the apparent positions of UI elements. Browsers also
                // get a track-size jolt on the first→second-frame
                // resolution flip that confuses `<video>.videoWidth`
                // and lands clicks at the wrong OS coordinate.
                // Aspect-preserving target eliminates both.
                fn aspect_preserved_target(
                    src_w: u32,
                    src_h: u32,
                    cap_long_edge: u32,
                ) -> (u32, u32) {
                    if src_w == 0 || src_h == 0 {
                        return (cap_long_edge, cap_long_edge * 9 / 16);
                    }
                    let long = src_w.max(src_h);
                    if long <= cap_long_edge {
                        return (src_w, src_h);
                    }
                    let num = cap_long_edge as u64;
                    let new_w = ((src_w as u64) * num / long as u64) as u32;
                    let new_h = ((src_h as u64) * num / long as u64) as u32;
                    // Encoders require even dims; round DOWN to nearest even.
                    (new_w & !1, new_h & !1)
                }
                let heavy_codec = chosen_codec == "h265" || chosen_codec == "av1";
                let h264 = chosen_codec == "h264";
                let above_1080p =
                    (frame.width as u64) * (frame.height as u64) > (1920u64 * 1080u64);
                let above_720p = (frame.width as u64) * (frame.height as u64) > (1280u64 * 720u64);
                let mut auto_downscale_just_fired = false;
                if backend_is_sw && heavy_codec && above_1080p {
                    let (tw, th) = aspect_preserved_target(frame.width, frame.height, 1920);
                    let mut guard = target_resolution.lock().unwrap();
                    if matches!(*guard, TargetResolution::Native) {
                        *guard = TargetResolution::Fixed {
                            width: tw,
                            height: th,
                        };
                        auto_downscale_just_fired = true;
                        tracing::warn!(
                            %session_id,
                            native_w = frame.width,
                            native_h = frame.height,
                            target_w = tw,
                            target_h = th,
                            codec = %chosen_codec,
                            encoder = enc_ref.name(),
                            "auto-downscale: SW heavy codec on high-res source — capping capture at aspect-preserved ≤1920 long-edge to preserve fps. Operator can override via rc:resolution."
                        );
                    }
                } else if backend_is_sw && h264 && above_720p {
                    let (tw, th) = aspect_preserved_target(frame.width, frame.height, 1280);
                    let mut guard = target_resolution.lock().unwrap();
                    if matches!(*guard, TargetResolution::Native) {
                        *guard = TargetResolution::Fixed {
                            width: tw,
                            height: th,
                        };
                        auto_downscale_just_fired = true;
                        tracing::warn!(
                            %session_id,
                            native_w = frame.width,
                            native_h = frame.height,
                            target_w = tw,
                            target_h = th,
                            codec = %chosen_codec,
                            encoder = enc_ref.name(),
                            "auto-downscale: SW H.264 on high-res source — capping capture at aspect-preserved ≤1280 long-edge so encode stays under the 33 ms 30-fps budget. Operator can override via rc:resolution."
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

                // rc.38 — when auto-downscale changes target_resolution
                // from Native → Fixed, the encoder we just built is at
                // the NATIVE dims and would emit a first frame at those
                // dims. The next loop iteration would then downscale +
                // rebuild the encoder, causing the WebRTC track to see
                // a frame-1 → frame-2 resolution flip. Chrome's
                // `<video>.videoWidth` latches to frame-1 dims and the
                // browser-side input normalisation (letterboxedNormalise)
                // then uses a stale aspect ratio against the actual
                // rendered surface — clicks land at wrong OS pixels
                // (the field-test host field bug 2026-05-17).
                //
                // Fix: drop the native-dim encoder + skip writing this
                // frame to the track. The next iteration will rebuild
                // at the downscaled dims and emit frame-1 there. Costs
                // one captured frame's latency at session start
                // (~30 ms); track never sees a resize.
                if auto_downscale_just_fired {
                    encoder = None;
                    encoder_dims = None;
                    keyframe_requested.store(true, std::sync::atomic::Ordering::Relaxed);
                    tracing::info!(
                        %session_id,
                        "auto-downscale fired on first encoder build — dropping native-dim encoder so the track's first frame is at the downscaled dims (avoids browser videoWidth resize race)"
                    );
                    continue;
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
/// rc.33 additions (RustDesk-parity smoothness sprint):
/// - Resolution + quality-derived bitrate target (was hard-cap 8 Mbps);
///   `rc:quality` from the controller now moves this on the fly.
/// - DC backpressure AIMD: `dc.buffered_amount` over 1 MiB cuts the
///   target by 20% (MD); under 64 KiB for ≥ 5 s adds 10% (AI). Replaces
///   the absent REMB feedback path for the DC-transport.
/// - Optional 60 fps via `ROOMLER_AGENT_VP9_FPS` env var (operator
///   opt-in escape hatch — full warmup-probe / control-DC plumbing
///   deferred to a follow-up).
#[cfg(feature = "vp9-444")]
#[allow(clippy::too_many_arguments)]
async fn media_pump_vp9_444_dc(
    session_id: bson::oid::ObjectId,
    video_bytes_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    keyframe_requested: Arc<std::sync::atomic::AtomicBool>,
    target_resolution: Arc<std::sync::Mutex<TargetResolution>>,
    lock_state_rx: tokio::sync::watch::Receiver<lock_state::LockState>,
    quality_state: Arc<std::sync::atomic::AtomicU8>,
    chroma_pref: Option<String>,
    pc: Arc<RTCPeerConnection>,
) {
    // See `media_pump`: tracks lock-state transitions so we can
    // request a keyframe on the lock/unlock boundary.
    let mut was_locked_last_iter = matches!(*lock_state_rx.borrow(), lock_state::LockState::Locked);
    // rc.26 — same gate as the legacy pump. Under SystemContext the
    // captured frame IS the real Winlogon screen; substituting an
    // overlay over it would hide the password prompt and block remote
    // unlock.
    let sys_ctx_worker = is_system_context_worker();
    if sys_ctx_worker {
        info!(
            %session_id,
            "media_pump_vp9_444_dc: SystemContext worker — lock overlay disabled"
        );
    }
    use crate::encode::VideoEncoder; // brings encode/request_keyframe into scope
    use crate::encode::libvpx::Vp9Encoder;

    // rc.33: opt-in 60 fps via env var. Default 30 (the pre-rc.33
    // behaviour). Operators on hosts that can sustain SW VP9 encode at
    // 4K@60 with cpu-used 6 can flip `ROOMLER_AGENT_VP9_FPS=60` to
    // halve perceptual motion latency. No warmup probe in rc.33 — the
    // env var is operator-acknowledged; a CPU-starved host will see
    // frame drops surface in the heartbeat log (`frames_encoded /
    // frames_captured` ratio < 0.95).
    let target_fps: u32 = vp9_444_target_fps_from_env();
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
    // rc.130 — 60 ms (was 1 s), matching the FFmpeg pump. libvpx is synchronous
    // (g_lag_in_frames=0) so there's no encoder-output queue to drain here, but
    // the faster keepalive still feeds the browser decoder more tightly and
    // pushes the last idle frame through the (now bounded, see the send task
    // below) DC path promptly. Fires only on capture-None.
    const IDLE_KEEPALIVE: Duration = Duration::from_millis(60);
    let start = std::time::Instant::now();

    // rc.166 freeze fix — relay-aware bitrate clamp + tighter backpressure.
    // The WSL / corp path forces all media over a single TURN-TCP relay
    // (ROOMLER_AGENT_ICE_RELAY_TCP=1), which carries only ~1-4 Mbps and is
    // head-of-line-blocked. The 0.20-bpp VP9-444 target (~12 Mbps at
    // 2560×1600) collapses it. Clamp the encoder to relay_max_bps (3 Mbps
    // default) and, per Change D, trip AIMD at a shallower 256 KiB buffered
    // watermark so we shed BEFORE the relay's tiny pipe backs up seconds deep.
    // Adaptive bitrate (A1) — detect THIS session's actual ICE path rather
    // than reading the process-wide env flag. The env flag still wins as an
    // explicit override (see `detect_constrained_transport`).
    let constrained_transport = detect_constrained_transport(&pc, session_id).await;
    let bitrate_cap: u32 = if constrained_transport {
        crate::encode::relay_max_bps()
    } else {
        u32::MAX
    };
    // Change D: trigger AIMD earlier on the shallow relay-TCP pipe.
    let dc_buffered_high: u64 = if constrained_transport {
        256 * 1024
    } else {
        DC_BUFFERED_HIGH_BYTES
    };
    if constrained_transport {
        info!(%session_id, bitrate_cap, dc_buffered_high, "VP9-444 DC pump: constrained (relay-TCP) transport — clamping bitrate + tightening backpressure");
    }

    let mut frames_captured: u64 = 0;
    let mut frames_encoded: u64 = 0;
    // rc.166 freeze fix — these three are now owned by a dedicated DC send
    // task (spawned below, mirroring the FFmpeg pump rc.106 pattern) and
    // shared back as atomics so the heartbeat can still read them. Moving the
    // chunked `dc.send().await` off the pump's hot path stops a big
    // (IDR / high-motion) frame from stalling capture+encode on the send —
    // the 27s screen+input freeze the WSL relay-TCP path hit under motion.
    let frames_sent = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let bytes_written = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let send_errors = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut dc_unopen_drops: u64 = 0;
    let mut frames_skipped_backpressure: u64 = 0;
    let mut scene_change_keyframes: u64 = 0;

    // rc.39 — agent-side scene-change keyframe trigger. Heuristic:
    // after each encode, if the latest delta packet's size exceeds
    // SCENE_CHANGE_SPIKE_RATIO× the recent average AND >=
    // SCENE_CHANGE_MIN_BYTES, we assume a scene-change happened.
    // Force a keyframe on the NEXT frame so the operator sees a clean
    // refresh within 2 frames instead of waiting for the periodic IDR.
    //
    // rc.43 — RETUNED for VBR-mode regression. Field log the field-test host
    // 2026-05-18 (rc.42 + VBR opt-in) showed 33 forced keyframes in
    // 3.5 minutes — one every 6 seconds — because VBR's natural delta
    // size variance (3-10× depending on motion) was tripping the
    // rc.39 ratio=4 + 50 KB thresholds far too often. Each forced
    // keyframe was 200-900 KB; those big keyframe SCTP chunks shared
    // the same DC transport as the cursor DC and stalled cursor:pos
    // updates for 100-200 ms each, producing visibly sluggish mouse.
    // Three tweaks:
    //
    //   (1) rate-limit: at most one forced keyframe per
    //       SCENE_CHANGE_MIN_INTERVAL (1.5 s). Prevents the keyframe
    //       cascade where each forced keyframe inflates the bitrate
    //       envelope and re-triggers the heuristic on the next frame.
    //
    //   (2) MIN_BYTES 50 KB → 150 KB. VBR motion deltas routinely
    //       hit 100 KB even without an actual scene change; require
    //       a stronger signal to act.
    //
    //   (3) SPIKE_RATIO 4× → 8×. Natural VBR variance is ~5×; need
    //       a steeper spike to count.
    //
    // Combined: scene-change still fires reliably on window-uncover
    // (typical ratio >> 8 + size >> 150 KB) but stops mis-firing on
    // pure motion frames.
    //
    // Ring buffer of recent delta-frame sizes (skip the keyframes
    // themselves, which are naturally large).
    let mut recent_delta_sizes: std::collections::VecDeque<usize> =
        std::collections::VecDeque::with_capacity(30);
    const SCENE_CHANGE_SPIKE_RATIO: usize = 8;
    const SCENE_CHANGE_MIN_BYTES: usize = 150_000;
    const SCENE_CHANGE_MIN_INTERVAL: Duration = Duration::from_millis(1500);
    let mut last_scene_change_kf_at: Option<std::time::Instant> = None;

    // rc.45 — dynamic cpu-used boost during motion. the field-test host field
    // test 2026-05-18 (rc.43) confirmed scene-change keyframe cascade
    // is fixed (5× reduction in forced IDRs) but heavy-motion fps is
    // still 8-12 because SW VP9 4:4:4 at 1920×1200 + cpu-used=6 on
    // Iris Xe takes 80-120 ms per motion frame. cpu-used=8 cuts that
    // ~50 %, recovering 15-25 fps during motion. Quality drop is
    // ~20 % per-frame; barely visible during motion-blur anyway.
    //
    // Heuristic: piggyback on the existing scene-change detector. When
    // a scene-change spike fires, BOOST cpu-used from base (env or
    // default 6) to 8 for the next BOOST_DURATION frames. After the
    // boost expires, drop back to base. Sustained-motion windows
    // re-trigger the boost as long as motion continues; static
    // periods restore quality automatically.
    let base_cpu_used = crate::encode::libvpx::cpu_used_from_env();
    const MOTION_BOOST_CPU_USED: std::os::raw::c_int = 8;
    const MOTION_BOOST_DURATION_FRAMES: u64 = 60;
    let mut motion_boost_until_frame: u64 = 0;
    let mut current_cpu_used: std::os::raw::c_int = base_cpu_used;

    // rc.33 — bitrate / quality state. Pre-rc.33 the encoder ran at
    // its `DEFAULT_BITRATE_BPS = 8 Mbps` ceiling regardless of source
    // resolution; at 4K this is ~1/3 of what RustDesk sends and is the
    // dominant cause of blocky motion frames. We now drive
    // `enc.set_bitrate(target)` after each encoder rebuild AND on
    // `rc:quality` change AND on AIMD watermark crossings.
    //
    // Sentinel 0xFF on `last_applied_quality` so the first iteration
    // unconditionally applies the current preference even when the
    // controller hasn't yet pushed `rc:quality`.
    let mut last_applied_quality: u8 = 0xFF;
    // Mirror of the AIMD controller's applied bitrate, for the heartbeat log.
    let mut last_applied_bitrate: u32 = 0;
    // AIMD backpressure controller (rc.171) — substitutes the missing REMB
    // path on the DC transport. It's driven off the SEND-CHANNEL OCCUPANCY at
    // the capacity gate (the real webrtc-rs backpressure signal), NOT
    // `dc.buffered_amount()` — the send task's `dc.send().await` blocks under
    // SCTP flow control, so buffered_amount stays low even while the link is
    // saturated, and the pre-rc.171 AIMD (which ran AFTER the gate's
    // `continue`) never fired under sustained congestion → the bitrate stayed
    // pinned at 12.4 Mbps and the pump collapsed to ~2 fps. Constructed lazily
    // once the first encoder gives us dims → a quality/relay ceiling. See
    // `encode::aimd` for the full signal model + the ×0.8/×1.1 factors that
    // used to live here inline.
    let mut aimd: Option<encode::aimd::AimdController> = None;
    // High watermark for the SECONDARY buffer-overflow decrease trigger
    // (`dc_buffered_high` above resolves it to 256 KiB on a constrained relay,
    // this 1 MiB const otherwise).
    const DC_BUFFERED_HIGH_BYTES: u64 = 1_048_576; // 1 MiB

    // rc.166 freeze fix — dedicated DC send task, ported from the FFmpeg pump
    // (rc.106). The chunked `dc.send().await` is SCTP-flow-controlled; on a
    // multi-MB frame over the relay-TCP path it blocks for tens of ms → whole
    // seconds under the 27s freeze. Doing it inline (pre-rc.166) stalled
    // capture + input. Hand framed frames to this task over a small bounded
    // channel; the pump never blocks on the link (see the `try_send` in the
    // loop). A SINGLE consumer keeps the 16 KiB chunk order intact (the browser
    // reassembler needs it). Depth is intentionally shallow so we stay
    // low-latency — under sustained congestion the pump sheds load rather than
    // building a stale backlog.
    const VP9_SEND_QUEUE_DEPTH: usize = 2; // shallower than FFmpeg's 4 — VP9-444 frames are large; minimise input head-of-line delay
    let send_depth = if constrained_transport {
        VP9_SEND_QUEUE_DEPTH
    } else {
        // Direct/LAN path (localhost under WSL mirrored networking): plenty of
        // bandwidth + sub-ms latency, so a deeper queue absorbs high-motion
        // frame bursts instead of shedding them (the "movement stutter").
        // Input rides a SEPARATE DC, so a deeper video queue adds no input lag.
        8
    };
    let (send_tx, mut send_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(send_depth);
    {
        let video_bytes_dc = video_bytes_dc.clone();
        let frames_sent = frames_sent.clone();
        let bytes_written = bytes_written.clone();
        let send_errors = send_errors.clone();
        let task_session = session_id;
        tokio::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            const SCTP_CHUNK_SIZE: usize = 16 * 1024;
            while let Some(wire) = send_rx.recv().await {
                let Some(dc) = video_bytes_dc.lock().await.clone() else {
                    continue;
                };
                let total = wire.len();
                let mut off = 0usize;
                let mut ok = true;
                while off < total {
                    let end = (off + SCTP_CHUNK_SIZE).min(total);
                    if let Err(e) = dc.send(&wire.slice(off..end)).await {
                        let n = send_errors.fetch_add(1, Relaxed) + 1;
                        tracing::warn!(session = %task_session, %e, send_errors = n, "VP9-444 DC send task: DC send failed");
                        ok = false;
                        break;
                    }
                    off = end;
                }
                if ok {
                    frames_sent.fetch_add(1, Relaxed);
                    bytes_written.fetch_add(total as u64, Relaxed);
                }
            }
            tracing::debug!(session = %task_session, "VP9-444 DC send task exiting (channel closed)");
        });
    }

    loop {
        // rc.166 freeze fix — BACKPRESSURE GATE (ported from FFmpeg pump
        // rc.111). Gate frame PRODUCTION on the send channel having capacity.
        // When the send task can't drain the relay-TCP link fast enough the
        // bounded channel fills; skip BEFORE capture+encode so we don't waste a
        // VP9 encode on a frame we can't send AND — unlike the AIMD-skip below
        // — we do NOT request a keyframe here: skipping before encode leaves the
        // encoder's reference chain intact (the next encoded frame just deltas
        // from the last ENCODED one across the gap), same rationale as the
        // FFmpeg rc.111 comment. Check is_closed() FIRST so a dead send task
        // exits the pump instead of livelocking on a permanently-0 capacity.
        if send_tx.is_closed() {
            warn!(%session_id, "VP9-444 DC pump: send task gone — exiting pump");
            return;
        }
        if send_tx.capacity() == 0 {
            frames_skipped_backpressure += 1;
            // Adaptive bitrate (rc.171) — a FULL send channel is the real DC
            // backpressure signal. Drive the multiplicative decrease HERE,
            // before the `continue`, so it runs DURING sustained congestion
            // (pre-rc.171 the loop bailed at this gate and the AIMD below
            // never ran → bitrate pinned at 12.4 Mbps, the ~2 fps starvation
            // bug). Apply to the existing encoder immediately so the next
            // frame that DOES get through is already smaller.
            if let Some(ctrl) = aimd.as_mut() {
                ctrl.observe(send_depth as u32, true, std::time::Instant::now());
                if let Some(bps) = ctrl.take_pending() {
                    if let Some(enc) = encoder.as_mut() {
                        enc.set_bitrate(bps);
                    }
                    last_applied_bitrate = bps;
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
            continue;
        }

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

        // Lock-screen overlay (M3 phase 3, Z-path). Same logic as
        // the legacy track pump — when the user-context worker
        // can't see the real desktop (input desktop on Winlogon),
        // substitute a static "Host is locked" overlay frame.
        // rc.26 — short-circuit when sys_ctx_worker; see media_pump.
        let frame = if *lock_state_rx.borrow() == lock_state::LockState::Locked {
            if !was_locked_last_iter {
                keyframe_requested.store(true, std::sync::atomic::Ordering::Relaxed);
                was_locked_last_iter = true;
            }
            if sys_ctx_worker {
                frame
            } else {
                lock_overlay::produce(frame.width, frame.height, frame.monotonic_us, frame.monitor)
            }
        } else {
            if was_locked_last_iter {
                keyframe_requested.store(true, std::sync::atomic::Ordering::Relaxed);
                was_locked_last_iter = false;
            }
            frame
        };

        let w = frame.width & !1;
        let h = frame.height & !1;
        if w != frame.width || h != frame.height {
            // Drop this frame; the next one will arrive at-or-near the
            // same dims and we'll handle the rebuild then. Safer than
            // shrinking the buffer in-place and risking off-by-one.
            continue;
        }

        if encoder_dims != Some((w, h)) {
            // rc.61 — resolve chroma format. Priority order:
            //   1. Per-session `chroma_pref` from `rc:session.request`
            //      (rc.62 — controller's UI choice).
            //   2. `ROOMLER_AGENT_VP9_CHROMA` env var (rc.61, operator
            //      default at the host).
            //   3. Yuv444 (pre-rc.61 default, sharpest text).
            // Read at every rebuild so a mid-session env-var flip
            // (operator changes it via the SCM service env block +
            // restart) takes effect on the next dim change without
            // needing a separate hook.
            let chroma = chroma_pref
                .as_deref()
                .and_then(|s| match s.trim().to_ascii_lowercase().as_str() {
                    "yuv420" | "420" => Some(crate::encode::libvpx::Vp9Chroma::Yuv420),
                    "yuv444" | "444" => Some(crate::encode::libvpx::Vp9Chroma::Yuv444),
                    _ => None,
                })
                .unwrap_or_else(crate::encode::libvpx::vp9_chroma_from_env);
            info!(%session_id, w, h, target_fps, chroma = chroma.as_str(), chroma_source = if chroma_pref.is_some() { "session_request" } else { "env_var" }, "VP9-444 encoder rebuild for dims");
            match Vp9Encoder::new_with_fps_chroma(w, h, target_fps, chroma) {
                Ok(e) => {
                    encoder = Some(e);
                    encoder_dims = Some((w, h));
                    // rc.33 — force quality re-apply on the new
                    // encoder. set_bitrate state lives on the
                    // encoder instance, so a rebuild reverts to the
                    // boot-time default bitrate until we push the
                    // resolution-derived quality target through.
                    last_applied_quality = 0xFF;
                    last_applied_bitrate = 0;
                    // rc.45 — encoder rebuild starts at base cpu-used
                    // (apply_screen_content_controls reads the env
                    // var). Reset our tracking so the next motion
                    // boost properly logs the from-value.
                    current_cpu_used = base_cpu_used;
                    motion_boost_until_frame = 0;
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

        // rc.33/rc.171 — resolution + quality-derived bitrate CEILING, fed to
        // the AIMD controller. The controller (not this block) owns the actual
        // applied bitrate: it starts at the ceiling and tracks the link down
        // under congestion / back up on recovery. The ceiling still lifts 4K
        // Quality=High to ~25-30 Mbps (the largest motion-smoothness lever)
        // and clamps to the relay cap on a constrained transport.
        let q_now = quality_state.load(std::sync::atomic::Ordering::Relaxed);
        if let Some((ew, eh)) = encoder_dims {
            let base = encode::initial_bitrate_for_fps(ew, eh, target_fps);
            let ceiling = quality::target_bitrate(q_now, base).min(bitrate_cap);
            let now = std::time::Instant::now();
            let ctrl = aimd.get_or_insert_with(|| {
                encode::aimd::AimdController::new(
                    ceiling,
                    encode::MIN_BITRATE_BPS,
                    ceiling,
                    send_depth as u32,
                    now,
                )
            });
            ctrl.set_ceiling(ceiling);
            // Non-full occupancy sample so the additive-increase can recover
            // once the link has drained (the FULL samples come from the gate).
            let cap = send_tx.capacity();
            ctrl.observe(send_depth.saturating_sub(cap) as u32, cap == 0, now);
            if let Some(target) = ctrl.take_pending() {
                enc.set_bitrate(target);
                if q_now != last_applied_quality || target != last_applied_bitrate {
                    info!(
                        %session_id,
                        quality = quality::label(q_now),
                        base_bps = base,
                        ceiling_bps = ceiling,
                        target_bps = target,
                        "VP9-444 set_bitrate (AIMD)"
                    );
                }
                last_applied_bitrate = target;
            }
            last_applied_quality = q_now;
        }

        let packets = match enc.encode(frame).await {
            Ok(p) => p,
            Err(e) => {
                warn!(%session_id, %e, "VP9-444 encode error — pump exits");
                return;
            }
        };
        frames_encoded += packets.len() as u64;

        // rc.39 — scene-change detection. Inspect each delta packet's
        // size against the rolling average; on a sufficient spike,
        // arm keyframe_requested so the *next* encode emits an IDR.
        // Recovery becomes 2 frames (1 oversized delta + 1 sharp IDR)
        // instead of waiting up to kf_max_dist frames.
        let mut should_force_kf = false;
        for pkt in &packets {
            if pkt.is_keyframe {
                // A keyframe just landed (likely the periodic IDR or
                // a previous scene-change trigger). Reset the rolling
                // window — keyframe sizes would skew the average for
                // many seconds.
                recent_delta_sizes.clear();
                continue;
            }
            let size = pkt.data.len();
            if !recent_delta_sizes.is_empty() {
                let sum: usize = recent_delta_sizes.iter().sum();
                let avg = sum / recent_delta_sizes.len();
                if avg > 0
                    && size >= SCENE_CHANGE_MIN_BYTES
                    && size > avg * SCENE_CHANGE_SPIKE_RATIO
                {
                    should_force_kf = true;
                    tracing::info!(
                        %session_id,
                        size,
                        avg,
                        ratio = size as f32 / avg as f32,
                        "VP9-444 scene-change detected (delta-size spike) — forcing keyframe next frame"
                    );
                }
            }
            recent_delta_sizes.push_back(size);
            if recent_delta_sizes.len() > 30 {
                recent_delta_sizes.pop_front();
            }
        }
        if should_force_kf {
            // rc.43 — rate-limit. The recovery target is ~2 frames after
            // a real scene change; a 1.5 s cooldown is well past any
            // realistic single uncover-event recovery while preventing
            // the cascade where each forced keyframe inflates the
            // bitrate envelope and re-triggers the heuristic on the
            // next encode pass.
            let now = std::time::Instant::now();
            let within_cooldown = last_scene_change_kf_at
                .map(|t| now.duration_since(t) < SCENE_CHANGE_MIN_INTERVAL)
                .unwrap_or(false);
            if within_cooldown {
                tracing::debug!(
                    %session_id,
                    "VP9-444 scene-change candidate suppressed (rate-limit cooldown active)"
                );
            } else {
                keyframe_requested.store(true, std::sync::atomic::Ordering::Relaxed);
                scene_change_keyframes += 1;
                last_scene_change_kf_at = Some(now);

                // rc.45 — piggyback motion-cpu-used boost on every
                // scene-change firing. The same signal that flags
                // "this frame needed a lot of new bits" also flags
                // "we're in a motion window where fps matters more
                // than per-frame quality". Boost stays armed for
                // MOTION_BOOST_DURATION_FRAMES; sustained motion
                // re-arms it; static periods let it expire and
                // restore base quality.
                if current_cpu_used != MOTION_BOOST_CPU_USED {
                    enc.set_speed(MOTION_BOOST_CPU_USED);
                    tracing::info!(
                        %session_id,
                        from = current_cpu_used,
                        to = MOTION_BOOST_CPU_USED,
                        "VP9-444 motion boost engaged — cpu-used raised (faster encode, lower per-frame quality)"
                    );
                    current_cpu_used = MOTION_BOOST_CPU_USED;
                }
                motion_boost_until_frame = frames_encoded + MOTION_BOOST_DURATION_FRAMES;
            }
        }

        // rc.45 — decay the motion boost when the duration elapses.
        // After MOTION_BOOST_DURATION_FRAMES frames without a new
        // scene-change refresh, drop cpu-used back to the base value
        // so static text snaps back to sharp encoding.
        if motion_boost_until_frame > 0
            && frames_encoded >= motion_boost_until_frame
            && current_cpu_used != base_cpu_used
        {
            enc.set_speed(base_cpu_used);
            tracing::info!(
                %session_id,
                from = current_cpu_used,
                to = base_cpu_used,
                "VP9-444 motion boost expired — cpu-used restored to base"
            );
            current_cpu_used = base_cpu_used;
            motion_boost_until_frame = 0;
        }

        // Pull the DC handle once per frame. `try_lock` would race
        // with the on_data_channel callback that stashes it; the
        // contention here is microseconds.
        let dc_opt = video_bytes_dc.lock().await.clone();
        let Some(dc) = dc_opt else {
            // DC not yet open — drop frames until the controller
            // opens it. Common during the first ~100 ms of a session
            // (offer/answer + ICE + SCTP handshake). Counted so a
            // controller that never opens the DC is greppable.
            //
            // CRITICAL: also re-request a keyframe so the FIRST frame
            // the browser worker actually receives (whenever the DC
            // finally opens) is a keyframe. Without this, the encoder
            // proceeds along its 240-frame keyframe interval, so the
            // first delivered frame is a delta and the browser's
            // VideoDecoder rejects it with
            // "A key frame is required after configure() or flush()"
            // — every subsequent delta also fails since the decoder
            // never advanced past the configured-but-unfed state.
            dc_unopen_drops += packets.len() as u64;
            keyframe_requested.store(true, std::sync::atomic::Ordering::Relaxed);
            continue;
        };
        if dc.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {
            dc_unopen_drops += packets.len() as u64;
            keyframe_requested.store(true, std::sync::atomic::Ordering::Relaxed);
            continue;
        }

        // Secondary congestion signal (rc.171) — the PRIMARY AIMD driver is
        // send-channel occupancy (see the capacity gate + the ceiling/observe
        // block above). But if the SCTP buffer DOES spike over the high
        // watermark, note it to the controller (a rate-limited decrease) and
        // shed this frame so we don't pile more bytes on an already-backed-up
        // queue. On webrtc-rs this rarely fires (dc.send().await blocks first,
        // keeping buffered_amount low), but it's a cheap belt-and-suspenders
        // check that also preserves the shed-on-overflow behaviour.
        let buffered = dc.buffered_amount().await as u64;
        if buffered > dc_buffered_high {
            if let Some(ctrl) = aimd.as_mut() {
                ctrl.note_buffer_overflow(std::time::Instant::now());
                if let Some(bps) = ctrl.take_pending() {
                    enc.set_bitrate(bps);
                    info!(
                        %session_id,
                        buffered,
                        new_target = bps,
                        "VP9-444 AIMD decrease (DC buffer over high watermark)"
                    );
                    last_applied_bitrate = bps;
                }
            }
            // Skip this frame entirely + ask the controller for a keyframe on
            // resume so the decoder doesn't choke on a delta-after-gap.
            frames_skipped_backpressure += packets.len() as u64;
            keyframe_requested.store(true, std::sync::atomic::Ordering::Relaxed);
            continue;
        }

        // rc.166 freeze fix — hand each framed packet to the dedicated send
        // task (see above the loop) rather than chunk-sending inline. The send
        // task owns the 16 KiB SCTP chunking + the flow-controlled
        // `dc.send().await`; `try_send` NEVER blocks the capture/encode loop.
        // If the send task is behind (the relay-TCP link can't drain a big
        // motion/IDR frame fast enough) the bounded channel fills and we shed
        // THIS frame + request a keyframe so the browser resyncs cleanly when
        // the queue drains. A single consumer preserves 16 KiB chunk order for
        // the browser reassembler. (frames_sent / bytes_written / send_errors
        // are incremented by the send task via the shared atomics now.)
        for p in packets {
            let ts_us = start.elapsed().as_micros() as u64;
            let wire = bytes::Bytes::from(frame_video_bytes(&p.data, p.is_keyframe, ts_us));
            match send_tx.try_send(wire) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    frames_skipped_backpressure += 1;
                    keyframe_requested.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    warn!(%session_id, "VP9-444 DC pump: send task gone — exiting pump");
                    return;
                }
            }
        }

        if frames_encoded.is_multiple_of(30) {
            // rc.36 — surface target_fps so field operators can verify
            // ROOMLER_AGENT_VP9_FPS env-var was honored. If target_fps
            // shows 30 when the operator set 60, the env var didn't
            // reach the agent process (wrong service-block scope, or
            // process wasn't restarted to inherit the new block).
            // rc.166 freeze fix — the send-owned counters are snapshotted from
            // the atomics for the log line.
            let frames_sent = frames_sent.load(std::sync::atomic::Ordering::Relaxed);
            let bytes_written = bytes_written.load(std::sync::atomic::Ordering::Relaxed);
            let send_errors = send_errors.load(std::sync::atomic::Ordering::Relaxed);
            info!(
                %session_id,
                target_fps,
                cpu_used = current_cpu_used,
                frames_captured, frames_encoded, frames_sent, bytes_written,
                send_errors, dc_unopen_drops,
                frames_skipped_backpressure,
                scene_change_keyframes,
                target_bps = last_applied_bitrate,
                "VP9-444 DC pump heartbeat (≈1s window)"
            );
        }
    }
}

/// rc.83 — Codec selector for the unified FFmpeg DC pump. Lets one
/// pump function serve both HEVC (over `data-channel-hevc`) and VP9
/// (over `data-channel-vp9-444` when FFmpeg vp9_qsv is preferred over
/// libvpx SW) without duplicating the capture → encode → frame →
/// send loop.
#[cfg(feature = "ffmpeg-encoder")]
#[derive(Debug, Clone, Copy)]
enum FfmpegDcCodec {
    Hevc,
    Vp9,
}

#[cfg(feature = "ffmpeg-encoder")]
impl FfmpegDcCodec {
    /// Phase B — `fps` + `maxrate_bps` are the pump's per-session values
    /// (real `target_fps`, relay-aware ceiling), threaded into the encoder so
    /// its framerate + burst cap match the actual link instead of a fixed 30.
    fn open(
        self,
        w: u32,
        h: u32,
        fps: u32,
        maxrate_bps: usize,
    ) -> anyhow::Result<crate::encode::ffmpeg::FfmpegEncoder> {
        use crate::encode::ffmpeg::FfmpegEncoder;
        match self {
            Self::Hevc => FfmpegEncoder::new_hevc_adaptive(w, h, fps, maxrate_bps),
            Self::Vp9 => FfmpegEncoder::new_vp9_adaptive(w, h, fps, maxrate_bps),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Hevc => "HEVC",
            Self::Vp9 => "VP9",
        }
    }

    /// Wire codec name for the `rc:video-info` message — matches the
    /// `AgentCaps.codecs` / negotiation vocabulary the browser uses.
    fn wire_codec(self) -> &'static str {
        match self {
            Self::Hevc => "h265",
            Self::Vp9 => "vp9",
        }
    }

    /// Chroma the FFmpeg path emits. Both `hevc_*` (Main profile) and
    /// `vp9_qsv` (profile 0) are 4:2:0 8-bit — the 4:4:4 path stays on
    /// libvpx SW (`media_pump_vp9_444_dc`), never this pump.
    fn wire_chroma(self) -> &'static str {
        "yuv420"
    }
}

/// rc.77/rc.83 — Unified FFmpeg-encoder DataChannel pump.
///
/// Mirrors `media_pump_vp9_444_dc` structurally but uses
/// `FfmpegEncoder` (which dispatches to vendor SDKs) and emits raw
/// codec bytes length-prefixed over the `video-bytes` DC. Shares the
/// same 13-byte header as the VP9 path so `frame_video_bytes` is
/// reused verbatim.
///
/// `codec` chooses which encoder dispatch the FfmpegEncoder uses:
/// - `Hevc` → `hevc_nvenc` / `hevc_qsv` / `hevc_amf` (rc.77 path)
/// - `Vp9` → `vp9_qsv` (rc.83 path — Intel-only HW VP9, unblocks
///   the Iris Xe CPU-bound 17 fps → 60 fps target)
///
/// Capture → encode → frame → send, with (Phase B) a per-session AIMD
/// backpressure controller mirroring `media_pump_vp9_444_dc`: it detects THIS
/// session's ICE path (relay vs direct), picks the target fps + a relay-aware
/// maxrate ceiling accordingly, and drives `FfmpegEncoder::set_bitrate` off
/// send-channel occupancy so the HEVC/vp9_qsv burst cap tracks the actual link.
/// (No scene-change detection / ROI hints yet — those remain future work.)
#[cfg(feature = "ffmpeg-encoder")]
#[allow(clippy::too_many_arguments)]
async fn media_pump_ffmpeg_dc(
    codec: FfmpegDcCodec,
    session_id: bson::oid::ObjectId,
    video_bytes_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    keyframe_requested: Arc<std::sync::atomic::AtomicBool>,
    target_resolution: Arc<std::sync::Mutex<TargetResolution>>,
    lock_state_rx: tokio::sync::watch::Receiver<lock_state::LockState>,
    _quality_state: Arc<std::sync::atomic::AtomicU8>,
    control_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    // Phase B — the peer connection, so the pump can detect THIS session's
    // actual ICE path (relay vs direct) at runtime for the per-session
    // bitrate/fps clamp instead of the process-wide env flag.
    pc: Arc<RTCPeerConnection>,
) {
    use crate::encode::VideoEncoder;
    use crate::encode::ffmpeg::FfmpegEncoder;

    let codec_label = codec.label();
    // rc.87 — emit `rc:video-info` once the encoder is built so the
    // browser stats badge shows the TRUTH (real encoder + HW + chroma)
    // instead of the hardcoded "VP9 4:4:4 SW". Sent once on first build.
    let mut video_info_sent = false;

    // Mirror the VP9 pump's overlay gate. Under SystemContext the
    // captured frame IS the real Winlogon screen; an overlay over it
    // would hide the password prompt and block remote unlock.
    let sys_ctx_worker = is_system_context_worker();
    if sys_ctx_worker {
        info!(
            %session_id,
            codec_label,
            "media_pump_ffmpeg_dc: SystemContext worker — lock overlay disabled"
        );
    }
    let mut was_locked_last_iter = matches!(*lock_state_rx.borrow(), lock_state::LockState::Locked);

    // Phase B — detect THIS session's ICE path (relay vs direct) up front so
    // the target fps + maxrate ceiling + send-queue depth all match the actual
    // link, and the AIMD clamps per session rather than off the process-wide
    // env flag. The env flag still wins as an explicit override (see
    // `detect_constrained_transport`). ICE may not have nominated yet, so this
    // briefly polls; the AIMD converges regardless of the initial guess.
    let constrained = detect_constrained_transport(&pc, session_id).await;

    // rc.93 — target fps. Pre-rc.93 this was hardcoded 30, which capped the
    // scrap backend's internal pacer (`scrap_backend::next_frame` sleeps to
    // `1000/target_fps` ms) AND drove a redundant pump-side floor sleep.
    // The fast legacy `media_pump` runs the SAME capturer at 60 with no
    // pump floor and hits ~55 fps; HW encode (vp9_qsv/hevc_qsv ~4-6 ms)
    // easily sustains that. Phase B: default 60 on a direct link, 30 on a
    // constrained relay (which can't sustain 60 fps of HEVC without shedding);
    // an explicit env override wins either way.
    let target_fps: u32 = ffmpeg_target_fps(constrained);
    let downscale = crate::capture::DownscalePolicy::Never;
    info!(
        %session_id,
        codec_label,
        target_fps,
        "FFmpeg DC pump starting"
    );
    let mut capturer = capture::open_default(target_fps, downscale);
    let mut encoder: Option<FfmpegEncoder> = None;
    let mut encoder_dims: Option<(u32, u32)> = None;
    // rc.93 — single keepalive clock, mirroring `media_pump`. The rc.92
    // pacing clock + pump-side floor sleep were REMOVED: the capture
    // backend is the single pacer (scrap sleeps to `1000/target_fps`;
    // SystemContext capture delivers at display rate), so a second
    // pump-side floor just halved fps and amplified idle Nones — that was
    // the real vp9_qsv ~15 fps bug (rc.92's timer theory was a red herring;
    // timeBeginPeriod(1) landed but didn't move fps).
    let mut last_capture_at = std::time::Instant::now();
    let mut last_good_frame: Option<std::sync::Arc<crate::capture::Frame>> = None;
    // rc.130 — 60 ms (was 1 s). Doubles as the SPARSE-INPUT DRAIN. With the
    // HW encoder's output queue capped to ~1 frame (encoder.rs delay=0 /
    // async_depth=1), re-feeding the last good frame here flushes the held
    // frame within ~60 ms of the screen going idle — so the LAST keystroke's
    // pixels reach the browser promptly instead of waiting up to a full
    // second (the old keepalive value) for the next caret blink to push them
    // out. Fires only on capture-None (no new frame) and, via the rc.111
    // capacity gate above, only when the send channel has room — so it adds
    // ZERO frames under motion (real frames keep resetting last_capture_at).
    // Idle cost: ~16 fps of near-zero-byte static deltas.
    const IDLE_KEEPALIVE: Duration = Duration::from_millis(60);

    let mut frames_captured: u64 = 0;
    let mut frames_encoded: u64 = 0;
    // rc.106 — frames_sent / bytes_written / send_errors are owned by the
    // dedicated send task (spawned below) and shared back as atomics so the
    // heartbeat can still read them. Moving the chunked DC send off the
    // pump's hot path stops a big (IDR / motion) frame from stalling
    // capture+encode on `send().await` — the "hangs every few seconds"
    // under window movement (field GORAN-XMG-NEO16: 46 fps with periodic
    // freezes; the inline send blocked the loop ~tens of ms per multi-MB
    // frame).
    let frames_sent = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let bytes_written = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let send_errors = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    // rc.106 — set after a backpressure drop so the first frame that
    // successfully re-enqueues is forced to a keyframe (clean browser resync).
    let mut resync_pending = false;
    let mut dc_unopen_drops: u64 = 0;
    // rc.88 — frames dropped because the DC send buffer was over the
    // high-water mark (congested link). Shedding a delta frame keeps the
    // capture/encode loop at cadence instead of stalling on `send().await`
    // — the likely cause of the field's "13 fps under motion".
    let mut frames_dropped_backpressure: u64 = 0;
    // rc.111 — frames skipped at the SOURCE (before capture+encode) because
    // the send channel was full. Distinct from frames_dropped_backpressure
    // (which counts frames encoded THEN dropped at try_send). Skipping before
    // encode is the cheaper, smoother response: no wasted GPU encode and no
    // resync-keyframe churn (the HEVC delta chain stays intact). See the gate
    // at the top of the loop.
    let mut frames_skipped_backpressure: u64 = 0;
    // rc.88 — per-stage timing accumulators (µs since last heartbeat) so
    // the field log localises the bottleneck: capture vs encode vs send.
    let mut capture_us: u64 = 0;
    let mut encode_us: u64 = 0;
    let mut send_us: u64 = 0;
    // rc.93 — count Ok(None) ticks (capturer had no new frame). Replaces
    // the rc.92 floor-sleep accumulator now that the pump floor is gone. A
    // high frames_empty *under motion* would mean the capture backend (not
    // the pump) is the fps limiter; near-zero under motion confirms the
    // pump now runs at capture rate like `media_pump`.
    let mut frames_empty: u64 = 0;
    // rc.98 — one-shot confirmation that the encoder actually emits a
    // key-FLAGGED packet (pkt.is_keyframe). On NVENC this only happens with
    // `forced-idr=1`; if this log never fires while the browser reports "A
    // key frame is required", the encoder isn't flagging IDRs.
    let mut first_keyframe_logged = false;
    let mut heartbeat_frames_base: u64 = 0;
    let mut last_heartbeat = std::time::Instant::now();
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

    // rc.106 — dedicated DC send task. The chunked `dc.send().await` is
    // flow-controlled by SCTP; on a multi-MB frame (HEVC IDR / high-motion
    // delta) it blocks for tens of ms. Doing that inline in the pump (rc.88)
    // stalled capture+encode → the periodic freeze (field GORAN-XMG-NEO16).
    // Hand framed frames to this task over a small bounded channel instead;
    // the pump never blocks on the link (see the `try_send` below). A SINGLE
    // consumer keeps the 16 KiB chunk order intact (the browser reassembler
    // needs it). Depth is small so we stay low-latency — under sustained
    // congestion the pump sheds load (drops + schedules a resync keyframe)
    // rather than building a stale backlog.
    // Deeper queue on the direct/LAN path (localhost under WSL mirrored
    // networking) so high-motion HEVC bursts (big IDR/motion frames) get
    // BUFFERED instead of shed (the "movement stutter"); a constrained relay-TCP
    // path stays shallow to shed fast rather than build a stale backlog. Input
    // rides a SEPARATE DC, so a deeper video queue adds no input lag.
    let ffmpeg_send_depth = if constrained { 4 } else { 12 };
    let (send_tx, mut send_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(ffmpeg_send_depth);

    // Phase B — AIMD backpressure controller (mirrors media_pump_vp9_444_dc).
    // Substitutes the missing REMB signal on the DC transport: driven off
    // SEND-CHANNEL OCCUPANCY at the capacity gate (the real webrtc-rs
    // backpressure signal — the send task's `dc.send().await` blocks under SCTP
    // flow control, so `buffered_amount()` stays low even while saturated).
    // Constructed lazily once the first encoder gives us dims → a per-resolution,
    // relay-aware maxrate ceiling. The controller emits a continuous target;
    // `FfmpegEncoder::set_bitrate` coarsens it to a ladder and applies it as an
    // in-place NVENC reconfigure or a debounced QSV/AMF rebuild.
    let mut aimd: Option<encode::aimd::AimdController> = None;
    // Mirror of the AIMD's applied bitrate, for the heartbeat + change-gated log.
    let mut last_applied_bitrate: u32 = 0;
    {
        let video_bytes_dc = video_bytes_dc.clone();
        let frames_sent = frames_sent.clone();
        let bytes_written = bytes_written.clone();
        let send_errors = send_errors.clone();
        let task_session = session_id;
        tokio::spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            const SCTP_CHUNK_SIZE: usize = 16 * 1024;
            while let Some(wire) = send_rx.recv().await {
                // Fetch the DC fresh each frame — the same handle the pump's
                // open-check uses. None means it closed under us (the pump's
                // open-guard re-requests a keyframe on its side).
                let Some(dc) = video_bytes_dc.lock().await.clone() else {
                    continue;
                };
                let total = wire.len();
                let mut off = 0usize;
                let mut ok = true;
                while off < total {
                    let end = (off + SCTP_CHUNK_SIZE).min(total);
                    // `wire.slice` is zero-copy (shares the Bytes buffer).
                    if let Err(e) = dc.send(&wire.slice(off..end)).await {
                        let n = send_errors.fetch_add(1, Relaxed) + 1;
                        tracing::warn!(
                            session = %task_session, %e, send_errors = n,
                            "FFmpeg DC pump send task: DC send failed"
                        );
                        ok = false;
                        break;
                    }
                    off = end;
                }
                if ok {
                    frames_sent.fetch_add(1, Relaxed);
                    bytes_written.fetch_add(total as u64, Relaxed);
                }
            }
            tracing::debug!(session = %task_session, "FFmpeg DC pump send task exiting (channel closed)");
        });
    }

    loop {
        // rc.93 — NO pump-side pacing floor (the rc.86→rc.92 floor sleep
        // was the fps bug). The capture backend is the single pacer:
        // scrap_backend sleeps to `1000/target_fps` internally, and the
        // SystemContext worker delivers at display rate. A second floor
        // here just halved the achieved fps and amplified idle Nones. Poll
        // continuously, exactly like the fast `media_pump`.

        // rc.111 — BACKPRESSURE GATE. Gate frame PRODUCTION on the send
        // channel having capacity. When the dedicated send task can't drain
        // the link fast enough (bandwidth-limited / relayed path), the bounded
        // channel (depth FFMPEG_SEND_QUEUE_DEPTH) fills. Pre-rc.111 the pump
        // kept capturing + encoding at full rate and DROPPED the encoded frame
        // at `try_send` (frames_dropped_backpressure) + scheduled a resync
        // keyframe — wasting GPU encode and, worse, the resync IDRs (the
        // LARGEST frames) piled MORE bytes onto an already-congested link,
        // amplifying the stall. Field GORAN-XMG-NEO16 (RTX 5090, 2560×1600):
        // capture 6 ms + encode 8 ms (fast) but ~37% of encoded frames dropped
        // + resync churn → stutter.
        //
        // Skipping at the source instead: don't capture/encode a frame we
        // can't send. Production auto-paces to the drain rate, the HEVC delta
        // chain stays continuous (no resync keyframe needed — the next encoded
        // frame just deltas from the last ENCODED one across the gap), and the
        // GPU is freed. Single-producer, so capacity()>0 here guarantees the
        // post-encode try_send below won't block; that try_send stays as a
        // safety net for the rare multi-packet frame that overflows mid-send.
        // The 2 ms yield matches the empty-poll pace (precise at the rc.92 1 ms
        // timer resolution) and only fires under genuine congestion.
        //
        // Check is_closed() FIRST: the send task only dies if its receiver is
        // dropped (or it panics). Were that to happen, capacity() stays 0 and
        // the skip-loop would livelock without ever reaching the try_send that
        // detects Closed — so exit the pump here instead, mirroring the
        // try_send Closed arm below.
        if send_tx.is_closed() {
            tracing::warn!(
                %session_id, codec_label,
                "FFmpeg DC pump: send task gone (channel closed) — exiting pump"
            );
            return;
        }
        if send_tx.capacity() == 0 {
            frames_skipped_backpressure += 1;
            // Phase B — a FULL send channel is the real DC backpressure signal.
            // Drive the multiplicative decrease HERE, before the `continue`, so
            // it runs DURING sustained congestion (the VP9 pump's rc.171
            // starvation-fix rationale) instead of never firing. Apply to the
            // live encoder so the next frame that gets through is already smaller.
            if let Some(ctrl) = aimd.as_mut() {
                ctrl.observe(ffmpeg_send_depth as u32, true, std::time::Instant::now());
                if let Some(bps) = ctrl.take_pending() {
                    if let Some(enc) = encoder.as_mut() {
                        enc.set_bitrate(bps);
                    }
                    last_applied_bitrate = bps;
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
            continue;
        }

        // Capture one frame; on transient failure, reuse the last
        // good one as a keepalive so the browser decoder doesn't
        // pause. ScreenCapture's method is `next_frame() -> Result<
        // Option<Frame>>`, not Iterator::next — matches the pattern
        // used by `media_pump_vp9_444_dc` at line ~1741.
        let capture_start = std::time::Instant::now();
        let next = capturer.next_frame().await;
        capture_us += capture_start.elapsed().as_micros() as u64;
        let frame: std::sync::Arc<crate::capture::Frame> = match next {
            Ok(Some(f)) => {
                let arc = std::sync::Arc::new(f);
                last_good_frame = Some(arc.clone());
                last_capture_at = std::time::Instant::now();
                frames_captured += 1;
                arc
            }
            Ok(None) => {
                // No new frame this tick (DXGI only fires on screen change).
                // Once idle ≥ IDLE_KEEPALIVE, re-encode the last good frame so
                // the browser decoder doesn't pause.
                frames_empty += 1;
                if last_capture_at.elapsed() >= IDLE_KEEPALIVE
                    && let Some(ref f) = last_good_frame
                {
                    last_capture_at = std::time::Instant::now();
                    f.clone()
                } else {
                    // rc.99 — pace empty polls with a short sleep before
                    // retrying. rc.93 removed the top-of-loop floor (correctly
                    // — it capped the Some-rate) AND made this `continue`
                    // immediately, on the assumption the capture backend self-
                    // paces. That's TRUE for scrap (internal target_frame_period
                    // sleep) but FALSE for the SystemContext worker, which has
                    // NO pacer: the pump then spins MILLIONS of empty oneshot
                    // round-trips/session (frames_empty ≫ frames_encoded),
                    // saturating the runtime so the real-frame round-trip
                    // latency spikes intermittently → fps swings (field
                    // GORAN-XMG-NEO16 2560×1600 SystemContext: cap 7↔117ms,
                    // fps 9↔67, stuttery). A 2 ms sleep paces empties to
                    // ~500/s (vs millions) — precise at 1 ms timer resolution
                    // (win_timer rc.92) — WITHOUT capping the Some-rate (this
                    // only fires when there's no new frame), so it does NOT
                    // regress the rc.93 fps win. ~2 ms adds negligible
                    // frame-catch latency vs a 60 Hz (16 ms) source.
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    continue;
                }
            }
            Err(e) => {
                tracing::warn!(%session_id, codec_label, %e, "FFmpeg DC pump: capture error");
                continue;
            }
        };

        // rc.96 — apply the controller-chosen resolution (`rc:resolution`),
        // bringing the FFmpeg pump to parity with `media_pump_vp9_444_dc`
        // (which already does this). `Native` is a passthrough; `Fixed`
        // downscales (CPU box filter) before encode, so the encoder rebuilds
        // for the smaller dims via the dim-change check below. This shrinks
        // the encode + the wire bytes + the browser-side decode load.
        //
        // NOTE: this runs AFTER the full-resolution capture, so on a host
        // whose bottleneck is the capture/duplication rate (e.g. a 4K panel
        // on a weak iGPU — frames_empty ≫ frames_encoded) it does NOT raise
        // fps; it's a bandwidth/quality lever, not a capture-fps fix. Hosts
        // that are genuinely encode-bound do gain fps from the smaller encode.
        let frame = apply_target_resolution(frame, *target_resolution.lock().unwrap());

        // Lazily build / rebuild the encoder when the frame dims change.
        let (w, h) = (frame.width, frame.height);
        // Phase B — per-resolution, per-session (relay-aware) maxrate ceiling.
        // Recomputed each frame (cheap) so a dim change or a mid-session env
        // tweak re-seeds it; the AIMD starts at this ceiling and tracks the
        // link down from it. Also the initial maxrate the encoder is built with.
        let ceiling =
            crate::encode::ffmpeg::encoder::ffmpeg_maxrate_bps(w, h, target_fps, constrained)
                as u32;
        let need_rebuild = match encoder_dims {
            Some((ew, eh)) => ew != w || eh != h,
            None => true,
        };
        if need_rebuild {
            match codec.open(w, h, target_fps, ceiling as usize) {
                Ok(enc) => {
                    let encoder_name = enc.name();
                    info!(
                        %session_id,
                        codec_label,
                        width = w,
                        height = h,
                        encoder = encoder_name,
                        "FFmpeg DC pump: encoder (re)built"
                    );
                    // rc.87 — tell the browser the real encoder so the
                    // stats badge stops claiming "VP9 4:4:4 SW". Sent
                    // once on first successful build. FFmpeg HEVC/VP9
                    // paths are always HW (NVENC/QSV/AMF) and 4:2:0.
                    if !video_info_sent {
                        let payload = format!(
                            r#"{{"t":"rc:video-info","codec":"{}","encoder":"{}","hardware":true,"chroma":"{}"}}"#,
                            codec.wire_codec(),
                            encoder_name,
                            codec.wire_chroma(),
                        );
                        let cdc = control_dc.lock().await.clone();
                        if let Some(cdc) = cdc {
                            if let Err(e) = cdc.send_text(payload).await {
                                debug!(%session_id, %e, "rc:video-info send failed (control DC closed?)");
                            } else {
                                video_info_sent = true;
                            }
                        }
                    }
                    encoder = Some(enc);
                    encoder_dims = Some((w, h));
                    // Phase B — a fresh encoder starts at the full `ceiling`
                    // maxrate; force the AIMD to re-apply its current (possibly
                    // lower) target so we don't snap back up to the ceiling
                    // after a dim change / resolution switch.
                    if let Some(ctrl) = aimd.as_mut() {
                        ctrl.force_reapply();
                    }
                }
                Err(e) => {
                    tracing::error!(
                        %session_id,
                        codec_label,
                        %e,
                        "FFmpeg DC pump: encoder construction failed — exiting pump"
                    );
                    return;
                }
            }
        }

        // Lock-state transitions force a keyframe so the browser sees
        // a clean refresh when the operator's lock overlay paints or
        // clears.
        let is_locked_now = matches!(*lock_state_rx.borrow(), lock_state::LockState::Locked);
        if is_locked_now != was_locked_last_iter {
            if let Some(enc) = encoder.as_mut() {
                enc.request_keyframe();
            }
            was_locked_last_iter = is_locked_now;
        }

        // Apply browser-requested keyframe (PLI/RTCP equivalent on DC).
        if keyframe_requested.swap(false, std::sync::atomic::Ordering::SeqCst)
            && let Some(enc) = encoder.as_mut()
        {
            enc.request_keyframe();
        }

        let Some(enc) = encoder.as_mut() else {
            continue;
        };

        // Phase B — drive the AIMD off send-channel occupancy (the real DC
        // backpressure signal) each frame. Ceiling = the per-resolution,
        // relay-aware maxrate cap; the controller starts there and tracks the
        // link down under congestion / back up on recovery. `set_bitrate`
        // coarsens the target to a ladder before reconfiguring, so applying it
        // every frame is cheap (a no-op unless the coarse bucket moved).
        {
            let now = std::time::Instant::now();
            let ctrl = aimd.get_or_insert_with(|| {
                encode::aimd::AimdController::new(
                    ceiling,
                    encode::MIN_BITRATE_BPS,
                    ceiling,
                    ffmpeg_send_depth as u32,
                    now,
                )
            });
            ctrl.set_ceiling(ceiling);
            // Non-full occupancy sample so the additive-increase can recover
            // once the link has drained (the FULL samples come from the gate).
            let cap = send_tx.capacity();
            ctrl.observe(ffmpeg_send_depth.saturating_sub(cap) as u32, cap == 0, now);
            if let Some(target) = ctrl.take_pending() {
                enc.set_bitrate(target);
                if target != last_applied_bitrate {
                    info!(
                        %session_id,
                        codec_label,
                        ceiling_bps = ceiling,
                        target_bps = target,
                        "FFmpeg DC pump set_bitrate (AIMD)"
                    );
                }
                last_applied_bitrate = target;
            }
        }

        let encode_start = std::time::Instant::now();
        let packets = match enc.encode(frame.clone()).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(%session_id, codec_label, %e, "FFmpeg DC pump: encode error");
                continue;
            }
        };
        encode_us += encode_start.elapsed().as_micros() as u64;
        frames_encoded += 1;

        // Push each emitted packet through the framer + DC. FFmpeg may
        // emit zero packets for some inputs (buffered B-frame
        // equivalents); we set max_b_frames=0 so this is rare but
        // possible at GOP boundaries.
        let dc_arc = video_bytes_dc.lock().await.clone();
        let Some(dc) = dc_arc else {
            // rc.97 — DC not open yet (offer/answer/ICE/SCTP still setting
            // up). Force a keyframe so that whenever the DC *does* open, the
            // FIRST frame the browser receives is an IDR. Without this the
            // encoder proceeds along its GOP and the first delivered frame is
            // a delta → the browser's WebCodecs decoder rejects it with "A key
            // frame is required after configure() or flush()" → black screen
            // (field: GORAN-XMG-NEO16 HEVC). media_pump_vp9_444_dc already
            // does this; the FFmpeg pump didn't, so it only rendered when the
            // DC happened to open at a GOP boundary (timing luck). Covers both
            // HEVC and vp9_qsv DC paths.
            keyframe_requested.store(true, std::sync::atomic::Ordering::SeqCst);
            dc_unopen_drops += 1;
            continue;
        };
        // Handle exists but the channel hasn't reached Open yet — same guard.
        if dc.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {
            keyframe_requested.store(true, std::sync::atomic::Ordering::SeqCst);
            dc_unopen_drops += 1;
            continue;
        }

        // rc.106 — backpressure moved to the bounded send channel (the
        // `try_send` below sheds load instead of blocking). This block now
        // only powers the one-shot first-key-flagged-packet diagnostic.
        let has_keyframe = packets.iter().any(|p| p.is_keyframe);
        if has_keyframe && !first_keyframe_logged {
            first_keyframe_logged = true;
            info!(
                %session_id, codec_label, encoder = enc.name(),
                "FFmpeg DC pump: first key-flagged packet emitted (rc.98 — confirms IDR flagging; NVENC needs forced-idr=1)"
            );
        }
        // rc.106 — hand each framed packet to the send task rather than
        // chunk-sending inline. `try_send` NEVER blocks the capture/encode
        // loop: if the task is behind (the link can't drain a big motion/IDR
        // frame fast enough) the bounded channel fills and we shed THIS frame,
        // scheduling a resync keyframe for when the queue drains. The send
        // task owns the 16 KiB chunking + the flow-controlled `dc.send().await`
        // (still ≤ 16 KiB per message — the webrtc-data 65535-byte read-buffer
        // cap that rc.85 fixed). A single consumer preserves chunk order for
        // the browser reassembler.
        let send_start = std::time::Instant::now();
        for pkt in packets {
            let wire = bytes::Bytes::from(frame_video_bytes(
                &pkt.data,
                pkt.is_keyframe,
                frame.monotonic_us,
            ));
            match send_tx.try_send(wire) {
                Ok(()) => {
                    if resync_pending {
                        // First frame through after a drop burst — make the
                        // NEXT one a keyframe so the browser resyncs the
                        // deltas it missed during congestion.
                        resync_pending = false;
                        enc.request_keyframe();
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    frames_dropped_backpressure += 1;
                    resync_pending = true;
                    // Phase B — a full channel at try_send (a big IDR / motion
                    // frame the link can't drain) is a secondary congestion
                    // signal; note it to the AIMD (rate-limited MD internally).
                    // `enc` is in scope from the encode above.
                    if let Some(ctrl) = aimd.as_mut() {
                        ctrl.note_buffer_overflow(std::time::Instant::now());
                        if let Some(bps) = ctrl.take_pending() {
                            enc.set_bitrate(bps);
                            last_applied_bitrate = bps;
                        }
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    tracing::warn!(
                        %session_id, codec_label,
                        "FFmpeg DC pump: send task gone — exiting pump"
                    );
                    return;
                }
            }
        }
        send_us += send_start.elapsed().as_micros() as u64;

        // Heartbeat for log-grep observability. rc.88 adds per-stage
        // averages (capture/encode/send ms) so the field can localise
        // the bottleneck behind a low fps, plus the backpressure-drop
        // counter. `_avg_*` are over frames encoded since the last
        // heartbeat.
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            last_heartbeat = std::time::Instant::now();
            // rc.106 — these three are owned by the send task now; snapshot
            // them for the log line.
            let frames_sent = frames_sent.load(std::sync::atomic::Ordering::Relaxed);
            let bytes_written = bytes_written.load(std::sync::atomic::Ordering::Relaxed);
            let send_errors = send_errors.load(std::sync::atomic::Ordering::Relaxed);
            let window_frames = frames_encoded.saturating_sub(heartbeat_frames_base).max(1);
            let avg_capture_ms = (capture_us / window_frames) as f64 / 1000.0;
            let avg_encode_ms = (encode_us / window_frames) as f64 / 1000.0;
            let avg_send_ms = (send_us / window_frames) as f64 / 1000.0;
            info!(
                %session_id,
                codec_label,
                target_fps,
                constrained,
                target_bps = last_applied_bitrate,
                width = w,
                height = h,
                encoder = enc.name(),
                frames_captured, frames_encoded, frames_sent, bytes_written,
                send_errors, dc_unopen_drops, frames_dropped_backpressure,
                frames_skipped_backpressure, frames_empty,
                avg_capture_ms, avg_encode_ms, avg_send_ms,
                "FFmpeg DC pump heartbeat (≈2s window)"
            );
            heartbeat_frames_base = frames_encoded;
            capture_us = 0;
            encode_us = 0;
            send_us = 0;
        }
    }
}

/// Read the `ROOMLER_AGENT_VP9_FPS` env var. Default 30 (pre-rc.33
/// behaviour). Accepts 30 or 60 — any other value rounds to the
/// nearest of those two. Operator-opt-in escape hatch for 4K capable
/// hosts; default stays at 30 so CPU-starved boxes keep working
/// without a config touch.
#[cfg(feature = "vp9-444")]
fn vp9_444_target_fps_from_env() -> u32 {
    const DEFAULT_FPS: u32 = 30;
    match std::env::var("ROOMLER_AGENT_VP9_FPS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        Some(fps) if fps >= 45 => 60,
        Some(_) => 30,
        None => DEFAULT_FPS,
    }
}

/// Target fps for the FFmpeg HW DC pump (HEVC / vp9_qsv): explicit env wins,
/// else pick per-session by transport (Phase B).
///
/// An explicit `ROOMLER_AGENT_FFMPEG_FPS` ALWAYS wins (clamped 1..=240) — a
/// high-refresh host can force 60 even on a relay, or pin 30 on a direct link.
/// With no override: a direct/LAN link defaults to **60** (HW encode sustains
/// it and the capture backend caps the real delivered rate anyway); a
/// constrained relay-TCP link defaults to **30**, because 60 fps of HEVC
/// overruns the ~1-4 Mbps pipe and just sheds frames. Deliberately distinct
/// from the libvpx VP9-444 pump's `ROOMLER_AGENT_VP9_FPS` (default 30 — SW
/// 4:4:4 can't keep up at 60).
///
/// Pre-rc.93 the pump hardcoded 30, which throttled the scrap backend's
/// internal pacer to 30 fps and was the root of the vp9_qsv ~15 fps field bug.
#[cfg(feature = "ffmpeg-encoder")]
fn ffmpeg_target_fps(constrained: bool) -> u32 {
    if let Some(fps) = std::env::var("ROOMLER_AGENT_FFMPEG_FPS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        return fps.clamp(1, 240);
    }
    if constrained { 30 } else { 60 }
}

/// Attach the `input` data-channel message handler. Each inbound payload
/// is parsed as [`input::InputMsg`] and injected via the thread-pinned
/// OS backend. The injector is built once per channel (the first frame
/// may race with initialisation, but `open_default` is synchronous so
/// it's ready before the first real keystroke).
///
/// `lock_state_rx` lets the handler short-circuit injection when the
/// host's input desktop has transitioned to `winsta0\Winlogon` (Win+L
/// lock, UAC, etc.). On those transitions the user-context worker is
/// still attached to `winsta0\Default` and SendInput would silently
/// dispatch to the wrong desktop — events appear to be delivered from
/// the WS side but achieve nothing on the host. Dropping them at this
/// layer keeps the audit trail honest and avoids polluting `enigo`
/// internal state.
///
/// Unparseable payloads are dropped with a debug log — we don't want a
/// flood of warnings if the controller sends an unknown event type.
fn attach_input_handler(
    dc: Arc<RTCDataChannel>,
    lock_state_rx: tokio::sync::watch::Receiver<lock_state::LockState>,
) {
    // rc.57 — reset the per-process `to_pixels` diagnostic counter so
    // the FIRST 50 input events of THIS session land at INFO level
    // again. Without the reset, the static counter is exhausted after
    // session 1 and subsequent sessions only log at DEBUG — hiding any
    // session-specific norm/px mismatch (e.g. the Crystal-Clear-OFF
    // auto-downscale path, where the misposition reproduces but the
    // earlier rc.55 field log had no INFO dispatch lines to inspect).
    input::reset_input_diag_counter();
    // Injector is wrapped in `parking_lot::Mutex`-equivalent-style — we
    // don't have parking_lot imported here, so fall back to tokio's
    // Mutex. The inject() call is fast (just a channel send), so lock
    // contention is not a concern.
    let injector = std::sync::Arc::new(tokio::sync::Mutex::new(input::open_default()));
    // Counter for batched suppression logging. Without this, a busy
    // session with the host locked would spam one debug line per
    // mouse-move (~60 Hz when the operator is jiggling). Log every
    // 60th drop so the field gets a steady "yes, the suppression is
    // working" signal without filling the log file.
    let suppressed_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    // rc.26 — gate the Locked-state suppression on worker role.
    // SystemContext (LocalSystem) can drive Winlogon via `SendInput`
    // because it holds SE_TCB; suppressing input in that case blocks
    // remote unlock for no benefit. User-context still suppresses
    // (its SendInput cannot reach Winlogon and would silently fail).
    let sys_ctx_worker = is_system_context_worker();
    if sys_ctx_worker {
        info!(
            "input: SystemContext worker — Locked-state suppression disabled (remote unlock enabled)"
        );
    }
    dc.on_message(Box::new(move |msg| {
        let injector = injector.clone();
        let lock_state_rx = lock_state_rx.clone();
        let suppressed_count = suppressed_count.clone();
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
            // M3 Z-path: drop input early when the host is locked.
            // The browser's auto-reconnect ladder will keep the peer
            // alive across short lock screens; the operator just
            // can't drive the lock UI itself (that's the A1-path
            // future work).
            //
            // rc.26 — A1-path: under SystemContext, allow input
            // through. The injector thread runs as LocalSystem with
            // SE_TCB privilege, so SendInput CAN reach Winlogon's
            // input desktop. This is the "drive lock screen
            // remotely" path documented in
            // `docs/remote-control-m3-elevated-switching.md`
            // Change C ("refine the suppression policy under
            // SystemContext").
            if !sys_ctx_worker
                && matches!(*lock_state_rx.borrow(), lock_state::LockState::Locked)
            {
                let n = suppressed_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    .wrapping_add(1);
                // rc.25 — promote the FIRST suppressed event of a
                // run to INFO so the field gets a clear signal in
                // the default log level, then drop back to DEBUG
                // every 60th. Pre-rc.25 this was DEBUG-only, which
                // was invisible at the default INFO level and made
                // "input suppressed when admin pwsh hovered"
                // reports hard to confirm from the log.
                if n == 1 {
                    info!(
                        "input: lock_state=Locked — suppressing input (first event); see `lock_state: transition observed` for the observed desktop name"
                    );
                } else if n.is_multiple_of(60) {
                    debug!(
                        suppressed_total = n,
                        "input: host locked — suppressing input events"
                    );
                }
                return;
            }
            let mut guard = injector.lock().await;
            if let Err(e) = guard.inject(parsed) {
                debug!(%e, "input: inject failed");
            }
        })
    }));
}

/// Send `rc:host_locked` over the stashed `control` data channel.
/// No-op when the channel hasn't opened yet (session in negotiation),
/// when the channel has been torn down, or when the send itself fails
/// — none of those are recoverable from this task and a missing badge
/// is a much softer failure than a panicked emitter.
async fn emit_host_locked(
    stash: &Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    locked: bool,
) {
    let dc = {
        let guard = stash.lock().await;
        match guard.as_ref() {
            Some(dc) => dc.clone(),
            None => return,
        }
    };
    let payload = format!(r#"{{"t":"rc:host_locked","locked":{locked}}}"#);
    if let Err(e) = dc.send_text(payload).await {
        debug!(%e, "rc:host_locked send failed (control DC closed?)");
    }
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
    keyframe_requested: Arc<std::sync::atomic::AtomicBool>,
) {
    // Clone the Arc so the on_message closure can send replies
    // (e.g. rc:logs-fetch.reply) back over the same DC. Original
    // `dc` parameter is kept for the on_message registration below.
    let dc_for_reply = dc.clone();
    // rc.130 — min-gap clamp for browser-requested keyframes (rc:keyframe).
    // The atomic itself coalesces (the pump forces at most one IDR per encode
    // regardless of how often it's set) and the browser debounces, but this
    // bounds a misbehaving/old controller to one forced IDR per gap so a
    // resync storm can't pile the LARGEST frames onto a congested link.
    let last_kf_request = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
    dc.on_message(Box::new(move |msg| {
        let quality_state = quality_state.clone();
        let target_resolution = target_resolution.clone();
        let keyframe_requested = keyframe_requested.clone();
        let last_kf_request = last_kf_request.clone();
        let dc_for_reply = dc_for_reply.clone();
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
                "rc:logs-fetch" => {
                    // rc.23 diagnostic feature; rc.24 added reply
                    // streaming. Browser requests the tail of the
                    // agent's current rolling log file so the
                    // operator can see what's actually happening on
                    // the host without RDPing in. Single round-trip
                    // for sub-32-KB payloads (rc.23-compatible);
                    // chunked stream for larger ones because a
                    // single SCTP message can't exceed the
                    // negotiated `max_message_size` (65536 default)
                    // — field repro the field-test host 2026-05-13 showed
                    // 1000-line requests silently dropping.
                    let lines = val
                        .get("lines")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(200)
                        .clamp(1, 5000) as usize;
                    let request_id = val
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    info!(
                        %session_id,
                        request_id = ?request_id,
                        lines,
                        "control: rc:logs-fetch received"
                    );
                    let envelopes = match logs_fetch::fetch_tail_chunked(
                        lines,
                        request_id.as_deref(),
                    )
                    .await
                    {
                        Ok(envs) => envs,
                        Err(e) => {
                            warn!(%session_id, %e, "control: rc:logs-fetch fetch_tail_chunked failed");
                            vec![serde_json::json!({
                                "t": "rc:logs-fetch.reply",
                                "ok": false,
                                "error": format!("{e:#}"),
                            })]
                        }
                    };
                    let envelope_count = envelopes.len();
                    info!(
                        %session_id,
                        envelopes = envelope_count,
                        "control: rc:logs-fetch sending reply"
                    );
                    for env in envelopes {
                        let text = match serde_json::to_string(&env) {
                            Ok(s) => s,
                            Err(e) => {
                                debug!(%session_id, %e, "control: rc:logs-fetch.reply serialise failed");
                                continue;
                            }
                        };
                        if let Err(e) = dc_for_reply.send_text(text).await {
                            debug!(%session_id, %e, "control: rc:logs-fetch.reply send failed");
                            // Stop sending the rest of the stream —
                            // browser will get a partial response and
                            // can retry. Better than spamming dead
                            // sends.
                            break;
                        }
                    }
                }
                "rc:keyframe" => {
                    // Browser's decode queue backed up → it dropped deltas and
                    // needs a fresh IDR to resync. Force one (min-gap clamped).
                    const MIN_KF_GAP: Duration = Duration::from_millis(200);
                    let now = std::time::Instant::now();
                    let mut guard = last_kf_request.lock().unwrap();
                    let allow = guard
                        .map(|t| now.duration_since(t) >= MIN_KF_GAP)
                        .unwrap_or(true);
                    if allow {
                        *guard = Some(now);
                        drop(guard);
                        keyframe_requested.store(true, std::sync::atomic::Ordering::Relaxed);
                        debug!(%session_id, "control: rc:keyframe — forcing IDR (browser decode-backlog resync)");
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
        // rc.38 — bumped 33 ms (30 Hz) → 8 ms (120 Hz) after the field-test host
        // field test 2026-05-17 surfaced sluggish cursor tracking even
        // when the controller's local pointermove was timely.
        // Operator perceives "where the cursor is" via:
        //   (a) the synthetic local cursor at the controller's
        //       pointermove position (instant), and
        //   (b) the remote-reported cursor canvas at the agent's
        //       polled position (this poller's cadence).
        // The browser hides (a) once (b) reports a shape, so the
        // poller's cadence dominates "feels-responsive" once the
        // first cursor shape arrives. 120 Hz matches RustDesk and is
        // cheap: each tick is one GetCursorInfo + JSON encode + DC
        // send_text, well under 1 ms even on weak hosts. Idle frames
        // dedupe via the tracker's per-shape cache so we don't burn
        // DC bandwidth on a static cursor.
        let mut ticker = tokio::time::interval(Duration::from_millis(8));
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
    // rc.44 — per-session reassembler for `clipboard:write-chunk`
    // envelopes. Shared across all on_message callbacks via the Arc.
    // Browser-side chunker is bounded at 14 KB per envelope; this
    // reassembler enforces the 1 MB total cap per write transaction
    // (see [`clipboard::MAX_CLIPBOARD_BYTES`]). `std::sync::Mutex` is
    // fine here: the lock is held for the duration of one synchronous
    // `feed()` call (push_str + invariant checks) and dropped before
    // the awaited `cb.write()`, so we never hold the guard across an
    // .await point.
    let reassembler = Arc::new(std::sync::Mutex::new(
        crate::clipboard::WriteReassembler::new(),
    ));
    let dc_for_handler = dc.clone();
    dc.on_message(Box::new(move |msg| {
        let dc = dc_for_handler.clone();
        let cb = cb.clone();
        let reassembler = reassembler.clone();
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
                crate::clipboard::ClipboardIncoming::WriteChunk {
                    id,
                    seq,
                    text,
                    last,
                } => {
                    let chunk_bytes = text.len();
                    let outcome = {
                        let mut g = reassembler.lock().expect("clipboard reassembler poisoned");
                        g.feed(id.clone(), seq, text, last)
                    };
                    match outcome {
                        crate::clipboard::WriteChunkOutcome::Pending => {
                            debug!(%session_id, id=%id, seq, bytes=chunk_bytes, "clipboard: chunk accepted, awaiting more");
                        }
                        crate::clipboard::WriteChunkOutcome::Complete(full_text) => {
                            let bytes = full_text.len();
                            match cb.write(full_text).await {
                                Ok(()) => info!(%session_id, id=%id, bytes, chunks=seq + 1, "clipboard: wrote chunked payload to host"),
                                Err(e) => {
                                    warn!(%session_id, id=%id, %e, "clipboard: chunked write failed");
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
                        crate::clipboard::WriteChunkOutcome::Rejected(reason) => {
                            warn!(%session_id, id=%id, %reason, "clipboard: chunk rejected");
                            let reply = serde_json::json!({
                                "t": "clipboard:error",
                                "message": reason,
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
                        if bytes <= crate::clipboard::CHUNK_BYTES {
                            // Single envelope — back-compat path for
                            // browsers that don't yet handle
                            // `clipboard:content-chunk`. Small payloads
                            // (most common case) fit inside a single
                            // SCTP message comfortably.
                            info!(%session_id, bytes, "clipboard: read from host (single envelope)");
                            let reply = serde_json::json!({
                                "t": "clipboard:content",
                                "text": text,
                                "req_id": req_id,
                            });
                            if let Ok(s) = serde_json::to_string(&reply) {
                                let _ = dc.send_text(s).await;
                            }
                        } else {
                            // Large payload — chunk it so each
                            // envelope stays under the SCTP ceiling.
                            // Browser reassembles by `req_id` until
                            // `last: true`.
                            let chunks = crate::clipboard::split_into_chunks(&text);
                            let total = chunks.len();
                            info!(%session_id, bytes, chunks = total, "clipboard: read from host (chunked)");
                            for (i, chunk) in chunks.iter().enumerate() {
                                let reply = serde_json::json!({
                                    "t": "clipboard:content-chunk",
                                    "req_id": req_id,
                                    "seq": i as u32,
                                    "text": chunk,
                                    "last": i + 1 == total,
                                });
                                if let Ok(s) = serde_json::to_string(&reply) {
                                    if dc.send_text(s).await.is_err() {
                                        debug!(%session_id, "clipboard: DC closed mid-chunk-send; abandoning");
                                        break;
                                    }
                                }
                            }
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
///
/// Public so `crates/tests/src/file_dc_tests.rs` can attach the same
/// dispatcher to a loopback DC and lock the wire format end-to-end.
/// The dispatcher itself stays private (free fns below) — only the
/// wiring entry point is needed across crates.
pub fn attach_files_handler(dc: Arc<RTCDataChannel>, session_id: bson::oid::ObjectId) {
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
            rel_path,
            dest_path,
        } => {
            info!(%session_id, %id, %name, size, ?mime, ?rel_path, ?dest_path, "files: begin");
            match handler
                .begin(
                    id.clone(),
                    name,
                    size,
                    rel_path.as_deref(),
                    dest_path.as_deref(),
                )
                .await
            {
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
        crate::files::FilesIncoming::Get { id, path } => {
            info!(%session_id, %id, path = %path, "files: get (download requested)");
            spawn_outgoing_pump(dc.clone(), handler, session_id, id, path);
        }
        crate::files::FilesIncoming::GetFolder { id, path, format } => {
            // v1 only honours `format=zip` (or unset, treated as zip).
            if let Some(f) = format.as_deref()
                && f != "zip"
            {
                warn!(%session_id, %id, format = %f, "files: get-folder unsupported format");
                send_files_json(
                    &dc,
                    &crate::files::FilesOutgoing::Error {
                        id: &id,
                        message: "unsupported folder-download format (only 'zip' is supported)",
                    },
                )
                .await;
                return;
            }
            info!(%session_id, %id, path = %path, "files: get-folder (zip) requested");
            spawn_outgoing_zip_pump(dc.clone(), handler, session_id, id, path);
        }
        crate::files::FilesIncoming::Cancel { id } => {
            // rc.19: try both directions. cancel_outgoing flips a
            // flag if the id matches an in-flight download.
            // cancel_incoming clears upload state + removes the
            // .roomler-partial/<id>/ staging dir + registry entry so
            // the partial doesn't sit until the 24h orphan sweep.
            // Browsers send files:cancel on terminal upload failure
            // (6 reconnect attempts exhausted).
            let out_cancelled = handler.cancel_outgoing(&id).await;
            let in_cancelled = handler.cancel_incoming(&id).await;
            info!(
                %session_id, %id, out_cancelled, in_cancelled,
                "files: cancel requested"
            );
        }
        crate::files::FilesIncoming::Dir { req_id, path } => {
            if !crate::files::is_remote_browse_enabled() {
                info!(%session_id, %req_id, path = %path, "files: dir refused — remote browse disabled");
                send_files_json(
                    &dc,
                    &crate::files::FilesOutgoing::DirError {
                        req_id: &req_id,
                        message: "remote browse disabled by host config",
                    },
                )
                .await;
                return;
            }
            match crate::files::list_dir(&path).await {
                Ok(listing) => {
                    info!(
                        %session_id, %req_id,
                        path = %listing.path,
                        entries = listing.entries.len(),
                        "files: dir listed"
                    );
                    let parent_owned = listing.parent.clone();
                    send_files_json(
                        &dc,
                        &crate::files::FilesOutgoing::DirList {
                            req_id: &req_id,
                            path: &listing.path,
                            parent: parent_owned.as_deref(),
                            entries: &listing.entries,
                        },
                    )
                    .await;
                }
                Err(e) => {
                    warn!(%session_id, %req_id, path = %path, %e, "files: dir failed");
                    let msg = format!("{e}");
                    send_files_json(
                        &dc,
                        &crate::files::FilesOutgoing::DirError {
                            req_id: &req_id,
                            message: &msg,
                        },
                    )
                    .await;
                }
            }
        }
        crate::files::FilesIncoming::Resume {
            id,
            offset,
            sha256_prefix: _,
        } => {
            // rc.19 P2: look up the partial in PARTIAL_REGISTRY (or
            // on-demand stat under Downloads), truncate the data
            // file to a 256 KiB-aligned offset, reinstall
            // IncomingTransfer state in this DC's incoming Mutex,
            // and reply `files:resumed { id, accepted_offset }`.
            // sha256_prefix is reserved for v2 — v1 ignores it.
            match handler.resume_incoming(&id, offset).await {
                Ok(accepted_offset) => {
                    info!(
                        %session_id, %id, requested = %offset,
                        accepted_offset, "files: resume accepted"
                    );
                    send_files_json(
                        &dc,
                        &crate::files::FilesOutgoing::Resumed {
                            id: &id,
                            accepted_offset,
                        },
                    )
                    .await;
                }
                Err(e) => {
                    warn!(%session_id, %id, %offset, %e, "files: resume rejected");
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
    }
}

/// Spawn a tokio task that pumps an outgoing single-file download.
/// The task owns the `Arc<RTCDataChannel>` so the DC outlives the
/// stream even if the original `attach_files_handler` closure has
/// returned. Cancellation flows via the AtomicBool on
/// `OutgoingTransfer`; the caller flips it via `cancel_outgoing`.
fn spawn_outgoing_pump(
    dc: Arc<RTCDataChannel>,
    handler: crate::files::FilesHandler,
    session_id: bson::oid::ObjectId,
    id: String,
    requested_path: String,
) {
    tokio::spawn(async move {
        // begin_outgoing validates the path + denylist, opens the
        // file, and stashes outgoing state. Success → send `Offer`
        // and start streaming.
        let offer = match handler.begin_outgoing(id.clone(), &requested_path).await {
            Ok(o) => o,
            Err(e) => {
                warn!(%session_id, %id, path = %requested_path, %e, "files: begin_outgoing failed");
                let msg = format!("{e}");
                send_files_json(
                    &dc,
                    &crate::files::FilesOutgoing::Error {
                        id: &id,
                        message: &msg,
                    },
                )
                .await;
                return;
            }
        };

        send_files_json(
            &dc,
            &crate::files::FilesOutgoing::Offer {
                id: &offer.id,
                name: &offer.name,
                size: offer.size,
                mime: offer.mime,
            },
        )
        .await;

        let bytes_sent = match pump_outgoing_file(&dc, &handler, &offer).await {
            Ok(n) => n,
            Err(e) => {
                warn!(%session_id, id = %offer.id, %e, "files: pump_outgoing failed");
                let msg = format!("{e}");
                send_files_json(
                    &dc,
                    &crate::files::FilesOutgoing::Error {
                        id: &offer.id,
                        message: &msg,
                    },
                )
                .await;
                handler.finish_outgoing(&offer.id).await;
                return;
            }
        };

        // Successful end-of-stream: send Eof so browser closes
        // the writable cleanly, then clear state.
        info!(
            %session_id, id = %offer.id, bytes_sent, path = %offer.path.display(),
            "files: outgoing complete"
        );
        send_files_json(
            &dc,
            &crate::files::FilesOutgoing::Eof {
                id: &offer.id,
                bytes: bytes_sent,
            },
        )
        .await;
        handler.finish_outgoing(&offer.id).await;
    });
}

/// Spawn a tokio task that streams a folder as a zip. The zip is
/// produced by `async_zip::tokio::write::ZipFileWriter` writing into
/// the write end of a `tokio::io::duplex` pipe. A second task reads
/// from the pipe and pushes chunks to the DC with backpressure.
/// The bounded duplex buffer (256 KiB) is what gives async_zip
/// natural backpressure: if the DC drains slowly, the pipe fills
/// and async_zip's writes block.
fn spawn_outgoing_zip_pump(
    dc: Arc<RTCDataChannel>,
    handler: crate::files::FilesHandler,
    session_id: bson::oid::ObjectId,
    id: String,
    requested_path: String,
) {
    tokio::spawn(async move {
        let offer = match handler
            .begin_outgoing_zip(id.clone(), &requested_path)
            .await
        {
            Ok(o) => o,
            Err(e) => {
                warn!(%session_id, %id, path = %requested_path, %e, "files: begin_outgoing_zip failed");
                let msg = format!("{e}");
                send_files_json(
                    &dc,
                    &crate::files::FilesOutgoing::Error {
                        id: &id,
                        message: &msg,
                    },
                )
                .await;
                return;
            }
        };

        send_files_json(
            &dc,
            &crate::files::FilesOutgoing::Offer {
                id: &offer.id,
                name: &offer.name,
                size: None, // streaming — total unknown
                mime: offer.mime,
            },
        )
        .await;

        // Bounded duplex pipe: write side fed by async_zip; read
        // side fed to the DC. 256 KiB buffer = ~4 of our 64 KiB
        // chunks before async_zip's writes start blocking. Keeps
        // memory usage low and gives backpressure-free crash
        // protection if the DC is wedged.
        const PIPE_BUFFER: usize = 256 * 1024;
        let (writer_half, reader_half) = tokio::io::duplex(PIPE_BUFFER);
        let cancel = offer.cancel.clone();
        let path = offer.path.clone();
        let walk_cancel = cancel.clone();
        let walk_handle = tokio::spawn(async move {
            crate::files::walk_and_zip(writer_half, &path, walk_cancel).await
        });

        let dc_for_pump = dc.clone();
        let pump_cancel = cancel.clone();
        let id_for_pump = offer.id.clone();
        let pump_handle = tokio::spawn(async move {
            zip_pump_loop(dc_for_pump, reader_half, pump_cancel, id_for_pump).await
        });

        // Wait for both sides. The walk task closes the writer
        // half; the pump task sees EOF and exits.
        let walk_res = walk_handle.await;
        let pump_res = pump_handle.await;

        let total_bytes = match (walk_res, pump_res) {
            (Ok(Ok(_count)), Ok(Ok(bytes_sent))) => bytes_sent,
            (Ok(Err(e)), _) | (_, Ok(Err(e))) => {
                warn!(%session_id, id = %offer.id, %e, "files: zip pump failed");
                let msg = format!("{e}");
                send_files_json(
                    &dc,
                    &crate::files::FilesOutgoing::Error {
                        id: &offer.id,
                        message: &msg,
                    },
                )
                .await;
                handler.finish_outgoing(&offer.id).await;
                return;
            }
            (Err(je), _) | (_, Err(je)) => {
                warn!(%session_id, id = %offer.id, %je, "files: zip pump task panicked");
                send_files_json(
                    &dc,
                    &crate::files::FilesOutgoing::Error {
                        id: &offer.id,
                        message: "zip pump task panicked",
                    },
                )
                .await;
                handler.finish_outgoing(&offer.id).await;
                return;
            }
        };

        info!(
            %session_id, id = %offer.id, total_bytes,
            path = %offer.path.display(),
            "files: outgoing zip complete"
        );
        send_files_json(
            &dc,
            &crate::files::FilesOutgoing::Eof {
                id: &offer.id,
                bytes: total_bytes,
            },
        )
        .await;
        handler.finish_outgoing(&offer.id).await;
    });
}

/// Pump bytes from the duplex reader to the DC, applying SCTP
/// backpressure. Returns total bytes sent. Exits on EOF, cancel,
/// or DC failure.
async fn zip_pump_loop(
    dc: Arc<RTCDataChannel>,
    mut reader: tokio::io::DuplexStream,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    id: String,
) -> anyhow::Result<u64> {
    use tokio::io::AsyncReadExt;
    const CHUNK: usize = 64 * 1024;
    const BACKPRESSURE_HIGH: usize = 4 * 1024 * 1024;

    let mut buf = vec![0u8; CHUNK];
    let mut total: u64 = 0;
    let mut last_progress: u64 = 0;
    loop {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err(anyhow::anyhow!("cancelled by browser"));
        }
        let n = match reader.read(&mut buf).await {
            Ok(0) => break, // EOF — zip writer closed
            Ok(n) => n,
            Err(e) => return Err(anyhow::anyhow!("duplex read: {e}")),
        };
        // Backpressure on SCTP send buffer.
        while dc.buffered_amount().await > BACKPRESSURE_HIGH {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err(anyhow::anyhow!("cancelled by browser"));
            }
        }
        let chunk = bytes::Bytes::copy_from_slice(&buf[..n]);
        if let Err(e) = dc.send(&chunk).await {
            return Err(anyhow::anyhow!("dc.send failed: {e}"));
        }
        total += n as u64;
        if total - last_progress >= 256 * 1024 {
            last_progress = total;
            send_files_json(
                &dc,
                &crate::files::FilesOutgoing::Progress {
                    id: &id,
                    bytes: total,
                },
            )
            .await;
        }
    }
    Ok(total)
}

/// Pump a single open file through the DC in 64 KiB chunks. Backs
/// off when the SCTP send buffer is over 4 MiB to avoid OOMing on
/// large files. Checks the cancel flag between chunks. Returns the
/// total bytes sent on clean stream exit.
async fn pump_outgoing_file(
    dc: &Arc<RTCDataChannel>,
    handler: &crate::files::FilesHandler,
    offer: &crate::files::OutgoingOffer,
) -> anyhow::Result<u64> {
    use tokio::io::AsyncReadExt;
    const CHUNK: usize = 64 * 1024;
    const BACKPRESSURE_HIGH: u64 = 4 * 1024 * 1024;

    let mut file = handler.open_outgoing(&offer.id).await?;
    let mut buf = vec![0u8; CHUNK];
    let mut total: u64 = 0;
    let mut last_progress: u64 = 0;
    loop {
        if offer.cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err(anyhow::anyhow!("cancelled by browser"));
        }
        // Backpressure: poll buffered_amount and yield until it
        // drops below the high-watermark. webrtc-rs's DC reports
        // bufferedAmount synchronously.
        while dc.buffered_amount().await > BACKPRESSURE_HIGH as usize {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if offer.cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err(anyhow::anyhow!("cancelled by browser"));
            }
        }
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let chunk = bytes::Bytes::copy_from_slice(&buf[..n]);
        if let Err(e) = dc.send(&chunk).await {
            return Err(anyhow::anyhow!("dc.send failed: {e}"));
        }
        total += n as u64;
        // Progress reports every 256 KiB
        if total - last_progress >= 256 * 1024 {
            last_progress = total;
            send_files_json(
                dc,
                &crate::files::FilesOutgoing::Progress {
                    id: &offer.id,
                    bytes: total,
                },
            )
            .await;
        }
    }
    Ok(total)
}

async fn handle_files_chunk(
    dc: Arc<RTCDataChannel>,
    handler: crate::files::FilesHandler,
    session_id: bson::oid::ObjectId,
    data: &[u8],
) {
    // Capture the active transfer's id BEFORE we run chunk(); on the
    // error path we need it to address the `files:error` reply
    // correctly. Without this the browser's per-upload promise
    // listener (which filters by id) silently drops the error and
    // the upload spinner spins forever — field repro the field-test host rc.8
    // (2026-05-06).
    let active_id = handler.current_id().await.unwrap_or_default();
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
            warn!(%session_id, id = %active_id, %e, "files: chunk failed");
            let msg = format!("{e}");
            send_files_json(
                &dc,
                &crate::files::FilesOutgoing::Error {
                    id: &active_id,
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

/// Filter mapped ICE servers to the TURNS-over-TCP relay only, for
/// `ROOMLER_AGENT_ICE_RELAY_TCP` mode. Hostile-NAT hosts (WSL2 +
/// wsl-vpnkit, other userspace-VPN stacks) mangle UDP source ports so the
/// TURN allocation refresh fails and the media peer flaps; a single
/// TURNS/TCP connection (handled by the vendored `webrtc-ice` TCP branch)
/// survives it. Keeps only `turns:…?transport=tcp` URLs and drops STUN +
/// plain-UDP TURN. Returns the full mapping unchanged when no TCP-relay
/// URL is present, so the knob can never break connectivity outright.
fn map_ice_servers_relay_tcp(servers: &[IceServer]) -> Vec<RTCIceServer> {
    let all = map_ice_servers(servers);
    let filtered: Vec<RTCIceServer> = all
        .iter()
        .filter_map(|s| {
            let tcp_urls: Vec<String> = s
                .urls
                .iter()
                .filter(|u| {
                    let lu = u.to_ascii_lowercase();
                    lu.starts_with("turns:") && lu.contains("transport=tcp")
                })
                .cloned()
                .collect();
            (!tcp_urls.is_empty()).then(|| RTCIceServer {
                urls: tcp_urls,
                username: s.username.clone(),
                credential: s.credential.clone(),
            })
        })
        .collect();
    if filtered.is_empty() {
        warn!(
            "ICE_RELAY_TCP set but no turns:…?transport=tcp URL available — using all ICE servers"
        );
        all
    } else {
        info!(
            servers = filtered.len(),
            "ICE relay-over-TCP: media pinned to TURNS/TCP relay (hostile-NAT mode)"
        );
        filtered
    }
}

#[cfg(test)]
mod ice_relay_tcp_tests {
    use super::{map_ice_servers, map_ice_servers_relay_tcp};
    use roomler_ai_remote_control::signaling::IceServer;

    fn srv(urls: &[&str], with_cred: bool) -> IceServer {
        IceServer {
            urls: urls.iter().map(|s| s.to_string()).collect(),
            username: with_cred.then(|| "u".to_string()),
            credential: with_cred.then(|| "c".to_string()),
        }
    }

    #[test]
    fn relay_tcp_keeps_only_turns_tcp_and_drops_stun_and_udp() {
        let servers = vec![
            srv(&["stun:stun.l.google.com:19302"], false),
            srv(
                &[
                    "turn:coturn.example:443?transport=udp",
                    "turn:coturn.example:3478?transport=tcp",
                    "turns:coturn.example:443?transport=tcp",
                ],
                true,
            ),
        ];
        let out = map_ice_servers_relay_tcp(&servers);
        // Only the server that carries a `turns:…?transport=tcp` URL
        // survives, and only that URL is kept (STUN + UDP + plain-TCP TURN
        // dropped — the vendored ice fork only handles TURNS-over-TCP).
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].urls, vec!["turns:coturn.example:443?transport=tcp"]);
        assert_eq!(out[0].username, "u");
        assert_eq!(out[0].credential, "c");
    }

    #[test]
    fn relay_tcp_falls_back_to_all_when_no_tcp_relay() {
        // No `turns:…?transport=tcp` anywhere → never break connectivity;
        // return the full mapping unchanged.
        let servers = vec![
            srv(&["stun:stun.l.google.com:19302"], false),
            srv(&["turn:coturn.example:3478?transport=udp"], true),
        ];
        let out = map_ice_servers_relay_tcp(&servers);
        assert_eq!(out.len(), map_ice_servers(&servers).len());
        assert_eq!(out.len(), 2);
    }
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

/// Opus capability for the system-audio track. MUST match webrtc-rs's
/// default MediaEngine Opus registration byte-for-byte — mime
/// `audio/opus`, clock_rate 48000, channels 2, fmtp
/// `minptime=10;useinbandfec=1`, empty rtcp_feedback (PT 111). A
/// mismatch on any field makes the transceiver fail to resolve a
/// payload type and the browser gets an m=audio section it can't bind.
/// (Verified against `crates/vendored/webrtc/.../media_engine/mod.rs`.)
#[cfg(feature = "audio")]
fn build_audio_codec_cap() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: "audio/opus".to_string(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
        rtcp_feedback: vec![],
    }
}

/// Per-session system-audio pump. Captures desktop/system audio,
/// encodes to Opus (48 kHz stereo, 20 ms frames), and writes each
/// packet into the WebRTC Opus track. Self-regulating: with no capture
/// backend (macOS, or a device-open failure) `open_default` returns a
/// Noop that parks forever, so this task idles producing no samples —
/// the m=audio section is still negotiated but stays silent.
///
/// Each 20 ms Opus packet is written with a fixed `duration` of 20 ms so
/// the track's RTP timestamps advance at the real audio clock (unlike
/// the video pump, audio frames are inherently fixed-cadence).
///
/// Aborted by `AgentPeer::close()`.
#[cfg(feature = "audio")]
async fn audio_pump(session_id: bson::oid::ObjectId, audio_track: Arc<TrackLocalStaticSample>) {
    use crate::audio;

    let mut capture = audio::open_default();
    let mut encoder = match audio::opus_encode::OpusEncoder::new() {
        Ok(e) => e,
        Err(e) => {
            warn!(%session_id, %e, "audio: failed to init Opus encoder — audio pump exiting");
            return;
        }
    };

    // 20 ms per Opus frame (960 samples/ch @ 48 kHz). Fixed cadence —
    // the browser's jitter buffer relies on this.
    const FRAME_DURATION: Duration = Duration::from_millis(20);

    let mut frames_captured: u64 = 0;
    let mut packets_sent: u64 = 0;
    let mut write_errors: u64 = 0;
    let mut bytes_sent: u64 = 0;
    let mut last_heartbeat = std::time::Instant::now();
    const HEARTBEAT: Duration = Duration::from_secs(5);

    info!(%session_id, "audio pump started");

    loop {
        let frame = match capture.next_frame().await {
            Ok(Some(f)) => f,
            Ok(None) => {
                // Capture exhausted (stream torn down). Nothing more to
                // do — exit cleanly.
                info!(
                    %session_id,
                    frames_captured, packets_sent,
                    "audio: capture exhausted — audio pump exiting"
                );
                return;
            }
            Err(e) => {
                warn!(%session_id, %e, "audio: capture error — audio pump exiting");
                return;
            }
        };
        frames_captured += 1;

        let packets = match encoder.push(&frame.samples, frame.channels, frame.sample_rate) {
            Ok(p) => p,
            Err(e) => {
                warn!(%session_id, %e, "audio: opus encode error — audio pump exiting");
                return;
            }
        };

        for packet in packets {
            let len = packet.len() as u64;
            let sample = Sample {
                data: Bytes::from(packet),
                timestamp: SystemTime::now(),
                duration: FRAME_DURATION,
                packet_timestamp: 0,
                prev_dropped_packets: 0,
                prev_padding_packets: 0,
            };
            if let Err(e) = audio_track.write_sample(&sample).await {
                write_errors += 1;
                // Sample once per ~5s heartbeat window rather than per
                // failure; a dead track fails every frame and would flood.
                if write_errors == 1 {
                    warn!(%session_id, %e, "audio: write_sample failed (first)");
                }
            } else {
                packets_sent += 1;
                bytes_sent += len;
            }
        }

        if last_heartbeat.elapsed() >= HEARTBEAT {
            info!(
                %session_id,
                frames_captured,
                packets_sent,
                bytes_sent,
                write_errors,
                "audio pump heartbeat"
            );
            last_heartbeat = std::time::Instant::now();
        }
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
    fn target_bitrate_high_caps_at_50_mbps() {
        // 1920×1200 base is 24 Mbps after the rc.36 bpp/cap bump; High
        // should add 50% giving 36 Mbps (under the 50 Mbps cap).
        assert_eq!(target_bitrate(HIGH, 12_000_000), 18_000_000);
        // 4K60 base saturates MAX_BITRATE_BPS at 40 Mbps; High then
        // multiplies × 1.5 → 60 Mbps which the post-multiply cap
        // clamps back to the rc.36 ceiling of 50 Mbps.
        assert_eq!(target_bitrate(HIGH, 40_000_000), 50_000_000);
        // Very high synthetic base: cap engages.
        assert_eq!(target_bitrate(HIGH, 50_000_000), 50_000_000);
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

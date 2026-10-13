// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-16 L0 — capture → decode latency of a REAL remote-desktop session, in
//! CI, on one clock.
//!
//! The agent side is the production `AgentPeer` with the production media
//! pump, on the synthetic frame source and openh264. The controller side is a
//! plain webrtc-rs peer that receives the RTP track, reassembles access units
//! itself (webrtc-rs' `SampleBuilder` holds a frame until the NEXT packet
//! arrives, which would add a whole frame interval to every reading), decodes
//! them with openh264 and reads the synthetic source's marker back out of the
//! decoded luma. The capture instant of that sequence number is in
//! `synthetic_backend::clock`, in this same process: latency is
//! `decode_done − captured`, no clock offset anywhere.
//!
//! Both peers sit on a webrtc-util `vnet`, so delay, jitter and loss are set
//! per cell and deterministic. The cells and what each must show:
//!
//! | Cell | Link | Must show |
//! |---|---|---|
//! | `clean` | no delay | frames flow at ≥ 0.75× the source rate, p50 under a generous debug-build bound |
//! | `delay100` | +100 ms one way | p50 rises by ≥ 80 ms: the metric sees the network |
//! | `gate50` | a planted 50 ms capture-stage hold | p50 rises by ≥ 20 ms: the metric sees the pipeline. This is the negative control a gate needs; a harness that cannot be made to fail is not one |
//! | `lossy` | +20 ms, 3 % of media packets dropped | the session survives and keeps decoding; its numbers are reported |
//! | `probe` | no delay, L4 probes on | a probe sent on the `input` channel comes back painted into decoded frames |
//! | `probe50` | +50 ms one way, L4 probes on | the probe round trip rises by ≥ 80 ms: it crosses the link in both directions |
//! | `gate_lgcy` / `gate_slot` | an emulated 25 fps event-driven source behind each capture gate (FR-98 C1) | the slot-first gate's p50 is lower by ≥ a quarter period: the clock starts when a frame lands in the slot, so a frame the legacy gate holds there counts its wait |
//! | `playout0` | no delay; the agent's `playout-delay` 0/0 switch on (FR-98 C6) | the extension is negotiated and on every video packet at 0/0, without costing frames. In every other cell (the switch off) the answer does not negotiate it at all, though the controller offers it as Chrome does |
//!
//! The two `probe` cells are L4 in process. A bench viewer sends
//! `{"t":"probe","seq":N}` on the input DataChannel; the synthetic source
//! paints the last probe it saw into a second marker, bottom-right; the
//! viewer times send → the first decoded frame carrying `N`. That is input
//! to photon through the real agent, minus the host's own input stack and
//! the viewer's display, which the field half of L4 covers.
//!
//! Absolute numbers here are from an unoptimised build on a CI runner. The
//! field numbers come from L4 in the real viewer; this file is the gate that
//! catches a regression in the pipeline, and proves it can.

#![cfg(all(feature = "openh264-encoder", feature = "synthetic-frame-source"))]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use roomler_ai_remote_control::permissions::Permissions;
use roomler_ai_remote_control::signaling::ClientMsg;
use roomlerd::capture::gate::GateMode;
use roomlerd::capture::synthetic_backend::{self, clock, marker};
use roomlerd::encode::EncoderPreference;
use roomlerd::peer::{AgentPeer, l0};
use roomlerd::rc_overlay::RcOverlay;
use tokio::sync::{Mutex, mpsc};
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::rtp::packetizer::Depacketizer;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::util::vnet::chunk::Chunk;
use webrtc::util::vnet::net::{Net, NetConfig};
use webrtc::util::vnet::router::{Router, RouterConfig};

const AGENT_IP: &str = "1.2.3.4";
const CONTROLLER_IP: &str = "1.2.3.5";

/// The software track pump's frame rate (`peer.rs` `TARGET_FPS_SW`).
const SOURCE_FPS: f64 = 30.0;

/// How long a cell streams, and how much of the start it ignores (ICE,
/// DTLS, the first IDR, the encoder's opening ramp).
const RUN: Duration = Duration::from_secs(8);
const WARMUP: Duration = Duration::from_millis(1_500);

/// A gap between two decoded frames longer than this is a freeze (FR-16).
const FREEZE: Duration = Duration::from_millis(200);

/// How often a probe cell sends a probe: at least three frame intervals
/// apart, so every probe gets a frame of its own, plus a jitter of up to one
/// frame interval. Without the jitter a 100 ms period is exactly three
/// frames, every probe lands on the same phase of the capture clock, and a
/// cell reports one arbitrary point of the wait for the next capture
/// (anywhere in 0–33 ms) instead of its distribution: two cells then differ
/// by up to a frame interval for no reason at all.
const PROBE_EVERY: Duration = Duration::from_millis(100);
const PROBE_JITTER_MS: u64 = 34;

/// The clean cell must decode at least this share of the source rate. Not
/// 0.9: an unoptimised build on a busy machine measured 20 fps (a parallel
/// repo-wide grep on the same box), and a gate that flakes under load gets
/// ignored. A real cadence regression (half the rate) still fails it.
const CLEAN_FPS_FLOOR: f64 = 0.75;

/// One link condition.
#[derive(Clone, Copy, Debug)]
struct Cell {
    name: &'static str,
    delay: Duration,
    jitter: Duration,
    /// Per-mille of agent→controller packets dropped.
    loss_permille: u32,
    /// The synthetic source's planted capture-stage hold.
    gate: Duration,
    /// L4 in process: send bench probes on an `input` DataChannel and time
    /// each one to the first decoded frame that carries it.
    probe: bool,
    /// FR-98 C1: the synthetic source emulates an event-driven source
    /// behind this capture gate, and the clock starts when a frame lands.
    compositor: Option<synthetic_backend::TestCompositor>,
    /// FR-98 C6: force the agent's `playout-delay` 0/0 switch for this
    /// session (`None` reads the environment, which is off here).
    playout_zero: Option<bool>,
}

/// What the controller saw of the `playout-delay` extension.
#[derive(Debug, Default, Clone, Copy)]
struct PlayoutSeen {
    /// The answer negotiated the extension at all.
    negotiated: bool,
    /// Packets carrying min = max = 0.
    zero: u64,
    /// Packets carrying any other value.
    other: u64,
    /// Packets without it, on a session that negotiated it.
    without: u64,
}

/// What one cell measured.
#[derive(Debug, Default)]
struct Run {
    latencies_ms: Vec<f64>,
    /// Probe round trips: send → the first decoded frame carrying it.
    probe_ms: Vec<f64>,
    decoded: usize,
    freezes: usize,
    idr: usize,
    window: Duration,
    playout: PlayoutSeen,
}

/// The `p`-quantile of `samples`, NaN when empty.
fn quantile(samples: &[f64], p: f64) -> f64 {
    if samples.is_empty() {
        return f64::NAN;
    }
    let mut v = samples.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((v.len() - 1) as f64 * p).round() as usize;
    v[i]
}

impl Run {
    fn pct(&self, p: f64) -> f64 {
        quantile(&self.latencies_ms, p)
    }

    fn probe_pct(&self, p: f64) -> f64 {
        quantile(&self.probe_ms, p)
    }

    fn fps(&self) -> f64 {
        self.decoded as f64 / self.window.as_secs_f64().max(0.001)
    }

    fn line(&self, cell: &Cell) -> String {
        let probe = if cell.probe {
            format!(
                " | L4 probes={} p50={:.1}ms p95={:.1}ms",
                self.probe_ms.len(),
                self.probe_pct(0.50),
                self.probe_pct(0.95)
            )
        } else {
            String::new()
        };
        format!(
            "L0 {:<9} decoded={:>3} fps={:>5.1} p50={:>6.1}ms p95={:>6.1}ms max={:>6.1}ms freezes={} idr={}{probe}",
            cell.name,
            self.decoded,
            self.fps(),
            self.pct(0.50),
            self.pct(0.95),
            self.pct(1.0),
            self.freezes,
            self.idr
        )
    }
}

/// Reassembles access units from RTP by sequence number, and releases one
/// the moment its marker packet completes it: no waiting for the next
/// frame's first packet. A frame whose packets never all arrive is skipped
/// once frames behind it are complete, and the caller asks for a keyframe.
#[derive(Default)]
struct Assembler {
    /// Unwrapped sequence → (timestamp, marker, payload).
    pkts: BTreeMap<u64, (u32, bool, Bytes)>,
    next: Option<u64>,
    /// The highest sequence seen, raw and unwrapped. A late packet is placed
    /// relative to it and never moves it.
    highest_raw: Option<u16>,
    highest: u64,
}

impl Assembler {
    fn unwrap_seq(&mut self, seq: u16) -> u64 {
        let Some(top) = self.highest_raw else {
            self.highest_raw = Some(seq);
            self.highest = seq as u64 + (1 << 16);
            return self.highest;
        };
        let delta = seq.wrapping_sub(top) as i16 as i64;
        let s = (self.highest as i64 + delta) as u64;
        if delta > 0 {
            self.highest_raw = Some(seq);
            self.highest = s;
        }
        s
    }

    /// Push one packet; return the access units it completed, and whether a
    /// frame had to be skipped.
    fn push(&mut self, seq: u16, ts: u32, marker: bool, payload: Bytes) -> (Vec<Vec<u8>>, bool) {
        let s = self.unwrap_seq(seq);
        if self.next.is_some_and(|n| s < n) {
            return (Vec::new(), false); // a late retransmit of a frame already released or skipped
        }
        self.pkts.insert(s, (ts, marker, payload));
        let mut out = Vec::new();
        let mut skipped = false;
        while let Some(start) = self.next.or_else(|| self.pkts.keys().next().copied()) {
            match self.complete_from(start) {
                Some((end, au)) => {
                    self.pkts.retain(|&k, _| k > end);
                    self.next = Some(end + 1);
                    out.push(au);
                }
                None => {
                    // Skip a frame only when a LATER frame is already whole:
                    // its packets are not coming back.
                    let Some(later) = self.next_frame_start_after(start) else {
                        break;
                    };
                    if self.complete_from(later).is_none() {
                        break;
                    }
                    self.pkts.retain(|&k, _| k >= later);
                    self.next = Some(later);
                    skipped = true;
                }
            }
        }
        (out, skipped)
    }

    /// The packets `start..=marker` if all are present and share a timestamp.
    fn complete_from(&self, start: u64) -> Option<(u64, Vec<u8>)> {
        let (ts0, _, _) = self.pkts.get(&start)?;
        // A fresh depacketizer per attempt: it buffers FU-A fragments, and an
        // attempt that stops at a missing packet must not leave half a NAL
        // behind for the next one.
        let mut depacketizer = H264Packet::default();
        let mut au = Vec::new();
        let mut s = start;
        loop {
            let (ts, marker, payload) = self.pkts.get(&s)?;
            if ts != ts0 {
                return None;
            }
            let part = depacketizer.depacketize(payload).ok()?;
            au.extend_from_slice(&part);
            if *marker {
                return Some((s, au));
            }
            s += 1;
        }
    }

    /// The first held packet after `start` whose predecessor is a marker or
    /// carries another timestamp: where the next frame begins.
    fn next_frame_start_after(&self, start: u64) -> Option<u64> {
        let ts0 = self.pkts.get(&start).map(|p| p.0);
        self.pkts
            .range(start + 1..)
            .find(|(k, (ts, _, _))| {
                Some(*ts) != ts0
                    && self
                        .pkts
                        .get(&(*k - 1))
                        .is_none_or(|(pts, pmarker, _)| *pmarker || pts != ts)
            })
            .map(|(k, _)| *k)
    }
}

/// A picture-loss indication for `ssrc`: what a browser sends when it
/// cannot decode, and what makes the agent cut a keyframe.
fn pli(ssrc: u32) -> [Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>; 1] {
    [Box::new(PictureLossIndication {
        sender_ssrc: 0,
        media_ssrc: ssrc,
    })]
}

/// Does this Annex-B access unit carry an IDR slice (NAL type 5)?
fn has_idr(au: &[u8]) -> bool {
    au.windows(4)
        .any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && (w[3] & 0x1F) == 5)
        || au
            .windows(5)
            .any(|w| w[0] == 0 && w[1] == 0 && w[2] == 0 && w[3] == 1 && (w[4] & 0x1F) == 5)
}

/// A tiny deterministic generator for the loss filter, so a cell drops the
/// same packets on every run.
struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

async fn vnet_pair(cell: &Cell) -> Result<(Arc<Net>, Arc<Net>, Arc<Mutex<Router>>)> {
    let wan = Arc::new(Mutex::new(Router::new(RouterConfig {
        cidr: "1.2.3.0/24".to_owned(),
        min_delay: cell.delay,
        max_jitter: cell.jitter,
        ..Default::default()
    })?));
    if cell.loss_permille > 0 {
        let permille = cell.loss_permille as u64;
        let rng = std::sync::Mutex::new(XorShift(0x9E37_79B9_7F4A_7C15));
        let agent_ip: std::net::IpAddr = AGENT_IP.parse()?;
        wan.lock()
            .await
            .add_chunk_filter(Box::new(move |c: &(dyn Chunk + Send + Sync)| {
                // Media direction only: agent → controller.
                if c.get_source_ip() != agent_ip {
                    return true;
                }
                rng.lock().unwrap().next() % 1000 >= permille
            }))
            .await;
    }
    let mut nets = Vec::new();
    for ip in [AGENT_IP, CONTROLLER_IP] {
        let net = Arc::new(Net::new(Some(NetConfig {
            static_ips: vec![ip.to_owned()],
            ..Default::default()
        })));
        let nic = net.get_nic()?;
        wan.lock().await.add_net(Arc::clone(&nic)).await?;
        nic.lock().await.set_router(Arc::clone(&wan)).await?;
        nets.push(net);
    }
    wan.lock().await.start().await?;
    let controller = nets.pop().unwrap();
    let agent = nets.pop().unwrap();
    Ok((agent, controller, wan))
}

fn ice_timeouts(s: &mut SettingEngine) {
    s.set_ice_timeouts(
        Some(Duration::from_secs(5)),
        Some(Duration::from_secs(10)),
        Some(Duration::from_millis(200)),
    );
}

async fn controller_peer(net: Arc<Net>) -> Result<Arc<RTCPeerConnection>> {
    let mut engine = MediaEngine::default();
    engine.register_default_codecs()?;
    // Offer `playout-delay` the way Chrome does, so the agent's answer
    // decides whether it is negotiated (FR-98 C6).
    roomlerd::rtp_playout::register(&mut engine)?;
    // The browser's receive-side interceptors: NACK generation and receiver
    // reports, so loss behaves as it does against Chrome.
    let registry = register_default_interceptors(Registry::new(), &mut engine)?;
    let mut setting = SettingEngine::default();
    setting.set_vnet(Some(net));
    ice_timeouts(&mut setting);
    let api = APIBuilder::new()
        .with_media_engine(engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting)
        .build();
    let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);
    pc.add_transceiver_from_kind(
        RTPCodecType::Video,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            send_encodings: vec![],
        }),
    )
    .await?;
    Ok(pc)
}

/// Run one cell end to end and return what it measured.
async fn run_cell(cell: Cell) -> Result<Run> {
    synthetic_backend::set_test_gate(cell.gate);
    synthetic_backend::set_test_compositor(cell.compositor);
    roomlerd::rtp_playout::set_test_override(cell.playout_zero);
    let (agent_net, controller_net, wan) = vnet_pair(&cell).await?;
    l0::set_setting_engine_hook(Some(Arc::new(move |s: &mut SettingEngine| {
        s.set_vnet(Some(Arc::clone(&agent_net)));
        ice_timeouts(s);
    })));

    // A probe cell needs INPUT: the agent attaches its input handler, the
    // one place a probe is read, only for a session that holds it.
    let permissions = if cell.probe {
        Permissions::VIEW | Permissions::INPUT
    } else {
        Permissions::VIEW
    };
    let (tx, mut rx) = mpsc::channel::<ClientMsg>(1024);
    let agent = AgentPeer::new(
        bson::oid::ObjectId::new(),
        &[],
        tx,
        EncoderPreference::Software,
        "h264".into(),
        None, // the RTP track, as a browser without a DC cell gets
        None,
        false,
        false,
        permissions,
        "l0-harness".into(),
        None,
        RcOverlay::default(),
    )
    .await
    .context("AgentPeer::new")?;

    let pc = controller_peer(controller_net).await?;

    // L4: the channel a bench viewer sends its probes on, created before the
    // offer with the viewer's own reliability profile (unordered, no
    // retransmits: `useRemoteControl.ts`).
    let input_dc = if cell.probe {
        Some(
            pc.create_data_channel(
                "input",
                Some(RTCDataChannelInit {
                    ordered: Some(false),
                    max_retransmits: Some(0),
                    ..Default::default()
                }),
            )
            .await?,
        )
    } else {
        None
    };
    // Probe sequence → send instant, and (decode-done instant, round trip).
    let sent: Arc<std::sync::Mutex<HashMap<u16, Instant>>> = Arc::default();
    let probe_rtts: Arc<std::sync::Mutex<Vec<(Instant, Duration)>>> = Arc::default();

    // Decoded frames: (decode-done instant, latency).
    let results: Arc<std::sync::Mutex<Vec<(Instant, Duration)>>> = Arc::default();
    let idr_count = Arc::new(AtomicU64::new(0));
    let playout: Arc<std::sync::Mutex<PlayoutSeen>> = Arc::default();
    {
        let results = Arc::clone(&results);
        let idr_count = Arc::clone(&idr_count);
        let sent = Arc::clone(&sent);
        let probe_rtts = Arc::clone(&probe_rtts);
        let playout = Arc::clone(&playout);
        let pc_weak = Arc::downgrade(&pc);
        pc.on_track(Box::new(move |track, _, _| {
            let results = Arc::clone(&results);
            let idr_count = Arc::clone(&idr_count);
            let sent = Arc::clone(&sent);
            let probe_rtts = Arc::clone(&probe_rtts);
            let playout = Arc::clone(&playout);
            let pc_weak = pc_weak.clone();
            Box::pin(async move {
                let ssrc = track.ssrc();
                // FR-98 C6: the id the answer gave `playout-delay`, if any.
                let playout_id = track
                    .params()
                    .header_extensions
                    .iter()
                    .find(|e| e.uri == roomlerd::rtp_playout::URI)
                    .map(|e| e.id as u8);
                playout.lock().unwrap().negotiated = playout_id.is_some();
                tokio::spawn(async move {
                    let mut asm = Assembler::default();
                    let mut decoder = match openh264::decoder::Decoder::new() {
                        Ok(d) => d,
                        Err(e) => {
                            eprintln!("L0: openh264 decoder: {e}");
                            return;
                        }
                    };
                    let mut last_pli = Instant::now() - Duration::from_secs(1);
                    let ask_keyframe = |last_pli: &mut Instant| {
                        if last_pli.elapsed() < Duration::from_millis(500) {
                            return None;
                        }
                        *last_pli = Instant::now();
                        pc_weak.upgrade()
                    };
                    // Ask for a keyframe at once: the first IDR may have been
                    // written before the track was bound.
                    if let Some(pc) = ask_keyframe(&mut last_pli) {
                        let _ = pc.write_rtcp(&pli(ssrc)).await;
                    }
                    while let Ok((pkt, _)) = track.read_rtp().await {
                        if let Some(id) = playout_id {
                            use webrtc::util::Unmarshal;
                            let mut seen = playout.lock().unwrap();
                            match pkt.header.get_extension(id) {
                                Some(mut raw) => {
                                    match webrtc::rtp::extension::playout_delay_extension::PlayoutDelayExtension::unmarshal(&mut raw) {
                                        Ok(p) if p.min_delay == 0 && p.max_delay == 0 => seen.zero += 1,
                                        _ => seen.other += 1,
                                    }
                                }
                                None => seen.without += 1,
                            }
                        }
                        let (aus, skipped) = asm.push(
                            pkt.header.sequence_number,
                            pkt.header.timestamp,
                            pkt.header.marker,
                            pkt.payload.clone(),
                        );
                        let mut want_keyframe = skipped;
                        for au in aus {
                            if has_idr(&au) {
                                idr_count.fetch_add(1, Ordering::Relaxed);
                            }
                            let (seq, probe) = match decoder.decode(&au) {
                                Ok(Some(pic)) => {
                                    use openh264::formats::YUVSource;
                                    let (w, h) = pic.dimensions();
                                    let (w, h) = (w as u32, h as u32);
                                    let (ys, _, _) = pic.strides();
                                    let y = pic.y();
                                    (
                                        marker::decode_luma(y, w, h, ys),
                                        if cell.probe {
                                            marker::decode_luma_at(
                                                marker::Corner::BottomRight,
                                                y,
                                                w,
                                                h,
                                                ys,
                                            )
                                        } else {
                                            None
                                        },
                                    )
                                }
                                Ok(None) => (None, None),
                                Err(_) => {
                                    want_keyframe = true;
                                    (None, None)
                                }
                            };
                            let done = Instant::now();
                            if let Some(seq) = seq
                                && let Some(captured) = clock::captured_at(seq)
                                && captured <= done
                            {
                                results.lock().unwrap().push((done, done - captured));
                            }
                            // The FIRST frame carrying a probe ends its round
                            // trip; removing it keeps the frames after it,
                            // which carry it too, from counting again.
                            if let Some(p) = probe
                                && let Some(at) = sent.lock().unwrap().remove(&p)
                            {
                                probe_rtts.lock().unwrap().push((done, done - at));
                            }
                        }
                        if want_keyframe && let Some(pc) = ask_keyframe(&mut last_pli) {
                            let _ = pc.write_rtcp(&pli(ssrc)).await;
                        }
                    }
                });
            })
        }));
    }

    // Offer with every candidate in it (vnet gathers at once), then trickle
    // the agent's candidates the other way once the answer is applied.
    let offer = pc.create_offer(None).await?;
    let mut gathered = pc.gathering_complete_promise().await;
    pc.set_local_description(offer).await?;
    let _ = gathered.recv().await;
    let offer_sdp = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("no local description"))?
        .sdp;
    let answer = agent
        .handle_offer(offer_sdp)
        .await
        .context("handle_offer")?;
    pc.set_remote_description(RTCSessionDescription::answer(answer)?)
        .await?;
    let ice_pc = Arc::clone(&pc);
    let trickle = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let ClientMsg::Ice { candidate, .. } = msg
                && let Ok(init) = serde_json::from_value::<RTCIceCandidateInit>(candidate)
            {
                let _ = ice_pc.add_ice_candidate(init).await;
            }
        }
    });

    // L4: once the channel opens, a probe every PROBE_EVERY plus jitter,
    // each stamped just before it is sent.
    let prober = input_dc.map(|dc| {
        let sent = Arc::clone(&sent);
        let (open_tx, open_rx) = tokio::sync::oneshot::channel::<()>();
        dc.on_open(Box::new(move || {
            let _ = open_tx.send(());
            Box::pin(async {})
        }));
        tokio::spawn(async move {
            if open_rx.await.is_err() {
                return;
            }
            let mut rng = XorShift(0xD1B5_4A32_D192_ED03);
            loop {
                let jitter = Duration::from_millis(rng.next() % PROBE_JITTER_MS);
                tokio::time::sleep(PROBE_EVERY + jitter).await;
                let seq = next_probe_seq();
                sent.lock().unwrap().insert(seq, Instant::now());
                if dc
                    .send_text(format!(r#"{{"t":"probe","seq":{seq}}}"#))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    });

    tokio::time::sleep(RUN).await;

    if let Some(prober) = prober {
        prober.abort();
    }
    agent.close().await;
    let _ = pc.close().await;
    trickle.abort();
    l0::set_setting_engine_hook(None);
    synthetic_backend::set_test_gate(Duration::ZERO);
    synthetic_backend::set_test_compositor(None);
    roomlerd::rtp_playout::set_test_override(None);
    let _ = wan.lock().await.stop().await;

    let samples = results.lock().unwrap().clone();
    let Some(&(first, _)) = samples.first() else {
        return Err(anyhow!("cell {}: no frame was decoded at all", cell.name));
    };
    let from = first + WARMUP;
    let kept: Vec<_> = samples.iter().filter(|(t, _)| *t >= from).collect();
    let probe_ms = probe_rtts
        .lock()
        .unwrap()
        .iter()
        .filter(|(t, _)| *t >= from)
        .map(|(_, rtt)| rtt.as_secs_f64() * 1e3)
        .collect();
    let mut run = Run {
        latencies_ms: kept.iter().map(|(_, l)| l.as_secs_f64() * 1e3).collect(),
        probe_ms,
        decoded: kept.len(),
        idr: idr_count.load(Ordering::Relaxed) as usize,
        playout: *playout.lock().unwrap(),
        ..Default::default()
    };
    if let (Some(a), Some(b)) = (kept.first(), kept.last()) {
        run.window = b.0 - a.0;
    }
    run.freezes = kept.windows(2).filter(|w| w[1].0 - w[0].0 > FREEZE).count();
    Ok(run)
}

const fn cell(name: &'static str, delay_ms: u64, loss_permille: u32, gate_ms: u64) -> Cell {
    Cell {
        name,
        delay: Duration::from_millis(delay_ms),
        jitter: Duration::ZERO,
        loss_permille,
        gate: Duration::from_millis(gate_ms),
        probe: false,
        compositor: None,
        playout_zero: None,
    }
}

/// FR-98 C1's cells: a 25 fps source behind a 25 fps gate. A whole number of
/// milliseconds per period (40), so the legacy gate's arithmetic is exact
/// and only its sleeps drift, and frames land three quarters of a period
/// ahead of the cadence the legacy gate keeps.
const GATE_FPS: u32 = 25;

fn gate_cell(name: &'static str, gate: GateMode) -> Cell {
    Cell {
        compositor: Some(synthetic_backend::TestCompositor {
            source_fps: GATE_FPS,
            target_fps: GATE_FPS,
            lead: 0.75,
            gate,
        }),
        ..cell(name, 0, 0, 0)
    }
}

/// The next probe number. Process-wide, so no cell can match a probe the
/// previous cell left painted in the agent's bottom-right marker. Starts at
/// 1: an all-zero word is a black grid.
fn next_probe_seq() -> u16 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    (NEXT.fetch_add(1, Ordering::Relaxed) & 0x0FFF) as u16
}

#[test]
fn l0_capture_to_decode_latency_sees_the_network_and_the_pipeline() {
    // SAFETY: the only test in this binary, and no thread of ours exists yet:
    // the runtime is built below. These are read when a session opens.
    // `test_env::set` clears every other spelling of each suffix first, so an
    // inherited value under a retired prefix cannot decide the run.
    unsafe {
        tunnel_core::env::test_env::set("SYNTHETIC_FRAMES", "1");
        tunnel_core::env::test_env::set("BENCH", "1");
        tunnel_core::env::test_env::set("ENCODER", "software");
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    let clean = cell("clean", 0, 0, 0);
    let delay100 = cell("delay100", 100, 0, 0);
    let gate50 = cell("gate50", 0, 0, 50);
    let lossy = Cell {
        jitter: Duration::from_millis(5),
        ..cell("lossy", 20, 30, 0)
    };
    let probe_clean = Cell {
        probe: true,
        ..cell("probe", 0, 0, 0)
    };
    let probe_delay50 = Cell {
        probe: true,
        ..cell("probe50", 50, 0, 0)
    };

    let gate_legacy = gate_cell("gate_lgcy", GateMode::Legacy);
    let gate_slot = gate_cell("gate_slot", GateMode::SlotFirst);

    let mut runs = Vec::new();
    let playout0 = Cell {
        playout_zero: Some(true),
        ..cell("playout0", 0, 0, 0)
    };
    for c in [
        clean,
        delay100,
        gate50,
        lossy,
        probe_clean,
        probe_delay50,
        gate_legacy,
        gate_slot,
        playout0,
    ] {
        let run = rt.block_on(run_cell(c)).unwrap_or_else(|e| panic!("{e:#}"));
        println!("{} | playout {:?}", run.line(&c), run.playout);
        runs.push(run);
    }
    let [
        clean_run,
        delay_run,
        gate_run,
        lossy_run,
        probe_run,
        probe50_run,
        legacy_run,
        slot_run,
        playout_run,
    ] = <[Run; 9]>::try_from(runs).unwrap();

    // FR-98 C6. Off (the default), the agent does not even register the
    // extension, so an answer to an offer that carries it negotiates nothing
    // and the session is byte-identical to before.
    assert!(
        !clean_run.playout.negotiated,
        "clean: the answer negotiated playout-delay with the switch off: {:?}",
        clean_run.playout
    );
    // On, it is negotiated and on every video packet, at 0/0.
    let p = playout_run.playout;
    assert!(
        p.negotiated && p.zero > 0 && p.other == 0 && p.without == 0,
        "playout0: want playout-delay 0/0 on every packet, saw {p:?}"
    );
    assert!(
        playout_run.fps() >= SOURCE_FPS * CLEAN_FPS_FLOOR,
        "playout0: {:.1} fps decoded: writing the extension must not cost frames",
        playout_run.fps()
    );

    // `clean`: frames flow at the source rate, through a real pipeline.
    assert!(
        clean_run.fps() >= SOURCE_FPS * CLEAN_FPS_FLOOR,
        "clean: {:.1} fps decoded, want ≥ {:.1}",
        clean_run.fps(),
        SOURCE_FPS * CLEAN_FPS_FLOOR
    );
    assert!(
        clean_run.pct(0.5) < 150.0,
        "clean: p50 {:.1} ms on a loopback link means the pipeline itself is stuck",
        clean_run.pct(0.5)
    );
    assert_eq!(
        clean_run.freezes, 0,
        "clean: a loopback link must not freeze"
    );

    // `delay100`: the metric sees the network.
    let d = delay_run.pct(0.5) - clean_run.pct(0.5);
    assert!(
        d >= 80.0,
        "delay100: p50 rose by {d:.1} ms, want ≥ 80: the metric does not see the link"
    );

    // `gate50`: the negative control. A planted 50 ms hold in the capture
    // stage must move the number, or this harness gates nothing.
    let g = gate_run.pct(0.5) - clean_run.pct(0.5);
    assert!(
        g >= 20.0,
        "gate50: p50 rose by {g:.1} ms, want ≥ 20: the metric does not see the pipeline"
    );

    // `lossy`: the session survives loss and keeps decoding.
    assert!(
        lossy_run.decoded > 0 && lossy_run.fps() >= SOURCE_FPS * 0.3,
        "lossy: {:.1} fps decoded — the session did not survive 3 % loss",
        lossy_run.fps()
    );

    // `probe`: L4 in process. A probe sent on the input channel comes back
    // painted into a decoded frame: the round trip exists end to end. About
    // 45 are measured after the warm-up; 20 is the floor.
    assert!(
        probe_run.probe_ms.len() >= 20,
        "probe: {} round trips measured, want ≥ 20 — probes are not reaching the \
         source, or not coming back in the picture",
        probe_run.probe_ms.len()
    );
    assert!(
        probe_run.probe_pct(0.5) < 200.0,
        "probe: p50 {:.1} ms on a loopback link",
        probe_run.probe_pct(0.5)
    );

    // `probe50`: 50 ms each way must cost the round trip about 100 ms. Only a
    // probe that really crosses the link in BOTH directions moves by more
    // than the one-way 50, so this is what proves the instrument measures a
    // round trip and not either half of one.
    let r = probe50_run.probe_pct(0.5) - probe_run.probe_pct(0.5);
    assert!(
        r >= 80.0,
        "probe50: the round trip rose by {r:.1} ms, want ≥ 80: the probe does not \
         cross the link both ways"
    );

    // `gate_lgcy` vs `gate_slot`: FR-98 C1. Frames that land ahead of the
    // legacy cadence wait for it, and the slot-first gate hands them out on
    // arrival. With exact arithmetic the legacy wait would be locked at 30 ms;
    // a real sleep overshoots, so the cadence drifts and the wait saws
    // through the period instead, averaging about half of it. A quarter of a
    // period is the bound, half the expected difference.
    let period_ms = 1000.0 / f64::from(GATE_FPS);
    for (run, name) in [(&legacy_run, "gate_lgcy"), (&slot_run, "gate_slot")] {
        assert!(
            run.fps() >= f64::from(GATE_FPS) * CLEAN_FPS_FLOOR,
            "{name}: {:.1} fps decoded from a {GATE_FPS} fps source",
            run.fps()
        );
    }
    let c1 = legacy_run.pct(0.5) - slot_run.pct(0.5);
    assert!(
        c1 >= period_ms / 4.0,
        "FR-98 C1: the slot-first gate cut p50 by {c1:.1} ms, want ≥ {:.1} (a quarter \
         of the {period_ms:.0} ms period): the gate does not see frames that wait in the slot",
        period_ms / 4.0
    );
}

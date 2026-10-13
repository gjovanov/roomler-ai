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
//! | `clean` | no delay | frames flow at ≥ 0.9× the source rate, p50 under a generous debug-build bound |
//! | `delay100` | +100 ms one way | p50 rises by ≥ 80 ms: the metric sees the network |
//! | `gate50` | a planted 50 ms capture-stage hold | p50 rises by ≥ 20 ms: the metric sees the pipeline. This is the negative control a gate needs; a harness that cannot be made to fail is not one |
//! | `lossy` | +20 ms, 3 % of media packets dropped | the session survives and keeps decoding; its numbers are reported |
//!
//! Absolute numbers here are from an unoptimised build on a CI runner. The
//! field numbers come from L4 in the real viewer; this file is the gate that
//! catches a regression in the pipeline, and proves it can.

#![cfg(all(feature = "openh264-encoder", feature = "synthetic-frame-source"))]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use roomler_ai_remote_control::permissions::Permissions;
use roomler_ai_remote_control::signaling::ClientMsg;
use roomlerd::capture::synthetic_backend::{self, clock, marker};
use roomlerd::encode::EncoderPreference;
use roomlerd::peer::{AgentPeer, l0};
use roomlerd::rc_overlay::RcOverlay;
use tokio::sync::{Mutex, mpsc};
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
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
}

/// What one cell measured.
#[derive(Debug, Default)]
struct Run {
    latencies_ms: Vec<f64>,
    decoded: usize,
    freezes: usize,
    idr: usize,
    window: Duration,
}

impl Run {
    fn pct(&self, p: f64) -> f64 {
        if self.latencies_ms.is_empty() {
            return f64::NAN;
        }
        let mut v = self.latencies_ms.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let i = ((v.len() - 1) as f64 * p).round() as usize;
        v[i]
    }

    fn fps(&self) -> f64 {
        self.decoded as f64 / self.window.as_secs_f64().max(0.001)
    }

    fn line(&self, cell: &Cell) -> String {
        format!(
            "L0 {:<9} decoded={:>3} fps={:>5.1} p50={:>6.1}ms p95={:>6.1}ms max={:>6.1}ms freezes={} idr={}",
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
    let (agent_net, controller_net, wan) = vnet_pair(&cell).await?;
    l0::set_setting_engine_hook(Some(Arc::new(move |s: &mut SettingEngine| {
        s.set_vnet(Some(Arc::clone(&agent_net)));
        ice_timeouts(s);
    })));

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
        Permissions::VIEW,
        "l0-harness".into(),
        None,
        RcOverlay::default(),
    )
    .await
    .context("AgentPeer::new")?;

    let pc = controller_peer(controller_net).await?;

    // Decoded frames: (decode-done instant, latency).
    let results: Arc<std::sync::Mutex<Vec<(Instant, Duration)>>> = Arc::default();
    let idr_count = Arc::new(AtomicU64::new(0));
    {
        let results = Arc::clone(&results);
        let idr_count = Arc::clone(&idr_count);
        let pc_weak = Arc::downgrade(&pc);
        pc.on_track(Box::new(move |track, _, _| {
            let results = Arc::clone(&results);
            let idr_count = Arc::clone(&idr_count);
            let pc_weak = pc_weak.clone();
            Box::pin(async move {
                let ssrc = track.ssrc();
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
                            let seq = match decoder.decode(&au) {
                                Ok(Some(pic)) => {
                                    use openh264::formats::YUVSource;
                                    let (w, h) = pic.dimensions();
                                    let (ys, _, _) = pic.strides();
                                    marker::decode_luma(pic.y(), w as u32, h as u32, ys)
                                }
                                Ok(None) => None,
                                Err(_) => {
                                    want_keyframe = true;
                                    None
                                }
                            };
                            let done = Instant::now();
                            if let Some(seq) = seq
                                && let Some(captured) = clock::captured_at(seq)
                                && captured <= done
                            {
                                results.lock().unwrap().push((done, done - captured));
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

    tokio::time::sleep(RUN).await;

    agent.close().await;
    let _ = pc.close().await;
    trickle.abort();
    l0::set_setting_engine_hook(None);
    synthetic_backend::set_test_gate(Duration::ZERO);
    let _ = wan.lock().await.stop().await;

    let samples = results.lock().unwrap().clone();
    let Some(&(first, _)) = samples.first() else {
        return Err(anyhow!("cell {}: no frame was decoded at all", cell.name));
    };
    let from = first + WARMUP;
    let kept: Vec<_> = samples.iter().filter(|(t, _)| *t >= from).collect();
    let mut run = Run {
        latencies_ms: kept.iter().map(|(_, l)| l.as_secs_f64() * 1e3).collect(),
        decoded: kept.len(),
        idr: idr_count.load(Ordering::Relaxed) as usize,
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
    }
}

#[test]
fn l0_capture_to_decode_latency_sees_the_network_and_the_pipeline() {
    // SAFETY: the only test in this binary, and no thread of ours exists yet:
    // the runtime is built below. These are read when a session opens.
    unsafe {
        std::env::set_var("ROOMLERD_SYNTHETIC_FRAMES", "1");
        std::env::set_var("ROOMLERD_BENCH", "1");
        std::env::set_var("ROOMLERD_ENCODER", "software");
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

    let mut runs = Vec::new();
    for c in [clean, delay100, gate50, lossy] {
        let run = rt.block_on(run_cell(c)).unwrap_or_else(|e| panic!("{e:#}"));
        println!("{}", run.line(&c));
        runs.push(run);
    }
    let [clean_run, delay_run, gate_run, lossy_run] = <[Run; 4]>::try_from(runs).unwrap();

    // `clean`: frames flow at the source rate, through a real pipeline.
    assert!(
        clean_run.fps() >= SOURCE_FPS * 0.9,
        "clean: {:.1} fps decoded, want ≥ {:.1}",
        clean_run.fps(),
        SOURCE_FPS * 0.9
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
}

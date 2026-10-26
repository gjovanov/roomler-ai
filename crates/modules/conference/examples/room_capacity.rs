// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-115 O4 — what one meeting room carries.
//!
//! A meeting is one mediasoup router on one worker thread
//! (`media::room_manager::create_room`, `media::worker_pool`): nothing pipes a
//! room across workers, so a room is as big as one worker thread can forward.
//! This bench drives that worker and router with the production settings
//! ([`worker_settings`], [`media_codecs`]) and with the media the real client
//! sends (`ui/src/stores/conference.ts`: one VP8 camera stream with no simulcast
//! and one Opus stream per participant, every participant consuming every other
//! one's), and reports at each room size N:
//!
//! - `cpu`: the worker THREAD's CPU, from `/proc/self/task/<tid>/stat` (the
//!   worker's own `get_resource_usage` is process-wide when it runs as a thread,
//!   so it would count this bench as well);
//! - `delivered`: the packets that reached the receivers over the packets that
//!   should have (each generated packet once per other participant).
//!
//! Each synthetic participant sends RTP into its own `PlainTransport` (comedia)
//! and receives on a second one with SRTP on, so the per-consumer encryption,
//! the N² half of the work, is paid as on a `WebRtcTransport`. Not modelled, and
//! each would make the real cost HIGHER, so a capacity measured here is an upper
//! bound: ICE consent checks and DTLS, receiver RTCP (receiver reports,
//! transport-cc feedback, NACKs) and retransmissions, and decrypting the
//! senders' SRTP.
//!
//! ```text
//! cargo run --release -p roomler-ai-mod-conference --example room_capacity -- \
//!     --steps 5,10,15,20,25,30,40,50 --video-kbps 1500
//! ```
//!
//! Flags:
//! - `--steps`: the room sizes to measure, ascending (participants are only added);
//! - `--video-kbps`: per camera, 0 for audio only (default 1500);
//! - `--audio-kbps` (default 40), `--fps` (default 30), `--packet-bytes`: the
//!   video RTP payload (default 1100);
//! - `--warmup-s` (default 5) and `--window-s` (default 15) per step;
//! - `--log-level debug|warn|error|none`: overrides the production worker's level
//!   (left out, the worker runs exactly as production runs it);
//! - `--no-liburing`: the worker without io_uring, which it otherwise uses
//!   wherever the kernel allows it (a container's seccomp profile may not);
//! - `--no-srtp`, `--sinks`: receiving threads (default 8);
//! - `--stop-below`: stop after the first step delivering under this percentage
//!   (default 90).
//!
//! The CPU figures need Linux (procfs); elsewhere they read `-`.

use base64::Engine;
use mediasoup::prelude::*;
use mediasoup::types::srtp_parameters::SrtpParameters;
use mediasoup::worker::WorkerLogLevel;
use rand::RngCore;
use roomler_ai_config::MediasoupSettings;
use roomler_ai_mod_conference::media::room_manager::media_codecs;
use roomler_ai_mod_conference::media::worker_pool::worker_settings;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::num::NonZero;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Opus at 20 ms per packet.
const AUDIO_PERIOD: Duration = Duration::from_millis(20);

struct Args {
    steps: Vec<usize>,
    video_kbps: u32,
    audio_kbps: u32,
    fps: u32,
    packet_bytes: usize,
    warmup: Duration,
    window: Duration,
    log_level: Option<WorkerLogLevel>,
    liburing: bool,
    srtp: bool,
    sinks: usize,
    stop_below: f64,
}

fn next_value(it: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    it.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}

fn parse_args() -> anyhow::Result<Args> {
    let mut a = Args {
        steps: vec![5, 10, 15, 20, 25, 30, 40, 50],
        video_kbps: 1500,
        audio_kbps: 40,
        fps: 30,
        packet_bytes: 1100,
        warmup: Duration::from_secs(5),
        window: Duration::from_secs(15),
        log_level: None,
        liburing: true,
        srtp: true,
        sinks: 8,
        stop_below: 90.0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--steps" => {
                a.steps = next_value(&mut it, &flag)?
                    .split(',')
                    .map(|s| s.trim().parse())
                    .collect::<Result<_, _>>()?
            }
            "--video-kbps" => a.video_kbps = next_value(&mut it, &flag)?.parse()?,
            "--audio-kbps" => a.audio_kbps = next_value(&mut it, &flag)?.parse()?,
            "--fps" => a.fps = next_value(&mut it, &flag)?.parse()?,
            "--packet-bytes" => a.packet_bytes = next_value(&mut it, &flag)?.parse()?,
            "--warmup-s" => {
                a.warmup = Duration::from_secs(next_value(&mut it, &flag)?.parse()?)
            }
            "--window-s" => {
                a.window = Duration::from_secs(next_value(&mut it, &flag)?.parse()?)
            }
            "--log-level" => {
                a.log_level = Some(match next_value(&mut it, &flag)?.as_str() {
                    "debug" => WorkerLogLevel::Debug,
                    "warn" => WorkerLogLevel::Warn,
                    "error" => WorkerLogLevel::Error,
                    "none" => WorkerLogLevel::None,
                    other => anyhow::bail!("--log-level: unknown level {other}"),
                })
            }
            "--no-liburing" => a.liburing = false,
            "--no-srtp" => a.srtp = false,
            "--sinks" => a.sinks = next_value(&mut it, &flag)?.parse()?,
            "--stop-below" => a.stop_below = next_value(&mut it, &flag)?.parse()?,
            other => anyhow::bail!("unknown flag {other} (see the doc comment)"),
        }
    }
    anyhow::ensure!(
        !a.steps.is_empty() && a.steps[0] >= 2 && a.steps.windows(2).all(|w| w[0] < w[1]),
        "--steps must ascend from at least 2"
    );
    anyhow::ensure!((1..=60).contains(&a.fps), "--fps must be 1..=60");
    anyhow::ensure!(
        (200..=1200).contains(&a.packet_bytes),
        "--packet-bytes must be 200..=1200"
    );
    anyhow::ensure!(a.sinks >= 1, "--sinks must be at least 1");
    Ok(a)
}

/// The media one synthetic participant sends, derived from the flags.
#[derive(Clone, Copy)]
struct Shape {
    audio_payload: usize,
    /// Video RTP packets per frame; 0 = no camera.
    packets_per_frame: usize,
    packet_bytes: usize,
    fps: u32,
    frame_period: Duration,
}

impl Shape {
    fn new(a: &Args) -> Self {
        let frame_bytes = f64::from(a.video_kbps) * 1000.0 / 8.0 / f64::from(a.fps);
        let packets_per_frame = if a.video_kbps == 0 {
            0
        } else {
            ((frame_bytes / a.packet_bytes as f64).round() as usize).max(1)
        };
        Self {
            audio_payload: ((a.audio_kbps as usize * 1000 / 8) / 50).max(20),
            packets_per_frame,
            packet_bytes: a.packet_bytes,
            fps: a.fps,
            frame_period: Duration::from_secs_f64(1.0 / f64::from(a.fps)),
        }
    }

    fn video_kbps(&self) -> f64 {
        (self.packets_per_frame * self.packet_bytes * 8) as f64 * f64::from(self.fps) / 1000.0
    }

    fn packets_per_second(&self) -> f64 {
        50.0 + (self.packets_per_frame as f64 * f64::from(self.fps))
    }
}

#[derive(Default)]
struct Counter {
    packets: AtomicU64,
    bytes: AtomicU64,
}

impl Counter {
    fn add(&self, bytes: usize) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn read(&self) -> (u64, u64) {
        (
            self.packets.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
}

/// A receiving socket on its own thread, counting the RTP the worker sends it.
/// The transports multiplex RTCP onto the same port, and an RTCP packet's second
/// byte (its type, 200..=206, in the clear even under SRTCP) is one no RTP packet
/// of ours can have (payload types 96 and 111, with or without the marker), so
/// the worker's sender reports are told apart rather than counted as delivered.
fn spawn_sink(received: Arc<Counter>) -> anyhow::Result<u16> {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = socket.local_addr()?.port();
    std::thread::Builder::new()
        .name("o4-sink".into())
        .spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                match socket.recv(&mut buf) {
                    Ok(n) if n >= 2 && !(200..=206).contains(&buf[1]) => received.add(n),
                    Ok(_) => {}
                    Err(_) => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        })?;
    Ok(port)
}

/// One synthetic sender: where its RTP goes and the state of its two streams.
struct Target {
    addr: SocketAddr,
    audio_ssrc: u32,
    video_ssrc: Option<u32>,
    audio_seq: u16,
    audio_ts: u32,
    video_seq: u16,
    video_ts: u32,
    picture_id: u16,
    frame: u64,
    next_audio: Instant,
    next_frame: Instant,
}

impl Target {
    fn new(addr: SocketAddr, audio_ssrc: u32, video_ssrc: Option<u32>, shape: &Shape) -> Self {
        // Spread the newcomer's packets over the period, so N senders do not
        // fire in lockstep.
        let now = Instant::now();
        let jitter = |period: Duration| period.mul_f64(rand::random::<f64>());
        Self {
            addr,
            audio_ssrc,
            video_ssrc,
            audio_seq: rand::random(),
            audio_ts: rand::random(),
            video_seq: rand::random(),
            video_ts: rand::random(),
            picture_id: rand::random::<u16>() & 0x7fff,
            frame: 0,
            next_audio: now + jitter(AUDIO_PERIOD),
            next_frame: now + jitter(shape.frame_period),
        }
    }
}

fn rtp_header(buf: &mut Vec<u8>, payload_type: u8, marker: bool, seq: u16, ts: u32, ssrc: u32) {
    buf.clear();
    buf.push(0x80);
    buf.push((u8::from(marker) << 7) | payload_type);
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(&ts.to_be_bytes());
    buf.extend_from_slice(&ssrc.to_be_bytes());
}

/// The sender thread: Opus every 20 ms and one VP8 frame every `1/fps` s for
/// each participant, with a key frame once a second. A sender that falls
/// behind skips rather than bursts, and counts it in `late`.
fn spawn_generator(
    targets: Arc<Mutex<Vec<Target>>>,
    shape: Shape,
    sent: Arc<Counter>,
    late: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    std::thread::Builder::new()
        .name("o4-generator".into())
        .spawn(move || {
            let mut pkt = Vec::with_capacity(1500);
            loop {
                let now = Instant::now();
                {
                    let mut targets = targets.lock().unwrap_or_else(|e| e.into_inner());
                    for t in targets.iter_mut() {
                        if t.next_audio <= now {
                            rtp_header(&mut pkt, 111, false, t.audio_seq, t.audio_ts, t.audio_ssrc);
                            // An Opus TOC byte (CELT fullband, 20 ms), then filler.
                            pkt.push(0xfc);
                            pkt.resize(12 + shape.audio_payload, 0x55);
                            if socket.send_to(&pkt, t.addr).is_ok() {
                                sent.add(pkt.len());
                            }
                            t.audio_seq = t.audio_seq.wrapping_add(1);
                            t.audio_ts = t.audio_ts.wrapping_add(960);
                            t.next_audio += AUDIO_PERIOD;
                            if t.next_audio < now {
                                late.fetch_add(1, Ordering::Relaxed);
                                t.next_audio = now + AUDIO_PERIOD;
                            }
                        }
                        let Some(video_ssrc) = t.video_ssrc else {
                            continue;
                        };
                        if t.next_frame <= now {
                            let key = t.frame % u64::from(shape.fps) == 0;
                            for j in 0..shape.packets_per_frame {
                                let last = j + 1 == shape.packets_per_frame;
                                rtp_header(&mut pkt, 96, last, t.video_seq, t.video_ts, video_ssrc);
                                // The VP8 payload descriptor as Chrome sends it: X=1
                                // (mediasoup drops a packet without it), S on the
                                // frame's first packet, I=1 with a 15-bit PictureID.
                                pkt.push(if j == 0 { 0x90 } else { 0x80 });
                                pkt.push(0x80);
                                pkt.push(0x80 | ((t.picture_id >> 8) as u8 & 0x7f));
                                pkt.push(t.picture_id as u8);
                                if j == 0 {
                                    // The frame tag: bit 0 clear = key frame. A key
                                    // frame carries the start code and 640x480.
                                    pkt.extend_from_slice(&[if key { 0x10 } else { 0x11 }, 0, 0]);
                                    if key {
                                        pkt.extend_from_slice(&[0x9d, 0x01, 0x2a, 0x80, 0x02, 0xe0, 0x01]);
                                    }
                                }
                                pkt.resize(12 + shape.packet_bytes, 0xaa);
                                if socket.send_to(&pkt, t.addr).is_ok() {
                                    sent.add(pkt.len());
                                }
                                t.video_seq = t.video_seq.wrapping_add(1);
                            }
                            t.video_ts = t.video_ts.wrapping_add(90_000 / shape.fps);
                            t.picture_id = (t.picture_id + 1) & 0x7fff;
                            t.frame += 1;
                            t.next_frame += shape.frame_period;
                            if t.next_frame < now {
                                late.fetch_add(1, Ordering::Relaxed);
                                t.next_frame = now + shape.frame_period;
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        })?;
    Ok(())
}

fn audio_parameters(ssrc: u32, cname: &str) -> RtpParameters {
    RtpParameters {
        codecs: vec![RtpCodecParameters::Audio {
            mime_type: MimeTypeAudio::Opus,
            payload_type: 111,
            clock_rate: NonZero::new(48_000).expect("non-zero"),
            channels: NonZero::new(2).expect("non-zero"),
            parameters: RtpCodecParametersParameters::default(),
            rtcp_feedback: vec![RtcpFeedback::TransportCc],
        }],
        encodings: vec![RtpEncodingParameters {
            ssrc: Some(ssrc),
            ..RtpEncodingParameters::default()
        }],
        rtcp: RtcpParameters {
            cname: Some(cname.to_owned()),
            ..RtcpParameters::default()
        },
        ..RtpParameters::default()
    }
}

fn video_parameters(ssrc: u32, cname: &str) -> RtpParameters {
    RtpParameters {
        codecs: vec![RtpCodecParameters::Video {
            mime_type: MimeTypeVideo::Vp8,
            payload_type: 96,
            clock_rate: NonZero::new(90_000).expect("non-zero"),
            parameters: RtpCodecParametersParameters::default(),
            rtcp_feedback: vec![
                RtcpFeedback::Nack,
                RtcpFeedback::NackPli,
                RtcpFeedback::CcmFir,
                RtcpFeedback::GoogRemb,
                RtcpFeedback::TransportCc,
            ],
        }],
        encodings: vec![RtpEncodingParameters {
            ssrc: Some(ssrc),
            ..RtpEncodingParameters::default()
        }],
        rtcp: RtcpParameters {
            cname: Some(cname.to_owned()),
            ..RtcpParameters::default()
        },
        ..RtpParameters::default()
    }
}

/// SRTP keys made at run time, never a literal: AES_CM_128_HMAC_SHA1_80 takes
/// a 16-byte key and a 14-byte salt.
fn random_srtp() -> SrtpParameters {
    let mut key = [0u8; 30];
    rand::rng().fill_bytes(&mut key);
    SrtpParameters {
        crypto_suite: SrtpCryptoSuite::AesCm128HmacSha180,
        key_base64: base64::engine::general_purpose::STANDARD.encode(key),
    }
}

fn loopback() -> ListenInfo {
    ListenInfo {
        protocol: Protocol::Udp,
        ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        announced_address: None,
        port: None,
        port_range: None,
        flags: None,
        send_buffer_size: None,
        recv_buffer_size: None,
        expose_internal_ip: false,
    }
}

struct Participant {
    _send: PlainTransport,
    recv: PlainTransport,
    producers: Vec<Producer>,
    /// Held so they stay open: dropping a consumer closes it.
    _consumers: Vec<Consumer>,
}

async fn consume(
    transport: &PlainTransport,
    producer: &Producer,
    caps: &RtpCapabilities,
) -> anyhow::Result<Consumer> {
    let consumer = transport
        .consume(ConsumerOptions::new(producer.id(), caps.clone()))
        .await?;
    if consumer.paused() {
        consumer.resume().await?;
    }
    Ok(consumer)
}

struct Room {
    router: Router,
    caps: RtpCapabilities,
    participants: Vec<Participant>,
    sink_ports: Vec<u16>,
    srtp: bool,
    shape: Shape,
    targets: Arc<Mutex<Vec<Target>>>,
}

impl Room {
    /// A participant joins: it produces, receives everyone already there, and
    /// everyone already there receives it.
    async fn join(&mut self) -> anyhow::Result<()> {
        let idx = self.participants.len();
        let cname = format!("o4-{idx}");
        let mut send_options = PlainTransportOptions::new(loopback());
        send_options.comedia = true;
        let send = self.router.create_plain_transport(send_options).await?;
        let tuple = send.tuple();
        let send_addr = SocketAddr::new(tuple.local_address().parse()?, tuple.local_port());

        let audio_ssrc: u32 = rand::random();
        let mut producers = vec![
            send.produce(ProducerOptions::new(
                MediaKind::Audio,
                audio_parameters(audio_ssrc, &cname),
            ))
            .await?,
        ];
        let video_ssrc = if self.shape.packets_per_frame > 0 {
            let ssrc: u32 = rand::random();
            producers.push(
                send.produce(ProducerOptions::new(
                    MediaKind::Video,
                    video_parameters(ssrc, &cname),
                ))
                .await?,
            );
            Some(ssrc)
        } else {
            None
        };

        let mut recv_options = PlainTransportOptions::new(loopback());
        recv_options.enable_srtp = self.srtp;
        recv_options.srtp_crypto_suite = SrtpCryptoSuite::AesCm128HmacSha180;
        let recv = self.router.create_plain_transport(recv_options).await?;
        recv.connect(PlainTransportRemoteParameters {
            ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            port: Some(self.sink_ports[idx % self.sink_ports.len()]),
            rtcp_port: None,
            srtp_parameters: self.srtp.then(random_srtp),
        })
        .await?;

        let mut consumers = Vec::new();
        for other in &self.participants {
            for producer in &other.producers {
                consumers.push(consume(&recv, producer, &self.caps).await?);
            }
        }
        for other in &mut self.participants {
            for producer in &producers {
                let consumer = consume(&other.recv, producer, &self.caps).await?;
                other._consumers.push(consumer);
            }
        }
        self.participants.push(Participant {
            _send: send,
            recv,
            producers,
            _consumers: consumers,
        });
        self.targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Target::new(send_addr, audio_ssrc, video_ssrc, &self.shape));
        Ok(())
    }

    /// The RTP bytes the worker says it sent to the receivers, all transports.
    async fn forwarded_bytes(&self) -> u64 {
        let mut total = 0;
        for p in &self.participants {
            if let Ok(stats) = p.recv.get_stats().await {
                total += stats.iter().map(|s| s.rtp_bytes_sent).sum::<u64>();
            }
        }
        total
    }
}

/// The worker thread: mediasoup names it `mediasoup-worker-<id>`, which the
/// kernel truncates to 15 bytes, and this process starts exactly one.
#[cfg(target_os = "linux")]
fn worker_tid() -> Option<u32> {
    let mut found = None;
    for entry in std::fs::read_dir("/proc/self/task").ok()?.flatten() {
        let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        if comm.trim_end().starts_with("mediasoup-worke") {
            found = entry.file_name().to_str().and_then(|s| s.parse().ok());
        }
    }
    found
}

#[cfg(not(target_os = "linux"))]
fn worker_tid() -> Option<u32> {
    None
}

/// CPU seconds (user + system) from a procfs `stat` line. Field 3, the state,
/// follows the parenthesised command; utime and stime are fields 14 and 15,
/// in USER_HZ, which is 100 on Linux.
#[cfg(target_os = "linux")]
fn cpu_seconds(path: &str) -> Option<f64> {
    let stat = std::fs::read_to_string(path).ok()?;
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) / 100.0)
}

#[cfg(not(target_os = "linux"))]
fn cpu_seconds(_path: &str) -> Option<f64> {
    None
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|m| m.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

struct Snapshot {
    at: Instant,
    worker_cpu: Option<f64>,
    process_cpu: Option<f64>,
    sent: (u64, u64),
    received: (u64, u64),
    forwarded: u64,
    late: u64,
}

fn pct(x: Option<f64>) -> String {
    x.map_or_else(|| "-".to_owned(), |v| format!("{v:.1}"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = parse_args()?;
    let shape = Shape::new(&args);

    // Production's port range (`crates/config/src/settings.rs`); only the worker
    // settings are read from this.
    let production = MediasoupSettings {
        num_workers: 1,
        listen_ip: "127.0.0.1".to_owned(),
        announced_ip: String::new(),
        announced_ip_map: None,
        rtc_min_port: 40000,
        rtc_max_port: 49999,
    };
    let mut settings = worker_settings(&production);
    if let Some(level) = args.log_level {
        settings.log_level = level;
    }
    if !args.liburing {
        settings.enable_liburing = false;
    }
    let level = format!("{:?}", settings.log_level);
    let liburing = settings.enable_liburing;
    let worker = WorkerManager::new().create_worker(settings).await?;
    let router = worker.create_router(RouterOptions::new(media_codecs())).await?;
    // `RtpCapabilitiesFinalized` → `RtpCapabilities`, as `create_rtp_tap` does.
    let caps: RtpCapabilities =
        serde_json::from_value(serde_json::to_value(router.rtp_capabilities())?)?;
    let tid = worker_tid();

    let sent = Arc::new(Counter::default());
    let received = Arc::new(Counter::default());
    let late = Arc::new(AtomicU64::new(0));
    let sink_ports = (0..args.sinks)
        .map(|_| spawn_sink(received.clone()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let targets = Arc::new(Mutex::new(Vec::new()));
    spawn_generator(targets.clone(), shape, sent.clone(), late.clone())?;

    println!(
        "FR-115 O4 room capacity — cpu {} · worker log level {level} · io_uring {liburing} · srtp {} · per participant: \
         opus {} B/20 ms + vp8 {:.0} kbit/s ({} x {} B at {} fps) = {:.0} pkt/s · worker thread {}",
        cpu_model(),
        args.srtp,
        shape.audio_payload,
        shape.video_kbps(),
        shape.packets_per_frame,
        shape.packet_bytes,
        shape.fps,
        shape.packets_per_second(),
        tid.map_or_else(|| "not found".to_owned(), |t| t.to_string()),
    );
    println!(
        "{:>4} {:>9} {:>9} {:>10} {:>9} {:>9} {:>8} {:>10} {:>9} {:>6}",
        "N", "consumers", "in_pkt/s", "out_pkt/s", "out_Mb/s", "fwd_Mb/s", "cpu_%", "delivered", "bench_%", "late"
    );

    let streams = if shape.packets_per_frame > 0 { 2 } else { 1 };
    let mut room = Room {
        router,
        caps,
        participants: Vec::new(),
        sink_ports,
        srtp: args.srtp,
        shape,
        targets,
    };
    let snapshot = |room_forwarded: u64| Snapshot {
        at: Instant::now(),
        worker_cpu: tid.and_then(|t| cpu_seconds(&format!("/proc/self/task/{t}/stat"))),
        process_cpu: cpu_seconds("/proc/self/stat"),
        sent: sent.read(),
        received: received.read(),
        forwarded: room_forwarded,
        late: late.load(Ordering::Relaxed),
    };

    for &n in &args.steps {
        while room.participants.len() < n {
            room.join().await?;
        }
        tokio::time::sleep(args.warmup).await;
        let a = snapshot(room.forwarded_bytes().await);
        tokio::time::sleep(args.window).await;
        let b = snapshot(room.forwarded_bytes().await);

        let secs = b.at.duration_since(a.at).as_secs_f64();
        let in_pkts = (b.sent.0 - a.sent.0) as f64;
        let expected = in_pkts * (n - 1) as f64;
        let out_pkts = (b.received.0 - a.received.0) as f64;
        let delivered = if expected > 0.0 { 100.0 * out_pkts / expected } else { 0.0 };
        let worker = b.worker_cpu.zip(a.worker_cpu).map(|(b, a)| 100.0 * (b - a) / secs);
        let bench = b
            .process_cpu
            .zip(a.process_cpu)
            .map(|(b, a)| 100.0 * (b - a) / secs)
            .zip(worker)
            .map(|(p, w)| p - w);
        let consumers = n * (n - 1) * streams;
        println!(
            "{:>4} {:>9} {:>9.0} {:>10.0} {:>9.1} {:>9.1} {:>8} {:>9.1}% {:>9} {:>6}",
            n,
            consumers,
            in_pkts / secs,
            out_pkts / secs,
            (b.received.1 - a.received.1) as f64 * 8.0 / secs / 1e6,
            b.forwarded.saturating_sub(a.forwarded) as f64 * 8.0 / secs / 1e6,
            pct(worker),
            delivered,
            pct(bench),
            b.late - a.late,
        );
        println!(
            "O4 {}",
            serde_json::json!({
                "n": n,
                "consumers": consumers,
                "in_pps": in_pkts / secs,
                "out_pps": out_pkts / secs,
                "cpu_pct": worker,
                "delivered_pct": delivered,
                "video_kbps": shape.video_kbps(),
                "log_level": level,
                "liburing": liburing,
                "srtp": args.srtp,
            })
        );
        if delivered < args.stop_below {
            println!("delivery fell below {}% at N = {n}: stopping", args.stop_below);
            break;
        }
    }
    Ok(())
}

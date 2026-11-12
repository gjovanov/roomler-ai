// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3716 — a peer whose media path is dead when it closes must still free
//! everything it holds.
//!
//! A session's video rides a data channel (the VP9-444 and FFmpeg DC pumps),
//! written by a send task of its own. When the path dies under it (the viewer's
//! tab closed, a network drop the viewer redialled around, a host asleep), no
//! acknowledgement comes back, SCTP's send queue fills, and the send task parks
//! in flow control. In the field the peer's close then overran its budget and
//! was dropped, and the peer's sockets, its ICE agent and its SCTP association
//! (still retransmitting into the dead link once a minute) lived until the
//! daemon exited.
//!
//! Two plain webrtc-rs peers on a vnet stand in for the agent and the viewer:
//! the stall is below the agent's own code, so the test drives the same
//! `RTCPeerConnection::close` the agent's sessions do, through the agent's own
//! budget. Every assertion is black-box: how long the close took, whether the
//! parked writer was let go, whether the closed peer still transmits, and the
//! tasks alive on a runtime of the test's own.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::Mutex;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::util::vnet::chunk::Chunk;
use webrtc::util::vnet::net::{Net, NetConfig};
use webrtc::util::vnet::router::{Router, RouterConfig};

use crate::peer_close::close as close_peer_connection;
use crate::signaling::close_within_budget;

const AGENT_IP: &str = "1.2.3.4";
const VIEWER_IP: &str = "1.2.3.5";

/// A dead path's close has nothing to wait for. The bound is far above the
/// milliseconds it takes and far below the 5 s budget it overran.
const DEAD_PATH_CLOSE: Duration = Duration::from_secs(2);

/// How long a closed peer's tasks get to finish. Everything they wait on is
/// local and already signalled, so this is slack, not a timing assumption:
/// alone they settle within milliseconds, but under the whole lib suite in
/// parallel 5 s was once too short, and a real leak never settles at all.
const SETTLE: Duration = Duration::from_secs(20);

/// A runtime of the test's own: its task count is the two peers' alone.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

fn alive() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

/// Waits for the runtime to come back down to `baseline` tasks; returns the
/// count it ended on.
async fn settle_to(baseline: usize) -> usize {
    let deadline = Instant::now() + SETTLE;
    loop {
        let now = alive();
        if now <= baseline || Instant::now() >= deadline {
            return now;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The wire between the two peers: it can be cut, and it counts what the
/// agent puts on it, cut or not.
#[derive(Default)]
struct Wire {
    cut: AtomicBool,
    from_agent: AtomicU64,
}

async fn wan(wire: &Arc<Wire>) -> (Arc<Net>, Arc<Net>) {
    let wan = Arc::new(Mutex::new(
        Router::new(RouterConfig {
            cidr: "1.2.3.0/24".to_owned(),
            ..Default::default()
        })
        .expect("router"),
    ));
    let tap = Arc::clone(wire);
    let agent: std::net::IpAddr = AGENT_IP.parse().expect("ip");
    wan.lock()
        .await
        .add_chunk_filter(Box::new(move |c: &(dyn Chunk + Send + Sync)| {
            if c.get_source_ip() == agent {
                tap.from_agent.fetch_add(1, Ordering::SeqCst);
            }
            !tap.cut.load(Ordering::SeqCst)
        }))
        .await;
    let mut nets = Vec::new();
    for ip in [AGENT_IP, VIEWER_IP] {
        let net = Arc::new(Net::new(Some(NetConfig {
            static_ips: vec![ip.to_owned()],
            ..Default::default()
        })));
        let nic = net.get_nic().expect("nic");
        wan.lock()
            .await
            .add_net(Arc::clone(&nic))
            .await
            .expect("add");
        nic.lock()
            .await
            .set_router(Arc::clone(&wan))
            .await
            .expect("router");
        nets.push(net);
    }
    wan.lock().await.start().await.expect("start");
    let viewer = nets.pop().expect("viewer net");
    let agent = nets.pop().expect("agent net");
    (agent, viewer)
}

/// A peer with the interceptors a session has (NACK, reports, TWCC), so their
/// close is part of what is timed.
async fn peer(net: Arc<Net>) -> Arc<RTCPeerConnection> {
    let mut engine = MediaEngine::default();
    engine.register_default_codecs().expect("codecs");
    let registry =
        register_default_interceptors(Registry::new(), &mut engine).expect("interceptors");
    let mut setting = SettingEngine::default();
    setting.set_vnet(Some(net));
    let api = APIBuilder::new()
        .with_media_engine(engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting)
        .build();
    Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .expect("peer"),
    )
}

/// The viewer offers with a data channel, as the browser does, and the agent
/// answers. Returns both ends once the channel is open: the agent's, the viewer's.
async fn connect(
    viewer: &Arc<RTCPeerConnection>,
    agent: &Arc<RTCPeerConnection>,
) -> (Arc<RTCDataChannel>, Arc<RTCDataChannel>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Arc<RTCDataChannel>>(1);
    agent.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        let tx = tx.clone();
        Box::pin(async move {
            let opened = Arc::clone(&dc);
            dc.on_open(Box::new(move || {
                Box::pin(async move {
                    let _ = tx.send(opened).await;
                })
            }));
        })
    }));
    let viewer_end = viewer
        .create_data_channel("video", None)
        .await
        .expect("data channel");
    let offer = viewer.create_offer(None).await.expect("offer");
    let mut gathered = viewer.gathering_complete_promise().await;
    viewer
        .set_local_description(offer)
        .await
        .expect("offer set");
    let _ = gathered.recv().await;
    let offer = viewer.local_description().await.expect("offer");
    agent.set_remote_description(offer).await.expect("offer in");
    let answer = agent.create_answer(None).await.expect("answer");
    let mut gathered = agent.gathering_complete_promise().await;
    agent
        .set_local_description(answer)
        .await
        .expect("answer set");
    let _ = gathered.recv().await;
    let answer = agent.local_description().await.expect("answer");
    viewer
        .set_remote_description(answer)
        .await
        .expect("answer in");
    let agent_end = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("the channel opens")
        .expect("channel");
    (agent_end, viewer_end)
}

/// The video send task's shape: 16 KiB messages, back to back, until a send
/// fails. Returns the task and the instant of its last completed send.
fn video_writer(
    dc: &Arc<RTCDataChannel>,
) -> (tokio::task::JoinHandle<()>, Arc<std::sync::Mutex<Instant>>) {
    let last_sent = Arc::new(std::sync::Mutex::new(Instant::now()));
    let task = {
        let dc = Arc::clone(dc);
        let last_sent = Arc::clone(&last_sent);
        tokio::spawn(async move {
            let chunk = Bytes::from(vec![0x5a_u8; 16 * 1024]);
            while dc.send(&chunk).await.is_ok() {
                *last_sent.lock().expect("clock") = Instant::now();
            }
        })
    };
    (task, last_sent)
}

fn states(agent: &RTCPeerConnection, dc: &RTCDataChannel) -> String {
    let sctp = agent.sctp();
    let dtls = sctp.transport();
    format!(
        "data channel {}, sctp {}, dtls {}, ice {}",
        dc.ready_state(),
        sctp.state(),
        dtls.state(),
        dtls.ice_transport().state()
    )
}

/// `RTCPeerConnection::close` inline under the agent's budget, which drops it
/// on an overrun: the close as the agent ran it before #3716, without the
/// background safety net (`peer_close::close`), so these two tests hold
/// the root cause itself.
async fn close_like_the_agent(agent: &RTCPeerConnection) {
    let session = bson::oid::ObjectId::new();
    close_within_budget(
        async {
            let _ = agent.close().await;
        },
        session,
        "terminate",
    )
    .await;
}

#[test]
fn a_dead_path_under_a_data_channel_writer_does_not_stall_the_close() {
    runtime().block_on(async {
        let wire = Arc::new(Wire::default());
        let (agent_net, viewer_net) = wan(&wire).await;
        let baseline = alive();
        let viewer = peer(viewer_net).await;
        let agent = peer(agent_net).await;
        let (dc, _) = connect(&viewer, &agent).await;
        let (writer, last_sent) = video_writer(&dc);
        tokio::time::sleep(Duration::from_millis(300)).await;

        // The path dies: nothing either peer sends arrives any more.
        wire.cut.store(true, Ordering::SeqCst);
        let parked_by = Instant::now() + Duration::from_secs(15);
        while last_sent.lock().expect("clock").elapsed() < Duration::from_secs(1) {
            assert!(
                Instant::now() < parked_by,
                "the writer never parked: this run proves nothing"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // The viewer's tab closes; its goodbye is lost on the dead wire.
        viewer.close().await.expect("viewer close");

        let started = Instant::now();
        close_like_the_agent(&agent).await;
        let took = started.elapsed();
        assert!(
            took < DEAD_PATH_CLOSE,
            "the close of a peer whose path died under a data-channel writer took {took:?}: \
             it overran its budget and was dropped (#3716), stuck at {}",
            states(&agent, &dc)
        );

        let released = tokio::time::timeout(SETTLE, writer).await.is_ok();
        assert!(
            released,
            "the writer parked in SCTP flow control was never let go"
        );

        let before = wire.from_agent.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(2)).await;
        let after = wire.from_agent.load(Ordering::SeqCst) - before;
        assert_eq!(
            after, 0,
            "the closed peer still put {after} packets on the wire in 2 s"
        );

        // The session drops its peer after the close, as `AgentPeer` does.
        drop((dc, agent, viewer));
        let left = settle_to(baseline).await;
        assert!(
            left <= baseline,
            "{left} tasks alive after both closes, {baseline} before the peers: \
             what the close did not reach stayed alive"
        );
    });
}

#[test]
fn a_healthy_close_still_takes_milliseconds() {
    runtime().block_on(async {
        let wire = Arc::new(Wire::default());
        let (agent_net, viewer_net) = wan(&wire).await;
        let baseline = alive();
        let viewer = peer(viewer_net).await;
        let agent = peer(agent_net).await;
        let (dc, _) = connect(&viewer, &agent).await;
        let (writer, _last_sent) = video_writer(&dc);
        tokio::time::sleep(Duration::from_millis(500)).await;

        let started = Instant::now();
        close_like_the_agent(&agent).await;
        let took = started.elapsed();
        eprintln!("healthy close: {took:?}");
        assert!(
            took < Duration::from_secs(1),
            "a healthy close took {took:?}, stuck at {}",
            states(&agent, &dc)
        );
        assert!(
            tokio::time::timeout(SETTLE, writer).await.is_ok(),
            "the writer outlived its channel's close"
        );
        viewer.close().await.expect("viewer close");
        drop((dc, agent, viewer));
        let left = settle_to(baseline).await;
        assert!(
            left <= baseline,
            "{left} tasks alive after both closes, {baseline} before the peers"
        );
    });
}

/// The safety net under every stall a root-cause fix has not met yet: a close
/// whose caller stops waiting (the agent's budget) goes on to the end on a
/// task of its own. Here the caller polls the close once and drops it, which
/// cuts an inline `RTCPeerConnection::close` at its first wait (the
/// interceptors' loops ending): checked against that, this test fails.
#[test]
fn a_close_its_caller_stopped_waiting_for_still_finishes() {
    runtime().block_on(async {
        let wire = Arc::new(Wire::default());
        let (agent_net, viewer_net) = wan(&wire).await;
        let baseline = alive();
        let viewer = peer(viewer_net).await;
        let agent = peer(agent_net).await;
        let (dc, _) = connect(&viewer, &agent).await;

        let session = bson::oid::ObjectId::new();
        let mut close = Box::pin(close_peer_connection(Arc::clone(&agent), session));
        let finished =
            std::future::poll_fn(|cx| std::task::Poll::Ready(close.as_mut().poll(cx).is_ready()))
                .await;
        assert!(
            !finished,
            "the close must still be running when its caller stops waiting, or this proves nothing"
        );
        drop(close);

        viewer.close().await.expect("viewer close");
        drop((dc, agent, viewer));
        let left = settle_to(baseline).await;
        assert!(
            left <= baseline,
            "{left} tasks alive after both closes, {baseline} before the peers: \
             a close its caller stopped waiting for was cut short"
        );
    });
}

/// Writers sending at once on one channel in [`many_writers_share_one_ordered_channel`].
const WRITERS: u16 = 8;
/// Messages each writer sends.
const EACH: u32 = 60;
/// Writer, number and length, ahead of each test message's payload.
const HEADER: usize = 10;
/// How long the receiver waits for the next message before it stops.
const QUIET: Duration = Duration::from_secs(15);

/// Message `seq` of writer `writer`: even ones are 9 to 29 KB, which SCTP cuts
/// into 8 to 25 fragments, odd ones are under 320 bytes, one fragment. Every
/// byte is a function of the writer, the number and the position, so a
/// message that arrives merged with another, cut short, or put together from
/// two does not compare equal to any message.
fn test_message(writer: u16, seq: u32) -> Bytes {
    let (w, s) = (usize::from(writer), seq as usize);
    let len = if seq.is_multiple_of(2) {
        9_000 + (w * 7_919 + s * 104_729) % 20_000
    } else {
        HEADER + (w * 31 + s * 17) % 300
    };
    let mut m = Vec::with_capacity(len);
    m.extend_from_slice(&writer.to_le_bytes());
    m.extend_from_slice(&seq.to_le_bytes());
    m.extend_from_slice(&(len as u32).to_le_bytes());
    m.extend((HEADER..len).map(|i| (w.wrapping_mul(37) ^ s.wrapping_mul(11) ^ i) as u8));
    Bytes::from(m)
}

/// The writer and number of a received message, if it is exactly one whole
/// test message.
fn whole_message(m: &[u8]) -> Option<(u16, u32)> {
    let header = m.get(..HEADER)?;
    let writer = u16::from_le_bytes([header[0], header[1]]);
    let seq = u32::from_le_bytes([header[2], header[3], header[4], header[5]]);
    (writer < WRITERS && seq < EACH && test_message(writer, seq) == m).then_some((writer, seq))
}

/// Everything wrong with what arrived: a message that is not whole, one that
/// came twice, one out of its writer's order, and the count that never came.
fn delivery_problems(arrived: &[Bytes]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut due = vec![0_u32; usize::from(WRITERS)];
    for (i, m) in arrived.iter().enumerate() {
        let Some((writer, seq)) = whole_message(m) else {
            problems.push(format!(
                "arrival {i} ({} bytes) is not one whole message: two merged, or one split or cut",
                m.len()
            ));
            continue;
        };
        let next = &mut due[usize::from(writer)];
        if !seen.insert((writer, seq)) {
            problems.push(format!("writer {writer}'s message {seq} arrived twice"));
        } else if seq != *next {
            problems.push(format!(
                "writer {writer}'s message {seq} arrived where its message {} was due",
                *next
            ));
        }
        *next = (*next).max(seq + 1);
    }
    let missing = usize::from(WRITERS) * EACH as usize - seen.len();
    if missing > 0 {
        problems.push(format!("{missing} message(s) never arrived"));
    }
    problems
}

/// #3716's own send path (the review's F1). `send` stopped holding the
/// channel's slot lock across the SCTP write so that a writer parked in flow
/// control could not hold the close, and that lock was the only thing
/// ordering a channel's writes: an SCTP stream takes none, and each message
/// reads its stream sequence number fragment by fragment, then increments it.
/// Two sends at once on two worker threads then shared a number or split one
/// message across two, and the receiver dropped one, merged them, or waited
/// for good. The agent has such writers on one channel: the control channel
/// (the cursor poller, the arbiter, the media plan, the idle timer) and each
/// of a tunnel's eight pooled channels (every flow's pump, and the
/// keepalive). Here eight writers send at once on one ordered channel, with
/// messages that fragment and messages that don't, to a webrtc-rs receiver:
/// every message must arrive whole, once, and in its writer's order.
#[test]
fn many_writers_share_one_ordered_channel() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let wire = Arc::new(Wire::default());
            let (agent_net, viewer_net) = wan(&wire).await;
            let viewer = peer(viewer_net).await;
            let agent = peer(agent_net).await;

            // The viewer's end keeps every message it receives. Nothing is
            // sent before the handler is in place.
            let (dc, receiving) = connect(&viewer, &agent).await;
            assert!(
                dc.ordered() && receiving.ordered(),
                "the channel must be ordered"
            );
            let (inbox_tx, mut inbox) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
            receiving.on_message(Box::new(
                move |m: webrtc::data_channel::data_channel_message::DataChannelMessage| {
                    let _ = inbox_tx.send(m.data);
                    Box::pin(async {})
                },
            ));

            let started = Instant::now();
            let writers: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let dc = Arc::clone(&dc);
                    tokio::spawn(async move {
                        for seq in 0..EACH {
                            if dc.send(&test_message(writer, seq)).await.is_err() {
                                return;
                            }
                        }
                    })
                })
                .collect();

            let expected = usize::from(WRITERS) * EACH as usize;
            let mut arrived = Vec::with_capacity(expected);
            while arrived.len() < expected {
                match tokio::time::timeout(QUIET, inbox.recv()).await {
                    Ok(Some(m)) => arrived.push(m),
                    _ => break,
                }
            }
            let took = started.elapsed();
            for writer in &writers {
                writer.abort();
            }
            let problems = delivery_problems(&arrived);
            eprintln!(
                "{} of {expected} messages arrived in {took:?}; {} problem(s)",
                arrived.len(),
                problems.len()
            );

            let _ = tokio::time::timeout(Duration::from_secs(10), agent.close()).await;
            let _ = tokio::time::timeout(Duration::from_secs(10), viewer.close()).await;
            assert!(
                problems.is_empty(),
                "{WRITERS} writers sent {expected} messages at once on one ordered channel, and \
                 {} of them arrived; every one must arrive whole, once, and in its writer's \
                 order. {} problem(s), the first: {:#?}",
                arrived.len(),
                problems.len(),
                &problems[..problems.len().min(6)]
            );
        });
}

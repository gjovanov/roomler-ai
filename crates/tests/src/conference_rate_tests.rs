// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The per-socket limit on `media:*` frames
//! (`roomler_ai_mod_conference::frame_limit`).
//!
//! The limit is sized from the deepest legitimate burst a socket sends: a join
//! into a large meeting. [`a_join_into_a_full_business_meeting_passes_and_a_flood_is_refused`]
//! builds that meeting, Business's 100 people with a camera and a microphone
//! each, and joins it the way the web client does (`ui/src/stores/conference.ts`)
//! as fast as the server answers. It prints the burst it measured, and holds
//! the shipped default to it: the whole join fits the burst with no help from
//! the refill. A flood from the same socket after the join is refused, and
//! the socket stays.
//!
//! Ordering, as in `conference_gate_tests`: a socket's frames are handled one
//! at a time, so the `pong` that answers a `ping` comes after everything the
//! frames before it caused on that socket ([`settle`]).

use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::fixtures::test_app::TestApp;

/// Business's `video_max_participants`: the largest meeting a plan sells.
const MEETING: usize = 100;

/// Frames in the flood that follows the join.
const FLOOD: usize = 1000;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Peer {
    ws: Ws,
    conn: String,
}

async fn connect(app: &TestApp, token: &str) -> Peer {
    let url = format!("ws://{}/ws?token={token}", app.addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("the user socket opens");
    let first = ws.next().await.expect("a frame").expect("a text frame");
    let parsed: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "connected", "{parsed}");
    let conn = parsed["connection_id"]
        .as_str()
        .expect("the connected frame carries connection_id")
        .to_string();
    Peer { ws, conn }
}

async fn send(peer: &mut Peer, msg_type: &str, data: Value) {
    peer.ws
        .send(Message::Text(
            json!({ "type": msg_type, "data": data }).to_string().into(),
        ))
        .await
        .unwrap();
}

fn is_rate_limited(f: &Value) -> bool {
    f["type"] == "media:error" && f["data"]["code"] == "rate_limited"
}

/// Every frame up to and including the first of type `kind`, within 10 s.
/// A refusal over the limit on the way fails at once, naming it.
async fn frames_until(peer: &mut Peer, kind: &str) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    loop {
        let left = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_else(|| panic!("no {kind} within 10 s; saw {seen:?}"));
        let frame = match tokio::time::timeout(left, peer.ws.next()).await {
            Ok(Some(Ok(frame))) => frame,
            other => panic!("socket ended waiting for {kind}: {other:?}; saw {seen:?}"),
        };
        let Ok(text) = frame.to_text() else { continue };
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if kind != "pong" && is_rate_limited(&v) {
            panic!("a frame was refused over the limit waiting for {kind}: {v}");
        }
        let done = v["type"] == kind;
        seen.push(v);
        if done {
            return seen;
        }
    }
}

/// Everything this socket received up to the answer to a `ping` sent now.
/// A `ping` is not a `media:*` frame, so it costs no budget.
async fn settle(peer: &mut Peer) -> Vec<Value> {
    send(peer, "ping", Value::Null).await;
    frames_until(peer, "pong").await
}

async fn call(
    app: &TestApp,
    tenant_id: &str,
    room_id: &str,
    token: &str,
    action: &str,
    connection_id: Option<&str>,
) -> u16 {
    let req = app.auth_post(
        &format!("/api/tenant/{tenant_id}/room/{room_id}/call/{action}"),
        token,
    );
    let req = match connection_id {
        Some(cid) => req.json(&json!({ "connection_id": cid })),
        None => req,
    };
    req.send().await.unwrap().status().as_u16()
}

fn produce_frame(room_id: &str, kind: &str, ssrc: u32) -> Value {
    let codec = match kind {
        "audio" => json!({
            "mimeType": "audio/opus", "payloadType": 111, "clockRate": 48000, "channels": 2,
            "parameters": {}, "rtcpFeedback": []
        }),
        _ => json!({
            "mimeType": "video/VP8", "payloadType": 96, "clockRate": 90000,
            "parameters": {}, "rtcpFeedback": []
        }),
    };
    json!({
        "room_id": room_id,
        "kind": kind,
        "rtp_parameters": {
            "codecs": [codec],
            "headerExtensions": [],
            "encodings": [{ "ssrc": ssrc }],
            "rtcp": { "cname": format!("cname-{ssrc}"), "reducedSize": true }
        },
        "source": if kind == "audio" { "audio" } else { "camera" },
    })
}

/// The DTLS parameters a browser's transport sends on its `connect` event.
fn dtls() -> Value {
    let value = (0..32)
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    json!({ "role": "client", "fingerprints": [{ "algorithm": "sha-256", "value": value }] })
}

/// One of the meeting's people: in its media from a socket of its own,
/// sending a microphone and a camera.
async fn seat_producer(app: &TestApp, token: &str, room: &str, i: u32) -> Peer {
    let mut p = connect(app, token).await;
    send(&mut p, "media:join", json!({ "room_id": room })).await;
    frames_until(&mut p, "media:transport_created").await;
    for (kind, ssrc) in [("audio", 200_000 + 2 * i), ("video", 200_001 + 2 * i)] {
        send(&mut p, "media:produce", produce_frame(room, kind, ssrc)).await;
        frames_until(&mut p, "media:produce_result").await;
    }
    p
}

/// What the joining client sent, frame by frame.
#[derive(Default)]
struct Sent {
    at: Vec<Instant>,
    join: usize,
    consume: usize,
    connect: usize,
    produce: usize,
    view: usize,
}

impl Sent {
    fn total(&self) -> usize {
        self.at.len()
    }

    /// The most frames sent in any one second.
    fn peak_per_second(&self) -> usize {
        let mut peak = 0;
        let mut start = 0;
        for end in 0..self.at.len() {
            while self.at[end].duration_since(self.at[start]) >= Duration::from_secs(1) {
                start += 1;
            }
            peak = peak.max(end - start + 1);
        }
        peak
    }

    fn span(&self) -> Duration {
        match (self.at.first(), self.at.last()) {
            (Some(a), Some(b)) => b.duration_since(*a),
            _ => Duration::ZERO,
        }
    }
}

async fn send_counted(peer: &mut Peer, sent: &mut Sent, msg_type: &str, data: Value) {
    sent.at.push(Instant::now());
    match msg_type {
        "media:join" => sent.join += 1,
        "media:consume" => sent.consume += 1,
        "media:connect_transport" => sent.connect += 1,
        "media:produce" => sent.produce += 1,
        "media:view" => sent.view += 1,
        other => panic!("not a join's frame: {other}"),
    }
    send(peer, msg_type, data).await;
}

/// A join into Business's largest meeting is the deepest burst a real client
/// sends, and the shipped limit passes all of it on the bucket's depth alone.
/// A flood from the same socket afterwards is refused frame by frame, no more
/// of it runs than the bucket held and refilled, and the socket stays: it
/// answers a ping, and the call works again once the bucket refills.
#[tokio::test]
async fn a_join_into_a_full_business_meeting_passes_and_a_flood_is_refused() {
    let app = TestApp::spawn_with_settings(|s| {
        // Two transports a person, a UDP and a TCP port each: the default
        // test range holds about fifty people.
        s.mediasoup.rtc_min_port = 44_000;
        s.mediasoup.rtc_max_port = 45_999;
    })
    .await;
    let shipped = roomler_ai_config::MeetingsSettings::default();
    let limits = &app.state.settings.meetings;
    assert_eq!(
        (limits.media_frame_burst, limits.media_frame_rate),
        (shipped.media_frame_burst, shipped.media_frame_rate),
        "the test runs the shipped limit"
    );
    let (burst, rate) = (
        limits.media_frame_burst as usize,
        f64::from(limits.media_frame_rate),
    );

    let tenant = app.seed_tenant("mrate").await;
    let (tid, room) = (tenant.tenant_id.clone(), tenant.rooms[0].id.clone());
    let host_token = tenant.admin.access_token.clone();
    assert_eq!(
        call(&app, &tid, &room, &host_token, "start", None).await,
        200
    );
    // One call session with no connection id admits every socket of the
    // account (an older client's shape), so the meeting's other 99 people
    // are 99 sockets of one account: what the joiner sees is the same.
    assert_eq!(
        call(&app, &tid, &room, &host_token, "join", None).await,
        200
    );

    let people: Vec<Peer> = futures::stream::iter(0..(MEETING as u32 - 1))
        .map(|i| {
            let (app, token, room) = (&app, host_token.as_str(), room.as_str());
            async move { seat_producer(app, token, room, i).await }
        })
        .buffer_unordered(8)
        .collect()
        .await;
    assert_eq!(people.len(), MEETING - 1);

    // The 100th person joins, as the web client does.
    let member_token = tenant.member.access_token.clone();
    let mut j = connect(&app, &member_token).await;
    assert_eq!(
        call(&app, &tid, &room, &member_token, "join", Some(&j.conn)).await,
        200
    );
    let mut sent = Sent::default();
    send_counted(
        &mut j,
        &mut sent,
        "media:join",
        json!({ "room_id": room, "caps": ["view"] }),
    )
    .await;
    let mut frames = frames_until(&mut j, "media:router_capabilities").await;
    let caps = frames.last().unwrap()["data"]["rtp_capabilities"].clone();
    frames.extend(frames_until(&mut j, "media:transport_created").await);
    let transports = frames.last().unwrap()["data"].clone();
    let (send_id, recv_id) = (
        transports["send_transport"]["id"].clone(),
        transports["recv_transport"]["id"].clone(),
    );
    // The producers already in the call are announced before the join's
    // handler returns, so all of them are in by the pong.
    frames.extend(settle(&mut j).await);
    let producers: Vec<(String, String)> = frames
        .iter()
        .filter(|f| f["type"] == "media:new_producer")
        .map(|f| {
            (
                f["data"]["producer_id"].as_str().unwrap().to_string(),
                f["data"]["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        producers.len(),
        2 * (MEETING - 1),
        "every camera and microphone is announced"
    );

    // The client's own camera and microphone: the send transport connects on
    // its first produce, then each produce waits for its id.
    send_counted(
        &mut j,
        &mut sent,
        "media:connect_transport",
        json!({ "room_id": room, "transport_id": send_id, "dtls_parameters": dtls() }),
    )
    .await;
    for (kind, ssrc) in [("audio", 300_001), ("video", 300_002)] {
        send_counted(
            &mut j,
            &mut sent,
            "media:produce",
            produce_frame(&room, kind, ssrc),
        )
        .await;
        frames_until(&mut j, "media:produce_result").await;
    }
    // Every producer consumed, one after another, as the client's consume
    // chain does: each waits for its consumer. The receive transport connects
    // on the first; a video consumer schedules a view report, which the client
    // sends at most every 100 ms.
    let mut videos = Vec::new();
    let mut last_view: Option<Instant> = None;
    for (n, (producer_id, kind)) in producers.iter().enumerate() {
        send_counted(
            &mut j,
            &mut sent,
            "media:consume",
            json!({ "room_id": room, "producer_id": producer_id, "rtp_capabilities": caps }),
        )
        .await;
        frames_until(&mut j, "media:consumer_created").await;
        if n == 0 {
            send_counted(
                &mut j,
                &mut sent,
                "media:connect_transport",
                json!({ "room_id": room, "transport_id": recv_id, "dtls_parameters": dtls() }),
            )
            .await;
        }
        if kind == "video" {
            videos.push(json!({ "producer_id": producer_id, "w": 320, "h": 180 }));
            if last_view.is_none_or(|t| t.elapsed() >= Duration::from_millis(100)) {
                last_view = Some(Instant::now());
                let seq = sent.view as u64 + 1;
                send_counted(
                    &mut j,
                    &mut sent,
                    "media:view",
                    json!({ "room_id": room, "seq": seq, "video": videos }),
                )
                .await;
            }
        }
    }
    let after_join = settle(&mut j).await;
    assert!(
        !after_join.iter().any(is_rate_limited),
        "the join was cut short by the limit: {after_join:?}"
    );

    println!(
        "JOIN BURST: a meeting of {MEETING} ({} producers): {} media frames \
         (join {}, consume {}, connect {}, produce {}, view {}) in {} ms, \
         at most {} in any one second; the shipped limit is {burst} at once, {rate}/s",
        producers.len(),
        sent.total(),
        sent.join,
        sent.consume,
        sent.connect,
        sent.produce,
        sent.view,
        sent.span().as_millis(),
        sent.peak_per_second(),
    );
    assert!(
        sent.total() <= burst,
        "the join ({} frames) does not fit the burst ({burst}) on its own: \
         a client that answers faster than this test would be cut short",
        sent.total()
    );

    // A flood on the same socket: refused frame by frame, never run past
    // what the bucket held and refilled, and the socket stays.
    let counted_before = roomler_core::cluster::metrics::MEDIA_FRAMES_RATE_LIMITED_TOTAL
        .load(std::sync::atomic::Ordering::Relaxed);
    let flood_started = Instant::now();
    for _ in 0..FLOOD {
        send(
            &mut j,
            "media:view",
            json!({ "room_id": room, "seq": 1, "video": [] }),
        )
        .await;
    }
    let answered = settle(&mut j).await;
    let flood_took = flood_started.elapsed();
    let refused: Vec<&Value> = answered.iter().filter(|f| is_rate_limited(f)).collect();
    let ran = FLOOD - refused.len();
    let budget = burst as f64 + rate * flood_took.as_secs_f64().ceil() + 1.0;
    println!(
        "FLOOD: {FLOOD} media:view frames in {} ms: {} refused, {ran} ran",
        flood_took.as_millis(),
        refused.len()
    );
    assert!(
        !refused.is_empty(),
        "a flood of {FLOOD} frames was not refused"
    );
    assert!(
        ran as f64 <= budget,
        "{ran} frames of the flood ran, more than the bucket's {burst} plus its refill"
    );
    for r in &refused {
        assert!(r["data"]["retry_after_ms"].is_u64(), "{r}");
        assert!(r["data"]["message"].is_string(), "{r}");
    }
    let counted = roomler_core::cluster::metrics::MEDIA_FRAMES_RATE_LIMITED_TOTAL
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        counted >= counted_before + refused.len() as u64,
        "each refusal is counted: {counted_before} -> {counted}"
    );

    // The bucket refills, and the same socket gets on with the call.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    send(
        &mut j,
        "media:consume",
        json!({ "room_id": room, "producer_id": producers[0].0, "rtp_capabilities": caps }),
    )
    .await;
    let back = frames_until(&mut j, "media:consumer_created").await;
    assert_eq!(
        back.last().unwrap()["data"]["producer_id"],
        producers[0].0.as_str()
    );
    drop(people);
}

/// The wire of a refusal, on a small limit: the frame past the burst is
/// answered `media:error {code: "rate_limited", retry_after_ms}` and not run
/// (the participation gate never sees it, so it does not answer), and the
/// socket stays and is let through again once the bucket refills.
#[tokio::test]
async fn a_frame_over_the_limit_is_answered_not_run_and_the_socket_stays() {
    let app = TestApp::spawn_with_settings(|s| {
        s.meetings.media_frame_burst = 3;
        s.meetings.media_frame_rate = 1;
    })
    .await;
    let tenant = app.seed_tenant("mrate-wire").await;
    let room = tenant.rooms[0].id.clone();
    let mut p = connect(&app, &tenant.member.access_token).await;

    // Four produces from a socket in no call: the gate answers the first
    // three, the limit the fourth.
    for i in 0..4 {
        send(
            &mut p,
            "media:produce",
            produce_frame(&room, "video", 1_000 + i),
        )
        .await;
    }
    let answers: Vec<Value> = settle(&mut p)
        .await
        .into_iter()
        .filter(|f| f["type"] == "media:error")
        .collect();
    assert_eq!(answers.len(), 4, "{answers:?}");
    for gate in &answers[..3] {
        assert_eq!(
            gate["data"]["message"], "Not a participant of this call",
            "{gate}"
        );
        assert!(gate["data"]["code"].is_null(), "{gate}");
    }
    let limited = &answers[3];
    assert_eq!(limited["data"]["code"], "rate_limited", "{limited}");
    let wait = limited["data"]["retry_after_ms"].as_u64().unwrap();
    assert!(wait > 0 && wait <= 1000, "one frame a second: {limited}");

    // The socket stays, and the refilled bucket lets the next frame through
    // to the gate.
    tokio::time::sleep(Duration::from_millis(wait + 100)).await;
    send(
        &mut p,
        "media:produce",
        produce_frame(&room, "video", 2_000),
    )
    .await;
    let next: Vec<Value> = settle(&mut p)
        .await
        .into_iter()
        .filter(|f| f["type"] == "media:error")
        .collect();
    assert_eq!(next.len(), 1, "{next:?}");
    assert_eq!(next[0]["data"]["message"], "Not a participant of this call");
}

/// `media_frame_burst = 0` turns the limit off: a socket's frames all run.
#[tokio::test]
async fn a_zero_burst_turns_the_limit_off() {
    let app = TestApp::spawn_with_settings(|s| {
        s.meetings.media_frame_burst = 0;
    })
    .await;
    let tenant = app.seed_tenant("mrate-off").await;
    let room = tenant.rooms[0].id.clone();
    let mut p = connect(&app, &tenant.member.access_token).await;
    for i in 0..50 {
        send(
            &mut p,
            "media:produce",
            produce_frame(&room, "video", 3_000 + i),
        )
        .await;
    }
    let answers: Vec<Value> = settle(&mut p)
        .await
        .into_iter()
        .filter(|f| f["type"] == "media:error")
        .collect();
    assert_eq!(answers.len(), 50, "{answers:?}");
    assert!(
        answers
            .iter()
            .all(|a| a["data"]["message"] == "Not a participant of this call"),
        "{answers:?}"
    );
    assert_eq!(
        conference(&app).media_frames.sockets(),
        0,
        "an off limit keeps no buckets"
    );
}

fn conference(app: &TestApp) -> &roomler_ai_mod_conference::ConferenceState {
    app.state
        .modules
        .conference
        .as_ref()
        .expect("the conference module is mounted in tests")
}

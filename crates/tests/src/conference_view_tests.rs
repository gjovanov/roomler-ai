// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-127 P1 — receive what is visible, against a real mediasoup worker and
//! real sockets: a client that declares `caps: ["view"]` has its video
//! consumers created paused and resumed by the `media:view` reports that
//! name them, audio always flows, a legacy client receives everything, the
//! kill switch keeps today's shape, an older report is dropped, a report may
//! name a producer before its consumer exists, the server's cap truncates a
//! long report, a report sent through the other pod reaches the owner's
//! consumers, and — AC2 — an owner's FR-30 pause and a viewer's off-screen
//! pause never merge.
//!
//! The consumers' flags are read straight off the owner pod's room manager
//! (`consumer_states` / `producer_states`, plain data). `media:view` has no
//! reply, so a state that must CHANGE is polled for, and a state that must
//! NOT change is read after a `barrier`: a `media:consume` with an invalid
//! producer id, whose `media:error` reply proves every earlier frame on that
//! socket was handled (the WS loop awaits each message in turn).

use std::time::Duration;

use bson::oid::ObjectId;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use roomler_ai_mod_conference::ConferenceState;
use roomler_ai_mod_conference::media::room_manager::ConsumerSnapshot;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A user socket and the connection id the server minted for it.
struct Peer {
    ws: Ws,
    conn: String,
}

fn conf(app: &TestApp) -> &ConferenceState {
    app.state
        .modules
        .conference
        .as_ref()
        .expect("the conference module is mounted in tests")
}

fn oid(hex: &str) -> ObjectId {
    ObjectId::parse_str(hex).unwrap()
}

async fn connect(app: &TestApp, token: &str) -> Peer {
    let url = format!("ws://{}/ws?token={token}", app.addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("the user socket opens");
    let first = ws.next().await.expect("a frame").expect("a text frame");
    let parsed: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "connected");
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

/// Every frame up to and including the first of type `kind`, within 10 s.
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
        let done = v["type"] == kind;
        seen.push(v);
        if done {
            return seen;
        }
    }
}

async fn next_of(peer: &mut Peer, kind: &str) -> Value {
    frames_until(peer, kind).await.pop().unwrap()
}

/// `media:join`, with or without `caps`; returns the router capabilities
/// (to consume with) and the `media:transport_created` frame.
async fn join(peer: &mut Peer, room_id: &str, caps: Option<&[&str]>) -> (Value, Value) {
    let mut data = json!({ "room_id": room_id });
    if let Some(caps) = caps {
        data["caps"] = json!(caps);
    }
    send(peer, "media:join", data).await;
    let caps = next_of(peer, "media:router_capabilities").await;
    let transport = next_of(peer, "media:transport_created").await;
    (caps["data"]["rtp_capabilities"].clone(), transport)
}

/// Parameters the worker accepts for a producer on a transport that never
/// DTLS-connects: the router's VP8 and Opus, one SSRC each. `channels`,
/// `headerExtensions` and `rtcp.reducedSize` have no serde default.
fn producer_params(kind: &str, ssrc: u32) -> Value {
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
        "codecs": [codec],
        "headerExtensions": [],
        "encodings": [{ "ssrc": ssrc }],
        "rtcp": { "cname": format!("cname-{ssrc}"), "reducedSize": true }
    })
}

/// Produce `kind` and return the producer id the server minted.
async fn produce(peer: &mut Peer, room_id: &str, kind: &str, ssrc: u32) -> String {
    send(
        peer,
        "media:produce",
        json!({
            "room_id": room_id,
            "kind": kind,
            "rtp_parameters": producer_params(kind, ssrc),
            "source": if kind == "audio" { "audio" } else { "camera" },
        }),
    )
    .await;
    let frames = frames_until(peer, "media:produce_result").await;
    let result = frames.last().unwrap();
    result["data"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("produce_result carries an id: {frames:?}"))
        .to_string()
}

/// Consume `producer_id` and return the `media:consumer_created` frame.
async fn consume(peer: &mut Peer, room_id: &str, producer_id: &str, caps: &Value) -> Value {
    send(
        peer,
        "media:consume",
        json!({ "room_id": room_id, "producer_id": producer_id, "rtp_capabilities": caps }),
    )
    .await;
    let frames = frames_until(peer, "media:consumer_created").await;
    let created = frames.last().unwrap().clone();
    assert_eq!(created["data"]["producer_id"], producer_id, "{frames:?}");
    created
}

async fn view(peer: &mut Peer, room_id: &str, seq: u64, producers: &[&str]) {
    let video: Vec<Value> = producers
        .iter()
        .map(|p| json!({ "producer_id": p, "w": 320, "h": 240 }))
        .collect();
    send(
        peer,
        "media:view",
        json!({ "room_id": room_id, "seq": seq, "video": video }),
    )
    .await;
}

/// An ordered probe: when its `media:error` reply arrives, every frame this
/// socket sent before it has been handled. Returns the frames seen on the way.
async fn barrier(peer: &mut Peer, room_id: &str, caps: &Value) -> Vec<Value> {
    send(
        peer,
        "media:consume",
        json!({ "room_id": room_id, "producer_id": "not-a-producer", "rtp_capabilities": caps }),
    )
    .await;
    let frames = frames_until(peer, "media:error").await;
    assert_eq!(
        frames.last().unwrap()["data"]["message"],
        "Invalid producer_id",
        "{frames:?}"
    );
    frames
}

async fn producer_pause(peer: &mut Peer, room_id: &str, producer_id: &str, paused: bool) {
    send(
        peer,
        "media:producer_pause",
        json!({ "room_id": room_id, "producer_id": producer_id, "paused": paused }),
    )
    .await;
}

/// Poll `cond` every 25 ms for up to 5 s.
async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: not within 5 s"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn consumer_of(app: &TestApp, room_id: &str, conn: &str, producer_id: &str) -> ConsumerSnapshot {
    conf(app)
        .room_manager
        .consumer_states(&oid(room_id), conn)
        .into_iter()
        .find(|c| c.producer_id == producer_id)
        .unwrap_or_else(|| panic!("{conn} has a consumer of {producer_id}"))
}

fn producer_paused(app: &TestApp, room_id: &str, conn: &str, producer_id: &str) -> bool {
    conf(app)
        .room_manager
        .producer_states(&oid(room_id), conn)
        .into_iter()
        .find(|p| p.producer_id == producer_id)
        .unwrap_or_else(|| panic!("{conn} owns producer {producer_id}"))
        .paused
}

/// A tenant with a started call in its first room, both users REST-joined.
async fn started_call(app: &TestApp, slug: &str) -> (SeededTenant, String) {
    let tenant = app.seed_tenant(slug).await;
    let room_id = tenant.rooms[0].id.clone();
    let start = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/call/start",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(start.status().as_u16(), 200);
    for token in [&tenant.admin.access_token, &tenant.member.access_token] {
        let joined = app
            .auth_post(
                &format!(
                    "/api/tenant/{}/room/{}/call/join",
                    tenant.tenant_id, room_id
                ),
                token,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(joined.status().as_u16(), 200);
    }
    (tenant, room_id)
}

fn levers(transport: &Value) -> Value {
    transport["data"]["meeting"].clone()
}

/// The core of P1: a view client's video consumer is created paused and
/// follows its reports; its audio consumer always flows; an older report is
/// dropped; the same report again is applied, idempotently; the gauge counts
/// what is held back.
#[tokio::test]
async fn a_view_client_receives_only_the_video_it_shows() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "view1").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, transport) = join(&mut b, &room_id, Some(&["view"])).await;
    assert_eq!(
        levers(&transport),
        json!({ "view": true, "audio_dtx": true, "simulcast": null }),
        "the server announces its levers: {transport}"
    );

    let a_video = produce(&mut a, &room_id, "video", 11_001).await;
    let a_audio = produce(&mut a, &room_id, "audio", 11_002).await;

    // Created paused: a view client that has reported nothing shows nothing.
    consume(&mut b, &room_id, &a_video, &caps).await;
    let v = consumer_of(&app, &room_id, &b.conn, &a_video);
    assert_eq!(v.kind, "video");
    assert!(v.paused, "a video consumer of a view client starts paused");
    assert!(!v.producer_paused, "the owner has not paused anything");
    consume(&mut b, &room_id, &a_audio, &caps).await;
    let au = consumer_of(&app, &room_id, &b.conn, &a_audio);
    assert_eq!(au.kind, "audio");
    assert!(!au.paused, "audio always flows");
    assert_eq!(
        conf(&app).media_gauges().video_paused_total,
        1,
        "the gauge counts the held-back video consumer"
    );

    // The report names it: resumed.
    view(&mut b, &room_id, 1, &[&a_video]).await;
    wait_for("the named video consumer resumes", || {
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;
    assert_eq!(conf(&app).media_gauges().video_paused_total, 0);

    // An empty report: paused again; audio untouched.
    view(&mut b, &room_id, 2, &[]).await;
    wait_for("an empty view pauses the video consumer", || {
        consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;
    assert!(!consumer_of(&app, &room_id, &b.conn, &a_audio).paused);

    // A report older than the one held arrives late: dropped.
    view(&mut b, &room_id, 1, &[&a_video]).await;
    barrier(&mut b, &room_id, &caps).await;
    assert!(
        consumer_of(&app, &room_id, &b.conn, &a_video).paused,
        "seq 1 after seq 2 is stale and changes nothing"
    );

    // The report held, sent again: applied — the set is idempotent.
    view(&mut b, &room_id, 2, &[&a_video]).await;
    wait_for("a re-sent report with the held seq is applied", || {
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// A client that never declared `caps` is a legacy client: every consumer
/// is resumed at creation, and a `media:view` from it is ignored — never
/// read as "show nothing".
#[tokio::test]
async fn a_legacy_client_receives_everything_and_its_views_are_ignored() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "view2").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 12_001).await;

    let mut c = connect(&app, &tenant.member.access_token).await;
    let (caps, transport) = join(&mut c, &room_id, None).await;
    assert_eq!(
        levers(&transport)["view"],
        true,
        "the lever is announced to every client; the legacy one just never asked"
    );
    consume(&mut c, &room_id, &a_video, &caps).await;
    assert!(
        !consumer_of(&app, &room_id, &c.conn, &a_video).paused,
        "a legacy client's video consumer is resumed at creation"
    );

    view(&mut c, &room_id, 1, &[]).await;
    barrier(&mut c, &room_id, &caps).await;
    assert!(
        !consumer_of(&app, &room_id, &c.conn, &a_video).paused,
        "an empty view from a legacy client is ignored, not read as Some(empty)"
    );

    a.ws.close(None).await.ok();
    c.ws.close(None).await.ok();
}

/// `[meetings] visible_tiles = false` and `audio_dtx = false`: the client's
/// `caps` and reports are ignored, every consumer is resumed at creation,
/// and the client is told both levers are off.
#[tokio::test]
async fn the_kill_switch_keeps_todays_shape() {
    let app = TestApp::spawn_with_settings(|s| {
        s.meetings.visible_tiles = false;
        s.meetings.audio_dtx = false;
    })
    .await;
    let (tenant, room_id) = started_call(&app, "view3").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 13_001).await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, transport) = join(&mut b, &room_id, Some(&["view"])).await;
    assert_eq!(
        levers(&transport),
        json!({ "view": false, "audio_dtx": false, "simulcast": null })
    );
    consume(&mut b, &room_id, &a_video, &caps).await;
    assert!(
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused,
        "with the lever off a view client's consumer is resumed at creation"
    );
    view(&mut b, &room_id, 1, &[]).await;
    barrier(&mut b, &room_id, &caps).await;
    assert!(
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused,
        "with the lever off a report changes nothing"
    );
    assert_eq!(conf(&app).media_gauges().video_paused_total, 0);

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// AC2 — the owner's pause (FR-30, `Producer::pause`, seen by every viewer
/// as `producer_paused`) and the viewer's pause (`Consumer::pause`, the
/// viewer's own `paused`) are two flags: a viewer's report never moves the
/// owner's, the owner's toggle never moves a viewer's, and a viewer's report
/// fans no `media:producer_paused` out to anyone.
#[tokio::test]
async fn an_owners_pause_and_a_viewers_pause_never_merge() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "view4").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 14_001).await;

    // B is a view client, C a legacy one (the admin's second browser).
    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps_b, _) = join(&mut b, &room_id, Some(&["view"])).await;
    consume(&mut b, &room_id, &a_video, &caps_b).await;
    let mut c = connect(&app, &tenant.admin.access_token).await;
    let (caps_c, _) = join(&mut c, &room_id, None).await;
    consume(&mut c, &room_id, &a_video, &caps_c).await;
    assert!(consumer_of(&app, &room_id, &b.conn, &a_video).paused);
    assert!(!consumer_of(&app, &room_id, &c.conn, &a_video).paused);

    // The owner pauses: both viewers see producer_paused, and both hear it.
    producer_pause(&mut a, &room_id, &a_video, true).await;
    wait_for("the owner's producer is paused", || {
        producer_paused(&app, &room_id, &a.conn, &a_video)
    })
    .await;
    wait_for("both viewers' consumers mirror the owner's pause", || {
        consumer_of(&app, &room_id, &b.conn, &a_video).producer_paused
            && consumer_of(&app, &room_id, &c.conn, &a_video).producer_paused
    })
    .await;
    for (name, peer) in [("B", &mut b), ("C", &mut c)] {
        let ev = next_of(peer, "media:producer_paused").await;
        assert_eq!(ev["data"]["producer_id"], a_video, "{name}: {ev}");
        assert_eq!(ev["data"]["paused"], true, "{name}: {ev}");
    }

    // B's report names the producer: B's own flag resumes, the owner's pause
    // stays — on the producer, on B's consumer and on C's — and C hears
    // nothing: a viewer's view is nobody else's business.
    view(&mut b, &room_id, 1, &[&a_video]).await;
    wait_for("B's own flag resumes", || {
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;
    let vb = consumer_of(&app, &room_id, &b.conn, &a_video);
    assert!(
        vb.producer_paused,
        "the owner's pause survives the viewer's view"
    );
    assert!(producer_paused(&app, &room_id, &a.conn, &a_video));
    let vc = consumer_of(&app, &room_id, &c.conn, &a_video);
    assert!(!vc.paused && vc.producer_paused, "C is untouched: {vc:?}");
    let heard = barrier(&mut c, &room_id, &caps_c).await;
    assert!(
        heard.iter().all(|f| f["type"] != "media:producer_paused"),
        "a viewer's report fans nothing out: {heard:?}"
    );

    // B hides the tile again: B's flag only.
    view(&mut b, &room_id, 2, &[]).await;
    wait_for("B's own flag pauses", || {
        consumer_of(&app, &room_id, &b.conn, &a_video).paused
    })
    .await;
    assert!(producer_paused(&app, &room_id, &a.conn, &a_video));
    assert!(!consumer_of(&app, &room_id, &c.conn, &a_video).paused);

    // The owner resumes: the owner's flag clears everywhere, the viewers'
    // own flags stay where their views left them.
    producer_pause(&mut a, &room_id, &a_video, false).await;
    wait_for("the owner's resume reaches both consumers", || {
        !consumer_of(&app, &room_id, &b.conn, &a_video).producer_paused
            && !consumer_of(&app, &room_id, &c.conn, &a_video).producer_paused
    })
    .await;
    assert!(
        consumer_of(&app, &room_id, &b.conn, &a_video).paused,
        "B still shows nothing, so B's own flag stays paused"
    );
    assert!(!consumer_of(&app, &room_id, &c.conn, &a_video).paused);

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
    c.ws.close(None).await.ok();
}

/// The client learns producer ids from `media:new_producer` before it
/// consumes, so a report may name a producer whose consumer does not exist
/// yet: the set is kept and the consumer is created resumed.
#[tokio::test]
async fn a_report_naming_a_producer_before_its_consumer_applies_at_consume() {
    let app = TestApp::spawn().await;
    let (tenant, room_id) = started_call(&app, "view5").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 15_001).await;
    let a_audio = produce(&mut a, &room_id, "audio", 15_002).await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, _) = join(&mut b, &room_id, Some(&["view"])).await;
    view(&mut b, &room_id, 1, &[&a_video]).await;
    barrier(&mut b, &room_id, &caps).await;

    consume(&mut b, &room_id, &a_video, &caps).await;
    assert!(
        !consumer_of(&app, &room_id, &b.conn, &a_video).paused,
        "named before it existed: created resumed"
    );
    consume(&mut b, &room_id, &a_audio, &caps).await;
    assert!(!consumer_of(&app, &room_id, &b.conn, &a_audio).paused);

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// `[meetings] view_max` truncates a report in the order sent.
#[tokio::test]
async fn a_long_report_is_truncated_at_view_max() {
    let app = TestApp::spawn_with_settings(|s| s.meetings.view_max = 1).await;
    let (tenant, room_id) = started_call(&app, "view6").await;

    let mut a = connect(&app, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 16_001).await;
    let mut d = connect(&app, &tenant.admin.access_token).await;
    join(&mut d, &room_id, None).await;
    let d_video = produce(&mut d, &room_id, "video", 16_002).await;

    let mut b = connect(&app, &tenant.member.access_token).await;
    let (caps, _) = join(&mut b, &room_id, Some(&["view"])).await;
    consume(&mut b, &room_id, &a_video, &caps).await;
    consume(&mut b, &room_id, &d_video, &caps).await;

    view(&mut b, &room_id, 1, &[&d_video, &a_video]).await;
    wait_for("the first tile of the report resumes", || {
        !consumer_of(&app, &room_id, &b.conn, &d_video).paused
    })
    .await;
    barrier(&mut b, &room_id, &caps).await;
    assert!(
        consumer_of(&app, &room_id, &b.conn, &a_video).paused,
        "the second tile is past the cap and stays paused"
    );

    a.ws.close(None).await.ok();
    d.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

/// Multi-pod (C-4): a viewer whose socket is on the non-owner pod sends
/// `media:view` like any other command — it is forwarded over `media.cmd`
/// and applied to the owner pod's `ParticipantMedia`.
#[tokio::test]
async fn a_view_sent_through_the_other_pod_reaches_the_owners_consumers() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let (tenant, room_id) = started_call(&app1, "view7").await;
    let rid = oid(&room_id);
    assert!(conf(&app1).room_manager.has_room(&rid));
    assert!(!conf(&app2).room_manager.has_room(&rid));

    // The producer on the owner pod.
    let mut a = connect(&app1, &tenant.admin.access_token).await;
    join(&mut a, &room_id, None).await;
    let a_video = produce(&mut a, &room_id, "video", 17_001).await;

    // The viewer through the other pod: join, consume and view all forwarded.
    let mut b = connect(&app2, &tenant.member.access_token).await;
    let (caps, transport) = join(&mut b, &room_id, Some(&["view"])).await;
    assert_eq!(levers(&transport)["view"], true, "{transport}");
    assert!(
        !conf(&app2).room_manager.has_room(&rid),
        "no second island: the owner served the join"
    );
    consume(&mut b, &room_id, &a_video, &caps).await;
    assert!(
        consumer_of(&app1, &room_id, &b.conn, &a_video).paused,
        "the owner pod holds B's consumer, created paused"
    );

    view(&mut b, &room_id, 1, &[&a_video]).await;
    wait_for(
        "the forwarded view resumes the consumer on the owner pod",
        || !consumer_of(&app1, &room_id, &b.conn, &a_video).paused,
    )
    .await;
    view(&mut b, &room_id, 2, &[]).await;
    wait_for("the forwarded empty view pauses it again", || {
        consumer_of(&app1, &room_id, &b.conn, &a_video).paused
    })
    .await;

    a.ws.close(None).await.ok();
    b.ws.close(None).await.ok();
}

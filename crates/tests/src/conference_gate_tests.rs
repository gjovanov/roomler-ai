// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The `media:*` namespace admits a connection only while it is a participant
//! of the room's call.
//!
//! Being a participant is what REST `call/join` establishes: a member of the
//! room's tenant, allowed in the room by its visibility, holding an open call
//! session. `media:join` requires all three for THIS connection, and every
//! other `media:*` command requires that this connection joined that room's
//! media. Each test reads both sides: what reached the caller's socket, and
//! what reached the people already in the call.
//!
//! Ordering: a socket's frames are handled one at a time, and everything a
//! frame causes is written to the sockets it reaches before the next frame is
//! read. So the `pong` that answers a `ping` arrives after everything the
//! frames before it caused on that socket ([`settle`]), and a test reads up to
//! it before it asserts that something did NOT arrive.

use std::time::Duration;

use bson::oid::ObjectId;
use futures::{SinkExt, StreamExt};
use reqwest::multipart;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use roomler_ai_mod_conference::ConferenceState;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A user socket and the connection id the server minted for it.
struct Peer {
    ws: Ws,
    conn: String,
}

/// The frames that admit a socket to a call or carry the call's media, its
/// participants or its files. None may reach someone who is not in the call.
const IN_CALL: [&str; 7] = [
    "media:router_capabilities",
    "media:transport_created",
    "media:new_producer",
    "media:consumer_created",
    "media:produce_result",
    "media:audio_playback",
    "media:peer_left",
];

fn in_call(frames: &[Value]) -> Vec<&Value> {
    frames
        .iter()
        .filter(|f| IN_CALL.contains(&f["type"].as_str().unwrap_or("")))
        .collect()
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

/// Everything this socket received up to the answer to a `ping` sent now:
/// all that its earlier frames caused (see the module doc). The `pong` goes to
/// every socket of the user, so a test pings only the socket it reads.
async fn settle(peer: &mut Peer) -> Vec<Value> {
    send(peer, "ping", Value::Null).await;
    frames_until(peer, "pong").await
}

/// `call/{action}` as `token`, naming `connection_id` when given (what the
/// web client sends). Returns the status.
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

/// `media:join` on a socket the call admits; returns the router capabilities
/// (to consume with) and the frames the join brought.
async fn join_media(peer: &mut Peer, room_id: &str) -> (Value, Vec<Value>) {
    send(peer, "media:join", json!({ "room_id": room_id })).await;
    let mut frames = frames_until(peer, "media:router_capabilities").await;
    let caps = frames.last().unwrap()["data"]["rtp_capabilities"].clone();
    frames.extend(frames_until(peer, "media:transport_created").await);
    (caps, frames)
}

/// Parameters the worker accepts for a producer on a transport that never
/// DTLS-connects: the router's VP8 and Opus, one SSRC each.
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

fn produce_frame(room_id: &str, kind: &str, ssrc: u32) -> Value {
    json!({
        "room_id": room_id,
        "kind": kind,
        "rtp_parameters": producer_params(kind, ssrc),
        "source": if kind == "audio" { "audio" } else { "camera" },
    })
}

/// Produce `kind` on an admitted socket and return the producer id.
async fn produce(peer: &mut Peer, room_id: &str, kind: &str, ssrc: u32) -> String {
    send(peer, "media:produce", produce_frame(room_id, kind, ssrc)).await;
    let frames = frames_until(peer, "media:produce_result").await;
    frames.last().unwrap()["data"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("produce_result carries an id: {frames:?}"))
        .to_string()
}

/// Consume `producer_id` on an admitted socket; returns `media:consumer_created`.
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

/// Upload a small file into `room_id` as `token`; returns its id.
async fn upload_to_room(
    app: &TestApp,
    tenant_id: &str,
    room_id: &str,
    token: &str,
    name: &str,
) -> String {
    let part = multipart::Part::bytes(b"a short jingle".to_vec())
        .file_name(name.to_string())
        .mime_str("text/plain")
        .unwrap();
    let form = multipart::Form::new()
        .part("file", part)
        .text("room_id", room_id.to_string());
    let resp = app
        .client
        .post(app.url(&format!("/api/tenant/{tenant_id}/file/upload")))
        .header("Authorization", format!("Bearer {token}"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 200, "upload of {name}: {body}");
    body["id"]
        .as_str()
        .expect("an uploaded file id")
        .to_string()
}

/// A call in progress: the org's admin started it in `room`, joined it over
/// REST naming its socket, joined its media and sends a camera and a
/// microphone.
struct LiveCall {
    tenant: SeededTenant,
    room: String,
    host: Peer,
    caps: Value,
    video: String,
    audio: String,
}

async fn start_live_call(app: &TestApp, tenant: SeededTenant, room: String) -> LiveCall {
    let (tid, token) = (tenant.tenant_id.clone(), tenant.admin.access_token.clone());
    let mut host = connect(app, &token).await;
    assert_eq!(call(app, &tid, &room, &token, "start", None).await, 200);
    assert_eq!(
        call(app, &tid, &room, &token, "join", Some(&host.conn)).await,
        200
    );
    let (caps, _) = join_media(&mut host, &room).await;
    let video = produce(&mut host, &room, "video", 31_001).await;
    let audio = produce(&mut host, &room, "audio", 31_002).await;
    LiveCall {
        tenant,
        room,
        host,
        caps,
        video,
        audio,
    }
}

async fn live_call(app: &TestApp, slug: &str) -> LiveCall {
    let tenant = app.seed_tenant(slug).await;
    let room = tenant.rooms[0].id.clone();
    start_live_call(app, tenant, room).await
}

/// Every `media:*` command a socket could send into `live`'s call: join it,
/// consume the host's camera, send a camera of its own, play one of the
/// room's files and stop a playback.
async fn try_every_command(peer: &mut Peer, live: &LiveCall, file_id: &str) {
    let room = live.room.as_str();
    send(peer, "media:join", json!({ "room_id": room })).await;
    send(
        peer,
        "media:consume",
        json!({ "room_id": room, "producer_id": live.video, "rtp_capabilities": live.caps }),
    )
    .await;
    send(peer, "media:produce", produce_frame(room, "video", 41_001)).await;
    send(
        peer,
        "media:play_audio",
        json!({ "room_id": room, "file_id": file_id }),
    )
    .await;
    send(
        peer,
        "media:stop_audio",
        json!({ "room_id": room, "playback_id": "" }),
    )
    .await;
}

/// Neither the refused socket nor anyone in the call saw anything of the
/// other, and the room's media holds nothing for the refused connection.
async fn assert_kept_out(app: &TestApp, live: &mut LiveCall, peer: &mut Peer, who: &str) {
    let theirs = settle(peer).await;
    assert!(
        in_call(&theirs).is_empty(),
        "{who} got into the call: {:?}",
        in_call(&theirs)
    );
    let host = settle(&mut live.host).await;
    assert!(
        in_call(&host).is_empty(),
        "{who} reached the people in the call: {:?}",
        in_call(&host)
    );
    let rm = &conf(app).room_manager;
    let rid = oid(&live.room);
    assert_eq!(
        rm.get_connection_room(&peer.conn),
        None,
        "{who} holds a seat in the room's media"
    );
    assert!(
        rm.consumer_states(&rid, &peer.conn).is_empty(),
        "{who} consumes the call's media"
    );
    assert_eq!(
        rm.get_participant_user_ids(&rid).len(),
        1,
        "only the host is in the call's media"
    );
}

async fn set_visibility(app: &TestApp, t: &SeededTenant, room_id: &str, visibility: &str) {
    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/room/{}", t.tenant_id, room_id),
            &t.admin.access_token,
        )
        .json(&json!({ "visibility": visibility }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "setting visibility={visibility} failed: {}",
        resp.status()
    );
}

/// A signed-in account of another org, holding a live call's room id, gets
/// nothing from any `media:*` command: no transports, no TURN credential, no
/// producer list, no consumer, no producer of its own in the call, and no
/// file playback started or stopped in it.
#[tokio::test]
async fn another_orgs_account_gets_nothing_from_a_live_call() {
    let app = TestApp::spawn().await;
    let mut live = live_call(&app, "mgate-org").await;
    let file = upload_to_room(
        &app,
        &live.tenant.tenant_id,
        &live.room,
        &live.tenant.admin.access_token,
        "board-minutes.txt",
    )
    .await;

    let outsider = app.seed_tenant("mgate-org-out").await;
    let mut x = connect(&app, &outsider.admin.access_token).await;
    try_every_command(&mut x, &live, &file).await;
    assert_kept_out(&app, &mut live, &mut x, "another org's account").await;
}

/// A member of the org who may not read a Private room is refused its call
/// over REST — and gets nothing from the socket either.
#[tokio::test]
async fn a_member_outside_a_private_room_gets_nothing_from_its_call() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("mgate-priv").await;
    let room = tenant.rooms[0].id.clone();
    // The admin is added as a member when the room closes; the member is not.
    set_visibility(&app, &tenant, &room, "private").await;
    let mut live = start_live_call(&app, tenant, room).await;
    let file = upload_to_room(
        &app,
        &live.tenant.tenant_id,
        &live.room,
        &live.tenant.admin.access_token,
        "private-notes.txt",
    )
    .await;

    let token = live.tenant.member.access_token.clone();
    let mut m = connect(&app, &token).await;
    assert_eq!(
        call(
            &app,
            &live.tenant.tenant_id,
            &live.room,
            &token,
            "join",
            Some(&m.conn)
        )
        .await,
        403,
        "the REST join refuses a member outside the private room"
    );
    try_every_command(&mut m, &live, &file).await;
    assert_kept_out(&app, &mut live, &mut m, "a member outside the private room").await;
}

/// A member who may be in the room but has not joined the call gets nothing
/// from its media; a call session opened for another of its sockets does not
/// admit this one; the session opened for this socket does.
#[tokio::test]
async fn a_member_gets_the_calls_media_only_after_joining_the_call_on_that_socket() {
    let app = TestApp::spawn().await;
    let mut live = live_call(&app, "mgate-session").await;
    let (tid, room) = (live.tenant.tenant_id.clone(), live.room.clone());
    let file = upload_to_room(
        &app,
        &tid,
        &room,
        &live.tenant.admin.access_token,
        "agenda.txt",
    )
    .await;
    let token = live.tenant.member.access_token.clone();

    let mut m = connect(&app, &token).await;
    try_every_command(&mut m, &live, &file).await;
    assert_kept_out(&app, &mut live, &mut m, "a member not in the call").await;

    // Joined over REST, but naming ANOTHER socket of the same account.
    let other = connect(&app, &token).await;
    assert_eq!(
        call(&app, &tid, &room, &token, "join", Some(&other.conn)).await,
        200
    );
    try_every_command(&mut m, &live, &file).await;
    assert_kept_out(
        &app,
        &mut live,
        &mut m,
        "a socket whose account joined the call on another socket",
    )
    .await;

    // The session for THIS socket is what was missing.
    assert_eq!(
        call(&app, &tid, &room, &token, "join", Some(&m.conn)).await,
        200
    );
    let (caps, _) = join_media(&mut m, &room).await;
    consume(&mut m, &room, &live.video, &caps).await;
    assert_eq!(
        conf(&app).room_manager.get_connection_room(&m.conn),
        Some(oid(&room))
    );
    drop(other);
}

/// The participant path, end to end: a member joins over REST and then the
/// media, hears the host's producers, consumes the camera, sends its own, a
/// room file plays for both, a stop reaches both, and leaving tells the host.
#[tokio::test]
async fn participants_join_produce_consume_play_and_leave() {
    let app = TestApp::spawn().await;
    let mut live = live_call(&app, "mgate-ok").await;
    let (tid, room) = (live.tenant.tenant_id.clone(), live.room.clone());
    let file = upload_to_room(
        &app,
        &tid,
        &room,
        &live.tenant.admin.access_token,
        "standup-jingle.txt",
    )
    .await;
    let token = live.tenant.member.access_token.clone();
    let member_id = live.tenant.member.id.clone();

    let mut b = connect(&app, &token).await;
    assert_eq!(
        call(&app, &tid, &room, &token, "join", Some(&b.conn)).await,
        200
    );
    let (caps, mut frames) = join_media(&mut b, &room).await;
    frames.extend(settle(&mut b).await);
    let announced: Vec<&str> = frames
        .iter()
        .filter(|f| f["type"] == "media:new_producer")
        .filter_map(|f| f["data"]["producer_id"].as_str())
        .collect();
    assert!(
        announced.contains(&live.video.as_str()) && announced.contains(&live.audio.as_str()),
        "the join announces the host's camera and microphone: {frames:?}"
    );

    consume(&mut b, &room, &live.video, &caps).await;

    let b_video = produce(&mut b, &room, "video", 51_001).await;
    let heard = next_of(&mut live.host, "media:new_producer").await;
    assert_eq!(heard["data"]["producer_id"], b_video.as_str(), "{heard}");
    assert_eq!(heard["data"]["user_id"], member_id.as_str(), "{heard}");

    send(
        &mut b,
        "media:play_audio",
        json!({ "room_id": room, "file_id": file }),
    )
    .await;
    for peer in [&mut b, &mut live.host] {
        let start = next_of(peer, "media:audio_playback").await;
        assert_eq!(start["data"]["action"], "start", "{start}");
        assert_eq!(start["data"]["filename"], "standup-jingle.txt", "{start}");
    }

    send(
        &mut live.host,
        "media:stop_audio",
        json!({ "room_id": room, "playback_id": "" }),
    )
    .await;
    for peer in [&mut live.host, &mut b] {
        let stop = next_of(peer, "media:audio_playback").await;
        assert_eq!(stop["data"]["action"], "stop", "{stop}");
    }

    send(&mut b, "media:leave", json!({ "room_id": room })).await;
    let left = next_of(&mut live.host, "media:peer_left").await;
    assert_eq!(left["data"]["connection_id"], b.conn.as_str(), "{left}");
    assert_eq!(
        conf(&app).room_manager.get_connection_room(&b.conn),
        None,
        "the leave closed the member's media"
    );
}

/// A participant plays only one of this room's live files, and each playback
/// has an id of its own: a file of another room of the same org, or one that
/// was removed, plays nowhere.
#[tokio::test]
async fn a_playback_is_one_of_this_rooms_live_files_with_its_own_id() {
    let app = TestApp::spawn().await;
    let mut live = live_call(&app, "mgate-play").await;
    let (tid, room) = (live.tenant.tenant_id.clone(), live.room.clone());
    let token = live.tenant.admin.access_token.clone();
    let other_room = live.tenant.rooms[1].id.clone();

    let here = upload_to_room(&app, &tid, &room, &token, "here.txt").await;
    let elsewhere = upload_to_room(&app, &tid, &other_room, &token, "elsewhere.txt").await;
    let removed = upload_to_room(&app, &tid, &room, &token, "removed.txt").await;
    let resp = app
        .auth_delete(&format!("/api/tenant/{tid}/file/{removed}"), &token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the file is removed");

    for file in [&elsewhere, &removed] {
        send(
            &mut live.host,
            "media:play_audio",
            json!({ "room_id": room, "file_id": file }),
        )
        .await;
    }
    let frames = settle(&mut live.host).await;
    let played: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "media:audio_playback")
        .collect();
    assert!(
        played.is_empty(),
        "another room's file, or a removed one, played: {played:?}"
    );

    send(
        &mut live.host,
        "media:play_audio",
        json!({ "room_id": room, "file_id": here }),
    )
    .await;
    let start = next_of(&mut live.host, "media:audio_playback").await;
    assert_eq!(start["data"]["action"], "start", "{start}");
    assert_eq!(start["data"]["filename"], "here.txt", "{start}");
    let id = start["data"]["playback_id"].as_str().unwrap_or("");
    assert!(!id.is_empty(), "a playback has an id of its own: {start}");
}

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-127 — the user socket as the media tests drive it: a [`Peer`] with the
//! connection id the server minted, and the `media:*` round trips (join,
//! produce, consume, view, the owner's pause) against a real mediasoup
//! worker. Lifted out of `conference_view_tests` (P1) when
//! `conference_simulcast_tests` (P2) needed the same socket.
//!
//! `media:view` has no reply, so a state that must CHANGE is polled for
//! ([`wait_for`]), and a state that must NOT change is read after a
//! [`barrier`]: a `media:consume` with an invalid producer id, whose
//! `media:error` reply proves every earlier frame on that socket was handled
//! (the WS loop awaits each message in turn). The consumers' flags and layers
//! are read straight off the owner pod's room manager as plain data
//! ([`consumer_of`]), and — P2 — off the worker itself ([`dump_layers`]).

use std::time::Duration;

use bson::oid::ObjectId;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use roomler_ai_mod_conference::ConferenceState;
use roomler_ai_mod_conference::media::room_manager::{ConsumerLayersDump, ConsumerSnapshot};

pub type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A user socket and the connection id the server minted for it.
pub struct Peer {
    pub ws: Ws,
    pub conn: String,
}

pub fn conf(app: &TestApp) -> &ConferenceState {
    app.state
        .modules
        .conference
        .as_ref()
        .expect("the conference module is mounted in tests")
}

pub fn oid(hex: &str) -> ObjectId {
    ObjectId::parse_str(hex).unwrap()
}

pub async fn connect(app: &TestApp, token: &str) -> Peer {
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

pub async fn send(peer: &mut Peer, msg_type: &str, data: Value) {
    peer.ws
        .send(Message::Text(
            json!({ "type": msg_type, "data": data }).to_string().into(),
        ))
        .await
        .unwrap();
}

/// Every frame up to and including the first of type `kind`, within 10 s.
pub async fn frames_until(peer: &mut Peer, kind: &str) -> Vec<Value> {
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

pub async fn next_of(peer: &mut Peer, kind: &str) -> Value {
    frames_until(peer, kind).await.pop().unwrap()
}

/// `media:join`, with or without `caps`; returns the router capabilities
/// (to consume with) and the `media:transport_created` frame.
pub async fn join(peer: &mut Peer, room_id: &str, caps: Option<&[&str]>) -> (Value, Value) {
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
pub fn producer_params(kind: &str, ssrc: u32) -> Value {
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

/// FR-127 P2 — a camera published as a ladder: VP8 with one SSRC-keyed
/// encoding per rung, lowest first, each `L1T3`, the way mediasoup-client
/// sends it (by `rid` there; SSRCs do the same job on a transport that never
/// connects). The worker makes a `Simulcast` producer of it.
pub fn simulcast_params(ssrcs: &[u32]) -> Value {
    let encodings: Vec<Value> = ssrcs
        .iter()
        .map(|ssrc| json!({ "ssrc": ssrc, "scalabilityMode": "L1T3" }))
        .collect();
    json!({
        "codecs": [{
            "mimeType": "video/VP8", "payloadType": 96, "clockRate": 90000,
            "parameters": {}, "rtcpFeedback": []
        }],
        "headerExtensions": [],
        "encodings": encodings,
        "rtcp": { "cname": format!("cname-{}", ssrcs[0]), "reducedSize": true }
    })
}

/// Produce `kind` with one encoding and return the producer id the server minted.
pub async fn produce(peer: &mut Peer, room_id: &str, kind: &str, ssrc: u32) -> String {
    let source = if kind == "audio" { "audio" } else { "camera" };
    produce_with(
        peer,
        room_id,
        kind,
        source,
        producer_params(kind, ssrc),
        None,
    )
    .await
}

/// Produce with the given parameters, source label and (P2) capture size.
pub async fn produce_with(
    peer: &mut Peer,
    room_id: &str,
    kind: &str,
    source: &str,
    rtp_parameters: Value,
    capture: Option<(u32, u32)>,
) -> String {
    let mut data = json!({
        "room_id": room_id,
        "kind": kind,
        "rtp_parameters": rtp_parameters,
        "source": source,
    });
    if let Some((w, h)) = capture {
        data["capture"] = json!({ "w": w, "h": h });
    }
    send(peer, "media:produce", data).await;
    let frames = frames_until(peer, "media:produce_result").await;
    let result = frames.last().unwrap();
    result["data"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("produce_result carries an id: {frames:?}"))
        .to_string()
}

/// Consume `producer_id` and return the `media:consumer_created` frame.
pub async fn consume(peer: &mut Peer, room_id: &str, producer_id: &str, caps: &Value) -> Value {
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

/// A view naming `producers` as 320×240 tiles, none spotlit.
pub async fn view(peer: &mut Peer, room_id: &str, seq: u64, producers: &[&str]) {
    let tiles: Vec<(&str, u32, u32, bool)> =
        producers.iter().map(|p| (*p, 320, 240, false)).collect();
    view_tiles(peer, room_id, seq, &tiles).await;
}

/// FR-127 P2 — a view with each tile's size in device pixels and its
/// spotlit flag (`spotlit` is sent only when true, as the client does).
pub async fn view_tiles(
    peer: &mut Peer,
    room_id: &str,
    seq: u64,
    tiles: &[(&str, u32, u32, bool)],
) {
    let video: Vec<Value> = tiles
        .iter()
        .map(|(p, w, h, spotlit)| {
            let mut t = json!({ "producer_id": p, "w": w, "h": h });
            if *spotlit {
                t["spotlit"] = json!(true);
            }
            t
        })
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
pub async fn barrier(peer: &mut Peer, room_id: &str, caps: &Value) -> Vec<Value> {
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

pub async fn producer_pause(peer: &mut Peer, room_id: &str, producer_id: &str, paused: bool) {
    send(
        peer,
        "media:producer_pause",
        json!({ "room_id": room_id, "producer_id": producer_id, "paused": paused }),
    )
    .await;
}

/// Poll `cond` every 25 ms for up to 5 s.
pub async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: not within 5 s"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub fn consumer_of(
    app: &TestApp,
    room_id: &str,
    conn: &str,
    producer_id: &str,
) -> ConsumerSnapshot {
    conf(app)
        .room_manager
        .consumer_states(&oid(room_id), conn)
        .into_iter()
        .find(|c| c.producer_id == producer_id)
        .unwrap_or_else(|| panic!("{conn} has a consumer of {producer_id}"))
}

/// FR-127 P2 — one consumer's layers as the WORKER holds them.
pub async fn dump_layers(
    app: &TestApp,
    room_id: &str,
    conn: &str,
    producer_id: &str,
) -> ConsumerLayersDump {
    conf(app)
        .room_manager
        .dump_consumer_layers(&oid(room_id), conn, producer_id)
        .await
        .expect("the worker answers the dump")
}

pub fn producer_paused(app: &TestApp, room_id: &str, conn: &str, producer_id: &str) -> bool {
    conf(app)
        .room_manager
        .producer_states(&oid(room_id), conn)
        .into_iter()
        .find(|p| p.producer_id == producer_id)
        .unwrap_or_else(|| panic!("{conn} owns producer {producer_id}"))
        .paused
}

/// A tenant with a started call in its first room, both users REST-joined.
pub async fn started_call(app: &TestApp, slug: &str) -> (SeededTenant, String) {
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

pub fn levers(transport: &Value) -> Value {
    transport["data"]["meeting"].clone()
}

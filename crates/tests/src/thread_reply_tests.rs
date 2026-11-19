// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! A reply's thread, and the message a reply quotes, are live messages of the
//! reply's own room. A thread lists the replies of its own room, and a mention
//! reaches the members of the room it is written in.
//!
//! A thread or a quote outside the path room answers exactly as a message
//! that does not exist: 404, nothing stored, the root untouched, nothing on
//! any socket. The refusal tests read the caller's socket up to the `pong`
//! that answers a `ping` sent after the response ([`settle`]): a request
//! writes everything it broadcasts before it answers, so whatever it sent
//! arrives before that `pong`.

use std::time::Duration;

use bson::{Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use roomler_ai_services::dao::message::MessageDao;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn oid(hex: &str) -> ObjectId {
    ObjectId::parse_str(hex).unwrap()
}

/// `POST …/room/{room_id}/message` as `token`; the status and the body.
async fn post(
    app: &TestApp,
    tenant_id: &str,
    room_id: &str,
    token: &str,
    body: Value,
) -> (u16, Value) {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{tenant_id}/room/{room_id}/message"),
            token,
        )
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// A message by the org's admin in its first room, carrying a marker no other
/// message has. Returns the message id and the marker.
async fn post_root(app: &TestApp, t: &SeededTenant) -> (String, String) {
    let marker = format!("root-{}", uuid::Uuid::new_v4().simple());
    let (status, body) = post(
        app,
        &t.tenant_id,
        &t.rooms[0].id,
        &t.admin.access_token,
        json!({ "content": marker }),
    )
    .await;
    assert_eq!(status, 200, "the root is posted: {body}");
    (body["id"].as_str().unwrap().to_string(), marker)
}

/// The stored message, as the database holds it.
async fn stored(app: &TestApp, id: &str) -> Document {
    app.db
        .collection::<Document>("messages")
        .find_one(doc! { "_id": oid(id) })
        .await
        .unwrap()
        .expect("the message is stored")
}

/// How many stored messages name `id` as their thread.
async fn replies_stored(app: &TestApp, id: &str) -> u64 {
    app.db
        .collection::<Document>("messages")
        .count_documents(doc! { "thread_id": oid(id) })
        .await
        .unwrap()
}

/// `GET …/message/{root}/thread` as `token`.
async fn thread(app: &TestApp, t: &SeededTenant, room_id: &str, root: &str, token: &str) -> Value {
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{room_id}/message/{root}/thread",
                t.tenant_id
            ),
            token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the thread lists");
    resp.json().await.unwrap()
}

/// A reply row naming `root` in the given org and room, written straight to
/// the database: the listing and the repair read stored rows, and the send
/// would refuse this one.
async fn insert_reply_row(app: &TestApp, tenant_id: &str, room_id: &str, author: &str, root: &str) {
    let now = bson::DateTime::now();
    app.db
        .collection::<Document>("messages")
        .insert_one(doc! {
            "tenant_id": oid(tenant_id),
            "room_id": oid(room_id),
            "thread_id": oid(root),
            "is_thread_root": false,
            "thread_metadata": bson::Bson::Null,
            "author_id": oid(author),
            "content": "a stored reply row",
            "referenced_message_id": bson::Bson::Null,
            "edited_at": bson::Bson::Null,
            "nonce": bson::Bson::Null,
            "created_at": now,
            "updated_at": now,
            "deleted_at": bson::Bson::Null,
        })
        .await
        .unwrap();
}

async fn join(app: &TestApp, t: &SeededTenant, room_id: &str, token: &str) {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room/{room_id}/join", t.tenant_id),
            token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "join: {}", resp.status());
}

async fn notifications_for(app: &TestApp, user_id: &str) -> u64 {
    app.db
        .collection::<Document>("notifications")
        .count_documents(doc! { "user_id": oid(user_id) })
        .await
        .unwrap()
}

async fn connect(app: &TestApp, token: &str) -> Ws {
    let url = format!("ws://{}/ws?token={token}", app.addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("the user socket opens");
    let first = ws.next().await.expect("a frame").expect("a text frame");
    let parsed: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "connected", "{parsed}");
    ws
}

/// Every frame this socket received up to the answer to a `ping` sent now.
async fn settle(ws: &mut Ws) -> Vec<Value> {
    ws.send(WsMessage::Text(
        json!({ "type": "ping", "data": null }).to_string().into(),
    ))
    .await
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    loop {
        let left = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_else(|| panic!("no pong within 10 s; saw {seen:?}"));
        let frame = match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(frame))) => frame,
            other => panic!("the socket ended before its pong: {other:?}; saw {seen:?}"),
        };
        let Ok(text) = frame.to_text() else { continue };
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if v["type"] == "pong" {
            return seen;
        }
        seen.push(v);
    }
}

/// What a reply naming a thread that never existed gets, in the same room as
/// the same caller: the answer every refusal here must be indistinguishable
/// from.
async fn missing_thread_answer(
    app: &TestApp,
    tenant_id: &str,
    room_id: &str,
    token: &str,
) -> (u16, Value) {
    let never = ObjectId::new().to_hex();
    post(
        app,
        tenant_id,
        room_id,
        token,
        json!({ "content": "a reply", "thread_id": never }),
    )
    .await
}

/// No chat frame at all, and nothing that carries the root's marker.
fn assert_quiet(frames: &[Value], marker: &str) {
    let chat: Vec<&Value> = frames
        .iter()
        .filter(|f| {
            f["type"]
                .as_str()
                .is_some_and(|t| t.starts_with("message:"))
        })
        .collect();
    assert!(chat.is_empty(), "a refused reply sends nothing: {chat:?}");
    assert!(
        !frames.iter().any(|f| f.to_string().contains(marker)),
        "the root's content reaches no socket: {frames:?}"
    );
}

#[tokio::test]
async fn a_replys_thread_is_a_message_of_its_own_org() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("thrown1").await;
    let b = app.seed_tenant("thrown2").await;
    let (root, marker) = post_root(&app, &a).await;
    let before = stored(&app, &root).await;

    let mut ws = connect(&app, &b.admin.access_token).await;
    let (status, body) = post(
        &app,
        &b.tenant_id,
        &b.rooms[0].id,
        &b.admin.access_token,
        json!({ "content": "a reply", "thread_id": root }),
    )
    .await;

    assert_eq!(status, 404, "{body}");
    assert_eq!(stored(&app, &root).await, before, "the root is untouched");
    assert_eq!(replies_stored(&app, &root).await, 0, "nothing is stored");
    assert_quiet(&settle(&mut ws).await, &marker);
    let listed = thread(&app, &a, &a.rooms[0].id, &root, &a.admin.access_token).await;
    assert_eq!(listed["total"], 0, "{listed}");
    let missing =
        missing_thread_answer(&app, &b.tenant_id, &b.rooms[0].id, &b.admin.access_token).await;
    assert_eq!((status, body), missing, "the answer to a missing thread");
}

#[tokio::test]
async fn a_replys_thread_is_a_message_of_its_own_room() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("throwr1").await;
    let (root, marker) = post_root(&app, &a).await;
    let before = stored(&app, &root).await;

    // The admin is a member of both rooms: the other room is theirs too.
    let mut ws = connect(&app, &a.admin.access_token).await;
    let (status, body) = post(
        &app,
        &a.tenant_id,
        &a.rooms[1].id,
        &a.admin.access_token,
        json!({ "content": "a reply", "thread_id": root }),
    )
    .await;

    assert_eq!(status, 404, "{body}");
    assert_eq!(stored(&app, &root).await, before, "the root is untouched");
    assert_eq!(replies_stored(&app, &root).await, 0, "nothing is stored");
    assert_quiet(&settle(&mut ws).await, &marker);
    let listed = thread(&app, &a, &a.rooms[0].id, &root, &a.admin.access_token).await;
    assert_eq!(listed["total"], 0, "{listed}");
    let missing =
        missing_thread_answer(&app, &a.tenant_id, &a.rooms[1].id, &a.admin.access_token).await;
    assert_eq!((status, body), missing, "the answer to a missing thread");
}

#[tokio::test]
async fn a_replys_thread_is_a_live_message() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("thrlive1").await;
    let (root, marker) = post_root(&app, &a).await;
    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/room/{}/message/{root}",
                a.tenant_id, a.rooms[0].id
            ),
            &a.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the root is deleted");
    let before = stored(&app, &root).await;

    let mut ws = connect(&app, &a.admin.access_token).await;
    let (status, body) = post(
        &app,
        &a.tenant_id,
        &a.rooms[0].id,
        &a.admin.access_token,
        json!({ "content": "a reply", "thread_id": root }),
    )
    .await;

    assert_eq!(status, 404, "{body}");
    assert_eq!(stored(&app, &root).await, before, "the root is untouched");
    assert_eq!(replies_stored(&app, &root).await, 0, "nothing is stored");
    assert_quiet(&settle(&mut ws).await, &marker);
    let missing =
        missing_thread_answer(&app, &a.tenant_id, &a.rooms[0].id, &a.admin.access_token).await;
    assert_eq!((status, body), missing, "the answer to a missing thread");
}

#[tokio::test]
async fn a_quoted_message_is_a_live_message_of_its_own_room() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("thrquo1").await;
    let b = app.seed_tenant("thrquo2").await;
    let (root, _) = post_root(&app, &a).await;
    let quoting = json!({ "content": "a quote", "referenced_message_id": root });

    let (status, body) = post(
        &app,
        &b.tenant_id,
        &b.rooms[0].id,
        &b.admin.access_token,
        quoting.clone(),
    )
    .await;
    assert_eq!(status, 404, "another org's message: {body}");
    let (status, body) = post(
        &app,
        &a.tenant_id,
        &a.rooms[1].id,
        &a.admin.access_token,
        quoting.clone(),
    )
    .await;
    assert_eq!(status, 404, "another room's message: {body}");

    let (status, body) = post(
        &app,
        &a.tenant_id,
        &a.rooms[0].id,
        &a.admin.access_token,
        quoting.clone(),
    )
    .await;
    assert_eq!(status, 200, "a message of its own room: {body}");
    assert_eq!(body["referenced_message_id"], root.as_str(), "{body}");

    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/room/{}/message/{root}",
                a.tenant_id, a.rooms[0].id
            ),
            &a.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the root is deleted");
    let (status, body) = post(
        &app,
        &a.tenant_id,
        &a.rooms[0].id,
        &a.admin.access_token,
        quoting,
    )
    .await;
    assert_eq!(status, 404, "a deleted message: {body}");

    let quotes = app
        .db
        .collection::<Document>("messages")
        .count_documents(doc! { "referenced_message_id": oid(&root) })
        .await
        .unwrap();
    assert_eq!(quotes, 1, "only the quote in its own room is stored");
}

#[tokio::test]
async fn a_reply_in_its_own_room_is_counted_listed_and_broadcast() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("throk1").await;
    let (root, _) = post_root(&app, &a).await;
    join(&app, &a, &a.rooms[0].id, &a.member.access_token).await;

    let mut ws = connect(&app, &a.admin.access_token).await;
    let (status, reply) = post(
        &app,
        &a.tenant_id,
        &a.rooms[0].id,
        &a.member.access_token,
        json!({ "content": "an in-room reply", "thread_id": root }),
    )
    .await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["thread_id"], root.as_str(), "{reply}");
    let reply_id = reply["id"].as_str().unwrap();

    let parent = stored(&app, &root).await;
    assert_eq!(parent.get_bool("is_thread_root"), Ok(true), "{parent}");
    let meta = parent.get_document("thread_metadata").expect("{parent}");
    assert_eq!(meta.get_i32("reply_count"), Ok(1), "{parent}");
    let participants = meta.get_array("participant_ids").unwrap();
    assert_eq!(
        participants,
        &vec![bson::Bson::ObjectId(oid(&a.member.id))],
        "{parent}"
    );

    let listed = thread(&app, &a, &a.rooms[0].id, &root, &a.admin.access_token).await;
    assert_eq!(listed["total"], 1, "{listed}");
    assert_eq!(listed["items"][0]["id"], reply_id, "{listed}");

    let frames = settle(&mut ws).await;
    assert!(
        frames
            .iter()
            .any(|f| f["type"] == "message:create" && f["data"]["id"] == reply_id),
        "the room hears the reply: {frames:?}"
    );
    let update = frames
        .iter()
        .find(|f| f["type"] == "message:update" && f["data"]["id"] == root.as_str())
        .unwrap_or_else(|| panic!("the room hears the root's new count: {frames:?}"));
    assert_eq!(update["data"]["reply_count"], 1, "{update}");
    assert_eq!(update["data"]["is_thread_root"], true, "{update}");
}

#[tokio::test]
async fn a_threads_replies_are_those_of_its_own_room() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("thrlist1").await;
    let b = app.seed_tenant("thrlist2").await;
    let (root, _) = post_root(&app, &a).await;
    let (status, kept) = post(
        &app,
        &a.tenant_id,
        &a.rooms[0].id,
        &a.admin.access_token,
        json!({ "content": "kept", "thread_id": root }),
    )
    .await;
    assert_eq!(status, 200, "{kept}");

    insert_reply_row(&app, &b.tenant_id, &b.rooms[0].id, &b.admin.id, &root).await;
    insert_reply_row(&app, &a.tenant_id, &a.rooms[1].id, &a.admin.id, &root).await;

    let listed = thread(&app, &a, &a.rooms[0].id, &root, &a.admin.access_token).await;
    assert_eq!(listed["total"], 1, "{listed}");
    assert_eq!(listed["items"][0]["id"], kept["id"], "{listed}");
}

#[tokio::test]
async fn the_thread_repair_counts_only_replies_of_the_roots_room() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("thrfix1").await;
    let b = app.seed_tenant("thrfix2").await;
    let (root, _) = post_root(&app, &a).await;
    let (status, body) = post(
        &app,
        &a.tenant_id,
        &a.rooms[0].id,
        &a.admin.access_token,
        json!({ "content": "counted", "thread_id": root }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    insert_reply_row(&app, &b.tenant_id, &b.rooms[0].id, &b.admin.id, &root).await;
    insert_reply_row(&app, &a.tenant_id, &a.rooms[1].id, &a.member.id, &root).await;

    MessageDao::new(&app.db)
        .rebuild_thread_metadata()
        .await
        .expect("the repair runs");

    let parent = stored(&app, &root).await;
    let meta = parent.get_document("thread_metadata").expect("{parent}");
    assert_eq!(meta.get_i32("reply_count"), Ok(1), "{parent}");
    assert_eq!(
        meta.get_array("participant_ids").unwrap(),
        &vec![bson::Bson::ObjectId(oid(&a.admin.id))],
        "{parent}"
    );
}

#[tokio::test]
async fn a_mention_reaches_only_members_of_the_room() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("thrmen1").await;
    let b = app.seed_tenant("thrmen2").await;
    // The org's member has not joined the room; the other org's admin is in
    // another org altogether.
    let mentioning = json!({
        "content": "hello there",
        "mentions": {
            "users": [a.member.id, b.admin.id],
            "everyone": false,
            "here": false,
        },
    });

    let (status, body) = post(
        &app,
        &a.tenant_id,
        &a.rooms[0].id,
        &a.admin.access_token,
        mentioning.clone(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(notifications_for(&app, &a.member.id).await, 0);
    assert_eq!(notifications_for(&app, &b.admin.id).await, 0);

    join(&app, &a, &a.rooms[0].id, &a.member.access_token).await;
    let (status, body) = post(
        &app,
        &a.tenant_id,
        &a.rooms[0].id,
        &a.admin.access_token,
        mentioning,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        notifications_for(&app, &a.member.id).await,
        1,
        "a member of the room is reached"
    );
    assert_eq!(notifications_for(&app, &b.admin.id).await, 0);
}

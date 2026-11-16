// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3327 — a deleted message or file answers nothing, and the per-id lookup
//! answers live rows only.
//!
//! A `DELETE` that answered 200 leaves a tombstone: the row stays, with a
//! `deleted_at`. Every route keyed by its id then answers 404, as for an id
//! that names nothing, and nothing is written to it or sent about it. A write
//! that passed its route's guard before the delete landed matches nothing, so
//! a second delete keeps the first removal time. A row that never carried the
//! field is live: the predicate is `deleted_at: null`, never
//! `{"$type": "null"}`. A room that still carries a removal date, from before
//! a room's delete removed it outright, answers as a missing one too.

use std::time::Duration;

use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use reqwest::multipart;
use roomler_ai_db::models::ReactionSummary;
use roomler_ai_services::dao::{file::FileDao, message::MessageDao, recording::RecordingDao};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::fixtures::test_app::TestApp;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn oid(hex: &str) -> ObjectId {
    ObjectId::parse_str(hex).unwrap()
}

/// A server whose per-address rate limit is out of the way: each test sends
/// more requests than the default burst, and a 429 would read as a refusal.
async fn spawn() -> TestApp {
    TestApp::spawn_with_settings(|s| {
        s.app.rate_limit_per_sec = 1_000;
        s.app.rate_limit_burst = 10_000;
    })
    .await
}

/// Any method on `path` as `token`; the status and the body.
async fn call(
    app: &TestApp,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
    let mut req = app
        .client
        .request(method, app.url(path))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn message(app: &TestApp, tid: &str, rid: &str, token: &str, content: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/tenant/{tid}/room/{rid}/message"),
        token,
        Some(json!({ "content": content })),
    )
    .await;
    assert_eq!(status, 200, "message posted: {body}");
    body["id"].as_str().unwrap().to_string()
}

/// A file uploaded into `room_id` (`POST …/file/upload` naming the room).
async fn file(app: &TestApp, tid: &str, room_id: &str, token: &str, name: &str) -> String {
    let part = multipart::Part::bytes(format!("the bytes of {name}").into_bytes())
        .file_name(name.to_string())
        .mime_str("text/plain")
        .unwrap();
    let form = multipart::Form::new()
        .part("file", part)
        .text("room_id", room_id.to_string());
    let resp = app
        .client
        .post(app.url(&format!("/api/tenant/{tid}/file/upload")))
        .header("Authorization", format!("Bearer {token}"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 200, "upload of {name}: {body}");
    body["id"].as_str().unwrap().to_string()
}

/// A recording of `rid` started by `token`'s account.
async fn recording(app: &TestApp, tid: &str, rid: &str, token: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/tenant/{tid}/room/{rid}/recording"),
        token,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, 200, "a recording is started: {body}");
    body["id"].as_str().unwrap().to_string()
}

async fn join(app: &TestApp, tid: &str, rid: &str, token: &str) {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/tenant/{tid}/room/{rid}/join"),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "join: {body}");
}

async fn stored(app: &TestApp, collection: &str, id: &str) -> Document {
    app.db
        .collection::<Document>(collection)
        .find_one(doc! { "_id": oid(id) })
        .await
        .unwrap()
        .expect("the row is stored")
}

/// The row's `deleted_at`, which a delete has set.
async fn removal_time(app: &TestApp, collection: &str, id: &str) -> DateTime {
    let row = stored(app, collection, id).await;
    *row.get_datetime("deleted_at")
        .unwrap_or_else(|_| panic!("{collection} {id} carries a removal time: {row}"))
}

/// The emoji of every reaction stored for `mid`, sorted.
async fn reactions_of(app: &TestApp, mid: &str) -> Vec<String> {
    let mut cursor = app
        .db
        .collection::<Document>("reactions")
        .find(doc! { "message_id": oid(mid) })
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(row) = cursor.next().await {
        let row = row.unwrap();
        out.push(
            row.get_document("emoji")
                .and_then(|e| e.get_str("value"))
                .unwrap_or_default()
                .to_string(),
        );
    }
    out.sort();
    out
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
/// A route sends its fan-out before it answers, and a socket's frames go out
/// in order, so what the requests before this call sent is in front of the
/// pong.
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

fn of_type<'a>(frames: &'a [Value], prefix: &str) -> Vec<&'a Value> {
    frames
        .iter()
        .filter(|f| f["type"].as_str().is_some_and(|t| t.starts_with(prefix)))
        .collect()
}

/// The ids of the org's files as `token` lists them (`GET …/file`).
async fn org_file_ids(app: &TestApp, tid: &str, token: &str) -> Vec<String> {
    let (status, body) = call(app, "GET", &format!("/api/tenant/{tid}/file"), token, None).await;
    assert_eq!(status, 200, "the org's file list: {body}");
    ids(&body["items"])
}

/// The ids of the messages a search for `word` finds as `token`.
async fn searched_message_ids(app: &TestApp, tid: &str, token: &str, word: &str) -> Vec<String> {
    let (status, body) = call(
        app,
        "GET",
        &format!("/api/tenant/{tid}/search?q={word}"),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "search: {body}");
    ids(&body["messages"])
}

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap_or_else(|| panic!("a list: {list}"))
        .iter()
        .map(|x| x["id"].as_str().unwrap().to_string())
        .collect()
}

/// After a message's `DELETE` answered 200, every route keyed by its id
/// answers 404: an edit, a reaction added and one removed, the thread, a pin,
/// an unpin and a second delete. Nothing is written to the tombstone, no
/// reaction comes or goes, and the room hears nothing more about it.
#[tokio::test]
async fn a_deleted_message_answers_404_on_every_route_and_the_room_hears_nothing() {
    let app = spawn().await;
    let t = app.seed_tenant("tombmsg").await;
    let (tid, rid) = (t.tenant_id.clone(), t.rooms[0].id.clone());
    let admin = t.admin.access_token.clone();
    join(&app, &tid, &rid, &t.member.access_token).await;
    let mid = message(&app, &tid, &rid, &admin, "a note to remove").await;
    let path = format!("/api/tenant/{tid}/room/{rid}/message/{mid}");
    // A reaction and a pin before the delete, so that the remove and the
    // unpin below would each have something to undo.
    let (status, body) = call(
        &app,
        "POST",
        &format!("{path}/reaction"),
        &admin,
        Some(json!({ "emoji": "thumbsup" })),
    )
    .await;
    assert_eq!(status, 200, "a reaction on the live message: {body}");
    let (status, body) = call(
        &app,
        "PUT",
        &format!("{path}/pin"),
        &admin,
        Some(json!({ "pinned": true })),
    )
    .await;
    assert_eq!(status, 200, "a pin of the live message: {body}");

    let mut member_ws = connect(&app, &t.member.access_token).await;
    let (status, body) = call(&app, "DELETE", &path, &admin, None).await;
    assert_eq!(status, 200, "the delete: {body}");
    let frames = settle(&mut member_ws).await;
    assert_eq!(
        of_type(&frames, "message:delete").len(),
        1,
        "the room hears the delete once: {frames:?}"
    );
    let before = stored(&app, "messages", &mid).await;
    assert!(
        matches!(before.get("deleted_at"), Some(Bson::DateTime(_))),
        "the delete left a tombstone: {before}"
    );
    let reactions_before = reactions_of(&app, &mid).await;
    assert_eq!(reactions_before, vec!["thumbsup".to_string()]);

    let attempts = [
        ("PUT", path.clone(), Some(json!({ "content": "an edit" }))),
        (
            "POST",
            format!("{path}/reaction"),
            Some(json!({ "emoji": "heart" })),
        ),
        ("DELETE", format!("{path}/reaction/thumbsup"), None),
        ("GET", format!("{path}/thread"), None),
        (
            "PUT",
            format!("{path}/pin"),
            Some(json!({ "pinned": true })),
        ),
        (
            "PUT",
            format!("{path}/pin"),
            Some(json!({ "pinned": false })),
        ),
        ("DELETE", path.clone(), None),
    ];
    let mut wrong = Vec::new();
    for (method, target, body) in attempts {
        let (status, answer) = call(&app, method, &target, &admin, body.clone()).await;
        if status != 404 {
            wrong.push(format!("{method} {target} {body:?}: {status} {answer}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "a deleted message answered:\n{}",
        wrong.join("\n")
    );

    assert_eq!(
        stored(&app, "messages", &mid).await,
        before,
        "nothing is written to the tombstone"
    );
    assert_eq!(
        reactions_of(&app, &mid).await,
        reactions_before,
        "no reaction comes or goes"
    );
    let frames = settle(&mut member_ws).await;
    assert!(
        of_type(&frames, "message:").is_empty(),
        "nothing is sent about a deleted message: {frames:?}"
    );
}

/// A write that passed its route's guard before a delete landed matches
/// nothing once the delete is in. The routes resolve the live message or file
/// first and write second, so each write keeps to live rows on its own: a
/// second delete answers 404 and leaves the first removal time, and an edit,
/// a pin or a reaction count does not land on a tombstone. Recordings share
/// the soft delete, and their route answers as it did.
#[tokio::test]
async fn a_write_that_raced_the_delete_matches_nothing_and_the_first_removal_time_stays() {
    let app = spawn().await;
    let t = app.seed_tenant("tombrace").await;
    let (tid, rid) = (t.tenant_id.clone(), t.rooms[0].id.clone());
    let admin = t.admin.access_token.clone();
    let (toid, roid) = (oid(&tid), oid(&rid));

    // A file: the second DELETE is a 404, and the soft delete that a racing
    // DELETE would make after its guard matches nothing.
    let fid = file(&app, &tid, &rid, &admin, "raced.txt").await;
    let file_path = format!("/api/tenant/{tid}/file/{fid}");
    let (status, body) = call(&app, "DELETE", &file_path, &admin, None).await;
    assert_eq!(status, 200, "the file's delete: {body}");
    let file_removed = removal_time(&app, "files", &fid).await;
    // Past the millisecond the removal time was stamped in, so a second
    // stamp would read as a different time.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (status, body) = call(&app, "DELETE", &file_path, &admin, None).await;
    assert_eq!(status, 404, "a second delete of the file: {body}");
    assert!(
        !FileDao::new(&app.db)
            .soft_delete(toid, oid(&fid))
            .await
            .unwrap(),
        "a soft delete of a deleted file matches nothing"
    );
    assert_eq!(
        removal_time(&app, "files", &fid).await,
        file_removed,
        "the file keeps its first removal time"
    );

    // A message: the same, and every other write a message route makes after
    // its guard.
    let mid = message(&app, &tid, &rid, &admin, "a raced note").await;
    let msg_path = format!("/api/tenant/{tid}/room/{rid}/message/{mid}");
    let (status, body) = call(&app, "DELETE", &msg_path, &admin, None).await;
    assert_eq!(status, 200, "the message's delete: {body}");
    let before = stored(&app, "messages", &mid).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (status, body) = call(&app, "DELETE", &msg_path, &admin, None).await;
    assert_eq!(status, 404, "a second delete of the message: {body}");
    let messages = MessageDao::new(&app.db);
    let (moid, author) = (oid(&mid), oid(&t.admin.id));
    let raced = [
        (
            "the soft delete",
            messages
                .base
                .soft_delete_in_tenant(toid, moid)
                .await
                .unwrap(),
        ),
        (
            "an edit",
            messages
                .update_content(toid, roid, moid, author, "a late edit".into())
                .await
                .unwrap(),
        ),
        (
            "a pin",
            messages.toggle_pin(toid, roid, moid, true).await.unwrap(),
        ),
        (
            "an unpin",
            messages.toggle_pin(toid, roid, moid, false).await.unwrap(),
        ),
        (
            "a reaction count",
            messages
                .update_reaction_summary(
                    moid,
                    &[ReactionSummary {
                        emoji: "heart".into(),
                        count: 1,
                    }],
                )
                .await
                .unwrap(),
        ),
    ];
    let landed: Vec<&str> = raced
        .iter()
        .filter(|(_, wrote)| *wrote)
        .map(|(what, _)| *what)
        .collect();
    assert!(landed.is_empty(), "landed on a deleted message: {landed:?}");
    assert_eq!(
        stored(&app, "messages", &mid).await,
        before,
        "the tombstone is as the delete left it"
    );

    // A recording: its route answers as it did, 200 and then 404, and the
    // soft delete it shares keeps the first removal time too.
    let rec = recording(&app, &tid, &rid, &admin).await;
    let rec_path = format!("/api/tenant/{tid}/room/{rid}/recording/{rec}");
    let (status, body) = call(&app, "DELETE", &rec_path, &admin, None).await;
    assert_eq!(status, 200, "the recording's delete: {body}");
    let rec_removed = removal_time(&app, "recordings", &rec).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (status, body) = call(&app, "DELETE", &rec_path, &admin, None).await;
    assert_eq!(status, 404, "a second delete of the recording: {body}");
    assert!(
        !RecordingDao::new(&app.db)
            .soft_delete(toid, oid(&rec))
            .await
            .unwrap(),
        "a soft delete of a deleted recording matches nothing"
    );
    assert_eq!(
        removal_time(&app, "recordings", &rec).await,
        rec_removed,
        "the recording keeps its first removal time"
    );
}

/// A message or file that never carried `deleted_at` is live. The live
/// predicate is `deleted_at: null`, which matches null and absent alike and is
/// the one every live listing uses. `{"$type": "null"}` is the partial unique
/// indexes' spelling: in a query it would 404 exactly these rows, a stored row
/// from before the field was written. Locked through the per-id lookup itself,
/// the routes on it, and a delete that still takes.
#[tokio::test]
async fn a_message_or_file_with_no_deleted_at_field_is_live() {
    let app = spawn().await;
    let t = app.seed_tenant("tombabsent").await;
    let (tid, rid) = (t.tenant_id.clone(), t.rooms[0].id.clone());
    let admin = t.admin.access_token.clone();
    let fid = file(&app, &tid, &rid, &admin, "older.txt").await;
    let mid = message(&app, &tid, &rid, &admin, "an older note").await;
    for (collection, id) in [("files", &fid), ("messages", &mid)] {
        app.db
            .collection::<Document>(collection)
            .update_one(
                doc! { "_id": oid(id) },
                doc! { "$unset": { "deleted_at": "" } },
            )
            .await
            .unwrap();
        assert!(
            !stored(&app, collection, id)
                .await
                .contains_key("deleted_at"),
            "{collection}: the field is gone"
        );
    }

    // The per-id lookup itself.
    let toid = oid(&tid);
    assert!(
        FileDao::new(&app.db)
            .base
            .find_by_id_in_tenant(toid, oid(&fid))
            .await
            .is_ok(),
        "a file with no deleted_at field resolves"
    );
    assert!(
        MessageDao::new(&app.db)
            .base
            .find_by_id_in_tenant(toid, oid(&mid))
            .await
            .is_ok(),
        "a message with no deleted_at field resolves"
    );

    // The routes on it.
    let file_path = format!("/api/tenant/{tid}/file/{fid}");
    let msg_path = format!("/api/tenant/{tid}/room/{rid}/message/{mid}");
    for (method, target, body) in [
        ("GET", file_path.clone(), None),
        ("GET", format!("{file_path}/download"), None),
        (
            "PUT",
            msg_path.clone(),
            Some(json!({ "content": "still here" })),
        ),
        (
            "PUT",
            format!("{msg_path}/pin"),
            Some(json!({ "pinned": true })),
        ),
        ("GET", format!("{msg_path}/thread"), None),
    ] {
        let (status, answer) = call(&app, method, &target, &admin, body).await;
        assert_eq!(status, 200, "{method} {target}: absent is live: {answer}");
    }

    // Its delete takes, once.
    for target in [&file_path, &msg_path] {
        let (status, answer) = call(&app, "DELETE", target, &admin, None).await;
        assert_eq!(status, 200, "DELETE {target}: {answer}");
        let (status, answer) = call(&app, "DELETE", target, &admin, None).await;
        assert_eq!(status, 404, "DELETE {target} again: {answer}");
    }
}

/// A room that carries a removal date answers as a missing one. A room's
/// `DELETE` removes it outright today (`cascade_delete`), but a room deleted
/// before that kept its row with a `deleted_at`, and every room listing has
/// left such a row out since. Its id now agrees with the listings: the room,
/// its messages and its files answer 404, and the org's file list and search
/// leave them out.
#[tokio::test]
async fn a_room_carrying_a_removal_date_answers_as_a_missing_one() {
    let app = spawn().await;
    let t = app.seed_tenant("tombroom").await;
    let (tid, rid) = (t.tenant_id.clone(), t.rooms[2].id.clone());
    let admin = t.admin.access_token.clone();
    let word = "quartzremoved";
    let mid = message(&app, &tid, &rid, &admin, &format!("{word} note")).await;
    let fid = file(&app, &tid, &rid, &admin, "removed-room.txt").await;
    let room_path = format!("/api/tenant/{tid}/room/{rid}");
    let file_path = format!("/api/tenant/{tid}/file/{fid}");

    // The control: while the room is live, all of it answers.
    let (status, body) = call(&app, "GET", &room_path, &admin, None).await;
    assert_eq!(status, 200, "the live room: {body}");
    assert!(org_file_ids(&app, &tid, &admin).await.contains(&fid));
    assert!(
        searched_message_ids(&app, &tid, &admin, word)
            .await
            .contains(&mid)
    );

    app.db
        .collection::<Document>("rooms")
        .update_one(
            doc! { "_id": oid(&rid) },
            doc! { "$set": { "deleted_at": DateTime::now() } },
        )
        .await
        .unwrap();

    let mut wrong = Vec::new();
    for (method, target, body) in [
        ("GET", room_path.clone(), None),
        ("GET", format!("{room_path}/message"), None),
        (
            "POST",
            format!("{room_path}/message"),
            Some(json!({ "content": "into a removed room" })),
        ),
        (
            "PUT",
            format!("{room_path}/message/{mid}"),
            Some(json!({ "content": "an edit" })),
        ),
        ("GET", file_path.clone(), None),
        ("GET", format!("{file_path}/download"), None),
    ] {
        let (status, answer) = call(&app, method, &target, &admin, body.clone()).await;
        if status != 404 {
            wrong.push(format!("{method} {target} {body:?}: {status} {answer}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "a removed room answered:\n{}",
        wrong.join("\n")
    );
    assert!(
        !org_file_ids(&app, &tid, &admin).await.contains(&fid),
        "the org's file list leaves out a removed room's file"
    );
    assert!(
        !searched_message_ids(&app, &tid, &admin, word)
            .await
            .contains(&mid),
        "search leaves out a removed room's message"
    );
}

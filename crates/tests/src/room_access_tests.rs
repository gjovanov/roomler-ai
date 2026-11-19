// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! A room's messages and files are reached only by someone who may read the
//! room.
//!
//! Every route that takes a message or a file id resolves it through one
//! guard per id kind. The object must be live and of the path tenant; a
//! message must also be of the path room. Its room must be one the caller may
//! read: a Public room is open to every member of the org, and a Private or
//! Secret room to its members only. A Private room answers 403 and a Secret
//! room 404, as the room routes do. The listings (a room's files, the org's
//! files, search) leave out what the caller may not read, an upload names a
//! room of the org that the caller may read, and only the uploader deletes a
//! file.
//!
//! [`every_message_and_file_route_answers_only_for_a_room_the_caller_can_read`]
//! walks the routes with a message or file id in their path as the
//! composition baseline lists them, so a route added later joins the walk.

use std::collections::BTreeSet;
use std::time::Duration;

use bson::{Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use reqwest::multipart;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn oid(hex: &str) -> ObjectId {
    ObjectId::parse_str(hex).unwrap()
}

/// A server whose per-address rate limit is out of the way: the walk sends
/// more requests than the default burst, and a 429 would read as a refusal.
async fn spawn() -> TestApp {
    TestApp::spawn_with_settings(|s| {
        s.app.rate_limit_per_sec = 1_000;
        s.app.rate_limit_burst = 10_000;
    })
    .await
}

/// An org with its seeded open rooms and a Private and a Secret room made by
/// its admin. The admin is a member of all of them. The seeded member is in
/// none, so of these rooms they may read only the open (Public) ones.
struct Org {
    t: SeededTenant,
    open: String,
    other_open: String,
    private: String,
    secret: String,
}

async fn org(app: &TestApp, slug: &str) -> Org {
    let t = app.seed_tenant(slug).await;
    let private = closed_room(app, &t, &format!("{slug} private"), "private").await;
    let secret = closed_room(app, &t, &format!("{slug} secret"), "secret").await;
    Org {
        open: t.rooms[0].id.clone(),
        other_open: t.rooms[1].id.clone(),
        private,
        secret,
        t,
    }
}

/// A room by the org's admin with the given visibility.
async fn closed_room(app: &TestApp, t: &SeededTenant, name: &str, visibility: &str) -> String {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "name": name }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert!(status.is_success(), "room {name}: {status} {body}");
    let id = body["id"].as_str().unwrap().to_string();
    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/room/{id}", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "visibility": visibility }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "visibility {visibility}");
    id
}

async fn post_message(app: &TestApp, tid: &str, rid: &str, token: &str, body: Value) -> Value {
    let resp = app
        .auth_post(&format!("/api/tenant/{tid}/room/{rid}/message"), token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 200, "message posted: {body}");
    body
}

async fn message(app: &TestApp, tid: &str, rid: &str, token: &str, content: &str) -> String {
    post_message(app, tid, rid, token, json!({ "content": content })).await["id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// `POST …/file/upload` naming `room_id` in the form.
async fn upload(app: &TestApp, tid: &str, room_id: &str, token: &str, name: &str) -> (u16, Value) {
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
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `POST …/room/{room_id}/file/upload`.
async fn upload_into_room(
    app: &TestApp,
    tid: &str,
    room_id: &str,
    token: &str,
    name: &str,
) -> (u16, Value) {
    let part = multipart::Part::bytes(format!("the bytes of {name}").into_bytes())
        .file_name(name.to_string())
        .mime_str("text/plain")
        .unwrap();
    let form = multipart::Form::new().part("file", part);
    let resp = app
        .client
        .post(app.url(&format!("/api/tenant/{tid}/room/{room_id}/file/upload")))
        .header("Authorization", format!("Bearer {token}"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn file(app: &TestApp, tid: &str, room_id: &str, token: &str, name: &str) -> String {
    let (status, body) = upload(app, tid, room_id, token, name).await;
    assert_eq!(status, 200, "upload of {name}: {body}");
    body["id"].as_str().unwrap().to_string()
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

async fn delete_message(app: &TestApp, tid: &str, rid: &str, mid: &str, token: &str) {
    let (status, body) = call(
        app,
        "DELETE",
        &format!("/api/tenant/{tid}/room/{rid}/message/{mid}"),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "message delete: {body}");
}

async fn delete_file(app: &TestApp, tid: &str, fid: &str, token: &str) {
    let (status, body) = call(
        app,
        "DELETE",
        &format!("/api/tenant/{tid}/file/{fid}"),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "file delete: {body}");
}

async fn stored(app: &TestApp, collection: &str, id: &str) -> Document {
    app.db
        .collection::<Document>(collection)
        .find_one(doc! { "_id": oid(id) })
        .await
        .unwrap()
        .expect("the row is stored")
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

async fn send(ws: &mut Ws, frame: Value) {
    ws.send(WsMessage::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// Every frame this socket received up to the answer to a `ping` sent now. A
/// socket's frames are handled one at a time, and what a frame causes is
/// written before the next one is read, so a frame this socket sent before
/// the ping has had all its effects by the time the pong arrives.
async fn settle(ws: &mut Ws) -> Vec<Value> {
    send(ws, json!({ "type": "ping", "data": null })).await;
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

/// The routes with a message or a file id in their path, as the walk knows
/// them. The walk refuses to run when the composition baseline lists one that
/// is not here: a new route must be added with the body it needs, and then it
/// is walked like the rest.
const ID_ROUTES: [(&str, &str); 9] = [
    (
        "PUT",
        "/api/tenant/{tenant_id}/room/{room_id}/message/{message_id}",
    ),
    (
        "PUT",
        "/api/tenant/{tenant_id}/room/{room_id}/message/{message_id}/pin",
    ),
    (
        "POST",
        "/api/tenant/{tenant_id}/room/{room_id}/message/{message_id}/reaction",
    ),
    (
        "DELETE",
        "/api/tenant/{tenant_id}/room/{room_id}/message/{message_id}/reaction/{emoji}",
    ),
    (
        "GET",
        "/api/tenant/{tenant_id}/room/{room_id}/message/{message_id}/thread",
    ),
    ("GET", "/api/tenant/{tenant_id}/file/{file_id}"),
    ("GET", "/api/tenant/{tenant_id}/file/{file_id}/download"),
    // The deletes last: a refused one must leave the object for the rest.
    (
        "DELETE",
        "/api/tenant/{tenant_id}/room/{room_id}/message/{message_id}",
    ),
    ("DELETE", "/api/tenant/{tenant_id}/file/{file_id}"),
];

fn body_for(method: &str, template: &str) -> Option<Value> {
    if template.ends_with("/pin") {
        Some(json!({ "pinned": true }))
    } else if method == "POST" && template.ends_with("/reaction") {
        Some(json!({ "emoji": "thumbsup" }))
    } else if method == "PUT" && template.ends_with("{message_id}") {
        Some(json!({ "content": "an edit" }))
    } else {
        None
    }
}

/// The baseline's routes with a message or a file id in their path.
fn baseline_id_routes() -> BTreeSet<(String, String)> {
    let baseline: Value =
        serde_json::from_str(include_str!("../fixtures/composition.baseline.json")).unwrap();
    let mut out = BTreeSet::new();
    for route in baseline["routes"].as_array().expect("routes") {
        let path = route["path"].as_str().unwrap();
        if !(path.contains("{message_id}") || path.contains("{file_id}")) {
            continue;
        }
        for method in route["methods"].as_str().unwrap().split(',') {
            if method != "HEAD" {
                out.insert((method.to_string(), path.to_string()));
            }
        }
    }
    out
}

/// One caller and the ids it names. `room` fills `{room_id}`; `file` is
/// `None` when the case is about the path room, which file routes do not
/// have.
struct Case {
    who: &'static str,
    token: String,
    tenant: String,
    room: String,
    message: String,
    file: Option<String>,
    want: u16,
}

#[tokio::test]
async fn every_message_and_file_route_answers_only_for_a_room_the_caller_can_read() {
    let known: BTreeSet<(String, String)> = ID_ROUTES
        .iter()
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect();
    assert_eq!(
        baseline_id_routes(),
        known,
        "every route with a message or file id in its path is walked here; add a new one to ID_ROUTES with its body"
    );

    let app = spawn().await;
    let a = org(&app, "walka").await;
    let b = org(&app, "walkb").await;
    let (tid, admin) = (a.t.tenant_id.clone(), a.t.admin.access_token.clone());
    let m_open = message(&app, &tid, &a.open, &admin, "an open note").await;
    let m_private = message(&app, &tid, &a.private, &admin, "a private note").await;
    let m_secret = message(&app, &tid, &a.secret, &admin, "a secret note").await;
    let m_gone = message(&app, &tid, &a.open, &admin, "a removed note").await;
    delete_message(&app, &tid, &a.open, &m_gone, &admin).await;
    let f_open = file(&app, &tid, &a.open, &admin, "open.txt").await;
    let f_private = file(&app, &tid, &a.private, &admin, "private.txt").await;
    let f_secret = file(&app, &tid, &a.secret, &admin, "secret.txt").await;
    let f_gone = file(&app, &tid, &a.open, &admin, "removed.txt").await;
    delete_file(&app, &tid, &f_gone, &admin).await;
    let before: Vec<Document> = vec![
        stored(&app, "messages", &m_private).await,
        stored(&app, "messages", &m_secret).await,
        stored(&app, "files", &f_private).await,
        stored(&app, "files", &f_secret).await,
    ];

    let member = a.t.member.access_token.clone();
    let cases = vec![
        Case {
            who: "another org's admin, in their own org and room",
            token: b.t.admin.access_token.clone(),
            tenant: b.t.tenant_id.clone(),
            room: b.open.clone(),
            message: m_open.clone(),
            file: Some(f_open.clone()),
            want: 404,
        },
        Case {
            who: "a member outside the Secret room",
            token: member.clone(),
            tenant: tid.clone(),
            room: a.secret.clone(),
            message: m_secret.clone(),
            file: Some(f_secret.clone()),
            want: 404,
        },
        Case {
            who: "a member outside the Private room",
            token: member.clone(),
            tenant: tid.clone(),
            room: a.private.clone(),
            message: m_private.clone(),
            file: Some(f_private.clone()),
            want: 403,
        },
        Case {
            who: "a member naming the Secret room's message through an open room",
            token: member.clone(),
            tenant: tid.clone(),
            room: a.open.clone(),
            message: m_secret.clone(),
            file: None,
            want: 404,
        },
        Case {
            who: "a member of both rooms naming a message through the other room",
            token: admin.clone(),
            tenant: tid.clone(),
            room: a.other_open.clone(),
            message: m_open.clone(),
            file: None,
            want: 404,
        },
        Case {
            who: "anyone naming a deleted message or file",
            token: admin.clone(),
            tenant: tid.clone(),
            room: a.open.clone(),
            message: m_gone.clone(),
            file: Some(f_gone.clone()),
            want: 404,
        },
    ];

    let mut wrong = Vec::new();
    for case in &cases {
        for (method, template) in ID_ROUTES {
            let id = if template.contains("{message_id}") {
                case.message.clone()
            } else if let Some(file) = &case.file {
                file.clone()
            } else {
                continue;
            };
            let path = template
                .replace("{tenant_id}", &case.tenant)
                .replace("{room_id}", &case.room)
                .replace("{message_id}", &id)
                .replace("{file_id}", &id)
                .replace("{emoji}", "thumbsup");
            let (status, body) =
                call(&app, method, &path, &case.token, body_for(method, template)).await;
            if status != case.want {
                wrong.push(format!(
                    "{method} {template} as {}: {status} (want {}) {body}",
                    case.who, case.want
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "refusals:\n{}", wrong.join("\n"));

    let after: Vec<Document> = vec![
        stored(&app, "messages", &m_private).await,
        stored(&app, "messages", &m_secret).await,
        stored(&app, "files", &f_private).await,
        stored(&app, "files", &f_secret).await,
    ];
    assert_eq!(after, before, "a refused request changes nothing");

    // The control: the admin, a member of the open room, the author and the
    // uploader, gets an answer from every route.
    for (method, template) in ID_ROUTES {
        let id = if template.contains("{message_id}") {
            &m_open
        } else {
            &f_open
        };
        let path = template
            .replace("{tenant_id}", &tid)
            .replace("{room_id}", &a.open)
            .replace("{message_id}", id)
            .replace("{file_id}", id)
            .replace("{emoji}", "thumbsup");
        let (status, body) = call(&app, method, &path, &admin, body_for(method, template)).await;
        assert_eq!(
            status, 200,
            "{method} {template} for the room's member: {body}"
        );
    }
}

#[tokio::test]
async fn an_edit_is_the_authors_on_a_live_message_of_the_path_room() {
    let app = spawn().await;
    let a = org(&app, "edita").await;
    let (tid, admin) = (a.t.tenant_id.clone(), a.t.admin.access_token.clone());
    let member = a.t.member.access_token.clone();
    join(&app, &tid, &a.open, &member).await;
    let mid = message(&app, &tid, &a.open, &admin, "the first text").await;
    let gone = message(&app, &tid, &a.open, &admin, "a removed text").await;
    delete_message(&app, &tid, &a.open, &gone, &admin).await;
    let before = stored(&app, "messages", &mid).await;
    let mut admin_ws = connect(&app, &admin).await;
    let mut member_ws = connect(&app, &member).await;

    let edit = |room: &str, id: &str| format!("/api/tenant/{tid}/room/{room}/message/{id}");
    let refused = [
        (
            "a reader who is not the author",
            edit(&a.open, &mid),
            &member,
        ),
        (
            "the author, through another room",
            edit(&a.other_open, &mid),
            &admin,
        ),
        (
            "the author, on a deleted message",
            edit(&a.open, &gone),
            &admin,
        ),
    ];
    for (who, path, token) in refused {
        let (status, body) = call(
            &app,
            "PUT",
            &path,
            token,
            Some(json!({ "content": "a rewrite" })),
        )
        .await;
        assert_eq!(status, 404, "{who}: {body}");
        assert!(body.get("content").is_none(), "{who}: {body}");
    }
    assert_eq!(
        stored(&app, "messages", &mid).await,
        before,
        "nothing written"
    );
    for ws in [&mut admin_ws, &mut member_ws] {
        let frames = settle(ws).await;
        assert!(
            of_type(&frames, "message:").is_empty(),
            "a refused edit sends nothing: {frames:?}"
        );
    }

    let (status, body) = call(
        &app,
        "PUT",
        &edit(&a.open, &mid),
        &admin,
        Some(json!({ "content": "the second text" })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["content"], "the second text");
    let frames = settle(&mut member_ws).await;
    assert!(
        frames
            .iter()
            .any(|f| f["type"] == "message:update" && f["data"]["content"] == "the second text"),
        "the room hears the edit: {frames:?}"
    );
}

#[tokio::test]
async fn a_rooms_files_and_uploads_follow_the_rooms_visibility() {
    let app = spawn().await;
    let a = org(&app, "roomfa").await;
    let b = org(&app, "roomfb").await;
    let tid = a.t.tenant_id.clone();
    let member = a.t.member.access_token.clone();
    file(&app, &tid, &a.secret, &a.t.admin.access_token, "kept.txt").await;

    let list = |room: &str| format!("/api/tenant/{tid}/room/{room}/file");
    for (room, want) in [(&a.secret, 404), (&a.private, 403), (&a.open, 200)] {
        let (status, body) = call(&app, "GET", &list(room), &member, None).await;
        assert_eq!(status, want, "the file list of {room}: {body}");
    }
    for (room, want) in [(&a.secret, 404), (&a.private, 403)] {
        let (status, body) = upload_into_room(&app, &tid, room, &member, "x.txt").await;
        assert_eq!(status, want, "an upload into {room}: {body}");
        let (status, body) = upload(&app, &tid, room, &member, "y.txt").await;
        assert_eq!(status, want, "an upload naming {room}: {body}");
    }
    let (status, body) = upload(&app, &tid, &a.open, &member, "z.txt").await;
    assert_eq!(status, 200, "an upload into an open room: {body}");

    // Another org's room, named from the caller's own org.
    let outsider = b.t.admin.access_token.clone();
    let (status, body) = upload(&app, &b.t.tenant_id, &a.open, &outsider, "o.txt").await;
    assert_eq!(status, 404, "{body}");
    let (status, body) = upload_into_room(&app, &b.t.tenant_id, &a.open, &outsider, "p.txt").await;
    assert_eq!(status, 404, "{body}");

    let stored_by = |user: &str| {
        let user = oid(user);
        let files = app.db.collection::<Document>("files");
        async move {
            files
                .count_documents(doc! { "uploaded_by": user })
                .await
                .unwrap()
        }
    };
    assert_eq!(
        stored_by(&a.t.member.id).await,
        1,
        "only the open-room upload"
    );
    assert_eq!(
        stored_by(&b.t.admin.id).await,
        0,
        "nothing for the outsider"
    );
}

#[tokio::test]
async fn the_orgs_file_list_lists_and_names_only_rooms_the_caller_can_read() {
    let app = spawn().await;
    let a = org(&app, "orgfa").await;
    let b = org(&app, "orgfb").await;
    let (tid, admin) = (a.t.tenant_id.clone(), a.t.admin.access_token.clone());
    let f_open = file(&app, &tid, &a.open, &admin, "open.txt").await;
    let f_private = file(&app, &tid, &a.private, &admin, "private.txt").await;
    let f_secret = file(&app, &tid, &a.secret, &admin, "secret.txt").await;
    // A stored row whose room is another org's: neither listed nor named.
    let f_elsewhere = file(&app, &tid, &a.open, &admin, "elsewhere.txt").await;
    app.db
        .collection::<Document>("files")
        .update_one(
            doc! { "_id": oid(&f_elsewhere) },
            doc! { "$set": {
                "context.room_id": oid(&b.secret),
                "context.entity_id": oid(&b.secret),
            } },
        )
        .await
        .unwrap();

    let listed = |token: String| {
        let app = &app;
        let path = format!("/api/tenant/{tid}/file?per_page=100");
        async move {
            let (status, body) = call(app, "GET", &path, &token, None).await;
            assert_eq!(status, 200, "{body}");
            body
        }
    };
    let ids = |body: &Value| -> BTreeSet<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].as_str().unwrap().to_string())
            .collect()
    };

    let seen = listed(a.t.member.access_token.clone()).await;
    assert_eq!(ids(&seen), BTreeSet::from([f_open.clone()]), "{seen}");
    assert_eq!(seen["total"], 1, "{seen}");
    assert_eq!(seen["items"][0]["room_name"], "general", "{seen}");

    let seen = listed(admin.clone()).await;
    assert_eq!(
        ids(&seen),
        BTreeSet::from([f_open, f_private, f_secret]),
        "{seen}"
    );
    assert!(
        !seen.to_string().contains("orgfb secret"),
        "no other org's room is named: {seen}"
    );
}

#[tokio::test]
async fn a_file_is_deleted_by_its_uploader_only() {
    let app = spawn().await;
    let a = org(&app, "fdel").await;
    let (tid, admin) = (a.t.tenant_id.clone(), a.t.admin.access_token.clone());
    let fid = file(&app, &tid, &a.open, &admin, "mine.txt").await;
    let path = format!("/api/tenant/{tid}/file/{fid}");

    let (status, body) = call(&app, "DELETE", &path, &a.t.member.access_token, None).await;
    assert_eq!(status, 403, "a reader who did not upload it: {body}");
    let (status, _) = call(&app, "GET", &path, &admin, None).await;
    assert_eq!(status, 200, "the file is still there");

    let (status, body) = call(&app, "DELETE", &path, &admin, None).await;
    assert_eq!(status, 200, "the uploader: {body}");
    let (status, _) = call(&app, "GET", &path, &admin, None).await;
    assert_eq!(status, 404, "a deleted file is gone");
}

#[tokio::test]
async fn search_finds_only_rooms_and_messages_the_caller_can_read() {
    let app = spawn().await;
    let a = org(&app, "srch").await;
    let (tid, admin) = (a.t.tenant_id.clone(), a.t.admin.access_token.clone());
    let word = "quartzword";
    let private = closed_room(&app, &a.t, &format!("{word} private"), "private").await;
    let secret = closed_room(&app, &a.t, &format!("{word} secret"), "secret").await;
    let m_open = message(&app, &tid, &a.open, &admin, &format!("{word} in the open")).await;
    message(&app, &tid, &private, &admin, &format!("{word} in private")).await;
    message(&app, &tid, &secret, &admin, &format!("{word} in secret")).await;

    let search = |token: String| {
        let app = &app;
        let path = format!("/api/tenant/{tid}/search?q={word}");
        async move {
            let (status, body) = call(app, "GET", &path, &token, None).await;
            assert_eq!(status, 200, "{body}");
            body
        }
    };
    let ids = |body: &Value, key: &str| -> BTreeSet<String> {
        body[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["id"].as_str().unwrap().to_string())
            .collect()
    };

    let seen = search(a.t.member.access_token.clone()).await;
    assert_eq!(ids(&seen, "messages"), BTreeSet::from([m_open]), "{seen}");
    // A Private room is listed to everyone in the org, as the room list
    // lists it; a Secret room only to its members.
    assert_eq!(
        ids(&seen, "rooms"),
        BTreeSet::from([private.clone()]),
        "{seen}"
    );

    let seen = search(admin).await;
    assert_eq!(ids(&seen, "messages").len(), 3, "{seen}");
    assert_eq!(
        ids(&seen, "rooms"),
        BTreeSet::from([private, secret]),
        "{seen}"
    );
}

#[tokio::test]
async fn an_attachment_is_a_live_file_of_the_messages_room() {
    let app = spawn().await;
    let a = org(&app, "attach").await;
    let (tid, admin) = (a.t.tenant_id.clone(), a.t.admin.access_token.clone());
    let here = file(&app, &tid, &a.open, &admin, "here.txt").await;
    let elsewhere = file(&app, &tid, &a.secret, &admin, "elsewhere.txt").await;
    let gone = file(&app, &tid, &a.open, &admin, "gone.txt").await;
    delete_file(&app, &tid, &gone, &admin).await;

    let posted = post_message(
        &app,
        &tid,
        &a.open,
        &admin,
        json!({ "content": "files", "attachment_ids": [here, elsewhere, gone] }),
    )
    .await;
    let attached: Vec<&str> = posted["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["file_id"].as_str().unwrap())
        .collect();
    assert_eq!(attached, vec![here.as_str()], "{posted}");
}

#[tokio::test]
async fn typing_reaches_a_room_only_from_someone_who_can_read_it() {
    let app = spawn().await;
    let a = org(&app, "typea").await;
    let b = org(&app, "typeb").await;
    let mut admin_ws = connect(&app, &a.t.admin.access_token).await;
    let mut member_ws = connect(&app, &a.t.member.access_token).await;
    let mut outsider_ws = connect(&app, &b.t.admin.access_token).await;

    let typing = |room: &str| json!({ "type": "typing:start", "data": { "room_id": room } });
    send(&mut outsider_ws, typing(&a.open)).await;
    settle(&mut outsider_ws).await;
    send(&mut member_ws, typing(&a.secret)).await;
    settle(&mut member_ws).await;
    let frames = settle(&mut admin_ws).await;
    assert!(
        of_type(&frames, "typing:").is_empty(),
        "no typing from someone who cannot read the room: {frames:?}"
    );

    send(&mut member_ws, typing(&a.open)).await;
    settle(&mut member_ws).await;
    let frames = settle(&mut admin_ws).await;
    let typing = of_type(&frames, "typing:start");
    assert_eq!(typing.len(), 1, "{frames:?}");
    assert_eq!(typing[0]["data"]["user_id"], a.t.member.id.as_str());
}

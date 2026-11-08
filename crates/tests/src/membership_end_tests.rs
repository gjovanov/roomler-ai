// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! An org membership takes its room memberships with it.
//!
//! When someone stops being a member of an org (an admin removes them, or
//! they leave), their membership rows in that org's rooms go too: chat's
//! `MembershipLifecycle` hook, after the call seats are ended. From then on
//! the org's rooms reach them neither over the socket nor by a mention.
//! Rows that outlived an org membership before this are ended by an operator
//! step, `POST /api/admin/room-members/prune`: a dry run by default, bounded,
//! and a no-op once nothing is left.

use std::time::Duration;

use bson::{Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn oid(hex: &str) -> ObjectId {
    ObjectId::parse_str(hex).unwrap()
}

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

async fn join(app: &TestApp, t: &SeededTenant, room: &str, token: &str) {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/tenant/{}/room/{room}/join", t.tenant_id),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "join: {body}");
}

/// The person's membership rows in the org's rooms.
async fn room_rows(app: &TestApp, tenant_id: &str, user_id: &str) -> u64 {
    app.db
        .collection::<Document>("room_members")
        .count_documents(doc! { "tenant_id": oid(tenant_id), "user_id": oid(user_id) })
        .await
        .unwrap()
}

async fn member_count(app: &TestApp, room: &str) -> i64 {
    let row = app
        .db
        .collection::<Document>("rooms")
        .find_one(doc! { "_id": oid(room) })
        .await
        .unwrap()
        .expect("the room");
    row.get_i32("member_count")
        .map(i64::from)
        .or_else(|_| row.get_i64("member_count"))
        .expect("a member count")
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

#[tokio::test]
async fn an_org_membership_takes_its_room_memberships_with_it() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("orgend").await;
    let (room, other) = (a.rooms[0].id.clone(), a.rooms[1].id.clone());
    join(&app, &a, &room, &a.member.access_token).await;
    join(&app, &a, &other, &a.member.access_token).await;
    assert_eq!(room_rows(&app, &a.tenant_id, &a.member.id).await, 2);
    let count = member_count(&app, &room).await;

    let (status, body) = call(
        &app,
        "DELETE",
        &format!("/api/tenant/{}/member/{}", a.tenant_id, a.member.id),
        &a.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        room_rows(&app, &a.tenant_id, &a.member.id).await,
        0,
        "the room memberships end with the org membership"
    );
    assert_eq!(member_count(&app, &room).await, count - 1);

    // The org's rooms no longer reach them: no message, no mention.
    let mut ws = connect(&app, &a.member.access_token).await;
    let (status, body) = call(
        &app,
        "POST",
        &format!("/api/tenant/{}/room/{room}/message", a.tenant_id),
        &a.admin.access_token,
        Some(json!({
            "content": "after the removal",
            "mentions": { "users": [a.member.id], "everyone": false, "here": false },
        })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let frames = settle(&mut ws).await;
    assert!(
        !frames.iter().any(|f| {
            let t = f["type"].as_str().unwrap_or("");
            t.starts_with("message:") || t.starts_with("notification:")
        }),
        "nothing from the org's rooms: {frames:?}"
    );
    assert_eq!(notifications_for(&app, &a.member.id).await, 0);

    // Leaving the org yourself is the same end.
    let b = app.seed_tenant("orgleave").await;
    join(&app, &b, &b.rooms[0].id, &b.member.access_token).await;
    let (status, body) = call(
        &app,
        "DELETE",
        &format!("/api/tenant/{}/member/{}", b.tenant_id, b.member.id),
        &b.member.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(room_rows(&app, &b.tenant_id, &b.member.id).await, 0);
}

#[tokio::test]
async fn the_operator_step_ends_room_memberships_without_an_org_membership() {
    let operator = ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(operator.to_hex());
    })
    .await;
    let a = app.seed_tenant("prunerm").await;
    join(&app, &a, &a.rooms[0].id, &a.member.access_token).await;
    join(&app, &a, &a.rooms[1].id, &a.member.access_token).await;
    // Rows as an org membership that ended before the hook existed left them.
    app.db
        .collection::<Document>("tenant_members")
        .delete_one(doc! { "tenant_id": oid(&a.tenant_id), "user_id": oid(&a.member.id) })
        .await
        .unwrap();
    let admin_rows = room_rows(&app, &a.tenant_id, &a.admin.id).await;
    let op = app
        .state
        .auth
        .generate_tokens(operator, "operator@prunerm.test", "prunerm_operator")
        .unwrap()
        .access_token;
    let prune = "/api/admin/room-members/prune";

    let (status, _) = call(&app, "POST", prune, &a.admin.access_token, Some(json!({}))).await;
    assert_eq!(status, 404, "only a platform operator");

    let (status, body) = call(&app, "POST", prune, &op, Some(json!({}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["dry_run"], true, "a dry run unless asked: {body}");
    assert_eq!(body["pairs"], 1, "{body}");
    assert_eq!(body["rows"], 2, "{body}");
    assert_eq!(body["ended"], 0, "{body}");
    assert_eq!(room_rows(&app, &a.tenant_id, &a.member.id).await, 2);

    let (status, body) = call(&app, "POST", prune, &op, Some(json!({ "dry_run": false }))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ended"], 1, "{body}");
    assert_eq!(body["more"], false, "{body}");
    assert_eq!(room_rows(&app, &a.tenant_id, &a.member.id).await, 0);
    assert_eq!(
        room_rows(&app, &a.tenant_id, &a.admin.id).await,
        admin_rows,
        "a member's rows stay"
    );

    let (status, body) = call(&app, "POST", prune, &op, Some(json!({ "dry_run": false }))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (body["pairs"].clone(), body["ended"].clone()),
        (json!(0), json!(0))
    );
}

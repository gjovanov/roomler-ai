// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4409 — a room's stored `member_count` is the number of its member rows.
//!
//! A member row is a `room_members` document of the room: the rows the member
//! route lists, and counts as its `total`. Every way a row comes or goes moves
//! the count with it: the room's creation (its creator's row), a join, a
//! leave, an org membership that ends, and a first call join by someone who
//! was not a member yet.

use bson::{Bson, Document, doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;

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

/// A new room of the org, made by its admin through the API.
async fn new_room(app: &TestApp, t: &SeededTenant, name: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/tenant/{}/room", t.tenant_id),
        &t.admin.access_token,
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(status, 200, "room create: {body}");
    body["id"].as_str().expect("the room's id").to_string()
}

async fn post_ok(app: &TestApp, t: &SeededTenant, room: &str, what: &str, token: &str) {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/tenant/{}/room/{room}/{what}", t.tenant_id),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{what}: {body}");
}

/// The room row's stored `member_count`.
async fn stored_count(app: &TestApp, room: &str) -> i64 {
    let row = app
        .db
        .collection::<Document>("rooms")
        .find_one(doc! { "_id": oid(room) })
        .await
        .unwrap()
        .expect("the room");
    match row.get("member_count") {
        Some(Bson::Int32(n)) => i64::from(*n),
        Some(Bson::Int64(n)) => *n,
        other => panic!("member_count is {other:?}"),
    }
}

/// The room's member rows.
async fn member_rows(app: &TestApp, room: &str) -> i64 {
    app.db
        .collection::<Document>("room_members")
        .count_documents(doc! { "room_id": oid(room) })
        .await
        .unwrap() as i64
}

/// The member route's `total`, read by the admin.
async fn route_total(app: &TestApp, t: &SeededTenant, room: &str) -> i64 {
    let (status, body) = call(
        app,
        "GET",
        &format!("/api/tenant/{}/room/{room}/member", t.tenant_id),
        &t.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "member route: {body}");
    body["total"].as_i64().expect("a total")
}

/// The stored count, the rows and the route's `total` all read `rows`.
async fn assert_counts(app: &TestApp, t: &SeededTenant, room: &str, rows: i64, when: &str) {
    assert_eq!(member_rows(app, room).await, rows, "{when}: member rows");
    assert_eq!(route_total(app, t, room).await, rows, "{when}: route total");
    assert_eq!(
        stored_count(app, room).await,
        rows,
        "{when}: the room's member_count against its {rows} member rows"
    );
}

#[tokio::test]
async fn a_new_room_counts_its_creator_once() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("mcnew").await;
    let room = new_room(&app, &t, "counted").await;
    assert_counts(&app, &t, &room, 1, "right after the room is created").await;
}

#[tokio::test]
async fn a_join_and_a_leave_move_the_count_with_the_rows() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("mcjoin").await;
    let room = new_room(&app, &t, "joined").await;

    post_ok(&app, &t, &room, "join", &t.member.access_token).await;
    assert_counts(&app, &t, &room, 2, "after a second person joins").await;

    post_ok(&app, &t, &room, "leave", &t.member.access_token).await;
    assert_counts(&app, &t, &room, 1, "after they leave").await;
}

#[tokio::test]
async fn an_ended_org_membership_takes_its_count_with_it() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("mcend").await;
    let room = new_room(&app, &t, "ended").await;
    post_ok(&app, &t, &room, "join", &t.member.access_token).await;
    assert_counts(&app, &t, &room, 2, "after a second person joins").await;

    let (status, body) = call(
        &app,
        "DELETE",
        &format!("/api/tenant/{}/member/{}", t.tenant_id, t.member.id),
        &t.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_counts(&app, &t, &room, 1, "after their org membership ends").await;
}

#[tokio::test]
async fn a_first_time_call_participant_is_counted_once() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("mccall").await;
    let room = new_room(&app, &t, "called").await;
    post_ok(&app, &t, &room, "call/start", &t.admin.access_token).await;

    // The member holds no row in the room: their first call join makes one.
    assert_eq!(member_rows(&app, &room).await, 1);
    post_ok(&app, &t, &room, "call/join", &t.member.access_token).await;
    assert_counts(&app, &t, &room, 2, "after a first-time call participant").await;

    // Leaving the call keeps the membership the join made.
    post_ok(&app, &t, &room, "call/leave", &t.member.access_token).await;
    assert_counts(&app, &t, &room, 2, "after they leave the call").await;
}

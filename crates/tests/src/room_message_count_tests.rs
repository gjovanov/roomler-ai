// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4448 — a room's `message_count` is the number of its live messages.
//!
//! A live message is a `messages` document of the room with no `deleted_at`:
//! a post, a thread reply and a quote each count one, and a delete (a soft
//! delete, which leaves the tombstone) takes its one away. The room API reads
//! the count on all three of its room routes: the room, the org's room list
//! (the dashboard sums it) and Explore (its "N messages" line).

use bson::{Document, doc, oid::ObjectId};
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

/// A new open room of the org (listed in Explore), made by its admin.
async fn new_room(app: &TestApp, t: &SeededTenant, name: &str) -> String {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/tenant/{}/room", t.tenant_id),
        &t.admin.access_token,
        Some(json!({ "name": name, "is_open": true })),
    )
    .await;
    assert_eq!(status, 200, "room create: {body}");
    body["id"].as_str().expect("the room's id").to_string()
}

/// A message the admin posts in `room`, with `extra` merged into the body.
async fn post(app: &TestApp, t: &SeededTenant, room: &str, content: &str, extra: Value) -> String {
    let mut body = json!({ "content": content });
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        b.extend(e.clone());
    }
    let (status, resp) = call(
        app,
        "POST",
        &format!("/api/tenant/{}/room/{room}/message", t.tenant_id),
        &t.admin.access_token,
        Some(body),
    )
    .await;
    assert_eq!(status, 200, "message posted: {resp}");
    resp["id"].as_str().expect("the message's id").to_string()
}

async fn delete(app: &TestApp, t: &SeededTenant, room: &str, message: &str, token: &str) -> u16 {
    call(
        app,
        "DELETE",
        &format!("/api/tenant/{}/room/{room}/message/{message}", t.tenant_id),
        token,
        None,
    )
    .await
    .0
}

/// The room's live messages, counted in the collection itself.
async fn live_rows(app: &TestApp, room: &str) -> u64 {
    app.db
        .collection::<Document>("messages")
        .count_documents(doc! { "room_id": oid(room), "deleted_at": null })
        .await
        .unwrap()
}

/// `message_count` as each room route answers it for `room`: the room, the
/// org's room list and Explore (searched by the room's own name).
async fn route_counts(app: &TestApp, t: &SeededTenant, room: &str, name: &str) -> [i64; 3] {
    let tid = &t.tenant_id;
    let token = &t.admin.access_token;

    let (status, one) = call(
        app,
        "GET",
        &format!("/api/tenant/{tid}/room/{room}"),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "room: {one}");

    let (status, list) = call(app, "GET", &format!("/api/tenant/{tid}/room"), token, None).await;
    assert_eq!(status, 200, "room list: {list}");
    let in_list = rooms_of(&list)
        .into_iter()
        .find(|r| r["id"] == room)
        .unwrap_or_else(|| panic!("the room list has no {room}: {list}"));

    let (status, found) = call(
        app,
        "GET",
        &format!("/api/tenant/{tid}/room/explore?q={name}"),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "explore: {found}");
    let in_explore = rooms_of(&found)
        .into_iter()
        .find(|r| r["id"] == room)
        .unwrap_or_else(|| panic!("Explore has no {room}: {found}"));

    [&one, &in_list, &in_explore].map(|r| {
        r["message_count"]
            .as_i64()
            .unwrap_or_else(|| panic!("no message_count in {r}"))
    })
}

/// A room list answer: a bare array, or a page with `items`.
fn rooms_of(v: &Value) -> Vec<Value> {
    v.as_array()
        .or_else(|| v["items"].as_array())
        .cloned()
        .unwrap_or_default()
}

/// The live rows read `n`, and so does every room route.
async fn assert_counts(
    app: &TestApp,
    t: &SeededTenant,
    room: &str,
    name: &str,
    n: u64,
    when: &str,
) {
    assert_eq!(live_rows(app, room).await, n, "{when}: live rows");
    let [one, list, explore] = route_counts(app, t, room, name).await;
    let n = n as i64;
    assert_eq!(
        one, n,
        "{when}: the room's message_count against its {n} live messages"
    );
    assert_eq!(
        list, n,
        "{when}: the room list's message_count (the dashboard sums it)"
    );
    assert_eq!(
        explore, n,
        "{when}: Explore's message_count (its \"N messages\" line)"
    );
}

#[tokio::test]
async fn n_posted_read_n_and_one_deleted_reads_n_minus_one() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("msgcnt").await;
    let name = "msgcounted";
    let room = new_room(&app, &t, name).await;
    assert_counts(&app, &t, &room, name, 0, "a new room").await;

    // A post, a quote and a thread reply each count one.
    let first = post(&app, &t, &room, "one", json!({})).await;
    post(
        &app,
        &t,
        &room,
        "two",
        json!({ "referenced_message_id": first }),
    )
    .await;
    let reply = post(&app, &t, &room, "three", json!({ "thread_id": first })).await;
    assert_counts(&app, &t, &room, name, 3, "after 3 posts").await;

    // The delete leaves a tombstone, and the count drops by its one.
    assert_eq!(
        delete(&app, &t, &room, &reply, &t.admin.access_token).await,
        200
    );
    assert_counts(&app, &t, &room, name, 2, "after one of 3 is deleted").await;

    // A second delete of the same message matches no live message: 404, and
    // the count stays.
    assert_eq!(
        delete(&app, &t, &room, &reply, &t.admin.access_token).await,
        404
    );
    assert_counts(&app, &t, &room, name, 2, "after a second delete of it").await;
}

#[tokio::test]
async fn a_refused_delete_leaves_the_count() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("msgcntno").await;
    let name = "msgrefused";
    let room = new_room(&app, &t, name).await;
    let id = post(&app, &t, &room, "kept", json!({})).await;

    // The member is not the author: the delete is refused, nothing moves.
    let (status, body) = call(
        &app,
        "POST",
        &format!("/api/tenant/{}/room/{room}/join", t.tenant_id),
        &t.member.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "join: {body}");
    assert_eq!(
        delete(&app, &t, &room, &id, &t.member.access_token).await,
        403
    );
    assert_counts(&app, &t, &room, name, 1, "after a refused delete").await;
}

#[tokio::test]
async fn a_post_in_another_room_leaves_this_one() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("msgcntxr").await;
    let (a, b) = ("msgroomalpha", "msgroombeta");
    let room_a = new_room(&app, &t, a).await;
    let room_b = new_room(&app, &t, b).await;
    post(&app, &t, &room_a, "in a", json!({})).await;
    post(&app, &t, &room_a, "in a again", json!({})).await;
    post(&app, &t, &room_b, "in b", json!({})).await;
    assert_counts(&app, &t, &room_a, a, 2, "room a").await;
    assert_counts(&app, &t, &room_b, b, 1, "room b").await;
}

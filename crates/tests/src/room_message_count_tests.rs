// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4448 — a room's `message_count` is the number of its live messages.
//!
//! A live message is a `messages` document of the room with no `deleted_at`:
//! a post, a thread reply and a quote each count one, and a delete (a soft
//! delete, which leaves the tombstone) takes its one away. The room API reads
//! the count on all three of its room routes: the room, the org's room list
//! (the dashboard sums it) and Explore (its "N messages" line).
//!
//! The counts stored before the fix (0 over every room's messages) are set
//! right by chat's startup job `room-message-count` (`MessageCountRepair`):
//! bounded, on a compare with the count it read, and right when it runs twice
//! or on two pods at once.

use std::time::Duration;

use bson::{Bson, Document, doc, oid::ObjectId};
use roomler_ai_services::dao::room_message_count::{CURSOR, MessageCountPass, MessageCountRepair};
use roomler_core::{Cadence, Module};
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

// ── The repair of the counts stored before the fix ──────────────

/// A bound no test reaches: the pass stops only at its cap or the last room.
const NO_BUDGET: Duration = Duration::from_secs(3600);

fn repair(app: &TestApp) -> MessageCountRepair {
    MessageCountRepair::new(&app.db)
}

/// Store `n` as the room's count, the way the code before the fix left it
/// (0) or a fault did.
async fn store_count(app: &TestApp, room: &ObjectId, n: i64) {
    app.db
        .collection::<Document>("rooms")
        .update_one(
            doc! { "_id": room },
            doc! { "$set": { "message_count": n } },
        )
        .await
        .unwrap();
}

async fn room_doc(app: &TestApp, room: &str) -> Document {
    app.db
        .collection::<Document>("rooms")
        .find_one(doc! { "_id": oid(room) })
        .await
        .unwrap()
        .expect("the room")
}

/// Every room of the test's database, in `_id` order.
async fn all_rooms(app: &TestApp) -> Vec<ObjectId> {
    let mut ids: Vec<ObjectId> = app
        .db
        .collection::<Document>("rooms")
        .distinct("_id", doc! {})
        .await
        .unwrap()
        .into_iter()
        .filter_map(|v| v.as_object_id())
        .collect();
    ids.sort();
    ids
}

/// The room row's stored `message_count`.
async fn stored(app: &TestApp, room: &ObjectId) -> i64 {
    match room_doc(app, &room.to_hex()).await.get("message_count") {
        Some(Bson::Int32(n)) => i64::from(*n),
        Some(Bson::Int64(n)) => *n,
        other => panic!("message_count is {other:?}"),
    }
}

/// Every room of the database stored at 0, as the code before the fix left
/// it. Returns the rooms, and how many of them hold a live message and so
/// read wrong.
async fn all_at_zero(app: &TestApp) -> (Vec<ObjectId>, u64) {
    let rooms = all_rooms(app).await;
    let mut wrong = 0;
    for room in &rooms {
        store_count(app, room, 0).await;
        if live_rows(app, &room.to_hex()).await > 0 {
            wrong += 1;
        }
    }
    (rooms, wrong)
}

async fn assert_every_room_right(app: &TestApp, when: &str) {
    for room in all_rooms(app).await {
        assert_eq!(
            stored(app, &room).await,
            live_rows(app, &room.to_hex()).await as i64,
            "{when}: room {room}"
        );
    }
}

async fn cursor(app: &TestApp) -> Option<ObjectId> {
    app.db
        .collection::<Document>("locks")
        .find_one(doc! { "_id": CURSOR })
        .await
        .unwrap()
        .and_then(|d| d.get_object_id("after").ok())
}

fn pass_of(read: u64, changed: u64, skipped: u64, more: bool) -> MessageCountPass {
    MessageCountPass {
        read,
        changed,
        skipped,
        more,
    }
}

#[tokio::test]
async fn the_repair_sets_each_count_to_its_live_messages_and_nothing_else() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("msgfix").await;
    let name = "msgrepaired";
    let room = new_room(&app, &t, name).await;
    for content in ["a", "b", "c"] {
        post(&app, &t, &room, content, json!({})).await;
    }
    let gone = post(&app, &t, &room, "d", json!({})).await;
    assert_eq!(
        delete(&app, &t, &room, &gone, &t.admin.access_token).await,
        200
    );
    let empty = new_room(&app, &t, "msgempty").await;
    let (rooms, wrong) = all_at_zero(&app).await;
    assert_eq!(wrong, 1, "only the room with messages reads wrong at 0");
    let before = room_doc(&app, &room).await;
    let before_empty = room_doc(&app, &empty).await;

    let pass = repair(&app).pass().await.unwrap();
    assert_eq!(pass, pass_of(rooms.len() as u64, 1, 0, false));
    // The deleted one is not counted.
    assert_counts(&app, &t, &room, name, 3, "repaired").await;
    let mut now = room_doc(&app, &room).await;
    let mut was = before;
    now.remove("message_count");
    was.remove("message_count");
    assert_eq!(now, was, "the pass wrote only message_count");
    assert_eq!(
        room_doc(&app, &empty).await,
        before_empty,
        "an empty room at 0 is left alone"
    );

    let again = repair(&app).pass().await.unwrap();
    assert_eq!(
        again,
        pass_of(rooms.len() as u64, 0, 0, false),
        "a second pass changes nothing"
    );
}

#[tokio::test]
async fn a_delete_in_a_room_still_at_zero_leaves_it_at_zero_and_the_room_readable() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("msgneg").await;
    let name = "msgatzero";
    let room = new_room(&app, &t, name).await;
    let id = post(&app, &t, &room, "before the fix", json!({})).await;
    post(&app, &t, &room, "also before", json!({})).await;
    // As the code before the fix left it: 0 over two live messages.
    store_count(&app, &oid(&room), 0).await;

    // The delete matches, and the count may not go below 0: a stored -1
    // would fail every read of the room, the org's room list included.
    assert_eq!(
        delete(&app, &t, &room, &id, &t.admin.access_token).await,
        200
    );
    assert_eq!(stored(&app, &oid(&room)).await, 0);
    let counts = route_counts(&app, &t, &room, name).await;
    assert_eq!(counts, [0, 0, 0], "the room still reads, on every route");

    // The next boot's pass sets it to its one live message.
    repair(&app).pass().await.unwrap();
    assert_counts(&app, &t, &room, name, 1, "after the pass").await;
}

#[tokio::test]
async fn a_post_that_lands_after_the_count_is_read_is_not_lost() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("msgrace").await;
    let name = "msgraced";
    let room = new_room(&app, &t, name).await;
    post(&app, &t, &room, "one", json!({})).await;
    post(&app, &t, &room, "two", json!({})).await;
    store_count(&app, &oid(&room), 0).await;

    // What a pass reads: a count of 0 over two live messages.
    let read = room_doc(&app, &room).await.get("message_count").cloned();
    assert_eq!(read, Some(Bson::Int64(0)));

    // Before its write, a third is posted, and the count moves to 1.
    post(&app, &t, &room, "three", json!({})).await;
    assert_eq!(stored(&app, &oid(&room)).await, 1);

    // The write it planned from its read finds the count it read gone.
    let wrote = repair(&app)
        .set_if(&[oid(&room)], read.as_ref(), 2)
        .await
        .unwrap();
    assert_eq!(wrote, 0, "the compare refuses the write");
    assert_eq!(
        stored(&app, &oid(&room)).await,
        1,
        "the post's count is not lost"
    );

    let pass = repair(&app).pass().await.unwrap();
    assert_eq!((pass.changed, pass.skipped), (1, 0));
    assert_counts(&app, &t, &room, name, 3, "after the next pass").await;
}

#[tokio::test]
async fn a_pass_stops_at_its_cap_or_out_of_time_and_the_next_begins_where_it_stopped() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("msgcap").await;
    for name in ["msgc1", "msgc2", "msgc3"] {
        let room = new_room(&app, &t, name).await;
        post(&app, &t, &room, "counted", json!({})).await;
    }
    // Every room one ahead of its live messages, so every one reads wrong.
    let rooms = all_rooms(&app).await;
    for room in &rooms {
        let live = live_rows(&app, &room.to_hex()).await as i64;
        store_count(&app, room, live + 1).await;
    }
    let n = rooms.len() as u64;
    assert!(n > 4, "{n} rooms");

    let first = repair(&app).pass_with(2, 3, NO_BUDGET).await.unwrap();
    assert_eq!(first, pass_of(3, 3, 0, true));
    assert_eq!(
        cursor(&app).await,
        Some(rooms[2]),
        "it stopped at the third room"
    );

    // No time at all: it still finishes its first batch, then stops there.
    let second = repair(&app)
        .pass_with(1, 100, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(second, pass_of(1, 1, 0, true));
    assert_eq!(
        cursor(&app).await,
        Some(rooms[3]),
        "the cursor is the last room it finished"
    );

    let third = repair(&app).pass_with(2, 100, NO_BUDGET).await.unwrap();
    assert_eq!(third, pass_of(n - 4, n - 4, 0, false));
    assert_eq!(
        cursor(&app).await,
        None,
        "a pass that reached the end clears it"
    );
    assert_every_room_right(&app, "after the three passes").await;
}

#[tokio::test]
async fn two_passes_at_once_end_with_the_same_counts() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("msgtwo").await;
    for name in ["msgd1", "msgd2", "msgd3"] {
        let room = new_room(&app, &t, name).await;
        post(&app, &t, &room, "counted", json!({})).await;
        post(&app, &t, &room, "twice", json!({})).await;
    }
    let (rooms, wrong) = all_at_zero(&app).await;
    let n = rooms.len() as u64;

    let (a, b) = (repair(&app), repair(&app));
    let (a, b) = tokio::join!(
        a.pass_with(1, 100, NO_BUDGET),
        b.pass_with(1, 100, NO_BUDGET)
    );
    let (a, b) = (a.expect("pass a"), b.expect("pass b"));
    assert_eq!((a.read, b.read), (n, n));
    assert_eq!(
        a.changed + b.changed,
        wrong,
        "each room set once: {a:?} {b:?}"
    );
    assert_every_room_right(&app, "after two passes at once").await;
}

/// A roll's LAST pod to boot must run the pass, because a pod still on the
/// old code writes messages with no count beside them; gating the job on the
/// startup lease would let only the first pod pass.
#[tokio::test]
async fn the_message_count_job_runs_on_a_pod_without_the_lease_because_every_write_compares_the_count_it_read()
 {
    let app = TestApp::spawn().await;
    let jobs = app
        .state
        .modules
        .chat
        .as_ref()
        .expect("chat is mounted")
        .jobs();
    let job = jobs
        .iter()
        .find(|j| j.name == "room-message-count")
        .expect("chat declares room-message-count");
    assert_eq!(job.cadence, Cadence::AtStartup);
    assert!(!job.leader_gated, "room-message-count is not leader-gated");

    let t = app.seed_tenant("msggate").await;
    let room = new_room(&app, &t, "msggated").await;
    post(&app, &t, &room, "counted", json!({})).await;
    all_at_zero(&app).await;
    // A pod that does not hold the startup lease.
    app.state.modules.run_startup_jobs(false).await;
    assert_every_room_right(&app, "after a boot without the lease").await;
}

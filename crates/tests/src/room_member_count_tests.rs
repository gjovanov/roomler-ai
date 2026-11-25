// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4409 — a room's stored `member_count` is the number of its member rows.
//!
//! A member row is a `room_members` document of the room: the rows the member
//! route lists, and counts as its `total`. Every way a row comes or goes moves
//! the count with it: the room's creation (its creator's row), a join, a
//! leave, an org membership that ends, and a first call join by someone who
//! was not a member yet.
//!
//! The counts stored before the fix are set right by chat's startup job
//! `room-member-count` (`MemberCountRepair`): bounded, on a compare with the
//! count it read, and right when it runs twice or on two pods at once.

use std::time::{Duration, Instant};

use bson::{Bson, Document, doc, oid::ObjectId};
use roomler_ai_services::dao::room_member_count::{
    BATCH, CAP, CURSOR, MemberCountPass, MemberCountRepair,
};
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

// ── The repair of the counts stored wrong ───────────────────────

/// A bound no test reaches: the pass stops only at its cap or the last room.
const NO_BUDGET: Duration = Duration::from_secs(3600);

fn repair(app: &TestApp) -> MemberCountRepair {
    MemberCountRepair::new(&app.db)
}

/// Store `v` as the room's count, the way a fault left it.
async fn store_raw(app: &TestApp, room: &ObjectId, v: Bson) {
    app.db
        .collection::<Document>("rooms")
        .update_one(doc! { "_id": room }, doc! { "$set": { "member_count": v } })
        .await
        .unwrap();
}

async fn store_count(app: &TestApp, room: &ObjectId, n: i64) {
    store_raw(app, room, Bson::Int64(n)).await;
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

/// Every room of the database stored one ahead of its rows, as the code
/// before the fix left it. Returns them.
async fn all_one_ahead(app: &TestApp) -> Vec<ObjectId> {
    let rooms = all_rooms(app).await;
    for room in &rooms {
        let rows = member_rows(app, &room.to_hex()).await;
        store_count(app, room, rows + 1).await;
    }
    rooms
}

async fn assert_every_room_right(app: &TestApp, when: &str) {
    for room in all_rooms(app).await {
        let room = room.to_hex();
        assert_eq!(
            stored_count(app, &room).await,
            member_rows(app, &room).await,
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

fn pass_of(read: u64, changed: u64, skipped: u64, more: bool) -> MemberCountPass {
    MemberCountPass {
        read,
        changed,
        skipped,
        more,
    }
}

#[tokio::test]
async fn the_repair_sets_each_count_to_its_rows_and_nothing_else() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("mcfix").await;
    let over = new_room(&app, &t, "over").await;
    let under = new_room(&app, &t, "under").await;
    post_ok(&app, &t, &under, "join", &t.member.access_token).await;
    let right = new_room(&app, &t, "right").await;
    store_count(&app, &oid(&over), 2).await; // one row: one too many
    store_count(&app, &oid(&under), 1).await; // two rows: one too few
    let mut before = Vec::new();
    for room in [&over, &under, &right] {
        before.push(room_doc(&app, room).await);
    }
    let total = all_rooms(&app).await.len() as u64;

    let pass = repair(&app).pass().await.unwrap();
    assert_eq!(pass, pass_of(total, 2, 0, false));
    assert_counts(&app, &t, &over, 1, "one too many, repaired").await;
    assert_counts(&app, &t, &under, 2, "one too few, repaired").await;
    assert_counts(&app, &t, &right, 1, "right, left alone").await;
    for (room, mut was) in [&over, &under, &right].into_iter().zip(before) {
        let mut now = room_doc(&app, room).await;
        was.remove("member_count");
        now.remove("member_count");
        assert_eq!(now, was, "the pass wrote only member_count");
    }

    let again = repair(&app).pass().await.unwrap();
    assert_eq!(
        again,
        pass_of(total, 0, 0, false),
        "a second pass changes nothing"
    );
    assert_eq!(cursor(&app).await, None);
}

#[tokio::test]
async fn a_stored_count_that_is_missing_negative_or_not_whole_is_set_to_its_rows() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("mcodd").await;
    let missing = new_room(&app, &t, "missing").await;
    let negative = new_room(&app, &t, "negative").await;
    let fraction = new_room(&app, &t, "fraction").await;
    let text = new_room(&app, &t, "text").await;
    let whole = new_room(&app, &t, "whole").await;
    app.db
        .collection::<Document>("rooms")
        .update_one(
            doc! { "_id": oid(&missing) },
            doc! { "$unset": { "member_count": "" } },
        )
        .await
        .unwrap();
    store_raw(&app, &oid(&negative), Bson::Int64(-1)).await;
    store_raw(&app, &oid(&fraction), Bson::Double(1.5)).await;
    store_raw(&app, &oid(&text), Bson::String("1".into())).await;
    // A whole double that equals the rows is a right count, left alone.
    store_raw(&app, &oid(&whole), Bson::Double(1.0)).await;

    let pass = repair(&app).pass().await.unwrap();
    assert_eq!((pass.changed, pass.skipped, pass.more), (4, 0, false));
    for room in [&missing, &negative, &fraction, &text] {
        assert_counts(&app, &t, room, 1, "an odd count, repaired").await;
    }
    assert_eq!(
        room_doc(&app, &whole).await.get("member_count"),
        Some(&Bson::Double(1.0)),
        "a whole double equal to the rows is not rewritten"
    );
    let again = repair(&app).pass().await.unwrap();
    assert_eq!(again.changed, 0, "a second pass changes nothing");
}

#[tokio::test]
async fn a_count_that_moves_between_the_read_and_the_write_is_left_for_the_next_pass() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("mcrace").await;
    let room = new_room(&app, &t, "raced").await;
    store_count(&app, &oid(&room), 3).await;
    // What a pass reads: a count of 3 over one row.
    let read = room_doc(&app, &room).await.get("member_count").cloned();
    assert_eq!(read, Some(Bson::Int64(3)));
    assert_eq!(member_rows(&app, &room).await, 1);

    // Before its write, someone joins: a second row, and the count to 4.
    post_ok(&app, &t, &room, "join", &t.member.access_token).await;
    assert_eq!(stored_count(&app, &room).await, 4);

    // The write it planned from its read finds the count it read gone.
    let wrote = repair(&app)
        .set_if(&[oid(&room)], read.as_ref(), 1)
        .await
        .unwrap();
    assert_eq!(wrote, 0, "the compare refuses the write");
    assert_eq!(stored_count(&app, &room).await, 4, "the join is not lost");

    // The next pass reads the room again and sets it to its rows.
    let pass = repair(&app).pass().await.unwrap();
    assert_eq!((pass.changed, pass.skipped), (1, 0));
    assert_counts(&app, &t, &room, 2, "after the next pass").await;
}

#[tokio::test]
async fn a_pass_stops_at_its_cap_and_the_next_begins_where_it_stopped() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("mccap").await;
    new_room(&app, &t, "c1").await;
    new_room(&app, &t, "c2").await;
    let rooms = all_one_ahead(&app).await;
    let n = rooms.len() as u64;
    assert!(n > 3, "{n} rooms");

    let first = repair(&app).pass_with(2, 3, NO_BUDGET).await.unwrap();
    assert_eq!(first, pass_of(3, 3, 0, true));
    assert_eq!(
        cursor(&app).await,
        Some(rooms[2]),
        "it stopped at the third room"
    );
    assert_eq!(
        stored_count(&app, &rooms[3].to_hex()).await,
        member_rows(&app, &rooms[3].to_hex()).await + 1,
        "a room past the cap waits"
    );

    let second = repair(&app).pass_with(2, 3, NO_BUDGET).await.unwrap();
    assert_eq!(second, pass_of(n - 3, n - 3, 0, false));
    assert_eq!(
        cursor(&app).await,
        None,
        "a pass that reached the end clears it"
    );
    assert_every_room_right(&app, "after the two passes").await;

    let third = repair(&app).pass_with(2, 100, NO_BUDGET).await.unwrap();
    assert_eq!(
        third,
        pass_of(n, 0, 0, false),
        "the pass after begins at the first room again"
    );
}

#[tokio::test]
async fn a_pass_out_of_time_stops_after_the_batch_it_finished() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("mctime").await;
    new_room(&app, &t, "t1").await;
    let rooms = all_one_ahead(&app).await;
    let n = rooms.len() as u64;
    assert!(n > 2, "{n} rooms");

    // No time at all: it still finishes its first batch, then stops there.
    let first = repair(&app)
        .pass_with(2, 100, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(first, pass_of(2, 2, 0, true));
    assert_eq!(
        cursor(&app).await,
        Some(rooms[1]),
        "the cursor is the last room it finished"
    );

    let second = repair(&app).pass_with(2, 100, NO_BUDGET).await.unwrap();
    assert_eq!(second, pass_of(n - 2, n - 2, 0, false));
    assert_eq!(
        cursor(&app).await,
        None,
        "a pass that reached the end clears it"
    );
    assert_every_room_right(&app, "after the two passes").await;
}

#[tokio::test]
async fn two_passes_at_once_end_with_the_same_rows() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("mctwo").await;
    for name in ["d1", "d2", "d3"] {
        new_room(&app, &t, name).await;
    }
    post_ok(&app, &t, &t.rooms[0].id, "join", &t.member.access_token).await;
    let n = all_one_ahead(&app).await.len() as u64;

    let (a, b) = (repair(&app), repair(&app));
    let (a, b) = tokio::join!(
        a.pass_with(1, 100, NO_BUDGET),
        b.pass_with(1, 100, NO_BUDGET)
    );
    let (a, b) = (a.expect("pass a"), b.expect("pass b"));
    assert_eq!((a.read, b.read), (n, n));
    assert_eq!(a.changed + b.changed, n, "each room set once: {a:?} {b:?}");
    assert_every_room_right(&app, "after two passes at once").await;
}

#[tokio::test]
async fn a_room_the_old_code_makes_after_a_pass_is_set_right_by_the_next_boot() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("mcroll").await;
    let n = all_one_ahead(&app).await.len() as u64;

    // The first pod of a roll boots on the fix and passes.
    let first = repair(&app).pass().await.unwrap();
    assert_eq!(first, pass_of(n, n, 0, false));

    // The pod still on the old code makes a room one ahead.
    let late = new_room(&app, &t, "late").await;
    store_count(&app, &oid(&late), 2).await;

    // The second pod boots on the fix and passes.
    let second = repair(&app).pass().await.unwrap();
    assert_eq!(second, pass_of(n + 1, 1, 0, false));
    assert_counts(&app, &t, &late, 1, "after the second pod's pass").await;

    let third = repair(&app).pass().await.unwrap();
    assert_eq!(
        third,
        pass_of(n + 1, 0, 0, false),
        "a third changes nothing"
    );
}

/// The gate is the point of this test's name: a pass compares each write with
/// the count it read, so it is right beside a live pod's own joins and leaves
/// and beside a second pass, and a roll's LAST pod to boot must run it (the
/// first runs while the old code still makes rooms one ahead). Gating it on
/// the startup lease would let only the first pod pass.
#[tokio::test]
async fn the_member_count_job_runs_on_a_pod_without_the_lease_because_every_write_compares_the_count_it_read()
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
        .find(|j| j.name == "room-member-count")
        .expect("chat declares room-member-count");
    assert_eq!(job.cadence, Cadence::AtStartup);
    assert!(!job.leader_gated, "room-member-count is not leader-gated");

    app.seed_tenant("mcgate").await;
    all_one_ahead(&app).await;
    // A pod that does not hold the startup lease.
    app.state.modules.run_startup_jobs(false).await;
    assert_every_room_right(&app, "after a boot without the lease").await;
}

/// A measurement, run by hand: `--ignored --nocapture`. It prints one line a
/// pass: the rooms, what changed, and the wall time.
#[tokio::test]
#[ignore = "a measurement of a pass at its cap, run by hand"]
async fn measure_a_pass_over_the_cap() {
    let app = TestApp::spawn().await;
    let tenant = ObjectId::new();
    let rooms_c = app.db.collection::<Document>("rooms");
    let members_c = app.db.collection::<Document>("room_members");
    let ids: Vec<ObjectId> = (0..CAP).map(|_| ObjectId::new()).collect();
    let rows_of = |i: usize| (1 + i % 4) as i64;
    for (c, chunk) in ids.chunks(2_000).enumerate() {
        let mut rooms = Vec::new();
        let mut members = Vec::new();
        for (k, id) in chunk.iter().enumerate() {
            let i = c * 2_000 + k;
            rooms.push(doc! {
                "_id": id, "tenant_id": tenant, "path": format!("m{i}"),
                "member_count": rows_of(i) + 1,
            });
            for _ in 0..rows_of(i) {
                members
                    .push(doc! { "room_id": id, "user_id": ObjectId::new(), "tenant_id": tenant });
            }
        }
        rooms_c.insert_many(rooms).await.unwrap();
        members_c.insert_many(members).await.unwrap();
    }
    let total = all_rooms(&app).await.len();

    let line = |what: &str, pass: MemberCountPass, ms: u128| {
        let per = if pass.changed > 0 {
            format!("{:.3}", ms as f64 / pass.changed as f64)
        } else {
            "-".into()
        };
        println!(
            "W58-MEASURE {what}: rooms={total} read={} changed={} skipped={} more={} ms={ms} ms_per_changed={per}",
            pass.read, pass.changed, pass.skipped, pass.more
        );
    };

    // Every room one ahead, as the code before the fix left them. A pass that
    // reads exactly its cap says `more`, so the next one reads what is left.
    loop {
        let t0 = Instant::now();
        let p = repair(&app).pass().await.unwrap();
        line("one-ahead", p, t0.elapsed().as_millis());
        if !p.more {
            break;
        }
    }
    // Every room right: a pass reads them all and writes nothing.
    let t0 = Instant::now();
    let p = repair(&app).pass().await.unwrap();
    line("all-right", p, t0.elapsed().as_millis());

    // The worst shape: every room of a batch its own (count read, rows) pair,
    // so one write a room.
    for (i, id) in ids.iter().enumerate() {
        rooms_c
            .update_one(
                doc! { "_id": id },
                doc! { "$set": { "member_count": rows_of(i) + 1 + (i % BATCH) as i64 } },
            )
            .await
            .unwrap();
    }
    loop {
        let t0 = Instant::now();
        let p = repair(&app).pass().await.unwrap();
        line("distinct", p, t0.elapsed().as_millis());
        if !p.more {
            break;
        }
    }
    assert_every_room_right(&app, "after the measurement").await;
}

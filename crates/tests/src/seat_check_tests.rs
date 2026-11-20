// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3950 — the follow-ups of the seat check (`conference/src/seat_check.rs`).
//!
//! What these pin, each under the check's one invariant (no change may end a
//! seat whose admission holds):
//!
//! - a document that does not decode costs only the seats it bears on their
//!   verdict, and blinds the check to no other seat (T1-T4);
//! - a read that fails or answers late ends nothing and keeps the count (T5,
//!   T6);
//! - one person on two connections loses both seats, counted once (T7);
//! - an ended seat settles its call once (T8);
//! - the room guard decides each end, whatever the reads found (T9, T10);
//! - a pass ends only the seats it read (T11, T12), and a withdrawal that
//!   reached them first leaves it nothing to end (T13).
//!
//! The check's own loop runs beside these on every pod, every 10 s, with a
//! memory of its own, so it ends a seat no sooner than 10 s after its
//! admission goes. Each cell drives its passes directly, asserts what its own
//! passes end, and finishes within 10 s of the change it makes.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bson::{Bson, Document, doc};
use roomler_ai_mod_conference::ConferenceState;
use roomler_ai_mod_conference::guards::require_room_in_tenant;
use roomler_ai_mod_conference::seat_check::{
    Gone, ODD_DOCUMENT, READ_TIMEOUT, Reading, SeatCheckMemory, SeatKey, SeatRead, Verdict,
    check_seats_once, end_seats, read_readings, read_seats,
};
use roomler_ai_services::dao::{room::RoomDao, tenant::TenantDao};
use roomler_core::ApiError;
use tracing::instrument::WithSubscriber;

use crate::cluster_tests::{
    UserWs, assert_seated, conf, connect_user_ws_conn, delete_org_row, delete_room_row,
    frame_of_within, frames_within, heard_no_call_end, join_call, next_frame_of, oid_of,
    passes_end_nothing, seat_in_call, send_media, set_visibility, start_call_in_new_room,
};
use crate::fixtures::seed::{SeededTenant, SeededUser};
use crate::fixtures::test_app::TestApp;

/// The wording, and the alert, of the warning for a seat the reads found gone
/// and the room guard still admits.
const GUARD_ADMITS: &str =
    "seat check: the room guard admits a seat its reads found gone; the seat stays";
const GUARD_ADMITS_ALERT: &str = "seat_check_guard_admits";

/// The seat key of `user_id` in `room_id`.
fn key(room_id: &str, user_id: &str) -> SeatKey {
    (oid_of(room_id), oid_of(user_id))
}

/// The seat keys a read half's verdicts name.
fn keys(read: &SeatRead) -> Vec<SeatKey> {
    read.verdicts.iter().map(|verdict| verdict.key).collect()
}

/// The cell ran inside the window in which the check's own loop cannot have
/// ended the seat it is about: less than 10 s from the change it made.
fn within_the_loops_window(since: Instant) {
    let took = since.elapsed();
    assert!(
        took < Duration::from_secs(10),
        "the cell took {took:?} from its change: the check's own loop may have acted"
    );
}

/// A third person in `tenant`'s org, as a plain member.
async fn third_member(app: &TestApp, tenant: &SeededTenant, name: &str) -> SeededUser {
    let slug = &tenant.tenant_slug;
    let user = app
        .register_user(
            &format!("{name}@{slug}.test"),
            &format!("{slug}_{name}"),
            name,
            "Member123!",
            None,
            None,
        )
        .await;
    let role: Document = app
        .db
        .collection::<Document>("roles")
        .find_one(doc! { "tenant_id": oid_of(&tenant.tenant_id), "name": "member" })
        .await
        .unwrap()
        .expect("the org's member role");
    conf(app)
        .tenants
        .add_member(
            oid_of(&tenant.tenant_id),
            oid_of(&user.id),
            vec![role.get_object_id("_id").unwrap()],
            None,
        )
        .await
        .unwrap();
    user
}

fn org_filter(tenant_id: &str, user_id: &str) -> Document {
    doc! { "tenant_id": oid_of(tenant_id), "user_id": oid_of(user_id) }
}

fn room_filter(room_id: &str, user_id: &str) -> Document {
    doc! { "room_id": oid_of(room_id), "user_id": oid_of(user_id) }
}

/// The one row `filter` names in `collection`, whole: what a raw write takes
/// away and may put back.
async fn row_of(app: &TestApp, collection: &str, filter: Document) -> Document {
    app.db
        .collection::<Document>(collection)
        .find_one(filter.clone())
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("a {collection} row for {filter}"))
}

/// Write `row` into `collection` raw, the way a path that tells no hook would.
async fn put_row(app: &TestApp, collection: &str, row: Document) {
    app.db
        .collection::<Document>(collection)
        .insert_one(row)
        .await
        .unwrap();
}

/// `state` whose own reads (`core.db`) go to `db`; everything else as it is.
fn reading_from(state: &ConferenceState, db: &mongodb::Database) -> ConferenceState {
    let mut st = state.clone();
    st.core.db = db.clone();
    st
}

/// `state` in which whatever the room guard reads goes to `db`: the database
/// handle, and the two DAOs the guard is called with.
fn guarded_from(state: &ConferenceState, db: &mongodb::Database) -> ConferenceState {
    let mut st = reading_from(state, db);
    st.core.tenants = Arc::new(TenantDao::new(db));
    st.rooms = Arc::new(RoomDao::new(db));
    st
}

/// A database on a port nothing listens on: every read on it fails within a
/// third of a second.
async fn refusing_db() -> mongodb::Database {
    mongodb::Client::with_uri_str(
        "mongodb://127.0.0.1:1/?directConnection=true&serverSelectionTimeoutMS=300&connectTimeoutMS=300",
    )
    .await
    .expect("a client is made without a server")
    .database("seat_check_refused")
}

/// A server that takes every connection and never answers, until dropped: a
/// read on it waits for as long as whoever asked is willing to.
struct SilentServer {
    db: mongodb::Database,
    holder: tokio::task::JoinHandle<()>,
}

impl SilentServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let holder = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let db = mongodb::Client::with_uri_str(format!(
            "mongodb://127.0.0.1:{port}/?directConnection=true&serverSelectionTimeoutMS=60000&connectTimeoutMS=60000"
        ))
        .await
        .expect("a client is made without a server")
        .database("seat_check_silent");
        Self { db, holder }
    }
}

impl Drop for SilentServer {
    fn drop(&mut self) {
        self.holder.abort();
    }
}

/// What the code under a test logs while it runs inside
/// [`Captured::dispatch`].
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    /// A subscriber that writes every event, debug and above, here.
    fn dispatch(&self) -> tracing::Dispatch {
        let sink = self.clone();
        tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(move || sink.clone())
                .finish(),
        )
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// `media:join` on `ws`, whatever it held before: the new seat's send
/// transport id, which is new for every join.
async fn join_media(ws: &mut UserWs, room_id: &str) -> String {
    send_media(ws, "media:join", room_id).await;
    let created = next_frame_of(ws, "media:transport_created").await;
    created["data"]["send_transport"]["id"]
        .as_str()
        .expect("transport_created names its send transport")
        .to_string()
}

/// Another socket of `token`'s account in `room_id`'s media on `app`, on the
/// call session the account already holds. Returns it with its connection.
async fn seat_again(app: &TestApp, room_id: &str, token: &str) -> (UserWs, String) {
    let (mut ws, conn) = connect_user_ws_conn(app, token).await;
    join_media(&mut ws, room_id).await;
    assert_seated(app, &conn, room_id, "another socket of the same person");
    (ws, conn)
}

/// The frames of type `kind` about `room_id` that `ws` hears within `ms`.
async fn heard_within(
    ws: &mut UserWs,
    kind: &str,
    room_id: &str,
    ms: u64,
) -> Vec<serde_json::Value> {
    frames_within(ws, ms)
        .await
        .into_iter()
        .filter(|f| f["type"] == kind && f["data"]["room_id"] == room_id)
        .collect()
}

/// `ws`'s seat in `room_id` ended as a lost admission ends one: its call
/// ended for it with `reason: "not_a_member"`, once. (A call that ends
/// because nobody is left in it says so without a reason.)
async fn heard_seat_end_once(ws: &mut UserWs, room_id: &str, what: &str) {
    let heard = frame_of_within(ws, "room:call_ended", Duration::from_secs(5))
        .await
        .unwrap_or_else(|| panic!("{what}: no room:call_ended within 5 s"));
    assert_eq!(heard["data"]["room_id"], room_id, "{what}: {heard}");
    assert_eq!(heard["data"]["reason"], "not_a_member", "{what}: {heard}");
    let again: Vec<serde_json::Value> = heard_within(ws, "room:call_ended", room_id, 500)
        .await
        .into_iter()
        .filter(|f| f["data"]["reason"] == "not_a_member")
        .collect();
    assert!(again.is_empty(), "{what}: its seat ended again: {again:?}");
}

/// No seat of `conn` is held any more.
fn assert_unseated(app: &TestApp, conn: &str, what: &str) {
    assert_eq!(
        conf(app).room_manager.get_connection_room(conn),
        None,
        "{what}: the seat is still held"
    );
}

/// The room guard's answer, for the log.
fn answer<T>(guard: &Result<T, ApiError>) -> String {
    match guard {
        Ok(_) => "admits".to_string(),
        Err(e) => format!("{e:?}"),
    }
}

/// T1. A room document that does not decode costs the seats in that room
/// their verdict and no other seat: a seat in another room of the org whose
/// admission is gone still ends on its second reading, and the seat in the
/// room that does not decode holds over four passes.
#[tokio::test]
async fn a_room_document_that_does_not_decode_costs_only_its_own_seats_their_verdict() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatoddroom").await;
    let (a, b) = (&tenant.admin, &tenant.member);
    let r1 = start_call_in_new_room(&app, &tenant, "R1").await;
    let r2 = start_call_in_new_room(&app, &tenant, "R2").await;
    let (mut in_r1, conn_a) = seat_in_call(&app, &tenant.tenant_id, &r1, &a.access_token).await;
    let (mut in_r2, conn_b) = seat_in_call(&app, &tenant.tenant_id, &r2, &b.access_token).await;

    // R1's visibility becomes a value the room model does not know, and B's
    // org row goes.
    set_visibility(&app, &r1, "unheard-of").await;
    delete_org_row(&app, &tenant.tenant_id, &b.id).await;
    let since = Instant::now();

    let read = read_readings(state, &[key(&r1, &a.id), key(&r2, &b.id)]).await;
    eprintln!("T1 the read: {read:?}");
    let mut memory = SeatCheckMemory::default();
    passes_end_nothing(&app, &mut memory, 1, "B's first reading").await;
    let ended = check_seats_once(state, &mut memory).await;
    assert_eq!(
        ended,
        vec![key(&r2, &b.id)],
        "B's seat in R2 ends on its second reading, whatever R1's document holds; the read: {read:?}"
    );
    heard_seat_end_once(&mut in_r2, &r2, "B in R2").await;
    assert_unseated(&app, &conn_b, "B in R2");

    passes_end_nothing(&app, &mut memory, 2, "passes 3 and 4").await;
    assert_seated(
        &app,
        &conn_a,
        &r1,
        "A, in the room whose document does not decode",
    );
    heard_no_call_end(&mut in_r1, &r1, "A in R1").await;
    within_the_loops_window(since);
    in_r1.close(None).await.ok();
    in_r2.close(None).await.ok();
}

/// T2. An org row that does not decode (its `tenant_id` an array that holds
/// the org) still admits its person through the room guard, which counts it,
/// so the check gives that person's seats no verdict, and goes on reading
/// every other seat: a seat in the same room whose org row is gone ends on
/// its second reading.
#[tokio::test]
async fn an_org_row_that_does_not_decode_keeps_the_seats_it_could_admit_and_blinds_no_other() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatoddorg").await;
    let tid = oid_of(&tenant.tenant_id);
    let (a, b) = (&tenant.admin, &tenant.member);
    let c = third_member(&app, &tenant, "carol").await;
    let r1 = start_call_in_new_room(&app, &tenant, "R1").await;
    let (mut host, conn_a) = seat_in_call(&app, &tenant.tenant_id, &r1, &a.access_token).await;
    let (mut in_b, conn_b) = seat_in_call(&app, &tenant.tenant_id, &r1, &b.access_token).await;
    let (mut in_c, conn_c) = seat_in_call(&app, &tenant.tenant_id, &r1, &c.access_token).await;

    // B's org row becomes one whose `tenant_id` is an array holding the org.
    // The unique `(tenant_id, user_id)` index holds array elements too, so
    // the real row goes first.
    let mut odd = row_of(&app, "tenant_members", org_filter(&tenant.tenant_id, &b.id)).await;
    delete_org_row(&app, &tenant.tenant_id, &b.id).await;
    odd.insert("tenant_id", Bson::Array(vec![Bson::ObjectId(tid)]));
    put_row(&app, "tenant_members", odd).await;
    let guard = require_room_in_tenant(state, tid, oid_of(&r1), oid_of(&b.id)).await;
    eprintln!("T2 the room guard on B: {}", answer(&guard));
    assert!(
        guard.is_ok(),
        "the room guard admits B through the odd row: {}",
        answer(&guard)
    );

    delete_org_row(&app, &tenant.tenant_id, &c.id).await;
    let since = Instant::now();
    let seats = [key(&r1, &a.id), key(&r1, &b.id), key(&r1, &c.id)];
    let read = read_readings(state, &seats).await;
    eprintln!("T2 the read: {read:?}");
    let Ok(readings) = &read else {
        panic!("one odd org row failed the whole read, so no seat can end: {read:?}");
    };
    assert!(
        !matches!(readings.get(&key(&r1, &b.id)), Some(Reading::Gone(_))),
        "the check reads B's admission gone though the room guard admits B: {readings:?}"
    );
    assert_eq!(
        readings.get(&key(&r1, &c.id)),
        Some(&Reading::Gone(Gone::NotInOrg)),
        "C, whose org row is gone: {readings:?}"
    );
    assert_eq!(
        readings.get(&key(&r1, &a.id)),
        Some(&Reading::Admitted),
        "A: {readings:?}"
    );

    let mut memory = SeatCheckMemory::default();
    passes_end_nothing(&app, &mut memory, 1, "C's first reading").await;
    let ended = check_seats_once(state, &mut memory).await;
    assert_eq!(
        ended,
        vec![key(&r1, &c.id)],
        "C's second reading ends C's seat alone"
    );
    heard_seat_end_once(&mut in_c, &r1, "C").await;
    assert_unseated(&app, &conn_c, "C");
    passes_end_nothing(&app, &mut memory, 2, "passes 3 and 4").await;
    assert_seated(&app, &conn_b, &r1, "B, whom the odd row admits");
    assert_seated(&app, &conn_a, &r1, "A");
    heard_no_call_end(&mut in_b, &r1, "B").await;
    within_the_loops_window(since);
    for ws in [&mut host, &mut in_b, &mut in_c] {
        ws.close(None).await.ok();
    }
}

/// T3. A room row that does not decode (its `room_id` an array that holds
/// the room) is one the room guard cannot read either (its typed read fails),
/// so the check gives the seats it could admit no verdict, and goes on reading
/// every other seat: a seat in another members-only room whose row is gone
/// ends on its second reading.
#[tokio::test]
async fn a_room_row_that_does_not_decode_keeps_the_seats_it_could_admit_and_blinds_no_other() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatoddrow").await;
    let tid = oid_of(&tenant.tenant_id);
    let (a, b) = (&tenant.admin, &tenant.member);
    let c = third_member(&app, &tenant, "carol").await;
    let r1 = start_call_in_new_room(&app, &tenant, "R1").await;
    let r2 = start_call_in_new_room(&app, &tenant, "R2").await;
    let (mut host, conn_a) = seat_in_call(&app, &tenant.tenant_id, &r1, &a.access_token).await;
    let (mut in_b, conn_b) = seat_in_call(&app, &tenant.tenant_id, &r1, &b.access_token).await;
    let (mut in_c, conn_c) = seat_in_call(&app, &tenant.tenant_id, &r2, &c.access_token).await;
    set_visibility(&app, &r1, "private").await;
    set_visibility(&app, &r2, "private").await;

    // B's row in R1 becomes one whose `room_id` is an array holding R1.
    let mut odd = row_of(&app, "room_members", room_filter(&r1, &b.id)).await;
    delete_room_row(&app, &r1, &b.id).await;
    odd.insert("room_id", Bson::Array(vec![Bson::ObjectId(oid_of(&r1))]));
    put_row(&app, "room_members", odd).await;
    let guard = require_room_in_tenant(state, tid, oid_of(&r1), oid_of(&b.id)).await;
    eprintln!("T3 the room guard on B: {}", answer(&guard));
    assert!(
        matches!(guard, Err(ApiError::Internal(_))),
        "the room guard's typed read of the odd row fails: {}",
        answer(&guard)
    );

    delete_room_row(&app, &r2, &c.id).await;
    let since = Instant::now();
    let seats = [key(&r1, &a.id), key(&r1, &b.id), key(&r2, &c.id)];
    let read = read_readings(state, &seats).await;
    eprintln!("T3 the read: {read:?}");
    let Ok(readings) = &read else {
        panic!("one odd room row failed the whole read, so no seat can end: {read:?}");
    };
    assert!(
        !matches!(readings.get(&key(&r1, &b.id)), Some(Reading::Gone(_))),
        "the check reads B's admission gone from a row it cannot decode: {readings:?}"
    );
    assert_eq!(
        readings.get(&key(&r2, &c.id)),
        Some(&Reading::Gone(Gone::NotInRoom)),
        "C, whose row in R2 is gone: {readings:?}"
    );
    assert_eq!(
        readings.get(&key(&r1, &a.id)),
        Some(&Reading::Admitted),
        "A: {readings:?}"
    );

    let mut memory = SeatCheckMemory::default();
    passes_end_nothing(&app, &mut memory, 1, "C's first reading").await;
    let ended = check_seats_once(state, &mut memory).await;
    assert_eq!(
        ended,
        vec![key(&r2, &c.id)],
        "C's second reading ends C's seat alone"
    );
    heard_seat_end_once(&mut in_c, &r2, "C").await;
    assert_unseated(&app, &conn_c, "C");
    passes_end_nothing(&app, &mut memory, 2, "passes 3 and 4").await;
    assert_seated(&app, &conn_b, &r1, "B, whose row does not decode");
    assert_seated(&app, &conn_a, &r1, "A");
    heard_no_call_end(&mut in_b, &r1, "B").await;
    within_the_loops_window(since);
    for ws in [&mut host, &mut in_b, &mut in_c] {
        ws.close(None).await.ok();
    }
}

/// T4. A room row with no `tenant_id` is not read: the check asks for the
/// rows of the room's org, as `RoomDao::is_member` does, so a row the guard's
/// own filter never matches cannot fail the read.
#[tokio::test]
async fn a_room_row_without_its_org_is_not_read() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatnoorg").await;
    let (a, x) = (&tenant.admin, &tenant.member);
    let y = third_member(&app, &tenant, "yara").await;
    let r1 = start_call_in_new_room(&app, &tenant, "R1").await;
    let r2 = start_call_in_new_room(&app, &tenant, "R2").await;
    let (mut host, conn_a) = seat_in_call(&app, &tenant.tenant_id, &r1, &a.access_token).await;
    let (mut in_y, conn_y) = seat_in_call(&app, &tenant.tenant_id, &r1, &y.access_token).await;
    let (mut in_x, conn_x) = seat_in_call(&app, &tenant.tenant_id, &r2, &x.access_token).await;
    set_visibility(&app, &r1, "private").await;
    set_visibility(&app, &r2, "private").await;

    // X's row in R2 goes, and a row for (R1, X) with no org is written: R1
    // and X are both among the ids the check asks about.
    delete_room_row(&app, &r2, &x.id).await;
    put_row(
        &app,
        "room_members",
        doc! {
            "room_id": oid_of(&r1),
            "user_id": oid_of(&x.id),
            "joined_at": bson::DateTime::now(),
        },
    )
    .await;
    let since = Instant::now();
    let seats = [key(&r1, &a.id), key(&r1, &y.id), key(&r2, &x.id)];
    let read = read_readings(state, &seats).await;
    eprintln!("T4 the read: {read:?}");

    let mut memory = SeatCheckMemory::default();
    passes_end_nothing(&app, &mut memory, 1, "X's first reading").await;
    let ended = check_seats_once(state, &mut memory).await;
    assert_eq!(
        ended,
        vec![key(&r2, &x.id)],
        "X's seat in R2 ends on its second reading; the read: {read:?}"
    );
    heard_seat_end_once(&mut in_x, &r2, "X").await;
    assert_unseated(&app, &conn_x, "X");
    passes_end_nothing(&app, &mut memory, 2, "passes 3 and 4").await;
    assert_seated(&app, &conn_y, &r1, "Y");
    assert_seated(&app, &conn_a, &r1, "A");
    heard_no_call_end(&mut in_y, &r1, "Y").await;
    within_the_loops_window(since);
    for ws in [&mut host, &mut in_y, &mut in_x] {
        ws.close(None).await.ok();
    }
}

/// T5. A read that fails ends no seat and keeps the count: the miss before
/// it still counts, so the next complete read that finds the admission gone
/// ends the seat.
#[tokio::test]
async fn a_failed_read_ends_no_seat_and_keeps_the_count() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatfailed").await;
    let member = &tenant.member;
    let r = start_call_in_new_room(&app, &tenant, "R").await;
    let (mut host, _) = seat_in_call(&app, &tenant.tenant_id, &r, &tenant.admin.access_token).await;
    let (mut ws, conn) = seat_in_call(&app, &tenant.tenant_id, &r, &member.access_token).await;
    let refused = reading_from(state, &refusing_db().await);

    delete_org_row(&app, &tenant.tenant_id, &member.id).await;
    let since = Instant::now();
    let mut memory = SeatCheckMemory::default();
    passes_end_nothing(&app, &mut memory, 1, "the first reading").await;
    let read = read_seats(&refused, &mut memory).await;
    eprintln!("T5 the failed read: {:?}", read.failed);
    assert!(read.failed.is_some(), "a read on a refusing server fails");
    assert!(read.verdicts.is_empty(), "a failed read gives no verdict");
    let ended = end_seats(&refused, &mut memory, &read).await;
    assert!(ended.is_empty(), "a failed read ended {ended:?}");
    assert_seated(&app, &conn, &r, "the member, after the failed read");

    let ended = check_seats_once(state, &mut memory).await;
    assert_eq!(
        ended,
        vec![key(&r, &member.id)],
        "the next complete read ends the seat: the miss before the failed read was kept"
    );
    heard_seat_end_once(&mut ws, &r, "the member").await;
    within_the_loops_window(since);
    host.close(None).await.ok();
    ws.close(None).await.ok();
}

/// T6. A read that answers late ends no seat and keeps the count: it fails at
/// `READ_TIMEOUT` with "no answer within", and the next complete read ends
/// the seat.
#[tokio::test]
async fn a_late_read_ends_no_seat_and_keeps_the_count() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatlate").await;
    let member = &tenant.member;
    let r = start_call_in_new_room(&app, &tenant, "R").await;
    let (mut host, _) = seat_in_call(&app, &tenant.tenant_id, &r, &tenant.admin.access_token).await;
    let (mut ws, conn) = seat_in_call(&app, &tenant.tenant_id, &r, &member.access_token).await;
    let silent = SilentServer::start().await;
    let late = reading_from(state, &silent.db);

    delete_org_row(&app, &tenant.tenant_id, &member.id).await;
    let since = Instant::now();
    let mut memory = SeatCheckMemory::default();
    passes_end_nothing(&app, &mut memory, 1, "the first reading").await;
    let asked = Instant::now();
    let read = read_seats(&late, &mut memory).await;
    let took = asked.elapsed();
    eprintln!("T6 the late read took {took:?}: {:?}", read.failed);
    assert!(
        took >= READ_TIMEOUT,
        "the read waited out READ_TIMEOUT: {took:?}"
    );
    assert!(
        read.failed
            .as_deref()
            .is_some_and(|e| e.contains("no answer within")),
        "a late read fails as late: {:?}",
        read.failed
    );
    let ended = end_seats(&late, &mut memory, &read).await;
    assert!(ended.is_empty(), "a late read ended {ended:?}");
    assert_seated(&app, &conn, &r, "the member, after the late read");

    let ended = check_seats_once(state, &mut memory).await;
    assert_eq!(
        ended,
        vec![key(&r, &member.id)],
        "the next complete read ends the seat: the miss before the late read was kept"
    );
    heard_seat_end_once(&mut ws, &r, "the member").await;
    within_the_loops_window(since);
    drop(silent);
    host.close(None).await.ok();
    ws.close(None).await.ok();
}

/// T7. One person on two connections in one call loses both seats, counted
/// once: the pass returns their key once, each socket hears its seat end
/// once, and the passes after end nothing.
#[tokio::test]
async fn one_person_on_two_connections_loses_both_seats_counted_once() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seattwoconns").await;
    let member = &tenant.member;
    let r = start_call_in_new_room(&app, &tenant, "R").await;
    let (mut host, _) = seat_in_call(&app, &tenant.tenant_id, &r, &tenant.admin.access_token).await;
    let (mut first, conn1) = seat_in_call(&app, &tenant.tenant_id, &r, &member.access_token).await;
    let (mut second, conn2) = seat_again(&app, &r, &member.access_token).await;

    delete_org_row(&app, &tenant.tenant_id, &member.id).await;
    let since = Instant::now();
    let mut memory = SeatCheckMemory::default();
    passes_end_nothing(&app, &mut memory, 1, "the first reading").await;
    let ended = check_seats_once(state, &mut memory).await;
    assert_eq!(
        ended,
        vec![key(&r, &member.id)],
        "the person's key, once, for both of their seats"
    );
    heard_seat_end_once(&mut first, &r, "the first connection").await;
    heard_seat_end_once(&mut second, &r, "the second connection").await;
    assert_unseated(&app, &conn1, "the first connection");
    assert_unseated(&app, &conn2, "the second connection");
    passes_end_nothing(&app, &mut memory, 2, "after the closes").await;
    within_the_loops_window(since);
    for ws in [&mut host, &mut first, &mut second] {
        ws.close(None).await.ok();
    }
}

/// T8. An ended seat settles its call once: a host still in the call hears
/// one `room:call_updated` for it, as for a hang-up. The negative control
/// first: a hang-up is heard once.
#[tokio::test]
async fn an_ended_seat_settles_its_call_once() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatsettle").await;
    let member = &tenant.member;
    let other = third_member(&app, &tenant, "ada").await;
    let r = start_call_in_new_room(&app, &tenant, "R").await;
    let (mut host, _) = seat_in_call(&app, &tenant.tenant_id, &r, &tenant.admin.access_token).await;
    let (mut ws, conn) = seat_in_call(&app, &tenant.tenant_id, &r, &member.access_token).await;
    let (mut other_ws, _) = seat_in_call(&app, &tenant.tenant_id, &r, &other.access_token).await;
    // What the joins told the host is read now.
    frames_within(&mut host, 500).await;

    // The negative control: the third person hangs up on their own.
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room/{r}/call/leave", tenant.tenant_id),
            &other.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let updates = heard_within(&mut host, "room:call_updated", &r, 1500).await;
    assert_eq!(
        updates.len(),
        1,
        "a hang-up settles its call once: {updates:?}"
    );

    // The member's org row goes; their call sessions stay open.
    delete_org_row(&app, &tenant.tenant_id, &member.id).await;
    let since = Instant::now();
    let mut memory = SeatCheckMemory::default();
    passes_end_nothing(&app, &mut memory, 1, "the first reading").await;
    let ended = check_seats_once(state, &mut memory).await;
    assert_eq!(
        ended,
        vec![key(&r, &member.id)],
        "the second reading ends it"
    );
    let updates = heard_within(&mut host, "room:call_updated", &r, 1500).await;
    assert_eq!(
        updates.len(),
        1,
        "the ended seat settles its call once, as a hang-up does: {updates:?}"
    );
    heard_seat_end_once(&mut ws, &r, "the member").await;
    assert_unseated(&app, &conn, "the member");
    within_the_loops_window(since);
    for sock in [&mut host, &mut ws, &mut other_ws] {
        sock.close(None).await.ok();
    }
}

/// T9. A seat the room guard admits is not ended, whatever the reads found:
/// handed a verdict for an entitled member's seat, the end half keeps the
/// seat, tells no one, and warns. The positive control: once the member's
/// org row is gone, the same verdict ends the seat.
#[tokio::test]
async fn a_seat_the_room_guard_admits_is_not_ended_whatever_the_reads_found() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatguardsays").await;
    let member = &tenant.member;
    let r = start_call_in_new_room(&app, &tenant, "R").await;
    let (mut host, _) = seat_in_call(&app, &tenant.tenant_id, &r, &tenant.admin.access_token).await;
    let (mut ws, conn) = seat_in_call(&app, &tenant.tenant_id, &r, &member.access_token).await;
    let seat = key(&r, &member.id);

    let mut memory = SeatCheckMemory::default();
    let mut read = read_seats(state, &mut memory).await;
    assert!(
        read.failed.is_none() && read.verdicts.is_empty(),
        "an entitled member reads as admitted: {read:?}"
    );
    read.verdicts.push(Verdict {
        key: seat,
        first: Gone::NotInOrg,
        last: Gone::NotInOrg,
    });
    let log = Captured::default();
    let ended = end_seats(state, &mut memory, &read)
        .with_subscriber(log.dispatch())
        .await;
    assert!(
        ended.is_empty(),
        "a verdict ended a seat the room guard admits: {ended:?}"
    );
    assert_seated(&app, &conn, &r, "the entitled member");
    heard_no_call_end(&mut ws, &r, "the entitled member").await;
    let text = log.text();
    assert!(
        text.contains(GUARD_ADMITS) && text.contains(GUARD_ADMITS_ALERT),
        "the guard's word is in the log: {text}"
    );

    // The positive control: the org row goes, the guard refuses, and the
    // same verdict ends the seat.
    delete_org_row(&app, &tenant.tenant_id, &member.id).await;
    let ended = end_seats(state, &mut memory, &read).await;
    assert_eq!(
        ended,
        vec![seat],
        "the verdict the guard agrees with ends it"
    );
    heard_seat_end_once(&mut ws, &r, "the member").await;
    assert_unseated(&app, &conn, "the member");
    host.close(None).await.ok();
    ws.close(None).await.ok();
}

/// T10. A seat whose room guard cannot answer is kept: with a real verdict
/// (the member's org row gone, two readings), the end half, run where
/// whatever the guard reads cannot be reached, ends nothing.
#[tokio::test]
async fn a_seat_whose_room_guard_cannot_answer_is_kept() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatguardmute").await;
    let member = &tenant.member;
    let r = start_call_in_new_room(&app, &tenant, "R").await;
    let (mut host, _) = seat_in_call(&app, &tenant.tenant_id, &r, &tenant.admin.access_token).await;
    let (mut ws, conn) = seat_in_call(&app, &tenant.tenant_id, &r, &member.access_token).await;
    let seat = key(&r, &member.id);
    let unreachable = guarded_from(state, &refusing_db().await);

    delete_org_row(&app, &tenant.tenant_id, &member.id).await;
    let since = Instant::now();
    let mut memory = SeatCheckMemory::default();
    let first = read_seats(state, &mut memory).await;
    assert!(keys(&first).is_empty(), "one reading gives no verdict");
    let read = read_seats(state, &mut memory).await;
    assert_eq!(keys(&read), vec![seat], "two readings give the verdict");

    let ended = end_seats(&unreachable, &mut memory, &read).await;
    assert!(
        ended.is_empty(),
        "a seat was ended though its room guard could not answer: {ended:?}"
    );
    assert_seated(&app, &conn, &r, "the member whose guard cannot answer");
    heard_no_call_end(&mut ws, &r, "the member").await;
    within_the_loops_window(since);
    host.close(None).await.ok();
    ws.close(None).await.ok();
}

/// T11. A pass ends only the seat it read. The person loses the org (two
/// readings give the verdict), is added back, joins again on the same
/// connection (a new seat, with a new send transport, replaces the one the
/// pass read) and loses the org again before the end half runs: the guard
/// refuses them, so only the seat's identity keeps the new seat. Two more
/// passes end it.
#[tokio::test]
async fn a_pass_ends_only_the_seat_it_read() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatidentity").await;
    let member = &tenant.member;
    let r = start_call_in_new_room(&app, &tenant, "R").await;
    let (mut host, _) = seat_in_call(&app, &tenant.tenant_id, &r, &tenant.admin.access_token).await;
    join_call(&app, &tenant.tenant_id, &r, &member.access_token).await;
    let (mut ws, conn) = connect_user_ws_conn(&app, &member.access_token).await;
    let read_seat = join_media(&mut ws, &r).await;
    assert_seated(&app, &conn, &r, "the seat the pass reads");
    let seat = key(&r, &member.id);
    let row = row_of(
        &app,
        "tenant_members",
        org_filter(&tenant.tenant_id, &member.id),
    )
    .await;

    delete_org_row(&app, &tenant.tenant_id, &member.id).await;
    let since = Instant::now();
    let mut memory = SeatCheckMemory::default();
    read_seats(state, &mut memory).await;
    let read = read_seats(state, &mut memory).await;
    assert_eq!(keys(&read), vec![seat], "two readings give the verdict");

    // Added back, a new seat on the same connection, and the org lost again.
    put_row(&app, "tenant_members", row).await;
    let new_seat = join_media(&mut ws, &r).await;
    assert_ne!(new_seat, read_seat, "the second join is a new seat");
    assert_seated(&app, &conn, &r, "the new seat");
    delete_org_row(&app, &tenant.tenant_id, &member.id).await;

    let ended = end_seats(state, &mut memory, &read).await;
    assert!(
        ended.is_empty(),
        "the pass ended a seat taken after its read: {ended:?}"
    );
    assert_seated(&app, &conn, &r, "the new seat, after the end half");
    heard_no_call_end(&mut ws, &r, "the new seat").await;

    passes_end_nothing(&app, &mut memory, 1, "the new seat's first reading").await;
    let ended = check_seats_once(state, &mut memory).await;
    assert_eq!(ended, vec![seat], "two more passes end the new seat");
    heard_seat_end_once(&mut ws, &r, "the new seat").await;
    assert_unseated(&app, &conn, "the new seat");
    within_the_loops_window(since);
    host.close(None).await.ok();
    ws.close(None).await.ok();
}

/// T12. A join on a new connection after the read is not ended by that pass:
/// the end half closes the connection it read, and the new one holds.
#[tokio::test]
async fn a_join_on_a_new_connection_after_the_read_is_not_ended_by_that_pass() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatnewconn").await;
    let member = &tenant.member;
    let r = start_call_in_new_room(&app, &tenant, "R").await;
    let (mut host, _) = seat_in_call(&app, &tenant.tenant_id, &r, &tenant.admin.access_token).await;
    let (mut ws, conn) = seat_in_call(&app, &tenant.tenant_id, &r, &member.access_token).await;
    let seat = key(&r, &member.id);
    let row = row_of(
        &app,
        "tenant_members",
        org_filter(&tenant.tenant_id, &member.id),
    )
    .await;

    delete_org_row(&app, &tenant.tenant_id, &member.id).await;
    let since = Instant::now();
    let mut memory = SeatCheckMemory::default();
    read_seats(state, &mut memory).await;
    let read = read_seats(state, &mut memory).await;
    assert_eq!(keys(&read), vec![seat], "two readings give the verdict");

    // Added back, a seat on a new connection, and the org lost again.
    put_row(&app, "tenant_members", row).await;
    let (mut later, later_conn) = seat_again(&app, &r, &member.access_token).await;
    delete_org_row(&app, &tenant.tenant_id, &member.id).await;

    let ended = end_seats(state, &mut memory, &read).await;
    assert_eq!(ended, vec![seat], "the end half ends the seat it read");
    heard_seat_end_once(&mut ws, &r, "the connection the pass read").await;
    assert_unseated(&app, &conn, "the connection the pass read");
    assert_seated(
        &app,
        &later_conn,
        &r,
        "the connection that joined after the read",
    );
    heard_no_call_end(&mut later, &r, "the connection that joined after the read").await;
    within_the_loops_window(since);
    for sock in [&mut host, &mut ws, &mut later] {
        sock.close(None).await.ok();
    }
}

/// T13. A withdrawal that reached the seats during a pass leaves it nothing
/// to end. The verdict comes from a membership that ended outside the hooks;
/// then the same person's org membership ends through the route on this pod,
/// whose hook withdraws both of their seats at once, and the end half ends
/// nothing. Each of their sockets heard its seat end once, and the host heard
/// each of them leave once.
#[tokio::test]
async fn a_reached_withdrawal_during_a_pass_leaves_it_nothing_to_end() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatreached").await;
    let member = &tenant.member;
    let r = start_call_in_new_room(&app, &tenant, "R").await;
    let (mut host, _) = seat_in_call(&app, &tenant.tenant_id, &r, &tenant.admin.access_token).await;
    let (mut first, conn1) = seat_in_call(&app, &tenant.tenant_id, &r, &member.access_token).await;
    let (mut second, conn2) = seat_again(&app, &r, &member.access_token).await;
    // What the joins told the host is read now.
    frames_within(&mut host, 500).await;
    let seat = key(&r, &member.id);
    let row = row_of(
        &app,
        "tenant_members",
        org_filter(&tenant.tenant_id, &member.id),
    )
    .await;

    // A verdict from a membership that ended outside the hooks.
    delete_org_row(&app, &tenant.tenant_id, &member.id).await;
    let since = Instant::now();
    let mut memory = SeatCheckMemory::default();
    read_seats(state, &mut memory).await;
    let read = read_seats(state, &mut memory).await;
    assert_eq!(keys(&read), vec![seat], "two readings give the verdict");

    // The membership ends through the route on this pod: its hook withdraws
    // the person's seats at once.
    put_row(&app, "tenant_members", row).await;
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/member/{}", tenant.tenant_id, member.id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    for (ws, what) in [
        (&mut first, "the first socket"),
        (&mut second, "the second socket"),
    ] {
        let heard = frame_of_within(ws, "room:call_ended", Duration::from_secs(5))
            .await
            .unwrap_or_else(|| panic!("{what}: the hook ended no seat"));
        assert_eq!(heard["data"]["room_id"], r.as_str(), "{what}: {heard}");
        assert_eq!(heard["data"]["reason"], "not_a_member", "{what}: {heard}");
    }
    assert_unseated(&app, &conn1, "the first socket");
    assert_unseated(&app, &conn2, "the second socket");

    let ended = end_seats(state, &mut memory, &read).await;
    assert!(
        ended.is_empty(),
        "the end half found the seats withdrawn and ended nothing: {ended:?}"
    );
    for (ws, what) in [
        (&mut first, "the first socket"),
        (&mut second, "the second socket"),
    ] {
        let again: Vec<serde_json::Value> = heard_within(ws, "room:call_ended", &r, 700)
            .await
            .into_iter()
            .filter(|f| f["data"]["reason"] == "not_a_member")
            .collect();
        assert!(again.is_empty(), "{what}: its seat ended again: {again:?}");
    }
    let left = heard_within(&mut host, "media:peer_left", &r, 700).await;
    let mut conns: Vec<&str> = left
        .iter()
        .map(|f| f["data"]["connection_id"].as_str().unwrap_or(""))
        .collect();
    conns.sort_unstable();
    let mut want = [conn1.as_str(), conn2.as_str()];
    want.sort_unstable();
    assert_eq!(
        conns, want,
        "the host heard each of the person's seats leave once: {left:?}"
    );
    within_the_loops_window(since);
    for ws in [&mut host, &mut first, &mut second] {
        ws.close(None).await.ok();
    }
}

/// T1b (with the fix). The warning for a document that does not decode
/// fires, and once while the document stays odd: three passes over a room
/// whose visibility the model does not know warn once, naming the
/// collection and the room, and end nothing.
#[tokio::test]
async fn a_document_that_stays_odd_warns_once() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatoddwarn").await;
    let r = start_call_in_new_room(&app, &tenant, "R").await;
    let (mut ws, conn) =
        seat_in_call(&app, &tenant.tenant_id, &r, &tenant.member.access_token).await;
    set_visibility(&app, &r, "unheard-of").await;

    let log = Captured::default();
    let mut memory = SeatCheckMemory::default();
    for pass in 1..=3 {
        let read = read_seats(state, &mut memory)
            .with_subscriber(log.dispatch())
            .await;
        assert!(
            read.failed.is_none() && read.verdicts.is_empty(),
            "pass {pass}: {read:?}"
        );
    }
    let text = log.text();
    let warned: Vec<&str> = text
        .lines()
        .filter(|line| line.contains(ODD_DOCUMENT))
        .collect();
    eprintln!("T1b the warning: {warned:?}");
    assert_eq!(warned.len(), 1, "one warning while it stays odd: {text}");
    assert!(
        warned[0].contains("seat_check_odd_document")
            && warned[0].contains("rooms")
            && warned[0].contains(&r),
        "the warning names its alert, the collection and the room: {}",
        warned[0]
    );
    assert_seated(&app, &conn, &r, "the seat in the room that does not decode");
    ws.close(None).await.ok();
}

/// T4b (with the fix). A room row of no org that names the seat is not read,
/// so it hides the seat from no reading. The person's own row in a
/// members-only room goes and a row with the same room and person and no org
/// is written raw: the room guard, which reads the rows of the room's org,
/// refuses the person, and the seat ends on its second reading. Were the row
/// read, it would not decode, and the seat would get no verdict.
#[tokio::test]
async fn a_room_row_of_no_org_does_not_hide_the_seat_it_names() {
    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatnoorgrow").await;
    let tid = oid_of(&tenant.tenant_id);
    let x = &tenant.member;
    let r = start_call_in_new_room(&app, &tenant, "R").await;
    let (mut host, _) = seat_in_call(&app, &tenant.tenant_id, &r, &tenant.admin.access_token).await;
    let (mut in_x, conn_x) = seat_in_call(&app, &tenant.tenant_id, &r, &x.access_token).await;
    set_visibility(&app, &r, "private").await;

    delete_room_row(&app, &r, &x.id).await;
    put_row(
        &app,
        "room_members",
        doc! {
            "room_id": oid_of(&r),
            "user_id": oid_of(&x.id),
            "joined_at": bson::DateTime::now(),
        },
    )
    .await;
    let since = Instant::now();
    let guard = require_room_in_tenant(state, tid, oid_of(&r), oid_of(&x.id)).await;
    eprintln!("T4b the room guard on X: {}", answer(&guard));
    assert!(
        matches!(
            guard,
            Err(ApiError::NotAMember | ApiError::Forbidden(_) | ApiError::NotFound(_))
        ),
        "the room guard refuses X: {}",
        answer(&guard)
    );
    let seat = key(&r, &x.id);
    let read = read_readings(state, &[seat]).await;
    eprintln!("T4b the read: {read:?}");
    assert_eq!(
        read.as_ref().ok().and_then(|readings| readings.get(&seat)),
        Some(&Reading::Gone(Gone::NotInRoom)),
        "the row of no org is not read: {read:?}"
    );

    let mut memory = SeatCheckMemory::default();
    passes_end_nothing(&app, &mut memory, 1, "X's first reading").await;
    let ended = check_seats_once(state, &mut memory).await;
    assert_eq!(ended, vec![seat], "X's second reading ends the seat");
    heard_seat_end_once(&mut in_x, &r, "X").await;
    assert_unseated(&app, &conn_x, "X");
    within_the_loops_window(since);
    host.close(None).await.ok();
    in_x.close(None).await.ok();
}

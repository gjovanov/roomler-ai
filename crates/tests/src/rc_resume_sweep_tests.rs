// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-138 P1b-2, part two — the rest of the server's resume, through real
//! servers and one MongoDB:
//!
//! * **the sweep** (cluster-singleton, on the presence sweep's cadence) ends
//!   a detached row past its `resume_by`, and a row a pod that died without
//!   a word left open on a device the scope covers that has been offline for
//!   longer than the window. Each ends at its own last clock with
//!   `end_source: sweep`. It leaves alone a row still inside its window, a
//!   resumed one, one on a device outside the scope and, at window 0, any row
//!   no detach marked;
//! * **the re-announce**: a change to the resume scope reaches a connected
//!   device on the other pod at once, and taking it out withdraws its window;
//! * **the dead-row close** (`POST /api/admin/rc-sessions/close-dead`), a
//!   platform admin's: its dry run lists without writing, and a real run
//!   closes at the row's own last clock with `end_source: backfill`.

use std::time::{Duration, Instant};

use bson::{DateTime, Document, doc, oid::ObjectId};
use roomler_ai_services::dao::rc_resume_scope::ResumeScopeKind;
use serde_json::{Value, json};

use crate::fixtures::test_app::TestApp;
use crate::rc_resume_claim_tests::{
    QUIET, RESUME, WINDOW, WITHIN, ended, enroll, hello, insert, leftover, next_of, now_ms, oid,
    open, row, scope, scope_all, tenant_of,
};

/// The presence sweep's cadence, the shortest it takes: the resume sweep runs
/// on it.
const SWEEP_SECS: u64 = 5;
/// How long a sweep may take to land: two cycles and the batched writes.
const SWEPT_WITHIN: Duration = Duration::from_secs(2 * SWEEP_SECS + 4);

/// One pod with the window `window`, sweeping every [`SWEEP_SECS`].
async fn sweeping(window: u64) -> TestApp {
    TestApp::spawn_with_settings(move |s| {
        s.rc.session_resume_secs = window;
        s.rc.presence_sweep_secs = SWEEP_SECS;
    })
    .await
}

/// A device the presence sweep found offline `hours` ago.
async fn offline_since(app: &TestApp, agent: ObjectId, hours: i64) {
    app.db
        .collection::<Document>("agents")
        .update_one(
            doc! { "_id": agent },
            doc! { "$set": {
                "status": "offline",
                "last_presence": "offline",
                "last_seen_at": DateTime::from_millis(now_ms() - hours * 3_600_000),
            }},
        )
        .await
        .unwrap();
}

/// The row once `pred` holds, within `within`; panics with `what` otherwise.
async fn wait_for(
    app: &TestApp,
    sid: ObjectId,
    within: Duration,
    what: &str,
    pred: impl Fn(&Document) -> bool,
) -> Document {
    let deadline = Instant::now() + within;
    loop {
        let r = row(app, sid).await;
        if pred(&r) {
            return r;
        }
        assert!(Instant::now() < deadline, "{what}: {r}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn swept(r: &Document) -> bool {
    ended(r) && r.get_str("end_source") == Ok("sweep")
}

/// AC4, AC9 — the sweep, window on. The device the scope covers has been
/// offline for two hours, the other device is outside the scope. Over two
/// cycles the sweep ends the detached row past its `resume_by` at its
/// `detached_at`, and the row a dead pod left open on the covered device at
/// its `stats_at`, each `agent_disconnect` with `end_source: sweep`. It leaves
/// the detached row still inside its window, the resumed row whose stats are
/// fresh, and the dead pod's row on the device outside the scope.
#[tokio::test]
async fn the_sweep_ends_what_nobody_will_claim_and_leaves_the_rest() {
    let app = sweeping(WINDOW).await;
    let seeded = app.seed_tenant("fr138sweep").await;
    let (covered, _) = enroll(&app, &seeded, "fr138-sweep-in").await;
    let (outside, _) = enroll(&app, &seeded, "fr138-sweep-out").await;
    scope(&app, &[(ResumeScopeKind::Device, Some(covered))]).await;
    offline_since(&app, covered, 2).await;
    offline_since(&app, outside, 2).await;
    let (tenant, ctl) = (tenant_of(&seeded), oid(&seeded.admin.id));
    let base = now_ms() - 3 * 3_600_000;
    let at = DateTime::from_millis;
    let ids: Vec<ObjectId> = (0..5).map(|_| ObjectId::new()).collect();
    let (expired, inside, abandoned, foreign, resumed) = (ids[0], ids[1], ids[2], ids[3], ids[4]);

    let detached_at = now_ms() - 120_000;
    insert(
        &app,
        leftover(
            expired,
            covered,
            tenant,
            ctl,
            base,
            doc! { "detached_at": at(detached_at), "resume_by": at(detached_at + 60_000) },
        ),
    )
    .await;
    insert(
        &app,
        leftover(
            inside,
            covered,
            tenant,
            ctl,
            base,
            doc! { "detached_at": at(now_ms()), "resume_by": at(now_ms() + 600_000) },
        ),
    )
    .await;
    let stats_at = now_ms() - 3_600_000;
    insert(
        &app,
        leftover(
            abandoned,
            covered,
            tenant,
            ctl,
            base,
            doc! { "stats_at": at(stats_at) },
        ),
    )
    .await;
    insert(
        &app,
        leftover(
            foreign,
            outside,
            tenant,
            ctl,
            base,
            doc! { "stats_at": at(stats_at) },
        ),
    )
    .await;
    insert(
        &app,
        leftover(
            resumed,
            covered,
            tenant,
            ctl,
            base,
            doc! { "attach_seq": 1_i64, "stats_at": at(now_ms() + 600_000) },
        ),
    )
    .await;
    let untouched = [
        (inside, row(&app, inside).await),
        (foreign, row(&app, foreign).await),
        (resumed, row(&app, resumed).await),
    ];

    for (sid, last_clock, what) in [
        (expired, at(detached_at), "the detached row past resume_by"),
        (
            abandoned,
            at(stats_at),
            "the dead pod's row on the covered device",
        ),
    ] {
        let r = wait_for(&app, sid, SWEPT_WITHIN, what, swept).await;
        assert_eq!(
            r.get_str("end_reason"),
            Ok("agent_disconnect"),
            "{what}: {r}"
        );
        assert_eq!(r.get_str("phase"), Ok("closed"), "{what}: {r}");
        assert_eq!(
            r.get_datetime("ended_at").unwrap(),
            &last_clock,
            "{what}: dated by its own last clock: {r}"
        );
    }
    for (sid, before) in untouched {
        assert_eq!(row(&app, sid).await, before, "left alone: {sid}");
    }
}

/// Off is today: at window 0 the sweep ends nothing no detach marked. A row a
/// dead pod left open stays as it is whatever its device and the scope say,
/// while a row a detach marked before the window was turned back to 0 still
/// ends at its `resume_by`.
#[tokio::test]
async fn at_window_zero_the_sweep_ends_only_what_a_detach_marked() {
    let app = sweeping(0).await;
    let seeded = app.seed_tenant("fr138sweepzero").await;
    scope_all(&app).await;
    let (device, _) = enroll(&app, &seeded, "fr138-sweep-zero").await;
    offline_since(&app, device, 2).await;
    let (tenant, ctl) = (tenant_of(&seeded), oid(&seeded.admin.id));
    let base = now_ms() - 3 * 3_600_000;
    let at = DateTime::from_millis;
    let (marked, dead) = (ObjectId::new(), ObjectId::new());
    let detached_at = now_ms() - 120_000;
    insert(
        &app,
        leftover(
            marked,
            device,
            tenant,
            ctl,
            base,
            doc! { "detached_at": at(detached_at), "resume_by": at(detached_at + 60_000) },
        ),
    )
    .await;
    insert(
        &app,
        leftover(
            dead,
            device,
            tenant,
            ctl,
            base,
            doc! { "stats_at": at(now_ms() - 3_600_000) },
        ),
    )
    .await;
    let before = row(&app, dead).await;

    let r = wait_for(&app, marked, SWEPT_WITHIN, "the marked row", swept).await;
    assert_eq!(r.get_datetime("ended_at").unwrap(), &at(detached_at), "{r}");
    assert_eq!(
        row(&app, dead).await,
        before,
        "a row no detach marked stays"
    );
}

/// Platform operators only.
async fn pair_with_operator(window: u64, operator: ObjectId) -> (TestApp, TestApp) {
    TestApp::spawn_pair(move |s| {
        s.rc.session_resume_secs = window;
        s.stats.platform_admins = Some(operator.to_hex());
    })
    .await
}

fn operator_token(app: &TestApp, operator: ObjectId) -> String {
    app.state
        .auth
        .generate_tokens(operator, "operator@fr138.test", "fr138_operator")
        .unwrap()
        .access_token
}

/// A client with no cookie jar, so the bearer token is the only identity:
/// `app.client` keeps the cookie the last sign-up set.
fn bare_client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(false)
        .build()
        .unwrap()
}

async fn put_scope(app: &TestApp, token: &str, entries: Value) {
    let resp = bare_client()
        .put(app.url("/api/admin/rc-resume-scope"))
        .bearer_auth(token)
        .json(&json!({ "entries": entries }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the scope saves");
}

/// The re-announce (spec §4.1): a device on pod 2 that advertises the verb
/// and is outside the scope holds no window. A platform admin puts it in the
/// scope on pod 1, and the device is told its window at once, across pods.
/// Taken out again, it is told 0, which withdraws it. A device on pod 1 hears
/// the same through pod 1's own apply.
#[tokio::test]
async fn a_scope_change_reaches_a_device_on_the_other_pod() {
    let operator = ObjectId::new();
    let (app1, app2) = pair_with_operator(WINDOW, operator).await;
    if app1.state.cluster_bus.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let seeded = app1.seed_tenant("fr138announce").await;
    let token = operator_token(&app1, operator);
    put_scope(&app1, &token, json!([])).await;
    let (far, far_token) = enroll(&app1, &seeded, "fr138-announce-far").await;
    let (near, near_token) = enroll(&app1, &seeded, "fr138-announce-near").await;
    let mut far_ws = hello(&app2, &far_token, &[RESUME], &[]).await;
    let mut near_ws = hello(&app1, &near_token, &[RESUME], &[]).await;
    for ws in [&mut far_ws, &mut near_ws] {
        assert!(
            next_of(ws, "rc:agent.resume_window", QUIET).await.is_none(),
            "no window outside the scope"
        );
    }

    put_scope(
        &app1,
        &token,
        json!([
            { "kind": "device", "id": far.to_hex() },
            { "kind": "device", "id": near.to_hex() },
        ]),
    )
    .await;
    for (ws, who) in [
        (&mut far_ws, "pod 2's device"),
        (&mut near_ws, "pod 1's device"),
    ] {
        let told = next_of(ws, "rc:agent.resume_window", WITHIN)
            .await
            .unwrap_or_else(|| panic!("{who} is told its window"));
        assert_eq!(told["secs"], WINDOW, "{who}: {told}");
    }
    assert_eq!(
        app2.state.fleet().rc_hub.agent_resume_window(far),
        Some(WINDOW as u32)
    );

    put_scope(&app1, &token, json!([])).await;
    for (ws, who) in [
        (&mut far_ws, "pod 2's device"),
        (&mut near_ws, "pod 1's device"),
    ] {
        let told = next_of(ws, "rc:agent.resume_window", WITHIN)
            .await
            .unwrap_or_else(|| panic!("{who} is told its window is withdrawn"));
        assert_eq!(told["secs"], 0, "{who}: {told}");
    }
    assert_eq!(app2.state.fleet().rc_hub.agent_resume_window(far), None);
}

async fn close_dead(app: &TestApp, token: &str, body: Value) -> (u16, Value) {
    let resp = bare_client()
        .post(app.url("/api/admin/rc-sessions/close-dead"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn listed(body: &Value) -> Vec<String> {
    body["rows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["session_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The dead-row close (P0's 11 rows): a platform admin's. Its dry run lists
/// the live row a dead pod left open days ago on a device that has had a
/// session since, and writes nothing. It does not list a row on a device with
/// no later session, nor a row younger than a day. A real run then closes
/// the listed row as `agent_disconnect`, at its own last clock, marked
/// `end_source: backfill`, and a second run finds nothing left.
#[tokio::test]
async fn the_dead_row_close_lists_in_its_dry_run_and_closes_on_a_real_one() {
    let operator = ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(operator.to_hex());
    })
    .await;
    let seeded = app.seed_tenant("fr138deadrows").await;
    let token = operator_token(&app, operator);
    let (used, _) = enroll(&app, &seeded, "fr138-dead-used").await;
    let (idle, _) = enroll(&app, &seeded, "fr138-dead-idle").await;
    let (tenant, ctl) = (tenant_of(&seeded), oid(&seeded.admin.id));
    let day = 24 * 3_600_000;
    let at = DateTime::from_millis;
    let (dead, later, lonely, young) = (
        ObjectId::new(),
        ObjectId::new(),
        ObjectId::new(),
        ObjectId::new(),
    );
    let started = now_ms() - 5 * day;
    insert(&app, leftover(dead, used, tenant, ctl, started, doc! {})).await;
    insert(
        &app,
        leftover(
            later,
            used,
            tenant,
            ctl,
            now_ms() - day,
            doc! { "phase": "closed", "ended_at": at(now_ms() - day + 60_000), "end_reason": "controller_hangup" },
        ),
    )
    .await;
    insert(&app, leftover(lonely, idle, tenant, ctl, started, doc! {})).await;
    insert(
        &app,
        leftover(young, used, tenant, ctl, now_ms() - 3_600_000, doc! {}),
    )
    .await;
    let before: Vec<(ObjectId, Document)> = {
        let mut v = Vec::new();
        for sid in [dead, lonely, young] {
            v.push((sid, row(&app, sid).await));
        }
        v
    };

    let stranger = seeded.admin.access_token.clone();
    let (status, _) = close_dead(&app, &stranger, json!({})).await;
    assert_eq!(status, 404, "a platform admin's route");

    let (status, body) = close_dead(&app, &token, json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["dry_run"], true, "a dry run by default: {body}");
    assert_eq!(listed(&body), vec![dead.to_hex()], "{body}");
    assert_eq!(body["closed"], 0, "{body}");
    for (sid, b) in &before {
        assert_eq!(&row(&app, *sid).await, b, "the dry run writes nothing");
    }

    let (status, body) = close_dead(&app, &token, json!({ "dry_run": false })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["closed"], 1, "{body}");
    let r = row(&app, dead).await;
    assert!(ended(&r), "{r}");
    assert_eq!(r.get_str("end_reason"), Ok("agent_disconnect"), "{r}");
    assert_eq!(r.get_str("end_source"), Ok("backfill"), "{r}");
    assert_eq!(
        r.get_datetime("ended_at").unwrap(),
        &at(started),
        "at its own last clock, never the operator's: {r}"
    );
    for sid in [lonely, young] {
        assert!(open(&row(&app, sid).await), "{sid} is not dead");
    }

    let (_, body) = close_dead(&app, &token, json!({ "dry_run": false })).await;
    assert!(listed(&body).is_empty(), "nothing left: {body}");
}

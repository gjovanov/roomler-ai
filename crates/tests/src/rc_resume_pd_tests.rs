// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-138 — the server's resume at the edges the P1b reviews deferred to
//! "PR D", through real servers, real sockets and one MongoDB shared by the
//! pods (`TestApp::spawn_pair`):
//!
//! * a detached session keeps its FR-115 slot against a request on its own
//!   pod, whose stale-lease check reads the row first (M5), and at window 0
//!   that check is what it always was;
//! * a claim that loses its commit never renews or retags the winner's lease
//!   (`refuse_claim`'s race);
//! * a claim whose connection was displaced after its commit writes the
//!   detach back, so the row is not left open with no pod and no deadline
//!   (L2);
//! * a detached session's lease lives until `resume_by` plus 10 s, and no
//!   longer (L5);
//! * an external session is marked on its row while its device holds a
//!   window, and a claim of a marked row is refused (L6, D3);
//! * an admin's end during the gap that cannot be written answers 503, not
//!   200 (N10);
//! * the rejoin of a detached session runs the gate at most once per 5 s for
//!   its viewer (N14).

use std::sync::atomic::Ordering;
use std::time::Duration;

use bson::{DateTime, Document, doc, oid::ObjectId};
use roomler_ai_mod_fleet::hub::{OrgPolicy, ResumeHello};
use roomler_ai_remote_control::models::{ConsentMode, OsKind, RcOverlayMode};
use roomler_ai_remote_control::permissions::Permissions;
use roomler_ai_remote_control::signaling::{ExternalGrant, ServerMsg};
use roomler_core::cluster::session_slots::{SlotClaim, SlotScope};
use tokio::sync::mpsc;

use crate::fixtures::test_app::TestApp;
use crate::rc_resume_claim_tests::{
    RESUME, WINDOW, WITHIN, answer_for, attach_seq, detached, drop_socket, end_reason, ended,
    enroll, go_live, hello, insert, leftover, now_ms, oid, one, open, row, scope_all, tenant_of,
    wait_row,
};
use crate::rc_resume_ends_tests::{as_read, claiming_connection, hub_answer, remote};
use crate::rc_resume_rejoin_tests::{controller, rejoin, rejoin_answer, request};

/// One pod with the window `window` and the org's sessions counted in
/// cluster-wide slots (FR-115 P3b): the harness's orgs run one at a time.
async fn one_slotted(window: u64) -> TestApp {
    TestApp::spawn_with_settings(move |s| {
        s.rc.session_resume_secs = window;
        s.rc.session_slots = true;
    })
    .await
}

/// The row's `resume_by`, in ms.
fn resume_by_ms(r: &Document) -> i64 {
    r.get_datetime("resume_by")
        .expect("a resume_by")
        .timestamp_millis()
}

/// Review 1, M5 — the org runs one session at a time. Its session on device
/// A is detached when A's socket drops, and the same pod is then asked for a
/// session to device B. The pod claimed A's lease and its hub no longer holds
/// the session, so FR-115's stale-lease check called it stale, dropped it and
/// gave the slot to B, and A's claim then found the org full and ended the
/// session. The row says the session may still be resumed: B is refused
/// `org_session_limit`, and A's claim resumes it on the slot it kept.
#[tokio::test]
async fn a_detached_sessions_slot_is_kept_against_a_request_on_its_own_pod() {
    let app = one_slotted(WINDOW).await;
    let Some(slots) = app.state.session_slots.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let seeded = app.seed_tenant("fr138pdm5").await;
    scope_all(&app).await;
    let org = SlotScope::Org(tenant_of(&seeded));
    let (a_id, a_token) = enroll(&app, &seeded, "fr138-pd-m5a").await;
    let (b_id, b_token) = enroll(&app, &seeded, "fr138-pd-m5b").await;
    let admin = seeded.admin.access_token.clone();
    let mut a = hello(&app, &a_token, &[RESUME], &[]).await;
    let _b = hello(&app, &b_token, &[RESUME], &[]).await;
    let (_ctl, sid) = go_live(&app, &admin, a_id, &mut a).await;
    wait_row(&app, sid, "the session goes live", open).await;
    assert_eq!(slots.held(org).await.unwrap(), 1, "the org's one slot");
    drop_socket(a).await;
    wait_row(&app, sid, "the row records the detach", detached).await;

    let mut ctl = controller(&app, &admin).await;
    let refused = request(&mut ctl, b_id, None).await;
    assert_eq!(
        refused["t"], "rc:error",
        "the detached session keeps the org's slot: {refused}"
    );
    assert_eq!(refused["code"], "org_session_limit", "{refused}");

    let mut a2 = hello(&app, &a_token, &[RESUME], &[sid]).await;
    let answer = answer_for(&mut a2, sid, WITHIN)
        .await
        .expect("the claim is answered");
    assert_eq!(
        answer["t"], "rc:session.resumed",
        "resumed on its own slot: {answer}"
    );
    assert_eq!(slots.held(org).await.unwrap(), 1);
}

/// Review 1, M5's guard — at window 0 the stale-lease check is FR-115's,
/// with no read: a lease this pod claimed for a session its hub does not
/// hold is dropped, even when that session's row reads detached (above 0,
/// the M5 test keeps exactly that lease).
#[tokio::test]
async fn at_window_zero_the_stale_lease_check_reads_no_row() {
    let app = one_slotted(0).await;
    let Some(slots) = app.state.session_slots.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let seeded = app.seed_tenant("fr138pdm5z").await;
    let tenant = tenant_of(&seeded);
    let org = SlotScope::Org(tenant);
    let (agent_id, token) = enroll(&app, &seeded, "fr138-pd-m5z").await;
    let _agent = hello(&app, &token, &[RESUME], &[]).await;
    let admin_id = oid(&seeded.admin.id);

    let stale = ObjectId::new();
    let claimed = slots
        .claim(org, stale, admin_id, agent_id, 1, &app.state.pod.pod_id)
        .await
        .unwrap();
    assert_eq!(claimed, SlotClaim::Claimed, "the org's one slot");
    let now = now_ms();
    insert(
        &app,
        leftover(
            stale,
            agent_id,
            tenant,
            admin_id,
            now - 60_000,
            doc! {
                "detached_at": DateTime::from_millis(now - 1_000),
                "resume_by": DateTime::from_millis(now + 59_000),
            },
        ),
    )
    .await;

    let mut ctl = controller(&app, &seeded.admin.access_token).await;
    let created = request(&mut ctl, agent_id, None).await;
    assert_eq!(
        created["t"], "rc:session.created",
        "the stale lease is dropped as it always was: {created}"
    );
}

/// `refuse_claim`'s race (PR C, deferred) — the device's newest connection
/// claims the session on pod 1 and resumes it there. An older connection's
/// claim of it, on pod 2, read the row before that commit and loses its own.
/// It must leave the winner's lease as the winner left it: before, it took
/// the slot BEFORE its commit, which renewed the lease and wrote pod 2 into
/// it, so pod 2 could later read the winner's live lease as its own stale one.
#[tokio::test]
async fn a_claim_that_loses_its_commit_never_retags_the_winners_lease() {
    let (app1, app2) = TestApp::spawn_pair(|s| {
        s.rc.session_resume_secs = WINDOW;
        s.rc.session_slots = true;
    })
    .await;
    let Some(slots) = app1.state.session_slots.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let seeded = app1.seed_tenant("fr138pdrace").await;
    scope_all(&app1).await;
    let (agent_id, token) = enroll(&app1, &seeded, "fr138-pd-race").await;
    let tenant = tenant_of(&seeded);
    let org = SlotScope::Org(tenant);
    let mut agent = hello(&app1, &token, &[RESUME], &[]).await;
    let (_ctl, sid) = go_live(&app1, &seeded.admin.access_token, agent_id, &mut agent).await;
    wait_row(&app1, sid, "the session goes live", open).await;
    drop_socket(agent).await;
    wait_row(&app1, sid, "the row records the detach", detached).await;

    // The older connection's claim read the row here, at 0.
    let read = as_read(&app1, tenant, sid).await;
    let mut agent2 = hello(&app1, &token, &[RESUME], &[sid]).await;
    let won = answer_for(&mut agent2, sid, WITHIN)
        .await
        .expect("the newest claim is answered");
    assert_eq!(won["t"], "rc:session.resumed", "{won}");

    let (ctx, mut rx) =
        claiming_connection(&app2, agent_id, tenant, oid(&seeded.admin.id), vec![sid]);
    roomler_ai_mod_remote::resume::claim_as_read(remote(&app2), &ctx, sid, read).await;
    let lost = hub_answer(&mut rx, sid)
        .await
        .expect("the older claim is answered");
    assert!(matches!(lost, ServerMsg::Terminate { .. }), "{lost:?}");

    match slots
        .claim(
            org,
            ObjectId::new(),
            ObjectId::new(),
            ObjectId::new(),
            1,
            "fr138-pd-probe",
        )
        .await
        .unwrap()
    {
        SlotClaim::Full(holders) => {
            assert_eq!(holders.len(), 1, "{holders:?}");
            assert_eq!(holders[0].session, sid.to_hex(), "{holders:?}");
            assert_eq!(
                holders[0].pod, app1.state.pod.pod_id,
                "the lease names the pod that holds the session: {holders:?}"
            );
        }
        other => panic!("the org's one slot is the winner's: {other:?}"),
    }
}

/// Review 1, L2 — a claim commits, and its connection is displaced before
/// the hub rebuilds the session: the claim is no longer its to answer. The
/// commit cleared the row's deadline, so the row was left open at the new
/// number with no pod holding it and no `resume_by` for the sweep. The claim
/// now writes the detach back, fenced at the number it committed: the row
/// reads detached again, and the newer connection's claim resumes it.
#[tokio::test]
async fn a_claim_whose_connection_is_gone_after_its_commit_writes_the_detach_back() {
    let app = one(WINDOW).await;
    let seeded = app.seed_tenant("fr138pdl2").await;
    scope_all(&app).await;
    let (agent_id, token) = enroll(&app, &seeded, "fr138-pd-l2").await;
    let tenant = tenant_of(&seeded);
    let owner = oid(&seeded.admin.id);
    let mut agent = hello(&app, &token, &[RESUME], &[]).await;
    let (_ctl, sid) = go_live(&app, &seeded.admin.access_token, agent_id, &mut agent).await;
    wait_row(&app, sid, "the session goes live", open).await;
    drop_socket(agent).await;
    wait_row(&app, sid, "the row records the detach", detached).await;

    let (ctx1, _rx1) = claiming_connection(&app, agent_id, tenant, owner, vec![sid]);
    // A newer connection of the device displaces the first one.
    let (ctx2, mut rx2) = claiming_connection(&app, agent_id, tenant, owner, vec![sid]);
    let before = now_ms();
    let read = as_read(&app, tenant, sid).await;
    roomler_ai_mod_remote::resume::claim_as_read(remote(&app), &ctx1, sid, read).await;
    let r = wait_row(&app, sid, "the lost claim writes the detach back", detached).await;
    assert_eq!(attach_seq(&r), 1, "fenced at the number it committed: {r}");
    assert!(resume_by_ms(&r) > before, "a deadline ahead: {r}");
    assert!(app.state.fleet().rc_hub.session_snapshot(sid).is_none());

    let read = as_read(&app, tenant, sid).await;
    roomler_ai_mod_remote::resume::claim_as_read(remote(&app), &ctx2, sid, read).await;
    let answer = hub_answer(&mut rx2, sid)
        .await
        .expect("the newer claim is answered");
    assert!(
        matches!(answer, ServerMsg::SessionResumed { .. }),
        "{answer:?}"
    );
    let r = wait_row(&app, sid, "resumed at the next number", |r| {
        open(r) && attach_seq(r) == 2
    })
    .await;
    assert!(r.get("resume_by").is_none(), "{r}");
}

/// Review 1, L5 — the lease a session holds lives 90 s and is renewed every
/// 30 s by the pod that holds the session, so at a detach it has 60 to 90 s
/// left: less than a 120 s window. The detach now keeps it until `resume_by`
/// plus 10 s, in Redis, where no pod has to renew it (the pod that detached it
/// may be the one a roll takes down), and no longer: a session nobody claims
/// gives its slot back 10 s after its deadline.
#[tokio::test]
async fn a_detached_sessions_lease_lives_until_its_deadline_and_no_longer() {
    let app = one_slotted(120).await;
    let (Some(slots), Some(redis)) = (
        app.state.session_slots.clone(),
        app.state.redis_pubsub.clone(),
    ) else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let seeded = app.seed_tenant("fr138pdl5").await;
    scope_all(&app).await;
    let org = SlotScope::Org(tenant_of(&seeded));
    let (agent_id, token) = enroll(&app, &seeded, "fr138-pd-l5").await;
    let mut agent = hello(&app, &token, &[RESUME], &[]).await;
    let (_ctl, sid) = go_live(&app, &seeded.admin.access_token, agent_id, &mut agent).await;
    wait_row(&app, sid, "the session goes live", open).await;
    drop_socket(agent).await;
    let r = wait_row(&app, sid, "the row records the detach", detached).await;
    let keep_until = resume_by_ms(&r) + 10_000;

    let set = slots.slots_key(org);
    let holder = format!("roomler:rc:slot:{}:{}", app.db.name(), sid.to_hex());
    let mut conn = redis.connection();
    let deadline = tokio::time::Instant::now() + WITHIN;
    let score = loop {
        let score: Option<String> = redis::cmd("ZSCORE")
            .arg(&set)
            .arg(sid.to_hex())
            .query_async(&mut conn)
            .await
            .unwrap();
        let score = score.and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) as i64;
        if score >= keep_until - 1_000 || tokio::time::Instant::now() >= deadline {
            break score;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        (keep_until - 1_000..=keep_until + 1_000).contains(&score),
        "the lease is kept until resume_by + 10 s ({keep_until}), not {score}"
    );
    let left = keep_until - now_ms();
    for key in [&set, &holder] {
        let pttl: i64 = redis::cmd("PTTL")
            .arg(key)
            .query_async(&mut conn)
            .await
            .unwrap();
        assert!(
            (left - 2_000..=left + 1_000).contains(&pttl),
            "{key} expires with the lease ({left} ms), not in {pttl} ms"
        );
    }
}

/// Review 1, L6 (D3) — a device never keeps an external session, so a claim
/// of a row marked external is refused `agent_disconnect` and the row ended,
/// whatever the gate says. Before, the row carried no marker, the claim made
/// no D3 check, and the session came back as an ordinary one that an FR-52
/// revocation would miss.
#[tokio::test]
async fn a_claim_of_an_external_session_is_refused() {
    let app = one(WINDOW).await;
    let seeded = app.seed_tenant("fr138pdl6").await;
    scope_all(&app).await;
    let (agent_id, token) = enroll(&app, &seeded, "fr138-pd-l6").await;
    let sid = ObjectId::new();
    let now = now_ms();
    insert(
        &app,
        leftover(
            sid,
            agent_id,
            tenant_of(&seeded),
            oid(&seeded.admin.id),
            now - 60_000,
            doc! {
                "external": true,
                "detached_at": DateTime::from_millis(now - 1_000),
                "resume_by": DateTime::from_millis(now + 59_000),
            },
        ),
    )
    .await;

    let mut agent = hello(&app, &token, &[RESUME], &[sid]).await;
    let answer = answer_for(&mut agent, sid, WITHIN)
        .await
        .expect("the claim is answered");
    assert_eq!(answer["t"], "rc:terminate", "{answer}");
    assert_eq!(answer["reason"], "agent_disconnect", "{answer}");
    let r = wait_row(&app, sid, "the row ends", ended).await;
    assert_eq!(end_reason(&r), Some("agent_disconnect"), "{r}");
    assert!(app.state.fleet().rc_hub.session_snapshot(sid).is_none());
}

/// Review 1, L6 — what marks the row: an external session (FR-52's door)
/// created while its device's connection holds a window is marked
/// `external: true` on its row; one created while it holds none writes the
/// row it always wrote, with no marker.
#[tokio::test]
async fn an_external_session_is_marked_on_its_row_only_under_a_window() {
    let app = one(WINDOW).await;
    let hub = &app.state.fleet().rc_hub;
    let mut kept = Vec::new();
    for (window, marked) in [(Some(60), true), (None, false)] {
        let (agent_id, tenant_id) = (ObjectId::new(), ObjectId::new());
        let reg = hub.register_agent_resuming(
            agent_id,
            tenant_id,
            ObjectId::new(),
            OsKind::Linux,
            2,
            true,
            false,
            ResumeHello {
                advertised: true,
                window,
                kept: Vec::new(),
            },
        );
        let (tx, rx) = mpsc::channel(32);
        let sid = hub
            .create_session_full(
                agent_id,
                ObjectId::new(),
                "An Outsider".into(),
                tx,
                Permissions::VIEW,
                Vec::new(),
                None,
                None,
                None,
                false,
                ConsentMode::Prompt,
                None,
                Vec::new(),
                RcOverlayMode::Off,
                None,
                None,
                false,
                OrgPolicy::Unavailable,
                None,
                None,
                None,
                Some(ExternalGrant {
                    attempt_id: "fr138-pd-attempt".into(),
                }),
                None,
                None,
                None,
            )
            .expect("the external session is created");
        let r = wait_row(&app, sid, "the row is written", |r| !r.is_empty()).await;
        if marked {
            assert_eq!(
                r.get_bool("external"),
                Ok(true),
                "marked under a window: {r}"
            );
        } else {
            assert!(r.get("external").is_none(), "no marker without one: {r}");
        }
        kept.push((reg, rx));
    }
}

/// Review 2, N10 — an admin ends a detached session, and the write to its row
/// fails (a validator stands in for the database refusing it). The device's
/// claim would then resume a session the admin was told was ended: the route
/// answers 503, never 200.
#[tokio::test]
async fn an_admins_end_that_cannot_be_written_answers_503() {
    let app = one(WINDOW).await;
    let seeded = app.seed_tenant("fr138pdn10").await;
    scope_all(&app).await;
    let (agent_id, token) = enroll(&app, &seeded, "fr138-pd-n10").await;
    let admin = seeded.admin.access_token.clone();
    let mut agent = hello(&app, &token, &[RESUME], &[]).await;
    let (_ctl, sid) = go_live(&app, &admin, agent_id, &mut agent).await;
    wait_row(&app, sid, "the session goes live", open).await;
    drop_socket(agent).await;
    wait_row(&app, sid, "the row records the detach", detached).await;

    app.db
        .run_command(doc! {
            "collMod": "remote_sessions",
            "validator": { "end_reason": { "$ne": "admin_terminated" } },
        })
        .await
        .expect("the fault is in place");
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/session/{sid}/terminate", seeded.tenant_id),
            &admin,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        503,
        "an end that was not written is not taken"
    );
    assert!(open(&row(&app, sid).await), "the row is as it was");
}

/// Review 2, N14 — the viewer of a detached session asks again every second
/// until the device claims it, and each ask ran the whole gate (about five
/// reads). Nothing is bound or relayed while the row is detached, so the
/// gate now runs at most once per 5 s for the viewer: five asks inside that
/// run it once, and every ask is still answered with the session's deadline.
#[tokio::test]
async fn a_detached_sessions_rejoins_run_the_gate_once_per_five_seconds() {
    let app = one(WINDOW).await;
    let seeded = app.seed_tenant("fr138pdn14").await;
    scope_all(&app).await;
    let (agent_id, token) = enroll(&app, &seeded, "fr138-pd-n14").await;
    let admin = seeded.admin.access_token.clone();
    let mut agent = hello(&app, &token, &[RESUME], &[]).await;
    let (_ctl, sid) = go_live(&app, &admin, agent_id, &mut agent).await;
    wait_row(&app, sid, "the session goes live", open).await;
    drop_socket(agent).await;
    let r = wait_row(&app, sid, "the row records the detach", detached).await;

    let runs = &remote(&app).resume_gate_runs;
    let before = runs.load(Ordering::Relaxed);
    let mut ctl = controller(&app, &admin).await;
    for _ in 0..5 {
        rejoin(&mut ctl, sid).await;
        let answer = rejoin_answer(&mut ctl, sid, WITHIN)
            .await
            .expect("the rejoin is answered");
        assert_eq!(answer["t"], "rc:session.detached", "{answer}");
        assert_eq!(
            answer["resume_by"].as_i64(),
            Some(resume_by_ms(&r)),
            "{answer}"
        );
    }
    assert_eq!(
        runs.load(Ordering::Relaxed) - before,
        1,
        "five asks inside 5 s run the gate once"
    );
}

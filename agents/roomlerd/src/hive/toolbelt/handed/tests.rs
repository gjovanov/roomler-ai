// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2c-2 — unit tests of the toolbelt's hand-over dispatch, on
//! both sides, without a real listener or a real worker process: a
//! `Serving`/`Pending` built directly, exactly as [`super::keep_for_test`]
//! and [`super::Serving::claim_for_test`] exist for.

use bson::oid::ObjectId;
use tokio::sync::mpsc;

use super::*;
use crate::hive::toolbelt::{Answer as DriverAnswer, Author, Decision, Ended};

fn author() -> Author {
    Author {
        user_id: ObjectId::new(),
        name: "a driver".into(),
    }
}

// ─── The worker's side: `answered` / `withdrawn` / `withdrawn_all` / `close_handed` ───

#[tokio::test]
async fn a_real_wait_is_answered_through_its_endpoint() {
    let serving = Arc::new(Serving::default());
    let pending = Arc::new(Pending::default());
    serving.claim_for_test(1, Arc::clone(&pending));
    let rx = pending.open("a1").expect("under MAX_PENDING");

    let answer = DriverAnswer {
        decision: Decision::Allow,
        by: author(),
    };
    assert!(matches!(
        answered(&serving, 1, "a1", answer.clone()),
        Answer::Done
    ));
    assert_eq!(rx.await.expect("resolved"), answer);
}

#[tokio::test]
async fn answering_twice_refuses_the_second_time() {
    let serving = Arc::new(Serving::default());
    let pending = Arc::new(Pending::default());
    serving.claim_for_test(2, Arc::clone(&pending));
    let _rx = pending.open("a2").expect("under MAX_PENDING");
    let answer = DriverAnswer {
        decision: Decision::Allow,
        by: author(),
    };
    assert!(matches!(
        answered(&serving, 2, "a2", answer.clone()),
        Answer::Done
    ));
    assert!(matches!(
        answered(&serving, 2, "a2", answer),
        Answer::Refused { .. }
    ));
}

#[tokio::test]
async fn an_unknown_endpoint_is_refused_not_a_panic() {
    let serving = Arc::new(Serving::default());
    let answer = DriverAnswer {
        decision: Decision::Allow,
        by: author(),
    };
    assert!(matches!(
        answered(&serving, 999, "whatever", answer),
        Answer::Refused { .. }
    ));
    assert!(matches!(
        withdrawn(&serving, 999, "whatever"),
        Answer::Refused { .. }
    ));
    assert!(matches!(
        withdrawn_all(&serving, 999),
        Answer::Refused { .. }
    ));
}

#[tokio::test]
async fn withdrawing_closes_the_wait_as_withdrawn_not_answered() {
    let serving = Arc::new(Serving::default());
    let pending = Arc::new(Pending::default());
    serving.claim_for_test(3, Arc::clone(&pending));
    let rx = pending.open("a3").expect("under MAX_PENDING");
    assert!(matches!(withdrawn(&serving, 3, "a3"), Answer::Done));
    // The sender dropped: the wait's own receiver sees its channel close,
    // which `run_approval` reads as `Ended::Withdrawn` (toolbelt.rs).
    assert!(rx.await.is_err());
}

#[tokio::test]
async fn withdraw_all_closes_every_open_wait_on_that_endpoint() {
    let serving = Arc::new(Serving::default());
    let pending = Arc::new(Pending::default());
    serving.claim_for_test(4, Arc::clone(&pending));
    let rx1 = pending.open("a4a").expect("under MAX_PENDING");
    let rx2 = pending.open("a4b").expect("under MAX_PENDING");
    assert!(matches!(withdrawn_all(&serving, 4), Answer::Done));
    assert!(rx1.await.is_err());
    assert!(rx2.await.is_err());
}

#[tokio::test]
async fn close_handed_stops_the_endpoints_own_token_and_nothing_else() {
    let serving = Arc::new(Serving::default());
    let a = serving.claim_for_test(5, Arc::new(Pending::default()));
    let b = serving.claim_for_test(6, Arc::new(Pending::default()));
    assert!(matches!(close_handed(&serving, 5), Answer::Done));
    assert!(a.is_cancelled());
    assert!(
        !b.is_cancelled(),
        "closing one endpoint must not stop another"
    );
    // A second close of the same (already-removed) endpoint is a no-op,
    // not a refusal: a hand-over racing the close must not see an error.
    assert!(matches!(close_handed(&serving, 5), Answer::Done));
}

// ─── The daemon's side: `opened` / `closed` ────────────────────────────────

#[tokio::test]
async fn opened_records_it_and_tells_the_sessions_task() {
    let session = ObjectId::new();
    let fence = 1;
    let (tx, mut rx) = mpsc::channel(4);
    let pending = keep_for_test(session, fence, 7, tx);

    let answer = opened(
        session,
        fence,
        7,
        "o1".into(),
        "approve".into(),
        None,
        serde_json::json!({}),
    )
    .await;
    assert!(matches!(answer, Answer::Done));
    assert_eq!(pending.ids(), vec!["o1".to_string()]);
    match rx.recv().await {
        Some(ApprovalEvent::Opened { id, .. }) => assert_eq!(id, "o1"),
        other => panic!("expected Opened, got {other:?}"),
    }
    forget(session, fence);
}

#[tokio::test]
async fn opened_refuses_the_wrong_endpoint() {
    let session = ObjectId::new();
    let fence = 2;
    let (tx, _rx) = mpsc::channel(4);
    keep_for_test(session, fence, 8, tx);

    let answer = opened(
        session,
        fence,
        // not the endpoint `keep_for_test` kept it as
        9,
        "o2".into(),
        "approve".into(),
        None,
        serde_json::json!({}),
    )
    .await;
    assert!(matches!(answer, Answer::Refused { .. }));
    forget(session, fence);
}

#[tokio::test]
async fn opened_refuses_an_unknown_run() {
    let answer = opened(
        ObjectId::new(),
        1,
        1,
        "o3".into(),
        "approve".into(),
        None,
        serde_json::json!({}),
    )
    .await;
    assert!(matches!(answer, Answer::Refused { .. }));
}

#[tokio::test]
async fn closed_tells_the_sessions_task_however_it_ended() {
    let session = ObjectId::new();
    let fence = 3;
    let (tx, mut rx) = mpsc::channel(4);
    keep_for_test(session, fence, 10, tx);

    let answer = closed(session, fence, 10, "c1".into(), Ended::Expired).await;
    assert!(matches!(answer, Answer::Done));
    match rx.recv().await {
        Some(ApprovalEvent::Closed { id, ended }) => {
            assert_eq!(id, "c1");
            assert_eq!(ended, Ended::Expired);
        }
        other => panic!("expected Closed, got {other:?}"),
    }
    forget(session, fence);
}

#[tokio::test]
async fn closed_refuses_the_wrong_endpoint() {
    let session = ObjectId::new();
    let fence = 4;
    let (tx, _rx) = mpsc::channel(4);
    keep_for_test(session, fence, 11, tx);
    let answer = closed(session, fence, 12, "c2".into(), Ended::Withdrawn).await;
    assert!(matches!(answer, Answer::Refused { .. }));
    forget(session, fence);
}

// ─── `forget` ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn forget_makes_a_later_rehand_find_nothing_to_hand() {
    let session = ObjectId::new();
    let fence = 5;
    let (tx, _rx) = mpsc::channel(4);
    keep_for_test(session, fence, 13, tx);
    assert!(is_kept(session, fence));
    forget(session, fence);
    assert!(!is_kept(session, fence));
}

// ─── `prepare`'s gate ───────────────────────────────────────────────────────

/// FR-90 P8-2c-4 — a fresh launch's own case: the device runs workers, and
/// this run's has not started yet, as it never has when its toolbelt is
/// opened. Kept for it all the same: asked whether a worker exists yet
/// (`worker::has`), this answered no at every launch, and nothing was ever
/// handed. Where the device runs none, the daemon serves it, as before.
#[tokio::test]
async fn prepare_keeps_it_for_a_worker_not_started_yet() {
    let dir = tempfile::tempdir().expect("a temp dir for a test socket");
    let listener = UnixListener::bind(dir.path().join("t.sock")).expect("a test socket binds");
    let (tx, _rx) = mpsc::channel(4);
    let session = ObjectId::new();
    assert!(!crate::hive::worker::has(session, 1), "no worker yet");
    let prepared = prepare(session, 1, 0, &listener, tx.clone(), true).expect("kept");
    assert!(prepared.is_some(), "handed, not served by the daemon");
    assert!(is_kept(session, 1), "kept for the worker to come");
    forget(session, 1);

    let off = prepare(session, 2, 0, &listener, tx, false).expect("no failure");
    assert!(off.is_none(), "`hive_worker` off: the daemon serves it");
    assert!(!is_kept(session, 2));
}

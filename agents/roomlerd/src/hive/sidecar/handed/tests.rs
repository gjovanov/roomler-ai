// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The sidecar's worker hand-over: its own dispatch, against a `Kept`/
//! `Serving` built directly — no real listener or worker process, mirroring
//! `toolbelt::handed::tests`'s own shape.

use super::*;

#[tokio::test]
async fn an_endpoint_nobody_kept_is_refused() {
    let answer = admit(ObjectId::new(), 1, 1, Some("tok".into())).await;
    assert!(matches!(answer, Answer::Refused { .. }));
}

#[tokio::test]
async fn forgetting_a_key_nobody_kept_is_a_quiet_done() {
    let answer = key_forgotten(ObjectId::new(), 1, 1).await;
    assert!(matches!(answer, Answer::Done));
}

#[test]
fn prepare_answers_none_where_the_run_has_no_worker() {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let listener = rt
        .block_on(TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)))
        .expect("a listener binds");
    let session = ObjectId::new();
    let prepared = prepare(
        session,
        1,
        &listener,
        Weak::new(),
        "https://x".into(),
        None,
        false,
    )
    .expect("prepare does not fail just because there is no worker");
    assert!(prepared.is_none());
    assert!(!is_kept(session, 1));
}

/// FR-90 P8-2c-4 — a fresh launch's own case: the device runs workers, and
/// this run's has not started yet, as it never has when its sidecar is
/// opened. Kept for it all the same: asked whether a worker exists yet
/// (`worker::has`), this answered no at every launch, and nothing was
/// ever handed.
#[tokio::test]
async fn prepare_keeps_it_for_a_worker_not_started_yet() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("a listener binds");
    let session = ObjectId::new();
    assert!(!crate::hive::worker::has(session, 1), "no worker yet");
    let prepared = prepare(
        session,
        1,
        &listener,
        Weak::new(),
        "https://x".into(),
        None,
        true,
    )
    .expect("kept");
    assert!(prepared.is_some(), "handed, not served by the daemon");
    assert!(is_kept(session, 1), "kept for the worker to come");
    forget(session, 1);
}

#[tokio::test]
async fn a_kept_sidecar_whose_supervisor_is_gone_is_refused_not_panicked() {
    let session = ObjectId::new();
    // `Weak::new()` never upgrades: stands in for a daemon that has
    // already dropped its `Supervisor` by the time a worker's call
    // arrives (shutdown racing a worker's last request).
    keep_for_test(session, 1, 7, Weak::new());
    assert!(is_kept(session, 1));
    let answer = admit(session, 1, 7, Some("tok".into())).await;
    assert!(matches!(answer, Answer::Refused { .. }));
    forget(session, 1);
    assert!(!is_kept(session, 1));
}

#[tokio::test]
async fn a_hand_over_answered_twice_serves_it_once() {
    let serving: Arc<Serving> = Arc::default();
    let endpoint = 42;
    {
        let mut endpoints = serving.endpoints.lock().unwrap();
        endpoints.insert(endpoint, serving.all.child_token());
    }
    // `close_handed` on the same endpoint cancels the real stop this
    // "first answer" claimed — proving `serve_handed`'s own
    // double-claim guard is reachable, without needing a real listener
    // to exercise the rest of it (P8-2c-4: covered end to end by
    // `sidecar::tests`' fresh launch on Unix and by `worker::win::tests`'
    // kept-before-its-worker pair on Windows, which before it nothing
    // covered).
    let answer = close_handed(&serving, endpoint);
    assert!(matches!(answer, Answer::Done));
    assert!(!serving.endpoints.lock().unwrap().contains_key(&endpoint));
}

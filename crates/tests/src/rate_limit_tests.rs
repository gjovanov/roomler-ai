// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Tests for the global per-IP `tower_governor` limiter.
//!
//! ⚠️ Both tests here used to hit `/health`, which the governor **deliberately
//! does not cover**: `build_router` layers it onto `Router::new().nest("/api",
//! api)` alone, under the comment *"not health/ws which need unrestricted
//! access"*. So one test asserted rate limiting on the single endpoint
//! guaranteed never to be rate limited — it failed 3/3 in CI and was written
//! off as "timing-sensitive" and skipped — while the other passed VACUOUSLY,
//! asserting that an unlimited endpoint kept answering.
//!
//! Point them at a route the limiter actually covers and both mean something.

use crate::fixtures::test_app::TestApp;

/// A route that IS behind the governor, and is cheap to hit.
///
/// `/api/auth/refresh` is under `/api` and deliberately outside the stricter
/// per-(address, account) credential gate, so it exercises the general limiter
/// on its own. With no token it answers 401 without touching the database —
/// and the governor runs BEFORE the handler, so any non-429 status is proof
/// the request got past the limiter, whatever the handler then decides.
const LIMITED_ROUTE: &str = "/api/auth/refresh";

/// Fire `n` requests CONCURRENTLY and collect their status codes.
///
/// ⚠️ Concurrent, not sequential, and that part is load-bearing too: the
/// limiter refills one token per second, so a sequential loop of 65
/// round-trips races its own refill. On a slow runner such a loop outlasts the
/// burst it is trying to exhaust, nothing is ever limited, and the failure
/// reports the runner's speed rather than the limiter's behaviour. A burst
/// test has to actually burst.
async fn burst(app: &TestApp, n: usize) -> Vec<u16> {
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let client = app.client.clone();
        let url = app.url(LIMITED_ROUTE);
        handles.push(tokio::spawn(async move {
            client
                .post(url)
                .json(&serde_json::json!({}))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }));
    }
    let mut out = Vec::with_capacity(n);
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}

#[tokio::test]
async fn rate_limit_returns_429_after_burst() {
    let app = TestApp::spawn().await;

    // Burst is 60, refill 1/s. 65 at once must overrun it.
    let statuses = burst(&app, 65).await;
    let limited = statuses.iter().filter(|&&s| s == 429).count();
    let passed = statuses.len() - limited;

    assert!(
        passed >= 60,
        "the burst is 60, so at least that many should reach the handler; only {passed} did"
    );
    assert!(
        limited >= 1,
        "65 concurrent requests against a burst of 60 must produce at least one 429; got none \
         (statuses: {statuses:?})"
    );
}

#[tokio::test]
async fn rate_limit_recovers_after_wait() {
    let app = TestApp::spawn().await;

    let statuses = burst(&app, 65).await;
    // ⚠️ Precondition, and the reason this test is no longer vacuous: if the
    // burst was never exhausted, "it works again afterwards" asserts nothing.
    assert!(
        statuses.contains(&429),
        "precondition: the burst must actually be exhausted before recovery means anything"
    );

    // One token per second; two is comfortable slack.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let after = app
        .client
        .post(app.url(LIMITED_ROUTE))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_ne!(
        after, 429,
        "after refill a request must pass the limiter again"
    );
}

use crate::fixtures::test_app::TestApp;

#[tokio::test]
async fn rate_limit_returns_429_after_burst() {
    let app = TestApp::spawn().await;

    // The limiter allows a burst of 60, then refills 1 token per second.
    //
    // ⚠️ These requests MUST be concurrent, and that is the whole fix. The
    // original loop issued 65 round-trips SEQUENTIALLY, so it raced its own
    // refill: on a slow runner the loop outlasts the burst it is trying to
    // exhaust — 65 requests spread over >5 s never exceed 60 + refill, every
    // one returns 200, and the test fails claiming the limiter is broken.
    // That made it the suite's one timing flake, and it was SKIPPED in CI
    // rather than fixed, which hides real rate-limit regressions.
    //
    // A burst test has to actually burst. Firing all 65 at once makes the
    // outcome depend on the limiter instead of on how fast the runner is.
    let mut handles = Vec::with_capacity(65);
    for _ in 0..65 {
        let client = app.client.clone();
        let url = app.url("/health");
        handles.push(tokio::spawn(async move {
            client.get(url).send().await.unwrap().status().as_u16()
        }));
    }
    let mut statuses = Vec::with_capacity(handles.len());
    for h in handles {
        statuses.push(h.await.unwrap());
    }

    // Count how many got 429
    let rate_limited = statuses.iter().filter(|&&s| s == 429).count();
    let successful = statuses.iter().filter(|&&s| s == 200).count();

    // We expect at least 60 successful (the burst) and at least 1 rate-limited
    assert!(
        successful >= 60,
        "Expected at least 60 successful requests, got {}",
        successful
    );
    assert!(
        rate_limited >= 1,
        "Expected at least 1 rate-limited (429) response, got 0. All {} requests succeeded.",
        statuses.len()
    );
}

#[tokio::test]
async fn rate_limit_recovers_after_wait() {
    let app = TestApp::spawn().await;

    // Exhaust the burst
    for _ in 0..62 {
        app.client.get(app.url("/health")).send().await.unwrap();
    }

    // Wait for token replenishment (1 token per second, wait 2s for safety)
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Should succeed again
    let resp = app.client.get(app.url("/health")).send().await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "Request should succeed after rate limit recovery"
    );
}

use crate::fixtures::test_app::TestApp;

#[tokio::test]
async fn rate_limit_returns_429_after_burst() {
    let app = TestApp::spawn().await;

    // The rate limiter allows burst of 60 requests, then 1 per second.
    // Send 65 rapid requests to /health (a lightweight unauthenticated endpoint).
    let mut statuses = Vec::new();
    for _ in 0..65 {
        let resp = app.client.get(app.url("/health")).send().await.unwrap();
        statuses.push(resp.status().as_u16());
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

use crate::fixtures::test_app::TestApp;

#[tokio::test]
async fn preflight_options_returns_cors_headers() {
    let app = TestApp::spawn().await;

    // Send a preflight OPTIONS request with typical CORS headers
    let resp = app
        .client
        .request(reqwest::Method::OPTIONS, app.url("/api/auth/login"))
        .header("Origin", "http://example.com")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type,authorization")
        .send()
        .await
        .unwrap();

    // Default config has empty cors_origins -> fully permissive (Any)
    let status = resp.status().as_u16();
    assert!(
        status == 200 || status == 204,
        "OPTIONS preflight should return 200 or 204, got {}",
        status
    );

    let headers = resp.headers();
    assert!(
        headers.contains_key("access-control-allow-origin"),
        "Response should contain Access-Control-Allow-Origin header"
    );
    assert!(
        headers.contains_key("access-control-allow-methods"),
        "Response should contain Access-Control-Allow-Methods header"
    );
    assert!(
        headers.contains_key("access-control-allow-headers"),
        "Response should contain Access-Control-Allow-Headers header"
    );
}

#[tokio::test]
async fn cors_allows_any_origin_by_default() {
    let app = TestApp::spawn().await;

    let resp = app
        .client
        .get(app.url("/health"))
        .header("Origin", "http://random-origin.example.com")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);

    let allow_origin = resp
        .headers()
        .get("access-control-allow-origin")
        .expect("Expected Access-Control-Allow-Origin header")
        .to_str()
        .unwrap();

    assert_eq!(
        allow_origin, "*",
        "Default CORS should allow any origin (*)"
    );
}

#[tokio::test]
async fn cors_with_specific_origins() {
    // Spawn a server with specific CORS origins configured
    let app = TestApp::spawn_with_settings(|settings| {
        settings.app.cors_origins = vec![
            "http://allowed.example.com".to_string(),
            "http://also-allowed.example.com".to_string(),
        ];
    })
    .await;

    // Request from an allowed origin
    let resp = app
        .client
        .get(app.url("/health"))
        .header("Origin", "http://allowed.example.com")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);

    let allow_origin = resp
        .headers()
        .get("access-control-allow-origin");

    // When specific origins are configured, the server should echo back the allowed origin
    assert!(
        allow_origin.is_some(),
        "Expected Access-Control-Allow-Origin header for allowed origin"
    );
    assert_eq!(
        allow_origin.unwrap().to_str().unwrap(),
        "http://allowed.example.com"
    );
}

#[tokio::test]
async fn cors_rejects_disallowed_origin() {
    // Spawn a server with specific CORS origins configured
    let app = TestApp::spawn_with_settings(|settings| {
        settings.app.cors_origins = vec!["http://allowed.example.com".to_string()];
    })
    .await;

    // Request from a disallowed origin
    let resp = app
        .client
        .get(app.url("/health"))
        .header("Origin", "http://evil.example.com")
        .send()
        .await
        .unwrap();

    // The request itself may still succeed (CORS is enforced by browser),
    // but the Access-Control-Allow-Origin header should NOT be present
    // for disallowed origins.
    let allow_origin = resp.headers().get("access-control-allow-origin");
    assert!(
        allow_origin.is_none(),
        "Disallowed origin should not receive Access-Control-Allow-Origin header, but got: {:?}",
        allow_origin
    );
}

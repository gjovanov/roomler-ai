use crate::fixtures::test_app::TestApp;
use serde_json::Value;

/// Helper: join room and create N messages, returning all message IDs in creation order.
async fn seed_messages(
    app: &TestApp,
    tenant_id: &str,
    room_id: &str,
    token: &str,
    count: usize,
) -> Vec<String> {
    // Join room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant_id, room_id),
        token,
    )
    .send()
    .await
    .unwrap();

    let mut ids = Vec::new();
    for i in 1..=count {
        let resp = app
            .auth_post(
                &format!("/api/tenant/{}/room/{}/message", tenant_id, room_id),
                token,
            )
            .json(&serde_json::json!({
                "content": format!("Pagination test message {}", i),
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200, "Failed to create message {}", i);
        let json: Value = resp.json().await.unwrap();
        ids.push(json["id"].as_str().unwrap().to_string());
    }
    ids
}

#[tokio::test]
async fn paginate_messages_three_pages() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("page1").await;
    let room_id = &tenant.rooms[0].id;

    seed_messages(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        30,
    )
    .await;

    // Fetch page 1 with per_page=10
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=1&per_page=10",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"], 30);
    assert_eq!(json["per_page"], 10);
    assert_eq!(json["total_pages"], 3);
    assert_eq!(json["items"].as_array().unwrap().len(), 10);

    // Fetch page 3
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=3&per_page=10",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["items"].as_array().unwrap().len(), 10);
    assert_eq!(json["page"], 3);
}

#[tokio::test]
async fn per_page_is_clamped_to_100() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("page2").await;
    let room_id = &tenant.rooms[0].id;

    // Create a few messages (we just need the pagination metadata)
    seed_messages(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        5,
    )
    .await;

    // Request with per_page=1000
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=1&per_page=1000",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    // per_page in the response should be clamped to 100
    assert_eq!(
        json["per_page"], 100,
        "per_page should be clamped to 100, got {}",
        json["per_page"]
    );
}

#[tokio::test]
async fn cursor_pagination_with_before() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("page3").await;
    let room_id = &tenant.rooms[0].id;

    seed_messages(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        10,
    )
    .await;

    // Fetch all messages first to get a timestamp for the "before" cursor
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=1&per_page=100",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    let json: Value = resp.json().await.unwrap();
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 10);

    // Messages are sorted newest first. Take created_at of the 5th message (index 4)
    // to use as the "before" cursor — should return only older messages.
    let before_ts = items[4]["created_at"].as_str().unwrap();

    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=1&per_page=100&before={}",
                tenant.tenant_id, room_id, before_ts
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    let filtered_items = json["items"].as_array().unwrap();

    // Should return fewer items than the full 10 (only those created before the cursor)
    assert!(
        filtered_items.len() < 10,
        "Expected fewer than 10 items with 'before' cursor, got {}",
        filtered_items.len()
    );
}

#[tokio::test]
async fn total_count_and_total_pages_correct() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("page4").await;
    let room_id = &tenant.rooms[0].id;

    seed_messages(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        17,
    )
    .await;

    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=1&per_page=5",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"], 17);
    assert_eq!(json["per_page"], 5);
    // ceil(17/5) = 4
    assert_eq!(json["total_pages"], 4);
    assert_eq!(json["items"].as_array().unwrap().len(), 5);

    // Page 4 should have 2 items (17 - 15 = 2)
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=4&per_page=5",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["items"].as_array().unwrap().len(), 2);
}

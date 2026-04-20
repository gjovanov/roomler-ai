use crate::fixtures::test_app::TestApp;
use serde_json::Value;

/// Helper: admin joins room, sends a message mentioning member, returns the message JSON.
async fn send_mention_message(
    app: &TestApp,
    tenant_id: &str,
    room_id: &str,
    admin_token: &str,
    member_id: &str,
) -> Value {
    // Admin joins room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant_id, room_id),
        admin_token,
    )
    .send()
    .await
    .unwrap();

    // Send message with mention
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room/{}/message", tenant_id, room_id),
            admin_token,
        )
        .json(&serde_json::json!({
            "content": "Hey check this out",
            "mentions": {
                "users": [member_id],
                "everyone": false,
                "here": false,
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        200,
        "Failed to create mention message"
    );
    resp.json().await.unwrap()
}

#[tokio::test]
async fn mention_creates_notification() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("notif1").await;
    let room_id = &tenant.rooms[0].id;

    // Member joins the room so they can be mentioned
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.member.access_token,
    )
    .send()
    .await
    .unwrap();

    send_mention_message(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        &tenant.member.id,
    )
    .await;

    // Give async notification creation a moment
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Member should see the notification
    let resp = app
        .auth_get("/api/notification", &tenant.member.access_token)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    let items = json["items"].as_array().unwrap();
    assert!(
        !items.is_empty(),
        "Expected at least 1 notification for mentioned user, got 0"
    );

    // Verify notification type is mention
    let first = &items[0];
    assert_eq!(first["notification_type"], "mention");
    assert_eq!(first["is_read"], false);
}

#[tokio::test]
async fn unread_count_reflects_notifications() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("notif2").await;
    let room_id = &tenant.rooms[0].id;

    // Member joins room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.member.access_token,
    )
    .send()
    .await
    .unwrap();

    // Initially unread count should be 0
    let resp = app
        .auth_get(
            "/api/notification/unread-count",
            &tenant.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["count"], 0);

    // Send a mention
    send_mention_message(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        &tenant.member.id,
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Unread count should now be >= 1
    let resp = app
        .auth_get(
            "/api/notification/unread-count",
            &tenant.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert!(
        json["count"].as_u64().unwrap() >= 1,
        "Expected unread count >= 1, got {}",
        json["count"]
    );
}

#[tokio::test]
async fn mark_single_notification_read() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("notif3").await;
    let room_id = &tenant.rooms[0].id;

    // Member joins room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.member.access_token,
    )
    .send()
    .await
    .unwrap();

    send_mention_message(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        &tenant.member.id,
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Fetch the notification
    let resp = app
        .auth_get("/api/notification", &tenant.member.access_token)
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    let items = json["items"].as_array().unwrap();
    assert!(!items.is_empty(), "Expected at least 1 notification");
    let notification_id = items[0]["id"].as_str().unwrap();

    // Mark it as read
    let resp = app
        .auth_put(
            &format!("/api/notification/{}/read", notification_id),
            &tenant.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["read"], true);

    // Unread count should now be 0
    let resp = app
        .auth_get(
            "/api/notification/unread-count",
            &tenant.member.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["count"], 0);
}

#[tokio::test]
async fn mark_all_notifications_read() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("notif4").await;
    let room_id = &tenant.rooms[0].id;

    // Member joins room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.member.access_token,
    )
    .send()
    .await
    .unwrap();

    // Create 2 mention messages
    for _ in 0..2 {
        send_mention_message(
            &app,
            &tenant.tenant_id,
            room_id,
            &tenant.admin.access_token,
            &tenant.member.id,
        )
        .await;
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Mark all as read
    let resp = app
        .auth_post("/api/notification/read-all", &tenant.member.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert!(
        json["marked"].as_u64().unwrap() >= 2,
        "Expected at least 2 marked, got {}",
        json["marked"]
    );

    // Unread count should be 0
    let resp = app
        .auth_get(
            "/api/notification/unread-count",
            &tenant.member.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["count"], 0);
}

#[tokio::test]
async fn notifications_are_user_scoped() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("notif5").await;
    let room_id = &tenant.rooms[0].id;

    // Member joins room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.member.access_token,
    )
    .send()
    .await
    .unwrap();

    // Admin mentions member -> notification goes to member only
    send_mention_message(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        &tenant.member.id,
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Admin should see 0 notifications (the mention was for member)
    let resp = app
        .auth_get("/api/notification", &tenant.admin.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    let admin_items = json["items"].as_array().unwrap();
    assert_eq!(
        admin_items.len(),
        0,
        "Admin should not see member's notifications"
    );

    // Member should see >= 1 notification
    let resp = app
        .auth_get("/api/notification", &tenant.member.access_token)
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    let member_items = json["items"].as_array().unwrap();
    assert!(
        !member_items.is_empty(),
        "Member should see at least 1 notification"
    );
}

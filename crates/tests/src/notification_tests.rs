use crate::fixtures::test_app::TestApp;
use futures::StreamExt;
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

// ─── P4 — cross-org payload contracts + read sync + unread summary ─────

/// Await the next `{type: <ty>}` frame on a user WS, skipping others.
async fn wait_for_event(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    ty: &str,
    secs: u64,
) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = tokio::time::timeout(remaining, ws.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {ty}"))
            .expect("ws stream ended")
            .expect("ws frame error");
        let Ok(v) = serde_json::from_str::<Value>(msg.to_text().unwrap_or_default()) else {
            continue;
        };
        if v["type"] == ty {
            return v;
        }
    }
}

/// P4 — `notification:new` (WS) and the REST rows both carry `tenant_id`
/// so multi-org clients can route badges per org. Contract lock.
#[tokio::test]
async fn notification_payloads_carry_tenant_id() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("notif6").await;
    let room_id = &tenant.rooms[0].id;

    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.member.access_token,
    )
    .send()
    .await
    .unwrap();

    let ws_url = format!("ws://{}/ws?token={}", app.addr, tenant.member.access_token);
    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
    ws.next().await; // connected frame

    send_mention_message(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        &tenant.member.id,
    )
    .await;

    let ev = wait_for_event(&mut ws, "notification:new", 5).await;
    assert_eq!(
        ev["data"]["tenant_id"],
        tenant.tenant_id.as_str(),
        "ev: {ev}"
    );
    assert_eq!(ev["data"]["notification_type"], "mention");

    let resp = app
        .auth_get("/api/notification", &tenant.member.access_token)
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    let items = json["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert_eq!(items[0]["tenant_id"], tenant.tenant_id.as_str());
}

/// P4 — marking all read pushes `notification:unread_count` over WS
/// (cross-device read sync: the bell on every other device converges
/// without a reload).
#[tokio::test]
async fn read_all_emits_unread_count_over_ws() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("notif7").await;
    let room_id = &tenant.rooms[0].id;

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
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Connect the member's "other device" AFTER the mention exists so the
    // only expected frames are ours.
    let ws_url = format!("ws://{}/ws?token={}", app.addr, tenant.member.access_token);
    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();
    ws.next().await;

    let resp = app
        .auth_post("/api/notification/read-all", &tenant.member.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let ev = wait_for_event(&mut ws, "notification:unread_count", 5).await;
    assert_eq!(ev["data"]["count"], 0, "ev: {ev}");
}

/// P4 — `GET /api/user/unread-summary`: per-org rows for EVERY membership,
/// with unread messages/rooms + notification/mention counts scoped per
/// tenant (a second, quiet org reports zeros).
#[tokio::test]
async fn unread_summary_reports_per_tenant_counts() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("notif8").await;
    let room_id = &tenant.rooms[0].id;

    // The member also owns a second, quiet org.
    let resp = app
        .auth_post("/api/tenant", &tenant.member.access_token)
        .json(&serde_json::json!({ "name": "Second Org", "slug": "notif8-second" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "second tenant create");
    let second: Value = resp.json().await.unwrap();
    let second_id = second["id"].as_str().unwrap().to_string();

    // Activity in the FIRST org: a mention (notification + unread message).
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

    let resp = app
        .auth_get("/api/user/unread-summary", &tenant.member.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    let rows = json["tenants"].as_array().unwrap();
    assert!(rows.len() >= 2, "one row per membership: {json}");

    let first = rows
        .iter()
        .find(|r| r["tenant_id"] == tenant.tenant_id.as_str())
        .unwrap_or_else(|| panic!("first org missing from summary: {json}"));
    assert!(
        first["unread_messages"].as_u64().unwrap() >= 1,
        "mention message unread: {first}"
    );
    assert!(first["unread_rooms"].as_u64().unwrap() >= 1);
    assert!(first["notifications"].as_u64().unwrap() >= 1);
    assert!(first["mentions"].as_u64().unwrap() >= 1);

    let quiet = rows
        .iter()
        .find(|r| r["tenant_id"] == second_id.as_str())
        .unwrap_or_else(|| panic!("second org missing from summary: {json}"));
    assert_eq!(quiet["unread_messages"], 0);
    assert_eq!(quiet["notifications"], 0);
}

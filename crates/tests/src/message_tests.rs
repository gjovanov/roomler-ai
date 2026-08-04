use crate::fixtures::test_app::TestApp;
use futures::StreamExt;
use serde_json::Value;

#[tokio::test]
async fn create_and_list_messages() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msgtest").await;
    let room_id = &tenant.rooms[0].id;

    // Admin joins the room first
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Create messages
    for i in 1..=3 {
        let resp = app
            .auth_post(
                &format!("/api/tenant/{}/room/{}/message", tenant.tenant_id, room_id),
                &tenant.admin.access_token,
            )
            .json(&serde_json::json!({
                "content": format!("Hello message {}", i),
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            200,
            "Failed to create message {}",
            i
        );
    }

    // List messages (paginated response)
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/room/{}/message", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"], 3);
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
}

#[tokio::test]
async fn update_message() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msgedit").await;
    let room_id = &tenant.rooms[0].id;

    // Join room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Create a message
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room/{}/message", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "content": "Original message",
        }))
        .send()
        .await
        .unwrap();

    let msg: Value = resp.json().await.unwrap();
    let message_id = msg["id"].as_str().unwrap();

    // Update the message
    let resp = app
        .auth_put(
            &format!(
                "/api/tenant/{}/room/{}/message/{}",
                tenant.tenant_id, room_id, message_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "content": "Updated message",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["content"], "Updated message");
    assert_eq!(json["is_edited"], true);
}

#[tokio::test]
async fn delete_message_soft_deletes() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msgdel").await;
    let room_id = &tenant.rooms[0].id;

    // Join room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Create a message
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room/{}/message", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "content": "To be deleted",
        }))
        .send()
        .await
        .unwrap();

    let msg: Value = resp.json().await.unwrap();
    let message_id = msg["id"].as_str().unwrap();

    // Delete
    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/room/{}/message/{}",
                tenant.tenant_id, room_id, message_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);

    // List messages - should be empty (soft deleted not returned)
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/room/{}/message", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"], 0);
    assert_eq!(json["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn message_broadcast_reaches_member_and_sender() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msgws").await;
    let room_id = &tenant.rooms[0].id;

    // Both users join the room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.member.access_token,
    )
    .send()
    .await
    .unwrap();

    // Connect WS for admin (sender) and member (receiver)
    let ws_url_admin = format!("ws://{}/ws?token={}", app.addr, tenant.admin.access_token);
    let ws_url_member = format!("ws://{}/ws?token={}", app.addr, tenant.member.access_token);

    let (mut ws_admin, _) = tokio_tungstenite::connect_async(&ws_url_admin)
        .await
        .unwrap();
    let (mut ws_member, _) = tokio_tungstenite::connect_async(&ws_url_member)
        .await
        .unwrap();

    // Drain "connected" messages
    ws_admin.next().await;
    ws_member.next().await;

    // Admin sends a message via HTTP
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room/{}/message", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "content": "Hello from admin" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Member should receive message:create via WS
    let msg = tokio::time::timeout(std::time::Duration::from_secs(3), ws_member.next())
        .await
        .expect("Timed out waiting for WS message")
        .unwrap()
        .unwrap();

    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "message:create");
    assert_eq!(parsed["data"]["content"], "Hello from admin");
    // P4 contract lock: the fan-out payload carries tenant_id so multi-org
    // clients can route non-active-org messages into per-org badges.
    assert_eq!(parsed["data"]["tenant_id"], tenant.tenant_id.as_str());

    // The sender's connection ALSO receives it (2026-08-04): broadcasts are
    // per-user, so including the sender is what delivers realtime messages
    // to their OTHER browsers/devices; the client dedups by message id.
    let msg = tokio::time::timeout(std::time::Duration::from_secs(3), ws_admin.next())
        .await
        .expect("sender must receive their own message:create (multi-device fan-out)")
        .unwrap()
        .unwrap();

    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "message:create");
    assert_eq!(parsed["data"]["content"], "Hello from admin");

    ws_admin.close(None).await.ok();
    ws_member.close(None).await.ok();
}

/// PR-2 (2026-08-04): `POST message/read-all` marks EVERY unread message in
/// the room read for the caller — same filter as `unread-count`, so stuck
/// badges (messages outside the fetch window, call-view reads) heal to 0.
#[tokio::test]
async fn read_all_zeroes_unread_count() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("readall1").await;
    let room_id = &tenant.rooms[0].id;

    for token in [&tenant.admin.access_token, &tenant.member.access_token] {
        app.auth_post(
            &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
            token,
        )
        .send()
        .await
        .unwrap();
    }

    // Admin writes 8 messages; the member has read none of them.
    for i in 0..8 {
        app.auth_post(
            &format!("/api/tenant/{}/room/{}/message", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "content": format!("Unread {i}") }))
        .send()
        .await
        .unwrap();
    }

    let count: Value = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message/unread-count",
                tenant.tenant_id, room_id
            ),
            &tenant.member.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(count["count"], 8);

    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/message/read-all",
                tenant.tenant_id, room_id
            ),
            &tenant.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let marked: Value = resp.json().await.unwrap();
    assert_eq!(marked["marked"], 8);

    let count: Value = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message/unread-count",
                tenant.tenant_id, room_id
            ),
            &tenant.member.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(count["count"], 0);

    // Idempotent: a second read-all marks nothing further.
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/message/read-all",
                tenant.tenant_id, room_id
            ),
            &tenant.member.access_token,
        )
        .send()
        .await
        .unwrap();
    let marked: Value = resp.json().await.unwrap();
    assert_eq!(marked["marked"], 0);
}

/// Non-members must not be able to mark-all-read.
#[tokio::test]
async fn read_all_requires_membership() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("readall2").await;
    let outsider = app.seed_tenant("readall2b").await;
    let room_id = &tenant.rooms[0].id;

    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/message/read-all",
                tenant.tenant_id, room_id
            ),
            &outsider.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

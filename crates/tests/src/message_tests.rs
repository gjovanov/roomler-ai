use crate::fixtures::test_app::TestApp;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn create_and_list_messages() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msgtest").await;
    let channel_id = &tenant.channels[0].id;

    // Admin joins the channel first
    app.auth_post(
        &format!(
            "/api/tenant/{}/channel/{}/join",
            tenant.tenant_id, channel_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Create messages
    for i in 1..=3 {
        let resp = app
            .auth_post(
                &format!(
                    "/api/tenant/{}/channel/{}/message",
                    tenant.tenant_id, channel_id
                ),
                &tenant.admin.access_token,
            )
            .json(&serde_json::json!({
                "content": format!("Hello message {}", i),
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200, "Failed to create message {}", i);
    }

    // List messages (paginated response)
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/channel/{}/message",
                tenant.tenant_id, channel_id
            ),
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
    let channel_id = &tenant.channels[0].id;

    // Join channel
    app.auth_post(
        &format!(
            "/api/tenant/{}/channel/{}/join",
            tenant.tenant_id, channel_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Create a message
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/channel/{}/message",
                tenant.tenant_id, channel_id
            ),
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
                "/api/tenant/{}/channel/{}/message/{}",
                tenant.tenant_id, channel_id, message_id
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
    assert_eq!(json["updated"], true);
}

#[tokio::test]
async fn delete_message_soft_deletes() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msgdel").await;
    let channel_id = &tenant.channels[0].id;

    // Join channel
    app.auth_post(
        &format!(
            "/api/tenant/{}/channel/{}/join",
            tenant.tenant_id, channel_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Create a message
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/channel/{}/message",
                tenant.tenant_id, channel_id
            ),
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
                "/api/tenant/{}/channel/{}/message/{}",
                tenant.tenant_id, channel_id, message_id
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
            &format!(
                "/api/tenant/{}/channel/{}/message",
                tenant.tenant_id, channel_id
            ),
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
async fn message_broadcast_excludes_sender_reaches_member() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msgws").await;
    let channel_id = &tenant.channels[0].id;

    // Both users join the channel
    app.auth_post(
        &format!("/api/tenant/{}/channel/{}/join", tenant.tenant_id, channel_id),
        &tenant.admin.access_token,
    ).send().await.unwrap();

    app.auth_post(
        &format!("/api/tenant/{}/channel/{}/join", tenant.tenant_id, channel_id),
        &tenant.member.access_token,
    ).send().await.unwrap();

    // Connect WS for admin (sender) and member (receiver)
    let ws_url_admin = format!("ws://{}/ws?token={}", app.addr, tenant.admin.access_token);
    let ws_url_member = format!("ws://{}/ws?token={}", app.addr, tenant.member.access_token);

    let (mut ws_admin, _) = tokio_tungstenite::connect_async(&ws_url_admin).await.unwrap();
    let (mut ws_member, _) = tokio_tungstenite::connect_async(&ws_url_member).await.unwrap();

    // Drain "connected" messages
    ws_admin.next().await;
    ws_member.next().await;

    // Admin sends a message via HTTP
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/channel/{}/message", tenant.tenant_id, channel_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "content": "Hello from admin" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Member should receive message:create via WS
    let msg = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        ws_member.next(),
    ).await.expect("Timed out waiting for WS message").unwrap().unwrap();

    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "message:create");
    assert_eq!(parsed["data"]["content"], "Hello from admin");

    // Admin should NOT receive message:create (sender excluded from broadcast).
    // Send a ping to flush any pending messages, then check.
    ws_admin.send(Message::Text(
        serde_json::to_string(&serde_json::json!({ "type": "ping" })).unwrap().into(),
    )).await.unwrap();

    let msg = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        ws_admin.next(),
    ).await.expect("Timed out waiting for pong").unwrap().unwrap();

    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "pong", "Admin should receive pong, not message:create (sender excluded)");

    ws_admin.close(None).await.ok();
    ws_member.close(None).await.ok();
}

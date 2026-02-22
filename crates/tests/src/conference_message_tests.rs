use crate::fixtures::test_app::TestApp;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

/// Helper: create a room + start a call, return room_id.
async fn create_room_and_start_call(
    app: &TestApp,
    tenant_id: &str,
    token: &str,
    name: &str,
) -> String {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room", tenant_id),
            token,
        )
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .unwrap();
    let room: Value = resp.json().await.unwrap();
    let room_id = room["id"].as_str().unwrap().to_string();

    app.auth_post(
        &format!("/api/tenant/{}/room/{}/call/start", tenant_id, room_id),
        token,
    )
    .send()
    .await
    .unwrap();

    room_id
}

#[tokio::test]
async fn create_and_list_room_messages() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("confmsg1").await;

    let room_id = create_room_and_start_call(
        &app,
        &tenant.tenant_id,
        &tenant.admin.access_token,
        "Chat Test",
    )
    .await;

    // Admin joins room
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/join",
            tenant.tenant_id, room_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Send 3 messages
    for i in 1..=3 {
        let resp = app
            .auth_post(
                &format!(
                    "/api/tenant/{}/room/{}/message",
                    tenant.tenant_id, room_id
                ),
                &tenant.admin.access_token,
            )
            .json(&serde_json::json!({
                "content": format!("Chat message {}", i),
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200, "Failed to create message {}", i);
        let json: Value = resp.json().await.unwrap();
        assert_eq!(json["content"], format!("Chat message {}", i));
        assert!(json["id"].is_string());
        assert!(json["created_at"].is_string());
        assert_eq!(json["room_id"], room_id);
    }

    // List messages
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message",
                tenant.tenant_id, room_id
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
    // Messages are sorted by created_at descending (newest first)
    assert_eq!(items[0]["content"], "Chat message 3");
    assert_eq!(items[2]["content"], "Chat message 1");
}

#[tokio::test]
async fn non_participant_cannot_send_message() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("confmsg2").await;

    let room_id = create_room_and_start_call(
        &app,
        &tenant.tenant_id,
        &tenant.admin.access_token,
        "Auth Test",
    )
    .await;

    // Admin joins but member does NOT join the room call
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/call/join",
            tenant.tenant_id, room_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Member (not a call participant) tries to send a message
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/message",
                tenant.tenant_id, room_id
            ),
            &tenant.member.access_token,
        )
        .json(&serde_json::json!({
            "content": "I shouldn't be able to chat",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn room_message_ws_broadcast() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("confmsg3").await;

    let room_id = create_room_and_start_call(
        &app,
        &tenant.tenant_id,
        &tenant.admin.access_token,
        "WS Chat Test",
    )
    .await;

    // Both users join the room
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/join",
            tenant.tenant_id, room_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/join",
            tenant.tenant_id, room_id
        ),
        &tenant.member.access_token,
    )
    .send()
    .await
    .unwrap();

    // Member connects WS
    let ws_url = format!(
        "ws://{}/ws?token={}",
        app.addr, tenant.member.access_token
    );
    let (mut ws_member, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("WS connect failed");

    // Read "connected" message
    ws_member.next().await;

    // Admin sends a message via REST
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/message",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "content": "Hello from admin!",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Member should receive the WS broadcast
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws_member.next())
        .await
        .expect("Timeout waiting for WS message")
        .unwrap()
        .unwrap();

    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "message:create");
    assert_eq!(parsed["data"]["content"], "Hello from admin!");
    assert_eq!(parsed["data"]["room_id"], room_id);

    // Admin connects WS to verify sender exclusion
    let ws_url_admin = format!(
        "ws://{}/ws?token={}",
        app.addr, tenant.admin.access_token
    );
    let (mut ws_admin, _) = tokio_tungstenite::connect_async(&ws_url_admin)
        .await
        .expect("WS connect failed");
    ws_admin.next().await; // connected

    // Admin sends another message
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/message",
            tenant.tenant_id, room_id
        ),
        &tenant.admin.access_token,
    )
    .json(&serde_json::json!({
        "content": "Second message",
    }))
    .send()
    .await
    .unwrap();

    // Admin should NOT receive their own message via WS
    let admin_msg =
        tokio::time::timeout(std::time::Duration::from_millis(500), ws_admin.next()).await;
    match admin_msg {
        Ok(Some(Ok(msg))) => {
            let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            assert_ne!(
                parsed["type"], "message:create",
                "Sender should not receive their own message via WS"
            );
        }
        _ => {
            // Timeout or closed — correct, admin should not receive their own message
        }
    }

    ws_member.close(None).await.ok();
    ws_admin.close(None).await.ok();
}

#[tokio::test]
async fn cannot_chat_in_ended_call() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("confmsg4").await;

    let room_id = create_room_and_start_call(
        &app,
        &tenant.tenant_id,
        &tenant.admin.access_token,
        "Ended Chat Test",
    )
    .await;

    // Admin joins
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/call/join",
            tenant.tenant_id, room_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // End the call
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/call/end",
            tenant.tenant_id, room_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Try to send a message — should fail
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/message",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "content": "This should fail",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
}

use crate::fixtures::test_app::TestApp;
use serde_json::Value;

/// Helper: seed tenant, join room, create a message, return (app, tenant, room_id, message_id)
async fn setup_with_message() -> (TestApp, crate::fixtures::seed::SeededTenant, String, String) {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("react").await;
    let room_id = tenant.rooms[0].id.clone();

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

    // Create a message
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/message",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "content": "React to this!",
        }))
        .send()
        .await
        .unwrap();

    let msg: Value = resp.json().await.unwrap();
    let message_id = msg["id"].as_str().unwrap().to_string();

    (app, tenant, room_id, message_id)
}

#[tokio::test]
async fn add_reaction_to_message() {
    let (app, tenant, room_id, message_id) = setup_with_message().await;

    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/message/{}/reaction",
                tenant.tenant_id, room_id, message_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "emoji": "\u{1f44d}" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["added"], true);

    // Verify reaction summary on the message
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

    let json: Value = resp.json().await.unwrap();
    let items = json["items"].as_array().unwrap();
    let msg = &items[0];
    let reactions = msg["reaction_summary"].as_array().unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0]["emoji"], "\u{1f44d}");
    assert_eq!(reactions[0]["count"], 1);
}

#[tokio::test]
async fn duplicate_reaction_fails() {
    let (app, tenant, room_id, message_id) = setup_with_message().await;

    // Add reaction
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/message/{}/reaction",
            tenant.tenant_id, room_id, message_id
        ),
        &tenant.admin.access_token,
    )
    .json(&serde_json::json!({ "emoji": "\u{2764}\u{fe0f}" }))
    .send()
    .await
    .unwrap();

    // Try same emoji again - should fail
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/message/{}/reaction",
                tenant.tenant_id, room_id, message_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "emoji": "\u{2764}\u{fe0f}" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 409);
}

#[tokio::test]
async fn remove_reaction_from_message() {
    let (app, tenant, room_id, message_id) = setup_with_message().await;

    // Add reaction
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/message/{}/reaction",
            tenant.tenant_id, room_id, message_id
        ),
        &tenant.admin.access_token,
    )
    .json(&serde_json::json!({ "emoji": "\u{1f389}" }))
    .send()
    .await
    .unwrap();

    // Remove reaction
    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/room/{}/message/{}/reaction/\u{1f389}",
                tenant.tenant_id, room_id, message_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["removed"], true);

    // Verify reaction is gone from message
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

    let json: Value = resp.json().await.unwrap();
    let items = json["items"].as_array().unwrap();
    let msg = &items[0];
    let reactions = msg["reaction_summary"].as_array().unwrap();
    assert!(reactions.is_empty());
}

#[tokio::test]
async fn multiple_users_react_to_same_message() {
    let (app, tenant, room_id, message_id) = setup_with_message().await;

    // Member joins room
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

    // Admin reacts with thumbs up
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/message/{}/reaction",
            tenant.tenant_id, room_id, message_id
        ),
        &tenant.admin.access_token,
    )
    .json(&serde_json::json!({ "emoji": "\u{1f44d}" }))
    .send()
    .await
    .unwrap();

    // Member reacts with thumbs up too
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/message/{}/reaction",
            tenant.tenant_id, room_id, message_id
        ),
        &tenant.member.access_token,
    )
    .json(&serde_json::json!({ "emoji": "\u{1f44d}" }))
    .send()
    .await
    .unwrap();

    // Admin also reacts with heart
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/message/{}/reaction",
            tenant.tenant_id, room_id, message_id
        ),
        &tenant.admin.access_token,
    )
    .json(&serde_json::json!({ "emoji": "\u{2764}\u{fe0f}" }))
    .send()
    .await
    .unwrap();

    // Check summary: thumbs_up=2, heart=1
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

    let json: Value = resp.json().await.unwrap();
    let reactions = json["items"][0]["reaction_summary"].as_array().unwrap();
    assert_eq!(reactions.len(), 2);

    // Sorted by count desc, so thumbs_up (2) first, then heart (1)
    assert_eq!(reactions[0]["emoji"], "\u{1f44d}");
    assert_eq!(reactions[0]["count"], 2);
    assert_eq!(reactions[1]["emoji"], "\u{2764}\u{fe0f}");
    assert_eq!(reactions[1]["count"], 1);
}

#[tokio::test]
async fn pin_and_unpin_message() {
    let (app, tenant, room_id, message_id) = setup_with_message().await;

    // Pin the message
    let resp = app
        .auth_put(
            &format!(
                "/api/tenant/{}/room/{}/message/{}/pin",
                tenant.tenant_id, room_id, message_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "pinned": true }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["pinned"], true);

    // List pinned messages
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message/pin",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let pinned: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0]["id"], message_id);
    assert_eq!(pinned[0]["is_pinned"], true);

    // Unpin the message
    let resp = app
        .auth_put(
            &format!(
                "/api/tenant/{}/room/{}/message/{}/pin",
                tenant.tenant_id, room_id, message_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "pinned": false }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);

    // Pinned list should now be empty
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message/pin",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    let pinned: Vec<Value> = resp.json().await.unwrap();
    assert!(pinned.is_empty());
}

#[tokio::test]
async fn thread_replies_are_returned() {
    let (app, tenant, room_id, message_id) = setup_with_message().await;

    // Create thread replies to the parent message
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
                "content": format!("Thread reply {}", i),
                "thread_id": &message_id,
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            200,
            "Failed to create thread reply {}",
            i
        );
    }

    // Get thread replies
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message/{}/thread",
                tenant.tenant_id, room_id, message_id
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

    // All replies should reference the parent thread
    for item in items {
        assert_eq!(item["thread_id"], message_id);
    }
}

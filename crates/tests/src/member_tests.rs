use crate::fixtures::test_app::TestApp;
use serde_json::Value;

#[tokio::test]
async fn list_room_members_returns_paginated_items_with_user_details() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("members").await;
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

    // Fetch members
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/room/{}/member", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();

    // Response must be paginated with items array
    assert!(
        json["items"].is_array(),
        "Response must contain 'items' array"
    );
    assert!(json["total"].is_number(), "Response must contain 'total'");

    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "Should have 2 members (admin + member)");

    // Each member should have user details enriched
    for item in items {
        assert!(item["id"].is_string(), "Member must have 'id'");
        assert!(item["user_id"].is_string(), "Member must have 'user_id'");
        assert!(
            item["display_name"].is_string(),
            "Member must have 'display_name'"
        );
        assert!(item["username"].is_string(), "Member must have 'username'");
        assert!(
            item["joined_at"].is_string(),
            "Member must have 'joined_at'"
        );

        let display_name = item["display_name"].as_str().unwrap();
        assert!(!display_name.is_empty(), "display_name must not be empty");

        let username = item["username"].as_str().unwrap();
        assert!(!username.is_empty(), "username must not be empty");
    }

    // Verify specific users are present
    let usernames: Vec<&str> = items
        .iter()
        .map(|i| i["username"].as_str().unwrap())
        .collect();
    assert!(
        usernames.contains(&tenant.admin.username.as_str()),
        "Admin '{}' should be in members list, got {:?}",
        tenant.admin.username,
        usernames,
    );
    assert!(
        usernames.contains(&tenant.member.username.as_str()),
        "Member '{}' should be in members list, got {:?}",
        tenant.member.username,
        usernames,
    );
}

#[tokio::test]
async fn list_room_members_requires_tenant_membership() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("memauth").await;
    let room_id = &tenant.rooms[0].id;

    // Register a third user who is NOT a tenant member
    let outsider = app
        .register_user(
            "outsider@memauth.test",
            "memauth_outsider",
            "Outsider",
            "Outsider123!",
            None,
            None,
        )
        .await;

    // Outsider should be forbidden from listing room members
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/room/{}/member", tenant.tenant_id, room_id),
            &outsider.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "Non-tenant-member should get 403"
    );
}

#[tokio::test]
async fn create_message_with_mentions() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("mentions").await;
    let room_id = &tenant.rooms[0].id;

    // Both users join
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

    // Admin sends a message mentioning the member
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room/{}/message", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "content": format!("Hey @{} check this out", tenant.member.username),
            "mentions": {
                "users": [&tenant.member.id],
                "everyone": false,
                "here": false,
            },
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let msg: Value = resp.json().await.unwrap();
    assert!(
        msg["id"].is_string(),
        "Message should be created with an ID"
    );
    assert_eq!(
        msg["content"].as_str().unwrap(),
        format!("Hey @{} check this out", tenant.member.username)
    );
}

#[tokio::test]
async fn create_message_with_everyone_mention() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("evmention").await;
    let room_id = &tenant.rooms[0].id;

    // Admin joins
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Send message with @everyone
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room/{}/message", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "content": "Attention @everyone!",
            "mentions": {
                "users": [],
                "everyone": true,
                "here": false,
            },
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let msg: Value = resp.json().await.unwrap();
    assert!(
        msg["id"].is_string(),
        "Message with @everyone should be created"
    );
    assert_eq!(msg["content"].as_str().unwrap(), "Attention @everyone!");
}

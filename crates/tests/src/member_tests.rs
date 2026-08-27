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

/// Deferred-S4 — GET /api/tenant/{tid}/member/me powers client-side nav
/// gating (Devices + Network groups). Locks: owner flag, mask presence,
/// plain member lacking fleet bits, and non-member 403.
#[tokio::test]
async fn member_me_reports_permissions_owner_and_403s_outsiders() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("permme").await;

    const ADMINISTRATOR: u64 = 1 << 23;
    const MANAGE_AGENTS: u64 = 1 << 24;
    const REMOTE_CONTROL: u64 = 1 << 25;

    // Tenant creator: owner + a mask that grants the fleet surfaces.
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/member/me", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["is_owner"].as_bool(), Some(true));
    let admin_perms = json["permissions"].as_u64().unwrap();
    assert!(
        admin_perms & (ADMINISTRATOR | MANAGE_AGENTS | REMOTE_CONTROL) != 0,
        "tenant creator must hold ADMINISTRATOR or fleet bits, got {admin_perms:#x}"
    );

    // Plain member: not owner, no fleet bits (nav hides Devices/Network).
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/member/me", t.tenant_id),
            &t.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["is_owner"].as_bool(), Some(false));
    let member_perms = json["permissions"].as_u64().unwrap();
    assert_eq!(
        member_perms & (ADMINISTRATOR | MANAGE_AGENTS | REMOTE_CONTROL),
        0,
        "plain member must not hold admin/fleet bits, got {member_perms:#x}"
    );

    // A user from another tenant is not a member here → 403.
    let other = app.seed_tenant("permme2").await;
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/member/me", t.tenant_id),
            &other.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

// ─── FR-11 (#784): members grid — email, q, sort, add-by-email, remove ───

/// The tenant member list is now a grid feed: paginated envelope, email on
/// every row, case-insensitive `q` over display name / email / nickname, and
/// a whitelisted sort.
#[tokio::test]
async fn tenant_member_list_grid_email_q_and_sort() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("memgrid").await;

    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/member", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"].as_u64(), Some(2));
    assert_eq!(json["page"].as_u64(), Some(1));
    let items = json["items"].as_array().unwrap();
    let emails: Vec<&str> = items.iter().map(|i| i["email"].as_str().unwrap()).collect();
    assert!(
        emails.contains(&"admin@memgrid.test") && emails.contains(&"member@memgrid.test"),
        "rows must carry the joined email, got {emails:?}"
    );
    // Default order = joined_at asc: the tenant creator joined first.
    assert_eq!(items[0]["email"], "admin@memgrid.test");

    // q by email fragment finds exactly the member.
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/member?q=member%40memgrid", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"].as_u64(), Some(1));
    assert_eq!(json["items"][0]["email"], "member@memgrid.test");

    // q by display-name fragment (proves the users-collection join is what
    // the filter runs over, not the membership row alone).
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/member?q=memgrid%20Admin", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"].as_u64(), Some(1));
    assert_eq!(json["items"][0]["email"], "admin@memgrid.test");

    // sort=name desc puts "… Member" before "… Admin".
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/member?sort=name&dir=desc", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["items"][0]["email"], "member@memgrid.test");

    // Unknown sort key / dir must 400, never fall back silently.
    for bad in [
        format!("/api/tenant/{}/member?sort=evil", t.tenant_id),
        format!("/api/tenant/{}/member?dir=sideways", t.tenant_id),
    ] {
        let resp = app
            .auth_get(&bad, &t.admin.access_token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400, "{bad} must 400");
    }
}

/// FR-11: POST /member accepts `email` as an alternative to `user_id`,
/// resolving only a PROVEN address (the users.email reservation), and the
/// exactly-one-of rule is enforced.
#[tokio::test]
async fn add_member_by_email_resolves_case_insensitively_or_404s() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("addmail").await;
    let joiner = app
        .register_user(
            "joiner@addmail.test",
            "addmail_joiner",
            "Joiner",
            "Joiner123!",
            None,
            None,
        )
        .await;

    let member_url = format!("/api/tenant/{}/member", t.tenant_id);

    // Unknown address is a 404 (the UI points the admin at Invites instead).
    let resp = app
        .auth_post(&member_url, &t.admin.access_token)
        .json(&serde_json::json!({ "email": "nobody@addmail.test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    // Both fields, or neither, is a 400.
    let resp = app
        .auth_post(&member_url, &t.admin.access_token)
        .json(&serde_json::json!({ "email": "joiner@addmail.test", "user_id": joiner.id }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let resp = app
        .auth_post(&member_url, &t.admin.access_token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    // Mixed-case spelling still resolves (lookup normalizes like signup did).
    let resp = app
        .auth_post(&member_url, &t.admin.access_token)
        .json(&serde_json::json!({ "email": "Joiner@ADDMAIL.test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["user_id"].as_str(), Some(joiner.id.as_str()));

    // Repeating the add is a 409 — already a member.
    let resp = app
        .auth_post(&member_url, &t.admin.access_token)
        .json(&serde_json::json!({ "email": "joiner@addmail.test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);

    // A plain member holds no INVITE_MEMBERS: 403, before any resolution.
    let resp = app
        .auth_post(&member_url, &t.member.access_token)
        .json(&serde_json::json!({ "email": "joiner@addmail.test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

/// FR-11: DELETE /member/{user_id}. Kicking needs KICK_MEMBERS, the owner is
/// unremovable (409, even by themselves), and self-removal is leaving.
#[tokio::test]
async fn remove_member_owner_409_kick_permission_and_self_leave() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("memrm").await;
    let third = app
        .register_user(
            "third@memrm.test",
            "memrm_third",
            "Third",
            "Third123!",
            None,
            None,
        )
        .await;
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/member", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&serde_json::json!({ "email": "third@memrm.test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);

    // The owner cannot be removed — not even by themselves.
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/member/{}", t.tenant_id, t.admin.id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);

    // A plain member cannot kick someone else.
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/member/{}", t.tenant_id, third.id),
            &t.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    // The admin (owner role carries KICK_MEMBERS) kicks the third user.
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/member/{}", t.tenant_id, third.id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["removed"].as_bool(), Some(true));

    // The row is really gone: a second delete finds nothing.
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/member/{}", t.tenant_id, third.id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    // Self-removal = leaving; no permission needed.
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/member/{}", t.tenant_id, t.member.id),
            &t.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/member", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"].as_u64(), Some(1), "only the owner remains");
}

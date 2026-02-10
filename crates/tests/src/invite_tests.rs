use crate::fixtures::test_app::TestApp;
use serde_json::Value;

// ─── Helper ─────────────────────────────────────────────────────

async fn setup_with_invite(app: &TestApp, slug: &str) -> (
    /* admin */ crate::fixtures::seed::SeededUser,
    /* tenant_id */ String,
    /* invite_code */ String,
) {
    let seeded = app.seed_tenant(slug).await;
    let admin = seeded.admin;
    let tenant_id = seeded.tenant_id;

    // Create a shareable invite
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/invite", tenant_id),
            &admin.access_token,
        )
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let invite: Value = resp.json().await.unwrap();
    let code = invite["code"].as_str().unwrap().to_string();

    (admin, tenant_id, code)
}

// ─── Happy Path ─────────────────────────────────────────────────

#[tokio::test]
async fn test_create_shareable_invite() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("inv1").await;

    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/invite", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&serde_json::json!({
            "max_uses": 10,
            "expires_in_hours": 24,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 201);
    let body: Value = resp.json().await.unwrap();
    assert!(!body["code"].as_str().unwrap().is_empty());
    assert_eq!(body["max_uses"].as_u64(), Some(10));
    assert_eq!(body["status"].as_str(), Some("active"));
    assert!(body["target_email"].is_null());
}

#[tokio::test]
async fn test_create_targeted_invite() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("inv2").await;

    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/invite", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&serde_json::json!({
            "target_email": "target@test.local",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["target_email"].as_str(), Some("target@test.local"));
    assert_eq!(body["max_uses"].as_u64(), Some(1)); // forced to 1 for targeted
}

#[tokio::test]
async fn test_accept_shareable_invite() {
    let app = TestApp::spawn().await;
    let (admin, tenant_id, code) = setup_with_invite(&app, "inv3").await;

    // Register a new user
    let new_user = app
        .register_user("newuser@inv3.test", "inv3_new", "New User", "Pass123!", None, None)
        .await;

    // Accept the invite
    let resp = app
        .auth_post(
            &format!("/api/invite/{}/accept", code),
            &new_user.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["tenant_id"].as_str().unwrap(), tenant_id);

    // Verify they're now a member (can list channels)
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/channel", tenant_id),
            &new_user.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
}

#[tokio::test]
async fn test_accept_targeted_invite() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("inv4").await;

    // Create targeted invite
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/invite", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&serde_json::json!({
            "target_email": "target@inv4.test",
        }))
        .send()
        .await
        .unwrap();
    let invite: Value = resp.json().await.unwrap();
    let code = invite["code"].as_str().unwrap();

    // Register the target user
    let target = app
        .register_user("target@inv4.test", "inv4_target", "Target User", "Pass123!", None, None)
        .await;

    // Accept
    let resp = app
        .auth_post(
            &format!("/api/invite/{}/accept", code),
            &target.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn test_accept_invite_with_custom_role() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("inv5").await;

    // Get admin role ID
    use bson::doc;
    let tid = bson::oid::ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let admin_role: bson::Document = app
        .db
        .collection::<bson::Document>("roles")
        .find_one(doc! { "tenant_id": tid, "name": "admin" })
        .await
        .unwrap()
        .unwrap();
    let admin_role_id = admin_role.get_object_id("_id").unwrap().to_hex();

    // Create invite with admin role
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/invite", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&serde_json::json!({
            "assign_role_ids": [admin_role_id],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let invite: Value = resp.json().await.unwrap();
    let code = invite["code"].as_str().unwrap();

    // Register and accept
    let new_user = app
        .register_user("admin2@inv5.test", "inv5_admin2", "Admin 2", "Pass123!", None, None)
        .await;

    let resp = app
        .auth_post(
            &format!("/api/invite/{}/accept", code),
            &new_user.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Verify the user has admin-level permissions (can create invites)
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/invite", seeded.tenant_id),
            &new_user.access_token,
        )
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201); // should succeed with admin role
}

#[tokio::test]
async fn test_list_invites() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("inv6").await;

    // Create multiple invites
    for _ in 0..3 {
        app.auth_post(
            &format!("/api/tenant/{}/invite", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    }

    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/invite", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["total"].as_u64(), Some(3));
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn test_revoke_invite() {
    let app = TestApp::spawn().await;
    let (admin, tenant_id, code) = setup_with_invite(&app, "inv7").await;

    // Get invite ID from list
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/invite", tenant_id),
            &admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let invite_id = body["items"][0]["id"].as_str().unwrap();

    // Revoke
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/invite/{}", tenant_id, invite_id),
            &admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    // Verify status changed
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/invite", tenant_id),
            &admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["items"][0]["status"].as_str(), Some("revoked"));
}

#[tokio::test]
async fn test_get_invite_info_public() {
    let app = TestApp::spawn().await;
    let (_admin, _tenant_id, code) = setup_with_invite(&app, "inv8").await;

    // Unauthenticated request
    let resp = app
        .client
        .get(app.url(&format!("/api/invite/{}", code)))
        .send()
        .await
        .unwrap();

    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["is_valid"].as_bool(), Some(true));
    assert!(!body["tenant_name"].as_str().unwrap().is_empty());
    assert!(!body["inviter_name"].as_str().unwrap().is_empty());
    assert!(body["already_member"].is_null()); // not present for unauth
}

#[tokio::test]
async fn test_get_invite_info_authenticated() {
    let app = TestApp::spawn().await;
    let (admin, _tenant_id, code) = setup_with_invite(&app, "inv9").await;

    // Authenticated request (admin IS a member)
    let resp = app
        .auth_get(
            &format!("/api/invite/{}", code),
            &admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["already_member"].as_bool(), Some(true));
}

#[tokio::test]
async fn test_direct_add_member() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("inv10").await;

    // Register a user NOT in the tenant
    let outsider = app
        .register_user("outsider@inv10.test", "inv10_out", "Outsider", "Pass123!", None, None)
        .await;

    // Admin directly adds them
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/member", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&serde_json::json!({
            "user_id": outsider.id,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 201);

    // Verify they can access channels
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/channel", seeded.tenant_id),
            &outsider.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
}

// ─── Edge Cases ─────────────────────────────────────────────────

#[tokio::test]
async fn test_accept_expired_invite() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("inv11").await;

    // Create invite that expires immediately (0 hours → already expired)
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/invite", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&serde_json::json!({
            "expires_in_hours": 0,
        }))
        .send()
        .await
        .unwrap();
    let invite: Value = resp.json().await.unwrap();
    let code = invite["code"].as_str().unwrap();

    let new_user = app
        .register_user("exp@inv11.test", "inv11_exp", "Expired User", "Pass123!", None, None)
        .await;

    // Small delay to ensure expiry
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let resp = app
        .auth_post(
            &format!("/api/invite/{}/accept", code),
            &new_user.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn test_accept_revoked_invite() {
    let app = TestApp::spawn().await;
    let (admin, tenant_id, code) = setup_with_invite(&app, "inv12").await;

    // Get invite ID and revoke
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/invite", tenant_id),
            &admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let invite_id = body["items"][0]["id"].as_str().unwrap();

    app.auth_delete(
        &format!("/api/tenant/{}/invite/{}", tenant_id, invite_id),
        &admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Try to accept
    let new_user = app
        .register_user("rev@inv12.test", "inv12_rev", "Rev User", "Pass123!", None, None)
        .await;

    let resp = app
        .auth_post(
            &format!("/api/invite/{}/accept", code),
            &new_user.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn test_accept_exhausted_invite() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("inv13").await;

    // Create invite with max_uses=1
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/invite", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&serde_json::json!({ "max_uses": 1 }))
        .send()
        .await
        .unwrap();
    let invite: Value = resp.json().await.unwrap();
    let code = invite["code"].as_str().unwrap();

    // First user accepts
    let user1 = app
        .register_user("u1@inv13.test", "inv13_u1", "User 1", "Pass123!", None, None)
        .await;
    let resp = app
        .auth_post(
            &format!("/api/invite/{}/accept", code),
            &user1.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Second user tries to accept → exhausted
    let user2 = app
        .register_user("u2@inv13.test", "inv13_u2", "User 2", "Pass123!", None, None)
        .await;
    let resp = app
        .auth_post(
            &format!("/api/invite/{}/accept", code),
            &user2.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn test_accept_already_member() {
    let app = TestApp::spawn().await;
    let (admin, _tenant_id, code) = setup_with_invite(&app, "inv14").await;

    // Admin is already a member, try to accept
    let resp = app
        .auth_post(
            &format!("/api/invite/{}/accept", code),
            &admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);
}

#[tokio::test]
async fn test_accept_wrong_target() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("inv15").await;

    // Create targeted invite for specific email
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/invite", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&serde_json::json!({
            "target_email": "specific@inv15.test",
        }))
        .send()
        .await
        .unwrap();
    let invite: Value = resp.json().await.unwrap();
    let code = invite["code"].as_str().unwrap();

    // Wrong email user tries to accept
    let wrong_user = app
        .register_user("wrong@inv15.test", "inv15_wrong", "Wrong User", "Pass123!", None, None)
        .await;

    let resp = app
        .auth_post(
            &format!("/api/invite/{}/accept", code),
            &wrong_user.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn test_concurrent_accept() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("inv16").await;

    // Create invite with max_uses=1
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/invite", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&serde_json::json!({ "max_uses": 1 }))
        .send()
        .await
        .unwrap();
    let invite: Value = resp.json().await.unwrap();
    let code = invite["code"].as_str().unwrap().to_string();

    // Register two users
    let user1 = app
        .register_user("c1@inv16.test", "inv16_c1", "Concurrent 1", "Pass123!", None, None)
        .await;
    let user2 = app
        .register_user("c2@inv16.test", "inv16_c2", "Concurrent 2", "Pass123!", None, None)
        .await;

    // Try to accept concurrently
    let (r1, r2) = tokio::join!(
        app.auth_post(&format!("/api/invite/{}/accept", code), &user1.access_token)
            .send(),
        app.auth_post(&format!("/api/invite/{}/accept", code), &user2.access_token)
            .send(),
    );

    let s1 = r1.unwrap().status().as_u16();
    let s2 = r2.unwrap().status().as_u16();

    // Exactly one should succeed (200) and one should fail (400 or 409)
    let successes = [s1, s2].iter().filter(|&&s| s == 200).count();
    assert_eq!(successes, 1, "Exactly one concurrent accept should succeed, got: {} and {}", s1, s2);
}

// ─── Permission Tests ───────────────────────────────────────────

#[tokio::test]
async fn test_create_invite_no_permission() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("inv17").await;

    // The seeded member has "member" role which does NOT include INVITE_MEMBERS
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/invite", seeded.tenant_id),
            &seeded.member.access_token,
        )
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn test_revoke_invite_no_permission() {
    let app = TestApp::spawn().await;
    let (admin, tenant_id, _code) = setup_with_invite(&app, "inv18").await;

    // Get invite ID
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/invite", tenant_id),
            &admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let invite_id = body["items"][0]["id"].as_str().unwrap();

    // Register a regular user (no invite permission)
    let regular = app
        .register_user("regular@inv18.test", "inv18_reg", "Regular", "Pass123!", None, None)
        .await;

    // Try to revoke (should fail — not even a member of the tenant)
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/invite/{}", tenant_id, invite_id),
            &regular.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

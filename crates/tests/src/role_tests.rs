use crate::fixtures::test_app::TestApp;
use serde_json::Value;

#[tokio::test]
async fn list_default_roles() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("role1").await;

    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/role", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let roles: Vec<Value> = resp.json().await.unwrap();
    // Tenant creation should produce default roles (admin, member at minimum)
    assert!(
        roles.len() >= 2,
        "Expected at least 2 default roles, got {}",
        roles.len()
    );

    let role_names: Vec<&str> = roles.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(role_names.contains(&"admin"), "Expected 'admin' role");
    assert!(role_names.contains(&"member"), "Expected 'member' role");
}

#[tokio::test]
async fn create_custom_role() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("role2").await;

    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/role", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "name": "moderator",
            "description": "Can moderate messages",
            "color": 0xFF5500,
            "permissions": 42,
            "position": 50,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let role: Value = resp.json().await.unwrap();
    assert_eq!(role["name"], "moderator");
    assert_eq!(role["description"], "Can moderate messages");
    assert_eq!(role["color"], 0xFF5500);
    assert_eq!(role["permissions"], 42);
    assert_eq!(role["position"], 50);
    assert_eq!(role["is_default"], false);
    assert_eq!(role["is_managed"], false);
    assert!(role["id"].as_str().is_some(), "Role should have an id");
}

#[tokio::test]
async fn update_role() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("role3").await;

    // Create a role
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/role", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "name": "editor",
        }))
        .send()
        .await
        .unwrap();

    let role: Value = resp.json().await.unwrap();
    let role_id = role["id"].as_str().unwrap();

    // Update it
    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/role/{}", tenant.tenant_id, role_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "name": "senior-editor",
            "description": "Can edit everything",
            "permissions": 99,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["updated"], true);

    // Verify the update via list
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/role", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    let roles: Vec<Value> = resp.json().await.unwrap();
    let updated_role = roles.iter().find(|r| r["id"].as_str() == Some(role_id));
    assert!(updated_role.is_some(), "Updated role should still exist");
    assert_eq!(updated_role.unwrap()["name"], "senior-editor");
}

#[tokio::test]
async fn delete_role() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("role4").await;

    // Create a role
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/role", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "name": "temp-role",
        }))
        .send()
        .await
        .unwrap();

    let role: Value = resp.json().await.unwrap();
    let role_id = role["id"].as_str().unwrap();

    // Delete it
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/role/{}", tenant.tenant_id, role_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["deleted"], true);

    // Verify it's gone from the list
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/role", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    let roles: Vec<Value> = resp.json().await.unwrap();
    let found = roles.iter().any(|r| r["id"].as_str() == Some(role_id));
    assert!(!found, "Deleted role should not appear in list");
}

#[tokio::test]
async fn assign_and_unassign_role_to_user() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("role5").await;

    // Create a custom role
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/role", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "name": "reviewer",
        }))
        .send()
        .await
        .unwrap();

    let role: Value = resp.json().await.unwrap();
    let role_id = role["id"].as_str().unwrap();

    // Assign role to the member user
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/role/{}/assign/{}",
                tenant.tenant_id, role_id, tenant.member.id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["assigned"], true);

    // Unassign the role
    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/role/{}/assign/{}",
                tenant.tenant_id, role_id, tenant.member.id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["removed"], true);
}

#[tokio::test]
async fn non_member_cannot_list_roles() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("role6").await;

    // Register an outsider who is NOT a member of this tenant
    let outsider = app
        .register_user(
            "outsider@role6.test",
            "outsider_role6",
            "Outsider",
            "Outsider123!",
            None,
            None,
        )
        .await;

    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/role", tenant.tenant_id),
            &outsider.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "Non-member should get 403 Forbidden when listing roles"
    );
}

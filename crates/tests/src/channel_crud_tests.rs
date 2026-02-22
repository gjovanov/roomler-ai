use crate::fixtures::test_app::TestApp;
use serde_json::Value;

#[tokio::test]
async fn get_room_by_id() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("chget").await;
    let room = &tenant.rooms[0];

    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}",
                tenant.tenant_id, room.id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["id"], room.id);
    assert_eq!(json["name"], room.name);
    assert_eq!(json["path"], room.path);
}

#[tokio::test]
async fn update_room() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("chupd").await;
    let room = &tenant.rooms[0];

    let resp = app
        .auth_put(
            &format!(
                "/api/tenant/{}/room/{}",
                tenant.tenant_id, room.id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "name": "renamed-general",
            "purpose": "Updated purpose",
            "is_read_only": true,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["updated"], true);

    // Verify the update
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}",
                tenant.tenant_id, room.id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["name"], "renamed-general");
}

#[tokio::test]
async fn delete_room_soft_deletes() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("chdel").await;
    let room = &tenant.rooms[2]; // delete "random"

    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/room/{}",
                tenant.tenant_id, room.id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["deleted"], true);

    // List rooms - should now show 2 instead of 3
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/room", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    let rooms: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(rooms.len(), 2);
}

#[tokio::test]
async fn list_room_members() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("chmem").await;
    let room = &tenant.rooms[0];

    // Admin is already a member from room creation.
    // Member joins room.
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/join",
            tenant.tenant_id, room.id
        ),
        &tenant.member.access_token,
    )
    .send()
    .await
    .unwrap();

    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/member",
                tenant.tenant_id, room.id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    // Admin (creator auto-joined) + member = 2
    assert_eq!(json["total"], 2);
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn explore_rooms() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("chexpl").await;

    // Search for "engineer" - should match "engineering"
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/explore?q=engineer",
                tenant.tenant_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let rooms: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0]["name"], "engineering");
}

#[tokio::test]
async fn explore_rooms_no_match() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("chexpl2").await;

    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/explore?q=nonexistent",
                tenant.tenant_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let rooms: Vec<Value> = resp.json().await.unwrap();
    assert!(rooms.is_empty());
}

#[tokio::test]
async fn cross_tenant_room_get_forbidden() {
    let app = TestApp::spawn().await;
    let tenant_a = app.seed_tenant("chisoa").await;
    let tenant_b = app.seed_tenant("chisob").await;

    // Try to get tenant_a's room using tenant_b's member token
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}",
                tenant_a.tenant_id, tenant_a.rooms[0].id
            ),
            &tenant_b.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 403);
}

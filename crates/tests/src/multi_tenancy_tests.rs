use crate::fixtures::test_app::TestApp;
use serde_json::Value;

#[tokio::test]
async fn tenant_isolation_rooms_not_visible_cross_tenant() {
    let app = TestApp::spawn().await;

    // Seed two tenants
    let acme = app.seed_tenant("acme").await;
    let beta = app.seed_tenant("beta").await;

    // Acme admin lists rooms - sees 3 acme rooms
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/room", acme.tenant_id),
            &acme.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let acme_rooms: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(acme_rooms.len(), 3);

    // Beta admin lists beta rooms - sees 3 beta rooms
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/room", beta.tenant_id),
            &beta.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let beta_rooms: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(beta_rooms.len(), 3);

    // Beta admin tries to access acme tenant - should be forbidden
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/room", acme.tenant_id),
            &beta.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "Cross-tenant access should be forbidden"
    );
}

#[tokio::test]
async fn tenant_isolation_messages_not_visible_cross_tenant() {
    let app = TestApp::spawn().await;

    let acme = app.seed_tenant("acme2").await;
    let beta = app.seed_tenant("beta2").await;

    let acme_room_id = &acme.rooms[0].id;

    // Acme admin joins room and posts a message
    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/join",
            acme.tenant_id, acme_room_id
        ),
        &acme.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/message",
            acme.tenant_id, acme_room_id
        ),
        &acme.admin.access_token,
    )
    .json(&serde_json::json!({
        "content": "Secret acme message",
    }))
    .send()
    .await
    .unwrap();

    // Beta admin cannot list acme's messages
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message",
                acme.tenant_id, acme_room_id
            ),
            &beta.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    // Should be forbidden (not a member of acme tenant)
    assert_eq!(
        resp.status().as_u16(),
        403,
        "Cross-tenant message access should be forbidden"
    );
}

#[tokio::test]
async fn tenant_isolation_get_tenant_details_cross_tenant() {
    let app = TestApp::spawn().await;

    let acme = app.seed_tenant("acme3").await;
    let beta = app.seed_tenant("beta3").await;

    // Acme admin can get acme tenant details
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}", acme.tenant_id),
            &acme.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Beta admin cannot get acme tenant details
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}", acme.tenant_id),
            &beta.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "Cross-tenant tenant detail access should be forbidden"
    );
}

#[tokio::test]
async fn tenant_list_only_shows_user_tenants() {
    let app = TestApp::spawn().await;

    let acme = app.seed_tenant("acme4").await;
    let _beta = app.seed_tenant("beta4").await;

    // Acme admin lists tenants - should only see acme
    let resp = app
        .auth_get("/api/tenant", &acme.admin.access_token)
        .send()
        .await
        .unwrap();
    let tenants: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(tenants.len(), 1);
    assert_eq!(tenants[0]["slug"], "acme4");
}

#[tokio::test]
async fn unauthenticated_request_gets_401() {
    let app = TestApp::spawn().await;

    let resp = app
        .client
        .get(app.url("/api/tenant"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn cannot_create_room_in_foreign_tenant() {
    let app = TestApp::spawn().await;

    let acme = app.seed_tenant("acme5").await;
    let beta = app.seed_tenant("beta5").await;

    // Beta admin tries to create room in acme's tenant
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room", acme.tenant_id),
            &beta.admin.access_token,
        )
        .json(&serde_json::json!({
            "name": "infiltrator",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        403,
        "Creating room in foreign tenant should be forbidden"
    );
}

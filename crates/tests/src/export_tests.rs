use crate::fixtures::test_app::TestApp;
use serde_json::Value;

#[tokio::test]
async fn export_conversation_creates_background_task() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("export1").await;
    let room_id = tenant.rooms[0].id.clone();

    // Admin joins room and creates messages
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

    for i in 1..=3 {
        app.auth_post(
            &format!(
                "/api/tenant/{}/room/{}/message",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "content": format!("Export message {}", i),
        }))
        .send()
        .await
        .unwrap();
    }

    // Trigger export
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/export/conversation", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "room_id": room_id,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["status"], "pending");
    assert!(json["task_id"].as_str().unwrap().len() > 0);
}

#[tokio::test]
async fn export_task_completes_and_download_works() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("export2").await;
    let room_id = tenant.rooms[0].id.clone();

    // Admin joins room and creates a message
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
            "/api/tenant/{}/room/{}/message",
            tenant.tenant_id, room_id
        ),
        &tenant.admin.access_token,
    )
    .json(&serde_json::json!({
        "content": "Message for export test",
    }))
    .send()
    .await
    .unwrap();

    // Trigger export
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/export/conversation", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "room_id": room_id,
        }))
        .send()
        .await
        .unwrap();

    let json: Value = resp.json().await.unwrap();
    let task_id = json["task_id"].as_str().unwrap().to_string();

    // Poll for task completion (background task runs async)
    let mut completed = false;
    for _ in 0..20 {
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;

        let resp = app
            .auth_get(
                &format!("/api/tenant/{}/task/{}", tenant.tenant_id, task_id),
                &tenant.admin.access_token,
            )
            .send()
            .await
            .unwrap();

        let json: Value = resp.json().await.unwrap();
        let status = json["status"].as_str().unwrap();
        if status == "Completed" {
            completed = true;
            assert_eq!(json["progress"], 100);
            assert!(json["file_name"]
                .as_str()
                .unwrap()
                .ends_with(".xlsx"));
            break;
        } else if status == "Failed" {
            panic!("Export task failed: {:?}", json["error"]);
        }
    }
    assert!(completed, "Export task did not complete within timeout");

    // Download the export file
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/task/{}/download",
                tenant.tenant_id, task_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("spreadsheetml"));

    let body = resp.bytes().await.unwrap();
    // XLSX files start with PK zip signature
    assert!(body.len() > 0);
    assert_eq!(body[0], 0x50); // 'P'
    assert_eq!(body[1], 0x4B); // 'K'
}

#[tokio::test]
async fn list_background_tasks() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("tasklist").await;
    let room_id = tenant.rooms[0].id.clone();

    // Create a message so export has something
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
            "/api/tenant/{}/room/{}/message",
            tenant.tenant_id, room_id
        ),
        &tenant.admin.access_token,
    )
    .json(&serde_json::json!({ "content": "test" }))
    .send()
    .await
    .unwrap();

    // Create 2 export tasks
    for _ in 0..2 {
        app.auth_post(
            &format!("/api/tenant/{}/export/conversation", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "room_id": room_id }))
        .send()
        .await
        .unwrap();
    }

    // List tasks
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/task", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"], 2);
}

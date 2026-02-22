use crate::fixtures::test_app::TestApp;
use serde_json::Value;

#[tokio::test]
async fn export_conversation_as_pdf() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("pdfexp").await;
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

    for i in 1..=2 {
        app.auth_post(
            &format!(
                "/api/tenant/{}/room/{}/message",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "content": format!("PDF export message {}", i),
        }))
        .send()
        .await
        .unwrap();
    }

    // Trigger PDF export
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/export/conversation-pdf", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "room_id": room_id }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["status"], "pending");
    let task_id = json["task_id"].as_str().unwrap().to_string();

    // Poll for completion
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
            assert!(json["file_name"].as_str().unwrap().ends_with(".pdf"));
            break;
        } else if status == "Failed" {
            panic!("PDF export failed: {:?}", json["error"]);
        }
    }
    assert!(completed, "PDF export did not complete within timeout");

    // Download
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
        .contains("pdf"));

    let body = resp.bytes().await.unwrap();
    // PDF files start with %PDF
    assert!(body.len() > 0);
    assert_eq!(&body[0..5], b"%PDF-");
}

#[tokio::test]
async fn recognize_file_returns_error_without_api_key() {
    let app = TestApp::spawn_with_settings(|s| {
        s.claude.api_key = None;
    })
    .await;
    let tenant = app.seed_tenant("recog1").await;
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

    // Upload a file
    let file_part = reqwest::multipart::Part::bytes(b"fake image content".to_vec())
        .file_name("test.png")
        .mime_str("image/png")
        .unwrap();

    let form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("room_id", room_id.clone());

    let resp = app
        .client
        .post(app.url(&format!(
            "/api/tenant/{}/file/upload",
            tenant.tenant_id
        )))
        .header(
            "Authorization",
            format!("Bearer {}", tenant.admin.access_token),
        )
        .multipart(form)
        .send()
        .await
        .unwrap();

    let upload_json: Value = resp.json().await.unwrap();
    let file_id = upload_json["id"].as_str().unwrap();

    // Try to recognize - should fail since no API key is configured in tests
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/file/{}/recognize",
                tenant.tenant_id, file_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    // Should return 400 because Claude API key is not set
    assert_eq!(resp.status().as_u16(), 400);
    let json: Value = resp.json().await.unwrap();
    assert!(json["message"]
        .as_str()
        .unwrap()
        .contains("not configured"));
}

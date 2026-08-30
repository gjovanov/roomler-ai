// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use crate::fixtures::test_app::TestApp;
use serde_json::Value;

#[tokio::test]
async fn export_conversation_as_pdf() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("pdfexp").await;
    let room_id = tenant.rooms[0].id.clone();

    // Admin joins room and creates messages
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    for i in 1..=2 {
        app.auth_post(
            &format!("/api/tenant/{}/room/{}/message", tenant.tenant_id, room_id),
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
            &format!("/api/tenant/{}/task/{}/download", tenant.tenant_id, task_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("pdf")
    );

    let body = resp.bytes().await.unwrap();
    // PDF files start with %PDF
    assert!(!body.is_empty());
    assert_eq!(&body[0..5], b"%PDF-");
}

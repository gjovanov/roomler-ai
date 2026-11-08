// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use bson::{doc, oid::ObjectId};
use serde_json::{Value, json};

/// `POST …/export/conversation-pdf` naming `room_id`; the status and body.
async fn export(app: &TestApp, tenant_id: &str, room_id: &str, token: &str) -> (u16, Value) {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{tenant_id}/export/conversation-pdf"),
            token,
        )
        .json(&json!({ "room_id": room_id }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// How many background tasks `user_id` holds.
async fn tasks_of(app: &TestApp, user_id: &str) -> u64 {
    app.db
        .collection::<bson::Document>("background_tasks")
        .count_documents(doc! { "user_id": ObjectId::parse_str(user_id).unwrap() })
        .await
        .unwrap()
}

/// A room by the org's admin with the given visibility.
async fn room_with(app: &TestApp, t: &SeededTenant, name: &str, visibility: &str) -> String {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "name": name }))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let id = body["id"].as_str().expect("a room id").to_string();
    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/room/{id}", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "visibility": visibility }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "visibility {visibility}");
    id
}

#[tokio::test]
async fn a_pdf_export_reads_only_a_room_of_its_own_org() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("pdfowna").await;
    let b = app.seed_tenant("pdfownb").await;

    let (status, body) = export(&app, &b.tenant_id, &a.rooms[0].id, &b.admin.access_token).await;
    assert_eq!(status, 404, "another org's room: {body}");
    assert_eq!(tasks_of(&app, &b.admin.id).await, 0, "no task is created");

    let never = ObjectId::new().to_hex();
    let missing = export(&app, &b.tenant_id, &never, &b.admin.access_token).await;
    assert_eq!(
        (status, body),
        missing,
        "the answer to a room that does not exist"
    );
}

#[tokio::test]
async fn a_pdf_export_follows_the_rooms_visibility() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("pdfvis").await;
    let private = room_with(&app, &t, "pdf private", "private").await;
    let secret = room_with(&app, &t, "pdf secret", "secret").await;

    for (room, want) in [(&secret, 404), (&private, 403)] {
        let (status, body) = export(&app, &t.tenant_id, room, &t.member.access_token).await;
        assert_eq!(status, want, "a member outside the room: {body}");
    }
    assert_eq!(tasks_of(&app, &t.member.id).await, 0, "no task is created");

    let (status, body) = export(&app, &t.tenant_id, &t.rooms[0].id, &t.member.access_token).await;
    assert_eq!(status, 200, "an open room: {body}");
}

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

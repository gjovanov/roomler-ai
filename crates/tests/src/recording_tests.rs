// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use crate::fixtures::test_app::TestApp;
use serde_json::Value;

#[tokio::test]
async fn create_recording_for_room() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("rec1").await;

    // Create room and start call
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "name": "Recording Test" }))
        .send()
        .await
        .unwrap();
    let room: Value = resp.json().await.unwrap();
    let room_id = room["id"].as_str().unwrap();

    app.auth_post(
        &format!(
            "/api/tenant/{}/room/{}/call/start",
            tenant.tenant_id, room_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Create recording
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/recording",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "recording_type": "video" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["room_id"], room_id);
    assert_eq!(json["recording_type"], "Video");
    assert_eq!(json["status"], "Processing");
}

#[tokio::test]
async fn list_recordings_for_room() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("rec2").await;

    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "name": "Recording List" }))
        .send()
        .await
        .unwrap();
    let room: Value = resp.json().await.unwrap();
    let room_id = room["id"].as_str().unwrap();

    // Create 2 recordings
    for rec_type in &["video", "audio"] {
        app.auth_post(
            &format!(
                "/api/tenant/{}/room/{}/recording",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "recording_type": rec_type }))
        .send()
        .await
        .unwrap();
    }

    // List recordings
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/recording",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"], 2);
}

#[tokio::test]
async fn delete_recording() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("rec3").await;

    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "name": "Delete Rec" }))
        .send()
        .await
        .unwrap();
    let room: Value = resp.json().await.unwrap();
    let room_id = room["id"].as_str().unwrap();

    // Create recording
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/recording",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    let rec: Value = resp.json().await.unwrap();
    let rec_id = rec["id"].as_str().unwrap();

    // Delete it
    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/room/{}/recording/{}",
                tenant.tenant_id, room_id, rec_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["deleted"], true);
}

/// A room of the org made by its admin, with the given visibility.
async fn room_with(
    app: &TestApp,
    t: &crate::fixtures::seed::SeededTenant,
    name: &str,
    visibility: &str,
) -> String {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .unwrap();
    let room: Value = resp.json().await.unwrap();
    let id = room["id"].as_str().expect("a room id").to_string();
    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/room/{id}", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&serde_json::json!({ "visibility": visibility }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "visibility {visibility}");
    id
}

/// A recording of `room` started by `token`'s account.
async fn recording_in(app: &TestApp, tenant_id: &str, room: &str, token: &str) -> String {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{tenant_id}/room/{room}/recording"),
            token,
        )
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "a recording is started");
    let rec: Value = resp.json().await.unwrap();
    rec["id"].as_str().unwrap().to_string()
}

async fn delete_status(app: &TestApp, tenant_id: &str, room: &str, rec: &str, token: &str) -> u16 {
    app.auth_delete(
        &format!("/api/tenant/{tenant_id}/room/{room}/recording/{rec}"),
        token,
    )
    .send()
    .await
    .unwrap()
    .status()
    .as_u16()
}

/// Whether the recording is still live in the database.
async fn live(app: &TestApp, rec: &str) -> bool {
    let row = app
        .db
        .collection::<bson::Document>("recordings")
        .find_one(bson::doc! { "_id": bson::oid::ObjectId::parse_str(rec).unwrap() })
        .await
        .unwrap()
        .expect("the recording row");
    matches!(row.get("deleted_at"), None | Some(bson::Bson::Null))
}

#[tokio::test]
async fn a_recording_resolves_in_its_own_room_of_its_own_org() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("recowna").await;
    let b = app.seed_tenant("recownb").await;
    let (tid, admin) = (a.tenant_id.clone(), a.admin.access_token.clone());
    let open = a.rooms[0].id.clone();
    let private = room_with(&app, &a, "rec private", "private").await;
    let secret = room_with(&app, &a, "rec secret", "secret").await;
    let in_open = recording_in(&app, &tid, &open, &admin).await;
    let in_private = recording_in(&app, &tid, &private, &admin).await;
    let in_secret = recording_in(&app, &tid, &secret, &admin).await;

    // Another org's admin, in their own org and room.
    let status = delete_status(
        &app,
        &b.tenant_id,
        &b.rooms[0].id,
        &in_open,
        &b.admin.access_token,
    )
    .await;
    assert_eq!(status, 404, "another org's recording");
    // The org's admin, through another room of the org.
    let status = delete_status(&app, &tid, &a.rooms[1].id, &in_open, &admin).await;
    assert_eq!(status, 404, "a recording of another room");
    // A member outside the Secret and the Private room.
    let member = a.member.access_token.clone();
    let status = delete_status(&app, &tid, &secret, &in_secret, &member).await;
    assert_eq!(status, 404, "a Secret room's recording");
    let status = delete_status(&app, &tid, &private, &in_private, &member).await;
    assert_eq!(status, 403, "a Private room's recording");
    for rec in [&in_open, &in_private, &in_secret] {
        assert!(live(&app, rec).await, "a refused delete removes nothing");
    }

    // A deleted recording answers as a missing one.
    assert_eq!(
        delete_status(&app, &tid, &open, &in_open, &admin).await,
        200
    );
    assert_eq!(
        delete_status(&app, &tid, &open, &in_open, &admin).await,
        404
    );
}

#[tokio::test]
async fn a_recording_is_deleted_by_its_starter_or_a_room_admin() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("recwho").await;
    let (tid, room) = (a.tenant_id.clone(), a.rooms[0].id.clone());
    let (admin, member) = (a.admin.access_token.clone(), a.member.access_token.clone());
    let by_admin = recording_in(&app, &tid, &room, &admin).await;
    let by_member = recording_in(&app, &tid, &room, &member).await;
    let by_member_too = recording_in(&app, &tid, &room, &member).await;

    // A member who did not start it, without MANAGE_CHANNELS.
    assert_eq!(
        delete_status(&app, &tid, &room, &by_admin, &member).await,
        403
    );
    assert!(live(&app, &by_admin).await, "it stays");
    // Its starter.
    assert_eq!(
        delete_status(&app, &tid, &room, &by_member, &member).await,
        200
    );
    assert!(!live(&app, &by_member).await);
    // Whoever may delete the room (MANAGE_CHANNELS) may delete its recordings.
    assert_eq!(
        delete_status(&app, &tid, &room, &by_member_too, &admin).await,
        200
    );
    assert!(!live(&app, &by_member_too).await);
}

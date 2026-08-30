// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use crate::fixtures::test_app::TestApp;
use reqwest::multipart;
use serde_json::Value;

#[tokio::test]
async fn upload_file_to_room() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("fileup").await;
    let room_id = tenant.rooms[0].id.clone();

    // Admin joins room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Upload a file via multipart
    let file_part = multipart::Part::bytes(b"Hello, World!".to_vec())
        .file_name("test.txt")
        .mime_str("text/plain")
        .unwrap();

    let form = multipart::Form::new()
        .part("file", file_part)
        .text("room_id", room_id.clone());

    let resp = app
        .client
        .post(app.url(&format!("/api/tenant/{}/file/upload", tenant.tenant_id)))
        .header(
            "Authorization",
            format!("Bearer {}", tenant.admin.access_token),
        )
        .multipart(form)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["filename"], "test.txt");
    assert_eq!(json["content_type"], "text/plain");
    assert_eq!(json["size"], 13); // "Hello, World!" = 13 bytes
    assert!(!json["id"].as_str().unwrap().is_empty());
    assert!(!json["url"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn get_file_metadata() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("fileget").await;
    let room_id = tenant.rooms[0].id.clone();

    // Admin joins room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Upload a file whose BYTES really are a PDF. They used to be the string
    // "file content here", and this test passed only because the server stored
    // whatever content-type the client claimed — i.e. it asserted that the
    // server believed a lie. The upload path now sniffs, so the fixture has to
    // be honest for `application/pdf` to be the right answer.
    let file_part =
        multipart::Part::bytes(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\nfile content here".to_vec())
            .file_name("document.pdf")
            .mime_str("application/pdf")
            .unwrap();

    let form = multipart::Form::new()
        .part("file", file_part)
        .text("room_id", room_id.clone());

    let resp = app
        .client
        .post(app.url(&format!("/api/tenant/{}/file/upload", tenant.tenant_id)))
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

    // Get file metadata
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/file/{}", tenant.tenant_id, file_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["id"], file_id);
    assert_eq!(json["filename"], "document.pdf");
    assert_eq!(json["content_type"], "application/pdf");
}

#[tokio::test]
async fn download_uploaded_file() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("filedl").await;
    let room_id = tenant.rooms[0].id.clone();

    // Admin joins room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    let content = b"Download me please!";

    // Upload file
    let file_part = multipart::Part::bytes(content.to_vec())
        .file_name("download_me.txt")
        .mime_str("text/plain")
        .unwrap();

    let form = multipart::Form::new()
        .part("file", file_part)
        .text("room_id", room_id.clone());

    let resp = app
        .client
        .post(app.url(&format!("/api/tenant/{}/file/upload", tenant.tenant_id)))
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

    // Download file
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/file/{}/download", tenant.tenant_id, file_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/plain"
    );
    assert!(
        resp.headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("download_me.txt")
    );

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), content);
}

#[tokio::test]
async fn delete_file() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("filedel").await;
    let room_id = tenant.rooms[0].id.clone();

    // Admin joins room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Upload file
    let file_part = multipart::Part::bytes(b"to be deleted".to_vec())
        .file_name("delete_me.txt")
        .mime_str("text/plain")
        .unwrap();

    let form = multipart::Form::new()
        .part("file", file_part)
        .text("room_id", room_id.clone());

    let resp = app
        .client
        .post(app.url(&format!("/api/tenant/{}/file/upload", tenant.tenant_id)))
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

    // Delete file
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/file/{}", tenant.tenant_id, file_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["deleted"], true);
}

#[tokio::test]
async fn list_files_in_room() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("filelist").await;
    let room_id = tenant.rooms[0].id.clone();

    // Admin joins room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Upload 2 files
    for (name, content) in &[("a.txt", "aaa"), ("b.txt", "bbb")] {
        let file_part = multipart::Part::bytes(content.as_bytes().to_vec())
            .file_name(name.to_string())
            .mime_str("text/plain")
            .unwrap();

        let form = multipart::Form::new()
            .part("file", file_part)
            .text("room_id", room_id.clone());

        app.client
            .post(app.url(&format!("/api/tenant/{}/file/upload", tenant.tenant_id)))
            .header(
                "Authorization",
                format!("Bearer {}", tenant.admin.access_token),
            )
            .multipart(form)
            .send()
            .await
            .unwrap();
    }

    // List files in room
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/room/{}/file", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"], 2);
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
}

/// The stored content-type must come from the BYTES, not from what the
/// uploader claimed — end to end, through the real multipart handler.
///
/// The unit tests in `api::media_type` prove the resolver; this proves the
/// upload route actually calls it. Those are different failures: a resolver
/// that works and is never invoked looks identical to no fix at all.
#[tokio::test]
async fn upload_stores_the_sniffed_type_not_the_claimed_one() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("filesniff").await;
    let room_id = tenant.rooms[0].id.clone();

    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // An HTML body, named and declared as a PNG — the shape that mattered,
    // because the chat bubble renders <v-img> for anything typed `image/*`.
    let file_part = multipart::Part::bytes(b"<html><body>not an image</body></html>".to_vec())
        .file_name("totally-a-picture.png")
        .mime_str("image/png")
        .unwrap();
    let form = multipart::Form::new()
        .part("file", file_part)
        .text("room_id", room_id.clone());

    let resp = app
        .client
        .post(app.url(&format!("/api/tenant/{}/file/upload", tenant.tenant_id)))
        .header(
            "Authorization",
            format!("Bearer {}", tenant.admin.access_token),
        )
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let json: Value = resp.json().await.unwrap();
    let stored = json["content_type"].as_str().unwrap_or_default();
    assert!(
        !stored.starts_with("image/"),
        "an HTML body claimed as image/png was stored as {stored:?} — the client's \
         claim is being trusted, and the chat renderer draws <v-img> on `image/*`"
    );
    assert_eq!(
        stored, "text/html",
        "the sniffer recognises HTML by content, so that is the honest answer"
    );
}

// ─── FR-11 (#784): file grids — q search + whitelisted sort ──────────────

/// Uploads three named files, then exercises the tenant-wide and room list
/// endpoints' new `q`/`sort`/`dir` params (server-side grid feed).
#[tokio::test]
async fn file_lists_support_q_and_sort() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("filegrid").await;
    let room_id = tenant.rooms[0].id.clone();

    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // (name, body) — sizes deliberately distinct for the size sort.
    for (name, body) in [
        ("alpha-report.txt", "a".repeat(10)),
        ("beta-notes.txt", "b".repeat(30)),
        ("gamma-summary.txt", "c".repeat(20)),
    ] {
        let file_part = multipart::Part::bytes(body.into_bytes())
            .file_name(name.to_string())
            .mime_str("text/plain")
            .unwrap();
        let form = multipart::Form::new()
            .part("file", file_part)
            .text("room_id", room_id.clone());
        let resp = app
            .client
            .post(app.url(&format!("/api/tenant/{}/file/upload", tenant.tenant_id)))
            .header(
                "Authorization",
                format!("Bearer {}", tenant.admin.access_token),
            )
            .multipart(form)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "upload {name} failed");
    }

    // Tenant-wide list: q narrows by filename substring, case-insensitively.
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/file?q=BETA", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"].as_u64(), Some(1));
    assert_eq!(json["items"][0]["filename"], "beta-notes.txt");

    // sort=filename asc → alpha, beta, gamma.
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/file?sort=filename&dir=asc",
                tenant.tenant_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    let names: Vec<&str> = json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["filename"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["alpha-report.txt", "beta-notes.txt", "gamma-summary.txt"]
    );

    // sort=size desc → 30, 20, 10.
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/file?sort=size&dir=desc", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    let sizes: Vec<u64> = json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["size"].as_u64().unwrap())
        .collect();
    assert_eq!(sizes, vec![30, 20, 10]);

    // Room-scoped list takes the same params.
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/file?q=gamma&sort=filename",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"].as_u64(), Some(1));
    assert_eq!(json["items"][0]["filename"], "gamma-summary.txt");

    // No params → unchanged default (created_at desc), full envelope.
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/file", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"].as_u64(), Some(3));
    assert_eq!(json["items"][0]["filename"], "gamma-summary.txt");

    // Unknown sort key must 400.
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/file?sort=evil", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

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
        &format!(
            "/api/tenant/{}/room/{}/join",
            tenant.tenant_id, room_id
        ),
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

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["filename"], "test.txt");
    assert_eq!(json["content_type"], "text/plain");
    assert_eq!(json["size"], 13); // "Hello, World!" = 13 bytes
    assert!(json["id"].as_str().unwrap().len() > 0);
    assert!(json["url"].as_str().unwrap().len() > 0);
}

#[tokio::test]
async fn get_file_metadata() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("fileget").await;
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
    let file_part = multipart::Part::bytes(b"file content here".to_vec())
        .file_name("document.pdf")
        .mime_str("application/pdf")
        .unwrap();

    let form = multipart::Form::new()
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

    // Get file metadata
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/file/{}",
                tenant.tenant_id, file_id
            ),
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
        &format!(
            "/api/tenant/{}/room/{}/join",
            tenant.tenant_id, room_id
        ),
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

    // Download file
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/file/{}/download",
                tenant.tenant_id, file_id
            ),
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
    assert!(resp
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("download_me.txt"));

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
        &format!(
            "/api/tenant/{}/room/{}/join",
            tenant.tenant_id, room_id
        ),
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

    // Delete file
    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/file/{}",
                tenant.tenant_id, file_id
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

#[tokio::test]
async fn list_files_in_room() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("filelist").await;
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
    }

    // List files in room
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/file",
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
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
}

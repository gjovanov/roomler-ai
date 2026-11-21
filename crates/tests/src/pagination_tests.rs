// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use crate::fixtures::test_app::TestApp;
use bson::{DateTime, oid::ObjectId};
use roomler_ai_remote_control::models::{OverlayPolicy, TunnelPolicy};
use serde_json::Value;

/// Helper: join room and create N messages, returning all message IDs in creation order.
async fn seed_messages(
    app: &TestApp,
    tenant_id: &str,
    room_id: &str,
    token: &str,
    count: usize,
) -> Vec<String> {
    // Join room
    app.auth_post(
        &format!("/api/tenant/{}/room/{}/join", tenant_id, room_id),
        token,
    )
    .send()
    .await
    .unwrap();

    let mut ids = Vec::new();
    for i in 1..=count {
        let resp = app
            .auth_post(
                &format!("/api/tenant/{}/room/{}/message", tenant_id, room_id),
                token,
            )
            .json(&serde_json::json!({
                "content": format!("Pagination test message {}", i),
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            200,
            "Failed to create message {}",
            i
        );
        let json: Value = resp.json().await.unwrap();
        ids.push(json["id"].as_str().unwrap().to_string());
    }
    ids
}

#[tokio::test]
async fn paginate_messages_three_pages() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("page1").await;
    let room_id = &tenant.rooms[0].id;

    seed_messages(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        30,
    )
    .await;

    // Fetch page 1 with per_page=10
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=1&per_page=10",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"], 30);
    assert_eq!(json["per_page"], 10);
    assert_eq!(json["total_pages"], 3);
    assert_eq!(json["items"].as_array().unwrap().len(), 10);

    // Fetch page 3
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=3&per_page=10",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["items"].as_array().unwrap().len(), 10);
    assert_eq!(json["page"], 3);
}

#[tokio::test]
async fn per_page_is_clamped_to_100() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("page2").await;
    let room_id = &tenant.rooms[0].id;

    // Create a few messages (we just need the pagination metadata)
    seed_messages(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        5,
    )
    .await;

    // Request with per_page=1000
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=1&per_page=1000",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    // per_page in the response should be clamped to 100
    assert_eq!(
        json["per_page"], 100,
        "per_page should be clamped to 100, got {}",
        json["per_page"]
    );
}

#[tokio::test]
async fn cursor_pagination_with_before() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("page3").await;
    let room_id = &tenant.rooms[0].id;

    seed_messages(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        10,
    )
    .await;

    // Fetch all messages first to get a timestamp for the "before" cursor
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=1&per_page=100",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    let json: Value = resp.json().await.unwrap();
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 10);

    // Messages are sorted newest first. Take created_at of the 5th message (index 4)
    // to use as the "before" cursor — should return only older messages.
    let before_ts = items[4]["created_at"].as_str().unwrap();

    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=1&per_page=100&before={}",
                tenant.tenant_id, room_id, before_ts
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    let filtered_items = json["items"].as_array().unwrap();

    // Should return fewer items than the full 10 (only those created before the cursor)
    assert!(
        filtered_items.len() < 10,
        "Expected fewer than 10 items with 'before' cursor, got {}",
        filtered_items.len()
    );
}

#[tokio::test]
async fn total_count_and_total_pages_correct() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("page4").await;
    let room_id = &tenant.rooms[0].id;

    seed_messages(
        &app,
        &tenant.tenant_id,
        room_id,
        &tenant.admin.access_token,
        17,
    )
    .await;

    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=1&per_page=5",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"], 17);
    assert_eq!(json["per_page"], 5);
    // ceil(17/5) = 4
    assert_eq!(json["total_pages"], 4);
    assert_eq!(json["items"].as_array().unwrap().len(), 5);

    // Page 4 should have 2 items (17 - 15 = 2)
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/room/{}/message?page=4&per_page=5",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["items"].as_array().unwrap().len(), 2);
}

/// One GET with a bearer token: the status and the body as JSON. A request
/// that gets no answer reads as status 0, with the error as its body.
async fn get_json(app: &TestApp, path: &str, token: &str) -> (u16, Value) {
    match app.auth_get(path, token).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            (status, resp.json().await.unwrap_or(Value::Null))
        }
        Err(e) => (0, Value::String(e.to_string())),
    }
}

/// The `id` of every item on one page, in order.
fn item_ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| item["id"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// A paged list has one stable order: rows that share a `created_at` land on
/// exactly one page each, newest first and then by `_id` (#4215).
///
/// Seven policies share ONE `created_at` and are read three to a page from
/// the two lists #4215 names. They are inserted out of `_id` order, so the
/// order the collection stores them in is not the order asserted.
#[tokio::test]
async fn rows_sharing_a_created_at_land_on_exactly_one_page() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("pageorder").await;
    let tid = ObjectId::parse_str(&tenant.tenant_id).unwrap();
    let token = &tenant.admin.access_token;

    let mut ids: Vec<ObjectId> = (0..7).map(|_| ObjectId::new()).collect();
    ids.sort();
    let created_at = DateTime::now();
    let tunnel = app.db.collection::<TunnelPolicy>(TunnelPolicy::COLLECTION);
    let overlay = app
        .db
        .collection::<OverlayPolicy>(OverlayPolicy::COLLECTION);
    for i in [3, 0, 6, 1, 5, 2, 4] {
        tunnel
            .insert_one(TunnelPolicy {
                id: Some(ids[i]),
                tenant_id: tid,
                name: format!("shared-created-at-{i}"),
                subjects: Vec::new(),
                targets: Vec::new(),
                allowlist: Vec::new(),
                max_concurrent_flows: None,
                max_bytes_per_session: None,
                created_at,
                updated_at: created_at,
                deleted_at: None,
            })
            .await
            .unwrap();
        overlay
            .insert_one(OverlayPolicy {
                id: Some(ids[i]),
                tenant_id: tid,
                name: format!("shared-created-at-{i}"),
                enabled: true,
                sources: Vec::new(),
                via: Vec::new(),
                destinations: Vec::new(),
                created_at,
                updated_at: created_at,
                deleted_at: None,
            })
            .await
            .unwrap();
    }
    // Newest first, then `_id` in the same direction.
    let want: Vec<String> = ids.iter().rev().map(|id| id.to_hex()).collect();

    for list in ["tunnel-policy", "overlay-acl"] {
        let mut pages = Vec::new();
        for page in 1..=3 {
            let path = format!(
                "/api/tenant/{}/{list}?page={page}&per_page=3",
                tenant.tenant_id
            );
            let (status, body) = get_json(&app, &path, token).await;
            assert_eq!(status, 200, "{list} page {page}: {body}");
            assert_eq!(body["total"], 7, "{list} page {page}: {body}");
            pages.push(item_ids(&body));
        }
        assert_eq!(
            pages.concat(),
            want,
            "{list}: each row once, newest first then by _id; the pages read: {pages:?}"
        );
    }
}

/// The page parameters are kept within their range: a request whose `page`
/// or `per_page` is outside it reads a bounded first page, the same rows as
/// the in-range request it is read as.
#[tokio::test]
async fn page_parameters_outside_their_range_read_a_bounded_first_page() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("pagerange").await;
    let room_id = &tenant.rooms[0].id;
    let token = &tenant.admin.access_token;
    seed_messages(&app, &tenant.tenant_id, room_id, token, 5).await;
    let list = format!("/api/tenant/{}/room/{}/message", tenant.tenant_id, room_id);

    // (the query sent, the `per_page` it is read with)
    let cases = [
        ("page=0&per_page=2", 2u64),
        ("page=1&per_page=0", 1),
        ("page=0&per_page=0", 1),
        ("page=1&per_page=1000", 100),
        ("page=0&per_page=1000", 100),
    ];
    let mut wrong = Vec::new();
    for (query, per_page) in cases {
        let in_range = format!("{list}?page=1&per_page={per_page}");
        let (status, first) = get_json(&app, &in_range, token).await;
        assert_eq!(status, 200, "{in_range}: {first}");
        assert_eq!(item_ids(&first).len() as u64, per_page.min(5), "{in_range}");

        let (status, body) = get_json(&app, &format!("{list}?{query}"), token).await;
        let read = (
            status,
            body["page"].clone(),
            body["per_page"].clone(),
            body["total"].clone(),
            body["total_pages"].clone(),
            item_ids(&body),
        );
        let want = (
            200,
            Value::from(1),
            Value::from(per_page),
            Value::from(5),
            Value::from(5u64.div_ceil(per_page)),
            item_ids(&first),
        );
        if read != want {
            wrong.push(format!("{query}: read {read:?}, want {want:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

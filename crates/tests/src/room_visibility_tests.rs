// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Room-level read authorization (`RoomVisibility`).
//!
//! Before this there was none: every member of a tenant could read every room
//! in it, while the sidebar drew a padlock on any room with `is_open: false` —
//! which is most of them, since API-created rooms default to that. A padlock
//! on a room the whole org can read is worse than no padlock, and these tests
//! exist so it stays true rather than becoming decorative again.
//!
//! The default is `Public`, so the first test here is the one that matters
//! operationally: nothing changed for existing rooms.

use crate::fixtures::test_app::TestApp;
use serde_json::{Value, json};

/// Create a room as the tenant admin, returning its id.
async fn make_room(app: &TestApp, t: &crate::fixtures::seed::SeededTenant, name: &str) -> String {
    let raw = app
        .auth_post(
            &format!("/api/tenant/{}/room", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "name": name }))
        .send()
        .await
        .unwrap();
    // Assert on the status and show the body: a bare `["id"].expect("room id")`
    // reports "room id" for every possible failure, which says nothing about
    // whether the create was refused, throttled, or malformed.
    let status = raw.status();
    let body = raw.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "creating room {name:?} failed: {status} — {body}"
    );
    let resp: Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("room create returned non-JSON ({e}): {body}"));
    resp["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no room id in create response: {body}"))
        .to_string()
}

async fn set_visibility(
    app: &TestApp,
    t: &crate::fixtures::seed::SeededTenant,
    room_id: &str,
    visibility: &str,
) {
    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/room/{}", t.tenant_id, room_id),
            &t.admin.access_token,
        )
        .json(&json!({ "visibility": visibility }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "setting visibility={visibility} failed: {}",
        resp.status()
    );
}

async fn get_room_status(
    app: &TestApp,
    t: &crate::fixtures::seed::SeededTenant,
    room_id: &str,
    token: &str,
) -> u16 {
    app.auth_get(
        &format!("/api/tenant/{}/room/{}", t.tenant_id, room_id),
        token,
    )
    .send()
    .await
    .unwrap()
    .status()
    .as_u16()
}

async fn list_room_ids(
    app: &TestApp,
    t: &crate::fixtures::seed::SeededTenant,
    token: &str,
) -> Vec<String> {
    let rooms: Value = app
        .auth_get(&format!("/api/tenant/{}/room", t.tenant_id), token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    rooms
        .as_array()
        .expect("room list")
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn a_room_is_public_by_default_and_every_tenant_member_can_read_it() {
    // The operationally important one. `visibility` is `#[serde(default)]`, so
    // every pre-existing document reads back as Public — shipping this must
    // not revoke anyone's access to anything.
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("vis-default").await;
    let room = make_room(&app, &t, "vis-public-room").await;

    assert_eq!(
        get_room_status(&app, &t, &room, &t.member.access_token).await,
        200,
        "a member who never joined must still read a Public room"
    );
    assert!(
        list_room_ids(&app, &t, &t.member.access_token)
            .await
            .contains(&room),
        "a Public room must be listed"
    );
}

#[tokio::test]
async fn a_private_room_is_refused_to_a_non_member_but_still_listed() {
    // Listed-but-not-readable is the whole point of Private: you can see it
    // exists, so you can ask to be let in.
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("vis-private").await;
    let room = make_room(&app, &t, "leadership").await;

    set_visibility(&app, &t, &room, "private").await;

    assert_eq!(
        get_room_status(&app, &t, &room, &t.member.access_token).await,
        403,
        "a non-member must be refused a Private room"
    );
    assert!(
        list_room_ids(&app, &t, &t.member.access_token)
            .await
            .contains(&room),
        "a Private room must STILL be listed — that is what distinguishes it from Secret"
    );
}

#[tokio::test]
async fn a_secret_room_is_404_and_absent_from_the_listing() {
    // 404 not 403: a 403 confirms the room exists to someone who is not meant
    // to know that, which is the only thing Secret adds over Private.
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("vis-secret").await;
    let room = make_room(&app, &t, "acquisition").await;

    set_visibility(&app, &t, &room, "secret").await;

    assert_eq!(
        get_room_status(&app, &t, &room, &t.member.access_token).await,
        404,
        "a Secret room must not even confirm it exists"
    );
    assert!(
        !list_room_ids(&app, &t, &t.member.access_token)
            .await
            .contains(&room),
        "a Secret room must not appear in a non-member's listing"
    );
}

#[tokio::test]
async fn the_admin_who_closes_a_room_keeps_access_to_it() {
    // Closing a room must not lock its own admin out. The creator is
    // auto-joined by `RoomDao::create`, and `update` adds the actor before
    // writing, so there is never a members-only room with no members.
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("vis-noselflock").await;
    let room = make_room(&app, &t, "board").await;

    for v in ["private", "secret"] {
        set_visibility(&app, &t, &room, v).await;
        assert_eq!(
            get_room_status(&app, &t, &room, &t.admin.access_token).await,
            200,
            "the admin must still read the room after setting visibility={v}"
        );
        assert!(
            list_room_ids(&app, &t, &t.admin.access_token)
                .await
                .contains(&room),
            "the room must still be listed for its own admin at visibility={v}"
        );
    }
}

#[tokio::test]
async fn joining_a_private_room_grants_access() {
    // The way back in: Private is listed, so a member can join it and then
    // read it. (Whether self-join SHOULD be allowed for Private is a product
    // question; this locks the mechanism that membership is what grants read.)
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("vis-join").await;
    let room = make_room(&app, &t, "projects").await;
    set_visibility(&app, &t, &room, "private").await;

    assert_eq!(
        get_room_status(&app, &t, &room, &t.member.access_token).await,
        403
    );

    let joined = app
        .auth_post(
            &format!("/api/tenant/{}/room/{}/join", t.tenant_id, room),
            &t.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(joined.status().is_success(), "join: {}", joined.status());

    assert_eq!(
        get_room_status(&app, &t, &room, &t.member.access_token).await,
        200,
        "membership is what grants read"
    );
}

#[tokio::test]
async fn visibility_survives_a_round_trip_through_the_api() {
    // Wire-format lock. The value is stored through `bson::to_bson`, and the
    // enum is snake_case — a drift here would silently store something the
    // deserializer reads back as the DEFAULT, i.e. Public, i.e. wide open.
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("vis-roundtrip").await;
    let room = make_room(&app, &t, "wire").await;

    for v in ["private", "secret", "public"] {
        set_visibility(&app, &t, &room, v).await;
        let body: Value = app
            .auth_get(
                &format!("/api/tenant/{}/room/{}", t.tenant_id, room),
                &t.admin.access_token,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            body["visibility"].as_str(),
            Some(v),
            "visibility must round-trip as {v}"
        );
    }
}

#[tokio::test]
async fn a_secret_room_cannot_be_joined_by_guessing_its_id() {
    // Join resolves the room WITHOUT the visibility gate (otherwise Private
    // would be unjoinable), so Secret needs its own refusal here — else the
    // one property Secret adds over Private is defeated by anyone who learns
    // the id: walk in, become a member, read everything.
    //
    // 404, matching the read path: a 403 would confirm the room exists.
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("vis-secret-join").await;
    let room = make_room(&app, &t, "warroom").await;
    set_visibility(&app, &t, &room, "secret").await;

    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room/{}/join", t.tenant_id, room),
            &t.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "a Secret room must not be joinable by id"
    );

    assert_eq!(
        get_room_status(&app, &t, &room, &t.member.access_token).await,
        404,
        "and it must still be unreadable afterwards"
    );
}

// ─── Server-side sidebar search (PR-D, 2026-08-26) ──────────────

/// `GET /room?q=…&page=…` — the capped-sidebar search. The secret-room
/// condition is pushed INTO the query, so results respect visibility at the
/// same strength as the bare list, and the response stays a bare array
/// (the client infers has_more from a full page).
#[tokio::test]
async fn list_with_q_searches_and_respects_secret_visibility() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("vis-search").await;

    make_room(&app, &t, "growth-general").await;
    make_room(&app, &t, "growth-metrics").await;
    let secret = make_room(&app, &t, "growth-secret-plans").await;
    set_visibility(&app, &t, &secret, "secret").await;

    // A plain member searching the shared prefix sees the two public rooms,
    // never the secret one.
    let body: Value = app
        .auth_get(
            &format!("/api/tenant/{}/room?q=growth", t.tenant_id),
            &t.member.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rooms = body.as_array().expect("search response stays a bare array");
    let names: Vec<&str> = rooms.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"growth-general"));
    assert!(names.contains(&"growth-metrics"));
    assert!(
        !names.contains(&"growth-secret-plans"),
        "secret room leaked into member search"
    );

    // The admin (creator, hence member of the secret room) DOES see it.
    let body: Value = app
        .auth_get(
            &format!("/api/tenant/{}/room?q=secret-plans", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Regex metacharacters in q are literal, not a pattern.
    let body: Value = app
        .auth_get(
            &format!("/api/tenant/{}/room?q=.%2A", t.tenant_id), // ".*"
            &t.member.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body.as_array().unwrap().len(),
        0,
        ".* must match nothing as a literal"
    );

    // Paging: per_page=1 walks disjoint results and can't under-fill past
    // the secret room (the visibility condition is inside the query).
    let mut seen = Vec::new();
    for page in 1..=2 {
        let body: Value = app
            .auth_get(
                &format!(
                    "/api/tenant/{}/room?q=growth&per_page=1&page={page}",
                    t.tenant_id
                ),
                &t.member.access_token,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "page {page} under-filled");
        seen.push(arr[0]["id"].as_str().unwrap().to_string());
    }
    seen.dedup();
    assert_eq!(seen.len(), 2, "pages overlapped");
}

/// The room update's body, field for field (`chat::room::UpdateRoomRequest`).
/// This crate does not link the chat module, and what axum's `Json` extractor
/// answers a body depends on the body type's shape alone.
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct UpdateRoomRequest {
    name: Option<String>,
    topic: Option<String>,
    purpose: Option<String>,
    is_open: Option<bool>,
    is_archived: Option<bool>,
    is_read_only: Option<bool>,
    visibility: Option<roomler_ai_db::models::RoomVisibility>,
}

/// #4254 — the room update decides its gates before it reads its body at
/// all. Someone outside the org gets the room guard's one `not_a_member`, and
/// a member who may read the room but holds no `MANAGE_CHANNELS` the one 403
/// `forbidden`, for a body that does not even parse, the answer a body the
/// route reads gets. While it took `Json<T>` as an extractor, axum answered
/// such a body before the handler ran, with a 400, 415 or 422 decided by the
/// body alone.
#[tokio::test]
async fn the_room_update_decides_the_bit_before_a_body_that_does_not_parse() {
    use crate::tunnel_policy_permission_tests::{
        Gate, GatedWrite, assert_no_problems, gate_problems,
    };

    let app = TestApp::spawn().await;
    let t = app.seed_tenant("roomupd-unparsed").await;
    let other = app.seed_tenant("roomupd-unparsed-b").await;
    // A Public room, which every member of the org may read: the member is
    // refused for the bit alone.
    let write = GatedWrite {
        method: "PUT",
        path: format!("/api/tenant/{}/room/{}", t.tenant_id, t.rooms[0].id),
        reads: json!({ "topic": "a new topic" }),
        refused_field: r#"{"name": 7}"#,
    };

    let mut wrong = Vec::new();
    for (who, token, gate) in [
        (
            "a member without the bit",
            &t.member.access_token,
            Gate::Bit("MANAGE_CHANNELS"),
        ),
        (
            "another org's owner",
            &other.admin.access_token,
            Gate::NotAMember,
        ),
    ] {
        wrong.extend(gate_problems(&app, who, token, gate, &write).await);
    }
    assert_no_problems("answered before the gate", &wrong);
}

/// #4254's control. The org's owner is answered a room update that does not
/// parse exactly as axum's `Json` extractor answers it, the status and the
/// plain-text words, and the room is left as it was.
#[tokio::test]
async fn a_holder_is_answered_a_room_update_that_does_not_parse_as_the_json_extractor_answers_it() {
    use crate::tunnel_policy_permission_tests::{assert_no_problems, holder_problems};

    async fn read(app: &TestApp, path: &str, token: &str) -> Value {
        let resp = app.auth_get(path, token).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        resp.json().await.unwrap()
    }

    let app = TestApp::spawn().await;
    let t = app.seed_tenant("roomupd-unparsed-holder").await;
    let owner = &t.admin.access_token;
    let path = format!("/api/tenant/{}/room/{}", t.tenant_id, t.rooms[0].id);
    let before = read(&app, &path, owner).await;

    let wrong =
        holder_problems::<UpdateRoomRequest>(&app, owner, "PUT", &path, r#"{"name": 7}"#).await;
    assert_no_problems("the org's owner", &wrong);
    assert_eq!(
        read(&app, &path, owner).await,
        before,
        "the room is left as it was"
    );
}

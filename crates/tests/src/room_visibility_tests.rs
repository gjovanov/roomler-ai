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

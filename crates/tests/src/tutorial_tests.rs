// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-12 P3 — the tutorial's server-side mirror.
//!
//! The point of the mirror is that progress follows the PERSON, not the
//! browser profile: someone who did the welcome tour on their laptop should
//! not be walked through it again on their phone. The client keeps working
//! from `localStorage` if this route is unreachable, so what these tests hold
//! down is the round trip and its bounds — not whether the UI renders a tick.

use crate::fixtures::test_app::TestApp;
use serde_json::Value;

/// The whole contract in one pass: what a client PUTs comes back on the
/// response it already fetches at boot.
#[tokio::test]
async fn tutorial_state_round_trips_through_auth_me() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("tut1").await;
    let token = &tenant.member.access_token;

    // A fresh account has nothing, and says so with an empty list rather than
    // a null the client would have to special-case.
    let me: Value = app
        .auth_get("/api/auth/me", token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        me["tutorial"]["done"].as_array().map(|a| a.len()),
        Some(0),
        "fresh account: {me}"
    );
    assert!(me["tutorial"]["seen_at"].is_null(), "fresh account: {me}");

    let resp = app
        .auth_put("/api/user/tutorial", token)
        .json(&serde_json::json!({ "done": ["devices", "acl"], "seen": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let me: Value = app
        .auth_get("/api/auth/me", token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        me["tutorial"]["done"],
        serde_json::json!(["devices", "acl"]),
        "after write: {me}"
    );
    assert!(
        me["tutorial"]["seen_at"].is_string(),
        "seen must persist: {me}"
    );
}

/// `done` REPLACES. Un-ticking a chapter has to be expressible, which it would
/// not be if the server unioned — the client would send a shorter list and the
/// server would keep the longer one, silently.
#[tokio::test]
async fn writing_done_replaces_rather_than_unions() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("tut2").await;
    let token = &tenant.member.access_token;

    for body in [
        serde_json::json!({ "done": ["devices", "acl", "rooms"] }),
        serde_json::json!({ "done": ["devices"] }),
    ] {
        let resp = app
            .auth_put("/api/user/tutorial", token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    let me: Value = app
        .auth_get("/api/auth/me", token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        me["tutorial"]["done"],
        serde_json::json!(["devices"]),
        "{me}"
    );
}

/// `seen` is a one-way latch. Its job is to stop the tour ambushing someone a
/// second time, so nothing a client sends may clear it — `seen: false` is a
/// no-op, not a reset.
#[tokio::test]
async fn seen_is_a_latch_and_a_later_write_cannot_clear_it() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("tut3").await;
    let token = &tenant.member.access_token;

    app.auth_put("/api/user/tutorial", token)
        .json(&serde_json::json!({ "seen": true }))
        .send()
        .await
        .unwrap();
    app.auth_put("/api/user/tutorial", token)
        .json(&serde_json::json!({ "seen": false, "done": ["rooms"] }))
        .send()
        .await
        .unwrap();

    let me: Value = app
        .auth_get("/api/auth/me", token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        me["tutorial"]["seen_at"].is_string(),
        "seen_at must survive a later write: {me}"
    );
}

/// The list is client-supplied and lands on the caller's own user document, so
/// it is bounded. Truncation rather than rejection, deliberately: a checklist
/// request that is merely too long should not fail someone's tutorial.
#[tokio::test]
async fn an_oversized_chapter_list_is_bounded_not_rejected() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("tut4").await;
    let token = &tenant.member.access_token;

    let mut done: Vec<String> = (0..500).map(|i| format!("chapter-{i}")).collect();
    // A single absurd id alongside the flood: length is capped per-entry too.
    done.push("x".repeat(4096));

    let resp = app
        .auth_put("/api/user/tutorial", token)
        .json(&serde_json::json!({ "done": done }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "bounded, not refused");

    let me: Value = app
        .auth_get("/api/auth/me", token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let stored = me["tutorial"]["done"].as_array().unwrap();
    assert_eq!(stored.len(), 64, "capped at MAX_TUTORIAL_CHAPTERS: {me}");
    assert!(
        stored.iter().all(|c| c.as_str().unwrap().len() <= 64),
        "over-long ids dropped: {me}"
    );
}

/// A user may only ever write their OWN state — there is no id in the path, so
/// the only thing to prove is that an unauthenticated call cannot write at all.
#[tokio::test]
async fn tutorial_write_requires_authentication() {
    let app = TestApp::spawn().await;
    let resp = app
        .client
        .put(format!("{}/api/user/tutorial", app.base_url))
        .json(&serde_json::json!({ "done": ["devices"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

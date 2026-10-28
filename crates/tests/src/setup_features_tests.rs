// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-129 B1-s — `GET /api/setup/features`, the server's kill switch for
//! Phase B (AC14's server half): the default shape, each flag following its
//! setting, and no credential needed — an installer and a guest's homepage
//! read it before anyone has signed in.

use serde_json::{Value, json};

use crate::fixtures::test_app::TestApp;

const ROUTE: &str = "/api/setup/features";

/// A request with NO credential at all: a fresh client, no cookie jar.
/// `app.client` keeps one, and a seeded org's logins would leave a session
/// cookie in it — the second place `AuthUser` looks.
async fn get_anon(app: &TestApp) -> reqwest::Response {
    reqwest::Client::new()
        .get(app.url(ROUTE))
        .send()
        .await
        .expect("request")
}

/// The shipped defaults: pairing ON (A1), the Linux/macOS daemon wizard OFF
/// (the wizard that can do it has not shipped). Exactly these two keys, both
/// booleans — a consumer never has to default a missing one.
#[tokio::test]
async fn the_default_shape_is_pairing_on_and_the_unix_daemon_wizard_off() {
    let app = TestApp::spawn().await;
    let resp = get_anon(&app).await;
    assert_eq!(resp.status().as_u16(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "a JSON object, not {content_type:?}"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "pairing": true, "wizard_unix_daemon": false })
    );
}

/// Each flag follows its own setting, and what `pairing` says matches what
/// the pairing routes do: with `[pairing] enabled = false` the flag is
/// false AND `/api/pair/start` is the empty 404 of an absent path.
#[tokio::test]
async fn each_flag_follows_its_setting() {
    let app = TestApp::spawn_with_settings(|s| {
        s.pairing.enabled = false;
        s.setup.wizard_unix_daemon = true;
    })
    .await;
    let body: Value = get_anon(&app).await.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "pairing": false, "wizard_unix_daemon": true })
    );

    let start = reqwest::Client::new()
        .post(app.url("/api/pair/start"))
        .json(&json!({
            "machine_name": "features test",
            "os": "linux",
            "arch": "x86_64",
            "role": "daemon",
            "client": "wizard",
            "client_version": "0.4.182",
        }))
        .send()
        .await
        .expect("request");
    assert_eq!(
        start.status().as_u16(),
        404,
        "pairing: false ⇒ the pairing routes are gone too"
    );

    // The other way round: the wizard switch alone, pairing untouched.
    let app = TestApp::spawn_with_settings(|s| s.setup.wizard_unix_daemon = true).await;
    let body: Value = get_anon(&app).await.json().await.unwrap();
    assert_eq!(body, json!({ "pairing": true, "wizard_unix_daemon": true }));
}

/// A signed-in person gets the same answer as nobody: the switches are
/// server-wide, not a capability of the caller.
#[tokio::test]
async fn a_member_reads_the_same_switches_as_a_guest() {
    let app = TestApp::spawn_with_settings(|s| s.setup.wizard_unix_daemon = true).await;
    let org = app.seed_tenant("setupfeat").await;
    let signed_in: Value = app
        .auth_get(ROUTE, &org.admin.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let guest: Value = get_anon(&app).await.json().await.unwrap();
    assert_eq!(signed_in, guest);
    assert_eq!(guest["wizard_unix_daemon"], true);
}

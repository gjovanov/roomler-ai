// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-129 B1-s — `GET /api/setup/features`, the server's kill switch for
//! Phase B (AC14's server half): the default shape, each flag following its
//! setting — the daemon switch per OS since B1-s2, with `wizard_unix_daemon`
//! answered as their AND for the wizards released before the split — and no
//! credential needed: an installer and a guest's homepage read it before
//! anyone has signed in.

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

/// The shipped defaults: pairing ON (A1, the server's fact), every claim
/// about the released wizard OFF (the wizard that installs the daemon off
/// Windows, or pairs, has not shipped). Exactly these five keys, all
/// booleans — a consumer never has to default a missing one.
#[tokio::test]
async fn the_default_shape_is_pairing_on_and_the_wizard_claims_off() {
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
        json!({
            "pairing": true,
            "wizard_linux_daemon": false,
            "wizard_macos_daemon": false,
            "wizard_unix_daemon": false,
            "wizard_pairing": false,
        })
    );
}

/// Each flag follows its own setting, and what `pairing` says matches what
/// the pairing routes do: with `[pairing] enabled = false` the flag is false
/// AND `/api/pair/start` is the empty 404 of an absent path.
#[tokio::test]
async fn each_flag_follows_its_setting() {
    let app = TestApp::spawn_with_settings(|s| {
        s.pairing.enabled = false;
        s.setup.wizard_unix_daemon = true;
        s.setup.wizard_pairing = true;
    })
    .await;
    let body: Value = get_anon(&app).await.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "pairing": false,
            "wizard_linux_daemon": true,
            "wizard_macos_daemon": true,
            "wizard_unix_daemon": true,
            "wizard_pairing": true,
        })
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

    // The pairing claim alone: no daemon key follows it.
    let app = TestApp::spawn_with_settings(|s| s.setup.wizard_pairing = true).await;
    let body: Value = get_anon(&app).await.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "pairing": true,
            "wizard_linux_daemon": false,
            "wizard_macos_daemon": false,
            "wizard_unix_daemon": false,
            "wizard_pairing": true,
        })
    );
}

/// FR-129 B1-s2 — Linux goes live without macOS. Each OS's key turns that
/// OS on alone, and `wizard_unix_daemon` stays `false`: it is the one key the
/// wizards released before the split read for BOTH OSes, and their
/// User-Agent carries no OS, so a Linux-only server must show them nothing
/// rather than macOS cards the operator left off.
#[tokio::test]
async fn linux_alone_hides_the_cards_from_wizards_released_before_the_split() {
    let app = TestApp::spawn_with_settings(|s| s.setup.wizard_linux_daemon = true).await;
    let body: Value = get_anon(&app).await.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "pairing": true,
            "wizard_linux_daemon": true,
            "wizard_macos_daemon": false,
            "wizard_unix_daemon": false,
            "wizard_pairing": false,
        })
    );

    // And macOS on alone, the same way round.
    let app = TestApp::spawn_with_settings(|s| s.setup.wizard_macos_daemon = true).await;
    let body: Value = get_anon(&app).await.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "pairing": true,
            "wizard_linux_daemon": false,
            "wizard_macos_daemon": true,
            "wizard_unix_daemon": false,
            "wizard_pairing": false,
        })
    );
}

/// Both per-OS keys on: every daemon key on, `wizard_unix_daemon` with them,
/// so the wizards released before the split show their cards on both OSes.
#[tokio::test]
async fn both_oses_on_turn_every_daemon_key_on() {
    let app = TestApp::spawn_with_settings(|s| {
        s.setup.wizard_linux_daemon = true;
        s.setup.wizard_macos_daemon = true;
    })
    .await;
    let body: Value = get_anon(&app).await.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "pairing": true,
            "wizard_linux_daemon": true,
            "wizard_macos_daemon": true,
            "wizard_unix_daemon": true,
            "wizard_pairing": false,
        })
    );
}

/// FR-129 B1-s2 — `[setup] wizard_unix_daemon` is the key a deployment set
/// before the split, and it keeps meaning what it meant: the daemon cards on
/// Linux AND macOS. So it turns both per-OS keys on, and the wire's
/// `wizard_unix_daemon` with them — pairing untouched.
#[tokio::test]
async fn the_legacy_key_turns_both_oses_on() {
    let app = TestApp::spawn_with_settings(|s| s.setup.wizard_unix_daemon = true).await;
    let body: Value = get_anon(&app).await.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "pairing": true,
            "wizard_linux_daemon": true,
            "wizard_macos_daemon": true,
            "wizard_unix_daemon": true,
            "wizard_pairing": false,
        })
    );
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
    assert_eq!(guest["wizard_pairing"], false);
}

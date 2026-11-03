// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3357 (FR-128 D2) — a device enrolled from now on starts on
//! `rc_overlay = auto`, whichever door it came through, and a device that
//! exists keeps what it has.
//!
//! The default is WRITTEN into a new row, and never read into an old one:
//! `rc_overlay_mode()` still reads a missing field as `off`. Reading it as
//! `auto` instead would flip every row that never chose, the whole fleet. So
//! a stored policy has these shapes, and these tests keep them apart:
//!
//! | `rc_overlay` | `rc_overlay_auto` | what it says |
//! |---|---|---|
//! | absent or null | absent or null | never chose: `off` |
//! | `false` | `false` or absent | someone chose `off` |
//! | null | `true` | the default a new row was stamped with: `auto` |
//! | `false` | `true` | someone chose `auto` |
//! | `true` | any | someone chose `on` |

use bson::{Bson, Document, doc, oid::ObjectId};
use futures::SinkExt;
use roomler_ai_remote_control::models::RcOverlayMode;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

use crate::fixtures::{seed::SeededTenant, test_app::TestApp};
use crate::rc_session_limits_tests::{connect_agent_rpc, request_session_from};

const AUTO: RcOverlayMode = RcOverlayMode::Auto { far_turn: None };

fn oid(id: &str) -> ObjectId {
    ObjectId::parse_str(id).unwrap()
}

/// The admin route's single-use token, as an installer receives it.
async fn enrollment_token(app: &TestApp, t: &SeededTenant) -> String {
    let body: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["enrollment_token"]
        .as_str()
        .expect("enrollment_token")
        .to_string()
}

/// `POST /api/agent/enroll`: the route every installer, the desktop companion
/// and a join-org end at. Returns the row's id.
async fn enroll(app: &TestApp, credential: &str, machine_id: &str, os: &str) -> String {
    let resp = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": credential,
            "machine_id": machine_id,
            "machine_name": format!("host-{machine_id}"),
            "os": os,
            "agent_version": "0.4.197",
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert!(status.is_success(), "enroll {machine_id}: {status} {body}");
    body["agent_id"].as_str().expect("agent_id").to_string()
}

/// A pairing, start to tokens: the installer starts it, the admin approves
/// it into the org, and the installer's poll is handed the tokens.
async fn pair(app: &TestApp, t: &SeededTenant, os: &str, halves: u8) -> Vec<String> {
    let started: Value = app
        .client
        .post(app.url("/api/pair/start"))
        .json(&json!({
            "machine_name": "default test laptop",
            "os": os,
            "arch": "x86_64",
            "role": "daemon",
            "client": "wizard",
            "client_version": "0.4.197",
            "halves": halves,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let approved = app
        .auth_post("/api/pair/approve", &t.admin.access_token)
        .json(&json!({ "user_code": started["user_code"], "tenant_id": t.tenant_id }))
        .send()
        .await
        .unwrap();
    assert!(approved.status().is_success(), "{}", approved.status());
    let polled: Value = app
        .client
        .post(app.url("/api/pair/poll"))
        .json(&json!({ "device_code": started["device_code"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(polled["status"], "approved", "{polled}");
    polled["tokens"]
        .as_array()
        .expect("tokens")
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect()
}

/// An ephemeral enrollment key, with the org's gate open.
async fn ephemeral_key(app: &TestApp, t: &SeededTenant) -> String {
    let opened = app
        .auth_put(
            &format!("/api/tenant/{}/ephemeral-key-settings", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "ephemeral_keys_enabled": true }))
        .send()
        .await
        .unwrap();
    assert!(opened.status().is_success(), "{}", opened.status());
    let minted: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-key", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "label": "default test" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    minted["key"].as_str().expect("key").to_string()
}

/// The row's `access_policy` as stored: raw, so that null and absent stay
/// two things.
async fn stored_policy(app: &TestApp, id: &str) -> Document {
    app.db
        .collection::<Document>("agents")
        .find_one(doc! { "_id": oid(id) })
        .await
        .unwrap()
        .expect("agent row")
        .get_document("access_policy")
        .expect("access_policy")
        .clone()
}

/// Put a row in a shape only history can produce: a route can't write a
/// missing field, and most of the fleet is missing one.
async fn store_policy(app: &TestApp, id: &str, policy: &Document) {
    app.db
        .collection::<Document>("agents")
        .update_one(
            doc! { "_id": oid(id) },
            doc! { "$set": { "access_policy": policy } },
        )
        .await
        .unwrap();
}

/// The row as the authz gate reads it.
async fn mode(app: &TestApp, id: &str) -> RcOverlayMode {
    app.state
        .fleet()
        .agents
        .base
        .find_by_id(oid(id))
        .await
        .expect("agent")
        .access_policy
        .rc_overlay_mode()
}

/// A new row, stored and served: `auto`, stamped beside an `rc_overlay` that
/// nobody set. The page writes `false` there beside a person's `auto`, so a
/// read of the fleet can tell the default from a choice.
async fn assert_starts_on_auto(app: &TestApp, t: &SeededTenant, id: &str) {
    assert_eq!(mode(app, id).await, AUTO, "{id}");
    let stored = stored_policy(app, id).await;
    assert_eq!(
        stored.get("rc_overlay_auto"),
        Some(&Bson::Boolean(true)),
        "{stored}"
    );
    assert_eq!(stored.get("rc_overlay"), Some(&Bson::Null), "{stored}");
    let served: Value = app
        .auth_get(
            &format!("/api/tenant/{}/agent/{}", t.tenant_id, id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(served["access_policy"]["rc_overlay_auto"], true, "{served}");
}

#[tokio::test]
async fn a_token_enrollment_starts_on_auto() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("d2token").await;
    let token = enrollment_token(&app, &t).await;
    let id = enroll(&app, &token, "d2-token", "linux").await;
    assert_starts_on_auto(&app, &t, &id).await;
}

/// FR-129's pairing, and a Mac: its person half (the GUI worker's row, a
/// session's target) and its root half are two enrollments of one approval.
#[tokio::test]
async fn a_pairing_starts_on_auto_and_so_does_each_half_of_a_mac() {
    let app = TestApp::spawn_with_settings(|s| s.pairing.poll_interval_secs = 0).await;
    let t = app.seed_tenant("d2pair").await;

    let tokens = pair(&app, &t, "windows", 1).await;
    let id = enroll(&app, &tokens[0], "d2-pair", "windows").await;
    assert_starts_on_auto(&app, &t, &id).await;

    let tokens = pair(&app, &t, "macos", 2).await;
    assert_eq!(tokens.len(), 2);
    for (token, machine) in tokens.iter().zip(["d2-mac-person", "d2-mac-root"]) {
        let id = enroll(&app, token, machine, "macos").await;
        assert_starts_on_auto(&app, &t, &id).await;
    }
}

#[tokio::test]
async fn an_ephemeral_key_starts_on_auto() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("d2ephemeral").await;
    let key = ephemeral_key(&app, &t).await;
    let id = enroll(&app, &key, "d2-ephemeral", "linux").await;
    assert_starts_on_auto(&app, &t, &id).await;
}

/// Whatever a row says, enrolling its machine again changes none of it: a
/// live row, and a removed one, which the enrollment revives. The unique
/// `(tenant_id, machine_id)` index is why it's always the same row.
///
/// The first two cases are the ones that matter: most of the fleet never
/// chose, and those rows must stay `off`, not pick up the default.
#[tokio::test]
async fn a_known_machine_keeps_its_policy_when_it_enrolls_again() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("d2known").await;
    app.set_plan(&t.tenant_id, "business").await;

    let cases = [
        (
            "never chose, older than both fields",
            doc! {},
            RcOverlayMode::Off,
        ),
        (
            "never chose, both stored null",
            doc! { "rc_overlay": Bson::Null, "rc_overlay_auto": Bson::Null },
            RcOverlayMode::Off,
        ),
        (
            "chose off, older than auto",
            doc! { "rc_overlay": false },
            RcOverlayMode::Off,
        ),
        (
            "chose off",
            doc! { "rc_overlay": false, "rc_overlay_auto": false },
            RcOverlayMode::Off,
        ),
        (
            "chose auto",
            doc! { "rc_overlay": false, "rc_overlay_auto": true },
            AUTO,
        ),
        ("chose on", doc! { "rc_overlay": true }, RcOverlayMode::On),
    ];
    for (i, (what, shape, want)) in cases.into_iter().enumerate() {
        let machine = format!("d2-known-{i}");
        let token = enrollment_token(&app, &t).await;
        let id = enroll(&app, &token, &machine, "linux").await;

        let mut policy = doc! {
            "allowed_role_ids": [],
            "allowed_user_ids": [],
            "auto_terminate_idle_minutes": Bson::Null,
        };
        policy.extend(shape);
        store_policy(&app, &id, &policy).await;
        assert_eq!(mode(&app, &id).await, want, "{what}: as stored");

        let token = enrollment_token(&app, &t).await;
        let again = enroll(&app, &token, &machine, "linux").await;
        assert_eq!(again, id, "{what}: a known machine reuses its row");
        assert_eq!(stored_policy(&app, &id).await, policy, "{what}: live row");
        assert_eq!(mode(&app, &id).await, want, "{what}: live row");

        let removed = app
            .auth_delete(
                &format!("/api/tenant/{}/agent/{}", t.tenant_id, id),
                &t.admin.access_token,
            )
            .send()
            .await
            .unwrap();
        assert!(
            removed.status().is_success(),
            "{what}: {}",
            removed.status()
        );
        let token = enrollment_token(&app, &t).await;
        let revived = enroll(&app, &token, &machine, "linux").await;
        assert_eq!(revived, id, "{what}: a removed row is revived");
        assert_eq!(stored_policy(&app, &id).await, policy, "{what}: revived");
        assert_eq!(mode(&app, &id).await, want, "{what}: revived");
    }
}

/// Someone sets a device to `off` on the Devices page, which writes both
/// fields. Its next enrollment leaves that alone: the default is for a row
/// being created, never for a row that lacks a choice.
#[tokio::test]
async fn a_device_set_to_off_stays_off_when_it_enrolls_again() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("d2off").await;
    let token = enrollment_token(&app, &t).await;
    let id = enroll(&app, &token, "d2-off", "linux").await;

    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/agent/{}", t.tenant_id, id),
            &t.admin.access_token,
        )
        .json(&json!({ "access_policy": {
            "allowed_role_ids": [],
            "allowed_user_ids": [],
            "auto_terminate_idle_minutes": null,
            "rc_overlay": false,
            "rc_overlay_auto": false,
        }}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    assert_eq!(mode(&app, &id).await, RcOverlayMode::Off);

    let token = enrollment_token(&app, &t).await;
    assert_eq!(enroll(&app, &token, "d2-off", "linux").await, id);
    assert_eq!(mode(&app, &id).await, RcOverlayMode::Off);
    let stored = stored_policy(&app, &id).await;
    assert_eq!(stored.get("rc_overlay_auto"), Some(&Bson::Boolean(false)));
}

/// FR-128 D1 holds for a row nobody set: a device that decides `auto` is sent
/// it, and a device without the `rc-overlay-auto` verb reads the very same
/// stored `auto` as `off`: no `rc_overlay_auto`, and no relay handed out.
#[tokio::test]
async fn a_new_rows_auto_reaches_only_a_device_that_decides_it() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("d2verb").await;
    // FR-115 P3 — several sessions at once in one org: Free runs one.
    app.set_plan(&t.tenant_id, "business").await;
    let admin = t.admin.access_token.clone();

    // The device the browser runs on, reporting its relay.
    let (viewer, mut viewer_ws) = connect_agent_rpc(&app, &t, "d2-viewer", &["local-relay"]).await;
    viewer_ws
        .send(Message::Text(
            json!({
                "t": "rc:agent.local_relay",
                "relay": {
                    "turn_port": 47989,
                    "overlay_ip": "100.64.0.9",
                    "username": "1700000600:uid",
                    "credential": "abcd1234",
                },
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (new_agent, mut new_ws) = connect_agent_rpc(&app, &t, "d2-new", &["rc-overlay-auto"]).await;
    assert_starts_on_auto(&app, &t, &new_agent).await;
    let (_ctl1, created, request) =
        request_session_from(&app, &admin, &new_agent, &mut new_ws, &viewer).await;
    assert_eq!(created["local_relay"]["turn_port"], 47989, "{created}");
    assert_eq!(
        request["rc_overlay_auto"]["viewers"],
        json!(["100.64.0.9"]),
        "{request}"
    );
    assert_ne!(request["rc_overlay"], true, "{request}");

    let (old_agent, mut old_ws) = connect_agent_rpc(&app, &t, "d2-old", &[]).await;
    assert_starts_on_auto(&app, &t, &old_agent).await;
    let (_ctl2, created, request) =
        request_session_from(&app, &admin, &old_agent, &mut old_ws, &viewer).await;
    assert!(created["local_relay"].is_null(), "{created}");
    assert_eq!(
        created["local_relay_refused"], "rc_overlay_off",
        "{created}"
    );
    assert!(request["rc_overlay_auto"].is_null(), "{request}");
    assert_ne!(request["rc_overlay"], true, "{request}");
}

/// `ROOMLER__RC__NEW_DEVICE_RC_OVERLAY_AUTO=false`: new rows go back to what
/// they were, never chosen, through both `create` paths. A row that already
/// says `auto` keeps it: the switch decides what a new row is written with.
#[tokio::test]
async fn the_server_switch_puts_new_rows_back_on_off() {
    let app = TestApp::spawn_with_settings(|s| s.rc.new_device_rc_overlay_auto = false).await;
    let t = app.seed_tenant("d2switch").await;

    let token = enrollment_token(&app, &t).await;
    let by_token = enroll(&app, &token, "d2-switch-token", "linux").await;
    let key = ephemeral_key(&app, &t).await;
    let by_key = enroll(&app, &key, "d2-switch-key", "linux").await;
    for id in [&by_token, &by_key] {
        assert_eq!(mode(&app, id).await, RcOverlayMode::Off, "{id}");
        let stored = stored_policy(&app, id).await;
        assert_eq!(stored.get("rc_overlay"), Some(&Bson::Null), "{stored}");
        assert_eq!(stored.get("rc_overlay_auto"), Some(&Bson::Null), "{stored}");
    }

    let mut stamped = stored_policy(&app, &by_token).await;
    stamped.insert("rc_overlay_auto", true);
    store_policy(&app, &by_token, &stamped).await;
    let token = enrollment_token(&app, &t).await;
    let again = enroll(&app, &token, "d2-switch-token", "linux").await;
    assert_eq!(again, by_token);
    assert_eq!(mode(&app, &by_token).await, AUTO);
}

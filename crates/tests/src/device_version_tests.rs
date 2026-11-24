// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3565 — a device row's version is its daemon's own.
//!
//! Every program that enrolls a device posts the same enroll request: the
//! daemon's own `enroll`, the setup wizard and the companion. The row's
//! `agent_version` is written only by the daemon's hello. The enrollment
//! stores the program that enrolled, and its version, in `enrolling_program`,
//! a field of its own. A row whose daemon has not said hello yet has no
//! version: the field is absent from the row, and the API sends `""`, which
//! every surface reads as "not reported yet".
//!
//! Every enroll here is a raw JSON body, so these tests pin the wire both
//! ways: a request with the new field, and one from an enroller that
//! predates it.

use bson::oid::ObjectId;
use futures::SinkExt;
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::fixtures::{seed::SeededTenant, test_app::TestApp};

/// The setup wizard's version and the daemon's. They differ, as they do in
/// the field: the two are released apart.
const WIZARD: &str = "0.4.215";
const DAEMON: &str = "0.4.197";

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// A single-use enrollment token, minted by the org's admin.
async fn enrollment_token(app: &TestApp, t: &SeededTenant) -> String {
    let v: Value = app
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
    v["enrollment_token"].as_str().unwrap().to_string()
}

/// POST /api/agent/enroll with `extra` merged into the base body. Asserts
/// success and returns the answer.
async fn enroll(app: &TestApp, credential: &str, machine_id: &str, extra: Value) -> Value {
    let mut body = json!({
        "enrollment_token": credential,
        "machine_id": machine_id,
        "machine_name": format!("host-{machine_id}"),
        "os": "windows",
    });
    let obj = body.as_object_mut().unwrap();
    for (k, v) in extra.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    let resp = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 200, "enroll of {machine_id} refused: {v}");
    v
}

/// The device as the admin API shows it.
async fn device(app: &TestApp, t: &SeededTenant, agent_id: &str) -> Value {
    app.auth_get(
        &format!("/api/tenant/{}/agent/{agent_id}", t.tenant_id),
        &t.admin.access_token,
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap()
}

/// The row as stored.
async fn stored_row(app: &TestApp, agent_id: &str) -> bson::Document {
    app.db
        .collection::<bson::Document>("agents")
        .find_one(bson::doc! { "_id": ObjectId::parse_str(agent_id).unwrap() })
        .await
        .unwrap()
        .expect("agent row")
}

type AgentSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Connect as the daemon and say hello with `version`; returns the socket,
/// which the caller keeps open or drops.
async fn daemon_hello(app: &TestApp, agent_token: &str, version: &str) -> AgentSocket {
    let ws_url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        urlencode(agent_token)
    );
    let (mut ws, _) = connect_async(&ws_url).await.expect("ws connect");
    let hello = json!({
        "t": "rc:agent.hello",
        "machine_name": "versioned host",
        "os": "windows",
        "agent_version": version,
        "displays": [{
            "index": 0,
            "name": "DISPLAY1",
            "width_px": 1920,
            "height_px": 1080,
            "scale": 1.0,
            "primary": true,
        }],
        "caps": {
            "hw_encoders": ["openh264"],
            "codecs": ["h264"],
            "has_input_permission": true,
            "supports_clipboard": true,
            "supports_file_transfer": true,
            "max_simultaneous_sessions": 2,
        }
    });
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    ws
}

/// Poll the admin API until the device reads `version`, or fail.
async fn wait_for_version(app: &TestApp, t: &SeededTenant, agent_id: &str, version: &str) -> Value {
    let mut last = Value::Null;
    for _ in 0..60 {
        last = device(app, t, agent_id).await;
        if last["agent_version"] == version {
            return last;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the device never read version {version}: {last}");
}

fn program(name: &str, version: &str) -> Value {
    json!({ "name": name, "version": version })
}

// ────────────────────────────────────────────────────────────────────────────
// AC1 — an enrollment writes no version; the daemon's first hello sets its own
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_enrollment_leaves_the_version_to_the_daemons_first_hello() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("devver1").await;
    let token = enrollment_token(&app, &t).await;
    let ej = enroll(
        &app,
        &token,
        "devver1-m",
        json!({
            "agent_version": WIZARD,
            "enrolling_program": program("roomler-setup", WIZARD),
        }),
    )
    .await;
    let agent_id = ej["agent_id"].as_str().unwrap();

    let row = device(&app, &t, agent_id).await;
    assert_eq!(
        row["agent_version"], "",
        "an enrollment must leave the version not reported yet: {row}"
    );
    let stored = stored_row(&app, agent_id).await;
    assert!(
        stored.get("agent_version").is_none(),
        "a row whose daemon has not said hello stores no version: {stored:?}"
    );

    let _ws = daemon_hello(&app, ej["agent_token"].as_str().unwrap(), DAEMON).await;
    let row = wait_for_version(&app, &t, agent_id, DAEMON).await;
    assert_eq!(row["enrolling_program"], program("roomler-setup", WIZARD));
}

/// FR-51's branch: an ephemeral key's enrollment writes no version either.
#[tokio::test]
async fn an_ephemeral_enrollment_leaves_the_version_to_the_daemons_first_hello() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("devver2").await;
    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/ephemeral-key-settings", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "ephemeral_keys_enabled": true }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "org gate flip failed");
    let minted: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-key", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "ephemeral_ttl_secs": 120 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let key = minted["key"].as_str().expect("ephemeral key");

    let ej = enroll(
        &app,
        key,
        "devver2-m",
        json!({
            "agent_version": WIZARD,
            "enrolling_program": program("roomler-setup", WIZARD),
        }),
    )
    .await;
    assert_eq!(ej["ephemeral"], json!(true), "the key's branch ran: {ej}");
    let agent_id = ej["agent_id"].as_str().unwrap();

    let row = device(&app, &t, agent_id).await;
    assert_eq!(
        row["agent_version"], "",
        "an ephemeral enrollment must leave the version not reported yet: {row}"
    );
    assert_eq!(row["enrolling_program"], program("roomler-setup", WIZARD));

    let _ws = daemon_hello(&app, ej["agent_token"].as_str().unwrap(), DAEMON).await;
    wait_for_version(&app, &t, agent_id, DAEMON).await;
}

// ────────────────────────────────────────────────────────────────────────────
// AC2 — re-enrolling a known machine keeps the version its daemon reported
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn re_enrolling_a_known_machine_keeps_its_daemons_version() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("devver3").await;
    let token = enrollment_token(&app, &t).await;
    let first = enroll(
        &app,
        &token,
        "devver3-m",
        json!({
            "agent_version": DAEMON,
            "enrolling_program": program("roomlerd", DAEMON),
        }),
    )
    .await;
    let agent_id = first["agent_id"].as_str().unwrap();
    let ws = daemon_hello(&app, first["agent_token"].as_str().unwrap(), DAEMON).await;
    wait_for_version(&app, &t, agent_id, DAEMON).await;
    drop(ws);

    // The same machine enrolls again, from a program of another version.
    let token = enrollment_token(&app, &t).await;
    let again = enroll(
        &app,
        &token,
        "devver3-m",
        json!({
            "agent_version": WIZARD,
            "enrolling_program": program("roomler-setup", WIZARD),
        }),
    )
    .await;
    assert_eq!(again["agent_id"], first["agent_id"], "the row is reused");

    let row = device(&app, &t, agent_id).await;
    assert_eq!(
        row["agent_version"], DAEMON,
        "a re-enrollment must keep the version the daemon reported: {row}"
    );
    assert_eq!(
        row["enrolling_program"],
        program("roomler-setup", WIZARD),
        "the row names the program of the latest enrollment"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// AC3 — the row names the program that enrolled it
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_row_names_the_program_that_enrolled_it() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("devver4").await;

    let token = enrollment_token(&app, &t).await;
    let ej = enroll(
        &app,
        &token,
        "devver4-desktop",
        json!({
            "agent_version": "0.4.230",
            "enrolling_program": program("roomler-desktop", "0.4.230"),
        }),
    )
    .await;
    let agent_id = ej["agent_id"].as_str().unwrap();
    let row = device(&app, &t, agent_id).await;
    assert_eq!(
        row["enrolling_program"],
        program("roomler-desktop", "0.4.230")
    );
    assert_eq!(row["agent_version"], "");
    let stored = stored_row(&app, agent_id).await;
    let stored_program = stored
        .get_document("enrolling_program")
        .expect("the row stores the enrolling program");
    assert_eq!(stored_program.get_str("name").unwrap(), "roomler-desktop");
    assert_eq!(stored_program.get_str("version").unwrap(), "0.4.230");

    // A name this server does not know is clamped to `other`, never refused:
    // an older server keeps enrolling a newer program.
    let token = enrollment_token(&app, &t).await;
    let ej = enroll(
        &app,
        &token,
        "devver4-other",
        json!({
            "agent_version": "0.9.0",
            "enrolling_program": program("a-program-from-later", "0.9.0"),
        }),
    )
    .await;
    let row = device(&app, &t, ej["agent_id"].as_str().unwrap()).await;
    assert_eq!(row["enrolling_program"], program("other", "0.9.0"));
}

#[tokio::test]
async fn an_enroller_without_the_field_still_enrolls_and_names_only_its_own_version() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("devver5").await;
    let token = enrollment_token(&app, &t).await;
    // The body an enroller from before #3565 sends: `agent_version` only.
    let ej = enroll(
        &app,
        &token,
        "devver5-m",
        json!({ "agent_version": "0.4.190" }),
    )
    .await;
    let agent_id = ej["agent_id"].as_str().unwrap();

    let row = device(&app, &t, agent_id).await;
    assert_eq!(
        row["enrolling_program"],
        program("unknown", "0.4.190"),
        "the request's agent_version is the enrolling program's version: {row}"
    );
    assert_eq!(row["agent_version"], "", "and never the device's: {row}");
}

// ────────────────────────────────────────────────────────────────────────────
// AC4 — a pin to a row with no reported version is refused unless forced
// ────────────────────────────────────────────────────────────────────────────

/// On the test database only: the row's daemon never connects, so no push
/// is delivered anywhere.
#[tokio::test]
async fn a_pin_to_a_row_with_no_reported_version_is_refused_unless_forced() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("devver6").await;
    let token = enrollment_token(&app, &t).await;
    let ej = enroll(
        &app,
        &token,
        "devver6-m",
        json!({
            "agent_version": WIZARD,
            "enrolling_program": program("roomler-setup", WIZARD),
        }),
    )
    .await;
    let agent_id = ej["agent_id"].as_str().unwrap();
    let pin = format!("agent-v{WIZARD}");
    let one = format!("/api/tenant/{}/agent/{agent_id}/update", t.tenant_id);
    let bulk = format!("/api/tenant/{}/agent/update", t.tenant_id);

    // One device, pinned: refused.
    let resp = app
        .auth_post(&one, &t.admin.access_token)
        .json(&json!({ "pin": pin }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(
        status, 409,
        "a pin to a row with no reported version must be refused: {body}"
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not reported"),
        "the refusal says why: {body}"
    );

    // Forced: sent (the device is offline, so not delivered).
    let resp = app
        .auth_post(&one, &t.admin.access_token)
        .json(&json!({ "pin": pin, "force": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert!(v.get("refused").is_none(), "forced: {v}");

    // No pin: nothing to check against the row, so nothing to refuse.
    let resp = app
        .auth_post(&one, &t.admin.access_token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // The bulk push refuses the row on its own line, with a code.
    let v: Value = app
        .auth_post(&bulk, &t.admin.access_token)
        .json(&json!({ "agent_ids": [agent_id], "pin": pin }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["refused"], 1, "bulk, pinned: {v}");
    assert_eq!(v["results"][0]["refused_code"], "version_unreported");
    assert_eq!(v["results"][0]["delivered"], false);

    let v: Value = app
        .auth_post(&bulk, &t.admin.access_token)
        .json(&json!({ "agent_ids": [agent_id], "pin": pin, "force": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["refused"], 0, "bulk, forced: {v}");
}

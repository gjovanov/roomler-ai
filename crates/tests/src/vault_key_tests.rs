// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-3a — a device's vault key, as the fleet stores it from the hello.
//!
//! The vault (P3-3b) seals a leased value to the key on the device's row, so
//! the row must name exactly the key the running daemon holds. Three rules,
//! each driven here over a real agent WebSocket:
//!
//! - written from EVERY hello: the daemon makes a new key at each start, so a
//!   reconnect with another key replaces the old one;
//! - a malformed key is stored as none, never kept and never stored as sent:
//!   the vault must not meet a key it cannot seal to;
//! - a hello without a key clears it: the owner switched `vault_enabled` off,
//!   or the device went back to a build without the vault.

use std::time::Duration;

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE},
};
use bson::{Document, doc, oid::ObjectId};
use futures::SinkExt;
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::fixtures::{seed::SeededTenant, test_app::TestApp};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// Enroll a device; `(agent_id, agent_token)`.
async fn enroll(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> (String, String) {
    let et: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ej: Value = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": machine_id,
            "machine_name": machine_id,
            "os": "linux",
            "agent_version": "0.4.142",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        ej["agent_id"].as_str().expect("agent_id").to_string(),
        ej["agent_token"].as_str().expect("agent_token").to_string(),
    )
}

/// Connect as the device and say hello, with `vault_key` when given and
/// without the field at all otherwise (an older agent's hello, or a device
/// whose vault is off: the agent omits an empty key).
async fn hello(app: &TestApp, token: &str, vault_key: Option<&str>) -> Ws {
    let url = format!("ws://{}/ws?token={}&role=agent", app.addr, urlencode(token));
    let (mut ws, _) = connect_async(&url).await.expect("agent ws connect");
    let mut hello = json!({
        "t": "rc:agent.hello",
        "machine_name": "vault-key-host",
        "os": "linux",
        "agent_version": "0.4.142",
        "displays": [],
        "caps": {
            "hw_encoders": [], "codecs": ["h264"], "has_input_permission": false,
            "supports_clipboard": false, "supports_file_transfer": false,
            "max_simultaneous_sessions": 1, "rpc": [],
        }
    });
    if let Some(key) = vault_key {
        hello["vault_key"] = json!(key);
    }
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .expect("send hello");
    ws
}

/// The key on the device's row, `None` for an empty one or none at all.
async fn stored_key(app: &TestApp, agent_id: &str) -> Option<String> {
    let id = ObjectId::parse_str(agent_id).unwrap();
    let row: Document = app
        .db
        .collection::<Document>("agents")
        .find_one(doc! { "_id": id })
        .await
        .unwrap()
        .expect("the agent row");
    row.get_str("vault_key")
        .ok()
        .filter(|k| !k.is_empty())
        .map(str::to_string)
}

/// Wait until the row holds `want`, failing with what it held instead.
async fn wait_for_key(app: &TestApp, agent_id: &str, want: Option<&str>) {
    let mut last = None;
    for _ in 0..100 {
        last = stored_key(app, agent_id).await;
        if last.as_deref() == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the row's vault key stayed {last:?}, expected {want:?}");
}

#[tokio::test]
async fn every_hello_writes_the_vault_key_and_a_malformed_one_is_stored_as_none() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("vaultkey").await;
    let (agent_id, token) = enroll(&app, &seeded, "vault-key-host").await;
    assert_eq!(
        stored_key(&app, &agent_id).await,
        None,
        "enrollment knows no key"
    );

    // The daemon's first start: its key is stored as sent.
    let first = STANDARD.encode([7u8; 32]);
    let mut ws = hello(&app, &token, Some(&first)).await;
    wait_for_key(&app, &agent_id, Some(&first)).await;
    ws.close(None).await.ok();

    // A restart makes a new key, and the next hello replaces the old one.
    let second = STANDARD.encode([9u8; 32]);
    let mut ws = hello(&app, &token, Some(&second)).await;
    wait_for_key(&app, &agent_id, Some(&second)).await;
    ws.close(None).await.ok();

    // A malformed key (the URL-safe alphabet) is stored as none: neither as
    // sent nor by keeping the key before it.
    let mut ws = hello(&app, &token, Some(&URL_SAFE.encode([0xfbu8; 32]))).await;
    wait_for_key(&app, &agent_id, None).await;
    ws.close(None).await.ok();

    // A key again, then a hello without one, as from a device whose owner
    // switched `vault_enabled` off: the stored key goes with it.
    let mut ws = hello(&app, &token, Some(&first)).await;
    wait_for_key(&app, &agent_id, Some(&first)).await;
    ws.close(None).await.ok();
    let mut ws = hello(&app, &token, None).await;
    wait_for_key(&app, &agent_id, None).await;
    ws.close(None).await.ok();
}

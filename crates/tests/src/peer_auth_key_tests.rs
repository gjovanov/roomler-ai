// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-100 P1a — a device's identity key, as the fleet stores it from the hello
//! and as the device list shows it.
//!
//! The key is a HINT for a browser's first pin (P2), never an authority, so
//! what matters here is that the row and the API name exactly the key the
//! device last sent, in its one shape:
//!
//! - written from EVERY hello, and cleared by a hello without one (a rollback
//!   to a build with no key must not leave browsers expecting its signature);
//! - a malformed key is stored as none, never as sent;
//! - the API shows the key with its fingerprint, both or neither.
//!
//! And one test runs the REAL agent: its hello carries a well-formed key that
//! it keeps beside its config, in one file per enrollment.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bson::{Document, doc, oid::ObjectId};
use futures::SinkExt;
use roomler_ai_remote_control::signaling::{peer_auth_fingerprint, peer_auth_key_is_well_formed};
use roomlerd::{config::AgentConfig, encode::EncoderPreference, enrollment, signaling};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::fixtures::{seed::SeededTenant, test_app::TestApp};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The P-256 generator G (FIPS 186-4 D.1.2.3) as a device key would carry a
/// point, built from its coordinates rather than written as a base64 literal
/// (a secret scanner cannot tell a public curve constant from a key).
fn p256_g() -> String {
    const GX: [u8; 32] = [
        0x6b, 0x17, 0xd1, 0xf2, 0xe1, 0x2c, 0x42, 0x47, 0xf8, 0xbc, 0xe6, 0xe5, 0x63, 0xa4, 0x40,
        0xf2, 0x77, 0x03, 0x7d, 0x81, 0x2d, 0xeb, 0x33, 0xa0, 0xf4, 0xa1, 0x39, 0x45, 0xd8, 0x98,
        0xc2, 0x96,
    ];
    const GY: [u8; 32] = [
        0x4f, 0xe3, 0x42, 0xe2, 0xfe, 0x1a, 0x7f, 0x9b, 0x8e, 0xe7, 0xeb, 0x4a, 0x7c, 0x0f, 0x9e,
        0x16, 0x2b, 0xce, 0x33, 0x57, 0x6b, 0x31, 0x5e, 0xce, 0xcb, 0xb6, 0x40, 0x68, 0x37, 0xbf,
        0x51, 0xf5,
    ];
    let mut point = vec![0x04];
    point.extend_from_slice(&GX);
    point.extend_from_slice(&GY);
    STANDARD.encode(point)
}
/// G's fingerprint, computed outside this code (coreutils `sha256sum`).
const P256_G_FINGERPRINT: &str = "698b ea63 dc44 a344 663f";

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

async fn enroll_token(app: &TestApp, seeded: &SeededTenant) -> String {
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
    et["enrollment_token"].as_str().unwrap().to_string()
}

/// Enroll a device over the API; `(agent_id, agent_token)`.
async fn enroll(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> (String, String) {
    let ej: Value = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": enroll_token(app, seeded).await,
            "machine_id": machine_id,
            "machine_name": machine_id,
            "os": "linux",
            "agent_version": "0.4.145",
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

/// Connect as the device and say hello, with `peer_auth_key` when given and
/// without the field otherwise (an older agent, or one that kept no key).
async fn hello(app: &TestApp, token: &str, peer_auth_key: Option<&str>) -> Ws {
    let url = format!("ws://{}/ws?token={}&role=agent", app.addr, urlencode(token));
    let (mut ws, _) = connect_async(&url).await.expect("agent ws connect");
    let mut hello = json!({
        "t": "rc:agent.hello",
        "machine_name": "device-key-host",
        "os": "linux",
        "agent_version": "0.4.145",
        "displays": [],
        "caps": {
            "hw_encoders": [], "codecs": ["h264"], "has_input_permission": false,
            "supports_clipboard": false, "supports_file_transfer": false,
            "max_simultaneous_sessions": 1, "rpc": [],
        }
    });
    if let Some(key) = peer_auth_key {
        hello["peer_auth_key"] = json!(key);
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
    row.get_str("peer_auth_key")
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
    panic!("the row's device key stayed {last:?}, expected {want:?}");
}

/// `(peer_auth_key, peer_auth_fingerprint)` as the device API shows them.
async fn shown(app: &TestApp, seeded: &SeededTenant, agent_id: &str) -> (Value, Value) {
    let row: Value = app
        .auth_get(
            &format!("/api/tenant/{}/agent/{agent_id}", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        row["peer_auth_key"].clone(),
        row["peer_auth_fingerprint"].clone(),
    )
}

#[tokio::test]
async fn every_hello_writes_the_device_key_and_the_api_shows_it_with_its_fingerprint() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("devicekey").await;
    let (agent_id, token) = enroll(&app, &seeded, "device-key-host").await;
    assert_eq!(
        stored_key(&app, &agent_id).await,
        None,
        "enrollment knows no key"
    );
    assert_eq!(
        shown(&app, &seeded, &agent_id).await,
        (Value::Null, Value::Null),
        "and the API shows none"
    );

    // A key: stored as sent, shown with the fingerprint every surface uses.
    let g = p256_g();
    let mut ws = hello(&app, &token, Some(&g)).await;
    wait_for_key(&app, &agent_id, Some(&g)).await;
    assert_eq!(
        shown(&app, &seeded, &agent_id).await,
        (json!(g), json!(P256_G_FINGERPRINT))
    );
    ws.close(None).await.ok();

    // A compressed point is another shape: stored as none, neither as sent
    // nor by keeping the key before it, and the API shows neither half.
    let point = STANDARD.decode(&g).unwrap();
    let mut compressed = vec![0x02];
    compressed.extend_from_slice(&point[1..33]);
    let mut ws = hello(&app, &token, Some(&STANDARD.encode(&compressed))).await;
    wait_for_key(&app, &agent_id, None).await;
    assert_eq!(
        shown(&app, &seeded, &agent_id).await,
        (Value::Null, Value::Null)
    );
    ws.close(None).await.ok();

    // A key again, then a hello without one, as from a build rolled back to
    // before the key: the stored key goes with it.
    let mut ws = hello(&app, &token, Some(&g)).await;
    wait_for_key(&app, &agent_id, Some(&g)).await;
    ws.close(None).await.ok();
    let mut ws = hello(&app, &token, None).await;
    wait_for_key(&app, &agent_id, None).await;
    ws.close(None).await.ok();
}

/// The real agent, with a config path in a directory of its own: its hello
/// carries a well-formed key, the key file sits beside the config, and the
/// API's fingerprint is that key's.
#[tokio::test]
async fn the_real_agent_says_hello_with_a_key_it_keeps_beside_its_config() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("devicekey-real").await;
    let cfg: AgentConfig = enrollment::enroll(enrollment::EnrollInputs {
        server_url: &app.base_url,
        enrollment_token: &enroll_token(&app, &seeded).await,
        machine_id: "device-key-real",
        machine_name: "Device key host",
        install: None,
    })
    .await
    .expect("agent enrollment");
    let agent_id = cfg.agent_id.clone();
    let tenant_id = cfg.tenant_id.clone();
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let agent = {
        let config_path = config_path.clone();
        tokio::spawn(async move {
            let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (view_tx, _view_rx) = tokio::sync::watch::channel(Default::default());
            let broker = roomlerd::consent::ConsentBroker::new(
                roomlerd::consent::Mode::AutoGrant,
                std::env::temp_dir().join(format!("roomler-device-key-consent-{}", cfg.agent_id)),
            )
            .expect("consent broker init");
            let _ = signaling::run(
                signaling::OrgCtx::primary(roomlerd::token_slot::TokenSlot::new(
                    cfg.agent_token.clone(),
                )),
                roomlerd::delegate::Delegation::Off,
                cfg,
                EncoderPreference::Software,
                stop_rx,
                connected,
                view_tx,
                Default::default(),
                broker,
                roomlerd::tunnel::client_mgr::TunnelClientHub::new("test".into()),
                // The one thing this test varies: a config path WITH a
                // directory, which is where the key file goes.
                roomlerd::remote_config::RemoteConfigServices::new(
                    config_path,
                    std::sync::Arc::new(tokio::sync::Mutex::new(())),
                    false,
                    false,
                ),
                roomlerd::rc_sessions::RcSessionRegistry::new(),
            )
            .await;
        })
    };

    let mut key = None;
    for _ in 0..100 {
        key = stored_key(&app, &agent_id).await;
        if key.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let key = key.expect("the real agent's hello carried a device key");
    assert!(peer_auth_key_is_well_formed(&key), "{key}");
    let file = dir
        .path()
        .join(roomlerd::peer_auth::DIR_NAME)
        .join(format!("{tenant_id}.json"));
    assert!(
        file.exists(),
        "the key is kept beside the config: {}",
        file.display()
    );
    assert_eq!(
        shown(&app, &seeded, &agent_id).await,
        (json!(key), json!(peer_auth_fingerprint(&key).unwrap()))
    );

    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(10), agent).await;
}

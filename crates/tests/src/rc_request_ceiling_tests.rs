// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-101 P4a — the per-(caller, device) ceiling on `rc:session.request`,
//! through a real server and real WebSockets.
//!
//! Session requests ride the control WS, which the per-IP HTTP governor never
//! sees. The ceiling sits AFTER the authz gate, as exec's does: a caller who
//! may ask is refused `rate_limited` past the limit, and a caller who may not
//! is refused `permission_denied` every time and never spends a slot. The key
//! is the pair: hitting the ceiling on one device leaves another untouched.

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The remote module's `controller::SESSION_REQUESTS_PER_MINUTE`, pinned
/// here on purpose: the ceiling is a reviewed number, and moving it should
/// fail this test until the test agrees.
const LIMIT: usize = 60;

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// Enroll and connect a device; the socket is returned so it stays online.
async fn connect_agent(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> (String, Ws) {
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
            "agent_version": "0.1.0",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let agent_id = ej["agent_id"].as_str().unwrap().to_string();
    let token = ej["agent_token"].as_str().unwrap().to_string();
    let url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        urlencode(&token)
    );
    let (mut ws, _) = connect_async(&url).await.expect("agent ws");
    let hello = json!({
        "t": "rc:agent.hello",
        "machine_name": machine_id,
        "os": "linux",
        "agent_version": "0.1.0",
        "displays": [{
            "index": 0, "name": "x", "width_px": 800, "height_px": 600,
            "scale": 1.0, "primary": true,
        }],
        "caps": {
            "hw_encoders": [],
            "codecs": ["h264"],
            "has_input_permission": false,
            "supports_clipboard": false,
            "supports_file_transfer": false,
            "max_simultaneous_sessions": 4,
            "input": ["arbiter"],
        },
    });
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    (agent_id, ws)
}

async fn controller(app: &TestApp, token: &str) -> Ws {
    let url = format!("ws://{}/ws?token={}", app.addr, urlencode(token));
    let (mut ws, _) = connect_async(&url).await.expect("controller ws");
    let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    ws
}

async fn request(ws: &mut Ws, agent_id: &str) {
    let req = json!({
        "t": "rc:session.request",
        "agent_id": agent_id,
        "permissions": "VIEW",
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();
}

/// Read what arrives within `quiet` of the last frame and count the
/// `rc:error` codes, plus `rc:session.created`.
async fn drain(ws: &mut Ws, quiet: Duration) -> (usize, Vec<String>) {
    let mut created = 0;
    let mut errors = Vec::new();
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(quiet, ws.next()).await {
        let Message::Text(text) = frame else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match v["t"].as_str() {
            Some("rc:session.created") => created += 1,
            Some("rc:error") => errors.push(v["code"].as_str().unwrap_or("?").to_string()),
            _ => {}
        }
    }
    (created, errors)
}

#[tokio::test]
async fn session_requests_meet_a_per_device_ceiling_after_the_identity_gates() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr101p4a").await;
    let (a, _a_ws) = connect_agent(&app, &seeded, "mach-fr101-ceiling-a").await;
    let (b, _b_ws) = connect_agent(&app, &seeded, "mach-fr101-ceiling-b").await;
    let limit = LIMIT;

    // The device's owner may ask: every request up to the ceiling passes the
    // limiter. A repeat on one socket coalesces onto the first session
    // (#1045), so the device is asked for one session, not sixty.
    let mut owner = controller(&app, &seeded.admin.access_token).await;
    for _ in 0..limit {
        request(&mut owner, &a).await;
    }
    let (created, errors) = drain(&mut owner, Duration::from_millis(800)).await;
    assert!(created >= 1, "the owner's session was created");
    assert!(
        !errors.iter().any(|c| c == "rate_limited"),
        "refused before the ceiling: {errors:?}"
    );

    // One past it is refused, with a reason a viewer can tell apart.
    request(&mut owner, &a).await;
    let (_, errors) = drain(&mut owner, Duration::from_millis(800)).await;
    assert_eq!(errors, ["rate_limited"], "request {} of {limit}", limit + 1);

    // The key is the (caller, device) pair: another device still answers.
    let mut other = controller(&app, &seeded.admin.access_token).await;
    request(&mut other, &b).await;
    let (created, errors) = drain(&mut other, Duration::from_millis(800)).await;
    assert!(
        created == 1 && errors.is_empty(),
        "device B is untouched by A's ceiling: created {created}, errors {errors:?}"
    );

    // A caller with no right to ask is refused by identity, every time, and
    // never reaches the limiter: no request of theirs is `rate_limited`.
    let mut member = controller(&app, &seeded.member.access_token).await;
    for _ in 0..=limit {
        request(&mut member, &a).await;
    }
    let (created, errors) = drain(&mut member, Duration::from_millis(800)).await;
    assert_eq!(created, 0);
    assert_eq!(errors.len(), limit + 1, "{errors:?}");
    assert!(
        errors.iter().all(|c| c == "permission_denied"),
        "refused by identity first: {errors:?}"
    );
}

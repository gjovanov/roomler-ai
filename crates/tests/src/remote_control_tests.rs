//! Integration tests for the remote-control subsystem.
//!
//! These exercise the REST flow (enroll → list → delete), the JWT audience
//! separation (enrollment vs agent tokens), and the WS handshake that marks an
//! agent row `online` after `rc:agent.hello`.
//!
//! Full SDP/ICE round-trip tests belong in a follow-up once the native agent
//! binary exists — here we verify the surface the browser + agent talk to.

use crate::fixtures::test_app::TestApp;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ────────────────────────────────────────────────────────────────────────────
// REST: enrollment flow
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn enroll_token_requires_auth() {
    let app = TestApp::spawn().await;
    let resp = app
        .client
        .post(app.url("/api/tenant/000000000000000000000000/agent/enroll-token"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn enroll_agent_full_round_trip() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("rcflow1").await;

    // 1. Admin issues an enrollment token.
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let et: Value = resp.json().await.unwrap();
    let enrollment_token = et["enrollment_token"].as_str().unwrap().to_string();
    assert_eq!(et["expires_in"].as_u64().unwrap(), 600);
    assert!(!et["jti"].as_str().unwrap().is_empty());

    // 2. Agent exchanges it for a long-lived agent token.
    let resp = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": enrollment_token,
            "machine_id": "mach-rcflow1-A",
            "machine_name": "Goran's Laptop",
            "os": "linux",
            "agent_version": "0.1.0",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let ej: Value = resp.json().await.unwrap();
    let agent_token = ej["agent_token"].as_str().unwrap();
    let agent_id = ej["agent_id"].as_str().unwrap();
    assert_eq!(ej["tenant_id"].as_str().unwrap(), seeded.tenant_id);
    assert!(!agent_token.is_empty());
    assert_eq!(agent_id.len(), 24); // hex ObjectId

    // 3. Re-enrolling the same machine_id returns the same agent row.
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let et2: Value = resp.json().await.unwrap();
    let resp = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": et2["enrollment_token"].as_str().unwrap(),
            "machine_id": "mach-rcflow1-A",
            "machine_name": "Goran's Laptop (reinstall)",
            "os": "linux",
            "agent_version": "0.1.1",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let ej2: Value = resp.json().await.unwrap();
    assert_eq!(ej2["agent_id"].as_str().unwrap(), agent_id);
}

#[tokio::test]
async fn enroll_rejects_bogus_token() {
    let app = TestApp::spawn().await;
    let resp = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": "not-a-jwt",
            "machine_id": "mach-x",
            "machine_name": "x",
            "os": "linux",
            "agent_version": "0.1.0",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn agent_token_rejected_on_enroll_endpoint() {
    // The agent token (aud=agent) must not be usable as an enrollment token —
    // verifies JWT audience separation in AuthService.
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("rcflow2").await;

    // Enroll one agent to obtain a real agent_token.
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
            "machine_id": "mach-cross",
            "machine_name": "cross",
            "os": "linux",
            "agent_version": "0.1.0",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let agent_token = ej["agent_token"].as_str().unwrap();

    // Now try to use the agent_token as an enrollment token — must fail.
    let resp = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": agent_token,
            "machine_id": "mach-cross-2",
            "machine_name": "cross2",
            "os": "linux",
            "agent_version": "0.1.0",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

// ────────────────────────────────────────────────────────────────────────────
// REST: agent CRUD
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_agents_shows_enrolled_agent() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("rclist").await;

    let (_, _) = enroll_helper(&app, &seeded, "mach-rclist-A", "laptop").await;

    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/agent", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"].as_str().unwrap(), "laptop");
    // No live WS yet → is_online must be false.
    assert_eq!(items[0]["is_online"].as_bool().unwrap(), false);
}

#[tokio::test]
async fn delete_agent_removes_from_list() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("rcdel").await;

    let (agent_id, _) = enroll_helper(&app, &seeded, "mach-rcdel-A", "A").await;

    app.auth_delete(
        &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, agent_id),
        &seeded.admin.access_token,
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();

    let list: Value = app
        .auth_get(
            &format!("/api/tenant/{}/agent", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn get_missing_agent_returns_404() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("rc404").await;

    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/agent/000000000000000000000000",
                seeded.tenant_id
            ),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn turn_credentials_returns_stun_fallback() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("rcturn").await;

    let resp = app
        .auth_get("/api/turn/credentials", &seeded.admin.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    let servers = json["ice_servers"].as_array().unwrap();
    assert!(!servers.is_empty());
    // Default test settings have no TURN shared_secret → only STUN is returned.
    let first_url = servers[0]["urls"][0].as_str().unwrap();
    assert!(
        first_url.starts_with("stun:"),
        "first ICE server should be STUN; got {first_url}"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// WebSocket: agent handshake marks row online
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn agent_hello_marks_status_online() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("rcws").await;

    let (agent_id, agent_token) = enroll_helper(&app, &seeded, "mach-rcws-A", "WS laptop").await;

    // Connect WS as agent.
    let ws_url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        urlencode(&agent_token)
    );
    let (mut ws, _) = connect_async(&ws_url).await.expect("ws connect");

    // Send rc:agent.hello.
    let hello = json!({
        "t": "rc:agent.hello",
        "machine_name": "WS laptop",
        "os": "linux",
        "agent_version": "0.1.0",
        "displays": [{
            "index": 0,
            "name": "eDP-1",
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

    // Give the server a moment to process the hello + update Mongo.
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let resp: Value = app
            .auth_get(
                &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, agent_id),
                &seeded.admin.access_token,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if resp["status"].as_str() == Some("online") {
            assert_eq!(resp["is_online"].as_bool().unwrap(), true);
            assert_eq!(resp["agent_version"].as_str().unwrap(), "0.1.0");
            // Drain one message just to make sure we can still read — there
            // should be none queued, but the next `next()` may time out which
            // is fine.
            let _ = tokio::time::timeout(std::time::Duration::from_millis(50), ws.next()).await;
            return;
        }
    }
    panic!("agent row never transitioned to online");
}

#[tokio::test]
async fn agent_ws_rejects_user_token() {
    // ?role=agent with a user JWT must be rejected — verifies the WS upgrade
    // honours audience checks rather than accepting any valid JWT.
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("rcws2").await;

    let ws_url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        urlencode(&seeded.admin.access_token)
    );
    let err = connect_async(&ws_url).await;
    assert!(
        err.is_err(),
        "user JWT must not be accepted for agent role; got Ok"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

async fn enroll_helper(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    machine_id: &str,
    machine_name: &str,
) -> (String, String) {
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
            "machine_name": machine_name,
            "os": "linux",
            "agent_version": "0.1.0",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        ej["agent_id"].as_str().unwrap().to_string(),
        ej["agent_token"].as_str().unwrap().to_string(),
    )
}

fn urlencode(s: &str) -> String {
    // Minimal URL-encoding for the JWT (only `+`, `/`, `=` need escaping).
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

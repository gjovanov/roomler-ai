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
// Phase Y.3: preferred_transport on rc:session.request flows controller → agent
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rc_session_request_forwards_preferred_transport_to_agent() {
    // The browser advertises `preferred_transport: data-channel-vp9-444` on
    // the rc:session.request payload (Phase Y.3 — see
    // ui/src/composables/useRemoteControl.ts). The server's Hub forwards
    // the field verbatim to the agent inside the rc:request envelope so
    // the agent can intersect it with its own AgentCaps.transports and
    // pick a video transport. This test locks the relay path: any
    // future regression that drops the field on the way through will
    // surface here.
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("rcprefxport").await;

    // Enroll + connect the agent so the Hub knows about it. Caps don't
    // matter for this test — the relay forwards `preferred_transport`
    // unconditionally; intersection happens on the agent side.
    let (agent_id, agent_token) =
        enroll_helper(&app, &seeded, "mach-rcprefxport-A", "Y.3 agent").await;
    let agent_ws_url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        urlencode(&agent_token)
    );
    let (mut agent_ws, _) = connect_async(&agent_ws_url).await.expect("agent ws");
    let hello = json!({
        "t": "rc:agent.hello",
        "machine_name": "Y.3 agent",
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
            "max_simultaneous_sessions": 1,
            "transports": ["data-channel-vp9-444"],
        }
    });
    agent_ws
        .send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();

    // Controller WS — uses the admin's user JWT (no `role` query param).
    let ctrl_ws_url = format!(
        "ws://{}/ws?token={}",
        app.addr,
        urlencode(&seeded.admin.access_token)
    );
    let (mut ctrl_ws, _) = connect_async(&ctrl_ws_url).await.expect("controller ws");

    // Drain any startup frames the controller WS might emit (e.g.
    // initial presence). Best-effort timeout — many tenants emit
    // nothing.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(200), ctrl_ws.next()).await;

    // Kick off the session with the new field set.
    let req = json!({
        "t": "rc:session.request",
        "agent_id": agent_id,
        "permissions": "VIEW",
        "preferred_transport": "data-channel-vp9-444",
    });
    ctrl_ws
        .send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    // Read agent-side messages until we see `rc:request` carrying the
    // forwarded preferred_transport, or hit the deadline.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut saw_request = false;
    while tokio::time::Instant::now() < deadline && !saw_request {
        let msg = match tokio::time::timeout(std::time::Duration::from_millis(500), agent_ws.next())
            .await
        {
            Ok(Some(Ok(m))) => m,
            _ => continue,
        };
        let Message::Text(text) = msg else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if v.get("t").and_then(|x| x.as_str()) == Some("rc:request") {
            // Lock the field name + value: a typo or accidental rename
            // server-side would silently break the agent's negotiation
            // logic.
            assert_eq!(
                v.get("preferred_transport").and_then(|x| x.as_str()),
                Some("data-channel-vp9-444"),
                "rc:request must forward preferred_transport verbatim; got {v}"
            );
            saw_request = true;
        }
    }
    assert!(
        saw_request,
        "agent never received rc:request with preferred_transport"
    );
}

#[tokio::test]
async fn rc_session_request_omits_preferred_transport_when_unset() {
    // Older browsers (and the default code path) don't include the
    // `preferred_transport` field. The relay must NOT inject a
    // default value — the agent's negotiation logic distinguishes
    // None ("any transport") from Some("webrtc-video") and the wire
    // format uses serde `skip_serializing_if = "Option::is_none"` to
    // express that. Lock the absence on the wire so a future
    // refactor that helpfully fills in defaults regresses here.
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("rcprefxport2").await;

    let (agent_id, agent_token) =
        enroll_helper(&app, &seeded, "mach-rcprefxport-B", "Y.3 default agent").await;
    let agent_ws_url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        urlencode(&agent_token)
    );
    let (mut agent_ws, _) = connect_async(&agent_ws_url).await.expect("agent ws");
    let hello = json!({
        "t": "rc:agent.hello",
        "machine_name": "Y.3 default agent",
        "os": "linux",
        "agent_version": "0.1.0",
        "displays": [{
            "index": 0, "name": "x", "width_px": 800, "height_px": 600,
            "scale": 1.0, "primary": true,
        }],
        "caps": {
            "hw_encoders": [], "codecs": ["h264"],
            "has_input_permission": false, "supports_clipboard": false,
            "supports_file_transfer": false, "max_simultaneous_sessions": 1,
        }
    });
    agent_ws
        .send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();

    let ctrl_ws_url = format!(
        "ws://{}/ws?token={}",
        app.addr,
        urlencode(&seeded.admin.access_token)
    );
    let (mut ctrl_ws, _) = connect_async(&ctrl_ws_url).await.expect("controller ws");
    let _ = tokio::time::timeout(std::time::Duration::from_millis(200), ctrl_ws.next()).await;

    // Same shape as the previous test, sans preferred_transport.
    let req = json!({
        "t": "rc:session.request",
        "agent_id": agent_id,
        "permissions": "VIEW",
    });
    ctrl_ws
        .send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut saw_request = false;
    while tokio::time::Instant::now() < deadline && !saw_request {
        let msg = match tokio::time::timeout(std::time::Duration::from_millis(500), agent_ws.next())
            .await
        {
            Ok(Some(Ok(m))) => m,
            _ => continue,
        };
        let Message::Text(text) = msg else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if v.get("t").and_then(|x| x.as_str()) == Some("rc:request") {
            assert!(
                v.get("preferred_transport").is_none(),
                "rc:request must omit preferred_transport when unset; got {v}"
            );
            saw_request = true;
        }
    }
    assert!(
        saw_request,
        "agent never received rc:request — server may have rejected the request"
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

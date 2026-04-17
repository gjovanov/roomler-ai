//! End-to-end tests that drive the `roomler-agent` library crate against a
//! live `TestApp`. Unlike the REST-only `remote_control_tests`, these
//! exercise the agent's HTTP enrollment + WSS signaling loop in-process,
//! so a regression in either side (server rename, protocol drift, WS auth)
//! fails here too.

use crate::fixtures::test_app::TestApp;
use roomler_agent::{config::AgentConfig, enrollment, signaling};
use serde_json::{Value, json};
use std::time::Duration;

/// Helper: issue an enrollment token via the admin REST route, then run the
/// agent's own `enrollment::enroll()` to get back an `AgentConfig` pointed
/// at the test server.
async fn enrol_via_agent_lib(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    machine_id: &str,
    machine_name: &str,
) -> AgentConfig {
    // Issue enrollment token (admin path).
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

    // Agent library exchanges it for a real agent config.
    enrollment::enroll(enrollment::EnrollInputs {
        server_url: &app.base_url,
        enrollment_token: et["enrollment_token"].as_str().unwrap(),
        machine_id,
        machine_name,
    })
    .await
    .expect("agent enrollment")
}

#[tokio::test]
async fn agent_library_enrolls_successfully() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("agentlib1").await;

    let cfg = enrol_via_agent_lib(&app, &seeded, "mach-agentlib-1", "Test laptop").await;
    assert!(!cfg.agent_token.is_empty());
    assert_eq!(cfg.tenant_id, seeded.tenant_id);
    assert_eq!(cfg.machine_id, "mach-agentlib-1");
    assert_eq!(cfg.machine_name, "Test laptop");

    // Sanity-check the REST layer sees us.
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
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str().unwrap(), cfg.agent_id);
}

#[tokio::test]
async fn agent_library_connects_and_goes_online() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("agentlib2").await;

    let cfg = enrol_via_agent_lib(&app, &seeded, "mach-agentlib-2", "Online test").await;

    // Start the signaling loop. `run()` loops until shutdown; we just need it
    // to get through one successful connect + hello, then we stop it.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let sig_task = tokio::spawn({
        let cfg = cfg.clone();
        async move {
            let _ = signaling::run(cfg, stop_rx).await;
        }
    });

    // Poll the admin API until the agent's DB row flips to online.
    let agent_id = cfg.agent_id.clone();
    let mut online = false;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let row: Value = app
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
        if row["is_online"].as_bool() == Some(true) {
            assert_eq!(row["status"].as_str(), Some("online"));
            online = true;
            break;
        }
    }
    assert!(online, "agent never transitioned to is_online=true");

    // Shut the agent down. Drop time is fast because the WS select arm
    // watches the shutdown signal.
    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), sig_task).await;
}

#[tokio::test]
async fn agent_library_rejects_bogus_enrollment_token() {
    let app = TestApp::spawn().await;
    let err = enrollment::enroll(enrollment::EnrollInputs {
        server_url: &app.base_url,
        enrollment_token: "not-a-jwt",
        machine_id: "mach-bogus",
        machine_name: "bogus",
    })
    .await
    .expect_err("bogus token must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("401") || msg.contains("rejected"),
        "expected 401/rejected, got: {msg}"
    );
}

#[tokio::test]
async fn agent_library_auto_grants_consent_and_declines_media() {
    // Exercises the full rc:* handshake end-to-end:
    //   - agent sends rc:agent.hello
    //   - controller (simulated by a raw tokio-tungstenite client) requests
    //     a session
    //   - server routes rc:request to the agent
    //   - agent auto-grants consent
    //   - controller receives rc:ready and sends an offer
    //   - agent replies with rc:terminate(Error) since media is not wired
    //   - server relays the terminate back to the controller
    //
    // This is the smoke test that locks in the signaling-only contract
    // until the real WebRTC peer lands.
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("agentlib3").await;
    let cfg = enrol_via_agent_lib(&app, &seeded, "mach-agentlib-3", "Auto consent").await;

    // Spin up the agent library against the test server.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let sig_task = tokio::spawn({
        let cfg = cfg.clone();
        async move {
            let _ = signaling::run(cfg, stop_rx).await;
        }
    });

    // Wait for the agent to go online before starting the controller side,
    // otherwise the session.request would fail with AgentOffline.
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let row: Value = app
            .auth_get(
                &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, cfg.agent_id),
                &seeded.admin.access_token,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if row["is_online"].as_bool() == Some(true) {
            break;
        }
    }

    // Controller opens a user-role WS and sends rc:session.request.
    let ctrl_url = format!(
        "ws://{}/ws?token={}",
        app.addr,
        urlencode(&seeded.admin.access_token)
    );
    let (mut ctrl_ws, _) = connect_async(&ctrl_url).await.expect("controller ws");

    // Drain the initial "connected" event the server pushes.
    let _ = tokio::time::timeout(Duration::from_secs(2), ctrl_ws.next()).await;

    let session_request = json!({
        "t": "rc:session.request",
        "agent_id": cfg.agent_id,
        // bitflags serialises as pipe-separated names — see
        // remote_control::permissions::tests for the lock-in test.
        "permissions": "VIEW | INPUT",
    });
    ctrl_ws
        .send(Message::Text(session_request.to_string().into()))
        .await
        .unwrap();

    // Collect messages until we see rc:terminate (end of the handshake).
    let mut saw_created = false;
    let mut saw_ready = false;
    let mut saw_terminate = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut session_id: Option<String> = None;
    let mut seen: Vec<String> = Vec::new();
    while tokio::time::Instant::now() < deadline && !saw_terminate {
        let msg = match tokio::time::timeout(Duration::from_millis(500), ctrl_ws.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => continue,
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            _ => continue,
        };
        seen.push(text.clone());
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
        match t {
            "rc:session.created" => {
                saw_created = true;
                session_id = extract_oid(&v["session_id"]);
            }
            "rc:ready" => {
                saw_ready = true;
                // Send a stub SDP offer so the agent replies with terminate.
                if let Some(sid) = session_id.as_deref() {
                    let offer = json!({
                        "t": "rc:sdp.offer",
                        "session_id": sid,
                        "sdp": "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n",
                    });
                    ctrl_ws.send(Message::Text(offer.to_string().into())).await.unwrap();
                }
            }
            "rc:terminate" => {
                saw_terminate = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_created,
        "controller never received rc:session.created. seen={seen:#?}"
    );
    assert!(
        saw_ready,
        "controller never received rc:ready. seen={seen:#?}"
    );
    assert!(
        saw_terminate,
        "agent never terminated the session after rc:sdp.offer. seen={seen:#?}"
    );

    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), sig_task).await;
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
}

/// Extract a hex ObjectId. The wire format is raw hex on both REST and WS
/// paths — see `signaling::tests::object_ids_serialise_as_raw_hex_on_wire`.
/// If a regression ever reverts to bson-extended JSON we want this helper
/// to fail loudly, not paper over it.
fn extract_oid(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

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
async fn agent_answers_sdp_offer_with_real_webrtc_peer() {
    // Exercises the full rc:* handshake end-to-end with a real webrtc-rs
    // peer on each side:
    //   - agent sends rc:agent.hello
    //   - "browser" (a second webrtc-rs PC) is opened in this test,
    //     creates an offer with a data channel
    //   - controller sends rc:session.request
    //   - server routes rc:request to the agent
    //   - agent auto-grants consent
    //   - controller receives rc:ready and sends the real offer
    //   - agent creates its PC, replies with rc:sdp.answer carrying a
    //     valid answer SDP
    //   - both sides trickle ICE through the signalling relay
    //
    // Asserts the answer is a well-formed SDP (agent's PC accepted the
    // offer and produced an answer the browser side would apply).
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message};
    use webrtc::api::APIBuilder;
    use webrtc::api::media_engine::MediaEngine;
    use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
    use webrtc::peer_connection::configuration::RTCConfiguration;
    use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("agentlib3").await;
    let cfg = enrol_via_agent_lib(&app, &seeded, "mach-agentlib-3", "Real peer").await;

    // Spin up the agent library.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let sig_task = tokio::spawn({
        let cfg = cfg.clone();
        async move {
            let _ = signaling::run(cfg, stop_rx).await;
        }
    });

    // Wait for the agent to go online.
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

    // Build a browser-side PC and a data channel (so the offer has media).
    let mut me = MediaEngine::default();
    me.register_default_codecs().unwrap();
    let api = APIBuilder::new().with_media_engine(me).build();
    let browser_pc = api
        .new_peer_connection(RTCConfiguration::default())
        .await
        .unwrap();
    let _dc = browser_pc
        .create_data_channel("control", Some(RTCDataChannelInit::default()))
        .await
        .unwrap();
    let browser_offer = browser_pc.create_offer(None).await.unwrap();
    browser_pc
        .set_local_description(browser_offer.clone())
        .await
        .unwrap();

    // Controller WS.
    let ctrl_url = format!(
        "ws://{}/ws?token={}",
        app.addr,
        urlencode(&seeded.admin.access_token)
    );
    let (mut ctrl_ws, _) = connect_async(&ctrl_url).await.expect("controller ws");
    let _ = tokio::time::timeout(Duration::from_secs(2), ctrl_ws.next()).await;

    // Kick off the session.
    ctrl_ws
        .send(Message::Text(
            json!({
                "t": "rc:session.request",
                "agent_id": cfg.agent_id,
                "permissions": "VIEW | INPUT",
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let mut saw_created = false;
    let mut saw_ready = false;
    let mut saw_answer = false;
    let mut saw_agent_ice = false;
    let mut answer_sdp: Option<String> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut session_id: Option<String> = None;

    while tokio::time::Instant::now() < deadline && !saw_answer {
        let msg = match tokio::time::timeout(Duration::from_millis(500), ctrl_ws.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => continue,
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        match v.get("t").and_then(|x| x.as_str()).unwrap_or("") {
            "rc:session.created" => {
                saw_created = true;
                session_id = extract_oid(&v["session_id"]);
            }
            "rc:ready" => {
                saw_ready = true;
                let sid = session_id.clone().expect("session_id from earlier");
                ctrl_ws
                    .send(Message::Text(
                        json!({
                            "t": "rc:sdp.offer",
                            "session_id": sid,
                            "sdp": browser_offer.sdp,
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
            }
            "rc:sdp.answer" => {
                saw_answer = true;
                answer_sdp = v["sdp"].as_str().map(|s| s.to_owned());
            }
            "rc:ice" => {
                // At least one ICE candidate trickled back from the agent's
                // gather phase means the PC is actually running.
                saw_agent_ice = true;
            }
            _ => {}
        }
    }

    assert!(saw_created, "rc:session.created missing");
    assert!(saw_ready, "rc:ready missing");
    assert!(saw_answer, "rc:sdp.answer missing — agent PC failed to build one");

    // Apply the answer on the browser side — proves it's a valid SDP.
    let sdp = answer_sdp.expect("answer SDP");
    assert!(sdp.contains("v=0"), "answer SDP looks malformed: {sdp:.200}");
    let answer = RTCSessionDescription::answer(sdp).expect("parse answer");
    browser_pc
        .set_remote_description(answer)
        .await
        .expect("browser accepts agent's answer");

    // ICE trickle is best-effort in this environment (localhost only, tight
    // ports); we log whether we saw any but don't fail on it.
    if !saw_agent_ice {
        eprintln!("note: no rc:ice from agent within window — acceptable for CI");
    }

    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), sig_task).await;
    let _ = browser_pc.close().await;
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

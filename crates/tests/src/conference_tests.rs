use crate::fixtures::test_app::TestApp;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn create_conference() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("conf1").await;

    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/conference", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "subject": "Team Standup",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["subject"], "Team Standup");
    assert_eq!(json["status"], "Scheduled");
    assert!(json["meeting_code"].as_str().unwrap().len() > 0);
    assert_eq!(json["participant_count"], 0);
}

#[tokio::test]
async fn conference_lifecycle_start_join_leave_end() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("conflife").await;

    // Create conference
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/conference", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({
            "subject": "Sprint Planning",
        }))
        .send()
        .await
        .unwrap();

    let conf: Value = resp.json().await.unwrap();
    let conf_id = conf["id"].as_str().unwrap();

    // Start conference
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/conference/{}/start",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["started"], true);

    // Get conference - check status is InProgress
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/conference/{}",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["status"], "InProgress");

    // Join conference
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/conference/{}/join",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["joined"], true);

    // List participants
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/conference/{}/participant",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let parts: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["user_id"], tenant.admin.id);

    // Leave conference
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/conference/{}/leave",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // End conference
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/conference/{}/end",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["ended"], true);

    // Get conference - check status is Ended
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/conference/{}",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["status"], "Ended");
}

#[tokio::test]
async fn list_conferences() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("conflist").await;

    // Create 2 conferences
    for subject in &["Standup", "Retro"] {
        app.auth_post(
            &format!("/api/tenant/{}/conference", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "subject": subject }))
        .send()
        .await
        .unwrap();
    }

    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/conference", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["total"], 2);
}

// --- Phase 6 mediasoup integration tests ---

#[tokio::test]
async fn conference_start_creates_mediasoup_room() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msoup1").await;

    // Create conference
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/conference", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "subject": "Media Test" }))
        .send()
        .await
        .unwrap();
    let conf: Value = resp.json().await.unwrap();
    let conf_id = conf["id"].as_str().unwrap();

    // Start conference — should create mediasoup room and return rtp_capabilities
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/conference/{}/start",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["started"], true);

    // Verify rtp_capabilities is present and has codecs
    let caps = &json["rtp_capabilities"];
    assert!(caps.is_object(), "rtp_capabilities should be an object");
    assert!(
        caps.get("codecs").is_some(),
        "rtp_capabilities should have codecs"
    );
    let codecs = caps["codecs"].as_array().unwrap();
    assert!(codecs.len() >= 2, "Should have at least opus + VP8 codecs");
}

#[tokio::test]
async fn conference_join_returns_transport_options() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msoup2").await;

    // Create + start conference
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/conference", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "subject": "Transport Test" }))
        .send()
        .await
        .unwrap();
    let conf: Value = resp.json().await.unwrap();
    let conf_id = conf["id"].as_str().unwrap();

    app.auth_post(
        &format!(
            "/api/tenant/{}/conference/{}/start",
            tenant.tenant_id, conf_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Join — should return transport options
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/conference/{}/join",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["joined"], true);

    // Transports are now created via WS media:join, not REST join.
    // Connect WS and send media:join to get transports.
    let ws_url = format!(
        "ws://{}/ws?token={}",
        app.addr, tenant.admin.access_token
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("WS connect failed");

    use futures::StreamExt;
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    ws.next().await; // connected msg

    ws.send(Message::Text(
        serde_json::to_string(&serde_json::json!({
            "type": "media:join",
            "data": { "conference_id": conf_id }
        }))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();

    // Should receive router_capabilities
    let msg = ws.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:router_capabilities");

    // Should receive transport_created
    let msg = ws.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:transport_created");

    let send = &parsed["data"]["send_transport"];
    assert!(send["id"].is_string(), "send_transport should have id");
    assert!(
        send["ice_parameters"].is_object(),
        "send_transport should have ice_parameters"
    );
    assert!(
        send["ice_candidates"].is_array(),
        "send_transport should have ice_candidates"
    );
    assert!(
        send["dtls_parameters"].is_object(),
        "send_transport should have dtls_parameters"
    );

    let recv = &parsed["data"]["recv_transport"];
    assert!(recv["id"].is_string(), "recv_transport should have id");
    assert!(
        recv["ice_parameters"].is_object(),
        "recv_transport should have ice_parameters"
    );

    ws.close(None).await.ok();
}

#[tokio::test]
async fn conference_end_cleans_up_room() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msoup3").await;

    // Create + start + end
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/conference", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "subject": "Cleanup Test" }))
        .send()
        .await
        .unwrap();
    let conf: Value = resp.json().await.unwrap();
    let conf_id = conf["id"].as_str().unwrap();

    app.auth_post(
        &format!(
            "/api/tenant/{}/conference/{}/start",
            tenant.tenant_id, conf_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/conference/{}/end",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["ended"], true);

    // Verify status is Ended
    let resp = app
        .auth_get(
            &format!(
                "/api/tenant/{}/conference/{}",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["status"], "Ended");
}

#[tokio::test]
async fn ws_media_join_signaling() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msoup4").await;

    // Create + start conference
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/conference", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "subject": "WS Test" }))
        .send()
        .await
        .unwrap();
    let conf: Value = resp.json().await.unwrap();
    let conf_id = conf["id"].as_str().unwrap();

    app.auth_post(
        &format!(
            "/api/tenant/{}/conference/{}/start",
            tenant.tenant_id, conf_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Connect WebSocket
    let ws_url = format!(
        "ws://{}/ws?token={}",
        app.addr, tenant.admin.access_token
    );
    let (mut ws, _) = connect_async(&ws_url)
        .await
        .expect("Failed to connect WS");

    // Read initial "connected" message
    let msg = ws.next().await.unwrap().unwrap();
    let connected: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(connected["type"], "connected");

    // REST join first (creates transports)
    app.auth_post(
        &format!(
            "/api/tenant/{}/conference/{}/join",
            tenant.tenant_id, conf_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Send media:join
    let join_msg = serde_json::json!({
        "type": "media:join",
        "data": { "conference_id": conf_id }
    });
    ws.send(Message::Text(serde_json::to_string(&join_msg).unwrap().into()))
        .await
        .unwrap();

    // Should receive media:router_capabilities
    let msg = ws.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:router_capabilities");
    assert!(parsed["data"]["rtp_capabilities"]["codecs"].is_array());

    ws.close(None).await.ok();
}

#[tokio::test]
async fn ws_media_leave_broadcasts_peer_left() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msoup5").await;

    // Create + start conference
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/conference", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "subject": "Leave Test" }))
        .send()
        .await
        .unwrap();
    let conf: Value = resp.json().await.unwrap();
    let conf_id = conf["id"].as_str().unwrap();

    app.auth_post(
        &format!(
            "/api/tenant/{}/conference/{}/start",
            tenant.tenant_id, conf_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // User 1 (admin) connects WS and joins
    let ws_url1 = format!(
        "ws://{}/ws?token={}",
        app.addr, tenant.admin.access_token
    );
    let (mut ws1, _) = connect_async(&ws_url1)
        .await
        .expect("Failed to connect WS1");
    ws1.next().await; // connected msg

    app.auth_post(
        &format!(
            "/api/tenant/{}/conference/{}/join",
            tenant.tenant_id, conf_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    ws1.send(Message::Text(
        serde_json::to_string(&serde_json::json!({
            "type": "media:join",
            "data": { "conference_id": conf_id }
        }))
        .unwrap().into(),
    ))
    .await
    .unwrap();

    // Read router_capabilities for user 1
    let msg = ws1.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:router_capabilities");

    // Read transport_created for user 1
    let msg = ws1.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:transport_created");

    // User 2 (member) connects WS and joins
    let ws_url2 = format!(
        "ws://{}/ws?token={}",
        app.addr, tenant.member.access_token
    );
    let (mut ws2, _) = connect_async(&ws_url2)
        .await
        .expect("Failed to connect WS2");
    ws2.next().await; // connected msg

    app.auth_post(
        &format!(
            "/api/tenant/{}/conference/{}/join",
            tenant.tenant_id, conf_id
        ),
        &tenant.member.access_token,
    )
    .send()
    .await
    .unwrap();

    ws2.send(Message::Text(
        serde_json::to_string(&serde_json::json!({
            "type": "media:join",
            "data": { "conference_id": conf_id }
        }))
        .unwrap().into(),
    ))
    .await
    .unwrap();

    // Read router_capabilities for user 2
    let msg = ws2.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:router_capabilities");

    // Read transport_created for user 2
    let msg = ws2.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:transport_created");

    // User 2 sends media:leave
    ws2.send(Message::Text(
        serde_json::to_string(&serde_json::json!({
            "type": "media:leave",
            "data": { "conference_id": conf_id }
        }))
        .unwrap().into(),
    ))
    .await
    .unwrap();

    // Give a moment for the broadcast to propagate
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // User 1 should receive peer_left
    let msg = ws1.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:peer_left");
    assert_eq!(parsed["data"]["user_id"], tenant.member.id);

    ws1.close(None).await.ok();
    ws2.close(None).await.ok();
}

#[tokio::test]
async fn conference_leave_cleans_up_participant_media() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("msoup6").await;

    // Create + start
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/conference", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "subject": "Leave Cleanup" }))
        .send()
        .await
        .unwrap();
    let conf: Value = resp.json().await.unwrap();
    let conf_id = conf["id"].as_str().unwrap();

    app.auth_post(
        &format!(
            "/api/tenant/{}/conference/{}/start",
            tenant.tenant_id, conf_id
        ),
        &tenant.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    // Join via REST
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/conference/{}/join",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["joined"], true);

    // Connect WS and join media room to create transports
    let ws_url = format!(
        "ws://{}/ws?token={}",
        app.addr, tenant.admin.access_token
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("WS connect failed");

    use futures::StreamExt;
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    ws.next().await; // connected msg

    ws.send(Message::Text(
        serde_json::to_string(&serde_json::json!({
            "type": "media:join",
            "data": { "conference_id": conf_id }
        }))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();

    // Drain router_capabilities + transport_created
    let msg = ws.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:router_capabilities");

    let msg = ws.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:transport_created");

    // Leave — should clean up transports
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/conference/{}/leave",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Re-join via REST
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/conference/{}/join",
                tenant.tenant_id, conf_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["joined"], true);

    // WS media:join again — should create new transports (proving old ones were cleaned up)
    ws.send(Message::Text(
        serde_json::to_string(&serde_json::json!({
            "type": "media:join",
            "data": { "conference_id": conf_id }
        }))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:router_capabilities");

    let msg = ws.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:transport_created", "Should get new transports after re-join");

    ws.close(None).await.ok();
}

// --- Connection-ID isolation tests ---

/// Helper: connect WS, read "connected" message, send media:join, read router_capabilities + transport_created.
/// Returns the WS stream and the transport_created data.
async fn ws_join_media(
    addr: &std::net::SocketAddr,
    token: &str,
    conf_id: &str,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Value,
) {
    let ws_url = format!("ws://{}/ws?token={}", addr, token);
    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("WS connect failed");

    // Read "connected"
    ws.next().await;

    // Send media:join
    ws.send(Message::Text(
        serde_json::to_string(&serde_json::json!({
            "type": "media:join",
            "data": { "conference_id": conf_id }
        }))
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();

    // Read router_capabilities
    let msg = ws.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:router_capabilities");

    // Read transport_created
    let msg = ws.next().await.unwrap().unwrap();
    let transport: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(transport["type"], "media:transport_created");

    (ws, transport)
}

/// Helper: create + start a conference, return conf_id.
async fn create_and_start_conference(app: &TestApp, tenant_id: &str, token: &str, subject: &str) -> String {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/conference", tenant_id),
            token,
        )
        .json(&serde_json::json!({ "subject": subject }))
        .send()
        .await
        .unwrap();
    let conf: Value = resp.json().await.unwrap();
    let conf_id = conf["id"].as_str().unwrap().to_string();

    app.auth_post(
        &format!("/api/tenant/{}/conference/{}/start", tenant_id, conf_id),
        token,
    )
    .send()
    .await
    .unwrap();

    conf_id
}

#[tokio::test]
async fn two_different_users_get_independent_transports() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("connid1").await;
    let conf_id = create_and_start_conference(&app, &tenant.tenant_id, &tenant.admin.access_token, "ConnID Test").await;

    // Both users REST-join
    app.auth_post(
        &format!("/api/tenant/{}/conference/{}/join", tenant.tenant_id, conf_id),
        &tenant.admin.access_token,
    ).send().await.unwrap();

    app.auth_post(
        &format!("/api/tenant/{}/conference/{}/join", tenant.tenant_id, conf_id),
        &tenant.member.access_token,
    ).send().await.unwrap();

    // Both users WS media:join — each gets their own transports
    let (mut ws1, t1) = ws_join_media(&app.addr, &tenant.admin.access_token, &conf_id).await;
    let (mut ws2, t2) = ws_join_media(&app.addr, &tenant.member.access_token, &conf_id).await;

    // Verify different transport IDs (proves separate connection_ids)
    let send1_id = t1["data"]["send_transport"]["id"].as_str().unwrap();
    let send2_id = t2["data"]["send_transport"]["id"].as_str().unwrap();
    assert_ne!(send1_id, send2_id, "Different users should get different transport IDs");

    let recv1_id = t1["data"]["recv_transport"]["id"].as_str().unwrap();
    let recv2_id = t2["data"]["recv_transport"]["id"].as_str().unwrap();
    assert_ne!(recv1_id, recv2_id, "Different users should get different recv transport IDs");

    ws1.close(None).await.ok();
    ws2.close(None).await.ok();
}

#[tokio::test]
async fn same_user_two_connections_get_independent_transports() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("connid2").await;
    let conf_id = create_and_start_conference(&app, &tenant.tenant_id, &tenant.admin.access_token, "Same User Multi-Tab").await;

    // REST join once
    app.auth_post(
        &format!("/api/tenant/{}/conference/{}/join", tenant.tenant_id, conf_id),
        &tenant.admin.access_token,
    ).send().await.unwrap();

    // Same user, two WS connections, each sends media:join
    let (mut ws1, t1) = ws_join_media(&app.addr, &tenant.admin.access_token, &conf_id).await;
    let (mut ws2, t2) = ws_join_media(&app.addr, &tenant.admin.access_token, &conf_id).await;

    // Both should get unique transport IDs (keyed by connection_id, not user_id)
    let send1_id = t1["data"]["send_transport"]["id"].as_str().unwrap();
    let send2_id = t2["data"]["send_transport"]["id"].as_str().unwrap();
    assert_ne!(
        send1_id, send2_id,
        "Same user from two connections must get different transport IDs (connection_id isolation)"
    );

    // Closing ws2 should NOT destroy ws1's transports.
    // ws1 should still be able to communicate.
    ws2.close(None).await.ok();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ws1 may receive media:peer_left (ws2 left the conference) — that's expected
    // Then send a ping to verify ws1 is still alive
    ws1.send(Message::Text(
        serde_json::to_string(&serde_json::json!({ "type": "ping" }))
            .unwrap()
            .into(),
    ))
    .await
    .unwrap();

    // Read messages until we get pong (there may be a peer_left first)
    let mut got_pong = false;
    for _ in 0..5 {
        match tokio::time::timeout(std::time::Duration::from_secs(2), ws1.next()).await {
            Ok(Some(Ok(msg))) => {
                let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
                if parsed["type"] == "pong" {
                    got_pong = true;
                    break;
                }
                // media:peer_left is expected, skip it
            }
            _ => break,
        }
    }
    assert!(got_pong, "ws1 should still be alive after ws2 closes (got pong)");

    ws1.close(None).await.ok();
}

#[tokio::test]
async fn ws_disconnect_notifies_peers_with_peer_left() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("connid3").await;
    let conf_id = create_and_start_conference(&app, &tenant.tenant_id, &tenant.admin.access_token, "Disconnect Notify").await;

    // Both users REST-join
    app.auth_post(
        &format!("/api/tenant/{}/conference/{}/join", tenant.tenant_id, conf_id),
        &tenant.admin.access_token,
    ).send().await.unwrap();

    app.auth_post(
        &format!("/api/tenant/{}/conference/{}/join", tenant.tenant_id, conf_id),
        &tenant.member.access_token,
    ).send().await.unwrap();

    // Both users WS media:join
    let (mut ws1, _) = ws_join_media(&app.addr, &tenant.admin.access_token, &conf_id).await;
    let (ws2, _) = ws_join_media(&app.addr, &tenant.member.access_token, &conf_id).await;

    // User 2 disconnects (drops WS)
    drop(ws2);

    // User 1 should receive peer_left
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let msg = ws1.next().await.unwrap().unwrap();
    let parsed: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(parsed["type"], "media:peer_left");
    assert_eq!(parsed["data"]["user_id"], tenant.member.id);

    ws1.close(None).await.ok();
}

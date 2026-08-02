//! Phase A-1 — cross-pod agent presence truth.
//!
//! Two-pod model: `TestApp::spawn_pair` runs two servers over ONE Mongo
//! database and the one shared Redis (`redis://127.0.0.1:6379`), each with
//! its own pod-local in-memory rc-hub — exactly the prod S6 topology that
//! produced the 2026-08-02 green-but-"not online" incident.

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

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
            "machine_name": "presence test box",
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

/// Connect an agent WS to `app` and complete the hello handshake.
async fn connect_agent(
    app: &TestApp,
    agent_token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let ws_url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        urlencode(agent_token)
    );
    let (mut ws, _) = connect_async(&ws_url).await.expect("ws connect");
    let hello = json!({
        "t": "rc:agent.hello",
        "machine_name": "presence test box",
        "os": "linux",
        "agent_version": "0.1.0",
        "displays": [],
        "caps": {
            "hw_encoders": ["openh264"],
            "codecs": ["h264"],
            "has_input_permission": true,
            "supports_clipboard": true,
            "supports_file_transfer": true,
            "max_simultaneous_sessions": 1,
        }
    });
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .expect("send hello");
    ws
}

/// Fetch one agent's listing row via `app`'s HTTP API.
async fn fetch_agent_row(app: &TestApp, seeded: &SeededTenant, agent_id: &str) -> Value {
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
    resp
}

/// (a) Agent's WS on pod 1 → listing served by pod 2 reads `presence:
/// "online"` through the Redis directory (pod 2's local hub has no entry —
/// pre-A-1 this was the exact green-but-dead disjunction, now it is TRUE
/// reachability knowledge).
#[tokio::test]
async fn presence_online_cross_pod_via_redis() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.redis_pubsub.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let seeded = app1.seed_tenant("presx").await;
    let (agent_id, agent_token) = enroll(&app1, &seeded, "mach-presx-a").await;

    let mut ws = connect_agent(&app1, &agent_token).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    // Sanity: pod 2's LOCAL hub genuinely doesn't know the agent.
    let aid = bson::oid::ObjectId::parse_str(&agent_id).unwrap();
    assert!(!app2.state.rc_hub.is_agent_online(aid));
    assert!(app1.state.rc_hub.is_agent_online(aid));

    let row = fetch_agent_row(&app2, &seeded, &agent_id).await;
    assert_eq!(row["presence"], "online", "row: {row}");
    assert_eq!(row["is_online"], true);

    let _ = ws.close(None).await;
}

/// (b) Re-home race: agent moves pod 1 → pod 2; pod 1's LATE teardown must
/// not release pod 2's fresh Redis claim, and the heartbeat self-heal
/// bounds the Mongo status clobber.
#[tokio::test]
async fn rehome_race_keeps_newest_claim() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.redis_pubsub.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let seeded = app1.seed_tenant("presy").await;
    let (agent_id, agent_token) = enroll(&app1, &seeded, "mach-presy-a").await;
    let aid = bson::oid::ObjectId::parse_str(&agent_id).unwrap();

    // Old socket on pod 1, then the agent re-homes to pod 2.
    let mut ws_old = connect_agent(&app1, &agent_token).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let mut ws_new = connect_agent(&app2, &agent_token).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Tear the OLD socket down LAST — its compare-DEL must no-op against
    // pod 2's newer claim.
    let _ = ws_old.close(None).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    assert!(app2.state.rc_hub.is_agent_online(aid));
    let row = fetch_agent_row(&app1, &seeded, &agent_id).await;
    assert_eq!(
        row["presence"], "online",
        "old teardown must not erase the new claim: {row}"
    );

    // The old teardown may have written status Offline (cross-pod removal
    // is not identity-gated across hubs by design); a heartbeat self-heals
    // it within one beat.
    ws_new
        .send(Message::Text(
            json!({"t": "rc:agent.heartbeat", "rss_mb": 0, "cpu_pct": 0.0, "active_sessions": 0})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let row = fetch_agent_row(&app1, &seeded, &agent_id).await;
    assert_eq!(row["status"], "online", "heartbeat self-heal: {row}");

    let _ = ws_new.close(None).await;
}

/// (c) Server receive-liveness: a silent socket (no heartbeats, no pings —
/// the half-open middlebox shape) is reaped within the shortened deadline;
/// the teardown flips presence away from "online".
#[tokio::test]
async fn rx_deadline_reaps_silent_socket() {
    let app = TestApp::spawn_with_settings(|s| {
        s.rc.ws_rx_deadline_secs = 2;
        s.rc.ws_liveness_tick_secs = 1;
    })
    .await;
    let seeded = app.seed_tenant("presz").await;
    let (agent_id, agent_token) = enroll(&app, &seeded, "mach-presz-a").await;
    let aid = bson::oid::ObjectId::parse_str(&agent_id).unwrap();

    let ws = connect_agent(&app, &agent_token).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(app.state.rc_hub.is_agent_online(aid));

    // Go silent WITHOUT closing (a close frame would be a normal
    // disconnect, not a reap). Leaking the stream keeps the TCP socket
    // open with no traffic — the middlebox half-open shape.
    std::mem::forget(ws);

    // Deadline 2 s, tick 1 s ⇒ reaped in ≤ ~4 s.
    let mut reaped = false;
    for _ in 0..12 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if !app.state.rc_hub.is_agent_online(aid) {
            reaped = true;
            break;
        }
    }
    assert!(
        reaped,
        "silent agent socket was not reaped by the rx deadline"
    );

    let row = fetch_agent_row(&app, &seeded, &agent_id).await;
    assert_ne!(row["presence"], "online", "row: {row}");
    assert_eq!(row["is_online"], false);
}

/// (d) Graceful shutdown sweep: `shutdown_cleanup` cancels local sockets,
/// bulk-offlines their rows, and releases presence.
#[tokio::test]
async fn shutdown_cleanup_marks_local_agents_offline() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("presw").await;
    let (agent_id, agent_token) = enroll(&app, &seeded, "mach-presw-a").await;
    let aid = bson::oid::ObjectId::parse_str(&agent_id).unwrap();

    let _ws = connect_agent(&app, &agent_token).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(app.state.rc_hub.is_agent_online(aid));

    roomler_ai_api::state::shutdown_cleanup(&app.state).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert!(!app.state.rc_hub.is_agent_online(aid));
    let row = fetch_agent_row(&app, &seeded, &agent_id).await;
    assert_ne!(row["presence"], "online", "row: {row}");
    assert_eq!(row["status"], "offline", "row: {row}");
}

/// (e) The "stale" state: heartbeat trail fresh + status Online in Mongo,
/// but NO socket anywhere (the stranded-row shape a hard-killed pod
/// leaves) ⇒ amber `stale`, `is_online: false` — never a green lie.
#[tokio::test]
async fn stale_when_heartbeat_fresh_but_no_socket() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("presv").await;
    let (agent_id, _token) = enroll(&app, &seeded, "mach-presv-a").await;
    let aid = bson::oid::ObjectId::parse_str(&agent_id).unwrap();

    // Fabricate the stranded row directly: status Online + fresh
    // last_seen_at, no WS ever connected.
    use bson::doc;
    app.db
        .collection::<bson::Document>("agents")
        .update_one(
            doc! { "_id": aid },
            doc! { "$set": { "status": "online", "last_seen_at": bson::DateTime::now() } },
        )
        .await
        .unwrap();

    let row = fetch_agent_row(&app, &seeded, &agent_id).await;
    assert_eq!(row["presence"], "stale", "row: {row}");
    assert_eq!(row["is_online"], false, "stale must not read green: {row}");
}

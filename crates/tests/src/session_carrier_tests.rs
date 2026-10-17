// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P9c — the carrier a session rides, on the server side: the agent
//! records it on `rc:session.stats` once the session reaches Active, and the
//! server stores it on `remote_sessions.carrier`. Through a real server, a
//! real agent WebSocket and a real MongoDB.
//!
//! * a sample carrying a carrier code writes it on the session row, beside
//!   `stats`;
//! * a later sample with NO carrier never overwrites it (absent means
//!   "nothing new to say", not "direct"), and one with a non-code value is
//!   dropped (the device is the only writer; a compromised one gets no free
//!   string field);
//! * a session whose agent never recorded a carrier has none — the growth
//!   freeze reads that as `unknown`, never direct (asserted in
//!   `growth_quality_tests`).
//!
//! The row is read straight from Mongo: the carrier has no HTTP route (it is
//! not controller-facing), exactly as `transfer_totals` has none.

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use bson::{Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// Enroll a device and connect its agent WS.
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
            "agent_version": "0.4.140",
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
        "t": "rc:agent.hello", "machine_name": machine_id, "os": "linux", "agent_version": "0.4.140",
        "displays": [{ "index": 0, "name": "x", "width_px": 800, "height_px": 600, "scale": 1.0, "primary": true }],
        "caps": {
            "hw_encoders": [], "codecs": ["h264"], "has_input_permission": false,
            "supports_clipboard": false, "supports_file_transfer": false,
            "max_simultaneous_sessions": 16, "input": ["arbiter"],
        },
    });
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    (agent_id, ws)
}

/// Ask for a session on a fresh controller connection; returns it live.
async fn ask(app: &TestApp, token: &str, agent_id: &str) -> (Ws, String) {
    let url = format!("ws://{}/ws?token={}", app.addr, urlencode(token));
    let (mut ws, _) = connect_async(&url).await.expect("controller ws");
    let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    ws.send(Message::Text(
        json!({ "t": "rc:session.request", "agent_id": agent_id, "permissions": "VIEW" })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(500), ws.next()).await
        else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match v.get("t").and_then(|t| t.as_str()) {
            Some("rc:session.created") => {
                return (ws, v["session_id"].as_str().unwrap().to_string());
            }
            Some("rc:error") => panic!("request refused: {v}"),
            _ => {}
        }
    }
    panic!("no rc:session.created within 5 s");
}

async fn wait_for_session_row(app: &TestApp, seeded: &SeededTenant, session_id: &str) {
    for _ in 0..50 {
        let resp = app
            .client
            .get(app.url(&format!(
                "/api/tenant/{}/session/{session_id}",
                seeded.tenant_id
            )))
            .bearer_auth(&seeded.admin.access_token)
            .send()
            .await
            .unwrap();
        if resp.status().is_success() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("session {session_id} never got its row");
}

/// One `rc:session.stats`, `carrier` present only when `Some`.
async fn report(ws: &mut Ws, session_id: &str, carrier: Option<&str>) {
    let mut msg = json!({
        "t": "rc:session.stats", "session_id": session_id,
        "bytes_sent": 1, "bytes_recv": 2, "fps": 30.0, "rtt_ms": 40.0,
        "keyframe_requests": 0, "input_events": 0,
    });
    if let Some(c) = carrier {
        msg["carrier"] = json!(c);
    }
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .unwrap();
}

/// The stored `carrier`, polled until it reaches `want` (or the timeout).
async fn await_carrier(app: &TestApp, session_id: &str, want: Option<&str>) -> Option<String> {
    let sid = ObjectId::parse_str(session_id).unwrap();
    let mut last = None;
    for _ in 0..50 {
        let row = app
            .db
            .collection::<Document>("remote_sessions")
            .find_one(doc! { "_id": sid })
            .await
            .unwrap();
        last = row.and_then(|d| d.get_str("carrier").ok().map(str::to_string));
        if last.as_deref() == want {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    last
}

#[tokio::test]
async fn the_server_records_the_carrier_a_sample_names_and_never_unlearns_it() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("carrier").await;
    let (agent_id, mut agent) = connect_agent(&app, &seeded, "carrier-host").await;
    let (_ctl, sid) = ask(&app, &seeded.admin.access_token, &agent_id).await;
    wait_for_session_row(&app, &seeded, &sid).await;

    // A sample naming a carrier code writes it on the row.
    report(&mut agent, &sid, Some("overlay_turn")).await;
    assert_eq!(
        await_carrier(&app, &sid, Some("overlay_turn"))
            .await
            .as_deref(),
        Some("overlay_turn"),
        "the carrier the agent recorded is stored"
    );

    // A later sample with NO carrier does not erase it — absent is "nothing
    // new", not "direct".
    report(&mut agent, &sid, None).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        await_carrier(&app, &sid, Some("overlay_turn"))
            .await
            .as_deref(),
        Some("overlay_turn"),
        "an absent carrier never unlearns the stored one"
    );

    // A non-code value is dropped at the socket gate: the row is untouched.
    report(&mut agent, &sid, Some("DROP TABLE sessions")).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        await_carrier(&app, &sid, Some("overlay_turn"))
            .await
            .as_deref(),
        Some("overlay_turn"),
        "a non-code carrier is dropped, not stored"
    );
}

#[tokio::test]
async fn a_session_whose_agent_recorded_no_carrier_has_none() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("nocarrier").await;
    let (agent_id, mut agent) = connect_agent(&app, &seeded, "nocarrier-host").await;
    let (_ctl, sid) = ask(&app, &seeded.admin.access_token, &agent_id).await;
    wait_for_session_row(&app, &seeded, &sid).await;

    // An older agent (or one before the observation landed) sends stats with
    // no carrier: the field is simply absent — the freeze reads that as
    // `unknown`, never direct.
    report(&mut agent, &sid, None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let sid_oid = ObjectId::parse_str(&sid).unwrap();
    let row = app
        .db
        .collection::<Document>("remote_sessions")
        .find_one(doc! { "_id": sid_oid })
        .await
        .unwrap()
        .expect("the session row");
    assert!(row.get("carrier").is_none(), "no carrier recorded: {row:?}");
    // But the rest of the sample landed — the merge ran.
    let stats = row.get_document("stats").unwrap();
    assert_eq!(stats.get_i64("bytes_recv").unwrap(), 2);
}

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-101 P3 — the clipboard and file traffic a device reports on
//! `rc:session.stats`, through real servers, real WebSockets and a real
//! MongoDB.
//!
//! What a device reports lands in `transfer_activity` for a session of THAT
//! device, and only the kind its grant allowed: clipboard traffic needs
//! `CLIPBOARD`, files need `FILES`. Every refusal has its positive control
//! beside it, because a filter that drops everything would pass every
//! "dropped" cell. The clipboard tallies are cumulative, so a repeated
//! sample adds no row and a grown one adds only the difference. A row never
//! carries content or a file name.

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const REMOTE_CONTROL: u64 = 1 << 25;

/// FIPS 180-2's SHA-256 of "abc", and of the empty string: two valid name
/// hashes.
const HASH_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const HASH_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// Enroll a device that serves the clipboard and files, and connect it.
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
            "supports_clipboard": true,
            "supports_file_transfer": true,
            "max_simultaneous_sessions": 16,
            "input": ["arbiter"],
        },
    });
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    (agent_id, ws)
}

/// A session for `permissions`, asked on a FRESH controller connection (a
/// second request on one socket coalesces onto its first session, #1045).
/// The connection is returned so the caller holds the session live.
async fn ask(
    app: &TestApp,
    token: &str,
    agent_id: &str,
    permissions: &str,
) -> (Ws, String, String) {
    let url = format!("ws://{}/ws?token={}", app.addr, urlencode(token));
    let (mut ws, _) = connect_async(&url).await.expect("controller ws");
    let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    let req = json!({
        "t": "rc:session.request",
        "agent_id": agent_id,
        "permissions": permissions,
    });
    ws.send(Message::Text(req.to_string().into()))
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
                let sid = v["session_id"].as_str().unwrap().to_string();
                let granted = v["permissions"].as_str().unwrap_or_default().to_string();
                return (ws, sid, granted);
            }
            Some("rc:error") => panic!("the request was refused outright: {v}"),
            _ => {}
        }
    }
    panic!("no rc:session.created within 5 s");
}

fn holds(granted: &str, bit: &str) -> bool {
    granted.split('|').any(|p| p.trim() == bit)
}

/// The session's row exists once the audit sink has flushed its request
/// (batched, every 200 ms). The clipboard tallies fold into that row.
async fn wait_for_session_row(app: &TestApp, seeded: &SeededTenant, session_id: &str) {
    for _ in 0..50 {
        let resp = app
            .client
            .get(app.url(&format!(
                "/api/tenant/{}/session/{}",
                seeded.tenant_id, session_id
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

/// One `rc:session.stats` with `transfers` (and zeros for the rest).
async fn report(ws: &mut Ws, session_id: &str, transfers: Value) {
    let msg = json!({
        "t": "rc:session.stats",
        "session_id": session_id,
        "bytes_sent": 0,
        "bytes_recv": 0,
        "fps": 0.0,
        "rtt_ms": 0.0,
        "keyframe_requests": 0,
        "input_events": 0,
        "transfers": transfers,
    });
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .unwrap();
}

async fn give_member(app: &TestApp, seeded: &SeededTenant, name: &str, mask: u64) {
    let role: Value = app
        .auth_post(
            &format!("/api/tenant/{}/role", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&json!({ "name": name, "permissions": mask }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let role_id = role["id"].as_str().expect("a role id").to_string();
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/role/{}/assign/{}",
                seeded.tenant_id, role_id, seeded.member.id
            ),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "assign {name}");
}

async fn activity(
    app: &TestApp,
    seeded: &SeededTenant,
    agent_id: &str,
    token: &str,
) -> (u16, Value) {
    let resp = app
        .client
        .get(app.url(&format!(
            "/api/tenant/{}/transfer-activity/{}?per_page=100",
            seeded.tenant_id, agent_id
        )))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Poll until the device's activity reaches `total` rows, then give anything
/// that should have been dropped every chance to (wrongly) land.
async fn settle(app: &TestApp, seeded: &SeededTenant, agent_id: &str, total: u64) -> Value {
    for _ in 0..30 {
        let (status, body) = activity(app, seeded, agent_id, &seeded.admin.access_token).await;
        assert_eq!(status, 200, "{body}");
        if body["total"].as_u64() == Some(total) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, body) = activity(app, seeded, agent_id, &seeded.admin.access_token).await;
    assert_eq!(body["total"], total, "{body}");
    body
}

fn rows_of<'a>(body: &'a Value, kind: &str) -> Vec<&'a Value> {
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["kind"] == kind)
        .collect()
}

#[tokio::test]
async fn transfer_activity_is_kept_only_for_a_session_that_could_carry_it() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr101p3").await;
    let (a, mut a_ws) = connect_agent(&app, &seeded, "mach-fr101-a").await;
    let (b, mut b_ws) = connect_agent(&app, &seeded, "mach-fr101-b").await;

    // A session that may use the clipboard and files, and one that may not.
    let (owner_ws, with, granted) = ask(
        &app,
        &seeded.admin.access_token,
        &a,
        "VIEW | CLIPBOARD | FILES",
    )
    .await;
    assert!(
        holds(&granted, "CLIPBOARD") && holds(&granted, "FILES"),
        "the positive control needs both bits: {granted}"
    );
    give_member(&app, &seeded, "viewer", REMOTE_CONTROL).await;
    let (member_ws, without, granted) = ask(&app, &seeded.member.access_token, &a, "VIEW").await;
    assert!(
        !holds(&granted, "CLIPBOARD") && !holds(&granted, "FILES"),
        "{granted}"
    );
    wait_for_session_row(&app, &seeded, &with).await;
    wait_for_session_row(&app, &seeded, &without).await;

    let traffic = json!({
        "clipboard_to_host": { "count": 2, "bytes": 15 },
        "clipboard_from_host": { "count": 1, "bytes": 7 },
        "files": [
            { "dir": "to_host", "name_sha256": HASH_ABC, "bytes": 1234 },
            { "dir": "from_host", "name_sha256": HASH_EMPTY, "bytes": 99 },
            // Not a hash: a name a compromised host tried to slip in.
            { "dir": "to_host", "name_sha256": "salaries 2026.xlsx", "bytes": 5 },
        ],
    });
    report(&mut a_ws, &with, traffic.clone()).await; // kept: 2 clipboard rows + 2 files
    report(&mut a_ws, &without, traffic.clone()).await; // no CLIPBOARD, no FILES: dropped
    report(&mut b_ws, &with, traffic).await; // another device's session: dropped

    let body = settle(&app, &seeded, &a, 4).await;
    for row in body["items"].as_array().unwrap() {
        assert_eq!(row["session_id"].as_str(), Some(with.as_str()), "{row}");
        assert_eq!(
            row["controller_user_id"].as_str(),
            Some(seeded.admin.id.as_str()),
            "the SERVER's record of the controller: {row}"
        );
        assert!(row["at"].is_string(), "RFC 3339: {row}");
        // No content and no name: these are every field a row has.
        let mut keys: Vec<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "at",
                "bytes",
                "controller_user_id",
                "count",
                "id",
                "kind",
                "name_sha256",
                "session_id"
            ],
            "{row}"
        );
    }
    let to = rows_of(&body, "clipboard_to_host");
    assert_eq!(
        (to.len(), &to[0]["count"], &to[0]["bytes"]),
        (1, &json!(2), &json!(15))
    );
    let from = rows_of(&body, "clipboard_from_host");
    assert_eq!(
        (from.len(), &from[0]["count"], &from[0]["bytes"]),
        (1, &json!(1), &json!(7))
    );
    let up = rows_of(&body, "file_to_host");
    assert_eq!(up.len(), 1, "the row without a hash is dropped");
    assert_eq!(
        (&up[0]["name_sha256"], &up[0]["bytes"]),
        (&json!(HASH_ABC), &json!(1234))
    );
    let down = rows_of(&body, "file_from_host");
    assert_eq!(
        (&down[0]["name_sha256"], &down[0]["count"]),
        (&json!(HASH_EMPTY), &json!(1))
    );

    // The same cumulative reading again adds nothing; a grown one adds only
    // the difference.
    report(
        &mut a_ws,
        &with,
        json!({ "clipboard_to_host": { "count": 2, "bytes": 15 } }),
    )
    .await;
    report(
        &mut a_ws,
        &with,
        json!({ "clipboard_to_host": { "count": 3, "bytes": 20 } }),
    )
    .await;
    let body = settle(&app, &seeded, &a, 5).await;
    let to = rows_of(&body, "clipboard_to_host");
    assert_eq!(to.len(), 2, "{body}");
    assert_eq!(
        (&to[0]["count"], &to[0]["bytes"]),
        (&json!(1), &json!(5)),
        "newest first, and only what the reading added"
    );

    // Device B wrote nothing about A's session.
    let (_, rows_b) = activity(&app, &seeded, &b, &seeded.admin.access_token).await;
    assert_eq!(rows_b["total"], 0, "{rows_b}");
    // The trail is read under VIEW_REMOTE_AUDIT: the member, who holds only
    // REMOTE_CONTROL, is refused.
    let (status, _) = activity(&app, &seeded, &a, &seeded.member.access_token).await;
    assert_eq!(status, 403);
    drop((owner_ws, member_ws));
}

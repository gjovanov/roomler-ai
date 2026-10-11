// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-92 P5 — the org's keep-busy deny, server side.
//!
//! Keep busy is on by default; an org owner can deny it org-wide. The server
//! never enforces that on a data path (the toggle rides the P2P control
//! channel) — it DELIVERS the policy, and the device enforces it. These lock
//! the delivery: who may read and set it, that a device that parses it gets
//! it on connect AND on every change, and that a device that does not (no
//! `keep-busy` word in `AgentCaps.input`) never gets a frame it would log as
//! an unknown `rc:*` at WARN.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::fixtures::test_app::TestApp;
use crate::tunnel_tests::{enroll_agent, read_until, wait_agent_online};

type AgentWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const POLICY: &str = "rc:agent.keep_busy_policy";

/// The hello's caps blob — and a heartbeat's re-announce — advertising `input`.
fn caps(input: &[&str]) -> Value {
    json!({
        "hw_encoders": [],
        "codecs": ["h264"],
        "has_input_permission": true,
        "supports_clipboard": false,
        "supports_file_transfer": false,
        "max_simultaneous_sessions": 4,
        "input": input,
    })
}

/// `rc:agent.heartbeat` re-announcing the caps (FR-43 P2c's `caps` field),
/// as an agent does when its owner flips `keep_busy_enabled`.
async fn heartbeat_caps(ws: &mut AgentWs, input: &[&str]) {
    let hb = json!({
        "t": "rc:agent.heartbeat", "rss_mb": 0, "cpu_pct": 0.0, "active_sessions": 0,
        "caps": caps(input),
    });
    ws.send(Message::Text(hb.to_string().into())).await.unwrap();
}

/// A raw agent socket whose hello advertises `input`.
async fn connect(app: &TestApp, token: &str, machine: &str, input: &[&str]) -> AgentWs {
    let url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        token
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    );
    let (mut ws, _) = connect_async(&url).await.expect("agent ws connect");
    ws.send(Message::Text(
        json!({
            "t": "rc:agent.hello",
            "machine_name": machine,
            "os": "windows",
            "agent_version": "0.4.125",
            "displays": [],
            "caps": caps(input),
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    ws
}

async fn put(app: &TestApp, tid: &str, token: &str, denied: bool) -> reqwest::Response {
    app.auth_put(&format!("/api/tenant/{tid}/keep-busy-settings"), token)
        .json(&json!({ "keep_busy_denied": denied }))
        .send()
        .await
        .unwrap()
}

async fn get(app: &TestApp, tid: &str, token: &str) -> reqwest::Response {
    app.auth_get(&format!("/api/tenant/{tid}/keep-busy-settings"), token)
        .send()
        .await
        .unwrap()
}

/// Any `want` frame within `for_`? (A short bound: proving a NEGATIVE.)
async fn saw_within(ws: &mut AgentWs, want: &str, for_: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + for_;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&text)
                    && v.get("t").and_then(|x| x.as_str()) == Some(want)
                {
                    return true;
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Ok(None) => return false,
            Err(_) => continue,
        }
    }
    false
}

#[tokio::test]
async fn keep_busy_is_allowed_by_default_and_only_an_owner_may_deny_it() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("kbsettings").await;
    let tid = seeded.tenant_id.clone();

    // Default: allowed (every pre-feature tenant row reads `false`).
    let resp = get(&app, &tid, &seeded.admin.access_token).await;
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["keep_busy_denied"], false);

    // A plain member may neither read nor set it.
    assert_eq!(
        put(&app, &tid, &seeded.member.access_token, true)
            .await
            .status()
            .as_u16(),
        403
    );
    assert_eq!(
        get(&app, &tid, &seeded.member.access_token)
            .await
            .status()
            .as_u16(),
        403
    );

    // The owner denies it, and the deny is what reads back.
    let resp = put(&app, &tid, &seeded.admin.access_token, true).await;
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["keep_busy_denied"], true);
    let v: Value = get(&app, &tid, &seeded.admin.access_token)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["keep_busy_denied"], true);
}

#[tokio::test]
async fn a_keep_busy_device_gets_the_policy_on_connect_and_on_every_change() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("kbpush").await;
    let tid = seeded.tenant_id.clone();
    let (agent_id, token) = enroll_agent(&app, &seeded, "kb-push", "KB push").await;
    let mut ws = connect(&app, &token, "KB push", &["arbiter", "keep-busy"]).await;
    wait_agent_online(&app, &seeded, &agent_id).await;

    // On connect — even an ALLOW is sent, so a re-allow made while the device
    // was offline clears the deny it persisted.
    let f = read_until(&mut ws, POLICY)
        .await
        .expect("policy on connect");
    assert_eq!(f["denied"], false, "{f}");

    // On change, to the connected device, at once.
    assert_eq!(
        put(&app, &tid, &seeded.admin.access_token, true)
            .await
            .status()
            .as_u16(),
        200
    );
    let f = read_until(&mut ws, POLICY).await.expect("policy on change");
    assert_eq!(f["denied"], true, "{f}");
    assert_eq!(
        put(&app, &tid, &seeded.admin.access_token, false)
            .await
            .status()
            .as_u16(),
        200
    );
    let f = read_until(&mut ws, POLICY)
        .await
        .expect("policy on the re-allow");
    assert_eq!(f["denied"], false, "{f}");

    // A device that connects while denied learns it on connect.
    assert_eq!(
        put(&app, &tid, &seeded.admin.access_token, true)
            .await
            .status()
            .as_u16(),
        200
    );
    let (agent2, token2) = enroll_agent(&app, &seeded, "kb-late", "KB late").await;
    let mut ws2 = connect(&app, &token2, "KB late", &["arbiter", "keep-busy"]).await;
    wait_agent_online(&app, &seeded, &agent2).await;
    let f = read_until(&mut ws2, POLICY)
        .await
        .expect("policy on a later connect");
    assert_eq!(f["denied"], true, "{f}");
}

#[tokio::test]
async fn a_device_without_the_word_never_gets_the_frame() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("kbnoword").await;
    let tid = seeded.tenant_id.clone();
    let (agent_id, token) = enroll_agent(&app, &seeded, "kb-old", "KB old").await;
    // A pre-FR-92 agent: the arbiter words, no `keep-busy`. A prefix-match bug
    // ("keep" or "keep-busy-v2") must not count either.
    let mut ws = connect(&app, &token, "KB old", &["arbiter", "keep-busy-v2", "keep"]).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    assert_eq!(
        put(&app, &tid, &seeded.admin.access_token, true)
            .await
            .status()
            .as_u16(),
        200
    );
    assert!(
        !saw_within(&mut ws, POLICY, Duration::from_millis(1500)).await,
        "a device that cannot parse the policy must never be sent it"
    );
}

/// The owner switches keep busy back on while the device is connected: its
/// word reappears on a heartbeat, and the org's policy must reach it THEN —
/// a deny made while the word was absent would otherwise never arrive. Only
/// on the reappearance: caps are also re-announced for the record word and a
/// worker's permissions, and those say nothing new about keep busy.
#[tokio::test]
async fn the_word_reappearing_on_a_heartbeat_brings_the_policy_once() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("kbreappear").await;
    let tid = seeded.tenant_id.clone();
    let (agent_id, token) = enroll_agent(&app, &seeded, "kb-owner", "KB owner").await;
    // The owner has keep busy off on this device: no word, so no frame.
    let mut ws = connect(&app, &token, "KB owner", &["arbiter"]).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    assert_eq!(
        put(&app, &tid, &seeded.admin.access_token, true)
            .await
            .status()
            .as_u16(),
        200
    );
    assert!(
        !saw_within(&mut ws, POLICY, Duration::from_millis(800)).await,
        "no word: no frame"
    );

    // The owner turns it back on; the agent re-announces its caps.
    heartbeat_caps(&mut ws, &["arbiter", "keep-busy"]).await;
    let f = read_until(&mut ws, POLICY)
        .await
        .expect("policy when the word reappears");
    assert_eq!(f["denied"], true, "{f}");

    // A re-announce that still carries the word: nothing new to say.
    heartbeat_caps(&mut ws, &["arbiter", "keep-busy"]).await;
    assert!(
        !saw_within(&mut ws, POLICY, Duration::from_millis(1500)).await,
        "only on the reappearance"
    );
}

/// The device's agent row from the tenant's listing, polled (≤ ~5 s) until
/// `ready` holds: the heartbeat and the GET travel different sockets. Slow
/// enough that TestApp's rate limiter never sees a burst.
async fn listed_until(
    app: &TestApp,
    tid: &str,
    token: &str,
    agent_id: &str,
    ready: impl Fn(&Value) -> bool,
) -> Value {
    let mut last = Value::Null;
    for _ in 0..16 {
        let list: Value = app
            .auth_get(&format!("/api/tenant/{tid}/agent"), token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(row) = list["items"]
            .as_array()
            .and_then(|a| a.iter().find(|r| r["id"] == agent_id))
        {
            last = row.clone();
            if ready(&last) {
                return last;
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    last
}

/// P5b — a device's keep busy rides its heartbeat to the device list: shown
/// as the device reports it, and GONE once it stops reporting (an older
/// agent after a rollback, a supervised Mac's daemon) — unknown, never a
/// stale "on" and never an invented "off".
#[tokio::test]
async fn the_device_list_shows_keep_busy_as_the_device_reports_it() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("kbbrief").await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let (agent_id, token) = enroll_agent(&app, &seeded, "kb-brief", "KB brief").await;
    let mut ws = connect(&app, &token, "KB brief", &["arbiter", "keep-busy"]).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    let beat = |brief: Option<Value>| {
        let mut hb = json!({
            "t": "rc:agent.heartbeat", "rss_mb": 0, "cpu_pct": 0.0, "active_sessions": 0,
        });
        if let Some(b) = brief {
            hb["keep_busy"] = b;
        }
        Message::Text(hb.to_string().into())
    };

    ws.send(beat(Some(json!({
        "on": true, "phase": "running", "pattern": "heart", "set_by": "Alice",
    }))))
    .await
    .unwrap();
    let row = listed_until(&app, &tid, &admin, &agent_id, |r| {
        r["keep_busy"]["on"] == true
    })
    .await;
    assert_eq!(row["keep_busy"]["phase"], "running", "{row}");
    assert_eq!(row["keep_busy"]["pattern"], "heart");
    assert_eq!(row["keep_busy"]["set_by"], "Alice");

    // A heartbeat that says nothing: the field goes.
    ws.send(beat(None)).await.unwrap();
    let row = listed_until(&app, &tid, &admin, &agent_id, |r| {
        r.get("keep_busy").is_none()
    })
    .await;
    assert!(row.get("keep_busy").is_none(), "{row}");
}

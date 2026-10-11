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

    // A plain member may not set it. (Reading moved to "any member" in P8:
    // `the_default_pattern_is_validated_and_any_member_may_read` below.)
    assert_eq!(
        put(&app, &tid, &seeded.member.access_token, true)
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

// ── FR-92 P7 — the permanent history ────────────────────────────────

/// `GET …/keep-busy-history` with a query string, as a JSON list.
async fn history(app: &TestApp, tid: &str, token: &str, query: &str) -> reqwest::Response {
    app.auth_get(
        &format!("/api/tenant/{tid}/keep-busy-history{query}"),
        token,
    )
    .send()
    .await
    .unwrap()
}

/// The history, polled (≤ ~5 s, at the cadence the rate limiter tolerates)
/// until `ready` holds. The heartbeat and the GET travel different sockets,
/// so a row written on the agent's read loop is not there the instant the
/// beat was sent.
async fn history_until(
    app: &TestApp,
    tid: &str,
    token: &str,
    query: &str,
    ready: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let mut last = Vec::new();
    for _ in 0..16 {
        let v: Value = history(app, tid, token, query).await.json().await.unwrap();
        last = v.as_array().cloned().unwrap_or_default();
        if ready(&last) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    last
}

/// A heartbeat carrying `brief` as the device's keep-busy claim, or none.
fn beat_with(brief: Option<Value>) -> Message {
    let mut hb = json!({
        "t": "rc:agent.heartbeat", "rss_mb": 0, "cpu_pct": 0.0, "active_sessions": 0,
    });
    if let Some(b) = brief {
        hb["keep_busy"] = b;
    }
    Message::Text(hb.to_string().into())
}

/// The route is MANAGE_AGENTS, and every query is scoped by the tenant:
/// another org's admin is `not_a_member`, and another org's device id —
/// paired with MY tenant — answers with nothing, never with their rows.
#[tokio::test]
async fn the_history_needs_manage_agents_and_never_crosses_a_tenant() {
    let app = TestApp::spawn().await;
    let a = app.seed_tenant("kbhista").await;
    let b = app.seed_tenant("kbhistb").await;

    // Empty, as a list, for the admin.
    let resp = history(&app, &a.tenant_id, &a.admin.access_token, "").await;
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v, json!([]));

    // A plain member: 403, an answer, not a logout.
    assert_eq!(
        history(&app, &a.tenant_id, &a.member.access_token, "")
            .await
            .status()
            .as_u16(),
        403
    );
    // Another org's admin: not a member of A.
    let resp = history(&app, &a.tenant_id, &b.admin.access_token, "").await;
    assert_eq!(resp.status().as_u16(), 403);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"], "not_a_member", "{v}");

    // Make B a row: its owner denies keep busy.
    assert_eq!(
        put(&app, &b.tenant_id, &b.admin.access_token, true)
            .await
            .status()
            .as_u16(),
        200
    );
    let rows = history_until(&app, &b.tenant_id, &b.admin.access_token, "", |r| {
        !r.is_empty()
    })
    .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["kind"], "org_denied");
    // A still has none — not even by asking for B's "device" under A.
    let fake_agent = bson::oid::ObjectId::new().to_hex();
    let v: Value = history(
        &app,
        &a.tenant_id,
        &a.admin.access_token,
        &format!("?agent_id={fake_agent}"),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(v, json!([]));

    // Bad inputs are 400, not 500.
    assert_eq!(
        history(&app, &a.tenant_id, &a.admin.access_token, "?agent_id=nope")
            .await
            .status()
            .as_u16(),
        400
    );
    assert_eq!(
        history(
            &app,
            &a.tenant_id,
            &a.admin.access_token,
            "?before=yesterday"
        )
        .await
        .status()
        .as_u16(),
        400
    );
}

/// A PUT records a row ONLY when a value changed, naming the admin who
/// changed it — the server's own decision (`source: server`).
#[tokio::test]
async fn a_put_records_only_a_change_and_names_the_admin() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("kbhistput").await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();

    // deny → one row
    assert_eq!(put(&app, &tid, &admin, true).await.status().as_u16(), 200);
    let rows = history_until(&app, &tid, &admin, "", |r| !r.is_empty()).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["kind"], "org_denied");
    assert_eq!(rows[0]["source"], "server");
    assert_eq!(rows[0]["actor_user_id"], seeded.admin.id);
    assert_eq!(
        rows[0]["actor_name"].as_str().unwrap_or(""),
        format!("{} Admin", seeded.tenant_slug),
        "the admin's display name, denormalised at write time"
    );
    assert!(
        rows[0].get("agent_id").is_none(),
        "an org row names no device"
    );

    // The same deny again → nothing new.
    assert_eq!(put(&app, &tid, &admin, true).await.status().as_u16(), 200);
    // re-allow → one more
    assert_eq!(put(&app, &tid, &admin, false).await.status().as_u16(), 200);
    let rows = history_until(&app, &tid, &admin, "", |r| r.len() >= 2).await;
    assert_eq!(
        rows.len(),
        2,
        "the repeated deny must not have made a row: {rows:?}"
    );
    assert_eq!(rows[0]["kind"], "org_allowed", "newest first");
    assert_eq!(rows[1]["kind"], "org_denied");

    // P8: the default pattern, on change only.
    let set = |pattern: Value| {
        app.auth_put(&format!("/api/tenant/{tid}/keep-busy-settings"), &admin)
            .json(&json!({ "keep_busy_default_pattern": pattern }))
            .send()
    };
    assert_eq!(set(json!("heart")).await.unwrap().status().as_u16(), 200);
    assert_eq!(set(json!("heart")).await.unwrap().status().as_u16(), 200);
    assert_eq!(set(json!(null)).await.unwrap().status().as_u16(), 200);
    let rows = history_until(&app, &tid, &admin, "", |r| r.len() >= 4).await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(rows[0]["kind"], "org_default_pattern");
    assert!(
        rows[0].get("pattern").is_none(),
        "cleared = no pattern: {}",
        rows[0]
    );
    assert_eq!(rows[1]["kind"], "org_default_pattern");
    assert_eq!(rows[1]["pattern"], "heart");

    // Paging: `limit` and `before` (the last row's `at`), newest first.
    let page: Vec<Value> = history(&app, &tid, &admin, "?limit=2")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0]["kind"], "org_default_pattern");
    let before = page[1]["at"].as_str().unwrap().to_string();
    let rest: Vec<Value> = history(&app, &tid, &admin, &format!("?limit=200&before={before}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(rest.len(), 2, "{rest:?}");
    assert_eq!(rest[0]["kind"], "org_allowed");
    assert_eq!(rest[1]["kind"], "org_denied");
    // The device filter matches no org row.
    let fake_agent = bson::oid::ObjectId::new().to_hex();
    let none: Vec<Value> = history(&app, &tid, &admin, &format!("?agent_id={fake_agent}"))
        .await
        .json()
        .await
        .unwrap();
    assert!(none.is_empty());
}

/// Device rows are DERIVED from consecutive heartbeats: an on and an off
/// each make one row (the device's claim, `source: device`), a repeated beat
/// makes none, a beat that says nothing makes none (unknown is not off),
/// and the same keep busy seen again after a gap in reporting is not a new
/// one (the restart dedupe).
#[tokio::test]
async fn heartbeat_edges_make_the_device_rows() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("kbhistdev").await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let (agent_id, token) = enroll_agent(&app, &seeded, "kb-hist", "KB hist").await;
    let mut ws = connect(&app, &token, "KB hist", &["arbiter", "keep-busy"]).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    let mine = format!("?agent_id={agent_id}");
    let on = |pattern: &str, who: &str| json!({ "on": true, "phase": "running", "pattern": pattern, "set_by": who });
    let off = json!({ "on": false, "phase": "off", "reason": "stopped_by_controller" });

    // unknown → on: a row, naming the device, the pattern and who.
    ws.send(beat_with(Some(on("heart", "Alice"))))
        .await
        .unwrap();
    let rows = history_until(&app, &tid, &admin, &mine, |r| !r.is_empty()).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["kind"], "on");
    assert_eq!(rows[0]["source"], "device");
    assert_eq!(rows[0]["agent_id"], agent_id);
    assert_eq!(rows[0]["agent_name"], "KB hist");
    assert_eq!(rows[0]["pattern"], "heart");
    assert_eq!(rows[0]["actor_name"], "Alice");
    assert!(
        rows[0].get("actor_user_id").is_none(),
        "a device reports a name, not an identity"
    );

    // on → on (repeated beat): nothing. on → absent: nothing (unknown ≠ off).
    ws.send(beat_with(Some(on("heart", "Alice"))))
        .await
        .unwrap();
    ws.send(beat_with(None)).await.unwrap();
    // absent → on, the SAME keep busy (pattern + who): the restart dedupe.
    ws.send(beat_with(Some(on("heart", "Alice"))))
        .await
        .unwrap();
    // Let those three land: the device list shows the last one.
    listed_until(&app, &tid, &admin, &agent_id, |r| {
        r["keep_busy"]["on"] == true
    })
    .await;
    // on → absent, then absent → on with a DIFFERENT pattern: a new row.
    ws.send(beat_with(None)).await.unwrap();
    listed_until(&app, &tid, &admin, &agent_id, |r| {
        r.get("keep_busy").is_none()
    })
    .await;
    ws.send(beat_with(Some(on("star", "Bob")))).await.unwrap();
    let rows = history_until(&app, &tid, &admin, &mine, |r| r.len() >= 2).await;
    assert_eq!(
        rows.len(),
        2,
        "exactly one more row — the dedupe and the no-edges made none: {rows:?}"
    );
    assert_eq!(rows[0]["pattern"], "star");
    assert_eq!(rows[0]["actor_name"], "Bob");

    // on → off: a row with the device's reason. off → off: nothing.
    ws.send(beat_with(Some(off.clone()))).await.unwrap();
    ws.send(beat_with(Some(off.clone()))).await.unwrap();
    let rows = history_until(&app, &tid, &admin, &mine, |r| r.len() >= 3).await;
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0]["kind"], "off");
    assert_eq!(rows[0]["reason"], "stopped_by_controller");
    assert!(rows[0].get("pattern").is_none());

    // off → absent → off: nothing either way; a final beat that IS an edge
    // proves the others wrote nothing in between.
    ws.send(beat_with(None)).await.unwrap();
    ws.send(beat_with(Some(off))).await.unwrap();
    ws.send(beat_with(Some(on("wave", "Alice")))).await.unwrap();
    let rows = history_until(&app, &tid, &admin, &mine, |r| r.len() >= 4).await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(rows[0]["kind"], "on");
    assert_eq!(rows[0]["pattern"], "wave");
    // on → absent → off: the off happened while the device was not
    // reporting, and the history still says on — so it is recorded now.
    ws.send(beat_with(None)).await.unwrap();
    listed_until(&app, &tid, &admin, &agent_id, |r| {
        r.get("keep_busy").is_none()
    })
    .await;
    ws.send(beat_with(Some(
        json!({ "on": false, "phase": "off", "reason": "stopped_locally" }),
    )))
    .await
    .unwrap();
    let rows = history_until(&app, &tid, &admin, &mine, |r| r.len() >= 5).await;
    assert_eq!(rows.len(), 5, "{rows:?}");
    assert_eq!(rows[0]["kind"], "off");
    assert_eq!(rows[0]["reason"], "stopped_locally");
    // The org-wide listing carries the device rows too.
    let all: Vec<Value> = history(&app, &tid, &admin, "").await.json().await.unwrap();
    assert_eq!(all.len(), 5);
}

// ── FR-92 P8 — the default pattern ──────────────────────────────────

/// Validated by equality against the agent's wire names, cleared by an
/// explicit `null`, left alone by an absent field — and READABLE by any
/// member, because a viewer pre-selects it. Setting stays MANAGE_TENANT.
#[tokio::test]
async fn the_default_pattern_is_validated_and_any_member_may_read() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("kbdefault").await;
    let other = app.seed_tenant("kbdefaultx").await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let set = |pattern: Value| {
        app.auth_put(&format!("/api/tenant/{tid}/keep-busy-settings"), &admin)
            .json(&json!({ "keep_busy_default_pattern": pattern }))
            .send()
    };

    // Unset by default, and a plain member reads it.
    let resp = get(&app, &tid, &seeded.member.access_token).await;
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["keep_busy_denied"], false);
    assert!(v["keep_busy_default_pattern"].is_null(), "{v}");
    // A non-member does not: `not_a_member`, the one 403 with a navigation.
    let resp = get(&app, &tid, &other.admin.access_token).await;
    assert_eq!(resp.status().as_u16(), 403);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"], "not_a_member", "{v}");

    // Every real pattern is accepted; a near miss is 400.
    for p in [
        "circle",
        "triangle",
        "square",
        "star",
        "figure8",
        "heart",
        "spirograph",
        "lissajous",
        "rose",
        "wave",
        "wander",
        "subtle",
        "shuffle",
    ] {
        assert_eq!(set(json!(p)).await.unwrap().status().as_u16(), 200, "{p}");
    }
    for p in ["hearts", "hea", "Heart", "circle ", ""] {
        assert_eq!(set(json!(p)).await.unwrap().status().as_u16(), 400, "{p:?}");
    }
    assert_eq!(
        set(json!(42)).await.unwrap().status().as_u16(),
        422,
        "not a string"
    );
    // Nothing to set is a refusal, not a silent no-op.
    assert_eq!(
        app.auth_put(&format!("/api/tenant/{tid}/keep-busy-settings"), &admin)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        400
    );
    // The last accepted one is what reads back, for the member too.
    let v: Value = get(&app, &tid, &seeded.member.access_token)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["keep_busy_default_pattern"], "shuffle");
    assert_eq!(v["keep_busy_denied"], false);

    // The pre-P8 body leaves the default alone, and sets the deny.
    let resp = put(&app, &tid, &admin, true).await;
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["keep_busy_denied"], true);
    assert_eq!(v["keep_busy_default_pattern"], "shuffle");
    // And a pattern-only body leaves the deny alone.
    let v: Value = set(json!("rose")).await.unwrap().json().await.unwrap();
    assert_eq!(v["keep_busy_denied"], true);
    assert_eq!(v["keep_busy_default_pattern"], "rose");
    // `null` clears it to the product default.
    let v: Value = set(json!(null)).await.unwrap().json().await.unwrap();
    assert!(v["keep_busy_default_pattern"].is_null(), "{v}");
    assert_eq!(v["keep_busy_denied"], true);

    // A member may read, never set.
    assert_eq!(
        app.auth_put(
            &format!("/api/tenant/{tid}/keep-busy-settings"),
            &seeded.member.access_token
        )
        .json(&json!({ "keep_busy_default_pattern": "heart" }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16(),
        403
    );
}

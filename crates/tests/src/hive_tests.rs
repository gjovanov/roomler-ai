// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P0b — the server side of an agent session's lifecycle.
//!
//! Driven through a RAW agent socket, like FR-83's grant-ack tests: no
//! daemon runs sessions yet (P0c), and what these lock is the SERVER — that a
//! start reaches only a device that runs sessions, that the caller is
//! answered with the device's own words, that a frame from the wrong device
//! or an old fence changes nothing, that a device which was away is told
//! what it missed, and that removing a member or a device ends what ran for
//! it. A peer that decides exactly when — and whether — it answers is the
//! only way to observe those.

use std::time::{Duration, Instant};

use bson::{Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::tunnel_tests::{enroll_agent, read_until, wait_agent_online};
use roomler_ai_remote_control::hive::hive_limits;

type AgentWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// What a Hive-capable build advertises…
const RUNS_HIVE: &[&str] = &["exec", "hive"];
/// …and what every build before it does.
const NO_HIVE: &[&str] = &["exec", "ssh"];

/// `HIVE_RUN` — bit 32, the first permission above 31.
const HIVE_RUN: u64 = 1 << 32;

/// How long the device sits on a start before answering — far longer than a
/// same-host round trip, so an answer that did not wait for the device
/// cannot arrive after it by accident.
const HOLD: Duration = Duration::from_millis(1200);

struct Device {
    agent_id: String,
    token: String,
    machine: String,
    ws: AgentWs,
}

async fn hive_app() -> TestApp {
    // The module's switch defaults OFF; every test here but the first
    // turns it on.
    TestApp::spawn_with_settings(|s| s.modules.hive = true).await
}

async fn connect(app: &TestApp, token: &str, machine: &str, rpc: &[&str]) -> AgentWs {
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
            "os": "linux",
            "agent_version": "0.4.117",
            "displays": [],
            "caps": {
                "hw_encoders": [],
                "codecs": ["h264"],
                "has_input_permission": true,
                "supports_clipboard": false,
                "supports_file_transfer": false,
                "max_simultaneous_sessions": 4,
                "rpc": rpc,
            }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    ws
}

/// Enrol a device in `seeded`'s org and connect a raw socket advertising
/// `rpc`; returns once the server holds it.
async fn device(app: &TestApp, seeded: &SeededTenant, machine: &str, rpc: &[&str]) -> Device {
    let (agent_id, token) = enroll_agent(app, seeded, machine, machine).await;
    let ws = connect(app, &token, machine, rpc).await;
    wait_agent_online(app, seeded, &agent_id).await;
    Device {
        agent_id,
        token,
        machine: machine.to_string(),
        ws,
    }
}

async fn send(ws: &mut AgentWs, frame: Value) {
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// `POST …/hive/session` once, as `token`.
async fn post_start(app: &TestApp, tid: &str, token: &str, device_id: &str, folder: &str) -> Value {
    let resp = app
        .auth_post(&format!("/api/tenant/{tid}/hive/session"), token)
        .json(&json!({ "device_id": device_id, "folder": folder }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "start");
    resp.json().await.unwrap()
}

/// `POST …/hive/session`, retrying ONLY the refusals that come from racing
/// the device's registration: the row reads online a moment before the Hub
/// has recorded the connection's capabilities (`update_hello` precedes
/// `register_agent`). Both are refused before any session is created and
/// before the ceiling spends a token, so a retry is safe.
async fn start(app: &TestApp, tid: &str, token: &str, device_id: &str, folder: &str) -> Value {
    for _ in 0..40 {
        let body = post_start(app, tid, token, device_id, folder).await;
        let racing = body["outcome"] == "refused"
            && body["session"].is_null()
            && matches!(
                body["reason"].as_str(),
                Some("device_offline" | "device_unsupported")
            );
        if !racing {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the Hub never recorded the device as running Hive sessions");
}

async fn get_session(app: &TestApp, tid: &str, token: &str, sid: &str) -> (u16, Value) {
    let resp = app
        .auth_get(&format!("/api/tenant/{tid}/hive/session/{sid}"), token)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Poll a session until its status reads `want` — the device's reports are
/// applied by a per-connection task, after the frame is read.
async fn wait_status(app: &TestApp, tid: &str, token: &str, sid: &str, want: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (code, s) = get_session(app, tid, token, sid).await;
        assert_eq!(code, 200, "session read");
        if s["status"] == want {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "session {sid} never reached `{want}`: {s}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn stop(app: &TestApp, tid: &str, token: &str, sid: &str) -> (u16, Value) {
    let resp = app
        .auth_post(&format!("/api/tenant/{tid}/hive/session/{sid}/stop"), token)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The stored session, read past the API (for a removed member's).
async fn stored(app: &TestApp, sid: &str) -> Document {
    app.db
        .collection::<Document>("agent_sessions")
        .find_one(doc! { "_id": ObjectId::parse_str(sid).unwrap() })
        .await
        .unwrap()
        .expect("the session record")
}

async fn wait_offline(app: &TestApp, seeded: &SeededTenant, agent_id: &str) {
    for _ in 0..100 {
        let row: Value = app
            .auth_get(
                &format!("/api/tenant/{}/agent/{agent_id}", seeded.tenant_id),
                &seeded.admin.access_token,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if row["status"].as_str() != Some("online") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("agent {agent_id} never went offline");
}

/// Grant the seeded MEMBER a role holding `HIVE_RUN` (and nothing else).
async fn grant_hive_run_to_member(app: &TestApp, seeded: &SeededTenant) {
    let role: Value = app
        .auth_post(
            &format!("/api/tenant/{}/role", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&json!({
            "name": "hive-runner",
            "description": "starts agent sessions",
            "permissions": HIVE_RUN,
            "position": 60,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(role["permissions"], HIVE_RUN, "{role}");
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/role/{}/assign/{}",
                seeded.tenant_id,
                role["id"].as_str().unwrap(),
                seeded.member.id
            ),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "assign: {:?}", resp.status());
}

/// The session room's messages as `token` reads them: (status, items).
async fn room_messages(app: &TestApp, tid: &str, token: &str, room: &str) -> (u16, Vec<Value>) {
    let resp = app
        .auth_get(
            &format!("/api/tenant/{tid}/room/{room}/message?per_page=100"),
            token,
        )
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    (status, v["items"].as_array().cloned().unwrap_or_default())
}

/// Poll the session room until `pred` holds for its messages — the device's
/// reports are applied, and their notes posted, by a task after the frame.
async fn wait_messages(
    app: &TestApp,
    tid: &str,
    token: &str,
    room: &str,
    what: &str,
    pred: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (code, items) = room_messages(app, tid, token, room).await;
        assert_eq!(code, 200, "the owner reads the session's room");
        if pred(&items) {
            return items;
        }
        assert!(Instant::now() < deadline, "{what}: {items:#?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn bound_to(m: &Value, reference: &str) -> bool {
    m["binding"]["module"] == "hive" && m["binding"]["ref"] == reference
}

/// The module's switch is the one that defaults OFF: a roll that ships the
/// code serves none of it until an operator turns it on.
#[tokio::test]
async fn the_module_is_off_by_default_and_serves_nothing() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("hiveoff").await;
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/hive/session", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&json!({ "device_id": ObjectId::new().to_hex(), "folder": "/srv" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "not mounted ⇒ no route");
}

// ─── P1g — the organizations agent sessions serve (`hive.tenants`) ─────────

/// A second server over `first`'s database serving only the organizations
/// listed — as `first` restarted with `hive.tenants` set. The list alone
/// switches the module on, with no `modules.hive`: the form a hosted server
/// uses, so these tests hold the one prod runs.
async fn serving_only(first: &TestApp, tenants: &str) -> TestApp {
    let db = first.db.name().to_string();
    let tenants = tenants.to_string();
    TestApp::spawn_with_settings(move |s| {
        s.database.name = db;
        s.hive.tenants = tenants;
        // Polls, like `hive_app_polling`: the default limiter answers a
        // poll loop 429 before the condition it waits for.
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await
}

/// `hive.tenants` set: its organizations get every route; any other is
/// answered as if the module were not there for it, and someone outside an
/// organization learns nothing about whether it is served.
#[tokio::test]
async fn an_organization_hive_does_not_serve_sees_none_of_it() {
    let first = hive_app().await;
    let a = first.seed_tenant("hiveserved").await;
    let b = first.seed_tenant("hiveunserved").await;
    let app = serving_only(&first, &a.tenant_id).await;
    let get = |tid: &str, path: &str, token: &str| {
        let url = format!("/api/tenant/{tid}/hive{path}");
        app.auth_get(&url, token).send()
    };

    // A — served: the question, the list, the brain.
    let (code, body) =
        status_and_json(get(&a.tenant_id, "", &a.admin.access_token).await.unwrap()).await;
    assert_eq!(
        (code, body["enabled"].clone()),
        (200, json!(true)),
        "{body}"
    );
    for path in ["/session", "/brain"] {
        let resp = get(&a.tenant_id, path, &a.admin.access_token)
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200, "{path}");
    }

    // B — not served: every route, read or write, is not found, in words.
    let (code, body) =
        status_and_json(get(&b.tenant_id, "", &b.admin.access_token).await.unwrap()).await;
    assert_eq!(code, 404, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("not available"),
        "{body}"
    );
    for path in ["/session", "/brain"] {
        let resp = get(&b.tenant_id, path, &b.admin.access_token)
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404, "{path}");
    }
    let start = app
        .auth_post(
            &format!("/api/tenant/{}/hive/session", b.tenant_id),
            &b.admin.access_token,
        )
        .json(&json!({ "device_id": ObjectId::new().to_hex(), "folder": "/srv" }))
        .send()
        .await
        .unwrap();
    assert_eq!(start.status().as_u16(), 404, "no start for an unserved org");
    let keep = app
        .auth_post(
            &format!("/api/tenant/{}/hive/brain", b.tenant_id),
            &b.admin.access_token,
        )
        .json(&json!({ "scope": "org", "text": "kept nowhere" }))
        .send()
        .await
        .unwrap();
    assert_eq!(keep.status().as_u16(), 404, "no fact for an unserved org");

    // The list alone switched the module on, and the server says so.
    let caps: Value = app
        .client
        .get(app.url("/api/capabilities"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed = |key: &str| caps[key].as_array().unwrap().iter().any(|m| m == "hive");
    assert!(listed("modules") && !listed("switched_off"), "{caps}");

    // Outside A: the answer is about membership, never about the gate.
    let (code, body) =
        status_and_json(get(&a.tenant_id, "", &b.admin.access_token).await.unwrap()).await;
    assert_eq!(
        (code, body["error"].clone()),
        (403, json!("not_a_member")),
        "{body}"
    );
}

/// An organization taken out of `hive.tenants`: when one of its devices
/// connects, what it still runs for that organization ends — the device is
/// told to stop, the record says why, and nothing of it can be read.
#[tokio::test]
async fn a_device_of_an_organization_no_longer_served_is_told_to_stop() {
    let first = hive_app_polling().await;
    let a = first.seed_tenant("hivestays").await;
    let b = first.seed_tenant("hiveleaves").await;
    let (tid, token) = (b.tenant_id.clone(), b.admin.access_token.clone());
    let mut dev = device(&first, &b, "hive-leaves", RUNS_HIVE).await;
    let sid = started_session(&first, &tid, &token, &mut dev).await;
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "idle"}),
    )
    .await;
    wait_status(&first, &tid, &token, &sid, "idle").await;
    drop(dev.ws);
    wait_offline(&first, &b, &dev.agent_id).await;

    // The server restarted without B in the list; B's device comes back.
    let app = serving_only(&first, &a.tenant_id).await;
    let mut ws = connect(&app, &dev.token, &dev.machine, RUNS_HIVE).await;
    let order = read_until(&mut ws, "rc:hive.stop")
        .await
        .expect("the device is told to stop what it runs for an unserved org");
    assert_eq!(order["session_id"], sid);
    assert_eq!(order["reason"], "hive_not_enabled");

    let s = stored(&app, &sid).await;
    assert_eq!(s.get_str("status").unwrap(), "ended");
    assert_eq!(s.get_str("end_reason").unwrap(), "hive_not_enabled");
    let (code, _) = get_session(&app, &tid, &token, &sid).await;
    assert_eq!(code, 404, "an unserved org's session cannot be read");
}

/// The whole round trip. The caller is answered only after the device
/// decided (held for [`HOLD`]); the start carries metadata and names no
/// account; the device's run states move the record; a stop reaches the
/// device and its `ended` closes the session as `stopped`.
#[tokio::test]
async fn a_start_reaches_the_device_and_the_caller_hears_the_device() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiveflow").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-flow", RUNS_HIVE).await;

    let caller = async {
        let body = start(&app, &tid, &token, &dev.agent_id, "/home/dev/src/app").await;
        (body, Instant::now())
    };
    let target = async {
        let frame = read_until(&mut dev.ws, "rc:hive.start")
            .await
            .expect("the device receives rc:hive.start");
        tokio::time::sleep(HOLD).await;
        let acked_at = Instant::now();
        send(
            &mut dev.ws,
            json!({
                "t": "rc:hive.start_ack",
                "session_id": frame["session_id"],
                "fence": frame["fence"],
                "account": "dev",
            }),
        )
        .await;
        (frame, acked_at)
    };
    let ((body, answered_at), (frame, acked_at)) = tokio::join!(caller, target);

    assert!(
        answered_at >= acked_at,
        "the caller was answered {:?} BEFORE the device decided",
        acked_at - answered_at
    );
    assert_eq!(body["outcome"], "accepted", "{body}");
    let s = &body["session"];
    assert_eq!(s["status"], "starting", "accepted, harness not up yet: {s}");
    assert_eq!(s["account"], "dev", "the device's mapping is shown: {s}");
    assert_eq!(s["title"], "app", "the folder's last component: {s}");
    let sid = s["id"].as_str().unwrap().to_string();

    // The frame: who, where, which fence — and nothing else.
    assert_eq!(frame["session_id"], sid);
    assert_eq!(frame["fence"], 1);
    assert_eq!(frame["resume"], false);
    assert_eq!(frame["harness"], "claude-code");
    assert_eq!(frame["folder"], "/home/dev/src/app");
    assert_eq!(frame["user_id"], seeded.admin.id);
    assert_eq!(frame["harness_session"], s["harness_session"]);
    assert!(
        uuid::Uuid::parse_str(frame["harness_session"].as_str().unwrap()).is_ok(),
        "the harness id is a UUID: {frame}"
    );
    for absent in ["prompt", "account", "env", "context", "settings"] {
        assert!(frame.get(absent).is_none(), "`{absent}` in {frame}");
    }

    // Run states move the record, in order.
    for state in ["idle", "running", "awaiting_approval", "running", "idle"] {
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": state}),
        )
        .await;
    }
    wait_status(&app, &tid, &token, &sid, "idle").await;

    // A state this build cannot name keeps what it knew.
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "compacting"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        get_session(&app, &tid, &token, &sid).await.1["status"],
        "idle"
    );

    // Stop: the device is told, and its `ended` closes the session.
    let (code, stopped) = stop(&app, &tid, &token, &sid).await;
    assert_eq!(code, 200);
    assert_eq!(stopped["outcome"], "stopping", "{stopped}");
    let order = read_until(&mut dev.ws, "rc:hive.stop")
        .await
        .expect("the device receives rc:hive.stop");
    assert_eq!(order["session_id"], sid);
    assert_eq!(order["fence"], 1);
    assert_eq!(order["reason"], "owner");

    // An `idle` that crossed the stop on the wire does not undo it.
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "idle"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        get_session(&app, &tid, &token, &sid).await.1["status"],
        "stopping"
    );

    send(
        &mut dev.ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "ended",
               "detail": "stopped by request"}),
    )
    .await;
    let s = wait_status(&app, &tid, &token, &sid, "ended").await;
    assert_eq!(s["end_reason"], "stopped", "{s}");
    assert!(s["ended_at"].is_string(), "{s}");

    // Stopping again is idempotent and touches nothing.
    let (_, again) = stop(&app, &tid, &token, &sid).await;
    assert_eq!(again["outcome"], "ended");

    // The audit holds the server's two decisions — and no content.
    let audit: Vec<Document> = app
        .db
        .collection::<Document>("hive_audit")
        .find(doc! { "session_id": ObjectId::parse_str(&sid).unwrap() })
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    let mut actions: Vec<(String, String)> = audit
        .iter()
        .map(|d| {
            (
                d.get_str("action").unwrap().to_string(),
                d.get_str("outcome").unwrap().to_string(),
            )
        })
        .collect();
    actions.sort();
    assert!(
        actions.contains(&("start".into(), "sent".into())),
        "{actions:?}"
    );
    assert!(
        actions.contains(&("stop".into(), "sent".into())),
        "{actions:?}"
    );
}

/// The REAL daemon's order: it reports `idle` the moment its harness is up
/// and answers the start right after, so the state lands first. The answer
/// must still be heard — the caller woken by it, its account recorded, the
/// room told. Field, 2026-10-07: every start lost its answer this way; the
/// caller waited out the whole bound and the account and note never came.
#[tokio::test]
async fn a_state_that_beats_the_devices_answer_does_not_lose_it() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiveorder").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-order", RUNS_HIVE).await;

    let caller = async {
        let asked = Instant::now();
        let body = start(&app, &tid, &token, &dev.agent_id, "/home/dev/src/app").await;
        (body, asked.elapsed())
    };
    let target = async {
        let frame = read_until(&mut dev.ws, "rc:hive.start")
            .await
            .expect("the device receives rc:hive.start");
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.state", "session_id": frame["session_id"],
                   "fence": frame["fence"], "state": "idle"}),
        )
        .await;
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": frame["session_id"],
                   "fence": frame["fence"], "account": "dev"}),
        )
        .await;
    };
    let ((body, waited), ()) = tokio::join!(caller, target);

    assert_eq!(body["outcome"], "accepted", "{body}");
    let s = &body["session"];
    assert_eq!(s["account"], "dev", "the device's answer was heard: {s}");
    assert!(
        waited < Duration::from_secs(hive_limits::START_ACK_TIMEOUT_SECS / 2),
        "the caller waited {waited:?}: the answer was dropped and only the bound released it"
    );
    let sid = s["id"].as_str().unwrap().to_string();
    assert!(
        matches!(
            stored(&app, &sid).await.get("accepted_at"),
            Some(bson::Bson::DateTime(_))
        ),
        "the answer is on the record"
    );
    let room = s["room_id"].as_str().unwrap().to_string();
    wait_messages(
        &app,
        &tid,
        &token,
        &room,
        "the start note, with the account",
        |items| {
            items.iter().any(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("Started on") && c.contains("**dev**"))
            })
        },
    )
    .await;
}

/// The device's gate is the caller's answer: which gate, in its own word,
/// with a message saying what to change.
#[tokio::test]
async fn a_device_refusal_is_the_callers_answer_with_its_gate() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiverefuse").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-refuse", RUNS_HIVE).await;

    let caller = start(&app, &tid, &token, &dev.agent_id, "/srv/data");
    let target = async {
        let frame = read_until(&mut dev.ws, "rc:hive.start").await.unwrap();
        send(
            &mut dev.ws,
            json!({
                "t": "rc:hive.start_ack",
                "session_id": frame["session_id"],
                "fence": 1,
                "refused": "no_account",
                "detail": "no hive_accounts entry\nfor this user",
            }),
        )
        .await;
    };
    let (body, ()) = tokio::join!(caller, target);

    assert_eq!(body["outcome"], "refused", "{body}");
    assert_eq!(body["reason"], "no_account");
    assert!(
        body["message"].as_str().unwrap().contains("hive_accounts"),
        "{body}"
    );
    let s = &body["session"];
    assert_eq!(s["status"], "refused");
    assert_eq!(s["refusal"], "no_account");
    assert_eq!(
        s["detail"], "no hive_accounts entry for this user",
        "kept on one line"
    );

    // P0d — and the session's room says so, in the device's own words.
    let room = s["room_id"].as_str().expect("the session has a room");
    let items = wait_messages(&app, &tid, &token, room, "a refusal note", |items| {
        items.iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains("refused the session"))
        })
    })
    .await;
    let note = items
        .iter()
        .find(|m| m["content"].as_str().unwrap().contains("refused"))
        .unwrap();
    assert!(
        note["content"].as_str().unwrap().contains("hive_accounts"),
        "{note}"
    );
    assert_eq!(note["author_type"], "bot");
}

/// P0d — a session is a `Secret` room the session itself writes into: a
/// note when it starts, one stub per turn updated in place as the turn
/// finishes, a note when it ends. Nobody else in the org can see the room,
/// and a turn report from another device writes nothing.
#[tokio::test]
async fn a_session_is_a_secret_room_its_device_writes_stubs_into() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiveroom").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-room", RUNS_HIVE).await;
    let mut stranger = device(&app, &seeded, "hive-room-x", RUNS_HIVE).await;

    let caller = start(&app, &tid, &token, &dev.agent_id, "/srv/app");
    let target = async {
        let f = read_until(&mut dev.ws, "rc:hive.start").await.unwrap();
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": f["session_id"], "fence": 1, "account": "dev"}),
        )
        .await;
    };
    let (body, ()) = tokio::join!(caller, target);
    assert_eq!(body["outcome"], "accepted", "{body}");
    let sid = body["session"]["id"].as_str().unwrap().to_string();
    let room = body["session"]["room_id"]
        .as_str()
        .expect("a session has a room")
        .to_string();

    // The room is the session's: bound to it, and secret to everyone else.
    let r: Value = app
        .auth_get(&format!("/api/tenant/{tid}/room/{room}"), &token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["visibility"], "secret", "{r}");
    assert!(bound_to(&r, &sid), "{r}");
    assert_eq!(r["name"], "app", "titled like the session: {r}");
    let other = &seeded.member.access_token;
    let resp = app
        .auth_get(&format!("/api/tenant/{tid}/room/{room}"), other)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "a non-member learns nothing");
    assert_eq!(room_messages(&app, &tid, other, &room).await.0, 404);

    // …and not a channel: the plan's `max_channels` gate and the compliance
    // report count with `channel_filter`, which must not see a bound room,
    // or a few sessions would lock the org out of creating channels.
    // The seeded rooms are the positive control: a filter that saw nothing
    // would pass the absence alone.
    let rooms = app.db.collection::<Document>("rooms");
    let in_org = doc! { "tenant_id": ObjectId::parse_str(&tid).unwrap() };
    let mut live = doc! { "deleted_at": null };
    live.extend(in_org.clone());
    let mut channels = roomler_ai_services::dao::room::RoomDao::channel_filter();
    channels.extend(in_org);
    let seeded_rooms = seeded.rooms.len() as u64;
    assert_eq!(rooms.count_documents(live).await.unwrap(), seeded_rooms + 1);
    assert_eq!(
        rooms.count_documents(channels).await.unwrap(),
        seeded_rooms,
        "the seeded rooms are channels; the session's room is not"
    );

    // The start note, authored by the session.
    let items = wait_messages(&app, &tid, &token, &room, "the start note", |items| {
        items.iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains("Started on"))
        })
    })
    .await;
    let started = items
        .iter()
        .find(|m| m["content"].as_str().unwrap().contains("Started on"))
        .unwrap();
    assert_eq!(started["author_type"], "bot");
    assert_eq!(started["author_name"], "Claude · hive-room");
    assert_eq!(started["author_id"], sid, "the session authors it");
    assert!(
        started["content"].as_str().unwrap().contains("**dev**"),
        "as the account the device reported: {started}"
    );

    // A turn starts: one stub, bound to the session's turn 1.
    let turn_ref = format!("{sid}#1");
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.turn", "session_id": sid, "fence": 1, "turn": 1,
               "status": "running", "prompted_by": seeded.admin.id, "steps": 0}),
    )
    .await;
    let items = wait_messages(&app, &tid, &token, &room, "a running stub", |items| {
        items.iter().any(|m| bound_to(m, &turn_ref))
    })
    .await;
    let stub = items.iter().find(|m| bound_to(m, &turn_ref)).unwrap();
    let stub_id = stub["id"].as_str().unwrap().to_string();
    let content = stub["content"].as_str().unwrap();
    assert!(
        content.contains("Turn 1") && content.contains("working"),
        "{content}"
    );
    assert!(content.contains("asked by"), "attributed: {content}");

    // It finishes: the SAME message is updated, no second stub.
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.turn", "session_id": sid, "fence": 1, "turn": 1,
               "status": "ok", "prompted_by": seeded.admin.id, "steps": 3,
               "duration_ms": 42000, "cost_usd": 0.12}),
    )
    .await;
    let items = wait_messages(&app, &tid, &token, &room, "the stub updated", |items| {
        items.iter().any(|m| {
            bound_to(m, &turn_ref) && m["content"].as_str().is_some_and(|c| c.contains("done"))
        })
    })
    .await;
    let stubs: Vec<&Value> = items.iter().filter(|m| bound_to(m, &turn_ref)).collect();
    assert_eq!(stubs.len(), 1, "one stub per turn: {stubs:#?}");
    assert_eq!(stubs[0]["id"], stub_id, "updated in place");
    let content = stubs[0]["content"].as_str().unwrap();
    assert!(
        content.contains("3 steps") && content.contains("42 s") && content.contains("$0.12"),
        "{content}"
    );

    // Another device's report for this session writes nothing. Its frame is
    // applied by ITS connection's task, so give that time before looking.
    let stranger_ref = format!("{sid}#2");
    send(
        &mut stranger.ws,
        json!({"t": "rc:hive.turn", "session_id": sid, "fence": 1, "turn": 2, "status": "running"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, items) = room_messages(&app, &tid, &token, &room).await;
    assert!(
        !items.iter().any(|m| bound_to(m, &stranger_ref)),
        "another device's turn report must not write into the room: {items:#?}"
    );

    // The session's device ends it: the end note follows, and the stranger's
    // turn 2 still never appeared.
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "ended",
               "detail": "the harness exited"}),
    )
    .await;
    let items = wait_messages(&app, &tid, &token, &room, "the end note", |items| {
        items.iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains("Session ended"))
        })
    })
    .await;
    assert!(
        !items.iter().any(|m| bound_to(m, &stranger_ref)),
        "another device's turn report must not write into the room: {items:#?}"
    );
}

/// Gate 4 (`HIVE_RUN`) for a plain member: a 200 carrying the reason, no
/// session created, nothing sent — and an audit row, because a refusal is
/// the interesting decision.
/// P1a-2 — an approval is ONE stub in the session's room, however often its
/// opening is replayed, edited to say how it ended; who answered is named
/// only when that is a driver; the driver is notified, naming the session
/// and never the call; and a session that ends with one open withdraws it.
#[tokio::test]
async fn an_approval_is_one_stub_that_says_how_it_ended() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiveappr").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-appr", RUNS_HIVE).await;
    let sid = started_session(&app, &tid, &token, &mut dev).await;
    let (_, s) = get_session(&app, &tid, &token, &sid).await;
    let room = s["room_id"].as_str().expect("a room").to_string();
    let approval = |id: &str, status: &str, by: Option<&str>| {
        let mut f = json!({"t": "rc:hive.approval", "session_id": sid, "fence": 1,
                           "approval_id": id, "turn": 1, "status": status});
        if let Some(by) = by {
            f["answered_by"] = json!(by);
        }
        f
    };
    let record = |id: &'static str| {
        let db = app.db.clone();
        async move {
            db.collection::<Document>("agent_approvals")
                .find_one(doc! { "approval_id": id })
                .await
                .unwrap()
                .expect("on the record")
        }
    };
    let content = |items: &[Value], reference: &str| -> Vec<String> {
        items
            .iter()
            .filter(|m| bound_to(m, reference))
            .map(|m| m["content"].as_str().unwrap_or_default().to_string())
            .collect()
    };

    // Opened — and said again, as a reconnect replays it.
    let a1 = format!("{sid}!a1");
    send(&mut dev.ws, approval("a1", "open", None)).await;
    send(&mut dev.ws, approval("a1", "open", None)).await;
    wait_messages(&app, &tid, &token, &room, "the approval's stub", |items| {
        items.iter().any(|m| bound_to(m, &a1))
    })
    .await;
    // Past the replay: a stub for a frame it carried would be there by now.
    send(&mut dev.ws, approval("amarker", "open", None)).await;
    let items = wait_messages(&app, &tid, &token, &room, "the marker", |items| {
        items.iter().any(|m| bound_to(m, &format!("{sid}!amarker")))
    })
    .await;
    assert_eq!(
        content(&items, &a1),
        ["🔐 **Approval needed** · turn 1 — open the session to answer"],
        "one stub, however often it is said"
    );
    assert_eq!(record("a1").await.get_str("status").unwrap(), "open");
    let notes = app.db.collection::<Document>("notifications");
    let n = notes
        .find_one(doc! { "notification_type": "approval_request" })
        .await
        .unwrap()
        .expect("the driver was notified");
    assert_eq!(
        n.get_object_id("user_id").unwrap().to_hex(),
        seeded.admin.id
    );
    assert_eq!(
        n.get_str("link").unwrap(),
        format!("/tenant/{tid}/room/{room}")
    );
    assert!(
        n.get_str("title").unwrap().ends_with("needs approval"),
        "{n}"
    );
    assert_eq!(
        notes
            .count_documents(doc! { "notification_type": "approval_request" })
            .await
            .unwrap(),
        2,
        "one per approval (a1 and the marker), none for the replay"
    );

    // Answered by someone who is not a driver: the end is taken, the name
    // is not — no stub says a person allowed what they could not have.
    send(
        &mut dev.ws,
        approval("a1", "allowed", Some(&seeded.member.id)),
    )
    .await;
    let items = wait_messages(&app, &tid, &token, &room, "a1 allowed", |items| {
        content(items, &a1).iter().any(|c| c.contains("allowed"))
    })
    .await;
    assert_eq!(
        content(&items, &a1),
        ["🔐 **Approval** · turn 1 — ✅ allowed"]
    );
    let r1 = record("a1").await;
    assert_eq!(r1.get_str("status").unwrap(), "allowed");
    assert!(r1.get("answered_by").is_none(), "{r1}");
    assert!(r1.get("resolved_at").is_some(), "{r1}");

    // Denied by the driver: named. An end said again changes nothing.
    let a2 = format!("{sid}!a2");
    send(&mut dev.ws, approval("a2", "open", None)).await;
    send(
        &mut dev.ws,
        approval("a2", "denied", Some(&seeded.admin.id)),
    )
    .await;
    send(&mut dev.ws, approval("a2", "expired", None)).await;
    let items = wait_messages(&app, &tid, &token, &room, "a2 denied", |items| {
        content(items, &a2).iter().any(|c| c.contains("denied by"))
    })
    .await;
    let said = content(&items, &a2);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].starts_with("🔐 **Approval** · turn 1 — ⛔ denied by **"),
        "{said:?}"
    );
    let r2 = record("a2").await;
    assert_eq!(
        r2.get_str("status").unwrap(),
        "denied",
        "the late `expired` changed nothing"
    );
    assert_eq!(
        r2.get_object_id("answered_by").unwrap().to_hex(),
        seeded.admin.id
    );

    // An end whose opening never arrived is recorded, and stubbed, as it ended.
    let a3 = format!("{sid}!a3");
    send(&mut dev.ws, approval("a3", "expired", None)).await;
    wait_messages(&app, &tid, &token, &room, "a3 expired", |items| {
        content(items, &a3) == ["🔐 **Approval** · turn 1 — ⌛ nobody answered in time"]
    })
    .await;

    // Still open when the session ends: withdrawn, on the record and in the
    // room. (The marker is that one.)
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "ended",
               "detail": "the harness exited"}),
    )
    .await;
    let marker = format!("{sid}!amarker");
    wait_messages(
        &app,
        &tid,
        &token,
        &room,
        "the open one withdrawn",
        |items| content(items, &marker) == ["🔐 **Approval** · turn 1 — ⏹ withdrawn"],
    )
    .await;
    assert_eq!(
        record("amarker").await.get_str("status").unwrap(),
        "withdrawn"
    );

    // Nothing opens in a session that is over — nor ends there, unrecorded.
    send(&mut dev.ws, approval("a4", "open", None)).await;
    send(&mut dev.ws, approval("a5", "withdrawn", None)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, items) = room_messages(&app, &tid, &token, &room).await;
    assert!(content(&items, &format!("{sid}!a4")).is_empty());
    assert!(content(&items, &format!("{sid}!a5")).is_empty());
}

/// P1b — a device that says it no longer runs a session ends it (finding 7:
/// a restarted daemon runs none of what it ran, and nothing else would ever
/// say so). What its manifest names stays; a start still in flight is
/// reconcile's, not the manifest's; another device's list changes nothing
/// of this one's.
#[tokio::test]
async fn a_device_that_no_longer_runs_a_session_ends_it() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hivemani").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-mani", RUNS_HIVE).await;
    let mut stranger = device(&app, &seeded, "hive-mani-x", RUNS_HIVE).await;
    let gone = started_session(&app, &tid, &token, &mut dev).await;
    let kept = started_session(&app, &tid, &token, &mut dev).await;
    for sid in [&gone, &kept] {
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "idle"}),
        )
        .await;
        wait_status(&app, &tid, &token, sid, "idle").await;
    }

    // Another device's empty list is about that device only.
    send(
        &mut stranger.ws,
        json!({"t": "rc:hive.manifest", "sessions": []}),
    )
    .await;

    // A start in flight: sent, not yet answered.
    let caller = start(&app, &tid, &token, &dev.agent_id, "/srv/third");
    let device_side = async {
        let f = read_until(&mut dev.ws, "rc:hive.start").await.unwrap();
        let pending = f["session_id"].as_str().unwrap().to_string();
        // The device lists only `kept`: `gone` is over; the unanswered
        // start is not "running" yet.
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.manifest", "sessions": [{"session_id": kept, "fence": 1}]}),
        )
        .await;
        wait_status(&app, &tid, &token, &gone, "ended").await;
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": pending, "fence": 1, "account": "dev"}),
        )
        .await;
        pending
    };
    let (body, pending) = tokio::join!(caller, device_side);
    assert_eq!(
        body["outcome"], "accepted",
        "the start in flight survived: {body}"
    );

    let (_, g) = get_session(&app, &tid, &token, &gone).await;
    assert_eq!(g["end_reason"], "not_on_device", "{g}");
    let (_, k) = get_session(&app, &tid, &token, &kept).await;
    assert_eq!(k["status"], "idle", "what the list names stays: {k}");
    let (_, p) = get_session(&app, &tid, &token, &pending).await;
    assert_ne!(p["status"], "ended", "{p}");
    let room = g["room_id"].as_str().unwrap();
    wait_messages(&app, &tid, &token, room, "the ended note", |items| {
        items.iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains("its device no longer runs it"))
        })
    })
    .await;
}

/// P1b — the state usually lands BEFORE the answer (P0g), so a socket that
/// drops between the two leaves the session `idle` with no `accepted_at`.
/// Reconcile never re-sends it — it is not `starting` — so the manifest is
/// the only word that it is over: a run state is the device's own word that
/// it launched the session, as good as its answer. Field, 2026-10-08: the one
/// record the first P1b run left `idle`.
#[tokio::test]
async fn a_session_whose_answer_was_lost_still_ends() {
    // It polls the record and the device's row, past the default limiter.
    let app = TestApp::spawn_with_settings(|s| {
        s.modules.hive = true;
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await;
    let seeded = app.seed_tenant("hivelostack").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-lostack", RUNS_HIVE).await;
    let agent_id = dev.agent_id.clone();

    // The caller is held for the answer that never comes; the device's side
    // runs meanwhile.
    let caller = start(&app, &tid, &token, &agent_id, "/srv");
    let device_side = async {
        let f = read_until(&mut dev.ws, "rc:hive.start").await.unwrap();
        let sid = f["session_id"].as_str().unwrap().to_string();
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "idle"}),
        )
        .await;
        wait_status(&app, &tid, &token, &sid, "idle").await;
        // The socket drops before the answer, and the daemon restarts.
        drop(dev.ws);
        wait_offline(&app, &seeded, &dev.agent_id).await;
        let mut ws = connect(&app, &dev.token, &dev.machine, RUNS_HIVE).await;
        send(&mut ws, json!({"t": "rc:hive.manifest", "sessions": []})).await;
        let s = wait_status(&app, &tid, &token, &sid, "ended").await;
        assert!(
            read_until(&mut ws, "rc:hive.start").await.is_none(),
            "reconcile re-sent it: the manifest is not the only word"
        );
        (sid, s)
    };
    let (_, (sid, s)) = tokio::join!(caller, device_side);
    assert_eq!(s["end_reason"], "not_on_device", "{s}");
    assert!(s["accepted_at"].is_null(), "the answer never landed: {s}");
    let room = s["room_id"].as_str().unwrap();
    wait_messages(&app, &tid, &token, room, "the ended note", |items| {
        items.iter().any(|m| {
            bound_to(m, &sid)
                && m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("its device no longer runs it"))
        })
    })
    .await;
}

#[tokio::test]
async fn a_member_without_hive_run_is_refused_and_audited() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiveperm").await;
    let mut dev = device(&app, &seeded, "hive-perm", RUNS_HIVE).await;

    let body = post_start(
        &app,
        &seeded.tenant_id,
        &seeded.member.access_token,
        &dev.agent_id,
        "/srv",
    )
    .await;
    assert_eq!(body["outcome"], "refused", "{body}");
    assert_eq!(body["reason"], "no_permission");
    assert!(
        body["session"].is_null(),
        "no session for a refusal: {body}"
    );
    assert!(
        read_until(&mut dev.ws, "rc:hive.start").await.is_none(),
        "nothing reaches the device"
    );

    let audit = app
        .db
        .collection::<Document>("hive_audit")
        .find_one(doc! { "reason": "no_permission" })
        .await
        .unwrap()
        .expect("the refusal is audited");
    assert_eq!(audit.get_str("action").unwrap(), "start");
    assert_eq!(audit.get_str("outcome").unwrap(), "refused");
    let sessions = app
        .db
        .collection::<Document>("agent_sessions")
        .count_documents(doc! {})
        .await
        .unwrap();
    assert_eq!(sessions, 0);
}

/// A connection that does not advertise `hive` is never sent a start: a
/// pre-feature agent drops the unknown tag at `debug!` and the caller would
/// wait on silence. An enrolled device that is not connected is `offline`.
#[tokio::test]
async fn a_device_that_does_not_run_sessions_is_never_sent_one() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hivecaps").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut old = device(&app, &seeded, "hive-old", NO_HIVE).await;

    // Past the registration race (offline), the answer settles on
    // `device_unsupported` and stays there.
    let mut body = Value::Null;
    for _ in 0..40 {
        body = post_start(&app, &tid, &token, &old.agent_id, "/srv").await;
        if body["reason"] != "device_offline" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(body["reason"], "device_unsupported", "{body}");
    assert!(
        read_until(&mut old.ws, "rc:hive.start").await.is_none(),
        "a start was pushed to a build that would drop it"
    );

    let (never_connected, _token) = enroll_agent(&app, &seeded, "hive-away", "hive-away").await;
    let body = post_start(&app, &tid, &token, &never_connected, "/srv").await;
    assert_eq!(body["reason"], "device_offline", "{body}");
}

/// Another org's device is a 404 — the answer a bogus id gets — and a
/// folder no filesystem holds is a 400, before anything is created.
#[tokio::test]
async fn another_orgs_device_is_unknown_and_a_bad_folder_is_refused() {
    let app = hive_app().await;
    let ours = app.seed_tenant("hiveours").await;
    let theirs = app.seed_tenant("hivetheirs").await;
    let foreign = device(&app, &theirs, "hive-foreign", RUNS_HIVE).await;

    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/hive/session", ours.tenant_id),
            &ours.admin.access_token,
        )
        .json(&json!({ "device_id": foreign.agent_id, "folder": "/srv" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "another org's device");

    let mine = device(&app, &ours, "hive-mine", RUNS_HIVE).await;
    for folder in ["", "   ", "/srv\nINFO forged"] {
        let resp = app
            .auth_post(
                &format!("/api/tenant/{}/hive/session", ours.tenant_id),
                &ours.admin.access_token,
            )
            .json(&json!({ "device_id": mine.agent_id, "folder": folder }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400, "folder {folder:?}");
    }
    let sessions = app
        .db
        .collection::<Document>("agent_sessions")
        .count_documents(doc! {})
        .await
        .unwrap();
    assert_eq!(sessions, 0);
}

/// In P0 a session belongs to the member who started it: anyone else's
/// read or stop is a 404, and their listing does not show it.
#[tokio::test]
async fn only_the_starter_sees_or_stops_a_session() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiveown").await;
    let tid = seeded.tenant_id.clone();
    let mut dev = device(&app, &seeded, "hive-own", RUNS_HIVE).await;

    let caller = start(
        &app,
        &tid,
        &seeded.admin.access_token,
        &dev.agent_id,
        "/srv",
    );
    let target = async {
        let f = read_until(&mut dev.ws, "rc:hive.start").await.unwrap();
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": f["session_id"], "fence": 1}),
        )
        .await;
    };
    let (body, ()) = tokio::join!(caller, target);
    let sid = body["session"]["id"].as_str().unwrap().to_string();

    let other = &seeded.member.access_token;
    assert_eq!(get_session(&app, &tid, other, &sid).await.0, 404);
    assert_eq!(stop(&app, &tid, other, &sid).await.0, 404);
    let list: Value = app
        .auth_get(&format!("/api/tenant/{tid}/hive/session"), other)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["total"], 0, "{list}");

    let mine: Value = app
        .auth_get(
            &format!("/api/tenant/{tid}/hive/session"),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mine["total"], 1, "{mine}");
    assert_eq!(mine["items"][0]["id"], sid);
    assert!(mine["items"][0]["created_at"].is_string(), "{mine}");
}

/// Session ids are ObjectIds — structured, not secret. An answer from
/// another device, or from this one at an old fence, changes nothing and
/// does not wake the caller; the right answer still does.
#[tokio::test]
async fn answers_from_another_device_or_another_fence_change_nothing() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hivefence").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-target", RUNS_HIVE).await;
    let mut stranger = device(&app, &seeded, "hive-stranger", RUNS_HIVE).await;

    let caller = async {
        let body = start(&app, &tid, &token, &dev.agent_id, "/srv").await;
        (body, Instant::now())
    };
    let target = async {
        let f = read_until(&mut dev.ws, "rc:hive.start").await.unwrap();
        let sid = f["session_id"].clone();
        send(
            &mut stranger.ws,
            json!({"t": "rc:hive.start_ack", "session_id": sid, "fence": 1, "refused": "hive_disabled"}),
        )
        .await;
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": sid, "fence": 2, "refused": "hive_disabled"}),
        )
        .await;
        tokio::time::sleep(HOLD).await;
        let acked_at = Instant::now();
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": sid, "fence": 1, "account": "dev"}),
        )
        .await;
        acked_at
    };
    let ((body, answered_at), acked_at) = tokio::join!(caller, target);
    assert!(
        answered_at >= acked_at,
        "a wrong answer woke the caller {:?} early",
        acked_at - answered_at
    );
    assert_eq!(body["outcome"], "accepted", "{body}");
    assert_eq!(body["session"]["status"], "starting");
    assert!(body["session"]["refusal"].is_null(), "{body}");

    // And a state report from the stranger moves nothing either.
    let sid = body["session"]["id"].as_str().unwrap().to_string();
    send(
        &mut stranger.ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "ended"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        get_session(&app, &tid, &token, &sid).await.1["status"],
        "starting"
    );
}

/// The record is the source of truth, not the wait. A device that does not
/// answer leaves the caller with `pending` after the bound; when the device
/// reconnects it is sent the unanswered start again, and its answer lands.
#[tokio::test]
async fn a_silent_device_leaves_the_start_pending_and_is_asked_again() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hivesilent").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-silent", RUNS_HIVE).await;

    let caller = start(&app, &tid, &token, &dev.agent_id, "/srv");
    let target = async { read_until(&mut dev.ws, "rc:hive.start").await.unwrap() };
    let (body, first) = tokio::join!(caller, target);
    assert_eq!(body["outcome"], "pending", "{body}");
    assert_eq!(body["session"]["status"], "starting");
    let sid = body["session"]["id"].as_str().unwrap().to_string();
    assert_eq!(first["session_id"], sid);

    // The device drops and comes back: it is asked again, same session,
    // same fence, same harness id.
    drop(dev.ws);
    wait_offline(&app, &seeded, &dev.agent_id).await;
    let mut ws = connect(&app, &dev.token, &dev.machine, RUNS_HIVE).await;
    let again = read_until(&mut ws, "rc:hive.start")
        .await
        .expect("the unanswered start is re-sent on connect");
    assert_eq!(again["session_id"], sid);
    assert_eq!(again["fence"], 1);
    assert_eq!(again["harness_session"], first["harness_session"]);

    send(
        &mut ws,
        json!({"t": "rc:hive.start_ack", "session_id": sid, "fence": 1, "account": "dev"}),
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let s = get_session(&app, &tid, &token, &sid).await.1;
        if s["accepted_at"].is_string() {
            assert_eq!(s["account"], "dev");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the late answer never landed: {s}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// An unanswered start whose device comes back as a build that does not run
/// sessions is over: nothing there can launch it. The record says `lost`, and
/// the session's room says so — once — like every other way a session ends.
#[tokio::test]
async fn a_start_its_device_can_no_longer_run_is_lost_and_the_room_says_so() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hivelost").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-lost", RUNS_HIVE).await;

    let caller = start(&app, &tid, &token, &dev.agent_id, "/srv");
    let target = async { read_until(&mut dev.ws, "rc:hive.start").await.unwrap() };
    let (body, _) = tokio::join!(caller, target);
    assert_eq!(body["outcome"], "pending", "{body}");
    let sid = body["session"]["id"].as_str().unwrap().to_string();
    let room = body["session"]["room_id"]
        .as_str()
        .expect("the session has a room")
        .to_string();

    drop(dev.ws);
    wait_offline(&app, &seeded, &dev.agent_id).await;
    let mut ws = connect(&app, &dev.token, &dev.machine, NO_HIVE).await;
    let s = wait_status(&app, &tid, &token, &sid, "lost").await;
    assert_eq!(s["end_reason"], "never_answered", "{s}");
    assert!(
        read_until(&mut ws, "rc:hive.start").await.is_none(),
        "a start was re-sent to a build that would drop it"
    );

    let ended = |m: &Value| {
        bound_to(m, &sid)
            && m["content"]
                .as_str()
                .is_some_and(|c| c.starts_with("⏹ Session ended"))
    };
    let items = wait_messages(&app, &tid, &token, &room, "the lost note", |items| {
        items.iter().any(ended)
    })
    .await;
    let note = items.iter().find(|m| ended(m)).unwrap();
    let text = note["content"].as_str().unwrap();
    assert!(text.contains("never answered"), "{text}");
    assert!(text.contains("does not run agent sessions"), "{text}");
    assert_eq!(note["author_type"], "bot");
    assert_eq!(items.iter().filter(|m| ended(m)).count(), 1, "{items:#?}");
}

/// P1b's other door. A build without `hive` runs no session and sends no
/// manifest, so what its device ran stayed live on the record. Field,
/// 2026-10-10: a Windows device ran an accepted session, updated itself to
/// 0.4.123, whose build has no `hive`, and the session read `idle` for ever.
/// Back without `hive`, the device runs none of what it ran: each session
/// ends (`not_on_device`, or `stopped` for one being stopped), its room is
/// told once, and its open approval is withdrawn. The control comes first:
/// the same device back WITH `hive`, its manifest naming both, keeps them.
#[tokio::test]
async fn a_device_back_without_agent_sessions_ends_the_ones_it_ran() {
    // It polls the records, the rooms and the device's row.
    let app = hive_app_polling().await;
    let seeded = app.seed_tenant("hivenohive").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-nohive", RUNS_HIVE).await;
    let ran = started_session(&app, &tid, &token, &mut dev).await;
    let stopping = started_session(&app, &tid, &token, &mut dev).await;
    for sid in [&ran, &stopping] {
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "idle"}),
        )
        .await;
        wait_status(&app, &tid, &token, sid, "idle").await;
    }
    let room_of = |s: Value| s["room_id"].as_str().expect("a room").to_string();
    let room = room_of(get_session(&app, &tid, &token, &ran).await.1);
    let stopping_room = room_of(get_session(&app, &tid, &token, &stopping).await.1);
    let a1 = format!("{ran}!a1");
    let approval = || {
        let db = app.db.clone();
        async move {
            db.collection::<Document>("agent_approvals")
                .find_one(doc! { "approval_id": "a1" })
                .await
                .unwrap()
                .expect("on the record")
        }
    };
    let ended = |m: &Value, sid: &str| {
        bound_to(m, sid)
            && m["content"]
                .as_str()
                .is_some_and(|c| c.starts_with("⏹ Session ended"))
    };

    // The control: back WITH `hive`, its manifest naming both. The approval
    // it opens next is applied after the manifest, from the same ordered
    // queue, so its stub says the manifest was read.
    drop(dev.ws);
    wait_offline(&app, &seeded, &dev.agent_id).await;
    let mut ws = connect(&app, &dev.token, &dev.machine, RUNS_HIVE).await;
    send(
        &mut ws,
        json!({"t": "rc:hive.manifest", "sessions": [
            {"session_id": ran, "fence": 1},
            {"session_id": stopping, "fence": 1}
        ]}),
    )
    .await;
    send(
        &mut ws,
        json!({"t": "rc:hive.approval", "session_id": ran, "fence": 1,
               "approval_id": "a1", "turn": 1, "status": "open"}),
    )
    .await;
    wait_messages(&app, &tid, &token, &room, "the approval's stub", |items| {
        items.iter().any(|m| bound_to(m, &a1))
    })
    .await;
    for sid in [&ran, &stopping] {
        let (_, s) = get_session(&app, &tid, &token, sid).await;
        assert_eq!(s["status"], "idle", "what the manifest names stays: {s}");
    }
    assert_eq!(approval().await.get_str("status").unwrap(), "open");

    // Away again, and a stop is queued for one of them.
    drop(ws);
    wait_offline(&app, &seeded, &dev.agent_id).await;
    let (code, queued) = stop(&app, &tid, &token, &stopping).await;
    assert_eq!(code, 200);
    assert_eq!(queued["outcome"], "queued", "{queued}");

    // Back as a build WITHOUT `hive`: it sends no manifest and runs nothing.
    let mut ws = connect(&app, &dev.token, &dev.machine, NO_HIVE).await;
    let s = wait_status(&app, &tid, &token, &ran, "ended").await;
    assert_eq!(s["end_reason"], "not_on_device", "{s}");
    let s = wait_status(&app, &tid, &token, &stopping, "ended").await;
    assert_eq!(s["end_reason"], "stopped", "{s}");

    let items = wait_messages(&app, &tid, &token, &room, "the ended note", |items| {
        items.iter().any(|m| ended(m, &ran))
    })
    .await;
    let note = items.iter().find(|m| ended(m, &ran)).unwrap();
    let text = note["content"].as_str().unwrap();
    assert!(text.contains("its device no longer runs it"), "{text}");
    let withdrawn = "🔐 **Approval** · turn 1 — ⏹ withdrawn";
    wait_messages(
        &app,
        &tid,
        &token,
        &room,
        "the approval withdrawn",
        |items| {
            items
                .iter()
                .any(|m| bound_to(m, &a1) && m["content"] == withdrawn)
        },
    )
    .await;
    assert_eq!(approval().await.get_str("status").unwrap(), "withdrawn");
    wait_messages(
        &app,
        &tid,
        &token,
        &stopping_room,
        "the stopped session's note",
        |items| items.iter().any(|m| ended(m, &stopping)),
    )
    .await;

    // Nothing is pushed to a build that would drop it, and meanwhile no
    // room is told twice.
    assert!(
        !device_hears(&mut ws, "rc:hive.stop", Duration::from_millis(500)).await,
        "a stop was pushed to a build without `hive`"
    );
    for (sid, room) in [(&ran, &room), (&stopping, &stopping_room)] {
        let (_, items) = room_messages(&app, &tid, &token, room).await;
        let notes = items.iter().filter(|m| ended(m, sid)).count();
        assert_eq!(notes, 1, "{items:#?}");
    }
}

/// A stop for a device that is away is queued, and delivered when it
/// connects — the same path as an online one.
#[tokio::test]
async fn a_stop_for_an_absent_device_is_delivered_when_it_connects() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hivequeue").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-queue", RUNS_HIVE).await;

    let caller = start(&app, &tid, &token, &dev.agent_id, "/srv");
    let target = async {
        let f = read_until(&mut dev.ws, "rc:hive.start").await.unwrap();
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": f["session_id"], "fence": 1}),
        )
        .await;
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.state", "session_id": f["session_id"], "fence": 1, "state": "idle"}),
        )
        .await;
    };
    let (body, ()) = tokio::join!(caller, target);
    let sid = body["session"]["id"].as_str().unwrap().to_string();
    wait_status(&app, &tid, &token, &sid, "idle").await;

    drop(dev.ws);
    wait_offline(&app, &seeded, &dev.agent_id).await;
    let (code, queued) = stop(&app, &tid, &token, &sid).await;
    assert_eq!(code, 200);
    assert_eq!(queued["outcome"], "queued", "{queued}");
    assert_eq!(queued["session"]["status"], "stopping");

    let mut ws = connect(&app, &dev.token, &dev.machine, RUNS_HIVE).await;
    let order = read_until(&mut ws, "rc:hive.stop")
        .await
        .expect("the queued stop is delivered on connect");
    assert_eq!(order["session_id"], sid);
    send(
        &mut ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "ended"}),
    )
    .await;
    let s = wait_status(&app, &tid, &token, &sid, "ended").await;
    assert_eq!(s["end_reason"], "stopped");
}

/// A member who is removed stops driving agent sessions on the org's
/// devices at once: the device is told, and the record ends.
#[tokio::test]
async fn removing_a_member_ends_their_sessions_and_tells_the_device() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hivemember").await;
    let tid = seeded.tenant_id.clone();
    grant_hive_run_to_member(&app, &seeded).await;
    let mut dev = device(&app, &seeded, "hive-member", RUNS_HIVE).await;

    let caller = start(
        &app,
        &tid,
        &seeded.member.access_token,
        &dev.agent_id,
        "/srv",
    );
    let target = async {
        let f = read_until(&mut dev.ws, "rc:hive.start").await.unwrap();
        assert_eq!(f["user_id"], seeded.member.id, "the starter, not the owner");
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": f["session_id"], "fence": 1}),
        )
        .await;
    };
    let (body, ()) = tokio::join!(caller, target);
    assert_eq!(body["outcome"], "accepted", "{body}");
    let sid = body["session"]["id"].as_str().unwrap().to_string();

    let resp = app
        .auth_delete(
            &format!("/api/tenant/{tid}/member/{}", seeded.member.id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "remove: {:?}", resp.status());

    let order = read_until(&mut dev.ws, "rc:hive.stop")
        .await
        .expect("the device is told to stop");
    assert_eq!(order["session_id"], sid);
    assert_eq!(order["reason"], "member_removed");
    let s = stored(&app, &sid).await;
    assert_eq!(s.get_str("status").unwrap(), "ended");
    assert_eq!(s.get_str("end_reason").unwrap(), "member_removed");
}

/// The same removal while the device is AWAY: the record ends at once, but
/// the stop cannot reach it, and nothing is pending for reconcile to re-send.
/// The device's own replay on reconnect — "I run this, idle" — is what it is
/// answered on: a stop, with the record's reason. Its `ended` reply changes
/// nothing, and draws no second stop.
#[tokio::test]
async fn a_device_that_missed_the_end_is_told_when_it_reports_the_session() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hivemissed").await;
    let tid = seeded.tenant_id.clone();
    grant_hive_run_to_member(&app, &seeded).await;
    let mut dev = device(&app, &seeded, "hive-missed", RUNS_HIVE).await;

    let caller = start(
        &app,
        &tid,
        &seeded.member.access_token,
        &dev.agent_id,
        "/srv",
    );
    let target = async {
        let f = read_until(&mut dev.ws, "rc:hive.start").await.unwrap();
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": f["session_id"], "fence": 1}),
        )
        .await;
    };
    let (body, ()) = tokio::join!(caller, target);
    assert_eq!(body["outcome"], "accepted", "{body}");
    let sid = body["session"]["id"].as_str().unwrap().to_string();

    drop(dev.ws);
    wait_offline(&app, &seeded, &dev.agent_id).await;
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{tid}/member/{}", seeded.member.id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "remove: {:?}", resp.status());
    assert_eq!(stored(&app, &sid).await.get_str("status").unwrap(), "ended");

    // Back, and still running it — as the device's replay says.
    let mut ws = connect(&app, &dev.token, &dev.machine, RUNS_HIVE).await;
    send(
        &mut ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "idle"}),
    )
    .await;
    let order = read_until(&mut ws, "rc:hive.stop")
        .await
        .expect("a device running a session that is over is told to stop");
    assert_eq!(order["session_id"], sid);
    assert_eq!(order["fence"], 1);
    assert_eq!(order["reason"], "member_removed");

    send(
        &mut ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "ended"}),
    )
    .await;
    assert!(
        read_until(&mut ws, "rc:hive.stop").await.is_none(),
        "an `ended` answer drew another stop"
    );
    let s = stored(&app, &sid).await;
    assert_eq!(s.get_str("status").unwrap(), "ended");
    assert_eq!(s.get_str("end_reason").unwrap(), "member_removed");
}

/// Removing a device ends the sessions it ran — before its row and socket
/// go, so it is still there to be told.
#[tokio::test]
async fn removing_a_device_ends_its_sessions() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hivedevice").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-device", RUNS_HIVE).await;

    let caller = start(&app, &tid, &token, &dev.agent_id, "/srv");
    let target = async {
        let f = read_until(&mut dev.ws, "rc:hive.start").await.unwrap();
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": f["session_id"], "fence": 1}),
        )
        .await;
    };
    let (body, ()) = tokio::join!(caller, target);
    let sid = body["session"]["id"].as_str().unwrap().to_string();

    let resp = app
        .auth_delete(&format!("/api/tenant/{tid}/agent/{}", dev.agent_id), &token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "delete: {:?}", resp.status());

    let order = read_until(&mut dev.ws, "rc:hive.stop")
        .await
        .expect("the device is told before it is removed");
    assert_eq!(order["reason"], "device_removed");
    let (_, s) = get_session(&app, &tid, &token, &sid).await;
    assert_eq!(s["status"], "ended", "{s}");
    assert_eq!(s["end_reason"], "device_removed");
}

// ─── P2e-1 — a report from a device that is no longer the location ─────────

/// Move a session's record to `device` at `fence`, as a promotion's lease
/// CAS will (P2e-3): the record alone, nobody told.
async fn move_record(app: &TestApp, sid: &str, device: &str, fence: i64) {
    let moved = app
        .db
        .collection::<Document>("agent_sessions")
        .update_one(
            doc! { "_id": ObjectId::parse_str(sid).unwrap() },
            doc! { "$set": {
                "location.device_id": ObjectId::parse_str(device).unwrap(),
                "fence": fence,
            } },
        )
        .await
        .unwrap();
    assert_eq!(moved.modified_count, 1, "the record moved");
}

/// FR-90 P2e-1 — a device that reports a session the record holds on
/// another device (its run state, a turn, an approval, its manifest, a late
/// answer to the start) is told it no longer holds it: `rc:hive.demote`, at
/// the RECORD's fence. The record does not move. The holder is told
/// nothing, even at an older fence of its own, and neither is another
/// organization's device that names the session.
#[tokio::test]
async fn a_device_that_reports_a_session_held_elsewhere_is_demoted() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hivedemote").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut old = device(&app, &seeded, "hive-old", RUNS_HIVE).await;
    let mut holder = device(&app, &seeded, "hive-holder", RUNS_HIVE).await;
    let theirs = app.seed_tenant("hivedemote2").await;
    let mut foreign = device(&app, &theirs, "hive-foreign", RUNS_HIVE).await;
    let sid = started_session(&app, &tid, &token, &mut old).await;
    send(
        &mut old.ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "idle"}),
    )
    .await;
    wait_status(&app, &tid, &token, &sid, "idle").await;

    move_record(&app, &sid, &holder.agent_id, 2).await;

    let reports = [
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "running"}),
        json!({"t": "rc:hive.turn", "session_id": sid, "fence": 1, "turn": 1, "status": "running"}),
        json!({"t": "rc:hive.approval", "session_id": sid, "fence": 1,
               "approval_id": "a1", "turn": 1, "status": "open"}),
        json!({"t": "rc:hive.manifest", "sessions": [{"session_id": sid, "fence": 1}]}),
        json!({"t": "rc:hive.start_ack", "session_id": sid, "fence": 1, "account": "dev"}),
    ];
    for report in reports {
        send(&mut old.ws, report.clone()).await;
        let demote = read_until(&mut old.ws, "rc:hive.demote")
            .await
            .unwrap_or_else(|| panic!("no demote for {report}"));
        assert_eq!(demote["session_id"], sid, "{report}");
        assert_eq!(demote["fence"], 2, "the record's fence: {report}");
        assert_eq!(demote["reason"], "not_location", "{report}");
    }

    // The holder, at its own fence and at an older one: nothing.
    for fence in [2, 1] {
        send(
            &mut holder.ws,
            json!({"t": "rc:hive.state", "session_id": sid, "fence": fence, "state": "idle"}),
        )
        .await;
    }
    assert!(
        !device_hears(&mut holder.ws, "rc:hive.demote", Duration::from_millis(800)).await,
        "the holder is not demoted"
    );
    // Another organization's device naming the session: nothing either.
    send(
        &mut foreign.ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "running"}),
    )
    .await;
    send(
        &mut foreign.ws,
        json!({"t": "rc:hive.manifest", "sessions": [{"session_id": sid, "fence": 1}]}),
    )
    .await;
    assert!(
        !device_hears(
            &mut foreign.ws,
            "rc:hive.demote",
            Duration::from_millis(800)
        )
        .await,
        "another organization's device is told nothing"
    );

    let s = stored(&app, &sid).await;
    assert_eq!(
        s.get_document("location")
            .unwrap()
            .get_object_id("device_id")
            .unwrap()
            .to_hex(),
        holder.agent_id,
        "the record does not move"
    );
    assert_eq!(s.get_i64("fence").unwrap(), 2);
    assert_eq!(s.get_str("status").unwrap(), "idle");
}

// ─── P0d-2b — the viewer peer's signalling ─────────────────────────────────

/// A build that runs sessions AND serves them to a browser.
const VIEWS_HIVE: &[&str] = &["exec", "hive", "hive-view"];

type UserWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn user_ws(app: &TestApp, token: &str) -> UserWs {
    let url = format!(
        "ws://{}/ws?token={}",
        app.addr,
        token
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    );
    let (ws, _) = connect_async(&url).await.expect("user ws connect");
    ws
}

async fn user_send(ws: &mut UserWs, kind: &str, data: Value) {
    ws.send(Message::Text(
        json!({ "type": kind, "data": data }).to_string().into(),
    ))
    .await
    .unwrap();
}

/// The `data` of the next user-socket frame of `kind`, within `within`.
async fn user_read_for(ws: &mut UserWs, kind: &str, within: Duration) -> Option<Value> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&text)
                    && v["type"] == kind
                {
                    return Some(v["data"].clone());
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => return None,
            Err(_) => {}
        }
    }
    None
}

async fn user_read(ws: &mut UserWs, kind: &str) -> Option<Value> {
    user_read_for(ws, kind, Duration::from_secs(5)).await
}

/// Whether the device receives `want` within `within` — for asserting that
/// it does NOT, without waiting `read_until`'s five seconds.
async fn device_hears(ws: &mut AgentWs, want: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&text)
                    && v["t"] == want
                {
                    return true;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => return false,
            Err(_) => {}
        }
    }
    false
}

/// Start a session as `token` on `dev`, which accepts it; its id.
async fn started_session(app: &TestApp, tid: &str, token: &str, dev: &mut Device) -> String {
    let caller = start(app, tid, token, &dev.agent_id, "/srv");
    let target = async {
        let f = read_until(&mut dev.ws, "rc:hive.start").await.unwrap();
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": f["session_id"], "fence": 1, "account": "dev"}),
        )
        .await;
    };
    let (body, ()) = tokio::join!(caller, target);
    assert_eq!(body["outcome"], "accepted", "{body}");
    body["session"]["id"].as_str().unwrap().to_string()
}

/// Open a view and have the device confirm it; the grant id.
async fn ready_view(ws: &mut UserWs, dev: &mut Device, sid: &str) -> String {
    user_send(ws, "hive:view.open", json!({"session_id": sid})).await;
    let grant = read_until(&mut dev.ws, "rc:hive.view.grant")
        .await
        .expect("the device is asked");
    let gid = grant["grant_id"].as_str().unwrap().to_string();
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.view.grant_ack", "grant_id": gid}),
    )
    .await;
    let ready = user_read(ws, "hive:view.ready").await.expect("ready");
    assert_eq!(ready["grant_id"], gid);
    gid
}

/// FR-83's rule for the viewer: the browser is told it may dial only once
/// the DEVICE confirmed the grant; then the handshake is relayed both ways
/// with the ICE servers minted for this one peer, and the browser's socket
/// closing tells the device.
#[tokio::test]
async fn a_view_is_ready_only_after_the_device_confirms_its_grant() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiveview").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-view", VIEWS_HIVE).await;
    let sid = started_session(&app, &tid, &token, &mut dev).await;

    let mut ws = user_ws(&app, &token).await;
    user_send(
        &mut ws,
        "hive:view.open",
        json!({"session_id": sid, "ref": "r1"}),
    )
    .await;
    let grant = read_until(&mut dev.ws, "rc:hive.view.grant")
        .await
        .expect("the device is asked first");
    assert_eq!(grant["session_id"], sid);
    assert_eq!(grant["user_id"], seeded.admin.id);
    assert_eq!(
        grant["may_prompt"], true,
        "the starter drives a live session"
    );
    assert_eq!(grant["ttl_secs"], 600);
    let gid = grant["grant_id"].as_str().unwrap().to_string();
    assert!(
        user_read_for(&mut ws, "hive:view.ready", Duration::from_millis(400))
            .await
            .is_none(),
        "the browser was told to dial before the device confirmed"
    );

    send(
        &mut dev.ws,
        json!({"t": "rc:hive.view.grant_ack", "grant_id": gid}),
    )
    .await;
    let ready = user_read(&mut ws, "hive:view.ready")
        .await
        .expect("ready once the device confirmed");
    assert_eq!(ready["grant_id"], gid);
    assert_eq!(ready["ref"], "r1");
    assert_eq!(ready["may_prompt"], true);
    assert!(!ready["ice_servers"].as_array().unwrap().is_empty());

    // The handshake, relayed — with the same ICE servers on both ends.
    user_send(
        &mut ws,
        "hive:view.offer",
        json!({"grant_id": gid, "sdp": "v=0 offer"}),
    )
    .await;
    let offer = read_until(&mut dev.ws, "rc:hive.view.offer").await.unwrap();
    assert_eq!(offer["sdp"], "v=0 offer");
    assert_eq!(offer["ice_servers"], ready["ice_servers"]);
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.view.answer", "grant_id": gid, "sdp": "v=0 answer"}),
    )
    .await;
    assert_eq!(
        user_read(&mut ws, "hive:view.answer").await.unwrap()["sdp"],
        "v=0 answer"
    );
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.view.ice", "grant_id": gid, "candidate": {"candidate": "from-device"}}),
    )
    .await;
    assert_eq!(
        user_read(&mut ws, "hive:view.ice").await.unwrap()["candidate"]["candidate"],
        "from-device"
    );
    user_send(
        &mut ws,
        "hive:view.ice",
        json!({"grant_id": gid, "candidate": {"candidate": "from-browser"}}),
    )
    .await;
    assert_eq!(
        read_until(&mut dev.ws, "rc:hive.view.ice").await.unwrap()["candidate"]["candidate"],
        "from-browser"
    );

    // A renewal re-checks the right to read and gives the device a fresh TTL.
    user_send(&mut ws, "hive:view.renew", json!({"grant_id": gid})).await;
    assert_eq!(
        read_until(&mut dev.ws, "rc:hive.view.renew").await.unwrap()["ttl_secs"],
        600
    );
    assert_eq!(
        user_read(&mut ws, "hive:view.renewed").await.unwrap()["ttl_secs"],
        600
    );

    let audited = app
        .db
        .collection::<Document>("hive_audit")
        .count_documents(doc! { "action": "view", "outcome": "sent" })
        .await
        .unwrap();
    assert_eq!(audited, 1, "the grant is the server's decision, audited");

    // The browser goes: the device closes the peer.
    drop(ws);
    let close = read_until(&mut dev.ws, "rc:hive.view.close")
        .await
        .expect("the device is told the viewer left");
    assert_eq!(close["grant_id"], gid);
    assert_eq!(close["reason"], "viewer_left");
}

/// Every refusal names who said no. A member outside the session's room
/// learns nothing — the answer a bogus id gets — and the device's own
/// refusal reaches the viewer in its words.
#[tokio::test]
async fn a_view_is_refused_in_the_servers_words_or_the_devices() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiveviewno").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-view-no", VIEWS_HIVE).await;
    let sid = started_session(&app, &tid, &token, &mut dev).await;

    let mut member = user_ws(&app, &seeded.member.access_token).await;
    user_send(
        &mut member,
        "hive:view.open",
        json!({"session_id": sid, "ref": "m"}),
    )
    .await;
    let no = user_read(&mut member, "hive:view.refused").await.unwrap();
    assert_eq!(no["reason"], "not_found", "{no}");
    assert_eq!(no["ref"], "m");
    user_send(
        &mut member,
        "hive:view.open",
        json!({"session_id": ObjectId::new().to_hex()}),
    )
    .await;
    assert_eq!(
        user_read(&mut member, "hive:view.refused").await.unwrap()["reason"],
        "not_found",
        "a bogus id and a room you are not in answer alike"
    );
    assert!(
        !device_hears(
            &mut dev.ws,
            "rc:hive.view.grant",
            Duration::from_millis(300)
        )
        .await,
        "no grant was minted for a non-member"
    );

    // The device's own word.
    let mut ws = user_ws(&app, &token).await;
    user_send(&mut ws, "hive:view.open", json!({"session_id": sid})).await;
    let grant = read_until(&mut dev.ws, "rc:hive.view.grant").await.unwrap();
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.view.grant_ack", "grant_id": grant["grant_id"], "refused": "no_session", "detail": "store\nlost"}),
    )
    .await;
    let refused = user_read(&mut ws, "hive:view.refused").await.unwrap();
    assert_eq!(refused["reason"], "no_session");
    assert_eq!(
        refused["message"], "store lost",
        "the device's words, on one line"
    );

    // A build that runs sessions but does not serve them is never asked.
    let mut old = device(&app, &seeded, "hive-old-view", RUNS_HIVE).await;
    let old_sid = started_session(&app, &tid, &token, &mut old).await;
    user_send(&mut ws, "hive:view.open", json!({"session_id": old_sid})).await;
    assert_eq!(
        user_read(&mut ws, "hive:view.refused").await.unwrap()["reason"],
        "device_unsupported"
    );
    assert!(
        !device_hears(
            &mut old.ws,
            "rc:hive.view.grant",
            Duration::from_millis(300)
        )
        .await,
        "a grant was pushed to a build that would drop it"
    );

    // And one that is away is offline.
    drop(old.ws);
    wait_offline(&app, &seeded, &old.agent_id).await;
    user_send(&mut ws, "hive:view.open", json!({"session_id": old_sid})).await;
    assert_eq!(
        user_read(&mut ws, "hive:view.refused").await.unwrap()["reason"],
        "device_offline"
    );
}

/// A grant belongs to one connection and one device: another socket's
/// frames for it, or another device's, move nothing.
#[tokio::test]
async fn frames_for_someone_elses_grant_move_nothing() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiveviewx").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-view-x", VIEWS_HIVE).await;
    let sid = started_session(&app, &tid, &token, &mut dev).await;
    let mut ws = user_ws(&app, &token).await;
    let gid = ready_view(&mut ws, &mut dev, &sid).await;

    // The same user on another tab, and another user: neither can drive it.
    let mut tab = user_ws(&app, &token).await;
    let mut stranger = user_ws(&app, &seeded.member.access_token).await;
    for other in [&mut tab, &mut stranger] {
        user_send(
            other,
            "hive:view.offer",
            json!({"grant_id": gid, "sdp": "v=0 hijack"}),
        )
        .await;
        user_send(other, "hive:view.close", json!({"grant_id": gid})).await;
    }
    assert!(
        !device_hears(
            &mut dev.ws,
            "rc:hive.view.offer",
            Duration::from_millis(500)
        )
        .await,
        "an offer from a socket that does not hold the grant reached the device"
    );

    // Another device answering for it reaches nobody.
    let mut other_dev = device(&app, &seeded, "hive-view-y", VIEWS_HIVE).await;
    send(
        &mut other_dev.ws,
        json!({"t": "rc:hive.view.answer", "grant_id": gid, "sdp": "v=0 forged"}),
    )
    .await;
    assert!(
        user_read_for(&mut ws, "hive:view.answer", Duration::from_millis(500))
            .await
            .is_none(),
        "another device's answer reached the viewer"
    );

    // The grant still works for its holder.
    user_send(
        &mut ws,
        "hive:view.offer",
        json!({"grant_id": gid, "sdp": "v=0 real"}),
    )
    .await;
    assert_eq!(
        read_until(&mut dev.ws, "rc:hive.view.offer").await.unwrap()["sdp"],
        "v=0 real"
    );
}

/// A device that never answers a grant: the viewer is told, and the device
/// told to drop it should its answer come after all.
#[tokio::test]
async fn a_silent_device_is_no_answer_for_the_viewer() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiveviewq").await;
    let tid = seeded.tenant_id.clone();
    let token = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "hive-view-q", VIEWS_HIVE).await;
    let sid = started_session(&app, &tid, &token, &mut dev).await;

    let mut ws = user_ws(&app, &token).await;
    user_send(&mut ws, "hive:view.open", json!({"session_id": sid})).await;
    let grant = read_until(&mut dev.ws, "rc:hive.view.grant").await.unwrap();
    let refused = user_read_for(&mut ws, "hive:view.refused", Duration::from_secs(15))
        .await
        .expect("the viewer hears no_answer");
    assert_eq!(refused["reason"], "no_answer");
    let close = read_until(&mut dev.ws, "rc:hive.view.close").await.unwrap();
    assert_eq!(close["grant_id"], grant["grant_id"]);
    assert_eq!(close["reason"], "no_answer");
}

/// A member who is removed stops READING too: their views close, on the
/// device, at once — not when the grant runs out.
#[tokio::test]
async fn removing_a_member_closes_their_views() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiveviewm").await;
    let tid = seeded.tenant_id.clone();
    grant_hive_run_to_member(&app, &seeded).await;
    let mut dev = device(&app, &seeded, "hive-view-m", VIEWS_HIVE).await;
    let sid = started_session(&app, &tid, &seeded.member.access_token, &mut dev).await;
    let mut ws = user_ws(&app, &seeded.member.access_token).await;
    let gid = ready_view(&mut ws, &mut dev, &sid).await;

    let resp = app
        .auth_delete(
            &format!("/api/tenant/{tid}/member/{}", seeded.member.id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "remove: {:?}", resp.status());

    let close = read_until(&mut dev.ws, "rc:hive.view.close")
        .await
        .expect("the device hears the view close");
    assert_eq!(close["grant_id"], gid);
    assert_eq!(close["reason"], "member_removed");
}

// ─── P1c — who takes part, and who drives ────────────────────────────────

/// A hive app that a polling test cannot run into the default limiter with.
async fn hive_app_polling() -> TestApp {
    TestApp::spawn_with_settings(|s| {
        s.modules.hive = true;
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await
}

fn participant_url(tid: &str, sid: &str, user: Option<&str>) -> String {
    match user {
        Some(u) => format!("/api/tenant/{tid}/hive/session/{sid}/participant/{u}"),
        None => format!("/api/tenant/{tid}/hive/session/{sid}/participant"),
    }
}

async fn status_and_json(resp: reqwest::Response) -> (u16, Value) {
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `PUT …/participant/{user}` `{role}` as `token`.
async fn set_part(
    app: &TestApp,
    tid: &str,
    token: &str,
    sid: &str,
    user: &str,
    role: &str,
) -> (u16, Value) {
    let resp = app
        .auth_put(&participant_url(tid, sid, Some(user)), token)
        .json(&json!({ "role": role }))
        .send()
        .await
        .unwrap();
    status_and_json(resp).await
}

async fn remove_part(app: &TestApp, tid: &str, token: &str, sid: &str, user: &str) -> (u16, Value) {
    let resp = app
        .auth_delete(&participant_url(tid, sid, Some(user)), token)
        .send()
        .await
        .unwrap();
    status_and_json(resp).await
}

async fn parts(app: &TestApp, tid: &str, token: &str, sid: &str) -> (u16, Value) {
    let resp = app
        .auth_get(&participant_url(tid, sid, None), token)
        .send()
        .await
        .unwrap();
    status_and_json(resp).await
}

/// Whether a stored session names no driver: the field absent, or emptied.
fn no_drivers(s: &Document) -> bool {
    s.get_array("drivers").map(|d| d.is_empty()).unwrap_or(true)
}

/// Whom every `approval_request` notification went to, sorted.
async fn approval_pushes(app: &TestApp) -> Vec<String> {
    let mut to: Vec<String> = Vec::new();
    let mut cur = app
        .db
        .collection::<Document>("notifications")
        .find(doc! { "notification_type": "approval_request" })
        .await
        .unwrap();
    while cur.advance().await.unwrap() {
        let n = cur.deserialize_current().unwrap();
        to.push(n.get_object_id("user_id").unwrap().to_hex());
    }
    to.sort();
    to
}

/// `user`'s role in a participants answer, if listed.
fn role_of(body: &Value, user: &str) -> Option<String> {
    body["items"]
        .as_array()?
        .iter()
        .find(|p| p["user_id"] == user)
        .and_then(|p| p["role"].as_str().map(str::to_string))
}

/// P1c — the owner decides who takes part: a READER joins the session's
/// Secret room and reads it; a DRIVER also prompts it. Naming a driver needs
/// `HIVE_RUN` (driving runs code on the device as the session's account);
/// every change ends the person's open views, because a grant carries
/// `may_prompt` as it was minted; and nobody but the owner changes anything.
#[tokio::test]
async fn the_owner_names_who_reads_and_who_drives() {
    let app = hive_app_polling().await;
    let seeded = app.seed_tenant("hiveparts").await;
    let tid = seeded.tenant_id.clone();
    let owner = seeded.admin.access_token.clone();
    let member = seeded.member.access_token.clone();
    let mid = seeded.member.id.clone();
    let mut dev = device(&app, &seeded, "hive-parts", VIEWS_HIVE).await;
    let sid = started_session(&app, &tid, &owner, &mut dev).await;
    let room = get_session(&app, &tid, &owner, &sid).await.1["room_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Before: the member learns nothing, and changes nothing.
    assert_eq!(get_session(&app, &tid, &member, &sid).await.0, 404);
    assert_eq!(parts(&app, &tid, &member, &sid).await.0, 404);
    assert_eq!(
        set_part(&app, &tid, &member, &sid, &mid, "reader").await.0,
        404
    );

    // A reader: in the room, reading — not prompting.
    let (code, body) = set_part(&app, &tid, &owner, &sid, &mid, "reader").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(role_of(&body, &seeded.admin.id).as_deref(), Some("owner"));
    assert_eq!(role_of(&body, &mid).as_deref(), Some("reader"));
    assert_eq!(body["may_manage"], true);
    let (code, s) = get_session(&app, &tid, &member, &sid).await;
    assert_eq!(code, 200, "a reader reads the session: {s}");
    assert_eq!(s["drivers"], json!([]), "{s}");
    let (code, theirs) = parts(&app, &tid, &member, &sid).await;
    assert_eq!(code, 200);
    assert_eq!(
        theirs["may_manage"], false,
        "only the owner manages: {theirs}"
    );
    assert_eq!(
        set_part(&app, &tid, &member, &sid, &mid, "driver").await.0,
        404,
        "a reader cannot make themselves a driver"
    );
    wait_messages(&app, &tid, &owner, &room, "the room is told", |items| {
        items.iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains("Member** joined to read"))
        })
    })
    .await;

    let mut mws = user_ws(&app, &member).await;
    user_send(&mut mws, "hive:view.open", json!({"session_id": sid})).await;
    let grant = read_until(&mut dev.ws, "rc:hive.view.grant")
        .await
        .expect("a reader may view");
    assert_eq!(grant["user_id"], mid);
    assert_eq!(grant["may_prompt"], false, "a reader does not prompt");
    assert!(
        grant.get("user_email").is_none(),
        "a reader's address never leaves the server (P1c-2): {grant}"
    );
    let gid = grant["grant_id"].as_str().unwrap().to_string();
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.view.grant_ack", "grant_id": gid}),
    )
    .await;
    assert_eq!(
        user_read(&mut mws, "hive:view.ready").await.unwrap()["may_prompt"],
        false
    );

    // A driver needs HIVE_RUN: refused, audited, nothing changed.
    let (code, body) = set_part(&app, &tid, &owner, &sid, &mid, "driver").await;
    assert_eq!(code, 403, "{body}");
    assert!(
        body.to_string().contains("HIVE_RUN"),
        "the refusal names what is missing: {body}"
    );
    let refused = app
        .db
        .collection::<Document>("hive_audit")
        .find_one(doc! { "action": "participant", "outcome": "refused" })
        .await
        .unwrap()
        .expect("the refusal is audited");
    assert_eq!(refused.get_str("reason").unwrap(), "no_permission");
    assert_eq!(refused.get_object_id("target_id").unwrap().to_hex(), mid);
    assert!(
        !device_hears(
            &mut dev.ws,
            "rc:hive.view.close",
            Duration::from_millis(300)
        )
        .await,
        "a refused change ends no view"
    );

    // Granted it, the member is named a driver; their view says otherwise
    // now, so it ends, and the one they reopen may prompt.
    grant_hive_run_to_member(&app, &seeded).await;
    let (code, body) = set_part(&app, &tid, &owner, &sid, &mid, "driver").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(role_of(&body, &mid).as_deref(), Some("driver"));
    let close = read_until(&mut dev.ws, "rc:hive.view.close")
        .await
        .expect("the device closes the stale grant");
    assert_eq!(close["grant_id"], gid);
    assert_eq!(close["reason"], "role_changed");
    assert_eq!(
        user_read(&mut mws, "hive:view.closed").await.unwrap()["reason"],
        "role_changed"
    );
    user_send(&mut mws, "hive:view.open", json!({"session_id": sid})).await;
    let grant = read_until(&mut dev.ws, "rc:hive.view.grant")
        .await
        .expect("reopened");
    assert_eq!(grant["may_prompt"], true, "a driver prompts a live session");
    assert_eq!(
        grant["user_email"], seeded.member.email,
        "a driver's address goes with the grant, for the device's own hive_accounts"
    );
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.view.grant_ack", "grant_id": grant["grant_id"]}),
    )
    .await;
    assert_eq!(
        user_read(&mut mws, "hive:view.ready").await.unwrap()["may_prompt"],
        true
    );
    assert_eq!(
        get_session(&app, &tid, &owner, &sid).await.1["drivers"],
        json!([mid]),
    );
    // Saying it again changes nothing, and ends nothing.
    assert_eq!(
        set_part(&app, &tid, &owner, &sid, &mid, "driver").await.0,
        200
    );
    assert!(
        !device_hears(
            &mut dev.ws,
            "rc:hive.view.close",
            Duration::from_millis(300)
        )
        .await,
        "no change, no ended view"
    );

    // What is not a participant change.
    assert_eq!(
        set_part(&app, &tid, &owner, &sid, &seeded.admin.id, "reader")
            .await
            .0,
        400,
        "the owner always drives"
    );
    assert_eq!(
        set_part(&app, &tid, &owner, &sid, &mid, "admin").await.0,
        400
    );
    assert_eq!(
        set_part(
            &app,
            &tid,
            &owner,
            &sid,
            &ObjectId::new().to_hex(),
            "reader"
        )
        .await
        .0,
        404,
        "a stranger's id answers like a bogus one"
    );

    // Taken out: out of the room, driving nothing, reading nothing.
    let (code, body) = remove_part(&app, &tid, &owner, &sid, &mid).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(role_of(&body, &mid), None);
    assert_eq!(
        read_until(&mut dev.ws, "rc:hive.view.close").await.unwrap()["reason"],
        "removed"
    );
    assert_eq!(get_session(&app, &tid, &member, &sid).await.0, 404);
    assert!(no_drivers(&stored(&app, &sid).await));
    // Again: nothing to do, nothing said.
    assert_eq!(remove_part(&app, &tid, &owner, &sid, &mid).await.0, 200);
    let audit = app.db.collection::<Document>("hive_audit");
    for (outcome, n) in [("reader", 1), ("driver", 1), ("removed", 1)] {
        assert_eq!(
            audit
                .count_documents(doc! { "action": "participant", "outcome": outcome })
                .await
                .unwrap(),
            n,
            "{outcome}"
        );
    }
}

/// P1c — what a device says about WHO acted is believed only when it names
/// a driver: who answered an approval, who asked a turn. A device naming a
/// reader is not believed — no stub says a person did what they could not
/// have. And an approval is pushed to every driver still in the room.
#[tokio::test]
async fn a_drivers_word_is_believed_and_a_readers_is_not() {
    let app = hive_app_polling().await;
    let seeded = app.seed_tenant("hivewho").await;
    let tid = seeded.tenant_id.clone();
    let owner = seeded.admin.access_token.clone();
    let mid = seeded.member.id.clone();
    grant_hive_run_to_member(&app, &seeded).await;
    let mut dev = device(&app, &seeded, "hive-who", RUNS_HIVE).await;
    let sid = started_session(&app, &tid, &owner, &mut dev).await;
    let room = get_session(&app, &tid, &owner, &sid).await.1["room_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        set_part(&app, &tid, &owner, &sid, &mid, "reader").await.0,
        200
    );

    let turn = |n: u32, by: &str| {
        json!({"t": "rc:hive.turn", "session_id": sid, "fence": 1, "turn": n,
               "status": "ok", "prompted_by": by, "steps": 0})
    };
    let approval = |id: &str, status: &str, by: Option<&str>| {
        let mut f = json!({"t": "rc:hive.approval", "session_id": sid, "fence": 1,
                           "approval_id": id, "turn": 1, "status": status});
        if let Some(by) = by {
            f["answered_by"] = json!(by);
        }
        f
    };
    let stub = |items: &[Value], reference: &str| -> String {
        items
            .iter()
            .find(|m| bound_to(m, reference))
            .and_then(|m| m["content"].as_str())
            .unwrap_or_default()
            .to_string()
    };

    // As a reader: neither claim is believed.
    send(&mut dev.ws, turn(1, &mid)).await;
    send(&mut dev.ws, approval("r1", "open", None)).await;
    send(&mut dev.ws, approval("r1", "allowed", Some(&mid))).await;
    let (t1, r1) = (format!("{sid}#1"), format!("{sid}!r1"));
    let items = wait_messages(&app, &tid, &owner, &room, "the reader's claims", |items| {
        items.iter().any(|m| bound_to(m, &t1)) && stub(items, &r1).contains("allowed")
    })
    .await;
    assert!(
        !stub(&items, &t1).contains("asked by"),
        "{}",
        stub(&items, &t1)
    );
    assert_eq!(stub(&items, &r1), "🔐 **Approval** · turn 1 — ✅ allowed");
    assert_eq!(
        approval_pushes(&app).await,
        std::slice::from_ref(&seeded.admin.id),
        "a reader is not asked to answer"
    );

    // As a driver: both are, and the push reaches them too.
    assert_eq!(
        set_part(&app, &tid, &owner, &sid, &mid, "driver").await.0,
        200
    );
    send(&mut dev.ws, turn(2, &mid)).await;
    send(&mut dev.ws, approval("d1", "open", None)).await;
    send(&mut dev.ws, approval("d1", "denied", Some(&mid))).await;
    let (t2, d1) = (format!("{sid}#2"), format!("{sid}!d1"));
    let items = wait_messages(&app, &tid, &owner, &room, "the driver's claims", |items| {
        stub(items, &t2).contains("asked by") && stub(items, &d1).contains("denied by")
    })
    .await;
    assert!(
        stub(&items, &t2).contains("Member"),
        "asked by the driver: {}",
        stub(&items, &t2)
    );
    assert!(
        stub(&items, &d1).contains("denied by **hivewho Member**"),
        "{}",
        stub(&items, &d1)
    );
    let mut want = vec![
        seeded.admin.id.clone(),
        seeded.admin.id.clone(),
        mid.clone(),
    ];
    want.sort();
    assert_eq!(
        approval_pushes(&app).await,
        want,
        "r1 to the owner; d1 to the owner AND the driver"
    );
}

/// P1c — a member who leaves the org drives nobody's session any more: a
/// seat nobody re-offered must not come back if they rejoin.
#[tokio::test]
async fn a_member_who_leaves_the_org_drives_nothing() {
    let app = hive_app_polling().await;
    let seeded = app.seed_tenant("hivegone").await;
    let tid = seeded.tenant_id.clone();
    let owner = seeded.admin.access_token.clone();
    let mid = seeded.member.id.clone();
    grant_hive_run_to_member(&app, &seeded).await;
    let mut dev = device(&app, &seeded, "hive-gone", RUNS_HIVE).await;
    let sid = started_session(&app, &tid, &owner, &mut dev).await;
    assert_eq!(
        set_part(&app, &tid, &owner, &sid, &mid, "driver").await.0,
        200
    );
    assert_eq!(
        stored(&app, &sid).await.get_array("drivers").unwrap().len(),
        1
    );

    let resp = app
        .auth_delete(&format!("/api/tenant/{tid}/member/{mid}"), &owner)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "remove: {:?}", resp.status());
    let s = stored(&app, &sid).await;
    assert!(no_drivers(&s), "dropped from the drivers: {s}");
    assert_ne!(
        s.get_str("status").unwrap(),
        "ended",
        "the OWNER's session runs on — only the member's own end"
    );
}

// ─── P1e — core memory, the brain's first layer ─────────────────────────

/// What a build that understands core memory advertises.
const TAKES_MEMORY: &[&str] = &["exec", "hive", "hive-memory"];

fn brain_url(tid: &str, fact: Option<&str>) -> String {
    match fact {
        Some(f) => format!("/api/tenant/{tid}/hive/brain/{f}"),
        None => format!("/api/tenant/{tid}/hive/brain"),
    }
}

/// `POST …/hive/brain`.
async fn keep(app: &TestApp, tid: &str, token: &str, body: Value) -> (u16, Value) {
    status_and_json(
        app.auth_post(&brain_url(tid, None), token)
            .json(&body)
            .send()
            .await
            .unwrap(),
    )
    .await
}

/// `PUT …/hive/brain/{fact}`.
async fn edit_fact(app: &TestApp, tid: &str, token: &str, fact: &str, body: Value) -> (u16, Value) {
    status_and_json(
        app.auth_put(&brain_url(tid, Some(fact)), token)
            .json(&body)
            .send()
            .await
            .unwrap(),
    )
    .await
}

/// `DELETE …/hive/brain/{fact}`.
async fn archive_fact(app: &TestApp, tid: &str, token: &str, fact: &str) -> (u16, Value) {
    status_and_json(
        app.auth_delete(&brain_url(tid, Some(fact)), token)
            .send()
            .await
            .unwrap(),
    )
    .await
}

/// `GET …/hive/brain`, with a device's facts too when one is named.
async fn brain(app: &TestApp, tid: &str, token: &str, device: Option<&str>) -> Value {
    let url = match device {
        Some(d) => format!("{}?device_id={d}", brain_url(tid, None)),
        None => brain_url(tid, None),
    };
    let (code, body) = status_and_json(app.auth_get(&url, token).send().await.unwrap()).await;
    assert_eq!(code, 200, "{body}");
    body
}

fn budget_of(view: &Value, scope: &str) -> Value {
    view["budgets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["scope"] == scope)
        .cloned()
        .unwrap_or_else(|| panic!("no {scope} budget in {view}"))
}

/// The brain's audit rows, by outcome.
async fn brain_audit(app: &TestApp) -> Vec<Document> {
    let mut cursor = app
        .db
        .collection::<Document>("hive_audit")
        .find(doc! { "action": "brain" })
        .await
        .unwrap();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next().await {
        rows.push(row.unwrap());
    }
    rows
}

/// AC8's second half: a write past its scope's budget fails VISIBLY — the
/// numbers in the answer, nothing evicted — and the budget follows every
/// add, edit and archive.
#[tokio::test]
async fn core_memory_keeps_facts_inside_their_budgets_and_says_so() {
    let app = hive_app_polling().await;
    let seeded = app.seed_tenant("brainbudget").await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();

    let first = "Deploys go through the release branch.";
    let (code, f) = keep(&app, &tid, &admin, json!({"scope": "org", "text": first})).await;
    assert_eq!(code, 200, "{f}");
    assert_eq!(f["scope"], "org");
    assert_eq!(f["kind"], "convention", "the default kind");
    assert_eq!(f["version"], 1);
    assert!(f.get("owner_id").is_none(), "an org fact has no owner: {f}");
    let view = brain(&app, &tid, &admin, None).await;
    assert_eq!(view["brain_rev"], 1);
    assert_eq!(budget_of(&view, "org")["used"], first.len());
    assert_eq!(budget_of(&view, "org")["budget"], 3000);

    let big = "x".repeat(500);
    for _ in 0..5 {
        let (code, b) = keep(
            &app,
            &tid,
            &admin,
            json!({"scope": "org", "text": big, "kind": "gotcha"}),
        )
        .await;
        assert_eq!(code, 200, "{b}");
    }
    let used = first.len() as i64 + 2_500;
    let (code, refused) = keep(&app, &tid, &admin, json!({"scope": "org", "text": big})).await;
    assert_eq!(code, 409, "{refused}");
    assert_eq!(refused["error"], "over_budget");
    assert_eq!(refused["scope"], "org");
    assert_eq!(refused["used"], used);
    assert_eq!(refused["budget"], 3000);
    assert_eq!(refused["needed"], 500);
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("archive a fact"),
        "the refusal names the way out: {refused}"
    );
    let view = brain(&app, &tid, &admin, None).await;
    assert_eq!(
        view["facts"].as_array().unwrap().len(),
        6,
        "nothing evicted"
    );
    assert_eq!(budget_of(&view, "org")["used"], used);
    assert_eq!(view["brain_rev"], 6, "a refusal is no revision");

    // One character more, so growing the first fact to the longest a fact
    // may be no longer fits (without it, it would fit exactly).
    let (code, z) = keep(&app, &tid, &admin, json!({"scope": "org", "text": "z"})).await;
    assert_eq!(code, 200, "{z}");
    let others = 2_501_i64;

    // An edit that grows a fact past the budget is refused the same way;
    // one that fits is the next version; a stale one is a conflict.
    let fid = f["id"].as_str().unwrap().to_string();
    let (code, e) = edit_fact(
        &app,
        &tid,
        &admin,
        &fid,
        json!({"text": "y".repeat(500), "version": 1}),
    )
    .await;
    assert_eq!(
        (code, e["error"].as_str()),
        (409, Some("over_budget")),
        "{e}"
    );
    let shorter = "Deploys go through `release`.";
    let (code, e) = edit_fact(
        &app,
        &tid,
        &admin,
        &fid,
        json!({"text": shorter, "version": 1}),
    )
    .await;
    assert_eq!(code, 200, "{e}");
    assert_eq!(e["version"], 2);
    let (code, e) = edit_fact(
        &app,
        &tid,
        &admin,
        &fid,
        json!({"text": "again", "version": 1}),
    )
    .await;
    assert_eq!(
        (code, e["error"].as_str()),
        (409, Some("conflict")),
        "stale: {e}"
    );
    let view = brain(&app, &tid, &admin, None).await;
    assert_eq!(
        budget_of(&view, "org")["used"],
        shorter.len() as i64 + others
    );

    // Archiving gives the room back.
    let (code, a) = archive_fact(&app, &tid, &admin, &fid).await;
    assert_eq!((code, a["archived"].as_bool()), (200, Some(true)), "{a}");
    let (code, a) = archive_fact(&app, &tid, &admin, &fid).await;
    assert_eq!(
        (code, a["archived"].as_bool()),
        (200, Some(false)),
        "once: {a}"
    );
    let view = brain(&app, &tid, &admin, None).await;
    assert_eq!(budget_of(&view, "org")["used"], others);
    assert!(
        view["facts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["id"] != fid.as_str())
    );
    let fits = "w".repeat(499);
    let (code, b) = keep(&app, &tid, &admin, json!({"scope": "org", "text": fits})).await;
    assert_eq!(code, 200, "now it fits, exactly: {b}");
    let view = brain(&app, &tid, &admin, None).await;
    assert_eq!(budget_of(&view, "org")["used"], 3_000);

    let rows = brain_audit(&app).await;
    let count = |outcome: &str| {
        rows.iter()
            .filter(|r| r.get_str("outcome").ok() == Some(outcome))
            .count()
    };
    assert_eq!(
        (
            count("added"),
            count("refused"),
            count("edited"),
            count("archived")
        ),
        (8, 2, 1, 1),
        "every write audited: {rows:?}"
    );
    assert!(
        rows.iter().all(|r| !r.contains_key("device_id")),
        "an org fact concerns no device"
    );
}

/// The org's memory is its administrators'; a person's is theirs, with
/// `HIVE_RUN`; a device's is whoever manages devices'. Someone else's user
/// memory answers like a bogus id.
#[tokio::test]
async fn who_keeps_which_memory() {
    let app = hive_app_polling().await;
    let seeded = app.seed_tenant("brainwho").await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let member = seeded.member.access_token.clone();
    let dev = device(&app, &seeded, "brain-who", TAKES_MEMORY).await;

    assert_eq!(
        keep(&app, &tid, &member, json!({"scope": "org", "text": "t"}))
            .await
            .0,
        403
    );
    assert_eq!(
        keep(&app, &tid, &member, json!({"scope": "user", "text": "t"}))
            .await
            .0,
        403,
        "no HIVE_RUN, no memory for agent sessions"
    );
    grant_hive_run_to_member(&app, &seeded).await;
    let (code, mine) = keep(
        &app,
        &tid,
        &member,
        json!({"scope": "user", "text": "Small commits.", "kind": "preference"}),
    )
    .await;
    assert_eq!(code, 200, "{mine}");
    assert_eq!(mine["owner_id"], seeded.member.id, "their own by default");
    assert_eq!(
        keep(
            &app,
            &tid,
            &member,
            json!({"scope": "user", "owner_id": seeded.admin.id, "text": "t"})
        )
        .await
        .0,
        403,
        "never someone else's"
    );
    assert_eq!(
        keep(
            &app,
            &tid,
            &member,
            json!({"scope": "device", "owner_id": dev.agent_id, "text": "t"})
        )
        .await
        .0,
        403
    );
    let (code, d) = keep(
        &app,
        &tid,
        &admin,
        json!({"scope": "device", "owner_id": dev.agent_id, "text": "Datasets in /data.", "kind": "path"}),
    )
    .await;
    assert_eq!(code, 200, "{d}");
    assert_eq!(
        keep(
            &app,
            &tid,
            &admin,
            json!({"scope": "device", "owner_id": ObjectId::new().to_hex(), "text": "t"})
        )
        .await
        .0,
        404,
        "a device not in this org answers like a bogus id"
    );
    assert_eq!(
        keep(
            &app,
            &tid,
            &admin,
            json!({"scope": "org", "owner_id": seeded.admin.id, "text": "t"})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        keep(&app, &tid, &admin, json!({"scope": "project", "text": "t"}))
            .await
            .0,
        400
    );
    assert_eq!(
        keep(&app, &tid, &admin, json!({"scope": "org", "text": " \n "}))
            .await
            .0,
        400
    );

    let mid = mine["id"].as_str().unwrap().to_string();
    assert_eq!(
        archive_fact(&app, &tid, &admin, &mid).await.0,
        404,
        "not even an admin's"
    );
    let view = brain(&app, &tid, &admin, None).await;
    assert!(
        view["facts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["id"] != mid.as_str())
    );
    let view = brain(&app, &tid, &member, Some(&dev.agent_id)).await;
    let scopes: Vec<&str> = view["facts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["scope"].as_str().unwrap())
        .collect();
    assert_eq!(scopes, ["user", "device"], "{view}");
    assert_eq!(budget_of(&view, "device")["budget"], 800);

    let rows = brain_audit(&app).await;
    let device_row = rows
        .iter()
        .find(|r| r.get_str("outcome").ok() == Some("added") && r.contains_key("device_id"))
        .expect("a device fact's audit names the device");
    assert_eq!(
        device_row.get_object_id("device_id").unwrap().to_hex(),
        dev.agent_id
    );
}

/// Two writes racing for the last room: ONE conditional update decides, so
/// exactly one fits.
#[tokio::test]
async fn two_writes_racing_for_the_last_room_cannot_both_fit() {
    let app = hive_app_polling().await;
    let seeded = app.seed_tenant("brainrace").await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let big = "x".repeat(500);
    for _ in 0..5 {
        assert_eq!(
            keep(&app, &tid, &admin, json!({"scope": "org", "text": big}))
                .await
                .0,
            200
        );
    }
    let (a, b) = tokio::join!(
        keep(&app, &tid, &admin, json!({"scope": "org", "text": big})),
        keep(&app, &tid, &admin, json!({"scope": "org", "text": big})),
    );
    let mut codes = [a.0, b.0];
    codes.sort_unstable();
    assert_eq!(codes, [200, 409], "{a:?} {b:?}");
    let view = brain(&app, &tid, &admin, None).await;
    assert_eq!(budget_of(&view, "org")["used"], 3_000);
    assert_eq!(view["facts"].as_array().unwrap().len(), 6);
}

/// AC8's first half, on the server: a session's core memory is rendered when
/// it is created, reaches the device BEFORE its start, and is FROZEN — a fact
/// written afterwards is in the next session's snapshot and never in this one.
#[tokio::test]
async fn a_sessions_core_memory_comes_before_its_start_and_is_frozen() {
    let app = hive_app_polling().await;
    let seeded = app.seed_tenant("brainsnap").await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "brain-snap", TAKES_MEMORY).await;
    for (scope, text, owner) in [
        ("org", "ORG-FACT-ONE", None),
        ("user", "MY-FACT", None),
        ("device", "DEVICE-FACT", Some(dev.agent_id.clone())),
    ] {
        let mut body = json!({"scope": scope, "text": text});
        if let Some(o) = owner {
            body["owner_id"] = json!(o);
        }
        assert_eq!(keep(&app, &tid, &admin, body).await.0, 200);
    }

    let caller = start(&app, &tid, &admin, &dev.agent_id, "/srv");
    let target = async {
        // In order: the memory, THEN the start (a start first would be
        // skipped by the first wait, and the second would time out).
        let memory = read_until(&mut dev.ws, "rc:hive.memory")
            .await
            .expect("the memory");
        let start = read_until(&mut dev.ws, "rc:hive.start")
            .await
            .expect("then the start");
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": start["session_id"], "fence": 1, "account": "dev"}),
        )
        .await;
        (memory, start)
    };
    let (body, (memory, start_frame)) = tokio::join!(caller, target);
    let sid = body["session"]["id"].as_str().unwrap().to_string();
    assert_eq!(memory["session_id"], sid.as_str());
    assert_eq!(start_frame["session_id"], sid.as_str());
    assert_eq!(memory["fence"], 1);
    assert_eq!(memory["brain_rev"], 3);
    assert_eq!(body["session"]["brain_rev"], 3, "pinned on the record");
    let claude_md = memory["claude_md"].as_str().unwrap();
    assert!(
        claude_md.contains("ORG-FACT-ONE") && claude_md.contains("MY-FACT"),
        "{claude_md}"
    );
    assert!(!claude_md.contains("DEVICE-FACT"), "{claude_md}");
    assert!(
        memory["memory_md"]
            .as_str()
            .unwrap()
            .contains("DEVICE-FACT"),
        "{memory}"
    );
    for absent in ["prompt", "transcript", "account"] {
        assert!(memory.get(absent).is_none(), "`{absent}` in {memory}");
    }

    // A fact written now is not this session's…
    assert_eq!(
        keep(
            &app,
            &tid,
            &admin,
            json!({"scope": "org", "text": "ORG-FACT-TWO"})
        )
        .await
        .0,
        200
    );
    let stored = app
        .db
        .collection::<Document>("hive_session_memory")
        .find_one(doc! { "_id": ObjectId::parse_str(&sid).unwrap() })
        .await
        .unwrap()
        .expect("the snapshot is kept for a re-send");
    assert!(
        !stored
            .get_str("claude_md")
            .unwrap()
            .contains("ORG-FACT-TWO"),
        "frozen: {stored}"
    );
    assert_eq!(stored.get_i64("brain_rev").unwrap(), 3);

    // …and is the next one's.
    let caller = start(&app, &tid, &admin, &dev.agent_id, "/srv");
    let target = async {
        let memory = read_until(&mut dev.ws, "rc:hive.memory")
            .await
            .expect("the memory");
        let start = read_until(&mut dev.ws, "rc:hive.start")
            .await
            .expect("then the start");
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": start["session_id"], "fence": 1, "account": "dev"}),
        )
        .await;
        memory
    };
    let (_, next) = tokio::join!(caller, target);
    assert_eq!(next["brain_rev"], 4);
    let claude_md = next["claude_md"].as_str().unwrap();
    assert!(
        claude_md.contains("ORG-FACT-ONE") && claude_md.contains("ORG-FACT-TWO"),
        "{claude_md}"
    );
}

/// A device that does not advertise `hive-memory` is never sent core memory
/// — and its session starts all the same.
#[tokio::test]
async fn a_device_without_hive_memory_gets_none_and_still_starts() {
    let app = hive_app_polling().await;
    let seeded = app.seed_tenant("brainnone").await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "brain-none", RUNS_HIVE).await;
    assert_eq!(
        keep(
            &app,
            &tid,
            &admin,
            json!({"scope": "org", "text": "ORG-FACT"})
        )
        .await
        .0,
        200
    );

    let caller = start(&app, &tid, &admin, &dev.agent_id, "/srv");
    let target = async {
        let mut seen: Vec<String> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let Ok(Some(Ok(Message::Text(text)))) =
                tokio::time::timeout(Duration::from_millis(100), dev.ws.next()).await
            else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let t = v["t"].as_str().unwrap_or_default().to_string();
            let is_start = t == "rc:hive.start";
            seen.push(t);
            if is_start {
                send(
                    &mut dev.ws,
                    json!({"t": "rc:hive.start_ack", "session_id": v["session_id"], "fence": 1, "account": "dev"}),
                )
                .await;
                break;
            }
        }
        seen
    };
    let (body, seen) = tokio::join!(caller, target);
    assert_eq!(body["outcome"], "accepted", "{body}");
    assert!(seen.iter().any(|t| t == "rc:hive.start"), "{seen:?}");
    assert!(!seen.iter().any(|t| t == "rc:hive.memory"), "{seen:?}");
    assert!(body["session"].get("brain_rev").is_none(), "{body}");
}

/// A start re-sent on connect carries the SAME frozen snapshot ahead of it,
/// even when the brain moved on in between.
#[tokio::test]
async fn a_re_sent_start_carries_the_same_snapshot_first() {
    let app = hive_app_polling().await;
    let seeded = app.seed_tenant("brainresend").await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut dev = device(&app, &seeded, "brain-resend", TAKES_MEMORY).await;
    assert_eq!(
        keep(
            &app,
            &tid,
            &admin,
            json!({"scope": "org", "text": "ORG-FACT-ONE"})
        )
        .await
        .0,
        200
    );

    let caller = start(&app, &tid, &admin, &dev.agent_id, "/srv");
    let target = async { read_until(&mut dev.ws, "rc:hive.memory").await.unwrap() };
    let (body, first) = tokio::join!(caller, target);
    assert_eq!(
        body["outcome"], "pending",
        "the device stayed silent: {body}"
    );
    let sid = body["session"]["id"].as_str().unwrap().to_string();

    assert_eq!(
        keep(
            &app,
            &tid,
            &admin,
            json!({"scope": "org", "text": "ORG-FACT-TWO"})
        )
        .await
        .0,
        200
    );
    drop(dev.ws);
    wait_offline(&app, &seeded, &dev.agent_id).await;
    let mut ws = connect(&app, &dev.token, &dev.machine, TAKES_MEMORY).await;
    let again = read_until(&mut ws, "rc:hive.memory")
        .await
        .expect("the memory again");
    let start = read_until(&mut ws, "rc:hive.start")
        .await
        .expect("then the start again");
    assert_eq!(again["session_id"], sid.as_str());
    assert_eq!(start["session_id"], sid.as_str());
    assert_eq!(again["claude_md"], first["claude_md"], "the same snapshot");
    assert_eq!(again["brain_rev"], first["brain_rev"]);
    assert!(
        !again["claude_md"]
            .as_str()
            .unwrap()
            .contains("ORG-FACT-TWO")
    );
}

// ─── P1j — terminal sessions a person adopted (`rc:hive.adopt`) ─────────────

/// The device offers a terminal session; the server's answer.
async fn adopt(ws: &mut AgentWs, adopt_id: &str, harness_session: &str, keys: &[&str]) -> Value {
    send(
        ws,
        json!({
            "t": "rc:hive.adopt",
            "adopt_id": adopt_id,
            "harness_session": harness_session,
            "keys": keys,
            "account": "alice",
            "folder": "/home/alice/client-x",
        }),
    )
    .await;
    let ack = read_until(ws, "rc:hive.adopt_ack")
        .await
        .expect("an answer to the offer");
    assert_eq!(ack["adopt_id"], adopt_id, "{ack}");
    ack
}

/// Decision 11 on the server: an adopted terminal session is attributed
/// through the device's keys, never by guess; it is its owner's alone, the
/// org's admin included; it is read-only, with readers by name and no
/// drivers; and one terminal session is one record, however often offered.
#[tokio::test]
async fn an_adopted_terminal_session_is_its_owners_alone() {
    let app = hive_app_polling().await;
    let seeded = app.seed_tenant("hiveadopt").await;
    let (owner, admin) = (&seeded.member, &seeded.admin);
    let tid = seeded.tenant_id.clone();
    let mut dev = device(
        &app,
        &seeded,
        "adopter",
        &["hive", "hive-view", "hive-adopt"],
    )
    .await;

    let uuid = "6f1c8a2e-3b7d-4e5f-9a10-2b3c4d5e6f70";
    let ack = adopt(&mut dev.ws, "a1", uuid, &[&owner.email]).await;
    assert!(ack.get("refused").is_none(), "{ack}");
    assert_eq!(ack["fence"], 1);
    let sid = ack["session_id"].as_str().unwrap().to_string();

    // Its owner's: read-only, named after its folder (no title is offered).
    let (code, s) = get_session(&app, &tid, &owner.access_token, &sid).await;
    assert_eq!(code, 200, "{s}");
    assert_eq!(
        (
            s["origin"].clone(),
            s["status"].clone(),
            s["title"].clone(),
            s["account"].clone()
        ),
        (
            json!("adopted"),
            json!("idle"),
            json!("client-x"),
            json!("alice")
        ),
        "{s}"
    );
    let row = stored(&app, &sid).await;
    assert_eq!(row.get_str("origin").unwrap(), "adopted");
    // Nobody else's — the org's admin included.
    let (code, _) = get_session(&app, &tid, &admin.access_token, &sid).await;
    assert_eq!(code, 404, "an admin does not read someone's terminal");

    // No drivers, even ones the org trusts; readers by name.
    let (code, body) = set_part(&app, &tid, &owner.access_token, &sid, &admin.id, "driver").await;
    assert_eq!(code, 409, "{body}");
    let (code, body) = set_part(&app, &tid, &owner.access_token, &sid, &admin.id, "reader").await;
    assert_eq!(code, 200, "{body}");
    let (code, _) = get_session(&app, &tid, &admin.access_token, &sid).await;
    assert_eq!(code, 200, "a reader the owner named reads it");

    // The same terminal offered again — a restart, a lost ack: the same record.
    let again = adopt(&mut dev.ws, "a2", uuid, &[&owner.email]).await;
    assert_eq!(again["session_id"], sid.as_str(), "{again}");

    // Attributed by the keys, never by guess.
    let two = adopt(
        &mut dev.ws,
        "a3",
        "11111111-2222-3333-4444-555555555555",
        &[&owner.email, &admin.email],
    )
    .await;
    assert_eq!(
        two["refused"], "ambiguous_account",
        "two people behind one account: {two}"
    );
    let nobody = adopt(
        &mut dev.ws,
        "a4",
        "22222222-2222-3333-4444-555555555555",
        &["nobody@example.com"],
    )
    .await;
    assert_eq!(nobody["refused"], "no_account", "{nobody}");
    let placeholder = format!("{}.invalid", owner.email);
    let unproven = adopt(
        &mut dev.ws,
        "a5",
        "33333333-2222-3333-4444-555555555555",
        &[&placeholder],
    )
    .await;
    assert_eq!(
        unproven["refused"], "no_account",
        "a placeholder names nobody: {unproven}"
    );
    let elsewhere = app.seed_tenant("hiveadopt2").await;
    let foreign = adopt(
        &mut dev.ws,
        "a6",
        "44444444-2222-3333-4444-555555555555",
        &[&elsewhere.member.email],
    )
    .await;
    assert_eq!(foreign["refused"], "not_a_member", "{foreign}");
    // By user id, and an address in another case, both name the owner.
    let by_id = adopt(
        &mut dev.ws,
        "a7",
        "55555555-2222-3333-4444-555555555555",
        &[&owner.id],
    )
    .await;
    assert!(by_id.get("refused").is_none(), "{by_id}");
    let upper = owner.email.to_ascii_uppercase();
    let by_case = adopt(
        &mut dev.ws,
        "a8",
        "66666666-2222-3333-4444-555555555555",
        &[&upper],
    )
    .await;
    assert!(by_case.get("refused").is_none(), "{by_case}");

    // The device's owner remapped the account: the same terminal session now
    // names someone else. Its record ends; nothing more is mirrored into it.
    let remapped_sid = by_id["session_id"].as_str().unwrap().to_string();
    let moved = adopt(
        &mut dev.ws,
        "a9",
        "55555555-2222-3333-4444-555555555555",
        &[&admin.email],
    )
    .await;
    assert_eq!(moved["refused"], "ambiguous_account", "{moved}");
    let row = stored(&app, &remapped_sid).await;
    assert_eq!(
        (
            row.get_str("status").unwrap(),
            row.get_str("end_reason").unwrap()
        ),
        ("ended", "attribution_changed"),
        "the record held for the old owner ends"
    );

    // The device's reports move it like any session's.
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.state", "session_id": sid, "fence": 1, "state": "ended", "detail": "the terminal closed"}),
    )
    .await;
    let s = wait_status(&app, &tid, &owner.access_token, &sid, "ended").await;
    assert_eq!(s["origin"], "adopted", "{s}");
}

/// The gates before attribution: a device that did not say it adopts is not
/// answered at all, and an org agent sessions do not serve is refused in
/// those words.
#[tokio::test]
async fn an_adopt_offer_passes_the_device_and_org_gates_first() {
    let first = hive_app().await;
    let a = first.seed_tenant("hiveadoptgate").await;
    let b = first.seed_tenant("hiveadoptother").await;
    // Hive serves B only.
    let app = serving_only(&first, &b.tenant_id).await;

    let mut quiet = device(&app, &a, "noadopt", &["hive", "hive-view"]).await;
    send(
        &mut quiet.ws,
        json!({
            "t": "rc:hive.adopt", "adopt_id": "q1",
            "harness_session": "77777777-2222-3333-4444-555555555555",
            "keys": [a.member.email.clone()], "account": "alice", "folder": "/home/alice/x",
        }),
    )
    .await;
    assert!(
        read_until(&mut quiet.ws, "rc:hive.adopt_ack")
            .await
            .is_none(),
        "a device that did not advertise hive-adopt is not answered"
    );

    let mut unserved = device(&app, &a, "adopter2", &["hive", "hive-view", "hive-adopt"]).await;
    let ack = adopt(
        &mut unserved.ws,
        "u1",
        "88888888-2222-3333-4444-555555555555",
        &[&a.member.email],
    )
    .await;
    assert_eq!(ack["refused"], "hive_not_enabled", "{ack}");
    let none = app
        .db
        .collection::<Document>("agent_sessions")
        .count_documents(doc! { "origin": "adopted" })
        .await
        .unwrap();
    assert_eq!(none, 0, "no record for a refused or unanswered offer");
}

// ─── P2c-2a — the replica policy (`hive.replicaset`) ───────────────────────

/// [`hive_app`] with the replicaset's server switch on.
async fn replicaset_app() -> TestApp {
    TestApp::spawn_with_settings(|s| {
        s.modules.hive = true;
        s.hive.replicaset = true;
        // Polls, like `hive_app_polling`: the default limiter answers a poll
        // loop 429 before the condition it waits for, so a wait that runs to
        // its deadline would fail on the 429, not on what it waited for.
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await
}

async fn get_policy(app: &TestApp, tid: &str, token: &str) -> (u16, Value) {
    let resp = app
        .auth_get(&format!("/api/tenant/{tid}/hive/policy"), token)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn put_policy(app: &TestApp, tid: &str, token: &str, body: &Value) -> (u16, Value) {
    let resp = app
        .auth_put(&format!("/api/tenant/{tid}/hive/policy"), token)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn a_policy(revision: i64) -> Value {
    json!({
        "replicaset": {
            "min": 3,
            "max": 5,
            "archive": false,
            "prefer": ["owner_devices"],
            "restricted_tags": [" pci ", "pci", ""],
        },
        "retention_days": 30,
        "archive_devices": [],
        "revision": revision,
    })
}

/// With the replicaset's switch off, the policy is not there for anyone.
#[tokio::test]
async fn the_replica_policy_is_not_there_while_its_switch_is_off() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hivepolicyoff").await;
    let admin = &seeded.admin.access_token;
    assert_eq!(get_policy(&app, &seeded.tenant_id, admin).await.0, 404);
    assert_eq!(
        put_policy(&app, &seeded.tenant_id, admin, &a_policy(0))
            .await
            .0,
        404
    );
}

/// An organization starts with decision 2's defaults. Any member reads the
/// policy; only an administrator writes it, from the revision they read, an
/// archive device must be a live device of this organization, and every
/// attempt is audited.
#[tokio::test]
async fn an_administrator_sets_the_replica_policy_from_the_revision_they_read() {
    let app = replicaset_app().await;
    let seeded = app.seed_tenant("hivepolicy").await;
    let tid = seeded.tenant_id.as_str();
    let admin = seeded.admin.access_token.as_str();
    let member = seeded.member.access_token.as_str();

    let (s, p) = get_policy(&app, tid, member).await;
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["is_default"], true);
    assert_eq!(p["revision"], 0);
    assert_eq!(
        p["replicaset"],
        json!({
            "min": 2, "max": 4, "archive": true,
            "prefer": ["owner_devices"], "restricted_tags": ["prod"],
        })
    );
    assert_eq!(p["retention_days"], 90);

    assert_eq!(put_policy(&app, tid, member, &a_policy(0)).await.0, 403);

    let (s, p) = put_policy(&app, tid, admin, &a_policy(0)).await;
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["revision"], 1);
    assert_eq!(p["is_default"], false);
    assert_eq!(
        p["replicaset"]["restricted_tags"],
        json!(["pci"]),
        "normalized"
    );
    assert_eq!(p["updated_by"], seeded.admin.id.as_str());
    assert_eq!(get_policy(&app, tid, member).await.1["revision"], 1);

    let (s, _) = put_policy(&app, tid, admin, &a_policy(0)).await;
    assert_eq!(s, 409, "a write from the revision before is stale");

    let mut next = a_policy(1);
    next["replicaset"]["min"] = json!(1);
    let (s, p) = put_policy(&app, tid, admin, &next).await;
    assert_eq!(s, 200, "{p}");
    assert_eq!(
        (p["revision"].as_i64(), p["replicaset"]["min"].as_i64()),
        (Some(2), Some(1))
    );
    let (s, _) = put_policy(&app, tid, admin, &next).await;
    assert_eq!(s, 409, "revision 1 is no longer the stored one");

    let mut bad = a_policy(2);
    bad["replicaset"]["max"] = json!(0);
    assert_eq!(put_policy(&app, tid, admin, &bad).await.0, 400);

    let other = app.seed_tenant("hivepolicyother").await;
    let (foreign, _) = enroll_agent(&app, &other, "mach-policy-foreign", "foreign").await;
    let mut archive = a_policy(2);
    archive["archive_devices"] = json!([foreign]);
    assert_eq!(
        put_policy(&app, tid, admin, &archive).await.0,
        400,
        "another organization's device"
    );
    let agents = app.db.collection::<Document>("agents");
    let (removed, _) = enroll_agent(&app, &seeded, "mach-policy-removed", "removed").await;
    agents
        .update_one(
            doc! { "_id": ObjectId::parse_str(&removed).unwrap() },
            doc! { "$set": { "deleted_at": bson::DateTime::now() } },
        )
        .await
        .unwrap();
    archive["archive_devices"] = json!([removed]);
    assert_eq!(
        put_policy(&app, tid, admin, &archive).await.0,
        400,
        "a removed device"
    );
    let (ephemeral, _) = enroll_agent(&app, &seeded, "mach-policy-eph", "eph").await;
    agents
        .update_one(
            doc! { "_id": ObjectId::parse_str(&ephemeral).unwrap() },
            doc! { "$set": { "ephemeral": true } },
        )
        .await
        .unwrap();
    archive["archive_devices"] = json!([ephemeral]);
    let (s, p) = put_policy(&app, tid, admin, &archive).await;
    assert_eq!(s, 400, "an ephemeral device: {p}");
    let (own, _) = enroll_agent(&app, &seeded, "mach-policy-own", "own").await;
    archive["archive_devices"] = json!([own, own]);
    let (s, p) = put_policy(&app, tid, admin, &archive).await;
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["archive_devices"], json!([own]), "once");

    let tid_oid = ObjectId::parse_str(tid).unwrap();
    let mut audits = app
        .db
        .collection::<Document>("hive_audit")
        .find(doc! { "tenant_id": tid_oid, "action": "policy" })
        .await
        .unwrap();
    let mut seen: Vec<(String, String)> = Vec::new();
    while let Some(d) = audits.next().await {
        let d = d.unwrap();
        seen.push((
            d.get_str("outcome").unwrap().to_string(),
            d.get_str("reason").unwrap_or("").to_string(),
        ));
    }
    seen.sort();
    let want = [
        ("refused", "ephemeral"),
        ("refused", "invalid"),
        ("refused", "no_permission"),
        ("refused", "not_a_device"),
        ("refused", "not_a_device"),
        ("refused", "stale"),
        ("refused", "stale"),
        ("set", ""),
        ("set", ""),
        ("set", ""),
    ]
    .map(|(o, r)| (o.to_string(), r.to_string()));
    assert_eq!(seen, want);
}

// ─── P2c-2b — placement (`hive.replicaset`) ────────────────────────────────

/// Set fields on a device's stored row, as its hello or an admin would.
async fn set_device(app: &TestApp, id: &str, set: Document) {
    app.db
        .collection::<Document>("agents")
        .update_one(
            doc! { "_id": ObjectId::parse_str(id).unwrap() },
            doc! { "$set": set },
        )
        .await
        .unwrap();
}

/// A device of the admin's that never connected, whose stored capabilities
/// are `rpc`: placement reads the last hello, so an offline device is placed
/// too.
async fn stored_device(
    app: &TestApp,
    seeded: &SeededTenant,
    machine: &str,
    rpc: &[&str],
) -> String {
    let (id, _) = enroll_agent(app, seeded, machine, machine).await;
    let rpc: Vec<String> = rpc.iter().map(|s| s.to_string()).collect();
    set_device(app, &id, doc! { "capabilities.rpc": rpc }).await;
    id
}

/// Start a session on `dev` as `token`, answered as the device would.
async fn started(app: &TestApp, tid: &str, token: &str, dev: &mut Device, folder: &str) -> Value {
    let caller = start(app, tid, token, &dev.agent_id, folder);
    let target = async {
        let frame = read_until(&mut dev.ws, "rc:hive.start")
            .await
            .expect("the device receives rc:hive.start");
        send(
            &mut dev.ws,
            json!({"t": "rc:hive.start_ack", "session_id": frame["session_id"],
                   "fence": frame["fence"], "account": "dev"}),
        )
        .await;
    };
    let (body, ()) = tokio::join!(caller, target);
    assert_eq!(body["outcome"], "accepted", "{body}");
    body["session"].clone()
}

fn members(s: &Value) -> Vec<(String, String)> {
    s["replicaset"]["members"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|m| {
            (
                m["device_id"].as_str().unwrap_or("").to_string(),
                m["role"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// With the replicaset's switch off, a session is placed nowhere: the
/// record carries no replicaset and nothing is audited.
#[tokio::test]
async fn a_session_started_with_the_switch_off_is_placed_nowhere() {
    let app = hive_app().await;
    let seeded = app.seed_tenant("hiveplaceoff").await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-placeoff-p", RUNS_HIVE).await;
    stored_device(&app, &seeded, "mach-placeoff-r", &["hive-replica"]).await;
    let s = started(&app, &tid, &admin, &mut primary, "/srv/off").await;
    assert!(s.get("replicaset").is_none(), "{s}");
    let sid = s["id"].as_str().unwrap();
    assert!(stored(&app, sid).await.get("replicaset").is_none());
    let places = app
        .db
        .collection::<Document>("hive_audit")
        .count_documents(doc! { "action": "place" })
        .await
        .unwrap();
    assert_eq!(places, 0);
}

/// Placement at a start, from the devices' stored rows: the designated
/// archive that offers itself, then the owner's own devices that offer,
/// last seen first, until `min`. Never a device that does not offer, an
/// ephemeral one, one handed to someone else, or a removed one; and with a
/// restricted primary, only devices carrying its tag, in any case.
#[tokio::test]
async fn placement_takes_the_archive_then_the_owners_offering_devices() {
    let app = replicaset_app().await;
    let seeded = app.seed_tenant("hiveplace").await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    // Eight devices: past the free plan's three.
    app.db
        .collection::<Document>("tenants")
        .update_one(
            doc! { "_id": ObjectId::parse_str(&tid).unwrap() },
            doc! { "$set": { "plan": "pro" } },
        )
        .await
        .unwrap();
    let mut primary = device(&app, &seeded, "mach-place-primary", RUNS_HIVE).await;
    // Online, and its hello offers copies.
    let laptop = device(
        &app,
        &seeded,
        "mach-place-laptop",
        &["exec", "hive", "hive-replica"],
    )
    .await;
    let old = stored_device(&app, &seeded, "mach-place-old", &["hive-replica"]).await;
    let day_ago =
        bson::DateTime::from_millis(bson::DateTime::now().timestamp_millis() - 86_400_000);
    set_device(&app, &old, doc! { "last_seen_at": day_ago }).await;
    let nas = stored_device(
        &app,
        &seeded,
        "mach-place-nas",
        &["hive-replica", "hive-archive"],
    )
    .await;
    let silent = stored_device(&app, &seeded, "mach-place-silent", &["exec"]).await;
    let eph = stored_device(&app, &seeded, "mach-place-eph", &["hive-replica"]).await;
    set_device(&app, &eph, doc! { "ephemeral": true }).await;
    let handed = stored_device(&app, &seeded, "mach-place-handed", &["hive-replica"]).await;
    let member_id = ObjectId::parse_str(&seeded.member.id).unwrap();
    set_device(&app, &handed, doc! { "owner_user_id": member_id }).await;
    let removed = stored_device(&app, &seeded, "mach-place-removed", &["hive-replica"]).await;
    set_device(&app, &removed, doc! { "deleted_at": bson::DateTime::now() }).await;

    let mut policy = a_policy(0);
    policy["replicaset"] = json!({
        "min": 3, "max": 4, "archive": true,
        "prefer": ["owner_devices"], "restricted_tags": ["prod"],
    });
    policy["archive_devices"] = json!([nas]);
    let (code, p) = put_policy(&app, &tid, &admin, &policy).await;
    assert_eq!(code, 200, "{p}");

    let s = started(&app, &tid, &admin, &mut primary, "/srv/place").await;
    let r = &s["replicaset"];
    assert_eq!(
        (
            r["policy_revision"].as_i64(),
            r["min"].as_u64(),
            r["max"].as_u64()
        ),
        (Some(1), Some(3), Some(4)),
        "{s}"
    );
    let want = |rows: &[(&str, &str)]| -> Vec<(String, String)> {
        rows.iter()
            .map(|(d, r)| (d.to_string(), r.to_string()))
            .collect()
    };
    assert_eq!(
        members(&s),
        want(&[
            (&primary.agent_id, "primary"),
            (&nas, "archive"),
            (&laptop.agent_id, "owner"),
        ]),
        "{r}"
    );
    assert!(r.get("short").is_none(), "{r}");
    assert_eq!(r["restricted_tags"], json!([]));
    let sid = s["id"].as_str().unwrap().to_string();
    let (_, read) = get_session(&app, &tid, &admin, &sid).await;
    assert_eq!(read["replicaset"], s["replicaset"], "kept on the record");
    for never in [&silent, &eph, &handed, &removed, &old] {
        assert!(!members(&s).iter().any(|(d, _)| d == never), "{never}: {r}");
    }

    // A primary tagged `Prod` is restricted by the policy's `prod`; only the
    // device tagged `PROD` may hold a copy, and the session is short.
    set_device(&app, &primary.agent_id, doc! { "tags": ["Prod"] }).await;
    set_device(&app, &old, doc! { "tags": ["PROD"] }).await;
    let s = started(&app, &tid, &admin, &mut primary, "/srv/prod").await;
    let r = &s["replicaset"];
    assert_eq!(r["restricted_tags"], json!(["prod"]), "{r}");
    assert_eq!(
        members(&s),
        want(&[(&primary.agent_id, "primary"), (&old, "owner")]),
        "{r}"
    );
    assert_eq!(r["short"], "restricted");

    let tid_oid = ObjectId::parse_str(&tid).unwrap();
    let mut rows = app
        .db
        .collection::<Document>("hive_audit")
        .find(doc! { "tenant_id": tid_oid, "action": "place" })
        .await
        .unwrap();
    let mut seen: Vec<(String, String)> = Vec::new();
    while let Some(d) = rows.next().await {
        let d = d.unwrap();
        seen.push((
            d.get_str("outcome").unwrap().to_string(),
            d.get_str("reason").unwrap_or("").to_string(),
        ));
    }
    seen.sort();
    assert_eq!(
        seen,
        [
            ("placed".to_string(), String::new()),
            ("short".to_string(), "restricted".to_string()),
        ]
    );
}

// ─── P2c-3a — the join, and what a member says back ─────────────────────────

/// What a device that holds copies advertises (`hive-replica`).
const HOLDS_COPIES: &[&str] = &["exec", "hive", "hive-replica"];

/// The free plan's three devices are not enough for a replicaset test.
async fn on_pro(app: &TestApp, seeded: &SeededTenant) {
    app.db
        .collection::<Document>("tenants")
        .update_one(
            doc! { "_id": ObjectId::parse_str(&seeded.tenant_id).unwrap() },
            doc! { "$set": { "plan": "pro" } },
        )
        .await
        .unwrap();
}

/// The member of `sid` that is `device`, as the session view reads it.
async fn member_of(app: &TestApp, tid: &str, token: &str, sid: &str, device: &str) -> Value {
    let (code, s) = get_session(app, tid, token, sid).await;
    assert_eq!(code, 200, "{s}");
    s["replicaset"]["members"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["device_id"] == device).cloned())
        .unwrap_or(Value::Null)
}

/// Poll until the member `device` of `sid` satisfies `pred`.
async fn wait_member(
    app: &TestApp,
    tid: &str,
    token: &str,
    sid: &str,
    device: &str,
    what: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let m = member_of(app, tid, token, sid, device).await;
        if pred(&m) {
            return m;
        }
        assert!(Instant::now() < deadline, "{what}: {m}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Whether a frame `want` arrives within `within`.
async fn arrives(ws: &mut AgentWs, want: &str, within: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&text)
                    && v["t"] == want
                {
                    return Some(v);
                }
            }
            Ok(Some(Ok(_))) | Err(_) => continue,
            Ok(Some(Err(_))) | Ok(None) => return None,
        }
    }
    None
}

/// A well-formed hash, every character `fill`.
fn hash(fill: &str) -> String {
    fill.repeat(64 / fill.len())
}

/// A member that holds copies is sent its join once the primary accepts,
/// and the primary's start says the session is replicated. The member's
/// answer and its tips land on its entry; the primary's tips on the
/// primary's. A device the session does not hold as a member changes
/// nothing, and a malformed hash is dropped.
#[tokio::test]
async fn a_member_is_joined_once_the_primary_accepts_and_its_tips_are_kept() {
    let app = replicaset_app().await;
    let seeded = app.seed_tenant("hivejoin").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-join-primary", RUNS_HIVE).await;
    let mut member = device(&app, &seeded, "mach-join-member", HOLDS_COPIES).await;
    let mut stranger = device(&app, &seeded, "mach-join-stranger", RUNS_HIVE).await;
    let other = app.seed_tenant("hivejoinother").await;
    // Someone else's device: never the admin's own, so never placed.
    let mut theirs = device(&app, &other, "mach-join-theirs", HOLDS_COPIES).await;

    let caller = start(&app, &tid, &admin, &primary.agent_id, "/srv/join");
    let target = async {
        let frame = read_until(&mut primary.ws, "rc:hive.start")
            .await
            .expect("the primary receives rc:hive.start");
        assert_eq!(frame["replicated"], true, "a member was placed: {frame}");
        send(
            &mut primary.ws,
            json!({"t": "rc:hive.start_ack", "session_id": frame["session_id"],
                   "fence": frame["fence"], "account": "dev"}),
        )
        .await;
    };
    let (body, ()) = tokio::join!(caller, target);
    assert_eq!(body["outcome"], "accepted", "{body}");
    let sid = body["session"]["id"].as_str().unwrap().to_string();

    let join = read_until(&mut member.ws, "rc:hive.replica.join")
        .await
        .expect("the member receives its join");
    assert_eq!(join["session_id"], sid.as_str());
    assert_eq!(
        (join["fence"].as_u64(), join["role"].as_str()),
        (Some(1), Some("owner"))
    );
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &member.agent_id).await["state"],
        "pending",
        "sent, not yet answered"
    );
    assert!(
        arrives(
            &mut theirs.ws,
            "rc:hive.replica.join",
            Duration::from_millis(300)
        )
        .await
        .is_none(),
        "another organization's device is never joined"
    );

    // A stranger's answer and tip change nothing, nor an answer to another
    // fence.
    send(
        &mut stranger.ws,
        json!({"t": "rc:hive.replica.join_ack", "session_id": sid, "fence": 1}),
    )
    .await;
    send(
        &mut stranger.ws,
        json!({"t": "rc:hive.replica.tip", "session_id": sid, "fence": 1, "seq": 9,
               "hash": hash("ee")}),
    )
    .await;
    send(
        &mut member.ws,
        json!({"t": "rc:hive.replica.join_ack", "session_id": sid, "fence": 7}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &member.agent_id).await["state"],
        "pending",
        "neither the stranger's answer nor one to fence 7 counted"
    );
    send(
        &mut member.ws,
        json!({"t": "rc:hive.replica.join_ack", "session_id": sid, "fence": 1}),
    )
    .await;
    let m = wait_member(&app, &tid, &admin, &sid, &member.agent_id, "joined", |m| {
        m["state"] == "joined"
    })
    .await;
    assert!(m["answered_at"].is_string(), "{m}");
    assert!(m.get("refusal").is_none(), "{m}");

    send(
        &mut member.ws,
        json!({"t": "rc:hive.replica.tip", "session_id": sid, "fence": 1, "seq": 3,
               "hash": hash("ab"), "checkpoint": {"seq": 2, "hash": hash("cd")}}),
    )
    .await;
    let m = wait_member(&app, &tid, &admin, &sid, &member.agent_id, "its tip", |m| {
        m["tip"]["seq"] == 3
    })
    .await;
    assert_eq!(
        (
            m["tip"]["fence"].as_i64(),
            m["tip"]["hash"].as_str(),
            m["tip"]["checkpoint_seq"].as_i64(),
            m["tip"]["checkpoint_hash"].as_str()
        ),
        (
            Some(1),
            Some(hash("ab").as_str()),
            Some(2),
            Some(hash("cd").as_str())
        )
    );
    // Malformed: dropped, and the last good tip stays. Then the primary's
    // own tip, through its manifest, which lands after it.
    send(
        &mut member.ws,
        json!({"t": "rc:hive.replica.tip", "session_id": sid, "fence": 1, "seq": 5,
               "hash": "AB".repeat(32)}),
    )
    .await;
    send(
        &mut primary.ws,
        json!({"t": "rc:hive.replica.manifest", "sessions": [
            {"session_id": sid, "fence": 1, "seq": 4, "hash": hash("01")}
        ]}),
    )
    .await;
    wait_member(
        &app,
        &tid,
        &admin,
        &sid,
        &primary.agent_id,
        "the primary's tip",
        |m| m["tip"]["seq"] == 4,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &member.agent_id).await["tip"]["seq"],
        3,
        "the malformed tip was dropped"
    );
    let s = stored(&app, &sid).await;
    let members = s
        .get_document("replicaset")
        .unwrap()
        .get_array("members")
        .unwrap();
    assert_eq!(members.len(), 2, "nobody else was added: {members:?}");
    let stranger_got_tip = members.iter().any(|m| {
        m.as_document()
            .and_then(|d| d.get_document("tip").ok())
            .is_some_and(|t| t.get_i64("seq").ok() == Some(9))
    });
    assert!(!stranger_got_tip, "the stranger's tip landed nowhere");
}

/// A member offline at the start keeps its join pending; a connection that
/// does not hold copies gets nothing; the one that does gets the join, and
/// its refusal lands with the device's word. A member that refused holds
/// nothing, so its tips are not kept.
#[tokio::test]
async fn a_pending_join_waits_for_a_connection_that_holds_copies() {
    let app = replicaset_app().await;
    let seeded = app.seed_tenant("hivejoinlater").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-later-primary", RUNS_HIVE).await;
    let (member_id, member_token) =
        enroll_agent(&app, &seeded, "mach-later-member", "mach-later-member").await;
    set_device(
        &app,
        &member_id,
        doc! { "capabilities.rpc": ["hive-replica"] },
    )
    .await;

    let s = started(&app, &tid, &admin, &mut primary, "/srv/later").await;
    let sid = s["id"].as_str().unwrap().to_string();
    assert_eq!(
        members(&s),
        [
            (primary.agent_id.clone(), "primary".to_string()),
            (member_id.clone(), "owner".to_string()),
        ]
    );

    // Connected, but not holding copies: nothing is sent.
    let mut ws = connect(&app, &member_token, "mach-later-member", RUNS_HIVE).await;
    wait_agent_online(&app, &seeded, &member_id).await;
    assert!(
        arrives(&mut ws, "rc:hive.replica.join", Duration::from_millis(800))
            .await
            .is_none(),
        "no join to a connection without hive-replica"
    );
    ws.close(None).await.unwrap();
    wait_offline(&app, &seeded, &member_id).await;
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &member_id).await["state"],
        "pending"
    );

    // The connection that holds copies gets it, and refuses.
    let mut ws = connect(&app, &member_token, "mach-later-member", HOLDS_COPIES).await;
    let join = read_until(&mut ws, "rc:hive.replica.join")
        .await
        .expect("the join, on the connection that holds copies");
    assert_eq!(join["session_id"], sid.as_str());
    send(
        &mut ws,
        json!({"t": "rc:hive.replica.join_ack", "session_id": sid, "fence": 1,
               "refused": "replica_disabled", "detail": "hive_replica is off"}),
    )
    .await;
    let m = wait_member(&app, &tid, &admin, &sid, &member_id, "refused", |m| {
        m["state"] == "refused"
    })
    .await;
    assert_eq!(m["refusal"], "replica_disabled");
    send(
        &mut ws,
        json!({"t": "rc:hive.replica.tip", "session_id": sid, "fence": 1, "seq": 2,
               "hash": hash("ab")}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        member_of(&app, &tid, &admin, &sid, &member_id)
            .await
            .get("tip")
            .is_none(),
        "a member that refused holds nothing"
    );
}

// ─── P2c-4a — placement decided again ───────────────────────────────────────

/// Set a device's tags through fleet's own route, as an admin does: the
/// change that makes placement decide again (core's `agent_updated`).
async fn tag(app: &TestApp, tid: &str, token: &str, device: &str, tags: &[&str]) {
    let resp = app
        .auth_put(&format!("/api/tenant/{tid}/agent/{device}"), token)
        .json(&json!({ "tags": tags }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert_eq!(
        status,
        200,
        "tags: {}",
        resp.text().await.unwrap_or_default()
    );
}

/// A policy with decision 2's rules but `min`, written from `revision`.
fn policy_min(min: u32, revision: i64) -> Value {
    json!({
        "replicaset": {
            "min": min, "max": 4, "archive": true,
            "prefer": ["owner_devices"], "restricted_tags": ["prod"],
        },
        "retention_days": 90,
        "archive_devices": [],
        "revision": revision,
    })
}

/// A device's tags change, and placement decides again. An unrelated change
/// moves nothing: a member that still qualifies is kept. A tag taken off
/// makes the member leave, and a device that now qualifies takes its place
/// and is joined; tagged again, the one that left stays leaving while its
/// replacement still qualifies.
#[tokio::test]
async fn placement_is_decided_again_when_a_devices_tags_change() {
    let app = replicaset_app().await;
    let seeded = app.seed_tenant("hivereplace").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-re-primary", RUNS_HIVE).await;
    set_device(&app, &primary.agent_id, doc! { "tags": ["prod"] }).await;
    let mut a = device(&app, &seeded, "mach-re-a", HOLDS_COPIES).await;
    set_device(&app, &a.agent_id, doc! { "tags": ["prod"] }).await;
    let mut b = device(&app, &seeded, "mach-re-b", HOLDS_COPIES).await;

    let s = started(&app, &tid, &admin, &mut primary, "/srv/re").await;
    let sid = s["id"].as_str().unwrap().to_string();
    assert_eq!(
        members(&s),
        [
            (primary.agent_id.clone(), "primary".to_string()),
            (a.agent_id.clone(), "owner".to_string()),
        ],
        "B lacks the restricted tag"
    );
    read_until(&mut a.ws, "rc:hive.replica.join")
        .await
        .expect("A's join");
    send(
        &mut a.ws,
        json!({"t": "rc:hive.replica.join_ack", "session_id": sid, "fence": 1}),
    )
    .await;
    wait_member(&app, &tid, &admin, &sid, &a.agent_id, "A joined", |m| {
        m["state"] == "joined"
    })
    .await;

    // An unrelated change: B now qualifies too, and nothing moves.
    tag(&app, &tid, &admin, &b.agent_id, &["prod"]).await;
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &a.agent_id).await["state"],
        "joined"
    );
    assert!(
        member_of(&app, &tid, &admin, &sid, &b.agent_id)
            .await
            .is_null(),
        "min holds: B is not taken"
    );
    assert!(
        arrives(
            &mut b.ws,
            "rc:hive.replica.join",
            Duration::from_millis(300)
        )
        .await
        .is_none()
    );

    // A's tag taken off: A leaves, B takes its place and is joined.
    tag(&app, &tid, &admin, &a.agent_id, &[]).await;
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &a.agent_id).await["state"],
        "leaving"
    );
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &b.agent_id).await["state"],
        "pending"
    );
    let join = read_until(&mut b.ws, "rc:hive.replica.join")
        .await
        .expect("B's join");
    assert_eq!(join["session_id"], sid.as_str());
    // A's join answer, late, does not take it back.
    send(
        &mut a.ws,
        json!({"t": "rc:hive.replica.join_ack", "session_id": sid, "fence": 1}),
    )
    .await;

    // A tagged again: B still qualifies and is kept; A stays leaving.
    tag(&app, &tid, &admin, &a.agent_id, &["prod"]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &a.agent_id).await["state"],
        "leaving"
    );
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &b.agent_id).await["state"],
        "pending"
    );
    let (_, read) = get_session(&app, &tid, &admin, &sid).await;
    assert_eq!(read["replicaset"]["restricted_tags"], json!(["prod"]));

    let tid_oid = ObjectId::parse_str(&tid).unwrap();
    let replaced = app
        .db
        .collection::<Document>("hive_audit")
        .count_documents(doc! { "tenant_id": tid_oid, "action": "place", "outcome": "replaced" })
        .await
        .unwrap();
    assert_eq!(replaced, 1, "one change moved anyone: A out, B in");
}

/// A policy that asks for more copies places them on the live sessions it
/// governs, and joins them.
#[tokio::test]
async fn a_policy_that_asks_for_more_copies_places_them() {
    let app = replicaset_app().await;
    let seeded = app.seed_tenant("hivereplacepolicy").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let (code, p) = put_policy(&app, &tid, &admin, &policy_min(1, 0)).await;
    assert_eq!(code, 200, "{p}");
    let mut primary = device(&app, &seeded, "mach-rp-primary", RUNS_HIVE).await;
    let mut b = device(&app, &seeded, "mach-rp-b", HOLDS_COPIES).await;
    let s = started(&app, &tid, &admin, &mut primary, "/srv/rp").await;
    let sid = s["id"].as_str().unwrap().to_string();
    assert_eq!(members(&s).len(), 1, "min 1: the primary alone");

    let (code, p) = put_policy(&app, &tid, &admin, &policy_min(2, 1)).await;
    assert_eq!(code, 200, "{p}");
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &b.agent_id).await["state"],
        "pending"
    );
    read_until(&mut b.ws, "rc:hive.replica.join")
        .await
        .expect("B's join, from the policy change");
    let (_, read) = get_session(&app, &tid, &admin, &sid).await;
    assert_eq!(
        (
            read["replicaset"]["min"].as_u64(),
            read["replicaset"]["policy_revision"].as_i64()
        ),
        (Some(2), Some(2))
    );
}

/// A member removed from the fleet leaves the sessions it held copies of, and
/// another device that qualifies takes its place.
#[tokio::test]
async fn a_removed_member_leaves_and_another_takes_its_place() {
    let app = replicaset_app().await;
    let seeded = app.seed_tenant("hivereplacegone").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-rg-primary", RUNS_HIVE).await;
    let mut b = device(&app, &seeded, "mach-rg-b", HOLDS_COPIES).await;
    let day_ago =
        bson::DateTime::from_millis(bson::DateTime::now().timestamp_millis() - 86_400_000);
    set_device(&app, &b.agent_id, doc! { "last_seen_at": day_ago }).await;
    let a = device(&app, &seeded, "mach-rg-a", HOLDS_COPIES).await;
    let s = started(&app, &tid, &admin, &mut primary, "/srv/rg").await;
    let sid = s["id"].as_str().unwrap().to_string();
    assert_eq!(
        members(&s)[1],
        (a.agent_id.clone(), "owner".to_string()),
        "A, seen last"
    );

    let resp = app
        .auth_delete(&format!("/api/tenant/{tid}/agent/{}", a.agent_id), &admin)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "remove: {}", resp.status());
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &a.agent_id).await["state"],
        "leaving"
    );
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &b.agent_id).await["state"],
        "pending"
    );
    read_until(&mut b.ws, "rc:hive.replica.join")
        .await
        .expect("B's join, in A's place");
}

// ─── P2c-4b — the primary told, and sessions from before placement ─────────

/// A session that started alone gains its first member: its primary is owed
/// its join, `role: primary`, and checkpoints once it answers. A primary that
/// refused (a build that cannot name the role) is asked again at its next
/// connection; one that answered `joined` is owed nothing more, and an answer
/// it was not owed changes nothing.
#[tokio::test]
async fn a_session_that_gains_its_first_member_tells_its_primary() {
    let app = replicaset_app().await;
    let seeded = app.seed_tenant("hiveprimaryjoin").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let (code, p) = put_policy(&app, &tid, &admin, &policy_min(1, 0)).await;
    assert_eq!(code, 200, "{p}");
    let mut primary = device(&app, &seeded, "mach-pj-primary", RUNS_HIVE).await;
    let mut b = device(&app, &seeded, "mach-pj-b", HOLDS_COPIES).await;
    let s = started(&app, &tid, &admin, &mut primary, "/srv/pj").await;
    let sid = s["id"].as_str().unwrap().to_string();
    assert_eq!(members(&s).len(), 1, "min 1: the primary alone");
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &primary.agent_id).await["state"],
        "joined",
        "its start told it all there was"
    );

    let (code, p) = put_policy(&app, &tid, &admin, &policy_min(2, 1)).await;
    assert_eq!(code, 200, "{p}");
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &primary.agent_id).await["state"],
        "pending",
        "owed its join"
    );
    let join = read_until(&mut primary.ws, "rc:hive.replica.join")
        .await
        .expect("the primary's join");
    assert_eq!(
        (
            join["session_id"].as_str(),
            join["fence"].as_u64(),
            join["role"].as_str()
        ),
        (Some(sid.as_str()), Some(1), Some("primary"))
    );
    read_until(&mut b.ws, "rc:hive.replica.join")
        .await
        .expect("B's join");

    // A build that cannot name the role refuses it, in its own word.
    send(
        &mut primary.ws,
        json!({"t": "rc:hive.replica.join_ack", "session_id": sid, "fence": 1,
               "refused": "other", "detail": "a role this build cannot name"}),
    )
    .await;
    let m = wait_member(
        &app,
        &tid,
        &admin,
        &sid,
        &primary.agent_id,
        "refused",
        |m| m["state"] == "refused",
    )
    .await;
    assert_eq!(m["refusal"], "other");

    // Asked again at its next connection, and this time it takes it.
    primary.ws.close(None).await.unwrap();
    wait_offline(&app, &seeded, &primary.agent_id).await;
    let mut ws = connect(&app, &primary.token, &primary.machine, RUNS_HIVE).await;
    let again = read_until(&mut ws, "rc:hive.replica.join")
        .await
        .expect("asked again at its next connection");
    assert_eq!(again["role"], "primary");
    send(
        &mut ws,
        json!({"t": "rc:hive.replica.join_ack", "session_id": sid, "fence": 1}),
    )
    .await;
    wait_member(&app, &tid, &admin, &sid, &primary.agent_id, "joined", |m| {
        m["state"] == "joined"
    })
    .await;

    // An answer it was not owed changes nothing; nor is it asked again.
    send(
        &mut ws,
        json!({"t": "rc:hive.replica.join_ack", "session_id": sid, "fence": 1,
               "refused": "other"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &primary.agent_id).await["state"],
        "joined"
    );
    ws.close(None).await.unwrap();
    wait_offline(&app, &seeded, &primary.agent_id).await;
    let mut ws = connect(&app, &primary.token, &primary.machine, RUNS_HIVE).await;
    assert!(
        arrives(&mut ws, "rc:hive.replica.join", Duration::from_millis(800))
            .await
            .is_none(),
        "joined: owed nothing"
    );
}

/// A session placed nowhere (one from before P2, or started while the switch
/// was off) is placed the first time its primary connects with the switch
/// on, as at a start: its primary, which runs it without `replicated`, is
/// owed its join, and its member is sent its own. A second connection places
/// nothing more.
#[tokio::test]
async fn a_session_from_before_placement_is_placed_when_its_primary_connects() {
    let app = replicaset_app().await;
    let seeded = app.seed_tenant("hiveunplaced").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-up-primary", RUNS_HIVE).await;
    let mut b = device(&app, &seeded, "mach-up-b", HOLDS_COPIES).await;
    let s = started(&app, &tid, &admin, &mut primary, "/srv/up").await;
    let sid = s["id"].as_str().unwrap().to_string();
    read_until(&mut b.ws, "rc:hive.replica.join")
        .await
        .expect("B's join at the start");
    // As a record from before P2 has it: no replicaset at all.
    let oid = ObjectId::parse_str(&sid).unwrap();
    app.db
        .collection::<Document>("agent_sessions")
        .update_one(doc! { "_id": oid }, doc! { "$unset": { "replicaset": "" } })
        .await
        .unwrap();
    let places = || async {
        app.db
            .collection::<Document>("hive_audit")
            .count_documents(doc! { "session_id": oid, "action": "place" })
            .await
            .unwrap()
    };
    assert_eq!(places().await, 1, "the start's");

    primary.ws.close(None).await.unwrap();
    wait_offline(&app, &seeded, &primary.agent_id).await;
    let mut ws = connect(&app, &primary.token, &primary.machine, RUNS_HIVE).await;
    let join = read_until(&mut ws, "rc:hive.replica.join")
        .await
        .expect("the primary's join");
    assert_eq!(
        (join["session_id"].as_str(), join["role"].as_str()),
        (Some(sid.as_str()), Some("primary"))
    );
    let bj = read_until(&mut b.ws, "rc:hive.replica.join")
        .await
        .expect("B's join, placed at the primary's connection");
    assert_eq!(bj["session_id"], sid.as_str());
    let (_, read) = get_session(&app, &tid, &admin, &sid).await;
    assert_eq!(
        members(&read),
        [
            (primary.agent_id.clone(), "primary".to_string()),
            (b.agent_id.clone(), "owner".to_string()),
        ]
    );
    assert_eq!(
        member_of(&app, &tid, &admin, &sid, &primary.agent_id).await["state"],
        "pending"
    );
    assert_eq!(places().await, 2, "placed once more, by the connection");

    ws.close(None).await.unwrap();
    wait_offline(&app, &seeded, &primary.agent_id).await;
    let _ws = connect(&app, &primary.token, &primary.machine, RUNS_HIVE).await;
    wait_agent_online(&app, &seeded, &primary.agent_id).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(places().await, 2, "placed already: nothing more");
}

/// `first`'s database, served by a second server with the replicaset's
/// switch off: as `first` restarted with `hive.replicaset = false`.
async fn switch_off(first: &TestApp) -> TestApp {
    let db = first.db.name().to_string();
    TestApp::spawn_with_settings(move |s| {
        s.database.name = db;
        s.modules.hive = true;
        s.hive.replicaset = false;
        // Polls, like `serving_only`: the default limiter answers a poll
        // loop 429 before the condition it waits for.
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await
}

/// Every frame `ws` receives within `within`.
async fn frames_within(ws: &mut AgentWs, within: Duration) -> Vec<Value> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    out.push(v);
                }
            }
            Ok(Some(Ok(_))) | Err(_) => continue,
            Ok(Some(Err(_))) | Ok(None) => break,
        }
    }
    out
}

/// The replicaset's switch off is a session on its primary alone, as in P1,
/// for sessions placed while it was on as well: no join goes to a member or
/// to a primary owed one, at a connection or at an acceptance, and a start
/// sent again does not say `replicated`.
#[tokio::test]
async fn with_the_switch_off_no_replica_frame_is_sent() {
    let on = replicaset_app().await;
    let seeded = on.seed_tenant("hiveswitchoff").await;
    on_pro(&on, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let (code, p) = put_policy(&on, &tid, &admin, &policy_min(1, 0)).await;
    assert_eq!(code, 200, "{p}");
    let mut primary = device(&on, &seeded, "mach-so-primary", RUNS_HIVE).await;
    let (b_id, b_token) = enroll_agent(&on, &seeded, "mach-so-b", "mach-so-b").await;
    set_device(&on, &b_id, doc! { "capabilities.rpc": ["hive-replica"] }).await;

    // X starts alone, then gains B: its primary is owed its join.
    let x = started(&on, &tid, &admin, &mut primary, "/srv/so-x").await;
    let x = x["id"].as_str().unwrap().to_string();
    let (code, p) = put_policy(&on, &tid, &admin, &policy_min(2, 1)).await;
    assert_eq!(code, 200, "{p}");
    read_until(&mut primary.ws, "rc:hive.replica.join")
        .await
        .expect("X's primary join, with the switch on");
    assert_eq!(
        member_of(&on, &tid, &admin, &x, &primary.agent_id).await["state"],
        "pending"
    );
    // Y starts with B as a member, and its device does not answer yet.
    let caller = start(&on, &tid, &admin, &primary.agent_id, "/srv/so-y");
    let target = async {
        read_until(&mut primary.ws, "rc:hive.start")
            .await
            .expect("Y's start")
    };
    let (body, first) = tokio::join!(caller, target);
    assert_eq!(body["outcome"], "pending", "{body}");
    assert_eq!(first["replicated"], true, "placed with B, the switch on");
    let y = body["session"]["id"].as_str().unwrap().to_string();

    // The switch off: the same database, served again.
    let off = switch_off(&on).await;
    primary.ws.close(None).await.unwrap();
    wait_offline(&on, &seeded, &primary.agent_id).await;
    let mut ws = connect(&off, &primary.token, &primary.machine, RUNS_HIVE).await;
    let frames = frames_within(&mut ws, Duration::from_millis(1500)).await;
    let again = frames
        .iter()
        .find(|f| f["t"] == "rc:hive.start" && f["session_id"] == y.as_str())
        .expect("Y's start, sent again");
    assert!(again.get("replicated").is_none(), "not replicated: {again}");
    assert!(
        !frames.iter().any(|f| f["t"] == "rc:hive.replica.join"),
        "no primary join: {frames:?}"
    );

    let mut b = connect(&off, &b_token, "mach-so-b", HOLDS_COPIES).await;
    wait_agent_online(&off, &seeded, &b_id).await;
    send(
        &mut ws,
        json!({"t": "rc:hive.start_ack", "session_id": y, "fence": 1, "account": "dev"}),
    )
    .await;
    assert!(
        arrives(&mut b, "rc:hive.replica.join", Duration::from_millis(1200))
            .await
            .is_none(),
        "no member join, at its connection or at Y's acceptance"
    );
}

// ─── P2d-1 — the replica stream's grants ───────────────────────────────────

/// What a source advertises: it runs sessions and carries streams.
const SERVES_STREAMS: &[&str] = &["exec", "hive", "hive-stream"];
/// What a member that follows advertises: it holds copies and carries streams.
const FOLLOWS: &[&str] = &["exec", "hive", "hive-replica", "hive-stream"];

/// The replicaset on, a TURN server configured so the `relay` rung can be
/// granted, and a limiter a polling test cannot trip.
async fn stream_app() -> TestApp {
    TestApp::spawn_with_settings(|s| {
        s.modules.hive = true;
        s.hive.replicaset = true;
        s.turn.url = Some("turn:turn.example.com:3478".into());
        s.turn.shared_secret = Some("stream-test-secret".into());
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await
}

/// A session on `primary`, with `member` joined: it answered its join.
async fn followed(
    app: &TestApp,
    tid: &str,
    token: &str,
    primary: &mut Device,
    member: &mut Device,
    folder: &str,
) -> String {
    let s = started(app, tid, token, primary, folder).await;
    let sid = s["id"].as_str().unwrap().to_string();
    read_until(&mut member.ws, "rc:hive.replica.join")
        .await
        .expect("the member's join");
    send(
        &mut member.ws,
        json!({"t": "rc:hive.replica.join_ack", "session_id": sid, "fence": 1}),
    )
    .await;
    wait_member(app, tid, token, &sid, &member.agent_id, "joined", |m| {
        m["state"] == "joined"
    })
    .await;
    sid
}

/// A member's want, from an empty copy, on `path`.
fn want(sid: &str, path: &str) -> Value {
    json!({
        "t": "rc:hive.replica.want", "session_id": sid, "fence": 1,
        "seq": 0, "hash": "0".repeat(64), "path": path,
    })
}

/// The member's refusal, and why. `waited` names what the caller waited
/// for, in the panic when none came: every call shares the panic's line.
async fn refused_why(ws: &mut AgentWs, waited: &str) -> Value {
    read_until(ws, "rc:hive.replica.refused")
        .await
        .unwrap_or_else(|| panic!("no refusal: {waited}"))
}

/// The member is told to dial only once the source said it is ready, with
/// the token the source was granted and the certificate it reported. A
/// candidate on the direct rung goes nowhere; a close reaches the other side.
#[tokio::test]
async fn a_member_is_told_to_dial_only_once_the_source_is_ready() {
    let app = stream_app().await;
    let seeded = app.seed_tenant("hivestream").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-st-primary", SERVES_STREAMS).await;
    let mut member = device(&app, &seeded, "mach-st-member", FOLLOWS).await;
    let sid = followed(&app, &tid, &admin, &mut primary, &mut member, "/srv/st").await;

    send(&mut member.ws, want(&sid, "direct")).await;
    let grant = read_until(&mut primary.ws, "rc:hive.replica.grant")
        .await
        .expect("the source's grant");
    assert_eq!(grant["session_id"], sid.as_str());
    assert_eq!(grant["member_id"], member.agent_id.as_str());
    assert_eq!(
        (grant["path"].as_str(), grant["ttl_secs"].as_u64()),
        (Some("direct"), Some(600))
    );
    let token = grant["token"].as_str().unwrap().to_string();
    assert!(token.len() >= 32, "unguessable: {token}");
    assert!(
        grant["ice_servers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s.get("username").is_none()),
        "STUN only on the direct rung: {grant}"
    );
    let gid = grant["grant_id"].as_str().unwrap().to_string();
    // The member cannot answer for its source.
    send(
        &mut member.ws,
        json!({"t": "rc:hive.replica.ready", "grant_id": gid, "cert_fingerprint": hash("ee"),
               "addrs": ["192.0.2.66:1"]}),
    )
    .await;
    assert!(
        arrives(
            &mut member.ws,
            "rc:hive.replica.dial",
            Duration::from_millis(300)
        )
        .await
        .is_none(),
        "no dial before the source is ready, nor on the member's own word"
    );

    let fp = hash("ab");
    send(
        &mut primary.ws,
        json!({"t": "rc:hive.replica.ready", "grant_id": gid, "cert_fingerprint": fp,
               "addrs": ["192.0.2.7:41641"]}),
    )
    .await;
    let dial = read_until(&mut member.ws, "rc:hive.replica.dial")
        .await
        .expect("the member's dial");
    assert_eq!(
        (
            dial["grant_id"].as_str(),
            dial["source_id"].as_str(),
            dial["token"].as_str(),
            dial["cert_fingerprint"].as_str(),
        ),
        (
            Some(gid.as_str()),
            Some(primary.agent_id.as_str()),
            Some(token.as_str()),
            Some(fp.as_str()),
        )
    );
    assert_eq!(dial["addrs"], json!(["192.0.2.7:41641"]));

    send(
        &mut member.ws,
        json!({"t": "rc:hive.replica.candidate", "grant_id": gid, "addr": "198.51.100.4:3478"}),
    )
    .await;
    assert!(
        arrives(
            &mut primary.ws,
            "rc:hive.replica.candidate",
            Duration::from_millis(300)
        )
        .await
        .is_none(),
        "no candidate on the direct rung"
    );
    send(
        &mut member.ws,
        json!({"t": "rc:hive.replica.close", "grant_id": gid, "reason": "caught_up"}),
    )
    .await;
    let close = read_until(&mut primary.ws, "rc:hive.replica.close")
        .await
        .expect("the source is told");
    assert_eq!(
        (close["grant_id"].as_str(), close["reason"].as_str()),
        (Some(gid.as_str()), Some("caught_up"))
    );
    let tid_oid = ObjectId::parse_str(&tid).unwrap();
    let rows = app
        .db
        .collection::<Document>("hive_audit")
        .count_documents(doc! { "tenant_id": tid_oid, "action": "stream" })
        .await
        .unwrap();
    assert_eq!(rows, 2, "sent, then ready");
}

/// The `relay` rung carries TURN credentials to both sides, minted for one
/// worker, and relays the member's address to the source; the `derp` rung
/// carries both devices' keys, and a ready without the source's is no ready.
#[tokio::test]
async fn the_relay_and_derp_rungs_carry_what_they_need() {
    let app = stream_app().await;
    let seeded = app.seed_tenant("hivestreamrungs").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-sr-primary", SERVES_STREAMS).await;
    let mut member = device(&app, &seeded, "mach-sr-member", FOLLOWS).await;
    let sid = followed(&app, &tid, &admin, &mut primary, &mut member, "/srv/sr").await;

    let turn = |ice: &Value| {
        ice.as_array()
            .unwrap()
            .iter()
            .any(|s| s["username"].is_string() && s["credential"].is_string())
    };
    send(&mut member.ws, want(&sid, "relay")).await;
    let grant = read_until(&mut primary.ws, "rc:hive.replica.grant")
        .await
        .expect("a relay grant");
    assert!(turn(&grant["ice_servers"]), "the source's TURN credentials");
    let gid = grant["grant_id"].as_str().unwrap().to_string();
    send(
        &mut primary.ws,
        json!({"t": "rc:hive.replica.ready", "grant_id": gid, "cert_fingerprint": hash("cd"),
               "addrs": ["203.0.113.9:49152"]}),
    )
    .await;
    let dial = read_until(&mut member.ws, "rc:hive.replica.dial")
        .await
        .expect("a relay dial");
    assert!(turn(&dial["ice_servers"]), "the member's TURN credentials");
    send(
        &mut member.ws,
        json!({"t": "rc:hive.replica.candidate", "grant_id": gid, "addr": "198.51.100.4:3478"}),
    )
    .await;
    let c = read_until(&mut primary.ws, "rc:hive.replica.candidate")
        .await
        .expect("the member's relay address, for the source to permit");
    assert_eq!(c["addr"], "198.51.100.4:3478");
    // Not an address, or not the member's grant: nothing.
    send(
        &mut member.ws,
        json!({"t": "rc:hive.replica.candidate", "grant_id": gid, "addr": "relay.example:3478"}),
    )
    .await;
    send(
        &mut primary.ws,
        json!({"t": "rc:hive.replica.candidate", "grant_id": gid, "addr": "198.51.100.5:3478"}),
    )
    .await;
    assert!(
        arrives(
            &mut primary.ws,
            "rc:hive.replica.candidate",
            Duration::from_millis(300)
        )
        .await
        .is_none()
    );

    let member_key = "1f".repeat(32);
    let mut w = want(&sid, "derp");
    w["derp_pubkey"] = json!(member_key);
    send(&mut member.ws, w.clone()).await;
    let grant = read_until(&mut primary.ws, "rc:hive.replica.grant")
        .await
        .expect("a derp grant");
    assert_eq!(grant["member_derp_pubkey"], member_key.as_str());
    let gid = grant["grant_id"].as_str().unwrap().to_string();
    send(
        &mut primary.ws,
        json!({"t": "rc:hive.replica.ready", "grant_id": gid, "cert_fingerprint": hash("ef")}),
    )
    .await;
    let no = refused_why(&mut member.ws, "a derp ready without the source's key").await;
    assert_eq!(
        no["reason"], "other",
        "a derp ready without the source's key: {no}"
    );
    let close = read_until(&mut primary.ws, "rc:hive.replica.close")
        .await
        .expect("the source's endpoint is told to go");
    assert_eq!(close["reason"], "malformed");

    send(&mut member.ws, w).await;
    let grant = read_until(&mut primary.ws, "rc:hive.replica.grant")
        .await
        .expect("a second derp grant");
    let gid = grant["grant_id"].as_str().unwrap().to_string();
    let source_key = "2e".repeat(32);
    send(
        &mut primary.ws,
        json!({"t": "rc:hive.replica.ready", "grant_id": gid, "cert_fingerprint": hash("ef"),
               "derp_pubkey": source_key}),
    )
    .await;
    let dial = read_until(&mut member.ws, "rc:hive.replica.dial")
        .await
        .expect("a derp dial");
    assert_eq!(dial["derp_pubkey"], source_key.as_str());
    assert_eq!(dial["path"], "derp");
}

/// FR-90 P2d-2b — an address no device could be reached at is never
/// relayed: a `ready` that offers one is no ready (the member is refused
/// `other`, the source's endpoint is told to go), and such a candidate is
/// dropped. The device checks again before it dials or permits: this is the
/// server's courtesy, the device's the gate.
#[tokio::test]
async fn an_address_no_device_could_be_reached_at_is_never_relayed() {
    let app = stream_app().await;
    let seeded = app.seed_tenant("hivestreamaddrs").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-sa-primary", SERVES_STREAMS).await;
    let mut member = device(&app, &seeded, "mach-sa-member", FOLLOWS).await;
    let sid = followed(&app, &tid, &admin, &mut primary, &mut member, "/srv/sa").await;

    for nowhere in [
        "224.0.0.251:5353",
        "255.255.255.255:41641",
        "0.0.0.0:41641",
        "192.0.2.7:0",
    ] {
        send(&mut member.ws, want(&sid, "direct")).await;
        let grant = read_until(&mut primary.ws, "rc:hive.replica.grant")
            .await
            .expect("a grant");
        let gid = grant["grant_id"].as_str().unwrap().to_string();
        send(
            &mut primary.ws,
            json!({"t": "rc:hive.replica.ready", "grant_id": gid, "cert_fingerprint": hash("ab"),
                   "addrs": [nowhere]}),
        )
        .await;
        let no = refused_why(&mut member.ws, &format!("a ready offering {nowhere}")).await;
        assert_eq!(no["reason"], "other", "{nowhere}: {no}");
        let close = read_until(&mut primary.ws, "rc:hive.replica.close")
            .await
            .expect("the source's endpoint is told to go");
        assert_eq!(close["reason"], "malformed", "{nowhere}");
    }

    send(&mut member.ws, want(&sid, "relay")).await;
    let grant = read_until(&mut primary.ws, "rc:hive.replica.grant")
        .await
        .expect("a relay grant");
    let gid = grant["grant_id"].as_str().unwrap().to_string();
    send(
        &mut primary.ws,
        json!({"t": "rc:hive.replica.ready", "grant_id": gid, "cert_fingerprint": hash("cd"),
               "addrs": ["203.0.113.9:49152"]}),
    )
    .await;
    read_until(&mut member.ws, "rc:hive.replica.dial")
        .await
        .expect("a relay dial");
    send(
        &mut member.ws,
        json!({"t": "rc:hive.replica.candidate", "grant_id": gid, "addr": "224.0.0.251:5353"}),
    )
    .await;
    assert!(
        arrives(
            &mut primary.ws,
            "rc:hive.replica.candidate",
            Duration::from_millis(300)
        )
        .await
        .is_none(),
        "a multicast candidate is dropped"
    );
}

/// Each of the server's gates refuses in its own word: not a member, another
/// fence, a rung that cannot be had, a source away or of an older build, and
/// the rate. A connection that does not carry streams is not heard at all.
#[tokio::test]
async fn a_want_is_refused_by_the_gate_it_failed() {
    let app = replicaset_app().await;
    let seeded = app.seed_tenant("hivestreamgates").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-sg-primary", SERVES_STREAMS).await;
    let mut member = device(&app, &seeded, "mach-sg-member", FOLLOWS).await;
    let sid = followed(&app, &tid, &admin, &mut primary, &mut member, "/srv/sg").await;
    // Enrolled after the session was placed: no member of it.
    let mut outsider = device(&app, &seeded, "mach-sg-outsider", FOLLOWS).await;

    send(&mut outsider.ws, want(&sid, "direct")).await;
    assert_eq!(
        refused_why(&mut outsider.ws, "the outsider's not_a_member").await["reason"],
        "not_a_member"
    );
    let mut stale = want(&sid, "direct");
    stale["fence"] = json!(2);
    send(&mut member.ws, stale).await;
    assert_eq!(
        refused_why(&mut member.ws, "the stale fence").await["reason"],
        "stale_fence"
    );
    for (path, why) in [
        ("smoke_signals", "a rung this build cannot name"),
        ("derp", "derp without the member's key"),
        ("relay", "relay with no TURN server configured"),
    ] {
        send(&mut member.ws, want(&sid, path)).await;
        let no = refused_why(&mut member.ws, why).await;
        assert_eq!(no["reason"], "path_unavailable", "{why}: {no}");
    }
    assert!(
        arrives(
            &mut primary.ws,
            "rc:hive.replica.grant",
            Duration::from_millis(300)
        )
        .await
        .is_none(),
        "no gate let a grant through"
    );

    // A connection that does not carry streams is not heard, though the
    // source is right there to be granted to.
    member.ws.close(None).await.unwrap();
    wait_offline(&app, &seeded, &member.agent_id).await;
    let mut no_streams = connect(&app, &member.token, &member.machine, HOLDS_COPIES).await;
    wait_agent_online(&app, &seeded, &member.agent_id).await;
    send(&mut no_streams, want(&sid, "direct")).await;
    assert!(
        arrives(
            &mut primary.ws,
            "rc:hive.replica.grant",
            Duration::from_millis(600)
        )
        .await
        .is_none(),
        "no grant from a connection without hive-stream"
    );
    drop(no_streams);
    wait_offline(&app, &seeded, &member.agent_id).await;
    let mut follower = connect(&app, &member.token, &member.machine, FOLLOWS).await;
    wait_agent_online(&app, &seeded, &member.agent_id).await;

    // A primary of an older build carries no stream; one away is offline.
    primary.ws.close(None).await.unwrap();
    wait_offline(&app, &seeded, &primary.agent_id).await;
    let older = connect(&app, &primary.token, &primary.machine, RUNS_HIVE).await;
    wait_agent_online(&app, &seeded, &primary.agent_id).await;
    send(&mut follower, want(&sid, "direct")).await;
    assert_eq!(
        refused_why(&mut follower, "an older source").await["reason"],
        "source_unsupported"
    );
    drop(older);
    wait_offline(&app, &seeded, &primary.agent_id).await;
    send(&mut follower, want(&sid, "direct")).await;
    assert_eq!(
        refused_why(&mut follower, "a source away").await["reason"],
        "source_offline"
    );

    // The rate, after the identity gates: two wants passed it above.
    let mut reasons = Vec::new();
    for _ in 0..12 {
        send(&mut follower, want(&sid, "direct")).await;
        reasons.push(refused_why(&mut follower, "the rate").await["reason"].clone());
    }
    assert_eq!(reasons.last().unwrap(), "rate_limited", "{reasons:?}");
    assert_eq!(
        reasons.iter().filter(|r| *r == "source_offline").count(),
        10,
        "twelve a minute: {reasons:?}"
    );
}

/// A source's refusal reaches the member in the source's word, and a source
/// that never answers is the member's `no_answer`, its grant dropped.
#[tokio::test]
async fn the_sources_refusal_or_silence_reaches_the_member() {
    let app = stream_app().await;
    let seeded = app.seed_tenant("hivestreamsource").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-ss-primary", SERVES_STREAMS).await;
    let mut member = device(&app, &seeded, "mach-ss-member", FOLLOWS).await;
    let sid = followed(&app, &tid, &admin, &mut primary, &mut member, "/srv/ss").await;

    send(&mut member.ws, want(&sid, "direct")).await;
    let grant = read_until(&mut primary.ws, "rc:hive.replica.grant")
        .await
        .expect("a grant");
    send(
        &mut primary.ws,
        json!({"t": "rc:hive.replica.ready", "grant_id": grant["grant_id"], "refused": "not_held",
               "detail": "this device lost its store"}),
    )
    .await;
    let no = refused_why(&mut member.ws, "the source's not_held").await;
    assert_eq!(
        (no["reason"].as_str(), no["detail"].as_str()),
        (Some("not_held"), Some("this device lost its store"))
    );

    send(&mut member.ws, want(&sid, "direct")).await;
    let grant = read_until(&mut primary.ws, "rc:hive.replica.grant")
        .await
        .expect("a second grant");
    let gid = grant["grant_id"].as_str().unwrap().to_string();
    let silent = Duration::from_secs(
        roomler_ai_remote_control::hive::stream_limits::GRANT_ACK_TIMEOUT_SECS + 5,
    );
    let no = arrives(&mut member.ws, "rc:hive.replica.refused", silent)
        .await
        .expect("the member hears no_answer");
    assert_eq!(no["reason"], "no_answer");
    let close = read_until(&mut primary.ws, "rc:hive.replica.close")
        .await
        .expect("the source is told to drop it");
    assert_eq!(
        (close["grant_id"].as_str(), close["reason"].as_str()),
        (Some(gid.as_str()), Some("no_answer"))
    );
    // Its answer after all changes nothing.
    send(
        &mut primary.ws,
        json!({"t": "rc:hive.replica.ready", "grant_id": gid, "cert_fingerprint": hash("ab"),
               "addrs": ["192.0.2.7:41641"]}),
    )
    .await;
    assert!(
        arrives(
            &mut member.ws,
            "rc:hive.replica.dial",
            Duration::from_millis(400)
        )
        .await
        .is_none()
    );
}

/// A renewal comes up beside the grant the member follows
/// (make-before-break); a third want replaces the oldest, and both sides are
/// told.
#[tokio::test]
async fn a_renewal_keeps_the_grant_it_follows_and_a_third_replaces_the_oldest() {
    let app = stream_app().await;
    let seeded = app.seed_tenant("hivestreamrenew").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-sn-primary", SERVES_STREAMS).await;
    let mut member = device(&app, &seeded, "mach-sn-member", FOLLOWS).await;
    let sid = followed(&app, &tid, &admin, &mut primary, &mut member, "/srv/sn").await;

    let mut grants = Vec::new();
    for _ in 0..2 {
        send(&mut member.ws, want(&sid, "direct")).await;
        let g = read_until(&mut primary.ws, "rc:hive.replica.grant")
            .await
            .expect("a grant");
        grants.push(g["grant_id"].as_str().unwrap().to_string());
    }
    assert!(
        arrives(
            &mut primary.ws,
            "rc:hive.replica.close",
            Duration::from_millis(300)
        )
        .await
        .is_none(),
        "the second comes up beside the first"
    );
    send(&mut member.ws, want(&sid, "direct")).await;
    let frames = frames_within(&mut primary.ws, Duration::from_millis(1200)).await;
    assert!(
        frames.iter().any(|f| f["t"] == "rc:hive.replica.grant"),
        "the third is granted: {frames:?}"
    );
    let closed: Vec<&Value> = frames
        .iter()
        .filter(|f| f["t"] == "rc:hive.replica.close")
        .collect();
    assert_eq!(closed.len(), 1, "one replaced: {frames:?}");
    assert_eq!(
        (closed[0]["grant_id"].as_str(), closed[0]["reason"].as_str()),
        (Some(grants[0].as_str()), Some("replaced"))
    );
    let told = read_until(&mut member.ws, "rc:hive.replica.close")
        .await
        .expect("the member is told too");
    assert_eq!(told["grant_id"], grants[0].as_str());
}

/// A member placement lets go stops following at once, and a removed
/// device's streams end with it: the other side is told each time.
#[tokio::test]
async fn a_member_let_go_or_a_device_removed_ends_its_streams() {
    let app = stream_app().await;
    let seeded = app.seed_tenant("hivestreamend").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-se-primary", SERVES_STREAMS).await;
    set_device(&app, &primary.agent_id, doc! { "tags": ["prod"] }).await;
    let mut member = device(&app, &seeded, "mach-se-member", FOLLOWS).await;
    set_device(&app, &member.agent_id, doc! { "tags": ["prod"] }).await;
    let sid = followed(&app, &tid, &admin, &mut primary, &mut member, "/srv/se").await;

    let ready = |gid: &str| {
        json!({"t": "rc:hive.replica.ready", "grant_id": gid, "cert_fingerprint": hash("ab"),
               "addrs": ["192.0.2.7:41641"]})
    };
    send(&mut member.ws, want(&sid, "direct")).await;
    let g = read_until(&mut primary.ws, "rc:hive.replica.grant")
        .await
        .expect("a grant");
    let gid = g["grant_id"].as_str().unwrap().to_string();
    send(&mut primary.ws, ready(&gid)).await;
    read_until(&mut member.ws, "rc:hive.replica.dial")
        .await
        .expect("a dial");

    // The member's tag taken off: the restricted session lets it go.
    tag(&app, &tid, &admin, &member.agent_id, &[]).await;
    for (who, ws) in [("source", &mut primary.ws), ("member", &mut member.ws)] {
        let c = read_until(ws, "rc:hive.replica.close")
            .await
            .unwrap_or_else(|| panic!("the {who} is told"));
        assert_eq!(
            (c["grant_id"].as_str(), c["reason"].as_str()),
            (Some(gid.as_str()), Some("member_left")),
            "{who}"
        );
    }

    // A second session, followed by another device carrying the tag; then
    // its source is removed.
    let mut other = device(&app, &seeded, "mach-se-other", FOLLOWS).await;
    set_device(&app, &other.agent_id, doc! { "tags": ["prod"] }).await;
    let sid2 = followed(&app, &tid, &admin, &mut primary, &mut other, "/srv/se2").await;
    send(&mut other.ws, want(&sid2, "direct")).await;
    let g = read_until(&mut primary.ws, "rc:hive.replica.grant")
        .await
        .expect("a grant for the second");
    let gid = g["grant_id"].as_str().unwrap().to_string();
    send(&mut primary.ws, ready(&gid)).await;
    read_until(&mut other.ws, "rc:hive.replica.dial")
        .await
        .expect("a dial");
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{tid}/agent/{}", primary.agent_id),
            &admin,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "remove: {}", resp.status());
    let c = read_until(&mut other.ws, "rc:hive.replica.close")
        .await
        .expect("the member is told its source is gone");
    assert_eq!(
        (c["grant_id"].as_str(), c["reason"].as_str()),
        (Some(gid.as_str()), Some("device_removed"))
    );
}

/// With the replicaset's switch off, a want is answered by nothing: no grant
/// to the source, no refusal to the member.
#[tokio::test]
async fn with_the_switch_off_no_stream_is_granted() {
    let on = stream_app().await;
    let seeded = on.seed_tenant("hivestreamoff").await;
    on_pro(&on, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&on, &seeded, "mach-so2-primary", SERVES_STREAMS).await;
    let mut member = device(&on, &seeded, "mach-so2-member", FOLLOWS).await;
    let sid = followed(&on, &tid, &admin, &mut primary, &mut member, "/srv/so2").await;

    let off = switch_off(&on).await;
    primary.ws.close(None).await.unwrap();
    member.ws.close(None).await.unwrap();
    wait_offline(&on, &seeded, &primary.agent_id).await;
    wait_offline(&on, &seeded, &member.agent_id).await;
    let mut source = connect(&off, &primary.token, &primary.machine, SERVES_STREAMS).await;
    let mut follower = connect(&off, &member.token, &member.machine, FOLLOWS).await;
    wait_agent_online(&off, &seeded, &member.agent_id).await;
    send(&mut follower, want(&sid, "direct")).await;
    assert!(
        arrives(
            &mut source,
            "rc:hive.replica.grant",
            Duration::from_millis(800)
        )
        .await
        .is_none(),
        "no grant"
    );
    assert!(
        arrives(
            &mut follower,
            "rc:hive.replica.refused",
            Duration::from_millis(300)
        )
        .await
        .is_none(),
        "no refusal either"
    );
}

// ─── P2d-5 — a replica's read-only view ───────────────────────────────────

/// A primary that runs sessions, carries streams and serves views.
const SERVES_VIEWS: &[&str] = &["exec", "hive", "hive-view", "hive-stream"];
/// A member that holds copies, carries streams and serves views of them.
const COPIES_VIEWS: &[&str] = &["exec", "hive", "hive-view", "hive-replica", "hive-stream"];

/// Open a view of `sid`, have `dev` confirm the grant it is asked, and the
/// grant it was asked and the browser's `ready`. `asked` says why `dev` is
/// the one asked, in the panic when it is not.
async fn view_on(ws: &mut UserWs, dev: &mut Device, sid: &str, asked: &str) -> (Value, Value) {
    user_send(ws, "hive:view.open", json!({"session_id": sid})).await;
    let grant = read_until(&mut dev.ws, "rc:hive.view.grant")
        .await
        .unwrap_or_else(|| panic!("not asked: {asked}"));
    let gid = grant["grant_id"].as_str().unwrap().to_string();
    send(
        &mut dev.ws,
        json!({"t": "rc:hive.view.grant_ack", "grant_id": gid}),
    )
    .await;
    let ready = user_read(ws, "hive:view.ready").await.expect("ready");
    assert_eq!(ready["grant_id"], gid.as_str());
    (grant, ready)
}

/// The views of `tenant` sent to `device` from a copy, as the audit counts
/// them, once they number `want` (within 5 s), or the count then.
async fn copy_views(app: &TestApp, tenant: &str, device: &str, want: u64) -> u64 {
    let filter = doc! {
        "tenant_id": ObjectId::parse_str(tenant).unwrap(),
        "device_id": ObjectId::parse_str(device).unwrap(),
        "action": "view",
        "outcome": "sent",
        "reason": "copy",
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let n = app
            .db
            .collection::<Document>("hive_audit")
            .count_documents(filter.clone())
            .await
            .unwrap();
        if n == want || Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// P2d-5 — the primary serves a view while it can, the starter driving. Once
/// it cannot, a member's copy does, read only for everyone, the starter
/// included, its grant carrying no address; and the audit row says `copy`.
#[tokio::test]
async fn a_view_reads_a_members_copy_once_the_primary_cannot_serve() {
    let app = stream_app().await;
    let seeded = app.seed_tenant("hivecopy").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&app, &seeded, "mach-cp-primary", SERVES_VIEWS).await;
    let mut member = device(&app, &seeded, "mach-cp-member", COPIES_VIEWS).await;
    let sid = followed(&app, &tid, &admin, &mut primary, &mut member, "/srv/cp").await;
    let mut ws = user_ws(&app, &admin).await;

    let (grant, ready) = view_on(
        &mut ws,
        &mut primary,
        &sid,
        "the primary, while it can serve",
    )
    .await;
    assert_eq!(
        (grant["may_prompt"].as_bool(), ready["may_prompt"].as_bool()),
        (Some(true), Some(true)),
        "the primary while it can serve, the starter driving: {grant}"
    );
    assert!(
        !device_hears(
            &mut member.ws,
            "rc:hive.view.grant",
            Duration::from_millis(300)
        )
        .await,
        "no copy is asked while the primary serves"
    );
    user_send(
        &mut ws,
        "hive:view.close",
        json!({"grant_id": ready["grant_id"]}),
    )
    .await;

    primary.ws.close(None).await.unwrap();
    wait_offline(&app, &seeded, &primary.agent_id).await;
    let (grant, ready) = view_on(
        &mut ws,
        &mut member,
        &sid,
        "the member's copy, once the primary cannot",
    )
    .await;
    assert_eq!(grant["session_id"], sid.as_str());
    assert_eq!(
        (grant["may_prompt"].as_bool(), ready["may_prompt"].as_bool()),
        (Some(false), Some(false)),
        "a copy is read only, the starter's view too: {grant}"
    );
    assert!(
        grant["user_email"].is_null(),
        "a reader's grant carries no address: {grant}"
    );
    assert_eq!(
        copy_views(&app, &tid, &member.agent_id, 1).await,
        1,
        "the audit row says the view was a copy"
    );
}

/// P2d-5 — of the members that hold a copy, the freshest by the tip it last
/// reported serves the view. One whose connection serves no copies (no
/// `hive-replica`) is passed over, and with none left the primary's word
/// refuses.
#[tokio::test]
async fn a_view_goes_to_the_freshest_copy_that_can_serve() {
    let app = stream_app().await;
    let seeded = app.seed_tenant("hivefresh").await;
    on_pro(&app, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    // Three copies: the primary and both members (decision 2's two would
    // place one).
    let mut policy = a_policy(0);
    policy["replicaset"]["restricted_tags"] = json!([]);
    let (code, p) = put_policy(&app, &tid, &admin, &policy).await;
    assert_eq!(code, 200, "{p}");
    let mut primary = device(&app, &seeded, "mach-fr-primary", SERVES_VIEWS).await;
    let mut behind = device(&app, &seeded, "mach-fr-behind", COPIES_VIEWS).await;
    let mut ahead = device(&app, &seeded, "mach-fr-ahead", COPIES_VIEWS).await;
    let s = started(&app, &tid, &admin, &mut primary, "/srv/fr").await;
    let sid = s["id"].as_str().unwrap().to_string();
    for (m, seq) in [(&mut behind, 3), (&mut ahead, 7)] {
        read_until(&mut m.ws, "rc:hive.replica.join")
            .await
            .expect("the member's join");
        send(
            &mut m.ws,
            json!({"t": "rc:hive.replica.join_ack", "session_id": sid, "fence": 1}),
        )
        .await;
        wait_member(&app, &tid, &admin, &sid, &m.agent_id, "joined", |v| {
            v["state"] == "joined"
        })
        .await;
        send(
            &mut m.ws,
            json!({"t": "rc:hive.replica.tip", "session_id": sid, "fence": 1, "seq": seq,
                   "hash": hash("ab")}),
        )
        .await;
        wait_member(&app, &tid, &admin, &sid, &m.agent_id, "its tip", |v| {
            v["tip"]["seq"] == seq
        })
        .await;
    }
    primary.ws.close(None).await.unwrap();
    wait_offline(&app, &seeded, &primary.agent_id).await;

    let mut ws = user_ws(&app, &admin).await;
    let (grant, ready) = view_on(&mut ws, &mut ahead, &sid, "the freshest copy").await;
    assert_eq!(grant["may_prompt"], false, "{grant}");
    assert!(
        !device_hears(
            &mut behind.ws,
            "rc:hive.view.grant",
            Duration::from_millis(300)
        )
        .await,
        "the copy behind is not asked"
    );
    user_send(
        &mut ws,
        "hive:view.close",
        json!({"grant_id": ready["grant_id"]}),
    )
    .await;

    // The copy ahead comes back on a connection that serves no copies.
    ahead.ws.close(None).await.unwrap();
    wait_offline(&app, &seeded, &ahead.agent_id).await;
    let mut no_copies = connect(&app, &ahead.token, &ahead.machine, VIEWS_HIVE).await;
    wait_agent_online(&app, &seeded, &ahead.agent_id).await;
    let (grant, ready) = view_on(
        &mut ws,
        &mut behind,
        &sid,
        "the copy behind, once the one ahead serves no copies",
    )
    .await;
    assert_eq!(grant["may_prompt"], false, "{grant}");
    assert!(
        !device_hears(
            &mut no_copies,
            "rc:hive.view.grant",
            Duration::from_millis(300)
        )
        .await,
        "a connection that serves no copies is passed over"
    );
    user_send(
        &mut ws,
        "hive:view.close",
        json!({"grant_id": ready["grant_id"]}),
    )
    .await;

    behind.ws.close(None).await.unwrap();
    wait_offline(&app, &seeded, &behind.agent_id).await;
    user_send(&mut ws, "hive:view.open", json!({"session_id": sid})).await;
    let refused = user_read(&mut ws, "hive:view.refused")
        .await
        .expect("with no copy left, the primary's word refuses");
    assert_eq!(
        refused["reason"], "device_offline",
        "with no copy left, the primary's word: {refused}"
    );
}

/// P2d-5 — with the replicaset's switch off, no copy serves a view, whatever
/// its members hold: the primary's word refuses.
#[tokio::test]
async fn with_the_switch_off_no_copy_serves_a_view() {
    let on = stream_app().await;
    let seeded = on.seed_tenant("hivecopyoff").await;
    on_pro(&on, &seeded).await;
    let tid = seeded.tenant_id.clone();
    let admin = seeded.admin.access_token.clone();
    let mut primary = device(&on, &seeded, "mach-co-primary", SERVES_VIEWS).await;
    let mut member = device(&on, &seeded, "mach-co-member", COPIES_VIEWS).await;
    let sid = followed(&on, &tid, &admin, &mut primary, &mut member, "/srv/co").await;

    let off = switch_off(&on).await;
    primary.ws.close(None).await.unwrap();
    member.ws.close(None).await.unwrap();
    wait_offline(&on, &seeded, &primary.agent_id).await;
    wait_offline(&on, &seeded, &member.agent_id).await;
    let mut copier = connect(&off, &member.token, &member.machine, COPIES_VIEWS).await;
    wait_agent_online(&off, &seeded, &member.agent_id).await;

    let mut ws = user_ws(&off, &admin).await;
    user_send(&mut ws, "hive:view.open", json!({"session_id": sid})).await;
    let refused = user_read(&mut ws, "hive:view.refused")
        .await
        .expect("with the switch off, the primary's word refuses");
    assert_eq!(refused["reason"], "device_offline", "{refused}");
    assert!(
        !device_hears(
            &mut copier,
            "rc:hive.view.grant",
            Duration::from_millis(300)
        )
        .await,
        "no copy is asked"
    );
}

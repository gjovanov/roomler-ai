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
}

/// Gate 4 (`HIVE_RUN`) for a plain member: a 200 carrying the reason, no
/// session created, nothing sent — and an audit row, because a refusal is
/// the interesting decision.
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

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

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-3a-1 — a scan, asked, delivered and answered.
//!
//! What these lock: a person with `KNOWHOW_EDIT` asks a device to scan, the
//! device is sent `rc:knowhow.scan` on its own connection, and its answer is
//! recorded once, counted by kind; a device that cannot scan, or is a
//! secondary enrollment, is refused 409 up front; a pending scan is never
//! doubled; an offline device is told at its next hello, and a scan sent to
//! a connection that died is sent again; an answer for a scan the device
//! was not sent is dropped; a scan asked on another pod goes out at this
//! pod's sweep; and one never answered expires, freeing the device for the
//! next.

use std::time::Duration;

use bson::{Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::knowhow_tests::{call, kpath};
use crate::vault_lease_tests::enroll;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Knowhow on, and a sweep every second, so a test waits on it in seconds.
async fn scan_app() -> TestApp {
    TestApp::spawn_with_settings(|s| {
        s.modules.knowhow = true;
        s.knowhow.scan_sweep_secs = 1;
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// An agent socket's hello, advertising `rpc` and, when given, the
/// `update_mode` a secondary enrollment says (`primary_org`).
async fn hello(app: &TestApp, token: &str, rpc: &[&str], update_mode: &str) -> Ws {
    let url = format!("ws://{}/ws?token={}&role=agent", app.addr, urlencode(token));
    let (mut ws, _) = connect_async(&url).await.expect("ws connect");
    let hello = json!({
        "t": "rc:agent.hello",
        "machine_name": "scan box",
        "os": "linux",
        "agent_version": "0.4.155",
        "displays": [],
        "caps": {
            "hw_encoders": [], "codecs": ["h264"],
            "has_input_permission": false, "supports_clipboard": false,
            "supports_file_transfer": false, "max_simultaneous_sessions": 1,
            "rpc": rpc,
            "update_mode": update_mode,
        }
    });
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .expect("send hello");
    ws
}

/// The next message of type `want`, or `None` after `wait`.
async fn recv_t(ws: &mut Ws, want: &str, wait: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&txt)
                    && v["t"] == want
                {
                    return Some(v);
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return None,
        }
    }
}

/// The scan the device is sent next: its id.
async fn scan_sent(ws: &mut Ws) -> String {
    let frame = recv_t(ws, "rc:knowhow.scan", Duration::from_secs(10))
        .await
        .expect("the device is sent its scan");
    assert_eq!(
        frame.as_object().unwrap().len(),
        2,
        "an id and nothing else: {frame}"
    );
    frame["scan_id"].as_str().unwrap().to_string()
}

async fn answer(ws: &mut Ws, body: Value) {
    let mut frame = body;
    frame["t"] = json!("rc:knowhow.scanned");
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .expect("send the answer");
}

/// Ask `device` to scan, as `token`.
async fn ask(app: &TestApp, t: &SeededTenant, token: &str, device: &str) -> (u16, Value) {
    call(
        app.auth_post(&kpath(&t.tenant_id, "/scan"), token)
            .json(&json!({ "device": device })),
    )
    .await
}

/// A device's scans, newest first.
async fn scans(app: &TestApp, t: &SeededTenant, device: &str) -> Vec<Value> {
    let (code, body) = call(app.auth_get(
        &kpath(&t.tenant_id, &format!("/scan?device={device}")),
        &t.admin.access_token,
    ))
    .await;
    assert_eq!(code, 200, "{body}");
    body["scans"].as_array().unwrap().clone()
}

/// Wait until the device's newest scan satisfies `ready`.
async fn wait_scan(
    app: &TestApp,
    t: &SeededTenant,
    device: &str,
    ready: impl Fn(&Value) -> bool,
) -> Value {
    for _ in 0..100 {
        if let Some(s) = scans(app, t, device).await.into_iter().next()
            && ready(&s)
        {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "the scan of {device} never got there: {:?}",
        scans(app, t, device).await
    );
}

fn coll(app: &TestApp) -> mongodb::Collection<Document> {
    app.db.collection::<Document>("knowhow_scans")
}

/// A person asks, the device is sent the scan on its own connection, and its
/// answer is recorded once, counted by kind.
#[tokio::test]
async fn a_scan_reaches_the_device_and_its_answer_is_recorded() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowscanok").await;
    let (device, token) = enroll(&app, &t, "mach-knowscan-a", "linux").await;
    let mut ws = hello(&app, &token, &["exec", "knowhow-scan"], "self").await;

    let (code, asked) = ask(&app, &t, &t.admin.access_token, &device).await;
    assert_eq!(code, 202, "{asked}");
    assert!(
        matches!(asked["state"].as_str(), Some("requested" | "delivered")),
        "{asked}"
    );
    assert_eq!(asked["device_id"], json!(device));
    assert_eq!(asked["requested_by"], json!(t.admin.id));
    let id = asked["id"].as_str().unwrap().to_string();
    assert_eq!(scan_sent(&mut ws).await, id);

    answer(
        &mut ws,
        json!({
            "scan_id": id,
            "outcome": "done",
            "findings": [
                { "kind": "checkout", "path": "/home/dev/src/api",
                  "remotes": [{ "name": "origin", "url": "git@github.com:acme/api.git" }] },
                { "kind": "context", "name": "prod", "server": "https://k8s.example.com:6443" },
                { "kind": "listener", "port": 5432, "protocol": "tcp", "address": "0.0.0.0" },
                { "kind": "tunnel_route", "id": "db", "flow": "forward", "local": 15432,
                  "node": ObjectId::new().to_hex(), "remote": "10.0.4.12:5432" },
                { "kind": "bucket", "name": "logs" },
                { "kind": "checkout", "path": "/src/x", "secret": "nope" },
            ],
        }),
    )
    .await;
    let done = wait_scan(&app, &t, &device, |s| s["state"] == json!("done")).await;
    assert_eq!(
        done["counts"],
        json!({ "checkouts": 1, "contexts": 1, "listeners": 1, "tunnel_routes": 1,
                "unknown": 1, "refused": 1 }),
        "{done}"
    );
    assert!(done.get("delivered_at").is_some() && done.get("answered_at").is_some());

    // Answered once: the same answer again changes nothing.
    answer(
        &mut ws,
        json!({ "scan_id": id, "outcome": "failed", "detail": "late" }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = scans(&app, &t, &device).await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0]["state"], json!("done"), "{after:?}");
    assert!(after[0].get("detail").is_none(), "{after:?}");
}

/// A device that cannot scan, or is a secondary enrollment, is refused 409
/// up front; a device that is not there is 404; a member without
/// `KNOWHOW_EDIT` is refused 403.
#[tokio::test]
async fn who_is_asked_and_who_may_ask() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowscanwho").await;
    let (old, old_token) = enroll(&app, &t, "mach-knowscan-b", "linux").await;
    let (second, second_token) = enroll(&app, &t, "mach-knowscan-c", "linux").await;
    let _w1 = hello(&app, &old_token, &["exec"], "self").await;
    let _w2 = hello(&app, &second_token, &["knowhow-scan"], "primary_org").await;
    // Each hello is recorded before its socket answers anything else.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (code, body) = ask(&app, &t, &t.admin.access_token, &old).await;
    assert_eq!(code, 409, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("cannot scan"),
        "{body}"
    );
    let (code, body) = ask(&app, &t, &t.admin.access_token, &second).await;
    assert_eq!(code, 409, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("primary organization"),
        "{body}"
    );
    let (code, body) = ask(&app, &t, &t.admin.access_token, &ObjectId::new().to_hex()).await;
    assert_eq!(code, 404, "{body}");
    let (code, body) = ask(&app, &t, &t.member.access_token, &old).await;
    assert_eq!(code, 403, "a member reads the map but does not ask: {body}");
    assert_eq!(
        coll(&app).count_documents(doc! {}).await.unwrap(),
        0,
        "no refusal left a scan"
    );
}

/// A pending scan is never doubled: a second request answers the first.
#[tokio::test]
async fn a_pending_scan_is_never_doubled() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowscanonce").await;
    let (device, token) = enroll(&app, &t, "mach-knowscan-d", "linux").await;
    let ws = hello(&app, &token, &["knowhow-scan"], "self").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(ws);
    let (c1, first) = ask(&app, &t, &t.admin.access_token, &device).await;
    let (c2, second) = ask(&app, &t, &t.admin.access_token, &device).await;
    assert_eq!((c1, c2), (202, 202), "{first} {second}");
    assert_eq!(first["id"], second["id"]);
    assert_eq!(scans(&app, &t, &device).await.len(), 1);
}

/// An offline device is told at its next hello, and a scan sent to a
/// connection that died unanswered is sent again at the one after.
#[tokio::test]
async fn an_offline_device_is_told_at_its_hello_and_again_after_a_dead_connection() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowscanhello").await;
    let (device, token) = enroll(&app, &t, "mach-knowscan-e", "linux").await;
    let ws = hello(&app, &token, &["knowhow-scan"], "self").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(ws);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (code, asked) = ask(&app, &t, &t.admin.access_token, &device).await;
    assert_eq!(
        (code, asked["state"].clone()),
        (202, json!("requested")),
        "{asked}"
    );
    let id = asked["id"].as_str().unwrap().to_string();

    let mut ws = hello(&app, &token, &["knowhow-scan"], "self").await;
    assert_eq!(scan_sent(&mut ws).await, id, "told at its hello");
    drop(ws);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut ws = hello(&app, &token, &["knowhow-scan"], "self").await;
    assert_eq!(
        scan_sent(&mut ws).await,
        id,
        "and again: it was never answered"
    );

    answer(&mut ws, json!({ "scan_id": id, "outcome": "no_roots" })).await;
    let s = wait_scan(&app, &t, &device, |s| s["state"] == json!("no_roots")).await;
    assert_eq!(
        s["counts"],
        json!({ "checkouts": 0, "contexts": 0, "listeners": 0, "tunnel_routes": 0,
                "unknown": 0, "refused": 0 })
    );
}

/// An answer for a scan the device was not sent is dropped; the device's own
/// answer is recorded, and a detail holding a credential's shape withheld.
#[tokio::test]
async fn an_answer_for_a_scan_it_was_not_sent_is_dropped() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowscanforge").await;
    let (a, a_token) = enroll(&app, &t, "mach-knowscan-f", "linux").await;
    let (_b, b_token) = enroll(&app, &t, "mach-knowscan-g", "linux").await;
    let mut wa = hello(&app, &a_token, &["knowhow-scan"], "self").await;
    let mut wb = hello(&app, &b_token, &["knowhow-scan"], "self").await;

    let (code, asked) = ask(&app, &t, &t.admin.access_token, &a).await;
    assert_eq!(code, 202, "{asked}");
    let id = scan_sent(&mut wa).await;
    answer(
        &mut wb,
        json!({ "scan_id": id, "outcome": "done",
                "findings": [{ "kind": "checkout", "path": "/planted" }] }),
    )
    .await;
    answer(
        &mut wb,
        json!({ "scan_id": ObjectId::new().to_hex(), "outcome": "done" }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let pending = scans(&app, &t, &a).await;
    assert_eq!(pending[0]["state"], json!("delivered"), "{pending:?}");

    let leak = format!(
        "git fetch https://dev:{}@git.example.com/api failed",
        "hunter2"
    );
    answer(
        &mut wa,
        json!({ "scan_id": id, "outcome": "failed", "detail": leak }),
    )
    .await;
    let s = wait_scan(&app, &t, &a, |s| s["state"] == json!("failed")).await;
    assert_eq!(
        s["detail"],
        json!("(withheld: it held a credential's shape)"),
        "{s}"
    );
}

/// A scan asked on another pod, a row this pod never saw requested, goes out
/// at this pod's sweep; one never answered expires, freeing the device for
/// the next.
#[tokio::test]
async fn the_sweep_delivers_what_another_pod_took_and_expires_the_unanswered() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowscansweep").await;
    let (device, token) = enroll(&app, &t, "mach-knowscan-h", "linux").await;
    let mut ws = hello(&app, &token, &["knowhow-scan"], "self").await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let tid = ObjectId::parse_str(&t.tenant_id).unwrap();
    let did = ObjectId::parse_str(&device).unwrap();
    let admin = ObjectId::parse_str(&t.admin.id).unwrap();
    let row = |at: bson::DateTime| {
        doc! {
            "tenant_id": tid, "device_id": did, "requested_by": admin,
            "requested_at": at, "state": "requested", "pending": true,
            "counts": {}, "truncated": false,
        }
    };
    let elsewhere = coll(&app)
        .insert_one(row(bson::DateTime::now()))
        .await
        .unwrap()
        .inserted_id
        .as_object_id()
        .unwrap();
    assert_eq!(
        scan_sent(&mut ws).await,
        elsewhere.to_hex(),
        "the sweep sent it"
    );
    answer(
        &mut ws,
        json!({ "scan_id": elsewhere.to_hex(), "outcome": "no_roots" }),
    )
    .await;
    wait_scan(&app, &t, &device, |s| s["state"] == json!("no_roots")).await;

    // Asked 25 hours ago, and never answered.
    drop(ws);
    let old =
        bson::DateTime::from_millis(bson::DateTime::now().timestamp_millis() - 25 * 3_600_000);
    let stale = coll(&app)
        .insert_one(row(old))
        .await
        .unwrap()
        .inserted_id
        .as_object_id()
        .unwrap();
    let mut expired = false;
    for _ in 0..60 {
        let r = coll(&app)
            .find_one(doc! { "_id": stale })
            .await
            .unwrap()
            .unwrap();
        if r.get_str("state") == Ok("expired") {
            assert!(r.get("pending").is_none(), "{r}");
            expired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(expired, "the sweep expired the unanswered scan");
    let (code, next) = ask(&app, &t, &t.admin.access_token, &device).await;
    assert_eq!(code, 202, "{next}");
    assert_ne!(
        next["id"],
        json!(stale.to_hex()),
        "a new scan, not the expired one"
    );
}

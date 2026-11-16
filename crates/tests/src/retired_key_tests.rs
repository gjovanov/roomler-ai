// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-40 P2 — a key a device rotated away from cannot be used to rejoin
//! (`docs/fr/FR-40-overlay-key-rotation.md` § Retired keys).
//!
//! Driven over real agent sockets: a join under a new key retires the old one
//! in every mode; under `enforce` a join or a DERP registration presenting a
//! retired key is refused `key_retired`, and a device that advertises
//! `key-rotate` is ordered to rotate (once, while that order runs); `warn`
//! admits and says so; `off` is the pre-P2 join. What the DEVICE then does with
//! the order is the agent's P1 path and the field criterion.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::{Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt, TryStreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::agent_presence_tests::enroll;
use crate::fixtures::{seed::SeededTenant, test_app::TestApp};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const WAIT: Duration = Duration::from_secs(5);
/// Long enough for a frame the server would send to arrive, when the test
/// asserts that none does.
const QUIET: Duration = Duration::from_millis(1500);

fn key(seed: u8) -> String {
    BASE64.encode([seed; 32])
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// An agent socket whose hello advertises `rpc`.
async fn connect(app: &TestApp, token: &str, rpc: &[&str]) -> Ws {
    let url = format!("ws://{}/ws?token={}&role=agent", app.addr, urlencode(token));
    let (mut ws, _) = connect_async(&url).await.expect("agent ws connect");
    ws.send(Message::Text(
        json!({
            "t": "rc:agent.hello",
            "machine_name": "retired-key box",
            "os": "linux",
            "agent_version": "0.4.199",
            "displays": [],
            "caps": {
                "hw_encoders": [], "codecs": ["h264"], "has_input_permission": true,
                "supports_clipboard": false, "supports_file_transfer": false,
                "max_simultaneous_sessions": 1, "rpc": rpc,
            },
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    ws
}

/// `rc:overlay.join` under the key made of `seed`. `refusal_aware` is the
/// FR-47 capability (0.4.41+); a 0.4.24 device omits it.
async fn join(ws: &mut Ws, seed: u8, refusal_aware: bool) {
    let mut msg = json!({
        "t": "rc:overlay.join",
        "wg_public_key": key(seed),
        "key_epoch": 0,
        "mtu": 1280,
        "endpoints": [format!("203.0.113.{seed}:51820")],
        "advertised_routes": [],
    });
    if refusal_aware {
        msg["supports_join_refusal"] = json!(true);
    }
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .expect("send join");
}

/// The next frame of type `want`, or `None` once `wait` passes.
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

/// Every frame that arrives within `wait`, by type.
async fn frames_within(ws: &mut Ws, wait: Duration) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut out = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return out;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                    out.push(v);
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return out,
        }
    }
}

/// A ping comes back: the socket is still served, so an absent frame means
/// the server sent none, not that the socket went away.
async fn still_served(ws: &mut Ws) -> bool {
    if ws
        .send(Message::Ping(b"retired".to_vec().into()))
        .await
        .is_err()
    {
        return false;
    }
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Pong(_)))) => return true,
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return false,
        }
    }
}

/// Join and require the netmap: the join was admitted.
async fn join_admitted(ws: &mut Ws, seed: u8) {
    join(ws, seed, true).await;
    assert!(
        recv_t(ws, "rc:overlay.netmap", WAIT).await.is_some(),
        "the join under key {seed} must be admitted"
    );
}

async fn set_mode(app: &TestApp, t: &SeededTenant, mode: &str) {
    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/overlay-retired-keys", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "mode": mode }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "set mode {mode}");
}

/// The device's live overlay node row.
async fn node_row(app: &TestApp, t: &SeededTenant, machine: &str) -> Document {
    app.db
        .collection::<Document>("overlay_nodes")
        .find_one(doc! {
            "tenant_id": ObjectId::parse_str(&t.tenant_id).unwrap(),
            "machine_id": machine,
            "deleted_at": null,
        })
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no live node for {machine}"))
}

/// The ledger's keys, oldest first.
fn ledger(row: &Document) -> Vec<(String, String)> {
    row.get_array("retired_keys")
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_document())
                .map(|e| {
                    (
                        e.get_str("public_key").unwrap_or_default().to_string(),
                        e.get_str("reason").unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn audit_rows(app: &TestApp, agent_id: &str) -> Vec<Document> {
    app.db
        .collection::<Document>("key_rotation_audit")
        .find(doc! { "agent_id": ObjectId::parse_str(agent_id).unwrap() })
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap()
}

async fn agent_view(app: &TestApp, t: &SeededTenant, agent_id: &str) -> Value {
    app.auth_get(
        &format!("/api/tenant/{}/agent/{agent_id}", t.tenant_id),
        &t.admin.access_token,
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap()
}

/// The switch: off by default, `MANAGE_AGENTS` to read or write it, and an
/// admin's typo is a 400 rather than a quiet `off`.
#[tokio::test]
async fn the_switch_is_off_by_default_and_its_write_is_strict() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr40p2a").await;
    let path = format!("/api/tenant/{}/overlay-retired-keys", t.tenant_id);

    let got: Value = app
        .auth_get(&path, &t.admin.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["mode"], "off", "{got}");

    for typo in ["enforced", "Enforce", "on", ""] {
        let resp = app
            .auth_put(&path, &t.admin.access_token)
            .json(&json!({ "mode": typo }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400, "{typo:?} must be refused");
    }
    let resp = app
        .auth_put(&path, &t.member.access_token)
        .json(&json!({ "mode": "enforce" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    let resp = app
        .auth_get(&path, &t.member.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    set_mode(&app, &t, "enforce").await;
    let got: Value = app
        .auth_get(&path, &t.admin.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["mode"], "enforce", "{got}");
}

/// The ledger records in every mode, from day one: a join under a new key
/// retires the old one, once, and `off` still admits a retired key exactly as
/// before P2 (a rollback join) — retiring the key it leaves behind.
#[tokio::test]
async fn a_key_change_at_a_join_retires_the_old_key_and_off_admits_as_before() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr40p2b").await;
    let (_aid, token) = enroll(&app, &t, "mach-fr40p2-b").await;
    let mut ws = connect(&app, &token, &["key-rotate"]).await;

    join_admitted(&mut ws, 11).await;
    assert!(ledger(&node_row(&app, &t, "mach-fr40p2-b").await).is_empty());

    join_admitted(&mut ws, 12).await;
    let row = node_row(&app, &t, "mach-fr40p2-b").await;
    assert_eq!(row.get_str("wg_public_key").unwrap(), key(12));
    assert_eq!(ledger(&row), vec![(key(11), "rejoin".to_string())]);

    // The same key again changes nothing: no entry for a key that stayed.
    join_admitted(&mut ws, 12).await;
    assert_eq!(ledger(&node_row(&app, &t, "mach-fr40p2-b").await).len(), 1);

    // `off`: the rolled-back key is ADMITTED, as before P2, and the key it
    // replaced is retired in turn. The retired key it presented is not
    // written twice.
    join_admitted(&mut ws, 11).await;
    let row = node_row(&app, &t, "mach-fr40p2-b").await;
    assert_eq!(row.get_str("wg_public_key").unwrap(), key(11));
    assert_eq!(
        ledger(&row),
        vec![
            (key(11), "rejoin".to_string()),
            (key(12), "rejoin".to_string()),
        ]
    );
}

/// The arm AC9 reads first: under `enforce` a join presenting a retired key
/// is refused `key_retired`, the device that advertises `key-rotate` is
/// ordered to rotate — by the server, audited as such — and is ordered ONCE
/// while that order runs. Its join under a new key is admitted, satisfies the
/// order, and retires the key it replaced as a rotation.
#[tokio::test]
async fn under_enforce_a_retired_key_is_refused_and_the_device_is_ordered_to_rotate() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr40p2c").await;
    let (aid, token) = enroll(&app, &t, "mach-fr40p2-c").await;
    let mut ws = connect(&app, &token, &["key-rotate"]).await;
    join_admitted(&mut ws, 21).await;
    join_admitted(&mut ws, 22).await; // retires 21
    set_mode(&app, &t, "enforce").await;

    join(&mut ws, 21, true).await;
    let frames = frames_within(&mut ws, QUIET).await;
    let refused = frames
        .iter()
        .find(|f| f["t"] == "rc:overlay.join_refused")
        .unwrap_or_else(|| panic!("no refusal frame: {frames:?}"));
    assert_eq!(refused["reason"], "key_retired", "{refused}");
    assert!(
        !refused["detail"]
            .as_str()
            .unwrap_or_default()
            .contains(&key(21)),
        "the detail names no key: {refused}"
    );
    let order = frames
        .iter()
        .find(|f| f["t"] == "rc:agent.key_rotate")
        .unwrap_or_else(|| panic!("no rotation order: {frames:?}"));
    let request_id = order["request_id"].as_str().unwrap().to_string();
    assert!(
        frames.iter().all(|f| f["t"] != "rc:overlay.netmap"),
        "a refused join gets no netmap: {frames:?}"
    );
    // Refusal first, then the order: the device logs why before it rotates.
    let at = |t: &str| frames.iter().position(|f| f["t"] == t).unwrap();
    assert!(at("rc:overlay.join_refused") < at("rc:agent.key_rotate"));

    // The node row still holds the key it held.
    assert_eq!(
        node_row(&app, &t, "mach-fr40p2-c")
            .await
            .get_str("wg_public_key")
            .unwrap(),
        key(22)
    );

    // The order is the SERVER's: no user, a reason, the presented key as the
    // key to move away from, delivered.
    let a = agent_view(&app, &t, &aid).await;
    let kr = &a["key_rotation"];
    assert_eq!(kr["request_id"].as_str(), Some(request_id.as_str()), "{a}");
    assert_eq!(kr["trigger"], "key_retired", "{kr}");
    assert_eq!(kr["requested_by"], ObjectId::from_bytes([0; 12]).to_hex());
    assert_eq!(kr["public_key_before"], key(21), "{kr}");
    assert!(!kr["delivered_at"].is_null(), "{kr}");

    // The loop guard: the same retired key again while the order runs is
    // refused again, and ordered NOTHING more.
    join(&mut ws, 21, true).await;
    let again = frames_within(&mut ws, QUIET).await;
    assert!(
        again.iter().any(|f| f["t"] == "rc:overlay.join_refused"),
        "{again:?}"
    );
    assert!(
        again.iter().all(|f| f["t"] != "rc:agent.key_rotate"),
        "a second order while the first runs: {again:?}"
    );
    let rows = audit_rows(&app, &aid).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].get_str("trigger").ok(), Some("key_retired"));
    assert_eq!(rows[0].get_str("dispatch").ok(), Some("pushed"));
    assert_eq!(
        rows[0].get_object_id("user_id").unwrap(),
        ObjectId::from_bytes([0; 12])
    );

    // The device rotates and joins under its new key: admitted, the order is
    // satisfied, and the key it replaced retires as a ROTATION.
    join_admitted(&mut ws, 23).await;
    let row = node_row(&app, &t, "mach-fr40p2-c").await;
    assert_eq!(row.get_str("wg_public_key").unwrap(), key(23));
    assert_eq!(
        ledger(&row),
        vec![
            (key(21), "rejoin".to_string()),
            (key(22), "rotate".to_string()),
        ]
    );
    // The identity stamp is written off the join path.
    let mut state = Value::Null;
    for _ in 0..30 {
        state = agent_view(&app, &t, &aid).await["key_rotation"]["state"].clone();
        if state == "rotated" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(state, "rotated");
}

/// The other arm: a device WITHOUT the verb is refused and ordered nothing —
/// audited as such — and a pre-FR-47 device (0.4.24 and older, no
/// `supports_join_refusal`) is not even sent the frame: it stays off, its
/// socket still served, and the server's log and audit carry the reason.
#[tokio::test]
async fn under_enforce_a_device_without_the_verb_is_refused_and_ordered_nothing() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr40p2d").await;
    let (aid, token) = enroll(&app, &t, "mach-fr40p2-d").await;
    let mut ws = connect(&app, &token, &[]).await;
    join_admitted(&mut ws, 31).await;
    join_admitted(&mut ws, 32).await;
    set_mode(&app, &t, "enforce").await;

    // Refusal-aware without the verb: no shipped build is both, but the
    // server must not infer either from the other.
    join(&mut ws, 31, true).await;
    let frames = frames_within(&mut ws, QUIET).await;
    let refused = frames
        .iter()
        .find(|f| f["t"] == "rc:overlay.join_refused")
        .unwrap_or_else(|| panic!("no refusal frame: {frames:?}"));
    assert_eq!(refused["reason"], "key_retired");
    assert!(
        frames
            .iter()
            .all(|f| f["t"] != "rc:agent.key_rotate" && f["t"] != "rc:overlay.netmap"),
        "{frames:?}"
    );
    assert!(agent_view(&app, &t, &aid).await["key_rotation"].is_null());
    let rows = audit_rows(&app, &aid).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].get_str("denied").ok(), Some("agent_unsupported"));
    assert_eq!(rows[0].get_str("trigger").ok(), Some("key_retired"));

    // The 0.4.24 shape: no refusal capability, so no frame at all — and no
    // netmap — while the socket stays served.
    join(&mut ws, 31, false).await;
    let frames = frames_within(&mut ws, QUIET).await;
    assert!(
        frames.iter().all(|f| f["t"] != "rc:overlay.join_refused"
            && f["t"] != "rc:overlay.netmap"
            && f["t"] != "rc:agent.key_rotate"),
        "{frames:?}"
    );
    assert!(
        still_served(&mut ws).await,
        "the refused device keeps its socket"
    );
    assert_eq!(
        node_row(&app, &t, "mach-fr40p2-d")
            .await
            .get_str("wg_public_key")
            .unwrap(),
        key(32)
    );
}

/// `warn` admits a retired key and orders nothing; flipped back to `off`, the
/// join is the pre-P2 join.
#[tokio::test]
async fn warn_admits_and_orders_nothing() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr40p2e").await;
    let (aid, token) = enroll(&app, &t, "mach-fr40p2-e").await;
    let mut ws = connect(&app, &token, &["key-rotate"]).await;
    join_admitted(&mut ws, 41).await;
    join_admitted(&mut ws, 42).await;

    set_mode(&app, &t, "warn").await;
    join(&mut ws, 41, true).await;
    let frames = frames_within(&mut ws, QUIET).await;
    assert!(
        frames.iter().any(|f| f["t"] == "rc:overlay.netmap"),
        "warn admits: {frames:?}"
    );
    assert!(
        frames
            .iter()
            .all(|f| f["t"] != "rc:overlay.join_refused" && f["t"] != "rc:agent.key_rotate"),
        "{frames:?}"
    );
    assert!(audit_rows(&app, &aid).await.is_empty());
    assert!(agent_view(&app, &t, &aid).await["key_rotation"].is_null());
}

/// The ledger is read network-wide — a key retired by one device is refused
/// from another device of the SAME org — and per org: another org's device
/// presenting the same key is admitted, its network has retired nothing.
#[tokio::test]
async fn a_retired_key_is_refused_across_the_org_and_nowhere_else() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr40p2f").await;
    let other = app.seed_tenant("fr40p2g").await;
    let (_a, token_a) = enroll(&app, &t, "mach-fr40p2-f1").await;
    let (_b, token_b) = enroll(&app, &t, "mach-fr40p2-f2").await;
    let (_c, token_c) = enroll(&app, &other, "mach-fr40p2-g1").await;

    let mut a = connect(&app, &token_a, &[]).await;
    join_admitted(&mut a, 51).await;
    join_admitted(&mut a, 52).await; // retires 51 in org t
    set_mode(&app, &t, "enforce").await;
    set_mode(&app, &other, "enforce").await;

    let mut b = connect(&app, &token_b, &[]).await;
    join(&mut b, 51, true).await;
    let refused = recv_t(&mut b, "rc:overlay.join_refused", WAIT)
        .await
        .expect("org t refuses a key one of its devices retired");
    assert_eq!(refused["reason"], "key_retired");

    let mut c = connect(&app, &token_c, &[]).await;
    join_admitted(&mut c, 51).await;
}

/// DERP: a node whose OWN key is retired — admitted while the switch was off
/// — is refused registration under `enforce`, and registers under `off` as
/// before P2.
#[tokio::test]
async fn derp_refuses_a_retired_key_under_enforce_and_not_under_off() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr40p2h").await;
    let (_aid, token) = enroll(&app, &t, "mach-fr40p2-h").await;
    let mut ws = connect(&app, &token, &[]).await;
    join_admitted(&mut ws, 61).await;
    join_admitted(&mut ws, 62).await; // retires 61
    join_admitted(&mut ws, 61).await; // `off`: the rollback is admitted; 61 is live AND retired
    let network_id = node_row(&app, &t, "mach-fr40p2-h")
        .await
        .get_object_id("network_id")
        .unwrap();
    let registered = |app: &TestApp| {
        app.state
            .network()
            .derp_registry
            .contains_key(&(network_id, [61u8; 32]))
    };

    async fn register(app: &TestApp, token: &str) -> Ws {
        let (mut d, _) = connect_async(&format!(
            "ws://{}/derp?token={}",
            app.addr,
            urlencode(token)
        ))
        .await
        .expect("derp ws");
        d.send(Message::Binary([61u8; 32].to_vec().into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        d
    }

    set_mode(&app, &t, "enforce").await;
    let d = register(&app, &token).await;
    assert!(!registered(&app), "enforce refuses the retired key at DERP");
    drop(d);

    set_mode(&app, &t, "off").await;
    let d = register(&app, &token).await;
    assert!(registered(&app), "off registers it, as before P2");
    drop(d);
}

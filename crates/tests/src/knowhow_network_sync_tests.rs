// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-2b — the network's word on each machine.
//!
//! What these lock: through a real overlay join and an admin's approvals, a
//! machine carries the address its device leases, its name and MagicDNS name
//! exactly as the device list shows them, the subnets it carries for its
//! peers, whether it is an exit, gate 3, whether its build serves SSH and its
//! tunnel routes, every one the sync's; an approval the device no longer
//! offers is no exit; a reconnect's hello carries what a known node says; an
//! evicted node's facts leave the machine while the rest of it stays; and a
//! machine keeps room for the sync's facts beside a person's own.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::{doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use roomler_ai_services::dao::tenant::TenantDao;
use serde_json::{Map, Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::knowhow_sync_tests::{fact, knowhow, machine, update_device, wait_machine};
use crate::knowhow_tests::{call, knowhow_app, kpath, patch};
use crate::vault_lease_tests::enroll;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const DEFAULT_ROUTE: &str = "0.0.0.0/0";

/// The overlay's facts on a machine.
const OVERLAY: [&str; 5] = [
    "overlay.exit",
    "overlay.ipv4",
    "overlay.magicdns",
    "overlay.name",
    "overlay.routes",
];

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// An agent socket's hello, advertising the verbs `rpc`.
async fn hello(app: &TestApp, token: &str, rpc: &[&str]) -> Ws {
    let url = format!("ws://{}/ws?token={}&role=agent", app.addr, urlencode(token));
    let (mut ws, _) = connect_async(&url).await.expect("ws connect");
    let hello = json!({
        "t": "rc:agent.hello",
        "machine_name": "knownet box",
        "os": "linux",
        "agent_version": "0.4.154",
        "displays": [],
        "caps": {
            "hw_encoders": [], "codecs": ["h264"],
            "has_input_permission": false, "supports_clipboard": false,
            "supports_file_transfer": false, "max_simultaneous_sessions": 1,
            "rpc": rpc,
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

/// A real `rc:overlay.join` offering `routes`: the address it leases.
async fn join(ws: &mut Ws, seed: u8, routes: &[&str]) -> String {
    let msg = json!({
        "t": "rc:overlay.join",
        "wg_public_key": BASE64.encode([seed; 32]),
        "mtu": 1280,
        "endpoints": [format!("203.0.113.{seed}:51820")],
        "advertised_routes": routes,
    });
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .expect("send join");
    let netmap = recv_t(ws, "rc:overlay.netmap", Duration::from_secs(10))
        .await
        .expect("a netmap answers the join");
    netmap["self_ip"]
        .as_str()
        .expect("the address it leases")
        .to_string()
}

/// The overlay node leasing `ip`, from the admin's list.
async fn node_id(app: &TestApp, t: &SeededTenant, ip: &str) -> String {
    let (code, nodes) = call(app.auth_get(
        &format!("/api/tenant/{}/overlay-node", t.tenant_id),
        &t.admin.access_token,
    ))
    .await;
    assert_eq!(code, 200, "{nodes}");
    nodes["items"]
        .as_array()
        .expect("the node list")
        .iter()
        .find(|n| n["overlay_ip"] == ip)
        .and_then(|n| n["id"].as_str())
        .expect("the node's row")
        .to_string()
}

/// An admin's `PUT /api/tenant/{tid}{rest}`, which must succeed.
async fn admin_put(app: &TestApp, t: &SeededTenant, rest: &str, body: Value) {
    let (code, resp) = call(
        app.auth_put(
            &format!("/api/tenant/{}{rest}", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&body),
    )
    .await;
    assert!(
        (200..300).contains(&code),
        "PUT {rest} {body} → {code} {resp}"
    );
}

/// The device list's row for `agent_id`.
async fn device_row(app: &TestApp, t: &SeededTenant, agent_id: &str) -> Value {
    let (code, list) = call(app.auth_get(
        &format!("/api/tenant/{}/device", t.tenant_id),
        &t.admin.access_token,
    ))
    .await;
    assert_eq!(code, 200, "{list}");
    list["items"]
        .as_array()
        .expect("the device list")
        .iter()
        .find(|r| r["id"] == agent_id)
        .cloned()
        .expect("the device's row")
}

async fn reconcile(app: &TestApp) {
    roomler_ai_mod_knowhow::sync::all(knowhow(app))
        .await
        .unwrap();
}

/// Through a real join and an admin's approvals, a machine carries what the
/// network says of its device, each as the device list shows it and every
/// one the sync's; an approval the device no longer offers is no exit; and a
/// person cannot change any of it.
#[tokio::test]
async fn the_networks_word_reaches_the_machine() {
    let app = knowhow_app().await;
    let t = app.seed_tenant("knownetword").await;
    let (agent_id, token) = enroll(&app, &t, "mach-knownet-a", "linux").await;
    let mut ws = hello(&app, &token, &["exec", "ssh"]).await;

    // The hello's sync, before any join: SSH and the tunnel routes, nothing
    // of the overlay.
    let card = wait_machine(&app, &t, &agent_id, |c| {
        fact(c, "ssh.server")["value"] == json!(true)
    })
    .await;
    assert_eq!(fact(&card, "ssh.policy")["value"], json!("off"), "{card}");
    assert_eq!(fact(&card, "tunnel.routes")["value"], json!([]), "{card}");
    for path in OVERLAY {
        assert_eq!(
            fact(&card, path),
            Value::Null,
            "{path} before any join: {card}"
        );
    }

    // The device joins offering a subnet and the default route. An admin
    // approves both, opens gate 3 and gives it a tunnel route, and the
    // organization takes a MagicDNS domain.
    let ip = join(&mut ws, 7, &["10.20.0.0/16", DEFAULT_ROUTE]).await;
    let node = node_id(&app, &t, &ip).await;
    admin_put(
        &app,
        &t,
        &format!("/overlay-node/{node}/approved-routes"),
        json!({ "approved_routes": ["10.20.0.0/16"] }),
    )
    .await;
    admin_put(
        &app,
        &t,
        &format!("/overlay-node/{node}/exit-node"),
        json!({ "enabled": true }),
    )
    .await;
    admin_put(
        &app,
        &t,
        &format!("/agent/{agent_id}/ssh-policy"),
        json!({ "mode": "on", "account_mode": "daemon", "consent_mode": "auto" }),
    )
    .await;
    update_device(
        &app,
        &t,
        &agent_id,
        json!({ "routes": ["192.168.50.0/24"] }),
    )
    .await;
    TenantDao::new(&app.db)
        .set_magic_dns(
            ObjectId::parse_str(&t.tenant_id).unwrap(),
            Some("corp.example".to_string()),
            vec![],
        )
        .await
        .unwrap();

    // None of that called knowhow: the reconcile carries it.
    reconcile(&app).await;
    let card = machine(&app, &t, &agent_id).await.unwrap();
    let row = device_row(&app, &t, &agent_id).await;
    assert_eq!(fact(&card, "overlay.ipv4")["value"], json!(ip), "{card}");
    assert_eq!(
        fact(&card, "overlay.ipv4")["value"],
        row["overlay_ip"],
        "{row}"
    );
    assert_eq!(
        fact(&card, "overlay.name")["value"],
        row["magic_dns_name"],
        "{row}"
    );
    assert_eq!(
        fact(&card, "overlay.magicdns")["value"],
        row["magic_dns_fqdn"],
        "{row}"
    );
    assert!(
        row["magic_dns_fqdn"]
            .as_str()
            .is_some_and(|n| n.ends_with(".corp.example")),
        "{row}"
    );
    assert_eq!(
        fact(&card, "overlay.routes")["value"],
        json!(["10.20.0.0/16"]),
        "the subnets it carries, the default route apart: {card}"
    );
    assert_eq!(fact(&card, "overlay.exit")["value"], json!(true), "{card}");
    assert_eq!(fact(&card, "ssh.policy")["value"], json!("on"), "{card}");
    assert_eq!(
        fact(&card, "tunnel.routes")["value"],
        json!(["192.168.50.0/24"]),
        "{card}"
    );
    for path in OVERLAY
        .iter()
        .chain(["ssh.policy", "ssh.server", "tunnel.routes"].iter())
    {
        let f = fact(&card, path);
        assert_eq!(f["source"], json!("sync"), "{path}: {card}");
        assert!(
            f.get("by").is_none(),
            "{path} has no person behind it: {card}"
        );
    }

    // The device stops offering the default route: the approval stays, and
    // the exit goes.
    join(&mut ws, 7, &["10.20.0.0/16"]).await;
    reconcile(&app).await;
    let card = machine(&app, &t, &agent_id).await.unwrap();
    assert_eq!(
        fact(&card, "overlay.exit")["value"],
        json!(false),
        "an approval the device no longer offers is no exit: {card}"
    );
    assert_eq!(
        fact(&card, "overlay.routes")["value"],
        json!(["10.20.0.0/16"]),
        "{card}"
    );

    // And the network's word is not a person's to change.
    let id = card["entity"]["id"].as_str().unwrap();
    let body = json!({ "rev": card["entity"]["rev"], "facts": { "overlay.ipv4": "100.64.9.9" } });
    let (code, resp) = call(
        patch(
            &app,
            &kpath(&t.tenant_id, &format!("/entity/{id}")),
            &t.admin.access_token,
        )
        .json(&body),
    )
    .await;
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["message"].as_str().unwrap().contains("overlay.ipv4"),
        "{resp}"
    );
}

/// A reconnect's hello carries what the device's known node says, with no
/// reconcile; an evicted node's facts leave the machine, and the rest of it
/// stays.
#[tokio::test]
async fn a_known_nodes_word_comes_with_a_hello_and_goes_with_an_eviction() {
    let app = knowhow_app().await;
    let t = app.seed_tenant("knownetevict").await;
    let (agent_id, token) = enroll(&app, &t, "mach-knownet-b", "linux").await;
    let mut ws = hello(&app, &token, &[]).await;
    wait_machine(&app, &t, &agent_id, |_| true).await;
    let ip = join(&mut ws, 9, &[]).await;
    drop(ws);

    // Its next hello reads the node it already holds.
    let _ws = hello(&app, &token, &[]).await;
    let card = wait_machine(&app, &t, &agent_id, |c| {
        fact(c, "overlay.ipv4")["value"] == json!(ip)
    })
    .await;
    assert_eq!(fact(&card, "overlay.exit")["value"], json!(false), "{card}");
    assert_eq!(fact(&card, "overlay.routes")["value"], json!([]), "{card}");
    assert_eq!(
        fact(&card, "ssh.server")["value"],
        json!(false),
        "a build without the verb: {card}"
    );

    // An admin evicts the node: the device stays, its lease goes.
    let node = node_id(&app, &t, &ip).await;
    let (code, resp) = call(app.auth_delete(
        &format!("/api/tenant/{}/overlay-node/{node}", t.tenant_id),
        &t.admin.access_token,
    ))
    .await;
    assert!((200..300).contains(&code), "{code} {resp}");
    reconcile(&app).await;
    let card = machine(&app, &t, &agent_id)
        .await
        .expect("the device is still a machine");
    for path in OVERLAY {
        assert_eq!(
            fact(&card, path),
            Value::Null,
            "{path} after the eviction: {card}"
        );
    }
    assert_eq!(fact(&card, "os")["value"], json!("linux"), "{card}");
    assert_eq!(fact(&card, "ssh.policy")["value"], json!("off"), "{card}");
}

/// A machine keeps room for every fact the sync owns: a person's own facts
/// stop short of it; an edit that adds none passes, even on a machine past
/// its room; and the network's word still fits on a full machine.
#[tokio::test]
async fn a_machine_keeps_room_for_the_syncs_facts() {
    use roomler_ai_mod_knowhow::{
        intake::SCANNED_MACHINE_FACTS, model::MAX_FACTS, sync::SYNCED_FACTS,
    };

    let app = knowhow_app().await;
    let t = app.seed_tenant("knownetroom").await;
    let (agent_id, token) = enroll(&app, &t, "mach-knownet-c", "linux").await;
    let mut ws = hello(&app, &token, &["ssh"]).await;
    let card = wait_machine(&app, &t, &agent_id, |_| true).await;
    let id = card["entity"]["id"].as_str().unwrap().to_string();
    let path = kpath(&t.tenant_id, &format!("/entity/{id}"));
    let owner = &t.admin.access_token;
    let rev = || async {
        machine(&app, &t, &agent_id).await.unwrap()["entity"]["rev"]
            .as_i64()
            .unwrap()
    };
    // P4-3a-2 — and its scans' (`tunnel.declared`).
    let room = MAX_FACTS - SYNCED_FACTS.len() - SCANNED_MACHINE_FACTS.len();

    let mine: Map<String, Value> = (0..room).map(|i| (format!("note{i}"), json!(i))).collect();
    let body = json!({ "rev": rev().await, "facts": mine });
    let (code, resp) = call(patch(&app, &path, owner).json(&body)).await;
    assert_eq!(code, 200, "a person's facts up to the room: {resp}");
    let body = json!({ "rev": rev().await, "facts": { "one-more": 1 } });
    let (code, resp) = call(patch(&app, &path, owner).json(&body)).await;
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["message"]
            .as_str()
            .unwrap()
            .contains(&format!("at most {room} facts of a person's")),
        "{resp}"
    );
    // An edit that adds none passes, and so does one that trades a fact.
    let body = json!({ "rev": rev().await, "facts": { "note0": "changed" } });
    let (code, resp) = call(patch(&app, &path, owner).json(&body)).await;
    assert_eq!(code, 200, "{resp}");
    let body = json!({ "rev": rev().await, "facts": { "note1": null, "fresh": 1 } });
    let (code, resp) = call(patch(&app, &path, owner).json(&body)).await;
    assert_eq!(code, 200, "{resp}");

    // The network's word arrives on the full machine, and it still fits.
    let ip = join(&mut ws, 11, &["10.30.0.0/24"]).await;
    reconcile(&app).await;
    let card = machine(&app, &t, &agent_id).await.unwrap();
    assert_eq!(fact(&card, "overlay.ipv4")["value"], json!(ip), "{card}");
    let held = card["entity"]["facts"].as_array().unwrap().len();
    assert!(held <= MAX_FACTS, "{held} facts on a machine: {card}");

    // A machine past its room (facts a person held before their paths were
    // counted) is still tidied by an edit that adds none, and grown by none.
    let entity = ObjectId::parse_str(&id).unwrap();
    let now = bson::DateTime::now();
    let admin = ObjectId::parse_str(&t.admin.id).unwrap();
    app.db
        .collection::<bson::Document>("knowhow_entities")
        .update_one(
            doc! { "_id": entity },
            doc! { "$push": { "facts": {
                "path": "older", "value": 1, "source": "person", "by": admin,
                "observed_at": now, "status": "active",
            } } },
        )
        .await
        .unwrap();
    let body = json!({ "rev": rev().await, "facts": { "note2": "changed" } });
    let (code, resp) = call(patch(&app, &path, owner).json(&body)).await;
    assert_eq!(code, 200, "past its room, an edit that adds none: {resp}");
    let body = json!({ "rev": rev().await, "facts": { "newer": 1 } });
    let (code, resp) = call(patch(&app, &path, owner).json(&body)).await;
    assert_eq!(code, 400, "past its room, one that adds: {resp}");
}

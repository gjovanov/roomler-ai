// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-6-1a — a session's connect (decisions 59–61): how an entity of
//! the map is reached, for the session's own device.
//!
//! What these lock: the map offers `roomler_connect` only to a device with a
//! vault client. A connect is answered with the engine, the upstream and the
//! credential handle the entity's `active` and `stale` facts say, and with
//! the paths the devices' findings measured: this device's own fresh `ok`
//! one first, then each other device's. A stale or an unreachable finding is
//! listed beside them, never as a path, and a probe that measured nothing is
//! no finding. A credential that is only proposed is read as absent, and
//! named. A name not on the map is `not_found`, a bare frame `bad_request`,
//! and a starter who may not read the map connects nowhere.

use std::time::Duration;

use bson::{Document, doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::knowhow_scan_tests::recv_t;
use crate::knowhow_session_tests::{ask, refused, session_app};
use crate::knowhow_tests::{create, hold_only};
use crate::vault_lease_tests::{Device, device, start};

/// A device whose sessions ask the map, which probes, and which holds a
/// vault client.
const CONNECTS: &[&str] = &["exec", "hive", "knowhow-tools", "knowhow-probe", "vault"];

/// An entity the owner puts on the map with `facts`; its id.
async fn entity(app: &TestApp, t: &SeededTenant, kind: &str, key: &str, facts: Value) -> String {
    let (code, body) = create(
        app,
        t,
        &t.admin.access_token,
        json!({ "kind": kind, "key": key, "name": key, "facts": facts }),
    )
    .await;
    assert_eq!(code, 201, "{body}");
    body["id"].as_str().unwrap().to_string()
}

/// Turn one of an entity's facts into a session's proposal, as P4-4-2 writes
/// one.
async fn propose(app: &TestApp, entity: &str, path: &str) {
    let id = ObjectId::parse_str(entity).unwrap();
    let r = app
        .db
        .collection::<Document>("knowhow_entities")
        .update_one(
            doc! { "_id": id, "facts.path": path },
            doc! { "$set": { "facts.$.status": "proposed", "facts.$.source": "session",
            "facts.$.session": ObjectId::new() } },
        )
        .await
        .unwrap();
    assert_eq!(r.modified_count, 1, "{path} was there to propose");
}

/// A device that connects, told the map is there with `roomler_connect`; a
/// session it runs, started.
async fn connecting(app: &TestApp, t: &SeededTenant, machine: &str) -> (Device, String, u64) {
    let mut dev = device(app, t, machine, "linux", CONNECTS, true).await;
    let served = recv_t(&mut dev.ws, "rc:knowhow.served", Duration::from_secs(10))
        .await
        .expect("a device whose sessions ask is told the map is there");
    assert_eq!(
        served["tools"],
        json!([
            "knowhow_search",
            "knowhow_get",
            "knowhow_propose",
            "knowhow_verify",
            "roomler_connect"
        ]),
        "a device with a vault client is offered the connect"
    );
    let (sid, fence) = start(app, t, &mut dev, "/srv/api", None).await;
    (dev, sid, fence)
}

/// A finding of `device` of the entity, `secs` ago: a probe that ended.
fn finding(
    tid: ObjectId,
    entity: ObjectId,
    device: ObjectId,
    kind: &str,
    state: &str,
    secs: i64,
) -> Document {
    let at = bson::DateTime::from_millis(bson::DateTime::now().timestamp_millis() - secs * 1000);
    doc! {
        "tenant_id": tid, "entity_id": entity, "device_id": device, "requested_at": at,
        "kind": kind, "state": state, "answered_at": at, "rtt_ms": 4_i32, "carrier": "direct",
        "target": { "host": "db.example.com", "host_path": "host",
                    "host_value": "db.example.com", "port": 5432_i32, "port_value": 5432_i64 },
    }
}

fn connect(kind: &str, key: &str) -> Value {
    json!({ "kind": kind, "key": key })
}

/// A connect is told the engine, the upstream as `vault_connect` spells it,
/// the credential handle and the options, and the paths: each device's last
/// finding when it is `ok` and fresh, this device's first. A device whose
/// last word is unreachable or stale is listed beside them, its older `ok`
/// with it, and a `not_allowed` probe is no finding at all.
#[tokio::test]
async fn a_session_is_told_how_an_entity_is_reached_and_by_which_paths() {
    let app = session_app().await;
    let t = app.seed_tenant("knowconnect").await;
    let id = entity(
        &app,
        &t,
        "database",
        "prod-pg",
        json!({ "engine": "postgres", "host": "db.example.com", "port": 5432,
                "credential": "secret://knowconnect/db/prod", "database": "app" }),
    )
    .await;
    let (mut dev, sid, fence) = connecting(&app, &t, "connect-box").await;
    let tid = ObjectId::parse_str(&t.tenant_id).unwrap();
    let eid = ObjectId::parse_str(&id).unwrap();
    let here = ObjectId::parse_str(&dev.agent_id).unwrap();
    let (w, x, y, z) = (
        ObjectId::new(),
        ObjectId::new(),
        ObjectId::new(),
        ObjectId::new(),
    );
    app.db
        .collection::<Document>("knowhow_probes")
        .insert_many(vec![
            finding(tid, eid, here, "tls", "ok", 60),
            finding(tid, eid, x, "tcp", "ok", 30),
            finding(tid, eid, y, "tcp", "unreachable", 10),
            finding(tid, eid, z, "tls", "ok", 3 * 86_400),
            finding(tid, eid, x, "tls", "not_allowed", 5),
            // w's last word is unreachable, whatever it found before.
            finding(tid, eid, w, "tcp", "unreachable", 20),
            finding(tid, eid, w, "tls", "ok", 40),
        ])
        .await
        .unwrap();

    let answer = ask(
        &mut dev.ws,
        "c1",
        "rc:knowhow.connect",
        &sid,
        fence,
        connect("database", "prod-pg"),
    )
    .await;
    assert_eq!(answer["t"], "rc:knowhow.access", "{answer}");
    let a = &answer["access"];
    assert_eq!(
        (
            a["engine"].clone(),
            a["upstream"].clone(),
            a["credential"].clone()
        ),
        (
            json!("postgres"),
            json!("db.example.com:5432"),
            json!("secret://knowconnect/db/prod")
        ),
        "{a}"
    );
    assert_eq!(a["options"], json!({ "database": "app" }));
    assert_eq!(a["missing"], json!([]));
    assert_eq!(a["here"], json!("verified"));
    let paths: Vec<String> = a["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["device_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        paths,
        [here.to_hex(), x.to_hex()],
        "this device's first: {a}"
    );
    assert_eq!(a["paths"][0]["here"], json!(true));
    assert_eq!(a["paths"][1]["kind"], json!("tcp"));
    let seen: Vec<(String, Value, Value)> = a["seen"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["device_id"].as_str().unwrap().to_string(),
                p["state"].clone(),
                p["fresh"].clone(),
            )
        })
        .collect();
    let row = |d: ObjectId, state: &str, fresh: bool| (d.to_hex(), json!(state), json!(fresh));
    assert_eq!(
        seen,
        [
            row(y, "unreachable", true),
            row(w, "unreachable", true),
            row(w, "ok", true),
            row(z, "ok", false),
        ],
        "unreachable and stale beside the paths, never one: {a}"
    );
}

/// A credential that is only a session's proposal places nothing: it is read
/// as absent, and named. What an entity lacks is named. A name not on the
/// map is `not_found`, and a bare frame `bad_request`.
#[tokio::test]
async fn a_proposal_places_no_credential_and_what_is_missing_is_named() {
    let app = session_app().await;
    let t = app.seed_tenant("knowconnectno").await;
    let id = entity(
        &app,
        &t,
        "database",
        "prod-pg",
        json!({ "engine": "postgres", "host": "db.example.com", "port": 5432,
                "credential": "secret://knowconnectno/db/prod" }),
    )
    .await;
    propose(&app, &id, "credential").await;
    entity(&app, &t, "endpoint", "bare", json!({ "version": "16" })).await;
    let (mut dev, sid, fence) = connecting(&app, &t, "connect-no-box").await;

    let a = ask(
        &mut dev.ws,
        "n1",
        "rc:knowhow.connect",
        &sid,
        fence,
        connect("database", "prod-pg"),
    )
    .await;
    assert_eq!(a["t"], "rc:knowhow.access", "{a}");
    assert_eq!(a["access"]["credential"], Value::Null, "{a}");
    assert_eq!(a["access"]["missing"], json!(["credential"]));
    assert_eq!(a["access"]["proposed"], json!(["credential"]));
    assert_eq!(a["access"]["here"], json!("unmeasured"));
    assert_eq!(a["access"]["paths"], json!([]));

    let bare = ask(
        &mut dev.ws,
        "n2",
        "rc:knowhow.connect",
        &sid,
        fence,
        connect("endpoint", "bare"),
    )
    .await;
    assert_eq!(
        bare["access"]["missing"],
        json!(["engine", "host", "port"]),
        "{bare}"
    );
    let nowhere = ask(
        &mut dev.ws,
        "n3",
        "rc:knowhow.connect",
        &sid,
        fence,
        connect("database", "nowhere"),
    )
    .await;
    assert_eq!(refused(&nowhere), "not_found");
    let empty = ask(
        &mut dev.ws,
        "n4",
        "rc:knowhow.connect",
        &sid,
        fence,
        json!({}),
    )
    .await;
    assert_eq!(refused(&empty), "bad_request");
}

/// `roomler_connect` leases its credential through the device's vault
/// client, so a device without one is not offered it.
#[tokio::test]
async fn connect_is_offered_only_to_a_device_with_a_vault_client() {
    let app = session_app().await;
    let t = app.seed_tenant("knowconnectvault").await;
    let mut dev = device(
        &app,
        &t,
        "no-vault-box",
        "linux",
        &["exec", "hive", "knowhow-tools", "knowhow-probe"],
        false,
    )
    .await;
    let served = recv_t(&mut dev.ws, "rc:knowhow.served", Duration::from_secs(10))
        .await
        .expect("told the map is there");
    assert_eq!(
        served["tools"],
        json!([
            "knowhow_search",
            "knowhow_get",
            "knowhow_propose",
            "knowhow_verify"
        ])
    );
}

/// Decision 61: a starter who may not read the map has sessions that connect
/// nowhere through it, read at every connect.
#[tokio::test]
async fn a_starter_who_may_not_read_the_map_connects_nowhere() {
    let app = session_app().await;
    let t = app.seed_tenant("knowconnectstarter").await;
    entity(
        &app,
        &t,
        "database",
        "prod-pg",
        json!({ "engine": "postgres", "host": "db.example.com", "port": 5432,
                "credential": "secret://knowconnectstarter/db/prod" }),
    )
    .await;
    let (mut dev, sid, fence) = connecting(&app, &t, "connect-starter-box").await;
    let first = ask(
        &mut dev.ws,
        "s1",
        "rc:knowhow.connect",
        &sid,
        fence,
        connect("database", "prod-pg"),
    )
    .await;
    assert_eq!(first["t"], "rc:knowhow.access", "{first}");
    hold_only(&app, &t, &t.admin.id, "guest").await;
    let denied = ask(
        &mut dev.ws,
        "s2",
        "rc:knowhow.connect",
        &sid,
        fence,
        connect("database", "prod-pg"),
    )
    .await;
    assert_eq!(refused(&denied), "denied");
}

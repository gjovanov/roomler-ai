// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-5-1a — a probe, asked, delivered, answered, and recorded on the
//! facts it measured.
//!
//! What these lock: a person with `KNOWHOW_EDIT` asks a device to probe an
//! entity, and the device is sent the target the entity's own facts say,
//! never one the request names; its answer is recorded once, and what it
//! measured lands on those facts: `verified` with `ok`, a proposed one turned
//! `active`, a different certificate `ok: false` with its status kept, an
//! unreachable target `ok: false`, and `not_allowed` nothing at all; the
//! carrier says `overlay` for an address in the organization's mesh; a device
//! that cannot probe, or is a secondary enrollment, is refused 409, an entity
//! that says nowhere a probe reaches 400, a member 403; one probe at a time a
//! device, 30 a minute; an offline device is told at its hello; an answer for
//! a probe it was not sent is dropped; and one never answered expires.

use std::time::Duration;

use bson::{Document, doc, oid::ObjectId};
use futures::SinkExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::knowhow_scan_tests::{Ws, hello, recv_t, scan_app};
use crate::knowhow_tests::{call, create, kpath};
use crate::vault_lease_tests::enroll;

/// A SHA-256 fingerprint's shape, built here, as `openssl x509` prints one.
fn printed(byte: &str) -> String {
    vec![byte; 32].join(":")
}

/// An endpoint the owner puts on the map with `facts`; its id.
async fn endpoint(app: &TestApp, t: &SeededTenant, key: &str, facts: Value) -> String {
    let (code, body) = create(
        app,
        t,
        &t.admin.access_token,
        json!({ "kind": "endpoint", "key": key, "name": key, "facts": facts }),
    )
    .await;
    assert_eq!(code, 201, "{body}");
    body["id"].as_str().unwrap().to_string()
}

/// Ask `device` to probe `entity`, as `token`.
async fn ask(
    app: &TestApp,
    t: &SeededTenant,
    token: &str,
    entity: &str,
    device: &str,
    kind: &str,
) -> (u16, Value) {
    call(
        app.auth_post(&kpath(&t.tenant_id, "/probe"), token)
            .json(&json!({ "entity": entity, "device": device, "kind": kind })),
    )
    .await
}

/// The probe the device is sent next.
async fn probe_sent(ws: &mut Ws) -> Value {
    recv_t(ws, "rc:knowhow.probe", Duration::from_secs(10))
        .await
        .expect("the device is sent its probe")
}

async fn answer(ws: &mut Ws, body: Value) {
    let mut frame = body;
    frame["t"] = json!("rc:knowhow.probed");
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .expect("send the answer");
}

/// An entity's probes, newest first.
async fn probes(app: &TestApp, t: &SeededTenant, entity: &str) -> Vec<Value> {
    let (code, body) = call(app.auth_get(
        &kpath(&t.tenant_id, &format!("/probe?entity={entity}")),
        &t.admin.access_token,
    ))
    .await;
    assert_eq!(code, 200, "{body}");
    body["probes"].as_array().unwrap().clone()
}

/// Wait until the entity's newest probe satisfies `ready`.
async fn wait_probe(
    app: &TestApp,
    t: &SeededTenant,
    entity: &str,
    ready: impl Fn(&Value) -> bool,
) -> Value {
    for _ in 0..100 {
        if let Some(p) = probes(app, t, entity).await.into_iter().next()
            && ready(&p)
        {
            return p;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "the probe of {entity} never got there: {:?}",
        probes(app, t, entity).await
    );
}

/// An entity's facts, by path, as its card shows them.
async fn facts(app: &TestApp, t: &SeededTenant, entity: &str) -> Value {
    let (code, card) = call(app.auth_get(
        &kpath(&t.tenant_id, &format!("/entity/{entity}")),
        &t.admin.access_token,
    ))
    .await;
    assert_eq!(code, 200, "{card}");
    let mut by_path = serde_json::Map::new();
    for f in card["entity"]["facts"].as_array().unwrap() {
        by_path.insert(f["path"].as_str().unwrap().to_string(), f.clone());
    }
    Value::Object(by_path)
}

/// Turn one of an entity's facts into a proposal, as a session's is
/// (P4-4-2), so a probe can be seen to confirm it.
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

fn coll(app: &TestApp) -> mongodb::Collection<Document> {
    app.db.collection::<Document>("knowhow_probes")
}

/// A person asks, the device is sent the target the entity's facts say, and
/// what its answer measured is recorded on them: each `verified` with `ok`,
/// the proposed port turned `active`, a `verify` row in the history naming
/// the probe; and the answer is taken once.
#[tokio::test]
async fn a_probe_reaches_the_device_and_confirms_what_it_measured() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowprobeok").await;
    let (device, token) = enroll(&app, &t, "mach-knowprobe-a", "linux").await;
    let mut ws = hello(&app, &token, &["exec", "knowhow-probe"], "self").await;
    let db = endpoint(
        &app,
        &t,
        "db.example.com:5432",
        json!({ "host": "DB.Example.com", "port": 5432,
                "tls.fingerprint": printed("AB"), "tls.names": ["db.example.com"],
                "version": "16" }),
    )
    .await;
    propose(&app, &db, "port").await;

    let (code, asked) = ask(&app, &t, &t.admin.access_token, &db, &device, "tls").await;
    assert_eq!(code, 202, "{asked}");
    assert_eq!(asked["requested_by"], json!(t.admin.id));
    assert_eq!(
        asked["target"],
        json!({ "host": "db.example.com", "from": "host", "port": 5432, "sni": "db.example.com" })
    );
    let id = asked["id"].as_str().unwrap().to_string();
    let frame = probe_sent(&mut ws).await;
    assert_eq!(
        frame,
        json!({ "t": "rc:knowhow.probe", "probe_id": id, "kind": "tls",
                "host": "db.example.com", "port": 5432, "sni": "db.example.com" }),
        "the target as the server read it, and nothing else"
    );

    answer(
        &mut ws,
        json!({ "probe_id": id, "outcome": "ok", "rtt_ms": 12, "address": "10.0.4.12",
                "tls": { "names": ["db.example.com", "*.example.com"],
                         "fingerprint": "ab".repeat(32) } }),
    )
    .await;
    let done = wait_probe(&app, &t, &db, |p| !p["measured"].is_null()).await;
    assert_eq!(done["state"], json!("ok"), "{done}");
    assert_eq!(
        (
            done["rtt_ms"].clone(),
            done["address"].clone(),
            done["carrier"].clone()
        ),
        (json!(12), json!("10.0.4.12"), json!("direct")),
        "{done}"
    );
    assert_eq!(done["tls"]["fingerprint"], json!("ab".repeat(32)));
    assert_eq!(
        done["measured"],
        json!([{ "path": "host", "ok": true }, { "path": "port", "ok": true },
               { "path": "tls.fingerprint", "ok": true }, { "path": "tls.names", "ok": true }])
    );
    let f = facts(&app, &t, &db).await;
    for path in ["host", "port", "tls.fingerprint", "tls.names"] {
        let v = &f[path]["verified"];
        assert_eq!(
            (
                v["ok"].clone(),
                v["probe"].clone(),
                v["from_device"].clone(),
                v["rtt_ms"].clone()
            ),
            (json!(true), json!(id), json!(device), json!(12)),
            "{path}: {}",
            f[path]
        );
        assert!(f[path].get("verified_at").is_some(), "{path}");
    }
    assert_eq!(
        f["port"]["status"],
        json!("active"),
        "the proposal it found as it says"
    );
    assert!(
        f["port"].get("confirmed_by").is_none(),
        "a person's word stays a person's"
    );
    assert!(f["version"].get("verified").is_none(), "never measured");

    let (code, history) = call(app.auth_get(
        &kpath(&t.tenant_id, &format!("/history?entity={db}")),
        &t.admin.access_token,
    ))
    .await;
    assert_eq!(code, 200, "{history}");
    let row = &history["history"][0];
    assert_eq!(
        (
            row["op"].clone(),
            row["source"].clone(),
            row["device"].clone()
        ),
        (json!("verify"), json!("probe"), json!(device)),
        "{row}"
    );
    assert_eq!(row["set"]["probe"], json!(id), "{row}");

    // Answered once: another answer changes nothing.
    answer(
        &mut ws,
        json!({ "probe_id": id, "outcome": "unreachable", "detail": "late" }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = probes(&app, &t, &db).await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0]["state"], json!("ok"), "{after:?}");
    assert_eq!(
        facts(&app, &t, &db).await["port"]["verified"]["ok"],
        json!(true)
    );
}

/// A different certificate is `ok: false` on the facts it contradicts, their
/// status kept; an unreachable target `ok: false` on its host and port; and
/// `not_allowed` records nothing on a fact: "the device may not reach it" is
/// not "it did not answer". An address in the organization's mesh is the
/// `overlay` carrier.
#[tokio::test]
async fn a_failed_or_different_probe_changes_nothing_but_its_record() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowprobeno").await;
    let (device, token) = enroll(&app, &t, "mach-knowprobe-b", "linux").await;
    let mut ws = hello(&app, &token, &["knowhow-probe"], "self").await;
    let tid = ObjectId::parse_str(&t.tenant_id).unwrap();
    let now = bson::DateTime::now();
    app.db
        .collection::<Document>("overlay_networks")
        .insert_one(doc! {
            "tenant_id": tid, "cidr": "100.64.0.0/10", "next_host": 2_i64, "free_hosts": [],
            "mtu": 1280_i32, "created_at": now, "updated_at": now,
        })
        .await
        .unwrap();
    let api = endpoint(
        &app,
        &t,
        "api.example.com:443",
        json!({ "address": "100.64.0.9", "port": 443, "tls.fingerprint": printed("AB") }),
    )
    .await;
    propose(&app, &api, "tls.fingerprint").await;

    // A different certificate.
    let (code, asked) = ask(&app, &t, &t.admin.access_token, &api, &device, "tls").await;
    assert_eq!(code, 202, "{asked}");
    assert!(
        asked["target"].get("sni").is_none(),
        "an address names no server: {asked}"
    );
    let id = probe_sent(&mut ws).await["probe_id"].clone();
    answer(
        &mut ws,
        json!({ "probe_id": id, "outcome": "ok", "rtt_ms": 3, "address": "100.64.0.9",
                "tls": { "names": ["other.example.com"], "fingerprint": "cd".repeat(32) } }),
    )
    .await;
    let p = wait_probe(&app, &t, &api, |p| !p["measured"].is_null()).await;
    assert_eq!(p["carrier"], json!("overlay"), "{p}");
    let f = facts(&app, &t, &api).await;
    assert_eq!(f["tls.fingerprint"]["verified"]["ok"], json!(false));
    assert_eq!(
        f["tls.fingerprint"]["status"],
        json!("proposed"),
        "a different one changes nothing else"
    );
    assert!(f["tls.fingerprint"].get("verified_at").is_none());
    assert_eq!(f["address"]["verified"]["carrier"], json!("overlay"));

    // Not allowed: nothing on a fact.
    let (code, asked) = ask(&app, &t, &t.admin.access_token, &api, &device, "tcp").await;
    assert_eq!(code, 202, "{asked}");
    let id = probe_sent(&mut ws).await["probe_id"].clone();
    answer(&mut ws, json!({ "probe_id": id, "outcome": "not_allowed" })).await;
    let p = wait_probe(&app, &t, &api, |p| p["state"] == json!("not_allowed")).await;
    assert!(p.get("measured").is_none(), "{p}");
    let before = f;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        facts(&app, &t, &api).await,
        before,
        "the device may not reach it: said of no fact"
    );

    // Unreachable: the host and port, `ok: false`.
    let (code, asked) = ask(&app, &t, &t.admin.access_token, &api, &device, "tcp").await;
    assert_eq!(code, 202, "{asked}");
    let id = probe_sent(&mut ws).await["probe_id"].clone();
    answer(
        &mut ws,
        json!({ "probe_id": id, "outcome": "unreachable", "detail": "connection refused" }),
    )
    .await;
    let p = wait_probe(&app, &t, &api, |p| {
        p["state"] == json!("unreachable") && !p["measured"].is_null()
    })
    .await;
    assert_eq!(
        p["measured"],
        json!([{ "path": "address", "ok": false }, { "path": "port", "ok": false }])
    );
    assert_eq!(p["detail"], json!("connection refused"));
    let f = facts(&app, &t, &api).await;
    assert_eq!(f["port"]["verified"]["ok"], json!(false));
    assert_eq!(
        f["port"]["status"],
        json!("active"),
        "a person's fact stays as it was"
    );
}

/// A device that cannot probe, or is a secondary enrollment, is refused 409;
/// a device or an entity that is not there 404; a kind not known, or an
/// entity that says nowhere a probe reaches, 400; a member 403. No refusal
/// leaves a probe.
#[tokio::test]
async fn who_is_asked_who_may_ask_and_where_a_probe_may_go() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowprobewho").await;
    let (old, old_token) = enroll(&app, &t, "mach-knowprobe-c", "linux").await;
    let (second, second_token) = enroll(&app, &t, "mach-knowprobe-d", "linux").await;
    let (ok, ok_token) = enroll(&app, &t, "mach-knowprobe-e", "linux").await;
    let _w1 = hello(&app, &old_token, &["knowhow-scan", "knowhow-tools"], "self").await;
    let _w2 = hello(&app, &second_token, &["knowhow-probe"], "primary_org").await;
    let _w3 = hello(&app, &ok_token, &["knowhow-probe"], "self").await;
    let db = endpoint(
        &app,
        &t,
        "db",
        json!({ "address": "10.0.4.12", "port": 5432 }),
    )
    .await;
    let admin = t.admin.access_token.clone();

    let (code, body) = ask(&app, &t, &admin, &db, &old, "tcp").await;
    assert_eq!(code, 409, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("cannot probe"),
        "{body}"
    );
    let (code, body) = ask(&app, &t, &admin, &db, &second, "tcp").await;
    assert_eq!(code, 409, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("primary organization"),
        "{body}"
    );
    let (code, body) = ask(&app, &t, &admin, &db, &ObjectId::new().to_hex(), "tcp").await;
    assert_eq!(code, 404, "{body}");
    let (code, body) = ask(&app, &t, &admin, &ObjectId::new().to_hex(), &ok, "tcp").await;
    assert_eq!(code, 404, "{body}");
    let (code, body) = ask(&app, &t, &admin, &db, &ok, "http").await;
    assert_eq!(code, 400, "{body}");
    let (code, body) = ask(&app, &t, &t.member.access_token, &db, &ok, "tcp").await;
    assert_eq!(code, 403, "a member reads the map but does not ask: {body}");

    for (key, facts, says) in [
        ("bare", json!({ "version": "16" }), "nowhere to probe"),
        (
            "imds",
            json!({ "address": "169.254.169.254", "port": 80 }),
            "never probed",
        ),
        (
            "bound",
            json!({ "address": "0.0.0.0", "port": 5432 }),
            "where a listener binds",
        ),
        (
            "url",
            json!({ "host": "https://db.example.com", "port": 443 }),
            "not an IP address",
        ),
        ("portless", json!({ "address": "10.0.0.1" }), "no port"),
        (
            "home",
            json!({ "address": "127.0.0.1", "port": 5432 }),
            "the device itself",
        ),
    ] {
        let e = endpoint(&app, &t, key, facts).await;
        let (code, body) = ask(&app, &t, &admin, &e, &ok, "tcp").await;
        assert_eq!(code, 400, "{key}: {body}");
        assert!(
            body["message"].as_str().unwrap().contains(says),
            "{key}: {body}"
        );
    }
    assert_eq!(
        coll(&app).count_documents(doc! {}).await.unwrap(),
        0,
        "no refusal left a probe"
    );
}

/// One probe at a time a device: the same one asked again answers the
/// pending one, another is 409; and past 30 a minute the device is 429.
#[tokio::test]
async fn one_probe_at_a_time_and_thirty_a_minute() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowprobeonce").await;
    let (device, token) = enroll(&app, &t, "mach-knowprobe-f", "linux").await;
    let ws = hello(&app, &token, &["knowhow-probe"], "self").await;
    drop(ws);
    let db = endpoint(
        &app,
        &t,
        "db",
        json!({ "address": "10.0.4.12", "port": 5432 }),
    )
    .await;
    let cache = endpoint(
        &app,
        &t,
        "cache",
        json!({ "address": "10.0.4.13", "port": 6379 }),
    )
    .await;
    let admin = t.admin.access_token.clone();

    let (c1, first) = ask(&app, &t, &admin, &db, &device, "tcp").await;
    let (c2, again) = ask(&app, &t, &admin, &db, &device, "tcp").await;
    assert_eq!((c1, c2), (202, 202), "{first} {again}");
    assert_eq!(first["id"], again["id"]);
    let (code, body) = ask(&app, &t, &admin, &cache, &device, "tcp").await;
    assert_eq!(code, 409, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("another probe"),
        "{body}"
    );
    let (code, body) = ask(&app, &t, &admin, &db, &device, "tls").await;
    assert_eq!(code, 409, "another kind is another probe: {body}");

    // Four asked; the rest of the minute's thirty, then one more.
    for _ in 4..30 {
        let (code, body) = ask(&app, &t, &admin, &db, &device, "tcp").await;
        assert_eq!(code, 202, "{body}");
    }
    let (code, body) = ask(&app, &t, &admin, &db, &device, "tcp").await;
    assert_eq!(code, 429, "{body}");
    assert_eq!(body["error"], json!("rate_limited"));
    assert_eq!(coll(&app).count_documents(doc! {}).await.unwrap(), 1);
}

/// An offline device is told at its next hello, and again after a
/// connection that died unanswered; an answer for a probe it was not sent is
/// dropped; a probe asked on another pod goes out at this pod's sweep; and
/// one never answered expires, freeing the device for the next.
#[tokio::test]
async fn a_probe_waits_for_its_device_and_expires_unanswered() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowprobewait").await;
    let (device, token) = enroll(&app, &t, "mach-knowprobe-g", "linux").await;
    let (_other, other_token) = enroll(&app, &t, "mach-knowprobe-h", "linux").await;
    let ws = hello(&app, &token, &["knowhow-probe"], "self").await;
    drop(ws);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let db = endpoint(
        &app,
        &t,
        "db",
        json!({ "address": "10.0.4.12", "port": 5432 }),
    )
    .await;
    let admin = t.admin.access_token.clone();

    let (code, asked) = ask(&app, &t, &admin, &db, &device, "tcp").await;
    assert_eq!(
        (code, asked["state"].clone()),
        (202, json!("requested")),
        "{asked}"
    );
    let id = asked["id"].clone();
    let mut ws = hello(&app, &token, &["knowhow-probe"], "self").await;
    assert_eq!(
        probe_sent(&mut ws).await["probe_id"],
        id,
        "told at its hello"
    );
    drop(ws);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut ws = hello(&app, &token, &["knowhow-probe"], "self").await;
    assert_eq!(
        probe_sent(&mut ws).await["probe_id"],
        id,
        "and again: never answered"
    );

    let mut wo = hello(&app, &other_token, &["knowhow-probe"], "self").await;
    answer(
        &mut wo,
        json!({ "probe_id": id, "outcome": "ok", "rtt_ms": 1 }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        probes(&app, &t, &db).await[0]["state"],
        json!("delivered"),
        "another device's word is dropped"
    );
    answer(&mut ws, json!({ "probe_id": id, "outcome": "unreachable" })).await;
    wait_probe(&app, &t, &db, |p| p["state"] == json!("unreachable")).await;

    // Asked on another pod: a row this pod never saw requested.
    let tid = ObjectId::parse_str(&t.tenant_id).unwrap();
    let did = ObjectId::parse_str(&device).unwrap();
    let eid = ObjectId::parse_str(&db).unwrap();
    let row = |at: bson::DateTime| {
        doc! {
            "tenant_id": tid, "entity_id": eid, "device_id": did, "requested_at": at,
            "kind": "tcp", "state": "requested", "pending": true,
            "target": { "host": "10.0.4.12", "host_path": "address", "host_value": "10.0.4.12",
                        "port": 5432_i32, "port_value": 5432_i64 },
        }
    };
    let elsewhere = coll(&app)
        .insert_one(row(bson::DateTime::now()))
        .await
        .unwrap()
        .inserted_id;
    let frame = probe_sent(&mut ws).await;
    assert_eq!(
        frame["probe_id"],
        json!(elsewhere.as_object_id().unwrap().to_hex()),
        "the sweep sent it"
    );
    answer(
        &mut ws,
        json!({ "probe_id": frame["probe_id"], "outcome": "not_allowed" }),
    )
    .await;
    wait_probe(&app, &t, &db, |p| p["state"] == json!("not_allowed")).await;

    // Asked eleven minutes ago, and never answered.
    drop(ws);
    let old = bson::DateTime::from_millis(bson::DateTime::now().timestamp_millis() - 11 * 60_000);
    let stale = coll(&app).insert_one(row(old)).await.unwrap().inserted_id;
    let mut expired = false;
    for _ in 0..60 {
        let r = coll(&app)
            .find_one(doc! { "_id": &stale })
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
    assert!(expired, "the sweep expired the unanswered probe");
    let (code, next) = ask(&app, &t, &admin, &db, &device, "tcp").await;
    assert_eq!(code, 202, "{next}");
    assert_ne!(next["id"], json!(stale.as_object_id().unwrap().to_hex()));
}

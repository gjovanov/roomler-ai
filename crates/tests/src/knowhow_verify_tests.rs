// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-5-2a — a session's verify (decision 54): a session asks its own
//! device to probe an entity of the map, by its kind and key.
//!
//! What these lock: the map offers `knowhow_verify` only to a device that can
//! probe; a verify is answered at once with the probe it asked, which goes to
//! the session's own device and nowhere else, its target read from the
//! entity's facts as a person's probe's is; once the device answers, what it
//! measured comes back to the session as `rc:knowhow.verified`, the probe's
//! record naming the session and no person; a verify the map cannot ask is
//! refused in a word (`unprobeable`, `not_found`, `bad_request`); and a
//! session asks a probe only while its starter holds `KNOWHOW_EDIT`, as a
//! person's probe needs (`denied`). P4-5-4: the card a session reads then
//! shows what its device found.

use std::time::Duration;

use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::knowhow_scan_tests::{Ws, recv_t};
use crate::knowhow_session_tests::{ask, refused, session_app};
use crate::knowhow_tests::{create, hold_only};
use crate::vault_lease_tests::{Device, device, send, start};

/// A device whose sessions ask the map, and whose daemon probes.
const PROBES: &[&str] = &["exec", "hive", "knowhow-tools", "knowhow-probe"];

async fn endpoint(app: &TestApp, t: &SeededTenant, key: &str, facts: Value) {
    let (code, body) = create(
        app,
        t,
        &t.admin.access_token,
        json!({ "kind": "endpoint", "key": key, "name": key, "facts": facts }),
    )
    .await;
    assert_eq!(code, 201, "{body}");
}

/// A device that probes, told the map is there with `knowhow_verify`; a
/// session it runs, started.
async fn verifying(app: &TestApp, t: &SeededTenant, machine: &str) -> (Device, String, u64) {
    let mut dev = device(app, t, machine, "linux", PROBES, false).await;
    let served = recv_t(&mut dev.ws, "rc:knowhow.served", Duration::from_secs(10))
        .await
        .expect("a device whose sessions ask is told the map is there");
    assert_eq!(
        served["tools"],
        json!([
            "knowhow_search",
            "knowhow_get",
            "knowhow_propose",
            "knowhow_verify"
        ]),
        "a device that probes is offered the verify"
    );
    let (sid, fence) = start(app, t, &mut dev, "/srv/api", None).await;
    (dev, sid, fence)
}

/// Verify as the session `sid` at `fence`: the answer, `rc:knowhow.verifying`,
/// and the probe the device is sent. The probe goes out as soon as it is
/// asked, so it may come before the answer or after it: a device reads both.
async fn verify_asked(
    ws: &mut Ws,
    req_id: &str,
    sid: &str,
    fence: u64,
    rest: Value,
) -> (Value, Value) {
    let mut frame = rest;
    frame["t"] = json!("rc:knowhow.verify");
    frame["req_id"] = json!(req_id);
    frame["session_id"] = json!(sid);
    frame["fence"] = json!(fence);
    send(ws, frame).await;
    let (mut answer, mut probe) = (None, None);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while answer.is_none() || probe.is_none() {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let next = tokio::time::timeout(left, futures::StreamExt::next(ws))
            .await
            .expect("the verify's answer and its probe, within 10 s")
            .expect("agent socket closed")
            .expect("agent socket error");
        if let tokio_tungstenite::tungstenite::Message::Text(text) = next {
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            match v["t"].as_str().unwrap_or_default() {
                "rc:knowhow.verifying" | "rc:knowhow.refused" if v["req_id"] == req_id => {
                    assert_eq!(v["t"], "rc:knowhow.verifying", "{v}");
                    answer = Some(v);
                }
                "rc:knowhow.probe" => probe = Some(v),
                _ => {}
            }
        }
    }
    let (answer, probe) = (answer.unwrap(), probe.unwrap());
    assert_eq!(
        answer["probe_id"], probe["probe_id"],
        "the answer names the probe the device is sent"
    );
    (answer, probe)
}

/// A session verifies an entity: the probe goes to its own device with the
/// target the entity's facts say, and what it measured comes back to the
/// session, the record naming the session and no person.
#[tokio::test]
async fn a_session_verifies_an_entity_from_its_own_device() {
    let app = session_app().await;
    let t = app.seed_tenant("knowverify").await;
    endpoint(
        &app,
        &t,
        "db",
        json!({ "address": "10.0.4.12", "port": 5432 }),
    )
    .await;
    let (mut dev, sid, fence) = verifying(&app, &t, "verify-box").await;

    let (asked, frame) = verify_asked(
        &mut dev.ws,
        "v1",
        &sid,
        fence,
        json!({ "kind": "endpoint", "key": "db", "probe": "tcp" }),
    )
    .await;
    let probe_id = asked["probe_id"].as_str().unwrap().to_string();
    assert_eq!(
        frame,
        json!({ "t": "rc:knowhow.probe", "probe_id": probe_id, "kind": "tcp",
                "host": "10.0.4.12", "port": 5432 })
    );
    send(
        &mut dev.ws,
        json!({ "t": "rc:knowhow.probed", "probe_id": probe_id, "outcome": "ok",
                "rtt_ms": 4, "address": "10.0.4.12" }),
    )
    .await;
    let verified = recv_t(&mut dev.ws, "rc:knowhow.verified", Duration::from_secs(10))
        .await
        .expect("what the probe measured comes back to the session");
    assert_eq!(verified["probe_id"], json!(probe_id));
    let probe = &verified["probe"];
    assert_eq!(probe["state"], "ok", "{verified}");
    assert_eq!(probe["session"], json!(sid));
    assert!(
        probe.get("requested_by").is_none(),
        "no person asked: {probe}"
    );
    assert_eq!(
        probe["measured"],
        json!([{ "path": "address", "ok": true }, { "path": "port", "ok": true }])
    );

    // P4-5-4 — the card the session reads shows what its device found.
    let card = ask(
        &mut dev.ws,
        "g1",
        "rc:knowhow.get",
        &sid,
        fence,
        json!({ "kind": "endpoint", "key": "db" }),
    )
    .await;
    assert_eq!(card["t"], "rc:knowhow.card", "{card}");
    let rows = card["card"]["probes"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{card}");
    assert_eq!(
        (
            rows[0]["id"].clone(),
            rows[0]["device_id"].clone(),
            rows[0]["session"].clone()
        ),
        (json!(probe_id), json!(dev.agent_id), json!(sid))
    );
    assert_eq!(card["card"]["probes_truncated"], json!(false));

    // Once it has answered, the same verify asks a new probe: one at a time
    // and the ceiling are the device's, as for a person's probe.
    let again = ask(
        &mut dev.ws,
        "v2",
        "rc:knowhow.verify",
        &sid,
        fence,
        json!({ "kind": "endpoint", "key": "db", "probe": "tcp" }),
    )
    .await;
    assert_eq!(again["t"], "rc:knowhow.verifying", "{again}");
    assert_ne!(
        again["probe_id"],
        json!(probe_id),
        "the last one ended: a new probe"
    );
}

/// A verify the map cannot ask is refused in a word: an entity whose facts
/// say nowhere a probe reaches, a name not on the map, a probe kind not
/// known, a frame missing its name.
#[tokio::test]
async fn a_verify_is_refused_in_a_word() {
    let app = session_app().await;
    let t = app.seed_tenant("knowverifyno").await;
    endpoint(
        &app,
        &t,
        "imds",
        json!({ "address": "169.254.169.254", "port": 80 }),
    )
    .await;
    endpoint(&app, &t, "bare", json!({ "version": "16" })).await;
    let (mut dev, sid, fence) = verifying(&app, &t, "verify-no-box").await;

    for (req, rest, word) in [
        (
            "n1",
            json!({ "kind": "endpoint", "key": "imds", "probe": "tcp" }),
            "unprobeable",
        ),
        (
            "n2",
            json!({ "kind": "endpoint", "key": "bare", "probe": "tcp" }),
            "unprobeable",
        ),
        (
            "n3",
            json!({ "kind": "endpoint", "key": "nowhere", "probe": "tcp" }),
            "not_found",
        ),
        (
            "n4",
            json!({ "kind": "endpoint", "key": "imds", "probe": "http" }),
            "bad_request",
        ),
        ("n5", json!({ "probe": "tcp" }), "bad_request"),
    ] {
        let answer = ask(&mut dev.ws, req, "rc:knowhow.verify", &sid, fence, rest).await;
        assert_eq!(refused(&answer), word, "{req}: {answer}");
    }
    assert_eq!(probes(&app).await, 0, "no refusal left a probe");
}

/// A session asks only what its starter may ask: a starter who holds
/// `KNOWHOW_VIEW` but not `KNOWHOW_EDIT` reads the map from a session and
/// verifies nothing, read at every verify; one who holds it again verifies.
#[tokio::test]
async fn a_starter_who_may_not_probe_has_sessions_that_verify_nothing() {
    let app = session_app().await;
    let t = app.seed_tenant("knowverifyedit").await;
    endpoint(
        &app,
        &t,
        "db",
        json!({ "address": "10.0.4.12", "port": 5432 }),
    )
    .await;
    let (mut dev, sid, fence) = verifying(&app, &t, "verify-edit-box").await;
    let verify = json!({ "kind": "endpoint", "key": "db", "probe": "tcp" });

    hold_only(&app, &t, &t.admin.id, "member").await;
    let read = ask(
        &mut dev.ws,
        "e1",
        "rc:knowhow.get",
        &sid,
        fence,
        json!({ "kind": "endpoint", "key": "db" }),
    )
    .await;
    assert_eq!(read["t"], "rc:knowhow.card", "a member reads: {read}");
    let denied = ask(
        &mut dev.ws,
        "e2",
        "rc:knowhow.verify",
        &sid,
        fence,
        verify.clone(),
    )
    .await;
    assert_eq!(refused(&denied), "denied");
    assert_eq!(probes(&app).await, 0, "a denied verify left a probe");

    hold_only(&app, &t, &t.admin.id, "admin").await;
    let asked = ask(&mut dev.ws, "e3", "rc:knowhow.verify", &sid, fence, verify).await;
    assert_eq!(asked["t"], "rc:knowhow.verifying", "{asked}");
    assert_eq!(probes(&app).await, 1);
}

async fn probes(app: &TestApp) -> u64 {
    app.db
        .collection::<bson::Document>("knowhow_probes")
        .count_documents(bson::doc! {})
        .await
        .unwrap()
}

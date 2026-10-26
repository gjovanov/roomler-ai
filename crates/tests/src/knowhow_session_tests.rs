// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-4-1 — a session's questions about the map, the server's half.
//!
//! Driven through a RAW agent socket, as the vault's tests drive leases: the
//! device starts a session through the real start route, then asks what a
//! session's toolbelt will ask. What these lock is the SERVER:
//!
//! - a device advertising `knowhow-tools` is told the map is there at its
//!   hello (`rc:knowhow.served`, naming the tools it answers), and only where
//!   knowhow serves its organization, on its primary enrollment;
//! - `rc:knowhow.search` and `rc:knowhow.get` answer the people's search and
//!   card, bounded for a session;
//! - a question is believed only from the device the session's record places
//!   it on, at the record's fence, while the session is live;
//! - and only while its starter may read the map, read at every question.

use std::time::Duration;

use bson::{doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::fixtures::test_app::TestApp;
use crate::knowhow_scan_tests::{Ws, hello, recv_t};
use crate::knowhow_tests::{entity, hold_only, link};
use crate::vault_lease_tests::{Device, device, enroll, send, start};

/// What a build whose sessions ask the map says in its hello.
pub(crate) const ASKS: &[&str] = &["exec", "hive", "knowhow-tools"];

/// Hive and knowhow on.
pub(crate) async fn session_app() -> TestApp {
    TestApp::spawn_with_settings(|s| {
        s.modules.hive = true;
        s.modules.knowhow = true;
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await
}

/// Ask a question as the session `sid` at `fence`: the server's answer to it.
pub(crate) async fn ask(
    ws: &mut Ws,
    req_id: &str,
    t: &str,
    sid: &str,
    fence: u64,
    rest: Value,
) -> Value {
    let mut frame = rest;
    frame["t"] = json!(t);
    frame["req_id"] = json!(req_id);
    frame["session_id"] = json!(sid);
    frame["fence"] = json!(fence);
    send(ws, frame).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let next = tokio::time::timeout(left, futures::StreamExt::next(ws))
            .await
            .expect("no answer to the question within 10 s")
            .expect("agent socket closed")
            .expect("agent socket error");
        if let tokio_tungstenite::tungstenite::Message::Text(text) = next {
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let kind = v["t"].as_str().unwrap_or_default();
            if matches!(
                kind,
                "rc:knowhow.found"
                    | "rc:knowhow.card"
                    | "rc:knowhow.proposed"
                    | "rc:knowhow.verifying"
                    | "rc:knowhow.access"
                    | "rc:knowhow.refused"
            ) && v["req_id"] == req_id
            {
                return v;
            }
        }
    }
}

pub(crate) fn refused(answer: &Value) -> &str {
    assert_eq!(answer["t"], "rc:knowhow.refused", "{answer}");
    answer["reason"].as_str().unwrap()
}

/// A device that asks, told the map is there; a session it runs, started.
pub(crate) async fn asking(
    app: &TestApp,
    t: &crate::fixtures::seed::SeededTenant,
    machine: &str,
) -> (Device, String, u64) {
    let mut dev = device(app, t, machine, "linux", ASKS, false).await;
    let served = recv_t(&mut dev.ws, "rc:knowhow.served", Duration::from_secs(10))
        .await
        .expect("a device whose sessions ask is told the map is there");
    assert_eq!(
        served["tools"],
        json!(["knowhow_search", "knowhow_get", "knowhow_propose"])
    );
    let (sid, fence) = start(app, t, &mut dev, "/srv/api", None).await;
    (dev, sid, fence)
}

/// A session reads the map its starter may read: the people's search and
/// card, each entity and link with its status and source; a name not on the
/// map is `not_found`, and a question past its bounds `bad_request`.
#[tokio::test]
async fn a_session_reads_the_map_its_starter_may_read() {
    let app = session_app().await;
    let t = app.seed_tenant("knowhowask").await;
    let (db, _) = entity(&app, &t, "database", "prod-mongo").await;
    let (svc, _) = entity(&app, &t, "service", "api").await;
    let (code, body) = link(&app, &t, &svc, "reads", &db).await;
    assert_eq!(code, 201, "{body}");
    let (mut dev, sid, fence) = asking(&app, &t, "ask-box").await;

    let found = ask(
        &mut dev.ws,
        "s1",
        "rc:knowhow.search",
        &sid,
        fence,
        json!({"q": "mongo"}),
    )
    .await;
    assert_eq!(found["t"], "rc:knowhow.found", "{found}");
    let keys: Vec<&str> = found["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["prod-mongo"]);
    assert_eq!(found["entities"][0]["status"], "active");
    assert!(
        found["entities"][0].get("facts").is_none(),
        "a listing holds no facts: {found}"
    );

    let card = ask(
        &mut dev.ws,
        "g1",
        "rc:knowhow.get",
        &sid,
        fence,
        json!({"kind": "database", "key": "prod-mongo"}),
    )
    .await;
    assert_eq!(card["t"], "rc:knowhow.card", "{card}");
    assert_eq!(card["card"]["entity"]["key"], "prod-mongo");
    assert_eq!(card["card"]["entity"]["source"], "person");
    let links = card["card"]["links"].as_array().unwrap();
    assert_eq!(links.len(), 1, "{card}");
    assert_eq!(links[0]["direction"], "in");
    assert_eq!(links[0]["edge"]["kind"], "reads");
    assert_eq!(links[0]["other"]["key"], "api");
    assert_eq!(card["card"]["truncated"], false);

    let missing = ask(
        &mut dev.ws,
        "g2",
        "rc:knowhow.get",
        &sid,
        fence,
        json!({"kind": "database", "key": "staging-mongo"}),
    )
    .await;
    assert_eq!(refused(&missing), "not_found");
    for (req, t, rest) in [
        ("b1", "rc:knowhow.get", json!({"key": "prod-mongo"})),
        (
            "b2",
            "rc:knowhow.get",
            json!({"kind": "database", "key": "Prod Mongo"}),
        ),
        ("b3", "rc:knowhow.search", json!({"limit": 51})),
        ("b4", "rc:knowhow.search", json!({"q": "m".repeat(201)})),
    ] {
        let answer = ask(&mut dev.ws, req, t, &sid, fence, rest).await;
        assert_eq!(refused(&answer), "bad_request", "{req}");
    }
}

/// The record decides who asks: a session id from another device of the
/// organization is `not_located`, and so is one no session holds; an older
/// fence is `stale_fence`; a session past its run is `ended`.
#[tokio::test]
async fn a_question_is_believed_only_from_the_sessions_device_and_fence() {
    let app = session_app().await;
    let t = app.seed_tenant("knowhowfence").await;
    entity(&app, &t, "database", "prod-mongo").await;
    let (mut dev, sid, fence) = asking(&app, &t, "fence-a").await;
    let mut other = device(&app, &t, "fence-b", "linux", ASKS, false).await;

    let ok = ask(
        &mut dev.ws,
        "a1",
        "rc:knowhow.search",
        &sid,
        fence,
        json!({}),
    )
    .await;
    assert_eq!(ok["t"], "rc:knowhow.found", "{ok}");
    let elsewhere = ask(
        &mut other.ws,
        "b1",
        "rc:knowhow.search",
        &sid,
        fence,
        json!({}),
    )
    .await;
    assert_eq!(refused(&elsewhere), "not_located");
    let nobody = ObjectId::new().to_hex();
    let none = ask(
        &mut dev.ws,
        "a2",
        "rc:knowhow.search",
        &nobody,
        fence,
        json!({}),
    )
    .await;
    assert_eq!(refused(&none), "not_located");
    let older = ask(
        &mut dev.ws,
        "a3",
        "rc:knowhow.search",
        &sid,
        fence + 1,
        json!({}),
    )
    .await;
    assert_eq!(refused(&older), "stale_fence");

    app.db
        .collection::<bson::Document>("agent_sessions")
        .update_one(
            doc! { "_id": ObjectId::parse_str(&sid).unwrap() },
            doc! { "$set": { "status": "ended" } },
        )
        .await
        .unwrap();
    let ended = ask(
        &mut dev.ws,
        "a4",
        "rc:knowhow.search",
        &sid,
        fence,
        json!({}),
    )
    .await;
    assert_eq!(refused(&ended), "ended");
}

/// Decision 48: a session reads what its starter may, now. A starter whose
/// role loses `KNOWHOW_VIEW` stops the session's reads at its next question,
/// and a role that holds it again lets them read again.
#[tokio::test]
async fn a_starter_who_loses_knowhow_view_stops_their_sessions_reads() {
    let app = session_app().await;
    let t = app.seed_tenant("knowhowstarter").await;
    entity(&app, &t, "database", "prod-mongo").await;
    let (mut dev, sid, fence) = asking(&app, &t, "starter-box").await;
    let get = json!({"kind": "database", "key": "prod-mongo"});

    let card = ask(
        &mut dev.ws,
        "v1",
        "rc:knowhow.get",
        &sid,
        fence,
        get.clone(),
    )
    .await;
    assert_eq!(card["t"], "rc:knowhow.card", "{card}");
    hold_only(&app, &t, &t.admin.id, "guest").await;
    let denied = ask(
        &mut dev.ws,
        "v2",
        "rc:knowhow.get",
        &sid,
        fence,
        get.clone(),
    )
    .await;
    assert_eq!(refused(&denied), "denied");
    hold_only(&app, &t, &t.admin.id, "member").await;
    let again = ask(&mut dev.ws, "v3", "rc:knowhow.get", &sid, fence, get).await;
    assert_eq!(again["t"], "rc:knowhow.card", "{again}");
}

/// Decision 49: the map is said to be there only to a device that asks for
/// the word, on its primary enrollment, in an organization knowhow serves;
/// a question from an organization it does not serve is `not_served`.
#[tokio::test]
async fn the_map_is_served_only_where_knowhow_serves_and_the_device_asks() {
    let app = session_app().await;
    let t = app.seed_tenant("knowhowserved").await;
    let mut quiet = device(&app, &t, "quiet-box", "linux", &["exec", "hive"], false).await;
    assert!(
        recv_t(&mut quiet.ws, "rc:knowhow.served", Duration::from_secs(2))
            .await
            .is_none(),
        "a device that never asked for the word is not sent it"
    );
    let (_, token) = enroll(&app, &t, "second-box", "linux").await;
    let mut second = hello(&app, &token, ASKS, "primary_org").await;
    assert!(
        recv_t(&mut second, "rc:knowhow.served", Duration::from_secs(2))
            .await
            .is_none(),
        "a secondary enrollment runs no session for this organization"
    );

    let elsewhere = ObjectId::new().to_hex();
    let narrow = TestApp::spawn_with_settings(move |s| {
        s.modules.hive = true;
        s.knowhow.tenants = elsewhere;
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await;
    let n = narrow.seed_tenant("knowhowunserved").await;
    let mut dev = device(&narrow, &n, "unserved-box", "linux", ASKS, false).await;
    assert!(
        recv_t(&mut dev.ws, "rc:knowhow.served", Duration::from_secs(2))
            .await
            .is_none(),
        "an organization knowhow does not serve is told nothing"
    );
    let (sid, fence) = start(&narrow, &n, &mut dev, "/srv/api", None).await;
    let answer = ask(
        &mut dev.ws,
        "n1",
        "rc:knowhow.search",
        &sid,
        fence,
        json!({}),
    )
    .await;
    assert_eq!(refused(&answer), "not_served");
}

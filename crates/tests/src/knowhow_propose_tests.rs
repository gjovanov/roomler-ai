// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-4-2 — a session's proposals, the server's half.
//!
//! Driven through a RAW agent socket, as the reads' tests are
//! (`knowhow_session_tests`): the device starts a session through the real
//! start route, then proposes what a session's toolbelt will. What these
//! lock is the SERVER:
//!
//! - a proposal is written `proposed`, said by the session and naming it,
//!   its `why` on the history row, and the people's card shows it so;
//! - it adds, and never changes what is there (`exists`, nothing written);
//! - it never reaches into a device's part of the map (`reserved`);
//! - a credential's shape anywhere in it refuses it whole (`refused`);
//! - a session makes at most 50 (`ceiling`), counted from the history;
//! - a malformed one is `bad_request`, and one from an older fence is
//!   `stale_fence`, as a question is.

use bson::{Bson, doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::knowhow_scan_tests::Ws;
use crate::knowhow_session_tests::{ask, asking, refused, session_app};
use crate::knowhow_sync_tests::wait_machine;
use crate::knowhow_tests::{call, entity, kpath, link, patch};
use crate::vault_lease_tests::rows;

/// Propose `proposal` as the session `sid` at `fence`, saying `why`.
async fn propose(
    ws: &mut Ws,
    req_id: &str,
    sid: &str,
    fence: u64,
    proposal: Value,
    why: &str,
) -> Value {
    ask(
        ws,
        req_id,
        "rc:knowhow.propose",
        sid,
        fence,
        json!({ "proposal": proposal, "why": why }),
    )
    .await
}

fn proposed(answer: &Value) -> &Value {
    assert_eq!(answer["t"], "rc:knowhow.proposed", "{answer}");
    answer
}

/// The people's card of `kind` `key`, as the organization's admin reads it.
async fn card(app: &TestApp, t: &SeededTenant, kind: &str, key: &str) -> (u16, Value) {
    call(app.auth_get(
        &kpath(&t.tenant_id, &format!("/entity?kind={kind}&key={key}")),
        &t.admin.access_token,
    ))
    .await
}

/// The history rows the session `sid` wrote.
async fn written(app: &TestApp, sid: &str) -> Vec<bson::Document> {
    rows(
        app,
        "knowhow_history",
        doc! { "session": ObjectId::parse_str(sid).unwrap() },
    )
    .await
}

/// A token's shape, built here and never written down whole: a scanner
/// reading this repository must not take a test for a leak.
fn token() -> String {
    format!("ghp_{}", "a1B2".repeat(9))
}

/// A session proposes an entity, a fact of a person's entity and a link
/// between the two; each is written `proposed`, said by the session and
/// naming it, the person's entity left the person's, and the history says
/// each with its why. Each answer counts down what the session has left.
#[tokio::test]
async fn a_session_proposes_what_is_new_and_it_is_shown_as_a_proposal() {
    let app = session_app().await;
    let t = app.seed_tenant("knowhowpropose").await;
    entity(&app, &t, "database", "prod-mongo").await;
    let (mut dev, sid, fence) = asking(&app, &t, "propose-box").await;

    let made = propose(
        &mut dev.ws,
        "p1",
        &sid,
        fence,
        json!({"entity": {"kind": "service", "key": "payments", "name": "Payments",
                          "env": "prod", "facts": {"port": 8080}}}),
        "the compose file names it",
    )
    .await;
    assert_eq!(proposed(&made)["what"], "entity");
    assert_eq!(made["left"], 49);
    let (code, svc) = card(&app, &t, "service", "payments").await;
    assert_eq!(code, 200, "{svc}");
    let e = &svc["entity"];
    assert_eq!(e["id"], made["id"]);
    assert_eq!(
        (&e["status"], &e["source"], &e["session"], &e["env"]),
        (
            &json!("proposed"),
            &json!("session"),
            &json!(sid),
            &json!("prod")
        )
    );
    assert!(e.get("created_by").is_none(), "no person made it: {e}");
    let port = &e["facts"][0];
    assert_eq!(
        (&port["path"], &port["status"], &port["session"]),
        (&json!("port"), &json!("proposed"), &json!(sid))
    );

    let fact = propose(
        &mut dev.ws,
        "p2",
        &sid,
        fence,
        json!({"fact": {"kind": "database", "key": "prod-mongo", "path": "port",
                        "value": 27017}}),
        "its connection string names the port",
    )
    .await;
    assert_eq!(proposed(&fact)["what"], "fact");
    assert_eq!(fact["left"], 48);
    let linked = propose(
        &mut dev.ws,
        "p3",
        &sid,
        fence,
        json!({"link": {"from": {"kind": "service", "key": "payments"}, "kind": "reads",
                        "to": {"kind": "database", "key": "prod-mongo"}}}),
        "it reads its orders there",
    )
    .await;
    assert_eq!(proposed(&linked)["what"], "link");
    assert_eq!(linked["left"], 47);

    let (_, db) = card(&app, &t, "database", "prod-mongo").await;
    let d = &db["entity"];
    assert_eq!(d["id"], fact["id"], "a fact's answer names its entity");
    assert_eq!(
        (&d["status"], &d["source"]),
        (&json!("active"), &json!("person")),
        "the person's entity stays the person's: {d}"
    );
    let port = &d["facts"][0];
    assert_eq!(
        (
            &port["value"],
            &port["status"],
            &port["source"],
            &port["session"]
        ),
        (
            &json!(27017),
            &json!("proposed"),
            &json!("session"),
            &json!(sid)
        )
    );
    let l = &db["links"][0];
    assert_eq!(l["edge"]["id"], linked["id"]);
    assert_eq!(
        (
            &l["edge"]["status"],
            &l["edge"]["session"],
            &l["other"]["key"]
        ),
        (&json!("proposed"), &json!(sid), &json!("payments"))
    );

    let rows = written(&app, &sid).await;
    let whys: Vec<&str> = rows.iter().map(|h| h.get_str("why").unwrap()).collect();
    assert_eq!(
        whys,
        [
            "the compose file names it",
            "its connection string names the port",
            "it reads its orders there"
        ]
    );
    for h in &rows {
        assert_eq!(h.get_str("source").unwrap(), "session", "{h}");
        assert_eq!(h.get("by"), Some(&Bson::Null), "no person wrote it: {h}");
    }
}

/// A proposal adds; it never changes what is there. An entity by a key the
/// map holds, a fact at a path the entity holds, a link the two already
/// have: each `exists`, and nothing is written. One that names an entity not
/// on the map is `not_found`.
#[tokio::test]
async fn a_proposal_never_changes_what_is_there() {
    let app = session_app().await;
    let t = app.seed_tenant("knowhowexists").await;
    let (db, rev) = entity(&app, &t, "database", "prod-mongo").await;
    let (svc, _) = entity(&app, &t, "service", "api").await;
    let (code, body) = call(
        patch(
            &app,
            &kpath(&t.tenant_id, &format!("/entity/{db}")),
            &t.admin.access_token,
        )
        .json(&json!({ "rev": rev, "facts": { "port": 27017 } })),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let (code, body) = link(&app, &t, &svc, "reads", &db).await;
    assert_eq!(code, 201, "{body}");
    let (mut dev, sid, fence) = asking(&app, &t, "exists-box").await;
    let (_, before) = card(&app, &t, "database", "prod-mongo").await;

    for (req, proposal) in [
        (
            "e1",
            json!({"entity": {"kind": "database", "key": "prod-mongo", "name": "Mongo"}}),
        ),
        (
            "e2",
            json!({"fact": {"kind": "database", "key": "prod-mongo", "path": "port",
                            "value": 27018}}),
        ),
        (
            "e3",
            json!({"link": {"from": {"kind": "service", "key": "api"}, "kind": "reads",
                            "to": {"kind": "database", "key": "prod-mongo"}}}),
        ),
    ] {
        let answer = propose(&mut dev.ws, req, &sid, fence, proposal, "it said so").await;
        assert_eq!(refused(&answer), "exists", "{req}");
    }
    for (req, proposal) in [
        (
            "n1",
            json!({"fact": {"kind": "database", "key": "staging-mongo", "path": "port",
                            "value": 27017}}),
        ),
        (
            "n2",
            json!({"link": {"from": {"kind": "service", "key": "api"}, "kind": "reads",
                            "to": {"kind": "database", "key": "staging-mongo"}}}),
        ),
    ] {
        let answer = propose(&mut dev.ws, req, &sid, fence, proposal, "it said so").await;
        assert_eq!(refused(&answer), "not_found", "{req}");
    }
    let (_, after) = card(&app, &t, "database", "prod-mongo").await;
    assert_eq!(after["entity"]["rev"], before["entity"]["rev"], "unchanged");
    assert_eq!(after["entity"]["facts"], before["entity"]["facts"]);
    assert_eq!(after["entity"]["facts"][0]["source"], "person");
    assert!(written(&app, &sid).await.is_empty(), "nothing written");
}

/// A session never proposes into a device's part of the map: a machine, a
/// key in a device's scan shape, a fact the fleet or a scan keeps on a
/// machine, a link a scan makes. A fact of its own at a free path, and a
/// link to a machine a scan never makes, are proposals like any other.
#[tokio::test]
async fn a_session_never_proposes_into_a_devices_part() {
    let app = session_app().await;
    let t = app.seed_tenant("knowhowreserved").await;
    let (mut dev, sid, fence) = asking(&app, &t, "reserved-box").await;
    let node = dev.agent_id.clone();
    wait_machine(&app, &t, &node, |c| c["entity"]["key"] == json!(node)).await;
    entity(&app, &t, "cluster", "k8s.example.test:6443").await;
    entity(&app, &t, "service", "ci").await;

    for (req, proposal) in [
        (
            "r1",
            json!({"entity": {"kind": "machine", "key": "build-box", "name": "Build box"}}),
        ),
        (
            "r2",
            json!({"entity": {"kind": "checkout", "key": format!("{node}/0123456789abcdef"),
                              "name": "api"}}),
        ),
        (
            "r3",
            json!({"entity": {"kind": "endpoint", "key": format!("{node}:5432/tcp"),
                              "name": "postgres"}}),
        ),
        (
            "r4",
            json!({"fact": {"kind": "machine", "key": node, "path": "os", "value": "plan9"}}),
        ),
        (
            "r5",
            json!({"fact": {"kind": "machine", "key": node, "path": "tunnel.declared",
                            "value": []}}),
        ),
        (
            "r6",
            json!({"link": {"from": {"kind": "cluster", "key": "k8s.example.test:6443"},
                            "kind": "reachable_via", "to": {"kind": "machine", "key": node}}}),
        ),
    ] {
        let answer = propose(&mut dev.ws, req, &sid, fence, proposal, "it said so").await;
        assert_eq!(refused(&answer), "reserved", "{req}");
    }
    assert!(written(&app, &sid).await.is_empty(), "nothing written");

    let role = propose(
        &mut dev.ws,
        "ok1",
        &sid,
        fence,
        json!({"fact": {"kind": "machine", "key": node, "path": "role", "value": "ci runner"}}),
        "its jobs say so",
    )
    .await;
    assert_eq!(proposed(&role)["what"], "fact");
    let runs = propose(
        &mut dev.ws,
        "ok2",
        &sid,
        fence,
        json!({"link": {"from": {"kind": "service", "key": "ci"}, "kind": "runs_on",
                        "to": {"kind": "machine", "key": node}}}),
        "its runner is this box",
    )
    .await;
    assert_eq!(proposed(&runs)["what"], "link");
    assert_eq!(written(&app, &sid).await.len(), 2);
}

/// Text in a credential's shape anywhere in a proposal, or in its why,
/// refuses it whole: nothing of it is written.
#[tokio::test]
async fn a_proposal_holding_a_credentials_shape_is_refused_whole() {
    let app = session_app().await;
    let t = app.seed_tenant("knowhowcredential").await;
    entity(&app, &t, "database", "prod-mongo").await;
    let (mut dev, sid, fence) = asking(&app, &t, "credential-box").await;
    let (t1, url) = (
        token(),
        format!("https://app:{}@db.example.test:27017/", "s3cret"),
    );
    let why = format!("the env file says TOKEN={t1}");

    for (req, proposal, why) in [
        (
            "c1",
            json!({"entity": {"kind": "service", "key": "api", "name": "API",
                              "attrs": {"auth": {"token": t1}}}}),
            "the env file names it",
        ),
        (
            "c2",
            json!({"fact": {"kind": "database", "key": "prod-mongo", "path": "url",
                            "value": url}}),
            "its connection string",
        ),
        (
            "c3",
            json!({"entity": {"kind": "service", "key": "api", "name": "API"}}),
            why.as_str(),
        ),
    ] {
        let answer = propose(&mut dev.ws, req, &sid, fence, proposal, why).await;
        assert_eq!(refused(&answer), "refused", "{req}");
    }
    assert!(written(&app, &sid).await.is_empty(), "nothing written");
    assert_eq!(card(&app, &t, "service", "api").await.0, 404);
    let (_, db) = card(&app, &t, "database", "prod-mongo").await;
    assert_eq!(db["entity"]["facts"], json!([]), "{db}");
}

/// Decision 51: a session makes at most 50 proposals, counted from the
/// history; the next is `ceiling`, and nothing of it is written.
#[tokio::test]
async fn a_session_proposes_at_most_fifty() {
    let app = session_app().await;
    let t = app.seed_tenant("knowhowceiling").await;
    let (mut dev, sid, fence) = asking(&app, &t, "ceiling-box").await;
    for i in 0..50_u64 {
        let answer = propose(
            &mut dev.ws,
            &format!("m{i}"),
            &sid,
            fence,
            json!({"entity": {"kind": "service", "key": format!("svc-{i}"),
                              "name": format!("Service {i}")}}),
            "it is in the compose file",
        )
        .await;
        assert_eq!(proposed(&answer)["left"], json!(49 - i), "{i}");
    }
    let past = propose(
        &mut dev.ws,
        "m50",
        &sid,
        fence,
        json!({"entity": {"kind": "service", "key": "svc-50", "name": "Service 50"}}),
        "it is in the compose file",
    )
    .await;
    assert_eq!(refused(&past), "ceiling");
    assert_eq!(card(&app, &t, "service", "svc-50").await.0, 404);
    assert_eq!(written(&app, &sid).await.len(), 50);
}

/// A malformed proposal is `bad_request`: none, two at once, a kind or a key
/// the people's write refuses, a link from an entity to itself, a why
/// missing or past its bound. One from an older fence is `stale_fence`, as
/// a question is. Nothing of any is written.
#[tokio::test]
async fn a_malformed_or_stale_proposal_writes_nothing() {
    let app = session_app().await;
    let t = app.seed_tenant("knowhowbadproposal").await;
    entity(&app, &t, "service", "api").await;
    let (mut dev, sid, fence) = asking(&app, &t, "bad-proposal-box").await;
    let svc = json!({"entity": {"kind": "service", "key": "billing", "name": "Billing"}});
    let long = "w".repeat(501);

    for (req, proposal, why) in [
        ("b1", Value::Null, "it said so"),
        (
            "b2",
            json!({"entity": svc["entity"].clone(), "fact": {"kind": "service", "key": "api",
                                                             "path": "port", "value": 1}}),
            "it said so",
        ),
        (
            "b3",
            json!({"entity": {"kind": "spaceship", "key": "api2", "name": "API"}}),
            "it said so",
        ),
        (
            "b4",
            json!({"entity": {"kind": "service", "key": "Billing API", "name": "API"}}),
            "it said so",
        ),
        (
            "b5",
            json!({"link": {"from": {"kind": "service", "key": "api"}, "kind": "depends_on",
                            "to": {"kind": "service", "key": "api"}}}),
            "it said so",
        ),
        ("b6", svc.clone(), ""),
        ("b7", svc.clone(), long.as_str()),
    ] {
        let answer = propose(&mut dev.ws, req, &sid, fence, proposal, why).await;
        assert_eq!(refused(&answer), "bad_request", "{req}");
    }
    let stale = propose(&mut dev.ws, "s1", &sid, fence + 1, svc, "it said so").await;
    assert_eq!(refused(&stale), "stale_fence");
    assert!(written(&app, &sid).await.is_empty(), "nothing written");
    assert_eq!(card(&app, &t, "service", "billing").await.0, 404);
}

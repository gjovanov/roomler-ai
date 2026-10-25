// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-5-3-1 — staleness (decisions 40 and 57; `docs/knowhow.md` §13.4).
//!
//! What these lock, through the real sweep on a one-second period:
//!
//! - a person's fact nothing confirmed for `knowhow.stale_days` turns
//!   `stale`, with a history row the system's, and one confirmed since
//!   stays `active`;
//! - a person's confirm, or a person's write, makes it `active` and young
//!   again;
//! - the sync's facts, a scan's own and a proposal still waiting never turn
//!   stale, however old;
//! - an organization knowhow does not serve is never swept, and
//!   `knowhow.stale_days = 0` turns nothing.

use std::time::Duration;

use bson::{DateTime, Document, doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::knowhow_tests::{call, create, kpath, patch};

const DAY_MS: i64 = 86_400_000;

/// Knowhow on, staleness at `days`, swept every second.
async fn stale_app(days: Option<u64>, tenants: Option<String>) -> TestApp {
    TestApp::spawn_with_settings(move |s| {
        s.modules.knowhow = true;
        s.knowhow.stale_days = days;
        s.knowhow.stale_sweep_secs = 1;
        if let Some(t) = tenants.clone() {
            s.knowhow.tenants = t;
        }
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await
}

fn entities(app: &TestApp) -> mongodb::Collection<Document> {
    app.db.collection::<Document>("knowhow_entities")
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

/// Back-date the facts at `paths` to `days_ago`, as if written then; a
/// `source` and a `status` given too, as another writer would have left
/// them. Nothing else of the entity moves, its rev included.
async fn age(app: &TestApp, id: &str, paths: &[&str], days_ago: i64, as_: Option<(&str, &str)>) {
    let oid = ObjectId::parse_str(id).unwrap();
    let mut row = entities(app)
        .find_one(doc! { "_id": oid })
        .await
        .unwrap()
        .expect("the entity's row");
    let then = DateTime::from_millis(DateTime::now().timestamp_millis() - days_ago * DAY_MS);
    let mut facts = row.get_array_mut("facts").unwrap().clone();
    for f in facts.iter_mut() {
        let f = f.as_document_mut().unwrap();
        if paths.contains(&f.get_str("path").unwrap()) {
            f.insert("observed_at", then);
            if let Some((source, status)) = as_ {
                f.insert("source", source);
                f.insert("status", status);
            }
        }
    }
    entities(app)
        .update_one(doc! { "_id": oid }, doc! { "$set": { "facts": facts } })
        .await
        .unwrap();
}

/// The entity as the people's route reads it.
async fn read(app: &TestApp, t: &SeededTenant, id: &str) -> Value {
    let (code, body) = call(app.auth_get(
        &kpath(&t.tenant_id, &format!("/entity/{id}")),
        &t.admin.access_token,
    ))
    .await;
    assert_eq!(code, 200, "{body}");
    body
}

fn status_of<'a>(entity: &'a Value, path: &str) -> &'a str {
    let entity = entity.get("entity").unwrap_or(entity);
    entity["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == path)
        .unwrap_or_else(|| panic!("no fact {path}: {entity}"))["status"]
        .as_str()
        .unwrap()
}

fn rev_of(entity: &Value) -> i64 {
    entity.get("entity").unwrap_or(entity)["rev"]
        .as_i64()
        .unwrap()
}

/// Wait until the entity's fact at `path` reads `want`, 10 s at most.
async fn until_status(app: &TestApp, t: &SeededTenant, id: &str, path: &str, want: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let entity = read(app, t, id).await;
        if status_of(&entity, path) == want {
            return entity;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{path} never read {want}: {entity}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A person's facts nothing confirmed for 30 days turn stale at the sweep,
/// the history saying so as the system's; one confirmed since stays active.
/// A person's confirm, and a person's write, make a stale fact active and
/// young again; the next sweeps leave it so.
#[tokio::test]
async fn a_persons_fact_nothing_confirmed_turns_stale_and_a_word_revives_it() {
    let app = stale_app(None, None).await;
    let t = app.seed_tenant("knowstale").await;
    let db = endpoint(
        &app,
        &t,
        "db",
        json!({ "address": "10.0.4.12", "port": 5432, "team": "payments" }),
    )
    .await;
    age(&app, &db, &["address", "port"], 31, None).await;

    let entity = until_status(&app, &t, &db, "address", "stale").await;
    assert_eq!(status_of(&entity, "port"), "stale");
    assert_eq!(status_of(&entity, "team"), "active", "written today");
    let row = app
        .db
        .collection::<Document>("knowhow_history")
        .find_one(doc! { "op": "stale" })
        .await
        .unwrap()
        .expect("a history row for it");
    assert_eq!(row.get_str("source").unwrap(), "system");
    assert!(
        row.get("by").is_none_or(|b| b == &bson::Bson::Null),
        "{row}"
    );
    let said: Vec<String> = row
        .get_document("set")
        .unwrap()
        .get_array("facts")
        .unwrap()
        .iter()
        .map(|f| {
            f.as_document()
                .unwrap()
                .get_str("path")
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(said, ["address", "port"]);

    // A person's word on the stale port: active, confirmed by them, now.
    let (code, body) = call(
        app.auth_post(
            &kpath(&t.tenant_id, &format!("/entity/{db}/fact/confirm")),
            &t.admin.access_token,
        )
        .json(&json!({ "rev": rev_of(&entity), "path": "port" })),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(status_of(&body, "port"), "active");
    // Only a proposal or a stale fact is confirmed.
    let (code, _) = call(
        app.auth_post(
            &kpath(&t.tenant_id, &format!("/entity/{db}/fact/confirm")),
            &t.admin.access_token,
        )
        .json(&json!({ "rev": rev_of(&body), "path": "team" })),
    )
    .await;
    assert_eq!(code, 409, "an active fact is no one's to confirm");

    // A person's write of the stale address, its value as it was.
    let (code, body) = call(
        patch(
            &app,
            &kpath(&t.tenant_id, &format!("/entity/{db}")),
            &t.admin.access_token,
        )
        .json(&json!({ "rev": rev_of(&body), "facts": { "address": "10.0.4.12" } })),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(status_of(&body, "address"), "active");

    // Young again: the sweeps after leave both active.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let entity = read(&app, &t, &db).await;
    assert_eq!(
        (status_of(&entity, "address"), status_of(&entity, "port")),
        ("active", "active"),
        "{entity}"
    );
}

/// However old: the sync's facts, a scan's own and a proposal still
/// waiting never turn stale; a person's fact beside them does.
#[tokio::test]
async fn the_syncs_a_scans_and_a_waiting_proposals_facts_never_turn_stale() {
    let app = stale_app(None, None).await;
    let t = app.seed_tenant("knowstalenever").await;
    let svc = endpoint(
        &app,
        &t,
        "svc",
        json!({ "os": "linux", "path": "/srv/api", "version": "16", "owner": "ops" }),
    )
    .await;
    age(&app, &svc, &["os"], 400, Some(("sync", "active"))).await;
    age(&app, &svc, &["path"], 400, Some(("scan", "active"))).await;
    age(&app, &svc, &["version"], 400, Some(("session", "proposed"))).await;
    age(&app, &svc, &["owner"], 400, None).await;

    let entity = until_status(&app, &t, &svc, "owner", "stale").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let entity = read(&app, &t, &svc)
        .await
        .get("entity")
        .cloned()
        .unwrap_or(entity);
    for (path, want) in [
        ("os", "active"),
        ("path", "active"),
        ("version", "proposed"),
    ] {
        assert_eq!(status_of(&entity, path), want, "{path}: {entity}");
    }
}

/// An organization knowhow does not serve is never swept; one it serves
/// is. And at `knowhow.stale_days = 0` nothing turns.
#[tokio::test]
async fn an_unserved_organization_and_a_zero_setting_turn_nothing() {
    let served = ObjectId::new();
    let app = stale_app(None, Some(served.to_hex())).await;
    let t = app.seed_tenant("knowstaleunserved").await;
    let old = DateTime::from_millis(DateTime::now().timestamp_millis() - 400 * DAY_MS);
    let fact = |path: &str| {
        doc! { "path": path, "value": 1, "source": "person", "observed_at": old, "status": "active" }
    };
    let row = |tenant: ObjectId, key: &str| {
        doc! {
            "tenant_id": tenant, "kind": "endpoint", "key": key, "name": key, "env": null,
            "attrs": {}, "facts": [fact("port")], "source": "person", "status": "active",
            "rev": 1_i64, "created_at": old, "updated_at": old, "deleted_at": null,
        }
    };
    let mine = ObjectId::parse_str(&t.tenant_id).unwrap();
    let theirs = entities(&app)
        .insert_one(row(mine, "unserved"))
        .await
        .unwrap()
        .inserted_id;
    let listed = entities(&app)
        .insert_one(row(served, "served"))
        .await
        .unwrap()
        .inserted_id;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let r = entities(&app)
            .find_one(doc! { "_id": &listed })
            .await
            .unwrap()
            .unwrap();
        let facts = r.get_array("facts").unwrap();
        if facts[0].as_document().unwrap().get_str("status").unwrap() == "stale" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the served one never turned: {r}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let r = entities(&app)
        .find_one(doc! { "_id": &theirs })
        .await
        .unwrap()
        .unwrap();
    let status = r.get_array("facts").unwrap()[0]
        .as_document()
        .unwrap()
        .get_str("status")
        .unwrap()
        .to_string();
    assert_eq!(
        status, "active",
        "an organization knowhow does not serve is never swept"
    );

    let off = stale_app(Some(0), None).await;
    let t = off.seed_tenant("knowstaleoff").await;
    let db = endpoint(&off, &t, "db", json!({ "port": 5432 })).await;
    age(&off, &db, &["port"], 400, None).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(status_of(&read(&off, &t, &db).await, "port"), "active");
}

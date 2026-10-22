// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-1 — knowhow, the organization's map: the store and the people's
//! routes.
//!
//! What these lock: the module is off until turned on, and a
//! `knowhow.tenants` list both turns it on and keeps every other organization
//! out; the map is read from `member` up and changed by `admin` (decisions 37
//! and 36), never by a guest; an entity is named once by its kind and key,
//! and a `machine` is never a person's; a change is made at the rev it was
//! read and keeps every fact it does not name as it was, provenance and all;
//! links walk both ways, are bounded, and go with what they link; every write
//! is in the history; and an id of another organization answers as a bogus
//! one does.

use bson::{Document, doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;

async fn knowhow_app() -> TestApp {
    TestApp::spawn_with_settings(|s| {
        s.modules.knowhow = true;
        // Many calls per test: the default limiter would answer 429 first.
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await
}

fn kpath(tid: &str, rest: &str) -> String {
    format!("/api/tenant/{tid}/knowhow{rest}")
}

/// Status and the body as JSON.
async fn call(req: reqwest::RequestBuilder) -> (u16, Value) {
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn patch(app: &TestApp, path: &str, token: &str) -> reqwest::RequestBuilder {
    app.client
        .patch(app.url(path))
        .header("Authorization", format!("Bearer {token}"))
}

async fn create(app: &TestApp, t: &SeededTenant, token: &str, body: Value) -> (u16, Value) {
    call(
        app.auth_post(&kpath(&t.tenant_id, "/entity"), token)
            .json(&body),
    )
    .await
}

/// An entity the owner puts on the map; its id and rev.
async fn entity(app: &TestApp, t: &SeededTenant, kind: &str, key: &str) -> (String, i64) {
    let (code, body) = create(
        app,
        t,
        &t.admin.access_token,
        json!({ "kind": kind, "key": key, "name": key }),
    )
    .await;
    assert_eq!(code, 201, "{body}");
    (
        body["id"].as_str().unwrap().to_string(),
        body["rev"].as_i64().unwrap(),
    )
}

async fn link(app: &TestApp, t: &SeededTenant, from: &str, kind: &str, to: &str) -> (u16, Value) {
    call(
        app.auth_post(&kpath(&t.tenant_id, "/edge"), &t.admin.access_token)
            .json(&json!({ "from": from, "kind": kind, "to": to })),
    )
    .await
}

/// The ids of a map's entities, sorted.
fn ids_of(body: &Value, key: &str) -> Vec<String> {
    let mut ids: Vec<String> = body[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

fn sorted(ids: &[&String]) -> Vec<String> {
    let mut ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
    ids.sort();
    ids
}

/// Give a member exactly one of the organization's managed roles.
async fn hold_only(app: &TestApp, t: &SeededTenant, user_id: &str, role: &str) {
    let tid = ObjectId::parse_str(&t.tenant_id).unwrap();
    let uid = ObjectId::parse_str(user_id).unwrap();
    let role_id = app
        .db
        .collection::<Document>("roles")
        .find_one(doc! { "tenant_id": tid, "name": role })
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no {role} role"))
        .get_object_id("_id")
        .unwrap();
    let r = app
        .db
        .collection::<Document>("tenant_members")
        .update_one(
            doc! { "tenant_id": tid, "user_id": uid },
            doc! { "$set": { "role_ids": [role_id] } },
        )
        .await
        .unwrap();
    assert_eq!(r.matched_count, 1, "not a member");
}

/// The module ships off: no route, and the server says it is switched off.
#[tokio::test]
async fn knowhow_is_off_until_turned_on() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("knowoff").await;
    let (code, _) = call(app.auth_get(&kpath(&t.tenant_id, ""), &t.admin.access_token)).await;
    assert_eq!(code, 404, "not mounted ⇒ no route");
    let caps: Value = app
        .client
        .get(app.url("/api/capabilities"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed = |key: &str| caps[key].as_array().unwrap().iter().any(|m| m == "knowhow");
    assert!(!listed("modules") && listed("switched_off"), "{caps}");
}

/// `knowhow.tenants` set: the list alone mounts the module, its organizations
/// get their map, any other is answered as if it were not there, and someone
/// outside an organization learns nothing about whether it is served.
#[tokio::test]
async fn an_organization_knowhow_does_not_serve_sees_none_of_it() {
    let first = knowhow_app().await;
    let a = first.seed_tenant("knowserved").await;
    let b = first.seed_tenant("knowunserved").await;
    let db = first.db.name().to_string();
    let only_a = a.tenant_id.clone();
    let app = TestApp::spawn_with_settings(move |s| {
        s.database.name = db;
        s.knowhow.tenants = only_a;
    })
    .await;

    let (code, body) = call(app.auth_get(&kpath(&a.tenant_id, ""), &a.admin.access_token)).await;
    assert_eq!((code, body["entities"].clone()), (200, json!(0)), "{body}");
    for path in ["", "/search", "/history"] {
        let (code, body) =
            call(app.auth_get(&kpath(&b.tenant_id, path), &b.admin.access_token)).await;
        assert_eq!(code, 404, "{path}: {body}");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or("")
                .contains("not available"),
            "{path}: {body}"
        );
    }
    let (code, _) = create(
        &app,
        &b,
        &b.admin.access_token,
        json!({ "kind": "database", "key": "db", "name": "db" }),
    )
    .await;
    assert_eq!(code, 404, "no map for an unserved org");

    let caps: Value = app
        .client
        .get(app.url("/api/capabilities"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed = |key: &str| caps[key].as_array().unwrap().iter().any(|m| m == "knowhow");
    assert!(listed("modules") && !listed("switched_off"), "{caps}");

    // Outside an organization the answer is about membership, never about
    // the gate: the same for one served and one not.
    for (org, outsider) in [(&a, &b), (&b, &a)] {
        let (code, body) =
            call(app.auth_get(&kpath(&org.tenant_id, ""), &outsider.admin.access_token)).await;
        assert_eq!(
            (code, body["error"].clone()),
            (403, json!("not_a_member")),
            "{body}"
        );
    }
}

/// Decisions 37 and 36: a member reads the map and cannot change it, a guest
/// cannot even read it, and an admin changes it.
#[tokio::test]
async fn the_map_is_read_from_member_up_and_changed_by_admins() {
    let app = knowhow_app().await;
    let t = app.seed_tenant("knowroles").await;
    let (id, rev) = entity(&app, &t, "database", "prod-mongo").await;
    let member = &t.member.access_token;

    for path in [
        String::new(),
        "/search".to_string(),
        "/history".to_string(),
        format!("/entity/{id}"),
        format!("/map/{id}"),
        "/entity?kind=database&key=prod-mongo".to_string(),
    ] {
        let (code, body) = call(app.auth_get(&kpath(&t.tenant_id, &path), member)).await;
        assert_eq!(code, 200, "a member reads {path:?}: {body}");
    }
    let (code, body) = create(
        &app,
        &t,
        member,
        json!({ "kind": "service", "key": "api", "name": "api" }),
    )
    .await;
    assert_eq!(code, 403, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("KNOWHOW_EDIT"),
        "{body}"
    );
    let (code, _) = call(
        patch(&app, &kpath(&t.tenant_id, &format!("/entity/{id}")), member)
            .json(&json!({ "rev": rev, "name": "x" })),
    )
    .await;
    assert_eq!(code, 403);
    let (code, _) = call(app.auth_delete(
        &kpath(&t.tenant_id, &format!("/entity/{id}?rev={rev}")),
        member,
    ))
    .await;
    assert_eq!(code, 403);

    // A guest reads no map.
    hold_only(&app, &t, &t.member.id, "guest").await;
    let (code, body) = call(app.auth_get(&kpath(&t.tenant_id, "/search"), member)).await;
    assert_eq!(code, 403, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("KNOWHOW_VIEW"),
        "{body}"
    );
    assert_ne!(
        body["error"],
        json!("not_a_member"),
        "a refusal, not an eviction"
    );

    // An admin changes it.
    hold_only(&app, &t, &t.member.id, "admin").await;
    let (code, body) = create(
        &app,
        &t,
        member,
        json!({ "kind": "service", "key": "api", "name": "api" }),
    )
    .await;
    assert_eq!(code, 201, "{body}");
    assert_eq!(body["created_by"], json!(t.member.id));
}

/// An entity is named once by its kind and key, is found by them, by its
/// text and by its environment; a `machine` is the fleet's to record, and a
/// write is held to one spelling.
#[tokio::test]
async fn an_entity_is_put_on_the_map_found_and_named_once() {
    let app = knowhow_app().await;
    let t = app.seed_tenant("knowentity").await;
    let owner = &t.admin.access_token;
    let (code, made) = create(
        &app,
        &t,
        owner,
        json!({
            "kind": "database", "key": "prod-mongo", "name": "Prod Mongo", "env": "prod",
            "owner_id": t.member.id,
            "attrs": { "engine": "mongodb", "replicas": [1, 2, 3] },
            "facts": { "port": 27017, "tls.enabled": true },
        }),
    )
    .await;
    assert_eq!(code, 201, "{made}");
    assert_eq!(
        (
            made["rev"].clone(),
            made["status"].clone(),
            made["source"].clone()
        ),
        (json!(1), json!("active"), json!("person"))
    );
    assert_eq!(
        made["attrs"],
        json!({ "engine": "mongodb", "replicas": [1, 2, 3] })
    );
    assert_eq!(made["owner_id"], json!(t.member.id));
    let facts = made["facts"].as_array().unwrap();
    assert_eq!(
        facts.iter().map(|f| f["path"].clone()).collect::<Vec<_>>(),
        [json!("port"), json!("tls.enabled")],
        "by path"
    );
    assert!(
        facts
            .iter()
            .all(|f| f["source"] == json!("person") && f["by"] == json!(t.admin.id)),
        "{made}"
    );
    let id = made["id"].as_str().unwrap();

    // Its card, by id and by name.
    for path in [
        format!("/entity/{id}"),
        "/entity?kind=database&key=prod-mongo".to_string(),
    ] {
        let (code, card) = call(app.auth_get(&kpath(&t.tenant_id, &path), owner)).await;
        assert_eq!(code, 200, "{path}: {card}");
        assert_eq!(card["entity"]["id"], json!(id));
        assert_eq!(card["links"], json!([]));
    }

    // Found by text, case aside and never as a pattern; by kind; by env.
    entity(&app, &t, "service", "roomler2").await;
    for (query, want) in [
        ("?q=MONGO", vec!["prod-mongo"]),
        ("?q=prod", vec!["prod-mongo"]),
        ("?q=.*", vec![]),
        ("?kind=service", vec!["roomler2"]),
        ("?env=prod", vec!["prod-mongo"]),
        ("", vec!["prod-mongo", "roomler2"]),
        // A kind a newer server may write is searched for, not refused.
        ("?kind=queue", vec![]),
    ] {
        let (code, body) =
            call(app.auth_get(&kpath(&t.tenant_id, &format!("/search{query}")), owner)).await;
        assert_eq!(code, 200, "{query}: {body}");
        let keys: Vec<&str> = body["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, want, "{query}");
        assert!(
            body["entities"][0].get("attrs").is_none(),
            "a listing carries no attributes: {body}"
        );
    }
    let (code, body) = call(app.auth_get(&kpath(&t.tenant_id, "/search?limit=1"), owner)).await;
    assert_eq!(
        (code, body["truncated"].clone()),
        (200, json!(true)),
        "{body}"
    );

    // Named once.
    let (code, body) = create(
        &app,
        &t,
        owner,
        json!({ "kind": "database", "key": "prod-mongo", "name": "again" }),
    )
    .await;
    assert_eq!(code, 409, "{body}");
    // The same key under another kind is another entity, and a lookup tells
    // the two apart.
    let (code, _) = create(
        &app,
        &t,
        owner,
        json!({ "kind": "service", "key": "prod-mongo", "name": "a service" }),
    )
    .await;
    assert_eq!(code, 201);
    for kind in ["database", "service"] {
        let (code, card) = call(app.auth_get(
            &kpath(&t.tenant_id, &format!("/entity?kind={kind}&key=prod-mongo")),
            owner,
        ))
        .await;
        assert_eq!(
            (code, card["entity"]["kind"].clone()),
            (200, json!(kind)),
            "{card}"
        );
    }
    let (code, _) = call(app.auth_get(
        &kpath(&t.tenant_id, "/entity?kind=cluster&key=prod-mongo"),
        owner,
    ))
    .await;
    assert_eq!(code, 404);

    // Never a person's machine; one spelling; nothing a document could read
    // as a path or an operator.
    for (bad, why) in [
        (
            json!({ "kind": "machine", "key": "mars", "name": "mars" }),
            "machine",
        ),
        (json!({ "kind": "server", "key": "x", "name": "x" }), "kind"),
        (
            json!({ "kind": "database", "key": "Prod", "name": "x" }),
            "key",
        ),
        (
            json!({ "kind": "database", "key": "a//b", "name": "x" }),
            "key",
        ),
        (
            json!({ "kind": "database", "key": "x", "name": "  " }),
            "name",
        ),
        (
            json!({ "kind": "database", "key": "x", "name": "x", "env": "Prod" }),
            "env",
        ),
        (
            json!({ "kind": "database", "key": "x", "name": "x", "attrs": { "$where": 1 } }),
            "attrs",
        ),
        (
            json!({ "kind": "database", "key": "x", "name": "x", "attrs": { "a.b": 1 } }),
            "attrs",
        ),
        (
            json!({ "kind": "database", "key": "x", "name": "x", "facts": { "port": null } }),
            "fact",
        ),
        (
            json!({ "kind": "database", "key": "x", "name": "x", "owner_id": ObjectId::new().to_hex() }),
            "owner",
        ),
    ] {
        let (code, body) = create(&app, &t, owner, bad.clone()).await;
        assert_eq!(code, 400, "{why}: {bad} → {body}");
    }
    let (code, _) = create(
        &app,
        &t,
        owner,
        json!({ "kind": "database", "key": "x", "name": "x", "status": "active" }),
    )
    .await;
    assert_eq!(code, 422, "an unknown field is refused");
}

/// A change is made at the rev it was read, a change that changes nothing
/// writes nothing, and the facts it does not name keep who said them.
#[tokio::test]
async fn a_change_is_made_at_the_rev_it_was_read_and_keeps_every_other_facts_provenance() {
    let app = knowhow_app().await;
    let t = app.seed_tenant("knowchange").await;
    let (code, made) = create(
        &app,
        &t,
        &t.admin.access_token,
        json!({
            "kind": "database", "key": "orders", "name": "Orders", "env": "staging",
            "facts": { "port": 5432, "engine": "postgres", "version": "16" },
        }),
    )
    .await;
    assert_eq!(code, 201, "{made}");
    let id = made["id"].as_str().unwrap().to_string();
    hold_only(&app, &t, &t.member.id, "admin").await;
    let editor = &t.member.access_token;
    let at = |rev: i64, body: Value| {
        let mut b = body;
        b["rev"] = json!(rev);
        patch(&app, &kpath(&t.tenant_id, &format!("/entity/{id}")), editor).json(&b)
    };

    // A fact changed, one removed, one never there (nothing to remove), the
    // environment cleared.
    let (code, after) = call(at(
        1,
        json!({ "facts": { "port": 6432, "version": null, "never": null }, "env": "" }),
    ))
    .await;
    assert_eq!(code, 200, "{after}");
    assert_eq!(after["rev"], json!(2));
    assert!(after.get("env").is_none(), "cleared: {after}");
    assert_eq!(
        after["facts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].clone())
            .collect::<Vec<_>>(),
        [json!("engine"), json!("port")],
        "{after}"
    );
    let fact = |body: &Value, path: &str| {
        body["facts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == json!(path))
            .cloned()
            .unwrap()
    };
    assert_eq!(
        (
            fact(&after, "port")["value"].clone(),
            fact(&after, "port")["by"].clone()
        ),
        (json!(6432), json!(t.member.id)),
        "the editor's value, the editor's fact"
    );
    assert_eq!(
        fact(&after, "engine")["by"],
        json!(t.admin.id),
        "a fact the change did not name keeps who said it: {after}"
    );
    assert_eq!(
        fact(&after, "engine")["observed_at"],
        fact(&made, "engine")["observed_at"]
    );

    // At a rev already gone: refused, and nothing changed.
    let (code, body) = call(at(1, json!({ "name": "Stale" }))).await;
    assert_eq!(code, 409, "{body}");
    let (_, card) =
        call(app.auth_get(&kpath(&t.tenant_id, &format!("/entity/{id}")), editor)).await;
    assert_eq!(
        (
            card["entity"]["name"].clone(),
            card["entity"]["rev"].clone()
        ),
        (json!("Orders"), json!(2))
    );

    // A change that changes nothing writes nothing.
    let (code, same) = call(at(
        2,
        json!({ "name": "Orders", "facts": { "engine": "postgres" } }),
    ))
    .await;
    assert_eq!((code, same["rev"].clone()), (200, json!(2)), "{same}");
    let (code, body) = call(at(2, json!({}))).await;
    assert_eq!(code, 400, "nothing to change: {body}");
    // Its kind and key never change.
    let (code, _) = call(at(2, json!({ "key": "orders-2" }))).await;
    assert_eq!(code, 422);

    // The history: newest first, the create and the one real change.
    let (code, hist) = call(app.auth_get(
        &kpath(&t.tenant_id, &format!("/history?entity={id}")),
        editor,
    ))
    .await;
    assert_eq!(code, 200, "{hist}");
    let rows = hist["history"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r["op"].clone(), r["rev"].clone()))
            .collect::<Vec<_>>(),
        [(json!("update"), json!(2)), (json!("create"), json!(1))],
        "{hist}"
    );
    assert_eq!(rows[0]["by"], json!(t.member.id));
    assert_eq!(
        rows[0]["set"]["facts"],
        json!([{ "path": "port", "value": 6432 }, { "path": "version", "value": null }]),
        "{hist}"
    );
    assert_eq!(rows[0]["set"]["env"], Value::Null);
    assert_eq!(rows[1]["set"]["key"], json!("orders"));
}

/// Links walk both ways one or two steps out, are named once, never loop on
/// one entity, and go with what they link — each in the history.
#[tokio::test]
async fn links_walk_both_ways_and_go_with_what_they_link() {
    let app = knowhow_app().await;
    let t = app.seed_tenant("knowlinks").await;
    let owner = &t.admin.access_token;
    let (svc, _) = entity(&app, &t, "service", "roomler2").await;
    let (db, _) = entity(&app, &t, "database", "prod-mongo").await;
    let (ns, ns_rev) = entity(&app, &t, "namespace", "prod/roomler-ai").await;
    let (cl, _) = entity(&app, &t, "cluster", "prod").await;
    for (from, kind, to) in [
        (&svc, "reads", &db),
        (&svc, "part_of", &ns),
        (&ns, "part_of", &cl),
    ] {
        let (code, body) = link(&app, &t, from, kind, to).await;
        assert_eq!(code, 201, "{body}");
    }
    let (code, _) = link(&app, &t, &svc, "reads", &db).await;
    assert_eq!(code, 409, "named once");
    let (code, _) = link(&app, &t, &svc, "writes", &db).await;
    assert_eq!(code, 201, "another kind is another link");
    let (code, _) = link(&app, &t, &svc, "reads", &svc).await;
    assert_eq!(code, 400, "never to itself");
    let (code, _) = link(&app, &t, &svc, "hosts", &db).await;
    assert_eq!(code, 400, "a kind of link this build knows");

    let map = |center: &str, query: &str| {
        app.auth_get(
            &kpath(&t.tenant_id, &format!("/map/{center}{query}")),
            owner,
        )
    };
    let (code, one) = call(map(&svc, "")).await;
    assert_eq!(code, 200, "{one}");
    assert_eq!(ids_of(&one, "entities"), sorted(&[&svc, &db, &ns]));
    assert_eq!(one["edges"].as_array().unwrap().len(), 3, "{one}");
    let (_, two) = call(map(&svc, "?depth=2")).await;
    assert_eq!(ids_of(&two, "entities"), sorted(&[&svc, &db, &ns, &cl]));
    assert_eq!(two["edges"].as_array().unwrap().len(), 4, "{two}");
    // Both ways: from the cluster, two steps in.
    let (_, up) = call(map(&cl, "?depth=2")).await;
    assert_eq!(ids_of(&up, "entities"), sorted(&[&cl, &ns, &svc]));
    // Filtered: the centre and the databases.
    let (_, only) = call(map(&svc, "?depth=2&kind=database")).await;
    assert_eq!(ids_of(&only, "entities"), sorted(&[&svc, &db]));
    let (code, _) = call(map(&svc, "?depth=3")).await;
    assert_eq!(code, 400);

    // A card's links, each way.
    let (_, card) = call(app.auth_get(&kpath(&t.tenant_id, &format!("/entity/{db}")), owner)).await;
    let links = card["links"].as_array().unwrap();
    assert_eq!(links.len(), 2, "{card}");
    assert!(
        links
            .iter()
            .all(|l| l["direction"] == json!("in") && l["other"]["id"] == json!(svc)),
        "{card}"
    );

    // Archive the namespace: its two links go with it, at the rev it was read.
    let gone = kpath(&t.tenant_id, &format!("/entity/{ns}?rev={}", ns_rev + 7));
    let (code, _) = call(app.auth_delete(&gone, owner)).await;
    assert_eq!(code, 409, "not at a rev it never had");
    let gone = kpath(&t.tenant_id, &format!("/entity/{ns}?rev={ns_rev}"));
    let (code, body) = call(app.auth_delete(&gone, owner)).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["links_archived"], json!(2), "{body}");
    let (code, _) = call(app.auth_delete(&gone, owner)).await;
    assert_eq!(code, 404, "archived once");
    let (_, after) = call(map(&svc, "?depth=2")).await;
    assert_eq!(ids_of(&after, "entities"), sorted(&[&svc, &db]));
    let (code, _) = call(app.auth_get(&kpath(&t.tenant_id, &format!("/entity/{ns}")), owner)).await;
    assert_eq!(code, 404, "off the map");
    let (code, _) = link(&app, &t, &svc, "part_of", &ns).await;
    assert_eq!(code, 404, "nothing links to an archived entity");
    // Its key is free again.
    let (code, _) = create(
        &app,
        &t,
        owner,
        json!({ "kind": "namespace", "key": "prod/roomler-ai", "name": "again" }),
    )
    .await;
    assert_eq!(code, 201);

    // The history keeps all of it: the archive and both links it took.
    let (_, hist) = call(app.auth_get(
        &kpath(&t.tenant_id, &format!("/history?entity={ns}")),
        owner,
    ))
    .await;
    let rows = hist["history"].as_array().unwrap();
    let archives: Vec<(&str, &str)> = rows
        .iter()
        .filter(|r| r["op"] == json!("archive"))
        .map(|r| {
            (
                r["target"].as_str().unwrap(),
                r["set"]["cause"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(archives.len(), 3, "{hist}");
    assert_eq!(
        archives
            .iter()
            .filter(|(t, c)| *t == "edge" && *c == "entity_archived")
            .count(),
        2,
        "{hist}"
    );
    assert!(archives.contains(&("entity", "")), "{hist}");
    assert_eq!(
        rows.iter().filter(|r| r["op"] == json!("create")).count(),
        3,
        "the namespace and its two links: {hist}"
    );

    // A link's attributes change at its rev; its archive at its rev.
    let (code, edge) = link(&app, &t, &svc, "depends_on", &cl).await;
    assert_eq!(code, 201);
    let eid = edge["id"].as_str().unwrap();
    let epath = kpath(&t.tenant_id, &format!("/edge/{eid}"));
    let (code, changed) =
        call(patch(&app, &epath, owner).json(&json!({ "rev": 1, "attrs": { "via": "forward" } })))
            .await;
    assert_eq!((code, changed["rev"].clone()), (200, json!(2)), "{changed}");
    let (code, _) = call(patch(&app, &epath, owner).json(&json!({ "rev": 1, "attrs": {} }))).await;
    assert_eq!(code, 409);
    let (code, _) = call(app.auth_delete(&format!("{epath}?rev=2"), owner)).await;
    assert_eq!(code, 200);
    let (_, status) = call(app.auth_get(&kpath(&t.tenant_id, ""), owner)).await;
    assert_eq!(
        (status["entities"].clone(), status["edges"].clone()),
        (json!(4), json!(2)),
        "{status}"
    );
}

/// Every id resolves within the organization: another's entity answers as a
/// bogus id does, on every route that names one.
#[tokio::test]
async fn an_id_of_another_organization_answers_as_a_bogus_one() {
    let app = knowhow_app().await;
    let a = app.seed_tenant("knowa").await;
    let b = app.seed_tenant("knowb").await;
    let (theirs, rev) = entity(&app, &a, "database", "theirs").await;
    let (mine, _) = entity(&app, &b, "service", "mine").await;
    let tok = &b.admin.access_token;
    for path in [format!("/entity/{theirs}"), format!("/map/{theirs}")] {
        let (code, body) = call(app.auth_get(&kpath(&b.tenant_id, &path), tok)).await;
        assert_eq!(code, 404, "{path}: {body}");
    }
    let (code, _) = call(
        patch(
            &app,
            &kpath(&b.tenant_id, &format!("/entity/{theirs}")),
            tok,
        )
        .json(&json!({ "rev": rev, "name": "mine now" })),
    )
    .await;
    assert_eq!(code, 404);
    let (code, _) = call(app.auth_delete(
        &kpath(&b.tenant_id, &format!("/entity/{theirs}?rev={rev}")),
        tok,
    ))
    .await;
    assert_eq!(code, 404);
    let (code, _) = link(&app, &b, &mine, "reads", &theirs).await;
    assert_eq!(code, 404, "no link into another organization");
    let (_, hist) = call(app.auth_get(
        &kpath(&b.tenant_id, &format!("/history?entity={theirs}")),
        tok,
    ))
    .await;
    assert_eq!(hist["history"], json!([]), "{hist}");
    // And theirs is as it was.
    let (_, card) = call(app.auth_get(
        &kpath(&a.tenant_id, &format!("/entity/{theirs}")),
        &a.admin.access_token,
    ))
    .await;
    assert_eq!(
        (
            card["entity"]["name"].clone(),
            card["entity"]["rev"].clone()
        ),
        (json!("theirs"), json!(rev))
    );
}

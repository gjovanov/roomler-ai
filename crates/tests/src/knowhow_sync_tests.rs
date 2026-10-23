// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-2a — the fleet's devices, mirrored onto the map.
//!
//! What these lock: a device's hello puts its machine on the map, keyed by
//! its node id and said by the sync; the fleet's rename, tags and owner reach
//! it; what the fleet says of a machine is never a person's to change, while
//! a person's own description of it survives every sync; a removed device's
//! machine leaves the map with its links; the reconcile mirrors every device,
//! catches what no hook carries, retires a machine whose device is gone, and
//! changes nothing the second time; and an organization knowhow does not
//! serve has no machines.

use std::time::Duration;

use bson::{Document, doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::agent_presence_tests::connect_agent;
use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::knowhow_tests::{call, create, entity, knowhow_app, kpath, link, patch};
use crate::vault_lease_tests::enroll;

/// The machine a device is on the map, as its card: `None` while there is
/// none.
pub(crate) async fn machine(app: &TestApp, t: &SeededTenant, agent_id: &str) -> Option<Value> {
    let (code, card) = call(app.auth_get(
        &kpath(
            &t.tenant_id,
            &format!("/entity?kind=machine&key={agent_id}"),
        ),
        &t.admin.access_token,
    ))
    .await;
    (code == 200).then_some(card)
}

/// Wait until the machine's card satisfies `ready` (the hello's sync runs
/// off the socket's path).
pub(crate) async fn wait_machine(
    app: &TestApp,
    t: &SeededTenant,
    agent_id: &str,
    ready: impl Fn(&Value) -> bool,
) -> Value {
    for _ in 0..100 {
        if let Some(card) = machine(app, t, agent_id).await
            && ready(&card)
        {
            return card;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "the machine of {agent_id} never got there: {:?}",
        machine(app, t, agent_id).await
    );
}

pub(crate) fn fact(card: &Value, path: &str) -> Value {
    card["entity"]["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == json!(path))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Change the device the fleet's way (`PUT …/agent/{id}`), as its owner.
pub(crate) async fn update_device(app: &TestApp, t: &SeededTenant, agent_id: &str, body: Value) {
    let (code, resp) = call(
        app.auth_put(
            &format!("/api/tenant/{}/agent/{agent_id}", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&body),
    )
    .await;
    assert_eq!(code, 200, "{body} → {resp}");
}

pub(crate) fn knowhow(app: &TestApp) -> &roomler_ai_mod_knowhow::KnowhowState {
    app.state
        .modules
        .knowhow
        .as_ref()
        .expect("knowhow is mounted")
}

/// A device's hello puts its machine on the map: keyed by its node id,
/// named by its label, owned by its owner, and said by the sync.
#[tokio::test]
async fn a_devices_hello_puts_its_machine_on_the_map() {
    let app = knowhow_app().await;
    let t = app.seed_tenant("knowsynchello").await;
    let (agent_id, token) = enroll(&app, &t, "mach-knowsync-a", "linux").await;
    assert!(
        machine(&app, &t, &agent_id).await.is_none(),
        "no hello, no machine yet"
    );
    let _ws = connect_agent(&app, &token).await;
    let card = wait_machine(&app, &t, &agent_id, |_| true).await;
    let e = &card["entity"];
    assert_eq!(
        (
            e["kind"].clone(),
            e["key"].clone(),
            e["source"].clone(),
            e["status"].clone()
        ),
        (
            json!("machine"),
            json!(agent_id),
            json!("sync"),
            json!("active")
        ),
        "{card}"
    );
    assert_eq!(
        e["owner_id"],
        json!(t.admin.id),
        "the device's owner: {card}"
    );
    assert!(e.get("created_by").is_none(), "nobody wrote it: {card}");
    assert_eq!(fact(&card, "os")["value"], json!("linux"), "{card}");
    for path in ["machine_name", "version", "ephemeral", "tags"] {
        let f = fact(&card, path);
        assert_eq!(f["source"], json!("sync"), "{path}: {card}");
        assert!(
            f.get("by").is_none(),
            "{path} has no person behind it: {card}"
        );
    }
    // Its label is the device list's.
    let (_, device) = call(app.auth_get(
        &format!("/api/tenant/{}/agent/{agent_id}", t.tenant_id),
        &t.admin.access_token,
    ))
    .await;
    let label = device["display_name"]
        .as_str()
        .filter(|d| !d.is_empty())
        .or(device["name"].as_str())
        .unwrap()
        .to_string();
    assert_eq!(e["name"], json!(label), "{device}");

    // The history says the sync made it.
    let (_, hist) = call(app.auth_get(
        &kpath(
            &t.tenant_id,
            &format!("/history?entity={}", e["id"].as_str().unwrap()),
        ),
        &t.admin.access_token,
    ))
    .await;
    let rows = hist["history"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{hist}");
    assert_eq!(
        (rows[0]["op"].clone(), rows[0]["source"].clone()),
        (json!("create"), json!("sync"))
    );
    assert!(rows[0].get("by").is_none(), "{hist}");
}

/// The fleet's rename, tags and owner reach the machine through its hooks;
/// a display name, which no hook carries, through the reconcile.
#[tokio::test]
async fn the_fleets_changes_reach_the_machine() {
    let app = knowhow_app().await;
    let t = app.seed_tenant("knowsyncchange").await;
    let (agent_id, token) = enroll(&app, &t, "mach-knowsync-b", "windows").await;
    let _ws = connect_agent(&app, &token).await;
    wait_machine(&app, &t, &agent_id, |_| true).await;

    update_device(&app, &t, &agent_id, json!({ "name": "renamed-box" })).await;
    let card = machine(&app, &t, &agent_id).await.unwrap();
    assert_eq!(card["entity"]["name"], json!("renamed-box"), "{card}");
    assert_eq!(fact(&card, "machine_name")["value"], json!("renamed-box"));

    update_device(&app, &t, &agent_id, json!({ "tags": ["ci", "eu"] })).await;
    let card = machine(&app, &t, &agent_id).await.unwrap();
    assert_eq!(fact(&card, "tags")["value"], json!(["ci", "eu"]), "{card}");

    update_device(&app, &t, &agent_id, json!({ "owner_user_id": t.member.id })).await;
    let card = machine(&app, &t, &agent_id).await.unwrap();
    assert_eq!(card["entity"]["owner_id"], json!(t.member.id), "{card}");

    // A display name reaches no hook: the reconcile carries it.
    update_device(&app, &t, &agent_id, json!({ "display_name": "Build box" })).await;
    assert_eq!(
        machine(&app, &t, &agent_id).await.unwrap()["entity"]["name"],
        json!("renamed-box"),
        "not yet"
    );
    roomler_ai_mod_knowhow::sync::all(knowhow(&app))
        .await
        .unwrap();
    let card = machine(&app, &t, &agent_id).await.unwrap();
    assert_eq!(card["entity"]["name"], json!("Build box"), "{card}");
    assert_eq!(
        fact(&card, "machine_name")["value"],
        json!("renamed-box"),
        "the machine's own name, beside its label"
    );
}

/// What the fleet says of a machine is the fleet's: its name, its owner,
/// the synced facts, and its leaving the map. A person's own description of
/// it survives every sync.
#[tokio::test]
async fn what_the_fleet_says_of_a_machine_is_the_fleets() {
    let app = knowhow_app().await;
    let t = app.seed_tenant("knowsyncowns").await;
    let (agent_id, token) = enroll(&app, &t, "mach-knowsync-c", "macos").await;
    let _ws = connect_agent(&app, &token).await;
    let card = wait_machine(&app, &t, &agent_id, |_| true).await;
    let id = card["entity"]["id"].as_str().unwrap().to_string();
    let path = kpath(&t.tenant_id, &format!("/entity/{id}"));
    let owner = &t.admin.access_token;
    let rev = || async {
        machine(&app, &t, &agent_id).await.unwrap()["entity"]["rev"]
            .as_i64()
            .unwrap()
    };

    for body in [
        json!({ "facts": { "os": "windows" } }),
        json!({ "facts": { "tags": [] } }),
        json!({ "name": "mine now" }),
        json!({ "owner_id": t.member.id }),
    ] {
        let mut b = body.clone();
        b["rev"] = json!(rev().await);
        let (code, resp) = call(patch(&app, &path, owner).json(&b)).await;
        assert_eq!(code, 400, "{body} → {resp}");
        assert!(
            resp["message"].as_str().unwrap().contains("the fleet says"),
            "{resp}"
        );
    }
    let (code, resp) = call(app.auth_delete(&format!("{path}?rev={}", rev().await), owner)).await;
    assert_eq!(
        code, 400,
        "a person does not take a machine off the map: {resp}"
    );

    // A person's own description: an environment and a fact of their own.
    let b = json!({ "rev": rev().await, "env": "prod", "facts": { "role": "build" } });
    let (code, resp) = call(patch(&app, &path, owner).json(&b)).await;
    assert_eq!(code, 200, "{resp}");
    // The fleet changes the device; its sync leaves the person's part alone.
    update_device(&app, &t, &agent_id, json!({ "tags": ["gpu"] })).await;
    let card = machine(&app, &t, &agent_id).await.unwrap();
    assert_eq!(card["entity"]["env"], json!("prod"), "{card}");
    let role = fact(&card, "role");
    assert_eq!(
        (
            role["value"].clone(),
            role["source"].clone(),
            role["by"].clone()
        ),
        (json!("build"), json!("person"), json!(t.admin.id)),
        "{card}"
    );
    assert_eq!(fact(&card, "tags")["value"], json!(["gpu"]));
    assert_eq!(fact(&card, "os")["value"], json!("macos"));
}

/// A removed device's machine leaves the map, and its links with it, the
/// sync's archive in the history.
#[tokio::test]
async fn a_removed_devices_machine_leaves_the_map_with_its_links() {
    let app = knowhow_app().await;
    let t = app.seed_tenant("knowsyncgone").await;
    let (agent_id, token) = enroll(&app, &t, "mach-knowsync-d", "linux").await;
    let ws = connect_agent(&app, &token).await;
    let card = wait_machine(&app, &t, &agent_id, |_| true).await;
    let machine_id = card["entity"]["id"].as_str().unwrap().to_string();
    let (svc, _) = entity(&app, &t, "service", "api").await;
    let (code, _) = link(&app, &t, &svc, "runs_on", &machine_id).await;
    assert_eq!(code, 201);
    drop(ws);

    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/agent/{agent_id}", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    assert!(machine(&app, &t, &agent_id).await.is_none(), "off the map");
    let (_, svc_card) = call(app.auth_get(
        &kpath(&t.tenant_id, &format!("/entity/{svc}")),
        &t.admin.access_token,
    ))
    .await;
    assert_eq!(
        svc_card["links"],
        json!([]),
        "the link went with it: {svc_card}"
    );

    let (_, hist) = call(app.auth_get(
        &kpath(&t.tenant_id, &format!("/history?entity={machine_id}")),
        &t.admin.access_token,
    ))
    .await;
    let archives: Vec<(Value, Value, Value)> = hist["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["op"] == json!("archive"))
        .map(|r| {
            (
                r["target"].clone(),
                r["source"].clone(),
                r["set"]["cause"].clone(),
            )
        })
        .collect();
    assert!(
        archives.contains(&(json!("entity"), json!("sync"), Value::Null)),
        "{hist}"
    );
    assert!(
        archives.contains(&(json!("edge"), json!("sync"), json!("entity_archived"))),
        "{hist}"
    );
}

/// The reconcile mirrors every live device, retires a machine whose device is
/// gone, and the second time changes nothing.
#[tokio::test]
async fn the_reconcile_mirrors_every_device_and_retires_the_gone() {
    let app = knowhow_app().await;
    let t = app.seed_tenant("knowsyncall").await;
    // Two devices that never said hello: no machine yet.
    let (a, _) = enroll(&app, &t, "mach-knowsync-e", "linux").await;
    let (b, _) = enroll(&app, &t, "mach-knowsync-f", "windows").await;
    // A machine whose device went while knowhow was not looking.
    let tid = ObjectId::parse_str(&t.tenant_id).unwrap();
    let gone = ObjectId::new().to_hex();
    let now = bson::DateTime::now();
    app.db
        .collection::<Document>("knowhow_entities")
        .insert_one(doc! {
            "tenant_id": tid, "kind": "machine", "key": &gone, "name": "old box",
            "env": null, "owner_id": null, "attrs": {}, "facts": [],
            "status": "active", "source": "sync", "rev": 1_i64,
            "created_by": null, "created_at": now, "updated_by": null, "updated_at": now,
            "deleted_at": null,
        })
        .await
        .unwrap();

    let r = roomler_ai_mod_knowhow::sync::all(knowhow(&app))
        .await
        .unwrap();
    assert_eq!((r.created, r.retired, r.failed), (2, 1, 0), "{r:?}");
    for id in [&a, &b] {
        assert!(machine(&app, &t, id).await.is_some(), "{id}");
    }
    assert!(machine(&app, &t, &gone).await.is_none(), "retired");

    let again = roomler_ai_mod_knowhow::sync::all(knowhow(&app))
        .await
        .unwrap();
    assert_eq!(
        (again.created, again.updated, again.retired, again.failed),
        (0, 0, 0, 0),
        "{again:?}"
    );
}

/// An organization knowhow does not serve has no machines, whatever its
/// devices say.
#[tokio::test]
async fn an_unserved_organizations_devices_are_not_mirrored() {
    let first = knowhow_app().await;
    let a = first.seed_tenant("knowsyncserved").await;
    let b = first.seed_tenant("knowsyncunserved").await;
    let db = first.db.name().to_string();
    let only_a = a.tenant_id.clone();
    let app = TestApp::spawn_with_settings(move |s| {
        s.database.name = db;
        s.knowhow.tenants = only_a;
    })
    .await;
    let (in_a, token_a) = enroll(&app, &a, "mach-knowsync-g", "linux").await;
    let (in_b, token_b) = enroll(&app, &b, "mach-knowsync-h", "linux").await;
    let _wa = connect_agent(&app, &token_a).await;
    let _wb = connect_agent(&app, &token_b).await;
    wait_machine(&app, &a, &in_a, |_| true).await;
    roomler_ai_mod_knowhow::sync::all(knowhow(&app))
        .await
        .unwrap();
    let held = app
        .db
        .collection::<Document>("knowhow_entities")
        .count_documents(doc! { "tenant_id": ObjectId::parse_str(&b.tenant_id).unwrap() })
        .await
        .unwrap();
    assert_eq!(held, 0, "no machine of {in_b} in an unserved organization");
    let (code, _) = create(
        &app,
        &b,
        &b.admin.access_token,
        json!({ "kind": "service", "key": "x", "name": "x" }),
    )
    .await;
    assert_eq!(code, 404, "and no map at all");
}

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-3a-2 — what a scan writes (decision 42).
//!
//! What these lock: a scan writes what lives on its device, its checkouts
//! and listeners keyed by its node id and its machine's declared tunnel
//! routes, makes the repositories and clusters they reach bare, and links
//! them; a later complete scan archives what it no longer reports while a
//! truncated or failed one archives nothing, and a `no_roots` answer all of
//! it, never a shared entity; what a scan says is the device's, while a
//! person's own description survives and a shared entity stays a person's
//! to describe; a device describes only itself; and a removed device's scan
//! part leaves the map.

use bson::oid::ObjectId;
use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::knowhow_scan_tests::{Ws, answer, ask, hello, scan_app, scan_sent, wait_scan};
use crate::knowhow_sync_tests::fact;
use crate::knowhow_tests::{call, kpath, patch};
use crate::vault_lease_tests::enroll;
use roomler_ai_mod_knowhow::intake::checkout_key;

/// The live entity of `kind` and `key`, as its card: `None` while there is
/// none.
pub(crate) async fn card(app: &TestApp, t: &SeededTenant, kind: &str, key: &str) -> Option<Value> {
    let (code, card) = call(app.auth_get(
        &kpath(&t.tenant_id, &format!("/entity?kind={kind}&key={key}")),
        &t.admin.access_token,
    ))
    .await;
    (code == 200).then_some(card)
}

/// The kinds and other ends' keys of a card's links, as `kind:direction:key`.
fn links(card: &Value) -> Vec<String> {
    let mut out: Vec<String> = card["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            format!(
                "{}:{}:{}",
                l["edge"]["kind"].as_str().unwrap(),
                l["direction"].as_str().unwrap(),
                l["other"]["key"].as_str().unwrap()
            )
        })
        .collect();
    out.sort();
    out
}

/// A device connected to scan, its id and socket.
async fn scanner(app: &TestApp, t: &SeededTenant, machine: &str) -> (String, Ws) {
    let (device, token) = enroll(app, t, machine, "linux").await;
    let ws = hello(app, &token, &["knowhow-scan"], "self").await;
    (device, ws)
}

/// Ask `device` to scan and answer `body` for it; the scan once it ended.
async fn scan(app: &TestApp, t: &SeededTenant, device: &str, ws: &mut Ws, body: Value) -> Value {
    let (code, asked) = ask(app, t, &t.admin.access_token, device).await;
    assert_eq!(code, 202, "{asked}");
    let id = scan_sent(ws).await;
    let mut body = body;
    body["scan_id"] = json!(id);
    let ends = body["outcome"].as_str().unwrap_or("failed").to_string();
    answer(ws, body).await;
    // Answered, and, unless it failed, what it wrote stamped on it: the
    // intake runs after the answer took.
    wait_scan(app, t, device, |s| {
        s["id"] == json!(id)
            && s["state"] == json!(ends)
            && (ends == "failed" || s.get("written").is_some())
    })
    .await
}

fn checkout(path: &str, remotes: &[(&str, &str)]) -> Value {
    let remotes: Vec<Value> = remotes
        .iter()
        .map(|(n, u)| json!({ "name": n, "url": u }))
        .collect();
    json!({ "kind": "checkout", "path": path, "remotes": remotes, "branch": "main",
            "root_commit": "0123456789abcdef".repeat(3)[..40].to_string(),
            "services": [{ "name": "api", "image": "ghcr.io/acme/api:1.2" }] })
}

/// A scan writes what lives on its device and links it: its checkout and
/// listener keyed by its node id, its machine's declared routes, and the
/// repositories and cluster they reach, made bare.
#[tokio::test]
async fn a_scan_writes_what_lives_on_its_device() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowintakeok").await;
    let (device, mut ws) = scanner(&app, &t, "mach-knowintake-a").await;
    let did = ObjectId::parse_str(&device).unwrap();
    let node = ObjectId::new().to_hex();
    let s = scan(
        &app,
        &t,
        &device,
        &mut ws,
        json!({
            "outcome": "done",
            "findings": [
                checkout("/home/dev/src/api", &[("origin", "git@github.com:acme/api.git"),
                                               ("upstream", "https://github.com/up/api.git")]),
                { "kind": "context", "name": "prod", "server": "https://k8s.example.com:6443",
                  "namespace": "api", "ca_sha256": "ab".repeat(32) },
                { "kind": "listener", "port": 5432, "protocol": "tcp", "address": "0.0.0.0",
                  "process": "postgres" },
                { "kind": "tunnel_route", "id": "db", "flow": "forward", "local": 15432,
                  "node": node, "remote": "10.0.4.12:5432" },
            ],
        }),
    )
    .await;
    assert_eq!(
        s["written"],
        json!({ "checkouts": 1, "endpoints": 1, "repos": 2, "clusters": 1, "links": 5,
                "retired": 0, "full": false }),
        "{s}"
    );

    // The checkout: the device's, its owner's, every fact the scan's.
    let key = checkout_key(did, "/home/dev/src/api");
    let c = card(&app, &t, "checkout", &key)
        .await
        .expect("its checkout");
    let e = &c["entity"];
    assert_eq!(
        (
            e["source"].clone(),
            e["device"].clone(),
            e["owner_id"].clone()
        ),
        (json!("scan"), json!(device), json!(t.admin.id)),
        "{c}"
    );
    assert!(e["name"].as_str().unwrap().starts_with("api on "), "{c}");
    for path in ["branch", "path", "remotes", "root_commit", "services"] {
        let f = fact(&c, path);
        assert_eq!(
            (f["source"].clone(), f["device"].clone()),
            (json!("scan"), json!(device)),
            "{path}: {c}"
        );
        assert!(f.get("by").is_none(), "{path} has no person behind it");
    }
    assert_eq!(fact(&c, "path")["value"], json!("/home/dev/src/api"));
    assert_eq!(
        links(&c),
        [
            format!("checked_out_at:out:{device}"),
            "part_of:out:github.com/acme/api".to_string(),
            "part_of:out:github.com/up/api".to_string(),
        ]
    );
    let origin = c["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["other"]["key"] == json!("github.com/acme/api"))
        .unwrap();
    assert_eq!(origin["edge"]["attrs"], json!({ "remote": "origin" }));
    assert_eq!(origin["edge"]["device"], json!(device));

    // The repositories and the cluster: shared, bare, no device's.
    let repo = card(&app, &t, "repo", "github.com/acme/api").await.unwrap();
    assert_eq!(repo["entity"]["name"], json!("acme/api"));
    assert_eq!(repo["entity"]["source"], json!("scan"));
    assert!(repo["entity"].get("device").is_none(), "shared: {repo}");
    assert_eq!(repo["entity"]["facts"], json!([]));
    let cluster = card(&app, &t, "cluster", "k8s.example.com:6443")
        .await
        .unwrap();
    assert_eq!(cluster["entity"]["name"], json!("prod"));
    assert!(cluster["entity"].get("device").is_none());
    let via = &cluster["links"][0];
    assert_eq!(via["edge"]["kind"], json!("reachable_via"));
    assert_eq!(
        via["edge"]["attrs"],
        json!({ "contexts": [{ "name": "prod", "namespace": "api", "ca_sha256": "ab".repeat(32) }] })
    );

    // The listener, and the machine's own scan fact.
    let ep = card(&app, &t, "endpoint", &format!("{device}:5432/tcp"))
        .await
        .unwrap();
    assert_eq!(fact(&ep, "process")["value"], json!("postgres"));
    assert_eq!(fact(&ep, "port")["value"], json!(5432));
    assert_eq!(links(&ep), [format!("runs_on:out:{device}")]);
    let m = card(&app, &t, "machine", &device).await.unwrap();
    let declared = fact(&m, "tunnel.declared");
    assert_eq!(
        (declared["source"].clone(), declared["device"].clone()),
        (json!("scan"), json!(device))
    );
    assert_eq!(
        declared["value"],
        json!([{ "id": "db", "flow": "forward", "local": 15432, "node": node,
                 "enabled": true, "remote": "10.0.4.12:5432" }])
    );
}

/// A later complete scan archives what the device's earlier scans wrote and
/// it no longer reports, never a shared entity; a truncated or failed one
/// archives nothing; a `no_roots` answer archives all of it.
#[tokio::test]
async fn a_later_scan_replaces_its_devices_part_and_never_a_shared_one() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowintakeagain").await;
    let (device, mut ws) = scanner(&app, &t, "mach-knowintake-b").await;
    let did = ObjectId::parse_str(&device).unwrap();
    let (a, b) = (checkout_key(did, "/src/a"), checkout_key(did, "/src/b"));
    scan(
        &app,
        &t,
        &device,
        &mut ws,
        json!({ "outcome": "done", "findings": [
            checkout("/src/a", &[("origin", "git@github.com:acme/api.git")]),
            { "kind": "listener", "port": 22, "protocol": "tcp", "address": "0.0.0.0" },
            { "kind": "context", "name": "prod", "server": "https://k8s.example.com:6443" },
            { "kind": "tunnel_route", "id": "db", "flow": "socks5", "local": 1080,
              "node": ObjectId::new().to_hex() },
        ] }),
    )
    .await;

    let s = scan(
        &app,
        &t,
        &device,
        &mut ws,
        json!({ "outcome": "done", "findings": [
            checkout("/src/b", &[("origin", "git@github.com:acme/web.git")]),
        ] }),
    )
    .await;
    assert_eq!(
        s["written"]["retired"],
        json!(3),
        "a, the listener, the cluster's link: {s}"
    );
    assert!(card(&app, &t, "checkout", &a).await.is_none(), "a is gone");
    assert!(
        card(&app, &t, "endpoint", &format!("{device}:22/tcp"))
            .await
            .is_none()
    );
    assert!(card(&app, &t, "checkout", &b).await.is_some());
    let repo = card(&app, &t, "repo", "github.com/acme/api").await;
    assert!(repo.is_some(), "a shared repository stays");
    assert_eq!(repo.unwrap()["links"], json!([]), "its link went with a");
    let cluster = card(&app, &t, "cluster", "k8s.example.com:6443")
        .await
        .unwrap();
    assert_eq!(
        cluster["links"],
        json!([]),
        "the cluster stays, its link does not"
    );
    let m = card(&app, &t, "machine", &device).await.unwrap();
    assert_eq!(
        fact(&m, "tunnel.declared"),
        Value::Null,
        "no routes declared now"
    );

    // Truncated, or failed: nothing archived.
    let s = scan(
        &app,
        &t,
        &device,
        &mut ws,
        json!({ "outcome": "done", "truncated": true, "findings": [] }),
    )
    .await;
    assert_eq!(s["written"]["retired"], json!(0), "{s}");
    scan(
        &app,
        &t,
        &device,
        &mut ws,
        json!({ "outcome": "failed", "detail": "git: not found" }),
    )
    .await;
    assert!(card(&app, &t, "checkout", &b).await.is_some(), "b stays");

    // Nothing listed any more: the device's part goes.
    let s = scan(&app, &t, &device, &mut ws, json!({ "outcome": "no_roots" })).await;
    assert_eq!(s["written"]["retired"], json!(1), "{s}");
    assert!(card(&app, &t, "checkout", &b).await.is_none());
    assert!(
        card(&app, &t, "repo", "github.com/acme/web")
            .await
            .is_some()
    );
}

/// What a scan says is the device's: a person may describe its checkout and
/// keep that through every scan, never change or archive what the scan
/// said, nor its machine's scan fact. A shared entity stays a person's to
/// describe, and a scan never changes it again.
#[tokio::test]
async fn what_a_scan_says_is_the_devices() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowintakeowns").await;
    let (device, mut ws) = scanner(&app, &t, "mach-knowintake-c").await;
    let did = ObjectId::parse_str(&device).unwrap();
    let body = json!({ "outcome": "done", "findings": [
        checkout("/src/api", &[("origin", "git@github.com:acme/api.git")]),
        { "kind": "tunnel_route", "id": "db", "flow": "socks5", "local": 1080,
          "node": ObjectId::new().to_hex() },
    ] });
    scan(&app, &t, &device, &mut ws, body.clone()).await;
    let key = checkout_key(did, "/src/api");
    let owner = &t.admin.access_token;
    let rev_of = |c: &Value| c["entity"]["rev"].as_i64().unwrap();

    let c = card(&app, &t, "checkout", &key).await.unwrap();
    let path = kpath(
        &t.tenant_id,
        &format!("/entity/{}", c["entity"]["id"].as_str().unwrap()),
    );
    for change in [
        json!({ "facts": { "path": "/elsewhere" } }),
        json!({ "facts": { "branch": "dev" } }),
        json!({ "name": "mine now" }),
    ] {
        let mut b = change.clone();
        let fresh = card(&app, &t, "checkout", &key).await.unwrap();
        b["rev"] = json!(rev_of(&fresh));
        let (code, resp) = call(patch(&app, &path, owner).json(&b)).await;
        assert_eq!(code, 400, "{change} → {resp}");
        assert!(
            resp["message"]
                .as_str()
                .unwrap()
                .contains("its device's scan says"),
            "{resp}"
        );
    }
    let fresh = card(&app, &t, "checkout", &key).await.unwrap();
    let (code, resp) =
        call(app.auth_delete(&format!("{path}?rev={}", rev_of(&fresh)), owner)).await;
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["message"]
            .as_str()
            .unwrap()
            .contains("no longer report"),
        "{resp}"
    );

    // The machine's scan fact is the device's too.
    let m = card(&app, &t, "machine", &device).await.unwrap();
    let mpath = kpath(
        &t.tenant_id,
        &format!("/entity/{}", m["entity"]["id"].as_str().unwrap()),
    );
    let b = json!({ "rev": rev_of(&m), "facts": { "tunnel.declared": [] } });
    let (code, resp) = call(patch(&app, &mpath, owner).json(&b)).await;
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["message"]
            .as_str()
            .unwrap()
            .contains("its device's scan says"),
        "{resp}"
    );

    // A person's own description of the checkout survives the next scan.
    let b = json!({ "rev": rev_of(&fresh), "env": "prod", "facts": { "note": "the main one" } });
    let (code, resp) = call(patch(&app, &path, owner).json(&b)).await;
    assert_eq!(code, 200, "{resp}");
    // And a shared repository is a person's to describe.
    let repo = card(&app, &t, "repo", "github.com/acme/api").await.unwrap();
    let rpath = kpath(
        &t.tenant_id,
        &format!("/entity/{}", repo["entity"]["id"].as_str().unwrap()),
    );
    let b = json!({ "rev": rev_of(&repo), "name": "Acme API", "facts": { "lang": "rust" } });
    let (code, resp) = call(patch(&app, &rpath, owner).json(&b)).await;
    assert_eq!(code, 200, "{resp}");

    scan(&app, &t, &device, &mut ws, body).await;
    let c = card(&app, &t, "checkout", &key).await.unwrap();
    assert_eq!(c["entity"]["env"], json!("prod"), "{c}");
    let note = fact(&c, "note");
    assert_eq!(
        (note["value"].clone(), note["source"].clone()),
        (json!("the main one"), json!("person"))
    );
    let repo = card(&app, &t, "repo", "github.com/acme/api").await.unwrap();
    assert_eq!(
        repo["entity"]["name"],
        json!("Acme API"),
        "a scan never changes it"
    );
    assert_eq!(fact(&repo, "lang")["value"], json!("rust"));
}

/// A device describes only itself: another device's scan of the same path
/// is its own checkout, and a key a person holds is left as it is.
#[tokio::test]
async fn a_device_describes_only_itself() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowintakeself").await;
    let (a, mut wa) = scanner(&app, &t, "mach-knowintake-d").await;
    let (b, mut wb) = scanner(&app, &t, "mach-knowintake-e").await;
    let (aid, bid) = (
        ObjectId::parse_str(&a).unwrap(),
        ObjectId::parse_str(&b).unwrap(),
    );
    let one = json!({ "outcome": "done", "findings": [
        checkout("/src/api", &[("origin", "git@github.com:acme/api.git")]),
    ] });
    scan(&app, &t, &a, &mut wa, one.clone()).await;

    // A person already holds the key b's scan would make for another path.
    let taken = checkout_key(bid, "/src/web");
    let (code, resp) = crate::knowhow_tests::create(
        &app,
        &t,
        &t.admin.access_token,
        json!({ "kind": "checkout", "key": taken, "name": "a person's" }),
    )
    .await;
    assert_eq!(code, 201, "{resp}");
    let two = json!({ "outcome": "done", "findings": [
        checkout("/src/api", &[("origin", "git@github.com:acme/api.git")]),
        checkout("/src/web", &[]),
    ] });
    let s = scan(&app, &t, &b, &mut wb, two).await;
    assert_eq!(
        s["written"]["checkouts"],
        json!(1),
        "the person's key is left: {s}"
    );
    assert_eq!(
        s["written"]["repos"],
        json!(0),
        "the repository was there already"
    );

    let ca = card(&app, &t, "checkout", &checkout_key(aid, "/src/api"))
        .await
        .unwrap();
    let cb = card(&app, &t, "checkout", &checkout_key(bid, "/src/api"))
        .await
        .unwrap();
    assert_ne!(ca["entity"]["id"], cb["entity"]["id"], "each its own");
    assert_eq!(ca["entity"]["device"], json!(a));
    assert_eq!(cb["entity"]["device"], json!(b));
    let person = card(&app, &t, "checkout", &taken).await.unwrap();
    assert_eq!(
        (
            person["entity"]["source"].clone(),
            person["entity"]["name"].clone()
        ),
        (json!("person"), json!("a person's")),
        "{person}"
    );
    // Both checkouts are part of the one repository.
    let repo = card(&app, &t, "repo", "github.com/acme/api").await.unwrap();
    assert_eq!(repo["links"].as_array().unwrap().len(), 2, "{repo}");
}

/// A removed device's scan part leaves the map with it; what it reached
/// stays.
#[tokio::test]
async fn a_removed_devices_scan_part_leaves_the_map() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowintakegone").await;
    let (device, mut ws) = scanner(&app, &t, "mach-knowintake-f").await;
    let did = ObjectId::parse_str(&device).unwrap();
    scan(
        &app,
        &t,
        &device,
        &mut ws,
        json!({ "outcome": "done", "findings": [
            checkout("/src/api", &[("origin", "git@github.com:acme/api.git")]),
            { "kind": "listener", "port": 8080, "protocol": "tcp", "address": "127.0.0.1" },
        ] }),
    )
    .await;
    drop(ws);
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/agent/{device}", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    assert!(
        card(&app, &t, "checkout", &checkout_key(did, "/src/api"))
            .await
            .is_none()
    );
    assert!(
        card(&app, &t, "endpoint", &format!("{device}:8080/tcp"))
            .await
            .is_none()
    );
    assert!(card(&app, &t, "machine", &device).await.is_none());
    let repo = card(&app, &t, "repo", "github.com/acme/api").await;
    assert!(repo.is_some(), "a shared repository stays");
}

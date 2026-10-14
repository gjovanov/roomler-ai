// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P3 — the install trace, against a real server (spec §3e).
//!
//! * **AC11** — a minted token is on record by its jti with `issued_at` and
//!   nothing else about it; the enroll that spends it stamps `redeemed_at`
//!   and `agent_id`; an unused one stays unredeemed; the token string is
//!   stored nowhere (the whole collection is scanned). A token minted before
//!   the ledger existed is recorded at its redemption from its own claims.
//! * **AC10** (the server half) — a fetch by each client class increments
//!   exactly its counter, the updater's User-Agent increments nothing, and
//!   no User-Agent and no IP survive into `growth_daily` (every row is
//!   scanned). The counters switch off by config.
//! * the enroll's `install`: stored as sent, `unknown` for the old shape,
//!   clamped to `other` for a spelling the server does not know, kept across
//!   a re-enroll that says nothing, and an unknown field never refuses the
//!   enroll (the compatibility both directions rely on).

use std::time::Duration;

use bson::{Bson, Document, doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::fixtures::{seed::SeededTenant, test_app::TestApp};

const PS51: &str = "Mozilla/5.0 (Windows NT; Windows NT 10.0; en-US) WindowsPowerShell/5.1.19041.1";
const CHROME_WIN: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36";

/// Mint a standard enrollment token: `{enrollment_token, jti, …}`.
async fn mint(app: &TestApp, t: &SeededTenant) -> Value {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "mint");
    resp.json().await.unwrap()
}

/// `POST /api/agent/enroll` with the pre-P3 five-field body plus `extra`
/// merged in — the raw response, so refusals are assertable too.
async fn enroll(app: &TestApp, token: &str, machine_id: &str, extra: Value) -> reqwest::Response {
    let mut body = json!({
        "enrollment_token": token,
        "machine_id": machine_id,
        "machine_name": format!("host-{machine_id}"),
        "os": "linux",
        "agent_version": "0.4.136",
    });
    if let (Some(obj), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    app.client
        .post(app.url("/api/agent/enroll"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// Mint and enroll in one go; the agent id.
async fn enroll_ok(app: &TestApp, t: &SeededTenant, machine_id: &str, extra: Value) -> String {
    let m = mint(app, t).await;
    let resp = enroll(
        app,
        m["enrollment_token"].as_str().unwrap(),
        machine_id,
        extra,
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200, "enroll {machine_id}");
    let v: Value = resp.json().await.unwrap();
    v["agent_id"].as_str().unwrap().to_string()
}

async fn ledger_row(app: &TestApp, jti: &str) -> Option<Document> {
    app.db
        .collection::<Document>("enroll_token_issued")
        .find_one(doc! { "_id": jti })
        .await
        .unwrap()
}

async fn agent_row(app: &TestApp, agent_id: &str) -> Document {
    app.db
        .collection::<Document>("agents")
        .find_one(doc! { "_id": ObjectId::parse_str(agent_id).unwrap() })
        .await
        .unwrap()
        .expect("agent row")
}

/// The device as the API shows it (`AgentResponse`).
async fn agent_view(app: &TestApp, t: &SeededTenant, agent_id: &str) -> Value {
    app.auth_get(
        &format!("/api/tenant/{}/agent/{agent_id}", t.tenant_id),
        &t.admin.access_token,
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap()
}

async fn all_docs(app: &TestApp, coll: &str) -> Vec<Document> {
    use futures::TryStreamExt;
    app.db
        .collection::<Document>(coll)
        .find(doc! {})
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap()
}

/// Poll `growth_daily` until `done` holds (the counter writes are spawned
/// off the request), for at most five seconds.
async fn wait_daily(app: &TestApp, done: impl Fn(&[Document]) -> bool) -> Vec<Document> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let rows = all_docs(app, "growth_daily").await;
        if done(&rows) || tokio::time::Instant::now() > deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Every key and every string value in a document, recursively — what a
/// "nothing of X in here" scan reads.
fn strings_of(doc: &Document) -> Vec<String> {
    fn walk(b: &Bson, out: &mut Vec<String>) {
        match b {
            Bson::String(s) => out.push(s.clone()),
            Bson::Document(d) => {
                for (k, v) in d {
                    out.push(k.clone());
                    walk(v, out);
                }
            }
            Bson::Array(a) => a.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(&Bson::Document(doc.clone()), &mut out);
    out
}

/// Every key in a document, recursively.
fn keys_of(doc: &Document) -> Vec<String> {
    fn walk(d: &Document, out: &mut Vec<String>) {
        for (k, v) in d {
            out.push(k.clone());
            if let Bson::Document(inner) = v {
                walk(inner, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(doc, &mut out);
    out
}

fn dim<'a>(row: &'a Document, key: &str) -> &'a str {
    row.get_document("dims")
        .expect("dims")
        .get_str(key)
        .unwrap_or("")
}

fn value_of(rows: &[Document], asset: &str, client: &str, os: &str) -> Option<i64> {
    rows.iter()
        .find(|r| dim(r, "asset") == asset && dim(r, "client") == client && dim(r, "os") == os)
        .map(|r| {
            r.get_i64("value")
                .unwrap_or_else(|_| i64::from(r.get_i32("value").unwrap()))
        })
}

// ── AC11: the token ledger ───────────────────────────────────────────────

#[tokio::test]
async fn an_issued_token_is_on_record_and_its_enroll_redeems_it() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("instr1").await;

    let minted = mint(&app, &t).await;
    let jti = minted["jti"].as_str().unwrap();
    let token = minted["enrollment_token"].as_str().unwrap();
    let row = ledger_row(&app, jti).await.expect("the mint is on record");
    assert_eq!(row.get_str("kind").unwrap(), "standard");
    assert_eq!(
        row.get_object_id("tenant_id").unwrap().to_hex(),
        t.tenant_id
    );
    assert_eq!(row.get_object_id("issued_by").unwrap().to_hex(), t.admin.id);
    assert!(row.get_datetime("issued_at").is_ok(), "{row:?}");
    assert_eq!(row.get("redeemed_at"), Some(&Bson::Null));
    assert_eq!(row.get("agent_id"), Some(&Bson::Null));
    let text = strings_of(&row).join("\n");
    assert!(!text.contains(token), "the token string is on the row");
    assert!(!text.contains("eyJ"), "a JWT is on the row: {text}");

    let resp = enroll(&app, token, "m-instr1", json!({})).await;
    assert_eq!(resp.status().as_u16(), 200);
    let agent_id = resp.json::<Value>().await.unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    let row = ledger_row(&app, jti).await.unwrap();
    assert!(row.get_datetime("redeemed_at").is_ok(), "{row:?}");
    assert_eq!(row.get_object_id("agent_id").unwrap().to_hex(), agent_id);
    assert!(
        row.get_datetime("issued_at").is_ok(),
        "the mint's own stamp survives"
    );

    // An unused token stays unredeemed.
    let unused = mint(&app, &t).await;
    let row = ledger_row(&app, unused["jti"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(row.get("redeemed_at"), Some(&Bson::Null));
    assert_eq!(row.get("agent_id"), Some(&Bson::Null));

    // The token string is nowhere in the collection — not as a value, not
    // as a key, in no row.
    let docs = all_docs(&app, "enroll_token_issued").await;
    assert_eq!(docs.len(), 2);
    for d in &docs {
        let text = strings_of(d).join("\n");
        assert!(!text.contains("eyJ") && !text.contains(token), "{text}");
        assert!(d.get("token").is_none() && d.get("enrollment_token").is_none());
    }
}

/// A token minted before the ledger existed has no row at mint time; its
/// redemption writes one from the claims it carries, so "issued → redeemed"
/// is complete across the deploy that introduced the ledger.
#[tokio::test]
async fn a_token_minted_before_the_ledger_is_recorded_at_its_redemption() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("instr3").await;
    let minted = mint(&app, &t).await;
    let jti = minted["jti"].as_str().unwrap();
    app.db
        .collection::<Document>("enroll_token_issued")
        .delete_one(doc! { "_id": jti })
        .await
        .unwrap();
    assert!(ledger_row(&app, jti).await.is_none());

    let before = bson::DateTime::now().timestamp_millis();
    let resp = enroll(
        &app,
        minted["enrollment_token"].as_str().unwrap(),
        "m-instr3",
        json!({}),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let agent_id = resp.json::<Value>().await.unwrap()["agent_id"]
        .as_str()
        .unwrap()
        .to_string();
    let row = ledger_row(&app, jti).await.expect("upserted at redemption");
    assert_eq!(row.get_str("kind").unwrap(), "standard");
    assert_eq!(
        row.get_object_id("tenant_id").unwrap().to_hex(),
        t.tenant_id
    );
    assert_eq!(row.get_object_id("issued_by").unwrap().to_hex(), t.admin.id);
    let issued = row.get_datetime("issued_at").unwrap().timestamp_millis();
    assert!(
        (before - 60_000..=before + 1_000).contains(&issued),
        "issued_at comes from the token's iat (seconds), not from now: {issued} vs {before}"
    );
    assert!(row.get_datetime("redeemed_at").is_ok());
    assert_eq!(row.get_object_id("agent_id").unwrap().to_hex(), agent_id);
}

// ── AC10: the fetch counters ─────────────────────────────────────────────

#[tokio::test]
async fn fetches_count_by_client_class_and_never_the_updater() {
    let app = TestApp::spawn().await;
    let get = |path: &str, ua: Option<&str>| {
        let mut req = app.client.get(app.url(path));
        if let Some(ua) = ua {
            req = req.header("User-Agent", ua);
        }
        req.send()
    };

    assert_eq!(
        get("/api/setup/install.sh", Some("curl/8.5.0"))
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    get("/api/setup/install.sh", Some("curl/8.5.0"))
        .await
        .unwrap();
    get("/api/setup/install.ps1", Some(PS51)).await.unwrap();
    get("/api/setup/install.sh", Some("roomler-setup/0.4.136"))
        .await
        .unwrap();
    get("/api/setup/install.sh", Some(CHROME_WIN))
        .await
        .unwrap();
    get("/api/setup/install.sh", None).await.unwrap();
    // The updater, and any other identity of ours: never.
    get("/api/setup/install.sh", Some("roomlerd/0.4.135"))
        .await
        .unwrap();
    get(
        "/api/setup/install.ps1",
        Some("roomler-token-renewal/0.4.135"),
    )
    .await
    .unwrap();
    // The manifest route counts BEFORE its upstream read, so this would be
    // on record already if the updater were counted; the request itself is
    // not awaited (a cold cache may take its full timeout to fail).
    let manifest = app.url("/api/setup/latest-release");
    tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .get(manifest)
            .header("User-Agent", "roomlerd/0.4.135")
            .send()
            .await;
    });

    // Six counted fetches across five rows; the writes are spawned, so wait
    // for the SUM, not the row count.
    let sum = |rows: &[Document]| -> i64 {
        rows.iter()
            .map(|r| {
                r.get_i64("value")
                    .unwrap_or_else(|_| i64::from(r.get_i32("value").unwrap()))
            })
            .sum()
    };
    let rows = wait_daily(&app, |rows| sum(rows) >= 6).await;
    assert_eq!(sum(&rows), 6, "{rows:?}");
    assert_eq!(
        value_of(&rows, "install.sh", "script-sh", "unknown"),
        Some(2)
    );
    assert_eq!(
        value_of(&rows, "install.ps1", "script-ps1", "windows"),
        Some(1)
    );
    assert_eq!(value_of(&rows, "install.sh", "wizard", "unknown"), Some(1));
    assert_eq!(value_of(&rows, "install.sh", "browser", "windows"), Some(1));
    assert_eq!(value_of(&rows, "install.sh", "other", "unknown"), Some(1));

    // Let any straggling write land, then: the updater wrote nothing.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let rows = all_docs(&app, "growth_daily").await;
    assert_eq!(rows.len(), 5, "{rows:?}");
    assert_eq!(sum(&rows), 6);

    // No User-Agent and no IP anywhere: a row is three enum-like
    // dimensions and a count.
    for r in &rows {
        assert_eq!(r.get_str("source").unwrap(), "install");
        assert_eq!(r.get_str("metric").unwrap(), "fetch");
        let day = r.get_str("day").unwrap();
        assert!(
            day.len() == 10 && day.as_bytes()[4] == b'-' && day.as_bytes()[7] == b'-',
            "{day}"
        );
        let dims = r.get_document("dims").unwrap();
        assert_eq!(
            dims.keys().cloned().collect::<Vec<_>>(),
            ["asset", "client", "os"],
            "{dims:?}"
        );
        // No key that would hold a UA or an address — exact names, since
        // "ip" is a substring of "script-sh" — and no UA fragment or the
        // loopback address as a value.
        let keys = keys_of(r);
        for k in [
            "ip",
            "user_agent",
            "user-agent",
            "ua",
            "remote_addr",
            "addr",
        ] {
            assert!(
                !keys.iter().any(|x| x == k),
                "key {k:?} reached growth_daily: {keys:?}"
            );
        }
        let text = strings_of(r).join("\n");
        for leak in [
            "Mozilla",
            "curl/",
            "roomler-setup/",
            "PowerShell",
            "127.0.0.1",
        ] {
            assert!(
                !text.contains(leak),
                "{leak:?} reached growth_daily: {text}"
            );
        }
    }
}

/// The release manifest counts too, and before the upstream read: the row
/// is there whether or not GitHub answers (the request itself is not
/// awaited — a cold cache may take its full timeout to fail).
#[tokio::test]
async fn the_release_manifest_counts_before_the_upstream_read() {
    let app = TestApp::spawn().await;
    let url = app.url("/api/agent/latest-release");
    tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .get(url)
            .header("User-Agent", "curl/8.5.0")
            .send()
            .await;
    });
    let rows = wait_daily(&app, |rows| {
        value_of(rows, "agent-latest-release", "script-sh", "unknown").is_some()
    })
    .await;
    assert_eq!(
        value_of(&rows, "agent-latest-release", "script-sh", "unknown"),
        Some(1),
        "{rows:?}"
    );
}

#[tokio::test]
async fn the_counters_switch_off_by_config() {
    let app = TestApp::spawn_with_settings(|s| s.growth.install_counters = false).await;
    let resp = app
        .client
        .get(app.url("/api/setup/install.sh"))
        .header("User-Agent", "curl/8.5.0")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the script is still served");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(all_docs(&app, "growth_daily").await.is_empty());
}

// ── The enroll's `install` ───────────────────────────────────────────────

/// FR-51's ephemeral path records the same claim: a container image's
/// entrypoint is the typical caller.
#[tokio::test]
async fn an_ephemeral_enroll_records_its_install_too() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("instr4").await;
    let r = app
        .auth_put(
            &format!("/api/tenant/{}/ephemeral-key-settings", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "ephemeral_keys_enabled": true }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let minted: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-key", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let key = minted["key"].as_str().expect("key");
    let resp = enroll(
        &app,
        key,
        "m-eph",
        json!({ "install": { "method": "container", "scope": "system" } }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["ephemeral"], true);
    let id = v["agent_id"].as_str().unwrap();
    assert_eq!(
        agent_view(&app, &t, id).await["install"],
        json!({ "method": "container", "scope": "system" })
    );
    // And the ephemeral KEY is not in the standard token ledger: it has its
    // own trail (`enrollment_keys` + `enrollment_key_uses`).
    assert!(all_docs(&app, "enroll_token_issued").await.is_empty());
}

#[tokio::test]
async fn the_enroll_records_how_it_was_installed_and_clamps_what_it_does_not_know() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("instr2").await;

    // A new agent says so: stored as sent, shown on the device.
    let a = enroll_ok(
        &app,
        &t,
        "m-a",
        json!({ "install": { "method": "script-sh", "scope": "system" } }),
    )
    .await;
    assert_eq!(
        agent_view(&app, &t, &a).await["install"],
        json!({ "method": "script-sh", "scope": "system" })
    );
    let row = agent_row(&app, &a).await;
    let install = row.get_document("install").unwrap();
    assert_eq!(install.get_str("method").unwrap(), "script-sh");
    assert_eq!(install.get_str("scope").unwrap(), "system");
    assert!(
        install.get("run").is_none(),
        "no run until P3b: {install:?}"
    );

    // The old shape (an agent from before P3): `unknown`, never absent and
    // never a refusal.
    let b = enroll_ok(&app, &t, "m-b", json!({})).await;
    assert_eq!(
        agent_view(&app, &t, &b).await["install"],
        json!({ "method": "unknown", "scope": "unknown" })
    );

    // A spelling the server does not know is clamped, a bad `run` dropped.
    let c = enroll_ok(
        &app,
        &t,
        "m-c",
        json!({ "install": { "method": "snap", "scope": "galaxy", "run": "not a token at all" } }),
    )
    .await;
    assert_eq!(
        agent_view(&app, &t, &c).await["install"],
        json!({ "method": "other", "scope": "unknown" })
    );

    // Three devices is the free plan's cap; everything below re-enrolls one
    // of them (a known machine never consumes a slot).

    // A re-enroll that says nothing keeps the row's record; one that says
    // something updates it — and a well-formed `run` is kept (P3b's).
    let a2 = enroll_ok(&app, &t, "m-a", json!({})).await;
    assert_eq!(a2, a, "same machine, same row");
    assert_eq!(
        agent_view(&app, &t, &a).await["install"],
        json!({ "method": "script-sh", "scope": "system" })
    );
    let a3 = enroll_ok(
        &app,
        &t,
        "m-a",
        json!({ "install": { "method": "wizard", "scope": "attended", "run": "0123456789abcdef0123456789abcdef" } }),
    )
    .await;
    assert_eq!(a3, a);
    assert_eq!(
        agent_view(&app, &t, &a).await["install"],
        json!({ "method": "wizard", "scope": "attended", "run": "0123456789abcdef0123456789abcdef" })
    );

    // Compatibility: a field this server has never heard of — in the body
    // or inside `install` — never refuses the enroll (here: the old-shape
    // device from above, re-enrolled by a build from the future).
    let b2 = enroll_ok(
        &app,
        &t,
        "m-b",
        json!({
            "install": { "method": "deb", "scope": "system", "flavour": "from-the-future" },
            "future_field": { "x": 1 },
        }),
    )
    .await;
    assert_eq!(b2, b);
    assert_eq!(
        agent_view(&app, &t, &b).await["install"],
        json!({ "method": "deb", "scope": "system" })
    );
}

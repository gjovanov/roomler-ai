// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-4d-1 — what a device REPORTS doing with its leases
//! (`rc:vault.activity`), on the server: a claim, kept in `vault_activity`
//! apart from the vault's own decisions.
//!
//! Driven through the lease tests' raw agent socket. What these lock:
//!
//! - a report is kept only when every lease it names was released to that
//!   device, for that session, in the use's mode and toward its target;
//!   anything else is dropped, answered with nothing;
//! - a kept report records no decision: `vault_audit` does not move;
//! - only an administrator reads the reports, newest first, by session or by
//!   lease.

use std::time::Duration;

use bson::{doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::vault_lease_tests::{
    Device, TAKES_LEASES, ask, device, lease_app, rows, seed_vault, send, start,
};

/// `db/main` handed to `psql` or `kubectl` in `exec` mode, as a use asks.
async fn allow_exec(app: &TestApp, t: &SeededTenant) {
    let main = rows(app, "vault_secrets", doc! { "path": "db/main" }).await;
    let id = main[0].get_object_id("_id").unwrap().to_hex();
    let resp = app
        .client
        .patch(app.url(&format!("/api/tenant/{}/vault/secret/{id}", t.tenant_id)))
        .header("Authorization", format!("Bearer {}", t.admin.access_token))
        .json(&json!({ "exec_allow": ["psql", "kubectl"] }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
}

/// An `exec` lease of `db/main` for `program`: its id.
async fn exec_lease(dev: &mut Device, req: &str, sid: &str, fence: u64, program: &str) -> String {
    let answer = ask(
        dev,
        req,
        json!({ "session_id": sid, "fence": fence, "secret": "db/main", "mode": "exec",
                "audience": program }),
    )
    .await;
    assert_eq!(answer["t"], "rc:vault.leased", "{answer}");
    answer["lease_id"].as_str().unwrap().to_string()
}

fn report(sid: &str, fence: u64, kind: &str, leases: &[&str], target: &str) -> Value {
    json!({ "t": "rc:vault.activity", "session_id": sid, "fence": fence, "kind": kind,
            "lease_ids": leases, "target": target })
}

/// `GET …/vault/activity` with `query`, as `token`: the status and body.
async fn read(app: &TestApp, t: &SeededTenant, token: &str, query: &str) -> (u16, Value) {
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/vault/activity{query}", t.tenant_id),
            token,
        )
        .send()
        .await
        .unwrap();
    let code = resp.status().as_u16();
    (code, resp.json().await.unwrap_or(Value::Null))
}

/// The organization's reports once there are `n`; a report is answered with
/// nothing, so the test waits for its row.
async fn reports(app: &TestApp, t: &SeededTenant, n: usize) -> Vec<Value> {
    for _ in 0..200 {
        let (code, body) = read(app, t, &t.admin.access_token, "").await;
        assert_eq!(code, 200, "{body}");
        let got = body.as_array().unwrap();
        if got.len() >= n {
            return got.clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("fewer than {n} activity reports were kept within 10 s");
}

/// A command's report, from the device the lease was released to, is kept:
/// what it ran, how it ended, which lease. It records no decision. A file's
/// keeps no exit code or timeout, and the route finds each by session and by
/// lease.
#[tokio::test]
async fn a_devices_report_of_a_use_is_kept_apart_from_the_decisions() {
    let app = lease_app().await;
    let t = app.seed_tenant("vaultact").await;
    seed_vault(&app, &t).await;
    allow_exec(&app, &t).await;
    let mut dev = device(&app, &t, "act-a", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/act", Some("dev")).await;
    let lease = exec_lease(&mut dev, "x1", &sid, fence, "psql").await;
    let decisions = rows(&app, "vault_audit", doc! {}).await.len();

    let mut exec = report(&sid, fence, "exec", &[&lease], "psql");
    exec["exit_code"] = json!(3);
    send(&mut dev.ws, exec).await;
    let got = reports(&app, &t, 1).await;
    assert_eq!(got.len(), 1, "{got:?}");
    let row = &got[0];
    assert_eq!(row["kind"], "exec");
    assert_eq!(row["target"], "psql");
    assert_eq!(row["exit_code"], 3);
    assert_eq!(row["timed_out"], false);
    assert_eq!(row["lease_ids"], json!([lease]));
    assert_eq!(row["session_id"], sid);
    assert_eq!(row["device_id"], dev.agent_id);
    assert_eq!(row["fence"], fence);
    assert_eq!(
        rows(&app, "vault_audit", doc! {}).await.len(),
        decisions,
        "a claim records no decision"
    );

    // A file's report keeps no exit code and no timeout, whatever it says.
    let file = ask(
        &mut dev,
        "f1",
        json!({ "session_id": sid, "fence": fence, "secret": "db/main", "mode": "file",
                "audience": "db.env" }),
    )
    .await;
    assert_eq!(file["t"], "rc:vault.leased", "{file}");
    let file_lease = file["lease_id"].as_str().unwrap().to_string();
    let mut render = report(&sid, fence, "render", &[&file_lease], "db.env");
    render["exit_code"] = json!(7);
    render["timed_out"] = json!(true);
    send(&mut dev.ws, render).await;
    let got = reports(&app, &t, 2).await;
    let row = &got[0];
    assert_eq!(row["kind"], "render", "newest first: {got:?}");
    assert_eq!(row["target"], "db.env");
    assert_eq!(row["exit_code"], Value::Null, "{row}");
    assert_eq!(row["timed_out"], false, "{row}");

    let token = &t.admin.access_token;
    for (query, n) in [
        (format!("?session_id={sid}"), 2),
        (format!("?lease_id={lease}"), 1),
        (format!("?lease_id={file_lease}"), 1),
        (format!("?lease_id={}", ObjectId::new().to_hex()), 0),
        (format!("?session_id={}", ObjectId::new().to_hex()), 0),
    ] {
        let (code, body) = read(&app, &t, token, &query).await;
        assert_eq!(code, 200, "{query}: {body}");
        assert_eq!(body.as_array().unwrap().len(), n, "{query}: {body}");
    }
    let (code, _) = read(&app, &t, token, "?lease_id=not-an-id").await;
    assert_eq!(code, 400);
}

/// A report naming anything but leases released to its device, for its
/// session, in the use's mode and toward its target, is dropped. Each
/// connection's reports are handled in order, so a valid one sent last
/// proves the others before it were handled, and dropped.
#[tokio::test]
async fn a_report_is_kept_only_for_leases_released_to_that_device_for_that_use() {
    let app = lease_app().await;
    let t = app.seed_tenant("vaultact2").await;
    seed_vault(&app, &t).await;
    allow_exec(&app, &t).await;
    let mut a = device(&app, &t, "act-a2", "linux", TAKES_LEASES, true).await;
    let mut b = device(&app, &t, "act-b2", "linux", TAKES_LEASES, true).await;
    let (sid_a, fence_a) = start(&app, &t, &mut a, "/srv/a", Some("dev")).await;
    let (sid_b, fence_b) = start(&app, &t, &mut b, "/srv/b", Some("dev")).await;
    let la = exec_lease(&mut a, "a1", &sid_a, fence_a, "psql").await;
    let lb = exec_lease(&mut b, "b1", &sid_b, fence_b, "psql").await;
    // A file lease toward a name no use would write: the server holds a file
    // audience to no rule, so the report's target check is what drops it.
    let odd = ask(
        &mut a,
        "a2",
        json!({ "session_id": sid_a, "fence": fence_a, "secret": "db/main", "mode": "file",
                "audience": "../db.env" }),
    )
    .await;
    assert_eq!(odd["t"], "rc:vault.leased", "{odd}");
    let lodd = odd["lease_id"].as_str().unwrap().to_string();

    // Nine times one lease that would pass: refused for the count alone.
    let nine = vec![la.as_str(); 9];
    for bad in [
        // An exec lease reported as a file's.
        report(&sid_a, fence_a, "render", &[&la], "psql"),
        // Toward a program it was not released for.
        report(&sid_a, fence_a, "exec", &[&la], "kubectl"),
        // For a session it was not released to.
        report(&sid_b, fence_a, "exec", &[&la], "psql"),
        // Another device's lease.
        report(&sid_a, fence_a, "exec", &[&lb], "psql"),
        // A kind this server cannot name, no lease, too many, not an id, a path.
        report(&sid_a, fence_a, "teleport", &[&la], "psql"),
        report(&sid_a, fence_a, "exec", &[], "psql"),
        report(&sid_a, fence_a, "exec", &nine, "psql"),
        report(&sid_a, fence_a, "exec", &["not-an-id"], "psql"),
        report(&sid_a, fence_a, "exec", &[&la], "/usr/bin/psql"),
        // A target that is no bare name, though a lease was released toward it.
        report(&sid_a, fence_a, "render", &[&lodd], "../db.env"),
    ] {
        send(&mut a.ws, bad).await;
    }
    send(&mut b.ws, report(&sid_a, fence_a, "exec", &[&la], "psql")).await;
    send(&mut a.ws, report(&sid_a, fence_a, "exec", &[&la], "psql")).await;
    send(&mut b.ws, report(&sid_b, fence_b, "exec", &[&lb], "psql")).await;

    // Wait for the two valid reports themselves: by then every report sent
    // before each on its connection was handled, so one wrongly kept would
    // be a third row already.
    let valid = |r: &Value, dev: &str, sid: &str, lease: &str| {
        r["device_id"] == dev
            && r["session_id"] == sid
            && r["lease_ids"] == json!([lease])
            && r["kind"] == "exec"
            && r["target"] == "psql"
    };
    let mut got = Vec::new();
    for _ in 0..200 {
        let (code, body) = read(&app, &t, &t.admin.access_token, "").await;
        assert_eq!(code, 200, "{body}");
        got = body.as_array().unwrap().clone();
        let has = |dev: &str, sid: &str, lease: &str| got.iter().any(|r| valid(r, dev, sid, lease));
        if has(&a.agent_id, &sid_a, &la) && has(&b.agent_id, &sid_b, &lb) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        got.iter().any(|r| valid(r, &a.agent_id, &sid_a, &la))
            && got.iter().any(|r| valid(r, &b.agent_id, &sid_b, &lb)),
        "both valid reports kept within 10 s: {got:?}"
    );
    assert_eq!(got.len(), 2, "only the two valid reports: {got:?}");
}

/// Only an administrator reads what devices report.
#[tokio::test]
async fn only_an_administrator_reads_what_devices_report() {
    let app = lease_app().await;
    let t = app.seed_tenant("vaultact3").await;
    let (code, _) = read(&app, &t, &t.member.access_token, "").await;
    assert_eq!(code, 403);
    let (code, body) = read(&app, &t, &t.admin.access_token, "").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body, json!([]));
}

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-129 A1 — device pairing, the server half: an installer starts a
//! pairing, a signed-in person looks the code up and approves it into an
//! org, and the installer's poll receives the ordinary enrollment token —
//! minted on delivery, once, behind the same gate as the admin route,
//! with every refusal on the audit trail.

use bson::{Bson, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use serde_json::{Value, json};
use std::time::Duration;

use crate::fixtures::{seed::SeededTenant, test_app::TestApp};

/// `MANAGE_AGENTS` (`role::permissions`), as the role route takes it.
const MANAGE_AGENTS: u64 = 1 << 24;

/// A server whose polls are never "too soon", so a test can poll at once.
async fn app_fast() -> TestApp {
    TestApp::spawn_with_settings(|s| s.pairing.poll_interval_secs = 0).await
}

fn start_body(role: &str, os: &str) -> Value {
    json!({
        "machine_name": "pairing test laptop",
        "os": os,
        "arch": "x86_64",
        "role": role,
        "client": "wizard",
        "client_version": "0.4.180",
    })
}

async fn post_json(app: &TestApp, path: &str, body: &Value) -> (u16, String) {
    let resp = app
        .client
        .post(app.url(path))
        .json(body)
        .send()
        .await
        .expect("request");
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap_or_default())
}

async fn post_json_as(app: &TestApp, path: &str, token: &str, body: &Value) -> (u16, String) {
    let resp = app
        .auth_post(path, token)
        .json(body)
        .send()
        .await
        .expect("request");
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap_or_default())
}

/// A request with NO credential at all. `app.client` keeps a cookie jar,
/// and `seed_tenant`'s logins leave the last user's `access_token` cookie
/// in it, so an "anonymous" call through it is the member's — the cookie
/// is the second place `AuthUser` looks.
async fn post_json_anon(app: &TestApp, path: &str, body: &Value) -> (u16, String) {
    let resp = reqwest::Client::new()
        .post(app.url(path))
        .json(body)
        .send()
        .await
        .expect("request");
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap_or_default())
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

async fn start(app: &TestApp, body: &Value) -> (u16, Value) {
    let (s, t) = post_json(app, "/api/pair/start", body).await;
    (s, parse(&t))
}

async fn poll(app: &TestApp, device_code: &str) -> (u16, Value) {
    let (s, t) = post_json(
        app,
        "/api/pair/poll",
        &json!({ "device_code": device_code }),
    )
    .await;
    (s, parse(&t))
}

async fn lookup(app: &TestApp, token: &str, user_code: &str) -> (u16, Value) {
    let (s, t) = post_json_as(
        app,
        "/api/pair/lookup",
        token,
        &json!({ "user_code": user_code }),
    )
    .await;
    (s, parse(&t))
}

async fn approve(app: &TestApp, token: &str, user_code: &str, tenant_id: &str) -> (u16, Value) {
    let (s, t) = post_json_as(
        app,
        "/api/pair/approve",
        token,
        &json!({ "user_code": user_code, "tenant_id": tenant_id }),
    )
    .await;
    (s, parse(&t))
}

async fn deny(app: &TestApp, token: &str, user_code: &str) -> (u16, Value) {
    let (s, t) = post_json_as(
        app,
        "/api/pair/deny",
        token,
        &json!({ "user_code": user_code }),
    )
    .await;
    (s, parse(&t))
}

/// Start a daemon pairing and return `(device_code, user_code)`.
async fn start_daemon(app: &TestApp) -> (String, String) {
    let (status, body) = start(app, &start_body("daemon", "linux")).await;
    assert_eq!(status, 200, "start: {body}");
    (
        body["device_code"].as_str().unwrap().to_string(),
        body["user_code"].as_str().unwrap().to_string(),
    )
}

/// The whole flow for an admin: start, approve, poll → the tokens.
async fn pair_as_admin(app: &TestApp, org: &SeededTenant, body: &Value) -> Vec<String> {
    let (status, started) = start(app, body).await;
    assert_eq!(status, 200, "start: {started}");
    let device_code = started["device_code"].as_str().unwrap();
    let user_code = started["user_code"].as_str().unwrap();
    let (status, approved) = approve(app, &org.admin.access_token, user_code, &org.tenant_id).await;
    assert_eq!(status, 200, "approve: {approved}");
    let (status, polled) = poll(app, device_code).await;
    assert_eq!(status, 200, "poll: {polled}");
    assert_eq!(polled["status"], "approved", "{polled}");
    polled["tokens"]
        .as_array()
        .expect("tokens")
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect()
}

async fn enroll_agent(app: &TestApp, token: &str, machine_id: &str) -> (u16, Value) {
    let (s, t) = post_json(
        app,
        "/api/agent/enroll",
        &json!({
            "enrollment_token": token,
            "machine_id": machine_id,
            "machine_name": "pairing test laptop",
            "os": "linux",
            "agent_version": "0.4.180",
            "install": { "method": "wizard", "scope": "user" },
        }),
    )
    .await;
    (s, parse(&t))
}

async fn rows(app: &TestApp, collection: &str, filter: Document) -> Vec<Document> {
    app.db
        .collection::<Document>(collection)
        .find(filter)
        .sort(doc! { "_id": 1 })
        .await
        .expect("find")
        .try_collect()
        .await
        .expect("collect")
}

async fn audit(app: &TestApp, action: &str, outcome: &str) -> Vec<Document> {
    rows(
        app,
        "pairing_audit",
        doc! { "action": action, "outcome": outcome },
    )
    .await
}

async fn pairing_row(app: &TestApp, user_code: &str) -> Document {
    let stored = user_code.replace('-', "");
    let mut found = rows(app, "device_pairings", doc! { "user_code": stored }).await;
    assert_eq!(found.len(), 1, "one pairing row for {user_code}");
    found.remove(0)
}

/// Create a role holding only `MANAGE_AGENTS` and hand it to `user_id`.
/// Returns the role id, so the test can take it back.
async fn grant_manage_agents(app: &TestApp, org: &SeededTenant, user_id: &str) -> String {
    let (status, role) = post_json_as(
        app,
        &format!("/api/tenant/{}/role", org.tenant_id),
        &org.admin.access_token,
        &json!({ "name": "pairer", "permissions": MANAGE_AGENTS }),
    )
    .await;
    assert!(status < 300, "create role: {role}");
    let role_id = parse(&role)["id"].as_str().unwrap().to_string();
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/role/{}/assign/{}",
                org.tenant_id, role_id, user_id
            ),
            &org.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "assign role");
    role_id
}

async fn revoke_role(app: &TestApp, org: &SeededTenant, role_id: &str, user_id: &str) {
    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/role/{}/assign/{}",
                org.tenant_id, role_id, user_id
            ),
            &org.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "unassign role");
}

// ── the happy paths ─────────────────────────────────────────────────────────

/// AC2 / AC9's positive half: the daemon flow end to end, the FR-102 ledger
/// written, the token enrolling through the unchanged enroll route, and
/// delivery once.
#[tokio::test]
async fn a_daemon_pairing_delivers_an_enrollment_token_once_and_it_enrolls() {
    let app = app_fast().await;
    let org = app.seed_tenant("pair1").await;

    let (status, started) = start(&app, &start_body("daemon", "linux")).await;
    assert_eq!(status, 200, "{started}");
    let device_code = started["device_code"].as_str().unwrap();
    let user_code = started["user_code"].as_str().unwrap();
    assert_eq!(device_code.len(), 43, "32 bytes, base64url, unpadded");
    assert_eq!(user_code.len(), 9);
    assert_eq!(&user_code[4..5], "-");
    assert!(
        user_code
            .bytes()
            .all(|b| b == b'-' || b"BCDFGHJKLMNPQRSTVWXZ".contains(&b)),
        "{user_code}"
    );
    assert!(
        started["verification_uri"]
            .as_str()
            .unwrap()
            .ends_with("/pair")
    );
    assert_eq!(started["expires_in"], 900);
    assert_eq!(started["interval"], 0);

    // Nothing decided yet.
    let (status, polled) = poll(&app, device_code).await;
    assert_eq!(status, 200);
    assert_eq!(polled["status"], "authorization_pending", "{polled}");

    // The person sees the device before approving (AC4's server half).
    let (status, seen) = lookup(&app, &org.admin.access_token, user_code).await;
    assert_eq!(status, 200, "{seen}");
    assert_eq!(seen["machine_name"], "pairing test laptop");
    assert_eq!(seen["os"], "linux");
    assert_eq!(seen["arch"], "x86_64");
    assert_eq!(seen["role"], "daemon");
    assert_eq!(seen["halves"], 1);
    assert_eq!(seen["client"], "wizard");
    assert_eq!(seen["client_version"], "0.4.180");
    assert!(
        seen["country"].is_null(),
        "no geoip database ⇒ no country, never a guess"
    );
    assert!(seen["started_at"].as_str().unwrap().contains('T'));
    assert!(seen["expires_at"].as_str().unwrap().contains('T'));

    let (status, approved) =
        approve(&app, &org.admin.access_token, user_code, &org.tenant_id).await;
    assert_eq!(status, 200, "{approved}");
    assert_eq!(approved, json!({ "approved": true }));
    // The decision is on the row; no token exists yet.
    let row = pairing_row(&app, user_code).await;
    assert_eq!(row.get_str("status").unwrap(), "approved");
    assert!(row.get("delivered_at").is_none_or(|v| v == &Bson::Null));
    let ledger_before = rows(&app, "enroll_token_issued", doc! {}).await;
    assert!(ledger_before.is_empty(), "approval mints nothing");

    // The first poll after the approval mints.
    let (status, polled) = poll(&app, device_code).await;
    assert_eq!(status, 200, "{polled}");
    assert_eq!(polled["status"], "approved");
    let tokens = polled["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 1);
    assert_eq!(polled["org_name"], "pair1 Corp");
    assert_eq!(polled["approver_name"], "pair1 Admin");
    let token = tokens[0].as_str().unwrap();

    // FR-102: the issuance is on the ledger exactly as the admin route's is.
    let ledger = rows(&app, "enroll_token_issued", doc! {}).await;
    assert_eq!(ledger.len(), 1);
    assert_eq!(
        ledger[0].get_object_id("tenant_id").unwrap().to_hex(),
        org.tenant_id
    );
    assert_eq!(
        ledger[0].get_object_id("issued_by").unwrap().to_hex(),
        org.admin.id
    );

    // Delivery is once: the same device code now retrieves nothing.
    let (_, again) = poll(&app, device_code).await;
    assert_eq!(again["status"], "expired_token", "{again}");
    let row = pairing_row(&app, user_code).await;
    assert_eq!(row.get_str("status").unwrap(), "delivered");

    // The token is the ordinary single-use enrollment token.
    let (status, enrolled) = enroll_agent(&app, token, "pair1-machine").await;
    assert_eq!(status, 200, "{enrolled}");
    assert_eq!(enrolled["tenant_id"], org.tenant_id);
    let (status, replay) = enroll_agent(&app, token, "pair1-machine-2").await;
    assert_eq!(status, 401, "single-use: {replay}");

    // AC8: every step is on the trail.
    for action in ["start", "lookup", "approve", "deliver"] {
        let hits = audit(&app, action, "ok").await;
        assert_eq!(hits.len(), 1, "one `{action}` row");
        assert_eq!(hits[0].get_str("role").unwrap(), "daemon");
        assert_eq!(
            hits[0].get_str("machine_name").unwrap(),
            "pairing test laptop"
        );
        assert!(hits[0].get_object_id("pairing_id").is_ok());
    }
    let delivered = audit(&app, "deliver", "ok").await;
    assert_eq!(
        delivered[0].get_object_id("user_id").unwrap().to_hex(),
        org.admin.id
    );
    assert_eq!(
        delivered[0].get_object_id("tenant_id").unwrap().to_hex(),
        org.tenant_id
    );
}

/// The tunnel role: any member approves, the tunnel enrollment token is
/// minted, and it enrolls a tunnel client. An outsider is refused with a
/// plain `forbidden` — never `not_a_member`, the code the SPA leaves the
/// org on.
#[tokio::test]
async fn a_tunnel_pairing_is_approved_by_a_member_and_enrolls_a_tunnel_client() {
    let app = app_fast().await;
    let org = app.seed_tenant("pair2").await;
    let other = app.seed_tenant("pair2b").await;

    let (status, started) = start(&app, &start_body("tunnel", "macos")).await;
    assert_eq!(status, 200, "{started}");
    let device_code = started["device_code"].as_str().unwrap();
    let user_code = started["user_code"].as_str().unwrap();

    let (status, refused) =
        approve(&app, &other.admin.access_token, user_code, &org.tenant_id).await;
    assert_eq!(status, 403, "{refused}");
    assert_eq!(refused["error"], "forbidden", "{refused}");
    assert_ne!(refused["error"], "not_a_member");
    assert_eq!(
        audit(&app, "approve", "refused:not_a_member").await.len(),
        1
    );

    // A plain member holds no MANAGE_AGENTS and may still enroll a tunnel
    // client, exactly as on `…/tunnel-client/enroll-token`.
    let (status, approved) =
        approve(&app, &org.member.access_token, user_code, &org.tenant_id).await;
    assert_eq!(status, 200, "{approved}");

    let (_, polled) = poll(&app, device_code).await;
    assert_eq!(polled["status"], "approved", "{polled}");
    let token = polled["tokens"][0].as_str().unwrap();

    let (status, enrolled) = post_json(
        &app,
        "/api/tunnel-client/enroll",
        &json!({
            "enrollment_token": token,
            "machine_id": "pair2-tunnel",
            "machine_name": "pairing test laptop",
            "os": "macos",
            "client_version": "0.4.180",
        }),
    )
    .await;
    assert_eq!(status, 200, "{enrolled}");
    assert_eq!(parse(&enrolled)["tenant_id"], org.tenant_id);

    // A tunnel token is not an agent token.
    let (status, wrong) = enroll_agent(&app, token, "pair2-agent").await;
    assert_eq!(status, 401, "{wrong}");

    // The tunnel kind is not on the agent-token ledger (FR-102 counts the
    // standard token only, as the admin route does).
    assert!(rows(&app, "enroll_token_issued", doc! {}).await.is_empty());
    let delivered = audit(&app, "deliver", "ok").await;
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].get_str("role").unwrap(), "tunnel");
}

/// Operator decision 7: a macOS daemon's root half is a second enrollment,
/// so one approval delivers two tokens — and only there.
#[tokio::test]
async fn two_halves_deliver_two_tokens_for_a_macos_daemon_only() {
    let app = app_fast().await;
    let org = app.seed_tenant("pair3").await;

    for (os, role) in [
        ("linux", "daemon"),
        ("windows", "daemon"),
        ("macos", "tunnel"),
    ] {
        let mut body = start_body(role, os);
        body["halves"] = json!(2);
        let (status, refused) = start(&app, &body).await;
        assert_eq!(status, 400, "{os} {role}: {refused}");
        assert_eq!(refused["error"], "invalid_request");
    }
    let mut body = start_body("daemon", "linux");
    body["halves"] = json!(3);
    let (status, _) = start(&app, &body).await;
    assert_eq!(status, 400);

    let mut body = start_body("daemon", "macos");
    body["halves"] = json!(2);
    let tokens = pair_as_admin(&app, &org, &body).await;
    assert_eq!(tokens.len(), 2);
    assert_ne!(tokens[0], tokens[1]);
    // Both halves enroll, as two devices, and both are on the ledger.
    let (status, _) = enroll_agent(&app, &tokens[0], "pair3-user-half").await;
    assert_eq!(status, 200);
    let (status, _) = enroll_agent(&app, &tokens[1], "pair3-root-half").await;
    assert_eq!(status, 200);
    assert_eq!(rows(&app, "enroll_token_issued", doc! {}).await.len(), 2);
}

/// AC6: start on pod A, look up, approve and poll on pod B. No pod-local
/// state — the Mongo row is the pairing.
#[tokio::test]
async fn a_pairing_started_on_one_pod_is_approved_and_delivered_on_another() {
    let (pod_a, pod_b) = TestApp::spawn_pair(|s| s.pairing.poll_interval_secs = 0).await;
    let org = pod_b.seed_tenant("pair4").await;

    let (device_code, user_code) = start_daemon(&pod_a).await;

    let (status, seen) = lookup(&pod_b, &org.admin.access_token, &user_code).await;
    assert_eq!(status, 200, "{seen}");
    let (status, approved) =
        approve(&pod_b, &org.admin.access_token, &user_code, &org.tenant_id).await;
    assert_eq!(status, 200, "{approved}");

    let (_, polled) = poll(&pod_b, &device_code).await;
    assert_eq!(polled["status"], "approved", "{polled}");
    assert_eq!(polled["tokens"].as_array().unwrap().len(), 1);

    // Delivery is once across pods too.
    let (_, again) = poll(&pod_a, &device_code).await;
    assert_eq!(again["status"], "expired_token", "{again}");
}

// ── the codes ───────────────────────────────────────────────────────────────

/// AC3: a user code alone, a forged device code, or none retrieve nothing —
/// and the answer never says which it was.
#[tokio::test]
async fn a_user_code_or_a_forged_device_code_retrieves_nothing() {
    let app = app_fast().await;
    let org = app.seed_tenant("pair5").await;
    let (device_code, user_code) = start_daemon(&app).await;
    let (status, _) = approve(&app, &org.admin.access_token, &user_code, &org.tenant_id).await;
    assert_eq!(status, 200);

    for forged in [
        user_code.clone(),
        user_code.replace('-', ""),
        "not-a-device-code".to_string(),
        String::new(),
        format!("{device_code}x"),
        device_code[..42].to_string(),
    ] {
        let (status, polled) = poll(&app, &forged).await;
        assert_eq!(status, 200);
        assert_eq!(polled["status"], "expired_token", "{forged:?}: {polled}");
        assert!(polled.get("tokens").is_none());
    }
    // Every forged poll is on the trail.
    assert_eq!(audit(&app, "refuse", "refused:unknown_code").await.len(), 6);

    // The real code still delivers: the forgeries cost the pairing nothing.
    let (_, polled) = poll(&app, &device_code).await;
    assert_eq!(polled["status"], "approved", "{polled}");

    // A poll with no body shape at all is a 400, not an oracle.
    let (status, _) = post_json(&app, "/api/pair/poll", &json!({ "code": "x" })).await;
    assert_eq!(status, 400);
}

/// AC3's other half: the device code leaves the server once, in `start`'s
/// response, and is nowhere in the database.
#[tokio::test]
async fn the_device_code_is_in_no_response_but_starts_and_nowhere_in_the_database() {
    let app = app_fast().await;
    let org = app.seed_tenant("pair6").await;
    let (device_code, user_code) = start_daemon(&app).await;

    let mut texts = Vec::new();
    texts.push(
        post_json(
            &app,
            "/api/pair/poll",
            &json!({ "device_code": &device_code }),
        )
        .await
        .1,
    );
    texts.push(
        post_json_as(
            &app,
            "/api/pair/lookup",
            &org.admin.access_token,
            &json!({ "user_code": &user_code }),
        )
        .await
        .1,
    );
    texts.push(
        post_json_as(
            &app,
            "/api/pair/approve",
            &org.admin.access_token,
            &json!({ "user_code": &user_code, "tenant_id": &org.tenant_id }),
        )
        .await
        .1,
    );
    let delivered = post_json(
        &app,
        "/api/pair/poll",
        &json!({ "device_code": &device_code }),
    )
    .await
    .1;
    assert!(delivered.contains("\"approved\""), "{delivered}");
    texts.push(delivered);
    texts.push(
        post_json(
            &app,
            "/api/pair/poll",
            &json!({ "device_code": &device_code }),
        )
        .await
        .1,
    );
    for t in &texts {
        assert!(
            !t.contains(&device_code),
            "a response carried the device code: {t}"
        );
    }

    for collection in ["device_pairings", "pairing_audit", "enroll_token_issued"] {
        for row in rows(&app, collection, doc! {}).await {
            let dumped = format!("{row:?}");
            assert!(
                !dumped.contains(&device_code),
                "{collection} holds the device code: {dumped}"
            );
        }
    }
    let row = pairing_row(&app, &user_code).await;
    let hash = row.get_str("device_code_hash").unwrap();
    assert_eq!(hash.len(), 64);
    assert_ne!(hash, device_code);
}

// ── the clocks ──────────────────────────────────────────────────────────────

/// AC7: a poll sooner than the interval is told to slow down, and the
/// interval it is told grows by five seconds each time (RFC 8628 §3.5).
#[tokio::test]
async fn polling_faster_than_the_interval_answers_slow_down_with_a_longer_interval() {
    let app = TestApp::spawn().await; // the default interval: 5 s
    let (device_code, _) = start_daemon(&app).await;

    let (_, first) = poll(&app, &device_code).await;
    assert_eq!(first["status"], "authorization_pending", "{first}");
    let (_, second) = poll(&app, &device_code).await;
    assert_eq!(second["status"], "slow_down", "{second}");
    assert_eq!(second["interval"], 10);
    let (_, third) = poll(&app, &device_code).await;
    assert_eq!(third["status"], "slow_down", "{third}");
    assert_eq!(third["interval"], 15);
    assert_eq!(audit(&app, "refuse", "refused:slow_down").await.len(), 2);
}

/// A pairing past its life answers `expired_token`, names nothing to the
/// person, and its expiry is audited exactly once — without waiting for
/// the TTL sweep, which runs a minute late.
#[tokio::test]
async fn an_expired_pairing_is_gone_everywhere_and_its_expiry_is_audited_once() {
    let app = TestApp::spawn_with_settings(|s| {
        s.pairing.poll_interval_secs = 0;
        s.pairing.ttl_secs = 1;
    })
    .await;
    let org = app.seed_tenant("pair7").await;
    let (device_code, user_code) = start_daemon(&app).await;
    let (_, live) = lookup(&app, &org.admin.access_token, &user_code).await;
    assert_eq!(live["role"], "daemon");

    tokio::time::sleep(Duration::from_millis(1500)).await;

    let (_, polled) = poll(&app, &device_code).await;
    assert_eq!(polled["status"], "expired_token", "{polled}");
    let (status, _) = lookup(&app, &org.admin.access_token, &user_code).await;
    assert_eq!(status, 404);
    let (status, _) = approve(&app, &org.admin.access_token, &user_code, &org.tenant_id).await;
    assert_eq!(status, 404);
    let (_, polled) = poll(&app, &device_code).await;
    assert_eq!(polled["status"], "expired_token");

    let row = pairing_row(&app, &user_code).await;
    assert_eq!(row.get_str("status").unwrap(), "expired");
    assert_eq!(audit(&app, "expire", "ok").await.len(), 1, "audited once");
}

// ── the decisions ───────────────────────────────────────────────────────────

/// A denial closes the pairing: the installer hears `access_denied`, the
/// code names nothing any more.
#[tokio::test]
async fn a_denied_pairing_answers_access_denied() {
    let app = app_fast().await;
    let org = app.seed_tenant("pair8").await;
    let (device_code, user_code) = start_daemon(&app).await;

    let (status, denied) = deny(&app, &org.member.access_token, &user_code).await;
    assert_eq!(status, 200, "{denied}");
    assert_eq!(denied, json!({ "denied": true }));

    let (_, polled) = poll(&app, &device_code).await;
    assert_eq!(polled["status"], "access_denied", "{polled}");
    let (status, _) = lookup(&app, &org.admin.access_token, &user_code).await;
    assert_eq!(status, 404);
    let (status, _) = approve(&app, &org.admin.access_token, &user_code, &org.tenant_id).await;
    assert_eq!(status, 404);
    let (status, _) = deny(&app, &org.admin.access_token, &user_code).await;
    assert_eq!(status, 404, "denied once");

    let row = pairing_row(&app, &user_code).await;
    assert_eq!(row.get_str("status").unwrap(), "denied");
    assert_eq!(
        row.get_object_id("approver_user_id").unwrap().to_hex(),
        org.member.id
    );
    assert_eq!(audit(&app, "deny", "ok").await.len(), 1);
}

/// Invariant 2 and 3: the gate is re-checked at delivery. An approver who
/// lost `MANAGE_AGENTS` between approve and poll gets nothing, and the
/// pairing closes as denied.
#[tokio::test]
async fn an_approver_demoted_before_the_poll_gets_access_denied() {
    let app = app_fast().await;
    let org = app.seed_tenant("pair9").await;
    let role_id = grant_manage_agents(&app, &org, &org.member.id).await;
    let (device_code, user_code) = start_daemon(&app).await;

    let (status, approved) =
        approve(&app, &org.member.access_token, &user_code, &org.tenant_id).await;
    assert_eq!(status, 200, "{approved}");

    revoke_role(&app, &org, &role_id, &org.member.id).await;

    let (_, polled) = poll(&app, &device_code).await;
    assert_eq!(polled["status"], "access_denied", "{polled}");
    assert!(polled.get("tokens").is_none());
    let row = pairing_row(&app, &user_code).await;
    assert_eq!(row.get_str("status").unwrap(), "denied");
    assert!(
        rows(&app, "enroll_token_issued", doc! {}).await.is_empty(),
        "nothing minted"
    );
    let refused = audit(&app, "refuse", "refused:gate_lost").await;
    assert_eq!(refused.len(), 1);
    assert_eq!(
        refused[0].get_object_id("user_id").unwrap().to_hex(),
        org.member.id
    );
    // And the closed pairing stays closed.
    let (_, polled) = poll(&app, &device_code).await;
    assert_eq!(polled["status"], "access_denied");
}

/// The daemon gate at approve: a plain member and an outsider are refused,
/// each refusal audited and counted as a miss.
#[tokio::test]
async fn approving_a_daemon_pairing_needs_manage_agents_in_that_org() {
    let app = app_fast().await;
    let org = app.seed_tenant("pair10").await;
    let other = app.seed_tenant("pair10b").await;
    let (device_code, user_code) = start_daemon(&app).await;

    let (status, refused) =
        approve(&app, &org.member.access_token, &user_code, &org.tenant_id).await;
    assert_eq!(status, 403, "{refused}");
    assert_eq!(refused["error"], "forbidden");

    // An outsider is refused with a PLAIN `forbidden`, never the
    // `not_a_member` code: the SPA leaves the org on that code, and a
    // refused approval must not evict the person from the `/pair` page.
    let (status, refused) =
        approve(&app, &other.admin.access_token, &user_code, &org.tenant_id).await;
    assert_eq!(status, 403, "{refused}");
    assert_eq!(refused["error"], "forbidden", "{refused}");
    assert_ne!(refused["error"], "not_a_member");

    // Still pending: the refusals changed nothing.
    let (_, polled) = poll(&app, &device_code).await;
    assert_eq!(polled["status"], "authorization_pending");
    assert_eq!(
        audit(&app, "approve", "refused:no_permission").await.len(),
        1
    );
    assert_eq!(
        audit(&app, "approve", "refused:not_a_member").await.len(),
        1
    );

    // The other org's admin may approve it into THEIR org, where they hold
    // the bit — the wizard then shows that org's name before installing.
    let (status, approved) = approve(
        &app,
        &other.admin.access_token,
        &user_code,
        &other.tenant_id,
    )
    .await;
    assert_eq!(status, 200, "{approved}");
    let (_, polled) = poll(&app, &device_code).await;
    assert_eq!(polled["org_name"], "pair10b Corp", "{polled}");
}

// ── downstream, unchanged ───────────────────────────────────────────────────

/// AC9: the plan's device cap refuses a paired token exactly as it refuses
/// a pasted one — at enroll, after the token was delivered.
#[tokio::test]
async fn the_device_cap_refuses_a_paired_token_like_a_pasted_one() {
    let app = app_fast().await;
    let org = app.seed_tenant("pair11").await;
    // Free.max_devices == 100: seed the org to its cap directly.
    let tid = ObjectId::parse_str(&org.tenant_id).unwrap();
    let now = bson::DateTime::now();
    let docs: Vec<Document> = (0..100)
        .map(|i| {
            doc! {
                "tenant_id": tid,
                "machine_id": format!("pair11-seed-{i}"),
                "name": format!("seed-{i}"),
                "created_at": now,
                "updated_at": now,
                "deleted_at": Bson::Null,
            }
        })
        .collect();
    app.db
        .collection::<Document>("agents")
        .insert_many(docs)
        .await
        .expect("agent seed");

    let tokens = pair_as_admin(&app, &org, &start_body("daemon", "linux")).await;
    let (status, refused) = enroll_agent(&app, &tokens[0], "pair11-one-too-many").await;
    assert_eq!(status, 403, "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Device limit"),
        "{refused}"
    );
}

/// AC9: an archived org refuses a paired token exactly as it refuses a
/// pasted one.
#[tokio::test]
async fn an_archived_org_refuses_a_paired_token_like_a_pasted_one() {
    let app = app_fast().await;
    let org = app.seed_tenant("pair12").await;
    let tokens = pair_as_admin(&app, &org, &start_body("daemon", "linux")).await;

    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/archive", org.tenant_id),
            &org.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "archive");

    let (status, refused) = enroll_agent(&app, &tokens[0], "pair12-late").await;
    assert_eq!(status, 403, "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap_or_default()
            .contains("archived"),
        "{refused}"
    );
}

// ── the limits ──────────────────────────────────────────────────────────────

/// AC7: a start flood from one address is refused past its ceiling, and
/// the refusal is on the trail with the address.
#[tokio::test]
async fn a_start_flood_from_one_address_is_refused_past_the_ceiling() {
    let app = TestApp::spawn_with_settings(|s| s.pairing.start_per_ip_per_hour = 3).await;
    for i in 0..3 {
        let (status, body) = start(&app, &start_body("daemon", "linux")).await;
        assert_eq!(status, 200, "start {i}: {body}");
    }
    let (status, refused) = start(&app, &start_body("daemon", "linux")).await;
    assert_eq!(status, 429, "{refused}");
    assert_eq!(refused["error"], "rate_limited");
    assert!(refused.get("device_code").is_none());
    let hits = audit(&app, "start", "refused:ip_ceiling").await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].get_str("client_ip").unwrap(), "127.0.0.1");
    assert_eq!(audit(&app, "start", "ok").await.len(), 3);
}

/// AC7: misses past the per-account ceiling lock that account's lookups,
/// approvals and denials for the hour — and nobody else's.
#[tokio::test]
async fn lookup_misses_lock_the_account_for_the_hour() {
    let app = TestApp::spawn_with_settings(|s| {
        s.pairing.poll_interval_secs = 0;
        s.pairing.misses_per_account_per_hour = 2;
    })
    .await;
    let org = app.seed_tenant("pair13").await;
    let (_, user_code) = start_daemon(&app).await;

    // Two codes of the right shape that name nothing: two misses.
    for guess in ["BCDF-GHJK", "zzzz-xxxx"] {
        let (status, _) = lookup(&app, &org.admin.access_token, guess).await;
        assert_eq!(status, 404, "{guess}");
    }
    assert_eq!(audit(&app, "lookup", "refused:not_pending").await.len(), 2);

    // At the ceiling: even the REAL code is refused, on every route.
    let (status, refused) = lookup(&app, &org.admin.access_token, &user_code).await;
    assert_eq!(status, 429, "{refused}");
    assert_eq!(refused["error"], "rate_limited");
    let (status, _) = approve(&app, &org.admin.access_token, &user_code, &org.tenant_id).await;
    assert_eq!(status, 429);
    let (status, _) = deny(&app, &org.admin.access_token, &user_code).await;
    assert_eq!(status, 429);
    assert_eq!(
        audit(&app, "lookup", "refused:account_locked").await.len(),
        1
    );
    assert_eq!(
        audit(&app, "approve", "refused:account_locked").await.len(),
        1
    );
    assert_eq!(audit(&app, "deny", "refused:account_locked").await.len(), 1);

    // Another account is not locked by the first one's misses.
    let (status, seen) = lookup(&app, &org.member.access_token, &user_code).await;
    assert_eq!(status, 200, "{seen}");
}

// ── the switches ────────────────────────────────────────────────────────────

/// AC10's server half: with `[pairing] enabled = false` every route is the
/// empty 404 an older server answers — to everyone, signed in or not.
#[tokio::test]
async fn pairing_disabled_answers_404_on_every_route() {
    let app = TestApp::spawn_with_settings(|s| s.pairing.enabled = false).await;
    let org = app.seed_tenant("pair14").await;

    let (status, body) = post_json(&app, "/api/pair/start", &start_body("daemon", "linux")).await;
    assert_eq!(status, 404);
    assert!(body.is_empty(), "the empty 404: {body}");
    let (status, _) = post_json(&app, "/api/pair/poll", &json!({ "device_code": "x" })).await;
    assert_eq!(status, 404);
    for path in ["/api/pair/lookup", "/api/pair/approve", "/api/pair/deny"] {
        let (status, body) = post_json_as(
            &app,
            path,
            &org.admin.access_token,
            &json!({ "user_code": "BCDF-GHJK", "tenant_id": &org.tenant_id }),
        )
        .await;
        assert_eq!(status, 404, "{path}");
        assert!(body.is_empty(), "{path}: {body}");
        // Not a 401: a disabled server does not tell an anonymous caller
        // the routes exist either.
        let (status, _) = post_json_anon(&app, path, &json!({ "user_code": "BCDF-GHJK" })).await;
        assert_eq!(status, 404, "{path} unauthenticated");
    }
    assert!(rows(&app, "device_pairings", doc! {}).await.is_empty());

    // A pasted token still enrolls unchanged.
    let et: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", org.tenant_id),
            &org.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (status, _) = enroll_agent(
        &app,
        et["enrollment_token"].as_str().unwrap(),
        "pair14-paste",
    )
    .await;
    assert_eq!(status, 200);
}

/// A tunnel-role pairing is refused while the network module is unmounted
/// (its token would have no route to be redeemed on), read from the same
/// mounted set `/api/capabilities` reports; a daemon pairing is unaffected.
#[tokio::test]
async fn a_tunnel_pairing_is_refused_while_the_network_module_is_unmounted() {
    let app = TestApp::spawn_with_settings(|s| {
        s.pairing.poll_interval_secs = 0;
        s.modules.network = false;
    })
    .await;
    // The core's copy is what the fleet module reads; it is the host's own
    // answer, so the two can never disagree.
    assert_eq!(app.state.mounted.ids(), app.state.modules.mounted());
    assert!(!app.state.mounted.contains("network"));
    assert!(app.state.mounted.contains("fleet"));
    let caps: Value = app
        .client
        .get(app.url("/api/capabilities"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !caps["modules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "network")
    );

    let (status, refused) = start(&app, &start_body("tunnel", "linux")).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["error"], "network_unmounted");
    assert_eq!(
        audit(&app, "start", "refused:network_unmounted")
            .await
            .len(),
        1
    );

    let (status, started) = start(&app, &start_body("daemon", "linux")).await;
    assert_eq!(status, 200, "{started}");
}

/// The request shapes a `start` refuses — with a code, never silently.
#[tokio::test]
async fn malformed_requests_are_refused_with_400() {
    let app = app_fast().await;
    let org = app.seed_tenant("pair15").await;

    let mut body = start_body("daemon", "linux");
    body["os"] = json!("beos");
    let (status, refused) = start(&app, &body).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["error"], "invalid_request");
    let mut body = start_body("daemon", "linux");
    body["arch"] = json!("mips");
    assert_eq!(start(&app, &body).await.0, 400);
    let mut body = start_body("daemon", "linux");
    body["role"] = json!("agent");
    assert_eq!(start(&app, &body).await.0, 400);
    assert_eq!(
        audit(&app, "start", "refused:invalid_request").await.len(),
        3
    );

    // Odd content in the free-text fields is clamped, never refused.
    let mut body = start_body("daemon", "linux");
    body["machine_name"] = json!(format!("  {}  ", "n".repeat(300)));
    body["client"] = json!("a-client-from-the-future");
    body["client_version"] = json!("<script>alert(1)</script>");
    let (status, started) = start(&app, &body).await;
    assert_eq!(status, 200, "{started}");
    let (status, seen) = lookup(
        &app,
        &org.admin.access_token,
        started["user_code"].as_str().unwrap(),
    )
    .await;
    assert_eq!(status, 200, "{seen}");
    assert_eq!(seen["machine_name"].as_str().unwrap().len(), 64);
    assert_eq!(seen["client"], "other");
    assert_eq!(seen["client_version"], "<script>alert(1)</script>");

    let (status, _) = post_json_as(
        &app,
        "/api/pair/approve",
        &org.admin.access_token,
        &json!({ "user_code": "BCDF-GHJK", "tenant_id": "not-an-id" }),
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = post_json_as(
        &app,
        "/api/pair/lookup",
        &org.admin.access_token,
        &json!({ "nope": 1 }),
    )
    .await;
    assert_eq!(status, 400);
    // Unauthenticated, with pairing on: the routes that need a person say so.
    let (status, _) = post_json_anon(
        &app,
        "/api/pair/lookup",
        &json!({ "user_code": "BCDF-GHJK" }),
    )
    .await;
    assert_eq!(status, 401);
}

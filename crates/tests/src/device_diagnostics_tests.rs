// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! A device's diagnostics, its crash reports (`…/agent/{aid}/crash`) and its
//! uploaded logs (`…/agent/{aid}/logs`), are read under `MANAGE_AGENTS`, and
//! what a device uploads is scrubbed again on the way in.
//!
//! The ordinary device list needs only membership. Crash reports and logs are
//! the least-filtered view of a device, so they sit with the connect code
//! behind the bit an org gives the people who manage its devices:
//!
//! - the bit is decided before the device is looked up, so a caller without it
//!   gets one 403 whatever the id names, and never `not_a_member`, the one 403
//!   the web app leaves the org on;
//! - for a holder the device resolves WITHIN the path tenant: another org's id,
//!   or one that names nothing, is a 404;
//! - a removed device still reads. Both are audit views, and removing a device
//!   must not be the way to hide what it reported (security-baseline.md §4).
//!
//! The scrub half drives the real ingest routes with the shapes the scrub
//! knows (the line a `toml` error quotes, a WireGuard key, a JWT) in structured
//! fields as well as in `msg`, and checks that an ordinary line comes back
//! exactly as it went in.

use crate::fixtures::{seed::SeededTenant, test_app::TestApp};
use bson::oid::ObjectId;
use roomler_ai_db::models::role::permissions::MANAGE_AGENTS;
use roomlerd::crash_recorder::{Payload as CrashPayload, Reason as CrashReason};
use roomlerd::{config::AgentConfig, enrollment};
use serde_json::{Value, json};

/// The 32 bytes 0x00..=0x1f in standard base64: the shape of a WireGuard key,
/// public or private. A test vector, never a key anything uses.
const WG_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

/// A JWT's shape (header `{"typ":"JWT","alg":"HS256"}`), signed by nothing.
const JWT: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMiLCJpYXQiOjE1MTYyMzkwMjJ9.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";

fn url(seeded: &SeededTenant, tail: &str) -> String {
    format!("/api/tenant/{}{}", seeded.tenant_id, tail)
}

async fn get(app: &TestApp, path: &str, token: &str) -> (u16, Value) {
    let resp = app.auth_get(path, token).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn enrol(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> AgentConfig {
    let et: Value = app
        .auth_post(
            &url(seeded, "/agent/enroll-token"),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    enrollment::enroll(enrollment::EnrollInputs {
        server_url: &app.base_url,
        enrollment_token: et["enrollment_token"].as_str().unwrap(),
        machine_id,
        machine_name: "Diagnostics test host",
        install: None,
    })
    .await
    .expect("agent enrollment")
}

async fn create_role(app: &TestApp, seeded: &SeededTenant, name: &str, permissions: u64) -> String {
    let resp = app
        .auth_post(&url(seeded, "/role"), &seeded.admin.access_token)
        .json(&json!({ "name": name, "permissions": permissions }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "owner may mint {name}");
    let role: Value = resp.json().await.unwrap();
    role["id"].as_str().unwrap().to_string()
}

async fn assign_role(app: &TestApp, seeded: &SeededTenant, role_id: &str, user_id: &str) {
    let resp = app
        .auth_post(
            &url(seeded, &format!("/role/{role_id}/assign/{user_id}")),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "owner may assign a role");
}

/// One log line in the uploader's wire shape (`ts` as canonical extended JSON,
/// which is what `bson::DateTime` round-trips through `serde_json`).
fn line(level: &str, target: &str, msg: &str, fields: Value) -> Value {
    json!({
        "ts": { "$date": { "$numberLong": "1762596000000" } },
        "level": level,
        "target": target,
        "msg": msg,
        "fields": fields,
    })
}

/// POST a batch the way the daemon's uploader does: its own token, its own ids.
async fn upload_logs(app: &TestApp, cfg: &AgentConfig, lines: Value) {
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/tenant/{}/agent/{}/logs",
            app.base_url, cfg.tenant_id, cfg.agent_id
        ))
        .bearer_auth(&cfg.agent_token)
        .json(&json!({
            "source": "agent",
            "agent_version": "0.4.150",
            "lines": lines,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        201,
        "the device's batch is accepted"
    );
}

fn crash(summary: &str, log_tail: &str) -> CrashPayload {
    CrashPayload {
        crashed_at_unix: chrono::Utc::now().timestamp(),
        reason: CrashReason::Panic,
        summary: summary.to_string(),
        log_tail: log_tail.to_string(),
        agent_version: "0.4.150".to_string(),
        os: "linux".to_string(),
        hostname: "diag-host".to_string(),
        pid: 4242,
        suppressed_since_last: 0,
    }
}

async fn upload_crash(app: &TestApp, cfg: &AgentConfig, payload: &CrashPayload) {
    let resp = reqwest::Client::new()
        .post(format!("{}/api/agent/crash", app.base_url))
        .bearer_auth(&cfg.agent_token)
        .json(payload)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        201,
        "the device's report is accepted"
    );
}

/// What `toml` prints when a value ends in a stray character: the location,
/// a gutter, the source line it stopped at, a caret, the reason. Chained behind
/// the file's path, as an `error` field carries it.
fn toml_error_quoting(source_line: &str) -> String {
    format!(
        "parse /etc/roomler/config.toml: TOML parse error at line 7, column 71\n  |\n7 | {source_line}\n  |{caret}^\nexpected newline, `#`\n",
        caret = " ".repeat(71)
    )
}

/// Seed one crash report and one log batch on a fresh device.
async fn device_with_diagnostics(
    app: &TestApp,
    seeded: &SeededTenant,
    machine_id: &str,
) -> AgentConfig {
    let cfg = enrol(app, seeded, machine_id).await;
    upload_logs(
        app,
        &cfg,
        json!([line("INFO", "roomlerd::peer", "session started", json!({}))]),
    )
    .await;
    upload_crash(app, &cfg, &crash("worker panicked", "one line of tail")).await;
    cfg
}

/// The audience: `MANAGE_AGENTS` in the path org, decided before the device is
/// looked up. A plain member is refused on both routes, for a real id and for
/// one that names nothing alike; the same member with the bit (and nothing
/// else) reads both; the owner reads both.
#[tokio::test]
async fn device_diagnostics_are_read_under_manage_agents() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("diag-perm").await;
    let cfg = device_with_diagnostics(&app, &seeded, "mach-diag-perm").await;
    let member = &seeded.member.access_token;
    let bogus = ObjectId::new().to_hex();

    for tail in ["crash", "logs"] {
        for id in [cfg.agent_id.as_str(), bogus.as_str()] {
            let path = url(&seeded, &format!("/agent/{id}/{tail}"));
            let (s, body) = get(&app, &path, member).await;
            assert_eq!(s, 403, "a plain member reads no {tail}: {path} {body}");
            // A refusal of THIS read, never "you are not in this org": the web
            // app leaves the org on `not_a_member`, and only on that.
            assert_eq!(body["error"], "forbidden", "{path} {body}");
            assert!(
                body["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("MANAGE_AGENTS")),
                "the refusal names the permission: {body}"
            );
        }
    }

    // Someone outside the org is still told just that.
    let outsider = app.seed_tenant("diag-perm-out").await;
    let (s, body) = get(
        &app,
        &url(&seeded, &format!("/agent/{}/logs", cfg.agent_id)),
        &outsider.admin.access_token,
    )
    .await;
    assert_eq!(s, 403, "{body}");
    assert_eq!(body["error"], "not_a_member", "{body}");

    // The same member with a role that carries MANAGE_AGENTS and nothing else.
    let role = create_role(&app, &seeded, "device-admin", MANAGE_AGENTS).await;
    assign_role(&app, &seeded, &role, &seeded.member.id).await;
    let crash_url = url(&seeded, &format!("/agent/{}/crash", cfg.agent_id));
    let logs_url = url(&seeded, &format!("/agent/{}/logs", cfg.agent_id));
    for token in [member, &seeded.admin.access_token] {
        let (s, body) = get(&app, &crash_url, token).await;
        assert_eq!(s, 200, "a holder reads the crash reports: {body}");
        assert_eq!(body["items"].as_array().map(Vec::len), Some(1), "{body}");
        assert_eq!(body["items"][0]["summary"], "worker panicked", "{body}");
        let (s, body) = get(&app, &logs_url, token).await;
        assert_eq!(s, 200, "a holder reads the logs: {body}");
        assert_eq!(body["batches"].as_array().map(Vec::len), Some(1), "{body}");
        assert_eq!(
            body["batches"][0]["lines"][0]["msg"], "session started",
            "{body}"
        );
    }
}

/// For a holder the device resolves WITHIN the path org: another org's device,
/// named under the caller's own org, answers as an id that names nothing does.
/// A removed device still reads (an audit view, the tombstone table in
/// security-baseline.md §4).
#[tokio::test]
async fn device_diagnostics_resolve_the_device_within_the_tenant() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("diag-scope").await;
    let other = app.seed_tenant("diag-scope-b").await;
    let cfg = device_with_diagnostics(&app, &seeded, "mach-diag-scope").await;
    let owner = &seeded.admin.access_token;

    for tail in ["crash", "logs"] {
        // Another org's owner holds MANAGE_AGENTS in their own org, and names
        // this org's device there.
        let foreign = url(&other, &format!("/agent/{}/{tail}", cfg.agent_id));
        let (s, body) = get(&app, &foreign, &other.admin.access_token).await;
        assert_eq!(s, 404, "another org's device must not resolve: {body}");
        let bogus = url(
            &seeded,
            &format!("/agent/{}/{tail}", ObjectId::new().to_hex()),
        );
        let (s, body) = get(&app, &bogus, owner).await;
        assert_eq!(s, 404, "an id that names nothing: {body}");
    }

    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, cfg.agent_id),
            owner,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the owner removes the device");

    let (s, body) = get(
        &app,
        &url(&seeded, &format!("/agent/{}/crash", cfg.agent_id)),
        owner,
    )
    .await;
    assert_eq!(s, 200, "a removed device's reports still read: {body}");
    assert_eq!(body["items"].as_array().map(Vec::len), Some(1), "{body}");
    let (s, body) = get(
        &app,
        &url(&seeded, &format!("/agent/{}/logs", cfg.agent_id)),
        owner,
    )
    .await;
    assert_eq!(s, 200, "a removed device's logs still read: {body}");
    assert_eq!(body["batches"].as_array().map(Vec::len), Some(1), "{body}");
}

/// The log ingest scrubs every string a line carries, not only `msg`, and knows
/// a WireGuard key and the line a `toml` error quotes as well as a JWT. An
/// ordinary line comes back exactly as it went in.
#[tokio::test]
async fn uploaded_log_lines_are_scrubbed_in_every_field() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("diag-scrub").await;
    let cfg = enrol(&app, &seeded, "mach-diag-scrub").await;

    let parse_error = toml_error_quoting(&format!("overlay_wg_secret_key = \"{WG_KEY}\"x"));
    let ordinary_fields = json!({
        "peer": "6a1f0c2e9b3d4a5f6e7d8c9b",
        "own": "203.0.113.7:41641",
        "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        "n": 3,
        "ok": true,
    });
    upload_logs(
        &app,
        &cfg,
        json!([
            line(
                "ERROR",
                "roomlerd::config",
                "config unreadable — RECOVERED from the previous good copy",
                json!({ "path": "/etc/roomler/config.toml", "error": parse_error }),
            ),
            line(
                "WARN",
                "roomlerd::signaling",
                &format!("overlay key {WG_KEY} refused"),
                json!({ "token": JWT, "auth": format!("Bearer {JWT}") }),
            ),
            line(
                "INFO",
                "roomlerd::overlay",
                "overlay relay: allocated (srflx 203.0.113.7:41641)",
                ordinary_fields.clone(),
            ),
        ]),
    )
    .await;

    let (s, body) = get(
        &app,
        &url(&seeded, &format!("/agent/{}/logs", cfg.agent_id)),
        &seeded.admin.access_token,
    )
    .await;
    assert_eq!(s, 200, "{body}");
    let stored = body.to_string();
    assert!(
        !stored.contains(WG_KEY),
        "a WireGuard key was stored: {stored}"
    );
    assert!(!stored.contains(JWT), "a JWT was stored: {stored}");
    assert!(
        !stored.contains("overlay_wg_secret_key"),
        "the quoted config line was stored: {stored}"
    );

    let lines = &body["batches"][0]["lines"];
    // The parser's error keeps where and why, and loses only what it quoted.
    let error = lines[0]["fields"]["error"]
        .as_str()
        .expect("the error field");
    assert!(
        error.contains("TOML parse error at line 7, column 71"),
        "{error}"
    );
    assert!(error.contains("expected newline"), "{error}");
    assert_eq!(lines[0]["fields"]["path"], "/etc/roomler/config.toml");
    assert_eq!(
        lines[0]["msg"],
        "config unreadable — RECOVERED from the previous good copy"
    );
    // `msg` keeps its words around the key.
    let msg = lines[1]["msg"].as_str().expect("msg");
    assert!(
        msg.starts_with("overlay key ") && msg.ends_with(" refused"),
        "{msg}"
    );
    // The ordinary line: untouched, field by field.
    assert_eq!(
        lines[2]["msg"],
        "overlay relay: allocated (srflx 203.0.113.7:41641)"
    );
    assert_eq!(lines[2]["fields"], ordinary_fields, "{body}");
}

/// A crash report is scrubbed the same way on the way in: the device scrubs
/// its tail before upload, but a daemon that predates a device-side fix sends
/// what it had.
#[tokio::test]
async fn crash_reports_are_scrubbed_on_the_way_in() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("diag-crash-scrub").await;
    let cfg = enrol(&app, &seeded, "mach-diag-crash-scrub").await;

    let tail = format!(
        "2026-11-01T10:00:00.000000Z  INFO roomlerd::app: starting\n\
         2026-11-01T10:00:00.100000Z ERROR roomlerd::config: config unreadable — RECOVERED from the previous good copy error={}\
         2026-11-01T10:00:01.000000Z  WARN roomlerd::overlay: overlay key {WG_KEY} refused\n\
         2026-11-01T10:00:02.000000Z  INFO roomlerd::peer: session started\n",
        toml_error_quoting(&format!("overlay_wg_secret_key = \"{WG_KEY}\"x")),
    );
    upload_crash(
        &app,
        &cfg,
        &crash(&format!("worker panicked: connect with {JWT}"), &tail),
    )
    .await;

    let (s, body) = get(
        &app,
        &url(&seeded, &format!("/agent/{}/crash", cfg.agent_id)),
        &seeded.admin.access_token,
    )
    .await;
    assert_eq!(s, 200, "{body}");
    let stored = body.to_string();
    assert!(
        !stored.contains(WG_KEY),
        "a WireGuard key was stored: {stored}"
    );
    assert!(!stored.contains(JWT), "a JWT was stored: {stored}");
    assert!(
        !stored.contains("overlay_wg_secret_key"),
        "the quoted config line was stored: {stored}"
    );
    let item = &body["items"][0];
    let summary = item["summary"].as_str().expect("summary");
    assert!(
        summary.starts_with("worker panicked: connect with "),
        "{summary}"
    );
    let stored_tail = item["logTail"].as_str().expect("logTail");
    // Every line that held nothing comes back as it went in.
    for kept in [
        "2026-11-01T10:00:00.000000Z  INFO roomlerd::app: starting\n",
        "TOML parse error at line 7, column 71\n",
        "expected newline, `#`\n",
        "2026-11-01T10:00:02.000000Z  INFO roomlerd::peer: session started\n",
    ] {
        assert!(stored_tail.contains(kept), "lost {kept:?}: {stored_tail}");
    }
    assert_eq!(item["hostname"], "diag-host");
    assert_eq!(item["agentVersion"], "0.4.150");
}

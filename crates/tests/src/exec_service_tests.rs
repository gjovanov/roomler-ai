// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-94 — Fleet RPC as SYSTEM on Windows, the server's half: the anchor, the
//! single-use claim, the result, and the org's `exec_require_system`.
//!
//! The device side is a RAW agent socket whose hello advertises
//! `exec-system`. The test itself plays the SYSTEM service, with the
//! credential the anchor minted. That is the whole contract a service relies
//! on, and it is what these lock:
//! - nothing runs without a claim;
//! - a claim is single use, this device's only, inside its window, and
//!   refused once withdrawn, by the caller's cancel or the worker's own answer;
//! - a result can land on another pod and still reach the caller;
//! - one live SYSTEM service per device: re-anchoring or un-anchoring
//!   revokes the earlier credential;
//! - the agent token and the service credential never stand in for each
//!   other (FR-94 §4).

use crate::fixtures::test_app::TestApp;
use crate::tunnel_tests::{read_until, wait_agent_online};
use futures::SinkExt;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::protocol::Message,
};

type AgentWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Enroll a WINDOWS device (the shared `enroll_agent` enrolls Linux).
async fn enroll_windows(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    machine_id: &str,
    machine_name: &str,
) -> (String, String) {
    enroll_os(app, seeded, machine_id, machine_name, "windows").await
}

async fn enroll_os(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    machine_id: &str,
    machine_name: &str,
    os: &str,
) -> (String, String) {
    let ej: Value = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": enrollment_token(app, seeded).await,
            "machine_id": machine_id,
            "machine_name": machine_name,
            "os": os,
            "agent_version": "0.4.131",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        ej["agent_id"].as_str().unwrap().to_string(),
        ej["agent_token"].as_str().unwrap().to_string(),
    )
}

/// A fresh single-use enrollment token from the org's admin.
async fn enrollment_token(app: &TestApp, seeded: &crate::fixtures::seed::SeededTenant) -> String {
    let et: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    et["enrollment_token"].as_str().unwrap().to_string()
}

/// A raw agent socket whose hello advertises `rpc`.
async fn connect_agent(app: &TestApp, token: &str, machine: &str, rpc: &[&str]) -> AgentWs {
    let url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        token
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    );
    let (mut ws, _) = connect_async(&url).await.expect("agent ws connect");
    ws.send(Message::Text(
        json!({
            "t": "rc:agent.hello",
            "machine_name": machine,
            "os": "windows",
            "agent_version": "0.4.131",
            "displays": [],
            "caps": {
                "hw_encoders": [], "codecs": ["h264"], "has_input_permission": true,
                "supports_clipboard": false, "supports_file_transfer": false,
                "max_simultaneous_sessions": 4, "rpc": rpc,
            },
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    ws
}

async fn set_org_exec(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    body: Value,
) -> Value {
    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/exec-settings", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "org exec: {:?}", resp.status());
    resp.json().await.unwrap()
}

async fn set_device_policy(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    agent_id: &str,
) {
    let resp = put_policy(
        app,
        seeded,
        agent_id,
        json!({ "mode": "on", "consent_mode": "auto" }),
    )
    .await;
    assert!(resp.status().is_success(), "policy: {:?}", resp.status());
}

/// `PUT …/exec-policy` with `body`, answered as it came.
async fn put_policy(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    agent_id: &str,
    body: Value,
) -> reqwest::Response {
    app.auth_put(
        &format!(
            "/api/tenant/{}/agent/{}/exec-policy",
            seeded.tenant_id, agent_id
        ),
        &seeded.admin.access_token,
    )
    .json(&body)
    .send()
    .await
    .unwrap()
}

/// `POST /api/agent/anchor`, the request `roomlerd anchor` sends.
async fn anchor_req(app: &TestApp, body: Value) -> reqwest::Response {
    app.client
        .post(app.url("/api/agent/anchor"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// Anchor `agent_id` (named `host`) and return the service credential.
async fn anchor(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    agent_id: &str,
    host: &str,
) -> String {
    let resp = anchor_req(
        app,
        json!({
            "enrollment_token": enrollment_token(app, seeded).await,
            "agent_id": agent_id,
            "host_name": host,
        }),
    )
    .await;
    assert!(resp.status().is_success(), "anchor: {:?}", resp.status());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["agent_id"], json!(agent_id));
    body["service_token"].as_str().unwrap().to_string()
}

/// Start an exec in the background: it waits on the device's answer.
fn spawn_exec(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    agent_id: &str,
    command: &str,
) -> tokio::task::JoinHandle<Value> {
    let req = app
        .auth_post(
            &format!("/api/tenant/{}/agent/{}/exec", seeded.tenant_id, agent_id),
            &seeded.admin.access_token,
        )
        .json(&json!({ "command": command, "timeout_ms": 20_000 }));
    tokio::spawn(async move {
        let resp = req.send().await.unwrap();
        assert!(resp.status().is_success(), "exec: {:?}", resp.status());
        resp.json().await.unwrap()
    })
}

async fn claim(app: &TestApp, token: &str, request_id: &str) -> reqwest::Response {
    app.auth_post(&format!("/api/agent/service/rpc/{request_id}/claim"), token)
        .send()
        .await
        .unwrap()
}

async fn post_result(
    app: &TestApp,
    token: &str,
    request_id: &str,
    outcome: Value,
) -> reqwest::Response {
    app.auth_post(
        &format!("/api/agent/service/rpc/{request_id}/result"),
        token,
    )
    .json(&outcome)
    .send()
    .await
    .unwrap()
}

/// The worker's own answer to a `via_service` frame: what a refusal, a
/// declined prompt or an unanswered doorbell looks like on the wire.
async fn worker_answers(ws: &mut AgentWs, request_id: &str, error: &str) {
    ws.send(Message::Text(
        json!({ "t": "rc:rpc.result", "request_id": request_id, "error": error })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
}

async fn renew(app: &TestApp, token: &str) -> reqwest::Response {
    app.auth_post("/api/agent/service/renew", token)
        .send()
        .await
        .unwrap()
}

async fn unanchor(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    agent_id: &str,
    access_token: &str,
) -> reqwest::Response {
    app.auth_delete(
        &format!(
            "/api/tenant/{}/agent/{}/exec-anchor",
            seeded.tenant_id, agent_id
        ),
        access_token,
    )
    .send()
    .await
    .unwrap()
}

async fn audit_rows(app: &TestApp, seeded: &crate::fixtures::seed::SeededTenant) -> Vec<Value> {
    let body: Value = app
        .auth_get(
            &format!("/api/tenant/{}/exec-audit", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["items"].as_array().cloned().unwrap_or_default()
}

/// The anchor mints a service credential for the NAMED Windows device and
/// records who anchored it; the credential claims, the agent token does
/// not, and neither passes for the other.
#[tokio::test]
async fn anchor_mints_a_service_credential_that_only_the_service_routes_accept() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr94anchor").await;
    let (agent_id, agent_token) = enroll_windows(&app, &seeded, "m-fr94-anchor", "HOST-A").await;

    // The host name is matched without regard to case.
    let service = anchor(&app, &seeded, &agent_id, "host-a").await;

    // The row records the anchor: who and when, never the credential.
    let row: Value = app
        .auth_get(
            &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, agent_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        row.get("exec_anchor").is_some_and(|a| !a.is_null()),
        "the device row should record the anchor: {row}"
    );
    assert!(
        !row.to_string().contains(&service),
        "the credential must never be stored"
    );

    // The service credential is ACCEPTED by the claim route: an unknown
    // command is a 404 (refused, nothing pending), not a 401.
    assert_eq!(claim(&app, &service, "no-such-request").await.status(), 404);
    // The agent token is not: the user on an attended install can read it.
    assert_eq!(
        claim(&app, &agent_token, "no-such-request").await.status(),
        401
    );
    // And the service credential is no agent token either.
    let as_agent = app
        .auth_get("/api/agent/self/devices", &service)
        .send()
        .await
        .unwrap();
    assert_eq!(
        as_agent.status(),
        401,
        "the service credential must not pass as an agent"
    );
}

/// What the anchor refuses: a host that is not the device (without burning
/// the token), another org's device, a non-Windows device, a used token.
#[tokio::test]
async fn anchor_refuses_the_wrong_host_another_org_a_non_windows_device_and_a_used_token() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr94anchorno").await;
    let other = app.seed_tenant("fr94anchorother").await;
    let (agent_id, _) = enroll_windows(&app, &seeded, "m-fr94-no", "HOST-B").await;
    let (linux_id, _) = enroll_os(&app, &seeded, "m-fr94-lx", "lx", "linux").await;
    let (foreign_id, _) = enroll_windows(&app, &other, "m-fr94-foreign", "HOST-F").await;

    // The wrong host: 409, and the token is NOT burned.
    let token = enrollment_token(&app, &seeded).await;
    let wrong = anchor_req(
        &app,
        json!({ "enrollment_token": token, "agent_id": agent_id, "host_name": "HOST-X" }),
    )
    .await;
    assert_eq!(wrong.status(), 409);
    let right = anchor_req(
        &app,
        json!({ "enrollment_token": token, "agent_id": agent_id, "host_name": "HOST-B" }),
    )
    .await;
    assert_eq!(
        right.status(),
        200,
        "a refused anchor must not burn the token"
    );
    // The same token again: single use.
    let again = anchor_req(
        &app,
        json!({ "enrollment_token": token, "agent_id": agent_id, "host_name": "HOST-B" }),
    )
    .await;
    assert_eq!(again.status(), 401);

    // Another org's device, with THIS org's token: as if it did not exist.
    let foreign = anchor_req(
        &app,
        json!({
            "enrollment_token": enrollment_token(&app, &seeded).await,
            "agent_id": foreign_id, "host_name": "HOST-F",
        }),
    )
    .await;
    assert_eq!(foreign.status(), 404);

    // A Linux device: its daemon already runs as root.
    let linux = anchor_req(
        &app,
        json!({
            "enrollment_token": enrollment_token(&app, &seeded).await,
            "agent_id": linux_id, "host_name": "lx",
        }),
    )
    .await;
    assert_eq!(linux.status(), 400);

    // `force` anchors a renamed host.
    let forced = anchor_req(
        &app,
        json!({
            "enrollment_token": enrollment_token(&app, &seeded).await,
            "agent_id": agent_id, "host_name": "RENAMED", "force": true,
        }),
    )
    .await;
    assert_eq!(forced.status(), 200);
}

/// End to end on one pod: an anchored device gets `via_service`, the
/// command waits for the claim, the claim is single use and this device's
/// only, and the posted result reaches the caller and the audit as `system`.
#[tokio::test]
async fn exec_runs_through_the_service_and_the_claim_is_single_use() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr94svc").await;
    let (agent_id, agent_token) = enroll_windows(&app, &seeded, "m-fr94-svc", "HOST-S").await;
    let (other_id, _) = enroll_windows(&app, &seeded, "m-fr94-svc2", "HOST-T").await;
    let mut ws = connect_agent(&app, &agent_token, "HOST-S", &["exec", "exec-system"]).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    set_org_exec(&app, &seeded, json!({ "remote_exec_enabled": true })).await;
    set_device_policy(&app, &seeded, &agent_id).await;
    let service = anchor(&app, &seeded, &agent_id, "HOST-S").await;
    let other_service = anchor(&app, &seeded, &other_id, "HOST-T").await;

    let exec = spawn_exec(&app, &seeded, &agent_id, "whoami");
    let frame = read_until(&mut ws, "rc:rpc.exec")
        .await
        .expect("the worker should be told to ring its service");
    assert_eq!(frame["via_service"], json!(true), "{frame}");
    let request_id = frame["request_id"].as_str().unwrap().to_string();

    // The claim closes early: with no consent prompt, CLAIM_WINDOW after the
    // dispatch, so a command claimed at the last moment still has its whole
    // 20 s budget inside the caller's deadline (20 s + RESULT_GRACE).
    let row = app
        .db
        .collection::<bson::Document>("exec_pending")
        .find_one(bson::doc! { "request_id": &request_id })
        .await
        .unwrap()
        .expect("the dispatch queued the command");
    let window = row.get_datetime("claim_by").unwrap().timestamp_millis()
        - row.get_datetime("created_at").unwrap().timestamp_millis();
    assert_eq!(
        window, 10_000,
        "the claim window, not the caller's deadline"
    );

    // Another device's service cannot take it, even in the same org.
    assert_eq!(claim(&app, &other_service, &request_id).await.status(), 404);
    // This device's service can, once.
    let first = claim(&app, &service, &request_id).await;
    assert_eq!(first.status(), 200);
    let claimed: Value = first.json().await.unwrap();
    assert_eq!(claimed["command"], json!("whoami"));
    assert_eq!(claimed["run_as"], json!("system"));
    assert_eq!(
        claim(&app, &service, &request_id).await.status(),
        404,
        "single use"
    );

    // A result for something this device never claimed is dropped.
    assert_eq!(
        post_result(&app, &service, "never-claimed", json!({ "exit_code": 0 }))
            .await
            .status(),
        404
    );
    let posted = post_result(
        &app,
        &service,
        &request_id,
        json!({ "exit_code": 0, "stdout": "nt authority\\system\r\n", "ran_as": "system" }),
    )
    .await;
    assert_eq!(posted.status(), 200);

    let out = tokio::time::timeout(Duration::from_secs(10), exec)
        .await
        .expect("the caller should resolve")
        .unwrap();
    assert_eq!(out["error"], Value::Null, "{out}");
    assert_eq!(out["exit_code"], json!(0));
    assert!(
        out["stdout"]
            .as_str()
            .unwrap()
            .contains("nt authority\\system")
    );

    let rows = audit_rows(&app, &seeded).await;
    // The refused claim below carries the same `request_id` and is newer
    // (the run's row is stamped when the dispatch began), so pick the row
    // that ran.
    let ran = rows
        .iter()
        .find(|r| r["request_id"] == json!(request_id) && r["denied"].is_null())
        .expect("the run is audited");
    assert_eq!(ran["ran_as"], json!("system"));
    // The refused claim (the other device's) is audited too, as
    // `claim_refused` with no command.
    assert!(
        rows.iter()
            .any(|r| r["denied"] == json!("claim_refused") && r["source"] == json!("service-claim")),
        "refused claims must be audited: {rows:?}"
    );
}

/// One live SYSTEM service per device. A renewal keeps the anchor; anchoring
/// again revokes every credential the earlier anchor minted; un-anchoring
/// revokes them all and sends commands back to the worker, although the
/// device still advertises `exec-system`. Only a device manager un-anchors.
///
/// "Accepted" below is a 404 from the claim route (nothing pending), and
/// "revoked" a 401.
#[tokio::test]
async fn re_anchoring_and_un_anchoring_revoke_the_service_credential() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr94revoke").await;
    let (agent_id, agent_token) = enroll_windows(&app, &seeded, "m-fr94-rev", "HOST-V").await;
    let mut ws = connect_agent(&app, &agent_token, "HOST-V", &["exec", "exec-system"]).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    set_org_exec(&app, &seeded, json!({ "remote_exec_enabled": true })).await;
    set_device_policy(&app, &seeded, &agent_id).await;

    let first = anchor(&app, &seeded, &agent_id, "HOST-V").await;
    let renewed_resp = renew(&app, &first).await;
    assert_eq!(renewed_resp.status(), 200);
    let renewed: Value = renewed_resp.json().await.unwrap();
    let renewed = renewed["service_token"].as_str().unwrap().to_string();
    assert_eq!(claim(&app, &renewed, "none").await.status(), 404, "renewed");
    assert_eq!(
        claim(&app, &first, "none").await.status(),
        404,
        "still live"
    );

    // Anchoring again: both earlier credentials die, the new one lives.
    let second = anchor(&app, &seeded, &agent_id, "HOST-V").await;
    assert_eq!(claim(&app, &first, "none").await.status(), 401);
    assert_eq!(claim(&app, &renewed, "none").await.status(), 401);
    assert_eq!(
        renew(&app, &first).await.status(),
        401,
        "a dead one cannot renew"
    );
    assert_eq!(claim(&app, &second, "none").await.status(), 404);

    // A member without MANAGE_AGENTS cannot un-anchor.
    let refused = unanchor(&app, &seeded, &agent_id, &seeded.member.access_token).await;
    assert_eq!(refused.status(), 403);
    assert_eq!(claim(&app, &second, "none").await.status(), 404);

    let done = unanchor(&app, &seeded, &agent_id, &seeded.admin.access_token).await;
    assert_eq!(done.status(), 200);
    assert_eq!(claim(&app, &second, "none").await.status(), 401);

    // The device still advertises `exec-system`, but no anchor stands behind
    // it now: the command goes to the worker, as before FR-94.
    let exec = spawn_exec(&app, &seeded, &agent_id, "whoami");
    let frame = read_until(&mut ws, "rc:rpc.exec").await.expect("frame");
    assert!(
        frame.get("via_service").is_none_or(|v| v == &json!(false)),
        "an un-anchored device must not be routed to a service: {frame}"
    );
    exec.abort();
}

/// The worker can answer a `via_service` frame itself: a declined prompt, a
/// doorbell nobody answered. Before any claim, that answer WITHDRAWS the
/// command, so a late claim cannot run what the caller was told did not run.
/// After a claim, the command is running, and the caller gets the service's
/// result, not the worker's.
#[tokio::test]
async fn a_worker_answer_withdraws_the_command_unless_the_service_already_runs_it() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr94worker").await;
    let (agent_id, agent_token) = enroll_windows(&app, &seeded, "m-fr94-wa", "HOST-W").await;
    let mut ws = connect_agent(&app, &agent_token, "HOST-W", &["exec", "exec-system"]).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    set_org_exec(&app, &seeded, json!({ "remote_exec_enabled": true })).await;
    set_device_policy(&app, &seeded, &agent_id).await;
    let service = anchor(&app, &seeded, &agent_id, "HOST-W").await;

    // Before any claim: the worker's answer stands, and the row is withdrawn.
    let exec = spawn_exec(&app, &seeded, &agent_id, "whoami");
    let frame = read_until(&mut ws, "rc:rpc.exec").await.expect("frame");
    let first = frame["request_id"].as_str().unwrap().to_string();
    worker_answers(&mut ws, &first, "declined at the device").await;
    let out = tokio::time::timeout(Duration::from_secs(10), exec)
        .await
        .expect("the worker's answer should resolve the caller")
        .unwrap();
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("declined")),
        "{out}"
    );
    assert_eq!(
        claim(&app, &service, &first).await.status(),
        404,
        "a late claim must not run a command the caller was told did not run"
    );

    // After the claim: the worker's answer does not end the wait.
    let exec = spawn_exec(&app, &seeded, &agent_id, "hostname");
    let frame = read_until(&mut ws, "rc:rpc.exec").await.expect("frame");
    let second = frame["request_id"].as_str().unwrap().to_string();
    assert_eq!(claim(&app, &service, &second).await.status(), 200);
    worker_answers(&mut ws, &second, "the service did not answer the doorbell").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !exec.is_finished(),
        "the service is running it: keep waiting"
    );
    let posted = post_result(
        &app,
        &service,
        &second,
        json!({ "exit_code": 0, "stdout": "HOST-W\r\n", "ran_as": "system" }),
    )
    .await;
    assert_eq!(posted.status(), 200);
    let out = tokio::time::timeout(Duration::from_secs(10), exec)
        .await
        .expect("the service's result should resolve the caller")
        .unwrap();
    assert_eq!(out["error"], Value::Null, "{out}");
    assert!(out["stdout"].as_str().unwrap().contains("HOST-W"), "{out}");
}

/// AC5 — the device's socket and the caller are on pod A; the service's claim
/// and result land on pod B. The caller still resolves, through the shared
/// row.
#[tokio::test]
async fn a_claim_and_result_on_the_other_pod_still_reach_the_caller() {
    let (a, b) = TestApp::spawn_pair(|_| {}).await;
    let seeded = a.seed_tenant("fr94pods").await;
    let (agent_id, agent_token) = enroll_windows(&a, &seeded, "m-fr94-pods", "HOST-P").await;
    let mut ws = connect_agent(&a, &agent_token, "HOST-P", &["exec", "exec-system"]).await;
    wait_agent_online(&a, &seeded, &agent_id).await;
    set_org_exec(&a, &seeded, json!({ "remote_exec_enabled": true })).await;
    set_device_policy(&a, &seeded, &agent_id).await;
    let service = anchor(&a, &seeded, &agent_id, "HOST-P").await;

    let exec = spawn_exec(&a, &seeded, &agent_id, "hostname");
    let frame = read_until(&mut ws, "rc:rpc.exec").await.expect("frame");
    let request_id = frame["request_id"].as_str().unwrap().to_string();

    assert_eq!(claim(&b, &service, &request_id).await.status(), 200);
    let posted = post_result(
        &b,
        &service,
        &request_id,
        json!({ "exit_code": 0, "stdout": "HOST-P\r\n", "ran_as": "system" }),
    )
    .await;
    assert_eq!(posted.status(), 200);
    let body: Value = posted.json().await.unwrap();
    assert_eq!(
        body["delivered_here"],
        json!(false),
        "the caller waits on pod A"
    );

    let out = tokio::time::timeout(Duration::from_secs(10), exec)
        .await
        .expect("the caller on pod A should resolve through the shared row")
        .unwrap();
    assert_eq!(out["exit_code"], json!(0), "{out}");
    assert!(out["stdout"].as_str().unwrap().contains("HOST-P"));
}

/// A command withdrawn before any claim can never be claimed, and a claim
/// after `claim_by` is refused although the row is still there.
#[tokio::test]
async fn a_withdrawn_or_expired_command_cannot_be_claimed() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr94cancel").await;
    let (agent_id, agent_token) = enroll_windows(&app, &seeded, "m-fr94-cancel", "HOST-C").await;
    let mut ws = connect_agent(&app, &agent_token, "HOST-C", &["exec", "exec-system"]).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    set_org_exec(&app, &seeded, json!({ "remote_exec_enabled": true })).await;
    set_device_policy(&app, &seeded, &agent_id).await;
    let service = anchor(&app, &seeded, &agent_id, "HOST-C").await;

    let exec = spawn_exec(&app, &seeded, &agent_id, "whoami");
    let frame = read_until(&mut ws, "rc:rpc.exec").await.expect("frame");
    let request_id = frame["request_id"].as_str().unwrap().to_string();
    let cancel = app
        .auth_post(
            &format!(
                "/api/tenant/{}/agent/{}/exec/{}/cancel",
                seeded.tenant_id, agent_id, request_id
            ),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(
        cancel.status().is_success(),
        "cancel: {:?}",
        cancel.status()
    );
    assert_eq!(
        claim(&app, &service, &request_id).await.status(),
        404,
        "withdrawn"
    );
    exec.abort();

    // Expired: a pending row whose `claim_by` has passed.
    let expired_id = "fr94-expired-1";
    let now = bson::DateTime::now().timestamp_millis();
    app.db
        .collection::<bson::Document>("exec_pending")
        .insert_one(bson::doc! {
            "request_id": expired_id,
            "tenant_id": bson::oid::ObjectId::parse_str(&seeded.tenant_id).unwrap(),
            "agent_id": bson::oid::ObjectId::parse_str(&agent_id).unwrap(),
            "shell": "powershell", "command": "whoami",
            "timeout_ms": 1000_i64, "max_output_bytes": 1024_i64,
            "caller": "test", "run_as": "system", "state": "pending",
            "created_at": bson::DateTime::from_millis(now - 60_000),
            "claim_by": bson::DateTime::from_millis(now - 1_000),
            "expires_at": bson::DateTime::from_millis(now + 600_000),
        })
        .await
        .unwrap();
    assert_eq!(
        claim(&app, &service, expired_id).await.status(),
        404,
        "expired"
    );
}

/// The org's `exec_require_system`: an unanchored Windows device is refused
/// rather than run as the signed-in user, and a PUT that does not name the
/// field leaves it alone.
#[tokio::test]
async fn exec_require_system_refuses_an_unanchored_windows_device() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr94require").await;
    let (agent_id, agent_token) = enroll_windows(&app, &seeded, "m-fr94-req", "HOST-R").await;
    // `exec` only: no anchor, no SYSTEM service.
    let _ws = connect_agent(&app, &agent_token, "HOST-R", &["exec"]).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    set_device_policy(&app, &seeded, &agent_id).await;

    let set = set_org_exec(
        &app,
        &seeded,
        json!({ "remote_exec_enabled": true, "exec_require_system": true }),
    )
    .await;
    assert_eq!(set["exec_require_system"], json!(true));
    // A client before FR-94 names only the kill-switch: the field stays.
    let after = set_org_exec(&app, &seeded, json!({ "remote_exec_enabled": true })).await;
    assert_eq!(
        after["exec_require_system"],
        json!(true),
        "absent means leave it alone"
    );

    let out = spawn_exec(&app, &seeded, &agent_id, "whoami")
        .await
        .unwrap();
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("requires Windows commands to run as SYSTEM")),
        "{out}"
    );
    let rows = audit_rows(&app, &seeded).await;
    assert_eq!(rows[0]["denied"], json!("system_required"));
}

/// FR-94 P4 — a policy that names the signed-in user reaches the service in
/// the claim, and the audit records that identity, not SYSTEM.
#[tokio::test]
async fn console_user_runs_through_the_service_and_is_audited_as_such() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr94cu").await;
    let (agent_id, agent_token) = enroll_windows(&app, &seeded, "m-fr94-cu", "HOST-U").await;
    let mut ws = connect_agent(&app, &agent_token, "HOST-U", &["exec", "exec-system"]).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    set_org_exec(&app, &seeded, json!({ "remote_exec_enabled": true })).await;
    let set = put_policy(
        &app,
        &seeded,
        &agent_id,
        json!({ "mode": "on", "consent_mode": "auto", "run_as": "console_user" }),
    )
    .await;
    assert!(set.status().is_success(), "{:?}", set.status());
    let service = anchor(&app, &seeded, &agent_id, "HOST-U").await;

    let exec = spawn_exec(&app, &seeded, &agent_id, "whoami");
    let frame = read_until(&mut ws, "rc:rpc.exec").await.expect("frame");
    let request_id = frame["request_id"].as_str().unwrap().to_string();
    let claimed = claim(&app, &service, &request_id).await;
    assert_eq!(claimed.status(), 200);
    let claimed: Value = claimed.json().await.unwrap();
    assert_eq!(claimed["run_as"], json!("console_user"));
    let posted = post_result(
        &app,
        &service,
        &request_id,
        json!({ "exit_code": 0, "stdout": "host-u person\r\n", "ran_as": "console_user" }),
    )
    .await;
    assert_eq!(posted.status(), 200);
    let out = tokio::time::timeout(Duration::from_secs(10), exec)
        .await
        .expect("the caller should resolve")
        .unwrap();
    assert_eq!(out["ran_as"], json!("console_user"), "{out}");

    let rows = audit_rows(&app, &seeded).await;
    let ran = rows
        .iter()
        .find(|r| r["request_id"] == json!(request_id) && r["denied"].is_null())
        .expect("the run is audited");
    assert_eq!(ran["ran_as"], json!("console_user"));
}

/// Without the service, `console_user` is refused rather than run as the
/// worker: in SystemContext mode the worker IS SYSTEM, the opposite of what
/// the policy says.
#[tokio::test]
async fn console_user_without_the_service_is_refused() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr94cuno").await;
    let (agent_id, agent_token) = enroll_windows(&app, &seeded, "m-fr94-cuno", "HOST-N").await;
    // `exec` only: no anchor, no service.
    let _ws = connect_agent(&app, &agent_token, "HOST-N", &["exec"]).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    set_org_exec(&app, &seeded, json!({ "remote_exec_enabled": true })).await;
    let set = put_policy(
        &app,
        &seeded,
        &agent_id,
        json!({ "mode": "on", "consent_mode": "auto", "run_as": "console_user" }),
    )
    .await;
    assert!(set.status().is_success(), "{:?}", set.status());

    let out = spawn_exec(&app, &seeded, &agent_id, "whoami")
        .await
        .unwrap();
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("only its SYSTEM service can do")),
        "{out}"
    );
    let rows = audit_rows(&app, &seeded).await;
    assert_eq!(rows[0]["denied"], json!("run_as_unavailable"));
}

/// `console_user` is a Windows mode, and a PUT that does not name `run_as`
/// keeps the stored one: the policy PUT is a full replace, and a stale client
/// saving it must not turn `console_user` back into SYSTEM.
#[tokio::test]
async fn run_as_is_windows_only_and_survives_a_put_that_omits_it() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr94ra").await;
    let (linux_id, _) = enroll_os(&app, &seeded, "m-fr94-ra-l", "HOST-L", "linux").await;
    let refused = put_policy(
        &app,
        &seeded,
        &linux_id,
        json!({ "mode": "on", "run_as": "console_user" }),
    )
    .await;
    assert_eq!(refused.status(), 400);

    let (win_id, _) = enroll_windows(&app, &seeded, "m-fr94-ra-w", "HOST-W2").await;
    let set: Value = put_policy(
        &app,
        &seeded,
        &win_id,
        json!({ "mode": "on", "run_as": "console_user" }),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(set["run_as"], json!("console_user"));
    let kept: Value = put_policy(&app, &seeded, &win_id, json!({ "mode": "on" }))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        kept["run_as"],
        json!("console_user"),
        "absent means keep it"
    );
    let back: Value = put_policy(
        &app,
        &seeded,
        &win_id,
        json!({ "mode": "on", "run_as": "system" }),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(back["run_as"], json!("system"));
}

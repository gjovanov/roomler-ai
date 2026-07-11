//! Phase 5 — tunnel subsystem integration tests.
//!
//! Covers the REST + ACL surface end-to-end against a live `TestApp`.
//! Higher-leverage than unit tests in `crates/tunnel-core/` because
//! it exercises the full request → DAO → response cycle (incl. JSON
//! adjacent-tagged serde for `PolicySubject` / `PolicyTarget` /
//! `DestinationRule`), the cross-tenant gate, and the tunnel-client
//! enrollment idempotence the rehydrate path is supposed to guarantee.
//!
//! Tests:
//!
//! * `policy_crud_round_trips` — POST → GET list → GET one → PUT →
//!   GET one (verifies update applied) → DELETE → gone-from-listing.
//!   Locks the policy CRUD wire shape (`{kind, id}` subjects) that the
//!   admin UI consumes; a serde drift on any of the adjacently-tagged
//!   enums (e.g. someone renames `kind` to `t`) would break the admin
//!   UI silently — this catches it server-side first.
//!
//! * `tunnel_client_enrollment_idempotent_on_same_machine_id` —
//!   enrol → enrol again with the same `machine_id` → assert the
//!   second response carries the same `tunnel_client_id`. Locks the
//!   `find_by_tenant_and_machine` + `rehydrate` path. Without this
//!   the operator who re-runs `roomler-tunnel enroll` after a config
//!   loss would mint a fresh row each time and the listing UI would
//!   show ghosts.
//!
//! * `cross_tenant_enrollment_rejected` — admin from tenant A
//!   cannot issue an enrollment token for tenant B. Validates the
//!   `is_member` gate at the route level. Sev0 per the tunnel
//!   architecture (cross-tenant boundary is the only defence
//!   against an admin in tenant A exfiltrating an enrollment token
//!   that grants tunnel access into tenant B's agents).
//!
//! * `agent_originates_tunnel_authorized_and_relayed_to_target` (P3b-2)
//!   — an agent drives the tunnel-CLIENT role over its own agent WS
//!   (`Principal::Agent`), opens a tunnel to another agent, and the
//!   server authorizes it (via an `all_users` policy) + relays the TCP
//!   forward to the target. Locks the identity-model-(b) origination
//!   path end-to-end.
//!
//! * `agent_tunnel_open_cross_tenant_rejected` (P3b-2) — the cross-tenant
//!   wall holds for an agent origin (a tenant-1 agent must not open a
//!   tunnel to a tenant-2 agent).
//!
//! WebRTC DC round-trip (agent + tunnel-client + echo server +
//! TCP byte exchange) is NOT covered here — the existing in-process
//! ICE flakiness in `agent_tests::agent_answers_sdp_offer_with_real_
//! webrtc_peer` (see its "best-effort" comment) makes it too
//! unstable for CI without a TURN fixture. Cluster-side validation
//! happens via the Phase 5 cluster harness (Chunk 5B, follow-on).

use crate::fixtures::test_app::TestApp;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

#[tokio::test]
async fn policy_crud_round_trips() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tunnel-policy-crud").await;

    // --- CREATE ---
    // Subjects: the seeded admin user.
    // Targets: AllAgents (catch-all — tenant-wide).
    // Allowlist: one rule allowing the loopback range on a high port.
    // Wire shape locks (see crates/remote_control/src/models.rs):
    //   - HostPattern: adjacently-tagged `{kind, value}`,
    //     variants snake_cased — `cidr` / `exact` / `wildcard`.
    //   - PolicySubject: internally-tagged `{kind, id}` — `kind` names
    //     the variant (`user_id` / `role_id` / `tunnel_client_id` /
    //     `agent_id` / `all_users`) and the data field is ALWAYS `id`
    //     (`#[serde(rename = "id")]`), NOT the kind's own name. Locked
    //     against the admin UI, which posts `{kind, id}` (see
    //     ui/src/stores/tunnelPolicies.ts). `all_users` is a bare
    //     `{kind: "all_users"}` unit.
    //   - PolicyTarget: internally-tagged the same way — `{kind, id}`
    //     for `agent_id`, bare `{kind}` for `all_agents`.
    let create_body = json!({
        "name": "loopback-test",
        "subjects": [
            { "kind": "user_id", "id": seeded.admin.id }
        ],
        "targets": [
            { "kind": "all_agents" }
        ],
        "allowlist": [
            {
                "host_pattern": { "kind": "cidr", "value": "127.0.0.0/8" },
                "port_range": { "low": 9000, "high": 9999 }
            }
        ],
        "max_concurrent_flows": 32,
        "max_bytes_per_session": 1048576
    });
    let created: Value = app
        .auth_post(
            &format!("/api/tenant/{}/tunnel-policy", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&create_body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let policy_id = created["id"]
        .as_str()
        .expect("policy id in create response");
    assert_eq!(created["name"].as_str(), Some("loopback-test"));
    assert_eq!(
        created["tenant_id"].as_str(),
        Some(seeded.tenant_id.as_str()),
        "policy must be tenant-scoped"
    );
    assert_eq!(
        created["max_concurrent_flows"].as_u64(),
        Some(32),
        "max_concurrent_flows round-trips"
    );

    // --- LIST ---
    let list: Value = app
        .auth_get(
            &format!("/api/tenant/{}/tunnel-policy", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = list["items"].as_array().expect("items in list response");
    assert_eq!(items.len(), 1, "exactly one policy");
    assert_eq!(items[0]["id"].as_str(), Some(policy_id));

    // --- GET one ---
    let fetched: Value = app
        .auth_get(
            &format!(
                "/api/tenant/{}/tunnel-policy/{}",
                seeded.tenant_id, policy_id
            ),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(fetched["name"].as_str(), Some("loopback-test"));
    // Adjacent-tag wire-shape lock: the allowlist must round-trip with
    // the exact `kind` discriminator the admin UI's TypeScript expects.
    let allowlist = fetched["allowlist"].as_array().expect("allowlist array");
    assert_eq!(allowlist.len(), 1);
    assert_eq!(
        allowlist[0]["host_pattern"]["kind"].as_str(),
        Some("cidr"),
        "host_pattern.kind must serialise as adjacently-tagged 'cidr'"
    );
    assert_eq!(
        allowlist[0]["host_pattern"]["value"].as_str(),
        Some("127.0.0.0/8")
    );

    // --- UPDATE (rename + clear max_concurrent_flows via null) ---
    let update_body = json!({
        "name": "loopback-renamed",
        "max_concurrent_flows": null
    });
    let updated: Value = app
        .auth_put(
            &format!(
                "/api/tenant/{}/tunnel-policy/{}",
                seeded.tenant_id, policy_id
            ),
            &seeded.admin.access_token,
        )
        .json(&update_body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        updated["name"].as_str(),
        Some("loopback-renamed"),
        "update must apply"
    );
    assert!(
        updated["max_concurrent_flows"].is_null(),
        "max_concurrent_flows null clears the ceiling"
    );
    // Subjects/targets/allowlist were OMITTED in the update body —
    // they must be unchanged (locks the partial-update semantic).
    let allowlist_after = updated["allowlist"]
        .as_array()
        .expect("allowlist still present");
    assert_eq!(
        allowlist_after.len(),
        1,
        "omitted fields must NOT be cleared by partial update"
    );

    // --- DELETE ---
    let resp = app
        .auth_delete(
            &format!(
                "/api/tenant/{}/tunnel-policy/{}",
                seeded.tenant_id, policy_id
            ),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "delete returns 2xx; got {}",
        resp.status()
    );

    // --- LIST after delete → policy gone from the active listing ---
    // `soft_delete` sets `deleted_at`; the listing the admin UI reads
    // (`list_active_for_tenant` / `list_for_tenant`) filters on
    // `deleted_at: null`, so the row disappears from it. NOTE: GET-one
    // (`find_in_tenant`) does NOT filter `deleted_at`, so it still returns
    // the tombstone (200) — an intentional asymmetry today; whether GET-one
    // should 404 for a soft-deleted policy is a separate API-semantics call
    // (tracked outside P3b-2). The listing is the contract the UI relies on.
    let list_after: Value = app
        .auth_get(
            &format!("/api/tenant/{}/tunnel-policy", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let remaining = list_after["items"].as_array().expect("items after delete");
    assert!(
        remaining
            .iter()
            .all(|p| p["id"].as_str() != Some(policy_id)),
        "soft-deleted policy must not appear in the active listing"
    );
}

#[tokio::test]
async fn tunnel_client_enrollment_idempotent_on_same_machine_id() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tunnel-enroll-idemp").await;

    // Mint an enrollment token (admin).
    let et: Value = app
        .auth_post(
            &format!(
                "/api/tenant/{}/tunnel-client/enroll-token",
                seeded.tenant_id
            ),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let enroll_token = et["enrollment_token"]
        .as_str()
        .expect("enrollment_token in response")
        .to_string();

    // Enrol once.
    let machine_id = "tunnel-mach-idemp-1";
    let first: Value = app
        .client
        .post(format!("{}/api/tunnel-client/enroll", app.base_url))
        .json(&json!({
            "enrollment_token": enroll_token,
            "machine_id": machine_id,
            "machine_name": "Operator Laptop A",
            "os": "linux",
            "client_version": "0.3.0-test"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first_id = first["tunnel_client_id"]
        .as_str()
        .expect("tunnel_client_id in first enroll");
    assert!(!first["tunnel_client_token"].as_str().unwrap().is_empty());

    // Mint a second enrollment token. The first one is single-use
    // (JTI tracked server-side) and won't work again.
    let et2: Value = app
        .auth_post(
            &format!(
                "/api/tenant/{}/tunnel-client/enroll-token",
                seeded.tenant_id
            ),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Enrol again with the SAME machine_id but a different label —
    // server should rehydrate the existing row, not mint a new one.
    let second: Value = app
        .client
        .post(format!("{}/api/tunnel-client/enroll", app.base_url))
        .json(&json!({
            "enrollment_token": et2["enrollment_token"].as_str().unwrap(),
            "machine_id": machine_id,
            "machine_name": "Operator Laptop A (renamed)",
            "os": "linux",
            "client_version": "0.3.0-test"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let second_id = second["tunnel_client_id"]
        .as_str()
        .expect("tunnel_client_id in second enroll");

    assert_eq!(
        first_id, second_id,
        "re-enrolment with same machine_id must reuse the row (rehydrate); \
         minting a fresh row would surface as ghost rows in the admin UI"
    );

    // List should show exactly ONE client, not two.
    let list: Value = app
        .auth_get(
            &format!("/api/tenant/{}/tunnel-client", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = list["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "rehydrate must not duplicate the row");
    // The rehydrate also updated the friendly name.
    assert_eq!(
        items[0]["name"].as_str(),
        Some("Operator Laptop A (renamed)"),
        "rehydrate must apply the new name"
    );
}

#[tokio::test]
async fn cross_tenant_enrollment_token_request_rejected() {
    // Two separate tenants; admin from tenant A tries to mint an
    // enrollment token for tenant B's id. Server's `is_member` gate
    // must reject with 403. Locks the cross-tenant boundary at the
    // REST layer — the WS-layer cross-tenant gate (in handle_tunnel_
    // open) is a second line of defence; this one is the first.
    let app = TestApp::spawn().await;
    let tenant_a = app.seed_tenant("tunnel-xtenant-a").await;
    let tenant_b = app.seed_tenant("tunnel-xtenant-b").await;

    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/tunnel-client/enroll-token",
                tenant_b.tenant_id
            ),
            &tenant_a.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "admin from tenant A must not mint enrollment tokens for tenant B; \
         got {}",
        resp.status()
    );
}

// ────────────────────────────────────────────────────────────────────────────
// P3b-2: an agent drives the tunnel-CLIENT role over its own agent WS
// ────────────────────────────────────────────────────────────────────────────

type AgentWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Enroll an agent via the REST flow; returns `(agent_id_hex, agent_token)`.
/// Mirrors `remote_control_tests::enroll_helper` (duplicated so the tunnel
/// suite stands alone).
async fn enroll_agent(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    machine_id: &str,
    machine_name: &str,
) -> (String, String) {
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
    let ej: Value = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": machine_id,
            "machine_name": machine_name,
            "os": "linux",
            "agent_version": "0.3.0",
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

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// Connect a RAW agent WS (`?role=agent`) and send `rc:agent.hello` so the
/// Hub registers the agent. Returns the live stream.
async fn connect_agent_ws(app: &TestApp, agent_token: &str, machine_name: &str) -> AgentWs {
    let ws_url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        urlencode(agent_token)
    );
    let (mut ws, _) = connect_async(&ws_url).await.expect("agent ws connect");
    ws.send(Message::Text(
        json!({
            "t": "rc:agent.hello",
            "machine_name": machine_name,
            "os": "linux",
            "agent_version": "0.3.0",
            "displays": [],
            "caps": {
                "hw_encoders": [],
                "codecs": ["h264"],
                "has_input_permission": true,
                "supports_clipboard": false,
                "supports_file_transfer": false,
                "max_simultaneous_sessions": 4,
            }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    ws
}

/// Poll the agent row until `status == "online"` (hello processed → Hub
/// registered), so a subsequent `send_to_agent` relay lands.
async fn wait_agent_online(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    agent_id: &str,
) {
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
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
        if row["status"].as_str() == Some("online") {
            return;
        }
    }
    panic!("agent {agent_id} never came online");
}

/// Read WS frames until one arrives whose `t` == `want`, or a 5 s deadline
/// elapses. Non-matching frames are skipped.
async fn read_until(ws: &mut AgentWs, want: &str) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&text)
                    && v.get("t").and_then(|x| x.as_str()) == Some(want)
                {
                    return Some(v);
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Ok(None) => return None,
            Err(_) => continue,
        }
    }
    None
}

#[tokio::test]
async fn agent_originates_tunnel_authorized_and_relayed_to_target() {
    // P3b-2 identity model (b): agent A opens a tunnel to agent B (same
    // tenant) over A's OWN agent WS (Principal::Agent). An `all_users`
    // policy authorizes the forward — PR-A proved AllUsers matches an agent
    // principal — and the server relays the TCP forward to target B.
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("p3b2-agentorig").await;

    // Policy: any user (⇒ any principal, incl. an agent) may reach
    // 127.0.0.1:9000-9999 on any agent in the tenant.
    let policy = json!({
        "name": "allow-loopback",
        "subjects": [{ "kind": "all_users" }],
        "targets": [{ "kind": "all_agents" }],
        "allowlist": [{
            "host_pattern": { "kind": "cidr", "value": "127.0.0.0/8" },
            "port_range": { "low": 9000, "high": 9999 }
        }],
        "max_concurrent_flows": 32,
        "max_bytes_per_session": 1048576
    });
    app.auth_post(
        &format!("/api/tenant/{}/tunnel-policy", seeded.tenant_id),
        &seeded.admin.access_token,
    )
    .json(&policy)
    .send()
    .await
    .unwrap();

    let (a_id, a_tok) = enroll_agent(&app, &seeded, "mach-p3b2-A", "origin-A").await;
    let (b_id, b_tok) = enroll_agent(&app, &seeded, "mach-p3b2-B", "target-B").await;
    let mut a_ws = connect_agent_ws(&app, &a_tok, "origin-A").await;
    let mut b_ws = connect_agent_ws(&app, &b_tok, "target-B").await;
    wait_agent_online(&app, &seeded, &a_id).await;
    wait_agent_online(&app, &seeded, &b_id).await;

    // A drives the tunnel-client role: hello, then open a peer to B.
    a_ws.send(Message::Text(
        json!({
            "t": "rc:tunnel.hello",
            "role": "client",
            "version": "0.3.0",
            "supported_transports": ["webrtc-dc-v1"],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    a_ws.send(Message::Text(
        json!({
            "t": "rc:tunnel.open",
            "agent_id": b_id,
            "transport": "webrtc-dc-v1",
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let opened = read_until(&mut a_ws, "rc:tunnel.opened")
        .await
        .expect("agent A must receive rc:tunnel.opened for its own origination");
    let session_id = opened["session_id"]
        .as_str()
        .expect("session_id in rc:tunnel.opened")
        .to_string();

    // A requests a TCP forward to 127.0.0.1:9000 — the all_users policy
    // authorizes it for the agent principal.
    a_ws.send(Message::Text(
        json!({
            "t": "rc:tunnel.tcp.request",
            "session_id": session_id,
            "flow_id": 1,
            "dst_host": "127.0.0.1",
            "dst_port": 9000,
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    // The server must relay the forward to TARGET agent B — proving the
    // agent-principal authz passed AND the relay reached the target.
    let fwd = read_until(&mut b_ws, "rc:tunnel.tcp.forward")
        .await
        .expect("target agent B must receive the relayed rc:tunnel.tcp.forward");
    assert_eq!(fwd["session_id"].as_str(), Some(session_id.as_str()));
    assert_eq!(fwd["dst_host"].as_str(), Some("127.0.0.1"));
    assert_eq!(fwd["dst_port"].as_u64(), Some(9000));
}

#[tokio::test]
async fn agent_tunnel_open_cross_tenant_rejected() {
    // The cross-tenant wall holds for the agent-origination path: an agent
    // in tenant 1 must not open a tunnel to an agent in tenant 2.
    let app = TestApp::spawn().await;
    let t1 = app.seed_tenant("p3b2-xtenant-1").await;
    let t2 = app.seed_tenant("p3b2-xtenant-2").await;

    let (_a_id, a_tok) = enroll_agent(&app, &t1, "mach-p3b2-xt-A", "A").await;
    let (c_id, _c_tok) = enroll_agent(&app, &t2, "mach-p3b2-xt-C", "C").await;

    let mut a_ws = connect_agent_ws(&app, &a_tok, "A").await;

    a_ws.send(Message::Text(
        json!({
            "t": "rc:tunnel.open",
            "agent_id": c_id,
            "transport": "webrtc-dc-v1",
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let err = read_until(&mut a_ws, "rc:error")
        .await
        .expect("agent A must receive rc:error for a cross-tenant open");
    assert_eq!(
        err["code"].as_str(),
        Some("cross_tenant"),
        "cross-tenant open must be refused with code=cross_tenant"
    );
}

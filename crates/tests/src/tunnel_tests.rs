//! Phase 5 — tunnel subsystem integration tests.
//!
//! Covers the REST + ACL surface end-to-end against a live `TestApp`.
//! Higher-leverage than unit tests in `crates/tunnel-core/` because
//! it exercises the full request → DAO → response cycle (incl. JSON
//! adjacent-tagged serde for `PolicySubject` / `PolicyTarget` /
//! `DestinationRule`), the cross-tenant gate, and the tunnel-client
//! enrollment idempotence the rehydrate path is supposed to guarantee.
//!
//! Three tests:
//!
//! * `policy_crud_round_trips` — POST → GET list → GET one → PUT →
//!   GET one (verifies update applied) → DELETE → GET (404). Locks
//!   the policy CRUD wire shape that the admin UI consumes; a
//!   serde drift on any of the adjacently-tagged enums (e.g. someone
//!   renames `kind` to `t`) would break the admin UI silently — this
//!   catches it server-side first.
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
//! WebRTC DC round-trip (agent + tunnel-client + echo server +
//! TCP byte exchange) is NOT covered here — the existing in-process
//! ICE flakiness in `agent_tests::agent_answers_sdp_offer_with_real_
//! webrtc_peer` (see its "best-effort" comment) makes it too
//! unstable for CI without a TURN fixture. Cluster-side validation
//! happens via the Phase 5 cluster harness (Chunk 5B, follow-on).

use crate::fixtures::test_app::TestApp;
use serde_json::{Value, json};

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
    //   - PolicySubject: internally-tagged `{kind, <variant-field>}`,
    //     variants snake_cased — `user_id` / `role_id` /
    //     `tunnel_client_id` / `all_users`. So `kind` and the data
    //     field share the same name for the *_id variants; that's
    //     by-design (the variant's data field carries the value).
    //   - PolicyTarget: internally-tagged the same way —
    //     `agent_id` / `all_agents`.
    let create_body = json!({
        "name": "loopback-test",
        "subjects": [
            { "kind": "user_id", "user_id": seeded.admin.id }
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
    let policy_id = created["id"].as_str().expect("policy id in create response");
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
            &format!("/api/tenant/{}/tunnel-policy/{}", seeded.tenant_id, policy_id),
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
            &format!("/api/tenant/{}/tunnel-policy/{}", seeded.tenant_id, policy_id),
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
    let allowlist_after =
        updated["allowlist"].as_array().expect("allowlist still present");
    assert_eq!(
        allowlist_after.len(),
        1,
        "omitted fields must NOT be cleared by partial update"
    );

    // --- DELETE ---
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/tunnel-policy/{}", seeded.tenant_id, policy_id),
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

    // --- GET after delete → 404 ---
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/tunnel-policy/{}", seeded.tenant_id, policy_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "GET after delete must 404; got {}",
        resp.status()
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

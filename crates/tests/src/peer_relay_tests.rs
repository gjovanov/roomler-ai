// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-19 P3b — the peer-relay admin surface, end to end: the org switch
//! (gate 1), per-device approval (gate 3) with its composite permission and
//! its audit rows, the audit reader's own permission, and the fail-closed ACL
//! loader the mint (P3c) stands on.
//!
//! What these lock is the WIRING. `decide_approval` is unit-tested in the api
//! crate; a handler that stops calling it, or audits only one arm, passes
//! every unit test and fails here.

use crate::fixtures::{seed::SeededTenant, test_app::TestApp};
use bson::{doc, oid::ObjectId};
use roomler_ai_db::models::role::permissions::{DEFAULT_ADMIN, EXEC_DEVICE};
use roomler_ai_mod_network::overlay::{PolicyLoad, load_acl, try_load_acl};
use roomler_ai_remote_control::models::OverlayAclMode;
use roomlerd::{config::AgentConfig, enrollment};
use serde_json::{Value, json};

fn url(seeded: &SeededTenant, tail: &str) -> String {
    format!("/api/tenant/{}{}", seeded.tenant_id, tail)
}

async fn get(app: &TestApp, path: &str, token: &str) -> (u16, Value) {
    let resp = app.auth_get(path, token).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn put(app: &TestApp, path: &str, token: &str, body: Value) -> (u16, Value) {
    let resp = app.auth_put(path, token).json(&body).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Enrol a device so there is a row to approve. No WS on purpose: approval is
/// a row edit, and an OFFLINE device can be approved — it simply is not
/// serving until it comes up with `relay_server_enabled`.
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
        machine_name: "Relay test host",
    })
    .await
    .expect("agent enrollment")
}

async fn seeded_role_id(app: &TestApp, seeded: &SeededTenant, name: &str) -> String {
    let (_, roles) = get(app, &url(seeded, "/role"), &seeded.admin.access_token).await;
    roles
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("seeded role {name} not found"))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The owner mints a role — allowed for any mask, because the owner holds
/// ADMINISTRATOR.
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

/// Gate 1. `off` on every tenant that existed before the feature; flipping it
/// is `MANAGE_TENANT` (an org-owner decision, like `exec-settings`), while
/// reading the settings is `MANAGE_AGENTS` like every other fleet view.
#[tokio::test]
async fn org_mode_defaults_off_and_flipping_it_is_an_owner_decision() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("relaymode").await;
    let owner = &seeded.admin.access_token;
    let settings = url(&seeded, "/peer-relay");

    let (s, body) = get(&app, &settings, owner).await;
    assert_eq!(s, 200);
    assert_eq!(body["mode"], json!("off"), "closed by default");
    assert_eq!(body["relays"], json!([]));

    // A plain member can neither read nor flip it.
    let (s, _) = get(&app, &settings, &seeded.member.access_token).await;
    assert_eq!(s, 403);
    let (s, _) = put(
        &app,
        &settings,
        &seeded.member.access_token,
        json!({ "mode": "on" }),
    )
    .await;
    assert_eq!(s, 403);
    let (_, body) = get(&app, &settings, owner).await;
    assert_eq!(body["mode"], json!("off"), "a refused flip changes nothing");

    // The owner flips it; the read reflects it.
    let (s, body) = put(&app, &settings, owner, json!({ "mode": "on" })).await;
    assert_eq!(s, 200);
    assert_eq!(body["mode"], json!("on"));
    let (_, body) = get(&app, &settings, owner).await;
    assert_eq!(body["mode"], json!("on"));

    // An admin (DEFAULT_ADMIN: MANAGE_AGENTS, not MANAGE_TENANT) can read the
    // settings but cannot flip the org switch.
    let admin_role = seeded_role_id(&app, &seeded, "admin").await;
    assign_role(&app, &seeded, &admin_role, &seeded.member.id).await;
    let (s, body) = get(&app, &settings, &seeded.member.access_token).await;
    assert_eq!(s, 200);
    assert_eq!(body["mode"], json!("on"));
    let (s, _) = put(
        &app,
        &settings,
        &seeded.member.access_token,
        json!({ "mode": "warn" }),
    )
    .await;
    assert_eq!(s, 403, "MANAGE_AGENTS alone must not flip the org switch");
}

/// Gate 3 and its audit. `DEFAULT_ADMIN` carries `MANAGE_AGENTS` but
/// deliberately not `EXEC_DEVICE`; approving a relay needs both, clearing one
/// needs only the first, and EVERY attempt — granted or refused — is a row.
#[tokio::test]
async fn approval_needs_manage_agents_and_exec_device_and_audits_both_arms() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("relayappr").await;
    let owner = &seeded.admin.access_token;
    let member = &seeded.member.access_token;
    let cfg = enrol(&app, &seeded, "mach-relay-1").await;
    let policy = url(
        &seeded,
        &format!("/agent/{}/peer-relay-policy", cfg.agent_id),
    );
    let settings = url(&seeded, "/peer-relay");
    let audit = url(&seeded, "/peer-relay-audit");

    // 0. The audit reader is behind VIEW_EXEC_AUDIT, which a member lacks.
    let (s, _) = get(&app, &audit, member).await;
    assert_eq!(s, 403);

    // 1. A plain member is not a device admin.
    let (s, body) = put(&app, &policy, member, json!({ "serve": true })).await;
    assert_eq!(s, 403, "{body}");

    // 2. Promoted to `admin`: MANAGE_AGENTS, still no EXEC_DEVICE.
    let admin_role = seeded_role_id(&app, &seeded, "admin").await;
    assign_role(&app, &seeded, &admin_role, &seeded.member.id).await;
    let (s, body) = put(&app, &policy, member, json!({ "serve": true })).await;
    assert_eq!(s, 403);
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("EXEC_DEVICE"),
        "the refusal must name the missing permission, got {body}"
    );
    let (_, body) = get(&app, &settings, owner).await;
    assert_eq!(
        body["relays"],
        json!([]),
        "a refused approval changes nothing"
    );

    // 3. The owner approves (ADMINISTRATOR bypass). The device is listed —
    //    and listed as NOT serving, because this row never advertised
    //    `relay-server`: gate 3 is not gate 4.
    let (s, body) = put(&app, &policy, owner, json!({ "serve": true })).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["serve"], json!(true));
    let (_, body) = get(&app, &settings, owner).await;
    assert_eq!(body["relays"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["relays"][0]["id"], json!(cfg.agent_id));
    assert_eq!(body["relays"][0]["serving"], json!(false));

    // 4. Clearing is not a grant: the EXEC_DEVICE-less admin may revoke.
    let (s, _) = put(&app, &policy, member, json!({ "serve": false })).await;
    assert_eq!(s, 200);
    let (_, body) = get(&app, &settings, owner).await;
    assert_eq!(body["relays"], json!([]));

    // 5. ...and still may not re-approve.
    let (s, _) = put(&app, &policy, member, json!({ "serve": true })).await;
    assert_eq!(s, 403);

    // 6. With EXEC_DEVICE added to the same person (roles union), it works.
    let relay_admin = create_role(&app, &seeded, "relay-admin", DEFAULT_ADMIN | EXEC_DEVICE).await;
    assign_role(&app, &seeded, &relay_admin, &seeded.member.id).await;
    let (s, body) = put(&app, &policy, member, json!({ "serve": true })).await;
    assert_eq!(s, 200, "{body}");
    let (_, body) = get(&app, &settings, owner).await;
    assert_eq!(body["relays"][0]["id"], json!(cfg.agent_id));

    // 7. Six attempts, six rows — the refused ones included — all naming the
    //    device and who asked, newest first.
    let (s, body) = get(&app, &audit, owner).await;
    assert_eq!(s, 200);
    assert_eq!(body["total"], json!(6));
    let items = body["items"].as_array().unwrap();
    assert!(items.iter().all(|r| r["action"] == json!("approve")));
    assert!(items.iter().all(|r| r["agent_id"] == json!(cfg.agent_id)));
    let denied: Vec<&str> = items.iter().filter_map(|r| r["denied"].as_str()).collect();
    assert_eq!(
        denied.iter().filter(|d| **d == "not_device_admin").count(),
        1
    );
    assert_eq!(
        denied
            .iter()
            .filter(|d| **d == "cannot_grant_relay")
            .count(),
        2
    );
    assert_eq!(items.iter().filter(|r| r["denied"].is_null()).count(), 3);
    assert_eq!(items[0]["serve"], json!(true));
    assert_eq!(items[0]["user_id"], json!(seeded.member.id));
    assert!(
        items[0].get("vni").is_none(),
        "an approve row carries no mint fields"
    );

    // The per-device filter and the (now admin) member's own read.
    let (s, body) = get(&app, &format!("{audit}?agent_id={}", cfg.agent_id), member).await;
    assert_eq!(s, 200, "DEFAULT_ADMIN carries VIEW_EXEC_AUDIT");
    assert_eq!(body["total"], json!(6));
}

/// The loader the P3c mint will stand on. `load_acl` fails OPEN by design (a
/// spurious deny there would withhold a tenant's whole mesh on a Mongo blip);
/// a relay grant must instead be REFUSED and audited when the rules cannot be
/// read, which needs an error the caller can see. Both postures, side by side,
/// on the same unreadable row.
#[tokio::test]
async fn try_load_acl_fails_closed_where_load_acl_fails_open() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("relayacl").await;
    let tid = ObjectId::parse_str(&seeded.tenant_id).unwrap();

    // Readable: both agree, and `Always` reads the rows even under `off`.
    let ctx = try_load_acl(app.state.network(), tid, PolicyLoad::Always)
        .await
        .expect("a readable tenant loads");
    assert_eq!(ctx.mode, OverlayAclMode::Off);
    assert!(ctx.policies.is_empty());

    // Put the tenant in `warn` so the netmap loader really reads the rows...
    let (s, _) = put(
        &app,
        &url(&seeded, "/overlay-acl/mode"),
        &seeded.admin.access_token,
        json!({ "mode": "warn" }),
    )
    .await;
    assert_eq!(s, 200);

    // ...then make them unreadable: a document that matches the live filter
    // but cannot deserialise into an `OverlayPolicy`.
    app.db
        .collection::<bson::Document>("overlay_policies")
        .insert_one(doc! {
            "tenant_id": tid,
            "name": "broken",
            "enabled": true,
            "sources": "not-an-array",
            "via": [],
            "destinations": [],
            "created_at": bson::DateTime::now(),
            "updated_at": bson::DateTime::now(),
            "deleted_at": null,
        })
        .await
        .unwrap();

    // The strict loader refuses to answer.
    assert!(
        try_load_acl(app.state.network(), tid, PolicyLoad::Always)
            .await
            .is_err(),
        "an unreadable policy set must be an error, not an empty ACL"
    );

    // The netmap loader keeps its posture: OPEN, exactly as before.
    let ctx = load_acl(app.state.network(), tid).await;
    assert_eq!(ctx.mode, OverlayAclMode::Off);
    assert!(ctx.policies.is_empty());
}

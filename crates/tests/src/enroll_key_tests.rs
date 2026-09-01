// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-51 P2 — ephemeral enrollment keys: the org gate, the four §4 controls
//! (ceiling / expiry / revocation / per-use audit), create-only enrollment,
//! and AC9 (the request body can never pick which path runs).
//!
//! Everything drives the REAL routes; the only raw-collection write is the
//! expiry rewind (there is deliberately no API for aging a key).

use bson::oid::ObjectId;
use serde_json::{Value, json};

use crate::fixtures::test_app::TestApp;

async fn enable_keys(app: &TestApp, tenant_id: &str, admin_token: &str) {
    let resp = app
        .auth_put(
            &format!("/api/tenant/{tenant_id}/ephemeral-key-settings"),
            admin_token,
        )
        .json(&json!({ "ephemeral_keys_enabled": true }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "org gate flip failed");
}

async fn mint_key(app: &TestApp, tenant_id: &str, admin_token: &str, body: Value) -> Value {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{tenant_id}/agent/enroll-key"),
            admin_token,
        )
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "mint failed: {}", resp.status());
    resp.json().await.unwrap()
}

/// Enroll against `/api/agent/enroll` with an arbitrary credential and an
/// arbitrary extra body — returns the raw response so refusals are
/// assertable too.
async fn enroll_raw(
    app: &TestApp,
    credential: &str,
    machine_id: &str,
    extra: Value,
) -> reqwest::Response {
    let mut body = json!({
        "enrollment_token": credential,
        "machine_id": machine_id,
        "machine_name": format!("host-{machine_id}"),
        "os": "linux",
        "agent_version": "0.4.42",
    });
    if let (Some(obj), Some(extra_obj)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in extra_obj {
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

async fn agent_row(app: &TestApp, agent_id: &str) -> Option<bson::Document> {
    app.db
        .collection::<bson::Document>("agents")
        .find_one(bson::doc! { "_id": ObjectId::parse_str(agent_id).unwrap() })
        .await
        .unwrap()
}

#[tokio::test]
async fn gate_off_refuses_mint_and_gate_flip_revokes_the_class() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("ekey1").await;

    // Gate off (the default): the mint refuses.
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-key", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "mint must refuse while the class is off"
    );

    // On: mint works, and the SECRET appears exactly once (never in the list).
    enable_keys(&app, &t.tenant_id, &t.admin.access_token).await;
    let minted = mint_key(
        &app,
        &t.tenant_id,
        &t.admin.access_token,
        json!({"label": "ci"}),
    )
    .await;
    let key = minted["key"].as_str().expect("key JWT shown at mint");
    let list: Value = app
        .auth_get(
            &format!("/api/tenant/{}/agent/enroll-key", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = list["items"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["label"], json!("ci"));
    assert!(
        !serde_json::to_string(&list).unwrap().contains(key),
        "the key secret must never be listable back"
    );

    // Gate flipped OFF again: the already-minted key stops working — an
    // org-wide revocation that burns nothing (uses stays 0).
    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/ephemeral-key-settings", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "ephemeral_keys_enabled": false }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let resp = enroll_raw(&app, key, "ekey1-m1", json!({})).await;
    assert_eq!(resp.status(), 403);
    let list: Value = app
        .auth_get(
            &format!("/api/tenant/{}/agent/enroll-key", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        list["items"][0]["uses"],
        json!(0),
        "the gate refusal must precede the use-claim"
    );
}

#[tokio::test]
async fn one_key_mints_n_distinct_ephemeral_devices_with_audit_trail() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("ekey2").await;
    enable_keys(&app, &t.tenant_id, &t.admin.access_token).await;
    let minted = mint_key(
        &app,
        &t.tenant_id,
        &t.admin.access_token,
        json!({"ephemeral_ttl_secs": 120}),
    )
    .await;
    let key = minted["key"].as_str().unwrap();
    let key_id = minted["id"].as_str().unwrap();

    // The server half of AC7: one credential, three machine ids, THREE rows.
    let mut agent_ids = Vec::new();
    for m in ["ekey2-a", "ekey2-b", "ekey2-c"] {
        let resp = enroll_raw(&app, key, m, json!({})).await;
        assert!(resp.status().is_success());
        let v: Value = resp.json().await.unwrap();
        assert_eq!(
            v["ephemeral"],
            json!(true),
            "the response says what it minted"
        );
        agent_ids.push(v["agent_id"].as_str().unwrap().to_string());
    }
    for aid in &agent_ids {
        let row = agent_row(&app, aid).await.expect("row exists");
        assert!(row.get_bool("ephemeral").unwrap_or(false));
        assert_eq!(row.get_i64("ephemeral_ttl_secs").ok(), Some(120));
        assert!(
            row.get_object_id("enroll_key_id").is_ok(),
            "device → key chain"
        );
    }

    // Control 4 — the per-use trail carries every mint, by the API.
    let uses: Value = app
        .auth_get(
            &format!("/api/tenant/{}/agent/enroll-key/{key_id}/uses", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(uses["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn ceiling_expiry_and_revocation_each_refuse() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("ekey3").await;
    enable_keys(&app, &t.tenant_id, &t.admin.access_token).await;

    // Ceiling: max_uses=2 ⇒ the third enrollment refuses.
    let minted = mint_key(
        &app,
        &t.tenant_id,
        &t.admin.access_token,
        json!({"max_uses": 2}),
    )
    .await;
    let key = minted["key"].as_str().unwrap();
    assert!(
        enroll_raw(&app, key, "ekey3-a", json!({}))
            .await
            .status()
            .is_success()
    );
    assert!(
        enroll_raw(&app, key, "ekey3-b", json!({}))
            .await
            .status()
            .is_success()
    );
    let resp = enroll_raw(&app, key, "ekey3-c", json!({})).await;
    assert_eq!(resp.status(), 401);
    assert!(resp.text().await.unwrap().contains("exhausted"));

    // Expiry: rewind the ROW (the row is the authority; the JWT is only the
    // belt to its braces, and stays formally valid here).
    let minted = mint_key(&app, &t.tenant_id, &t.admin.access_token, json!({})).await;
    let key = minted["key"].as_str().unwrap();
    app.db
        .collection::<bson::Document>("enrollment_keys")
        .update_one(
            bson::doc! { "_id": ObjectId::parse_str(minted["id"].as_str().unwrap()).unwrap() },
            bson::doc! { "$set": { "expires_at": bson::DateTime::from_millis(
                bson::DateTime::now().timestamp_millis() - 1000
            ) } },
        )
        .await
        .unwrap();
    let resp = enroll_raw(&app, key, "ekey3-d", json!({})).await;
    assert_eq!(resp.status(), 401);
    assert!(resp.text().await.unwrap().contains("expired"));

    // Revocation (AC8): refused while the expiry is still in the future.
    let minted = mint_key(&app, &t.tenant_id, &t.admin.access_token, json!({})).await;
    let key = minted["key"].as_str().unwrap();
    let kid = minted["id"].as_str().unwrap();
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/agent/enroll-key/{kid}", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let resp = enroll_raw(&app, key, "ekey3-e", json!({})).await;
    assert_eq!(resp.status(), 401);
    assert!(resp.text().await.unwrap().contains("revoked"));
}

#[tokio::test]
async fn body_cannot_pick_the_path_and_keys_cannot_take_over_rows() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("ekey4").await;
    enable_keys(&app, &t.tenant_id, &t.admin.access_token).await;

    // AC9 half 1: a STANDARD token with `"ephemeral": true` in the body
    // mints a PERMANENT device — the body field simply does not exist.
    let et: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resp = enroll_raw(
        &app,
        et["enrollment_token"].as_str().unwrap(),
        "ekey4-perm",
        json!({"ephemeral": true}),
    )
    .await;
    assert!(resp.status().is_success());
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["ephemeral"], json!(false));
    let perm_id = v["agent_id"].as_str().unwrap().to_string();
    let row = agent_row(&app, &perm_id).await.unwrap();
    assert!(!row.get_bool("ephemeral").unwrap_or(true));

    // AC9 half 2: an ephemeral KEY with `"ephemeral": false` in the body
    // still mints an ephemeral device.
    let minted = mint_key(&app, &t.tenant_id, &t.admin.access_token, json!({})).await;
    let key = minted["key"].as_str().unwrap();
    let resp = enroll_raw(&app, key, "ekey4-eph", json!({"ephemeral": false})).await;
    assert!(resp.status().is_success());
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["ephemeral"], json!(true));

    // F1's takeover closure: the key posting the PERMANENT device's
    // machine_id gets a final 409 — and the existing row is untouched.
    let resp = enroll_raw(&app, key, "ekey4-perm", json!({})).await;
    assert_eq!(
        resp.status(),
        409,
        "an ephemeral enrollment never revives or replaces"
    );
    let row = agent_row(&app, &perm_id).await.unwrap();
    assert!(!row.get_bool("ephemeral").unwrap_or(true), "row untouched");
}

#[tokio::test]
async fn mint_requires_manage_agents() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("ekey5").await;
    enable_keys(&app, &t.tenant_id, &t.admin.access_token).await;
    // The seeded plain member holds no MANAGE_AGENTS.
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-key", t.tenant_id),
            &t.member.access_token,
        )
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

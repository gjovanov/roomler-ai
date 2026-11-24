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

/// #4254 — the class switch (`MANAGE_TENANT`) and the key mint
/// (`MANAGE_AGENTS`) decide their bit before they read their body at all: a
/// member without the bit gets the one 403 `forbidden` naming it for a body
/// that does not even parse, the answer a body the route reads gets, and
/// someone outside the org the one `not_a_member`. While they took `Json<T>`
/// as an extractor, axum answered such a body before the handler ran, with a
/// 400, 415 or 422 decided by the body alone.
#[tokio::test]
async fn the_key_writes_decide_their_bit_before_a_body_that_does_not_parse() {
    use crate::tunnel_policy_permission_tests::{
        Gate, GatedWrite, assert_no_problems, gate_problems,
    };

    let app = TestApp::spawn().await;
    let t = app.seed_tenant("ekey-unparsed").await;
    let other = app.seed_tenant("ekey-unparsed-b").await;
    let org = format!("/api/tenant/{}", t.tenant_id);
    let writes = [
        (
            "MANAGE_TENANT",
            GatedWrite {
                method: "PUT",
                path: format!("{org}/ephemeral-key-settings"),
                reads: json!({ "ephemeral_keys_enabled": false }),
                refused_field: r#"{"ephemeral_keys_enabled": "x"}"#,
            },
        ),
        (
            "MANAGE_AGENTS",
            GatedWrite {
                method: "POST",
                path: format!("{org}/agent/enroll-key"),
                reads: json!({}),
                refused_field: r#"{"max_uses": "x"}"#,
            },
        ),
    ];

    let mut wrong = Vec::new();
    for (bit, write) in &writes {
        wrong.extend(
            gate_problems(
                &app,
                "a member without the bit",
                &t.member.access_token,
                Gate::Bit(bit),
                write,
            )
            .await,
        );
        wrong.extend(
            gate_problems(
                &app,
                "another org's owner",
                &other.admin.access_token,
                Gate::NotAMember,
                write,
            )
            .await,
        );
    }
    assert_no_problems("answered before the bit", &wrong);
}

/// #4254's control. The org's owner is answered a key write that does not
/// parse exactly as axum's `Json` extractor answers it, the status and the
/// plain-text words. The class switch is off, and the mint's body is read
/// before the switch is asked about, as the extractor read it before the
/// handler ran.
#[tokio::test]
async fn a_holder_is_answered_a_key_write_that_does_not_parse_as_the_json_extractor_answers_it() {
    use crate::tunnel_policy_permission_tests::{assert_no_problems, holder_problems};
    use roomler_ai_mod_fleet::enroll_key::{MintKeyRequest, OrgEphemeralKeySettings};

    let app = TestApp::spawn().await;
    let t = app.seed_tenant("ekey-unparsed-holder").await;
    let owner = &t.admin.access_token;
    let org = format!("/api/tenant/{}", t.tenant_id);

    let mut wrong = holder_problems::<OrgEphemeralKeySettings>(
        &app,
        owner,
        "PUT",
        &format!("{org}/ephemeral-key-settings"),
        r#"{"ephemeral_keys_enabled": "x"}"#,
    )
    .await;
    wrong.extend(
        holder_problems::<MintKeyRequest>(
            &app,
            owner,
            "POST",
            &format!("{org}/agent/enroll-key"),
            r#"{"max_uses": "x"}"#,
        )
        .await,
    );
    assert_no_problems("the org's owner", &wrong);
}

/// FR-51 P3 — the self-unenroll route: an ephemeral device removes itself
/// with its own agent JWT; a permanent device is refused; a second call
/// after success is a 401 (the row is gone, which `AuthAgent` enforces) —
/// the exact status the agent treats as "already gone".
#[tokio::test]
async fn self_unenroll_removes_ephemeral_and_refuses_permanent() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("ekey6").await;
    enable_keys(&app, &t.tenant_id, &t.admin.access_token).await;

    // Ephemeral device, via the real key path.
    let minted = mint_key(&app, &t.tenant_id, &t.admin.access_token, json!({})).await;
    let resp = enroll_raw(
        &app,
        minted["key"].as_str().unwrap(),
        "ekey6-eph",
        json!({}),
    )
    .await;
    assert!(resp.status().is_success());
    let v: Value = resp.json().await.unwrap();
    let eph_id = v["agent_id"].as_str().unwrap().to_string();
    let eph_token = v["agent_token"].as_str().unwrap().to_string();

    // Permanent device, via a standard token.
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
        "ekey6-perm",
        json!({}),
    )
    .await;
    let v: Value = resp.json().await.unwrap();
    let perm_id = v["agent_id"].as_str().unwrap().to_string();
    let perm_token = v["agent_token"].as_str().unwrap().to_string();

    // No credential at all: refused before anything else.
    let resp = app
        .client
        .post(app.url("/api/agent/self/unenroll"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // A PERMANENT device may not remove itself — a compromised host must not
    // be able to erase its own fleet record.
    let resp = app
        .client
        .post(app.url("/api/agent/self/unenroll"))
        .bearer_auth(&perm_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert!(
        agent_row(&app, &perm_id).await.is_some(),
        "permanent row untouched"
    );

    // The ephemeral device removes itself — outright, not a tombstone.
    let resp = app
        .client
        .post(app.url("/api/agent/self/unenroll"))
        .bearer_auth(&eph_token)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert!(
        agent_row(&app, &eph_id).await.is_none(),
        "ephemeral row hard-deleted by self-unenroll"
    );

    // P4 — the birth row now carries the whole lifecycle: the use-trail
    // (which survives the hard delete) says when and by which path the
    // device left.
    let uses: Value = app
        .auth_get(
            &format!(
                "/api/tenant/{}/agent/enroll-key/{}/uses",
                t.tenant_id,
                minted["id"].as_str().unwrap()
            ),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = uses["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|u| u["agent_id"] == json!(eph_id))
        .expect("the reaped device's birth row survives");
    assert_eq!(row["removal"], json!("ephemeral_self_unenroll"));
    assert!(row["removed_at"].as_str().is_some_and(|s| !s.is_empty()));

    // Second call: the row is gone, so the extractor refuses — 401, the
    // status the agent deliberately reads as success ("already gone").
    let resp = app
        .client
        .post(app.url("/api/agent/self/unenroll"))
        .bearer_auth(&eph_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

/// A standard (single-use) enrollment token, minted by the org's admin.
async fn standard_token(app: &TestApp, tenant_id: &str, admin_token: &str) -> String {
    let et: Value = app
        .auth_post(
            &format!("/api/tenant/{tenant_id}/agent/enroll-token"),
            admin_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    et["enrollment_token"].as_str().unwrap().to_string()
}

/// #4331 — the enroll answer's `ephemeral` is what the ROW is, because a
/// daemon records it into its config.
///
/// T11 (#4352) — a standard token over a LIVE ephemeral row with the same
/// machine id is a 409, decided before the token is spent: the row stays as it
/// was (ephemeral, the same id), and the person is told to remove the device
/// first. Once the row is removed, the SAME token enrolls the machine id as a
/// NEW permanent device, and the answer says that. Before #4352 the token
/// refreshed the live row, which stayed ephemeral, and the device then removed
/// it at its next OS stop.
#[tokio::test]
async fn the_enroll_answer_is_the_rows_nature_not_the_tokens() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("ekey7").await;
    let admin = t.admin.access_token.as_str();
    enable_keys(&app, &t.tenant_id, admin).await;

    let minted = mint_key(&app, &t.tenant_id, admin, json!({})).await;
    let resp = enroll_raw(
        &app,
        minted["key"].as_str().unwrap(),
        "ekey7-mid",
        json!({}),
    )
    .await;
    assert!(resp.status().is_success());
    let eph: Value = resp.json().await.unwrap();
    assert_eq!(eph["ephemeral"], json!(true));
    let eph_id = eph["agent_id"].as_str().unwrap().to_string();
    let before = agent_row(&app, &eph_id).await.unwrap();

    // A standard token, the same machine id, the row still live: 409.
    let token = standard_token(&app, &t.tenant_id, admin).await;
    let resp = enroll_raw(&app, &token, "ekey7-mid", json!({})).await;
    assert_eq!(resp.status(), 409, "a live ephemeral row is not refreshed");
    let refused: Value = resp.json().await.unwrap();
    assert_eq!(refused["error"], json!("conflict"));
    let message = refused["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("ephemeral") && message.contains("Devices page"),
        "{message}"
    );
    assert_eq!(
        agent_row(&app, &eph_id).await.as_ref(),
        Some(&before),
        "the row is unchanged: still ephemeral, the same id"
    );

    // Removed (the Devices page), the SAME token makes the machine id a new
    // permanent device: the 409 did not spend it.
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/agent/{eph_id}", t.tenant_id),
            admin,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let resp = enroll_raw(&app, &token, "ekey7-mid", json!({})).await;
    assert!(
        resp.status().is_success(),
        "the token was not spent: {}",
        resp.status()
    );
    let permanent: Value = resp.json().await.unwrap();
    assert_ne!(permanent["agent_id"], eph["agent_id"], "a new row");
    assert_eq!(permanent["ephemeral"], json!(false));
    let row = agent_row(&app, permanent["agent_id"].as_str().unwrap())
        .await
        .unwrap();
    assert!(
        !row.get_bool("ephemeral").unwrap_or(true),
        "a permanent row"
    );
}

/// T12 (#4352) — what a standard token did before stays: over a LIVE
/// PERMANENT row it refreshes that row (the same id, still permanent), and
/// over a REMOVED ephemeral row it makes a new permanent device.
#[tokio::test]
async fn a_standard_token_still_refreshes_a_live_permanent_row() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("ekey9").await;
    let admin = t.admin.access_token.as_str();
    enable_keys(&app, &t.tenant_id, admin).await;

    // A removed ephemeral row: hard-deleted, so the machine id is new again.
    let minted = mint_key(&app, &t.tenant_id, admin, json!({})).await;
    let resp = enroll_raw(
        &app,
        minted["key"].as_str().unwrap(),
        "ekey9-mid",
        json!({}),
    )
    .await;
    assert!(resp.status().is_success());
    let eph: Value = resp.json().await.unwrap();
    let eph_id = eph["agent_id"].as_str().unwrap().to_string();
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/agent/{eph_id}", t.tenant_id),
            admin,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let token = standard_token(&app, &t.tenant_id, admin).await;
    let resp = enroll_raw(&app, &token, "ekey9-mid", json!({})).await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let permanent: Value = resp.json().await.unwrap();
    assert_ne!(permanent["agent_id"], eph["agent_id"], "a new row");
    assert_eq!(permanent["ephemeral"], json!(false));

    // A live permanent row: refreshed, as before.
    let again = standard_token(&app, &t.tenant_id, admin).await;
    let resp = enroll_raw(&app, &again, "ekey9-mid", json!({})).await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let refreshed: Value = resp.json().await.unwrap();
    assert_eq!(
        refreshed["agent_id"], permanent["agent_id"],
        "the live row is refreshed"
    );
    assert_eq!(refreshed["ephemeral"], json!(false));
    let row = agent_row(&app, permanent["agent_id"].as_str().unwrap())
        .await
        .unwrap();
    assert!(
        !row.get_bool("ephemeral").unwrap_or(true),
        "still permanent"
    );
}

/// T13 (#4352) — an ephemeral key adding a SECOND organization to an enrolled
/// device is refused: the row the key made is gone again, and the device's
/// config file is unchanged. Driven through `org join`, the daemon's own
/// caller, against the real routes.
#[tokio::test]
async fn an_ephemeral_key_cannot_add_a_second_org_to_an_enrolled_device() {
    let app = TestApp::spawn().await;
    let home = app.seed_tenant("ekey8a").await;
    let other = app.seed_tenant("ekey8b").await;
    enable_keys(&app, &other.tenant_id, &other.admin.access_token).await;

    // The device: enrolled in `home` with a standard token, its config on disk.
    let cfg =
        crate::agent_tests::enrol_via_agent_lib(&app, &home, "ekey8-mid", "Two-org box").await;
    assert!(!cfg.ephemeral);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    roomlerd::config::save(&path, &cfg).unwrap();
    let before = std::fs::read(&path).unwrap();

    // An ephemeral key of the OTHER organization, handed to the device.
    let minted = mint_key(&app, &other.tenant_id, &other.admin.access_token, json!({})).await;
    let joined = roomlerd::org_join::join_org(
        &cfg,
        &path,
        std::sync::Arc::new(tokio::sync::Mutex::new(())),
        minted["key"].as_str().unwrap(),
        None,
        None,
    )
    .await;
    let error = match joined {
        Ok(outcome) => panic!("an ephemeral key added an organization: {outcome:?}"),
        Err(e) => e,
    };
    let refused = error
        .downcast_ref::<roomlerd::enrollment::EphemeralOrgRefused>()
        .unwrap_or_else(|| panic!("refused for another reason: {error:#}"));
    assert_eq!(refused.tenant_id, other.tenant_id);
    assert!(
        matches!(
            refused.row,
            roomlerd::enrollment::RefusedRow::Made {
                removed: Some(Ok(())),
                ..
            }
        ),
        "{:?}",
        refused.row
    );

    // The row the key made is gone, and the file is as it was.
    assert!(
        agent_row(&app, &refused.agent_id).await.is_none(),
        "the row the key made is removed"
    );
    let left = app
        .db
        .collection::<bson::Document>("agents")
        .find_one(bson::doc! {
            "tenant_id": ObjectId::parse_str(&other.tenant_id).unwrap(),
            "machine_id": "ekey8-mid",
        })
        .await
        .unwrap();
    assert!(left.is_none(), "no row of this machine in the other org");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the config is unchanged"
    );
}

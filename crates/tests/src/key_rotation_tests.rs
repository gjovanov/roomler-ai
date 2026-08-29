// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-40 — `POST /api/tenant/{tid}/agent/{id}/overlay-key/rotate`.
//!
//! What a server-only test can prove: the gates (membership, MANAGE_AGENTS,
//! the per-device ceiling), the offline path (an order is QUEUED on the row
//! and the resolved state is honest about a device that cannot act on it),
//! and that every decision — both arms — lands in `key_rotation_audit`. The
//! live path (push → device mints → re-join under the new key) is the
//! field-verification criterion in the spec; the wire frames are locked by
//! unit tests in `remote_control`.

use bson::{doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::fixtures::test_app::TestApp;

async fn mint_enroll_token(app: &TestApp, tenant_id: &str, admin_token: &str) -> String {
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

async fn enroll(app: &TestApp, tenant_id: &str, admin_token: &str, machine_id: &str) -> String {
    let token = mint_enroll_token(app, tenant_id, admin_token).await;
    let resp: Value = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": token,
            "machine_id": machine_id,
            "machine_name": "Rotating Box",
            "os": "linux",
            "agent_version": "0.4.24",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["agent_id"].as_str().expect("agent_id").to_string()
}

async fn get_agent(app: &TestApp, tenant_id: &str, agent_id: &str, token: &str) -> Value {
    app.auth_get(&format!("/api/tenant/{tenant_id}/agent/{agent_id}"), token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// The order carries nothing but an id in either direction, and an offline
/// device is queued — with the state saying it cannot act on it yet.
#[tokio::test]
async fn an_offline_device_is_queued_and_the_state_is_honest_about_it() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr40a").await;
    let aid = enroll(&app, &t.tenant_id, &t.admin.access_token, "mach-fr40-a").await;

    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/{aid}/overlay-key/rotate", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["dispatch"].as_str(), Some("queued"));
    assert_eq!(body["delivered"].as_bool(), Some(false));
    let request_id = body["request_id"].as_str().expect("request_id").to_string();
    assert!(
        ObjectId::parse_str(&request_id).is_ok(),
        "request_id is a fresh ObjectId hex: {request_id}"
    );

    // The row now carries the desired state, and the resolved view says the
    // truth about a device that never advertised `key-rotate`: it is queued,
    // and it will not act on the order until it is updated.
    let a = get_agent(&app, &t.tenant_id, &aid, &t.admin.access_token).await;
    let kr = &a["key_rotation"];
    assert_eq!(kr["request_id"].as_str(), Some(request_id.as_str()));
    assert_eq!(kr["state"].as_str(), Some("unsupported"));
    assert!(kr["delivered_at"].is_null(), "{kr}");
    assert!(kr["report"].is_null(), "{kr}");
    // No key of any kind on the response — the device never joined, and the
    // server never had one to show.
    assert!(a["overlay_public_key"].is_null(), "{a}");

    // ONE audit row, recording the dispatch.
    let rows: Vec<bson::Document> = app
        .db
        .collection::<bson::Document>("key_rotation_audit")
        .find(doc! { "agent_id": ObjectId::parse_str(&aid).unwrap() })
        .await
        .unwrap()
        .try_collect_vec()
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].get_str("dispatch").ok(), Some("queued"));
    assert_eq!(
        rows[0].get_str("request_id").ok(),
        Some(request_id.as_str())
    );
    assert!(rows[0].get("denied").is_none(), "{:?}", rows[0]);
}

/// The per-device ceiling: a second order inside a minute is refused, and
/// the refusal is audited like the grant was.
#[tokio::test]
async fn a_second_order_within_a_minute_is_refused_and_audited() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr40b").await;
    let aid = enroll(&app, &t.tenant_id, &t.admin.access_token, "mach-fr40-b").await;
    let path = format!("/api/tenant/{}/agent/{aid}/overlay-key/rotate", t.tenant_id);

    let first = app
        .auth_post(&path, &t.admin.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 200);
    let first: Value = first.json().await.unwrap();

    let second = app
        .auth_post(&path, &t.admin.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status().as_u16(), 409);
    let text = second.text().await.unwrap_or_default();
    assert!(text.contains("rate_limited"), "{text}");

    // The FIRST order still stands on the row — a refused second order must
    // not disturb it.
    let a = get_agent(&app, &t.tenant_id, &aid, &t.admin.access_token).await;
    assert_eq!(
        a["key_rotation"]["request_id"].as_str(),
        first["request_id"].as_str()
    );

    let rows: Vec<bson::Document> = app
        .db
        .collection::<bson::Document>("key_rotation_audit")
        .find(doc! { "agent_id": ObjectId::parse_str(&aid).unwrap() })
        .sort(doc! { "at": 1 })
        .await
        .unwrap()
        .try_collect_vec()
        .await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].get_str("dispatch").ok(), Some("queued"));
    assert_eq!(rows[1].get_str("denied").ok(), Some("rate_limited"));
    assert!(rows[1].get("dispatch").is_none(), "{:?}", rows[1]);
}

/// Membership and MANAGE_AGENTS gate the route; a foreign agent id is a
/// 404 (no cross-tenant order, no existence leak).
#[tokio::test]
async fn members_without_manage_agents_and_foreign_tenants_are_refused() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr40c").await;
    let other = app.seed_tenant("fr40d").await;
    let aid = enroll(&app, &t.tenant_id, &t.admin.access_token, "mach-fr40-c").await;

    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/{aid}/overlay-key/rotate", t.tenant_id),
            &t.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    // The other org's admin, naming this org's agent under THEIR tenant id.
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/agent/{aid}/overlay-key/rotate",
                other.tenant_id
            ),
            &other.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    // Neither refusal wrote an order onto the row.
    let a = get_agent(&app, &t.tenant_id, &aid, &t.admin.access_token).await;
    assert!(a["key_rotation"].is_null(), "{a}");
}

/// Small helper so the audit reads stay one expression.
trait TryCollectVec {
    async fn try_collect_vec(self) -> Vec<bson::Document>;
}

impl TryCollectVec for mongodb::Cursor<bson::Document> {
    async fn try_collect_vec(mut self) -> Vec<bson::Document> {
        use futures::StreamExt;
        let mut out = Vec::new();
        while let Some(item) = self.next().await {
            out.push(item.unwrap());
        }
        out
    }
}

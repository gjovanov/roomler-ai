// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-32 — plan limits, end to end against a real server and a real MongoDB.
//!
//! The unit tests in `services::quota` pin the decision function. These pin the
//! part a pure function cannot: that the gate is actually *wired* at the route,
//! that it reads the tenant's real usage, and that the three modes produce three
//! different HTTP outcomes.
//!
//! ⚠ The most valuable test here is [`an_established_limit_ignores_the_mode`].
//! P0 re-pointed the pre-FR-32 device cap through the same helper as the new
//! gates, and the mode defaults to `Warn` — so a plausible "simplification"
//! (dropping `Limit::is_established`) would silently stop enforcing the device
//! cap fleet-wide while every other test still passed. That is a billing
//! regression inside a diff that reads as a cleanup, which is exactly the kind
//! a unit test on the helper alone would not catch at the route.

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use bson::{Bson, doc, oid::ObjectId};
use serde_json::Value;

/// Set a tenant's plan and enforcement mode directly, the way an operator
/// would during P2. Both are plain fields on the tenant document.
async fn set_plan(app: &TestApp, tenant_id: &str, plan: &str, mode: &str) {
    let tid = ObjectId::parse_str(tenant_id).unwrap();
    app.db
        .collection::<bson::Document>("tenants")
        .update_one(
            doc! { "_id": tid },
            doc! { "$set": { "plan": plan, "settings.plan_enforcement": mode } },
        )
        .await
        .expect("failed to set plan/enforcement");
}

async fn create_room(app: &TestApp, tenant_id: &str, token: &str, name: &str) -> reqwest::Response {
    app.auth_post(&format!("/api/tenant/{tenant_id}/room"), token)
        .json(&serde_json::json!({ "name": name, "is_open": true }))
        .send()
        .await
        .expect("create room request failed")
}

/// Bring the tenant to exactly `target` live channels. A seeded tenant starts
/// with three, and Free caps at five.
async fn fill_channels(app: &TestApp, t: &SeededTenant, target: usize) {
    let tid = ObjectId::parse_str(&t.tenant_id).unwrap();
    loop {
        let n = app
            .db
            .collection::<bson::Document>("rooms")
            .count_documents(doc! { "tenant_id": tid, "deleted_at": Bson::Null })
            .await
            .unwrap() as usize;
        if n >= target {
            break;
        }
        let r = create_room(
            app,
            &t.tenant_id,
            &t.admin.access_token,
            &format!("fill-{n}"),
        )
        .await;
        assert!(r.status().is_success(), "seeding channel {n} failed");
    }
}

// ── max_channels: the three modes, three outcomes ───────────────────

#[tokio::test]
async fn warn_records_the_channel_cap_but_does_not_refuse() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("planwarn").await;
    set_plan(&app, &t.tenant_id, "free", "warn").await;
    fill_channels(&app, &t, 5).await; // Free.max_channels == 5

    let r = create_room(&app, &t.tenant_id, &t.admin.access_token, "over-the-cap").await;
    assert!(
        r.status().is_success(),
        "Warn must record and let the request through, got {}",
        r.status()
    );
}

#[tokio::test]
async fn enforce_refuses_at_the_channel_cap() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("planenf").await;
    set_plan(&app, &t.tenant_id, "free", "enforce").await;
    fill_channels(&app, &t, 5).await;

    let r = create_room(&app, &t.tenant_id, &t.admin.access_token, "over-the-cap").await;
    assert_eq!(r.status(), 403, "Enforce must refuse at the cap");
    let body = r.text().await.unwrap_or_default();
    assert!(
        body.contains("Channels limit reached") && body.contains("5 of 5"),
        "refusal must name the limit and the numbers, got: {body}"
    );
}

#[tokio::test]
async fn off_disables_a_newly_wired_gate_entirely() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("planoff").await;
    set_plan(&app, &t.tenant_id, "free", "off").await;
    fill_channels(&app, &t, 5).await;

    for i in 0..3 {
        let r = create_room(
            &app,
            &t.tenant_id,
            &t.admin.access_token,
            &format!("off-{i}"),
        )
        .await;
        assert!(
            r.status().is_success(),
            "Off must not check at all (iteration {i}), got {}",
            r.status()
        );
    }
}

// ── The regression guard ────────────────────────────────────────────

/// `max_devices` was enforced before FR-32. The staging mode must not be able
/// to weaken it — including under `Off`, which disables every *new* gate.
///
/// If `Limit::is_established` were dropped, this is the test that fails; the
/// channel tests above would all still pass, and the fleet would silently stop
/// enforcing the device cap it bills for.
#[tokio::test]
async fn an_established_limit_ignores_the_mode() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("planest").await;
    // `Off` is the most permissive mode there is.
    set_plan(&app, &t.tenant_id, "free", "off").await;

    // Free.max_devices == 3. Seed the tenant to its cap directly.
    let tid = ObjectId::parse_str(&t.tenant_id).unwrap();
    let now = bson::DateTime::now();
    for i in 0..3 {
        app.db
            .collection::<bson::Document>("agents")
            .insert_one(doc! {
                "tenant_id": tid,
                "machine_id": format!("est-machine-{i}"),
                "name": format!("est-{i}"),
                "created_at": now,
                "updated_at": now,
                "deleted_at": Bson::Null,
            })
            .await
            .expect("agent seed failed");
    }

    // Mint an enrollment token and try to enroll a fourth device.
    let tok: Value = app
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
    let enrollment_token = tok["enrollment_token"]
        .as_str()
        .expect("no token")
        .to_string();

    let r = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&serde_json::json!({
            "enrollment_token": enrollment_token,
            "machine_id": "est-machine-fourth",
            "machine_name": "fourth",
            "os": "linux",
            "agent_version": "0.0.0",
        }))
        .send()
        .await
        .expect("enroll request failed");

    assert_eq!(
        r.status(),
        403,
        "the device cap must refuse even under Off — it predates FR-32 and the \
         staging mode must never be able to turn billing off"
    );
}

// ── Feature gates read as features, not counts ──────────────────────

#[tokio::test]
async fn a_disabled_feature_is_refused_with_feature_wording() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("planfeat").await;
    set_plan(&app, &t.tenant_id, "free", "enforce").await; // Free.recordings == false

    let room = &t.rooms[0].id;
    let r = app
        .auth_post(
            &format!("/api/tenant/{}/room/{}/recording", t.tenant_id, room),
            &t.admin.access_token,
        )
        .json(&serde_json::json!({ "recording_type": "video" }))
        .send()
        .await
        .expect("recording request failed");

    assert_eq!(r.status(), 403);
    let body = r.text().await.unwrap_or_default();
    assert!(
        body.contains("not available on the Free plan"),
        "a disabled feature must not be described by counting, got: {body}"
    );
    assert!(
        !body.contains("0 of 0"),
        "feature gates must never tell a customer to remove something that does \
         not exist, got: {body}"
    );
}

#[tokio::test]
async fn a_feature_the_plan_includes_is_allowed() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("planbiz").await;
    set_plan(&app, &t.tenant_id, "business", "enforce").await; // recordings == true

    let room = &t.rooms[0].id;
    let r = app
        .auth_post(
            &format!("/api/tenant/{}/room/{}/recording", t.tenant_id, room),
            &t.admin.access_token,
        )
        .json(&serde_json::json!({ "recording_type": "video" }))
        .send()
        .await
        .expect("recording request failed");

    assert!(
        r.status().is_success(),
        "Business includes recordings, got {}",
        r.status()
    );
}

// ── The enable-only asymmetry ───────────────────────────────────────

/// Gating a toggle's *disable* path would let a plan downgrade strand a tenant
/// holding a feature they are not allowed to turn off — strictly worse than the
/// feature having been free. Only enabling is gated.
#[tokio::test]
async fn magic_dns_can_always_be_turned_off_but_not_on() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("plandns").await;

    // Grant it on a plan that includes MagicDNS, so the tenant really holds one.
    set_plan(&app, &t.tenant_id, "pro", "enforce").await;
    let on = app
        .auth_put(
            &format!("/api/tenant/{}/magic-dns", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&serde_json::json!({
            "magic_dns_domain": "plandns.example",
            "magic_dns_nameservers": ["1.1.1.1"],
        }))
        .send()
        .await
        .expect("set magic dns failed");
    assert!(on.status().is_success(), "Pro includes MagicDNS");

    // Now downgrade to a plan that does not.
    set_plan(&app, &t.tenant_id, "free", "enforce").await;

    // Turning it back ON is refused...
    let re_on = app
        .auth_put(
            &format!("/api/tenant/{}/magic-dns", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&serde_json::json!({
            "magic_dns_domain": "plandns2.example",
            "magic_dns_nameservers": [],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(re_on.status(), 403, "Free must not be able to set a zone");

    // ...but turning it OFF must always work, or the downgrade traps them.
    let off = app
        .auth_put(
            &format!("/api/tenant/{}/magic-dns", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&serde_json::json!({
            "magic_dns_domain": null,
            "magic_dns_nameservers": [],
        }))
        .send()
        .await
        .unwrap();
    assert!(
        off.status().is_success(),
        "clearing a zone must never be gated, got {}",
        off.status()
    );
}

// ── The unlimited sentinel, through the route ───────────────────────

/// `Pro.max_channels` is `u32::MAX`, the codebase's "unlimited" spelling. If the
/// gate treated it as a real number the report and the refusal would both be
/// wrong; here it must simply never fire.
#[tokio::test]
async fn an_unlimited_plan_never_trips_a_count_gate() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("planpro").await;
    set_plan(&app, &t.tenant_id, "pro", "enforce").await;
    fill_channels(&app, &t, 12).await; // far past Free's 5

    let r = create_room(&app, &t.tenant_id, &t.admin.access_token, "still-fine").await;
    assert!(
        r.status().is_success(),
        "u32::MAX means unlimited, not 4294967295, got {}",
        r.status()
    );
}

// ── The member cap gates BOTH paths ─────────────────────────────────

/// `add_member` is the admin route; invite redemption is the one an invitee
/// walks. Leaving either open makes the cap a formality.
#[tokio::test]
async fn the_member_cap_gates_the_admin_add_path() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("planmem").await;
    set_plan(&app, &t.tenant_id, "free", "enforce").await;

    // Seed to Free's cap of 10 (the tenant starts with 2).
    let tid = ObjectId::parse_str(&t.tenant_id).unwrap();
    let now = bson::DateTime::now();
    let existing = app
        .db
        .collection::<bson::Document>("tenant_members")
        .count_documents(doc! { "tenant_id": tid })
        .await
        .unwrap();
    for i in existing..10 {
        app.db
            .collection::<bson::Document>("tenant_members")
            .insert_one(doc! {
                "tenant_id": tid,
                "user_id": ObjectId::new(),
                "role_ids": [],
                "joined_at": now,
                "is_pending": false,
                "is_muted": false,
                "created_at": now,
                "updated_at": now,
                "nickname": Bson::Null,
                "notification_override": Bson::Null,
                "invited_by": Bson::Null,
                "last_seen_at": Bson::Null,
                "_filler": i as i64,
            })
            .await
            .expect("member seed failed");
    }

    // A real, verified account that is not yet a member.
    let outsider = app
        .register_user(
            "outsider@planmem.test",
            "planmem_outsider",
            "Outsider",
            "Outsider123!",
            None,
            None,
        )
        .await;

    let r = app
        .auth_post(
            &format!("/api/tenant/{}/member", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&serde_json::json!({ "user_id": outsider.id }))
        .send()
        .await
        .expect("add member failed");

    assert_eq!(r.status(), 403, "the member cap must refuse the 11th seat");
    let body = r.text().await.unwrap_or_default();
    assert!(
        body.contains("10 of 10 members used"),
        "refusal must name the real counts, got: {body}"
    );
}

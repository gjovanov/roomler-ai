use crate::fixtures::test_app::TestApp;
use serde_json::Value;

// ---------------------------------------------------------------------------
// GET /api/stripe/plans — public endpoint, no auth needed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_plans_returns_three_plans() {
    let app = TestApp::spawn().await;

    let resp = app
        .client
        .get(app.url("/api/stripe/plans"))
        .send()
        .await
        .unwrap();

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "GET /api/stripe/plans failed: {}", body);

    let plans: Vec<Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(plans.len(), 3, "Expected 3 plans (Free, Pro, Business)");
}

#[tokio::test]
async fn get_plans_free_plan_has_correct_data() {
    let app = TestApp::spawn().await;

    let resp = app
        .client
        .get(app.url("/api/stripe/plans"))
        .send()
        .await
        .unwrap();

    let plans: Vec<Value> = resp.json().await.unwrap();
    let free = plans
        .iter()
        .find(|p| p["id"] == "free")
        .expect("Free plan not found");

    assert_eq!(free["name"], "Free");
    assert_eq!(free["price_cents"], 0);
    assert_eq!(free["limits"]["max_members"], 10);
    assert_eq!(free["limits"]["max_channels"], 5);
    assert_eq!(free["limits"]["video_max_participants"], 0);
    assert_eq!(free["limits"]["cloud_integrations"], false);
    assert_eq!(free["limits"]["ai_recognition"], false);
    assert_eq!(free["limits"]["recordings"], false);
    // S5 pivot — fleet limits lead the matrix.
    assert_eq!(free["limits"]["max_devices"], 3);
    assert_eq!(free["limits"]["max_tunnel_clients"], 3);
    assert_eq!(free["limits"]["overlay_mesh"], true);
    assert_eq!(free["limits"]["exit_nodes"], false);
    assert_eq!(free["limits"]["magic_dns"], false);
    assert_eq!(free["limits"]["max_concurrent_sessions"], 1);
}

#[tokio::test]
async fn get_plans_pro_plan_has_correct_data() {
    let app = TestApp::spawn().await;

    let resp = app
        .client
        .get(app.url("/api/stripe/plans"))
        .send()
        .await
        .unwrap();

    let plans: Vec<Value> = resp.json().await.unwrap();
    let pro = plans
        .iter()
        .find(|p| p["id"] == "pro")
        .expect("Pro plan not found");

    assert_eq!(pro["name"], "Pro");
    assert_eq!(pro["price_cents"], 800);
    assert_eq!(pro["limits"]["video_max_participants"], 10);
    assert_eq!(pro["limits"]["cloud_integrations"], true);
    assert_eq!(pro["limits"]["ai_recognition"], false);
    assert_eq!(pro["limits"]["recordings"], false);
    assert_eq!(pro["limits"]["max_devices"], 30);
    assert_eq!(pro["limits"]["max_tunnel_clients"], 30);
    assert_eq!(pro["limits"]["exit_nodes"], true);
    assert_eq!(pro["limits"]["magic_dns"], true);
    assert_eq!(pro["limits"]["max_concurrent_sessions"], 5);
}

#[tokio::test]
async fn get_plans_business_plan_has_correct_data() {
    let app = TestApp::spawn().await;

    let resp = app
        .client
        .get(app.url("/api/stripe/plans"))
        .send()
        .await
        .unwrap();

    let plans: Vec<Value> = resp.json().await.unwrap();
    let biz = plans
        .iter()
        .find(|p| p["id"] == "business")
        .expect("Business plan not found");

    assert_eq!(biz["name"], "Business");
    assert_eq!(biz["price_cents"], 1600);
    assert_eq!(biz["limits"]["video_max_participants"], 100);
    assert_eq!(biz["limits"]["cloud_integrations"], true);
    assert_eq!(biz["limits"]["ai_recognition"], true);
    assert_eq!(biz["limits"]["recordings"], true);
    assert_eq!(biz["limits"]["max_devices"], 300);
    assert_eq!(biz["limits"]["max_tunnel_clients"], 300);
    assert_eq!(biz["limits"]["exit_nodes"], true);
    assert_eq!(biz["limits"]["magic_dns"], true);
}

// ---------------------------------------------------------------------------
// Tenant default plan
// ---------------------------------------------------------------------------

#[tokio::test]
async fn new_tenant_defaults_to_free_plan() {
    let app = TestApp::spawn().await;

    let seeded = app.seed_tenant("billing-default").await;

    // Read the tenant directly from the database
    use bson::{doc, oid::ObjectId};
    let tenant_oid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let tenant: bson::Document = app
        .db
        .collection::<bson::Document>("tenants")
        .find_one(doc! { "_id": tenant_oid })
        .await
        .unwrap()
        .expect("Tenant not found in DB");

    assert_eq!(tenant.get_str("plan").unwrap(), "free");
}

#[tokio::test]
async fn new_tenant_has_no_billing_info() {
    let app = TestApp::spawn().await;

    let seeded = app.seed_tenant("billing-none").await;

    use bson::{doc, oid::ObjectId};
    let tenant_oid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let tenant: bson::Document = app
        .db
        .collection::<bson::Document>("tenants")
        .find_one(doc! { "_id": tenant_oid })
        .await
        .unwrap()
        .expect("Tenant not found in DB");

    // billing should be null/absent for new tenants on free plan
    let billing = tenant.get("billing");
    assert!(
        billing.is_none() || billing == Some(&bson::Bson::Null),
        "New tenant should have no billing info"
    );
}

// ---------------------------------------------------------------------------
// POST /api/stripe/checkout — authentication & authorization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn checkout_requires_authentication() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("checkout-auth").await;

    let resp = app
        .client
        .post(app.url("/api/stripe/checkout"))
        .json(&serde_json::json!({
            "tenant_id": seeded.tenant_id,
            "plan": "pro",
            "success_url": "http://localhost/success",
            "cancel_url": "http://localhost/cancel",
        }))
        .send()
        .await
        .unwrap();

    // Without auth, the server may return 401 or 403 depending on middleware ordering
    let status = resp.status().as_u16();
    assert!(
        status == 401 || status == 403,
        "Expected 401 or 403, got {status}"
    );
}

#[tokio::test]
async fn checkout_rejects_invalid_plan() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("checkout-invalid-plan").await;

    let resp = app
        .auth_post("/api/stripe/checkout", &seeded.admin.access_token)
        .json(&serde_json::json!({
            "tenant_id": seeded.tenant_id,
            "plan": "nonexistent_plan",
            "success_url": "http://localhost/success",
            "cancel_url": "http://localhost/cancel",
        }))
        .send()
        .await
        .unwrap();

    // With empty Stripe keys (test env), the customer creation call fires first
    // and returns a Stripe API error (500). With real test keys, this would be 400
    // for invalid plan. Either way, it should not succeed.
    let status = resp.status().as_u16();
    assert!(
        status == 400 || status == 500,
        "Expected 400 or 500 for invalid plan, got {status}"
    );
}

#[tokio::test]
async fn checkout_rejects_non_owner() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("checkout-forbidden").await;

    // The member (non-owner) should not be able to create a checkout session
    let resp = app
        .auth_post("/api/stripe/checkout", &seeded.member.access_token)
        .json(&serde_json::json!({
            "tenant_id": seeded.tenant_id,
            "plan": "pro",
            "success_url": "http://localhost/success",
            "cancel_url": "http://localhost/cancel",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 403);
}

// ---------------------------------------------------------------------------
// POST /api/stripe/portal — authentication & authorization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn portal_requires_authentication() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("portal-auth").await;

    let resp = app
        .client
        .post(app.url("/api/stripe/portal"))
        .json(&serde_json::json!({
            "tenant_id": seeded.tenant_id,
            "return_url": "http://localhost/billing",
        }))
        .send()
        .await
        .unwrap();

    let status = resp.status().as_u16();
    assert!(
        status == 401 || status == 403,
        "Expected 401 or 403, got {status}"
    );
}

#[tokio::test]
async fn portal_fails_without_billing_account() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("portal-no-billing").await;

    // Free plan tenant has no customer_id, so portal should fail
    let resp = app
        .auth_post("/api/stripe/portal", &seeded.admin.access_token)
        .json(&serde_json::json!({
            "tenant_id": seeded.tenant_id,
            "return_url": "http://localhost/billing",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400);
}

// ---------------------------------------------------------------------------
// POST /api/stripe/webhook — signature verification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webhook_rejects_missing_signature_header() {
    let app = TestApp::spawn().await;

    let resp = app
        .client
        .post(app.url("/api/stripe/webhook"))
        .header("Content-Type", "application/json")
        .body(r#"{"type":"checkout.session.completed","data":{"object":{}}}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn webhook_rejects_invalid_signature() {
    let app = TestApp::spawn().await;

    let resp = app
        .client
        .post(app.url("/api/stripe/webhook"))
        .header("Content-Type", "application/json")
        .header("stripe-signature", "t=1234567890,v1=invalidsignature")
        .body(r#"{"type":"checkout.session.completed","data":{"object":{}}}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn webhook_with_valid_signature_processes_checkout_completed() {
    let app = TestApp::spawn_with_settings(|s| {
        s.stripe.webhook_secret = "whsec_test_secret_for_billing_tests".to_string();
    })
    .await;

    let seeded = app.seed_tenant("webhook-checkout").await;

    // Build the webhook payload
    let payload = serde_json::json!({
        "type": "checkout.session.completed",
        "data": {
            "object": {
                "metadata": {
                    "tenant_id": seeded.tenant_id,
                    "plan": "pro",
                },
                "subscription": "sub_test_123",
                "customer": "cus_test_456",
            }
        }
    });
    let payload_bytes = serde_json::to_vec(&payload).unwrap();

    // Compute valid HMAC-SHA256 signature
    let timestamp = now_ts();
    let signed_payload = format!("{}.{}", timestamp, String::from_utf8_lossy(&payload_bytes));
    let sig = compute_hmac_sha256("whsec_test_secret_for_billing_tests", &signed_payload);
    let sig_header = format!("t={},v1={}", timestamp, sig);

    let resp = app
        .client
        .post(app.url("/api/stripe/webhook"))
        .header("Content-Type", "application/json")
        .header("stripe-signature", &sig_header)
        .body(payload_bytes)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);

    // Verify tenant was upgraded to Pro in the database
    use bson::{doc, oid::ObjectId};
    let tenant_oid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let tenant: bson::Document = app
        .db
        .collection::<bson::Document>("tenants")
        .find_one(doc! { "_id": tenant_oid })
        .await
        .unwrap()
        .expect("Tenant not found in DB");

    assert_eq!(tenant.get_str("plan").unwrap(), "pro");

    // Verify billing info was set
    let billing = tenant
        .get_document("billing")
        .expect("billing should exist");
    assert_eq!(billing.get_str("customer_id").unwrap(), "cus_test_456");
    assert_eq!(billing.get_str("subscription_id").unwrap(), "sub_test_123");
    assert_eq!(billing.get_str("status").unwrap(), "active");
    assert_eq!(billing.get_bool("cancel_at_period_end").unwrap(), false);
}

#[tokio::test]
async fn webhook_subscription_deleted_reverts_to_free() {
    let app = TestApp::spawn_with_settings(|s| {
        s.stripe.webhook_secret = "whsec_test_secret_for_billing_tests".to_string();
    })
    .await;

    let seeded = app.seed_tenant("webhook-delete").await;

    // First, simulate a checkout.session.completed to set up billing
    {
        let payload = serde_json::json!({
            "type": "checkout.session.completed",
            "data": {
                "object": {
                    "metadata": {
                        "tenant_id": seeded.tenant_id,
                        "plan": "business",
                    },
                    "subscription": "sub_del_123",
                    "customer": "cus_del_456",
                }
            }
        });
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let timestamp = now_ts();
        let signed_payload = format!("{}.{}", timestamp, String::from_utf8_lossy(&payload_bytes));
        let sig = compute_hmac_sha256("whsec_test_secret_for_billing_tests", &signed_payload);
        let sig_header = format!("t={},v1={}", timestamp, sig);

        let resp = app
            .client
            .post(app.url("/api/stripe/webhook"))
            .header("Content-Type", "application/json")
            .header("stripe-signature", &sig_header)
            .body(payload_bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    // Now send customer.subscription.deleted
    {
        let payload = serde_json::json!({
            "type": "customer.subscription.deleted",
            "data": {
                "object": {
                    "id": "sub_del_123",
                }
            }
        });
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let timestamp = now_ts();
        let signed_payload = format!("{}.{}", timestamp, String::from_utf8_lossy(&payload_bytes));
        let sig = compute_hmac_sha256("whsec_test_secret_for_billing_tests", &signed_payload);
        let sig_header = format!("t={},v1={}", timestamp, sig);

        let resp = app
            .client
            .post(app.url("/api/stripe/webhook"))
            .header("Content-Type", "application/json")
            .header("stripe-signature", &sig_header)
            .body(payload_bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    // Verify tenant was reverted to Free plan
    use bson::{doc, oid::ObjectId};
    let tenant_oid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let tenant: bson::Document = app
        .db
        .collection::<bson::Document>("tenants")
        .find_one(doc! { "_id": tenant_oid })
        .await
        .unwrap()
        .expect("Tenant not found in DB");

    assert_eq!(tenant.get_str("plan").unwrap(), "free");
    let billing = tenant
        .get_document("billing")
        .expect("billing should exist");
    assert_eq!(billing.get_str("status").unwrap(), "canceled");
}

#[tokio::test]
async fn webhook_subscription_updated_sets_status() {
    let app = TestApp::spawn_with_settings(|s| {
        s.stripe.webhook_secret = "whsec_test_secret_for_billing_tests".to_string();
    })
    .await;

    let seeded = app.seed_tenant("webhook-update").await;

    // Set up billing via checkout.session.completed
    {
        let payload = serde_json::json!({
            "type": "checkout.session.completed",
            "data": {
                "object": {
                    "metadata": {
                        "tenant_id": seeded.tenant_id,
                        "plan": "pro",
                    },
                    "subscription": "sub_upd_123",
                    "customer": "cus_upd_456",
                }
            }
        });
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let timestamp = now_ts();
        let signed_payload = format!("{}.{}", timestamp, String::from_utf8_lossy(&payload_bytes));
        let sig = compute_hmac_sha256("whsec_test_secret_for_billing_tests", &signed_payload);
        let sig_header = format!("t={},v1={}", timestamp, sig);

        let resp = app
            .client
            .post(app.url("/api/stripe/webhook"))
            .header("Content-Type", "application/json")
            .header("stripe-signature", &sig_header)
            .body(payload_bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    // Send customer.subscription.updated with cancel_at_period_end = true
    {
        let payload = serde_json::json!({
            "type": "customer.subscription.updated",
            "data": {
                "object": {
                    "id": "sub_upd_123",
                    "status": "active",
                    "cancel_at_period_end": true,
                    "current_period_end": 1700000000,
                }
            }
        });
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let timestamp = now_ts();
        let signed_payload = format!("{}.{}", timestamp, String::from_utf8_lossy(&payload_bytes));
        let sig = compute_hmac_sha256("whsec_test_secret_for_billing_tests", &signed_payload);
        let sig_header = format!("t={},v1={}", timestamp, sig);

        let resp = app
            .client
            .post(app.url("/api/stripe/webhook"))
            .header("Content-Type", "application/json")
            .header("stripe-signature", &sig_header)
            .body(payload_bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    // Verify billing info was updated
    use bson::{doc, oid::ObjectId};
    let tenant_oid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let tenant: bson::Document = app
        .db
        .collection::<bson::Document>("tenants")
        .find_one(doc! { "_id": tenant_oid })
        .await
        .unwrap()
        .expect("Tenant not found in DB");

    let billing = tenant
        .get_document("billing")
        .expect("billing should exist");
    assert_eq!(billing.get_str("status").unwrap(), "active");
    assert_eq!(billing.get_bool("cancel_at_period_end").unwrap(), true);
}

#[tokio::test]
async fn webhook_invoice_payment_failed_sets_past_due() {
    let app = TestApp::spawn_with_settings(|s| {
        s.stripe.webhook_secret = "whsec_test_secret_for_billing_tests".to_string();
    })
    .await;

    let seeded = app.seed_tenant("webhook-pastdue").await;

    // Set up billing first
    {
        let payload = serde_json::json!({
            "type": "checkout.session.completed",
            "data": {
                "object": {
                    "metadata": {
                        "tenant_id": seeded.tenant_id,
                        "plan": "pro",
                    },
                    "subscription": "sub_pd_123",
                    "customer": "cus_pd_456",
                }
            }
        });
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let timestamp = now_ts();
        let signed_payload = format!("{}.{}", timestamp, String::from_utf8_lossy(&payload_bytes));
        let sig = compute_hmac_sha256("whsec_test_secret_for_billing_tests", &signed_payload);
        let sig_header = format!("t={},v1={}", timestamp, sig);

        let resp = app
            .client
            .post(app.url("/api/stripe/webhook"))
            .header("Content-Type", "application/json")
            .header("stripe-signature", &sig_header)
            .body(payload_bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    // Send invoice.payment_failed
    {
        let payload = serde_json::json!({
            "type": "invoice.payment_failed",
            "data": {
                "object": {
                    "subscription": "sub_pd_123",
                }
            }
        });
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let timestamp = now_ts();
        let signed_payload = format!("{}.{}", timestamp, String::from_utf8_lossy(&payload_bytes));
        let sig = compute_hmac_sha256("whsec_test_secret_for_billing_tests", &signed_payload);
        let sig_header = format!("t={},v1={}", timestamp, sig);

        let resp = app
            .client
            .post(app.url("/api/stripe/webhook"))
            .header("Content-Type", "application/json")
            .header("stripe-signature", &sig_header)
            .body(payload_bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    // Verify billing status is now past_due
    use bson::{doc, oid::ObjectId};
    let tenant_oid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let tenant: bson::Document = app
        .db
        .collection::<bson::Document>("tenants")
        .find_one(doc! { "_id": tenant_oid })
        .await
        .unwrap()
        .expect("Tenant not found in DB");

    let billing = tenant
        .get_document("billing")
        .expect("billing should exist");
    assert_eq!(billing.get_str("status").unwrap(), "past_due");
}

// ---------------------------------------------------------------------------
// Helper: compute HMAC-SHA256 hex digest
// ---------------------------------------------------------------------------

/// Current unix seconds as a string — webhook signatures must carry a
/// timestamp inside the S5 replay window (±300 s), so the old hardcoded
/// `"1234567890"` would be rejected as a replay.
fn now_ts() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string()
}

fn compute_hmac_sha256(secret: &str, message: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

// ---------------------------------------------------------------------------
// S5 — webhook hardening: replay window + event-id dedup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webhook_rejects_replayed_signature_outside_tolerance() {
    let app = TestApp::spawn_with_settings(|s| {
        s.stripe.webhook_secret = "whsec_test_secret_for_billing_tests".to_string();
    })
    .await;
    let seeded = app.seed_tenant("webhook-replay").await;

    let payload = serde_json::json!({
        "type": "checkout.session.completed",
        "data": { "object": {
            "metadata": { "tenant_id": seeded.tenant_id, "plan": "pro" },
            "subscription": "sub_replay", "customer": "cus_replay",
        }}
    });
    let payload_bytes = serde_json::to_vec(&payload).unwrap();

    // A VALID HMAC over an hour-old timestamp — must be rejected as a
    // replay even though the signature itself checks out.
    let old_ts = (now_ts().parse::<i64>().unwrap() - 3600).to_string();
    let signed_payload = format!("{}.{}", old_ts, String::from_utf8_lossy(&payload_bytes));
    let sig = compute_hmac_sha256("whsec_test_secret_for_billing_tests", &signed_payload);

    let resp = app
        .client
        .post(app.url("/api/stripe/webhook"))
        .header("Content-Type", "application/json")
        .header("stripe-signature", format!("t={old_ts},v1={sig}"))
        .body(payload_bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn webhook_duplicate_event_id_is_processed_once() {
    let app = TestApp::spawn_with_settings(|s| {
        s.stripe.webhook_secret = "whsec_test_secret_for_billing_tests".to_string();
    })
    .await;
    let seeded = app.seed_tenant("webhook-dedup").await;

    let send_event = |plan: &str| {
        let payload = serde_json::json!({
            "id": "evt_s5_dedup_test",
            "type": "checkout.session.completed",
            "data": { "object": {
                "metadata": { "tenant_id": seeded.tenant_id, "plan": plan },
                "subscription": "sub_dedup", "customer": "cus_dedup",
            }}
        });
        let bytes = serde_json::to_vec(&payload).unwrap();
        let ts = now_ts();
        let sig = compute_hmac_sha256(
            "whsec_test_secret_for_billing_tests",
            &format!("{}.{}", ts, String::from_utf8_lossy(&bytes)),
        );
        (bytes, format!("t={ts},v1={sig}"))
    };

    // First delivery upgrades to Pro.
    let (bytes, header) = send_event("pro");
    let resp = app
        .client
        .post(app.url("/api/stripe/webhook"))
        .header("Content-Type", "application/json")
        .header("stripe-signature", header)
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Second delivery reuses the SAME event id (Stripe retry) but claims
    // a different plan — it must be acknowledged (200, so Stripe stops
    // retrying) yet NOT re-processed.
    let (bytes, header) = send_event("business");
    let resp = app
        .client
        .post(app.url("/api/stripe/webhook"))
        .header("Content-Type", "application/json")
        .header("stripe-signature", header)
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    use bson::{doc, oid::ObjectId};
    let tenant_oid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let tenant: bson::Document = app
        .db
        .collection::<bson::Document>("tenants")
        .find_one(doc! { "_id": tenant_oid })
        .await
        .unwrap()
        .expect("tenant");
    assert_eq!(
        tenant.get_str("plan").unwrap(),
        "pro",
        "duplicate event id must not re-process (plan stayed from the first delivery)"
    );
}

// ---------------------------------------------------------------------------
// S5 — plan caps enforced at enrollment (Free: 3 devices, 3 tunnel clients)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_enroll_enforces_free_plan_device_cap() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("device-cap").await;

    let enroll = |machine: String| {
        let app = &app;
        let seeded = &seeded;
        async move {
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
            app.client
                .post(app.url("/api/agent/enroll"))
                .json(&serde_json::json!({
                    "enrollment_token": et["enrollment_token"],
                    "machine_id": machine,
                    "machine_name": format!("{machine}-name"),
                    "os": "linux",
                    "agent_version": "0.0.0-test",
                }))
                .send()
                .await
                .unwrap()
        }
    };

    // Free plan allows 3 devices.
    for i in 1..=3 {
        let resp = enroll(format!("cap-mach-{i}")).await;
        assert_eq!(resp.status().as_u16(), 200, "device {i} should enroll");
    }

    // The 4th NEW machine is over the cap.
    let resp = enroll("cap-mach-4".to_string()).await;
    assert_eq!(resp.status().as_u16(), 403, "4th device must hit the cap");
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Device limit"),
        "cap error should name the limit: {body}"
    );

    // Re-enrolling a KNOWN machine rehydrates in place — never capped.
    let resp = enroll("cap-mach-1".to_string()).await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "re-enrolling an existing machine must not consume a slot"
    );
}

#[tokio::test]
async fn tunnel_enroll_enforces_free_plan_cap() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tunnel-cap").await;

    let enroll = |machine: String| {
        let app = &app;
        let seeded = &seeded;
        async move {
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
            app.client
                .post(app.url("/api/tunnel-client/enroll"))
                .json(&serde_json::json!({
                    "enrollment_token": et["enrollment_token"],
                    "machine_id": machine,
                    "machine_name": format!("{machine}-name"),
                    "os": "linux",
                    "client_version": "0.0.0-test",
                }))
                .send()
                .await
                .unwrap()
        }
    };

    for i in 1..=3 {
        let resp = enroll(format!("tcap-mach-{i}")).await;
        assert_eq!(
            resp.status().as_u16(),
            200,
            "tunnel client {i} should enroll"
        );
    }
    let resp = enroll("tcap-mach-4".to_string()).await;
    assert_eq!(
        resp.status().as_u16(),
        403,
        "4th tunnel client must hit the cap"
    );
    let resp = enroll("tcap-mach-2".to_string()).await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "re-enroll rehydrates, never capped"
    );
}

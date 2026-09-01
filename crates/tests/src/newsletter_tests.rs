// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-58 — the newsletter admin surface (issues CRUD, preview, test-send),
//! against a real server and a real MongoDB.
//!
//! The load-bearing assertions: the platform-admin gate answers **404** for
//! both an unset allowlist and a non-listed caller (never 403 — the web
//! client force-logs-out on 403); a slug is claimed by the unique index (409,
//! never a second issue); an issue that left `draft` is not editable; the
//! preview is the branded send-path bytes with raw operator HTML structurally
//! absent; and test-send refuses loudly when no mailer is configured.

use crate::fixtures::test_app::TestApp;
use bson::{doc, oid::ObjectId};

fn issue_json(slug: &str) -> serde_json::Value {
    serde_json::json!({
        "slug": slug,
        "subject": "Three products, one daemon",
        "preheader": "Why one agent per machine is the whole point.",
        "body_md": "## The story\n\nOne daemon per machine.\n\n[Read more](https://roomler.ai/)\n\n<script>alert(1)</script>",
        "hero_url": "https://roomler.ai/newsletter-img/test-v1.png",
        "hero_alt": "One daemon radiating four capabilities",
        "cta_text": "Try Roomler",
        "cta_url": "https://roomler.ai/"
    })
}

/// Mint an access token for an id without creating a user row — the admin
/// endpoints never read the users collection, so the JWT is what's exercised
/// (same pattern as `stats_tests::admin_stats_gate_…`).
fn token_for(app: &TestApp, id: ObjectId) -> String {
    app.state
        .auth
        .generate_tokens(id, "who@test.io", "who")
        .unwrap()
        .access_token
}

#[tokio::test]
async fn every_admin_newsletter_route_is_404_without_authority() {
    // Arm 1: allowlist UNSET (the default) — the whole surface must not exist.
    let app = TestApp::spawn().await;
    let token = token_for(&app, ObjectId::new());

    let gets = [
        "/api/admin/newsletter/issues",
        "/api/admin/newsletter/issues/some-slug",
        "/api/admin/newsletter/issues/some-slug/preview",
    ];
    for path in gets {
        let r = app.auth_get(path, &token).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 404, "GET {path} with unset allowlist");
    }
    let r = app
        .auth_post("/api/admin/newsletter/issues", &token)
        .json(&issue_json("x"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404, "POST create with unset allowlist");

    // Arm 2: allowlist SET, caller is a different authenticated id — same 404,
    // and the admin id itself passes (proving the 404 is the gate, not a
    // missing route).
    let admin_id = ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(admin_id.to_hex());
    })
    .await;
    let outsider = token_for(&app, ObjectId::new());
    let r = app
        .auth_get("/api/admin/newsletter/issues", &outsider)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        404,
        "a non-listed caller must get 404, NEVER 403 (the client logs out on 403)"
    );
    let admin = token_for(&app, admin_id);
    let r = app
        .auth_get("/api/admin/newsletter/issues", &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        200,
        "the allowlisted id passes the gate"
    );
}

#[tokio::test]
async fn draft_crud_roundtrip_slug_conflict_and_edit_lock() {
    let admin_id = ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(admin_id.to_hex());
    })
    .await;
    let admin = token_for(&app, admin_id);

    // Create.
    let r = app
        .auth_post("/api/admin/newsletter/issues", &admin)
        .json(&issue_json("first-issue"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let created: serde_json::Value = r.json().await.unwrap();
    assert_eq!(created["slug"], "first-issue");
    assert_eq!(created["status"], "draft");
    assert!(
        created["created_at"].as_str().unwrap().contains('T'),
        "timestamps must be plain RFC 3339 strings on the wire, not bson objects"
    );

    // The slug is a claim: a second create answers 409 and no second row exists.
    let r = app
        .auth_post("/api/admin/newsletter/issues", &admin)
        .json(&issue_json("first-issue"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 409, "the unique index arbitrates");
    let n = app
        .db
        .collection::<bson::Document>("newsletter_issues")
        .count_documents(doc! { "slug": "first-issue" })
        .await
        .unwrap();
    assert_eq!(n, 1);

    // Update while draft: change the subject, REMOVE the CTA — an omitted
    // optional field must be unset, not silently kept.
    let mut edited = issue_json("ignored");
    edited["subject"] = serde_json::json!("A better subject");
    edited.as_object_mut().unwrap().remove("cta_text");
    edited.as_object_mut().unwrap().remove("cta_url");
    edited.as_object_mut().unwrap().remove("slug");
    let r = app
        .auth_put("/api/admin/newsletter/issues/first-issue", &admin)
        .json(&edited)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let updated: serde_json::Value = r.json().await.unwrap();
    assert_eq!(updated["subject"], "A better subject");
    assert!(
        updated["cta_text"].is_null(),
        "a removed optional field must actually be gone"
    );

    // Unknown slug → 404 (an update must never upsert a new issue).
    let r = app
        .auth_put("/api/admin/newsletter/issues/typo-slug", &admin)
        .json(&edited)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);

    // Once the issue leaves draft, edits answer 409 with the reason.
    app.db
        .collection::<bson::Document>("newsletter_issues")
        .update_one(
            doc! { "slug": "first-issue" },
            doc! { "$set": { "status": "sending" } },
        )
        .await
        .unwrap();
    let r = app
        .auth_put("/api/admin/newsletter/issues/first-issue", &admin)
        .json(&edited)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 409, "a non-draft is not editable");

    // List shows it.
    let r = app
        .auth_get("/api/admin/newsletter/issues", &admin)
        .send()
        .await
        .unwrap();
    let list: serde_json::Value = r.json().await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn preview_is_the_branded_send_bytes_and_drops_raw_html() {
    let admin_id = ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(admin_id.to_hex());
    })
    .await;
    let admin = token_for(&app, admin_id);

    app.auth_post("/api/admin/newsletter/issues", &admin)
        .json(&issue_json("preview-me"))
        .send()
        .await
        .unwrap();

    let r = app
        .auth_get("/api/admin/newsletter/issues/preview-me/preview", &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let ct = r
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(ct.starts_with("text/html"), "content-type was {ct:?}");
    let html = r.text().await.unwrap();

    // Branded wrapper + rendered markdown.
    assert!(html.contains("Field Notes"));
    assert!(html.contains("The story"), "the md body must be rendered");
    assert!(html.contains("https://roomler.ai/newsletter-img/test-v1.png"));
    // The raw-HTML boundary: the <script> in body_md is structurally absent.
    assert!(
        !html.contains("<script"),
        "raw operator HTML must never reach the rendered email"
    );
    // Preview IS the sent artifact: the placeholder is substituted, none left.
    assert!(!html.contains("%%UNSUBSCRIBE_URL%%"));
    assert!(html.contains("/api/subscribe/unsubscribe/preview-sample-token"));
}

#[tokio::test]
async fn test_send_refuses_loudly_without_a_mailer() {
    let admin_id = ObjectId::new();
    // Default test settings leave email unconfigured ⇒ state.email = None.
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(admin_id.to_hex());
    })
    .await;
    let admin = token_for(&app, admin_id);

    app.auth_post("/api/admin/newsletter/issues", &admin)
        .json(&issue_json("no-mailer"))
        .send()
        .await
        .unwrap();

    let r = app
        .auth_post("/api/admin/newsletter/issues/no-mailer/test-send", &admin)
        .json(&serde_json::json!({ "email": "operator@test.io" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        400,
        "no mailer must be a loud refusal, not a silent skip"
    );

    // And the issue is untouched.
    let row = app
        .db
        .collection::<bson::Document>("newsletter_issues")
        .find_one(doc! { "slug": "no-mailer" })
        .await
        .unwrap()
        .expect("issue row exists");
    assert_eq!(row.get_str("status").unwrap(), "draft");
}

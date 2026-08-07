//! Archiving an organization.
//!
//! `Tenant.is_archived` existed from the beginning and nothing read it, so
//! "retire an org" had no meaning and a throwaway org was permanent — which
//! is what blocked the multi-org refusal field test (docs/multi-org.md §12).
//!
//! It means this now: **an archived org stops acting and keeps everything it
//! knows.** These tests pin the five effects and, just as importantly, the
//! things archiving must NOT do.

use crate::fixtures::test_app::TestApp;
use serde_json::{Value, json};

/// Mint an enrollment token and enroll a device, returning its agent id.
async fn enroll_device(app: &TestApp, tenant_id: &str, admin_token: &str, machine: &str) -> String {
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
    let resp = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": machine,
            "machine_name": machine,
            "os": "linux",
            "agent_version": "0.3.0-rc.322",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "enroll should succeed");
    let ej: Value = resp.json().await.unwrap();
    ej["agent_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn archiving_revokes_devices_hides_the_org_and_refuses_new_enrollments() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("throwaway").await;

    let agent_id =
        enroll_device(&app, &org.tenant_id, &org.admin.access_token, "mach-arch-1").await;
    assert!(!agent_id.is_empty());

    // The org is visible in the switcher before archiving.
    let before: Vec<Value> = app
        .auth_get("/api/tenant", &org.admin.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        before.iter().any(|t| t["id"] == org.tenant_id.as_str()),
        "the org should be listed before archiving"
    );

    // Archive it.
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/archive", org.tenant_id),
            &org.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["is_archived"], json!(true));
    assert_eq!(
        body["devices_revoked"].as_u64().unwrap(),
        1,
        "the enrolled device's enrollment must be revoked"
    );

    // 1 — hidden from the switcher, and visible again only on request.
    let after: Vec<Value> = app
        .auth_get("/api/tenant", &org.admin.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !after.iter().any(|t| t["id"] == org.tenant_id.as_str()),
        "an archived org must not appear in the switcher"
    );
    let all: Vec<Value> = app
        .auth_get("/api/tenant?include_archived=true", &org.admin.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        all.iter().any(|t| t["id"] == org.tenant_id.as_str()),
        "?include_archived=true must still show it — archiving is not erasure"
    );

    // 2 — no new enrollments. This is what makes retiring a throwaway org
    // final; without it the org keeps collecting devices forever.
    let et: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", org.tenant_id),
            &org.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resp = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": "mach-arch-2",
            "machine_name": "later arrival",
            "os": "linux",
            "agent_version": "0.3.0-rc.322",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "an archived org must accept no device enrollments"
    );

    // 5 — the org's DATA is retained. Rooms are the cheapest witness: the
    // seed created three and archiving must not have touched them.
    let rooms: Vec<Value> = app
        .auth_get(
            &format!("/api/tenant/{}/room", org.tenant_id),
            &org.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        rooms.len(),
        3,
        "archiving retires an org, it does not delete its content"
    );
}

#[tokio::test]
async fn unarchiving_restores_the_org_and_lets_devices_enroll_again() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("comeback").await;

    app.auth_post(
        &format!("/api/tenant/{}/archive", org.tenant_id),
        &org.admin.access_token,
    )
    .send()
    .await
    .unwrap();

    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/unarchive", org.tenant_id),
            &org.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["is_archived"], json!(false));

    let listed: Vec<Value> = app
        .auth_get("/api/tenant", &org.admin.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        listed.iter().any(|t| t["id"] == org.tenant_id.as_str()),
        "a restored org is back in the switcher"
    );

    // And it takes devices again — the gate was the flag, not a tombstone.
    let agent_id =
        enroll_device(&app, &org.tenant_id, &org.admin.access_token, "mach-back-1").await;
    assert!(!agent_id.is_empty());
}

#[tokio::test]
async fn only_the_owner_may_archive_and_only_once() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("ownergate").await;
    let other = app.seed_tenant("stranger").await;

    // A member of a DIFFERENT org cannot archive this one. Archiving
    // revokes every device's enrollment, so it is owner-only on purpose —
    // not a MANAGE_TENANT delegate's call.
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/archive", org.tenant_id),
            &other.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    // The owner can.
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/archive", org.tenant_id),
            &org.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Twice is a mistake worth naming rather than a silent no-op — the
    // second call would report devices_revoked=0 and read like success.
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/archive", org.tenant_id),
            &org.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

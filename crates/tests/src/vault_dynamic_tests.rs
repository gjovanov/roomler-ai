// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-6b-1 — dynamic secrets, the vault's half: an issuer the vault
//! stores and a device mints with. A dynamic secret is declared with its
//! provider and one audience, holds its issuer's fields alone, and is leased
//! in `short-lived` as a whole: every field in one bundle sealed to the
//! device. The pairing of the kind with its mode is the guardrails', and each
//! refusal is recorded with the guardrail that made it.

use bson::doc;
use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::vault_lease_tests::{
    TAKES_LEASES, ask, device, fresh_kek, lease_app, opened, post, refused, rows, seed_vault, start,
};

/// Write one field's value.
pub(crate) async fn put(
    app: &TestApp,
    t: &SeededTenant,
    id: &str,
    field: &str,
    value: &str,
) -> u16 {
    app.auth_put(
        &format!("/api/tenant/{}/vault/secret/{id}/value", t.tenant_id),
        &t.admin.access_token,
    )
    .json(&json!({ "field": field, "value": value }))
    .send()
    .await
    .unwrap()
    .status()
    .as_u16()
}

/// The issuer's fields, made here and never written down.
pub(crate) struct Issuer {
    pub(crate) user: String,
    pub(crate) password: String,
}

impl Issuer {
    pub(crate) fn made() -> Self {
        Self {
            user: format!("issuer-{}", bson::oid::ObjectId::new().to_hex()),
            password: format!("pw-{}", bson::oid::ObjectId::new().to_hex()),
        }
    }
}

/// `db/dyn`, a Postgres issuer toward `db.internal:5432`, inside the folder
/// `dev` may use; its id.
pub(crate) async fn dynamic_secret(
    app: &TestApp,
    t: &SeededTenant,
    issuer: &Issuer,
    with_password: bool,
) -> String {
    let (code, body) = post(
        app,
        t,
        "/vault/secret",
        json!({ "path": "db/dyn", "kind": "dynamic", "provider": "postgres",
                "audiences": ["db.internal:5432"] }),
    )
    .await;
    assert_eq!(code, 201, "{body}");
    assert_eq!(body["provider"], "postgres", "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    assert_eq!(put(app, t, &id, "admin_user", &issuer.user).await, 200);
    if with_password {
        assert_eq!(
            put(app, t, &id, "admin_password", &issuer.password).await,
            200
        );
    }
    assert_eq!(put(app, t, &id, "database", "orders").await, 200);
    id
}

pub(crate) fn short_lived(sid: &str, fence: u64) -> Value {
    json!({ "session_id": sid, "fence": fence, "secret": "db/dyn", "mode": "short-lived" })
}

/// A dynamic secret is leased as a whole: one bundle, the issuer's fields
/// and the audience to mint toward, sealed to the device, its lease row at
/// each field's version.
#[tokio::test]
async fn a_dynamic_secret_is_leased_whole_its_issuer_sealed_to_the_device() {
    let app = lease_app().await;
    let t = app.seed_tenant("vaultdyn").await;
    seed_vault(&app, &t).await;
    let issuer = Issuer::made();
    dynamic_secret(&app, &t, &issuer, true).await;
    let mut dev = device(&app, &t, "dyn-linux", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/dyn", Some("dev")).await;

    let answer = ask(&mut dev, "d-1", short_lived(&sid, fence)).await;
    assert_eq!(answer["provider"], "postgres", "{answer}");
    assert_eq!(answer["n"], 0, "a bundle spans its fields: {answer}");
    let bundle: Value = serde_json::from_str(&opened(&dev, &answer)).unwrap();
    assert_eq!(
        bundle,
        json!({
            "audience": "db.internal:5432",
            "fields": {
                "admin_user": issuer.user,
                "admin_password": issuer.password,
                "database": "orders",
            },
        })
    );
    let lease = rows(&app, "vault_leases", doc! {}).await;
    assert_eq!(lease.len(), 1);
    let row = &lease[0];
    assert_eq!(
        (
            row.get_str("field").unwrap(),
            row.get_i64("n").unwrap(),
            row.get_str("mode").unwrap(),
            row.get_str("audience").unwrap(),
        ),
        ("", 0, "short-lived", "db.internal:5432")
    );
    // The fields it carried, each at its first version.
    let versions = row.get_document("bundle").unwrap();
    let names: Vec<&str> = versions.keys().map(String::as_str).collect();
    assert_eq!(names, ["admin_password", "admin_user", "database"]);
    assert!(
        versions.values().all(|n| n.as_i64() == Some(1)),
        "{versions:?}"
    );
    // The secret's own row never holds a value, and the audit names none.
    for collection in ["vault_secrets", "vault_audit", "vault_leases"] {
        for d in rows(&app, collection, doc! {}).await {
            let text = d.to_string();
            assert!(!text.contains(&issuer.password), "{collection}: {text}");
        }
    }
}

/// The guardrails pair the kind with its mode, whatever a policy permits: a
/// dynamic secret in any other mode, a stored one in `short-lived`, each
/// refused and recorded with the guardrail that made it. A field named, an
/// issuer missing what a mint needs: refused too.
#[tokio::test]
async fn a_dynamic_secret_is_leased_short_lived_alone_and_whole() {
    let app = lease_app().await;
    let t = app.seed_tenant("vaultdyn2").await;
    seed_vault(&app, &t).await;
    dynamic_secret(&app, &t, &Issuer::made(), true).await;
    let mut dev = device(&app, &t, "dyn-pair", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/dyn2", Some("dev")).await;

    for (i, mode) in ["env", "file", "exec", "proxy"].iter().enumerate() {
        let mut frame = short_lived(&sid, fence);
        frame["mode"] = json!(mode);
        frame["audience"] = json!("db.internal:5432");
        let answer = ask(&mut dev, &format!("m-{i}"), frame).await;
        assert_eq!(refused(&answer), "denied", "{mode}: {answer}");
    }
    let mut stored = short_lived(&sid, fence);
    stored["secret"] = json!("db/main");
    assert_eq!(refused(&ask(&mut dev, "s-1", stored).await), "denied");

    let denied = rows(
        &app,
        "vault_audit",
        doc! { "action": "secret:use", "decision": "deny" },
    )
    .await;
    // A reason names every guardrail that decided (`exec` meets the exec
    // guardrail too, the secret naming no program), so count them.
    let reasons: Vec<&str> = denied
        .iter()
        .map(|r| r.get_str("reason").unwrap())
        .collect();
    let by = |g: &str| reasons.iter().filter(|r| r.contains(g)).count();
    assert_eq!(reasons.len(), 5, "{reasons:?}");
    assert_eq!(by("guardrail:dynamic-only-short-lived"), 4, "{reasons:?}");
    assert_eq!(by("guardrail:short-lived-only-dynamic"), 1, "{reasons:?}");
    for row in &denied {
        assert!(row.contains_key("request"), "replayable: {row:?}");
    }

    // The secret as a whole: a field named is no session's to have.
    let mut field = short_lived(&sid, fence);
    field["field"] = json!("admin_password");
    assert_eq!(refused(&ask(&mut dev, "f-1", field).await), "no_value");
    assert!(rows(&app, "vault_leases", doc! {}).await.is_empty());
}

/// An issuer missing a field no mint can do without is refused, recorded;
/// and so is a provider this server does not serve.
#[tokio::test]
async fn an_issuer_short_of_a_mint_or_a_provider_not_served_is_refused() {
    let app = lease_app().await;
    let t = app.seed_tenant("vaultdyn3").await;
    seed_vault(&app, &t).await;
    dynamic_secret(&app, &t, &Issuer::made(), false).await;
    let mut dev = device(&app, &t, "dyn-short", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/dyn3", Some("dev")).await;
    assert_eq!(
        refused(&ask(&mut dev, "x-1", short_lived(&sid, fence)).await),
        "no_value"
    );
    let reasons: Vec<String> = rows(&app, "vault_audit", doc! { "decision": "deny" })
        .await
        .iter()
        .map(|r| r.get_str("reason").unwrap().to_string())
        .collect();
    assert_eq!(reasons, ["issuer_lacks_admin_password"]);

    let kek = fresh_kek();
    let off = TestApp::spawn_with_settings(move |s| {
        s.modules.hive = true;
        s.modules.vault = true;
        s.vault.kek = kek;
        s.vault.providers = "none".into();
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await;
    let t = off.seed_tenant("vaultdyn4").await;
    seed_vault(&off, &t).await;
    dynamic_secret(&off, &t, &Issuer::made(), true).await;
    let mut dev = device(&off, &t, "dyn-off", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&off, &t, &mut dev, "/srv/dyn4", Some("dev")).await;
    assert_eq!(
        refused(&ask(&mut dev, "o-1", short_lived(&sid, fence)).await),
        "not_served"
    );
    let reasons: Vec<String> = rows(&off, "vault_audit", doc! { "decision": "deny" })
        .await
        .iter()
        .map(|r| r.get_str("reason").unwrap().to_string())
        .collect();
    assert_eq!(reasons, ["provider_not_served"]);
}

/// A Windows device is never leased a dynamic secret: `short-lived`
/// materializes a credential, and Windows has no sandbox to keep it in.
#[tokio::test]
async fn a_windows_device_is_never_leased_a_dynamic_secret() {
    let app = lease_app().await;
    let t = app.seed_tenant("vaultdyn5").await;
    seed_vault(&app, &t).await;
    dynamic_secret(&app, &t, &Issuer::made(), true).await;
    let mut dev = device(&app, &t, "dyn-win", "windows", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "C:\\srv\\dyn5", Some("dev")).await;
    assert_eq!(
        refused(&ask(&mut dev, "w-1", short_lived(&sid, fence)).await),
        "denied"
    );
    let denied = rows(&app, "vault_audit", doc! { "decision": "deny" }).await;
    assert_eq!(denied.len(), 1);
    assert_eq!(
        denied[0].get_str("reason").unwrap(),
        "guardrail:windows-proxy-only",
        "the pairing holds, so Windows' own guardrail alone decides"
    );
}

/// What a dynamic secret is declared with: its provider, known, and one
/// audience; its issuer's fields alone; and no other kind names a provider.
#[tokio::test]
async fn a_dynamic_secret_is_declared_with_its_provider_and_one_audience() {
    let app = lease_app().await;
    let t = app.seed_tenant("vaultdyn6").await;
    for (body, why) in [
        (
            json!({ "path": "db/a", "kind": "dynamic", "audiences": ["db:5432"] }),
            "no provider",
        ),
        (
            json!({ "path": "db/b", "kind": "dynamic", "provider": "mysql", "audiences": ["db:3306"] }),
            "a provider this build cannot name",
        ),
        (
            json!({ "path": "db/c", "kind": "dynamic", "provider": "postgres" }),
            "no audience",
        ),
        (
            json!({ "path": "db/d", "kind": "dynamic", "provider": "postgres",
                    "audiences": ["a:5432", "b:5432"] }),
            "two audiences",
        ),
        (
            json!({ "path": "db/e", "kind": "static", "provider": "postgres" }),
            "a stored secret naming a provider",
        ),
    ] {
        let (code, answer) = post(&app, &t, "/vault/secret", body).await;
        assert_eq!(code, 400, "{why}: {answer}");
    }
    let issuer = Issuer::made();
    let id = dynamic_secret(&app, &t, &issuer, true).await;
    // The minted credential's fields are not the issuer's to hold.
    for field in ["user", "password", "value", "host"] {
        assert_eq!(put(&app, &t, &id, field, "x").await, 400, "{field}");
    }
    let resp = app
        .client
        .patch(app.url(&format!("/api/tenant/{}/vault/secret/{id}", t.tenant_id)))
        .header("Authorization", format!("Bearer {}", t.admin.access_token))
        .json(&json!({ "audiences": ["a:5432", "b:5432"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400, "it keeps one audience");
}

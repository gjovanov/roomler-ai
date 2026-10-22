// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3b-2 — placeholder leases, the vault's half (design §8.5): a
//! session's lease in `placeholder` mode releases the value to its daemon,
//! sealed as any lease, toward an origin its secret names. The daemon swaps
//! it in at the session's egress proxy (P3b-3); the session holds an opaque
//! token.
//!
//! Driven through the lease tests' raw agent socket. What these lock is the
//! SERVER:
//!
//! - `vault.placeholder` is opt-in: empty, the default, releases no
//!   placeholder whatever the policies say, a list only to the organizations
//!   it names, and that refusal records nothing;
//! - a placeholder's audience is an origin its secret names, exactly
//!   (`guardrail:placeholder-only-named-audiences`), a refusal recorded with
//!   its request;
//! - a Windows device holds one too (P3b-5-1): `guardrail:windows-proxy-only`
//!   (decision 17) allows `placeholder` beside `proxy`, as decision 30
//!   decided, and still refuses every mode that hands a value over.

use bson::{doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::fixtures::test_app::TestApp;
use crate::vault_lease_tests::{
    TAKES_LEASES, ask, device, fresh_kek, name_audience, opened, refused, rows, seed_vault, start,
};

/// The origin the tests' secret names.
const ORIGIN: &str = "https://api.example.internal";

/// Hive and the vault on, with a KEK, releasing placeholders to the
/// organizations `placeholder` lists (`vault.placeholder`).
async fn placeholder_app(placeholder: &str) -> TestApp {
    let kek = fresh_kek();
    let placeholder = placeholder.to_string();
    TestApp::spawn_with_settings(move |s| {
        s.modules.hive = true;
        s.modules.vault = true;
        s.vault.kek = kek;
        s.vault.placeholder = placeholder;
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await
}

/// A lease request for `db/main` in `mode`, toward `audience`.
fn frame(sid: &str, fence: u64, mode: &str, audience: &str) -> Value {
    json!({ "session_id": sid, "fence": fence, "secret": "db/main", "mode": mode,
            "audience": audience })
}

/// FR-90 P3b-2 — with the organization listed, a session leases a value as
/// a placeholder toward an origin its secret names: sealed to its device and
/// opened there, as a `proxy` lease is, and its row names the mode and the
/// origin. Any other origin, a bare host and none are refused by
/// `placeholder-only-named-audiences`, recorded with the request.
#[tokio::test]
async fn a_placeholder_lease_goes_only_toward_an_origin_its_secret_names() {
    let app = placeholder_app("*").await;
    let t = app.seed_tenant("vaultph").await;
    let value = seed_vault(&app, &t).await;
    name_audience(&app, &t, "db/main", ORIGIN).await;
    let mut dev = device(&app, &t, "lease-ph", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/ph", Some("dev")).await;
    let ok = ask(&mut dev, "ph-ok", frame(&sid, fence, "placeholder", ORIGIN)).await;
    assert_eq!(opened(&dev, &ok), value);
    let leased = rows(&app, "vault_leases", doc! { "mode": "placeholder" }).await;
    assert_eq!(leased.len(), 1, "{leased:?}");
    assert_eq!(leased[0].get_str("audience").unwrap(), ORIGIN);
    for (i, audience) in [
        "https://evil.example",
        "https://api.example.internal:8443",
        "api.example.internal",
        "",
    ]
    .iter()
    .enumerate()
    {
        let answer = ask(
            &mut dev,
            &format!("ph-{i}"),
            frame(&sid, fence, "placeholder", audience),
        )
        .await;
        assert_eq!(refused(&answer), "denied", "{audience:?}: {answer}");
    }
    let denied = rows(
        &app,
        "vault_audit",
        doc! { "action": "secret:use", "decision": "deny" },
    )
    .await;
    assert_eq!(denied.len(), 4);
    for row in &denied {
        assert_eq!(
            row.get_str("reason").unwrap(),
            "guardrail:placeholder-only-named-audiences",
            "{row:?}"
        );
        assert!(row.contains_key("request"), "replayable: {row:?}");
    }
}

/// FR-90 P3b-2 — `vault.placeholder` is opt-in: empty by default, it
/// releases no placeholder to any organization whatever its policies say,
/// and a list releases only to the organizations it names. That refusal
/// records nothing, and the switch gates this mode alone: a `proxy` lease
/// in the same organization is released.
#[tokio::test]
async fn the_vault_releases_no_placeholder_unless_vault_placeholder_lists_the_organization() {
    let another = ObjectId::new().to_hex();
    for listed in ["", another.as_str()] {
        let app = placeholder_app(listed).await;
        let t = app.seed_tenant("vaultnoph").await;
        let value = seed_vault(&app, &t).await;
        name_audience(&app, &t, "db/main", ORIGIN).await;
        let mut dev = device(&app, &t, "lease-noph", "linux", TAKES_LEASES, true).await;
        let (sid, fence) = start(&app, &t, &mut dev, "/srv/noph", Some("dev")).await;
        let answer = ask(&mut dev, "n1", frame(&sid, fence, "placeholder", ORIGIN)).await;
        assert_eq!(refused(&answer), "not_served", "{listed:?}: {answer}");
        let recorded = rows(&app, "vault_audit", doc! { "action": "secret:use" }).await;
        assert!(recorded.is_empty(), "{listed:?}: {recorded:?}");
        let proxy = ask(&mut dev, "n2", frame(&sid, fence, "proxy", ORIGIN)).await;
        assert_eq!(opened(&dev, &proxy), value, "{listed:?}");
    }
}

/// FR-90 P3b-5-1 — a Windows device holds a placeholder, as decision 30
/// decided: `windows-proxy-only` (decision 17, AC13) allows `placeholder`
/// beside `proxy` there, in both of which the value stays in the daemon, and
/// still refuses a mode that hands it over, recorded with its request. The
/// placeholder's origin is held to its secret's on Windows as elsewhere.
#[tokio::test]
async fn a_windows_device_holds_a_placeholder_and_is_handed_no_value() {
    let app = placeholder_app("*").await;
    let t = app.seed_tenant("vaultphwin").await;
    let value = seed_vault(&app, &t).await;
    name_audience(&app, &t, "db/main", ORIGIN).await;
    let mut win = device(&app, &t, "lease-phwin", "windows", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut win, "C:\\src\\ph", Some("dev")).await;
    let answer = ask(&mut win, "w1", frame(&sid, fence, "placeholder", ORIGIN)).await;
    assert_eq!(opened(&win, &answer), value, "{answer}");
    let elsewhere = ask(
        &mut win,
        "w2",
        frame(&sid, fence, "placeholder", "https://evil.example"),
    )
    .await;
    assert_eq!(refused(&elsewhere), "denied", "{elsewhere}");
    let env = ask(
        &mut win,
        "w3",
        json!({ "session_id": sid, "fence": fence, "secret": "db/main", "mode": "env" }),
    )
    .await;
    assert_eq!(refused(&env), "denied", "{env}");
    let denied = rows(
        &app,
        "vault_audit",
        doc! { "action": "secret:use", "decision": "deny" },
    )
    .await;
    let mut reasons: Vec<&str> = denied
        .iter()
        .map(|row| row.get_str("reason").unwrap())
        .collect();
    reasons.sort_unstable();
    assert_eq!(
        reasons,
        [
            "guardrail:placeholder-only-named-audiences",
            "guardrail:windows-proxy-only"
        ],
        "{denied:?}"
    );
}

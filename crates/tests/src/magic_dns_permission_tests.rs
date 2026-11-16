// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! An org's MagicDNS settings are changed under `MANAGE_AGENTS`, the bit its
//! overlay ACL and tunnel policies sit behind, and read by any member.
//!
//! Every overlay node of the org applies them: its resolver answers for the
//! MagicDNS domain and forwards every other name to the upstream resolvers, and
//! under an exit node the host's whole DNS goes there. Changing them is a
//! device-management action:
//!
//! - `PUT …/magic-dns` decides the bit first, so a member without it gets one
//!   403 `forbidden` whatever the body says, never `not_a_member`, and nothing
//!   they send is stored; someone outside the org still gets `not_a_member`.
//!   The settings are the org's own, with no id in the path, so there is no
//!   foreign id to resolve: the outsider is the cross-org case;
//! - `GET …/magic-dns` stays any member's. Every node of the org receives the
//!   domain and the resolvers anyway, and a member reaches peers by
//!   `<node>.<domain>`.

use roomler_ai_db::models::role::permissions::MANAGE_AGENTS;
use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::tunnel_policy_permission_tests::{assert_refused, call, grant_role, person_with_role};

fn magic_dns(seeded: &SeededTenant) -> String {
    format!("/api/tenant/{}/magic-dns", seeded.tenant_id)
}

fn settings(domain: Option<&str>, nameservers: &[&str]) -> Value {
    json!({ "magic_dns_domain": domain, "magic_dns_nameservers": nameservers })
}

/// The owner sets the org's MagicDNS; returns what the server answered.
async fn owner_sets(app: &TestApp, seeded: &SeededTenant, body: Value) -> Value {
    let (status, stored) = call(
        app,
        "PUT",
        &magic_dns(seeded),
        &seeded.admin.access_token,
        Some(body),
    )
    .await;
    assert_eq!(status, 200, "the owner sets MagicDNS: {stored}");
    stored
}

/// A plain member, and a moderator (who holds `REMOTE_CONTROL` and so sees the
/// Network pages), may not change the org's MagicDNS: not its domain, not its
/// upstream resolvers, and not by switching it off. Nothing they send is
/// stored.
#[tokio::test]
async fn a_member_without_manage_agents_changes_no_magic_dns() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("mdnsperm-write").await;
    let moderator = person_with_role(&app, &seeded, "mod", "moderator").await;
    let before = owner_sets(
        &app,
        &seeded,
        settings(Some("corp.example"), &["192.0.2.53"]),
    )
    .await;

    for who in [&seeded.member, &moderator] {
        for body in [
            settings(Some("their.example"), &["203.0.113.53"]),
            settings(None, &["203.0.113.53"]),
            settings(None, &[]),
        ] {
            let (s, answer) = call(
                &app,
                "PUT",
                &magic_dns(&seeded),
                &who.access_token,
                Some(body.clone()),
            )
            .await;
            assert_refused(&format!("PUT {body}"), s, &answer);
        }
    }

    // Nothing they sent was stored.
    let (s, now) = call(
        &app,
        "GET",
        &magic_dns(&seeded),
        &seeded.admin.access_token,
        None,
    )
    .await;
    assert_eq!(s, 200, "{now}");
    assert_eq!(now, before, "the org's MagicDNS is as the owner set it");
}

/// The bit is decided before the body is read: a member without it gets the
/// same 403 for a domain the route would refuse 400, as for any other.
/// Someone outside the org is told just that, on both routes.
#[tokio::test]
async fn the_bit_is_decided_before_the_body_is_read() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("mdnsperm-order").await;
    let other = app.seed_tenant("mdnsperm-order-b").await;
    let member = &seeded.member.access_token;

    for body in [
        // Characters a domain may not hold.
        settings(Some("bad domain!"), &[]),
        // A zone that would shadow the product's own infrastructure names.
        settings(Some("api.roomler.ai"), &[]),
    ] {
        let (s, answer) = call(&app, "PUT", &magic_dns(&seeded), member, Some(body.clone())).await;
        assert_refused(&format!("PUT {body}"), s, &answer);
    }

    // Another org's owner holds MANAGE_AGENTS, in their own org only.
    let outsider = &other.admin.access_token;
    for (method, body) in [
        ("GET", None),
        ("PUT", Some(settings(Some("their.example"), &[]))),
    ] {
        let (s, answer) = call(&app, method, &magic_dns(&seeded), outsider, body).await;
        assert_eq!(s, 403, "{method}: {answer}");
        assert_eq!(answer["error"], "not_a_member", "{method}: {answer}");
    }
}

/// The people who manage the org's devices change its MagicDNS: a member
/// holding MANAGE_AGENTS through a role of the org's own making, and a member
/// holding the org's managed `admin` role.
#[tokio::test]
async fn holders_of_manage_agents_change_magic_dns() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("mdnsperm-holder").await;
    let org_admin = person_with_role(&app, &seeded, "orgadmin", "admin").await;
    grant_role(&app, &seeded, &seeded.member, MANAGE_AGENTS).await;

    for (who, domain) in [
        (&seeded.member, "devices.example"),
        (&org_admin, "admins.example"),
    ] {
        let (s, stored) = call(
            &app,
            "PUT",
            &magic_dns(&seeded),
            &who.access_token,
            Some(settings(Some(domain), &["192.0.2.53"])),
        )
        .await;
        assert_eq!(s, 200, "a holder changes MagicDNS: {stored}");
        assert_eq!(stored["magic_dns_domain"], domain, "{stored}");

        let (s, read) = call(
            &app,
            "GET",
            &magic_dns(&seeded),
            &seeded.admin.access_token,
            None,
        )
        .await;
        assert_eq!(s, 200, "{read}");
        assert_eq!(read, stored, "the change is what the org now has");
    }
}

/// Any member reads the settings: a plain member and a guest alike. Every node
/// of the org receives them anyway, and a member reaches peers by name in the
/// MagicDNS domain.
#[tokio::test]
async fn any_member_reads_the_magic_dns_settings() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("mdnsperm-read").await;
    let guest = person_with_role(&app, &seeded, "guest", "guest").await;
    let stored = owner_sets(
        &app,
        &seeded,
        settings(Some("corp.example"), &["192.0.2.53"]),
    )
    .await;

    for who in [&seeded.member, &guest] {
        let (s, read) = call(&app, "GET", &magic_dns(&seeded), &who.access_token, None).await;
        assert_eq!(s, 200, "a member reads MagicDNS: {read}");
        assert_eq!(read, stored, "{read}");
    }
}

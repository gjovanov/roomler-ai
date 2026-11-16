// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Archiving an organization.
//!
//! `Tenant.is_archived` existed from the beginning and nothing read it, so
//! "retire an org" had no meaning and a throwaway org was permanent — which
//! is what blocked the multi-org refusal field test (docs/multi-org.md §12).
//!
//! It means this now: **an archived org stops acting and keeps everything it
//! knows.** These tests pin the five effects and, just as importantly, the
//! things archiving must NOT do.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::{Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::fixtures::test_app::TestApp;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

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

    // A token minted while the org was open: the mint routes refuse an
    // archived org themselves now, so this is the one the enroll route has to.
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

/// What the daemon route says, so a person meets one sentence at every door.
const ARCHIVED: &str = "This organization is archived and accepts no device enrollments";

/// Mint a tunnel-enrollment token, answering the route's status and body.
async fn tunnel_enroll_token(app: &TestApp, tenant_id: &str, admin_token: &str) -> (u16, Value) {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{tenant_id}/tunnel-client/enroll-token"),
            admin_token,
        )
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `POST /api/tunnel-client/enroll`, answering the route's status and body.
async fn enroll_tunnel_client(app: &TestApp, token: &str, machine: &str) -> (u16, Value) {
    let resp = app
        .client
        .post(app.url("/api/tunnel-client/enroll"))
        .json(&json!({
            "enrollment_token": token,
            "machine_id": machine,
            "machine_name": machine,
            "os": "linux",
            "client_version": "0.3.0-test",
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn tunnel_clients_in(app: &TestApp, tenant: ObjectId, filter: Document) -> u64 {
    let mut filter = filter;
    filter.insert("tenant_id", tenant);
    app.db
        .collection::<Document>("tunnel_clients")
        .count_documents(filter)
        .await
        .unwrap()
}

/// #3042 — "an archived org accepts no devices" held for one kind of device.
/// A `roomler` client still enrolled, both as a new row and back into its old
/// one, on a token minted before or after the archive. Every door says the
/// same sentence now: the enroll route, and the three routes that mint what it
/// consumes. Refused at the mint too, because a token for a closed org is a
/// failure deferred to someone's terminal.
///
/// Negative control: with `org_accepts_enrollment` taken back out of
/// `enroll_tunnel_client` this fails at "a token minted before the archive"
/// with a 200; out of each mint route, at that route's own line.
#[tokio::test]
async fn an_archived_org_refuses_a_tunnel_client_and_every_token_for_one() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("closeddoor").await;
    let admin = org.admin.access_token.clone();
    let tid = ObjectId::parse_str(&org.tenant_id).unwrap();

    // While it is open: a client enrolled, and a token kept for later.
    let (status, et) = tunnel_enroll_token(&app, &org.tenant_id, &admin).await;
    assert_eq!(status, 200, "control: the mint answers an open org");
    let first = et["enrollment_token"].as_str().unwrap();
    let (status, _) = enroll_tunnel_client(&app, first, "mach-door-old").await;
    assert_eq!(status, 200, "control: an open org takes a client");
    let (_, et) = tunnel_enroll_token(&app, &org.tenant_id, &admin).await;
    let kept = et["enrollment_token"].as_str().unwrap().to_string();

    let resp = app
        .auth_post(&format!("/api/tenant/{}/archive", org.tenant_id), &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let rows_after_archive = tunnel_clients_in(&app, tid, doc! {}).await;

    // A token minted before the archive, a machine it never saw.
    let (status, body) = enroll_tunnel_client(&app, &kept, "mach-door-new").await;
    assert_eq!(status, 403, "a token minted before the archive: {body}");
    assert_eq!(body["message"], ARCHIVED, "{body}");
    // The same token, back into the row the archive retired.
    let (status, body) = enroll_tunnel_client(&app, &kept, "mach-door-old").await;
    assert_eq!(status, 403, "back into its old row: {body}");
    assert_eq!(body["message"], ARCHIVED, "{body}");
    assert_eq!(
        tunnel_clients_in(&app, tid, doc! {}).await,
        rows_after_archive,
        "a refused enrollment writes no row"
    );

    // Every mint route refuses, in the same words.
    let (status, body) = tunnel_enroll_token(&app, &org.tenant_id, &admin).await;
    assert_eq!(status, 403, "the tunnel mint: {body}");
    assert_eq!(body["message"], ARCHIVED, "{body}");
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", org.tenant_id),
            &admin,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403, "the daemon mint");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["message"], ARCHIVED, "{body}");
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-key", org.tenant_id),
            &admin,
        )
        .json(&json!({ "label": "a key" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403, "the ephemeral key mint");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["message"], ARCHIVED, "{body}");

    // Restored: the refusal was the flag's, not the token's. The same token
    // that was refused above still works, so a refusal burned nothing.
    let resp = app
        .auth_post(&format!("/api/tenant/{}/unarchive", org.tenant_id), &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let (status, body) = enroll_tunnel_client(&app, &kept, "mach-door-new").await;
    assert_eq!(status, 200, "a restored org takes the same token: {body}");
    let (status, _) = tunnel_enroll_token(&app, &org.tenant_id, &admin).await;
    assert_eq!(status, 200, "and mints again");
}

/// #3042 — an archive revokes every device's enrollment, and a tunnel client
/// is a device: its token is good for a year, so the row is the only thing
/// that can withdraw it. Left live, it reconnected with no objection and took a
/// fresh address in the org's mesh, and its renewals kept working.
///
/// Negative control: with the tombstone taken back out of the network
/// module's `tenant_archived` this fails at `devices_revoked`; with the join's
/// `removed` check taken out, at "a revoked client takes no new lease".
#[tokio::test]
async fn archiving_revokes_the_orgs_tunnel_clients() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("clientgone").await;
    let admin = org.admin.access_token.clone();
    let tid = ObjectId::parse_str(&org.tenant_id).unwrap();
    let nodes = app.db.collection::<Document>("overlay_nodes");
    let live_nodes = doc! { "tenant_id": tid, "deleted_at": null };

    let (_, et) = tunnel_enroll_token(&app, &org.tenant_id, &admin).await;
    let (status, enrolled) = enroll_tunnel_client(
        &app,
        et["enrollment_token"].as_str().unwrap(),
        "mach-client-gone",
    )
    .await;
    assert_eq!(status, 200);
    let token = enrolled["tunnel_client_token"]
        .as_str()
        .unwrap()
        .to_string();
    let url = format!(
        "ws://{}/ws?token={}&role=tunnel-client",
        app.addr,
        token
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    );
    let (mut ws, _) = connect_async(&url).await.expect("tunnel-client ws");
    let join = Message::Text(
        json!({
            "t": "rc:overlay.join",
            "wg_public_key": BASE64.encode([21u8; 32]),
            "mtu": 1280,
            "endpoints": ["203.0.113.21:51820"],
            "advertised_routes": [],
        })
        .to_string()
        .into(),
    );
    ws.send(join.clone()).await.expect("send join");
    assert!(
        netmap_within(&mut ws, Duration::from_secs(10)).await,
        "control: a client of an open org joins its mesh"
    );

    let resp = app
        .auth_post(&format!("/api/tenant/{}/archive", org.tenant_id), &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["devices_revoked"].as_u64().unwrap(),
        1,
        "the tunnel client is one of the org's devices: {body}"
    );
    assert_eq!(body["nodes_released"].as_u64().unwrap(), 1, "{body}");
    assert_eq!(
        tunnel_clients_in(&app, tid, doc! { "deleted_at": null }).await,
        0,
        "no live tunnel client is left in an archived org"
    );

    // On the socket it still holds, for as long as the revocation poll leaves
    // it: no address.
    ws.send(join).await.expect("send join");
    assert!(
        !netmap_within(&mut ws, Duration::from_secs(3)).await,
        "a revoked client takes no new lease"
    );
    assert_eq!(nodes.count_documents(live_nodes).await.unwrap(), 0);

    // Its credential is dead at every door that checks one.
    let (mut again, _) = connect_async(&url).await.expect("the upgrade itself");
    let mut told = None;
    while let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(5), again.next()).await {
        if let Message::Text(t) = msg
            && let Ok(v) = serde_json::from_str::<Value>(&t)
            && v["t"] == "rc:tunnel.revoked"
        {
            told = Some(v);
            break;
        }
    }
    assert!(told.is_some(), "a reconnect is told it was revoked");
    let resp = app
        .client
        .post(app.url("/api/tunnel-client/renew"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        401,
        "no renewal for a revoked client"
    );

    // A restore does not bring it back: "Devices re-enroll".
    let resp = app
        .auth_post(&format!("/api/tenant/{}/unarchive", org.tenant_id), &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        tunnel_clients_in(&app, tid, doc! { "deleted_at": null }).await,
        0,
        "a restored org does not resurrect a revoked enrollment"
    );
    let (_, et) = tunnel_enroll_token(&app, &org.tenant_id, &admin).await;
    let (status, _) = enroll_tunnel_client(
        &app,
        et["enrollment_token"].as_str().unwrap(),
        "mach-client-gone",
    )
    .await;
    assert_eq!(status, 200, "it re-enrolls into the restored org");
}

/// A row the archive's cascade did not get to, whether it raced the teardown or
/// the org was archived before the cascade covered tunnel clients: its row reads
/// live, and the org's flag is what must stop it. The flag is flipped directly,
/// so the cascade has not run and the row stays live.
///
/// Negative control: with the `is_archived` check taken back out of
/// `handle_overlay_join` this fails at "an archived org's mesh takes no member".
#[tokio::test]
async fn an_archived_orgs_mesh_takes_no_member_even_on_a_live_row() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("flagonly").await;
    let admin = org.admin.access_token.clone();
    let tid = ObjectId::parse_str(&org.tenant_id).unwrap();
    let nodes = app.db.collection::<Document>("overlay_nodes");
    let live_nodes = doc! { "tenant_id": tid, "deleted_at": null };

    let (_, et) = tunnel_enroll_token(&app, &org.tenant_id, &admin).await;
    let (status, enrolled) = enroll_tunnel_client(
        &app,
        et["enrollment_token"].as_str().unwrap(),
        "mach-flag-only",
    )
    .await;
    assert_eq!(status, 200);
    let url = format!(
        "ws://{}/ws?token={}&role=tunnel-client",
        app.addr,
        enrolled["tunnel_client_token"]
            .as_str()
            .unwrap()
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    );
    let (mut ws, _) = connect_async(&url).await.expect("tunnel-client ws");
    let join = Message::Text(
        json!({
            "t": "rc:overlay.join",
            "wg_public_key": BASE64.encode([23u8; 32]),
            "mtu": 1280,
            "endpoints": ["203.0.113.23:51820"],
            "advertised_routes": [],
        })
        .to_string()
        .into(),
    );

    // The positive control: before the flag, this client on this socket joins.
    ws.send(join.clone()).await.expect("send join");
    assert!(
        netmap_within(&mut ws, Duration::from_secs(10)).await,
        "control: a client of an open org joins its mesh"
    );
    nodes
        .update_many(
            live_nodes.clone(),
            doc! { "$set": { "deleted_at": bson::DateTime::now() } },
        )
        .await
        .unwrap();

    roomler_ai_services::dao::tenant::TenantDao::new(&app.db)
        .set_archived(tid, true)
        .await
        .expect("flip the flag");
    ws.send(join).await.expect("send join");
    assert!(
        !netmap_within(&mut ws, Duration::from_secs(3)).await,
        "an archived org's mesh takes no member"
    );
    assert_eq!(nodes.count_documents(live_nodes).await.unwrap(), 0);
}

/// A netmap arrives within `wait`.
async fn netmap_within(ws: &mut Ws, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&txt)
                    && v["t"] == "rc:overlay.netmap"
                {
                    return true;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return false,
        }
    }
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

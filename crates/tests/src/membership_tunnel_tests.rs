// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! A person's tunnel clients end with their org membership.
//!
//! When someone stops being a member of an org (an admin removes them, or
//! they leave), the tunnel clients they own there are removed the way a
//! device manager's `DELETE …/tunnel-client/{id}` removes one: the row is
//! tombstoned, then its overlay node is released (the peers hear `removes`,
//! the address goes back to the pool), and its socket closes at the next
//! revocation poll. An enrollment token the person minted before they left
//! enrolls nothing afterwards. Clients that outlived a membership before
//! this, or that a failed removal left, are ended by an operator step,
//! `POST /api/admin/tunnel-clients/prune`. Devices the person enrolled are
//! not touched.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::{Bson, Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use roomler_ai_remote_control::models::{NodeRef, OsKind};
use roomler_ai_services::dao::overlay_network::OverlayNetworkDao;
use roomler_ai_services::dao::overlay_node::{NewOverlayNode, OverlayNodeDao};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::fixtures::seed::{SeededTenant, SeededUser};
use crate::fixtures::test_app::TestApp;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// What the enroll route answers for a token whose person left the org.
const LEFT_THE_ORG: &str = "This enrollment token was issued to someone who is no longer a \
     member of this organization, so it cannot enroll a tunnel client. Ask a member of the \
     organization for a new enrollment token.";

const PRUNE: &str = "/api/admin/tunnel-clients/prune";

fn oid(hex: &str) -> ObjectId {
    ObjectId::parse_str(hex).unwrap()
}

fn tid(org: &SeededTenant) -> ObjectId {
    oid(&org.tenant_id)
}

async fn call(
    app: &TestApp,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
    let mut req = app
        .client
        .request(method, app.url(path))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// A call to a public route (no credential of its own), as an installer
/// makes it.
async fn post_public(app: &TestApp, path: &str, body: Value) -> (u16, Value) {
    let resp = app
        .client
        .post(app.url(path))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// A plain member of `org` (the `member` role), added by its admin.
async fn add_member(app: &TestApp, org: &SeededTenant, name: &str) -> SeededUser {
    let user = app
        .register_user(
            &format!("{name}@{}.test", org.tenant_slug),
            &format!("{}_{name}", org.tenant_slug),
            name,
            "Member123!",
            None,
            None,
        )
        .await;
    readd(app, org, &user).await;
    user
}

/// Make `user` a member of `org` (again), through the admin route.
async fn readd(app: &TestApp, org: &SeededTenant, user: &SeededUser) {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/tenant/{}/member", org.tenant_id),
        &org.admin.access_token,
        Some(json!({ "user_id": user.id })),
    )
    .await;
    assert_eq!(status, 201, "add {} to the org: {body}", user.username);
}

/// The admin removes `user` from `org`: the route the hook runs behind.
async fn remove(app: &TestApp, org: &SeededTenant, user: &SeededUser) {
    let (status, body) = call(
        app,
        "DELETE",
        &format!("/api/tenant/{}/member/{}", org.tenant_id, user.id),
        &org.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "remove {}: {body}", user.username);
}

/// End a membership the way removals did before the hook existed: the row
/// goes, nothing else runs.
async fn end_membership_directly(app: &TestApp, org: &SeededTenant, user: &SeededUser) {
    let gone = app
        .db
        .collection::<Document>("tenant_members")
        .delete_one(doc! { "tenant_id": tid(org), "user_id": oid(&user.id) })
        .await
        .unwrap();
    assert_eq!(gone.deleted_count, 1);
}

/// Mint a tunnel enrollment token as `who`: `(token, jti)`.
async fn mint(app: &TestApp, org: &SeededTenant, who: &SeededUser) -> (String, String) {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/tenant/{}/tunnel-client/enroll-token", org.tenant_id),
        &who.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "mint a tunnel enrollment token: {body}");
    (
        body["enrollment_token"].as_str().unwrap().to_string(),
        body["jti"].as_str().unwrap().to_string(),
    )
}

/// `POST /api/tunnel-client/enroll`, the body `roomler enroll` sends.
async fn enroll(app: &TestApp, token: &str, machine_id: &str) -> (u16, Value) {
    let resp = app
        .client
        .post(app.url("/api/tunnel-client/enroll"))
        .json(&json!({
            "enrollment_token": token,
            "machine_id": machine_id,
            "machine_name": machine_id,
            "os": "linux",
            "client_version": "0.4.0-test",
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Enroll a tunnel client for `who`: `(client_id, client_token)`.
async fn enroll_as(
    app: &TestApp,
    org: &SeededTenant,
    who: &SeededUser,
    machine_id: &str,
) -> (String, String) {
    let (token, _) = mint(app, org, who).await;
    let (status, body) = enroll(app, &token, machine_id).await;
    assert_eq!(status, 200, "enroll {machine_id}: {body}");
    (
        body["tunnel_client_id"].as_str().unwrap().to_string(),
        body["tunnel_client_token"].as_str().unwrap().to_string(),
    )
}

/// The used-token ledger's record of a token's one use, if it has one.
async fn ledger_use(app: &TestApp, jti: &str) -> Option<Document> {
    app.db
        .collection::<Document>("used_tokens")
        .find_one(doc! { "_id": jti })
        .await
        .unwrap()
}

async fn client_row(app: &TestApp, client_id: &str) -> Document {
    app.db
        .collection::<Document>("tunnel_clients")
        .find_one(doc! { "_id": oid(client_id) })
        .await
        .unwrap()
        .expect("the tunnel-client row")
}

/// Every tunnel-client row of the org, removed ones included.
async fn client_rows(app: &TestApp, org: &SeededTenant) -> u64 {
    app.db
        .collection::<Document>("tunnel_clients")
        .count_documents(doc! { "tenant_id": tid(org) })
        .await
        .unwrap()
}

fn removed_at(row: &Document) -> Option<bson::DateTime> {
    match row.get("deleted_at") {
        Some(Bson::DateTime(at)) => Some(*at),
        _ => None,
    }
}

async fn is_removed(app: &TestApp, client_id: &str) -> bool {
    removed_at(&client_row(app, client_id).await).is_some()
}

/// The live overlay node of a tunnel client, if it holds one.
async fn live_node_of(app: &TestApp, org: &SeededTenant, client_id: &str) -> Option<Document> {
    app.db
        .collection::<Document>("overlay_nodes")
        .find_one(doc! {
            "tenant_id": tid(org),
            "node_ref.kind": "tunnel_client",
            "node_ref.id": oid(client_id),
            "deleted_at": Bson::Null,
        })
        .await
        .unwrap()
}

async fn live_nodes(app: &TestApp, org: &SeededTenant) -> u64 {
    app.db
        .collection::<Document>("overlay_nodes")
        .count_documents(doc! { "tenant_id": tid(org), "deleted_at": Bson::Null })
        .await
        .unwrap()
}

/// The org's pooled host numbers: an address a release gave back.
async fn pooled_hosts(app: &TestApp, org: &SeededTenant) -> usize {
    OverlayNetworkDao::new(&app.db)
        .find_for_tenant(tid(org))
        .await
        .unwrap()
        .expect("the org's overlay network")
        .free_hosts
        .len()
}

/// A live overlay lease for a tunnel client, as `handle_overlay_join` would
/// leave it (for the tests that do not drive a join).
async fn lease(app: &TestApp, org: &SeededTenant, client_id: &str, machine_id: &str) {
    let tenant_id = tid(org);
    let network_id = OverlayNetworkDao::new(&app.db)
        .get_or_create(tenant_id)
        .await
        .unwrap()
        .id
        .unwrap();
    OverlayNodeDao::new(&app.db)
        .create(NewOverlayNode {
            tenant_id,
            node_ref: NodeRef::TunnelClient {
                tunnel_client_id: oid(client_id),
            },
            network_id,
            machine_id: machine_id.to_string(),
            name: machine_id.to_string(),
            overlay_ip: "100.64.0.9".to_string(),
            wg_public_key: format!("pk-{machine_id}"),
            key_epoch: 0,
            endpoints: vec![],
            supports_quic: false,
            supports_relay_single: false,
            supports_derp: false,
            supports_forced_derp: false,
            supports_server_relay_strategy: false,
            supports_derp_floor: false,
            supports_overlay_echo: false,
            supports_org_relay: false,
            advertised_routes: vec![],
            route_offer_withheld: None,
        })
        .await
        .unwrap();
}

fn ws_url(app: &TestApp, client_token: &str) -> String {
    format!(
        "ws://{}/ws?token={}&role=tunnel-client",
        app.addr,
        client_token
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    )
}

fn join_msg(seed: u8) -> Message {
    Message::Text(
        json!({
            "t": "rc:overlay.join",
            "wg_public_key": BASE64.encode([seed; 32]),
            "mtu": 1280,
            "endpoints": [format!("203.0.113.{seed}:51820")],
            "advertised_routes": [],
        })
        .to_string()
        .into(),
    )
}

/// The next frame of type `want` within `wait`; `None` if the socket ends or
/// the time runs out first.
async fn recv_t(ws: &mut Ws, want: &str, wait: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&txt)
                    && v["t"] == want
                {
                    return Some(v);
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return None,
        }
    }
}

/// Open a tunnel client's socket and join the org's mesh with it.
async fn join_mesh(app: &TestApp, client_token: &str, seed: u8) -> Ws {
    let (mut ws, _) = connect_async(&ws_url(app, client_token))
        .await
        .expect("the tunnel-client socket opens");
    ws.send(join_msg(seed)).await.expect("send the join");
    assert!(
        recv_t(&mut ws, "rc:overlay.netmap", Duration::from_secs(10))
            .await
            .is_some(),
        "a netmap answers the join"
    );
    ws
}

/// Does a netmap delta naming `node_id` among its removes arrive in `wait`?
async fn hears_removes(ws: &mut Ws, node_id: ObjectId, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(delta) = recv_t(ws, "rc:overlay.netmap_delta", left).await else {
            return false;
        };
        if delta["removes"].to_string().contains(&node_id.to_hex()) {
            return true;
        }
    }
}

async fn renew(app: &TestApp, client_token: &str) -> u16 {
    app.client
        .post(app.url("/api/tunnel-client/renew"))
        .bearer_auth(client_token)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// Make every write that sets `deleted_at` on `collection` fail, so a
/// removal's tombstone or release fails as a database fault would.
async fn refuse_tombstones(app: &TestApp, collection: &str) {
    app.db
        .run_command(doc! { "collMod": collection, "validator": { "deleted_at": Bson::Null } })
        .await
        .unwrap();
}

async fn accept_tombstones(app: &TestApp, collection: &str) {
    app.db
        .run_command(doc! { "collMod": collection, "validator": {} })
        .await
        .unwrap();
}

async fn room_rows(app: &TestApp, org: &SeededTenant, user: &SeededUser) -> u64 {
    app.db
        .collection::<Document>("room_members")
        .count_documents(doc! { "tenant_id": tid(org), "user_id": oid(&user.id) })
        .await
        .unwrap()
}

fn operator_token(app: &TestApp, operator: ObjectId, slug: &str) -> String {
    app.state
        .auth
        .generate_tokens(
            operator,
            &format!("operator@{slug}.test"),
            &format!("{slug}_operator"),
        )
        .unwrap()
        .access_token
}

/// The client ids a prune answer lists.
fn listed_clients(body: &Value) -> Vec<String> {
    body["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .flat_map(|item| item["clients"].as_array().cloned().unwrap_or_default())
                .filter_map(|c| c["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Removing a member ends their tunnel clients in that org the way a manual
/// delete does: the row is tombstoned, the node released (a peer hears its
/// `removes`, the address is pooled), a reconnect is told it was revoked, a
/// join on the socket it still holds gets no lease, and its token renews
/// nothing. A second run of the cascade changes nothing. Another member's
/// client and node, the admin's client, and a device the removed member
/// enrolled are untouched.
#[tokio::test]
async fn a_members_tunnel_clients_end_with_their_membership() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("tcend").await;
    let member = &org.member;
    let other = add_member(&app, &org, "other").await;

    let (mine, mine_token) = enroll_as(&app, &org, member, "mach-tcend-mine").await;
    let (theirs, theirs_token) = enroll_as(&app, &org, &other, "mach-tcend-other").await;
    let (admins, _) = enroll_as(&app, &org, &org.admin, "mach-tcend-admin").await;
    let mut mine_ws = join_mesh(&app, &mine_token, 31).await;
    let mut theirs_ws = join_mesh(&app, &theirs_token, 32).await;
    let my_node = live_node_of(&app, &org, &mine)
        .await
        .expect("the member's client holds a lease")
        .get_object_id("_id")
        .unwrap();
    assert!(live_node_of(&app, &org, &theirs).await.is_some());
    assert_eq!(pooled_hosts(&app, &org).await, 0);
    // A device the member enrolled: out of this, and it stays.
    let device = app
        .state
        .fleet()
        .agents
        .create(
            tid(&org),
            oid(&member.id),
            "member's device".to_string(),
            "mach-tcend-device".to_string(),
            OsKind::Linux,
            "0.4.0-test".to_string(),
            String::new(),
            None,
        )
        .await
        .unwrap()
        .id
        .unwrap();

    remove(&app, &org, member).await;

    let row = client_row(&app, &mine).await;
    let removed = removed_at(&row).expect("the member's tunnel client is removed with them");
    assert_eq!(
        row.get_i32("token_gen")
            .map(i64::from)
            .unwrap_or_else(|_| row.get_i64("token_gen").unwrap()),
        1,
        "every token it held is refused from here"
    );
    assert!(
        live_node_of(&app, &org, &mine).await.is_none(),
        "its overlay node is released"
    );
    assert_eq!(pooled_hosts(&app, &org).await, 1, "its address is pooled");
    assert!(
        hears_removes(&mut theirs_ws, my_node, Duration::from_secs(10)).await,
        "a peer is told to drop it"
    );

    // On the socket it still holds, for as long as the revocation poll leaves
    // it: no address.
    mine_ws.send(join_msg(31)).await.expect("send the join");
    assert!(
        recv_t(&mut mine_ws, "rc:overlay.netmap", Duration::from_secs(3))
            .await
            .is_none(),
        "a removed client takes no new lease"
    );
    // A reconnect is told it was revoked, and the token renews nothing.
    let (mut again, _) = connect_async(&ws_url(&app, &mine_token))
        .await
        .expect("the upgrade itself");
    assert!(
        recv_t(&mut again, "rc:tunnel.revoked", Duration::from_secs(5))
            .await
            .is_some(),
        "a reconnect is told it was revoked"
    );
    assert_eq!(renew(&app, &mine_token).await, 401, "no renewal");

    // Untouched: another member's client and node, the admin's client, and
    // the device the removed member enrolled.
    assert!(!is_removed(&app, &theirs).await);
    assert!(live_node_of(&app, &org, &theirs).await.is_some());
    assert!(!is_removed(&app, &admins).await);
    assert_eq!(renew(&app, &theirs_token).await, 200);
    let device_row = app
        .db
        .collection::<Document>("agents")
        .find_one(doc! { "_id": device })
        .await
        .unwrap()
        .expect("the device row");
    assert!(
        removed_at(&device_row).is_none(),
        "a device the member enrolled is not part of this"
    );

    // The cascade run again finds nothing left.
    let nodes = live_nodes(&app, &org).await;
    app.state
        .core
        .hooks
        .org_member_removed(tid(&org), oid(&member.id))
        .await
        .expect("a second run of the cascade");
    assert_eq!(
        removed_at(&client_row(&app, &mine).await),
        Some(removed),
        "the removal time is not re-stamped"
    );
    assert_eq!(live_nodes(&app, &org).await, nodes);
    assert_eq!(pooled_hosts(&app, &org).await, 1);

    // Leaving the org yourself is the same end.
    let leaver = add_member(&app, &org, "leaver").await;
    let (left, _) = enroll_as(&app, &org, &leaver, "mach-tcend-leaver").await;
    let (status, body) = call(
        &app,
        "DELETE",
        &format!("/api/tenant/{}/member/{}", org.tenant_id, leaver.id),
        &leaver.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        is_removed(&app, &left).await,
        "leaving ends your own clients"
    );
}

/// An enrollment token acts for the person it was minted for, so once they
/// are no longer a member it enrolls nothing: not a new machine, not their
/// own (removed) client, and not through a token a pairing delivered (its
/// subject is the approver). Each refusal comes before the token is spent
/// and writes nothing. A member's token still enrolls, and the person added
/// back enrolls with a fresh one.
#[tokio::test]
async fn a_token_minted_before_a_removal_enrolls_nothing_after_it() {
    let app = TestApp::spawn_with_settings(|s| s.pairing.poll_interval_secs = 0).await;
    let org = app.seed_tenant("tcleft").await;
    let member = &org.member;

    let (own, _) = enroll_as(&app, &org, member, "mach-left-own").await;
    let (for_new, new_jti) = mint(&app, &org, member).await;
    let (for_own, own_jti) = mint(&app, &org, member).await;
    // A tunnel pairing the member approves: its token's subject is them.
    let (status, started) = post_public(
        &app,
        "/api/pair/start",
        json!({
            "machine_name": "paired laptop",
            "os": "linux",
            "arch": "x86_64",
            "role": "tunnel",
            "client": "wizard",
            "client_version": "0.4.180",
        }),
    )
    .await;
    assert_eq!(status, 200, "{started}");
    let (status, approved) = call(
        &app,
        "POST",
        "/api/pair/approve",
        &member.access_token,
        Some(json!({ "user_code": started["user_code"], "tenant_id": org.tenant_id })),
    )
    .await;
    assert_eq!(status, 200, "{approved}");
    let (_, polled) = post_public(
        &app,
        "/api/pair/poll",
        json!({ "device_code": started["device_code"] }),
    )
    .await;
    assert_eq!(polled["status"], "approved", "{polled}");
    let paired = polled["tokens"][0].as_str().unwrap().to_string();

    remove(&app, &org, member).await;
    let rows = client_rows(&app, &org).await;

    for (token, machine, what) in [
        (&for_new, "mach-left-new", "a new machine"),
        (&for_own, "mach-left-own", "their own machine"),
        (&paired, "mach-left-paired", "a token a pairing delivered"),
    ] {
        let (status, body) = enroll(&app, token, machine).await;
        assert_eq!(status, 403, "{what}: {body}");
        assert_eq!(body["error"], "forbidden", "{what}: {body}");
        assert_eq!(body["message"], LEFT_THE_ORG, "{what}: {body}");
    }
    assert!(
        ledger_use(&app, &new_jti).await.is_none(),
        "a refused enrollment leaves its token unspent"
    );
    assert!(ledger_use(&app, &own_jti).await.is_none());
    assert_eq!(
        client_rows(&app, &org).await,
        rows,
        "a refused enrollment writes no row"
    );
    assert!(
        is_removed(&app, &own).await,
        "their own client stays removed"
    );

    // A member's token enrolls as before.
    enroll_as(&app, &org, &org.admin, "mach-left-admin").await;
    // Added back, the person enrolls a new machine with a fresh token.
    readd(&app, &org, member).await;
    enroll_as(&app, &org, member, "mach-left-back").await;

    // A membership that ended before the hook existed left a LIVE client.
    // The owner re-enrolling their own live client is the one re-enrollment
    // that reads no permission, so the membership is what refuses it.
    let stale = add_member(&app, &org, "stale").await;
    let (stale_client, _) = enroll_as(&app, &org, &stale, "mach-left-stale").await;
    let (stale_token, stale_jti) = mint(&app, &org, &stale).await;
    end_membership_directly(&app, &org, &stale).await;
    let before = client_row(&app, &stale_client).await;
    let (status, body) = enroll(&app, &stale_token, "mach-left-stale").await;
    assert_eq!(status, 403, "their own live client: {body}");
    assert_eq!(body["message"], LEFT_THE_ORG, "{body}");
    assert!(ledger_use(&app, &stale_jti).await.is_none());
    assert_eq!(
        client_row(&app, &stale_client).await,
        before,
        "nothing is written: the client is the operator step's"
    );
}

/// The operator step for the tunnel clients that outlived a membership
/// before the hook existed: platform operators only, a dry run that counts
/// and lists, a real run that ends each pair through the removal's own
/// cascade (the client and its lease), a person who is a member again left
/// alone, and nothing found on a second run.
#[tokio::test]
async fn the_operator_step_ends_tunnel_clients_without_a_membership() {
    let operator = ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(operator.to_hex());
    })
    .await;
    let org = app.seed_tenant("tcprune").await;
    let back = add_member(&app, &org, "back").await;
    let stays = add_member(&app, &org, "stays").await;
    let (gone_client, _) = enroll_as(&app, &org, &org.member, "mach-prune-gone").await;
    lease(&app, &org, &gone_client, "mach-prune-gone").await;
    let (back_client, _) = enroll_as(&app, &org, &back, "mach-prune-back").await;
    let (stays_client, _) = enroll_as(&app, &org, &stays, "mach-prune-stays").await;
    let (admin_client, _) = enroll_as(&app, &org, &org.admin, "mach-prune-admin").await;
    // Memberships that ended before the hook existed.
    end_membership_directly(&app, &org, &org.member).await;
    end_membership_directly(&app, &org, &back).await;
    let op = operator_token(&app, operator, "tcprune");

    let (status, _) = call(
        &app,
        "POST",
        PRUNE,
        &org.admin.access_token,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, 404, "only a platform operator");

    let (status, body) = call(&app, "POST", PRUNE, &op, Some(json!({}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["dry_run"], true, "a dry run unless asked: {body}");
    assert_eq!(body["pairs"], 2, "{body}");
    assert_eq!(body["clients"], 2, "{body}");
    assert_eq!(body["leases"], 1, "{body}");
    assert_eq!(body["ended"], 0, "{body}");
    let listed = listed_clients(&body);
    assert!(listed.contains(&gone_client), "{body}");
    assert!(listed.contains(&back_client), "{body}");
    assert!(
        !is_removed(&app, &gone_client).await,
        "a dry run changes nothing"
    );

    // A member again before the real run: left alone.
    readd(&app, &org, &back).await;

    let (status, body) = call(&app, "POST", PRUNE, &op, Some(json!({ "dry_run": false }))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["pairs"], 1, "{body}");
    assert_eq!(body["ended"], 1, "{body}");
    assert_eq!(body["failed"], 0, "{body}");
    assert_eq!(body["more"], false, "{body}");
    assert!(is_removed(&app, &gone_client).await);
    assert!(
        live_node_of(&app, &org, &gone_client).await.is_none(),
        "and its lease is released"
    );
    assert!(
        !is_removed(&app, &back_client).await,
        "a person who is a member again is left alone"
    );
    assert!(!is_removed(&app, &stays_client).await);
    assert!(!is_removed(&app, &admin_client).await);

    let (status, body) = call(&app, "POST", PRUNE, &op, Some(json!({ "dry_run": false }))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (body["pairs"].clone(), body["ended"].clone()),
        (json!(0), json!(0)),
        "{body}"
    );
}

/// A device manager who brings back the tunnel client of someone who left
/// the org owns it: the row then names the person its new token acts for, so
/// it is no client of a non-member, and the operator step leaves it alone.
/// The revived client works.
#[tokio::test]
async fn a_device_manager_who_brings_back_a_removed_members_client_owns_it() {
    let operator = ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(operator.to_hex());
    })
    .await;
    let org = app.seed_tenant("tcback").await;
    let (client, _) = enroll_as(&app, &org, &org.member, "mach-back").await;
    remove(&app, &org, &org.member).await;
    assert!(is_removed(&app, &client).await, "removed with its owner");

    let (revived, revived_token) = enroll_as(&app, &org, &org.admin, "mach-back").await;
    assert_eq!(revived, client, "the same client comes back");
    let row = client_row(&app, &client).await;
    assert!(removed_at(&row).is_none());
    assert_eq!(
        row.get_object_id("owner_user_id").unwrap().to_hex(),
        org.admin.id,
        "as its reviver's"
    );
    assert_eq!(renew(&app, &revived_token).await, 200);

    let op = operator_token(&app, operator, "tcback");
    let (status, body) = call(&app, "POST", PRUNE, &op, Some(json!({}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["pairs"], 0, "nobody's leftover: {body}");
}

/// What a removal could not end is left for the operator step, and the rest
/// of the removal still happens. A tombstone that fails leaves the client
/// live, yet the person's room memberships still end (the network hook does
/// not stop the cascade before chat's); a release that fails leaves the
/// lease of a removed client. The operator step finds both pairs, by the
/// live client and by the lease, and ends them.
#[tokio::test]
async fn what_a_removal_could_not_end_is_left_for_the_operator_step() {
    let operator = ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(operator.to_hex());
    })
    .await;
    let org = app.seed_tenant("tcfail").await;
    let member = &org.member;
    let late = add_member(&app, &org, "late").await;
    let (status, body) = call(
        &app,
        "POST",
        &format!(
            "/api/tenant/{}/room/{}/join",
            org.tenant_id, org.rooms[0].id
        ),
        &member.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "join: {body}");
    assert_eq!(room_rows(&app, &org, member).await, 1);
    let (client, _) = enroll_as(&app, &org, member, "mach-fail-row").await;
    let (late_client, _) = enroll_as(&app, &org, &late, "mach-fail-lease").await;
    lease(&app, &org, &late_client, "mach-fail-lease").await;

    // The tombstone fails.
    refuse_tombstones(&app, "tunnel_clients").await;
    remove(&app, &org, member).await;
    accept_tombstones(&app, "tunnel_clients").await;
    assert_eq!(
        room_rows(&app, &org, member).await,
        0,
        "the rest of the removal still ran"
    );
    assert!(!is_removed(&app, &client).await, "the client is left live");

    // The release fails.
    refuse_tombstones(&app, "overlay_nodes").await;
    remove(&app, &org, &late).await;
    accept_tombstones(&app, "overlay_nodes").await;
    assert!(
        is_removed(&app, &late_client).await,
        "the client is removed"
    );
    assert!(
        live_node_of(&app, &org, &late_client).await.is_some(),
        "its lease is left"
    );

    let op = operator_token(&app, operator, "tcfail");
    let (status, body) = call(&app, "POST", PRUNE, &op, Some(json!({}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["pairs"], 2, "{body}");
    assert_eq!(body["clients"], 1, "{body}");
    assert_eq!(body["leases"], 1, "{body}");
    let (status, body) = call(&app, "POST", PRUNE, &op, Some(json!({ "dry_run": false }))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ended"], 2, "{body}");
    assert_eq!(body["failed"], 0, "{body}");
    assert!(is_removed(&app, &client).await);
    assert!(live_node_of(&app, &org, &late_client).await.is_none());
}

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #1821, #1829 — what a removal leaves behind answers nothing.
//!
//! #1824 and #1832 made the per-id lookups live by name, and their locks live
//! beside the routes they cover (`a_removed_device_is_a_404_on_every_per_device_route`,
//! `a_removed_tunnel_client_is_a_404_on_its_per_client_routes`,
//! `a_removed_overlay_acl_policy_is_a_404_on_every_per_policy_route`). These are
//! the holes found when the audit was run again against what landed since:
//!
//! * FR-52's External access routes were written after #1824 and resolved the
//!   device through `base`, past the name that asks the question.
//! * An overlay join reads its identity straight from the row. A removed tunnel
//!   client keeps its socket until the 60 s revocation poll, and an agent keeps
//!   its until the kick reaches its pod, so either could take a fresh lease
//!   after its removal, and the `DELETE` that would release it is a 404 now.
//! * A relay request names its peer by id and read it the same way, so a live
//!   node could be granted a relay to a node that had already been released.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::{Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use roomler_ai_remote_control::models::{ExternalAccessPolicy, NodeRef};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::agent_presence_tests::{connect_agent, enroll};
use crate::fixtures::{seed::SeededTenant, test_app::TestApp};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn oid(hex: &str) -> ObjectId {
    ObjectId::parse_str(hex).unwrap()
}

fn tail(seeded: &SeededTenant, path: &str) -> String {
    format!("/api/tenant/{}{path}", seeded.tenant_id)
}

async fn status_of_put(app: &TestApp, path: &str, token: &str, body: Value) -> u16 {
    let resp = app.auth_put(path, token).json(&body).send().await.unwrap();
    resp.status().as_u16()
}

async fn status_of_post(app: &TestApp, path: &str, token: &str) -> u16 {
    let resp = app
        .auth_post(path, token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    resp.status().as_u16()
}

async fn status_of_get(app: &TestApp, path: &str, token: &str) -> u16 {
    let resp = app.auth_get(path, token).send().await.unwrap();
    resp.status().as_u16()
}

/// `rc:overlay.join` under a key made of `seed`.
async fn send_join(ws: &mut Ws, seed: u8) {
    let msg = json!({
        "t": "rc:overlay.join",
        "wg_public_key": BASE64.encode([seed; 32]),
        "mtu": 1280,
        "endpoints": [format!("203.0.113.{seed}:51820")],
        "advertised_routes": [],
    });
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .expect("send join");
}

/// The next message of type `want`, or `None` once `wait` passes or the socket
/// closes.
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

async fn netmap_within(ws: &mut Ws, wait: Duration) -> Option<Value> {
    recv_t(ws, "rc:overlay.netmap", wait).await
}

/// The socket is still served: a ping comes back. Without this, a join that
/// got no answer could simply mean the socket had gone, and the test would be
/// proving nothing about the join.
async fn still_served(ws: &mut Ws) -> bool {
    if ws
        .send(Message::Ping(b"tombstone".to_vec().into()))
        .await
        .is_err()
    {
        return false;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Pong(_)))) => return true,
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return false,
        }
    }
}

async fn live_nodes(app: &TestApp, tenant: ObjectId) -> u64 {
    app.db
        .collection::<Document>("overlay_nodes")
        .count_documents(doc! { "tenant_id": tenant, "deleted_at": null })
        .await
        .unwrap()
}

/// Enrol a tunnel client, returning `(client id, its long-lived token)`.
async fn enroll_tunnel_client(
    app: &TestApp,
    seeded: &SeededTenant,
    machine_id: &str,
) -> (String, String) {
    let et: Value = app
        .auth_post(
            &tail(seeded, "/tunnel-client/enroll-token"),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let enrolled: Value = app
        .client
        .post(app.url("/api/tunnel-client/enroll"))
        .json(&json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": machine_id,
            "machine_name": "tombstone test laptop",
            "os": "linux",
            "client_version": "0.3.0-test",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        enrolled["tunnel_client_id"].as_str().unwrap().to_string(),
        enrolled["tunnel_client_token"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

async fn connect_tunnel_client(app: &TestApp, token: &str) -> Ws {
    let url = format!(
        "ws://{}/ws?token={}&role=tunnel-client",
        app.addr,
        token
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    );
    let (ws, _) = connect_async(&url).await.expect("tunnel-client ws");
    ws
}

/// #1821 — every External access route answers a removed device as it answers
/// a bogus id, and writes nothing: no connect code minted onto the tombstone, no
/// approval, nobody barred or let back, no audit row claiming any of it. A
/// tombstone revives whole when its machine re-enrolls, so a code minted or an
/// approval written here would come back to life with it.
///
/// Negative control: with `find_live_in_tenant` swapped back for
/// `base.find_by_id_in_tenant` in the routes this fails at the first 404
/// (`POST connect-code` answers 200 with a fresh code); with `live_row` taken
/// back out of the two setters, at "a setter must not land on a tombstone".
#[tokio::test]
async fn a_removed_device_is_a_404_on_every_external_access_route() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tombext").await;
    let admin = seeded.admin.access_token.clone();
    let (agent_id, _) = enroll(&app, &seeded, "mach-tomb-ext").await;
    let (tid, aid) = (oid(&seeded.tenant_id), oid(&agent_id));
    let agents = app.db.collection::<Document>("agents");
    let audit = app.db.collection::<Document>("external_rc_audit");
    let principals = app.db.collection::<Document>("external_principals");

    agents
        .update_one(
            doc! { "_id": aid },
            doc! { "$set": { "capabilities.rpc": ["external-access"] } },
        )
        .await
        .unwrap();
    let visitor = ObjectId::new();
    app.state
        .fleet()
        .external_principals
        .note_session(tid, aid, visitor)
        .await
        .unwrap();

    let policy = tail(
        &seeded,
        &format!("/agent/{agent_id}/external-access-policy"),
    );
    let code = tail(&seeded, &format!("/agent/{agent_id}/connect-code"));
    let principal = tail(
        &seeded,
        &format!("/agent/{agent_id}/external-principals/{}", visitor.to_hex()),
    );

    // The positive control: each of these resolves a LIVE device, so a 404
    // below is the lookup's answer and not a route, a body or a gate.
    assert_eq!(
        status_of_put(&app, &policy, &admin, json!({ "approved": true })).await,
        200,
        "control: approve a live device"
    );
    assert_eq!(
        status_of_post(&app, &code, &admin).await,
        200,
        "control: mint a live device's code"
    );
    assert_eq!(
        status_of_get(&app, &code, &admin).await,
        200,
        "control: read a live device's code"
    );
    assert_eq!(
        status_of_put(&app, &principal, &admin, json!({ "revoked": true })).await,
        200,
        "control: bar a person from a live device"
    );
    assert_eq!(
        status_of_put(&app, &principal, &admin, json!({ "revoked": false })).await,
        200,
        "control: let them back"
    );

    // Remove it.
    let resp = app
        .auth_delete(&tail(&seeded, &format!("/agent/{agent_id}")), &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let tombstone = agents.find_one(doc! { "_id": aid }).await.unwrap().unwrap();
    assert!(tombstone.get_datetime("deleted_at").is_ok());
    let audit_rows = audit
        .count_documents(doc! { "tenant_id": tid })
        .await
        .unwrap();
    let principal_row = principals
        .find_one(doc! { "agent_id": aid, "user_id": visitor })
        .await
        .unwrap()
        .unwrap();

    // Each route: 404, exactly like a bogus id.
    assert_eq!(
        status_of_post(&app, &code, &admin).await,
        404,
        "POST connect-code on a removed device"
    );
    assert_eq!(
        status_of_get(&app, &code, &admin).await,
        404,
        "GET connect-code on a removed device"
    );
    for approved in [true, false] {
        assert_eq!(
            status_of_put(&app, &policy, &admin, json!({ "approved": approved })).await,
            404,
            "PUT external-access-policy (approved: {approved}) on a removed device"
        );
    }
    for revoked in [true, false] {
        assert_eq!(
            status_of_put(&app, &principal, &admin, json!({ "revoked": revoked })).await,
            404,
            "PUT external-principals (revoked: {revoked}) on a removed device"
        );
    }

    // …and none of them wrote anything.
    assert_eq!(
        agents.find_one(doc! { "_id": aid }).await.unwrap().unwrap(),
        tombstone,
        "the tombstone is what the removal left, field for field"
    );
    assert_eq!(
        audit
            .count_documents(doc! { "tenant_id": tid })
            .await
            .unwrap(),
        audit_rows,
        "an act that did not happen leaves no audit row"
    );
    assert_eq!(
        principals
            .find_one(doc! { "agent_id": aid, "user_id": visitor })
            .await
            .unwrap()
            .unwrap(),
        principal_row,
        "nobody was barred or let back on a removed device"
    );

    // The setters are live-scoped on their own, not only behind the routes'
    // lookup ("a gate applies at every entry point, or it is a courtesy").
    let fleet = app.state.fleet();
    assert!(
        !fleet
            .agents
            .update_external_access_policy(tid, aid, &ExternalAccessPolicy::default())
            .await
            .unwrap(),
        "a setter must not land on a tombstone"
    );
    assert!(
        !fleet
            .agents
            .set_connect_code(tid, aid, "NOTAREALCODE")
            .await
            .unwrap(),
        "a setter must not land on a tombstone"
    );
    assert_eq!(
        agents.find_one(doc! { "_id": aid }).await.unwrap().unwrap(),
        tombstone,
    );

    // A removed device stays auditable: what it did before the removal reads
    // as it did. `DELETE` must not be the way to hide what was approved here.
    let log: Value = app
        .auth_get(&tail(&seeded, "/external-rc-audit"), &admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        log["items"].as_array().map_or(0, Vec::len) as u64,
        audit_rows,
        "the decision log still reads in full: {log}"
    );
}

/// #1829 — a removed tunnel client's socket outlives its row by up to the 60 s
/// of the revocation poll. A join on that socket read the row by id, found it,
/// and leased a fresh address to a client that no longer exists: peers got the
/// upsert, and the `DELETE` that would release it is a 404 now, which leaves
/// only someone finding the node by hand and evicting it.
///
/// Negative control: with the `removed` check taken back out of
/// `handle_overlay_join` this fails at "a removed client takes no new lease"
/// with a netmap in hand.
#[tokio::test]
async fn a_removed_tunnel_client_takes_no_new_lease_on_the_socket_it_still_holds() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tombjoin").await;
    let admin = seeded.admin.access_token.clone();
    let tid = oid(&seeded.tenant_id);
    let (client_id, token) = enroll_tunnel_client(&app, &seeded, "mach-tomb-tc").await;
    let mut ws = connect_tunnel_client(&app, &token).await;

    // The positive control: a live client joins on this socket.
    send_join(&mut ws, 7).await;
    assert!(
        netmap_within(&mut ws, Duration::from_secs(10))
            .await
            .is_some(),
        "control: a live client gets a netmap"
    );
    assert_eq!(live_nodes(&app, tid).await, 1, "control: it holds a lease");

    let resp = app
        .auth_delete(
            &tail(&seeded, &format!("/tunnel-client/{client_id}")),
            &admin,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(live_nodes(&app, tid).await, 0, "the removal released it");
    assert!(
        still_served(&mut ws).await,
        "the premise: the poll has not run, and the socket is still served"
    );

    send_join(&mut ws, 7).await;
    assert!(
        netmap_within(&mut ws, Duration::from_secs(3))
            .await
            .is_none(),
        "a removed client takes no new lease"
    );
    assert_eq!(
        live_nodes(&app, tid).await,
        0,
        "a removed client holds no address"
    );
}

/// #1821 — the same, for a device whose kick has not reached its socket yet: its
/// row is a tombstone, and the socket is on another pod or the kick is still
/// queued behind the removal's own steps. Driven at the DAO, so the kick that
/// the route sends does not close the socket first.
///
/// Negative control: as above, with the `removed` check taken out.
#[tokio::test]
async fn a_removed_device_takes_no_new_lease_while_its_kick_is_on_the_way() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tombkick").await;
    let tid = oid(&seeded.tenant_id);
    let (agent_id, token) = enroll(&app, &seeded, "mach-tomb-kick").await;
    let aid = oid(&agent_id);
    let mut ws = connect_agent(&app, &token).await;

    send_join(&mut ws, 9).await;
    assert!(
        netmap_within(&mut ws, Duration::from_secs(10))
            .await
            .is_some(),
        "control: a live device gets a netmap"
    );
    assert_eq!(live_nodes(&app, tid).await, 1, "control: it holds a lease");

    // The sequence's first two steps; the kick is left for later.
    roomler_ai_mod_network::overlay::release_overlay_node_for(
        app.state.network(),
        tid,
        "mach-tomb-kick",
        &NodeRef::Agent { agent_id: aid },
        "tombstone test",
    )
    .await
    .expect("the lease is released");
    assert!(
        app.state
            .fleet()
            .agents
            .soft_delete(tid, aid)
            .await
            .unwrap()
    );
    assert!(
        still_served(&mut ws).await,
        "the premise: nothing has closed the socket yet"
    );

    send_join(&mut ws, 9).await;
    assert!(
        netmap_within(&mut ws, Duration::from_secs(3))
            .await
            .is_none(),
        "a removed device takes no new lease"
    );
    assert_eq!(
        live_nodes(&app, tid).await,
        0,
        "a removed device holds no address"
    );
}

/// #1829 — a relay request names its peer by id. The requester's own lookup is
/// live, the peer's was not: a live node asking about a released one was granted
/// a relay to it, and the grant went out to the tombstone's socket too if it
/// still had one. The address it was asked about may already be someone else's.
///
/// Negative control: with the `deleted_at` check taken back out of
/// `handle_overlay_relay_request` this fails at "no relay to a released peer".
#[tokio::test]
async fn a_released_peer_gets_no_relay_grant() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tombrelay").await;
    let admin = seeded.admin.access_token.clone();
    let tid = oid(&seeded.tenant_id);
    let mut sockets = Vec::new();
    for (machine, seed) in [("mach-tomb-ra", 31u8), ("mach-tomb-rb", 32u8)] {
        let (_, token) = enroll(&app, &seeded, machine).await;
        let mut ws = connect_agent(&app, &token).await;
        let join = json!({
            "t": "rc:overlay.join",
            "wg_public_key": BASE64.encode([seed; 32]),
            "mtu": 1280,
            "endpoints": [format!("203.0.113.{seed}:41641")],
            "supports_derp": true,
            "supports_relay_single": true,
        });
        ws.send(Message::Text(join.to_string().into()))
            .await
            .expect("send join");
        assert!(
            netmap_within(&mut ws, Duration::from_secs(10))
                .await
                .is_some(),
            "control: {machine} joins"
        );
        sockets.push(ws);
    }
    let peer_id = app
        .db
        .collection::<Document>("overlay_nodes")
        .find_one(doc! { "tenant_id": tid, "machine_id": "mach-tomb-rb", "deleted_at": null })
        .await
        .unwrap()
        .unwrap()
        .get_object_id("_id")
        .unwrap();
    let ask = Message::Text(
        json!({ "t": "rc:overlay.relay_request", "peer_node_id": peer_id.to_hex() })
            .to_string()
            .into(),
    );
    let requester = &mut sockets[0];

    // The positive control: the same request about a live peer is granted.
    requester
        .send(ask.clone())
        .await
        .expect("send relay_request");
    assert!(
        recv_t(requester, "rc:overlay.relay_grant", Duration::from_secs(5))
            .await
            .is_some(),
        "control: a relay to a live peer is granted"
    );

    let resp = app
        .auth_delete(&tail(&seeded, &format!("/overlay-node/{peer_id}")), &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the admin evicts the peer");

    requester.send(ask).await.expect("send relay_request");
    assert!(
        recv_t(requester, "rc:overlay.relay_grant", Duration::from_secs(3))
            .await
            .is_none(),
        "no relay to a released peer"
    );
}

/// A tombstone that came back through its own door is a live row again: the
/// join check reads `deleted_at`, it does not remember a removal. Re-enrolling
/// is the way back, and the machine gets a fresh lease, never the old node.
///
/// Negative control: this is the one a too-eager fix breaks. Refuse on any row
/// whose node was ever tombstoned, and the re-enrolled client never joins.
#[tokio::test]
async fn a_re_enrolled_tunnel_client_joins_again_on_a_fresh_lease() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tombback").await;
    let admin = seeded.admin.access_token.clone();
    let tid = oid(&seeded.tenant_id);
    let (client_id, token) = enroll_tunnel_client(&app, &seeded, "mach-tomb-back").await;
    let mut ws = connect_tunnel_client(&app, &token).await;
    send_join(&mut ws, 11).await;
    netmap_within(&mut ws, Duration::from_secs(10))
        .await
        .expect("control: a live client gets a netmap");
    let nodes = app.db.collection::<Document>("overlay_nodes");
    let live = doc! { "tenant_id": tid, "deleted_at": null };
    let first = nodes.find_one(live.clone()).await.unwrap().unwrap();
    let resp = app
        .auth_delete(
            &tail(&seeded, &format!("/tunnel-client/{client_id}")),
            &admin,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    drop(ws);

    let (again, token) = enroll_tunnel_client(&app, &seeded, "mach-tomb-back").await;
    assert_eq!(again, client_id, "the machine revives its own row");
    let mut ws = connect_tunnel_client(&app, &token).await;
    send_join(&mut ws, 11).await;
    netmap_within(&mut ws, Duration::from_secs(10))
        .await
        .expect("a re-enrolled client joins");
    assert_eq!(live_nodes(&app, tid).await, 1, "one live node, the new one");
    let second = nodes.find_one(live).await.unwrap().unwrap();
    assert_ne!(
        first.get_object_id("_id").unwrap(),
        second.get_object_id("_id").unwrap(),
        "a fresh node, never the revived tombstone"
    );
}

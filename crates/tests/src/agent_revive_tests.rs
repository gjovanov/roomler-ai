// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! A removed device that enrolls again starts from a new device's settings.
//!
//! Enrolling a machine id its org has removed revives the removed row in
//! place: the unique `(tenant_id, machine_id)` index covers tombstones, so the
//! row has to come back rather than a second one being made. Before this, the
//! revived row came back with everything the removed device had been allowed:
//! its SSH, exec and access policies, the org-relay and External access
//! approvals, its connect code, approved routes, owner and labels, a pending
//! config (pushed to the machine again on its first connect, `ssh_enabled` and
//! authorized keys included) and a standing key-rotation order. The org's
//! tunnel policies that named it applied to it again too.
//!
//! A revive keeps what identifies the row and its history (`_id`,
//! `machine_id`, `created_at`, `enrolled_by`), the removal generation that
//! keeps every earlier token dead (FR-103), and the desired-config revision
//! counter. Everything else is what a new enrollment gets. A device nobody
//! removed keeps all of it when it enrolls again (the control).
//!
//! The overlay is unchanged and pinned here: the removal released the lease
//! (tombstone, then the address pool), and the revived device's first join
//! takes a NEW node, at the pool's head, which in a small org is its own old
//! address: a fresh lease that any joiner could have got.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::{DateTime, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use roomler_ai_remote_control::models::{
    AccessPolicy, Agent, ConsentMode, DesiredConfig, ExecMode, ExecPolicy, ExternalAccessPolicy,
    KeyRotationRequest, OsKind, PeerRelayPolicy, PolicySubject, PolicyTarget, SshMode, SshPolicy,
};
use roomler_ai_services::dao::base::DaoError;
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::fixtures::{seed::SeededTenant, test_app::TestApp};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const WAIT: Duration = Duration::from_secs(10);

fn oid(hex: &str) -> ObjectId {
    ObjectId::parse_str(hex).unwrap()
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// Enroll `machine_id` as `machine_name` with a fresh token minted by the
/// seeded admin. Returns `(agent_id, agent_token)`.
async fn enroll(
    app: &TestApp,
    seeded: &SeededTenant,
    machine_id: &str,
    machine_name: &str,
) -> (String, String) {
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
    let resp = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": machine_id,
            "machine_name": machine_name,
            "os": "linux",
            "agent_version": "0.4.207",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "enroll {machine_id}");
    let ej: Value = resp.json().await.unwrap();
    (
        ej["agent_id"].as_str().unwrap().to_string(),
        ej["agent_token"].as_str().unwrap().to_string(),
    )
}

/// An agent socket whose hello advertises `rpc`.
async fn connect(app: &TestApp, token: &str, rpc: &[&str]) -> Ws {
    let url = format!("ws://{}/ws?token={}&role=agent", app.addr, urlencode(token));
    let (mut ws, _) = connect_async(&url).await.expect("agent ws connect");
    ws.send(Message::Text(
        json!({
            "t": "rc:agent.hello",
            "machine_name": "revive box",
            "os": "linux",
            "agent_version": "0.4.207",
            "displays": [],
            "caps": {
                "hw_encoders": [], "codecs": ["h264"], "has_input_permission": true,
                "supports_clipboard": false, "supports_file_transfer": false,
                "max_simultaneous_sessions": 1, "rpc": rpc,
            },
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    ws
}

/// `rc:overlay.join` under the key made of `seed`, then every frame up to and
/// including the netmap that answers it.
async fn join(ws: &mut Ws, seed: u8) -> (Vec<Value>, Value) {
    ws.send(Message::Text(
        json!({
            "t": "rc:overlay.join",
            "wg_public_key": BASE64.encode([seed; 32]),
            "key_epoch": 0,
            "mtu": 1280,
            "endpoints": [format!("203.0.113.{seed}:51820")],
            "advertised_routes": [],
            "supports_join_refusal": true,
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send join");
    let deadline = tokio::time::Instant::now() + WAIT;
    let mut before = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "no netmap answered the join: {before:?}");
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(txt)))) => {
                let v: Value = serde_json::from_str(&txt).unwrap_or(Value::Null);
                if v["t"] == "rc:overlay.netmap" {
                    return (before, v);
                }
                before.push(v);
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("the socket ended before a netmap: {other:?}; frames {before:?}"),
        }
    }
}

/// A tunnel policy through the admin route, naming the device as `subjects` /
/// `targets` say. Returns its id.
async fn tunnel_policy(app: &TestApp, seeded: &SeededTenant, name: &str, body: Value) -> ObjectId {
    let mut body = body;
    body["name"] = json!(name);
    body["allowlist"] = json!([{
        "host_pattern": { "kind": "cidr", "value": "10.20.0.0/24" },
        "port_range": { "low": 1, "high": 65535 }
    }]);
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/tunnel-policy", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "create tunnel policy {name}");
    let v: Value = resp.json().await.unwrap();
    oid(v["id"].as_str().unwrap())
}

/// Every grant the device can hold on its own row, set while it is live.
async fn grant_everything(app: &TestApp, tid: ObjectId, aid: ObjectId, member: ObjectId) {
    let fleet = app.state.fleet();
    let agents = &fleet.agents;
    let ok = |what: &str, wrote: bool| assert!(wrote, "setting {what} on the live device");
    ok(
        "ssh_policy",
        agents
            .update_ssh_policy(
                tid,
                aid,
                &SshPolicy {
                    mode: SshMode::On,
                    can_originate: true,
                    allowed_user_ids: vec![member],
                    consent_mode: Some(ConsentMode::Auto),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
    );
    ok(
        "exec_policy",
        agents
            .update_exec_policy(
                tid,
                aid,
                &ExecPolicy {
                    mode: ExecMode::On,
                    can_originate: true,
                    consent_mode: Some(ConsentMode::Auto),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
    );
    ok(
        "access_policy",
        agents
            .update_access_policy(
                tid,
                aid,
                &AccessPolicy {
                    consent_mode: Some(ConsentMode::Auto),
                    allowed_user_ids: vec![member],
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
    );
    ok(
        "peer_relay_policy",
        agents
            .update_peer_relay_policy(
                tid,
                aid,
                &PeerRelayPolicy {
                    serve: true,
                    static_endpoints: vec!["198.51.100.7:3478".into()],
                },
            )
            .await
            .unwrap(),
    );
    ok(
        "external_access_policy",
        agents
            .update_external_access_policy(
                tid,
                aid,
                &ExternalAccessPolicy {
                    approved: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
    );
    ok(
        "connect_code",
        agents
            .set_connect_code(tid, aid, &format!("R{}", &aid.to_hex()[13..]))
            .await
            .unwrap(),
    );
    ok(
        "desired_config",
        agents
            .set_desired_config(
                tid,
                aid,
                &DesiredConfig {
                    exec_enabled: Some(true),
                    ssh_enabled: Some(true),
                    ssh_authorized_keys: Some(vec![
                        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIRevive revive-test".into(),
                    ]),
                    revision: 3,
                    updated_by: Some(member),
                    updated_at: Some(DateTime::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
    );
    ok(
        "routes",
        agents
            .update_routes(tid, aid, &["10.20.0.0/24".to_string()])
            .await
            .unwrap(),
    );
    ok(
        "name",
        agents.rename(tid, aid, "front-desk-pc").await.unwrap(),
    );
    ok(
        "display_name",
        agents
            .set_display_name(tid, aid, Some("Front desk"))
            .await
            .unwrap(),
    );
    ok(
        "tags",
        agents.set_tags(tid, aid, &["prod".into()]).await.unwrap(),
    );
    ok(
        "owner",
        agents.update_owner(tid, aid, member).await.unwrap(),
    );
    ok(
        "key_rotation",
        agents
            .record_key_rotation_request(
                tid,
                aid,
                &KeyRotationRequest {
                    request_id: "revive-order".into(),
                    requested_by: member,
                    requested_at: DateTime::now(),
                    trigger: None,
                    delivered_at: None,
                    public_key_before: Some(BASE64.encode([1u8; 32])),
                },
            )
            .await
            .unwrap(),
    );
}

/// What on the revived row still says what the removed device was allowed.
/// Empty = the row reads as a new enrollment by `enroller` named `name`.
fn came_back(row: &Agent, enroller: ObjectId, name: &str) -> Vec<String> {
    let mut back = Vec::new();
    let mut check = |field: &str, carried: bool, value: String| {
        if carried {
            back.push(format!("{field} = {value}"));
        }
    };
    check(
        "ssh_policy",
        row.ssh_policy != SshPolicy::default(),
        format!("{:?}", row.ssh_policy),
    );
    check(
        "exec_policy",
        row.exec_policy != ExecPolicy::default(),
        format!("{:?}", row.exec_policy),
    );
    // No `PartialEq` on `AccessPolicy`: compare what is stored.
    let access = serde_json::to_value(&row.access_policy).unwrap();
    check(
        "access_policy",
        access != serde_json::to_value(AccessPolicy::default()).unwrap(),
        access.to_string(),
    );
    check(
        "peer_relay_policy",
        row.peer_relay_policy != PeerRelayPolicy::default(),
        format!("{:?}", row.peer_relay_policy),
    );
    check(
        "external_access_policy",
        row.external_access_policy != ExternalAccessPolicy::default(),
        format!("{:?}", row.external_access_policy),
    );
    check(
        "connect_code",
        row.connect_code.is_some(),
        format!("{:?}", row.connect_code),
    );
    check(
        "desired_config",
        !row.desired_config.is_empty() || row.desired_config.updated_by.is_some(),
        format!("{:?}", row.desired_config),
    );
    check(
        "routes",
        !row.routes.is_empty(),
        format!("{:?}", row.routes),
    );
    check(
        "key_rotation",
        row.key_rotation.is_some(),
        format!("{:?}", row.key_rotation),
    );
    check(
        "overlay_identity",
        row.overlay_identity.is_some(),
        format!("{:?}", row.overlay_identity),
    );
    check(
        "owner_user_id",
        row.owner_user_id != enroller,
        row.owner_user_id.to_hex(),
    );
    check("name", row.name != name, row.name.clone());
    check(
        "name_admin_set",
        row.name_admin_set,
        row.name_admin_set.to_string(),
    );
    check(
        "display_name",
        row.display_name.is_some(),
        format!("{:?}", row.display_name),
    );
    check("tags", !row.tags.is_empty(), format!("{:?}", row.tags));
    check(
        "ssh_host_pubkey",
        !row.ssh_host_pubkey.is_empty(),
        row.ssh_host_pubkey.clone(),
    );
    back
}

/// The finding: delete a device, enroll its machine again, and the row must
/// read as a new enrollment, the org's tunnel policies must no longer name it,
/// and its first connect must be pushed nothing meant for the removed device.
#[tokio::test]
async fn a_removed_device_that_enrolls_again_starts_from_a_new_devices_settings() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("revive1").await;
    let (tid, admin, member) = (
        oid(&seeded.tenant_id),
        oid(&seeded.admin.id),
        oid(&seeded.member.id),
    );

    // A live device that joined the mesh, with every grant set.
    let (agent_id, token) = enroll(&app, &seeded, "mach-revive", "first box").await;
    let aid = oid(&agent_id);
    let mut ws = connect(&app, &token, &["config", "key-rotate"]).await;
    let (_, netmap) = join(&mut ws, 1).await;
    let first_ip = netmap["self_ip"].as_str().unwrap().to_string();
    // The join stamps the overlay identity off the netmap path; let it land.
    for _ in 0..50 {
        let row = app
            .state
            .fleet()
            .agents
            .find_live_in_tenant(tid, aid)
            .await
            .unwrap();
        if row.overlay_identity.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    grant_everything(&app, tid, aid, member).await;
    // Another device, so a policy naming it shows what must stay.
    let (other_id, _) = enroll(&app, &seeded, "mach-other", "other box").await;
    let other = oid(&other_id);
    let to_it = tunnel_policy(
        &app,
        &seeded,
        "to the removed device",
        json!({
            "subjects": [{ "kind": "all_users" }],
            "targets": [
                { "kind": "agent_id", "id": agent_id },
                { "kind": "agent_id", "id": other_id },
            ],
        }),
    )
    .await;
    let from_it = tunnel_policy(
        &app,
        &seeded,
        "from the removed device",
        json!({
            "subjects": [
                { "kind": "agent_id", "id": agent_id },
                { "kind": "user_id", "id": seeded.member.id },
            ],
            "targets": [{ "kind": "all_agents" }],
        }),
    )
    .await;
    let unrelated = tunnel_policy(
        &app,
        &seeded,
        "the other device only",
        json!({
            "subjects": [{ "kind": "all_users" }],
            "targets": [{ "kind": "agent_id", "id": other_id }],
        }),
    )
    .await;
    let before = app
        .state
        .fleet()
        .agents
        .find_live_in_tenant(tid, aid)
        .await
        .unwrap();
    assert!(
        came_back(&before, admin, "first box").len() >= 14,
        "precondition: the live device holds every grant: {:?}",
        came_back(&before, admin, "first box")
    );

    // The admin removes it. The lease goes back to the pool.
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/agent/{agent_id}", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "delete");
    let removed: Value = resp.json().await.unwrap();
    assert_eq!(removed["overlay_ip"].as_str(), Some(first_ip.as_str()));
    drop(ws);
    let network = app.state.network();
    let first_node = network
        .overlay_nodes
        .base
        .find_one(doc! { "tenant_id": tid, "machine_id": "mach-revive" })
        .await
        .unwrap()
        .expect("the released node stays as a tombstone");
    assert!(first_node.deleted_at.is_some());

    // The same machine id enrolls again, with a fresh token from the admin.
    let (again_id, again_token) = enroll(&app, &seeded, "mach-revive", "replacement box").await;
    assert_eq!(again_id, agent_id, "the removed row is revived in place");
    let row = app
        .state
        .fleet()
        .agents
        .find_live_in_tenant(tid, aid)
        .await
        .unwrap();

    let mut wrong = came_back(&row, admin, "replacement box");

    // What a revive keeps.
    assert_eq!(row.created_at, before.created_at, "created_at is history");
    assert_eq!(
        row.enrolled_by, before.enrolled_by,
        "enrolled_by is history"
    );
    assert_eq!(
        row.token_gen,
        before.token_gen + 1,
        "the removal's generation stays, or the removed device's tokens would work again"
    );
    assert_eq!(
        row.desired_config.revision, 3,
        "the revision counter carries, so the next save is never a number already used"
    );

    // The org's tunnel policies no longer name the removed device; nothing
    // else about them changed.
    let network = app.state.network();
    let p = network
        .tunnel_policies
        .find_live_in_tenant(tid, to_it)
        .await
        .unwrap();
    if p.targets.contains(&PolicyTarget::AgentId { agent_id: aid }) {
        wrong.push(format!("tunnel policy targets = {:?}", p.targets));
    }
    assert!(
        p.targets
            .contains(&PolicyTarget::AgentId { agent_id: other }),
        "another device's place in the policy stays"
    );
    let p = network
        .tunnel_policies
        .find_live_in_tenant(tid, from_it)
        .await
        .unwrap();
    if p.subjects
        .contains(&PolicySubject::AgentId { agent_id: aid })
    {
        wrong.push(format!("tunnel policy subjects = {:?}", p.subjects));
    }
    assert!(
        p.subjects
            .contains(&PolicySubject::UserId { user_id: member }),
        "the policy's other subjects stay"
    );
    assert_eq!(
        network
            .tunnel_policies
            .find_live_in_tenant(tid, unrelated)
            .await
            .unwrap()
            .targets,
        vec![PolicyTarget::AgentId { agent_id: other }],
        "a policy that never named the device is untouched"
    );

    // Its first connect: nothing meant for the removed device is pushed, and
    // the join leases a NEW node. The address is the pool's head, which here is
    // the removed device's own: a fresh lease, as any joiner would get.
    let mut ws = connect(&app, &again_token, &["config", "key-rotate"]).await;
    let (frames, netmap) = join(&mut ws, 1).await;
    for f in &frames {
        if f["t"] == "rc:agent.config" || f["t"] == "rc:agent.key_rotate" {
            wrong.push(format!("pushed on connect: {f}"));
        }
    }
    assert_eq!(
        netmap["self_ip"].as_str(),
        Some(first_ip.as_str()),
        "the released address is the pool's head"
    );
    let live = network
        .overlay_nodes
        .find_live_by_tenant_and_machine(tid, "mach-revive")
        .await
        .unwrap()
        .expect("the revived device holds a lease");
    assert_ne!(live.id, first_node.id, "a new node, never the tombstone");
    let tomb = network
        .overlay_nodes
        .base
        .find_by_id(first_node.id.unwrap())
        .await
        .unwrap();
    assert!(tomb.deleted_at.is_some(), "the old node stays a tombstone");

    assert!(
        wrong.is_empty(),
        "the removed device's settings came back with its machine id:\n  {}",
        wrong.join("\n  ")
    );
}

/// The control: re-enrolling a device nobody removed (a lost token, a
/// reinstall) keeps everything it was allowed, as it always has.
#[tokio::test]
async fn a_device_nobody_removed_keeps_its_settings_when_it_enrolls_again() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("revive2").await;
    let (tid, admin, member) = (
        oid(&seeded.tenant_id),
        oid(&seeded.admin.id),
        oid(&seeded.member.id),
    );
    let (agent_id, _) = enroll(&app, &seeded, "mach-keep", "kept box").await;
    let aid = oid(&agent_id);
    grant_everything(&app, tid, aid, member).await;
    let target = tunnel_policy(
        &app,
        &seeded,
        "to the kept device",
        json!({
            "subjects": [{ "kind": "all_users" }],
            "targets": [{ "kind": "agent_id", "id": agent_id }],
        }),
    )
    .await;
    let before = app
        .state
        .fleet()
        .agents
        .find_live_in_tenant(tid, aid)
        .await
        .unwrap();

    let (again_id, _) = enroll(&app, &seeded, "mach-keep", "kept box renamed").await;
    assert_eq!(again_id, agent_id);
    let row = app
        .state
        .fleet()
        .agents
        .find_live_in_tenant(tid, aid)
        .await
        .unwrap();

    assert_eq!(row.ssh_policy, before.ssh_policy);
    assert_eq!(row.exec_policy, before.exec_policy);
    assert_eq!(
        serde_json::to_value(&row.access_policy).unwrap(),
        serde_json::to_value(&before.access_policy).unwrap()
    );
    assert_eq!(row.peer_relay_policy, before.peer_relay_policy);
    assert_eq!(row.external_access_policy, before.external_access_policy);
    assert_eq!(row.connect_code, before.connect_code);
    assert_eq!(row.desired_config, before.desired_config);
    assert_eq!(row.routes, before.routes);
    assert_eq!(row.key_rotation, before.key_rotation);
    assert_eq!(row.owner_user_id, member, "the owner an admin chose stays");
    assert_ne!(row.owner_user_id, admin);
    assert_eq!(
        row.name, "front-desk-pc",
        "an admin's rename survives a re-enrollment"
    );
    assert_eq!(row.display_name.as_deref(), Some("Front desk"));
    assert_eq!(row.tags, vec!["prod".to_string()]);
    assert_eq!(row.token_gen, before.token_gen);
    assert_eq!(
        app.state
            .network()
            .tunnel_policies
            .find_live_in_tenant(tid, target)
            .await
            .unwrap()
            .targets,
        vec![PolicyTarget::AgentId { agent_id: aid }],
        "a live device keeps its place in the org's tunnel policies"
    );
}

/// A live re-enrollment writes only while the row is still live: a removal that
/// lands after the route read the row live is not undone (the same race #3328
/// closed for tunnel clients with `ReviveScope`).
#[tokio::test]
async fn a_live_re_enrollment_does_not_undo_a_removal_that_landed_after_its_read() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("revive3").await;
    let (tid, member) = (oid(&seeded.tenant_id), oid(&seeded.member.id));
    let (agent_id, _) = enroll(&app, &seeded, "mach-race", "race box").await;
    let aid = oid(&agent_id);
    grant_everything(&app, tid, aid, member).await;
    let agents = &app.state.fleet().agents;
    assert!(agents.soft_delete(tid, aid).await.unwrap());

    // The route read the row live, then the removal landed: the write it was
    // about to make must match nothing.
    let refreshed = agents
        .rehydrate(aid, "race box", OsKind::Linux, "0.4.207", None)
        .await;
    assert!(
        matches!(refreshed, Err(DaoError::NotFound)),
        "a live re-enrollment must not write onto a row removed after its read: {:?}",
        refreshed.map(|a| a.deleted_at)
    );
    let row = agents.find_any_in_tenant(tid, aid).await.unwrap();
    assert!(row.deleted_at.is_some(), "the removal stands");
    assert_eq!(row.ssh_policy.mode, SshMode::On, "and nothing was written");
}

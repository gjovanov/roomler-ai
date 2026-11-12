// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! A tunnel policy whose subject is a role grants the forward to the people who
//! hold that role in the policy's org, at the moment they ask (#3635).
//!
//! The forward gate reads the caller's roles on every forward request: the
//! roles the caller's owner holds in the org whose policies decide the forward,
//! and only roles that org still has. Before this the gate matched every policy
//! against an empty role list, so a role subject granted nobody, silently.
//!
//! - a role subject forwards for a member who holds the role, and refuses a
//!   member who does not with `acl_denied`;
//! - the read is per forward: losing the role, or the org, refuses the next
//!   forward on the same open session, and getting the role back allows it;
//! - a device forwards on its owner's roles, until its owner leaves the org (a
//!   device is not removed with the person);
//! - a role of another org, or one this org no longer has, satisfies no policy
//!   here;
//! - the other subject kinds (user, tunnel client, device, all users) forward
//!   exactly as before.

use bson::{doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::fixtures::seed::{SeededTenant, SeededUser};
use crate::fixtures::test_app::TestApp;
use crate::tunnel_policy_permission_tests::{
    Ws, call, connect_tunnel_client, frame_for_flow, owner_creates, person_with_role, send,
};
use crate::tunnel_tests::{connect_agent_ws, enroll_agent, read_until, wait_agent_online};

/// The owner mints a role that carries no permission bit. In a tunnel policy a
/// role is a group of people: the forward reads no bit.
async fn mint_role(app: &TestApp, seeded: &SeededTenant, name: &str) -> String {
    let (status, role) = call(
        app,
        "POST",
        &format!("/api/tenant/{}/role", seeded.tenant_id),
        &seeded.admin.access_token,
        Some(json!({ "name": name, "permissions": 0 })),
    )
    .await;
    assert_eq!(status, 200, "the owner mints a role: {role}");
    role["id"].as_str().expect("role id").to_string()
}

/// The owner gives `user_id` the role (`held`), or takes it back.
async fn set_role(app: &TestApp, seeded: &SeededTenant, role_id: &str, user_id: &str, held: bool) {
    let method = if held { "POST" } else { "DELETE" };
    let (status, body) = call(
        app,
        method,
        &format!(
            "/api/tenant/{}/role/{role_id}/assign/{user_id}",
            seeded.tenant_id
        ),
        &seeded.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{method} role {role_id} for {user_id}: {body}");
}

/// The owner removes `user_id` from the org (`DELETE …/member/{user_id}`).
async fn remove_from_org(app: &TestApp, seeded: &SeededTenant, user_id: &str) {
    let (status, body) = call(
        app,
        "DELETE",
        &format!("/api/tenant/{}/member/{user_id}", seeded.tenant_id),
        &seeded.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "the owner removes {user_id}: {body}");
}

/// `user` joins `seeded`'s org too, holding its managed role `role`, the way
/// the fixture seeds a member.
async fn add_to_org(app: &TestApp, seeded: &SeededTenant, user: &SeededUser, role: &str) {
    let tid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let role_row = app
        .db
        .collection::<bson::Document>("roles")
        .find_one(doc! { "tenant_id": tid, "name": role })
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("the org has no `{role}` role"));
    let now = bson::DateTime::now();
    app.db
        .collection::<bson::Document>("tenant_members")
        .insert_one(doc! {
            "tenant_id": tid,
            "user_id": ObjectId::parse_str(&user.id).unwrap(),
            "nickname": bson::Bson::Null,
            "role_ids": [role_row.get_object_id("_id").unwrap()],
            "joined_at": now,
            "is_pending": false,
            "is_muted": false,
            "notification_override": bson::Bson::Null,
            "invited_by": bson::Bson::Null,
            "last_seen_at": bson::Bson::Null,
            "created_at": now,
            "updated_at": now,
        })
        .await
        .unwrap();
}

/// Write `role_id` into `user`'s membership row in `seeded`'s org directly, so
/// the test controls which ids the row lists: what is under test is what the
/// forward makes of them.
async fn list_in_row(app: &TestApp, seeded: &SeededTenant, user: &SeededUser, role_id: &str) {
    app.db
        .collection::<bson::Document>("tenant_members")
        .update_one(
            doc! {
                "tenant_id": ObjectId::parse_str(&seeded.tenant_id).unwrap(),
                "user_id": ObjectId::parse_str(&user.id).unwrap(),
            },
            doc! { "$addToSet": { "role_ids": ObjectId::parse_str(role_id).unwrap() } },
        )
        .await
        .unwrap();
}

/// A grant of `127.0.0.1:port` on every device of the org to `subjects`.
fn grant(name: &str, subjects: Value, port: u16) -> Value {
    json!({
        "name": name,
        "subjects": subjects,
        "targets": [{ "kind": "all_agents" }],
        "allowlist": [{
            "host_pattern": { "kind": "cidr", "value": "127.0.0.0/8" },
            "port_range": { "low": port, "high": port }
        }],
    })
}

fn role_subject(role_id: &str) -> Value {
    json!([{ "kind": "role_id", "id": role_id }])
}

/// The org's device every forward here goes through, connected and online.
async fn online_device(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> (String, Ws) {
    let (id, token) = enroll_agent(app, seeded, machine_id, machine_id).await;
    let ws = connect_agent_ws(app, &token, machine_id).await;
    wait_agent_online(app, seeded, &id).await;
    (id, ws)
}

/// `who` enrolls their own tunnel client, the way `roomler enroll` does.
/// Returns the client's id and token.
async fn enroll_client(
    app: &TestApp,
    seeded: &SeededTenant,
    who: &SeededUser,
    machine_id: &str,
) -> (String, String) {
    let (status, et) = call(
        app,
        "POST",
        &format!(
            "/api/tenant/{}/tunnel-client/enroll-token",
            seeded.tenant_id
        ),
        &who.access_token,
        None,
    )
    .await;
    assert_eq!(
        status, 200,
        "a member mints a tunnel enrollment token: {et}"
    );
    let enrolled: Value = app
        .client
        .post(app.url("/api/tunnel-client/enroll"))
        .json(&json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": machine_id,
            "machine_name": machine_id,
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
        enrolled["tunnel_client_id"]
            .as_str()
            .expect("tunnel_client_id")
            .to_string(),
        enrolled["tunnel_client_token"]
            .as_str()
            .expect("tunnel_client_token")
            .to_string(),
    )
}

/// Say hello in the tunnel-client role and open a session to `target_id`.
/// Returns the session id.
async fn open_session(ws: &mut Ws, target_id: &str) -> String {
    send(
        ws,
        json!({
            "t": "rc:tunnel.hello",
            "role": "client",
            "version": "0.3.0",
            "supported_transports": ["webrtc-dc-v1"],
        }),
    )
    .await;
    send(
        ws,
        json!({ "t": "rc:tunnel.open", "agent_id": target_id, "transport": "webrtc-dc-v1" }),
    )
    .await;
    let opened = read_until(ws, "rc:tunnel.opened")
        .await
        .expect("the session opens");
    opened["session_id"].as_str().unwrap().to_string()
}

/// `who`'s own tunnel client, connected, with a session open to `target_id`.
async fn client_session(
    app: &TestApp,
    seeded: &SeededTenant,
    who: &SeededUser,
    machine_id: &str,
    target_id: &str,
) -> (Ws, String) {
    let (_, token) = enroll_client(app, seeded, who, machine_id).await;
    let mut ws = connect_tunnel_client(app, &token).await;
    let session_id = open_session(&mut ws, target_id).await;
    (ws, session_id)
}

/// Ask for `127.0.0.1:port` as flow `flow_id`.
async fn request(ws: &mut Ws, session_id: &str, flow_id: u32, port: u16) {
    send(
        ws,
        json!({
            "t": "rc:tunnel.tcp.request",
            "session_id": session_id,
            "flow_id": flow_id,
            "dst_host": "127.0.0.1",
            "dst_port": port,
        }),
    )
    .await;
}

/// The forward reached the device, on `owner`'s behalf. When it did not, the
/// failure names what the gate answered the client instead.
async fn assert_forwarded(client: &mut Ws, device: &mut Ws, flow_id: u32, owner: &str, what: &str) {
    match frame_for_flow(device, "rc:tunnel.tcp.forward", flow_id.into()).await {
        Some(forwarded) => assert_eq!(forwarded["owner_user_id"], owner, "{what}: {forwarded}"),
        None => {
            let answer = frame_for_flow(client, "rc:tunnel.tcp.reject", flow_id.into()).await;
            panic!("{what}: the forward must reach the device; the gate answered {answer:?}");
        }
    }
}

/// The gate refused the forward: `acl_denied`.
async fn assert_acl_denied(client: &mut Ws, flow_id: u32, what: &str) {
    let refused = frame_for_flow(client, "rc:tunnel.tcp.reject", flow_id.into())
        .await
        .unwrap_or_else(|| panic!("{what}: the forward must be refused"));
    assert_eq!(refused["kind"], "acl_denied", "{what}: {refused}");
}

/// The heart of #3635: a policy naming a role forwards for a member who holds
/// it, and only for them.
#[tokio::test]
async fn a_role_subject_forwards_for_a_member_who_holds_the_role_and_for_no_one_else() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("trole-holds").await;
    let ops = mint_role(&app, &seeded, "ops").await;
    owner_creates(
        &app,
        &seeded,
        grant("ops reach the db", role_subject(&ops), 9000),
    )
    .await;
    let (device_id, mut device) = online_device(&app, &seeded, "mach-trole-holds-dev").await;

    let holder = person_with_role(&app, &seeded, "holder", "member").await;
    set_role(&app, &seeded, &ops, &holder.id, true).await;
    let (mut h, h_session) =
        client_session(&app, &seeded, &holder, "mach-trole-holds-h", &device_id).await;
    // The seeded member holds the org's `member` role and not `ops`.
    let (mut n, n_session) = client_session(
        &app,
        &seeded,
        &seeded.member,
        "mach-trole-holds-n",
        &device_id,
    )
    .await;

    request(&mut h, &h_session, 1, 9000).await;
    assert_forwarded(
        &mut h,
        &mut device,
        1,
        &holder.id,
        "a member who holds the role",
    )
    .await;

    request(&mut n, &n_session, 2, 9000).await;
    assert_acl_denied(&mut n, 2, "a member who does not hold the role").await;
}

/// The roles are read on every forward request, never kept from an earlier
/// one: on one open session, losing the role refuses the next forward, getting
/// it back allows it, and leaving the org refuses it although the socket is
/// still open (its 60 s revocation poll has not run).
#[tokio::test]
async fn the_role_is_read_on_every_forward_so_losing_it_or_the_org_stops_the_next_one() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("trole-fresh").await;
    let ops = mint_role(&app, &seeded, "ops").await;
    owner_creates(
        &app,
        &seeded,
        grant("ops reach the db", role_subject(&ops), 9000),
    )
    .await;
    let (device_id, mut device) = online_device(&app, &seeded, "mach-trole-fresh-dev").await;
    let holder = person_with_role(&app, &seeded, "holder", "member").await;
    set_role(&app, &seeded, &ops, &holder.id, true).await;
    let (mut h, session) =
        client_session(&app, &seeded, &holder, "mach-trole-fresh-h", &device_id).await;

    request(&mut h, &session, 1, 9000).await;
    assert_forwarded(
        &mut h,
        &mut device,
        1,
        &holder.id,
        "while the member holds the role",
    )
    .await;

    set_role(&app, &seeded, &ops, &holder.id, false).await;
    request(&mut h, &session, 2, 9000).await;
    assert_acl_denied(&mut h, 2, "once the owner took the role back").await;

    set_role(&app, &seeded, &ops, &holder.id, true).await;
    request(&mut h, &session, 3, 9000).await;
    assert_forwarded(
        &mut h,
        &mut device,
        3,
        &holder.id,
        "once the owner gave it back",
    )
    .await;

    remove_from_org(&app, &seeded, &holder.id).await;
    request(&mut h, &session, 4, 9000).await;
    assert_acl_denied(
        &mut h,
        4,
        "once the member left the org, on the still-open socket",
    )
    .await;
}

/// A device forwarding as a tunnel client acts for its owner, so it forwards
/// on its owner's roles. The device is not removed with the person (a device
/// manager decides that), so the role read is what stops it the moment its
/// owner leaves the org.
#[tokio::test]
async fn a_device_forwards_on_its_owners_roles_until_its_owner_leaves_the_org() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("trole-device").await;
    let ops = mint_role(&app, &seeded, "ops").await;
    owner_creates(
        &app,
        &seeded,
        grant("ops reach the db", role_subject(&ops), 9000),
    )
    .await;
    let (target_id, mut target) = online_device(&app, &seeded, "mach-trole-device-dst").await;
    let holder = person_with_role(&app, &seeded, "holder", "member").await;
    set_role(&app, &seeded, &ops, &holder.id, true).await;

    // The origin device is the holder's. Its owner is read when it connects,
    // so the hand-over comes first.
    let (origin_id, origin_token) =
        enroll_agent(&app, &seeded, "mach-trole-device-src", "origin").await;
    let (status, body) = call(
        &app,
        "PUT",
        &format!("/api/tenant/{}/agent/{origin_id}", seeded.tenant_id),
        &seeded.admin.access_token,
        Some(json!({ "owner_user_id": holder.id })),
    )
    .await;
    assert_eq!(
        status, 200,
        "the owner hands the device to the holder: {body}"
    );
    let mut origin = connect_agent_ws(&app, &origin_token, "origin").await;
    wait_agent_online(&app, &seeded, &origin_id).await;
    let session = open_session(&mut origin, &target_id).await;

    request(&mut origin, &session, 1, 9000).await;
    assert_forwarded(
        &mut origin,
        &mut target,
        1,
        &holder.id,
        "a device whose owner holds the role",
    )
    .await;

    remove_from_org(&app, &seeded, &holder.id).await;
    request(&mut origin, &session, 2, 9000).await;
    assert_acl_denied(
        &mut origin,
        2,
        "the same device once its owner left the org",
    )
    .await;
}

/// Roles belong to one org. A policy here is matched against the roles the
/// person holds here, and only roles this org has count.
#[tokio::test]
async fn a_role_of_another_org_or_one_the_org_deleted_satisfies_no_policy_here() {
    let app = TestApp::spawn().await;
    let here = app.seed_tenant("trole-here").await;
    let there = app.seed_tenant("trole-there").await;
    let holder = person_with_role(&app, &here, "holder", "member").await;
    add_to_org(&app, &there, &holder, "member").await;

    // A role of the other org, held there; the test also writes its id into
    // the row here.
    let theirs = mint_role(&app, &there, "ops").await;
    set_role(&app, &there, &theirs, &holder.id, true).await;
    list_in_row(&app, &here, &holder, &theirs).await;
    // Two roles of this org, both held here; one is deleted further down.
    let ours = mint_role(&app, &here, "ops").await;
    let doomed = mint_role(&app, &here, "contractors").await;
    set_role(&app, &here, &ours, &holder.id, true).await;
    set_role(&app, &here, &doomed, &holder.id, true).await;

    owner_creates(
        &app,
        &here,
        grant("another org's role", role_subject(&theirs), 9001),
    )
    .await;
    owner_creates(&app, &here, grant("our role", role_subject(&ours), 9002)).await;
    owner_creates(
        &app,
        &here,
        grant("a role we delete", role_subject(&doomed), 9003),
    )
    .await;
    let (device_id, mut device) = online_device(&app, &here, "mach-trole-xorg-dev").await;
    let (mut h, session) =
        client_session(&app, &here, &holder, "mach-trole-xorg-h", &device_id).await;

    request(&mut h, &session, 1, 9002).await;
    assert_forwarded(
        &mut h,
        &mut device,
        1,
        &holder.id,
        "a role of this org, held here",
    )
    .await;

    request(&mut h, &session, 2, 9001).await;
    assert_acl_denied(&mut h, 2, "a role of another org").await;

    request(&mut h, &session, 3, 9003).await;
    assert_forwarded(
        &mut h,
        &mut device,
        3,
        &holder.id,
        "before the org deletes the role",
    )
    .await;

    let (status, body) = call(
        &app,
        "DELETE",
        &format!("/api/tenant/{}/role/{doomed}", here.tenant_id),
        &here.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "the owner deletes the role: {body}");
    // The row lists the id (written directly) and the policy still names it:
    // the forward must not count a role the org no longer has.
    list_in_row(&app, &here, &holder, &doomed).await;
    request(&mut h, &session, 4, 9003).await;
    assert_acl_denied(&mut h, 4, "a role the org deleted").await;
}

/// The subjects that never read a role forward exactly as before, for a caller
/// who holds no role a policy names: their user id, their tunnel client's id,
/// all users, and a device's own id for that device. A policy naming a role
/// AND the caller's user id still forwards for them, through the user id; the
/// id-specific kinds never cross (a device id does not match a tunnel client).
#[tokio::test]
async fn the_other_subject_kinds_forward_as_before() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("trole-others").await;
    let ops = mint_role(&app, &seeded, "ops").await;
    let (target_id, mut target) = online_device(&app, &seeded, "mach-trole-others-dst").await;
    let caller = &seeded.member;
    let (client_id, client_token) =
        enroll_client(&app, &seeded, caller, "mach-trole-others-c").await;
    let (origin_id, origin_token) =
        enroll_agent(&app, &seeded, "mach-trole-others-src", "origin").await;

    let user = json!([{ "kind": "user_id", "id": caller.id }]);
    let client = json!([{ "kind": "tunnel_client_id", "id": client_id }]);
    let everyone = json!([{ "kind": "all_users" }]);
    let role_or_user =
        json!([{ "kind": "role_id", "id": ops }, { "kind": "user_id", "id": caller.id }]);
    let origin_device = json!([{ "kind": "agent_id", "id": origin_id }]);
    owner_creates(&app, &seeded, grant("the caller", user, 9001)).await;
    owner_creates(&app, &seeded, grant("the caller's client", client, 9002)).await;
    owner_creates(&app, &seeded, grant("everyone", everyone, 9003)).await;
    owner_creates(
        &app,
        &seeded,
        grant("ops or the caller", role_or_user, 9004),
    )
    .await;
    owner_creates(
        &app,
        &seeded,
        grant("the origin device", origin_device, 9005),
    )
    .await;
    owner_creates(&app, &seeded, grant("ops only", role_subject(&ops), 9006)).await;

    let mut c = connect_tunnel_client(&app, &client_token).await;
    let session = open_session(&mut c, &target_id).await;
    for (flow, port, what) in [
        (1, 9001, "a user subject"),
        (2, 9002, "a tunnel-client subject"),
        (3, 9003, "all users"),
        (4, 9004, "a role-or-user policy, through the user"),
    ] {
        request(&mut c, &session, flow, port).await;
        assert_forwarded(&mut c, &mut target, flow, &caller.id, what).await;
    }
    request(&mut c, &session, 5, 9005).await;
    assert_acl_denied(&mut c, 5, "a device subject never matches a tunnel client").await;
    request(&mut c, &session, 6, 9006).await;
    assert_acl_denied(&mut c, 6, "a role the caller does not hold").await;

    // A device, owned by the org's owner who enrolled it, under its own id.
    let mut origin = connect_agent_ws(&app, &origin_token, "origin").await;
    wait_agent_online(&app, &seeded, &origin_id).await;
    let origin_session = open_session(&mut origin, &target_id).await;
    request(&mut origin, &origin_session, 7, 9005).await;
    assert_forwarded(
        &mut origin,
        &mut target,
        7,
        &seeded.admin.id,
        "a device subject, for that device",
    )
    .await;
    request(&mut origin, &origin_session, 8, 9002).await;
    assert_acl_denied(
        &mut origin,
        8,
        "a tunnel-client subject never matches a device",
    )
    .await;
}

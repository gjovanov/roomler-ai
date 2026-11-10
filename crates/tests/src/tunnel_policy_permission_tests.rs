// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! An org's tunnel policies are read and written under `MANAGE_AGENTS`, the bit
//! its overlay ACL already sits behind.
//!
//! A tunnel policy is an access grant: it names who may reach which `host:port`
//! through which of the org's devices. Writing one is a device-management
//! action, and the org's whole tunnel ACL (who may reach what, and the org's
//! internal hosts and ports) is read by the same people:
//!
//! - every route (list, get, create, update, delete) decides the bit first, so a
//!   member without it gets one 403 `forbidden` whatever the id names or the
//!   body says, and never `not_a_member`, the one 403 the web app leaves the org
//!   on; someone outside the org is still told `not_a_member`;
//! - for a holder the policy resolves within the path org: another org's id, or
//!   one that names nothing, is a 404;
//! - the forward itself checks no permission bit. The policy is the grant: a
//!   member a holder's policy names forwards, and a member no policy names is
//!   refused `acl_denied`, whatever they tried to write.

use std::time::Duration;

use bson::{doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use roomler_ai_db::models::role::permissions::MANAGE_AGENTS;
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::fixtures::seed::{SeededTenant, SeededUser};
use crate::fixtures::test_app::TestApp;
use crate::tunnel_tests::{connect_agent_ws, enroll_agent, read_until, wait_agent_online};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

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

fn policies(seeded: &SeededTenant) -> String {
    format!("/api/tenant/{}/tunnel-policy", seeded.tenant_id)
}

fn policy(seeded: &SeededTenant, policy_id: &str) -> String {
    format!("/api/tenant/{}/tunnel-policy/{policy_id}", seeded.tenant_id)
}

/// A grant of the loopback range's high ports on every device of the org, to
/// one person.
fn grant_to(user_id: &str, name: &str) -> Value {
    json!({
        "name": name,
        "subjects": [{ "kind": "user_id", "id": user_id }],
        "targets": [{ "kind": "all_agents" }],
        "allowlist": [{
            "host_pattern": { "kind": "cidr", "value": "127.0.0.0/8" },
            "port_range": { "low": 9000, "high": 9999 }
        }],
    })
}

/// The owner writes a policy; returns its id.
async fn owner_creates(app: &TestApp, seeded: &SeededTenant, body: Value) -> String {
    let (status, created) = call(
        app,
        "POST",
        &policies(seeded),
        &seeded.admin.access_token,
        Some(body),
    )
    .await;
    assert_eq!(status, 201, "the owner writes a policy: {created}");
    created["id"].as_str().expect("policy id").to_string()
}

/// One more person in the org, holding the org's own managed role `role`
/// (`member`, `moderator`, `admin`) and nothing else. Seeded the way the
/// fixture seeds its member.
async fn person_with_role(
    app: &TestApp,
    seeded: &SeededTenant,
    tag: &str,
    role: &str,
) -> SeededUser {
    let slug = &seeded.tenant_slug;
    let user = app
        .register_user(
            &format!("{tag}@{slug}.test"),
            &format!("{slug}_{tag}"),
            &format!("{slug} {tag}"),
            "Person123!",
            None,
            None,
        )
        .await;
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
    user
}

/// The owner mints a custom role with `permissions` and gives it to `user`.
async fn grant_role(app: &TestApp, seeded: &SeededTenant, user: &SeededUser, permissions: u64) {
    let owner = &seeded.admin.access_token;
    let (status, role) = call(
        app,
        "POST",
        &format!("/api/tenant/{}/role", seeded.tenant_id),
        owner,
        Some(json!({ "name": "device-admin", "permissions": permissions })),
    )
    .await;
    assert_eq!(status, 200, "the owner mints a role: {role}");
    let role_id = role["id"].as_str().unwrap();
    let (status, body) = call(
        app,
        "POST",
        &format!(
            "/api/tenant/{}/role/{role_id}/assign/{}",
            seeded.tenant_id, user.id
        ),
        owner,
        None,
    )
    .await;
    assert_eq!(status, 200, "the owner assigns it: {body}");
}

/// The refusal a member without the bit gets: a 403 that names the
/// permission, never `not_a_member`.
fn assert_refused(what: &str, status: u16, body: &Value) {
    assert_eq!(status, 403, "{what}: {body}");
    assert_eq!(body["error"], "forbidden", "{what}: {body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("MANAGE_AGENTS")),
        "{what}: the refusal names the permission: {body}"
    );
}

/// The heart of it. A plain member, and a moderator (who holds
/// `REMOTE_CONTROL` and so sees the Network pages), may neither grant
/// themselves a forward, nor take over an existing grant, nor remove one.
/// Nothing either of them sent is written.
#[tokio::test]
async fn a_member_without_manage_agents_writes_no_tunnel_policy() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tpperm-write").await;
    let moderator = person_with_role(&app, &seeded, "mod", "moderator").await;
    let existing = owner_creates(
        &app,
        &seeded,
        grant_to(&seeded.admin.id, "the owner's own grant"),
    )
    .await;

    for who in [&seeded.member, &moderator] {
        let token = &who.access_token;
        let (s, body) = call(
            &app,
            "POST",
            &policies(&seeded),
            token,
            Some(grant_to(&who.id, "a grant to myself")),
        )
        .await;
        assert_refused("create a grant to myself", s, &body);

        let (s, body) = call(
            &app,
            "PUT",
            &policy(&seeded, &existing),
            token,
            Some(json!({ "subjects": [{ "kind": "user_id", "id": who.id }] })),
        )
        .await;
        assert_refused("move an existing grant to myself", s, &body);

        let (s, body) = call(&app, "DELETE", &policy(&seeded, &existing), token, None).await;
        assert_refused("remove an existing grant", s, &body);
    }

    // Nothing they sent was written: the one policy is the owner's, as the
    // owner wrote it.
    let (s, list) = call(
        &app,
        "GET",
        &policies(&seeded),
        &seeded.admin.access_token,
        None,
    )
    .await;
    assert_eq!(s, 200, "{list}");
    assert_eq!(list["total"], 1, "no policy was created: {list}");
    let only = &list["items"][0];
    assert_eq!(only["id"], existing.as_str(), "{list}");
    let subjects = only["subjects"].as_array().expect("subjects");
    assert_eq!(
        subjects.len(),
        1,
        "the grant still names one subject: {list}"
    );
    assert_eq!(subjects[0]["kind"], "user_id", "{list}");
    assert_eq!(
        oid(&subjects[0]["id"]),
        Some(seeded.admin.id.as_str()),
        "the grant still names the owner: {list}"
    );
}

/// An id as a policy answers it: the `ObjectId` crosses the wire as
/// `{"$oid": "<hex>"}`, though a request may name it as the bare hex.
fn oid(v: &Value) -> Option<&str> {
    v.as_str().or_else(|| v["$oid"].as_str())
}

/// The org's tunnel ACL names who may reach which device and which internal
/// hosts and ports. A member without the bit reads none of it: neither the
/// list nor one policy.
#[tokio::test]
async fn a_member_without_manage_agents_reads_no_tunnel_policy() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tpperm-read").await;
    let existing = owner_creates(
        &app,
        &seeded,
        grant_to(&seeded.admin.id, "the owner's own grant"),
    )
    .await;
    let member = &seeded.member.access_token;

    let (s, body) = call(&app, "GET", &policies(&seeded), member, None).await;
    assert_refused("list the org's policies", s, &body);
    let (s, body) = call(&app, "GET", &policy(&seeded, &existing), member, None).await;
    assert_refused("read one policy", s, &body);
}

/// The bit is decided before the policy is looked up and before the body is
/// read. A member without it gets the same 403 for a policy of their own org,
/// for another org's policy, for an id that names nothing, and for a body that
/// would not validate, so a refusal says nothing about which policies exist.
/// Someone outside the org is told just that, on every route.
#[tokio::test]
async fn the_bit_is_decided_before_the_policy_or_the_body_is_read() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tpperm-order").await;
    let other = app.seed_tenant("tpperm-order-b").await;
    let mine = owner_creates(&app, &seeded, grant_to(&seeded.admin.id, "mine")).await;
    let theirs = owner_creates(&app, &other, grant_to(&other.admin.id, "theirs")).await;
    let member = &seeded.member.access_token;
    let bogus = ObjectId::new().to_hex();

    for id in [mine.as_str(), theirs.as_str(), bogus.as_str()] {
        let one = policy(&seeded, id);
        let (s, body) = call(&app, "GET", &one, member, None).await;
        assert_refused(&format!("GET {id}"), s, &body);
        let (s, body) = call(&app, "PUT", &one, member, Some(json!({ "name": "x" }))).await;
        assert_refused(&format!("PUT {id}"), s, &body);
        let (s, body) = call(&app, "DELETE", &one, member, None).await;
        assert_refused(&format!("DELETE {id}"), s, &body);
    }
    // A body the route would answer 400 to, from a member without the bit.
    let invalid = json!({
        "name": "",
        "subjects": [],
        "targets": [],
        "allowlist": [],
    });
    let (s, body) = call(&app, "POST", &policies(&seeded), member, Some(invalid)).await;
    assert_refused("POST an invalid body", s, &body);
    let (s, body) = call(
        &app,
        "PUT",
        &policy(&seeded, &mine),
        member,
        Some(json!({ "subjects": [] })),
    )
    .await;
    assert_refused("PUT an invalid body", s, &body);

    // Another org's owner holds MANAGE_AGENTS, in their own org only.
    let outsider = &other.admin.access_token;
    let requests = [
        ("GET", policies(&seeded), None),
        (
            "POST",
            policies(&seeded),
            Some(grant_to(&other.admin.id, "in")),
        ),
        ("GET", policy(&seeded, &mine), None),
        ("PUT", policy(&seeded, &mine), Some(json!({ "name": "x" }))),
        ("DELETE", policy(&seeded, &mine), None),
    ];
    for (method, path, body) in requests {
        let (s, answer) = call(&app, method, &path, outsider, body).await;
        assert_eq!(s, 403, "{method} {path}: {answer}");
        assert_eq!(answer["error"], "not_a_member", "{method} {path}: {answer}");
    }
}

/// The people who manage the org's devices manage its tunnel policies: a
/// member holding MANAGE_AGENTS through a role of the org's own making, and a
/// member holding the org's managed `admin` role. For them a policy resolves
/// within the path org: another org's, or an id that names nothing, is a 404.
#[tokio::test]
async fn holders_of_manage_agents_manage_tunnel_policies() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tpperm-holder").await;
    let other = app.seed_tenant("tpperm-holder-b").await;
    let theirs = owner_creates(&app, &other, grant_to(&other.admin.id, "theirs")).await;
    let org_admin = person_with_role(&app, &seeded, "orgadmin", "admin").await;
    grant_role(&app, &seeded, &seeded.member, MANAGE_AGENTS).await;

    for who in [&seeded.member, &org_admin] {
        let token = &who.access_token;
        let (s, created) = call(
            &app,
            "POST",
            &policies(&seeded),
            token,
            Some(grant_to(&who.id, "a holder's grant")),
        )
        .await;
        assert_eq!(s, 201, "a holder creates: {created}");
        let id = created["id"].as_str().unwrap().to_string();

        let (s, list) = call(&app, "GET", &policies(&seeded), token, None).await;
        assert_eq!(s, 200, "a holder lists: {list}");
        assert!(
            list["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(|p| p["id"] == id.as_str())),
            "{list}"
        );
        let (s, one) = call(&app, "GET", &policy(&seeded, &id), token, None).await;
        assert_eq!(s, 200, "a holder reads one: {one}");

        let (s, updated) = call(
            &app,
            "PUT",
            &policy(&seeded, &id),
            token,
            Some(json!({ "name": "renamed" })),
        )
        .await;
        assert_eq!(s, 200, "a holder updates: {updated}");
        assert_eq!(updated["name"], "renamed", "{updated}");

        let (s, body) = call(&app, "DELETE", &policy(&seeded, &id), token, None).await;
        assert_eq!(s, 204, "a holder removes: {body}");
        let (s, body) = call(&app, "GET", &policy(&seeded, &id), token, None).await;
        assert_eq!(s, 404, "a removed policy answers nothing: {body}");

        // Another org's policy, named under this org, and an id that names
        // nothing: 404 on every route.
        for elsewhere in [theirs.clone(), ObjectId::new().to_hex()] {
            let one = policy(&seeded, &elsewhere);
            let (s, body) = call(&app, "GET", &one, token, None).await;
            assert_eq!(s, 404, "GET {elsewhere}: {body}");
            let (s, body) = call(&app, "PUT", &one, token, Some(json!({ "name": "x" }))).await;
            assert_eq!(s, 404, "PUT {elsewhere}: {body}");
            let (s, body) = call(&app, "DELETE", &one, token, None).await;
            assert_eq!(s, 404, "DELETE {elsewhere}: {body}");
        }
    }

    // The other org's policy is untouched.
    let (s, body) = call(
        &app,
        "GET",
        &policy(&other, &theirs),
        &other.admin.access_token,
        None,
    )
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["name"], "theirs", "{body}");
}

/// Enroll a tunnel client the way `roomler enroll` does, with an enrollment
/// token minted by `who` (membership is enough to mint one: it enrolls the
/// minter's own machine). Returns the client's token.
async fn enroll_own_tunnel_client(
    app: &TestApp,
    seeded: &SeededTenant,
    who: &SeededUser,
    machine_id: &str,
) -> String {
    let (s, et) = call(
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
    assert_eq!(s, 200, "a member mints a tunnel enrollment token: {et}");
    let enrolled: Value = app
        .client
        .post(app.url("/api/tunnel-client/enroll"))
        .json(&json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": machine_id,
            "machine_name": "a member's laptop",
            "os": "linux",
            "client_version": "0.3.0-test",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    enrolled["tunnel_client_token"]
        .as_str()
        .expect("tunnel_client_token")
        .to_string()
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

async fn send(ws: &mut Ws, frame: Value) {
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// The first frame of type `t` for `flow_id` within 5 s; frames of other types
/// and flows are skipped.
async fn frame_for_flow(ws: &mut Ws, t: &str, flow_id: u64) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&text)
                    && v["t"] == t
                    && v["flow_id"].as_u64() == Some(flow_id)
                {
                    return Some(v);
                }
            }
            Ok(Some(Ok(_))) | Err(_) => continue,
            Ok(Some(Err(_))) | Ok(None) => return None,
        }
    }
    None
}

/// The forward path, end to end. A member's own tunnel client forwards only
/// what a holder's policy grants: the member's attempt to grant it themselves
/// is refused and the forward stays `acl_denied`; once the owner names the
/// member in a policy, the same client's forward reaches the device, under
/// the member's identity. The forward reads no permission bit, by design: the
/// policy IS the grant.
#[tokio::test]
async fn a_member_cannot_grant_their_own_tunnel_client_a_forward() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("tpperm-fwd").await;
    let member = &seeded.member;

    let (target_id, target_token) = enroll_agent(&app, &seeded, "mach-tpperm-fwd", "target").await;
    let mut target = connect_agent_ws(&app, &target_token, "target").await;
    wait_agent_online(&app, &seeded, &target_id).await;

    let client_token = enroll_own_tunnel_client(&app, &seeded, member, "mach-tpperm-laptop").await;
    let mut client = connect_tunnel_client(&app, &client_token).await;
    send(
        &mut client,
        json!({
            "t": "rc:tunnel.hello",
            "role": "client",
            "version": "0.3.0",
            "supported_transports": ["webrtc-dc-v1"],
        }),
    )
    .await;
    send(
        &mut client,
        json!({ "t": "rc:tunnel.open", "agent_id": target_id, "transport": "webrtc-dc-v1" }),
    )
    .await;
    let opened = read_until(&mut client, "rc:tunnel.opened")
        .await
        .expect("the member's client opens a session to the device");
    let session_id = opened["session_id"].as_str().unwrap().to_string();
    let request = |flow_id: u32| {
        json!({
            "t": "rc:tunnel.tcp.request",
            "session_id": session_id,
            "flow_id": flow_id,
            "dst_host": "127.0.0.1",
            "dst_port": 9000,
        })
    };

    // No policy names the member: default deny.
    send(&mut client, request(1)).await;
    let refused = frame_for_flow(&mut client, "rc:tunnel.tcp.reject", 1)
        .await
        .expect("an ungranted forward is refused");
    assert_eq!(refused["kind"], "acl_denied", "{refused}");

    // The member tries to grant it themselves, and is refused...
    let (s, body) = call(
        &app,
        "POST",
        &policies(&seeded),
        &member.access_token,
        Some(grant_to(&member.id, "my own way in")),
    )
    .await;
    assert_refused("a member grants themselves a forward", s, &body);
    // ...so the forward is still refused.
    send(&mut client, request(2)).await;
    let refused = frame_for_flow(&mut client, "rc:tunnel.tcp.reject", 2)
        .await
        .expect("the forward is still refused after the member's own attempt");
    assert_eq!(refused["kind"], "acl_denied", "{refused}");

    // The owner names the member in a policy: the same client's forward now
    // reaches the device, on the member's behalf.
    owner_creates(&app, &seeded, grant_to(&member.id, "the owner's grant")).await;
    send(&mut client, request(3)).await;
    let forwarded = frame_for_flow(&mut target, "rc:tunnel.tcp.forward", 3)
        .await
        .expect("a forward a holder's policy grants reaches the device");
    assert_eq!(forwarded["session_id"], session_id.as_str(), "{forwarded}");
    assert_eq!(
        forwarded["owner_user_id"],
        member.id.as_str(),
        "{forwarded}"
    );
    assert_eq!(forwarded["dst_port"], 9000, "{forwarded}");
}

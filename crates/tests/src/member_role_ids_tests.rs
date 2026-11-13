// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! A member's roles in an org are the roles OF THAT ORG their membership row
//! lists, read where a rule is evaluated; a role id is written only from the
//! org's own roles; deleting a role takes it from the people who held it.
//!
//! Readers. A row can list an id that is no role of the org (one deleted
//! since, or another org's), so every reader keeps only the org's live roles:
//! - the permission mask (`GET …/member/me`, every gated route): another
//!   org's role adds no bit;
//! - the exec, SSH and remote-control allow-lists of a device: a role deleted
//!   since, or another org's, matches no entry;
//! - the overlay ACL: a role selector matches neither, and a change of who
//!   holds a role re-fans the org, so the nodes follow at once;
//! - the members list shows only the roles that count.
//!
//! Writers. Assigning a role, adding a member, inviting, redeeming an invite,
//! and every rule or policy that names a role take only the org's own roles,
//! for every caller, an org's administrator included. A policy that already
//! names a role deleted since still saves, and keeps the id (an empty role
//! allow-list means "no restriction by role"). Deleting a role pulls it from
//! its holders' rows.
//!
//! Positive control: an owner, an administrator, a plain member and an
//! invitee keep exactly the permissions their roles give, for roles made by
//! each creation path.

use std::collections::BTreeSet;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::{doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use roomler_ai_db::models::role::permissions;
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::fixtures::seed::{SeededTenant, SeededUser};
use crate::fixtures::test_app::TestApp;
use crate::tunnel_policy_permission_tests::{Ws, call, person_with_role};
use crate::tunnel_tests::{connect_agent_ws, enroll_agent, wait_agent_online};

// ─── helpers ────────────────────────────────────────────────────────────────

fn oid(hex: &str) -> ObjectId {
    ObjectId::parse_str(hex).unwrap()
}

fn tenant_url(seeded: &SeededTenant, tail: &str) -> String {
    format!("/api/tenant/{}{tail}", seeded.tenant_id)
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// The org's owner mints a role carrying `perms`; its id.
async fn mint_role(app: &TestApp, seeded: &SeededTenant, name: &str, perms: u64) -> String {
    let (status, role) = call(
        app,
        "POST",
        &tenant_url(seeded, "/role"),
        &seeded.admin.access_token,
        Some(json!({ "name": name, "permissions": perms })),
    )
    .await;
    assert_eq!(status, 200, "the owner mints {name}: {role}");
    role["id"].as_str().expect("role id").to_string()
}

/// The owner gives `user_id` the role; the status and body.
async fn assign(app: &TestApp, seeded: &SeededTenant, role: &str, user_id: &str) -> (u16, Value) {
    call(
        app,
        "POST",
        &tenant_url(seeded, &format!("/role/{role}/assign/{user_id}")),
        &seeded.admin.access_token,
        None,
    )
    .await
}

async fn assign_ok(app: &TestApp, seeded: &SeededTenant, role: &str, user_id: &str) {
    let (status, body) = assign(app, seeded, role, user_id).await;
    assert_eq!(status, 200, "the owner assigns {role} to {user_id}: {body}");
}

async fn unassign_ok(app: &TestApp, seeded: &SeededTenant, role: &str, user_id: &str) {
    let (status, body) = call(
        app,
        "DELETE",
        &tenant_url(seeded, &format!("/role/{role}/assign/{user_id}")),
        &seeded.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "the owner takes {role} from {user_id}: {body}");
}

/// The owner deletes the role through the route; the status and body.
async fn delete_role(app: &TestApp, seeded: &SeededTenant, role: &str) -> (u16, Value) {
    call(
        app,
        "DELETE",
        &tenant_url(seeded, &format!("/role/{role}")),
        &seeded.admin.access_token,
        None,
    )
    .await
}

/// Remove the role's row straight from the database, as no route does: the
/// membership rows that list it keep the id. What a reader makes of such a
/// row is what is under test.
async fn drop_role_row(app: &TestApp, role: &str) {
    let gone = app
        .db
        .collection::<bson::Document>("roles")
        .delete_one(doc! { "_id": oid(role) })
        .await
        .unwrap();
    assert_eq!(gone.deleted_count, 1, "the role row {role} is gone");
}

/// Write `role` into `user_id`'s membership row of `seeded`'s org directly.
async fn list_in_row(app: &TestApp, seeded: &SeededTenant, user_id: &str, role: &str) {
    let r = app
        .db
        .collection::<bson::Document>("tenant_members")
        .update_one(
            doc! { "tenant_id": oid(&seeded.tenant_id), "user_id": oid(user_id) },
            doc! { "$addToSet": { "role_ids": oid(role) } },
        )
        .await
        .unwrap();
    assert_eq!(r.matched_count, 1, "{user_id} is a member of the org");
}

/// The role ids `user_id`'s membership row of `seeded`'s org lists, or `None`
/// when there is no row.
async fn row_role_ids(app: &TestApp, seeded: &SeededTenant, user_id: &str) -> Option<Vec<String>> {
    let row = app
        .db
        .collection::<bson::Document>("tenant_members")
        .find_one(doc! { "tenant_id": oid(&seeded.tenant_id), "user_id": oid(user_id) })
        .await
        .unwrap()?;
    Some(
        row.get_array("role_ids")
            .unwrap()
            .iter()
            .map(|b| b.as_object_id().unwrap().to_hex())
            .collect(),
    )
}

/// The caller's permission mask in the org, as `GET …/member/me` reports it.
async fn my_permissions(app: &TestApp, seeded: &SeededTenant, token: &str) -> u64 {
    let (status, me) = call(app, "GET", &tenant_url(seeded, "/member/me"), token, None).await;
    assert_eq!(status, 200, "member/me: {me}");
    me["permissions"].as_u64().expect("permissions")
}

/// The OR of the masks of the `roles` rows named `names` in the org.
async fn masks_of(app: &TestApp, seeded: &SeededTenant, names: &[&str]) -> u64 {
    let mut mask = 0u64;
    for name in names {
        let row = app
            .db
            .collection::<bson::Document>("roles")
            .find_one(doc! { "tenant_id": oid(&seeded.tenant_id), "name": *name })
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("the org has no `{name}` role"));
        mask |= match row.get("permissions") {
            Some(bson::Bson::Int64(v)) => *v as u64,
            Some(bson::Bson::Int32(v)) => *v as u64,
            other => panic!("{name}'s mask: {other:?}"),
        };
    }
    mask
}

/// The id of the org's managed role `name`.
async fn role_id_named(app: &TestApp, seeded: &SeededTenant, name: &str) -> String {
    app.db
        .collection::<bson::Document>("roles")
        .find_one(doc! { "tenant_id": oid(&seeded.tenant_id), "name": name })
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("the org has no `{name}` role"))
        .get_object_id("_id")
        .unwrap()
        .to_hex()
}

/// A fresh account, not a member of any org.
async fn outsider(app: &TestApp, tag: &str) -> SeededUser {
    app.register_user(
        &format!("{tag}@outside.test"),
        &format!("outside_{tag}"),
        &format!("Outside {tag}"),
        "Person123!",
        None,
        None,
    )
    .await
}

/// Set one field of a device's row straight in the database: a stored policy
/// written before writes were checked.
async fn set_agent_field(app: &TestApp, agent_id: &str, field: &str, value: bson::Bson) {
    let r = app
        .db
        .collection::<bson::Document>("agents")
        .update_one(
            doc! { "_id": oid(agent_id) },
            doc! { "$set": { field: value } },
        )
        .await
        .unwrap();
    assert_eq!(r.matched_count, 1, "the device row {agent_id}");
}

fn oid_array(ids: &[&str]) -> bson::Bson {
    bson::Bson::Array(ids.iter().map(|i| bson::Bson::ObjectId(oid(i))).collect())
}

/// A 400 that names the unknown role.
fn assert_unknown_role(what: &str, status: u16, body: &Value, role: &str) {
    assert_eq!(status, 400, "{what}: {body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("Unknown role id") && m.contains(role)),
        "{what}: the refusal names the role: {body}"
    );
}

/// A device of the org, enrolled and never connected.
async fn offline_device(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> String {
    enroll_agent(app, seeded, machine_id, machine_id).await.0
}

// ─── exec / SSH ────────────────────────────────────────────────────────────

async fn exec_on(app: &TestApp, seeded: &SeededTenant, on: bool) {
    let (status, body) = call(
        app,
        "PUT",
        &tenant_url(seeded, "/exec-settings"),
        &seeded.admin.access_token,
        Some(json!({ "remote_exec_enabled": on })),
    )
    .await;
    assert_eq!(status, 200, "exec settings: {body}");
}

async fn ssh_on(app: &TestApp, seeded: &SeededTenant) {
    let (status, body) = call(
        app,
        "PUT",
        &tenant_url(seeded, "/ssh-settings"),
        &seeded.admin.access_token,
        Some(json!({ "remote_ssh_enabled": true })),
    )
    .await;
    assert_eq!(status, 200, "ssh settings: {body}");
}

/// `who` asks the device to run a command; the reason the newest exec audit
/// row of that device gives (`None` = it ran).
async fn exec_answer(
    app: &TestApp,
    seeded: &SeededTenant,
    who: &SeededUser,
    agent_id: &str,
) -> Option<String> {
    let (status, out) = call(
        app,
        "POST",
        &tenant_url(seeded, &format!("/agent/{agent_id}/exec")),
        &who.access_token,
        Some(json!({ "command": "echo hello", "timeout_ms": 5_000 })),
    )
    .await;
    assert_eq!(status, 200, "a refusal is an answer, not an error: {out}");
    let (status, audit) = call(
        app,
        "GET",
        &tenant_url(seeded, "/exec-audit"),
        &seeded.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "exec audit: {audit}");
    let row = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["agent_id"] == agent_id && r["user_id"] == who.id.as_str())
        .unwrap_or_else(|| panic!("an exec audit row for {agent_id}: {audit}"))
        .clone();
    row["denied"].as_str().map(str::to_string)
}

/// An OpenSSH ed25519 public key; the server checks its structure only.
fn ssh_key() -> String {
    let mut raw = Vec::with_capacity(51);
    raw.extend_from_slice(&11u32.to_be_bytes());
    raw.extend_from_slice(b"ssh-ed25519");
    raw.extend_from_slice(&32u32.to_be_bytes());
    raw.extend_from_slice(&[7u8; 32]);
    format!("ssh-ed25519 {} caller", BASE64.encode(raw))
}

/// `who` asks for an SSH grant; the reason the newest SSH audit row of that
/// device gives (`None` = granted).
async fn ssh_answer(
    app: &TestApp,
    seeded: &SeededTenant,
    who: &SeededUser,
    agent_id: &str,
) -> Option<String> {
    let _ = call(
        app,
        "POST",
        &tenant_url(seeded, &format!("/agent/{agent_id}/ssh")),
        &who.access_token,
        Some(json!({ "public_key": ssh_key() })),
    )
    .await;
    let (status, audit) = call(
        app,
        "GET",
        &tenant_url(seeded, "/ssh-audit"),
        &seeded.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "ssh audit: {audit}");
    let row = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["agent_id"] == agent_id && r["user_id"] == who.id.as_str())
        .unwrap_or_else(|| panic!("an ssh audit row for {agent_id}: {audit}"))
        .clone();
    row["denied"].as_str().map(str::to_string)
}

// ─── remote control ───────────────────────────────────────────────────────

/// `who` asks the device for a session over a controller socket; the frame
/// that answers (`rc:session.created` or `rc:error`).
async fn rc_answer(app: &TestApp, who: &SeededUser, agent_id: &str) -> Value {
    let url = format!(
        "ws://{}/ws?token={}",
        app.addr,
        urlencode(&who.access_token)
    );
    let (mut ctl, _) = connect_async(&url).await.expect("controller ws");
    let _ = tokio::time::timeout(Duration::from_millis(200), ctl.next()).await;
    ctl.send(Message::Text(
        json!({ "t": "rc:session.request", "agent_id": agent_id, "permissions": "VIEW" })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(250), ctl.next()).await
        else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if v["t"] == "rc:session.created" || v["t"] == "rc:error" {
            return v;
        }
    }
    panic!("no answer to the session request");
}

fn assert_rc_allowed(what: &str, answer: &Value) {
    assert_eq!(answer["t"], "rc:session.created", "{what}: {answer}");
}

fn assert_rc_not_on_allowlist(what: &str, answer: &Value) {
    assert_eq!(answer["t"], "rc:error", "{what}: {answer}");
    assert_eq!(answer["code"], "permission_denied", "{what}: {answer}");
    assert!(
        answer["message"]
            .as_str()
            .is_some_and(|m| m.contains("allowlist")),
        "{what}: refused by the device's allow-list: {answer}"
    );
}

// ─── overlay ──────────────────────────────────────────────────────────────

async fn recv_any(ws: &mut Ws, wait: Duration) -> Option<Value> {
    match tokio::time::timeout(wait, ws.next()).await {
        Ok(Some(Ok(Message::Text(txt)))) => serde_json::from_str::<Value>(&txt).ok(),
        Ok(Some(Ok(_))) => Some(Value::Null),
        _ => None,
    }
}

/// Discard everything queued until the socket is quiet: an ACL edit or a
/// role change re-fans every node, and those frames must not be read as the
/// answer to what comes next.
async fn settle(ws: &mut Ws) {
    while recv_any(ws, Duration::from_millis(1_500)).await.is_some() {}
}

/// (Re-)join the overlay and return the netmap that answers it.
async fn join(ws: &mut Ws, seed: u8) -> Value {
    settle(ws).await;
    let msg = json!({
        "t": "rc:overlay.join",
        "wg_public_key": BASE64.encode([seed; 32]),
        "mtu": 1280,
        "endpoints": [format!("203.0.113.{seed}:41641")],
        "supports_server_relay_strategy": true,
        "org_primary": true,
    });
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .expect("send join");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "a netmap answers the join");
        if let Some(v) = recv_any(ws, left).await
            && v["t"] == "rc:overlay.netmap"
        {
            return v;
        }
    }
}

fn peer_ids(netmap: &Value) -> BTreeSet<String> {
    netmap["peers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["node_id"].as_str().unwrap().to_string())
        .collect()
}

/// The first `rc:overlay.netmap_delta` within `wait` that removes `node`
/// (`removed`), or upserts it (`!removed`).
async fn delta_for(ws: &mut Ws, node: &str, removed: bool, wait: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        let v = recv_any(ws, left).await?;
        if v["t"] != "rc:overlay.netmap_delta" {
            continue;
        }
        let hit = if removed {
            v["removes"]
                .as_array()
                .is_some_and(|r| r.iter().any(|n| n == node))
        } else {
            v["upserts"]
                .as_array()
                .is_some_and(|u| u.iter().any(|p| p["node_id"] == node))
        };
        if hit {
            return Some(v);
        }
    }
}

async fn node_id_of(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> String {
    app.state
        .network()
        .overlay_nodes
        .find_live_by_tenant_and_machine(oid(&seeded.tenant_id), machine_id)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no live node for {machine_id}"))
        .id
        .unwrap()
        .to_hex()
}

/// Two devices on the overlay of one org under `enforce`: A, owned by a
/// member `holder`, and B, the owner's. One rule lets the holders of `ops`
/// reach B; `holder` holds `ops`.
struct Mesh {
    app: TestApp,
    seeded: SeededTenant,
    holder: SeededUser,
    ops: String,
    rule: String,
    a: Ws,
    b: Ws,
    b_node: String,
}

impl Mesh {
    async fn up(slug: &str) -> Self {
        let app = TestApp::spawn().await;
        let seeded = app.seed_tenant(slug).await;
        let holder = person_with_role(&app, &seeded, "holder", "member").await;
        let ops = mint_role(&app, &seeded, "ops", 0).await;
        assign_ok(&app, &seeded, &ops, &holder.id).await;

        let (a_id, a_tok) = enroll_agent(&app, &seeded, &format!("{slug}-a"), "alpha").await;
        let (b_id, b_tok) = enroll_agent(&app, &seeded, &format!("{slug}-b"), "bravo").await;
        // A is the holder's device: role selectors match a node by its owner.
        let (status, body) = call(
            &app,
            "PUT",
            &tenant_url(&seeded, &format!("/agent/{a_id}")),
            &seeded.admin.access_token,
            Some(json!({ "owner_user_id": holder.id })),
        )
        .await;
        assert_eq!(status, 200, "A goes to the holder: {body}");
        let mut a = connect_agent_ws(&app, &a_tok, "alpha").await;
        let mut b = connect_agent_ws(&app, &b_tok, "bravo").await;
        wait_agent_online(&app, &seeded, &a_id).await;
        wait_agent_online(&app, &seeded, &b_id).await;
        join(&mut a, 1).await;
        join(&mut b, 2).await;
        let b_node = node_id_of(&app, &seeded, &format!("{slug}-b")).await;

        let (status, body) = call(
            &app,
            "PUT",
            &tenant_url(&seeded, "/overlay-acl/mode"),
            &seeded.admin.access_token,
            Some(json!({ "mode": "enforce" })),
        )
        .await;
        assert_eq!(status, 200, "enforce: {body}");
        let (status, rule) = call(
            &app,
            "POST",
            &tenant_url(&seeded, "/overlay-acl"),
            &seeded.admin.access_token,
            Some(json!({
                "name": "ops reach bravo",
                "sources": [{ "kind": "role_id", "id": ops }],
                "via": [{ "kind": "node_id", "id": b_node }],
                "destinations": [{ "cidr": "100.64.0.0/10" }],
            })),
        )
        .await;
        assert_eq!(status, 200, "the rule: {rule}");
        let rule = rule["id"].as_str().unwrap().to_string();
        Self {
            app,
            seeded,
            holder,
            ops,
            rule,
            a,
            b,
            b_node,
        }
    }
}

// ─── readers ───────────────────────────────────────────────────────────────

/// Another org's role in a member's row adds no permission bit in this org,
/// in the mask a member reads and at a gated route.
#[tokio::test]
async fn a_role_of_another_org_grants_no_permission_here() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-perm").await;
    let other = app.seed_tenant("mrid-perm-other").await;
    let foreign = mint_role(&app, &other, "fleet-admins", permissions::MANAGE_AGENTS).await;

    let before = my_permissions(&app, &org, &org.member.access_token).await;
    assert_eq!(
        before & permissions::MANAGE_AGENTS,
        0,
        "control: {before:#x}"
    );
    list_in_row(&app, &org, &org.member.id, &foreign).await;

    assert_eq!(
        my_permissions(&app, &org, &org.member.access_token).await,
        before,
        "another org's role adds no bit here"
    );
    let (status, body) = call(
        &app,
        "GET",
        &tenant_url(&org, "/tunnel-policy"),
        &org.member.access_token,
        None,
    )
    .await;
    assert_eq!(status, 403, "MANAGE_AGENTS is not held here: {body}");
}

/// Positive control for the tenant-scoped mask: the owner, an administrator, a
/// plain member, a holder of a custom role and an invitee each hold exactly
/// the OR of their roles' masks, for roles from both creation paths (the
/// org's seeded roles and `POST …/role`) and after an invite is redeemed.
#[tokio::test]
async fn every_member_keeps_the_permissions_their_roles_give() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-keep").await;

    // The owner: the seeded `owner` role (the org's creation).
    assert_eq!(
        my_permissions(&app, &org, &org.admin.access_token).await,
        masks_of(&app, &org, &["owner"]).await,
        "the owner"
    );
    // A plain member and an administrator: seeded managed roles.
    assert_eq!(
        my_permissions(&app, &org, &org.member.access_token).await,
        masks_of(&app, &org, &["member"]).await,
        "a plain member"
    );
    let admin = person_with_role(&app, &org, "admin2", "admin").await;
    assert_eq!(
        my_permissions(&app, &org, &admin.access_token).await,
        masks_of(&app, &org, &["admin"]).await,
        "an administrator"
    );
    // A custom role (`POST …/role`) on top of `member`.
    let custom_bits = permissions::MANAGE_AGENTS | permissions::REMOTE_CONTROL;
    let custom = mint_role(&app, &org, "devices", custom_bits).await;
    let holder = person_with_role(&app, &org, "holder", "member").await;
    assign_ok(&app, &org, &custom, &holder.id).await;
    assert_eq!(
        my_permissions(&app, &org, &holder.access_token).await,
        masks_of(&app, &org, &["member", "devices"]).await,
        "a custom role's holder"
    );
    // An invitee, through the invite's roles.
    let moderator = role_id_named(&app, &org, "moderator").await;
    let (status, invite) = call(
        &app,
        "POST",
        &tenant_url(&org, "/invite"),
        &org.admin.access_token,
        Some(json!({ "assign_role_ids": [moderator, custom] })),
    )
    .await;
    assert_eq!(status, 201, "the invite: {invite}");
    let invitee = outsider(&app, "keep-invitee").await;
    let (status, body) = call(
        &app,
        "POST",
        &format!("/api/invite/{}/accept", invite["code"].as_str().unwrap()),
        &invitee.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "the invitee joins: {body}");
    assert_eq!(
        my_permissions(&app, &org, &invitee.access_token).await,
        masks_of(&app, &org, &["moderator", "devices"]).await,
        "an invitee"
    );
}

/// The exec allow-list of a device: a role deleted since, or another org's,
/// matches no entry, while a live role of the org still does. The device is
/// offline, so a caller past the allow-list is refused `offline` instead.
#[tokio::test]
async fn exec_a_deleted_or_foreign_role_matches_no_allow_list_entry() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-exec").await;
    let other = app.seed_tenant("mrid-exec-other").await;
    exec_on(&app, &org, true).await;
    let device = offline_device(&app, &org, "mach-mrid-exec").await;
    let caller = person_with_role(&app, &org, "caller", "member").await;
    let operators = mint_role(&app, &org, "operators", permissions::EXEC_DEVICE).await;
    let ops = mint_role(&app, &org, "ops", 0).await;
    assign_ok(&app, &org, &operators, &caller.id).await;
    assign_ok(&app, &org, &ops, &caller.id).await;
    let (status, body) = call(
        &app,
        "PUT",
        &tenant_url(&org, &format!("/agent/{device}/exec-policy")),
        &org.admin.access_token,
        Some(json!({ "mode": "on", "consent_mode": "auto", "allowed_role_ids": [ops] })),
    )
    .await;
    assert_eq!(status, 200, "exec policy: {body}");

    assert_eq!(
        exec_answer(&app, &org, &caller, &device).await.as_deref(),
        Some("offline"),
        "control: a holder of the listed role passes the allow-list"
    );

    drop_role_row(&app, &ops).await;
    assert_eq!(
        exec_answer(&app, &org, &caller, &device).await.as_deref(),
        Some("caller_not_allowed"),
        "a role the org no longer has matches no entry"
    );

    let foreign = mint_role(&app, &other, "ops", 0).await;
    set_agent_field(
        &app,
        &device,
        "exec_policy.allowed_role_ids",
        oid_array(&[&foreign]),
    )
    .await;
    list_in_row(&app, &org, &caller.id, &foreign).await;
    assert_eq!(
        exec_answer(&app, &org, &caller, &device).await.as_deref(),
        Some("caller_not_allowed"),
        "another org's role matches no entry here"
    );
}

/// The SSH allow-list of a device: the same rule as exec's.
#[tokio::test]
async fn ssh_a_deleted_or_foreign_role_matches_no_allow_list_entry() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-ssh").await;
    let other = app.seed_tenant("mrid-ssh-other").await;
    ssh_on(&app, &org).await;
    let device = offline_device(&app, &org, "mach-mrid-ssh").await;
    let caller = person_with_role(&app, &org, "caller", "member").await;
    let operators = mint_role(&app, &org, "operators", permissions::SSH_DEVICE).await;
    let ops = mint_role(&app, &org, "ops", 0).await;
    assign_ok(&app, &org, &operators, &caller.id).await;
    assign_ok(&app, &org, &ops, &caller.id).await;
    let (status, body) = call(
        &app,
        "PUT",
        &tenant_url(&org, &format!("/agent/{device}/ssh-policy")),
        &org.admin.access_token,
        Some(json!({
            "mode": "on", "account_mode": "daemon", "consent_mode": "auto",
            "allowed_role_ids": [ops],
        })),
    )
    .await;
    assert_eq!(status, 200, "ssh policy: {body}");

    let control = ssh_answer(&app, &org, &caller, &device).await;
    assert!(
        control.is_some() && control.as_deref() != Some("caller_not_allowed"),
        "control: a holder of the listed role passes the allow-list (refused later: {control:?})"
    );

    drop_role_row(&app, &ops).await;
    assert_eq!(
        ssh_answer(&app, &org, &caller, &device).await.as_deref(),
        Some("caller_not_allowed"),
        "a role the org no longer has matches no entry"
    );

    let foreign = mint_role(&app, &other, "ops", 0).await;
    set_agent_field(
        &app,
        &device,
        "ssh_policy.allowed_role_ids",
        oid_array(&[&foreign]),
    )
    .await;
    list_in_row(&app, &org, &caller.id, &foreign).await;
    assert_eq!(
        ssh_answer(&app, &org, &caller, &device).await.as_deref(),
        Some("caller_not_allowed"),
        "another org's role matches no entry here"
    );
}

/// The remote-control allow-list of a device: the same rule, at the session
/// request.
#[tokio::test]
async fn rc_a_deleted_or_foreign_role_matches_no_allow_list_entry() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-rc").await;
    let other = app.seed_tenant("mrid-rc-other").await;
    // Several pending sessions at once in one org: Free runs one.
    app.set_plan(&org.tenant_id, "business").await;
    let (device, device_tok) = enroll_agent(&app, &org, "mach-mrid-rc", "mrid-rc").await;
    let _device_ws = connect_agent_ws(&app, &device_tok, "mrid-rc").await;
    wait_agent_online(&app, &org, &device).await;
    let caller = person_with_role(&app, &org, "caller", "member").await;
    let controllers = mint_role(&app, &org, "controllers", permissions::REMOTE_CONTROL).await;
    let ops = mint_role(&app, &org, "ops", 0).await;
    assign_ok(&app, &org, &controllers, &caller.id).await;
    assign_ok(&app, &org, &ops, &caller.id).await;
    let (status, body) = call(
        &app,
        "PUT",
        &tenant_url(&org, &format!("/agent/{device}")),
        &org.admin.access_token,
        Some(json!({ "access_policy": { "allowed_role_ids": [ops] } })),
    )
    .await;
    assert_eq!(status, 200, "access policy: {body}");

    assert_rc_allowed(
        "control: a holder of the listed role",
        &rc_answer(&app, &caller, &device).await,
    );

    drop_role_row(&app, &ops).await;
    assert_rc_not_on_allowlist(
        "a role the org no longer has",
        &rc_answer(&app, &caller, &device).await,
    );

    let foreign = mint_role(&app, &other, "ops", 0).await;
    set_agent_field(
        &app,
        &device,
        "access_policy.allowed_role_ids",
        oid_array(&[&foreign]),
    )
    .await;
    list_in_row(&app, &org, &caller.id, &foreign).await;
    assert_rc_not_on_allowlist(
        "another org's role",
        &rc_answer(&app, &caller, &device).await,
    );
}

/// The overlay ACL, compiled at a join: a role selector matches a holder of a
/// live role of the org, and neither a role deleted since nor another org's.
#[tokio::test]
async fn overlay_a_deleted_or_foreign_role_matches_no_rule() {
    let mut m = Mesh::up("mrid-ovl").await;
    let other = m.app.seed_tenant("mrid-ovl-other").await;

    let netmap = join(&mut m.a, 1).await;
    assert_eq!(
        peer_ids(&netmap),
        BTreeSet::from([m.b_node.clone()]),
        "control: the holder's device reaches B: {netmap}"
    );

    drop_role_row(&m.app, &m.ops).await;
    let netmap = join(&mut m.a, 1).await;
    assert!(
        peer_ids(&netmap).is_empty(),
        "a role the org no longer has matches no rule: {netmap}"
    );

    // A stored rule naming another org's role, and the holder's row listing it.
    let foreign = mint_role(&m.app, &other, "ops", 0).await;
    let r = m
        .app
        .db
        .collection::<bson::Document>("overlay_policies")
        .update_one(
            doc! { "_id": oid(&m.rule) },
            doc! { "$set": { "sources": [{ "kind": "role_id", "id": oid(&foreign) }] } },
        )
        .await
        .unwrap();
    assert_eq!(r.matched_count, 1, "the rule row");
    list_in_row(&m.app, &m.seeded, &m.holder.id, &foreign).await;
    let netmap = join(&mut m.a, 1).await;
    assert!(
        peer_ids(&netmap).is_empty(),
        "another org's role matches no rule here: {netmap}"
    );
    drop(m.b);
}

/// A change of who holds a role re-fans the org: the holder's device is told
/// at once that B is gone when the role is taken, that B is back when it is
/// given again, and that B is gone when the role is deleted.
#[tokio::test]
async fn overlay_a_role_change_refans_the_org() {
    let mut m = Mesh::up("mrid-refan").await;
    let netmap = join(&mut m.a, 1).await;
    assert_eq!(peer_ids(&netmap), BTreeSet::from([m.b_node.clone()]));
    settle(&mut m.a).await;

    unassign_ok(&m.app, &m.seeded, &m.ops, &m.holder.id).await;
    assert!(
        delta_for(&mut m.a, &m.b_node, true, Duration::from_secs(8))
            .await
            .is_some(),
        "taking the role removes B from the holder's device"
    );
    settle(&mut m.a).await;

    assign_ok(&m.app, &m.seeded, &m.ops, &m.holder.id).await;
    assert!(
        delta_for(&mut m.a, &m.b_node, false, Duration::from_secs(8))
            .await
            .is_some(),
        "giving it back brings B back"
    );
    settle(&mut m.a).await;

    let (status, body) = delete_role(&m.app, &m.seeded, &m.ops).await;
    assert_eq!(status, 200, "the owner deletes ops: {body}");
    assert!(
        delta_for(&mut m.a, &m.b_node, true, Duration::from_secs(8))
            .await
            .is_some(),
        "deleting the role removes B"
    );
    drop(m.b);
}

/// The members list shows the roles that count: neither a role the org no
/// longer has nor another org's.
#[tokio::test]
async fn the_members_list_shows_only_the_orgs_roles() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-list").await;
    let other = app.seed_tenant("mrid-list-other").await;
    let gone = mint_role(&app, &org, "gone", 0).await;
    assign_ok(&app, &org, &gone, &org.member.id).await;
    drop_role_row(&app, &gone).await;
    let foreign = mint_role(&app, &other, "ops", 0).await;
    list_in_row(&app, &org, &org.member.id, &foreign).await;
    let member_role = role_id_named(&app, &org, "member").await;

    let (status, page) = call(
        &app,
        "GET",
        &tenant_url(&org, "/member"),
        &org.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{page}");
    let row = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["user_id"] == org.member.id.as_str())
        .unwrap_or_else(|| panic!("the member's row: {page}"));
    assert_eq!(row["role_ids"], json!([member_role]), "{row}");
}

// ─── writers ───────────────────────────────────────────────────────────────

/// An org's owner (ADMINISTRATOR) cannot give a member another org's role: a
/// 404, and the row is unchanged.
#[tokio::test]
async fn assigning_a_role_of_another_org_is_not_found_for_an_administrator_too() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-assign").await;
    let other = app.seed_tenant("mrid-assign-other").await;
    let foreign = mint_role(&app, &other, "ops", permissions::MANAGE_AGENTS).await;
    let before = row_role_ids(&app, &org, &org.member.id).await;

    let (status, body) = assign(&app, &org, &foreign, &org.member.id).await;
    assert_eq!(status, 404, "another org's role: {body}");
    assert_eq!(row_role_ids(&app, &org, &org.member.id).await, before);
}

/// Adding a member with another org's role is a 404 for the owner too, and
/// adds nobody.
#[tokio::test]
async fn adding_a_member_with_a_role_of_another_org_is_not_found() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-add").await;
    let other = app.seed_tenant("mrid-add-other").await;
    let foreign = mint_role(&app, &other, "ops", 0).await;
    let newcomer = outsider(&app, "add-newcomer").await;

    let (status, body) = call(
        &app,
        "POST",
        &tenant_url(&org, "/member"),
        &org.admin.access_token,
        Some(json!({ "user_id": newcomer.id, "role_ids": [foreign] })),
    )
    .await;
    assert_eq!(status, 404, "another org's role: {body}");
    assert!(row_role_ids(&app, &org, &newcomer.id).await.is_none());
}

/// An invite cannot carry another org's role, whoever writes it.
#[tokio::test]
async fn an_invite_with_a_role_of_another_org_is_not_found() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-inv").await;
    let other = app.seed_tenant("mrid-inv-other").await;
    let foreign = mint_role(&app, &other, "ops", 0).await;

    let (status, body) = call(
        &app,
        "POST",
        &tenant_url(&org, "/invite"),
        &org.admin.access_token,
        Some(json!({ "assign_role_ids": [foreign] })),
    )
    .await;
    assert_eq!(status, 404, "another org's role: {body}");
}

/// The owner makes an invite that gives `names` (custom roles); its code and
/// the roles' ids.
async fn invite_with(app: &TestApp, org: &SeededTenant, names: &[&str]) -> (String, Vec<String>) {
    let mut ids = Vec::new();
    for name in names {
        ids.push(mint_role(app, org, name, 0).await);
    }
    let (status, invite) = call(
        app,
        "POST",
        &tenant_url(org, "/invite"),
        &org.admin.access_token,
        Some(json!({ "assign_role_ids": ids })),
    )
    .await;
    assert_eq!(status, 201, "the invite: {invite}");
    (invite["code"].as_str().unwrap().to_string(), ids)
}

async fn invite_uses(app: &TestApp, code: &str) -> i64 {
    let row = app
        .db
        .collection::<bson::Document>("invites")
        .find_one(doc! { "code": code })
        .await
        .unwrap()
        .expect("the invite row");
    match row.get("use_count") {
        Some(bson::Bson::Int32(n)) => *n as i64,
        Some(bson::Bson::Int64(n)) => *n,
        other => panic!("use_count: {other:?}"),
    }
}

/// An invite whose roles are all gone by the time it is redeemed is refused
/// (409), joins nobody and is not spent: not turned into another grant.
#[tokio::test]
async fn an_invite_whose_roles_are_all_gone_is_refused_and_spends_nothing() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-gone").await;
    let (code, roles) = invite_with(&app, &org, &["guests"]).await;
    let (status, body) = delete_role(&app, &org, &roles[0]).await;
    assert_eq!(status, 200, "{body}");
    let invitee = outsider(&app, "gone-invitee").await;

    let (status, body) = call(
        &app,
        "POST",
        &format!("/api/invite/{code}/accept"),
        &invitee.access_token,
        None,
    )
    .await;
    assert_eq!(status, 409, "the invite's role no longer exists: {body}");
    assert!(row_role_ids(&app, &org, &invitee.id).await.is_none());
    assert_eq!(invite_uses(&app, &code).await, 0, "nothing is spent");
}

/// An invite that lost one of its roles joins with the ones the org still has.
#[tokio::test]
async fn an_invite_gives_only_the_roles_its_org_still_has() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-part").await;
    let (code, roles) = invite_with(&app, &org, &["guests", "helpers"]).await;
    let (status, body) = delete_role(&app, &org, &roles[0]).await;
    assert_eq!(status, 200, "{body}");
    let invitee = outsider(&app, "part-invitee").await;

    let (status, body) = call(
        &app,
        "POST",
        &format!("/api/invite/{code}/accept"),
        &invitee.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        row_role_ids(&app, &org, &invitee.id).await,
        Some(vec![roles[1].clone()])
    );
}

/// Registering with the code of an invite whose roles are all gone creates the
/// account and joins no org.
#[tokio::test]
async fn registering_with_an_invite_whose_roles_are_all_gone_joins_nothing() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-reg").await;
    let (code, roles) = invite_with(&app, &org, &["guests"]).await;
    let (status, body) = delete_role(&app, &org, &roles[0]).await;
    assert_eq!(status, 200, "{body}");

    let resp = app
        .client
        .post(app.url("/api/auth/register"))
        .json(&json!({
            "email": "reg-invitee@outside.test",
            "username": "reg_invitee",
            "display_name": "Reg Invitee",
            "password": "Person123!",
            "invite_code": code,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "the account is created");
    let user = app
        .db
        .collection::<bson::Document>("users")
        .find_one(doc! { "email": "reg-invitee@outside.test" })
        .await
        .unwrap()
        .expect("the user row");
    let uid = user.get_object_id("_id").unwrap().to_hex();
    assert!(
        row_role_ids(&app, &org, &uid).await.is_none(),
        "no membership from an invite whose roles are gone"
    );
    assert_eq!(invite_uses(&app, &code).await, 0, "nothing is spent");
}

/// The role ids a device's stored `policy` (`access_policy`, `exec_policy`,
/// `ssh_policy`) allows.
async fn stored_policy_roles(app: &TestApp, agent_id: &str, policy: &str) -> Vec<String> {
    let row = app
        .db
        .collection::<bson::Document>("agents")
        .find_one(doc! { "_id": oid(agent_id) })
        .await
        .unwrap()
        .expect("the device row");
    row.get_document(policy)
        .unwrap()
        .get_array("allowed_role_ids")
        .unwrap()
        .iter()
        .map(|b| b.as_object_id().unwrap().to_hex())
        .collect()
}

/// The owner saves the device's access policy with `roles`, the way the
/// device dialog sends it.
async fn put_access_roles(
    app: &TestApp,
    org: &SeededTenant,
    device: &str,
    roles: &[&str],
) -> (u16, Value) {
    call(
        app,
        "PUT",
        &tenant_url(org, &format!("/agent/{device}")),
        &org.admin.access_token,
        Some(json!({ "access_policy": { "allowed_role_ids": roles } })),
    )
    .await
}

/// The device's remote-control access policy names only the org's roles: a
/// new id of another org is a 400; one the stored policy already names is
/// saved back as it is after its role is deleted, and never pruned.
#[tokio::test]
async fn a_device_access_policy_names_only_the_orgs_roles() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-wacc").await;
    let other = app.seed_tenant("mrid-wacc-other").await;
    let foreign = mint_role(&app, &other, "ops", 0).await;
    let device = offline_device(&app, &org, "mach-mrid-wacc").await;
    let ops = mint_role(&app, &org, "ops", 0).await;

    let (status, body) = put_access_roles(&app, &org, &device, &[&foreign]).await;
    assert_unknown_role("another org's role", status, &body, &foreign);
    let (status, body) = put_access_roles(&app, &org, &device, &[&ops]).await;
    assert_eq!(status, 200, "the org's own role: {body}");
    let (status, body) = delete_role(&app, &org, &ops).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = put_access_roles(&app, &org, &device, &[&ops]).await;
    assert_eq!(status, 200, "a stored id is saved back: {body}");
    assert_eq!(
        stored_policy_roles(&app, &device, "access_policy").await,
        vec![ops.clone()],
        "never pruned"
    );
    let (status, body) = put_access_roles(&app, &org, &device, &[&ops, &foreign]).await;
    assert_unknown_role("a new foreign id beside it", status, &body, &foreign);
}

/// The exec policy: the same contract.
#[tokio::test]
async fn a_device_exec_policy_names_only_the_orgs_roles() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-wexec").await;
    let other = app.seed_tenant("mrid-wexec-other").await;
    let foreign = mint_role(&app, &other, "ops", 0).await;
    let device = offline_device(&app, &org, "mach-mrid-wexec").await;
    let ops = mint_role(&app, &org, "ops", 0).await;
    let path = tenant_url(&org, &format!("/agent/{device}/exec-policy"));
    let body_with = |roles: Vec<&str>| json!({ "mode": "on", "allowed_role_ids": roles });
    let owner = &org.admin.access_token;

    let (status, body) = call(&app, "PUT", &path, owner, Some(body_with(vec![&foreign]))).await;
    assert_unknown_role("another org's role", status, &body, &foreign);
    let (status, body) = call(&app, "PUT", &path, owner, Some(body_with(vec![&ops]))).await;
    assert_eq!(status, 200, "the org's own role: {body}");
    let (status, body) = delete_role(&app, &org, &ops).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = call(&app, "PUT", &path, owner, Some(body_with(vec![&ops]))).await;
    assert_eq!(status, 200, "a stored id is saved back: {body}");
    assert_eq!(
        stored_policy_roles(&app, &device, "exec_policy").await,
        vec![ops.clone()],
        "never pruned"
    );
    let (status, body) = call(
        &app,
        "PUT",
        &path,
        owner,
        Some(body_with(vec![&ops, &foreign])),
    )
    .await;
    assert_unknown_role("a new foreign id beside it", status, &body, &foreign);
}

/// The SSH policy: the same contract.
#[tokio::test]
async fn a_device_ssh_policy_names_only_the_orgs_roles() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-wssh").await;
    let other = app.seed_tenant("mrid-wssh-other").await;
    let foreign = mint_role(&app, &other, "ops", 0).await;
    let device = offline_device(&app, &org, "mach-mrid-wssh").await;
    let ops = mint_role(&app, &org, "ops", 0).await;
    let path = tenant_url(&org, &format!("/agent/{device}/ssh-policy"));
    let body_with = |roles: Vec<&str>| json!({ "mode": "on", "account_mode": "daemon", "allowed_role_ids": roles });
    let owner = &org.admin.access_token;

    let (status, body) = call(&app, "PUT", &path, owner, Some(body_with(vec![&foreign]))).await;
    assert_unknown_role("another org's role", status, &body, &foreign);
    let (status, body) = call(&app, "PUT", &path, owner, Some(body_with(vec![&ops]))).await;
    assert_eq!(status, 200, "the org's own role: {body}");
    let (status, body) = delete_role(&app, &org, &ops).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = call(&app, "PUT", &path, owner, Some(body_with(vec![&ops]))).await;
    assert_eq!(status, 200, "a stored id is saved back: {body}");
    assert_eq!(
        stored_policy_roles(&app, &device, "ssh_policy").await,
        vec![ops.clone()],
        "never pruned"
    );
    let (status, body) = call(
        &app,
        "PUT",
        &path,
        owner,
        Some(body_with(vec![&ops, &foreign])),
    )
    .await;
    assert_unknown_role("a new foreign id beside it", status, &body, &foreign);
}

/// A tunnel policy's role subject names a role of the org: create and update.
#[tokio::test]
async fn a_tunnel_policy_names_only_the_orgs_roles() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-wtun").await;
    let other = app.seed_tenant("mrid-wtun-other").await;
    let foreign = mint_role(&app, &other, "ops", 0).await;
    let ops = mint_role(&app, &org, "ops", 0).await;
    let grant = |roles: Vec<&str>| {
        json!({
            "name": "ops reach the db",
            "subjects": roles.iter().map(|r| json!({ "kind": "role_id", "id": r })).collect::<Vec<_>>(),
            "targets": [{ "kind": "all_agents" }],
            "allowlist": [{
                "host_pattern": { "kind": "cidr", "value": "127.0.0.0/8" },
                "port_range": { "low": 9000, "high": 9000 }
            }],
        })
    };
    let owner = &org.admin.access_token;
    let list = tenant_url(&org, "/tunnel-policy");

    let (status, body) = call(&app, "POST", &list, owner, Some(grant(vec![&foreign]))).await;
    assert_unknown_role("create: another org's role", status, &body, &foreign);
    let (status, created) = call(&app, "POST", &list, owner, Some(grant(vec![&ops]))).await;
    assert_eq!(status, 201, "the org's own role: {created}");
    let one = tenant_url(
        &org,
        &format!("/tunnel-policy/{}", created["id"].as_str().unwrap()),
    );
    let (status, body) = delete_role(&app, &org, &ops).await;
    assert_eq!(status, 200, "{body}");
    let subjects = |roles: Vec<&str>| json!({ "subjects": roles.iter().map(|r| json!({ "kind": "role_id", "id": r })).collect::<Vec<_>>() });
    let (status, body) = call(&app, "PUT", &one, owner, Some(subjects(vec![&ops]))).await;
    assert_eq!(status, 200, "update: a stored id is saved back: {body}");
    let (status, body) = call(
        &app,
        "PUT",
        &one,
        owner,
        Some(subjects(vec![&ops, &foreign])),
    )
    .await;
    assert_unknown_role("update: a new foreign id", status, &body, &foreign);
}

/// An overlay rule's role source names a role of the org: create and update.
#[tokio::test]
async fn an_overlay_rule_names_only_the_orgs_roles() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-wovl").await;
    let other = app.seed_tenant("mrid-wovl-other").await;
    let foreign = mint_role(&app, &other, "ops", 0).await;
    let ops = mint_role(&app, &org, "ops", 0).await;
    let rule = |roles: Vec<&str>| {
        json!({
            "name": "ops reach everything",
            "sources": roles.iter().map(|r| json!({ "kind": "role_id", "id": r })).collect::<Vec<_>>(),
            "via": [{ "kind": "all_nodes" }],
            "destinations": [{ "cidr": "100.64.0.0/10" }],
        })
    };
    let owner = &org.admin.access_token;
    let list = tenant_url(&org, "/overlay-acl");

    let (status, body) = call(&app, "POST", &list, owner, Some(rule(vec![&foreign]))).await;
    assert_unknown_role("create: another org's role", status, &body, &foreign);
    let (status, created) = call(&app, "POST", &list, owner, Some(rule(vec![&ops]))).await;
    assert_eq!(status, 200, "the org's own role: {created}");
    let one = tenant_url(
        &org,
        &format!("/overlay-acl/{}", created["id"].as_str().unwrap()),
    );
    let (status, body) = delete_role(&app, &org, &ops).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = call(&app, "PUT", &one, owner, Some(rule(vec![&ops]))).await;
    assert_eq!(status, 200, "update: a stored id is saved back: {body}");
    let (status, body) = call(&app, "PUT", &one, owner, Some(rule(vec![&ops, &foreign]))).await;
    assert_unknown_role("update: a new foreign id", status, &body, &foreign);
}

// ─── delete ────────────────────────────────────────────────────────────────

/// Deleting a role takes it from everyone who held it, and from nobody else's
/// other roles.
#[tokio::test]
async fn deleting_a_role_takes_it_from_its_holders() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-del").await;
    let ops = mint_role(&app, &org, "ops", permissions::MANAGE_AGENTS).await;
    let devs = mint_role(&app, &org, "devs", 0).await;
    let second = person_with_role(&app, &org, "second", "member").await;
    assign_ok(&app, &org, &ops, &org.member.id).await;
    assign_ok(&app, &org, &devs, &org.member.id).await;
    assign_ok(&app, &org, &ops, &second.id).await;
    let member_role = role_id_named(&app, &org, "member").await;

    let (status, body) = delete_role(&app, &org, &ops).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        row_role_ids(&app, &org, &org.member.id).await,
        Some(vec![member_role.clone(), devs.clone()])
    );
    assert_eq!(
        row_role_ids(&app, &org, &second.id).await,
        Some(vec![member_role])
    );
}

/// A role of another org is not found here: neither a custom role (once
/// answered as deleted although nothing was) nor a managed one (once a 403).
#[tokio::test]
async fn a_role_of_another_org_is_not_found_by_the_delete() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("mrid-delx").await;
    let other = app.seed_tenant("mrid-delx-other").await;
    let custom = mint_role(&app, &other, "ops", 0).await;
    let managed = role_id_named(&app, &other, "member").await;

    let (status, body) = delete_role(&app, &org, &custom).await;
    assert_eq!(status, 404, "another org's custom role: {body}");
    let (status, body) = delete_role(&app, &org, &managed).await;
    assert_eq!(status, 404, "another org's managed role: {body}");
    let still = app
        .db
        .collection::<bson::Document>("roles")
        .count_documents(doc! { "_id": { "$in": [oid(&custom), oid(&managed)] } })
        .await
        .unwrap();
    assert_eq!(still, 2, "the other org keeps both");
}

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! An enrollment credential acts for the person who minted it, so it works
//! only while that person may still enroll devices in its org.
//!
//! A single-use enrollment token (pasted, or delivered by a pairing) and an
//! FR-51 ephemeral key both act for the admin who minted them: the device
//! they enroll is that person's, and an anchor records them. So every
//! redemption asks the mint's own gate again (`MANAGE_AGENTS` in the org):
//! when the person has left the org, or no longer manages its devices, the
//! credential is refused with a plain 403 before anything is spent. The
//! token stays unspent, the key's `uses` stay as they were, and nothing is
//! written. When a person is removed from an org, the live keys they minted
//! there are revoked (fleet's membership hook); a key no removal reached is
//! revoked at its first refused use. Losing the permission while staying a
//! member only pauses a key. Devices the person enrolled are not touched.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use bson::{Bson, Document, doc, oid::ObjectId};
use serde_json::{Value, json};

use crate::fixtures::seed::{SeededTenant, SeededUser};
use crate::fixtures::test_app::TestApp;

/// `MANAGE_AGENTS` (`role::permissions`), as the role route takes it.
const MANAGE_AGENTS: u64 = 1 << 24;

/// What `/api/agent/enroll` and `/api/agent/anchor` answer for a token
/// whose issuer no longer manages the org's devices.
const TOKEN_REFUSED: &str = "This enrollment token was issued by someone who no longer \
     manages this organization's devices, so it cannot be used. Ask someone who manages the \
     organization's devices for a new enrollment token.";

/// What `/api/agent/enroll` answers for such a key.
const KEY_REFUSED: &str = "This enrollment key was minted by someone who no longer manages \
     this organization's devices, so it enrolls nothing. Mint a new key from an account that \
     manages the organization's devices.";

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

/// A call to a public route (the credential is in the body), as an agent
/// or an installer makes it.
async fn post_public(app: &TestApp, path: &str, body: Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(app.url(path))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
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

/// End a membership with nothing else running: the row goes, no hook runs
/// (a removal from before the hook existed, or one whose hook failed).
async fn end_membership_directly(app: &TestApp, org: &SeededTenant, user: &SeededUser) {
    let gone = app
        .db
        .collection::<Document>("tenant_members")
        .delete_one(doc! { "tenant_id": tid(org), "user_id": oid(&user.id) })
        .await
        .unwrap();
    assert_eq!(gone.deleted_count, 1);
}

/// A role holding only `MANAGE_AGENTS`, handed to `user`: the role id.
async fn grant_manage_agents(app: &TestApp, org: &SeededTenant, user: &SeededUser) -> String {
    let (status, role) = call(
        app,
        "POST",
        &format!("/api/tenant/{}/role", org.tenant_id),
        &org.admin.access_token,
        Some(json!({ "name": format!("devices-{}", user.username), "permissions": MANAGE_AGENTS })),
    )
    .await;
    assert!(status < 300, "create the role: {role}");
    let role_id = role["id"].as_str().unwrap().to_string();
    assign(app, org, &role_id, user).await;
    role_id
}

async fn assign(app: &TestApp, org: &SeededTenant, role_id: &str, user: &SeededUser) {
    let (status, body) = call(
        app,
        "POST",
        &format!(
            "/api/tenant/{}/role/{role_id}/assign/{}",
            org.tenant_id, user.id
        ),
        &org.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "assign the role: {body}");
}

async fn unassign(app: &TestApp, org: &SeededTenant, role_id: &str, user: &SeededUser) {
    let (status, body) = call(
        app,
        "DELETE",
        &format!(
            "/api/tenant/{}/role/{role_id}/assign/{}",
            org.tenant_id, user.id
        ),
        &org.admin.access_token,
        None,
    )
    .await;
    assert_eq!(status, 200, "unassign the role: {body}");
}

/// Edit a role's bits (`PUT …/role/{id}`): every member holding it changes.
async fn set_role_bits(app: &TestApp, org: &SeededTenant, role_id: &str, bits: u64) {
    let (status, body) = call(
        app,
        "PUT",
        &format!("/api/tenant/{}/role/{role_id}", org.tenant_id),
        &org.admin.access_token,
        Some(json!({ "permissions": bits })),
    )
    .await;
    assert_eq!(status, 200, "edit the role: {body}");
}

/// Mint a single-use enrollment token as `who`: `(token, jti)`.
async fn mint_token(
    app: &TestApp,
    org: &SeededTenant,
    who: &SeededUser,
    anchor: bool,
) -> (String, String) {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/tenant/{}/agent/enroll-token", org.tenant_id),
        &who.access_token,
        Some(json!({ "anchor": anchor })),
    )
    .await;
    assert_eq!(status, 200, "mint an enrollment token: {body}");
    assert_eq!(body["anchor"], json!(anchor), "{body}");
    (
        body["enrollment_token"].as_str().unwrap().to_string(),
        body["jti"].as_str().unwrap().to_string(),
    )
}

/// Turn the org's FR-51 switch on (the owner's `MANAGE_TENANT` route).
async fn enable_keys(app: &TestApp, org: &SeededTenant) {
    let (status, body) = call(
        app,
        "PUT",
        &format!("/api/tenant/{}/ephemeral-key-settings", org.tenant_id),
        &org.admin.access_token,
        Some(json!({ "ephemeral_keys_enabled": true })),
    )
    .await;
    assert_eq!(status, 200, "turn ephemeral keys on: {body}");
}

/// Mint an ephemeral key as `who`: `(key, key_id)`.
async fn mint_key(
    app: &TestApp,
    org: &SeededTenant,
    who: &SeededUser,
    max_uses: i64,
) -> (String, String) {
    let (status, body) = call(
        app,
        "POST",
        &format!("/api/tenant/{}/agent/enroll-key", org.tenant_id),
        &who.access_token,
        Some(json!({ "label": format!("ci-{}", who.username), "max_uses": max_uses })),
    )
    .await;
    assert_eq!(status, 200, "mint an enrollment key: {body}");
    (
        body["key"].as_str().unwrap().to_string(),
        body["id"].as_str().unwrap().to_string(),
    )
}

/// `POST /api/agent/enroll`, the body `roomlerd enroll` sends.
async fn enroll(app: &TestApp, credential: &str, machine_id: &str, os: &str) -> (u16, Value) {
    post_public(
        app,
        "/api/agent/enroll",
        json!({
            "enrollment_token": credential,
            "machine_id": machine_id,
            "machine_name": format!("HOST-{machine_id}"),
            "os": os,
            "agent_version": "0.4.208",
        }),
    )
    .await
}

/// Enroll a device that must enroll: its agent id.
async fn enroll_ok(app: &TestApp, credential: &str, machine_id: &str, os: &str) -> String {
    let (status, body) = enroll(app, credential, machine_id, os).await;
    assert_eq!(status, 200, "enroll {machine_id}: {body}");
    body["agent_id"].as_str().unwrap().to_string()
}

/// `POST /api/agent/anchor` for the device `machine_id` enrolled as.
async fn anchor(app: &TestApp, token: &str, agent_id: &str, machine_id: &str) -> (u16, Value) {
    post_public(
        app,
        "/api/agent/anchor",
        json!({
            "enrollment_token": token,
            "agent_id": agent_id,
            "host_name": format!("HOST-{machine_id}"),
        }),
    )
    .await
}

/// The refusal both doors answer: a plain `forbidden` with the sentence,
/// never `not_a_member` (the code the dashboard leaves the org on).
fn assert_refused(status: u16, body: &Value, message: &str) {
    assert_eq!(status, 403, "refused: {body}");
    assert_eq!(body["error"], "forbidden", "never not_a_member: {body}");
    assert_eq!(body["message"], message, "{body}");
}

/// The used-token ledger's record of a spend under `key`, if it has one.
async fn ledger_use(app: &TestApp, key: &str) -> Option<Document> {
    app.db
        .collection::<Document>("used_tokens")
        .find_one(doc! { "_id": key })
        .await
        .unwrap()
}

/// The jti of a JWT, read from its payload (a delivered token comes alone).
fn jti_of(token: &str) -> String {
    let payload = token.split('.').nth(1).expect("a JWT");
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    claims["jti"].as_str().unwrap().to_string()
}

async fn agent_by_machine(app: &TestApp, org: &SeededTenant, machine_id: &str) -> Option<Document> {
    app.db
        .collection::<Document>("agents")
        .find_one(doc! { "tenant_id": tid(org), "machine_id": machine_id })
        .await
        .unwrap()
}

async fn agent_row(app: &TestApp, agent_id: &str) -> Document {
    app.db
        .collection::<Document>("agents")
        .find_one(doc! { "_id": oid(agent_id) })
        .await
        .unwrap()
        .expect("the device row")
}

/// A device row that is live and still `owner`'s.
fn assert_live_and_owned(row: &Document, owner: &SeededUser) {
    assert!(
        matches!(row.get("deleted_at"), None | Some(Bson::Null)),
        "the device is live: {row}"
    );
    assert_eq!(
        row.get_object_id("owner_user_id").unwrap().to_hex(),
        owner.id,
        "the device is still theirs"
    );
}

async fn key_row(app: &TestApp, key_id: &str) -> Document {
    app.db
        .collection::<Document>("enrollment_keys")
        .find_one(doc! { "_id": oid(key_id) })
        .await
        .unwrap()
        .expect("the key row")
}

fn revoked_at(row: &Document) -> Option<bson::DateTime> {
    row.get_datetime("revoked_at").ok().copied()
}

fn uses(row: &Document) -> i64 {
    row.get_i64("uses").unwrap()
}

// ── the single-use enrollment token ─────────────────────────────────────────

/// A token minted by an admin who then leaves the org enrolls nothing: 403,
/// the token unspent, no device written. The device they enrolled before is
/// still theirs, and the owner's token enrolls as always.
#[tokio::test]
async fn a_token_whose_issuer_left_the_org_enrolls_nothing_and_stays_unspent() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("iss-tok").await;
    let admin2 = add_member(&app, &org, "admin2").await;
    grant_manage_agents(&app, &org, &admin2).await;
    let (before, _) = mint_token(&app, &org, &admin2, false).await;
    let theirs = enroll_ok(&app, &before, "m-tok-before", "linux").await;
    let (token, jti) = mint_token(&app, &org, &admin2, false).await;

    remove(&app, &org, &admin2).await;

    let (status, body) = enroll(&app, &token, "m-tok-after", "linux").await;
    assert_refused(status, &body, TOKEN_REFUSED);
    assert!(
        ledger_use(&app, &jti).await.is_none(),
        "the refusal spent nothing"
    );
    assert!(
        agent_by_machine(&app, &org, "m-tok-after").await.is_none(),
        "the refusal wrote nothing"
    );
    assert_live_and_owned(&agent_row(&app, &theirs).await, &admin2);

    let (owners, _) = mint_token(&app, &org, &org.admin, false).await;
    enroll_ok(&app, &owners, "m-tok-owner", "linux").await;
}

/// Losing Manage devices while staying a member pauses a token: refused and
/// unspent while the role is gone, and it enrolls once the role is back.
#[tokio::test]
async fn a_token_pauses_while_its_issuer_lacks_manage_devices() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("iss-demote").await;
    let admin2 = add_member(&app, &org, "admin2").await;
    let role = grant_manage_agents(&app, &org, &admin2).await;
    let (token, jti) = mint_token(&app, &org, &admin2, false).await;

    unassign(&app, &org, &role, &admin2).await;

    let (status, body) = enroll(&app, &token, "m-demote", "linux").await;
    assert_refused(status, &body, TOKEN_REFUSED);
    assert!(ledger_use(&app, &jti).await.is_none(), "still unspent");
    assert!(agent_by_machine(&app, &org, "m-demote").await.is_none());

    assign(&app, &org, &role, &admin2).await;
    let agent = enroll_ok(&app, &token, "m-demote", "linux").await;
    assert!(
        ledger_use(&app, &jti).await.is_some(),
        "spent by its one use"
    );
    assert_live_and_owned(&agent_row(&app, &agent).await, &admin2);
}

/// A token a pairing delivered acts for its approver, so it enrolls nothing
/// once the approver has left the org.
#[tokio::test]
async fn a_paired_token_enrolls_nothing_once_its_approver_left_the_org() {
    let app = TestApp::spawn_with_settings(|s| s.pairing.poll_interval_secs = 0).await;
    let org = app.seed_tenant("iss-pair").await;
    let approver = add_member(&app, &org, "approver").await;
    grant_manage_agents(&app, &org, &approver).await;

    let (status, started) = post_public(
        &app,
        "/api/pair/start",
        json!({
            "machine_name": "pairing test laptop",
            "os": "linux",
            "arch": "x86_64",
            "role": "daemon",
            "client": "wizard",
            "client_version": "0.4.208",
        }),
    )
    .await;
    assert_eq!(status, 200, "start: {started}");
    let (status, approved) = call(
        &app,
        "POST",
        "/api/pair/approve",
        &approver.access_token,
        Some(json!({
            "user_code": started["user_code"],
            "tenant_id": org.tenant_id,
        })),
    )
    .await;
    assert_eq!(status, 200, "approve: {approved}");
    let (status, polled) = post_public(
        &app,
        "/api/pair/poll",
        json!({ "device_code": started["device_code"] }),
    )
    .await;
    assert_eq!(status, 200, "poll: {polled}");
    assert_eq!(polled["status"], "approved", "{polled}");
    let token = polled["tokens"][0].as_str().unwrap().to_string();

    remove(&app, &org, &approver).await;

    let (status, body) = enroll(&app, &token, "m-paired", "linux").await;
    assert_refused(status, &body, TOKEN_REFUSED);
    assert!(
        ledger_use(&app, &jti_of(&token)).await.is_none(),
        "the delivered token is unspent"
    );
    assert!(agent_by_machine(&app, &org, "m-paired").await.is_none());
}

/// The anchor redeems the same token, so it asks the same question: a plain
/// token, and an anchoring token's second use, of an issuer who left the org
/// anchor nothing and spend nothing. The owner's token still anchors.
#[tokio::test]
async fn an_anchor_by_a_token_whose_issuer_left_the_org_is_refused_and_unspent() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("iss-anchor").await;
    let admin2 = add_member(&app, &org, "admin2").await;
    grant_manage_agents(&app, &org, &admin2).await;

    let (owners, _) = mint_token(&app, &org, &org.admin, false).await;
    let device = enroll_ok(&app, &owners, "m-anchor-w", "windows").await;
    let (plain, plain_jti) = mint_token(&app, &org, &admin2, false).await;
    let (anchoring, anchoring_jti) = mint_token(&app, &org, &admin2, true).await;
    let theirs = enroll_ok(&app, &anchoring, "m-anchor-theirs", "windows").await;

    remove(&app, &org, &admin2).await;

    let (status, body) = anchor(&app, &plain, &device, "m-anchor-w").await;
    assert_refused(status, &body, TOKEN_REFUSED);
    assert!(
        ledger_use(&app, &plain_jti).await.is_none(),
        "the plain token is unspent"
    );
    assert!(
        matches!(
            agent_row(&app, &device).await.get("exec_anchor"),
            None | Some(Bson::Null)
        ),
        "the device is not anchored"
    );

    let (status, body) = anchor(&app, &anchoring, &theirs, "m-anchor-theirs").await;
    assert_refused(status, &body, TOKEN_REFUSED);
    assert!(
        ledger_use(&app, &format!("{anchoring_jti}:anchor"))
            .await
            .is_none(),
        "the anchoring token's second use is unspent"
    );
    let theirs_row = agent_row(&app, &theirs).await;
    assert!(
        matches!(theirs_row.get("exec_anchor"), None | Some(Bson::Null)),
        "their device is not anchored"
    );
    assert_live_and_owned(&theirs_row, &admin2);

    let (fresh, _) = mint_token(&app, &org, &org.admin, false).await;
    let (status, body) = anchor(&app, &fresh, &device, "m-anchor-w").await;
    assert_eq!(status, 200, "the owner's token anchors: {body}");
}

// ── the FR-51 ephemeral key ─────────────────────────────────────────────────

/// A removal revokes the live keys the person minted in that org, and their
/// key enrolls nothing (403, `uses` unchanged). A second run of the cascade
/// changes nothing. A key that was used up keeps how it ended; the owner's
/// key, the person's key in another org and the device they enrolled are
/// untouched.
#[tokio::test]
async fn a_key_whose_minter_left_the_org_is_revoked_and_enrolls_nothing() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("iss-key").await;
    let other = app.seed_tenant("iss-key-b").await;
    // The other org's owner also manages this org's devices.
    let admin2 = &other.admin;
    readd(&app, &org, admin2).await;
    grant_manage_agents(&app, &org, admin2).await;
    enable_keys(&app, &org).await;
    enable_keys(&app, &other).await;

    let (live, live_id) = mint_key(&app, &org, admin2, 5).await;
    let (used_up, used_up_id) = mint_key(&app, &org, admin2, 1).await;
    let (owners, owners_id) = mint_key(&app, &org, &org.admin, 5).await;
    let (elsewhere, elsewhere_id) = mint_key(&app, &other, admin2, 5).await;
    let theirs = enroll_ok(&app, &used_up, "m-key-before", "linux").await;

    remove(&app, &org, admin2).await;

    let (status, body) = enroll(&app, &live, "m-key-after", "linux").await;
    assert_refused(status, &body, KEY_REFUSED);
    let row = key_row(&app, &live_id).await;
    assert_eq!(uses(&row), 0, "the refusal claimed no use");
    let revoked = revoked_at(&row).expect("the removal revoked the key");
    assert!(agent_by_machine(&app, &org, "m-key-after").await.is_none());

    let used_up_row = key_row(&app, &used_up_id).await;
    assert_eq!(uses(&used_up_row), 1);
    assert!(
        revoked_at(&used_up_row).is_none(),
        "a used-up key keeps how it ended"
    );
    assert!(revoked_at(&key_row(&app, &owners_id).await).is_none());
    assert!(
        revoked_at(&key_row(&app, &elsewhere_id).await).is_none(),
        "the removal is this org's only"
    );
    assert_live_and_owned(&agent_row(&app, &theirs).await, admin2);

    // The cascade run again finds nothing left.
    app.state
        .core
        .hooks
        .org_member_removed(tid(&org), oid(&admin2.id))
        .await
        .expect("a second run of the cascade");
    assert_eq!(
        revoked_at(&key_row(&app, &live_id).await),
        Some(revoked),
        "the revocation time is not re-stamped"
    );

    enroll_ok(&app, &owners, "m-key-owner", "linux").await;
    enroll_ok(&app, &elsewhere, "m-key-elsewhere", "linux").await;
}

/// A key whose minter lost Manage devices but stayed a member is refused and
/// NOT revoked: it is paused, and enrolls again once the role holds the bit.
#[tokio::test]
async fn a_key_pauses_while_its_minters_role_lacks_manage_devices() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("iss-key-demote").await;
    let admin2 = add_member(&app, &org, "admin2").await;
    let role = grant_manage_agents(&app, &org, &admin2).await;
    enable_keys(&app, &org).await;
    let (key, key_id) = mint_key(&app, &org, &admin2, 5).await;

    set_role_bits(&app, &org, &role, 0).await;

    let (status, body) = enroll(&app, &key, "m-key-paused", "linux").await;
    assert_refused(status, &body, KEY_REFUSED);
    let row = key_row(&app, &key_id).await;
    assert_eq!(uses(&row), 0, "the refusal claimed no use");
    assert!(revoked_at(&row).is_none(), "a demotion does not revoke");

    set_role_bits(&app, &org, &role, MANAGE_AGENTS).await;
    enroll_ok(&app, &key, "m-key-paused", "linux").await;
    assert_eq!(uses(&key_row(&app, &key_id).await), 1);
}

/// A key whose minter is no longer a member, but which no removal revoked (a
/// membership that ended without the hook), is revoked at its first refused
/// use, so a later re-add does not bring it back.
#[tokio::test]
async fn a_key_no_removal_reached_is_revoked_at_its_first_refused_use() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("iss-key-lazy").await;
    let admin2 = add_member(&app, &org, "admin2").await;
    let role = grant_manage_agents(&app, &org, &admin2).await;
    enable_keys(&app, &org).await;
    let (key, key_id) = mint_key(&app, &org, &admin2, 5).await;

    end_membership_directly(&app, &org, &admin2).await;
    assert!(
        revoked_at(&key_row(&app, &key_id).await).is_none(),
        "no hook ran"
    );

    let (status, body) = enroll(&app, &key, "m-key-lazy", "linux").await;
    assert_refused(status, &body, KEY_REFUSED);
    let row = key_row(&app, &key_id).await;
    assert_eq!(uses(&row), 0, "the refusal claimed no use");
    assert!(
        revoked_at(&row).is_some(),
        "revoked at its first refused use"
    );

    // Back in the org, with the role again: the key stays revoked.
    readd(&app, &org, &admin2).await;
    assign(&app, &org, &role, &admin2).await;
    let (status, body) = enroll(&app, &key, "m-key-lazy", "linux").await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(
        body["message"], "This enrollment key was refused (revoked)",
        "{body}"
    );
    assert_eq!(uses(&key_row(&app, &key_id).await), 0);
    assert!(agent_by_machine(&app, &org, "m-key-lazy").await.is_none());
}

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3328 — a tunnel enrollment token enrolls once, and re-enrolling a machine
//! id that already has a tunnel client in the org revives that client only for
//! its owner or for a holder of `MANAGE_AGENTS`.
//!
//! `POST /api/tunnel-client/enroll` decides in this order, and these tests pin
//! the order as much as the answers:
//!
//! 1. the token is verified;
//! 2. the archived-org gate (403);
//! 3. a known machine id: may the token's subject re-enroll it? (409) — a new
//!    one: the plan's tunnel-client cap (403);
//! 4. the token is spent on the used-token ledger (401 already used, or a
//!    retryable 503 when the ledger cannot answer);
//! 5. the row is written and the client's token minted.
//!
//! Every refusal before step 4 leaves the token unspent, and no refusal writes
//! anything: no row, no ledger entry, no overlay change.

use bson::{Bson, Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use roomler_ai_remote_control::models::NodeRef;
use roomler_ai_services::dao::overlay_network::OverlayNetworkDao;
use roomler_ai_services::dao::overlay_node::{NewOverlayNode, OverlayNodeDao};
use serde_json::{Value, json};

use crate::fixtures::seed::{SeededTenant, SeededUser};
use crate::fixtures::test_app::TestApp;

/// What every enrollment door answers for a token the ledger already holds.
const ALREADY_USED: &str = "This enrollment token has already been used";
/// What every enrollment door answers when the ledger cannot answer.
const LEDGER_DOWN: &str = "Could not verify this enrollment token is unused; please retry";

fn tid(org: &SeededTenant) -> ObjectId {
    ObjectId::parse_str(&org.tenant_id).unwrap()
}

/// Mint a tunnel enrollment token as `who`: `(token, jti)`.
async fn mint(app: &TestApp, org: &SeededTenant, who: &SeededUser) -> (String, String) {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/tunnel-client/enroll-token", org.tenant_id),
            &who.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "mint a tunnel enrollment token"
    );
    let body: Value = resp.json().await.unwrap();
    (
        body["enrollment_token"].as_str().unwrap().to_string(),
        body["jti"].as_str().unwrap().to_string(),
    )
}

/// `POST /api/tunnel-client/enroll`, the body `roomler enroll` sends:
/// `(status, body)`.
async fn enroll(app: &TestApp, token: &str, machine_id: &str, name: &str) -> (u16, Value) {
    let resp = app
        .client
        .post(app.url("/api/tunnel-client/enroll"))
        .json(&json!({
            "enrollment_token": token,
            "machine_id": machine_id,
            "machine_name": name,
            "os": "linux",
            "client_version": "0.4.0-test",
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// The org's tunnel-client row for `machine_id`, live or removed.
async fn client_row(app: &TestApp, org: &SeededTenant, machine_id: &str) -> Option<Document> {
    app.db
        .collection::<Document>("tunnel_clients")
        .find_one(doc! { "tenant_id": tid(org), "machine_id": machine_id })
        .await
        .unwrap()
}

/// Every tunnel-client row of the org, removed ones included.
async fn client_rows(app: &TestApp, org: &SeededTenant) -> u64 {
    app.db
        .collection::<Document>("tunnel_clients")
        .count_documents(doc! { "tenant_id": tid(org) })
        .await
        .unwrap()
}

/// The used-token ledger's record of a token's one use, if it has one.
async fn ledger_use(app: &TestApp, jti: &str) -> Option<Document> {
    app.db
        .collection::<Document>("used_tokens")
        .find_one(doc! { "_id": jti })
        .await
        .unwrap()
}

/// The org's overlay state, in a stable order: its networks (the address
/// pool) and its nodes (the leases). What a refusal must leave alone.
async fn overlay_state(app: &TestApp, org: &SeededTenant) -> (Vec<Document>, Vec<Document>) {
    let read = |collection: &'static str| {
        let coll = app.db.collection::<Document>(collection);
        let filter = doc! { "tenant_id": tid(org) };
        async move {
            coll.find(filter)
                .sort(doc! { "_id": 1 })
                .await
                .unwrap()
                .try_collect::<Vec<Document>>()
                .await
                .unwrap()
        }
    };
    (read("overlay_networks").await, read("overlay_nodes").await)
}

/// A live overlay lease for a tunnel client, as `handle_overlay_join` would
/// leave it (the overlay suite owns the join itself).
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
                tunnel_client_id: ObjectId::parse_str(client_id).unwrap(),
            },
            network_id,
            machine_id: machine_id.to_string(),
            name: "leased laptop".to_string(),
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

/// A second plain member of `org` (the `member` role: no `MANAGE_AGENTS`),
/// added by its admin.
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
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/member", org.tenant_id),
            &org.admin.access_token,
        )
        .json(&json!({ "user_id": user.id }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "add {name} to the org");
    user
}

/// The `gen` claim of a tunnel-client token (FR-103).
fn token_generation(token: &str) -> u64 {
    use base64::Engine as _;
    let payload = token.split('.').nth(1).expect("a JWT payload");
    let json: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap(),
    )
    .unwrap();
    json["gen"].as_u64().expect("a gen claim")
}

/// The answer a refused re-enrollment gives, with what it must say.
fn assert_refused_revive(status: u16, body: &Value, says: &str) {
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "conflict", "{body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(message.contains(says), "the sentence names why: {body}");
}

/// A tunnel enrollment token enrolls once. The second use is refused 401
/// "already used" whether it names the machine the first use enrolled (the
/// row it would revive) or a new one, and it writes nothing.
///
/// Negative control: with the spend taken out of `enroll_tunnel_client` this
/// fails at the first replay with a 200.
#[tokio::test]
async fn a_tunnel_enrollment_token_enrolls_once() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("tenonce").await;

    // A plain member enrolls their own laptop: members keep doing that.
    let (token, jti) = mint(&app, &org, &org.member).await;
    let (status, first) = enroll(&app, &token, "mach-once-a", "Once laptop").await;
    assert_eq!(status, 200, "{first}");
    let row_before = client_row(&app, &org, "mach-once-a").await.unwrap();
    let rows_before = client_rows(&app, &org).await;

    // The same token, back into the row it enrolled.
    let (status, body) = enroll(&app, &token, "mach-once-a", "Renamed by a replay").await;
    assert_eq!(status, 401, "the same machine again: {body}");
    assert_eq!(body["message"], ALREADY_USED, "{body}");
    // The same token, a machine it never saw.
    let (status, body) = enroll(&app, &token, "mach-once-b", "Second laptop").await;
    assert_eq!(status, 401, "a new machine: {body}");
    assert_eq!(body["message"], ALREADY_USED, "{body}");

    // Nothing was written: no second row, and the first exactly as it was.
    assert_eq!(client_rows(&app, &org).await, rows_before);
    assert!(client_row(&app, &org, "mach-once-b").await.is_none());
    assert_eq!(
        client_row(&app, &org, "mach-once-a").await.unwrap(),
        row_before,
        "a refused replay must not touch the row it named"
    );
    // The one use is on the ledger, under the tunnel door's purpose.
    let used = ledger_use(&app, &jti)
        .await
        .expect("the ledger holds the token's one use");
    assert_eq!(used.get_str("purpose").unwrap(), "tunnel-enroll");

    // A fresh token still enrolls, so the ledger spends only what it must.
    let (token2, _) = mint(&app, &org, &org.member).await;
    let (status, body) = enroll(&app, &token2, "mach-once-b", "Second laptop").await;
    assert_eq!(status, 200, "{body}");
    assert_ne!(body["tunnel_client_id"], first["tunnel_client_id"]);
}

/// A refusal that comes before the spend leaves the token unspent: an
/// archived org writes nothing to the ledger, and after a cap refusal the
/// same token enrolls once a slot is free — once.
///
/// Negative controls: with the spend moved ahead of the archived-org gate this
/// fails at "an archived-org refusal writes nothing to the ledger"; moved
/// between that gate and the cap, at "a cap refusal writes nothing to the
/// ledger"; taken out, at the second use's 401 with a 200.
#[tokio::test]
async fn a_refusal_before_the_spend_leaves_the_tunnel_token_unspent() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("tenkeep").await;
    let admin = org.admin.access_token.clone();
    let (token, jti) = mint(&app, &org, &org.admin).await;

    // An archived org takes no tunnel client, and the refusal spends nothing.
    let resp = app
        .auth_post(&format!("/api/tenant/{}/archive", org.tenant_id), &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let (status, body) = enroll(&app, &token, "mach-keep-a", "Keep laptop").await;
    assert_eq!(status, 403, "{body}");
    assert!(
        ledger_use(&app, &jti).await.is_none(),
        "an archived-org refusal writes nothing to the ledger"
    );
    let resp = app
        .auth_post(&format!("/api/tenant/{}/unarchive", org.tenant_id), &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // The plan's cap: Free takes 100 tunnel clients (FR-115), all in use.
    let now = bson::DateTime::now();
    let seeded: Vec<Document> = (0..100)
        .map(|i| {
            doc! {
                "tenant_id": tid(&org),
                "machine_id": format!("keep-seed-{i}"),
                "name": format!("keep-seed-{i}"),
                "created_at": now,
                "updated_at": now,
                "deleted_at": Bson::Null,
            }
        })
        .collect();
    app.db
        .collection::<Document>("tunnel_clients")
        .insert_many(seeded)
        .await
        .unwrap();
    let (status, body) = enroll(&app, &token, "mach-keep-a", "Keep laptop").await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Tunnel-client limit"),
        "{body}"
    );
    assert!(
        ledger_use(&app, &jti).await.is_none(),
        "a cap refusal writes nothing to the ledger"
    );
    assert!(client_row(&app, &org, "mach-keep-a").await.is_none());

    // A slot frees up, and the same token enrolls.
    app.db
        .collection::<Document>("tunnel_clients")
        .update_one(
            doc! { "tenant_id": tid(&org), "machine_id": "keep-seed-0" },
            doc! { "$set": { "deleted_at": bson::DateTime::now() } },
        )
        .await
        .unwrap();
    let (status, body) = enroll(&app, &token, "mach-keep-a", "Keep laptop").await;
    assert_eq!(status, 200, "the same token, once a slot is free: {body}");

    // …and that was its one use. (The machine it enrolled: a new one would
    // meet the cap again, which comes first.)
    let (status, body) = enroll(&app, &token, "mach-keep-a", "Keep laptop 2").await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(body["message"], ALREADY_USED, "{body}");
    assert!(ledger_use(&app, &jti).await.is_some());
}

/// Make the used-token ledger refuse every write (`refuse`), or answer again.
/// A validator no document passes turns an insert into a document validation
/// error: not a duplicate key, so to the spend it is a ledger that could not
/// answer, as an unreachable primary or a stepdown would be.
async fn ledger_refuses_writes(app: &TestApp, refuse: bool) {
    let validator = if refuse {
        doc! { "_id": { "$exists": false } }
    } else {
        doc! {}
    };
    let set = app
        .db
        .run_command(doc! {
            "collMod": "used_tokens",
            "validator": validator.clone(),
            "validationLevel": "strict",
            "validationAction": "error",
        })
        .await;
    if set.is_err() {
        // No ledger collection yet: make it, with the validator.
        app.db
            .create_collection("used_tokens")
            .validator(validator)
            .await
            .unwrap();
    }
}

/// When the ledger cannot answer, the tunnel door refuses with a retryable
/// 503, never "already used", writes nothing, and the same token enrolls on
/// the retry.
///
/// Negative control: with the spend failing open (a ledger error read as a
/// first use) this fails at the 503 with a 200.
#[tokio::test]
async fn when_the_ledger_cannot_answer_the_tunnel_token_stays_usable() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("tenledger").await;
    let (token, jti) = mint(&app, &org, &org.member).await;

    ledger_refuses_writes(&app, true).await;
    let (status, body) = enroll(&app, &token, "mach-ledger-a", "Ledger laptop").await;
    ledger_refuses_writes(&app, false).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["message"], LEDGER_DOWN, "{body}");
    assert!(
        client_row(&app, &org, "mach-ledger-a").await.is_none(),
        "nothing is written"
    );
    assert!(ledger_use(&app, &jti).await.is_none());

    let (status, body) = enroll(&app, &token, "mach-ledger-a", "Ledger laptop").await;
    assert_eq!(status, 200, "the same token on the retry: {body}");
    assert!(ledger_use(&app, &jti).await.is_some());
}

/// A machine id that already has a live tunnel client re-enrolls only for
/// that client's owner or for a holder of `MANAGE_AGENTS`. Anyone else gets
/// 409, and nothing is written: the row, its overlay lease, the address pool
/// and the ledger all stay as they were, so the refused token still enrolls a
/// machine of its own.
///
/// Negative controls: with the re-enrollment check taken out of the enroll
/// route this fails at the other member's 409 with a 200; with the spend moved
/// ahead of the check, at "the refusal comes before the spend".
#[tokio::test]
async fn a_known_machine_id_re_enrolls_only_for_its_owner_or_a_device_manager() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("tenowner").await;
    let other = add_member(&app, &org, "other").await;

    // The member enrolls their laptop: they own its row.
    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, enrolled) = enroll(&app, &token, "mach-owner-a", "Member laptop").await;
    assert_eq!(status, 200, "{enrolled}");
    let client_id = enrolled["tunnel_client_id"].as_str().unwrap().to_string();
    let row = client_row(&app, &org, "mach-owner-a").await.unwrap();
    assert_eq!(
        row.get_object_id("owner_user_id").unwrap().to_hex(),
        org.member.id
    );
    lease(&app, &org, &client_id, "mach-owner-a").await;
    let overlay_before = overlay_state(&app, &org).await;
    let rows_before = client_rows(&app, &org).await;

    // Another member, with a token of their own: refused, nothing written.
    let (other_token, other_jti) = mint(&app, &org, &other).await;
    let (status, body) = enroll(&app, &other_token, "mach-owner-a", "Not mine").await;
    assert_refused_revive(status, &body, "someone else's tunnel client");
    assert!(body.get("tunnel_client_token").is_none(), "{body}");
    assert_eq!(client_row(&app, &org, "mach-owner-a").await.unwrap(), row);
    assert_eq!(client_rows(&app, &org).await, rows_before);
    assert_eq!(overlay_state(&app, &org).await, overlay_before);
    assert!(
        ledger_use(&app, &other_jti).await.is_none(),
        "the refusal comes before the spend"
    );

    // The refusal spent nothing: the same token enrolls the other member's own
    // machine, which they then own.
    let (status, body) = enroll(&app, &other_token, "mach-owner-b", "Other laptop").await;
    assert_eq!(status, 200, "the refusal spent nothing: {body}");
    let theirs = client_row(&app, &org, "mach-owner-b").await.unwrap();
    assert_eq!(
        theirs.get_object_id("owner_user_id").unwrap().to_hex(),
        other.id
    );

    // The owner re-enrolls their own laptop: the same client, as before.
    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, body) = enroll(&app, &token, "mach-owner-a", "Member laptop 2").await;
    assert_eq!(status, 200, "the owner: {body}");
    assert_eq!(body["tunnel_client_id"], client_id.as_str());
    let revived = client_row(&app, &org, "mach-owner-a").await.unwrap();
    assert_eq!(revived.get_str("name").unwrap(), "Member laptop 2");
    assert_eq!(
        revived.get("token_gen"),
        row.get("token_gen"),
        "a re-enrollment advances nothing a removal would"
    );

    // A holder of MANAGE_AGENTS re-enrolls it too, and it stays the member's.
    let (token, _) = mint(&app, &org, &org.admin).await;
    let (status, body) = enroll(&app, &token, "mach-owner-a", "Member laptop 3").await;
    assert_eq!(status, 200, "a device manager: {body}");
    assert_eq!(body["tunnel_client_id"], client_id.as_str());
    let revived = client_row(&app, &org, "mach-owner-a").await.unwrap();
    assert_eq!(
        revived.get_object_id("owner_user_id").unwrap().to_hex(),
        org.member.id
    );
    // An allowed re-enrollment leaves the overlay alone too: the lease is the
    // join's business, as it always was.
    assert_eq!(overlay_state(&app, &org).await, overlay_before);
}

/// A removed tunnel client comes back only through a holder of
/// `MANAGE_AGENTS`, even for its owner. The owner's attempt is a 409 that
/// leaves the tombstone, its generation, the overlay and the ledger untouched;
/// the device manager's revives the same row at the generation the removal
/// set, so the client's old tokens stay dead and the new one works.
///
/// Negative controls: with the re-enrollment check taken out of the enroll
/// route this fails at the owner's 409 with a 200. With only its removed-client
/// rule taken out, the write's scope still refuses the owner, but after the
/// token is spent, so it fails at the ledger check.
#[tokio::test]
async fn a_removed_tunnel_client_comes_back_only_through_a_device_manager() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("tengone").await;

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, enrolled) = enroll(&app, &token, "mach-gone-a", "Gone laptop").await;
    assert_eq!(status, 200, "{enrolled}");
    let client_id = enrolled["tunnel_client_id"].as_str().unwrap().to_string();
    let old_token = enrolled["tunnel_client_token"]
        .as_str()
        .unwrap()
        .to_string();
    lease(&app, &org, &client_id, "mach-gone-a").await;

    // An admin removes it: tombstoned, generation 1, its lease released.
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/tunnel-client/{client_id}", org.tenant_id),
            &org.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let tombstone = client_row(&app, &org, "mach-gone-a").await.unwrap();
    assert!(matches!(
        tombstone.get("deleted_at"),
        Some(Bson::DateTime(_))
    ));
    let overlay_before = overlay_state(&app, &org).await;

    // Its owner, who does not hold MANAGE_AGENTS: refused, nothing written.
    let (token, jti) = mint(&app, &org, &org.member).await;
    let (status, body) = enroll(&app, &token, "mach-gone-a", "Gone laptop").await;
    assert_refused_revive(status, &body, "was removed");
    assert_eq!(
        client_row(&app, &org, "mach-gone-a").await.unwrap(),
        tombstone,
        "the tombstone, its removal time and its generation stay as they were"
    );
    assert_eq!(overlay_state(&app, &org).await, overlay_before);
    assert!(ledger_use(&app, &jti).await.is_none());

    // A device manager brings it back: the same row, at the removal's
    // generation, and nothing in the overlay moves until the client joins.
    let (token, _) = mint(&app, &org, &org.admin).await;
    let (status, body) = enroll(&app, &token, "mach-gone-a", "Gone laptop").await;
    assert_eq!(status, 200, "a device manager: {body}");
    assert_eq!(body["tunnel_client_id"], client_id.as_str());
    let revived = client_row(&app, &org, "mach-gone-a").await.unwrap();
    assert_eq!(revived.get("deleted_at"), Some(&Bson::Null));
    assert_eq!(revived.get("token_gen"), tombstone.get("token_gen"));
    let new_token = body["tunnel_client_token"].as_str().unwrap().to_string();
    assert_eq!(token_generation(&new_token), 1);
    assert_eq!(overlay_state(&app, &org).await, overlay_before);

    let roster = |token: String| {
        let req = app.auth_get("/api/tunnel-client/agents", &token);
        async move { req.send().await.unwrap().status().as_u16() }
    };
    assert_eq!(
        roster(new_token).await,
        200,
        "the revived client's token works"
    );
    assert_eq!(
        roster(old_token).await,
        401,
        "a token from before the removal"
    );
}

/// The write holds the decision again: an owner's re-enrollment refreshes the
/// row only while it is still live and theirs, so a removal that lands
/// between the route's check and the write is not undone. A device manager's
/// revives it, at the generation the removal set.
///
/// Negative control: with the `deleted_at` predicate taken out of
/// `ReviveScope::LiveOwnedBy`'s filter this fails at "a removal that landed
/// after the check" with the row revived.
#[tokio::test]
async fn an_owners_re_enrollment_never_revives_a_client_removed_meanwhile() {
    use roomler_ai_remote_control::models::OsKind;
    use roomler_ai_services::dao::tunnel_client::ReviveScope;

    let app = TestApp::spawn().await;
    let org = app.seed_tenant("tenrace").await;
    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, enrolled) = enroll(&app, &token, "mach-race-a", "Race laptop").await;
    assert_eq!(status, 200, "{enrolled}");
    let cid = ObjectId::parse_str(enrolled["tunnel_client_id"].as_str().unwrap()).unwrap();
    let owner = ObjectId::parse_str(&org.member.id).unwrap();
    let clients = &app.state.network().tunnel_clients;
    let refresh = |scope: ReviveScope, name: &'static str| async move {
        clients
            .rehydrate(cid, name, OsKind::Linux, "0.4.1-test", scope)
            .await
            .unwrap()
    };

    // Removed after the route read it live, before the write.
    assert!(clients.soft_delete(tid(&org), cid).await.unwrap());
    let tombstone = client_row(&app, &org, "mach-race-a").await.unwrap();
    assert!(
        refresh(ReviveScope::LiveOwnedBy(owner), "Race laptop 2")
            .await
            .is_none(),
        "a removal that landed after the check"
    );
    assert_eq!(
        client_row(&app, &org, "mach-race-a").await.unwrap(),
        tombstone,
        "nothing is written"
    );

    // A device manager revives it, at the generation the removal set.
    let revived = refresh(ReviveScope::LiveOrRemoved, "Race laptop 3")
        .await
        .expect("a device manager revives a removed client");
    assert!(revived.deleted_at.is_none());
    assert_eq!(revived.token_gen, 1);
    assert_eq!(revived.owner_user_id, owner, "and it stays its owner's");

    // Live again: its owner refreshes it, and nobody else's scope matches it.
    assert!(
        refresh(ReviveScope::LiveOwnedBy(ObjectId::new()), "Not theirs")
            .await
            .is_none(),
        "someone else's client"
    );
    let refreshed = refresh(ReviveScope::LiveOwnedBy(owner), "Race laptop 4")
        .await
        .expect("the owner refreshes their live client");
    assert_eq!(refreshed.name, "Race laptop 4");
    assert_eq!(refreshed.token_gen, 1);
}

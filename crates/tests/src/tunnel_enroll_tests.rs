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
//!
//! FR-159 P1 — the tests from `two_installs_with_one_legacy_hash_get_two_rows`
//! on: a request that carries the client's own `instance_id` (beside the
//! legacy `machine_id`, D2) revives the row that holds it, else claims an
//! unclaimed row with its `machine_id` once, else gets a row of its own; a
//! request without one is answered exactly as above. Each was read RED on
//! master's enrollment first.

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
    enroll_with(app, token, machine_id, None, name).await
}

/// [`enroll`], and with `instance_id` the body an FR-159 client sends: the
/// legacy `machine_id` beside the id its install keeps (D2).
async fn enroll_with(
    app: &TestApp,
    token: &str,
    machine_id: &str,
    instance_id: Option<&str>,
    name: &str,
) -> (u16, Value) {
    let mut body = json!({
        "enrollment_token": token,
        "machine_id": machine_id,
        "machine_name": name,
        "os": "linux",
        "client_version": "0.4.0-test",
    });
    if let Some(instance_id) = instance_id {
        body["instance_id"] = json!(instance_id);
    }
    post_enroll(app, &body).await
}

/// `POST /api/tunnel-client/enroll` with exactly `body`: `(status, body)`.
async fn post_enroll(app: &TestApp, body: &Value) -> (u16, Value) {
    let resp = app
        .client
        .post(app.url("/api/tunnel-client/enroll"))
        .json(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// FR-159 — an install's own id, as P2's clients make one: 32 lowercase hex.
fn install(n: u8) -> String {
    format!("{n:032x}")
}

/// The org's tunnel-client row an install's id holds, live or removed.
async fn row_of_install(app: &TestApp, org: &SeededTenant, instance_id: &str) -> Option<Document> {
    app.db
        .collection::<Document>("tunnel_clients")
        .find_one(doc! { "tenant_id": tid(org), "instance_id": instance_id })
        .await
        .unwrap()
}

/// How many of the org's tunnel-client rows hold an install's id.
async fn rows_of_install(app: &TestApp, org: &SeededTenant, instance_id: &str) -> u64 {
    app.db
        .collection::<Document>("tunnel_clients")
        .count_documents(doc! { "tenant_id": tid(org), "instance_id": instance_id })
        .await
        .unwrap()
}

/// The org at its plan's cap: Free takes 100 tunnel clients (FR-115), and
/// `live` are in use already, so this adds the rest.
async fn fill_to_the_cap(app: &TestApp, org: &SeededTenant, live: usize) {
    let now = bson::DateTime::now();
    let seeded: Vec<Document> = (live..100)
        .map(|i| {
            doc! {
                "tenant_id": tid(org),
                "machine_id": format!("cap-seed-{i}"),
                "name": format!("cap-seed-{i}"),
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
            exit_serves_v6: None,
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
    assert_eq!(
        revived.get_object_id("owner_user_id").unwrap().to_hex(),
        org.admin.id,
        "it comes back as the reviver's: its new token acts for them"
    );
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
/// revives it, at the generation the removal set, as the device manager's
/// own; a device manager's re-enrollment of a live client keeps its owner.
///
/// Negative controls: with the `deleted_at` predicate taken out of
/// `ReviveScope::LiveOwnedBy`'s filter this fails at "a removal that landed
/// after the check" with the row revived; with the owner left out of the
/// revival, at "a removed client comes back as its reviver's".
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

    // A device manager revives it, at the generation the removal set, as
    // their own: the token the revival mints acts for them.
    let manager = ObjectId::parse_str(&org.admin.id).unwrap();
    let revived = refresh(ReviveScope::LiveOrRemoved(manager), "Race laptop 3")
        .await
        .expect("a device manager revives a removed client");
    assert!(revived.deleted_at.is_none());
    assert_eq!(revived.token_gen, 1);
    assert_eq!(
        revived.owner_user_id, manager,
        "a removed client comes back as its reviver's"
    );

    // Live again: another device manager's re-enrollment keeps its owner.
    let kept = refresh(ReviveScope::LiveOrRemoved(ObjectId::new()), "Race laptop 4")
        .await
        .expect("a device manager re-enrolls a live client");
    assert_eq!(kept.owner_user_id, manager, "a live client keeps its owner");

    // Its owner refreshes it, and nobody else's scope matches it: not even
    // the owner it had before its removal.
    assert!(
        refresh(ReviveScope::LiveOwnedBy(owner), "Not theirs any more")
            .await
            .is_none(),
        "the owner before the removal"
    );
    let refreshed = refresh(ReviveScope::LiveOwnedBy(manager), "Race laptop 5")
        .await
        .expect("the owner refreshes their live client");
    assert_eq!(refreshed.name, "Race laptop 5");
    assert_eq!(refreshed.token_gen, 1);
}

// ────────────────────────────────────────────────────────────────────────────
// FR-159 P1 — the client's own id
// ────────────────────────────────────────────────────────────────────────────

/// Two machines of one person, enrolled under one name where `HOSTNAME` is
/// not set, send one legacy hash (#4479). With their own ids they are two
/// clients: each row holds its install's id, and takes it as its
/// `machine_id` too, so no legacy hash finds either.
///
/// RED on master: the second enrollment revives the first one's row.
#[tokio::test]
async fn two_installs_with_one_legacy_hash_get_two_rows() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("teniidtwo").await;
    let legacy = "mach-iid-two-shared";
    let (x, y) = (install(1), install(2));

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, first) = enroll_with(&app, &token, legacy, Some(&x), "Shared name").await;
    assert_eq!(status, 200, "{first}");
    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, second) = enroll_with(&app, &token, legacy, Some(&y), "Shared name").await;
    assert_eq!(status, 200, "{second}");

    assert_ne!(
        first["tunnel_client_id"], second["tunnel_client_id"],
        "two machines are two clients"
    );
    assert_eq!(client_rows(&app, &org).await, 2);
    for (instance_id, answer) in [(&x, &first), (&y, &second)] {
        let row = row_of_install(&app, &org, instance_id)
            .await
            .expect("a row holds the install's id");
        assert_eq!(
            row.get_object_id("_id").unwrap().to_hex(),
            answer["tunnel_client_id"].as_str().unwrap()
        );
        assert_eq!(row.get_str("machine_id").unwrap(), instance_id.as_str());
        assert_eq!(answer["keyed_by"], "instance_id", "{answer}");
    }
    assert!(
        client_row(&app, &org, legacy).await.is_none(),
        "no row is keyed by the shared hash"
    );
}

/// One install enrolled with `HOSTNAME` set, then again with it not set,
/// sends two legacy hashes and one id: the second enrollment revives the
/// first one's row, renames it in place, and leaves its `machine_id` and its
/// generation alone. Someone else who brings the same id is refused 409 with
/// a sentence of its own (another name would change nothing), before the
/// spend.
///
/// RED on master: the second hash makes a second row.
#[tokio::test]
async fn an_install_revives_its_own_row_whatever_its_legacy_hash_says() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("teniidown").await;
    let x = install(3);

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, first) = enroll_with(&app, &token, "mach-iid-own-set", Some(&x), "Laptop").await;
    assert_eq!(status, 200, "{first}");
    let row = row_of_install(&app, &org, &x).await.unwrap();

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, again) = enroll_with(
        &app,
        &token,
        "mach-iid-own-unset",
        Some(&x),
        "Laptop renamed",
    )
    .await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["tunnel_client_id"], first["tunnel_client_id"]);
    assert_eq!(again["keyed_by"], "instance_id", "{again}");
    assert_eq!(client_rows(&app, &org).await, 1);
    let revived = row_of_install(&app, &org, &x).await.unwrap();
    assert_eq!(
        revived.get_str("machine_id").unwrap(),
        row.get_str("machine_id").unwrap(),
        "the machine_id is never rewritten"
    );
    assert_eq!(revived.get_str("name").unwrap(), "Laptop renamed");
    assert_eq!(revived.get("token_gen"), row.get("token_gen"));

    // Another member's token, the same id: refused, nothing written.
    let other = add_member(&app, &org, "other").await;
    let (token, jti) = mint(&app, &org, &other).await;
    let (status, body) =
        enroll_with(&app, &token, "mach-iid-own-other", Some(&x), "Not mine").await;
    assert_refused_revive(status, &body, "the id its configuration keeps");
    assert_eq!(row_of_install(&app, &org, &x).await.unwrap(), revived);
    assert_eq!(client_rows(&app, &org).await, 1);
    assert!(
        ledger_use(&app, &jti).await.is_none(),
        "the refusal comes before the spend"
    );
    // …so the same token enrolls their own install.
    let (status, body) = enroll_with(
        &app,
        &token,
        "mach-iid-own-other",
        Some(&install(4)),
        "Mine",
    )
    .await;
    assert_eq!(status, 200, "the refusal spent nothing: {body}");
}

/// A row enrolled before FR-159 holds the legacy hash and no instance id.
/// The first install that brings that hash, in scope, claims it: the same
/// row, revived, and holding the install's id from then on. The claim mints
/// no new generation (D3) and never writes `machine_id`, so the overlay node
/// keyed by it is left alone. Once: a second install with the hash gets a
/// row of its own, and an older client with it still revives the claimed
/// row, as it would have before.
///
/// RED on master: no `instance_id` is written.
#[tokio::test]
async fn an_unclaimed_legacy_row_is_claimed_once_by_the_first_install_that_brings_it() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("teniidclaim").await;
    let legacy = "mach-iid-claim-legacy";

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, old) = enroll(&app, &token, legacy, "Old laptop").await;
    assert_eq!(status, 200, "{old}");
    let client_id = old["tunnel_client_id"].as_str().unwrap().to_string();
    lease(&app, &org, &client_id, legacy).await;
    let overlay_before = overlay_state(&app, &org).await;
    let before = client_row(&app, &org, legacy).await.unwrap();
    assert!(before.get("instance_id").is_none());

    let x = install(5);
    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, claimed) = enroll_with(&app, &token, legacy, Some(&x), "Old laptop").await;
    assert_eq!(status, 200, "{claimed}");
    assert_eq!(
        claimed["tunnel_client_id"],
        client_id.as_str(),
        "its own row"
    );
    assert_eq!(claimed["keyed_by"], "instance_id", "{claimed}");
    let row = client_row(&app, &org, legacy).await.unwrap();
    assert_eq!(row.get_str("instance_id").unwrap(), x);
    assert_eq!(
        row.get_str("machine_id").unwrap(),
        legacy,
        "a claim never writes machine_id"
    );
    assert_eq!(
        row.get("token_gen"),
        before.get("token_gen"),
        "a claim mints no new generation (D3)"
    );
    assert_eq!(
        token_generation(claimed["tunnel_client_token"].as_str().unwrap()),
        0
    );
    assert_eq!(overlay_state(&app, &org).await, overlay_before);

    // Once: another install with the same hash gets a row of its own.
    let y = install(6);
    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, other) = enroll_with(&app, &token, legacy, Some(&y), "Old laptop").await;
    assert_eq!(status, 200, "{other}");
    assert_ne!(other["tunnel_client_id"], client_id.as_str());
    assert_eq!(
        client_row(&app, &org, legacy)
            .await
            .unwrap()
            .get_str("instance_id")
            .unwrap(),
        x,
        "the claim holds"
    );
    assert_eq!(client_rows(&app, &org).await, 2);

    // An older client with the hash revives the claimed row, as before.
    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, older) = enroll(&app, &token, legacy, "Old laptop").await;
    assert_eq!(status, 200, "{older}");
    assert_eq!(older["tunnel_client_id"], client_id.as_str());
    assert!(older.get("keyed_by").is_none(), "{older}");
    assert_eq!(client_rows(&app, &org).await, 2);
}

/// Someone else's live row, met by the legacy hash alone, is only a guess
/// that the row is this install's. The install gets a row of its own, as a
/// new name would get it (D4), and the legacy row, its lease and the address
/// pool stay as they were.
///
/// RED on master: 409 `SOMEONE_ELSES_TUNNEL_CLIENT`.
#[tokio::test]
async fn someone_elses_live_legacy_row_gets_a_new_install_a_row_of_its_own() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("teniidtheirs").await;
    let other = add_member(&app, &org, "other").await;
    let legacy = "mach-iid-theirs";

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, enrolled) = enroll(&app, &token, legacy, "Member laptop").await;
    assert_eq!(status, 200, "{enrolled}");
    let client_id = enrolled["tunnel_client_id"].as_str().unwrap().to_string();
    lease(&app, &org, &client_id, legacy).await;
    let overlay_before = overlay_state(&app, &org).await;
    let row = client_row(&app, &org, legacy).await.unwrap();

    let y = install(7);
    let (token, jti) = mint(&app, &org, &other).await;
    let (status, theirs) = enroll_with(&app, &token, legacy, Some(&y), "Member laptop").await;
    assert_eq!(status, 200, "a row of its own, not a 409 (D4): {theirs}");
    assert_ne!(theirs["tunnel_client_id"], client_id.as_str());
    let own = row_of_install(&app, &org, &y).await.unwrap();
    assert_eq!(
        own.get_object_id("owner_user_id").unwrap().to_hex(),
        other.id
    );
    assert_eq!(own.get_str("machine_id").unwrap(), y);
    assert_eq!(
        client_row(&app, &org, legacy).await.unwrap(),
        row,
        "the legacy row is left as it was"
    );
    assert_eq!(overlay_state(&app, &org).await, overlay_before);
    assert!(ledger_use(&app, &jti).await.is_some());
    assert_eq!(client_rows(&app, &org).await, 2);
}

/// A split, the second machine of a shared row getting a row of its own, is
/// a new row, so an org at its cap refuses it 403 before the spend; a claim
/// takes no slot, so an unclaimed row is still claimed at the cap. Once a
/// slot frees up, the same token makes the split.
///
/// RED on master: the second machine revives the shared row, so there is no
/// 403.
#[tokio::test]
async fn a_split_meets_the_cap_before_the_spend_and_a_claim_takes_no_slot() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("teniidcap").await;
    let (shared, spare) = ("mach-iid-cap-shared", "mach-iid-cap-spare");

    for (legacy, name) in [(shared, "Shared"), (spare, "Spare")] {
        let (token, _) = mint(&app, &org, &org.member).await;
        let (status, body) = enroll(&app, &token, legacy, name).await;
        assert_eq!(status, 200, "{body}");
    }
    let shared_id = client_row(&app, &org, shared)
        .await
        .unwrap()
        .get_object_id("_id")
        .unwrap()
        .to_hex();
    // The first machine of the shared row, updated, claims it.
    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, body) = enroll_with(&app, &token, shared, Some(&install(8)), "Shared").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tunnel_client_id"], shared_id.as_str());
    fill_to_the_cap(&app, &org, 2).await;
    let rows_at_cap = client_rows(&app, &org).await;

    // The second machine of the shared row: refused, nothing written.
    let y = install(9);
    let (split_token, jti) = mint(&app, &org, &org.member).await;
    let (status, body) = enroll_with(&app, &split_token, shared, Some(&y), "Shared").await;
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
        "the cap's refusal comes before the spend"
    );
    assert!(row_of_install(&app, &org, &y).await.is_none());
    assert_eq!(client_rows(&app, &org).await, rows_at_cap);

    // At the cap, the spare row is still claimed: a claim takes no slot.
    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, body) = enroll_with(&app, &token, spare, Some(&install(10)), "Spare").await;
    assert_eq!(status, 200, "a claim at the cap: {body}");
    assert_eq!(
        client_row(&app, &org, spare)
            .await
            .unwrap()
            .get_str("instance_id")
            .unwrap(),
        install(10)
    );
    assert_eq!(client_rows(&app, &org).await, rows_at_cap);

    // A slot frees up, and the refused token makes the split.
    app.db
        .collection::<Document>("tunnel_clients")
        .update_one(
            doc! { "tenant_id": tid(&org), "machine_id": "cap-seed-2" },
            doc! { "$set": { "deleted_at": bson::DateTime::now() } },
        )
        .await
        .unwrap();
    let (status, body) = enroll_with(&app, &split_token, shared, Some(&y), "Shared").await;
    assert_eq!(status, 200, "the refusal spent nothing: {body}");
    assert_ne!(body["tunnel_client_id"], shared_id.as_str());
    assert!(ledger_use(&app, &jti).await.is_some());
}

/// Two installs that share a legacy row enroll at once. However the two
/// requests interleave, one claims the row and the other gets a row of its
/// own: the claim's update matches only while the row holds no install, so
/// the second never overwrites the first, and no install is left without a
/// row of its own.
///
/// RED on master: both revive the shared row, one row.
#[tokio::test]
async fn two_concurrent_claims_leave_one_claim_and_one_new_row() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("teniidrace").await;
    let legacy = "mach-iid-race-shared";

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, old) = enroll(&app, &token, legacy, "Shared").await;
    assert_eq!(status, 200, "{old}");
    let client_id = old["tunnel_client_id"].as_str().unwrap().to_string();

    let (x, y) = (install(11), install(12));
    let (tx, _) = mint(&app, &org, &org.member).await;
    let (ty, _) = mint(&app, &org, &org.member).await;
    let ((sx, bx), (sy, by)) = tokio::join!(
        enroll_with(&app, &tx, legacy, Some(&x), "Shared"),
        enroll_with(&app, &ty, legacy, Some(&y), "Shared"),
    );
    assert_eq!((sx, sy), (200, 200), "{bx} {by}");
    assert_eq!(
        client_rows(&app, &org).await,
        2,
        "one claim and one new row"
    );

    let legacy_row = client_row(&app, &org, legacy).await.unwrap();
    let winner = legacy_row
        .get_str("instance_id")
        .expect("the shared row is claimed")
        .to_string();
    let (won, lost, loser) = if winner == x {
        (&bx, &by, &y)
    } else {
        assert_eq!(winner, y, "the row holds one of the two ids");
        (&by, &bx, &x)
    };
    assert_eq!(won["tunnel_client_id"], client_id.as_str());
    assert_ne!(lost["tunnel_client_id"], client_id.as_str());
    let own = row_of_install(&app, &org, loser).await.unwrap();
    assert_eq!(
        own.get_object_id("_id").unwrap().to_hex(),
        lost["tunnel_client_id"].as_str().unwrap()
    );
    assert_eq!(own.get_str("machine_id").unwrap(), loser.as_str());
    for instance_id in [&x, &y] {
        assert_eq!(
            rows_of_install(&app, &org, instance_id).await,
            1,
            "each install holds exactly one row"
        );
    }
}

/// The claim, driven at the DAO, so each of its outcomes is read without a
/// race to arrange: the first claim wins and the row holds the install's id,
/// with its `machine_id` as it was; a claim that lost to another install
/// writes nothing (the route then makes a row of its own); one that lost to
/// the same install is a revival of the row, not an insert; and one whose
/// row left the scope meanwhile writes nothing (the route refuses it 409).
///
/// RED on master's code: there is no claim.
#[tokio::test]
async fn a_claim_holds_its_row_once_and_says_why_it_lost() {
    use roomler_ai_remote_control::models::OsKind;
    use roomler_ai_services::dao::tunnel_client::{Claim, ReviveScope};

    let app = TestApp::spawn().await;
    let org = app.seed_tenant("teniiddao").await;
    let mut ids = Vec::new();
    for legacy in ["mach-iid-dao-a", "mach-iid-dao-b"] {
        let (token, _) = mint(&app, &org, &org.member).await;
        let (status, body) = enroll(&app, &token, legacy, "Claim laptop").await;
        assert_eq!(status, 200, "{body}");
        ids.push(ObjectId::parse_str(body["tunnel_client_id"].as_str().unwrap()).unwrap());
    }
    let (a, b) = (ids[0], ids[1]);
    let owner = ObjectId::parse_str(&org.member.id).unwrap();
    let clients = &app.state.network().tunnel_clients;
    let claim = |id: ObjectId, instance_id: String, scope: ReviveScope| async move {
        clients
            .claim(
                id,
                &instance_id,
                "Claimed laptop",
                OsKind::Linux,
                "0.4.2-test",
                scope,
            )
            .await
            .unwrap()
    };
    let (x, y) = (install(13), install(14));

    let won = claim(a, x.clone(), ReviveScope::LiveOwnedBy(owner)).await;
    let Claim::Claimed(row) = &won else {
        panic!("the first claim wins: {won:?}");
    };
    assert_eq!(row.instance_id.as_deref(), Some(x.as_str()));
    assert_eq!(
        row.machine_id, "mach-iid-dao-a",
        "a claim never writes machine_id"
    );
    assert_eq!(row.name, "Claimed laptop");

    let before = client_row(&app, &org, "mach-iid-dao-a").await.unwrap();
    let lost = claim(a, y.clone(), ReviveScope::LiveOwnedBy(owner)).await;
    assert!(matches!(lost, Claim::TakenByAnother), "{lost:?}");
    assert_eq!(
        client_row(&app, &org, "mach-iid-dao-a").await.unwrap(),
        before,
        "a lost claim writes nothing"
    );

    let again = claim(a, x.clone(), ReviveScope::LiveOwnedBy(owner)).await;
    let Claim::Revived(row) = &again else {
        panic!("the same install again is a revival: {again:?}");
    };
    assert_eq!(row.id, Some(a));
    assert_eq!(rows_of_install(&app, &org, &x).await, 1);

    // Removed after the route read it live, before the claim.
    assert!(clients.soft_delete(tid(&org), b).await.unwrap());
    let tombstone = client_row(&app, &org, "mach-iid-dao-b").await.unwrap();
    let gone = claim(b, y.clone(), ReviveScope::LiveOwnedBy(owner)).await;
    assert!(matches!(gone, Claim::OutOfScope), "{gone:?}");
    assert_eq!(
        client_row(&app, &org, "mach-iid-dao-b").await.unwrap(),
        tombstone,
        "a claim that lost its scope writes nothing"
    );

    // A device manager's claim brings it back, as theirs, at the removal's
    // generation.
    let manager = ObjectId::parse_str(&org.admin.id).unwrap();
    let back = claim(b, y.clone(), ReviveScope::LiveOrRemoved(manager)).await;
    let Claim::Claimed(row) = &back else {
        panic!("a device manager claims a removed row: {back:?}");
    };
    assert!(row.deleted_at.is_none());
    assert_eq!(row.owner_user_id, manager);
    assert_eq!(row.token_gen, 1);
    assert_eq!(row.instance_id.as_deref(), Some(y.as_str()));
}

/// An `instance_id` that is not 32 lowercase hex is refused 400 before the
/// token is spent, and nothing is written; the same token then enrolls with
/// a well-formed id.
///
/// RED on master: the field is ignored, 200.
#[tokio::test]
async fn a_malformed_instance_id_is_400_and_the_token_stays_unspent() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("teniidbad").await;
    let (token, jti) = mint(&app, &org, &org.member).await;

    for bad in [
        "",
        "0123456789abcdef0123456789ABCDEF",
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0",
        "0123456789abcdef0123456789abcdeg",
        "../../../../../../../../../../ab",
    ] {
        let (status, body) = enroll_with(&app, &token, "mach-iid-bad", Some(bad), "Bad id").await;
        assert_eq!(status, 400, "{bad:?}: {body}");
        assert_eq!(body["error"], "bad_request", "{body}");
    }
    assert!(
        ledger_use(&app, &jti).await.is_none(),
        "the 400 comes before the spend"
    );
    assert_eq!(client_rows(&app, &org).await, 0);

    let (status, body) =
        enroll_with(&app, &token, "mach-iid-bad", Some(&install(15)), "Good id").await;
    assert_eq!(status, 200, "the 400 spent nothing: {body}");
}

/// With the switch off a request that carried an id is told so: it was keyed
/// by `machine_id`, the old way. An older client is told nothing, switch or
/// not.
///
/// RED on master: no `keyed_by`.
#[tokio::test]
async fn with_the_switch_off_an_install_is_told_it_was_keyed_by_machine_id() {
    let app = TestApp::spawn_with_settings(|s| s.rc.tunnel_instance_id = false).await;
    let org = app.seed_tenant("teniidkeyed").await;

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, body) = enroll_with(
        &app,
        &token,
        "mach-iid-keyed",
        Some(&install(16)),
        "Switch off",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["keyed_by"], "machine_id", "{body}");

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, body) = enroll(&app, &token, "mach-iid-keyed", "Switch off").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("keyed_by").is_none(), "{body}");
}

/// Guard, green on master and after: a request without an `instance_id`,
/// or with a JSON `null` one, is an older client's, answered as before:
/// the answer's three fields and no `keyed_by`, and a row that holds no
/// instance id.
#[tokio::test]
async fn a_request_without_an_instance_id_is_answered_as_before() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("teniidold").await;

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, body) = enroll(&app, &token, "mach-iid-old", "Old client").await;
    assert_eq!(status, 200, "{body}");
    let mut keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["tenant_id", "tunnel_client_id", "tunnel_client_token"],
        "{body}"
    );
    let row = client_row(&app, &org, "mach-iid-old").await.unwrap();
    assert!(row.get("instance_id").is_none(), "{row}");

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, again) = post_enroll(
        &app,
        &json!({
            "enrollment_token": token,
            "machine_id": "mach-iid-old",
            "machine_name": "Old client",
            "os": "linux",
            "client_version": "0.4.0-test",
            "instance_id": null,
        }),
    )
    .await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["tunnel_client_id"], body["tunnel_client_id"]);
    assert!(again.get("keyed_by").is_none(), "{again}");
    assert_eq!(client_rows(&app, &org).await, 1);
    assert!(
        client_row(&app, &org, "mach-iid-old")
            .await
            .unwrap()
            .get("instance_id")
            .is_none()
    );
}

/// Guard, green on master and after: a removed legacy row stays 409
/// `REMOVED_TUNNEL_CLIENT` for its owner without `MANAGE_AGENTS`, though the
/// machine now brings an id of its own (D4): a removal is still undone only
/// by a device manager, and no row of its own is made instead.
#[tokio::test]
async fn a_removed_legacy_row_stays_409_for_its_owner_without_the_permission() {
    let app = TestApp::spawn().await;
    let org = app.seed_tenant("teniidgone").await;
    let legacy = "mach-iid-gone";

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, enrolled) = enroll(&app, &token, legacy, "Gone laptop").await;
    assert_eq!(status, 200, "{enrolled}");
    let client_id = enrolled["tunnel_client_id"].as_str().unwrap().to_string();
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/tunnel-client/{client_id}", org.tenant_id),
            &org.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let tombstone = client_row(&app, &org, legacy).await.unwrap();

    let (token, jti) = mint(&app, &org, &org.member).await;
    let (status, body) = enroll_with(&app, &token, legacy, Some(&install(17)), "Gone laptop").await;
    assert_refused_revive(status, &body, "was removed");
    assert_eq!(client_row(&app, &org, legacy).await.unwrap(), tombstone);
    assert!(ledger_use(&app, &jti).await.is_none());
    assert_eq!(client_rows(&app, &org).await, 1, "no row of its own either");
}

/// Guard, green on master and after: with `rc.tunnel_instance_id` off the
/// server reads a request that carries an id as an older client's. Two
/// installs with one legacy hash share one row, no instance id is written,
/// and a malformed id is not refused.
#[tokio::test]
async fn with_the_switch_off_an_install_is_looked_up_by_machine_id_alone() {
    let app = TestApp::spawn_with_settings(|s| s.rc.tunnel_instance_id = false).await;
    let org = app.seed_tenant("teniidoff").await;
    let legacy = "mach-iid-off-shared";

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, first) = enroll_with(&app, &token, legacy, Some(&install(18)), "Shared").await;
    assert_eq!(status, 200, "{first}");
    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, second) = enroll_with(&app, &token, legacy, Some(&install(19)), "Shared").await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["tunnel_client_id"], first["tunnel_client_id"]);
    assert_eq!(client_rows(&app, &org).await, 1);
    let row = client_row(&app, &org, legacy).await.unwrap();
    assert!(row.get("instance_id").is_none(), "{row}");

    let (token, _) = mint(&app, &org, &org.member).await;
    let (status, body) = enroll_with(&app, &token, legacy, Some("NOT AN ID"), "Shared").await;
    assert_eq!(status, 200, "the switch off reads no shape: {body}");
    assert_eq!(body["tunnel_client_id"], first["tunnel_client_id"]);
}

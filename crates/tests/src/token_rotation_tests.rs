// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-155 P1a — a device trades its agent token for one of the next
//! generation (`POST /api/agent/self/rotate`), and a token of any generation
//! below it is refused from then on. The server's half:
//! - a rotation answers a token of generation G+1 and moves the row to G+1;
//! - an answer that was lost is asked for again with the same token and the
//!   same rotation key, and answered at G+1 with nothing else changed;
//! - one rotation per generation, and at most one first rotation an hour per
//!   device (429), an answer asked again exempt;
//! - a removed or quarantined device does not rotate;
//! - both switches; and one audit row per decision, with the caller's
//!   address and the generations.
//!
//! Every server here starts with `rc.agent_token_rotation` on (its default is
//! off until the phase that completes the server's half), except where a
//! cell is about the switch.

use crate::fixtures::test_app::TestApp;
use crate::token_renewal_tests::{enroll, forge_agent_token, self_devices};
use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use serde_json::{Value, json};

const ROTATE: &str = "/api/agent/self/rotate";
const AUDIT: &str = "token_rotation_audit";

async fn spawn_on() -> TestApp {
    TestApp::spawn_with_settings(|s| s.rc.agent_token_rotation = true).await
}

/// A server over the database `db`, as `spawn_pair` builds its two, with
/// its own pod id.
async fn spawn_sharing(
    db: &str,
    pod: &str,
    mutate: impl FnOnce(&mut roomler_ai_config::Settings),
) -> TestApp {
    let db = db.to_string();
    let pod = format!(
        "fr155-{pod}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    TestApp::spawn_with_settings(move |s| {
        s.database.name = db;
        s.app.pod_id = Some(pod);
        mutate(s);
    })
    .await
}

/// Remove a device, as an administrator does.
async fn remove(app: &TestApp, seeded: &crate::fixtures::seed::SeededTenant, agent_id: &str) {
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, agent_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "remove the device");
}

/// A rotation key: base64 of 32 bytes, each `seed`.
fn rotation_key(seed: u8) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode([seed; 32])
}

async fn rotate(app: &TestApp, token: &str, key: &str) -> reqwest::Response {
    app.auth_post(ROTATE, token)
        .json(&json!({ "rotation_key": key }))
        .send()
        .await
        .unwrap()
}

/// The agent token of a 200 answer.
async fn answered_token(resp: reqwest::Response) -> String {
    assert_eq!(resp.status(), 200, "a rotation answer");
    let body: Value = resp.json().await.unwrap();
    chrono::DateTime::parse_from_rfc3339(body["expires_at"].as_str().unwrap())
        .expect("expires_at is RFC 3339");
    body["agent_token"].as_str().unwrap().to_string()
}

/// The claims of an agent token.
fn token_claims(token: &str) -> Value {
    use base64::Engine as _;
    let payload = token.split('.').nth(1).expect("a JWT payload");
    serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap(),
    )
    .unwrap()
}

/// The `gen` claim of an agent token.
fn token_generation(token: &str) -> u64 {
    token_claims(token)["gen"].as_u64().expect("a gen claim")
}

fn oid(id: &str) -> ObjectId {
    ObjectId::parse_str(id).unwrap()
}

async fn agent_row(app: &TestApp, agent_id: &str) -> Document {
    app.db
        .collection::<Document>("agents")
        .find_one(doc! { "_id": oid(agent_id) })
        .await
        .unwrap()
        .expect("the device row")
}

/// A whole number in a document, whichever integer type it was stored as.
fn int(doc: &Document, key: &str) -> Option<i64> {
    match doc.get(key) {
        Some(Bson::Int32(n)) => Some(i64::from(*n)),
        Some(Bson::Int64(n)) => Some(*n),
        None | Some(Bson::Null) => None,
        Some(other) => panic!("{key} is {other:?}"),
    }
}

/// The row's generation. A row from before FR-103 has no field: 0.
fn row_generation(row: &Document) -> i64 {
    int(row, "token_gen").unwrap_or(0)
}

/// One device's audit rows, oldest first.
async fn audit_rows(app: &TestApp, agent_id: &str) -> Vec<Document> {
    use futures::TryStreamExt;
    app.db
        .collection::<Document>(AUDIT)
        .find(doc! { "agent_id": oid(agent_id) })
        .sort(doc! { "at": 1, "_id": 1 })
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap()
}

/// Each row's decision and reason, in order.
fn decisions(rows: &[Document]) -> Vec<(String, Option<String>)> {
    rows.iter()
        .map(|r| {
            (
                r.get_str("decision").expect("a decision").to_string(),
                r.get_str("reason").ok().map(str::to_string),
            )
        })
        .collect()
}

fn decision(d: &str, reason: Option<&str>) -> (String, Option<String>) {
    (d.to_string(), reason.map(str::to_string))
}

/// T1 (its first half): a rotation answers a token of the next generation,
/// the row moves to it, and a token of the generation before is refused on
/// the HTTP routes with the rotation's own words.
#[tokio::test]
async fn a_rotation_issues_the_next_generation() {
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155next").await;
    let (agent_id, old) = enroll(&app, &seeded, "m-fr155-next").await;
    assert_eq!(token_generation(&old), 0);

    let new = answered_token(rotate(&app, &old, &rotation_key(1)).await).await;
    assert_eq!(token_generation(&new), 1, "a token of the next generation");
    assert_eq!(
        row_generation(&agent_row(&app, &agent_id).await),
        1,
        "the row"
    );
    assert_eq!(self_devices(&app, &new).await, 200, "the new token works");

    let resp = app
        .auth_get("/api/agent/self/devices", &old)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "a token of the generation before");
    let refusal: Value = resp.json().await.unwrap();
    let message = refusal["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("rotation") && !message.contains("removed"),
        "refused for the rotation, not for a removal: {refusal}"
    );

    let rows = audit_rows(&app, &agent_id).await;
    assert_eq!(decisions(&rows), vec![decision("first", None)]);
    assert_eq!(rows[0].get_str("client_ip").unwrap(), "127.0.0.1");
    assert_eq!(int(&rows[0], "gen_from"), Some(0));
    assert_eq!(int(&rows[0], "gen_to"), Some(1));
}

/// T2: an answer that was lost is asked for again with the same token and
/// the same rotation key. It is answered at G+1, and nothing else changes.
/// With another key it is refused.
#[tokio::test]
async fn a_lost_answer_is_recovered_with_the_rotation_key() {
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155replay").await;
    let (agent_id, old) = enroll(&app, &seeded, "m-fr155-replay").await;
    let key = rotation_key(2);
    answered_token(rotate(&app, &old, &key).await).await;
    let before = agent_row(&app, &agent_id).await;
    assert!(before.get_document("token_rotation").is_ok(), "{before}");

    let again = answered_token(rotate(&app, &old, &key).await).await;
    assert_eq!(
        token_generation(&again),
        1,
        "the generation the rotation made"
    );
    assert_eq!(self_devices(&app, &again).await, 200);
    let after = agent_row(&app, &agent_id).await;
    for field in [
        "token_gen",
        "token_rotation",
        "token_exp",
        "status",
        "deleted_at",
    ] {
        assert_eq!(after.get(field), before.get(field), "{field} is unchanged");
    }

    let other = rotate(&app, &old, &rotation_key(3)).await;
    assert_eq!(other.status(), 401, "another rotation key");
    let last = agent_row(&app, &agent_id).await;
    for field in ["token_gen", "token_rotation", "token_exp"] {
        assert_eq!(last.get(field), before.get(field), "{field} is unchanged");
    }

    let rows = audit_rows(&app, &agent_id).await;
    assert_eq!(
        decisions(&rows),
        vec![
            decision("first", None),
            decision("replay", None),
            decision("refused", Some("rotation_key")),
        ]
    );
    for row in &rows {
        assert_eq!(row.get_str("client_ip").unwrap(), "127.0.0.1", "{row}");
        assert_eq!(int(row, "gen_from"), Some(0), "{row}");
        assert_eq!(int(row, "gen_to"), Some(1), "{row}");
    }
}

/// T3: one rotation per generation: the conditional update commits once at
/// G, and the row ends at G+1.
#[tokio::test]
async fn one_rotation_per_generation() {
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155once").await;
    let (agent_id, token) = enroll(&app, &seeded, "m-fr155-once").await;

    let calls = (0..8u8).map(|i| {
        let (app, token, key) = (&app, &token, rotation_key(10 + i));
        async move { rotate(app, token, &key).await.status().as_u16() }
    });
    let statuses = futures::future::join_all(calls).await;
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        1,
        "one commit at G: {statuses:?}"
    );
    assert!(
        statuses.iter().all(|s| *s == 200 || *s == 401),
        "every answer is 200 or 401: {statuses:?}"
    );
    assert_eq!(
        row_generation(&agent_row(&app, &agent_id).await),
        1,
        "the row ends at G+1"
    );

    let rows = audit_rows(&app, &agent_id).await;
    let firsts = rows
        .iter()
        .filter(|r| r.get_str("decision").ok() == Some("first"))
        .count();
    assert_eq!(firsts, 1, "one commit at G: {:?}", decisions(&rows));
    assert_eq!(rows.len(), 8, "one audit row per decision");
}

/// T6: `rc.agent_token_rotation` off answers 404 to a first call, with an
/// `unsupported` audit row, and answers a call asking again for a rotation
/// made before. `rc.token_generation_check` off answers 404 to every call.
/// Three servers over one database: the switch on, the switch off, and the
/// generation check off.
#[tokio::test]
async fn rotation_is_unsupported_while_switched_off() {
    let db = format!("roomler_ai_test_{}", uuid::Uuid::new_v4().simple());
    let on = spawn_sharing(&db, "on", |s| s.rc.agent_token_rotation = true).await;
    let off = spawn_sharing(&db, "off", |s| s.rc.agent_token_rotation = false).await;
    let lax = spawn_sharing(&db, "lax", |s| {
        s.rc.agent_token_rotation = true;
        s.rc.token_generation_check = false;
    })
    .await;
    let seeded = on.seed_tenant("fr155off").await;
    let (agent_id, g0) = enroll(&on, &seeded, "m-fr155-off").await;
    let key = rotation_key(20);
    let g1 = answered_token(rotate(&on, &g0, &key).await).await;
    assert_eq!(token_generation(&g1), 1);

    // The switch off.
    assert_eq!(
        rotate(&off, &g1, &rotation_key(21)).await.status(),
        404,
        "a first call"
    );
    let again = answered_token(rotate(&off, &g0, &key).await).await;
    assert_eq!(token_generation(&again), 1, "the rotation made before");
    assert_eq!(row_generation(&agent_row(&off, &agent_id).await), 1);
    let (fresh_id, fresh) = enroll(&off, &seeded, "m-fr155-off-fresh").await;
    assert_eq!(
        rotate(&off, &fresh, &rotation_key(22)).await.status(),
        404,
        "a device that never rotated"
    );
    assert_eq!(row_generation(&agent_row(&off, &fresh_id).await), 0);
    assert_eq!(
        decisions(&audit_rows(&off, &fresh_id).await),
        vec![decision("unsupported", Some("agent_token_rotation"))]
    );

    // The generation check off: every call.
    assert_eq!(
        rotate(&lax, &g1, &rotation_key(23)).await.status(),
        404,
        "a first call"
    );
    assert_eq!(
        rotate(&lax, &g0, &key).await.status(),
        404,
        "a call asking again"
    );
    assert_eq!(row_generation(&agent_row(&lax, &agent_id).await), 1);

    assert_eq!(
        decisions(&audit_rows(&on, &agent_id).await),
        vec![
            decision("first", None),
            decision("unsupported", Some("agent_token_rotation")),
            decision("replay", None),
            decision("unsupported", Some("token_generation_check")),
            decision("unsupported", Some("token_generation_check")),
        ]
    );
}

/// T7: a removed (tombstoned) device and a quarantined one do not rotate,
/// and their generation is unchanged.
#[tokio::test]
async fn a_removed_or_quarantined_row_does_not_rotate() {
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155refuse").await;

    let (quarantined, q_token) = enroll(&app, &seeded, "m-fr155-quarantined").await;
    app.db
        .collection::<Document>("agents")
        .update_one(
            doc! { "_id": oid(&quarantined) },
            doc! { "$set": { "status": "quarantined" } },
        )
        .await
        .unwrap();
    assert_eq!(
        rotate(&app, &q_token, &rotation_key(30)).await.status(),
        401,
        "quarantined"
    );
    assert_eq!(row_generation(&agent_row(&app, &quarantined).await), 0);

    let (removed, r_token) = enroll(&app, &seeded, "m-fr155-removed").await;
    remove(&app, &seeded, &removed).await;
    let at_removal = row_generation(&agent_row(&app, &removed).await);
    assert_eq!(
        rotate(&app, &r_token, &rotation_key(31)).await.status(),
        401,
        "removed"
    );
    assert_eq!(
        row_generation(&agent_row(&app, &removed).await),
        at_removal,
        "the generation is unchanged"
    );

    assert_eq!(
        decisions(&audit_rows(&app, &quarantined).await),
        vec![decision("refused", Some("quarantined"))]
    );
    assert_eq!(
        decisions(&audit_rows(&app, &removed).await),
        vec![decision("refused", Some("removed"))]
    );
}

/// T13: a first call within the hour of the device's last rotation is
/// answered 429 and the generation is unchanged; asking again for the answer
/// to that rotation is exempt; and once the rotation is more than an hour
/// old, the second rotation commits.
#[tokio::test]
async fn a_second_rotation_within_the_hour_waits() {
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155hour").await;
    let (agent_id, g0) = enroll(&app, &seeded, "m-fr155-hour").await;
    let k1 = rotation_key(40);
    let g1 = answered_token(rotate(&app, &g0, &k1).await).await;

    let resp = rotate(&app, &g1, &rotation_key(41)).await;
    assert_eq!(resp.status(), 429, "within the hour");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "rate_limited", "{body}");
    assert_eq!(row_generation(&agent_row(&app, &agent_id).await), 1);

    let again = answered_token(rotate(&app, &g0, &k1).await).await;
    assert_eq!(
        token_generation(&again),
        1,
        "the answer asked again is exempt"
    );

    let two_hours_ago = DateTime::from_millis(DateTime::now().timestamp_millis() - 2 * 3600 * 1000);
    app.db
        .collection::<Document>("agents")
        .update_one(
            doc! { "_id": oid(&agent_id) },
            doc! { "$set": { "token_rotation.at": two_hours_ago } },
        )
        .await
        .unwrap();
    let g2 = answered_token(rotate(&app, &g1, &rotation_key(41)).await).await;
    assert_eq!(token_generation(&g2), 2, "the second rotation");
    assert_eq!(row_generation(&agent_row(&app, &agent_id).await), 2);

    assert_eq!(
        decisions(&audit_rows(&app, &agent_id).await),
        vec![
            decision("first", None),
            decision("rate_limited", None),
            decision("replay", None),
            decision("first", None),
        ]
    );
}

/// A row enrolled before FR-103 has no `token_gen` field (read as 0), and
/// its token no `gen` claim (also 0). It rotates from 0 to 1 like any other.
#[tokio::test]
async fn a_row_without_a_generation_field_rotates_from_zero() {
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155nogen").await;
    let (agent_id, _) = enroll(&app, &seeded, "m-fr155-nogen").await;
    app.db
        .collection::<Document>("agents")
        .update_one(
            doc! { "_id": oid(&agent_id) },
            doc! { "$unset": { "token_gen": "" } },
        )
        .await
        .unwrap();
    let token = forge_agent_token(&app, &agent_id, &seeded.tenant_id, 3600, None);

    let new = answered_token(rotate(&app, &token, &rotation_key(50)).await).await;
    assert_eq!(token_generation(&new), 1);
    assert_eq!(int(&agent_row(&app, &agent_id).await, "token_gen"), Some(1));
}

/// The commit's filter takes the repository's live predicate,
/// `deleted_at: null`, which matches a live row with or without the field
/// (#1821). A live row that has no `deleted_at` field rotates like any other.
#[tokio::test]
async fn a_live_row_without_a_deleted_at_field_rotates() {
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155nodel").await;
    let (agent_id, token) = enroll(&app, &seeded, "m-fr155-nodel").await;
    app.db
        .collection::<Document>("agents")
        .update_one(
            doc! { "_id": oid(&agent_id) },
            doc! { "$unset": { "deleted_at": "" } },
        )
        .await
        .unwrap();
    assert!(
        !agent_row(&app, &agent_id).await.contains_key("deleted_at"),
        "the row has no deleted_at field"
    );

    let new = answered_token(rotate(&app, &token, &rotation_key(60)).await).await;
    assert_eq!(token_generation(&new), 1);
    assert_eq!(int(&agent_row(&app, &agent_id).await, "token_gen"), Some(1));
}

/// A rotation key that is not the standard base64 of 32 bytes is answered
/// 400 and audited `invalid_request`, and the row is not written: text that
/// is not base64, and the base64 of 16 bytes.
#[tokio::test]
async fn a_malformed_rotation_key_is_refused() {
    use base64::Engine as _;
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155badkey").await;
    let (agent_id, token) = enroll(&app, &seeded, "m-fr155-badkey").await;

    let sixteen_bytes = base64::engine::general_purpose::STANDARD.encode([7u8; 16]);
    for key in ["not base64", sixteen_bytes.as_str()] {
        let resp = rotate(&app, &token, key).await;
        assert_eq!(resp.status(), 400, "{key}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"], "bad_request", "{body}");
    }
    let device = agent_row(&app, &agent_id).await;
    assert_eq!(row_generation(&device), 0, "the row is not written");
    assert!(!device.contains_key("token_rotation"), "{device}");

    let rows = audit_rows(&app, &agent_id).await;
    assert_eq!(
        decisions(&rows),
        vec![decision("refused", Some("invalid_request")); 2]
    );
    for row in &rows {
        assert_eq!(int(row, "gen_from"), Some(0), "{row}");
        assert_eq!(int(row, "gen_to"), Some(0), "{row}");
    }
}

/// A token past its `exp` is answered 401 and audited `expired`, and the row
/// is not written: it renews first. This one is a few seconds past its
/// `exp`, inside the token library's leeway, so it verifies and the route's
/// own `exp` check refuses it (further past, the token check itself refuses
/// it, with no audit row).
#[tokio::test]
async fn an_expired_token_does_not_rotate() {
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155expired").await;
    let (agent_id, _) = enroll(&app, &seeded, "m-fr155-expired").await;
    let expired = forge_agent_token(&app, &agent_id, &seeded.tenant_id, -5, Some(0));

    let resp = rotate(&app, &expired, &rotation_key(70)).await;
    assert_eq!(resp.status(), 401, "a token five seconds past its exp");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "unauthorized", "{body}");
    assert_eq!(
        row_generation(&agent_row(&app, &agent_id).await),
        0,
        "the row is not written"
    );

    let rows = audit_rows(&app, &agent_id).await;
    assert_eq!(decisions(&rows), vec![decision("refused", Some("expired"))]);
    assert_eq!(int(&rows[0], "gen_from"), Some(0));
    assert_eq!(int(&rows[0], "gen_to"), Some(0));
}

/// A token of a generation the rotation does not take (neither the row's,
/// nor the one below it when a rotation made the row's) is answered 401 and
/// audited `generation`, and the row is not written: a token minted before
/// the device was removed, on the row its re-enrollment revived (one below
/// the row's, which the removal made), and a token of a generation above the
/// row's.
#[tokio::test]
async fn a_token_of_another_generation_does_not_rotate() {
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155othergen").await;
    let (agent_id, before_removal) = enroll(&app, &seeded, "m-fr155-othergen").await;
    remove(&app, &seeded, &agent_id).await;
    let (again, _) = enroll(&app, &seeded, "m-fr155-othergen").await;
    assert_eq!(again, agent_id, "the same row is revived");
    assert_eq!(
        row_generation(&agent_row(&app, &agent_id).await),
        1,
        "the removal's generation"
    );
    let above = forge_agent_token(&app, &agent_id, &seeded.tenant_id, 3600, Some(5));

    for (token, key, generation) in [
        (&before_removal, rotation_key(80), 0),
        (&above, rotation_key(81), 5),
    ] {
        let resp = rotate(&app, token, &key).await;
        assert_eq!(resp.status(), 401, "a token of generation {generation}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"], "unauthorized", "{body}");
    }
    let device = agent_row(&app, &agent_id).await;
    assert_eq!(row_generation(&device), 1, "the row is not written");
    assert!(!device.contains_key("token_rotation"), "{device}");

    let rows = audit_rows(&app, &agent_id).await;
    assert_eq!(
        decisions(&rows),
        vec![decision("refused", Some("generation")); 2]
    );
    assert_eq!(int(&rows[0], "gen_from"), Some(0));
    assert_eq!(int(&rows[1], "gen_from"), Some(5));
    for row in &rows {
        assert_eq!(int(row, "gen_to"), Some(1), "{row}");
    }
}

/// A first call that commits writes the new token's expiry on the row
/// (`token_exp`, which the devices list shows): the answer's `expires_at` to
/// the second, and the new token's own `exp` or up to a few seconds below
/// it, since the token is minted after the commit.
#[tokio::test]
async fn a_rotation_writes_the_new_tokens_expiry_on_the_row() {
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155exp").await;
    let (agent_id, token) = enroll(&app, &seeded, "m-fr155-exp").await;
    // Whatever enrollment wrote, the row starts without one.
    app.db
        .collection::<Document>("agents")
        .update_one(
            doc! { "_id": oid(&agent_id) },
            doc! { "$unset": { "token_exp": "" } },
        )
        .await
        .unwrap();

    let resp = rotate(&app, &token, &rotation_key(90)).await;
    assert_eq!(resp.status(), 200, "a first call");
    let body: Value = resp.json().await.unwrap();
    let expires_at = chrono::DateTime::parse_from_rfc3339(body["expires_at"].as_str().unwrap())
        .expect("expires_at is RFC 3339")
        .timestamp();
    let token_exp = token_claims(body["agent_token"].as_str().unwrap())["exp"]
        .as_i64()
        .expect("an exp claim");

    let written = agent_row(&app, &agent_id)
        .await
        .get_datetime("token_exp")
        .expect("the row's token_exp")
        .timestamp_millis()
        / 1000;
    assert_eq!(written, expires_at, "the answer's expires_at");
    assert!(
        (0..=5).contains(&(token_exp - written)),
        "the new token's exp {token_exp}, the row's {written}"
    );
    assert_eq!(
        decisions(&audit_rows(&app, &agent_id).await),
        vec![decision("first", None)]
    );
}

/// A token whose row is gone (a reaped ephemeral device's row is deleted
/// outright) is answered 401 and audited `row_gone`, with no `gen_to`: there
/// was no row to read.
#[tokio::test]
async fn a_token_whose_row_is_gone_does_not_rotate() {
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155gone").await;
    let (agent_id, token) = enroll(&app, &seeded, "m-fr155-gone").await;
    let deleted = app
        .db
        .collection::<Document>("agents")
        .delete_one(doc! { "_id": oid(&agent_id) })
        .await
        .unwrap();
    assert_eq!(deleted.deleted_count, 1, "the row is deleted");

    let resp = rotate(&app, &token, &rotation_key(100)).await;
    assert_eq!(resp.status(), 401, "no row");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "unauthorized", "{body}");

    let rows = audit_rows(&app, &agent_id).await;
    assert_eq!(
        decisions(&rows),
        vec![decision("refused", Some("row_gone"))]
    );
    assert_eq!(int(&rows[0], "gen_from"), Some(0));
    assert!(!rows[0].contains_key("gen_to"), "{}", rows[0]);
}

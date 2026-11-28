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
//! P1b and P1c: the device's agent sockets below the new generation end on
//! every pod, and a registration admits only the device's current
//! generation. P1d: the same holds for its DERP registrations, and the
//! rotation takes its overlay node offline and orders a new overlay key.
//! P1e: the work a commit starts runs to its end, and an answer asked again
//! waits for it.
//!
//! Every server here starts with `rc.agent_token_rotation` on (its default is
//! off until the phase that completes the server's half), except where a
//! cell is about the switch.

use crate::agent_presence_tests::{connect_agent, open_agent_socket, say_hello};
use crate::agent_removal_tests::{Heard, listen, still_served, wait_registered};
use crate::fixtures::test_app::TestApp;
use crate::retired_key_tests::{self as rk, Ws};
use crate::token_renewal_tests::{enroll, forge_agent_token, renew, self_devices, upgrade_status};
use bson::{Bson, DateTime, Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

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

/// T1: a rotation answers a token of the next generation, the row moves to
/// it, and a token of the generation before is refused on the HTTP routes
/// with the rotation's own words (P1a). The agent socket's upgrade and a DERP
/// dial admit the new token and answer 401 to the one before, and a renewal
/// with the one before is refused after the commit (P1b).
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

    // P1b — every other place an agent token is checked.
    assert_eq!(
        upgrade_status(&app, "/ws", &new).await,
        101,
        "the agent socket admits the new token"
    );
    assert_eq!(
        upgrade_status(&app, "/ws", &old).await,
        401,
        "the agent socket refuses the one before"
    );
    assert_eq!(
        upgrade_status(&app, "/derp", &new).await,
        101,
        "a DERP dial admits the new token"
    );
    assert_eq!(
        upgrade_status(&app, "/derp", &old).await,
        401,
        "a DERP dial refuses the one before"
    );
    assert_eq!(
        renew(&app, &old).await.status(),
        401,
        "a renewal with the one before, after the commit"
    );
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

/// The socket ended with no goodbye: the commit tells the device nothing on
/// it (`AgentDeleted` would stop the daemon), and the device that rotated is
/// already connecting with the new token.
fn assert_ended(heard: Heard, place: &str) {
    match heard {
        Heard::Closed => {}
        other => panic!(
            "{place}: a rotation ends the device's socket of the generation before \
             within 5 s, with no goodbye; heard {other:?}"
        ),
    }
}

/// T4 (its P1b half): the commit ends the device's agent socket admitted
/// below the new generation within 5 s, on the pod the rotation reached and,
/// through the control channel, on any other. A socket opened at the new
/// generation survives the rotation's envelope arriving late. The DERP
/// registration's half is the next cell (P1d).
#[tokio::test]
async fn a_rotation_ends_the_connections_of_earlier_generations() {
    let (a, b) = TestApp::spawn_pair(|s| s.rc.agent_token_rotation = true).await;
    let seeded = a.seed_tenant("fr155ends").await;

    // The pod the rotation reaches holds the socket.
    let (here, old) = enroll(&a, &seeded, "m-fr155-ends-here").await;
    let mut socket = connect_agent(&a, &old).await;
    let here_id = wait_registered(&a, &here).await;
    let new = answered_token(rotate(&a, &old, &rotation_key(110)).await).await;
    assert_ended(
        listen(&mut socket, Duration::from_secs(5)).await,
        "on the pod the rotation reached",
    );
    assert!(
        !a.state.fleet().rc_hub.is_agent_online(here_id),
        "out of the hub"
    );

    // A socket at the new generation, then the rotation's envelope, late.
    let mut fresh = connect_agent(&a, &new).await;
    wait_registered(&a, &here).await;
    a.state.modules.apply_rc_ctrl(&json!({
        "class": "rc",
        "evt": "end_below_generation",
        "agent_id": here,
        "tenant_id": seeded.tenant_id.to_string(),
        "generation": token_generation(&new),
    }));
    assert!(
        still_served(&mut fresh).await,
        "a late envelope ends nothing at the new generation"
    );
    assert!(a.state.fleet().rc_hub.is_agent_online(here_id));

    if a.state.redis_pubsub.is_none() {
        eprintln!("skipping the second pod: no Redis available");
        return;
    }
    // Another pod holds the socket.
    let (there, old) = enroll(&a, &seeded, "m-fr155-ends-there").await;
    let mut socket = connect_agent(&b, &old).await;
    let there_id = wait_registered(&b, &there).await;
    assert!(
        !a.state.fleet().rc_hub.is_agent_online(there_id),
        "the socket is on the second pod only"
    );
    let _ = answered_token(rotate(&a, &old, &rotation_key(111)).await).await;
    assert_ended(
        listen(&mut socket, Duration::from_secs(5)).await,
        "on another pod",
    );
    assert!(!b.state.fleet().rc_hub.is_agent_online(there_id));
}

/// The device's overlay node row, as its join writes it: what a DERP
/// registration resolves and checks its key against. Returns the network.
async fn seed_node(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    agent_id: &str,
    machine: &str,
    key: [u8; 32],
    overlay_ip: &str,
) -> ObjectId {
    use base64::Engine as _;
    use roomler_ai_services::dao::{overlay_network::OverlayNetworkDao, overlay_node};
    let tenant_id = oid(&seeded.tenant_id);
    let network_id = OverlayNetworkDao::new(&app.db)
        .get_or_create(tenant_id)
        .await
        .unwrap()
        .id
        .unwrap();
    overlay_node::OverlayNodeDao::new(&app.db)
        .create(overlay_node::NewOverlayNode {
            tenant_id,
            node_ref: roomler_ai_remote_control::models::NodeRef::Agent {
                agent_id: oid(agent_id),
            },
            network_id,
            machine_id: machine.to_string(),
            name: machine.to_string(),
            overlay_ip: overlay_ip.to_string(),
            wg_public_key: base64::engine::general_purpose::STANDARD.encode(key),
            key_epoch: 0,
            endpoints: vec![],
            supports_quic: false,
            supports_relay_single: false,
            supports_derp: true,
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
    network_id
}

/// A DERP socket of `token`, upgraded; `send_registration` sends its
/// registration frame.
async fn open_derp(app: &TestApp, token: &str) -> Ws {
    let url = format!("ws://{}/derp?token={}", app.addr, rk::urlencode(token));
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("a DERP upgrade");
    ws
}

/// The registration frame: the node's key.
async fn send_registration(ws: &mut Ws, key: [u8; 32]) {
    ws.send(Message::Binary(key.to_vec().into()))
        .await
        .expect("the registration frame");
}

fn derp_registered(app: &TestApp, network: ObjectId, key: [u8; 32]) -> bool {
    app.state
        .network()
        .derp_registry
        .contains_key(&(network, key))
}

/// Wait until `key` is registered on `app`'s pod, or until it is not.
async fn wait_derp(app: &TestApp, network: ObjectId, key: [u8; 32], registered: bool) {
    for _ in 0..100 {
        if derp_registered(app, network, key) == registered {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the DERP registration never became registered={registered}");
}

/// Whether the DERP socket closes within `wait`.
async fn derp_closes(ws: &mut Ws, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Err(_) => return false,
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return true,
            Ok(Some(Ok(_))) => {}
        }
    }
}

/// Whether a frame that `from`, registered under `from_key`, sends to
/// `to_key` reaches `to`, with the sender's key in front of the payload.
async fn derp_relays(from: &mut Ws, from_key: [u8; 32], to: &mut Ws, to_key: [u8; 32]) -> bool {
    let payload = b"fr155-p1d";
    let mut frame = to_key.to_vec();
    frame.extend_from_slice(payload);
    if from.send(Message::Binary(frame.into())).await.is_err() {
        return false;
    }
    let mut relayed = from_key.to_vec();
    relayed.extend_from_slice(payload);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, to.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) if b[..] == relayed[..] => return true,
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return false,
        }
    }
}

/// T4, its DERP half (P1d): the commit ends the device's DERP registration
/// admitted below the new generation within 5 s, on the pod the rotation
/// reached and, through the control channel, on any other. A registration at
/// the new generation survives the rotation's event arriving late.
#[tokio::test]
async fn a_rotation_ends_the_derp_registrations_of_earlier_generations() {
    let (a, b) = TestApp::spawn_pair(|s| s.rc.agent_token_rotation = true).await;
    let seeded = a.seed_tenant("fr155derpends").await;

    // The pod the rotation reaches holds the registration.
    let (here, old) = enroll(&a, &seeded, "m-fr155-derp-ends-here").await;
    let mine = [0x41; 32];
    let network = seed_node(
        &a,
        &seeded,
        &here,
        "m-fr155-derp-ends-here",
        mine,
        "100.64.4.1",
    )
    .await;
    let mut derp = open_derp(&a, &old).await;
    send_registration(&mut derp, mine).await;
    wait_derp(&a, network, mine, true).await;
    let new = answered_token(rotate(&a, &old, &rotation_key(112)).await).await;
    assert!(
        derp_closes(&mut derp, Duration::from_secs(5)).await,
        "on the pod the rotation reached, a rotation ends the device's DERP registration of \
         the generation before within 5 s"
    );
    wait_derp(&a, network, mine, false).await;

    // A registration at the new generation, then the rotation's event, late.
    let mut fresh = open_derp(&a, &new).await;
    send_registration(&mut fresh, mine).await;
    wait_derp(&a, network, mine, true).await;
    a.state.modules.apply_rc_ctrl(&json!({
        "class": "rc",
        "evt": "end_below_generation",
        "agent_id": here,
        "tenant_id": seeded.tenant_id.to_string(),
        "generation": token_generation(&new),
    }));
    assert!(
        !derp_closes(&mut fresh, Duration::from_millis(1500)).await,
        "a late event ends no registration at the new generation"
    );
    assert!(derp_registered(&a, network, mine), "it stays registered");
    fresh.close(None).await.ok();

    if a.state.redis_pubsub.is_none() {
        eprintln!("skipping the second pod: no Redis available");
        return;
    }
    // Another pod holds the registration. Another org, so that every DERP
    // socket of its network is on that pod.
    let other = a.seed_tenant("fr155derpends2").await;
    let (there, old) = enroll(&a, &other, "m-fr155-derp-ends-there").await;
    let theirs = [0x42; 32];
    let network = seed_node(
        &a,
        &other,
        &there,
        "m-fr155-derp-ends-there",
        theirs,
        "100.64.4.2",
    )
    .await;
    let mut derp = open_derp(&b, &old).await;
    send_registration(&mut derp, theirs).await;
    wait_derp(&b, network, theirs, true).await;
    let _ = answered_token(rotate(&a, &old, &rotation_key(113)).await).await;
    assert!(
        derp_closes(&mut derp, Duration::from_secs(5)).await,
        "on another pod, a rotation ends the device's DERP registration of the generation \
         before within 5 s"
    );
    wait_derp(&b, network, theirs, false).await;
}

/// Whether `ws` hears, within `wait`, a netmap delta that marks the node
/// `node` (hex) unreachable.
async fn heard_unreachable(ws: &mut Ws, node: &str, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(delta) = rk::recv_t(ws, "rc:overlay.netmap_delta", left).await else {
            return false;
        };
        if delta["upserts"].as_array().is_some_and(|upserts| {
            upserts
                .iter()
                .any(|u| u["node_id"] == node && u["reachable"] == false)
        }) {
            return true;
        }
    }
}

/// T5 (P1d): a rotation takes the device's overlay node offline and orders a
/// new overlay key. The node is Offline and a peer hears it unreachable. The
/// row carries the server's `credential_rotation` order, queued, naming the
/// key the device held, and the device's connection at G+1 is handed it.
#[tokio::test]
async fn a_rotation_takes_the_node_offline_and_orders_a_key() {
    let app = spawn_on().await;
    let seeded = app.seed_tenant("fr155node").await;
    let (agent_id, old) = enroll(&app, &seeded, "m-fr155-node").await;
    let (_, peer_token) = enroll(&app, &seeded, "m-fr155-node-peer").await;
    let mut device = rk::connect(&app, &old, &["key-rotate"]).await;
    rk::join_admitted(&mut device, 81).await;
    let mut peer = rk::connect(&app, &peer_token, &[]).await;
    rk::join_admitted(&mut peer, 82).await;
    let node = rk::node_row(&app, &seeded, "m-fr155-node").await;
    assert_eq!(node.get_str("status").ok(), Some("online"), "{node}");
    let node_id = node.get_object_id("_id").unwrap().to_hex();

    let new = answered_token(rotate(&app, &old, &rotation_key(161)).await).await;
    assert!(
        heard_unreachable(&mut peer, &node_id, Duration::from_secs(5)).await,
        "a rotation takes the node offline, and a peer's netmap marks it unreachable"
    );
    let node = rk::node_row(&app, &seeded, "m-fr155-node").await;
    assert_eq!(node.get_str("status").ok(), Some("offline"), "{node}");

    let row = agent_row(&app, &agent_id).await;
    let order = row
        .get_document("key_rotation")
        .unwrap_or_else(|_| panic!("a key order on the row: {row}"));
    assert_eq!(
        order.get_str("trigger").ok(),
        Some("credential_rotation"),
        "{order}"
    );
    assert_eq!(
        order.get_object_id("requested_by").ok(),
        Some(ObjectId::from_bytes([0; 12])),
        "the server's order: {order}"
    );
    assert_eq!(
        order.get_str("public_key_before").ok(),
        Some(rk::key(81).as_str()),
        "the key the device held: {order}"
    );
    assert!(
        !order.contains_key("delivered_at"),
        "queued for the next connection: {order}"
    );
    let request_id = order.get_str("request_id").unwrap().to_string();

    let mut fresh = rk::connect(&app, &new, &["key-rotate"]).await;
    let delivered = rk::recv_t(&mut fresh, "rc:agent.key_rotate", Duration::from_secs(5))
        .await
        .unwrap_or_else(|| panic!("the order is delivered at the next connection, at G+1"));
    assert_eq!(
        delivered["request_id"].as_str(),
        Some(request_id.as_str()),
        "{delivered}"
    );
}

/// With `rc.agent_token_rotation` off, its default, a first call is answered
/// 404 and ends nothing: the device's socket stays served on the pod the call
/// reached and on another, and a new socket of the same generation registers,
/// so no floor was kept.
#[tokio::test]
async fn a_rotation_switched_off_ends_no_connection() {
    let (a, b) = TestApp::spawn_pair(|_| {}).await;
    let seeded = a.seed_tenant("fr155offends").await;

    let (here, token) = enroll(&a, &seeded, "m-fr155-off-here").await;
    let mut socket = connect_agent(&a, &token).await;
    wait_registered(&a, &here).await;
    assert_eq!(rotate(&a, &token, &rotation_key(120)).await.status(), 404);
    assert!(still_served(&mut socket).await, "the socket stays served");
    // The same generation again: it registers, and displaces the first.
    let mut again = connect_agent(&a, &token).await;
    match listen(&mut socket, Duration::from_secs(5)).await {
        Heard::Goodbye(v) => assert_eq!(v["reason"], "replaced_by_newer_connection", "{v}"),
        other => panic!(
            "a second socket of the same generation registers and displaces the \
             first; heard {other:?}"
        ),
    }
    assert!(still_served(&mut again).await, "no floor refused it");

    if a.state.redis_pubsub.is_none() {
        eprintln!("skipping the second pod: no Redis available");
        return;
    }
    let (there, token) = enroll(&a, &seeded, "m-fr155-off-there").await;
    let mut socket = connect_agent(&b, &token).await;
    wait_registered(&b, &there).await;
    assert_eq!(rotate(&a, &token, &rotation_key(121)).await.status(), 404);
    assert!(
        still_served(&mut socket).await,
        "nothing reached the second pod"
    );
}

/// How often the pods of T17 and T18 run their generation check
/// (`rc.presence_sweep_secs`, which is never under 5 s).
const CHECK_SECS: u64 = 5;

/// A pod over `db` with the rotation on and no Redis: the control event of a
/// rotation it commits reaches no other pod. Its own check never runs within
/// a cell.
async fn spawn_without_redis(db: &str) -> TestApp {
    spawn_sharing(db, "unheard", |s| {
        s.rc.agent_token_rotation = true;
        s.redis.url = String::new();
        s.rc.presence_sweep_secs = 3600;
    })
    .await
}

/// The generation of an agent token, as the hub holds it.
fn generation_of(token: &str) -> u32 {
    u32::try_from(token_generation(token)).expect("a generation the hub holds")
}

/// Wait until the device has no entry on `app`'s pod.
async fn wait_unregistered(app: &TestApp, agent: ObjectId) {
    for _ in 0..100 {
        if !app.state.fleet().rc_hub.is_agent_online(agent) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the device's entry never left its pod");
}

/// The socket was refused at its hello: it closed with no goodbye.
fn assert_refused(heard: Heard, place: &str) {
    match heard {
        Heard::Closed => {}
        other => panic!(
            "{place}: a registration admits only the device's current generation, so a \
             socket of the generation before is refused at its hello; heard {other:?}"
        ),
    }
}

/// T15 (P1c): a registration admits only the device's current generation,
/// whatever the time from its upgrade to its hello. Two sockets are upgraded
/// at G, then the token rotates to G+1 on a pod without Redis, so the only
/// floor this pod keeps is the one the cell sets, for 1 s
/// (`end_below_generation_for`). After that floor ended, each socket's hello
/// is refused: the first with a socket of G+1 registered, which stays served
/// and hears no goodbye, and the second with none, leaving no entry.
#[tokio::test]
async fn a_registration_admits_only_the_current_generation() {
    let db = format!("roomler_ai_test_{}", uuid::Uuid::new_v4().simple());
    // No round of this pod's own check runs within the cell: what refuses
    // here is the registration.
    let here = spawn_sharing(&db, "here", |s| {
        s.rc.agent_token_rotation = true;
        s.rc.presence_sweep_secs = 3600;
    })
    .await;
    let rotating = spawn_without_redis(&db).await;
    let seeded = here.seed_tenant("fr155hello").await;
    let (agent_id, old) = enroll(&here, &seeded, "m-fr155-hello").await;
    let (agent, tenant) = (oid(&agent_id), oid(&seeded.tenant_id));
    let hub = &here.state.fleet().rc_hub;

    let mut first = open_agent_socket(&here, &old).await;
    let mut second = open_agent_socket(&here, &old).await;
    let new = answered_token(rotate(&rotating, &old, &rotation_key(130)).await).await;
    assert!(
        !hub.end_below_generation_for(agent, tenant, generation_of(&new), Duration::from_secs(1)),
        "no entry to end yet"
    );
    let mut current = connect_agent(&here, &new).await;
    wait_registered(&here, &agent_id).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    say_hello(&mut first).await;
    assert_refused(
        listen(&mut first, Duration::from_secs(5)).await,
        "with a socket of G+1 registered",
    );
    assert!(
        still_served(&mut current).await,
        "the socket of G+1 stays served and hears no goodbye"
    );

    current.close(None).await.expect("close the socket of G+1");
    wait_unregistered(&here, agent).await;
    say_hello(&mut second).await;
    assert_refused(
        listen(&mut second, Duration::from_secs(5)).await,
        "with no socket registered",
    );
    assert!(
        !hub.is_agent_online(agent),
        "a refused registration leaves no entry"
    );
}

/// T17 (P1c): every pod ends an entry below its row's generation within one
/// period of its check (`rc.presence_sweep_secs`), also a pod that the
/// rotation's control event does not reach. The socket registers at G on a
/// pod with Redis, and the token rotates to G+1 on a pod without Redis, so
/// its event reaches no other pod. The socket ends within one period and a
/// margin for the check's read, and a socket of G+1 then stays served
/// through two periods.
#[tokio::test]
async fn every_pod_ends_an_entry_below_its_rows_generation() {
    let db = format!("roomler_ai_test_{}", uuid::Uuid::new_v4().simple());
    let here = spawn_sharing(&db, "here", |s| {
        s.rc.agent_token_rotation = true;
        s.rc.presence_sweep_secs = CHECK_SECS;
    })
    .await;
    let rotating = spawn_without_redis(&db).await;
    let seeded = here.seed_tenant("fr155check").await;
    let (agent_id, old) = enroll(&here, &seeded, "m-fr155-check").await;
    let mut socket = connect_agent(&here, &old).await;
    let agent = wait_registered(&here, &agent_id).await;

    let new = answered_token(rotate(&rotating, &old, &rotation_key(140)).await).await;
    match listen(&mut socket, Duration::from_secs(CHECK_SECS + 5)).await {
        Heard::Closed => {}
        other => panic!(
            "every pod ends an entry below its row's generation within one period of its \
             check, with no goodbye; heard {other:?}"
        ),
    }
    assert!(
        !here.state.fleet().rc_hub.is_agent_online(agent),
        "out of the hub"
    );

    let mut current = connect_agent(&here, &new).await;
    wait_registered(&here, &agent_id).await;
    tokio::time::sleep(Duration::from_secs(2 * CHECK_SECS)).await;
    assert!(
        still_served(&mut current).await,
        "an entry at the row's generation stays"
    );
}

/// T18 (P1c, T14's extension): on a pod with `rc.agent_token_rotation` off,
/// nothing that P1c adds runs, beside a rotation committed on a pod with it
/// on (and without Redis, so nothing reaches this pod). In T15's setup the
/// pod refuses nothing new, and a registration replaces the entry as it
/// always has (`replaced_by_newer_connection`). In T17's setup the pod ends
/// nothing new through two of the check's periods.
#[tokio::test]
async fn a_pod_switched_off_ends_and_refuses_nothing_new() {
    let db = format!("roomler_ai_test_{}", uuid::Uuid::new_v4().simple());
    let off = spawn_sharing(&db, "off", |s| {
        s.rc.agent_token_rotation = false;
        s.rc.presence_sweep_secs = CHECK_SECS;
    })
    .await;
    let rotating = spawn_without_redis(&db).await;
    let seeded = off.seed_tenant("fr155offnew").await;
    let tenant = oid(&seeded.tenant_id);

    // T17's setup: a socket registered at G, then a rotation.
    let (kept_id, kept_token) = enroll(&off, &seeded, "m-fr155-off-kept").await;
    let mut kept = connect_agent(&off, &kept_token).await;
    wait_registered(&off, &kept_id).await;
    let _ = answered_token(rotate(&rotating, &kept_token, &rotation_key(150)).await).await;

    // T15's setup: a socket upgraded at G, a rotation, this pod's floor for
    // it ending within the cell, and a socket of G+1 registered.
    let (late_id, late_token) = enroll(&off, &seeded, "m-fr155-off-late").await;
    let mut late = open_agent_socket(&off, &late_token).await;
    let new = answered_token(rotate(&rotating, &late_token, &rotation_key(151)).await).await;
    off.state.fleet().rc_hub.end_below_generation_for(
        oid(&late_id),
        tenant,
        generation_of(&new),
        Duration::from_secs(1),
    );
    let mut current = connect_agent(&off, &new).await;
    wait_registered(&off, &late_id).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    say_hello(&mut late).await;
    match listen(&mut current, Duration::from_secs(5)).await {
        Heard::Goodbye(v) => assert_eq!(v["reason"], "replaced_by_newer_connection", "{v}"),
        other => {
            panic!(
                "with the switch off, a registration replaces the entry as it always has; heard {other:?}"
            )
        }
    }
    assert!(
        still_served(&mut late).await,
        "with the switch off, the pod refuses nothing new"
    );

    tokio::time::sleep(Duration::from_secs(2 * CHECK_SECS)).await;
    assert!(
        still_served(&mut kept).await,
        "with the switch off, the pod ends nothing new"
    );
}

/// T19 (P1d): a DERP registration admits only the device's current
/// generation, whatever the timing. Two DERP sockets are upgraded at G, then
/// the token rotates to G+1 on a pod without Redis, and the cell sets this
/// pod's floor for 1 s (`DerpGenerations::end_below_generation_for`), so the
/// registration's read decides. Each socket's registration frame is refused:
/// the first with a registration of G+1 in place, which stays registered and
/// keeps relaying, and the second with none, which leaves the key
/// unregistered.
#[tokio::test]
async fn a_derp_registration_admits_only_the_current_generation() {
    let db = format!("roomler_ai_test_{}", uuid::Uuid::new_v4().simple());
    // No round of this pod's own check runs within the cell: what refuses
    // here is the registration.
    let here = spawn_sharing(&db, "here", |s| {
        s.rc.agent_token_rotation = true;
        s.rc.presence_sweep_secs = 3600;
    })
    .await;
    let rotating = spawn_without_redis(&db).await;
    let seeded = here.seed_tenant("fr155derphello").await;
    let (agent_id, old) = enroll(&here, &seeded, "m-fr155-derp-hello").await;
    let (peer_id, peer_token) = enroll(&here, &seeded, "m-fr155-derp-hello-peer").await;
    let (mine, theirs) = ([0x51; 32], [0x52; 32]);
    let network = seed_node(
        &here,
        &seeded,
        &agent_id,
        "m-fr155-derp-hello",
        mine,
        "100.64.19.1",
    )
    .await;
    seed_node(
        &here,
        &seeded,
        &peer_id,
        "m-fr155-derp-hello-peer",
        theirs,
        "100.64.19.2",
    )
    .await;
    let mut peer = open_derp(&here, &peer_token).await;
    send_registration(&mut peer, theirs).await;
    wait_derp(&here, network, theirs, true).await;

    let mut first = open_derp(&here, &old).await;
    let mut second = open_derp(&here, &old).await;
    let new = answered_token(rotate(&rotating, &old, &rotation_key(190)).await).await;
    assert_eq!(
        here.state
            .network()
            .derp_generations
            .end_below_generation_for(
                oid(&agent_id),
                oid(&seeded.tenant_id),
                generation_of(&new),
                Duration::from_secs(1),
            ),
        0,
        "no registration to end yet"
    );
    let mut current = open_derp(&here, &new).await;
    send_registration(&mut current, mine).await;
    wait_derp(&here, network, mine, true).await;
    assert!(
        derp_relays(&mut peer, theirs, &mut current, mine).await,
        "the registration of G+1 relays"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;

    send_registration(&mut first, mine).await;
    assert!(
        derp_closes(&mut first, Duration::from_secs(5)).await,
        "with a registration of G+1 in place: a DERP registration admits only the device's \
         current generation, so a socket of the generation before is refused at its \
         registration frame"
    );
    assert!(
        derp_relays(&mut peer, theirs, &mut current, mine).await,
        "the registration of G+1 stays in place and keeps relaying"
    );
    assert!(
        derp_relays(&mut current, mine, &mut peer, theirs).await,
        "both ways"
    );

    current
        .close(None)
        .await
        .expect("close the registration of G+1");
    wait_derp(&here, network, mine, false).await;
    send_registration(&mut second, mine).await;
    assert!(
        derp_closes(&mut second, Duration::from_secs(5)).await,
        "with no registration in place: a socket of the generation before is refused at its \
         registration frame"
    );
    assert!(
        !derp_registered(&here, network, mine),
        "a refused registration leaves the key unregistered"
    );
}

/// T21 (P1d): every pod ends a DERP registration below its row's generation
/// within one period of its check (`rc.presence_sweep_secs`), also a pod that
/// the rotation's control event does not reach. The registration is made at
/// G on a pod with Redis, and the token rotates to G+1 on a pod without
/// Redis, so its event reaches no other pod. The socket closes within one
/// period and a margin for the check's read, and a registration of G+1 then
/// stays through two periods.
#[tokio::test]
async fn every_pod_ends_a_derp_registration_below_its_rows_generation() {
    let db = format!("roomler_ai_test_{}", uuid::Uuid::new_v4().simple());
    let here = spawn_sharing(&db, "here", |s| {
        s.rc.agent_token_rotation = true;
        s.rc.presence_sweep_secs = CHECK_SECS;
    })
    .await;
    let rotating = spawn_without_redis(&db).await;
    let seeded = here.seed_tenant("fr155derpcheck").await;
    let (agent_id, old) = enroll(&here, &seeded, "m-fr155-derp-check").await;
    let mine = [0x61; 32];
    let network = seed_node(
        &here,
        &seeded,
        &agent_id,
        "m-fr155-derp-check",
        mine,
        "100.64.21.1",
    )
    .await;
    let mut derp = open_derp(&here, &old).await;
    send_registration(&mut derp, mine).await;
    wait_derp(&here, network, mine, true).await;

    let new = answered_token(rotate(&rotating, &old, &rotation_key(210)).await).await;
    assert!(
        derp_closes(&mut derp, Duration::from_secs(CHECK_SECS + 5)).await,
        "every pod ends a DERP registration below its row's generation within one period of \
         its check"
    );
    wait_derp(&here, network, mine, false).await;

    let mut current = open_derp(&here, &new).await;
    send_registration(&mut current, mine).await;
    wait_derp(&here, network, mine, true).await;
    assert!(
        !derp_closes(&mut current, Duration::from_secs(2 * CHECK_SECS)).await,
        "a registration at the row's generation stays"
    );
    assert!(
        derp_registered(&here, network, mine),
        "and stays registered"
    );
}

/// T22 (P1d, T18's extension to DERP): on a pod with
/// `rc.agent_token_rotation` off, nothing that P1d adds to DERP runs, beside a
/// rotation committed on a pod with it on (and without Redis, so nothing
/// reaches this pod). In T19's setup the pod refuses no registration, and
/// last-writer-wins as before. In T21's setup the pod ends no DERP
/// registration through two of the check's periods.
#[tokio::test]
async fn a_pod_switched_off_ends_and_refuses_no_derp_registration() {
    let db = format!("roomler_ai_test_{}", uuid::Uuid::new_v4().simple());
    let off = spawn_sharing(&db, "off", |s| {
        s.rc.agent_token_rotation = false;
        s.rc.presence_sweep_secs = CHECK_SECS;
    })
    .await;
    let rotating = spawn_without_redis(&db).await;
    let seeded = off.seed_tenant("fr155derpoff").await;

    // T21's setup: a registration at G, then a rotation.
    let (kept_id, kept_token) = enroll(&off, &seeded, "m-fr155-derp-off-kept").await;
    let kept_key = [0x71; 32];
    let network = seed_node(
        &off,
        &seeded,
        &kept_id,
        "m-fr155-derp-off-kept",
        kept_key,
        "100.64.22.1",
    )
    .await;
    let mut kept = open_derp(&off, &kept_token).await;
    send_registration(&mut kept, kept_key).await;
    wait_derp(&off, network, kept_key, true).await;
    let _ = answered_token(rotate(&rotating, &kept_token, &rotation_key(220)).await).await;

    // T19's setup: a socket upgraded at G, a rotation, this pod's floor for it
    // set for 1 s, and a registration of G+1 in place.
    let (upgraded_id, upgraded_token) = enroll(&off, &seeded, "m-fr155-derp-off-upgraded").await;
    let (peer_id, peer_token) = enroll(&off, &seeded, "m-fr155-derp-off-peer").await;
    let (upgraded_key, peer_key) = ([0x72; 32], [0x73; 32]);
    seed_node(
        &off,
        &seeded,
        &upgraded_id,
        "m-fr155-derp-off-upgraded",
        upgraded_key,
        "100.64.22.2",
    )
    .await;
    seed_node(
        &off,
        &seeded,
        &peer_id,
        "m-fr155-derp-off-peer",
        peer_key,
        "100.64.22.3",
    )
    .await;
    let mut peer = open_derp(&off, &peer_token).await;
    send_registration(&mut peer, peer_key).await;
    wait_derp(&off, network, peer_key, true).await;
    let mut upgraded = open_derp(&off, &upgraded_token).await;
    let new = answered_token(rotate(&rotating, &upgraded_token, &rotation_key(221)).await).await;
    off.state
        .network()
        .derp_generations
        .end_below_generation_for(
            oid(&upgraded_id),
            oid(&seeded.tenant_id),
            generation_of(&new),
            Duration::from_secs(1),
        );
    let mut current = open_derp(&off, &new).await;
    send_registration(&mut current, upgraded_key).await;
    wait_derp(&off, network, upgraded_key, true).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    send_registration(&mut upgraded, upgraded_key).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        derp_relays(&mut peer, peer_key, &mut upgraded, upgraded_key).await,
        "with the switch off, the pod refuses no registration, and last-writer-wins as before"
    );

    assert!(
        !derp_closes(&mut kept, Duration::from_secs(2 * CHECK_SECS)).await,
        "with the switch off, the pod ends no DERP registration"
    );
    assert!(derp_registered(&off, network, kept_key));
}

/// How long T24 and T25 give each effect of a commit's work, once the seam
/// lets the work go on.
const WORK_WAIT: Duration = Duration::from_secs(10);

/// The bound on an answer asked again while the work of its commit runs
/// (P1e, `rotation_work` in the fleet module).
const REPLAY_WAIT_SECS: u64 = 10;

/// A pair of servers over one database for T24 and T25, with the rotation
/// on. No pod's own check runs within a cell (`rc.presence_sweep_secs`), so
/// only a commit's work ends a connection.
async fn spawn_pair_on() -> (TestApp, TestApp) {
    TestApp::spawn_pair(|s| {
        s.rc.agent_token_rotation = true;
        s.rc.presence_sweep_secs = 3600;
    })
    .await
}

/// Until `works` works of a commit wait at `app`'s seam, at most 10 s.
async fn wait_at_the_seam(app: &TestApp, works: usize) {
    let seam = &app.state.fleet().rotation_seam;
    for _ in 0..200 {
        if seam.waiting() >= works {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the rotation commits, and the work of its commit reaches the seam");
}

/// A rotation sent over a connection of its own, returned once the work of
/// its commit waits at `app`'s seam. The request is complete: the route
/// reads its body before it commits.
async fn rotate_to_the_seam(app: &TestApp, token: &str, key: &str) -> tokio::net::TcpStream {
    use tokio::io::AsyncWriteExt;
    let body = json!({ "rotation_key": key }).to_string();
    let request = format!(
        "POST {ROTATE} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        app.addr,
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect(app.addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    wait_at_the_seam(app, 1).await;
    stream
}

/// A rotation on `app` in a task of its own, so the cell can watch it.
fn spawn_rotate(
    app: &TestApp,
    token: &str,
    key: &str,
) -> tokio::task::JoinHandle<reqwest::Response> {
    let request = app
        .auth_post(ROTATE, token)
        .json(&json!({ "rotation_key": key }));
    tokio::spawn(async move { request.send().await.unwrap() })
}

/// Poll `what` every 100 ms until it holds, at most [`WORK_WAIT`].
async fn within_the_work<F, Fut>(mut what: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + WORK_WAIT;
    loop {
        if what().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// T24 (P1e): the work a rotation's commit starts runs to its end, also when
/// the request that committed drops. The seam holds the work right after the
/// commit, the client closes its connection, and the seam then lets the work
/// go on: the decision is recorded (`first`), the overlay node goes offline,
/// the `credential_rotation` order is queued, and a second server applies the
/// control event's end to the device's socket there.
#[tokio::test]
async fn the_work_a_commit_starts_runs_to_its_end_when_the_request_drops() {
    let (a, b) = spawn_pair_on().await;
    let seeded = a.seed_tenant("fr155work").await;
    let (agent_id, old) = enroll(&a, &seeded, "m-fr155-work").await;
    // The device's socket, and its overlay join, are on the second server
    // when Redis carries the control event there.
    let (socket_pod, place) = if a.state.redis_pubsub.is_some() {
        (&b, "on the second server, by the control event")
    } else {
        eprintln!("no Redis available: the device's socket is on the pod that commits");
        (&a, "on the pod that commits")
    };
    let mut device = rk::connect(socket_pod, &old, &["key-rotate"]).await;
    rk::join_admitted(&mut device, 91).await;
    let node = rk::node_row(&a, &seeded, "m-fr155-work").await;
    assert_eq!(node.get_str("status").ok(), Some("online"), "{node}");

    let seam = a.state.fleet().rotation_seam.hold();
    let connection = rotate_to_the_seam(&a, &old, &rotation_key(241)).await;
    assert_eq!(
        row_generation(&agent_row(&a, &agent_id).await),
        1,
        "the rotation committed"
    );
    drop(connection);
    // Up to 2 s for the server to see the connection close.
    for _ in 0..40 {
        if a.state.fleet().rotation_seam.waiting() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    seam.release();

    let (app, id, tenant) = (&a, agent_id.as_str(), &seeded);
    let recorded = within_the_work(|| async move {
        decisions(&audit_rows(app, id).await).contains(&decision("first", None))
    })
    .await;
    let rows = decisions(&audit_rows(app, id).await);
    assert!(
        recorded,
        "the work a commit starts runs to its end when the request drops: the decision is \
         recorded; rows {rows:?}"
    );
    let offline = within_the_work(|| async move {
        rk::node_row(app, tenant, "m-fr155-work")
            .await
            .get_str("status")
            .ok()
            == Some("offline")
    })
    .await;
    assert!(
        offline,
        "the work a commit starts runs to its end when the request drops: the overlay node \
         goes offline"
    );
    let queued = within_the_work(|| async move {
        agent_row(app, id)
            .await
            .get_document("key_rotation")
            .is_ok_and(|order| {
                order.get_str("trigger").ok() == Some("credential_rotation")
                    && !order.contains_key("delivered_at")
            })
    })
    .await;
    let row = agent_row(app, id).await;
    assert!(
        queued,
        "the work a commit starts runs to its end when the request drops: the \
         `credential_rotation` order is queued; row {row}"
    );
    assert_ended(listen(&mut device, Duration::from_secs(5)).await, place);
}

/// T25 (P1e): an answer asked again waits for the work of its commit, on the
/// pod that committed and on another server. The seam holds the work, and the
/// answer asked again with the same token and key is given once the work has
/// run, after the commit's `first` row. Each wait is bounded: with the work
/// held past [`REPLAY_WAIT_SECS`], the answer asked again is given all the
/// same, on either pod.
#[tokio::test]
async fn an_answer_asked_again_waits_for_the_work_of_its_commit() {
    let (a, b) = spawn_pair_on().await;
    let seeded = a.seed_tenant("fr155wait").await;

    for (place, asked, machine, seed) in [
        ("on the pod that committed", &a, "m-fr155-wait-here", 251_u8),
        ("on another server", &b, "m-fr155-wait-there", 252_u8),
    ] {
        let (agent_id, old) = enroll(&a, &seeded, machine).await;
        let key = rotation_key(seed);
        let seam = a.state.fleet().rotation_seam.hold();
        let first = spawn_rotate(&a, &old, &key);
        wait_at_the_seam(&a, 1).await;
        let mut again = spawn_rotate(asked, &old, &key);
        assert!(
            tokio::time::timeout(Duration::from_millis(1500), &mut again)
                .await
                .is_err(),
            "{place}: an answer asked again waits while the work of its commit runs"
        );
        seam.release();
        let again = tokio::time::timeout(WORK_WAIT, again)
            .await
            .unwrap_or_else(|_| panic!("{place}: the answer asked again, once the work ran"))
            .unwrap();
        assert_eq!(token_generation(&answered_token(again).await), 1, "{place}");
        let first = tokio::time::timeout(WORK_WAIT, first)
            .await
            .unwrap_or_else(|_| panic!("{place}: the commit's own answer"))
            .unwrap();
        assert_eq!(token_generation(&answered_token(first).await), 1, "{place}");
        assert_eq!(
            decisions(&audit_rows(&a, &agent_id).await),
            vec![decision("first", None), decision("replay", None)],
            "{place}: the answer asked again comes after the work of its commit"
        );
    }

    // The bound, on both pods at once: the work of two commits held past it.
    let (_, here) = enroll(&a, &seeded, "m-fr155-wait-bound-here").await;
    let (_, there) = enroll(&a, &seeded, "m-fr155-wait-bound-there").await;
    let (here_key, there_key) = (rotation_key(253), rotation_key(254));
    let seam = a.state.fleet().rotation_seam.hold();
    let firsts = [
        spawn_rotate(&a, &here, &here_key),
        spawn_rotate(&a, &there, &there_key),
    ];
    wait_at_the_seam(&a, 2).await;
    let started = tokio::time::Instant::now();
    let agains = [
        (
            "on the pod that committed",
            spawn_rotate(&a, &here, &here_key),
        ),
        ("on another server", spawn_rotate(&b, &there, &there_key)),
    ];
    for (place, again) in agains {
        let again = tokio::time::timeout(Duration::from_secs(REPLAY_WAIT_SECS + 5), again)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{place}: an answer asked again is given within its bound, also while \
                     the work of its commit is held"
                )
            })
            .unwrap();
        assert_eq!(token_generation(&answered_token(again).await), 1, "{place}");
    }
    assert!(
        tokio::time::Instant::now().duration_since(started)
            >= Duration::from_secs(REPLAY_WAIT_SECS - 1),
        "each answer asked again waited for the work of its commit up to its bound"
    );
    seam.release();
    for first in firsts {
        let first = tokio::time::timeout(WORK_WAIT, first)
            .await
            .expect("the commit's own answer, once the work ran")
            .unwrap();
        assert_eq!(token_generation(&answered_token(first).await), 1);
    }
}

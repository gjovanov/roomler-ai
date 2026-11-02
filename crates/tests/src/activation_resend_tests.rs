// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3199 — the way back from an activation link that ran out.
//!
//! A sign-up's unverified row keeps holding the address, so registering again
//! answers 409, and until this route existed nothing got that person out. What
//! these pin:
//!
//! - the answer is the same for every address and does not wait for any lookup,
//!   so neither its bytes nor its timing say whether an account exists;
//! - a resend is limited per address, across pods, and per client address;
//! - a resend only ever reaches a password sign-up still waiting for activation.
//!
//! The work runs after the answer, so these tests wait for its result, the new
//! activation code. `TestApp` has no mailer, and the code is what the mail
//! would carry.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use bson::{DateTime, Document, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_api::middleware::auth_rate_limit::RESEND_BURST;
use roomler_ai_config::Settings;
use serde_json::{Value, json};
use tokio::task::JoinSet;

use crate::fixtures::test_app::TestApp;

const RESEND: &str = "/api/auth/resend-activation";
const PASSWORD: &str = "Password123!";

/// How long the pool is held in the timing test. Long against the ceiling on an
/// answer, so a handler that waited for a connection cannot pass by luck.
const HOLD_MS: u64 = 3_000;
const ANSWER_CEILING_MS: u64 = 1_000;

/// One proxy hop, so a test can be several clients: the entry left of the hop
/// is the client address (`roomler_core::client_ip`).
fn behind_a_proxy(s: &mut Settings) {
    s.app.rate_limit_trusted_hops = 1;
}

async fn sign_up(app: &TestApp, email: &str, username: &str) -> reqwest::Response {
    app.client
        .post(app.url("/api/auth/register"))
        .json(&json!({
            "email": email,
            "username": username,
            "display_name": "Late",
            "password": PASSWORD,
        }))
        .send()
        .await
        .unwrap()
}

async fn signed_up(app: &TestApp, email: &str, username: &str) {
    assert_eq!(sign_up(app, email, username).await.status().as_u16(), 201);
}

async fn ask(app: &TestApp, email: &str) -> u16 {
    app.client
        .post(app.url(RESEND))
        .json(&json!({ "email": email }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn ask_from(app: &TestApp, client: &str, email: &str) -> reqwest::Response {
    app.client
        .post(app.url(RESEND))
        .header("X-Forwarded-For", format!("{client}, 10.10.0.1"))
        .json(&json!({ "email": email }))
        .send()
        .await
        .unwrap()
}

fn users(app: &TestApp) -> mongodb::Collection<Document> {
    app.db.collection("users")
}

async fn row(app: &TestApp, email: &str) -> Document {
    users(app)
        .find_one(doc! { "email": email })
        .await
        .unwrap()
        .expect("the row exists")
}

async fn user_id(app: &TestApp, email: &str) -> ObjectId {
    row(app, email).await.get_object_id("_id").unwrap()
}

async fn code(app: &TestApp, email: &str) -> Option<Document> {
    let id = user_id(app, email).await;
    app.db
        .collection::<Document>("activation_codes")
        .find_one(doc! { "user_id": id })
        .await
        .unwrap()
}

async fn token(app: &TestApp, email: &str) -> Option<String> {
    code(app, email)
        .await
        .map(|c| c.get_str("token").unwrap().to_string())
}

/// The work runs after the answer, so wait for what it leaves behind.
async fn new_token(app: &TestApp, email: &str, old: Option<&str>) -> String {
    for _ in 0..200 {
        if let Some(t) = token(app, email).await
            && Some(t.as_str()) != old
        {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no new activation code for {email}");
}

fn minutes_ago(minutes: i64) -> DateTime {
    DateTime::from_millis(DateTime::now().timestamp_millis() - minutes * 60_000)
}

/// The link as a person finds it: past its `valid_to`, in the window before the
/// TTL monitor reaps the row.
async fn let_the_link_expire(app: &TestApp, email: &str) {
    let id = user_id(app, email).await;
    app.db
        .collection::<Document>("activation_codes")
        .update_many(
            doc! { "user_id": id },
            doc! { "$set": { "valid_to": minutes_ago(1) } },
        )
        .await
        .unwrap();
}

async fn redeem(app: &TestApp, id: ObjectId, token: &str) -> u16 {
    app.client
        .post(app.url("/api/auth/activate"))
        .json(&json!({ "user_id": id.to_hex(), "token": token }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn sign_in(app: &TestApp, email: &str) -> u16 {
    app.client
        .post(app.url("/api/auth/login"))
        .json(&json!({ "email": email, "password": PASSWORD }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

type Answer = (u16, BTreeMap<String, String>, String);

/// Everything a caller can read off an answer, bar the clock.
async fn answer_of(resp: reqwest::Response) -> Answer {
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .filter(|(name, _)| name.as_str() != "date")
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_string()))
        .collect();
    (status, headers, resp.text().await.unwrap())
}

#[tokio::test]
async fn an_expired_link_leads_to_a_new_link_and_a_sign_in() {
    let app = TestApp::spawn().await;
    let email = "late@test.com";
    signed_up(&app, email, "late").await;
    let id = user_id(&app, email).await;
    let expired = token(&app, email).await.expect("sign-up minted a code");
    let_the_link_expire(&app, email).await;

    // The dead end this exists for: the link is refused, and the address cannot
    // be registered again.
    assert_eq!(redeem(&app, id, &expired).await, 400);
    let again = sign_up(&app, email, "late2").await;
    assert_eq!(again.status().as_u16(), 409);
    assert_eq!(sign_in(&app, email).await, 401);

    assert_eq!(ask(&app, &format!("  {email} ")).await, 202);
    let fresh = new_token(&app, email, Some(&expired)).await;

    // A resend proves nothing by itself: the row still claims what it claimed.
    let waiting = row(&app, email).await;
    assert_eq!(waiting.get_str("email").unwrap(), email);
    assert!(!waiting.get_bool("is_verified").unwrap());
    assert_eq!(sign_in(&app, email).await, 401);

    assert_eq!(
        redeem(&app, id, &expired).await,
        400,
        "the old link stays dead"
    );
    assert_eq!(redeem(&app, id, &fresh).await, 200);
    assert_eq!(sign_in(&app, email).await, 200);
}

/// An account stuck since before this route: no code left at all, the TTL
/// monitor took it long ago, and no stamp either.
#[tokio::test]
async fn an_account_stuck_for_months_recovers_the_same_way() {
    let app = TestApp::spawn().await;
    let email = "stuck@test.com";
    signed_up(&app, email, "stuck").await;
    let id = user_id(&app, email).await;
    app.db
        .collection::<Document>("activation_codes")
        .delete_many(doc! { "user_id": id })
        .await
        .unwrap();
    users(&app)
        .update_one(
            doc! { "_id": id },
            doc! { "$set": { "created_at": minutes_ago(90 * 24 * 60) } },
        )
        .await
        .unwrap();

    assert_eq!(ask(&app, email).await, 202);
    let fresh = new_token(&app, email, None).await;
    assert_eq!(redeem(&app, id, &fresh).await, 200);
    assert_eq!(sign_in(&app, email).await, 200);
}

#[tokio::test]
async fn every_address_gets_the_same_answer() {
    let app = TestApp::spawn_with_settings(behind_a_proxy).await;
    signed_up(&app, "waiting@test.com", "waiting").await;
    signed_up(&app, "active@test.com", "active").await;
    app.activate_user("active@test.com").await;
    signed_up(&app, "evicted@unverified.invalid", "evicted").await;

    // One already inside its cooldown, so the arm that sends nothing is real.
    signed_up(&app, "cooling@test.com", "cooling").await;
    let cooling = token(&app, "cooling@test.com").await;
    ask_from(&app, "203.0.113.1", "cooling@test.com").await;
    let cooling = new_token(&app, "cooling@test.com", cooling.as_deref()).await;

    let waiting = token(&app, "waiting@test.com").await;
    let active = token(&app, "active@test.com").await;

    let addresses = [
        "nobody@test.com",
        "waiting@test.com",
        "cooling@test.com",
        "active@test.com",
        "evicted@unverified.invalid",
        "not an address",
    ];
    let mut answers = Vec::new();
    for (i, email) in addresses.iter().enumerate() {
        let client = format!("203.0.113.{}", i + 10);
        answers.push(answer_of(ask_from(&app, &client, email).await).await);
    }

    let (status, _, body) = &answers[0];
    assert_eq!(*status, 202);
    let body: Value = serde_json::from_str(body).unwrap();
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .starts_with("If that address is waiting for activation"),
        "{body}"
    );
    for (email, answer) in addresses.iter().zip(&answers) {
        assert_eq!(answer, &answers[0], "{email} answered differently");
    }

    // ...and the identical answers hid different outcomes, so this is not
    // passing because every arm happened to do nothing.
    new_token(&app, "waiting@test.com", waiting.as_deref()).await;
    assert_eq!(token(&app, "active@test.com").await, active);
    assert_eq!(
        token(&app, "cooling@test.com").await.as_deref(),
        Some(cooling.as_str())
    );
}

async fn hold_a_connection(db: Database) {
    db.collection::<Document>("users")
        .find_one(doc! { "$where": format!("sleep({HOLD_MS}); return true;") })
        .await
        .expect("the held query");
}

/// Same timing, by construction: the answer does not touch the database, so it
/// cannot take longer for an address that has a row.
///
/// `cap_pool` gives a `TestApp` two Mongo connections, shared with its server.
/// With both held by a slow query, anything that awaits Mongo waits out the
/// hold, and a lookup moved back into the handler fails this for every address.
#[tokio::test]
async fn the_answer_does_not_wait_for_the_lookup() {
    let app = TestApp::spawn_with_settings(behind_a_proxy).await;
    signed_up(&app, "waiting@test.com", "waiting").await;
    signed_up(&app, "active@test.com", "active").await;
    app.activate_user("active@test.com").await;
    let before = token(&app, "waiting@test.com").await;

    let started = Instant::now();
    let holds: Vec<_> = (0..2)
        .map(|_| tokio::spawn(hold_a_connection(app.db.clone())))
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let addresses = ["waiting@test.com", "active@test.com", "nobody@test.com"];
    for (i, email) in addresses.iter().enumerate() {
        let asked = Instant::now();
        let client = format!("203.0.113.{}", i + 1);
        let status = ask_from(&app, &client, email).await.status().as_u16();
        let took = asked.elapsed();
        assert_eq!(status, 202);
        assert!(
            took < Duration::from_millis(ANSWER_CEILING_MS),
            "{email} waited {took:?} for its answer"
        );
    }
    let answered_after = started.elapsed();

    // The control: the pool really was held while those answers went out. A
    // read queued now can only finish once a hold lets go.
    users(&app).count_documents(doc! {}).await.unwrap();
    assert!(
        answered_after < Duration::from_millis(HOLD_MS)
            && started.elapsed() >= Duration::from_millis(HOLD_MS),
        "the pool was not held: answered after {answered_after:?}, read back after {:?}",
        started.elapsed()
    );
    for hold in holds {
        hold.await.unwrap();
    }

    // ...and the work still ran, once the database was back.
    new_token(&app, "waiting@test.com", before.as_deref()).await;
}

#[tokio::test]
async fn a_second_ask_inside_the_cooldown_sends_nothing_from_either_pod() {
    let (a, b) = TestApp::spawn_pair(|_| {}).await;
    let email = "late@test.com";
    signed_up(&a, email, "late").await;
    signed_up(&a, "control@test.com", "control").await;
    let first = token(&a, email).await;

    // Sign-up does not start the clock: the person back with an expired link at
    // minute 6 of a 5-minute TTL gets one at once.
    assert_eq!(ask(&a, email).await, 202);
    let second = new_token(&a, email, first.as_deref()).await;

    // The other pod reads the same stamp, and still answers 202.
    assert_eq!(ask(&b, email).await, 202);
    let control = token(&a, "control@test.com").await;
    assert_eq!(ask(&b, "control@test.com").await, 202);
    new_token(&a, "control@test.com", control.as_deref()).await;
    assert_eq!(token(&a, email).await.as_deref(), Some(second.as_str()));

    // The cooldown ends: nobody stays locked out.
    users(&a)
        .update_one(
            doc! { "email": email },
            doc! { "$set": { "activation_resent_at": minutes_ago(16) } },
        )
        .await
        .unwrap();
    assert_eq!(ask(&a, email).await, 202);
    new_token(&a, email, Some(&second)).await;
}

/// The cooldown is the only thing between a public form and a mail bomb, and
/// two pods can read the same row at the same moment.
#[tokio::test]
async fn concurrent_asks_claim_one_mail() {
    let app = TestApp::spawn().await;
    signed_up(&app, "late@test.com", "late").await;

    let mut claims = JoinSet::new();
    for _ in 0..8 {
        let dao = app.state.users.clone();
        claims.spawn(async move {
            dao.claim_activation_resend("late@test.com")
                .await
                .unwrap()
                .is_some()
        });
    }
    let mut won = 0;
    while let Some(claim) = claims.join_next().await {
        won += usize::from(claim.unwrap());
    }
    assert_eq!(won, 1);
}

#[tokio::test]
async fn one_client_walking_a_list_of_addresses_runs_out() {
    let app = TestApp::spawn_with_settings(behind_a_proxy).await;
    signed_up(&app, "waiting@test.com", "waiting").await;
    let client = "203.0.113.9";

    for i in 0..RESEND_BURST {
        let resp = ask_from(&app, client, &format!("stranger{i}@test.com")).await;
        assert_eq!(resp.status().as_u16(), 202, "ask {i}");
    }

    // Spent on addresses nobody holds, and refused for one that exists: the
    // bucket knows the client, not the address.
    let refused = ask_from(&app, client, "waiting@test.com").await;
    assert_eq!(refused.status().as_u16(), 429);
    assert!(refused.headers().contains_key("retry-after"));
    let (_, _, body) = answer_of(refused).await;
    assert!(!body.contains("waiting@test.com"), "{body}");
    assert!(!body.contains("account"), "{body}");

    // Another client is not affected.
    let other = ask_from(&app, "198.51.100.7", "waiting@test.com").await;
    assert_eq!(other.status().as_u16(), 202);
}

async fn can_be_mailed(app: &TestApp, email: &str) -> bool {
    app.state
        .users
        .claim_activation_resend(email)
        .await
        .unwrap()
        .is_some()
}

/// `claim_activation_resend` decides who is mailed, so it is asked directly:
/// `None` is "no mail", which the answer is built never to show.
#[tokio::test]
async fn only_a_password_sign_up_still_waiting_can_be_mailed() {
    let app = TestApp::spawn().await;

    signed_up(&app, "waiting@test.com", "waiting").await;
    assert!(
        can_be_mailed(&app, "waiting@test.com").await,
        "the positive control"
    );

    // The field defaults to false on read, so this row cannot sign in either.
    signed_up(&app, "legacy@test.com", "legacy").await;
    users(&app)
        .update_one(
            doc! { "email": "legacy@test.com" },
            doc! { "$unset": { "is_verified": "" } },
        )
        .await
        .unwrap();
    assert!(
        can_be_mailed(&app, "legacy@test.com").await,
        "a row older than the field"
    );

    assert!(
        !can_be_mailed(&app, "nobody@test.com").await,
        "an unknown address"
    );

    signed_up(&app, "active@test.com", "active").await;
    app.activate_user("active@test.com").await;
    assert!(
        !can_be_mailed(&app, "active@test.com").await,
        "an active account"
    );

    signed_up(&app, "gone@test.com", "gone").await;
    users(&app)
        .update_one(
            doc! { "email": "gone@test.com" },
            doc! { "$set": { "deleted_at": DateTime::now() } },
        )
        .await
        .unwrap();
    assert!(!can_be_mailed(&app, "gone@test.com").await, "a tombstone");

    signed_up(&app, "oauth@test.com", "oauth").await;
    users(&app)
        .update_one(
            doc! { "email": "oauth@test.com" },
            doc! { "$unset": { "password_hash": "" } },
        )
        .await
        .unwrap();
    assert!(
        !can_be_mailed(&app, "oauth@test.com").await,
        "nothing to activate"
    );

    // An evicted row keeps its password, so only the address gives it away.
    signed_up(&app, "unclaimed-1@unverified.invalid", "evicted").await;
    assert!(
        !can_be_mailed(&app, "unclaimed-1@unverified.invalid").await,
        "a placeholder"
    );

    // The index is case-sensitive, so this is another address.
    signed_up(&app, "cased@test.com", "cased").await;
    assert!(!can_be_mailed(&app, "CASED@test.com").await, "another case");
}

/// The fix must not lean on the value, which is a separate decision.
#[tokio::test]
async fn the_first_ask_is_served_at_any_ttl() {
    for ttl in [1u64, 24 * 60] {
        let app = TestApp::spawn_with_settings(|s| {
            s.email.activation_token_ttl_minutes = ttl;
        })
        .await;
        let email = "late@test.com";
        signed_up(&app, email, "late").await;
        let first = token(&app, email).await;

        assert_eq!(ask(&app, email).await, 202);
        new_token(&app, email, first.as_deref()).await;

        let valid_to = code(&app, email)
            .await
            .unwrap()
            .get_datetime("valid_to")
            .unwrap()
            .timestamp_millis();
        let lives_for = valid_to - DateTime::now().timestamp_millis();
        let ttl_ms = ttl as i64 * 60_000;
        assert!(
            lives_for > ttl_ms - 30_000 && lives_for <= ttl_ms,
            "ttl {ttl}: the new link lives {lives_for} ms"
        );
    }
}

#[tokio::test]
async fn a_taken_address_or_username_is_said_in_words() {
    let app = TestApp::spawn().await;
    signed_up(&app, "late@test.com", "late").await;

    let address = sign_up(&app, "late@test.com", "other").await;
    assert_eq!(address.status().as_u16(), 409);
    let body = address.text().await.unwrap();
    assert!(body.contains("ask for a new activation link"), "{body}");
    assert!(!body.contains("E11000"), "{body}");
    assert!(!body.contains("late@test.com"), "{body}");

    let username = sign_up(&app, "other@test.com", "late").await;
    assert_eq!(username.status().as_u16(), 409);
    let body = username.text().await.unwrap();
    assert!(body.contains("username is taken"), "{body}");
    assert!(!body.contains("E11000"), "{body}");
}

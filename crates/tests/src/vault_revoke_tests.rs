// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-3c — revocation: the vault takes a lease back, and tells the
//! device on its own connection (`rc:vault.revoke`).
//!
//! Driven through P3-3b's raw agent socket, which holds its own vault key and
//! leases through the real start route. What these lock is the SERVER:
//!
//! - a lease is taken back when its session ends or moves, its field gets a
//!   newer version, its secret is disabled or deleted, its device's key
//!   changes, or its device is removed — the row first, then the frame;
//! - a change to a secret is told at once on the device's pod, and the rest
//!   at that pod's next sweep;
//! - a hello is told again of what its device missed while away;
//! - an expired lease is never told, and a connection that cannot read the
//!   frame is never sent it.

use std::time::Duration;

use bson::{doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::tunnel_tests::wait_agent_online;
use crate::vault_lease_tests::{
    Device, RUNS_HIVE, TAKES_LEASES, ask, device, fresh_kek, rows, seed_vault, start,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};

/// Hive and the vault on, with a KEK, each pod sweeping every `sweep` s.
async fn revoke_app(sweep: u64) -> TestApp {
    let kek = fresh_kek();
    TestApp::spawn_with_settings(move |s| {
        s.modules.hive = true;
        s.modules.vault = true;
        s.vault.kek = kek;
        s.vault.revoke_sweep_secs = sweep;
        s.app.rate_limit_per_sec = 100;
        s.app.rate_limit_burst = 1000;
    })
    .await
}

/// A sweep this long never runs in a test: a frame seen came from the
/// change itself.
const NEVER: u64 = 3_600;

/// The next `rc:vault.revoke` within `within`, skipping other frames.
async fn revoked(dev: &mut Device, within: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let next = match tokio::time::timeout(left, dev.ws.next()).await {
            Err(_) => return None,
            Ok(None | Some(Err(_))) => return None,
            Ok(Some(Ok(m))) => m,
        };
        if let Message::Text(text) = next {
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if v["t"] == "rc:vault.revoke" {
                return Some(v);
            }
        }
    }
}

/// A lease of `db/main` for `session` at `fence`; its id.
async fn lease_main(dev: &mut Device, req_id: &str, session: &str, fence: u64) -> String {
    let answer = ask(
        dev,
        req_id,
        json!({ "session_id": session, "fence": fence, "secret": "db/main" }),
    )
    .await;
    assert_eq!(answer["t"], "rc:vault.leased", "{answer}");
    answer["lease_id"].as_str().unwrap().to_string()
}

async fn lease_row(app: &TestApp, lease: &str) -> bson::Document {
    rows(
        app,
        "vault_leases",
        doc! { "_id": ObjectId::parse_str(lease).unwrap() },
    )
    .await
    .pop()
    .expect("the lease's row")
}

fn assert_revoked(frame: &Value, lease: &str, why: &str) {
    assert_eq!(frame["reason"], why, "{frame}");
    let ids: Vec<&str> = frame["lease_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(ids.contains(&lease), "{frame}");
}

/// The session's record, as hive's own paths would leave it.
async fn set_session(app: &TestApp, session: &str, update: bson::Document) {
    let r = app
        .db
        .collection::<bson::Document>("agent_sessions")
        .update_one(
            doc! { "_id": ObjectId::parse_str(session).unwrap() },
            update,
        )
        .await
        .unwrap();
    assert_eq!(r.matched_count, 1);
}

async fn secret_id(app: &TestApp, t: &SeededTenant, path: &str) -> String {
    rows(
        app,
        "vault_secrets",
        doc! { "tenant_id": ObjectId::parse_str(&t.tenant_id).unwrap(), "path": path, "deleted_at": null },
    )
    .await
    .pop()
    .expect("the secret")
    .get_object_id("_id")
    .unwrap()
    .to_hex()
}

/// Dial again as the same enrolled device: its next hello, with `rpc`, and
/// a vault key — a new one when `new_key` (a restarted daemon), the old one
/// otherwise (a reconnect of the same process) — when `with_key`.
async fn reconnect(
    app: &TestApp,
    t: &SeededTenant,
    dev: &mut Device,
    rpc: &[&str],
    with_key: bool,
    new_key: bool,
) {
    let _ = dev.ws.close(None).await;
    if new_key {
        dev.key = crypto_box::SecretKey::generate(&mut crypto_box::aead::OsRng);
    }
    let url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        dev.token
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    );
    let (mut ws, _) = connect_async(&url).await.expect("agent ws connect");
    let mut hello = json!({
        "t": "rc:agent.hello",
        "machine_name": "rv",
        "os": "linux",
        "agent_version": "0.4.143",
        "displays": [],
        "caps": {
            "hw_encoders": [], "codecs": ["h264"], "has_input_permission": false,
            "supports_clipboard": false, "supports_file_transfer": false,
            "max_simultaneous_sessions": 1, "rpc": rpc,
        }
    });
    if with_key {
        hello["vault_key"] = json!(STANDARD.encode(dev.key.public_key().as_bytes()));
    }
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    dev.ws = ws;
    wait_agent_online(app, t, &dev.agent_id).await;
}

/// FR-90 P3-3c — a session's end and its move each take its leases back, at
/// the device's pod's next sweep: the row first, then the frame, and the
/// audit says why. A lease at the session's new fence stands until the end.
#[tokio::test]
async fn a_lease_is_taken_back_when_its_session_moves_or_ends() {
    let app = revoke_app(1).await;
    let t = app.seed_tenant("revoke-session").await;
    seed_vault(&app, &t).await;
    let mut dev = device(&app, &t, "rv-session", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/a", Some("dev")).await;

    let first = lease_main(&mut dev, "m1", &sid, fence).await;
    // A move: the record's fence goes on, as a promote's or teleport's does.
    set_session(&app, &sid, doc! { "$inc": { "fence": 1_i64 } }).await;
    let frame = revoked(&mut dev, Duration::from_secs(10))
        .await
        .expect("the moved session's lease is taken back");
    assert_revoked(&frame, &first, "session_moved");
    let row = lease_row(&app, &first).await;
    assert_eq!(row.get_str("revoked_why").unwrap(), "session_moved");
    assert!(row.get_datetime("revoked_at").is_ok());

    // At the new fence, a lease stands — until the session ends.
    let second = lease_main(&mut dev, "m2", &sid, fence + 1).await;
    assert!(
        revoked(&mut dev, Duration::from_secs(3)).await.is_none(),
        "a lease at the record's fence stands"
    );
    set_session(&app, &sid, doc! { "$set": { "status": "ended" } }).await;
    let frame = revoked(&mut dev, Duration::from_secs(10))
        .await
        .expect("the ended session's lease is taken back");
    assert_revoked(&frame, &second, "session_ended");

    let audit = rows(
        &app,
        "vault_audit",
        doc! { "op": "revoke", "target": format!("lease:{second}") },
    )
    .await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0].get_str("decision").unwrap(), "revoke");
    assert_eq!(audit[0].get_str("reason").unwrap(), "session_ended");
    assert_eq!(audit[0].get_str("principal").unwrap(), "vault");
}

/// A change to a secret takes back what it released at once — no sweep
/// runs here — and only what it released: a newer version takes back the
/// field's older leases, a disable or a delete every lease of the secret.
#[tokio::test]
async fn a_change_to_a_secret_takes_back_what_it_released_at_once() {
    let app = revoke_app(NEVER).await;
    let t = app.seed_tenant("revoke-secret").await;
    seed_vault(&app, &t).await;
    let other = crate::vault_lease_tests::secret(
        &app,
        &t,
        "db/other",
        &format!("other-{}", ObjectId::new().to_hex()),
    )
    .await;
    let mut dev = device(&app, &t, "rv-secret", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/b", Some("dev")).await;
    let main = secret_id(&app, &t, "db/main").await;

    let old = lease_main(&mut dev, "v1", &sid, fence).await;
    let kept = ask(
        &mut dev,
        "o1",
        json!({ "session_id": sid, "fence": fence, "secret": "db/other" }),
    )
    .await;
    let kept = kept["lease_id"].as_str().unwrap().to_string();
    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/vault/secret/{main}/value", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({ "value": format!("rotated-{}", ObjectId::new().to_hex()) }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let frame = revoked(&mut dev, Duration::from_secs(5))
        .await
        .expect("a newer version takes the older lease back at once");
    assert_revoked(&frame, &old, "value_changed");
    assert!(
        lease_row(&app, &kept)
            .await
            .get_datetime("revoked_at")
            .is_err(),
        "another secret's lease stands"
    );

    let new = lease_main(&mut dev, "v2", &sid, fence).await;
    let resp = app
        .client
        .patch(app.url(&format!("/api/tenant/{}/vault/secret/{main}", t.tenant_id)))
        .header("Authorization", format!("Bearer {}", t.admin.access_token))
        .json(&json!({ "disabled": true }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let frame = revoked(&mut dev, Duration::from_secs(5))
        .await
        .expect("a disable takes the lease back at once");
    assert_revoked(&frame, &new, "secret_disabled");

    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/vault/secret/{other}", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let frame = revoked(&mut dev, Duration::from_secs(5))
        .await
        .expect("a delete takes the lease back at once");
    assert_revoked(&frame, &kept, "secret_deleted");
}

/// A restarted daemon has a new key and none of its old process's memory:
/// its hello takes back every lease sealed to the old key, and tells it.
#[tokio::test]
async fn a_restarted_daemons_leases_are_taken_back_at_its_hello() {
    let app = revoke_app(NEVER).await;
    let t = app.seed_tenant("revoke-key").await;
    seed_vault(&app, &t).await;
    let mut dev = device(&app, &t, "rv-key", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/c", Some("dev")).await;
    let lease = lease_main(&mut dev, "k1", &sid, fence).await;

    reconnect(&app, &t, &mut dev, TAKES_LEASES, true, true).await;
    let frame = revoked(&mut dev, Duration::from_secs(5))
        .await
        .expect("the hello is told");
    assert_revoked(&frame, &lease, "key_changed");
    assert_eq!(
        lease_row(&app, &lease)
            .await
            .get_str("revoked_why")
            .unwrap(),
        "key_changed"
    );
}

/// A revocation made while the device was away is told at its next hello —
/// the same process back, its key unchanged.
#[tokio::test]
async fn a_revocation_missed_while_away_is_told_at_the_next_hello() {
    let app = revoke_app(NEVER).await;
    let t = app.seed_tenant("revoke-away").await;
    seed_vault(&app, &t).await;
    let mut dev = device(&app, &t, "rv-away", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/d", Some("dev")).await;
    let lease = lease_main(&mut dev, "a1", &sid, fence).await;
    let main = secret_id(&app, &t, "db/main").await;

    let _ = dev.ws.close(None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let resp = app
        .client
        .patch(app.url(&format!("/api/tenant/{}/vault/secret/{main}", t.tenant_id)))
        .header("Authorization", format!("Bearer {}", t.admin.access_token))
        .json(&json!({ "disabled": true }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    assert_eq!(
        lease_row(&app, &lease)
            .await
            .get_str("revoked_why")
            .unwrap(),
        "secret_disabled",
        "taken back while away"
    );

    reconnect(&app, &t, &mut dev, TAKES_LEASES, true, false).await;
    let frame = revoked(&mut dev, Duration::from_secs(5))
        .await
        .expect("the next hello is told");
    assert_revoked(&frame, &lease, "secret_disabled");
}

/// An expired lease is its own revocation: never taken back, never told.
#[tokio::test]
async fn an_expired_lease_is_never_told() {
    let app = revoke_app(1).await;
    let t = app.seed_tenant("revoke-expired").await;
    seed_vault(&app, &t).await;
    let mut dev = device(&app, &t, "rv-expired", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/e", Some("dev")).await;
    let lease = lease_main(&mut dev, "e1", &sid, fence).await;

    let past = bson::DateTime::from_millis(bson::DateTime::now().timestamp_millis() - 1_000);
    app.db
        .collection::<bson::Document>("vault_leases")
        .update_one(
            doc! { "_id": ObjectId::parse_str(&lease).unwrap() },
            doc! { "$set": { "expires_at": past } },
        )
        .await
        .unwrap();
    set_session(&app, &sid, doc! { "$set": { "status": "ended" } }).await;
    assert!(
        revoked(&mut dev, Duration::from_secs(4)).await.is_none(),
        "an expired lease is not told"
    );
    assert!(
        lease_row(&app, &lease)
            .await
            .get_datetime("revoked_at")
            .is_err()
    );
}

/// A device being removed is told before its row and socket go: the vault's
/// place in fleet's cascade is after hive's and before fleet's own.
#[tokio::test]
async fn a_removed_device_is_told_before_it_goes() {
    let app = revoke_app(NEVER).await;
    let t = app.seed_tenant("revoke-removed").await;
    seed_vault(&app, &t).await;
    let mut dev = device(&app, &t, "rv-removed", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/f", Some("dev")).await;
    let lease = lease_main(&mut dev, "r1", &sid, fence).await;

    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/agent/{}", t.tenant_id, dev.agent_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let frame = revoked(&mut dev, Duration::from_secs(5))
        .await
        .expect("told before the socket went");
    assert_revoked(&frame, &lease, "device_removed");
    assert_eq!(
        lease_row(&app, &lease)
            .await
            .get_str("revoked_why")
            .unwrap(),
        "device_removed"
    );
}

/// A build that cannot read `rc:vault.revoke` is never sent it — its lease is
/// taken back all the same.
#[tokio::test]
async fn a_connection_that_takes_no_leases_is_never_told() {
    let app = revoke_app(1).await;
    let t = app.seed_tenant("revoke-older").await;
    seed_vault(&app, &t).await;
    let mut dev = device(&app, &t, "rv-older", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/g", Some("dev")).await;
    let lease = lease_main(&mut dev, "g1", &sid, fence).await;

    // An older build back on the device: no `vault` verb, no key.
    reconnect(&app, &t, &mut dev, RUNS_HIVE, false, true).await;
    assert!(
        revoked(&mut dev, Duration::from_secs(4)).await.is_none(),
        "never sent to a build that cannot read it"
    );
    assert_eq!(
        lease_row(&app, &lease)
            .await
            .get_str("revoked_why")
            .unwrap(),
        "key_changed"
    );
}

/// A version past its `expires_at` is never released, and no lease outlives
/// its version.
#[tokio::test]
async fn a_lease_never_outlives_its_version() {
    let app = revoke_app(NEVER).await;
    let t = app.seed_tenant("revoke-version").await;
    seed_vault(&app, &t).await;
    let mut dev = device(&app, &t, "rv-version", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/h", Some("dev")).await;
    let main = secret_id(&app, &t, "db/main").await;

    let until = bson::DateTime::from_millis(bson::DateTime::now().timestamp_millis() + 120_000);
    let resp = app
        .auth_put(
            &format!("/api/tenant/{}/vault/secret/{main}/value", t.tenant_id),
            &t.admin.access_token,
        )
        .json(&json!({
            "value": format!("short-{}", ObjectId::new().to_hex()),
            "expires_at": until.try_to_rfc3339_string().unwrap(),
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let answer = ask(
        &mut dev,
        "x1",
        json!({ "session_id": sid, "fence": fence, "secret": "db/main", "ttl_secs": 900 }),
    )
    .await;
    assert_eq!(answer["t"], "rc:vault.leased", "{answer}");
    assert!(
        answer["expires_at_ms"].as_i64().unwrap() <= until.timestamp_millis(),
        "capped at the version's expiry: {answer}"
    );

    // That version, past its expiry: never released.
    let past = bson::DateTime::from_millis(bson::DateTime::now().timestamp_millis() - 1_000);
    app.db
        .collection::<bson::Document>("vault_versions")
        .update_many(
            doc! { "secret_id": ObjectId::parse_str(&main).unwrap() },
            doc! { "$set": { "expires_at": past } },
        )
        .await
        .unwrap();
    let answer = ask(
        &mut dev,
        "x2",
        json!({ "session_id": sid, "fence": fence, "secret": "db/main" }),
    )
    .await;
    assert_eq!(answer["t"], "rc:vault.refused", "{answer}");
    assert_eq!(answer["reason"], "no_value", "{answer}");
    let audit = rows(&app, "vault_audit", doc! { "reason": "version_expired" }).await;
    assert_eq!(audit.len(), 1, "{audit:?}");
}

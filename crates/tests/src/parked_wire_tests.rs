// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-137 P2 — FR-90 (Hive) is parked, and the wire keeps two of its fields
//! parse-only so that a 0.4.199 agent's request is refused rather than read as
//! its owner's (S-B3, decision D8). Compatibility tests 2 and 6 (spec §3.4):
//!
//! * `a_ticketed_ssh_request_is_refused_and_audited` — an `rc:ssh.request`
//!   carrying `ssh_ticket` is answered with an error, never a grant, and its
//!   `ssh_audit` row says `ticket_refused`, source `session`.
//! * `a_tunnel_open_with_a_net_grant_is_refused` — an `rc:tunnel.open`
//!   carrying `net_grant` is refused `net_grant_refused`, never opened as the
//!   device's own tunnel.
//! * `ssh_audit_rows_written_before_p2_list` — rows with FR-90's three
//!   refusals, its `session` source and the two fields the model no longer has
//!   list through the route, which reads every row through `BaseDao`: one row
//!   that failed to decode would fail the whole page.

use crate::fixtures::test_app::TestApp;
use crate::ssh_grant_ack_tests::SESSION_KEY;
use crate::tunnel_tests::{connect_agent_ws, enroll_agent, read_until, wait_agent_online};
use bson::oid::ObjectId;
use futures::SinkExt;
use roomler_ai_remote_control::signaling::NET_GRANT_REFUSED;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

async fn audit_items(app: &TestApp, seeded: &crate::fixtures::seed::SeededTenant) -> Vec<Value> {
    let audit: Value = app
        .auth_get(
            &format!("/api/tenant/{}/ssh-audit", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    audit["items"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn a_ticketed_ssh_request_is_refused_and_audited() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr137-ssh-ticket").await;
    let (a_id, a_tok) = enroll_agent(&app, &t, "mach-fr137-ssh-a", "fr137-a").await;
    let (b_id, _b_tok) = enroll_agent(&app, &t, "mach-fr137-ssh-b", "fr137-b").await;
    let mut a_ws = connect_agent_ws(&app, &a_tok, "fr137-a").await;
    wait_agent_online(&app, &t, &a_id).await;

    a_ws.send(Message::Text(
        json!({
            "t": "rc:ssh.request",
            "request_id": "fr137-1",
            "target": b_id,
            "public_key": SESSION_KEY,
            "session_secs": 60,
            // FR-90's agent-session ticket, as a 0.4.199 daemon sends it.
            "ssh_ticket": "a-ticket",
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let resp = read_until(&mut a_ws, "rc:ssh.response")
        .await
        .expect("the origin device is answered");
    assert_eq!(resp["request_id"], json!("fr137-1"), "{resp}");
    assert!(
        resp["error"].is_string(),
        "a refusal, never a grant: {resp}"
    );
    assert!(
        resp["address"].is_null() && resp["grant_id"].is_null(),
        "a refusal carries no grant: {resp}"
    );

    let rows = audit_items(&app, &t).await;
    assert!(
        rows.iter()
            .any(|r| r["denied"] == json!("ticket_refused") && r["source"] == json!("session")),
        "the refusal is audited as the session's: {rows:?}"
    );
}

#[tokio::test]
async fn a_tunnel_open_with_a_net_grant_is_refused() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr137-net-grant").await;
    let (a_id, a_tok) = enroll_agent(&app, &t, "mach-fr137-tun-a", "fr137-ta").await;
    let (b_id, _b_tok) = enroll_agent(&app, &t, "mach-fr137-tun-b", "fr137-tb").await;
    let mut a_ws = connect_agent_ws(&app, &a_tok, "fr137-ta").await;
    wait_agent_online(&app, &t, &a_id).await;

    a_ws.send(Message::Text(
        json!({
            "t": "rc:tunnel.open",
            "agent_id": b_id,
            "transport": "webrtc-dc-v1",
            // FR-90's agent-session grant, as a 0.4.199 daemon sends it.
            "net_grant": "a-grant",
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let err = read_until(&mut a_ws, "rc:error")
        .await
        .expect("the open is refused");
    assert_eq!(err["code"].as_str(), Some(NET_GRANT_REFUSED), "{err}");
}

#[tokio::test]
async fn ssh_audit_rows_written_before_p2_list() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("fr137-ssh-audit").await;
    let tid = ObjectId::parse_str(&t.tenant_id).unwrap();
    let id = ObjectId::new();
    let rows: Vec<bson::Document> = ["ticket_refused", "session_not_allowed", "target_too_old"]
        .into_iter()
        .map(|word| {
            bson::doc! {
                "tenant_id": tid,
                "agent_id": id,
                "user_id": id,
                "grant_id": "g-1",
                "source": "session",
                "session_id": ObjectId::new(),
                "ssh_ticket_id": "t-1",
                "caller": "an agent session",
                "at": bson::DateTime::now(),
                "denied": word,
            }
        })
        .collect();
    app.db
        .collection::<bson::Document>("ssh_audit")
        .insert_many(rows)
        .await
        .unwrap();

    let items = audit_items(&app, &t).await;
    for word in ["ticket_refused", "session_not_allowed", "target_too_old"] {
        assert!(
            items
                .iter()
                .any(|r| r["denied"] == json!(word) && r["source"] == json!("session")),
            "{word} lists: {items:?}"
        );
    }
}

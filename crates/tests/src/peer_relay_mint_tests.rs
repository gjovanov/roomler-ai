// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-19 P3c — the org-relay MINT, driven end to end over real agent
//! WebSockets against a live `TestApp`: three enrolled agents join the
//! overlay, one is approved as the relay, and a `rc:overlay.relay_request`
//! from one member must produce `relay_serve` at the relay and
//! `relay_session` at both members — or an audited refusal naming its gate.
//!
//! What these lock is the WIRING of §1–§7: that the mint sits on the one path
//! a relay request takes, that every refusal reaches `peer_relay_audit` with
//! the reason the spec enumerates, that a live session flips the server
//! verdict to `org-relay`, and that all four revocation triggers push
//! `relay_revoke` to every party. The data plane — the members actually
//! binding at a relay — is `tunnel-core`'s loopback test; this crate does not
//! compile the overlay (spec §10b).

use crate::agent_presence_tests::enroll;
use crate::fixtures::{seed::SeededTenant, test_app::TestApp};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::{doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

fn url(seeded: &SeededTenant, tail: &str) -> String {
    format!("/api/tenant/{}{}", seeded.tenant_id, tail)
}

/// Connect an agent WS and complete the hello with the given RPC verbs — the
/// relay's `relay-server` capability rides here, exactly as a real daemon
/// advertises it.
async fn connect(app: &TestApp, token: &str, rpc: &[&str]) -> Ws {
    let ws_url = format!("ws://{}/ws?token={}&role=agent", app.addr, urlencode(token));
    let (mut ws, _) = connect_async(&ws_url).await.expect("ws connect");
    let hello = json!({
        "t": "rc:agent.hello",
        "machine_name": "relay mint box",
        "os": "linux",
        "agent_version": "0.4.20",
        "displays": [],
        "caps": {
            "hw_encoders": ["openh264"],
            "codecs": ["h264"],
            "has_input_permission": true,
            "supports_clipboard": true,
            "supports_file_transfer": true,
            "max_simultaneous_sessions": 1,
            "rpc": rpc,
        }
    });
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .expect("send hello");
    ws
}

/// Read frames until one with `t == want` arrives, or give up.
async fn recv_t(ws: &mut Ws, want: &str, wait: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&txt)
                    && v["t"] == want
                {
                    return Some(v);
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => return None,
            Err(_) => return None,
        }
    }
}

/// Join the overlay as a node that understands org relays. `seed` makes the
/// WG key distinct per node; `endpoint` is the public address the relay
/// candidate is reachable at.
async fn join(ws: &mut Ws, seed: u8, org_primary: Option<bool>, endpoint: &str) -> Value {
    let key = BASE64.encode([seed; 32]);
    let mut msg = json!({
        "t": "rc:overlay.join",
        "wg_public_key": key,
        "mtu": 1280,
        "endpoints": [endpoint],
        "supports_org_relay": true,
        "supports_server_relay_strategy": true,
        "relay_port": 3478,
    });
    if let Some(p) = org_primary {
        msg["org_primary"] = json!(p);
    }
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .expect("send join");
    recv_t(ws, "rc:overlay.netmap", Duration::from_secs(5))
        .await
        .expect("a netmap answers the join")
}

async fn put(app: &TestApp, path: &str, token: &str, body: Value) -> (u16, Value) {
    let resp = app.auth_put(path, token).json(&body).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn get(app: &TestApp, path: &str, token: &str) -> Value {
    app.auth_get(path, token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap_or(Value::Null)
}

async fn set_mode(app: &TestApp, seeded: &SeededTenant, mode: &str) {
    let (s, _) = put(
        app,
        &url(seeded, "/peer-relay"),
        &seeded.admin.access_token,
        json!({ "mode": mode }),
    )
    .await;
    assert_eq!(s, 200, "set mode {mode}");
}

async fn approve(app: &TestApp, seeded: &SeededTenant, agent_id: &str, serve: bool) -> u16 {
    let (s, _) = put(
        app,
        &url(seeded, &format!("/agent/{agent_id}/peer-relay-policy")),
        &seeded.admin.access_token,
        json!({ "serve": serve }),
    )
    .await;
    s
}

/// The gate-2 grant: every node may reach the relay node. `via` names the
/// relay; the destination CIDR covers its overlay address.
async fn grant_relay(app: &TestApp, seeded: &SeededTenant, relay_node_id: &str) -> String {
    let resp = app
        .auth_post(&url(seeded, "/overlay-acl"), &seeded.admin.access_token)
        .json(&json!({
            "name": "relay grant",
            "sources": [{ "kind": "all_nodes" }],
            "via": [{ "kind": "node_id", "id": relay_node_id }],
            "destinations": [{ "cidr": "100.64.0.0/10" }],
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "policy create: {}",
        resp.status()
    );
    let v: Value = resp.json().await.unwrap();
    v["id"].as_str().unwrap().to_string()
}

async fn node_id(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> String {
    let tid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    app.state
        .network()
        .overlay_nodes
        .find_live_by_tenant_and_machine(tid, machine_id)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no live node for {machine_id}"))
        .id
        .unwrap()
        .to_hex()
}

async fn audit_rows(app: &TestApp, seeded: &SeededTenant) -> Vec<Value> {
    get(
        app,
        &url(seeded, "/peer-relay-audit"),
        &seeded.admin.access_token,
    )
    .await["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// Wait for the newest audit row to be a `mint` row, then return it.
async fn wait_mint_row(app: &TestApp, seeded: &SeededTenant) -> Value {
    for _ in 0..40 {
        let rows = audit_rows(app, seeded).await;
        if let Some(r) = rows.first()
            && r["action"] == json!("mint")
        {
            return r.clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "no mint row appeared; rows: {:?}",
        audit_rows(app, seeded).await
    );
}

/// Three nodes on one tenant: `a` asks, `b` is the peer, `r` may relay.
struct Rig {
    app: TestApp,
    seeded: SeededTenant,
    a: Ws,
    b: Ws,
    r: Ws,
    a_node: String,
    b_node: String,
    r_node: String,
    r_agent: String,
}

impl Rig {
    /// `relay_rpc` = whether the relay's hello advertises `relay-server`;
    /// `a_primary` = what `a`'s join says about its org.
    async fn up(slug: &str, relay_rpc: bool, a_primary: Option<bool>) -> Self {
        let app = TestApp::spawn().await;
        let seeded = app.seed_tenant(slug).await;
        let (a_agent, a_tok) = enroll(&app, &seeded, &format!("{slug}-a")).await;
        let (b_agent, b_tok) = enroll(&app, &seeded, &format!("{slug}-b")).await;
        let (r_agent, r_tok) = enroll(&app, &seeded, &format!("{slug}-r")).await;
        let _ = (a_agent, b_agent);
        let mut a = connect(&app, &a_tok, &[]).await;
        let mut b = connect(&app, &b_tok, &[]).await;
        let rpc: &[&str] = if relay_rpc { &["relay-server"] } else { &[] };
        let mut r = connect(&app, &r_tok, rpc).await;
        // Let the hellos land (the hub registers, the row stores caps).
        tokio::time::sleep(Duration::from_millis(300)).await;
        join(&mut r, 3, Some(true), "8.8.8.8:41641").await;
        join(&mut a, 1, a_primary, "203.0.113.10:41641").await;
        join(&mut b, 2, Some(true), "203.0.113.11:41641").await;
        let a_node = node_id(&app, &seeded, &format!("{slug}-a")).await;
        let b_node = node_id(&app, &seeded, &format!("{slug}-b")).await;
        let r_node = node_id(&app, &seeded, &format!("{slug}-r")).await;
        Self {
            app,
            seeded,
            a,
            b,
            r,
            a_node,
            b_node,
            r_node,
            r_agent,
        }
    }

    /// Everything a mint needs: org on, relay approved, gate-2 grant.
    async fn arm(&mut self) -> String {
        set_mode(&self.app, &self.seeded, "on").await;
        assert_eq!(
            approve(&self.app, &self.seeded, &self.r_agent, true).await,
            200
        );
        grant_relay(&self.app, &self.seeded, &self.r_node).await
    }

    async fn request(&mut self) {
        self.a
            .send(Message::Text(
                json!({ "t": "rc:overlay.relay_request", "peer_node_id": self.b_node })
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send relay_request");
    }

    /// The three frames of a mint, in the order the server sends them.
    async fn expect_minted(&mut self) -> (Value, Value, Value) {
        let wait = Duration::from_secs(5);
        let serve = recv_t(&mut self.r, "rc:overlay.relay_serve", wait)
            .await
            .expect("the relay receives relay_serve");
        let sa = recv_t(&mut self.a, "rc:overlay.relay_session", wait)
            .await
            .expect("member a receives relay_session");
        let sb = recv_t(&mut self.b, "rc:overlay.relay_session", wait)
            .await
            .expect("member b receives relay_session");
        (serve, sa, sb)
    }

    async fn expect_revoked(&mut self, vni: u64) {
        let wait = Duration::from_secs(5);
        for (name, ws) in [("a", &mut self.a), ("b", &mut self.b), ("r", &mut self.r)] {
            let v = recv_t(ws, "rc:overlay.relay_revoke", wait)
                .await
                .unwrap_or_else(|| panic!("{name} receives relay_revoke"));
            assert_eq!(v["vni"], json!(vni), "{name}: the revoke names the session");
        }
    }

    async fn assert_nothing_pushed_to_a(&mut self) {
        assert!(
            recv_t(
                &mut self.a,
                "rc:overlay.relay_session",
                Duration::from_millis(600)
            )
            .await
            .is_none(),
            "no session must reach a"
        );
    }
}

/// The happy path, and what a live session changes: three frames with one
/// VNI, a mint row naming every party, an idempotent re-request that re-pushes
/// the same session without a second row, and the server verdict flipping to
/// `org-relay` on the pair's next netmap.
#[tokio::test]
async fn mint_pushes_serve_and_sessions_flips_the_verdict_and_is_idempotent() {
    let mut rig = Rig::up("mint-ok", true, Some(true)).await;
    rig.arm().await;
    rig.request().await;
    let (serve, sa, sb) = rig.expect_minted().await;

    let vni = serve["vni"].as_u64().expect("vni");
    assert!(vni > 0 && vni <= 0xFF_FFFF, "24-bit vni, never 0: {vni}");
    assert_ne!(vni, 0x2112A4, "the STUN cookie is never minted");
    assert_eq!(sa["vni"], json!(vni));
    assert_eq!(sb["vni"], json!(vni));
    assert_eq!(serve["generation"], sa["generation"]);
    assert_eq!(serve["members"].as_array().map(Vec::len), Some(2));
    assert_eq!(sa["peer_node_id"], json!(rig.b_node));
    assert_eq!(sb["peer_node_id"], json!(rig.a_node));
    assert_eq!(sa["relay_node_id"], json!(rig.r_node));
    assert_eq!(
        sa["relay_endpoints"],
        json!(["8.8.8.8:3478"]),
        "the relay's public address, re-paired with its relay port"
    );
    assert_ne!(
        sa["bind_secret"], sb["bind_secret"],
        "one secret per member"
    );
    let secrets: Vec<&Value> = serve["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| &m["bind_secret"])
        .collect();
    assert!(secrets.contains(&&sa["bind_secret"]) && secrets.contains(&&sb["bind_secret"]));
    assert_eq!(sa["bind_secs"], json!(30));
    assert_eq!(serve["idle_secs"], json!(300));
    assert_eq!(serve["max_lifetime_secs"], json!(3600));

    let row = wait_mint_row(&rig.app, &rig.seeded).await;
    assert!(row["denied"].is_null(), "{row}");
    assert_eq!(row["vni"], json!(vni));
    assert_eq!(row["relay_node_id"], json!(rig.r_node));
    assert_eq!(row["agent_id"], json!(rig.r_agent));
    assert_eq!(row["requester_node_id"], json!(rig.a_node));
    assert_eq!(row["peer_node_id"], json!(rig.b_node));
    assert_eq!(row["warn_only"], json!(false));
    let rows_before = audit_rows(&rig.app, &rig.seeded).await.len();

    // Idempotent: the same pair asks again → the SAME session comes back to
    // the asker, nobody else hears anything, no new row.
    rig.request().await;
    let again = recv_t(
        &mut rig.a,
        "rc:overlay.relay_session",
        Duration::from_secs(5),
    )
    .await
    .expect("re-push to the asker");
    assert_eq!(again["vni"], json!(vni));
    assert_eq!(again["bind_secret"], sa["bind_secret"]);
    assert!(
        recv_t(
            &mut rig.r,
            "rc:overlay.relay_serve",
            Duration::from_millis(500)
        )
        .await
        .is_none()
    );
    assert_eq!(audit_rows(&rig.app, &rig.seeded).await.len(), rows_before);

    // The verdict: on a's next netmap, b is stamped `org-relay`.
    let netmap = join(&mut rig.a, 1, Some(true), "203.0.113.10:41641").await;
    let b_entry = netmap["peers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["node_id"] == json!(rig.b_node))
        .expect("b is in a's netmap");
    assert_eq!(b_entry["relay_strategy"], json!("org-relay"), "{b_entry}");
    let r_entry = netmap["peers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["node_id"] == json!(rig.r_node))
        .expect("r is in a's netmap");
    assert_ne!(
        r_entry["relay_strategy"],
        json!("org-relay"),
        "only the pair with a session"
    );
}

/// §7 — revocation is a push, from all four triggers. Each trigger starts
/// from a fresh mint; each revoke reaches the relay and both members and
/// leaves an audit row naming the trigger.
#[tokio::test]
async fn all_four_revocation_triggers_push_relay_revoke_to_every_party() {
    let mut rig = Rig::up("mint-revoke", true, Some(true)).await;
    let policy_id = rig.arm().await;

    // 1. Policy revoke — the relay's approval cleared.
    rig.request().await;
    let (serve, _, _) = rig.expect_minted().await;
    let vni1 = serve["vni"].as_u64().unwrap();
    assert_eq!(
        approve(&rig.app, &rig.seeded, &rig.r_agent, false).await,
        200
    );
    rig.expect_revoked(vni1).await;

    // 2. Mode off.
    assert_eq!(
        approve(&rig.app, &rig.seeded, &rig.r_agent, true).await,
        200
    );
    rig.request().await;
    let (serve, _, _) = rig.expect_minted().await;
    let vni2 = serve["vni"].as_u64().unwrap();
    assert_ne!(vni2, vni1, "a re-mint gets a fresh vni");
    assert!(
        serve["generation"].as_u64().unwrap() > 1,
        "generation is monotonic"
    );
    set_mode(&rig.app, &rig.seeded, "off").await;
    rig.expect_revoked(vni2).await;

    // 3. ACL revoke — the grant deleted.
    set_mode(&rig.app, &rig.seeded, "on").await;
    rig.request().await;
    let (serve, _, _) = rig.expect_minted().await;
    let vni3 = serve["vni"].as_u64().unwrap();
    let resp = app_delete(
        &rig.app,
        &url(&rig.seeded, &format!("/overlay-acl/{policy_id}")),
        &rig.seeded.admin.access_token,
    )
    .await;
    assert_eq!(resp, 200, "policy delete");
    rig.expect_revoked(vni3).await;

    // 4. Device removal — the relay deleted from the fleet.
    grant_relay(&rig.app, &rig.seeded, &rig.r_node).await;
    rig.request().await;
    let (serve, _, _) = rig.expect_minted().await;
    let vni4 = serve["vni"].as_u64().unwrap();
    let resp = app_delete(
        &rig.app,
        &url(&rig.seeded, &format!("/agent/{}", rig.r_agent)),
        &rig.seeded.admin.access_token,
    )
    .await;
    assert_eq!(resp, 200, "agent delete");
    let wait = Duration::from_secs(5);
    for (name, ws) in [("a", &mut rig.a), ("b", &mut rig.b)] {
        let v = recv_t(ws, "rc:overlay.relay_revoke", wait)
            .await
            .unwrap_or_else(|| panic!("{name} receives relay_revoke after the relay was removed"));
        assert_eq!(v["vni"], json!(vni4));
    }

    let reasons: Vec<String> = audit_rows(&rig.app, &rig.seeded)
        .await
        .iter()
        .filter(|r| r["action"] == json!("revoke"))
        .filter_map(|r| r["reason"].as_str().map(str::to_string))
        .collect();
    for want in [
        "policy_revoked",
        "mode_off",
        "acl_revoked",
        "device_removed",
    ] {
        assert!(
            reasons.contains(&want.to_string()),
            "missing revoke reason {want} in {reasons:?}"
        );
    }
}

async fn app_delete(app: &TestApp, path: &str, token: &str) -> u16 {
    app.client
        .delete(app.url(path))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// Gate 1's two closed postures: `off` writes nothing at all, `warn` decides
/// and audits exactly what `on` would have and pushes nothing.
#[tokio::test]
async fn mode_off_writes_nothing_and_warn_audits_without_pushing() {
    let mut rig = Rig::up("mint-mode", true, Some(true)).await;
    assert_eq!(
        approve(&rig.app, &rig.seeded, &rig.r_agent, true).await,
        200
    );
    grant_relay(&rig.app, &rig.seeded, &rig.r_node).await;

    // off (the default): no frames, no rows.
    rig.request().await;
    rig.assert_nothing_pushed_to_a().await;
    assert!(
        audit_rows(&rig.app, &rig.seeded)
            .await
            .iter()
            .all(|r| r["action"] != json!("mint")),
        "off ⇒ zero MINT rows (the approval writes its own row, legitimately)"
    );

    // warn: the decision is recorded as a would-be mint, nothing is pushed.
    set_mode(&rig.app, &rig.seeded, "warn").await;
    rig.request().await;
    let row = wait_mint_row(&rig.app, &rig.seeded).await;
    assert_eq!(row["warn_only"], json!(true), "{row}");
    assert!(row["denied"].is_null());
    assert!(
        row["vni"].is_number(),
        "warn records the vni it would have issued"
    );
    assert_eq!(row["relay_node_id"], json!(rig.r_node));
    rig.assert_nothing_pushed_to_a().await;
    assert!(
        recv_t(
            &mut rig.r,
            "rc:overlay.relay_serve",
            Duration::from_millis(300)
        )
        .await
        .is_none()
    );
}

/// Every refusal the spec enumerates for the server side, each on its own
/// rig, each audited with its reason and pushing nothing.
#[tokio::test]
async fn every_refusal_is_audited_with_its_reason() {
    // No grant at all → the ACL is an affirmative capability, so no relay.
    {
        let mut rig = Rig::up("mint-noacl", true, Some(true)).await;
        set_mode(&rig.app, &rig.seeded, "on").await;
        assert_eq!(
            approve(&rig.app, &rig.seeded, &rig.r_agent, true).await,
            200
        );
        rig.request().await;
        let row = wait_mint_row(&rig.app, &rig.seeded).await;
        assert_eq!(row["denied"], json!("acl_denied"), "{row}");
        rig.assert_nothing_pushed_to_a().await;
    }
    // Approved but not serving (no `relay-server` on its hello).
    {
        let mut rig = Rig::up("mint-nocap", false, Some(true)).await;
        rig.arm().await;
        rig.request().await;
        let row = wait_mint_row(&rig.app, &rig.seeded).await;
        assert_eq!(row["denied"], json!("no_relay"), "{row}");
        rig.assert_nothing_pushed_to_a().await;
    }
    // The asker joined from a secondary org — or did not say.
    {
        let mut rig = Rig::up("mint-secondary", true, Some(false)).await;
        rig.arm().await;
        rig.request().await;
        let row = wait_mint_row(&rig.app, &rig.seeded).await;
        assert_eq!(row["denied"], json!("secondary_org"), "{row}");
    }
    {
        let mut rig = Rig::up("mint-unsaid", true, None).await;
        rig.arm().await;
        rig.request().await;
        let row = wait_mint_row(&rig.app, &rig.seeded).await;
        assert_eq!(
            row["denied"],
            json!("secondary_org"),
            "an absent flag fails closed: {row}"
        );
    }
    // A static endpoint that is not a public address — refused by the
    // approval route, and refused at mint time when smuggled past it.
    {
        let mut rig = Rig::up("mint-ssrf", true, Some(true)).await;
        rig.arm().await;
        let (s, _) = put(
            &rig.app,
            &url(
                &rig.seeded,
                &format!("/agent/{}/peer-relay-policy", rig.r_agent),
            ),
            &rig.seeded.admin.access_token,
            json!({ "serve": true, "static_endpoints": ["169.254.169.254:80"] }),
        )
        .await;
        assert_eq!(s, 400, "the route refuses a metadata-service endpoint");
        rig.app
            .db
            .collection::<bson::Document>("agents")
            .update_one(
                doc! { "_id": ObjectId::parse_str(&rig.r_agent).unwrap() },
                doc! { "$set": { "peer_relay_policy.static_endpoints": ["10.0.0.5:3478"] } },
            )
            .await
            .unwrap();
        rig.request().await;
        let row = wait_mint_row(&rig.app, &rig.seeded).await;
        assert_eq!(row["denied"], json!("non_routable_endpoint"), "{row}");
        rig.assert_nothing_pushed_to_a().await;
    }
    // The per-(requester, relay) ceiling, pre-spent in process.
    {
        let mut rig = Rig::up("mint-rate", true, Some(true)).await;
        rig.arm().await;
        let (a, r) = (
            ObjectId::parse_str(&rig.a_node).unwrap(),
            ObjectId::parse_str(&rig.r_node).unwrap(),
        );
        for _ in 0..30 {
            assert!(rig.app.state.network().relay_rate_limiter.check(a, r, 30));
        }
        rig.request().await;
        let row = wait_mint_row(&rig.app, &rig.seeded).await;
        assert_eq!(row["denied"], json!("rate_limited"), "{row}");
        assert_eq!(
            row["relay_node_id"].as_str(),
            None,
            "no relay is named on a refusal before the mint"
        );
        rig.assert_nothing_pushed_to_a().await;
    }
    // Unreadable policies: fail CLOSED, with the reason that says so.
    {
        let mut rig = Rig::up("mint-unread", true, Some(true)).await;
        rig.arm().await;
        let tid = ObjectId::parse_str(&rig.seeded.tenant_id).unwrap();
        rig.app
            .db
            .collection::<bson::Document>("overlay_policies")
            .insert_one(doc! {
                "tenant_id": tid,
                "name": "broken",
                "enabled": true,
                "sources": "not-an-array",
                "via": [],
                "destinations": [],
                "created_at": bson::DateTime::now(),
                "updated_at": bson::DateTime::now(),
                "deleted_at": null,
            })
            .await
            .unwrap();
        rig.request().await;
        let row = wait_mint_row(&rig.app, &rig.seeded).await;
        assert_eq!(row["denied"], json!("policy_unreadable"), "{row}");
        rig.assert_nothing_pushed_to_a().await;
    }
}

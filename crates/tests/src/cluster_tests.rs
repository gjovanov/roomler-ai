// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! C-1 — cluster foundation: ownership directory + per-pod bus, exercised
//! across two in-process "pods" sharing one Redis.

use crate::fixtures::test_app::TestApp;
use roomler_ai_api::cluster::directory::{ClaimOutcome, OwnerRecord};
use roomler_ai_mod_conference::ConferenceState;

/// FR-69 P4 — the mediasoup room manager and the media claim maps are the
/// `conference` module's state now.
pub(crate) fn conf(app: &TestApp) -> &ConferenceState {
    app.state
        .modules
        .conference
        .as_ref()
        .expect("the conference module is mounted in tests")
}

/// Directory disciplines against real Redis: LWW overwrite, NX mutex,
/// refresh-if-mine conflict, compare-DEL identity gating.
#[tokio::test]
async fn directory_claim_refresh_release_conflict() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    let (Some(d1), Some(d2)) = (
        app1.state.cluster_directory.clone(),
        app2.state.cluster_directory.clone(),
    ) else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let key = format!("roomler:test:own:{}", uuid::Uuid::new_v4().simple());

    // LWW: pod1 claims, pod2 overwrites (newest wins), pod1's late release
    // must NOT free pod2's claim.
    let t1 = d1.owner_token("conn-a");
    d1.claim_lww(&key, &t1).await.unwrap();
    let t2 = d2.owner_token("conn-b");
    d2.claim_lww(&key, &t2).await.unwrap();
    assert!(
        !d1.release(&key, &t1).await.unwrap(),
        "stale release must no-op"
    );
    let raw = d1.get(&key).await.unwrap().expect("claim survives");
    assert_eq!(raw, t2);
    let rec = OwnerRecord::parse(&raw).expect("canonical record parses");
    assert_eq!(rec.pod_id, app2.state.pod.pod_id);
    assert!(d1.is_foreign(&raw));
    assert!(!d2.is_foreign(&raw));

    // refresh-if-mine: pod2 refreshes fine; pod1 gets CONFLICT and must
    // not clobber.
    assert!(d2.refresh_if_mine(&key, &t2, 90).await.unwrap());
    assert!(!d1.refresh_if_mine(&key, &t1, 90).await.unwrap());
    assert_eq!(d1.get(&key).await.unwrap().as_deref(), Some(raw.as_str()));

    // Identity-gated release by the true owner works.
    assert!(d2.release(&key, &t2).await.unwrap());
    assert_eq!(d1.get(&key).await.unwrap(), None);

    // NX mutex: exactly one winner; loser learns the holder.
    let m_key = format!("roomler:test:mutex:{}", uuid::Uuid::new_v4().simple());
    let (m1, m2) = (d1.owner_token("x"), d2.owner_token("y"));
    assert_eq!(
        d1.claim_nx(&m_key, &m1, 30).await.unwrap(),
        ClaimOutcome::Won
    );
    match d2.claim_nx(&m_key, &m2, 30).await.unwrap() {
        ClaimOutcome::Foreign(holder) => assert_eq!(holder, m1),
        other => panic!("expected Foreign, got {other:?}"),
    }
    // Absent-key refresh re-asserts (owner restart healing).
    assert!(d1.release(&m_key, &m1).await.unwrap());
    assert!(d1.refresh_if_mine(&m_key, &m1, 30).await.unwrap());
    assert_eq!(d1.get(&m_key).await.unwrap().as_deref(), Some(m1.as_str()));
    let _ = d1.release(&m_key, &m1).await;
}

/// Bus request/reply across pods: sys.ping round-trip, unknown-class NACK,
/// dead-pod deadline.
#[tokio::test]
async fn bus_rpc_roundtrip_nack_and_deadline() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    let (Some(b1), Some(b2)) = (
        app1.state.cluster_bus.clone(),
        app2.state.cluster_bus.clone(),
    ) else {
        eprintln!("skipping: no Redis available");
        return;
    };
    // Both subscriptions live (poll briefly — spawned async).
    for _ in 0..40 {
        if b1.sub_alive.load(std::sync::atomic::Ordering::Relaxed)
            && b2.sub_alive.load(std::sync::atomic::Ordering::Relaxed)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Round-trip pod1 → pod2.
    let rep = b1
        .request(b2.pod_id(), "sys.ping", serde_json::json!({"n": 7}))
        .await
        .expect("ping round-trip");
    assert_eq!(rep["pong"]["n"], 7);

    // Reverse direction too (reply routing is symmetric).
    let rep = b2
        .request(b1.pod_id(), "sys.ping", serde_json::json!({"n": 8}))
        .await
        .expect("reverse ping");
    assert_eq!(rep["pong"]["n"], 8);

    // Unknown class ⇒ structured NACK, not a deadline.
    let err = b1
        .request(b2.pod_id(), "media.nonexistent", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(err, roomler_ai_api::cluster::bus::BusError::Nack(_)),
        "expected NACK, got {err:?}"
    );

    // Nobody subscribes to a dead pod's channel ⇒ deadline (the ACTIVE
    // failure detector).
    let t0 = std::time::Instant::now();
    let err = b1
        .request_with_deadline(
            "no-such-pod",
            "sys.ping",
            serde_json::json!({}),
            std::time::Duration::from_millis(600),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, roomler_ai_api::cluster::bus::BusError::Deadline(_)),
        "expected Deadline, got {err:?}"
    );
    assert!(t0.elapsed() >= std::time::Duration::from_millis(550));
}

/// The Phase A-1 agent presence records are canonical directory records
/// now — C-2's rehome reads the owning pod straight out of them.
#[tokio::test]
async fn agent_presence_records_are_canonical_and_pod_attributed() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let seeded = app1.seed_tenant("clus").await;
    let (agent_id, agent_token) =
        crate::agent_presence_tests::enroll(&app1, &seeded, "mach-clus-a").await;
    let mut ws = crate::agent_presence_tests::connect_agent(&app1, &agent_token).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let d2 = app2.state.cluster_directory.clone().unwrap();
    let raw = d2
        .get(&roomler_ai_api::cluster::directory::agent_key(&agent_id))
        .await
        .unwrap()
        .expect("presence record exists");
    let rec = OwnerRecord::parse(&raw).expect("agent record is canonical");
    assert_eq!(
        rec.pod_id, app1.state.pod.pod_id,
        "owned by the WS-holding pod"
    );
    assert!(d2.is_foreign(&raw), "and foreign to the other pod");

    let _ = ws.close(None).await;
}

/// PR-2 - the relay end to end: a KEY-LESS controller on pod2 (the
/// worst dial shape, the 2026-08-04 incident) requests an agent homed
/// on pod1. Pre-PR-2 this was a rehome error plus eleven refused
/// nudges; now the frame relays to the owner pod over `rc.cmd`, the
/// replies route back conn-addressed, and the agent is never touched.
/// Closing the browser socket forwards the teardown (`rc.conn_closed`)
/// so the agent-side session dies and the slot frees.
#[tokio::test]
async fn cross_pod_session_relays_without_nudge() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_bus.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    for _ in 0..40 {
        let a = app1.state.cluster_bus.as_ref().unwrap();
        let b = app2.state.cluster_bus.as_ref().unwrap();
        if a.sub_alive.load(std::sync::atomic::Ordering::Relaxed)
            && b.sub_alive.load(std::sync::atomic::Ordering::Relaxed)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let seeded = app1.seed_tenant("rcrelay").await;
    let (agent_id, agent_token) =
        crate::agent_presence_tests::enroll(&app1, &seeded, "mach-rcrelay-a").await;
    let mut agent_ws = crate::agent_presence_tests::connect_agent(&app1, &agent_token).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let aid = bson::oid::ObjectId::parse_str(&agent_id).unwrap();

    // KEY-LESS controller on the OTHER pod.
    let (mut ctrl_ws, _) = connect_async(&format!(
        "ws://{}/ws?token={}",
        app2.addr,
        seeded
            .admin
            .access_token
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    ))
    .await
    .expect("controller ws");
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        futures::StreamExt::next(&mut ctrl_ws),
    )
    .await;

    ctrl_ws
        .send(Message::Text(
            serde_json::json!({
                "t": "rc:session.request",
                "agent_id": agent_id,
                "permissions": "VIEW",
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    // The relayed create answers rc:session.created over the
    // conn-addressed lane - an rc:error here means the relay regressed
    // to the rehome bounce.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut created = false;
    while tokio::time::Instant::now() < deadline && !created {
        let msg = match tokio::time::timeout(std::time::Duration::from_millis(500), ctrl_ws.next())
            .await
        {
            Ok(Some(Ok(Message::Text(t)))) => t,
            Ok(None) => break,
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg) else {
            continue;
        };
        match v.get("t").and_then(|x| x.as_str()) {
            Some("rc:session.created") => created = true,
            Some("rc:error") => panic!("relay must not bounce the controller: {v}"),
            _ => {}
        }
    }
    assert!(created, "rc:session.created never arrived via the relay");

    // The agent received the forwarded request and its socket SURVIVES
    // (no nudge on the relay path - a cycle would tear the session the
    // relay just built).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut saw_request = false;
    while tokio::time::Instant::now() < deadline && !saw_request {
        match tokio::time::timeout(std::time::Duration::from_millis(400), agent_ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t)
                    && v.get("t").and_then(|x| x.as_str()) == Some("rc:request")
                {
                    saw_request = true;
                }
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                panic!("agent WS closed - nudged on the relay path")
            }
            _ => continue,
        }
    }
    assert!(saw_request, "forwarded rc:request never reached the agent");
    assert!(app1.state.fleet().rc_hub.is_agent_online(aid));

    // Browser socket dies -> rc.conn_closed forwards -> the owner pod's
    // proxy unregisters -> the agent sees rc:terminate; its socket
    // stays up.
    let _ = ctrl_ws.close(None).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut terminated = false;
    while tokio::time::Instant::now() < deadline && !terminated {
        match tokio::time::timeout(std::time::Duration::from_millis(400), agent_ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t)
                    && v.get("t").and_then(|x| x.as_str()) == Some("rc:terminate")
                {
                    terminated = true;
                }
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                panic!("agent WS closed during proxy teardown")
            }
            _ => continue,
        }
    }
    assert!(
        terminated,
        "rc.conn_closed teardown never reached the agent as rc:terminate"
    );
    let _ = agent_ws.close(None).await;
}

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// `true` iff the agent's WS survives (no Close/EOF) for `for_ms`.
async fn agent_ws_stays_open(agent_ws: &mut WsClient, for_ms: u64) -> bool {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(for_ms);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(250), agent_ws.next()).await {
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => return false,
            _ => continue,
        }
    }
    true
}

/// PR-1/PR-2 - the owner-side `rc.agent_nudge` verb at its own layer
/// (with the relay in front, rc misses no longer trigger nudges):
/// truthful refusal reasons for tunnel-TARGET busy and ORIGIN busy (the
/// pre-PR-1 blind spot), a fired cycle for the idle case, and the
/// cooldown right after.
#[tokio::test]
async fn nudge_verb_reasons_cooldown_and_idle_cycle() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_bus.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    for _ in 0..40 {
        let a = app1.state.cluster_bus.as_ref().unwrap();
        let b = app2.state.cluster_bus.as_ref().unwrap();
        if a.sub_alive.load(std::sync::atomic::Ordering::Relaxed)
            && b.sub_alive.load(std::sync::atomic::Ordering::Relaxed)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let seeded = app1.seed_tenant("nudgev").await;
    let (agent_id, agent_token) =
        crate::agent_presence_tests::enroll(&app1, &seeded, "mach-nudgev-a").await;
    let mut agent_ws = crate::agent_presence_tests::connect_agent(&app1, &agent_token).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let aid = bson::oid::ObjectId::parse_str(&agent_id).unwrap();
    let b2 = app2.state.cluster_bus.clone().unwrap();
    let pod1 = app1.state.pod.pod_id.clone();
    let body = serde_json::json!({ "agent_id": agent_id });

    // (a) tunnel session TARGETING the agent: refused, truthfully.
    let fake_session = bson::oid::ObjectId::new();
    app1.state
        .network()
        .tunnel_sessions_by_target_agent
        .entry(aid)
        .or_default()
        .insert(fake_session);
    let rep = b2
        .request(&pod1, "rc.agent_nudge", body.clone())
        .await
        .expect("nudge rpc");
    assert_eq!(rep["nudged"], false, "{rep}");
    assert_eq!(rep["reason"], "tunnel_busy", "{rep}");
    app1.state
        .network()
        .tunnel_sessions_by_target_agent
        .remove(&aid);

    // (b) session the agent ORIGINATED (declared routes) - the pre-PR-1
    // blind spot index.
    app1.state
        .network()
        .tunnel_sessions_by_origin_agent
        .entry(aid)
        .or_default()
        .insert(fake_session);
    let rep = b2
        .request(&pod1, "rc.agent_nudge", body.clone())
        .await
        .expect("nudge rpc");
    assert_eq!(rep["nudged"], false, "{rep}");
    assert_eq!(rep["reason"], "origin_busy", "{rep}");
    app1.state
        .network()
        .tunnel_sessions_by_origin_agent
        .remove(&aid);

    // Busy refusals never cycled the socket (nor consumed cooldown
    // attempts - the fired cycle below proves the gate stayed open).
    assert!(
        agent_ws_stays_open(&mut agent_ws, 1_000).await,
        "busy refusal must not cycle the agent WS"
    );

    // (c) fully idle: the cycle fires and the WS closes.
    let rep = b2
        .request(&pod1, "rc.agent_nudge", body.clone())
        .await
        .expect("nudge rpc");
    assert_eq!(rep["nudged"], true, "{rep}");
    assert!(
        !agent_ws_stays_open(&mut agent_ws, 5_000).await,
        "idle nudge must cycle the agent WS"
    );
    for _ in 0..10 {
        if !app1.state.fleet().rc_hub.is_agent_online(aid) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(!app1.state.fleet().rc_hub.is_agent_online(aid));

    // (d) straight after a fired cycle: the cooldown refuses.
    let rep = b2
        .request(&pod1, "rc.agent_nudge", body)
        .await
        .expect("nudge rpc");
    assert_eq!(rep["nudged"], false, "{rep}");
    assert_eq!(rep["reason"], "cooldown", "{rep}");
}

/// C-2 — an admin kick issued on pod2 reaches the hub on pod1 via the
/// broadcast ctrl event (the DELETE can land on any pod).
#[tokio::test]
async fn kick_ctrl_event_applies_cross_pod() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.redis_pubsub.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let seeded = app1.seed_tenant("kickx").await;
    let (agent_id, agent_token) =
        crate::agent_presence_tests::enroll(&app1, &seeded, "mach-kickx-a").await;
    let _agent_ws = crate::agent_presence_tests::connect_agent(&app1, &agent_token).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let aid = bson::oid::ObjectId::parse_str(&agent_id).unwrap();
    assert!(app1.state.fleet().rc_hub.is_agent_online(aid));

    // DELETE via pod2's HTTP API — its local hub has no entry; the ctrl
    // broadcast must reach pod1.
    let resp = app2
        .auth_delete(
            &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, agent_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "delete failed: {}",
        resp.status()
    );

    let mut kicked = false;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if !app1.state.fleet().rc_hub.is_agent_online(aid) {
            kicked = true;
            break;
        }
    }
    assert!(kicked, "kick ctrl event never reached pod1's hub");
}

/// C-3 — tunnel rehome: an open driven through pod2 targeting an agent
/// homed on pod1 is rejected `agent_on_other_pod` at OPEN time (never a
/// session that black-holes at forward time), and the idle target gets
/// nudged. The CLI driver bails on open-errors and its reconnect loop
/// dials a fresh WS — the redial-retry is structurally free client-side.
/// PR-1: the origin dials KEYED (like every rc.29x+ agent/CLI build) and
/// the guard band is zeroed — the direction rule only nudges for a
/// keyed, provably-newer originator.
#[tokio::test]
async fn tunnel_open_rehome_cross_pod() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (app1, app2) = TestApp::spawn_pair(|s| {
        s.rc.rehome_direction_guard_ms = 0;
    })
    .await;
    if app1.state.cluster_bus.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    for _ in 0..40 {
        let a = app1.state.cluster_bus.as_ref().unwrap();
        let b = app2.state.cluster_bus.as_ref().unwrap();
        if a.sub_alive.load(std::sync::atomic::Ordering::Relaxed)
            && b.sub_alive.load(std::sync::atomic::Ordering::Relaxed)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let seeded = app1.seed_tenant("trehome").await;
    // Target B homed on pod1; origin A drives the tunnel-client role on pod2.
    let (b_id, b_tok) =
        crate::tunnel_tests::enroll_agent(&app1, &seeded, "mach-trehome-B", "target-B").await;
    let mut b_ws = crate::tunnel_tests::connect_agent_ws(&app1, &b_tok, "target-B").await;
    let (_a_id, a_tok) =
        crate::tunnel_tests::enroll_agent(&app2, &seeded, "mach-trehome-A", "origin-A").await;
    let mut a_ws =
        crate::tunnel_tests::connect_agent_ws_keyed(&app2, &a_tok, "origin-A", &seeded.tenant_id)
            .await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    a_ws.send(Message::Text(
        serde_json::json!({
            "t": "rc:tunnel.hello",
            "role": "client",
            "version": "0.3.0",
            "supported_transports": ["webrtc-dc-v1"],
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    a_ws.send(Message::Text(
        serde_json::json!({
            "t": "rc:tunnel.open",
            "agent_id": b_id,
            "transport": "webrtc-dc-v1",
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let err = crate::tunnel_tests::read_until(&mut a_ws, "rc:error")
        .await
        .expect("origin must receive the rehome error");
    assert_eq!(
        err["code"].as_str(),
        Some("agent_on_other_pod"),
        "cross-pod open must rehome, not black-hole: {err}"
    );

    // The idle target's WS on pod1 gets nudged closed.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut closed = false;
    while tokio::time::Instant::now() < deadline && !closed {
        match tokio::time::timeout(std::time::Duration::from_millis(500), b_ws.next()).await {
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => closed = true,
            Ok(Some(Err(_))) => closed = true,
            _ => continue,
        }
    }
    assert!(closed, "idle target agent's WS was not nudged closed");
}

// ── C-4: media claim-or-route ───────────────────────────────────────

pub(crate) type UserWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Connect a user WS and swallow the initial `connected` frame.
async fn connect_user_ws(app: &TestApp, token: &str) -> UserWs {
    use futures::StreamExt;
    let url = format!("ws://{}/ws?token={}", app.addr, token);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("user WS connects");
    ws.next().await;
    ws
}

/// Next `media:*` frame (skips room:*/presence/notification noise).
pub(crate) async fn next_media_msg(ws: &mut UserWs) -> serde_json::Value {
    use futures::StreamExt;
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .expect("media frame within 10s")
            .expect("ws open")
            .expect("ws frame");
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(msg.to_text().unwrap_or(""))
        else {
            continue;
        };
        if parsed["type"].as_str().unwrap_or("").starts_with("media:") {
            return parsed;
        }
    }
}

pub(crate) async fn send_media(ws: &mut UserWs, msg_type: &str, room_id: &str) {
    use futures::SinkExt;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::json!({ "type": msg_type, "data": { "room_id": room_id } })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
}

/// Seed a room with a started call on app1; returns (tenant, room_id hex).
async fn seed_started_call(app1: &TestApp) -> (crate::fixtures::seed::SeededTenant, String) {
    let tenant = app1.seed_tenant("mediac4").await;
    let resp = app1
        .auth_post(
            &format!("/api/tenant/{}/room", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "name": "C4 Conference" }))
        .send()
        .await
        .unwrap();
    let room: serde_json::Value = resp.json().await.unwrap();
    let room_id = room["id"].as_str().unwrap().to_string();
    let resp = app1
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/call/start",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    (tenant, room_id)
}

/// `call/join` as `token`, in the bodyless shape an older client sends: the
/// media namespace admits the account's sockets only once it holds an open
/// call session (`conference_gate_tests`).
pub(crate) async fn join_call(app: &TestApp, tenant_id: &str, room_id: &str, token: &str) {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{tenant_id}/room/{room_id}/call/join"),
            token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

/// The C-4 core: a viewer on the non-owner pod joins through the bus,
/// gets its transports conn-addressed back, and NO second router island
/// is created; a leave on the owner reaches the remote viewer.
#[tokio::test]
async fn media_join_routes_to_owner_pod_single_router() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let (tenant, room_id) = seed_started_call(&app1).await;
    let rid = bson::oid::ObjectId::parse_str(&room_id).unwrap();

    // call/start claimed + created on app1 only.
    assert!(conf(&app1).room_manager.has_room(&rid));
    assert!(!conf(&app2).room_manager.has_room(&rid));

    // Member joins via app2 (the NON-owner pod).
    join_call(
        &app2,
        &tenant.tenant_id,
        &room_id,
        &tenant.member.access_token,
    )
    .await;
    let mut ws2 = connect_user_ws(&app2, &tenant.member.access_token).await;
    send_media(&mut ws2, "media:join", &room_id).await;
    let caps = next_media_msg(&mut ws2).await;
    assert_eq!(caps["type"], "media:router_capabilities", "{caps}");
    let transport = next_media_msg(&mut ws2).await;
    assert_eq!(transport["type"], "media:transport_created", "{transport}");
    assert!(
        transport["data"]["send_transport"]["id"].as_str().is_some(),
        "owner-built transports must round-trip: {transport}"
    );

    // Exactly ONE router: app2 must NOT have materialized the room.
    assert!(!conf(&app2).room_manager.has_room(&rid));
    assert!(
        conf(&app2).remote_media_conns.len() == 1,
        "remote membership tracked for WS-close forwarding"
    );

    // Admin joins locally on the owner, then leaves — the peer_left push
    // must cross pods to the remote viewer's connection.
    join_call(
        &app1,
        &tenant.tenant_id,
        &room_id,
        &tenant.admin.access_token,
    )
    .await;
    let mut ws1 = connect_user_ws(&app1, &tenant.admin.access_token).await;
    send_media(&mut ws1, "media:join", &room_id).await;
    let m = next_media_msg(&mut ws1).await;
    assert_eq!(m["type"], "media:router_capabilities");
    let m = next_media_msg(&mut ws1).await;
    assert_eq!(m["type"], "media:transport_created");

    send_media(&mut ws1, "media:leave", &room_id).await;
    let peer_left = next_media_msg(&mut ws2).await;
    assert_eq!(peer_left["type"], "media:peer_left", "{peer_left}");
    assert_eq!(
        peer_left["data"]["user_id"].as_str(),
        Some(tenant.admin.id.as_str())
    );

    ws1.close(None).await.ok();
    ws2.close(None).await.ok();
}

/// Owner death (graceful): shutdown releases the claim with zero gap —
/// the next join claims fresh on the surviving pod and serves locally.
#[tokio::test]
async fn media_owner_shutdown_releases_claim_rejoin_wins() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    let Some(d2) = app2.state.cluster_directory.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let (tenant, room_id) = seed_started_call(&app1).await;
    let rid = bson::oid::ObjectId::parse_str(&room_id).unwrap();
    let key = roomler_ai_api::cluster::directory::media_key(&room_id);

    join_call(
        &app2,
        &tenant.tenant_id,
        &room_id,
        &tenant.member.access_token,
    )
    .await;
    let mut ws2 = connect_user_ws(&app2, &tenant.member.access_token).await;
    send_media(&mut ws2, "media:join", &room_id).await;
    let m = next_media_msg(&mut ws2).await;
    assert_eq!(m["type"], "media:router_capabilities");
    let m = next_media_msg(&mut ws2).await;
    assert_eq!(m["type"], "media:transport_created");

    // Graceful owner shutdown → claim compare-DELed immediately.
    roomler_ai_api::state::shutdown_cleanup(&app1.state).await;
    assert_eq!(
        d2.get(&key).await.unwrap(),
        None,
        "graceful shutdown must release the media claim"
    );

    // Rejoin on the survivor: claims fresh + materializes locally.
    send_media(&mut ws2, "media:join", &room_id).await;
    let m = next_media_msg(&mut ws2).await;
    assert_eq!(m["type"], "media:router_capabilities", "{m}");
    let m = next_media_msg(&mut ws2).await;
    assert_eq!(m["type"], "media:transport_created");
    assert!(
        conf(&app2).room_manager.has_room(&rid),
        "survivor must own the rebuilt room"
    );
    let raw = d2.get(&key).await.unwrap().expect("fresh claim exists");
    let rec = OwnerRecord::parse(&raw).unwrap();
    assert_eq!(rec.pod_id, app2.state.pod.pod_id);

    ws2.close(None).await.ok();
}

/// The NX discipline under a real concurrent race: exactly one winner.
#[tokio::test]
async fn media_claim_race_single_winner() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    let (Some(d1), Some(d2)) = (
        app1.state.cluster_directory.clone(),
        app2.state.cluster_directory.clone(),
    ) else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let key =
        roomler_ai_api::cluster::directory::media_key(&uuid::Uuid::new_v4().simple().to_string());
    let (t1, t2) = (d1.owner_token("media"), d2.owner_token("media"));
    let (r1, r2) = tokio::join!(d1.claim_nx(&key, &t1, 30), d2.claim_nx(&key, &t2, 30));
    let wins = [r1.unwrap(), r2.unwrap()]
        .iter()
        .filter(|o| matches!(o, ClaimOutcome::Won))
        .count();
    assert_eq!(wins, 1, "exactly one pod may materialize a room");
}

/// The claim-loser fold: a foreign owner on the key (post-outage double
/// claim) makes the refresh CONFLICT — the loser tears down its island
/// and pushes the rejoin signal to its participants.
#[tokio::test]
async fn media_conflict_folds_loser_island() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    let Some(d2) = app2.state.cluster_directory.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let (tenant, room_id) = seed_started_call(&app1).await;
    let rid = bson::oid::ObjectId::parse_str(&room_id).unwrap();

    join_call(
        &app1,
        &tenant.tenant_id,
        &room_id,
        &tenant.admin.access_token,
    )
    .await;
    let mut ws1 = connect_user_ws(&app1, &tenant.admin.access_token).await;
    send_media(&mut ws1, "media:join", &room_id).await;
    let m = next_media_msg(&mut ws1).await;
    assert_eq!(m["type"], "media:router_capabilities");
    let m = next_media_msg(&mut ws1).await;
    assert_eq!(m["type"], "media:transport_created");

    // Simulate the post-Redis-outage double claim: app2 overwrites the key.
    let key = roomler_ai_api::cluster::directory::media_key(&room_id);
    d2.claim_lww(&key, &d2.owner_token("media")).await.unwrap();

    // One maintenance pass on the loser: CONFLICT → fold.
    roomler_ai_mod_conference::media_cluster::refresh_media_claims_once(conf(&app1)).await;
    assert!(
        !conf(&app1).room_manager.has_room(&rid),
        "claim loser must tear down its island"
    );
    let closed = next_media_msg(&mut ws1).await;
    assert_eq!(closed["type"], "media:room_closed", "{closed}");
    assert_eq!(closed["data"]["reason"].as_str(), Some("rehomed"));

    ws1.close(None).await.ok();
}

/// The participation gate holds across pods. A `media:join` from a socket on
/// the non-owner pod is refused there, before it is forwarded; and a join
/// that reaches the owner over the bus unchecked — what a pod that does not
/// gate would send, mid-roll — is refused by the owner, the pod that would
/// seat the connection.
#[tokio::test]
async fn media_join_is_admitted_on_both_pods() {
    use futures::StreamExt;

    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let (_tenant, room_id) = seed_started_call(&app1).await;
    let rid = bson::oid::ObjectId::parse_str(&room_id).unwrap();
    assert!(conf(&app1).room_manager.has_room(&rid));

    // A signed-in account of another org, its socket on the non-owner pod.
    let outsider = app2.seed_tenant("mediac4-out").await;
    let outsider_id = bson::oid::ObjectId::parse_str(&outsider.admin.id).unwrap();
    let url = format!(
        "ws://{}/ws?token={}",
        app2.addr, outsider.admin.access_token
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("user WS connects");
    let first = ws.next().await.expect("a frame").expect("a text frame");
    let connected: serde_json::Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    let conn = connected["connection_id"]
        .as_str()
        .expect("the connected frame carries connection_id")
        .to_string();

    // Through its socket: refused on the non-owner pod, nothing forwarded.
    send_media(&mut ws, "media:join", &room_id).await;
    let m = next_media_msg(&mut ws).await;
    assert_eq!(m["type"], "media:error", "{m}");
    assert!(
        conf(&app2).remote_media_conns.get(&conn).is_none(),
        "a refused join is not forwarded"
    );

    // Straight over the bus: the owner refuses it and seats nobody.
    let ok = roomler_ai_mod_conference::media_cluster::forward_media_cmd(
        conf(&app2),
        &app1.state.pod.pod_id,
        &outsider_id,
        &conn,
        "media:join",
        Some(&serde_json::json!({ "room_id": room_id })),
    )
    .await;
    assert!(ok, "the owner answers the RPC");
    let m = next_media_msg(&mut ws).await;
    assert_eq!(m["type"], "media:error", "{m}");
    assert_eq!(
        conf(&app1).room_manager.get_connection_room(&conn),
        None,
        "the owner seated the forwarded join"
    );
    assert!(
        !conf(&app1)
            .room_manager
            .get_participant_user_ids(&rid)
            .contains(&outsider_id)
    );

    ws.close(None).await.ok();
}

/// Connect a user WS; returns it with the connection id the server minted.
pub(crate) async fn connect_user_ws_conn(app: &TestApp, token: &str) -> (UserWs, String) {
    use futures::StreamExt;
    let url = format!("ws://{}/ws?token={}", app.addr, token);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("user WS connects");
    let first = ws.next().await.expect("a frame").expect("a text frame");
    let connected: serde_json::Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    let conn = connected["connection_id"]
        .as_str()
        .expect("the connected frame carries connection_id")
        .to_string();
    (ws, conn)
}

/// The next frame of type `kind`, whatever namespace it is in, within 10 s.
pub(crate) async fn next_frame_of(ws: &mut UserWs, kind: &str) -> serde_json::Value {
    use futures::StreamExt;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut seen = Vec::new();
    loop {
        let left = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_else(|| panic!("no {kind} within 10 s; saw {seen:?}"));
        let msg = match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(msg))) => msg,
            other => panic!("socket ended waiting for {kind}: {other:?}; saw {seen:?}"),
        };
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(msg.to_text().unwrap_or(""))
        else {
            continue;
        };
        if parsed["type"] == kind {
            return parsed;
        }
        seen.push(parsed["type"].as_str().unwrap_or("").to_string());
    }
}

/// A call owned by app1 with the org's admin in its media on app1, and the
/// org's member in it from a socket on app2: the member's seat lives on app1,
/// its socket on app2. Returns the two sockets and the member's connection.
async fn call_across_pods(
    app1: &TestApp,
    app2: &TestApp,
) -> (
    crate::fixtures::seed::SeededTenant,
    String,
    UserWs,
    UserWs,
    String,
) {
    let (tenant, room_id) = seed_started_call(app1).await;
    let rid = bson::oid::ObjectId::parse_str(&room_id).unwrap();

    join_call(
        app1,
        &tenant.tenant_id,
        &room_id,
        &tenant.admin.access_token,
    )
    .await;
    let mut host = connect_user_ws(app1, &tenant.admin.access_token).await;
    send_media(&mut host, "media:join", &room_id).await;
    assert_eq!(
        next_media_msg(&mut host).await["type"],
        "media:router_capabilities"
    );
    assert_eq!(
        next_media_msg(&mut host).await["type"],
        "media:transport_created"
    );

    join_call(
        app2,
        &tenant.tenant_id,
        &room_id,
        &tenant.member.access_token,
    )
    .await;
    let (mut member, conn) = connect_user_ws_conn(app2, &tenant.member.access_token).await;
    send_media(&mut member, "media:join", &room_id).await;
    assert_eq!(
        next_media_msg(&mut member).await["type"],
        "media:router_capabilities"
    );
    assert_eq!(
        next_media_msg(&mut member).await["type"],
        "media:transport_created"
    );
    assert_eq!(
        conf(app1).room_manager.get_connection_room(&conn),
        Some(rid),
        "the member's seat is on the owner pod"
    );
    assert!(!conf(app2).room_manager.has_room(&rid));
    (tenant, room_id, host, member, conn)
}

/// The seat is withdrawn on the pod that holds it, though the membership
/// ended on the other: the member's socket (on app2) hears its call ended,
/// the host (on app1) sees it leave, and app1 holds no seat for it.
async fn assert_seat_gone_across_pods(
    app1: &TestApp,
    room_id: &str,
    host: &mut UserWs,
    member: &mut UserWs,
    conn: &str,
) {
    let ended = next_frame_of(member, "room:call_ended").await;
    assert_eq!(ended["data"]["room_id"], room_id, "{ended}");
    let left = next_frame_of(host, "media:peer_left").await;
    assert_eq!(left["data"]["connection_id"], conn, "{left}");
    assert_eq!(
        conf(app1).room_manager.get_connection_room(conn),
        None,
        "the owner pod still seats the member"
    );
}

/// A member removed from the org by a request the NON-owner pod serves loses
/// the seat the owner pod holds for them.
#[tokio::test]
async fn an_org_removal_on_one_pod_withdraws_the_seat_the_other_pod_holds() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let (tenant, room_id, mut host, mut member, conn) = call_across_pods(&app1, &app2).await;

    let resp = app2
        .auth_delete(
            &format!(
                "/api/tenant/{}/member/{}",
                tenant.tenant_id, tenant.member.id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    assert_seat_gone_across_pods(&app1, &room_id, &mut host, &mut member, &conn).await;
    host.close(None).await.ok();
    member.close(None).await.ok();
}

/// A member who leaves the room through the NON-owner pod loses the seat the
/// owner pod holds for them.
#[tokio::test]
async fn a_room_leave_on_one_pod_withdraws_the_seat_the_other_pod_holds() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let (tenant, room_id, mut host, mut member, conn) = call_across_pods(&app1, &app2).await;

    let resp = app2
        .auth_post(
            &format!("/api/tenant/{}/room/{}/leave", tenant.tenant_id, room_id),
            &tenant.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    assert_seat_gone_across_pods(&app1, &room_id, &mut host, &mut member, &conn).await;
    host.close(None).await.ok();
    member.close(None).await.ok();
}

/// The id of the call running in `room_id`, read from the room's document
/// before the room goes (the id goes with it).
async fn current_call_id(app: &TestApp, room_id: &str) -> bson::oid::ObjectId {
    let rid = bson::oid::ObjectId::parse_str(room_id).unwrap();
    let found = app
        .db
        .collection::<bson::Document>("rooms")
        .find_one(bson::doc! { "_id": rid })
        .await
        .unwrap()
        .expect("the room exists");
    found
        .get_object_id("current_call_id")
        .unwrap_or_else(|_| panic!("a call is running in the room: {found}"))
}

/// `call_across_pods`' room is deleted by a request `via` serves: the call
/// ends on both pods. Both sockets hear it ended (the member's is on app2,
/// its seat on app1), neither pod keeps a router, seat or claim of it, and
/// its stats record closes.
async fn delete_the_call_room_via(via: &TestApp, app1: &TestApp, app2: &TestApp) {
    let (tenant, room_id, mut host, mut member, conn) = call_across_pods(app1, app2).await;
    let call_id = current_call_id(app1, &room_id).await;

    let resp = via
        .auth_delete(
            &format!("/api/tenant/{}/room/{}", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the admin deletes the room");

    assert_deleted_rooms_call_ended(app1, app2, &room_id, call_id, &mut host, &mut member, &conn)
        .await;
    host.close(None).await.ok();
    member.close(None).await.ok();
}

/// Every frame `ws` receives within `ms` milliseconds.
pub(crate) async fn frames_within(ws: &mut UserWs, ms: u64) -> Vec<serde_json::Value> {
    use futures::StreamExt;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
    let mut seen = Vec::new();
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(deadline, ws.next()).await {
        if let Ok(frame) = serde_json::from_str::<serde_json::Value>(msg.to_text().unwrap_or("")) {
            seen.push(frame);
        }
    }
    seen
}

/// Delete the room's document and nothing else: the room is gone and no one
/// told conference, as when a delete's request died after the delete, a pod
/// whose code ends no call served it, or its word to the owner was lost.
async fn delete_room_document(app: &TestApp, room_id: &str) {
    let rid = bson::oid::ObjectId::parse_str(room_id).unwrap();
    let gone = app
        .db
        .collection::<bson::Document>("rooms")
        .delete_one(bson::doc! { "_id": rid })
        .await
        .unwrap();
    assert_eq!(gone.deleted_count, 1, "the room's document was there");
}

/// The call of the deleted room `room_id` (served by app1, the member's socket
/// on app2) ended for everyone in it: each socket hears it once, with
/// `reason: "room_deleted"`; neither pod keeps its router or a claim, the
/// owner seats no one, the shared claim record is gone, and its stats record
/// closed.
async fn assert_deleted_rooms_call_ended(
    app1: &TestApp,
    app2: &TestApp,
    room_id: &str,
    call_id: bson::oid::ObjectId,
    host: &mut UserWs,
    member: &mut UserWs,
    conn: &str,
) {
    let rid = bson::oid::ObjectId::parse_str(room_id).unwrap();
    for (who, ws) in [("the host", &mut *host), ("the member", &mut *member)] {
        let ended = next_frame_of(ws, "room:call_ended").await;
        assert_eq!(ended["data"]["room_id"], room_id, "{who}: {ended}");
        assert_eq!(ended["data"]["reason"], "room_deleted", "{who}: {ended}");
    }
    for (who, ws) in [("the host", &mut *host), ("the member", &mut *member)] {
        let again: Vec<serde_json::Value> = frames_within(ws, 1500)
            .await
            .into_iter()
            .filter(|f| f["type"] == "room:call_ended")
            .collect();
        assert!(again.is_empty(), "{who} heard it twice: {again:?}");
    }
    for (pod, app) in [("app1", app1), ("app2", app2)] {
        assert!(
            !conf(app).room_manager.has_room(&rid),
            "{pod} kept the call's router"
        );
        assert!(
            !conf(app).media_claim_tokens.contains_key(&rid),
            "{pod} still holds the call's claim"
        );
    }
    assert_eq!(
        conf(app1).room_manager.get_connection_room(conn),
        None,
        "the owner pod still seats the member"
    );
    let key = roomler_ai_api::cluster::directory::media_key(room_id);
    let dir = app1.state.cluster_directory.clone().unwrap();
    assert_eq!(
        dir.get(&key).await.unwrap(),
        None,
        "the call's claim outlived its room"
    );
    let record = app1
        .db
        .collection::<bson::Document>("call_sessions")
        .find_one(bson::doc! { "_id": call_id })
        .await
        .unwrap()
        .expect("the call has a stats record");
    assert_eq!(
        record.get_str("end_reason").ok(),
        Some("room_deleted"),
        "the call's stats record: {record}"
    );
}

/// A room deleted on the pod that serves its call ends the call there, and
/// tells the socket the other pod holds.
#[tokio::test]
async fn a_room_deleted_on_the_pod_serving_its_call_ends_the_call_on_both_pods() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    delete_the_call_room_via(&app1, &app1, &app2).await;
}

/// A room deleted on the pod that does NOT serve its call ends the call on
/// the pod that does.
#[tokio::test]
async fn a_room_deleted_on_the_other_pod_ends_its_call_on_the_pod_serving_it() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    delete_the_call_room_via(&app2, &app1, &app2).await;
}

/// The room's document goes while app1 serves its call and app2 holds the
/// member's socket, and nothing tells conference. app1, the pod serving the
/// call, ends it on its own once two of its reconcile reads in a row found no
/// document: for both sockets, and with its claim.
#[tokio::test]
async fn a_call_whose_room_is_gone_is_ended_by_the_pod_serving_it_across_pods() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let (_tenant, room_id, mut host, mut member, conn) = call_across_pods(&app1, &app2).await;
    let call_id = current_call_id(&app1, &room_id).await;

    delete_room_document(&app1, &room_id).await;
    for _ in 0..2 {
        roomler_ai_mod_conference::media_cluster::end_calls_of_rooms_gone(conf(&app1)).await;
    }

    assert_deleted_rooms_call_ended(
        &app1,
        &app2,
        &room_id,
        call_id,
        &mut host,
        &mut member,
        &conn,
    )
    .await;
    host.close(None).await.ok();
    member.close(None).await.ok();
}

/// The pod serving a deleted room's call, asked over the bus to close it for
/// that reason (what a deleting pod that does not serve the call sends), tells
/// its own people itself: both sockets hear that the call ended, once, and the
/// pod that asked says nothing to anyone.
#[tokio::test]
async fn the_pod_serving_a_deleted_rooms_call_tells_its_own_people() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let (_tenant, room_id, mut host, mut member, conn) = call_across_pods(&app1, &app2).await;
    let call_id = current_call_id(&app1, &room_id).await;

    delete_room_document(&app1, &room_id).await;
    let reply = app2
        .state
        .cluster_bus
        .clone()
        .unwrap()
        .request(
            &app1.state.pod.pod_id,
            "media.close_room",
            serde_json::json!({ "room_id": room_id, "reason": "room_deleted" }),
        )
        .await
        .expect("the pod serving the call answers");
    assert_eq!(reply["ok"], true, "{reply}");

    assert_deleted_rooms_call_ended(
        &app1,
        &app2,
        &room_id,
        call_id,
        &mut host,
        &mut member,
        &conn,
    )
    .await;
    host.close(None).await.ok();
    member.close(None).await.ok();
}

/// A media claim record naming `pod`, as that pod's own directory writes one.
fn claim_record_of(pod: &str) -> String {
    format!("{pod}/1/media/{}", bson::DateTime::now().timestamp_millis())
}

/// The pod serving a deleted room's call cannot be reached when the room is
/// deleted: the room's claim names a pod no one runs, while app1 serves the
/// call and app2 holds the member's socket. The delete, which app2 serves,
/// still answers 200, within its bound, once it has waited out the bus
/// deadline for that pod; its hook could not end the call's media, so no one
/// hears of it yet and app1 still serves it, though the hook has closed the
/// call's stats record by the room already. app1's reconcile ends the call
/// afterwards, for both sockets, with its claim.
#[tokio::test]
async fn a_room_deleted_while_the_pod_serving_its_call_cannot_be_reached_ends_it_afterwards() {
    use roomler_ai_api::cluster::bus::RPC_DEADLINE;
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    let Some(dir) = app1.state.cluster_directory.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let (tenant, room_id, mut host, mut member, conn) = call_across_pods(&app1, &app2).await;
    let call_id = current_call_id(&app1, &room_id).await;
    let rid = bson::oid::ObjectId::parse_str(&room_id).unwrap();

    // The claim now names a pod no one runs. app1 holds that record as its
    // own, so its claim refresh neither folds the call nor gives the record
    // up while the test runs. The two writes are one Redis round trip apart:
    // a refresh of app1's landing between them finds a record that is not its
    // token, in either order of the two, and folds the call (about one run in
    // ten thousand; no order closes that gap without a seam in the refresh).
    let key = roomler_ai_api::cluster::directory::media_key(&room_id);
    let unreachable = format!("gone-{}", bson::oid::ObjectId::new().to_hex());
    let raw = claim_record_of(&unreachable);
    dir.claim_lww(&key, &raw).await.unwrap();
    conf(&app1).media_claim_tokens.insert(rid, raw);

    let started = std::time::Instant::now();
    let resp = app2
        .auth_delete(
            &format!("/api/tenant/{}/room/{}", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let took = started.elapsed();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "the delete answers whatever its hook says"
    );
    assert!(
        took >= RPC_DEADLINE,
        "the delete did not wait for the pod its claim names: {took:?}"
    );
    // It answers within its bound, `ROOM_REMOVED_BOUND` (5 s) in chat's room
    // delete, plus the delete's own reads and writes: a pod that never
    // answers holds it no longer. The hook alone takes about `RPC_DEADLINE`
    // here; the rest is slack for a loaded runner.
    let room_removed_bound = std::time::Duration::from_secs(5);
    assert!(
        took < room_removed_bound + std::time::Duration::from_secs(5),
        "the delete did not answer within its bound: {took:?}"
    );

    // The hook could not end the call: no one has heard of it, and app1
    // still serves it.
    for (who, ws) in [("the host", &mut host), ("the member", &mut member)] {
        let heard: Vec<serde_json::Value> = frames_within(ws, 500)
            .await
            .into_iter()
            .filter(|f| f["type"] == "room:call_ended")
            .collect();
        assert!(
            heard.is_empty(),
            "{who} heard the call end before the pod serving it knew: {heard:?}"
        );
    }
    assert!(
        conf(&app1).room_manager.has_room(&rid),
        "the call ended though no one reached the pod serving it"
    );

    for _ in 0..2 {
        roomler_ai_mod_conference::media_cluster::end_calls_of_rooms_gone(conf(&app1)).await;
    }
    assert_deleted_rooms_call_ended(
        &app1,
        &app2,
        &room_id,
        call_id,
        &mut host,
        &mut member,
        &conn,
    )
    .await;
    host.close(None).await.ok();
    member.close(None).await.ok();
}

/// A deleted room's call whose pod answered the close too late, its claim
/// already gone with the media (it closed the call and answered after the
/// deadline, or its record ran out with it): nothing is left to close, so
/// ending what the room held is no failure. The claim here names a pod that
/// never answers and runs out while the close waits for it.
#[tokio::test]
async fn a_close_answered_too_late_after_its_claim_went_is_no_failure() {
    use roomler_ai_api::cluster::bus::RPC_DEADLINE;
    let app = TestApp::spawn().await;
    let (Some(dir), Some(_)) = (
        app.state.cluster_directory.clone(),
        app.state.cluster_bus.clone(),
    ) else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let room = bson::oid::ObjectId::new();
    let key = roomler_ai_api::cluster::directory::media_key(&room.to_hex());
    let late = format!("late-{}", bson::oid::ObjectId::new().to_hex());
    let raw = claim_record_of(&late);
    // It runs out while the close waits for its pod: half the bus deadline
    // after it was set, so about a second after the close's first read finds
    // it and about a second before the second read looks again.
    assert_eq!(
        dir.claim_nx(&key, &raw, RPC_DEADLINE.as_secs() / 2)
            .await
            .unwrap(),
        ClaimOutcome::Won
    );

    let started = std::time::Instant::now();
    let ended = app
        .state
        .core
        .hooks
        .room_removed(bson::oid::ObjectId::new(), room)
        .await;
    let took = started.elapsed();
    assert!(
        took >= RPC_DEADLINE,
        "the close did not wait for the pod the claim named: {took:?}"
    );
    ended.expect("a call whose claim went with its media ended");
    assert_eq!(dir.get(&key).await.unwrap(), None);
}

// ── C-5: DERP directory + registration-driven rehome ────────────────

/// A DERP network split across pods converges toward the NEWEST
/// registration: the parked socket on the losing pod is closed by the
/// `derp.rehome` RPC, its record is compare-DEL-released on teardown,
/// and the surviving registration keeps its record.
#[tokio::test]
async fn derp_split_rehomes_toward_newest_registration() {
    use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
    use futures::{SinkExt, StreamExt};
    use roomler_ai_api::cluster::directory::derp_key;
    use roomler_ai_mod_network::derp_cluster::pk_hex;
    use roomler_ai_remote_control::models::NodeRef;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    let Some(d1) = app1.state.cluster_directory.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let tenant = app1.seed_tenant("derpc5").await;
    let tid = bson::oid::ObjectId::parse_str(&tenant.tenant_id).unwrap();

    let (aid_a, tok_a) = crate::agent_presence_tests::enroll(&app1, &tenant, "derp-mach-a").await;
    let (aid_b, tok_b) = crate::agent_presence_tests::enroll(&app1, &tenant, "derp-mach-b").await;

    // Overlay network + node rows (what rc:overlay.join would create) —
    // /derp registration resolves the agent's node and validates its
    // wg_public_key against the registration frame.
    let networks = roomler_ai_services::dao::overlay_network::OverlayNetworkDao::new(&app1.db);
    let nodes = roomler_ai_services::dao::overlay_node::OverlayNodeDao::new(&app1.db);
    let network_id = networks.get_or_create(tid).await.unwrap().id.unwrap();
    let pk_a: [u8; 32] = [0xa1; 32];
    let pk_b: [u8; 32] = [0xb2; 32];
    // ⚠️ `machine_id` MUST be the id the agent enrolled with. `/derp`
    // resolves the socket's node via `current_node` →
    // agent_id → `agent.machine_id` → node by {tenant_id, machine_id}; a node
    // row keyed to any other machine is invisible to it, the registration is
    // refused with only an INFO line, and the directory record never appears.
    // Production cannot produce that mismatch (`rc:overlay.join` writes the
    // agent's own machine id), so it is purely a fixture hazard — and this
    // fixture had `mach-{name}` against agents enrolled as `derp-mach-{a,b}`.
    for (aid, machine, pk, ip, name) in [
        (&aid_a, "derp-mach-a", pk_a, "100.64.0.1", "derp-node-a"),
        (&aid_b, "derp-mach-b", pk_b, "100.64.0.2", "derp-node-b"),
    ] {
        nodes
            .create(roomler_ai_services::dao::overlay_node::NewOverlayNode {
                tenant_id: tid,
                node_ref: NodeRef::Agent {
                    agent_id: bson::oid::ObjectId::parse_str(aid).unwrap(),
                },
                network_id,
                machine_id: machine.to_string(),
                name: name.to_string(),
                overlay_ip: ip.to_string(),
                wg_public_key: B64.encode(pk),
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
    }

    // A registers on pod1 (the socket that will end up parked).
    let (mut ws_a, _) = connect_async(&format!("ws://{}/derp?token={}", app1.addr, tok_a))
        .await
        .expect("derp WS A");
    ws_a.send(Message::Binary(pk_a.to_vec().into()))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let key_a = derp_key(&network_id.to_hex(), &pk_hex(&pk_a));
    let raw = d1.get(&key_a).await.unwrap().expect("A's derp record");
    assert_eq!(
        OwnerRecord::parse(&raw).unwrap().pod_id,
        app1.state.pod.pod_id
    );

    // B registers on pod2 — the NEWEST registration makes pod2 the
    // convergence target; pod1 is told to close A.
    let (mut ws_b, _) = connect_async(&format!("ws://{}/derp?token={}", app2.addr, tok_b))
        .await
        .expect("derp WS B");
    ws_b.send(Message::Binary(pk_b.to_vec().into()))
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut closed = false;
    while tokio::time::Instant::now() < deadline && !closed {
        match tokio::time::timeout(std::time::Duration::from_millis(500), ws_a.next()).await {
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => closed = true,
            Ok(Some(Err(_))) => closed = true,
            _ => {}
        }
    }
    assert!(closed, "parked derp socket must be rehome-closed");

    // A's teardown compare-DELs its record (not just TTL expiry).
    let mut released = false;
    for _ in 0..30 {
        if d1.get(&key_a).await.unwrap().is_none() {
            released = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(released, "rehome-closed socket must release its record");

    // B's record survives, attributed to pod2.
    let key_b = derp_key(&network_id.to_hex(), &pk_hex(&pk_b));
    let raw = d1.get(&key_b).await.unwrap().expect("B's derp record");
    assert_eq!(
        OwnerRecord::parse(&raw).unwrap().pod_id,
        app2.state.pod.pod_id
    );

    ws_b.close(None).await.ok();
}

// ── C-6: shutdown sweep for all classes + metrics + status route ────

/// The status route is auth-gated and reports this pod's identity,
/// cluster health, the rehome/fallback counters and live gauges — to a
/// platform admin (`cluster_status_is_for_platform_admins_only`).
#[tokio::test]
async fn cluster_status_reports_pod_counters_and_gauges() {
    let operator = bson::oid::ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(operator.to_hex());
    })
    .await;
    let _tenant = app.seed_tenant("clstat").await;
    let op = app
        .state
        .auth
        .generate_tokens(operator, "operator@clstat.test", "clstat_operator")
        .unwrap();

    // Unauthenticated → 401.
    //
    // ⚠️ Must NOT reuse `app.client`: it is built with `.cookie_store(true)`,
    // and `seed_tenant` above registers/logs in, so the jar is holding the
    // `access_token` cookie that `/api/auth/{register,login}` set. `AuthUser`
    // accepts a cookie as well as a bearer header, so this "anonymous" request
    // was quietly authenticated and came back 200 — the test was asserting
    // against a request it never actually made. A fresh jarless client is the
    // only way to probe the anonymous path.
    let anon = reqwest::Client::builder()
        .cookie_store(false)
        .build()
        .unwrap();
    let resp = anon
        .get(app.url("/api/cluster/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        401,
        "cluster status must refuse an anonymous caller"
    );

    let body: serde_json::Value = app
        .auth_get("/api/cluster/status", &op.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["pod"]["pod_id"].as_str(),
        Some(app.state.pod.pod_id.as_str())
    );
    assert_eq!(
        body["pod"]["epoch"].as_str(),
        Some(app.state.pod.epoch.as_str())
    );
    for counter in [
        "rc_rehome_total",
        "tunnel_rehome_total",
        "agent_nudge_total",
        "bus_deadline_total",
        "media_fold_total",
        "media_belt_fallback_total",
        "media_frames_rate_limited_total",
        "derp_rehome_close_total",
        "derp_rehome_stuck_total",
        "split_evidence_total",
        "rc_rehome_controller_total",
        "agent_nudge_refused_total",
        "agent_nudge_stuck_total",
        "rc_relay_total",
    ] {
        assert!(
            body["counters"][counter].is_u64(),
            "missing counter {counter}: {body}"
        );
    }
    for gauge in [
        "agents_online",
        "tunnel_sessions",
        "derp_registrations",
        "media_rooms",
        "media_participants",
        "media_consumers",
    ] {
        assert!(
            body["local"][gauge].is_u64(),
            "missing gauge {gauge}: {body}"
        );
    }
    if app.state.cluster_directory.is_some() {
        assert!(
            body["cluster"]["pods_alive"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            "own pod-alive record must be visible: {body}"
        );
    }
}

/// The status route is a platform operator's view: it names every live
/// meeting room on the pod, from every org, and the pods' addresses. A
/// signed-in account that is not a platform admin gets the 404 every
/// platform-admin route gives, whatever org it belongs to or runs.
#[tokio::test]
async fn cluster_status_is_for_platform_admins_only() {
    let operator = bson::oid::ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(operator.to_hex());
    })
    .await;
    let tenant = app.seed_tenant("clgate").await;
    let room_id = tenant.rooms[0].id.clone();
    // A live meeting room on this pod.
    let start = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/call/start",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(start.status().as_u16(), 200);
    let outsider = app.seed_tenant("clgate-out").await;

    for (who, token) in [
        ("another org's admin", &outsider.admin.access_token),
        ("the meeting org's admin", &tenant.admin.access_token),
        ("the meeting org's member", &tenant.member.access_token),
    ] {
        let resp = app
            .auth_get("/api/cluster/status", token)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        assert_eq!(status, 404, "{who} read the cluster status: {body}");
        assert!(
            !body.contains(&room_id),
            "{who} learned a live room id: {body}"
        );
    }

    let op = app
        .state
        .auth
        .generate_tokens(operator, "operator@clgate.test", "clgate_operator")
        .unwrap();
    let body: serde_json::Value = app
        .auth_get("/api/cluster/status", &op.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["pod"]["pod_id"].as_str(),
        Some(app.state.pod.pod_id.as_str())
    );
    let rooms = body["local"]["media_rooms_detail"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        rooms.iter().any(|r| r["room_id"] == room_id.as_str()),
        "a platform admin sees the live room: {body}"
    );
}

/// Graceful shutdown releases tunnel + derp directory records (zero
/// ownerless window on deploys — TTL is only the crash backstop).
#[tokio::test]
async fn shutdown_releases_tunnel_and_derp_records() {
    use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
    use futures::SinkExt;
    use roomler_ai_api::cluster::directory::{derp_key, tunnel_key};
    use roomler_ai_mod_network::derp_cluster::pk_hex;
    use roomler_ai_remote_control::models::NodeRef;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    let app = TestApp::spawn().await;
    let Some(dir) = app.state.cluster_directory.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let tenant = app.seed_tenant("clshut").await;
    let tid = bson::oid::ObjectId::parse_str(&tenant.tenant_id).unwrap();

    // A live derp registration (real socket over a seeded overlay node).
    let (aid, tok) = crate::agent_presence_tests::enroll(&app, &tenant, "shut-mach").await;
    let networks = roomler_ai_services::dao::overlay_network::OverlayNetworkDao::new(&app.db);
    let nodes = roomler_ai_services::dao::overlay_node::OverlayNodeDao::new(&app.db);
    let network_id = networks.get_or_create(tid).await.unwrap().id.unwrap();
    let pk: [u8; 32] = [0xc3; 32];
    nodes
        .create(roomler_ai_services::dao::overlay_node::NewOverlayNode {
            tenant_id: tid,
            node_ref: NodeRef::Agent {
                agent_id: bson::oid::ObjectId::parse_str(&aid).unwrap(),
            },
            network_id,
            // Must match the enrolled machine id — see the field doc on
            // `NewOverlayNode::machine_id`. This said "mach-shut" against an
            // agent enrolled as "shut-mach", which is exactly the
            // transposition the named fields now make hard to write.
            machine_id: "shut-mach".into(),
            name: "shut-node".into(),
            overlay_ip: "100.64.0.9".into(),
            wg_public_key: B64.encode(pk),
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
    let (mut ws, _) = connect_async(&format!("ws://{}/derp?token={}", app.addr, tok))
        .await
        .expect("derp WS");
    ws.send(Message::Binary(pk.to_vec().into())).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let dkey = derp_key(&network_id.to_hex(), &pk_hex(&pk));
    assert!(dir.get(&dkey).await.unwrap().is_some(), "derp record live");

    // A tunnel session record (write path locked by the C-3 tests —
    // here we only need a held token for the sweep to release).
    let sid = bson::oid::ObjectId::new();
    let ttoken = dir.owner_token("conn-x");
    dir.claim_lww(&tunnel_key(&sid.to_hex()), &ttoken)
        .await
        .unwrap();
    app.state
        .network()
        .tunnel_presence_tokens
        .insert(sid, ttoken);

    roomler_ai_api::state::shutdown_cleanup(&app.state).await;

    assert_eq!(
        dir.get(&dkey).await.unwrap(),
        None,
        "shutdown must release the derp record"
    );
    assert_eq!(
        dir.get(&tunnel_key(&sid.to_hex())).await.unwrap(),
        None,
        "shutdown must release the tunnel record"
    );
    ws.close(None).await.ok();
}

/// #1186 — a removal initiated on a pod that homes NO sockets must still
/// reach the peers' pod: the `overlay_removes` ctrl broadcast re-fans on
/// every other pod. Field shape (2026-09-01): both members' WSs on the
/// tenant-affine pod, the FR-51 self-unenroll (tenant-less path, IP-hashed)
/// landing on the other — the peer never heard the removes and kept
/// WG-handshaking at a dead peer until its next rejoin.
///
/// Here the removal runs through the admin DELETE aimed at app2 on purpose:
/// in prod that route is tenant-affine by the LB, but the test drives it at
/// the WRONG pod to stand in for every non-affine initiator (self-unenroll,
/// the reaper) — they all share `release_overlay_node`, which is where the
/// broadcast lives. FAILED before the fix (10 s timeout, no delta).
#[tokio::test]
async fn overlay_removes_reach_peers_homed_on_another_pod() {
    use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
    use bson::oid::ObjectId;
    use futures::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio_tungstenite::tungstenite::Message;
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.redis_pubsub.is_none() || app2.state.redis_pubsub.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let seeded = app1.seed_tenant("ovremoves").await;

    // Two members, both sockets on app1 (the "affine" pod).
    let (aid_a, tok_a) = crate::agent_presence_tests::enroll(&app1, &seeded, "ovrm-a").await;
    let (_aid_b, tok_b) = crate::agent_presence_tests::enroll(&app1, &seeded, "ovrm-b").await;
    let mut ws_a = crate::agent_presence_tests::connect_agent(&app1, &tok_a).await;
    let mut ws_b = crate::agent_presence_tests::connect_agent(&app1, &tok_b).await;

    // Join A, receive A's netmap, THEN join B — strictly sequential per
    // socket, the overlay_growth_tests pattern. Sending both joins at once
    // raced the FR-47 carve-on-first-join (both are the tenant's *first*
    // join until one commits the network), and the loser errored — which is
    // why the first two CI runs failed nondeterministically on DIFFERENT
    // members (A's node absent, then B's netmap absent).
    async fn join_and_await_netmap(
        ws: &mut (
                 impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
                 + SinkExt<Message>
                 + Unpin
             ),
        seed: u8,
        who: &str,
    ) {
        let msg = serde_json::json!({
            "t": "rc:overlay.join",
            "wg_public_key": B64.encode([seed; 32]),
            "mtu": 1280,
            "endpoints": ["203.0.113.9:51820"],
        })
        .to_string();
        let _ = ws.send(Message::Text(msg.into())).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await {
                // Match the full netmap, not a `_delta`: the closing quote
                // after the type name distinguishes `"rc:overlay.netmap"`
                // from `"rc:overlay.netmap_delta"`.
                Ok(Some(Ok(Message::Text(t)))) if t.contains("\"rc:overlay.netmap\"") => return,
                Ok(Some(Ok(_))) => {}
                _ => break,
            }
        }
        panic!("{who} must receive a netmap for its join");
    }
    join_and_await_netmap(&mut ws_a, 11, "A").await;
    join_and_await_netmap(&mut ws_b, 12, "B").await;

    let a_oid = ObjectId::parse_str(&aid_a).unwrap();
    let tid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let node_a = app1
        .state
        .network()
        .overlay_nodes
        .find_live_by_tenant_and_machine(tid, "ovrm-a")
        .await
        .unwrap()
        .expect("A holds a live overlay node");
    let node_a_hex = node_a.id.unwrap().to_hex();

    // The removal, initiated on the pod that homes NEITHER socket.
    let del: Value = app2
        .auth_delete(
            &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, a_oid.to_hex()),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(del["deleted"], serde_json::json!(true));
    assert_eq!(
        del["overlay_released"],
        serde_json::json!(true),
        "the lease release itself is pod-agnostic (Mongo) and must succeed"
    );

    // B — whose socket lives on app1 — must hear the removes anyway.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut heard = false;
    while std::time::Instant::now() < deadline && !heard {
        match tokio::time::timeout(std::time::Duration::from_secs(2), ws_b.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if t.contains("rc:overlay.netmap_delta") && t.contains(&node_a_hex) {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if v["removes"]
                        .as_array()
                        .is_some_and(|r| r.iter().any(|x| x == &Value::from(node_a_hex.clone())))
                    {
                        heard = true;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => break,
        }
    }
    assert!(
        heard,
        "the peer's pod must re-fan a foreign-pod removal (#1186) — \
         before the fix this timed out exactly like the field run"
    );
    ws_a.close(None).await.ok();
    ws_b.close(None).await.ok();
}

// ---------------------------------------------------------------------------
// A seat whose membership ended while the pod serving its call could not be
// reached (#3839). The withdrawal of `revoke` could not reach that pod, so
// no one closed the seat; the pod serving the call finds the seat's admission
// gone in its own check and ends the seat itself.
// ---------------------------------------------------------------------------

/// How long these tests wait for the pod serving a call to end a seat whose
/// admission is gone once no withdrawal reached it: two passes of its seat
/// check, ten seconds apart, each a read and the room guard's reads, each
/// bounded at five seconds (40 s), and slack for a loaded runner.
const SEAT_CHECK_WAIT: std::time::Duration = std::time::Duration::from_secs(45);

/// The pod serving `room_id`'s call (app1) can no longer be reached by the
/// other: the call's claim names a pod no one runs, which app1 holds as its
/// own, so its claim refresh neither folds the call nor gives the record up
/// (`a_room_deleted_while_the_pod_serving_its_call_cannot_be_reached_ends_it_afterwards`
/// does the same, and says what the two writes leave open).
async fn cut_off_the_pod_serving(app1: &TestApp, room_id: &str) {
    let dir = app1
        .state
        .cluster_directory
        .clone()
        .expect("a directory: the caller skips without Redis");
    let rid = bson::oid::ObjectId::parse_str(room_id).unwrap();
    let key = roomler_ai_api::cluster::directory::media_key(room_id);
    let unreachable = format!("gone-{}", bson::oid::ObjectId::new().to_hex());
    let raw = claim_record_of(&unreachable);
    dir.claim_lww(&key, &raw).await.unwrap();
    conf(app1).media_claim_tokens.insert(rid, raw);
}

/// The next frame of type `kind` within `within`, or `None` once it passed.
pub(crate) async fn frame_of_within(
    ws: &mut UserWs,
    kind: &str,
    within: std::time::Duration,
) -> Option<serde_json::Value> {
    use futures::StreamExt;
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let msg = match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(other) => panic!("socket ended waiting for {kind}: {other:?}"),
            Err(_) => return None,
        };
        if let Ok(frame) = serde_json::from_str::<serde_json::Value>(msg.to_text().unwrap_or(""))
            && frame["type"] == kind
        {
            return Some(frame);
        }
    }
}

/// The membership ended and no word of it reached the pod serving the call:
/// the member's socket heard no end of its call, and app1 still seats it.
async fn assert_seat_still_held(
    app1: &TestApp,
    room_id: &str,
    member: &mut UserWs,
    conn: &str,
    what: &str,
) {
    let rid = bson::oid::ObjectId::parse_str(room_id).unwrap();
    let heard: Vec<serde_json::Value> = frames_within(member, 500)
        .await
        .into_iter()
        .filter(|f| f["type"] == "room:call_ended")
        .collect();
    assert!(
        heard.is_empty(),
        "{what}: the member heard the call end though no one reached the pod serving it: {heard:?}"
    );
    assert_eq!(
        conf(app1).room_manager.get_connection_room(conn),
        Some(rid),
        "{what}: the seat went though no one reached the pod serving it"
    );
}

/// The room's `participant_count`, whichever integer width it was stored as.
fn participant_count_of(room: &bson::Document) -> Option<i64> {
    match room.get("participant_count") {
        Some(bson::Bson::Int32(n)) => Some(i64::from(*n)),
        Some(bson::Bson::Int64(n)) => Some(*n),
        _ => None,
    }
}

/// The pod serving the call (app1) ended the seat by itself, the way a
/// withdrawal that reached it ends one: the member's socket (on app2) hears
/// its call ended with `reason: "not_a_member"`, the host (on app1) hears it
/// leave, app1 seats it no more, and the call goes on for the host, counted
/// once.
async fn assert_seat_ended_by_the_pod_serving_it(
    app1: &TestApp,
    room_id: &str,
    host: &mut UserWs,
    member: &mut UserWs,
    conn: &str,
    what: &str,
) {
    let ended = frame_of_within(member, "room:call_ended", SEAT_CHECK_WAIT)
        .await
        .unwrap_or_else(|| {
            panic!(
                "{what}: the seat outlived its membership: no room:call_ended within \
                 {SEAT_CHECK_WAIT:?} of the membership's end"
            )
        });
    assert_eq!(ended["data"]["room_id"], room_id, "{ended}");
    assert_eq!(ended["data"]["reason"], "not_a_member", "{ended}");
    let left = next_frame_of(host, "media:peer_left").await;
    assert_eq!(left["data"]["connection_id"], conn, "{left}");
    assert_eq!(
        conf(app1).room_manager.get_connection_room(conn),
        None,
        "{what}: the pod serving the call still seats the member"
    );
    let rid = bson::oid::ObjectId::parse_str(room_id).unwrap();
    let room = app1
        .db
        .collection::<bson::Document>("rooms")
        .find_one(bson::doc! { "_id": rid })
        .await
        .unwrap()
        .expect("the room exists");
    assert_eq!(
        room.get_str("conference_status").ok(),
        Some("in_progress"),
        "{what}: the host is still in the call: {room:?}"
    );
    assert_eq!(
        participant_count_of(&room),
        Some(1),
        "{what}: the call counts the host alone: {room:?}"
    );
}

/// The `room_members` row of `user_id` in `room_id`, if any.
async fn room_row_of(app: &TestApp, room_id: &str, user_id: &str) -> Option<bson::Document> {
    app.db
        .collection::<bson::Document>("room_members")
        .find_one(bson::doc! {
            "room_id": bson::oid::ObjectId::parse_str(room_id).unwrap(),
            "user_id": bson::oid::ObjectId::parse_str(user_id).unwrap(),
        })
        .await
        .unwrap()
}

/// A member removed from the org while the pod serving their call cannot be
/// reached keeps their seat only until that pod checks its seats: it finds
/// them no longer a member of the room's org, twice, a pass apart, and ends
/// the seat. The removal itself answers, and every hook after conference's
/// runs (chat's ends the person's room memberships).
#[tokio::test]
async fn an_org_removal_the_pod_serving_the_call_did_not_hear_ends_the_seat_there() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let (tenant, room_id, mut host, mut member, conn) = call_across_pods(&app1, &app2).await;
    cut_off_the_pod_serving(&app1, &room_id).await;

    let resp = app2
        .auth_delete(
            &format!(
                "/api/tenant/{}/member/{}",
                tenant.tenant_id, tenant.member.id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        room_row_of(&app1, &room_id, &tenant.member.id)
            .await
            .is_none(),
        "chat's hook did not run after conference's: the person's room membership stayed"
    );
    assert_seat_still_held(&app1, &room_id, &mut member, &conn, "org removal").await;

    assert_seat_ended_by_the_pod_serving_it(
        &app1,
        &room_id,
        &mut host,
        &mut member,
        &conn,
        "org removal",
    )
    .await;
    host.close(None).await.ok();
    member.close(None).await.ok();
}

/// A member who leaves a room only its members may enter, while the pod
/// serving its call cannot be reached, keeps their seat only until that pod
/// checks its seats: the room's visibility wants a membership they no longer
/// hold, twice, a pass apart, and the pod ends the seat.
#[tokio::test]
async fn a_private_room_leave_the_pod_serving_the_call_did_not_hear_ends_the_seat_there() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_directory.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let (tenant, room_id, mut host, mut member, conn) = call_across_pods(&app1, &app2).await;
    // Only its members may enter the room from now on; both are members.
    let rid = bson::oid::ObjectId::parse_str(&room_id).unwrap();
    app1.db
        .collection::<bson::Document>("rooms")
        .update_one(
            bson::doc! { "_id": rid },
            bson::doc! { "$set": { "visibility": "private" } },
        )
        .await
        .unwrap();
    cut_off_the_pod_serving(&app1, &room_id).await;

    let resp = app2
        .auth_post(
            &format!("/api/tenant/{}/room/{}/leave", tenant.tenant_id, room_id),
            &tenant.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        room_row_of(&app1, &room_id, &tenant.member.id)
            .await
            .is_none(),
        "the leave did not end the room membership"
    );
    assert_seat_still_held(&app1, &room_id, &mut member, &conn, "private room leave").await;

    assert_seat_ended_by_the_pod_serving_it(
        &app1,
        &room_id,
        &mut host,
        &mut member,
        &conn,
        "private room leave",
    )
    .await;
    host.close(None).await.ok();
    member.close(None).await.ok();
}

// ---------------------------------------------------------------------------
// What the seat check must never end (#3839): its positive controls. The
// check closes a seat only on a positive reading, twice, that its admission
// is gone; everything else keeps the seat. These drive it directly, one pass
// at a time, with one memory between passes as its loop keeps one. Its own
// loop runs beside them on every pod, so a control asserts what a pass
// ends, never that only its own passes ran.
// ---------------------------------------------------------------------------

pub(crate) fn oid_of(hex: &str) -> bson::oid::ObjectId {
    bson::oid::ObjectId::parse_str(hex).unwrap()
}

/// A new room in `tenant`, made by its admin, with a call started in it.
pub(crate) async fn start_call_in_new_room(
    app: &TestApp,
    tenant: &crate::fixtures::seed::SeededTenant,
    name: &str,
) -> String {
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/room", tenant.tenant_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "room create: {}", resp.status());
    let room: serde_json::Value = resp.json().await.unwrap();
    let room_id = room["id"].as_str().unwrap().to_string();
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/call/start",
                tenant.tenant_id, room_id
            ),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    room_id
}

/// `token`'s account joins `room_id`'s call (`call/join`, then `media:join`
/// on a socket of its own) and is seated on `app`. Returns the socket and
/// its connection.
pub(crate) async fn seat_in_call(
    app: &TestApp,
    tenant_id: &str,
    room_id: &str,
    token: &str,
) -> (UserWs, String) {
    join_call(app, tenant_id, room_id, token).await;
    let (mut ws, conn) = connect_user_ws_conn(app, token).await;
    send_media(&mut ws, "media:join", room_id).await;
    assert_eq!(
        next_media_msg(&mut ws).await["type"],
        "media:router_capabilities"
    );
    assert_eq!(
        next_media_msg(&mut ws).await["type"],
        "media:transport_created"
    );
    assert_eq!(
        conf(app).room_manager.get_connection_room(&conn),
        Some(oid_of(room_id)),
        "seated in {room_id}"
    );
    (ws, conn)
}

/// Set the room's visibility as its document stores it.
pub(crate) async fn set_visibility(app: &TestApp, room_id: &str, visibility: &str) {
    app.db
        .collection::<bson::Document>("rooms")
        .update_one(
            bson::doc! { "_id": oid_of(room_id) },
            bson::doc! { "$set": { "visibility": visibility } },
        )
        .await
        .unwrap();
}

/// Remove `user_id`'s `room_members` row in `room_id` by a raw write, the way
/// a path that tells no hook would: nothing reaches the seat but the check.
pub(crate) async fn delete_room_row(app: &TestApp, room_id: &str, user_id: &str) {
    let gone = app
        .db
        .collection::<bson::Document>("room_members")
        .delete_many(bson::doc! { "room_id": oid_of(room_id), "user_id": oid_of(user_id) })
        .await
        .unwrap();
    assert_eq!(
        gone.deleted_count, 1,
        "one room row of {user_id} in {room_id}"
    );
}

/// Remove `user_id`'s `tenant_members` row in `tenant_id` by a raw write.
pub(crate) async fn delete_org_row(app: &TestApp, tenant_id: &str, user_id: &str) {
    let gone = app
        .db
        .collection::<bson::Document>("tenant_members")
        .delete_many(bson::doc! { "tenant_id": oid_of(tenant_id), "user_id": oid_of(user_id) })
        .await
        .unwrap();
    assert_eq!(
        gone.deleted_count, 1,
        "one org row of {user_id} in {tenant_id}"
    );
}

/// `n` passes of the seat check, each ending nothing.
pub(crate) async fn passes_end_nothing(
    app: &TestApp,
    memory: &mut roomler_ai_mod_conference::seat_check::SeatCheckMemory,
    n: usize,
    what: &str,
) {
    for pass in 1..=n {
        let ended =
            roomler_ai_mod_conference::seat_check::check_seats_once(conf(app), memory).await;
        assert!(ended.is_empty(), "{what}: pass {pass} ended {ended:?}");
    }
}

/// No `room:call_ended` for `room_id` reached `ws` within half a second. (A
/// call that ends tells every socket of its room's members, whatever room
/// each sits in, so a socket is asked about one room.)
pub(crate) async fn heard_no_call_end(ws: &mut UserWs, room_id: &str, what: &str) {
    let heard: Vec<serde_json::Value> = frames_within(ws, 500)
        .await
        .into_iter()
        .filter(|f| f["type"] == "room:call_ended" && f["data"]["room_id"] == room_id)
        .collect();
    assert!(heard.is_empty(), "{what} heard its call end: {heard:?}");
}

/// The seat of `conn` is still held in `room_id`.
pub(crate) fn assert_seated(app: &TestApp, conn: &str, room_id: &str, what: &str) {
    assert_eq!(
        conf(app).room_manager.get_connection_room(conn),
        Some(oid_of(room_id)),
        "{what}: the seat went"
    );
}

/// The seat check's reading agrees with the room guard `media:join` runs, in
/// every cell: each room visibility, with and without a room row, for a
/// member of the org and for someone who is not one, for a plain member and
/// for the org's administrator. Wherever the guard admits, the check must read
/// `Admitted`, or it would end a seat the guard would give back at once; a
/// change to the guard (a role that may enter without a row, say) breaks this
/// test before it ends anyone's seat. Both answers are held against one table,
/// `expected`, whose `match` names every visibility with no wildcard: a new
/// visibility does not compile here until someone decides what a seat in such
/// a room reads.
#[tokio::test]
async fn the_seat_checks_reading_agrees_with_the_room_guard_in_every_cell() {
    use roomler_ai_db::models::RoomVisibility;
    use roomler_ai_mod_conference::seat_check::{Gone, Reading, read_readings};

    /// The reading a seat must get, cell by cell. Adding a visibility means
    /// an arm here and an entry in `visibilities` below.
    fn expected(visibility: RoomVisibility, in_org: bool, with_row: bool) -> Reading {
        let needs_a_room_row = match visibility {
            RoomVisibility::Public => false,
            RoomVisibility::Private | RoomVisibility::Secret => true,
        };
        if !in_org {
            Reading::Gone(Gone::NotInOrg)
        } else if needs_a_room_row && !with_row {
            Reading::Gone(Gone::NotInRoom)
        } else {
            Reading::Admitted
        }
    }

    let app = TestApp::spawn().await;
    let state = conf(&app);
    let tenant = app.seed_tenant("seatmatrix").await;
    let tid = oid_of(&tenant.tenant_id);
    let people = [
        ("member", tenant.member.id.clone()),
        ("administrator", tenant.admin.id.clone()),
    ];
    let visibilities = [
        RoomVisibility::Public,
        RoomVisibility::Private,
        RoomVisibility::Secret,
    ];

    // One room for each visibility and row: (visibility, with a row, room).
    let mut rooms = Vec::new();
    for visibility in visibilities {
        for with_row in [true, false] {
            let resp = app
                .auth_post(
                    &format!("/api/tenant/{}/room", tenant.tenant_id),
                    &tenant.admin.access_token,
                )
                .json(&serde_json::json!({ "name": format!("{visibility:?} {with_row}") }))
                .send()
                .await
                .unwrap();
            assert!(resp.status().is_success(), "room create: {}", resp.status());
            let room: serde_json::Value = resp.json().await.unwrap();
            let room_id = room["id"].as_str().unwrap().to_string();
            let stored = bson::to_bson(&visibility).unwrap();
            let stored = stored.as_str().unwrap().to_string();
            set_visibility(&app, &room_id, &stored).await;
            for (_, person) in &people {
                // Exactly the row this cell wants, through the DAO that writes it.
                app.db
                    .collection::<bson::Document>("room_members")
                    .delete_many(
                        bson::doc! { "room_id": oid_of(&room_id), "user_id": oid_of(person) },
                    )
                    .await
                    .unwrap();
                if with_row {
                    state
                        .rooms
                        .join(tid, oid_of(&room_id), oid_of(person))
                        .await
                        .unwrap();
                }
            }
            rooms.push((visibility, with_row, room_id));
        }
    }

    let mut cells = 0;
    for in_org in [true, false] {
        if !in_org {
            for (_, person) in &people {
                delete_org_row(&app, &tenant.tenant_id, person).await;
            }
        }
        let seated: Vec<(bson::oid::ObjectId, bson::oid::ObjectId)> = rooms
            .iter()
            .flat_map(|(_, _, room)| people.iter().map(move |(_, p)| (oid_of(room), oid_of(p))))
            .collect();
        let readings = read_readings(state, &seated)
            .await
            .expect("a complete read");
        for (visibility, with_row, room_id) in &rooms {
            for (who, person) in &people {
                let cell = format!(
                    "{who}, {visibility:?} room, room row: {with_row}, in the org: {in_org}"
                );
                let admitted = match roomler_ai_mod_conference::guards::require_room_in_tenant(
                    state,
                    tid,
                    oid_of(room_id),
                    oid_of(person),
                )
                .await
                {
                    Ok(_) => true,
                    Err(
                        roomler_core::ApiError::NotAMember
                        | roomler_core::ApiError::Forbidden(_)
                        | roomler_core::ApiError::NotFound(_),
                    ) => false,
                    Err(other) => panic!("{cell}: the guard could not answer: {other:?}"),
                };
                let reading = readings
                    .get(&(oid_of(room_id), oid_of(person)))
                    .copied()
                    .unwrap_or_else(|| panic!("{cell}: no reading"));
                let want = expected(*visibility, in_org, *with_row);
                assert_eq!(
                    admitted,
                    want == Reading::Admitted,
                    "{cell}: the room guard and the table disagree (the table reads {want:?})"
                );
                assert_eq!(
                    reading, want,
                    "{cell}: the seat check reads {reading:?} where the guard's table reads {want:?}"
                );
                cells += 1;
            }
        }
    }
    assert_eq!(cells, 24, "every cell was read");
}

/// A member the room guard admits keeps their seat however often the pod
/// serving the call checks it: in a room only its members may enter, holding
/// their room membership, three passes end nothing and nobody hears a thing.
#[tokio::test]
async fn an_entitled_member_keeps_the_seat_across_seat_checks() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("seatkeep").await;
    let room_id = start_call_in_new_room(&app, &tenant, "Keep").await;
    let (mut host, host_conn) = seat_in_call(
        &app,
        &tenant.tenant_id,
        &room_id,
        &tenant.admin.access_token,
    )
    .await;
    let (mut member, conn) = seat_in_call(
        &app,
        &tenant.tenant_id,
        &room_id,
        &tenant.member.access_token,
    )
    .await;
    set_visibility(&app, &room_id, "private").await;
    assert!(
        room_row_of(&app, &room_id, &tenant.member.id)
            .await
            .is_some(),
        "the member holds the room's membership"
    );

    let mut memory = roomler_ai_mod_conference::seat_check::SeatCheckMemory::default();
    passes_end_nothing(&app, &mut memory, 3, "an entitled member").await;
    heard_no_call_end(&mut member, &room_id, "the member").await;
    heard_no_call_end(&mut host, &room_id, "the host").await;
    assert_seated(&app, &conn, &room_id, "the member");
    assert_seated(&app, &host_conn, &room_id, "the host");
    host.close(None).await.ok();
    member.close(None).await.ok();
}

/// A person a room's visibility admits with no room membership at all (any
/// member of the org may enter a public room) keeps their seat: three passes
/// end nothing.
#[tokio::test]
async fn a_person_the_rooms_visibility_admits_without_a_room_row_keeps_the_seat() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("seatpublic").await;
    let room_id = start_call_in_new_room(&app, &tenant, "Public").await;
    let (mut host, _) = seat_in_call(
        &app,
        &tenant.tenant_id,
        &room_id,
        &tenant.admin.access_token,
    )
    .await;
    let (mut member, conn) = seat_in_call(
        &app,
        &tenant.tenant_id,
        &room_id,
        &tenant.member.access_token,
    )
    .await;
    delete_room_row(&app, &room_id, &tenant.member.id).await;

    let mut memory = roomler_ai_mod_conference::seat_check::SeatCheckMemory::default();
    passes_end_nothing(&app, &mut memory, 3, "admitted by the visibility").await;
    heard_no_call_end(&mut member, &room_id, "the member").await;
    assert_seated(&app, &conn, &room_id, "the member");
    host.close(None).await.ok();
    member.close(None).await.ok();
}

/// The check ends exactly the seat whose admission is gone, and only on its
/// second reading of it: the same person's seat in another room of the org,
/// and in a room of another org, stays; a pass after a close ends nothing. The
/// memberships go by raw writes, the way a path that tells no hook would.
#[tokio::test]
async fn the_seat_check_ends_only_the_seat_whose_admission_is_gone_and_once() {
    use roomler_ai_mod_conference::seat_check::{SeatCheckMemory, check_seats_once};
    let app = TestApp::spawn().await;
    let org_a = app.seed_tenant("seatonlya").await;
    let org_b = app.seed_tenant("seatonlyb").await;
    let person = org_a.member.id.clone();
    let token = org_a.member.access_token.clone();

    // The person is a member of org B too.
    let role_b: bson::Document = app
        .db
        .collection::<bson::Document>("roles")
        .find_one(bson::doc! { "tenant_id": oid_of(&org_b.tenant_id), "name": "member" })
        .await
        .unwrap()
        .expect("org B's member role");
    conf(&app)
        .tenants
        .add_member(
            oid_of(&org_b.tenant_id),
            oid_of(&person),
            vec![role_b.get_object_id("_id").unwrap()],
            None,
        )
        .await
        .unwrap();

    // Room A1 (members only, the admin in it too), room A2 (the person
    // alone), and room B1 in org B: the person holds a seat in each.
    let a1 = start_call_in_new_room(&app, &org_a, "A1").await;
    let a2 = start_call_in_new_room(&app, &org_a, "A2").await;
    let b1 = start_call_in_new_room(&app, &org_b, "B1").await;
    let (mut host, _) = seat_in_call(&app, &org_a.tenant_id, &a1, &org_a.admin.access_token).await;
    let (mut in_a1, conn_a1) = seat_in_call(&app, &org_a.tenant_id, &a1, &token).await;
    let (mut in_a2, conn_a2) = seat_in_call(&app, &org_a.tenant_id, &a2, &token).await;
    let (mut in_b1, conn_b1) = seat_in_call(&app, &org_b.tenant_id, &b1, &token).await;
    set_visibility(&app, &a1, "private").await;
    let mut memory = SeatCheckMemory::default();
    passes_end_nothing(&app, &mut memory, 1, "every admission holds").await;

    // The person's room membership of A1 goes: one reading never ends a
    // seat, the second ends that one seat.
    delete_room_row(&app, &a1, &person).await;
    passes_end_nothing(&app, &mut memory, 1, "the first reading of A1's").await;
    assert_seated(&app, &conn_a1, &a1, "A1 after one reading");
    let ended = check_seats_once(conf(&app), &mut memory).await;
    assert_eq!(
        ended,
        vec![(oid_of(&a1), oid_of(&person))],
        "the second reading ends A1's seat"
    );
    let heard = frame_of_within(
        &mut in_a1,
        "room:call_ended",
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("A1's socket hears its call end");
    assert_eq!(heard["data"]["room_id"], a1.as_str(), "{heard}");
    assert_eq!(heard["data"]["reason"], "not_a_member", "{heard}");
    let left = next_frame_of(&mut host, "media:peer_left").await;
    assert_eq!(left["data"]["connection_id"], conn_a1.as_str(), "{left}");
    assert_eq!(conf(&app).room_manager.get_connection_room(&conn_a1), None);
    assert_seated(&app, &conn_a2, &a2, "another room of the org");
    assert_seated(&app, &conn_b1, &b1, "a room of another org");
    heard_no_call_end(&mut in_a2, &a2, "the seat in A2").await;
    heard_no_call_end(&mut in_b1, &b1, "the seat in B1").await;

    // The person's membership of org A goes: A2's seat ends on the second
    // reading, and the call they were the last one in ends with it; B1's
    // seat, in another org, stays.
    delete_org_row(&app, &org_a.tenant_id, &person).await;
    passes_end_nothing(&app, &mut memory, 1, "the first reading of org A's").await;
    assert_seated(&app, &conn_a2, &a2, "A2 after one reading");
    let ended = check_seats_once(conf(&app), &mut memory).await;
    assert_eq!(
        ended,
        vec![(oid_of(&a2), oid_of(&person))],
        "the second reading ends A2's seat"
    );
    let heard = frame_of_within(
        &mut in_a2,
        "room:call_ended",
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("A2's socket hears its call end");
    assert_eq!(heard["data"]["room_id"], a2.as_str(), "{heard}");
    assert_eq!(heard["data"]["reason"], "not_a_member", "{heard}");
    let a2_room = app
        .db
        .collection::<bson::Document>("rooms")
        .find_one(bson::doc! { "_id": oid_of(&a2) })
        .await
        .unwrap()
        .expect("A2 exists");
    assert_ne!(
        a2_room.get_str("conference_status").ok(),
        Some("in_progress"),
        "the call the person was the last one in ended: {a2_room:?}"
    );
    assert_seated(&app, &conn_b1, &b1, "a room of another org");
    heard_no_call_end(&mut in_b1, &b1, "the seat in B1").await;
    // Whatever else the end of A2's call told its sockets is read now.
    frames_within(&mut in_a1, 500).await;
    frames_within(&mut in_a2, 500).await;

    // Passes after the closes end nothing and tell nobody again.
    passes_end_nothing(&app, &mut memory, 2, "after the closes").await;
    heard_no_call_end(&mut in_a1, &a1, "A1's socket again").await;
    heard_no_call_end(&mut in_a2, &a2, "A2's socket again").await;
    heard_no_call_end(&mut in_b1, &b1, "the seat in B1").await;
    assert_seated(&app, &conn_b1, &b1, "a room of another org");
    for ws in [&mut host, &mut in_a1, &mut in_a2, &mut in_b1] {
        ws.close(None).await.ok();
    }
}

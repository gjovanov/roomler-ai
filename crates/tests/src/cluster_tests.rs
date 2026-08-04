//! C-1 — cluster foundation: ownership directory + per-pod bus, exercised
//! across two in-process "pods" sharing one Redis.

use crate::fixtures::test_app::TestApp;
use roomler_ai_api::cluster::directory::{ClaimOutcome, OwnerRecord};

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
    assert!(app1.state.rc_hub.is_agent_online(aid));

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
    app1.state.tunnel_sessions_by_target_agent.remove(&aid);

    // (b) session the agent ORIGINATED (declared routes) - the pre-PR-1
    // blind spot index.
    app1.state
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
    app1.state.tunnel_sessions_by_origin_agent.remove(&aid);

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
        if !app1.state.rc_hub.is_agent_online(aid) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(!app1.state.rc_hub.is_agent_online(aid));

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
    assert!(app1.state.rc_hub.is_agent_online(aid));

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
        if !app1.state.rc_hub.is_agent_online(aid) {
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

type UserWs =
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
async fn next_media_msg(ws: &mut UserWs) -> serde_json::Value {
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

async fn send_media(ws: &mut UserWs, msg_type: &str, room_id: &str) {
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
    assert!(app1.state.room_manager.has_room(&rid));
    assert!(!app2.state.room_manager.has_room(&rid));

    // Member joins via app2 (the NON-owner pod).
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
    assert!(!app2.state.room_manager.has_room(&rid));
    assert!(
        app2.state.remote_media_conns.len() == 1,
        "remote membership tracked for WS-close forwarding"
    );

    // Admin joins locally on the owner, then leaves — the peer_left push
    // must cross pods to the remote viewer's connection.
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

    use futures::SinkExt;
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
        app2.state.room_manager.has_room(&rid),
        "survivor must own the rebuilt room"
    );
    let raw = d2.get(&key).await.unwrap().expect("fresh claim exists");
    let rec = OwnerRecord::parse(&raw).unwrap();
    assert_eq!(rec.pod_id, app2.state.pod.pod_id);

    use futures::SinkExt;
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
    roomler_ai_api::ws::media_cluster::refresh_media_claims_once(&app1.state).await;
    assert!(
        !app1.state.room_manager.has_room(&rid),
        "claim loser must tear down its island"
    );
    let closed = next_media_msg(&mut ws1).await;
    assert_eq!(closed["type"], "media:room_closed", "{closed}");
    assert_eq!(closed["data"]["reason"].as_str(), Some("rehomed"));

    use futures::SinkExt;
    ws1.close(None).await.ok();
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
    use roomler_ai_api::ws::derp_cluster::pk_hex;
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
    for (aid, pk, ip, name) in [
        (&aid_a, pk_a, "100.64.0.1", "derp-node-a"),
        (&aid_b, pk_b, "100.64.0.2", "derp-node-b"),
    ] {
        nodes
            .create(
                tid,
                NodeRef::Agent {
                    agent_id: bson::oid::ObjectId::parse_str(aid).unwrap(),
                },
                network_id,
                format!("mach-{name}"),
                name.to_string(),
                ip.to_string(),
                B64.encode(pk),
                0,
                vec![],
                false,
                false,
                true,
                false,
                vec![],
            )
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
/// cluster health, the rehome/fallback counters and live gauges.
#[tokio::test]
async fn cluster_status_reports_pod_counters_and_gauges() {
    let app = TestApp::spawn().await;
    let tenant = app.seed_tenant("clstat").await;

    // Unauthenticated → 401.
    let resp = app
        .client
        .get(app.url("/api/cluster/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    let body: serde_json::Value = app
        .auth_get("/api/cluster/status", &tenant.admin.access_token)
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

/// Graceful shutdown releases tunnel + derp directory records (zero
/// ownerless window on deploys — TTL is only the crash backstop).
#[tokio::test]
async fn shutdown_releases_tunnel_and_derp_records() {
    use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
    use futures::{SinkExt, StreamExt};
    use roomler_ai_api::cluster::directory::{derp_key, tunnel_key};
    use roomler_ai_api::ws::derp_cluster::pk_hex;
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
        .create(
            tid,
            NodeRef::Agent {
                agent_id: bson::oid::ObjectId::parse_str(&aid).unwrap(),
            },
            network_id,
            "mach-shut".into(),
            "shut-node".into(),
            "100.64.0.9".into(),
            B64.encode(pk),
            0,
            vec![],
            false,
            false,
            true,
            false,
            vec![],
        )
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
    app.state.tunnel_presence_tokens.insert(sid, ttoken);

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

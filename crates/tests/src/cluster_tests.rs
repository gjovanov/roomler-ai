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

/// C-2 — the rehome loop end to end: a controller on pod2 requesting an
/// agent homed on pod1 gets `agent_on_other_pod` (never a lying
/// `agent_offline`), and the owner pod nudges the idle agent's WS closed
/// so its reconnect re-hashes.
#[tokio::test]
async fn rehome_error_and_idle_nudge_cross_pod() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_bus.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    // Bus subscriptions live.
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

    let seeded = app1.seed_tenant("rehome").await;
    let (agent_id, agent_token) =
        crate::agent_presence_tests::enroll(&app1, &seeded, "mach-rehome-a").await;
    let mut agent_ws = crate::agent_presence_tests::connect_agent(&app1, &agent_token).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    // Controller WS on the OTHER pod (user JWT, no role param).
    let ctrl_url = format!(
        "ws://{}/ws?token={}",
        app2.addr,
        seeded
            .admin
            .access_token
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    );
    let (mut ctrl_ws, _) = connect_async(&ctrl_url).await.expect("controller ws");
    let _ = tokio::time::timeout(std::time::Duration::from_millis(200), ctrl_ws.next()).await;

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

    // Expect the rehome error on the controller.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut saw_rehome = false;
    while tokio::time::Instant::now() < deadline && !saw_rehome {
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
        if v.get("t").and_then(|x| x.as_str()) == Some("rc:error") {
            assert_eq!(
                v.get("code").and_then(|x| x.as_str()),
                Some("agent_on_other_pod"),
                "cross-pod miss must rehome, not lie offline: {v}"
            );
            saw_rehome = true;
        }
    }
    assert!(saw_rehome, "controller never received the rehome error");

    // The nudge closes the idle agent's WS on pod1 (read returns Close/None).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut closed = false;
    while tokio::time::Instant::now() < deadline && !closed {
        match tokio::time::timeout(std::time::Duration::from_millis(500), agent_ws.next()).await {
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => closed = true,
            Ok(Some(Err(_))) => closed = true,
            _ => continue,
        }
    }
    assert!(
        closed,
        "idle agent's WS was not nudged closed by the owner pod"
    );
    let aid = bson::oid::ObjectId::parse_str(&agent_id).unwrap();
    for _ in 0..10 {
        if !app1.state.rc_hub.is_agent_online(aid) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(!app1.state.rc_hub.is_agent_online(aid));
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
#[tokio::test]
async fn tunnel_open_rehome_cross_pod() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

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

    let seeded = app1.seed_tenant("trehome").await;
    // Target B homed on pod1; origin A drives the tunnel-client role on pod2.
    let (b_id, b_tok) =
        crate::tunnel_tests::enroll_agent(&app1, &seeded, "mach-trehome-B", "target-B").await;
    let mut b_ws = crate::tunnel_tests::connect_agent_ws(&app1, &b_tok, "target-B").await;
    let (_a_id, a_tok) =
        crate::tunnel_tests::enroll_agent(&app2, &seeded, "mach-trehome-A", "origin-A").await;
    let mut a_ws = crate::tunnel_tests::connect_agent_ws(&app2, &a_tok, "origin-A").await;
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

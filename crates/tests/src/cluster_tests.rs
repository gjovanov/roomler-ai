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

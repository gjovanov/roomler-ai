//! Stats PR-1 — collector + rollup integration tests (real MongoDB).
//!
//! Locks the three storage invariants the multi-pod design leans on:
//! deterministic-`_id` upserts are idempotent, the relay healthy-vote is
//! monotonic (a failure can never clobber a success in the same bucket),
//! and the rollup compactor is re-runnable without drift.

use bson::{Document, doc, oid::ObjectId};

use crate::fixtures::test_app::TestApp;
use roomler_ai_api::stats_rollup::run_stats_rollup_once;
use roomler_ai_services::dao::stats::{RelaySample, bucket_start};

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Aggregation `$sum` outputs are Int32 for small values but may widen to
/// Int64 — assert on the numeric value, not the BSON width.
fn int(doc: &Document, key: &str) -> i64 {
    match doc.get(key) {
        Some(bson::Bson::Int32(v)) => i64::from(*v),
        Some(bson::Bson::Int64(v)) => *v,
        Some(bson::Bson::Double(v)) => *v as i64,
        other => panic!("field {key} not numeric: {other:?}"),
    }
}

async fn count(app: &TestApp, coll: &str, filter: Document) -> u64 {
    app.state
        .db
        .collection::<Document>(coll)
        .count_documents(filter)
        .await
        .expect("count")
}

async fn find_one(app: &TestApp, coll: &str, filter: Document) -> Option<Document> {
    app.state
        .db
        .collection::<Document>(coll)
        .find_one(filter)
        .await
        .expect("find_one")
}

#[tokio::test]
async fn machine_sample_upserts_are_idempotent() {
    let app = TestApp::spawn().await;
    let tenant = ObjectId::new();
    let agent = ObjectId::new();
    let unix = unix_now();

    // Two heartbeats inside the same minute bucket (30 s cadence) → ONE
    // document, last write's gauge wins.
    app.state
        .stats
        .upsert_machine_sample(tenant, agent, unix, 1, None)
        .await
        .expect("first upsert");
    app.state
        .stats
        .upsert_machine_sample(tenant, agent, unix, 2, None)
        .await
        .expect("second upsert");

    let bucket = bucket_start(unix, 60);
    let id = format!("{}:{}", agent.to_hex(), bucket);
    assert_eq!(count(&app, "stats_machine", doc! {}).await, 1);
    let doc = find_one(&app, "stats_machine", doc! { "_id": &id })
        .await
        .expect("bucket doc");
    assert_eq!(doc.get_i32("active_sessions").unwrap(), 2);
    assert!(doc.get_bool("online").unwrap());
}

#[tokio::test]
async fn relay_healthy_vote_failure_never_clobbers_success() {
    let app = TestApp::spawn().await;
    let unix = unix_now();
    let sample = RelaySample {
        region: "us-east".into(),
        unix,
        poll_rtt_ms: 42,
        cpus: 2.0,
        load1: 0.5,
        load5: 0.4,
        mem_total_kb: 4_000_000.0,
        mem_available_kb: 2_000_000.0,
        rx_mbps: 1.0,
        tx_mbps: 2.0,
        allocations: 3.0,
        coturn_sessions: 1.0,
        derp_registrations: 5.0,
        uptime_s: 1000.0,
    };
    let id = format!("us-east:{}", bucket_start(unix, 30));

    // Pod A succeeds, pod B fails the SAME bucket → healthy must stay true
    // ($setOnInsert-only failure path).
    app.state
        .stats
        .upsert_relay_sample(&sample)
        .await
        .expect("success upsert");
    app.state
        .stats
        .upsert_relay_unreachable("us-east", unix)
        .await
        .expect("failure upsert");
    let doc = find_one(&app, "stats_relay", doc! { "_id": &id })
        .await
        .expect("bucket");
    assert!(doc.get_bool("healthy").unwrap());
    assert_eq!(doc.get_i64("poll_rtt_ms").unwrap(), 42);

    // Reverse order in a DIFFERENT bucket: failure first inserts
    // healthy:false, the late success overwrites it.
    let unix2 = unix + 30;
    let id2 = format!("us-east:{}", bucket_start(unix2, 30));
    app.state
        .stats
        .upsert_relay_unreachable("us-east", unix2)
        .await
        .expect("failure first");
    let doc2 = find_one(&app, "stats_relay", doc! { "_id": &id2 })
        .await
        .expect("failure bucket");
    assert!(!doc2.get_bool("healthy").unwrap());
    let late = RelaySample {
        unix: unix2,
        ..sample.clone()
    };
    app.state
        .stats
        .upsert_relay_sample(&late)
        .await
        .expect("late success");
    let doc2 = find_one(&app, "stats_relay", doc! { "_id": &id2 })
        .await
        .expect("healed bucket");
    assert!(doc2.get_bool("healthy").unwrap());
}

#[tokio::test]
async fn rollup_builds_hourly_and_daily_buckets_idempotently() {
    let app = TestApp::spawn().await;
    let tenant = ObjectId::new();
    let agent = ObjectId::new();
    let room = ObjectId::new();
    let call = ObjectId::new();

    // Seed into the PREVIOUS (closed) hour so the assertion set is stable
    // across the two rollup runs.
    let hour = bucket_start(unix_now(), 3600) - 3600;

    // 3 machine minute-buckets in that hour.
    for i in 0..3i64 {
        app.state
            .stats
            .upsert_machine_sample(tenant, agent, hour + i * 60, 1, None)
            .await
            .expect("machine seed");
    }
    // 2 relay buckets: one healthy, one unreachable.
    let s = RelaySample {
        region: "eu-north".into(),
        unix: hour,
        poll_rtt_ms: 50,
        cpus: 2.0,
        load1: 0.2,
        load5: 0.1,
        mem_total_kb: 4_000_000.0,
        mem_available_kb: 3_000_000.0,
        rx_mbps: 1.0,
        tx_mbps: 2.0,
        allocations: 0.0,
        coturn_sessions: 0.0,
        derp_registrations: 2.0,
        uptime_s: 10.0,
    };
    app.state
        .stats
        .upsert_relay_sample(&s)
        .await
        .expect("relay seed");
    app.state
        .stats
        .upsert_relay_unreachable("eu-north", hour + 30)
        .await
        .expect("relay unreachable seed");
    // 2 call buckets (shape written by the PR-2 sampler): 3 then 5
    // participants, 1 relayed each.
    for (i, participants) in [(0i64, 3i32), (1, 5)] {
        app.state
            .db
            .collection::<Document>("stats_call")
            .insert_one(doc! {
                "_id": format!("{}:{}", room.to_hex(), hour + i * 30),
                "tenant_id": tenant,
                "room_id": room,
                "call_id": call,
                "ts": bson::DateTime::from_millis((hour + i * 30) * 1000),
                "participants": participants,
                "relayed": 1i32,
                "direct": participants - 1,
                "send_bps": 500_000.0,
                "recv_bps": 400_000.0,
                "loss_pct": 0.5,
            })
            .await
            .expect("call seed");
    }

    let first = run_stats_rollup_once(&app.state).await;
    assert_eq!(first, 6, "all six merge passes should run");

    let mid = format!("{}:{}", agent.to_hex(), hour);
    let m1h = find_one(&app, "stats_machine_1h", doc! { "_id": &mid })
        .await
        .expect("machine 1h bucket");
    assert_eq!(int(&m1h, "online_minutes"), 3);
    assert_eq!(int(&m1h, "sample_count"), 3);

    let rid = format!("eu-north:{hour}");
    let r1h = find_one(&app, "stats_relay_1h", doc! { "_id": &rid })
        .await
        .expect("relay 1h bucket");
    assert_eq!(int(&r1h, "sample_count"), 2);
    assert_eq!(int(&r1h, "healthy_count"), 1);

    let cid = format!("{}:{}", tenant.to_hex(), hour);
    let c1h = find_one(&app, "stats_call_1h", doc! { "_id": &cid })
        .await
        .expect("call 1h bucket");
    // (3 + 5) participants × 30 s buckets.
    assert_eq!(int(&c1h, "participant_seconds"), 240);
    assert_eq!(int(&c1h, "relayed_seconds"), 60);
    assert_eq!(int(&c1h, "call_seconds"), 60);
    assert_eq!(int(&c1h, "distinct_rooms"), 1);

    // Daily buckets exist and carry the sums.
    let day = hour - hour.rem_euclid(86_400);
    let m1d = find_one(
        &app,
        "stats_machine_1d",
        doc! { "_id": format!("{}:{}", agent.to_hex(), day) },
    )
    .await
    .expect("machine 1d bucket");
    assert_eq!(int(&m1d, "online_minutes"), 3);

    // Re-running must not drift any counter (whole-bucket replace).
    let second = run_stats_rollup_once(&app.state).await;
    assert_eq!(second, 6);
    let m1h_again = find_one(&app, "stats_machine_1h", doc! { "_id": &mid })
        .await
        .expect("machine 1h after rerun");
    assert_eq!(int(&m1h_again, "online_minutes"), 3);
    assert_eq!(int(&m1h_again, "sample_count"), 3);
    let c1h_again = find_one(&app, "stats_call_1h", doc! { "_id": &cid })
        .await
        .expect("call 1h after rerun");
    assert_eq!(int(&c1h_again, "participant_seconds"), 240);
}

#[tokio::test]
async fn presence_events_ledger_appends() {
    let app = TestApp::spawn().await;
    let tenant = ObjectId::new();
    let agent = ObjectId::new();
    app.state
        .stats
        .append_presence_event(tenant, agent, "online")
        .await
        .expect("append online");
    app.state
        .stats
        .append_presence_event(tenant, agent, "offline")
        .await
        .expect("append offline");
    assert_eq!(
        count(&app, "stats_events", doc! { "agent_id": agent }).await,
        2
    );
}

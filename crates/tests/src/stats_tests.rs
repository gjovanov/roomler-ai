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

// ── Stats PR-2 — call lifecycle + orphan sweep ──────────────────────────

#[tokio::test]
async fn call_lifecycle_books_call_session_and_fills_dead_fields() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("statscall").await;
    let tid = &seeded.tenant_id;
    let rid_hex = &seeded.rooms[0].id;
    let token = &seeded.admin.access_token;
    let rid = ObjectId::parse_str(rid_hex).unwrap();
    let base = format!("/api/tenant/{tid}/room/{rid_hex}");

    // start ×2 → exactly ONE call instance, started_at stable (the
    // transition gate: a re-invoked start must not reset the clock).
    let r = app
        .auth_post(&format!("{base}/call/start"), token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let call1 = find_one(&app, "call_sessions", doc! { "room_id": rid })
        .await
        .expect("call doc created");
    let started_first = *call1.get_datetime("started_at").unwrap();
    let r = app
        .auth_post(&format!("{base}/call/start"), token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        count(&app, "call_sessions", doc! { "room_id": rid }).await,
        1
    );
    let room = find_one(&app, "rooms", doc! { "_id": rid }).await.unwrap();
    let call_id = room
        .get_object_id("current_call_id")
        .expect("current_call_id set");
    let call_again = find_one(&app, "call_sessions", doc! { "_id": call_id })
        .await
        .unwrap();
    assert_eq!(
        *call_again.get_datetime("started_at").unwrap(),
        started_first
    );

    // join → previously-dead rooms.peak_participant_count fills.
    let r = app
        .auth_post(&format!("{base}/call/join"), token)
        .json(&serde_json::json!({ "connection_id": "c1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let room = find_one(&app, "rooms", doc! { "_id": rid }).await.unwrap();
    assert!(int(&room, "peak_participant_count") >= 1);

    // Hold the session >1 s so the booked seconds are non-zero.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    // leave (last participant) → session closed WITH duration (dead field),
    // total_duration incremented, call minutes booked, call auto-ended.
    let r = app
        .auth_post(&format!("{base}/call/leave"), token)
        .json(&serde_json::json!({ "connection_id": "c1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);

    let member = find_one(
        &app,
        "room_members",
        doc! { "room_id": rid, "sessions.duration": { "$gte": 1 } },
    )
    .await;
    assert!(member.is_some(), "closed session should carry a duration");
    let member = member.unwrap();
    assert!(int(&member, "total_duration") >= 1);

    let call = find_one(&app, "call_sessions", doc! { "_id": call_id })
        .await
        .unwrap();
    assert!(int(&call, "participant_seconds") >= 1);
    assert!(
        call.get_datetime("ended_at").is_ok(),
        "last leaver must auto-end the call instance"
    );
    assert_eq!(call.get_str("end_reason").unwrap(), "last_left");
    let room = find_one(&app, "rooms", doc! { "_id": rid }).await.unwrap();
    assert!(
        room.get_object_id("current_call_id").is_err(),
        "current_call_id cleared on end"
    );
}

#[tokio::test]
async fn orphan_sweep_closes_stale_call_state() {
    let app = TestApp::spawn().await;
    let tenant = ObjectId::new();
    let room = ObjectId::new();
    let call = ObjectId::new();
    let hour_ago =
        bson::DateTime::from_millis(bson::DateTime::now().timestamp_millis() - 3_600_000);

    // A call doc + an open member session for a room that is NOT
    // in_progress (no room doc at all — same class as a crashed pod's
    // leftovers).
    app.state
        .db
        .collection::<Document>("call_sessions")
        .insert_one(doc! {
            "_id": call,
            "tenant_id": tenant,
            "room_id": room,
            "started_by": tenant,
            "started_at": hour_ago,
            "ended_at": bson::Bson::Null,
            "peak_participants": 2_i32,
            "participant_seconds": 0_i64,
        })
        .await
        .unwrap();
    app.state
        .db
        .collection::<Document>("room_members")
        .insert_one(doc! {
            "tenant_id": tenant,
            "room_id": room,
            "sessions": [ {
                "joined_at": hour_ago,
                "left_at": bson::Bson::Null,
                "duration": bson::Bson::Null,
                "device_type": "web",
                "connection_id": "dead-conn",
            } ],
            "total_duration": 0_i64,
        })
        .await
        .unwrap();

    let (calls, sessions) =
        roomler_ai_api::stats_rollup::close_orphaned_call_state(&app.state).await;
    assert_eq!(calls, 1);
    assert_eq!(sessions, 1);

    let call_doc = find_one(&app, "call_sessions", doc! { "_id": call })
        .await
        .unwrap();
    assert!(call_doc.get_datetime("ended_at").is_ok());
    assert_eq!(call_doc.get_str("end_reason").unwrap(), "stale_reset");
    let member = find_one(&app, "room_members", doc! { "room_id": room })
        .await
        .unwrap();
    let sess = member.get_array("sessions").unwrap()[0]
        .as_document()
        .unwrap();
    assert!(sess.get_datetime("left_at").is_ok());
    let d = match sess.get("duration") {
        Some(bson::Bson::Int64(v)) => *v,
        Some(bson::Bson::Int32(v)) => i64::from(*v),
        other => panic!("duration not filled: {other:?}"),
    };
    assert!((3500..3700).contains(&d), "duration ≈1h, got {d}");
}

// ── Stats PR-3 — platform admin + query APIs ────────────────────────────

#[tokio::test]
async fn admin_stats_gate_is_objectid_allowlist_with_404_miss() {
    let admin_id = ObjectId::new();
    let app = TestApp::spawn_with_settings(move |s| {
        s.stats.platform_admins = Some(admin_id.to_hex());
    })
    .await;
    // The admin endpoints never read the users collection — a minted
    // token for the allowlisted id suffices to exercise the gate.
    let tokens = app
        .state
        .auth
        .generate_tokens(admin_id, "padmin@test.io", "padmin")
        .unwrap();
    let r = app
        .auth_get("/api/admin/stats/relay/current", &tokens.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["enabled"], serde_json::json!(true));
    let r = app
        .auth_get("/api/admin/stats/orgs", &tokens.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);

    // Any other authed user → 404 (NEVER 403: the web client wipes
    // tokens and force-logs-out on 403).
    let user = app
        .register_user(
            "nobody@test.io",
            "nobody",
            "No Body",
            "Password123!",
            None,
            None,
        )
        .await;
    let r = app
        .auth_get("/api/admin/stats/relay/current", &user.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
}

#[tokio::test]
async fn tenant_stats_visibility_member_vs_admin_vs_outsider() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("statsvis").await;
    let tid = &seeded.tenant_id;

    // overview: any member 200.
    let r = app
        .auth_get(
            &format!("/api/tenant/{tid}/stats/overview"),
            &seeded.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["enabled"], serde_json::json!(true));
    assert!(body["machines"]["total"].is_number());

    // Queryable series: plain member (no MANAGE_AGENTS) → 404; owner → 200.
    let r = app
        .auth_get(
            &format!("/api/tenant/{tid}/stats/machines?range=7d"),
            &seeded.member.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
    let r = app
        .auth_get(
            &format!("/api/tenant/{tid}/stats/machines?range=7d"),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let r = app
        .auth_get(
            &format!("/api/tenant/{tid}/stats/tunnels"),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);

    // Outsider (authed, not a member): overview → 404.
    let outsider = app
        .register_user(
            "out@statsvis.io",
            "outsider",
            "Out Sider",
            "Password123!",
            None,
            None,
        )
        .await;
    let r = app
        .auth_get(
            &format!("/api/tenant/{tid}/stats/overview"),
            &outsider.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
}

#[tokio::test]
async fn me_payload_carries_platform_admin_flag() {
    let app = TestApp::spawn().await;
    let user = app
        .register_user(
            "flag@test.io",
            "flaguser",
            "Flag User",
            "Password123!",
            None,
            None,
        )
        .await;
    let r = app
        .auth_get("/api/auth/me", &user.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["is_platform_admin"], serde_json::json!(false));
}

#[tokio::test]
async fn tenant_calls_series_reads_rollups() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("statsq").await;
    let tid_oid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let hour = bucket_start(unix_now(), 3600) - 3600;
    app.state
        .db
        .collection::<Document>("stats_call_1h")
        .insert_one(doc! {
            "_id": format!("{}:{}", tid_oid.to_hex(), hour),
            "tenant_id": tid_oid,
            "ts": bson::DateTime::from_millis(hour * 1000),
            "sample_count": 10,
            "participant_seconds": 600_i64,
            "relayed_seconds": 60_i64,
            "direct_seconds": 540_i64,
            "call_seconds": 300_i64,
            "peak_participants": 4,
            "send_bps": 1_000_000.0,
            "recv_bps": 800_000.0,
            "loss_pct": 0.2,
            "distinct_rooms": 1,
        })
        .await
        .unwrap();
    let r = app
        .auth_get(
            &format!("/api/tenant/{}/stats/calls?range=7d", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    let series = body["series"].as_array().unwrap();
    assert_eq!(series.len(), 1);
    assert_eq!(series[0]["participant_seconds"], serde_json::json!(600));
    assert!(series[0]["t"].is_number());
    assert!(body["totals"]["calls"].is_number());
}

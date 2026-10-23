// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-20 — the cost ledger, against a real MongoDB.
//!
//! These close acceptance criteria the phase work did not: the ledger's
//! concurrency property, and the rule that only *carried* bytes are billed.
//!
//! ⚠ The concurrency test is the one the spec names explicitly ("two pods
//! writing the same bucket yield exactly one row"). It is not hypothetical: the
//! deployment runs two API pods, both flushing DERP bytes on their own 60 s
//! timer, and if the `_id` were not deterministic they would write two rows and
//! every bill would be split — silently, and in the *under*-reporting direction
//! that is hardest to notice.

use crate::fixtures::test_app::TestApp;
use bson::{doc, oid::ObjectId};
use roomler_ai_services::dao::stats::{Meter, STATS_USAGE, USAGE_BUCKET_SECS, bucket_start};

#[tokio::test]
async fn concurrent_writers_produce_exactly_one_summed_bucket() {
    let app = TestApp::spawn().await;
    let tenant = ObjectId::new();
    let unix = 1_788_102_180i64;

    // Two writers, same tenant/meter/minute — the two-pod case.
    let (a, b) = (app.state.stats.clone(), app.state.stats.clone());
    let (ra, rb) = tokio::join!(
        a.add_usage(tenant, Meter::DerpBytes, unix, 1_000),
        // A different second INSIDE the same minute must land on the same
        // bucket — that is what makes the pods agree without coordinating.
        b.add_usage(tenant, Meter::DerpBytes, unix + 41, 2_345),
    );
    ra.expect("writer a");
    rb.expect("writer b");

    let rows: Vec<bson::Document> = app
        .state
        .stats
        .coll(STATS_USAGE)
        .find(doc! { "tenant_id": tenant })
        .await
        .unwrap()
        .try_collect_all()
        .await;

    assert_eq!(rows.len(), 1, "two pods must produce ONE row, got {rows:?}");
    assert_eq!(
        rows[0].get_i64("value").unwrap(),
        3_345,
        "the row must carry the SUM of both pods' contributions"
    );
    let expected_id = format!(
        "{}:{}:{}",
        tenant.to_hex(),
        Meter::DerpBytes.as_str(),
        bucket_start(unix, USAGE_BUCKET_SECS)
    );
    assert_eq!(rows[0].get_str("_id").unwrap(), expected_id);
}

/// Two meters for one tenant in one minute are two ledger lines. Without this
/// the `$merge`d derived meter and the `$inc`ed observed one could share a
/// document, and the SFU rollup would silently replace a bytes bucket.
#[tokio::test]
async fn different_meters_never_share_a_bucket() {
    let app = TestApp::spawn().await;
    let tenant = ObjectId::new();
    let unix = 1_788_102_180i64;

    app.state
        .stats
        .add_usage(tenant, Meter::DerpBytes, unix, 500)
        .await
        .unwrap();
    app.state
        .stats
        .add_usage(tenant, Meter::TurnBytes, unix, 700)
        .await
        .unwrap();

    let rows: Vec<bson::Document> = app
        .state
        .stats
        .coll(STATS_USAGE)
        .find(doc! { "tenant_id": tenant })
        .await
        .unwrap()
        .try_collect_all()
        .await;
    assert_eq!(rows.len(), 2, "one row per meter");
}

/// A tenant that relayed nothing has NO ledger rows — it does not get a zero
/// row. "No traffic" and "not monitored" must stay distinguishable all the way
/// to the UI, and a fabricated zero would collapse them.
#[tokio::test]
async fn a_tenant_with_no_traffic_has_no_rows() {
    let app = TestApp::spawn().await;
    let tenant = ObjectId::new();

    // A zero-valued write is a no-op by construction.
    app.state
        .stats
        .add_usage(tenant, Meter::DerpBytes, 1_788_102_180, 0)
        .await
        .unwrap();

    let n = app
        .state
        .stats
        .coll(STATS_USAGE)
        .count_documents(doc! { "tenant_id": tenant })
        .await
        .unwrap();
    assert_eq!(n, 0, "no traffic must mean no row, never a zero row");
}

/// #2763 — the baseline every pod shares for a PoP's cumulative counter,
/// against the real Redis: only a higher value from the same process books
/// anything, and only the first pod to see it books it.
#[tokio::test]
async fn the_shared_baseline_books_only_what_is_new() {
    let app = TestApp::spawn().await;
    let Some(redis) = app.state.redis_pubsub.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let key = format!("roomler:pop_net_baseline:test:{}", ObjectId::new().to_hex());
    for (boot, value, books, why) in [
        ("b1", 100, 0, "no baseline yet: it starts one"),
        ("b1", 150, 50, "the growth since the baseline"),
        ("b1", 120, 0, "another pod's older sample books nothing"),
        ("b1", 170, 20, "and did not lower the baseline"),
        ("b1", 170, 0, "the pod that saw it first booked it"),
        ("b2", 10, 0, "a restart starts a new baseline"),
        ("b2", 25, 15, "the new process's growth"),
        ("b1", 200, 0, "an old process's late sample books nothing"),
        ("b2", 40, 0, "the new one loses an interval; no double"),
    ] {
        let got = redis.advance_baseline(&key, boot, value, 60).await.unwrap();
        assert_eq!(got, books, "{boot}|{value}: {why}");
    }
    let ttl: i64 = redis::cmd("TTL")
        .arg(&key)
        .query_async(&mut redis.connection())
        .await
        .unwrap();
    assert!((1..=60).contains(&ttl), "a baseline expires, got TTL {ttl}");

    // A stored value that does not parse starts a new baseline.
    redis.kv_set_ex(&key, "b3|garbage", 60).await.unwrap();
    assert_eq!(redis.advance_baseline(&key, "b3", 10, 60).await.unwrap(), 0);
    assert_eq!(
        redis.advance_baseline(&key, "b3", 30, 60).await.unwrap(),
        20
    );
    redis.kv_del(&key).await.unwrap();
}

/// The database's `derp_bytes` for `tenant`, summed over its buckets.
async fn derp_bytes(app: &TestApp, tenant: ObjectId) -> i64 {
    app.state
        .stats
        .coll(STATS_USAGE)
        .find(doc! { "tenant_id": tenant, "meter": Meter::DerpBytes.as_str() })
        .await
        .unwrap()
        .try_collect_all()
        .await
        .iter()
        .map(|row| row.get_i64("value").unwrap_or(0))
        .sum()
}

/// #2763 — both pods poll every PoP, and each used to diff the PoP's
/// cumulative counter against its OWN last sample, so every byte a PoP
/// relayed was booked once per pod. Two pods sample 10,000 then 60,000 here:
/// the tenant is booked the 50,000 once, where each pod used to book it.
/// The same PoP also reports a network no tenant owns, and a second endpoint
/// (a derp-relay from before #2763) reports another tenant's bytes with no
/// `boot`: neither may book anything.
#[tokio::test]
async fn a_pops_network_bytes_are_booked_once_across_pods() {
    use roomler_ai_services::dao::overlay_network::OverlayNetworkDao;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    // What each endpoint answers; no bytes until the networks exist.
    let current: Arc<Mutex<Value>> = Arc::new(Mutex::new(json!({ "cpus": 1, "boot": "b1" })));
    let old: Arc<Mutex<Value>> = Arc::new(Mutex::new(json!({ "cpus": 1 })));
    let served = Arc::new(AtomicUsize::new(0));
    let pop = {
        let (current, old, served) = (current.clone(), old.clone(), served.clone());
        axum::Router::new()
            .route(
                "/stats",
                axum::routing::get(move || {
                    let (current, served) = (current.clone(), served.clone());
                    async move {
                        served.fetch_add(1, Ordering::Relaxed);
                        axum::Json(current.lock().unwrap().clone())
                    }
                }),
            )
            .route(
                "/old",
                axum::routing::get(move || {
                    let old = old.clone();
                    async move { axum::Json(old.lock().unwrap().clone()) }
                }),
            )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pop_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, pop).await.unwrap() });

    let (app1, _app2) = TestApp::spawn_pair(move |s| {
        s.stats.enabled = true;
        s.turn.url = Some("turn:coturn.example:3478".into());
        s.turn.shared_secret = Some(ObjectId::new().to_hex());
        s.relay.regions_enabled = true;
        s.relay.regions = Some(format!(
            r#"[{{"id":"pop-net","turn_url":"turn:coturn-pop.example:3478","stats_urls":["http://{pop_addr}/stats","http://{pop_addr}/old"]}}]"#
        ));
        s.relay.stats_poll_secs = 1;
    })
    .await;
    if app1.state.redis_pubsub.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let networks = OverlayNetworkDao::new(&app1.db);
    let mut tenants = Vec::new();
    for name in ["fr20pop1", "fr20pop2"] {
        let t = app1.seed_tenant(name).await;
        let tenant = ObjectId::parse_str(&t.tenant_id).unwrap();
        let network = networks.get_or_create(tenant).await.unwrap().id.unwrap();
        tenants.push((tenant, network.to_hex()));
    }
    let [(tenant, network), (old_tenant, old_network)] = &tenants[..] else {
        unreachable!()
    };
    let stranger = ObjectId::new().to_hex();
    let report = |boot: &str, bytes: u64| {
        *current.lock().unwrap() = json!({
            "cpus": 1,
            "boot": boot,
            "per_network": { network.as_str(): bytes, stranger.as_str(): bytes },
        });
        *old.lock().unwrap() = json!({
            "cpus": 1,
            "per_network": { old_network.as_str(): bytes },
        });
    };
    // Enough `/stats` answers that both pods (each polling every second)
    // have sampled what is being served.
    let polls = |n: usize| {
        let served = served.clone();
        async move {
            let target = served.load(Ordering::Relaxed) + n;
            let deadline = Instant::now() + Duration::from_secs(20);
            while served.load(Ordering::Relaxed) < target && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(
                served.load(Ordering::Relaxed) >= target,
                "the pods kept polling"
            );
        }
    };
    let booked_at_least = |want: i64| {
        let app = &app1;
        async move {
            let deadline = Instant::now() + Duration::from_secs(20);
            while derp_bytes(app, *tenant).await < want && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    };

    report("b1", 10_000);
    polls(6).await;
    report("b1", 60_000);
    booked_at_least(50_000).await;
    polls(6).await;
    assert_eq!(
        derp_bytes(&app1, *tenant).await,
        50_000,
        "the growth is booked once, not once per pod"
    );

    // The PoP restarts: its counter starts again under a new boot.
    report("b2", 5_000);
    polls(6).await;
    assert_eq!(
        derp_bytes(&app1, *tenant).await,
        50_000,
        "a restart books nothing"
    );
    report("b2", 8_000);
    booked_at_least(53_000).await;
    polls(6).await;
    assert_eq!(
        derp_bytes(&app1, *tenant).await,
        53_000,
        "the new process's growth, once"
    );

    assert_eq!(
        derp_bytes(&app1, *old_tenant).await,
        0,
        "an endpoint with no boot books nothing"
    );
    let elsewhere = app1
        .state
        .stats
        .coll(STATS_USAGE)
        .count_documents(doc! {
            "meter": Meter::DerpBytes.as_str(),
            "tenant_id": { "$nin": [*tenant, *old_tenant] },
        })
        .await
        .unwrap();
    assert_eq!(elsewhere, 0, "a network no tenant owns books nothing");
}

/// Helper: collect a cursor without pulling in extra deps at the call sites.
trait CollectAll {
    async fn try_collect_all(self) -> Vec<bson::Document>;
}
impl CollectAll for mongodb::Cursor<bson::Document> {
    async fn try_collect_all(mut self) -> Vec<bson::Document> {
        use futures::TryStreamExt;
        let mut out = Vec::new();
        while let Some(d) = self.try_next().await.unwrap() {
            out.push(d);
        }
        out
    }
}

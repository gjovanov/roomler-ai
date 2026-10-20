// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-115 P4a — the relay allowance's job, through a real server and a real
//! MongoDB. An org past its plan's monthly relay has every overlay network
//! in the over-allowance set, at its plan's rate. Inside the allowance, last
//! month, with the kill switch off, or on a server that sells no plans,
//! nothing is.
//!
//! The process-wide map `forward_frame` reads is the unit tests'
//! (`relay_allowance::tests`): these tests read what the job COMPUTES and
//! PUBLISHES, which is scoped to each test's own database.

use bson::{DateTime, Document, doc, oid::ObjectId};
use roomler_ai_mod_network::{NetworkState, relay_allowance};
use roomler_ai_services::dao::overlay_network::OverlayNetworkDao;

use crate::fixtures::test_app::TestApp;

const GB: i64 = 1_000_000_000;

fn net(app: &TestApp) -> &NetworkState {
    app.state
        .modules
        .network
        .as_ref()
        .expect("the network module is mounted in tests")
}

fn oid(hex: &str) -> ObjectId {
    ObjectId::parse_str(hex).unwrap()
}

fn enable(s: &mut roomler_ai_config::Settings) {
    s.relay_allowance.enabled = true;
}

/// `bytes` of DERP relay for `tenant` this month, as FR-20 records them: in
/// the hourly tier for a completed hour when the month has one, the rest in
/// the raw tier's open hour.
async fn relayed_this_month(app: &TestApp, tenant: ObjectId, bytes: i64) {
    let now = DateTime::now();
    let start = relay_allowance::month_start(now);
    let hour = DateTime::from_millis(now.timestamp_millis() / 3_600_000 * 3_600_000);
    let (hourly, raw) = if hour.timestamp_millis() > start.timestamp_millis() {
        (bytes / 2, bytes - bytes / 2)
    } else {
        (0, bytes)
    };
    if hourly > 0 {
        usage_row(app, "stats_usage_1h", tenant, start, hourly).await;
    }
    usage_row(app, "stats_usage", tenant, now, raw).await;
}

async fn usage_row(app: &TestApp, coll: &str, tenant: ObjectId, ts: DateTime, value: i64) {
    app.db
        .collection::<Document>(coll)
        .insert_one(doc! {
            "_id": format!("{}:derp_bytes:{}:{}", tenant.to_hex(), ts.timestamp_millis() / 1000, ObjectId::new().to_hex()),
            "tenant_id": tenant,
            "meter": "derp_bytes",
            "ts": ts,
            "value": value,
        })
        .await
        .unwrap();
}

async fn network_of(app: &TestApp, tenant: ObjectId) -> ObjectId {
    OverlayNetworkDao::new(&app.db)
        .get_or_create(tenant)
        .await
        .unwrap()
        .id
        .expect("network _id")
}

async fn set_plan(app: &TestApp, tenant: ObjectId, plan: &str, seats: i64) {
    app.db
        .collection::<Document>("tenants")
        .update_one(
            doc! { "_id": tenant },
            doc! { "$set": { "plan": plan, "billing": { "seats": seats } } },
        )
        .await
        .unwrap();
}

/// A Free org past its 10 GB has its network in the set at the Free rate
/// (1 Mbit/s = 125,000 B/s), and the job publishes exactly that.
#[tokio::test]
async fn a_free_org_past_its_allowance_is_throttled_at_the_free_rate() {
    let app = TestApp::spawn_with_settings(enable).await;
    let t = app.seed_tenant("fr115relay1").await;
    let tid = oid(&t.tenant_id);
    let network = network_of(&app, tid).await;

    relayed_this_month(&app, tid, 9 * GB).await;
    let set = relay_allowance::compute(net(&app), DateTime::now())
        .await
        .unwrap();
    assert!(set.is_empty(), "inside its allowance: {set:?}");

    relayed_this_month(&app, tid, 2 * GB).await;
    let set = relay_allowance::compute(net(&app), DateTime::now())
        .await
        .unwrap();
    assert_eq!(set.get(&network), Some(&125_000), "{set:?}");

    // Published for every pod to read, scoped to this database. `publish` is
    // the job's body: the module's own loop claimed this tick at startup, so
    // `run_job_once` here would lose the claim and publish nothing.
    assert_eq!(
        relay_allowance::publish(net(&app), DateTime::now()).await,
        Some(1)
    );
    if let Some(redis) = &net(&app).redis_pubsub {
        let raw = redis
            .kv_get(&relay_allowance::set_key(app.db.name()))
            .await
            .unwrap()
            .expect("the set is published");
        let published: std::collections::HashMap<String, u64> = serde_json::from_str(&raw).unwrap();
        assert_eq!(published.get(&network.to_hex()), Some(&125_000), "{raw}");
    }
}

/// Pro's allowance is 25 GB per billed seat, and its rate is the paid one
/// (4 Mbit/s = 500,000 B/s).
#[tokio::test]
async fn a_pro_orgs_allowance_grows_with_its_seats() {
    let app = TestApp::spawn_with_settings(enable).await;
    let t = app.seed_tenant("fr115relay2").await;
    let tid = oid(&t.tenant_id);
    let network = network_of(&app, tid).await;
    set_plan(&app, tid, "pro", 3).await;

    relayed_this_month(&app, tid, 60 * GB).await;
    let set = relay_allowance::compute(net(&app), DateTime::now())
        .await
        .unwrap();
    assert!(
        set.is_empty(),
        "60 GB is inside three seats' 75 GB: {set:?}"
    );

    relayed_this_month(&app, tid, 20 * GB).await;
    let set = relay_allowance::compute(net(&app), DateTime::now())
        .await
        .unwrap();
    assert_eq!(set.get(&network), Some(&500_000), "{set:?}");
}

/// The allowance is monthly: last month's relay does not count.
#[tokio::test]
async fn last_months_relay_does_not_count() {
    let app = TestApp::spawn_with_settings(enable).await;
    let t = app.seed_tenant("fr115relay3").await;
    let tid = oid(&t.tenant_id);
    network_of(&app, tid).await;
    let start = relay_allowance::month_start(DateTime::now());
    let last_month = DateTime::from_millis(start.timestamp_millis() - 86_400_000);
    usage_row(&app, "stats_usage_1h", tid, last_month, 50 * GB).await;

    let set = relay_allowance::compute(net(&app), DateTime::now())
        .await
        .unwrap();
    assert!(set.is_empty(), "{set:?}");
}

/// The kill switch: the job publishes nothing, whoever is over.
#[tokio::test]
async fn the_kill_switch_publishes_nothing() {
    let app = TestApp::spawn().await; // `relay_allowance.enabled` defaults off
    let t = app.seed_tenant("fr115relay4").await;
    let tid = oid(&t.tenant_id);
    network_of(&app, tid).await;
    relayed_this_month(&app, tid, 20 * GB).await;

    assert_eq!(
        relay_allowance::publish(net(&app), DateTime::now()).await,
        Some(0)
    );
    if let Some(redis) = &net(&app).redis_pubsub {
        let raw = redis
            .kv_get(&relay_allowance::set_key(app.db.name()))
            .await
            .unwrap();
        assert!(
            raw.is_none(),
            "nothing is published with the switch off: {raw:?}"
        );
    }
}

/// A server that sells no plans throttles nobody: self-hosting is
/// unlimited (`Core::plans_sold`).
#[tokio::test]
async fn a_server_that_sells_no_plans_throttles_nobody() {
    let app = TestApp::spawn_with_settings(|s| {
        enable(s);
        s.modules.saas = false;
    })
    .await;
    let t = app.seed_tenant("fr115relay5").await;
    let tid = oid(&t.tenant_id);
    network_of(&app, tid).await;
    relayed_this_month(&app, tid, 20 * GB).await;

    assert_eq!(
        relay_allowance::publish(net(&app), DateTime::now()).await,
        Some(0)
    );
}

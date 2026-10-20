// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-115 P4b (AC8) — coturn's statistics DB into `turn_bytes` per org,
//! through real servers, a real Redis and a real MongoDB. The test plays
//! coturn: it publishes an allocation's totals on coturn's channels, in
//! coturn's format, under usernames issued the way the server issues them
//! (`turn_creds::charged`).
//!
//! ⚠️ Every TestApp with `turn_attribution` on subscribes to the same Redis,
//! so tests running at once hear each other's messages (and record them in
//! their own databases). Each test asserts only on its own ids.

use std::time::{Duration, Instant};

use bson::{Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use roomler_ai_remote_control::turn_creds::charged;

use crate::fixtures::test_app::TestApp;

fn attribution_on(s: &mut roomler_ai_config::Settings) {
    s.relay_allowance.turn_attribution = true;
}

/// The database's `turn_bytes` for `tenant`, summed over its buckets.
async fn turn_bytes(app: &TestApp, tenant: ObjectId) -> i64 {
    let mut cursor = app
        .db
        .collection::<Document>("stats_usage")
        .find(doc! { "tenant_id": tenant, "meter": "turn_bytes" })
        .await
        .unwrap();
    let mut total = 0i64;
    while let Some(row) = cursor.try_next().await.unwrap() {
        total += row.get_i64("value").unwrap_or(0);
    }
    total
}

/// coturn's two total channels for one allocation: the client leg, and the
/// relay ↔ peer leg.
fn channels(username: &str, allocation: u64) -> (String, String) {
    let client =
        format!("turn/realm/roomler.ai/user/{username}/allocation/{allocation:018}/total_traffic");
    let peer = format!("{client}/peer");
    (client, peer)
}

/// One allocation's two legs, charged to an org, are counted once for that
/// org as what coturn SENT: toward the client plus toward the peer, though
/// both pods hear every message. The subscriptions are asynchronous, so the
/// test publishes until the total lands, then publishes the same allocation
/// again: a repeat is the same allocation leg, and must not count twice.
#[tokio::test]
async fn a_charged_allocation_is_counted_once_across_pods() {
    let (app1, _app2) = TestApp::spawn_pair(attribution_on).await;
    let Some(redis) = app1.state.redis_pubsub.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let t = app1.seed_tenant("fr115turn1").await;
    let org = ObjectId::parse_str(&t.tenant_id).unwrap();
    let principal = charged(&ObjectId::new().to_hex(), &org.to_hex());
    let (client, peer) = channels(&format!("1792000000:{principal}"), 4242);

    // Publish until both legs have landed, and keep publishing the same
    // allocation for two more rounds after that.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut rounds_since_counted = 0;
    while rounds_since_counted < 3 && Instant::now() < deadline {
        redis
            .publish_to(&client, "rcvp=10, rcvb=1000, sentp=20, sentb=50000")
            .await
            .unwrap();
        redis
            .publish_to(&peer, "rcvp=20, rcvb=50000, sentp=10, sentb=1000")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        if turn_bytes(&app1, org).await >= 51_000 {
            rounds_since_counted += 1;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        turn_bytes(&app1, org).await,
        51_000,
        "each leg once: 50,000 sent toward the client plus 1,000 toward the peer"
    );
}

/// A username that names no org is counted nowhere. The case that matters
/// is an overlay pair's: its principal is `<node>:<node>`, so uncharged it
/// still ends in an ObjectId, and a parser that took the last hex part for
/// the org would book the bytes under a node. A charged control proves the
/// subscription was live.
#[tokio::test]
async fn a_username_naming_no_org_is_not_counted() {
    let app = TestApp::spawn_with_settings(attribution_on).await;
    let Some(redis) = app.state.redis_pubsub.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let t = app.seed_tenant("fr115turn2").await;
    let org = ObjectId::parse_str(&t.tenant_id).unwrap();
    let (a, b) = (ObjectId::new(), ObjectId::new());
    let pair = format!("{}:{}", a.to_hex(), b.to_hex());
    let (uncharged, _) = channels(&format!("1792000000:{pair}"), 7);
    let (control, _) = channels(&format!("1792000000:{}", charged(&pair, &org.to_hex())), 8);

    let deadline = Instant::now() + Duration::from_secs(20);
    while turn_bytes(&app, org).await < 9_000 && Instant::now() < deadline {
        redis
            .publish_to(&uncharged, "rcvp=1, rcvb=1, sentp=1, sentb=777")
            .await
            .unwrap();
        redis
            .publish_to(&control, "rcvp=1, rcvb=1, sentp=1, sentb=9000")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert_eq!(turn_bytes(&app, org).await, 9_000, "the control is counted");
    assert_eq!(
        turn_bytes(&app, b).await,
        0,
        "nothing is booked under the node an uncharged pair username ends with"
    );
}

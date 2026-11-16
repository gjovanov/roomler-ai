// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3840 — a Redis that stops answering without a reset or a close (a node
//! lost on a dropped path, a frozen machine) fails a command on the shared
//! connection, and a request that awaits one, within the response timeout,
//! instead of hanging for as long as the operating system keeps the socket.
//!
//! The stand-in is [`RedisBlackout`]: a forwarder in front of the lane's real
//! Redis that drops every byte in both directions and closes nothing.

use std::time::{Duration, Instant};

use roomler_core::ws::redis_pubsub::RedisPubSub;

use crate::fixtures::redis_blackout::{RedisBlackout, test_redis_url};
use crate::fixtures::test_app::TestApp;

/// The default response timeout (`ROOMLER__REDIS__RESPONSE_TIMEOUT_MS`,
/// 3000): a command with no answer fails this long after it was sent.
const BOUND: Duration = Duration::from_secs(3);
/// What a test allows on top of the bound: scheduling, and the rest of a
/// request's own work.
const SLACK: Duration = Duration::from_secs(2);
/// The test's own limit. A wait that reaches it was bounded by nothing.
const OUTER: Duration = Duration::from_secs(8);

fn unique(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}

/// A GET on the shared connection, sent after Redis stopped answering, fails
/// with the response timeout within the bound.
#[tokio::test]
async fn a_redis_command_fails_within_the_bound_when_redis_stops_answering() {
    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let ps = RedisPubSub::new(&fwd.url_for(&test_redis_url()), unique("w14-test/"))
        .await
        .expect("the shared connection connects through the forwarder");
    let key = unique("roomler:test:w14:");
    ps.kv_set_ex(&key, "v", 60)
        .await
        .expect("a Redis that answers answers");

    fwd.swallow();
    let started = Instant::now();
    let res = tokio::time::timeout(OUTER, ps.kv_get(&key)).await;
    let took = started.elapsed();
    eprintln!(
        "W14-RESULT command: {} after {took:?}",
        match &res {
            Ok(Ok(_)) => "answered",
            Ok(Err(_)) => "failed",
            Err(_) => "still waiting",
        }
    );
    assert!(
        res.is_ok(),
        "W14-RED a GET sent after Redis stopped answering was still waiting after {took:?} \
         (the test's own limit): nothing bounds a command on the shared connection"
    );
    let err = res
        .unwrap()
        .expect_err("a Redis that never answers cannot have answered");
    assert!(
        err.is_timeout(),
        "the command failed with `{err}`, not with the response timeout"
    );
    assert!(
        took <= BOUND + SLACK,
        "the command failed after {took:?}; the bound is {BOUND:?}"
    );
}

/// A chat message POST awaits the cross-pod fan-out (a PUBLISH on the shared
/// connection) before it answers. With Redis gone silent it still answers 200
/// within the bound: the fan-out is lost, exactly as with Redis down, and the
/// post is not.
#[tokio::test]
async fn a_message_post_answers_within_the_bound_when_redis_stops_answering() {
    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let via = fwd.url_for(&test_redis_url());
    let app = TestApp::spawn_with_settings(move |s| s.redis.url = via).await;
    assert!(
        app.state.redis_pubsub.is_some(),
        "the server reached Redis through the forwarder"
    );
    let tenant = app.seed_tenant("w14rb").await;
    let room_id = tenant.rooms[0].id.clone();
    // The seeded admin may already be a member of the seeded room (409).
    let join = app
        .auth_post(
            &format!("/api/tenant/{}/room/{}/join", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .send()
        .await
        .expect("join");
    assert!(
        join.status().is_success() || join.status().as_u16() == 409,
        "join: {}",
        join.status()
    );
    let post = |content: &str| {
        app.auth_post(
            &format!("/api/tenant/{}/room/{}/message", tenant.tenant_id, room_id),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "content": content }))
        .send()
    };

    let before = tokio::time::timeout(OUTER, post("while Redis answers"))
        .await
        .expect("a post while Redis answers answers")
        .expect("the server answered");
    assert_eq!(
        before.status().as_u16(),
        200,
        "control: a post while Redis answers"
    );

    fwd.swallow();
    let started = Instant::now();
    let res = tokio::time::timeout(OUTER, post("after Redis stopped answering")).await;
    let took = started.elapsed();
    eprintln!(
        "W14-RESULT message post: {} after {took:?}",
        match &res {
            Ok(Ok(r)) => format!("answered {}", r.status()),
            Ok(Err(e)) => format!("transport error {e}"),
            Err(_) => "still waiting".to_string(),
        }
    );
    assert!(
        res.is_ok(),
        "W14-RED a message POST was still waiting after {took:?} (the test's own limit): \
         the request hangs on the fan-out PUBLISH to a Redis that stopped answering"
    );
    let resp = res.unwrap().expect("the server answered");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "a Redis that stopped answering costs the fan-out, never the post"
    );
    assert!(
        took <= BOUND + SLACK,
        "the post answered after {took:?}; the bound is {BOUND:?}"
    );
}

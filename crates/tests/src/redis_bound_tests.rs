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

use roomler_core::ws::redis_conn::RedisBounds;
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

// ── Controls: the bound changes nothing while Redis answers ─────────────────

/// How long after Redis answers again a test allows before the shared
/// connection is used again: a replacement attempt that began while Redis was
/// silent ends at the connection timeout (3 s), then waits at most
/// `REPLACE_WAIT_MAX` (5 s) before the next attempt; plus slack.
const RECOVERY: Duration = Duration::from_secs(10);

async fn through(fwd: &RedisBlackout) -> RedisPubSub {
    RedisPubSub::new(&fwd.url_for(&test_redis_url()), unique("w14-test/"))
        .await
        .expect("the shared connection connects through the forwarder")
}

/// A Redis that answers: every kind of call the server sends (a command, a
/// pipeline, a script, a publish) is answered, with no timeout and so no warn
/// line (the counter counts exactly those lines), no replacement and no
/// second connection.
#[tokio::test]
async fn a_healthy_redis_sees_no_timeout_and_no_replacement() {
    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let ps = through(&fwd).await;
    let key = unique("roomler:test:w14:");
    for i in 0..50 {
        ps.kv_set_ex(&key, &i.to_string(), 60).await.expect("SET");
        assert_eq!(ps.kv_get(&key).await.expect("GET"), Some(i.to_string()));
        ps.online_add(&key).await.expect("pipeline");
        ps.agent_presence_del_if_owned(&key, "nobody")
            .await
            .expect("script");
        ps.publish_to(&key, "x").await.expect("PUBLISH");
    }
    ps.online_remove(&key).await.expect("SREM");
    ps.kv_del(&key).await.expect("DEL");
    let conn = ps.connection();
    assert_eq!(conn.timeouts(), 0, "a Redis that answers logs no timeout");
    assert_eq!(conn.replacements(), 0, "and replaces nothing");
    assert_eq!(fwd.accepted(), 1, "one connection carried all of it");
}

/// One answer slower than usual but under the bound arrives, and changes
/// nothing: no timeout, no replacement.
#[tokio::test]
async fn one_slow_answer_under_the_bound_changes_nothing() {
    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let ps = through(&fwd).await;
    let key = unique("roomler:test:w14:");
    ps.kv_set_ex(&key, "v", 60).await.expect("SET");

    let slow = Duration::from_millis(1500);
    fwd.delay_next_reply(slow);
    let started = Instant::now();
    let got = ps
        .kv_get(&key)
        .await
        .expect("a slow answer is still an answer");
    let took = started.elapsed();
    eprintln!("W14-RESULT slow answer: {got:?} after {took:?}");
    assert_eq!(got.as_deref(), Some("v"));
    assert!(
        took >= slow - Duration::from_millis(100),
        "the reply was held only {took:?}"
    );
    assert!(
        took < BOUND,
        "a reply held {slow:?} answered after {took:?}"
    );
    let conn = ps.connection();
    assert_eq!(
        conn.timeouts(),
        0,
        "an answer under the bound is no timeout"
    );
    assert_eq!(conn.replacements(), 0, "and replaces nothing");
    assert_eq!(fwd.accepted(), 1);
    ps.kv_del(&key).await.expect("DEL");
}

/// After a blackout the old connection stays dead (the forwarder never
/// forwards again on a connection that lost a byte, like a node that is
/// gone), and a Redis that answers again is used again without a restart,
/// through the replacement the first timeout started. The blackout outlasts
/// the replacement's first attempt, so the time measured includes a failed
/// attempt and the wait before the next one.
#[tokio::test]
async fn redis_answering_again_is_used_again_without_a_restart() {
    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let ps = through(&fwd).await;
    let key = unique("roomler:test:w14:");
    ps.kv_set_ex(&key, "v", 60).await.expect("SET");

    fwd.swallow();
    let err = ps
        .kv_get(&key)
        .await
        .expect_err("a Redis that stopped answering cannot answer");
    assert!(err.is_timeout(), "{err}");
    assert_eq!(ps.connection().timeouts(), 1);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    fwd.pass();
    let back = Instant::now();
    let mut answered = None;
    while back.elapsed() < RECOVERY + BOUND {
        // A short wait of the test's own: a probe that lands on the dead
        // connection is dropped, never counted as a timeout.
        if let Ok(Ok(v)) = tokio::time::timeout(Duration::from_millis(300), ps.kv_get(&key)).await {
            answered = Some(v);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let took = back.elapsed();
    let conn = ps.connection();
    eprintln!(
        "W14-RESULT recovery: used again {took:?} after Redis answered again \
         (timeouts {}, replacements {}, connections {})",
        conn.timeouts(),
        conn.replacements(),
        fwd.accepted()
    );
    assert_eq!(
        answered,
        Some(Some("v".to_string())),
        "Redis answering again was not used again within {:?}",
        RECOVERY + BOUND
    );
    assert!(
        took <= RECOVERY,
        "used again only {took:?} after Redis answered again"
    );
    assert_eq!(conn.replacements(), 1, "through one replacement");
    assert!(fwd.accepted() >= 2, "on a new connection");
    ps.kv_del(&key).await.expect("DEL");
}

/// Both settings at 0 restore the connection as it was before #3840: a
/// command sent after Redis stopped answering is still waiting at the test's
/// own limit, and nothing counts it.
#[tokio::test]
async fn with_both_bounds_at_0_a_command_waits_as_before() {
    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let settings = roomler_ai_config::RedisSettings {
        url: fwd.url_for(&test_redis_url()),
        response_timeout_ms: 0,
        connection_timeout_ms: 0,
    };
    let ps = RedisPubSub::from_settings(&settings, unique("w14-test/"))
        .await
        .expect("connects");
    assert_eq!(ps.connection().bounds(), RedisBounds::NONE);
    let key = unique("roomler:test:w14:");
    ps.kv_set_ex(&key, "v", 60).await.expect("SET");

    fwd.swallow();
    let res = tokio::time::timeout(BOUND + SLACK, ps.kv_get(&key)).await;
    assert!(
        res.is_err(),
        "with no bound the command must still be waiting after {:?}, as before #3840",
        BOUND + SLACK
    );
    assert_eq!(ps.connection().timeouts(), 0);
}

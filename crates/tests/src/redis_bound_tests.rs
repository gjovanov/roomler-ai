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

use roomler_core::ws::redis_conn::{CONNECT_LINE, RedisBounds, TIMEOUT_LINE};
use roomler_core::ws::redis_pubsub::RedisPubSub;

use crate::fixtures::redis_blackout::{RedisBlackout, test_redis_url};
use crate::fixtures::test_app::TestApp;
use crate::session_media_plan_tests::Logged;

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
/// pipeline, a script, a publish) is answered, with no timeout, no connection
/// failure, no warn line of either wording, no replacement and no second
/// connection.
#[tokio::test]
async fn a_healthy_redis_sees_no_timeout_and_no_replacement() {
    let logged = Logged::default();
    let _log = logged.install();
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
    assert_eq!(conn.connect_failures(), 0, "and no connection failure");
    let text = logged.text();
    assert!(
        !text.contains(TIMEOUT_LINE) && !text.contains(CONNECT_LINE),
        "a Redis that answers writes neither warn line:\n{text}"
    );
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
        subscriber_probe_ms: 15_000,
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

// ── A session frame whose relay Redis fails ─────────────────────────────────

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Every JSON frame a socket receives within `window`.
async fn frames_within(ws: &mut Ws, window: Duration) -> Vec<serde_json::Value> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let until = tokio::time::Instant::now() + window;
    let mut got = Vec::new();
    loop {
        let left = until.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                    got.push(v);
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
        }
    }
    got
}

fn kind(frame: &serde_json::Value) -> &str {
    frame.get("t").and_then(|t| t.as_str()).unwrap_or("")
}

/// #3840, a relayed session frame. A controller on one pod runs a session
/// with a device on the other pod through the relay. While the controller's
/// pod gets no answer from Redis, the session frames it relays (`rc:sdp.offer`,
/// `rc:ice`) fail on Redis, which says nothing about the owner pod: each is
/// dropped, the controller is told nothing, and the route to the owner pod is
/// kept, so a frame sent once Redis answers again reaches the device.
/// `rc:terminate` rides along as a control: it is never relayed, because the
/// hub of the controller's pod answers it Ok for a session it does not hold.
#[tokio::test]
async fn a_session_frame_redis_fails_to_relay_keeps_its_route_and_tells_the_controller_nothing() {
    use futures::SinkExt;
    use std::sync::atomic::Ordering;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let via = fwd.url_for(&test_redis_url());
    // The device's pod reaches Redis directly, the controller's through the
    // forwarder.
    let (app1, app2) = TestApp::spawn_pair(move |s| {
        if s.app
            .pod_id
            .as_deref()
            .is_some_and(|p| p.starts_with("testpod2-"))
        {
            s.redis.url = via.clone();
        }
    })
    .await;
    let (Some(bus1), Some(bus2)) = (
        app1.state.cluster_bus.clone(),
        app2.state.cluster_bus.clone(),
    ) else {
        panic!("both pods reach Redis, the controller's through the forwarder");
    };
    let subscribing = Instant::now();
    while !(bus1.sub_alive.load(Ordering::Relaxed) && bus2.sub_alive.load(Ordering::Relaxed)) {
        assert!(subscribing.elapsed() < OUTER, "both pod buses subscribe");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The device on pod 1, the controller on pod 2.
    let seeded = app1.seed_tenant("w14relay").await;
    let (agent_id, agent_token) =
        crate::agent_presence_tests::enroll(&app1, &seeded, "mach-w14-relay").await;
    let mut agent_ws = crate::agent_presence_tests::connect_agent(&app1, &agent_token).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let token = seeded
        .admin
        .access_token
        .replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D");
    let (mut ctrl_ws, _) = connect_async(format!("ws://{}/ws?token={token}", app2.addr))
        .await
        .expect("controller ws");
    let _ = frames_within(&mut ctrl_ws, Duration::from_millis(200)).await;
    let text = |v: &serde_json::Value| Message::Text(v.to_string().into());

    // Control: the session is created through the relay, and the
    // controller's pod holds a route to the device's pod.
    ctrl_ws
        .send(text(&serde_json::json!({
            "t": "rc:session.request",
            "agent_id": agent_id,
            "permissions": "VIEW",
        })))
        .await
        .expect("send the request");
    let mut session_id = None;
    let asked = Instant::now();
    while session_id.is_none() && asked.elapsed() < OUTER {
        for v in frames_within(&mut ctrl_ws, Duration::from_millis(500)).await {
            match kind(&v) {
                "rc:session.created" => session_id = v["session_id"].as_str().map(str::to_string),
                "rc:error" => panic!("control: the relay creates the session, got {v}"),
                _ => {}
            }
        }
    }
    let sid = session_id.expect("control: rc:session.created arrives through the relay");
    let at_device = frames_within(&mut agent_ws, Duration::from_secs(2)).await;
    assert!(
        at_device.iter().any(|v| kind(v) == "rc:request"),
        "control: the device got the relayed request"
    );
    let remote2 = app2
        .state
        .modules
        .remote
        .clone()
        .expect("the remote module is mounted");
    let pod1 = app1.state.pod.pod_id.clone();
    let routes = || {
        remote2
            .remote_rc_conns
            .iter()
            .filter(|e| e.value().contains(&pod1))
            .count()
    };
    assert_eq!(
        routes(),
        1,
        "control: the controller's pod holds a route to the device's pod"
    );

    // Redis stops answering the controller's pod.
    fwd.swallow();
    let ice = serde_json::json!({
        "t": "rc:ice",
        "session_id": sid,
        "candidate": { "candidate": "candidate:1 1 udp 1 192.0.2.1 9 typ host", "sdpMid": "0" },
    });
    let frames = [
        serde_json::json!({ "t": "rc:sdp.offer", "session_id": sid, "sdp": "v=0" }),
        ice.clone(),
        serde_json::json!({ "t": "rc:terminate", "session_id": sid, "reason": "controller_hangup" }),
    ];
    let sent = Instant::now();
    for frame in &frames {
        ctrl_ws
            .send(text(frame))
            .await
            .expect("send a session frame");
    }
    // The socket's frames run one after another, and each relayed one fails
    // at the bound.
    let told = frames_within(&mut ctrl_ws, 2 * BOUND + SLACK + Duration::from_secs(1)).await;
    let errors: Vec<&serde_json::Value> = told.iter().filter(|v| kind(v) == "rc:error").collect();
    let kept = routes();
    eprintln!(
        "W14-RESULT relayed frames while Redis is silent: the controller was told {} rc:error \
         ({:?}) within {:?}; routes to the device's pod: {kept}",
        errors.len(),
        errors
            .iter()
            .map(|v| v["code"].as_str().unwrap_or(""))
            .collect::<Vec<_>>(),
        sent.elapsed()
    );
    assert!(
        errors.is_empty(),
        "W14-RED the controller was told {errors:?} for session frames whose relay Redis \
         failed: a Redis error says nothing about the device's pod"
    );
    assert_eq!(
        kept, 1,
        "W14-RED the route to the device's pod was forgotten on a Redis failure"
    );

    // Redis answers again: the kept route carries the next frame to the
    // device, once the shared connection is replaced.
    fwd.pass();
    let back = Instant::now();
    let mut reached = false;
    while !reached && back.elapsed() < RECOVERY + BOUND + SLACK {
        ctrl_ws.send(text(&ice)).await.expect("send the ice again");
        reached = frames_within(&mut agent_ws, Duration::from_secs(1))
            .await
            .iter()
            .any(|v| kind(v) == "rc:ice");
    }
    eprintln!(
        "W14-RESULT relay once Redis answers again: {} after {:?}",
        if reached {
            "the device got the ice"
        } else {
            "nothing reached the device"
        },
        back.elapsed()
    );
    assert!(
        reached,
        "the route kept through the silence carries a frame to the device once Redis answers \
         again: nothing arrived within {:?}",
        RECOVERY + BOUND + SLACK
    );
}

/// #3840 — a detached session's slot keep that Redis did not answer is tried
/// again while its lease can still exist, so a silence longer than the bound
/// does not give away the slot of the session it kept (the lease would lapse
/// at its own expiry, and a resume at the org's cap would then be refused).
#[tokio::test]
async fn a_slot_keep_redis_did_not_answer_lands_once_redis_answers_again() {
    use roomler_core::cluster::session_slots::{SessionSlots, SlotClaim, SlotScope};

    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let ps = through(&fwd).await;
    let slots = SessionSlots::new(ps.connection(), &unique("w14db"));
    let (org, session) = (
        SlotScope::Org(bson::oid::ObjectId::new()),
        bson::oid::ObjectId::new(),
    );
    let (user, device) = (bson::oid::ObjectId::new(), bson::oid::ObjectId::new());
    let claimed = slots
        .claim(org, session, user, device, 1, "w14-pod")
        .await
        .expect("the claim is answered");
    assert_eq!(claimed, SlotClaim::Claimed);
    let until = bson::DateTime::now().timestamp_millis() + 600_000;

    fwd.swallow();
    let started = Instant::now();
    let keep = tokio::spawn({
        let slots = slots.clone();
        async move { slots.keep_retrying(org, session, until).await }
    });
    // The first try has had no answer within the bound by now.
    tokio::time::sleep(BOUND + Duration::from_millis(500)).await;
    fwd.pass();
    let answered = tokio::time::timeout(RECOVERY + RECOVERY, keep).await;
    let elapsed = started.elapsed();
    eprintln!(
        "W14-RESULT slot keep through a {:?} silence: {answered:?} after {elapsed:?}",
        BOUND + Duration::from_millis(500)
    );
    let kept = match answered {
        Ok(Ok(Ok(kept))) => kept,
        other => panic!(
            "W14-RED the keep did not land once Redis answered again: {other:?} after {elapsed:?}"
        ),
    };
    assert!(kept, "the lease still existed, so it was kept");

    // Read the lease's expiry past the forwarder, straight from Redis.
    let direct = RedisPubSub::new(&test_redis_url(), unique("w14-direct/"))
        .await
        .expect("a direct connection");
    let mut conn = direct.connection();
    let expiry: Option<f64> = redis::cmd("ZSCORE")
        .arg(slots.slots_key(org))
        .arg(session.to_hex())
        .query_async(&mut conn)
        .await
        .expect("ZSCORE");
    assert!(
        expiry.is_some_and(|ms| ms as i64 >= until),
        "the lease lives to the kept deadline: {expiry:?} < {until}"
    );
    let _ = slots.release(org, session).await;
}

/// #3840 — the claim a registration makes late, after its first claim had no
/// answer (`claim_if_newer`), writes its record where no registration minted
/// after it holds the key: absent, its own, older from this pod or another,
/// or a value that names no registration. It leaves a token minted in the
/// same millisecond or later alone, from either pod, and answers `false`.
#[tokio::test]
async fn a_late_claim_takes_a_record_only_from_an_older_registration() {
    use roomler_core::cluster::directory::OwnershipDirectory;

    let direct = RedisPubSub::new(&test_redis_url(), unique("w14-direct/"))
        .await
        .expect("a direct connection");
    let here = OwnershipDirectory::new(direct.connection(), "w14-here/e1".to_string());
    let mut conn = direct.connection();
    let now = bson::DateTime::now().timestamp_millis();
    let at = |origin: &str, ms: i64| format!("{origin}/agent/{ms}");
    let ours = at("w14-here/e1", now);
    let cases = [
        ("absent", None, true),
        ("its own", Some(ours.clone()), true),
        ("older, this pod", Some(at("w14-here/e1", now - 1)), true),
        (
            "older, another pod",
            Some(at("w14-there/e2", now - 60_000)),
            true,
        ),
        ("no token", Some("garbage".to_string()), true),
        (
            "same millisecond, another pod",
            Some(at("w14-there/e2", now)),
            false,
        ),
        (
            "newer, another pod",
            Some(at("w14-there/e2", now + 1)),
            false,
        ),
        (
            "newer, this pod",
            Some(at("w14-here/e1", now + 60_000)),
            false,
        ),
    ];
    let mut wrong = Vec::new();
    for (case, held, writes) in cases {
        let key = unique("roomler:test:w14:late:");
        if let Some(held) = &held {
            let _: () = redis::cmd("SET")
                .arg(&key)
                .arg(held)
                .arg("EX")
                .arg(90)
                .query_async(&mut conn)
                .await
                .expect("SET");
        }
        let answered = here
            .claim_if_newer(&key, &ours, 90)
            .await
            .expect("a Redis that answers answers");
        let holds = here.get(&key).await.expect("GET");
        let want = if writes { Some(ours.clone()) } else { held };
        eprintln!("W14-RESULT late claim, {case}: answered {answered}, holds {holds:?}");
        if answered != writes || holds != want {
            wrong.push(format!(
                "{case}: answered {answered}, holds {holds:?}, wanted {want:?}"
            ));
        }
        let _: () = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut conn)
            .await
            .expect("DEL");
    }
    assert!(
        wrong.is_empty(),
        "W14-RED a late claim took or left the wrong records: {wrong:#?}"
    );
}

/// A node's DERP record, and whether its network's index names it, read
/// straight from Redis.
async fn derp_entry(
    conn: &mut roomler_core::ws::redis_conn::RedisConnection,
    (network, pk): (bson::oid::ObjectId, [u8; 32]),
) -> (Option<String>, bool) {
    use roomler_ai_mod_network::derp_cluster::pk_hex;
    use roomler_core::cluster::directory::{derp_key, derpnet_key};

    let member = pk_hex(&pk);
    let record: Option<String> = redis::cmd("GET")
        .arg(derp_key(&network.to_hex(), &member))
        .query_async(&mut *conn)
        .await
        .expect("GET");
    let indexed: bool = redis::cmd("SISMEMBER")
        .arg(derpnet_key(&network.to_hex()))
        .arg(&member)
        .query_async(&mut *conn)
        .await
        .expect("SISMEMBER");
    (record, indexed)
}

/// #3840 — a DERP registration whose claim Redis did not answer keeps its
/// token, and once Redis answers again its record and its member of the
/// network's index are written, so convergence sees the node without waiting
/// for it to reconnect.
///
/// Four registrations on the pod behind the forwarder while Redis is silent:
/// a first one; one that displaced this pod's earlier registration of the
/// same node; one made after the other pod's older registration of the same
/// node; and one made BEFORE the other pod registers the same node. The first
/// three end with the record and the member; the last leaves the record to
/// the other pod's newer registration. Then the other pod's older
/// registration ends, and its teardown leaves the record and the member of
/// the registration that replaced it.
#[tokio::test]
async fn a_derp_registration_redis_did_not_answer_is_written_once_redis_answers_again() {
    use bson::oid::ObjectId;
    use roomler_ai_mod_network::NetworkState;
    use roomler_ai_mod_network::derp_cluster::{on_derp_register, on_derp_teardown};

    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let via = fwd.url_for(&test_redis_url());
    // This pod reaches Redis through the forwarder, the other pod directly.
    let (other_app, app) = TestApp::spawn_pair(move |s| {
        if s.app
            .pod_id
            .as_deref()
            .is_some_and(|p| p.starts_with("testpod2-"))
        {
            s.redis.url = via.clone();
        }
    })
    .await;
    let (here, there) = (app.state.network(), other_app.state.network());
    let agent = ObjectId::new();
    // One network per case, so no case's sweep reads another case's members.
    let node = |n: u8| {
        let mut pk = [n; 32];
        pk[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        (ObjectId::new(), pk)
    };
    let (first, again, after_older, before_newer) = (node(1), node(2), node(3), node(4));
    // A socket for each registration, as derp.rs inserts one before the hook
    // (the heartbeat drops a token whose socket is gone).
    let mut sockets = Vec::new();
    for (pod, key) in [
        (here, first),
        (here, again),
        (here, after_older),
        (here, before_newer),
        (there, after_older),
        (there, before_newer),
    ] {
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        pod.derp_registry.insert(key, tx);
        sockets.push(rx);
    }
    let token = |pod: &NetworkState, key: (ObjectId, [u8; 32])| {
        pod.derp_presence_tokens
            .get(&key)
            .map(|t| t.value().clone())
    };

    // Before the silence: this pod's earlier registration of `again`, and the
    // other pod's older registration of `after_older`.
    on_derp_register(here, again.0, &again.1, agent).await;
    on_derp_register(there, after_older.0, &after_older.1, agent).await;
    let displaced = token(here, again).expect("a registration Redis answered keeps its token");
    // A token carries the millisecond it was minted in.
    tokio::time::sleep(Duration::from_millis(5)).await;

    fwd.swallow();
    let started = Instant::now();
    let registering = futures::future::join_all(
        [first, again, after_older, before_newer]
            .map(move |(n, pk)| async move { on_derp_register(here, n, &pk, agent).await }),
    );
    let registered = tokio::time::timeout(OUTER, registering).await;
    let took = started.elapsed();
    eprintln!("W14-RESULT four DERP registrations while Redis was silent returned after {took:?}");
    assert!(
        registered.is_ok() && took < BOUND + SLACK,
        "W14-RED a registration waited {took:?} on a silent Redis: its path waits one bound at most"
    );
    // The other pod registers `before_newer` after this pod did.
    tokio::time::sleep(Duration::from_millis(5)).await;
    on_derp_register(there, before_newer.0, &before_newer.1, agent).await;
    let newer = token(there, before_newer).expect("the other pod's registration keeps its token");
    fwd.pass();

    let direct = RedisPubSub::new(&test_redis_url(), unique("w14-direct/"))
        .await
        .expect("a direct connection");
    let mut conn = direct.connection();
    let mine = [
        ("first", first),
        ("again", again),
        ("after an older one", after_older),
    ];
    let deadline = Instant::now() + RECOVERY + RECOVERY;
    loop {
        let mut written = true;
        for (_, key) in mine {
            let held = token(here, key);
            let (record, indexed) = derp_entry(&mut conn, key).await;
            written &= held.is_some() && record == held && indexed;
        }
        if written || Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let settled = started.elapsed();
    // The other pod's older registration of the node ends now.
    on_derp_teardown(there, after_older.0, &after_older.1).await;

    let mut report = Vec::new();
    let mut wrong = Vec::new();
    for (case, key) in mine {
        let held = token(here, key);
        let (record, indexed) = derp_entry(&mut conn, key).await;
        report.push(format!(
            "{case}: token {held:?}, record {record:?}, indexed {indexed}"
        ));
        if held.is_none() || record != held || !indexed {
            wrong.push(case);
        }
    }
    if token(here, again).as_ref() == Some(&displaced) {
        wrong.push("again: this pod still holds the token it displaced");
    }
    let (record, _) = derp_entry(&mut conn, before_newer).await;
    report.push(format!(
        "before a newer one: record {record:?}, the other pod's token {newer:?}"
    ));
    if record.as_ref() != Some(&newer) {
        wrong.push("before a newer one: the newer registration lost its record");
    }
    eprintln!("W14-RESULT DERP registrations, settled after {settled:?}: {report:#?}");
    assert!(
        wrong.is_empty(),
        "W14-RED once Redis answered again: {wrong:?} in {report:#?}"
    );
    drop(sockets);
}

// ── The bounds as they are set, and the lines a firing writes ───────────────

/// A response timeout set under 1 s (3, meant as seconds) is used as 1 s,
/// with one warn line at boot that names the setting; a bound set to 0 stays
/// no bound and writes nothing. The boot line names the address Redis is
/// reached at (host and port, never the URL) and both bounds as used, in ms.
#[tokio::test]
async fn a_bound_under_1_s_is_used_as_1_s_and_the_boot_line_says_what_is_used() {
    let logged = Logged::default();
    let _log = logged.install();
    let settings = roomler_ai_config::RedisSettings {
        url: test_redis_url(),
        response_timeout_ms: 3,
        connection_timeout_ms: 0,
        subscriber_probe_ms: 15_000,
    };
    let ps = RedisPubSub::from_settings(&settings, unique("w14-test/"))
        .await
        .expect("connects");
    let used = ps.connection().bounds();
    let text = logged.text();
    let raised: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("ROOMLER__REDIS__"))
        .collect();
    let boot = text
        .lines()
        .find(|l| l.contains("Redis Pub/Sub publisher connected"))
        .unwrap_or_default();
    eprintln!(
        "W14-RESULT bounds set 3 and 0: used {used:?}; raised {raised:?}; boot line `{boot}`"
    );
    assert_eq!(
        used,
        RedisBounds {
            response_timeout: Some(Duration::from_secs(1)),
            connection_timeout: None,
        },
        "W14-RED a response timeout set to 3 must be used as 1 s, and 0 as no bound"
    );
    assert_eq!(raised.len(), 1, "one warn line, for the one setting raised");
    for part in [
        "WARN",
        "ROOMLER__REDIS__RESPONSE_TIMEOUT_MS",
        "given_ms=3",
        "used_ms=1000",
    ] {
        assert!(
            raised[0].contains(part),
            "the warn line has `{part}`: {}",
            raised[0]
        );
    }
    let addr = redis::Client::open(test_redis_url().as_str())
        .expect("the lane's Redis URL")
        .get_connection_info()
        .addr
        .to_string();
    for part in [
        format!("addr={addr}"),
        "response_timeout_ms=1000".to_string(),
        "connection_timeout_ms=0".to_string(),
    ] {
        assert!(boot.contains(&part), "the boot line has `{part}`: {boot}");
    }
    assert!(
        !boot.contains("redis://"),
        "the boot line names no URL: {boot}"
    );
}

/// How long the restart test lets Redis refuse: longer than the crate's first
/// retry of a reconnect (1 to 2 s), after which that reconnect waits 60 to
/// 120 s before it tries again.
const REFUSED: Duration = Duration::from_secs(5);

/// A Redis that restarts: every connection is closed and every new one
/// refused, then it listens again. Every command in the gap fails within the
/// bound, and none of it is logged as "no answer within the timeout", the
/// line of a Redis that took the bytes and said nothing: the gap is "no
/// connection could be made". Once Redis listens again the connection is
/// used again within `RECOVERY`, without a restart of the server.
#[tokio::test]
async fn a_restarting_redis_is_logged_as_no_connection_and_used_again_once_it_listens() {
    let logged = Logged::default();
    let _log = logged.install();
    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let ps = through(&fwd).await;
    let key = unique("roomler:test:w14:");
    ps.kv_set_ex(&key, "v", 60).await.expect("SET");

    fwd.refuse().await;
    let gone = Instant::now();
    let (mut failed, mut slowest) = (0, Duration::ZERO);
    while gone.elapsed() < REFUSED {
        let started = Instant::now();
        let res = tokio::time::timeout(OUTER, ps.kv_get(&key)).await;
        let took = started.elapsed();
        slowest = slowest.max(took);
        match res {
            Ok(Err(_)) => failed += 1,
            Ok(Ok(v)) => panic!("a Redis that refuses answered {v:?}"),
            Err(_) => panic!(
                "W14-RED a GET while Redis refused was still waiting after {took:?} \
                 (the test's own limit)"
            ),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    fwd.listen_again();
    let back = Instant::now();
    let mut answered = None;
    while back.elapsed() < RECOVERY + BOUND {
        // A short wait of the test's own, as in the blackout's recovery test.
        if let Ok(Ok(v)) = tokio::time::timeout(Duration::from_millis(300), ps.kv_get(&key)).await {
            answered = Some(v);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let took = back.elapsed();
    let conn = ps.connection();
    let text = logged.text();
    let no_answer: Vec<&str> = text.lines().filter(|l| l.contains(TIMEOUT_LINE)).collect();
    let no_connection = text.lines().filter(|l| l.contains(CONNECT_LINE)).count();
    eprintln!(
        "W14-RESULT restart: {failed} GETs failed while Redis refused, the slowest after \
         {slowest:?}; used again {took:?} after it listened again; lines: no answer {}, no \
         connection {no_connection}; counted: timeouts {}, connection failures {}, \
         replacements {}",
        no_answer.len(),
        conn.timeouts(),
        conn.connect_failures(),
        conn.replacements()
    );
    assert!(
        slowest <= BOUND + SLACK,
        "a GET while Redis refused failed only after {slowest:?}; the bound is {BOUND:?}"
    );
    assert!(
        no_answer.is_empty(),
        "W14-RED a Redis that refused was logged as one that took the bytes and said \
         nothing: {no_answer:#?}"
    );
    assert_eq!(conn.timeouts(), 0, "and counted as one");
    assert!(no_connection >= 1, "the gap is logged as no connection");
    assert!(conn.connect_failures() >= 1, "and counted");
    assert_eq!(
        answered,
        Some(Some("v".to_string())),
        "a Redis that listens again was not used again within {:?}",
        RECOVERY + BOUND
    );
    assert!(
        took <= RECOVERY,
        "used again only {took:?} after Redis listened again"
    );
    ps.kv_del(&key).await.expect("DEL");
}

/// Twenty commands that time out within one second write one line between
/// them, and the next line written says how many it stood for
/// (`suppressed`). The counter counts every one, so an outage of an hour
/// writes one line a second per pod at most and the count stays exact.
#[tokio::test]
async fn timeouts_within_a_second_write_one_line_and_every_one_is_counted() {
    let logged = Logged::default();
    let _log = logged.install();
    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let ps = through(&fwd).await;
    let key = unique("roomler:test:w14:");
    ps.kv_set_ex(&key, "v", 60).await.expect("SET");

    fwd.swallow();
    let mut gets = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let (ps, key) = (ps.clone(), key.clone());
        gets.spawn(async move { ps.kv_get(&key).await });
    }
    let results = tokio::time::timeout(OUTER, gets.join_all())
        .await
        .expect("W14-RED twenty GETs into a silent Redis were still waiting at the test's limit");
    assert!(
        results
            .iter()
            .all(|r| matches!(r, Err(e) if e.is_timeout())),
        "every GET failed with the response timeout: {results:?}"
    );
    // The replacement the first timeout started tries one connection; Redis
    // is silent, so it gives up after the connection timeout (3 s), seconds
    // after the twenty: its line is written, and says how many were held
    // back. The next attempt waits 1 s, then 3 s more.
    tokio::time::sleep(BOUND + Duration::from_secs(1)).await;
    let conn = ps.connection();
    let text = logged.text();
    let lines: Vec<&str> = text.lines().filter(|l| l.contains(TIMEOUT_LINE)).collect();
    eprintln!(
        "W14-RESULT twenty timeouts within a second: {} lines, counted {}: {lines:#?}",
        lines.len(),
        conn.timeouts()
    );
    assert_eq!(
        conn.timeouts(),
        21,
        "every timeout is counted: the 20 GETs and 1 connection attempt"
    );
    assert_eq!(
        lines.len(),
        2,
        "W14-RED one line for the twenty, and one for the attempt seconds later"
    );
    assert!(
        lines[0].contains("suppressed=0"),
        "the first line held nothing back: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("suppressed=19") && lines[1].contains("timeouts=21"),
        "the second line stands for the 19 held back: {}",
        lines[1]
    );
}

/// Callers queued in front of a socket Redis stopped reading. The crate's own
/// response timeout starts only once a command is in its queue to the socket,
/// and that queue holds 50, so a caller still waiting for room in it is
/// bounded by the outer timeout alone. 160 SETs of 256 KiB fill the socket
/// and the queue, and every one fails with the response timeout within the
/// bound.
#[tokio::test]
async fn callers_queued_in_front_of_a_socket_redis_stopped_reading_fail_within_the_bound() {
    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let ps = through(&fwd).await;
    let key = unique("roomler:test:w14:");
    ps.kv_set_ex(&key, "v", 60).await.expect("SET");

    fwd.stop_reading();
    let value: std::sync::Arc<str> = "x".repeat(256 * 1024).into();
    let started = Instant::now();
    let mut sets = tokio::task::JoinSet::new();
    for i in 0..160 {
        let (ps, key, value) = (ps.clone(), format!("{key}:{i}"), value.clone());
        sets.spawn(async move { ps.kv_set_ex(&key, &value, 60).await });
    }
    let results = tokio::time::timeout(OUTER, sets.join_all()).await;
    let took = started.elapsed();
    let counted = ps.connection().timeouts();
    let timed_out = results.as_ref().map_or(0, |r| {
        r.iter()
            .filter(|r| matches!(r, Err(e) if e.is_timeout()))
            .count()
    });
    eprintln!(
        "W14-RESULT 160 queued SETs: {timed_out} failed with the response timeout after \
         {took:?}, counted {counted}"
    );
    assert!(
        results.is_ok(),
        "W14-RED SETs queued in front of a socket Redis stopped reading were still waiting \
         after {took:?} (the test's own limit)"
    );
    assert_eq!(timed_out, 160, "every SET failed with the response timeout");
    assert!(
        took <= BOUND + SLACK,
        "the queued SETs failed after {took:?}; the bound is {BOUND:?}"
    );
    assert_eq!(counted, 160, "each counted once");
}

/// #3840 — a DERP registration's late claim can land after its own teardown:
/// the teardown ran while the claim was in flight and found no record to
/// release. The step after the claim (`keep_late_claim`) releases the record
/// the claim wrote, so no record outlives its socket, and a registration that
/// still holds its token keeps its record.
#[tokio::test]
async fn a_late_claim_that_lands_after_its_teardown_is_released() {
    use bson::oid::ObjectId;
    use roomler_ai_mod_network::derp_cluster::{keep_late_claim, pk_hex};
    use roomler_core::cluster::directory::derp_key;

    let app = TestApp::spawn().await;
    let pod = app.state.network();
    let dir = pod.cluster_directory.clone().expect("the pod's directory");
    let mut wrong = Vec::new();
    for (case, held) in [
        ("torn down while its claim was in flight", false),
        ("still registered", true),
    ] {
        let network = ObjectId::new();
        let mut pk = [7u8; 32];
        pk[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        let key = derp_key(&network.to_hex(), &pk_hex(&pk));
        let token = dir.owner_token(&ObjectId::new().to_hex());
        if held {
            pod.derp_presence_tokens
                .insert((network, pk), token.clone());
        }
        // The late claim landed: the record holds the registration's token.
        let wrote = dir
            .claim_if_newer(&key, &token, 90)
            .await
            .expect("a Redis that answers answers");
        let kept = keep_late_claim(pod, network, &pk, &token).await;
        let record = dir.get(&key).await.expect("GET");
        eprintln!(
            "W14-RESULT late claim, {case}: wrote {wrote}, kept {kept}, record held {}",
            record.is_some()
        );
        if !wrote || kept != held || record.is_some() != held {
            wrong.push(format!(
                "{case}: wrote {wrote}, kept {kept}, record {record:?}"
            ));
        }
        pod.derp_presence_tokens.remove(&(network, pk));
        let _ = dir.release(&key, &token).await;
    }
    assert!(
        wrong.is_empty(),
        "W14-RED a late claim that landed after its teardown kept its record: {wrong:#?}"
    );
}

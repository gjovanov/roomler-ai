// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3952 — each of the server's Redis subscribers (the fan-out, the pod bus,
//! TURN usage) learns within a stated bound that its own connection has gone
//! deaf, and subscribes again on a fresh one; a subscribe that gets no answer
//! gives up within #3840's bounds and tries again; and a subscriber on a
//! healthy Redis never subscribes again.
//!
//! The stand-in is #3840's [`RedisBlackout`]: a connection that has a byte
//! dropped while the forwarder swallows never forwards again. So a message
//! published to a subscriber's channel during a swallow makes that subscriber
//! deaf for good, while its socket stays open and its stream never ends: the
//! shape of a lost path, a frozen node or a proxy that lost its backend.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use roomler_ai_remote_control::turn_creds::charged;
use roomler_core::cluster::bus::{BusError, PodBus};
use roomler_core::cluster::identity::PodIdentity;
use roomler_core::ws::redis_pubsub::RedisPubSub;
use roomler_core::ws::sub_probe::ProbeSettings;

use crate::fixtures::redis_blackout::{RedisBlackout, test_redis_url};
use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::session_media_plan_tests::Logged;
use crate::turn_usage_tests::{channels, turn_bytes};

/// How long a deaf cell waits, after Redis answers again, for the subscriber
/// to hear again.
const WINDOW: Duration = Duration::from_secs(25);
/// The bound a deaf cell holds the fix to, after Redis answers again, at the
/// probe's floor interval: #3840's command connection is back within C + 5 s,
/// then two missed probes, the 1 s backoff, a connect and the subscribes.
const FOUND: Duration = Duration::from_secs(20);
/// A setup step's own limit.
const OUTER: Duration = Duration::from_secs(10);
/// How often a flag is read.
const SAMPLE: Duration = Duration::from_millis(50);
/// The traffic of a deaf cell: never the probe interval or a multiple of it.
const PACE: Duration = Duration::from_millis(700);
/// The probe interval of the deaf cells: the floor.
const FLOOR_MS: u64 = 1_000;
/// How long a silent-listener cell watches the reconnect loop.
const SILENT: Duration = Duration::from_secs(10);

/// The lines a healthy run must never print: a verdict, a refused probe, and
/// every subscriber's ended and failed wordings.
const NEVER_ON_A_HEALTHY_REDIS: &[&str] = &[
    "stopped hearing its probe",
    "probe channel was refused",
    "Redis Pub/Sub subscription stream ended",
    "Redis Pub/Sub subscribe failed",
    "pod bus subscription ended",
    "pod bus subscribe failed",
    "turn usage: the subscription ended",
    "turn usage: subscribe failed",
    "undecodable envelope",
];
/// The DEBUG line of one missed probe.
const MISS_LINE: &str = "probe went unheard";

fn unique(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}

fn probe(ms: u64) -> ProbeSettings {
    ProbeSettings {
        interval: (ms > 0).then(|| Duration::from_millis(ms)),
    }
}

// ── Fixtures ────────────────────────────────────────────────────────────────

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_ws(app: &TestApp, token: &str) -> Ws {
    let token = token
        .replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D");
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={token}", app.addr))
        .await
        .expect("the member's socket opens");
    ws
}

/// The content of every `message:create` a socket receives within `window`.
async fn messages_within(ws: &mut Ws, window: Duration) -> Vec<String> {
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
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t)
                    && v["type"] == "message:create"
                    && let Some(content) = v["data"]["content"].as_str()
                {
                    got.push(content.to_string());
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
        }
    }
    got
}

async fn join_room(app: &TestApp, tenant: &SeededTenant, token: &str) {
    let join = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/join",
                tenant.tenant_id, tenant.rooms[0].id
            ),
            token,
        )
        .send()
        .await
        .expect("join");
    // The seeded admin may already be a member of the seeded room (409).
    assert!(
        join.status().is_success() || join.status().as_u16() == 409,
        "join: {}",
        join.status()
    );
}

/// The admin posts `content` to the seeded room; the status, or 0 when the
/// server did not answer within the test's own limit.
async fn post_message(app: &TestApp, tenant: &SeededTenant, content: &str) -> u16 {
    let send = app
        .auth_post(
            &format!(
                "/api/tenant/{}/room/{}/message",
                tenant.tenant_id, tenant.rooms[0].id
            ),
            &tenant.admin.access_token,
        )
        .json(&serde_json::json!({ "content": content }))
        .send();
    match tokio::time::timeout(OUTER, send).await {
        Ok(Ok(resp)) => resp.status().as_u16(),
        _ => 0,
    }
}

/// A pair of pods with the probe at `probe_ms` on both. Pod 2 reaches Redis
/// through the forwarder (its command connection and all its subscribers);
/// pod 1 reaches it directly.
async fn pod2_behind(probe_ms: u64) -> (RedisBlackout, TestApp, TestApp) {
    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let via = fwd.url_for(&test_redis_url());
    let (app1, app2) = TestApp::spawn_pair(move |s| {
        s.redis.subscriber_probe_ms = probe_ms;
        if s.app
            .pod_id
            .as_deref()
            .is_some_and(|p| p.starts_with("testpod2-"))
        {
            s.redis.url = via.clone();
        }
    })
    .await;
    (fwd, app1, app2)
}

/// Both pods' buses, a tenant whose admin posts on pod 1, and the socket of
/// its member on pod 2, which hears those posts only through pod 2's fan-out
/// subscriber.
struct CrossPod {
    bus1: Arc<PodBus>,
    bus2: Arc<PodBus>,
    tenant: SeededTenant,
    member_ws: Ws,
}

/// Wait for every subscriber of both pods, then prove a post crosses from
/// pod 1 to the member on pod 2. `None`: no Redis.
async fn cross_pod(app1: &TestApp, app2: &TestApp, tag: &str) -> Option<CrossPod> {
    let (Some(bus1), Some(bus2)) = (
        app1.state.cluster_bus.clone(),
        app2.state.cluster_bus.clone(),
    ) else {
        return None;
    };
    let flags = [
        app1.state.redis_sub_alive.clone(),
        app2.state.redis_sub_alive.clone(),
        bus1.sub_alive.clone(),
        bus2.sub_alive.clone(),
    ];
    let subscribing = Instant::now();
    while !flags.iter().all(|f| f.load(Ordering::Relaxed)) {
        assert!(
            subscribing.elapsed() < OUTER,
            "every subscriber of both pods subscribes"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let tenant = app1.seed_tenant(tag).await;
    join_room(app1, &tenant, &tenant.admin.access_token).await;
    join_room(app1, &tenant, &tenant.member.access_token).await;
    let mut member_ws = connect_ws(app2, &tenant.member.access_token).await;
    let _ = messages_within(&mut member_ws, Duration::from_millis(300)).await;
    let base = format!("{tag}-base");
    let crossing = Instant::now();
    let mut crossed = false;
    while !crossed && crossing.elapsed() < OUTER {
        assert_eq!(
            post_message(app1, &tenant, &base).await,
            200,
            "control: a post answers"
        );
        crossed = messages_within(&mut member_ws, Duration::from_secs(1))
            .await
            .contains(&base);
    }
    assert!(
        crossed,
        "control: a post on pod 1 reaches the member on pod 2 while Redis answers"
    );
    Some(CrossPod {
        bus1,
        bus2,
        tenant,
        member_ws,
    })
}

/// Make pod 2's fan-out subscriber deaf: Redis stops answering pod 2, a post
/// on pod 1 meanwhile is published to pod 2's subscription, whose connection
/// drops it and never forwards again, and Redis answers again about 2 s
/// later. Returns when it answers again.
async fn deafen_pod2_fanout(
    fwd: &RedisBlackout,
    app1: &TestApp,
    pair: &CrossPod,
    tag: &str,
) -> Instant {
    fwd.swallow();
    assert_eq!(
        post_message(app1, &pair.tenant, &format!("{tag}-during")).await,
        200,
        "pod 1 reaches Redis directly, so its post answers"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    fwd.pass();
    Instant::now()
}

/// Post `<prefix>-<n>` on pod 1 every `PACE` for up to `window` from `from`,
/// reading the member's socket on pod 2 between posts. Returns how long after
/// `from` the first of them was read (`None`: none in the window) and every
/// content the member received; after a first arrival it posts three more, so
/// a second delivery of anything would show.
async fn post_until_heard(
    app1: &TestApp,
    pair: &mut CrossPod,
    prefix: &str,
    from: Instant,
    window: Duration,
) -> (Option<Duration>, Vec<String>) {
    let (mut first, mut seen, mut more, mut n) = (None, Vec::new(), 0, 0u32);
    while from.elapsed() < window && more < 3 {
        n += 1;
        assert_eq!(
            post_message(app1, &pair.tenant, &format!("{prefix}-{n}")).await,
            200,
            "pod 1 reaches Redis directly, so its post answers"
        );
        for content in messages_within(&mut pair.member_ws, PACE).await {
            if first.is_none() && content.starts_with(prefix) {
                first = Some(from.elapsed());
            }
            seen.push(content);
        }
        if first.is_some() {
            more += 1;
        }
    }
    (first, seen)
}

/// Reads flags every [`SAMPLE`] from `from` until finished.
struct Sampler {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<(u64, u64, Option<Duration>)>,
}

impl Sampler {
    fn start(flags: Vec<Arc<AtomicBool>>, from: Instant) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn({
            let stop = stop.clone();
            async move {
                let (mut samples, mut falses, mut first_false) = (0u64, 0u64, None);
                while !stop.load(Ordering::SeqCst) {
                    samples += 1;
                    if !flags.iter().all(|f| f.load(Ordering::Relaxed)) {
                        falses += 1;
                        if first_false.is_none() {
                            first_false = Some(from.elapsed());
                        }
                    }
                    tokio::time::sleep(SAMPLE).await;
                }
                (samples, falses, first_false)
            }
        });
        Self { stop, task }
    }

    /// The samples, how many read a flag false, and when the first false one
    /// was taken.
    async fn finish(self) -> (u64, u64, Option<Duration>) {
        self.stop.store(true, Ordering::SeqCst);
        self.task.await.expect("the sampler ends")
    }
}

/// A listener that takes every connection, keeps what it is sent, and never
/// answers or closes: a Redis that accepts and then says nothing.
struct Silent {
    addr: SocketAddr,
    accepts: Arc<AtomicU64>,
    seen: Arc<Mutex<Vec<u8>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Silent {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the silent listener");
        let addr = listener.local_addr().expect("its address");
        let accepts = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn({
            let (accepts, seen) = (accepts.clone(), seen.clone());
            async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        continue;
                    };
                    accepts.fetch_add(1, Ordering::SeqCst);
                    let seen = seen.clone();
                    tokio::spawn(async move {
                        use tokio::io::AsyncReadExt;
                        let mut buf = [0u8; 4096];
                        // Read until the client goes; never write, never close first.
                        while let Ok(n) = stream.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                            // A std mutex, held for the copy only.
                            seen.lock().expect("seen").extend_from_slice(&buf[..n]);
                        }
                    });
                }
            }
        });
        Self {
            addr,
            accepts,
            seen,
            task,
        }
    }

    fn url(&self) -> String {
        format!("redis://{}", self.addr)
    }

    fn accepts(&self) -> u64 {
        self.accepts.load(Ordering::SeqCst)
    }

    /// Whether a client sent `word` (RESP carries each argument as is).
    fn saw(&self, word: &str) -> bool {
        String::from_utf8_lossy(&self.seen.lock().expect("seen"))
            .to_ascii_uppercase()
            .contains(word)
    }
}

impl Drop for Silent {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One fresh allocation's client-leg total of `sentb` bytes, published on
/// coturn's channel the way coturn does.
async fn publish_total(coturn: &RedisPubSub, principal: &str, allocation: u64, sentb: u64) {
    let (client, _) = channels(&format!("1792000000:{principal}"), allocation);
    coturn
        .publish_to(&client, &format!("rcvp=1, rcvb=1, sentp=1, sentb={sentb}"))
        .await
        .expect("the test publishes to Redis directly");
}

// ── Box 1: a deaf subscriber is found and subscribes again ──────────────────

/// T1 (boxes 1 and 4). Pod 2's fan-out subscriber goes deaf while its socket
/// stays open. Within the bound after Redis answers again it is found deaf
/// and subscribes again, so a post from pod 1 reaches the member on pod 2;
/// meanwhile `redis_sub_alive`, which `/health/ready` reads, reads false from
/// the verdict to the new subscription. Nothing arrives twice, and nothing
/// lost in the gap comes back. Q1: the deaf connection is closed, so the
/// forwarder's live connections come back to their count before the blackout.
#[tokio::test]
async fn a_deaf_fanout_subscriber_is_found_and_hears_again() {
    let (fwd, app1, app2) = pod2_behind(FLOOR_MS).await;
    let Some(mut pair) = cross_pod(&app1, &app2, "w18t1").await else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let live_before = fwd.live();
    let accepted_before = fwd.accepted();
    let passed = deafen_pod2_fanout(&fwd, &app1, &pair, "w18t1").await;
    let sampler = Sampler::start(vec![app2.state.redis_sub_alive.clone()], passed);
    let (first, seen) = post_until_heard(&app1, &mut pair, "w18t1-after", passed, WINDOW).await;
    let (samples, falses, first_false) = sampler.finish().await;
    let settling = Instant::now();
    while first.is_some()
        && fwd.live() != live_before
        && settling.elapsed() < Duration::from_secs(5)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eprintln!(
        "W18-RESULT T1 fan-out: first post heard {first:?} after Redis answered again; \
         redis_sub_alive false at {falses}/{samples} samples (first at {first_false:?}); \
         forwarder live {live_before} before the blackout, {} after; accepted {accepted_before} \
         before, {} after",
        fwd.live(),
        fwd.accepted()
    );
    let Some(first) = first else {
        panic!(
            "W18-RED no post from pod 1 reached the member on pod 2 in {WINDOW:?} after Redis \
             answered again: pod 2's fan-out subscriber went deaf and nothing noticed \
             (redis_sub_alive read true at {}/{samples} samples)",
            samples - falses
        );
    };
    assert!(
        first <= FOUND,
        "the first post was heard {first:?} after Redis answered again; the bound is {FOUND:?}"
    );
    assert!(
        first_false.is_some_and(|f| f < first),
        "redis_sub_alive reads false from the verdict to the new subscription, so at least one \
         sample before the first post was heard ({first_false:?}, heard at {first:?})"
    );
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for content in &seen {
        *counts.entry(content.as_str()).or_default() += 1;
    }
    assert!(
        counts
            .iter()
            .all(|(c, n)| *n == 1 || !c.starts_with("w18t1-after")),
        "at most once: nothing arrives twice ({counts:?})"
    );
    assert!(
        !counts.contains_key("w18t1-during"),
        "what the deaf connection dropped is never replayed"
    );
    assert_eq!(
        fwd.live(),
        live_before,
        "Q1: the deaf subscription's connection is closed when it is dropped"
    );
}

/// T2 (box 1). Pod 2's bus subscriber goes deaf: a ping from pod 1 during the
/// blackout is published to pod 2's channel and dropped on its connection.
/// Within the bound after Redis answers again pod 2 subscribes again, so a
/// ping from pod 1 is answered.
#[tokio::test]
async fn a_deaf_bus_subscriber_is_found_and_answers_again() {
    let (fwd, app1, app2) = pod2_behind(FLOOR_MS).await;
    let (Some(b1), Some(b2)) = (
        app1.state.cluster_bus.clone(),
        app2.state.cluster_bus.clone(),
    ) else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let subscribing = Instant::now();
    while !(b1.sub_alive.load(Ordering::Relaxed) && b2.sub_alive.load(Ordering::Relaxed)) {
        assert!(subscribing.elapsed() < OUTER, "both pod buses subscribe");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let control = b1
        .request(b2.pod_id(), "sys.ping", serde_json::json!({ "n": 0 }))
        .await
        .expect("control: pod 2 answers a ping while Redis answers");
    assert_eq!(control["pong"]["n"], 0);

    fwd.swallow();
    let lost = b1
        .request_with_deadline(
            b2.pod_id(),
            "sys.ping",
            serde_json::json!({ "n": 1 }),
            Duration::from_secs(1),
        )
        .await;
    assert!(
        matches!(lost, Err(BusError::Deadline(_))),
        "a ping while Redis does not answer pod 2 meets its deadline: {lost:?}"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    fwd.pass();
    let passed = Instant::now();
    let sampler = Sampler::start(vec![b2.sub_alive.clone()], passed);
    let (mut first, mut asked, mut n) = (None, 0u32, 1u32);
    while first.is_none() && passed.elapsed() < WINDOW {
        n += 1;
        asked += 1;
        let reply = b1
            .request_with_deadline(
                b2.pod_id(),
                "sys.ping",
                serde_json::json!({ "n": n }),
                Duration::from_secs(1),
            )
            .await;
        if let Ok(v) = reply {
            assert_eq!(v["pong"]["n"], n, "the answer is to this ping");
            first = Some(passed.elapsed());
        } else {
            tokio::time::sleep(PACE).await;
        }
    }
    let (samples, falses, first_false) = sampler.finish().await;
    eprintln!(
        "W18-RESULT T2 bus: first ping answered {first:?} after Redis answered again, of \
         {asked} sent; pod 2's sub_alive false at {falses}/{samples} samples (first at \
         {first_false:?})"
    );
    let Some(first) = first else {
        panic!(
            "W18-RED no ping from pod 1 was answered by pod 2 in {WINDOW:?} after Redis answered \
             again ({asked} sent, each to its 1 s deadline): pod 2's bus subscriber went deaf and \
             nothing noticed (sub_alive read true at {}/{samples} samples)",
            samples - falses
        );
    };
    assert!(
        first <= FOUND,
        "the first ping was answered {first:?} after Redis answered again; the bound is \
         {FOUND:?}"
    );
}

/// T3 (box 1). One server reaches Redis through the forwarder with TURN
/// attribution on. A total published during the blackout is dropped on its
/// TURN usage subscription. Within the bound after Redis answers again it
/// subscribes again, so a fresh allocation's total is booked.
#[tokio::test]
async fn a_deaf_turn_usage_subscriber_is_found_and_books_again() {
    let fwd = RedisBlackout::start(&test_redis_url()).await;
    let via = fwd.url_for(&test_redis_url());
    let app = TestApp::spawn_with_settings(move |s| {
        s.redis.url = via;
        s.redis.subscriber_probe_ms = FLOOR_MS;
        s.relay_allowance.turn_attribution = true;
    })
    .await;
    if app.state.redis_pubsub.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let Ok(coturn) = RedisPubSub::new(&test_redis_url(), unique("w18-t3/")).await else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let t = app.seed_tenant("w18t3").await;
    let org = ObjectId::parse_str(&t.tenant_id).unwrap();
    let principal = charged(&ObjectId::new().to_hex(), &org.to_hex());
    let mut allocation = 0u64;

    // Control: a total books while Redis answers (the subscription is
    // asynchronous, so publish until one lands).
    let booking = Instant::now();
    while turn_bytes(&app, org).await == 0 {
        assert!(
            booking.elapsed() < OUTER,
            "control: a total books while Redis answers"
        );
        allocation += 1;
        publish_total(&coturn, &principal, allocation, 1_000).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let booked = turn_bytes(&app, org).await;

    fwd.swallow();
    allocation += 1;
    publish_total(&coturn, &principal, allocation, 1_000).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    fwd.pass();
    let passed = Instant::now();
    let mut first = None;
    while first.is_none() && passed.elapsed() < WINDOW {
        allocation += 1;
        publish_total(&coturn, &principal, allocation, 1_000).await;
        tokio::time::sleep(PACE).await;
        if turn_bytes(&app, org).await > booked {
            first = Some(passed.elapsed());
        }
    }
    eprintln!(
        "W18-RESULT T3 TURN usage: first total booked {first:?} after Redis answered again; \
         {booked} bytes booked before the blackout, {} at the end",
        turn_bytes(&app, org).await
    );
    let Some(first) = first else {
        panic!(
            "W18-RED no TURN total published in {WINDOW:?} after Redis answered again was \
             booked: the TURN usage subscriber went deaf and nothing noticed"
        );
    };
    assert!(
        first <= FOUND,
        "the first total booked {first:?} after Redis answered again; the bound is {FOUND:?}"
    );
}

// ── Box 2: a subscribe that gets no answer gives up and tries again ─────────

/// T4 (box 2). The fan-out subscriber points at a listener that takes the
/// connection and never answers; its publisher is the real Redis with
/// #3840's defaults. It gives up within the connection timeout and tries
/// again. Q2: which step a subscriber with no bound waits in.
#[tokio::test]
async fn a_fanout_subscribe_to_a_silent_listener_gives_up_and_retries() {
    let Ok(real) = RedisPubSub::new(&test_redis_url(), unique("w18-t4/")).await else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let silent = Silent::start().await;
    let (tx, _rx) = tokio::sync::broadcast::channel::<String>(16);
    let alive = Arc::new(AtomicBool::new(false));
    RedisPubSub::subscribe_with_reconnect(
        silent.url(),
        tx,
        alive.clone(),
        real.connection(),
        ProbeSettings::default(),
    );
    tokio::time::sleep(SILENT).await;
    let accepts = silent.accepts();
    eprintln!(
        "W18-RESULT T4 fan-out against a silent listener: {accepts} connections in {SILENT:?}; \
         Q2: the listener was sent CLIENT SETINFO: {}, SUBSCRIBE: {}",
        silent.saw("SETINFO"),
        silent.saw("SUBSCRIBE")
    );
    assert!(
        accepts >= 2,
        "W18-RED the fan-out subscriber connected {accepts} time(s) in {SILENT:?} to a listener \
         that never answers: nothing bounds its connect or its subscribe, so it waits there for \
         as long as the socket lives"
    );
    assert!(
        !alive.load(Ordering::Relaxed),
        "nothing answered, so nothing is subscribed"
    );
}

/// T5 (box 2). The same through the pod bus.
#[tokio::test]
async fn a_bus_subscribe_to_a_silent_listener_gives_up_and_retries() {
    let Ok(real) = RedisPubSub::new(&test_redis_url(), unique("w18-t5/")).await else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let silent = Silent::start().await;
    let bus = PodBus::start(
        PodIdentity::resolve(Some(unique("w18pod-"))),
        real.connection(),
        silent.url(),
        ProbeSettings::default(),
    );
    tokio::time::sleep(SILENT).await;
    let accepts = silent.accepts();
    eprintln!(
        "W18-RESULT T5 bus against a silent listener: {accepts} connections in {SILENT:?}; the \
         listener was sent CLIENT SETINFO: {}, SUBSCRIBE: {}",
        silent.saw("SETINFO"),
        silent.saw("SUBSCRIBE")
    );
    assert!(
        accepts >= 2,
        "W18-RED the pod bus subscriber connected {accepts} time(s) in {SILENT:?} to a listener \
         that never answers: nothing bounds its connect or its subscribe"
    );
    assert!(
        !bus.sub_alive.load(Ordering::Relaxed),
        "nothing answered, so nothing is subscribed"
    );
}

// ── Box 3: a healthy Redis loses no probe and subscribes nothing again ───────

struct Healthy {
    tag: &'static str,
    probe_ms: u64,
    run_for: Duration,
    /// One reply chunk held this long ...
    delay: Duration,
    /// ... this often.
    delay_every: Duration,
    /// The traffic's cadence: a post, a ping each way, every third a total.
    pace: Duration,
    misses_allowed: bool,
    q3: bool,
}

/// Both pods behind a forwarder that passes everything, with one reply held
/// now and then, and real traffic on all three subscribers, plus the test's
/// own fan-out subscriber, whose every message is what a pod's fan-out loop
/// would read.
async fn healthy(cfg: Healthy) {
    let logged = Logged::default();
    let _log = logged.install_debug();
    let fwd = Arc::new(RedisBlackout::start(&test_redis_url()).await);
    let via = fwd.url_for(&test_redis_url());
    let probe_ms = cfg.probe_ms;
    let (app1, app2) = TestApp::spawn_pair({
        let via = via.clone();
        move |s| {
            s.redis.url = via.clone();
            s.redis.subscriber_probe_ms = probe_ms;
            s.relay_allowance.turn_attribution = true;
        }
    })
    .await;
    let Some(mut pair) = cross_pod(&app1, &app2, cfg.tag).await else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let Ok(coturn) = RedisPubSub::new(&test_redis_url(), unique("w18-coturn/")).await else {
        eprintln!("skipping: no Redis available");
        return;
    };

    let (tx, mut rx) = tokio::sync::broadcast::channel::<String>(4096);
    let own_alive = Arc::new(AtomicBool::new(false));
    RedisPubSub::subscribe_with_reconnect(
        via.clone(),
        tx,
        own_alive.clone(),
        app1.state
            .redis_pubsub
            .as_ref()
            .expect("pod 1 reaches Redis")
            .connection(),
        probe(cfg.probe_ms),
    );
    let not_envelopes = Arc::new(AtomicU64::new(0));
    let reader = tokio::spawn({
        let not_envelopes = not_envelopes.clone();
        async move {
            loop {
                match rx.recv().await {
                    // Every envelope on the channel is a JSON object.
                    Ok(payload) => {
                        if !serde_json::from_str::<serde_json::Value>(&payload)
                            .is_ok_and(|v| v.is_object())
                        {
                            not_envelopes.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    });
    let subscribing = Instant::now();
    while !own_alive.load(Ordering::Relaxed) {
        assert!(
            subscribing.elapsed() < OUTER,
            "the test's own fan-out subscriber subscribes"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let org = ObjectId::parse_str(&pair.tenant.tenant_id).unwrap();
    let principal = charged(&ObjectId::new().to_hex(), &org.to_hex());
    let mut allocation = 0u64;
    let booking = Instant::now();
    while turn_bytes(&app1, org).await == 0 {
        assert!(
            booking.elapsed() < OUTER,
            "control: a total books while Redis answers"
        );
        allocation += 1;
        publish_total(&coturn, &principal, allocation, 1_000).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let booked_before = turn_bytes(&app1, org).await;

    let accepted_setup = fwd.accepted();
    let started = Instant::now();
    let sampler = Sampler::start(
        vec![
            app1.state.redis_sub_alive.clone(),
            app2.state.redis_sub_alive.clone(),
            pair.bus1.sub_alive.clone(),
            pair.bus2.sub_alive.clone(),
            own_alive.clone(),
        ],
        started,
    );
    let delayer = tokio::spawn({
        let (fwd, delay, every) = (fwd.clone(), cfg.delay, cfg.delay_every);
        async move {
            loop {
                tokio::time::sleep(every).await;
                fwd.delay_next_reply(delay);
            }
        }
    });
    let (mut posted, mut heard) = (Vec::new(), HashMap::<String, u32>::new());
    let (mut pings, mut answered, mut published, mut n) = (0u32, 0u32, 0i64, 0u32);
    while started.elapsed() < cfg.run_for {
        n += 1;
        let content = format!("{}-{n}", cfg.tag);
        assert_eq!(
            post_message(&app1, &pair.tenant, &content).await,
            200,
            "every post answers"
        );
        posted.push(content);
        for (from, to) in [(&pair.bus1, &pair.bus2), (&pair.bus2, &pair.bus1)] {
            pings += 1;
            if let Ok(v) = from
                .request_with_deadline(
                    to.pod_id(),
                    "sys.ping",
                    serde_json::json!({ "n": n }),
                    Duration::from_secs(5),
                )
                .await
                && v["pong"]["n"] == n
            {
                answered += 1;
            }
        }
        if n % 3 == 0 {
            allocation += 1;
            publish_total(&coturn, &principal, allocation, 1_000).await;
            published += 1_000;
        }
        for content in messages_within(&mut pair.member_ws, cfg.pace).await {
            *heard.entry(content).or_default() += 1;
        }
    }
    for content in messages_within(&mut pair.member_ws, Duration::from_secs(3)).await {
        *heard.entry(content).or_default() += 1;
    }
    let landing = Instant::now();
    while turn_bytes(&app1, org).await < booked_before + published
        && landing.elapsed() < Duration::from_secs(5)
    {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let booked = turn_bytes(&app1, org).await - booked_before;
    delayer.abort();
    let (samples, falses, first_false) = sampler.finish().await;
    reader.abort();

    let text = logged.text();
    let count = |needle: &str| text.lines().filter(|l| l.contains(needle)).count();
    let misses = count(MISS_LINE);
    let probe_in_turn = text
        .lines()
        .filter(|l| l.contains("not a coturn total") && l.contains("sub-probe"))
        .count();
    let heard_once = posted.iter().filter(|c| heard.get(*c) == Some(&1)).count();
    eprintln!(
        "W18-RESULT {} healthy at P = {} ms for {:?}: posts {heard_once}/{} heard once; pings \
         {answered}/{pings}; totals {booked}/{published} bytes booked; accepted {accepted_setup} \
         at setup, {} at the end; a flag false at {falses}/{samples} samples (first at \
         {first_false:?}); misses {misses}; non-envelope fan-out messages {}; probe in TURN \
         usage's parser {probe_in_turn}",
        cfg.tag,
        cfg.probe_ms,
        cfg.run_for,
        posted.len(),
        fwd.accepted(),
        not_envelopes.load(Ordering::SeqCst),
    );
    if cfg.q3 {
        let mut gaps = reply_against_arrival(20).await;
        gaps.sort_unstable();
        eprintln!(
            "W18-RESULT Q3 arrival minus PUBLISH reply over {} PUBLISHes: min {} us, median {} \
             us, max {} us; {} arrived before their reply",
            gaps.len(),
            gaps[0],
            gaps[gaps.len() / 2],
            gaps[gaps.len() - 1],
            gaps.iter().filter(|g| **g < 0).count()
        );
    }
    for needle in NEVER_ON_A_HEALTHY_REDIS {
        assert_eq!(count(needle), 0, "no `{needle}` line on a healthy Redis");
    }
    assert_eq!(
        probe_in_turn, 0,
        "a probe never reaches TURN usage's parser"
    );
    assert_eq!(
        not_envelopes.load(Ordering::SeqCst),
        0,
        "a probe is never handed on as a fan-out message"
    );
    if !cfg.misses_allowed {
        assert_eq!(misses, 0, "no probe goes unheard on a healthy Redis");
    }
    assert_eq!(
        fwd.accepted(),
        accepted_setup,
        "no new connection: nothing subscribed again and nothing was replaced"
    );
    assert_eq!(
        falses, 0,
        "every subscriber's flag read true at every sample (first false at {first_false:?})"
    );
    assert_eq!(heard_once, posted.len(), "every post heard, once");
    assert_eq!(answered, pings, "every ping answered");
    assert_eq!(booked, published, "every total booked, once");
}

/// Q3: how long a subscriber's message trails the PUBLISH's reply, in
/// microseconds (negative: it arrived first), on a direct connection.
async fn reply_against_arrival(samples: usize) -> Vec<i64> {
    use futures::StreamExt;
    let client = redis::Client::open(test_redis_url()).expect("the test Redis URL");
    let mut subscriber = client.get_async_pubsub().await.expect("subscribe");
    let channel = unique("roomler:test:w18:q3:");
    subscriber.subscribe(&channel).await.expect("subscribe");
    let mut publisher = client
        .get_multiplexed_async_connection()
        .await
        .expect("publisher");
    let mut stream = subscriber.on_message();
    let mut gaps = Vec::with_capacity(samples);
    for i in 0..samples {
        let sent = Instant::now();
        let publish = async {
            let _: i64 = redis::cmd("PUBLISH")
                .arg(&channel)
                .arg(i)
                .query_async(&mut publisher)
                .await
                .expect("PUBLISH");
            sent.elapsed()
        };
        let arrive = async {
            let _ = stream.next().await;
            sent.elapsed()
        };
        let (reply, arrival) = tokio::join!(publish, arrive);
        gaps.push(arrival.as_micros() as i64 - reply.as_micros() as i64);
    }
    gaps
}

/// C1 (box 3), the no-false-reconnect control at the production interval:
/// three minutes of posts, pings and totals with one reply held 1.5 s every
/// 37 s. No probe goes unheard, nothing subscribes again, no connection is
/// added, every flag stays true, and no probe reaches a subscriber's own
/// handling. Q3: the PUBLISH reply against the subscriber's arrival.
#[tokio::test]
async fn a_healthy_redis_loses_no_probe_and_subscribes_nothing_again() {
    healthy(Healthy {
        tag: "w18c1",
        probe_ms: 15_000,
        run_for: Duration::from_secs(180),
        delay: Duration::from_millis(1_500),
        delay_every: Duration::from_secs(37),
        pace: Duration::from_millis(1_300),
        misses_allowed: false,
        q3: true,
    })
    .await;
}

/// C2 (box 3), the same at the floor interval for two minutes, with one reply
/// held 500 ms every 2.3 s: no verdict and no new connection. It prints the
/// misses.
#[tokio::test]
async fn at_the_floor_interval_a_healthy_redis_sees_no_verdict() {
    healthy(Healthy {
        tag: "w18c2",
        probe_ms: FLOOR_MS,
        run_for: Duration::from_secs(120),
        delay: Duration::from_millis(500),
        delay_every: Duration::from_millis(2_300),
        pace: Duration::from_millis(1_100),
        misses_allowed: true,
        q3: false,
    })
    .await;
}

// ── AC7: the kill switch ────────────────────────────────────────────────────

/// C3 (AC7). T1's scenario with the probe at 0: the deaf fan-out subscriber
/// stays deaf as before #3952, `redis_sub_alive` stays true, and nothing
/// probes.
#[tokio::test]
async fn with_the_probe_at_0_a_deaf_subscriber_stays_deaf_as_before() {
    let logged = Logged::default();
    let _log = logged.install_debug();
    let (fwd, app1, app2) = pod2_behind(0).await;
    let Some(mut pair) = cross_pod(&app1, &app2, "w18c3").await else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let passed = deafen_pod2_fanout(&fwd, &app1, &pair, "w18c3").await;
    let sampler = Sampler::start(vec![app2.state.redis_sub_alive.clone()], passed);
    let (first, _) = post_until_heard(&app1, &mut pair, "w18c3-after", passed, WINDOW).await;
    let (samples, falses, _) = sampler.finish().await;
    let text = logged.text();
    let probe_lines = text
        .lines()
        .filter(|l| {
            l.contains(MISS_LINE)
                || l.contains("stopped hearing its probe")
                || l.contains("probe channel was refused")
        })
        .count();
    eprintln!(
        "W18-RESULT C3 probe off: first post heard {first:?}; redis_sub_alive false at \
         {falses}/{samples} samples; probe lines {probe_lines}"
    );
    assert!(
        first.is_none(),
        "with the probe off a deaf fan-out subscriber stays deaf, as before #3952, yet a post \
         was heard {first:?} after Redis answered again"
    );
    assert_eq!(
        falses, 0,
        "with the probe off nothing changes redis_sub_alive"
    );
    assert_eq!(probe_lines, 0, "with the probe off nothing probes");
}

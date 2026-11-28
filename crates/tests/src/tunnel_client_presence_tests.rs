// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4553 — a tunnel client's presence comes from its live socket, read the
//! way an agent's is (Phase A-1): a socket on this pod, the cross-pod
//! presence record in Redis, and the heartbeat trail when the directory
//! cannot answer. Nothing ever writes `online` to the row's `status`, so a
//! list that read that stored field showed every tunnel client offline.
//!
//! Two pods as `agent_presence_tests` models them: `TestApp::spawn_pair` runs
//! two servers over ONE database and the one Redis, each with its own
//! pod-local socket registry. Both lists are read: the unified device list
//! (`GET …/device`) and the tunnel-client list (`GET …/tunnel-client`).
//!
//! #4571 — so a client that vanished without closing its socket (a laptop
//! that slept, a NAT that dropped its mapping) must not read online for as
//! long as that socket stays open: the server pings the socket and ends one
//! that has delivered nothing for three periods, through its normal teardown.
//! The last two cells run that keepalive at one second.

use std::time::Duration;

use bson::{doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// How long a list may take to show a socket opening or closing.
const SETTLE: Duration = Duration::from_secs(5);

/// Both pods with their per-address rate limit out of the way: the lists are
/// read until they settle, more requests than the default burst, and a 429
/// would read as an answer.
fn unlimited(s: &mut roomler_ai_config::Settings) {
    s.app.rate_limit_per_sec = 1_000;
    s.app.rate_limit_burst = 10_000;
}

async fn enroll_client(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> (String, String) {
    let et: Value = app
        .auth_post(
            &format!(
                "/api/tenant/{}/tunnel-client/enroll-token",
                seeded.tenant_id
            ),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let enrolled: Value = app
        .client
        .post(app.url("/api/tunnel-client/enroll"))
        .json(&json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": machine_id,
            "machine_name": "presence test laptop",
            "os": "linux",
            "client_version": "0.3.0-test",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        enrolled["tunnel_client_id"].as_str().unwrap().to_string(),
        enrolled["tunnel_client_token"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

async fn connect_client(app: &TestApp, token: &str) -> Ws {
    let url = format!(
        "ws://{}/ws?token={}&role=tunnel-client",
        app.addr,
        token
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    );
    let (ws, _) = connect_async(&url).await.expect("tunnel-client ws");
    ws
}

/// A list as `app` serves it. Any answer but a 2xx fails here, with its
/// status and body, rather than as a body that does not parse.
async fn read_list(app: &TestApp, seeded: &SeededTenant, path: &str) -> Value {
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/{path}?per_page=100", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert!(status.is_success(), "GET {path}: {status} {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("GET {path}: {e} in {body}"))
}

/// The client's row in the device list and in the tunnel-client list, as
/// `app` serves them (`Value::Null` for a list that does not hold it).
async fn rows(app: &TestApp, seeded: &SeededTenant, client_id: &str) -> (Value, Value) {
    let device = read_list(app, seeded, "device").await;
    let clients = read_list(app, seeded, "tunnel-client").await;
    let find = |list: &Value| {
        list["items"]
            .as_array()
            .and_then(|items| items.iter().find(|r| r["id"] == client_id).cloned())
            .unwrap_or(Value::Null)
    };
    (find(&device), find(&clients))
}

/// Read both lists on `app` until both say `want` for the client, or
/// [`SETTLE`] passes; then assert what was read last.
async fn expect_presence(
    app: &TestApp,
    seeded: &SeededTenant,
    client_id: &str,
    want: &str,
    what: &str,
) {
    expect_presence_within(app, seeded, client_id, want, what, SETTLE).await;
}

/// [`expect_presence`], waiting at most `within`.
async fn expect_presence_within(
    app: &TestApp,
    seeded: &SeededTenant,
    client_id: &str,
    want: &str,
    what: &str,
    within: Duration,
) {
    let deadline = tokio::time::Instant::now() + within;
    let read = loop {
        let read = rows(app, seeded, client_id).await;
        let settled = [&read.0, &read.1].iter().all(|r| r["presence"] == want);
        if settled || tokio::time::Instant::now() >= deadline {
            break read;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    for (list, row) in [("device", &read.0), ("tunnel-client", &read.1)] {
        assert_eq!(row["presence"], want, "{what}: the {list} list reads {row}");
        assert_eq!(
            row["is_online"],
            want == "online",
            "{what}: the {list} list reads {row}"
        );
    }
}

/// A connected client reads online in both lists on the pod that holds its
/// socket and, through the Redis record, on the pod that does not. Once the
/// socket closes it reads offline on both: the record is gone, and a trail
/// that is fresh because it closed a moment ago says nothing more.
#[tokio::test]
async fn a_connected_client_reads_online_on_both_pods_and_offline_once_it_closes() {
    let (app1, app2) = TestApp::spawn_pair(unlimited).await;
    if app1.state.redis_pubsub.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let seeded = app1.seed_tenant("tcpres1").await;
    let (client_id, token) = enroll_client(&app1, &seeded, "mach-tcpres1").await;

    let mut ws = connect_client(&app1, &token).await;
    expect_presence(&app1, &seeded, &client_id, "online", "its own pod").await;
    expect_presence(&app2, &seeded, &client_id, "online", "the other pod").await;

    let _ = ws.close(None).await;
    expect_presence(&app1, &seeded, &client_id, "offline", "its own pod, closed").await;
    expect_presence(
        &app2,
        &seeded,
        &client_id,
        "offline",
        "the other pod, closed",
    )
    .await;
}

/// The two-pod race the stored `status` could not survive: the client
/// reconnects through the other pod, and the OLD socket closes after that.
/// The old pod's late close releases only its own claim, so the client reads
/// online on both pods until the socket it really holds closes.
#[tokio::test]
async fn a_late_close_on_the_old_pod_leaves_the_new_socket_online() {
    let (app1, app2) = TestApp::spawn_pair(unlimited).await;
    if app1.state.redis_pubsub.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let seeded = app1.seed_tenant("tcpres2").await;
    let (client_id, token) = enroll_client(&app1, &seeded, "mach-tcpres2").await;

    let mut old = connect_client(&app1, &token).await;
    expect_presence(&app2, &seeded, &client_id, "online", "the old socket").await;
    let mut new = connect_client(&app2, &token).await;
    expect_presence(&app1, &seeded, &client_id, "online", "both sockets").await;

    let _ = old.close(None).await;
    // Long enough for the old socket's teardown to have run: a release that
    // took the new socket's record with it would show here.
    tokio::time::sleep(Duration::from_millis(500)).await;
    expect_presence(
        &app1,
        &seeded,
        &client_id,
        "online",
        "the old pod, after its late close",
    )
    .await;
    expect_presence(
        &app2,
        &seeded,
        &client_id,
        "online",
        "the new pod, after the old close",
    )
    .await;

    let _ = new.close(None).await;
    expect_presence(
        &app1,
        &seeded,
        &client_id,
        "offline",
        "the old pod, both closed",
    )
    .await;
    expect_presence(
        &app2,
        &seeded,
        &client_id,
        "offline",
        "the new pod, both closed",
    )
    .await;
}

/// Two sockets of one client on ONE pod (two `roomler` processes on one
/// machine) stand behind one record. The newer socket closes first, as a
/// short `roomler ssh` beside a long-lived forward does: that takes nothing
/// from the older socket, which both pods still read online, and the last
/// socket out takes the client offline on both.
#[tokio::test]
async fn a_second_socket_closing_leaves_the_first_online_on_both_pods() {
    let (app1, app2) = TestApp::spawn_pair(unlimited).await;
    if app1.state.redis_pubsub.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let seeded = app1.seed_tenant("tcpres4").await;
    let (client_id, token) = enroll_client(&app1, &seeded, "mach-tcpres4").await;

    let mut first = connect_client(&app1, &token).await;
    expect_presence(&app2, &seeded, &client_id, "online", "the first socket").await;
    let mut second = connect_client(&app1, &token).await;
    // A list cannot tell one socket from two: give the second socket's
    // handler time to have taken its seat and claimed.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let _ = second.close(None).await;
    // Long enough for the second socket's teardown to have run: a release
    // that took the first socket's record with it would show here.
    tokio::time::sleep(Duration::from_millis(500)).await;
    expect_presence(
        &app2,
        &seeded,
        &client_id,
        "online",
        "the other pod, after the second socket closed",
    )
    .await;
    expect_presence(
        &app1,
        &seeded,
        &client_id,
        "online",
        "its own pod, after the second socket closed",
    )
    .await;

    let _ = first.close(None).await;
    expect_presence(
        &app2,
        &seeded,
        &client_id,
        "offline",
        "the other pod, both closed",
    )
    .await;
    expect_presence(
        &app1,
        &seeded,
        &client_id,
        "offline",
        "its own pod, both closed",
    )
    .await;
}

/// With no Redis a pod sees only its own sockets. The pod that holds the
/// socket reads it online; the other reads the heartbeat trail: stale while
/// the trail is fresh, never online, and offline once the trail is old.
#[tokio::test]
async fn without_redis_the_other_pod_reads_the_trail_as_stale_never_online() {
    let (app1, app2) = TestApp::spawn_pair(|s| {
        unlimited(s);
        s.redis.url = String::new();
    })
    .await;
    assert!(
        app1.state.redis_pubsub.is_none() && app2.state.redis_pubsub.is_none(),
        "this test models pods with no Redis"
    );
    let seeded = app1.seed_tenant("tcpres3").await;
    let (client_id, token) = enroll_client(&app1, &seeded, "mach-tcpres3").await;

    let mut ws = connect_client(&app1, &token).await;
    expect_presence(
        &app1,
        &seeded,
        &client_id,
        "online",
        "the pod with the socket",
    )
    .await;
    expect_presence(&app2, &seeded, &client_id, "stale", "a pod with no Redis").await;

    // The trail ages out: the other pod has nothing left to go on.
    app1.db
        .collection::<bson::Document>("tunnel_clients")
        .update_one(
            doc! { "_id": ObjectId::parse_str(&client_id).unwrap() },
            doc! { "$set": { "last_seen_at": bson::DateTime::from_millis(
                bson::DateTime::now().timestamp_millis() - 10 * 60_000
            ) } },
        )
        .await
        .unwrap();
    expect_presence(&app2, &seeded, &client_id, "offline", "an old trail").await;
    expect_presence(
        &app1,
        &seeded,
        &client_id,
        "online",
        "the socket, whatever the trail",
    )
    .await;

    let _ = ws.close(None).await;
    expect_presence(
        &app1,
        &seeded,
        &client_id,
        "offline",
        "the pod whose socket closed",
    )
    .await;
}

// ─── #4571: the server's keepalive and receive deadline ──────────────────────

/// The period the cells below run the server's keepalive at, in place of its
/// 30 s (`rc.tunnel_ping_every`).
const PING_EVERY: Duration = Duration::from_secs(1);
/// Three periods with no frame from the client end its socket.
const DEADLINE: Duration = Duration::from_secs(3 * PING_EVERY.as_secs());

/// [`unlimited`], with the server's keepalive at [`PING_EVERY`] on both pods.
fn short_keepalive(s: &mut roomler_ai_config::Settings) {
    unlimited(s);
    s.rc.tunnel_ping_every = Some(PING_EVERY);
}

/// What a client's reading came to.
struct Reading {
    ws: Ws,
    /// The server's Pings it read. tungstenite answers each one as it reads it.
    pings: usize,
    /// The socket ended while the client read it.
    ended: bool,
}

/// Read `ws`, as every released `roomler` reads its socket, so that each
/// server Ping is answered, until `stop` fires or the socket ends; then hand
/// it back, read no more.
async fn read_until(mut ws: Ws, mut stop: oneshot::Receiver<()>) -> Reading {
    let mut pings = 0;
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => return Reading { ws, pings, ended: false },
            frame = ws.next() => match frame {
                Some(Ok(Message::Ping(_))) => pings += 1,
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    return Reading { ws, pings, ended: true };
                }
                Some(Ok(_)) => {}
            },
        }
    }
}

/// Send a Ping every [`PING_EVERY`] and read nothing, as `roomler`'s writer
/// does whatever its reader is doing, until `stop` fires. `false` once a Ping
/// could not be written.
async fn ping_until(mut ws: Ws, mut stop: oneshot::Receiver<()>) -> bool {
    let mut tick = tokio::time::interval(PING_EVERY);
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => return true,
            _ = tick.tick() => {
                if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return false;
                }
            }
        }
    }
}

/// A client that stops reading and sends nothing, without closing, as a
/// laptop that slept or a NAT that dropped its mapping leaves its socket. The
/// server ends that socket within the deadline, through the teardown a lost
/// link takes: both lists then read the client offline on both pods, and its
/// socket is closed.
#[tokio::test]
async fn a_client_gone_silent_is_ended_and_reads_offline_on_both_pods() {
    let (app1, app2) = TestApp::spawn_pair(short_keepalive).await;
    if app1.state.redis_pubsub.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let seeded = app1.seed_tenant("tcpres5").await;
    let (client_id, token) = enroll_client(&app1, &seeded, "mach-tcpres5").await;

    // While it reads, it answers the server's Pings and reads online.
    let (stop, stopped) = oneshot::channel();
    let reader = tokio::spawn(read_until(connect_client(&app1, &token).await, stopped));
    expect_presence(&app1, &seeded, &client_id, "online", "its own pod, reading").await;
    expect_presence(
        &app2,
        &seeded,
        &client_id,
        "online",
        "the other pod, reading",
    )
    .await;

    // Then it goes silent: it reads nothing more, so it answers nothing.
    let _ = stop.send(());
    let Reading { mut ws, ended, .. } = reader.await.unwrap();
    assert!(!ended, "the socket ended while the client still read it");

    expect_presence_within(
        &app1,
        &seeded,
        &client_id,
        "offline",
        "its own pod, the client silent past the deadline",
        DEADLINE + SETTLE,
    )
    .await;
    expect_presence(
        &app2,
        &seeded,
        &client_id,
        "offline",
        "the other pod, the client silent past the deadline",
    )
    .await;

    // The server closed it: what the client can still read (the Pings sent
    // before the end) runs out.
    let drained =
        tokio::time::timeout(SETTLE, async { while let Some(Ok(_)) = ws.next().await {} }).await;
    assert!(
        drained.is_ok(),
        "the server ended the client's presence but left its socket open"
    );
}

/// A healthy client is never ended by the deadline, though it sends no
/// message: one that reads, so it answers the server's Pings as every
/// released client does, and one that reads nothing and sends a Ping of its
/// own each period, as `roomler`'s writer does. Both read online on both pods
/// past three deadlines, and the one that reads was pinged all along.
#[tokio::test]
async fn a_client_that_answers_or_pings_stays_online_past_three_deadlines() {
    let (app1, app2) = TestApp::spawn_pair(short_keepalive).await;
    if app1.state.redis_pubsub.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    let seeded = app1.seed_tenant("tcpres6").await;
    let (reader_id, reader_token) = enroll_client(&app1, &seeded, "mach-tcpres6r").await;
    let (pinger_id, pinger_token) = enroll_client(&app1, &seeded, "mach-tcpres6p").await;

    let (stop_reader, stopped) = oneshot::channel();
    let reader = tokio::spawn(read_until(
        connect_client(&app1, &reader_token).await,
        stopped,
    ));
    let (stop_pinger, stopped) = oneshot::channel();
    let pinger = tokio::spawn(ping_until(
        connect_client(&app1, &pinger_token).await,
        stopped,
    ));
    let clients = [
        (&reader_id, "the client that reads"),
        (&pinger_id, "the client that pings"),
    ];
    let pods = [(&app1, "its own pod"), (&app2, "the other pod")];
    for (id, what) in clients {
        for (app, pod) in pods {
            expect_presence(app, &seeded, id, "online", &format!("{what}, {pod}")).await;
        }
    }

    let start = tokio::time::Instant::now();
    while start.elapsed() < 3 * DEADLINE + PING_EVERY {
        for (id, what) in clients {
            for (app, pod) in pods {
                let (device, tunnel_client) = rows(app, &seeded, id).await;
                for (list, row) in [("device", &device), ("tunnel-client", &tunnel_client)] {
                    assert!(
                        row["presence"] == "online" && row["is_online"].as_bool() == Some(true),
                        "{what}, {pod}, {:?} in: the {list} list reads {row}",
                        start.elapsed()
                    );
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let _ = stop_reader.send(());
    let _ = stop_pinger.send(());
    let read = reader.await.unwrap();
    assert!(
        !read.ended,
        "the server ended a client that answered its Pings"
    );
    assert!(
        pinger.await.unwrap(),
        "the server ended a client that sent its own Pings"
    );
    assert!(
        read.pings >= 3,
        "the server sent {} Pings in {:?}",
        read.pings,
        start.elapsed()
    );
}

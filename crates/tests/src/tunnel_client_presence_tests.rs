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

use std::time::Duration;

use bson::{doc, oid::ObjectId};
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// How long a list may take to show a socket opening or closing.
const SETTLE: Duration = Duration::from_secs(5);

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

/// The client's row in the device list and in the tunnel-client list, as
/// `app` serves them (`Value::Null` for a list that does not hold it).
async fn rows(app: &TestApp, seeded: &SeededTenant, client_id: &str) -> (Value, Value) {
    let device: Value = app
        .auth_get(
            &format!("/api/tenant/{}/device?per_page=100", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let clients: Value = app
        .auth_get(
            &format!(
                "/api/tenant/{}/tunnel-client?per_page=100",
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
    let deadline = tokio::time::Instant::now() + SETTLE;
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
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
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
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
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

/// With no Redis a pod sees only its own sockets. The pod that holds the
/// socket reads it online; the other reads the heartbeat trail: stale while
/// the trail is fresh, never online, and offline once the trail is old.
#[tokio::test]
async fn without_redis_the_other_pod_reads_the_trail_as_stale_never_online() {
    let (app1, app2) = TestApp::spawn_pair(|s| s.redis.url = String::new()).await;
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

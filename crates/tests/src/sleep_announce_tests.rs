// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-55 P4 — a device that says it is going to sleep reads "asleep", not a
//! bare "offline", until it says hello again.
//!
//! Just before the OS sleeps it, the agent sends `rc:agent.sleeping` with its
//! power posture and closes (`agents/roomlerd/src/sleep_notice.rs`). These lock
//! the server's half through the real socket: the goodbye ends the connection
//! at once (no wait for the receive deadline), the agent row and the devices
//! grid carry the reason and the posture, the dashboard's `device:presence`
//! offline row carries them too, the next hello erases them, and a connection
//! that no longer holds the row cannot write them.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::agent_presence_tests::{connect_agent, enroll};
use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::tunnel_tests::{wait_agent_offline, wait_agent_online};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn posture(policy: &str, on_ac: bool, held: bool) -> Value {
    json!({ "policy": policy, "on_ac": on_ac, "held": held })
}

async fn agent_row(app: &TestApp, seeded: &SeededTenant, agent_id: &str) -> Value {
    app.auth_get(
        &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, agent_id),
        &seeded.admin.access_token,
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap()
}

/// The device's row on the unified devices grid (`GET /tenant/{tid}/device`).
async fn device_row(app: &TestApp, seeded: &SeededTenant, agent_id: &str) -> Value {
    let page: Value = app
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
    page["items"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["id"] == agent_id))
        .cloned()
        .unwrap_or_else(|| panic!("{agent_id} is not on the devices grid: {page}"))
}

/// The next `device:presence` row for `agent_id` reading `presence`, on a
/// USER socket, within 5 s. Other events and other rows are skipped.
async fn presence_row(user_ws: &mut Ws, agent_id: &str, presence: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = tokio::time::timeout(left, user_ws.next())
            .await
            .unwrap_or_else(|_| panic!("no device:presence {presence} for {agent_id} within 5 s"))
            .expect("user ws ended")
            .expect("user ws frame");
        let Ok(v) = serde_json::from_str::<Value>(msg.to_text().unwrap_or_default()) else {
            continue;
        };
        if v["type"] != "device:presence" {
            continue;
        }
        if let Some(row) = v["data"]["agents"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|r| r["agent_id"] == agent_id && r["presence"] == presence)
            })
        {
            return row.clone();
        }
    }
}

/// The server ended the connection: a Close frame, or the stream ending,
/// within 5 s.
async fn closed_by_server(ws: &mut Ws) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), ws.next()).await {
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => return true,
            Ok(Some(Ok(_))) | Err(_) => continue,
        }
    }
    false
}

#[tokio::test]
async fn a_device_that_announces_its_sleep_reads_asleep_until_its_next_hello() {
    let app = TestApp::spawn_with_settings(|s| {
        s.rc.presence_batch_ms = 50;
    })
    .await;
    let seeded = app.seed_tenant("fr55sleep").await;
    let ws_url = format!("ws://{}/ws?token={}", app.addr, seeded.member.access_token);
    let (mut user_ws, _) = connect_async(&ws_url).await.expect("user ws connect");
    user_ws.next().await; // the `connected` frame

    let (agent_id, token) = enroll(&app, &seeded, "mach-fr55-sleep").await;
    let mut ws = connect_agent(&app, &token).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    presence_row(&mut user_ws, &agent_id, "online").await;

    // The heartbeat reports the posture. Online, there is no reason to show.
    let hb = json!({
        "t": "rc:agent.heartbeat", "rss_mb": 0, "cpu_pct": 0.0, "active_sessions": 0,
        "power": posture("on-ac", false, false),
    });
    ws.send(Message::Text(hb.to_string().into())).await.unwrap();
    let mut row = Value::Null;
    for _ in 0..100 {
        row = agent_row(&app, &seeded, &agent_id).await;
        if row["power"]["policy"] == "on-ac" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(row["power"]["policy"], "on-ac", "the heartbeat's posture: {row}");
    assert!(row.get("sleep").is_none(), "online: no reason to show: {row}");

    // The goodbye before a sleep ends the connection NOW: the device reads
    // offline at once, not after the receive deadline reaps a silent socket.
    let started = std::time::Instant::now();
    let goodbye = json!({ "t": "rc:agent.sleeping", "power": posture("on-ac", false, false) });
    ws.send(Message::Text(goodbye.to_string().into()))
        .await
        .unwrap();
    assert!(
        closed_by_server(&mut ws).await,
        "the goodbye ends the connection"
    );
    wait_agent_offline(&app, &seeded, &agent_id).await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "offline at once, not at the receive deadline: {:?}",
        started.elapsed()
    );

    // The dashboard's event says why, in the same row that turns it grey.
    let ev = presence_row(&mut user_ws, &agent_id, "offline").await;
    assert_eq!(ev["sleep"]["power"]["policy"], "on-ac", "{ev}");
    assert_eq!(ev["sleep"]["power"]["on_ac"], false, "{ev}");
    assert_eq!(ev["sleep"]["power"]["held"], false, "{ev}");
    assert!(
        ev["sleep"]["since"].as_str().is_some_and(|s| s.contains('T')),
        "RFC 3339: {ev}"
    );

    // So do the agent row and the devices grid.
    let row = agent_row(&app, &seeded, &agent_id).await;
    assert_eq!(row["presence"], "offline", "{row}");
    assert_eq!(row["sleep"]["power"]["policy"], "on-ac", "{row}");
    assert_eq!(row["sleep"]["power"]["held"], false, "{row}");
    assert_eq!(row["power"]["policy"], "on-ac", "{row}");
    let dev = device_row(&app, &seeded, &agent_id).await;
    assert_eq!(dev["presence"], "offline", "{dev}");
    assert_eq!(dev["sleep"]["power"]["policy"], "on-ac", "{dev}");
    assert_eq!(dev["sleep"]["since"], row["sleep"]["since"], "{dev}");

    // Awake again: the hello erases the reason. Proven by going offline
    // once more WITHOUT a word, which must read as a plain offline.
    let mut ws = connect_agent(&app, &token).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    let row = agent_row(&app, &seeded, &agent_id).await;
    assert!(row.get("sleep").is_none(), "awake: {row}");
    let _ = ws.close(None).await;
    wait_agent_offline(&app, &seeded, &agent_id).await;
    let row = agent_row(&app, &seeded, &agent_id).await;
    assert!(
        row.get("sleep").is_none(),
        "the hello erased the old reason: {row}"
    );
    assert!(device_row(&app, &seeded, &agent_id).await.get("sleep").is_none());
    let ev = presence_row(&mut user_ws, &agent_id, "offline").await;
    assert!(ev.get("sleep").is_none(), "no reason given, none sent: {ev}");
}

/// The notice is keyed by the connection that wrote the row's hello, like the
/// teardown's offline write (#2552): a displaced connection's late goodbye
/// can never mark a device that has already reconnected as asleep.
#[tokio::test]
async fn only_the_connection_that_said_hello_can_say_the_device_is_asleep() {
    use roomler_ai_remote_control::models::{AgentCaps, PowerBrief, SleepNotice};
    use roomler_ai_services::dao::agent::AgentDao;

    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr55conn").await;
    let (agent_id, _) = enroll(&app, &seeded, "mach-fr55-conn").await;
    let id = bson::oid::ObjectId::parse_str(&agent_id).unwrap();
    let dao = AgentDao::new(&app.db);
    let hello = |conn: &'static str| {
        let dao = &dao;
        async move {
            dao.update_hello(
                id,
                "0.4.188",
                &[],
                &AgentCaps::default(),
                &[],
                "",
                "",
                "",
                "",
                false,
                conn,
            )
            .await
            .unwrap();
        }
    };
    hello("the-connection").await;
    let notice = SleepNotice {
        at: bson::DateTime::now(),
        power: PowerBrief {
            policy: "never".into(),
            on_ac: None,
            held: false,
        },
    };
    let row = || async { dao.base.find_by_id(id).await.unwrap() };

    assert!(
        !dao.record_sleep(id, "a-displaced-connection", &notice)
            .await
            .unwrap(),
        "a connection that does not hold the row writes nothing"
    );
    assert!(row().await.sleep.is_none());

    assert!(
        dao.record_sleep(id, "the-connection", &notice)
            .await
            .unwrap()
    );
    let r = row().await;
    assert_eq!(r.sleep.as_ref().map(|s| &s.power), Some(&notice.power));
    assert_eq!(
        r.power.as_ref(),
        Some(&notice.power),
        "the moment of sleep is the freshest posture"
    );

    hello("the-next-connection").await;
    assert!(row().await.sleep.is_none(), "a hello erases the notice");
}

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-97 P0b — an approval is not an offer, end to end over two real agent
//! WebSockets.
//!
//! An admin's exit-node approval (`approved_routes` holding `0.0.0.0/0`)
//! survives a rejoin on purpose: the join refreshes only what the device
//! ADVERTISES. Before P0b the netmap fanned the approval alone, so a device
//! that stopped offering its `/0` (the operator turned the exit off, or the
//! device found it cannot NAT: #2077) stayed an exit in every peer's netmap,
//! and a client that had picked it lost its whole internet egress until an
//! admin noticed. This drives the real join → approve → rejoin-without-the-
//! offer → rejoin-with-it sequence and reads what the OTHER device is sent.

use crate::agent_presence_tests::enroll;
use crate::fixtures::{seed::SeededTenant, test_app::TestApp};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const DEFAULT_ROUTE: &str = "0.0.0.0/0";

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

async fn connect(app: &TestApp, token: &str) -> Ws {
    let ws_url = format!("ws://{}/ws?token={}&role=agent", app.addr, urlencode(token));
    let (mut ws, _) = connect_async(&ws_url).await.expect("ws connect");
    let hello = json!({
        "t": "rc:agent.hello",
        "machine_name": "offer box",
        "os": "linux",
        "agent_version": "0.4.132",
        "displays": [],
        "caps": {
            "hw_encoders": [], "codecs": ["h264"],
            "has_input_permission": false, "supports_clipboard": false,
            "supports_file_transfer": false, "max_simultaneous_sessions": 1,
            "rpc": [],
        }
    });
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .expect("send hello");
    ws
}

/// The next message of type `want`, or `None` after `wait`.
async fn recv_t(ws: &mut Ws, want: &str, wait: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(txt)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&txt)
                    && v["t"] == want
                {
                    return Some(v);
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => return None,
            Err(_) => return None,
        }
    }
}

/// A real `rc:overlay.join` advertising `routes`; returns the joiner's netmap.
async fn join_offering(ws: &mut Ws, seed: u8, routes: &[&str]) -> Value {
    let msg = json!({
        "t": "rc:overlay.join",
        "wg_public_key": BASE64.encode([seed; 32]),
        "mtu": 1280,
        "endpoints": [format!("203.0.113.{seed}:51820")],
        "advertised_routes": routes,
    });
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .expect("send join");
    recv_t(ws, "rc:overlay.netmap", Duration::from_secs(10))
        .await
        .expect("a netmap answers the join")
}

/// Read deltas until one upserts `ip` with routes that satisfy `want`; return
/// those routes, or `None` when none does within `wait`. Waiting for the
/// MATCHING delta (rather than the next one) keeps an unrelated trickle from
/// deciding the assertion either way.
async fn routes_upserted(
    ws: &mut Ws,
    ip: &str,
    want: impl Fn(&[String]) -> bool,
    wait: Duration,
) -> Option<Vec<String>> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let delta = recv_t(ws, "rc:overlay.netmap_delta", left).await?;
        for p in delta["upserts"].as_array().into_iter().flatten() {
            if p["overlay_ip"] != ip {
                continue;
            }
            let routes: Vec<String> = p["routes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| r.as_str().map(str::to_string))
                .collect();
            if want(&routes) {
                return Some(routes);
            }
        }
    }
}

fn has_default(routes: &[String]) -> bool {
    routes.iter().any(|r| r == DEFAULT_ROUTE)
}

#[tokio::test]
async fn an_exit_that_stops_offering_its_default_route_stops_being_one() {
    let app = TestApp::spawn().await;
    let seeded: SeededTenant = app.seed_tenant("p0b-offer").await;
    let (_exit_agent, exit_token) = enroll(&app, &seeded, "p0b-exit").await;
    let (_client_agent, client_token) = enroll(&app, &seeded, "p0b-client").await;

    let mut exit_ws = connect(&app, &exit_token).await;
    let exit_ip = join_offering(&mut exit_ws, 1, &[DEFAULT_ROUTE]).await["self_ip"]
        .as_str()
        .expect("the exit's own address")
        .to_string();
    let mut client_ws = connect(&app, &client_token).await;
    join_offering(&mut client_ws, 2, &[]).await;

    // The admin approves the exit.
    let nodes: Value = app
        .auth_get(
            &format!("/api/tenant/{}/overlay-node", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let exit_node_id = nodes["items"]
        .as_array()
        .expect("node list")
        .iter()
        .find(|n| n["overlay_ip"] == exit_ip.as_str())
        .and_then(|n| n["id"].as_str())
        .expect("the exit's node row")
        .to_string();
    let resp = app
        .auth_put(
            &format!(
                "/api/tenant/{}/overlay-node/{exit_node_id}/exit-node",
                seeded.tenant_id
            ),
            &seeded.admin.access_token,
        )
        .json(&json!({ "enabled": true }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "approving the exit: {}",
        resp.status()
    );

    let wait = Duration::from_secs(10);
    let approved = routes_upserted(&mut client_ws, &exit_ip, has_default, wait).await;
    assert!(
        approved.is_some(),
        "approved AND offered: the client is told the exit carries {DEFAULT_ROUTE}"
    );

    // The device stops offering: it rejoins advertising nothing.
    join_offering(&mut exit_ws, 1, &[]).await;
    let withdrawn = routes_upserted(&mut client_ws, &exit_ip, |r| !has_default(r), wait).await;
    assert!(
        withdrawn.is_some(),
        "an approval the device no longer offers must stop reaching the client: \
         no delta without {DEFAULT_ROUTE} arrived after the rejoin"
    );

    // The approval itself was kept, so the offer coming back restores the exit
    // with no admin action.
    join_offering(&mut exit_ws, 1, &[DEFAULT_ROUTE]).await;
    let restored = routes_upserted(&mut client_ws, &exit_ip, has_default, wait).await;
    assert!(
        restored.is_some(),
        "the offer is back, and the stored approval with it"
    );
}

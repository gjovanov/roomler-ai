// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4402 — one fan-out pass writes one line at production's level, not one
//! line for each peer it could not reach.
//!
//! The busier server pod wrote about 50,000 lines an hour, and 40 % of them
//! were "overlay: agent node unreachable; skipped". A delta fanned to an
//! overlay network writes that line for every peer this pod holds no socket
//! for: a device that is offline, whose row stays in the network, or one whose
//! socket is on the other pod. One change in a network with M such peers wrote
//! M lines. These tests fan one delta to a network of enrolled devices that
//! never connected, and count what the pass writes at DEBUG, the network
//! module's level in production (`crates/api/src/main.rs`). Each of the three
//! loops that fan a delta is driven once: a re-fan, a removal re-fanned from
//! another pod, and a real join.

use crate::agent_presence_tests::{connect_agent, enroll};
use crate::fixtures::{seed::SeededTenant, test_app::TestApp};
use crate::session_media_plan_tests::Logged;
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use bson::oid::ObjectId;
use futures::{SinkExt, StreamExt};
use roomler_ai_remote_control::models::{NodeRef, OverlayNode};
use roomler_ai_services::dao::overlay_node::NewOverlayNode;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// The line a pass wrote for each peer it could not reach.
const PER_PEER: &str = "overlay: agent node unreachable; skipped";
/// The one line a pass writes, with its counts.
const PASS: &str = "overlay: delta fanned out";
/// The peers with no socket in each test's network.
const OFFLINE: usize = 3;

/// `n` overlay nodes in the tenant's network, each the node of an enrolled
/// device that never connected. The agent rows are real, so the fan resolves
/// each peer's owner as it does an offline device's; only the socket is
/// missing.
async fn offline_nodes(
    app: &TestApp,
    seeded: &SeededTenant,
    tag: &str,
    n: usize,
) -> Vec<OverlayNode> {
    let state = app.state.network();
    let tid = ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let net = state.overlay_networks.get_or_create(tid).await.unwrap();
    let network_id = net.id.unwrap();
    let mut nodes = Vec::with_capacity(n);
    for i in 0..n {
        let machine = format!("{tag}-off-{i}");
        let (agent_id, _token) = enroll(app, seeded, &machine).await;
        let node = state
            .overlay_nodes
            .create(NewOverlayNode {
                tenant_id: tid,
                node_ref: NodeRef::Agent {
                    agent_id: ObjectId::parse_str(&agent_id).unwrap(),
                },
                network_id,
                machine_id: machine.clone(),
                name: machine,
                // Far above the hosts a join allocates first, so a real join
                // beside these rows takes an address of its own.
                overlay_ip: net
                    .host_ip(200 + i as u32)
                    .expect("an address in the network"),
                wg_public_key: B64.encode([0x40 + i as u8; 32]),
                key_epoch: 0,
                endpoints: vec!["203.0.113.20:51820".into()],
                supports_quic: false,
                supports_relay_single: false,
                supports_derp: true,
                supports_forced_derp: false,
                supports_server_relay_strategy: false,
                supports_derp_floor: false,
                supports_overlay_echo: false,
                supports_org_relay: false,
                advertised_routes: vec![],
                route_offer_withheld: None,
                exit_serves_v6: None,
            })
            .await
            .unwrap();
        nodes.push(node);
    }
    nodes
}

/// What the capture holds: the per-peer lines, and each pass line.
fn read(logged: &Logged) -> (usize, Vec<String>) {
    let text = logged.text();
    let per_peer = text.lines().filter(|l| l.contains(PER_PEER)).count();
    let passes = text
        .lines()
        .filter(|l| l.contains(PASS))
        .map(str::to_string)
        .collect();
    (per_peer, passes)
}

/// [`read`], once a pass line is there or `wait` has run out.
async fn read_after_the_pass(logged: &Logged, wait: Duration) -> (usize, Vec<String>) {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let (per_peer, passes) = read(logged);
        if !passes.is_empty() || tokio::time::Instant::now() >= deadline {
            return (per_peer, passes);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The pass wrote no per-peer line at DEBUG, and one line that counts every
/// peer it could not reach.
fn assert_one_line(test: &str, per_peer: usize, passes: &[String], not_here: usize) {
    eprintln!(
        "W56-RESULT {test}: per_peer_lines={per_peer} pass_lines={} peers_not_here={not_here}",
        passes.len()
    );
    assert_eq!(
        per_peer, 0,
        "{test}: the pass wrote {per_peer} lines at DEBUG for the {not_here} peers it could \
         not reach, one a peer; it should count them in one line"
    );
    assert_eq!(passes.len(), 1, "{test}: one pass, one line: {passes:?}");
    for field in [
        format!("not_here={not_here}"),
        "sent=0".to_string(),
        "queue_full=0".to_string(),
    ] {
        assert!(
            passes[0].contains(&field),
            "{test}: the pass line lacks {field}: {}",
            passes[0]
        );
    }
}

/// #4402 — a re-fan of one node (a route approval, a rename, a relay
/// allowance change) writes one line for the peers it could not reach.
#[tokio::test]
async fn a_refan_writes_one_line_for_the_peers_it_cannot_reach() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fanlog-refan").await;
    let nodes = offline_nodes(&app, &seeded, "refan", OFFLINE + 1).await;

    let logged = Logged::default();
    let _log = logged.install_debug();
    roomler_ai_mod_network::overlay::refan_node(app.state.network(), &nodes[OFFLINE]).await;

    let (per_peer, passes) = read(&logged);
    assert_one_line("refan", per_peer, &passes, OFFLINE);
}

/// #4402 — a removal re-fanned from another pod (#1186) writes one line for
/// the peers it could not reach.
#[tokio::test]
async fn a_removal_refanned_from_another_pod_writes_one_line() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fanlog-removes").await;
    let nodes = offline_nodes(&app, &seeded, "removes", OFFLINE).await;

    let logged = Logged::default();
    let _log = logged.install_debug();
    // The removed node's id names no row here, so only the peers hear the
    // removal: the removed node's own push is a single send, with a line of
    // its own when it misses.
    roomler_ai_mod_network::overlay::apply_removes_ctrl(
        app.state.network(),
        &serde_json::json!({
            "tenant_id": seeded.tenant_id.as_str(),
            "network_id": nodes[0].network_id.to_hex(),
            "node_id": ObjectId::new().to_hex(),
            "epoch": 7,
        }),
    )
    .await;

    let (per_peer, passes) = read(&logged);
    assert_one_line("removes", per_peer, &passes, OFFLINE);
}

/// #4402 — a join fans the joiner to every peer, and writes one line for the
/// peers it could not reach.
#[tokio::test]
async fn a_join_writes_one_line_for_the_peers_it_cannot_reach() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fanlog-join").await;
    offline_nodes(&app, &seeded, "join", OFFLINE).await;
    let (_agent_id, token) = enroll(&app, &seeded, "fanlog-joiner").await;
    let mut ws = connect_agent(&app, &token).await;

    let logged = Logged::default();
    let _log = logged.install_debug();
    let join = serde_json::json!({
        "t": "rc:overlay.join",
        "wg_public_key": B64.encode([0x21; 32]),
        "mtu": 1280,
        "endpoints": ["203.0.113.9:51820"],
    });
    ws.send(Message::Text(join.to_string().into()))
        .await
        .expect("send join");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let netmap = loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break false;
        }
        match tokio::time::timeout(left, ws.next()).await {
            // The full netmap, not a `_delta`: the closing quote tells them apart.
            Ok(Some(Ok(Message::Text(t)))) if t.contains("\"rc:overlay.netmap\"") => break true,
            Ok(Some(Ok(_))) => {}
            _ => break false,
        }
    };
    assert!(netmap, "the joiner must receive its netmap");

    // The joiner's netmap is sent before the fan to its peers, so the pass may
    // still be running here: read once its line is written.
    let (per_peer, passes) = read_after_the_pass(&logged, Duration::from_secs(10)).await;
    assert_one_line("join", per_peer, &passes, OFFLINE);
    ws.close(None).await.ok();
}

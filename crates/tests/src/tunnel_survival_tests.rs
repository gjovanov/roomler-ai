// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4501: an in-flight TCP connection through a daemon-supervised tunnel
//! forward keeps moving bytes when the control plane under it moves.
//!
//! Two cells, both on two pods over one database (`TestApp::spawn_pair`):
//!
//! - **A roll.** The pod serving both agents shuts down the way SIGTERM shuts
//!   it down (`shutdown_cleanup`), and the load balancer sends their redials to
//!   the other pod.
//! - **The exit's control socket cycles.** The exit's path to the server drops
//!   with no close, as a full-tunnel VPN rerouting under it drops it, while the
//!   exit itself stays up and redials the same pod.
//!
//! In both, the data plane is untouched: the bytes ride QUIC straight between
//! the two agents (quic-v1, host candidates), not the server. What the cells
//! ask is whether the control plane's move ends a session that its data plane
//! could have carried on. Each also opens a NEW connection after the event,
//! because a route people use opens connections all the time, and a session
//! that kept its old connections but could not open new ones would not be
//! serving.
//!
//! Both agents run their real `signaling::run`; the forward is the daemon's own
//! (`TunnelClientHub::create_forward`, what `roomler forward --daemon` asks for),
//! at the production defaults: no server grace (`rc.tunnel_grace_secs` 0) and
//! no agent peer survival (`tunnel_peers_survive_reattach` absent).

use crate::fixtures::test_app::TestApp;
use crate::tunnel_tests::{
    enrol_agent_lib, free_local_port, spawn_origin_signaling, wait_agent_online,
};
use roomlerd::tunnel::client_mgr::TunnelClientHub;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The load balancer in one process. Each agent dials its own front, and the
/// front pipes every connection to the pod it points at when the dial lands.
/// `cut` drops every live pipe with no close frame (a path that died, as both
/// ends hear it); `point_at` moves where the next dials land.
struct Front {
    addr: SocketAddr,
    upstream: Arc<Mutex<SocketAddr>>,
    live: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
}

impl Front {
    async fn to(pod: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let upstream = Arc::new(Mutex::new(pod));
        let live: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::default();
        let (up, pipes) = (upstream.clone(), live.clone());
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                let target = *up.lock().unwrap();
                let pipe = tokio::spawn(async move {
                    if let Ok(mut outbound) = TcpStream::connect(target).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
                pipes.lock().unwrap().push(pipe.abort_handle());
            }
        });
        Self {
            addr,
            upstream,
            live,
        }
    }

    fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    fn point_at(&self, pod: SocketAddr) {
        *self.upstream.lock().unwrap() = pod;
    }

    fn cut(&self) {
        for pipe in self.live.lock().unwrap().drain(..) {
            pipe.abort();
        }
    }
}

/// A TCP echo server on loopback: the forward's destination, on the exit.
async fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    addr
}

/// One round trip on `conn`: write `tag`, read it back within `within`. The
/// `Err` says how it failed (an early EOF is the tunnel closing the
/// connection).
async fn echo(conn: &mut TcpStream, tag: &str, within: Duration) -> Result<(), String> {
    conn.write_all(tag.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = vec![0u8; tag.len()];
    match tokio::time::timeout(within, conn.read_exact(&mut buf)).await {
        Err(_) => Err(format!("no echo within {within:?}")),
        Ok(Err(e)) => Err(format!("read: {e}")),
        Ok(Ok(_)) if buf == tag.as_bytes() => Ok(()),
        Ok(Ok(_)) => Err(format!(
            "echo mismatch: {:?}",
            String::from_utf8_lossy(&buf)
        )),
    }
}

/// A connection through the forward's local port. The flow binds its
/// listener asynchronously, so the connect is retried briefly.
async fn dial_forward(local: u16) -> Result<TcpStream, String> {
    let mut last = String::new();
    for _ in 0..60 {
        match TcpStream::connect(("127.0.0.1", local)).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                last = e.to_string();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    Err(format!("connect: {last}"))
}

/// Two pods, an exit and a tunnel client on pod 1 (each through its own
/// front), a live quic-v1 forward from the client through the exit to an
/// echo server, and one connection through it that has already moved bytes.
struct LiveForward {
    pod1: TestApp,
    pod2: TestApp,
    client_front: Front,
    exit_front: Front,
    hub: TunnelClientHub,
    flow_id: String,
    local: u16,
    in_flight: TcpStream,
    stops: Vec<tokio::sync::watch::Sender<bool>>,
}

async fn live_quic_forward(tag: &str) -> LiveForward {
    let (pod1, pod2) = TestApp::spawn_pair(|s| {
        // Fast presence writes, so `wait_agent_online` returns promptly.
        s.rc.presence_batch_ms = 50;
    })
    .await;
    let seeded = pod1.seed_tenant(tag).await;
    // Any principal (an agent included) may reach any loopback port on any
    // agent of the tenant: the echo server's port is random.
    let policy = json!({
        "name": "allow-loopback",
        "subjects": [{ "kind": "all_users" }],
        "targets": [{ "kind": "all_agents" }],
        "allowlist": [{
            "host_pattern": { "kind": "cidr", "value": "127.0.0.0/8" },
            "port_range": { "low": 1, "high": 65535 }
        }],
        "max_concurrent_flows": 32,
        "max_bytes_per_session": 1_073_741_824u64
    });
    let created = pod1
        .auth_post(
            &format!("/api/tenant/{}/tunnel-policy", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&policy)
        .send()
        .await
        .unwrap();
    assert!(
        created.status().is_success(),
        "tunnel policy create: {}",
        created.status()
    );
    let dst = echo_server().await;

    let exit_front = Front::to(pod1.addr).await;
    let client_front = Front::to(pod1.addr).await;
    let mut stops = Vec::new();

    let mut exit_cfg = enrol_agent_lib(&pod1, &seeded, &format!("mach-{tag}-exit"), "exit").await;
    exit_cfg.ws_url = Some(exit_front.ws_url());
    let exit_id = exit_cfg.agent_id.clone();
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    stops.push(stop);
    // The exit serves; the hub it is handed is never driven.
    let _ = spawn_origin_signaling(exit_cfg, TunnelClientHub::new("0.3.0-test".into()), stop_rx);
    wait_agent_online(&pod1, &seeded, &exit_id).await;

    let mut client_cfg =
        enrol_agent_lib(&pod1, &seeded, &format!("mach-{tag}-client"), "client").await;
    client_cfg.ws_url = Some(client_front.ws_url());
    let client_id = client_cfg.agent_id.clone();
    let hub = TunnelClientHub::new("0.3.0-test".into());
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    stops.push(stop);
    let _ = spawn_origin_signaling(client_cfg, hub.clone(), stop_rx);
    wait_agent_online(&pod1, &seeded, &client_id).await;

    let local = free_local_port().await;
    let flow_id = hub
        .create_forward(&exit_id, local, &dst.to_string(), "quic", "")
        .await
        .expect("create_forward registers the flow");
    let mut in_flight = dial_forward(local).await.expect("the forward's listener");
    // The first echo waits out the session's setup: open, QUIC setup and
    // handshake, then the flow's own forward request.
    echo(&mut in_flight, "first;", Duration::from_secs(30))
        .await
        .expect("the forward carries bytes before the event");
    assert_eq!(
        hub.negotiated_transport(&flow_id).as_deref(),
        Some("quic-v1"),
        "the cell is about a quic-v1 session"
    );
    echo(&mut in_flight, "second;", Duration::from_secs(5))
        .await
        .expect("the forward keeps carrying bytes before the event");

    LiveForward {
        pod1,
        pod2,
        client_front,
        exit_front,
        hub,
        flow_id,
        local,
        in_flight,
        stops,
    }
}

/// After the event: the in-flight connection must echo every probe for 12 s
/// (each within 5 s), and a connection opened after the event must be served
/// within 15 s. Returns both outcomes, so a failure says which half failed and
/// when.
async fn after_the_event(rig: &mut LiveForward) -> (Result<(), String>, Result<(), String>) {
    let event = Instant::now();
    let mut in_flight = Ok(());
    let mut n = 0u32;
    while event.elapsed() < Duration::from_secs(12) {
        n += 1;
        if let Err(e) = echo(
            &mut rig.in_flight,
            &format!("probe-{n};"),
            Duration::from_secs(5),
        )
        .await
        {
            in_flight = Err(format!(
                "probe {n}: {e}, {:.1} s after the event",
                event.elapsed().as_secs_f64()
            ));
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let fresh = match dial_forward(rig.local).await {
        Ok(mut c) => echo(&mut c, "fresh;", Duration::from_secs(15)).await,
        Err(e) => Err(e),
    };
    (in_flight, fresh)
}

impl LiveForward {
    fn stop(self) {
        let _ = self.hub.kill_flow(&self.flow_id);
        for stop in &self.stops {
            let _ = stop.send(true);
        }
    }
}

/// Cell A, the roll: pod 1 shuts down as SIGTERM shuts it down, with the
/// front already sending redials to pod 2 (maxSurge 0: the other pod is the
/// survivor). Field: every session the pod carried ended, and every in-flight
/// connection in them was reset.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_in_flight_tunnel_connection_survives_a_rolled_pod() {
    let mut rig = live_quic_forward("survive-roll").await;

    rig.client_front.point_at(rig.pod2.addr);
    rig.exit_front.point_at(rig.pod2.addr);
    roomler_ai_api::state::shutdown_cleanup(&rig.pod1.state).await;

    let (in_flight, fresh) = after_the_event(&mut rig).await;
    rig.stop();
    assert!(
        in_flight.is_ok() && fresh.is_ok(),
        "a roll must not end a tunnel session its data plane still carries: \
         in-flight connection = {in_flight:?}; new connection = {fresh:?}"
    );
}

/// Cell B, the exit's control socket cycles: its path to the server drops
/// with no close and it redials the same pod, while it and the QUIC path stay
/// up. Field: the server ended every session aimed at that exit at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_in_flight_tunnel_connection_survives_its_exits_control_socket_cycling() {
    let mut rig = live_quic_forward("survive-exit-ws").await;

    rig.exit_front.cut();

    let (in_flight, fresh) = after_the_event(&mut rig).await;
    rig.stop();
    assert!(
        in_flight.is_ok() && fresh.is_ok(),
        "an exit's control socket cycling must not end a tunnel session its data \
         plane still carries: in-flight connection = {in_flight:?}; \
         new connection = {fresh:?}"
    );
}

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3517 — the agent socket's disconnect line says how the socket ended.
//!
//! Through the real `/ws?role=agent` socket and its teardown, each end leaves
//! its own `cause` on `remote-control agent WS disconnected`: the daemon's own
//! clean stop (a close frame with no status code), a close frame with one, and
//! a TCP leg that ends with no close frame at all. A socket that ends before
//! its hello registers nothing, so the hello-phase line says how instead. The
//! fields, and the grep that reads them on a pod: `docs/real-time.md` § How an
//! agent socket ended.

use std::time::Duration;

use futures::SinkExt;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};

use crate::agent_presence_tests::{connect_agent, enroll, urlencode};
use crate::fixtures::test_app::TestApp;
use crate::session_media_plan_tests::Logged;
use crate::tunnel_tests::{wait_agent_offline, wait_agent_online};

const DISCONNECTED: &str = "remote-control agent WS disconnected";
const NO_HELLO: &str = "agent opened WS without rc:agent.hello";

/// This thread's log lines carrying `message` and `agent_id`, once there are
/// `n` of them (5 s at most; the caller asserts what it got).
async fn lines(logged: &Logged, message: &str, agent_id: &str, n: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let found: Vec<String> = logged
            .text()
            .lines()
            .filter(|l| l.contains(message) && l.contains(agent_id))
            .map(str::to_owned)
            .collect();
        if found.len() >= n || tokio::time::Instant::now() >= deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn the_disconnect_line_names_how_the_agent_socket_ended() {
    let logged = Logged::default();
    let _log = logged.install();
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("wsend3517").await;
    let (agent_id, token) = enroll(&app, &seeded, "mach-3517-ws-end").await;

    // Before its hello. This line used to read `other=None` however the
    // socket ended.
    let url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        urlencode(&token)
    );
    let (mut ws, _) = connect_async(&url).await.expect("ws connect");
    ws.send(Message::Close(None)).await.unwrap();
    let found = lines(&logged, NO_HELLO, &agent_id, 1).await;
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(found[0].contains(" cause=close_no_frame"), "{found:#?}");

    // The daemon's own clean stop: a close frame with no status code. A
    // field that means nothing for this end is left off the line.
    let mut ws = connect_agent(&app, &token).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    ws.send(Message::Close(None)).await.unwrap();
    wait_agent_offline(&app, &seeded, &agent_id).await;
    let found = lines(&logged, DISCONNECTED, &agent_id, 1).await;
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(found[0].contains(" cause=close_no_frame"), "{found:#?}");
    for absent in [
        " code=",
        " reason=",
        " kind=",
        " error=",
        " elapsed_s=",
        " by=",
    ] {
        assert!(
            !found[0].contains(absent),
            "`{absent}` means nothing for this end: {}",
            found[0]
        );
    }

    // A close frame with a status code: the code, and its reason quoted.
    let mut ws = connect_agent(&app, &token).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    ws.send(Message::Close(Some(CloseFrame {
        code: CloseCode::Away,
        reason: "going away".into(),
    })))
    .await
    .unwrap();
    wait_agent_offline(&app, &seeded, &agent_id).await;
    let found = lines(&logged, DISCONNECTED, &agent_id, 2).await;
    assert_eq!(found.len(), 2, "{found:#?}");
    assert!(
        found[1].contains(r#" cause=close code=1001 reason="going away""#),
        "{found:#?}"
    );

    // A TCP leg that ends with no close frame, as when the device's process
    // goes away. The kernel ends it with a FIN (`eof`), or with an RST
    // (`reset`) when frames the server sent are still unread on this side,
    // and this client reads none, so either word is this end. The loopback
    // test in fleet's `socket.rs` pins `eof` on its own.
    let ws = connect_agent(&app, &token).await;
    wait_agent_online(&app, &seeded, &agent_id).await;
    drop(ws);
    wait_agent_offline(&app, &seeded, &agent_id).await;
    let found = lines(&logged, DISCONNECTED, &agent_id, 3).await;
    assert_eq!(found.len(), 3, "{found:#?}");
    assert!(
        [
            " cause=error kind=eof error=",
            " cause=error kind=reset error="
        ]
        .iter()
        .any(|end| found[2].contains(end)),
        "{found:#?}"
    );
}

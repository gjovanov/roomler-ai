// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-99 P1a — the server's gate for privacy mode, through real servers, real
//! WebSockets and a real MongoDB.
//!
//! `Permissions::PRIVACY` survives a session request only when BOTH hold:
//!
//! 1. the device serves it — its caps advertise `rpc: ["privacy-mode"]`,
//!    which an agent does only while its owner allows privacy mode. A device
//!    that could but whose owner has not allowed it advertises
//!    `privacy-available` alone (`device_not_opted_in`); one that cannot
//!    curtain at all advertises neither (`device_cannot_curtain`, named first);
//! 2. the session is not break-glass (`break_glass`, decision 1).
//!
//! ⚠️ The point of the gate is the wire, not the viewer. `rc:*` carries
//! `Permissions` as NAMES, and an agent that predates `PRIVACY` fails the
//! whole `rc:request` on it. So every cell reads the frame the AGENT received
//! and asserts the name is not in it at all (AC5), beside what the viewer is
//! told. Each refusal has its positive control: a strip that fired for every
//! request would pass every "refused" cell.

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const ADMINISTRATOR: u64 = 1 << 23;

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// An agent's capabilities, with `rpc` as its verbs.
fn caps(rpc: &[&str]) -> Value {
    json!({
        "hw_encoders": [],
        "codecs": ["h264"],
        "has_input_permission": true,
        "supports_clipboard": false,
        "supports_file_transfer": false,
        // Enough for every cell below to hold a session at once.
        "max_simultaneous_sessions": 16,
        "input": ["arbiter"],
        "rpc": rpc,
    })
}

/// Enroll a device (owned by the seeded admin, who issues the token) and
/// connect it with `rpc` as its verbs.
async fn connect_agent(
    app: &TestApp,
    seeded: &SeededTenant,
    machine_id: &str,
    rpc: &[&str],
) -> (String, Ws) {
    let et: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ej: Value = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": machine_id,
            "machine_name": machine_id,
            "os": "windows",
            "agent_version": "0.1.0",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let agent_id = ej["agent_id"].as_str().unwrap().to_string();
    let token = ej["agent_token"].as_str().unwrap().to_string();
    let url = format!(
        "ws://{}/ws?token={}&role=agent",
        app.addr,
        urlencode(&token)
    );
    let (mut ws, _) = connect_async(&url).await.expect("agent ws");
    let hello = json!({
        "t": "rc:agent.hello",
        "machine_name": machine_id,
        "os": "windows",
        "agent_version": "0.1.0",
        "displays": [{
            "index": 0, "name": "x", "width_px": 800, "height_px": 600,
            "scale": 1.0, "primary": true,
        }],
        "caps": caps(rpc),
    });
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    // Let the hello land before anyone asks for a session.
    tokio::time::sleep(Duration::from_millis(300)).await;
    (agent_id, ws)
}

/// The server's answer to one request for `VIEW | INPUT | PRIVACY`, and the
/// frame the agent was sent for it.
struct Answer {
    /// The controller connection that asked. Held by the caller while the
    /// session must stay live; dropping it lets the next request reap it.
    #[allow(dead_code)]
    ws: Ws,
    session_id: String,
    permissions: String,
    why: Option<String>,
    /// The agent's `rc:request` for this session, as the raw text it received.
    agent_saw: String,
}

impl Answer {
    fn covers(&self) -> bool {
        self.permissions.split('|').any(|p| p.trim() == "PRIVACY")
    }
}

/// The next `rc:request` for `session_id` on the agent's socket, as raw text.
/// Raw on purpose: the claim is about the BYTES an older agent would parse.
async fn agent_request(agent: &mut Ws, session_id: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(500), agent.next()).await
        else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if v["t"] == "rc:request" && v["session_id"] == session_id {
            return text.to_string();
        }
    }
    panic!("the agent was sent no rc:request for {session_id} within 5 s");
}

/// Ask for `VIEW | INPUT | PRIVACY` on a FRESH controller connection, and
/// read what the agent was sent.
///
/// ⚠️ Fresh on purpose, as in `remote_recording_tests`: a second request on
/// the same socket coalesces onto the live session (#1045) and would answer
/// the first cell's question.
async fn ask_for_privacy(
    app: &TestApp,
    token: &str,
    agent_id: &str,
    agent: &mut Ws,
    override_reason: Option<&str>,
) -> Answer {
    let url = format!("ws://{}/ws?token={}", app.addr, urlencode(token));
    let (mut ws, _) = connect_async(&url).await.expect("controller ws");
    let _ = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    let mut req = json!({
        "t": "rc:session.request",
        "agent_id": agent_id,
        "permissions": "VIEW | INPUT | PRIVACY",
    });
    if let Some(r) = override_reason {
        req["override_reason"] = json!(r);
    }
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(500), ws.next()).await
        else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match v.get("t").and_then(|t| t.as_str()) {
            Some("rc:session.created") => {
                let session_id = v["session_id"].as_str().unwrap().to_string();
                let agent_saw = agent_request(agent, &session_id).await;
                return Answer {
                    permissions: v["permissions"].as_str().unwrap_or_default().to_string(),
                    why: v["privacy_refused"].as_str().map(str::to_string),
                    session_id,
                    agent_saw,
                    ws,
                };
            }
            Some("rc:error") => panic!("the request was refused outright: {v}"),
            _ => {}
        }
    }
    panic!("no rc:session.created within 5 s");
}

/// Assert the agent's frame does not carry the name at all, in any position.
fn assert_never_sent(a: &Answer) {
    assert!(
        !a.agent_saw.contains("PRIVACY"),
        "an agent that does not serve privacy mode was sent the name: {}",
        a.agent_saw
    );
    let v: Value = serde_json::from_str(&a.agent_saw).unwrap();
    let sent = v["permissions"].as_str().unwrap_or_default();
    assert!(
        sent.contains("VIEW") && sent.contains("INPUT"),
        "the rest of the grant must survive the strip: {sent}"
    );
}

/// A role with `mask`, assigned to the seeded member.
async fn give_member(app: &TestApp, seeded: &SeededTenant, name: &str, mask: u64) {
    let role: Value = app
        .auth_post(
            &format!("/api/tenant/{}/role", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .json(&json!({ "name": name, "permissions": mask }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let role_id = role["id"].as_str().expect("a role id").to_string();
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/role/{}/assign/{}",
                seeded.tenant_id, role_id, seeded.member.id
            ),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "assign {name}");
}

#[tokio::test]
async fn privacy_reaches_only_an_agent_that_serves_it() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr99gate1").await;
    let (serves, mut serves_ws) = connect_agent(
        &app,
        &seeded,
        "mach-fr99-serves",
        &["privacy-available", "privacy-mode"],
    )
    .await;
    let (plain, mut plain_ws) =
        connect_agent(&app, &seeded, "mach-fr99-plain", &["privacy-available"]).await;
    // An agent from before FR-99: the case the strip exists for.
    let (old, mut old_ws) = connect_agent(&app, &seeded, "mach-fr99-old", &["exec"]).await;
    let owner = &seeded.admin.access_token;

    // The positive control: the device's owner, on a device that serves it,
    // is granted PRIVACY — and the agent is sent it.
    let a = ask_for_privacy(&app, owner, &serves, &mut serves_ws, None).await;
    assert!(
        a.covers(),
        "a serving device keeps PRIVACY: {}",
        a.permissions
    );
    assert_eq!(a.why, None);
    assert!(
        a.agent_saw.contains("PRIVACY"),
        "the serving agent must be sent it: {}",
        a.agent_saw
    );

    // A device whose owner has not allowed it.
    let a = ask_for_privacy(&app, owner, &plain, &mut plain_ws, None).await;
    assert!(!a.covers(), "{}", a.permissions);
    assert_eq!(a.why.as_deref(), Some("device_not_opted_in"));
    assert_never_sent(&a);

    // AC5 — an agent without the verb never receives PRIVACY.
    let a = ask_for_privacy(&app, owner, &old, &mut old_ws, None).await;
    assert!(!a.covers(), "{}", a.permissions);
    assert_eq!(a.why.as_deref(), Some("device_cannot_curtain"));
    assert_never_sent(&a);
}

/// ⚠️ Break-glass never covers the screen (decision 1): it skips the host's
/// consent, and a curtain would hide the session from the one person who
/// could object. The same ADMINISTRATOR without a reason is the control.
#[tokio::test]
async fn break_glass_never_covers_the_screen() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr99gate2").await;
    let (serves, mut ws) = connect_agent(&app, &seeded, "mach-fr99-bg", &["privacy-mode"]).await;
    // The member becomes an ADMINISTRATOR who does not own the device.
    give_member(&app, &seeded, "admins", ADMINISTRATOR).await;
    let admin = &seeded.member.access_token;

    let a = ask_for_privacy(&app, admin, &serves, &mut ws, None).await;
    assert!(
        a.covers(),
        "a non-owner ADMINISTRATOR may ask for it: {}",
        a.permissions
    );
    assert_eq!(a.why, None);
    drop(a);

    let a = ask_for_privacy(&app, admin, &serves, &mut ws, Some("incident 42")).await;
    assert!(!a.covers(), "break-glass must not cover: {}", a.permissions);
    assert_eq!(a.why.as_deref(), Some("break_glass"));
    assert_never_sent(&a);
}

/// `rc:agent.heartbeat`, re-announcing the caps with `rpc` as its verbs.
async fn heartbeat(ws: &mut Ws, rpc: &[&str]) {
    let hb = json!({
        "t": "rc:agent.heartbeat", "rss_mb": 0, "cpu_pct": 0.0, "active_sessions": 0,
        "caps": caps(rpc),
    });
    ws.send(Message::Text(hb.to_string().into())).await.unwrap();
}

/// Ask until the answer's `privacy_refused` is `want`, for up to ~4 s: the
/// heartbeat and the request travel on two sockets, so one request straight
/// after the heartbeat could race it.
async fn ask_until(
    app: &TestApp,
    token: &str,
    agent_id: &str,
    agent: &mut Ws,
    want: Option<&str>,
) -> Option<String> {
    let mut last = None;
    for _ in 0..8 {
        let a = ask_for_privacy(app, token, agent_id, agent, None).await;
        if a.why.as_deref() == want {
            return a.why;
        }
        last = a.why;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    last
}

/// The owner switches privacy mode on and off while the device stays
/// connected; the agent re-announces its caps in a heartbeat, and the hub
/// follows in both directions without a reconnect. An OFF must stop the very
/// next grant.
#[tokio::test]
async fn a_heartbeat_switches_privacy_without_a_reconnect() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr99gate3").await;
    let (agent, mut agent_ws) =
        connect_agent(&app, &seeded, "mach-fr99-hb", &["privacy-available"]).await;
    let owner = &seeded.admin.access_token;

    let a = ask_for_privacy(&app, owner, &agent, &mut agent_ws, None).await;
    assert_eq!(a.why.as_deref(), Some("device_not_opted_in"));
    drop(a);

    heartbeat(&mut agent_ws, &["privacy-available", "privacy-mode"]).await;
    assert_eq!(
        ask_until(&app, owner, &agent, &mut agent_ws, None).await,
        None,
        "an ON announced in a heartbeat must reach the hub"
    );

    heartbeat(&mut agent_ws, &["privacy-available"]).await;
    assert_eq!(
        ask_until(
            &app,
            owner,
            &agent,
            &mut agent_ws,
            Some("device_not_opted_in")
        )
        .await
        .as_deref(),
        Some("device_not_opted_in"),
        "an OFF announced in a heartbeat must reach the hub"
    );
}

/// The strip is the server's DECISION, so it is audited beside the grant:
/// `privacy_stripped` on the session's `session_requested` row, and absent
/// when PRIVACY was granted.
#[tokio::test]
async fn the_strip_is_audited_on_the_session_request() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr99gate4").await;
    let (plain, mut plain_ws) =
        connect_agent(&app, &seeded, "mach-fr99-audit", &["privacy-available"]).await;
    let (serves, mut serves_ws) =
        connect_agent(&app, &seeded, "mach-fr99-audit2", &["privacy-mode"]).await;
    let owner = &seeded.admin.access_token;

    let refused = ask_for_privacy(&app, owner, &plain, &mut plain_ws, None).await;
    let granted = ask_for_privacy(&app, owner, &serves, &mut serves_ws, None).await;

    // The audit sink flushes on its own cadence: poll the route.
    let requested = |app: &TestApp, sid: String| {
        let url = app.url(&format!(
            "/api/tenant/{}/session/{}/audit",
            seeded.tenant_id, sid
        ));
        let client = app.client.clone();
        let token = owner.clone();
        async move {
            for _ in 0..40 {
                let resp = client.get(&url).bearer_auth(&token).send().await.unwrap();
                if resp.status().is_success() {
                    let body: Value = resp.json().await.unwrap();
                    if let Some(ev) = body["items"]
                        .as_array()
                        .and_then(|items| {
                            items
                                .iter()
                                .find(|i| i["event"]["kind"] == "session_requested")
                        })
                        .map(|i| i["event"].clone())
                    {
                        return ev;
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            panic!("no session_requested audit row for {sid} within 10 s");
        }
    };

    let ev = requested(&app, refused.session_id.clone()).await;
    assert_eq!(
        ev["privacy_stripped"].as_str(),
        Some("device_not_opted_in"),
        "{ev}"
    );
    let ev = requested(&app, granted.session_id.clone()).await;
    assert!(ev.get("privacy_stripped").is_none(), "{ev}");
}

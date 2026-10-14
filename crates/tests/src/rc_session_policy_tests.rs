// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-101 P2a — the org's session policy for clipboard, files and audio,
//! through real servers, real WebSockets and a real MongoDB.
//!
//! The policy is the org's (`GET`/`PUT …/rc-session-policy`), read by the
//! authz gate and applied by the hub before `rc:request` is built:
//!
//! - `off` takes the feature out of every session, the owner's included;
//! - `ask` keeps it only under the `prompt` consent mode, on an agent whose
//!   prompt names it (`session-policy`): `needs_consent` when nobody at the
//!   device is asked, `device_cannot_ask` when the agent cannot name it.
//!
//! Every cell reads BOTH ends: what the viewer is told (`rc:session.created`)
//! and the frame the AGENT received, because the agent is what acts on the
//! grant. Each refusal has its positive control: a strip that fired for every
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
const REMOTE_CONTROL: u64 = 1 << 25;

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

fn caps(rpc: &[&str]) -> Value {
    json!({
        "hw_encoders": [],
        "codecs": ["h264"],
        "audio": ["opus"],
        "has_input_permission": true,
        "supports_clipboard": true,
        "supports_file_transfer": true,
        "max_simultaneous_sessions": 16,
        "input": ["arbiter"],
        "rpc": rpc,
    })
}

/// Enroll a device owned by the seeded admin and connect it to `app` with
/// `rpc` as its verbs.
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
    tokio::time::sleep(Duration::from_millis(300)).await;
    (agent_id, ws)
}

/// The server's answer to one request for clipboard, files and audio, and
/// the frame the agent was sent for it.
struct Answer {
    /// The controller connection that asked, held while the session lives.
    #[allow(dead_code)]
    ws: Ws,
    session_id: String,
    /// What the viewer was told.
    granted: String,
    refused: Value,
    /// The agent's `rc:request`, parsed.
    agent_saw: Value,
}

impl Answer {
    fn viewer_has(&self, bit: &str) -> bool {
        self.granted.split('|').any(|p| p.trim() == bit)
    }
    fn agent_has(&self, bit: &str) -> bool {
        self.agent_saw["permissions"]
            .as_str()
            .unwrap_or_default()
            .split('|')
            .any(|p| p.trim() == bit)
    }
    fn agent_audio(&self) -> bool {
        self.agent_saw["audio_enabled"].as_bool().unwrap_or(false)
    }
    fn why(&self, feature: &str) -> Option<&str> {
        self.refused.get(feature).and_then(|v| v.as_str())
    }
}

async fn agent_request(agent: &mut Ws, session_id: &str) -> Value {
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
            return v;
        }
    }
    panic!("the agent was sent no rc:request for {session_id} within 5 s");
}

/// Ask `app` for `VIEW | INPUT | CLIPBOARD | FILES` with audio, on a FRESH
/// controller connection (a second request on one socket coalesces onto the
/// live session), and read what the agent was sent.
async fn ask(
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
        "permissions": "VIEW | INPUT | CLIPBOARD | FILES",
        "audio_enabled": true,
    });
    if let Some(r) = override_reason {
        req["override_reason"] = json!(r);
    }
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
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
                    granted: v["permissions"].as_str().unwrap_or_default().to_string(),
                    refused: v.get("features_refused").cloned().unwrap_or(Value::Null),
                    session_id,
                    agent_saw,
                    ws,
                };
            }
            Some("rc:error") => panic!("the request was refused outright: {v}"),
            _ => {}
        }
    }
    panic!("no rc:session.created within 8 s");
}

async fn set_policy(app: &TestApp, seeded: &SeededTenant, body: Value) -> reqwest::Response {
    app.client
        .put(app.url(&format!(
            "/api/tenant/{}/rc-session-policy",
            seeded.tenant_id
        )))
        .bearer_auth(&seeded.admin.access_token)
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn get_policy(app: &TestApp, tenant_id: &str, token: &str) -> reqwest::Response {
    app.client
        .get(app.url(&format!("/api/tenant/{tenant_id}/rc-session-policy")))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
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

/// The route: any member reads it, only `MANAGE_TENANT` writes it, a write
/// changes only what it names, and a bad value or key is refused.
#[tokio::test]
async fn the_policy_route_reads_for_members_and_writes_for_admins() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr101p2route").await;
    let other = app.seed_tenant("fr101p2other").await;
    let tid = seeded.tenant_id.clone();

    // A member reads the default: every feature on.
    let resp = get_policy(&app, &tid, &seeded.member.access_token).await;
    assert_eq!(resp.status().as_u16(), 200);
    let p: Value = resp.json().await.unwrap();
    assert_eq!(p, json!({"clipboard": "on", "files": "on", "audio": "on"}));

    // Someone from another org is not a member, and is told so by code.
    let resp = get_policy(&app, &tid, &other.admin.access_token).await;
    assert_eq!(resp.status().as_u16(), 403);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "not_a_member", "{body}");

    // A member without MANAGE_TENANT cannot write it: a permission refusal,
    // not an eviction from the org.
    let resp = app
        .client
        .put(app.url(&format!("/api/tenant/{tid}/rc-session-policy")))
        .bearer_auth(&seeded.member.access_token)
        .json(&json!({"files": "off"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    let body: Value = resp.json().await.unwrap();
    assert_ne!(body["error"], "not_a_member", "{body}");

    // The admin turns files off…
    let resp = set_policy(&app, &seeded, json!({"files": "off"})).await;
    assert_eq!(resp.status().as_u16(), 200);
    let p: Value = resp.json().await.unwrap();
    assert_eq!(p, json!({"clipboard": "on", "files": "off", "audio": "on"}));
    // …and a later write naming only the clipboard leaves files off.
    let resp = set_policy(&app, &seeded, json!({"clipboard": "ask"})).await;
    let p: Value = resp.json().await.unwrap();
    assert_eq!(
        p,
        json!({"clipboard": "ask", "files": "off", "audio": "on"})
    );

    // Refused, each leaving the policy as it was: an unknown value, an empty
    // body, a misspelt key.
    for bad in [
        json!({"files": "maybe"}),
        json!({}),
        json!({"clipbaord": "off"}),
    ] {
        let resp = set_policy(&app, &seeded, bad.clone()).await;
        assert!(resp.status().is_client_error(), "{bad}: {}", resp.status());
    }
    let p: Value = get_policy(&app, &tid, &seeded.member.access_token)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        p,
        json!({"clipboard": "ask", "files": "off", "audio": "on"})
    );
}

/// `off` reaches both ends: the viewer is told why, and the agent is sent a
/// grant without the bit, no audio track, and the policy itself. It binds the
/// device's own owner too.
#[tokio::test]
async fn an_org_switch_reaches_the_viewer_and_the_agent() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr101p2off").await;
    let (agent, mut agent_ws) = connect_agent(&app, &seeded, "mach-fr101-off", &[]).await;
    let owner = &seeded.admin.access_token;

    // The positive control: an org that set nothing grants everything asked.
    let a = ask(&app, owner, &agent, &mut agent_ws, None).await;
    assert!(
        a.viewer_has("FILES") && a.viewer_has("CLIPBOARD"),
        "{}",
        a.granted
    );
    assert!(
        a.agent_has("FILES") && a.agent_has("CLIPBOARD"),
        "{}",
        a.agent_saw
    );
    assert!(
        a.agent_audio(),
        "the agent is asked for audio: {}",
        a.agent_saw
    );
    assert!(a.refused.is_null(), "nothing refused: {}", a.refused);
    assert!(
        a.agent_saw.get("session_policy").is_none(),
        "{}",
        a.agent_saw
    );
    drop(a);

    let resp = set_policy(&app, &seeded, json!({"files": "off", "audio": "off"})).await;
    assert_eq!(resp.status().as_u16(), 200);

    let a = ask(&app, owner, &agent, &mut agent_ws, None).await;
    assert!(!a.viewer_has("FILES"), "{}", a.granted);
    assert!(
        a.viewer_has("CLIPBOARD"),
        "the rest survives: {}",
        a.granted
    );
    assert_eq!(a.why("files"), Some("org_policy_off"));
    assert_eq!(a.why("audio"), Some("org_policy_off"));
    assert_eq!(a.why("clipboard"), None);
    assert!(
        !a.agent_has("FILES"),
        "the agent must not be granted it: {}",
        a.agent_saw
    );
    assert!(a.agent_has("CLIPBOARD"), "{}", a.agent_saw);
    assert!(
        !a.agent_audio(),
        "no audio track is asked for: {}",
        a.agent_saw
    );
    assert_eq!(
        a.agent_saw["session_policy"],
        json!({"clipboard": "on", "files": "off", "audio": "off"}),
        "the agent is told the policy"
    );
}

/// `ask` survives only a host prompt that names it. The owner's own session
/// asks nobody (`needs_consent`); a member's session on an attended device
/// asks the person there, but only an agent that names the feature can
/// (`device_cannot_ask` otherwise); break-glass asks nobody.
#[tokio::test]
async fn ask_needs_a_prompt_that_names_the_feature() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr101p2ask").await;
    let (names, mut names_ws) =
        connect_agent(&app, &seeded, "mach-fr101-names", &["session-policy"]).await;
    let (plain, mut plain_ws) = connect_agent(&app, &seeded, "mach-fr101-plain", &[]).await;
    let resp = set_policy(&app, &seeded, json!({"clipboard": "ask"})).await;
    assert_eq!(resp.status().as_u16(), 200);
    give_member(&app, &seeded, "operators", REMOTE_CONTROL).await;
    let member = &seeded.member.access_token;
    let owner = &seeded.admin.access_token;

    // The positive control: a member on an attended device whose agent names
    // `ask` features keeps the clipboard, and the agent is told to ask.
    let a = ask(&app, member, &names, &mut names_ws, None).await;
    assert!(a.viewer_has("CLIPBOARD"), "{}", a.granted);
    assert!(a.agent_has("CLIPBOARD"), "{}", a.agent_saw);
    assert_eq!(a.why("clipboard"), None);
    assert_eq!(a.agent_saw["session_policy"]["clipboard"], "ask");
    drop(a);

    // The same request to an agent whose prompt cannot name it.
    let a = ask(&app, member, &plain, &mut plain_ws, None).await;
    assert!(!a.viewer_has("CLIPBOARD"), "{}", a.granted);
    assert!(!a.agent_has("CLIPBOARD"), "{}", a.agent_saw);
    assert_eq!(a.why("clipboard"), Some("device_cannot_ask"));
    assert!(a.viewer_has("FILES"), "`on` is untouched: {}", a.granted);
    drop(a);

    // The owner's own session auto-consents: nobody is asked.
    let a = ask(&app, owner, &names, &mut names_ws, None).await;
    assert!(!a.viewer_has("CLIPBOARD"), "{}", a.granted);
    assert_eq!(a.why("clipboard"), Some("needs_consent"));
    drop(a);

    // Break-glass skips the prompt: nobody is asked either.
    give_member(&app, &seeded, "admins", ADMINISTRATOR).await;
    let a = ask(&app, member, &names, &mut names_ws, Some("incident 7")).await;
    assert!(!a.viewer_has("CLIPBOARD"), "{}", a.granted);
    assert!(!a.agent_has("CLIPBOARD"), "{}", a.agent_saw);
    assert_eq!(a.why("clipboard"), Some("needs_consent"));
}

/// The strip is the server's DECISION, audited beside the grant:
/// `features_stripped` on the session's `session_requested` row, absent when
/// nothing was refused.
#[tokio::test]
async fn the_strip_is_audited_on_the_session_request() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr101p2audit").await;
    let (agent, mut agent_ws) = connect_agent(&app, &seeded, "mach-fr101-audit", &[]).await;
    let owner = seeded.admin.access_token.clone();

    let granted = ask(&app, &owner, &agent, &mut agent_ws, None).await;
    let resp = set_policy(&app, &seeded, json!({"files": "off"})).await;
    assert_eq!(resp.status().as_u16(), 200);
    let refused = ask(&app, &owner, &agent, &mut agent_ws, None).await;

    let requested = |sid: String| {
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

    let ev = requested(refused.session_id.clone()).await;
    assert_eq!(
        ev["features_stripped"],
        json!({"files": "org_policy_off"}),
        "{ev}"
    );
    let ev = requested(granted.session_id.clone()).await;
    assert!(ev.get("features_stripped").is_none(), "{ev}");
}

/// ⚠️ The relay is an entry point too. A controller on the OTHER pod asks for
/// an agent homed here: the origin pod's gate reads the policy and the owner
/// pod's hub must apply it, or every cross-pod session would skip the org's
/// switches. The same request before the switch is the control.
#[tokio::test]
async fn a_cross_pod_session_carries_the_policy() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_bus.is_none() {
        eprintln!("skipping: no Redis available");
        return;
    }
    for _ in 0..40 {
        let a = app1.state.cluster_bus.as_ref().unwrap();
        let b = app2.state.cluster_bus.as_ref().unwrap();
        if a.sub_alive.load(std::sync::atomic::Ordering::Relaxed)
            && b.sub_alive.load(std::sync::atomic::Ordering::Relaxed)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let seeded = app1.seed_tenant("fr101p2relay").await;
    // Homed on pod 1; every request below is made on pod 2.
    let (agent, mut agent_ws) = connect_agent(&app1, &seeded, "mach-fr101-relay", &[]).await;
    let owner = &seeded.admin.access_token;
    // Pod 2 routes the request by the agent's presence record, which pod 1
    // writes late in the hello, after several database writes. Wait until
    // pod 2 itself reads the agent online, not for a fixed time.
    let row_url = app2.url(&format!("/api/tenant/{}/agent/{}", seeded.tenant_id, agent));
    let mut online = false;
    for _ in 0..50 {
        let row: Value = app2
            .client
            .get(&row_url)
            .bearer_auth(owner)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if row["presence"] == "online" {
            online = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(online, "pod 2 never read the agent's presence");

    let a = ask(&app2, owner, &agent, &mut agent_ws, None).await;
    assert!(a.viewer_has("FILES"), "the control: {}", a.granted);
    assert!(a.agent_has("FILES"), "{}", a.agent_saw);
    drop(a);

    let resp = set_policy(&app2, &seeded, json!({"files": "off"})).await;
    assert_eq!(resp.status().as_u16(), 200);
    let a = ask(&app2, owner, &agent, &mut agent_ws, None).await;
    assert!(!a.viewer_has("FILES"), "{}", a.granted);
    assert_eq!(a.why("files"), Some("org_policy_off"));
    assert!(
        !a.agent_has("FILES"),
        "the owner pod must apply the policy the origin pod read: {}",
        a.agent_saw
    );
}

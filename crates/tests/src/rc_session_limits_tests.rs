// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-101 P4b — the org's maximum session length, through real servers, real
//! WebSockets and a real MongoDB.
//!
//! The limit is the org's (`GET`/`PUT …/rc-session-limits`), read by the
//! authz gate and armed by the hub when the session goes LIVE (the agent's
//! answer), not when it was requested: consent can take minutes. At the limit
//! the hub ends the session as `IdleTimeout` — a reason every reader already
//! knows — and names the limit (`limit: "max_duration"`) on `rc:terminate` to
//! both ends and in the `session_ended` audit row.

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// Enroll a device owned by the seeded admin and connect it to `app`.
async fn connect_agent(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> (String, Ws) {
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
            "os": "linux",
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
        "os": "linux",
        "agent_version": "0.1.0",
        "displays": [],
        "caps": {
            "hw_encoders": [], "codecs": ["h264"], "has_input_permission": true,
            "supports_clipboard": true, "supports_file_transfer": true,
            "max_simultaneous_sessions": 4, "input": ["arbiter"],
        },
    });
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    (agent_id, ws)
}

/// The next frame of type `t` on `ws` within `within`, or `None`.
async fn next_of(ws: &mut Ws, t: &str, within: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(250), ws.next()).await
        else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if v["t"] == t {
            return Some(v);
        }
    }
    None
}

/// Open a session and drive it LIVE the way the two ends do: the agent
/// consents and answers the controller's offer. Returns the controller's
/// socket and the session id.
async fn go_live(app: &TestApp, token: &str, agent_id: &str, agent: &mut Ws) -> (Ws, String) {
    let url = format!("ws://{}/ws?token={}", app.addr, urlencode(token));
    let (mut ctl, _) = connect_async(&url).await.expect("controller ws");
    let _ = tokio::time::timeout(Duration::from_millis(200), ctl.next()).await;
    ctl.send(Message::Text(
        json!({
            "t": "rc:session.request",
            "agent_id": agent_id,
            "permissions": "VIEW | INPUT",
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let created = next_of(&mut ctl, "rc:session.created", Duration::from_secs(5))
        .await
        .expect("rc:session.created");
    let sid = created["session_id"].as_str().unwrap().to_string();
    next_of(agent, "rc:request", Duration::from_secs(5))
        .await
        .expect("the agent's rc:request");
    agent
        .send(Message::Text(
            json!({ "t": "rc:consent", "session_id": sid, "granted": true })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    next_of(&mut ctl, "rc:ready", Duration::from_secs(5))
        .await
        .expect("rc:ready");
    ctl.send(Message::Text(
        json!({ "t": "rc:sdp.offer", "session_id": sid, "sdp": "v=0" })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    next_of(agent, "rc:sdp.offer", Duration::from_secs(5))
        .await
        .expect("the agent's offer");
    agent
        .send(Message::Text(
            json!({ "t": "rc:sdp.answer", "session_id": sid, "sdp": "v=0" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    next_of(&mut ctl, "rc:sdp.answer", Duration::from_secs(5))
        .await
        .expect("the controller's answer: the session is live");
    (ctl, sid)
}

/// Write the org's limit straight into its row: the route accepts a minute at
/// the least, and a test should not wait one.
async fn store_max_secs(app: &TestApp, seeded: &SeededTenant, secs: i64) {
    let tid = bson::oid::ObjectId::parse_str(&seeded.tenant_id).unwrap();
    app.db
        .collection::<bson::Document>("tenants")
        .update_one(
            bson::doc! { "_id": tid },
            bson::doc! { "$set": { "settings.rc_limits.max_secs": secs } },
        )
        .await
        .unwrap();
}

/// The route: any member reads it, only `MANAGE_TENANT` writes it, `null`
/// clears it, and a value outside a minute to a day is refused.
#[tokio::test]
async fn the_limits_route_reads_for_members_and_writes_for_admins() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr101p4broute").await;
    let url = app.url(&format!(
        "/api/tenant/{}/rc-session-limits",
        seeded.tenant_id
    ));
    let get = |token: String| {
        let (client, url) = (app.client.clone(), url.clone());
        async move { client.get(&url).bearer_auth(token).send().await.unwrap() }
    };
    let put = |token: String, body: Value| {
        let (client, url) = (app.client.clone(), url.clone());
        async move {
            client
                .put(&url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };
    let admin = seeded.admin.access_token.clone();
    let member = seeded.member.access_token.clone();

    let resp = get(member.clone()).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({ "max_secs": null })
    );

    let resp = put(member.clone(), json!({ "max_secs": 3600 })).await;
    assert_eq!(resp.status().as_u16(), 403);

    let resp = put(admin.clone(), json!({ "max_secs": 3600 })).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({ "max_secs": 3600 })
    );
    let resp = get(member.clone()).await;
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({ "max_secs": 3600 })
    );

    for bad in [
        json!({ "max_secs": 59 }),
        json!({ "max_secs": 86_401 }),
        json!({}),
        json!({ "max_sec": 60 }),
    ] {
        let resp = put(admin.clone(), bad.clone()).await;
        assert!(resp.status().is_client_error(), "{bad}: {}", resp.status());
    }

    let resp = put(admin.clone(), json!({ "max_secs": null })).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({ "max_secs": null })
    );
}

/// The limit, end to end: the session lives until it, then both ends are
/// told it ended as `IdleTimeout` because of the org's maximum length, and
/// the audit row says the same. The control is the same flow in an org with
/// no limit, which goes on.
#[tokio::test]
async fn a_live_session_ends_at_the_orgs_maximum_length() {
    let app = TestApp::spawn().await;
    let limited = app.seed_tenant("fr101p4bmax").await;
    let open = app.seed_tenant("fr101p4bopen").await;
    store_max_secs(&app, &limited, 2).await;
    let (agent, mut agent_ws) = connect_agent(&app, &limited, "mach-fr101-max").await;
    let (agent2, mut agent2_ws) = connect_agent(&app, &open, "mach-fr101-open").await;

    let (mut ctl, sid) = go_live(&app, &limited.admin.access_token, &agent, &mut agent_ws).await;
    let (mut ctl2, _sid2) = go_live(&app, &open.admin.access_token, &agent2, &mut agent2_ws).await;

    // Not before its time: still live a second in.
    assert!(
        next_of(&mut ctl, "rc:terminate", Duration::from_millis(900))
            .await
            .is_none(),
        "ended before the maximum length"
    );
    let ended = next_of(&mut ctl, "rc:terminate", Duration::from_secs(5))
        .await
        .expect("the controller is told the session ended");
    assert_eq!(ended["session_id"], sid);
    assert_eq!(ended["reason"], "idle_timeout", "{ended}");
    assert_eq!(ended["limit"], "max_duration", "{ended}");
    let ended = next_of(&mut agent_ws, "rc:terminate", Duration::from_secs(2))
        .await
        .expect("so is the agent");
    assert_eq!(ended["limit"], "max_duration", "{ended}");

    // The audit row names the limit too.
    let audit_url = app.url(&format!(
        "/api/tenant/{}/session/{}/audit",
        limited.tenant_id, sid
    ));
    let mut ended_row = None;
    for _ in 0..40 {
        let resp = app
            .client
            .get(&audit_url)
            .bearer_auth(&limited.admin.access_token)
            .send()
            .await
            .unwrap();
        if resp.status().is_success() {
            let body: Value = resp.json().await.unwrap();
            ended_row = body["items"].as_array().and_then(|items| {
                items
                    .iter()
                    .find(|i| i["event"]["kind"] == "session_ended")
                    .map(|i| i["event"].clone())
            });
            if ended_row.is_some() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let ev = ended_row.expect("a session_ended audit row");
    assert_eq!(ev["reason"], "idle_timeout", "{ev}");
    assert_eq!(ev["limit"], "max_duration", "{ev}");

    // The control: no limit, still live well past the other's.
    assert!(
        next_of(&mut ctl2, "rc:terminate", Duration::from_secs(1))
            .await
            .is_none(),
        "a session in an org with no limit must not end"
    );
}

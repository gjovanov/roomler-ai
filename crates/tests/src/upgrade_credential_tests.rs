// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3656 — a native client's credential in a request header.
//!
//! `/ws?role=agent`, `/ws?role=tunnel-client` and `/derp` take the device's
//! credential as `Authorization: Bearer <jwt>`, and keep taking it from the
//! query for every released client. Both forms reach the same checks, so each
//! check runs here through the header with the query form beside it:
//! audience, the `tid` affinity key, the row's status and the FR-103 token
//! generation. A header and a query that disagree get exactly what the header
//! alone would. Every refusal carries the challenge a new client reads before
//! it ever puts a credential in a URL (`upgrade_auth::asks_for_query_form`).
//!
//! Integration tests, because what matters is the upgrade's wiring: which
//! carrier the role gate reads and whether the refusal goes out with it.

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use futures::StreamExt;
use roomler_ai_remote_control::upgrade_auth::{self, CHALLENGE, CHALLENGE_REFUSED};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use tokio_tungstenite::tungstenite::protocol::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// What an upgrade answered: 101, or the refusal's status and its challenge.
#[derive(Debug, PartialEq)]
enum Answer {
    Upgraded,
    Refused(u16, Option<String>),
}

impl Answer {
    fn status(&self) -> u16 {
        match self {
            Answer::Upgraded => 101,
            Answer::Refused(status, _) => *status,
        }
    }

    /// Whether a new client would dial again with the credential in the URL.
    fn sends_a_new_client_to_the_query_form(&self) -> bool {
        match self {
            Answer::Upgraded => false,
            Answer::Refused(status, challenge) => {
                upgrade_auth::asks_for_query_form(*status, challenge.as_deref())
            }
        }
    }
}

fn refused_with(challenge: &str) -> Answer {
    Answer::Refused(401, Some(challenge.to_string()))
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// Dial `path_and_query` with an optional `Authorization` value.
async fn dial(
    app: &TestApp,
    path_and_query: &str,
    authorization: Option<&str>,
) -> Result<Ws, Answer> {
    let mut req = format!("ws://{}{path_and_query}", app.addr)
        .into_client_request()
        .expect("ws request");
    if let Some(a) = authorization {
        req.headers_mut()
            .insert(AUTHORIZATION, HeaderValue::from_str(a).unwrap());
    }
    match connect_async(req).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Err(Answer::Refused(
            resp.status().as_u16(),
            resp.headers()
                .get(WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        )),
        Err(e) => panic!("{path_and_query}: the upgrade failed oddly: {e}"),
    }
}

async fn answer(app: &TestApp, path_and_query: &str, authorization: Option<&str>) -> Answer {
    match dial(app, path_and_query, authorization).await {
        Ok(_) => Answer::Upgraded,
        Err(refused) => refused,
    }
}

/// Enroll a device with a fresh single-use token: its id and agent token.
async fn enroll_agent(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> (String, String) {
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
    let enrolled: Value = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": et["enrollment_token"].as_str().unwrap(),
            "machine_id": machine_id,
            "machine_name": machine_id,
            "os": "linux",
            "agent_version": "0.4.209",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        enrolled["agent_id"].as_str().unwrap().to_string(),
        enrolled["agent_token"].as_str().unwrap().to_string(),
    )
}

/// Enroll a `roomler` tunnel client: its token.
async fn enroll_tunnel_client(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> String {
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
            "machine_name": machine_id,
            "os": "linux",
            "client_version": "0.4.209",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    enrolled["tunnel_client_token"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn remove_agent(app: &TestApp, seeded: &SeededTenant, agent_id: &str) {
    let resp = app
        .auth_delete(
            &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, agent_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "remove the device");
}

/// The first text frame the server sends on `ws`.
async fn first_text(ws: &mut Ws) -> Value {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return serde_json::from_str(&t).unwrap(),
            Ok(Some(Ok(_))) => continue,
            other => panic!("no text frame: {other:?}"),
        }
    }
}

/// The matrix both agent endpoints answer the same way. `path` is `/ws` or
/// `/derp`, dialled with `role=agent` and the device's own `tid`.
async fn agent_endpoint_matrix(app: &TestApp, seeded: &SeededTenant, path: &str, token: &str) {
    let tid = &seeded.tenant_id;
    let base = format!("{path}?role=agent&tid={tid}");
    let with_query = |t: &str| format!("{base}&token={t}");

    // The header form, with nothing secret in the URL.
    assert_eq!(
        answer(app, &base, Some(&bearer(token))).await,
        Answer::Upgraded,
        "{path}: the token in an Authorization: Bearer header"
    );
    // The query form every released daemon sends.
    assert_eq!(
        answer(app, &with_query(token), None).await,
        Answer::Upgraded,
        "{path}: the query form"
    );

    // The header wins. A garbage query beside a good header changes nothing...
    assert_eq!(
        answer(app, &with_query("garbage"), Some(&bearer(token))).await,
        Answer::Upgraded,
        "{path}: the header decides, the query is not read"
    );
    // ...and a good query does not rescue a refused header.
    let a = answer(app, &with_query(token), Some("Bearer garbage")).await;
    assert_eq!(
        a,
        refused_with(CHALLENGE_REFUSED),
        "{path}: a refused header"
    );
    assert!(
        !a.sends_a_new_client_to_the_query_form(),
        "{path}: a verdict on the credential is final"
    );

    // No credential at all: a challenge that names no error.
    let a = answer(app, &base, None).await;
    assert_eq!(a, refused_with(CHALLENGE), "{path}: nothing presented");
    assert!(
        a.sends_a_new_client_to_the_query_form(),
        "{path}: a header dropped on the way leaves the query form"
    );
    // The query form refused: the same challenge as the header form.
    assert_eq!(
        answer(app, &with_query("garbage"), None).await,
        refused_with(CHALLENGE_REFUSED),
        "{path}: a refused query credential"
    );

    // Another scheme is as if no header came: the query still decides.
    assert_eq!(
        answer(app, &with_query(token), Some("Basic dXNlcjpwYXNz")).await,
        Answer::Upgraded,
        "{path}: a Basic header beside a released client's query"
    );

    // Audience: a user's access token in the header is no agent credential.
    assert_eq!(
        answer(app, &base, Some(&bearer(&seeded.admin.access_token))).await,
        refused_with(CHALLENGE_REFUSED),
        "{path}: a user token"
    );
    // The affinity key must still name the token's own tenant.
    let other = bson::oid::ObjectId::new().to_hex();
    assert_eq!(
        answer(
            app,
            &format!("{path}?role=agent&tid={other}"),
            Some(&bearer(token))
        )
        .await,
        Answer::Refused(403, None),
        "{path}: a tid for another tenant"
    );
}

#[tokio::test]
async fn the_agent_socket_takes_its_token_in_a_header() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("hdr-agent").await;
    let (_, token) = enroll_agent(&app, &seeded, "m-hdr-agent").await;

    agent_endpoint_matrix(&app, &seeded, "/ws", &token).await;

    // The role still comes from the query, and the user flow reads no header:
    // an agent token there opens no user socket.
    assert_eq!(
        answer(
            &app,
            &format!("/ws?tid={}", seeded.tenant_id),
            Some(&bearer(&token))
        )
        .await
        .status(),
        401,
        "the browser's socket does not read the header"
    );
}

#[tokio::test]
async fn derp_takes_the_agent_token_in_a_header() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("hdr-derp").await;
    let (_, token) = enroll_agent(&app, &seeded, "m-hdr-derp").await;

    agent_endpoint_matrix(&app, &seeded, "/derp", &token).await;
}

/// The row is read for the header form as for the query form: a removed
/// device is told why on its socket and refused at DERP, and after the same
/// machine enrolls again every token from before the removal is refused.
#[tokio::test]
async fn a_removed_device_is_refused_in_the_header_form_too() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("hdr-removed").await;
    let tid = &seeded.tenant_id;
    let (agent_id, old) = enroll_agent(&app, &seeded, "m-hdr-removed").await;
    remove_agent(&app, &seeded, &agent_id).await;

    let mut ws = dial(
        &app,
        &format!("/ws?role=agent&tid={tid}"),
        Some(&bearer(&old)),
    )
    .await
    .expect("a removed device's socket upgrades to hear why");
    let goodbye = first_text(&mut ws).await;
    assert_eq!(goodbye["t"], "rc:goodbye", "{goodbye}");
    assert_eq!(goodbye["reason"], "agent_deleted", "{goodbye}");
    assert_eq!(
        answer(
            &app,
            &format!("/derp?role=agent&tid={tid}"),
            Some(&bearer(&old))
        )
        .await,
        refused_with(CHALLENGE_REFUSED),
        "DERP refuses a removed device before the upgrade"
    );

    let (again, new) = enroll_agent(&app, &seeded, "m-hdr-removed").await;
    assert_eq!(again, agent_id, "the same row is revived");
    for path in ["/ws", "/derp"] {
        let base = format!("{path}?role=agent&tid={tid}");
        assert_eq!(
            answer(&app, &base, Some(&bearer(&old))).await,
            refused_with(CHALLENGE_REFUSED),
            "{path}: a token from before the removal (FR-103)"
        );
        assert_eq!(
            answer(&app, &base, Some(&bearer(&new))).await,
            Answer::Upgraded,
            "{path}: the token the revive minted"
        );
    }
}

#[tokio::test]
async fn the_tunnel_client_socket_takes_its_token_in_a_header() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("hdr-tunnel").await;
    let tid = &seeded.tenant_id;
    let token = enroll_tunnel_client(&app, &seeded, "m-hdr-tunnel").await;
    let (_, agent_token) = enroll_agent(&app, &seeded, "m-hdr-tunnel-agent").await;
    let base = format!("/ws?role=tunnel-client&tid={tid}");
    let with_query = |t: &str| format!("{base}&token={t}");

    assert_eq!(
        answer(&app, &base, Some(&bearer(&token))).await,
        Answer::Upgraded,
        "the token in an Authorization: Bearer header"
    );
    assert_eq!(
        answer(&app, &with_query(&token), None).await,
        Answer::Upgraded,
        "the query form every released CLI sends"
    );
    assert_eq!(
        answer(&app, &with_query("garbage"), Some(&bearer(&token))).await,
        Answer::Upgraded,
        "the header decides"
    );
    assert_eq!(
        answer(&app, &with_query(&token), Some("Bearer garbage")).await,
        refused_with(CHALLENGE_REFUSED),
        "a good query does not rescue a refused header"
    );
    assert_eq!(
        answer(&app, &base, None).await,
        refused_with(CHALLENGE),
        "nothing presented"
    );
    // Audience, both ways: neither long-lived token opens the other's socket.
    assert_eq!(
        answer(&app, &base, Some(&bearer(&agent_token))).await,
        refused_with(CHALLENGE_REFUSED),
        "an agent token on the tunnel-client socket"
    );
    assert_eq!(
        answer(
            &app,
            &format!("/ws?role=agent&tid={tid}"),
            Some(&bearer(&token))
        )
        .await,
        refused_with(CHALLENGE_REFUSED),
        "a tunnel-client token on the agent socket"
    );
    let other = bson::oid::ObjectId::new().to_hex();
    assert_eq!(
        answer(
            &app,
            &format!("/ws?role=tunnel-client&tid={other}"),
            Some(&bearer(&token))
        )
        .await,
        Answer::Refused(403, None),
        "a tid for another tenant"
    );
}

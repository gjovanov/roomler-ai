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
//!
//! The clients that present it meet the real server here too: the daemon's
//! signalling loop, the dial its DERP sockets make and the tunnel CLI's dial
//! each upgrade on their first, header-only dial (a fallback to the query form
//! would have noted their endpoint), and a refused credential hears the
//! verdict that ends a dial.

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use futures::StreamExt;
use roomler_ai_remote_control::upgrade_auth::{self, CHALLENGE, CHALLENGE_REFUSED, Carrier};
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

// ── The clients that present it ─────────────────────────────────────────────
//
// The daemon and the CLI dial with the credential in the header and redial once
// in the query form only after a refusal with no verdict on it; their own unit
// tests hold that rule against a loopback front. Here their real dials meet
// the real server.

/// Whether dials to `url`'s endpoint take the query form now: only after a
/// header-only dial there was refused with no verdict and the query redial
/// upgraded.
fn carrier_noted_for(url: &str) -> Carrier {
    upgrade_auth::query_form_notes().carrier_for(upgrade_auth::endpoint_of(url))
}

/// Whether a refused client dial, as `Debug` prints it, carries the verdict
/// that ends a dial. The daemon and the CLI build on a newer tungstenite than
/// this crate, whose types these tests never name; its `Http` error prints the
/// status and every header.
fn refused_with_a_verdict(shown: &str) -> bool {
    shown.contains("status: 401") && shown.contains("invalid_token")
}

/// The real signalling loop brings a device online on its first dial, the
/// token in the header: no query form is noted for its `/ws`.
#[tokio::test]
async fn the_daemons_control_socket_goes_online_on_its_header() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("hdr-loop").await;
    let cfg =
        crate::agent_tests::enrol_via_agent_lib(&app, &seeded, "m-hdr-loop", "Header loop").await;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let task = crate::agent_tests::spawn_agent_signaling(cfg.clone(), stop_rx);

    let mut online = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if crate::agent_tests::agent_is_online(&app, &seeded, &cfg.agent_id).await {
            online = true;
            break;
        }
    }
    let carrier = carrier_noted_for(&cfg.ws_url());
    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    assert!(
        online,
        "the signalling loop never brought the device online"
    );
    assert_eq!(carrier, Carrier::Header, "it upgraded on the header");
}

/// The daemon's dial (`roomlerd::ws_connect::dial`, which its DERP sockets
/// make; `roomlerd::derp` is overlay-gated and out of this crate's reach)
/// registers two devices on their tokens in the header, and `/derp` carries a
/// frame between them. A refused token hears the verdict that ends the dial.
#[tokio::test]
async fn the_daemons_derp_dial_relays_on_its_header() {
    use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
    use futures::SinkExt;
    use roomler_ai_remote_control::models::NodeRef;
    use roomler_ai_services::dao::overlay_network::OverlayNetworkDao;
    use roomler_ai_services::dao::overlay_node::{NewOverlayNode, OverlayNodeDao};
    use roomlerd::ws_connect::{self, Credential, Dial};

    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("hdr-relay").await;
    let tid = bson::oid::ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let network_id = OverlayNetworkDao::new(&app.db)
        .get_or_create(tid)
        .await
        .unwrap()
        .id
        .unwrap();
    let nodes = OverlayNodeDao::new(&app.db);
    // The central dial's URL: no credential in it.
    let url = format!("ws://{}/derp?role=agent&tid={}", app.addr, seeded.tenant_id);
    let (pk_a, pk_b) = ([0xa1u8; 32], [0xb2u8; 32]);
    let payload = b"over the header";

    let mut sockets = Vec::new();
    for (machine, pk, ip) in [
        ("m-hdr-relay-a", pk_a, "100.64.0.1"),
        ("m-hdr-relay-b", pk_b, "100.64.0.2"),
    ] {
        let (agent_id, token) = enroll_agent(&app, &seeded, machine).await;
        // `/derp` registers a key only off the node row of the machine the
        // device enrolled as.
        nodes
            .create(NewOverlayNode {
                tenant_id: tid,
                node_ref: NodeRef::Agent {
                    agent_id: bson::oid::ObjectId::parse_str(&agent_id).unwrap(),
                },
                network_id,
                machine_id: machine.to_string(),
                name: machine.to_string(),
                overlay_ip: ip.to_string(),
                wg_public_key: B64.encode(pk),
                key_epoch: 0,
                endpoints: vec![],
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
            })
            .await
            .unwrap();
        let dial = Dial {
            url: url.clone(),
            credential: Credential::token(token),
        };
        let (mut ws, _) = ws_connect::dial(&dial, Duration::from_secs(10))
            .await
            .expect("the dial answered in time")
            .expect("the token in the header upgraded /derp");
        ws.send(pk.to_vec().into()).await.unwrap();
        sockets.push(ws);
    }
    let registered = |pk: [u8; 32]| {
        app.state
            .network()
            .derp_registry
            .contains_key(&(network_id, pk))
    };
    let mut both = false;
    for _ in 0..50 {
        if registered(pk_a) && registered(pk_b) {
            both = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(both, "both devices registered on their header dials");

    let (mut a, mut b) = (sockets.remove(0), sockets.remove(0));
    let mut frame = pk_b.to_vec();
    frame.extend_from_slice(payload);
    a.send(frame.into()).await.unwrap();
    let got = loop {
        match tokio::time::timeout(Duration::from_secs(5), b.next()).await {
            Ok(Some(Ok(msg))) if msg.is_binary() => break msg.into_data(),
            Ok(Some(Ok(_))) => continue,
            other => panic!("B never got A's frame: {other:?}"),
        }
    };
    assert_eq!(&got[..32], &pk_a, "the relay names the sender");
    assert_eq!(&got[32..], payload);
    assert_eq!(carrier_noted_for(&url), Carrier::Header, "on the header");

    let refused = ws_connect::dial(
        &Dial {
            url: url.clone(),
            credential: Credential::token("not-a-token"),
        },
        Duration::from_secs(10),
    )
    .await
    .expect("the dial answered in time");
    let shown = format!("{:?}", refused.expect_err("a bad token is refused"));
    assert!(refused_with_a_verdict(&shown), "{shown}");
    assert_eq!(carrier_noted_for(&url), Carrier::Header, "nothing noted");
}

/// The tunnel CLI's dial (`roomler_cli::forward::dial`) opens the
/// tunnel-client socket on its token in the header, and a refused token hears
/// the verdict that ends the dial.
#[tokio::test]
async fn the_tunnel_clis_dial_upgrades_on_its_header() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("hdr-cli").await;
    let token = enroll_tunnel_client(&app, &seeded, "m-hdr-cli").await;
    let url = format!(
        "ws://{}/ws?role=tunnel-client&tid={}",
        app.addr, seeded.tenant_id
    );

    let opened = roomler_cli::forward::dial(&url, &token)
        .await
        .expect("the dial answered in time");
    assert!(
        opened.is_ok(),
        "the token in the header upgraded: {:?}",
        opened.err()
    );
    assert_eq!(carrier_noted_for(&url), Carrier::Header, "on the header");

    let refused = roomler_cli::forward::dial(&url, "not-a-token")
        .await
        .expect("the dial answered in time");
    let shown = format!("{:?}", refused.expect_err("a bad token is refused"));
    assert!(refused_with_a_verdict(&shown), "{shown}");
    assert_eq!(carrier_noted_for(&url), Carrier::Header, "nothing noted");
}

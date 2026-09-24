// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-52 P3c — the loopback proof: an OUTSIDER logs into a device through the
//! server, and the device verifies.
//!
//! Everything real except the browser: a real API server, the REAL agent
//! signalling loop (`roomlerd`, with `external-access`) reading a real config
//! file for gate 3 and the password record, an org-less user on a real user
//! socket, and opaque-ke standing in for `@serenity-kit/opaque` — which is
//! opaque-ke compiled to WASM, and was proven to log into this agent's records
//! in P3a with the real library. The spec's bar for P3 is "proven on loopback
//! against the real agent first", as FR-19's bind handshake was.
//!
//! The server in the middle is exactly what these tests are about: it relays
//! three messages it cannot use, applies the two gates it owns, and — the
//! property the device-side tests cannot see — routes each answer back to the
//! right socket.

use std::time::{Duration, Instant};

use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use opaque_ke::{ClientLogin, ClientLoginFinishParameters, CredentialResponse};
use rand_opaque::rngs::OsRng;
use roomlerd::external_access::{Suite, set_password};
use roomlerd::{config::AgentConfig, encode::EncoderPreference, enrollment, signaling};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Built, never written: a secret scanner cannot tell a KDF input in a test
/// from a leaked credential.
fn pw(tag: &str) -> String {
    format!("fr52-not-a-credential-{tag}")
}

fn b64url() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

fn url(seeded: &SeededTenant, tail: &str) -> String {
    format!("/api/tenant/{}{}", seeded.tenant_id, tail)
}

async fn put(app: &TestApp, path: &str, token: &str, body: Value) -> u16 {
    app.auth_put(path, token)
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn post_json(app: &TestApp, path: &str, token: &str) -> (u16, Value) {
    let resp = app.auth_post(path, token).send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn enrol(app: &TestApp, seeded: &SeededTenant, machine_id: &str) -> AgentConfig {
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
    enrollment::enroll(enrollment::EnrollInputs {
        server_url: &app.base_url,
        enrollment_token: et["enrollment_token"].as_str().unwrap(),
        machine_id,
        machine_name: machine_id,
    })
    .await
    .expect("enrollment")
}

/// The real agent loop, mirroring `main.rs`'s wiring — with a REAL config path.
///
/// ⚠️ The shared harnesses hand the agent `unused-in-tests.toml`. That is fine
/// for gates read from the in-memory config, and wrong here: the device reads
/// gate 3 and the password record from its config FILE on every knock, so that
/// a revocation is live. A fake path would make every login `unavailable`.
fn spawn_agent(
    cfg: AgentConfig,
    config_path: std::path::PathBuf,
    stop_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (view_tx, _view_rx) = tokio::sync::watch::channel(Default::default());
        let broker = roomlerd::consent::ConsentBroker::new(
            roomlerd::consent::Mode::AutoGrant,
            std::env::temp_dir().join(format!("roomler-extauth-consent-{}", cfg.agent_id)),
        )
        .expect("consent broker init");
        let (exec_enabled, remote_config_enabled) = (cfg.exec_enabled, cfg.remote_config_enabled);
        let _ = signaling::run(
            signaling::OrgCtx::primary(),
            roomlerd::delegate::Delegation::Off,
            cfg,
            EncoderPreference::Software,
            stop_rx,
            connected,
            view_tx,
            Default::default(),
            broker,
            roomlerd::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            roomlerd::remote_config::RemoteConfigServices::new(
                config_path,
                std::sync::Arc::new(tokio::sync::Mutex::new(())),
                exec_enabled,
                remote_config_enabled,
            ),
            roomlerd::rc_sessions::RcSessionRegistry::new(),
        )
        .await;
    })
}

/// A device ready for external access: enrolled, a password set on it, gate 3
/// open in its config FILE, gate 1 and gate 2 open on the server, and a connect
/// code minted. Returns the code in its dictation form.
struct Device {
    code: String,
    agent_id: String,
    _dir: tempfile::TempDir,
    stop: tokio::sync::watch::Sender<bool>,
    _task: tokio::task::JoinHandle<()>,
}

async fn device(app: &TestApp, seeded: &SeededTenant, machine: &str, password: &str) -> Device {
    let mut cfg = enrol(app, seeded, machine).await;
    let (cred, _) = set_password(None, password).expect("registration");
    cfg.external_access_enabled = true;
    cfg.external_access_setup = Some(cred.setup);
    cfg.external_access_verifier = Some(cred.verifier);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    roomlerd::config::save(&path, &cfg).unwrap();
    let agent_id = cfg.agent_id.clone();

    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let task = spawn_agent(cfg, path, stop_rx);

    let owner = &seeded.admin.access_token;
    assert_eq!(
        put(
            app,
            &url(seeded, "/external-access"),
            owner,
            json!({ "enabled": true })
        )
        .await,
        200,
        "gate 1"
    );
    // Gate 2 can only be granted once the device's hello has recorded that it
    // advertises `external-access` — which the REAL agent does on connect.
    let policy = url(seeded, &format!("/agent/{agent_id}/external-access-policy"));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if put(app, &policy, owner, json!({ "approved": true })).await == 200 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "gate 2 could not be granted: the agent never reported `external-access`"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let (s, body) = post_json(
        app,
        &url(seeded, &format!("/agent/{agent_id}/connect-code")),
        owner,
    )
    .await;
    assert_eq!(s, 200, "{body}");
    Device {
        code: body["connect_code"].as_str().unwrap().to_string(),
        agent_id,
        _dir: dir,
        stop,
        _task: task,
    }
}

/// An org-less account on a real user socket — the outsider.
async fn outsider(app: &TestApp, tag: &str) -> (String, Ws) {
    let user = app
        .register_user(
            &format!("{tag}@outside.test"),
            tag,
            "An Outsider",
            &pw(&format!("{tag}-account")),
            None,
            None,
        )
        .await;
    let ws_url = format!("ws://{}/ws?token={}", app.addr, user.access_token);
    let (mut ws, _) = connect_async(&ws_url).await.expect("user socket");
    // Drain whatever the server greets a socket with.
    let _ = tokio::time::timeout(Duration::from_millis(500), ws.next()).await;
    (user.id, ws)
}

async fn send(ws: &mut Ws, frame: Value) {
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// The next frame whose tag is one of `tags`, skipping unrelated traffic.
async fn next_of(ws: &mut Ws, tags: &[&str]) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = tokio::time::timeout(left, ws.next())
            .await
            .unwrap_or_else(|_| panic!("no {tags:?} within 15 s"))
            .expect("socket open")
            .expect("frame");
        if let Message::Text(t) = msg
            && let Ok(v) = serde_json::from_str::<Value>(&t)
            && tags.contains(&v["t"].as_str().unwrap_or(""))
        {
            return v;
        }
    }
}

fn client_ke1(password: &str) -> (ClientLogin<Suite>, String) {
    let s = ClientLogin::<Suite>::start(&mut OsRng, password.as_bytes()).unwrap();
    (s.state, b64url().encode(s.message.serialize()))
}

/// Open KE2. `None` = the client refused it — what a wrong password looks like.
fn client_ke3(state: ClientLogin<Suite>, password: &str, ke2: &str) -> Option<String> {
    let ke2 = b64url().decode(ke2).ok()?;
    let done = state
        .finish(
            &mut OsRng,
            password.as_bytes(),
            CredentialResponse::<Suite>::deserialize(&ke2).ok()?,
            ClientLoginFinishParameters::default(),
        )
        .ok()?;
    Some(b64url().encode(done.message.serialize()))
}

/// THE loopback proof: KE1 → the device, KE2 → the browser, KE3 → the device,
/// VERIFIED → the browser — and a `login` row in the org's audit log.
#[tokio::test]
async fn an_outsider_logs_in_through_the_server_and_the_device_verifies() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-ok").await;
    let dev = device(&app, &seeded, "mach-extauth-ok", &pw("device")).await;
    let (_outsider_id, mut ws) = outsider(&app, "extok").await;

    let (state, ke1) = client_ke1(&pw("device"));
    send(
        &mut ws,
        json!({"t": "rc:extauth.start", "connect_code": dev.code, "ke1": ke1}),
    )
    .await;
    let challenge = next_of(&mut ws, &["rc:extauth.challenge", "rc:extauth.result"]).await;
    assert_eq!(
        challenge["t"], "rc:extauth.challenge",
        "the device answered KE1: {challenge}"
    );
    let attempt_id = challenge["attempt_id"].as_str().unwrap().to_string();

    let ke3 = client_ke3(state, &pw("device"), challenge["ke2"].as_str().unwrap())
        .expect("the right password opens the device's KE2");
    send(
        &mut ws,
        json!({"t": "rc:extauth.finish", "connect_code": dev.code, "attempt_id": attempt_id, "ke3": ke3}),
    )
    .await;
    let result = next_of(&mut ws, &["rc:extauth.result"]).await;
    assert_eq!(result["attempt_id"], json!(attempt_id));
    assert!(
        result.get("refused").is_none(),
        "the device must have VERIFIED the login: {result}"
    );

    // The org sees it. Written after the reply, so allow it a moment.
    let audit = url(&seeded, "/external-rc-audit");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let body: Value = app
            .auth_get(&audit, &seeded.admin.access_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let row = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["action"] == "login" && i["attempt_id"] == json!(attempt_id))
            .cloned();
        if let Some(row) = row {
            assert!(
                row.get("login_refused").is_none(),
                "a verified login: {row}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "no `login` audit row: {body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = dev.stop.send(true);
}

/// A wrong password is decided in the BROWSER — and still costs the device a
/// guess. Five abandoned logins, then the device throttles the sixth, through
/// the real server, with the retry time the outsider needs.
///
/// This is P3a's measurement (the client learns the answer at KE2 and never
/// sends KE3) turned into an end-to-end property: if the device counted only
/// failed KE3s, the sixth knock would be answered like the first.
#[tokio::test]
async fn wrong_guesses_are_counted_at_ke1_and_the_device_throttles() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-throttle").await;
    let dev = device(&app, &seeded, "mach-extauth-throttle", &pw("device")).await;
    let (_id, mut ws) = outsider(&app, "extguess").await;

    for n in 0..5 {
        let (state, ke1) = client_ke1(&pw("guess"));
        send(
            &mut ws,
            json!({"t": "rc:extauth.start", "connect_code": dev.code, "ke1": ke1}),
        )
        .await;
        let challenge = next_of(&mut ws, &["rc:extauth.challenge", "rc:extauth.result"]).await;
        assert_eq!(
            challenge["t"], "rc:extauth.challenge",
            "guess {n} is answered: {challenge}"
        );
        assert!(
            client_ke3(state, &pw("guess"), challenge["ke2"].as_str().unwrap()).is_none(),
            "a wrong password must fail in the client, at KE2"
        );
        // …and the client stops. No finish, ever.
    }
    let (_, ke1) = client_ke1(&pw("guess"));
    send(
        &mut ws,
        json!({"t": "rc:extauth.start", "connect_code": dev.code, "ke1": ke1}),
    )
    .await;
    let result = next_of(&mut ws, &["rc:extauth.challenge", "rc:extauth.result"]).await;
    assert_eq!(
        result["t"], "rc:extauth.result",
        "the sixth guess is NOT answered: {result}"
    );
    assert_eq!(result["refused"], "throttled", "{result}");
    // Real time passes between the fifth answer and this knock (the client's
    // Argon2 plus two round trips), and the device rounds the rest of its 30 s
    // UP — measured 29 here. The exact value is the unit test's job; end to
    // end, "a real wait, never more than the backoff" is what holds.
    let retry = result["retry_after_secs"].as_u64().expect("a retry time");
    assert!((1..=30).contains(&retry), "{result}");
    let _ = dev.stop.send(true);
}

/// No such code, the org's switch off, the device not approved: the outsider
/// gets the SAME answer for each, and the org's audit log tells them apart.
#[tokio::test]
async fn every_server_refusal_looks_the_same_to_the_outsider() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-gates").await;
    let dev = device(&app, &seeded, "mach-extauth-gates", &pw("device")).await;
    let (_id, mut ws) = outsider(&app, "extgates").await;
    let owner = &seeded.admin.access_token;
    let cfg_id = dev.agent_id.clone();

    let mut answers = Vec::new();
    let knock = |code: String| {
        let (_, ke1) = client_ke1(&pw("device"));
        json!({"t": "rc:extauth.start", "connect_code": code, "ke1": ke1})
    };

    // 1. A code nobody was ever given.
    send(&mut ws, knock("0000-0000-0000".into())).await;
    answers.push(next_of(&mut ws, &["rc:extauth.challenge", "rc:extauth.result"]).await);

    // 2. The org switches external access off (gate 1).
    assert_eq!(
        put(
            &app,
            &url(&seeded, "/external-access"),
            owner,
            json!({"enabled": false})
        )
        .await,
        200
    );
    send(&mut ws, knock(dev.code.clone())).await;
    answers.push(next_of(&mut ws, &["rc:extauth.challenge", "rc:extauth.result"]).await);

    // 3. Back on, but the device's approval withdrawn (gate 2).
    assert_eq!(
        put(
            &app,
            &url(&seeded, "/external-access"),
            owner,
            json!({"enabled": true})
        )
        .await,
        200
    );
    assert_eq!(
        put(
            &app,
            &url(&seeded, &format!("/agent/{cfg_id}/external-access-policy")),
            owner,
            json!({"approved": false})
        )
        .await,
        200
    );
    send(&mut ws, knock(dev.code.clone())).await;
    answers.push(next_of(&mut ws, &["rc:extauth.challenge", "rc:extauth.result"]).await);

    for a in &answers {
        assert_eq!(
            a,
            &json!({"t": "rc:extauth.result", "refused": "unavailable"}),
            "every server refusal must be byte-identical — no attempt id, no hint"
        );
    }
    let _ = dev.stop.send(true);
}

/// Only the principal who STARTED an attempt can finish it — and a stranger's
/// try does not burn it for its owner.
#[tokio::test]
async fn only_the_starter_can_finish_an_attempt() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-owner").await;
    let dev = device(&app, &seeded, "mach-extauth-owner", &pw("device")).await;
    let (_a, mut ws_a) = outsider(&app, "extownera").await;
    let (_b, mut ws_b) = outsider(&app, "extownerb").await;

    let (state, ke1) = client_ke1(&pw("device"));
    send(
        &mut ws_a,
        json!({"t": "rc:extauth.start", "connect_code": dev.code, "ke1": ke1}),
    )
    .await;
    let challenge = next_of(&mut ws_a, &["rc:extauth.challenge", "rc:extauth.result"]).await;
    let attempt_id = challenge["attempt_id"].as_str().unwrap().to_string();
    let ke3 = client_ke3(state, &pw("device"), challenge["ke2"].as_str().unwrap()).unwrap();

    // B holds a perfectly valid KE3 for A's attempt, and is told it does not exist.
    send(
        &mut ws_b,
        json!({"t": "rc:extauth.finish", "connect_code": dev.code, "attempt_id": attempt_id, "ke3": ke3}),
    )
    .await;
    let b_result = next_of(&mut ws_b, &["rc:extauth.result"]).await;
    assert_eq!(b_result["refused"], "unknown_attempt", "{b_result}");

    // A's attempt is intact.
    send(
        &mut ws_a,
        json!({"t": "rc:extauth.finish", "connect_code": dev.code, "attempt_id": attempt_id, "ke3": ke3}),
    )
    .await;
    let a_result = next_of(&mut ws_a, &["rc:extauth.result"]).await;
    assert!(
        a_result.get("refused").is_none(),
        "A still verifies: {a_result}"
    );
    let _ = dev.stop.send(true);
}

/// P3d — the outsider lands on the OTHER pod, and still logs in.
///
/// An outsider has no tenant, so tenant affinity cannot put their socket on the
/// pod that holds the device, and `/ws` refuses a `tid` they are not a member
/// of. Before P3d this login answered `unavailable`. Now both frames are
/// forwarded over the PR-2 relay to the device's pod, handled there, and
/// answered back to THIS socket — including `finish`, whose attempt lives only
/// on the other pod.
///
/// ⚠️ Needs a cluster bus (Redis). Without one it cannot run and says so
/// loudly rather than passing on nothing.
#[tokio::test]
async fn an_outsider_on_the_other_pod_still_logs_in() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_bus.is_none() {
        eprintln!(
            "SKIPPED an_outsider_on_the_other_pod_still_logs_in: no Redis — P3d unproven here"
        );
        return;
    }
    for _ in 0..40 {
        let (a, b) = (
            app1.state.cluster_bus.as_ref().unwrap(),
            app2.state.cluster_bus.as_ref().unwrap(),
        );
        if a.sub_alive.load(std::sync::atomic::Ordering::Relaxed)
            && b.sub_alive.load(std::sync::atomic::Ordering::Relaxed)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let seeded = app1.seed_tenant("extauth-xpod").await;
    // The device is homed on pod 1…
    let dev = device(&app1, &seeded, "mach-extauth-xpod", &pw("device")).await;
    // …and the outsider lands on pod 2.
    let (_id, mut ws) = outsider(&app2, "extxpod").await;

    let (state, ke1) = client_ke1(&pw("device"));
    send(
        &mut ws,
        json!({"t": "rc:extauth.start", "connect_code": dev.code, "ke1": ke1}),
    )
    .await;
    let challenge = next_of(&mut ws, &["rc:extauth.challenge", "rc:extauth.result"]).await;
    assert_eq!(
        challenge["t"], "rc:extauth.challenge",
        "the device on the OTHER pod answered KE1: {challenge}"
    );
    let attempt_id = challenge["attempt_id"].as_str().unwrap().to_string();
    let ke3 = client_ke3(state, &pw("device"), challenge["ke2"].as_str().unwrap())
        .expect("the right password opens the device's KE2");
    send(
        &mut ws,
        json!({"t": "rc:extauth.finish", "connect_code": dev.code, "attempt_id": attempt_id, "ke3": ke3}),
    )
    .await;
    let result = next_of(&mut ws, &["rc:extauth.result"]).await;
    assert!(
        result.get("refused").is_none(),
        "verified across pods — the attempt lives on pod 1, the socket on pod 2: {result}"
    );
    let _ = dev.stop.send(true);
}

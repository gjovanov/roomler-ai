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
        install: None,
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
            signaling::OrgCtx::primary(roomlerd::token_slot::TokenSlot::new(
                cfg.agent_token.clone(),
            )),
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
    dir: tempfile::TempDir,
    stop: tokio::sync::watch::Sender<bool>,
    _task: tokio::task::JoinHandle<()>,
}

impl Device {
    /// Gate 3, in the config FILE, as the person at the machine sets it. The
    /// device reads it at every knock.
    fn set_gate_3(&self, open: bool) {
        let path = self.dir.path().join("config.toml");
        let mut cfg = roomlerd::config::load(&path).unwrap();
        cfg.external_access_enabled = open;
        roomlerd::config::save(&path, &cfg).unwrap();
    }
}

async fn device(app: &TestApp, seeded: &SeededTenant, machine: &str, password: &str) -> Device {
    device_with(app, seeded, machine, password, Terms::default()).await
}

/// P4 — the device's own terms for an external session, in its config file.
#[derive(Default, Clone, Copy)]
struct Terms {
    /// `external_consent_mode`. The tests that open a session say `auto`: the
    /// test agent has no prompt surface, and `prompt` would wait out the
    /// attended window. The ordering of consent and offer is proven where it
    /// can be made deterministic — the device's and the Hub's unit tests.
    consent: Option<&'static str>,
    /// `external_max_permissions`.
    ceiling: Option<&'static str>,
}

async fn device_with(
    app: &TestApp,
    seeded: &SeededTenant,
    machine: &str,
    password: &str,
    terms: Terms,
) -> Device {
    let mut cfg = enrol(app, seeded, machine).await;
    let (cred, _) = set_password(None, password).expect("registration");
    cfg.external_access_enabled = true;
    cfg.external_access_setup = Some(cred.setup);
    cfg.external_access_verifier = Some(cred.verifier);
    cfg.external_consent_mode = terms.consent.map(str::to_string);
    cfg.external_max_permissions = terms.ceiling.map(str::to_string);
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
        dir,
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

    // 4. Gate 2 back, and the DEVICE says no (gate 3). Its `unavailable` must
    //    not carry the attempt id: that would say the code resolved, the org's
    //    gates are open and the device is online.
    assert_eq!(
        put(
            &app,
            &url(&seeded, &format!("/agent/{cfg_id}/external-access-policy")),
            owner,
            json!({"approved": true})
        )
        .await,
        200
    );
    dev.set_gate_3(false);
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

// ─── P4 — the session a verified login opens ─────────────────────────────────
//
// The browser's half is written here from the SPEC, not from the device's code:
// the AppKey derivation, the MAC and the fingerprint normalisation below are an
// independent implementation (HKDF from two HMACs, per RFC 5869), so a device
// that drifted from the spec fails these tests instead of agreeing with itself.

/// `HKDF-SHA512(salt = none, ikm = the OPAQUE session key)`, expanded under the
/// label and the two length-prefixed fields — the browser's AppKey.
fn app_key(session_key: &[u8], attempt_id: &str, principal: &str) -> [u8; 32] {
    use hmac::Mac as _;
    type H = hmac::Hmac<sha2::Sha512>;
    // Extract with no salt: a hash-length block of zeros (RFC 5869 §2.2).
    let mut extract = <H as hmac::Mac>::new_from_slice(&[0u8; 64]).unwrap();
    extract.update(session_key);
    let prk = extract.finalize().into_bytes();
    // Expand, one block: T(1) = HMAC(PRK, info ‖ 0x01), truncated to 32.
    let mut info = b"roomler-extauth-v1 app-key".to_vec();
    for field in [attempt_id.as_bytes(), principal.as_bytes()] {
        info.extend_from_slice(&(field.len() as u16).to_be_bytes());
        info.extend_from_slice(field);
    }
    let mut expand = <H as hmac::Mac>::new_from_slice(&prk).unwrap();
    expand.update(&info);
    expand.update(&[1u8]);
    expand.finalize().into_bytes()[..32].try_into().unwrap()
}

/// `HMAC-SHA256(AppKey, label ‖ u16be ‖ role ‖ u16be ‖ fingerprint)`, base64url.
fn transport_mac(key: &[u8; 32], role: &str, fingerprint: &str) -> String {
    use hmac::Mac as _;
    let mut mac = <hmac::Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(key).unwrap();
    mac.update(b"roomler-extauth-v1 transport");
    for field in [role.as_bytes(), fingerprint.as_bytes()] {
        mac.update(&(field.len() as u16).to_be_bytes());
        mac.update(field);
    }
    b64url().encode(mac.finalize().into_bytes())
}

/// The SDP's one certificate fingerprint: `lowercase(hash) + " " + UPPERCASE(hex)`.
fn fingerprint(sdp: &str) -> String {
    let value = sdp
        .lines()
        .find_map(|l| l.trim().strip_prefix("a=fingerprint:"))
        .expect("an SDP with a DTLS fingerprint");
    let (hash, hex) = value.trim().split_once(' ').unwrap();
    format!(
        "{} {}",
        hash.to_ascii_lowercase(),
        hex.trim().to_ascii_uppercase()
    )
}

/// A whole login as a browser runs it: the attempt, and the key the BROWSER
/// derives from its own side of the exchange.
async fn login(ws: &mut Ws, code: &str, password: &str, principal: &str) -> (String, [u8; 32]) {
    let (state, ke1) = client_ke1(password);
    send(
        ws,
        json!({"t": "rc:extauth.start", "connect_code": code, "ke1": ke1}),
    )
    .await;
    let challenge = next_of(ws, &["rc:extauth.challenge", "rc:extauth.result"]).await;
    assert_eq!(challenge["t"], "rc:extauth.challenge", "{challenge}");
    let attempt_id = challenge["attempt_id"].as_str().unwrap().to_string();
    let ke2 = b64url().decode(challenge["ke2"].as_str().unwrap()).unwrap();
    let done = state
        .finish(
            &mut OsRng,
            password.as_bytes(),
            CredentialResponse::<Suite>::deserialize(&ke2).unwrap(),
            ClientLoginFinishParameters::default(),
        )
        .expect("the right password opens the device's KE2");
    send(
        ws,
        json!({
            "t": "rc:extauth.finish",
            "connect_code": code,
            "attempt_id": attempt_id,
            "ke3": b64url().encode(done.message.serialize()),
        }),
    )
    .await;
    let result = next_of(ws, &["rc:extauth.result"]).await;
    assert!(result.get("refused").is_none(), "not verified: {result}");
    let key = app_key(&done.session_key, &attempt_id, principal);
    (attempt_id, key)
}

/// A browser-side peer and its offer: a data channel, so the offer has an
/// m-section, and so a DTLS fingerprint.
async fn browser_offer() -> (
    std::sync::Arc<webrtc::peer_connection::RTCPeerConnection>,
    String,
) {
    use webrtc::api::APIBuilder;
    use webrtc::api::media_engine::MediaEngine;
    use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
    use webrtc::peer_connection::configuration::RTCConfiguration;
    let mut media = MediaEngine::default();
    media.register_default_codecs().unwrap();
    let api = APIBuilder::new().with_media_engine(media).build();
    let pc = std::sync::Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );
    let _dc = pc
        .create_data_channel("control", Some(RTCDataChannelInit::default()))
        .await
        .unwrap();
    let offer = pc.create_offer(None).await.unwrap();
    pc.set_local_description(offer.clone()).await.unwrap();
    (pc, offer.sdp)
}

/// Ask for the session a verified login earned; wait for `rc:session.created`
/// and `rc:ready` (the device consents by itself: `external_consent_mode =
/// auto`). Returns the session id.
async fn open_session(ws: &mut Ws, code: &str, attempt_id: &str, permissions: &str) -> String {
    open_session_ready(ws, code, attempt_id, permissions)
        .await
        .0
}

/// [`open_session`], with the `rc:ready` frame too: its ICE servers.
async fn open_session_ready(
    ws: &mut Ws,
    code: &str,
    attempt_id: &str,
    permissions: &str,
) -> (String, Value) {
    send(
        ws,
        json!({
            "t": "rc:extauth.session",
            "connect_code": code,
            "attempt_id": attempt_id,
            "permissions": permissions,
        }),
    )
    .await;
    let created = next_of(ws, &["rc:session.created", "rc:extauth.result", "rc:error"]).await;
    assert_eq!(created["t"], "rc:session.created", "{created}");
    assert_eq!(
        created["agent_id"], "000000000000000000000000",
        "an outsider named a connect code, and is never told the internal id: {created}"
    );
    let session_id = created["session_id"].as_str().unwrap().to_string();
    let ready = next_of(ws, &["rc:ready", "rc:terminate", "rc:error"]).await;
    assert_eq!(
        ready["t"], "rc:ready",
        "the device admitted the session and consented: {ready}"
    );
    (session_id, ready)
}

/// The device's terms for the tests that open a session.
fn unattended() -> Terms {
    Terms {
        consent: Some("auto"),
        ..Terms::default()
    }
}

/// THE P4 proof, end to end through the real server and the real agent: a
/// verified login opens a session; the offer carries a MAC over the browser's
/// certificate, under a key only the two ends hold; the device answers with a
/// MAC over ITS certificate under the same key; and the org's log has a
/// `session` row naming the session and the login it spent.
#[tokio::test]
async fn an_outsider_opens_a_session_bound_to_their_login() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-session").await;
    let dev = device_with(
        &app,
        &seeded,
        "mach-extauth-session",
        &pw("device"),
        unattended(),
    )
    .await;
    let (principal, mut ws) = outsider(&app, "extsession").await;

    let (attempt_id, key) = login(&mut ws, &dev.code, &pw("device"), &principal).await;
    let session_id = open_session(&mut ws, &dev.code, &attempt_id, "VIEW | INPUT").await;

    let (pc, offer) = browser_offer().await;
    send(
        &mut ws,
        json!({
            "t": "rc:sdp.offer",
            "session_id": session_id,
            "sdp": offer,
            "extauth_mac": transport_mac(&key, "offer", &fingerprint(&offer)),
        }),
    )
    .await;
    let answer = next_of(&mut ws, &["rc:sdp.answer", "rc:terminate", "rc:error"]).await;
    assert_eq!(
        answer["t"], "rc:sdp.answer",
        "the device accepted the sealed offer: {answer}"
    );
    let answer_sdp = answer["sdp"].as_str().unwrap();
    assert_eq!(
        answer["extauth_mac"].as_str(),
        Some(transport_mac(&key, "answer", &fingerprint(answer_sdp)).as_str()),
        "the answer is sealed over the DEVICE's certificate, under the login's key"
    );
    assert_ne!(
        fingerprint(answer_sdp),
        fingerprint(&offer),
        "two certificates, one per end"
    );
    pc.set_remote_description(
        webrtc::peer_connection::sdp::session_description::RTCSessionDescription::answer(
            answer_sdp.to_string(),
        )
        .unwrap(),
    )
    .await
    .expect("the browser accepts the device's answer");

    // The org sees who got in, on which login, into which session.
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
            .find(|i| i["action"] == "session" && i["attempt_id"] == json!(attempt_id))
            .cloned();
        if let Some(row) = row {
            assert!(row.get("login_refused").is_none(), "admitted: {row}");
            assert_eq!(row["session_id"], json!(session_id), "{row}");
            assert_eq!(row["user_id"], json!(principal), "{row}");
            break;
        }
        assert!(Instant::now() < deadline, "no `session` audit row: {body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = pc.close().await;
    let _ = dev.stop.send(true);
}

/// What binds every controller of a device binds an outsider too (FR-101). The
/// org says `files: off` and sets an idle limit; gate 2's ceiling allows FILES
/// and the outsider asks for it. The session opens without FILES, names the
/// org as the reason, and runs under the org's idle limit. FR-52 was built
/// before FR-101 existed. Rebased onto it, the extauth relay first handed the
/// Hub an unrestricted policy and no limits, so FILES survived and an
/// outsider's session could sit idle for ever: the outside door looser than
/// the inside one.
#[tokio::test]
async fn an_outsiders_session_is_bound_by_the_orgs_session_policy() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-policy").await;
    let dev = device_with(
        &app,
        &seeded,
        "mach-extauth-policy",
        &pw("device"),
        unattended(),
    )
    .await;
    let owner = &seeded.admin.access_token;
    // The ceiling allows FILES: what must take it out is the org's POLICY.
    let policy = url(
        &seeded,
        &format!("/agent/{}/external-access-policy", dev.agent_id),
    );
    let ceiling = json!({ "approved": true, "max_permissions": "VIEW | INPUT | FILES" });
    assert_eq!(put(&app, &policy, owner, ceiling).await, 200);
    let files_off = json!({ "files": "off" });
    assert_eq!(
        put(&app, &url(&seeded, "/rc-session-policy"), owner, files_off).await,
        200
    );
    let idle = json!({ "idle_secs": 600 });
    assert_eq!(
        put(&app, &url(&seeded, "/rc-session-limits"), owner, idle).await,
        200
    );

    let (principal, mut ws) = outsider(&app, "extpolicy").await;
    let (attempt_id, _key) = login(&mut ws, &dev.code, &pw("device"), &principal).await;
    send(
        &mut ws,
        json!({
            "t": "rc:extauth.session",
            "connect_code": dev.code,
            "attempt_id": attempt_id,
            "permissions": "VIEW | INPUT | FILES",
        }),
    )
    .await;
    let created = next_of(
        &mut ws,
        &["rc:session.created", "rc:extauth.result", "rc:error"],
    )
    .await;
    assert_eq!(created["t"], "rc:session.created", "{created}");
    let granted = created["permissions"].as_str().unwrap_or_default();
    assert!(granted.contains("VIEW"), "{created}");
    assert!(
        !granted.contains("FILES"),
        "the org's `files: off` binds an outsider: {created}"
    );
    assert_eq!(
        created["features_refused"]["files"], "org_policy_off",
        "{created}"
    );
    assert_eq!(
        created["idle_secs"], 600,
        "the org's idle limit binds an outsider: {created}"
    );
    let _ = dev.stop.send(true);
}

/// The binding is enforced BY THE DEVICE: an offer that does not carry the
/// login's MAC over its own certificate — a key from nowhere, no MAC at all, or
/// the right key over a swapped certificate — ends the session, and no answer
/// ever comes back. This is what stops a server that relays the signalling from
/// substituting its own DTLS endpoint for the browser's.
#[tokio::test]
async fn an_offer_the_login_did_not_seal_ends_the_session() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-badmac").await;
    let dev = device_with(
        &app,
        &seeded,
        "mach-extauth-badmac",
        &pw("device"),
        unattended(),
    )
    .await;
    let (principal, mut ws) = outsider(&app, "extbadmac").await;

    // A genuine tag over a certificate that is not the offer's: what an
    // interloper who saw a real offer could replay onto its own.
    const NOT_THE_OFFERS: &str = "sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:\
                                  11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00";
    for forgery in ["another key", "no mac", "swapped certificate"] {
        let (attempt_id, key) = login(&mut ws, &dev.code, &pw("device"), &principal).await;
        let session_id = open_session(&mut ws, &dev.code, &attempt_id, "VIEW").await;
        let (pc, offer) = browser_offer().await;
        let mac = match forgery {
            "another key" => Some(transport_mac(&[0x42; 32], "offer", &fingerprint(&offer))),
            "no mac" => None,
            _ => Some(transport_mac(&key, "offer", NOT_THE_OFFERS)),
        };
        let mut frame = json!({"t": "rc:sdp.offer", "session_id": session_id, "sdp": offer});
        if let Some(mac) = mac {
            frame["extauth_mac"] = json!(mac);
        }
        send(&mut ws, frame).await;
        let verdict = next_of(&mut ws, &["rc:sdp.answer", "rc:terminate", "rc:error"]).await;
        assert_eq!(
            verdict["t"], "rc:terminate",
            "{forgery}: the device must end the session, never answer it: {verdict}"
        );
        assert_eq!(
            verdict["session_id"],
            json!(session_id),
            "{forgery}: {verdict}"
        );
        assert_eq!(
            verdict["reason"], "agent_hangup",
            "{forgery}: ended by the device's own act: {verdict}"
        );
        let _ = pc.close().await;
    }
    let _ = dev.stop.send(true);
}

/// One verified login opens ONE session, and only for the principal who logged
/// in: another outsider holding the attempt id, a second ask on the same login,
/// and an attempt nobody verified are all `unknown_attempt`.
#[tokio::test]
async fn a_login_opens_one_session_for_its_own_principal() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-once").await;
    let dev = device_with(
        &app,
        &seeded,
        "mach-extauth-once",
        &pw("device"),
        unattended(),
    )
    .await;
    let (principal, mut ws_a) = outsider(&app, "extoncea").await;
    let (_b, mut ws_b) = outsider(&app, "extonceb").await;

    let (attempt_id, _key) = login(&mut ws_a, &dev.code, &pw("device"), &principal).await;
    let ask = |attempt: &str| {
        json!({
            "t": "rc:extauth.session",
            "connect_code": dev.code,
            "attempt_id": attempt,
            "permissions": "VIEW",
        })
    };

    send(&mut ws_b, ask(&attempt_id)).await;
    let stolen = next_of(&mut ws_b, &["rc:extauth.result", "rc:session.created"]).await;
    assert_eq!(stolen["refused"], "unknown_attempt", "{stolen}");

    // A's login survived B's try.
    let _session = open_session(&mut ws_a, &dev.code, &attempt_id, "VIEW").await;

    send(&mut ws_a, ask(&attempt_id)).await;
    let twice = next_of(&mut ws_a, &["rc:extauth.result", "rc:session.created"]).await;
    assert_eq!(
        twice["refused"], "unknown_attempt",
        "one login, one session: {twice}"
    );

    send(&mut ws_a, ask("66f0c0ffee0000000000abcd")).await;
    let never = next_of(&mut ws_a, &["rc:extauth.result", "rc:session.created"]).await;
    assert_eq!(never["refused"], "unknown_attempt", "{never}");
    let _ = dev.stop.send(true);
}

/// P4 across pods — the outsider on pod 2, the device on pod 1. The session is
/// opened on pod 1 with the relay's proxy sender, and every later frame of it
/// (the sealed offer, the sealed answer) crosses the PR-2 relay in both
/// directions with its MAC intact: the server carries a field it cannot check
/// and must not drop.
///
/// ⚠️ Needs a cluster bus (Redis). Without one it says so loudly rather than
/// passing on nothing.
#[tokio::test]
async fn an_outsider_on_the_other_pod_opens_a_sealed_session() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_bus.is_none() {
        eprintln!(
            "SKIPPED an_outsider_on_the_other_pod_opens_a_sealed_session: no Redis — P4 \
             cross-pod unproven here"
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

    let seeded = app1.seed_tenant("extauth-xpod-session").await;
    let dev = device_with(
        &app1,
        &seeded,
        "mach-extauth-xpod-session",
        &pw("device"),
        unattended(),
    )
    .await;
    let (principal, mut ws) = outsider(&app2, "extxpodsession").await;

    let (attempt_id, key) = login(&mut ws, &dev.code, &pw("device"), &principal).await;
    let session_id = open_session(&mut ws, &dev.code, &attempt_id, "VIEW | INPUT").await;
    let (pc, offer) = browser_offer().await;
    send(
        &mut ws,
        json!({
            "t": "rc:sdp.offer",
            "session_id": session_id,
            "sdp": offer,
            "extauth_mac": transport_mac(&key, "offer", &fingerprint(&offer)),
        }),
    )
    .await;
    let answer = next_of(&mut ws, &["rc:sdp.answer", "rc:terminate", "rc:error"]).await;
    assert_eq!(
        answer["t"], "rc:sdp.answer",
        "the offer crossed to pod 1 with its MAC, the answer came back: {answer}"
    );
    let answer_sdp = answer["sdp"].as_str().unwrap();
    assert_eq!(
        answer["extauth_mac"].as_str(),
        Some(transport_mac(&key, "answer", &fingerprint(answer_sdp)).as_str()),
        "the seal survived the relay"
    );
    let _ = pc.close().await;
    let _ = dev.stop.send(true);
}

// ─── The criteria P4 left open: each against the real agent ──────────────────

/// Every `tracing` line this thread emits while the guard lives, at TRACE.
///
/// A `#[tokio::test]` runs the server, the agent and the test on one thread, so
/// the thread's default subscriber sees all three. Every use asserts a line of
/// the server's and of the device's is in here, so a capture that saw nothing
/// fails rather than passes.
#[derive(Clone, Default)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;
    fn make_writer(&'a self) -> LogCapture {
        self.clone()
    }
}

impl LogCapture {
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(self.clone())
                .finish(),
        )
    }

    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }
}

/// The spellings of a secret a log line, a frame or a stored field could hold.
fn spellings(secret: &str) -> Vec<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    let hex: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
    vec![
        secret.as_bytes().to_vec(),
        STANDARD.encode(secret).into_bytes(),
        URL_SAFE_NO_PAD.encode(secret).into_bytes(),
        hex.into_bytes(),
        secret.encode_utf16().flat_map(u16::to_le_bytes).collect(),
    ]
}

fn holds(haystack: &[u8], secret: &str) -> bool {
    spellings(secret)
        .iter()
        .any(|needle| haystack.windows(needle.len()).any(|w| w == needle))
}

/// Every document of every collection in the test's database, as stored: the raw
/// BSON, so a binary field is searched too. Answers the collection names as well.
async fn dump_collections(app: &TestApp) -> (Vec<String>, Vec<u8>) {
    use futures::TryStreamExt as _;
    let names = app.db.list_collection_names().await.unwrap();
    let mut raw = Vec::new();
    for name in &names {
        let mut cursor = app
            .db
            .collection::<bson::RawDocumentBuf>(name)
            .find(bson::doc! {})
            .await
            .unwrap();
        while let Some(doc) = cursor.try_next().await.unwrap() {
            raw.extend_from_slice(doc.as_bytes());
        }
    }
    (names, raw)
}

/// An outsider's socket that keeps every frame in both directions, for the
/// capture: what the server received, and what it said.
struct TappedSocket<'a> {
    ws: &'a mut Ws,
    frames: Vec<u8>,
}

impl TappedSocket<'_> {
    async fn send(&mut self, frame: Value) {
        self.frames.extend_from_slice(frame.to_string().as_bytes());
        send(self.ws, frame).await;
    }

    async fn next(&mut self, tags: &[&str]) -> Value {
        let frame = next_of(self.ws, tags).await;
        self.frames.extend_from_slice(frame.to_string().as_bytes());
        frame
    }
}

/// AC: the password never appears in any server-side log, request body or
/// collection. The whole exchange, from the first frame to a sealed answer,
/// across two pods when a bus is there so the relay's path is in it, with every
/// `tracing` line of the server and the device at TRACE, every frame, and every
/// collection as stored.
///
/// A capture that finds nothing proves nothing unless it could have found it, so
/// a canary goes down each of the three lanes and must be found.
///
/// The client here is opaque-ke standing in for the browser library, the same
/// code compiled to WebAssembly. The real page's frames have their own capture in
/// `ui/src/__tests__/composables/useExternalLogin.spec.ts`.
#[tokio::test]
async fn the_password_is_in_no_log_no_frame_and_no_collection() {
    const CANARY: &str = "fr52-capture-canary-8c1f";
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    let two_pods = app1.state.cluster_bus.is_some();
    if !two_pods {
        eprintln!("the capture runs on one pod: no bus, so the relay's path is not in it");
    }
    let outsiders_pod = if two_pods { &app2 } else { &app1 };

    let capture = LogCapture::default();
    let _logs = capture.install();

    let seeded = app1.seed_tenant("extauth-capture").await;
    let password = pw("captured-device-password-3e7d");
    let dev = device_with(
        &app1,
        &seeded,
        "mach-extauth-capture",
        &password,
        unattended(),
    )
    .await;
    let (principal, mut ws) = outsider(outsiders_pod, "extcapture").await;
    let mut tap = TappedSocket {
        ws: &mut ws,
        frames: Vec::new(),
    };

    // A wrong guess first: the path that fails is a path too.
    let (wrong, ke1) = client_ke1(&pw("a-wrong-guess"));
    tap.send(json!({"t": "rc:extauth.start", "connect_code": dev.code, "ke1": ke1}))
        .await;
    let challenge = tap
        .next(&["rc:extauth.challenge", "rc:extauth.result"])
        .await;
    assert_eq!(challenge["t"], "rc:extauth.challenge", "{challenge}");
    assert!(
        client_ke3(
            wrong,
            &pw("a-wrong-guess"),
            challenge["ke2"].as_str().unwrap()
        )
        .is_none()
    );

    // The real login.
    let started = ClientLogin::<Suite>::start(&mut OsRng, password.as_bytes()).unwrap();
    let ke1 = b64url().encode(started.message.serialize());
    tap.send(json!({"t": "rc:extauth.start", "connect_code": dev.code, "ke1": ke1}))
        .await;
    let challenge = tap
        .next(&["rc:extauth.challenge", "rc:extauth.result"])
        .await;
    assert_eq!(challenge["t"], "rc:extauth.challenge", "{challenge}");
    let attempt_id = challenge["attempt_id"].as_str().unwrap().to_string();
    let ke2 = b64url().decode(challenge["ke2"].as_str().unwrap()).unwrap();
    let done = started
        .state
        .finish(
            &mut OsRng,
            password.as_bytes(),
            CredentialResponse::<Suite>::deserialize(&ke2).unwrap(),
            ClientLoginFinishParameters::default(),
        )
        .expect("the right password opens the device's KE2");
    tap.send(json!({
        "t": "rc:extauth.finish",
        "connect_code": dev.code,
        "attempt_id": attempt_id,
        "ke3": b64url().encode(done.message.serialize()),
    }))
    .await;
    let verdict = tap.next(&["rc:extauth.result"]).await;
    assert!(verdict.get("refused").is_none(), "{verdict}");
    let key = app_key(&done.session_key, &attempt_id, &principal);

    // The session the login opens, up to the sealed answer.
    tap.send(json!({
        "t": "rc:extauth.session",
        "connect_code": dev.code,
        "attempt_id": attempt_id,
        "permissions": "VIEW | INPUT",
    }))
    .await;
    let created = tap
        .next(&["rc:session.created", "rc:extauth.result", "rc:error"])
        .await;
    assert_eq!(created["t"], "rc:session.created", "{created}");
    let session_id = created["session_id"].as_str().unwrap().to_string();
    let ready = tap.next(&["rc:ready", "rc:terminate", "rc:error"]).await;
    assert_eq!(ready["t"], "rc:ready", "{ready}");
    let (pc, offer) = browser_offer().await;
    tap.send(json!({
        "t": "rc:sdp.offer",
        "session_id": session_id,
        "sdp": offer,
        "extauth_mac": transport_mac(&key, "offer", &fingerprint(&offer)),
    }))
    .await;
    let answer = tap
        .next(&["rc:sdp.answer", "rc:terminate", "rc:error"])
        .await;
    assert_eq!(answer["t"], "rc:sdp.answer", "{answer}");
    let frames = std::mem::take(&mut tap.frames);
    let _ = pc.close().await;

    // The audit rows are written after the replies.
    let audit = url(&seeded, "/external-rc-audit");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let body: Value = app1
            .auth_get(&audit, &seeded.admin.access_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let rows = body["items"].as_array().unwrap();
        if rows.iter().any(|r| r["action"] == "session") {
            break;
        }
        assert!(Instant::now() < deadline, "no `session` audit row: {body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The canaries: one down each lane the assertions below read.
    app1.db
        .collection::<bson::Document>("fr52_capture_canary")
        .insert_one(bson::doc! { "note": CANARY })
        .await
        .unwrap();
    tokio::spawn(async move { tracing::info!(canary = CANARY, "the capture's own line") })
        .await
        .unwrap();
    let mut frames_with_canary = frames.clone();
    frames_with_canary.extend_from_slice(CANARY.as_bytes());

    let (names, stored) = dump_collections(&app1).await;
    let logs = capture.bytes();
    assert!(holds(&stored, CANARY), "the collection scan is blind");
    assert!(holds(&logs, CANARY), "the log capture is blind");
    assert!(
        holds(&frames_with_canary, CANARY),
        "the frame scan is blind"
    );

    // And they read the real thing, not an empty room.
    for collection in ["agents", "users", "external_rc_audit", "tenants"] {
        assert!(
            names.iter().any(|n| n == collection),
            "{collection} is not among {names:?}"
        );
    }
    let text = capture.text();
    for line in [
        "extauth: login VERIFIED by the device",
        "extauth: KE1 answered (one guess spent)",
        "extauth: session opened",
    ] {
        assert!(text.contains(line), "the capture missed {line:?}");
    }
    assert!(String::from_utf8_lossy(&frames).contains("rc:extauth.finish"));

    assert!(!holds(&logs, &password), "the password is in a log line");
    assert!(!holds(&frames, &password), "the password is in a frame");
    assert!(
        !holds(&stored, &password),
        "the password is stored in a collection"
    );
    let _ = dev.stop.send(true);
}

/// AC: a server asked to start an external session WITHOUT a client proof
/// cannot. This drives the mint path itself, the Hub call `extauth::handle_session`
/// makes, the way a compromised server would, past every check the server's own
/// code applies, at a real agent with every gate open and `auto` consent. The
/// device binds the session to a login IT verified or ends it.
///
/// The arm that passes is the proof the others are the device's doing: the same
/// call on a login the device verified is admitted, and the refusals before it
/// left that login unspent.
///
/// Out of this test: a server that mints an ORDINARY session names no login and
/// the device cannot tell it from a colleague's. That is the trust FR-100 removes.
#[tokio::test]
async fn a_server_cannot_mint_a_session_the_device_never_logged_in() {
    use bson::oid::ObjectId;
    use roomler_ai_remote_control::signaling::{ExternalGrant, ServerMsg};

    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-mint").await;
    let dev = device_with(
        &app,
        &seeded,
        "mach-extauth-mint",
        &pw("device"),
        unattended(),
    )
    .await;
    let device = ObjectId::parse_str(&dev.agent_id).unwrap();
    let (principal, mut ws) = outsider(&app, "extmint").await;
    let principal = ObjectId::parse_str(&principal).unwrap();
    let (stranger, _stranger_ws) = outsider(&app, "extmintb").await;
    let stranger = ObjectId::parse_str(&stranger).unwrap();

    let mint = |who: ObjectId, attempt_id: &str| {
        mint_session(
            &app,
            device,
            who,
            Some(ExternalGrant {
                attempt_id: attempt_id.to_string(),
            }),
        )
    };
    // The session's outcome as the controller's channel hears it.
    async fn outcome(mut rx: tokio::sync::mpsc::Receiver<ServerMsg>) -> Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(15), rx.recv())
                .await
                .expect("an outcome within 15 s")
                .expect("the channel stays open");
            let frame = serde_json::to_value(&msg).unwrap();
            if matches!(frame["t"].as_str(), Some("rc:ready" | "rc:terminate")) {
                return frame;
            }
        }
    }

    // 1. No login at all: an attempt id made up by the server.
    let (session, rx) = mint(principal, "66f0c0ffee0000000000abcd");
    let end = outcome(rx).await;
    assert_eq!(end["t"], "rc:terminate", "no proof, no session: {end}");
    assert_eq!(end["reason"], "agent_hangup", "the device's own act: {end}");
    assert_eq!(end["session_id"], json!(session.to_hex()), "{end}");

    // 2. A real login, verified by the device, handed to someone else.
    let (attempt_id, _key) = login(&mut ws, &dev.code, &pw("device"), &principal.to_hex()).await;
    let (_, rx) = mint(stranger, &attempt_id);
    let end = outcome(rx).await;
    assert_eq!(end["t"], "rc:terminate", "another principal's login: {end}");
    assert_eq!(end["reason"], "agent_hangup", "{end}");

    // 3. The control: the same call, on the principal's own verified login.
    let (session, rx) = mint(principal, &attempt_id);
    let ready = outcome(rx).await;
    assert_eq!(
        ready["t"], "rc:ready",
        "the mint path works, and arm 2 did not spend the login: {ready}"
    );
    assert_eq!(ready["session_id"], json!(session.to_hex()), "{ready}");
    let _ = app
        .state
        .fleet()
        .rc_hub
        .terminate(session, roomler_ai_remote_control::models::EndReason::Error);

    // 4. One login, one session: the replay of a spent login.
    let (_, rx) = mint(principal, &attempt_id);
    let end = outcome(rx).await;
    assert_eq!(end["t"], "rc:terminate", "a spent login: {end}");
    assert_eq!(end["reason"], "agent_hangup", "{end}");
    let _ = dev.stop.send(true);
}

/// One `rc:extauth.start` straight into the handler, as a socket's frame would
/// arrive, under a fresh principal so the per-principal limit is not what gets
/// measured: how long until the answer left, and the answer.
async fn knock_timed(app: &TestApp, code: &str, ke1: &str) -> (Duration, Value) {
    knock_as(app, bson::oid::ObjectId::new(), code, ke1).await
}

/// What `handle_session` calls, minus everything that decides whether to. With
/// no grant, the session is an ordinary one, as an org member's is.
fn mint_session(
    app: &TestApp,
    device: bson::oid::ObjectId,
    who: bson::oid::ObjectId,
    external: Option<roomler_ai_remote_control::signaling::ExternalGrant>,
) -> (
    bson::oid::ObjectId,
    tokio::sync::mpsc::Receiver<roomler_ai_remote_control::signaling::ServerMsg>,
) {
    use roomler_ai_mod_fleet::hub::OrgPolicy;
    use roomler_ai_remote_control::models::{ConsentMode, RcOverlayMode, SessionPolicy};
    use roomler_ai_remote_control::permissions::Permissions;
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let session = app
        .state
        .fleet()
        .rc_hub
        .create_session_full(
            device,
            who,
            "minted by the server".to_string(),
            tx,
            Permissions::VIEW | Permissions::INPUT,
            Vec::new(),
            None,
            None,
            None,
            false,
            ConsentMode::Prompt,
            None,
            Vec::new(),
            RcOverlayMode::Off,
            None,
            None,
            false,
            OrgPolicy::Read(SessionPolicy::default()),
            None,
            None,
            None,
            external,
            None,
            None,
            None,
        )
        .expect("the Hub takes the request: the DEVICE is what refuses it");
    (session, rx)
}

/// [`knock_timed`], as one person.
async fn knock_as(
    app: &TestApp,
    principal: bson::oid::ObjectId,
    code: &str,
    ke1: &str,
) -> (Duration, Value) {
    let remote = app.state.modules.remote.clone().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let (code, ke1) = (code.to_string(), ke1.to_string());
    let started = Instant::now();
    tokio::spawn(async move {
        let hop = roomler_ai_mod_remote::extauth::Hop::Origin {
            connection_id: "knock".to_string(),
            frame: json!({}),
        };
        roomler_ai_mod_remote::extauth::handle_start(
            &remote, principal, "knock", &tx, &code, ke1, hop,
        )
        .await;
    });
    let answer = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("an answer within 15 s")
        .expect("an answer");
    (started.elapsed(), serde_json::to_value(&answer).unwrap())
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// AC: with the org's switch off, a valid code and the correct password get the
/// same response and the same latency as a code that does not exist (§5: the
/// resolution is not an existence oracle). So do a withdrawn approval and an
/// offline device.
///
/// Before `ANSWER_FLOOR` a hit and a miss differed by 0.4 ms at every percentile
/// (§12), which sampling recovers. What makes this deterministic is the lower
/// bound on EVERY answer: an arm that skips the floor fails it at once. The
/// medians are the sanity check on top, wide enough for a loaded runner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_code_answers_like_one_that_does_not_exist() {
    use roomler_ai_mod_remote::extauth::ANSWER_FLOOR;
    let app = TestApp::spawn().await;

    let closed = app.seed_tenant("extauth-floor-a").await;
    let gate_1_off = device(&app, &closed, "mach-extauth-floor-a", &pw("device")).await;
    assert_eq!(
        put(
            &app,
            &url(&closed, "/external-access"),
            &closed.admin.access_token,
            json!({"enabled": false})
        )
        .await,
        200
    );

    let withdrawn = app.seed_tenant("extauth-floor-b").await;
    let gate_2_off = device(&app, &withdrawn, "mach-extauth-floor-b", &pw("device")).await;
    assert_eq!(
        put(
            &app,
            &url(
                &withdrawn,
                &format!("/agent/{}/external-access-policy", gate_2_off.agent_id)
            ),
            &withdrawn.admin.access_token,
            json!({"approved": false})
        )
        .await,
        200
    );

    let open = app.seed_tenant("extauth-floor-c").await;
    let offline = device(&app, &open, "mach-extauth-floor-c", &pw("device")).await;
    let _ = offline.stop.send(true);
    let offline_id = bson::oid::ObjectId::parse_str(&offline.agent_id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.state.fleet().rc_hub.is_agent_online(offline_id) {
        assert!(Instant::now() < deadline, "the device never went offline");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The correct password, for all of them: it must make no difference.
    let (_, ke1) = client_ke1(&pw("device"));
    let arms = [
        ("no such code", "0000-0000-0000"),
        ("gate 1 off", gate_1_off.code.as_str()),
        ("gate 2 withdrawn", gate_2_off.code.as_str()),
        ("device offline", offline.code.as_str()),
    ];
    const ROUNDS: usize = 30;
    let mut timings: Vec<Vec<Duration>> = vec![Vec::new(); arms.len()];
    let want = json!({"t": "rc:extauth.result", "refused": "unavailable"});
    for _ in 0..ROUNDS {
        // All four at once, so a stall lands on all of them.
        let (a, b, c, d) = tokio::join!(
            knock_timed(&app, arms[0].1, &ke1),
            knock_timed(&app, arms[1].1, &ke1),
            knock_timed(&app, arms[2].1, &ke1),
            knock_timed(&app, arms[3].1, &ke1),
        );
        for (i, (took, answer)) in [a, b, c, d].into_iter().enumerate() {
            assert_eq!(answer, want, "{}: the answer differs", arms[i].0);
            assert!(
                took >= ANSWER_FLOOR,
                "{}: answered in {took:?}, under the floor: this arm tells a code apart",
                arms[i].0
            );
            timings[i].push(took);
        }
    }
    let mut medians = Vec::new();
    for (i, t) in timings.iter_mut().enumerate() {
        t.sort();
        println!(
            "AC1 {:17} n={} p10={:?} p50={:?} p90={:?} p99={:?}",
            arms[i].0,
            t.len(),
            percentile(t, 0.10),
            percentile(t, 0.50),
            percentile(t, 0.90),
            percentile(t, 0.99),
        );
        medians.push(percentile(t, 0.50));
    }
    let spread = *medians.iter().max().unwrap() - *medians.iter().min().unwrap();
    assert!(
        spread < Duration::from_millis(25),
        "the arms' medians differ by {spread:?}: {medians:?}"
    );
    let _ = gate_1_off.stop.send(true);
    let _ = gate_2_off.stop.send(true);
}

/// An answer at the floor is its arm's LAST act. On a single-threaded runtime an
/// arm that answered and then audited read 1 ms late at every percentile (spec
/// §12), because the spawned write ran ahead of the reply. Two idle workers hide
/// that, a busy pod may not have one, and the floor test above cannot see it at
/// all: every answer still leaves after the floor.
///
/// A row is stamped as it is made, so the stamp says which came first: one later
/// than half the floor came from an arm that answered first. Each arm has three
/// tries, so a stall on a busy runner does not read as that; an arm that really
/// answers first fails all three.
#[tokio::test]
async fn an_audit_row_is_made_before_the_answer_it_explains() {
    use roomler_ai_mod_remote::extauth::ANSWER_FLOOR;
    use roomler_ai_services::dao::base::PaginationParams;

    let app = TestApp::spawn().await;
    let (_id, mut ws) = outsider(&app, "extorder").await;
    let seeded = app.seed_tenant("extauth-order").await;
    let dev = device(&app, &seeded, "mach-extauth-order", &pw("device")).await;
    assert_eq!(
        put(
            &app,
            &url(&seeded, "/external-access"),
            &seeded.admin.access_token,
            json!({"enabled": false})
        )
        .await,
        200
    );

    // Offline with the switch off: the one state in which all three arms write
    // a row. On an online device a made-up attempt is refused without one.
    let _ = dev.stop.send(true);
    let tenant = bson::oid::ObjectId::parse_str(&seeded.tenant_id).unwrap();
    let agent = bson::oid::ObjectId::parse_str(&dev.agent_id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.state.fleet().rc_hub.is_agent_online(agent) {
        assert!(Instant::now() < deadline, "the device never went offline");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (_, ke1) = client_ke1(&pw("device"));
    const MADE_UP: &str = "66f0c0ffee0000000000abcd";
    let code = dev.code.as_str();
    let arms = [
        (
            "start",
            json!({"t": "rc:extauth.start", "connect_code": code, "ke1": ke1}),
        ),
        (
            "finish",
            json!({"t": "rc:extauth.finish", "connect_code": code, "attempt_id": MADE_UP, "ke3": "AAAA"}),
        ),
        (
            "session",
            json!({"t": "rc:extauth.session", "connect_code": code, "attempt_id": MADE_UP, "permissions": "VIEW"}),
        ),
    ];
    let early = 0..(ANSWER_FLOOR.as_millis() as i64 / 2);
    for (arm, frame) in arms {
        let mut delays = Vec::new();
        for _ in 0..3 {
            let since = bson::DateTime::now().timestamp_millis();
            send(&mut ws, frame.clone()).await;
            next_of(&mut ws, &["rc:extauth.result", "rc:session.created"]).await;
            let rows = app
                .state
                .fleet()
                .external_rc_audit
                .list_for_agent(tenant, agent, &PaginationParams::default())
                .await
                .unwrap();
            let delay = rows.items[0].at.timestamp_millis() - since;
            delays.push(delay);
            if early.contains(&delay) {
                break;
            }
        }
        assert!(
            delays.iter().any(|d| early.contains(d)),
            "{arm}: the row came after the answer, {delays:?} ms after the knock"
        );
    }
}

/// `rc:extauth.finish` and `rc:extauth.session` need no start and no password,
/// and carry a connect code. So a stranger with a made-up attempt id must learn
/// from them exactly what no such code tells them, whatever the code's device is
/// doing: online, the org's switch off, or offline, which used to answer
/// `unavailable` where a bogus code answers `unknown_attempt`.
#[tokio::test]
async fn a_made_up_attempt_tells_nothing_about_the_code() {
    let app = TestApp::spawn().await;
    let (_id, mut ws) = outsider(&app, "extprobe").await;

    let on = app.seed_tenant("extauth-probe-a").await;
    let online = device(&app, &on, "mach-extauth-probe-a", &pw("device")).await;
    let closed = app.seed_tenant("extauth-probe-b").await;
    let gate_off = device(&app, &closed, "mach-extauth-probe-b", &pw("device")).await;
    assert_eq!(
        put(
            &app,
            &url(&closed, "/external-access"),
            &closed.admin.access_token,
            json!({"enabled": false})
        )
        .await,
        200
    );
    let gone = app.seed_tenant("extauth-probe-c").await;
    let offline = device(&app, &gone, "mach-extauth-probe-c", &pw("device")).await;
    let _ = offline.stop.send(true);
    let offline_id = bson::oid::ObjectId::parse_str(&offline.agent_id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.state.fleet().rc_hub.is_agent_online(offline_id) {
        assert!(Instant::now() < deadline, "the device never went offline");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    const MADE_UP: &str = "66f0c0ffee0000000000abcd";
    let want = json!({
        "t": "rc:extauth.result",
        "attempt_id": MADE_UP,
        "refused": "unknown_attempt",
    });
    for (arm, code) in [
        ("no such code", "0000-0000-0000"),
        ("online", online.code.as_str()),
        ("gate 1 off", gate_off.code.as_str()),
        ("offline", offline.code.as_str()),
    ] {
        send(
            &mut ws,
            json!({"t": "rc:extauth.finish", "connect_code": code, "attempt_id": MADE_UP, "ke3": "AAAA"}),
        )
        .await;
        let finish = next_of(&mut ws, &["rc:extauth.result"]).await;
        assert_eq!(finish, want, "finish, {arm}");
        send(
            &mut ws,
            json!({"t": "rc:extauth.session", "connect_code": code, "attempt_id": MADE_UP, "permissions": "VIEW"}),
        )
        .await;
        let session = next_of(&mut ws, &["rc:extauth.result", "rc:session.created"]).await;
        assert_eq!(session, want, "session, {arm}");
    }
    let _ = online.stop.send(true);
    let _ = gate_off.stop.send(true);
}

/// AC (F4): an external controller's `local_relay` is rejected, with a log line.
/// The frame's type has no such field, which kept the relay out of the session
/// but said nothing to anyone: serde dropped the key. Now the raw frame is read:
/// the ask is refused, the org's log and the server's log say why, and no device
/// is asked.
///
/// Two controls. The same login opens its session without the key, so the
/// refusal came from the key alone. And the same frame on a code that does not
/// exist gets the same answer, so this arm is no oracle either.
#[tokio::test]
async fn an_external_local_relay_is_refused_and_logged() {
    let capture = LogCapture::default();
    let _logs = capture.install();

    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-f4").await;
    let dev = device_with(
        &app,
        &seeded,
        "mach-extauth-f4",
        &pw("device"),
        unattended(),
    )
    .await;
    let (principal, mut ws) = outsider(&app, "extfour").await;
    let (attempt_id, _key) = login(&mut ws, &dev.code, &pw("device"), &principal).await;

    let relay = json!({
        "overlay_ip": "100.64.0.9",
        "turn_port": 47989,
        "username": "1792000000:u",
        "credential": "c",
    });
    let ask = |code: &str, key: &str| {
        let mut frame = json!({
            "t": "rc:extauth.session",
            "connect_code": code,
            "attempt_id": attempt_id,
            "permissions": "VIEW",
        });
        frame[key] = relay.clone();
        frame
    };
    let refused = json!({
        "t": "rc:extauth.result",
        "attempt_id": attempt_id,
        "refused": "unavailable",
    });

    for key in ["local_relay", "local_relay_from", "local_relay_also"] {
        send(&mut ws, ask(&dev.code, key)).await;
        let answer = next_of(
            &mut ws,
            &["rc:extauth.result", "rc:session.created", "rc:error"],
        )
        .await;
        assert_eq!(answer, refused, "{key}");
    }
    send(&mut ws, ask("0000-0000-0000", "local_relay")).await;
    let answer = next_of(&mut ws, &["rc:extauth.result", "rc:session.created"]).await;
    assert_eq!(answer, refused, "the same answer, whether the code exists");

    assert!(
        capture
            .text()
            .contains("a local_relay on an external session"),
        "the log line"
    );
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
        let row = body["items"].as_array().unwrap().iter().find(|i| {
            i["action"] == "session" && i["login_detail"].as_str().is_some_and(|d| d.contains("F4"))
        });
        if let Some(row) = row {
            assert_eq!(row["login_refused"], "unavailable", "{row}");
            assert_eq!(row["attempt_id"], json!(attempt_id), "{row}");
            assert!(row.get("session_id").is_none(), "no session: {row}");
            break;
        }
        assert!(Instant::now() < deadline, "no F4 audit row: {body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The control: the login survived, and the page's own frame opens the session.
    let _session = open_session(&mut ws, &dev.code, &attempt_id, "VIEW").await;
    let _ = dev.stop.send(true);
}

/// AC (F3): relay bytes from an external session land on the DEVICE's org in the
/// FR-20 ledger. An outsider's `/turn/credentials` pre-fetch cannot be charged:
/// they are no member of that org and never learn its ids. `rc:ready` can, because
/// the Hub knows the session's org, and it is what the viewer now builds its peer
/// from. This takes the username out of the real frame and plays coturn with it,
/// so the test fails if the frame stops being charged.
///
/// The outsider here has an org of their own, whose concurrent-session slot the
/// session takes (FR-115 O6). The relay is still the device's: its owner let
/// them in, and the outsider's org must see none of it.
///
/// `turn_bytes` staying 0 until coturn reports is the direct-path arm as far as
/// loopback can show it: no allocation, nothing booked. The field half is a real
/// relayed session and a real direct one.
#[tokio::test]
async fn an_external_sessions_relay_is_charged_to_the_devices_org() {
    use crate::turn_usage_tests::{channels, turn_bytes};
    use bson::oid::ObjectId;
    use roomler_ai_remote_control::turn_creds::org_of_username;

    let app = TestApp::spawn_with_settings(|s| {
        s.relay_allowance.turn_attribution = true;
        s.turn.url = Some("turn:coturn.example:3478".into());
        s.turn.shared_secret = Some(ObjectId::new().to_hex());
    })
    .await;
    let Some(redis) = app.state.redis_pubsub.clone() else {
        eprintln!("skipping: no Redis available");
        return;
    };
    let home = app.seed_tenant("extauth-meter-a").await;
    let home_org = ObjectId::parse_str(&home.tenant_id).unwrap();
    let dev = device_with(
        &app,
        &home,
        "mach-extauth-meter",
        &pw("device"),
        unattended(),
    )
    .await;

    // An outsider with an org of their own.
    let theirs = app.seed_tenant("extauth-meter-b").await;
    let their_org = ObjectId::parse_str(&theirs.tenant_id).unwrap();
    let ws_url = format!("ws://{}/ws?token={}", app.addr, theirs.admin.access_token);
    let (mut ws, _) = connect_async(&ws_url).await.expect("user socket");
    let _ = tokio::time::timeout(Duration::from_millis(500), ws.next()).await;

    // What the viewer's pre-fetch gets: uncharged, which is the gap.
    let body: Value = app
        .auth_get("/api/turn/credentials", &theirs.admin.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let prefetch = body["ice_servers"][1]["username"].as_str().unwrap();
    assert_eq!(org_of_username(prefetch), None, "{prefetch}");

    let (attempt_id, _key) = login(&mut ws, &dev.code, &pw("device"), &theirs.admin.id).await;
    let (_session, ready) = open_session_ready(&mut ws, &dev.code, &attempt_id, "VIEW").await;
    let username = ready["ice_servers"][1]["username"]
        .as_str()
        .unwrap_or_else(|| panic!("rc:ready carries a TURN server: {ready}"));
    assert_eq!(
        org_of_username(username),
        Some(home.tenant_id.as_str()),
        "the device's org pays, not the outsider's: {username}"
    );
    assert_eq!(
        turn_bytes(&app, home_org).await,
        0,
        "nothing relayed, nothing booked"
    );

    // coturn reports the allocation made on those credentials.
    let (client, peer) = channels(username, 5252);
    let deadline = Instant::now() + Duration::from_secs(20);
    while turn_bytes(&app, home_org).await < 61_000 && Instant::now() < deadline {
        redis
            .publish_to(&client, "rcvp=10, rcvb=1000, sentp=20, sentb=60000")
            .await
            .unwrap();
        redis
            .publish_to(&peer, "rcvp=20, rcvb=60000, sentp=10, sentb=1000")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert_eq!(
        turn_bytes(&app, home_org).await,
        61_000,
        "both legs, once, on the device's org"
    );
    assert_eq!(
        turn_bytes(&app, their_org).await,
        0,
        "none of it on the outsider's"
    );
    let _ = dev.stop.send(true);
}

/// AC: a `DesiredConfig` push carrying any `external_*` key is rejected. The
/// route refuses it out loud and audits it, rather than answering 200 for a key
/// it dropped, and nothing is recorded for the device to reconcile. The device's
/// own refusal, the one that survives this server, is
/// `a_pushed_external_key_never_reaches_the_device`.
#[tokio::test]
async fn a_desired_config_push_with_an_external_key_is_refused() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-push").await;
    let dev = device(&app, &seeded, "mach-extauth-push", &pw("device")).await;
    let path = url(&seeded, &format!("/agent/{}/desired-config", dev.agent_id));
    let owner = &seeded.admin.access_token;

    for key in [
        "external_access_enabled",
        "external_access_verifier",
        "external_consent_mode",
        "external_max_permissions",
        "external_added_next_year",
    ] {
        let resp = app
            .auth_put(&path, owner)
            .json(&json!({ "exec_enabled": true, key: true }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 403, "{key}");
    }
    let agent_id = bson::oid::ObjectId::parse_str(&dev.agent_id).unwrap();
    let row = app
        .db
        .collection::<bson::Document>("agents")
        .find_one(bson::doc! { "_id": agent_id })
        .await
        .unwrap()
        .unwrap();
    assert!(
        row.get_document("desired_config")
            .map_or(true, |d| d.get_i64("revision").unwrap_or(0) == 0
                && d.get_i32("revision").unwrap_or(0) == 0),
        "a refused push records nothing: {row:?}"
    );
    let refused = app
        .db
        .collection::<bson::Document>("config_audit")
        .count_documents(bson::doc! { "agent_id": agent_id, "denied": "device_owned_key" })
        .await
        .unwrap();
    assert_eq!(refused, 5, "every refusal is on the record");

    // The control: the same body without the key is the push it always was.
    let resp = app
        .auth_put(&path, owner)
        .json(&json!({ "exec_enabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let _ = dev.stop.send(true);
}

// ─── P5: who came in, who hears of it, and who is barred ─────────────────────

fn oid(hex: &str) -> bson::oid::ObjectId {
    bson::oid::ObjectId::parse_str(hex).unwrap()
}

/// How long a notice that should not exist gets to appear. They are spawned, so
/// "none" is only read after a positive sign that the path ran, plus this.
const SETTLE: Duration = Duration::from_millis(500);

/// The FR-52 notices this token's owner holds.
async fn notices(app: &TestApp, token: &str) -> Vec<Value> {
    let body: Value = app
        .auth_get("/api/notification", token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["notification_type"] == "external_access")
        .cloned()
        .collect()
}

async fn wait_for_notices(app: &TestApp, token: &str, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = notices(app, token).await;
        if got.len() >= count {
            return got;
        }
        assert!(
            Instant::now() < deadline,
            "{} of {count} notices: {got:#?}",
            got.len()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// This person's row on the org's people list, once a session has made it.
async fn person(app: &TestApp, seeded: &SeededTenant, dev: &Device, who: &str) -> Option<Value> {
    let body: Value = app
        .auth_get(
            &url(seeded, "/external-principals"),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["agent_id"] == json!(dev.agent_id) && p["user_id"] == json!(who))
        .cloned()
}

/// Wait for a session to be noted: the write is spawned behind the session, so a
/// row whose `last_session_at` is past `since` is the sign that it ran.
async fn noted(
    app: &TestApp,
    seeded: &SeededTenant,
    dev: &Device,
    who: &str,
    since: Option<&Value>,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(row) = person(app, seeded, dev, who).await
            && Some(&row["last_session_at"]) != since
        {
            return row;
        }
        assert!(Instant::now() < deadline, "the session was never noted");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The first row of the org's log that `found` accepts. Rows are spawned too.
async fn wait_for_row(
    app: &TestApp,
    seeded: &SeededTenant,
    found: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let body: Value = app
            .auth_get(
                &url(seeded, "/external-rc-audit"),
                &seeded.admin.access_token,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(row) = body["items"].as_array().unwrap().iter().find(|r| found(r)) {
            return row.clone();
        }
        assert!(Instant::now() < deadline, "no such row: {body:#}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// End a session from the server's side, and read the end on the outsider's
/// socket, so the next one is a session of its own.
async fn end_session(app: &TestApp, ws: &mut Ws, session: &str) {
    use roomler_ai_remote_control::models::EndReason;
    app.state
        .fleet()
        .rc_hub
        .terminate(oid(session), EndReason::Error)
        .unwrap();
    let end = next_of(ws, &["rc:terminate"]).await;
    assert_eq!(end["session_id"], json!(session), "{end}");
}

/// Both pods' subscribers up, so a ctrl event published now is heard.
async fn wait_for_bus(app1: &TestApp, app2: &TestApp) {
    use std::sync::atomic::Ordering::Relaxed;
    for _ in 0..40 {
        let (a, b) = (
            app1.state.cluster_bus.as_ref().unwrap(),
            app2.state.cluster_bus.as_ref().unwrap(),
        );
        if a.sub_alive.load(Relaxed) && b.sub_alive.load(Relaxed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The org hears of a person's first session on a device, and only that one. The
/// log says nothing about it: "first ever" outlives the log's 90 days, or it would
/// come round every quarter. Whoever manages the org's devices hears, and the
/// device's owner, and a member who is neither does not. Nobody is told about
/// themselves, so an admin coming in from outside is news to the others alone.
#[tokio::test]
async fn the_org_hears_of_a_persons_first_session_once() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-first").await;
    let (admin, member) = (&seeded.admin.access_token, &seeded.member.access_token);
    let dev = device_with(
        &app,
        &seeded,
        "mach-extauth-first",
        &pw("device"),
        unattended(),
    )
    .await;

    // Their first.
    let (first, mut ws) = outsider(&app, "extfirst").await;
    let (attempt_id, _) = login(&mut ws, &dev.code, &pw("device"), &first).await;
    let session = open_session(&mut ws, &dev.code, &attempt_id, "VIEW | INPUT").await;
    let told = wait_for_notices(&app, admin, 1).await;
    assert_eq!(told.len(), 1, "{told:#?}");
    assert_eq!(
        told[0]["link"],
        json!(format!("/tenant/{}/audit/external", seeded.tenant_id)),
        "{told:#?}"
    );
    let (title, body) = (
        told[0]["title"].as_str().unwrap(),
        told[0]["body"].as_str().unwrap(),
    );
    assert!(title.contains("mach-extauth-first"), "{title}");
    assert!(
        body.contains("calling itself \"An Outsider\""),
        "the name is quoted as the claim it is: {body}"
    );
    let row = noted(&app, &seeded, &dev, &first, None).await;
    assert_eq!(row["first_session_at"], row["last_session_at"], "{row}");
    assert_eq!(row["name"], "An Outsider", "{row}");
    tokio::time::sleep(SETTLE).await;
    assert!(
        notices(&app, member).await.is_empty(),
        "a member who neither manages devices nor owns this one is not told"
    );

    // Their second: the row moves and nobody hears.
    end_session(&app, &mut ws, &session).await;
    let (attempt_id, _) = login(&mut ws, &dev.code, &pw("device"), &first).await;
    let session = open_session(&mut ws, &dev.code, &attempt_id, "VIEW | INPUT").await;
    let second = noted(
        &app,
        &seeded,
        &dev,
        &first,
        Some(&row["last_session_at"].clone()),
    )
    .await;
    assert_eq!(second["first_session_at"], row["first_session_at"]);
    tokio::time::sleep(SETTLE).await;
    assert_eq!(notices(&app, admin).await.len(), 1, "the same person again");

    // Their third, with the log empty as its TTL would leave it.
    app.db
        .collection::<bson::Document>("external_rc_audit")
        .delete_many(bson::doc! {})
        .await
        .unwrap();
    end_session(&app, &mut ws, &session).await;
    let (attempt_id, _) = login(&mut ws, &dev.code, &pw("device"), &first).await;
    let session = open_session(&mut ws, &dev.code, &attempt_id, "VIEW | INPUT").await;
    noted(
        &app,
        &seeded,
        &dev,
        &first,
        Some(&second["last_session_at"].clone()),
    )
    .await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        notices(&app, admin).await.len(),
        1,
        "it must not rest on the log, which forgets"
    );
    end_session(&app, &mut ws, &session).await;

    // The device changes hands, and a second person is a new fact.
    app.db
        .collection::<bson::Document>("agents")
        .update_one(
            bson::doc! { "_id": oid(&dev.agent_id) },
            bson::doc! { "$set": { "owner_user_id": oid(&seeded.member.id) } },
        )
        .await
        .unwrap();
    let (next, mut next_ws) = outsider(&app, "extfirstb").await;
    let (attempt_id, _) = login(&mut next_ws, &dev.code, &pw("device"), &next).await;
    let session = open_session(&mut next_ws, &dev.code, &attempt_id, "VIEW | INPUT").await;
    assert_eq!(wait_for_notices(&app, admin, 2).await.len(), 2);
    assert_eq!(
        wait_for_notices(&app, member, 1).await.len(),
        1,
        "the device's owner hears, with no role that says so"
    );
    end_session(&app, &mut next_ws, &session).await;

    // Someone the notice would otherwise go to comes in from outside.
    let ws_url = format!("ws://{}/ws?token={admin}", app.addr);
    let (mut own_ws, _) = connect_async(&ws_url).await.expect("user socket");
    let (attempt_id, _) = login(&mut own_ws, &dev.code, &pw("device"), &seeded.admin.id).await;
    open_session(&mut own_ws, &dev.code, &attempt_id, "VIEW | INPUT").await;
    assert_eq!(wait_for_notices(&app, member, 2).await.len(), 2);
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        notices(&app, admin).await.len(),
        2,
        "nobody is told about themselves"
    );
    let _ = dev.stop.send(true);
}

/// A wrong guess leaves nothing on the server, because the client stops at KE2
/// (§4b). The device starting to refuse is the first anyone sees of it, and it is
/// said once, however many accounts keep at it.
#[tokio::test]
async fn the_org_hears_once_that_a_password_is_being_guessed() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-guessing").await;
    let (admin, member) = (&seeded.admin.access_token, &seeded.member.access_token);
    let dev = device(&app, &seeded, "mach-extauth-guessing", &pw("device")).await;
    let (_id, mut ws) = outsider(&app, "extguessing").await;

    async fn guess(ws: &mut Ws, code: &str) -> Value {
        let (_, ke1) = client_ke1(&pw("guess"));
        send(
            ws,
            json!({"t": "rc:extauth.start", "connect_code": code, "ke1": ke1}),
        )
        .await;
        next_of(ws, &["rc:extauth.challenge", "rc:extauth.result"]).await
    }

    for n in 0..5 {
        let answer = guess(&mut ws, &dev.code).await;
        assert_eq!(answer["t"], "rc:extauth.challenge", "guess {n}: {answer}");
    }
    tokio::time::sleep(SETTLE).await;
    assert!(
        notices(&app, admin).await.is_empty(),
        "the budget is the device's to spend, and nothing has been refused yet"
    );

    let refused = guess(&mut ws, &dev.code).await;
    assert_eq!(refused["refused"], "throttled", "{refused}");
    let told = wait_for_notices(&app, admin, 1).await;
    let (title, body) = (
        told[0]["title"].as_str().unwrap(),
        told[0]["body"].as_str().unwrap(),
    );
    assert!(title.contains("mach-extauth-guessing"), "{title}");
    assert!(
        body.contains("rotate the device's connect code"),
        "it says what ends it: {body}"
    );

    // More of the same, and from another account.
    let refused = guess(&mut ws, &dev.code).await;
    assert_eq!(refused["refused"], "throttled", "{refused}");
    let (_other, mut other_ws) = outsider(&app, "extguessingb").await;
    let refused = guess(&mut other_ws, &dev.code).await;
    assert_eq!(refused["refused"], "throttled", "{refused}");
    tokio::time::sleep(SETTLE).await;
    assert_eq!(notices(&app, admin).await.len(), 1, "one notice a window");
    assert!(notices(&app, member).await.is_empty());
    let _ = dev.stop.send(true);
}

/// A bar puts a person out and keeps them out. The session they are in ends, a
/// login they have verified and one still in flight both die, and a new knock
/// answers what a code that does not exist answers, at the floor. The org's log
/// says why, and letting them back opens the door again.
#[tokio::test]
async fn a_barred_person_is_put_out_and_kept_out_until_let_back() {
    use roomler_ai_mod_remote::extauth::ANSWER_FLOOR;
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-bar").await;
    let admin = &seeded.admin.access_token;
    let dev = device_with(
        &app,
        &seeded,
        "mach-extauth-bar",
        &pw("device"),
        unattended(),
    )
    .await;
    let (principal, mut ws) = outsider(&app, "extbar").await;
    let standing = url(
        &seeded,
        &format!("/agent/{}/external-principals/{principal}", dev.agent_id),
    );

    let (attempt_id, _) = login(&mut ws, &dev.code, &pw("device"), &principal).await;
    let session = open_session(&mut ws, &dev.code, &attempt_id, "VIEW | INPUT").await;
    noted(&app, &seeded, &dev, &principal, None).await;

    // What they hold as the bar lands: a login the device has verified, and one
    // with only its KE3 left to send.
    let (verified, _) = login(&mut ws, &dev.code, &pw("device"), &principal).await;
    let (state, ke1) = client_ke1(&pw("device"));
    send(
        &mut ws,
        json!({"t": "rc:extauth.start", "connect_code": dev.code, "ke1": ke1}),
    )
    .await;
    let challenge = next_of(&mut ws, &["rc:extauth.challenge", "rc:extauth.result"]).await;
    assert_eq!(challenge["t"], "rc:extauth.challenge", "{challenge}");
    let in_flight = challenge["attempt_id"].as_str().unwrap().to_string();
    let ke3 = client_ke3(state, &pw("device"), challenge["ke2"].as_str().unwrap()).unwrap();

    assert_eq!(
        put(&app, &standing, admin, json!({ "revoked": true })).await,
        200
    );

    let end = next_of(&mut ws, &["rc:terminate"]).await;
    assert_eq!(end["session_id"], json!(session), "{end}");
    assert_eq!(end["reason"], "admin_terminated", "{end}");

    send(
        &mut ws,
        json!({
            "t": "rc:extauth.finish",
            "connect_code": dev.code,
            "attempt_id": in_flight,
            "ke3": ke3,
        }),
    )
    .await;
    let refused = next_of(&mut ws, &["rc:extauth.result"]).await;
    assert_eq!(
        refused["refused"], "unavailable",
        "a login in flight: {refused}"
    );

    send(
        &mut ws,
        json!({
            "t": "rc:extauth.session",
            "connect_code": dev.code,
            "attempt_id": verified,
            "permissions": "VIEW | INPUT",
        }),
    )
    .await;
    let refused = next_of(&mut ws, &["rc:session.created", "rc:extauth.result"]).await;
    assert_eq!(refused["t"], "rc:extauth.result", "{refused}");
    assert_eq!(
        refused["refused"], "unavailable",
        "a verified login: {refused}"
    );

    let (_, ke1) = client_ke1(&pw("device"));
    let (took, barred) = knock_as(&app, oid(&principal), &dev.code, &ke1).await;
    let (_, no_such_code) = knock_timed(&app, "0000-0000-0000", &ke1).await;
    assert_eq!(
        barred.to_string(),
        no_such_code.to_string(),
        "to the outsider, the same closed door"
    );
    assert!(
        took >= ANSWER_FLOOR,
        "answered in {took:?}, under the floor"
    );

    // The org's log tells them apart, at all three steps.
    for (action, step) in [
        ("login", in_flight.as_str()),
        ("session", verified.as_str()),
    ] {
        let row = wait_for_row(&app, &seeded, |r| {
            r["action"] == action && r["attempt_id"] == json!(step)
        })
        .await;
        assert_eq!(row["login_refused"], "unavailable", "{row}");
        assert_eq!(
            row["login_detail"], "an admin of the org barred this person from the device",
            "{row}"
        );
    }
    wait_for_row(&app, &seeded, |r| {
        r["action"] == "login"
            && r["attempt_id"].is_null()
            && r["login_detail"] == "an admin of the org barred this person from the device"
    })
    .await;
    let row = person(&app, &seeded, &dev, &principal).await.unwrap();
    assert!(row["revoked_at"].is_string(), "{row}");

    // Let back, and the door opens.
    assert_eq!(
        put(&app, &standing, admin, json!({ "revoked": false })).await,
        200
    );
    let (attempt_id, _) = login(&mut ws, &dev.code, &pw("device"), &principal).await;
    open_session(&mut ws, &dev.code, &attempt_id, "VIEW | INPUT").await;
    let row = person(&app, &seeded, &dev, &principal).await.unwrap();
    assert!(row["revoked_at"].is_null(), "{row}");

    for approved in [false, true] {
        let row = wait_for_row(&app, &seeded, |r| {
            r["action"] == "principal" && r["approved"] == json!(approved)
        })
        .await;
        assert_eq!(row["principal"], json!(principal), "{row}");
        assert_eq!(row["user_id"], json!(seeded.admin.id), "{row}");
    }
    let _ = dev.stop.send(true);
}

/// The admin's request lands on one pod and the session lives on the other: only
/// the ctrl event can reach it.
#[tokio::test]
async fn a_bar_on_the_other_pod_ends_the_session() {
    let (app1, app2) = TestApp::spawn_pair(|_| {}).await;
    if app1.state.cluster_bus.is_none() {
        eprintln!("SKIPPED a_bar_on_the_other_pod_ends_the_session: no Redis");
        return;
    }
    wait_for_bus(&app1, &app2).await;

    let seeded = app1.seed_tenant("extauth-xpod-bar").await;
    let dev = device_with(
        &app1,
        &seeded,
        "mach-extauth-xpod-bar",
        &pw("device"),
        unattended(),
    )
    .await;
    let (principal, mut ws) = outsider(&app2, "extxpodbar").await;
    let (attempt_id, _) = login(&mut ws, &dev.code, &pw("device"), &principal).await;
    let session = open_session(&mut ws, &dev.code, &attempt_id, "VIEW | INPUT").await;
    noted(&app1, &seeded, &dev, &principal, None).await;
    let hub1 = &app1.state.fleet().rc_hub;
    assert!(
        hub1.session_snapshot(oid(&session)).is_some()
            && app2
                .state
                .fleet()
                .rc_hub
                .session_snapshot(oid(&session))
                .is_none(),
        "the session lives with the device, on pod 1"
    );

    let standing = url(
        &seeded,
        &format!("/agent/{}/external-principals/{principal}", dev.agent_id),
    );
    assert_eq!(
        put(
            &app2,
            &standing,
            &seeded.admin.access_token,
            json!({ "revoked": true })
        )
        .await,
        200
    );
    let end = next_of(&mut ws, &["rc:terminate"]).await;
    assert_eq!(end["session_id"], json!(session), "{end}");
    assert_eq!(end["reason"], "admin_terminated", "{end}");
    assert!(hub1.session_snapshot(oid(&session)).is_none());
    let _ = dev.stop.send(true);
}

/// The crossing. The gates read the row, an admin bars the person and finds no
/// session yet, and the Hub opens one. Played here by writing the bar through
/// the DAO, which ends nothing, under a live session: what `session_opened` then
/// reads back is all that stands between that session and the bar.
#[tokio::test]
async fn a_bar_that_crossed_an_opening_session_ends_it() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-crossing").await;
    let dev = device_with(
        &app,
        &seeded,
        "mach-extauth-crossing",
        &pw("device"),
        unattended(),
    )
    .await;
    let (principal, mut ws) = outsider(&app, "extcrossing").await;
    let (attempt_id, _) = login(&mut ws, &dev.code, &pw("device"), &principal).await;
    let session = open_session(&mut ws, &dev.code, &attempt_id, "VIEW | INPUT").await;
    noted(&app, &seeded, &dev, &principal, None).await;

    let fleet = app.state.fleet();
    let (tenant, device, who) = (oid(&seeded.tenant_id), oid(&dev.agent_id), oid(&principal));
    fleet
        .external_principals
        .set_revoked(tenant, device, who, oid(&seeded.admin.id), true)
        .await
        .unwrap();
    tokio::time::sleep(SETTLE).await;
    assert!(
        fleet.rc_hub.session_snapshot(oid(&session)).is_some(),
        "the DAO ends nothing: this is the state the race leaves"
    );

    let agent = fleet
        .agents
        .base
        .find_by_id_in_tenant(tenant, device)
        .await
        .unwrap();
    let remote = app.state.modules.remote.clone().unwrap();
    roomler_ai_mod_remote::extauth::session_opened(&remote, &agent, device, who, "crossing").await;

    let end = next_of(&mut ws, &["rc:terminate"]).await;
    assert_eq!(end["session_id"], json!(session), "{end}");
    let _ = dev.stop.send(true);
}

/// A second bar changes no row and writes nothing to the log, and it still ends
/// the session: the first call may have raced it. The bar here is the state such
/// a race leaves, with the row revoked and the session live.
#[tokio::test]
async fn a_repeated_bar_still_ends_the_session() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-repeat").await;
    let dev = device_with(
        &app,
        &seeded,
        "mach-extauth-repeat",
        &pw("device"),
        unattended(),
    )
    .await;
    let (principal, mut ws) = outsider(&app, "extrepeat").await;
    let (attempt_id, _) = login(&mut ws, &dev.code, &pw("device"), &principal).await;
    let session = open_session(&mut ws, &dev.code, &attempt_id, "VIEW | INPUT").await;
    noted(&app, &seeded, &dev, &principal, None).await;

    let barred = app
        .state
        .fleet()
        .external_principals
        .set_revoked(
            oid(&seeded.tenant_id),
            oid(&dev.agent_id),
            oid(&principal),
            oid(&seeded.admin.id),
            true,
        )
        .await
        .unwrap();
    assert_eq!(barred, Some(true));
    let standing = url(
        &seeded,
        &format!("/agent/{}/external-principals/{principal}", dev.agent_id),
    );
    assert_eq!(
        put(
            &app,
            &standing,
            &seeded.admin.access_token,
            json!({ "revoked": true })
        )
        .await,
        200
    );
    let end = next_of(&mut ws, &["rc:terminate"]).await;
    assert_eq!(end["session_id"], json!(session), "{end}");
    let _ = dev.stop.send(true);
}

/// A bar stops this person's external standing on this device. The session the
/// same account holds as a member came through the other door, with gates of its
/// own, and it stands.
#[tokio::test]
async fn a_bar_leaves_a_session_the_account_holds_as_a_member() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("extauth-member").await;
    let dev = device_with(
        &app,
        &seeded,
        "mach-extauth-member",
        &pw("device"),
        unattended(),
    )
    .await;
    let (principal, mut ws) = outsider(&app, "extmember").await;
    let (attempt_id, _) = login(&mut ws, &dev.code, &pw("device"), &principal).await;
    let external = open_session(&mut ws, &dev.code, &attempt_id, "VIEW | INPUT").await;
    noted(&app, &seeded, &dev, &principal, None).await;

    let (ordinary, _rx) = mint_session(&app, oid(&dev.agent_id), oid(&principal), None);
    assert_ne!(ordinary.to_hex(), external, "two sessions");

    // A ctrl envelope naming another org ends nothing.
    let hub = &app.state.fleet().rc_hub;
    let foreign = bson::oid::ObjectId::new();
    assert_eq!(
        hub.terminate_external_of(oid(&dev.agent_id), oid(&principal), foreign),
        0
    );
    assert!(hub.session_snapshot(oid(&external)).is_some());

    let standing = url(
        &seeded,
        &format!("/agent/{}/external-principals/{principal}", dev.agent_id),
    );
    assert_eq!(
        put(
            &app,
            &standing,
            &seeded.admin.access_token,
            json!({ "revoked": true })
        )
        .await,
        200
    );
    let end = next_of(&mut ws, &["rc:terminate"]).await;
    assert_eq!(end["session_id"], json!(external), "{end}");
    tokio::time::sleep(SETTLE).await;
    assert!(hub.session_snapshot(oid(&external)).is_none());
    assert!(
        hub.session_snapshot(ordinary).is_some(),
        "the session held as a member stands"
    );
    let _ = dev.stop.send(true);
}

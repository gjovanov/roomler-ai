//! End-to-end tests that drive the `roomler-agent` library crate against a
//! live `TestApp`. Unlike the REST-only `remote_control_tests`, these
//! exercise the agent's HTTP enrollment + WSS signaling loop in-process,
//! so a regression in either side (server rename, protocol drift, WS auth)
//! fails here too.

use crate::fixtures::test_app::TestApp;
use roomler_agent::{config::AgentConfig, encode::EncoderPreference, enrollment, signaling};
use serde_json::{Value, json};
use std::time::Duration;

/// Spawn the agent signaling loop with test-friendly defaults for the
/// LocalAPI / consent handles `signaling::run` gained in the Unification
/// P1 (`connected` flag + overlay-view watch channel) and P2b
/// (`ConsentBroker`) work. Centralises the 6-arg call so the tests below
/// don't each have to fabricate those handles. `AutoGrant` consent + a
/// per-agent temp sentinel dir keep it side-effect-light.
fn spawn_agent_signaling(
    cfg: AgentConfig,
    stop_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    spawn_agent_signaling_as(signaling::OrgCtx::primary(), cfg, stop_rx)
}

/// Multi-org P1 — like [`spawn_agent_signaling`] but with an explicit
/// [`signaling::OrgCtx`], so a test can drive a SECONDARY enrollment's loop
/// exactly the way `run_cmd`'s org supervisors do.
fn spawn_agent_signaling_as(
    ctx: signaling::OrgCtx,
    cfg: AgentConfig,
    stop_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Value type (OverlayView) is inferred from `run`'s Sender param, so
        // the tests crate needs no direct tunnel-core dep. Keep `_view_rx`
        // alive for the lifetime of `run` so its sends don't fail.
        let (view_tx, _view_rx) = tokio::sync::watch::channel(Default::default());
        let broker = roomler_agent::consent::ConsentBroker::new(
            roomler_agent::consent::Mode::AutoGrant,
            std::env::temp_dir().join(format!("roomler-test-consent-{}", cfg.agent_id)),
        )
        .expect("consent broker init");
        let _ = signaling::run(
            ctx,
            cfg,
            EncoderPreference::Software,
            stop_rx,
            connected,
            view_tx,
            // B1 — the RTT-prober bridge slot. Tests never install a hook, so
            // an empty slot is the correct value, not a stub.
            Default::default(),
            broker,
            roomler_agent::tunnel::client_mgr::TunnelClientHub::new("test".into()),
        )
        .await;
    })
}

/// Helper: issue an enrollment token via the admin REST route, then run the
/// agent's own `enrollment::enroll()` to get back an `AgentConfig` pointed
/// at the test server.
async fn enrol_via_agent_lib(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    machine_id: &str,
    machine_name: &str,
) -> AgentConfig {
    // Issue enrollment token (admin path).
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

    // Agent library exchanges it for a real agent config.
    enrollment::enroll(enrollment::EnrollInputs {
        server_url: &app.base_url,
        enrollment_token: et["enrollment_token"].as_str().unwrap(),
        machine_id,
        machine_name,
    })
    .await
    .expect("agent enrollment")
}

#[tokio::test]
async fn agent_library_enrolls_successfully() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("agentlib1").await;

    let cfg = enrol_via_agent_lib(&app, &seeded, "mach-agentlib-1", "Test laptop").await;
    assert!(!cfg.agent_token.is_empty());
    assert_eq!(cfg.tenant_id, seeded.tenant_id);
    assert_eq!(cfg.machine_id, "mach-agentlib-1");
    assert_eq!(cfg.machine_name, "Test laptop");

    // Sanity-check the REST layer sees us.
    let list: Value = app
        .auth_get(
            &format!("/api/tenant/{}/agent", seeded.tenant_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str().unwrap(), cfg.agent_id);
}

#[tokio::test]
async fn agent_library_connects_and_goes_online() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("agentlib2").await;

    let cfg = enrol_via_agent_lib(&app, &seeded, "mach-agentlib-2", "Online test").await;

    // Start the signaling loop. `run()` loops until shutdown; we just need it
    // to get through one successful connect + hello, then we stop it.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let sig_task = spawn_agent_signaling(cfg.clone(), stop_rx);

    // Poll the admin API until the agent's DB row flips to online.
    let agent_id = cfg.agent_id.clone();
    let mut online = false;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let row: Value = app
            .auth_get(
                &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, agent_id),
                &seeded.admin.access_token,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if row["is_online"].as_bool() == Some(true) {
            assert_eq!(row["status"].as_str(), Some("online"));
            online = true;
            break;
        }
    }
    assert!(online, "agent never transitioned to is_online=true");

    // Shut the agent down. Drop time is fast because the WS select arm
    // watches the shutdown signal.
    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), sig_task).await;
}

/// Poll one tenant's agent row for `is_online`.
async fn agent_is_online(
    app: &TestApp,
    seeded: &crate::fixtures::seed::SeededTenant,
    agent_id: &str,
) -> bool {
    let row: Value = app
        .auth_get(
            &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, agent_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    row["is_online"].as_bool() == Some(true)
}

#[tokio::test]
async fn same_machine_enrolls_into_two_tenants_and_both_connect() {
    // Multi-org P1 end-to-end: ONE machine fingerprint enrolled into TWO
    // tenants of one server — the second enrollment APPENDS a secondary
    // org (the pre-multi-org behavior rebound the whole config, dropping
    // tenant 1) — then BOTH enrollments run their signaling loops from one
    // process (exactly `run_cmd`'s primary + org-supervisor pattern) and
    // hold their per-tenant agent rows online CONCURRENTLY.
    let app = TestApp::spawn().await;
    let t1 = app.seed_tenant("morg1").await;
    let t2 = app.seed_tenant("morg2").await;

    // First enrollment = fresh primary.
    let fresh1 = enrol_via_agent_lib(&app, &t1, "mach-morg-1", "Multi-org box").await;
    let (cfg, outcome) = enrollment::apply_enrollment(None, fresh1, None, false).unwrap();
    assert_eq!(outcome, enrollment::EnrollOutcome::FreshPrimary);

    // Second enrollment: same machine_id, DIFFERENT tenant → append.
    let fresh2 = enrol_via_agent_lib(&app, &t2, "mach-morg-1", "Multi-org box").await;
    let (cfg, outcome) =
        enrollment::apply_enrollment(Some(cfg), fresh2, Some("acme"), false).unwrap();
    assert_eq!(
        outcome,
        enrollment::EnrollOutcome::AppendedOrg {
            label: "acme".into()
        }
    );
    assert_eq!(cfg.tenant_id, t1.tenant_id, "primary identity untouched");
    assert_eq!(cfg.orgs.len(), 1);
    assert_eq!(cfg.orgs[0].tenant_id, t2.tenant_id);
    assert_eq!(cfg.machine_id, cfg.for_org(&cfg.orgs[0]).machine_id);

    // Drive both enrollments concurrently.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let prim_task = spawn_agent_signaling(cfg.clone(), stop_rx.clone());
    let org_cfg = cfg.for_org(&cfg.orgs[0]);
    let org_task = spawn_agent_signaling_as(
        signaling::OrgCtx::secondary(&cfg.orgs[0].label),
        org_cfg.clone(),
        stop_rx,
    );
    assert_ne!(
        cfg.agent_id, org_cfg.agent_id,
        "distinct per-tenant agent ids"
    );

    let mut both_online = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if agent_is_online(&app, &t1, &cfg.agent_id).await
            && agent_is_online(&app, &t2, &org_cfg.agent_id).await
        {
            both_online = true;
            break;
        }
    }
    assert!(
        both_online,
        "both org enrollments must be online at the same time"
    );

    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), prim_task).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), org_task).await;
}

#[tokio::test]
async fn agent_library_rejects_bogus_enrollment_token() {
    let app = TestApp::spawn().await;
    let err = enrollment::enroll(enrollment::EnrollInputs {
        server_url: &app.base_url,
        enrollment_token: "not-a-jwt",
        machine_id: "mach-bogus",
        machine_name: "bogus",
    })
    .await
    .expect_err("bogus token must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("401") || msg.contains("rejected"),
        "expected 401/rejected, got: {msg}"
    );
}

#[tokio::test]
async fn agent_answers_sdp_offer_with_real_webrtc_peer() {
    // Exercises the full rc:* handshake end-to-end with a real webrtc-rs
    // peer on each side:
    //   - agent sends rc:agent.hello
    //   - "browser" (a second webrtc-rs PC) is opened in this test,
    //     creates an offer with a data channel
    //   - controller sends rc:session.request
    //   - server routes rc:request to the agent
    //   - agent auto-grants consent
    //   - controller receives rc:ready and sends the real offer
    //   - agent creates its PC, replies with rc:sdp.answer carrying a
    //     valid answer SDP
    //   - both sides trickle ICE through the signalling relay
    //
    // Asserts the answer is a well-formed SDP (agent's PC accepted the
    // offer and produced an answer the browser side would apply).
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message};
    use webrtc::api::APIBuilder;
    use webrtc::api::media_engine::MediaEngine;
    use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
    use webrtc::peer_connection::configuration::RTCConfiguration;
    use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("agentlib3").await;
    let cfg = enrol_via_agent_lib(&app, &seeded, "mach-agentlib-3", "Real peer").await;

    // Spin up the agent library.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let sig_task = spawn_agent_signaling(cfg.clone(), stop_rx);

    // Wait for the agent to go online.
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let row: Value = app
            .auth_get(
                &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, cfg.agent_id),
                &seeded.admin.access_token,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if row["is_online"].as_bool() == Some(true) {
            break;
        }
    }

    // Build a browser-side PC and a data channel (so the offer has media).
    let mut me = MediaEngine::default();
    me.register_default_codecs().unwrap();
    let api = APIBuilder::new().with_media_engine(me).build();
    let browser_pc = api
        .new_peer_connection(RTCConfiguration::default())
        .await
        .unwrap();
    let _dc = browser_pc
        .create_data_channel("control", Some(RTCDataChannelInit::default()))
        .await
        .unwrap();
    let browser_offer = browser_pc.create_offer(None).await.unwrap();
    browser_pc
        .set_local_description(browser_offer.clone())
        .await
        .unwrap();

    // Controller WS.
    let ctrl_url = format!(
        "ws://{}/ws?token={}",
        app.addr,
        urlencode(&seeded.admin.access_token)
    );
    let (mut ctrl_ws, _) = connect_async(&ctrl_url).await.expect("controller ws");
    let _ = tokio::time::timeout(Duration::from_secs(2), ctrl_ws.next()).await;

    // Kick off the session.
    ctrl_ws
        .send(Message::Text(
            json!({
                "t": "rc:session.request",
                "agent_id": cfg.agent_id,
                "permissions": "VIEW | INPUT",
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    let mut saw_created = false;
    let mut saw_ready = false;
    let mut saw_answer = false;
    let mut saw_agent_ice = false;
    let mut answer_sdp: Option<String> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut session_id: Option<String> = None;

    while tokio::time::Instant::now() < deadline && !saw_answer {
        let msg = match tokio::time::timeout(Duration::from_millis(500), ctrl_ws.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => continue,
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match v.get("t").and_then(|x| x.as_str()).unwrap_or("") {
            "rc:session.created" => {
                saw_created = true;
                session_id = extract_oid(&v["session_id"]);
            }
            "rc:ready" => {
                saw_ready = true;
                let sid = session_id.clone().expect("session_id from earlier");
                ctrl_ws
                    .send(Message::Text(
                        json!({
                            "t": "rc:sdp.offer",
                            "session_id": sid,
                            "sdp": browser_offer.sdp,
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
            }
            "rc:sdp.answer" => {
                saw_answer = true;
                answer_sdp = v["sdp"].as_str().map(|s| s.to_owned());
            }
            "rc:ice" => {
                // At least one ICE candidate trickled back from the agent's
                // gather phase means the PC is actually running.
                saw_agent_ice = true;
            }
            _ => {}
        }
    }

    assert!(saw_created, "rc:session.created missing");
    assert!(saw_ready, "rc:ready missing");
    assert!(
        saw_answer,
        "rc:sdp.answer missing — agent PC failed to build one"
    );

    // Apply the answer on the browser side — proves it's a valid SDP.
    let sdp = answer_sdp.expect("answer SDP");
    assert!(
        sdp.contains("v=0"),
        "answer SDP looks malformed: {sdp:.200}"
    );
    let answer = RTCSessionDescription::answer(sdp).expect("parse answer");
    browser_pc
        .set_remote_description(answer)
        .await
        .expect("browser accepts agent's answer");

    // ICE trickle is best-effort in this environment (localhost only, tight
    // ports); we log whether we saw any but don't fail on it.
    if !saw_agent_ice {
        eprintln!("note: no rc:ice from agent within window — acceptable for CI");
    }

    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), sig_task).await;
    let _ = browser_pc.close().await;
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

/// Extract a hex ObjectId. The wire format is raw hex on both REST and WS
/// paths — see `signaling::tests::object_ids_serialise_as_raw_hex_on_wire`.
/// If a regression ever reverts to bson-extended JSON we want this helper
/// to fail loudly, not paper over it.
fn extract_oid(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

// ────────────────────────────────────────────────────────────────────────────
// Multi-org — cross-org device enrollment from the UI (`rc:agent.join_org`)
// + the single-use enrollment-token ledger
// ────────────────────────────────────────────────────────────────────────────

/// Create a SECOND org owned by the same user, so one caller legitimately
/// holds MANAGE_AGENTS in both — the only shape the join endpoint accepts,
/// and the real-world one (a person who administers two orgs).
async fn second_org_for(app: &TestApp, admin_token: &str, slug: &str) -> String {
    let resp = app
        .auth_post("/api/tenant", admin_token)
        .json(&json!({ "name": format!("{slug} Corp"), "slug": slug }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "create second tenant");
    let t: Value = resp.json().await.unwrap();
    t["id"].as_str().unwrap().to_string()
}

/// Enrollment tokens were single-use BY DESIGN and never enforced — a token
/// stayed replayable for its whole TTL, which the 2026-08-05 field test hit
/// by accident (a token rejected by the device cap was accepted on retry).
#[tokio::test]
async fn an_enrollment_token_cannot_be_used_twice() {
    let app = TestApp::spawn().await;
    let t = app.seed_tenant("jtionce").await;

    let et: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = et["enrollment_token"].as_str().unwrap().to_string();

    let enroll = |machine: &'static str| {
        let app = &app;
        let token = token.clone();
        async move {
            app.client
                .post(app.url("/api/agent/enroll"))
                .json(&json!({
                    "enrollment_token": token,
                    "machine_id": machine,
                    "machine_name": "Replay box",
                    "os": "linux",
                    "agent_version": "0.3.0",
                }))
                .send()
                .await
                .unwrap()
        }
    };

    assert_eq!(enroll("mach-jti-1").await.status().as_u16(), 200);
    // Same token, DIFFERENT machine: the replay that used to mint a second
    // device (and a second agent JWT) off one authorization.
    let replay = enroll("mach-jti-2").await;
    assert_eq!(replay.status().as_u16(), 401, "replay must be refused");

    // …and a fresh token still works, so the ledger only spends what it must.
    let et2: Value = app
        .auth_post(
            &format!("/api/tenant/{}/agent/enroll-token", t.tenant_id),
            &t.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resp = app
        .client
        .post(app.url("/api/agent/enroll"))
        .json(&json!({
            "enrollment_token": et2["enrollment_token"].as_str().unwrap(),
            "machine_id": "mach-jti-2",
            "machine_name": "Replay box",
            "os": "linux",
            "agent_version": "0.3.0",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

/// The authorization contract: MANAGE_AGENTS in BOTH orgs. Target-only would
/// let anyone pull a stranger's device into their org; source-only would let
/// a device's admin push it into an org that never asked for it.
#[tokio::test]
async fn join_org_requires_manage_agents_in_both_organizations() {
    let app = TestApp::spawn().await;
    let src = app.seed_tenant("joinsrc").await;
    // A completely separate org the src admin has nothing to do with.
    let stranger = app.seed_tenant("joinstranger").await;

    let fresh = enrol_via_agent_lib(&app, &src, "mach-join-authz", "Join box").await;
    let agent_id = fresh.agent_id.clone();

    // Source admin, target they don't administer ⇒ refused.
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/{agent_id}/join-org", src.tenant_id),
            &src.admin.access_token,
        )
        .json(&json!({ "target_tenant_id": stranger.tenant_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "no MANAGE_AGENTS in the target org"
    );

    // A plain member of the SOURCE org can't push their org's devices
    // anywhere either.
    let target = second_org_for(&app, &src.admin.access_token, "joindst-a").await;
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/{agent_id}/join-org", src.tenant_id),
            &src.member.access_token,
        )
        .json(&json!({ "target_tenant_id": target }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403, "member lacks MANAGE_AGENTS");

    // Same org as target is a no-op, not a half-applied join.
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/{agent_id}/join-org", src.tenant_id),
            &src.admin.access_token,
        )
        .json(&json!({ "target_tenant_id": src.tenant_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

/// Two gates that exist so a click can't fail silently: an agent that
/// predates the feature (its decoder drops the unknown variant), and an
/// offline one (nothing to push down). Neither may mint a token.
#[tokio::test]
async fn join_org_refuses_incapable_or_offline_devices() {
    let app = TestApp::spawn().await;
    let src = app.seed_tenant("joincap").await;
    let target = second_org_for(&app, &src.admin.access_token, "joincap-dst").await;

    // Enrolled but never connected ⇒ no caps at all, and offline.
    let fresh = enrol_via_agent_lib(&app, &src, "mach-join-caps", "Old box").await;
    let resp = app
        .auth_post(
            &format!(
                "/api/tenant/{}/agent/{}/join-org",
                src.tenant_id, fresh.agent_id
            ),
            &src.admin.access_token,
        )
        .json(&json!({ "target_tenant_id": target }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body: Value = resp.json().await.unwrap();
    let msg = body["message"].as_str().unwrap_or_default().to_string();
    assert!(
        msg.contains("predates remote org-join"),
        "capability gate should explain itself: {msg}"
    );

    // The picker endpoint tells the UI the same thing up front, so the
    // action can be greyed out instead of failing on click.
    let targets: Value = app
        .auth_get(
            &format!(
                "/api/tenant/{}/agent/{}/join-targets",
                src.tenant_id, fresh.agent_id
            ),
            &src.admin.access_token,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(targets["supported"], false);
    assert_eq!(targets["online"], false);
    // It still lists the org the caller manages — the dialog explains why
    // it's unavailable rather than showing an empty list.
    let items = targets["items"].as_array().unwrap();
    assert!(
        items.iter().any(|i| i["tenant_id"] == target),
        "the manageable org should be listed: {items:?}"
    );
}

/// The whole point, end to end: a LIVE agent in org A is pushed into org B
/// from the API, enrolls itself, appends an `[[orgs]]` entry, and the new
/// org's supervised loop brings the device online there — no restart, no
/// shell on the device (the exact thing that blocked PC50045 in the field).
#[tokio::test]
async fn join_org_pushes_a_live_device_into_a_second_organization() {
    let app = TestApp::spawn().await;
    let src = app.seed_tenant("joinlive").await;
    let target = second_org_for(&app, &src.admin.access_token, "joinlive-dst").await;

    // A real enrollment + a real signaling loop, so the agent is ONLINE and
    // its hello advertises `multi_org: ["join"]`.
    let cfg = enrol_via_agent_lib(&app, &src, "mach-join-live", "Live box").await;
    let agent_id = cfg.agent_id.clone();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let primary = spawn_agent_signaling(cfg.clone(), stop_rx.clone());

    // Install the join runtime the way `run_cmd` does: a temp config file,
    // a fresh write lock, and a spawner that supervises the appended org
    // exactly like a boot-time one.
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    roomler_agent::config::save(&cfg_path, &cfg).unwrap();
    let spawn_rx = stop_rx.clone();
    let spawn_path = cfg_path.clone();
    roomler_agent::org_join::install(roomler_agent::org_join::JoinRuntime {
        config_path: cfg_path.clone(),
        write_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        spawn_org: Box::new(move |org| {
            let base = roomler_agent::config::load(&spawn_path).expect("reload");
            spawn_agent_signaling_as(
                signaling::OrgCtx::secondary(&org.label),
                base.for_org(&org),
                spawn_rx.clone(),
            );
        }),
    });

    // Wait for the device to be online in the source org — the push needs a
    // live socket by design.
    let mut online = false;
    for _ in 0..100 {
        if agent_is_online(&app, &src, &agent_id).await {
            online = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(online, "agent never came online in the source org");

    // The click.
    let resp = app
        .auth_post(
            &format!("/api/tenant/{}/agent/{agent_id}/join-org", src.tenant_id),
            &src.admin.access_token,
        )
        .json(&json!({ "target_tenant_id": target, "label": "second" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "join push accepted");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["delivered"], true, "pushed down the live socket");
    assert_eq!(body["label"], "second");

    // The agent enrolls itself into the target org and its new supervisor
    // connects — so the device appears, and comes ONLINE, over there.
    let mut appeared = false;
    for _ in 0..150 {
        let list: Value = app
            .auth_get(
                &format!("/api/tenant/{target}/agent"),
                &src.admin.access_token,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(items) = list["items"].as_array()
            && items.iter().any(|a| a["is_online"].as_bool() == Some(true))
        {
            appeared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        appeared,
        "the device should enroll into the target org and come online there"
    );

    // The config on disk grew a labelled secondary with its OWN key.
    let saved = roomler_agent::config::load(&cfg_path).unwrap();
    assert_eq!(saved.tenant_id, src.tenant_id, "primary identity untouched");
    assert_eq!(saved.orgs.len(), 1);
    assert_eq!(saved.orgs[0].label, "second");
    assert_eq!(saved.orgs[0].tenant_id, target);
    assert!(saved.orgs[0].overlay_wg_secret_key.is_some());
    assert_ne!(
        saved.orgs[0].overlay_wg_secret_key, saved.overlay_wg_secret_key,
        "a secondary must never borrow the primary's WG key"
    );

    let _ = stop_tx.send(true);
    let _ = primary.await;
}

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-55 P4 — the REAL agent says goodbye before a sleep, stays away while
//! the computer sleeps, and says hello at the wake, through a real server.
//!
//! `src/sleep_announce_tests.rs` locks the server's half with a scripted
//! agent socket. This drives the agent's half: `roomlerd`'s own signalling
//! loop (`signaling::run`), told the computer is going to sleep the way an OS
//! hook tells it (`sleep_notice::simulate`). The device must read asleep with
//! the posture its keeper published, must not dial back in while the sleep
//! lasts (that hello would erase the reason), must come back by itself at the
//! wake, and must then read plain "offline" when it goes without a word,
//! because the wake's hello cleared the old reason.
//!
//! This file is its own test BINARY because the announcement is
//! process-wide, as it is in the daemon (one computer, one sleep): in the
//! lib's test binary, every other in-process agent would be told to say
//! goodbye too. `hive_canary.rs` is separate for the same kind of reason.

use std::sync::Arc;
use std::time::{Duration, Instant};

use roomler_ai_tests::fixtures::seed::SeededTenant;
use roomler_ai_tests::fixtures::test_app::TestApp;
use roomlerd::{config::AgentConfig, encode::EncoderPreference, enrollment, signaling};
use serde_json::Value;

/// The agent's signalling loop with test defaults, as
/// `src/rc_idle_limit_tests.rs` spawns it (duplicated so this binary stands
/// alone).
fn spawn_agent_signaling(
    cfg: AgentConfig,
    stop_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (view_tx, _view_rx) = tokio::sync::watch::channel(Default::default());
        let broker = roomlerd::consent::ConsentBroker::new(
            roomlerd::consent::Mode::AutoGrant,
            std::env::temp_dir().join(format!("roomler-test-consent-{}", cfg.agent_id)),
        )
        .expect("consent broker init");
        let exec_enabled = cfg.exec_enabled;
        let remote_config_enabled = cfg.remote_config_enabled;
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
                std::path::PathBuf::from("unused-in-tests.toml"),
                Arc::new(tokio::sync::Mutex::new(())),
                exec_enabled,
                remote_config_enabled,
            ),
            roomlerd::rc_sessions::RcSessionRegistry::new(),
        )
        .await;
    })
}

/// Enroll a device through the agent library.
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
    .expect("agent enrollment")
}

/// A GET as the tenant's admin. `None` for anything but a JSON success: the
/// per-IP limiter answers a fast poll with a 429 that is not JSON, and that
/// is a reason to look again, never a panic.
async fn get_json(app: &TestApp, seeded: &SeededTenant, path: &str) -> Option<Value> {
    let resp = app
        .auth_get(path, &seeded.admin.access_token)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

/// The admin listing's row for the agent (`GET /tenant/{tid}/agent/{id}`).
async fn agent_row(app: &TestApp, seeded: &SeededTenant, agent_id: &str) -> Option<Value> {
    get_json(
        app,
        seeded,
        &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, agent_id),
    )
    .await
}

/// The agent's row on the unified devices grid (`GET /tenant/{tid}/device`).
async fn device_row(app: &TestApp, seeded: &SeededTenant, agent_id: &str) -> Option<Value> {
    let page = get_json(
        app,
        seeded,
        &format!("/api/tenant/{}/device?per_page=100", seeded.tenant_id),
    )
    .await?;
    page["items"]
        .as_array()?
        .iter()
        .find(|r| r["id"] == agent_id)
        .cloned()
}

/// The agent's row once `ready` holds, polled gently, within `within`.
async fn wait_row(
    app: &TestApp,
    seeded: &SeededTenant,
    agent_id: &str,
    within: Duration,
    what: &str,
    ready: impl Fn(&Value) -> bool,
) -> Value {
    let started = Instant::now();
    let mut last = Value::Null;
    while started.elapsed() < within {
        if let Some(row) = agent_row(app, seeded, agent_id).await {
            if ready(&row) {
                return row;
            }
            last = row;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    panic!("{what}: not within {within:?}; the last row read {last}");
}

#[tokio::test]
async fn the_agent_says_goodbye_before_a_sleep_and_hello_after_the_wake() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("fr55p4agent").await;
    let cfg = enrol(&app, &seeded, "mach-fr55-sleep").await;
    let id = cfg.agent_id.clone();
    // What the daemon publishes before anything connects (`app.rs`): here a
    // device whose policy holds it awake only on mains. Nothing is held.
    roomlerd::power::publish_policy(roomlerd::power::PowerPolicy::OnAc);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let sig = spawn_agent_signaling(cfg.clone(), stop_rx);
    let online = wait_row(&app, &seeded, &id, Duration::from_secs(10), "online", |r| {
        r["is_online"] == true
    })
    .await;
    assert!(
        online.get("sleep").is_none(),
        "an awake device gives no reason: {online}"
    );

    // The OS says the computer is going to sleep. A hook runs on a thread of
    // its own, never on a runtime thread, and returns within its bound.
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || roomlerd::sleep_notice::simulate::will_sleep(&rt))
        .await
        .expect("the hook's thread");

    // The goodbye: offline at once, with the reason and the posture. Nothing
    // was held, so the policy let it sleep.
    let asleep = wait_row(&app, &seeded, &id, Duration::from_secs(5), "asleep", |r| {
        r["is_online"] == false && r["sleep"].is_object()
    })
    .await;
    assert_eq!(asleep["sleep"]["power"]["policy"], "on-ac", "{asleep}");
    assert_eq!(asleep["sleep"]["power"]["held"], false, "{asleep}");
    assert!(
        asleep["sleep"]["since"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "{asleep}"
    );
    let grid = device_row(&app, &seeded, &id)
        .await
        .expect("the devices grid lists the device");
    assert_eq!(grid["presence"], "offline", "{grid}");
    assert_eq!(grid["sleep"]["power"]["held"], false, "{grid}");

    // While the sleep lasts the agent does not dial back in. Without the hold
    // it is back within a second, and its hello erases the reason.
    let held_since = Instant::now();
    while held_since.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(r) = agent_row(&app, &seeded, &id).await {
            assert_eq!(
                r["is_online"], false,
                "dialled back in during the sleep: {r}"
            );
            assert!(
                r["sleep"].is_object(),
                "the reason went during the sleep: {r}"
            );
        }
    }

    // The wake: it reconnects by itself.
    roomlerd::sleep_notice::simulate::woke();
    let awake = wait_row(
        &app,
        &seeded,
        &id,
        Duration::from_secs(10),
        "back online after the wake",
        |r| r["is_online"] == true,
    )
    .await;
    assert!(awake.get("sleep").is_none(), "{awake}");

    // And its hello cleared the old reason: a device that now goes without a
    // word reads plain "offline", never the sleep from before.
    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), sig).await;
    let gone = wait_row(
        &app,
        &seeded,
        &id,
        Duration::from_secs(10),
        "offline after a stop",
        |r| r["is_online"] == false,
    )
    .await;
    assert!(
        gone.get("sleep").is_none(),
        "a stop is not a sleep, and the wake's hello cleared the old one: {gone}"
    );
}

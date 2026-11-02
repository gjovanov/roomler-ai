// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3280 — a host that is merely slow gains its hardware with no restart.
//!
//! `roomlerd`'s own signalling loop (`signaling::run`) against a real server,
//! with a real probe child that is slow and not stuck: its `base` phase takes
//! longer than a first probe gives a child, and less than the first try gives
//! one. The first hello has to say no hardware, and the row has to gain it on
//! the same connection.
//!
//! The probe spawns `current_exe()`, so THIS binary is the child: `main` plays
//! one when it finds `ROOMLERD_CAPS_CHILD`, which is why the target is
//! `harness = false`, with nothing in production code to swap the child out. The
//! bounds are the real ones, so this takes about 5 minutes. It answers just
//! enough of libtest's command line for the integration lane to list it, run it
//! and count it.
//!
//! Its own binary because the answer is process-wide, as it is in the daemon.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use roomler_ai_remote_control::models::{
    AgentCaps, ChromaFormat, VideoBackend, VideoCell, VideoCodec,
};
use roomler_ai_tests::fixtures::seed::SeededTenant;
use roomler_ai_tests::fixtures::test_app::TestApp;
use roomlerd::{config::AgentConfig, encode::EncoderPreference, enrollment, signaling};
use serde_json::Value;

const NAME: &str = "a_slow_host_gains_its_hardware_with_no_restart";

/// Over the first probe's 60 s for a child, under the first try's 2 min.
const BASE_TAKES: Duration = Duration::from_secs(70);
/// Two children at 60 s each, then the software child, which answers at once.
const FIRST_HELLO_WITHIN: Duration = Duration::from_secs(200);
/// The first try's minute of waiting, [`BASE_TAKES`] and a heartbeat's 30 s, with
/// room for a slow runner. On a build with no retry this runs out.
const GAINED_WITHIN: Duration = Duration::from_secs(300);

fn caps_of(hardware: bool) -> AgentCaps {
    let mut cells = vec![VideoCell::new(
        VideoCodec::H264,
        VideoBackend::Openh264,
        &[ChromaFormat::Yuv420],
        false,
    )];
    let mut encoders = vec!["openh264-sw".to_string()];
    if hardware {
        cells.push(VideoCell::new(
            VideoCodec::H264,
            VideoBackend::Nvenc,
            &[ChromaFormat::Yuv420],
            true,
        ));
        encoders.push("ffmpeg-h264_nvenc".to_string());
    }
    AgentCaps {
        codecs: vec!["h264".to_string()],
        hw_encoders: encoders,
        video_cells: cells,
        ..Default::default()
    }
}

/// `roomlerd caps-probe`, as far as the parent can tell.
fn be_the_probe_child() {
    let report = |caps: AgentCaps| serde_json::json!({ "caps": caps });
    match std::env::var("ROOMLERD_CAPS_PHASE").as_deref() {
        Ok("base") => {
            std::thread::sleep(BASE_TAKES);
            println!("ROOMLER_CAPS_JSON:{}", report(caps_of(true)));
        }
        Ok("software") => println!("ROOMLER_CAPS_JSON:{}", report(caps_of(false))),
        Ok("444") => println!("ROOMLER_CAPS_444:[]"),
        Ok("gpu") => println!("ROOMLER_CAPS_GPU:[]"),
        other => panic!("a phase this test does not know: {other:?}"),
    }
}

/// `--list` and the names to run. A flag's value is no filter.
fn read_args() -> (bool, bool) {
    let mut listing = false;
    let mut filters = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => listing = true,
            "--format" | "-Z" | "--test-threads" => {
                args.next();
            }
            flag if flag.starts_with('-') => {}
            _ => filters.push(arg),
        }
    }
    let selected = filters.is_empty() || filters.iter().any(|f| NAME.contains(f.as_str()));
    (listing, selected)
}

fn main() -> ExitCode {
    if std::env::var_os("ROOMLERD_CAPS_CHILD").is_some() {
        be_the_probe_child();
        return ExitCode::SUCCESS;
    }
    let (listing, selected) = read_args();
    if listing {
        if selected {
            println!("{NAME}: test");
        }
        return ExitCode::SUCCESS;
    }
    if !selected {
        println!(
            "\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 \
             filtered out; finished in 0.00s\n"
        );
        return ExitCode::SUCCESS;
    }
    // SAFETY: no other thread exists yet. The per-user cache is not this test's
    // to read or write, and no environment variable moves it.
    unsafe {
        std::env::set_var("ROOMLERD_CAPS_CACHE", "0");
        std::env::set_var("ROOMLERD_AUTO_UPDATE", "0");
    }
    println!("\nrunning 1 test");
    let started = Instant::now();
    // The stack a server-in-a-test needs (`RUST_MIN_STACK` in the lane), whether
    // or not the caller set it.
    let ran = std::thread::Builder::new()
        .stack_size(8 << 20)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(8 << 20)
                .build()
                .expect("a runtime")
                .block_on(the_test());
        })
        .expect("a thread")
        .join();
    let took = started.elapsed().as_secs_f32();
    if ran.is_ok() {
        println!(
            "test {NAME} ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 \
             filtered out; finished in {took:.2}s\n"
        );
        ExitCode::SUCCESS
    } else {
        println!(
            "test {NAME} ... FAILED\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 \
             measured; 0 filtered out; finished in {took:.2}s\n"
        );
        ExitCode::from(101)
    }
}

/// The agent's signalling loop with test defaults, as
/// `tests/sleep_announce_agent.rs` spawns it (duplicated so this binary stands
/// alone), with its connected flag handed out.
fn spawn_agent_signaling(
    cfg: AgentConfig,
    stop_rx: tokio::sync::watch::Receiver<bool>,
    connected: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
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

/// The admin listing's row for the agent. `None` for anything but a JSON
/// success: the per-IP limiter answers a fast poll with a 429 that is not JSON.
async fn agent_row(app: &TestApp, seeded: &SeededTenant, agent_id: &str) -> Option<Value> {
    let resp = app
        .auth_get(
            &format!("/api/tenant/{}/agent/{}", seeded.tenant_id, agent_id),
            &seeded.admin.access_token,
        )
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

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
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    panic!(
        "{what}: not within {within:?}; the last row's capabilities read {}",
        last["capabilities"]
    );
}

fn has_hardware(row: &Value) -> bool {
    row["capabilities"]["video_cells"]
        .as_array()
        .is_some_and(|cells| cells.iter().any(|c| c["hw"] == true))
}

fn said_hello(row: &Value) -> bool {
    row["is_online"] == true
        && row["capabilities"]["hw_encoders"]
            .as_array()
            .is_some_and(|e| !e.is_empty())
}

async fn the_test() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("caps3280").await;
    let cfg = enrol(&app, &seeded, "mach-caps-slow").await;
    let id = cfg.agent_id.clone();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let connected = Arc::new(AtomicBool::new(false));
    let sig = spawn_agent_signaling(cfg, stop_rx, connected.clone());

    // The first answer: both `base` children ran out of time, so the device
    // comes online with no hardware.
    let first = wait_row(
        &app,
        &seeded,
        &id,
        FIRST_HELLO_WITHIN,
        "the first hello",
        said_hello,
    )
    .await;
    assert!(
        !has_hardware(&first),
        "the first probe gives a child 60 s, and this one takes 70: {}",
        first["capabilities"]
    );

    // A reconnect's hello would also carry the new answer, and prove nothing
    // about the heartbeat, so the connection has to stay up throughout.
    let dropped = Arc::new(AtomicBool::new(false));
    let watch = {
        let (connected, dropped) = (connected.clone(), dropped.clone());
        tokio::spawn(async move {
            loop {
                if !connected.load(Ordering::SeqCst) {
                    dropped.store(true, Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };

    let gained = wait_row(
        &app,
        &seeded,
        &id,
        GAINED_WITHIN,
        "hardware, with no restart",
        has_hardware,
    )
    .await;
    watch.abort();
    assert!(
        !dropped.load(Ordering::SeqCst),
        "the connection dropped, so this may be a hello and not the heartbeat"
    );

    // A host short of time says why, and stops saying it once it has an answer.
    assert_eq!(
        first["capabilities"]["unavailable"]["encoder-cells"]["code"], "probe-short",
        "{}",
        first["capabilities"]
    );
    assert!(
        gained["capabilities"]["unavailable"]
            .get("encoder-cells")
            .is_none(),
        "{}",
        gained["capabilities"]
    );

    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), sig).await;
}

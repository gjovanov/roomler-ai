// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-3b-1 — a device's scan end to end: a person asks, and the REAL
//! `roomlerd` signaling loop scans the roots its config file lists, each read
//! in this process (`hive-test-launcher`: the harness's binary knows no
//! `knowhow-scan`), and what it found lands on the map as the device's.
//!
//! What these lock, that the unit tests cannot: the frame reaches the
//! scanner on the primary's context; the roots come from the config FILE at
//! each scan; a checkout is keyed as the intake keys it, its remote without
//! its password; only a scan that read everything it lists retires anything;
//! and an emptied list answers `no_roots`, which archives the device's part.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use bson::oid::ObjectId;
use roomlerd::{config::AgentConfig, encode::EncoderPreference, enrollment, signaling};
use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::knowhow_intake_tests::card;
use crate::knowhow_scan_tests::{ask, scan_app, wait_scan};
use crate::knowhow_sync_tests::fact;
use roomler_ai_mod_knowhow::intake::checkout_key;

/// Spawn the agent signalling loop as `run_cmd` wires it, its knowhow
/// scanner reading `config` in this process.
fn spawn_agent(
    cfg: AgentConfig,
    config: PathBuf,
    stop_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (view_tx, _view_rx) = tokio::sync::watch::channel(Default::default());
        let broker = roomlerd::consent::ConsentBroker::new(
            roomlerd::consent::Mode::AutoGrant,
            std::env::temp_dir().join(format!("roomler-knowhow-consent-{}", cfg.agent_id)),
        )
        .expect("consent broker init");
        let lock: roomlerd::config::WriteLock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let scanner = roomlerd::hive::knowhow::Scanner::in_process(config, lock);
        let ctx = signaling::OrgCtx::primary(roomlerd::token_slot::TokenSlot::new(
            cfg.agent_token.clone(),
        ))
        .with_knowhow(scanner);
        let (exec_enabled, remote_config_enabled) = (cfg.exec_enabled, cfg.remote_config_enabled);
        let _ = signaling::run(
            ctx,
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
                PathBuf::from("unused-in-tests.toml"),
                std::sync::Arc::new(tokio::sync::Mutex::new(())),
                exec_enabled,
                remote_config_enabled,
            ),
            roomlerd::rc_sessions::RcSessionRegistry::new(),
        )
        .await;
    })
}

async fn enrol(app: &TestApp, t: &SeededTenant, machine_id: &str) -> AgentConfig {
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
    enrollment::enroll(enrollment::EnrollInputs {
        server_url: &app.base_url,
        enrollment_token: et["enrollment_token"].as_str().unwrap(),
        machine_id,
        machine_name: "Knowhow scan host",
        install: None,
    })
    .await
    .expect("agent enrollment")
}

/// The device's scan is linked once its hello has been read: ask until the
/// server takes it.
async fn ask_when_linked(app: &TestApp, t: &SeededTenant, device: &str) -> String {
    for _ in 0..100 {
        let (code, asked) = ask(app, t, &t.admin.access_token, device).await;
        if code == 202 {
            return asked["id"].as_str().unwrap().to_string();
        }
        assert_eq!(code, 409, "{asked}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the device never said it can scan");
}

/// Ask, and the scan once its answer was written.
async fn scan_once(app: &TestApp, t: &SeededTenant, device: &str, ends: &str) -> Value {
    let id = ask_when_linked(app, t, device).await;
    wait_scan(app, t, device, |s| {
        s["id"] == json!(id) && s["state"] == json!(ends) && s.get("written").is_some()
    })
    .await
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A checkout with one commit and `origin` at `remote`; its first commit.
fn checkout(at: &Path, remote: &str) -> String {
    std::fs::create_dir_all(at).unwrap();
    git(at, &["init", "-q", "-b", "main"]);
    git(at, &["remote", "add", "origin", remote]);
    git(
        at,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "first",
        ],
    );
    git(at, &["rev-list", "--max-parents=0", "HEAD"])
}

fn set_roots(config: &Path, roots: &[&Path]) {
    let path = config.to_path_buf();
    let mut cfg = roomlerd::config::load(&path).unwrap();
    cfg.knowhow_scan_roots = roots.iter().map(|p| p.display().to_string()).collect();
    roomlerd::config::save(&path, &cfg).unwrap();
}

/// A device scans the roots its config file lists, and the map holds its
/// checkout and the repository it belongs to; a root gone unread retires
/// nothing; a whole scan retires what it no longer finds; an emptied list
/// answers `no_roots`.
#[tokio::test]
async fn a_device_scans_the_roots_its_config_lists_onto_the_map() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowdevscan").await;
    let mut cfg = enrol(&app, &t, "mach-knowdevscan-a").await;
    let device = cfg.agent_id.clone();
    let did = ObjectId::parse_str(&device).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("src");
    let api = root.join("api");
    // A password in the remote, built here: the device takes it out.
    let first = checkout(
        &api,
        &format!(
            "https://dev:{}@github.com/acme/api.git",
            ["p", "w"].concat()
        ),
    );
    let config = base.join("config.toml");
    cfg.knowhow_scan_roots = vec![root.display().to_string()];
    roomlerd::config::save(&config, &cfg).unwrap();

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let agent = spawn_agent(cfg, config.clone(), stop_rx);

    // 1. Every root read: done, written, the checkout the device's.
    let s = scan_once(&app, &t, &device, "done").await;
    assert_eq!(s["counts"]["checkouts"], json!(1), "{s}");
    assert!(s.get("truncated").is_none(), "{s}");
    assert_eq!(s["written"]["checkouts"], json!(1), "{s}");
    assert_eq!(s["written"]["repos"], json!(1), "{s}");
    let key = checkout_key(did, api.to_str().unwrap());
    let c = card(&app, &t, "checkout", &key)
        .await
        .expect("the device's checkout, keyed as the intake keys it");
    assert_eq!(c["entity"]["device"], json!(device));
    assert_eq!(
        fact(&c, "remotes")["value"],
        json!([{ "name": "origin", "url": "https://github.com/acme/api.git" }]),
        "the remote without its password: {c}"
    );
    assert_eq!(fact(&c, "branch")["value"], json!("main"));
    assert_eq!(fact(&c, "root_commit")["value"], json!(first));
    assert!(
        card(&app, &t, "repo", "github.com/acme/api")
            .await
            .is_some()
    );

    // 2. A root unread: done but truncated, and nothing retired, though the
    // checkout is gone from the folder that was read.
    std::fs::remove_dir_all(&api).unwrap();
    let missing = base.join("unplugged");
    set_roots(&config, &[&root, &missing]);
    let s = scan_once(&app, &t, &device, "done").await;
    assert_eq!(s["truncated"], json!(true), "{s}");
    assert!(
        s["detail"].as_str().unwrap().contains("unplugged"),
        "the root not read is named: {s}"
    );
    assert_eq!(s["written"]["retired"], json!(0), "{s}");
    assert!(
        card(&app, &t, "checkout", &key).await.is_some(),
        "a truncated scan retires nothing"
    );

    // 3. Every root read again: the checkout it no longer finds is retired.
    set_roots(&config, &[&root]);
    let s = scan_once(&app, &t, &device, "done").await;
    assert!(s.get("truncated").is_none(), "{s}");
    assert!(
        card(&app, &t, "checkout", &key).await.is_none(),
        "a whole scan retires what it no longer finds"
    );
    assert!(
        card(&app, &t, "repo", "github.com/acme/api")
            .await
            .is_some(),
        "never a shared entity"
    );

    // 4. No root listed: `no_roots`, read from the file with no restart.
    set_roots(&config, &[]);
    let s = scan_once(&app, &t, &device, "no_roots").await;
    assert!(s.get("detail").is_none(), "{s}");

    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(10), agent).await;
}

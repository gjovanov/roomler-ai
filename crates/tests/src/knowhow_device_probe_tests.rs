// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-5-1b — a device's probe end to end: a person asks, and the REAL
//! `roomlerd` signaling loop probes within what its config file lists in
//! `knowhow_probe_reach`, and what it measured lands on the map.
//!
//! What these lock, that the unit tests cannot: the agent advertises
//! `knowhow-probe` and the server asks it; the frame reaches the prober on
//! the primary's context; the list comes from the config FILE at each probe;
//! a port listening here, on the device's own entity, is measured `ok` and
//! `direct`; and an emptied list answers `not_allowed`, which says nothing
//! of the facts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bson::{Document, doc, oid::ObjectId};
use roomlerd::{config::AgentConfig, encode::EncoderPreference, enrollment, signaling};
use serde_json::{Value, json};

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::knowhow_scan_tests::scan_app;
use crate::knowhow_tests::{call, kpath};

/// Spawn the agent signalling loop as `run_cmd` wires it, its prober
/// reading `config`.
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
            std::env::temp_dir().join(format!("roomler-knowhow-probe-consent-{}", cfg.agent_id)),
        )
        .expect("consent broker init");
        let lock: roomlerd::config::WriteLock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let prober = roomlerd::hive::knowhow_probe::Prober::new(config, lock);
        let ctx = signaling::OrgCtx::primary(roomlerd::token_slot::TokenSlot::new(
            cfg.agent_token.clone(),
        ))
        .with_prober(prober);
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
        machine_name: "Knowhow probe host",
        install: None,
    })
    .await
    .expect("agent enrollment")
}

fn set_reach(config: &Path, reach: &[String]) {
    let path = config.to_path_buf();
    let mut cfg = roomlerd::config::load(&path).unwrap();
    cfg.knowhow_probe_reach = reach.to_vec();
    roomlerd::config::save(&path, &cfg).unwrap();
}

/// A listener on 127.0.0.1 that accepts and closes; its port.
async fn listening() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            drop(tcp);
        }
    });
    port
}

/// The device's own listener on the map, as its scan writes one (P4-3a-2):
/// an endpoint keyed by its node id and port, its address and port the
/// scan's facts. A loopback target is probed only from such an entity's own
/// device.
async fn own_endpoint(app: &TestApp, t: &SeededTenant, device: ObjectId, port: u16) -> String {
    let tid = ObjectId::parse_str(&t.tenant_id).unwrap();
    let now = bson::DateTime::now();
    let fact = |path: &str, value: bson::Bson| {
        doc! {
            "path": path, "value": value, "source": "scan", "device": device,
            "observed_at": now, "status": "active",
        }
    };
    let id = app
        .db
        .collection::<Document>("knowhow_entities")
        .insert_one(doc! {
            "tenant_id": tid, "kind": "endpoint", "key": format!("{}:{port}/tcp", device.to_hex()),
            "name": format!("port:{port}"), "attrs": {},
            "facts": [
                fact("address", bson::Bson::String("127.0.0.1".into())),
                fact("port", bson::Bson::Int32(i32::from(port))),
            ],
            "status": "active", "source": "scan", "device": device, "rev": 1_i64,
            "created_at": now, "updated_at": now, "deleted_at": bson::Bson::Null,
        })
        .await
        .unwrap()
        .inserted_id;
    id.as_object_id().unwrap().to_hex()
}

/// Ask until the server takes it: the device can probe once its hello has
/// been read. The probe's id.
async fn ask_when_linked(app: &TestApp, t: &SeededTenant, entity: &str, device: &str) -> String {
    for _ in 0..100 {
        let (code, asked) = call(
            app.auth_post(&kpath(&t.tenant_id, "/probe"), &t.admin.access_token)
                .json(&json!({ "entity": entity, "device": device, "kind": "tcp" })),
        )
        .await;
        if code == 202 {
            return asked["id"].as_str().unwrap().to_string();
        }
        assert_eq!(code, 409, "{asked}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the device never said it can probe");
}

/// The probe once it ended.
async fn ended(app: &TestApp, t: &SeededTenant, entity: &str, id: &str) -> Value {
    for _ in 0..150 {
        let (code, body) = call(app.auth_get(
            &kpath(&t.tenant_id, &format!("/probe?entity={entity}")),
            &t.admin.access_token,
        ))
        .await;
        assert_eq!(code, 200, "{body}");
        if let Some(p) = body["probes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == json!(id))
            && !matches!(p["state"].as_str(), Some("requested" | "delivered"))
            && (p["state"] != json!("ok") || p.get("measured").is_some())
        {
            return p.clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("probe {id} never ended");
}

async fn facts(app: &TestApp, t: &SeededTenant, entity: &str) -> Value {
    let (code, card) = call(app.auth_get(
        &kpath(&t.tenant_id, &format!("/entity/{entity}")),
        &t.admin.access_token,
    ))
    .await;
    assert_eq!(code, 200, "{card}");
    let mut by_path = serde_json::Map::new();
    for f in card["entity"]["facts"].as_array().unwrap() {
        by_path.insert(f["path"].as_str().unwrap().to_string(), f.clone());
    }
    Value::Object(by_path)
}

/// A device probes within the list its config file holds, and the map's
/// facts say what it measured; with the list emptied it answers
/// `not_allowed`, and the facts keep what the last probe found.
#[tokio::test]
async fn a_device_probes_within_its_list_onto_the_map() {
    let app = scan_app().await;
    let t = app.seed_tenant("knowdevprobe").await;
    let mut cfg = enrol(&app, &t, "mach-knowdevprobe-a").await;
    let device = cfg.agent_id.clone();
    let did = ObjectId::parse_str(&device).unwrap();
    let port = listening().await;
    let entity = own_endpoint(&app, &t, did, port).await;

    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    cfg.knowhow_probe_reach = vec![format!("127.0.0.1:{port}")];
    roomlerd::config::save(&config, &cfg).unwrap();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let agent = spawn_agent(cfg, config.clone(), stop_rx);

    let id = ask_when_linked(&app, &t, &entity, &device).await;
    let p = ended(&app, &t, &entity, &id).await;
    assert_eq!(p["state"], json!("ok"), "{p}");
    assert_eq!(
        (p["address"].clone(), p["carrier"].clone()),
        (json!("127.0.0.1"), json!("direct")),
        "{p}"
    );
    assert!(p["rtt_ms"].is_u64(), "{p}");
    assert_eq!(
        p["measured"],
        json!([{ "path": "address", "ok": true }, { "path": "port", "ok": true }])
    );
    let f = facts(&app, &t, &entity).await;
    assert_eq!(f["port"]["verified"]["ok"], json!(true), "{f}");
    assert_eq!(f["port"]["verified"]["from_device"], json!(device));

    // The list is read again at each probe: emptied, nowhere.
    set_reach(&config, &[]);
    let id = ask_when_linked(&app, &t, &entity, &device).await;
    let p = ended(&app, &t, &entity, &id).await;
    assert_eq!(p["state"], json!("not_allowed"), "{p}");
    assert!(
        p["detail"]
            .as_str()
            .unwrap()
            .contains("knowhow_probe_reach"),
        "{p}"
    );
    assert!(p.get("measured").is_none(), "{p}");
    let after = facts(&app, &t, &entity).await;
    assert_eq!(after, f, "the device may not reach it: said of no fact");

    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(10), agent).await;
}

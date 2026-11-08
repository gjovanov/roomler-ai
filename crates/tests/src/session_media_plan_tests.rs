// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-135 R2 — a session says what it got, through a REAL agent and a real
//! webrtc-rs viewer peer. Before this a viewer learned its encoder only from
//! `rc:video-info`, which only an opened encoder sends, so a pump that could not
//! open one left the page black with no reason anywhere but the host's log.
//!
//! * the agent advertises `media-plan`, the verb a viewer gates on;
//! * the plan reaches the page's control channel, though it is written before
//!   that channel opens, with what is known at the start and nothing else;
//! * a viewer's `rc:viewer-hz` reaches the agent, and one no screen could send
//!   does not.
//!
//! This test agent has no encoder compiled in, so it never reaches the FFmpeg
//! data-channel pump: the plan's later writes are in `media_plan::tests`, and the
//! pump's own read on a guest. The switch (`ROOMLERD_MEDIA_PLAN=0`) is read on a
//! guest too, because nothing in this binary writes the environment: its real
//! agents read it on other threads, and glibc's `setenv` does not take that.
//!
//! The agent has no route that says what it heard, so `rc:viewer-hz` is read from
//! its log line, which is what the field reads.

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::session_carrier_tests::{
    Ws, enrol_real, is_overlay_range, spawn_real_agent, urlencode, wait_online,
};
use futures::{SinkExt, StreamExt};
use roomlerd::config::AgentConfig;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

/// What this test thread logged. `#[tokio::test]` runs the agent's tasks on this
/// one thread, so a thread's default subscriber sees them, and a parallel test's
/// agent stays out of it. The server's tasks too (#3517's
/// `agent_socket_end_tests` reads the agent socket's disconnect line with it).
#[derive(Clone, Default)]
pub(crate) struct Logged(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logged {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logged {
    type Writer = Logged;
    fn make_writer(&'a self) -> Logged {
        self.clone()
    }
}

impl Logged {
    pub(crate) fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::INFO)
                .with_ansi(false)
                .with_writer(self.clone())
                .finish(),
        )
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// A viewer on a real session: the page's control channel, and each JSON message
/// that came in on it. `_ctl` stays open, or the server ends the session.
struct Viewer {
    pc: Arc<RTCPeerConnection>,
    control: Arc<RTCDataChannel>,
    inbox: mpsc::UnboundedReceiver<Value>,
    _ctl: Ws,
}

/// Ask for a session and run the handshake until the control channel opens.
async fn open_viewer(app: &TestApp, seeded: &SeededTenant, cfg: &AgentConfig) -> Viewer {
    let mut me = MediaEngine::default();
    me.register_default_codecs().unwrap();
    // No host candidate on an overlay address (see `session_carrier_tests`).
    let mut setting = SettingEngine::default();
    setting.set_ip_filter(Box::new(|ip| !is_overlay_range(ip)));
    let api = APIBuilder::new()
        .with_media_engine(me)
        .with_setting_engine(setting)
        .build();
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap(),
    );
    let control = pc
        .create_data_channel("control", Some(RTCDataChannelInit::default()))
        .await
        .unwrap();
    let (inbox_tx, inbox) = mpsc::unbounded_channel::<Value>();
    control.on_message(Box::new(move |m: DataChannelMessage| {
        if let Ok(v) = serde_json::from_slice::<Value>(&m.data) {
            let _ = inbox_tx.send(v);
        }
        Box::pin(async {})
    }));
    let (cand_tx, mut cand_rx) = mpsc::unbounded_channel::<Value>();
    pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
        let cand_tx = cand_tx.clone();
        Box::pin(async move {
            if let Some(c) = c
                && let Ok(init) = c.to_json()
                && let Ok(val) = serde_json::to_value(init)
            {
                let _ = cand_tx.send(val);
            }
        })
    }));
    let offer = pc.create_offer(None).await.unwrap();
    pc.set_local_description(offer.clone()).await.unwrap();

    let url = format!(
        "ws://{}/ws?token={}",
        app.addr,
        urlencode(&seeded.admin.access_token)
    );
    let (mut ctl, _) = connect_async(&url).await.expect("controller ws");
    let _ = tokio::time::timeout(Duration::from_millis(300), ctl.next()).await;
    ctl.send(Message::Text(
        json!({ "t": "rc:session.request", "agent_id": cfg.agent_id, "permissions": "VIEW" })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    let mut sid_hex = String::new();
    let mut answered = false;
    let mut pending_agent_ice: Vec<Value> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while control.ready_state() != RTCDataChannelState::Open {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the control channel never opened"
        );
        if let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(200), ctl.next()).await
            && let Ok(v) = serde_json::from_str::<Value>(&text)
        {
            match v.get("t").and_then(|t| t.as_str()) {
                Some("rc:session.created") => {
                    sid_hex = v["session_id"].as_str().unwrap().to_string();
                }
                Some("rc:ready") => {
                    ctl.send(Message::Text(
                        json!({ "t": "rc:sdp.offer", "session_id": sid_hex, "sdp": offer.sdp })
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                }
                Some("rc:sdp.answer") => {
                    let answer =
                        RTCSessionDescription::answer(v["sdp"].as_str().unwrap().to_string())
                            .unwrap();
                    pc.set_remote_description(answer).await.unwrap();
                    answered = true;
                    for c in pending_agent_ice.drain(..) {
                        if let Ok(init) = serde_json::from_value::<RTCIceCandidateInit>(c) {
                            let _ = pc.add_ice_candidate(init).await;
                        }
                    }
                }
                Some("rc:ice") => {
                    let cand = v["candidate"].clone();
                    if answered {
                        if let Ok(init) = serde_json::from_value::<RTCIceCandidateInit>(cand) {
                            let _ = pc.add_ice_candidate(init).await;
                        }
                    } else {
                        pending_agent_ice.push(cand);
                    }
                }
                _ => {}
            }
        }
        if !sid_hex.is_empty() {
            while let Ok(cand) = cand_rx.try_recv() {
                let _ = ctl
                    .send(Message::Text(
                        json!({ "t": "rc:ice", "session_id": sid_hex, "candidate": cand })
                            .to_string()
                            .into(),
                    ))
                    .await;
            }
        }
    }
    Viewer {
        pc,
        control,
        inbox,
        _ctl: ctl,
    }
}

/// The next plan on the channel, past whatever else the agent says at the start.
async fn next_plan(viewer: &mut Viewer, within: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.checked_duration_since(tokio::time::Instant::now())?;
        let v = tokio::time::timeout(left, viewer.inbox.recv())
            .await
            .ok()??;
        if v["t"] == "rc:media-plan" {
            return Some(v);
        }
    }
}

#[tokio::test]
async fn a_session_says_its_plan_on_the_control_channel_and_hears_the_viewers_refresh() {
    let logged = Logged::default();
    let _log = logged.install();
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("mediaplan").await;
    let cfg = enrol_real(&app, &seeded, "mediaplan-host").await;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let sig = spawn_real_agent(cfg.clone(), stop_rx);
    wait_online(&app, &seeded, &cfg).await;

    // The verb a viewer reads before it sends `rc:viewer-hz`.
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
    let rpc = row["capabilities"]["rpc"].as_array().expect("rpc").clone();
    assert!(
        rpc.iter().any(|v| v == "media-plan"),
        "the agent advertises the verb: {rpc:?}"
    );

    let mut viewer = open_viewer(&app, &seeded, &cfg).await;

    // The plan is written before this channel opens, and still arrives. It has
    // what the session knows at its start: no encoder has opened, and this agent
    // has no pump to open one.
    let plan = next_plan(&mut viewer, Duration::from_secs(10))
        .await
        .expect("no rc:media-plan on the control channel");
    assert_eq!(plan["epoch"], 1, "{plan}");
    assert_eq!(plan["transport"], "webrtc", "{plan}");
    assert!(
        plan["codec"].as_str().is_some_and(|c| !c.is_empty()),
        "{plan}"
    );
    assert_eq!(plan["ladder"], json!([]), "{plan}");
    for absent in ["cell", "chroma", "input", "fps", "capture", "stopped"] {
        assert!(plan.get(absent).is_none(), "{absent} is not known: {plan}");
    }

    // The channel is ordered, so once 60 has landed the one before it has been
    // read: a rate no screen has is dropped, and only what it could be is logged.
    viewer
        .control
        .send_text(json!({ "t": "rc:viewer-hz", "hz": 5000 }).to_string())
        .await
        .unwrap();
    viewer
        .control
        .send_text(json!({ "t": "rc:viewer-hz", "hz": 60 }).to_string())
        .await
        .unwrap();
    let mut heard = false;
    for _ in 0..50 {
        heard = logged
            .text()
            .lines()
            .any(|l| l.contains("control: rc:viewer-hz") && l.contains("hz=60"));
        if heard {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let log = logged.text();
    assert!(heard, "rc:viewer-hz never reached the agent");
    assert!(!log.contains("hz=5000"), "a rate no screen has was stored");

    // Nothing changed, and only a change is sent: that same plan, once. A late
    // second copy here would be a retry that lost count of what it told.
    assert_eq!(
        next_plan(&mut viewer, Duration::from_millis(1500)).await,
        None,
        "a second plan with nothing new"
    );

    viewer.pc.close().await.ok();
    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), sig).await;
}

/// FR-135 §7.1 — `rc_media_legacy`, pushed end to end, is in force from the
/// device's NEXT session with no restart: R2's plan is the path it sends back
/// here, because it is the one this agent, with no encoder compiled in, runs.
///
/// A device that opted in writes the list to its config file and reports it
/// under `live`, never `needs_restart`, which the listing resolves into
/// `applied`. The same process then opens a session that says no plan, where
/// the session before the push said one.
#[tokio::test]
async fn a_pushed_media_plan_entry_takes_the_next_sessions_plan_away_with_no_restart() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("mediaplanoff").await;
    let mut cfg = enrol_real(&app, &seeded, "mediaplanoff-host").await;
    // The device's own opt-in, set where only someone at the machine can set
    // it: in its config file.
    cfg.remote_config_enabled = true;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    roomlerd::config::save(&path, &cfg).unwrap();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let sig = crate::agent_tests::spawn_agent_signaling_on(
        roomlerd::signaling::OrgCtx::primary(roomlerd::token_slot::TokenSlot::new(
            cfg.agent_token.clone(),
        )),
        cfg.clone(),
        path.clone(),
        stop_rx,
    );
    wait_online(&app, &seeded, &cfg).await;

    // Before the push: the built-in default, a plan.
    let mut viewer = open_viewer(&app, &seeded, &cfg).await;
    assert!(
        next_plan(&mut viewer, Duration::from_secs(10))
            .await
            .is_some(),
        "the default session says its plan"
    );
    viewer.pc.close().await.ok();
    drop(viewer);

    let desired = format!(
        "/api/tenant/{}/agent/{}/desired-config",
        seeded.tenant_id, cfg.agent_id
    );
    let resp = app
        .auth_put(&desired, &seeded.admin.access_token)
        .json(&json!({ "rc_media_legacy": "media_plan" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "desired-config PUT");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["desired"]["rc_media_legacy"],
        json!("media_plan"),
        "{body}"
    );
    let revision = body["revision"].as_u64().expect("a revision");

    // FR-132 P1b delivers a save to a connected device at once.
    let url = format!("/api/tenant/{}/agent/{}", seeded.tenant_id, cfg.agent_id);
    let mut row = Value::Null;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        row = app
            .auth_get(&url, &seeded.admin.access_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if row["remote_config"]["report"]["revision"].as_u64() == Some(revision) {
            break;
        }
    }
    let rc = &row["remote_config"];
    assert_eq!(
        rc["report"]["revision"].as_u64(),
        Some(revision),
        "the device never reported on this revision: {row}"
    );
    assert_eq!(rc["report"]["outcome"].as_str(), Some("applied"), "{rc}");
    assert_eq!(rc["report"]["live"], json!(["rc_media_legacy"]), "{rc}");
    assert!(
        rc["report"]["needs_restart"]
            .as_array()
            .is_none_or(|l| l.is_empty()),
        "read at each session's open, so nothing waits on a restart: {rc}"
    );
    assert_eq!(rc["state"].as_str(), Some("applied"), "{rc}");
    assert_eq!(
        roomlerd::config::load(&path)
            .unwrap()
            .rc_media_legacy
            .as_deref(),
        Some("media_plan"),
        "the device wrote the list to its own config file"
    );

    // The next session, on the same process: no plan.
    let mut viewer = open_viewer(&app, &seeded, &cfg).await;
    assert_eq!(
        next_plan(&mut viewer, Duration::from_secs(3)).await,
        None,
        "media_plan is on this session's old path"
    );

    // What the door refuses, from the review: a spelling no device can read is
    // a 400 (it would be written, reported applied, and read as nothing), and
    // the key that turns new code on is the device's, a 403. Neither moves the
    // stored revision.
    for (body, want) in [
        (json!({ "rc_media_legacy": "media-plan" }), 400),
        (json!({ "rc_media_legacy": "Media_Plan" }), 400),
        (json!({ "rc_media_legacy": "media_plan;stats" }), 400),
        (json!({ "rc_media_canary": "media_plan" }), 403),
    ] {
        let resp = app
            .auth_put(&desired, &seeded.admin.access_token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), want, "{body}");
    }
    let row: Value = app
        .auth_get(&url, &seeded.admin.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        row["remote_config"]["desired"]["revision"].as_u64(),
        Some(revision),
        "a refused body stores nothing: {row}"
    );

    viewer.pc.close().await.ok();
    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), sig).await;
}

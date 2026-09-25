// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-85 P3b — the `record` DataChannel, end to end.
//!
//! Two `webrtc-rs` PeerConnections in loopback; the "agent" side runs the
//! PRODUCTION handler (`roomlerd::recording::remote::attach`), which drives
//! the PRODUCTION recorder supervisor, which launches the real `roomlerd
//! record` child on the synthetic frame source. The "controller" side speaks
//! the wire. What is checked is what a controller would see (the states),
//! what the device reports to the server (the activity queue), what the
//! banner shows (the session registry), and what lands on disk.
//!
//! No real companion is ever launched: the handler's `Companion` answer is
//! fixed per test. One process-wide recorder and gate set, so the tests run
//! one at a time.

#![cfg(all(
    feature = "recording",
    feature = "openh264-encoder",
    feature = "synthetic-frame-source"
))]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bson::oid::ObjectId;
use roomler_ai_remote_control::models::RecordingActivityKind;
use roomler_ai_remote_control::signaling::ClientMsg;
use roomlerd::consent::{ConsentBroker, Mode};
use roomlerd::indicator::ViewerIndicator;
use roomlerd::rc_sessions::RcSessionRegistry;
use roomlerd::recording::manager::RecordingManager;
use roomlerd::recording::remote::{self, Companion, SessionCtx};
use roomlerd::recording::sidecar::{Initiator, Sidecar};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;

/// One recorder and one gate set per process: one test at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Setup {
    /// Where recordings land (the configured `record_dir`).
    out: PathBuf,
    /// Consent markers, one sub-directory per test.
    consent_root: PathBuf,
    manager: Arc<RecordingManager>,
    _dir: tempfile::TempDir,
}

/// The process-wide recorder, installed once: the real `roomlerd record` on
/// the synthetic source, recording into a scratch folder.
fn setup() -> &'static Setup {
    static SETUP: OnceLock<Setup> = OnceLock::new();
    SETUP.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("Recordings");
        let consent_root = dir.path().join("consent");
        std::fs::create_dir_all(&consent_root).unwrap();
        let cfg_path = dir.path().join("config.toml");
        let mut cfg = roomler_node_core::config::test_fixture();
        cfg.record_dir = Some(out.to_string_lossy().into_owned());
        roomler_node_core::config::save(&cfg_path, &cfg).unwrap();
        let manager = Arc::new(
            RecordingManager::new(PathBuf::from(env!("CARGO_BIN_EXE_roomlerd")), cfg_path)
                .with_service_identity(false)
                .with_child_env([("ROOMLERD_SYNTHETIC_FRAMES", "1")]),
        );
        remote::install(manager.clone());
        Setup {
            out,
            consent_root,
            manager,
            _dir: dir,
        }
    })
}

/// Start a test from an idle recorder. A test that failed mid-recording
/// leaves the process-wide recorder running, and every later test would then
/// read `busy` instead of its own answer — one failure reported as five.
async fn idle(s: &Setup) {
    if s.manager.state().active {
        s.manager.stop_with(None).await;
    }
}

/// ONE runtime for every test in this file.
///
/// The recorder supervisor is process-wide, and the tasks it spawns (the
/// child's event reader, a recording's follower) live on the runtime that
/// started them. With a runtime per test (`#[tokio::test]`), a test that
/// failed mid-recording takes the reader down with its runtime: the
/// supervisor's state then says "recording" forever, [`idle`] cannot stop
/// what nothing reads, and every later test reads `busy` — one failure
/// reported as four (measured: the first negative-control run).
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("the test runtime")
    })
}

#[test]
fn the_gates_refuse_by_name_before_anything_runs() -> Result<()> {
    rt().block_on(the_gates_refuse_by_name_before_anything_runs_cell())
}

#[test]
fn no_indicator_surface_means_no_recording() -> Result<()> {
    rt().block_on(no_indicator_surface_means_no_recording_cell())
}

#[test]
fn an_auto_granted_session_records_and_stops() -> Result<()> {
    rt().block_on(an_auto_granted_session_records_and_stops_cell())
}

#[test]
fn the_host_is_asked_again_and_a_deny_holds() -> Result<()> {
    rt().block_on(the_host_is_asked_again_and_a_deny_holds_cell())
}

#[test]
fn every_way_it_ends_is_named() -> Result<()> {
    rt().block_on(every_way_it_ends_is_named_cell())
}

/// The owner's two gates, as a local `ConfigSet` would set them.
fn gates(enabled: bool, audio: bool) {
    let mut cfg = roomler_node_core::config::test_fixture();
    cfg.record_remote_enabled = enabled;
    cfg.record_remote_audio = audio;
    remote::adopt(&cfg);
}

fn mp4s(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "mp4"))
                .collect()
        })
        .unwrap_or_default()
}

/// How a test session is set up on the device side.
struct Opts {
    /// `false` = the grant lacks RECORD (the refusing channel).
    granted: bool,
    /// `Some` = the session was consented on the host, so it asks again.
    prompt_window: Option<Duration>,
    /// The companion's answer (the only indicator surface in these tests:
    /// the native badge is not built here).
    companion: bool,
    /// The session is on the banner (the registry) at all.
    listed: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            granted: true,
            prompt_window: None,
            companion: true,
            listed: true,
        }
    }
}

/// One session's controller end, plus what the test observes on the device.
struct Rig {
    dc: Arc<RTCDataChannel>,
    strings: mpsc::UnboundedReceiver<String>,
    activity: mpsc::Receiver<ClientMsg>,
    registry: RcSessionRegistry,
    broker: ConsentBroker,
    session_id: ObjectId,
    controller_user_id: ObjectId,
    pcs: (Arc<RTCPeerConnection>, Arc<RTCPeerConnection>),
    _kill_rx: mpsc::Receiver<ObjectId>,
}

impl Rig {
    async fn send(&self, v: Value) -> Result<()> {
        self.dc.send_text(v.to_string()).await.context("send")?;
        Ok(())
    }

    /// The next `rc:record.state`, within `within`.
    async fn state(&mut self, within: Duration) -> Result<Value> {
        let s = tokio::time::timeout(within, self.strings.recv())
            .await
            .map_err(|_| anyhow!("no rc:record.state within {within:?}"))?
            .ok_or_else(|| anyhow!("the channel closed"))?;
        let v: Value = serde_json::from_str(&s)?;
        assert_eq!(v["t"], "rc:record.state", "{v}");
        Ok(v)
    }

    /// States until one whose `state` is not `recording` (progress ticks).
    async fn until_not_recording(&mut self, within: Duration) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let v = self.state(left).await?;
            if v["state"] != "recording" {
                return Ok(v);
            }
        }
    }

    /// Everything the device reported so far (non-blocking).
    fn reports(&mut self) -> Vec<(RecordingActivityKind, Option<String>, Option<u64>)> {
        let mut out = Vec::new();
        while let Ok(m) = self.activity.try_recv() {
            if let ClientMsg::RecordingActivity {
                session_id,
                kind,
                reason,
                bytes,
                ..
            } = m
            {
                assert_eq!(
                    session_id, self.session_id,
                    "a report about another session"
                );
                out.push((kind, reason, bytes));
            }
        }
        out
    }

    fn banner_says_recording(&self) -> bool {
        self.registry.list().iter().any(|s| s.recording)
    }
}

async fn rig(opts: Opts) -> Result<Rig> {
    let s = setup();
    let (browser_pc, agent_pc) = mk_pc_pair().await?;
    let session_id = ObjectId::new();
    let controller_user_id = ObjectId::new();

    // The device side's surfaces: a real consent broker (its own marker
    // dir), and the session registry the companion's banner reads.
    let consent_dir = s.consent_root.join(session_id.to_hex());
    let broker = ConsentBroker::new(
        Mode::Prompt {
            timeout: Duration::from_secs(30),
        },
        consent_dir,
    )?;
    let registry = RcSessionRegistry::new();
    let (kill_tx, kill_rx) = mpsc::channel(4);
    let indicator = ViewerIndicator::disabled().with_registry(kill_tx, registry.clone());
    if opts.listed {
        indicator.show_session_full(
            session_id,
            "Tester".into(),
            "VIEW | RECORD".into(),
            String::new(),
        );
    }
    let (activity_tx, activity_rx) = mpsc::channel(64);
    let ctx = SessionCtx {
        session_id,
        controller_user_id,
        controller_name: "Tester".into(),
        org: String::new(),
        prompt_window: opts.prompt_window,
        consent: broker.clone(),
        indicator,
        outbound: activity_tx,
        companion: Companion::Fixed(opts.companion),
    };

    let browser_dc = browser_pc
        .create_data_channel("record", None)
        .await
        .context("create_data_channel(record)")?;
    let (agent_dc_tx, agent_dc_rx) = oneshot::channel::<Arc<RTCDataChannel>>();
    let agent_dc_tx = Arc::new(Mutex::new(Some(agent_dc_tx)));
    agent_pc.on_data_channel(Box::new(move |dc| {
        let tx = agent_dc_tx.clone();
        Box::pin(async move {
            if let Some(tx) = tx.lock().await.take() {
                let _ = tx.send(dc);
            }
        })
    }));
    let (strings_tx, strings_rx) = mpsc::unbounded_channel::<String>();
    browser_dc.on_message(Box::new(move |msg: DataChannelMessage| {
        let tx = strings_tx.clone();
        Box::pin(async move {
            if msg.is_string
                && let Ok(s) = std::str::from_utf8(&msg.data)
            {
                let _ = tx.send(s.to_string());
            }
        })
    }));
    exchange(&browser_pc, &agent_pc).await?;
    let agent_dc = tokio::time::timeout(Duration::from_secs(5), agent_dc_rx)
        .await
        .map_err(|_| anyhow!("agent on_data_channel timed out"))??;
    // The attach-time gate, as `peer.rs` runs it.
    if opts.granted {
        remote::attach(agent_dc, ctx);
    } else {
        remote::attach_refusing(agent_dc, session_id, "not_granted");
    }
    let (open_tx, open_rx) = oneshot::channel::<()>();
    let open_tx = Arc::new(Mutex::new(Some(open_tx)));
    browser_dc.on_open(Box::new(move || {
        let tx = open_tx.clone();
        Box::pin(async move {
            if let Some(tx) = tx.lock().await.take() {
                let _ = tx.send(());
            }
        })
    }));
    // An already-open channel does not fire `on_open` again.
    if browser_dc.ready_state()
        != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
    {
        tokio::time::timeout(Duration::from_secs(10), open_rx)
            .await
            .map_err(|_| anyhow!("record DC open timed out"))??;
    }
    Ok(Rig {
        dc: browser_dc,
        strings: strings_rx,
        activity: activity_rx,
        registry,
        broker,
        session_id,
        controller_user_id,
        pcs: (browser_pc, agent_pc),
        _kill_rx: kill_rx,
    })
}

async fn exchange(browser_pc: &RTCPeerConnection, agent_pc: &RTCPeerConnection) -> Result<()> {
    let offer = browser_pc.create_offer(None).await?;
    browser_pc.set_local_description(offer.clone()).await?;
    agent_pc.set_remote_description(offer).await?;
    let answer = agent_pc.create_answer(None).await?;
    agent_pc.set_local_description(answer.clone()).await?;
    browser_pc.set_remote_description(answer).await?;
    Ok(())
}

async fn mk_pc_pair() -> Result<(Arc<RTCPeerConnection>, Arc<RTCPeerConnection>)> {
    let api = || -> Result<_> {
        let mut me = MediaEngine::default();
        me.register_default_codecs()?;
        Ok(APIBuilder::new().with_media_engine(me).build())
    };
    let cfg = RTCConfiguration {
        ice_servers: vec![],
        ..Default::default()
    };
    let browser_pc = Arc::new(api()?.new_peer_connection(cfg.clone()).await?);
    let agent_pc = Arc::new(api()?.new_peer_connection(cfg).await?);
    let a = agent_pc.clone();
    browser_pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
        let a = a.clone();
        Box::pin(async move {
            if let Some(c) = c
                && let Ok(j) = c.to_json()
            {
                let _ = a.add_ice_candidate(j).await;
            }
        })
    }));
    let b = browser_pc.clone();
    agent_pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
        let b = b.clone();
        Box::pin(async move {
            if let Some(c) = c
                && let Ok(j) = c.to_json()
            {
                let _ = b.add_ice_candidate(j).await;
            }
        })
    }));
    Ok((browser_pc, agent_pc))
}

/// The prompt standing for this session: its marker's id and body.
async fn standing_prompt(r: &Rig) -> Result<(String, Value)> {
    let dir = r.broker.sentinel_dir().to_path_buf();
    for _ in 0..100 {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "pending")
                    && let Ok(body) = std::fs::read_to_string(&p)
                    && let Ok(v) = serde_json::from_str::<Value>(&body)
                {
                    let id = p.file_stem().unwrap().to_string_lossy().into_owned();
                    return Ok((id, v));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(anyhow!("no prompt appeared"))
}

/// Answer the standing prompt the way the native panel or the companion does
/// (the broker counts only an answer to a question it is asking, so retry
/// until it is).
async fn answer(r: &Rig, id: &str, allow: bool) {
    for _ in 0..100 {
        if r.broker.record_decision(id, allow) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the broker never took the answer");
}

const START: Duration = Duration::from_secs(25);
const STOP: Duration = Duration::from_secs(30);

/// Nothing starts without the owner's gate; each refusal says why, reaches
/// the server as a claim, and leaves no file and no banner behind.
async fn the_gates_refuse_by_name_before_anything_runs_cell() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let s = setup();
    idle(s).await;
    let before = mp4s(&s.out).len();

    gates(false, false);
    let mut r = rig(Opts::default()).await?;
    r.send(json!({"t": "rc:record.start", "id": "g1"})).await?;
    let v = r.state(START).await?;
    assert_eq!(
        (v["id"].as_str(), v["state"].as_str()),
        (Some("g1"), Some("refused"))
    );
    assert_eq!(v["reason"], "disabled_on_device");

    gates(true, false);
    r.send(json!({"t": "rc:record.start", "id": "g2", "audio": true}))
        .await?;
    let v = r.state(START).await?;
    assert_eq!(v["reason"], "audio_not_allowed", "{v}");

    let reports = r.reports();
    assert_eq!(
        reports
            .iter()
            .map(|(k, why, _)| (*k, why.clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                RecordingActivityKind::Refused,
                Some("disabled_on_device".into())
            ),
            (
                RecordingActivityKind::Refused,
                Some("audio_not_allowed".into())
            ),
        ]
    );
    assert!(!r.banner_says_recording());
    assert_eq!(mp4s(&s.out).len(), before, "a refused start wrote a file");

    // A session whose grant lacks RECORD gets a channel that refuses.
    let mut denied = rig(Opts {
        granted: false,
        ..Default::default()
    })
    .await?;
    denied
        .send(json!({"t": "rc:record.start", "id": "n1"}))
        .await?;
    let v = denied.state(START).await?;
    assert_eq!(
        (v["state"].as_str(), v["reason"].as_str()),
        (Some("refused"), Some("not_granted"))
    );
    Ok(())
}

/// ⚠️ Nothing on screen to say "recording" ⇒ no recording, and no frame.
async fn no_indicator_surface_means_no_recording_cell() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let s = setup();
    idle(s).await;
    let before = mp4s(&s.out).len();
    gates(true, false);
    let mut r = rig(Opts {
        companion: false,
        ..Default::default()
    })
    .await?;
    r.send(json!({"t": "rc:record.start", "id": "i1"})).await?;
    let v = r.state(START).await?;
    assert_eq!(v["reason"], "no_indicator_surface", "{v}");
    assert!(
        !r.banner_says_recording(),
        "the mark was not taken back down"
    );
    assert!(!s.manager.state().active, "a recorder is running anyway");
    assert_eq!(mp4s(&s.out).len(), before);

    // The same with the companion up but the session not on the banner.
    let mut r = rig(Opts {
        listed: false,
        ..Default::default()
    })
    .await?;
    r.send(json!({"t": "rc:record.start", "id": "i2"})).await?;
    let v = r.state(START).await?;
    assert_eq!(v["reason"], "no_indicator_surface", "{v}");
    Ok(())
}

/// An auto-granted session records: the banner is up before the file is
/// written, progress arrives, a Stop ends it `requested`, the sidecar names
/// the CONTROLLER (downloads are owned by user), and the device reports it.
async fn an_auto_granted_session_records_and_stops_cell() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let s = setup();
    idle(s).await;
    gates(true, false);
    let mut r = rig(Opts::default()).await?;
    r.send(json!({"t": "rc:record.start", "id": "a1"})).await?;
    let v = r.state(START).await?;
    assert_eq!(v["state"], "recording", "{v}");
    let name = v["name"].as_str().expect("a file name").to_string();
    assert!(
        !name.contains('/') && !name.contains('\\'),
        "the controller is told a name, never a path: {name}"
    );
    assert!(r.banner_says_recording(), "no banner while recording");
    assert_eq!(
        s.manager.state().remote_controller.as_deref(),
        Some("Tester"),
        "the host's own surfaces must say who it is for"
    );

    // A second start while this one runs is refused, by name.
    r.send(json!({"t": "rc:record.start", "id": "a2"})).await?;
    let v = r.until_not_recording(START).await?;
    assert_eq!(
        (v["id"].as_str(), v["reason"].as_str()),
        (Some("a2"), Some("busy"))
    );

    tokio::time::sleep(Duration::from_millis(2500)).await;
    r.send(json!({"t": "rc:record.stop", "id": "a1"})).await?;
    let v = r.until_not_recording(STOP).await?;
    assert_eq!(v["state"], "stopped", "{v}");
    assert_eq!(v["reason"], "requested");
    assert!(v["bytes"].as_u64().unwrap_or(0) > 0, "{v}");
    assert_eq!(v["name"].as_str(), Some(name.as_str()));
    assert!(
        !r.banner_says_recording(),
        "the banner outlived the recording"
    );

    let file = s.out.join(&name);
    assert!(file.is_file(), "{} missing", file.display());
    let sc: Sidecar = serde_json::from_str(&std::fs::read_to_string(Sidecar::path_for(&file))?)?;
    match &sc.initiator {
        Initiator::Remote {
            controller_user_id, ..
        } => assert_eq!(controller_user_id, &r.controller_user_id.to_hex()),
        other => panic!("the sidecar does not name the controller: {other:?}"),
    }
    assert!(
        !sc.audio.microphone,
        "a remote recording never takes the microphone"
    );

    let kinds: Vec<_> = r
        .reports()
        .into_iter()
        .map(|(k, why, bytes)| (k, why, bytes.is_some_and(|b| b > 0)))
        .collect();
    assert_eq!(
        kinds.first().map(|k| k.0),
        Some(RecordingActivityKind::Started)
    );
    assert!(
        kinds.contains(&(
            RecordingActivityKind::Stopped,
            Some("requested".into()),
            true
        )),
        "{kinds:?}"
    );
    let _ = std::fs::remove_file(Sidecar::path_for(&file));
    let _ = std::fs::remove_file(&file);
    Ok(())
}

/// A session consented ON THE HOST asks the host again, with a FRESH id.
/// A deny is final for a minute; an approval records.
async fn the_host_is_asked_again_and_a_deny_holds_cell() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let s = setup();
    idle(s).await;
    gates(true, false);
    let prompted = || Opts {
        prompt_window: Some(Duration::from_secs(20)),
        ..Default::default()
    };

    let mut r = rig(prompted()).await?;
    r.send(json!({"t": "rc:record.start", "id": "p1"})).await?;
    let v = r.state(START).await?;
    assert_eq!(v["state"], "pending_consent", "{v}");
    let (prompt_id, body) = standing_prompt(&r).await?;
    assert_eq!(body["kind"], "record", "{body}");
    assert_ne!(
        prompt_id,
        r.session_id.to_hex(),
        "the recording prompt reused the SESSION's id — its answer could be the session's"
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or("")
            .contains("record this screen"),
        "an older companion shows only the detail line: {body}"
    );
    answer(&r, &prompt_id, false).await;
    let v = r.state(START).await?;
    assert_eq!(
        (v["state"].as_str(), v["reason"].as_str()),
        (Some("refused"), Some("consent_denied"))
    );
    // Asking again straight away is refused without bothering the host.
    r.send(json!({"t": "rc:record.start", "id": "p2"})).await?;
    let v = r.state(START).await?;
    assert_eq!(v["reason"], "rate_limited", "{v}");
    let kinds: Vec<_> = r
        .reports()
        .into_iter()
        .map(|(k, why, _)| (k, why))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (RecordingActivityKind::PromptDenied, None),
            (
                RecordingActivityKind::Refused,
                Some("consent_denied".into())
            ),
            (RecordingActivityKind::Refused, Some("rate_limited".into())),
        ]
    );

    // Another session: the host approves, and it records.
    let mut r = rig(prompted()).await?;
    r.send(json!({"t": "rc:record.start", "id": "p3"})).await?;
    assert_eq!(r.state(START).await?["state"], "pending_consent");
    let (prompt_id, _) = standing_prompt(&r).await?;
    answer(&r, &prompt_id, true).await;
    let v = r.state(START).await?;
    assert_eq!(v["state"], "recording", "{v}");
    let name = v["name"].as_str().unwrap().to_string();
    r.send(json!({"t": "rc:record.stop", "id": "p3"})).await?;
    let v = r.until_not_recording(STOP).await?;
    assert_eq!(v["state"], "stopped", "{v}");
    let kinds: Vec<_> = r.reports().into_iter().map(|(k, _, _)| k).collect();
    assert_eq!(kinds.first(), Some(&RecordingActivityKind::PromptGranted));
    let file = s.out.join(&name);
    let _ = std::fs::remove_file(Sidecar::path_for(&file));
    let _ = std::fs::remove_file(&file);
    Ok(())
}

/// The owner switching remote recording off stops it (`gate_revoked`); the
/// host's own Stop is `host_stopped`; the session ending is `session_ended`.
async fn every_way_it_ends_is_named_cell() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let s = setup();
    idle(s).await;

    // The owner's OFF.
    gates(true, false);
    let mut r = rig(Opts::default()).await?;
    r.send(json!({"t": "rc:record.start", "id": "e1"})).await?;
    assert_eq!(r.state(START).await?["state"], "recording");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    gates(false, false);
    let v = r.until_not_recording(STOP).await?;
    assert_eq!(
        (v["state"].as_str(), v["reason"].as_str()),
        (Some("stopped"), Some("gate_revoked")),
        "{v}"
    );
    let _ = remove(&s.out, &v);

    // The host's Stop (the LocalAPI verb, the banner, the tray).
    gates(true, false);
    let mut r = rig(Opts::default()).await?;
    r.send(json!({"t": "rc:record.start", "id": "e2"})).await?;
    assert_eq!(r.state(START).await?["state"], "recording");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    s.manager.stop().await;
    let v = r.until_not_recording(STOP).await?;
    assert_eq!(v["reason"], "host_stopped", "{v}");
    let _ = remove(&s.out, &v);

    // The session ending: the controller's side goes away.
    let r = rig(Opts::default()).await?;
    let mut r = r;
    r.send(json!({"t": "rc:record.start", "id": "e3"})).await?;
    let v = r.state(START).await?;
    assert_eq!(v["state"], "recording");
    let name = v["name"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    r.pcs.0.close().await?;
    let deadline = tokio::time::Instant::now() + STOP;
    while s.manager.state().active && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let st = s.manager.state();
    assert!(!st.active, "the recording outlived its session");
    assert_eq!(
        st.last.as_ref().map(|l| l.reason.as_str()),
        Some("session_ended")
    );
    let file = s.out.join(&name);
    let _ = std::fs::remove_file(Sidecar::path_for(&file));
    let _ = std::fs::remove_file(&file);
    Ok(())
}

fn remove(out: &Path, v: &Value) -> Option<()> {
    let file = out.join(v["name"].as_str()?);
    let _ = std::fs::remove_file(Sidecar::path_for(&file));
    std::fs::remove_file(&file).ok()
}

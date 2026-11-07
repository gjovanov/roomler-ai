// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-132 P1 (§3b) — the viewer's decode errors, counted for a fleet read.
//! Before this a decode error in the viewer's worker reached nobody: the page
//! fell back to `<video>`, and a fleet week of a GPU-input cell could not be
//! read for decode errors without someone watching a viewer.
//!
//! * the server stores `remote_sessions.stats.decode_errors` from a sample
//!   that counted them, keeps a count when a later sample did not count, and
//!   writes nothing for a session whose samples never counted (absent is "not
//!   counted", never zero);
//! * end to end, through a REAL agent and a real webrtc-rs viewer peer: the
//!   viewer's `rc:decodestat` with `de: 1` makes the session's zero a stored
//!   zero, and one `rc:decode-error` on the control channel lands on the row
//!   as 1, each by an early sample well inside the agent's first 30 s
//!   heartbeat tick, so a session someone leaves right after still records it.
//!
//! The rows are read straight from Mongo: the stats have no controller-facing
//! route, as the carrier has none (`session_carrier_tests`).

use crate::fixtures::test_app::TestApp;
use crate::session_carrier_tests::{
    Ws, ask, connect_agent, enrol_real, is_overlay_range, spawn_real_agent, urlencode,
    wait_for_session_row, wait_online,
};
use bson::{Document, doc, oid::ObjectId};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

/// One hand-made `rc:session.stats`, `decode_errors` present only when `Some`.
async fn report(ws: &mut Ws, session_id: &str, decode_errors: Option<u32>) {
    let mut msg = json!({
        "t": "rc:session.stats", "session_id": session_id,
        "bytes_sent": 1, "bytes_recv": 2, "fps": 30.0, "rtt_ms": 40.0,
        "keyframe_requests": 0, "input_events": 0,
    });
    if let Some(n) = decode_errors {
        msg["decode_errors"] = json!(n);
    }
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .unwrap();
}

/// The session row's `stats` document, if the row exists.
async fn stats_of(app: &TestApp, sid: &ObjectId) -> Option<Document> {
    app.db
        .collection::<Document>("remote_sessions")
        .find_one(doc! { "_id": sid })
        .await
        .unwrap()
        .and_then(|row| row.get_document("stats").ok().cloned())
}

/// The stored `stats.decode_errors`, or `None` while absent.
async fn stored_decode_errors(app: &TestApp, sid: &ObjectId) -> Option<i64> {
    stats_of(app, sid).await.and_then(|s| {
        s.get_i64("decode_errors")
            .ok()
            .or_else(|| s.get_i32("decode_errors").ok().map(i64::from))
    })
}

/// Whether the row carries a carrier yet: the carrier's own early sample
/// (FR-102 P9d) has landed.
async fn carrier_stored(app: &TestApp, sid: &ObjectId) -> bool {
    app.db
        .collection::<Document>("remote_sessions")
        .find_one(doc! { "_id": sid })
        .await
        .unwrap()
        .is_some_and(|row| row.get_str("carrier").is_ok())
}

/// `stats.decode_errors`, polled until it reads `want` (or ~5 s pass).
async fn await_decode_errors(app: &TestApp, sid: &ObjectId, want: Option<i64>) -> Option<i64> {
    let mut last = None;
    for _ in 0..50 {
        last = stored_decode_errors(app, sid).await;
        if last == want {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    last
}

#[tokio::test]
async fn the_server_stores_a_counted_decode_error_count_and_no_sample_unlearns_it() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("decerr").await;
    let (agent_id, mut agent) = connect_agent(&app, &seeded, "decerr-host").await;
    let (_ctl, sid) = ask(&app, &seeded.admin.access_token, &agent_id).await;
    wait_for_session_row(&app, &seeded, &sid).await;
    let sid_oid = ObjectId::parse_str(&sid).unwrap();

    // A sample that counted writes its count beside the other stats.
    report(&mut agent, &sid, Some(2)).await;
    assert_eq!(
        await_decode_errors(&app, &sid_oid, Some(2)).await,
        Some(2),
        "a counted sample's decode errors are stored"
    );

    // A later sample that did NOT count (an older agent, a viewer that never
    // decoded in a worker) leaves the count alone: absent is "not counted",
    // never a zero to write over it. The rest of that sample still lands.
    report(&mut agent, &sid, None).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        stored_decode_errors(&app, &sid_oid).await,
        Some(2),
        "a sample without the count never overwrites it"
    );
    let stats = stats_of(&app, &sid_oid).await.unwrap();
    assert_eq!(stats.get_i64("bytes_recv").unwrap(), 2, "the merge ran");

    // A higher cumulative count replaces it.
    report(&mut agent, &sid, Some(3)).await;
    assert_eq!(await_decode_errors(&app, &sid_oid, Some(3)).await, Some(3));

    // A session whose samples never counted has no such field at all. Its own
    // org and device: the plan runs one remote session per org at a time.
    let seeded2 = app.seed_tenant("decerr-none").await;
    let (agent_id2, mut agent2) = connect_agent(&app, &seeded2, "decerr-none-host").await;
    let (_ctl2, sid2) = ask(&app, &seeded2.admin.access_token, &agent_id2).await;
    wait_for_session_row(&app, &seeded2, &sid2).await;
    report(&mut agent2, &sid2, None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let sid2_oid = ObjectId::parse_str(&sid2).unwrap();
    let stats2 = stats_of(&app, &sid2_oid).await.expect("the session row");
    assert!(
        stats2.get("decode_errors").is_none(),
        "never counted, never written: {stats2:?}"
    );
    assert_eq!(stats2.get_i64("bytes_recv").unwrap(), 2, "the merge ran");
}

/// One hand-made `rc:session.stats` with a `video` reading.
async fn report_video(ws: &mut Ws, session_id: &str, video: Value) {
    let msg = json!({
        "t": "rc:session.stats", "session_id": session_id,
        "bytes_sent": 1, "bytes_recv": 2, "fps": 30.0, "rtt_ms": 40.0,
        "keyframe_requests": 0, "input_events": 0, "video": video,
    });
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .unwrap();
}

/// The session row's `video`, if it has one.
async fn stored_video(app: &TestApp, sid: &ObjectId) -> Option<Document> {
    app.db
        .collection::<Document>("remote_sessions")
        .find_one(doc! { "_id": sid })
        .await
        .unwrap()
        .and_then(|row| row.get_document("video").ok().cloned())
}

/// `video.encoder`, polled until it reads `want` (or ~5 s pass).
async fn await_video_encoder(app: &TestApp, sid: &ObjectId, want: &str) -> Document {
    for _ in 0..50 {
        if let Some(v) = stored_video(app, sid).await
            && v.get_str("encoder") == Ok(want)
        {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "video.encoder never read {want}: {:?}",
        stored_video(app, sid).await
    );
}

/// FR-132 P1d — the canary week's source. The row holds the newest reading of
/// what the video ran on, beside `stats`. A sample without one leaves it alone,
/// because absent is "not reported", never CPU. And the device is the only
/// writer, so a reading that is not short lowercase names is dropped whole and
/// the row keeps the last one it could trust.
#[tokio::test]
async fn the_server_stores_what_a_sessions_video_ran_on() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("sessvideo").await;
    let (agent_id, mut agent) = connect_agent(&app, &seeded, "sessvideo-host").await;
    let (_ctl, sid) = ask(&app, &seeded.admin.access_token, &agent_id).await;
    wait_for_session_row(&app, &seeded, &sid).await;
    let sid_oid = ObjectId::parse_str(&sid).unwrap();

    // A sample before the pump's first heartbeat, or from an older agent.
    report(&mut agent, &sid, None).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        stored_video(&app, &sid_oid).await.is_none(),
        "not reported, so nothing is written"
    );
    assert_eq!(
        stats_of(&app, &sid_oid)
            .await
            .unwrap()
            .get_i64("bytes_recv")
            .unwrap(),
        2,
        "the merge ran"
    );

    report_video(
        &mut agent,
        &sid,
        json!({
            "encoder": "h264_qsv", "capture": "dxgi",
            "input_path": "cpu", "input_reason": "denied",
        }),
    )
    .await;
    let video = await_video_encoder(&app, &sid_oid, "h264_qsv").await;
    assert_eq!(video.get_str("capture"), Ok("dxgi"));
    assert_eq!(video.get_str("input_path"), Ok("cpu"));
    assert_eq!(video.get_str("input_reason"), Ok("denied"));

    // A later sample without one leaves it alone.
    report(&mut agent, &sid, None).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        stored_video(&app, &sid_oid).await.as_ref(),
        Some(&video),
        "absent is not reported, never a reset"
    );

    // A newer reading replaces it whole: GPU input has no reason.
    report_video(
        &mut agent,
        &sid,
        json!({ "encoder": "av1_qsv", "capture": "dxgi", "input_path": "gpu" }),
    )
    .await;
    let video = await_video_encoder(&app, &sid_oid, "av1_qsv").await;
    assert!(video.get("input_reason").is_none(), "{video:?}");

    // What a host cannot put on the row: not a name, or far too long.
    for bad in [
        json!({ "encoder": "<b>h264</b>", "capture": "wgc", "input_path": "gpu" }),
        json!({ "encoder": "h264_qsv", "capture": "wgc", "input_path": "x".repeat(33) }),
        json!({ "encoder": "h264_qsv", "capture": "A Name", "input_path": "gpu" }),
    ] {
        report_video(&mut agent, &sid, bad).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        stored_video(&app, &sid_oid).await.as_ref(),
        Some(&video),
        "a reading that is not names is dropped whole"
    );
}

/// AC3 end to end: a REAL agent, a real viewer peer that opens the control
/// channel and sends what the page sends. The session is held well under the
/// agent's first 30 s heartbeat tick, so the only samples that can carry the
/// count are the two early ones this phase adds: the first `de` (a counted
/// zero), and the first decode error.
#[tokio::test]
async fn a_viewer_decode_error_reaches_the_session_row_at_once() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("decerr-real").await;
    let cfg = enrol_real(&app, &seeded, "decerr-real-host").await;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let sig = spawn_real_agent(cfg.clone(), stop_rx);
    wait_online(&app, &seeded, &cfg).await;

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
    let (cand_tx, mut cand_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
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

    // Handshake, cross-feed ICE both ways through the server, then drive the
    // control channel: `de` first (a counted zero), then one decode error.
    let mut sid: Option<ObjectId> = None;
    let mut sid_hex = String::new();
    let mut answered = false;
    let mut pending_agent_ice: Vec<Value> = Vec::new();
    let mut carrier_seen = false;
    let mut sent_de = false;
    let mut sent_error = false;
    let mut zero_seen: Option<i64> = None;
    let mut after_error: Option<i64> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(22);
    while tokio::time::Instant::now() < deadline && after_error != Some(1) {
        if let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(200), ctl.next()).await
            && let Ok(v) = serde_json::from_str::<Value>(&text)
        {
            match v.get("t").and_then(|t| t.as_str()) {
                Some("rc:session.created") => {
                    sid_hex = v["session_id"].as_str().unwrap().to_string();
                    sid = Some(ObjectId::parse_str(&sid_hex).unwrap());
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
        let Some(sid) = sid.as_ref() else { continue };
        if control.ready_state() != RTCDataChannelState::Open {
            continue;
        }
        // Wait out the one early sample that already exists (FR-102 P9d, the
        // carrier's): once it has landed, only this phase's two early samples
        // can carry a count before the 30 s tick, so each check below proves
        // its own trigger.
        if !carrier_seen {
            carrier_seen = carrier_stored(&app, sid).await;
            continue;
        }
        // What `decodeStatWireMessage` sends on every window, `de` included.
        if !sent_de {
            control
                .send_text(
                    json!({ "t": "rc:decodestat", "fps": 0, "struggling": false, "de": 1 })
                        .to_string(),
                )
                .await
                .unwrap();
            sent_de = true;
            continue;
        }
        if zero_seen.is_none() {
            if stored_decode_errors(&app, sid).await == Some(0) {
                zero_seen = Some(0);
            }
            continue;
        }
        // What `decodeErrorWireMessage` sends before the fallback.
        if !sent_error {
            control
                .send_text(
                    json!({
                        "t": "rc:decode-error", "stage": "decode", "path": "hevc",
                        "codec": "hev1.1.6.L153.B0", "error": "Decoding error.",
                    })
                    .to_string(),
                )
                .await
                .unwrap();
            sent_error = true;
            continue;
        }
        after_error = stored_decode_errors(&app, sid).await;
    }

    pc.close().await.ok();
    let _ = stop_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), sig).await;

    assert!(carrier_seen, "the carrier's early sample never landed");
    assert!(sent_de, "the control channel never opened");
    assert_eq!(
        zero_seen,
        Some(0),
        "a viewer that counts and has no errors yet is stored as a measured zero, at once"
    );
    assert!(sent_error, "the decode error was never sent");
    assert_eq!(
        after_error,
        Some(1),
        "one decode error on the control channel reaches the row as 1, well inside 30 s"
    );
}

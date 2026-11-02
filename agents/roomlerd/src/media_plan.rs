// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-135 R2 — `rc:media-plan`: what a session promised its viewer, and every
//! step it took down from there.
//!
//! Before this a viewer learned its encoder from `rc:video-info`, which only an
//! opened encoder sends. A pump that could not open one exited with a log line:
//! the page stayed black, read connected, and re-created the session every 10 s
//! with nothing on it to say why, while the host's log named the reason.
//!
//! So a session now states its plan on the p2p control channel when it starts
//! (the transport and the codec, known before any pump runs), and again on every
//! change. The FFmpeg data-channel pump writes the rest: the cell it opened, the
//! input form, the granted rate, and one `ladder` entry per step down. When
//! nothing is left it says `stopped`, and why.
//!
//! The server sees none of it, and needs no change. It is additive: an older
//! viewer drops an unknown `t`, and an older agent never sends it. The verb
//! [`roomler_ai_remote_control::models::RpcCap::MediaPlan`] tells a viewer this
//! agent will say, so it can read silence on it as a fault.
//!
//! ⚠️ The RTP pump and the VP9 pump are not instrumented, on purpose (FR-135
//! §7.5, the pumps its ratchet stops growing): their sessions carry the plan's
//! start only. Their fault ladders arrive with their stages (R8, R11).
//!
//! The kill switch is `ROOMLERD_MEDIA_PLAN=0`, which R0 folds into
//! `rc_media_legacy` as `media_plan`.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use dashmap::DashMap;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;

/// The most steps one plan carries. A cascade tries about a dozen cells, and a
/// session that keeps rebuilding must not grow a message without bound: past
/// this the steps are dropped, and `stopped` still says how it ended.
const MAX_LADDER: usize = 24;
/// The most characters of a backend's error text, in `why` and `detail`.
const MAX_WHY: usize = 160;
/// Between tries while the control channel is not open: it usually opens after
/// the pump starts, and on a relay well after.
const RETRY: Duration = Duration::from_millis(500);

/// `ROOMLERD_MEDIA_PLAN`, default on. Read at session open, and for the verb.
pub fn enabled() -> bool {
    tunnel_core::env::flag("MEDIA_PLAN", true)
}

/// One arrow in FR-135 §6.9's ladder: `at_ms` from the session's start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Step {
    pub from: String,
    pub to: String,
    pub why: String,
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Input {
    /// `gpu` or `cpu`.
    pub form: &'static str,
    /// Why not GPU, in the words of the encoder's heartbeat.
    #[serde(skip_serializing_if = "str::is_empty")]
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Fps {
    /// The ask, when it was refused. An ask that was granted is `granted`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asked: Option<u32>,
    pub granted: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capture {
    /// The capture backend, as it names itself.
    pub arm: &'static str,
    /// `gpu` or `cpu`: what its frames were when the encoder opened.
    pub form: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stopped {
    /// `no_encoder` or `encoder_fault`. A viewer shows an unknown word as it
    /// came, as it does an unknown FR-80 code.
    pub why: &'static str,
    /// The backend's own words.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// The message. `t`, like every control message, where FR-135 §6.7's sketch
/// says `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub t: &'static str,
    /// Rises by one per change, so a viewer keeps the newest.
    pub epoch: u64,
    /// What the session negotiated: a `data-channel-*`, or `webrtc`.
    pub transport: String,
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chroma: Option<&'static str>,
    /// `name:chroma`, the spelling `encoder_cells_deny` takes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Input>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<Fps>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture: Option<Capture>,
    pub ladder: Vec<Step>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped: Option<Stopped>,
}

/// What a backend's error text may put on a page: one line, bounded.
fn bounded(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_WHY)
        .collect()
}

impl Plan {
    fn new(transport: Option<&str>, chosen_codec: &str) -> Self {
        // A data-channel transport names its own codec, and the session's
        // `chosen_codec` is the RTP track's, a different pick.
        let codec = match transport {
            Some("data-channel-hevc") => "h265",
            Some("data-channel-av1") => "av1",
            Some("data-channel-h264") => "h264",
            Some("data-channel-vp9-444") => "vp9",
            _ => chosen_codec,
        };
        Self {
            t: "rc:media-plan",
            epoch: 1,
            transport: transport.unwrap_or("webrtc").to_string(),
            codec: codec.to_string(),
            chroma: None,
            cell: None,
            input: None,
            fps: None,
            capture: None,
            ladder: Vec::new(),
            stopped: None,
        }
    }

    /// The session ends up on the RTP track. A session that negotiated it reads
    /// so already; one that negotiated a data channel its pump is not in this
    /// build fell here, and a page decoding that channel would stay black.
    fn on_rtp_track(&mut self, chosen_codec: &str, at_ms: u64) {
        if self.transport != "webrtc" {
            let from = std::mem::replace(&mut self.transport, "webrtc".to_string());
            self.step(&from, "webrtc", "no pump for it on this device", at_ms);
            self.codec = chosen_codec.to_string();
        }
    }

    fn step(&mut self, from: &str, to: &str, why: &str, at_ms: u64) {
        if self.ladder.len() < MAX_LADDER {
            self.ladder.push(Step {
                from: from.to_string(),
                to: to.to_string(),
                why: bounded(why),
                at_ms,
            });
        }
    }

    /// The candidates that failed before the next one was tried, as arrows:
    /// each to the one after it, the last to `end`.
    fn failed_steps(&mut self, failed: &[(String, String)], end: &str, at_ms: u64) {
        for (i, (name, why)) in failed.iter().enumerate() {
            let to = failed.get(i + 1).map_or(end, |(next, _)| next.as_str());
            self.step(name, to, why, at_ms);
        }
    }

    fn encoder_opened(
        &mut self,
        name: &str,
        chroma444: bool,
        capture: Capture,
        failed: &[(String, String)],
        at_ms: u64,
    ) {
        let chroma = if chroma444 { "yuv444" } else { "yuv420" };
        let cell = format!("{name}:{chroma}");
        if let Some(previous) = self.cell.take().filter(|c| *c != cell) {
            self.step(&previous, &cell, "rebuild", at_ms);
        }
        self.failed_steps(failed, name, at_ms);
        self.chroma = Some(chroma);
        self.cell = Some(cell);
        self.capture = Some(capture);
        self.stopped = None;
    }

    /// `form` is `gpu` or `cpu`. The arrow is a fall, or a climb back: a form
    /// the session never had any other way (`switch_off`, `no_gpu_cell`) is
    /// where it stands, not a step down from anywhere.
    fn input_form(&mut self, gpu: bool, reason: &'static str, at_ms: u64) {
        let was = self.input.as_ref().map(|i| i.form);
        let form = if gpu { "gpu" } else { "cpu" };
        match (was, gpu) {
            (Some("cpu"), true) => self.step("cpu", "gpu", "reopened", at_ms),
            (Some("gpu"), false) => self.step("gpu", "cpu", reason, at_ms),
            (None, false) if !matches!(reason, "switch_off" | "no_gpu_cell") => {
                self.step("gpu", "cpu", reason, at_ms)
            }
            _ => {}
        }
        self.input = Some(Input { form, reason });
    }

    fn fps_answer(&mut self, granted: u32, refused: Option<(u32, &'static str)>, at_ms: u64) {
        let was = self.fps.as_ref().and_then(|f| f.asked.zip(f.reason));
        if let Some((asked, why)) = refused
            && asked != granted
            && was != Some((asked, why))
        {
            self.step(
                &format!("{asked} fps"),
                &format!("{granted} fps"),
                why,
                at_ms,
            );
        }
        self.fps = Some(Fps {
            asked: refused.map(|(asked, _)| asked),
            granted,
            reason: refused.map(|(_, why)| why),
        });
    }

    fn no_encoder(&mut self, error: &str, failed: &[(String, String)], at_ms: u64) {
        self.failed_steps(failed, "none", at_ms);
        self.cell = None;
        self.chroma = None;
        self.input = None;
        self.stopped = Some(Stopped {
            why: "no_encoder",
            detail: bounded(error),
        });
    }

    fn encoder_fault(&mut self, error: &str, at_ms: u64) {
        let from = self.cell.take().unwrap_or_default();
        self.step(&from, "none", error, at_ms);
        self.chroma = None;
        self.input = None;
        self.stopped = Some(Stopped {
            why: "encoder_fault",
            detail: bounded(error),
        });
    }
}

/// One session's plan: written by its pump, read by [`start`]'s emitter.
struct SessionPlan {
    started: Instant,
    plan: Mutex<Plan>,
    changed: tokio::sync::Notify,
}

impl SessionPlan {
    /// Only a real change reaches the wire. A caller may report the same thing
    /// every heartbeat.
    fn update(&self, change: impl FnOnce(&mut Plan, u64)) {
        let at_ms = self.started.elapsed().as_millis() as u64;
        let mut plan = self.plan.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = plan.clone();
        change(&mut next, at_ms);
        if next != *plan {
            next.epoch = plan.epoch + 1;
            *plan = next;
            self.changed.notify_one();
        }
    }

    /// The epoch and its payload, or `None` if it cannot be written.
    fn snapshot(&self) -> (u64, Option<String>) {
        let plan = self.plan.lock().unwrap_or_else(|e| e.into_inner());
        (plan.epoch, serde_json::to_string(&*plan).ok())
    }
}

fn registry() -> &'static DashMap<ObjectId, Arc<SessionPlan>> {
    static REG: std::sync::OnceLock<DashMap<ObjectId, Arc<SessionPlan>>> =
        std::sync::OnceLock::new();
    REG.get_or_init(DashMap::new)
}

fn with(session_id: ObjectId, change: impl FnOnce(&mut Plan, u64)) {
    // The guard ends with this statement, so `update` never holds it.
    let plan = registry().get(&session_id).map(|p| p.clone());
    if let Some(plan) = plan {
        plan.update(change);
    }
}

/// Begin this session's plan, and keep the viewer told of it until `until`.
///
/// Called before the pump is spawned, when the stash of the control channel is
/// still empty, so the first message waits in the retry below. With the switch
/// off nothing is registered, and every writer here does nothing.
pub fn start(
    session_id: ObjectId,
    transport: Option<&str>,
    chosen_codec: &str,
    control_dc: Arc<tokio::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    until: CancellationToken,
) {
    if !enabled() {
        return;
    }
    let plan = Arc::new(SessionPlan {
        started: Instant::now(),
        plan: Mutex::new(Plan::new(transport, chosen_codec)),
        changed: tokio::sync::Notify::new(),
    });
    registry().insert(session_id, plan.clone());
    tokio::spawn(async move {
        let run = async {
            let mut told = 0;
            loop {
                let (epoch, payload) = plan.snapshot();
                if epoch == told {
                    plan.changed.notified().await;
                    continue;
                }
                let dc = control_dc.lock().await.clone();
                let delivered = match (dc, payload) {
                    (Some(dc), Some(payload)) => dc.send_text(payload).await.is_ok(),
                    _ => false,
                };
                if delivered {
                    told = epoch;
                } else {
                    tokio::select! {
                        _ = plan.changed.notified() => {}
                        _ = tokio::time::sleep(RETRY) => {}
                    }
                }
            }
        };
        tokio::select! {
            biased;
            _ = until.cancelled() => {}
            _ = run => {}
        }
        registry().remove(&session_id);
    });
}

/// This session is on the RTP track. Always compiled: it is what a build without
/// a data-channel pump reaches.
pub(crate) fn on_rtp_track(session_id: ObjectId, chosen_codec: &str) {
    with(session_id, |p, at| p.on_rtp_track(chosen_codec, at));
}

/// The encoder this session runs. `failed` is what the cascade tried first.
#[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
pub(crate) fn encoder_opened(
    session_id: ObjectId,
    name: &str,
    chroma444: bool,
    capture_arm: &'static str,
    capture_gpu: bool,
    failed: &[(String, String)],
) {
    let capture = Capture {
        arm: capture_arm,
        form: if capture_gpu { "gpu" } else { "cpu" },
    };
    with(session_id, |p, at| {
        p.encoder_opened(name, chroma444, capture, failed, at)
    });
}

/// The encoder's input form. Safe to call every heartbeat.
#[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
pub(crate) fn input_form(session_id: ObjectId, gpu: bool, reason: &'static str) {
    with(session_id, |p, at| p.input_form(gpu, reason, at));
}

/// The frame-rate answer, as `rc:video-info` carries it.
#[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
pub(crate) fn fps_answer(session_id: ObjectId, granted: u32, refused: Option<(u32, &'static str)>) {
    with(session_id, |p, at| p.fps_answer(granted, refused, at));
}

/// Nothing opened, and the pump is exiting.
#[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
pub(crate) fn no_encoder(session_id: ObjectId, error: &str, failed: &[(String, String)]) {
    with(session_id, |p, at| p.no_encoder(error, failed, at));
}

/// The running encoder failed past its rebuilds, and the pump is exiting.
#[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
pub(crate) fn encoder_fault(session_id: ObjectId, error: &str) {
    with(session_id, |p, at| p.encoder_fault(error, at));
}

thread_local! {
    /// The open running on this thread, while a pump wants its failures.
    static RECORDING: RefCell<Option<Vec<(String, String)>>> = const { RefCell::new(None) };
}

/// The candidates one open tried and lost, in order. The cascade drops each
/// failure on its way to the next, and keeps only the last in the error it
/// returns: on a guest that read `no vulkan device opened on this host`, with
/// three other cells tried and unnamed.
///
/// Shared, because the open runs on the blocking pool and the pump reads it
/// after. The same shape as `gpu_hint::opening_on`.
#[derive(Clone, Default)]
#[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
pub(crate) struct OpenTrail(Arc<Mutex<Vec<(String, String)>>>);

#[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
impl OpenTrail {
    /// Until the guard drops, keep what [`note_candidate_failed`] says on this
    /// thread. It drops on a panic too, so an open that dies in a driver still
    /// hands back what it tried, and this pool thread stops recording.
    pub(crate) fn recording(&self) -> Recording<'_> {
        RECORDING.with(|r| *r.borrow_mut() = Some(Vec::new()));
        Recording(self)
    }

    pub(crate) fn take(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// [`OpenTrail::recording`]'s guard.
#[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
pub(crate) struct Recording<'a>(&'a OpenTrail);

impl Drop for Recording<'_> {
    fn drop(&mut self) {
        let failed = RECORDING
            .with(|r| r.borrow_mut().take())
            .unwrap_or_default();
        *self.0.0.lock().unwrap_or_else(|e| e.into_inner()) = failed;
    }
}

/// A candidate failed to open. Does nothing unless a pump is listening, so the
/// probe's opens and a recording's cost one thread-local read.
#[cfg_attr(not(feature = "ffmpeg-encoder"), allow(dead_code))]
pub(crate) fn note_candidate_failed(name: &str, error: &dyn std::fmt::Display) {
    RECORDING.with(|r| {
        if let Some(failed) = r.borrow_mut().as_mut() {
            failed.push((name.to_string(), error.to_string()));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan::new(Some("data-channel-h264"), "h264")
    }

    fn cap() -> Capture {
        Capture {
            arm: "dxgi",
            form: "cpu",
        }
    }

    fn failed(names: &[&str]) -> Vec<(String, String)> {
        names
            .iter()
            .map(|n| (n.to_string(), format!("{n} would not open")))
            .collect()
    }

    /// The wire is a contract with viewers in the field: the first message has
    /// only what is known, and nothing absent is spelled `null`.
    #[test]
    fn the_start_of_a_plan_says_only_what_it_knows() {
        assert_eq!(
            serde_json::to_string(&plan()).unwrap(),
            r#"{"t":"rc:media-plan","epoch":1,"transport":"data-channel-h264","codec":"h264","ladder":[]}"#
        );
    }

    /// `chosen_codec` is the RTP track's pick. On a data-channel session it can
    /// name another codec, and the plan must say the one the transport carries.
    #[test]
    fn a_data_channel_names_its_own_codec_and_rtp_keeps_the_pick() {
        for (transport, codec) in [
            ("data-channel-hevc", "h265"),
            ("data-channel-av1", "av1"),
            ("data-channel-h264", "h264"),
            ("data-channel-vp9-444", "vp9"),
        ] {
            let p = Plan::new(Some(transport), "av1-not-this");
            assert_eq!((p.transport.as_str(), p.codec.as_str()), (transport, codec));
        }
        let p = Plan::new(None, "av1");
        assert_eq!((p.transport.as_str(), p.codec.as_str()), ("webrtc", "av1"));
    }

    /// A data channel whose pump is not there falls to the track. The plan says
    /// so, or the page goes on decoding a channel nothing will arrive on.
    #[test]
    fn a_session_that_fell_to_the_track_says_so_and_one_on_it_says_nothing() {
        let mut p = Plan::new(Some("data-channel-hevc"), "h264");
        p.on_rtp_track("h264", 5);
        assert_eq!((p.transport.as_str(), p.codec.as_str()), ("webrtc", "h264"));
        assert_eq!(p.ladder.len(), 1);
        assert_eq!(p.ladder[0].from, "data-channel-hevc");
        assert_eq!(p.ladder[0].to, "webrtc");

        let mut p = Plan::new(None, "h264");
        p.on_rtp_track("h264", 5);
        assert!(p.ladder.is_empty());
    }

    /// §7.5: the next hardware cell is an arrow, with the reason the one before
    /// it lost, and the winner is the end of the last arrow.
    #[test]
    fn the_next_cell_is_an_arrow_with_the_reason_the_last_one_lost() {
        let mut p = plan();
        p.encoder_opened(
            "h264_qsv",
            false,
            cap(),
            &failed(&["h264_nvenc", "h264_amf"]),
            40,
        );
        assert_eq!(p.cell.as_deref(), Some("h264_qsv:yuv420"));
        assert_eq!(p.chroma, Some("yuv420"));
        let arrows: Vec<_> = p
            .ladder
            .iter()
            .map(|s| (s.from.as_str(), s.to.as_str(), s.why.as_str(), s.at_ms))
            .collect();
        assert_eq!(
            arrows,
            [
                ("h264_nvenc", "h264_amf", "h264_nvenc would not open", 40),
                ("h264_amf", "h264_qsv", "h264_amf would not open", 40),
            ]
        );
    }

    /// The black page's reason. Every cell tried is on the ladder, the last one
    /// goes to `none`, and `stopped` carries the words the host's log has.
    #[test]
    fn nothing_opening_says_stopped_and_names_every_cell_it_tried() {
        let mut p = plan();
        p.no_encoder(
            "no vulkan device opened on this host",
            &failed(&["h264_nvenc", "h264_vulkan"]),
            7,
        );
        let s = p.stopped.as_ref().expect("stopped");
        assert_eq!(s.why, "no_encoder");
        assert_eq!(s.detail, "no vulkan device opened on this host");
        assert_eq!(p.ladder[1].from, "h264_vulkan");
        assert_eq!(p.ladder[1].to, "none");
        assert!(p.cell.is_none());
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            json.contains(r#""stopped":{"why":"no_encoder","detail":"#),
            "{json}"
        );
    }

    /// An exit after frames names the cell it left.
    #[test]
    fn a_running_encoder_that_fails_past_its_rebuilds_says_which() {
        let mut p = plan();
        p.encoder_opened("h264_qsv", false, cap(), &[], 1);
        p.input_form(false, "switch_off", 2);
        p.encoder_fault("the driver went away", 900);
        assert_eq!(p.stopped.as_ref().unwrap().why, "encoder_fault");
        let last = p.ladder.last().unwrap();
        assert_eq!(last.from, "h264_qsv:yuv420");
        assert_eq!(last.to, "none");
        assert_eq!(last.at_ms, 900);
        assert!(p.cell.is_none());
        assert!(p.input.is_none());
    }

    /// A later open succeeds: the old reason must not stay on the page.
    #[test]
    fn an_encoder_that_opens_clears_a_stop() {
        let mut p = plan();
        p.no_encoder("busy", &[], 1);
        p.encoder_opened("h264_qsv", false, cap(), &[], 2);
        assert!(p.stopped.is_none());
    }

    /// A rebuild that lands on another cell is an arrow. The same cell again,
    /// which every dims change is, is not.
    #[test]
    fn a_rebuild_is_an_arrow_only_when_the_cell_moved() {
        let mut p = plan();
        p.encoder_opened("h264_nvenc", false, cap(), &[], 1);
        p.encoder_opened("h264_nvenc", false, cap(), &[], 2);
        assert!(p.ladder.is_empty());
        p.encoder_opened("h264_qsv", false, cap(), &[], 3);
        assert_eq!(p.ladder[0].from, "h264_nvenc:yuv420");
        assert_eq!(p.ladder[0].to, "h264_qsv:yuv420");
    }

    /// §7.5: GPU form to CPU form with its reason. A session that never had a
    /// GPU form to lose has no arrow: `switch_off` is the fleet's default.
    #[test]
    fn a_fall_to_the_cpu_form_is_an_arrow_and_where_it_stands_is_not() {
        for reason in ["switch_off", "no_gpu_cell"] {
            let mut p = plan();
            p.input_form(false, reason, 1);
            assert!(p.ladder.is_empty(), "{reason}");
            assert_eq!(p.input.as_ref().unwrap().reason, reason);
        }
        for reason in [
            "cpu_frames",
            "denied",
            "unproven",
            "adapter_mismatch",
            "open_failed",
        ] {
            let mut p = plan();
            p.input_form(false, reason, 1);
            assert_eq!(p.ladder.len(), 1, "{reason}");
            assert_eq!(p.ladder[0].why, reason);
        }
        // Mid-session: a GPU form lost, for any reason, and back.
        let mut p = plan();
        p.input_form(true, "", 1);
        assert!(p.ladder.is_empty());
        p.input_form(false, "cpu_frames", 2);
        p.input_form(false, "cpu_frames", 3);
        assert_eq!(p.ladder.len(), 1);
        p.input_form(true, "", 4);
        assert_eq!(p.ladder[1].from, "cpu");
        assert_eq!(p.ladder[1].to, "gpu");
    }

    /// §7.5: asked to granted. The same refusal told again is one arrow, and a
    /// granted ask none.
    #[test]
    fn a_refused_ask_is_one_arrow_with_its_reason() {
        let mut p = plan();
        p.fps_answer(60, None, 1);
        assert!(p.ladder.is_empty());
        assert_eq!(p.fps.as_ref().unwrap().asked, None);
        p.fps_answer(120, Some((144, "refresh")), 2);
        p.fps_answer(120, Some((144, "refresh")), 3);
        assert_eq!(p.ladder.len(), 1);
        assert_eq!(p.ladder[0].from, "144 fps");
        assert_eq!(p.ladder[0].to, "120 fps");
        assert_eq!(p.ladder[0].why, "refresh");
        let f = p.fps.as_ref().unwrap();
        assert_eq!(
            (f.asked, f.granted, f.reason),
            (Some(144), 120, Some("refresh"))
        );
        // Another reason for the same pair is news.
        p.fps_answer(120, Some((144, "pipe")), 4);
        assert_eq!(p.ladder.len(), 2);
    }

    /// A backend's words on a page: one line, bounded, whatever it wrote.
    #[test]
    fn a_backends_words_are_one_bounded_line() {
        let mut p = plan();
        p.no_encoder(&format!("line one\nline two\t{}", "x".repeat(400)), &[], 1);
        let d = &p.stopped.as_ref().unwrap().detail;
        assert_eq!(d.chars().count(), MAX_WHY);
        assert!(d.starts_with("line one line two "));
    }

    /// A session that keeps rebuilding must not grow a control message.
    #[test]
    fn the_ladder_is_bounded() {
        let mut p = plan();
        for i in 0..200 {
            p.step("a", "b", "c", i);
        }
        assert_eq!(p.ladder.len(), MAX_LADDER);
    }

    /// Only a real change reaches the wire, because a heartbeat reports the
    /// input form every window and each message costs a viewer a re-render.
    #[test]
    fn the_same_report_again_does_not_raise_the_epoch() {
        let s = SessionPlan {
            started: Instant::now(),
            plan: Mutex::new(plan()),
            changed: tokio::sync::Notify::new(),
        };
        s.update(|p, at| p.input_form(false, "switch_off", at));
        assert_eq!(s.snapshot().0, 2);
        s.update(|p, at| p.input_form(false, "switch_off", at));
        s.update(|p, at| p.input_form(false, "switch_off", at));
        assert_eq!(s.snapshot().0, 2);
        s.update(|p, at| p.fps_answer(60, None, at));
        assert_eq!(s.snapshot().0, 3);
    }

    /// The writers' side of the registry: a session the switch left out has no
    /// plan, and a pump's report on it does nothing rather than start one.
    #[test]
    fn a_report_on_a_session_with_no_plan_does_nothing() {
        let id = ObjectId::new();
        fps_answer(id, 60, None);
        no_encoder(id, "x", &[]);
        assert!(registry().get(&id).is_none());
    }

    /// The trail: what failed on this thread during the open, and only that.
    #[test]
    fn the_trail_keeps_what_failed_during_its_own_open() {
        let e = anyhow::anyhow!("the first one");
        // Nobody is listening: a probe's open costs nothing and leaves nothing.
        note_candidate_failed("h264_nvenc", &e);
        let trail = OpenTrail::default();
        {
            let _recording = trail.recording();
            note_candidate_failed("h264_qsv", &e);
            note_candidate_failed("h264_amf", &"the second");
        }
        assert_eq!(
            trail.take(),
            [
                ("h264_qsv".to_string(), "the first one".to_string()),
                ("h264_amf".to_string(), "the second".to_string()),
            ]
        );
        assert!(trail.take().is_empty());
        // The open is over, so this thread is not recording any more.
        note_candidate_failed("h264_vulkan", &e);
        assert!(trail.take().is_empty());
    }

    /// An open that panics still hands back what it tried, and stops recording.
    #[test]
    fn the_trail_survives_a_panicking_open() {
        let trail = OpenTrail::default();
        let t = trail.clone();
        let hit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _recording = t.recording();
            note_candidate_failed("h264_qsv", &"before the panic");
            panic!("inside the driver");
        }));
        assert!(hit.is_err());
        assert_eq!(trail.take().len(), 1);
        note_candidate_failed("h264_amf", &"after");
        assert!(trail.take().is_empty());
    }

    /// On by default. Its off is read on a guest and not here: this switch gates
    /// a verb, and `the_daemon_assigns_every_config_list` compares two reads of
    /// the verb list on another test thread.
    #[test]
    fn the_switch_is_on_by_default() {
        let _saved = tunnel_core::env::test_env::Saved::cleared("MEDIA_PLAN");
        assert!(enabled());
    }

    /// A plan lives as long as its session: no channel ever opened here, so the
    /// emitter is still in its retry when the session ends, and it must go.
    #[tokio::test]
    async fn a_session_that_ends_leaves_no_plan_behind() {
        let id = ObjectId::new();
        let end = CancellationToken::new();
        start(
            id,
            Some("data-channel-h264"),
            "h264",
            Arc::new(tokio::sync::Mutex::new(None)),
            end.clone(),
        );
        assert!(registry().get(&id).is_some());
        fps_answer(id, 60, None);
        end.cancel();
        for _ in 0..200 {
            if registry().get(&id).is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the plan outlived its session");
    }
}

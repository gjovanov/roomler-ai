// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-85 — `roomlerd record`, the recorder as a process of its own.
//!
//! Spawned by the worker (P2/P3) and runnable by hand. A driver fault inside a
//! recording's encoder costs this process, never the daemon or a live session
//! — the same reason the capability probe is a child.
//!
//! **Protocol.** stdout carries one JSON object per line: the
//! [`RecorderEvent`]s (`started`, `progress`, `stopped`) plus `refused` (it
//! never started, with a closed `code`) and `error`. stdin takes JSON lines:
//! `{"cmd":"stop"}` (optionally `"reason":"host_stopped"`); **end of stdin
//! stops the recording** (`parent_gone`), so a recorder never outlives the
//! process that launched it. Ctrl+C stops it too. Every stop finalizes.
//!
//! ⚠️ The encoder cells denylist is read by the FFmpeg constructors through
//! `node_env("ENCODER_CELLS_DENY")`. A child spawned by the daemon receives it
//! as env (`config_fallbacks_for_child`); run by hand, [`register_encoder_fallbacks`]
//! reads it from the config file — a gate the probe and the live session honour
//! must be honoured here too, or it is a courtesy.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};

use super::folder;
use super::recorder::{self, EncoderFactory, RecordOptions, RecorderEvent, StartError};
use super::sidecar::{Initiator, StopReason};

/// Command-line options of `roomlerd record`.
#[derive(Debug, Clone)]
pub struct RecordArgs {
    /// Destination folder; `None` = the configured or default folder.
    pub out: Option<PathBuf>,
    pub fps: u32,
    /// `auto` | `hardware` | `software`.
    pub encoder: String,
    pub max_minutes: u32,
    /// A remote recording's controller (P3 passes it); `None` = local.
    pub remote_user_id: Option<String>,
    pub remote_user_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StdinCommand {
    cmd: String,
    #[serde(default)]
    reason: Option<String>,
}

/// Ask the recorder to stop. The FIRST reason wins: a parent that sends
/// `stop` and then closes stdin must get `requested`, not `parent_gone`.
pub fn request_stop(tx: &watch::Sender<Option<StopReason>>, reason: StopReason) {
    tx.send_if_modified(|v| {
        if v.is_none() {
            *v = Some(reason);
            true
        } else {
            false
        }
    });
}

/// Map a stdin stop reason to the closed set. Unknown words are a plain
/// `requested` stop — a newer parent must not be able to wedge an older child.
fn stop_reason_from(word: Option<&str>) -> StopReason {
    match word.unwrap_or("requested") {
        "host_stopped" => StopReason::HostStopped,
        "session_ended" => StopReason::SessionEnded,
        "session_changed" => StopReason::SessionChanged,
        "gate_revoked" => StopReason::GateRevoked,
        _ => StopReason::Requested,
    }
}

/// Register the encoder knobs the record child reads from config, exactly the
/// keys `main.rs` registers for the daemon's own encoders (`ENCODER_CELLS_DENY`
/// and the pinned hardware devices). An env var set by a parent still wins —
/// that is `node_env`'s precedence.
pub fn register_encoder_fallbacks(cfg: &roomler_node_core::config::AgentConfig) {
    let mut m: HashMap<String, String> = HashMap::new();
    if let Some(v) = &cfg.encoder_cells_deny {
        m.insert("ENCODER_CELLS_DENY".into(), v.clone());
    }
    if let Some(v) = &cfg.vaapi_device {
        m.insert("VAAPI_DEVICE".into(), v.clone());
    }
    if let Some(v) = &cfg.d3d12_adapter {
        m.insert("D3D12_ADAPTER".into(), v.clone());
    }
    if let Some(v) = &cfg.vulkan_device {
        m.insert("VULKAN_DEVICE".into(), v.clone());
    }
    if !m.is_empty() {
        tunnel_core::env::register_config_fallbacks(m);
    }
}

/// Every protocol line starts with this, like the probe children's
/// `ROOMLER_CAPS_JSON:` — the daemon's tracing writes to stdout too, so a
/// parent that parsed every line would choke on the first log line.
pub const EVENT_PREFIX: &str = "ROOMLER_REC_JSON:";

fn emit(value: &serde_json::Value) {
    let mut out = std::io::stdout().lock();
    // EPIPE (the parent went away) must not abort the finalize that follows.
    let _ = writeln!(out, "{EVENT_PREFIX}{value}");
    let _ = out.flush();
}

/// Parse one stdout line of a record child: `Some(event)` for a protocol
/// line, `None` for anything else (log lines).
pub fn parse_event_line(line: &str) -> Option<serde_json::Value> {
    serde_json::from_str(line.trim_end().strip_prefix(EVENT_PREFIX)?).ok()
}

/// The recording encoder for this run. Software is openh264's recording
/// profile (it keeps its own GOP); anything else goes through the same H.264
/// cascade as a live session — denylist included — with the recorder forcing
/// a keyframe every GOP.
fn encoder_factory(pref: &str, fps: u32, gop_seconds: u32) -> Result<(EncoderFactory, bool)> {
    match pref {
        "software" => {
            #[cfg(feature = "openh264-encoder")]
            {
                let f: EncoderFactory = Box::new(move |w, h| {
                    let e = crate::encode::openh264_backend::Openh264Encoder::new_recording(
                        w,
                        h,
                        fps,
                        fps * gop_seconds,
                    )?;
                    Ok(Box::new(e) as Box<dyn crate::encode::VideoEncoder>)
                });
                Ok((f, true))
            }
            #[cfg(not(feature = "openh264-encoder"))]
            {
                let _ = (fps, gop_seconds);
                bail!("this build has no software H.264 encoder (openh264-encoder)");
            }
        }
        "auto" | "hardware" => {
            let p = if pref == "hardware" {
                crate::encode::EncoderPreference::Hardware
            } else {
                crate::encode::EncoderPreference::Auto
            };
            let f: EncoderFactory = Box::new(move |w, h| {
                let (e, codec) = crate::encode::open_for_codec("h264", w, h, p);
                if codec != "h264" {
                    bail!("the H.264 cascade returned {codec}");
                }
                Ok(e)
            });
            Ok((f, false))
        }
        other => bail!("unknown --encoder {other:?} (auto | hardware | software)"),
    }
}

/// Body of `roomlerd record`. Returns once the file is final.
pub async fn run(args: RecordArgs, config_path: &std::path::Path) -> Result<()> {
    if let Ok(cfg) = roomler_node_core::config::load(&config_path.to_path_buf()) {
        register_encoder_fallbacks(&cfg);
    }

    let choice = match &args.out {
        Some(dir) => {
            if let Err(e) = folder::validate_record_dir(&dir.to_string_lossy()) {
                emit(
                    &serde_json::json!({"ev": "refused", "code": "folder_unwritable", "detail": e}),
                );
                bail!("{e}");
            }
            folder::resolve(Some(dir))
        }
        None => folder::resolve(None),
    };
    if let Some(reason) = &choice.reason {
        tracing::info!(folder = %choice.dir.display(), %reason, "recording: folder fallback");
        emit(&serde_json::json!({"ev": "folder", "path": choice.dir, "reason": reason}));
    }

    let initiator = match &args.remote_user_id {
        Some(id) => Initiator::Remote {
            controller_user_id: id.clone(),
            controller_name: args.remote_user_name.clone(),
        },
        None => Initiator::Local {
            user: std::env::var("USERNAME")
                .or_else(|_| std::env::var("USER"))
                .ok(),
        },
    };
    let name = folder::unique_recording_name(&choice.dir, chrono::Local::now());
    let mut opts = RecordOptions::new(choice.dir.clone(), name, initiator);
    opts.fps = args.fps;
    opts.max_duration = Duration::from_secs(u64::from(args.max_minutes.max(1)) * 60);

    let (factory, keeps_gop) = match encoder_factory(&args.encoder, opts.fps, opts.gop_seconds) {
        Ok(v) => v,
        Err(e) => {
            emit(
                &serde_json::json!({"ev": "refused", "code": "encoder_unavailable", "detail": format!("{e:#}")}),
            );
            return Err(e);
        }
    };
    opts.encoder_keeps_gop = keeps_gop;

    // The recorder's own capturer, at native resolution — never the live
    // pump's capped rung.
    let capturer = crate::capture::open_default(opts.fps, crate::capture::DownscalePolicy::Never);

    let (stop_tx, stop_rx) = watch::channel::<Option<StopReason>>(None);
    let stop_tx = Arc::new(stop_tx);
    // stdin: commands, and EOF = the parent is gone.
    //
    // ⚠️ A detached std thread, NOT `tokio::io::stdin()`: tokio reads stdin on
    // a blocking-pool thread, and the runtime waits for its blocking threads
    // at shutdown — so a recorder told to stop while its parent kept stdin
    // open finalized, reported `stopped`, and then never exited (the
    // `the_stop_command_finalizes_and_reports_the_file` test caught it). A
    // detached thread dies with the process.
    {
        let stop_tx = stop_tx.clone();
        std::thread::Builder::new()
            .name("roomlerd-rec-stdin".into())
            .spawn(move || {
                use std::io::BufRead as _;
                for line in std::io::stdin().lock().lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<StdinCommand>(line) {
                        Ok(c) if c.cmd == "stop" => {
                            request_stop(&stop_tx, stop_reason_from(c.reason.as_deref()));
                        }
                        Ok(c) => tracing::warn!(cmd = %c.cmd, "recording: unknown command ignored"),
                        Err(e) => tracing::warn!(%e, "recording: unparseable command ignored"),
                    }
                }
                // EOF or a read error: whoever launched us is gone.
                request_stop(&stop_tx, StopReason::ParentGone);
            })
            .map_err(|e| anyhow::anyhow!("recording: cannot start the stdin reader: {e}"))?;
    }
    {
        let stop_tx = stop_tx.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                request_stop(&stop_tx, StopReason::Requested);
            }
        });
    }

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<RecorderEvent>();
    let printer = tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            if let Ok(v) = serde_json::to_value(&ev) {
                emit(&v);
            }
        }
    });

    let result = recorder::run(opts, capturer, factory, stop_rx, ev_tx).await;
    let _ = printer.await;
    match result {
        Ok(summary) => {
            tracing::info!(reason = summary.reason.as_str(), path = ?summary.path, "recording: finished");
            Ok(())
        }
        Err(e) => {
            let (code, detail) = match e.downcast_ref::<StartError>() {
                Some(se) => (se.refusal.as_str().to_string(), se.detail.clone()),
                None => ("error".to_string(), format!("{e:#}")),
            };
            emit(
                &serde_json::json!({"ev": if code == "error" { "error" } else { "refused" }, "code": code, "detail": detail}),
            );
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_stop_reasons_map_to_the_closed_set() {
        assert_eq!(stop_reason_from(None), StopReason::Requested);
        assert_eq!(
            stop_reason_from(Some("host_stopped")),
            StopReason::HostStopped
        );
        assert_eq!(
            stop_reason_from(Some("gate_revoked")),
            StopReason::GateRevoked
        );
        assert_eq!(
            stop_reason_from(Some("something_from_a_newer_parent")),
            StopReason::Requested
        );
    }

    #[test]
    fn stdin_commands_parse() {
        let c: StdinCommand =
            serde_json::from_str(r#"{"cmd":"stop","reason":"host_stopped"}"#).unwrap();
        assert_eq!(c.cmd, "stop");
        assert_eq!(c.reason.as_deref(), Some("host_stopped"));
        let bare: StdinCommand = serde_json::from_str(r#"{"cmd":"stop"}"#).unwrap();
        assert!(bare.reason.is_none());
    }

    #[test]
    fn an_unknown_encoder_preference_is_refused() {
        assert!(encoder_factory("quantum", 30, 2).is_err());
    }

    #[test]
    fn the_first_stop_reason_wins() {
        let (tx, rx) = watch::channel(None);
        request_stop(&tx, StopReason::Requested);
        request_stop(&tx, StopReason::ParentGone);
        assert_eq!(*rx.borrow(), Some(StopReason::Requested));
    }

    #[test]
    fn only_prefixed_lines_are_protocol() {
        let v = parse_event_line(r#"ROOMLER_REC_JSON:{"ev":"started"}"#).unwrap();
        assert_eq!(v["ev"], "started");
        // A log line on the same stdout is ignored, not an error.
        assert!(
            parse_event_line("2026-09-25T15:00:00Z INFO roomlerd: recording: started").is_none()
        );
        assert!(
            parse_event_line(r#"{"ev":"started"}"#).is_none(),
            "unprefixed JSON is not protocol"
        );
        // CRLF from a Windows child.
        assert!(parse_event_line("ROOMLER_REC_JSON:{\"ev\":\"stopped\"}\r").is_some());
    }
}

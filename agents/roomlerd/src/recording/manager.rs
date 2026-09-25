// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-85 P2a — the daemon's side of a LOCAL recording: it launches
//! `roomlerd record` for the LocalAPI verbs, follows its events, and answers
//! "what is recording, and how did the last one end".
//!
//! The recorder stays a process of its own (a driver fault costs it, never the
//! daemon); this is only its supervisor. One recording at a time. A daemon
//! that goes away closes the child's stdin, which the child reads as
//! `parent_gone` and finalizes — a recording never outlives its launcher.
//!
//! ⚠️ Not yet here (FR-85 P1e): launching the recorder AS the console user.
//! The child inherits the daemon's identity, so a SYSTEM/root daemon refuses
//! a local recording ([`super::identity`]), and a Windows worker running the
//! elevated linked token of a UAC-split admin records elevated rather than
//! at medium integrity.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, watch};
use tunnel_core::localapi::{
    RecordStartOpts, RecordingEnded, RecordingItem, RecordingState, RecordingsListing, Response,
};

use super::child::parse_event_line;
use super::folder;
use super::sidecar::{Initiator, SIDECAR_SUFFIX, Sidecar};

/// How long `start` waits for the child to report `started` or `refused`
/// (the first frame, the encoder open, a folder probe). The child's own
/// first-frame timeout is 10 s, so a healthy refusal always arrives first.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `stop` waits for the file to be final — the remux of a long
/// recording copies every byte once.
const STOP_TIMEOUT: Duration = Duration::from_secs(300);

/// Recorders of this process that reported `started` and have not ended —
/// for the updater's defer gate. A count, not a flag: each recorder's reader
/// task adds itself once and removes itself once, so no path can clear the
/// mark of a recording it does not own.
static RECORDING: AtomicUsize = AtomicUsize::new(0);

/// Is a recording running in this process right now?
pub fn is_recording() -> bool {
    RECORDING.load(Ordering::Relaxed) > 0
}

struct Active {
    stdin: Option<tokio::process::ChildStdin>,
    child: tokio::process::Child,
    /// Flips to `true` when the child reported `stopped`/`refused`/`error`, or
    /// its stdout closed.
    ended: watch::Receiver<bool>,
}

/// The recorder's supervisor. Cheap to share (`Arc`).
pub struct RecordingManager {
    /// The binary to launch — this daemon's own executable in production.
    exe: PathBuf,
    /// Read at every start for `record_dir` (the key is live, no restart).
    config_path: PathBuf,
    state: Arc<StdMutex<RecordingState>>,
    active: Mutex<Option<Active>>,
    /// This daemon runs as SYSTEM/root, where a local recording would land in
    /// the service account's profile — refused until P1e.
    service_identity: bool,
    /// Extra environment for the child, on top of the config fallbacks.
    child_env: Vec<(String, String)>,
    /// [`START_TIMEOUT`], unless a test shortens it.
    start_timeout: Duration,
}

impl RecordingManager {
    pub fn new(exe: PathBuf, config_path: PathBuf) -> Self {
        Self {
            exe,
            config_path,
            state: Arc::new(StdMutex::new(RecordingState::default())),
            active: Mutex::new(None),
            service_identity: super::identity::daemon_is_service_account(),
            child_env: Vec::new(),
            start_timeout: START_TIMEOUT,
        }
    }

    /// Shorten the start deadline — for the test that proves a recorder
    /// missing it is stopped rather than left to start later, unseen.
    pub fn with_start_timeout(mut self, start_timeout: Duration) -> Self {
        self.start_timeout = start_timeout;
        self
    }

    /// Override the identity probe — for tests, which may run as root in a
    /// container and still need to drive the whole path.
    pub fn with_service_identity(mut self, service_identity: bool) -> Self {
        self.service_identity = service_identity;
        self
    }

    /// Extra environment for every recorder this manager launches — how a
    /// test selects the synthetic frame source without `set_var` on its own
    /// (multi-threaded) process.
    pub fn with_child_env<K: Into<String>, V: Into<String>>(
        mut self,
        env: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        self.child_env
            .extend(env.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    fn snapshot(&self) -> RecordingState {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// The configured folder, if any (read fresh — `record_dir` is live).
    fn configured_dir(&self) -> Option<PathBuf> {
        roomler_node_core::config::load(&self.config_path)
            .ok()
            .and_then(|c| c.record_dir)
            .map(PathBuf::from)
    }

    /// Start a recording. Answers once the child has either begun encoding or
    /// refused, so the caller learns the folder, the encoder, or why not.
    pub async fn start(&self, opts: RecordStartOpts) -> Response {
        if self.service_identity {
            return Response::Error {
                message: "this daemon runs as SYSTEM/root, so a recording would be saved in the \
                          service account's profile, not yours; local recording from a service \
                          daemon arrives with FR-85 P1e — until then use `roomlerd record` in \
                          your own session"
                    .into(),
            };
        }
        if opts.system_audio || opts.microphone {
            // P1c. Refusing beats silently recording without the audio the
            // person asked for.
            return Response::Error {
                message: "recording audio is not available yet (FR-85 P1c) — start without it"
                    .into(),
            };
        }
        let mut guard = self.active.lock().await;
        if let Some(a) = guard.as_ref()
            && !*a.ended.borrow()
        {
            return Response::Error {
                message: "a recording is already running".into(),
            };
        }
        // A previous child that ended: reap it before starting the next — but
        // never wait on it unboundedly: one that already reported `stopped`
        // may still be finishing a big reconcile, and dropping the handle is
        // safe (tokio reaps an orphaned child in the background).
        if let Some(mut old) = guard.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), old.child.wait()).await;
        }

        let mut cmd = tokio::process::Command::new(&self.exe);
        cmd.arg("record")
            .arg("--fps")
            .arg(opts.fps.unwrap_or(30).clamp(1, 60).to_string())
            .arg("--encoder")
            .arg(opts.encoder.as_deref().unwrap_or("auto"))
            .arg("--max-minutes")
            .arg(opts.max_minutes.unwrap_or(240).max(1).to_string())
            .arg("--config")
            .arg(&self.config_path);
        if let Some(dir) = self.configured_dir() {
            cmd.arg("--out").arg(dir);
        }
        // The config-backed knobs (the encoder denylist, pinned devices) are
        // process-local here; hand them to the child as real env, exactly as
        // the capability probe does, or the gate would be a courtesy.
        cmd.envs(tunnel_core::env::config_fallbacks_for_child())
            .envs(self.child_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(false);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Response::Error {
                    message: format!("could not launch the recorder: {e}"),
                };
            }
        };
        let stdin = child.stdin.take();
        let Some(stdout) = child.stdout.take() else {
            return Response::Error {
                message: "the recorder has no stdout".into(),
            };
        };

        // A fresh state for this recording; the last one's ending stays.
        if let Ok(mut s) = self.state.lock() {
            let last = s.last.take();
            *s = RecordingState {
                last,
                ..Default::default()
            };
        }
        let (ended_tx, ended_rx) = watch::channel(false);
        let (started_tx, mut started_rx) = watch::channel(false);
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            let mut reported_end = false;
            // This recorder's own mark in RECORDING: added once, removed once.
            let mut counted = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(ev) = parse_event_line(&line) else {
                    continue;
                };
                let mut finished = false;
                if let Ok(mut s) = state.lock() {
                    finished = apply_event(&mut s, &ev);
                }
                if ev["ev"] == "started" && !counted {
                    counted = true;
                    RECORDING.fetch_add(1, Ordering::Relaxed);
                    let _ = started_tx.send(true);
                }
                if finished {
                    reported_end = true;
                    if std::mem::take(&mut counted) {
                        RECORDING.fetch_sub(1, Ordering::Relaxed);
                    }
                    let _ = ended_tx.send(true);
                }
            }
            // stdout closed: whatever it said last, the child is done. One
            // that never said how it ended (a crash, bad arguments, a binary
            // without the recorder, a kill) must not leave the state claiming
            // a recording, nor a start answered with no reason at all.
            if counted {
                RECORDING.fetch_sub(1, Ordering::Relaxed);
            }
            if !reported_end && let Ok(mut s) = state.lock() {
                let detail = if s.active {
                    "the recorder exited without reporting a stop — the file may be \
                     finalized at the next recording (interrupted)"
                } else {
                    "the recorder exited before it started — see the daemon's log"
                };
                s.active = false;
                s.last = Some(RecordingEnded {
                    reason: "recorder_exited".into(),
                    path: s.path.clone(),
                    bytes: s.bytes,
                    duration_ms: s.duration_ms,
                    detail: Some(detail.into()),
                });
            }
            let _ = ended_tx.send(true);
        });

        *guard = Some(Active {
            stdin,
            child,
            ended: ended_rx.clone(),
        });
        drop(guard);

        let mut ended_rx = ended_rx;
        let outcome = tokio::time::timeout(self.start_timeout, async {
            tokio::select! {
                _ = started_rx.wait_for(|s| *s) => {}
                _ = ended_rx.wait_for(|e| *e) => {}
            }
        })
        .await;
        let snap = self.snapshot();
        match outcome {
            Ok(()) if snap.active => Response::Recording(snap),
            Ok(()) => Response::Error {
                message: snap
                    .last
                    .as_ref()
                    .map(|l| {
                        format!(
                            "the recording did not start ({}): {}",
                            l.reason,
                            l.detail.as_deref().unwrap_or("")
                        )
                    })
                    .unwrap_or_else(|| "the recording did not start".into()),
            },
            Err(_) => {
                // ⚠️ Never leave running a recorder whose caller was told it
                // did not start: stuck in an encoder open or a first frame,
                // it could begin recording a minute later, unseen. A stop
                // command would not reach it (the recorder reads its stop
                // only once it is recording), so it is killed; a partial it
                // left is finalized `interrupted` by the next recorder.
                let mut guard = self.active.lock().await;
                if let Some(a) = guard.as_mut() {
                    a.stdin.take();
                    let _ = a.child.start_kill();
                    let mut ended = a.ended.clone();
                    let _ =
                        tokio::time::timeout(Duration::from_secs(5), ended.wait_for(|e| *e)).await;
                }
                // (Its RECORDING mark, if it got that far, went with its
                // reader task when the kill closed its stdout.)
                *guard = None;
                if let Ok(mut s) = self.state.lock() {
                    s.active = false;
                    s.last = Some(RecordingEnded {
                        reason: "start_timeout".into(),
                        path: None,
                        bytes: 0,
                        duration_ms: 0,
                        detail: Some(format!(
                            "the recorder did not start within {} s and was stopped",
                            self.start_timeout.as_secs_f32()
                        )),
                    });
                }
                Response::Error {
                    message: format!(
                        "the recorder did not start within {} s — it was stopped",
                        self.start_timeout.as_secs_f32()
                    ),
                }
            }
        }
    }

    /// Stop the active recording and answer once its file is final.
    pub async fn stop(&self) -> Response {
        let mut guard = self.active.lock().await;
        let Some(active) = guard.as_mut() else {
            return Response::Recording(self.snapshot());
        };
        if !*active.ended.borrow()
            && let Some(stdin) = active.stdin.as_mut()
        {
            let _ = stdin.write_all(b"{\"cmd\":\"stop\"}\n").await;
            let _ = stdin.flush().await;
        }
        let mut ended = active.ended.clone();
        let finished = tokio::time::timeout(STOP_TIMEOUT, ended.wait_for(|e| *e))
            .await
            .is_ok();
        if finished {
            // Close stdin and reap: the child exits after its last event.
            active.stdin.take();
            let _ = tokio::time::timeout(Duration::from_secs(10), active.child.wait()).await;
            *guard = None;
        }
        Response::Recording(self.snapshot())
    }

    pub fn status(&self) -> Response {
        Response::Recording(self.snapshot())
    }

    /// The folder recordings go to, and the finished recordings in it.
    pub async fn list(&self) -> Response {
        let configured = self.configured_dir();
        let choice = tokio::task::spawn_blocking(move || folder::resolve(configured.as_deref()))
            .await
            .unwrap_or_else(|_| folder::FolderChoice {
                dir: std::env::temp_dir(),
                reason: Some("folder resolution failed".into()),
            });
        let dir = choice.dir.clone();
        let items = tokio::task::spawn_blocking(move || list_recordings(&dir))
            .await
            .unwrap_or_default();
        Response::Recordings(RecordingsListing {
            dir: choice.dir.to_string_lossy().into_owned(),
            folder_reason: choice.reason,
            items,
        })
    }

    /// Delete one recording (and its sidecar) by file name.
    pub async fn delete(&self, name: &str) -> Response {
        if let Err(message) = check_recording_name(name) {
            return Response::RecordingDeleted {
                ok: false,
                message: Some(message),
            };
        }
        let dir = match self.list().await {
            Response::Recordings(l) => PathBuf::from(l.dir),
            _ => {
                return Response::RecordingDeleted {
                    ok: false,
                    message: Some("the recordings folder could not be resolved".into()),
                };
            }
        };
        let path = dir.join(name);
        if self.snapshot().active
            && self.snapshot().path.as_deref() == Some(&*path.to_string_lossy())
        {
            return Response::RecordingDeleted {
                ok: false,
                message: Some("that recording is still being written".into()),
            };
        }
        let result = tokio::task::spawn_blocking(move || delete_recording(&path))
            .await
            .unwrap_or_else(|e| Err(format!("delete task: {e}")));
        match result {
            Ok(()) => Response::RecordingDeleted {
                ok: true,
                message: None,
            },
            Err(message) => Response::RecordingDeleted {
                ok: false,
                message: Some(message),
            },
        }
    }
}

/// Fold one child event into the state. Returns `true` when the recording is
/// over (it stopped, or never started).
fn apply_event(s: &mut RecordingState, ev: &serde_json::Value) -> bool {
    let str_of = |k: &str| ev[k].as_str().map(str::to_string);
    let u64_of = |k: &str| ev[k].as_u64().unwrap_or(0);
    match ev["ev"].as_str() {
        Some("folder") => {
            s.folder_reason = str_of("reason");
            false
        }
        Some("started") => {
            s.active = true;
            s.path = str_of("path");
            s.encoder = str_of("encoder");
            s.width = u64_of("width") as u32;
            s.height = u64_of("height") as u32;
            s.fps = u64_of("fps") as u32;
            s.started_at_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
            false
        }
        Some("progress") => {
            s.duration_ms = u64_of("duration_ms");
            s.bytes = u64_of("bytes");
            s.frames = u64_of("frames");
            false
        }
        Some("stopped") => {
            s.active = false;
            s.duration_ms = u64_of("duration_ms");
            s.bytes = u64_of("bytes");
            s.frames = u64_of("frames");
            s.last = Some(RecordingEnded {
                reason: str_of("reason").unwrap_or_else(|| "requested".into()),
                path: str_of("path"),
                bytes: u64_of("bytes"),
                duration_ms: u64_of("duration_ms"),
                detail: ev["fragmented"]
                    .as_bool()
                    .filter(|f| *f)
                    .map(|_| "kept as fragmented MP4 (the remux failed)".into()),
            });
            true
        }
        Some("refused") | Some("error") => {
            s.active = false;
            s.last = Some(RecordingEnded {
                reason: str_of("code").unwrap_or_else(|| "error".into()),
                path: None,
                bytes: 0,
                duration_ms: 0,
                detail: str_of("detail"),
            });
            true
        }
        _ => false,
    }
}

/// A recording's file name, from a client: a bare `*.mp4` name, nothing that
/// could leave the folder.
fn check_recording_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.contains(['/', '\\', ':'])
        || name.contains("..")
        || !name.to_ascii_lowercase().ends_with(".mp4")
    {
        return Err(format!("{name:?} is not a recording's file name"));
    }
    Ok(())
}

fn delete_recording(path: &Path) -> Result<(), String> {
    // Never follow a link out of the folder: a name that is a symlink is not
    // a recording this device made.
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    std::fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let _ = std::fs::remove_file(Sidecar::path_for(path));
    Ok(())
}

/// The finished recordings in `dir` (newest first), described by their
/// sidecars where there is one.
pub fn list_recordings(dir: &Path) -> Vec<RecordingItem> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut items: Vec<(std::time::SystemTime, RecordingItem)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_string();
            if !name.to_ascii_lowercase().ends_with(".mp4") || name.ends_with(SIDECAR_SUFFIX) {
                return None;
            }
            let meta = std::fs::symlink_metadata(&path).ok()?;
            if !meta.file_type().is_file() {
                return None;
            }
            let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            let sc: Option<Sidecar> = std::fs::read_to_string(Sidecar::path_for(&path))
                .ok()
                .and_then(|j| serde_json::from_str(&j).ok());
            let (origin, controller) = match sc.as_ref().map(|s| &s.initiator) {
                Some(Initiator::Remote {
                    controller_user_id,
                    controller_name,
                }) => (
                    "remote".to_string(),
                    Some(
                        controller_name
                            .clone()
                            .unwrap_or_else(|| controller_user_id.clone()),
                    ),
                ),
                _ => ("local".to_string(), None),
            };
            Some((
                modified,
                RecordingItem {
                    name,
                    bytes: meta.len(),
                    duration_ms: sc.as_ref().map(|s| s.duration_ms).unwrap_or(0),
                    started_at: sc
                        .as_ref()
                        .map(|s| s.started_at.clone())
                        .unwrap_or_default(),
                    origin,
                    controller,
                    stop_reason: sc
                        .as_ref()
                        .and_then(|s| s.stop_reason)
                        .map(|r| r.as_str().to_string()),
                    width: sc.as_ref().map(|s| s.width).unwrap_or(0),
                    height: sc.as_ref().map(|s| s.height).unwrap_or(0),
                },
            ))
        })
        .collect();
    items.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    items.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_fold_into_the_state_and_mark_the_end() {
        let mut s = RecordingState::default();
        assert!(!apply_event(
            &mut s,
            &json!({"ev":"folder","path":"x","reason":"OneDrive"})
        ));
        assert_eq!(s.folder_reason.as_deref(), Some("OneDrive"));
        assert!(!apply_event(
            &mut s,
            &json!({"ev":"started","path":"C:\\r.mp4","encoder":"h264_nvenc","width":1920,"height":1080,"fps":30})
        ));
        assert!(s.active && s.encoder.as_deref() == Some("h264_nvenc") && s.width == 1920);
        assert!(!apply_event(
            &mut s,
            &json!({"ev":"progress","duration_ms":1000,"bytes":5,"frames":30})
        ));
        assert_eq!((s.duration_ms, s.bytes, s.frames), (1000, 5, 30));
        assert!(apply_event(
            &mut s,
            &json!({"ev":"stopped","reason":"disk_low","path":"C:\\r.mp4","bytes":9,"duration_ms":2000,"frames":60,"fragmented":false})
        ));
        assert!(!s.active);
        let last = s.last.clone().unwrap();
        assert_eq!((last.reason.as_str(), last.bytes), ("disk_low", 9));
        assert!(last.detail.is_none());
    }

    #[test]
    fn a_refusal_is_an_ending_with_its_code() {
        let mut s = RecordingState::default();
        assert!(apply_event(
            &mut s,
            &json!({"ev":"refused","code":"encoder_unavailable","detail":"no GPU"})
        ));
        let last = s.last.unwrap();
        assert_eq!(last.reason, "encoder_unavailable");
        assert_eq!(last.detail.as_deref(), Some("no GPU"));
    }

    #[test]
    fn only_a_bare_mp4_name_can_be_deleted() {
        assert!(check_recording_name("Roomler Recording 2026-09-25 14-30-12.mp4").is_ok());
        for bad in [
            "",
            "../x.mp4",
            "a/b.mp4",
            r"a\b.mp4",
            "C:x.mp4",
            "notes.txt",
            "x.mp4.roomler.json",
        ] {
            assert!(check_recording_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_listing_reads_sidecars_newest_first_and_skips_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mp4");
        std::fs::write(&a, vec![0u8; 10]).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let b = dir.path().join("b.mp4");
        std::fs::write(&b, vec![0u8; 20]).unwrap();
        let sc = Sidecar {
            version: 1,
            file: "b.mp4".into(),
            initiator: Initiator::Remote {
                controller_user_id: "66f0".into(),
                controller_name: Some("GJ".into()),
            },
            started_at: "2026-09-25T12:00:00Z".into(),
            ended_at: None,
            duration_ms: 5000,
            width: 1920,
            height: 1080,
            fps: 30,
            codec: "h264".into(),
            encoder: "openh264".into(),
            color: super::super::mp4::ColorInfo::BT601_LIMITED,
            audio: Default::default(),
            frames: 150,
            late_ticks: 0,
            events: Vec::new(),
            stop_reason: Some(super::super::sidecar::StopReason::Requested),
            bytes: 20,
        };
        std::fs::write(Sidecar::path_for(&b), sc.to_json()).unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        std::fs::create_dir(dir.path().join(".roomler-partial")).unwrap();
        let items = list_recordings(dir.path());
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["b.mp4", "a.mp4"]);
        assert_eq!(items[0].origin, "remote");
        assert_eq!(items[0].controller.as_deref(), Some("GJ"));
        assert_eq!(items[0].duration_ms, 5000);
        assert_eq!(items[0].stop_reason.as_deref(), Some("requested"));
        assert_eq!(items[1].origin, "local", "no sidecar reads as local");
    }

    #[test]
    fn delete_refuses_a_link_and_removes_the_sidecar_with_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("r.mp4");
        std::fs::write(&f, b"x").unwrap();
        std::fs::write(Sidecar::path_for(&f), b"{}").unwrap();
        delete_recording(&f).unwrap();
        assert!(!f.exists() && !Sidecar::path_for(&f).exists());
        assert!(delete_recording(&dir.path().join("missing.mp4")).is_err());
        std::fs::create_dir(dir.path().join("d.mp4")).unwrap();
        assert!(
            delete_recording(&dir.path().join("d.mp4")).is_err(),
            "a directory is not a recording"
        );
    }
}

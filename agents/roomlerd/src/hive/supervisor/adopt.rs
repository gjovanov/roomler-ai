// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P1j-2 — terminal sessions this device's people adopted (`roomler
//! hive adopt`), mirrored into the store and offered to the server for a
//! record their owner alone reads (decision 11, spec §3h).
//!
//! Claude Code runs a person's hooks as that person. Each hook connects to
//! the adopt socket (`<runtime>/adopt.sock`, the protocol in
//! `tunnel_core::localapi::hive_adopt`) and says which terminal session it
//! is. The daemon answers with where its copy of that session's transcript
//! ends, and the hook sends the rest, read as the person.
//!
//! | rule | where |
//! |---|---|
//! | the socket exists only while the device's owner allows adopting (`hive_adopt`) | [`Supervisor::adopt_listen`] |
//! | who the peer is comes from the kernel (`SO_PEERCRED` / `getpeereid`), never from the hook; root is refused | [`Supervisor::adopt_conn`] |
//! | the account's `hive_accounts` keys, every one of them, go to the server, which adopts only for ONE person | [`Supervisor::adopt_keys`] |
//! | the daemon never opens a path a hook names; it takes the lines the hook read | [`Supervisor::adopt_lines`] |
//! | a terminal session belongs to the account that first offered it; another account's lines are refused | [`Supervisor::adopt_lines`] |
//! | nothing is taken for a session stopped from Roomler, or ended | [`Supervisor::adopt_stop`] |
//! | a terminal killed outright is found by its process (pid + start time) and ended | [`Supervisor::adopt_sweep`] |
//! | an ended session is forgotten as a live one (a resume is offered afresh), and stays its owner's to read | [`AdoptedFile::retire`] |
//!
//! ⚠️ An adopted session takes no prompt: it has no input channel here at
//! all. The terminal holds the harness; the server's grant says
//! `may_prompt: false`, and [`Supervisor::prompt`] refuses a session it does
//! not run anyway.
//!
//! P1i-2 — nothing is adopted on Windows (the hooks CLI refuses there, and
//! `hive_adopt` reads off): the socket and the connections it takes are
//! compiled out, and what only they reach is unused there.

#![cfg_attr(windows, allow(dead_code))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bson::oid::ObjectId;
use roomler_ai_remote_control::hive::{HiveAdoptRefusal, HiveRunState, HiveTurnStatus};
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_hive_node::TranscriptEvent;
use roomler_hive_node::stream_json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(unix)]
use tokio::io::{AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};
use tunnel_core::localapi::hive_adopt::{self as proto, Reply, refusal};
#[cfg(unix)]
use tunnel_core::localapi::hive_adopt::{HookEvent, Request};

use super::Supervisor;
#[cfg(unix)]
use crate::hive::lines::LineReader;
use crate::hive::procs;

/// The file beside `hosted.json` that keeps what this device mirrors.
pub(super) const ADOPTED_FILE: &str = "adopted.json";
const VERSION: u32 = 1;
/// How long a new terminal session's hook waits for the server's word. The
/// hooks run in the background (`async`), so nobody at the terminal waits.
const OFFER_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the daemon looks for terminals that closed without a word.
const SWEEP_EVERY: Duration = Duration::from_secs(60);
/// The largest request taken from a hook: a chunk of whole lines, escaped.
const MAX_REQUEST: usize = 8 * proto::MAX_LINE;
/// Adopted sessions one device mirrors at once; the server holds the same
/// bound (`hive_limits::MAX_ADOPTED_PER_DEVICE`).
const MAX_ADOPTED: usize = 16;
/// Ended adopted sessions the device still serves to their owner's viewer
/// with agent sessions off (`hive_enabled`); the oldest falls off.
const MAX_ENDED: usize = 1000;

/// One adopted terminal session, as this device keeps it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct Adopted {
    /// Claude Code's own session id (a UUID).
    pub harness_session: String,
    /// The server's record (hex), and the fence it is reported under.
    pub session: String,
    pub fence: u64,
    /// The account whose terminal it is, and its uid: the only peer whose
    /// lines are taken for it.
    pub account: String,
    pub uid: u32,
    /// Where this copy of the transcript ends, in bytes of the source file.
    pub offset: u64,
    /// Turns begun.
    pub turns: u32,
    #[serde(default)]
    pub running: Option<Running>,
    /// Claude Code's process, by pid and start time ([`procs::started`]).
    #[serde(default)]
    pub terminal_pid: Option<u32>,
    #[serde(default)]
    pub terminal_started: Option<String>,
    /// Stopped from Roomler: nothing more is taken for it.
    #[serde(default)]
    pub stopped: bool,
}

/// The turn in progress.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct Running {
    pub turn: u32,
    pub steps: u32,
    /// When it began, in milliseconds since the epoch.
    pub began_ms: u64,
    /// Claude Code's own measure (`system/turn_duration`), when it gave one.
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct Doc {
    v: u32,
    agent: String,
    sessions: Vec<Adopted>,
    /// The records of adopted sessions that ended here, oldest first.
    #[serde(default)]
    ended: Vec<String>,
}

/// The adopted sessions and the file that keeps them, `0600`, written whole.
#[derive(Debug, Default)]
pub(crate) struct AdoptedFile {
    path: Option<PathBuf>,
    agent: String,
    sessions: Vec<Adopted>,
    ended: Vec<String>,
}

impl AdoptedFile {
    /// What `path` holds for the enrollment `agent`. Never fails: a file that
    /// cannot be read, belongs to another enrollment or is from a newer
    /// daemon mirrors nothing, and the next write replaces it.
    pub(crate) fn load(path: Option<PathBuf>, agent: &str) -> Self {
        let (sessions, ended) = match path.as_ref().map(std::fs::read) {
            Some(Ok(bytes)) => match serde_json::from_slice::<Doc>(&bytes) {
                Ok(doc) if doc.v <= VERSION && doc.agent == agent => (doc.sessions, doc.ended),
                Ok(_) => Default::default(),
                Err(e) => {
                    warn!(%e, "hive: the adopted sessions could not be read — none is kept");
                    Default::default()
                }
            },
            _ => Default::default(),
        };
        Self {
            path,
            agent: agent.to_string(),
            sessions,
            ended,
        }
    }

    pub(crate) fn get(&self, harness_session: &str) -> Option<&Adopted> {
        self.sessions
            .iter()
            .find(|a| a.harness_session == harness_session)
    }

    fn by_session(&self, session: &str) -> Option<&Adopted> {
        self.sessions.iter().find(|a| a.session == session)
    }

    fn live(&self) -> impl Iterator<Item = &Adopted> {
        self.sessions.iter().filter(|a| !a.stopped)
    }

    /// Whether `session` is, or was, an adopted session of this device.
    fn was_adopted(&self, session: &str) -> bool {
        self.by_session(session).is_some() || self.ended.iter().any(|s| s == session)
    }

    fn put(&mut self, a: Adopted) {
        match self
            .sessions
            .iter_mut()
            .find(|e| e.harness_session == a.harness_session)
        {
            Some(e) => *e = a,
            None => self.sessions.push(a),
        }
        self.save();
    }

    fn update(&mut self, harness_session: &str, f: impl FnOnce(&mut Adopted)) -> Option<Adopted> {
        let e = self
            .sessions
            .iter_mut()
            .find(|e| e.harness_session == harness_session)?;
        f(e);
        let out = e.clone();
        self.save();
        Some(out)
    }

    /// A terminal session that ended: forgotten as a live one, so a `claude
    /// --resume` later is offered afresh, and its record kept among the ended
    /// ones, whose transcripts stay their owner's to read. A stopped session
    /// is held until its terminal ends, or every later hook would offer it
    /// again.
    fn retire(&mut self, harness_session: &str) {
        let Some(i) = self
            .sessions
            .iter()
            .position(|e| e.harness_session == harness_session)
        else {
            return;
        };
        let a = self.sessions.remove(i);
        self.ended.retain(|s| *s != a.session);
        self.ended.push(a.session);
        if self.ended.len() > MAX_ENDED {
            let over = self.ended.len() - MAX_ENDED;
            self.ended.drain(..over);
        }
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let doc = Doc {
            v: VERSION,
            agent: self.agent.clone(),
            sessions: self.sessions.clone(),
            ended: self.ended.clone(),
        };
        if let Err(e) = super::super::hosted::write_private_json(path, &doc) {
            warn!(file = %path.display(), %e, "hive: the adopted sessions were not saved");
        }
    }
}

/// What the server said to one offer.
pub(crate) type OfferAnswer = Result<(ObjectId, u64), HiveAdoptRefusal>;

/// Milliseconds since the epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Claude Code's session id: a UUID, or anything shaped like one.
fn clean_harness_session(raw: &str) -> Option<String> {
    let s = raw.trim();
    (!s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
        .then(|| s.to_ascii_lowercase())
}

/// What one transcript line means for the mirror.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct LineEffect {
    /// Events for the store.
    pub events: Vec<TranscriptEvent>,
    /// A person's prompt: a turn begins.
    pub prompt: bool,
    /// Tool calls in it.
    pub steps: u32,
    /// Claude Code's own measure of the turn that just ran.
    pub turn_duration_ms: Option<u64>,
}

/// Read one line of Claude Code's on-disk transcript. Only `user` and
/// `assistant` lines carry the conversation, in the same message shape the
/// stream-json adapter reads; a subagent's lines (`isSidechain`) and Claude
/// Code's own bookkeeping (`isMeta`, and every other line type) are not the
/// person's conversation and are skipped. A line that is not JSON is skipped
/// too: the format is internal to Claude Code, and a change in it must thin
/// the mirror, never stop it.
pub(crate) fn read_line(line: &str) -> LineEffect {
    let mut out = LineEffect::default();
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return out;
    };
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    if flag("isSidechain") || flag("isMeta") {
        return out;
    }
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "user" | "assistant" => {
            let Ok(parsed) = stream_json::parse_line(line, &stream_json::Limits::default()) else {
                return out;
            };
            if ty == "user" {
                // A prompt is text the person typed; a tool's result is not.
                out.prompt = parsed
                    .record
                    .iter()
                    .any(|e| matches!(e, TranscriptEvent::UserMessage { .. }));
            } else {
                out.steps = parsed
                    .record
                    .iter()
                    .filter(|e| matches!(e, TranscriptEvent::ToolUse { .. }))
                    .count() as u32;
            }
            out.events = parsed.record;
        }
        "system" => match v.get("subtype").and_then(Value::as_str).unwrap_or("") {
            "turn_duration" => {
                out.turn_duration_ms = v.get("durationMs").and_then(Value::as_u64);
            }
            "compact_boundary" => {
                if let Ok(parsed) = stream_json::parse_line(line, &stream_json::Limits::default()) {
                    out.events = parsed.record;
                }
            }
            _ => {}
        },
        _ => {}
    }
    out
}

impl Supervisor {
    /// Listen on the adopt socket: only while the device's owner allows
    /// adopting. The runtime directory is the daemon's (`0755`, no link on
    /// the way); the socket is `0666`, because every local account may adopt
    /// its own sessions and the kernel names each peer.
    #[cfg(unix)]
    pub(crate) async fn adopt_listen(self: Arc<Self>) -> Result<(), String> {
        if !self.cfg.adopt {
            return Ok(());
        }
        let runtime = self.runtime.clone();
        match std::fs::symlink_metadata(&runtime) {
            Ok(m) if !m.file_type().is_dir() => {
                return Err(format!("{} is not a plain directory", runtime.display()));
            }
            Ok(_) => {}
            Err(_) => std::fs::create_dir_all(&runtime)
                .map_err(|e| format!("{}: {e}", runtime.display()))?,
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{}: {e}", runtime.display()))?;
        let socket = runtime.join(proto::SOCKET_NAME);
        match std::fs::remove_file(&socket) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", socket.display())),
        }
        let listener =
            UnixListener::bind(&socket).map_err(|e| format!("{}: {e}", socket.display()))?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))
            .map_err(|e| format!("{}: {e}", socket.display()))?;
        info!(socket = %socket.display(), "hive: adopting terminal sessions (hive_adopt)");
        let me = Arc::clone(&self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SWEEP_EVERY).await;
                me.adopt_sweep();
            }
        });
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let me = Arc::clone(&self);
                    tokio::spawn(async move { me.adopt_conn(stream).await });
                }
                Err(e) => {
                    warn!(%e, "hive: the adopt socket stopped accepting");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    /// With adopting off: the socket a predecessor left when it was on.
    /// Nothing listens on it now, and its being there must not say otherwise
    /// (`roomler hive adopt` read it as "this device adopts", P1j-5's field
    /// run). A socket only: anything else at that path is left alone.
    #[cfg(unix)]
    pub(crate) fn adopt_remove_stale_socket(&self) {
        use std::os::unix::fs::FileTypeExt;
        let socket = self.runtime.join(proto::SOCKET_NAME);
        if !std::fs::symlink_metadata(&socket).is_ok_and(|m| m.file_type().is_socket()) {
            return;
        }
        match std::fs::remove_file(&socket) {
            Ok(()) => {
                info!(socket = %socket.display(), "hive: adopting is off — the socket left behind is removed")
            }
            Err(e) => {
                warn!(socket = %socket.display(), %e, "hive: a socket left behind could not be removed")
            }
        }
    }

    /// P1i-2 — nothing is adopted on Windows, so there is no socket to listen
    /// on. `hive_adopt` reads off there ([`super::gates::HiveConfig`]), so this
    /// is never reached; it refuses anyway rather than pretend to listen.
    #[cfg(windows)]
    pub(crate) async fn adopt_listen(self: Arc<Self>) -> Result<(), String> {
        Err("adopting terminal sessions is not available on Windows".into())
    }

    /// P1i-2 — and no socket to leave behind.
    #[cfg(windows)]
    pub(crate) fn adopt_remove_stale_socket(&self) {}

    /// One hook's connection: a `Hello`, then its requests, each answered.
    #[cfg(unix)]
    pub(crate) async fn adopt_conn(self: Arc<Self>, stream: UnixStream) {
        let cred = match stream.peer_cred() {
            Ok(c) => c,
            Err(e) => {
                debug!(%e, "hive: an adopt connection with no peer credentials — closed");
                return;
            }
        };
        let (uid, peer_pid) = (cred.uid(), cred.pid());
        let (read, mut write) = stream.into_split();
        let mut lines = LineReader::new(BufReader::new(read), MAX_REQUEST);
        let mut said_hello: Option<String> = None;
        loop {
            let line = match lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) | Err(_) => return,
            };
            let reply = match serde_json::from_slice::<Request>(&line) {
                Err(_) => Reply::refused(refusal::BAD_REQUEST, None),
                Ok(Request::Hello {
                    harness_session,
                    cwd,
                    event,
                }) => {
                    let (reply, hs) = self
                        .adopt_hello(uid, peer_pid, &harness_session, &cwd, event)
                        .await;
                    said_hello = hs;
                    reply
                }
                Ok(other) => match &said_hello {
                    None => Reply::refused(refusal::BAD_REQUEST, None),
                    Some(hs) => match other {
                        Request::Lines {
                            from,
                            data,
                            skipped,
                        } => self.adopt_lines(uid, hs, from, &data, skipped),
                        Request::TurnEnded => self.adopt_turn_ended(uid, hs),
                        Request::End { reason } => self.adopt_end(uid, hs, reason.as_deref()),
                        Request::Hello { .. } => unreachable!("matched above"),
                    },
                },
            };
            let mut out = match serde_json::to_vec(&reply) {
                Ok(b) => b,
                Err(_) => return,
            };
            out.push(b'\n');
            if write.write_all(&out).await.is_err() {
                return;
            }
        }
    }

    /// The `hive_accounts` keys that map to `account`, every one of them:
    /// the server adopts only when they name exactly one person.
    pub(crate) fn adopt_keys(&self, account: &str) -> Vec<String> {
        self.cfg
            .accounts
            .iter()
            .filter(|(_, a)| a.as_str() == account)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// `Hello`: the session is held, or offered to the server now. Returns the
    /// answer and, when the connection may go on, the session it speaks for.
    #[cfg(unix)]
    async fn adopt_hello(
        &self,
        uid: u32,
        peer_pid: Option<i32>,
        harness_session: &str,
        cwd: &str,
        event: HookEvent,
    ) -> (Reply, Option<String>) {
        if !self.cfg.adopt {
            return (Reply::refused(refusal::ADOPT_DISABLED, None), None);
        }
        if uid == 0 {
            return (Reply::refused(refusal::ROOT, None), None);
        }
        let Some(hs) = clean_harness_session(harness_session) else {
            return (Reply::refused(refusal::BAD_REQUEST, None), None);
        };
        let held = self.adopted_lock().get(&hs).cloned();
        if let Some(a) = held {
            if a.uid != uid {
                return (Reply::refused(refusal::NOT_YOURS, None), None);
            }
            if a.stopped {
                // Refused for good while its terminal runs. At its end it is
                // forgotten, as every ended session is: its `End` would never
                // come past this refusal.
                if matches!(event, HookEvent::SessionEnd) {
                    self.adopted_lock().retire(&hs);
                }
                return (Reply::refused(refusal::STOPPED, Some(a.offset)), None);
            }
            return (Reply::ok(Some(a.offset)), Some(hs));
        }
        let account = match crate::exec::account_name(uid) {
            Ok(a) => a,
            Err(_) => return (Reply::refused(refusal::NO_ACCOUNT, None), None),
        };
        let keys = self.adopt_keys(&account);
        if keys.is_empty() {
            return (Reply::refused(refusal::NO_ACCOUNT, None), None);
        }
        if self.adopted_lock().live().count() >= MAX_ADOPTED {
            return (
                Reply::refused(
                    format!(
                        "{}:{}",
                        refusal::SERVER,
                        HiveAdoptRefusal::AtCapacity.as_str()
                    ),
                    None,
                ),
                None,
            );
        }
        let folder = cwd.trim();
        if folder.is_empty() || folder.chars().any(char::is_control) {
            return (Reply::refused(refusal::BAD_REQUEST, None), None);
        }
        let answer = self
            .adopt_offer(&hs, keys, &account, folder.to_string())
            .await;
        let (session, fence) = match answer {
            None => return (Reply::refused(refusal::OFFLINE, None), None),
            Some(Err(word)) => {
                info!(account = %account, refused = word.as_str(), "hive: a terminal session was not adopted");
                return (
                    Reply::refused(format!("{}:{}", refusal::SERVER, word.as_str()), None),
                    None,
                );
            }
            Some(Ok(ok)) => ok,
        };
        // The terminal is the Claude Code that ran the hook — past the shell
        // it ran the hook's command line through (`procs::hook_terminal`).
        let terminal_pid = peer_pid
            .and_then(|p| u32::try_from(p).ok())
            .and_then(procs::hook_terminal);
        let terminal_started = terminal_pid.and_then(procs::started);
        let a = Adopted {
            harness_session: hs.clone(),
            session: session.to_hex(),
            fence,
            account,
            uid,
            offset: 0,
            turns: 0,
            running: None,
            terminal_pid,
            terminal_started,
            stopped: false,
        };
        // Two hooks of one session can race to offer it; the server answers
        // both with the same record, and the first one kept stands.
        {
            let mut file = self.adopted_lock();
            if let Some(kept) = file.get(&hs).cloned() {
                return (Reply::ok(Some(kept.offset)), Some(hs));
            }
            file.put(a);
        }
        info!(session = %session, ?event, "hive: a terminal session adopted");
        self.report(session, fence, HiveRunState::Idle, None);
        (Reply::ok(Some(0)), Some(hs))
    }

    /// Offer a terminal session to the server and wait for its word. `None`:
    /// not connected, or no answer in time.
    async fn adopt_offer(
        &self,
        harness_session: &str,
        keys: Vec<String>,
        account: &str,
        folder: String,
    ) -> Option<OfferAnswer> {
        let tx = self
            .reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        let adopt_id = ObjectId::new().to_hex();
        let (answer_tx, answer_rx) = oneshot::channel();
        self.adopt_offers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(adopt_id.clone(), answer_tx);
        let sent = tx
            .send(ClientMsg::HiveAdopt {
                adopt_id: adopt_id.clone(),
                harness_session: harness_session.to_string(),
                keys,
                account: account.to_string(),
                folder,
            })
            .await
            .is_ok();
        let answer = if sent {
            tokio::time::timeout(OFFER_TIMEOUT, answer_rx)
                .await
                .ok()?
                .ok()
        } else {
            None
        };
        self.adopt_offers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&adopt_id);
        answer
    }

    /// `rc:hive.adopt_ack`: the server's word on an offer still waiting.
    pub(crate) fn adopt_answered(&self, adopt_id: &str, answer: OfferAnswer) {
        let waiting = self
            .adopt_offers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(adopt_id);
        match waiting {
            Some(tx) => {
                let _ = tx.send(answer);
            }
            None => debug!(
                adopt_id,
                "hive: an adopt answer for no offer waiting — late"
            ),
        }
    }

    /// `Lines`: whole transcript lines from where this copy ends.
    pub(crate) fn adopt_lines(
        &self,
        uid: u32,
        hs: &str,
        from: u64,
        data: &str,
        skipped: u64,
    ) -> Reply {
        let mut file = self.adopted_lock();
        let Some(a) = file.get(hs).cloned() else {
            return Reply::refused(refusal::BAD_REQUEST, None);
        };
        if a.uid != uid {
            return Reply::refused(refusal::NOT_YOURS, None);
        }
        if a.stopped {
            return Reply::refused(refusal::STOPPED, Some(a.offset));
        }
        if from != a.offset || !(data.is_empty() || data.ends_with('\n')) {
            return Reply::refused(refusal::BAD_REQUEST, Some(a.offset));
        }
        let Ok(session) = ObjectId::parse_str(&a.session) else {
            return Reply::refused(refusal::BAD_REQUEST, None);
        };
        let mut next = a.clone();
        let mut frames: Vec<ClientMsg> = Vec::new();
        let mut states: Vec<HiveRunState> = Vec::new();
        for line in data.lines() {
            let effect = read_line(line);
            if effect.prompt {
                // A turn the terminal never closed (no `Stop`): over now.
                if let Some(r) = next.running.take() {
                    frames.push(turn_frame(
                        session,
                        next.fence,
                        &r,
                        HiveTurnStatus::Interrupted,
                    ));
                }
                next.turns += 1;
                let r = Running {
                    turn: next.turns,
                    steps: 0,
                    began_ms: now_ms(),
                    duration_ms: None,
                };
                frames.push(turn_frame(session, next.fence, &r, HiveTurnStatus::Running));
                states.push(HiveRunState::Running);
                next.running = Some(r);
            }
            if let Some(r) = next.running.as_mut() {
                r.steps += effect.steps;
                if effect.turn_duration_ms.is_some() {
                    r.duration_ms = effect.turn_duration_ms;
                }
            }
            if let Ok(store) = &self.store {
                for event in effect.events {
                    store.append(&a.session, a.fence, event);
                }
            }
        }
        // A line too long to send: its place is moved past, and marked.
        if skipped > 0
            && let Ok(store) = &self.store
        {
            store.append(
                &a.session,
                a.fence,
                TranscriptEvent::Note {
                    text: format!("A transcript line of {skipped} bytes was too long to mirror."),
                },
            );
        }
        next.offset = a.offset + data.len() as u64 + skipped;
        let offset = next.offset;
        file.put(next);
        drop(file);
        for f in frames {
            self.report_turn(session, f);
        }
        for s in states {
            self.report(session, a.fence, s, None);
        }
        Reply::ok(Some(offset))
    }

    /// `TurnEnded` (the `Stop` hook): the turn in progress is over.
    fn adopt_turn_ended(&self, uid: u32, hs: &str) -> Reply {
        let mut ended = None;
        let updated = self.adopted_lock().update(hs, |a| {
            if a.uid == uid && !a.stopped {
                ended = a.running.take();
            }
        });
        let Some(a) = updated else {
            return Reply::refused(refusal::BAD_REQUEST, None);
        };
        if a.uid != uid {
            return Reply::refused(refusal::NOT_YOURS, None);
        }
        if a.stopped {
            return Reply::refused(refusal::STOPPED, Some(a.offset));
        }
        let Ok(session) = ObjectId::parse_str(&a.session) else {
            return Reply::refused(refusal::BAD_REQUEST, None);
        };
        if let Some(r) = ended {
            self.report_turn(
                session,
                turn_frame(session, a.fence, &r, HiveTurnStatus::Ok),
            );
        }
        self.report(session, a.fence, HiveRunState::Idle, None);
        Reply::ok(Some(a.offset))
    }

    /// `End` (the `SessionEnd` hook): the terminal session is over, and this
    /// device forgets it. A `claude --resume` later is a new terminal run,
    /// offered afresh.
    fn adopt_end(&self, uid: u32, hs: &str, reason: Option<&str>) -> Reply {
        let a = self.adopted_lock().get(hs).cloned();
        let Some(a) = a else {
            return Reply::refused(refusal::BAD_REQUEST, None);
        };
        if a.uid != uid {
            return Reply::refused(refusal::NOT_YOURS, None);
        }
        let reason = reason
            .map(|r| {
                r.chars()
                    .filter(|c| !c.is_control())
                    .take(40)
                    .collect::<String>()
            })
            .filter(|r| !r.is_empty());
        let detail = match reason {
            Some(r) => format!("the terminal session ended ({r})"),
            None => "the terminal session ended".to_string(),
        };
        self.adopt_finish(&a, detail);
        Reply::ok(Some(a.offset))
    }

    /// End an adopted session for good: its turn interrupted if one ran, the
    /// state `ended`, and this device forgets it as a live one.
    fn adopt_finish(&self, a: &Adopted, detail: String) {
        self.adopted_lock().retire(&a.harness_session);
        let Ok(session) = ObjectId::parse_str(&a.session) else {
            return;
        };
        if a.stopped {
            // Already ended when it was stopped.
            return;
        }
        if let Some(r) = &a.running {
            self.report_turn(
                session,
                turn_frame(session, a.fence, r, HiveTurnStatus::Interrupted),
            );
        }
        self.report(session, a.fence, HiveRunState::Ended, Some(detail));
    }

    /// `rc:hive.stop` for an adopted session: stop mirroring it. The terminal
    /// session goes on; nothing more is taken for it. `false`: not adopted
    /// here, so the stop is the supervisor's.
    pub(crate) fn adopt_stop(&self, session: ObjectId, fence: u64) -> bool {
        let sid = session.to_hex();
        let found = {
            let mut file = self.adopted_lock();
            let Some(a) = file.by_session(&sid).cloned() else {
                return false;
            };
            if fence < a.fence {
                return true;
            }
            file.update(&a.harness_session, |e| {
                e.stopped = true;
                e.running = None;
            });
            a
        };
        info!(session = %session, "hive: mirroring stopped for an adopted session");
        if let Some(r) = &found.running {
            self.report_turn(
                session,
                turn_frame(session, found.fence, r, HiveTurnStatus::Interrupted),
            );
        }
        self.report(
            session,
            found.fence,
            HiveRunState::Ended,
            Some("mirroring stopped; the terminal session goes on".into()),
        );
        true
    }

    /// The terminals that closed without a word (killed, a reboot of their
    /// machine's session): their process is gone, or another holds its pid.
    /// A stopped session's terminal too: held until it ends, then forgotten.
    pub(crate) fn adopt_sweep(&self) {
        let gone: Vec<Adopted> = self
            .adopted_lock()
            .sessions
            .iter()
            .filter(|a| match (a.terminal_pid, a.terminal_started.as_deref()) {
                (Some(pid), Some(was)) => procs::started(pid).as_deref() != Some(was),
                _ => false,
            })
            .cloned()
            .collect();
        for a in gone {
            if !a.stopped {
                info!(session = %a.session, "hive: an adopted session's terminal closed");
            }
            self.adopt_finish(&a, "the terminal closed".into());
        }
    }

    /// The adopted sessions mirrored now, for the manifest.
    pub(crate) fn adopt_manifest(&self) -> Vec<(ObjectId, u64)> {
        self.adopted_lock()
            .live()
            .filter_map(|a| Some((ObjectId::parse_str(&a.session).ok()?, a.fence)))
            .collect()
    }

    /// Whether the device's owner allows adopting (`hive_adopt`).
    pub(crate) fn adopt_allowed(&self) -> bool {
        self.cfg.adopt
    }

    /// Whether `session` is an adopted one this device mirrors or mirrored:
    /// an ended one's transcript is still its owner's to read, with agent
    /// sessions off here too (P1j-5's field run: before, it was refused
    /// `hive_disabled` the moment its terminal ended).
    pub(crate) fn adopt_holds(&self, session: ObjectId) -> bool {
        self.adopted_lock().was_adopted(&session.to_hex())
    }

    fn adopted_lock(&self) -> std::sync::MutexGuard<'_, AdoptedFile> {
        self.adopted.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A turn's stub for the server, as for any session: no prompt, no output.
fn turn_frame(session: ObjectId, fence: u64, r: &Running, status: HiveTurnStatus) -> ClientMsg {
    let duration_ms = match status {
        HiveTurnStatus::Running => None,
        _ => r
            .duration_ms
            .or_else(|| Some(now_ms().saturating_sub(r.began_ms))),
    };
    ClientMsg::HiveTurn {
        session_id: session,
        fence,
        turn: r.turn,
        status: Some(status),
        // Asked at the terminal, by its owner: nobody to name from here.
        prompted_by: None,
        steps: r.steps,
        duration_ms,
        cost_usd: None,
    }
}

/// Where the adopted file goes, beside `hosted.json`.
pub(super) fn adopted_path(hosted: Option<&Path>) -> Option<PathBuf> {
    hosted.map(|p| p.with_file_name(ADOPTED_FILE))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const USER_PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":"tidy the build"},"isSidechain":false,"uuid":"1","timestamp":"2026-10-09T00:00:00Z"}"#;
    const ASSISTANT_TOOL: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]},"isSidechain":false}"#;
    const TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]},"isSidechain":false}"#;
    const ASSISTANT_TEXT: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]},"isSidechain":false}"#;
    const SIDECHAIN: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"a subagent"}]},"isSidechain":true}"#;
    const META: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<command-name>/clear</command-name>"}]},"isMeta":true}"#;
    const DURATION: &str =
        r#"{"type":"system","subtype":"turn_duration","durationMs":4321,"isMeta":false}"#;
    const BOOKKEEPING: &str = r#"{"type":"file-history-snapshot","messageId":"m","snapshot":{}}"#;

    #[test]
    fn only_the_persons_conversation_is_mirrored() {
        let p = read_line(USER_PROMPT);
        assert!(p.prompt, "a typed prompt begins a turn");
        assert!(
            matches!(p.events.as_slice(), [TranscriptEvent::UserMessage { text, .. }] if text == "tidy the build")
        );

        let t = read_line(ASSISTANT_TOOL);
        assert_eq!((t.prompt, t.steps), (false, 1));
        let r = read_line(TOOL_RESULT);
        assert!(!r.prompt, "a tool's result is not a prompt");
        assert!(matches!(
            r.events.as_slice(),
            [TranscriptEvent::ToolResult { ok: true, .. }]
        ));
        assert_eq!(read_line(ASSISTANT_TEXT).events.len(), 1);

        assert_eq!(
            read_line(SIDECHAIN),
            LineEffect::default(),
            "a subagent's turn"
        );
        assert_eq!(
            read_line(META),
            LineEffect::default(),
            "Claude Code's own bookkeeping"
        );
        assert_eq!(read_line(BOOKKEEPING), LineEffect::default());
        assert_eq!(read_line("not json"), LineEffect::default());
        assert_eq!(read_line(DURATION).turn_duration_ms, Some(4321));
    }

    #[test]
    fn a_harness_session_is_shaped_like_a_uuid() {
        assert_eq!(
            clean_harness_session("6F1C8A2E-3B7D-4E5F-9A10-2B3C4D5E6F70").as_deref(),
            Some("6f1c8a2e-3b7d-4e5f-9a10-2b3c4d5e6f70")
        );
        assert_eq!(clean_harness_session("../etc/passwd"), None);
        assert_eq!(clean_harness_session(""), None);
    }

    #[test]
    fn the_adopted_file_keeps_this_enrollments_sessions_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ADOPTED_FILE);
        let mut f = AdoptedFile::load(Some(path.clone()), "agent-a");
        f.put(Adopted {
            harness_session: "u1".into(),
            session: ObjectId::new().to_hex(),
            fence: 1,
            account: "alice".into(),
            uid: 1000,
            offset: 42,
            turns: 2,
            running: None,
            terminal_pid: None,
            terminal_started: None,
            stopped: false,
        });
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            AdoptedFile::load(Some(path.clone()), "agent-a")
                .get("u1")
                .unwrap()
                .offset,
            42
        );
        assert!(AdoptedFile::load(Some(path), "agent-b").get("u1").is_none());
    }

    // ─── the socket, end to end, as a hook speaks it ─────────────────────

    use crate::hive::supervisor::tests::{Rig, rig_with};
    use tokio::io::AsyncBufReadExt;

    const UUID: &str = "6f1c8a2e-3b7d-4e5f-9a10-2b3c4d5e6f70";

    /// The account this test runs as, or `None` as root (whose terminals are
    /// never adopted, so there is nothing to test).
    fn me() -> Option<String> {
        // SAFETY: geteuid reads our own credentials.
        let uid = unsafe { libc::geteuid() };
        (uid != 0).then(|| crate::exec::account_name(uid).expect("this account's name"))
    }

    /// A rig whose device allows adopting, mapping this account or not.
    async fn adopting(map_me: bool) -> (Rig, PathBuf) {
        let me = me().expect("not root");
        let r = rig_with(
            true,
            4,
            |c| {
                c.adopt = true;
                if map_me {
                    c.accounts.insert("me@example.com".into(), me.clone());
                }
            },
            |s| s,
        );
        let sup = Arc::clone(&r.sup);
        tokio::spawn(async move { sup.adopt_listen().await });
        let socket = r.root.path().join("run").join(proto::SOCKET_NAME);
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (r, socket)
    }

    struct Hook {
        lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        write: tokio::net::unix::OwnedWriteHalf,
    }

    impl Hook {
        async fn connect(socket: &Path) -> Self {
            let (read, write) = UnixStream::connect(socket).await.unwrap().into_split();
            Self {
                lines: BufReader::new(read).lines(),
                write,
            }
        }

        async fn ask(&mut self, req: Request) {
            let mut bytes = serde_json::to_vec(&req).unwrap();
            bytes.push(b'\n');
            self.write.write_all(&bytes).await.unwrap();
        }

        async fn reply(&mut self) -> Reply {
            let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
                .await
                .expect("a reply within 10 s")
                .unwrap()
                .expect("the socket is open");
            serde_json::from_str(&line).unwrap()
        }
    }

    fn hello(event: HookEvent) -> Request {
        Request::Hello {
            harness_session: UUID.into(),
            cwd: "/home/me/work".into(),
            event,
        }
    }

    /// The next report that `want` picks out.
    async fn next_report<T>(r: &mut Rig, want: impl Fn(&ClientMsg) -> Option<T>) -> T {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), r.reports.recv())
                .await
                .expect("a report within 10 s")
                .expect("the connection is open");
            if let Some(t) = want(&msg) {
                return t;
            }
        }
    }

    /// Decision 11 on the device: a terminal session is offered with the
    /// account's keys, mirrored from the lines the hook sends (the person's
    /// conversation only), reported turn by turn, named in the manifest, and
    /// stopped from Roomler for good.
    #[tokio::test]
    async fn a_terminal_session_is_offered_mirrored_and_stopped() {
        if me().is_none() {
            return;
        }
        let (mut r, socket) = adopting(true).await;
        let mut hook = Hook::connect(&socket).await;
        hook.ask(hello(HookEvent::SessionStart)).await;
        let (adopt_id, keys, account, folder) = next_report(&mut r, |m| match m {
            ClientMsg::HiveAdopt {
                adopt_id,
                keys,
                account,
                folder,
                harness_session,
            } if harness_session == UUID => Some((
                adopt_id.clone(),
                keys.clone(),
                account.clone(),
                folder.clone(),
            )),
            _ => None,
        })
        .await;
        assert_eq!(
            keys,
            ["me@example.com"],
            "the account's keys, from hive_accounts"
        );
        assert_eq!(
            account,
            me().unwrap(),
            "the kernel's word on who the peer is"
        );
        assert_eq!(folder, "/home/me/work");
        let sid = ObjectId::new();
        r.sup.adopt_answered(&adopt_id, Ok((sid, 1)));
        assert_eq!(hook.reply().await, Reply::ok(Some(0)));

        let data = format!(
            "{USER_PROMPT}\n{ASSISTANT_TOOL}\n{TOOL_RESULT}\n{SIDECHAIN}\n{META}\n{ASSISTANT_TEXT}\n{DURATION}\n"
        );
        hook.ask(Request::Lines {
            from: 0,
            data: data.clone(),
            skipped: 0,
        })
        .await;
        assert_eq!(hook.reply().await, Reply::ok(Some(data.len() as u64)));
        let events = r.store.events(&sid.to_hex());
        assert_eq!(
            events.len(),
            4,
            "the prompt, the tool call, its result, the answer: {events:?}"
        );
        let turn = next_report(&mut r, |m| match m {
            ClientMsg::HiveTurn {
                session_id,
                turn,
                status,
                ..
            } if *session_id == sid => Some((*turn, *status)),
            _ => None,
        })
        .await;
        assert_eq!(turn, (1, Some(HiveTurnStatus::Running)));

        // A hook that lost its place is told where the copy ends.
        hook.ask(Request::Lines {
            from: 3,
            data: format!("{ASSISTANT_TEXT}\n"),
            skipped: 0,
        })
        .await;
        assert_eq!(
            hook.reply().await,
            Reply::refused(refusal::BAD_REQUEST, Some(data.len() as u64))
        );

        // A line too long to send: the copy moves past it, and says so.
        let at = data.len() as u64;
        hook.ask(Request::Lines {
            from: at,
            data: String::new(),
            skipped: 9_000_000,
        })
        .await;
        assert_eq!(hook.reply().await, Reply::ok(Some(at + 9_000_000)));
        let events = r.store.events(&sid.to_hex());
        assert!(
            matches!(events.last(), Some(TranscriptEvent::Note { text }) if text.contains("9000000 bytes")),
            "{events:?}"
        );

        hook.ask(Request::TurnEnded).await;
        assert!(hook.reply().await.ok);
        let ended = next_report(&mut r, |m| match m {
            ClientMsg::HiveTurn {
                session_id,
                turn: 1,
                status: Some(HiveTurnStatus::Ok),
                steps,
                duration_ms,
                ..
            } if *session_id == sid => Some((*steps, *duration_ms)),
            _ => None,
        })
        .await;
        assert_eq!(
            ended,
            (1, Some(4321)),
            "one tool call, and Claude Code's own duration"
        );

        assert!(
            r.sup
                .manifest()
                .iter()
                .any(|e| e.session_id == sid && e.fence == 1),
            "the manifest names it"
        );
        assert!(r.sup.adopt_holds(sid));

        // Stopped from Roomler: ended, and nothing more is taken for it.
        r.sup.stop(sid, 1, "owner".into());
        let detail = next_report(&mut r, |m| match m {
            ClientMsg::HiveState {
                session_id,
                state: Some(HiveRunState::Ended),
                detail,
                ..
            } if *session_id == sid => Some(detail.clone()),
            _ => None,
        })
        .await;
        assert!(detail.unwrap().contains("mirroring stopped"));
        let mut again = Hook::connect(&socket).await;
        again.ask(hello(HookEvent::Stop)).await;
        let reply = again.reply().await;
        assert_eq!(
            reply.refused.as_deref(),
            Some(refusal::STOPPED),
            "{reply:?}"
        );
        assert!(
            !r.sup.manifest().iter().any(|e| e.session_id == sid),
            "a stopped session is not mirrored any more"
        );

        // Its terminal ends: refused still, and forgotten as a live session
        // (P1j-5's field run: it was held forever), its transcript still its
        // owner's to read. A `claude --resume` later is offered afresh.
        let mut last = Hook::connect(&socket).await;
        last.ask(hello(HookEvent::SessionEnd)).await;
        assert_eq!(
            last.reply().await.refused.as_deref(),
            Some(refusal::STOPPED)
        );
        assert!(r.sup.adopted_lock().get(UUID).is_none(), "forgotten");
        assert!(r.sup.adopt_holds(sid), "still readable");
        let mut resumed = Hook::connect(&socket).await;
        resumed.ask(hello(HookEvent::SessionStart)).await;
        next_report(&mut r, |m| match m {
            ClientMsg::HiveAdopt {
                harness_session, ..
            } if harness_session == UUID => Some(()),
            _ => None,
        })
        .await;
    }

    /// P1j-5 — an ended session is kept among the ended ones, across a
    /// restart, and the oldest falls off past the bound.
    #[test]
    fn an_ended_session_stays_readable_and_the_oldest_falls_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ADOPTED_FILE);
        let mut f = AdoptedFile::load(Some(path.clone()), "agent-a");
        let adopted = |hs: &str, session: &str| Adopted {
            harness_session: hs.into(),
            session: session.into(),
            fence: 1,
            account: "alice".into(),
            uid: 1000,
            offset: 0,
            turns: 0,
            running: None,
            terminal_pid: None,
            terminal_started: None,
            stopped: false,
        };
        let first = ObjectId::new().to_hex();
        f.put(adopted("u0", &first));
        f.retire("u0");
        assert!(f.get("u0").is_none(), "forgotten as a live session");
        assert!(f.was_adopted(&first));
        assert!(
            AdoptedFile::load(Some(path.clone()), "agent-a").was_adopted(&first),
            "kept across a restart"
        );
        // The bound, in memory (a file written a thousand times proves
        // nothing more).
        let mut g = AdoptedFile::load(None, "agent-a");
        g.put(adopted("u0", &first));
        g.retire("u0");
        for i in 1..=MAX_ENDED {
            g.put(adopted(&format!("u{i}"), &ObjectId::new().to_hex()));
            g.retire(&format!("u{i}"));
        }
        assert!(!g.was_adopted(&first), "the oldest fell off");
        assert_eq!(g.ended.len(), MAX_ENDED);
    }

    /// P1j-5 — with adopting off, the socket a predecessor left is removed;
    /// a file that is not a socket is left alone.
    #[tokio::test]
    async fn a_device_that_does_not_adopt_removes_the_socket_left_behind() {
        let r = rig_with(true, 4, |c| c.adopt = false, |s| s);
        let run = r.root.path().join("run");
        std::fs::create_dir_all(&run).unwrap();
        let socket = run.join(proto::SOCKET_NAME);
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        assert!(socket.exists(), "a socket left behind");
        r.sup.adopt_remove_stale_socket();
        assert!(!socket.exists(), "removed");
        std::fs::write(&socket, b"not a socket").unwrap();
        r.sup.adopt_remove_stale_socket();
        assert!(socket.exists(), "not the daemon's to remove");
    }

    /// P1j-5 — a stopped session whose terminal is gone is forgotten by the
    /// sweep, without a word: it ended when it was stopped.
    #[tokio::test]
    async fn the_sweep_forgets_a_stopped_session_whose_terminal_is_gone() {
        if me().is_none() {
            return;
        }
        let (mut r, _socket) = adopting(true).await;
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let pid = gone.id();
        gone.wait().unwrap();
        let sid = ObjectId::new();
        r.sup.adopted_lock().put(Adopted {
            harness_session: UUID.into(),
            session: sid.to_hex(),
            fence: 1,
            account: me().unwrap(),
            uid: 1000,
            offset: 7,
            turns: 1,
            running: None,
            terminal_pid: Some(pid),
            terminal_started: Some("a process that ended".into()),
            stopped: true,
        });
        r.sup.adopt_sweep();
        assert!(r.sup.adopted_lock().get(UUID).is_none(), "forgotten");
        assert!(r.sup.adopt_holds(sid), "still readable");
        while let Ok(Some(m)) =
            tokio::time::timeout(Duration::from_millis(300), r.reports.recv()).await
        {
            assert!(
                !matches!(&m, ClientMsg::HiveState { session_id, .. } if *session_id == sid),
                "nothing reported: {m:?}"
            );
        }
    }

    /// An account the device's owner did not map is refused at the socket,
    /// and the server is never asked.
    #[tokio::test]
    async fn an_unmapped_account_is_refused_and_nothing_is_offered() {
        if me().is_none() {
            return;
        }
        let (mut r, socket) = adopting(false).await;
        let mut hook = Hook::connect(&socket).await;
        hook.ask(hello(HookEvent::SessionStart)).await;
        let reply = hook.reply().await;
        assert_eq!(
            reply.refused.as_deref(),
            Some(refusal::NO_ACCOUNT),
            "{reply:?}"
        );
        while let Ok(Some(m)) =
            tokio::time::timeout(Duration::from_millis(300), r.reports.recv()).await
        {
            assert!(
                !matches!(m, ClientMsg::HiveAdopt { .. }),
                "nothing offered: {m:?}"
            );
        }
        // Nothing is taken before a Hello that was answered.
        hook.ask(Request::TurnEnded).await;
        assert_eq!(
            hook.reply().await.refused.as_deref(),
            Some(refusal::BAD_REQUEST)
        );
    }

    /// P1j-3 — the hook client (`roomler hive hook`) and the daemon speak the
    /// same protocol: the real client mirrors a real transcript file through
    /// the real socket, a `Stop` ends the turn, and the next run sends only
    /// what is new.
    #[tokio::test]
    async fn the_hook_client_and_the_daemon_speak_the_same_protocol() {
        use std::io::Write as _;
        if me().is_none() {
            return;
        }
        let (mut r, socket) = adopting(true).await;
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            format!("{USER_PROMPT}\n{ASSISTANT_TOOL}\n{TOOL_RESULT}\n{ASSISTANT_TEXT}\n"),
        )
        .unwrap();
        let input = |event| roomler_cli::hive_hooks::HookInput {
            session_id: UUID.into(),
            transcript_path: transcript.to_string_lossy().into_owned(),
            cwd: "/home/me/work".into(),
            event,
            reason: None,
        };

        let (sock, first) = (socket.clone(), input(HookEvent::Stop));
        let run =
            tokio::spawn(async move { roomler_cli::hive_hooks::mirror_at(&sock, &first).await });
        let adopt_id = next_report(&mut r, |m| match m {
            ClientMsg::HiveAdopt { adopt_id, .. } => Some(adopt_id.clone()),
            _ => None,
        })
        .await;
        let sid = ObjectId::new();
        r.sup.adopt_answered(&adopt_id, Ok((sid, 1)));
        run.await.unwrap().expect("the hook ran clean");
        assert_eq!(r.store.events(&sid.to_hex()).len(), 4);
        let steps = next_report(&mut r, |m| match m {
            ClientMsg::HiveTurn {
                session_id,
                turn: 1,
                status: Some(HiveTurnStatus::Ok),
                steps,
                ..
            } if *session_id == sid => Some(*steps),
            _ => None,
        })
        .await;
        assert_eq!(steps, 1);

        // The next turn's hook sends only the new lines.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap()
            .write_all(format!("{USER_PROMPT}\n{ASSISTANT_TEXT}\n").as_bytes())
            .unwrap();
        roomler_cli::hive_hooks::mirror_at(&socket, &input(HookEvent::Stop))
            .await
            .expect("the second run");
        assert_eq!(
            r.store.events(&sid.to_hex()).len(),
            6,
            "two new events, none twice"
        );
        next_report(&mut r, |m| match m {
            ClientMsg::HiveTurn {
                session_id,
                turn: 2,
                status: Some(HiveTurnStatus::Ok),
                ..
            } if *session_id == sid => Some(()),
            _ => None,
        })
        .await;

        // SessionEnd: ended, and forgotten as a live session; its transcript
        // is still its owner's to read.
        roomler_cli::hive_hooks::mirror_at(&socket, &input(HookEvent::SessionEnd))
            .await
            .expect("the last run");
        next_report(&mut r, |m| match m {
            ClientMsg::HiveState {
                session_id,
                state: Some(HiveRunState::Ended),
                ..
            } if *session_id == sid => Some(()),
            _ => None,
        })
        .await;
        assert!(r.sup.adopted_lock().get(UUID).is_none());
        assert!(r.sup.adopt_holds(sid));
    }
}

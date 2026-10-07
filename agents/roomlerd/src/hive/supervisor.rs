// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The supervisor: one per daemon, holding the sessions this device runs.
//!
//! # A start, end to end
//!
//! `handle_start` answers on the connection that asked, off the signaling
//! loop. Under one lock — starts are rare, and two of the same session must
//! not both launch — the supervisor checks, in order: the primary enrollment,
//! `hive_enabled`, the harness kind, a start it already runs (answered
//! `accepted` again and launched nothing: reconcile-on-connect re-sends a start
//! whose answer was lost), the account, the folder, capacity, and a harness
//! binary; then launches, and only a process that STARTED is `accepted`.
//!
//! # A session's life
//!
//! One task per session owns the harness's stdin, stdout and exit, so what it
//! sees happens in the order it happened: a prompt is written and recorded and
//! the session is `running`; a turn's `result` makes it `idle`; an exit — or a
//! stop, which closes stdin and signals the process group — makes it `ended`.
//! Reports go to the primary connection that is up NOW, and the latest of each
//! is replayed when a connection comes up, because a report sent into a dying
//! socket is lost and the server would otherwise show a stale state forever.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use roomler_ai_remote_control::hive::{
    HARNESS_CLAUDE_CODE, HiveRefusal, HiveRunState, HiveTurnStatus, hive_limits,
};
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_hive_node::TranscriptEvent;
use roomler_hive_node::launch::{LaunchSpec, SettingsSpec, unix_base_env, user_input_line};
use roomler_hive_node::stream_json::{Limits, parse_line};
use roomler_node_core::config::AgentConfig;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use super::gates::{self, HiveConfig};
use super::store::StoreHandle;

/// How long an `ended` report is replayed on reconnect: long enough to cross
/// a control-WS flap, short enough not to replay history forever.
const ENDED_REPLAY: Duration = Duration::from_secs(10 * 60);
/// A session task's input channel: its prompts and its stop.
const INPUT_QUEUE: usize = 16;
/// Prompts a session holds — admitted, not yet written to the harness —
/// before it refuses the next. Half the input channel, so a stop always
/// finds room in it.
const MAX_WAITING: usize = INPUT_QUEUE / 2;
/// Runs as the session's account: makes the session's config directory (so
/// the daemon never writes into a tree that account owns), enters the folder,
/// then becomes the harness. Arguments are positional — no value is ever
/// interpolated into the script.
const WRAPPER: &str = r#"umask 077 && mkdir -p -- "$1" && cd -- "$2" && shift 2 && exec "$@""#;
/// Longest stdout line kept; a longer one is consumed and skipped, never
/// buffered whole.
const MAX_LINE: usize = 8 * 1024 * 1024;
/// How long a stopped harness gets between SIGTERM and SIGKILL, and an
/// exiting one before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// How much of a failed harness's stderr its `ended` detail keeps.
const STDERR_TAIL: usize = 400;

/// What `rc:hive.start` asks for, as the supervisor uses it.
#[derive(Debug, Clone)]
pub struct StartOrder {
    pub session_id: ObjectId,
    pub harness: String,
    pub harness_session: String,
    pub fence: u64,
    pub folder: String,
    pub user_id: ObjectId,
    pub user_email: String,
    pub caller: String,
    pub resume: bool,
}

/// The device's answer to a start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    pub refused: Option<HiveRefusal>,
    pub account: Option<String>,
    pub detail: Option<String>,
}

impl Answer {
    fn refused(r: HiveRefusal, detail: impl Into<String>) -> Self {
        Self {
            refused: Some(r),
            account: None,
            detail: Some(detail.into()),
        }
    }

    fn accepted(account: &str) -> Self {
        Self {
            refused: None,
            account: Some(account.to_string()),
            detail: None,
        }
    }
}

/// Who a prompt came from: the user it is attributed to in the transcript
/// (by name) and on the turn's stub (by id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    pub user_id: ObjectId,
    pub name: String,
}

/// What the session task is fed.
enum Input {
    Prompt {
        author: Option<Author>,
        text: String,
    },
    Stop {
        reason: String,
    },
}

struct Live {
    fence: u64,
    account: String,
    input: mpsc::Sender<Input>,
    /// Prompts admitted and not yet begun — shared with the session task,
    /// which holds them while a turn runs (see `Task::prompt`).
    waiting: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct Report {
    fence: u64,
    state: HiveRunState,
    detail: Option<String>,
    at: Instant,
}

/// How a harness is spawned. Production has exactly one way: as the account
/// the device mapped the starter to. The other exists for the tests, which
/// cannot become another account, and is unconstructible outside them.
#[derive(Debug, Clone)]
enum Launcher {
    AsMappedAccount,
    #[cfg(test)]
    AsDaemon {
        home: PathBuf,
    },
}

pub struct Supervisor {
    cfg: HiveConfig,
    /// Per-session settings files (`<runtime>/<sid>/settings.json`): written
    /// by the daemon, readable by the session's account, writable only here.
    runtime: PathBuf,
    launcher: Launcher,
    store: Result<StoreHandle, String>,
    start_lock: tokio::sync::Mutex<()>,
    live: Mutex<HashMap<ObjectId, Live>>,
    reports: Mutex<HashMap<ObjectId, Report>>,
    /// The newest `rc:hive.turn` per session, replayed with the states.
    turns: Mutex<HashMap<ObjectId, ClientMsg>>,
    reporter: Mutex<Option<mpsc::Sender<ClientMsg>>>,
    /// Every state as it is reported, for the viewers of that session.
    states: broadcast::Sender<(ObjectId, HiveRunState)>,
    /// The viewer peers this device serves (P0d-2).
    viewers: super::view::Viewers,
}

static SUPERVISOR: OnceLock<Arc<Supervisor>> = OnceLock::new();

/// Build the daemon's supervisor from its config. Called once at start; a
/// second call is a no-op. Built even with `hive_enabled = false`, so a start
/// is answered with the device's actual refusal rather than silence.
pub fn init(cfg: &AgentConfig) {
    let hive = HiveConfig::from_agent(cfg);
    let store = store_path().and_then(|p| StoreHandle::spawn(Some(&p)));
    if let Err(e) = &store {
        warn!(%e, "hive: the replica store is unavailable — every start will be refused");
    }
    let sup = Supervisor::new(
        hive,
        PathBuf::from("/run/roomler-hive"),
        Launcher::AsMappedAccount,
        store,
    );
    let _ = SUPERVISOR.set(Arc::new(sup));
}

/// The daemon's supervisor, once [`init`] ran.
pub fn global() -> Option<Arc<Supervisor>> {
    SUPERVISOR.get().cloned()
}

/// `<data dir>/hive/hive.db`, in a directory locked to the daemon (0700, no
/// link on the way): other local users must not read anyone's transcript.
fn store_path() -> Result<PathBuf, String> {
    let dir = roomler_node_core::appdirs::project_dirs()
        .map(|p| p.data_local_dir().join("hive"))
        .ok_or("no data directory for the replica store")?;
    private_dir(&dir)?;
    Ok(dir.join("hive.db"))
}

/// Create `dir` if missing and lock it to the daemon's account. A link
/// anywhere on the path is refused, never followed.
fn private_dir(dir: &Path) -> Result<(), String> {
    if let Some(link) = roomler_node_core::recording_dir::link_component(dir) {
        return Err(format!("{} is a symbolic link", link.display()));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// `rc:hive.start` — answered on `tx`, the connection that asked, from a task
/// of its own: launching does I/O the signaling loop must not wait on.
pub fn handle_start(order: StartOrder, is_primary: bool, tx: mpsc::Sender<ClientMsg>) {
    tokio::spawn(async move {
        let (session_id, fence) = (order.session_id, order.fence);
        let answer = match global() {
            Some(sup) => sup.start(order, is_primary).await,
            None => Answer::refused(
                HiveRefusal::HiveDisabled,
                "agent sessions are not set up on this daemon",
            ),
        };
        let _ = tx
            .send(ClientMsg::HiveStartAck {
                session_id,
                fence,
                refused: answer.refused,
                account: answer.account,
                detail: answer.detail,
            })
            .await;
    });
}

/// `rc:hive.stop`. Idempotent: a session this device does not run is
/// reported `ended` at once, so the server's stop always completes.
pub fn handle_stop(session_id: ObjectId, fence: u64, reason: String, is_primary: bool) {
    if !is_primary {
        warn!(session = %session_id, "hive: rc:hive.stop ignored — not the primary enrollment");
        return;
    }
    match global() {
        Some(sup) => sup.stop(session_id, fence, reason),
        None => debug!(session = %session_id, "hive: rc:hive.stop with no supervisor"),
    }
}

/// The primary enrollment's control WS came up: send reports there from now
/// on, and replay the latest of each — a report sent into the socket that
/// just died never arrived.
pub fn on_connected(tx: mpsc::Sender<ClientMsg>) {
    if let Some(sup) = global() {
        sup.connected(tx);
    }
}

impl Supervisor {
    fn new(
        cfg: HiveConfig,
        runtime: PathBuf,
        launcher: Launcher,
        store: Result<StoreHandle, String>,
    ) -> Self {
        Self {
            cfg,
            runtime,
            launcher,
            store,
            start_lock: tokio::sync::Mutex::new(()),
            live: Mutex::new(HashMap::new()),
            reports: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            reporter: Mutex::new(None),
            states: broadcast::channel(64).0,
            viewers: Default::default(),
        }
    }

    /// Sessions running now.
    pub fn live_count(&self) -> usize {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub(crate) async fn start(self: &Arc<Self>, order: StartOrder, is_primary: bool) -> Answer {
        let _serial = self.start_lock.lock().await;
        let answer = self.decide_and_launch(&order, is_primary).await;
        match &answer.refused {
            None => info!(
                session = %order.session_id, account = ?answer.account, caller = %order.caller,
                "hive: session started"
            ),
            Some(r) => info!(
                session = %order.session_id, refused = r.as_str(), detail = ?answer.detail,
                caller = %order.caller, "hive: session start refused"
            ),
        }
        answer
    }

    async fn decide_and_launch(self: &Arc<Self>, order: &StartOrder, is_primary: bool) -> Answer {
        if !is_primary {
            return Answer::refused(
                HiveRefusal::HiveDisabled,
                "agent sessions run only for this device's primary organization",
            );
        }
        if !self.cfg.enabled {
            return Answer::refused(
                HiveRefusal::HiveDisabled,
                "agent sessions are off on this device (hive_enabled)",
            );
        }
        if order.harness != HARNESS_CLAUDE_CODE {
            return Answer::refused(
                HiveRefusal::Other,
                format!("unknown harness {:?}", order.harness),
            );
        }
        // Idempotent on session + fence: the start may be a re-send of one
        // whose answer was lost.
        if let Some(account) = self.running(order.session_id, order.fence) {
            return Answer::accepted(&account);
        }
        let account = match gates::account_for(&self.cfg, &order.user_id, &order.user_email) {
            Ok(a) => a,
            Err(r) => {
                return Answer::refused(r, "no hive_accounts entry maps you to a local account");
            }
        };
        let folder = match gates::folder_for(&self.cfg, &order.folder) {
            Ok(f) => f,
            Err(r) => {
                return Answer::refused(
                    r,
                    "the folder does not exist or is outside this device's hive_roots",
                );
            }
        };
        if self.live_count() >= self.cfg.max_sessions {
            return Answer::refused(
                HiveRefusal::AtCapacity,
                format!(
                    "this device already runs {} agent sessions (hive_max_sessions)",
                    self.cfg.max_sessions
                ),
            );
        }
        let store = match &self.store {
            Ok(s) => s.clone(),
            Err(e) => return Answer::refused(HiveRefusal::LaunchFailed, e.clone()),
        };
        match self.launch(order, &account, &folder, store) {
            Ok(()) => Answer::accepted(&account),
            Err((r, detail)) => Answer::refused(r, detail),
        }
    }

    /// The account a live session runs as, when it runs at `fence`.
    fn running(&self, session: ObjectId, fence: u64) -> Option<String> {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        live.get(&session)
            .filter(|l| l.fence == fence)
            .map(|l| l.account.clone())
    }

    /// Feed a prompt to a running session. The viewer peer (P0d-2) is what
    /// calls this; until then the tests do. A prompt that arrives while a turn
    /// runs waits for it (see `Task::prompt`).
    pub fn prompt(
        &self,
        session: ObjectId,
        author: Option<Author>,
        text: String,
    ) -> Result<(), String> {
        let (input, waiting) = {
            let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            live.get(&session)
                .map(|l| (l.input.clone(), Arc::clone(&l.waiting)))
        }
        .ok_or("no such session on this device")?;
        // A place is taken BEFORE the send, atomically, so two callers cannot
        // both have the last one.
        if waiting
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_WAITING).then_some(n + 1)
            })
            .is_err()
        {
            return Err(format!(
                "{MAX_WAITING} prompts are already waiting for this session"
            ));
        }
        input.try_send(Input::Prompt { author, text }).map_err(|_| {
            waiting.fetch_sub(1, Ordering::AcqRel);
            "the session is not taking prompts".to_string()
        })
    }

    pub(crate) fn stop(&self, session: ObjectId, fence: u64, reason: String) {
        let input = {
            let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            match live.get(&session) {
                // A device holding a NEWER fence ignores a stop for an older
                // one: the session moved on (P2's promotion).
                Some(l) if l.fence > fence => {
                    info!(session = %session, held = l.fence, fence, "hive: stop for an older fence ignored");
                    return;
                }
                Some(l) => Some(l.input.clone()),
                None => None,
            }
        };
        match input {
            Some(tx) => {
                if tx.try_send(Input::Stop { reason }).is_err() {
                    warn!(session = %session, "hive: stop could not reach the session task");
                }
            }
            None => self.report(
                session,
                fence,
                HiveRunState::Ended,
                Some("not running on this device".into()),
            ),
        }
    }

    fn connected(&self, tx: mpsc::Sender<ClientMsg>) {
        let replay: Vec<(ObjectId, Report)> = {
            let mut reports = self.reports.lock().unwrap_or_else(|e| e.into_inner());
            reports.retain(|_, r| r.state != HiveRunState::Ended || r.at.elapsed() < ENDED_REPLAY);
            reports.iter().map(|(s, r)| (*s, r.clone())).collect()
        };
        let turns: Vec<ClientMsg> = {
            let mut turns = self.turns.lock().unwrap_or_else(|e| e.into_inner());
            // A session whose state report was pruned is history.
            turns.retain(|s, _| replay.iter().any(|(r, _)| r == s));
            turns.values().cloned().collect()
        };
        // Each session's newest turn, then its state: the server applies
        // them in order, and a stub before the state is how they happened.
        for t in turns {
            let _ = tx.try_send(t);
        }
        for (session_id, r) in replay {
            let _ = tx.try_send(ClientMsg::HiveState {
                session_id,
                fence: r.fence,
                state: Some(r.state),
                detail: r.detail,
            });
        }
        *self.reporter.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    /// Record and send a turn's stub (`rc:hive.turn`), like [`Self::report`].
    fn report_turn(&self, session: ObjectId, msg: ClientMsg) {
        self.turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session, msg.clone());
        let tx = self
            .reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(msg);
        }
    }

    /// Record and send a state. `try_send` keeps reports in order and never
    /// blocks a session on a slow socket; what a full queue drops, the replay
    /// on the next connection carries.
    fn report(&self, session: ObjectId, fence: u64, state: HiveRunState, detail: Option<String>) {
        self.reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                session,
                Report {
                    fence,
                    state,
                    detail: detail.clone(),
                    at: Instant::now(),
                },
            );
        // Viewers of the session hear it too (P0d-2); nobody watching is fine.
        let _ = self.states.send((session, state));
        let tx = self
            .reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(ClientMsg::HiveState {
                session_id: session,
                fence,
                state: Some(state),
                detail,
            });
        }
    }

    // ─── What the viewer peer (P0d-2, `super::view`) reads ──────────────

    /// The device's own `hive_enabled`.
    pub(crate) fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub(crate) fn store_handle(&self) -> Result<StoreHandle, String> {
        self.store.clone()
    }

    /// Whether this device runs `session` now.
    pub(crate) fn holds_live(&self, session: ObjectId) -> bool {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&session)
    }

    /// The latest state reported for `session`, while it is remembered.
    pub(crate) fn run_state(&self, session: ObjectId) -> Option<HiveRunState> {
        self.reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session)
            .map(|r| r.state)
    }

    /// Every state reported from now on, of every session.
    pub(crate) fn subscribe_states(&self) -> broadcast::Receiver<(ObjectId, HiveRunState)> {
        self.states.subscribe()
    }

    pub(crate) fn viewers(&self) -> &super::view::Viewers {
        &self.viewers
    }

    /// Send a frame on the primary connection that is up NOW. Not replayed:
    /// a viewer's handshake that loses a frame times out, and the browser
    /// asks again.
    pub(crate) fn send(&self, msg: ClientMsg) {
        let tx = self
            .reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match tx {
            Some(tx) => {
                if tx.try_send(msg).is_err() {
                    debug!(
                        "hive: the control connection's queue is full — a view frame was dropped"
                    );
                }
            }
            None => debug!("hive: no control connection — a view frame was dropped"),
        }
    }

    fn finish(&self, session: ObjectId, fence: u64, detail: String) {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session);
        info!(session = %session, %detail, "hive: session ended");
        self.report(session, fence, HiveRunState::Ended, Some(detail));
    }

    fn launch(
        self: &Arc<Self>,
        order: &StartOrder,
        account: &str,
        folder: &Path,
        store: StoreHandle,
    ) -> Result<(), (HiveRefusal, String)> {
        let child = self.spawn(order, account, folder)?;
        let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE);
        let waiting = Arc::new(AtomicUsize::new(0));
        self.live.lock().unwrap_or_else(|e| e.into_inner()).insert(
            order.session_id,
            Live {
                fence: order.fence,
                account: account.to_string(),
                input: input_tx,
                waiting: Arc::clone(&waiting),
            },
        );
        // Up and waiting for its first prompt.
        self.report(order.session_id, order.fence, HiveRunState::Idle, None);
        let sup = Arc::clone(self);
        let (session, fence) = (order.session_id, order.fence);
        tokio::spawn(async move {
            let inputs = Inputs {
                rx: input_rx,
                waiting,
            };
            let detail = run(&sup, session, fence, child, inputs, store).await;
            sup.finish(session, fence, detail);
        });
        Ok(())
    }

    /// Spawn the harness as the session's account, through the wrapper.
    fn spawn(
        &self,
        order: &StartOrder,
        account: &str,
        folder: &Path,
    ) -> Result<Child, (HiveRefusal, String)> {
        let home = match &self.launcher {
            Launcher::AsMappedAccount => {
                crate::exec::account_home(account).map_err(|e| (HiveRefusal::NoAccount, e))?
            }
            #[cfg(test)]
            Launcher::AsDaemon { home } => home.clone(),
        };
        let harness = resolve_harness(&self.cfg, &home).ok_or_else(|| {
            (
                HiveRefusal::HarnessMissing,
                "Claude Code was not found (set hive_harness, or install it in ~/.local/bin, \
                 /usr/local/bin or /usr/bin)"
                    .to_string(),
            )
        })?;
        let sid = order.session_id.to_hex();
        let settings = write_settings(&self.runtime, &sid, &self.cfg).map_err(|e| {
            (
                HiveRefusal::LaunchFailed,
                format!("writing the session settings: {e}"),
            )
        })?;
        let spec = LaunchSpec {
            session: order.harness_session.clone(),
            harness,
            folder: folder.to_path_buf(),
            state_dir: home.join(".roomler").join("hive").join(&sid),
            settings,
            mcp_config: None,
            sidecar_base_url: None,
            resume: order.resume,
            permission_prompt_tool: None,
        };
        spec.validate()
            .map_err(|e| (HiveRefusal::LaunchFailed, e.to_string()))?;

        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(WRAPPER)
            .arg("roomler-hive")
            .arg(spec.config_dir())
            .arg(&spec.folder)
            .arg(&spec.harness)
            .args(spec.args());
        // A clean environment: none of the daemon's (root's, with its
        // ROOMLERD_* knobs) reaches a user's session.
        let path = format!(
            "{home}/.local/bin:{home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin",
            home = home.display()
        );
        cmd.env_clear()
            .envs(unix_base_env(&home, account, Some(&path)))
            .envs(spec.env_overrides())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // Its own process group, so a stop reaches the tools it started.
            .process_group(0);
        match &self.launcher {
            // The one privilege path exec, SSH and the PTY share: setgroups,
            // setgid, setuid in the child, verified, uid 0 refused.
            Launcher::AsMappedAccount => {
                crate::exec::apply_run_as(&mut cmd, &crate::exec::RunAs::Named(account.to_string()))
                    .map_err(|e| (HiveRefusal::NoAccount, e))?
            }
            #[cfg(test)]
            Launcher::AsDaemon { .. } => {}
        }
        cmd.spawn().map_err(|e| {
            (
                HiveRefusal::LaunchFailed,
                format!("starting the harness: {e}"),
            )
        })
    }
}

/// `hive_harness`, or the first default location that holds an executable
/// file.
fn resolve_harness(cfg: &HiveConfig, home: &Path) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = match &cfg.harness {
        Some(h) => vec![h.clone()],
        None => vec![
            home.join(".local/bin/claude"),
            PathBuf::from("/usr/local/bin/claude"),
            PathBuf::from("/usr/bin/claude"),
        ],
    };
    candidates.into_iter().find(|p| {
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// `<runtime>/<sid>/settings.json`, daemon-owned: readable by the session,
/// writable by nobody else. A link on the way is refused, never followed.
fn write_settings(runtime: &Path, sid: &str, cfg: &HiveConfig) -> Result<PathBuf, String> {
    let dir = runtime.join(sid);
    for d in [runtime, dir.as_path()] {
        match std::fs::symlink_metadata(d) {
            Ok(m) if !m.file_type().is_dir() => {
                return Err(format!("{} is not a plain directory", d.display()));
            }
            Ok(_) => {}
            Err(_) => std::fs::create_dir(d).map_err(|e| format!("{}: {e}", d.display()))?,
        }
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let doc = SettingsSpec {
        api_key_helper: cfg.api_key_helper.clone(),
        auto_memory_directory: None,
        sandbox_proxy: None,
        extra_read_denies: Vec::new(),
    }
    .to_json();
    let path = dir.join("settings.json");
    let tmp = dir.join("settings.json.tmp");
    std::fs::write(&tmp, doc.to_string()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Lines from a harness's stdout, bounded and CANCEL-SAFE: everything
/// consumed from the stream stays in `self` until a whole line is handed out,
/// so a prompt winning the `select!` mid-line costs nothing. A line longer
/// than [`MAX_LINE`] is consumed and handed out empty — a harness printing a
/// gigabyte without a newline cannot exhaust memory.
struct LineReader<R> {
    inner: R,
    buf: Vec<u8>,
    overlong: bool,
}

impl<R: AsyncBufRead + Unpin> LineReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            overlong: false,
        }
    }

    /// The next line without its newline; `None` at the end of the stream.
    async fn next_line(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            // The only await. Nothing is consumed before it returns, and
            // everything after it up to the next loop is synchronous.
            let chunk = self.inner.fill_buf().await?;
            if chunk.is_empty() {
                if self.buf.is_empty() && !self.overlong {
                    return Ok(None);
                }
                return Ok(Some(self.take()));
            }
            let (used, done) = match chunk.iter().position(|b| *b == b'\n') {
                Some(i) => (i + 1, true),
                None => (chunk.len(), false),
            };
            if !self.overlong {
                let body = &chunk[..if done { used - 1 } else { used }];
                if self.buf.len() + body.len() > MAX_LINE {
                    self.overlong = true;
                    self.buf = Vec::new();
                } else {
                    self.buf.extend_from_slice(body);
                }
            }
            self.inner.consume(used);
            if done {
                return Ok(Some(self.take()));
            }
        }
    }

    fn take(&mut self) -> Vec<u8> {
        let line = if self.overlong {
            Vec::new()
        } else {
            std::mem::take(&mut self.buf)
        };
        self.buf.clear();
        self.overlong = false;
        line
    }
}

/// The turn in progress.
struct Current {
    prompted_by: Option<ObjectId>,
    steps: u32,
    started: Instant,
}

/// One session's live state, owned by its task.
///
/// A TURN is one prompt and the harness's work on it, ended by the
/// stream-json `result`. Prompts are written ONE AT A TIME: one that arrives
/// while a turn runs waits in `queued`, because the harness would queue it
/// itself and its `result` would then close the wrong turn's stub.
struct Task<'a> {
    sup: &'a Supervisor,
    session: ObjectId,
    fence: u64,
    sid: String,
    store: StoreHandle,
    stdin: Option<ChildStdin>,
    state: HiveRunState,
    count: u32,
    current: Option<Current>,
    queued: std::collections::VecDeque<(Option<Author>, String)>,
    /// `Live::waiting`: a prompt's place is given back when it BEGINS.
    waiting: Arc<AtomicUsize>,
}

/// What a session task is fed, and the count of prompts it holds.
struct Inputs {
    rx: mpsc::Receiver<Input>,
    waiting: Arc<AtomicUsize>,
}

impl Task<'_> {
    fn set_state(&mut self, state: HiveRunState) {
        if self.state != state {
            self.state = state;
            self.sup.report(self.session, self.fence, state, None);
        }
    }

    async fn prompt(&mut self, author: Option<Author>, text: String) {
        if self.current.is_some() {
            self.queued.push_back((author, text));
            return;
        }
        self.begin(author, text).await;
    }

    /// Write one prompt and open its turn.
    async fn begin(&mut self, author: Option<Author>, text: String) {
        // Out of the waiting count whatever happens next: a prompt that
        // cannot be written is not waiting either.
        let _ = self
            .waiting
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        let Some(w) = self.stdin.as_mut() else { return };
        let mut out = user_input_line(&text);
        out.push('\n');
        if w.write_all(out.as_bytes()).await.is_err() || w.flush().await.is_err() {
            warn!(session = %self.session, "hive: the harness stopped reading prompts");
            return;
        }
        self.count += 1;
        let prompted_by = author.as_ref().map(|a| a.user_id);
        self.store.append(
            &self.sid,
            self.fence,
            TranscriptEvent::UserMessage {
                author: author.map(|a| a.name),
                text,
            },
        );
        self.current = Some(Current {
            prompted_by,
            steps: 0,
            started: Instant::now(),
        });
        self.turn_report(HiveTurnStatus::Running, None, None);
        self.set_state(HiveRunState::Running);
    }

    /// Record one event; a `result` closes the turn and lets the next queued
    /// prompt in.
    async fn on_event(&mut self, ev: TranscriptEvent) {
        if matches!(ev, TranscriptEvent::ToolUse { .. })
            && let Some(c) = self.current.as_mut()
        {
            c.steps += 1;
        }
        let end = match &ev {
            TranscriptEvent::Turn {
                ok,
                duration_ms,
                cost_usd,
                ..
            } => Some((*ok, *duration_ms, *cost_usd)),
            _ => None,
        };
        self.store.append(&self.sid, self.fence, ev);
        if let Some((ok, duration_ms, cost_usd)) = end
            && self.current.is_some()
        {
            let status = if ok {
                HiveTurnStatus::Ok
            } else {
                HiveTurnStatus::Error
            };
            let duration_ms = duration_ms.or_else(|| self.elapsed_ms());
            self.turn_report(status, duration_ms, cost_usd);
            self.current = None;
            match self.queued.pop_front() {
                Some((author, text)) => self.begin(author, text).await,
                None => self.set_state(HiveRunState::Idle),
            }
        }
    }

    /// A turn the harness never finished — a stop, an exit, a crash.
    fn interrupt(&mut self) {
        if self.current.is_some() {
            let elapsed = self.elapsed_ms();
            self.turn_report(HiveTurnStatus::Interrupted, elapsed, None);
            self.current = None;
        }
    }

    fn elapsed_ms(&self) -> Option<u64> {
        self.current
            .as_ref()
            .map(|c| u64::try_from(c.started.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    fn turn_report(&self, status: HiveTurnStatus, duration_ms: Option<u64>, cost_usd: Option<f64>) {
        let Some(c) = &self.current else { return };
        self.sup.report_turn(
            self.session,
            ClientMsg::HiveTurn {
                session_id: self.session,
                fence: self.fence,
                turn: self.count,
                status: Some(status),
                prompted_by: c.prompted_by,
                steps: c.steps,
                duration_ms,
                cost_usd,
            },
        );
    }
}

/// Own the session until its harness exits; return the `ended` detail.
async fn run(
    sup: &Supervisor,
    session: ObjectId,
    fence: u64,
    mut child: Child,
    inputs: Inputs,
    store: StoreHandle,
) -> String {
    let Inputs {
        rx: mut input,
        waiting,
    } = inputs;
    let Some(stdout) = child.stdout.take() else {
        return "the harness has no stdout".into();
    };
    let stderr = child
        .stderr
        .take()
        .map(|err| tokio::spawn(read_stderr_tail(session, err)));
    let mut task = Task {
        sup,
        session,
        fence,
        sid: session.to_hex(),
        store,
        stdin: child.stdin.take(),
        state: HiveRunState::Idle,
        count: 0,
        current: None,
        queued: Default::default(),
        waiting,
    };
    let mut lines = LineReader::new(BufReader::new(stdout));
    let limits = Limits::default();
    let mut stopped: Option<String> = None;

    loop {
        tokio::select! {
            // Biased to stdout: whatever the harness already printed is
            // recorded before the next prompt goes in, so the transcript's
            // order is the order things happened — never a coin toss between
            // two ready arms.
            biased;
            read = lines.next_line() => match read {
                Ok(Some(line)) => {
                    let Ok(text) = std::str::from_utf8(&line) else { continue };
                    if text.trim().is_empty() {
                        continue;
                    }
                    match parse_line(text.trim_end(), &limits) {
                        Ok(parsed) => {
                            for ev in parsed.record {
                                task.on_event(ev).await;
                            }
                            if !parsed.skipped.is_empty() {
                                debug!(session = %session, skipped = ?parsed.skipped, "hive: stream-json lines not recorded");
                            }
                        }
                        Err(e) => debug!(session = %session, %e, "hive: unparseable harness line"),
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    warn!(session = %session, %e, "hive: harness stdout failed");
                    break;
                }
            },
            cmd = input.recv() => match cmd {
                Some(Input::Prompt { author, text }) => task.prompt(author, text).await,
                Some(Input::Stop { reason }) => {
                    stopped = Some(reason);
                    break;
                }
                // The supervisor let go of the session: end it.
                None => {
                    stopped = Some("supervisor".into());
                    break;
                }
            },
        }
    }

    task.interrupt();
    // EOF on stdin ends a stream-json harness; the signals cover one that
    // does not listen, and the tools it started.
    drop(task.stdin.take());
    if stopped.is_some() {
        terminate(&mut child).await;
    }
    let status = match tokio::time::timeout(STOP_GRACE, child.wait()).await {
        Ok(Ok(s)) => Some(s),
        _ => {
            let _ = child.start_kill();
            child.wait().await.ok()
        }
    };
    // The process is gone, so its stderr is at EOF: wait for the tail.
    let tail = match stderr {
        Some(task) => tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default(),
        None => String::new(),
    };
    describe_end(stopped.as_deref(), status, &tail)
}

/// The last [`STDERR_TAIL`] characters a harness wrote to stderr, on one line.
async fn read_stderr_tail(session: ObjectId, err: tokio::process::ChildStderr) -> String {
    let mut buf = Vec::new();
    let _ = err.take(1024 * 1024).read_to_end(&mut buf).await;
    let text = String::from_utf8_lossy(&buf);
    let mut tail: Vec<char> = text.chars().rev().take(STDERR_TAIL).collect();
    tail.reverse();
    let tail: String = tail
        .into_iter()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let tail = tail.trim().to_string();
    if !tail.is_empty() {
        debug!(session = %session, stderr = %tail, "hive: harness stderr (tail)");
    }
    tail
}

/// SIGTERM to the harness's process group, then SIGKILL after the grace.
async fn terminate(child: &mut Child) {
    if let Some(pid) = child.id() {
        // SAFETY: a plain syscall on a pid this task owns; the negative pid
        // addresses the group `process_group(0)` created for it.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
        }
    }
    if tokio::time::timeout(STOP_GRACE, child.wait())
        .await
        .is_err()
        && let Some(pid) = child.id()
    {
        // SAFETY: as above.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
}

fn describe_end(
    stopped: Option<&str>,
    status: Option<std::process::ExitStatus>,
    stderr_tail: &str,
) -> String {
    use std::os::unix::process::ExitStatusExt;
    if let Some(reason) = stopped {
        return format!("stopped ({reason})");
    }
    let mut detail = match status.map(|s| (s.code(), s.signal())) {
        Some((Some(0), _)) => return "the harness exited".into(),
        Some((Some(c), _)) => format!("the harness exited with code {c}"),
        Some((None, Some(sig))) => format!("the harness was killed by signal {sig}"),
        _ => "the harness ended".into(),
    };
    if !stderr_tail.is_empty() {
        detail.push_str(": ");
        detail.push_str(stderr_tail);
    }
    detail.chars().take(hive_limits::MAX_DETAIL_LEN).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stand-in for Claude Code that speaks just enough stream-json. Like
    /// the headless harness reading stream-json input, it says `init` once
    /// its first prompt arrives, then answers each prompt with a text block
    /// and a turn result. A prompt containing `crash` exits 3 with a word on
    /// stderr; `slow` takes a second; `tool` makes one tool call first.
    pub(crate) const FAKE_HARNESS: &str = r#"#!/bin/sh
first=1
while IFS= read -r line; do
  case "$line" in
    *crash*) echo "boom" >&2; exit 3 ;;
  esac
  if [ "$first" = 1 ]; then
    echo '{"type":"system","subtype":"init","session_id":"fake","model":"m","cwd":"'"$PWD"'","tools":[]}'
    first=0
  fi
  case "$line" in
    *slow*) sleep 1 ;;
  esac
  case "$line" in
    *tool*)
      echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}'
      echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}}'
      ;;
  esac
  echo '{"type":"assistant","message":{"content":[{"type":"text","text":"hello from the fake"}]}}'
  echo '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"duration_ms":5,"total_cost_usd":0.25}'
done
"#;

    pub(crate) struct Rig {
        pub(crate) sup: Arc<Supervisor>,
        pub(crate) store: StoreHandle,
        pub(crate) reports: mpsc::Receiver<ClientMsg>,
        pub(crate) root: tempfile::TempDir,
        pub(crate) user: ObjectId,
    }

    pub(crate) fn rig(enabled: bool, max: usize) -> Rig {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let work = root.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let harness = root.path().join("claude");
        std::fs::write(&harness, FAKE_HARNESS).unwrap();
        std::fs::set_permissions(&harness, std::fs::Permissions::from_mode(0o755)).unwrap();
        let user = ObjectId::new();
        let cfg = HiveConfig {
            enabled,
            accounts: [(user.to_hex(), "dev".to_string())].into_iter().collect(),
            roots: vec![work],
            max_sessions: max,
            harness: Some(harness),
            api_key_helper: None,
        };
        let store = StoreHandle::spawn(None).unwrap();
        let sup = Arc::new(Supervisor::new(
            cfg,
            root.path().join("run"),
            Launcher::AsDaemon { home },
            Ok(store.clone()),
        ));
        let (tx, reports) = mpsc::channel(64);
        sup.connected(tx);
        Rig {
            sup,
            store,
            reports,
            root,
            user,
        }
    }

    pub(crate) fn order(r: &Rig) -> StartOrder {
        StartOrder {
            session_id: ObjectId::new(),
            harness: HARNESS_CLAUDE_CODE.into(),
            harness_session: "6f1c8a2e-3b7d-4e5f-9a10-2b3c4d5e6f70".into(),
            fence: 1,
            folder: r.root.path().join("work").to_string_lossy().into(),
            user_id: r.user,
            user_email: "dev@example.com".into(),
            caller: "Dev".into(),
            resume: false,
        }
    }

    /// The next state reported for `session`.
    pub(crate) async fn next_state(
        r: &mut Rig,
        session: ObjectId,
    ) -> (HiveRunState, Option<String>) {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), r.reports.recv())
                .await
                .expect("a state report within 10 s")
                .expect("the reporter is open");
            if let ClientMsg::HiveState {
                session_id,
                state: Some(s),
                detail,
                ..
            } = msg
                && session_id == session
            {
                return (s, detail);
            }
        }
    }

    async fn until_ended(r: &mut Rig, session: ObjectId) -> Option<String> {
        loop {
            let (s, detail) = next_state(r, session).await;
            if s == HiveRunState::Ended {
                return detail;
            }
        }
    }

    /// One turn report, as the tests compare it.
    #[derive(Debug, PartialEq)]
    struct Turn {
        turn: u32,
        status: HiveTurnStatus,
        steps: u32,
        prompted_by: Option<ObjectId>,
    }

    /// Every report for `session` up to the state `until`: the turns, and
    /// the states in order.
    async fn reports_until(
        r: &mut Rig,
        session: ObjectId,
        until: HiveRunState,
    ) -> (Vec<Turn>, Vec<HiveRunState>, Vec<ClientMsg>) {
        let (mut turns, mut states, mut raw) = (Vec::new(), Vec::new(), Vec::new());
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), r.reports.recv())
                .await
                .expect("a report within 10 s")
                .expect("the reporter is open");
            match &msg {
                ClientMsg::HiveTurn {
                    session_id,
                    turn,
                    status: Some(status),
                    steps,
                    prompted_by,
                    ..
                } if *session_id == session => turns.push(Turn {
                    turn: *turn,
                    status: *status,
                    steps: *steps,
                    prompted_by: *prompted_by,
                }),
                ClientMsg::HiveState {
                    session_id,
                    state: Some(s),
                    ..
                } if *session_id == session => {
                    states.push(*s);
                    if *s == until {
                        raw.push(msg);
                        return (turns, states, raw);
                    }
                }
                _ => {}
            }
            raw.push(msg);
        }
    }

    pub(crate) fn dev(r: &Rig) -> Option<Author> {
        Some(Author {
            user_id: r.user,
            name: "Dev".into(),
        })
    }

    #[tokio::test]
    async fn every_closed_gate_refuses_with_its_own_word() {
        let off = rig(false, 4);
        assert_eq!(
            off.sup.start(order(&off), true).await.refused,
            Some(HiveRefusal::HiveDisabled)
        );

        let r = rig(true, 4);
        assert_eq!(
            r.sup.start(order(&r), false).await.refused,
            Some(HiveRefusal::HiveDisabled),
            "a secondary org's start"
        );

        let mut o = order(&r);
        o.user_id = ObjectId::new();
        o.user_email = "stranger@example.com".into();
        assert_eq!(
            r.sup.start(o, true).await.refused,
            Some(HiveRefusal::NoAccount)
        );

        let mut o = order(&r);
        o.folder = "/etc".into();
        assert_eq!(
            r.sup.start(o, true).await.refused,
            Some(HiveRefusal::FolderNotAllowed)
        );

        let mut o = order(&r);
        o.harness = "codex".into();
        assert_eq!(r.sup.start(o, true).await.refused, Some(HiveRefusal::Other));
        assert_eq!(r.sup.live_count(), 0, "no refusal launched anything");
    }

    /// The whole life of one session: accepted with its account, `idle`, a
    /// prompt makes it `running`, the turn's result `idle` again, a stop
    /// `ended` — and the transcript is in the store, in order.
    #[tokio::test]
    async fn a_session_runs_reports_and_records_then_stops() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert_eq!(r.sup.start(o.clone(), true).await, Answer::accepted("dev"));
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);

        r.sup.prompt(sid, dev(&r), "say hello".into()).unwrap();
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Running);
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);

        // A re-sent start of the same session and fence launches nothing.
        assert_eq!(r.sup.start(o, true).await, Answer::accepted("dev"));
        assert_eq!(r.sup.live_count(), 1);

        r.sup.stop(sid, 1, "owner".into());
        assert_eq!(
            until_ended(&mut r, sid).await.as_deref(),
            Some("stopped (owner)")
        );
        assert_eq!(r.sup.live_count(), 0);

        let events = r.store.events(&sid.to_hex());
        let kinds: Vec<&str> = events.iter().map(TranscriptEvent::kind).collect();
        assert_eq!(
            kinds,
            ["user_message", "session_init", "assistant_text", "turn"],
            "the transcript, in the order it happened"
        );
        assert!(
            matches!(&events[0], TranscriptEvent::UserMessage { author: Some(a), text } if a == "Dev" && text == "say hello")
        );
    }

    #[tokio::test]
    async fn a_crash_ends_the_session_with_the_harness_s_words() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        r.sup.prompt(sid, None, "crash".into()).unwrap();
        let (turns, _, raw) = reports_until(&mut r, sid, HiveRunState::Ended).await;
        // The turn the crash cut short is reported as such, before the end.
        assert_eq!(
            turns.iter().map(|t| t.status).collect::<Vec<_>>(),
            [HiveTurnStatus::Running, HiveTurnStatus::Interrupted]
        );
        let detail = match raw.last() {
            Some(ClientMsg::HiveState {
                detail: Some(d), ..
            }) => d.clone(),
            other => panic!("expected the ended state, got {other:?}"),
        };
        assert!(detail.contains("code 3"), "{detail}");
        assert!(
            detail.contains("boom"),
            "the stderr tail explains it: {detail}"
        );
    }

    /// A turn is reported when its prompt goes in and when its result comes
    /// back — numbered, attributed, with its tool steps and what it cost.
    #[tokio::test]
    async fn a_turn_is_reported_when_it_starts_and_when_it_ends() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        r.sup.prompt(sid, dev(&r), "use a tool".into()).unwrap();
        let (turns, states, raw) = reports_until(&mut r, sid, HiveRunState::Idle).await;
        let by = Some(r.user);
        assert_eq!(
            turns,
            [
                Turn {
                    turn: 1,
                    status: HiveTurnStatus::Running,
                    steps: 0,
                    prompted_by: by
                },
                Turn {
                    turn: 1,
                    status: HiveTurnStatus::Ok,
                    steps: 1,
                    prompted_by: by
                },
            ]
        );
        assert_eq!(states, [HiveRunState::Running, HiveRunState::Idle]);
        let ended = raw
            .iter()
            .find_map(|m| match m {
                ClientMsg::HiveTurn {
                    status: Some(HiveTurnStatus::Ok),
                    duration_ms,
                    cost_usd,
                    ..
                } => Some((*duration_ms, *cost_usd)),
                _ => None,
            })
            .unwrap();
        assert_eq!(ended, (Some(5), Some(0.25)), "the harness's own figures");
        r.sup.stop(sid, 1, "owner".into());
    }

    /// A prompt that arrives while a turn runs waits for that turn's result:
    /// the harness would queue it too, and its result would then close the
    /// wrong turn. The session stays `running` across both.
    #[tokio::test]
    async fn a_prompt_during_a_turn_waits_for_it() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        r.sup.prompt(sid, dev(&r), "slow one".into()).unwrap();
        r.sup.prompt(sid, None, "second".into()).unwrap();
        let (turns, states, _) = reports_until(&mut r, sid, HiveRunState::Idle).await;
        assert_eq!(
            turns
                .iter()
                .map(|t| (t.turn, t.status, t.prompted_by))
                .collect::<Vec<_>>(),
            [
                (1, HiveTurnStatus::Running, Some(r.user)),
                (1, HiveTurnStatus::Ok, Some(r.user)),
                (2, HiveTurnStatus::Running, None),
                (2, HiveTurnStatus::Ok, None),
            ]
        );
        assert_eq!(
            states,
            [HiveRunState::Running, HiveRunState::Idle],
            "running across both turns, idle only after the second"
        );
        let events = r.store.events(&sid.to_hex());
        let kinds: Vec<&str> = events.iter().map(TranscriptEvent::kind).collect();
        assert_eq!(
            kinds,
            [
                "user_message",
                "session_init",
                "assistant_text",
                "turn",
                "user_message",
                "assistant_text",
                "turn",
            ],
            "the second prompt went in after the first turn's result"
        );
        r.sup.stop(sid, 1, "owner".into());
    }

    /// What waits behind a running turn is bounded, and the caller is TOLD:
    /// past `MAX_WAITING` a prompt is refused, never queued without end nor
    /// dropped after an `Ok`. Every admitted prompt still runs, in order, and
    /// the places come back as they begin.
    #[tokio::test]
    async fn prompts_waiting_behind_a_turn_are_bounded_and_refused_out_loud() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        r.sup.prompt(sid, None, "slow one".into()).unwrap();
        // Turn 1 has begun (its place is back) before the rest are sent.
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), r.reports.recv())
                .await
                .expect("a report within 10 s")
                .expect("the reporter is open");
            if matches!(
                msg,
                ClientMsg::HiveTurn {
                    turn: 1,
                    status: Some(HiveTurnStatus::Running),
                    ..
                }
            ) {
                break;
            }
        }
        for i in 0..MAX_WAITING {
            r.sup
                .prompt(sid, None, format!("waiting {i}"))
                .unwrap_or_else(|e| panic!("prompt {i} of {MAX_WAITING} refused: {e}"));
        }
        let refused = r
            .sup
            .prompt(sid, None, "one too many".into())
            .expect_err("a prompt past the bound is refused");
        assert!(refused.contains("already waiting"), "{refused}");

        let (turns, _, _) = reports_until(&mut r, sid, HiveRunState::Idle).await;
        let finished: Vec<u32> = turns
            .iter()
            .filter(|t| t.status == HiveTurnStatus::Ok)
            .map(|t| t.turn)
            .collect();
        let expected: Vec<u32> = (1..=1 + MAX_WAITING as u32).collect();
        assert_eq!(finished, expected, "every admitted prompt ran, in order");
        let texts: Vec<String> = r
            .store
            .events(&sid.to_hex())
            .into_iter()
            .filter_map(|e| match e {
                TranscriptEvent::UserMessage { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert!(
            !texts.iter().any(|t| t == "one too many"),
            "the refused prompt reached the harness: {texts:?}"
        );
        // Idle again: every place is back.
        r.sup
            .prompt(sid, None, "after".into())
            .expect("an idle session takes a prompt again");
        r.sup.stop(sid, 1, "owner".into());
    }

    #[tokio::test]
    async fn capacity_is_a_refusal_and_a_stop_for_nothing_still_ends() {
        let mut r = rig(true, 1);
        let first = order(&r);
        assert!(r.sup.start(first.clone(), true).await.refused.is_none());
        assert_eq!(
            r.sup.start(order(&r), true).await.refused,
            Some(HiveRefusal::AtCapacity)
        );
        r.sup.stop(first.session_id, 1, "owner".into());
        until_ended(&mut r, first.session_id).await;

        // A stop for a session this device never ran is answered `ended`, so
        // the server's stop completes.
        let unknown = ObjectId::new();
        r.sup.stop(unknown, 1, "owner".into());
        assert_eq!(
            until_ended(&mut r, unknown).await.as_deref(),
            Some("not running on this device")
        );
    }

    /// The settings file is the daemon's: readable by the session, nobody
    /// else's to write, and naming no secret.
    #[tokio::test]
    async fn the_settings_file_is_daemon_owned_and_names_no_secret() {
        let mut r = rig(true, 4);
        let o = order(&r);
        assert!(r.sup.start(o.clone(), true).await.refused.is_none());
        let path = r
            .root
            .path()
            .join("run")
            .join(o.session_id.to_hex())
            .join("settings.json");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(doc.get("apiKeyHelper").is_none(), "none configured: {doc}");
        assert!(doc["permissions"]["deny"].to_string().contains("/proc"));
        r.sup.stop(o.session_id, 1, "owner".into());
        until_ended(&mut r, o.session_id).await;
    }

    /// A reconnect replays the latest state of every session — what a dying
    /// socket swallowed.
    #[tokio::test]
    async fn a_new_connection_gets_the_latest_states_again() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        let (tx, mut again) = mpsc::channel(64);
        r.sup.connected(tx);
        match again.recv().await {
            Some(ClientMsg::HiveState {
                session_id,
                state: Some(s),
                ..
            }) => assert_eq!((session_id, s), (sid, HiveRunState::Idle)),
            other => panic!("expected a replayed state, got {other:?}"),
        }
        r.sup.stop(sid, 1, "owner".into());
    }

    /// Lines are split on newlines, an overlong one comes back empty, and a
    /// read cancelled mid-line loses nothing.
    #[tokio::test]
    async fn the_line_reader_is_bounded_and_cancel_safe() {
        let mut data: Vec<u8> = b"one\ntwo\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', MAX_LINE + 10));
        data.extend(b"\nthree");
        let mut r = LineReader::new(BufReader::new(&data[..]));
        let mut got = Vec::new();
        while let Some(line) = r.next_line().await.unwrap() {
            got.push(String::from_utf8(line).unwrap());
        }
        assert_eq!(got, ["one", "two", "", "three"]);

        // Half a line, a cancelled read, then the rest: one whole line.
        let (mut w, rd) = tokio::io::duplex(64);
        let mut r = LineReader::new(BufReader::new(rd));
        w.write_all(b"{\"type\":").await.unwrap();
        let cancelled = tokio::time::timeout(Duration::from_millis(50), r.next_line()).await;
        assert!(cancelled.is_err(), "no newline yet");
        w.write_all(b"\"x\"}\n").await.unwrap();
        assert_eq!(
            r.next_line().await.unwrap().unwrap(),
            b"{\"type\":\"x\"}".to_vec()
        );
    }
}

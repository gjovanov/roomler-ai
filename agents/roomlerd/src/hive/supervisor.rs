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
    HARNESS_CLAUDE_CODE, HiveApprovalStatus, HiveManifestEntry, HiveRefusal, HiveRunState,
    HiveTurnStatus, hive_limits,
};
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_hive_node::launch::{
    APPROVE_TOOL, DISALLOWED_TOOLS, LaunchSpec, PERMISSION_MODE, SettingsSpec, attributed_prompt,
    toolbelt_mcp_config, unix_base_env, user_input_line,
};
use roomler_hive_node::stream_json::{Limits, parse_line};
use roomler_hive_node::{TranscriptEvent, approval_outcome};
use roomler_node_core::config::AgentConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use super::gates::{self, HiveConfig};
use super::lines::LineReader;
use super::store::StoreHandle;
use super::toolbelt::{self, ApprovalEvent, Toolbelt};

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
/// How much of a failed harness's stderr its transcript note keeps — on the
/// device; the `ended` detail, which the server stores, never carries it.
const STDERR_TAIL: usize = 400;
/// The toolbelt's channel to its session task: an approval opening and
/// closing is two messages, and a session holds few open at once.
const APPROVAL_QUEUE: usize = 32;
/// Approvals per session whose newest frame a reconnect replays.
const APPROVAL_REPLAY: usize = 16;

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
    /// Who started it — the one driver this device knows without the server
    /// saying so (P1c-2, [`Supervisor::drives_here`]).
    starter: ObjectId,
    input: mpsc::Sender<Input>,
    /// Prompts admitted and not yet begun — shared with the session task,
    /// which holds them while a turn runs (see `Task::prompt`).
    waiting: Arc<AtomicUsize>,
    /// P1a — the session's open approvals, which a driver answers.
    approvals: Arc<toolbelt::Pending>,
}

/// What a launch hands the session task.
struct Spawned {
    child: Child,
    /// The model token this run holds, with the sidecar.
    token: Option<String>,
    /// P1a — owned by the task, so it ends exactly when the session does.
    toolbelt: Toolbelt,
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
/// cannot become another account, and is unconstructible outside them — the
/// unit tests, and an integration test built with `hive-test-launcher`.
#[derive(Debug, Clone)]
enum Launcher {
    AsMappedAccount,
    #[cfg(any(test, feature = "hive-test-launcher"))]
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
    /// P1a-2 — the newest `rc:hive.approval` of each of a session's recent
    /// approvals, replayed with the states: an end lost in a dying socket
    /// would leave the room's stub saying "needs approval" for ever.
    approval_frames: Mutex<HashMap<ObjectId, Vec<(String, ClientMsg)>>>,
    reporter: Mutex<Option<mpsc::Sender<ClientMsg>>>,
    /// Every state as it is reported, for the viewers of that session.
    states: broadcast::Sender<(ObjectId, HiveRunState)>,
    /// P1a — a session's open approvals each time they change, for its
    /// viewers: a driver's card shows its buttons only while it is open.
    approvals: broadcast::Sender<(ObjectId, Vec<String>)>,
    /// How long an approval waits for a person (shorter in the tests).
    approval_timing: toolbelt::Timing,
    /// The viewer peers this device serves (P0d-2).
    viewers: super::view::Viewers,
    /// P0e — the model sidecar: its port once bound, the session tokens it
    /// honours, the provider key the helper printed, and when the primary
    /// connection was lost (the `offline_grace` clock).
    sidecar_port: tokio::sync::OnceCell<u16>,
    tokens: super::sidecar::Tokens,
    model_key: tokio::sync::Mutex<Option<(String, Instant)>>,
    offline_since: Mutex<Option<Instant>>,
    upstream: String,
    offline_grace: Duration,
    /// P1d-1 — an update is about to restart the daemon: new prompts and
    /// starts are refused, so no turn begins in the gap before the installer
    /// runs ([`Supervisor::begin_update`]).
    updating: std::sync::atomic::AtomicBool,
}

static SUPERVISOR: OnceLock<Arc<Supervisor>> = OnceLock::new();

/// FR-90 P1d-1 (AC7) — the sessions mid-turn on this daemon: what an update
/// waits for. Empty where no supervisor was set up.
pub fn turns_running() -> Vec<ObjectId> {
    global().map(|s| s.turns_running()).unwrap_or_default()
}

/// FR-90 P1d-1 — how long an update waits for [`turns_running`]: the
/// device's `hive_update_wait_secs`.
pub fn update_wait() -> Duration {
    global().map_or(gates::DEFAULT_UPDATE_WAIT, |s| s.update_wait())
}

/// FR-90 P1d-1 — the update is going ahead: hold new prompts and starts.
pub fn begin_update() {
    if let Some(s) = global() {
        s.begin_update();
    }
}

/// FR-90 P1d-1 — the update did not happen (the installer did not start):
/// take prompts and starts again.
pub fn end_update() {
    if let Some(s) = global() {
        s.end_update();
    }
}

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

/// FR-90 P0f — an integration test's supervisor: sessions launch as the
/// daemon's OWN account, because a test process cannot become another one;
/// the store and the settings live where the test says.
///
/// ⚠️ Only with the `hive-test-launcher` feature, which no release build
/// enables — and even then refused when the daemon is root, so this can
/// never be how a session comes to run as root. One per process, like
/// [`init`]: a test that needs it gets a test binary of its own.
#[cfg(feature = "hive-test-launcher")]
pub fn init_as_daemon(
    cfg: &AgentConfig,
    store: &Path,
    runtime: &Path,
    home: &Path,
) -> Result<(), String> {
    // SAFETY: geteuid reads our own credentials.
    if unsafe { libc::geteuid() } == 0 {
        return Err("the test launcher never runs sessions as root".into());
    }
    let sup = Supervisor::new(
        HiveConfig::from_agent(cfg),
        runtime.to_path_buf(),
        Launcher::AsDaemon {
            home: home.to_path_buf(),
        },
        StoreHandle::spawn(Some(store)),
    );
    SUPERVISOR
        .set(Arc::new(sup))
        .map_err(|_| "a supervisor is already set up in this process".to_string())
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
            approval_frames: Mutex::new(HashMap::new()),
            reporter: Mutex::new(None),
            states: broadcast::channel(64).0,
            approvals: broadcast::channel(64).0,
            approval_timing: toolbelt::Timing::default(),
            viewers: Default::default(),
            sidecar_port: tokio::sync::OnceCell::new(),
            tokens: Default::default(),
            model_key: tokio::sync::Mutex::new(None),
            offline_since: Mutex::new(None),
            upstream: super::sidecar::DEFAULT_UPSTREAM.to_string(),
            offline_grace: super::sidecar::OFFLINE_GRACE,
            updating: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Sessions running now.
    pub fn live_count(&self) -> usize {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// P1d-1 (AC7) — the sessions mid-turn now: running, or waiting for an
    /// approval (a person answering is part of the turn).
    pub(crate) fn turns_running(&self) -> Vec<ObjectId> {
        let live: Vec<ObjectId> = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        let reports = self.reports.lock().unwrap_or_else(|e| e.into_inner());
        live.into_iter()
            .filter(|sid| {
                matches!(
                    reports.get(sid).map(|r| r.state),
                    Some(HiveRunState::Running | HiveRunState::AwaitingApproval)
                )
            })
            .collect()
    }

    pub(crate) fn update_wait(&self) -> Duration {
        self.cfg.update_wait
    }

    /// P1d-1 — the update is going ahead: refuse new prompts and starts
    /// until it has, or [`Self::end_update`] says it did not. A prompt
    /// already admitted still runs; the updater waits for it.
    pub(crate) fn begin_update(&self) {
        self.updating
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn end_update(&self) {
        self.updating
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    fn updating(&self) -> bool {
        self.updating.load(std::sync::atomic::Ordering::SeqCst)
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
        // P1d-1 — the daemon is about to restart for an update: a session
        // launched now would be cut at once.
        if self.updating() {
            return Answer::refused(
                HiveRefusal::Other,
                "this device is about to restart for an update — start the session again in a minute",
            );
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
        // P0e — with a model credential configured, the harness reaches the
        // provider only through the loopback sidecar, with a session token;
        // the key itself never enters the session.
        let sidecar = if self.cfg.api_key_helper.is_some() {
            match self.sidecar_port().await {
                Ok(port) => Some(port),
                Err(e) => return Answer::refused(HiveRefusal::LaunchFailed, e),
            }
        } else {
            None
        };
        match self.launch(order, &account, &folder, store, sidecar) {
            Ok(()) => Answer::accepted(&account),
            Err((r, detail)) => Answer::refused(r, detail),
        }
    }

    /// P1c-2 — the DEVICE's gate on a driver. The server names who drives a
    /// session; THIS device decides whom it lets act as one of its accounts:
    /// the session's starter (whom the start mapped), or someone its own
    /// `hive_accounts` maps to the account the session runs as. The gate that
    /// survives a wrong server, as `hive_accounts` already is for a start —
    /// a driver answers approvals too, so driving IS running code here as
    /// that account. `Err` says why, in words the viewer is shown.
    ///
    /// ⚠️ The starter is recognised by id, from the start order, never by the
    /// map: a server older than P1c-2 sends no address, and an account mapped
    /// by address must not lock the starter out of their own session.
    pub(crate) fn drives_here(
        &self,
        session: ObjectId,
        user: ObjectId,
        email: Option<&str>,
    ) -> Result<(), String> {
        let (starter, account) = {
            let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            match live.get(&session) {
                Some(l) => (l.starter, l.account.clone()),
                None => return Err("the session does not run on this device now".to_string()),
            }
        };
        if user == starter {
            return Ok(());
        }
        match gates::account_for(&self.cfg, &user, email.unwrap_or("")) {
            Ok(mapped) if mapped == account => Ok(()),
            Ok(_) => Err(format!(
                "this device's hive_accounts maps you to another account than {account}, \
                 which the session runs as — here you may read it, not drive it"
            )),
            Err(_) => Err("this device's hive_accounts maps you to no account — \
                 here you may read the session, not drive it"
                .to_string()),
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
        // P1d-1 — the daemon is about to restart for an update: a turn begun
        // now would be cut.
        if self.updating() {
            return Err(
                "this device is about to restart for an update — ask again in a minute".into(),
            );
        }
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

    pub(crate) fn connected(self: &Arc<Self>, tx: mpsc::Sender<ClientMsg>) {
        // P0e — online again; and when THIS connection closes, the
        // `offline_grace` clock starts (unless a newer one replaced it).
        *self.offline_since.lock().unwrap_or_else(|e| e.into_inner()) = None;
        {
            let watched = tx.clone();
            let me = Arc::downgrade(self);
            tokio::spawn(async move {
                watched.closed().await;
                if let Some(me) = me.upgrade() {
                    me.lost_connection(&watched);
                }
            });
        }
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
        let approvals: Vec<ClientMsg> = {
            let mut all = self
                .approval_frames
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            all.retain(|s, _| replay.iter().any(|(r, _)| r == s));
            all.values()
                .flat_map(|l| l.iter().map(|(_, m)| m.clone()))
                .collect()
        };
        // Each session's newest turn and approvals, then its state: the
        // server applies them in order, and a stub before the state is how
        // they happened.
        for t in turns.into_iter().chain(approvals) {
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
        // P1b — and every session this device RUNS, now: the server ends the
        // ones it holds as running here that the list leaves out. A daemon
        // that restarted holds no replay of what it ran, so this is the only
        // way the server learns those sessions are over (finding 7).
        let manifest: Vec<HiveManifestEntry> = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(session_id, l)| HiveManifestEntry {
                session_id: *session_id,
                fence: l.fence,
            })
            .collect();
        let _ = tx.try_send(ClientMsg::HiveManifest { sessions: manifest });
        *self.reporter.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    /// Record and send an approval's frame (`rc:hive.approval`), like
    /// [`Self::report_turn`]: the newest of each of the session's last
    /// [`APPROVAL_REPLAY`] approvals is replayed on the next connection.
    fn report_approval(&self, session: ObjectId, msg: ClientMsg) {
        let ClientMsg::HiveApproval { approval_id, .. } = &msg else {
            return;
        };
        {
            let mut all = self
                .approval_frames
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let list = all.entry(session).or_default();
            list.retain(|(a, _)| a != approval_id);
            list.push((approval_id.clone(), msg.clone()));
            if list.len() > APPROVAL_REPLAY {
                list.remove(0);
            }
        }
        let tx = self
            .reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(msg);
        }
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

    /// P1a — every change to any session's open approvals, from now on.
    pub(crate) fn subscribe_approvals(&self) -> broadcast::Receiver<(ObjectId, Vec<String>)> {
        self.approvals.subscribe()
    }

    /// The approvals open in `session` now; none for a session not live here.
    pub(crate) fn pending_approvals(&self, session: ObjectId) -> Vec<String> {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session)
            .map(|l| l.approvals.ids())
            .unwrap_or_default()
    }

    /// A driver's answer to one of `session`'s open approvals. The viewer
    /// peer calls this for a grant that may drive, and nothing else does.
    pub(crate) fn answer_approval(
        &self,
        session: ObjectId,
        approval: &str,
        decision: toolbelt::Decision,
        by: Author,
    ) -> Result<(), String> {
        let pending = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session)
            .map(|l| Arc::clone(&l.approvals))
            .ok_or("no such session on this device")?;
        if pending.answer(approval, toolbelt::Answer { decision, by }) {
            Ok(())
        } else {
            Err("that approval is not waiting (answered, expired or withdrawn)".into())
        }
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

    // ─── The model sidecar's side of the supervisor (P0e) ───────────────

    /// The sidecar's port, binding it on first use.
    async fn sidecar_port(self: &Arc<Self>) -> Result<u16, String> {
        self.sidecar_port
            .get_or_try_init(|| async {
                let (listener, port) = super::sidecar::bind().await?;
                super::sidecar::serve(listener, Arc::downgrade(self));
                info!(port, "hive: the model sidecar listens on loopback");
                Ok::<u16, String>(port)
            })
            .await
            .copied()
    }

    pub(crate) fn tokens(&self) -> &super::sidecar::Tokens {
        &self.tokens
    }

    /// Whether `session` runs here at `fence`.
    pub(crate) fn runs_at(&self, session: ObjectId, fence: u64) -> bool {
        self.running(session, fence).is_some()
    }

    /// How long the primary connection has been gone; `None` while it is up.
    pub(crate) fn offline_for(&self) -> Option<Duration> {
        self.offline_since
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|t| t.elapsed())
    }

    pub(crate) fn offline_grace(&self) -> Duration {
        self.offline_grace
    }

    pub(crate) fn upstream(&self) -> &str {
        &self.upstream
    }

    /// `watched` closed: the device is offline — unless a newer connection
    /// already took its place.
    fn lost_connection(&self, watched: &mpsc::Sender<ClientMsg>) {
        let current = self
            .reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|r| r.same_channel(watched));
        if current {
            let mut since = self.offline_since.lock().unwrap_or_else(|e| e.into_inner());
            if since.is_none() {
                *since = Some(Instant::now());
            }
        }
    }

    /// The provider key, from the device's helper, reused for [`KEY_TTL`].
    ///
    /// [`KEY_TTL`]: super::sidecar::KEY_TTL
    pub(crate) async fn model_key(&self) -> Result<String, String> {
        let mut cached = self.model_key.lock().await;
        if let Some((key, at)) = cached.as_ref()
            && at.elapsed() < super::sidecar::KEY_TTL
        {
            return Ok(key.clone());
        }
        let helper = self
            .cfg
            .api_key_helper
            .as_deref()
            .ok_or("this device has no model credential (hive_api_key_helper)")?;
        let key = super::sidecar::run_key_helper(helper).await?;
        *cached = Some((key.clone(), Instant::now()));
        Ok(key)
    }

    /// The provider refused the key: run the helper again next time.
    pub(crate) async fn forget_model_key(&self) {
        *self.model_key.lock().await = None;
    }

    /// The workspace a key not scoped to one makes its calls in.
    pub(crate) fn model_workspace(&self) -> Option<&str> {
        self.cfg.api_workspace_id.as_deref()
    }

    /// Tests only: a mock provider, and a shorter `offline_grace`.
    #[cfg(test)]
    pub(crate) fn with_sidecar(mut self, upstream: String, offline_grace: Duration) -> Self {
        self.upstream = upstream;
        self.offline_grace = offline_grace;
        self
    }

    /// Tests only: an approval that expires in a test's lifetime.
    #[cfg(test)]
    pub(crate) fn with_approval_timing(mut self, timing: toolbelt::Timing) -> Self {
        self.approval_timing = timing;
        self
    }

    fn finish(&self, session: ObjectId, fence: u64, token: Option<&str>, detail: String) {
        // Its model access ends with it.
        if let Some(token) = token {
            self.tokens.revoke(token);
        }
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
        sidecar: Option<u16>,
    ) -> Result<(), (HiveRefusal, String)> {
        let (approvals_tx, approvals_rx) = mpsc::channel(APPROVAL_QUEUE);
        let Spawned {
            child,
            token,
            toolbelt,
        } = self.spawn(order, account, folder, sidecar, approvals_tx)?;
        let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE);
        let waiting = Arc::new(AtomicUsize::new(0));
        self.live.lock().unwrap_or_else(|e| e.into_inner()).insert(
            order.session_id,
            Live {
                fence: order.fence,
                account: account.to_string(),
                starter: order.user_id,
                input: input_tx,
                waiting: Arc::clone(&waiting),
                approvals: toolbelt.pending(),
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
                approvals: approvals_rx,
                toolbelt,
            };
            let detail = run(&sup, session, fence, child, inputs, store).await;
            sup.finish(session, fence, token.as_deref(), detail);
        });
        Ok(())
    }

    /// Spawn the harness as the session's account, through the wrapper; with
    /// the sidecar, also the model token this run holds; and the session's
    /// toolbelt, bound before the harness starts because the harness
    /// connects to it as it starts.
    fn spawn(
        &self,
        order: &StartOrder,
        account: &str,
        folder: &Path,
        sidecar: Option<u16>,
        approvals: mpsc::Sender<ApprovalEvent>,
    ) -> Result<Spawned, (HiveRefusal, String)> {
        // The account the session runs as: its home, the ids its toolbelt
        // socket is handed to (`None`: it runs as the daemon), and its
        // groups — what the kernel checks when the harness starts the relay.
        let (home, owner, uid, groups) = match &self.launcher {
            Launcher::AsMappedAccount => {
                let home =
                    crate::exec::account_home(account).map_err(|e| (HiveRefusal::NoAccount, e))?;
                let (uid, gid, mut groups) =
                    crate::exec::account_ids(account).map_err(|e| (HiveRefusal::NoAccount, e))?;
                groups.push(gid);
                (home, Some((uid, gid)), uid, Some(groups))
            }
            #[cfg(any(test, feature = "hive-test-launcher"))]
            Launcher::AsDaemon { home } => {
                // SAFETY: getuid reads our own credentials.
                (home.clone(), None, unsafe { libc::getuid() }, None)
            }
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
        let settings = write_settings(&self.runtime, &sid).map_err(|e| {
            (
                HiveRefusal::LaunchFailed,
                format!("writing the session settings: {e}"),
            )
        })?;
        // P1a — the toolbelt, whose `approve` is the session's permission
        // tool: every tool call that needs one waits for a driver. Its relay
        // is this binary, started by the harness AS THE SESSION'S ACCOUNT:
        // one that account cannot run leaves the session without its
        // permission tool, and its first tool call ends it ("MCP tool …
        // not found") — far from the cause. Refused here, in words (field,
        // 2026-10-08: a daemon run from a 0750 home).
        let relay = own_exe().map_err(|e| (HiveRefusal::LaunchFailed, e))?;
        if let Some(groups) = &groups
            && !executable_by(Path::new(&relay), uid, groups)
        {
            return Err((
                HiveRefusal::LaunchFailed,
                format!(
                    "{account} cannot run {relay}, the session's toolbelt relay — install \
                     roomlerd where every account may execute it"
                ),
            ));
        }
        let dir = self.runtime.join(&sid);
        let toolbelt = toolbelt::open(
            &dir,
            owner,
            uid,
            order.session_id,
            approvals,
            self.approval_timing,
        )
        .map_err(|e| {
            (
                HiveRefusal::LaunchFailed,
                format!("opening the session's toolbelt: {e}"),
            )
        })?;
        let mcp = toolbelt_mcp_config(
            &relay,
            &[
                toolbelt::RELAY_SUBCOMMAND.to_string(),
                toolbelt.socket().to_string_lossy().into_owned(),
            ],
            toolbelt::TOOL_TIMEOUT_MS,
        );
        let mcp_config = write_doc(&dir, "mcp.json", &mcp).map_err(|e| {
            (
                HiveRefusal::LaunchFailed,
                format!("writing the session's MCP config: {e}"),
            )
        })?;
        let spec = LaunchSpec {
            session: order.harness_session.clone(),
            harness,
            folder: folder.to_path_buf(),
            state_dir: home.join(".roomler").join("hive").join(&sid),
            settings,
            mcp_config: Some(mcp_config),
            sidecar_base_url: sidecar.map(|port| format!("http://127.0.0.1:{port}/s/{sid}")),
            resume: order.resume,
            permission_prompt_tool: Some(APPROVE_TOOL.to_string()),
            permission_mode: Some(PERMISSION_MODE.to_string()),
            disallowed_tools: DISALLOWED_TOOLS.iter().map(|t| t.to_string()).collect(),
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
            .envs(spec.env_overrides());
        cmd.stdin(Stdio::piped())
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
            #[cfg(any(test, feature = "hive-test-launcher"))]
            Launcher::AsDaemon { .. } => {}
        }
        // The session's token, never the provider's key: good for this
        // session at this fence, through the sidecar, and nothing else.
        // Minted last, and taken back if the start fails, so no launch that
        // did not happen leaves one behind.
        let token = sidecar.map(|_| self.tokens.mint(order.session_id, order.fence));
        if let Some(token) = &token {
            cmd.env("ANTHROPIC_API_KEY", token);
        }
        match cmd.spawn() {
            Ok(child) => Ok(Spawned {
                child,
                token,
                toolbelt,
            }),
            // The toolbelt goes with the failed start: dropping it removes
            // its socket.
            Err(e) => {
                if let Some(token) = &token {
                    self.tokens.revoke(token);
                }
                Err((
                    HiveRefusal::LaunchFailed,
                    format!("starting the harness: {e}"),
                ))
            }
        }
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
fn write_settings(runtime: &Path, sid: &str) -> Result<PathBuf, String> {
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
    // P0e — the key helper is the DAEMON's (the sidecar runs it); the session
    // has no `apiKeyHelper`, so nothing in it can print the provider's key.
    let doc = SettingsSpec {
        api_key_helper: None,
        auto_memory_directory: None,
        sandbox_proxy: None,
        extra_read_denies: Vec::new(),
    }
    .to_json();
    write_doc(&dir, "settings.json", &doc)
}

/// `dir/name`, written whole (a temporary, then a rename) and `0644`: the
/// session reads it, and only the daemon writes it. `dir` is
/// [`write_settings`]'s, already made and checked.
fn write_doc(dir: &Path, name: &str, doc: &serde_json::Value) -> Result<PathBuf, String> {
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, doc.to_string()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Whether the account `uid` (with `groups`) may execute `path`: `x` on the
/// file and on every directory above it — by the owner's bits when it owns
/// the entry, else the group's when one of its groups does, else everyone's;
/// the kernel's order. (A POSIX ACL that grants more is not read: refusing a
/// start it would have allowed is the safe mistake.)
fn executable_by(path: &Path, uid: u32, groups: &[u32]) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mut entry = Some(path);
    while let Some(p) = entry {
        let Ok(m) = std::fs::metadata(p) else {
            return false;
        };
        let bit = if m.uid() == uid {
            0o100
        } else if groups.contains(&m.gid()) {
            0o010
        } else {
            0o001
        };
        if m.mode() & bit == 0 {
            return false;
        }
        entry = p.parent();
    }
    true
}

/// The daemon's own binary, which the session's MCP config names as the
/// toolbelt's relay. After a package upgrade replaced it under a running
/// daemon, Linux names the old inode `<path> (deleted)`; the path itself
/// holds the new binary, and a relay is a relay in every version.
fn own_exe() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("locating the daemon's binary: {e}"))?;
    let path = exe
        .to_str()
        .ok_or("the daemon's binary has a path that is not UTF-8")?;
    let path = path.strip_suffix(" (deleted)").unwrap_or(path);
    if !Path::new(path).is_file() {
        return Err(format!("the daemon's binary is not at {path} any more"));
    }
    Ok(path.to_string())
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
    /// The harness process's running cost at its last turn's end.
    spent: f64,
    /// P1a — the approvals open now, in the order the toolbelt reported
    /// them: the task's own view, so the run state follows the transcript.
    open_approvals: Vec<String>,
}

/// What a session task is fed, and the count of prompts it holds.
struct Inputs {
    rx: mpsc::Receiver<Input>,
    waiting: Arc<AtomicUsize>,
    /// P1a — the toolbelt's approvals as they open and close.
    approvals: mpsc::Receiver<ApprovalEvent>,
    toolbelt: Toolbelt,
}

impl Task<'_> {
    fn set_state(&mut self, state: HiveRunState) {
        if self.state != state {
            self.state = state;
            self.sup.report(self.session, self.fence, state, None);
        }
    }

    /// The state the session is in, from what it is doing: an open approval
    /// outranks a running turn, which outranks waiting for a prompt.
    fn settle_state(&mut self) {
        let state = if !self.open_approvals.is_empty() {
            HiveRunState::AwaitingApproval
        } else if self.current.is_some() {
            HiveRunState::Running
        } else {
            HiveRunState::Idle
        };
        self.set_state(state);
    }

    /// P1a — an approval opened or closed: recorded where the session's
    /// events are, in their order; the server hears THAT it did (P1a-2,
    /// `rc:hive.approval`, no tool, no arguments); the run state follows.
    fn on_approval(&mut self, ev: ApprovalEvent) {
        let (event, id, status, answered_by) = match ev {
            ApprovalEvent::Opened {
                id,
                tool_name,
                tool_use_id,
                input,
            } => {
                self.open_approvals.push(id.clone());
                let event = TranscriptEvent::ApprovalRequested {
                    id: id.clone(),
                    tool_name,
                    tool_use_id,
                    input,
                };
                (event, id, HiveApprovalStatus::Open, None)
            }
            ApprovalEvent::Closed { id, ended } => {
                self.open_approvals.retain(|a| *a != id);
                let (status, by) = approval_outcome_of(&ended);
                (resolved(id.clone(), ended), id, status, by)
            }
        };
        self.store.append(&self.sid, self.fence, event);
        self.report_approval(id, status, answered_by);
        self.settle_state();
        let _ = self
            .sup
            .approvals
            .send((self.session, self.open_approvals.clone()));
    }

    /// The session is ending with approvals still open: each is recorded as
    /// withdrawn, here, because the toolbelt's own word arrives after the
    /// task that would record it is gone.
    fn withdraw_open_approvals(&mut self) {
        for id in std::mem::take(&mut self.open_approvals) {
            self.store.append(
                &self.sid,
                self.fence,
                resolved(id.clone(), toolbelt::Ended::Withdrawn),
            );
            self.report_approval(id, HiveApprovalStatus::Withdrawn, None);
        }
        let _ = self.sup.approvals.send((self.session, Vec::new()));
    }

    fn report_approval(
        &self,
        approval_id: String,
        status: HiveApprovalStatus,
        answered_by: Option<ObjectId>,
    ) {
        let turn = self.current.as_ref().map(|_| self.count);
        self.sup.report_approval(
            self.session,
            ClientMsg::HiveApproval {
                session_id: self.session,
                fence: self.fence,
                approval_id,
                turn,
                status: Some(status),
                answered_by,
            },
        );
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
        // P1c-2 — the model is told who asked (`[Name] …`); the transcript
        // below keeps the prompt as typed, its author beside it.
        let mut out = user_input_line(&attributed_prompt(
            author.as_ref().map(|a| a.name.as_str()),
            &text,
        ));
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
        self.settle_state();
    }

    /// Record one event; a `result` closes the turn and lets the next queued
    /// prompt in.
    async fn on_event(&mut self, mut ev: TranscriptEvent) {
        if matches!(ev, TranscriptEvent::ToolUse { .. })
            && let Some(c) = self.current.as_mut()
        {
            c.steps += 1;
        }
        // Claude Code's `total_cost_usd` is its PROCESS's running total, so
        // every `result` carries all the turns before it too (field,
        // 2026-10-07: turn 3 read $0.33 for a turn that cost $0.16). The
        // transcript and the stub both say what THIS turn cost.
        if let TranscriptEvent::Turn {
            cost_usd: Some(total),
            ..
        } = &mut ev
        {
            let this_turn = ((*total - self.spent) * 1e6).round() / 1e6;
            self.spent = *total;
            *total = this_turn.max(0.0);
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
                None => self.settle_state(),
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

/// How an approval ended, as the server hears it: the word, and who
/// answered — never what was asked or what the driver said.
fn approval_outcome_of(ended: &toolbelt::Ended) -> (HiveApprovalStatus, Option<ObjectId>) {
    match ended {
        toolbelt::Ended::Answered(a) => match a.decision {
            toolbelt::Decision::Allow => (HiveApprovalStatus::Allowed, Some(a.by.user_id)),
            toolbelt::Decision::Deny { .. } => (HiveApprovalStatus::Denied, Some(a.by.user_id)),
        },
        toolbelt::Ended::Expired => (HiveApprovalStatus::Expired, None),
        toolbelt::Ended::Withdrawn => (HiveApprovalStatus::Withdrawn, None),
    }
}

/// How an approval ended, as the transcript records it.
fn resolved(id: String, ended: toolbelt::Ended) -> TranscriptEvent {
    let (outcome, by, message) = match ended {
        toolbelt::Ended::Answered(a) => match a.decision {
            toolbelt::Decision::Allow => (approval_outcome::ALLOWED, Some(a.by.name), None),
            toolbelt::Decision::Deny { message } => {
                (approval_outcome::DENIED, Some(a.by.name), message)
            }
        },
        toolbelt::Ended::Expired => (approval_outcome::EXPIRED, None, None),
        toolbelt::Ended::Withdrawn => (approval_outcome::WITHDRAWN, None, None),
    };
    TranscriptEvent::ApprovalResolved {
        id,
        outcome: outcome.to_string(),
        by,
        message,
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
        mut approvals,
        toolbelt,
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
        spent: 0.0,
        open_approvals: Vec::new(),
    };
    let mut lines = LineReader::new(BufReader::new(stdout), MAX_LINE);
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
            // P1a — after stdout, so the `tool_use` the harness printed is
            // recorded before the approval it asked for.
            Some(ev) = approvals.recv() => task.on_approval(ev),
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

    task.withdraw_open_approvals();
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
    // The harness is gone; its toolbelt goes with it.
    drop(toolbelt);
    // The process is gone, so its stderr is at EOF: wait for the tail.
    let tail = match stderr {
        Some(task) => tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default(),
        None => String::new(),
    };
    // What a harness writes is the session's own output — it can echo a
    // prompt, a path, a tool's error — so its last words stay with the
    // transcript, on the device. The server hears only HOW it ended (FR-90
    // AC2; the canary test found this channel).
    let clean = status.is_some_and(|s| s.success());
    let said = stopped.is_none() && !clean && !tail.is_empty();
    if said {
        task.store.append(
            &task.sid,
            fence,
            TranscriptEvent::Note {
                text: format!("The harness's last words on stderr: {tail}"),
            },
        );
    }
    describe_end(stopped.as_deref(), status, said)
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

/// The `ended` detail, which the SERVER stores and posts in the session's
/// room: how the harness ended, never what it said — `said` only points at
/// the transcript, where its last words are.
fn describe_end(
    stopped: Option<&str>,
    status: Option<std::process::ExitStatus>,
    said: bool,
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
    if said {
        detail.push_str("; its last words are in the transcript");
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
    /// stderr; `slow` takes a second; `tool` makes one tool call first;
    /// `hold` waits until the folder holds a `.release` file (and takes it),
    /// which is how a test keeps a turn running while it asks for an
    /// approval. Its `total_cost_usd`, like Claude Code's, is the PROCESS's
    /// running total: 0.25 more at every turn. Its argv is kept in the
    /// folder's `.argv`, one argument a line, and every line it reads on
    /// stdin in `.stdin` — what reached the harness (P1c-2).
    pub(crate) const FAKE_HARNESS: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$PWD/.argv"
first=1
spent=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$PWD/.stdin"
  case "$line" in
    *crash*) echo "boom" >&2; exit 3 ;;
  esac
  if [ "$first" = 1 ]; then
    echo '{"type":"system","subtype":"init","session_id":"fake","model":"m","cwd":"'"$PWD"'","tools":[]}'
    first=0
  fi
  case "$line" in
    *slow*) sleep 1 ;;
    *hold*) while [ ! -e "$PWD/.release" ]; do sleep 0.05; done; rm -f "$PWD/.release" ;;
  esac
  case "$line" in
    *tool*)
      echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}'
      echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}}'
      ;;
  esac
  echo '{"type":"assistant","message":{"content":[{"type":"text","text":"hello from the fake"}]}}'
  spent=$((spent + 25))
  cost=$(printf '%d.%02d' $((spent / 100)) $((spent % 100)))
  echo '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"duration_ms":5,"total_cost_usd":'"$cost"'}'
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
        rig_with(enabled, max, |_| {}, |s| s)
    }

    /// [`rig`], with the config and the supervisor adjusted before it runs
    /// (the sidecar's tests point it at a mock provider).
    pub(crate) fn rig_with(
        enabled: bool,
        max: usize,
        cfg_with: impl FnOnce(&mut HiveConfig),
        sup_with: impl FnOnce(Supervisor) -> Supervisor,
    ) -> Rig {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let work = root.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let harness = root.path().join("claude");
        std::fs::write(&harness, FAKE_HARNESS).unwrap();
        std::fs::set_permissions(&harness, std::fs::Permissions::from_mode(0o755)).unwrap();
        let user = ObjectId::new();
        let mut cfg = HiveConfig {
            enabled,
            accounts: [(user.to_hex(), "dev".to_string())].into_iter().collect(),
            roots: vec![work],
            max_sessions: max,
            harness: Some(harness),
            api_key_helper: None,
            api_workspace_id: None,
            update_wait: super::gates::DEFAULT_UPDATE_WAIT,
        };
        cfg_with(&mut cfg);
        let store = StoreHandle::spawn(None).unwrap();
        let sup = Arc::new(sup_with(Supervisor::new(
            cfg,
            root.path().join("run"),
            Launcher::AsDaemon { home },
            Ok(store.clone()),
        )));
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

    /// The `ended` detail goes to the SERVER, so it says how the harness
    /// ended and never what it wrote: its stderr can echo a prompt or a
    /// tool's output. The words stay in the transcript, on the device (AC2).
    #[tokio::test]
    async fn a_crash_ends_the_session_and_its_last_words_stay_on_the_device() {
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
            !detail.contains("boom"),
            "what the harness wrote never reaches the server: {detail}"
        );
        assert!(
            detail.contains("transcript"),
            "the detail says where the words are: {detail}"
        );
        let notes: Vec<String> = r
            .store
            .events(&sid.to_hex())
            .into_iter()
            .filter_map(|e| match e {
                TranscriptEvent::Note { text } => Some(text),
                _ => None,
            })
            .collect();
        assert!(
            notes.iter().any(|n| n.contains("boom")),
            "the stderr tail explains it, on the device: {notes:?}"
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

    /// Claude Code's `total_cost_usd` is its PROCESS's running total; a turn
    /// costs the difference. Field, 2026-10-07: turn 3 read $0.33 for a turn
    /// that cost $0.16.
    #[tokio::test]
    async fn each_turn_records_what_it_cost_not_the_running_total() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        for _ in 0..3 {
            r.sup.prompt(sid, None, "hi".into()).unwrap();
            assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Running);
            assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        }
        let costs: Vec<f64> = r
            .store
            .events(&sid.to_hex())
            .iter()
            .filter_map(|e| match e {
                TranscriptEvent::Turn { cost_usd, .. } => *cost_usd,
                _ => None,
            })
            .collect();
        assert_eq!(
            costs,
            [0.25, 0.25, 0.25],
            "the harness said 0.25, 0.50, 0.75"
        );
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

    /// The `rc:hive.manifest` a connection is sent, as (session, fence).
    async fn manifest_on(rx: &mut mpsc::Receiver<ClientMsg>) -> Vec<(ObjectId, u64)> {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("a manifest within 10 s")
                .expect("the connection is open");
            if let ClientMsg::HiveManifest { sessions } = msg {
                return sessions.iter().map(|e| (e.session_id, e.fence)).collect();
            }
        }
    }

    /// P1b — every connection hears which sessions run here, and only
    /// those: a session that ended is not in the list, so the server can end
    /// what a restart ended (finding 7).
    #[tokio::test]
    async fn a_new_connection_hears_which_sessions_run_here() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);

        let (tx, mut second) = mpsc::channel(64);
        r.sup.connected(tx);
        assert_eq!(manifest_on(&mut second).await, [(sid, 1)]);

        r.sup.stop(sid, 1, "owner".into());
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), second.recv())
                .await
                .expect("the stop's end within 10 s")
                .expect("open");
            if matches!(msg, ClientMsg::HiveState { session_id, state: Some(HiveRunState::Ended), .. } if session_id == sid)
            {
                break;
            }
        }
        let (tx, mut third) = mpsc::channel(64);
        r.sup.connected(tx);
        assert!(
            manifest_on(&mut third).await.is_empty(),
            "an ended session is not run here"
        );
    }

    // ─── P1a — approvals ────────────────────────────────────────────────

    use super::super::toolbelt::tests::{Client, call};
    use serde_json::json;

    pub(crate) fn toolbelt_socket(r: &Rig, sid: ObjectId) -> PathBuf {
        r.root
            .path()
            .join("run")
            .join(sid.to_hex())
            .join("toolbelt.sock")
    }

    /// Let a `hold` turn go on.
    pub(crate) fn release(r: &Rig) {
        std::fs::write(r.root.path().join("work").join(".release"), b"").unwrap();
    }

    /// Claude Code asking for one approval, as it does at a tool call: the
    /// toolbelt's handshake, then the permission prompt.
    pub(crate) async fn harness_asks(r: &Rig, sid: ObjectId, id: u64, command: &str) -> Client {
        let mut c = Client::at(&toolbelt_socket(r, sid)).await;
        c.send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25"}}))
            .await;
        assert_eq!(c.recv().await["id"], 0);
        c.send(call(id, "Bash", json!({"command": command}))).await;
        c
    }

    /// The one approval open in `sid`, once the session reports it waits.
    pub(crate) async fn until_awaiting(r: &mut Rig, sid: ObjectId) -> String {
        loop {
            if next_state(r, sid).await.0 == HiveRunState::AwaitingApproval {
                break;
            }
        }
        let open = r.sup.pending_approvals(sid);
        assert_eq!(open.len(), 1, "{open:?}");
        open[0].clone()
    }

    fn approval_events(r: &Rig, sid: ObjectId) -> Vec<TranscriptEvent> {
        r.store
            .events(&sid.to_hex())
            .into_iter()
            .filter(|e| e.kind().starts_with("approval_"))
            .collect()
    }

    /// A turn that needs approval waits for a driver, `awaiting_approval`;
    /// the answer goes back to the harness; the transcript says what was
    /// asked and who answered; and the session is back at its turn.
    #[tokio::test]
    async fn an_approval_waits_for_a_driver_and_the_transcript_says_who_answered() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        r.sup.prompt(sid, dev(&r), "hold".into()).unwrap();
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Running);

        let mut seen = r.sup.subscribe_approvals();
        let mut c = harness_asks(&r, sid, 2, "whoami").await;
        let approval = until_awaiting(&mut r, sid).await;
        assert_eq!(seen.recv().await.unwrap(), (sid, vec![approval.clone()]));

        let by = Author {
            user_id: r.user,
            name: "Dev".into(),
        };
        r.sup
            .answer_approval(sid, &approval, toolbelt::Decision::Allow, by.clone())
            .unwrap();
        assert_eq!(
            c.verdict(2).await,
            json!({"behavior": "allow", "updatedInput": {"command": "whoami"}})
        );
        assert_eq!(
            next_state(&mut r, sid).await.0,
            HiveRunState::Running,
            "back to the turn, which still runs"
        );
        assert_eq!(seen.recv().await.unwrap(), (sid, vec![]));
        assert!(
            r.sup
                .answer_approval(sid, &approval, toolbelt::Decision::Allow, by)
                .is_err(),
            "an approval is answered once"
        );

        release(&r);
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        assert_eq!(
            approval_events(&r, sid),
            [
                TranscriptEvent::ApprovalRequested {
                    id: approval.clone(),
                    tool_name: "Bash".into(),
                    tool_use_id: Some("toolu_01FAKE".into()),
                    input: json!({"command": "whoami"}),
                },
                TranscriptEvent::ApprovalResolved {
                    id: approval,
                    outcome: approval_outcome::ALLOWED.into(),
                    by: Some("Dev".into()),
                    message: None,
                },
            ]
        );
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }

    /// The relay check reads permissions as the kernel will for the session's
    /// account: a 0750 directory on the way stops anyone outside its group,
    /// whatever the binary's own bits say (field, 2026-10-08).
    #[test]
    fn a_relay_the_account_cannot_reach_is_not_executable_by_it() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let bin = home.join("roomlerd");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let me = std::fs::metadata(&bin).unwrap();
        let (uid, gid) = (me.uid(), me.gid());
        let other = uid.wrapping_add(1);

        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(executable_by(&bin, uid, &[]), "the owner");
        assert!(
            executable_by(&bin, other, &[gid]),
            "the group, through 0750"
        );
        assert!(
            !executable_by(&bin, other, &[]),
            "anyone else is stopped at the 0750 home"
        );

        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(executable_by(&bin, other, &[]), "a 0755 path is everyone's");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(
            !executable_by(&bin, other, &[]),
            "the binary's own bits still count"
        );
        assert!(!executable_by(&home.join("absent"), uid, &[]));
    }

    /// The next `rc:hive.approval` reported for `sid`.
    async fn next_approval_frame(r: &mut Rig, sid: ObjectId) -> ClientMsg {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), r.reports.recv())
                .await
                .expect("a report within 10 s")
                .expect("the reporter is open");
            if matches!(&msg, ClientMsg::HiveApproval { session_id, .. } if *session_id == sid) {
                return msg;
            }
        }
    }

    /// P1a-2 — the server hears THAT an approval waits, how it ended and who
    /// answered — never the tool or what it would run — and a reconnect
    /// hears it again, so a lost end cannot leave the room's stub open.
    #[tokio::test]
    async fn the_server_hears_that_an_approval_waits_and_how_it_ended_never_what() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        r.sup.prompt(sid, dev(&r), "hold".into()).unwrap();
        let mut c = harness_asks(&r, sid, 2, "cat /etc/canary-secret").await;
        let opened = next_approval_frame(&mut r, sid).await;
        let ClientMsg::HiveApproval {
            ref approval_id,
            turn,
            status,
            answered_by,
            ..
        } = opened
        else {
            unreachable!()
        };
        assert_eq!(
            (turn, status, answered_by),
            (Some(1), Some(HiveApprovalStatus::Open), None)
        );
        let id = approval_id.clone();
        let by = Author {
            user_id: r.user,
            name: "Dev".into(),
        };
        r.sup
            .answer_approval(sid, &id, toolbelt::Decision::Allow, by)
            .unwrap();
        c.verdict(2).await;
        let ended = next_approval_frame(&mut r, sid).await;
        let wire = serde_json::to_string(&ended).unwrap();
        for word in ["canary-secret", "Bash", "command"] {
            assert!(!wire.contains(word), "the frame names {word:?}: {wire}");
        }
        assert!(matches!(
            &ended,
            ClientMsg::HiveApproval { approval_id, status: Some(HiveApprovalStatus::Allowed), answered_by: Some(u), .. }
                if *approval_id == id && *u == r.user
        ));

        let (tx, mut again) = mpsc::channel(64);
        r.sup.connected(tx);
        let mut replayed = None;
        while let Ok(Some(m)) = tokio::time::timeout(Duration::from_secs(2), again.recv()).await {
            if let ClientMsg::HiveApproval { status, .. } = m {
                replayed = status;
            }
        }
        assert_eq!(
            replayed,
            Some(HiveApprovalStatus::Allowed),
            "its newest word, again"
        );
        release(&r);
        r.sup.stop(sid, 1, "owner".into());
    }

    /// Nobody answering: the model is told so, the transcript says
    /// `expired`, and the session is back at its turn.
    #[tokio::test]
    async fn an_unanswered_approval_expires_into_a_denial() {
        let mut r = rig_with(
            true,
            4,
            |_| {},
            |s| {
                s.with_approval_timing(toolbelt::Timing {
                    timeout: Duration::from_millis(300),
                    progress_every: Duration::from_secs(30),
                })
            },
        );
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        r.sup.prompt(sid, dev(&r), "hold".into()).unwrap();
        let mut c = harness_asks(&r, sid, 3, "make deploy").await;
        let approval = until_awaiting(&mut r, sid).await;
        let v = c.verdict(3).await;
        assert_eq!(v["behavior"], "deny");
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .starts_with("Nobody answered"),
            "{v}"
        );
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Running);
        release(&r);
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        assert!(matches!(
            approval_events(&r, sid).last(),
            Some(TranscriptEvent::ApprovalResolved { id, outcome, by: None, .. })
                if *id == approval && outcome == approval_outcome::EXPIRED
        ));
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }

    /// An approval still open when the session stops is withdrawn — in the
    /// transcript too — and the toolbelt goes with the session.
    #[tokio::test]
    async fn an_approval_open_at_a_stop_is_withdrawn_and_the_toolbelt_goes() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        r.sup.prompt(sid, dev(&r), "hold".into()).unwrap();
        let _harness = harness_asks(&r, sid, 2, "make").await;
        let approval = until_awaiting(&mut r, sid).await;
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
        assert!(
            matches!(
                approval_events(&r, sid).last(),
                Some(TranscriptEvent::ApprovalResolved { id, outcome, by: None, .. })
                    if *id == approval && outcome == approval_outcome::WITHDRAWN
            ),
            "{:?}",
            approval_events(&r, sid)
        );
        assert!(!toolbelt_socket(&r, sid).exists(), "the socket went too");
        assert!(r.sup.pending_approvals(sid).is_empty());
    }

    /// The harness is told who decides: the permission mode pinned, the
    /// toolbelt its only MCP config, its permission tool the toolbelt's
    /// `approve`; and the MCP config names the daemon's relay and the
    /// session's socket, which only the session's account may open.
    #[tokio::test]
    async fn the_launch_makes_the_toolbelt_the_one_that_decides() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        let argv_file = r.root.path().join("work").join(".argv");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !argv_file.exists() {
            assert!(Instant::now() < deadline, "the harness never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let argv: Vec<String> = std::fs::read_to_string(&argv_file)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        let pair = |a: &str, b: &str| argv.windows(2).any(|w| w[0] == a && w[1] == b);
        assert!(pair("--permission-mode", "default"), "{argv:?}");
        assert!(
            pair("--permission-prompt-tool", "mcp__roomler__approve"),
            "{argv:?}"
        );
        assert!(
            argv.contains(&"--strict-mcp-config".to_string()),
            "{argv:?}"
        );
        assert!(pair("--disallowedTools", "AskUserQuestion"), "{argv:?}");

        let mcp_path = r
            .root
            .path()
            .join("run")
            .join(sid.to_hex())
            .join("mcp.json");
        assert!(pair("--mcp-config", mcp_path.to_str().unwrap()), "{argv:?}");
        let mcp: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&mcp_path).unwrap()).unwrap();
        let server = &mcp["mcpServers"]["roomler"];
        assert_eq!(
            server["args"],
            json!(["hive-mcp", toolbelt_socket(&r, sid).to_str().unwrap()])
        );
        assert!(Path::new(server["command"].as_str().unwrap()).is_file());
        let mode = std::fs::metadata(&mcp_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o644,
            "the session reads it; only the daemon writes it"
        );
        let mode = std::fs::metadata(toolbelt_socket(&r, sid))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }

    /// P1c-2 — the model is told who asked: the harness reads `[Name] …`,
    /// while the transcript keeps the prompt as typed, its author beside it.
    /// A slash command reaches the harness as typed.
    #[tokio::test]
    async fn a_prompt_reaches_the_harness_labelled_with_its_driver() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        for text in ["say hello", "/compact"] {
            r.sup.prompt(sid, dev(&r), text.into()).unwrap();
            assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Running);
            assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        }
        let read: Vec<String> = std::fs::read_to_string(r.root.path().join("work").join(".stdin"))
            .unwrap()
            .lines()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["message"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(read, ["[Dev] say hello", "/compact"]);
        let typed: Vec<(Option<String>, String)> = r
            .store
            .events(&sid.to_hex())
            .into_iter()
            .filter_map(|e| match e {
                TranscriptEvent::UserMessage { author, text } => Some((author, text)),
                _ => None,
            })
            .collect();
        assert_eq!(
            typed,
            [
                (Some("Dev".to_string()), "say hello".to_string()),
                (Some("Dev".to_string()), "/compact".to_string()),
            ],
            "the transcript keeps what was typed"
        );
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }

    /// P1d-1 (AC7) — what an update waits for: a session mid-turn is in
    /// `turns_running`, an idle one is not. Once the update goes ahead, a new
    /// prompt and a new start are refused, while the turn already admitted
    /// runs to its end; an update that did not happen takes prompts again.
    #[tokio::test]
    async fn an_update_waits_for_turns_and_holds_new_ones() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        assert!(r.sup.turns_running().is_empty(), "idle is not a turn");

        r.sup.prompt(sid, dev(&r), "hold".into()).unwrap();
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Running);
        assert_eq!(r.sup.turns_running(), [sid]);

        r.sup.begin_update();
        let refused = r.sup.prompt(sid, dev(&r), "another".into()).unwrap_err();
        assert!(
            refused.contains("about to restart for an update"),
            "{refused}"
        );
        let start = r.sup.start(order(&r), true).await;
        assert_eq!(start.refused, Some(HiveRefusal::Other));
        assert!(
            start
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("update"),
            "{start:?}"
        );

        // The admitted turn still finishes, and is waited for no more.
        release(&r);
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        assert!(r.sup.turns_running().is_empty());

        // The update did not happen: prompts are taken again.
        r.sup.end_update();
        r.sup.prompt(sid, dev(&r), "after".into()).unwrap();
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Running);
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }

    /// P1c-2 — the device's own gate on a driver: the session's starter, or
    /// someone THIS device's `hive_accounts` maps to the account the session
    /// runs as; by id or by a proven address, never by an unproven one.
    #[tokio::test]
    async fn a_driver_acts_here_only_as_an_account_this_device_maps_them_to() {
        let same = ObjectId::new();
        let elsewhere = ObjectId::new();
        let mut r = rig_with(
            true,
            4,
            |c| {
                c.accounts.insert(same.to_hex(), "dev".into());
                c.accounts.insert("carol@example.com".into(), "dev".into());
                c.accounts
                    .insert("mallory@unverified.invalid".into(), "dev".into());
                c.accounts.insert(elsewhere.to_hex(), "ops".into());
            },
            |s| s,
        );
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);

        // The starter, by id from the start — even with no address at all,
        // as from a server before P1c-2.
        assert!(r.sup.drives_here(sid, r.user, None).is_ok());
        assert!(r.sup.drives_here(sid, same, None).is_ok(), "mapped by id");
        assert!(
            r.sup
                .drives_here(sid, ObjectId::new(), Some("Carol@Example.com"))
                .is_ok(),
            "mapped by a proven address"
        );
        assert!(
            r.sup
                .drives_here(sid, ObjectId::new(), Some("mallory@unverified.invalid"))
                .is_err(),
            "an unproven address maps nobody"
        );
        let other = r.sup.drives_here(sid, elsewhere, None).unwrap_err();
        assert!(other.contains("another account than dev"), "{other}");
        let nobody = r
            .sup
            .drives_here(sid, ObjectId::new(), Some("dave@example.com"))
            .unwrap_err();
        assert!(nobody.contains("no account"), "{nobody}");
        assert!(
            r.sup.drives_here(ObjectId::new(), r.user, None).is_err(),
            "a session this device does not run"
        );
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }
}

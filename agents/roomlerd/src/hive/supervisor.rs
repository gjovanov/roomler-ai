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
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bson::oid::ObjectId;
use roomler_ai_remote_control::hive::{
    HARNESS_CLAUDE_CODE, HiveApprovalStatus, HiveManifestEntry, HiveRefusal, HiveRunState,
    HiveTurnStatus, hive_limits,
};
use roomler_ai_remote_control::signaling::ClientMsg;
#[cfg(unix)]
use roomler_hive_node::launch::unix_base_env;
use roomler_hive_node::launch::{
    APPROVE_TOOL, DISALLOWED_TOOLS, LaunchSpec, PERMISSION_MODE, SettingsSpec, attributed_prompt,
    toolbelt_mcp_config, user_input_line,
};
use roomler_hive_node::stream_json::{Limits, parse_line};
use roomler_hive_node::{TranscriptEvent, approval_outcome};
use roomler_node_core::config::AgentConfig;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::process::{Child, ChildStdin};
use tokio::sync::{broadcast, mpsc};

/// P1i-2 — on Windows a harness is a [`crate::hive_win::HarnessChild`]: started
/// into its Job Object as the console user, with the part of tokio's `Child`
/// the session task uses, its stdin an async file over the pipe.
#[cfg(windows)]
use crate::hive_win::HarnessChild as Child;
#[cfg(windows)]
type ChildStdin = tokio::fs::File;
use tracing::{debug, info, warn};

use super::gates::{self, HiveConfig};
use super::hosted::{Hosted, HostedSession, RunningTurn};
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
///
/// P1e — `$3`, when set, is the daemon-owned directory holding the session's
/// core memory, and `$4` the session's auto-memory directory: each file is
/// copied in AS THE ACCOUNT, and only when nothing is there yet — so a
/// resume keeps what the session has (its own edits included). A copy that
/// fails is skipped: memory never stops a session.
///
/// P1i-2 — on Windows `roomlerd hive-prep` does this, run as the console user
/// ([`crate::hive_win::prep`]); the harness's folder is its working directory.
#[cfg(unix)]
const WRAPPER: &str = r#"umask 077 && mkdir -p -- "$1" && { if [ -n "$3" ]; then if [ -f "$3/CLAUDE.md" ] && [ ! -e "$1/CLAUDE.md" ] && [ ! -L "$1/CLAUDE.md" ]; then cp -- "$3/CLAUDE.md" "$1/CLAUDE.md" 2>/dev/null; fi; if [ -f "$3/MEMORY.md" ] && [ ! -e "$4/MEMORY.md" ] && [ ! -L "$4/MEMORY.md" ]; then mkdir -p -- "$4" 2>/dev/null && cp -- "$3/MEMORY.md" "$4/MEMORY.md" 2>/dev/null; fi; fi; true; } && cd -- "$2" && shift 4 && exec "$@""#;
/// Each session's runtime files (settings, MCP config, toolbelt socket, core
/// memory): the daemon's own, cleared at boot.
#[cfg(target_os = "linux")]
const RUNTIME_DIR: &str = tunnel_core::localapi::hive_adopt::RUNTIME_DIR_LINUX;
#[cfg(target_os = "macos")]
const RUNTIME_DIR: &str = tunnel_core::localapi::hive_adopt::RUNTIME_DIR_MACOS;

/// The runtime root this daemon writes each session's files under. On Windows
/// (P1i-2) under the daemon's machine-global directory, where nothing clears
/// it at boot: each launch gives its session's directory its DACL again.
fn runtime_root() -> PathBuf {
    #[cfg(unix)]
    return PathBuf::from(RUNTIME_DIR);
    #[cfg(windows)]
    return crate::hive_win::runtime_root();
}
/// P1h — how long the daemon gives its harnesses to exit on SIGTERM when it
/// leaves ([`wind_down`]) before SIGKILL: within the 5 s a service manager
/// usually waits for a stop.
const WIND_DOWN_GRACE: Duration = Duration::from_secs(3);
/// P1e — how long a session's core memory waits for its start's launch,
/// and how many snapshots wait at once: the server sends one right before
/// each start, so anything older or more is not waiting for anything.
const MEMORY_WAIT: Duration = Duration::from_secs(10 * 60);
const MEMORY_PENDING: usize = 64;
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
/// P1d-2 — what the daemon hosts, beside the replica store.
const HOSTED_FILE: &str = "hosted.json";
/// P1d-2 — how long a harness that ended without being stopped waits before
/// its end counts. Under systemd a stop reaches the daemon first and the
/// harness a moment later, and a session cut by the daemon's own stop is
/// surviving the restart, not ending.
const SETTLE: Duration = Duration::from_secs(1);
/// P1d-2 — a resumed session the daemon does not outlive by this, restarting
/// again, counts towards [`MAX_QUICK_RESUMES`].
const RESUME_STABLE: Duration = Duration::from_secs(120);
/// P1d-2 — resumes in a row that a session did not outlive by
/// [`RESUME_STABLE`]: at this many it is ended instead, so a resume that
/// takes the daemon down cannot become a crash loop.
const MAX_QUICK_RESUMES: u32 = 3;
/// P1d-2 — how long a connection's manifest waits for the resume. Past it,
/// the manifest goes out without what is still resuming; the server ends
/// those, and stops them again when their launch reports in.
const RESUME_BUDGET: Duration = Duration::from_secs(60);

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

/// FR-90 P1e — a session's core memory, as `rc:hive.memory` carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreMemory {
    pub session_id: ObjectId,
    pub fence: u64,
    pub brain_rev: u64,
    /// The org's facts, then the starter's.
    pub claude_md: Option<String>,
    /// The device's facts, for the auto-memory index.
    pub memory_md: Option<String>,
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
    /// P1h — the harness's pid, which leads its process group.
    pid: Option<u32>,
}

/// What a launch hands the session task.
struct Spawned {
    child: Child,
    /// The model token this run holds, with the sidecar.
    token: Option<String>,
    /// P1a — owned by the task, so it ends exactly when the session does.
    toolbelt: Toolbelt,
    /// P1d-2 — launched with `--resume`: the harness has its history.
    history: bool,
}

/// P1d-2 — where a session the device hosted stood when its last daemon
/// went down, as the session task takes it up again.
struct Resumed {
    turns: u32,
    /// The turn the restart cut, if one ran.
    cut: Option<RunningTurn>,
    /// The approvals it left open.
    approvals: Vec<String>,
    /// Whether the harness found its history (`--resume`).
    history: bool,
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
    #[cfg(all(unix, any(test, feature = "hive-test-launcher")))]
    AsDaemon {
        home: PathBuf,
    },
}

/// P1j — terminal sessions adopted on this device.
mod adopt;

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
    /// P1d-2 — the sessions this device hosts, on disk ([`super::hosted`]).
    hosted: Mutex<Hosted>,
    /// P1d-2 — what the previous daemon hosted, until the first connection
    /// resumes it. A stop that comes first takes its session out.
    to_resume: Mutex<Vec<HostedSession>>,
    /// P1d-2 — set once that resume has run: a manifest waits for it.
    resumed: tokio::sync::OnceCell<()>,
    /// P1d-2 — the daemon is stopping ([`begin_shutdown`]): a harness that
    /// ends from now on went down with it, and its session is the next
    /// daemon's to resume.
    going_down: AtomicBool,
    /// P1e — core memory waiting for its start's launch, by (session, fence).
    memory: Mutex<HashMap<(ObjectId, u64), (CoreMemory, Instant)>>,
    /// P1j — the terminal sessions this device mirrors ([`adopt`]), on disk.
    adopted: Mutex<adopt::AdoptedFile>,
    /// P1j — adopt offers waiting for the server's word, by offer id.
    adopt_offers: Mutex<HashMap<String, tokio::sync::oneshot::Sender<adopt::OfferAnswer>>>,
}

static SUPERVISOR: OnceLock<Arc<Supervisor>> = OnceLock::new();

/// FR-90 P1e — `rc:hive.memory`: kept for its start's launch. The primary
/// enrollment's only, like every Hive frame.
pub fn handle_memory(m: CoreMemory, is_primary: bool) {
    if !is_primary {
        warn!(session = %m.session_id, "hive: rc:hive.memory ignored — not the primary enrollment");
        return;
    }
    match global() {
        Some(sup) => sup.receive_memory(m),
        None => debug!(session = %m.session_id, "hive: rc:hive.memory with no supervisor"),
    }
}

/// FR-90 P1d-2 — the daemon is stopping: an OS stop, a restart it asked for,
/// or an update's. Called first thing on the way out. What its harnesses do
/// from now on is going down with it.
pub fn begin_shutdown() {
    if let Some(s) = global() {
        s.begin_shutdown();
    }
}

/// FR-90 P1h — the daemon is leaving: take down every harness it launched
/// ([`super::procs`]), the way systemd's `KillMode=control-group` takes down
/// a whole unit and the way nothing does under launchd or for an
/// unsupervised daemon. [`begin_shutdown`] first, so each session task reads
/// its harness's exit as going down with the daemon: the session is kept for
/// the next daemon to resume, and nothing is reported. Bounded by
/// [`WIND_DOWN_GRACE`].
pub async fn wind_down() {
    if let Some(s) = global() {
        s.wind_down().await;
    }
}

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
    // P1h-2 — `hive` is in the release builds, so every device runs this. One
    // whose owner never turned agent sessions on keeps no store: nothing is
    // created for it, and a start is refused `hive_disabled` before a store
    // is needed. One that did keeps serving the transcripts its store holds.
    let path = store_path(|file| store_wanted(&hive, file.exists()));
    let store = match &path {
        Ok(Some(p)) => StoreHandle::spawn(Some(p)),
        Ok(None) => Err("agent sessions are off on this device".to_string()),
        Err(e) => Err(e.clone()),
    };
    match (&path, &store) {
        (Ok(None), _) => debug!("hive: agent sessions are off here — no replica store is kept"),
        (_, Err(e)) => {
            warn!(%e, "hive: the replica store is unavailable — every start will be refused")
        }
        _ => {}
    }
    // P1d-2 — what the previous daemon hosted, resumed at the first
    // connection. Without a data directory nothing can be.
    let hosted = match &path {
        Ok(Some(p)) => Hosted::load(p.with_file_name(HOSTED_FILE), &cfg.agent_id),
        _ => Hosted::in_memory(),
    };
    let sup = Supervisor::new(
        hive,
        runtime_root(),
        Launcher::AsMappedAccount,
        store,
        hosted,
    );
    let sup = Arc::new(sup);
    let _ = SUPERVISOR.set(Arc::clone(&sup));
    // P1j — the adopt socket, only while the device's owner allows it; with
    // adopting off, a socket a predecessor left behind is taken away.
    if sup.cfg.adopt {
        tokio::spawn(async move {
            if let Err(e) = sup.adopt_listen().await {
                warn!(%e, "hive: the adopt socket could not be opened — nothing is adopted");
            }
        });
    } else {
        sup.adopt_remove_stale_socket();
    }
}

/// FR-90 P1j — `rc:hive.adopt_ack`: the server's word on an offer. The
/// primary enrollment's only, like every Hive frame.
pub fn handle_adopt_ack(
    adopt_id: &str,
    session_id: Option<ObjectId>,
    fence: Option<u64>,
    refused: Option<roomler_ai_remote_control::hive::HiveAdoptRefusal>,
    is_primary: bool,
) {
    if !is_primary {
        warn!(
            adopt_id,
            "hive: rc:hive.adopt_ack ignored — not the primary enrollment"
        );
        return;
    }
    let answer = match (refused, session_id) {
        (None, Some(sid)) => Ok((sid, fence.unwrap_or(1))),
        (Some(word), _) => Err(word),
        (None, None) => Err(roomler_ai_remote_control::hive::HiveAdoptRefusal::Other),
    };
    if let Some(sup) = global() {
        sup.adopt_answered(adopt_id, answer);
    }
}

/// FR-90 P1j — whether this device's owner allows adopting terminal
/// sessions: what decides whether it advertises `hive-adopt`.
pub fn adopt_enabled() -> bool {
    global().is_some_and(|s| s.cfg.adopt)
}

/// FR-90 P0f — an integration test's supervisor: sessions launch as the
/// daemon's OWN account, because a test process cannot become another one;
/// the store and the settings live where the test says.
///
/// ⚠️ Only with the `hive-test-launcher` feature, which no release build
/// enables — and even then refused when the daemon is root, so this can
/// never be how a session comes to run as root. One per process, like
/// [`init`]: a test that needs it gets a test binary of its own.
#[cfg(all(unix, feature = "hive-test-launcher"))]
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
        Hosted::load(store.with_file_name(HOSTED_FILE), &cfg.agent_id),
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
/// `Ok(None)` when `wanted` says this daemon keeps no store, and then
/// nothing is created (P1h-2).
fn store_path(wanted: impl FnOnce(&Path) -> bool) -> Result<Option<PathBuf>, String> {
    #[cfg(unix)]
    let dir = roomler_node_core::appdirs::project_dirs()
        .map(|p| p.data_local_dir().join("hive"))
        .ok_or("no data directory for the replica store")?;
    // P1i-2 — on Windows the service's machine-wide directory, which the
    // worker reaches whether it is SYSTEM or the console user elevated, so a
    // new worker finds what the last one kept.
    #[cfg(windows)]
    let dir = crate::hive_win::store_dir();
    let file = dir.join("hive.db");
    if !wanted(&file) {
        return Ok(None);
    }
    private_dir(&dir)?;
    Ok(Some(file))
}

/// FR-90 P1h-2 — whether this daemon keeps a replica store: while its owner
/// runs or adopts agent sessions, or once it holds one (whose transcripts
/// are still served after `hive_enabled` goes off). A device that never
/// turned agent sessions on keeps none.
fn store_wanted(hive: &HiveConfig, store_exists: bool) -> bool {
    hive.enabled || hive.adopt || store_exists
}

/// Create `dir` if missing and lock it to the daemon's account. A link on
/// the path that someone other than root could have made is refused, never
/// followed ([`untrusted_link`]).
#[cfg(unix)]
fn private_dir(dir: &Path) -> Result<(), String> {
    if let Some(link) = untrusted_link(dir) {
        return Err(format!("{} is a symbolic link", link.display()));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// P1h-3 — the first link on `path`'s way that someone other than root could
/// have made, or `None`. A link that root owns, in a directory that root owns
/// and that nobody else may write to, could only have been made by root, so
/// following it gives nothing away: macOS's own `/var` → `/private/var` is one,
/// and the root daemon's data directory is under it (`/var/root`).
/// Field-found: with every link refused, as FR-85's
/// [`roomler_node_core::recording_dir::link_component`] refuses them, a Mac
/// opened no store and refused every session "/var is a symbolic link". Any
/// other link on the way is refused, as before.
#[cfg(unix)]
fn untrusted_link(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;
    let mut cur = PathBuf::new();
    for c in path.components() {
        cur.push(c.as_os_str());
        if matches!(c, Component::RootDir | Component::Prefix(_)) {
            continue;
        }
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                let root_made = m.uid() == 0
                    && cur
                        .parent()
                        .and_then(|p| std::fs::metadata(p).ok())
                        .is_some_and(|p| p.uid() == 0 && p.mode() & 0o022 == 0);
                if !root_made {
                    return Some(cur);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    None
}

/// P1i-2 — on Windows: a protected DACL of SYSTEM and Administrators, set as
/// the directory is made (or again, on one already there), which is what
/// `0700` is for a daemon that runs as SYSTEM.
#[cfg(windows)]
fn private_dir(dir: &Path) -> Result<(), String> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    crate::hive_win::dir_with_dacl(dir, crate::hive_win::PRIVATE_DIR_SDDL)
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
        hosted: Hosted,
    ) -> Self {
        let to_resume = hosted.sessions().to_vec();
        let adopted = adopt::AdoptedFile::load(adopt::adopted_path(hosted.path()), hosted.agent());
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
            hosted: Mutex::new(hosted),
            to_resume: Mutex::new(to_resume),
            resumed: tokio::sync::OnceCell::new(),
            going_down: AtomicBool::new(false),
            memory: Mutex::new(HashMap::new()),
            adopted: Mutex::new(adopted),
            adopt_offers: Mutex::new(HashMap::new()),
        }
    }

    /// P1e — keep a session's core memory for its start's launch. A document
    /// larger than any snapshot the server renders is no snapshot: dropped,
    /// and said so.
    pub(crate) fn receive_memory(&self, m: CoreMemory) {
        use roomler_ai_remote_control::hive::hive_limits::MAX_CORE_MEMORY_BYTES;
        let too_big = [&m.claude_md, &m.memory_md]
            .iter()
            .any(|d| d.as_ref().is_some_and(|s| s.len() > MAX_CORE_MEMORY_BYTES));
        if too_big {
            warn!(session = %m.session_id, "hive: core memory larger than any snapshot — dropped");
            return;
        }
        let mut pending = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, (_, at)| at.elapsed() < MEMORY_WAIT);
        if pending.len() >= MEMORY_PENDING
            && let Some(oldest) = pending
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| *k)
        {
            pending.remove(&oldest);
        }
        debug!(session = %m.session_id, rev = m.brain_rev, "hive: core memory received");
        pending.insert((m.session_id, m.fence), (m, Instant::now()));
    }

    /// P1e — the core memory that came for this start, once.
    fn take_memory(&self, session: ObjectId, fence: u64) -> Option<CoreMemory> {
        self.memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(session, fence))
            .map(|(m, _)| m)
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
        self.start_locked(order, is_primary).await
    }

    /// [`Self::start`], under `start_lock`.
    async fn start_locked(self: &Arc<Self>, order: StartOrder, is_primary: bool) -> Answer {
        let answer = self.decide_and_launch(&order, is_primary).await;
        match &answer.refused {
            None => info!(
                session = %order.session_id, account = ?answer.account, caller = %order.caller,
                "hive: session started"
            ),
            Some(r) => {
                info!(
                    session = %order.session_id, refused = r.as_str(), detail = ?answer.detail,
                    caller = %order.caller, "hive: session start refused"
                );
                // P1d-2 — a session refused here does not run here: nothing
                // of it is left to resume.
                if !self.holds_live(order.session_id) {
                    self.hosted_remove(order.session_id);
                }
            }
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
        // P1d-2 — or stopping, for any reason.
        if self.going_down() {
            return Answer::refused(
                HiveRefusal::Other,
                "this device is restarting — start the session again in a minute",
            );
        }
        let account = match gates::account_for(&self.cfg, &order.user_id, &order.user_email) {
            Ok(a) => a,
            Err(r) => {
                return Answer::refused(r, "no hive_accounts entry maps you to a local account");
            }
        };
        // P1d-2 — a session this device hosted resumes as the account it ran
        // as, or not at all: its history is in that account's home, and a
        // remapped starter must not carry it into another.
        if let Some(ran_as) = self
            .hosted_get(order.session_id)
            .filter(|h| h.fence == order.fence)
            .map(|h| h.account)
            && ran_as != account
        {
            return Answer::refused(
                HiveRefusal::NoAccount,
                format!(
                    "hive_accounts now maps the starter to {account}, and the session ran as \
                     {ran_as}, whose home holds its history"
                ),
            );
        }
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
        // Decision 15 — on a Mac, an account whose `sudo` needs no password
        // gives its session root, whatever groups the session drops.
        if matches!(self.launcher, Launcher::AsMappedAccount)
            && let Err((r, detail)) =
                super::sudo::gate(&account, self.cfg.allow_passwordless_sudo).await
        {
            return Answer::refused(r, detail);
        }
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
        // P1d-2 — it is stopping: a turn begun now would be cut, and its
        // number would not reach what the next daemon resumes.
        if self.going_down() {
            return Err("this device is restarting — ask again in a minute".into());
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
        // P1j — an adopted session is stopped by no longer mirroring it.
        if self.adopt_stop(session, fence) {
            return;
        }
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
            // P1d-2 — hosted by the previous daemon and not resumed yet: it
            // ends here, without a launch.
            None if self.take_from_resume(session, fence) => {
                self.hosted_remove(session);
                self.report(
                    session,
                    fence,
                    HiveRunState::Ended,
                    Some(format!("stopped ({reason}) before it resumed")),
                );
            }
            None => self.report(
                session,
                fence,
                HiveRunState::Ended,
                Some("not running on this device".into()),
            ),
        }
    }

    /// P1d-2 — take `session` out of what waits to resume, when it waits
    /// there at `fence` or an older one.
    fn take_from_resume(&self, session: ObjectId, fence: u64) -> bool {
        let sid = session.to_hex();
        let mut q = self.to_resume.lock().unwrap_or_else(|e| e.into_inner());
        let before = q.len();
        q.retain(|h| !(h.session == sid && h.fence <= fence));
        q.len() != before
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
        // Reports go here from now on — a resumed session's among them.
        *self.reporter.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
        // P1b — and every session this device RUNS, now: the server ends the
        // ones it holds as running here that the list leaves out. A daemon
        // that restarted holds no replay of what it ran, so this is the only
        // way the server learns those sessions are over (finding 7).
        //
        // P1d-2 — but only once what the previous daemon hosted has resumed:
        // a manifest sent first would leave those sessions out, and the
        // server would end every one of them. The resume runs in a task of
        // its own, so the manifest's bound on waiting never cancels it
        // halfway through a session.
        let me = Arc::clone(self);
        let resume = tokio::spawn(async move { me.resume_once().await });
        let me = Arc::clone(self);
        tokio::spawn(async move {
            if tokio::time::timeout(RESUME_BUDGET, resume).await.is_err() {
                warn!(
                    "hive: the hosted sessions are still resuming — the manifest goes without them"
                );
            }
            let _ = tx.try_send(ClientMsg::HiveManifest {
                sessions: me.manifest(),
            });
        });
    }

    /// The sessions this device runs now, as the manifest names them.
    fn manifest(&self) -> Vec<HiveManifestEntry> {
        let mut m: Vec<HiveManifestEntry> = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(session_id, l)| HiveManifestEntry {
                session_id: *session_id,
                fence: l.fence,
            })
            .collect();
        // P1j — and the terminal sessions it mirrors: the server ends what
        // the list leaves out.
        m.extend(
            self.adopt_manifest()
                .into_iter()
                .map(|(session_id, fence)| HiveManifestEntry { session_id, fence }),
        );
        m
    }

    /// P1d-2 — resume what the previous daemon hosted: once, at the first
    /// connection of the primary enrollment, so a device that never comes
    /// back online launches nothing. A second connection waits for the same
    /// run.
    async fn resume_once(self: &Arc<Self>) {
        self.resumed
            .get_or_init(|| async {
                let n = self
                    .to_resume
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .len();
                if n > 0 {
                    info!(
                        sessions = n,
                        "hive: resuming what this device hosted before it restarted"
                    );
                }
                loop {
                    let next = {
                        let mut q = self.to_resume.lock().unwrap_or_else(|e| e.into_inner());
                        if q.is_empty() {
                            break;
                        }
                        q.remove(0)
                    };
                    self.resume(next).await;
                }
            })
            .await;
    }

    /// P1d-2 — one hosted session, through every gate a start passes, as the
    /// device is configured NOW: a device owner who turned sessions off,
    /// remapped the starter or moved `hive_roots`, then restarted, does not
    /// find the session back. One the gates refuse is reported ended, with
    /// the reason.
    async fn resume(self: &Arc<Self>, mut h: HostedSession) {
        let (Ok(session), Ok(starter)) = (
            ObjectId::parse_str(&h.session),
            ObjectId::parse_str(&h.starter),
        ) else {
            warn!(session = %h.session, "hive: a hosted session with an unreadable id — not resumed");
            self.hosted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&h.session);
            return;
        };
        // Under the start lock from here to the launch: a start the server
        // re-sent may launch the session first, and its entry is then the
        // live one's to keep.
        let _serial = self.start_lock.lock().await;
        if self.holds_live(session) {
            return;
        }
        let now = unix_now();
        let quick = match h.resumed_at {
            Some(at) if now.saturating_sub(at) < RESUME_STABLE.as_secs() => h.quick_resumes + 1,
            _ => 0,
        };
        if quick >= MAX_QUICK_RESUMES {
            self.not_resumed(
                session,
                h.fence,
                format!(
                    "the daemon restarted {quick} times within {} s of resuming it",
                    RESUME_STABLE.as_secs()
                ),
            );
            return;
        }
        // Counted BEFORE the launch: a resume that takes the daemon down is
        // the one the bound is for.
        // P1h — a harness the previous daemon launched and never took down
        // (it crashed, or was killed outright) is still running on this
        // history: it goes before the resume starts another on it.
        reap_leftover(&h).await;
        h.resumed_at = Some(now);
        h.quick_resumes = quick;
        self.hosted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(h.clone());
        let order = StartOrder {
            session_id: session,
            harness: h.harness,
            harness_session: h.harness_session,
            fence: h.fence,
            folder: h.folder,
            user_id: starter,
            user_email: h.starter_email,
            caller: "this device's resume".into(),
            // Decided by the history on disk, as for every launch.
            resume: false,
        };
        let answer = self.start_locked(order, true).await;
        if answer.refused.is_some() {
            self.not_resumed(
                session,
                h.fence,
                answer.detail.unwrap_or_else(|| "refused".into()),
            );
        }
    }

    /// P1d-2 — a hosted session will not resume: forgotten, and reported
    /// ended with why.
    fn not_resumed(&self, session: ObjectId, fence: u64, why: String) {
        info!(session = %session, %why, "hive: a hosted session was not resumed");
        self.hosted_remove(session);
        self.report(
            session,
            fence,
            HiveRunState::Ended,
            Some(format!("not resumed after the device restarted: {why}")),
        );
    }

    // ─── What the device hosts (P1d-2, `super::hosted`) ─────────────────

    fn hosted_get(&self, session: ObjectId) -> Option<HostedSession> {
        self.hosted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session.to_hex())
            .cloned()
    }

    /// ⚠️ From [`Self::begin_shutdown`] on, what the device hosts is FROZEN:
    /// the teardown cuts turns and withdraws approvals, and the frames saying
    /// so go into connections that are closing. Recorded here, they would be
    /// lost twice — once on the wire, and again because the next daemon
    /// would find nothing left to report (field, 2026-10-08: an approval the
    /// teardown withdrew stayed "needed" on the server).
    fn hosted_update(&self, session: ObjectId, f: impl FnOnce(&mut HostedSession)) {
        if self.going_down() {
            return;
        }
        self.hosted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .update(&session.to_hex(), f);
    }

    fn hosted_remove(&self, session: ObjectId) {
        if self.going_down() {
            return;
        }
        self.hosted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session.to_hex());
    }

    /// P1d-2 — the daemon is stopping (see the module-level
    /// [`begin_shutdown`]).
    pub(crate) fn begin_shutdown(&self) {
        if !self.going_down.swap(true, Ordering::SeqCst) {
            info!(
                sessions = self.live_count(),
                "hive: the daemon is stopping — its sessions are kept for the next start"
            );
        }
    }

    fn going_down(&self) -> bool {
        self.going_down.load(Ordering::SeqCst)
    }

    /// P1h — see [`wind_down`].
    pub(crate) async fn wind_down(&self) {
        self.begin_shutdown();
        let pids = self.harness_pids();
        if !pids.is_empty() {
            info!(
                harnesses = pids.len(),
                "hive: taking the harnesses down with the daemon"
            );
            super::procs::take_down(&pids, WIND_DOWN_GRACE).await;
        }
    }

    /// The pids of the harnesses running now.
    fn harness_pids(&self) -> Vec<u32> {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(|l| l.pid)
            .collect()
    }

    /// P1d-2 — whether a harness that ended without being stopped went down
    /// with the daemon: told so now, or within [`SETTLE`].
    async fn went_down_with_daemon(&self) -> bool {
        if self.going_down() {
            return true;
        }
        tokio::time::sleep(SETTLE).await;
        self.going_down()
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
    #[cfg(all(test, unix))]
    pub(crate) fn with_sidecar(mut self, upstream: String, offline_grace: Duration) -> Self {
        self.upstream = upstream;
        self.offline_grace = offline_grace;
        self
    }

    /// Tests only: an approval that expires in a test's lifetime.
    #[cfg(all(test, unix))]
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
        // P1d-2 — over, so nothing to resume.
        self.hosted_remove(session);
        info!(session = %session, %detail, "hive: session ended");
        self.report(session, fence, HiveRunState::Ended, Some(detail));
    }

    /// P1d-2 — a session resumed after its daemon went down. A turn the
    /// restart cut is reported interrupted, naming who asked; the approvals
    /// it left open are recorded and reported withdrawn; the transcript says
    /// what happened. The file forgets them only once all that is sent.
    fn report_resumed(&self, session: ObjectId, fence: u64, store: &StoreHandle, r: &Resumed) {
        let sid = session.to_hex();
        let mut words = vec!["The device restarted, and this session resumed".to_string()];
        if !r.history {
            words.push(
                "Claude Code's own history of it was not found, so the model starts afresh"
                    .to_string(),
            );
        }
        let cut_turn = r.cut.as_ref().map(|c| c.turn);
        if let Some(cut) = &r.cut {
            self.report_turn(
                session,
                ClientMsg::HiveTurn {
                    session_id: session,
                    fence,
                    turn: cut.turn,
                    status: Some(HiveTurnStatus::Interrupted),
                    prompted_by: cut
                        .prompted_by
                        .as_deref()
                        .and_then(|u| ObjectId::parse_str(u).ok()),
                    steps: 0,
                    duration_ms: None,
                    cost_usd: None,
                },
            );
            words.push(format!(
                "turn {} was cut by the restart — ask again to go on",
                cut.turn
            ));
        }
        for id in &r.approvals {
            store.append(
                &sid,
                fence,
                resolved(id.clone(), toolbelt::Ended::Withdrawn),
            );
            self.report_approval(
                session,
                ClientMsg::HiveApproval {
                    session_id: session,
                    fence,
                    approval_id: id.clone(),
                    turn: cut_turn,
                    status: Some(HiveApprovalStatus::Withdrawn),
                    answered_by: None,
                },
            );
        }
        store.append(
            &sid,
            fence,
            TranscriptEvent::Note {
                text: format!("{}.", words.join("; ")),
            },
        );
        self.hosted_update(session, |h| {
            h.running = None;
            h.approvals.clear();
        });
    }

    /// P1d-2 — the session's harness went down with the daemon: let go of
    /// it here and say nothing, exactly as a daemon killed outright would.
    /// What the device hosts keeps it, and the next daemon resumes it.
    fn let_go(&self, session: ObjectId, token: Option<&str>) {
        if let Some(token) = token {
            self.tokens.revoke(token);
        }
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session);
        info!(session = %session, "hive: the session went down with the daemon — kept for the next start");
    }

    fn launch(
        self: &Arc<Self>,
        order: &StartOrder,
        account: &str,
        folder: &Path,
        store: StoreHandle,
        sidecar: Option<u16>,
    ) -> Result<(), (HiveRefusal, String)> {
        // P1d-2 — a session this device hosted goes on where it stood.
        let prior = self
            .hosted_get(order.session_id)
            .filter(|h| h.fence == order.fence);
        // P1e — the core memory this start came with: shown to the session
        // only where the device's owner allows it (`hive_core_memory`).
        let memory = self.take_memory(order.session_id, order.fence);
        let shown = memory.as_ref().filter(|_| self.cfg.core_memory);
        let (approvals_tx, approvals_rx) = mpsc::channel(APPROVAL_QUEUE);
        let Spawned {
            child,
            token,
            toolbelt,
            history,
        } = self.spawn(order, account, folder, sidecar, approvals_tx, shown)?;
        // P1h — who the harness is, for whoever must take it down later: this
        // daemon when it leaves, or the next one after a crash.
        let pid = child.id();
        let started = pid.and_then(super::procs::started);
        if let Some(m) = &memory {
            store.append(
                &order.session_id.to_hex(),
                order.fence,
                TranscriptEvent::Note {
                    text: memory_note(m, self.cfg.core_memory),
                },
            );
        }
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
                pid,
            },
        );
        if prior.is_some() {
            self.hosted_update(order.session_id, |h| {
                h.harness_pid = pid;
                h.harness_started = started.clone();
            });
        }
        let resumed = match prior {
            Some(h) => Some(Resumed {
                turns: h.turns,
                cut: h.running,
                approvals: h.approvals,
                history,
            }),
            None => {
                self.hosted
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .put(HostedSession {
                        session: order.session_id.to_hex(),
                        fence: order.fence,
                        harness: order.harness.clone(),
                        harness_session: order.harness_session.clone(),
                        folder: order.folder.clone(),
                        account: account.to_string(),
                        starter: order.user_id.to_hex(),
                        starter_email: order.user_email.clone(),
                        turns: 0,
                        running: None,
                        approvals: Vec::new(),
                        resumed_at: None,
                        quick_resumes: 0,
                        harness_pid: pid,
                        harness_started: started.clone(),
                    });
                None
            }
        };
        // Up and waiting for its first prompt.
        self.report(order.session_id, order.fence, HiveRunState::Idle, None);
        // P1d-2 — and what its last daemon left behind, here, before the
        // launch returns: a connection's manifest goes out once the resume
        // has, and follows all of it.
        if let Some(r) = &resumed {
            self.report_resumed(order.session_id, order.fence, &store, r);
        }
        let sup = Arc::clone(self);
        let (session, fence) = (order.session_id, order.fence);
        tokio::spawn(async move {
            let inputs = Inputs {
                rx: input_rx,
                waiting,
                approvals: approvals_rx,
                toolbelt,
            };
            match run(&sup, session, fence, child, inputs, store, resumed).await {
                Some(detail) => sup.finish(session, fence, token.as_deref(), detail),
                None => sup.let_go(session, token.as_deref()),
            }
        });
        Ok(())
    }

    /// Spawn the harness as the session's account, through the wrapper; with
    /// the sidecar, also the model token this run holds; and the session's
    /// toolbelt, bound before the harness starts because the harness
    /// connects to it as it starts.
    #[cfg(unix)]
    fn spawn(
        &self,
        order: &StartOrder,
        account: &str,
        folder: &Path,
        sidecar: Option<u16>,
        approvals: mpsc::Sender<ApprovalEvent>,
        memory: Option<&CoreMemory>,
    ) -> Result<Spawned, (HiveRefusal, String)> {
        // The account the session runs as: its home, its toolbelt socket's ids
        // (`None`: it runs as the daemon), and the groups the SESSION holds,
        // without its administrator groups (decision 13): what the relay needs.
        let (home, owner, uid, groups) = match &self.launcher {
            Launcher::AsMappedAccount => {
                let home =
                    crate::exec::account_home(account).map_err(|e| (HiveRefusal::NoAccount, e))?;
                let (uid, gid, mut groups) = crate::exec::session_account_ids(account)
                    .map_err(|e| (HiveRefusal::LaunchFailed, e))?;
                groups.push(gid);
                (home, Some((uid, gid)), uid, Some(groups))
            }
            #[cfg(all(unix, any(test, feature = "hive-test-launcher")))]
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
        let mut spec = LaunchSpec {
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
        // P1d-2 — the session's own history decides, for every launch (a
        // resume, and a start the server re-sent after a crash): Claude Code
        // refuses `--session-id` for an id whose history exists, and
        // `--resume` for one without. The daemon only looks at the entry,
        // never reads or follows it; the harness reads it as the account.
        spec.resume = order.resume || std::fs::symlink_metadata(spec.history_path()).is_ok();

        let mut cmd = tokio::process::Command::new("/bin/sh");
        // P1e — the core memory, into the daemon's own directory; the wrapper
        // copies it in as the account. One that cannot be written is said
        // in the log, and the session starts without it.
        let memory_dir = memory.and_then(|m| match write_memory(&dir, m, owner) {
            Ok(d) => Some(d),
            Err(e) => {
                warn!(session = %order.session_id, %e, "hive: core memory not written — the session starts without it");
                None
            }
        });
        cmd.arg("-c")
            .arg(WRAPPER)
            .arg("roomler-hive")
            .arg(spec.config_dir())
            .arg(&spec.folder)
            .arg(
                memory_dir
                    .as_deref()
                    .map_or_else(Default::default, |d| d.as_os_str().to_os_string()),
            )
            .arg(spec.auto_memory_dir())
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
            // The one privilege path exec, SSH and the PTY share (verified,
            // uid 0 refused), as an agent session runs (decision 13): none of
            // the account's administrator groups, and on Linux no privilege
            // gained by exec, so no `sudo` whatever sudoers says of it.
            Launcher::AsMappedAccount => crate::exec::apply_session_run_as(&mut cmd, account)
                .map_err(|e| (HiveRefusal::LaunchFailed, e))?,
            #[cfg(all(unix, any(test, feature = "hive-test-launcher")))]
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
                history: spec.resume,
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

    /// P1i-2 — the same on Windows, as the user signed in at the console, whom
    /// `account` must name (D2: nobody else, never SYSTEM). `roomlerd
    /// hive-prep` does the wrapper's work as that user first. The harness then
    /// starts SUSPENDED into a Job Object of its own and is resumed only once
    /// it is in it, with the folder as its working directory.
    #[cfg(windows)]
    fn spawn(
        &self,
        order: &StartOrder,
        account: &str,
        folder: &Path,
        sidecar: Option<u16>,
        approvals: mpsc::Sender<ApprovalEvent>,
        memory: Option<&CoreMemory>,
    ) -> Result<Spawned, (HiveRefusal, String)> {
        use crate::hive_win;
        use crate::win_service::supervisor::{JobObject, spawn_into_job};
        let failed = |what: &str, e: String| (HiveRefusal::LaunchFailed, format!("{what}: {e}"));
        // The one way on Windows: as the console user. `console` holds the
        // user's token, which `who` only borrows, until both processes run.
        let Launcher::AsMappedAccount = &self.launcher;
        let console = hive_win::console_user()?;
        hive_win::check_mapping(account, &console)?;
        let who = console.spawn_as();
        let (home, peer, appdata) = (
            console.profile.clone(),
            console.sid.clone(),
            console.appdata.clone(),
        );
        let harness =
            hive_win::resolve_harness(self.cfg.harness.as_deref(), &home, appdata.as_deref())
                .ok_or_else(|| {
                    (
                        HiveRefusal::HarnessMissing,
                        "Claude Code was not found (set hive_harness, or install it with its \
                         native installer, into %USERPROFILE%\\.local\\bin, or with npm)"
                            .to_string(),
                    )
                })?;
        let sid = order.session_id.to_hex();
        let dir = self.runtime.join(&sid);
        session_dir(&self.runtime, &dir, &peer)
            .map_err(|e| failed("the session's runtime directory", e))?;
        let settings = write_settings(&self.runtime, &sid)
            .map_err(|e| failed("writing the session settings", e))?;
        let relay = own_exe().map_err(|e| (HiveRefusal::LaunchFailed, e))?;
        let toolbelt = toolbelt::open(peer, order.session_id, approvals, self.approval_timing)
            .map_err(|e| failed("opening the session's toolbelt", e))?;
        let mcp = toolbelt_mcp_config(
            &relay,
            &[
                toolbelt::RELAY_SUBCOMMAND.to_string(),
                toolbelt.socket().to_string_lossy().into_owned(),
            ],
            toolbelt::TOOL_TIMEOUT_MS,
        );
        let mcp_config = write_doc(&dir, "mcp.json", &mcp)
            .map_err(|e| failed("writing the session's MCP config", e))?;
        let mut spec = LaunchSpec {
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
        spec.resume = order.resume || std::fs::symlink_metadata(spec.history_path()).is_ok();
        let line = hive_win::harness_command_line(&spec.harness, &spec.args())
            .map_err(|e| (HiveRefusal::LaunchFailed, e))?;
        let memory_dir = memory.and_then(|m| match write_memory(&dir, m, None) {
            Ok(d) => Some(d),
            Err(e) => {
                warn!(session = %order.session_id, %e, "hive: core memory not written — the session starts without it");
                None
            }
        });
        // The wrapper's work, as the user: the config directory, the memory
        // copied in, the folder opened.
        let prep = hive_win::PrepArgs {
            config_dir: spec.config_dir(),
            folder: spec.folder.clone(),
            memory_dir,
            auto_memory_dir: spec.auto_memory_dir(),
        }
        .command_line(Path::new(&relay))
        .map_err(|e| (HiveRefusal::LaunchFailed, e))?;
        // SAFETY: `console` holds the token `who` names, across the call.
        let prepared =
            blocking(|| unsafe { hive_win::run_prep(who, &prep, hive_win::PREP_TIMEOUT) });
        prepared.map_err(|e| failed("preparing the session as its account", e))?;
        let job = JobObject::kill_on_close()
            .map_err(|e| failed("making the harness's job", format!("{e:#}")))?;
        let token = sidecar.map(|_| self.tokens.mint(order.session_id, order.fence));
        let mut env = spec.env_overrides();
        if let Some(token) = &token {
            env.push(("ANTHROPIC_API_KEY".into(), token.into()));
        }
        // SAFETY: `console` holds the token `who` names, across the call.
        let started = unsafe { spawn_into_job(who, &line, Some(&spec.folder), &env, true, &job) };
        drop(console);
        match started {
            Ok(child) => Ok(Spawned {
                child: hive_win::HarnessChild::new(child, job),
                token,
                toolbelt,
                history: spec.resume,
            }),
            Err(e) => {
                if let Some(token) = &token {
                    self.tokens.revoke(token);
                }
                Err(failed("starting the harness", format!("{e:#}")))
            }
        }
    }
}

/// P1i-2 — run `f`, which blocks for as long as a short preparation takes,
/// without stalling the other tasks of a multi-thread runtime.
#[cfg(windows)]
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// P1i-2 — a session's runtime directory on Windows, under a root that is
/// SYSTEM's and Administrators' alone; the session's own directory is also
/// readable by its account (`peer`). Each launch gives each its DACL again.
/// P1i-4 — the account may examine the two above it, and nothing more
/// ([`crate::hive_win::PRIVATE_DIR_SDDL`]): Claude Code refuses a settings
/// file whose path it cannot examine.
#[cfg(windows)]
fn session_dir(runtime: &Path, dir: &Path, peer: &str) -> Result<(), String> {
    use crate::hive_win::{PRIVATE_DIR_SDDL, dir_with_dacl, session_dir_sddl};
    if let Some(hive) = runtime.parent() {
        if let Some(base) = hive.parent() {
            std::fs::create_dir_all(base).map_err(|e| format!("{}: {e}", base.display()))?;
        }
        dir_with_dacl(hive, PRIVATE_DIR_SDDL)?;
    }
    dir_with_dacl(runtime, PRIVATE_DIR_SDDL)?;
    dir_with_dacl(dir, &session_dir_sddl(peer))
}

/// `hive_harness`, or the first default location that holds an executable
/// file.
#[cfg(unix)]
fn resolve_harness(cfg: &HiveConfig, home: &Path) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = match &cfg.harness {
        Some(h) => vec![h.clone()],
        None => vec![
            home.join(".local/bin/claude"),
            PathBuf::from("/usr/local/bin/claude"),
            // P1h — Homebrew's prefix on Apple silicon.
            #[cfg(target_os = "macos")]
            PathBuf::from("/opt/homebrew/bin/claude"),
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
///
/// On Windows both directories are made first, each with its DACL
/// ([`session_dir`]), and the file takes the session directory's.
fn write_settings(runtime: &Path, sid: &str) -> Result<PathBuf, String> {
    let dir = runtime.join(sid);
    #[cfg(unix)]
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
    #[cfg(unix)]
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// P1e — `<runtime>/<sid>/memory/`, daemon-owned: the snapshot's files, each
/// `0600` and handed to the session's account (`owner`; `None`: the session
/// runs as the daemon), in a `0755` directory the account cannot change. The
/// account reads them to copy them in; no other local account can, since a
/// session's `CLAUDE.md` carries its starter's own facts. A document the
/// snapshot does not carry is removed, so a file left by an earlier run can
/// never be copied for this one. A link in its place is refused, never
/// followed.
fn write_memory(dir: &Path, m: &CoreMemory, owner: Option<(u32, u32)>) -> Result<PathBuf, String> {
    let mem = dir.join("memory");
    match std::fs::symlink_metadata(&mem) {
        Ok(md) if !md.file_type().is_dir() => {
            return Err(format!("{} is not a plain directory", mem.display()));
        }
        Ok(_) => {}
        Err(_) => std::fs::create_dir(&mem).map_err(|e| format!("{}: {e}", mem.display()))?,
    }
    // On Windows it takes the session directory's DACL: the daemon's, and
    // readable by the session's account alone.
    #[cfg(unix)]
    std::fs::set_permissions(&mem, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("{}: {e}", mem.display()))?;
    for (name, body) in [("CLAUDE.md", &m.claude_md), ("MEMORY.md", &m.memory_md)] {
        match body {
            Some(text) => {
                write_private_text(&mem, name, text, owner)?;
            }
            None => match std::fs::remove_file(mem.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", mem.join(name).display())),
            },
        }
    }
    Ok(mem)
}

/// `dir/name`, whole: written to a temporary name created afresh at `0600`
/// (never through a link, nor into a file an earlier run left there),
/// handed to `owner` through the open file, then renamed into place — so it
/// is never readable by anyone else, not even while it is being written.
fn write_private_text(
    dir: &Path,
    name: &str,
    text: &str,
    owner: Option<(u32, u32)>,
) -> Result<PathBuf, String> {
    use std::io::Write;
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    let failed = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(failed(&tmp, e)),
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(|e| failed(&tmp, e))?;
    f.write_all(text.as_bytes()).map_err(|e| failed(&tmp, e))?;
    #[cfg(unix)]
    {
        f.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| failed(&tmp, e))?;
        if let Some((uid, gid)) = owner {
            std::os::unix::fs::fchown(&f, Some(uid), Some(gid)).map_err(|e| failed(&tmp, e))?;
        }
    }
    // On Windows the directory's DACL is the handover: the file is the
    // daemon's, and readable by the session's account alone.
    #[cfg(windows)]
    let _ = owner;
    drop(f);
    std::fs::rename(&tmp, &path).map_err(|e| failed(&path, e))?;
    Ok(path)
}

/// P1e — what the transcript says about the core memory a start came with.
fn memory_note(m: &CoreMemory, shown: bool) -> String {
    if !shown {
        return format!(
            "This start came with the organization's core memory (brain revision {}); this \
             device shows none to its sessions (hive_core_memory is off).",
            m.brain_rev
        );
    }
    let mut files = Vec::new();
    if m.claude_md.is_some() {
        files.push("CLAUDE.md");
    }
    if m.memory_md.is_some() {
        files.push("the auto-memory MEMORY.md");
    }
    if files.is_empty() {
        return format!(
            "Core memory: brain revision {}, with nothing in it for this session.",
            m.brain_rev
        );
    }
    format!(
        "Core memory from the organization's brain, revision {}: {}.",
        m.brain_rev,
        files.join(" and ")
    )
}

/// Whether the account `uid` (with `groups`) may execute `path`: `x` on the
/// file and on every directory above it — by the owner's bits when it owns
/// the entry, else the group's when one of its groups does, else everyone's;
/// the kernel's order. (A POSIX ACL that grants more is not read: refusing a
/// start it would have allowed is the safe mistake.)
#[cfg(unix)]
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
    /// The harness process's running cost at its last turn's end. `None`
    /// until a resumed process's first turn ends (P1d-2): Claude Code takes
    /// its running total back from the history's last `cost-state`, which
    /// only a clean exit writes, so where that process starts counting is
    /// not known here, and its first turn reports no cost rather than a
    /// wrong one.
    spent: Option<f64>,
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
        let opened = matches!(ev, ApprovalEvent::Opened { .. });
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
        // P1d-2 — an approval is on disk before the server hears it opened,
        // and comes off after it hears how it ended: lost in between, the
        // resume withdraws it again, which the server applies only to one
        // still open.
        if opened {
            self.save_open_approvals();
        }
        self.store.append(&self.sid, self.fence, event);
        self.report_approval(id, status, answered_by);
        if !opened {
            self.save_open_approvals();
        }
        self.settle_state();
        let _ = self
            .sup
            .approvals
            .send((self.session, self.open_approvals.clone()));
    }

    fn save_open_approvals(&self) {
        let open = self.open_approvals.clone();
        self.sup.hosted_update(self.session, |h| h.approvals = open);
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
        // P1d-2 — the turn's number is on disk before its stub goes out.
        let n = self.count;
        self.sup.hosted_update(self.session, |h| {
            h.turns = n;
            h.running = Some(RunningTurn {
                turn: n,
                prompted_by: prompted_by.map(|u| u.to_hex()),
            });
        });
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
        if let TranscriptEvent::Turn { cost_usd, .. } = &mut ev
            && let Some(total) = *cost_usd
        {
            debug!(session = %self.session, total, base = ?self.spent, "hive: the harness's running cost");
            *cost_usd = self
                .spent
                .map(|base| ((total - base) * 1e6).round() / 1e6)
                .map(|this_turn| this_turn.max(0.0));
            self.spent = Some(total);
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
            // P1d-2 — over before its stub says so: the server edits a stub
            // to whatever comes last, so a resume must never report a turn
            // that finished as cut.
            self.sup.hosted_update(self.session, |h| h.running = None);
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

/// Own the session until its harness exits; return the `ended` detail —
/// or `None` when the harness went down with the daemon (P1d-2), and the
/// session is the next daemon's to resume.
async fn run(
    sup: &Supervisor,
    session: ObjectId,
    fence: u64,
    mut child: Child,
    inputs: Inputs,
    store: StoreHandle,
    resumed: Option<Resumed>,
) -> Option<String> {
    let Inputs {
        rx: mut input,
        waiting,
        mut approvals,
        toolbelt,
    } = inputs;
    let Some(stdout) = child.stdout.take() else {
        return Some("the harness has no stdout".into());
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
        // P1d-2 — a resumed session counts on from its last turn: the
        // server ignores a stub for a turn older than its newest.
        count: resumed.as_ref().map_or(0, |r| r.turns),
        current: None,
        queued: Default::default(),
        waiting,
        // A process with no history counts from zero; one that resumed its
        // history, from wherever that history's last `cost-state` puts it.
        spent: match &resumed {
            Some(r) if r.history => None,
            _ => Some(0.0),
        },
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
            // recorded before the approval it asked for. P1d-2 — not once the
            // daemon is stopping: the harness letting go then is the teardown,
            // and the next daemon records and reports the end of each
            // approval it cut, exactly once.
            Some(ev) = approvals.recv() => {
                if !sup.going_down() {
                    task.on_approval(ev);
                }
            }
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

    // P1d-2 — a harness that ended without being stopped may have gone down
    // with the daemon: systemd stops the whole unit, the daemon first and the
    // harness a moment later. Then the session is not over; it is the next
    // daemon's to resume, and this one says nothing — no `ended`, no cut
    // turn, no withdrawn approval — exactly as if it had been killed
    // outright. The next daemon reports all of it from what the device
    // hosts.
    if stopped.is_none() && sup.went_down_with_daemon().await {
        return None;
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
    Some(describe_end(stopped.as_deref(), status, said))
}

/// Seconds since the epoch, for what the device hosts.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The last [`STDERR_TAIL`] characters a harness wrote to stderr, on one line.
async fn read_stderr_tail(session: ObjectId, err: impl AsyncRead + Unpin) -> String {
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

/// P1h — the harness `h` records, if that very process is still running: the
/// same pid AND the same start time ([`super::procs::started`]), so whatever
/// has the pid since is never touched. A record from before P1h carries
/// neither, and nothing is done.
async fn reap_leftover(h: &HostedSession) {
    let (Some(pid), Some(was)) = (h.harness_pid, h.harness_started.as_deref()) else {
        return;
    };
    if super::procs::started(pid).as_deref() != Some(was) {
        return;
    }
    info!(session = %h.session, pid, "hive: the previous daemon left its harness running — taken down before the resume");
    super::procs::take_down(&[pid], STOP_GRACE).await;
}

/// SIGTERM to the harness's process group, then SIGKILL after the grace.
#[cfg(unix)]
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

/// P1i-2 — on Windows no signal reaches a console-less harness. Its stdin is
/// closed already, which ends a stream-json harness; one still there after
/// the grace is ended with everything in its job.
#[cfg(windows)]
async fn terminate(child: &mut Child) {
    if tokio::time::timeout(STOP_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
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
    #[cfg(unix)]
    let signal = |s: std::process::ExitStatus| {
        use std::os::unix::process::ExitStatusExt;
        s.signal()
    };
    // A Windows process always ends with a code, ended or not.
    #[cfg(windows)]
    let signal = |_: std::process::ExitStatus| None::<i32>;
    if let Some(reason) = stopped {
        return format!("stopped ({reason})");
    }
    let mut detail = match status.map(|s| (s.code(), signal(s))) {
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

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;

    /// P1h-3 — a link root made, in a directory only root writes to, is
    /// followed, where FR-85's check refuses it; a link anyone else could have
    /// made is refused, and named. The system link is macOS's own `/var`, or
    /// `/var/run` on Ubuntu: the first link this host's root owns.
    #[test]
    fn a_link_root_made_is_followed_and_any_other_refused() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let system = ["/var", "/var/run", "/lib", "/bin", "/sbin", "/tmp", "/etc"]
            .into_iter()
            .map(Path::new)
            .find(|p| {
                std::fs::symlink_metadata(p)
                    .is_ok_and(|m| m.file_type().is_symlink() && m.uid() == 0)
            });
        if let Some(link) = system {
            let under = link.join("roomler-p1h3-probe");
            assert_eq!(
                untrusted_link(&under),
                None,
                "{} is root's own",
                link.display()
            );
            assert_eq!(
                roomler_node_core::recording_dir::link_component(&under).as_deref(),
                Some(link),
                "FR-85's check, unchanged, still refuses every link"
            );
        }

        // A link in a directory anyone may write to: whoever runs this test,
        // root included, could not tell who made it.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(untrusted_link(&link.join("hive")), Some(link.clone()));
        let refused = private_dir(&link.join("hive")).unwrap_err();
        assert!(refused.contains("is a symbolic link"), "{refused}");
        assert!(!real.join("hive").exists(), "nothing made through the link");
        assert_eq!(untrusted_link(&real.join("hive")), None);
    }

    /// P1h-2 — `hive` is in the release builds, so every device starts a
    /// supervisor: one whose owner never turned agent sessions on keeps no
    /// store, while one that runs or adopts them does, and so does one that
    /// holds a store already (its transcripts are still served).
    #[test]
    fn a_device_that_never_turned_agent_sessions_on_keeps_no_store() {
        let closed = HiveConfig::closed();
        assert!(!store_wanted(&closed, false), "nothing to keep");
        assert!(store_wanted(&closed, true), "a store kept from before");
        let enabled = HiveConfig {
            enabled: true,
            ..HiveConfig::closed()
        };
        assert!(store_wanted(&enabled, false));
        let adopting = HiveConfig {
            adopt: true,
            ..HiveConfig::closed()
        };
        assert!(store_wanted(&adopting, false));
    }

    /// A stand-in for Claude Code that speaks just enough stream-json. Like
    /// the headless harness reading stream-json input, it says `init` once
    /// its first prompt arrives, then answers each prompt with a text block
    /// and a turn result. A prompt containing `crash` exits 3 with a word on
    /// stderr; `slow` takes a second; `tool` makes one tool call first;
    /// `hold` waits until the folder holds a `.release` file (and takes it),
    /// which is how a test keeps a turn running while it asks for an
    /// approval. Its `total_cost_usd`, like Claude Code's, is the PROCESS's
    /// running total: 0.25 more at every turn. Its argv is kept in the
    /// folder's `.argv`, one argument a line, every line it reads on stdin
    /// in `.stdin` — what reached the harness (P1c-2) — and its pid in
    /// `.pid`.
    ///
    /// P1e — before anything else, it copies the core memory it finds where
    /// Claude Code reads it (`$CLAUDE_CONFIG_DIR/CLAUDE.md`, the auto-memory
    /// `MEMORY.md`) to the folder's `.claude_md` and `.memory_md`: what this
    /// harness was given.
    ///
    /// P1d-2 — like Claude Code it keeps the session's history, from the
    /// first prompt on, at the path `LaunchSpec::history_path` names, and
    /// refuses the same two ways: `--session-id` for an id whose history
    /// exists, `--resume` for one without. `recall` answers with how many
    /// prompts its history holds, so a test sees what a resumed process
    /// remembers.
    pub(crate) const FAKE_HARNESS: &str = r#"#!/bin/sh
[ -f "$CLAUDE_CONFIG_DIR/CLAUDE.md" ] && cp "$CLAUDE_CONFIG_DIR/CLAUDE.md" "$PWD/.claude_md"
[ -f "$CLAUDE_CONFIG_DIR/projects/$CLAUDE_CODE_PROJECT_DIR_NAME/memory/MEMORY.md" ] && cp "$CLAUDE_CONFIG_DIR/projects/$CLAUDE_CODE_PROJECT_DIR_NAME/memory/MEMORY.md" "$PWD/.memory_md"
printf '%s\n' "$@" > "$PWD/.argv"
echo $$ > "$PWD/.pid"
mode=""; sid=""; prev=""
for a in "$@"; do
  case "$prev" in --session-id|--resume) mode="$prev"; sid="$a" ;; esac
  prev="$a"
done
hist="$CLAUDE_CONFIG_DIR/projects/$CLAUDE_CODE_PROJECT_DIR_NAME/$sid.jsonl"
if [ "$mode" = "--session-id" ] && [ -e "$hist" ]; then
  echo "Error: Session ID $sid is already in use." >&2; exit 1
fi
if [ "$mode" = "--resume" ] && [ ! -e "$hist" ]; then
  echo "No conversation found with session ID: $sid" >&2; exit 1
fi
first=1
spent=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$PWD/.stdin"
  mkdir -p "${hist%/*}" && printf '%s\n' "$line" >> "$hist"
  case "$line" in
    *crash*) echo "boom" >&2; exit 3 ;;
  esac
  case "$line" in
    *recall*)
      n=$(wc -l < "$hist" | tr -d ' ')
      echo '{"type":"assistant","message":{"content":[{"type":"text","text":"history: '"$n"'"}]}}'
      ;;
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
        // Under /tmp, not $TMPDIR: on macOS that is /var/folders/…/T/, about
        // 50 characters, and a session's socket (`run/<session>/toolbelt.sock`)
        // beneath it would come within a byte of `sun_path`'s 104 (P1h-1).
        let root = tempfile::Builder::new()
            .prefix("hive")
            .tempdir_in("/tmp")
            .unwrap();
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
            core_memory: false,
            adopt: false,
            allow_passwordless_sudo: false,
        };
        cfg_with(&mut cfg);
        let store = StoreHandle::spawn(None).unwrap();
        let sup = Arc::new(sup_with(Supervisor::new(
            cfg,
            root.path().join("run"),
            Launcher::AsDaemon { home },
            Ok(store.clone()),
            Hosted::load(root.path().join(HOSTED_FILE), TEST_AGENT),
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

    /// The enrollment every rig's hosted file belongs to.
    const TEST_AGENT: &str = "agent-test";

    /// P1d-2 — the rig's daemon goes down the way systemd stops it: told
    /// first, then every harness stopped by the same signal a moment later.
    async fn go_down(r: &Rig, sid: ObjectId) {
        r.sup.begin_shutdown();
        // A turn can be `running` before the harness ran a line: the prompt
        // waits in its stdin, and an approval goes through the daemon's own
        // toolbelt.
        let pid_file = r.root.path().join("work").join(".pid");
        let deadline = Instant::now() + Duration::from_secs(10);
        let pid: i32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "the harness never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        // SAFETY: a signal to the fake harness's own process group.
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while r.sup.holds_live(sid) {
            assert!(Instant::now() < deadline, "the session never let go");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// P1d-2 — the next daemon on the rig's device: the same hosted file,
    /// store, runtime and accounts, with `cfg_with` applied to the config.
    /// Not connected yet ([`connect`]).
    fn next_daemon(r: &mut Rig, cfg_with: impl FnOnce(&mut HiveConfig)) {
        let mut cfg = r.sup.cfg.clone();
        cfg_with(&mut cfg);
        r.sup = Arc::new(Supervisor::new(
            cfg,
            r.root.path().join("run"),
            r.sup.launcher.clone(),
            Ok(r.store.clone()),
            Hosted::load(r.root.path().join(HOSTED_FILE), TEST_AGENT),
        ));
    }

    /// A new connection of the rig's daemon; its reports replace the old.
    fn connect(r: &mut Rig) {
        let (tx, reports) = mpsc::channel(64);
        r.sup.connected(tx);
        r.reports = reports;
    }

    /// [`next_daemon`], then its first connection — which resumes what the
    /// last one hosted.
    fn restart(r: &mut Rig, cfg_with: impl FnOnce(&mut HiveConfig)) {
        next_daemon(r, cfg_with);
        connect(r);
    }

    /// Every report up to the connection's manifest, and the manifest.
    async fn until_manifest(r: &mut Rig) -> (Vec<ClientMsg>, Vec<(ObjectId, u64)>) {
        let mut seen = Vec::new();
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), r.reports.recv())
                .await
                .expect("a manifest within 10 s")
                .expect("the connection is open");
            if let ClientMsg::HiveManifest { sessions } = &msg {
                let m = sessions.iter().map(|e| (e.session_id, e.fence)).collect();
                return (seen, m);
            }
            seen.push(msg);
        }
    }

    /// Nothing reported for `sid` in the next moment: what a daemon that
    /// went down says about a session.
    async fn silent_about(r: &mut Rig, sid: ObjectId) {
        while let Ok(Some(msg)) =
            tokio::time::timeout(Duration::from_millis(400), r.reports.recv()).await
        {
            let about = match &msg {
                ClientMsg::HiveState { session_id, .. }
                | ClientMsg::HiveTurn { session_id, .. }
                | ClientMsg::HiveApproval { session_id, .. } => *session_id == sid,
                _ => false,
            };
            assert!(!about, "a daemon going down reports nothing: {msg:?}");
        }
    }

    /// P1d-2 — a hosted entry for `sid` on the rig's device, as a daemon
    /// that went down left it: the rig's starter, mapped to `dev`.
    fn host(r: &Rig, sid: ObjectId, f: impl FnOnce(&mut HostedSession)) {
        let o = order(r);
        let mut h = HostedSession {
            session: sid.to_hex(),
            fence: 1,
            harness: o.harness,
            harness_session: o.harness_session,
            folder: o.folder,
            account: "dev".into(),
            starter: r.user.to_hex(),
            starter_email: o.user_email,
            turns: 0,
            running: None,
            approvals: Vec::new(),
            resumed_at: None,
            quick_resumes: 0,
            harness_pid: None,
            harness_started: None,
        };
        f(&mut h);
        Hosted::load(r.root.path().join(HOSTED_FILE), TEST_AGENT).put(h);
    }

    /// Before a launch whose argv a test reads: the last one's goes. (Not a
    /// modification time: the kernel stamps files from its coarse clock,
    /// which can run behind the test's `SystemTime::now()`.)
    fn forget_argv(r: &Rig) {
        let _ = std::fs::remove_file(r.root.path().join("work").join(".argv"));
    }

    /// The fake harness's argv from the launch after [`forget_argv`].
    async fn next_argv(r: &Rig) -> Vec<String> {
        let file = r.root.path().join("work").join(".argv");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if std::fs::metadata(&file).is_ok_and(|m| m.len() > 0) {
                tokio::time::sleep(Duration::from_millis(50)).await;
                return std::fs::read_to_string(&file)
                    .unwrap()
                    .lines()
                    .map(str::to_string)
                    .collect();
            }
            assert!(Instant::now() < deadline, "the harness never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// What the rig's hosted file holds now, by session.
    fn hosted_on_disk(r: &Rig) -> Vec<HostedSession> {
        Hosted::load(r.root.path().join(HOSTED_FILE), TEST_AGENT)
            .sessions()
            .to_vec()
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
        assert!(
            hosted_on_disk(&r).is_empty(),
            "a stop leaves nothing to resume"
        );

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
        // P1d-2 — a harness that ends while the daemon runs ends its
        // session: there is nothing to resume.
        assert!(hosted_on_disk(&r).is_empty());
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
        // Under /tmp, opened up: the walk reads EVERY directory to the root,
        // and macOS's $TMPDIR is a 0700 per-user directory, which no other
        // account passes whatever the bits below it say (P1h-1, measured on
        // the macOS runner).
        let root = tempfile::Builder::new()
            .prefix("hive")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
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

    // ─── P1d-2 — a restart resumes what the device hosted ───────────────

    /// The whole arc: a session runs a turn; the daemon goes down the way
    /// systemd stops it and reports nothing; the next daemon's first
    /// connection relaunches it with `--resume`, before a manifest that
    /// names it; the history is the harness's own; the turns count on; the
    /// first turn of the resumed process costs what nobody here can know,
    /// the next one what it cost; a stop forgets it.
    #[tokio::test]
    async fn a_restarted_daemon_resumes_what_it_hosted_and_counts_on() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let (sid, uuid) = (o.session_id, o.harness_session.clone());
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        r.sup.prompt(sid, dev(&r), "say hello".into()).unwrap();
        let (turns, _, _) = reports_until(&mut r, sid, HiveRunState::Idle).await;
        assert!(turns.iter().all(|t| t.turn == 1), "{turns:?}");
        let on_disk = hosted_on_disk(&r);
        assert_eq!(on_disk.len(), 1);
        assert_eq!((on_disk[0].turns, on_disk[0].running.as_ref()), (1, None));

        go_down(&r, sid).await;
        silent_about(&mut r, sid).await;
        assert_eq!(hosted_on_disk(&r).len(), 1, "kept for the next daemon");

        forget_argv(&r);
        restart(&mut r, |_| {});
        let (seen, manifest) = until_manifest(&mut r).await;
        assert!(
            seen.iter().any(|m| matches!(m,
                ClientMsg::HiveState { session_id, state: Some(HiveRunState::Idle), .. }
                    if *session_id == sid)),
            "resumed before the manifest: {seen:?}"
        );
        assert_eq!(manifest, [(sid, 1)], "the manifest names it");
        let argv = next_argv(&r).await;
        assert!(
            argv.windows(2).any(|w| w[0] == "--resume" && w[1] == uuid),
            "{argv:?}"
        );
        assert!(
            argv.contains(&"--mcp-config".to_string()) && argv.contains(&"--settings".to_string()),
            "rebuilt in full: {argv:?}"
        );

        r.sup.prompt(sid, dev(&r), "recall".into()).unwrap();
        let (turns, _, raw) = reports_until(&mut r, sid, HiveRunState::Idle).await;
        assert!(
            !turns.is_empty() && turns.iter().all(|t| t.turn == 2),
            "counts on: {turns:?}"
        );
        let cost_of = |raw: &[ClientMsg]| {
            raw.iter().find_map(|m| match m {
                ClientMsg::HiveTurn {
                    status: Some(HiveTurnStatus::Ok),
                    cost_usd,
                    ..
                } => Some(*cost_usd),
                _ => None,
            })
        };
        assert_eq!(
            cost_of(&raw),
            Some(None),
            "a resumed process's first turn: no cost rather than a wrong one"
        );
        let events = r.store.events(&sid.to_hex());
        assert!(
            events.iter().any(
                |e| matches!(e, TranscriptEvent::AssistantText { text, .. } if text == "history: 2")
            ),
            "the resumed harness has both prompts in its history: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, TranscriptEvent::Note { text } if text.starts_with("The device restarted, and this session resumed"))),
            "{events:?}"
        );

        r.sup.prompt(sid, dev(&r), "again".into()).unwrap();
        let (_, _, raw) = reports_until(&mut r, sid, HiveRunState::Idle).await;
        assert_eq!(cost_of(&raw), Some(Some(0.25)), "then what each turn cost");

        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
        assert!(hosted_on_disk(&r).is_empty(), "over, so nothing to resume");
    }

    /// The fake harness's pid, which leads its process group, once it ran.
    async fn fake_pid(r: &Rig) -> u32 {
        let pid_file = r.root.path().join("work").join(".pid");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "the harness never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Whether `pid`'s group is gone within a few seconds: its leader may be a
    /// zombie for a moment, until whoever spawned it reaps it.
    async fn group_gone(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while super::super::procs::group_alive(pid) {
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }

    /// P1h-1 — a daemon leaving with no service manager to end its harnesses
    /// (launchd does not; nothing does for an unsupervised daemon): `wind_down`
    /// ends each harness's process group itself, and the session is kept for
    /// the next daemon, reported nowhere — the same as a stop under systemd.
    #[tokio::test]
    async fn wind_down_takes_the_harness_down_and_keeps_the_session() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        let pid = fake_pid(&r).await;
        assert!(super::super::procs::group_alive(pid));
        let on_disk = hosted_on_disk(&r);
        assert_eq!(on_disk[0].harness_pid, Some(pid), "the launch records it");
        assert_eq!(
            on_disk[0].harness_started,
            super::super::procs::started(pid),
            "with its start time"
        );

        r.sup.wind_down().await;
        assert!(group_gone(pid).await, "the harness and its group are gone");
        let deadline = Instant::now() + Duration::from_secs(10);
        while r.sup.holds_live(sid) {
            assert!(Instant::now() < deadline, "the session never let go");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        silent_about(&mut r, sid).await;
        assert_eq!(hosted_on_disk(&r).len(), 1, "kept for the next daemon");
    }

    /// P1h-1 — a crash leaves its harness running, and nothing takes it down:
    /// the next daemon does, before it resumes the session on the same
    /// history, which two harnesses would both write.
    #[tokio::test]
    async fn a_harness_a_crash_left_running_is_taken_down_before_the_resume() {
        use std::os::unix::process::CommandExt;
        let mut r = rig(true, 4);
        // The leftover: a group leader with a tool beneath it, as a harness is.
        let leftover = std::process::Command::new("sh")
            .args(["-c", "sleep 60 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = leftover.id();
        // Reaped the moment it dies, as an orphan is by init: until then a
        // zombie still counts as its group, and the take-down would wait out
        // its grace for nothing.
        let (died_tx, died) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut leftover = leftover;
            let _ = died_tx.send(leftover.wait());
        });
        let sid = ObjectId::new();
        host(&r, sid, |h| {
            h.harness_pid = Some(pid);
            h.harness_started = super::super::procs::started(pid);
        });
        assert!(hosted_on_disk(&r)[0].harness_started.is_some());

        restart(&mut r, |_| {});
        let (_, manifest) = until_manifest(&mut r).await;
        assert_eq!(manifest, [(sid, 1)], "resumed");
        let status = died
            .recv_timeout(Duration::from_secs(5))
            .expect("the leftover was taken down")
            .unwrap();
        assert!(!status.success(), "{status:?}");
        assert!(group_gone(pid).await, "and the tool beneath it");
        let relaunched = fake_pid(&r).await;
        assert_ne!(relaunched, pid);
        assert!(
            super::super::procs::group_alive(relaunched),
            "the resume runs"
        );
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }

    /// P1h-1 — a recorded pid is only the harness while its start time still
    /// matches: whatever holds that pid since is left alone, and a record
    /// from before P1h (no pid) does nothing.
    #[tokio::test]
    async fn a_reused_pid_is_never_taken_down() {
        use std::os::unix::process::CommandExt;
        let r = rig(true, 4);
        let mut bystander = std::process::Command::new("sh")
            .args(["-c", "sleep 60 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = bystander.id();
        let o = order(&r);
        let mut h = HostedSession {
            session: ObjectId::new().to_hex(),
            fence: 1,
            harness: o.harness,
            harness_session: o.harness_session,
            folder: o.folder,
            account: "dev".into(),
            starter: r.user.to_hex(),
            starter_email: o.user_email,
            turns: 0,
            running: None,
            approvals: Vec::new(),
            resumed_at: None,
            quick_resumes: 0,
            harness_pid: Some(pid),
            harness_started: Some("an earlier process".into()),
        };
        reap_leftover(&h).await;
        h.harness_started = None;
        reap_leftover(&h).await;
        h.harness_pid = None;
        h.harness_started = super::super::procs::started(pid);
        reap_leftover(&h).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            bystander.try_wait().unwrap().is_none(),
            "a pid whose start time is not the recorded one is someone else's"
        );
        assert!(super::super::procs::group_alive(pid));
        super::super::procs::take_down(&[pid], Duration::from_millis(500)).await;
        bystander.wait().unwrap();
    }

    /// A restart in the middle of a turn that waits at an approval: the
    /// daemon going down says nothing; the next one reports the turn
    /// interrupted, naming who asked, and the approval withdrawn — in the
    /// transcript too — and the next prompt is the next turn.
    #[tokio::test]
    async fn a_turn_the_restart_cut_is_reported_and_its_approval_withdrawn() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        r.sup.prompt(sid, dev(&r), "hold".into()).unwrap();
        let harness = harness_asks(&r, sid, 2, "make").await;
        let approval = until_awaiting(&mut r, sid).await;
        let on_disk = hosted_on_disk(&r);
        assert_eq!(
            on_disk[0].running,
            Some(RunningTurn {
                turn: 1,
                prompted_by: Some(r.user.to_hex())
            }),
            "on disk before the server heard"
        );
        assert_eq!(on_disk[0].approvals, std::slice::from_ref(&approval));

        // The teardown as the field saw it: the daemon is stopping, and the
        // harness's MCP connection goes first, so the toolbelt withdraws the
        // approval while the session task still runs.
        r.sup.begin_shutdown();
        drop(harness);
        tokio::time::sleep(Duration::from_millis(300)).await;
        go_down(&r, sid).await;
        silent_about(&mut r, sid).await;
        assert_eq!(
            hosted_on_disk(&r)[0].approvals,
            std::slice::from_ref(&approval),
            "frozen from the shutdown on: the next daemon withdraws it"
        );

        restart(&mut r, |_| {});
        let (seen, manifest) = until_manifest(&mut r).await;
        assert_eq!(manifest, [(sid, 1)]);
        assert!(
            seen.iter().any(|m| matches!(m,
                ClientMsg::HiveTurn { session_id, turn: 1, status: Some(HiveTurnStatus::Interrupted), prompted_by, .. }
                    if *session_id == sid && *prompted_by == Some(r.user))),
            "{seen:?}"
        );
        assert!(
            seen.iter().any(|m| matches!(m,
                ClientMsg::HiveApproval { approval_id, status: Some(HiveApprovalStatus::Withdrawn), turn: Some(1), .. }
                    if *approval_id == approval)),
            "{seen:?}"
        );
        let resolutions: Vec<TranscriptEvent> = approval_events(&r, sid)
            .into_iter()
            .filter(|e| matches!(e, TranscriptEvent::ApprovalResolved { .. }))
            .collect();
        assert!(
            matches!(
                resolutions.as_slice(),
                [TranscriptEvent::ApprovalResolved { id, outcome, .. }]
                    if *id == approval && outcome == approval_outcome::WITHDRAWN
            ),
            "withdrawn exactly once, by the next daemon: {resolutions:?}"
        );
        let events = r.store.events(&sid.to_hex());
        assert!(
            events.iter().any(|e| matches!(e, TranscriptEvent::Note { text } if text.contains("turn 1 was cut by the restart"))),
            "{events:?}"
        );
        let on_disk = hosted_on_disk(&r);
        assert!(on_disk[0].running.is_none() && on_disk[0].approvals.is_empty());

        r.sup.prompt(sid, dev(&r), "say hello".into()).unwrap();
        let (turns, _, _) = reports_until(&mut r, sid, HiveRunState::Idle).await;
        assert!(
            !turns.is_empty() && turns.iter().all(|t| t.turn == 2),
            "{turns:?}"
        );
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }

    /// A daemon that is stopping takes no new turn and no new session: the
    /// turn would be cut with a number the next daemon never learns, and the
    /// session would not outlive its launch.
    #[tokio::test]
    async fn a_stopping_daemon_takes_no_new_prompt_or_start() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(next_state(&mut r, sid).await.0, HiveRunState::Idle);
        r.sup.begin_shutdown();
        let err = r.sup.prompt(sid, dev(&r), "say hello".into()).unwrap_err();
        assert!(err.contains("restarting"), "{err}");
        let refused = r.sup.start(order(&r), true).await;
        assert_eq!(refused.refused, Some(HiveRefusal::Other));
        assert!(refused.detail.unwrap_or_default().contains("restarting"));
        assert_eq!(r.sup.live_count(), 1);
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }

    /// The next daemon's gates are the ones configured NOW. Each refusal
    /// ends the session with its reason, forgets it, and launches nothing;
    /// so does a session that keeps taking the daemon down with it.
    #[tokio::test]
    async fn a_resume_passes_every_gate_again_as_the_device_is_configured_now() {
        async fn resumed_with(
            hosted: impl FnOnce(&mut HostedSession),
            cfg_with: impl FnOnce(&mut HiveConfig, ObjectId),
        ) -> String {
            let mut r = rig(true, 4);
            let sid = ObjectId::new();
            host(&r, sid, hosted);
            let user = r.user;
            restart(&mut r, |c| cfg_with(c, user));
            let (seen, manifest) = until_manifest(&mut r).await;
            assert!(manifest.is_empty(), "{manifest:?}");
            assert!(hosted_on_disk(&r).is_empty(), "forgotten");
            assert_eq!(r.sup.live_count(), 0);
            assert!(
                !r.root.path().join("work").join(".argv").exists(),
                "nothing launched"
            );
            seen.into_iter()
                .find_map(|m| match m {
                    ClientMsg::HiveState {
                        session_id,
                        state: Some(HiveRunState::Ended),
                        detail,
                        ..
                    } if session_id == sid => detail,
                    _ => None,
                })
                .expect("reported ended")
        }
        let why = resumed_with(|_| {}, |c, _| c.enabled = false).await;
        assert!(
            why.starts_with("not resumed after the device restarted: ")
                && why.contains("hive_enabled"),
            "{why}"
        );
        let why = resumed_with(
            |_| {},
            |c, user| {
                c.accounts.insert(user.to_hex(), "ops".into());
            },
        )
        .await;
        assert!(why.contains("whose home holds its history"), "{why}");
        let why = resumed_with(|_| {}, |c, _| c.roots = vec![PathBuf::from("/nowhere")]).await;
        assert!(why.contains("hive_roots"), "{why}");
        let why = resumed_with(
            |h| {
                h.resumed_at = Some(unix_now());
                h.quick_resumes = MAX_QUICK_RESUMES - 1;
            },
            |_, _| {},
        )
        .await;
        assert!(why.contains("restarted 3 times within 120 s"), "{why}");
    }

    /// Capacity holds across a restart: what the device now allows resumes,
    /// in the order it was hosted, and the rest end saying why.
    #[tokio::test]
    async fn a_resume_counts_against_capacity_as_configured_now() {
        let mut r = rig(true, 4);
        let (first, second) = (ObjectId::new(), ObjectId::new());
        host(&r, first, |_| {});
        host(&r, second, |_| {});
        restart(&mut r, |c| c.max_sessions = 1);
        let (seen, manifest) = until_manifest(&mut r).await;
        assert_eq!(manifest, [(first, 1)]);
        assert!(
            seen.iter().any(|m| matches!(m,
                ClientMsg::HiveState { session_id, state: Some(HiveRunState::Ended), detail: Some(d), .. }
                    if *session_id == second && d.contains("hive_max_sessions"))),
            "{seen:?}"
        );
        let on_disk = hosted_on_disk(&r);
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0].session, first.to_hex());
        r.sup.stop(first, 1, "owner".into());
        until_ended(&mut r, first).await;
    }

    /// A stop that reaches the next daemon before its first connection
    /// resumed the session ends it there, with no launch at all.
    #[tokio::test]
    async fn a_stop_before_the_resume_ends_the_session_without_a_launch() {
        let mut r = rig(true, 4);
        let sid = ObjectId::new();
        host(&r, sid, |_| {});
        next_daemon(&mut r, |_| {});
        r.sup.stop(sid, 1, "owner".into());
        connect(&mut r);
        let (seen, manifest) = until_manifest(&mut r).await;
        assert!(manifest.is_empty());
        assert!(
            seen.iter().any(|m| matches!(m,
                ClientMsg::HiveState { session_id, state: Some(HiveRunState::Ended), detail: Some(d), .. }
                    if *session_id == sid && d == "stopped (owner) before it resumed")),
            "{seen:?}"
        );
        assert!(hosted_on_disk(&r).is_empty());
        assert!(
            !r.root.path().join("work").join(".argv").exists(),
            "nothing launched"
        );
    }

    /// The server re-sends a start whose answer the last daemon never
    /// delivered, and it reaches the next daemon before the resume does: it
    /// IS the resume — `--resume` because the history exists, where a
    /// `--session-id` would be refused "already in use" — and the resume
    /// that follows leaves it alone.
    #[tokio::test]
    async fn a_start_the_server_re_sends_for_a_hosted_session_resumes_it() {
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        host(&r, sid, |h| h.turns = 2);
        let history = r
            .root
            .path()
            .join("home/.roomler/hive")
            .join(sid.to_hex())
            .join("claude/projects")
            .join(format!("hive-{}", o.harness_session))
            .join(format!("{}.jsonl", o.harness_session));
        std::fs::create_dir_all(history.parent().unwrap()).unwrap();
        std::fs::write(&history, "{}\n{}\n").unwrap();

        next_daemon(&mut r, |_| {});
        forget_argv(&r);
        assert_eq!(r.sup.start(o.clone(), true).await, Answer::accepted("dev"));
        let argv = next_argv(&r).await;
        assert!(argv.contains(&"--resume".to_string()), "{argv:?}");
        connect(&mut r);
        let (_, manifest) = until_manifest(&mut r).await;
        assert_eq!(manifest, [(sid, 1)]);
        assert_eq!(r.sup.live_count(), 1, "one harness, not two");

        r.sup.prompt(sid, dev(&r), "say hello".into()).unwrap();
        let (turns, _, _) = reports_until(&mut r, sid, HiveRunState::Idle).await;
        assert!(
            !turns.is_empty() && turns.iter().all(|t| t.turn == 3),
            "{turns:?}"
        );
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }

    // ─── P1e — core memory ──────────────────────────────────────────────

    fn memory_for(o: &StartOrder, claude: Option<&str>, auto: Option<&str>) -> CoreMemory {
        CoreMemory {
            session_id: o.session_id,
            fence: o.fence,
            brain_rev: 7,
            claude_md: claude.map(str::to_string),
            memory_md: auto.map(str::to_string),
        }
    }

    /// What the fake harness found where Claude Code reads `name`'s memory,
    /// once the launch after [`forget_argv`] has run (it copies before it
    /// writes its argv).
    async fn given(r: &Rig, name: &str) -> Option<String> {
        next_argv(r).await;
        std::fs::read_to_string(r.root.path().join("work").join(name)).ok()
    }

    fn notes(r: &Rig, sid: ObjectId) -> Vec<String> {
        r.store
            .events(&sid.to_hex())
            .into_iter()
            .filter_map(|e| match e {
                TranscriptEvent::Note { text } => Some(text),
                _ => None,
            })
            .collect()
    }

    fn config_claude_md(r: &Rig, sid: ObjectId) -> PathBuf {
        r.root
            .path()
            .join("home/.roomler/hive")
            .join(sid.to_hex())
            .join("claude/CLAUDE.md")
    }

    /// The device's own gate decides: allowed, both files are where Claude
    /// Code reads them, the account's own copies; off, neither is, and the
    /// transcript says why; no memory, nothing is placed or said.
    #[tokio::test]
    async fn core_memory_reaches_a_session_only_where_the_device_allows_it() {
        let mut r = rig_with(true, 4, |c| c.core_memory = true, |s| s);
        let o = order(&r);
        let sid = o.session_id;
        r.sup.receive_memory(memory_for(
            &o,
            Some("# Organization memory\n\n- (warning) ORG-FACT\n"),
            Some("# Device memory: d\n\n- (path) DEVICE-FACT\n"),
        ));
        forget_argv(&r);
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert!(
            given(&r, ".claude_md").await.unwrap().contains("ORG-FACT"),
            "CLAUDE.md, where Claude Code reads the user's instructions"
        );
        assert!(
            std::fs::read_to_string(r.root.path().join("work/.memory_md"))
                .unwrap()
                .contains("DEVICE-FACT"),
            "the auto-memory index"
        );
        let mode = std::fs::metadata(config_claude_md(&r, sid))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "copied in by the account, under its umask");
        // The daemon's own copy, which the account reads to copy in, is the
        // account's alone too: it carries the starter's own facts.
        for name in ["CLAUDE.md", "MEMORY.md"] {
            let own = r
                .root
                .path()
                .join("run")
                .join(sid.to_hex())
                .join("memory")
                .join(name);
            let mode = std::fs::metadata(&own).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", own.display());
        }
        assert!(
            notes(&r, sid)
                .iter()
                .any(|n| n.contains("revision 7") && n.contains("CLAUDE.md")),
            "{:?}",
            notes(&r, sid)
        );
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;

        // Off — the default.
        let mut r = rig(true, 4);
        let o = order(&r);
        let sid = o.session_id;
        r.sup
            .receive_memory(memory_for(&o, Some("ORG-FACT"), Some("DEVICE-FACT")));
        forget_argv(&r);
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(given(&r, ".claude_md").await, None);
        assert!(!r.root.path().join("work/.memory_md").exists());
        assert!(!config_claude_md(&r, sid).exists());
        assert!(
            notes(&r, sid)
                .iter()
                .any(|n| n.contains("hive_core_memory is off")),
            "{:?}",
            notes(&r, sid)
        );
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;

        // None came.
        let mut r = rig_with(true, 4, |c| c.core_memory = true, |s| s);
        let o = order(&r);
        let sid = o.session_id;
        forget_argv(&r);
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(given(&r, ".claude_md").await, None);
        assert!(
            !notes(&r, sid)
                .iter()
                .any(|n| n.to_lowercase().contains("core memory")),
            "{:?}",
            notes(&r, sid)
        );
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }

    /// A resume (P1d-2) keeps the session's own copy, its own edits
    /// included: no memory comes with it, and the wrapper copies only into
    /// an empty place.
    #[tokio::test]
    async fn a_resume_keeps_the_sessions_own_core_memory() {
        let mut r = rig_with(true, 4, |c| c.core_memory = true, |s| s);
        let o = order(&r);
        let sid = o.session_id;
        r.sup
            .receive_memory(memory_for(&o, Some("ORG-FACT v1\n"), None));
        forget_argv(&r);
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(
            given(&r, ".claude_md").await.as_deref(),
            Some("ORG-FACT v1\n")
        );
        // A turn, so there is a history to resume; and the session's own note.
        r.sup.prompt(sid, dev(&r), "say hello".into()).unwrap();
        reports_until(&mut r, sid, HiveRunState::Idle).await;
        let own = "ORG-FACT v1\n- a note the session kept\n";
        std::fs::write(config_claude_md(&r, sid), own).unwrap();

        go_down(&r, sid).await;
        std::fs::remove_file(r.root.path().join("work/.claude_md")).unwrap();
        forget_argv(&r);
        restart(&mut r, |_| {});
        let (_, manifest) = until_manifest(&mut r).await;
        assert_eq!(manifest, [(sid, 1)]);
        assert_eq!(
            given(&r, ".claude_md").await.as_deref(),
            Some(own),
            "the resumed harness has the session's own copy"
        );
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }

    /// A snapshot larger than the server ever renders is dropped; one for
    /// another start (another fence) is not this start's.
    #[tokio::test]
    async fn core_memory_too_large_or_for_another_start_is_not_used() {
        use roomler_ai_remote_control::hive::hive_limits::MAX_CORE_MEMORY_BYTES;
        let mut r = rig_with(true, 4, |c| c.core_memory = true, |s| s);
        let o = order(&r);
        let sid = o.session_id;
        let big = "x".repeat(MAX_CORE_MEMORY_BYTES + 1);
        r.sup.receive_memory(memory_for(&o, Some(&big), None));
        let mut other = memory_for(&o, Some("ANOTHER-FENCE"), None);
        other.fence = 2;
        r.sup.receive_memory(other);
        forget_argv(&r);
        assert!(r.sup.start(o, true).await.refused.is_none());
        assert_eq!(given(&r, ".claude_md").await, None);
        assert!(!config_claude_md(&r, sid).exists());
        r.sup.stop(sid, 1, "owner".into());
        until_ended(&mut r, sid).await;
    }
}

/// P1i-4 — a session's directories on Windows, as the session's own account
/// meets them: a restricted Medium copy of this process's token, its
/// Administrators group deny-only, on a thread of its own.
#[cfg(all(test, windows))]
mod win_tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    };

    /// Examine `p` as libuv's `lstat` does (Claude Code is a Bun build, whose
    /// file system on Windows is libuv's): `FILE_READ_ATTRIBUTES`, with backup
    /// semantics so a directory opens, and the reparse point itself.
    fn examine(p: &Path) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(p)
    }

    /// Field-found on Windows 11: Claude Code examines each component of its
    /// `--settings` path, and refused the session's file because its account
    /// could examine neither the store's directory nor the runtime root.
    #[test]
    fn a_sessions_account_examines_the_directories_on_its_way_and_lists_none() {
        let base = tempfile::tempdir().unwrap();
        let runtime = base.path().join("hive").join("run");
        let hive = runtime.parent().unwrap().to_path_buf();
        let me = crate::win_token::own_user_sid().unwrap();
        let mine = runtime.join("s1");
        session_dir(&runtime, &mine, &me).unwrap();
        let settings = mine.join("settings.json");
        std::fs::write(&settings, "{}").unwrap();
        // Another session's, run as another account (LocalService).
        let theirs = runtime.join("s2");
        session_dir(&runtime, &theirs, "S-1-5-19").unwrap();

        let (examined, listed, other) = std::thread::scope(|s| {
            s.spawn(|| {
                let token = crate::win_token::restricted_medium_copy().unwrap();
                // SAFETY: a live token opened for impersonation; the thread is
                // this test's alone, and reverts before it ends.
                assert_ne!(unsafe { ImpersonateLoggedOnUser(token.raw()) }, 0);
                // The test's own folder first: proof the thread really is that
                // account, since an impersonation Windows refused fails every
                // open, not just the ones under test.
                let own = base.path().to_path_buf();
                let examined: Vec<(String, Result<(), String>)> =
                    [&own, &hive, &runtime, &mine, &settings]
                        .into_iter()
                        .map(|p| {
                            let r = examine(p).map(drop).map_err(|e| e.to_string());
                            (p.display().to_string(), r)
                        })
                        .collect();
                let listed = [&hive, &runtime].map(|p| std::fs::read_dir(p).is_ok());
                let other = examine(&theirs).is_ok();
                // SAFETY: ends this thread's impersonation.
                assert_ne!(unsafe { RevertToSelf() }, 0);
                (examined, listed, other)
            })
            .join()
            .unwrap()
        });
        for (p, r) in &examined {
            assert!(r.is_ok(), "the session's account cannot examine {p}: {r:?}");
        }
        assert_eq!(
            listed,
            [false, false],
            "it lists neither the store nor the runtime root"
        );
        assert!(!other, "nor examines another session's directory");
    }
}

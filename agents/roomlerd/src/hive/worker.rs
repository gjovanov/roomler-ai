// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2a-1 — `roomlerd hive-worker`: a session's worker, the same
//! binary as a supervised child of the daemon (the FR-90 spec, §3i "P8-0, as
//! designed"). This step is the process model alone: the channel, the
//! handshake, the heartbeat, the restarts and the end of a crash loop.
//! Nothing of the session runs in it yet; P8-2a-2 moves the credential
//! proxies into it.
//!
//! | Piece | As built |
//! |---|---|
//! | the program | the daemon's own image: `/proc/self/exe` on Linux, so a package upgrade under a running daemon never gives it a worker of the next version; elsewhere its path, and the handshake refuses another version |
//! | the channel | a `socketpair(AF_UNIX, SOCK_STREAM)` the daemon makes before the spawn; the worker's end is its fd 3, the one descriptor it inherits besides stderr |
//! | a frame | a `u32` length, big-endian, then one JSON message, at most [`MAX_FRAME`], written in one write and read whole by a task of its own ([`Frames`]) |
//! | the handshake | the daemon says `hello {session, fence}`; the worker answers `ready {pid, version}` within the start bound, at the daemon's version |
//! | the heartbeat | the daemon sends `beat {n}` at each interval and the worker answers it; [`Timing::misses`] unanswered in a row, and the worker is killed |
//! | a restart | after a wait that doubles with each start still inside [`Timing::loop_window`], to a ceiling; [`Timing::loop_max`] starts within it, and the daemon gives up on this run's worker rather than start another |
//! | the stop | `bye`, and the channel's end awaited within a grace, then a kill |
//! | the daemon's end | the worker's too: it ends at its channel's end, and the channel is close-on-exec on both sides, so only the daemon holds the other end |
//!
//! ⚠️ Giving up costs nothing yet: no part of the session runs in the
//! worker, so the run goes on. Once a part does (P8-2a-2), giving up ends the
//! run with a reason.
//!
//! ⚠️ The worker runs as the daemon's own identity: this is crash and
//! resource isolation, not a privilege boundary.
//!
//! ⚠️ Not `PR_SET_PDEATHSIG`: it fires when the THREAD that forked exits, and
//! a tokio worker thread does exit while the daemon lives — `block_in_place`
//! (the encoder pumps, the recorder) hands its core to another thread and
//! lets it idle out — so the worker would be killed at a moment that has
//! nothing to do with the daemon, during a remote-control session say, and
//! soon given up on.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// The subcommand: `roomlerd hive-worker`.
pub const WORKER_SUBCOMMAND: &str = "hive-worker";

/// Set in the environment of a worker the daemon starts: `roomlerd
/// hive-worker` typed by hand has no channel on fd 3, and takes none.
const CHILD_ENV: &str = "ROOMLERD_HIVE_WORKER";

/// This build's version: a worker must be the daemon's.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The descriptor a worker's channel arrives on.
const CHANNEL_FD: std::os::fd::RawFd = 3;

/// The largest frame either side sends or takes.
pub(crate) const MAX_FRAME: usize = 1 << 20;

/// One message on the channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum Msg {
    /// The daemon: the run this worker serves.
    Hello { session: String, fence: u64 },
    /// The worker: up, which process it is, and which build.
    Ready { pid: u32, version: String },
    /// Either side: the heartbeat, and its answer.
    Beat { n: u64 },
    /// The daemon: end.
    Bye,
}

/// Write one frame, its length and its body in one write.
pub(crate) async fn send<W: AsyncWrite + Unpin>(w: &mut W, m: &Msg) -> std::io::Result<()> {
    let body = serde_json::to_vec(m).map_err(std::io::Error::other)?;
    if body.len() > MAX_FRAME {
        return Err(std::io::Error::other("a frame past its bound"));
    }
    let len = u32::try_from(body.len()).map_err(std::io::Error::other)?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame).await?;
    w.flush().await
}

/// Read one frame: `None` at the channel's clean end.
pub(crate) async fn recv<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Msg>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other("a frame past its bound"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(std::io::Error::other)
}

/// The daemon's reading side of a worker's channel: each frame read whole by
/// a task of its own, so a wait for the next one can be given up — at a
/// heartbeat's deadline, at the stop — without losing half a frame, which
/// would leave every later read out of step.
pub(crate) struct Frames {
    rx: tokio::sync::mpsc::Receiver<std::io::Result<Option<Msg>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Frames {
    pub(crate) fn read<R: AsyncRead + Unpin + Send + 'static>(mut r: R) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let task = tokio::spawn(async move {
            loop {
                let got = recv(&mut r).await;
                let more = matches!(got, Ok(Some(_)));
                if tx.send(got).await.is_err() || !more {
                    return;
                }
            }
        });
        Self { rx, task }
    }

    /// The next frame: `None` at the channel's end.
    pub(crate) async fn next(&mut self) -> std::io::Result<Option<Msg>> {
        self.rx.recv().await.unwrap_or(Ok(None))
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ─── The worker: what `roomlerd hive-worker` runs ───────────────────────────

/// `roomlerd hive-worker`, exactly, in a process the daemon started: read
/// from raw argv, so the worker runs none of the daemon's start-up.
pub fn worker_args() -> bool {
    is_worker(std::env::args_os(), std::env::var_os(CHILD_ENV))
}

/// [`worker_args`] of a process's argv and its [`CHILD_ENV`].
fn is_worker(
    mut argv: impl Iterator<Item = std::ffi::OsString>,
    child_env: Option<std::ffi::OsString>,
) -> bool {
    let _exe = argv.next();
    argv.next().is_some_and(|a| a == WORKER_SUBCOMMAND)
        && argv.next().is_none()
        && child_env.is_some()
}

/// A worker's life, from its channel on fd 3: answer the handshake and each
/// heartbeat, and end at `bye` or the channel's end. The process's exit
/// code.
pub async fn worker_main() -> i32 {
    // Linux runs this as `/proc/self/exe`, whose name the kernel takes for
    // the process's: named as the daemon's every other child is.
    #[cfg(target_os = "linux")]
    // SAFETY: a plain prctl on this process, with a NUL-terminated name.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c"roomlerd".as_ptr());
    }
    // The channel is this process's alone: nothing it starts inherits it, so
    // the daemon's end sees the channel end as this process does. A failure
    // here also says fd 3 is not open.
    // SAFETY: fcntl on a descriptor number, with constant arguments.
    if unsafe { libc::fcntl(CHANNEL_FD, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return 2;
    }
    use std::os::fd::FromRawFd;
    // SAFETY: the daemon handed this process its channel as fd 3 and nothing
    // else owns it: this is the one place it is taken.
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(CHANNEL_FD) };
    if std_stream.set_nonblocking(true).is_err() {
        return 2;
    }
    let Ok(stream) = UnixStream::from_std(std_stream) else {
        return 2;
    };
    match serve(stream).await {
        Ok(()) => 0,
        Err(e) => {
            crate::logging::say_on_stderr(format_args!("roomlerd hive-worker: {e}"));
            1
        }
    }
}

/// The worker's side of the channel.
pub(crate) async fn serve(mut ch: UnixStream) -> std::io::Result<()> {
    while let Some(m) = recv(&mut ch).await? {
        match m {
            Msg::Hello { session, fence } => {
                // The daemon's stderr, which can be gone by now: a line lost
                // beats a worker lost (#2719).
                crate::logging::say_on_stderr(format_args!(
                    "roomlerd hive-worker: serving {session} at fence {fence}"
                ));
                send(
                    &mut ch,
                    &Msg::Ready {
                        pid: std::process::id(),
                        version: VERSION.to_string(),
                    },
                )
                .await?;
            }
            Msg::Beat { n } => send(&mut ch, &Msg::Beat { n }).await?,
            Msg::Bye => return Ok(()),
            Msg::Ready { .. } => {}
        }
    }
    Ok(())
}

// ─── The daemon's side: spawn, supervise, restart ───────────────────────────

/// A running worker, as its supervisor holds it.
#[async_trait::async_trait]
pub(crate) trait Running: Send {
    /// Its process, where it has one.
    fn pid(&self) -> Option<u32>;
    /// End it now.
    async fn kill(&mut self);
}

/// How a worker is started: a child of a program (this binary's, or a
/// test's), or a test's task.
pub(crate) trait Spawn: Send + Sync {
    /// A worker with `theirs` as its channel.
    fn spawn(&self, theirs: std::os::unix::net::UnixStream) -> std::io::Result<Box<dyn Running>>;
}

/// A worker as a child process: `program` with `args`, its channel dup'ed
/// onto fd 3.
pub(crate) struct Program {
    pub(crate) exe: PathBuf,
    /// What `ps` shows for it, where not `exe`.
    pub(crate) arg0: Option<std::ffi::OsString>,
    pub(crate) args: Vec<String>,
}

impl Program {
    /// This binary's own worker: `roomlerd hive-worker`, run from the image
    /// this daemon runs.
    pub(crate) fn this_binary() -> std::io::Result<Self> {
        // `/proc/self/exe` names the running image even after a package
        // upgrade unlinked or renamed it, and it is what the child runs (the
        // fork's image, the daemon's). The path names the next version by
        // then, which the daemon may run for a day before it restarts.
        #[cfg(target_os = "linux")]
        let exe = PathBuf::from("/proc/self/exe");
        #[cfg(not(target_os = "linux"))]
        let exe = std::env::current_exe()?;
        Ok(Self {
            exe,
            arg0: std::env::args_os().next(),
            args: vec![WORKER_SUBCOMMAND.to_string()],
        })
    }
}

#[async_trait::async_trait]
impl Running for tokio::process::Child {
    fn pid(&self) -> Option<u32> {
        self.id()
    }

    async fn kill(&mut self) {
        let _ = tokio::process::Child::kill(self).await;
    }
}

impl Spawn for Program {
    fn spawn(&self, theirs: std::os::unix::net::UnixStream) -> std::io::Result<Box<dyn Running>> {
        use std::os::fd::AsRawFd;
        // A daemon started with a standard descriptor closed can be handed
        // the channel as one, which the child's stdio would overwrite before
        // the move onto fd 3: a copy above them goes instead.
        let theirs = if theirs.as_raw_fd() <= 2 {
            let above = theirs.try_clone()?;
            drop(theirs);
            above
        } else {
            theirs
        };
        let fd = theirs.as_raw_fd();
        let mut cmd = tokio::process::Command::new(&self.exe);
        if let Some(arg0) = &self.arg0 {
            cmd.arg0(arg0);
        }
        cmd.args(&self.args)
            .env(CHILD_ENV, "1")
            // Config fallbacks are process-local: the child is handed them as
            // real variables, as the encoder probe's child is.
            .envs(tunnel_core::env::config_fallbacks_for_child())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        // SAFETY: between fork and exec only async-signal-safe calls: dup2
        // and fcntl. Every descriptor the daemon makes is close-on-exec, so
        // the channel, moved onto fd 3 without that flag, is the one that
        // crosses.
        unsafe {
            cmd.pre_exec(move || {
                if fd == CHANNEL_FD {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::dup2(fd, CHANNEL_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        // The daemon's copy of the worker's end goes now: the worker's end is
        // the worker's, so the channel ends when the worker does.
        drop(theirs);
        Ok(Box::new(child))
    }
}

/// The supervisor's bounds.
#[derive(Debug, Clone)]
pub(crate) struct Timing {
    /// How long a worker has to answer `hello`.
    pub(crate) start: Duration,
    /// How often a heartbeat goes.
    pub(crate) beat: Duration,
    /// Heartbeats unanswered in a row before the worker is killed.
    pub(crate) misses: u32,
    /// The wait before a restart: this, doubled for each earlier start still
    /// inside `loop_window`, to `backoff_max` ([`wait_before`]).
    pub(crate) backoff: Duration,
    pub(crate) backoff_max: Duration,
    /// At most `loop_max` starts within `loop_window`: past them, the daemon
    /// gives up rather than start another.
    pub(crate) loop_window: Duration,
    pub(crate) loop_max: u32,
    /// How long a stopped worker has to go after `bye`, and how long `bye`
    /// itself may take.
    pub(crate) grace: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            start: Duration::from_secs(10),
            beat: Duration::from_secs(5),
            misses: 3,
            backoff: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            loop_window: Duration::from_secs(600),
            loop_max: 5,
            grace: Duration::from_secs(2),
        }
    }
}

/// What a supervisor has seen of its worker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Seen {
    /// Workers started.
    pub(crate) starts: u32,
    /// Whether one is up and answering now.
    pub(crate) up: bool,
    /// The last worker's process.
    pub(crate) pid: Option<u32>,
    /// Whether the daemon gave up on this run's worker.
    pub(crate) gave_up: bool,
}

/// One run's worker, supervised: ends when dropped (the run's end), or with
/// [`Worker::stop`], which waits for it.
pub(crate) struct Worker {
    stop: CancellationToken,
    seen: Arc<Mutex<Seen>>,
    /// Its supervisor, which the run's end leaves to finish on its own: only
    /// [`Worker::stop`] waits for it, and only the tests stop a worker so.
    #[cfg_attr(not(test), allow(dead_code))]
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Worker {
    /// Start the worker of `session` at `fence`, and keep one up until
    /// stopped or given up on.
    pub(crate) fn start(
        session: ObjectId,
        fence: u64,
        spawner: Arc<dyn Spawn>,
        timing: Timing,
    ) -> Self {
        let stop = CancellationToken::new();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let task = tokio::spawn(supervise(
            session,
            fence,
            spawner,
            timing,
            stop.clone(),
            Arc::clone(&seen),
        ));
        Self {
            stop,
            seen,
            task: Some(task),
        }
    }

    /// What it has seen so far.
    pub(crate) fn seen(&self) -> Seen {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// End it: `bye` to the worker, then a kill after the grace. Waits for
    /// its supervisor to finish.
    #[cfg(test)]
    pub(crate) async fn stop(mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// How one worker's life ended.
enum Ended {
    /// The supervisor was stopped: end for good.
    Stopped,
    /// The worker died, hung or never answered: start another.
    Lost(String),
    /// The worker is of another build, as the next one would be: give up now.
    Foreign(String),
}

/// Keep a worker up for the run: start it, hand it its run, beat, and
/// restart it as it is lost, until stopped or given up on.
async fn supervise(
    session: ObjectId,
    fence: u64,
    spawner: Arc<dyn Spawn>,
    timing: Timing,
    stop: CancellationToken,
    seen: Arc<Mutex<Seen>>,
) {
    let note = |f: &dyn Fn(&mut Seen)| f(&mut seen.lock().unwrap_or_else(|e| e.into_inner()));
    let give_up = |why: &str| {
        warn!(%session, fence, %why, "hive: a session's worker is given up on (the run goes on: nothing of it runs in the worker yet)");
        note(&|s| {
            s.gave_up = true;
            s.up = false;
        });
    };
    let mut starts: Vec<Instant> = Vec::new();
    loop {
        if stop.is_cancelled() {
            return;
        }
        let recent = within(&mut starts, timing.loop_window);
        if recent >= timing.loop_max as usize {
            give_up(&format!("{recent} starts within {:?}", timing.loop_window));
            return;
        }
        starts.push(Instant::now());
        note(&|s| s.starts += 1);
        let ended = match std::os::unix::net::UnixStream::pair() {
            Ok((ours, theirs)) => match spawner.spawn(theirs) {
                Ok(mut running) => {
                    let pid = running.pid();
                    note(&|s| s.pid = pid);
                    let ended = match ours
                        .set_nonblocking(true)
                        .map(|_| UnixStream::from_std(ours))
                    {
                        Ok(Ok(ch)) => life(session, fence, ch, &timing, &stop, &seen).await,
                        _ => Ended::Lost("its channel could not be served".into()),
                    };
                    note(&|s| s.up = false);
                    running.kill().await;
                    ended
                }
                Err(e) => Ended::Lost(format!("it could not be started: {e}")),
            },
            Err(e) => Ended::Lost(format!("its channel could not be made: {e}")),
        };
        let why = match ended {
            Ended::Stopped => return,
            Ended::Foreign(why) => return give_up(&why),
            Ended::Lost(why) => why,
        };
        let wait = wait_before(&timing, within(&mut starts, timing.loop_window));
        warn!(%session, fence, %why, wait_ms = wait.as_millis() as u64, "hive: a session's worker was lost: starting another");
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
    }
}

/// How many of `starts` are inside the window, the older ones forgotten.
fn within(starts: &mut Vec<Instant>, window: Duration) -> usize {
    let now = Instant::now();
    starts.retain(|t| now.duration_since(*t) < window);
    starts.len()
}

/// The wait before a start, with `recent` starts inside the window: the
/// first backoff, doubled for each but the latest, to the ceiling. Starts
/// age out of the window and the wait with them, so a worker lost once in a
/// long while is started again after the first backoff.
fn wait_before(timing: &Timing, recent: usize) -> Duration {
    let doublings = u32::try_from(recent.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(16);
    timing
        .backoff
        .saturating_mul(1_u32 << doublings)
        .min(timing.backoff_max)
}

/// One worker's life on its channel: the handshake, then the heartbeat, until
/// stopped or lost.
async fn life(
    session: ObjectId,
    fence: u64,
    ch: UnixStream,
    timing: &Timing,
    stop: &CancellationToken,
    seen: &Arc<Mutex<Seen>>,
) -> Ended {
    let (rd, mut wr) = ch.into_split();
    let mut frames = Frames::read(rd);
    let hello = Msg::Hello {
        session: session.to_hex(),
        fence,
    };
    // The hello's write is inside the start bound too.
    let ready = tokio::select! {
        () = stop.cancelled() => return bye(&mut wr, &mut frames, timing.grace).await,
        r = tokio::time::timeout(timing.start, async {
            send(&mut wr, &hello).await?;
            frames.next().await
        }) => r,
    };
    match ready {
        Ok(Ok(Some(Msg::Ready { pid, version }))) if version == VERSION => {
            info!(%session, fence, pid, "hive: a session's worker is up");
            let mut s = seen.lock().unwrap_or_else(|e| e.into_inner());
            s.up = true;
            s.pid = Some(pid);
        }
        Ok(Ok(Some(Msg::Ready { version, .. }))) => {
            return Ended::Foreign(format!("it is version {version}, the daemon {VERSION}"));
        }
        Ok(Ok(Some(other))) => return Ended::Lost(format!("it answered hello with {other:?}")),
        Ok(Ok(None)) => return Ended::Lost("it ended before it was ready".into()),
        Ok(Err(e)) => return Ended::Lost(format!("its channel failed: {e}")),
        Err(_) => return Ended::Lost("it was not ready in time".into()),
    }
    let mut n: u64 = 0;
    let mut missed: u32 = 0;
    loop {
        n += 1;
        if let Err(e) = send(&mut wr, &Msg::Beat { n }).await {
            return Ended::Lost(format!("its channel refused a beat: {e}"));
        }
        let deadline = tokio::time::Instant::now() + timing.beat;
        let mut answered = false;
        loop {
            let got = tokio::select! {
                () = stop.cancelled() => return bye(&mut wr, &mut frames, timing.grace).await,
                r = tokio::time::timeout_at(deadline, frames.next()) => r,
            };
            match got {
                Ok(Ok(Some(Msg::Beat { n: m }))) if m == n => answered = true,
                Ok(Ok(Some(_))) => continue,
                Ok(Ok(None)) => return Ended::Lost("its channel ended".into()),
                Ok(Err(e)) => return Ended::Lost(format!("its channel failed: {e}")),
                Err(_) => break,
            }
        }
        if answered {
            missed = 0;
        } else {
            missed += 1;
            if missed >= timing.misses {
                return Ended::Lost(format!("{missed} heartbeats went unanswered"));
            }
        }
    }
}

/// The stop: `bye` to the worker, and its channel's end awaited, within the
/// grace. A worker still there after it is killed all the same.
async fn bye(
    wr: &mut tokio::net::unix::OwnedWriteHalf,
    frames: &mut Frames,
    grace: Duration,
) -> Ended {
    let _ = tokio::time::timeout(grace, async {
        if send(wr, &Msg::Bye).await.is_ok() {
            while let Ok(Some(_)) = frames.next().await {}
        }
    })
    .await;
    Ended::Stopped
}

// ─── Each run's worker, for the supervisor ──────────────────────────────────

/// Every run's worker here, by session and fence.
type Runs = std::collections::HashMap<(ObjectId, u64), Worker>;

static RUNS: std::sync::LazyLock<Mutex<Runs>> = std::sync::LazyLock::new(Mutex::default);

fn runs() -> std::sync::MutexGuard<'static, Runs> {
    RUNS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The run of `session` at `fence` went live here: a worker beside it, this
/// binary's own, where the device's owner turned workers on (`hive_worker`).
pub(crate) fn run_started(session: ObjectId, fence: u64, on: bool) {
    if !on {
        return;
    }
    match Program::this_binary() {
        Ok(program) => run_started_with(session, fence, Arc::new(program), Timing::default()),
        Err(e) => {
            warn!(%session, fence, %e, "hive: no worker for the session: this binary's own path is unknown")
        }
    }
}

/// [`run_started`] with its spawner and bounds named.
pub(crate) fn run_started_with(
    session: ObjectId,
    fence: u64,
    spawner: Arc<dyn Spawn>,
    timing: Timing,
) {
    let worker = Worker::start(session, fence, spawner, timing);
    // A run restarted here at the same fence replaces its worker: the old
    // one's supervisor is stopped as it drops.
    runs().insert((session, fence), worker);
}

/// What the run's worker has seen, while it has one.
#[cfg(test)]
pub(crate) fn seen(session: ObjectId, fence: u64) -> Option<Seen> {
    runs().get(&(session, fence)).map(Worker::seen)
}

/// The run's end here: its worker goes with it, told `bye` and then
/// killed, as its supervisor stops, and what it went through is logged.
/// Whether it had one.
pub(crate) fn drop_run(session: ObjectId, fence: u64) -> bool {
    let Some(worker) = runs().remove(&(session, fence)) else {
        return false;
    };
    let s = worker.seen();
    info!(%session, fence, starts = s.starts, gave_up = s.gave_up, "hive: the run's worker ends with it");
    true
}

#[cfg(test)]
mod tests;

// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2a-1 — `roomlerd hive-worker`: a session's worker, the same
//! binary as a supervised child of the daemon (the FR-90 spec, §3i "P8-0, as
//! designed"): the channel, the handshake, the heartbeat, the restarts and
//! the end of a crash loop. P8-2a-2 — the session's credential endpoints are
//! served in it (`vault_connect::handed`), their listeners handed across a
//! second channel and their values asked of the daemon over the first, as
//! calls ([`link`]).
//!
//! | Piece | As built |
//! |---|---|
//! | the program | the daemon's own image: `/proc/self/exe` on Linux, so a package upgrade under a running daemon never gives it a worker of the next version; elsewhere its path, and the handshake refuses another version |
//! | the channel | a `socketpair(AF_UNIX, SOCK_STREAM)` the daemon makes before the spawn; the worker's end is its fd 3. P8-2a-2 — a second one, fd 4, for the listeners the daemon hands across ([`fds`]); the two are the only descriptors it inherits besides stderr |
//! | calls | P8-2a-2 — `call {id, …}` either way, answered by `answer {id, …}`, many in flight; a frame is wiped after it is written or read, since it can carry an endpoint's values. A call the daemon cannot send its worker, or that goes unanswered within its bound, ends that worker's life as a failed channel does, and another is started |
//! | a frame | a `u32` length, big-endian, then one JSON message, at most [`MAX_FRAME`], written in one write and read whole by a task of its own ([`Frames`]) |
//! | the handshake | the daemon says `hello {session, fence}`; the worker answers `ready {pid, version}` within the start bound, at the daemon's version |
//! | the heartbeat | the daemon sends `beat {n}` at each interval and the worker answers it; [`Timing::misses`] unanswered in a row, and the worker is killed |
//! | a restart | after a wait that doubles with each start still inside [`Timing::loop_window`], to a ceiling; [`Timing::loop_max`] starts within it, and the daemon gives up on this run's worker rather than start another |
//! | the stop | `bye`, and the channel's end awaited within a grace, then a kill |
//! | the daemon's end | the worker's too: it ends at its channel's end, and the channel is close-on-exec on both sides, so only the daemon holds the other end |
//! | limits | P8-2a-3 — set as it starts: a core limit of 1 byte, so it never dumps core, even where a helper takes the dump; at most [`MAX_OPEN_FILES`] descriptors. No limit on address space: glibc reserves 64 MiB a thread for its arenas, so a cap fails healthy threads long before a runaway |
//!
//! ⚠️ Giving up ends the run, with the reason: where the device's owner
//! turned workers on, what has moved runs in a worker or not at all, never
//! back in the daemon, which a part that keeps failing would take down with
//! it.
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

/// FR-90 P8-2a-2 — the descriptor its second channel arrives on: the one
/// the daemon hands listeners across ([`fds`]).
const DESCRIPTORS_FD: std::os::fd::RawFd = 4;

/// FR-90 P8-2a-3 — the most descriptors a worker holds open. Each connection
/// it serves is one or two, so a worker that leaks them stops here, long
/// before the host's ephemeral ports run out: a leak of sockets once took a
/// host's name resolution down while every address still answered.
const MAX_OPEN_FILES: libc::rlim_t = 4096;

/// How long a hand-over waits for a starting worker to be up.
#[cfg(feature = "vault")]
const UP_WAIT: Duration = Duration::from_secs(10);

#[cfg(feature = "vault")]
pub(crate) use super::vault_connect::handed::{Answer, Call};

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
    /// FR-90 P8-2a-5 — the worker: one line of its log, which the daemon
    /// writes into its own ([`log`]).
    Log {
        level: String,
        target: String,
        line: String,
    },
    /// FR-90 P8-2a-2 — either side: a call, which the other answers with
    /// the `answer` of the same id ([`link`]).
    #[cfg(feature = "vault")]
    Call { id: u64, call: Call },
    #[cfg(feature = "vault")]
    Answer { id: u64, answer: Answer },
}

/// Write one frame, its length and its body in one write. The buffers are
/// wiped after: a frame can carry an endpoint's values.
pub(crate) async fn send<W: AsyncWrite + Unpin>(w: &mut W, m: &Msg) -> std::io::Result<()> {
    let mut body = serde_json::to_vec(m).map_err(std::io::Error::other)?;
    if body.len() > MAX_FRAME {
        wipe(&mut body);
        return Err(std::io::Error::other("a frame past its bound"));
    }
    let len = u32::try_from(body.len()).map_err(std::io::Error::other)?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&body);
    wipe(&mut body);
    let written = w.write_all(&frame).await;
    wipe(&mut frame);
    written?;
    w.flush().await
}

/// Overwrite `buf` with zeros in a way the optimiser keeps.
fn wipe(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        // SAFETY: a write through a valid `&mut u8`.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
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
    let read = r.read_exact(&mut body).await;
    let parsed = read.and_then(|_| serde_json::from_slice(&body).map_err(std::io::Error::other));
    wipe(&mut body);
    parsed.map(Some)
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
    if let Err(e) = limit() {
        crate::logging::say_on_stderr(format_args!(
            "roomlerd hive-worker: its limits could not be set: {e}"
        ));
        return 3;
    }
    // FR-90 P8-2a-5 — its log, into the daemon's: this process runs none of
    // the daemon's start-up, its logging among it.
    let (layer, lines) = log::layer();
    log::install(layer);
    // The channels are this process's alone: nothing it starts inherits
    // them, so the daemon's end sees them end as this process does. A failure
    // here also says fd 3 or fd 4 is not open.
    for fd in [CHANNEL_FD, DESCRIPTORS_FD] {
        // SAFETY: fcntl on a descriptor number, with constant arguments.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return 2;
        }
    }
    use std::os::fd::FromRawFd;
    let take = |fd| {
        // SAFETY: the daemon handed this process its channels as fd 3 and
        // fd 4 and nothing else owns them: this is the one place they are
        // taken.
        let std = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
        std.set_nonblocking(true)
            .and_then(|()| UnixStream::from_std(std))
    };
    let (Ok(stream), Ok(descriptors)) = (take(CHANNEL_FD), take(DESCRIPTORS_FD)) else {
        return 2;
    };
    match serve(stream, Some(descriptors), Some(lines)).await {
        Ok(()) => 0,
        Err(e) => {
            crate::logging::say_on_stderr(format_args!("roomlerd hive-worker: {e}"));
            1
        }
    }
}

/// FR-90 P8-2a-3 — a worker's limits, set as it starts, before it takes its
/// channels: it never dumps core, since its memory holds the values of the
/// endpoints it serves; and it holds at most [`MAX_OPEN_FILES`] descriptors.
/// A worker that cannot set them does not run.
fn limit() -> std::io::Result<()> {
    // 1, not 0: where the system pipes a dump to a helper (systemd-coredump,
    // apport, WSL's), a limit of 0 is dumped all the same (measured on Linux
    // 6.6). The kernel keeps 1 for a helper's own crash, and dumps nothing
    // at it; a dump to a file needs more than a byte.
    let core = libc::rlimit {
        rlim_cur: 1,
        rlim_max: 1,
    };
    // SAFETY: getrlimit and setrlimit on this process, with valid limits.
    unsafe {
        if libc::setrlimit(libc::RLIMIT_CORE, &core) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut files = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut files) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let bound = open_files(files.rlim_max);
        let files = libc::rlimit {
            rlim_cur: bound,
            rlim_max: bound,
        };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &files) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The open-file limit a worker runs with, soft and hard, from the daemon's
/// hard limit: at most [`MAX_OPEN_FILES`], all of it usable, never raised.
fn open_files(hard: libc::rlim_t) -> libc::rlim_t {
    hard.min(MAX_OPEN_FILES)
}

/// The worker's side of the channel: the handshake, each heartbeat and
/// (P8-2a-2, with the vault) the daemon's calls, each serve's listener taken
/// from `descriptors` as its frame arrives, until `bye` or the channel's end.
/// P8-2a-5 — `lines`, the process's log, crosses from its `ready` on.
pub(crate) async fn serve(
    ch: UnixStream,
    descriptors: Option<UnixStream>,
    lines: Option<log::Lines>,
) -> std::io::Result<()> {
    #[cfg(not(feature = "vault"))]
    drop(descriptors);
    let mut lines = lines;
    let (mut rd, wr) = ch.into_split();
    let link = Arc::new(link::Link::worker(wr));
    // What this worker serves: stopped all at once as its loop ends, or is
    // dropped.
    #[cfg(feature = "vault")]
    let serving = Arc::new(super::vault_connect::handed::Serving::default());
    #[cfg(feature = "vault")]
    let _stops_all = serving.all.clone().drop_guard();
    let ended = loop {
        let m = match recv(&mut rd).await {
            Ok(Some(m)) => m,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        let sent = match m {
            Msg::Hello { session, fence } => {
                // The daemon's stderr, which can be gone by now: a line lost
                // beats a worker lost (#2719).
                crate::logging::say_on_stderr(format_args!(
                    "roomlerd hive-worker: serving {session} at fence {fence}"
                ));
                let ready = Msg::Ready {
                    pid: std::process::id(),
                    version: VERSION.to_string(),
                };
                let sent = link.send(&ready).await;
                // P8-2a-5 — its log crosses from its `ready` on, so the
                // handshake is never a log line.
                if let Some(lines) = lines.take() {
                    tokio::spawn(log::forward(lines, Arc::clone(&link)));
                }
                info!(%session, fence, "hive: the session's worker is up");
                sent
            }
            Msg::Beat { n } => link.send(&Msg::Beat { n }).await,
            Msg::Bye => break Ok(()),
            Msg::Ready { .. } | Msg::Log { .. } => Ok(()),
            #[cfg(feature = "vault")]
            Msg::Call { id, call } => {
                // A serve's listener came just before its frame (P8-2a-4b:
                // the egress fronts' two, the HTTP one's first), so they are
                // taken here, in order, before the next frame is read.
                let listeners = match (&call, &descriptors) {
                    (Call::Serve { .. }, Some(ch)) => vec![fds::recv(ch).await],
                    (Call::Egress { .. }, Some(ch)) => {
                        let http = fds::recv(ch).await;
                        vec![http, fds::recv(ch).await]
                    }
                    _ => Vec::new(),
                };
                let (link, serving) = (Arc::clone(&link), Arc::clone(&serving));
                tokio::spawn(async move {
                    let answer = answer_call(&link, &serving, call, listeners);
                    let _ = link.answer(id, answer).await;
                });
                Ok(())
            }
            #[cfg(feature = "vault")]
            Msg::Answer { id, answer } => {
                link.answered(id, answer);
                Ok(())
            }
        };
        if let Err(e) = sent {
            break Err(e);
        }
    };
    link.end();
    ended
}

/// FR-90 P8-2a-2 — the worker's answer to one of the daemon's calls.
#[cfg(feature = "vault")]
fn answer_call(
    link: &Arc<link::Link>,
    serving: &Arc<super::vault_connect::handed::Serving>,
    call: Call,
    listeners: Vec<std::io::Result<std::os::fd::OwnedFd>>,
) -> Answer {
    use super::vault_connect::handed;
    let mut listeners = listeners.into_iter();
    let mut next = || match listeners.next() {
        Some(Ok(fd)) => Ok(fd),
        Some(Err(e)) => Err(Answer::Refused {
            why: format!("its listener did not come: {e}"),
        }),
        None => Err(Answer::Refused {
            why: "no listener came with it".into(),
        }),
    };
    match call {
        Call::Serve { endpoint, spec } => match next() {
            Ok(fd) => handed::serve_handed(serving, Arc::clone(link), endpoint, &spec, fd),
            Err(refused) => refused,
        },
        // P8-2a-4b — the run's egress fronts.
        Call::Egress { endpoint, spec } => match (next(), next()) {
            (Ok(http), Ok(socks)) => super::egress::handed::serve_handed(
                serving,
                Arc::clone(link),
                endpoint,
                &spec,
                http,
                socks,
            ),
            (Err(refused), _) | (_, Err(refused)) => refused,
        },
        Call::Close { endpoint } => handed::close_handed(serving, endpoint),
        Call::Values { .. }
        | Call::Decide { .. }
        | Call::Bound { .. }
        | Call::Valued { .. }
        | Call::Leaf { .. } => Answer::Refused {
            why: "the daemon answers that, never a worker".into(),
        },
    }
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
    /// A worker with `theirs` as its channel, and `descriptors` as the second
    /// one, the daemon's listeners come across.
    fn spawn(
        &self,
        theirs: std::os::unix::net::UnixStream,
        descriptors: std::os::unix::net::UnixStream,
    ) -> std::io::Result<Box<dyn Running>>;
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
    fn spawn(
        &self,
        theirs: std::os::unix::net::UnixStream,
        descriptors: std::os::unix::net::UnixStream,
    ) -> std::io::Result<Box<dyn Running>> {
        use std::os::fd::AsRawFd;
        // Both moved above the descriptors they land on, before the fork:
        // then neither move in the child can overwrite the other, nor land on
        // a standard descriptor the child's stdio is set up on (a daemon
        // started with one closed can be handed a channel as one).
        let theirs = above(theirs)?;
        let descriptors = above(descriptors)?;
        let (fd, desc) = (theirs.as_raw_fd(), descriptors.as_raw_fd());
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
        // SAFETY: between fork and exec only an async-signal-safe call:
        // dup2. Every descriptor the daemon makes is close-on-exec, so the
        // channels, moved onto fd 3 and fd 4 without that flag, are the ones
        // that cross.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(fd, CHANNEL_FD) < 0 || libc::dup2(desc, DESCRIPTORS_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        // The daemon's copies of the worker's ends go now: they are the
        // worker's, so the channels end when the worker does.
        drop((theirs, descriptors));
        Ok(Box::new(child))
    }
}

/// `s` on a descriptor above the two a worker's channels land on, its copy
/// close-on-exec.
fn above(s: std::os::unix::net::UnixStream) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd};
    if s.as_raw_fd() > DESCRIPTORS_FD {
        return Ok(s);
    }
    // SAFETY: fcntl duplicating a descriptor this process owns.
    let fd = unsafe { libc::fcntl(s.as_raw_fd(), libc::F_DUPFD_CLOEXEC, DESCRIPTORS_FD + 1) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a new descriptor that nothing else owns.
    Ok(unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) })
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
    /// FR-90 P8-2a-2 — the link of the worker that is up now, the one an
    /// endpoint is handed through.
    #[cfg(feature = "vault")]
    published: Published,
    /// Its supervisor, which the run's end leaves to finish on its own: only
    /// [`Worker::stop`] waits for it, and only the tests stop a worker so.
    #[cfg_attr(not(test), allow(dead_code))]
    task: Option<tokio::task::JoinHandle<()>>,
}

/// FR-90 P8-2a-2 — the link of a run's worker while one is up.
#[cfg(feature = "vault")]
type Published = Arc<Mutex<Option<Arc<link::Link>>>>;

/// FR-90 P8-2a-2 — what the daemon does once it gives up on a run's
/// worker, with the reason: the supervisor ends the run.
pub(crate) type GiveUp = Arc<dyn Fn(String) + Send + Sync>;

impl Worker {
    /// Start the worker of `session` at `fence`, and keep one up until
    /// stopped or given up on, `give_up` called then.
    pub(crate) fn start(
        session: ObjectId,
        fence: u64,
        spawner: Arc<dyn Spawn>,
        timing: Timing,
        give_up: GiveUp,
    ) -> Self {
        let stop = CancellationToken::new();
        let seen = Arc::new(Mutex::new(Seen::default()));
        #[cfg(feature = "vault")]
        let published: Published = Arc::default();
        let task = tokio::spawn(supervise(
            Run {
                session,
                fence,
                #[cfg(feature = "vault")]
                published: Arc::clone(&published),
            },
            spawner,
            timing,
            stop.clone(),
            Arc::clone(&seen),
            give_up,
        ));
        Self {
            stop,
            seen,
            #[cfg(feature = "vault")]
            published,
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

/// The run a worker serves, as its supervisor and its life take it.
struct Run {
    session: ObjectId,
    fence: u64,
    #[cfg(feature = "vault")]
    published: Published,
}

/// Keep a worker up for the run: start it, hand it its run, beat, and
/// restart it as it is lost, until stopped or given up on. Giving up ends
/// the run (`end_run`): where the owner turned workers on, its parts run in
/// one or not at all.
async fn supervise(
    run: Run,
    spawner: Arc<dyn Spawn>,
    timing: Timing,
    stop: CancellationToken,
    seen: Arc<Mutex<Seen>>,
    end_run: GiveUp,
) {
    let (session, fence) = (run.session, run.fence);
    let note = |f: &dyn Fn(&mut Seen)| f(&mut seen.lock().unwrap_or_else(|e| e.into_inner()));
    let give_up = |why: &str| {
        warn!(%session, fence, %why, "hive: a session's worker is given up on: the run ends");
        note(&|s| {
            s.gave_up = true;
            s.up = false;
        });
        end_run(format!("its worker kept failing ({why})"));
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
        let pairs = std::os::unix::net::UnixStream::pair()
            .and_then(|channel| std::os::unix::net::UnixStream::pair().map(|d| (channel, d)));
        let ended = match pairs {
            Ok(((ours, theirs), (ours_d, theirs_d))) => match spawner.spawn(theirs, theirs_d) {
                Ok(mut running) => {
                    let pid = running.pid();
                    note(&|s| s.pid = pid);
                    let tokio_of = |s: std::os::unix::net::UnixStream| {
                        s.set_nonblocking(true)
                            .and_then(|()| UnixStream::from_std(s))
                    };
                    let ended = match (tokio_of(ours), tokio_of(ours_d)) {
                        (Ok(ch), Ok(d)) => life(&run, ch, d, &timing, &stop, &seen).await,
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

/// One worker's life on its channel: the handshake, then the heartbeat and
/// (P8-2a-2, with the vault) the calls either way, until stopped or lost.
/// While it is up its link is published, for the run's endpoints, and every
/// endpoint handed to the run's workers so far is handed to it.
async fn life(
    run: &Run,
    ch: UnixStream,
    descriptors: UnixStream,
    timing: &Timing,
    stop: &CancellationToken,
    seen: &Arc<Mutex<Seen>>,
) -> Ended {
    let (session, fence) = (run.session, run.fence);
    let (rd, wr) = ch.into_split();
    let mut frames = Frames::read(rd);
    let link = Arc::new(link::Link::daemon(wr, descriptors));
    let hello = Msg::Hello {
        session: session.to_hex(),
        fence,
    };
    // The hello's write is inside the start bound too.
    let ready = tokio::select! {
        () = stop.cancelled() => return bye(&link, &mut frames, timing.grace).await,
        r = tokio::time::timeout(timing.start, async {
            link.send(&hello).await?;
            frames.next().await
        }) => r,
    };
    let pid = match ready {
        Ok(Ok(Some(Msg::Ready { pid, version }))) if version == VERSION => {
            info!(%session, fence, pid, "hive: a session's worker is up");
            let mut s = seen.lock().unwrap_or_else(|e| e.into_inner());
            s.up = true;
            s.pid = Some(pid);
            pid
        }
        Ok(Ok(Some(Msg::Ready { version, .. }))) => {
            return Ended::Foreign(format!("it is version {version}, the daemon {VERSION}"));
        }
        Ok(Ok(Some(other))) => return Ended::Lost(format!("it answered hello with {other:?}")),
        Ok(Ok(None)) => return Ended::Lost("it ended before it was ready".into()),
        Ok(Err(e)) => return Ended::Lost(format!("its channel failed: {e}")),
        Err(_) => return Ended::Lost("it was not ready in time".into()),
    };
    #[cfg(feature = "vault")]
    {
        *published(run) = Some(Arc::clone(&link));
        // On a task of its own: the hand-overs' answers come through the
        // loop below.
        let link = Arc::clone(&link);
        tokio::spawn(async move {
            super::vault_connect::handed::rehand(session, fence, &link).await;
            // P8-2a-4b — and the run's egress fronts.
            super::egress::handed::rehand(session, fence, &link).await;
        });
    }
    let mut relay = log::Relay::new(pid);
    let ended = beats(run, &link, &mut frames, timing, stop, &mut relay).await;
    #[cfg(feature = "vault")]
    published(run).take();
    link.end();
    ended
}

/// The run's published link.
#[cfg(feature = "vault")]
fn published(run: &Run) -> std::sync::MutexGuard<'_, Option<Arc<link::Link>>> {
    run.published.lock().unwrap_or_else(|e| e.into_inner())
}

/// The heartbeat and (with the vault) the calls either way, until stopped
/// or lost.
async fn beats(
    run: &Run,
    link: &Arc<link::Link>,
    frames: &mut Frames,
    timing: &Timing,
    stop: &CancellationToken,
    relay: &mut log::Relay,
) -> Ended {
    #[cfg(not(feature = "vault"))]
    let _ = run;
    let mut n: u64 = 0;
    let mut missed: u32 = 0;
    loop {
        n += 1;
        if let Err(e) = link.send(&Msg::Beat { n }).await {
            return Ended::Lost(format!("its channel refused a beat: {e}"));
        }
        let deadline = tokio::time::Instant::now() + timing.beat;
        let mut answered = false;
        loop {
            let got = tokio::select! {
                () = stop.cancelled() => return bye(link, frames, timing.grace).await,
                why = link.lost() => return Ended::Lost(why),
                r = tokio::time::timeout_at(deadline, frames.next()) => r,
            };
            match got {
                Ok(Ok(Some(Msg::Beat { n: m }))) if m == n => answered = true,
                // P8-2a-5 — a line of its log, into the daemon's.
                Ok(Ok(Some(Msg::Log {
                    level,
                    target,
                    line,
                }))) => relay.write(&level, &target, &line),
                #[cfg(feature = "vault")]
                Ok(Ok(Some(Msg::Answer { id, answer }))) => {
                    link.answered(id, answer);
                }
                #[cfg(feature = "vault")]
                Ok(Ok(Some(Msg::Call { id, call }))) => answer_worker(run, link, id, call),
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

/// FR-90 P8-2a-2 — the daemon's answer to one of its worker's calls, made
/// and sent on a task of its own, so the heartbeat never waits on it
/// (P8-2a-4b: a decision can wait on the vault). Each is answered for the
/// run this worker serves, whatever the call names.
#[cfg(feature = "vault")]
fn answer_worker(run: &Run, link: &Arc<link::Link>, id: u64, call: Call) {
    use super::egress::handed as egress;
    let (session, fence) = (run.session, run.fence);
    let link = Arc::clone(link);
    tokio::spawn(async move {
        let answer = match call {
            Call::Values { endpoint } => {
                super::vault_connect::handed::values(session, fence, endpoint)
            }
            Call::Decide { host, port } => egress::decide((session, fence), &host, port).await,
            Call::Bound { host, port } => egress::bound((session, fence), &host, port).await,
            Call::Valued { host, port } => egress::valued((session, fence), &host, port).await,
            Call::Leaf { host } => egress::leaf((session, fence), &host).await,
            Call::Serve { .. } | Call::Close { .. } | Call::Egress { .. } => Answer::Refused {
                why: "a worker asks the daemon for values and the egress proxy's answers alone"
                    .into(),
            },
        };
        let _ = link.answer(id, answer).await;
    });
}

/// The stop: `bye` to the worker, and its channel's end awaited, within the
/// grace. A worker still there after it is killed all the same.
async fn bye(link: &link::Link, frames: &mut Frames, grace: Duration) -> Ended {
    let _ = tokio::time::timeout(grace, async {
        if link.send(&Msg::Bye).await.is_ok() {
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
/// Should the daemon give up on it, the daemon's supervisor ends the run.
pub(crate) fn run_started(session: ObjectId, fence: u64, on: bool) {
    if !on {
        return;
    }
    let give_up: GiveUp = Arc::new(move |why| {
        if let Some(supervisor) = super::supervisor::global() {
            supervisor.stop(session, fence, why);
        }
    });
    match Program::this_binary() {
        Ok(program) => run_started_with(
            session,
            fence,
            Arc::new(program),
            Timing::default(),
            give_up,
        ),
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
    give_up: GiveUp,
) {
    let worker = Worker::start(session, fence, spawner, timing, give_up);
    // A run restarted here at the same fence replaces its worker: the old
    // one's supervisor is stopped as it drops.
    runs().insert((session, fence), worker);
}

/// FR-90 P8-2a-2 — whether the run has a worker: its device's owner turned
/// workers on.
#[cfg(feature = "vault")]
pub(crate) fn has(session: ObjectId, fence: u64) -> bool {
    runs().contains_key(&(session, fence))
}

/// FR-90 P8-2a-2 — the link of the run's worker, while one is up.
#[cfg(feature = "vault")]
pub(crate) fn link_of(session: ObjectId, fence: u64) -> Option<Arc<link::Link>> {
    let published = runs()
        .get(&(session, fence))
        .map(|w| Arc::clone(&w.published))?;
    published.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// FR-90 P8-2a-2 — [`link_of`], waiting up to [`UP_WAIT`] for a worker that
/// is starting: none once the daemon gave up on it.
#[cfg(feature = "vault")]
pub(crate) async fn up(session: ObjectId, fence: u64) -> Option<Arc<link::Link>> {
    let deadline = Instant::now() + UP_WAIT;
    loop {
        if let Some(link) = link_of(session, fence) {
            return Some(link);
        }
        let gave_up = runs()
            .get(&(session, fence))
            .is_none_or(|w| w.seen().gave_up);
        if gave_up || Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// FR-90 P8-2a-2 — a test's: the run's worker on a task of this process,
/// running the worker's own loop, so a test can follow what is handed to it.
#[cfg(all(test, feature = "vault"))]
pub(crate) fn run_in_process(session: ObjectId, fence: u64, give_up: GiveUp) {
    let spawner = InProcess {
        first_lost_after: None,
        first_without_descriptors: false,
        started: std::sync::atomic::AtomicUsize::new(0),
    };
    run_started_with(
        session,
        fence,
        Arc::new(spawner),
        Timing::default(),
        give_up,
    );
}

/// FR-90 P8-2a-2 — a test's: as [`run_in_process`], its first worker lost
/// `after` it started (its loop dropped, as a process's end drops it), the
/// next one staying, at [`Timing::quick`]'s bounds.
#[cfg(all(test, feature = "vault"))]
pub(crate) fn run_in_process_lost_after(session: ObjectId, fence: u64, after: Duration) {
    let spawner = InProcess {
        first_lost_after: Some(after),
        first_without_descriptors: false,
        started: std::sync::atomic::AtomicUsize::new(0),
    };
    let nobody: GiveUp = Arc::new(|_| {});
    run_started_with(session, fence, Arc::new(spawner), Timing::quick(), nobody);
}

/// FR-90 P8-2a-2 — a test's: as [`run_in_process`], its first worker's
/// descriptor channel gone from its start while its frames are still
/// served, the next one whole, at [`Timing::quick`]'s bounds.
#[cfg(all(test, feature = "vault"))]
pub(crate) fn run_in_process_first_without_descriptors(session: ObjectId, fence: u64) {
    let spawner = InProcess {
        first_lost_after: None,
        first_without_descriptors: true,
        started: std::sync::atomic::AtomicUsize::new(0),
    };
    let nobody: GiveUp = Arc::new(|_| {});
    run_started_with(session, fence, Arc::new(spawner), Timing::quick(), nobody);
}

/// FR-90 P8-2a-2 — a test's: a run whose worker never starts, soon given up
/// on (at [`Timing::quick`]'s bounds), `give_up` called then.
#[cfg(all(test, feature = "vault"))]
pub(crate) fn run_never_up(session: ObjectId, fence: u64, give_up: GiveUp) {
    run_started_with(session, fence, Arc::new(NeverUp), Timing::quick(), give_up);
}

#[cfg(test)]
impl Timing {
    /// A test's: bounds a worker's whole life fits in a blink.
    pub(crate) fn quick() -> Self {
        Self {
            start: Duration::from_millis(300),
            beat: Duration::from_millis(60),
            misses: 2,
            backoff: Duration::from_millis(10),
            backoff_max: Duration::from_millis(40),
            loop_window: Duration::from_secs(30),
            loop_max: 3,
            grace: Duration::from_millis(20),
        }
    }
}

#[cfg(all(test, feature = "vault"))]
struct NeverUp;

#[cfg(all(test, feature = "vault"))]
impl Spawn for NeverUp {
    fn spawn(
        &self,
        _theirs: std::os::unix::net::UnixStream,
        _descriptors: std::os::unix::net::UnixStream,
    ) -> std::io::Result<Box<dyn Running>> {
        Err(std::io::Error::other("no worker in this test"))
    }
}

#[cfg(all(test, feature = "vault"))]
struct InProcess {
    first_lost_after: Option<Duration>,
    first_without_descriptors: bool,
    started: std::sync::atomic::AtomicUsize,
}

#[cfg(all(test, feature = "vault"))]
struct OnTask(tokio::task::JoinHandle<std::io::Result<()>>);

#[cfg(all(test, feature = "vault"))]
#[async_trait::async_trait]
impl Running for OnTask {
    fn pid(&self) -> Option<u32> {
        None
    }

    async fn kill(&mut self) {
        self.0.abort();
    }
}

#[cfg(all(test, feature = "vault"))]
impl Spawn for InProcess {
    fn spawn(
        &self,
        theirs: std::os::unix::net::UnixStream,
        descriptors: std::os::unix::net::UnixStream,
    ) -> std::io::Result<Box<dyn Running>> {
        let tokio_of = |s: std::os::unix::net::UnixStream| {
            s.set_nonblocking(true)
                .and_then(|()| UnixStream::from_std(s))
        };
        let (ch, d) = (tokio_of(theirs)?, tokio_of(descriptors)?);
        let first = self
            .started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0;
        // Dropped here, where its first worker goes without it.
        let d = (!(first && self.first_without_descriptors)).then_some(d);
        let served = serve(ch, d, None);
        let task = match self.first_lost_after {
            Some(after) if first => tokio::spawn(async move {
                let _ = tokio::time::timeout(after, served).await;
                Ok(())
            }),
            _ => tokio::spawn(served),
        };
        Ok(Box::new(OnTask(task)))
    }
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

/// FR-90 P8-2a-2 — the second channel: listeners from the daemon.
#[cfg(feature = "vault")]
mod fds;
/// FR-90 P8-2a-2 — one side's end of the channel, and the calls across it.
pub(crate) mod link;
/// FR-90 P8-2a-5 — the worker's log, in the daemon's.
mod log;

#[cfg(test)]
mod tests;

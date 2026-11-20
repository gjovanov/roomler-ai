// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The desktop's keeper (#3665): a process of the virtual desktop's own that
//! starts the window manager, the startup apps and the apps and terminals
//! Remote Apps asks for (#4100), adopts whatever they leave behind, collects
//! every one of them that exits, and takes all of them down when it is told
//! to or when the daemon is gone.
//!
//! ## Why a process of its own
//!
//! The daemon's teardown (#1684, [`super::kill_tree_with`]) reaches what it can
//! find from what it recorded: the desktop's process group and the parent links
//! down from its own children, read in one `/proc` snapshot. A process that
//! left the group (`setsid`, as `at-spi-bus-launcher` does) AND whose parent
//! exited before that snapshot belongs to init, or to the nearest subreaper,
//! so no link leads to it. An X server's exit makes exactly that order, since
//! every client exits on its X I/O error before the supervisor looks. Such an
//! orphan stays in the service's cgroup, and one that ignores SIGTERM holds the
//! next `systemctl stop` for `TimeoutStopSec`.
//!
//! The keeper is a child subreaper (`PR_SET_CHILD_SUBREAPER`) and the parent of
//! the window manager, the apps and Remote Apps' clients. What they leave
//! behind is adopted by the
//! keeper instead of init, so it stays a descendant of a process the daemon
//! recorded, and the teardown's parent-link closure reaches it for its whole
//! life. The keeper also waits for every child it has, so nothing that exits
//! stays a zombie.
//!
//! The daemon cannot be that subreaper itself: it would adopt every orphan of
//! every child it has (exec and SSH sessions, the encoder probe), and it waits
//! for its children one pid at a time, leaving each status to its own waiter.
//! The keeper's only children are the desktop's, so its `waitpid(-1)` is right.
//!
//! ## Its life
//!
//! The daemon runs its own binary as `roomlerd vd-keeper` (the argument matched
//! whole, never as a prefix), in the desktop's process group, with a pipe on
//! stdin, and keeps the pipe's write end for the desktop's life: that pipe is
//! the keeper's lifeline. The keeper:
//!
//! 1. checks that it is in a process group of the desktop's own, makes itself
//!    a subreaper and arms its signals, then says `ready` on stderr, or
//!    `refused: <why>` and exits (the daemon then starts the clients itself,
//!    as before, and says why);
//! 2. reads the [`Job`], one JSON line that the daemon writes only after
//!    `ready`: a keeper the daemon gave up on never had a job, so it started
//!    nothing. The daemon kills such a keeper before its input ends, so an
//!    input that ends first, a parent that changed, or no job within
//!    [`JOB_WAIT`] means that the daemon is gone, or was stopped for that
//!    long (SIGSTOP, a debugger, a freezer), and the keeper goes on as in
//!    step 6, with nothing of its own to take down. A daemon that was only
//!    stopped loses the desktop it was starting, and its supervisor starts
//!    it again;
//! 3. starts the window manager and the apps with `/dev/null` on stdin, stdout
//!    and stderr, and waits for every child it has, its own or adopted. A
//!    window manager that exits, or fails to start, is started again the same
//!    way after the X server's own backoff (#4099, [`Book::start_wm`]): a
//!    second at first, doubling to five minutes, and a second again after one
//!    that ran a minute. An exit in a teardown starts nothing, and a start
//!    that comes due in one, or while the X server is not running, is dropped;
//! 4. starts the apps and terminals Remote Apps (FR-56) asks for on the
//!    desktop (#4100): each is a [`Start`] line on its input after the job,
//!    an argv and a deadline, read whole by [`Lines`]. It is started through
//!    the same [`spawn_client`] at the loop's tail, in the wake that read it,
//!    and answered on stderr, `start <id>: ` and an [`Answer`]. One read in a
//!    teardown, while the X server is not running, or past its deadline (the
//!    daemon has stopped waiting, and told the person it failed) is refused,
//!    and starts nothing. Their exits are collected and said nowhere;
//! 5. on SIGTERM alone (a service manager's stop signals every process of the
//!    unit at once), takes down what it started and stays, for [`LINGER`] at
//!    most, until its input ends;
//! 6. when the daemon is gone or tearing down, refuses the starts it read and
//!    has not answered, takes down what it started AND every other member of
//!    the desktop's group, the X server included, then exits. It hears that
//!    from the end of its input, which only the daemon can cause (by its
//!    teardown, or by dying), and, should anything else hold the write end,
//!    from its parent no longer being the daemon.
//!
//! A client's exit is reported only once the desktop has outlived it by
//! [`NOTICE_DELAY`]. The X server's death and a stop make every client exit
//! within milliseconds, before the keeper hears why, and such an exit is no
//! news. A window manager's start does not wait for that notice: its delay
//! runs from the exit, and the start looks at the teardown and the X server
//! when it comes due (#4099).
//!
//! Every write to stderr is non-blocking and ignores its error: once the daemon
//! is gone, stderr is a broken pipe, and nothing in the teardown may block or
//! panic on it.
#![cfg(target_os = "linux")]

use super::{
    Backoff, ProcInfo, Procs, RealProcs, describe_exit, is_running, kill_tree_with,
    read_proc_snapshot, with_wslg_pulse,
};
use std::ffi::OsString;
use std::os::fd::RawFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The hidden argument that makes `roomlerd` a keeper. Matched whole: an
/// argument that merely starts with it is not this one.
pub(super) const VERB: &str = "vd-keeper";

/// The keeper's name in `/proc/<pid>/comm` (15 bytes and the NUL), so that
/// `pgrep -x roomlerd` still finds the daemon alone.
const COMM: &[u8; 16] = b"roomlerd-keeper\0";

/// How long a keeper that got SIGTERM, without its input ending, waits for
/// that end before it leaves. A service manager's stop signals the keeper and
/// the daemon at once, and the daemon's teardown closes the input about 200 ms
/// later; a keeper that left on the signal alone would be an exit the
/// supervisor reports, which a stop has no reason to cause. A person's
/// `kill -TERM` of the keeper still ends in a restart of the desktop, after
/// this.
const LINGER: Duration = Duration::from_secs(10);

/// The longest job line the keeper reads.
const JOB_MAX: usize = 64 * 1024;

/// The longest line the keeper writes, newline included: a write of at most
/// `PIPE_BUF` bytes to a pipe is atomic, so a line is never split or
/// interleaved, only dropped whole.
const LINE_MAX: usize = 4096;

/// How long a teardown may take beyond its grace before the keeper gives up
/// on a child it cannot end.
const TEARDOWN_SLACK: Duration = Duration::from_secs(2);

/// The longest grace a job may ask for. The daemon asks for 1.5 s; a keeper
/// run by hand with a larger one still ends in bounded time.
const GRACE_MAX: Duration = Duration::from_secs(10);

/// How long the desktop must outlive a client's exit before the keeper
/// reports it. An X server's death ends every client on its I/O error, and a
/// service manager's stop signals every process at once, so in both cases
/// the clients go first; the end of input, the keeper's own SIGTERM or the
/// server's death follows within milliseconds, and the exit is then dropped,
/// because the desktop did not run on without that client.
const NOTICE_DELAY: Duration = Duration::from_secs(2);

/// #4099 — the shortest delay before the window manager is started again,
/// whatever its job says: a job's 0 would start one that exits at once in a
/// loop, a core spun as root.
const WM_DELAY_MIN: Duration = Duration::from_millis(100);

/// The longest the loop's `poll` waits, so that a parent that changed and a
/// held exit that came due are each seen within a second.
const POLL_MAX: Duration = Duration::from_secs(1);

/// How long the keeper waits for its job after `ready` (review R2-3). The
/// daemon sends it as soon as it reads `ready`, with no lock and no blocking
/// call in between. A keeper it gives up on was not ready within its wait
/// (`KEEPER_READY_TIMEOUT`, 5 s from the spawn), and the daemon SIGKILLs it
/// after that wait, once `op` is free, which a stop holds for its whole kill.
/// So a job that has not come by now has no daemon left to send it (one
/// that died before the keeper read its parent, while something else holds
/// the write end of the keeper's input), or a daemon that was stopped for
/// this long (review R3-3).
const JOB_WAIT: Duration = Duration::from_secs(15);

// Review R3-3 — `JOB_WAIT` outlasts the latest moment at which the daemon can
// still kill a keeper it gave up on: its ready wait, a stop's whole kill that
// its discard waits out (`TEARDOWN_GRACE`, then about 1 s for the SIGKILL to
// land and 1 s for the reap), and the discard's own reap of 1 s. A shorter
// wait would let an ordinary slow start end the desktop it was starting.
const _: () = assert!(
    JOB_WAIT.as_millis()
        > super::KEEPER_READY_TIMEOUT.as_millis() + super::TEARDOWN_GRACE.as_millis() + 3_000
);

/// The grace of the teardown a keeper runs before it has a job: the daemon's
/// own, which the job would have carried (review R3-3: one constant).
const NO_JOB_GRACE: Duration = super::KEEPER_GRACE;

/// The variable a test sets on a keeper it starts from the test binary
/// ([`tests::keeper_role`]). A keeper removes it from its children.
pub(super) const TEST_ROLE_ENV: &str = "ROOMLERD_TEST_VD_KEEPER";

/// What the keeper starts, sent by the daemon as one JSON line on the
/// keeper's stdin. Unknown fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Job {
    /// The `:N` display the clients run on.
    pub(super) display: String,
    /// The desktop's process group: the X server's pid. The keeper must be a
    /// member of it, and it must not be the daemon's own.
    pub(super) group: i32,
    /// The window manager.
    pub(super) wm: String,
    /// The startup apps.
    #[serde(default)]
    pub(super) apps: Vec<String>,
    /// How long the keeper's teardown waits after SIGTERM before SIGKILL.
    pub(super) grace_ms: u64,
    /// #4099 — the window manager's restart delay after its first exit, in
    /// milliseconds. A job without it (a daemon from before the field) gets
    /// the X server's own, [`super::RESTART_DELAY_FLOOR`].
    #[serde(default = "server_delay_floor_ms")]
    pub(super) wm_delay_floor_ms: u64,
    /// #4099 — the longest restart delay, in milliseconds: the X server's
    /// [`super::RESTART_DELAY_CEIL`] when the job does not say.
    #[serde(default = "server_delay_ceil_ms")]
    pub(super) wm_delay_ceil_ms: u64,
    /// #4099 — how long, in milliseconds, a window manager must have run for
    /// its exit to start the delays over: the X server's
    /// [`super::STABLE_AFTER`] when the job does not say.
    #[serde(default = "server_stable_after_ms")]
    pub(super) wm_stable_after_ms: u64,
}

/// The X server's [`super::RESTART_DELAY_FLOOR`], in a job's milliseconds.
fn server_delay_floor_ms() -> u64 {
    super::RESTART_DELAY_FLOOR.as_millis() as u64
}

/// The X server's [`super::RESTART_DELAY_CEIL`], in a job's milliseconds.
fn server_delay_ceil_ms() -> u64 {
    super::RESTART_DELAY_CEIL.as_millis() as u64
}

/// The X server's [`super::STABLE_AFTER`], in a job's milliseconds.
fn server_stable_after_ms() -> u64 {
    super::STABLE_AFTER.as_millis() as u64
}

impl Job {
    /// The job as the line the daemon writes.
    pub(super) fn line(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_default();
        line.push('\n');
        line
    }

    fn parse(line: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(line).map_err(|e| format!("a job it cannot read: {e}"))
    }
}

/// #4100 — an X client Remote Apps asks the keeper to start: one JSON line on
/// the keeper's input, after the job. Unknown fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Start {
    /// The daemon's number for this start, which the keeper's answer names.
    pub(super) id: u64,
    /// What to run: the program, then its arguments. An argv, never a shell
    /// line.
    pub(super) argv: Vec<String>,
    /// The `CLOCK_MONOTONIC` time, in milliseconds, after which the daemon
    /// has given up on this start: read later, it starts nothing.
    pub(super) deadline_ms: u64,
}

impl Start {
    /// The start as the line the daemon writes.
    pub(super) fn line(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_default();
        line.push('\n');
        line
    }

    fn parse(line: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(line).map_err(|e| format!("a start it cannot read: {e}"))
    }
}

/// #4100 — what begins the keeper's answer to a [`Start`] on its stderr:
/// `start <id>: ` and the [`Answer`] as JSON, one line. No other line of the
/// keeper's begins so (`ready`, `refused: `, `info: `, `warn: `).
const ANSWER_PREFIX: &str = "start ";

/// #4100 — the keeper's answer to a [`Start`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Answer {
    /// It runs: the keeper's child, in the desktop's group.
    Started { pid: i32 },
    /// Its start failed, with the error the start returned: the OS error's
    /// number when it has one, its kind and its text, so that the daemon
    /// says what its own start of it would have said.
    Failed {
        os_error: Option<i32>,
        kind: String,
        text: String,
    },
    /// Not started, and why.
    Refused { why: String },
}

impl Answer {
    /// The answer to start `id`, as the line the keeper says.
    pub(super) fn line(&self, id: u64) -> String {
        format!(
            "{ANSWER_PREFIX}{id}: {}",
            serde_json::to_string(self).unwrap_or_default()
        )
    }

    /// The start an answer line answers, and the answer, or why it cannot be
    /// read; `None` for every other line.
    pub(super) fn parse(line: &str) -> Option<(u64, Result<Self, String>)> {
        let (id, answer) = line.strip_prefix(ANSWER_PREFIX)?.split_once(": ")?;
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let id = id.parse().ok()?;
        Some((
            id,
            serde_json::from_str(answer).map_err(|e| format!("an answer it cannot read: {e}")),
        ))
    }
}

/// #4100 — the longest line the keeper reads as a start, its newline left
/// out: one write the daemon makes whole or not at all
/// ([`super::KEEPER_JOB_MAX`], `PIPE_BUF`, with the newline).
const START_MAX: usize = super::KEEPER_JOB_MAX - 1;

/// #4100 — the keeper's input after the job, cut into whole lines: what came
/// behind the job's line in its read, then each read. A line longer than
/// [`START_MAX`] is no start the daemon wrote: it is dropped whole, up to its
/// newline, and counted. The carry never holds more than one line's bound
/// and one read.
#[derive(Debug, Default)]
struct Lines {
    /// The beginning of a line whose newline has not come yet.
    carry: Vec<u8>,
    /// The rest of a line that passed the bound is being dropped.
    dropping: bool,
}

/// What [`Lines::feed`] found: the whole lines, in order, and how many lines
/// it dropped for their length.
#[derive(Debug, Default, PartialEq, Eq)]
struct Fed {
    lines: Vec<Vec<u8>>,
    too_long: usize,
}

impl Lines {
    fn feed(&mut self, mut bytes: &[u8]) -> Fed {
        let mut fed = Fed::default();
        while !bytes.is_empty() {
            let (part, newline) = match bytes.iter().position(|b| *b == b'\n') {
                Some(nl) => (&bytes[..nl], true),
                None => (bytes, false),
            };
            bytes = if newline {
                &bytes[part.len() + 1..]
            } else {
                &[]
            };
            if !self.dropping {
                self.carry.extend_from_slice(part);
                if self.carry.len() > START_MAX {
                    self.carry.clear();
                    self.dropping = true;
                    fed.too_long += 1;
                }
            }
            if newline {
                if self.dropping {
                    self.dropping = false;
                } else {
                    fed.lines.push(std::mem::take(&mut self.carry));
                }
            }
        }
        fed
    }
}

/// The starts in what [`Lines::feed`] found, each line that is none said in
/// a warning of its own.
fn starts_in(fed: Fed) -> Vec<Start> {
    for _ in 0..fed.too_long {
        say(&format!(
            "warn: a line of more than {START_MAX} bytes on its input was dropped: it is no start"
        ));
    }
    fed.lines
        .into_iter()
        .filter_map(|line| match Start::parse(&line) {
            Ok(start) => Some(start),
            Err(why) => {
                say(&format!("warn: {why}"));
                None
            }
        })
        .collect()
}

/// Answer each of `starts` with a refusal: the keeper is leaving, and starts
/// nothing more.
fn refuse_all(starts: &mut Vec<Start>, why: &str) {
    for start in starts.drain(..) {
        say(&Answer::Refused { why: why.into() }.line(start.id));
    }
}

/// Is this command line the keeper's: the program and [`VERB`], nothing more?
pub(super) fn is_keeper_argv(mut args: impl Iterator<Item = OsString>) -> bool {
    let _program = args.next();
    args.next().is_some_and(|a| a == VERB) && args.next().is_none()
}

/// May the keeper signal `pid`? Never 0, 1, -1 or any other group (a
/// negative pid), never itself, and never the daemon that started it.
pub(super) fn may_signal(pid: i32, me: i32, daemon: i32) -> bool {
    pid > 1 && pid != me && pid != daemon
}

// ---------------------------------------------------------------------------
// Output: lines on stderr that never block and never fail
// ---------------------------------------------------------------------------

/// Lines dropped because the pipe was full or broken.
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Write `line` and a newline to `fd` in one `write(2)`, without blocking (the
/// keeper's stderr is non-blocking) and without failing: a line the pipe
/// cannot take now, or ever, is dropped and counted. `true` when it was
/// written. A line longer than [`LINE_MAX`] is cut, so the write stays atomic.
fn say_to(fd: RawFd, line: &str) -> bool {
    let mut end = line.len().min(LINE_MAX - 1);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let mut buf = Vec::with_capacity(end + 1);
    buf.extend_from_slice(&line.as_bytes()[..end]);
    buf.push(b'\n');
    // SAFETY: `write(2)` from a live buffer of `buf.len()` bytes. Its error
    // (EAGAIN on a full pipe, EPIPE once the daemon is gone; SIGPIPE is
    // ignored) is the caller's to drop, never to act on.
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if n == buf.len() as isize {
        true
    } else {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        false
    }
}

/// [`say_to`] on stderr.
fn say(line: &str) {
    say_to(2, line);
}

/// Set `O_NONBLOCK` on `fd`, keeping its other status flags. The keeper's
/// stderr is a pipe of its own: its children get `/dev/null`. The daemon's
/// end of the keeper's input too, once the job is out (#4100).
pub(super) fn set_nonblocking(fd: RawFd) {
    // SAFETY: `fcntl(2)` on a descriptor number; on a closed one both calls
    // fail with EBADF, which leaves nothing to do.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

// ---------------------------------------------------------------------------
// Signals: a flag and a byte on a pipe, nothing else
// ---------------------------------------------------------------------------

/// SIGTERM, SIGINT or SIGHUP arrived.
static TERM: AtomicBool = AtomicBool::new(false);
/// The write end of the wake pipe, once it exists.
static WAKE: AtomicI32 = AtomicI32::new(-1);

/// Async-signal-safe: an atomic store and a `write(2)` of one byte to a
/// non-blocking pipe, errno saved around them. A full pipe already holds a
/// wake, so EAGAIN loses nothing.
extern "C" fn on_signal(sig: libc::c_int) {
    // SAFETY: `__errno_location` returns this thread's errno; a handler that
    // makes a system call restores errno for the code it interrupted.
    let saved = unsafe { *libc::__errno_location() };
    if sig != libc::SIGCHLD {
        TERM.store(true, Ordering::SeqCst);
    }
    let fd = WAKE.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = [1u8];
        // SAFETY: `write(2)` is async-signal-safe, from a live one-byte buffer.
        unsafe {
            libc::write(fd, byte.as_ptr().cast(), 1);
        }
    }
    // SAFETY: as above.
    unsafe {
        *libc::__errno_location() = saved;
    }
}

/// Is `sig` ignored in this process now? Read before the keeper catches it:
/// what the daemon ignored, the keeper inherited.
fn ignored(sig: libc::c_int) -> bool {
    // SAFETY: `sigaction(2)` with no new action only reads the current one
    // into a zeroed, live struct.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(sig, std::ptr::null(), &mut old) == 0 && old.sa_sigaction == libc::SIG_IGN
    }
}

/// Make the wake pipe and catch SIGTERM, SIGINT, SIGHUP and SIGCHLD. The
/// handlers are the process's, whichever thread runs this: a test runs the
/// keeper on a thread of the test harness. The read end of the pipe.
///
/// A signal the keeper found ignored stays ignored, and is not caught: a
/// daemon run under `nohup` ignores SIGHUP, and the clients it started itself
/// inherited that, so the keeper's clients inherit it too (a caught signal is
/// reset to its default at `exec`, an ignored one is not). SIGCHLD is always
/// caught: the keeper waits for its children.
fn arm_signals() -> std::io::Result<RawFd> {
    let mut fds: [libc::c_int; 2] = [-1, -1];
    // SAFETY: `pipe2(2)` into a live two-element array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    WAKE.store(fds[1], Ordering::SeqCst);
    for (sig, flags) in [
        (libc::SIGTERM, libc::SA_RESTART),
        (libc::SIGINT, libc::SA_RESTART),
        (libc::SIGHUP, libc::SA_RESTART),
        (libc::SIGCHLD, libc::SA_RESTART | libc::SA_NOCLDSTOP),
    ] {
        if sig != libc::SIGCHLD && ignored(sig) {
            continue;
        }
        // SAFETY: `sigaction` is plain data; all-zero is a valid value, and
        // every field the call reads is set below.
        let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
        sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        sa.sa_flags = flags;
        // SAFETY: `sigemptyset` on the live mask, `sigaction(2)` with a live
        // action and no old one asked for.
        let rc = unsafe {
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut())
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(fds[0])
}

/// Read the wake pipe empty.
fn drain(fd: RawFd) {
    let mut buf = [0u8; 64];
    // SAFETY: `read(2)` into a live buffer; the pipe is non-blocking, so the
    // loop ends at EAGAIN.
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

// ---------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------

/// Has the daemon that started the keeper gone? Its orphans go to a
/// subreaper or init, so the keeper's parent is then another process.
fn daemon_gone(daemon: i32) -> bool {
    // SAFETY: `getppid(2)` cannot fail.
    unsafe { libc::getppid() != daemon }
}

/// [`read_job_and_rest`] without the rest: the job alone.
#[cfg(test)]
fn read_job(fd: RawFd, daemon: i32, wait: Duration) -> Result<Option<Job>, String> {
    read_job_and_rest(fd, daemon, wait).map(|read| read.map(|(job, _)| job))
}

/// Read the job's line from `fd`, and what came behind it in the same read:
/// #4100 — the starts the daemon wrote after the job, which the keeper's loop
/// reads first. `Ok(None)` when the input ends before a whole line, the
/// daemon is gone, or no whole line came within `wait`: the daemon left
/// before it said what to start.
fn read_job_and_rest(
    fd: RawFd,
    daemon: i32,
    wait: Duration,
) -> Result<Option<(Job, Vec<u8>)>, String> {
    let deadline = Instant::now() + wait;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let rest = buf.split_off(nl + 1);
            return Job::parse(&buf[..nl]).map(|job| Some((job, rest)));
        }
        if buf.len() > JOB_MAX {
            return Err(format!("a job longer than {JOB_MAX} bytes"));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll(2)` over one live descriptor; a signal returns early.
        if unsafe { libc::poll(&mut pfd, 1, 1000) } <= 0 {
            if daemon_gone(daemon) {
                return Ok(None);
            }
            continue;
        }
        // SAFETY: `read(2)` into a live buffer of `chunk.len()` bytes.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n == 0 {
            return Ok(None);
        }
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("its input could not be read: {e}"));
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
}

// ---------------------------------------------------------------------------
// The children
// ---------------------------------------------------------------------------

/// What the keeper started, so an exit can be told apart from an adopted
/// orphan's.
#[derive(Debug, Default)]
struct Book {
    wm: Option<i32>,
    /// #4099 — when the running window manager was started.
    wm_since: Option<Instant>,
    /// #4099 — when the window manager is to be started again, after an
    /// exit or a start that failed.
    wm_due: Option<Instant>,
    /// #4099 — the window manager's restart delays.
    wm_delays: WmDelays,
    apps: Vec<(i32, String)>,
    /// #4100 — the X clients Remote Apps asked for and the keeper started.
    clients: Vec<i32>,
    /// Adopted processes collected (orphans of the desktop's processes).
    adopted: u64,
    /// Exits are expected now: the keeper is taking the desktop down.
    tearing_down: bool,
    /// Exits of the keeper's own clients, each with the line that reports it,
    /// held for [`NOTICE_DELAY`] ([`Book::say_notices`]).
    notices: Vec<(Instant, String)>,
}

/// #4099 — the window manager's restart delays: the X server's backoff
/// (`supervise`'s), on the job's numbers.
struct WmDelays {
    delays: Backoff,
    /// A window manager that ran this long starts the delays over.
    stable_after: Duration,
}

impl WmDelays {
    /// The job's numbers, its floor at least [`WM_DELAY_MIN`] and its
    /// ceiling at least its floor.
    fn of(job: &Job) -> Self {
        let floor = Duration::from_millis(job.wm_delay_floor_ms).max(WM_DELAY_MIN);
        let ceil = Duration::from_millis(job.wm_delay_ceil_ms).max(floor);
        Self {
            delays: Backoff::new(floor, ceil),
            stable_after: Duration::from_millis(job.wm_stable_after_ms),
        }
    }

    /// The delay before the next start: after a window manager that ran
    /// `up_for`, or after a start that failed (`None`). A run of
    /// `stable_after` or longer starts the delays over, as the X server's
    /// does; a failed start never does.
    fn after(&mut self, up_for: Option<Duration>) -> Duration {
        if up_for.is_some_and(|up| up >= self.stable_after) {
            self.delays.reset();
        }
        self.delays.next()
    }
}

impl Default for WmDelays {
    /// The X server's numbers, as a job without its own gets them.
    fn default() -> Self {
        Self {
            delays: Backoff::new(super::RESTART_DELAY_FLOOR, super::RESTART_DELAY_CEIL),
            stable_after: super::STABLE_AFTER,
        }
    }
}

impl std::fmt::Debug for WmDelays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WmDelays")
            .field("next", &self.delays.peek())
            .field("stable_after", &self.stable_after)
            .finish()
    }
}

/// What a sweep of `waitpid` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Left {
    /// Children are still running.
    Some,
    /// The keeper has no child left.
    None,
}

impl Book {
    /// Start the window manager and the apps, as the daemon did before the
    /// keeper (`Desktop::start_clients_directly`): `DISPLAY` set, WSLg's
    /// PulseAudio where it exists, and nothing of the keeper's but its
    /// environment, its working directory and its umask.
    fn start(&mut self, job: &Job) {
        self.wm_delays = WmDelays::of(job);
        self.start_wm(job);
        for app in &job.apps {
            match spawn_client(app, &[], &job.display) {
                Ok(pid) => self.apps.push((pid, app.clone())),
                Err(e) => say(&format!("warn: startup app {app} failed to launch: {e}")),
            }
        }
    }

    /// Start the window manager: its first start and every one after it
    /// (#4099), all through [`spawn_client`], so that each gets what the
    /// first got. One that fails to start is tried again after the next
    /// delay, and said in one line.
    fn start_wm(&mut self, job: &Job) {
        match spawn_client(&job.wm, &[], &job.display) {
            Ok(pid) => {
                self.wm = Some(pid);
                self.wm_since = Some(Instant::now());
            }
            Err(e) => {
                let delay = self.wm_again_after(None);
                say(&format!(
                    "warn: the window manager {} failed to start: {e} — it is tried again in {delay:?}",
                    job.wm
                ));
            }
        }
    }

    /// Schedule the window manager's next start, after one that ran
    /// `up_for`, or after a start that failed (`None`); the delay it waits.
    fn wm_again_after(&mut self, up_for: Option<Duration>) -> Duration {
        let delay = self.wm_delays.after(up_for);
        self.wm_due = Some(Instant::now() + delay);
        delay
    }

    /// Start the window manager again once its delay has passed: never in a
    /// teardown, and never onto an X server that is not running
    /// (`server_runs`), each read now, at the start, not at the exit. A start
    /// refused is dropped, never left due: a due time in the past would make
    /// every `poll` a 0 ms one.
    fn start_wm_when_due(&mut self, job: &Job, server_runs: impl FnOnce() -> bool) {
        if self.wm_due.is_none_or(|due| Instant::now() < due) {
            return;
        }
        self.wm_due = None;
        if self.tearing_down {
            return;
        }
        if !server_runs() {
            return;
        }
        self.start_wm(job);
    }

    /// #4100 — start an X client Remote Apps asked for, on the desktop's
    /// display: never in a teardown, never onto an X server that is not
    /// running (`server_runs`, read now), and never past the start's deadline,
    /// when the daemon has stopped waiting for it and its launch has failed.
    fn start_x_client(
        &mut self,
        start: &Start,
        display: &str,
        server_runs: impl FnOnce() -> bool,
    ) -> Answer {
        let refused = |why: &str| Answer::Refused { why: why.into() };
        if self.tearing_down {
            return refused("the desktop is being taken down");
        }
        if !server_runs() {
            return refused("its X server is not running");
        }
        if super::monotonic_ms() > start.deadline_ms {
            return refused("it came after the daemon stopped waiting for it");
        }
        let Some((program, args)) = start.argv.split_first() else {
            return refused("it names no program");
        };
        match spawn_client(program, args, display) {
            Ok(pid) => {
                self.clients.push(pid);
                Answer::Started { pid }
            }
            Err(e) => Answer::Failed {
                os_error: e.raw_os_error(),
                kind: format!("{:?}", e.kind()),
                text: e.to_string(),
            },
        }
    }

    /// How long the loop's `poll` may wait, in its milliseconds: [`POLL_MAX`],
    /// or until the window manager's start is due, rounded up so that it
    /// never wakes before.
    fn wait_ms(&self) -> libc::c_int {
        let wait = self.wm_due.map_or(POLL_MAX, |due| {
            due.saturating_duration_since(Instant::now()).min(POLL_MAX)
        });
        wait.as_nanos().div_ceil(1_000_000) as libc::c_int
    }

    /// Collect every child that has exited, the keeper's own and adopted
    /// ones alike, and say which of the keeper's own left.
    fn reap(&mut self) -> Left {
        loop {
            let mut status: libc::c_int = 0;
            // SAFETY: `waitpid(2)` with a live out-pointer. `-1` is right
            // here, and only here: every child of the keeper is the desktop's.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid > 0 {
                self.collected(pid, status);
                continue;
            }
            if pid == 0 {
                return Left::Some;
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::ECHILD) => return Left::None,
                _ => return Left::Some,
            }
        }
    }

    fn collected(&mut self, pid: i32, status: libc::c_int) {
        let how = describe_status(status);
        let line = if self.wm == Some(pid) {
            self.wm = None;
            let up_for = self.wm_since.take().map_or(Duration::ZERO, |t| t.elapsed());
            // An exit in a teardown is expected: nothing to start or say.
            if self.tearing_down {
                return;
            }
            // #4099 — only scheduled here: the start, and its look at the
            // teardown and the X server, come at the loop's tail.
            let delay = self.wm_again_after(Some(up_for));
            format!(
                "warn: the window manager ({pid}) exited: {how} — it is started again {delay:?} after its exit"
            )
        } else if let Some(i) = self.apps.iter().position(|(p, _)| *p == pid) {
            let (_, app) = self.apps.swap_remove(i);
            format!("info: startup app {app} ({pid}) exited: {how}")
        } else if let Some(i) = self.clients.iter().position(|p| *p == pid) {
            // #4100 — a client Remote Apps started: people close these all
            // day, so its exit is collected and said nowhere.
            self.clients.swap_remove(i);
            return;
        } else {
            self.adopted += 1;
            return;
        };
        if !self.tearing_down {
            self.notices.push((Instant::now(), line));
        }
    }

    /// The desktop is going down: no held exit is news any more.
    fn tear_down(&mut self) {
        self.tearing_down = true;
        self.notices.clear();
    }

    /// Say each held exit that the desktop has outlived by [`NOTICE_DELAY`],
    /// while `server_alive` says its X server still runs; drop it when the
    /// server died. Exits not yet due wait for a later call.
    fn say_notices(&mut self, server_alive: impl FnOnce() -> bool) {
        if !self
            .notices
            .iter()
            .any(|(at, _)| at.elapsed() >= NOTICE_DELAY)
        {
            return;
        }
        let alive = !self.tearing_down && server_alive();
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.notices)
            .into_iter()
            .partition(|(at, _)| at.elapsed() >= NOTICE_DELAY);
        self.notices = later;
        if alive {
            for (_, line) in due {
                say(&line);
            }
        }
    }
}

/// A `waitpid` status in the words the supervisor uses for the X server.
fn describe_status(status: libc::c_int) -> String {
    if libc::WIFEXITED(status) {
        describe_exit(libc::CLD_EXITED, libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        let code = if libc::WCOREDUMP(status) {
            libc::CLD_DUMPED
        } else {
            libc::CLD_KILLED
        };
        describe_exit(code, libc::WTERMSIG(status))
    } else {
        format!("status {status}")
    }
}

/// Start one client, `program` with `args`, with `/dev/null` on its stdio and
/// nothing else of the keeper's open. Its `Child` is dropped: the keeper's
/// loop waits for every child it has, by pid.
#[allow(clippy::zombie_processes)] // reaped by `Book::reap` (`waitpid(-1)`)
fn spawn_client(program: &str, args: &[String], display: &str) -> std::io::Result<i32> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env("DISPLAY", display)
        .env_remove(TEST_ROLE_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    with_wslg_pulse(&mut cmd);
    tunnel_core::child::inherit_only_stdio(&mut cmd);
    Ok(cmd.spawn()?.id() as i32)
}

// ---------------------------------------------------------------------------
// The teardown
// ---------------------------------------------------------------------------

/// [`kill_tree_with`]'s operations, as the keeper performs them. Every pid it
/// signals was selected from a snapshot and matched its start time an
/// instant before (the keeper's only root is itself, which `select_targets`
/// drops), and [`may_signal`] refuses the rest. It never signals a group: it
/// is a member of the desktop's, so a `killpg` would end the keeper in the
/// middle of its own teardown. It reaps nothing inside a pass; the keeper
/// collects its children after each pass, by `waitpid(-1)`.
struct KeeperProcs {
    me: i32,
    daemon: i32,
}

impl Procs for KeeperProcs {
    fn snapshot(&mut self) -> Vec<ProcInfo> {
        read_proc_snapshot()
    }
    fn is_running(&mut self, pid: i32) -> bool {
        is_running(pid)
    }
    fn same_process_now(&mut self, info: &ProcInfo) -> bool {
        RealProcs.same_process_now(info)
    }
    fn signal(&mut self, pid: i32, sig: i32) {
        if may_signal(pid, self.me, self.daemon) {
            // SAFETY: `kill(2)` on a positive pid that is neither the keeper
            // nor the daemon, and whose start time `kill_tree_with` re-read
            // and matched against the snapshot that selected it.
            unsafe {
                libc::kill(pid, sig);
            }
        }
    }
    fn killpg(&mut self, _pgid: i32, _sig: i32) {}
    fn reap(&mut self, _roots: &[i32]) -> bool {
        true
    }
    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Take down everything the keeper has, and with `group` every other member
/// of the desktop's group: SIGTERM, `grace`, SIGKILL, collect. Then again,
/// with no grace, while any child is left, so that an orphan adopted during a
/// pass (its parent ended by that pass) is ended too. What it cannot end (a
/// process that forks faster than this loop kills) is counted in a warning.
fn teardown(me: i32, daemon: i32, group: Option<i32>, grace: Duration, book: &mut Book) {
    book.tear_down();
    let mut procs = KeeperProcs { me, daemon };
    if let Err(left) = teardown_with(&mut procs, me, group, grace, &mut |_| book.reap()) {
        say(&format!(
            "warn: {left} processes of the desktop were still running at the end of the keeper's teardown"
        ));
    }
}

/// [`teardown`]'s passes over `procs`; `collect` reaps what exited and says
/// whether any child is left. `Err` with the number of the keeper's children
/// still running when it gave up.
///
/// Its bound: the first pass takes `grace` and then up to one settle (1 s)
/// for its SIGKILL to land. Further passes have no grace and start only until
/// `grace` + [`TEARDOWN_SLACK`] after the start, and the last one can add one
/// settle: at most about `grace` + 3 s, 4.5 s with the daemon's 1.5 s.
pub(super) fn teardown_with(
    procs: &mut dyn Procs,
    me: i32,
    group: Option<i32>,
    grace: Duration,
    collect: &mut dyn FnMut(&mut dyn Procs) -> Left,
) -> Result<(), usize> {
    let deadline = Instant::now() + grace + TEARDOWN_SLACK;
    kill_tree_with(procs, &[me], group, grace);
    loop {
        if collect(procs) == Left::None {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(procs.snapshot().iter().filter(|p| p.ppid == me).count());
        }
        kill_tree_with(procs, &[me], group, Duration::ZERO);
        procs.sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// The keeper
// ---------------------------------------------------------------------------

/// Run the keeper; its exit code. See the module docs.
pub(super) fn run() -> i32 {
    // SAFETY: `prctl(PR_SET_NAME)` with a NUL-terminated buffer of 16 bytes.
    // It names the calling thread, which is the whole process's name when
    // that thread is the main one, as it is in the daemon's binary.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, COMM.as_ptr() as libc::c_ulong, 0, 0, 0);
    }
    // SAFETY: a signal disposition. Rust's runtime set it already; a write to
    // a broken stderr must be an EPIPE, never the end of the keeper.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    // A terminal it shares with a shell (a keeper run by hand) keeps its
    // flags; the daemon gives it a pipe of its own.
    // SAFETY: `isatty(3)` on a descriptor number.
    if unsafe { libc::isatty(2) } == 0 {
        set_nonblocking(2);
    }
    let me = std::process::id() as i32;
    // SAFETY: `getppid(2)` and `getpgrp(2)` cannot fail; `getpgid(2)` of the
    // daemon is -1 once it is gone.
    let (daemon, group) = unsafe { (libc::getppid(), libc::getpgrp()) };
    let daemons_group = unsafe { libc::getpgid(daemon) };
    if group <= 1 || group == me || group == daemons_group {
        say(&format!(
            "refused: not in a process group of the desktop's own (keeper {group}, daemon {daemons_group})"
        ));
        return 2;
    }
    // SAFETY: `prctl(PR_SET_CHILD_SUBREAPER)`; a seccomp filter or an old
    // kernel refuses it, and then there is no keeper.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        say(&format!(
            "refused: it cannot adopt orphans (PR_SET_CHILD_SUBREAPER): {}",
            std::io::Error::last_os_error()
        ));
        return 2;
    }
    let wake = match arm_signals() {
        Ok(fd) => fd,
        Err(e) => {
            say(&format!("refused: its signals could not be armed: {e}"));
            return 2;
        }
    };
    say("ready");
    // #3665 — the job comes only now: a keeper the daemon gave up on before
    // `ready` never gets one and starts nothing. The daemon kills a keeper it
    // gives up on before its input ends, so no job here (the input ended,
    // the parent changed, or nothing came within `JOB_WAIT`) means that the
    // daemon left between `ready` and the job, or was stopped for that long
    // (review R3-3). The desktop goes with it, as it does once the keeper
    // has its job (review R2-3).
    let (job, rest) = match read_job_and_rest(0, daemon, JOB_WAIT) {
        Ok(Some(read)) => read,
        Ok(None) => {
            // Review R3-4 — a daemon that left reads nothing of this, so the
            // line says what the keeper saw, which is true for whoever reads
            // it: a daemon that was stopped, or a person.
            say(&format!(
                "info: no job came (its input ended, its parent changed, or {JOB_WAIT:?} passed) — taking the whole desktop down"
            ));
            teardown(me, daemon, Some(group), NO_JOB_GRACE, &mut Book::default());
            return 0;
        }
        Err(why) => {
            say(&format!("refused: {why}"));
            return 2;
        }
    };
    if job.group != group {
        say(&format!(
            "refused: the job is for process group {}, and the keeper is in {group}",
            job.group
        ));
        return 2;
    }
    let mut book = Book::default();
    if !TERM.load(Ordering::SeqCst) {
        book.start(&job);
    }
    serve(me, daemon, group, wake, &job, &rest, &mut book)
}

/// The keeper's loop: collect children, report the exits the desktop
/// outlived, start the X clients Remote Apps asks for (#4100), and end when
/// the daemon is gone or tearing down (the whole desktop) or on SIGTERM (what
/// the keeper started, then the linger). `rest` is what came behind the job.
fn serve(
    me: i32,
    daemon: i32,
    group: i32,
    wake: RawFd,
    job: &Job,
    rest: &[u8],
    book: &mut Book,
) -> i32 {
    let grace = Duration::from_millis(job.grace_ms).min(GRACE_MAX);
    let mut termed: Option<Instant> = None;
    let mut input = [0u8; 4096];
    // #4100 — the starts read and not answered yet: what came behind the job
    // first, answered in the loop's first wake.
    let mut lines = Lines::default();
    let mut starts = starts_in(lines.feed(rest));
    loop {
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // A start already read is answered in this wake, without a wait.
        let wait = if starts.is_empty() { book.wait_ms() } else { 0 };
        // SAFETY: `poll(2)` over a live array of two descriptors. A signal
        // makes it return early (EINTR), which the loop treats as a wake.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, wait) };
        drain(wake);
        book.reap();
        let mut ended = false;
        if ready > 0 && fds[0].revents != 0 {
            // SAFETY: `read(2)` into a live buffer. #4100: after the job, the
            // input holds starts of X clients for Remote Apps, a line each.
            let n = unsafe { libc::read(0, input.as_mut_ptr().cast(), input.len()) };
            ended = n == 0
                || (n < 0
                    && !matches!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::EINTR) | Some(libc::EAGAIN)
                    ));
            if n > 0 {
                starts.extend(starts_in(lines.feed(&input[..n as usize])));
            }
        }
        if ended {
            say("info: the daemon closed the desktop's input — taking the whole desktop down");
            refuse_all(&mut starts, "the desktop is being taken down");
            teardown(me, daemon, Some(group), grace, book);
            say_goodbye(book);
            return 0;
        }
        // A second notice, for a write end that something besides the daemon
        // holds: the daemon's orphans, the keeper among them, have another
        // parent now.
        if daemon_gone(daemon) {
            say("info: the daemon is gone — taking the whole desktop down");
            refuse_all(&mut starts, "the desktop is being taken down");
            teardown(me, daemon, Some(group), grace, book);
            say_goodbye(book);
            return 0;
        }
        if termed.is_none() && TERM.load(Ordering::SeqCst) {
            say(
                "info: asked to stop — taking down what the desktop started, then waiting for the daemon",
            );
            teardown(me, daemon, None, grace, book);
            termed = Some(Instant::now());
        }
        if termed.is_some_and(|t| t.elapsed() >= LINGER) {
            say("info: the daemon did not close the desktop's input after the stop — leaving");
            refuse_all(&mut starts, "the desktop is being taken down");
            say_goodbye(book);
            return 0;
        }
        // The X server is the desktop's group leader, and the daemon's child:
        // dead, it stays a zombie until the daemon's teardown reaps it, and
        // a member of its group keeps its number from reuse.
        book.say_notices(|| is_running(group));
        // #4099 — a window manager that exited is started again here, once
        // its delay has passed, and only while the X server runs.
        book.start_wm_when_due(job, || is_running(group));
        // #4100 — each start read, answered in the wake that read it, the
        // teardown, the X server and its deadline each looked at now.
        for start in starts.drain(..) {
            let answer = book.start_x_client(&start, &job.display, || is_running(group));
            say(&answer.line(start.id));
        }
    }
}

/// The keeper's last line: what it collected that it had not started.
fn say_goodbye(book: &Book) {
    let dropped = DROPPED.load(Ordering::Relaxed);
    say(&format!(
        "info: keeper done: {} adopted processes collected, {dropped} log lines dropped",
        book.adopted
    ));
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Is this test binary running `test` (its path below the crate) alone, as
    /// a role: `--exact` and the test's own name on its command line? A role's
    /// variable that leaked into a plain run of the suite must not make a
    /// subreaper of the process every test shares, or end it midway.
    pub(in crate::virtual_desktop) fn runs_alone_as(test: &str) -> bool {
        let args: Vec<String> = std::env::args().skip(1).collect();
        args.iter().any(|a| a == "--exact") && args.iter().any(|a| a == test)
    }

    /// The keeper, as a test starts it: this test binary re-run with this
    /// function's name, `--exact`, and [`TEST_ROLE_ENV`] set. In a plain run of
    /// the suite it does nothing.
    #[test]
    fn keeper_role() {
        if std::env::var_os(TEST_ROLE_ENV).is_none()
            || !runs_alone_as("virtual_desktop::keeper::tests::keeper_role")
        {
            return;
        }
        std::process::exit(run());
    }

    fn argv(args: &[&str]) -> impl Iterator<Item = OsString> {
        args.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn only_the_exact_verb_makes_a_keeper() {
        assert!(is_keeper_argv(argv(&["roomlerd", "vd-keeper"])));
        assert!(is_keeper_argv(argv(&["/proc/self/exe", "vd-keeper"])));
        for no in [
            &[][..],
            &["roomlerd"][..],
            &["roomlerd", "run"][..],
            &["roomlerd", "vd-keeper", "extra"][..],
            &["roomlerd", "vd-keeperx"][..],
            &["roomlerd", "vd-keepe"][..],
            &["roomlerd", "--vd-keeper"][..],
            &["roomlerd", " vd-keeper"][..],
            &["roomlerd", "VD-KEEPER"][..],
        ] {
            assert!(!is_keeper_argv(argv(no)), "{no:?} is not the keeper");
        }
    }

    /// A pipe of this test's own: (read end, write end), both close-on-exec.
    fn pipe() -> (RawFd, RawFd) {
        let mut fds: [libc::c_int; 2] = [-1, -1];
        // SAFETY: `pipe2(2)` into a live two-element array.
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        assert_eq!(rc, 0, "pipe2: {}", std::io::Error::last_os_error());
        (fds[0], fds[1])
    }

    fn close(fd: RawFd) {
        // SAFETY: closes a descriptor this test made and closes once.
        unsafe {
            libc::close(fd);
        }
    }

    fn write_all(fd: RawFd, bytes: &[u8]) {
        // SAFETY: `write(2)` from a live buffer; the pipe is empty and the
        // bytes fit in it.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(n, bytes.len() as isize);
    }

    #[test]
    fn the_job_survives_the_pipe() {
        let job = Job {
            display: ":100".into(),
            group: 4242,
            wm: "/opt/my wm/openbox".into(),
            apps: vec!["xterm".into(), "/tmp/it's \"quoted\"\\ and\nnewline".into()],
            grace_ms: 1500,
            wm_delay_floor_ms: 250,
            wm_delay_ceil_ms: 4000,
            wm_stable_after_ms: 30_000,
        };
        let line = job.line();
        assert_eq!(line.matches('\n').count(), 1, "one line: {line:?}");
        // The "daemon" is whoever runs this test process: never gone here.
        // SAFETY: `getppid(2)` cannot fail.
        let parent = unsafe { libc::getppid() };
        let (r, w) = pipe();
        write_all(w, line.as_bytes());
        // A later line is the keeper loop's (#4100: a start): the job's
        // reader stops at the first.
        write_all(w, b"{\"later\":true}\n");
        close(w);
        let read = read_job(r, parent, JOB_WAIT);
        close(r);
        assert_eq!(read, Ok(Some(job)));

        // A newer daemon's field is ignored; a missing app list is empty.
        let (r, w) = pipe();
        write_all(
            w,
            b"{\"display\":\":7\",\"group\":9,\"wm\":\"openbox\",\"grace_ms\":1,\"new\":[1]}\n",
        );
        close(w);
        let read = read_job(r, parent, JOB_WAIT).unwrap().unwrap();
        close(r);
        assert!(read.apps.is_empty());

        // A line it cannot read is refused with a reason.
        let (r, w) = pipe();
        write_all(w, b"not json\n");
        close(w);
        let read = read_job(r, parent, JOB_WAIT);
        close(r);
        assert!(read.is_err(), "{read:?}");

        // The input ends before a whole line: the daemon left.
        let (r, w) = pipe();
        write_all(w, b"{\"display\"");
        close(w);
        let read = read_job(r, parent, JOB_WAIT);
        close(r);
        assert_eq!(read, Ok(None));

        // Review R1-9: the input stays open (something besides the daemon
        // holds its write end) but the daemon is gone: no job, after one
        // quiet poll. -1 is no process's parent.
        let (r, w) = pipe();
        write_all(w, b"{\"display\"");
        let asked = Instant::now();
        let read = read_job(r, -1, JOB_WAIT);
        let took = asked.elapsed();
        close(w);
        close(r);
        assert_eq!(read, Ok(None));
        assert!(took < Duration::from_secs(3), "it took {took:?}");

        // Review R2-3: the input stays open and the parent never changes, as
        // when the daemon died before the keeper read its parent: no job,
        // once the wait is over. On a thread, so that a reader that waits
        // for ever fails this test instead of hanging it.
        let (r, w) = pipe();
        write_all(w, b"{\"display\"");
        let (tx, rx) = std::sync::mpsc::channel();
        let asked = Instant::now();
        let reader = std::thread::spawn(move || {
            let _ = tx.send(read_job(r, parent, Duration::from_millis(1500)));
        });
        let read = rx.recv_timeout(Duration::from_secs(6));
        let took = asked.elapsed();
        // Ends a reader that still waits: its input ends.
        close(w);
        let _ = reader.join();
        close(r);
        assert_eq!(read, Ok(Ok(None)), "after {took:?}");
        assert!(took >= Duration::from_millis(1500), "it took {took:?}");
    }

    /// #4100 — what came behind the job's line in its read is handed on
    /// whole, a start and the beginning of the next one alike.
    #[test]
    fn what_came_behind_the_job_is_handed_on() {
        // SAFETY: `getppid(2)` cannot fail.
        let parent = unsafe { libc::getppid() };
        let job = Job {
            display: ":100".into(),
            group: 4242,
            wm: "openbox".into(),
            apps: vec![],
            grace_ms: 1500,
            wm_delay_floor_ms: 250,
            wm_delay_ceil_ms: 4000,
            wm_stable_after_ms: 30_000,
        };
        let start = Start {
            id: 3,
            argv: vec!["xterm".into(), "-T".into(), "a \"title\"\n".into()],
            deadline_ms: 99,
        };
        let (r, w) = pipe();
        write_all(
            w,
            format!("{}{}{{\"id\":4", job.line(), start.line()).as_bytes(),
        );
        close(w);
        let read = read_job_and_rest(r, parent, JOB_WAIT);
        close(r);
        let (got, rest) = read.unwrap().unwrap();
        assert_eq!(got, job);
        let mut lines = Lines::default();
        let fed = lines.feed(&rest);
        assert_eq!(fed.too_long, 0);
        assert_eq!(fed.lines.len(), 1, "{fed:?}");
        assert_eq!(Start::parse(&fed.lines[0]), Ok(start));
        assert_eq!(lines.carry, b"{\"id\":4");
    }

    /// #4100 — the keeper's input after the job, cut into lines: a line split
    /// across reads is read whole; one of exactly one write's bytes is read;
    /// a longer one is dropped whole, counted, and the line after it is read.
    #[test]
    fn the_input_is_cut_into_starts_and_an_overlong_line_is_dropped_whole() {
        let mut lines = Lines::default();
        assert_eq!(lines.feed(b"{\"id\":1,"), Fed::default());
        let fed = lines.feed(b"\"argv\":[\"a\"],\"deadline_ms\":5}\n{\"id\"");
        assert_eq!(
            fed.lines,
            vec![b"{\"id\":1,\"argv\":[\"a\"],\"deadline_ms\":5}".to_vec()]
        );
        assert_eq!(lines.carry, b"{\"id\"");

        // Exactly one write's bytes with its newline: read.
        let mut lines = Lines::default();
        let full = vec![b'x'; START_MAX];
        let mut bytes = full.clone();
        bytes.push(b'\n');
        let fed = lines.feed(&bytes);
        assert_eq!((fed.lines, fed.too_long), (vec![full], 0));

        // One byte more, over two reads: dropped whole up to its newline,
        // and the line after it is read.
        let mut lines = Lines::default();
        let fed = lines.feed(&[b'y'; START_MAX - 10]);
        assert_eq!(fed, Fed::default());
        let mut bytes = vec![b'y'; 11];
        bytes.extend_from_slice(b"still the long line\nnext\n");
        let fed = lines.feed(&bytes);
        assert_eq!(fed.too_long, 1);
        assert_eq!(fed.lines, vec![b"next".to_vec()]);
        assert!(lines.carry.is_empty() && !lines.dropping);

        // A long line's rest that never ends holds nothing: the carry stays
        // empty while it is dropped.
        let mut lines = Lines::default();
        let fed = lines.feed(&[b'z'; START_MAX + 1]);
        assert_eq!(fed.too_long, 1);
        for _ in 0..4 {
            assert_eq!(lines.feed(&[b'z'; 4096]), Fed::default());
        }
        assert!(lines.carry.is_empty() && lines.dropping);

        // Empty lines are lines: the start's reader says what it cannot read.
        let fed = Lines::default().feed(b"\n\n");
        assert_eq!(fed.lines, vec![Vec::<u8>::new(), Vec::new()]);
    }

    /// #4100 — an answer reads back as it was said, for the start it names;
    /// no other line of the keeper's reads as one.
    #[test]
    fn an_answer_reads_back_for_its_start_and_no_other_line_does() {
        for (id, answer) in [
            (7, Answer::Started { pid: 4242 }),
            (
                u64::MAX,
                Answer::Failed {
                    os_error: Some(13),
                    kind: "PermissionDenied".into(),
                    text: "Permission denied (os error 13)".into(),
                },
            ),
            (
                1,
                Answer::Failed {
                    os_error: None,
                    kind: "Other".into(),
                    text: "a: \"b\"\nc".into(),
                },
            ),
            (
                0,
                Answer::Refused {
                    why: "its X server is not running".into(),
                },
            ),
        ] {
            let line = answer.line(id);
            assert!(!line.contains('\n'), "one line: {line:?}");
            assert_eq!(Answer::parse(&line), Some((id, Ok(answer))));
        }
        for not_an_answer in [
            "ready",
            "refused: start 1: no",
            "info: start 1: {}",
            "warn: the window manager (12) exited",
            "start x: {}",
            "start : {}",
            "start -1: {}",
            "start 1 {}",
            "start 99999999999999999999999: {}",
        ] {
            assert_eq!(Answer::parse(not_an_answer), None, "{not_an_answer:?}");
        }
        // An answer line it cannot read is still the answer to its start.
        assert!(matches!(
            Answer::parse("start 5: {\"new\":1}"),
            Some((5, Err(_)))
        ));
    }

    /// #4100 — a start the keeper must not take is refused before anything
    /// is started: in a teardown, onto an X server that is not running, past
    /// its deadline, and with no program. One that cannot start says the
    /// error its start returned.
    #[test]
    fn a_start_the_keeper_must_not_take_is_refused_and_one_that_fails_says_why() {
        let start = |argv: &[&str], deadline_ms: u64| Start {
            id: 1,
            argv: argv.iter().map(|a| a.to_string()).collect(),
            deadline_ms,
        };
        let refused = |answer: Answer| match answer {
            Answer::Refused { why } => why,
            other => panic!("not refused: {other:?}"),
        };
        let never = "/nonexistent/roomler-4100/never-started";
        let mut book = Book::default();
        book.tear_down();
        let why = refused(book.start_x_client(&start(&[never], u64::MAX), ":9", || true));
        assert!(why.contains("taken down"), "{why}");

        let mut book = Book::default();
        let why = refused(book.start_x_client(&start(&[never], u64::MAX), ":9", || false));
        assert!(why.contains("X server is not running"), "{why}");
        let why = refused(book.start_x_client(&start(&[never], 0), ":9", || true));
        assert!(why.contains("stopped waiting"), "{why}");
        let why = refused(book.start_x_client(&start(&[], u64::MAX), ":9", || true));
        assert!(why.contains("no program"), "{why}");

        match book.start_x_client(&start(&[never], u64::MAX), ":9", || true) {
            Answer::Failed { os_error, kind, .. } => {
                assert_eq!(os_error, Some(libc::ENOENT));
                assert_eq!(kind, "NotFound");
            }
            other => panic!("{other:?}"),
        }
        assert!(book.clients.is_empty(), "{:?}", book.clients);
    }

    #[test]
    fn the_keeper_never_signals_0_1_minus_1_or_itself() {
        let (me, daemon) = (4000, 3000);
        for pid in [0, 1, -1, -2, -4000, i32::MIN, me, daemon] {
            assert!(
                !may_signal(pid, me, daemon),
                "pid {pid} must never be signalled"
            );
        }
        for pid in [2, 4001, 2999, i32::MAX] {
            assert!(
                may_signal(pid, me, daemon),
                "pid {pid} is a process like any other"
            );
        }
    }

    #[test]
    fn keeper_log_never_blocks_and_never_panics_on_a_full_or_broken_pipe() {
        // The pipe fills: lines are dropped, and nothing waits.
        let (r, w) = pipe();
        set_nonblocking(w);
        let line = format!("info: {}", "x".repeat(200));
        let started = Instant::now();
        let mut dropped = 0;
        for _ in 0..2_000 {
            if !say_to(w, &line) {
                dropped += 1;
            }
        }
        assert!(dropped > 0, "400 KB of lines never filled a pipe");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the writes waited: {:?}",
            started.elapsed()
        );
        // What was written is whole lines, never a split one.
        let mut buf = vec![0u8; 1 << 20];
        // SAFETY: `read(2)` into a live buffer.
        let n = unsafe { libc::read(r, buf.as_mut_ptr().cast(), buf.len()) };
        assert!(n > 0);
        let text = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
        assert!(text.ends_with('\n'), "a line was split");
        assert!(text.lines().all(|l| l == line), "a line was mangled");

        // A line longer than one atomic write is cut, not split.
        let (r2, w2) = pipe();
        set_nonblocking(w2);
        assert!(say_to(w2, &format!("warn: {}", "é".repeat(5_000))));
        // SAFETY: `read(2)` into a live buffer.
        let n2 = unsafe { libc::read(r2, buf.as_mut_ptr().cast(), buf.len()) };
        assert!(n2 > 0 && n2 as usize <= LINE_MAX, "{n2} bytes");
        assert_eq!(buf[n2 as usize - 1], b'\n');
        assert!(
            std::str::from_utf8(&buf[..n2 as usize]).is_ok(),
            "cut inside a character"
        );
        close(r2);
        close(w2);

        // The reader goes away (the daemon died). Rust's runtime ignores
        // SIGPIPE in this process as it does in the keeper; only then is a
        // write to a broken pipe safe to try here.
        // SAFETY: `sigaction(2)` with no new action reads the disposition.
        let ignored = unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut old) == 0
                && old.sa_sigaction == libc::SIG_IGN
        };
        close(r);
        if ignored {
            // Another test's child holds a copy of the read end for the moment
            // between its fork and its exec, so the pipe breaks once no such
            // copy is left. Each write returns at once, written or not.
            let by = Instant::now() + Duration::from_secs(5);
            let mut broke = false;
            while !broke && Instant::now() < by {
                broke = !say_to(w, "warn: into a broken pipe");
                if !broke {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            assert!(broke, "a write to a pipe that nobody reads did not fail");
        }
        close(w);
    }
}

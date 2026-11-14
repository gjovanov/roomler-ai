// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-43 P1 — the root daemon as supervisor of the GUI-session worker.
//!
//! macOS is the one platform where one process cannot serve both planes: a
//! root LaunchDaemon lives in session 0 with no WindowServer, so it can never
//! capture or inject; a GUI-session process is the console user, so it can
//! never create a `utun`. Windows escapes this because a SYSTEM process can
//! attach to the interactive desktop (`win_service/desktop.rs`); macOS has no
//! such API. Two processes are therefore forced — but two *enrollments* are
//! not, and this module is the first step of collapsing them (FR-43).
//!
//! P0 measured the mechanism on real hardware (2026-08-30, issue #971): from
//! the running root daemon, a capture in session 0 fails with "could not
//! create image from display", while the same call through
//! `launchctl asuser <console-uid>` produced an 8.4 MB screenshot in 233 ms —
//! and our own binary spawned that way reports both a live GUI session and its
//! TCC grants. So `launchctl asuser` is the launchd analogue of the Windows
//! supervisor's `WTSQueryUserToken` + `CreateProcessAsUserW`
//! (`win_service/supervisor.rs::spawn_in_session`), and this is the launchd
//! analogue of its `decide_spawn`.
//!
//! ## What P1 deliberately does NOT do
//!
//! It never fights launchd. If the LaunchAgent job is loaded in `gui/<uid>`,
//! **launchd owns the worker and this supervisor stands down** — because both
//! spawning would put two processes on ONE enrollment, and the hub displaces
//! the older control WS (`remote_control/src/hub.rs`), producing a
//! login/displace/relaunch loop that looks exactly like a flapping device.
//! That stand-down is what makes flipping the switch on a live Mac a no-op,
//! and it is why P1 can ship before the packaging change that stops
//! bootstrapping the LaunchAgent (P3).
//!
//! Kill switch: `macos_supervise_gui_worker`, default **off**. Off, this
//! module does nothing at all and the two halves behave byte-for-byte as they
//! do today.
//!
//! ## A worker never outlives its daemon (#2987)
//!
//! The worker chain runs in a process group of its own, so launchd's teardown
//! of the daemon never reaches it, and only `stop_worker` stops it.
//! A daemon that dies any other way (the watchdog's `exit(STALL_EXIT_CODE)`, a
//! crash, a SIGKILL) used to leave its worker running. That worker kept the
//! person's single-instance lock, so every later worker exited 0 at once: the
//! GUI half stayed on the old release through two updates, and its delegation
//! was refused (field-measured on the operator's MacBook, 2026-10-26). Two
//! mechanisms close it:
//!
//! | mechanism | catches |
//! |---|---|
//! | **the lifeline**: the daemon keeps its end of the worker's stdin open for the worker's life ([`LIFELINE_LINE`]), and the worker stops when it closes | a worker of this release, whatever ended its daemon |
//! | **the reap**: a daemon with no worker of its own stops every process running exactly `<exe> run --supervised` as a person ([`stale_workers`]), at start and after a worker exits 0 at once | workers from older releases, and any lifeline that leaked |

use std::time::Duration;

/// How often the supervisor re-reads the world (console user, worker health).
/// launchd exposes no session-change callback we can subscribe to cheaply, so
/// this is a poll; 5 s is far below any human-noticeable login latency and
/// costs one `stat` plus, at most, one `launchctl print`.
pub const POLL: Duration = Duration::from_secs(5);

/// A worker that exits sooner than this is treated as failing, so the backoff
/// grows. Longer than this and the next failure starts from the floor again.
/// Same shape (and rationale) as the tunnel flow supervisor's run threshold:
/// a process that came up and served is not a crash loop.
pub const HEALTHY_RUN: Duration = Duration::from_secs(30);

/// Backoff floor for respawns.
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
/// How many consecutive fast exits before the supervisor stops trying. The
/// 2026-08-30 outage looped because nothing bounded this: a worker that can
/// NEVER start is a configuration fault, and hammering it hides the cause.
pub const MAX_FAST_EXITS: u32 = 5;

/// Backoff ceiling for respawns.
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// How many polls the supervisor keeps re-trying to start launchd's own worker
/// after it has released its own. Bounded because a Mac where launchd simply
/// refuses should not spawn a `launchctl` every [`POLL`] for the daemon's whole
/// life; 12 polls is a minute, which is far longer than a hand-back takes.
pub const KICKSTART_RETRIES: u32 = 12;

/// What the supervisor knows when it decides. Deliberately plain data: the
/// decision is pure so it can be tested on every platform, while the syscalls
/// that produce these fields are macOS-only. (macOS has no `cargo test` lane
/// in CI, so logic that only compiles there is logic nothing verifies.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inputs {
    /// The `macos_supervise_gui_worker` kill switch.
    pub enabled: bool,
    /// Only a root daemon can spawn into another user's session.
    pub is_root: bool,
    /// The console user's uid, or `None` at the login window / no session.
    pub console_uid: Option<u32>,
    /// Whether `gui/<uid>/com.roomler.agent` is loaded — i.e. launchd already
    /// owns a worker for this session.
    pub launch_agent_loaded: bool,
    /// Consecutive workers that exited faster than [`HEALTHY_RUN`]. A worker
    /// that can never start (no per-user enrollment on this Mac, say) would
    /// otherwise respawn forever; past [`MAX_FAST_EXITS`] the supervisor
    /// stops and says why.
    pub consecutive_fast_exits: u32,
    /// The uid our own live worker was spawned for, if we have one.
    pub worker_uid: Option<u32>,
}

/// The supervisor's next move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Switch off, or not root: do nothing, own nothing.
    Disabled,
    /// Nobody is logged in. Any worker we hold is meaningless — drop it.
    NoGuiSession,
    /// launchd owns this session's agent and we hold nothing: stand down.
    LaunchdOwns,
    /// launchd owns this session's agent **and we still hold a worker**:
    /// stop ours, then make sure theirs is actually running.
    ///
    /// A separate state from [`Action::LaunchdOwns`] because the hand-back is
    /// not just "drop ours" — see [`imp::kickstart_launch_agent`] for the race
    /// that makes the second half load-bearing.
    HandBack(u32),
    /// Spawn a worker for this uid.
    Spawn(u32),
    /// The console user changed under a worker we own: replace it.
    Replace(u32),
    /// Too many workers died on arrival — stop spawning and say why. Cleared
    /// by anything that changes the situation: a session change, the
    /// LaunchAgent returning, or the switch going off.
    GaveUp,
    /// Our worker matches the current session and is alive.
    Healthy,
}

/// The whole policy, in one testable place.
pub fn decide(i: Inputs) -> Action {
    if !i.enabled || !i.is_root {
        return Action::Disabled;
    }
    let Some(uid) = i.console_uid else {
        return Action::NoGuiSession;
    };
    // launchd first: never race the LaunchAgent for one enrollment. Checked
    // even when we already hold a worker, because the plist can be
    // bootstrapped back at any time (a re-install does exactly that) and the
    // loser of that race must be us, not the enrolled agent.
    if i.launch_agent_loaded {
        return match i.worker_uid {
            Some(_) => Action::HandBack(uid),
            None => Action::LaunchdOwns,
        };
    }
    match i.worker_uid {
        Some(w) if w == uid => Action::Healthy,
        Some(_) => Action::Replace(uid),
        None if i.consecutive_fast_exits >= MAX_FAST_EXITS => Action::GaveUp,
        None => Action::Spawn(uid),
    }
}

/// Grow the respawn backoff. `ran` is how long the worker that just exited
/// stayed up; a healthy run resets the ladder.
pub fn next_backoff(current: Duration, ran: Duration) -> Duration {
    if ran >= HEALTHY_RUN {
        return BACKOFF_MIN;
    }
    let doubled = current.saturating_mul(2);
    if doubled > BACKOFF_MAX {
        BACKOFF_MAX
    } else {
        doubled
    }
}

/// How long to sleep before the next tick: [`POLL`], or what is left of a
/// respawn's backoff when that ends sooner. `retry_in` is `None` with no respawn
/// pending and zero once its backoff is over.
///
/// A backoff only came due on a tick, so a worker's 1 s floor waited out a whole
/// [`POLL`]: FR-43 AC11 measured 10.09 s from `kill -9` to the channel attached
/// on 0.4.191, where a tick that lands on the backoff makes it about 6 s.
///
/// ⚠️ A zero is a plain tick, never a zero sleep. The arms that do not spawn
/// leave `wait_until` set, so the loop would spin on one that has passed.
pub fn next_tick_in(retry_in: Option<Duration>) -> Duration {
    match retry_in {
        Some(left) if !left.is_zero() => left.min(POLL),
        _ => POLL,
    }
}

/// FR-97 P6e-2 — how a worker ended, read off its wait status.
///
/// The child the supervisor reaps is `sudo`, not the agent: `launchctl asuser`
/// execs into it. `sudo` passes its command's end through as its own, a signal
/// as the same signal and an exit code as the same code. Measured on macOS 15.7
/// with sudo 1.9.13 (FR-97 §8): `kill -9`, `-SEGV`, `-BUS`, `-ABRT`, `-TERM`
/// and `-HUP` on the worker came back as signals 9, 11, 10, 6, 15 and 1, and
/// `exit 7` as code 7. So this is the worker's own end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerEnd {
    /// It exited with this code.
    Code(i32),
    /// This signal ended it, in Darwin's numbering.
    Signal(i32),
}

/// The worker's end from the status `try_wait` returned. `None` only for a
/// status that carries neither, which a terminated child never has.
#[cfg(unix)]
pub fn worker_end(status: std::process::ExitStatus) -> Option<WorkerEnd> {
    use std::os::unix::process::ExitStatusExt;
    status
        .signal()
        .map(WorkerEnd::Signal)
        .or_else(|| status.code().map(WorkerEnd::Code))
}

// Darwin's signal numbers, which a wait status on a Mac carries. Spelt out
// rather than taken from `libc` because the pure half of this module also
// builds and tests on Linux and Windows, where some are numbered differently
// (SIGBUS is 7 on Linux) or are missing. The macOS build checks every one
// against `libc` (`imp`).
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGQUIT: i32 = 3;
const SIGILL: i32 = 4;
const SIGTRAP: i32 = 5;
const SIGABRT: i32 = 6;
const SIGFPE: i32 = 8;
const SIGKILL: i32 = 9;
const SIGBUS: i32 = 10;
const SIGSEGV: i32 = 11;
const SIGSYS: i32 = 12;
const SIGTERM: i32 = 15;

const SIGNAL_NAMES: &[(i32, &str)] = &[
    (SIGHUP, "SIGHUP"),
    (SIGINT, "SIGINT"),
    (SIGQUIT, "SIGQUIT"),
    (SIGILL, "SIGILL"),
    (SIGTRAP, "SIGTRAP"),
    (SIGABRT, "SIGABRT"),
    (SIGFPE, "SIGFPE"),
    (SIGKILL, "SIGKILL"),
    (SIGBUS, "SIGBUS"),
    (SIGSEGV, "SIGSEGV"),
    (SIGSYS, "SIGSYS"),
    (SIGTERM, "SIGTERM"),
];

/// FR-97 P6e-2 — the crash record's summary for a worker's end, or `None`
/// when the end is not a crash.
///
/// A crash is an end the worker's own hooks could not record: a signal that
/// killed it before its panic hook ran (SIGKILL, SIGSEGV, SIGBUS…), or an exit
/// the agent did not choose. Not a crash:
/// - exit code 0;
/// - the codes the agent exits with on purpose (`ALREADY_RUNNING`,
///   `AGENT_DELETED`, `RESTART_REQUESTED`), and `STALL`, which the watchdog
///   recorded itself before it forced the exit. The Windows supervisor skips
///   the same list (`should_record_supervisor_crash`), plus a session-teardown
///   code that macOS has no equivalent of;
/// - SIGTERM, SIGHUP and SIGINT: a logout, a closed terminal or a person
///   ending the process on purpose.
///
/// ⚠️ `RESTART_REQUESTED_EXIT_CODE` is 9, and so is SIGKILL. Exit code 9 is a
/// restart someone asked for; signal 9 is a kill. Only the [`WorkerEnd`]
/// variant tells them apart, which is why this takes no flat number.
pub fn crash_summary(end: WorkerEnd, uid: u32, ran: Duration) -> Option<String> {
    use crate::watchdog::{
        AGENT_DELETED_EXIT_CODE, ALREADY_RUNNING_EXIT_CODE, RESTART_REQUESTED_EXIT_CODE,
        STALL_EXIT_CODE,
    };
    let how = match end {
        WorkerEnd::Code(
            0
            | STALL_EXIT_CODE
            | AGENT_DELETED_EXIT_CODE
            | ALREADY_RUNNING_EXIT_CODE
            | RESTART_REQUESTED_EXIT_CODE,
        ) => return None,
        WorkerEnd::Code(code) => format!("exited with code {code}"),
        WorkerEnd::Signal(SIGHUP | SIGINT | SIGTERM) => return None,
        WorkerEnd::Signal(sig) => match SIGNAL_NAMES.iter().find(|(n, _)| *n == sig) {
            Some((_, name)) => format!("was killed by {name} ({sig})"),
            None => format!("was killed by signal {sig}"),
        },
    };
    Some(format!(
        "macOS supervisor: the GUI worker (uid {uid}) {how} after {} s",
        ran.as_secs()
    ))
}

/// FR-97 / FR-119 — the session-side knobs whose ROOT value must bind the
/// supervised worker, by suffix (`ROOMLERD_<suffix>`). The root daemon says the
/// hello from its own environment and config; the worker serves the session,
/// and `sudo`'s `env_reset` hands it neither (measured, see `spawn_worker`).
/// So the root's effective value of each — its environment, else its
/// `config.toml` fallback — reaches the worker as a real variable, which beats
/// the worker's own config: what the hello promised is what the session does.
///
/// ⚠️ Session-side knobs ONLY. The worker is a full agent, so a knob it must
/// not act on (the overlay, a relay server, the updater) never belongs here: it
/// would start a second one of them in the person's session.
pub const WORKER_ENV_FORWARD: &[&str] = &[
    // FR-97 P2d-1 — the lock hint, in a session and at a prompt.
    "LOCK_HINT",
    // FR-97 P2e-1 — copied images, gated on the pasteboard's change count.
    "CLIPBOARD_MAC_WATCH",
    // FR-97 P2d-2a — the keyboard layout through Text Input Sources.
    "MAC_LAYOUT",
    // FR-97 P2d-2b — display match on the main display, and the owner's policy
    // for it, which the worker holds as a floor under its own.
    "MAC_DISPLAY_MATCH",
    "DISPLAY_MATCH",
    // FR-97 P2c — Remote Apps: the root's `[virtual_desktop_apps] enabled =
    // false`, published as `0`, which the worker that serves `rc:apps.*` holds
    // as a floor under its own config.
    "APPS",
    // FR-97 P2a — system audio through ScreenCaptureKit: the worker is the
    // half that captures a session's (and a recording's) sound.
    "SCK_AUDIO",
    // FR-97 P2f-2 — the worker starts its recorders as their own responsible
    // process, so macOS can ask the person about the microphone; `0` reverts
    // to the plain spawn, where a supervised recorder refuses the microphone.
    "RECORDER_DISCLAIM",
    // FR-119 — the high-refresh picker and its per-cell ceilings.
    "FPS_HIGH",
    "FPS_DENY",
    // The encoder-cells kill switch: a gate applies at every entry point, and
    // the worker is where a session's encoders open.
    "ENCODER_CELLS_DENY",
    // FR-118 P5 — GPU-resident frames and their kill switch, for the same
    // reason: the worker captures, runs the probe whose `gpu` child proves
    // VideoToolbox's IOSurface form, and opens the session's encoders.
    "ZERO_COPY",
    "ZERO_COPY_DENY",
    // FR-130 — the colour description's switch: the worker's encoders write
    // it, and its probe says `color-described` in the hello it computes.
    "COLOR_DESCRIPTION",
    // FR-138 P4-0 — the remote-control sessions' switch, one per device: the
    // worker's own session table and its hello follow the root's value, never
    // the person's config, so the root's off is the worker's off.
    "RC_SESSIONS_SURVIVE_REATTACH",
];

/// The worker's `ROOMLERD_<suffix>=<value>` assignments: every
/// [`WORKER_ENV_FORWARD`] suffix `lookup` has a value for, in that order.
/// `lookup` is `tunnel_core::env::node_env` in the daemon (environment, then
/// config). No shell sees them, so a value like `*:144` stays literal.
pub fn worker_env_args(lookup: impl Fn(&str) -> Option<String>) -> Vec<String> {
    WORKER_ENV_FORWARD
        .iter()
        .filter_map(|suffix| lookup(suffix).map(|v| format!("ROOMLERD_{suffix}={v}")))
        .collect()
}

/// #2987 — the line the supervisor writes on a worker's stdin AFTER the
/// secret. It means "I hold my end of this pipe for as long as you live", so
/// from then on the pipe's end-of-file means the daemon is gone, however it
/// ended: the watchdog's `exit(STALL_EXIT_CODE)`, a crash, a SIGKILL. None of
/// those reach `stop_worker`, and a worker that outlived its daemon kept the
/// person's single-instance lock, so no later daemon's worker could start
/// (field-measured on the operator's MacBook, 2026-10-26).
///
/// ⚠️ It is a line of its own, never part of the secret's. A worker from
/// before #2987 uses ALL of its first line as the secret, and the supervisor
/// cannot tell which release is on disk when it spawns, because an update
/// replaces the binary under a running daemon. An old worker never reads this
/// line, and a new worker under an old supervisor reads end-of-file where it
/// would be, so it arms nothing. The `/1` makes a future change a new line,
/// which today's worker does not know and so does not arm on.
pub const LIFELINE_LINE: &str = "roomler-lifeline/1";

/// What the supervisor writes on a worker's stdin, in ONE write, so the
/// secret never arrives without the offer behind it. About 80 bytes, well
/// under the pipe's atomic-write size.
pub fn stdin_handoff(secret: &str) -> String {
    format!("{secret}\n{LIFELINE_LINE}\n")
}

/// #2987 — how long a worker whose daemon is gone gives its own graceful stop
/// before it forces the exit. That stop takes a few seconds at most (the
/// runtime alone has a 3 s budget). Nothing else will ever stop the process,
/// because its supervisor is what is gone.
pub const LIFELINE_BACKSTOP: Duration = Duration::from_secs(15);

/// #2987 — how long the reap waits after its SIGTERM before a SIGKILL. Longer
/// than [`LIFELINE_BACKSTOP`] on purpose. When the next daemon reaps a worker
/// of this release, that worker is already in its own clean stop, and it does
/// not hear the SIGTERM, because the select that listened for it has ended.
/// Killing it before that stop writes the clean-run mark would count a crash
/// on the person's own config, and three crashes in ten minutes start a
/// rollback there.
pub const REAP_GRACE: Duration = Duration::from_secs(LIFELINE_BACKSTOP.as_secs() + 5);
const _: () = assert!(REAP_GRACE.as_secs() > LIFELINE_BACKSTOP.as_secs());

/// How a worker's watch over its stdin ended (#2987).
#[derive(Debug)]
pub enum LinkEnd {
    /// No secret: end-of-file first, a blank line, or a read error.
    NoSecret,
    /// The secret, then end-of-file: a supervisor from before #2987, which
    /// closes its end after the secret. There is nothing to watch.
    NotOffered,
    /// The secret, then a line this worker does not know: a newer
    /// supervisor's offer. Left unwatched, as before #2987, rather than misread.
    Unknown,
    /// Watching, then the pipe failed. Stop watching and keep running.
    Lost(std::io::Error),
    /// Watching, then end-of-file: the daemon that held the other end is gone.
    Cut,
}

/// One handoff line is about 80 bytes. Anything that writes megabytes without
/// a newline must not grow the worker.
const HANDOFF_LINE_MAX: u64 = 4096;

fn handoff_line<R: std::io::BufRead>(r: &mut R) -> std::io::Result<Option<String>> {
    use std::io::{BufRead, Read};
    let mut line = String::new();
    let n = r.by_ref().take(HANDOFF_LINE_MAX).read_line(&mut line)?;
    Ok((n > 0).then(|| line.trim().to_string()))
}

/// The worker's side of the stdin handoff (#2987). It reads the secret and
/// passes it to `on_secret` straight away, so the attach never waits on what
/// follows. Then it reads the lifeline offer and, if there is one, blocks
/// until the pipe closes. It takes any reader so a test can drive it with a
/// real pipe; the worker passes its locked stdin, on a thread of its own.
pub fn stdin_link<R: std::io::BufRead>(
    mut r: R,
    on_secret: impl FnOnce(Option<String>),
) -> LinkEnd {
    let secret = match handoff_line(&mut r) {
        Ok(Some(s)) if !s.is_empty() => Some(s),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "delegation: could not read the secret from stdin");
            None
        }
    };
    let got_secret = secret.is_some();
    on_secret(secret);
    if !got_secret {
        return LinkEnd::NoSecret;
    }
    match handoff_line(&mut r) {
        Ok(None) => return LinkEnd::NotOffered,
        Ok(Some(line)) if line == LIFELINE_LINE => {}
        Ok(Some(_)) => return LinkEnd::Unknown,
        Err(e) => return LinkEnd::Lost(e),
    }
    let mut buf = [0u8; 64];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return LinkEnd::Cut,
            // Nothing more is sent today, and a newer supervisor may send
            // more. Only the end of the pipe means anything here.
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return LinkEnd::Lost(e),
        }
    }
}

/// FR-43 P3a — what `spawn_worker` starts a worker with, after the executable.
/// With `one_row` the worker holds no enrollment (`macos_one_row`).
pub fn worker_args(one_row: bool) -> &'static [&'static str] {
    if one_row {
        &["run", "--supervised", "--unenrolled"]
    } else {
        &["run", "--supervised"]
    }
}

/// #2987 — a supervised worker's command line, as `ps` prints it: the argv
/// `spawn_worker` builds (`/usr/bin/env` execs into the agent, so it is not
/// part of it).
pub fn worker_command(exe: &str, one_row: bool) -> String {
    format!("{exe} {}", worker_args(one_row).join(" "))
}

/// FR-43 P3a — every command line a supervised worker can run under. A daemon
/// looks for both whatever its own switch says: the worker left behind may be
/// from before the switch last moved.
pub fn worker_commands(exe: &str) -> [String; 2] {
    [worker_command(exe, false), worker_command(exe, true)]
}

/// #2987 — the workers a previous daemon left behind, out of
/// `ps -axww -o pid= -o uid= -o command=`: every process that runs exactly one
/// of `commands` as a person (uid ≠ 0), other than `own_pid`. Returns
/// `(pid, uid)` pairs.
///
/// Exact match only, because each match gets a SIGTERM from root.
/// `--supervised` appears on no command line except the ones `spawn_worker`
/// builds, so a hand-run `roomlerd run`, launchd's own worker (`… run`) and
/// the daemon (`… --config … run`) never match.
pub fn stale_workers(ps: &str, commands: &[String], own_pid: u32) -> Vec<(u32, u32)> {
    ps.lines()
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(char::is_whitespace)?;
            let (uid, cmd) = rest.trim_start().split_once(char::is_whitespace)?;
            let pid: u32 = pid.parse().ok()?;
            let uid: u32 = uid.parse().ok()?;
            let cmd = cmd.trim();
            (commands.iter().any(|c| c == cmd) && uid != 0 && pid != own_pid).then_some((pid, uid))
        })
        .collect()
}

/// #2987 — why the supervisor stopped spawning, read off how the last worker
/// ended. On a Mac, a worker that finds another roomlerd holding this
/// person's single-instance lock exits **0** (`already_running_exit`: 0 is
/// launchd's "don't relaunch me"). A missing per-user enrollment ends in an
/// error, which is code 1. Before #2987 the give-up named only the second, so
/// the first was misreported as an enrollment problem.
///
/// FR-43 P3a — a worker with no enrollment needs no per-user config, so under
/// `one_row` that second reading would send a person after the wrong thing.
///
/// #3802 — `not_theirs` is the person's folder when another account owns it
/// ([`FolderNotTheirs`]). The worker writes its log and its config there, so
/// that is the cause whatever the exit, and the reading names the folder and
/// what gives it back (`move_aside`), never the log the worker could not
/// write.
pub fn gave_up_reason(
    last: Option<WorkerEnd>,
    one_row: bool,
    not_theirs: Option<&FolderNotTheirs>,
) -> String {
    if let Some(f) = not_theirs {
        return format!(
            "the worker cannot write the person's folder {}: uid {} owns it (mode {:o}), \
             not the person, so the worker's own log is not there either. {}",
            f.path.display(),
            f.owner,
            f.mode,
            move_aside(f)
        );
    }
    match last {
        Some(WorkerEnd::Code(0)) => {
            "each worker exited 0 at once: another roomlerd holds this person's \
             single-instance lock, and it is not a worker this supervisor can identify \
             (a hand-run `roomlerd run`, say). Find it with: pgrep -lU <uid> roomlerd. \
             Once it is gone, restart this daemon: \
             sudo launchctl kickstart -k system/com.roomler.daemon"
        }
        _ if one_row => {
            "this worker holds no enrollment, so the cause is not a per-user config: read how \
             it ended in its own log, under the console user's \
             Library/Application Support/live.roomler.roomler/logs"
        }
        _ => {
            "almost always this Mac has no per-user enrollment: the worker runs AS the \
             console user and loads THAT user's config. Check with: \
             sudo launchctl asuser <uid> sudo -u '#<uid>' roomlerd caps"
        }
    }
    .to_string()
}

/// #3802 — what gives the person's folder back. macOS renames a folder only
/// for an account that may write in it, so no command the person runs with
/// their own rights moves a folder of root's, in any parent (measured on
/// macOS 15.7: `mv` answers `Permission denied` and moves nothing). Only an
/// administrator can, with `sudo`, and the line says so in words.
///
/// It prints that administrator's command only under the package's rule
/// (`sudo_line_fits`, `packaging/macos/postinstall:252-266`): the person at
/// the console is an administrator of this Mac, so they run it for
/// themselves, and the path names the folder it reads as
/// ([`FolderNotTheirs::administrator`]); the folder is a real folder of
/// root's; and the account name is plain ([`plain_account_name`]). Anywhere
/// else it gives no command, says that an administrator has to move the
/// folder aside and names the section of the docs ([`INSTALL_DOC`]). Every
/// path is quoted ([`sh_quote`]), and a folder whose path holds a `"`, `$`,
/// backtick, `\`, `!` or control character gets no command
/// ([`path_survives_double_quotes`]), so every command printed also survives
/// `sh -c "…"`; nor does one whose path holds a Unicode format character
/// that changes how a line displays ([`path_displays_as_it_runs`]), so every
/// command printed reads as it runs.
///
/// With nothing the revert reads in it, the command moves it aside and the
/// worker makes a new one, as the person, at its next start. With copies
/// `one-row --revert` reads there and nowhere else ([`FolderNotTheirs::kept`]),
/// the command stages first and moves last: the person's account
/// (`sudo -u '#<uid>'`, the form `spawn_worker` runs the worker with) makes a
/// new folder beside it and copies them into it, and only then is the folder
/// moved aside and the new one put in its place. So a copy the person cannot
/// reach stops the command with everything where it was, where the revert
/// still finds it, and if the last move fails, the one before it is undone
/// and the command still ends in failure (`false`), so a script reads no
/// success where nothing was fixed. It
/// does the same in an administrator's own shell and as root. The names it
/// makes carry [`FolderNotTheirs::stamp`], so no earlier print made them. Each
/// command starts with one `sudo test`, before anything runs: that the folder
/// is still a real directory of root's (not a link, a directory, and `-O`,
/// which under `sudo` means owned by root), so a line printed earlier and run
/// after the folder was given back, by any means, does nothing; and that the
/// name it moves the folder to is not there yet, so a second run of the same
/// line does nothing, where BSD `mv` would move the live folder INTO the one
/// the first run made.
///
/// A copy `lstat` says the person cannot read ([`person_cannot_read`]) gets
/// no command: moving the folder would leave it where the revert does not
/// look. Nor does a folder that is not a directory, nor one that holds an
/// entry named `config.toml` ([`FolderNotTheirs::holds_config`]): a move
/// would set it aside with the folder, so the line names it instead, and
/// still names the copies the revert reads there.
fn move_aside(f: &FolderNotTheirs) -> String {
    if !f.is_dir {
        return "It is not a folder but a link or a file, so this line gives no command for it."
            .to_string();
    }
    let cannot: Vec<String> = f
        .kept
        .iter()
        .filter_map(|k| {
            let (owner, mode) = k.stat.filter(|_| person_cannot_read(f.uid, k.stat))?;
            Some(format!(
                "{} (uid {owner}, mode {:o})",
                k.name.display(),
                mode & 0o7777
            ))
        })
        .collect();
    if !cannot.is_empty() {
        let one = cannot.len() == 1;
        return format!(
            "It holds {}, which the person cannot read, and `sudo roomlerd one-row --revert` \
             reads {} there and nowhere else: if this folder is moved aside, a later revert \
             would not restore {}, so this line gives no command to move it.",
            cannot.join(" and "),
            if one { "it" } else { "them" },
            if one { "that file" } else { "those files" },
        );
    }
    if f.owner != 0 {
        // Whether macOS lets the person rename a folder of another account
        // rests on its mode, so the line claims nothing about that: it is
        // that account's folder, and no command moves it.
        return format!(
            "It is that account's folder, so this line gives no command for it; see \
             {INSTALL_DOC}."
        );
    }
    let only = "Only an administrator of this Mac can move it aside, because macOS lets no other \
                account rename a folder that root owns.";
    let names: Vec<String> = f
        .kept
        .iter()
        .map(|k| k.name.display().to_string())
        .collect();
    let one = names.len() == 1;
    let holds = if names.is_empty() {
        String::new()
    } else {
        format!(
            " It holds {}, which `sudo roomlerd one-row --revert` reads there and nowhere else",
            names.join(" and ")
        )
    };
    // Rule 2 of the package's `sudo_line_fits`: the reads say yes, the folder
    // is root's own and real, and the name is plain.
    let refused = administrator_command(f);
    if refused == Err(NoCommand::HoldsConfig) {
        // An entry of that name, whatever `lstat` read (a link or a folder
        // too): only a file of that name is an enrollment.
        let (dot, also) = if holds.is_empty() {
            ("", "")
        } else {
            (".", " also")
        };
        return format!(
            "{only}{holds}{dot} It{also} holds an entry named config.toml, and a move would set \
             that entry aside with the folder (with `macos_one_row` off, a file of that name is \
             this person's enrollment), so this line gives no command to move it; see \
             {INSTALL_DOC}."
        );
    }
    let Ok(name) = refused else {
        let copy_back = match names.len() {
            0 => "",
            1 => " and copy that file back into a new folder that the person's account makes",
            _ => " and copy those files back into a new folder that the person's account makes",
        };
        return format!(
            "{only}{holds}{} This line gives no command for it: an administrator of this Mac \
             has to move it aside{copy_back}; see {INSTALL_DOC}.",
            if holds.is_empty() { "" } else { "." },
        );
    };
    let path = f.path.display().to_string();
    let folder = sh_quote(&path);
    let aside_path = format!("{path}.made-by-root-{}", f.stamp);
    let aside = sh_quote(&aside_path);
    // One test before anything runs. The folder is still a real directory of
    // root's (under `sudo`, `-O` is owned by root), so a line printed before
    // the folder was given back changes nothing when it is run after; and the
    // name it moves the folder to is not there yet, so a second run of the
    // same line does nothing either.
    let can = format!(
        " {name} is an administrator of this Mac and can run this: sudo test ! -h {folder} -a -d \
         {folder} -a -O {folder} -a ! -e {aside} && "
    );
    let inert = "once the folder is no longer a real folder of root's, as when it is the person's \
                 again, it changes nothing";
    let restart = " Then log out and back in, or restart this daemon: \
                   sudo launchctl kickstart -k system/com.roomler.daemon";
    if names.is_empty() {
        return format!(
            "{only}{can}sudo mv {folder} {aside} ({inert}; if the move fails, nothing moved), and \
             the worker makes the folder again as the person at its next start.{restart}"
        );
    }
    // Staged first, moved last: the copies are made, as the person, before
    // anything moves, so a `cp` that fails leaves every file where the revert
    // reads it.
    let as_person = format!("sudo -u '#{}'", f.uid);
    let new_path = format!("{path}.new-{}", f.stamp);
    let new = sh_quote(&new_path);
    let copies: String = names
        .iter()
        .map(|n| format!("{} ", sh_quote(&format!("{path}/{n}"))))
        .collect();
    let into = sh_quote(&format!("{new_path}/"));
    format!(
        "{only}{holds}, so the command copies {} into a new folder that the person's account \
         makes.{can}{as_person} mkdir {new} && {as_person} cp {copies}{into} && sudo mv \
         {folder} {aside} && {{ sudo mv {new} {folder} || {{ sudo mv {aside} {folder}; false; \
         }}; }} ({inert}; nothing moves until the copies are made, and if the last move fails, \
         the one before it is undone and the command fails).{restart}",
        if one { "it" } else { "them" },
    )
}

/// #3802 — the section of the docs a give-up line names where it prints no
/// command, in the words of the package's own (`INSTALL_DOC`,
/// `packaging/macos/postinstall:259`).
const INSTALL_DOC: &str =
    "docs/installation.md, section \"Which account each step of the package's script writes as\"";

/// #3802 — how long the reads behind an administrator's command may take
/// at a give-up, all of them together: past it, the line gives none.
pub const ADMIN_READ_DEADLINE: Duration = Duration::from_secs(5);

/// #3802 — why a give-up line gives no administrator's command: what the
/// folder's own read says, the account read that said no, or the deadline.
/// The supervisor logs its [`NoCommand::why`] once, beside the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoCommand {
    /// `lstat` read a link or a file at the folder.
    NotADirectory,
    /// The folder holds a kept copy the person cannot read.
    UnreadableCopy,
    /// Another account than root owns the folder: its uid.
    NotRoots(u32),
    /// The folder holds a `config.toml` ([`FolderNotTheirs::holds_config`]).
    HoldsConfig,
    /// The folder's path would not survive `sh -c "…"`
    /// ([`path_survives_double_quotes`]), or a command printed with it would
    /// not display as it runs ([`path_displays_as_it_runs`]).
    PathNotPlain,
    /// The account reads have not run.
    NotRead,
    /// The account database has no entry for the person's uid, or its name
    /// is not UTF-8.
    NoAccount,
    /// The account name is not plain ([`plain_account_name`]).
    NameNotPlain,
    /// The paths of the home do not read as the folder
    /// ([`path_reads_as_the_folder`]).
    PathsDoNotRead,
    /// The account read's task ended without an answer.
    LookupFailed,
    /// `dseditgroup` could not be started: the error.
    AdminCheckDidNotRun(String),
    /// `dseditgroup` did not confirm that the person is a member of
    /// `admin`: its exit code, `None` when a signal ended it.
    NotAdministrator(Option<i32>),
    /// The reads did not answer within [`ADMIN_READ_DEADLINE`].
    TimedOut,
}

impl NoCommand {
    /// The words the supervisor logs for it: which read refused the
    /// command, or that the deadline ran out.
    pub fn why(&self) -> String {
        match self {
            NoCommand::NotADirectory => {
                "the folder's own lstat reads a link or a file, not a directory".to_string()
            }
            NoCommand::UnreadableCopy => {
                "the folder holds a kept copy that the person cannot read".to_string()
            }
            NoCommand::NotRoots(uid) => format!("uid {uid} owns the folder, not root"),
            NoCommand::HoldsConfig => "the folder holds config.toml".to_string(),
            NoCommand::PathNotPlain => "the folder's path holds a double quote, `$`, a backtick, \
                                        a backslash, `!`, a control character or a Unicode \
                                        format character that changes how a line displays"
                .to_string(),
            NoCommand::NotRead => "the account reads have not run".to_string(),
            NoCommand::NoAccount => "the account lookup (getpwuid_r) found no entry with a UTF-8 \
                                     name for the person's uid"
                .to_string(),
            NoCommand::NameNotPlain => {
                "the person's account name is not plain ([A-Za-z0-9._-]+)".to_string()
            }
            NoCommand::PathsDoNotRead => "the person's home is not a folder, or its Library or \
                                          Library/Application Support is not a folder of their \
                                          own (a link, another account's, or unread), or the \
                                          folder is not one name inside it"
                .to_string(),
            NoCommand::LookupFailed => {
                "the account lookup's task ended without an answer".to_string()
            }
            NoCommand::AdminCheckDidNotRun(e) => format!("dseditgroup could not run: {e}"),
            NoCommand::NotAdministrator(code) => format!(
                "dseditgroup does not confirm that the person is a member of admin ({})",
                match code {
                    Some(code) => format!("exit {code}"),
                    None => "ended by a signal".to_string(),
                }
            ),
            NoCommand::TimedOut => format!(
                "the reads did not answer within {} s",
                ADMIN_READ_DEADLINE.as_secs()
            ),
        }
    }
}

/// #3802 — what the folder's own read says against an administrator's
/// command, before any read of the account: `None` when it leaves the
/// command to the account reads.
pub fn folder_refuses(f: &FolderNotTheirs) -> Option<NoCommand> {
    if !f.is_dir {
        return Some(NoCommand::NotADirectory);
    }
    if f.kept.iter().any(|k| person_cannot_read(f.uid, k.stat)) {
        return Some(NoCommand::UnreadableCopy);
    }
    if f.owner != 0 {
        return Some(NoCommand::NotRoots(f.owner));
    }
    if f.holds_config {
        return Some(NoCommand::HoldsConfig);
    }
    if !f
        .path
        .to_str()
        .is_some_and(|p| path_survives_double_quotes(p) && path_displays_as_it_runs(p))
    {
        return Some(NoCommand::PathNotPlain);
    }
    None
}

/// #3802 — whether `path` holds none of the Unicode format characters that
/// change how a line displays: the zero-width characters and the marks of
/// direction (U+200B to U+200F), the bidi embeddings, pop and overrides
/// (U+202A to U+202E), the word joiner, the invisible operators and the bidi
/// isolates (U+2060 to U+2069), and the byte order mark (U+FEFF). None of
/// them is a control character, and a command printed with one reads
/// otherwise than it runs.
fn path_displays_as_it_runs(path: &str) -> bool {
    !path.chars().any(|c| {
        matches!(
            c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
        )
    })
}

/// #3802 — whether `path`, quoted by [`sh_quote`], also survives
/// `sh -c "…"`: it holds no `"`, `$`, backtick, `\`, `!` or control
/// character, which a shell reads in its own way inside double quotes (or
/// expands from its history). An apostrophe is fine: [`sh_quote`] writes it
/// `'\''`, which double quotes leave alone.
fn path_survives_double_quotes(path: &str) -> bool {
    !path
        .chars()
        .any(|c| matches!(c, '"' | '$' | '`' | '\\' | '!') || c.is_control())
}

/// #3802 — the account name a give-up line names with its administrator's
/// command for `f`, or why it gives none: the folder's own read first
/// ([`folder_refuses`]), then the account reads
/// ([`FolderNotTheirs::administrator`]). The line's text and the
/// supervisor's log both read it, so they never disagree.
pub fn administrator_command(f: &FolderNotTheirs) -> Result<&str, NoCommand> {
    if let Some(no) = folder_refuses(f) {
        return Err(no);
    }
    match &f.administrator {
        Ok(name) if plain_account_name(name) => Ok(name),
        Ok(_) => Err(NoCommand::NameNotPlain),
        Err(no) => Err(no.clone()),
    }
}

/// #3802 — an account name a give-up line may print in its sentence and
/// hand to `dseditgroup`: one or more of `[A-Za-z0-9._-]`.
fn plain_account_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// #3802 — the paths half of the package's `sudo_line_fits`
/// (`packaging/macos/postinstall:262-265`), over what `lstat` read, so it
/// tests on every platform: whether the path of `folder`, the folder of the
/// person `uid` whose home the account database names `home`, names the
/// folder it reads as. The home is an absolute path to a folder, not a link
/// (not ending in `/`, the package's `/*[!/]`); `<home>/Library` and
/// `<home>/Library/Application Support` are each a folder of the person's
/// own, not a link; and the folder is one name inside the last of them. A
/// path that is not UTF-8 (it could not be printed as it reads) or a read
/// that fails is a no.
pub fn path_reads_as_the_folder(
    uid: u32,
    home: &std::path::Path,
    folder: &std::path::Path,
    lstat: impl Fn(&std::path::Path) -> std::io::Result<(u32, u32)>,
) -> bool {
    let (Some(home), Some(folder)) = (home.to_str(), folder.to_str()) else {
        return false;
    };
    if !home.starts_with('/') || home.ends_with('/') {
        return false;
    }
    let library = format!("{home}/Library");
    let support = format!("{library}/Application Support");
    let one_name_in_support = folder
        .strip_prefix(support.as_str())
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| !name.is_empty() && !name.contains('/'));
    let is_dir = |mode: u32| mode & 0o170000 == 0o040000;
    one_name_in_support
        && lstat(std::path::Path::new(home)).is_ok_and(|(_, mode)| is_dir(mode))
        && [library, support].iter().all(|p| {
            lstat(std::path::Path::new(p)).is_ok_and(|(owner, mode)| owner == uid && is_dir(mode))
        })
}

/// #3802 — `s` as one word of a POSIX shell: in single quotes, with each `'`
/// in it written `'\''`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// #3802 — whether what `lstat` read of a kept copy (owner, full mode) says
/// the person `uid` cannot read it: a regular file of theirs without the
/// owner's read bit, or another account's with neither the group's nor the
/// others' read bit (the copy the conversion kept of a config of root's is
/// root's, mode 600). Anything else, a copy the group or the others may read,
/// one that is no regular file or one whose read failed, is left to the `cp`,
/// which reads it as the person before anything moves: the command stages
/// first and moves last, so a `cp` that fails (a folder the person cannot
/// enter, say) leaves everything where it was.
fn person_cannot_read(uid: u32, stat: Option<(u32, u32)>) -> bool {
    stat.is_some_and(|(owner, mode)| {
        mode & 0o170000 == 0o100000
            && if owner == uid {
                mode & 0o400 == 0
            } else {
                mode & 0o044 == 0
            }
    })
}

/// #3802 — the console person's folder (the one their worker writes its log
/// and config in) when another account owns it: what `lstat` read, never
/// through a link at the folder itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderNotTheirs {
    pub path: std::path::PathBuf,
    /// The person, whose worker cannot write it.
    pub uid: u32,
    /// The account that owns it.
    pub owner: u32,
    /// Its permission bits.
    pub mode: u32,
    /// Whether `lstat` read a directory there (not a link, not a file).
    pub is_dir: bool,
    /// Which of [`kept_for_revert`] it holds, with what `lstat` read of each.
    pub kept: Vec<KeptCopy>,
    /// Whether `lstat` found a `config.toml` in it, or could not tell (a read
    /// that failed for any reason but "not there"). A move would set that
    /// file aside with the folder, and with `macos_one_row` off it is this
    /// person's enrollment. The product's own states never put one in a
    /// folder of root's.
    pub holds_config: bool,
    /// The UTC second the folder was read: unique for each print, so the
    /// names the command makes beside the folder were never made before.
    pub stamp: String,
    /// The console person's account name when the reads of the package's
    /// `sudo_line_fits` all said yes for this folder: that person is an
    /// administrator of this Mac, and its path names the folder it reads as.
    /// Otherwise the read that said no, or that they did not answer in time:
    /// the line then gives no command.
    pub administrator: Result<String, NoCommand>,
}

/// #3802 — one of [`kept_for_revert`] that the person's folder holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptCopy {
    /// Its name inside the folder.
    pub name: std::path::PathBuf,
    /// Its owner and full mode as `lstat` read them; `None` when that read
    /// failed for any reason but "not there".
    pub stat: Option<(u32, u32)>,
}

/// #3802 — the files `sudo roomlerd one-row --revert` reads back from the
/// person's folder, and from nowhere else (`one_row::revert_plan`): the
/// LaunchAgent plist the conversion moved there, and the copy of the person's
/// config it kept beside their config. Names, inside that folder.
pub fn kept_for_revert() -> [std::path::PathBuf; 2] {
    use crate::one_row::{AGENT_PLIST, kept_path};
    [
        kept_path(std::path::Path::new(AGENT_PLIST)),
        kept_path(std::path::Path::new("config.toml")),
    ]
}

/// #3802 — [`FolderNotTheirs`] out of what `lstat` read for `path`, the folder
/// of the person `uid` (its owner and full mode), the copies of
/// [`kept_for_revert`] found in it and the `stamp` of this read: `None` when
/// that person owns it. Its `administrator` is `None`: the supervisor reads
/// that apart, off its loop's thread.
pub fn folder_not_theirs(
    path: std::path::PathBuf,
    uid: u32,
    owner: u32,
    mode: u32,
    kept: Vec<KeptCopy>,
    stamp: &str,
) -> Option<FolderNotTheirs> {
    (owner != uid).then(|| FolderNotTheirs {
        path,
        uid,
        owner,
        mode: mode & 0o7777,
        is_dir: mode & 0o170000 == 0o040000,
        kept,
        holds_config: false,
        stamp: stamp.to_string(),
        administrator: Err(NoCommand::NotRead),
    })
}

/// #3802 — the read that fills [`FolderNotTheirs`] for the folder `path` of
/// the person `uid`, with `lstat` as the filesystem (owner and full mode of a
/// path, never through a link at its last name), so it tests on every
/// platform: `None` when the folder cannot be read or that person owns it.
/// The names of [`kept_for_revert`] are read in it only when the folder's own
/// read is a directory, so no read goes through a link at the folder: one
/// `lstat` of each name, no walk.
pub fn read_person_folder(
    path: std::path::PathBuf,
    uid: u32,
    stamp: &str,
    lstat: impl Fn(&std::path::Path) -> std::io::Result<(u32, u32)>,
) -> Option<FolderNotTheirs> {
    let (owner, mode) = lstat(&path).ok()?;
    let mut folder = folder_not_theirs(path, uid, owner, mode, Vec::new(), stamp)?;
    if folder.is_dir {
        // A name whose read fails for any reason but "not there" is held,
        // unread: the command's `cp` reads it before anything moves, and a
        // `cp` that fails stops the command with everything where it was.
        folder.kept = kept_for_revert()
            .into_iter()
            .filter_map(|name| match lstat(&folder.path.join(&name)) {
                Ok(stat) => Some(KeptCopy {
                    name,
                    stat: Some(stat),
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => Some(KeptCopy { name, stat: None }),
            })
            .collect();
        // A read of it that fails for any reason but "not there" counts as
        // there: no command moves a config it could not rule out.
        folder.holds_config = match lstat(&folder.path.join("config.toml")) {
            Ok(_) => true,
            Err(e) => e.kind() != std::io::ErrorKind::NotFound,
        };
    }
    Some(folder)
}

#[cfg(target_os = "macos")]
pub use imp::run;

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;

    // FR-97 P6e-2 — the Darwin numbers `crash_summary` reads, checked here, on
    // the one OS whose wait statuses carry them.
    const _: () = assert!(
        SIGHUP == libc::SIGHUP
            && SIGINT == libc::SIGINT
            && SIGQUIT == libc::SIGQUIT
            && SIGILL == libc::SIGILL
            && SIGTRAP == libc::SIGTRAP
            && SIGABRT == libc::SIGABRT
            && SIGFPE == libc::SIGFPE
            && SIGKILL == libc::SIGKILL
            && SIGBUS == libc::SIGBUS
            && SIGSEGV == libc::SIGSEGV
            && SIGSYS == libc::SIGSYS
            && SIGTERM == libc::SIGTERM
    );

    /// The LaunchAgent label the postinstall bootstraps. If this is loaded,
    /// we stand down — see the module docs.
    const AGENT_LABEL: &str = "com.roomler.agent";

    /// #2987 — our end of the current worker's stdin: its lifeline
    /// ([`LIFELINE_LINE`]). It lives here, for as long as the daemon PROCESS
    /// does, and not in the supervisor task's `Child`. A panic in that task
    /// would drop the `Child`, close the pipe and stop the person's GUI half,
    /// with nothing left to spawn another. [`stop_worker`] releases it, so
    /// does the reap of a worker that ended, and so does the next spawn.
    static LIFELINE: std::sync::Mutex<Option<std::process::ChildStdin>> =
        std::sync::Mutex::new(None);

    fn hold_lifeline(stdin: Option<std::process::ChildStdin>) {
        *LIFELINE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = stdin;
    }

    /// The console user's uid, or `None` when nobody is logged in.
    ///
    /// `/dev/console`'s owner is the canonical answer and the same one the
    /// pkg postinstall uses (`stat -f %Su /dev/console`). uid 0 means the
    /// login window: root "owns" the console with no Aqua session behind it,
    /// which is emphatically not a session we can spawn into.
    fn console_uid() -> Option<u32> {
        let uid = std::fs::metadata("/dev/console").ok()?.uid();
        (uid != 0).then_some(uid)
    }

    /// Is `gui/<uid>/com.roomler.agent` loaded? Exit status only — the output
    /// is launchd's own formatting and we depend on none of it.
    fn launch_agent_loaded(uid: u32) -> bool {
        Command::new("launchctl")
            .arg("print")
            .arg(format!("gui/{uid}/{AGENT_LABEL}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// #3802 — the console person's folder when another account owns it,
    /// read by [`read_person_folder`] with `lstat`, stamped with this UTC
    /// second. A read only: root changes nothing in a person's home.
    fn person_folder_not_theirs(uid: u32) -> Option<FolderNotTheirs> {
        let home = crate::appdirs::account_home(uid)?;
        let path = crate::appdirs::macos_config_dir_in_home(&home, std::path::Path::exists);
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        read_person_folder(path, uid, &stamp, |p| {
            std::fs::symlink_metadata(p).map(|m| (m.uid(), m.mode()))
        })
    }

    /// #3802 — the account name and the home the account database names for
    /// `uid` (`getpwuid_r`, as `appdirs::account_home` reads the home).
    /// `None` when there is no entry or its name is not UTF-8.
    // `appdirs::account_entry` (#3838) replaces this lookup once it is on master.
    fn name_and_home(uid: u32) -> Option<(String, std::path::PathBuf)> {
        use std::os::unix::ffi::OsStrExt;
        let mut buf: Vec<libc::c_char> = vec![0; 1024];
        loop {
            // SAFETY: an all-zero `passwd` is a valid value of a plain C
            // struct; `getpwuid_r` fills it in.
            let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
            let mut found: *mut libc::passwd = std::ptr::null_mut();
            // SAFETY: every pointer names memory this frame owns for the
            // whole call, and `buf.len()` is the length of `buf`, where the
            // entry's strings are written.
            let rc = unsafe {
                libc::getpwuid_r(uid, &mut entry, buf.as_mut_ptr(), buf.len(), &mut found)
            };
            if rc == libc::ERANGE && buf.len() < 1 << 20 {
                buf.resize(buf.len() * 2, 0);
                continue;
            }
            if rc != 0 || found.is_null() || entry.pw_name.is_null() || entry.pw_dir.is_null() {
                return None;
            }
            // SAFETY: `pw_name` and `pw_dir` point into `buf`, NUL-terminated
            // by `getpwuid_r`, and `buf` outlives these borrows.
            let (name, dir) = unsafe {
                (
                    std::ffi::CStr::from_ptr(entry.pw_name),
                    std::ffi::CStr::from_ptr(entry.pw_dir),
                )
            };
            let home = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()));
            return Some((name.to_str().ok()?.to_owned(), home));
        }
    }

    /// #3802 — the console person's account name when the give-up line may
    /// print an administrator's command for `f`: the package's
    /// `sudo_line_fits` (`packaging/macos/postinstall:260-266`). The folder
    /// is a real folder of root's, the account database names a plain name
    /// and a home whose paths read as the folder
    /// ([`path_reads_as_the_folder`]), and `dseditgroup` says that account is
    /// a member of `admin`. None of it blocks the supervisor's loop: the
    /// account database and the `lstat`s run on the blocking pool,
    /// `dseditgroup` is a tokio child with no pipe that is killed when
    /// dropped, and the whole read stops at [`ADMIN_READ_DEADLINE`]. Anything
    /// that fails or runs late is the [`NoCommand`] that names it: no
    /// command. A read only, once per give-up, and none at all for a folder
    /// whose own read already refuses the command ([`folder_refuses`]).
    async fn administrator_reads(f: &FolderNotTheirs) -> Result<String, NoCommand> {
        administrator_reads_within(f, ADMIN_READ_DEADLINE).await
    }

    /// [`administrator_reads`] with its deadline given, so a test can run
    /// it out.
    async fn administrator_reads_within(
        f: &FolderNotTheirs,
        deadline: Duration,
    ) -> Result<String, NoCommand> {
        if let Some(no) = folder_refuses(f) {
            return Err(no);
        }
        let (uid, folder) = (f.uid, f.path.clone());
        let read = async move {
            let name = match tokio::task::spawn_blocking(move || account_reads(uid, &folder)).await
            {
                Ok(Ok(name)) => name,
                Ok(Err(no)) => return Err(no),
                Err(_) => return Err(NoCommand::LookupFailed),
            };
            is_administrator(&name).await.map(|()| name)
        };
        tokio::time::timeout(deadline, read)
            .await
            .unwrap_or(Err(NoCommand::TimedOut))
    }

    /// #3802 — the account half of [`administrator_reads`], on the blocking
    /// pool: the account database names a plain name for `uid` and a home
    /// whose paths read as `folder` ([`path_reads_as_the_folder`]).
    fn account_reads(uid: u32, folder: &std::path::Path) -> Result<String, NoCommand> {
        let (name, home) = name_and_home(uid).ok_or(NoCommand::NoAccount)?;
        if !plain_account_name(&name) {
            return Err(NoCommand::NameNotPlain);
        }
        let lstat = |p: &std::path::Path| std::fs::symlink_metadata(p).map(|m| (m.uid(), m.mode()));
        if !path_reads_as_the_folder(uid, &home, folder, lstat) {
            return Err(NoCommand::PathsDoNotRead);
        }
        Ok(name)
    }

    /// #3802 — whether `dseditgroup` says the account `name` is a member of
    /// `admin` (the package's question): a child with no pipe, killed when
    /// dropped. Bounded by its caller.
    async fn is_administrator(name: &str) -> Result<(), NoCommand> {
        let status = tokio::process::Command::new("/usr/sbin/dseditgroup")
            .args(["-o", "checkmember", "-m", name, "admin"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|e| NoCommand::AdminCheckDidNotRun(e.to_string()))?;
        if status.success() {
            Ok(())
        } else {
            Err(NoCommand::NotAdministrator(status.code()))
        }
    }

    /// Spawn the agent into `uid`'s GUI session, **as that user**.
    ///
    /// ⚠️ `launchctl asuser <uid> <cmd>` joins the user's Mach bootstrap
    /// namespace but does **NOT** change credentials — `launchctl asuser 501
    /// id -u` prints `0`. Shipping without the `sudo -u` half cost a real
    /// outage on 2026-08-30: every worker started as ROOT, resolved root's
    /// profile config, died instantly with
    /// `no config found at /var/root/Library/Application Support/...`, and the
    /// supervisor respawned it on the backoff ladder for as long as the
    /// LaunchAgent stayed unloaded. The session half of the P0 spike was
    /// right; the identity half was never measured, because the probe used
    /// `caps` — which reports the GUI session and the TCC grants (both of
    /// which root-in-a-session genuinely has) and says nothing about *which*
    /// config the process would load.
    ///
    /// `sudo -u "#<uid>"` takes a numeric id, so this needs no `getpwuid`
    /// lookup and cannot pick the wrong account when a uid has several names.
    /// Verified on the MacBook: `asuser + sudo -u "#501"` yields uid 501,
    /// `has_input_permission: true`, and a config that resolves.
    ///
    /// No `--config`: the child now really is that user, so `appdirs`
    /// resolves the user's own config — the same enrollment the LaunchAgent
    /// would have used. P1 changes who launches the worker, nothing about its
    /// identity.
    ///
    /// FR-43 P3a — with `one_row` the worker holds no enrollment. The person's
    /// config still resolves the same way: its session knobs are theirs.
    fn spawn_worker(
        uid: u32,
        exe: &std::path::Path,
        secret: &str,
        one_row: bool,
    ) -> std::io::Result<Child> {
        let mut cmd = Command::new("launchctl");
        cmd.arg("asuser")
            .arg(uid.to_string())
            .arg("sudo")
            .arg("-u")
            .arg(format!("#{uid}"));
        // FR-97 / FR-119 — the same `env_reset` (below) is why the worker never
        // saw the root's kill switches or its config-only session knobs: the
        // hello the root sent promised one thing and the session did another.
        // The root's effective values go on as real variables
        // ([`WORKER_ENV_FORWARD`]); `env` execs into the agent, so the chain
        // the supervisor reaps, and the signals it relays, are unchanged. Not
        // secrets: settings, safe in a world-readable argv.
        let forward = worker_env_args(tunnel_core::env::node_env);
        if !forward.is_empty() {
            let keys: Vec<&str> = forward
                .iter()
                .map(|a| a.split_once('=').map_or(a.as_str(), |(k, _)| k))
                .collect();
            tracing::info!(
                ?keys,
                "macOS supervisor: the worker inherits the root's session knobs"
            );
            cmd.arg("/usr/bin/env").args(&forward);
        }
        // #3080 — the chain lives as long as the worker, so it must not keep a
        // pipe it inherited from a spawn on another thread (macOS has no
        // `pipe2`): that pipe's reader would never see EOF. It keeps its
        // stdio, the lifeline on stdin, and nothing else.
        tunnel_core::child::inherit_only_stdio(&mut cmd);
        let mut child = cmd
            .arg(exe)
            // FR-43 P2a — `--supervised` tells the worker it is ours, and that
            // its delegation-channel secret is waiting on stdin.
            //
            // ⚠️ This REPLACES the `ROOMLER_MACOS_SUPERVISED` environment
            // marker P1 shipped, which never arrived at a single worker:
            // `sudo` runs under the stock `Defaults env_reset` and discards the
            // parent's environment. Measured on the MacBook 2026-08-31 —
            // `ps -E` on a live supervised worker showed ZERO `ROOMLER_MACOS*`
            // variables. Nothing had noticed because nothing read the marker
            // yet, which is precisely what made it worth checking before
            // building on it.
            .args(worker_args(one_row))
            // The secret goes on the pipe, never in argv: `ps` is world
            // readable and the pipe is only ours.
            .stdin(Stdio::piped())
            // Own process group, so `stop_worker` can take the whole
            // launchctl -> sudo -> agent chain down instead of orphaning it.
            // That is also why the chain outlives a daemon that dies without
            // `stop_worker`: launchd's teardown of our job never reaches it
            // (#2987), hence the lifeline below.
            .process_group(0)
            .spawn()?;
        // #2987 — write the secret AND the lifeline offer, then KEEP our end
        // open. The pipe then closes only when this daemon is gone or stops
        // the worker, and either way the worker stops itself. Before #2987 the
        // handle was dropped right after the secret, so a daemon that died
        // without `stop_worker` left a worker running that kept the person's
        // single-instance lock. A worker that reads a partial line does not
        // wait forever: it gives up on the secret after 5 s
        // (`delegate_worker`), and the offer never arrives without it.
        let lifeline = child.stdin.take().and_then(|mut stdin| {
            use std::io::Write;
            match stdin.write_all(stdin_handoff(secret).as_bytes()) {
                Ok(()) => Some(stdin),
                Err(e) => {
                    // Not fatal: the worker will time out waiting, log loudly,
                    // and simply not attach. It is still a fully enrolled agent
                    // that serves its own sessions, so the remote-desktop half
                    // keeps working — degrading to P1 behaviour is the right
                    // failure.
                    tracing::warn!(error = %e, "macOS supervisor: could not hand the worker its delegation secret");
                    None
                }
            }
        });
        hold_lifeline(lifeline);
        Ok(child)
    }

    /// Complete a hand-back: make sure launchd's own worker is running.
    ///
    /// Dropping our worker is only HALF of handing ownership back, because the
    /// hand-back is a race and launchd's side loses it **silently**:
    ///
    /// 1. the LaunchAgent is bootstrapped, so launchd starts its worker at
    ///    once;
    /// 2. that worker finds OUR worker holding the single-instance lock and
    ///    exits — with status **0**, logging `single-instance lock held by
    ///    another process; exiting`;
    /// 3. the plist's `KeepAlive{SuccessfulExit=false}` therefore treats that
    ///    as a job that ran and finished, and never retries it;
    /// 4. up to [`POLL`] later we notice `launch_agent_loaded` and stop our
    ///    worker — by which time launchd has already given up.
    ///
    /// Net effect: the Mac ends with **no user half at all**, and nothing
    /// scheduled to bring one back. Field-measured on 0.4.35, 2026-08-31,
    /// on the exact sequence #1029 was cut to fix — its process-group kill
    /// worked (no orphan survived), which is precisely what uncovered this:
    /// the two are the same root cause seen from opposite ends, namely that
    /// the loser of the lock race exits cleanly and is never retried.
    ///
    /// `launchctl kickstart` (deliberately **without** `-k`) starts the job if
    /// it is not running and is a no-op if it is, so this is idempotent and
    /// cannot disturb a healthy launchd-owned worker. It runs AFTER
    /// [`stop_worker`], which reaps the group, so the lock is free by then.
    ///
    /// Returns whether launchd accepted it. A failure has to be **retried by
    /// the caller**, because by then we have already stopped our own worker:
    /// the next poll sees [`Action::LaunchdOwns`] (we no longer hold one), so
    /// nothing would bring the hand-back back around on its own, and the Mac
    /// would sit with no user half — the exact outcome this function exists to
    /// prevent. [`KICKSTART_RETRIES`] bounds that, so a Mac where launchd
    /// genuinely refuses does not spawn a subprocess every [`POLL`] forever.
    fn kickstart_launch_agent(uid: u32) -> bool {
        let job = format!("gui/{uid}/{AGENT_LABEL}");
        match Command::new("launchctl")
            .arg("kickstart")
            .arg(&job)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(s) if s.success() => {
                tracing::info!(
                    uid,
                    job = %job,
                    "macOS supervisor: handed the worker back to launchd"
                );
                true
            }
            Ok(s) => {
                tracing::warn!(
                    uid,
                    job = %job,
                    status = ?s.code(),
                    "macOS supervisor: could not kickstart the LaunchAgent — \
                     this session may be left with no user half until launchd \
                     starts one"
                );
                false
            }
            Err(e) => {
                tracing::warn!(
                    uid,
                    job = %job,
                    error = %e,
                    "macOS supervisor: could not run launchctl kickstart"
                );
                false
            }
        }
    }

    /// Stop a worker we own — the whole process GROUP, not just our child.
    ///
    /// `launchctl asuser … sudo -u '#uid' … run` gives us a three-deep chain
    /// (launchctl → sudo → agent), so killing the direct child leaves the
    /// agent alive and re-parented to launchd. Field-measured on 2026-08-30
    /// while restoring the Mac after the P1 test: the orphan kept running,
    /// held the agent's **single-instance lock**, and launchd's own
    /// LaunchAgent worker therefore exited with
    /// `single-instance lock held by another process; exiting` — cleanly, so
    /// `KeepAlive{SuccessfulExit=false}` never retried it. Net effect: turning
    /// the supervisor OFF left the Mac running an UNSUPERVISED orphan that
    /// nothing would restart. Handing ownership back has to be clean, or the
    /// kill switch is not one.
    ///
    /// [`spawn_worker`] therefore puts the chain in its own process group
    /// (`process_group(0)`), and this kills the group: SIGTERM, a short grace
    /// period, then SIGKILL for whatever is left.
    fn stop_worker(
        uid: u32,
        mut child: Child,
        why: &str,
        delegate: &crate::delegate::DelegateHost,
    ) {
        // FR-43 P2a — revoke FIRST, before the process is even signalled: a
        // worker being torn down must not be able to (re)attach on its way out,
        // and doing it here rather than at the four call sites is what makes
        // "someone forgot one" unrepresentable.
        delegate.revoke();
        let pgid = child.id() as i32;
        tracing::info!(
            uid,
            why,
            pgid,
            "macOS supervisor: stopping our GUI worker group"
        );
        // SAFETY: `kill` with a negative pid signals the process group; the
        // group is one we created in `spawn_worker`, so it contains only our
        // own descendants. A failure here is not actionable (the group may
        // already be gone), hence the ignored return.
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
        for _ in 0..20 {
            match child.try_wait() {
                Ok(Some(_)) => break,
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        // SAFETY: same contract as above.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
        let _ = child.wait();
        // #2987 — then our end of its stdin. Only now: closed before the
        // SIGTERM, it would race the signal into a second, differently
        // classified stop. Closing it promptly beats keeping a dead pipe open
        // until the next spawn.
        hold_lifeline(None);
    }

    /// #2987 — stop every supervised worker a previous daemon left behind.
    ///
    /// A daemon that dies without its clean shutdown (the watchdog's
    /// `exit(STALL_EXIT_CODE)`, a crash, a SIGKILL) never reaches
    /// [`stop_worker`]. A worker from before #2987 does not notice. It keeps
    /// the person's single-instance lock, so every worker the next daemon
    /// spawns exits 0 at once. Field-measured on the operator's MacBook,
    /// 2026-10-26: a 0.4.174 worker outlived two updates and its new daemon's
    /// give-up. A worker of this release stops itself when its stdin closes
    /// ([`LIFELINE_LINE`]). This catches what that cannot: older workers, and
    /// a lifeline a concurrent spawn inherited before `std` set `FD_CLOEXEC`
    /// on it.
    ///
    /// Called only while this daemon holds no worker of its own: at start,
    /// and right after reaping ours. Every match is then someone else's.
    /// Returns how many it stopped.
    async fn reap_stale_workers(exe: &std::path::Path) -> usize {
        let Some(exe) = exe.to_str() else {
            return 0;
        };
        // #3080 — `ps` blocks, so it runs on the blocking pool, never on a
        // runtime worker.
        let commands = worker_commands(exe);
        let stale = tokio::task::spawn_blocking(move || find_processes(&commands))
            .await
            .unwrap_or_default();
        for &(pid, uid) in &stale {
            tracing::warn!(
                pid,
                uid,
                "macOS supervisor: stopping a GUI worker a previous daemon left behind — it \
                 holds the person's single-instance lock, so no worker of ours could start \
                 (#2987)"
            );
        }
        let pids: Vec<u32> = stale.iter().map(|(pid, _)| *pid).collect();
        stop_processes(&pids).await;
        pids.len()
    }

    /// #2987 — the reap, plus what it owes launchd. While a stale worker held
    /// the lock, launchd's own worker may have exited 0, and
    /// `KeepAlive{SuccessfulExit=false}` never retries that. With the
    /// LaunchAgent loaded, the next poll is `LaunchdOwns`, which kickstarts
    /// only a debt. So a reap that stopped anything owes one.
    async fn reap_and_owe(exe: &std::path::Path, kickstart_debt: &mut u32) {
        if reap_stale_workers(exe).await > 0 && console_uid().is_some_and(launch_agent_loaded) {
            *kickstart_debt = KICKSTART_RETRIES;
        }
    }

    /// #3080 — how long `ps` may take. It answers in well under a second; past
    /// this the reap is skipped, exactly as when `ps` fails.
    const PS_DEADLINE: Duration = Duration::from_secs(10);

    /// `ps` once, read through [`stale_workers`]. If `ps` fails, nothing is
    /// found, which leaves things as they were before #2987.
    fn find_processes(commands: &[String]) -> Vec<(u32, u32)> {
        // One `-o` per column: BSD `ps` lets only the LAST keyword of an `-o`
        // carry `=`, and `pid=,uid=` would make `,uid=` pid's header.
        let mut ps = Command::new("/bin/ps");
        ps.args(["-axww", "-o", "pid=", "-o", "uid=", "-o", "command="]);
        find_processes_via(&mut ps, commands)
    }

    /// The read half of [`find_processes`], with the `ps` to run given, so a
    /// test can hand it one whose pipe another process holds.
    ///
    /// #3080 — read until `ps` EXITS, not until EOF, within [`PS_DEADLINE`].
    /// macOS has no `pipe2`, so a long-lived child spawned on another thread
    /// while `ps`'s pipe was new keeps a copy of its write end. Field, 0.4.184:
    /// `route -n monitor` did, `Command::output()` waited on a runtime worker
    /// for an EOF that never came, and the supervisor never reached its loop.
    /// The Mac had no GUI worker, so no capture or input, until a restart.
    fn find_processes_via(ps: &mut Command, commands: &[String]) -> Vec<(u32, u32)> {
        match tunnel_core::child::output_bounded(ps, PS_DEADLINE) {
            Ok(out) if out.status.success() => stale_workers(
                &String::from_utf8_lossy(&out.stdout),
                commands,
                std::process::id(),
            ),
            Ok(out) => {
                tracing::warn!(
                    status = ?out.status.code(),
                    "macOS supervisor: `ps` failed; not looking for a stale GUI worker"
                );
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "macOS supervisor: could not run `ps`, or it did not exit in time; not \
                     looking for a stale GUI worker"
                );
                Vec::new()
            }
        }
    }

    /// SIGTERM, then up to [`REAP_GRACE`], then SIGKILL for whatever is left.
    /// These processes are not our children, so we cannot wait for them:
    /// `kill(pid, 0)` tells us whether they are alive.
    async fn stop_processes(pids: &[u32]) {
        // SAFETY: signal 0 delivers nothing; it only asks whether the pid
        // exists.
        let alive = |pid: u32| unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        for &pid in pids {
            // SAFETY: one signal to one pid `stale_workers` matched exactly.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        for _ in 0..REAP_GRACE.as_millis() / 100 {
            if !pids.iter().any(|&p| alive(p)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for &pid in pids.iter().filter(|&&p| alive(p)) {
            tracing::warn!(
                pid,
                grace_secs = REAP_GRACE.as_secs(),
                "macOS supervisor: a stale GUI worker outlived SIGTERM; killing it"
            );
            // SAFETY: as above.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        for _ in 0..10 {
            if !pids.iter().any(|&p| alive(p)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Supervise for the process lifetime. Never returns an error to the
    /// caller: a supervisor that can take the daemon down with it would trade
    /// a missing remote-desktop half for a missing mesh — a worse failure
    /// than the one it exists to fix.
    pub async fn run(
        enabled: bool,
        // FR-43 P3a — `macos_one_row`: its workers hold no enrollment.
        one_row: bool,
        delegate: crate::delegate::DelegateHost,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let is_root = unsafe { libc::geteuid() } == 0;
        if !enabled || !is_root {
            tracing::debug!(
                enabled,
                is_root,
                "macOS GUI-worker supervisor idle (switch off, or not root)"
            );
            if one_row && is_root {
                tracing::warn!(
                    "macos_one_row is on and macos_supervise_gui_worker is off, so nothing runs \
                     a worker for this row: it has no screen until both are on (FR-43 P3a)"
                );
            }
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "macOS supervisor: cannot resolve own exe — not supervising"
                );
                return;
            }
        };
        tracing::info!(
            exe = %exe.display(),
            one_row,
            "macOS GUI-worker supervisor ON (FR-43 P1) — stands down whenever the LaunchAgent is loaded"
        );
        let mut worker: Option<(u32, Child, Instant)> = None;
        let mut backoff = BACKOFF_MIN;
        let mut fast_exits: u32 = 0;
        // How the last worker ended. The give-up's diagnosis reads it (#2987).
        let mut last_end: Option<WorkerEnd> = None;
        let mut wait_until: Option<Instant> = None;
        let mut last_action: Option<Action> = None;
        // Polls left to keep re-trying launchd's own worker after a hand-back
        // whose kickstart failed. 0 = nothing owed. (`//`, not `///`: a doc
        // comment on a `let` is an `unused_doc_comments` warning, and this
        // module compiles only in the macOS lane, where warnings are denied.)
        let mut kickstart_debt: u32 = 0;
        // #2987 — a worker a previous daemon left behind holds the person's
        // single-instance lock, and every worker we spawn would exit 0 at once.
        // We hold none yet, so every supervised worker running now is someone
        // else's.
        reap_and_owe(&exe, &mut kickstart_debt).await;

        loop {
            // Reap first, so `worker_uid` reflects reality this tick.
            //
            // The borrow of `worker` has to END before the assignment below —
            // hence the map/filter dance rather than mutating inside an
            // `if let ... = worker.as_mut()` body. (Windows never compiles
            // this module, so only the macOS lane would have caught it.)
            let exited = worker
                .as_mut()
                .and_then(|(uid, child, started)| match child.try_wait() {
                    Ok(Some(status)) => Some((*uid, started.elapsed(), status)),
                    _ => None,
                });
            if let Some((uid, ran, status)) = exited {
                backoff = next_backoff(backoff, ran);
                fast_exits = if ran >= HEALTHY_RUN {
                    0
                } else {
                    fast_exits + 1
                };
                tracing::warn!(
                    uid,
                    ran_secs = ran.as_secs(),
                    retry_in_secs = backoff.as_secs(),
                    %status,
                    "macOS supervisor: GUI worker exited"
                );
                // FR-97 P6e-2 — a worker killed by a signal never reaches its
                // own panic hook, so nothing else records it. The record goes
                // to this daemon's own crashes dir, which its own uploader
                // drains (`WriterContext::Supervisor`), so it reaches the
                // server on the daemon's enrollment. Ends this supervisor
                // causes never get here: `stop_worker` reaps its own child.
                let end = worker_end(status);
                if let Some(summary) = end.and_then(|e| crash_summary(e, uid, ran)) {
                    crate::crash_recorder::record(
                        crate::crash_recorder::Reason::SupervisorDetected,
                        &summary,
                        crate::crash_recorder::WriterContext::Supervisor,
                    );
                }
                worker = None;
                hold_lifeline(None);
                // #2987 — a worker that exits 0 at once found another roomlerd
                // holding the person's single-instance lock. If that one is a
                // worker a previous daemon left behind, stop it, so the next
                // spawn finds the lock free. The exit still counts towards
                // giving up: resetting the count after a reap would let two
                // spawners of `run --supervised` (two root daemons on
                // different configs) stop each other's workers forever.
                if ran < HEALTHY_RUN && end == Some(WorkerEnd::Code(0)) {
                    reap_and_owe(&exe, &mut kickstart_debt).await;
                }
                last_end = end;
                wait_until = Some(Instant::now() + backoff);
            }

            let uid_now = console_uid();
            let action = decide(Inputs {
                enabled: true,
                is_root: true,
                console_uid: uid_now,
                // Only ask launchd when there is a session to ask about.
                launch_agent_loaded: uid_now.is_some_and(launch_agent_loaded),
                worker_uid: worker.as_ref().map(|(u, _, _)| *u),
                consecutive_fast_exits: fast_exits,
            });

            // Log transitions, not every tick: this loop runs every 5 s for
            // the daemon's whole life.
            if last_action != Some(action) {
                if matches!(action, Action::GaveUp) {
                    let mut not_theirs = uid_now.and_then(person_folder_not_theirs);
                    // #3802 — bounded and off this thread (`administrator_reads`).
                    if let Some(f) = not_theirs.as_mut() {
                        f.administrator = administrator_reads(f).await;
                    }
                    tracing::error!(
                        console_uid = ?uid_now,
                        fast_exits,
                        max = MAX_FAST_EXITS,
                        last_end = ?last_end,
                        "macOS supervisor: the GUI worker died on arrival too many times — \
                         NOT retrying: {}",
                        gave_up_reason(last_end, one_row, not_theirs.as_ref())
                    );
                    // #3802 — why that line gives no administrator's command:
                    // the read that refused it, or the deadline. Once.
                    if let Some(Err(no)) = not_theirs.as_ref().map(administrator_command) {
                        tracing::info!(
                            "macOS supervisor: the give-up line gives no administrator's \
                             command: {}",
                            no.why()
                        );
                    }
                } else {
                    tracing::info!(?action, console_uid = ?uid_now, "macOS supervisor state");
                }
                last_action = Some(action);
            }

            match action {
                Action::Healthy | Action::Disabled => {}
                Action::GaveUp => {
                    // Logged once via the transition logger above; nothing to
                    // do until the situation changes.
                }
                Action::NoGuiSession => {
                    fast_exits = 0;
                    // Nobody is logged in, so there is no LaunchAgent to owe a
                    // kickstart to; forget any debt from the last session.
                    kickstart_debt = 0;
                    if let Some((uid, child, _)) = worker.take() {
                        stop_worker(uid, child, "GUI session ended", &delegate);
                    }
                }
                Action::LaunchdOwns => {
                    fast_exits = 0;
                    // We hold no worker here, so `HandBack` will never come
                    // around again — this is the ONLY place a failed hand-back
                    // can be retried from. See `kickstart_launch_agent`.
                    if kickstart_debt > 0 {
                        match uid_now {
                            // Cleared by success, not by reaching zero: those
                            // are opposite outcomes and must not share a
                            // branch.
                            Some(uid) if kickstart_launch_agent(uid) => kickstart_debt = 0,
                            Some(uid) => {
                                kickstart_debt -= 1;
                                if kickstart_debt == 0 {
                                    tracing::error!(
                                        uid,
                                        "macOS supervisor: gave up starting launchd's own \
                                         worker — this session has no user half until \
                                         launchd or an operator starts one"
                                    );
                                }
                            }
                            // Unreachable while the action is LaunchdOwns
                            // (`decide` returns NoGuiSession without a console
                            // uid), but a debt owed to a session that no longer
                            // exists is not a debt.
                            None => kickstart_debt = 0,
                        }
                    }
                }
                Action::HandBack(uid) => {
                    fast_exits = 0;
                    if let Some((held, child, _)) = worker.take() {
                        stop_worker(held, child, "launchd took ownership", &delegate);
                    }
                    kickstart_debt = if kickstart_launch_agent(uid) {
                        0
                    } else {
                        KICKSTART_RETRIES
                    };
                }
                Action::Replace(uid) => {
                    fast_exits = 0;
                    if let Some((old, child, _)) = worker.take() {
                        tracing::info!(
                            old_uid = old,
                            new_uid = uid,
                            "macOS supervisor: console user changed"
                        );
                        stop_worker(old, child, "console user changed", &delegate);
                    }
                    // Spawn on the next tick, through the normal Spawn arm.
                }
                Action::Spawn(uid) => {
                    if wait_until.is_none_or(|t| Instant::now() >= t) {
                        // Opens the per-uid socket AND mints the secret:
                        // the endpoint must exist before the child can dial it,
                        // and tying the two together means neither can be
                        // set up without the other.
                        let secret = delegate.open_for(uid);
                        match spawn_worker(uid, &exe, &secret, one_row) {
                            Ok(child) => {
                                tracing::info!(uid, "macOS supervisor: spawned GUI worker");
                                worker = Some((uid, child, Instant::now()));
                                wait_until = None;
                            }
                            Err(e) => {
                                backoff = next_backoff(backoff, Duration::ZERO);
                                wait_until = Some(Instant::now() + backoff);
                                tracing::warn!(
                                    uid,
                                    error = %e,
                                    retry_in_secs = backoff.as_secs(),
                                    "macOS supervisor: spawn failed"
                                );
                            }
                        }
                    }
                }
            }

            let tick =
                next_tick_in(wait_until.map(|t| t.saturating_duration_since(Instant::now())));
            tokio::select! {
                _ = tokio::time::sleep(tick) => {}
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        if let Some((uid, child, _)) = worker.take() {
                            stop_worker(uid, child, "daemon shutting down", &delegate);
                        }
                        return;
                    }
                }
            }
        }
    }

    /// #2987 — the reap's two system halves on a real Mac, the only place
    /// this module compiles: BSD `ps` prints the rows `stale_workers` parses,
    /// and a process we did not spawn is found by its exact command line and
    /// stopped. Runs in the macOS CI lane (`macos_supervisor::`).
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::Duration;

        /// What `/usr/bin/id <args>` prints, trimmed.
        fn id(args: &[&str]) -> String {
            let out = std::process::Command::new("/usr/bin/id")
                .args(args)
                .output()
                .expect("id runs");
            String::from_utf8(out.stdout)
                .expect("utf-8")
                .trim()
                .to_string()
        }

        /// #3802 — the account lookup behind the administrator's command
        /// reads this process's own entry: the name `id -un` prints, and the
        /// home `appdirs::account_home` reads. Holds whoever the runner is.
        #[test]
        fn the_account_lookup_reads_the_processs_own_name_and_home() {
            // SAFETY: `getuid` takes no arguments and cannot fail.
            let uid = unsafe { libc::getuid() };
            assert_eq!(
                name_and_home(uid),
                Some((
                    id(&["-un"]),
                    crate::appdirs::account_home(uid).expect("a home")
                )),
            );
        }

        /// #3802 — `dseditgroup` says no for a name no account has, and for
        /// the runner's own name agrees with whether `id -Gn` lists `admin`.
        #[tokio::test]
        async fn the_administrator_read_agrees_with_the_groups_id_lists() {
            let missing = format!("roomler-no-such-account-{}", std::process::id());
            let answer = tokio::time::timeout(Duration::from_secs(30), is_administrator(&missing))
                .await
                .expect("dseditgroup answers");
            assert!(
                matches!(
                    answer,
                    Err(super::super::NoCommand::NotAdministrator(Some(_)))
                ),
                "{missing}: {answer:?}"
            );
            let name = id(&["-un"]);
            let admin = id(&["-Gn", name.as_str()])
                .split_whitespace()
                .any(|g| g == "admin");
            let answer = tokio::time::timeout(Duration::from_secs(30), is_administrator(&name))
                .await
                .expect("dseditgroup answers");
            assert_eq!(answer.is_ok(), admin, "{name}: {answer:?}");
        }

        /// #3802 — the whole read, for a folder of root's in the runner's
        /// own `Library/Application Support`, returns inside its bound
        /// whoever the runner is, and names the runner when it says yes. A
        /// deadline that has run out answers that it ran out.
        #[tokio::test]
        async fn the_whole_administrator_read_returns_inside_its_bound() {
            // SAFETY: `getuid` takes no arguments and cannot fail.
            let uid = unsafe { libc::getuid() };
            let home = crate::appdirs::account_home(uid).expect("a home");
            let f = super::super::FolderNotTheirs {
                path: home.join("Library/Application Support/live.roomler.roomler-3802"),
                uid,
                owner: 0,
                mode: 0o755,
                is_dir: true,
                kept: Vec::new(),
                holds_config: false,
                stamp: "20261113T101502Z".to_string(),
                administrator: Err(super::super::NoCommand::NotRead),
            };
            let started = std::time::Instant::now();
            let answer = administrator_reads(&f).await;
            let took = started.elapsed();
            assert!(
                took < super::super::ADMIN_READ_DEADLINE + Duration::from_secs(1),
                "{took:?}: {answer:?}"
            );
            if let Ok(name) = &answer {
                assert_eq!(*name, id(&["-un"]));
            }
            let started = std::time::Instant::now();
            let ran_out = administrator_reads_within(&f, Duration::ZERO).await;
            assert!(started.elapsed() < Duration::from_secs(1), "{ran_out:?}");
            if answer.is_ok() {
                // Every read says yes here, so only the deadline can stop it.
                assert_eq!(ran_out, Err(super::super::NoCommand::TimedOut));
            } else {
                assert!(ran_out.is_err(), "{ran_out:?}");
            }
        }

        #[tokio::test]
        async fn a_process_left_behind_is_found_by_its_exact_command_and_stopped() {
            // SAFETY: `getuid` takes no arguments and cannot fail.
            if unsafe { libc::getuid() } == 0 {
                // `stale_workers` never matches root's processes, by design.
                eprintln!("skipped: running as root");
                return;
            }
            // A command line nothing else on the runner has. Detached by the
            // shell, so it is not our child, the same as a stale worker.
            let command = format!("/bin/sleep {}", 29_870 + std::process::id() % 1000);
            let status = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("{command} </dev/null >/dev/null 2>&1 &"))
                .status()
                .expect("sh");
            assert!(status.success());
            let mut found = Vec::new();
            for _ in 0..50 {
                found = find_processes(std::slice::from_ref(&command));
                if !found.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert_eq!(found.len(), 1, "found {found:?}");
            // SAFETY: as above.
            assert_eq!(found[0].1, unsafe { libc::getuid() });
            stop_processes(&[found[0].0]).await;
            assert!(
                find_processes(std::slice::from_ref(&command)).is_empty(),
                "still running after stop_processes"
            );
        }

        /// A file the shell below writes its background holder's pid to.
        fn pid_file(tag: &str) -> std::path::PathBuf {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            std::env::temp_dir().join(format!(
                "roomler-3080-{tag}-{}-{nanos}.pid",
                std::process::id()
            ))
        }

        /// #3080's end state: the shell prints one row shaped like `ps`
        /// output and exits, while the `sleep` it started in the background
        /// still holds the write end of its stdout, as `route -n monitor`
        /// held the real `ps`'s.
        fn pinned_ps(pid_file: &std::path::Path, row: &str) -> Command {
            let mut c = Command::new("/bin/sh");
            c.arg("-c")
                .arg(format!("sleep 30 & echo $! > \"$PIDFILE\"; echo '{row}'"))
                .env("PIDFILE", pid_file);
            c
        }

        /// End the holder [`pinned_ps`] started.
        fn end_holder(pid_file: &std::path::Path) {
            for _ in 0..250 {
                if let Some(pid) = std::fs::read_to_string(pid_file)
                    .ok()
                    .and_then(|s| s.trim().parse::<libc::pid_t>().ok())
                {
                    // SAFETY: one signal to the `sleep` this test started.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    let _ = std::fs::remove_file(pid_file);
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("the holder never wrote its pid to {}", pid_file.display());
        }

        /// #3080 — the reap answers as soon as `ps` exits, while another
        /// process still holds `ps`'s pipe.
        #[test]
        fn the_reap_answers_once_ps_exits_though_its_pipe_is_held() {
            let pid_file = pid_file("ps");
            let command = worker_command(
                "/Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd",
                false,
            );
            let started = Instant::now();
            let found = find_processes_via(
                &mut pinned_ps(&pid_file, &format!("  4242   501 {command}")),
                std::slice::from_ref(&command),
            );
            let took = started.elapsed();
            end_holder(&pid_file);
            assert_eq!(found, vec![(4242, 501)]);
            assert!(
                took < Duration::from_secs(5),
                "waited {took:?} on the holder"
            );
        }

        /// The positive control on a Mac: `Command::output()`, the read
        /// `find_processes` made before #3080, waits as long as the holder
        /// lives.
        #[test]
        fn before_3080_the_reap_waited_as_long_as_the_holder_lived() {
            let pid_file = pid_file("std");
            let mut cmd = pinned_ps(&pid_file, "  4242   501 x");
            let (tx, rx) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                let _ = tx.send(cmd.output());
            });
            let early = rx.recv_timeout(Duration::from_millis(1500));
            end_holder(&pid_file);
            assert!(
                early.is_err(),
                "output() returned while the holder lived: {early:?}"
            );
            rx.recv_timeout(Duration::from_secs(10))
                .expect("output() returns once the holder is gone")
                .expect("ran");
            reader.join().unwrap();
        }

        /// Whether the program a child execs still has descriptor 9 open, when
        /// the child got 9 the way the race hands one over: a copy made after
        /// the fork, open and without close-on-exec. (`tunnel_core::child`'s
        /// own tests run in a Linux lane; the macOS lane runs this module's.)
        /// In a quiet process 9 may be std's own exec-error pipe; replacing it
        /// is harmless here, because `/bin/sh` always execs.
        fn fd9_after_exec(sweep: bool) -> String {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c")
                .arg("if true 2>/dev/null >&9; then echo open; else echo closed; fi");
            // SAFETY: `dup2` only, which is async-signal-safe.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::dup2(2, 9) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            if sweep {
                tunnel_core::child::inherit_only_stdio(&mut cmd);
            }
            let out =
                tunnel_core::child::output_bounded(&mut cmd, Duration::from_secs(10)).expect("ran");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }

        /// #3080 — the other side of the race, on a Mac: without the sweep the
        /// program keeps the descriptor (the positive control), with it the
        /// program keeps only its stdio. The worker chain and the route monitor
        /// are spawned with the sweep.
        #[test]
        fn a_long_lived_child_keeps_only_its_stdio() {
            assert_eq!(fd9_after_exec(false), "open");
            assert_eq!(fd9_after_exec(true), "closed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FR-97 / FR-119 — the worker gets the root's value of each forwarded
    /// knob the root has, as a real variable, in the allowlist's order and
    /// verbatim, and nothing for a knob the root lacks or one not on the list.
    #[test]
    fn the_worker_inherits_the_roots_session_knobs_and_nothing_else() {
        let root = |suffix: &str| match suffix {
            "FPS_DENY" => Some("*:144".to_string()),
            "CLIPBOARD_MAC_WATCH" => Some("0".to_string()),
            "ENCODER_CELLS_DENY" => Some("hevc_videotoolbox:yuv444".to_string()),
            // FR-118 P5 — the switch and its denylist bind the worker too.
            "ZERO_COPY" => Some("1".to_string()),
            "ZERO_COPY_DENY" => Some("hevc_videotoolbox:gpu".to_string()),
            // FR-130 — the root's `color_description = false` binds the worker.
            "COLOR_DESCRIPTION" => Some("0".to_string()),
            // FR-97 P2c — the root's `[virtual_desktop_apps] enabled = false`.
            "APPS" => Some("0".to_string()),
            // Set on the root, but root-only: never forwarded.
            "RELAY_SERVER_ENABLED" => Some("true".to_string()),
            _ => None,
        };
        assert_eq!(
            worker_env_args(root),
            vec![
                "ROOMLERD_CLIPBOARD_MAC_WATCH=0",
                "ROOMLERD_APPS=0",
                "ROOMLERD_FPS_DENY=*:144",
                "ROOMLERD_ENCODER_CELLS_DENY=hevc_videotoolbox:yuv444",
                "ROOMLERD_ZERO_COPY=1",
                "ROOMLERD_ZERO_COPY_DENY=hevc_videotoolbox:gpu",
                "ROOMLERD_COLOR_DESCRIPTION=0",
            ]
        );
        assert!(
            worker_env_args(|_| None).is_empty(),
            "no knob set, no `env` at all"
        );
    }

    /// The worker is a full agent: a knob only the root may act on must never
    /// join the list, or the person's session would start a second one.
    #[test]
    fn no_root_only_knob_is_forwarded_to_the_worker() {
        for root_only in [
            "RELAY_SERVER_ENABLED",
            "OVERLAY_NETSTACK_SOCKS",
            "AUTO_UPDATE",
            "UPDATE_URL",
            "PERSON_SOCKET",
        ] {
            assert!(
                !WORKER_ENV_FORWARD.contains(&root_only),
                "{root_only} is the root's alone"
            );
        }
    }

    /// FR-138 P4-0 — one device, one switch: the root's
    /// `rc_sessions_survive_reattach`, set in its environment or its config,
    /// binds the supervised worker, whose own table would otherwise follow the
    /// person's config. Its off reaches the worker as off.
    #[test]
    fn the_roots_resume_switch_binds_the_worker() {
        assert!(WORKER_ENV_FORWARD.contains(&"RC_SESSIONS_SURVIVE_REATTACH"));
        for value in ["0", "off", "1"] {
            assert_eq!(
                worker_env_args(
                    |s| (s == "RC_SESSIONS_SURVIVE_REATTACH").then(|| value.to_string())
                ),
                vec![format!("ROOMLERD_RC_SESSIONS_SURVIVE_REATTACH={value}")],
                "{value}"
            );
        }
    }

    fn base() -> Inputs {
        Inputs {
            enabled: true,
            is_root: true,
            console_uid: Some(501),
            launch_agent_loaded: false,
            worker_uid: None,
            consecutive_fast_exits: 0,
        }
    }

    #[test]
    fn off_or_unprivileged_does_nothing() {
        assert_eq!(
            decide(Inputs {
                enabled: false,
                ..base()
            }),
            Action::Disabled
        );
        assert_eq!(
            decide(Inputs {
                is_root: false,
                ..base()
            }),
            Action::Disabled
        );
        // Even holding a worker, the switch wins — flipping it off must
        // restore today's behaviour without a restart.
        assert_eq!(
            decide(Inputs {
                enabled: false,
                worker_uid: Some(501),
                ..base()
            }),
            Action::Disabled
        );
    }

    #[test]
    fn no_console_user_means_nothing_to_supervise() {
        assert_eq!(
            decide(Inputs {
                console_uid: None,
                ..base()
            }),
            Action::NoGuiSession
        );
        // Holding a worker for a session that has ended is not "healthy".
        assert_eq!(
            decide(Inputs {
                console_uid: None,
                worker_uid: Some(501),
                ..base()
            }),
            Action::NoGuiSession
        );
    }

    /// The property that makes P1 safe to flip on a live Mac: while the
    /// LaunchAgent is loaded we never spawn, so one enrollment is never
    /// served twice and the hub never displaces one of our own connections.
    #[test]
    fn launchd_ownership_wins_always() {
        assert_eq!(
            decide(Inputs {
                launch_agent_loaded: true,
                ..base()
            }),
            Action::LaunchdOwns
        );
        // Including when we already hold a worker: a re-install bootstraps
        // the plist back, and the loser of that race must be us. Holding one
        // makes it a HAND-BACK rather than a plain stand-down, because
        // dropping ours is only half the job.
        assert_eq!(
            decide(Inputs {
                launch_agent_loaded: true,
                worker_uid: Some(501),
                ..base()
            }),
            Action::HandBack(501)
        );
    }

    /// Handing back is two steps, so it is its own state.
    ///
    /// Field-measured on 0.4.35 (2026-08-31): stopping our worker without
    /// starting launchd's left the Mac with NO user half, because launchd's
    /// worker had already lost the single-instance lock race and exited **0**
    /// — which `KeepAlive{SuccessfulExit=false}` never retries. Collapsing
    /// this back into `LaunchdOwns` would silently restore that outage, so
    /// the distinction is asserted rather than assumed.
    #[test]
    fn handing_back_is_distinct_from_standing_down() {
        // Nothing held: there is nothing to hand back.
        assert_eq!(
            decide(Inputs {
                launch_agent_loaded: true,
                worker_uid: None,
                ..base()
            }),
            Action::LaunchdOwns
        );
        // The uid carried is the CONSOLE user's — the session whose
        // LaunchAgent has to end up running — not necessarily the uid our
        // stale worker was spawned for.
        assert_eq!(
            decide(Inputs {
                launch_agent_loaded: true,
                console_uid: Some(502),
                worker_uid: Some(501),
                ..base()
            }),
            Action::HandBack(502)
        );
        // No session means no LaunchAgent to hand back TO: dropping our
        // worker is the whole job, and kickstarting `gui/<nobody>` would
        // fail. `console_uid` is checked before `launch_agent_loaded`.
        assert_eq!(
            decide(Inputs {
                launch_agent_loaded: true,
                console_uid: None,
                worker_uid: Some(501),
                ..base()
            }),
            Action::NoGuiSession
        );
    }

    #[test]
    fn spawns_when_the_field_is_clear_and_holds_when_healthy() {
        assert_eq!(decide(base()), Action::Spawn(501));
        assert_eq!(
            decide(Inputs {
                worker_uid: Some(501),
                ..base()
            }),
            Action::Healthy
        );
    }

    #[test]
    fn console_user_change_replaces_the_worker() {
        assert_eq!(
            decide(Inputs {
                console_uid: Some(502),
                worker_uid: Some(501),
                ..base()
            }),
            Action::Replace(502)
        );
    }

    #[test]
    fn backoff_climbs_on_fast_exits_and_resets_after_a_real_run() {
        let mut b = BACKOFF_MIN;
        for _ in 0..10 {
            b = next_backoff(b, Duration::from_secs(1));
        }
        assert_eq!(b, BACKOFF_MAX, "must saturate, not grow without bound");
        assert_eq!(
            next_backoff(b, HEALTHY_RUN),
            BACKOFF_MIN,
            "a worker that actually ran resets the ladder"
        );
        // The boundary itself counts as healthy.
        assert_eq!(next_backoff(BACKOFF_MAX, HEALTHY_RUN), BACKOFF_MIN);
    }

    /// FR-43 AC11: a healthy worker's 1 s floor is the whole wait. Rounded up to
    /// the next poll, a worst-phase `kill -9` took 10.09 s to come back.
    #[test]
    fn a_backoff_shorter_than_a_poll_sets_the_next_tick() {
        assert_eq!(next_tick_in(Some(BACKOFF_MIN)), BACKOFF_MIN);
        assert_eq!(
            next_tick_in(None),
            POLL,
            "nothing pending: an ordinary tick"
        );
        assert_eq!(
            next_tick_in(Some(BACKOFF_MAX)),
            POLL,
            "a long backoff still reads the world every poll"
        );
    }

    /// The arms that do not spawn leave `wait_until` behind, so a backoff that
    /// is over must not become a zero sleep: the loop would spin at full speed
    /// for as long as the Mac sits at the login window.
    #[test]
    fn a_backoff_already_over_is_an_ordinary_tick_not_a_spin() {
        assert_eq!(next_tick_in(Some(Duration::ZERO)), POLL);
    }

    /// The 2026-08-30 outage in one assertion: a worker that dies on arrival
    /// must eventually STOP being respawned. Nothing bounded this, so the
    /// supervisor hammered a doomed worker for as long as the LaunchAgent
    /// stayed unloaded, and the machine looked simply dead.
    #[test]
    fn gives_up_after_enough_workers_die_on_arrival() {
        for n in 0..MAX_FAST_EXITS {
            assert_eq!(
                decide(Inputs {
                    consecutive_fast_exits: n,
                    ..base()
                }),
                Action::Spawn(501),
                "still trying at {n} failures"
            );
        }
        assert_eq!(
            decide(Inputs {
                consecutive_fast_exits: MAX_FAST_EXITS,
                ..base()
            }),
            Action::GaveUp
        );
    }

    /// Giving up must not be a latch: anything that changes the situation
    /// gets another go, because the usual fix (enrol the per-user half, log
    /// in) happens outside this process.
    #[test]
    fn giving_up_yields_to_a_changed_situation() {
        let wedged = Inputs {
            consecutive_fast_exits: MAX_FAST_EXITS + 3,
            ..base()
        };
        assert_eq!(
            decide(Inputs {
                launch_agent_loaded: true,
                ..wedged
            }),
            Action::LaunchdOwns
        );
        assert_eq!(
            decide(Inputs {
                console_uid: None,
                ..wedged
            }),
            Action::NoGuiSession
        );
        assert_eq!(
            decide(Inputs {
                enabled: false,
                ..wedged
            }),
            Action::Disabled
        );
        // And a live worker is still healthy — the counter only gates SPAWN.
        assert_eq!(
            decide(Inputs {
                worker_uid: Some(501),
                ..wedged
            }),
            Action::Healthy
        );
    }

    // FR-97 P6e-2 — which ends leave a crash record.
    #[test]
    fn a_kill_or_a_fault_leaves_a_record() {
        let ran = Duration::from_secs(42);
        for (sig, name) in [
            (9, "SIGKILL"),
            (11, "SIGSEGV"),
            (10, "SIGBUS"),
            (6, "SIGABRT"),
            (4, "SIGILL"),
        ] {
            let s = crash_summary(WorkerEnd::Signal(sig), 501, ran).expect("a crash");
            assert_eq!(
                s,
                format!(
                    "macOS supervisor: the GUI worker (uid 501) was killed by {name} ({sig}) after 42 s"
                )
            );
        }
        assert_eq!(
            crash_summary(WorkerEnd::Signal(24), 501, ran).as_deref(),
            Some("macOS supervisor: the GUI worker (uid 501) was killed by signal 24 after 42 s")
        );
        assert_eq!(
            crash_summary(WorkerEnd::Code(1), 501, ran).as_deref(),
            Some("macOS supervisor: the GUI worker (uid 501) exited with code 1 after 42 s")
        );
        // A panic on the main thread exits 101. The worker's own hook recorded
        // it on the WORKER's row; this record is the daemon's, so it stays.
        assert!(crash_summary(WorkerEnd::Code(101), 501, ran).is_some());
    }

    #[test]
    fn an_end_the_agent_chose_or_a_session_asked_for_is_not_a_crash() {
        use crate::watchdog::{
            AGENT_DELETED_EXIT_CODE, ALREADY_RUNNING_EXIT_CODE, RESTART_REQUESTED_EXIT_CODE,
            STALL_EXIT_CODE,
        };
        let ran = Duration::from_secs(42);
        for code in [
            0,
            STALL_EXIT_CODE,
            AGENT_DELETED_EXIT_CODE,
            ALREADY_RUNNING_EXIT_CODE,
            RESTART_REQUESTED_EXIT_CODE,
        ] {
            assert_eq!(
                crash_summary(WorkerEnd::Code(code), 501, ran),
                None,
                "code {code}"
            );
        }
        // SIGHUP, SIGINT, SIGTERM.
        for sig in [1, 2, 15] {
            assert_eq!(
                crash_summary(WorkerEnd::Signal(sig), 501, ran),
                None,
                "signal {sig}"
            );
        }
    }

    #[test]
    fn exit_code_nine_is_a_restart_and_signal_nine_a_kill() {
        // The collision this test exists for: renumber either and it is moot.
        assert_eq!(crate::watchdog::RESTART_REQUESTED_EXIT_CODE, 9);
        assert_eq!(SIGKILL, 9);
        let ran = Duration::from_secs(42);
        assert_eq!(crash_summary(WorkerEnd::Code(9), 501, ran), None);
        assert!(crash_summary(WorkerEnd::Signal(9), 501, ran).is_some());
    }

    /// The raw statuses are the ones measured on a Mac (FR-97 §8): perl's `$?`
    /// read 9 for `kill -9`, 11 for `-SEGV`, and 1792 for `exit 7`. The
    /// encoding is the traditional one, the same on Linux, so this runs there.
    #[cfg(unix)]
    #[test]
    fn a_wait_status_reads_as_its_signal_or_its_code() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        assert_eq!(
            worker_end(ExitStatus::from_raw(9)),
            Some(WorkerEnd::Signal(9))
        );
        assert_eq!(
            worker_end(ExitStatus::from_raw(11)),
            Some(WorkerEnd::Signal(11))
        );
        assert_eq!(
            worker_end(ExitStatus::from_raw(1792)),
            Some(WorkerEnd::Code(7))
        );
        assert_eq!(
            worker_end(ExitStatus::from_raw(0)),
            Some(WorkerEnd::Code(0))
        );
        // A signal that dumped core sets 0x80; it is still that signal.
        assert_eq!(
            worker_end(ExitStatus::from_raw(11 | 0x80)),
            Some(WorkerEnd::Signal(11))
        );
    }

    // #2987 — the stdin handoff and the lifeline it carries.

    /// A worker from before #2987 reads ONE line and uses all of it as the
    /// secret. The offer must not change that line, or every such worker the
    /// new supervisor spawns (a downgrade, a half-applied update) would be
    /// refused its attach.
    #[test]
    fn the_offer_is_a_line_of_its_own_after_the_secret() {
        let secret = "a1b2c3";
        let handoff = stdin_handoff(secret);
        assert_eq!(handoff, "a1b2c3\nroomler-lifeline/1\n");
        assert_eq!(handoff.lines().next().map(str::trim), Some(secret));
        // A real secret is 32 bytes in hex (`delegate::SECRET_BYTES`).
        assert!(
            stdin_handoff(&"ab".repeat(32)).len() < 512,
            "one write under PIPE_BUF, so it is atomic"
        );
    }

    fn link(input: &str) -> (Option<String>, LinkEnd) {
        let mut got = None;
        let end = stdin_link(std::io::Cursor::new(input.as_bytes().to_vec()), |s| got = s);
        (got, end)
    }

    #[test]
    fn a_supervisor_from_before_the_lifeline_arms_nothing() {
        // The old supervisor closed its end after the secret: end-of-file
        // where the offer would be. The worker attaches as it always did and
        // must NOT read that EOF as its daemon ending.
        let (secret, end) = link("a1b2c3\n");
        assert_eq!(secret.as_deref(), Some("a1b2c3"));
        assert!(matches!(end, LinkEnd::NotOffered), "{end:?}");
    }

    #[test]
    fn an_offer_this_worker_does_not_know_arms_nothing() {
        let (secret, end) = link("a1b2c3\nroomler-lifeline/2\n");
        assert_eq!(secret.as_deref(), Some("a1b2c3"));
        assert!(matches!(end, LinkEnd::Unknown), "{end:?}");
        let (_, end) = link("a1b2c3\n\n");
        assert!(
            matches!(end, LinkEnd::Unknown),
            "a blank second line: {end:?}"
        );
    }

    #[test]
    fn no_secret_is_no_secret_and_no_watch() {
        for input in ["", "\n", "   \n"] {
            let (secret, end) = link(input);
            assert_eq!(secret, None, "{input:?}");
            assert!(matches!(end, LinkEnd::NoSecret), "{input:?}: {end:?}");
        }
    }

    #[test]
    fn the_offer_then_end_of_file_is_the_daemon_gone() {
        // A daemon that died right after the spawn: the bytes it wrote are
        // still read, then the pipe ends.
        let (secret, end) = link("a1b2c3\nroomler-lifeline/1\n");
        assert_eq!(secret.as_deref(), Some("a1b2c3"));
        assert!(matches!(end, LinkEnd::Cut), "{end:?}");
    }

    /// The property itself, over a real OS pipe: the secret arrives at once,
    /// the watch holds for as long as the writer does, whatever else it
    /// writes, and ends the moment the writer's end closes. Closing the write
    /// end is all a dying process does to the pipe, whatever killed it.
    #[test]
    fn the_watch_holds_while_the_daemon_does_and_ends_when_it_goes() {
        use std::io::Write;
        use std::sync::mpsc;
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let (secret_tx, secret_rx) = mpsc::channel();
        let (end_tx, end_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let end = stdin_link(std::io::BufReader::new(reader), |s| {
                let _ = secret_tx.send(s);
            });
            let _ = end_tx.send(end);
        });
        writer
            .write_all(stdin_handoff("a1b2c3").as_bytes())
            .expect("handoff");
        assert_eq!(
            secret_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the secret arrives without waiting for the pipe to end"),
            Some("a1b2c3".to_string())
        );
        assert!(
            end_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "the watch ended while the daemon still holds its end"
        );
        // More bytes are not an end: a newer supervisor may say more.
        writer.write_all(b"anything\n").expect("write");
        assert!(end_rx.recv_timeout(Duration::from_millis(300)).is_err());
        drop(writer);
        let end = end_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the watch ends when the writer's end closes");
        assert!(matches!(end, LinkEnd::Cut), "{end:?}");
    }

    const EXE: &str = "/Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd";

    /// The shape `ps -axww -o pid= -o uid= -o command=` prints, with the
    /// processes a supervised Mac actually runs: the 10-26 orphan was pid
    /// 68123, its `sudo` 68122.
    const PS: &str = "    1     0 /sbin/launchd
  412     0 /Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd --config /etc/roomler/config.toml run
68122     0 sudo -u #501 /usr/bin/env ROOMLERD_FPS_DENY=*:144 /Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd run --supervised
68123   501 /Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd run --supervised
70001   501 /Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd run
70002   501 /Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd run --supervised --extra
70003   501 /Users/someone/dev/target/debug/roomlerd run --supervised
70004     0 /Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd run --supervised
70005   502 /Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd run --supervised
";

    /// Each match gets a SIGTERM from root, so only an exact supervised
    /// worker of THIS install, run as a person, matches: not the daemon, not
    /// launchd's worker (`… run`), not the `sudo` above a worker, not another
    /// build, not a root process.
    #[test]
    fn only_a_supervised_worker_of_this_install_is_stale() {
        let commands = worker_commands(EXE);
        assert_eq!(commands[0], format!("{EXE} run --supervised"));
        assert_eq!(
            stale_workers(PS, &commands, 412),
            vec![(68123, 501), (70005, 502)]
        );
        // Never this process.
        assert_eq!(stale_workers(PS, &commands, 70005), vec![(68123, 501)]);
        assert!(stale_workers("", &commands, 412).is_empty());
        assert!(stale_workers("  PID   UID COMMAND\nnot a row\n", &commands, 412).is_empty());
    }

    /// FR-43 P3a — a worker with no enrollment holds the same single-instance
    /// lock, so the reap finds it, whatever the switch reads now: the switch may
    /// have moved since that worker started.
    #[test]
    fn the_reap_finds_a_worker_of_either_kind() {
        assert_eq!(
            worker_commands(EXE)[1],
            format!("{EXE} run --supervised --unenrolled")
        );
        let ps = format!(
            "  601   501 {EXE} run --supervised\n  \
             602   501 {EXE} run --supervised --unenrolled\n  \
             603   501 {EXE} run --unenrolled\n"
        );
        assert_eq!(
            stale_workers(&ps, &worker_commands(EXE), 1),
            vec![(601, 501), (602, 501)]
        );
    }

    /// FR-43 P3a — the spawn and the reap read the same arguments, and the switch
    /// decides them.
    #[test]
    fn the_switch_decides_how_a_worker_starts() {
        assert_eq!(worker_args(false), ["run", "--supervised"]);
        assert_eq!(worker_args(true), ["run", "--supervised", "--unenrolled"]);
        assert_eq!(
            worker_command(EXE, true),
            format!("{EXE} {}", worker_args(true).join(" "))
        );
    }

    #[test]
    fn an_install_path_with_spaces_still_matches_exactly() {
        let exe = "/Applications/Roomler Beta.app/Contents/MacOS/roomlerd";
        let ps = format!("  555   501 {exe} run --supervised\n  556   501 {exe} run\n");
        assert_eq!(
            stale_workers(&ps, &worker_commands(exe), 1),
            vec![(555, 501)]
        );
    }

    /// The 10-26 MacBook gave up naming "no per-user enrollment" while every
    /// worker had exited 0 on a held lock. The exit code tells them apart.
    ///
    /// #3802 — both commands need root (a person's `launchctl kickstart` of
    /// a system job answers "Operation not permitted", and `asuser` is
    /// root's), so each is printed with `sudo`.
    #[test]
    fn the_give_up_names_the_lock_when_workers_exit_cleanly() {
        let lock = gave_up_reason(Some(WorkerEnd::Code(0)), false, None);
        assert_eq!(
            lock,
            "each worker exited 0 at once: another roomlerd holds this person's single-instance \
             lock, and it is not a worker this supervisor can identify (a hand-run `roomlerd \
             run`, say). Find it with: pgrep -lU <uid> roomlerd. Once it is gone, restart this \
             daemon: sudo launchctl kickstart -k system/com.roomler.daemon"
        );
        for end in [Some(WorkerEnd::Code(1)), Some(WorkerEnd::Signal(9)), None] {
            assert_eq!(
                gave_up_reason(end, false, None),
                "almost always this Mac has no per-user enrollment: the worker runs AS the \
                 console user and loads THAT user's config. Check with: sudo launchctl asuser \
                 <uid> sudo -u '#<uid>' roomlerd caps",
                "{end:?}"
            );
        }
    }

    /// #3802 — a person's folder that a root step made: the give-up names the
    /// folder, its owner and the one command, under either switch and whatever
    /// the exit, and never sends the person to the log the worker could not
    /// write.
    #[test]
    fn the_give_up_names_a_folder_another_account_owns() {
        let path =
            std::path::PathBuf::from("/Users/p/Library/Application Support/live.roomler.roomler");
        assert_eq!(
            folder_not_theirs(path.clone(), 501, 501, 0o40755, Vec::new(), STAMP),
            None
        );
        let root_made =
            folder_not_theirs(path.clone(), 501, 0, 0o40755, Vec::new(), STAMP).expect("root's");
        assert_eq!(root_made.mode, 0o755);
        assert!(root_made.is_dir);
        assert_eq!(path.to_str(), Some(F));
        // The same two lines whatever the exit and the switch: never the log
        // the worker could not write, never an enrollment.
        let does_not_fit = format!(
            "{}{ONLY} This line gives no command for it: an administrator of this Mac has to \
             move it aside; {DOC}",
            cause(F)
        );
        let moves = format!(
            "{}{ONLY}{WHO}{}sudo mv '{F}' '{F}.made-by-root-{STAMP}' ({INERT}; if the move \
             fails, nothing moved), and the worker makes the folder again as the person at its \
             next start.{RESTART}",
            cause(F),
            guard(F)
        );
        for one_row in [true, false] {
            for end in [
                Some(WorkerEnd::Code(1)),
                Some(WorkerEnd::Code(0)),
                Some(WorkerEnd::Signal(9)),
                None,
            ] {
                // No read has said yes for it: the cause, and no command.
                assert_eq!(
                    gave_up_reason(end, one_row, Some(&root_made)),
                    does_not_fit,
                    "{end:?} {one_row}"
                );
                // The console person is an administrator of this Mac. It
                // holds nothing the revert reads: the move is the whole
                // command.
                let admin = FolderNotTheirs {
                    administrator: Ok("p".to_string()),
                    ..root_made.clone()
                };
                assert_eq!(
                    gave_up_reason(end, one_row, Some(&admin)),
                    moves,
                    "{end:?} {one_row}"
                );
            }
        }
    }

    /// #3802 — the person's folder in these tests. Its path always holds a
    /// space ("Application Support").
    const F: &str = "/Users/p/Library/Application Support/live.roomler.roomler";
    /// The UTC second of the read, in the names the command makes beside it.
    const STAMP: &str = "20261113T101502Z";
    /// What `lstat` reads of a kept copy that is the person's own, mode 644.
    const THEIRS: Option<(u32, u32)> = Some((501, 0o100644));
    /// A copy the conversion kept of a file of root's, mode 600.
    const ROOTS_600: Option<(u32, u32)> = Some((0, 0o100600));

    /// #3802 — the line for a folder of root's at `path` (full `mode`) that
    /// holds `kept`, each one of [`kept_for_revert`] with what `lstat` read,
    /// on a Mac whose reads all said yes: the console person `p` is an
    /// administrator of it, and the path names the folder it reads as.
    fn give_up_line(
        case: &str,
        path: &str,
        mode: u32,
        kept: &[(usize, Option<(u32, u32)>)],
    ) -> String {
        give_up_line_for(case, path, mode, kept, Some("p"))
    }

    /// #3802 — the same with what those reads gave: `administrator`, the
    /// console person's name where they all said yes. Printed, so the lines
    /// are read off a run with `--nocapture`.
    fn give_up_line_for(
        case: &str,
        path: &str,
        mode: u32,
        kept: &[(usize, Option<(u32, u32)>)],
        administrator: Option<&str>,
    ) -> String {
        let names = kept_for_revert();
        let kept = kept
            .iter()
            .map(|(i, stat)| KeptCopy {
                name: names[*i].clone(),
                stat: *stat,
            })
            .collect();
        let folder = FolderNotTheirs {
            administrator: administrator.map(str::to_string).ok_or(NoCommand::NotRead),
            ..folder_not_theirs(path.into(), 501, 0, mode, kept, STAMP).expect("root's")
        };
        let why = gave_up_reason(Some(WorkerEnd::Code(1)), true, Some(&folder));
        println!("GIVE-UP [{case}] {why}");
        why
    }

    /// The cause every line starts with, for the folder `path` of root's.
    fn cause(path: &str) -> String {
        format!(
            "the worker cannot write the person's folder {path}: uid 0 owns it (mode 755), \
             not the person, so the worker's own log is not there either. "
        )
    }

    /// Why no command the person runs with their own rights can move it.
    const ONLY: &str = "Only an administrator of this Mac can move it aside, because macOS \
                        lets no other account rename a folder that root owns.";
    /// Who runs the command: the console person, an administrator of this
    /// Mac, in the package's words.
    const WHO: &str = " p is an administrator of this Mac and can run this: ";
    const RESTART: &str = " Then log out and back in, or restart this daemon: \
                           sudo launchctl kickstart -k system/com.roomler.daemon";
    /// The section of the docs a line with no command names.
    const DOC: &str = "see docs/installation.md, section \"Which account each step of the \
                       package's script writes as\".";
    /// The copies the revert reads, as the command names them.
    const PLIST: &str = "com.roomler.agent.plist.pre-one-row";
    const CONFIG: &str = "config.toml.pre-one-row";
    /// The one test every command starts with, for the folder `q` as it is
    /// quoted: still a real directory of root's (under `sudo`, `-O` is owned
    /// by root), and the name it moves the folder to not there yet.
    fn guard(q: &str) -> String {
        format!(
            "sudo test ! -h '{q}' -a -d '{q}' -a -O '{q}' -a ! -e '{q}.made-by-root-{STAMP}' && "
        )
    }
    /// What the line says that test means.
    const INERT: &str = "once the folder is no longer a real folder of root's, as when it is the \
                         person's again, it changes nothing";

    /// #3802 — macOS lets no account but root rename a folder of root's, so
    /// a command the person runs with their own rights cannot move it: the
    /// line gives an administrator's `sudo mv`, never a check of who runs it.
    #[test]
    fn the_give_up_line_moves_the_folder_with_an_administrators_sudo() {
        let why = give_up_line("RED: no kept copy", F, 0o40755, &[]);
        assert!(
            why.contains(&format!("sudo mv '{F}' '{F}.made-by-root-{STAMP}'")),
            "{why}"
        );
        assert!(!why.contains("id -u"), "{why}");
        assert!(!why.contains("as that person"), "{why}");
    }

    /// #3802 — nothing the revert reads: the administrator moves the folder
    /// aside, under a name no earlier print made, and the person's worker
    /// makes a new one.
    #[test]
    fn the_give_up_line_for_a_folder_that_holds_no_kept_copy() {
        assert_eq!(
            give_up_line("fits + no kept copy", F, 0o40755, &[]),
            format!(
                "{}{ONLY}{WHO}sudo test ! -h '{F}' -a -d '{F}' -a -O '{F}' -a ! -e \
                 '{F}.made-by-root-{STAMP}' && sudo mv '{F}' '{F}.made-by-root-{STAMP}' (once \
                 the folder is no longer a real folder of root's, as when it is the person's \
                 again, it changes nothing; if the move fails, nothing moved), and the worker \
                 makes the folder again as the person at its next start.{RESTART}",
                cause(F)
            )
        );
    }

    /// #3802 — a printed line does nothing when it is run again, and a line
    /// printed before the folder was given back does nothing when it is run
    /// after: each command starts with one test, before anything runs, that
    /// the folder is still a real directory of root's (not a link, a
    /// directory, and `-O`, owned by root under `sudo`) and that the name it
    /// moves the folder to is not there yet. Without the first, an older line
    /// in the log would move the person's live folder aside; without the
    /// second, BSD `mv` would move the live folder INTO the one the first run
    /// made. The line says so in words.
    #[test]
    fn the_give_up_line_does_nothing_once_the_folder_is_given_back_or_moved() {
        let first = format!("{WHO}{}sudo ", guard(F));
        for kept in [
            &[][..],
            &[(0, THEIRS)],
            &[(1, THEIRS)],
            &[(0, THEIRS), (1, THEIRS)],
        ] {
            let why = give_up_line("which commands", F, 0o40755, kept);
            assert_eq!(why.matches(first.as_str()).count(), 1, "{kept:?}: {why}");
            assert_eq!(why.matches("sudo test").count(), 1, "{kept:?}: {why}");
            assert_eq!(
                why.matches(&format!("({INERT}; ")).count(),
                1,
                "{kept:?}: {why}"
            );
        }
    }

    /// #3802 — the line for a folder that holds `copies` (names inside it),
    /// on a Mac whose reads all said yes: staged first, moved last. The
    /// person's account makes a new folder beside it and copies them in, out
    /// of the folder before it moves; then the folder is moved aside and the
    /// new one put in its place, and if that last move fails the one before
    /// it is undone and the command still fails (`false`).
    fn kept_line(path: &str, q: &str, admin: &str, copies: &[&str]) -> String {
        let aside = format!("{q}.made-by-root-{STAMP}");
        let new = format!("{q}.new-{STAMP}");
        let from: String = copies.iter().map(|c| format!("'{q}/{c}' ")).collect();
        let it = match copies.len() {
            1 => "it",
            _ => "them",
        };
        format!(
            "{}{ONLY} It holds {}, which `sudo roomlerd one-row --revert` reads there and \
             nowhere else, so the command copies {it} into a new folder that the person's \
             account makes. {admin} is an administrator of this Mac and can run this: {}sudo -u \
             '#501' mkdir '{new}' && sudo -u '#501' cp {from}'{new}/' && sudo mv '{q}' \
             '{aside}' && {{ sudo mv '{new}' '{q}' || {{ sudo mv '{aside}' '{q}'; false; }}; }} \
             ({INERT}; nothing moves until the copies are made, and if the last move fails, the \
             one before it is undone and the command fails).{RESTART}",
            cause(path),
            copies.join(" and "),
            guard(q),
        )
    }

    /// #3802 — the LaunchAgent plist the revert reads there and nowhere else:
    /// the person's account copies it into a new folder before anything
    /// moves, so a `cp` that fails leaves it where the revert reads it.
    #[test]
    fn the_give_up_line_keeps_the_launch_agent_plist_the_revert_reads() {
        let why = give_up_line("fits + the plist copy", F, 0o40755, &[(0, THEIRS)]);
        assert_eq!(
            why,
            format!(
                "{}{ONLY} It holds com.roomler.agent.plist.pre-one-row, which `sudo roomlerd \
                 one-row --revert` reads there and nowhere else, so the command copies it into \
                 a new folder that the person's account makes.{WHO}sudo test ! -h '{F}' -a -d \
                 '{F}' -a -O '{F}' -a ! -e '{F}.made-by-root-{STAMP}' && sudo -u '#501' mkdir \
                 '{F}.new-{STAMP}' && sudo -u '#501' cp \
                 '{F}/com.roomler.agent.plist.pre-one-row' '{F}.new-{STAMP}/' && sudo mv '{F}' \
                 '{F}.made-by-root-{STAMP}' && {{ sudo mv '{F}.new-{STAMP}' '{F}' || {{ sudo mv \
                 '{F}.made-by-root-{STAMP}' '{F}'; false; }}; }} (once the folder is no longer a \
                 real folder of root's, as when it is the person's again, it changes nothing; \
                 nothing moves until the copies are made, and if the last move fails, the one \
                 before it is undone and the command fails).{RESTART}",
                cause(F)
            )
        );
        assert_eq!(why, kept_line(F, F, "p", &[PLIST]));
    }

    /// #3802 — the same for the copy of the person's config kept beside it.
    #[test]
    fn the_give_up_line_keeps_the_config_copy_the_revert_reads() {
        assert_eq!(
            give_up_line("fits + the config copy", F, 0o40755, &[(1, THEIRS)]),
            kept_line(F, F, "p", &[CONFIG])
        );
    }

    /// #3802 — both copies go into the new folder, in one `cp`, as the
    /// person.
    #[test]
    fn the_give_up_line_keeps_both_copies_the_revert_reads() {
        assert_eq!(
            give_up_line(
                "fits + both kept copies",
                F,
                0o40755,
                &[(0, THEIRS), (1, THEIRS)]
            ),
            kept_line(F, F, "p", &[PLIST, CONFIG])
        );
    }

    /// #3802 — a kept copy the person cannot read (the conversion's copy of a
    /// config of root's, mode 600): the line says so and what moving the
    /// folder would cost, and gives no command that cannot succeed.
    #[test]
    fn the_give_up_line_names_a_kept_copy_the_person_cannot_read() {
        assert_eq!(
            give_up_line(
                "an unreadable copy",
                F,
                0o40755,
                &[(0, THEIRS), (1, ROOTS_600)]
            ),
            format!(
                "{}It holds config.toml.pre-one-row (uid 0, mode 600), which the person cannot \
                 read, and `sudo roomlerd one-row --revert` reads it there and nowhere else: if \
                 this folder is moved aside, a later revert would not restore that file, so \
                 this line gives no command to move it.",
                cause(F)
            )
        );
        // Only a copy `lstat` says they cannot read: the person's own without
        // the owner's read bit, or another account's with no group or other
        // read bit. Anything else is read by the `cp` before anything moves,
        // and a `cp` that fails stops the command with everything in place.
        for (stat, cannot) in [
            (Some((501, 0o100200)), true),
            (Some((0, 0o100640)), false),
            (Some((0, 0o100604)), false),
            (Some((0, 0o120755)), false),
            (None, false),
        ] {
            let why = give_up_line("which copies", F, 0o40755, &[(1, stat)]);
            assert_eq!(why.contains("cannot read"), cannot, "{stat:?}: {why}");
            assert_eq!(why.contains(" && "), !cannot, "{stat:?}: {why}");
        }
    }

    /// #3802 — every path is quoted for a POSIX shell, an apostrophe in the
    /// home included.
    #[test]
    fn the_give_up_line_quotes_an_apostrophe_in_the_path() {
        let path = "/Users/o'brien/Library/Application Support/live.roomler.roomler";
        let q = r"/Users/o'\''brien/Library/Application Support/live.roomler.roomler";
        let why = give_up_line_for(
            "fits + an apostrophe",
            path,
            0o40755,
            &[(0, THEIRS), (1, THEIRS)],
            Some("obrien"),
        );
        assert_eq!(why, kept_line(path, q, "obrien", &[PLIST, CONFIG]));
        // The apostrophe appears in the shell text only escaped.
        assert_eq!(why.matches("o'brien").count(), 1, "{why}");
    }

    /// #3802 — where a read said no or did not answer in time, the line
    /// gives no command at all: only who can move the folder, what it holds
    /// that the revert reads, and the section of the docs. The same for a
    /// folder of another person's (the package prints its line for a folder
    /// of root's only) and for a name that is not plain.
    #[test]
    fn the_give_up_line_gives_no_command_where_the_reads_do_not_fit() {
        assert_eq!(
            give_up_line_for("does not fit", F, 0o40755, &[], None),
            format!(
                "{}{ONLY} This line gives no command for it: an administrator of this Mac \
                 has to move it aside; {DOC}",
                cause(F)
            )
        );
        assert_eq!(
            give_up_line_for(
                "does not fit + both kept copies",
                F,
                0o40755,
                &[(0, THEIRS), (1, THEIRS)],
                None
            ),
            format!(
                "{}{ONLY} It holds com.roomler.agent.plist.pre-one-row and \
                 config.toml.pre-one-row, which `sudo roomlerd one-row --revert` reads there \
                 and nowhere else. This line gives no command for it: an administrator of \
                 this Mac has to move it aside and copy those files back into a new folder \
                 that the person's account makes; {DOC}",
                cause(F)
            )
        );
        for name in ["", "p q", "p'q", "p$q", "p\"q", "p`q", "p/q", "p;q"] {
            let why = give_up_line_for("a name that is not plain", F, 0o40755, &[], Some(name));
            assert_eq!(
                why,
                format!(
                    "{}{ONLY} This line gives no command for it: an administrator of this Mac \
                     has to move it aside; {DOC}",
                    cause(F)
                ),
                "{name:?}"
            );
        }
        assert_eq!(
            give_up_line_for(
                "does not fit + one kept copy",
                F,
                0o40755,
                &[(0, THEIRS)],
                None
            ),
            format!(
                "{}{ONLY} It holds com.roomler.agent.plist.pre-one-row, which `sudo roomlerd \
                 one-row --revert` reads there and nowhere else. This line gives no command for \
                 it: an administrator of this Mac has to move it aside and copy that file back \
                 into a new folder that the person's account makes; {DOC}",
                cause(F)
            )
        );
        for name in ["p", "p.q", "p_q", "p-q", "P9"] {
            assert!(plain_account_name(name), "{name:?}");
        }
        let folder = FolderNotTheirs {
            administrator: Ok("p".to_string()),
            ..folder_not_theirs(F.into(), 501, 502, 0o40755, Vec::new(), STAMP).expect("502's")
        };
        let why = gave_up_reason(Some(WorkerEnd::Code(1)), true, Some(&folder));
        // Another account's folder: whether the person may rename it rests on
        // its mode, so the line claims nothing about that, and gives no
        // command (review N10).
        println!("GIVE-UP [a folder of uid 502] {why}");
        assert_eq!(
            why,
            format!(
                "the worker cannot write the person's folder {F}: uid 502 owns it (mode 755), \
                 not the person, so the worker's own log is not there either. It is that \
                 account's folder, so this line gives no command for it; {DOC}"
            )
        );
    }

    /// #3802 — when the line gives no administrator's command, the
    /// supervisor logs why, once: the folder's own read first, then the
    /// account read that said no, or the deadline. The words, exactly, and
    /// the line agrees: no command for each of them.
    #[test]
    fn the_log_names_why_the_line_gives_no_command() {
        let root = |administrator: Result<String, NoCommand>| FolderNotTheirs {
            administrator,
            ..folder_not_theirs(F.into(), 501, 0, 0o40755, Vec::new(), STAMP).expect("root's")
        };
        let p = || root(Ok("p".to_string()));
        assert_eq!(administrator_command(&p()), Ok("p"));
        let unreadable = vec![KeptCopy {
            name: kept_for_revert()[1].clone(),
            stat: ROOTS_600,
        }];
        let os_error = "No such file or directory (os error 2)".to_string();
        let cases = [
            (
                FolderNotTheirs {
                    is_dir: false,
                    ..p()
                },
                NoCommand::NotADirectory,
                "the folder's own lstat reads a link or a file, not a directory",
            ),
            (
                FolderNotTheirs {
                    kept: unreadable,
                    ..p()
                },
                NoCommand::UnreadableCopy,
                "the folder holds a kept copy that the person cannot read",
            ),
            (
                FolderNotTheirs { owner: 502, ..p() },
                NoCommand::NotRoots(502),
                "uid 502 owns the folder, not root",
            ),
            (
                FolderNotTheirs {
                    holds_config: true,
                    ..p()
                },
                NoCommand::HoldsConfig,
                "the folder holds config.toml",
            ),
            (
                FolderNotTheirs {
                    path: "/Users/p$q/Library/Application Support/x".into(),
                    ..p()
                },
                NoCommand::PathNotPlain,
                "the folder's path holds a double quote, `$`, a backtick, a backslash, `!`, a \
                 control character or a Unicode format character that changes how a line \
                 displays",
            ),
            (
                root(Ok("p q".to_string())),
                NoCommand::NameNotPlain,
                "the person's account name is not plain ([A-Za-z0-9._-]+)",
            ),
            (
                root(Err(NoCommand::NotRead)),
                NoCommand::NotRead,
                "the account reads have not run",
            ),
            (
                root(Err(NoCommand::NoAccount)),
                NoCommand::NoAccount,
                "the account lookup (getpwuid_r) found no entry with a UTF-8 name for the \
                 person's uid",
            ),
            (
                root(Err(NoCommand::PathsDoNotRead)),
                NoCommand::PathsDoNotRead,
                "the person's home is not a folder, or its Library or Library/Application \
                 Support is not a folder of their own (a link, another account's, or unread), \
                 or the folder is not one name inside it",
            ),
            (
                root(Err(NoCommand::LookupFailed)),
                NoCommand::LookupFailed,
                "the account lookup's task ended without an answer",
            ),
            (
                root(Err(NoCommand::AdminCheckDidNotRun(os_error.clone()))),
                NoCommand::AdminCheckDidNotRun(os_error),
                "dseditgroup could not run: No such file or directory (os error 2)",
            ),
            (
                root(Err(NoCommand::NotAdministrator(Some(67)))),
                NoCommand::NotAdministrator(Some(67)),
                "dseditgroup does not confirm that the person is a member of admin (exit 67)",
            ),
            (
                root(Err(NoCommand::NotAdministrator(None))),
                NoCommand::NotAdministrator(None),
                "dseditgroup does not confirm that the person is a member of admin (ended by a \
                 signal)",
            ),
            (
                root(Err(NoCommand::TimedOut)),
                NoCommand::TimedOut,
                "the reads did not answer within 5 s",
            ),
        ];
        for (f, no, why) in cases {
            assert_eq!(administrator_command(&f), Err(no.clone()), "{f:?}");
            assert_eq!(no.why(), why);
            let line = gave_up_reason(Some(WorkerEnd::Code(1)), true, Some(&f));
            assert!(!line.contains("can run this"), "{f:?}: {line}");
        }
    }

    /// #3802 — the paths half of the package's `sudo_line_fits`, with
    /// `lstat` injected: the home an absolute path to a folder, not a link;
    /// its `Library` and `Library/Application Support` folders of the
    /// person's own, not links; the folder one name inside the last. Any
    /// other read, or a read that fails, is a no.
    #[test]
    fn the_administrators_command_needs_paths_that_read_as_the_folder() {
        use std::collections::HashMap;
        const DIR: u32 = 0o40755;
        const LINK: u32 = 0o120755;
        let home = "/Users/p";
        let library = "/Users/p/Library";
        let support = "/Users/p/Library/Application Support";
        let fs = |over: &[(&str, Option<(u32, u32)>)]| {
            let mut fs: HashMap<String, Option<(u32, u32)>> = [
                (home, Some((501, DIR))),
                (library, Some((501, DIR))),
                (support, Some((501, DIR))),
            ]
            .into_iter()
            .map(|(p, s)| (p.to_string(), s))
            .collect();
            for (p, s) in over {
                fs.insert(p.to_string(), *s);
            }
            fs
        };
        let fits = |home: &str, folder: &str, fs: &HashMap<String, Option<(u32, u32)>>| {
            path_reads_as_the_folder(501, home.as_ref(), folder.as_ref(), |p| {
                let p = p.to_str().expect("utf-8").to_string();
                fs.get(&p)
                    .copied()
                    .flatten()
                    .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            })
        };
        assert!(fits(home, F, &fs(&[])));
        // The home: a link, a file, not absolute, ending in `/`, unread. A
        // home of root's is still a home (the package does not ask).
        for over in [
            (home, Some((501, LINK))),
            (home, Some((501, 0o100644))),
            (home, None),
        ] {
            assert!(!fits(home, F, &fs(&[over])), "{over:?}");
        }
        assert!(fits(home, F, &fs(&[(home, Some((0, DIR)))])));
        assert!(!fits(
            "Users/p",
            "Users/p/Library/Application Support/x",
            &fs(&[])
        ));
        assert!(!fits("/Users/p/", F, &fs(&[])));
        // `Library` and `Application Support`: a link, another account's, a
        // file, unread.
        for p in [library, support] {
            for s in [
                Some((501, LINK)),
                Some((0, DIR)),
                Some((502, DIR)),
                Some((501, 0o100644)),
                None,
            ] {
                assert!(!fits(home, F, &fs(&[(p, s)])), "{p}: {s:?}");
            }
        }
        // The folder is one name inside `Application Support` of that home.
        for folder in [
            "/Users/q/Library/Application Support/live.roomler.roomler",
            "/Users/p/Library/Application Support",
            "/Users/p/Library/Application Support/",
            "/Users/p/Library/Application Support/x/live.roomler.roomler",
            "/Users/p/Library/Application Supportx/live.roomler.roomler",
        ] {
            assert!(!fits(home, folder, &fs(&[])), "{folder}");
        }
    }

    /// #3802 — a link or a file where the folder should be: the cause only,
    /// and no command at all.
    #[test]
    fn the_give_up_line_for_a_folder_that_is_not_a_directory() {
        for (case, mode) in [
            ("not a directory: a link", 0o120755),
            ("not a directory: a file", 0o100755),
        ] {
            assert_eq!(
                give_up_line(case, F, mode, &[]),
                format!(
                    "{}It is not a folder but a link or a file, so this line gives no command \
                     for it.",
                    cause(F)
                )
            );
        }
    }

    /// #3802 — the read that fills the kept copies, with `lstat` injected: a
    /// name it reads is held with what it read, "not there" is not held, and
    /// any other error is held unread (the `cp` reads it before anything
    /// moves). A folder the person owns, or one it cannot read, is no answer.
    #[test]
    fn the_read_holds_what_lstat_finds_in_a_folder_of_roots() {
        let [plist, config] = kept_for_revert();
        let dir = std::path::PathBuf::from(F);
        let (p, c) = (dir.join(&plist), dir.join(&config));
        let lstat = |path: &std::path::Path| -> std::io::Result<(u32, u32)> {
            if path == dir.as_path() {
                Ok((0, 0o40755))
            } else if path == p.as_path() {
                Ok((501, 0o100644))
            } else if path == c.as_path() {
                Err(std::io::ErrorKind::PermissionDenied.into())
            } else {
                Err(std::io::ErrorKind::NotFound.into())
            }
        };
        let f = read_person_folder(dir.clone(), 501, STAMP, lstat).expect("root's");
        assert_eq!(
            f.kept,
            vec![
                KeptCopy {
                    name: plist,
                    stat: Some((501, 0o100644))
                },
                KeptCopy {
                    name: config,
                    stat: None
                },
            ]
        );
        assert_eq!(
            (f.uid, f.owner, f.mode, f.is_dir, f.stamp.as_str()),
            (501, 0, 0o755, true, STAMP)
        );
        let empty = |path: &std::path::Path| -> std::io::Result<(u32, u32)> {
            if path == dir.as_path() {
                Ok((0, 0o40755))
            } else {
                Err(std::io::ErrorKind::NotFound.into())
            }
        };
        let f = read_person_folder(dir.clone(), 501, STAMP, empty).expect("root's");
        assert_eq!(f.kept, Vec::new());
        assert_eq!(
            read_person_folder(dir.clone(), 501, STAMP, |_| Ok((501, 0o40755))),
            None
        );
        assert_eq!(
            read_person_folder(
                dir,
                501,
                STAMP,
                |_| Err(std::io::ErrorKind::NotFound.into())
            ),
            None
        );
    }

    /// #3802 — a link or a file at the folder is read as itself and nothing
    /// is read through it, so no read goes past a link at the folder.
    #[test]
    fn the_read_never_goes_through_a_link_at_the_folder() {
        let dir = std::path::PathBuf::from(F);
        for mode in [0o120755, 0o100644] {
            let asked = std::cell::RefCell::new(Vec::new());
            let lstat = |path: &std::path::Path| -> std::io::Result<(u32, u32)> {
                asked.borrow_mut().push(path.to_path_buf());
                Ok(if path == dir.as_path() {
                    (0, mode)
                } else {
                    (501, 0o100644)
                })
            };
            let f = read_person_folder(dir.clone(), 501, STAMP, lstat).expect("root's");
            assert_eq!(asked.into_inner(), vec![dir.clone()], "{mode:o}");
            assert!(!f.is_dir && f.kept.is_empty(), "{mode:o}: {f:?}");
        }
    }

    /// #3802 — every command printed survives `sh -c "…"`: a folder whose
    /// path holds a double quote, `$`, a backtick, a backslash, `!` or a
    /// control character gets none, where every read said yes. An
    /// apostrophe is quoted, so it keeps its command.
    #[test]
    fn the_give_up_line_gives_no_command_for_a_path_a_shell_reads_otherwise() {
        let line = |path: &str| {
            let folder = FolderNotTheirs {
                administrator: Ok("p".to_string()),
                ..folder_not_theirs(path.into(), 501, 0, 0o40755, Vec::new(), STAMP)
                    .expect("root's")
            };
            gave_up_reason(Some(WorkerEnd::Code(1)), true, Some(&folder))
        };
        for c in ['"', '$', '`', '\\', '!', '\n', '\t', '\u{7f}'] {
            let path = format!("/Users/p{c}q/Library/Application Support/live.roomler.roomler");
            assert!(!path_survives_double_quotes(&path), "{c:?}");
            assert_eq!(
                line(&path),
                format!(
                    "{}{ONLY} This line gives no command for it: an administrator of this Mac \
                     has to move it aside; {DOC}",
                    cause(&path)
                ),
                "{c:?}"
            );
        }
        let dollar = "/Users/p$q/Library/Application Support/live.roomler.roomler";
        give_up_line_for("fits + a $ in the path", dollar, 0o40755, &[], Some("p"));
        let apostrophe = "/Users/o'brien/Library/Application Support/live.roomler.roomler";
        assert!(path_survives_double_quotes(F) && path_survives_double_quotes(apostrophe));
        assert!(line(apostrophe).contains("can run this"));
    }

    /// #3802 — every command printed reads as it runs: a folder whose path
    /// holds a Unicode format character that changes how a line displays
    /// (U+200B to U+200F, U+202A to U+202E, U+2060 to U+2069, U+FEFF) gets
    /// none, where every read said yes. None of them is a control character,
    /// so the shell's check alone lets each through. Each end of each range is
    /// refused; visible characters beside the ranges are not.
    #[test]
    fn the_give_up_line_gives_no_command_for_a_path_that_displays_otherwise() {
        let path =
            |c: char| format!("/Users/p{c}q/Library/Application Support/live.roomler.roomler");
        for c in [
            '\u{200B}', '\u{200D}', '\u{200F}', '\u{202A}', '\u{202C}', '\u{202E}', '\u{2060}',
            '\u{2066}', '\u{2069}', '\u{FEFF}',
        ] {
            let path = path(c);
            assert!(path_survives_double_quotes(&path), "{c:?}");
            assert!(!path_displays_as_it_runs(&path), "{c:?}");
            let folder = FolderNotTheirs {
                administrator: Ok("p".to_string()),
                ..folder_not_theirs(path.as_str().into(), 501, 0, 0o40755, Vec::new(), STAMP)
                    .expect("root's")
            };
            assert_eq!(
                administrator_command(&folder),
                Err(NoCommand::PathNotPlain),
                "{c:?}"
            );
            assert_eq!(
                gave_up_reason(Some(WorkerEnd::Code(1)), true, Some(&folder)),
                format!(
                    "{}{ONLY} This line gives no command for it: an administrator of this Mac \
                     has to move it aside; {DOC}",
                    cause(&path)
                ),
                "{c:?}"
            );
        }
        for c in [
            '\u{2010}', '\u{2030}', '\u{205E}', '\u{2070}', '\u{FF01}', 'é',
        ] {
            assert!(path_displays_as_it_runs(&path(c)), "{c:?}");
        }
        assert!(path_displays_as_it_runs(F));
    }

    /// #3802 — the read takes a `config.toml` in a folder of root's as it
    /// takes the kept copies, with one `lstat`: "not there" is not held, and
    /// any other error is held, so no command moves a config it could not
    /// rule out.
    #[test]
    fn the_read_takes_a_config_in_the_folder() {
        let dir = std::path::PathBuf::from(F);
        let config = dir.join("config.toml");
        for (read, holds) in [
            (Ok((0, 0o100600)), true),
            (Err(std::io::ErrorKind::PermissionDenied), true),
            (Err(std::io::ErrorKind::NotFound), false),
        ] {
            let lstat = |path: &std::path::Path| -> std::io::Result<(u32, u32)> {
                if path == dir.as_path() {
                    Ok((0, 0o40755))
                } else if path == config.as_path() {
                    read.map_err(std::io::Error::from)
                } else {
                    Err(std::io::ErrorKind::NotFound.into())
                }
            };
            let f = read_person_folder(dir.clone(), 501, STAMP, lstat).expect("root's");
            assert_eq!(f.holds_config, holds, "{read:?}");
        }
    }

    /// #3802 — a folder of root's that holds an entry named `config.toml`
    /// (a file, or a link, a folder or a name whose read failed): a move
    /// would set it aside with the folder, and with `macos_one_row` off a
    /// file of that name is the person's enrollment, so the line names it,
    /// still names the copies the revert reads there, and gives no command,
    /// even where every read said yes.
    #[test]
    fn the_give_up_line_gives_no_command_for_a_folder_that_holds_a_config() {
        let names = kept_for_revert();
        let entry = "holds an entry named config.toml, and a move would set that entry aside \
                     with the folder (with `macos_one_row` off, a file of that name is this \
                     person's enrollment), so this line gives no command to move it;";
        for (case, kept, want) in [
            (
                "fits + config.toml in the folder",
                &[][..],
                format!("{}{ONLY} It {entry} {DOC}", cause(F)),
            ),
            (
                "fits + config.toml + the plist copy",
                &[0][..],
                format!(
                    "{}{ONLY} It holds com.roomler.agent.plist.pre-one-row, which `sudo roomlerd \
                     one-row --revert` reads there and nowhere else. It also {entry} {DOC}",
                    cause(F)
                ),
            ),
            (
                "fits + config.toml + both kept copies",
                &[0, 1][..],
                format!(
                    "{}{ONLY} It holds {PLIST} and {CONFIG}, which `sudo roomlerd one-row \
                     --revert` reads there and nowhere else. It also {entry} {DOC}",
                    cause(F)
                ),
            ),
        ] {
            let kept = kept
                .iter()
                .map(|i: &usize| KeptCopy {
                    name: names[*i].clone(),
                    stat: THEIRS,
                })
                .collect();
            let folder = FolderNotTheirs {
                holds_config: true,
                administrator: Ok("p".to_string()),
                ..folder_not_theirs(F.into(), 501, 0, 0o40755, kept, STAMP).expect("root's")
            };
            let why = gave_up_reason(Some(WorkerEnd::Code(1)), true, Some(&folder));
            println!("GIVE-UP [{case}] {why}");
            assert_eq!(why, want, "{case}");
        }
    }

    /// #3802 — the names the give-up keeps are the paths the revert reads in
    /// the person's folder (`one_row::revert_plan`).
    #[test]
    fn the_copies_the_give_up_keeps_are_the_ones_the_revert_reads() {
        let dir = std::path::PathBuf::from("/u/F");
        let person = crate::one_row::Person {
            user: "p".into(),
            uid: 501,
            home: "/u".into(),
            dir: dir.clone(),
            config: None,
            has_plist: true,
        };
        let [plist, config] = kept_for_revert();
        assert_eq!(dir.join(plist), person.plist_aside());
        assert_eq!(
            dir.join(config),
            crate::one_row::kept_path(&person.config_path())
        );
    }

    /// FR-43 P3a — a worker with no enrollment cannot be missing one, so the
    /// give-up does not send a person after a per-user config. A held lock reads
    /// the same under either.
    #[test]
    fn the_give_up_does_not_blame_an_enrollment_a_worker_never_needed() {
        for end in [Some(WorkerEnd::Code(1)), Some(WorkerEnd::Signal(9)), None] {
            assert!(
                !gave_up_reason(end, true, None).contains("per-user enrollment"),
                "{end:?}"
            );
        }
        assert_eq!(
            gave_up_reason(Some(WorkerEnd::Code(0)), true, None),
            gave_up_reason(Some(WorkerEnd::Code(0)), false, None)
        );
    }
}

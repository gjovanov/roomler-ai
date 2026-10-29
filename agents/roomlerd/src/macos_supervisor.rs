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

/// #2987 — a supervised worker's command line, as `ps` prints it: the argv
/// `spawn_worker` builds (`/usr/bin/env` execs into the agent, so it is not
/// part of it).
pub fn worker_command(exe: &str) -> String {
    format!("{exe} run --supervised")
}

/// #2987 — the workers a previous daemon left behind, out of
/// `ps -axww -o pid= -o uid= -o command=`: every process that runs exactly
/// `command` as a person (uid ≠ 0), other than `own_pid`. Returns
/// `(pid, uid)` pairs.
///
/// Exact match only, because each match gets a SIGTERM from root.
/// `--supervised` appears on no command line except the one `spawn_worker`
/// builds, so a hand-run `roomlerd run`, launchd's own worker (`… run`) and
/// the daemon (`… --config … run`) never match.
pub fn stale_workers(ps: &str, command: &str, own_pid: u32) -> Vec<(u32, u32)> {
    ps.lines()
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(char::is_whitespace)?;
            let (uid, cmd) = rest.trim_start().split_once(char::is_whitespace)?;
            let pid: u32 = pid.parse().ok()?;
            let uid: u32 = uid.parse().ok()?;
            (cmd.trim() == command && uid != 0 && pid != own_pid).then_some((pid, uid))
        })
        .collect()
}

/// #2987 — why the supervisor stopped spawning, read off how the last worker
/// ended. On a Mac, a worker that finds another roomlerd holding this
/// person's single-instance lock exits **0** (`already_running_exit`: 0 is
/// launchd's "don't relaunch me"). A missing per-user enrollment ends in an
/// error, which is code 1. Before #2987 the give-up named only the second, so
/// the first was misreported as an enrollment problem.
pub fn gave_up_reason(last: Option<WorkerEnd>) -> &'static str {
    match last {
        Some(WorkerEnd::Code(0)) => {
            "each worker exited 0 at once: another roomlerd holds this person's \
             single-instance lock, and it is not a worker this supervisor can identify \
             (a hand-run `roomlerd run`, say). Find it with: pgrep -lU <uid> roomlerd. \
             Once it is gone, restart this daemon: \
             launchctl kickstart -k system/com.roomler.daemon"
        }
        _ => {
            "almost always this Mac has no per-user enrollment: the worker runs AS the \
             console user and loads THAT user's config. Check with: \
             launchctl asuser <uid> sudo -u '#<uid>' roomlerd caps"
        }
    }
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
    fn spawn_worker(uid: u32, exe: &std::path::Path, secret: &str) -> std::io::Result<Child> {
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
            .arg("run")
            // FR-43 P2a — tell the worker it is ours, and that its
            // delegation-channel secret is waiting on stdin.
            //
            // ⚠️ This REPLACES the `ROOMLER_MACOS_SUPERVISED` environment
            // marker P1 shipped, which never arrived at a single worker:
            // `sudo` runs under the stock `Defaults env_reset` and discards the
            // parent's environment. Measured on the MacBook 2026-08-31 —
            // `ps -E` on a live supervised worker showed ZERO `ROOMLER_MACOS*`
            // variables. Nothing had noticed because nothing read the marker
            // yet, which is precisely what made it worth checking before
            // building on it.
            .arg("--supervised")
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
        let command = worker_command(exe);
        let stale = tokio::task::spawn_blocking(move || find_processes(&command))
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
    fn find_processes(command: &str) -> Vec<(u32, u32)> {
        // One `-o` per column: BSD `ps` lets only the LAST keyword of an `-o`
        // carry `=`, and `pid=,uid=` would make `,uid=` pid's header.
        let mut ps = Command::new("/bin/ps");
        ps.args(["-axww", "-o", "pid=", "-o", "uid=", "-o", "command="]);
        find_processes_via(&mut ps, command)
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
    fn find_processes_via(ps: &mut Command, command: &str) -> Vec<(u32, u32)> {
        match tunnel_core::child::output_bounded(ps, PS_DEADLINE) {
            Ok(out) if out.status.success() => stale_workers(
                &String::from_utf8_lossy(&out.stdout),
                command,
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
                    tracing::error!(
                        console_uid = ?uid_now,
                        fast_exits,
                        max = MAX_FAST_EXITS,
                        last_end = ?last_end,
                        "macOS supervisor: the GUI worker died on arrival too many times — \
                         NOT retrying: {}",
                        gave_up_reason(last_end)
                    );
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
                        match spawn_worker(uid, &exe, &secret) {
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

            tokio::select! {
                _ = tokio::time::sleep(POLL) => {}
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
                found = find_processes(&command);
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
                find_processes(&command).is_empty(),
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
            let command = worker_command("/Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd");
            let started = Instant::now();
            let found = find_processes_via(
                &mut pinned_ps(&pid_file, &format!("  4242   501 {command}")),
                &command,
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
        let command = worker_command(EXE);
        assert_eq!(command, format!("{EXE} run --supervised"));
        assert_eq!(
            stale_workers(PS, &command, 412),
            vec![(68123, 501), (70005, 502)]
        );
        // Never this process.
        assert_eq!(stale_workers(PS, &command, 70005), vec![(68123, 501)]);
        assert!(stale_workers("", &command, 412).is_empty());
        assert!(stale_workers("  PID   UID COMMAND\nnot a row\n", &command, 412).is_empty());
    }

    #[test]
    fn an_install_path_with_spaces_still_matches_exactly() {
        let exe = "/Applications/Roomler Beta.app/Contents/MacOS/roomlerd";
        let ps = format!("  555   501 {exe} run --supervised\n  556   501 {exe} run\n");
        assert_eq!(
            stale_workers(&ps, &worker_command(exe), 1),
            vec![(555, 501)]
        );
    }

    /// The 10-26 MacBook gave up naming "no per-user enrollment" while every
    /// worker had exited 0 on a held lock. The exit code tells them apart.
    #[test]
    fn the_give_up_names_the_lock_when_workers_exit_cleanly() {
        let lock = gave_up_reason(Some(WorkerEnd::Code(0)));
        assert!(lock.contains("single-instance lock"), "{lock}");
        assert!(!lock.contains("enrollment"), "{lock}");
        for end in [Some(WorkerEnd::Code(1)), Some(WorkerEnd::Signal(9)), None] {
            assert!(
                gave_up_reason(end).contains("per-user enrollment"),
                "{end:?}"
            );
        }
    }
}

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
/// Backoff ceiling for respawns.
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

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
    /// launchd owns this session's agent; stand down (and drop ours if the
    /// LaunchAgent came back while we were supervising).
    LaunchdOwns,
    /// Spawn a worker for this uid.
    Spawn(u32),
    /// The console user changed under a worker we own: replace it.
    Replace(u32),
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
        return Action::LaunchdOwns;
    }
    match i.worker_uid {
        Some(w) if w == uid => Action::Healthy,
        Some(_) => Action::Replace(uid),
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

#[cfg(target_os = "macos")]
pub use imp::run;

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;

    /// The LaunchAgent label the postinstall bootstraps. If this is loaded,
    /// we stand down — see the module docs.
    const AGENT_LABEL: &str = "com.roomler.agent";

    /// Marks a worker as OURS, so a later phase can tell a supervised worker
    /// from a launchd-owned one without guessing from the process tree.
    const SUPERVISED_MARKER: &str = "ROOMLER_MACOS_SUPERVISED";

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

    /// Spawn the agent into `uid`'s GUI session.
    ///
    /// No `--config`: the child runs AS that user, so `appdirs` resolves the
    /// user's own config — the same enrollment the LaunchAgent would have
    /// used. P1 changes who launches the worker, nothing about its identity.
    fn spawn_worker(uid: u32, exe: &std::path::Path) -> std::io::Result<Child> {
        Command::new("launchctl")
            .arg("asuser")
            .arg(uid.to_string())
            .arg(exe)
            .arg("run")
            .env(SUPERVISED_MARKER, "1")
            .spawn()
    }

    /// Stop a worker we own. Best effort by construction: `launchctl asuser`
    /// re-parents the agent into the user's bootstrap, so killing our direct
    /// child does not guarantee the grandchild dies. P1 accepts that (the
    /// only paths here are shutdown and a session change, both of which the
    /// worker itself also notices); P2 gets a real handshake over LocalAPI.
    fn stop_worker(uid: u32, mut child: Child, why: &str) {
        tracing::info!(uid, why, "macOS supervisor: stopping our GUI worker");
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Supervise for the process lifetime. Never returns an error to the
    /// caller: a supervisor that can take the daemon down with it would trade
    /// a missing remote-desktop half for a missing mesh — a worse failure
    /// than the one it exists to fix.
    pub async fn run(enabled: bool, mut shutdown: tokio::sync::watch::Receiver<bool>) {
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
        let mut wait_until: Option<Instant> = None;
        let mut last_action: Option<Action> = None;

        loop {
            // Reap first, so `worker_uid` reflects reality this tick.
            //
            // The borrow of `worker` has to END before the assignment below —
            // hence the map/filter dance rather than mutating inside an
            // `if let ... = worker.as_mut()` body. (Windows never compiles
            // this module, so only the macOS lane would have caught it.)
            let exited = worker.as_mut().and_then(|(uid, child, started)| {
                matches!(child.try_wait(), Ok(Some(_))).then(|| (*uid, started.elapsed()))
            });
            if let Some((uid, ran)) = exited {
                backoff = next_backoff(backoff, ran);
                tracing::warn!(
                    uid,
                    ran_secs = ran.as_secs(),
                    retry_in_secs = backoff.as_secs(),
                    "macOS supervisor: GUI worker exited"
                );
                wait_until = Some(Instant::now() + backoff);
                worker = None;
            }

            let uid_now = console_uid();
            let action = decide(Inputs {
                enabled: true,
                is_root: true,
                console_uid: uid_now,
                // Only ask launchd when there is a session to ask about.
                launch_agent_loaded: uid_now.is_some_and(launch_agent_loaded),
                worker_uid: worker.as_ref().map(|(u, _, _)| *u),
            });

            // Log transitions, not every tick: this loop runs every 5 s for
            // the daemon's whole life.
            if last_action != Some(action) {
                tracing::info!(?action, console_uid = ?uid_now, "macOS supervisor state");
                last_action = Some(action);
            }

            match action {
                Action::Healthy | Action::Disabled => {}
                Action::NoGuiSession | Action::LaunchdOwns => {
                    if let Some((uid, child, _)) = worker.take() {
                        stop_worker(uid, child, "session ended or launchd took ownership");
                    }
                }
                Action::Replace(uid) => {
                    if let Some((old, child, _)) = worker.take() {
                        tracing::info!(
                            old_uid = old,
                            new_uid = uid,
                            "macOS supervisor: console user changed"
                        );
                        stop_worker(old, child, "console user changed");
                    }
                    // Spawn on the next tick, through the normal Spawn arm.
                }
                Action::Spawn(uid) => {
                    if wait_until.is_none_or(|t| Instant::now() >= t) {
                        match spawn_worker(uid, &exe) {
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
                            stop_worker(uid, child, "daemon shutting down");
                        }
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Inputs {
        Inputs {
            enabled: true,
            is_root: true,
            console_uid: Some(501),
            launch_agent_loaded: false,
            worker_uid: None,
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
        // the plist back, and the loser of that race must be us.
        assert_eq!(
            decide(Inputs {
                launch_agent_loaded: true,
                worker_uid: Some(501),
                ..base()
            }),
            Action::LaunchdOwns
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
}

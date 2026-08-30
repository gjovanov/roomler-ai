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
/// How many consecutive fast exits before the supervisor stops trying. The
/// 2026-08-30 outage looped because nothing bounded this: a worker that can
/// NEVER start is a configuration fault, and hammering it hides the cause.
pub const MAX_FAST_EXITS: u32 = 5;

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

#[cfg(target_os = "macos")]
pub use imp::run;

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::process::CommandExt;
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
    fn spawn_worker(uid: u32, exe: &std::path::Path) -> std::io::Result<Child> {
        Command::new("launchctl")
            .arg("asuser")
            .arg(uid.to_string())
            .arg("sudo")
            .arg("-u")
            .arg(format!("#{uid}"))
            .arg(exe)
            .arg("run")
            .env(SUPERVISED_MARKER, "1")
            // Own process group, so `stop_worker` can take the whole
            // launchctl -> sudo -> agent chain down instead of orphaning it.
            .process_group(0)
            .spawn()
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
    /// A failure here is logged, not retried: the next poll sees
    /// [`Action::HandBack`] again for as long as we still hold a worker, and
    /// once we do not, launchd's own supervision is the right thing to trust.
    fn kickstart_launch_agent(uid: u32) {
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
                    target,
                    "macOS supervisor: handed the worker back to launchd"
                );
            }
            Ok(s) => {
                tracing::warn!(
                    uid,
                    job,
                    status = ?s.code(),
                    "macOS supervisor: could not kickstart the LaunchAgent — \
                     this session may be left with no user half until launchd \
                     starts one"
                );
            }
            Err(e) => {
                tracing::warn!(
                    uid,
                    job,
                    error = %e,
                    "macOS supervisor: could not run launchctl kickstart"
                );
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
    fn stop_worker(uid: u32, mut child: Child, why: &str) {
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
        let mut fast_exits: u32 = 0;
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
                fast_exits = if ran >= HEALTHY_RUN {
                    0
                } else {
                    fast_exits + 1
                };
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
                        "macOS supervisor: the GUI worker died on arrival too many times — \
                         NOT retrying. Almost always this Mac has no per-user enrollment: \
                         the worker runs AS the console user and loads THAT user's config. \
                         Check with: launchctl asuser <uid> sudo -u '#<uid>' roomlerd caps"
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
                Action::NoGuiSession | Action::LaunchdOwns => {
                    fast_exits = 0;
                    if let Some((uid, child, _)) = worker.take() {
                        stop_worker(uid, child, "GUI session ended");
                    }
                }
                Action::HandBack(uid) => {
                    fast_exits = 0;
                    if let Some((held, child, _)) = worker.take() {
                        stop_worker(held, child, "launchd took ownership");
                    }
                    kickstart_launch_agent(uid);
                }
                Action::Replace(uid) => {
                    fast_exits = 0;
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
}

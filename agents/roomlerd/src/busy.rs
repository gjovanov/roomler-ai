// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! What this daemon serves right now that restarting it would cut, or could
//! wedge: ONE model for both things that take the daemon down by themselves
//! (#3429, from FR-96 P2b).
//!
//! - **The periodic update check** ([`crate::updater::run_periodic`], every
//!   OS, and its early run after a long control-socket outage, #4245)
//!   defers while any of it is live: a recheck every hour, forced after
//!   [`MAX_CONSECUTIVE_DEFERS`](crate::updater::MAX_CONSECUTIVE_DEFERS)
//!   defers in a row (`updater::periodic_gate`). Only an operator's push
//!   skips it, and its warning names what it cuts.
//! - **The replaced-binary restart** ([`crate::replaced_binary`], a per-user
//!   Linux daemon) waits for it up to its 24 h ceiling.
//!
//! **Cut**: a remote-control session, an SSH connection, a Fleet RPC `exec`
//! run, a file transfer or a recording. **Wedged**: a network transition in
//! the last 10 min (R5b), an install of this daemon's own in flight.
//!
//! ⚠️ Only what THIS process serves. The Mac's root update helper reads it
//! from each half it restarts, over the LocalAPI's `live_work`
//! ([`crate::helper_gate`], FR-151); a per-user Linux install's helper
//! installs without reading it, and a command a Windows service host runs
//! for its worker (FR-94) is not counted in the worker.

use std::sync::atomic::Ordering;

use crate::power::{ActivityCounter, ActivityGuard};

/// What a restart would cut, or wedge, right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Busy {
    pub rc_sessions: usize,
    pub ssh_connections: usize,
    /// Fleet RPC commands running in this process (`exec_run`). A command
    /// of an SSH session is its connection's, counted there once.
    pub exec_runs: usize,
    /// File transfers in flight, plus a screen recording
    /// (`updater::active_work`).
    pub work: usize,
    /// The network moved in the last 10 min (R5b: a restart beside a VPN
    /// transition can lock the machine out).
    pub net_transition: bool,
    /// This daemon spawned an installer in the last 10 min: the in-process
    /// path restarts the daemon itself, and a restart now would kill the
    /// installer it is waiting on.
    pub install_in_flight: bool,
}

impl Busy {
    /// Nothing live.
    pub const IDLE: Busy = Busy {
        rc_sessions: 0,
        ssh_connections: 0,
        exec_runs: 0,
        work: 0,
        net_transition: false,
        install_in_flight: false,
    };

    /// How much work a restart would CUT: the update check's `active`.
    pub fn cuttable_count(&self) -> usize {
        self.rc_sessions + self.ssh_connections + self.exec_runs + self.work
    }

    /// Work a restart would CUT: cut after the replaced-binary watch's
    /// ceiling, and after the update check's defer budget.
    pub fn cuttable(&self) -> bool {
        self.cuttable_count() > 0
    }

    /// States a restart could WEDGE, which the replaced-binary watch never
    /// forces over. The update check reads only the transition, and defers
    /// for it on its own budget (R5b).
    pub fn unforceable(&self) -> bool {
        self.net_transition || self.install_in_flight
    }
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        if self.rc_sessions > 0 {
            parts.push(format!("{} remote-control session(s)", self.rc_sessions));
        }
        if self.ssh_connections > 0 {
            parts.push(format!("{} SSH connection(s)", self.ssh_connections));
        }
        if self.exec_runs > 0 {
            parts.push(format!("{} exec run(s)", self.exec_runs));
        }
        if self.work > 0 {
            parts.push(format!("{} transfer(s) or recording", self.work));
        }
        if self.net_transition {
            parts.push("the network moved in the last 10 min".into());
        }
        if self.install_in_flight {
            parts.push("an install of this daemon's own is in flight".into());
        }
        if parts.is_empty() {
            f.write_str("nothing")
        } else {
            f.write_str(&parts.join(", "))
        }
    }
}

/// What a restart would cut or wedge, now. `rc_sessions` is the daemon's own
/// registry, the one `run_cmd` makes and hands to each reader.
pub fn now(rc_sessions: &crate::rc_sessions::RcSessionRegistry) -> Busy {
    Busy {
        rc_sessions: rc_sessions.list().len(),
        ssh_connections: ssh_connections(),
        exec_runs: exec_runs(),
        work: crate::updater::active_work(),
        net_transition: crate::updater::net_transition_recent(),
        install_in_flight: crate::updater::update_in_flight(),
    }
}

/// Open SSH connections; none in a build without an overlay to serve on.
fn ssh_connections() -> usize {
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    {
        crate::ssh::live_connections()
    }
    #[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
    {
        0
    }
}

/// Count `req` as an exec run for as long as the returned guard lives, if it
/// is one: a Fleet RPC command. A command of a Roomler SSH session
/// ([`session_owned`](crate::exec::ExecRequest::session_owned)) is that
/// session's, already counted as its connection, so it is not counted twice.
/// The exec engine takes it for every run it starts; the run's keep-awake
/// hold is the engine's own, unchanged (`power::shared_activity`).
pub(crate) fn exec_run(req: &crate::exec::ExecRequest) -> Option<ActivityGuard> {
    (!req.session_owned).then(|| ActivityGuard::new(&exec_run_counter()))
}

/// Fleet RPC commands running in this process now.
pub(crate) fn exec_runs() -> usize {
    exec_run_counter().load(Ordering::Relaxed)
}

/// The count `exec_run` holds and `exec_runs` reads: the process's own,
/// or in a test the one this thread was handed (`EXEC_RUNS_HERE`).
fn exec_run_counter() -> ActivityCounter {
    #[cfg(test)]
    if let Some(here) = EXEC_RUNS_HERE.with(|h| h.borrow().clone()) {
        return here;
    }
    static RUNS: std::sync::OnceLock<ActivityCounter> = std::sync::OnceLock::new();
    RUNS.get_or_init(crate::power::new_activity_counter).clone()
}

#[cfg(test)]
thread_local! {
    /// `Some(count)` is `exec_run`'s and `exec_runs`'s count on this
    /// thread, as `capture::seat_guard::LIVE_HERE` answers `live`: a test
    /// reads no other test's commands, and its own reach no other test.
    pub(crate) static EXEC_RUNS_HERE: std::cell::RefCell<Option<ActivityCounter>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_says_what_it_would_cut() {
        assert_eq!(Busy::IDLE.to_string(), "nothing");
        assert_eq!(Busy::default(), Busy::IDLE);
        let b = Busy {
            rc_sessions: 1,
            ssh_connections: 2,
            exec_runs: 3,
            work: 1,
            net_transition: true,
            install_in_flight: true,
        };
        let s = b.to_string();
        for part in [
            "1 remote-control session",
            "2 SSH connection",
            "3 exec run",
            "1 transfer",
            "network moved",
            "install",
        ] {
            assert!(s.contains(part), "{s}");
        }
        assert_eq!(b.cuttable_count(), 7);
        assert!(b.cuttable() && b.unforceable());
        assert!(!Busy::IDLE.cuttable() && !Busy::IDLE.unforceable());
        // An exec run alone is work a restart cuts.
        let run = Busy {
            exec_runs: 1,
            ..Busy::IDLE
        };
        assert!(run.cuttable() && !run.unforceable());
        assert_eq!(run.to_string(), "1 exec run(s)");
    }

    fn command(session_owned: bool) -> crate::exec::ExecRequest {
        crate::exec::ExecRequest {
            request_id: "t3429".into(),
            shell: String::new(),
            command: "true".into(),
            timeout_ms: 1_000,
            max_output_bytes: 0,
            cwd: None,
            caller: "test".into(),
            run_as: crate::exec::RunAs::Daemon,
            start_in_home: false,
            session_owned,
        }
    }

    /// A Fleet RPC command counts while its guard lives; an SSH session's own
    /// command does not, since its connection is counted already. Never twice,
    /// and never under the other's name.
    #[test]
    fn an_exec_run_counts_only_a_fleet_rpc_command() {
        let runs = crate::power::new_activity_counter();
        EXEC_RUNS_HERE.with(|h| *h.borrow_mut() = Some(runs.clone()));
        let fleet = exec_run(&command(false));
        let fleet_counted = fleet.is_some();
        let fleet_live = exec_runs();
        let ssh = exec_run(&command(true));
        let with_ssh = exec_runs();
        drop(fleet);
        let after = exec_runs();
        EXEC_RUNS_HERE.with(|h| *h.borrow_mut() = None);
        assert!(fleet_counted, "a Fleet RPC command took no exec run");
        assert_eq!(fleet_live, 1);
        assert!(ssh.is_none(), "an SSH session's command took an exec run");
        assert_eq!(with_ssh, 1);
        assert_eq!(after, 0);
        assert_eq!(runs.load(Ordering::Relaxed), 0);
    }
}

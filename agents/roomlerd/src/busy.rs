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
//! installs without reading it. The worker's gate asks a Windows service host
//! for the commands it runs for the worker (FR-94): [`gate_now`], #4244.

use std::sync::atomic::Ordering;

use crate::power::{ActivityCounter, ActivityGuard};

/// What a restart would cut, or wedge, right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Busy {
    pub rc_sessions: usize,
    /// FR-156 P1 — the SSH connections whose client was heard within the
    /// liveness window ([`ssh_counted`]).
    pub ssh_connections: usize,
    /// FR-156 P1 — the tunnel flows that moved bytes, either way, within the
    /// liveness window ([`flows_counted`]).
    pub tunnel_flows: usize,
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
        tunnel_flows: 0,
        exec_runs: 0,
        work: 0,
        net_transition: false,
        install_in_flight: false,
    };

    /// How much work a restart would CUT: the update check's `active`.
    pub fn cuttable_count(&self) -> usize {
        self.rc_sessions + self.ssh_connections + self.tunnel_flows + self.exec_runs + self.work
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
        if self.tunnel_flows > 0 {
            parts.push(format!("{} tunnel flow(s)", self.tunnel_flows));
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
        tunnel_flows: flows_counted(
            tunnel_core::forward::flows(),
            tunnel_core::liveness::proof(),
        ),
        exec_runs: exec_runs(),
        work: crate::updater::active_work(),
        net_transition: crate::updater::net_transition_recent(),
        install_in_flight: crate::updater::update_in_flight(),
    }
}

/// FR-156 P1 — the SSH connections of `open` a restart would cut: those whose
/// client was heard within the liveness window
/// ([`tunnel_core::liveness::WINDOW`]); with `proof` off
/// (`ROOMLERD_LIVE_WORK_PROOF=0`), every open one, as before.
pub fn ssh_counted(open: &tunnel_core::liveness::LiveList, proof: bool) -> usize {
    if proof {
        open.heard_within(tunnel_core::liveness::WINDOW)
    } else {
        open.len()
    }
}

/// FR-156 P1 — the tunnel flows of `listed` a restart would cut: those that
/// moved bytes, either way, within the liveness window; with `proof` off,
/// none, as before.
pub fn flows_counted(listed: &tunnel_core::liveness::LiveList, proof: bool) -> usize {
    if proof {
        listed.heard_within(tunnel_core::liveness::WINDOW)
    } else {
        0
    }
}

/// #4244 — what the update gate reads now: [`now`], with the Fleet RPC runs
/// the device's SYSTEM service holds for this worker (FR-94), asked over its
/// doorbell ([`crate::exec_service::service_runs`]).
pub async fn gate_now(rc_sessions: &crate::rc_sessions::RcSessionRegistry) -> Busy {
    let service = crate::exec_service::service_runs().await;
    now(rc_sessions).with_service_runs(&service)
}

impl Busy {
    /// `self`, with the runs the device's SYSTEM service holds counted as
    /// exec runs (#4244).
    pub fn with_service_runs(self, service: &crate::exec_service::ServiceRuns) -> Busy {
        Busy {
            exec_runs: self.exec_runs.saturating_add(service.counted()),
            ..self
        }
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
            tunnel_flows: 0,
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

    /// FR-156 B1 — a tunnel flow that moved bytes in the window is work a
    /// restart cuts: `cuttable_count` includes it, `Display` names it, and a
    /// flow quiet for longer than the window is not counted. With the switch
    /// off, none count.
    #[tokio::test(start_paused = true)]
    async fn tunnel_flows_are_cut_work() {
        use tunnel_core::liveness::{Clock, LiveList};
        let flows = Busy {
            tunnel_flows: 2,
            ..Busy::IDLE
        };
        assert_eq!(flows.cuttable_count(), 2, "{flows}");
        assert!(flows.cuttable() && !flows.unforceable());
        assert_eq!(flows.to_string(), "2 tunnel flow(s)");

        let listed = LiveList::new();
        let moving = listed.join(Clock::tokio());
        let _quiet = listed.join(Clock::tokio());
        tokio::time::advance(std::time::Duration::from_secs(91)).await;
        moving.stamp().touch();
        assert_eq!(flows_counted(&listed, true), 1, "the flow that moved bytes");
        assert_eq!(flows_counted(&listed, false), 0, "the switch is off");
    }

    /// FR-156 — `ROOMLERD_LIVE_WORK_PROOF=0` gives back what was counted
    /// before: every open SSH connection, heard or not, and no tunnel flow.
    #[tokio::test(start_paused = true)]
    async fn with_the_switch_off_every_open_connection_counts_and_no_flow() {
        use tunnel_core::liveness::{Clock, LiveList, SWITCH};
        assert_eq!(
            format!("{}{SWITCH}", tunnel_core::env::PREFIXES[0]),
            "ROOMLERD_LIVE_WORK_PROOF"
        );
        assert!(!tunnel_core::env::flag_value(Some("0"), true));
        assert!(tunnel_core::env::flag_value(None, true), "on by default");
        let open = LiveList::new();
        let _heard = open.join(Clock::tokio());
        let _silent = open.join(Clock::tokio());
        tokio::time::advance(std::time::Duration::from_secs(600)).await;
        assert_eq!(ssh_counted(&open, false), 2);
        assert_eq!(flows_counted(&open, false), 0);
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

    // #4244 — a Fleet RPC run the SYSTEM service executes for the worker on
    // an anchored device (FR-94). The worker never hears its end: its gate
    // asks the service, over the doorbell's health exchange, what it holds.

    use crate::exec_service::{
        DoorbellReply, DoorbellRequest, HELD_AT_MOST, HeldRuns, RUNS_ASK_TIMEOUT, ServiceRuns,
        Unread,
    };
    use crate::updater::{
        DeferDecision, TRANSFER_DEFER_RECHECK, forced_update_warning, periodic_gate,
    };
    use std::time::Instant;

    /// The worker's own `Busy` with nothing of its own live. `IDLE`, not a
    /// read of `now`: another test's SSH connection is process-wide, and the
    /// cells below are about the service's runs alone.
    fn own() -> Busy {
        Busy::IDLE
    }

    /// One health exchange over an in-memory doorbell, with the worker's
    /// reading of it. The stand-in answers from `held` as the SYSTEM service's
    /// `health` does, or, `older`, as a service from before #4244 does: with
    /// no count.
    async fn service_says(held: &HeldRuns, older: bool, handed: usize) -> ServiceRuns {
        let (client, server) = tokio::io::duplex(8 * 1024);
        let exec_runs = (!older).then(|| held.reported_at(Instant::now()));
        let served = tokio::spawn(crate::exec_service::serve_connection(
            server,
            move |req| async move {
                match req {
                    DoorbellRequest::Health => DoorbellReply::Health {
                        anchored: true,
                        agent_id: Some("6ace07367d06cf41e19e54b0".into()),
                        exec_enabled: true,
                        problem: None,
                        version: "test".into(),
                        ssh: None,
                        exec_runs,
                    },
                    _ => DoorbellReply::Refused {
                        error: "only the health exchange is asked".into(),
                    },
                }
            },
        ));
        let reply =
            crate::exec_service::exchange(client, &DoorbellRequest::Health, RUNS_ASK_TIMEOUT).await;
        served.await.unwrap();
        ServiceRuns::of(reply, handed)
    }

    /// RED first (#4244): the run the service holds is in the gate's `Busy`,
    /// the gate defers on it with the hour's recheck, and the forced update's
    /// warning names it. Its count ends with the run.
    #[tokio::test]
    async fn a_run_the_system_service_holds_defers_the_periodic_update() {
        let held = HeldRuns::default();
        let run = held.hold(Instant::now());
        let service = service_says(&held, false, 0).await;
        assert_eq!(service, ServiceRuns::Answered(1));
        let busy = own().with_service_runs(&service);
        assert_eq!(
            busy.exec_runs, 1,
            "the run the SYSTEM service holds is absent from the gate's Busy: {busy}"
        );
        assert_eq!(
            periodic_gate(&busy, 0),
            (DeferDecision::DeferOnce, TRANSFER_DEFER_RECHECK),
            "the periodic gate does not defer on the service's run: {busy}"
        );
        assert_eq!(
            forced_update_warning(&busy).as_deref(),
            Some("forced update proceeding despite: 1 exec run(s)")
        );
        drop(run);
        let service = service_says(&held, false, 0).await;
        assert_eq!(
            service,
            ServiceRuns::Answered(0),
            "the count outlived the run"
        );
        let busy = own().with_service_runs(&service);
        assert_eq!(periodic_gate(&busy, 0).0, DeferDecision::Proceed, "{busy}");
    }

    /// Mixed versions (#4244): a service older than #4244 says nothing of its
    /// runs, so the gate counts the runs this worker handed it, each for
    /// [`HELD_AT_MOST`] at most. With none handed it holds nothing: an older
    /// service is no reason to wait for the update that replaces it.
    #[tokio::test]
    async fn an_older_service_counts_the_runs_this_worker_handed_it() {
        let held = HeldRuns::default();
        let _running = held.hold(Instant::now());
        let handed = HeldRuns::default();
        let t0 = Instant::now();
        handed.note(t0);

        let service = service_says(&held, true, handed.count_at(t0)).await;
        assert_eq!(
            service,
            ServiceRuns::Unread {
                why: Unread::Older,
                handed: 1
            }
        );
        let busy = own().with_service_runs(&service);
        assert_eq!(
            periodic_gate(&busy, 0),
            (DeferDecision::DeferOnce, TRANSFER_DEFER_RECHECK),
            "a run handed to an older service is absent from the gate's Busy: {busy}"
        );

        // Its bound spent, the record holds nothing.
        let service = service_says(&held, true, handed.count_at(t0 + HELD_AT_MOST)).await;
        assert_eq!(service.counted(), 0);
        let busy = own().with_service_runs(&service);
        assert_eq!(periodic_gate(&busy, 0).0, DeferDecision::Proceed, "{busy}");
    }

    /// A lost doorbell (#4244): a service that never answers is read within
    /// the bound, and then counts as what this worker handed it, until each
    /// handed run's bound. Never longer.
    #[tokio::test(start_paused = true)]
    async fn a_silent_service_counts_the_handed_runs_until_their_bound() {
        let (client, server) = tokio::io::duplex(1024);
        let silent = tokio::spawn(async move {
            let _open = server;
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        let reply =
            crate::exec_service::exchange(client, &DoorbellRequest::Health, RUNS_ASK_TIMEOUT).await;
        assert!(reply.is_err(), "{reply:?}");
        silent.abort();

        let handed = HeldRuns::default();
        let t0 = Instant::now();
        handed.note(t0);
        let service = ServiceRuns::of(reply.clone(), handed.count_at(t0));
        assert_eq!(
            service,
            ServiceRuns::Unread {
                why: Unread::NoAnswer,
                handed: 1
            }
        );
        let busy = own().with_service_runs(&service);
        assert_eq!(
            periodic_gate(&busy, 0).0,
            DeferDecision::DeferOnce,
            "a run handed to a silent service is absent from the gate's Busy: {busy}"
        );
        let later = t0 + HELD_AT_MOST;
        let service = ServiceRuns::of(reply, handed.count_at(later));
        let busy = own().with_service_runs(&service);
        assert_eq!(periodic_gate(&busy, 0).0, DeferDecision::Proceed, "{busy}");
    }

    /// A service started again since it took a run (a crash, an update by
    /// hand) holds nothing, and says so: its word stands over the worker's
    /// record of what it handed the service before.
    #[tokio::test]
    async fn a_restarted_service_is_believed_over_the_workers_record() {
        let restarted = HeldRuns::default();
        let handed = HeldRuns::default();
        handed.note(Instant::now());
        let service = service_says(&restarted, false, handed.count_at(Instant::now())).await;
        assert_eq!(service, ServiceRuns::Answered(0));
        let busy = own().with_service_runs(&service);
        assert_eq!(periodic_gate(&busy, 0).0, DeferDecision::Proceed, "{busy}");
    }
}

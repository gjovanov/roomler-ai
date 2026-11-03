// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #2718 — how the daemon lets its tokio runtime go: with a budget.
//!
//! Dropping a runtime waits for every `spawn_blocking` task to RETURN, and
//! nothing a shutdown does can make some of them return. On Windows, tokio
//! reads a child's stdout and stderr through its blocking pool
//! (`tokio::process`'s `ChildStdio` is a `Blocking` file), so a Fleet RPC
//! command that leaves a process holding its output pipe keeps a `ReadFile`
//! in flight until that process exits. The worker's drop then waited on it:
//!
//! - under a service stop (every exec-driven update), until the service host
//!   terminated it at the end of its 8 s budget (NEO16, 0.4.177 → 0.4.178:
//!   teardown done in 6 ms, then 8 s of silence);
//! - under its own restart, with no host budget behind it, until the
//!   watchdog's shutdown-stall guard forced `exit(2)` (NEO16, 0.4.178: 71.6 s
//!   offline, and a supervisor backoff for the "failure").
//!
//! [`shut_down`] waits for at most a budget and then leaves what is still
//! blocked behind; the process exit that follows ends it. By then the
//! graceful teardown has run (the clean-stop mark is on disk), and the drops
//! that still have work to do, such as the MagicDNS revert (~0.85 s measured),
//! run on the runtime's own threads inside the budget.

use std::time::{Duration, Instant};

/// How long the daemon waits for its runtime's tasks and blocking work after
/// the teardown. Inside the service host's 8 s stop budget, with room for the
/// teardown before it, and long enough for the drops that revert host state.
pub const SHUTDOWN_BUDGET: Duration = Duration::from_secs(3);

/// Shut `rt` down, waiting at most `budget` for its tasks and blocking work.
/// Returns how long it took; a result at the budget means something blocked
/// was left behind.
pub fn shut_down(rt: tokio::runtime::Runtime, budget: Duration) -> Duration {
    let started = Instant::now();
    rt.shutdown_timeout(budget);
    started.elapsed()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runtime with one blocking task that will not return for 3 s, as an
    /// exec's read on a pipe something else holds.
    fn runtime_with_a_stuck_blocking_task() -> tokio::runtime::Runtime {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let (started, running) = std::sync::mpsc::channel();
        rt.spawn_blocking(move || {
            let _ = started.send(());
            std::thread::sleep(Duration::from_secs(3));
        });
        running.recv().expect("the blocking task runs");
        rt
    }

    #[test]
    fn a_stuck_blocking_task_holds_a_dropped_runtime_but_not_a_budgeted_shutdown() {
        // The control first: a plain drop waits for the blocking task, so this
        // test can tell the budget from a task that ended anyway.
        let rt = runtime_with_a_stuck_blocking_task();
        let started = Instant::now();
        drop(rt);
        assert!(
            started.elapsed() >= Duration::from_millis(2_500),
            "the control: a drop waits for the blocking task ({:?})",
            started.elapsed()
        );

        let took = shut_down(
            runtime_with_a_stuck_blocking_task(),
            Duration::from_millis(300),
        );
        assert!(
            took < Duration::from_millis(1_500),
            "a budgeted shutdown leaves the blocking task behind ({took:?})"
        );
    }

    #[test]
    fn the_budget_fits_inside_the_service_hosts_stop_budget() {
        // The SCM host gives a worker 8 s from the stop to exit, and the
        // teardown before the runtime takes its share of that.
        assert!(SHUTDOWN_BUDGET <= Duration::from_secs(4));
    }
}

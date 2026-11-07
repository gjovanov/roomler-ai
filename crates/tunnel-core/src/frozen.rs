// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3530 — a carrier's silence, told apart from this agent's own pause.
//!
//! A loop that convicts a carrier of silence measures it from the last frame
//! it READ: the control WS's RX deadline, the `/derp` WS's, a relay carrier's
//! rx-stale rule. When the whole process did not run (a SIGSTOP, a cgroup
//! freeze, a VM pause, a host sleep that `Instant` counts), nothing was read,
//! and the loop wakes to a silence it made itself. The tick that found it says
//! so: it came back late by about as long as the process did not run.
//!
//! [`did_not_run_for`] reads that, so the line names the pause instead of a
//! middlebox or a relay. After a 152 s SIGSTOP on a lab guest (2026-11-07) the
//! agent blamed a "half-open socket (middlebox still ACKs our pings)", a
//! "half-open /derp WS" and a relay "not delivering the peer's frames", and
//! none of it was true.
//!
//! ⚠️ Wording only. No verdict reads it, and a carrier convicted after a pause
//! is torn down and rebuilt exactly as before: whether the socket survived the
//! pause is the carrier's own question.

use std::time::Duration;

/// A tick that comes back this late is the process not running, not a busy
/// scheduler. The same bound as the watchdog-of-watchdog's
/// (`THREAD_WATCHDOG_LATE_AFTER` in roomlerd).
pub const LATE_MIN: Duration = Duration::from_secs(10);

/// How long this agent did not run, when that explains a silence.
///
/// - `silent`: since the carrier's last frame was read.
/// - `late`: how far past its due time the tick that found the silence came.
///   The loop could not have read a frame during it.
/// - `slack`: how quiet the carrier gets while the loop runs: its keepalive
///   period and one tick.
///
/// `Some(late)` when the late tick covers the silence but for that slack: the
/// line says the agent did not run for about that long. `None` keeps the
/// line's verdict: the carrier went quiet while the agent was running.
pub fn did_not_run_for(silent: Duration, late: Duration, slack: Duration) -> Option<Duration> {
    (late >= LATE_MIN && silent <= late + slack).then_some(late)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    /// The lab guest's 152 s SIGSTOP: the control WS's keepalive (25 s) came
    /// back 140 s late to a 152 s silence. The agent did not run for it.
    #[test]
    fn a_freeze_explains_the_silence_it_made() {
        assert_eq!(did_not_run_for(s(152), s(140), s(50)), Some(s(140)));
        // The worst phase: the freeze began just after a keepalive whose pong
        // was not yet read, so the silence runs two periods past the lateness.
        assert_eq!(did_not_run_for(s(98), s(48), s(50)), Some(s(48)));
    }

    /// A half-open socket: the loop ran, its tick came on time, and the silence
    /// is the network's. Today's verdict stands.
    #[test]
    fn a_tick_on_time_keeps_the_verdict() {
        assert_eq!(did_not_run_for(s(81), Duration::ZERO, s(50)), None);
        assert_eq!(
            did_not_run_for(s(600), Duration::from_millis(40), s(50)),
            None
        );
    }

    /// A short pause on top of a longer silence: the carrier was quiet before the
    /// agent stopped, so the silence is not the pause's alone.
    #[test]
    fn a_silence_that_began_while_the_agent_ran_keeps_the_verdict() {
        assert_eq!(did_not_run_for(s(110), s(40), s(50)), None);
        // A stale lateness from an earlier tick (the overlay's net-change sweep
        // reads the last tick's) cannot relabel a relay silent for 10 minutes.
        assert_eq!(did_not_run_for(s(600), s(140), s(30)), None);
    }

    /// A busy scheduler is not a pause, whatever the silence.
    #[test]
    fn a_tick_a_few_seconds_late_is_no_pause() {
        assert_eq!(did_not_run_for(s(30), s(9), s(50)), None);
        assert_eq!(did_not_run_for(s(30), LATE_MIN, s(50)), Some(LATE_MIN));
    }
}

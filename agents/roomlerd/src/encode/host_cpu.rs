// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3439 — how busy the host was while a capability probe child ran, and how much
//! CPU the child itself had, so a child that ran out of time can say whether it
//! was starved or stuck.
//!
//! From its pipe the two look the same: no answer by the bound, no death by the
//! end of the kill wait. They differ in what the host did meanwhile. With no idle
//! CPU at all, a Normal-priority child behind higher-priority work runs only now
//! and then, and it cannot finish dying either. A killed child did die on #3439's
//! guest, 9 to 26 s after its kill, with the load still on. With idle CPU to
//! spare, a child that got nowhere was waiting on something, and a driver call is
//! the usual thing.
//!
//! Windows and Linux. Elsewhere every read is `None`, and the log keeps its old
//! words.

use std::time::Duration;

/// Below this share of idle CPU, a host has nothing left for a Normal process.
pub const STARVED_BELOW: f64 = 0.05;

/// A child that had less than this share of its bound in CPU time got almost
/// nowhere. One that had more was running, if not enough.
const RAN_BELOW: f64 = 0.10;

/// The host's CPU time, summed over every processor, at one moment, in the
/// platform's own units: only the shares between two of them mean anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTimes {
    idle: u64,
    total: u64,
}

impl HostTimes {
    /// Now. `None` off Windows and Linux, or when the read fails.
    pub fn now() -> Option<Self> {
        imp::host()
    }

    /// The idle share of the CPU between `self` and `later`, from 0 to 1. `None`
    /// when no time passed or the counters went back.
    pub fn idle_share(self, later: Self) -> Option<f64> {
        let total = later.total.checked_sub(self.total)?;
        let idle = later.idle.checked_sub(self.idle)?;
        if total == 0 {
            return None;
        }
        Some((idle as f64 / total as f64).clamp(0.0, 1.0))
    }
}

/// The CPU time a child process has had so far, user and kernel. `None` off
/// Windows and Linux, or when the read fails.
pub fn of_child(child: &std::process::Child) -> Option<Duration> {
    imp::child(child)
}

/// What the host and a child did over the child's wait.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Ran {
    /// The host's idle share from the spawn to the bound.
    pub idle_running: Option<f64>,
    /// The host's idle share over the kill wait.
    pub idle_dying: Option<f64>,
    /// The child's own CPU time at its bound.
    pub child_cpu: Option<Duration>,
    /// The bound itself.
    pub bound: Duration,
}

impl Ran {
    /// From three readings of the host (at the spawn, at the bound, at the end of
    /// the kill wait) and one of the child, at its bound.
    pub fn of(
        spawned: Option<HostTimes>,
        at_bound: Option<HostTimes>,
        ended: Option<HostTimes>,
        child_cpu: Option<Duration>,
        bound: Duration,
    ) -> Self {
        Self {
            idle_running: spawned.zip(at_bound).and_then(|(a, b)| a.idle_share(b)),
            idle_dying: at_bound.zip(ended).and_then(|(a, b)| a.idle_share(b)),
            child_cpu,
            bound,
        }
    }

    /// The host had no idle CPU while the child should have been running, and the
    /// child, where its time is known, got almost none of it.
    pub fn starved_running(&self) -> bool {
        let got_little = self
            .child_cpu
            .is_none_or(|cpu| cpu.as_secs_f64() < self.bound.as_secs_f64() * RAN_BELOW);
        self.idle_running.is_some_and(|s| s < STARVED_BELOW) && got_little
    }

    /// The host had no idle CPU while the child was meant to be dying.
    pub fn starved_dying(&self) -> bool {
        self.idle_dying.is_some_and(|s| s < STARVED_BELOW)
    }
}

/// A share for a log line: `0.4%`, or `unknown`.
pub fn pct(share: Option<f64>) -> String {
    share.map_or_else(|| "unknown".to_string(), |s| format!("{:.1}%", s * 100.0))
}

/// A CPU time for a log line, in milliseconds, or `unknown`.
pub fn ms(cpu: Option<Duration>) -> String {
    cpu.map_or_else(|| "unknown".to_string(), |d| d.as_millis().to_string())
}

/// `/proc/stat`'s `cpu` line: user, nice, system, idle, iowait, irq, softirq,
/// steal. Idle and iowait are the time nothing ran.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_stat(stat: &str) -> Option<HostTimes> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    if fields.len() < 5 {
        return None;
    }
    Some(HostTimes {
        idle: fields[3] + fields[4],
        total: fields.iter().sum(),
    })
}

/// `/proc/<pid>/stat`'s utime and stime (fields 14 and 15), in clock ticks. They
/// are counted after the last `)`, because the command name before it may hold
/// spaces and parentheses.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_pid_stat(stat: &str) -> Option<u64> {
    let after = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    // `fields[0]` is field 3, the state.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

#[cfg(windows)]
mod imp {
    use super::HostTimes;
    use std::os::windows::io::AsRawHandle;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetProcessTimes, GetSystemTimes};

    const ZERO: FILETIME = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };

    fn ticks(t: FILETIME) -> u64 {
        (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
    }

    pub(super) fn host() -> Option<HostTimes> {
        let (mut idle, mut kernel, mut user) = (ZERO, ZERO, ZERO);
        // SAFETY: three out-pointers to FILETIMEs on this stack, valid for the call.
        let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
        if ok == 0 {
            return None;
        }
        // The kernel time includes the idle time.
        Some(HostTimes {
            idle: ticks(idle),
            total: ticks(kernel).saturating_add(ticks(user)),
        })
    }

    pub(super) fn child(child: &std::process::Child) -> Option<Duration> {
        let (mut created, mut exited, mut kernel, mut user) = (ZERO, ZERO, ZERO, ZERO);
        // SAFETY: the handle is the child's own, open for as long as `child` lives,
        // and the out-pointers are FILETIMEs on this stack, valid for the call.
        let ok = unsafe {
            GetProcessTimes(
                child.as_raw_handle(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        };
        if ok == 0 {
            return None;
        }
        // In 100 ns units.
        let units = ticks(kernel).saturating_add(ticks(user));
        Some(Duration::from_nanos(units.saturating_mul(100)))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::HostTimes;
    use std::time::Duration;

    pub(super) fn host() -> Option<HostTimes> {
        super::parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
    }

    pub(super) fn child(child: &std::process::Child) -> Option<Duration> {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.id())).ok()?;
        let ticks = super::parse_pid_stat(&stat)?;
        // SAFETY: `sysconf` takes no pointers.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let hz = u64::try_from(hz).ok().filter(|h| *h > 0)?;
        Some(Duration::from_millis(ticks.saturating_mul(1000) / hz))
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::HostTimes;
    use std::time::Duration;

    pub(super) fn host() -> Option<HostTimes> {
        None
    }

    pub(super) fn child(_child: &std::process::Child) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(idle: u64, total: u64) -> HostTimes {
        HostTimes { idle, total }
    }

    #[test]
    fn the_idle_share_is_the_idle_time_over_the_total() {
        assert_eq!(at(100, 1000).idle_share(at(150, 2000)), Some(0.05));
        assert_eq!(at(0, 0).idle_share(at(0, 0)), None, "no time passed");
        assert_eq!(
            at(10, 10).idle_share(at(5, 20)),
            None,
            "a counter went back"
        );
    }

    #[test]
    fn a_host_with_no_idle_cpu_and_a_child_that_got_none_is_starved() {
        let bound = Duration::from_secs(60);
        // #3439's guest: 8 High spinners on 4 vCPUs.
        let ran = Ran::of(
            Some(at(0, 0)),
            Some(at(10, 24_000)),
            Some(at(12, 30_000)),
            Some(Duration::from_millis(400)),
            bound,
        );
        assert!(ran.starved_running());
        assert!(ran.starved_dying());
        // Unknown child time: the host alone decides.
        let ran = Ran {
            child_cpu: None,
            ..ran
        };
        assert!(ran.starved_running());
    }

    #[test]
    fn a_child_stuck_on_a_host_with_idle_cpu_is_not_starved() {
        let ran = Ran::of(
            Some(at(0, 0)),
            Some(at(20_000, 24_000)),
            Some(at(25_000, 30_000)),
            Some(Duration::from_millis(5)),
            Duration::from_secs(60),
        );
        assert!(!ran.starved_running());
        assert!(!ran.starved_dying());
    }

    #[test]
    fn a_child_that_ran_hard_on_a_busy_host_is_not_starved() {
        // It had the CPU, all the bound long: busy in a loop, not starved.
        let ran = Ran::of(
            Some(at(0, 0)),
            Some(at(10, 24_000)),
            None,
            Some(Duration::from_secs(58)),
            Duration::from_secs(60),
        );
        assert!(!ran.starved_running());
        assert!(
            !ran.starved_dying(),
            "no reading at the end: nothing to say"
        );
    }

    #[test]
    fn nothing_known_says_nothing() {
        let ran = Ran::default();
        assert!(!ran.starved_running());
        assert!(!ran.starved_dying());
        assert_eq!(pct(None), "unknown");
        assert_eq!(ms(None), "unknown");
        assert_eq!(pct(Some(0.004)), "0.4%");
    }

    #[test]
    fn proc_stat_is_read_from_its_cpu_line() {
        let stat = "cpu  100 5 50 800 40 3 2 0 0 0\ncpu0 50 2 25 400 20 1 1 0 0 0\n";
        assert_eq!(parse_proc_stat(stat), Some(at(840, 1000)));
        assert_eq!(parse_proc_stat("intr 1 2 3\n"), None);
    }

    #[test]
    fn pid_stat_counts_past_a_command_name_with_spaces() {
        // pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt majflt
        // cmajflt utime stime ...
        let stat = "4242 (roomlerd (caps) x) S 1 4242 4242 0 -1 4194560 120 0 0 0 37 5 0 0 20 0";
        assert_eq!(parse_pid_stat(stat), Some(42));
        assert_eq!(parse_pid_stat("4242 (short) S 1"), None);
    }

    #[test]
    fn this_host_reads_as_something_or_nothing_without_panicking() {
        if let Some(a) = HostTimes::now() {
            std::thread::sleep(Duration::from_millis(30));
            let b = HostTimes::now().expect("a second read where the first worked");
            if let Some(share) = a.idle_share(b) {
                assert!((0.0..=1.0).contains(&share));
            }
        }
        let me = std::process::Command::new(std::env::current_exe().expect("this test"))
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = me {
            let _ = of_child(&child);
            let _ = child.wait();
        }
    }
}

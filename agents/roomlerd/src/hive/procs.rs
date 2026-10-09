// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P1h — a harness's process, from outside its session task.
//!
//! Each harness leads its own process group (`process_group(0)`), so its
//! tools go down with it. A service manager reaches that group only when it
//! kills a whole cgroup: systemd's `KillMode=control-group` does, launchd
//! does not, and nothing does for a daemon that runs unsupervised. So the
//! daemon takes its harnesses down itself when it leaves ([`super::wind_down`]),
//! and a daemon resuming what a crashed one hosted first takes down a harness
//! the crash left running — two harnesses on one Claude Code history would
//! each write it.
//!
//! A pid alone does not name a process across time: the pid of a harness that
//! ended may belong to anything by now. [`started`] adds the process's start
//! time, and a group is signalled only when both still match.

use std::time::{Duration, Instant};

/// When `pid` started, as a token that differs for every process ever given
/// that pid: on Linux the boot id and the start time in clock ticks since
/// boot, on macOS the start time in microseconds since the epoch. `None`: no
/// such process (or none this daemon may read).
pub(crate) fn started(pid: u32) -> Option<String> {
    imp::started(pid)
}

/// Signal `pid`'s process group (the group a harness leads).
pub(crate) fn signal_group(pid: u32, signal: libc::c_int) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    if pid <= 1 {
        return;
    }
    // SAFETY: a plain syscall; the negative pid addresses the group.
    unsafe {
        libc::kill(-pid, signal);
    }
}

/// Whether anything is left in `pid`'s process group.
pub(crate) fn group_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 1 {
        return false;
    }
    // SAFETY: signal 0 delivers nothing; it only asks whether the group
    // exists. EPERM still means it does.
    let rc = unsafe { libc::kill(-pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// SIGTERM to each group — a harness exits cleanly on it and Claude Code
/// records its running cost — then SIGKILL, after `grace`, to any group still
/// there. Returns once every group is gone, or after the SIGKILL.
pub(crate) async fn take_down(pids: &[u32], grace: Duration) {
    if pids.is_empty() {
        return;
    }
    for &pid in pids {
        signal_group(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && pids.iter().any(|&p| group_alive(p)) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for &pid in pids {
        if group_alive(pid) {
            signal_group(pid, libc::SIGKILL);
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub(super) fn started(pid: u32) -> Option<String> {
        // Field 22 of /proc/<pid>/stat is the start time in clock ticks since
        // boot. Field 2, the command, is parenthesised and may hold spaces,
        // so the fields are counted from its closing parenthesis: the first
        // after it is field 3.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ticks: u64 = stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(19)?
            .parse()
            .ok()?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        Some(format!("{}:{ticks}", boot.trim()))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    pub(super) fn started(pid: u32) -> Option<String> {
        let pid = libc::c_int::try_from(pid).ok()?;
        // SAFETY: the buffer is a zeroed `proc_bsdinfo` this frame owns,
        // passed with its own size; the call writes at most that much.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if n != size {
            return None;
        }
        Some(format!(
            "{}",
            info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn a_live_process_has_a_start_time_that_holds_and_a_dead_one_has_none() {
        let me = std::process::id();
        let a = started(me).expect("this process's own start time");
        assert_eq!(started(me), Some(a), "the same process, the same token");
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert_eq!(started(pid), None, "a reaped process has no start time");
    }

    #[tokio::test]
    async fn take_down_ends_a_group_that_ignores_sigterm_too() {
        // A group leader that ignores SIGTERM, and a tool beneath it.
        let mut stubborn = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 60 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = stubborn.id();
        assert!(group_alive(pid));
        take_down(&[pid], Duration::from_millis(300)).await;
        stubborn.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while group_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!group_alive(pid), "every process in the group is gone");
    }

    #[test]
    fn pid_zero_and_one_are_never_signalled() {
        // `kill(0, …)` is our own group and `kill(-1, …)` is everyone:
        // neither may ever be reached from a recorded pid.
        assert!(!group_alive(0));
        assert!(!group_alive(1));
        signal_group(0, 0);
        signal_group(1, 0);
    }
}

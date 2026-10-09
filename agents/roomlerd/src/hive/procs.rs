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

/// P1j — the Claude Code that ran an adopt hook `pid`: the hook's nearest
/// ancestor that is not a shell. Claude Code runs a hook's command line
/// through `/bin/sh -c`, and a shell that forks the command instead of
/// exec'ing it stands between the two and exits with the hook — dash does
/// (Claude Code 2.1.293 on Ubuntu's dash 0.5.12, the field run). Taken for
/// the terminal, that shell ended every adopted session at the next sweep,
/// and the next turn's hook offered the session again as a new record.
/// Claude Code is never a shell, and a shell that exec'd the hook is not
/// there at all. `None`: no such process, or more shells than any wrapper.
pub(crate) fn hook_terminal(pid: u32) -> Option<u32> {
    let mut p = imp::parent(pid)?;
    for _ in 0..MAX_WRAPPING_SHELLS {
        if !SHELLS.contains(&imp::name(p)?.as_str()) {
            return Some(p);
        }
        p = imp::parent(p)?;
    }
    None
}

/// What a hook's command line may run under, by process name.
const SHELLS: [&str; 8] = ["sh", "dash", "bash", "zsh", "ksh", "mksh", "ash", "fish"];

/// The most shells between a hook and the Claude Code that ran it.
const MAX_WRAPPING_SHELLS: usize = 3;

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

    pub(super) fn parent(pid: u32) -> Option<u32> {
        // Field 4, the parent pid: the first after the command's closing
        // parenthesis is field 3 (the state).
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ppid: u32 = stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        (ppid > 1).then_some(ppid)
    }

    pub(super) fn name(pid: u32) -> Option<String> {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        Some(comm.trim_end_matches('\n').to_string())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    /// The process's BSD info: its start time, parent and name.
    fn bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
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
        (n == size).then_some(info)
    }

    pub(super) fn started(pid: u32) -> Option<String> {
        let info = bsdinfo(pid)?;
        Some(format!(
            "{}",
            info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec
        ))
    }

    pub(super) fn parent(pid: u32) -> Option<u32> {
        let info = bsdinfo(pid)?;
        (info.pbi_ppid > 1).then_some(info.pbi_ppid)
    }

    pub(super) fn name(pid: u32) -> Option<String> {
        let info = bsdinfo(pid)?;
        let bytes: Vec<u8> = info
            .pbi_comm
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8(bytes).ok()
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

    #[test]
    fn a_hook_belongs_to_its_nearest_ancestor_that_is_not_a_shell() {
        let me = std::process::id();
        // Run directly, as a shell that exec'd the command leaves it.
        let mut direct = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        assert_eq!(hook_terminal(direct.id()), Some(me));
        direct.kill().unwrap();
        direct.wait().unwrap();

        // Run through `sh -c` by a shell that forks it, as dash runs a hook's
        // command line for Claude Code; a background job keeps the shell
        // between the two whatever the shell.
        let dir = tempfile::tempdir().unwrap();
        let said = dir.path().join("pid");
        let mut sh = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("sleep 30 & echo $! > '{}'; wait", said.display()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let hook = loop {
            if let Some(p) = std::fs::read_to_string(&said)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
            {
                break p;
            }
            assert!(Instant::now() < deadline, "the shell never named its job");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(imp::parent(hook), Some(sh.id()), "the shell stands between");
        assert_eq!(hook_terminal(hook), Some(me), "and the terminal is past it");
        // SAFETY: a plain syscall to the job this test started.
        unsafe {
            libc::kill(libc::pid_t::try_from(hook).unwrap(), libc::SIGKILL);
        }
        sh.wait().unwrap();
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

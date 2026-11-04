// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3437 — what a run of the key helper leaves behind when it is ended: past
//! its time limit, or given up mid-run (a daemon-hosted call whose caller hung
//! up drops it).

use super::*;

/// A test helper's time limit: its shell, and the child the shell starts, are
/// up long before it.
const LIMIT: Duration = Duration::from_millis(1500);

// ─── Unix ────────────────────────────────────────────────────────────────────

/// How long a process the run ended is given to be gone.
#[cfg(unix)]
const GONE_WITHIN: Duration = Duration::from_secs(5);

/// A helper with a child of its own, whatever `/bin/sh` is: a `sleep` put in
/// the background, its pid written to `file`, and waited on.
#[cfg(unix)]
fn with_a_child(file: &std::path::Path) -> String {
    format!("sleep 300 & echo $! > '{}'; wait", file.display())
}

/// The pid the helper wrote to `file`.
#[cfg(unix)]
async fn written(file: &std::path::Path) -> libc::pid_t {
    for _ in 0..500 {
        if let Some(pid) = std::fs::read_to_string(file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the helper never wrote its child's pid");
}

/// Whether `pid` has ended: gone, or a zombie that whoever adopted it has yet
/// to wait for.
#[cfg(unix)]
fn ended(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 delivers nothing; it asks whether `pid` exists.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    #[cfg(target_os = "linux")]
    {
        // "pid (comm) state …": the state follows the comm's last ')'.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                s.rsplit_once(')')
                    .map(|(_, rest)| rest.trim_start().starts_with('Z'))
            })
            .unwrap_or(true)
    }
    #[cfg(not(target_os = "linux"))]
    false
}

/// Whether `pid` ends within [`GONE_WITHIN`].
#[cfg(unix)]
async fn ends(pid: libc::pid_t) -> bool {
    let until = tokio::time::Instant::now() + GONE_WITHIN;
    while tokio::time::Instant::now() < until {
        if ended(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    ended(pid)
}

/// A process the test ends at its own end, whatever it found: a red run must
/// not leave a `sleep 300` behind.
#[cfg(unix)]
struct Stray(libc::pid_t);

#[cfg(unix)]
impl Drop for Stray {
    fn drop(&mut self) {
        if !ended(self.0) {
            // SAFETY: a plain syscall, on a pid this test's helper started and
            // that was just seen running.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }
}

/// #3437 — a helper past its time limit leaves nothing it started running.
/// `kill_on_drop` alone reached the shell, and its `sleep` went on, adopted.
#[cfg(unix)]
#[tokio::test]
async fn a_helper_past_its_time_limit_leaves_nothing_it_started_running() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("child");
    let run = start(&with_a_child(&file)).unwrap();
    assert!(
        run.output_within(LIMIT).await.is_none(),
        "it outlasts its limit"
    );
    let child = Stray(written(&file).await);
    assert!(
        ends(child.0).await,
        "its child {} still runs after the limit",
        child.0
    );
}

/// #3437 — a run given up mid-way (its future dropped) leaves nothing it
/// started running either.
#[cfg(unix)]
#[tokio::test]
async fn a_helper_given_up_mid_run_leaves_nothing_it_started_running() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("child");
    let run = start(&with_a_child(&file)).unwrap();
    let output = run.output_within(Duration::from_secs(60));
    // The run's future goes with the select, once the child is written down.
    let child = tokio::select! {
        _ = output => panic!("it ended by itself"),
        pid = written(&file) => Stray(pid),
    };
    assert!(
        ends(child.0).await,
        "its child {} still runs after the run was given up",
        child.0
    );
}

/// `pid`'s children, from every process's `stat`.
#[cfg(target_os = "linux")]
fn children(of: libc::pid_t) -> Vec<libc::pid_t> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // "pid (comm) state ppid …": the comm may hold spaces and parentheses.
        let parent = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|p| p.parse::<libc::pid_t>().ok());
        if parent == Some(of) {
            found.push(pid);
        }
    }
    found
}

/// #3437's own shape: a helper of one command, which dash forks rather than
/// becoming, so the helper is the shell's child. Where `/bin/sh` becomes the
/// command instead (bash), the shell's pid is the helper, and that ends too.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_lone_command_the_shell_forks_ends_at_the_limit_too() {
    let run = start("sleep 300").unwrap();
    let shell = libc::pid_t::try_from(run.pid().unwrap()).unwrap();
    let until = tokio::time::Instant::now() + GONE_WITHIN;
    let helper = loop {
        if let Some(&child) = children(shell).first() {
            break child;
        }
        if std::fs::read_to_string(format!("/proc/{shell}/comm")).is_ok_and(|c| c.trim() == "sleep")
        {
            break shell;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "the shell started nothing"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let helper = Stray(helper);
    assert!(
        run.output_within(LIMIT).await.is_none(),
        "it outlasts its limit"
    );
    assert!(
        ends(helper.0).await,
        "the helper {} (the shell {shell}'s) still runs after the limit",
        helper.0
    );
}

/// What a helper that ends by itself left running of its own accord stays:
/// only a run that is ended takes its tree down. Its background `sleep`
/// holds none of the run's pipes, so the run ends at the shell's exit.
#[cfg(unix)]
#[tokio::test]
async fn what_a_helper_that_ends_by_itself_left_running_stays() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("child");
    let line = format!(
        "sleep 300 > /dev/null 2>&1 & echo $! > '{}'; echo key",
        file.display()
    );
    let out = start(&line)
        .unwrap()
        .output_within(Duration::from_secs(30))
        .await
        .expect("it ends by itself")
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "key");
    let child = Stray(written(&file).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!ended(child.0), "what it left running was ended");
}

// ─── Windows ─────────────────────────────────────────────────────────────────

/// The `ping` that `cmd.exe` starts for a helper line, watched by a handle of
/// its own, so a reused pid cannot answer for it; ended at the test's end.
#[cfg(windows)]
fn its_child(run: &Run) -> crate::exec::session_lifetime::Watched {
    let shell = run.pid().unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(child) = crate::hive::procs::child_of(shell) {
            return crate::exec::session_lifetime::Watched::open(child);
        }
        assert!(
            std::time::Instant::now() < until,
            "cmd.exe started nothing"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// #3437 — a helper past its time limit leaves nothing it started running:
/// `TerminateProcess` on `cmd.exe` alone left its `ping` running.
#[cfg(windows)]
#[tokio::test]
async fn a_helper_past_its_time_limit_leaves_nothing_it_started_running() {
    use crate::exec::session_lifetime::GONE_WITHIN;
    let run = start("ping -n 300 127.0.0.1").unwrap();
    let child = its_child(&run);
    assert!(
        run.output_within(LIMIT).await.is_none(),
        "it outlasts its limit"
    );
    assert!(
        child.gone_within(GONE_WITHIN),
        "its ping still runs after the limit"
    );
}

/// #3437 — a run given up mid-way (its future dropped) leaves nothing it
/// started running either.
#[cfg(windows)]
#[tokio::test]
async fn a_helper_given_up_mid_run_leaves_nothing_it_started_running() {
    use crate::exec::session_lifetime::GONE_WITHIN;
    let run = start("ping -n 300 127.0.0.1").unwrap();
    let child = its_child(&run);
    let output = run.output_within(Duration::from_secs(60));
    tokio::select! {
        _ = output => panic!("it ended by itself"),
        () = tokio::time::sleep(Duration::from_millis(300)) => {}
    }
    assert!(
        child.gone_within(GONE_WITHIN),
        "its ping still runs after the run was given up"
    );
}

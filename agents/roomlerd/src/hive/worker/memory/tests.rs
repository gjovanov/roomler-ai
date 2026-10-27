// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
use super::*;

/// A unit delegates the memory controller only where systemd says both:
/// `Delegate=yes`, and `memory` among the controllers it delegates.
#[test]
fn a_unit_delegates_memory_only_with_the_controller_named() {
    assert!(delegates_memory(
        "Delegate=yes\nDelegateControllers=cpu cpuacct io blkio memory pids\n"
    ));
    assert!(
        !delegates_memory("Delegate=yes\nDelegateControllers=cpu pids\n"),
        "delegated, without memory"
    );
    assert!(
        !delegates_memory("Delegate=no\nDelegateControllers=\n"),
        "not delegated"
    );
    assert!(
        !delegates_memory("DelegateControllers=memory\n"),
        "no word on Delegate"
    );
    assert!(!delegates_memory(""), "no answer");
    assert!(
        !delegates_memory("Delegate=yes\nDelegateControllersX=memory\n"),
        "another property"
    );
}

/// The daemon's cgroup is the `0::` line of `/proc/self/cgroup`, and none
/// where the host has no cgroup v2 hierarchy; the unit's cgroup is that one,
/// or the one above the daemon's leaf once the daemon is in it.
#[test]
fn the_daemons_cgroup_is_its_v2_line_and_the_units_is_above_its_leaf() {
    assert_eq!(
        v2_path("0::/system.slice/roomlerd.service\n"),
        Some("/system.slice/roomlerd.service")
    );
    assert_eq!(
        v2_path("12:memory:/system.slice/roomlerd.service\n0::/system.slice/roomlerd.service\n"),
        Some("/system.slice/roomlerd.service"),
        "a hybrid host"
    );
    assert_eq!(
        v2_path("12:memory:/x\n1:name=systemd:/x\n"),
        None,
        "v1 alone"
    );
    let unit = Path::new(CGROUP_ROOT).join("system.slice/roomlerd.service");
    assert_eq!(unit_dir("/system.slice/roomlerd.service"), unit);
    assert_eq!(
        unit_dir("/system.slice/roomlerd.service/roomlerd"),
        unit,
        "the daemon in its leaf already"
    );
}

/// The workers are bounded only where systemd said the unit delegates the
/// memory controller: never on a guess, nor where systemd did not answer;
/// delegated with no cgroup v2 hierarchy, an error, so no worker runs.
#[test]
fn the_workers_are_bounded_only_where_the_unit_delegates_memory() {
    let delegated = "Delegate=yes\nDelegateControllers=cpu memory pids\n";
    let v2 = "0::/system.slice/roomlerd.service\n";
    assert_eq!(
        base_of(Some(delegated), Some(v2)),
        Ok(Some(
            Path::new(CGROUP_ROOT).join("system.slice/roomlerd.service")
        ))
    );
    assert_eq!(
        base_of(Some("Delegate=no\nDelegateControllers=\n"), Some(v2)),
        Ok(None),
        "not delegated: never bounded on a guess"
    );
    assert_eq!(base_of(None, Some(v2)), Ok(None), "systemd did not answer");
    assert!(
        base_of(Some(delegated), Some("12:memory:/x\n")).is_err(),
        "delegated, on cgroup v1 alone"
    );
    assert!(base_of(Some(delegated), None).is_err(), "no cgroup read");
}

/// A worker's end reads its cgroup's `oom_kill`: how many times the kernel
/// killed there for its memory.
#[test]
fn an_oom_kill_is_read_from_its_cgroups_memory_events() {
    let events = "low 0\nhigh 0\nmax 12\noom 2\noom_kill 1\noom_group_kill 0\n";
    assert_eq!(oom_kills(events), 1, "the kills, not the times it was full");
    assert_eq!(oom_kills("low 0\noom_kill 0\n"), 0);
    assert_eq!(oom_kills(""), 0, "no account read");
}

/// Where the unit delegates a cgroup with the memory controller, a worker
/// runs in one of its own, joined before it runs a step: its memory bounded
/// at `MAX_MEMORY` and none of it on swap; the daemon's own processes in
/// their leaf; and the worker's cgroup gone at its end.
///
/// Run inside a delegated cgroup, which CI has not:
/// `systemd-run --user --scope -p Delegate=yes cargo test … -- --ignored`.
#[tokio::test]
#[ignore = "needs a delegated cgroup with the memory controller (systemd-run --user --scope -p Delegate=yes)"]
async fn a_worker_runs_in_a_bounded_cgroup_of_its_own_where_the_unit_delegates_one() {
    let own = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let base = unit_dir(v2_path(&own).expect("a cgroup v2 hierarchy"));
    ready_for_test(&base).expect(
        "a delegated cgroup with the memory controller (systemd-run --user --scope -p Delegate=yes)",
    );
    let me = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    assert!(
        me.trim_end().ends_with(&format!("/{DAEMON_LEAF}")),
        "the daemon's processes moved into their leaf: {me}"
    );
    let program = super::super::Program {
        exe: "/bin/sh".into(),
        arg0: None,
        args: vec!["-c".into(), "sleep 30".into()],
    };
    let (_ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let (_ours_d, theirs_d) = std::os::unix::net::UnixStream::pair().unwrap();
    let theirs = super::super::Theirs {
        ch: theirs,
        descriptors: theirs_d,
    };
    let mut running = super::super::Spawn::spawn(&program, theirs).unwrap();
    let pid = running.pid().unwrap();
    let theirs = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    let path = v2_path(&theirs).unwrap().to_string();
    let leaf = path.rsplit('/').next().unwrap();
    assert!(
        leaf.starts_with(WORKER_PREFIX),
        "the worker in a cgroup of its own: {path}"
    );
    let dir = Path::new(CGROUP_ROOT).join(path.trim_start_matches('/'));
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).map(|s| s.trim().to_string());
    assert_eq!(
        read("memory.max").unwrap(),
        MAX_MEMORY.to_string(),
        "its bound"
    );
    if let Ok(swap) = read("memory.swap.max") {
        assert_eq!(swap, "0", "none of it on swap");
    }
    running.kill().await;
    assert!(!dir.exists(), "its cgroup gone with it: {}", dir.display());
}

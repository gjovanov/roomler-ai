// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

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
/// memory controller, never on a guess. Delegated with no cgroup v2 hierarchy,
/// an error, so no worker runs. Where systemd said nothing, nothing is known,
/// which is not its saying no: that passes, and is asked again (#3349).
#[test]
fn the_workers_are_bounded_only_where_the_unit_delegates_memory() {
    let delegated = "Delegate=yes\nDelegateControllers=cpu memory pids\n";
    let v2 = "0::/system.slice/roomlerd.service\n";
    assert_eq!(
        base_of(Ok(delegated), Some(v2)),
        Ok(Some(
            Path::new(CGROUP_ROOT).join("system.slice/roomlerd.service")
        ))
    );
    assert_eq!(
        base_of(Ok("Delegate=no\nDelegateControllers=\n"), Some(v2)),
        Ok(None),
        "not delegated: never bounded on a guess"
    );
    assert_eq!(
        base_of(Err("systemd did not answer"), Some(v2)),
        Err(NotReady::Passing("systemd did not answer".into())),
        "no answer is no no"
    );
    assert!(
        matches!(
            base_of(Ok(delegated), Some("12:memory:/x\n")),
            Err(NotReady::Broken(_))
        ),
        "delegated, on cgroup v1 alone"
    );
    assert!(
        matches!(base_of(Ok(delegated), None), Err(NotReady::Broken(_))),
        "no cgroup read"
    );
}

/// `sh` with `script`, as `said` runs a `systemctl`: ended with its wait.
fn sh(script: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.args(["-c", script]).kill_on_drop(true);
    cmd
}

/// Systemd's answer is what it wrote. Each way it can say nothing is named in
/// words, and none of them reads as an answer: past the bound, a failure to
/// answer, and a `systemctl` that could not be run (#3349, q1's three).
#[tokio::test]
async fn each_silence_of_systemd_is_named_and_none_reads_as_an_answer() {
    assert_eq!(
        said(sh("echo Delegate=no"), NOT_REACHED).await,
        Ok("Delegate=no\n".to_string())
    );

    let from = Instant::now();
    let past = said(sh("sleep 30"), Duration::from_millis(50)).await;
    assert!(from.elapsed() < NOT_REACHED, "{:?}", from.elapsed());
    let failed = said(sh("echo Delegate=yes; exit 3"), NOT_REACHED).await;
    let not_run = said(
        tokio::process::Command::new("/nowhere/systemctl"),
        NOT_REACHED,
    )
    .await;
    for (got, words) in [
        (&past, "did not answer within 50ms"),
        (&failed, "failed to answer: exit status: 3"),
        (&not_run, "could not be run"),
    ] {
        let why = got.as_ref().expect_err(words);
        assert!(why.contains(words), "{words}: {why}");
        assert!(!why.contains("delegates"), "no word on delegation: {why}");
        assert_eq!(
            base_of(Err(why.as_str()), Some("0::/x\n")),
            Err(NotReady::Passing(why.clone()))
        );
    }
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
    ready_for_test(&base).await.expect(
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

/// FR-90 P8-2d-1 (decision 72, AC4) — the device's worker, as its own spawner
/// starts it: in a cgroup of its own at a bound of its own, none of it on swap,
/// and holding no process but itself. What the daemon spawns (a session's
/// account's command, `hive-pack`) goes on in the daemon's leaf, outside it, so
/// that bound is never spent on them, nor an out-of-memory kill aimed at them.
///
/// Run inside a delegated cgroup, which CI has not: see the test above. Its
/// bound exceeded is the field run's (§3i "P8-2d-1, as built"): a real out-of-
/// memory kill is no test for a shared box.
#[cfg(worker_calls)]
#[tokio::test]
#[ignore = "needs a delegated cgroup with the memory controller (systemd-run --user --scope -p Delegate=yes)"]
async fn the_devices_worker_has_its_own_bound_and_what_the_daemon_spawns_stays_outside() {
    let own = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let base = unit_dir(v2_path(&own).expect("a cgroup v2 hierarchy"));
    ready_for_test(&base).await.expect(
        "a delegated cgroup with the memory controller (systemd-run --user --scope -p Delegate=yes)",
    );
    let spawner = super::super::device::OwnBinary(super::super::Program {
        exe: "/bin/sh".into(),
        arg0: None,
        args: vec!["-c".into(), "sleep 30".into()],
    });
    let (_ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let (_ours_d, theirs_d) = std::os::unix::net::UnixStream::pair().unwrap();
    let theirs = super::super::Theirs {
        ch: theirs,
        descriptors: theirs_d,
    };
    let mut running = super::super::Spawn::spawn(&spawner, theirs).unwrap();
    let pid = running.pid().unwrap();
    let cgroup_of = |pid: u32| {
        let read = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        v2_path(&read).unwrap().to_string()
    };
    let path = cgroup_of(pid);
    let leaf = path.rsplit('/').next().unwrap();
    assert!(leaf.starts_with(WORKER_PREFIX), "its own: {path}");
    let dir = Path::new(CGROUP_ROOT).join(path.trim_start_matches('/'));
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).map(|s| s.trim().to_string());
    assert_eq!(
        read("memory.max").unwrap(),
        super::super::device::MAX_MEMORY.to_string(),
        "its own bound, not a session's"
    );
    assert_ne!(super::super::device::MAX_MEMORY, MAX_MEMORY);
    if let Ok(swap) = read("memory.swap.max") {
        assert_eq!(swap, "0", "none of it on swap");
    }
    assert_eq!(
        read("cgroup.procs").unwrap(),
        pid.to_string(),
        "itself alone"
    );

    let mut outside = tokio::process::Command::new("/bin/sh")
        .args(["-c", "sleep 30"])
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let theirs = cgroup_of(outside.id().unwrap());
    assert!(
        theirs.ends_with(&format!("/{DAEMON_LEAF}")),
        "what the daemon spawns stays in its leaf: {theirs}"
    );
    assert_eq!(
        read("cgroup.procs").unwrap(),
        pid.to_string(),
        "still alone"
    );
    let _ = outside.kill().await;
    running.kill().await;
    assert!(!dir.exists(), "its cgroup gone with it: {}", dir.display());
}

// ─── What passes (#3349) ────────────────────────────────────────────────────

/// A stand-in for a unit's cgroup, in a directory: `listed` is its
/// `cgroup.procs`, which a write to the leaf's never changes, as the kernel's
/// never does for a process in its exit.
fn stand_in(listed: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cgroup.controllers"), "cpu memory pids\n").unwrap();
    std::fs::write(dir.path().join("cgroup.procs"), listed).unwrap();
    dir
}

/// While this file is in a stand-in, a task is still in its cgroup. What is
/// listed does not say: only the hand-down does ([`as_the_kernel`]).
const A_TASK: &str = "a-task-is-here";

/// The kernel's answer to a hand-down: `EBUSY` while a task is here.
fn as_the_kernel(base: &Path) -> std::io::Result<()> {
    if base.join(A_TASK).exists() {
        return Err(std::io::Error::from_raw_os_error(libc::EBUSY));
    }
    hand_down(base)
}

/// A bound that no test is meant to reach.
const NOT_REACHED: Duration = Duration::from_secs(30);

/// The process stays listed and cannot be moved, and the kernel refuses the
/// hand-down until its exit ends. That is waited out, and nothing is lost.
#[tokio::test]
async fn a_process_in_its_exit_is_waited_out() {
    let dir = stand_in("4242\n");
    let task = dir.path().join(A_TASK);
    std::fs::write(&task, "").unwrap();
    let ends = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::remove_file(task).unwrap();
    });
    let base = dir.path().to_path_buf();
    let got = ready_within(base.clone(), NOT_REACHED, as_the_kernel).await;
    assert_eq!(got, Ok(base.clone()));
    assert_eq!(
        std::fs::read_to_string(base.join("cgroup.subtree_control")).unwrap(),
        "+memory"
    );
    assert_eq!(
        std::fs::read_to_string(base.join(DAEMON_LEAF).join("cgroup.procs")).unwrap(),
        "4242",
        "its move was tried"
    );
    ends.await.unwrap();
}

/// What the field found: a leader gone before a thread of its process is listed
/// with no task left here. The kernel hands the controller down, so what is
/// listed holds nothing up, not even for a bound of nothing.
#[tokio::test]
async fn a_zombie_still_listed_holds_nothing_up() {
    let dir = stand_in("4242\n");
    let base = dir.path().to_path_buf();
    let got = ready_within(base.clone(), Duration::ZERO, as_the_kernel).await;
    assert_eq!(got, Ok(base));
}

/// Past its bound a task still here is a failure that passes, and it names what
/// is listed: a pid with its name where it has one, and one that has none.
#[tokio::test]
async fn what_stays_past_the_bound_is_named() {
    let own = std::process::id();
    let dir = stand_in(&format!("{own}\n999999999\n"));
    let base = dir.path().to_path_buf();
    std::fs::write(base.join(A_TASK), "").unwrap();
    let from = Instant::now();
    let within = Duration::from_millis(40);
    let got = tokio::time::timeout(
        NOT_REACHED,
        ready_within(base.clone(), within, as_the_kernel),
    )
    .await
    .expect("its bound holds");
    assert!(from.elapsed() >= within, "{:?}", from.elapsed());
    let Err(NotReady::Passing(why)) = got else {
        panic!("{got:?}");
    };
    assert!(why.contains(&format!("{own} (")), "{why}");
    assert!(why.contains("999999999"), "{why}");
}

/// Only `EBUSY` is waited out. Anything else will not pass by itself, and says
/// so at once: a hand-down refused otherwise, a move refused, no controller.
#[tokio::test]
async fn what_will_not_pass_is_said_at_once() {
    let from = Instant::now();
    let dir = stand_in("");
    let base = dir.path().to_path_buf();
    let refused = |_: &Path| Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP));
    let got = ready_within(base.clone(), NOT_REACHED, refused).await;
    assert!(
        matches!(&got, Err(NotReady::Broken(why)) if why.contains("handing memory down")),
        "{got:?}"
    );

    let dir = stand_in("4242\n");
    let base = dir.path().to_path_buf();
    std::fs::create_dir_all(base.join(DAEMON_LEAF).join("cgroup.procs")).unwrap();
    let got = ready_within(base, NOT_REACHED, as_the_kernel).await;
    assert!(
        matches!(&got, Err(NotReady::Broken(why)) if why.contains("moving 4242")),
        "{got:?}"
    );

    let dir = stand_in("4242\n");
    let base = dir.path().to_path_buf();
    std::fs::write(base.join("cgroup.controllers"), "cpu pids\n").unwrap();
    let got = ready_within(base, NOT_REACHED, as_the_kernel).await;
    assert!(
        matches!(&got, Err(NotReady::Broken(why)) if why.contains("no memory controller")),
        "{got:?}"
    );
    assert!(from.elapsed() < NOT_REACHED, "{:?}", from.elapsed());
}

/// What a test's systemd says: this unit delegates, and here is its cgroup.
async fn delegated(base: &Path) -> Result<Option<PathBuf>, NotReady> {
    Ok(Some(base.to_path_buf()))
}

/// How a `slot` refused, where it did.
fn refusal(w: &Workers) -> String {
    match w.slot(1 << 20) {
        Err(e) => e.to_string(),
        Ok(slot) => panic!("a worker would start, in a cgroup: {}", slot.is_some()),
    }
}

/// A failure that passes is no worker for now and none outside its bound, and
/// a later start's try makes the cgroups, on what systemd already said.
#[tokio::test]
async fn a_failure_that_passes_is_tried_again_by_a_later_start() {
    let base = PathBuf::from("/nowhere/unit");
    let w = Workers::new();
    let passing = |_: PathBuf| async { Err(NotReady::Passing("9 still here".into())) };
    w.prepare(delegated(&base), passing, Duration::ZERO).await;
    assert!(
        refusal(&w).contains("9 still here"),
        "the bound is owed: {}",
        refusal(&w)
    );

    let asked = async { panic!("systemd has said") };
    w.prepare(asked, |b| async { Ok(b) }, Duration::ZERO).await;
    assert!(
        matches!(&*w.read(), Some(Ready::Bound(b)) if *b == base),
        "made by the later try"
    );
}

/// Systemd has said, and its silence on a later try decides nothing: asked
/// again and read as no delegation, every worker would run without its bound.
#[tokio::test]
async fn a_later_try_never_turns_on_what_systemd_says_then() {
    let base = PathBuf::from("/nowhere/unit");
    let w = Workers::new();
    let passing = |_: PathBuf| async { Err(NotReady::Passing("9 still here".into())) };
    w.prepare(delegated(&base), passing, Duration::ZERO).await;
    w.prepare(async { Ok(None) }, passing, Duration::ZERO).await;
    assert!(refusal(&w).contains("9 still here"), "{}", refusal(&w));
}

/// Systemd's silence is no answer: no worker for now, and a later start asks.
#[tokio::test]
async fn systemds_silence_is_asked_again() {
    let base = PathBuf::from("/nowhere/unit");
    let w = Workers::new();
    let silent = async { Err(NotReady::Passing("systemd did not say".into())) };
    let unreached = |_: PathBuf| async { panic!("no cgroup is known yet") };
    w.prepare(silent, unreached, Duration::ZERO).await;
    assert!(refusal(&w).contains("systemd did not say"));

    w.prepare(delegated(&base), |b| async { Ok(b) }, Duration::ZERO)
        .await;
    assert!(matches!(&*w.read(), Some(Ready::Bound(b)) if *b == base));
}

/// Workers that start together share one try: inside the wait between tries,
/// a start is refused at once, whoever asks.
#[tokio::test]
async fn tries_are_spaced_however_many_workers_ask() {
    let base = PathBuf::from("/nowhere/unit");
    let w = Arc::new(Workers::new());
    let tries = Arc::new(AtomicUsize::new(0));
    let again_after = Duration::from_secs(60);
    let mut asks = Vec::new();
    for _ in 0..16 {
        let (w, tries, base) = (Arc::clone(&w), Arc::clone(&tries), base.clone());
        asks.push(tokio::spawn(async move {
            let passing = |_: PathBuf| {
                tries.fetch_add(1, Ordering::SeqCst);
                async { Err(NotReady::Passing("9 still here".into())) }
            };
            w.prepare(delegated(&base), passing, again_after).await;
        }));
    }
    for ask in asks {
        ask.await.unwrap();
    }
    assert_eq!(tries.load(Ordering::SeqCst), 1);
    assert!(refusal(&w).contains("9 still here"));
}

/// A failure that will not pass stays the answer: no second try, however long
/// it has been and whoever asks, and no worker.
#[tokio::test]
async fn a_failure_that_will_not_pass_is_never_tried_again() {
    let base = PathBuf::from("/nowhere/unit");
    let w = Workers::new();
    let tries = AtomicUsize::new(0);
    let broken = |_: PathBuf| {
        tries.fetch_add(1, Ordering::SeqCst);
        async { Err(NotReady::Broken("no memory controller here".into())) }
    };
    for _ in 0..4 {
        w.prepare(delegated(&base), broken, Duration::ZERO).await;
    }
    assert_eq!(tries.load(Ordering::SeqCst), 1);
    assert!(refusal(&w).contains("no memory controller here"));

    let w = Workers::new();
    let broken = async { Err(NotReady::Broken("no cgroup v2 hierarchy".into())) };
    let unreached = |_: PathBuf| async { panic!("no cgroup is known") };
    w.prepare(broken, unreached, Duration::ZERO).await;
    w.prepare(delegated(&base), |b| async { Ok(b) }, Duration::ZERO)
        .await;
    assert!(refusal(&w).contains("no cgroup v2 hierarchy"));
}

/// What is settled stays settled: a unit that delegates nothing is asked once,
/// and a cgroup made ready is never tried again.
#[tokio::test]
async fn what_is_settled_is_asked_once() {
    let base = PathBuf::from("/nowhere/unit");
    let w = Workers::new();
    let unreached = |_: PathBuf| async { panic!("nothing to try") };
    w.prepare(async { Ok(None) }, unreached, Duration::ZERO)
        .await;
    assert!(
        matches!(w.slot(1 << 20), Ok(None)),
        "no bound: it delegates none"
    );
    let asked = async { panic!("it has said") };
    w.prepare(asked, unreached, Duration::ZERO).await;

    let w = Workers::new();
    w.prepare(delegated(&base), |b| async { Ok(b) }, Duration::ZERO)
        .await;
    let asked = async { panic!("it has said") };
    w.prepare(asked, unreached, Duration::ZERO).await;
    assert!(matches!(&*w.read(), Some(Ready::Bound(b)) if *b == base));
}

/// What the field found, on the real kernel, in a delegated cgroup CI has not:
/// a process whose leader has gone while another of its threads lives on. It
/// stays listed, its pid's write moves the thread and not the zombie, and the
/// kernel hands the controller down all the same.
///
/// Run as the two tests above are, in a delegated cgroup.
#[tokio::test]
#[ignore = "needs a delegated cgroup with the memory controller (systemd-run --user --scope -p Delegate=yes)"]
async fn a_zombie_still_listed_holds_up_no_real_cgroup() {
    let dir = cgroup_of_its_own("zombie").await;
    let pid = a_leader_gone_with_a_thread_left(&dir);
    let listed = std::fs::read_to_string(dir.join("cgroup.procs")).unwrap();
    assert_eq!(listed.trim(), pid.to_string(), "listed, its leader gone");
    let got = ready_within(dir.clone(), Duration::ZERO, hand_down).await;
    assert_eq!(got, Ok(dir.clone()));
    let listed = std::fs::read_to_string(dir.join("cgroup.procs")).unwrap();
    assert_eq!(listed.trim(), pid.to_string(), "listed, and handed down");
    assert_eq!(
        std::fs::read_to_string(dir.join("cgroup.subtree_control"))
            .unwrap()
            .trim(),
        "memory"
    );
    end(pid);
    remove_cgroup(&dir);
}

/// A process on the real kernel, killed, and in its exit while a big address
/// space goes: the kernel refuses the hand-down until it has gone, and that is
/// waited out.
///
/// Run as the two tests above are, in a delegated cgroup.
#[tokio::test]
#[ignore = "needs a delegated cgroup with the memory controller (systemd-run --user --scope -p Delegate=yes)"]
async fn a_process_in_its_exit_is_waited_out_on_a_real_cgroup() {
    let dir = cgroup_of_its_own("exit").await;
    let pid = a_process_holding_memory_in(&dir);
    // SAFETY: a signal to this test's own child.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    // Without a pause: its exit lasts tens of milliseconds.
    let from = Instant::now();
    while !in_its_exit(pid) {
        assert!(from.elapsed() < NOT_REACHED, "in its exit");
    }
    let busy = Arc::new(AtomicUsize::new(0));
    let refused = Arc::clone(&busy);
    let counted = move |base: &Path| {
        let said = hand_down(base);
        if said.as_ref().err().and_then(std::io::Error::raw_os_error) == Some(libc::EBUSY) {
            refused.fetch_add(1, Ordering::SeqCst);
        }
        said
    };
    let got = ready_within(dir.clone(), NOT_REACHED, counted).await;
    assert_eq!(got, Ok(dir.clone()));
    assert!(busy.load(Ordering::SeqCst) > 0, "it was refused first");
    end(pid);
    remove_cgroup(&dir);
}

/// A cgroup below the test's own, with the memory controller: the test's
/// process stays out of it, so what is in it is what the test puts there.
async fn cgroup_of_its_own(name: &str) -> PathBuf {
    let own = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let base = unit_dir(v2_path(&own).expect("a cgroup v2 hierarchy"));
    ready_for_test(&base).await.expect(
        "a delegated cgroup with the memory controller (systemd-run --user --scope -p Delegate=yes)",
    );
    let dir = base.join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    dir
}

/// A cgroup [`cgroup_of_its_own`] made, its process gone.
fn remove_cgroup(dir: &Path) {
    let _ = std::fs::remove_dir(dir.join(DAEMON_LEAF));
    std::fs::remove_dir(dir).unwrap();
}

/// A child that has joined `dir`, and goes on to run `then`: only system calls
/// from the fork on, since another thread of this test could hold any lock.
fn a_child_in(dir: &Path, then: unsafe fn(*mut libc::c_void) -> !, with: *mut libc::c_void) -> i32 {
    let join = OpenOptions::new()
        .write(true)
        .open(dir.join("cgroup.procs"))
        .unwrap();
    // SAFETY: the child makes system calls alone, and never returns.
    unsafe {
        let pid = libc::fork();
        assert!(pid >= 0);
        if pid == 0 {
            if libc::write(join.as_raw_fd(), b"0".as_ptr().cast(), 1) != 1 {
                libc::_exit(1);
            }
            then(with);
        }
        pid
    }
}

/// A process in `dir` whose leader has exited, with a thread left that sleeps
/// on until [`end`]: back once the leader is a zombie.
fn a_leader_gone_with_a_thread_left(dir: &Path) -> i32 {
    extern "C" fn thread(_: *mut libc::c_void) -> libc::c_int {
        let an_hour = libc::timespec {
            tv_sec: 3600,
            tv_nsec: 0,
        };
        // SAFETY: system calls on a thread with no other state.
        unsafe {
            libc::syscall(
                libc::SYS_nanosleep,
                &raw const an_hour,
                std::ptr::null_mut::<libc::timespec>(),
            );
            libc::syscall(libc::SYS_exit_group, 0);
        }
        0
    }
    unsafe fn leader(stack_top: *mut libc::c_void) -> ! {
        let flags = libc::CLONE_VM
            | libc::CLONE_FS
            | libc::CLONE_FILES
            | libc::CLONE_SIGHAND
            | libc::CLONE_THREAD
            | libc::CLONE_SYSVSEM;
        // SAFETY: the stack is this process's own, and nothing else uses it.
        unsafe {
            if libc::clone(thread, stack_top, flags, std::ptr::null_mut()) < 0 {
                libc::_exit(1);
            }
            // The leader alone: `exit`, not `exit_group`.
            libc::syscall(libc::SYS_exit, 0);
        }
        unreachable!()
    }
    let mut stack = vec![0_u8; 64 << 10];
    let top = stack.as_mut_ptr_range().end.map_addr(|a| a & !0xf);
    let pid = a_child_in(dir, leader, top.cast());
    wait_for(|| state_of(pid) == Some('Z'), "its leader a zombie");
    pid
}

/// How much memory a process in its exit gives back: enough to take it a while
/// (600 MiB took 20 to 40 ms), and well inside a test guest.
const HELD: usize = 512 << 20;

/// A process in `dir` holding [`HELD`], touched: back once it is held.
fn a_process_holding_memory_in(dir: &Path) -> i32 {
    unsafe fn hold(_: *mut libc::c_void) -> ! {
        // SAFETY: an anonymous mapping of this process's own, touched in pages.
        unsafe {
            let at = libc::mmap(
                std::ptr::null_mut(),
                HELD,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if at == libc::MAP_FAILED {
                libc::_exit(1);
            }
            for page in (0..HELD).step_by(4096) {
                at.cast::<u8>().add(page).write_volatile(1);
            }
            libc::raise(libc::SIGSTOP);
            loop {
                libc::pause();
            }
        }
    }
    let pid = a_child_in(dir, hold, std::ptr::null_mut());
    wait_for(|| state_of(pid) == Some('T'), "its memory touched");
    pid
}

/// Its state, from `/proc/<pid>/stat`, while it has one.
fn state_of(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.trim_start().chars().next()
}

/// Whether the kernel has it in its exit: `PF_EXITING`, in its flags.
fn in_its_exit(pid: i32) -> bool {
    const PF_EXITING: u64 = 0x4;
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(6)?.parse::<u64>().ok())
            .is_some_and(|flags| flags & PF_EXITING != 0)
    })
}

/// Until `ok`, within 30 s.
fn wait_for(ok: impl Fn() -> bool, what: &str) {
    let from = Instant::now();
    while !ok() {
        assert!(from.elapsed() < Duration::from_secs(30), "{what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Its end, reaped.
fn end(pid: i32) {
    // SAFETY: a signal to, and a wait for, this test's own child.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
}
